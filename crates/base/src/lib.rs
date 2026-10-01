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
//! base plane. On Windows every access is relative to a retained directory handle too
//! (`NtCreateFile` with `RootDirectory`, the reparse check made on the opened object; AUD-29-62),
//! and the host reports no watcher yet (fingerprints alone, the failure matrix's Masked cell),
//! recorded in GAPS. On both, a lookup names one entry of its directory, never a path
//! ([`one_entry`]).
//!
//! Evidence for the primitives: `research/disk-source-of-truth.md` §4 (verified per
//! platform), and the timestamp-granularity table there for the racy rule.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never indexes or slices out of bounds, never
// slices a string off a character boundary, and never overflows. Test builds are exempt. Once a crate is
// clean this holds it there.
#![cfg_attr(
  not(test),
  deny(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects
  )
)]

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod windows_records;

use slates_vfs::host::HostError;

/// Format: the characters that separate path components on Unix (`/`) and end a C string (NUL); a name
/// holding one is a path, never one entry.
#[cfg(any(unix, test))]
pub(crate) const UNIX_NAME_BREAKS: &[char] = &['/', '\0'];
/// Format: the characters that separate path components on Windows (`\` and `/`, which the object manager
/// treats alike under Win32), name a stream of an entry (`:`), or end a counted name early (NUL); a name
/// holding one is a path or a stream, never one entry.
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_NAME_BREAKS: &[char] = &['\\', '/', ':', '\0'];

/// One entry of a directory, by the rule both hosts hold (R1, §4.15, AUD-29-62): every lookup names exactly
/// one entry of the directory handle it is relative to, never a path. An empty name, `.`, `..`, or a name
/// holding one of the platform's `breaks` would name something other than one entry — the directory
/// itself, its parent, or a descendant reached through components no single-entry check sees (`O_NOFOLLOW`
/// and `FILE_OPEN_REPARSE_POINT` guard the final component only) — so it is refused as absent: no entry of
/// the directory has that name.
pub(crate) fn one_entry<'name>(name: &'name str, breaks: &[char]) -> Result<&'name str, HostError> {
  if name.is_empty() || name == "." || name == ".." || name.contains(breaks) {
    return Err(HostError::NotFound);
  }
  Ok(name)
}

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

#[cfg(test)]
mod tests {
  use super::*;

  /// AUD-29-62. Do: ask for every shape of name that is not one entry, on each platform's rule. Expect:
  /// each is refused as absent, and a plain name — including one holding the other platform's breaks — is
  /// one entry.
  #[test]
  fn a_name_that_is_not_one_entry_is_refused_as_absent() {
    for breaks in [UNIX_NAME_BREAKS, WINDOWS_NAME_BREAKS] {
      for name in ["", ".", "..", "a/b", "../x", "a\0b"] {
        assert_eq!(
          one_entry(name, breaks),
          Err(HostError::NotFound),
          "{name:?}"
        );
      }
      assert_eq!(one_entry("...", breaks), Ok("..."));
      assert_eq!(one_entry(".hidden", breaks), Ok(".hidden"));
    }
    for name in ["a\\b", "..\\x", "file:stream", "C:"] {
      assert_eq!(
        one_entry(name, WINDOWS_NAME_BREAKS),
        Err(HostError::NotFound),
        "{name:?}"
      );
      assert_eq!(
        one_entry(name, UNIX_NAME_BREAKS),
        Ok(name),
        "{name:?} is one Unix entry"
      );
    }
  }
}
