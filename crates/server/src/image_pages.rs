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
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Directory {
  /// Each page's depth, by its first coordinate.
  pages: BTreeMap<u64, u8>,
}

impl Clone for Directory {
  /// A copy of every page: work a family's size, which the encoder's turns and drains never pay (A-870 F2), counted
  /// under a test ([`DIRECTORY_PAGES_COPIED`]).
  fn clone(&self) -> Directory {
    #[cfg(test)]
    DIRECTORY_PAGES_COPIED.with(|copied| {
      copied.set(copied.get().saturating_add(self.pages.len()));
    });
    Directory {
      pages: self.pages.clone(),
    }
  }
}

#[cfg(test)]
thread_local! {
  /// Test support (F2): the directory pages this thread's copies of a [`Directory`] copied.
  static DIRECTORY_PAGES_COPIED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
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

/// What a shard's image encoder holds (A-870 piece 2; main 01a20182's ruling of 2026-11-23: the encoding moves off the
/// publication's step, the image a cut of one moment). The pages of the last committed cut, as their bytes, so a
/// checkpoint copies them; the cut in flight ([`Encoder::begin`]: the pages dirty at its moment, each frozen in its
/// store so it reads as at the cut whatever changes after, `slates_vcs::preimage`), its pages re-encoded a slice a turn
/// ([`Encoder::turn`]) and the directories they make; the pages a change since lies in, the next cut's. A publication
/// carries the re-encoded pages once every page of the cut is ([`Encoder::cut`]): then they and the committed ones are
/// the shard's entries at the cut's moment exactly (F2, copy-on-write by entry; main 01a20182's rulings of 2026-11-23).
/// Otherwise it carries the committed cut again, and the arena keeps every block that cut names.
#[derive(Debug, Default)]
pub(crate) struct Encoder {
  /// Each page's bytes in the last committed cut.
  committed: BTreeMap<(Family, Page), Vec<u8>>,
  /// The directories as the pages re-encoded since leave them.
  working: Directories,
  /// The pages re-encoded since the last committed cut: each one's bytes, none for a page now of no entry (its piece
  /// removed).
  pending: BTreeMap<(Family, Page), Option<Vec<u8>>>,
  /// The pages a change since lies in, not yet in a cut: the next cut's.
  dirty: BTreeSet<(Family, Page)>,
  /// The cut in flight: begun, its publication not yet committed.
  cut: Option<Cut>,
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

/// An image cut in flight (F2): the pages dirty at its moment not yet re-encoded, and its
/// generation in homes' tickets (`crate::vcs_store::homes::cut_begun`), published when it is carried.
#[derive(Debug)]
struct Cut {
  /// Its pages not yet re-encoded, each frozen in its store until it is.
  remaining: BTreeSet<(Family, Page)>,
  /// Its generation in homes' tickets.
  generation: u64,
  /// Test support (F2's oracle): its pages at its moment, which bound its pre-images.
  #[cfg(test)]
  frozen: Vec<(Family, Page)>,
}

/// What a turn of the encoder did: the bytes it encoded, and whether the cut in flight is wholly encoded after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Turned {
  /// The bytes spent: encoded, and read by a split ([`Encoder::split_at_stop`]).
  pub(crate) bytes: u64,
  /// Whether the cut in flight is wholly encoded, or none is and nothing is dirty: a publication carries it.
  pub(crate) caught_up: bool,
  /// Whether the turn ran while paused.
  pub(crate) paused: bool,
  /// Of the bytes encoded, those of pages first dirtied while paused: the never-paused changes' cost to the encoder,
  /// which the progress rule assumes it outpaces (main 01a20182, 2026-11-23).
  pub(crate) imposed: u64,
  /// The pages split where a read stopped at its cap ([`Encoder::split_at_stop`]).
  pub(crate) split: u64,
  /// The bytes those reads read and did not keep, read again later.
  pub(crate) discarded: u64,
}

/// A family's entries over a range of coordinates at the cut, read until their bytes pass a cap, and where the read
/// stopped short of the range ([`Encoder::turn`]'s `entries`).
pub(crate) type FamilyReader<'a> =
  &'a dyn Fn(Family, (u64, u64), usize) -> (Vec<Entry>, Option<u64>);

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
    // Read in place: a copy of the directory would be a family's size of work a drain.
    let pages = match self.working.families.get(&family) {
      Some(directory) => directory.plan_dirty(coordinates),
      None => Directory::default().plan_dirty(coordinates),
    };
    let paused = self.paused();
    for page in pages {
      if self.dirty.insert((family, page)) && paused {
        self.dirtied_paused.insert((family, page));
      }
    }
  }

  /// Whether a publication now carries a cut: the one in flight wholly encoded, or none in flight and nothing dirty.
  pub(crate) fn complete(&self) -> bool {
    match &self.cut {
      Some(cut) => cut.remaining.is_empty(),
      None => self.dirty.is_empty(),
    }
  }

  /// Whether a cut may begin: none in flight, and a page dirty.
  pub(crate) fn can_begin(&self) -> bool {
    self.cut.is_none() && !self.dirty.is_empty()
  }

  /// Begins a cut of the pages dirty now, of `generation` in homes' tickets, where one may ([`Encoder::can_begin`]): the
  /// pages, which the caller freezes in their stores before anything changes. None where none may begin.
  pub(crate) fn begin(&mut self, generation: u64) -> Option<Vec<(Family, Page)>> {
    if !self.can_begin() {
      return None;
    }
    let remaining = core::mem::take(&mut self.dirty);
    let pages: Vec<(Family, Page)> = remaining.iter().copied().collect();
    self.cut = Some(Cut {
      remaining,
      generation,
      #[cfg(test)]
      frozen: pages.clone(),
    });
    Some(pages)
  }

  /// Whether a cut is in flight: begun, its publication not yet committed.
  pub(crate) fn cut_is_in_flight(&self) -> bool {
    self.cut.is_some()
  }

  /// Test support (F2's oracle): the cut in flight's generation and its pages at its moment.
  #[cfg(test)]
  pub(crate) fn cut_in_flight(&self) -> Option<(u64, Vec<(Family, Page)>)> {
    self
      .cut
      .as_ref()
      .map(|cut| (cut.generation, cut.frozen.clone()))
  }

  /// Re-encodes the cut's pages until `limit` bytes are spent (at least one page), each from what the shard held at the
  /// cut (`entries` of a family over a range of coordinates, read through its pre-images, until their bytes pass a cap,
  /// [`Encoder::read_cap`]), one past twice `page` bytes split into the pages it now fills; each page re-encoded noted
  /// in `encoded`, for its store to thaw. A page whose read stopped at the cap ([`Encoder::split_at_stop`]) is split.
  pub(crate) fn turn(
    &mut self,
    (limit, page_bytes): (usize, usize),
    entries: FamilyReader<'_>,
    encoded: &mut Vec<(Family, Page)>,
  ) -> Turned {
    let paused = self.paused();
    self.turns = self.turns.saturating_add(1);
    let mut spent = 0usize;
    let mut imposed = 0usize;
    let mut split_at_stop = (0u64, 0usize);
    let cap = Self::read_cap(page_bytes);
    while spent < limit {
      let Some((family, page)) = self.cut.as_mut().and_then(|cut| cut.remaining.pop_first()) else {
        break;
      };
      let spent_before = spent;
      let (held, stopped) = entries(family, (page.start, page.last()), cap);
      if let Some(stop) = stopped.filter(|_| page.halves().is_some()) {
        // The read visited a cap's bytes at least, kept or passed over.
        let read = bytes_of(&held).max(cap);
        spent = spent.saturating_add(read);
        let kept = self.split_at_stop((family, page), (&held, stop), page_bytes, encoded);
        split_at_stop = (
          split_at_stop.0.saturating_add(1),
          split_at_stop.1.saturating_add(read.saturating_sub(kept)),
        );
        if self.dirtied_paused.remove(&(family, page)) {
          imposed = imposed.saturating_add(kept);
        }
        continue;
      }
      // A page of one coordinate is read whole: its entries share their first eight bytes, so it cannot split.
      let held = if stopped.is_some() {
        entries(family, (page.start, page.last()), usize::MAX).0
      } else {
        held
      };
      encoded.push((family, page));
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
      let split: Vec<Page> = planned
        .iter()
        .map(|(page, _)| *page)
        .filter(|into| *into != page)
        .collect();
      self
        .working
        .families
        .entry(family)
        .or_default()
        .replace(page, planned.into_iter().map(|(page, _)| page));
      // A change since the cut marked the page for the next cut; split, its halves are marked in its place.
      if !split.is_empty() && self.dirty.remove(&(family, page)) {
        self.dirty.extend(split.iter().map(|into| (family, *into)));
      }
      if self.dirtied_paused.remove(&(family, page)) {
        imposed = imposed.saturating_add(spent.saturating_sub(spent_before));
      }
    }
    Turned {
      bytes: u64::try_from(spent).unwrap_or(u64::MAX),
      caught_up: self.complete(),
      paused,
      imposed: u64::try_from(imposed).unwrap_or(u64::MAX),
      split: split_at_stop.0,
      discarded: u64::try_from(split_at_stop.1).unwrap_or(u64::MAX),
    }
  }

  /// The most bytes a turn reads of one page: twice the page size, the most a page holds before it splits
  /// ([`Encoder::turn`]). Derived: a page a gap left (a family's first, its directory empty) covers every coordinate the
  /// gap holds, so it can hold the family's every entry; read whole, one turn's work would be the family's.
  fn read_cap(page_bytes: usize) -> usize {
    page_bytes.saturating_mul(2)
  }

  /// Splits `page` of `family`, whose read (`held`, its entries in coordinate order up to the cap) stopped short of its
  /// end at coordinate `stop` (A-870 F2; main 01a20182's ruling of 2026-11-23 15:47): the read is planned over the page
  /// as a whole page's would be ([`plan`]); each planned page that ends before `stop` holds all its entries, so it is
  /// encoded now and noted in `encoded`; the rest go back to the cut, frozen still, read afresh in a later turn. Where
  /// the plan is the page whole (the read kept little: what it visited the cut lacks), its halves are planned instead,
  /// so the page shrinks toward `stop` until a half ends before it. Pages
  /// stay aligned halves, never cut at the read's stop: a page piece's key names its depth and start (the image's format)
  /// and a recovered directory is rebuilt from those keys, so an unaligned page would need a new format; and piece 1's
  /// bound on a family's pages (at most twice its bytes over the page size) rests on halving by bytes. The bytes read
  /// and kept: what was visited and not kept is read again, at most the read's cap and an entry (a turn splits once at
  /// most, its read spending the turn's slice): the planned page holding `stop` where the plan split, the whole visit
  /// where it halved.
  fn split_at_stop(
    &mut self,
    (family, page): (Family, Page),
    (held, stop): (&[Entry], u64),
    page_bytes: usize,
    encoded: &mut Vec<(Family, Page)>,
  ) -> usize {
    let mut planned = plan(held, page, page_bytes);
    if let (true, Some((first, second))) = (planned.len() < 2, page.halves()) {
      let split = held.partition_point(|(coordinate, _, _)| *coordinate < second.start);
      planned = vec![(first, 0..split), (second, split..held.len())];
    }
    // Its own piece removed, the pages it now is put as they are encoded.
    self.pending.insert((family, page), None);
    let mut kept = 0usize;
    let mut back = 0u64;
    for (into, range) in &planned {
      if into.last() < stop {
        let entries = held.get(range.clone()).filter(|held| !held.is_empty());
        kept = kept.saturating_add(entries.map_or(0, bytes_of));
        self
          .pending
          .insert((family, *into), entries.map(encode_page));
        encoded.push((family, *into));
      } else if let Some(cut) = self.cut.as_mut() {
        cut.remaining.insert((family, *into));
        back = back.saturating_add(1);
      }
    }
    let pages: Vec<Page> = planned.iter().map(|(into, _)| *into).collect();
    self
      .working
      .families
      .entry(family)
      .or_default()
      .replace(page, pages.iter().copied());
    // A change since the cut marked the page for the next cut, or first dirtied it in a pause: its pages take its place.
    if self.dirty.remove(&(family, page)) {
      self.dirty.extend(pages.iter().map(|into| (family, *into)));
    }
    // Each page put back is one more to encode, at most two turns' (the owed bound's rate, `Encoder::skipped`).
    if let Some((_, bound)) = self.owed.as_mut() {
      *bound = bound.saturating_add(back.saturating_mul(2));
    }
    kept
  }

  /// The pages a publication carries, where the cut is wholly encoded ([`Encoder::complete`]): each page re-encoded
  /// since the last committed cut, its bytes or none to remove its piece. None while a page of the cut is not: the
  /// publication carries no page then.
  pub(crate) fn cut(&self) -> Option<impl Iterator<Item = (FamilyPage, Option<&[u8]>)> + '_> {
    self.complete().then(|| {
      self
        .pending
        .iter()
        .map(|(page, bytes)| (*page, bytes.as_deref()))
    })
  }

  /// Every page a checkpoint carries with its bytes: the cut's, where it is wholly encoded (the committed cut with the
  /// re-encoded pages over it), else the committed cut alone; and whether it carries the cut.
  pub(crate) fn whole(&self) -> (Vec<(FamilyPage, &[u8])>, bool) {
    if !self.complete() {
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
  /// cut now and nothing is owed; else nothing changes. The carried cut's generation in homes' tickets, none where none
  /// was in flight (nothing dirty: the image is the shard's now).
  pub(crate) fn committed(&mut self, carried: bool) -> Option<u64> {
    if !carried {
      return None;
    }
    let generation = self.cut.take().map(|cut| cut.generation);
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
    generation
  }

  /// A publication carried no cut, or one with a cut begun after it: owed from this turn, within a bound of turns
  /// derived from the cut's pages left (each is at most twice the page size and a turn encodes at least the page size,
  /// so twice the pages and one more; F2: a turn encodes at least one, so the cut ends within them). The turns it has
  /// been owed, none where it was not.
  pub(crate) fn skipped(&mut self) -> u64 {
    let left = self
      .cut
      .as_ref()
      .map_or(self.dirty.len(), |cut| cut.remaining.len());
    let dirty = u64::try_from(left).unwrap_or(u64::MAX);
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

  /// Test support (A-870 piece 2's oracle): the turns run, the pages left to encode (the cut's and the next's), and the
  /// owed publication's start and bound.
  #[cfg(test)]
  pub(crate) fn progress(&self) -> (u64, usize, Option<(u64, u64)>) {
    let cut = self.cut.as_ref().map_or(0, |cut| cut.remaining.len());
    (self.turns, self.dirty.len().saturating_add(cut), self.owed)
  }

  /// Notes a task encoding on, where none is: whether this one is to start it.
  pub(crate) fn start_catching_up(&mut self) -> bool {
    !core::mem::replace(&mut self.catching_up, true)
  }

  /// Notes the catching-up task ended.
  pub(crate) fn stop_catching_up(&mut self) {
    self.catching_up = false;
  }

  /// Whether a publication is owed and no task is encoding on to make it: a catch-up whose publication was refused, or
  /// whose task could not be spawned, which the shard's sweep begins again (`content_holder::image_retry_owed`).
  pub(crate) fn owed_unattended(&self) -> bool {
    self.owed.is_some() && !self.catching_up
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
  /// of one moment; F2's of 15:47: a page a gap left read a cap at a time). Do: on a shard whose pages are small
  /// ([`crate::content_holder::TEST_PAGE_BYTES`]), let many objects go and note them, publish (`verbs::publish_shard`:
  /// every family's first page, its directory empty, is a page of every coordinate), then let as many more go, publish
  /// once and run the catch-up's steps as its task does (`content_holder::image_catch_up_step`: a turn, and once the cut
  /// is encoded its publication), reading the bytes each step encodes ([`encoded_bytes`]) and the bytes its splits read
  /// and did not keep, and after each publication the shard's committed image back as a restart would. Expect: no
  /// step encodes past three page sizes (a turn's slice and the one page that crosses it); none discards
  /// past the read's cap and an entry (twice a page: what a split visited and did not keep); pages split (the capped read
  /// ran); and within a bounded number of publications the committed image equals the shard's entries now, entry by
  /// entry. Failing first: the publication after the mass let-go encoded every dirty page in its one step; then (F2) a
  /// family's first page, its every entry, in one turn (327,620 bytes at a 4,096-byte page), unseen before because the
  /// check compared the journal with the encoder's committed cut, which it always equals while a cut is in flight.
  #[test]
  fn a_publication_encodes_at_most_a_slice_in_its_step_and_its_cuts_catch_up() {
    const PAGE: usize = 4096;
    /// Shape: the most bytes an entry here takes in a page (a landed record's piece: its key, length and bytes).
    const ENTRY: usize = 256;
    let (longest, discarded, split, caught_up) = crate::daemon::audit_on_shard(|state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
      let counted = |state: &crate::state::ShardState, name: &str| {
        state.refusals.get(name).copied().unwrap_or(0)
      };
      let _ = let_go_and_note(state, (0, ENTRIES));
      let _ = crate::verbs::publish_shard(state);
      let _ = let_go_and_note(state, (ENTRIES, ENTRIES));
      let _ = crate::verbs::publish_shard(state);
      let mut longest = 0u64;
      let mut discarded = 0u64;
      let mut caught_up = None;
      for at in 0..(4 * ENTRIES) {
        let before = encoded_bytes();
        let dropped = counted(state, crate::content_holder::IMAGE_SPLIT_DISCARDED_BYTES);
        let (ended, _) = crate::content_holder::image_catch_up_step(state);
        longest = longest.max(encoded_bytes().saturating_sub(before));
        discarded = discarded.max(
          counted(state, crate::content_holder::IMAGE_SPLIT_DISCARDED_BYTES)
            .saturating_sub(dropped),
        );
        if !ended {
          continue;
        }
        let committed = crate::verbs::committed_held(state)
          .map(|held| crate::content_holder::paged_entries_of(&held));
        if committed == Some(crate::content_holder::paged_entries_now(state)) {
          caught_up = Some(at);
          break;
        }
      }
      let split = counted(state, crate::content_holder::IMAGE_PAGES_SPLIT);
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
      (longest, discarded, split, caught_up)
    });
    assert!(
      longest <= u64::try_from(3 * PAGE).unwrap(),
      "a step encoded {longest} bytes at a {PAGE}-byte page"
    );
    assert!(
      discarded <= u64::try_from(2 * PAGE + ENTRY).unwrap(),
      "a step's splits read and dropped {discarded} bytes, past the read's cap and an entry"
    );
    assert!(split > 0, "no page split: the capped read never ran");
    assert!(
      caught_up.is_some(),
      "the committed image caught up with the shard's entries"
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

  /// Puts `count` distinct one-window objects in the shard's content store, numbered from `from`: their identities, in
  /// the order put (each window's handle the next).
  fn put_objects(
    state: &mut crate::state::ShardState,
    (from, count): (usize, usize),
  ) -> Vec<slates_vcs::ContentId> {
    let window = state.store.content.chunk_bytes();
    state.vcs_stores.content_made(window).unwrap();
    let texts: Vec<Vec<u8>> = (from..from + count)
      .map(|at| format!("a text kept in the shard's store, number {at}\n").into_bytes())
      .collect();
    let need: u64 = texts.iter().map(|text| text.len() as u64).sum();
    crate::vcs_store::within_budget(state, need, |stores| {
      let content = stores.content_mut().unwrap();
      texts
        .iter()
        .map(|text| content.put_object(text).unwrap())
        .collect()
    })
  }

  /// Lets the objects `ids` go landed, their windows given back.
  fn let_go(state: &mut crate::state::ShardState, ids: &[slates_vcs::ContentId]) {
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().unwrap();
      for id in ids {
        assert!(content.mark_landed(id));
        content.let_go_landed(id, None).unwrap();
      }
    });
  }

  /// A-870 piece 2, F2 (main 01a20182's ruling of 2026-11-23: the red on the property that matters). Do: on a shard whose
  /// pages are small, put [`ENTRIES`] objects and carry them; let go every hundredth and the third-last, so a cut's
  /// windows pages run from the first to the last, dozens of them; begin the cut with one turn, which encodes its first
  /// page in the encoder's order (by depth, then start: the widest, the last objects') and few more; note the shard's
  /// entries then; let go an object of the page the hundredth-but-one lies in (the 2,900th's), which the cut has not yet
  /// encoded; turn until the cut is
  /// encoded and publish. Expect: the committed image equal, entry for entry, to the shard's entries at the cut; the
  /// window let go after it placed in the windows page and not in the aways page, as at the cut (the two pages agree);
  /// an old value kept (the pre-image path ran). Its slab stays held by the windows still in it, so no chunk is freed
  /// here: the oracle holds the frees a cut holds.
  /// Failing first (each store's hook turned off): its page encoded after the let-go names the window nowhere,
  /// placed nor let go, an image of two moments.
  #[test]
  fn a_window_let_go_while_its_cut_is_encoded_leaves_the_carried_image_one_moment() {
    const PAGE: usize = 4096;
    let outcome = crate::daemon::audit_on_shard(|state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
      let ids = put_objects(state, (0, ENTRIES));
      crate::content_holder::image_drain(state);
      while !crate::content_holder::image_turn(state).caught_up {}
      crate::verbs::publish_shard(state).unwrap();
      let counted = |state: &crate::state::ShardState, name: &str| {
        state.refusals.get(name).copied().unwrap_or(0)
      };
      /// Shape: an object in the page of the last hundredth let go, among the narrow pages the cut encodes after its
      /// first.
      const GONE: usize = 2_905;
      let gone = ids[GONE];
      let mut spread: Vec<slates_vcs::ContentId> = ids.iter().step_by(100).copied().collect();
      spread.push(ids[ENTRIES - 3]);
      let_go(state, &spread);
      let turned = crate::content_holder::image_turn(state);
      let in_flight = state.image_encoder.cut_in_flight().is_some();
      let windows_pages = state.image_encoder.cut_in_flight().map_or(0, |(_, pages)| {
        pages
          .iter()
          .filter(|(family, _)| *family == Family::Windows)
          .count()
      });
      let left = state.image_encoder.progress().1;
      let at_cut = crate::content_holder::paged_entries_now(state);
      let old = counted(state, crate::content_holder::IMAGE_CUT_PREIMAGES_OLD);
      let_go(state, &[gone]);
      while !crate::content_holder::image_turn(state).caught_up {}
      crate::verbs::publish_shard(state).unwrap();
      let restored =
        crate::content_holder::paged_entries_of(&crate::verbs::committed_held(state).unwrap());
      let placed = crate::content_holder::window_blocks(&restored)
        .iter()
        .any(|(identity, ..)| *identity == gone);
      let away = crate::content_holder::aways_among(&restored).contains(&gone);
      let kept_old = counted(state, crate::content_holder::IMAGE_CUT_PREIMAGES_OLD) - old;
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
      (
        (turned.caught_up, in_flight),
        restored == at_cut,
        (placed, away),
        (kept_old, windows_pages, left),
      )
    });
    let ((caught_up, in_flight), one_moment, (placed, away), (kept_old, windows_pages, left)) =
      outcome;
    assert!(
      !caught_up && in_flight,
      "the cut's first turn left a page to encode"
    );
    assert!(one_moment, "the committed image is the shard's at the cut");
    assert!(
      placed && !away,
      "the window let go after the cut is placed and not let go, as at the cut (placed {placed}, away {away})"
    );
    assert!(
      kept_old > 0,
      "an old value kept (the cut's windows pages {windows_pages}, pages left after its first turn {left})"
    );
  }

  /// Shape: the pages of the directory the copy test's encoder holds: a family's pages after a large push (about 20,000
  /// at a 4,096-byte page for 65,536 objects' windows).
  const DIRECTORY_PAGES: u64 = 10_000;

  /// A-870 F2, a turn's and a drain's work against a family's size (CI's macOS debug runs, 2026-11-23: a third of the
  /// catch-up's time measured in its begin and drain). Do: hold a directory of [`DIRECTORY_PAGES`] pages in an encoder,
  /// mark one coordinate dirty, begin a cut and run a turn. Expect: no page of the directory copied. Failing first: the
  /// mark and the turn each copied the family's directory whole (`Directories::of`), a family's size of work a turn.
  #[test]
  fn a_turn_and_a_drain_copy_no_directory_however_many_pages_it_holds() {
    const DEPTH: u8 = 14;
    let pages: Vec<((Family, Page), Vec<u8>)> = (0..DIRECTORY_PAGES)
      .map(|at| {
        let page = Page {
          depth: DEPTH,
          start: at << (64 - u32::from(DEPTH)),
        };
        ((Family::Windows, page), Vec::new())
      })
      .collect();
    let mut encoder = Encoder::restored(pages);
    let coordinate = 5u64 << (64 - u32::from(DEPTH));
    DIRECTORY_PAGES_COPIED.with(|copied| copied.set(0));
    encoder.mark(Family::Windows, [coordinate]);
    assert!(encoder.begin(1).is_some(), "a cut of the dirty page begun");
    let mut encoded = Vec::new();
    let turned = encoder.turn(
      (4096, 4096),
      &|_, _, _| (vec![entry(coordinate, 10)], None),
      &mut encoded,
    );
    assert!(turned.caught_up, "the cut's one page encoded");
    let copied = DIRECTORY_PAGES_COPIED.with(core::cell::Cell::get);
    assert_eq!(
      copied, 0,
      "the mark and the turn copied {copied} directory pages"
    );
  }

  /// Shape: the objects a fresh shard keeps before its first cut in the chase test: a windows gap page of many entries.
  const CHASE_BACKLOG: usize = 2_000;
  /// Shape: the objects put between two of the chase test's turns, each a window at the end of the handle space: more
  /// than one capped read of its small page visits.
  const CHASE_PUTS_A_TURN: usize = 64;

  /// A-870 F2 against a dirtier at the frozen range's end (main 01a20182, 2026-11-23 22:09: CI's macOS debug runs red
  /// 4 of 10, about 1,021 pages split each, the cut never carried). Do: on a fresh shard whose pages are small, keep
  /// [`CHASE_BACKLOG`] objects (the windows family's first page a gap page, every coordinate on), begin a cut; then, a
  /// turn at a time, put [`CHASE_PUTS_A_TURN`] fresh objects (windows whose handles land at the end of that frozen page)
  /// and run a turn. Expect: the cut wholly encoded within its bound (twice its bytes over the page size, plus a page a
  /// depth for its splits), whatever is put meanwhile. A guard, not a red: written for the suspicion that the capped
  /// read, visiting every handle put after the cut, would chase the frozen page's tail forever; it passed before any
  /// change (21.9 s, debug), since handles are dense integers and a page of width w holds at most w of them, so a read
  /// finishes each page in bounded visits. CI's macOS debug reds were the catch-up's throughput instead
  /// ([`a_turn_and_a_drain_copy_no_directory_however_many_pages_it_holds`]).
  #[test]
  fn a_cut_is_encoded_while_every_turn_puts_windows_at_its_frozen_pages_end() {
    const PAGE: usize = 512;
    let (done, turns, bound) = crate::daemon::audit_on_shard(|state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
      let _ = put_objects(state, (0, CHASE_BACKLOG));
      let bytes: usize = crate::content_holder::paged_entries_placed(state)
        .iter()
        .map(|(_, _, key, value)| key.len() + value.len())
        .sum();
      let bound = 2 * bytes / PAGE + 64 * 6;
      let mut turns = 0;
      let mut done = crate::content_holder::image_turn(state).caught_up;
      let mut fresh = CHASE_BACKLOG;
      while !done && turns < bound {
        let _ = put_objects(state, (fresh, CHASE_PUTS_A_TURN));
        fresh += CHASE_PUTS_A_TURN;
        done = crate::content_holder::image_turn(state).caught_up;
        turns += 1;
      }
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
      (done, turns, bound)
    });
    assert!(
      done,
      "the cut wholly encoded within {bound} turns while windows were put at its end: not done after {turns}"
    );
  }

  /// A-870 F2, the catch-up's pacing (main 01a20182's ruling (a) of 2026-11-23 19:16). Do: on a shard whose pages are
  /// small (a page far below its copy slice, `DaemonConfig::copy_slice_bytes`), put [`ENTRIES`] objects and carry them,
  /// let every other one go and publish, so a cut of many pages is owed; run one wake of the catch-up
  /// (`content_holder::image_catch_up_slice`). Expect: the wake ran turns until the cut ended or a copy slice's bytes
  /// were spent, more than one turn. Failing first (a turn a wake): one turn, a page's bytes, the cut still owed.
  #[test]
  fn a_catch_up_wake_spends_a_copy_slice_not_one_turn() {
    /// Shape: a page far below the copy slice, which follows the shard's live step quantum (5,718 bytes measured on a
    /// loaded Mac, 41,509 on the next run), so a turn's three pages stay under it.
    const PAGE: usize = 512;
    let (slice, turns, spent, ended) = crate::daemon::audit_on_shard(|state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
      let counted = |state: &crate::state::ShardState, name: &str| {
        state.refusals.get(name).copied().unwrap_or(0)
      };
      let ids = put_objects(state, (0, ENTRIES));
      while !crate::content_holder::image_turn(state).caught_up {}
      crate::verbs::publish_shard(state).unwrap();
      let every_other: Vec<slates_vcs::ContentId> = ids.iter().step_by(2).copied().collect();
      let_go(state, &every_other);
      crate::verbs::publish_shard(state).unwrap();
      let slice = state.config.copy_slice_bytes();
      let turns = counted(state, crate::content_holder::IMAGE_TURNS);
      let spent = counted(state, crate::content_holder::IMAGE_ENCODED_BYTES);
      let ended = crate::content_holder::image_catch_up_slice(state);
      let outcome = (
        slice,
        counted(state, crate::content_holder::IMAGE_TURNS) - turns,
        counted(state, crate::content_holder::IMAGE_ENCODED_BYTES) - spent,
        ended,
      );
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
      outcome
    });
    assert!(
      slice > u64::try_from(3 * PAGE).unwrap(),
      "the copy slice ({slice} bytes) is past a turn's three pages, or the test asks nothing"
    );
    assert!(
      ended || spent >= slice,
      "the wake ended the cut or spent its slice: {spent} of {slice} bytes in {turns} turns"
    );
    assert!(turns > 1, "the wake ran {turns} turns");
  }

  /// A-870 piece 2, the pause under refused publications (main 01a20182's ruling of 2026-11-23: with F2 a cut ends within
  /// its pages, so only a refused publication keeps one owed past its bound). Do: on a shard whose pages are small, put
  /// [`ENTRIES`] objects and carry them, then let every other one go and publish, so a cut of many pages is owed; refuse the catch-up's publications past the owed bound's turns
  /// ([`crate::content_holder::CATCH_UP_REFUSALS`]), running the catch-up's steps as its task does, the shard's sweep
  /// beginning it again after each; ask the elective let-go's question while paused; then let the refusals clear.
  /// Expect: paused, the pause and its turns counted, while refused; the pause released, nothing owed, once a
  /// publication goes through.
  #[test]
  fn a_cut_owed_past_its_bound_while_its_publications_are_refused_pauses_let_gos_until_one_commits()
  {
    const PAGE: usize = 4096;
    let outcome = crate::daemon::audit_on_shard(|state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
      let ids = put_objects(state, (0, ENTRIES));
      while !crate::content_holder::image_turn(state).caught_up {}
      crate::verbs::publish_shard(state).unwrap();
      let every_other: Vec<slates_vcs::ContentId> = ids.iter().step_by(2).copied().collect();
      let_go(state, &every_other);
      crate::verbs::publish_shard(state).unwrap();
      let (_, _, owed) = state.image_encoder.progress();
      let (_, bound) = owed.expect("a cut owed");
      let refusals = u32::try_from(bound).unwrap() + 2;
      crate::content_holder::CATCH_UP_REFUSALS.with(|left| left.set(refusals));
      let mut paused_seen = false;
      for _ in 0..=refusals {
        while !crate::content_holder::image_catch_up_step(state).0 {}
        paused_seen |= crate::content_holder::image_paused(state);
      }
      let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      let counts = (
        counted(crate::content_holder::IMAGE_CATCH_UP_PUBLISH_REFUSED),
        counted(crate::content_holder::IMAGE_PAUSES),
        counted(crate::content_holder::IMAGE_PAUSED_TURNS),
      );
      let released = !state.image_encoder.paused() && !state.image_encoder.owed();
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
      (refusals, paused_seen, counts, released)
    });
    let (refusals, paused_seen, (refused, pauses, paused_turns), released) = outcome;
    assert_eq!(refused, u64::from(refusals), "each refusal counted");
    assert!(
      paused_seen && pauses > 0 && paused_turns > 0,
      "paused past the bound, counted ({pauses} pauses, {paused_turns} turns)"
    );
    assert!(
      released,
      "the pause released once a publication went through"
    );
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
    /// The cut in flight as the oracle saw it begin (F2): its generation, the shard's entries at its moment, the entries
    /// in its pages then (E), its page count, and the pre-image counters then.
    begun: Option<Begun>,
    /// The keys the changes since the last turn inserted into any paged family, and the windows they freed.
    since_turn: (u64, u64),
    /// The most keys, and windows freed, the changes between two turns made: I's and the held frees' factor a turn.
    most_a_turn: (u64, u64),
    /// The most chunks the windows held for one cut.
    held_most: usize,
    /// What the checks found.
    found: Vec<String>,
  }

  /// A cut as the oracle saw it begin (F2).
  struct Begun {
    /// Its generation in homes' tickets.
    generation: u64,
    /// The shard's paged entries at its moment, as (key, bytes) in key order.
    entries: Vec<([u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES], Vec<u8>)>,
    /// The entries in its pages at its moment: E.
    in_pages: u64,
    /// Its pages at its moment.
    pages: u64,
    /// The encoder's turns as the oracle saw it begin (the turn that began it among them).
    turned: u64,
    /// The old values' and absent markers' counters at its moment.
    counted: (u64, u64),
  }

  impl Oracle {
    /// A text of the history's, numbered afresh.
    fn text(&mut self) -> Vec<u8> {
      self.fresh += 1;
      format!("an oracle's text, number {}\n", self.fresh).into_bytes()
    }

    /// A counter of the shard's.
    fn counted(state: &crate::state::ShardState, name: &str) -> u64 {
      state.refusals.get(name).copied().unwrap_or(0)
    }

    /// Before a turn (the catch-up's, or a publication's): the changes since the last one closed as an interval.
    fn turning(&mut self) {
      self.most_a_turn = (
        self.most_a_turn.0.max(self.since_turn.0),
        self.most_a_turn.1.max(self.since_turn.1),
      );
      self.since_turn = (0, 0);
    }

    /// After a turn or a publication: a cut begun since noted with the shard's entries at its moment (nothing changes
    /// the stores between a cut's begin and the turn or publication returning).
    fn note_cut(&mut self, state: &crate::state::ShardState) {
      let Some((generation, pages)) = state.image_encoder.cut_in_flight() else {
        return;
      };
      if self
        .begun
        .as_ref()
        .is_some_and(|begun| begun.generation == generation)
      {
        return;
      }
      let placed = crate::content_holder::paged_entries_placed(state);
      let in_pages = placed
        .iter()
        .filter(|(family, coordinate, ..)| {
          pages
            .iter()
            .any(|(at, page)| at == family && page.contains(*coordinate))
        })
        .count() as u64;
      let mut entries: Vec<_> = placed
        .into_iter()
        .map(|(_, _, key, bytes)| (key, bytes))
        .collect();
      entries.sort();
      self.begun = Some(Begun {
        generation,
        entries,
        in_pages,
        pages: pages.len() as u64,
        turned: state.image_encoder.progress().0,
        counted: (
          Self::counted(state, crate::content_holder::IMAGE_CUT_PREIMAGES_OLD),
          Self::counted(state, crate::content_holder::IMAGE_CUT_PREIMAGES_ABSENT),
        ),
      });
    }

    /// The turns the cut in flight has lasted, the next among them: at most its pages and the pages its splits put back
    /// (each turn encodes or splits one at least), which the owed lag check bounds.
    fn turns_lasted(&self, state: &crate::state::ShardState) -> u64 {
      self.begun.as_ref().map_or(0, |begun| {
        state
          .image_encoder
          .progress()
          .0
          .saturating_sub(begun.turned)
          + 2
      })
    }

    /// The held frees' bound (main 01a20182, 2026-11-23): the chunks the windows hold for the cut in flight at most the
    /// windows a turn's changes free, times the turns it lasts.
    fn check_held(&mut self, state: &crate::state::ShardState, step: usize) {
      let held = crate::vcs_store::chunk_windows(&state.vcs_stores).map_or(
        0,
        crate::vcs_store::arena_windows::ChunkWindows::held_for_cut,
      );
      self.held_most = self.held_most.max(held);
      let turns = self.turns_lasted(state);
      let bound = self.most_a_turn.1.max(self.since_turn.1) * turns;
      if held as u64 > bound {
        self.found.push(format!(
          "step {step}: {held} chunks held for the cut, past {bound} (a turn's frees times its {turns} turns)"
        ));
      }
    }

    /// A cut carried (F2): its pre-images within E + I, old values at most the entries in its pages at its moment and
    /// absent markers at most a turn's inserts times its pages.
    fn check_carried(&mut self, state: &crate::state::ShardState, begun: &Begun, step: usize) {
      let old =
        Self::counted(state, crate::content_holder::IMAGE_CUT_PREIMAGES_OLD) - begun.counted.0;
      let absent =
        Self::counted(state, crate::content_holder::IMAGE_CUT_PREIMAGES_ABSENT) - begun.counted.1;
      if old > begun.in_pages {
        self.found.push(format!(
          "step {step}: {old} old values kept, past the {} entries in the cut's pages (E)",
          begun.in_pages
        ));
      }
      let turns = state
        .image_encoder
        .progress()
        .0
        .saturating_sub(begun.turned)
        + 1;
      let inserts = self.most_a_turn.0.max(self.since_turn.0) * turns;
      if absent > inserts {
        self.found.push(format!(
          "step {step}: {absent} absent markers kept, past {inserts} (I: a turn's inserts times {turns} turns, of {} pages)",
          begun.pages
        ));
      }
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

    /// A publication (`verbs::publish_shard`), noting the cut where it carried the encoder's (F2: the shard's entries at
    /// the cut's moment, or now where it began and was carried in this one publication), then the checks.
    fn publish(&mut self, state: &mut crate::state::ShardState, step: usize) {
      let skipped = Self::counted(state, crate::content_holder::IMAGE_SKIPPED);
      let in_flight = state
        .image_encoder
        .cut_in_flight()
        .map(|(generation, _)| generation);
      self.turning();
      let now_before = crate::content_holder::paged_entries_now(state);
      if crate::verbs::publish_shard(state).is_err() {
        self
          .found
          .push(format!("step {step}: the publication refused"));
        return;
      }
      if Self::counted(state, crate::content_holder::IMAGE_SKIPPED) == skipped {
        let begun = self.begun.take();
        match begun {
          Some(begun) if Some(begun.generation) == in_flight => {
            self.check_carried(state, &begun, step);
            self.cut = begun.entries;
          }
          // Begun and carried in this one publication, or none in flight: the shard's entries as it began.
          _ => self.cut = now_before,
        }
      }
      self.note_cut(state);
      self.check_held(state, step);
      self.check(state, step);
    }

    /// `count` turns of the encoder as the catch-up task runs them, publishing once caught up where one is owed.
    fn turns(&mut self, state: &mut crate::state::ShardState, (step, count): (usize, u64)) {
      for _ in 0..count {
        self.turning();
        let turned = crate::content_holder::image_turn(state);
        self.note_cut(state);
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
      let counters = |state: &crate::state::ShardState| {
        state
          .vcs_stores
          .content()
          .map(slates_vcs::ContentStore::counters)
          .unwrap_or_default()
      };
      let before = counters(state);
      self.make(state, (kind, count));
      let after = counters(state);
      // Each put inserts a window or list and at most a name; a let-go a window let go; a bring-back a window; a
      // let-go-and-note a landed record a window. A window freed or let go frees at most its chunk.
      let landed = if kind == 1 { count as u64 } else { 0 };
      self.since_turn.0 += 2 * (after.stored - before.stored)
        + (after.let_go - before.let_go)
        + (after.brought_back - before.brought_back)
        + landed;
      self.since_turn.1 += (after.freed - before.freed) + (after.let_go - before.let_go);
      self.check_held(state, 0);
    }

    /// The change itself ([`Oracle::change`]).
    fn make(&mut self, state: &mut crate::state::ShardState, (kind, count): (u64, usize)) {
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

  /// What one oracle history found, and its non-vacuity counts.
  struct Found {
    /// What its checks found.
    found: Vec<String>,
    /// The most windows a restored image named.
    named: usize,
    /// The old values and absent markers the cuts kept.
    preimages: (u64, u64),
    /// The most chunks the windows held for one cut.
    held_most: usize,
  }

  /// One oracle history on a shard (the oracle's test docs): `seed` 0 the continuous one.
  fn history(state: &mut crate::state::ShardState, seed: u64) -> Found {
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
    // F2: the cut in flight first, then the next of the pages left, each at most doubled by a split.
    crate::content_holder::image_drain(state);
    let left = state.image_encoder.progress().1 as u64;
    oracle.turns(state, (STEPS + 1, 2 * left + 2));
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
    for bug in [
      crate::vcs_store::homes::HOME_TICKET_STRANDED,
      crate::content_holder::IMAGE_CUT_LEFT,
    ] {
      if counted(bug) > 0 {
        oracle
          .found
          .push(format!("{bug} moved: {}, a bug signal", counted(bug)));
      }
    }
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
    Found {
      found: oracle.found,
      named: oracle.named,
      preimages: (
        counted(crate::content_holder::IMAGE_CUT_PREIMAGES_OLD),
        counted(crate::content_holder::IMAGE_CUT_PREIMAGES_ABSENT),
      ),
      held_most: oracle.held_most,
    }
  }

  /// A-870 piece 2's oracle (main 01a20182's rulings of 2026-11-23, F2 among them). Do: on shards whose pages are small
  /// ([`crate::content_holder::TEST_PAGE_BYTES`]), run [`HISTORIES`] generated histories of puts, let-gos, bring-backs,
  /// encoder turns (as the catch-up task runs them), publications and crashes (a restart's read of the committed
  /// journal), the first continuous (a change, a turn and a publication every step); the history makes no let-go while
  /// the encoder pauses them, as the shard's let-go sites hold theirs. Expect, at every publication and crash: the image a
  /// restart reads equal, entry for entry, to the shard's entries at the moment the last carried cut began (one moment,
  /// never a mix of two, whatever changed while it was encoded); no window that image names with its chunk's block held
  /// now by another window (the arena reused none); the owed publication's lag within its bound; each carried cut's old
  /// values at most the entries in its pages at its moment (E) and its absent markers at most a turn's inserts times its
  /// pages (I); the chunks held for a cut at most a turn's frees times its pages; no ticket stranded and no pre-image left
  /// at a cut's end; and, the changes stopped, the cut caught up with the shard. Non-vacuous: the cuts kept old values
  /// and absent markers, the windows held a chunk for a cut, and the images named windows.
  #[test]
  fn generated_histories_restore_one_committed_cut_reuse_no_named_block_and_keep_their_lag_bound() {
    const PAGE: usize = 4096;
    let (mut old, mut absent, mut held, mut named) = (0, 0, 0, 0);
    for seed in 0..HISTORIES {
      let found = crate::daemon::audit_on_shard(move |state| {
        crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(PAGE));
        let outcome = history(state, seed);
        crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(0));
        outcome
      });
      assert!(found.found.is_empty(), "history {seed}: {:#?}", found.found);
      old += found.preimages.0;
      absent += found.preimages.1;
      held = held.max(found.held_most);
      named = named.max(found.named);
    }
    assert!(
      old > 0 && absent > 0,
      "the cuts kept {old} old values and {absent} absent markers"
    );
    assert!(held > 0, "no chunk was held for a cut");
    assert!(named > 0, "no restored image named a window");
  }
}
