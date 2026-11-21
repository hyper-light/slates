//! macOS: the device under a path, from statfs(2) and the I/O Registry.
//!
//! statfs(2) names the mounted device (`f_mntfromname`, e.g. `/dev/disk3s5`) and the file
//! system (`f_fstypename`). The device's IOMedia object carries its block sizes; the storage
//! device above it in the service plane carries "Device Characteristics" ("Medium Type":
//! "Solid State" or "Rotational") and "Protocol Characteristics" ("Physical Interconnect",
//! "Physical Interconnect Location"), the keys of IOKit's IOStorageDeviceCharacteristics.h
//! and IOStorageProtocolCharacteristics.h. An APFS volume sits on a synthesized container
//! disk, so the characteristics are found by searching the volume's ancestors
//! (IORegistryEntrySearchCFProperty with kIORegistryIterateParents). A device node is the
//! device itself, found by its own BSD name: statfs(2) of a node describes devfs, which holds
//! it.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;

use crate::identity::{FileSystem, FileSystemKind, Identity, Interconnect, Medium, WriteCache};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type CFTypeID = usize;
type MachPort = u32;
type IoObject = MachPort;

const UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64: CFIndex = 4;
const ITERATE_RECURSIVELY: u32 = 0x1;
const ITERATE_PARENTS: u32 = 0x2;
/// kIOMainPortDefault.
const MAIN_PORT: MachPort = 0;
/// The longest string property read; IOKit names and model strings are far shorter.
const MAX_STRING: usize = 256;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFDictionaryGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, out: *mut c_void) -> u8;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOBSDNameMatching(port: MachPort, options: u32, bsd_name: *const c_char) -> CFTypeRef;
    fn IOServiceGetMatchingService(port: MachPort, matching: CFTypeRef) -> IoObject;
    fn IORegistryEntrySearchCFProperty(
        entry: IoObject,
        plane: *const c_char,
        key: CFTypeRef,
        alloc: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFTypeRef,
        alloc: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
    fn IOObjectRelease(object: IoObject) -> i32;
}

/// A Core Foundation object this code owns (a Create or Copy result), released on drop.
struct Owned(CFTypeRef);

impl Owned {
    /// Takes ownership of a Create/Copy result; null (absent) stays `None`. The wrapper is
    /// built only for a non-null object: dropping one releases it.
    fn new(cf: CFTypeRef) -> Option<Self> {
        if cf.is_null() { None } else { Some(Self(cf)) }
    }

    fn string(s: &str) -> Option<Self> {
        let c = CString::new(s).ok()?;
        // SAFETY: `c` is a valid NUL-terminated string for the duration of the call; a null
        // allocator selects the default one. The result is owned (Create rule).
        Self::new(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null object this value owns exactly one reference to.
        unsafe { CFRelease(self.0) }
    }
}

/// An I/O Registry object this code owns, released on drop.
struct Service(IoObject);

impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-zero registry object this value owns one reference to.
        unsafe {
            IOObjectRelease(self.0);
        }
    }
}

impl Service {
    fn for_bsd_name(name: &str) -> Option<Self> {
        let c = CString::new(name).ok()?;
        // SAFETY: `c` outlives the call. IOBSDNameMatching returns a new dictionary or null;
        // IOServiceGetMatchingService consumes that reference whatever it returns, so the
        // dictionary is not released here.
        let service = unsafe {
            let matching = IOBSDNameMatching(MAIN_PORT, 0, c.as_ptr());
            if matching.is_null() {
                return None;
            }
            IOServiceGetMatchingService(MAIN_PORT, matching)
        };
        if service == 0 {
            None
        } else {
            Some(Self(service))
        }
    }

    /// A property of this entry itself.
    fn property(&self, key: &str) -> Option<Owned> {
        let key = Owned::string(key)?;
        // SAFETY: `self.0` is a live registry entry and `key` a live CFString; the result is
        // owned (Create rule) or null.
        Owned::new(unsafe { IORegistryEntryCreateCFProperty(self.0, key.0, std::ptr::null(), 0) })
    }

    /// A property of this entry or the nearest ancestor that has it.
    fn inherited(&self, key: &str) -> Option<Owned> {
        let key = Owned::string(key)?;
        // SAFETY: as `property`; the plane name is a NUL-terminated literal. Search results
        // follow the Create rule.
        Owned::new(unsafe {
            IORegistryEntrySearchCFProperty(
                self.0,
                c"IOService".as_ptr(),
                key.0,
                std::ptr::null(),
                ITERATE_RECURSIVELY | ITERATE_PARENTS,
            )
        })
    }
}

fn type_is(cf: CFTypeRef, type_id: unsafe extern "C" fn() -> CFTypeID) -> bool {
    // SAFETY: `cf` is a live, non-null CF object (callers hold it); the type-ID functions
    // take no arguments.
    !cf.is_null() && unsafe { CFGetTypeID(cf) == type_id() }
}

/// The value under `key` in a dictionary, borrowed from it (Get rule).
fn entry(dict: &Owned, key: &str) -> Option<CFTypeRef> {
    if !type_is(dict.0, CFDictionaryGetTypeID) {
        return None;
    }
    let key = Owned::string(key)?;
    // SAFETY: `dict` is a live CFDictionary and `key` a live CFString; the value is borrowed
    // and used only while `dict` is alive.
    let value = unsafe { CFDictionaryGetValue(dict.0, key.0) };
    if value.is_null() { None } else { Some(value) }
}

fn string(cf: CFTypeRef) -> Option<String> {
    if !type_is(cf, CFStringGetTypeID) {
        return None;
    }
    let mut buf = [0 as c_char; MAX_STRING];
    let len = CFIndex::try_from(buf.len()).ok()?;
    // SAFETY: `cf` is a live CFString; `buf` is writable for `len` bytes and the call
    // NUL-terminates what it writes, or fails without writing.
    let ok = unsafe { CFStringGetCString(cf, buf.as_mut_ptr(), len, UTF8) };
    if ok == 0 {
        return None;
    }
    // SAFETY: on success the buffer holds a NUL-terminated string within its bounds.
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(s.to_string_lossy().into_owned())
}

fn number(cf: CFTypeRef) -> Option<i64> {
    if !type_is(cf, CFNumberGetTypeID) {
        return None;
    }
    let mut out = 0i64;
    // SAFETY: `cf` is a live CFNumber and `out` is a writable i64, the size of
    // kCFNumberSInt64Type.
    let ok = unsafe { CFNumberGetValue(cf, CF_NUMBER_SINT64, (&raw mut out).cast()) };
    if ok == 0 { None } else { Some(out) }
}

fn boolean(cf: CFTypeRef) -> Option<bool> {
    if !type_is(cf, CFBooleanGetTypeID) {
        return None;
    }
    // SAFETY: `cf` is a live CFBoolean.
    Some(unsafe { CFBooleanGetValue(cf) } != 0)
}

pub fn identify(path: &Path) -> Identity {
    if let Ok(stat) = rustix::fs::stat(path)
        && matches!(
            rustix::fs::FileType::from_raw_mode(stat.st_mode),
            rustix::fs::FileType::BlockDevice | rustix::fs::FileType::CharacterDevice
        )
    {
        return node(path);
    }
    let (file_system, device) = match rustix::fs::statfs(path) {
        Ok(fs) => {
            let name = c_chars(&fs.f_fstypename);
            let block = Some(u64::from(fs.f_bsize));
            let file_system = FileSystem {
                kind: kind_of(&name),
                block_size: block,
                total_bytes: block.and_then(|b| fs.f_blocks.checked_mul(b)),
                available_bytes: block.and_then(|b| fs.f_bavail.checked_mul(b)),
            };
            (file_system, c_chars(&fs.f_mntfromname))
        }
        Err(e) => {
            let mut identity = Identity::unknown(FileSystem {
                kind: FileSystemKind::Unknown,
                block_size: None,
                total_bytes: None,
                available_bytes: None,
            });
            identity.note(format!("statfs {}", path.display()), e.to_string());
            return identity;
        }
    };
    let mut identity = Identity::unknown(file_system);
    if identity.file_system.kind.is_network() {
        identity.interconnect = Interconnect::Network;
        return identity;
    }
    let Some(bsd) = device.strip_prefix("/dev/") else {
        identity.note(
            "statfs f_mntfromname",
            format!("{device:?} is not a device node"),
        );
        return identity;
    };
    describe(bsd, &mut identity);
    identity
}

/// A device node: the disk or partition it names, whose size is what the node addresses.
fn node(path: &Path) -> Identity {
    let mut identity = Identity::unknown(FileSystem {
        kind: FileSystemKind::Device,
        block_size: None,
        total_bytes: None,
        available_bytes: None,
    });
    // The node's own name, links resolved, is its BSD name; a disk's raw (character) node is
    // its block node's name after an "r", `rdisk4` beside `disk4`.
    let name = match std::fs::canonicalize(path) {
        Ok(real) => real
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        Err(e) => {
            identity.note(format!("resolve {}", path.display()), e.to_string());
            return identity;
        }
    };
    let bsd = name
        .strip_prefix('r')
        .filter(|n| n.starts_with("disk"))
        .unwrap_or(&name);
    describe(bsd, &mut identity);
    identity.file_system.total_bytes = identity.size_bytes;
    identity
}

/// Fills `identity` from the I/O Registry entry of BSD device `bsd` and its ancestors.
fn describe(bsd: &str, identity: &mut Identity) {
    identity.device = Some(bsd.to_owned());
    let Some(media) = Service::for_bsd_name(bsd) else {
        identity.note(format!("IOBSDNameMatching {bsd}"), "no registry entry");
        return;
    };
    identity.logical_block = media
        .property("Preferred Block Size")
        .and_then(|v| number(v.0))
        .and_then(|v| u32::try_from(v).ok());
    identity.physical_block = media
        .property("Physical Block Size")
        .and_then(|v| number(v.0))
        .and_then(|v| u32::try_from(v).ok());
    identity.size_bytes = media
        .property("Size")
        .and_then(|v| number(v.0))
        .and_then(|v| u64::try_from(v).ok());
    identity.removable = media.property("Removable").and_then(|v| boolean(v.0));
    // The commands the nearest controller above the media queues: 253 on this machine's NVMe
    // controller (`ioreg`, research/26 §1.1).
    identity.queue_depth = media
        .inherited("IOCommandPoolSize")
        .and_then(|v| number(v.0))
        .and_then(|v| u32::try_from(v).ok());

    match media.inherited("Device Characteristics") {
        Some(chars) => {
            identity.medium = match entry(&chars, "Medium Type").and_then(string).as_deref() {
                Some("Solid State") => Medium::SolidState,
                Some("Rotational") => Medium::Rotational,
                _ => Medium::Unknown,
            };
            identity.model = entry(&chars, "Product Name")
                .and_then(string)
                .map(|m| m.trim().to_owned());
        }
        None => identity.note("Device Characteristics", "not found above the media"),
    }
    match media.inherited("Protocol Characteristics") {
        Some(protocol) => {
            let interconnect = entry(&protocol, "Physical Interconnect").and_then(string);
            let location = entry(&protocol, "Physical Interconnect Location").and_then(string);
            identity.interconnect = interconnect_of(interconnect.as_deref(), location.as_deref());
        }
        None => identity.note("Protocol Characteristics", "not found above the media"),
    }
    // F_FULLFSYNC always asks the drive to empty its cache (fcntl(2)); whether the drive
    // has a volatile one is not in the registry.
    identity.write_cache = WriteCache::Unknown;
}

fn interconnect_of(interconnect: Option<&str>, location: Option<&str>) -> Interconnect {
    if location == Some("File") {
        // A disk image: the "device" is a file on another file system.
        return Interconnect::Virtual;
    }
    match interconnect {
        Some("Apple Fabric") => Interconnect::AppleFabric,
        Some("USB") => Interconnect::Usb,
        Some("SATA" | "SAS" | "SCSI" | "ATA") => Interconnect::Scsi,
        Some("Secure Digital") => Interconnect::Mmc,
        Some("Virtual Interface") => Interconnect::Virtual,
        Some("iSCSI" | "Fibre Channel") => Interconnect::Network,
        Some(other) => Interconnect::Other(other.to_owned()),
        None => Interconnect::Unknown,
    }
}

/// `f_fstypename` values of macOS's file systems.
fn kind_of(name: &str) -> FileSystemKind {
    match name {
        "apfs" => FileSystemKind::Apfs,
        "hfs" => FileSystemKind::Hfs,
        "msdos" => FileSystemKind::Fat,
        "exfat" => FileSystemKind::ExFat,
        "ntfs" => FileSystemKind::Ntfs,
        "nfs" => FileSystemKind::Nfs,
        "smbfs" | "afpfs" | "webdav" => FileSystemKind::Smb,
        name if name.contains("fuse") => FileSystemKind::Fuse,
        "" => FileSystemKind::Unknown,
        other => FileSystemKind::Other(other.to_owned()),
    }
}

/// A NUL-terminated C char array as a string, bounded by the array.
fn c_chars(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c.to_ne_bytes()[0])
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_test_directory() {
        let dir = tempfile::tempdir().unwrap();
        let id = identify(dir.path());
        eprintln!("{id:#?}");
        assert!(id.device.is_some());
        assert!(id.file_system.available_bytes.is_some());
        assert_ne!(id.medium, Medium::Unknown, "{:?}", id.notes);
        assert!(id.logical_block.is_some());
    }

    /// A disk image attached with hdiutil(1): a real block device whose registry entries
    /// lack properties a physical disk has, and whose "device" is a file.
    #[test]
    fn identifies_an_attached_disk_image() {
        use std::process::Command;
        struct Attached(std::path::PathBuf);
        impl Drop for Attached {
            fn drop(&mut self) {
                let _ = Command::new("hdiutil")
                    .arg("detach")
                    .arg(&self.0)
                    .arg("-force")
                    .status();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("probe.dmg");
        let mount = dir.path().join("mnt");
        let created = Command::new("hdiutil")
            .args([
                "create",
                "-quiet",
                "-size",
                "16m",
                "-fs",
                "HFS+",
                "-volname",
                "mantleprobe",
            ])
            .arg(&image)
            .status()
            .unwrap();
        assert!(created.success());
        let attached = Command::new("hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .status()
            .unwrap();
        assert!(attached.success());
        let _guard = Attached(mount.clone());
        let id = identify(&mount);
        assert_eq!(id.file_system.kind, FileSystemKind::Hfs);
        assert_eq!(id.interconnect, Interconnect::Virtual, "{id:#?}");
        assert!(id.device.as_deref().is_some_and(|d| d.starts_with("disk")));
    }

    /// A device node is the device it names, not devfs, which holds it; its raw (character)
    /// node names the same disk.
    #[test]
    fn a_device_node_is_the_device_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let image = hyper_block::image::DiskImage::attach(dir.path(), 16).unwrap();
        let bsd = image
            .node()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let raw = image.node().with_file_name(format!("r{bsd}"));
        for node in [image.node(), raw.as_path()] {
            let id = identify(node);
            assert_eq!(id.file_system.kind, FileSystemKind::Device, "{id:#?}");
            assert_eq!(id.device.as_deref(), Some(bsd.as_str()));
            assert_eq!(id.size_bytes, Some(16 << 20));
            assert_eq!(id.file_system.total_bytes, Some(16 << 20));
            assert_eq!(id.interconnect, Interconnect::Virtual);
        }
    }

    #[test]
    fn missing_paths_are_noted_not_fatal() {
        let id = identify(Path::new("/definitely/not/here"));
        assert_eq!(id.medium, Medium::Unknown);
        assert!(!id.notes.is_empty());
    }
}
