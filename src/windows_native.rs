// SPDX-License-Identifier: GPL-3.0-only

//! Narrow, audited wrappers around the native Windows APIs used by the installer.
//!
//! The rest of the crate remains `unsafe_code = "deny"`. Direct FFI is confined to this module,
//! and every exported operation presents a safe, ownership-based interface.

use std::collections::BTreeMap;
use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use serde::Deserialize;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID, SECURITY_NT_AUTHORITY,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, FindFirstVolumeW, FindNextVolumeW,
    FindVolumeClose, GetDriveTypeW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{DISK_EXTENT, FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME};
use windows_sys::Win32::System::SystemServices::{
    DOMAIN_ALIAS_RID_ADMINS, SECURITY_BUILTIN_DOMAIN_RID,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, SHELLEXECUTEINFOW_0,
    ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
use wmi::WMIConnection;

const MAX_PHYSICAL_DISKS: usize = 256;
const MAX_VOLUME_NAME: usize = 1024;
const MAX_VOLUME_EXTENTS_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these fields preserve independent safety facts returned by the Windows disk provider"
)]
pub struct DiskRecord {
    pub number: u32,
    pub friendly_name: String,
    pub path: String,
    pub unique_id: String,
    pub serial_number: String,
    pub size: u64,
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
    pub is_boot: bool,
    pub is_system: bool,
    pub is_offline: bool,
    pub is_read_only: bool,
    pub bus_type: String,
    pub supports_removable_media: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the deserialization shape must match the native MSFT_Disk schema"
)]
struct MsftDisk {
    number: u32,
    friendly_name: Option<String>,
    path: Option<String>,
    unique_id: Option<String>,
    serial_number: Option<String>,
    size: u64,
    logical_sector_size: u32,
    physical_sector_size: u32,
    is_boot: bool,
    is_system: bool,
    is_offline: bool,
    is_read_only: bool,
    bus_type: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32DiskDrive {
    index: u32,
    capabilities: Option<Vec<u16>>,
}

/// Query the Windows Storage Management provider directly through COM/WMI.
///
/// The second query supplies the documented removable-media capability needed to distinguish USB
/// card readers from ordinary USB disks.
pub fn query_disks() -> io::Result<Vec<DiskRecord>> {
    let storage = WMIConnection::with_namespace_path("ROOT\\Microsoft\\Windows\\Storage")
        .map_err(wmi_error)?;
    let disks: Vec<MsftDisk> = storage
        .raw_query(concat!(
            "SELECT Number,FriendlyName,Path,UniqueId,SerialNumber,Size,",
            "LogicalSectorSize,PhysicalSectorSize,IsBoot,IsSystem,IsOffline,IsReadOnly,BusType ",
            "FROM MSFT_Disk"
        ))
        .map_err(wmi_error)?;
    if disks.len() > MAX_PHYSICAL_DISKS {
        return Err(io::Error::other(
            "Windows returned an implausible number of physical disks",
        ));
    }

    let cim = WMIConnection::new().map_err(wmi_error)?;
    let capabilities: Vec<Win32DiskDrive> = cim
        .raw_query("SELECT Index,Capabilities FROM Win32_DiskDrive")
        .map_err(wmi_error)?;
    let mut removable = BTreeMap::<u32, Option<bool>>::new();
    for disk in capabilities {
        let value = disk
            .capabilities
            .as_deref()
            .is_some_and(|items| items.contains(&7));
        removable
            .entry(disk.index)
            .and_modify(|existing| *existing = None)
            .or_insert(Some(value));
    }

    Ok(disks
        .into_iter()
        .map(|disk| DiskRecord {
            number: disk.number,
            friendly_name: clean(disk.friendly_name),
            path: clean(disk.path),
            unique_id: clean(disk.unique_id),
            serial_number: clean(disk.serial_number),
            size: disk.size,
            logical_sector_size: disk.logical_sector_size,
            physical_sector_size: disk.physical_sector_size,
            is_boot: disk.is_boot,
            is_system: disk.is_system,
            is_offline: disk.is_offline,
            is_read_only: disk.is_read_only,
            bus_type: bus_name(disk.bus_type).to_owned(),
            supports_removable_media: removable
                .get(&disk.number)
                .copied()
                .flatten()
                .unwrap_or(false),
        })
        .collect())
}

pub fn query_disk(number: u32) -> io::Result<DiskRecord> {
    let matches: Vec<_> = query_disks()?
        .into_iter()
        .filter(|disk| disk.number == number)
        .collect();
    match matches.as_slice() {
        [disk] => Ok(disk.clone()),
        [] => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the selected physical disk is no longer present",
        )),
        _ => Err(io::Error::other(
            "Windows returned a duplicate physical-disk number",
        )),
    }
}

fn wmi_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("native Windows storage query failed: {error}"))
}

fn clean(value: Option<String>) -> String {
    value.unwrap_or_default().trim().to_owned()
}

const fn bus_name(value: u16) -> &'static str {
    match value {
        7 => "USB",
        12 => "SD",
        13 => "MMC",
        _ => "OTHER",
    }
}

/// Return whether the current process token is a member of the built-in Administrators group.
pub fn is_elevated() -> io::Result<bool> {
    let mut sid: PSID = null_mut();
    // SAFETY: `sid` is a valid out pointer, the authority is a process-lifetime constant, and the
    // eight sub-authority arguments match the Win32 contract for the Administrators SID.
    let allocated = unsafe {
        AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            SECURITY_BUILTIN_DOMAIN_RID as u32,
            DOMAIN_ALIAS_RID_ADMINS as u32,
            0,
            0,
            0,
            0,
            0,
            0,
            &raw mut sid,
        )
    };
    if allocated == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut member = 0;
    // SAFETY: the SID was allocated successfully above and remains alive until `FreeSid`; a null
    // token handle intentionally requests the effective thread/process token documented by Win32.
    let checked = unsafe { CheckTokenMembership(null_mut(), sid, &raw mut member) };
    // SAFETY: `sid` came from `AllocateAndInitializeSid` and is freed exactly once here.
    unsafe { FreeSid(sid) };
    if checked == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(member != 0)
    }
}

pub fn is_fixed_drive_path(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return false;
    }
    let root = wide_null(OsStr::new(&text[..3]));
    // SAFETY: `root` is a valid, NUL-terminated UTF-16 drive-root string.
    (unsafe { GetDriveTypeW(root.as_ptr()) }) == DRIVE_FIXED
}

/// Locked volume handles keep every filesystem belonging to one physical disk dismounted.
#[derive(Debug, Default)]
pub struct LockedVolumes {
    _handles: Vec<File>,
}

pub fn lock_and_dismount_volumes(disk_number: u32) -> io::Result<LockedVolumes> {
    let mut handles = Vec::new();
    for volume in volume_names()? {
        let query = open_volume(&volume, 0)?;
        match volume_relation(&query, disk_number)? {
            VolumeRelation::Unrelated => continue,
            VolumeRelation::Spanned => {
                return Err(io::Error::other(
                    "refusing a volume that spans the selected disk and another disk",
                ));
            }
            VolumeRelation::Exact => {}
        }
        drop(query);

        let handle = open_volume(&volume, GENERIC_READ | GENERIC_WRITE)?;
        device_io_no_buffer(&handle, FSCTL_LOCK_VOLUME).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not lock {volume}; close Explorer and applications using the card: {error}"
                ),
            )
        })?;
        device_io_no_buffer(&handle, FSCTL_DISMOUNT_VOLUME).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not dismount {volume}: {error}"),
            )
        })?;
        handles.push(handle);
    }
    Ok(LockedVolumes { _handles: handles })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VolumeRelation {
    Unrelated,
    Exact,
    Spanned,
}

fn volume_relation(volume: &File, disk_number: u32) -> io::Result<VolumeRelation> {
    let mut bytes = vec![0_u8; MAX_VOLUME_EXTENTS_BYTES];
    let count = device_io_output(volume, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &mut bytes)?;
    if count < size_of::<u32>() {
        return Err(io::Error::other(
            "Windows returned truncated volume extents",
        ));
    }
    // SAFETY: the byte buffer contains at least a `u32`, and unaligned reads are explicitly used.
    let extent_count = unsafe { (bytes.as_ptr().cast::<u32>()).read_unaligned() } as usize;
    let offset = offset_of!(
        windows_sys::Win32::System::Ioctl::VOLUME_DISK_EXTENTS,
        Extents
    );
    let required = offset
        .checked_add(
            extent_count
                .checked_mul(size_of::<DISK_EXTENT>())
                .ok_or_else(|| io::Error::other("volume extent count overflow"))?,
        )
        .ok_or_else(|| io::Error::other("volume extent size overflow"))?;
    if extent_count == 0 || required > count {
        return Err(io::Error::other("Windows returned invalid volume extents"));
    }
    let mut selected = false;
    let mut other = false;
    for index in 0..extent_count {
        // SAFETY: `required <= count` proves each computed `DISK_EXTENT` lies in the initialized
        // output buffer; the API layout may be unaligned, so `read_unaligned` is required.
        let extent = unsafe {
            bytes
                .as_ptr()
                .add(offset + index * size_of::<DISK_EXTENT>())
                .cast::<DISK_EXTENT>()
                .read_unaligned()
        };
        if extent.DiskNumber == disk_number {
            selected = true;
        } else {
            other = true;
        }
    }
    Ok(match (selected, other) {
        (false, _) => VolumeRelation::Unrelated,
        (true, false) => VolumeRelation::Exact,
        (true, true) => VolumeRelation::Spanned,
    })
}

fn volume_names() -> io::Result<Vec<String>> {
    let mut buffer = vec![0_u16; MAX_VOLUME_NAME];
    let buffer_len = u32::try_from(buffer.len())
        .map_err(|_| io::Error::other("Windows volume-name buffer is too large"))?;
    // SAFETY: `buffer` is writable for the supplied element count.
    let search = unsafe { FindFirstVolumeW(buffer.as_mut_ptr(), buffer_len) };
    if search == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut names = Vec::new();
    let result = loop {
        names.push(utf16_buffer(&buffer)?);
        buffer.fill(0);
        // SAFETY: `search` is a live volume-enumeration handle and `buffer` remains writable.
        if unsafe { FindNextVolumeW(search, buffer.as_mut_ptr(), buffer_len) } != 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(18) {
            break Ok(names);
        }
        break Err(error);
    };
    // SAFETY: `search` was returned by `FindFirstVolumeW` and is closed exactly once.
    unsafe { FindVolumeClose(search) };
    result
}

fn utf16_buffer(buffer: &[u16]) -> io::Result<String> {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .ok_or_else(|| io::Error::other("Windows volume name was not terminated"))?;
    String::from_utf16(&buffer[..length])
        .map_err(|_| io::Error::other("Windows returned an invalid volume name"))
}

fn open_volume(volume: &str, access: u32) -> io::Result<File> {
    let path = volume.strip_suffix('\\').unwrap_or(volume);
    let path = wide_null(OsStr::new(path));
    // SAFETY: all pointers are either null as permitted by Win32 or point to live, terminated
    // buffers. Ownership of a successful handle is transferred immediately into `File`.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    file_from_handle(handle)
}

fn file_from_handle(handle: HANDLE) -> io::Result<File> {
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this function takes sole ownership of a newly returned Win32 handle and converts it
    // into a `File`, whose destructor closes it exactly once.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

fn device_io_no_buffer(file: &File, code: u32) -> io::Result<()> {
    let mut returned = 0;
    // SAFETY: the file handle is live; this control code has no input or output buffers.
    let ok = unsafe {
        DeviceIoControl(
            std::os::windows::io::AsRawHandle::as_raw_handle(file) as HANDLE,
            code,
            null(),
            0,
            null_mut(),
            0,
            &raw mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn device_io_output(file: &File, code: u32, output: &mut [u8]) -> io::Result<usize> {
    let output_len = u32::try_from(output.len())
        .map_err(|_| io::Error::other("Windows I/O buffer is too large"))?;
    let mut returned = 0;
    // SAFETY: the file handle is live and `output` is writable for `output_len` bytes. The call is
    // synchronous because no OVERLAPPED pointer is supplied.
    let ok = unsafe {
        DeviceIoControl(
            std::os::windows::io::AsRawHandle::as_raw_handle(file) as HANDLE,
            code,
            null(),
            0,
            output.as_mut_ptr().cast::<c_void>(),
            output_len,
            &raw mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(returned).map_err(|_| io::Error::other("invalid Windows I/O length"))
    }
}

/// Process handle returned by the native UAC elevation boundary.
#[derive(Debug)]
pub struct ElevatedChild {
    handle: HANDLE,
}

impl ElevatedChild {
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        // SAFETY: the process handle remains owned by `self` for the duration of the call.
        match unsafe { WaitForSingleObject(self.handle, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => self.exit_code().map(Some),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!(
                "unexpected Windows process wait result {other}"
            ))),
        }
    }

    pub fn wait(&self) -> io::Result<u32> {
        // SAFETY: the process handle remains owned by `self` for the duration of the call.
        match unsafe { WaitForSingleObject(self.handle, INFINITE) } {
            WAIT_OBJECT_0 => self.exit_code(),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!(
                "unexpected Windows process wait result {other}"
            ))),
        }
    }

    fn exit_code(&self) -> io::Result<u32> {
        let mut code = 0;
        // SAFETY: `code` is a valid out pointer and the process handle is live.
        if unsafe { GetExitCodeProcess(self.handle, &raw mut code) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(code)
        }
    }
}

impl Drop for ElevatedChild {
    fn drop(&mut self) {
        // SAFETY: `handle` is owned by this value and closed exactly once. Closing the handle does
        // not terminate an elevated worker that may still be completing its safe cancellation.
        unsafe { CloseHandle(self.handle) };
    }
}

pub fn launch_elevated(executable: &Path, arguments: &OsStr) -> io::Result<ElevatedChild> {
    let executable = wide_null(executable.as_os_str());
    let arguments = wide_null(arguments);
    let verb = wide_null(OsStr::new("runas"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>())
            .map_err(|_| io::Error::other("invalid ShellExecute structure size"))?,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        hwnd: null_mut(),
        lpVerb: verb.as_ptr(),
        lpFile: executable.as_ptr(),
        lpParameters: arguments.as_ptr(),
        lpDirectory: null(),
        nShow: SW_HIDE,
        hInstApp: null_mut(),
        lpIDList: null_mut(),
        lpClass: null(),
        hkeyClass: null_mut(),
        dwHotKey: 0,
        Anonymous: SHELLEXECUTEINFOW_0::default(),
        hProcess: null_mut(),
    };
    // SAFETY: every referenced UTF-16 buffer remains alive for the synchronous call, and `info`
    // has the exact ABI size expected by `ShellExecuteExW`.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // `SHELLEXECUTEINFOW` is packed on some targets; perform an explicit unaligned read.
    // SAFETY: the API initialized `hProcess` because `SEE_MASK_NOCLOSEPROCESS` was requested.
    let handle = unsafe { std::ptr::addr_of!(info.hProcess).read_unaligned() };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::other(
            "Windows UAC returned no elevated process handle",
        ))
    } else {
        Ok(ElevatedChild { handle })
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    let mut encoded: Vec<u16> = value.encode_wide().collect();
    encoded.push(0);
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_only_supported_card_bus_types() {
        assert_eq!(bus_name(7), "USB");
        assert_eq!(bus_name(12), "SD");
        assert_eq!(bus_name(13), "MMC");
        assert_eq!(bus_name(11), "OTHER");
    }

    #[test]
    fn utf16_buffer_requires_termination_and_valid_utf16() {
        assert_eq!(utf16_buffer(&[u16::from(b'A'), 0, 9]).unwrap(), "A");
        assert!(utf16_buffer(&[u16::from(b'A')]).is_err());
        assert!(utf16_buffer(&[0xD800, 0]).is_err());
    }
}
