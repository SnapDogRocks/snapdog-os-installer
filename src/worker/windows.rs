// SPDX-License-Identifier: GPL-3.0-only

//! Windows raw-disk implementation for the privileged worker.
//!
//! Elevation and validation of the worker-job/session files are deliberately handled by the
//! parent worker integration. This module owns only physical-disk identity validation and the
//! destructive disk operations. Every `PowerShell` program is a source-code constant; values are
//! passed as separate process arguments and are never interpolated into program text.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::process::Stdio;

use aligned_vec::{AVec, RuntimeAlign};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    WorkerDrive, WorkerError, WorkerPlatform, WorkerTarget, compare_drive,
    windows_powershell_command,
};

const PHYSICAL_DRIVE_PREFIX: &str = r"\\.\PHYSICALDRIVE";
const STABLE_ID_VERSION: &str = "windows-disk-v2";
const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_POWERSHELL_OUTPUT: usize = 64 * 1024;
const VERIFICATION_BUFFER_SIZE: usize = 1024 * 1024;
const DISK_NUMBER_ENV: &str = "SNAPDOG_DISK_NUMBER";
const DISK_SIZE_ENV: &str = "SNAPDOG_DISK_SIZE";
const DISK_LOGICAL_SECTOR_ENV: &str = "SNAPDOG_DISK_LOGICAL_SECTOR";
const DISK_PHYSICAL_SECTOR_ENV: &str = "SNAPDOG_DISK_PHYSICAL_SECTOR";
const DISK_PATH_ENV: &str = "SNAPDOG_DISK_PATH";
const DISK_UNIQUE_ID_ENV: &str = "SNAPDOG_DISK_UNIQUE_ID";
const DISK_SERIAL_ENV: &str = "SNAPDOG_DISK_SERIAL";
const DISK_BUS_ENV: &str = "SNAPDOG_DISK_BUS";
const DISK_REMOVABLE_MEDIA_ENV: &str = "SNAPDOG_DISK_REMOVABLE_MEDIA";
const ACCESS_PATHS_ENV: &str = "SNAPDOG_ACCESS_PATHS_JSON";

const QUERY_DISK_SCRIPT: &str = r"
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSModuleAutoloadingPreference = 'None'
$env:PSModulePath = ''
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Import-Module -Name $env:SNAPDOG_STORAGE_MODULE -Force -ErrorAction Stop
Import-Module -Name $env:SNAPDOG_CIM_MODULE -Force -ErrorAction Stop
if ($args.Count -ne 0) { throw 'Invalid disk query arguments' }
$number = [uint32]$env:SNAPDOG_DISK_NUMBER
$disks = @(Get-Disk -Number $number -ErrorAction Stop)
if ($disks.Count -ne 1) { throw 'Expected exactly one physical disk' }
$disk = $disks[0]
$cimMatches = @(Get-CimInstance -ClassName Win32_DiskDrive -ErrorAction Stop | Where-Object {
    [uint32]$_.Index -eq $number
})
$supportsRemovableMedia = $false
if ($cimMatches.Count -eq 1) {
    $supportsRemovableMedia = @($cimMatches[0].Capabilities) -contains [uint16]7
}
[pscustomobject]@{
    Number = [uint32]$disk.Number
    Path = [string]$disk.Path
    UniqueId = [string]$disk.UniqueId
    SerialNumber = [string]$disk.SerialNumber
    Size = [uint64]$disk.Size
    LogicalSectorSize = [uint32]$disk.LogicalSectorSize
    PhysicalSectorSize = [uint32]$disk.PhysicalSectorSize
    IsBoot = [bool]$disk.IsBoot
    IsSystem = [bool]$disk.IsSystem
    IsOffline = [bool]$disk.IsOffline
    IsReadOnly = [bool]$disk.IsReadOnly
    BusType = [string]$disk.BusType
    SupportsRemovableMedia = [bool]$supportsRemovableMedia
} | ConvertTo-Json -Compress
";

const QUERY_ACCESS_PATHS_SCRIPT: &str = r"
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSModuleAutoloadingPreference = 'None'
$env:PSModulePath = ''
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Import-Module -Name $env:SNAPDOG_STORAGE_MODULE -Force -ErrorAction Stop
if ($args.Count -ne 0) { throw 'Invalid access-path query arguments' }
$number = [uint32]$env:SNAPDOG_DISK_NUMBER
$items = [System.Collections.Generic.List[object]]::new()
foreach ($partition in @(Get-Partition -DiskNumber $number -ErrorAction Stop)) {
    foreach ($accessPath in @($partition.AccessPaths)) {
        $path = [string]$accessPath
        if ([string]::IsNullOrWhiteSpace($path) -or $path -like '\\?\Volume{*}\') {
            continue
        }
        $items.Add([pscustomobject]@{
            PartitionNumber = [uint32]$partition.PartitionNumber
            AccessPath = $path
        })
    }
}
ConvertTo-Json -InputObject @($items) -Compress
";

const DISMOUNT_VOLUMES_SCRIPT: &str = r#"
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSModuleAutoloadingPreference = 'None'
$env:PSModulePath = ''
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Import-Module -Name $env:SNAPDOG_STORAGE_MODULE -Force -ErrorAction Stop
Import-Module -Name $env:SNAPDOG_CIM_MODULE -Force -ErrorAction Stop
if ($args.Count -ne 0) { throw 'Invalid volume-dismount arguments' }
$number = [uint32]$env:SNAPDOG_DISK_NUMBER
$expectedSize = [uint64]$env:SNAPDOG_DISK_SIZE
$expectedLogicalSector = [uint32]$env:SNAPDOG_DISK_LOGICAL_SECTOR
$expectedPhysicalSector = [uint32]$env:SNAPDOG_DISK_PHYSICAL_SECTOR
$expectedPath = [string]$env:SNAPDOG_DISK_PATH
$expectedUniqueId = [string]$env:SNAPDOG_DISK_UNIQUE_ID
$expectedSerial = [string]$env:SNAPDOG_DISK_SERIAL
$expectedBus = [string]$env:SNAPDOG_DISK_BUS
$expectedRemovableText = [string]$env:SNAPDOG_DISK_REMOVABLE_MEDIA
if (@('0', '1') -cnotcontains $expectedRemovableText) {
    throw 'Invalid removable-media expectation'
}
$expectedRemovableMedia = $expectedRemovableText -ceq '1'
$disks = @(Get-Disk -Number $number -ErrorAction Stop)
if ($disks.Count -ne 1) { throw 'Expected exactly one physical disk' }
$disk = $disks[0]
$path = ([string]$disk.Path).Trim()
$uniqueId = ([string]$disk.UniqueId).Trim()
$serial = ([string]$disk.SerialNumber).Trim()
$bus = ([string]$disk.BusType).Trim().ToUpperInvariant()
$cimMatches = @(Get-CimInstance -ClassName Win32_DiskDrive -ErrorAction Stop | Where-Object {
    [uint32]$_.Index -eq $number
})
$supportsRemovableMedia = $false
if ($cimMatches.Count -eq 1) {
    $supportsRemovableMedia = @($cimMatches[0].Capabilities) -contains [uint16]7
}
$isCardTarget = (@('SD', 'MMC') -ccontains $bus) -or ($bus -ceq 'USB' -and $supportsRemovableMedia)
if ([uint64]$disk.Size -ne $expectedSize -or
    [uint32]$disk.LogicalSectorSize -ne $expectedLogicalSector -or
    [uint32]$disk.PhysicalSectorSize -ne $expectedPhysicalSector -or
    $path -cne $expectedPath -or
    $uniqueId -cne $expectedUniqueId -or
    $serial -cne $expectedSerial -or
    $bus -cne $expectedBus -or
    $supportsRemovableMedia -ne $expectedRemovableMedia -or
    [bool]$disk.IsBoot -or [bool]$disk.IsSystem -or [bool]$disk.IsOffline -or
    [bool]$disk.IsReadOnly -or
    -not $isCardTarget) {
    throw 'Physical disk identity or safety properties changed'
}

$expectedAccessPaths = @(ConvertFrom-Json -InputObject ([string]$env:SNAPDOG_ACCESS_PATHS_JSON) -ErrorAction Stop)
$currentAccessPaths = [System.Collections.Generic.List[object]]::new()
$partitions = @(Get-Partition -DiskNumber $number -ErrorAction Stop)
foreach ($partition in $partitions) {
    foreach ($accessPath in @($partition.AccessPaths)) {
        $candidate = [string]$accessPath
        if ([string]::IsNullOrWhiteSpace($candidate) -or $candidate -like '\\?\Volume{*}\') {
            continue
        }
        $currentAccessPaths.Add([pscustomobject]@{
            PartitionNumber = [uint32]$partition.PartitionNumber
            AccessPath = $candidate
        })
    }
}
if ($currentAccessPaths.Count -ne $expectedAccessPaths.Count) {
    throw 'Target access paths changed before dismount'
}
foreach ($current in $currentAccessPaths) {
    $matches = @($expectedAccessPaths | Where-Object {
        [uint32]$_.PartitionNumber -eq [uint32]$current.PartitionNumber -and
        [string]$_.AccessPath -ceq [string]$current.AccessPath
    })
    if ($matches.Count -ne 1) { throw 'Target access paths changed before dismount' }
}

# Remove every user-mode access path first. Win32_Volume.Dismount(Permanent=true) documents status
# 2 when mount points remain. Volume GUID paths are identities, not removable access paths, and the
# Storage provider explicitly rejects attempts to remove them.
$volumePaths = [System.Collections.Generic.List[string]]::new()
$removedAccessPaths = [System.Collections.Generic.List[object]]::new()
try {
    foreach ($partition in $partitions) {
        $volumes = @($partition | Get-Volume -ErrorAction SilentlyContinue)
        foreach ($volume in $volumes) {
            $volumePath = ([string]$volume.Path).TrimEnd('\')
            if (-not [string]::IsNullOrWhiteSpace($volumePath)) {
                $volumePaths.Add($volumePath)
            }
        }
        foreach ($accessPath in @($partition.AccessPaths)) {
            $path = [string]$accessPath
            if ([string]::IsNullOrWhiteSpace($path) -or $path -like '\\?\Volume{*}\') {
                continue
            }
            Remove-PartitionAccessPath -DiskNumber $number -PartitionNumber ([uint32]$partition.PartitionNumber) -AccessPath $path -Confirm:$false -ErrorAction Stop
            $removedAccessPaths.Add([pscustomobject]@{
                PartitionNumber = [uint32]$partition.PartitionNumber
                AccessPath = $path
            })
        }
    }

    # Win32_Volume.Dismount requests the filesystem lock before dismounting. Never force-close
    # application handles. Status 3 means this removable filesystem cannot promise no-automount;
    # retry a temporary dismount, which is safe because Rust immediately requires ShareMode=0.
    $win32Volumes = @(Get-CimInstance -ClassName Win32_Volume -ErrorAction Stop)
    foreach ($volumePath in @($volumePaths | Sort-Object -Unique)) {
        $matches = @($win32Volumes | Where-Object {
            ([string]$_.DeviceID).TrimEnd('\') -ieq $volumePath
        })
        if ($matches.Count -ne 1) {
            throw "Could not identify target volume $volumePath"
        }
        $result = Invoke-CimMethod -InputObject $matches[0] -MethodName Dismount -Arguments @{
            Force = $false
            Permanent = $true
        } -ErrorAction Stop
        $status = [uint32]$result.ReturnValue
        if ($status -eq 3) {
            $result = Invoke-CimMethod -InputObject $matches[0] -MethodName Dismount -Arguments @{
                Force = $false
                Permanent = $false
            } -ErrorAction Stop
            $status = [uint32]$result.ReturnValue
        }
        switch ($status) {
            0 { continue }
            1 { throw 'Access denied while dismounting a target volume' }
            2 { throw 'A target volume still has an access path' }
            3 { throw 'A target volume does not support dismounting' }
            4 { throw 'A target volume is busy; close Explorer and other applications using it, then retry' }
            default { throw "Target-volume dismount failed with status $status" }
        }
    }
} catch {
    # No write has occurred. Best-effort restoration avoids hiding a drive letter after a clean,
    # fail-closed busy-volume error. The original exception remains the reported failure.
    for ($index = $removedAccessPaths.Count - 1; $index -ge 0; $index--) {
        $removed = $removedAccessPaths[$index]
        try {
            Add-PartitionAccessPath -DiskNumber $number -PartitionNumber ([uint32]$removed.PartitionNumber) -AccessPath ([string]$removed.AccessPath) -ErrorAction Stop
        } catch {}
    }
    throw
}
"#;

const RESTORE_ACCESS_PATHS_SCRIPT: &str = r"
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSModuleAutoloadingPreference = 'None'
$env:PSModulePath = ''
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Import-Module -Name $env:SNAPDOG_STORAGE_MODULE -Force -ErrorAction Stop
Import-Module -Name $env:SNAPDOG_CIM_MODULE -Force -ErrorAction Stop
if ($args.Count -ne 0) { throw 'Invalid access-path restore arguments' }
$number = [uint32]$env:SNAPDOG_DISK_NUMBER
$expectedSize = [uint64]$env:SNAPDOG_DISK_SIZE
$expectedLogicalSector = [uint32]$env:SNAPDOG_DISK_LOGICAL_SECTOR
$expectedPhysicalSector = [uint32]$env:SNAPDOG_DISK_PHYSICAL_SECTOR
$expectedPath = [string]$env:SNAPDOG_DISK_PATH
$expectedUniqueId = [string]$env:SNAPDOG_DISK_UNIQUE_ID
$expectedSerial = [string]$env:SNAPDOG_DISK_SERIAL
$expectedBus = [string]$env:SNAPDOG_DISK_BUS
$expectedRemovableText = [string]$env:SNAPDOG_DISK_REMOVABLE_MEDIA
if (@('0', '1') -cnotcontains $expectedRemovableText) {
    throw 'Invalid removable-media expectation'
}
$expectedRemovableMedia = $expectedRemovableText -ceq '1'
$disks = @(Get-Disk -Number $number -ErrorAction Stop)
if ($disks.Count -ne 1) { throw 'Expected exactly one physical disk' }
$disk = $disks[0]
$path = ([string]$disk.Path).Trim()
$uniqueId = ([string]$disk.UniqueId).Trim()
$serial = ([string]$disk.SerialNumber).Trim()
$bus = ([string]$disk.BusType).Trim().ToUpperInvariant()
$cimMatches = @(Get-CimInstance -ClassName Win32_DiskDrive -ErrorAction Stop | Where-Object {
    [uint32]$_.Index -eq $number
})
$supportsRemovableMedia = $false
if ($cimMatches.Count -eq 1) {
    $supportsRemovableMedia = @($cimMatches[0].Capabilities) -contains [uint16]7
}
$isCardTarget = (@('SD', 'MMC') -ccontains $bus) -or ($bus -ceq 'USB' -and $supportsRemovableMedia)
if ([uint64]$disk.Size -ne $expectedSize -or
    [uint32]$disk.LogicalSectorSize -ne $expectedLogicalSector -or
    [uint32]$disk.PhysicalSectorSize -ne $expectedPhysicalSector -or
    $path -cne $expectedPath -or
    $uniqueId -cne $expectedUniqueId -or
    $serial -cne $expectedSerial -or
    $bus -cne $expectedBus -or
    $supportsRemovableMedia -ne $expectedRemovableMedia -or
    [bool]$disk.IsBoot -or [bool]$disk.IsSystem -or [bool]$disk.IsOffline -or
    [bool]$disk.IsReadOnly -or
    -not $isCardTarget) {
    throw 'Physical disk identity or safety properties changed before rollback'
}

$entries = @(ConvertFrom-Json -InputObject ([string]$env:SNAPDOG_ACCESS_PATHS_JSON) -ErrorAction Stop)
foreach ($entry in $entries) {
    $partitionNumber = [uint32]$entry.PartitionNumber
    $accessPath = [string]$entry.AccessPath
    if ($partitionNumber -eq 0 -or [string]::IsNullOrWhiteSpace($accessPath) -or
        $accessPath.Contains([char]0)) {
        throw 'Invalid access-path rollback record'
    }
    $partitions = @(Get-Partition -DiskNumber $number -PartitionNumber $partitionNumber -ErrorAction Stop)
    if ($partitions.Count -ne 1) { throw 'Rollback partition is no longer unique' }
    $alreadyPresent = @($partitions[0].AccessPaths) | Where-Object { [string]$_ -ceq $accessPath }
    if (@($alreadyPresent).Count -eq 0) {
        Add-PartitionAccessPath -DiskNumber $number -PartitionNumber $partitionNumber -AccessPath $accessPath -ErrorAction Stop
    }
}
";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(transparent)]
struct BoolFlag(bool);

impl BoolFlag {
    const fn is_set(self) -> bool {
        self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct DiskSnapshot {
    number: u32,
    path: String,
    unique_id: String,
    serial_number: String,
    size: u64,
    logical_sector_size: u32,
    physical_sector_size: u32,
    is_boot: BoolFlag,
    is_system: BoolFlag,
    is_offline: BoolFlag,
    is_read_only: BoolFlag,
    bus_type: String,
    supports_removable_media: BoolFlag,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct RemovedAccessPath {
    partition_number: u32,
    access_path: String,
}

impl RemovedAccessPath {
    fn is_valid(&self) -> bool {
        self.partition_number > 0
            && !self.access_path.trim().is_empty()
            && self.access_path.len() <= 32_767
            && !self.access_path.contains('\0')
    }
}

impl DiskSnapshot {
    fn normalize(&mut self) {
        self.path = self.path.trim().to_owned();
        self.unique_id = self.unique_id.trim().to_owned();
        self.serial_number = self.serial_number.trim().to_owned();
        self.bus_type = self.bus_type.trim().to_ascii_uppercase();
    }

    fn fingerprint(&self) -> Result<String, WorkerError> {
        if self.path.is_empty()
            || (self.unique_id.is_empty() && self.serial_number.is_empty())
            || self.size == 0
            || !valid_sector_geometry(
                self.size,
                self.logical_sector_size,
                self.physical_sector_size,
            )
        {
            return Err(WorkerError::UnsafeTarget);
        }
        let number = self.number.to_string();
        let size = self.size.to_string();
        let logical_sector_size = self.logical_sector_size.to_string();
        let physical_sector_size = self.physical_sector_size.to_string();
        let supports_removable_media = self.supports_removable_media.is_set().to_string();
        let mut digest = Sha256::new();
        for field in [
            STABLE_ID_VERSION.as_bytes(),
            number.as_bytes(),
            size.as_bytes(),
            logical_sector_size.as_bytes(),
            physical_sector_size.as_bytes(),
            self.bus_type.as_bytes(),
            self.path.as_bytes(),
            self.unique_id.as_bytes(),
            self.serial_number.as_bytes(),
            supports_removable_media.as_bytes(),
        ] {
            let length = u64::try_from(field.len()).map_err(|_| WorkerError::UnsafeTarget)?;
            digest.update(length.to_le_bytes());
            digest.update(field);
        }
        Ok(hex::encode(digest.finalize()))
    }

    fn is_card_target(&self) -> bool {
        match self.bus_type.as_str() {
            "SD" | "MMC" => true,
            "USB" => self.supports_removable_media.is_set(),
            _ => false,
        }
    }

    fn helper_environment(&self) -> [(String, String); 9] {
        [
            (DISK_NUMBER_ENV.to_owned(), self.number.to_string()),
            (DISK_SIZE_ENV.to_owned(), self.size.to_string()),
            (
                DISK_LOGICAL_SECTOR_ENV.to_owned(),
                self.logical_sector_size.to_string(),
            ),
            (
                DISK_PHYSICAL_SECTOR_ENV.to_owned(),
                self.physical_sector_size.to_string(),
            ),
            (DISK_PATH_ENV.to_owned(), self.path.clone()),
            (DISK_UNIQUE_ID_ENV.to_owned(), self.unique_id.clone()),
            (DISK_SERIAL_ENV.to_owned(), self.serial_number.clone()),
            (DISK_BUS_ENV.to_owned(), self.bus_type.clone()),
            (
                DISK_REMOVABLE_MEDIA_ENV.to_owned(),
                if self.supports_removable_media.is_set() {
                    "1"
                } else {
                    "0"
                }
                .to_owned(),
            ),
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectedDisk {
    number: u32,
    device: String,
    fingerprint: String,
}

impl SelectedDisk {
    fn parse(selected: &WorkerDrive) -> Result<Self, WorkerError> {
        let (device, fingerprint) = selected
            .id
            .rsplit_once('@')
            .ok_or(WorkerError::UnsafeTarget)?;
        let suffix = device
            .strip_prefix(PHYSICAL_DRIVE_PREFIX)
            .ok_or(WorkerError::UnsafeTarget)?;
        if suffix.is_empty()
            || !suffix.bytes().all(|byte| byte.is_ascii_digit())
            || fingerprint.len() != 64
            || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            || selected.device != device
            || selected.capacity == 0
        {
            return Err(WorkerError::UnsafeTarget);
        }
        let number = suffix.parse().map_err(|_| WorkerError::UnsafeTarget)?;
        let canonical_device = format!(r"\\.\PHYSICALDRIVE{number}");
        if canonical_device != device {
            return Err(WorkerError::UnsafeTarget);
        }
        Ok(Self {
            number,
            device: canonical_device,
            fingerprint: fingerprint.to_ascii_lowercase(),
        })
    }
}

/// Raw-disk handle that switches from a flushed buffered writer to an unbuffered reader before
/// verification. `FILE_FLAG_WRITE_THROUGH` makes writes durable; the separate
/// `FILE_FLAG_NO_BUFFERING` handle guarantees readback bypasses the Windows system cache.
#[derive(Debug)]
pub struct WindowsTarget {
    writer: Option<File>,
    verifier: Option<UnbufferedVerifier>,
}

impl WindowsTarget {
    const fn new(writer: File) -> Self {
        Self {
            writer: Some(writer),
            verifier: None,
        }
    }

    fn begin_verification(
        &mut self,
        device: &str,
        capacity: u64,
        logical_sector_size: u32,
        physical_sector_size: u32,
    ) -> io::Result<()> {
        self.sync_all()?;
        drop(self.writer.take());
        let file = open_unbuffered_target(device)?;
        self.verifier = Some(UnbufferedVerifier::new(
            file,
            capacity,
            logical_sector_size,
            physical_sector_size,
        )?);
        Ok(())
    }
}

impl Read for WindowsTarget {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(verifier) = &mut self.verifier {
            verifier.read(buffer)
        } else {
            self.writer
                .as_mut()
                .ok_or_else(|| io::Error::other("Windows target handle is unavailable"))?
                .read(buffer)
        }
    }
}

impl Write for WindowsTarget {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.verifier.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot write while unbuffered verification is active",
            ));
        }
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other("Windows target handle is unavailable"))?
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.as_mut().map_or(Ok(()), std::io::Write::flush)
    }
}

impl Seek for WindowsTarget {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if let Some(verifier) = &mut self.verifier {
            verifier.seek(position)
        } else {
            self.writer
                .as_mut()
                .ok_or_else(|| io::Error::other("Windows target handle is unavailable"))?
                .seek(position)
        }
    }
}

impl WorkerTarget for WindowsTarget {
    fn sync_all(&self) -> io::Result<()> {
        // The write handle was synchronously flushed before the cache-bypassing verifier was
        // opened. A read-only unbuffered handle has no dirty state left to flush.
        self.writer.as_ref().map_or(Ok(()), File::sync_all)
    }
}

#[derive(Debug)]
struct UnbufferedVerifier {
    file: File,
    position: u64,
    capacity: u64,
    transfer_size: usize,
    buffer: AVec<u8, RuntimeAlign>,
}

impl UnbufferedVerifier {
    fn new(
        file: File,
        capacity: u64,
        logical_sector_size: u32,
        physical_sector_size: u32,
    ) -> io::Result<Self> {
        if !valid_sector_geometry(capacity, logical_sector_size, physical_sector_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Windows disk sector geometry",
            ));
        }
        let transfer_size = usize::try_from(logical_sector_size)
            .map_err(|_| io::Error::other("logical sector size does not fit usize"))?;
        let alignment = usize::try_from(physical_sector_size)
            .map_err(|_| io::Error::other("physical sector size does not fit usize"))?;
        let mut buffer = AVec::<u8, RuntimeAlign>::with_capacity(
            alignment,
            VERIFICATION_BUFFER_SIZE + alignment,
        );
        buffer.resize(VERIFICATION_BUFFER_SIZE + alignment, 0);
        Ok(Self {
            file,
            position: 0,
            capacity,
            transfer_size,
            buffer,
        })
    }

    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position >= self.capacity {
            return Ok(0);
        }
        let requested = output.len().min(VERIFICATION_BUFFER_SIZE);
        let requested_u64 = u64::try_from(requested)
            .map_err(|_| io::Error::other("verification read size does not fit u64"))?;
        let available = usize::try_from((self.capacity - self.position).min(requested_u64))
            .map_err(|_| io::Error::other("verification read size does not fit usize"))?;
        let transfer_size_u64 = u64::try_from(self.transfer_size)
            .map_err(|_| io::Error::other("logical sector size does not fit u64"))?;
        let aligned_start = self.position - (self.position % transfer_size_u64);
        let prefix = usize::try_from(self.position - aligned_start)
            .map_err(|_| io::Error::other("verification prefix does not fit usize"))?;
        let needed = prefix
            .checked_add(available)
            .ok_or_else(|| io::Error::other("verification transfer length overflow"))?;
        let aligned_length = needed
            .div_ceil(self.transfer_size)
            .checked_mul(self.transfer_size)
            .ok_or_else(|| io::Error::other("verification transfer length overflow"))?;
        let aligned_length_u64 = u64::try_from(aligned_length)
            .map_err(|_| io::Error::other("verification transfer length does not fit u64"))?;
        if aligned_length > self.buffer.len()
            || aligned_start
                .checked_add(aligned_length_u64)
                .is_none_or(|end| end > self.capacity)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unaligned Windows verification request",
            ));
        }
        self.file.seek(SeekFrom::Start(aligned_start))?;
        let count = self.file.read(&mut self.buffer[..aligned_length])?;
        if count == 0 {
            return Ok(0);
        }
        if !count.is_multiple_of(self.transfer_size) || count <= prefix {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Windows returned an incomplete unbuffered disk sector",
            ));
        }
        let copied = available.min(count - prefix);
        output[..copied].copy_from_slice(&self.buffer[prefix..prefix + copied]);
        self.position += u64::try_from(copied)
            .map_err(|_| io::Error::other("verification result length does not fit u64"))?;
        Ok(copied)
    }

    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.capacity) + i128::from(offset),
        };
        if next < 0 || next > i128::from(self.capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows verification seek is outside the physical disk",
            ));
        }
        self.position = u64::try_from(next)
            .map_err(|_| io::Error::other("verification position does not fit u64"))?;
        Ok(self.position)
    }
}

/// Windows physical-disk backend. The surrounding worker must already be elevated.
#[derive(Debug, Default)]
pub struct WindowsPlatform {
    volumes_dismounted: bool,
    write_may_have_started: bool,
    prepared_target: Option<File>,
    post_write_pin: Option<File>,
    post_write_identity: Option<PostWriteIdentity>,
    rollback_snapshot: Option<DiskSnapshot>,
    removed_access_paths: Vec<RemovedAccessPath>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PostWriteIdentity {
    number: u32,
    path: String,
    serial_number: String,
    size: u64,
    logical_sector_size: u32,
    physical_sector_size: u32,
    bus_type: String,
    supports_removable_media: BoolFlag,
}

impl From<&DiskSnapshot> for PostWriteIdentity {
    fn from(snapshot: &DiskSnapshot) -> Self {
        Self {
            number: snapshot.number,
            path: snapshot.path.clone(),
            serial_number: snapshot.serial_number.clone(),
            size: snapshot.size,
            logical_sector_size: snapshot.logical_sector_size,
            physical_sector_size: snapshot.physical_sector_size,
            bus_type: snapshot.bus_type.clone(),
            supports_removable_media: snapshot.supports_removable_media,
        }
    }
}

impl WindowsPlatform {
    fn rollback_unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        if self.write_may_have_started {
            return Err(WorkerError::UnsafeTarget);
        }
        drop(self.prepared_target.take());
        drop(self.post_write_pin.take());
        let expected = self
            .rollback_snapshot
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)?;
        let identity = SelectedDisk::parse(selected)?;
        let current = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &current, Some(false))?;
        if current != *expected {
            return Err(WorkerError::TargetChanged);
        }
        restore_access_paths(&current, &self.removed_access_paths)?;
        self.removed_access_paths.clear();
        self.rollback_snapshot = None;
        self.post_write_identity = None;
        self.volumes_dismounted = false;
        Ok(())
    }
}

impl WorkerPlatform for WindowsPlatform {
    type Target = WindowsTarget;

    fn validate_target(&mut self, selected: &WorkerDrive) -> Result<WorkerDrive, WorkerError> {
        let identity = SelectedDisk::parse(selected)?;
        let snapshot = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &snapshot, Some(false))?;
        Ok(selected.clone())
    }

    fn unmount(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        if self.prepared_target.is_some()
            || self.volumes_dismounted
            || self.write_may_have_started
            || !self.removed_access_paths.is_empty()
        {
            return Err(WorkerError::UnsafeTarget);
        }
        let identity = SelectedDisk::parse(selected)?;
        let snapshot = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &snapshot, Some(false))?;

        // A shared read handle pins the selected device object while the storage stack identifies
        // and locks each of its volumes. It permits Windows' own dismount handles but prevents a
        // hot-unplug/number-reuse race from silently changing the object under validation.
        let device_pin = open_device_pin(&identity.device)?;
        let pinned_snapshot = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &pinned_snapshot, Some(false))?;
        self.post_write_identity = Some(PostWriteIdentity::from(&pinned_snapshot));
        self.rollback_snapshot = Some(pinned_snapshot.clone());
        let removed_access_paths = match query_access_paths(&pinned_snapshot) {
            Ok(paths) => paths,
            Err(error) => {
                self.post_write_identity = None;
                self.rollback_snapshot = None;
                return Err(error);
            }
        };
        self.removed_access_paths = removed_access_paths;
        if let Err(error) = dismount_volumes(&pinned_snapshot, &self.removed_access_paths) {
            // The helper rolls paths back on failure, but retain the pre-mutation journal and mark
            // cleanup pending so a failed best-effort restoration can be retried by `eject`.
            self.volumes_dismounted = true;
            return Err(error);
        }
        self.volumes_dismounted = true;
        drop(device_pin);

        let prepared = (|| {
            let dismounted_snapshot = query_disk(identity.number)?;
            validate_snapshot(selected, &identity, &dismounted_snapshot, Some(false))?;

            // Acquiring this zero-share handle is the decisive proof that no mounted filesystem or
            // competing process still owns the target. It is retained across the final identity
            // check and handed directly to the writer, so the path is never reopened for writing.
            let target = open_exclusive_target(&identity.device)?;
            let exclusive_snapshot = query_disk(identity.number)?;
            validate_snapshot(selected, &identity, &exclusive_snapshot, Some(false))?;
            self.prepared_target = Some(target);
            self.post_write_identity = Some(PostWriteIdentity::from(&exclusive_snapshot));
            Ok(())
        })();
        if let Err(error) = prepared {
            return match self.rollback_unmount(selected) {
                Ok(()) => Err(error),
                Err(rollback) => Err(WorkerError::Platform(format!(
                    "{error}; Windows access-path rollback also failed: {rollback}"
                ))),
            };
        }
        Ok(())
    }

    fn open_target(
        &mut self,
        selected: &WorkerDrive,
        _verify: bool,
    ) -> Result<Self::Target, WorkerError> {
        if !self.volumes_dismounted {
            return Err(WorkerError::UnsafeTarget);
        }
        let identity = SelectedDisk::parse(selected)?;
        let snapshot = query_disk(identity.number)?;
        validate_snapshot(selected, &identity, &snapshot, Some(false))?;
        let target = self
            .prepared_target
            .take()
            .ok_or(WorkerError::UnsafeTarget)?;
        self.post_write_pin = Some(
            target
                .try_clone()
                .map_err(|error| WorkerError::Platform(error.to_string()))?,
        );
        self.write_may_have_started = true;
        self.rollback_snapshot = None;
        self.removed_access_paths.clear();
        Ok(WindowsTarget::new(target))
    }

    fn prepare_verification(
        &mut self,
        selected: &WorkerDrive,
        target: &mut Self::Target,
    ) -> Result<(), crate::flash::FlashError> {
        let identity = SelectedDisk::parse(selected).map_err(|error| worker_as_io(&error))?;
        let before = query_disk(identity.number).map_err(|error| worker_as_io(&error))?;
        let expected = self
            .post_write_identity
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)
            .map_err(|error| worker_as_io(&error))?;
        validate_post_write_snapshot(selected, &identity, &before, expected)
            .map_err(|error| worker_as_io(&error))?;

        // Both clones refer to the zero-share write-through handle. Flush first, then close every
        // clone so Windows will grant a new zero-share, NO_BUFFERING read handle.
        target.sync_all()?;
        drop(self.post_write_pin.take());
        target.begin_verification(
            &identity.device,
            before.size,
            before.logical_sector_size,
            before.physical_sector_size,
        )?;

        // Revalidate while the new exclusive handle pins the object. If the device number was
        // recycled during the close/reopen window, verification stops before hashing any bytes.
        let pinned = query_disk(identity.number).map_err(|error| worker_as_io(&error))?;
        validate_post_write_snapshot(selected, &identity, &pinned, expected)
            .map_err(|error| worker_as_io(&error))?;
        Ok(())
    }

    fn eject(&mut self, selected: &WorkerDrive) -> Result<(), WorkerError> {
        if self.volumes_dismounted && !self.write_may_have_started {
            return self.rollback_unmount(selected);
        }
        if !self.volumes_dismounted && !self.write_may_have_started {
            return Ok(());
        }
        // A failure can occur after the exclusive handle was prepared but before it was handed to
        // the writer. Release it before asking the volume manager for a final dismount.
        drop(self.prepared_target.take());
        let identity = SelectedDisk::parse(selected)?;
        // `open_target` duplicates the same exclusive kernel handle before handing it to the
        // writer. Retain that pin across the first post-write identity query so the device number
        // cannot be recycled between the write and cleanup.
        let snapshot = query_disk(identity.number)?;
        let post_write_identity = self
            .post_write_identity
            .as_ref()
            .ok_or(WorkerError::UnsafeTarget)?;
        validate_post_write_snapshot(selected, &identity, &snapshot, post_write_identity)?;
        drop(self.post_write_pin.take());

        let device_pin = open_device_pin(&identity.device)?;
        let pinned = query_disk(identity.number)?;
        validate_post_write_snapshot(selected, &identity, &pinned, post_write_identity)?;
        let access_paths = query_access_paths(&pinned)?;
        dismount_volumes(&pinned, &access_paths)?;
        let dismounted = query_disk(identity.number)?;
        validate_post_write_snapshot(selected, &identity, &dismounted, post_write_identity)?;
        drop(device_pin);

        // Reacquiring an exclusive handle proves that the freshly written partition table did not
        // race an automatic mount. Closing that handle after WRITE_THROUGH flushing leaves the
        // removable medium in the safe-removal state even on controllers without inbox eject.
        let proof = open_exclusive_target(&identity.device)?;
        proof
            .sync_all()
            .map_err(|error| WorkerError::Platform(error.to_string()))?;
        drop(proof);
        self.volumes_dismounted = true;
        self.write_may_have_started = true;
        Ok(())
    }
}

fn query_disk(number: u32) -> Result<DiskSnapshot, WorkerError> {
    let environment = [(DISK_NUMBER_ENV.to_owned(), number.to_string())];
    let mut snapshot = parse_snapshot(&run_powershell(QUERY_DISK_SCRIPT, &environment)?)?;
    snapshot.normalize();
    Ok(snapshot)
}

fn query_access_paths(snapshot: &DiskSnapshot) -> Result<Vec<RemovedAccessPath>, WorkerError> {
    let output = run_powershell(QUERY_ACCESS_PATHS_SCRIPT, &snapshot.helper_environment())?;
    let paths: Vec<RemovedAccessPath> = serde_json::from_slice(&output).map_err(|error| {
        WorkerError::Platform(format!(
            "Windows returned invalid access-path inventory data: {error}"
        ))
    })?;
    if paths.iter().any(|path| !path.is_valid()) {
        return Err(WorkerError::Platform(
            "Windows returned an unsafe access-path rollback record".to_owned(),
        ));
    }
    Ok(paths)
}

fn dismount_volumes(
    snapshot: &DiskSnapshot,
    paths: &[RemovedAccessPath],
) -> Result<(), WorkerError> {
    let json =
        serde_json::to_string(paths).map_err(|error| WorkerError::Platform(error.to_string()))?;
    if json.len() > MAX_POWERSHELL_OUTPUT {
        return Err(WorkerError::Platform(
            "Windows access-path inventory exceeded the safety limit".to_owned(),
        ));
    }
    let mut environment = snapshot.helper_environment().to_vec();
    environment.push((ACCESS_PATHS_ENV.to_owned(), json));
    run_powershell(DISMOUNT_VOLUMES_SCRIPT, &environment)?;
    Ok(())
}

fn restore_access_paths(
    snapshot: &DiskSnapshot,
    paths: &[RemovedAccessPath],
) -> Result<(), WorkerError> {
    if paths.is_empty() {
        return Ok(());
    }
    let json =
        serde_json::to_string(paths).map_err(|error| WorkerError::Platform(error.to_string()))?;
    if json.len() > MAX_POWERSHELL_OUTPUT {
        return Err(WorkerError::Platform(
            "Windows access-path rollback data exceeded the safety limit".to_owned(),
        ));
    }
    let mut environment = snapshot.helper_environment().to_vec();
    environment.push((ACCESS_PATHS_ENV.to_owned(), json));
    run_powershell(RESTORE_ACCESS_PATHS_SCRIPT, &environment)?;
    Ok(())
}

fn open_device_pin(device: &str) -> Result<File, WorkerError> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(device)
        .map_err(|error| device_open_error(&error))
}

fn open_exclusive_target(device: &str) -> Result<File, WorkerError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
        .open(device)
        .map_err(|error| device_open_error(&error))
}

fn open_unbuffered_target(device: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH)
        .open(device)
}

fn worker_as_io(error: &WorkerError) -> crate::flash::FlashError {
    crate::flash::FlashError::Io(io::Error::other(error.to_string()))
}

fn device_open_error(error: &io::Error) -> WorkerError {
    WorkerError::Platform(format!(
        "could not lock the physical disk; close Explorer and other applications using it, then retry: {error}"
    ))
}

fn parse_snapshot(bytes: &[u8]) -> Result<DiskSnapshot, WorkerError> {
    serde_json::from_slice(bytes).map_err(|error| {
        WorkerError::Platform(format!(
            "Windows returned invalid physical-disk data: {error}"
        ))
    })
}

fn validate_snapshot(
    selected: &WorkerDrive,
    identity: &SelectedDisk,
    snapshot: &DiskSnapshot,
    expected_offline: Option<bool>,
) -> Result<(), WorkerError> {
    validate_common_safety(selected, identity, snapshot)?;
    if expected_offline.is_some_and(|expected| snapshot.is_offline.is_set() != expected)
        || snapshot.fingerprint()? != identity.fingerprint
    {
        return Err(WorkerError::TargetChanged);
    }
    let current = WorkerDrive {
        id: format!("{}@{}", identity.device, snapshot.fingerprint()?),
        device: identity.device.clone(),
        capacity: snapshot.size,
    };
    compare_drive(selected, &current)
}

fn validate_post_write_snapshot(
    selected: &WorkerDrive,
    identity: &SelectedDisk,
    snapshot: &DiskSnapshot,
    expected: &PostWriteIdentity,
) -> Result<(), WorkerError> {
    // A raw image may replace the disk signature/GPT GUID represented by Get-Disk.UniqueId, so
    // the pre-write fingerprint is intentionally not required after the exclusive handle closes.
    // The hardware-facing path and serial remain stable, however, and prevent a same-size medium
    // swapped onto the same PhysicalDrive number from being taken offline after the write.
    validate_common_safety(selected, identity, snapshot)?;
    if snapshot.number != expected.number
        || snapshot.path != expected.path
        || snapshot.serial_number != expected.serial_number
        || snapshot.size != expected.size
        || snapshot.logical_sector_size != expected.logical_sector_size
        || snapshot.physical_sector_size != expected.physical_sector_size
        || snapshot.bus_type != expected.bus_type
        || snapshot.supports_removable_media != expected.supports_removable_media
    {
        return Err(WorkerError::TargetChanged);
    }
    Ok(())
}

fn validate_common_safety(
    selected: &WorkerDrive,
    identity: &SelectedDisk,
    snapshot: &DiskSnapshot,
) -> Result<(), WorkerError> {
    if snapshot.number != identity.number {
        return Err(WorkerError::UnsafeTarget);
    }
    if snapshot.size != selected.capacity {
        return Err(WorkerError::UnsafeTarget);
    }
    let unsafe_status = snapshot.is_boot.is_set()
        || snapshot.is_system.is_set()
        || snapshot.is_offline.is_set()
        || snapshot.is_read_only.is_set()
        || !snapshot.is_card_target()
        || !valid_sector_geometry(
            snapshot.size,
            snapshot.logical_sector_size,
            snapshot.physical_sector_size,
        )
        || snapshot.path.is_empty()
        || (snapshot.unique_id.is_empty() && snapshot.serial_number.is_empty());
    if unsafe_status {
        return Err(WorkerError::UnsafeTarget);
    }
    Ok(())
}

fn valid_sector_geometry(size: u64, logical: u32, physical: u32) -> bool {
    (512..=65_536).contains(&logical)
        && logical.is_power_of_two()
        && (logical..=65_536).contains(&physical)
        && physical.is_power_of_two()
        && size.is_multiple_of(u64::from(logical))
}

fn run_powershell(program: &str, environment: &[(String, String)]) -> Result<Vec<u8>, WorkerError> {
    let output = windows_powershell_command()?
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            program,
        ])
        .envs(environment.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| WorkerError::Platform(error.to_string()))?;
    if output.stdout.len() > MAX_POWERSHELL_OUTPUT || output.stderr.len() > MAX_POWERSHELL_OUTPUT {
        return Err(WorkerError::Platform(
            "Windows disk helper output exceeded the safety limit".to_owned(),
        ));
    }
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(WorkerError::Platform(if message.is_empty() {
            format!("Windows disk helper failed with status {}", output.status)
        } else {
            message
        }));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> DiskSnapshot {
        DiskSnapshot {
            number: 7,
            path: r"\\?\usbstor#disk&ven_snapdog&prod_test".to_owned(),
            unique_id: "MEDIA-1234".to_owned(),
            serial_number: "SERIAL-5678".to_owned(),
            size: 32_000_000_000,
            logical_sector_size: 512,
            physical_sector_size: 4_096,
            is_boot: BoolFlag(false),
            is_system: BoolFlag(false),
            is_offline: BoolFlag(false),
            is_read_only: BoolFlag(false),
            bus_type: "USB".to_owned(),
            supports_removable_media: BoolFlag(true),
        }
    }

    fn selected(snapshot: &DiskSnapshot) -> WorkerDrive {
        let device = format!(r"\\.\PHYSICALDRIVE{}", snapshot.number);
        WorkerDrive {
            id: format!("{device}@{}", snapshot.fingerprint().unwrap()),
            device,
            capacity: snapshot.size,
        }
    }

    #[test]
    fn stable_identity_round_trips() {
        let snapshot = snapshot();
        let selected = selected(&snapshot);
        let identity = SelectedDisk::parse(&selected).unwrap();
        assert_eq!(identity.number, 7);
        assert_eq!(identity.device, r"\\.\PHYSICALDRIVE7");
        assert!(validate_snapshot(&selected, &identity, &snapshot, Some(false)).is_ok());
    }

    #[test]
    fn identity_rejects_partitions_missing_fingerprint_and_aliases() {
        let snapshot = snapshot();
        let mut drive = selected(&snapshot);
        drive.id = r"\\.\PHYSICALDRIVE7".to_owned();
        assert!(SelectedDisk::parse(&drive).is_err());

        drive.id = format!(r"\\.\PHYSICALDRIVE07@{}", snapshot.fingerprint().unwrap());
        drive.device = r"\\.\PHYSICALDRIVE07".to_owned();
        assert!(SelectedDisk::parse(&drive).is_err());

        drive.id = format!(
            r"\\.\PHYSICALDRIVE7\Partition0@{}",
            snapshot.fingerprint().unwrap()
        );
        drive.device = r"\\.\PHYSICALDRIVE7\Partition0".to_owned();
        assert!(SelectedDisk::parse(&drive).is_err());
    }

    #[test]
    fn rejects_boot_system_read_only_and_non_removable_disks() {
        for mutate in [
            |disk: &mut DiskSnapshot| disk.is_boot = BoolFlag(true),
            |disk: &mut DiskSnapshot| disk.is_system = BoolFlag(true),
            |disk: &mut DiskSnapshot| disk.is_read_only = BoolFlag(true),
            |disk: &mut DiskSnapshot| disk.bus_type = "NVME".to_owned(),
            |disk: &mut DiskSnapshot| disk.supports_removable_media = BoolFlag(false),
        ] {
            let original = snapshot();
            let selected = selected(&original);
            let identity = SelectedDisk::parse(&selected).unwrap();
            let mut changed = original;
            mutate(&mut changed);
            assert!(matches!(
                validate_snapshot(&selected, &identity, &changed, None),
                Err(WorkerError::UnsafeTarget)
            ));
        }
    }

    #[test]
    fn built_in_sd_and_mmc_do_not_depend_on_usb_removable_capability() {
        for bus_type in ["SD", "MMC"] {
            let mut card = snapshot();
            card.bus_type = bus_type.to_owned();
            card.supports_removable_media = BoolFlag(false);
            let selected = selected(&card);
            let identity = SelectedDisk::parse(&selected).unwrap();
            assert!(validate_snapshot(&selected, &identity, &card, Some(false)).is_ok());
        }
    }

    #[test]
    fn rejects_unsafe_sector_geometry() {
        for mutate in [
            |disk: &mut DiskSnapshot| disk.logical_sector_size = 0,
            |disk: &mut DiskSnapshot| disk.logical_sector_size = 1_000,
            |disk: &mut DiskSnapshot| disk.physical_sector_size = 128,
            |disk: &mut DiskSnapshot| disk.physical_sector_size = 131_072,
            |disk: &mut DiskSnapshot| disk.size += 1,
        ] {
            let original = snapshot();
            let selected = selected(&original);
            let identity = SelectedDisk::parse(&selected).unwrap();
            let mut changed = original;
            mutate(&mut changed);
            assert!(matches!(
                validate_snapshot(&selected, &identity, &changed, None),
                Err(WorkerError::UnsafeTarget)
            ));
        }
    }

    #[test]
    fn aligned_verifier_handles_full_and_unaligned_logical_reads() {
        let mut file = tempfile::tempfile().unwrap();
        let bytes = (0_u16..16_384)
            .map(|value| u8::try_from(value % 251).unwrap())
            .collect::<Vec<_>>();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();

        let mut verifier = UnbufferedVerifier::new(file, 16_384, 512, 4_096).unwrap();
        let mut first = vec![0_u8; 6_000];
        assert_eq!(verifier.read(&mut first).unwrap(), first.len());
        assert_eq!(first, bytes[..first.len()]);

        verifier.seek(SeekFrom::Start(123)).unwrap();
        let mut unaligned = vec![0_u8; 1_001];
        assert_eq!(verifier.read(&mut unaligned).unwrap(), unaligned.len());
        assert_eq!(unaligned, bytes[123..123 + unaligned.len()]);
    }

    #[test]
    fn detects_media_swap_and_offline_state_mismatch() {
        let original = snapshot();
        let selected = selected(&original);
        let identity = SelectedDisk::parse(&selected).unwrap();

        let mut swapped = original.clone();
        swapped.unique_id = "DIFFERENT-MEDIA".to_owned();
        assert!(matches!(
            validate_snapshot(&selected, &identity, &swapped, Some(false)),
            Err(WorkerError::TargetChanged)
        ));

        let mut offline = original;
        offline.is_offline = BoolFlag(true);
        assert!(matches!(
            validate_snapshot(&selected, &identity, &offline, Some(false)),
            Err(WorkerError::UnsafeTarget)
        ));
    }

    #[test]
    fn dismount_helper_locks_volumes_without_set_disk_or_dynamic_source() {
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("Win32_Volume"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("-MethodName Dismount"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("Remove-PartitionAccessPath"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("Add-PartitionAccessPath"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("Force = $false"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("Permanent = $true"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("Permanent = $false"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("if ($status -eq 3)"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("4 { throw"));
        assert!(!DISMOUNT_VOLUMES_SCRIPT.contains("Force = $true"));
        assert!(!DISMOUNT_VOLUMES_SCRIPT.contains("Set-Disk"));
        assert!(!DISMOUNT_VOLUMES_SCRIPT.contains("Invoke-Expression"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("SNAPDOG_STORAGE_MODULE"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("SNAPDOG_CIM_MODULE"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("$env:PSModulePath = ''"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("Capabilities) -contains [uint16]7"));
        assert!(DISMOUNT_VOLUMES_SCRIPT.contains("SNAPDOG_ACCESS_PATHS_JSON"));
        assert!(QUERY_ACCESS_PATHS_SCRIPT.contains("Get-Partition"));
        assert!(RESTORE_ACCESS_PATHS_SCRIPT.contains("Add-PartitionAccessPath"));
        assert!(!RESTORE_ACCESS_PATHS_SCRIPT.contains("Invoke-Expression"));
        assert_eq!(FILE_FLAG_NO_BUFFERING, 0x2000_0000);
    }

    #[test]
    fn snapshot_helper_environment_has_fixed_order_and_shape() {
        let environment = snapshot().helper_environment();
        assert_eq!(environment[0], (DISK_NUMBER_ENV.to_owned(), "7".to_owned()));
        assert_eq!(environment[1].1, "32000000000");
        assert_eq!(environment[2].1, "512");
        assert_eq!(environment[3].1, "4096");
        assert_eq!(environment[7].1, "USB");
        assert_eq!(environment[8].1, "1");
    }

    #[test]
    fn parses_fixed_shape_powershell_json() {
        let json = br#"{
            "Number":7,
            "Path":"  \\\\?\\usbstor#disk  ",
            "UniqueId":" MEDIA-1234 ",
            "SerialNumber":" SERIAL-5678 ",
            "Size":32000000000,
            "LogicalSectorSize":512,
            "PhysicalSectorSize":4096,
            "IsBoot":false,
            "IsSystem":false,
            "IsOffline":true,
            "IsReadOnly":false,
            "BusType":" usb ",
            "SupportsRemovableMedia":true
        }"#;
        let mut parsed = parse_snapshot(json).unwrap();
        parsed.normalize();
        assert_eq!(parsed.path, r"\\?\usbstor#disk");
        assert_eq!(parsed.unique_id, "MEDIA-1234");
        assert_eq!(parsed.serial_number, "SERIAL-5678");
        assert_eq!(parsed.bus_type, "USB");
        assert!(parsed.supports_removable_media.is_set());
        assert!(valid_sector_geometry(
            parsed.size,
            parsed.logical_sector_size,
            parsed.physical_sector_size
        ));
    }

    #[test]
    fn rollback_records_reject_ambiguous_values() {
        assert!(
            RemovedAccessPath {
                partition_number: 1,
                access_path: "E:\\".to_owned(),
            }
            .is_valid()
        );
        assert!(
            !RemovedAccessPath {
                partition_number: 0,
                access_path: "E:\\".to_owned(),
            }
            .is_valid()
        );
        assert!(
            !RemovedAccessPath {
                partition_number: 1,
                access_path: "bad\0path".to_owned(),
            }
            .is_valid()
        );
    }
}
