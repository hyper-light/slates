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

/// A family of a shard image's paged entries: each keeps its own pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Family {
  /// The repository windows kept as chunks, by handle.
  Windows,
  /// The content store's lists, by identity.
  Lists,
  /// The content store's names, by identity.
  Names,
  /// The content store's windows let go, by identity.
  Aways,
  /// The objects this shard holds for the region, by identity.
  RegionHolds,
  /// Homes' landed records, by identity.
  Landed,
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

  /// Format: its byte in a page piece's key.
  pub(crate) fn byte(self) -> u8 {
    match self {
      Family::Windows => 1,
      Family::Lists => 2,
      Family::Names => 3,
      Family::Aways => 4,
      Family::RegionHolds => 5,
      Family::Landed => 6,
    }
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
pub(crate) type Entry = (
  u64,
  [u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
  Vec<u8>,
);

/// Format: the bytes a page piece spends on an entry beyond its bytes: its key and its length.
const ENTRY_HEAD: usize = slates_vfs::recover::HELD_PIECE_KEY_BYTES + size_of::<u32>();

/// The bytes `entries` take in a page piece: what a page past twice the page size is split by.
pub(crate) fn entries_bytes(entries: &[Entry]) -> usize {
  bytes_of(entries)
}

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

#[cfg(test)]
mod tests {
  use super::*;

  /// An entry at `coordinate` of `len` bytes.
  fn entry(coordinate: u64, len: usize) -> Entry {
    let mut key = [0u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES];
    key[1..9].copy_from_slice(&coordinate.to_be_bytes());
    (coordinate, key, vec![7u8; len])
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

  /// A-870 piece 1 (main 01a20182's rulings of 2026-11-22 ~22:10 and ~22:30; arena-4: 138,989 windows let go and as
  /// many landed records, one image piece each). Do: on a shard, let [`ENTRIES`] one-window objects go landed and note a
  /// landed record for each; take the shard's checkpoint image; read it back as a restore does. Expect: the image's
  /// pieces far fewer than its entries (a tenth at most), and every window let go and every record read back.
  /// Failing first: one piece an entry, twice [`ENTRIES`] pieces.
  #[test]
  fn a_checkpoint_carries_many_entries_in_few_pieces_and_restores_them_whole() {
    let (pieces, aways, landed) = crate::daemon::audit_on_shard(|state| {
      let _ = let_go_and_note(state, (0, ENTRIES));
      let image = crate::content_holder::held_image(state);
      let held = crate::content_holder::split_held(&image).unwrap();
      (image.len(), held.aways.len(), held.landed.len())
    });
    assert!(
      pieces <= ENTRIES / 10,
      "{pieces} pieces carry {} entries",
      2 * ENTRIES
    );
    assert_eq!((aways, landed), (ENTRIES, ENTRIES), "every entry read back");
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
      let ids = let_go_and_note(state, (0, ENTRIES));
      let (image, pages) = crate::content_holder::held_image_paged(state);
      let mut journal: BTreeMap<_, _> = image
        .iter()
        .map(|piece| (piece.key, piece.bytes.clone()))
        .collect();
      crate::content_holder::held_committed(state, crate::content_holder::whole_of(&image), true);
      crate::content_holder::pages_committed(state, pages);
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
        let changes = crate::content_holder::held_changes(state);
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
        crate::content_holder::pages_committed(state, changes.pages);
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
}
