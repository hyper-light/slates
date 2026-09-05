//! `slates-base` — the host side of the base plane (design §4.5, §4.15, D-25; Phase 1 task
//! 10): the read-only [`HostFs`] implementation over the operating system. It opens the base
//! directory once and every later access is relative to a descriptor (`openat` with
//! `O_NOFOLLOW`, `fstat`, `getdents` with a `statat` per entry, `pread`, `readlinkat`), so a
//! rename of the base directory by the user changes nothing and no path string is ever
//! resolved on a hot path. Watcher hints come from inotify on Linux and `EVFILT_VNODE` on
//! macOS, behind the same seam; they are never the truth (fingerprints are).
//!
//! This crate links no write-capable syscall: the workspace's structural test refuses every
//! write call and open flag in it (`cargo xtask structural`), which is R1's lint wall for the
//! base plane. On Windows the host is path-relative through the standard library (a directory
//! handle form arrives with the Windows bridge, Phase 4) and reports no watcher (fingerprints
//! alone, the failure matrix's Masked cell); both are recorded in GAPS.
//!
//! Evidence for the primitives: `research/disk-source-of-truth.md` §4 (verified per
//! platform), and the timestamp-granularity table there for the racy rule.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::OsHost;
#[cfg(windows)]
pub use windows::OsHost;

/// Derived: the timestamp granularity in nanoseconds by filesystem type, from the cited table
/// (`research/disk-source-of-truth.md` §4): ext4, XFS, Btrfs, tmpfs and APFS carry nanoseconds,
/// NTFS 100 ns, HFS+ one second, FAT and exFAT two seconds. A type the table does not name
/// takes the coarsest value, so the racy rule re-hashes more often rather than less.
pub fn granularity_for(kind: FsKind) -> u64 {
  /// Format: one nanosecond.
  const NANOSECOND: u64 = 1;
  /// Format: one hundred nanoseconds, NTFS's file-time unit.
  const HUNDRED_NS: u64 = 100;
  /// Format: one second in nanoseconds.
  const SECOND: u64 = 1_000_000_000;
  /// Format: two seconds in nanoseconds, FAT's modification-time unit.
  const TWO_SECONDS: u64 = 2 * SECOND;
  match kind {
    FsKind::Nanosecond => NANOSECOND,
    FsKind::HundredNanoseconds => HUNDRED_NS,
    FsKind::Second => SECOND,
    FsKind::TwoSeconds | FsKind::Unknown => TWO_SECONDS,
  }
}

/// A filesystem's timestamp class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
  /// ext4, XFS, Btrfs, tmpfs, APFS.
  Nanosecond,
  /// NTFS.
  HundredNanoseconds,
  /// HFS+.
  Second,
  /// FAT, exFAT.
  TwoSeconds,
  /// Not in the table.
  Unknown,
}
