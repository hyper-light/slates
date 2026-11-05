//! Which WinFsp DLL this host may drive (A-198, §4.6 "Windows"). The receiver (`crate::dispatcher`) writes
//! `FSP_FILE_SYSTEM` fields and calls its operation table by offset, transcribed from WinFsp v2.1's `winfsp.h`, and it
//! replaces the DLL's own receive loop. Neither is a promise WinFsp's public interface makes across versions, so the
//! DLL's version is checked before anything touches that layout, and any other version is refused with its number in
//! the refusal: serving through a layout read from another version is memory corruption, not a degraded mount.
//!
//! **What is checked, and why here.** `FspVersion` (`winfsp.h`) answers the DLL's file version as major in the high 16
//! bits and minor in the low 16. The field the guard read before, `FSP_FILE_SYSTEM.Version`, is reserved: v2.1's
//! `src/dll/fs.c` zeroes the object and never sets it, so that guard passed every DLL
//! (`docs/bugs/2026-11-04-the-winfsp-guard-read-a-field-winfsp-always-zeroes.md`). The decision is pure and cfg-free, so
//! it is tested on every host; the mount calls it before `FspFileSystemCreate`, so a refused DLL leaves nothing behind.

/// Format: the version `FspVersion` answers for WinFsp v2.1 (`major << 16 | minor`), whose headers and sources
/// `crate::ffi`, `crate::dispatcher` and `crate::inflight` were written from (WinFsp 2.1.25156 on CI's runners,
/// `docs/wip/research/winfsp-takeover.md`). Raising it is a review of that layout against the new version's sources.
pub const CHECKED_VERSION: u32 = (2 << u16::BITS) | 2;

/// A DLL whose version is not [`CHECKED_VERSION`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsupportedVersion {
  /// What the loaded DLL's `FspVersion` answered.
  pub found: u32,
}

impl std::fmt::Display for UnsupportedVersion {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "the WinFsp DLL's version {}.{} is not {}.{}, the version whose layout this host's receiver was written from",
      self.found >> u16::BITS,
      self.found & u32::from(u16::MAX),
      CHECKED_VERSION >> u16::BITS,
      CHECKED_VERSION & u32::from(u16::MAX),
    )
  }
}

/// Whether this host may drive a DLL answering `found` to `FspVersion`.
pub fn check(found: u32) -> Result<(), UnsupportedVersion> {
  if found == CHECKED_VERSION {
    Ok(())
  } else {
    Err(UnsupportedVersion { found })
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]
  use super::*;

  /// A-198. Do: ask about the checked version. Expect: admitted.
  #[test]
  fn the_version_the_receiver_was_written_from_is_admitted() {
    assert_eq!(check((2 << 16) | 1), Ok(()));
  }

  /// A-198. Do: ask about every neighbour of the checked version, and zero, which is what the reserved field the old
  /// guard read always holds. Expect: each refused, with its number in the refusal's words.
  #[test]
  fn any_other_version_is_refused_with_its_number() {
    for (found, words) in [
      ((2 << 16) | 2, "version 2.2 "),
      ((2 << 16), "version 2.0 "),
      ((1 << 16) | 12, "version 1.12 "),
      ((3 << 16) | 1, "version 3.1 "),
      (0, "version 0.0 "),
    ] {
      let refusal = check(found).unwrap_err();
      assert_eq!(refusal.found, found);
      let text = refusal.to_string();
      assert!(text.contains(words), "{text}");
      assert!(text.contains(" 2.1,"), "{text}");
    }
  }
}
