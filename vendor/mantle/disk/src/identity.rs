//! What the operating system says about the device and file system under a path.
//!
//! Every field is optional or has an `Unknown` case: the OS may not know (a virtual disk, a
//! network file system, a container overlay), may refuse (a query needing privileges), or
//! may be wrong (hypervisors commonly report a rotational medium for flash). An identity is
//! therefore a claim to be checked by measurement (`crate::measure`), and `notes` records
//! every query that failed and why, so an operator can see what mantle could not learn.

use std::fmt;

use hyper_block::buf::Alignment;

/// The storage medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// Flash or other media without seek cost.
    SolidState,
    /// A spinning disk: seeks and rotational latency dominate small random I/O.
    Rotational,
    /// Memory (tmpfs, ramfs, zram, pmem/DAX).
    Memory,
    Unknown,
}

/// How the device attaches to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interconnect {
    Nvme,
    /// ATA/SATA, SCSI or SAS; the kernel presents all three as SCSI disks.
    Scsi,
    Usb,
    /// Paravirtual or emulated disks: virtio-blk, Xen, Hyper-V, file-backed virtual disks.
    Virtual,
    /// SD/MMC cards and eMMC.
    Mmc,
    /// Apple's internal SSD fabric.
    AppleFabric,
    /// Storage reached over the network (NFS, SMB, NBD, RBD, iSCSI).
    Network,
    /// A logical device over others (device-mapper, md RAID, Storage Spaces); the members
    /// are described in [`Identity::members`].
    Composite,
    Memory,
    Other(String),
    Unknown,
}

/// Whether the device holds acknowledged writes in a volatile cache that a flush empties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCache {
    /// Volatile write-back cache: durability requires a flush (or FUA) to reach the medium.
    WriteBack,
    /// Writes complete only when on stable media (no cache, or a power-loss-protected one).
    WriteThrough,
    Unknown,
}

/// Zoned block devices (SMR drives, ZNS SSDs) require sequential writes within zones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoned {
    None,
    /// Accepts random writes but performs best written sequentially per zone.
    HostAware,
    /// Rejects writes that are not sequential within a zone.
    HostManaged,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSystemKind {
    Apfs,
    Hfs,
    Ext4,
    Xfs,
    Btrfs,
    Zfs,
    F2fs,
    Bcachefs,
    Ntfs,
    Refs,
    Fat,
    ExFat,
    Tmpfs,
    Overlay,
    Nfs,
    Smb,
    Fuse,
    Ceph,
    /// zonefs: each file is one zone of a zoned device, written only at its write pointer
    /// (Linux Documentation/filesystems/zonefs.rst).
    Zonefs,
    /// No file system: the path is a device node, and whoever opens it writes the device.
    Device,
    Other(String),
    Unknown,
}

impl FileSystemKind {
    /// File systems whose data lives on another host.
    pub fn is_network(&self) -> bool {
        matches!(self, Self::Nfs | Self::Smb | Self::Ceph)
    }

    /// File systems that keep a valid data length and fill a file with zeros up to a write past
    /// it before the write completes: NTFS, by SetFileValidData's remarks, KB 156932 and
    /// `CcZeroData` (docs/research/02 §4.7).
    pub fn fills_past_valid_data(&self) -> bool {
        matches!(self, Self::Ntfs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSystem {
    pub kind: FileSystemKind,
    /// Fundamental block size (statfs f_bsize / GetDiskFreeSpace bytes per cluster).
    pub block_size: Option<u64>,
    pub total_bytes: Option<u64>,
    /// Bytes available to an unprivileged writer.
    pub available_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The OS's name for the device (`nvme0n1`, `disk0`, `\\.\PhysicalDrive0`), if known.
    pub device: Option<String>,
    pub model: Option<String>,
    pub medium: Medium,
    pub interconnect: Interconnect,
    /// The smallest unit the device addresses; direct I/O offsets and lengths are multiples.
    pub logical_block: Option<u32>,
    /// The unit the device writes internally; smaller writes cost a read-modify-write.
    pub physical_block: Option<u32>,
    /// The device's preferred transfer size for sustained throughput, if it states one.
    pub optimal_io: Option<u32>,
    /// The largest single transfer the OS issues to the device.
    pub max_transfer: Option<u32>,
    /// The OS's queue depth for the device.
    pub queue_depth: Option<u32>,
    pub write_cache: WriteCache,
    /// Forced Unit Access: a write can bypass the volatile cache without a full flush.
    pub fua: Option<bool>,
    pub zoned: Zoned,
    pub removable: Option<bool>,
    pub size_bytes: Option<u64>,
    pub file_system: FileSystem,
    /// For a composite device, the devices it is built from, as one flat list.
    pub members: Vec<Member>,
    /// Queries that failed or were unavailable, with the reason.
    pub notes: Vec<Note>,
}

/// A device a composite device is built from, as its own description gives it, and the member it
/// sits under (`None`: the composite itself). Members are one flat list, each naming its parent
/// by index and listed after it, so a description never holds another (CLAUDE.md §10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub under: Option<usize>,
    pub device: Option<String>,
    pub model: Option<String>,
    pub medium: Medium,
    pub interconnect: Interconnect,
    pub size_bytes: Option<u64>,
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub query: String,
    pub outcome: String,
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.query, self.outcome)
    }
}

impl Identity {
    /// The alignment direct transfers to the device keep: the larger of its logical block, the
    /// unit it addresses, and its physical block, the unit it writes, so no write is split into a
    /// read-modify-write; and 4 KiB at least, for a device that states neither. `mantle disk
    /// probe --measure`, `mantle bench` and a node's volumes take it alike.
    pub fn transfer_alignment(&self) -> Alignment {
        [self.logical_block, self.physical_block]
            .into_iter()
            .flatten()
            .filter_map(|b| usize::try_from(b).ok())
            .filter_map(|b| Alignment::new(b).ok())
            .fold(
                Alignment::new(4096).unwrap_or(Alignment::BYTE),
                Alignment::max,
            )
    }

    pub(crate) fn unknown(file_system: FileSystem) -> Self {
        Self {
            device: None,
            model: None,
            medium: Medium::Unknown,
            interconnect: Interconnect::Unknown,
            logical_block: None,
            physical_block: None,
            optimal_io: None,
            max_transfer: None,
            queue_depth: None,
            write_cache: WriteCache::Unknown,
            fua: None,
            zoned: Zoned::Unknown,
            removable: None,
            size_bytes: None,
            file_system,
            members: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub(crate) fn note(&mut self, query: impl Into<String>, outcome: impl Into<String>) {
        // Bounded: one note per query a probe makes, and probes make a fixed set of queries.
        self.notes.push(Note {
            query: query.into(),
            outcome: outcome.into(),
        });
    }
}
