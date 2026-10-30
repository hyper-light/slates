//! Purging a mapped file's pages after another path changes it (§4.6 "Windows"; A-190, an experiment measured on CI's
//! Windows runner).
//!
//! WinFsp hears of a change by `FspFileSystemNotify` and purges what it caches of the file with `CcPurgeCacheSection`
//! (`src/sys/file.c`, `FspFileNodeInvalidateCachesAndNotifyChangeByName`), which "will not purge mapped files"
//! (Microsoft Learn), so a process's mapped view kept its pages after another path's change (A-177, measured on CI's
//! Windows runners). WinFsp's non-cached write purges first with `CcCoherencyFlushAndPurgeCache` (`src/sys/write.c`,
//! `FspFsvolWriteNonCached` calling `FspFileNodeFlushAndPurgeCache`), which Microsoft documents as invalidating user
//! mapped views. So after each notification round the mount's thread opens each changed file through the drive,
//! unbuffered, and writes the changed range: WinFsp flushes the file's dirty pages, purges the range and invalidates
//! every view of it, and a view's next touch reads the volume.
//!
//! The write itself is never applied. The host takes an unbuffered open made by this process as its own purge and
//! answers every write through it without passing it on (`crate::host`), and only that open can carry a purge's bytes,
//! so a purge cannot change a file whatever it carries. A process serving a WinFsp mount must therefore not write its
//! own mount unbuffered: such writes are taken as purges and dropped.
//!
//! This module plans what a purge covers: the changed bytes widened to whole sectors (an unbuffered transfer of anything
//! else is refused) and clipped to the file's last sector, and that range cut into the writes the mount's thread makes.
//! Pure and cfg-free, so it is tested on every host.

use slates_bridge_core::CachedBytes;

/// One purge the mount's thread carries out: the file's path from the volume's root as WinFsp names it (`\`-separated,
/// beginning with `\`, as [`crate::notify::OpenNames`] holds it), and the byte range it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Purge {
  /// The path.
  pub path: String,
  /// The first byte, a whole number of sectors.
  pub offset: u64,
  /// How many bytes, a whole number of sectors.
  pub len: u64,
}

/// What a purge of `changed` covers in a file of `size` bytes with sectors of `sector` bytes: the changed bytes widened
/// to whole sectors and clipped to the file's last sector, as `(offset, len)`. `None` when the change names no bytes,
/// when none of the file is left to cover (the bytes lie past its end), or when the arithmetic would overflow.
pub fn purge_range(changed: &CachedBytes, size: u64, sector: u64) -> Option<(u64, u64)> {
  if sector == 0 {
    return None;
  }
  let (at, end) = match *changed {
    CachedBytes::Kept => return None,
    CachedBytes::Range { at, len } => (at, at.saturating_add(len)),
    CachedBytes::From { at } => (at, u64::MAX),
  };
  let last = round_up(size, sector)?;
  let start = at.checked_sub(at.checked_rem(sector)?)?;
  let end = round_up(end.min(size), sector)?.min(last);
  let len = end.checked_sub(start)?;
  (len > 0).then_some((start, len))
}

/// `value` rounded up to a whole number of `unit`s; `None` past `u64::MAX`.
fn round_up(value: u64, unit: u64) -> Option<u64> {
  value.div_ceil(unit).checked_mul(unit)
}

/// The writes that cover `[offset, offset + len)`, each at most `piece` bytes: `(offset, len)` in order. `piece` is a
/// whole number of sectors when `offset` and `len` are, so every write is too. Nothing when `piece` is zero.
pub fn pieces(offset: u64, len: u64, piece: u32) -> impl Iterator<Item = (u64, u32)> {
  let piece = u64::from(piece);
  let end = offset.saturating_add(len);
  let mut at = offset;
  std::iter::from_fn(move || {
    if piece == 0 || at >= end {
      return None;
    }
    let this = end.saturating_sub(at).min(piece);
    let write = (at, u32::try_from(this).ok()?);
    at = at.saturating_add(this);
    Some(write)
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: the sector the tests plan in, the mount's (`crate::host`'s `SECTOR_SIZE`).
  const SECTOR: u64 = 4096;

  /// A-190. Do: plan a purge of a change inside one page, and of one that spans pages, of a file of three and a half
  /// pages. Expect each widened outward to whole sectors: the one page, and the pages it touches.
  #[test]
  fn a_change_is_widened_to_the_whole_sectors_it_touches() {
    let size = 3 * SECTOR + SECTOR / 2;
    assert_eq!(
      purge_range(&CachedBytes::Range { at: 10, len: 100 }, size, SECTOR),
      Some((0, SECTOR))
    );
    assert_eq!(
      purge_range(
        &CachedBytes::Range {
          at: SECTOR - 1,
          len: 2
        },
        size,
        SECTOR
      ),
      Some((0, 2 * SECTOR))
    );
  }

  /// A-190. Do: plan purges of a change running past the end of a file of three and a half pages, of one wholly past it
  /// (the file shrank since), of everything from an offset on, and of a change that names no bytes. Expect the first
  /// clipped to the file's last sector, the second nothing, the third from its sector to the last, the fourth nothing.
  #[test]
  fn a_purge_never_reaches_past_the_files_last_sector() {
    let size = 3 * SECTOR + SECTOR / 2;
    assert_eq!(
      purge_range(
        &CachedBytes::Range {
          at: 3 * SECTOR,
          len: 10 * SECTOR
        },
        size,
        SECTOR
      ),
      Some((3 * SECTOR, SECTOR))
    );
    assert_eq!(
      purge_range(
        &CachedBytes::Range {
          at: 5 * SECTOR,
          len: SECTOR
        },
        size,
        SECTOR
      ),
      None
    );
    assert_eq!(
      purge_range(&CachedBytes::From { at: SECTOR + 7 }, size, SECTOR),
      Some((SECTOR, 3 * SECTOR))
    );
    assert_eq!(purge_range(&CachedBytes::Kept, size, SECTOR), None);
    assert_eq!(purge_range(&CachedBytes::ALL, 0, SECTOR), None);
  }

  /// A-190, hostile inputs. Do: plan purges at offsets and sizes near `u64::MAX`, and with a sector of zero. Expect a
  /// plan or nothing, never a panic, and nothing from a zero sector.
  #[test]
  fn extreme_offsets_and_sizes_plan_without_overflow() {
    for (changed, size) in [
      (
        CachedBytes::Range {
          at: u64::MAX - 1,
          len: u64::MAX,
        },
        u64::MAX,
      ),
      (CachedBytes::From { at: u64::MAX }, u64::MAX),
      (
        CachedBytes::Range {
          at: 0,
          len: u64::MAX,
        },
        u64::MAX - 1,
      ),
    ] {
      let _ = purge_range(&changed, size, SECTOR);
    }
    assert_eq!(
      purge_range(&CachedBytes::Range { at: 0, len: 1 }, SECTOR, 0),
      None
    );
  }

  /// A-190. Do: cut a range of five sectors into writes of two sectors, a range smaller than one write, and a range with
  /// a zero piece. Expect two, two and one sectors in order covering the range exactly; the one small write; nothing.
  #[test]
  fn a_range_is_cut_into_writes_that_cover_it_exactly() {
    let piece = u32::try_from(2 * SECTOR).unwrap();
    let writes: Vec<(u64, u32)> = pieces(SECTOR, 5 * SECTOR, piece).collect();
    assert_eq!(
      writes,
      vec![
        (SECTOR, piece),
        (3 * SECTOR, piece),
        (5 * SECTOR, u32::try_from(SECTOR).unwrap())
      ]
    );
    assert_eq!(
      pieces(0, SECTOR, piece).collect::<Vec<_>>(),
      vec![(0, u32::try_from(SECTOR).unwrap())]
    );
    assert_eq!(pieces(0, SECTOR, 0).count(), 0);
  }
}
