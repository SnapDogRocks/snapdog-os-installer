// SPDX-License-Identifier: GPL-3.0-only

//! Audited safe wrappers around the macOS frameworks used by the installer.

use std::ffi::{CStr, CString, c_void};
use std::io;
#[cfg(any(not(debug_assertions), test))]
use std::path::Path;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

#[cfg(any(not(debug_assertions), test))]
use objc2_core_foundation::CFURL;
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFRunLoop, CFString, CFType, ConcreteType,
    kCFRunLoopDefaultMode,
};
use objc2_disk_arbitration::{
    DADisk, DADissenter, DASession, kDADiskDescriptionMediaBSDNameKey,
    kDADiskDescriptionMediaEjectableKey, kDADiskDescriptionMediaRemovableKey,
    kDADiskDescriptionMediaSizeKey, kDADiskDescriptionMediaWholeKey,
    kDADiskDescriptionMediaWritableKey, kDADiskEjectOptionDefault, kDADiskUnmountOptionWhole,
};
use objc2_io_kit::{
    IOIteratorNext, IOObjectGetClass, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IORegistryEntryGetName, IORegistryEntryGetParentEntry, IORegistryEntryGetRegistryEntryID,
    IOServiceGetMatchingServices, IOServiceMatching, kIOMainPortDefault, kIOReturnNoDevice,
    kIOReturnSuccess,
};
#[cfg(any(not(debug_assertions), test))]
use objc2_security::{
    SecCSFlags, SecRequirement, SecStaticCode, errSecSuccess, kSecCSCheckAllArchitectures,
    kSecCSCheckNestedCode, kSecCSStrictValidate,
};

const DISK_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskRecord {
    pub bsd_name: String,
    pub name: Option<String>,
    pub registry_entry_id: u64,
    pub size: u64,
    pub whole: bool,
    pub writable: bool,
    pub removable: bool,
    pub ejectable: bool,
    pub physical: bool,
}

struct IoObject(u32);

impl Drop for IoObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            let _ = IOObjectRelease(self.0);
        }
    }
}

pub fn query_removable_media() -> io::Result<Vec<DiskRecord>> {
    let class = CString::new("IOMedia").expect("static IOKit class has no NUL");
    // SAFETY: `class` is a valid, NUL-terminated C string for the duration of the call.
    let matching = unsafe { IOServiceMatching(class.as_ptr()) }
        .ok_or_else(|| io::Error::other("IOServiceMatching(IOMedia) returned no dictionary"))?;
    // SAFETY: CFMutableDictionary is a subtype of CFDictionary and IOKit consumes this reference.
    let matching = unsafe { CFRetained::cast_unchecked::<CFDictionary>(matching) };
    let mut iterator = 0;
    // SAFETY: `iterator` points to writable storage and `matching` is the owned matching dictionary
    // consumed by IOKit exactly once.
    let status = unsafe {
        IOServiceGetMatchingServices(kIOMainPortDefault, Some(matching), &raw mut iterator)
    };
    if status != kIOReturnSuccess {
        return Err(io::Error::other(format!(
            "IOServiceGetMatchingServices failed with IOKit status {status:#x}"
        )));
    }
    let iterator = IoObject(iterator);
    // SAFETY: the default allocator is valid and the returned create-rule object is retained.
    let session = unsafe { DASession::new(None) }
        .ok_or_else(|| io::Error::other("could not create a Disk Arbitration session"))?;
    let mut records = Vec::new();
    loop {
        let entry = IOIteratorNext(iterator.0);
        if entry == 0 {
            break;
        }
        let entry = IoObject(entry);
        if let Some(record) = io_media_record(entry.0)?
            && record.whole
            && record.writable
            && record.removable
            && record.ejectable
            && record.physical
        {
            cross_check_disk_arbitration(&session, &record)?;
            records.push(record);
        }
    }
    Ok(records)
}

fn io_media_record(entry: u32) -> io::Result<Option<DiskRecord>> {
    let Some(bsd_name) = property_string(entry, "BSD Name") else {
        return Ok(None);
    };
    let Some(size) = property_u64(entry, "Size") else {
        return Ok(None);
    };
    let mut registry_entry_id = 0;
    // SAFETY: `registry_entry_id` points to valid writable storage.
    let status = unsafe { IORegistryEntryGetRegistryEntryID(entry, &raw mut registry_entry_id) };
    if status != kIOReturnSuccess {
        return Err(io::Error::other(format!(
            "reading I/O Registry entry ID for {bsd_name} failed with {status:#x}"
        )));
    }
    let mut name = [0_i8; 128];
    // SAFETY: `name` is the 128-byte buffer required by `io_name_t`.
    let name = if unsafe { IORegistryEntryGetName(entry, &raw mut name) } == kIOReturnSuccess {
        // SAFETY: IOKit promises a NUL-terminated string on success.
        let value = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        (!value.trim().is_empty()).then_some(value)
    } else {
        None
    };
    Ok(Some(DiskRecord {
        bsd_name,
        name,
        registry_entry_id,
        size,
        whole: property_bool(entry, "Whole").unwrap_or(false),
        writable: property_bool(entry, "Writable").unwrap_or(false),
        removable: property_bool(entry, "Removable").unwrap_or(false),
        ejectable: property_bool(entry, "Ejectable").unwrap_or(false),
        physical: is_physical_media(entry)?,
    }))
}

fn is_physical_media(entry: u32) -> io::Result<bool> {
    let mut current = entry;
    let mut parents = Vec::new();
    let mut plane = [0_i8; 128];
    for (slot, byte) in plane.iter_mut().zip(b"IOService\0") {
        *slot = i8::try_from(*byte).expect("IOService is ASCII");
    }
    for _ in 0..64 {
        let mut class = [0_i8; 128];
        // SAFETY: `class` is the exact caller-owned buffer required by `io_name_t`.
        if unsafe { IOObjectGetClass(current, &raw mut class) } == kIOReturnSuccess {
            // SAFETY: IOKit promises a NUL-terminated class name on success.
            let class = unsafe { CStr::from_ptr(class.as_ptr()) }.to_string_lossy();
            if class.starts_with("IOHDIX") {
                return Ok(false);
            }
        }
        if property_string(current, "Physical Interconnect Location")
            .is_some_and(|location| location.eq_ignore_ascii_case("File"))
        {
            return Ok(false);
        }
        let mut parent = 0;
        // SAFETY: `plane` is a valid NUL-terminated io_name_t buffer and `parent` is valid output
        // storage. Every successful returned parent is released before the next iteration.
        let status =
            unsafe { IORegistryEntryGetParentEntry(current, &raw mut plane, &raw mut parent) };
        if status.cast_unsigned() == kIOReturnNoDevice {
            return Ok(true);
        }
        if status != kIOReturnSuccess || parent == 0 {
            return Err(io::Error::other(format!(
                "could not inspect I/O Registry ancestry (status {status:#x})"
            )));
        }
        parents.push(IoObject(parent));
        current = parents.last().expect("parent initialized").0;
    }
    Err(io::Error::other(
        "I/O Registry ancestry exceeded the safety traversal limit",
    ))
}

fn property(entry: u32, key: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(key);
    // SAFETY: the key is a valid CFString and the returned create-rule object is retained.
    unsafe { IORegistryEntryCreateCFProperty(entry, Some(&key), None, 0) }
}

fn property_string(entry: u32, key: &str) -> Option<String> {
    property(entry, key)?
        .downcast::<CFString>()
        .ok()
        .map(|value| value.to_string())
}

fn property_bool(entry: u32, key: &str) -> Option<bool> {
    property(entry, key)?
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.as_bool())
}

fn property_u64(entry: u32, key: &str) -> Option<u64> {
    property(entry, key)?
        .downcast::<CFNumber>()
        .ok()?
        .as_i64()
        .and_then(|value| u64::try_from(value).ok())
}

fn cross_check_disk_arbitration(session: &DASession, record: &DiskRecord) -> io::Result<()> {
    let disk = disk_from_bsd_name(session, &record.bsd_name)?;
    // SAFETY: `disk` is a live Disk Arbitration object owned by this scope.
    let description = unsafe { disk.description() }.ok_or_else(|| {
        io::Error::other(format!("no Disk Arbitration data for {}", record.bsd_name))
    })?;
    // SAFETY: these framework constants are valid CFString keys for this dictionary.
    let matches = unsafe {
        dictionary_string(&description, kDADiskDescriptionMediaBSDNameKey).as_deref()
            == Some(record.bsd_name.as_str())
            && dictionary_u64(&description, kDADiskDescriptionMediaSizeKey) == Some(record.size)
            && dictionary_bool(&description, kDADiskDescriptionMediaWholeKey) == Some(record.whole)
            && dictionary_bool(&description, kDADiskDescriptionMediaWritableKey)
                == Some(record.writable)
            && dictionary_bool(&description, kDADiskDescriptionMediaRemovableKey)
                == Some(record.removable)
            && dictionary_bool(&description, kDADiskDescriptionMediaEjectableKey)
                == Some(record.ejectable)
    };
    if !matches {
        return Err(io::Error::other(format!(
            "IOKit and Disk Arbitration disagree about {}",
            record.bsd_name
        )));
    }
    Ok(())
}

unsafe fn dictionary_value<'a, T: ConcreteType>(
    dictionary: &'a CFDictionary,
    key: &'a CFString,
) -> Option<&'a T> {
    // SAFETY: both pointers are live Core Foundation objects. The value remains owned by the
    // dictionary for the returned borrow, and downcast_ref checks its dynamic CF type.
    let value = unsafe { dictionary.value(ptr::from_ref(key).cast()) };
    (!value.is_null())
        .then(|| unsafe { &*value.cast::<CFType>() })
        .and_then(CFType::downcast_ref::<T>)
}

unsafe fn dictionary_string(dictionary: &CFDictionary, key: &CFString) -> Option<String> {
    unsafe { dictionary_value::<CFString>(dictionary, key) }.map(ToString::to_string)
}

unsafe fn dictionary_bool(dictionary: &CFDictionary, key: &CFString) -> Option<bool> {
    unsafe { dictionary_value::<CFBoolean>(dictionary, key) }.map(CFBoolean::as_bool)
}

unsafe fn dictionary_u64(dictionary: &CFDictionary, key: &CFString) -> Option<u64> {
    unsafe { dictionary_value::<CFNumber>(dictionary, key) }
        .and_then(CFNumber::as_i64)
        .and_then(|value| u64::try_from(value).ok())
}

fn disk_from_bsd_name(session: &DASession, bsd_name: &str) -> io::Result<CFRetained<DADisk>> {
    let name = CString::new(bsd_name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid BSD disk name"))?;
    let pointer = NonNull::new(name.as_ptr().cast_mut()).expect("CString pointer is non-null");
    // SAFETY: `pointer` is a valid NUL-terminated BSD device name during the call.
    unsafe { DADisk::from_bsd_name(None, session, pointer) }
        .ok_or_else(|| io::Error::other(format!("Disk Arbitration could not open {bsd_name}")))
}

#[derive(Clone, Copy)]
enum DiskAction {
    Unmount,
    Eject,
}

struct DiskActionState {
    complete: bool,
    error: Option<String>,
}

unsafe extern "C-unwind" fn disk_action_callback(
    _disk: NonNull<DADisk>,
    dissenter: *const DADissenter,
    context: *mut c_void,
) {
    // SAFETY: `perform_disk_action` keeps this state alive and runs the callback synchronously on
    // the current run loop before returning.
    let state = unsafe { &mut *context.cast::<DiskActionState>() };
    if let Some(dissenter) = unsafe { dissenter.as_ref() } {
        // SAFETY: Disk Arbitration guarantees the dissenter is valid for the callback duration.
        let status = unsafe { dissenter.status() };
        // SAFETY: same callback-lifetime guarantee as above.
        let detail = unsafe { dissenter.status_string() }.map_or_else(
            || "operation rejected".to_owned(),
            |value| value.to_string(),
        );
        state.error = Some(format!("{detail} (Disk Arbitration status {status:#x})"));
    }
    state.complete = true;
}

fn perform_disk_action(bsd_name: &str, action: DiskAction) -> io::Result<()> {
    // SAFETY: the default allocator is valid and the returned create-rule object is retained.
    let session = unsafe { DASession::new(None) }
        .ok_or_else(|| io::Error::other("could not create a Disk Arbitration session"))?;
    let disk = disk_from_bsd_name(&session, bsd_name)?;
    let run_loop = CFRunLoop::current()
        .ok_or_else(|| io::Error::other("could not access the current Core Foundation run loop"))?;
    // SAFETY: Core Foundation exports this as a non-null process-lifetime constant on macOS.
    let mode = unsafe { kCFRunLoopDefaultMode }
        .ok_or_else(|| io::Error::other("default Core Foundation run-loop mode is unavailable"))?;
    // SAFETY: scheduling and all callback processing happen on this thread and this run loop.
    unsafe { session.schedule_with_run_loop(&run_loop, mode) };
    let mut state = DiskActionState {
        complete: false,
        error: None,
    };
    let context = ptr::from_mut(&mut state).cast::<c_void>();
    // SAFETY: callback and context obey Disk Arbitration's contract and remain live until complete.
    unsafe {
        match action {
            DiskAction::Unmount => disk.unmount(
                kDADiskUnmountOptionWhole,
                Some(disk_action_callback),
                context,
            ),
            DiskAction::Eject => disk.eject(
                kDADiskEjectOptionDefault,
                Some(disk_action_callback),
                context,
            ),
        }
    }
    let deadline = Instant::now() + DISK_ACTION_TIMEOUT;
    while !state.complete && Instant::now() < deadline {
        CFRunLoop::run_in_mode(Some(mode), 0.1, true);
    }
    // SAFETY: this exactly balances the schedule call above on the same thread/run loop.
    unsafe { session.unschedule_from_run_loop(&run_loop, mode) };
    if !state.complete {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Disk Arbitration timed out for {bsd_name}"),
        ));
    }
    state
        .error
        .map_or(Ok(()), |error| Err(io::Error::other(error)))
}

pub fn unmount_whole_disk(bsd_name: &str) -> io::Result<()> {
    perform_disk_action(bsd_name, DiskAction::Unmount)
}

pub fn eject_disk(bsd_name: &str) -> io::Result<()> {
    perform_disk_action(bsd_name, DiskAction::Eject)
}

#[cfg(any(not(debug_assertions), test))]
pub fn verify_code_signature(bundle: &Path, requirement: &str) -> io::Result<()> {
    let url = CFURL::from_directory_path(bundle).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid application bundle path",
        )
    })?;
    let mut code_pointer: *const SecStaticCode = ptr::null();
    // SAFETY: `code_pointer` is valid output storage and URL is a live file URL.
    let status = unsafe {
        SecStaticCode::create_with_path(
            &url,
            SecCSFlags::DefaultFlags,
            NonNull::from(&mut code_pointer),
        )
    };
    if status != errSecSuccess {
        return Err(security_error("creating static code object", status));
    }
    let code_pointer = NonNull::new(code_pointer.cast_mut())
        .ok_or_else(|| io::Error::other("Security framework returned a null static code object"))?;
    // SAFETY: create_with_path returned a create-rule retained object on success.
    let code = unsafe { CFRetained::from_raw(code_pointer) };

    let requirement_text = CFString::from_str(requirement);
    let mut requirement_pointer: *mut SecRequirement = ptr::null_mut();
    // SAFETY: `requirement_pointer` is valid output storage and text is a live CFString.
    let status = unsafe {
        SecRequirement::create_with_string(
            &requirement_text,
            SecCSFlags::DefaultFlags,
            NonNull::from(&mut requirement_pointer),
        )
    };
    if status != errSecSuccess {
        return Err(security_error("compiling code requirement", status));
    }
    let requirement_pointer = NonNull::new(requirement_pointer)
        .ok_or_else(|| io::Error::other("Security framework returned a null requirement"))?;
    // SAFETY: create_with_string returned a create-rule retained object on success.
    let requirement = unsafe { CFRetained::from_raw(requirement_pointer) };
    let flags =
        SecCSFlags(kSecCSStrictValidate | kSecCSCheckAllArchitectures | kSecCSCheckNestedCode);
    // SAFETY: both objects are valid retained Security framework objects.
    let status = unsafe { code.check_validity(flags, Some(&requirement)) };
    if status == errSecSuccess {
        Ok(())
    } else {
        Err(security_error("validating application signature", status))
    }
}

#[cfg(any(not(debug_assertions), test))]
fn security_error(operation: &str, status: i32) -> io::Error {
    io::Error::other(format!(
        "{operation} failed with Security framework status {status}"
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_disk_query_is_available() {
        super::query_removable_media().unwrap();
    }
}
