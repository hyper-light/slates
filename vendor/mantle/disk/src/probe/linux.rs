//! Linux: the block device under a path, from sysfs.
//!
//! A path's file system reports its device number (stat(2) `st_dev`); sysfs exposes that
//! device at `/sys/dev/block/MAJOR:MINOR` (Documentation/ABI/stable/sysfs-dev). A block
//! device node is the device itself: its own number is `st_rdev` (stat(2)), and the file
//! system holding the node, devtmpfs, says nothing about the device. A partition
//! has a `partition` attribute and lives under its disk's directory; a device-mapper or md
//! device lists its members under `slaves/`. Queue attributes are those of
//! Documentation/ABI/stable/sysfs-block. File system types are the statfs(2) `f_type` magic
//! numbers of include/uapi/linux/magic.h.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use rustix::fs::FileType;

use crate::identity::{
    FileSystem, FileSystemKind, Identity, Interconnect, Medium, Member, Note, WriteCache, Zoned,
};

/// Composite devices nest (dm-crypt over LVM over md, three deep); the walk stops past this
/// depth and leaves the composite's medium unknown rather than guess it, so the device is
/// measured and treated as rule 5 of CLAUDE.md treats an undescribed one.
const MAX_DEPTH: usize = 8;

pub fn identify(path: &Path) -> Identity {
    let stat = rustix::fs::stat(path);
    if let Ok(stat) = &stat {
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::BlockDevice => return block_node(stat),
            FileType::CharacterDevice => {
                let mut identity = Identity::unknown(device());
                identity.note(
                    format!("stat {}", path.display()),
                    "a character device, not a block device",
                );
                return identity;
            }
            _ => {}
        }
    }
    let file_system = file_system(path);
    let mut identity = Identity::unknown(file_system.clone());
    match &file_system.kind {
        FileSystemKind::Tmpfs => {
            identity.medium = Medium::Memory;
            identity.interconnect = Interconnect::Memory;
            return identity;
        }
        kind if kind.is_network() => {
            identity.interconnect = Interconnect::Network;
            return identity;
        }
        _ => {}
    }
    let stat = match stat {
        Ok(stat) => stat,
        Err(e) => {
            identity.note(format!("stat {}", path.display()), e.to_string());
            return identity;
        }
    };
    describe_number(stat.st_dev, &mut identity);
    identity
}

/// A block device node: the device it names, and the size of what it addresses, a whole
/// disk or one partition of it.
fn block_node(stat: &rustix::fs::Stat) -> Identity {
    let mut identity = Identity::unknown(device());
    if let Some(partition) = describe_number(stat.st_rdev, &mut identity) {
        identity.size_bytes = read_u64(&partition, "size", &mut identity)
            .and_then(|sectors| sectors.checked_mul(512));
    }
    identity.file_system.total_bytes = identity.size_bytes;
    identity
}

fn device() -> FileSystem {
    FileSystem {
        kind: FileSystemKind::Device,
        block_size: None,
        total_bytes: None,
        available_bytes: None,
    }
}

/// Describes the whole disk of device number `number`, and returns the partition's sysfs
/// directory when the number names a partition.
fn describe_number(number: rustix::fs::Dev, identity: &mut Identity) -> Option<PathBuf> {
    let (major, minor) = (rustix::fs::major(number), rustix::fs::minor(number));
    let node = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
    let dir = match std::fs::canonicalize(&node) {
        Ok(dir) => dir,
        Err(e) => {
            // Major 0 is an anonymous device: overlayfs, FUSE, btrfs subvolumes. No queue.
            identity.note(format!("resolve {}", node.display()), e.to_string());
            return None;
        }
    };
    if !dir.join("partition").exists() {
        describe(&dir, identity);
        return None;
    }
    let disk = dir.parent().map_or_else(|| dir.clone(), Path::to_path_buf);
    describe(&disk, identity);
    Some(dir)
}

/// Fills `identity` from the sysfs directory of a whole block device, and a composite's members
/// as one flat list ([`stack`]).
fn describe(disk: &Path, identity: &mut Identity) {
    let mut seen = HashSet::from([disk.to_path_buf()]);
    describe_one(disk, identity);
    if identity.interconnect == Interconnect::Composite {
        stack(disk, identity, &mut seen);
    }
}

/// Fills `identity` from the sysfs directory of a whole block device, its members aside.
fn describe_one(disk: &Path, identity: &mut Identity) {
    let name = disk
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    identity.device = Some(name.clone());
    identity.size_bytes = read_u64(disk, "size", identity).and_then(|sectors| {
        // sysfs `size` counts 512-byte sectors whatever the logical block size.
        sectors.checked_mul(512)
    });
    identity.removable = read_u64(disk, "removable", identity).map(|v| v == 1);

    let queue = disk.join("queue");
    identity.logical_block = read_u32(&queue, "logical_block_size", identity);
    identity.physical_block = read_u32(&queue, "physical_block_size", identity);
    identity.optimal_io = read_u32(&queue, "optimal_io_size", identity).filter(|&v| v > 0);
    identity.max_transfer =
        read_u32(&queue, "max_sectors_kb", identity).and_then(|kib| kib.checked_mul(1024));
    identity.queue_depth = read_u32(&queue, "nr_requests", identity);
    identity.write_cache = match read_str(&queue, "write_cache", identity).as_deref() {
        Some("write back") => WriteCache::WriteBack,
        Some("write through") => WriteCache::WriteThrough,
        _ => WriteCache::Unknown,
    };
    identity.fua = read_u64(&queue, "fua", identity).map(|v| v == 1);
    identity.zoned = match read_str(&queue, "zoned", identity).as_deref() {
        Some("none") => Zoned::None,
        Some("host-aware") => Zoned::HostAware,
        Some("host-managed") => Zoned::HostManaged,
        _ => Zoned::Unknown,
    };
    let rotational = read_u64(&queue, "rotational", identity);
    let canonical = disk.to_string_lossy().into_owned();
    identity.interconnect = interconnect(&name, &canonical, disk, identity);
    identity.medium = match (&identity.interconnect, rotational) {
        (Interconnect::Memory, _) => Medium::Memory,
        (_, Some(1)) => Medium::Rotational,
        (_, Some(0)) => Medium::SolidState,
        _ => Medium::Unknown,
    };
    identity.model = read_str(&disk.join("device"), "model", identity);
}

fn interconnect(name: &str, canonical: &str, disk: &Path, identity: &mut Identity) -> Interconnect {
    let prefixed = |p: &str| name.starts_with(p);
    if prefixed("dm-") || prefixed("md") {
        return Interconnect::Composite;
    }
    if prefixed("zram") || prefixed("ram") || prefixed("pmem") {
        return Interconnect::Memory;
    }
    if prefixed("nbd") || prefixed("rbd") {
        return Interconnect::Network;
    }
    if canonical.contains("/usb") {
        return Interconnect::Usb;
    }
    if prefixed("nvme") {
        // NVMe over fabrics reports its transport on the controller: pcie, tcp, rdma, fc.
        return match read_str(&disk.join("device"), "transport", identity).as_deref() {
            Some("tcp" | "rdma" | "fc") => Interconnect::Network,
            _ => Interconnect::Nvme,
        };
    }
    if prefixed("vd") || prefixed("xvd") || canonical.contains("/virtio") {
        return Interconnect::Virtual;
    }
    if prefixed("mmcblk") {
        return Interconnect::Mmc;
    }
    if prefixed("sd") {
        return Interconnect::Scsi;
    }
    if prefixed("loop") {
        return Interconnect::Other("loop".to_owned());
    }
    Interconnect::Unknown
}

/// A composite's members, from each composite's `slaves/`: one walk over a queue of devices still
/// to describe, each with the member it sits under and its depth. A device is marked seen when
/// queued, so each is described once, however the stack shares it, and the queue holds at most
/// the system's block devices; a composite deeper than `MAX_DEPTH` is noted and not followed.
/// Then each composite is as slow as its slowest member: a member is listed after the member it
/// sits under, so a pass from the last member to the first meets children before parents.
fn stack(disk: &Path, identity: &mut Identity, seen: &mut HashSet<PathBuf>) {
    // Per composite (the top one first, then members by index): it shares a member described
    // under another, or nests too deep to follow.
    let mut queue = VecDeque::new();
    let found = slaves(disk, 0, seen, &mut queue, None);
    identity.notes.extend(found.notes);
    let (mut shared, mut deep) = (vec![found.shared], vec![found.deep]);
    while let Some((whole, under, depth)) = queue.pop_front() {
        let mut described = Identity::unknown(identity.file_system.clone());
        describe_one(&whole, &mut described);
        let index = identity.members.len();
        let mut notes = described.notes;
        let (mut is_shared, mut is_deep) = (false, false);
        if described.interconnect == Interconnect::Composite {
            let found = slaves(&whole, depth, seen, &mut queue, Some(index));
            notes.extend(found.notes);
            (is_shared, is_deep) = (found.shared, found.deep);
        }
        identity.members.push(Member {
            under,
            device: described.device,
            model: described.model,
            medium: described.medium,
            interconnect: described.interconnect,
            size_bytes: described.size_bytes,
            notes,
        });
        shared.push(is_shared);
        deep.push(is_deep);
    }
    // Children before parents: each member settles its own medium, then a rotational one makes
    // its composite rotational.
    for at in (0..identity.members.len()).rev() {
        let flag = at.saturating_add(1);
        let Some(member) = identity.members.get_mut(at) else {
            continue;
        };
        settle(
            &mut member.medium,
            shared.get(flag).copied().unwrap_or(false),
            deep.get(flag).copied().unwrap_or(false),
        );
        if member.medium != Medium::Rotational {
            continue;
        }
        match member.under {
            Some(parent) => {
                if let Some(p) = identity.members.get_mut(parent) {
                    p.medium = Medium::Rotational;
                }
            }
            None => identity.medium = Medium::Rotational,
        }
    }
    settle(
        &mut identity.medium,
        shared.first().copied().unwrap_or(false),
        deep.first().copied().unwrap_or(false),
    );
}

/// A composite's medium once its members' are in: rotational stays (as slow as its slowest
/// member); otherwise a member described under another, or a stack too deep to follow, leaves it
/// unknown rather than guessed.
fn settle(medium: &mut Medium, shared: bool, deep: bool) {
    if *medium != Medium::Rotational && (shared || deep) {
        *medium = Medium::Unknown;
    }
}

/// What a composite's `slaves/` gave: its notes, and whether a member was described under
/// another or the composite nests past `MAX_DEPTH`.
struct Found {
    notes: Vec<Note>,
    shared: bool,
    deep: bool,
}

/// Queues the whole devices under composite `disk`, at `depth`, under member `under`, each not
/// yet seen; marks them seen.
fn slaves(
    disk: &Path,
    depth: usize,
    seen: &mut HashSet<PathBuf>,
    queue: &mut VecDeque<(PathBuf, Option<usize>, usize)>,
    under: Option<usize>,
) -> Found {
    let mut found = Found {
        notes: Vec::new(),
        shared: false,
        deep: false,
    };
    let note = |found: &mut Found, query: String, outcome: String| {
        found.notes.push(Note { query, outcome });
    };
    if depth >= MAX_DEPTH {
        note(
            &mut found,
            "slaves".to_owned(),
            format!("nesting deeper than {MAX_DEPTH}; not followed"),
        );
        found.deep = true;
        return found;
    }
    let slaves = disk.join("slaves");
    let entries = match std::fs::read_dir(&slaves) {
        Ok(entries) => entries,
        Err(e) => {
            note(
                &mut found,
                format!("read_dir {}", slaves.display()),
                e.to_string(),
            );
            return found;
        }
    };
    for entry in entries.flatten() {
        let Ok(dir) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        let whole = if dir.join("partition").exists() {
            dir.parent().map(Path::to_path_buf).unwrap_or(dir)
        } else {
            dir
        };
        // A device under two members of the stack is described once; the composite's medium
        // then rests on a member not described here, and is left unknown.
        if !seen.insert(whole.clone()) {
            note(
                &mut found,
                format!("slave {}", whole.display()),
                "described under another member".to_owned(),
            );
            found.shared = true;
            continue;
        }
        queue.push_back((whole, under, depth.saturating_add(1)));
    }
    found
}

fn file_system(path: &Path) -> FileSystem {
    let kind = match rustix::fs::statfs(path) {
        Ok(fs) => kind_of(i128::from(fs.f_type)),
        Err(_) => FileSystemKind::Unknown,
    };
    let (block_size, total_bytes, available_bytes) = match rustix::fs::statvfs(path) {
        Ok(vfs) => (
            Some(vfs.f_frsize),
            vfs.f_blocks.checked_mul(vfs.f_frsize),
            vfs.f_bavail.checked_mul(vfs.f_frsize),
        ),
        Err(_) => (None, None, None),
    };
    FileSystem {
        kind,
        block_size,
        total_bytes,
        available_bytes,
    }
}

/// statfs(2) `f_type` values, from include/uapi/linux/magic.h (ZFS's from the OpenZFS source,
/// ZFS_SUPER_MAGIC). `f_type` is signed on some architectures, so compare as i128.
fn kind_of(f_type: i128) -> FileSystemKind {
    match f_type {
        0xEF53 => FileSystemKind::Ext4,
        0x5846_5342 => FileSystemKind::Xfs,
        0x9123_683E => FileSystemKind::Btrfs,
        0x2FC1_2FC1 => FileSystemKind::Zfs,
        0xF2F5_2010 => FileSystemKind::F2fs,
        0xCA45_1A4E => FileSystemKind::Bcachefs,
        0x0102_1994 | 0x8584_58F6 => FileSystemKind::Tmpfs,
        0x794C_7630 => FileSystemKind::Overlay,
        0x6969 => FileSystemKind::Nfs,
        0xFF53_4D42 | 0xFE53_4D42 | 0x517B => FileSystemKind::Smb,
        0x6573_5546 => FileSystemKind::Fuse,
        0x00C3_6400 => FileSystemKind::Ceph,
        0x5A4F_4653 => FileSystemKind::Zonefs,
        0x4D44 => FileSystemKind::Fat,
        0x2011_BAB0 => FileSystemKind::ExFat,
        0x7366_746E | 0x5346_544E => FileSystemKind::Ntfs,
        0x482B => FileSystemKind::Hfs,
        other => FileSystemKind::Other(format!("statfs f_type {other:#x}")),
    }
}

fn read_str(dir: &Path, attr: &str, identity: &mut Identity) -> Option<String> {
    let path = dir.join(attr);
    match std::fs::read_to_string(&path) {
        Ok(s) => Some(s.trim().to_owned()),
        Err(e) => {
            identity.note(format!("read {}", path.display()), e.to_string());
            None
        }
    }
}

fn read_u64(dir: &Path, attr: &str, identity: &mut Identity) -> Option<u64> {
    let text = read_str(dir, attr, identity)?;
    match text.parse() {
        Ok(v) => Some(v),
        Err(e) => {
            identity.note(
                format!("parse {}/{attr} {text:?}", dir.display()),
                e.to_string(),
            );
            None
        }
    }
}

fn read_u32(dir: &Path, attr: &str, identity: &mut Identity) -> Option<u32> {
    read_u64(dir, attr, identity).and_then(|v| u32::try_from(v).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_magic_numbers() {
        assert_eq!(kind_of(0xEF53), FileSystemKind::Ext4);
        assert_eq!(kind_of(0x0102_1994), FileSystemKind::Tmpfs);
        assert_eq!(kind_of(0x794C_7630), FileSystemKind::Overlay);
        assert_eq!(kind_of(0x5A4F_4653), FileSystemKind::Zonefs);
        assert!(matches!(kind_of(0x1234), FileSystemKind::Other(_)));
    }

    /// A block device node is the device it names, not devtmpfs, which holds it (run with
    /// `MANTLE_TEST_BLOCK_DEVICE` naming one, such as a loop device).
    #[test]
    #[ignore = "needs MANTLE_TEST_BLOCK_DEVICE, a block device"]
    fn a_block_device_node_is_the_device_it_names() {
        let device = std::env::var_os("MANTLE_TEST_BLOCK_DEVICE").unwrap();
        let path = Path::new(&device);
        let id = identify(path);
        let name = std::fs::canonicalize(path).unwrap();
        let name = name.file_name().unwrap().to_str().unwrap();
        assert_eq!(id.file_system.kind, FileSystemKind::Device, "{id:#?}");
        assert_eq!(id.device.as_deref(), Some(name), "{id:#?}");
        let file = hyper_block::file::DeviceFile::open(
            path,
            false,
            hyper_block::file::CachingRequest::Buffered,
            hyper_block::buf::Alignment::BYTE,
        )
        .unwrap();
        let size = file.len().unwrap();
        assert!(size > 0);
        assert_eq!(id.size_bytes, Some(size), "{id:#?}");
        assert_ne!(id.medium, Medium::Memory, "{id:#?}");
    }

    /// A sysfs-shaped block device under `root`: its queue's rotational flag, and its members as
    /// `slaves/` links to their directories.
    fn device(root: &Path, name: &str, rotational: u8, slaves: &[&str]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("queue")).unwrap();
        std::fs::write(dir.join("queue/rotational"), format!("{rotational}\n")).unwrap();
        std::fs::create_dir_all(dir.join("slaves")).unwrap();
        for slave in slaves {
            let target = root.join(slave);
            let link = dir.join("slaves").join(target.file_name().unwrap());
            std::os::unix::fs::symlink(&target, link).unwrap();
        }
        dir
    }

    fn member<'a>(id: &'a Identity, name: &str) -> (usize, &'a Member) {
        let found: Vec<_> = id
            .members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.device.as_deref() == Some(name))
            .collect();
        assert_eq!(found.len(), 1, "{name} once among {:#?}", id.members);
        found[0]
    }

    /// Do: describe dm-0 over md0 (over a rotational sda and a solid-state sdb) and a partition
    /// of sdc.
    /// Expect: four members in one flat list, md0 and sdc under the composite itself, sda and sdb
    /// under md0, sdc found through its partition; md0 and dm-0 as slow as sda.
    #[test]
    fn a_stack_of_composites_is_one_flat_list_as_slow_as_its_slowest_member() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        device(&root, "sda", 1, &[]);
        device(&root, "sdb", 0, &[]);
        let sdc = device(&root, "sdc", 0, &[]);
        std::fs::create_dir_all(sdc.join("sdc1")).unwrap();
        std::fs::write(sdc.join("sdc1/partition"), "1\n").unwrap();
        device(&root, "md0", 0, &["sda", "sdb"]);
        let dm = device(&root, "dm-0", 0, &["md0", "sdc/sdc1"]);
        let mut id = Identity::unknown(device_fs());
        describe(&dm, &mut id);
        assert_eq!(id.members.len(), 4, "{:#?}", id.members);
        let (md, md0) = member(&id, "md0");
        assert_eq!(md0.under, None);
        assert_eq!(member(&id, "sdc").1.under, None);
        assert_eq!(member(&id, "sda").1.under, Some(md));
        assert_eq!(member(&id, "sdb").1.under, Some(md));
        assert_eq!(member(&id, "sda").1.medium, Medium::Rotational);
        assert_eq!(member(&id, "sdc").1.medium, Medium::SolidState);
        assert_eq!(md0.medium, Medium::Rotational);
        assert_eq!(id.medium, Medium::Rotational);
        for (at, m) in id.members.iter().enumerate() {
            assert!(
                m.under.is_none_or(|p| p < at),
                "a member follows its parent"
            );
        }
    }

    /// Do: describe dm-1 over sdb and md1, md1 over the same sdb; then a chain of composites
    /// deeper than `MAX_DEPTH`.
    /// Expect: sdb described once, md1 noting it and its medium unknown rather than guessed; the
    /// chain followed to `MAX_DEPTH` members, the deepest noted and unknown.
    #[test]
    fn a_shared_member_and_a_stack_too_deep_leave_their_composite_unknown() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        device(&root, "sdb", 0, &[]);
        device(&root, "md1", 0, &["sdb"]);
        let dm = device(&root, "dm-1", 0, &["sdb", "md1"]);
        let mut id = Identity::unknown(device_fs());
        describe(&dm, &mut id);
        assert_eq!(id.members.len(), 2, "{:#?}", id.members);
        let md1 = member(&id, "md1").1;
        assert_eq!(md1.medium, Medium::Unknown);
        assert!(
            md1.notes
                .iter()
                .any(|n| n.outcome == "described under another member"),
            "{md1:#?}"
        );
        assert_eq!(id.medium, Medium::SolidState);

        let deep = tempfile::tempdir().unwrap();
        let deep = std::fs::canonicalize(deep.path()).unwrap();
        let names: Vec<String> = (0..=MAX_DEPTH.saturating_add(1))
            .map(|d| format!("dm-{d}"))
            .collect();
        for (d, name) in names.iter().enumerate().rev() {
            let below: Vec<&str> = names.get(d + 1).map(String::as_str).into_iter().collect();
            device(&deep, name, 0, &below);
        }
        let mut id = Identity::unknown(device_fs());
        describe(&deep.join("dm-0"), &mut id);
        assert_eq!(id.members.len(), MAX_DEPTH, "{:#?}", id.members);
        let last = member(&id, &format!("dm-{MAX_DEPTH}")).1;
        assert_eq!(last.medium, Medium::Unknown);
        assert!(
            last.notes
                .iter()
                .any(|n| n.outcome.contains("not followed")),
            "{last:#?}"
        );
    }

    fn device_fs() -> FileSystem {
        super::device()
    }

    #[test]
    fn identifies_the_test_directory_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let id = identify(dir.path());
        eprintln!("{id:#?}");
        assert!(id.file_system.available_bytes.is_some());
    }
}
