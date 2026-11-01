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
//! **A purge Windows does not take** (Step 8 of the VFS and DAX plan;
//! `docs/bugs/2026-11-01-a-winfsp-purge-windows-did-not-take-left-a-mapped-view-stale.md`). `CcCoherencyFlushAndPurgeCache`
//! reports a page it could not invalidate as `STATUS_CACHE_PAGE_LOCKED`, a success status, which WinFsp passes on as
//! success, so the write succeeds while a view keeps the page: 9 of 200 loaded iterations on CI's x86_64 runner. So each
//! piece is read back after its write, through Windows' cache and around it ([`purge_through`]): a dropped page is read
//! from the host again and the two agree; a kept page differs, the purge is untaken, and the server makes the round
//! again a tick on.
//!
//! This module plans what a purge covers: the changed bytes widened to whole sectors (an unbuffered transfer of anything
//! else is refused) and clipped to the file's last sector, and that range cut into the writes the mount's thread makes;
//! and it decides, over [`PurgeTarget`], whether a purge took. Pure and cfg-free, so it is tested on every host.

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

/// What one file's purge came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurgeOutcome {
  /// Every piece was written, and read back through Windows' cache as the host holds it: Windows dropped the range's
  /// pages, and every view of it reads the volume at its next touch.
  Taken,
  /// Every piece was written, but a read through Windows' cache still served bytes the host no longer holds: Windows did
  /// not drop a page. `CcCoherencyFlushAndPurgeCache` reports a page it could not invalidate as `STATUS_CACHE_PAGE_LOCKED`,
  /// which Microsoft documents as a success status and WinFsp's `FspFileNodeFlushAndPurgeCache` turns into
  /// `STATUS_SUCCESS`, so nothing but the read tells. The purge is owed again.
  Untaken,
  /// A piece's write, or a read of it, was refused (the file renamed or removed since the round, say).
  Refused,
}

/// The file a purge works on, through the drive (`crate::host` opens it): its range written unbuffered, which WinFsp
/// purges before the host drops the write.
pub trait PurgeTarget {
  /// Writes `len` zero bytes at `offset`, unbuffered: whether the whole length was written.
  fn write_zeros(&mut self, offset: u64, len: u32) -> bool;
  /// Reads `len` bytes at `offset` through Windows' cache into `out`, which it clears first: whether the read succeeded.
  fn read_cached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool;
  /// Reads `len` bytes at `offset` around Windows' cache, as the host holds them, into `out`, which it clears first:
  /// whether the read succeeded.
  fn read_uncached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool;
}

/// Purges `purge`'s range through `target`, a piece of `piece` bytes at a time, and checks each piece took: after its
/// write, a read through Windows' cache must serve what a read around it does. A page Windows dropped is read from the
/// host again, so the two agree; a page it kept still holds the bytes it had, so they differ. Every piece is written
/// whatever an earlier one came to, so the pages Windows can drop are dropped now.
pub fn purge_through(target: &mut impl PurgeTarget, purge: &Purge, piece: u32) -> PurgeOutcome {
  let mut cached = Vec::new();
  let mut uncached = Vec::new();
  let mut outcome = PurgeOutcome::Taken;
  for (offset, len) in pieces(purge.offset, purge.len, piece) {
    if !target.write_zeros(offset, len)
      || !target.read_cached(offset, len, &mut cached)
      || !target.read_uncached(offset, len, &mut uncached)
    {
      return PurgeOutcome::Refused;
    }
    if cached != uncached {
      outcome = PurgeOutcome::Untaken;
    }
  }
  outcome
}

#[cfg(test)]
mod tests {
  use std::collections::{BTreeMap, BTreeSet};

  use super::*;

  /// Format: the sector the tests plan in, the mount's (`crate::host`'s `SECTOR_SIZE`).
  const SECTOR: u64 = 4096;

  /// A file as Windows and the host hold it, for a purge to work on: the bytes the host holds now, the pages Windows'
  /// cache holds (each with the bytes it read), and the pages a purge cannot drop, as `CcCoherencyFlushAndPurgeCache`
  /// fails to invalidate a page it finds locked and reports `STATUS_CACHE_PAGE_LOCKED`, which WinFsp passes on as success
  /// (its `FspFileNodeFlushAndPurgeCache`). A purge's write is dropped by the host, as `crate::host` drops it.
  struct Modelled {
    host: Vec<u8>,
    cached: BTreeMap<u64, Vec<u8>>,
    locked: BTreeSet<u64>,
    writes_refused: bool,
    writes: usize,
  }

  impl Modelled {
    /// A file of `pages` pages that held `old` when Windows cached every page of it and holds `new` now.
    fn changed(pages: u64, old: u8, new: u8) -> Modelled {
      let page = usize::try_from(SECTOR).unwrap();
      Modelled {
        host: vec![new; usize::try_from(pages).unwrap() * page],
        cached: (0..pages).map(|index| (index, vec![old; page])).collect(),
        locked: BTreeSet::new(),
        writes_refused: false,
        writes: 0,
      }
    }

    /// The pages `[offset, offset + len)` touches.
    fn pages(offset: u64, len: u32) -> std::ops::Range<u64> {
      offset / SECTOR..(offset + u64::from(len)).div_ceil(SECTOR)
    }
  }

  impl PurgeTarget for Modelled {
    fn write_zeros(&mut self, offset: u64, len: u32) -> bool {
      if self.writes_refused {
        return false;
      }
      self.writes += 1;
      for index in Modelled::pages(offset, len) {
        if !self.locked.contains(&index) {
          self.cached.remove(&index);
        }
      }
      true
    }

    fn read_cached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool {
      out.clear();
      let page = usize::try_from(SECTOR).unwrap();
      for index in Modelled::pages(offset, len) {
        let start = usize::try_from(index).unwrap() * page;
        let host = self.host[start..start + page].to_vec();
        out.extend_from_slice(self.cached.entry(index).or_insert(host));
      }
      true
    }

    fn read_uncached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool {
      out.clear();
      let start = usize::try_from(offset).unwrap();
      out.extend_from_slice(&self.host[start..start + usize::try_from(len).unwrap()]);
      true
    }
  }

  /// The purge of a file's first `pages` pages, a page a write.
  fn purge_of(pages: u64) -> (Purge, u32) {
    (
      Purge {
        path: "\\file.bin".to_owned(),
        offset: 0,
        len: pages * SECTOR,
      },
      u32::try_from(SECTOR).unwrap(),
    )
  }

  /// Step 8 of the VFS and DAX plan, A-190. Do: purge a changed page Windows drops. Expect the purge taken, and a read
  /// through Windows' cache afterwards the host's new bytes.
  #[test]
  fn a_purge_whose_pages_windows_drops_is_taken() {
    let mut file = Modelled::changed(1, b'A', b'C');
    let (purge, piece) = purge_of(1);
    assert_eq!(purge_through(&mut file, &purge, piece), PurgeOutcome::Taken);
    let mut read = Vec::new();
    assert!(file.read_cached(0, piece, &mut read));
    assert!(read.iter().all(|byte| *byte == b'C'));
  }

  /// Step 8, A-190: the purge that did not take (docs/bugs/2026-11-01-a-winfsp-purge-windows-did-not-take-left-a-mapped-
  /// view-stale.md). Do: purge a changed page Windows cannot drop, its write still written whole. Expect the purge
  /// untaken, since a read through Windows' cache still serves the old bytes: a purge reported as made left a mapped view
  /// stale for 10 s in 9 of 200 loaded iterations on CI's x86_64 runner.
  #[test]
  fn a_purge_windows_could_not_drop_a_page_of_is_untaken() {
    let mut file = Modelled::changed(1, b'A', b'C');
    file.locked.insert(0);
    let (purge, piece) = purge_of(1);
    assert_eq!(
      purge_through(&mut file, &purge, piece),
      PurgeOutcome::Untaken
    );
    assert_eq!(file.writes, 1, "the purge's write was made");
  }

  /// Step 8, A-190. Do: purge three changed pages, the middle one a page Windows cannot drop. Expect the purge untaken,
  /// every piece still written, and the two pages Windows could drop read fresh through its cache.
  #[test]
  fn one_page_windows_could_not_drop_makes_the_purge_untaken_and_the_rest_drop() {
    let mut file = Modelled::changed(3, b'A', b'C');
    file.locked.insert(1);
    let (purge, piece) = purge_of(3);
    assert_eq!(
      purge_through(&mut file, &purge, piece),
      PurgeOutcome::Untaken
    );
    assert_eq!(file.writes, 3, "every piece was written");
    let mut read = Vec::new();
    for (offset, fresh) in [(0, true), (SECTOR, false), (2 * SECTOR, true)] {
      assert!(file.read_cached(offset, piece, &mut read));
      assert_eq!(
        read.iter().all(|byte| *byte == b'C'),
        fresh,
        "the page at {offset}"
      );
    }
  }

  /// Step 8, A-190. Do: purge a page nobody cached, and a purge whose write is refused. Expect the first taken (its read
  /// through the cache reaches the host), the second refused.
  #[test]
  fn a_page_nobody_cached_is_taken_and_a_refused_write_is_refused() {
    let mut uncached = Modelled::changed(1, b'A', b'C');
    uncached.cached.clear();
    let (purge, piece) = purge_of(1);
    assert_eq!(
      purge_through(&mut uncached, &purge, piece),
      PurgeOutcome::Taken
    );
    let mut refused = Modelled::changed(1, b'A', b'C');
    refused.writes_refused = true;
    assert_eq!(
      purge_through(&mut refused, &purge, piece),
      PurgeOutcome::Refused
    );
  }

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
