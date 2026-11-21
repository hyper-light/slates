//! Windows: the volume and disk under a path, from the volume management functions and
//! IOCTL_STORAGE_QUERY_PROPERTY.
//!
//! GetVolumePathNameW gives the mount point that holds the path, GetVolumeInformationW its
//! file system, GetDriveTypeW whether it is remote or a RAM disk, and GetDiskFreeSpaceExW
//! its space. The volume, opened by its GUID path with no access rights (which needs no
//! privilege), answers IOCTL_STORAGE_QUERY_PROPERTY for its disk: the seek penalty
//! (DEVICE_SEEK_PENALTY_DESCRIPTOR), bus type and product (STORAGE_DEVICE_DESCRIPTOR),
//! sector sizes (STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR) and write cache
//! (STORAGE_WRITE_CACHE_PROPERTY) and zone model (STORAGE_ZONED_DEVICE_DESCRIPTOR). A volume
//! spanning several disks answers for none of them, which is noted. A path in the device
//! namespace (`\\.\PhysicalDrive1`, `\\.\D:`) names a disk or volume itself, which is opened
//! and asked the same questions.
//!
//! Descriptors are read from the returned bytes at `offset_of!` offsets of the windows-sys
//! definitions, never by casting: their BOOLEAN fields are Rust `bool`, and a byte other
//! than 0 or 1 would make such a cast undefined.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, MAX_PATH,
};
use windows_sys::Win32::Storage::FileSystem::{
    BusType1394, BusTypeAta, BusTypeAtapi, BusTypeFibre, BusTypeFileBackedVirtual, BusTypeMmc,
    BusTypeNvme, BusTypeRAID, BusTypeSCM, BusTypeSas, BusTypeSata, BusTypeScsi, BusTypeSd,
    BusTypeSpaces, BusTypeUfs, BusTypeUsb, BusTypeVirtual, BusTypeiScsi, CreateFileW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW,
    GetDriveTypeW, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_DESCRIPTOR_HEADER, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_ID, STORAGE_PROPERTY_QUERY, STORAGE_WRITE_CACHE_PROPERTY,
    STORAGE_ZONED_DEVICE_DESCRIPTOR, StorageAccessAlignmentProperty, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty, StorageDeviceWriteCacheProperty,
    StorageDeviceZonedDeviceProperty, WriteCacheDisabled, WriteCacheEnabled,
    WriteCacheTypeWriteBack, WriteCacheTypeWriteThrough, ZonedDeviceTypeDeviceManaged,
    ZonedDeviceTypeHostAware, ZonedDeviceTypeHostManaged,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_RAMDISK, DRIVE_REMOTE};

use crate::identity::{
    FileSystem, FileSystemKind, Identity, Interconnect, Medium, WriteCache, Zoned,
};

/// Wide characters of a volume GUID path with its terminator: "A reasonable size for the
/// buffer to accommodate the largest possible volume GUID path is 50 characters"
/// (GetVolumeNameForVolumeMountPointW, Microsoft Learn).
const VOLUME_GUID_CHARS: usize = 50;

/// A kernel handle this code owns, closed on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle returned by CreateFileW that this value owns.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(buf.get(..end).unwrap_or_default())
}

fn last_error() -> String {
    // SAFETY: GetLastError reads the calling thread's last-error value.
    let code = unsafe { GetLastError() };
    std::io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX)).to_string()
}

pub fn identify(path: &Path) -> Identity {
    if path.as_os_str().to_string_lossy().starts_with(r"\\.\") {
        return node(path);
    }
    let mut identity = Identity::unknown(FileSystem {
        kind: FileSystemKind::Unknown,
        block_size: None,
        total_bytes: None,
        available_bytes: None,
    });
    let Some(root) = volume_root(path, &mut identity) else {
        return identity;
    };
    let root_w = wide(std::ffi::OsStr::new(&root));
    identity.file_system = file_system(&root_w, &mut identity);

    // SAFETY: `root_w` is NUL-terminated and outlives the call.
    match unsafe { GetDriveTypeW(root_w.as_ptr()) } {
        DRIVE_REMOTE => {
            identity.interconnect = Interconnect::Network;
            return identity;
        }
        DRIVE_RAMDISK => {
            identity.medium = Medium::Memory;
            identity.interconnect = Interconnect::Memory;
            return identity;
        }
        _ => {}
    }

    let Some(volume) = open_volume(&root_w, &mut identity) else {
        return identity;
    };
    describe(&volume, &mut identity);
    identity
}

/// A disk or volume named in the device namespace. Its capacity needs a handle with read
/// access, which `DeviceFile` has and this probe does not ask for.
fn node(path: &Path) -> Identity {
    let mut identity = Identity::unknown(FileSystem {
        kind: FileSystemKind::Device,
        block_size: None,
        total_bytes: None,
        available_bytes: None,
    });
    let name = path.as_os_str().to_string_lossy().into_owned();
    identity.device = Some(name.clone());
    if let Some(device) = open_device(&name, &mut identity) {
        describe(&device, &mut identity);
    }
    identity
}

/// Fills `identity` from the storage queries a disk or volume handle answers.
fn describe(volume: &Handle, identity: &mut Identity) {
    match query(volume, StorageDeviceSeekPenaltyProperty) {
        Ok(bytes) => {
            let at = offset_of!(DEVICE_SEEK_PENALTY_DESCRIPTOR, IncursSeekPenalty);
            identity.medium = match bytes.get(at) {
                Some(0) => Medium::SolidState,
                Some(_) => Medium::Rotational,
                None => Medium::Unknown,
            };
        }
        Err(e) => identity.note("StorageDeviceSeekPenaltyProperty", e),
    }
    match query(volume, StorageDeviceProperty) {
        Ok(bytes) => describe_device(&bytes, identity),
        Err(e) => identity.note("StorageDeviceProperty", e),
    }
    match query(volume, StorageAccessAlignmentProperty) {
        Ok(bytes) => {
            identity.logical_block = u32_at(
                &bytes,
                offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerLogicalSector),
            );
            identity.physical_block = u32_at(
                &bytes,
                offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerPhysicalSector),
            );
        }
        Err(e) => identity.note("StorageAccessAlignmentProperty", e),
    }
    match query(volume, StorageDeviceWriteCacheProperty) {
        Ok(bytes) => {
            let kind = i32_at(
                &bytes,
                offset_of!(STORAGE_WRITE_CACHE_PROPERTY, WriteCacheType),
            );
            let enabled = i32_at(
                &bytes,
                offset_of!(STORAGE_WRITE_CACHE_PROPERTY, WriteCacheEnabled),
            );
            identity.write_cache = write_cache(kind, enabled);
        }
        Err(e) => identity.note("StorageDeviceWriteCacheProperty", e),
    }
    match query(volume, StorageDeviceZonedDeviceProperty) {
        Ok(bytes) => {
            let kind = i32_at(
                &bytes,
                offset_of!(STORAGE_ZONED_DEVICE_DESCRIPTOR, DeviceType),
            );
            identity.zoned = zoned(kind);
        }
        Err(e) => identity.note("StorageDeviceZonedDeviceProperty", e),
    }
}

/// The mount point holding `path`, with its trailing separator.
fn volume_root(path: &Path, identity: &mut Identity) -> Option<String> {
    let path_w = wide(path.as_os_str());
    // "A reasonable size for the buffer to accommodate the largest possible volume path is the
    // length of the full path specified by lpszFileName" (GetVolumePathNameW, Microsoft
    // Learn), and one more for the separator a mount point's path gains.
    let full = match std::path::absolute(path) {
        Ok(full) => wide(full.as_os_str()).len(),
        Err(e) => {
            identity.note(format!("absolute {}", path.display()), e.to_string());
            return None;
        }
    };
    let mut root = vec![0u16; full.checked_add(1)?];
    let len = u32::try_from(root.len()).ok()?;
    // SAFETY: `path_w` is NUL-terminated; `root` is writable for `len` wide characters.
    let ok = unsafe { GetVolumePathNameW(path_w.as_ptr(), root.as_mut_ptr(), len) };
    if ok == 0 {
        identity.note(
            format!("GetVolumePathNameW {}", path.display()),
            last_error(),
        );
        return None;
    }
    Some(from_wide(&root))
}

fn file_system(root_w: &[u16], identity: &mut Identity) -> FileSystem {
    // "The maximum buffer size is MAX_PATH+1" (GetVolumeInformationW, Microsoft Learn).
    let mut name = vec![0u16; usize::try_from(MAX_PATH).unwrap_or(0).saturating_add(1)];
    let name_len = u32::try_from(name.len()).unwrap_or(0);
    // SAFETY: `root_w` is NUL-terminated; `name` is writable for `name_len` wide characters;
    // the optional outputs are null.
    let ok = unsafe {
        GetVolumeInformationW(
            root_w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name_len,
        )
    };
    let kind = if ok == 0 {
        identity.note("GetVolumeInformationW", last_error());
        FileSystemKind::Unknown
    } else {
        kind_of(&from_wide(&name))
    };
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `root_w` is NUL-terminated and the three outputs are writable u64s.
    let ok = unsafe { GetDiskFreeSpaceExW(root_w.as_ptr(), &mut available, &mut total, &mut free) };
    if ok == 0 {
        identity.note("GetDiskFreeSpaceExW", last_error());
        return FileSystem {
            kind,
            block_size: None,
            total_bytes: None,
            available_bytes: None,
        };
    }
    FileSystem {
        kind,
        block_size: None,
        total_bytes: Some(total),
        available_bytes: Some(available),
    }
}

/// Opens the volume under `root` by its GUID path with no access rights: enough for
/// IOCTL_STORAGE_QUERY_PROPERTY, and it needs no privilege.
fn open_volume(root_w: &[u16], identity: &mut Identity) -> Option<Handle> {
    let mut guid = vec![0u16; VOLUME_GUID_CHARS];
    let len = u32::try_from(guid.len()).ok()?;
    // SAFETY: `root_w` is NUL-terminated; `guid` is writable for `len` wide characters.
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(root_w.as_ptr(), guid.as_mut_ptr(), len) };
    if ok == 0 {
        identity.note("GetVolumeNameForVolumeMountPointW", last_error());
        return None;
    }
    // "\\?\Volume{GUID}\" names the root directory; without the trailing backslash it names
    // the volume device, which is what CreateFileW must open.
    let mut name = from_wide(&guid);
    if name.ends_with('\\') {
        name.pop();
    }
    identity.device = Some(name.clone());
    open_device(&name, identity)
}

/// Opens a device by name with no access rights: enough for IOCTL_STORAGE_QUERY_PROPERTY, and
/// it needs no privilege.
fn open_device(name: &str, identity: &mut Identity) -> Option<Handle> {
    let name_w = wide(std::ffi::OsStr::new(name));
    // SAFETY: `name_w` is NUL-terminated; no security attributes or template are passed.
    let handle = unsafe {
        CreateFileW(
            name_w.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        identity.note(format!("CreateFileW {name}"), last_error());
        return None;
    }
    Some(Handle(handle))
}

/// Runs a standard IOCTL_STORAGE_QUERY_PROPERTY query and returns the bytes written: first
/// for the descriptor's header, whose `Size` is the bytes the whole descriptor takes, then
/// for the descriptor in a buffer of that size (IOCTL_STORAGE_QUERY_PROPERTY, Microsoft
/// Learn).
fn query(volume: &Handle, property: STORAGE_PROPERTY_ID) -> Result<Vec<u8>, String> {
    let header = query_into(volume, property, size_of::<STORAGE_DESCRIPTOR_HEADER>())?;
    let size = u32_at(&header, offset_of!(STORAGE_DESCRIPTOR_HEADER, Size))
        .ok_or("a descriptor header shorter than its fields")?;
    let size = usize::try_from(size).map_err(|e| e.to_string())?;
    if size > hyper_block::buf::MAX_BUFFER {
        return Err(format!("a descriptor of {size} bytes"));
    }
    query_into(volume, property, size.max(header.len()))
}

/// One IOCTL_STORAGE_QUERY_PROPERTY query into a buffer of `len` bytes.
fn query_into(
    volume: &Handle,
    property: STORAGE_PROPERTY_ID,
    len: usize,
) -> Result<Vec<u8>, String> {
    let request = STORAGE_PROPERTY_QUERY {
        PropertyId: property,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut out = vec![0u8; len];
    let mut returned = 0u32;
    let in_len = u32::try_from(size_of::<STORAGE_PROPERTY_QUERY>()).map_err(|e| e.to_string())?;
    let out_len = u32::try_from(out.len()).map_err(|e| e.to_string())?;
    // SAFETY: `volume` is a live handle; the input points to a STORAGE_PROPERTY_QUERY of
    // `in_len` bytes and the output is writable for `out_len` bytes; the call is synchronous
    // (no OVERLAPPED), so both buffers outlive it.
    let ok = unsafe {
        DeviceIoControl(
            volume.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&raw const request).cast::<c_void>(),
            in_len,
            out.as_mut_ptr().cast::<c_void>(),
            out_len,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    out.truncate(usize::try_from(returned).unwrap_or(0));
    Ok(out)
}

fn describe_device(bytes: &[u8], identity: &mut Identity) {
    identity.interconnect = i32_at(bytes, offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType))
        .map_or(Interconnect::Unknown, interconnect_of);
    identity.removable = bytes
        .get(offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia))
        .map(|&b| b != 0);
    let vendor = string_at(bytes, offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset));
    let product = string_at(
        bytes,
        offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset),
    );
    identity.model = match (vendor, product) {
        (Some(v), Some(p)) => Some(format!("{v} {p}")),
        (v, p) => v.or(p),
    };
}

/// STORAGE_BUS_TYPE values (ntddstor.h).
fn interconnect_of(bus: i32) -> Interconnect {
    let is = |types: &[i32]| types.contains(&bus);
    if is(&[BusTypeNvme]) {
        Interconnect::Nvme
    } else if is(&[
        BusTypeSata,
        BusTypeAta,
        BusTypeAtapi,
        BusTypeScsi,
        BusTypeSas,
    ]) {
        Interconnect::Scsi
    } else if is(&[BusTypeUsb]) {
        Interconnect::Usb
    } else if is(&[BusTypeSd, BusTypeMmc]) {
        Interconnect::Mmc
    } else if is(&[BusTypeVirtual, BusTypeFileBackedVirtual]) {
        Interconnect::Virtual
    } else if is(&[BusTypeSpaces, BusTypeRAID]) {
        Interconnect::Composite
    } else if is(&[BusTypeiScsi, BusTypeFibre]) {
        Interconnect::Network
    } else if is(&[BusTypeSCM]) {
        Interconnect::Other("storage class memory".to_owned())
    } else if is(&[BusTypeUfs]) {
        Interconnect::Other("UFS".to_owned())
    } else if is(&[BusType1394]) {
        Interconnect::Other("IEEE 1394".to_owned())
    } else {
        Interconnect::Other(format!("STORAGE_BUS_TYPE {bus}"))
    }
}

/// STORAGE_ZONED_DEVICE_TYPES: a drive-managed zoned device takes writes anywhere and
/// places them itself, so to the host it is not zoned.
fn zoned(kind: Option<i32>) -> Zoned {
    match kind {
        Some(k) if k == ZonedDeviceTypeHostManaged => Zoned::HostManaged,
        Some(k) if k == ZonedDeviceTypeHostAware => Zoned::HostAware,
        Some(k) if k == ZonedDeviceTypeDeviceManaged => Zoned::None,
        _ => Zoned::Unknown,
    }
}

/// STORAGE_WRITE_CACHE_PROPERTY: a write-back cache that is enabled holds acknowledged
/// writes until flushed; a write-through or disabled one does not.
fn write_cache(kind: Option<i32>, enabled: Option<i32>) -> WriteCache {
    match (kind, enabled) {
        (Some(k), Some(e)) if k == WriteCacheTypeWriteBack && e == WriteCacheEnabled => {
            WriteCache::WriteBack
        }
        (Some(k), Some(e)) if k == WriteCacheTypeWriteBack && e == WriteCacheDisabled => {
            WriteCache::WriteThrough
        }
        (Some(k), _) if k == WriteCacheTypeWriteThrough => WriteCache::WriteThrough,
        _ => WriteCache::Unknown,
    }
}

/// The NUL-terminated ASCII string whose offset is stored at `field`; zero means absent.
fn string_at(bytes: &[u8], field: usize) -> Option<String> {
    let offset = usize::try_from(u32_at(bytes, field)?).ok()?;
    if offset == 0 {
        return None;
    }
    let tail = bytes.get(offset..)?;
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    let s = String::from_utf8_lossy(tail.get(..end)?).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn i32_at(bytes: &[u8], at: usize) -> Option<i32> {
    let raw: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(i32::from_le_bytes(raw))
}

/// GetVolumeInformationW file system names.
fn kind_of(name: &str) -> FileSystemKind {
    match name {
        "NTFS" => FileSystemKind::Ntfs,
        "ReFS" => FileSystemKind::Refs,
        "FAT" | "FAT32" => FileSystemKind::Fat,
        "exFAT" => FileSystemKind::ExFat,
        "" => FileSystemKind::Unknown,
        other => FileSystemKind::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_test_directory() {
        let dir = tempfile::tempdir().unwrap();
        let id = identify(dir.path());
        eprintln!("{id:#?}");
        assert!(id.file_system.available_bytes.is_some(), "{:?}", id.notes);
        assert_ne!(
            id.file_system.kind,
            FileSystemKind::Unknown,
            "{:?}",
            id.notes
        );
    }

    #[test]
    fn write_cache_is_write_back_only_when_enabled() {
        let wb = Some(WriteCacheTypeWriteBack);
        assert_eq!(
            write_cache(wb, Some(WriteCacheEnabled)),
            WriteCache::WriteBack
        );
        assert_eq!(
            write_cache(wb, Some(WriteCacheDisabled)),
            WriteCache::WriteThrough
        );
        assert_eq!(write_cache(wb, Some(0)), WriteCache::Unknown);
        assert_eq!(
            write_cache(Some(WriteCacheTypeWriteThrough), None),
            WriteCache::WriteThrough
        );
        assert_eq!(write_cache(None, None), WriteCache::Unknown);
    }

    #[test]
    fn zone_models_map_to_what_the_host_must_obey() {
        assert_eq!(zoned(Some(ZonedDeviceTypeHostManaged)), Zoned::HostManaged);
        assert_eq!(zoned(Some(ZonedDeviceTypeHostAware)), Zoned::HostAware);
        assert_eq!(zoned(Some(ZonedDeviceTypeDeviceManaged)), Zoned::None);
        assert_eq!(zoned(Some(0)), Zoned::Unknown);
        assert_eq!(zoned(None), Zoned::Unknown);
    }

    #[test]
    fn descriptor_strings_are_bounds_checked() {
        let mut bytes = vec![0u8; 64];
        let field = offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset);
        bytes[field..field + 4].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(string_at(&bytes, field), None);
        bytes[field..field + 4].copy_from_slice(&40u32.to_le_bytes());
        bytes[40..44].copy_from_slice(b"ACME");
        assert_eq!(string_at(&bytes, field).as_deref(), Some("ACME"));
    }
}
