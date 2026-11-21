//! A data device's profile (docs/design/node.md §1.6): what calibration measured of the device that
//! a volume on it runs with (`crate::calibrate`), kept in its directory's `mantle/profile` beside
//! the identity it was measured on, so a node starts again without measuring the device while it
//! still matches, and measures it again once it does not (§1.7 step 2). It is a hyper-block record:
//! kept whole or not at all, durable once written, its CRC-32C checked on read.
//!
//! Layout, little-endian, before the record's checksum: magic (8) | version (u16) | key length
//! (u16) | key | depth (u64) | read bytes (u64) | read gap (u64) | prewrite (u8). The key is the
//! line of the identity's facts that decide how the device performs ([`key`]). A record of another
//! key or version, or one that does not read, is measured again and replaced.

use std::path::Path;

use hyper_block::DiskError;

use crate::calibrate::{self, Calibration, Plan};
use crate::identity::{FileSystemKind, Identity};

/// The profile's file in a device directory's `mantle/`.
pub const PROFILE: &str = "profile";

const MAGIC: [u8; 8] = *b"MNTLPROF";
/// The layout's version: a record of another is measured again. 2: `prewrite` decided by first
/// writes past never-written space (calibrate.rs `measured`), where 1's sequential first writes
/// missed NTFS's fill of a file up to its valid data length.
const VERSION: u16 = 2;
/// The longest key a record carries, its length a u16: a device whose identity's line is longer is
/// measured at each start, its profile not kept.
const MAX_KEY: usize = 65_535;
/// A record's bytes beside its key.
const FIXED: usize = 8 + 2 + 2 + 8 + 8 + 8 + 1;

/// What a volume runs with of its device's measurement (`mantle_chunk::Reads`, `Config::prewrite`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Reads at the device at once where throughput stops growing
    /// (`Calibration::random_read_saturation`).
    pub depth: usize,
    /// Bytes the reads at the device buffer at once where large reads stop gaining: the depth of
    /// large transfers at which they stop (`Calibration::sequential_read_saturation`).
    pub read_bytes: u64,
    /// Bytes a read goes past rather than ask the device a second time
    /// (`Calibration::read_gap`).
    pub read_gap: u64,
    /// Whether a volume on the device is written once at format ([`prewrite`]).
    pub prewrite: bool,
}

impl Profile {
    /// What `c` measured, as `mantle bench chunk` runs a volume on it: a device whose reads did not
    /// come to a point where they stop growing reads one at a time, buffering none past what it
    /// asked, as a device not measured does (`mantle_chunk::Reads::default`); its volumes written
    /// once at format as [`prewrite`] decides on a file system of `kind`. None where the bytes of
    /// its large reads in flight pass a u64.
    pub fn of(c: &Calibration, kind: &FileSystemKind) -> Option<Self> {
        let prewrite = prewrite(kind, c);
        let (Some(random), Some(sequential)) =
            (c.random_read_saturation(), c.sequential_read_saturation())
        else {
            return Some(Self {
                depth: 1,
                read_bytes: 0,
                read_gap: 0,
                prewrite,
            });
        };
        let read_bytes = u64::try_from(sequential.depth)
            .ok()?
            .checked_mul(u64::try_from(c.large).ok()?)?;
        Some(Self {
            depth: random.depth,
            read_bytes,
            read_gap: c.read_gap().unwrap_or(0),
            prewrite,
        })
    }

    /// The record of this profile for a device of `key`; none where the key is past
    /// [`MAX_KEY`].
    pub fn encode(&self, key: &str) -> Option<Vec<u8>> {
        let len = u16::try_from(key.len()).ok()?;
        let mut out = Vec::with_capacity(FIXED.checked_add(key.len())?);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(&u64::try_from(self.depth).ok()?.to_le_bytes());
        out.extend_from_slice(&self.read_bytes.to_le_bytes());
        out.extend_from_slice(&self.read_gap.to_le_bytes());
        out.push(u8::from(self.prewrite));
        Some(out)
    }

    /// The profile `record` keeps for a device of `key`: none where it is of another key or
    /// version, or malformed.
    pub fn decode(record: &[u8], key: &str) -> Option<Self> {
        let mut at = record;
        let mut take = |n: usize| {
            let (head, rest) = at.split_at_checked(n)?;
            at = rest;
            Some(head)
        };
        if take(8)? != MAGIC {
            return None;
        }
        let u16_of = |b: &[u8]| b.first_chunk::<2>().map(|b| u16::from_le_bytes(*b));
        let u64_of = |b: &[u8]| b.first_chunk::<8>().map(|b| u64::from_le_bytes(*b));
        if u16_of(take(2)?)? != VERSION {
            return None;
        }
        let len = usize::from(u16_of(take(2)?)?);
        if take(len)? != key.as_bytes() {
            return None;
        }
        let depth = usize::try_from(u64_of(take(8)?)?).ok()?;
        let read_bytes = u64_of(take(8)?)?;
        let read_gap = u64_of(take(8)?)?;
        let prewrite = match take(1)? {
            [0] => false,
            [1] => true,
            _ => return None,
        };
        if !at.is_empty() || depth == 0 {
            return None;
        }
        Some(Self {
            depth,
            read_bytes,
            read_gap,
            prewrite,
        })
    }
}

/// The line of `identity`'s facts that decide how its device performs, which a kept profile must
/// match: its model, medium and interconnect, its blocks and transfers, its queue, write cache,
/// FUA, zones, removability and size, its file system's kind, block and size, and each member's
/// place, model, medium, interconnect and size. Not the OS's names for devices, which a restart may
/// change, its free space, or the queries that failed.
pub fn key(identity: &Identity) -> String {
    let fs = &identity.file_system;
    let mut line = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        identity.model,
        identity.medium,
        identity.interconnect,
        identity.logical_block,
        identity.physical_block,
        identity.optimal_io,
        identity.max_transfer,
        identity.queue_depth,
        identity.write_cache,
        identity.fua,
        identity.zoned,
        identity.removable,
        identity.size_bytes,
        fs.kind,
        fs.block_size,
        fs.total_bytes,
    );
    for member in &identity.members {
        line.push_str(&format!(
            "|{:?}:{:?}:{:?}:{:?}:{:?}",
            member.under, member.model, member.medium, member.interconnect, member.size_bytes
        ));
    }
    line
}

/// The profile kept in `mantle`, the device directory `dir`'s `mantle/`, for the device `identity`
/// describes, where its record reads and its key and version match; else the device under `dir`
/// measured now by `plan` (`calibrate::calibrate`; a node's is `Plan::standard`) and its profile
/// kept, unless its identity's line is past what a record carries. Says whether it was measured
/// now.
pub fn load_or_measure(
    dir: &Path,
    mantle: &Path,
    identity: &Identity,
    plan: &Plan,
) -> Result<(Profile, bool), DiskError> {
    let path = mantle.join(PROFILE);
    let key = key(identity);
    // A record that does not read is measured again and replaced, as one of another device is.
    let limit = FIXED.saturating_add(MAX_KEY);
    if let Ok(Some(record)) = hyper_block::record::read(&path, limit)
        && let Some(profile) = Profile::decode(&record, &key)
    {
        return Ok((profile, false));
    }
    let measured = calibrate::calibrate(
        dir,
        identity.transfer_alignment(),
        identity.file_system.available_bytes,
        plan,
    )?;
    let profile =
        Profile::of(&measured, &identity.file_system.kind).ok_or_else(|| DiskError::Io {
            op: "measure",
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the bytes of the device's large reads in flight pass a u64",
            ),
        })?;
    if let Some(record) = profile.encode(&key) {
        hyper_block::record::write(&path, &record)?;
    }
    Ok((profile, true))
}

/// Whether a volume on a device whose file system is `kind` is written once at format: where the
/// file system's documentation says it fills a file with zeros up to a write past its valid data
/// length (`FileSystemKind::fills_past_valid_data`), or where calibration measured a durable write
/// past never-written space slower than one over written space
/// (`Calibration::first_write_penalty`). The identification decides where the documentation
/// does, whatever the measurement's noise: one of 33 paired calibrations under load on
/// windows-x86_64 missed NTFS's fill, which a volume's first data write pays for in seconds
/// (docs/measurements/2026-10-24-first-writes-past-unwritten-space.md).
pub fn prewrite(kind: &FileSystemKind, c: &Calibration) -> bool {
    kind.fills_past_valid_data() || c.first_write_penalty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{FileSystem, FileSystemKind, Medium};

    const KEPT: Profile = Profile {
        depth: 16,
        read_bytes: 4 << 20,
        read_gap: 96 << 10,
        prewrite: true,
    };

    fn identity() -> Identity {
        let mut id = crate::probe::identify(Path::new("."));
        id.model = Some("disk".into());
        id.medium = Medium::SolidState;
        id.file_system = FileSystem {
            kind: FileSystemKind::Apfs,
            block_size: Some(4096),
            total_bytes: Some(1 << 40),
            available_bytes: Some(1 << 30),
        };
        id
    }

    /// NTFS's volumes are written once at format whatever the measurement says; on other file
    /// systems the measurement decides.
    #[test]
    fn ntfs_prewrites_and_others_follow_the_measurement() {
        let point = calibrate::Point {
            depth: 1,
            ops_per_sec: 0.0,
            bytes_per_sec: 0.0,
            spread: 0.0,
            rounds: 0,
            p50_ns: None,
            p99_ns: None,
            achieved: 0.0,
        };
        let measured = |ratios: &[f64]| Calibration {
            caching: hyper_block::file::Caching::Direct,
            small: 4096,
            large: 1 << 20,
            random_read: Vec::new(),
            random_read_capped: false,
            sequential_read: Vec::new(),
            sequential_write: Vec::new(),
            durable_write: point,
            durable_sequential: point,
            durable_large: 1 << 20,
            workers: 1,
            first_write_ratios: ratios.iter().map(|r| r.ln()).collect(),
            elapsed: std::time::Duration::ZERO,
        };
        let (penalty, none) = (measured(&[5.7, 6.1, 5.2, 6.3]), measured(&[1.1, 0.9, 1.05]));
        assert!(prewrite(&FileSystemKind::Ntfs, &none));
        assert!(prewrite(&FileSystemKind::Ntfs, &penalty));
        assert!(!prewrite(&FileSystemKind::Apfs, &none));
        assert!(prewrite(&FileSystemKind::Ext4, &penalty));
        assert!(!prewrite(&FileSystemKind::Unknown, &none));
        assert!(Profile::of(&none, &FileSystemKind::Ntfs).unwrap().prewrite);
    }

    /// A record reads back as the profile it keeps for its device's key, and for no other key,
    /// version, or record cut or grown by a byte.
    #[test]
    fn a_profile_reads_back_for_its_device_and_no_other() {
        let key = key(&identity());
        let record = KEPT.encode(&key).unwrap();
        assert_eq!(Profile::decode(&record, &key), Some(KEPT));
        assert_eq!(Profile::decode(&record, "another device"), None);
        let mut versioned = record.clone();
        versioned[8..10].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(Profile::decode(&versioned, &key), None);
        assert_eq!(Profile::decode(&record[..record.len() - 1], &key), None);
        let mut longer = record.clone();
        longer.push(0);
        assert_eq!(Profile::decode(&longer, &key), None);
        assert_eq!(KEPT.encode(&"k".repeat(MAX_KEY + 1)), None);
    }

    /// The key follows what decides how the device performs, and not its free space or the OS's
    /// name for it.
    #[test]
    fn a_devices_key_follows_its_performance_facts_only() {
        let id = identity();
        let mut freer = id.clone();
        freer.file_system.available_bytes = Some(1 << 35);
        freer.device = Some("renamed".into());
        assert_eq!(key(&freer), key(&id));
        let mut deeper = id.clone();
        deeper.queue_depth = Some(id.queue_depth.unwrap_or(0) + 1);
        assert_ne!(key(&deeper), key(&id));
        let mut other = id.clone();
        other.file_system.kind = FileSystemKind::Ext4;
        assert_ne!(key(&other), key(&id));
    }

    /// A small plan, as the calibration's own test runs one.
    fn small() -> Plan {
        Plan {
            span: 8 << 20,
            rounds: calibrate::Rounds {
                min: 2,
                max: 3,
                precision: 0.05,
            },
            step: std::time::Duration::from_millis(20),
            random_depths: vec![1, 2],
            max_read_depth: 2,
            device_queue: None,
            sequential_depths: vec![1],
            small: 4096,
            large: 1 << 20,
            durable_writes: 4,
            durable_large: 2 << 20,
        }
    }

    /// A kept profile is taken while its device matches it; a device that no longer matches it is
    /// measured again and its profile replaced, and so is a record that does not read.
    #[test]
    fn a_kept_profile_is_taken_while_its_device_matches_it() {
        let dir = tempfile::tempdir().unwrap();
        let mantle = dir.path().join("mantle");
        std::fs::create_dir_all(&mantle).unwrap();
        let id = crate::probe::identify(dir.path());
        let path = mantle.join(PROFILE);
        hyper_block::record::write(&path, &KEPT.encode(&key(&id)).unwrap()).unwrap();
        assert_eq!(
            load_or_measure(dir.path(), &mantle, &id, &small()).unwrap(),
            (KEPT, false)
        );
        let mut moved = id.clone();
        moved.model = Some("another model".into());
        let (measured, now) = load_or_measure(dir.path(), &mantle, &moved, &small()).unwrap();
        assert!(now && measured.depth >= 1);
        // Kept for the device it was measured on.
        assert_eq!(
            load_or_measure(dir.path(), &mantle, &moved, &small()).unwrap(),
            (measured, false)
        );
        // A record that does not read is measured again and replaced.
        let mut damaged = std::fs::read(&path).unwrap();
        damaged[0] ^= 1;
        std::fs::write(&path, &damaged).unwrap();
        let (again, now) = load_or_measure(dir.path(), &mantle, &moved, &small()).unwrap();
        assert!(now && again.depth >= 1);
        assert_eq!(
            load_or_measure(dir.path(), &mantle, &moved, &small()).unwrap(),
            (again, false)
        );
        // The scratch files went with each measurement: the profile alone remains.
        let left: Vec<_> = std::fs::read_dir(&mantle)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from(PROFILE)]);
    }
}
