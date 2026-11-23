//! A shard image's paged families (A-870 piece 1; main 01a20182's rulings of 2026-11-22 ~22:10 and ~22:30, goal 93).
//!
//! **Why.** The repository content store's and homes' entries rode the shard image one piece each: a window, a window
//! let go, a list, a name, a region hold, a landed record. After a whole-history push that is hundreds of thousands of
//! pieces, each built and parsed in the one call a publication or a restore makes; CI's x86 shard spun 1,040 ms in one
//! poll publishing them (run 45110563116, after arena-3's restart).
//!
//! **What.** Each family's entries are grouped into pages over the space of their coordinates (a content identity's
//! first eight bytes read big-endian, or a window's handle): a page is the aligned range of coordinates whose first
//! `depth` bits are `start`'s, and its piece carries every entry of the family in that range, each as the piece it
//! would have been alone (its key and bytes), so a restore expands a page and reads each entry as before. A page whose
//! bytes pass twice the page size splits into its two halves; a page of no entry has no piece. The page size is the
//! shard's copy slice (`ServerConfig::copy_slice_bytes`: the measured memcpy rate times a slice of the step quantum),
//! never a constant, so a page is encoded and parsed within a slice of a step.
//!
//! **The directory.** Which pages a family's coordinates lie in ([`Directory`]): rebuilt from the page pieces a
//! recovered image carries, a coordinate in no page found the largest aligned page its neighbours leave it
//! ([`Directory::page_of`]). A delta re-encodes only the pages its changes lie in ([`Directory::plan_dirty`]); a
//! checkpoint plans every page afresh ([`plan`]).

use std::collections::{BTreeMap, BTreeSet};

/// A family of a shard image's paged entries: each keeps its own pages. Its discriminant is its byte in a page piece's
/// key ([`Family::byte`]), part of the image's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum Family {
  /// The repository windows kept as chunks, by handle.
  /// Format: its byte in a page piece's key.
  Windows = 1,
  /// The content store's lists, by identity.
  /// Format: its byte in a page piece's key.
  Lists = 2,
  /// The content store's names, by identity.
  /// Format: its byte in a page piece's key.
  Names = 3,
  /// The content store's windows let go, by identity.
  /// Format: its byte in a page piece's key.
  Aways = 4,
  /// The objects this shard holds for the region, by identity.
  /// Format: its byte in a page piece's key.
  RegionHolds = 5,
  /// Homes' landed records, by identity.
  /// Format: its byte in a page piece's key.
  Landed = 6,
}

impl Family {
  /// Every family, in the order an image carries them.
  pub(crate) const ALL: [Family; 6] = [
    Family::Windows,
    Family::Lists,
    Family::Names,
    Family::Aways,
    Family::RegionHolds,
    Family::Landed,
  ];

  /// Its byte in a page piece's key: its discriminant.
  pub(crate) fn byte(self) -> u8 {
    self as u8
  }

  /// The family a page piece's key names by its byte.
  pub(crate) fn of_byte(byte: u8) -> Option<Family> {
    Family::ALL.into_iter().find(|family| family.byte() == byte)
  }
}

/// Format: the deepest page, a page of one coordinate: a coordinate's bits (`u64::BITS`).
const DEEPEST: u8 = 64;

/// The coordinates' bits past the first `depth`, set: a page of `depth`'s span less one (none past the deepest).
fn span_below(depth: u8) -> u64 {
  u64::MAX.checked_shr(u32::from(depth)).unwrap_or(0)
}

/// A page of a family: the coordinates whose first `depth` bits are `start`'s (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Page {
  /// How many leading bits its coordinates share.
  pub(crate) depth: u8,
  /// Its first coordinate.
  pub(crate) start: u64,
}

impl Page {
  /// The page of every coordinate.
  #[cfg(test)]
  pub(crate) const WHOLE: Page = Page { depth: 0, start: 0 };

  /// Its last coordinate.
  pub(crate) fn last(self) -> u64 {
    self.start.saturating_add(span_below(self.depth))
  }

  /// Whether `coordinate` lies in it.
  pub(crate) fn contains(self, coordinate: u64) -> bool {
    (self.start..=self.last()).contains(&coordinate)
  }

  /// Its two halves, none for a page of one coordinate.
  fn halves(self) -> Option<(Page, Page)> {
    if self.depth >= DEEPEST {
      return None;
    }
    let depth = self.depth.saturating_add(1);
    let second = self.start | 1u64.checked_shl(u32::from(DEEPEST.saturating_sub(depth)))?;
    Some((
      Page {
        depth,
        start: self.start,
      },
      Page {
        depth,
        start: second,
      },
    ))
  }

  /// The aligned page of `depth` holding `coordinate`.
  fn at(depth: u8, coordinate: u64) -> Page {
    Page {
      depth,
      start: coordinate & !span_below(depth),
    }
  }
}

/// One entry of a family, as the piece it would be alone: its coordinate, its piece key and its bytes.
/// A page of one family: the key a publication's and a checkpoint's pieces are named by.
pub(crate) type FamilyPage = (Family, Page);

pub(crate) type Entry = (
  u64,
  [u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
  Vec<u8>,
);

/// Format: the bytes a page piece spends on an entry beyond its bytes: its key and its length.
const ENTRY_HEAD: usize = slates_vfs::recover::HELD_PIECE_KEY_BYTES + size_of::<u32>();

/// The bytes `entries` take in a page piece.
fn bytes_of(entries: &[Entry]) -> usize {
  entries.iter().fold(0usize, |sum, (_, _, bytes)| {
    sum.saturating_add(ENTRY_HEAD).saturating_add(bytes.len())
  })
}

/// The pages `entries` (in coordinate order, all inside `page`) fill when each holds at most `limit` bytes, or is a
/// page of one coordinate: each with the range of `entries` it holds, every page of the cover named, those of no entry
/// with an empty range (the directory's, never a piece).
pub(crate) fn plan(
  entries: &[Entry],
  page: Page,
  limit: usize,
) -> Vec<(Page, core::ops::Range<usize>)> {
  let mut out = Vec::new();
  plan_into(entries, (page, 0), limit, &mut out);
  out
}

/// [`plan`] into `out`, `entries` beginning at index `at` of the whole.
fn plan_into(
  entries: &[Entry],
  (page, at): (Page, usize),
  limit: usize,
  out: &mut Vec<(Page, core::ops::Range<usize>)>,
) {
  let end = at.saturating_add(entries.len());
  let halves = page.halves().filter(|_| bytes_of(entries) > limit);
  let Some((first, second)) = halves else {
    out.push((page, at..end));
    return;
  };
  let split = entries.partition_point(|(coordinate, _, _)| *coordinate < second.start);
  let Some((low, high)) = entries.split_at_checked(split) else {
    out.push((page, at..end));
    return;
  };
  plan_into(low, (first, at), limit, out);
  plan_into(high, (second, at.saturating_add(split)), limit, out);
}

/// Format: the version of a page piece's bytes.
const PAGE_VERSION: u8 = 1;

std::thread_local! {
  /// The bytes this shard's thread has encoded into page pieces ([`encode_page`]), ever: what a step's encoding is read
  /// by (A-870 piece 2: a publication's step encodes at most a slice).
  static ENCODED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// The bytes this shard's thread has encoded into page pieces, ever ([`ENCODED`]).
#[cfg(test)]
pub(crate) fn encoded_bytes() -> u64 {
  ENCODED.with(core::cell::Cell::get)
}

/// A page piece's bytes: its version, its entries' count, then each entry's key, length and bytes.
pub(crate) fn encode_page(entries: &[Entry]) -> Vec<u8> {
  let mut out = Vec::with_capacity(
    1usize
      .saturating_add(size_of::<u32>())
      .saturating_add(bytes_of(entries)),
  );
  out.push(PAGE_VERSION);
  out.extend_from_slice(
    &u32::try_from(entries.len())
      .unwrap_or(u32::MAX)
      .to_le_bytes(),
  );
  for (_, key, bytes) in entries {
    out.extend_from_slice(key);
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
  }
  ENCODED.with(|encoded| {
    encoded.set(
      encoded
        .get()
        .saturating_add(u64::try_from(out.len()).unwrap_or(u64::MAX)),
    );
  });
  out
}

/// The entries a page piece's bytes carry, each its key and bytes; none for bytes no page wrote (a version, count or
/// length past what they hold).
pub(crate) fn decode_page(
  bytes: &[u8],
) -> Option<Vec<([u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES], Vec<u8>)>> {
  let (&version, rest) = bytes.split_first()?;
  if version != PAGE_VERSION {
    return None;
  }
  let (count, mut rest) = rest.split_at_checked(size_of::<u32>())?;
  let count = usize::try_from(u32::from_le_bytes(count.try_into().ok()?)).ok()?;
  // Each entry is at least its head: a count past what the bytes could hold is refused before anything is reserved.
  if count > rest.len().checked_div(ENTRY_HEAD)? {
    return None;
  }
  let mut entries = Vec::with_capacity(count);
  for _ in 0..count {
    let (key, after) = rest.split_at_checked(slates_vfs::recover::HELD_PIECE_KEY_BYTES)?;
    let (len, after) = after.split_at_checked(size_of::<u32>())?;
    let len = usize::try_from(u32::from_le_bytes(len.try_into().ok()?)).ok()?;
    let (entry, after) = after.split_at_checked(len)?;
    entries.push((key.try_into().ok()?, entry.to_vec()));
    rest = after;
  }
  rest.is_empty().then_some(entries)
}

/// A family's pages ([`Page`]), each by its first coordinate: a cover of the coordinates, every page apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Directory {
  /// Each page's depth, by its first coordinate.
  pages: BTreeMap<u64, u8>,
}

impl Directory {
  /// The directory of `pages`, as a recovered image's page pieces name them.
  pub(crate) fn of(pages: impl IntoIterator<Item = Page>) -> Directory {
    Directory {
      pages: pages
        .into_iter()
        .map(|page| (page.start, page.depth))
        .collect(),
    }
  }

  /// Its pages, in coordinate order.
  #[cfg(test)]
  pub(crate) fn pages(&self) -> impl Iterator<Item = Page> + '_ {
    self.pages.iter().map(|(start, depth)| Page {
      depth: *depth,
      start: *start,
    })
  }

  /// The page `coordinate` lies in: its page in the directory, or, in a gap no page of it covers, the largest aligned
  /// page the gap holds (a directory of no page is one page of every coordinate).
  pub(crate) fn page_of(&self, coordinate: u64) -> Page {
    let before = self
      .pages
      .range(..=coordinate)
      .next_back()
      .map(|(start, depth)| Page {
        depth: *depth,
        start: *start,
      });
    if let Some(page) = before.filter(|page| page.contains(coordinate)) {
      return page;
    }
    let after = self
      .pages
      .range(coordinate..)
      .next()
      .map(|(start, _)| *start);
    let gap_first = before.map_or(0, |page| page.last().saturating_add(1));
    let gap_last = after.map_or(u64::MAX, |start| start.saturating_sub(1));
    (0..=DEEPEST)
      .map(|depth| Page::at(depth, coordinate))
      .find(|page| page.start >= gap_first && page.last() <= gap_last)
      .unwrap_or(Page::at(DEEPEST, coordinate))
  }

  /// Replaces `old` with `new` (a page's replanned cover).
  pub(crate) fn replace(&mut self, old: Page, new: impl IntoIterator<Item = Page>) {
    self.pages.remove(&old.start);
    for page in new {
      self.pages.insert(page.start, page.depth);
    }
  }

  /// The pages `coordinates` lie in, each once ([`Directory::page_of`]): what a delta re-encodes.
  pub(crate) fn plan_dirty(&self, coordinates: impl IntoIterator<Item = u64>) -> BTreeSet<Page> {
    coordinates
      .into_iter()
      .map(|coordinate| self.page_of(coordinate))
      .collect()
  }
}

/// Every family's [`Directory`], the shard image's paged families as published.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Directories {
  /// Each family's.
  pub(crate) families: BTreeMap<Family, Directory>,
}

impl Directories {
  /// `family`'s directory: one page of every coordinate where none was published.
  pub(crate) fn of(&self, family: Family) -> Directory {
    self.families.get(&family).cloned().unwrap_or_default()
  }
}

/// What a shard's image encoder holds (A-870 piece 2; main 01a20182's ruling of 2026-11-23: the encoding moves off the
/// publication's step, the image a cut of one moment). The pages of the last committed cut, as their bytes, so a
/// checkpoint copies them; the pages re-encoded since, a slice a turn ([`Encoder::turn`]), and the directories they
/// make; the pages a change since lies in and no turn has re-encoded yet. A publication carries the re-encoded pages
/// only where none is dirty ([`Encoder::cut`]): then they and the committed ones are the shard's entries at that
/// moment exactly. Otherwise it carries the committed cut again, and the arena keeps every block that cut names.
#[derive(Debug, Default)]
pub(crate) struct Encoder {
  /// Each page's bytes in the last committed cut.
  committed: BTreeMap<(Family, Page), Vec<u8>>,
  /// The directories as the pages re-encoded since leave them.
  working: Directories,
  /// The pages re-encoded since the last committed cut: each one's bytes, none for a page now of no entry (its piece
  /// removed).
  pending: BTreeMap<(Family, Page), Option<Vec<u8>>>,
  /// The pages a change since lies in, not yet re-encoded.
  dirty: BTreeSet<(Family, Page)>,
  /// Owed since a publication found the pages dirty: the turn it began at and how many turns it may take
  /// ([`Encoder::skipped`]).
  owed: Option<(u64, u64)>,
  /// The turns the encoder has run.
  turns: u64,
  /// Whether a task is encoding on to publish once caught up (`content_holder::image_catch_up`).
  catching_up: bool,
  /// The tasks whose elective changes wait while paused ([`Encoder::paused`]), each once (`Waker::will_wake`), woken by
  /// the publication that carries the cut ([`Encoder::committed`]). Bound: the let-go tasks live on the shard (a landed
  /// pack's queue, one; a committed hg run's or import's, one each).
  waiting: Vec<core::task::Waker>,
  /// The pages first dirtied while paused, not yet re-encoded: the never-paused changes' own (puts, bring-backs), whose
  /// encoding a turn counts apart ([`Turned::imposed`]). Bound: the dirty pages.
  dirtied_paused: BTreeSet<(Family, Page)>,
}

/// What a turn of the encoder did: the bytes it encoded, and whether no page is dirty after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Turned {
  /// The bytes encoded.
  pub(crate) bytes: u64,
  /// Whether every change is encoded.
  pub(crate) caught_up: bool,
  /// Whether the turn ran while paused.
  pub(crate) paused: bool,
  /// Of the bytes encoded, those of pages first dirtied while paused: the never-paused changes' cost to the encoder,
  /// which the progress rule assumes it outpaces (main 01a20182, 2026-11-23).
  pub(crate) imposed: u64,
}

impl Encoder {
  /// The encoder of a recovered image's `pages` (each family's page and its piece's bytes): its committed cut.
  pub(crate) fn restored(pages: Vec<((Family, Page), Vec<u8>)>) -> Encoder {
    let mut by_family: BTreeMap<Family, Vec<Page>> = BTreeMap::new();
    for ((family, page), _) in &pages {
      by_family.entry(*family).or_default().push(*page);
    }
    let directories = Directories {
      families: by_family
        .into_iter()
        .map(|(family, pages)| (family, Directory::of(pages)))
        .collect(),
    };
    Encoder {
      committed: pages.into_iter().collect(),
      working: directories,
      ..Encoder::default()
    }
  }

  /// Notes the coordinates of `family`'s entries changed since: the pages they lie in are dirty.
  pub(crate) fn mark(&mut self, family: Family, coordinates: impl IntoIterator<Item = u64>) {
    let directory = self.working.of(family);
    let paused = self.paused();
    for page in directory.plan_dirty(coordinates) {
      if self.dirty.insert((family, page)) && paused {
        self.dirtied_paused.insert((family, page));
      }
    }
  }

  /// Whether a change is not yet encoded.
  pub(crate) fn is_dirty(&self) -> bool {
    !self.dirty.is_empty()
  }

  /// Re-encodes dirty pages until `limit` bytes are spent (at least one page), each from what the shard holds now
  /// (`entries` of a family over a range of coordinates), one past twice `page` bytes split into the pages it now fills.
  pub(crate) fn turn(
    &mut self,
    (limit, page_bytes): (usize, usize),
    entries: &dyn Fn(Family, (u64, u64)) -> Vec<Entry>,
  ) -> Turned {
    let paused = self.paused();
    self.turns = self.turns.saturating_add(1);
    let mut spent = 0usize;
    let mut imposed = 0usize;
    while spent < limit {
      let Some((family, page)) = self.dirty.pop_first() else {
        break;
      };
      let spent_before = spent;
      let held = entries(family, (page.start, page.last()));
      let planned = if bytes_of(&held) > page_bytes.saturating_mul(2) {
        // Its own piece removed, its halves put.
        self.pending.insert((family, page), None);
        plan(&held, page, page_bytes)
      } else {
        vec![(page, 0..held.len())]
      };
      for (into, range) in &planned {
        let bytes = held
          .get(range.clone())
          .filter(|held| !held.is_empty())
          .map(encode_page);
        spent = spent.saturating_add(bytes.as_ref().map_or(0, Vec::len));
        self.pending.insert((family, *into), bytes);
      }
      let mut directory = self.working.of(family);
      directory.replace(page, planned.into_iter().map(|(page, _)| page));
      self.working.families.insert(family, directory);
      if self.dirtied_paused.remove(&(family, page)) {
        imposed = imposed.saturating_add(spent.saturating_sub(spent_before));
      }
    }
    Turned {
      bytes: u64::try_from(spent).unwrap_or(u64::MAX),
      caught_up: self.dirty.is_empty(),
      paused,
      imposed: u64::try_from(imposed).unwrap_or(u64::MAX),
    }
  }

  /// The pages a publication carries, where no change is left to encode: each page re-encoded since the last committed
  /// cut, its bytes or none to remove its piece. None while a page is dirty: the publication carries no page then.
  pub(crate) fn cut(&self) -> Option<impl Iterator<Item = (FamilyPage, Option<&[u8]>)> + '_> {
    (!self.is_dirty()).then(|| {
      self
        .pending
        .iter()
        .map(|(page, bytes)| (*page, bytes.as_deref()))
    })
  }

  /// Every page a checkpoint carries with its bytes: the shard's now, where no change is left to encode (the committed
  /// cut with the re-encoded pages over it), else the committed cut alone; and whether it is the shard's now.
  pub(crate) fn whole(&self) -> (Vec<(FamilyPage, &[u8])>, bool) {
    if self.is_dirty() {
      let pages = self
        .committed
        .iter()
        .map(|(page, bytes)| (*page, bytes.as_slice()))
        .collect();
      return (pages, false);
    }
    let mut pages: BTreeMap<(Family, Page), &[u8]> = self
      .committed
      .iter()
      .map(|(page, bytes)| (*page, bytes.as_slice()))
      .collect();
    for (page, bytes) in &self.pending {
      match bytes {
        Some(bytes) => {
          pages.insert(*page, bytes.as_slice());
        }
        None => {
          pages.remove(page);
        }
      }
    }
    (pages.into_iter().collect(), true)
  }

  /// A publication committed: where it carried the encoder's cut (`carried`), the re-encoded pages are the committed
  /// cut now and nothing is owed; else nothing changes.
  pub(crate) fn committed(&mut self, carried: bool) {
    if !carried {
      return;
    }
    for (page, bytes) in core::mem::take(&mut self.pending) {
      match bytes {
        Some(bytes) => {
          self.committed.insert(page, bytes);
        }
        None => {
          self.committed.remove(&page);
        }
      }
    }
    self.owed = None;
    self.dirtied_paused.clear();
    for waker in core::mem::take(&mut self.waiting) {
      waker.wake();
    }
  }

  /// A publication found the pages dirty and carried none: owed from this turn, within a bound of turns derived from
  /// what is dirty (each dirty page is at most twice the page size and a turn encodes at least the page size, so twice
  /// the dirty pages and one more). The turns it has been owed, none where it was not.
  pub(crate) fn skipped(&mut self) -> u64 {
    let dirty = u64::try_from(self.dirty.len()).unwrap_or(u64::MAX);
    let turns = self.turns;
    let (since, _) = *self
      .owed
      .get_or_insert((turns, dirty.saturating_mul(2).saturating_add(1)));
    turns.saturating_sub(since)
  }

  /// Whether the elective changes pause (main's progress rule): a publication owed past its bound of turns.
  pub(crate) fn paused(&self) -> bool {
    self
      .owed
      .is_some_and(|(since, bound)| self.turns.saturating_sub(since) > bound)
  }

  /// Notes a task to wake once the pause ends ([`Encoder::waiting`]).
  pub(crate) fn wait(&mut self, waker: &core::task::Waker) {
    if !self.waiting.iter().any(|waiting| waiting.will_wake(waker)) {
      self.waiting.push(waker.clone());
    }
  }

  /// Whether a publication is owed.
  pub(crate) fn owed(&self) -> bool {
    self.owed.is_some()
  }

  /// Test support (A-870 piece 2's oracle): the turns run, the pages dirty, and the owed publication's start and bound.
  #[cfg(test)]
  pub(crate) fn progress(&self) -> (u64, usize, Option<(u64, u64)>) {
    (self.turns, self.dirty.len(), self.owed)
  }

  /// Notes a task encoding on, where none is: whether this one is to start it.
  pub(crate) fn start_catching_up(&mut self) -> bool {
    !core::mem::replace(&mut self.catching_up, true)
  }

  /// Notes the catching-up task ended.
  pub(crate) fn stop_catching_up(&mut self) {
    self.catching_up = false;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// An entry at `coordinate` of `len` bytes.
  fn entry(coordinate: u64, len: usize) -> Entry {
    let mut key = [0u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES];
    key[1..9].copy_from_slice(&coordinate.to_be_bytes());
    (coordinate, key, vec![7u8; len])
  }

  /// The plan's own half of `pages_cover_their_coordinates_and_hold_at_most_the_page_size`, in round `round`: the
  /// `planned` pages apart and in order from coordinate 0, the one ending the space the last; each page's bytes of
  /// `entries` within `limit`, or the page the deepest or of one coordinate; every entry it holds inside it.
  fn assert_the_plan_covers_in_order(
    round: u64,
    entries: &[Entry],
    planned: &[(Page, core::ops::Range<usize>)],
    limit: usize,
  ) {
    let mut expect_start = 0u64;
    for (at, (page, range)) in planned.iter().enumerate() {
      assert_eq!(
        page.start, expect_start,
        "round {round}: pages apart and in order"
      );
      let held = &entries[range.clone()];
      assert!(
        bytes_of(held) <= limit
          || page.depth == DEEPEST
          || held.len() <= 1
          || held.iter().all(|(c, _, _)| *c == held[0].0),
        "round {round}: page {at} past the page size"
      );
      assert!(
        held
          .iter()
          .all(|(coordinate, _, _)| page.contains(*coordinate))
      );
      expect_start = page.last().wrapping_add(1);
      if page.last() == u64::MAX {
        assert_eq!(
          at,
          planned.len() - 1,
          "round {round}: the last page ends the space"
        );
      }
    }
  }

  /// A-870 piece 1. Do: plan pages over generated entries (counts, coordinates spread or crowded, entry sizes), at a
  /// page size; then find each entry's page in the directory of the plan. Expect: the pages a cover of every coordinate,
  /// apart, in order; each page's bytes within the page size or a page of one coordinate; every entry in exactly the
  /// page its coordinate lies in; a gap of no page found a page of its own apart from the plan's.
  #[test]
  fn pages_cover_their_coordinates_and_hold_at_most_the_page_size() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
      state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      state
    };
    for round in 0..200u64 {
      let count = usize::try_from(next() % 400).unwrap();
      let crowded = round % 3 == 0;
      let mut entries: Vec<Entry> = (0..count)
        .map(|_| {
          let coordinate = if crowded { next() % 64 } else { next() };
          entry(coordinate, usize::try_from(next() % 90).unwrap())
        })
        .collect();
      entries.sort_by_key(|(coordinate, _, _)| *coordinate);
      let limit = 200 + usize::try_from(next() % 2000).unwrap();
      let planned = plan(&entries, Page::WHOLE, limit);
      assert_the_plan_covers_in_order(round, &entries, &planned, limit);
      let directory = Directory::of(planned.iter().map(|(page, _)| *page));
      for (page, range) in &planned {
        for (coordinate, _, _) in &entries[range.clone()] {
          assert_eq!(directory.page_of(*coordinate), *page);
        }
      }
      // A directory of only the plan's non-empty pages (as a recovered image names them) finds a gap's page apart.
      let published = Directory::of(
        planned
          .iter()
          .filter(|(_, range)| !range.is_empty())
          .map(|(page, _)| *page),
      );
      let probe = next();
      let found = published.page_of(probe);
      assert!(found.contains(probe));
      for page in published.pages() {
        assert!(page == found || page.last() < found.start || page.start > found.last());
      }
    }
  }

  /// A-870 piece 1. Do: encode a page of generated entries and decode it; decode cut and corrupted bytes. Expect: the
  /// entries back in order; every cut, a count past the bytes, and another version refused, none panicking.
  #[test]
  fn a_page_piece_reads_back_its_entries_and_refuses_what_no_page_wrote() {
    let entries: Vec<Entry> = (0..50)
      .map(|at| entry(at, usize::try_from(at % 7).unwrap()))
      .collect();
    let bytes = encode_page(&entries);
    let back = decode_page(&bytes).unwrap();
    assert_eq!(
      back,
      entries
        .iter()
        .map(|(_, key, bytes)| (*key, bytes.clone()))
        .collect::<Vec<_>>()
    );
    for cut in 0..bytes.len() {
      assert!(decode_page(&bytes[..cut]).is_none(), "cut at {cut}");
    }
    let mut counted = bytes.clone();
    counted[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_page(&counted).is_none());
    let mut versioned = bytes;
    versioned[0] = PAGE_VERSION.wrapping_add(1);
    assert!(decode_page(&versioned).is_none());
  }

  /// Shape: how many objects the paged-image tests let go and note: enough that one piece an entry is thousands.
  const ENTRIES: usize = 3_000;

  /// Puts `count` distinct one-window objects in the shard's content store and lets each go landed, then notes a landed
  /// record for each: the shard's windows let go and their records, as a whole-history push leaves them.
  fn let_go_and_note(
    state: &mut crate::state::ShardState,
    (from, count): (usize, usize),
  ) -> Vec<slates_vcs::ContentId> {
    let window = state.store.content.chunk_bytes();
    state.vcs_stores.content_made(window).unwrap();
    let texts: Vec<Vec<u8>> = (from..from + count)
      .map(|at| format!("a text let go once its segment landed, number {at}\n").into_bytes())
      .collect();
    let need: u64 = texts.iter().map(|text| text.len() as u64).sum();
    let ids: Vec<slates_vcs::ContentId> = crate::vcs_store::within_budget(state, need, |stores| {
      let content = stores.content_mut().unwrap();
      texts
        .iter()
        .map(|text| {
          let id = content.put_object(text).unwrap();
          assert!(content.mark_landed(&id));
          content.let_go_landed(&id, None).unwrap();
          id
        })
        .collect()
    });
    for (at, id) in ids.iter().enumerate() {
      crate::vcs_store::homes::note_landed(
        *id,
        crate::vcs_store::homes::Landed::Pack(crate::vcs_store::homes::LandedAt {
          pack: *id,
          len: 1 << 20,
          offset: u64::try_from(at).unwrap(),
        }),
      );
    }
    ids
  }

  /// Shape: the page a loaded machine's copy slice can come to, pinned (w-perf's probe of 2026-11-23: the derived page
  /// was 15,959 bytes, a step quantum of 28,452 ns; at 2,048 bytes the checkpoint carried 386 pieces, past the
  /// `ENTRIES / 10` an earlier bound held it to, which failed so once under a load average of 70 to 80).
  const LOADED_PAGE: usize = 2_048;

  /// Derived: a page splits only past the page size, so the two halves of every split hold more than a page between
  /// them, and over identities' coordinates (BLAKE3, uniform) a family's pages hold half a page or more on the mean. So a
  /// family's pages are at most twice its bytes over the page size, and one for a family of less than a page. Measured
  /// on the Studio, 2026-11-23: 1.43 to 1.47 times, at pages of 15,959, 4,096 and 2,048 bytes.
  const PAGES_A_PAGE_OF_BYTES: usize = 2;

  /// A-870 piece 1 (main 01a20182's rulings of 2026-11-22 ~22:10 and ~22:30; arena-4: 138,989 windows let go and as
  /// many landed records, one image piece each). Do: on a shard, let [`ENTRIES`] one-window objects go landed and note a
  /// landed record for each; take the shard's checkpoint image; read it back as a restore does; once at the shard's own
  /// page size and once at [`LOADED_PAGE`]. Expect, at each: the pieces not pages far fewer than the entries (a tenth at
  /// most); the page pieces within [`PAGES_A_PAGE_OF_BYTES`] times their bytes over the page size the encoder planned
  /// by, plus one a family; and every window let go and every record read back. The page size is the machine's copy
  /// slice, which load shrinks, so the bound is the encoder's own measure, never a count fixed against the wall clock.
  /// Failing first: one piece an entry, twice [`ENTRIES`] pieces.
  #[test]
  fn a_checkpoint_carries_many_entries_in_few_pieces_and_restores_them_whole() {
    for pinned in [0, LOADED_PAGE] {
      let (pieces, (paged, paged_bytes), page, aways, landed) =
        crate::daemon::audit_on_shard(move |state| {
          crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(pinned));
          let _ = let_go_and_note(state, (0, ENTRIES));
          // Since piece 2 a checkpoint carries the encoder's cut once its turns have caught up, else the committed one.
          crate::content_holder::image_drain(state);
          while !crate::content_holder::image_turn(state).caught_up {}
          let image = crate::content_holder::held_image(state);
          let page = crate::content_holder::image_page_bytes(state);
          crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
          let held = crate::content_holder::split_held(&image).unwrap();
          (
            image.len(),
            crate::content_holder::page_pieces_of(&image),
            page,
            held.aways.len(),
            held.landed.len(),
          )
        });
      println!(
        "checkpoint at a page of {page} bytes: {pieces} pieces, {paged} of them pages of {paged_bytes} bytes"
      );
      assert!(
        pieces - paged <= ENTRIES / 10,
        "{} pieces not pages carry {} entries",
        pieces - paged,
        2 * ENTRIES
      );
      let bound = PAGES_A_PAGE_OF_BYTES * paged_bytes.div_ceil(page) + Family::ALL.len();
      assert!(
        paged <= bound,
        "{paged} pages of {paged_bytes} bytes at a page of {page}, past {bound}"
      );
      assert_eq!((aways, landed), (ENTRIES, ENTRIES), "every entry read back");
    }
  }

  /// A-870 piece 1. Do: on a shard whose pages are small ([`crate::content_holder::TEST_PAGE_BYTES`]), let many objects
  /// go and note them, publish a checkpoint (pages planned afresh), then a delta for one window brought back and one more
  /// let go, then a delta for many more let go; fold each delta over the checkpoint as recovery does. Expect: the one-
  /// change delta carrying at most two pages of each family it touches, never one an entry; the mass delta splitting
  /// pages, each page within twice the page size; and the folded image, read entry by entry, the shard's own after
  /// every step, so a page split or re-encoded never loses or doubles an entry.
  #[test]
  fn a_delta_re_encodes_only_its_changes_pages_and_folds_to_the_shards_image() {
    const PAGE: usize = 4096;
    let outcome = crate::daemon::audit_on_shard(|state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
      let mut outcome: Vec<String> = Vec::new();
      // Since piece 2 the encoder re-encodes the dirty pages a turn at a time: each publication here follows its
      // turns to the end, so it carries the cut of that moment.
      let encode_up = |state: &mut crate::state::ShardState| {
        crate::content_holder::image_drain(state);
        while !crate::content_holder::image_turn(state).caught_up {}
      };
      let ids = let_go_and_note(state, (0, ENTRIES));
      encode_up(state);
      let (image, carried) = crate::content_holder::held_image_paged(state);
      assert!(carried, "the checkpoint carries the encoder's cut");
      let mut journal: BTreeMap<_, _> = image
        .iter()
        .map(|piece| (piece.key, piece.bytes.clone()))
        .collect();
      crate::content_holder::held_committed(state, crate::content_holder::whole_of(&image), true);
      crate::content_holder::image_committed(state, carried);
      let agrees =
        |journal: &BTreeMap<[u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES], Vec<u8>>,
         state: &crate::state::ShardState| {
          let folded: Vec<slates_vfs::recover::HeldPiece> = journal
            .iter()
            .map(|(key, bytes)| slates_vfs::recover::HeldPiece {
              key: *key,
              bytes: bytes.clone(),
            })
            .collect();
          crate::content_holder::expanded(&folded)
            == crate::content_holder::expanded(&crate::content_holder::held_image(state))
        };
      // One window brought back, one more let go.
      let bytes = format!("a text let go once its segment landed, number {}\n", 7).into_bytes();
      crate::vcs_store::within_budget(state, 64, |stores| {
        stores
          .content_mut()
          .unwrap()
          .bring_back(&ids[7], &bytes)
          .unwrap();
      });
      let _ = let_go_and_note(state, (ENTRIES, 1));
      let mut deltas = Vec::new();
      for step in ["one change", "many changes"] {
        if step == "many changes" {
          let _ = let_go_and_note(state, (ENTRIES + 1, ENTRIES));
        }
        encode_up(state);
        let changes = crate::content_holder::held_changes(state);
        assert!(
          changes.image_carried,
          "{step}: the delta carries the encoder's cut"
        );
        let paged: Vec<&slates_vfs::recover::HeldPiece> = changes
          .put
          .iter()
          .filter(|piece| crate::content_holder::page_family(&piece.key).is_some())
          .collect();
        for piece in &paged {
          let entries = decode_page(&piece.bytes).unwrap();
          let bytes: usize = entries
            .iter()
            .map(|(_, bytes)| ENTRY_HEAD + bytes.len())
            .sum();
          if bytes > 2 * PAGE && entries.len() > 1 {
            outcome.push(format!(
              "{step}: a page of {bytes} bytes past twice the page size"
            ));
          }
        }
        deltas.push(paged.len());
        for piece in &changes.put {
          journal.insert(piece.key, piece.bytes.clone());
        }
        for key in &changes.removed {
          journal.remove(key);
        }
        crate::content_holder::held_committed(state, changes.whole, false);
        crate::content_holder::image_committed(state, changes.image_carried);
        if !agrees(&journal, state) {
          outcome.push(format!("{step}: the folded image is not the shard's"));
        }
      }
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
      (outcome, deltas)
    });
    let (outcome, deltas) = outcome;
    assert!(outcome.is_empty(), "{outcome:#?}");
    // The one-change delta: the aways' page for the window back, its page for the one let go, and the landed
    // records' page: a handful, never thousands.
    assert!(
      deltas[0] <= 6,
      "the one-change delta carried {} pages",
      deltas[0]
    );
    assert!(
      deltas[1] > deltas[0],
      "the mass delta split pages: {deltas:?}"
    );
  }

  /// A-870 piece 2 (main 01a20182's ruling of 2026-11-23: the encoding moves off the publication's step, the image a cut
  /// of one moment). Do: on a shard whose pages are small ([`crate::content_holder::TEST_PAGE_BYTES`]), let many objects
  /// go and note them, publish (`verbs::publish_shard`), then let as many more go and publish again and again, reading
  /// the bytes each publication encodes in its own step ([`encoded_bytes`]); then read the shard's committed image back
  /// as a restart would. Expect: no publication's step encodes past three page sizes (a turn's slice and the one page
  /// that crosses it), and within a bounded number of publications the committed image equals the shard's own, entry by
  /// entry. Failing first: the publication after the mass let-go encoded every dirty page in its one step.
  #[test]
  fn a_publication_encodes_at_most_a_slice_in_its_step_and_its_cuts_catch_up() {
    const PAGE: usize = 4096;
    let (longest, caught_up) = crate::daemon::audit_on_shard(|state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
      let _ = let_go_and_note(state, (0, ENTRIES));
      let _ = crate::verbs::publish_shard(state);
      let _ = let_go_and_note(state, (ENTRIES, ENTRIES));
      let mut longest = 0u64;
      let mut caught_up = None;
      for at in 0..(4 * ENTRIES) {
        let before = encoded_bytes();
        let _ = crate::verbs::publish_shard(state);
        longest = longest.max(encoded_bytes().saturating_sub(before));
        if crate::verbs::committed_held_agrees(state) {
          caught_up = Some(at);
          break;
        }
      }
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
      (longest, caught_up)
    });
    assert!(
      longest <= u64::try_from(3 * PAGE).unwrap(),
      "a publication's step encoded {longest} bytes at a {PAGE}-byte page"
    );
    assert!(
      caught_up.is_some(),
      "the committed image caught up with the shard"
    );
  }

  /// A-870 piece 2, the pause's wake (w-git's catch: a let-go task that ends while packs are queued strands them, so a
  /// paused task waits instead). Do: owe a publication and run turns past its bound; register one task's waker twice;
  /// commit a publication that carried the committed cut again, then one that carried the encoder's. Expect: paused past
  /// the bound, the waker kept once, nothing woken by the older cut's publication, and the task woken once, the pause
  /// ended and no waker kept, by the one that carried the encoder's cut.
  #[test]
  fn a_paused_encoder_wakes_its_waiting_tasks_at_the_publication_that_carries_its_cut() {
    struct Count(std::sync::atomic::AtomicUsize);
    // D-8 test harness: `std::task::Wake` takes `Arc<Self>` by signature; its two owners are this test, which reads
    // the count, and the `Waker` the encoder keeps while it waits.
    #[allow(clippy::disallowed_types)]
    impl std::task::Wake for Count {
      fn wake(self: std::sync::Arc<Self>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      }
    }
    // D-8 test harness, as above: the `Waker` is made from an `Arc` of the count.
    #[allow(clippy::disallowed_types)]
    let count = std::sync::Arc::new(Count(std::sync::atomic::AtomicUsize::new(0)));
    let waker = std::task::Waker::from(count.clone());
    let woken = || count.0.load(std::sync::atomic::Ordering::Relaxed);
    let mut encoder = Encoder::default();
    let _ = encoder.skipped();
    let (_, _, owed) = encoder.progress();
    let (_, bound) = owed.expect("owed");
    assert!(!encoder.paused(), "paused within its bound");
    encoder.turns += bound + 1;
    assert!(encoder.paused(), "not paused past its bound");
    encoder.wait(&waker);
    encoder.wait(&waker);
    assert_eq!(encoder.waiting.len(), 1, "a task's waker kept once");
    encoder.committed(false);
    assert_eq!(woken(), 0, "woken by a publication of the older cut");
    assert!(encoder.paused());
    encoder.committed(true);
    assert_eq!(woken(), 1, "the task woken once");
    assert!(!encoder.paused() && encoder.waiting.is_empty());
  }

  /// Shape: the oracle's generated histories (the first continuous) and the steps of each: enough that publications
  /// skip, owe, pause and catch up many times in each.
  const HISTORIES: u64 = 6;
  /// Shape: the steps of one generated history.
  const STEPS: usize = 120;

  /// A step's draw from a history's generator (splitmix64), below `bound`.
  fn draw(state: &mut u64, bound: u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = *state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (mixed ^ (mixed >> 31)) % bound.max(1)
  }

  /// What an oracle history keeps beside the shard: the objects it holds and lets go, the cut a restart must read, and
  /// what it found.
  #[derive(Default)]
  struct Oracle {
    /// The generator's state.
    draws: u64,
    /// The next object's number.
    fresh: usize,
    /// The objects held, with their bytes.
    kept: Vec<(slates_vcs::ContentId, Vec<u8>)>,
    /// The objects let go, with their bytes.
    away: Vec<(slates_vcs::ContentId, Vec<u8>)>,
    /// The shard's paged entries at the last publication that carried the encoder's cut.
    cut: Vec<([u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES], Vec<u8>)>,
    /// Once paused: the pages dirty as the pause was seen, and the pages the history's other changes dirtied since.
    paused: Option<(u64, u64)>,
    /// The most windows a restored image named (the reuse check is never vacuous).
    named: usize,
    /// What the checks found.
    found: Vec<String>,
  }

  impl Oracle {
    /// A text of the history's, numbered afresh.
    fn text(&mut self) -> Vec<u8> {
      self.fresh += 1;
      format!("an oracle's text, number {}\n", self.fresh).into_bytes()
    }

    /// The checks at `step` (module test docs): the image a restart reads is the last carried cut, entry for entry; no
    /// window it names has its block held by another window now; the owed publication's lag within its bound.
    fn check(&mut self, state: &mut crate::state::ShardState, step: usize) {
      let Some(committed) = crate::verbs::committed_held(state) else {
        self
          .found
          .push(format!("step {step}: the committed journal did not read"));
        return;
      };
      let restored = crate::content_holder::paged_entries_of(&committed);
      if restored != self.cut {
        self.found.push(format!(
          "step {step}: a restart reads {} entries, not the last carried cut's {}",
          restored.len(),
          self.cut.len()
        ));
      }
      let named = crate::content_holder::window_blocks(&restored);
      self.named = self.named.max(named.len());
      let now: BTreeMap<(u16, u64), (u64, slates_vcs::ContentId)> =
        crate::content_holder::window_blocks(&crate::content_holder::paged_entries_now(state))
          .into_iter()
          .map(|(identity, region, offset, len)| ((region, offset), (len, identity)))
          .collect();
      for (identity, region, offset, len) in named {
        for (&(at_region, at), &(at_len, holder)) in now.range(..(region, offset + len)).rev() {
          if at_region != region || at + at_len <= offset {
            break;
          }
          if holder != identity {
            self.found.push(format!(
              "step {step}: the block at {region}:{offset} a committed image names is held by another window"
            ));
          }
        }
      }
      let (turns, dirty, owed) = state.image_encoder.progress();
      match owed {
        None => self.paused = None,
        Some((since, bound)) => {
          let lag = turns - since;
          if lag > bound {
            let (dirty_then, since_dirtied) = *self.paused.get_or_insert((dirty as u64, 0));
            let allowed = bound + dirty_then + since_dirtied + 2;
            if lag > allowed {
              self.found.push(format!(
                "step {step}: owed {lag} turns past its bound's allowance {allowed}"
              ));
            }
          }
        }
      }
    }

    /// A publication (`verbs::publish_shard`), noting the cut where it carried the encoder's, then the checks.
    fn publish(&mut self, state: &mut crate::state::ShardState, step: usize) {
      let skipped = state
        .refusals
        .get(crate::content_holder::IMAGE_SKIPPED)
        .copied()
        .unwrap_or(0);
      if crate::verbs::publish_shard(state).is_err() {
        self
          .found
          .push(format!("step {step}: the publication refused"));
        return;
      }
      let now_skipped = state
        .refusals
        .get(crate::content_holder::IMAGE_SKIPPED)
        .copied()
        .unwrap_or(0);
      if now_skipped == skipped {
        self.cut = crate::content_holder::paged_entries_now(state);
      }
      self.check(state, step);
    }

    /// `count` turns of the encoder as the catch-up task runs them, publishing once caught up where one is owed.
    fn turns(&mut self, state: &mut crate::state::ShardState, (step, count): (usize, u64)) {
      for _ in 0..count {
        crate::content_holder::image_drain(state);
        let turned = crate::content_holder::image_turn(state);
        if turned.caught_up && state.image_encoder.owed() {
          self.publish(state, step);
        }
      }
    }

    /// A change no pause holds (a put, a read's bring-back), whose pages count into the pause's allowance.
    fn unpaused(
      &mut self,
      state: &mut crate::state::ShardState,
      change: impl FnOnce(&mut Oracle, &mut crate::state::ShardState),
    ) {
      crate::content_holder::image_drain(state);
      let before = state.image_encoder.progress().1;
      change(self, state);
      crate::content_holder::image_drain(state);
      let after = state.image_encoder.progress().1;
      if let Some((_, since)) = self.paused.as_mut() {
        *since += (after.saturating_sub(before)) as u64;
      }
    }

    /// One change drawn from `kind`: puts, let-gos (elective: none while paused, as the shard's let-go sites hold them),
    /// a bring-back.
    fn change(&mut self, state: &mut crate::state::ShardState, kind: u64) {
      let count = 1 + usize::try_from(draw(&mut self.draws, 16)).unwrap();
      let elective = matches!(kind, 1 | 2);
      if elective && crate::content_holder::image_paused(state) {
        return;
      }
      match kind {
        0 => self.unpaused(state, |oracle, state| {
          let texts: Vec<Vec<u8>> = (0..count).map(|_| oracle.text()).collect();
          let need: u64 = texts.iter().map(|text| text.len() as u64).sum();
          let ids: Vec<slates_vcs::ContentId> =
            crate::vcs_store::within_budget(state, need, |stores| {
              let content = stores.content_mut().unwrap();
              texts
                .iter()
                .map(|text| content.put_object(text).unwrap())
                .collect()
            });
          oracle.kept.extend(ids.into_iter().zip(texts));
        }),
        1 => {
          let at = self.fresh + 1;
          self.fresh += count;
          let ids = let_go_and_note(state, (1_000_000 + at, count));
          self
            .away
            .extend(ids.into_iter().zip((at..at + count).map(|number| {
              format!(
                "a text let go once its segment landed, number {}\n",
                1_000_000 + number
              )
              .into_bytes()
            })));
        }
        2 => {
          let taken: Vec<(slates_vcs::ContentId, Vec<u8>)> =
            self.kept.drain(..count.min(self.kept.len())).collect();
          crate::vcs_store::within_budget(state, 0, |stores| {
            let content = stores.content_mut().unwrap();
            for (id, _) in &taken {
              let _ = content.mark_landed(id);
              content.let_go_landed(id, None).unwrap();
            }
          });
          self.away.extend(taken);
        }
        _ => self.unpaused(state, |oracle, state| {
          let Some((id, bytes)) = oracle.away.pop() else {
            return;
          };
          crate::vcs_store::within_budget(state, bytes.len() as u64, |stores| {
            stores
              .content_mut()
              .unwrap()
              .bring_back(&id, &bytes)
              .unwrap();
          });
          oracle.kept.push((id, bytes));
        }),
      }
    }
  }

  /// One oracle history on a shard (the oracle's test docs): `seed` 0 the continuous one. What it found, the pauses it
  /// met, and the most windows a restored image named.
  fn history(state: &mut crate::state::ShardState, seed: u64) -> (Vec<String>, u64, usize) {
    let window = state.store.content.chunk_bytes();
    state.vcs_stores.content_made(window).unwrap();
    let mut oracle = Oracle {
      draws: seed,
      ..Oracle::default()
    };
    oracle.publish(state, 0);
    for step in 1..=STEPS {
      if seed == 0 {
        // Continuous: a change, a turn and a publication every step, never a quiet one.
        let kind = draw(&mut oracle.draws, 4);
        oracle.change(state, kind);
        oracle.turns(state, (step, 1));
        oracle.publish(state, step);
        continue;
      }
      match draw(&mut oracle.draws, 10) {
        kind @ 0..=5 => oracle.change(state, kind % 4),
        6 => {
          let count = draw(&mut oracle.draws, 5);
          oracle.turns(state, (step, count));
        }
        7 => oracle.check(state, step),
        _ => oracle.publish(state, step),
      }
    }
    // Changes stopped: the encoder catches up within the pages left dirty, and its publication carries the shard's now.
    // The last steps' changes taken first, so the pages left are all of them: a turn encodes one at least.
    crate::content_holder::image_drain(state);
    let left = state.image_encoder.progress().1 as u64;
    oracle.turns(state, (STEPS + 1, left + 2));
    oracle.publish(state, STEPS + 1);
    let now = crate::content_holder::paged_entries_now(state);
    if oracle.cut != now {
      // Which entries differ, by kind (a key's first byte), and where the encoder stood.
      type Entries = [([u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES], Vec<u8>)];
      let only = |of: &Entries, against: &Entries| {
        let against: std::collections::BTreeSet<_> = against.iter().collect();
        let mut kinds: BTreeMap<u8, usize> = BTreeMap::new();
        for entry in of.iter().filter(|entry| !against.contains(entry)) {
          *kinds.entry(entry.0[0]).or_default() += 1;
        }
        kinds
      };
      oracle.found.push(format!(
        "once quiet, the committed cut is not the shard's own: only in the cut {:?}, only in the shard {:?}, the encoder at {:?}",
        only(&oracle.cut, &now),
        only(&now, &oracle.cut),
        state.image_encoder.progress()
      ));
    }
    let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    let pauses = counted(crate::content_holder::IMAGE_PAUSES);
    // The progress rule's assumption measured: turns while paused counted, the never-paused changes' bytes within the
    // bytes the encoder encoded then.
    let (turns, encoded, imposed) = (
      counted(crate::content_holder::IMAGE_PAUSED_TURNS),
      counted(crate::content_holder::IMAGE_PAUSED_ENCODED_BYTES),
      counted(crate::content_holder::IMAGE_PAUSED_IMPOSED_BYTES),
    );
    if pauses > 0 && turns == 0 {
      oracle
        .found
        .push("paused, yet no turn counted while paused".to_owned());
    }
    if imposed > encoded {
      oracle.found.push(format!(
        "the never-paused changes' {imposed} bytes past the {encoded} encoded while paused"
      ));
    }
    (oracle.found, pauses, oracle.named)
  }

  /// A-870 piece 2's oracle (main 01a20182's ruling of 2026-11-23). Do: on shards whose pages are small
  /// ([`crate::content_holder::TEST_PAGE_BYTES`]), run [`HISTORIES`] generated histories of puts, let-gos, bring-backs,
  /// encoder turns (as the catch-up task runs them), publications and crashes (a restart's read of the committed
  /// journal), the first continuous (a change, a turn and a publication every step); the history makes no let-go while
  /// the encoder pauses them, as the shard's let-go sites hold theirs. Expect, at every publication and crash: the image a
  /// restart reads equal, entry for entry, to the shard's entries at the last publication that carried the encoder's cut
  /// (one committed cut, never a mix of two); no window that image names with its chunk's block held now by another
  /// window (the arena reused none); the owed publication's lag within its bound (the bound it was owed with, then a turn
  /// a page dirty when the pause began and a page the history's other changes dirtied since, and the publication's
  /// own); and, the changes stopped, the cut caught up with the shard. Some history paused, and the images named windows.
  #[test]
  fn generated_histories_restore_one_committed_cut_reuse_no_named_block_and_keep_their_lag_bound() {
    const PAGE: usize = 4096;
    let mut paused = 0;
    let mut named = 0;
    for seed in 0..HISTORIES {
      let (found, pauses, windows) = crate::daemon::audit_on_shard(move |state| {
        crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
        let outcome = history(state, seed);
        crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
        outcome
      });
      assert!(found.is_empty(), "history {seed}: {found:#?}");
      paused += pauses;
      named = named.max(windows);
    }
    assert!(paused > 0, "no history paused its let-gos");
    assert!(named > 0, "no restored image named a window");
  }
}
