//! Kernel-coherence delivery (§4.6 "Cache posture"; AUD-02): the discipline by which a transport
//! whose kernel caches names and attributes **forever** keeps that cache true. The seam reports every
//! invalidation owed since a cursor ([`Bridge::invalidations`]: a change through another attachment
//! or the SDK, an outsider's change beneath a base directory); this module turns that into **rounds**
//! the transport runs at every wake — a kernel request *or* a change signalled by another mutation
//! source — and advances the cursor only past what the kernel was actually told. The sink is a
//! closure, so the rules are pure, cfg-free and unit-tested on every host; the Linux transport
//! (`crate::channel`) runs them against a real mount.
//!
//! The two losses this closes (AUD-02, `docs/bugs/2026-09-14_AUDIT.md`): a seam refusal to gather
//! left the cursor where it was, but the advance after the request then skipped the ungathered
//! changes for good; and delivery ran only before a kernel request, so a change while the kernel
//! answered from its cache was never told — the "infinite cache lifetimes require proven invalidation
//! delivery" rule of §4.6 was advertised, not held.

use slates_bridge_core::{Bridge, Invalidation, InvalidationCursor, OpContext};

/// What one delivery round did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delivered {
  /// The invalidations written to the kernel this round.
  pub written: usize,
  /// The seam refused to gather: nothing was written, the cursor stayed, and the round is owed
  /// again at the next wake.
  pub gather_refused: bool,
}

/// A transport's coherence state: where its kernel's cache stands, and what it is owed.
#[derive(Debug, Default)]
pub struct Coherence {
  /// Where the kernel's cache stands: nothing before the mount's first round can be in it, so the
  /// first round starts at the seam's cursor of that moment.
  cursor: Option<InvalidationCursor>,
  /// The invalidations gathered for the round in progress — one allocation reused, emptied every
  /// round; bounded by what one gather reports (the journal's retention budget and the entries
  /// loaded under the hinted directories, [`Bridge::invalidations`]).
  owed: Vec<Invalidation>,
  /// Gathers the seam refused so far — the counter a transport reports, and the non-vacuity of the
  /// retry: a missed change is delivered only if a refusal was actually retried.
  gather_refusals: u64,
}

impl Coherence {
  /// A transport's coherence state at mount time.
  pub fn new() -> Coherence {
    Coherence::default()
  }

  /// One round: gathers every invalidation owed since the cursor and hands each to `sink`, then
  /// advances the cursor — only past what was gathered **and** written. A seam refusal to gather
  /// leaves the cursor where it was (the round is owed again at the next wake, counted); a sink
  /// refusal is the transport's own and is returned, with the cursor unmoved, so a re-established
  /// transport gathers the same round again (an invalidation delivered twice is harmless — the
  /// kernel drops nothing it does not hold).
  pub fn deliver<E>(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    sink: &mut dyn FnMut(&Invalidation) -> Result<(), E>,
  ) -> Result<Delivered, E> {
    let since = self.cursor.unwrap_or_else(|| bridge.seen(cx));
    self.owed.clear();
    let next = match bridge.invalidations(cx, since, &mut self.owed) {
      Ok(next) => next,
      Err(_) => {
        self.owed.clear();
        self.gather_refusals = self.gather_refusals.saturating_add(1);
        self.cursor = Some(since);
        return Ok(Delivered {
          written: 0,
          gather_refused: true,
        });
      }
    };
    let mut written: usize = 0;
    for invalidation in &self.owed {
      sink(invalidation)?;
      written = written.saturating_add(1);
    }
    self.owed.clear();
    self.cursor = Some(next);
    Ok(Delivered {
      written,
      gather_refused: false,
    })
  }

  /// The transport served a kernel request of its own after the round `delivered`: the request's
  /// records are this kernel's own doing, so the cursor moves past them — but only when that round
  /// was delivered whole. After a refused gather the changes the round owed lie before the
  /// request's records and are still owed, so the cursor stays; the next round delivers them, and
  /// the kernel's own change with them (a redundant invalidation of what it did itself, harmless).
  pub fn served_own_request(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    delivered: Delivered,
  ) {
    if !delivered.gather_refused {
      self.cursor = Some(bridge.seen(cx));
    }
  }

  /// Moves the cursor back to `cursor`, so the next round delivers what changed since, the transport's own requests
  /// included. A request whose effect was applied but whose reply was replaced by an error (a refused barrier's
  /// `EIO`) left the kernel believing the change did not happen: a cached negative name, or old attributes. Only a
  /// cursor of an earlier round moves it; `None` leaves it.
  pub fn rewind(&mut self, cursor: Option<InvalidationCursor>) {
    if cursor.is_some() {
      self.cursor = cursor;
    }
  }

  /// Gathers the seam refused so far.
  pub fn gather_refusals(&self) -> u64 {
    self.gather_refusals
  }

  /// Where the kernel's cache stands, once a round has run.
  pub fn cursor(&self) -> Option<InvalidationCursor> {
    self.cursor
  }
}

/// The spans of journal sequences the transport's own requests took since its kernel's confirmed point (§4.6, the
/// dirty-page write-back rule; `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`).
/// Its own writes refreshed its kernel's copy of what they wrote, so a later write-back is not held stale there
/// ([`slates_bridge_core::Bridge::changed_since`]). A request is served whole on its shard between two readings of the
/// journal's head, so the records between them are its own and nobody else's.
#[derive(Debug, Default)]
pub struct OwnRecords {
  /// Ascending, disjoint, half-open spans of sequences.
  spans: Vec<std::ops::Range<u64>>,
}

impl OwnRecords {
  /// Notes that a request took the records after `before` up to `after` (nothing, when it took none), merged into the
  /// last span when they follow it. At most `capacity` spans are kept, the oldest leaving first: more than the journal
  /// retains are more than it can still be asked about, since the confirmed point then lies past retention and the
  /// answer is unknown whatever the spans say ([`slates_bridge_core::Bridge::journal_capacity`]).
  pub fn record(&mut self, before: u64, after: u64, capacity: usize) {
    if after <= before {
      return;
    }
    let span = before.saturating_add(1)..after.saturating_add(1);
    match self.spans.last_mut() {
      Some(last) if last.end == span.start => last.end = span.end,
      _ => self.spans.push(span),
    }
    let excess = self.spans.len().saturating_sub(capacity);
    self.spans.drain(..excess);
  }

  /// Forgets the spans wholly at or before `confirmed`: the kernel's cache stands past them.
  pub fn prune(&mut self, confirmed: u64) {
    let past = confirmed.saturating_add(1);
    let gone = self
      .spans
      .iter()
      .take_while(|span| span.end <= past)
      .count();
    self.spans.drain(..gone);
  }

  /// The spans, ascending.
  pub fn spans(&self) -> &[std::ops::Range<u64>] {
    &self.spans
  }
}

/// The handles this mount's kernel holds open for writing (§4.6, the dirty-page write-back rule): only a writable open
/// can dirty a page, so only its file needs fetch points ([`FetchPoints`]). Bounded by the handles the bridge's handle
/// store issues, since only a granted handle is recorded and each is forgotten at its release.
#[derive(Debug, Default)]
pub struct WritableOpens {
  /// Handle → the inode it opens.
  handles: std::collections::BTreeMap<u64, u64>,
  /// Inode → how many writable handles it has.
  per_inode: std::collections::BTreeMap<u64, usize>,
}

impl WritableOpens {
  /// Notes handle `fh` granted for writing on `ino`; whether it is the inode's first writable handle on this mount.
  pub fn opened(&mut self, fh: u64, ino: u64) -> bool {
    if self.handles.insert(fh, ino).is_some() {
      return false;
    }
    let count = self.per_inode.entry(ino).or_insert(0);
    *count = count.saturating_add(1);
    *count == 1
  }

  /// Forgets handle `fh`: the inode it was the last writable handle of, if it was.
  pub fn released(&mut self, fh: u64) -> Option<u64> {
    let ino = self.handles.remove(&fh)?;
    let count = self.per_inode.get_mut(&ino)?;
    *count = count.saturating_sub(1);
    if *count > 0 {
      return None;
    }
    self.per_inode.remove(&ino);
    Some(ino)
  }

  /// Whether `ino` has a writable handle on this mount.
  pub fn holds(&self, ino: u64) -> bool {
    self.per_inode.contains_key(&ino)
  }
}

/// When each cached page of a file open for writing was fetched, as journal sequences (§4.6, the dirty-page write-back
/// rule; `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`).
///
/// A cache write-back carries the kernel's copy of a page as it was fetched, plus the process's stores since, so its
/// bytes are stale exactly where another path changed the file after the fetch
/// ([`slates_bridge_core::Bridge::changed_since`]). Judging a write-back by its page's own fetch, not by invalidation
/// rounds, holds however late it arrives. Linux 6.12 writes a page back from a temporary copy and ends the page's
/// write-back at once (`fs/fuse/file.c` `fuse_writepage_locked`), so an invalidation passes the page without
/// laundering it while the stale copy is still queued: measured 2026-10-21, a write-back judged by rounds arrived
/// after its round had ended, in 9 runs of 10.
#[derive(Debug, Default)]
pub struct FetchPoints {
  inodes: std::collections::BTreeMap<u64, InodePoints>,
}

/// One tracked file's fetch points.
#[derive(Debug)]
struct InodePoints {
  /// The point of the file's first writable open, when the kernel dropped its cached pages: a page fetched since has
  /// its own point, and any other was written whole by this mount, so nothing in it is older than this.
  opened_at: u64,
  /// Fetched byte ranges, start → (end, point), disjoint; touching neighbours of one point are merged.
  ranges: std::collections::BTreeMap<u64, (u64, u64)>,
  /// The file needed more ranges than the bound: which point its pages were fetched at can no longer be told.
  lost: bool,
}

impl FetchPoints {
  /// The file `ino` is opened for writing for the first time on this mount, at journal point `point`, the kernel told
  /// to drop its cached pages: its pages are judged from here. A file already tracked keeps its record.
  pub fn opened(&mut self, ino: u64, point: u64) {
    self.inodes.entry(ino).or_insert_with(|| InodePoints {
      opened_at: point,
      ranges: std::collections::BTreeMap::new(),
      lost: false,
    });
  }

  /// Bytes `range` of `ino` were fetched at `point`: the kernel's copy of them is as of `point`. Nothing for a file not
  /// tracked. Past `bound` ranges the file's points are lost, and its write-backs are refused rather than guessed: more
  /// fetches between changes than the journal retains records are fetches older than the journal can be asked about.
  pub fn fetched(&mut self, ino: u64, range: std::ops::Range<u64>, point: u64, bound: usize) {
    let Some(file) = self.inodes.get_mut(&ino) else {
      return;
    };
    if file.lost || range.start >= range.end {
      return;
    }
    file.cut(range.clone());
    file.place(range, point);
    if file.ranges.len() > bound {
      file.ranges.clear();
      file.lost = true;
    }
  }

  /// The parts of `range` of `ino`, each with the point it was fetched at: a file not tracked is judged wholly from
  /// `untracked`, and a part of a tracked file never fetched from its first writable open. `None` when the file's points
  /// were lost.
  pub fn parts(
    &self,
    ino: u64,
    range: std::ops::Range<u64>,
    untracked: u64,
  ) -> Option<Vec<(std::ops::Range<u64>, u64)>> {
    let Some(file) = self.inodes.get(&ino) else {
      return Some(vec![(range, untracked)]);
    };
    if file.lost {
      return None;
    }
    let mut parts = Vec::new();
    let mut cursor = range.start;
    let reaching = file
      .ranges
      .range(..=range.start)
      .next_back()
      .filter(|(_, (end, _))| *end > range.start);
    let inside = file.ranges.range(range.start.saturating_add(1)..range.end);
    for (&start, &(end, point)) in reaching.into_iter().chain(inside) {
      if start > cursor {
        parts.push((cursor..start, file.opened_at));
      }
      let part_end = end.min(range.end);
      if start.max(cursor) < part_end {
        parts.push((start.max(cursor)..part_end, point));
      }
      cursor = cursor.max(part_end);
    }
    if cursor < range.end {
      parts.push((cursor..range.end, file.opened_at));
    }
    Some(parts)
  }

  /// The file `ino` has no writable open left on this mount: its record goes. No write-back of it can follow, since a
  /// write-back holds its file open until it is answered, so the file's release comes after.
  pub fn dropped(&mut self, ino: u64) {
    self.inodes.remove(&ino);
  }
}

impl InodePoints {
  /// Removes `range` from the fetched ranges, keeping what lies on either side of it.
  fn cut(&mut self, range: std::ops::Range<u64>) {
    if let Some((&before, &(before_end, point))) = self.ranges.range(..range.start).next_back()
      && before_end > range.start
    {
      self.ranges.insert(before, (range.start, point));
      if before_end > range.end {
        self.ranges.insert(range.end, (before_end, point));
      }
    }
    let inside: Vec<(u64, u64, u64)> = self
      .ranges
      .range(range.start..range.end)
      .map(|(&start, &(end, point))| (start, end, point))
      .collect();
    for (start, end, point) in inside {
      self.ranges.remove(&start);
      if end > range.end {
        self.ranges.insert(range.end, (end, point));
      }
    }
  }

  /// Places `range` at `point` where nothing is, merging a touching neighbour of the same point on either side.
  fn place(&mut self, range: std::ops::Range<u64>, point: u64) {
    let (mut start, mut end) = (range.start, range.end);
    if let Some((&before, &(before_end, before_point))) = self.ranges.range(..start).next_back()
      && before_end == start
      && before_point == point
    {
      start = before;
      self.ranges.remove(&before);
    }
    if let Some(&(after_end, after_point)) = self.ranges.get(&end)
      && after_point == point
    {
      self.ranges.remove(&end);
      end = after_end;
    }
    self.ranges.insert(start, (end, point));
  }
}

#[cfg(test)]
mod tests {
  use super::OwnRecords;

  /// The dirty-page write-back rule's own-record spans: consecutive requests' records merge into one span, another
  /// path's record between them splits it, the confirmed point forgets what lies wholly before it, and no more spans
  /// than the journal retains are kept.
  /// The dirty-page write-back rule's writable opens: a file's first writable handle is told apart, another handle of
  /// it is not first, the last release names the file, and a handle released twice or never granted names nothing.
  #[test]
  fn writable_opens_tell_the_first_and_the_last() {
    let mut opens = super::WritableOpens::default();
    assert!(opens.opened(1, 40), "the file's first writable handle");
    assert!(
      !opens.opened(2, 40),
      "a second handle of the same file is not first"
    );
    assert!(opens.opened(3, 41), "another file's first");
    assert!(opens.holds(40) && opens.holds(41) && !opens.holds(42));
    assert_eq!(opens.released(1), None, "the file still has handle 2");
    assert_eq!(
      opens.released(2),
      Some(40),
      "its last handle names the file"
    );
    assert!(!opens.holds(40));
    assert_eq!(
      opens.released(2),
      None,
      "a handle released twice names nothing"
    );
    assert_eq!(
      opens.released(9),
      None,
      "a handle never granted names nothing"
    );
  }

  #[test]
  #[allow(clippy::single_range_in_vec_init)] // a list of one span, not the numbers inside it
  fn own_spans_merge_split_prune_and_stay_within_the_journal() {
    let mut own = OwnRecords::default();
    own.record(10, 12, 8);
    own.record(12, 13, 8);
    assert_eq!(own.spans(), &[11..14], "consecutive requests merge");
    own.record(14, 15, 8);
    assert_eq!(
      own.spans(),
      &[11..14, 15..16],
      "another path's record 14 splits them"
    );
    own.record(15, 15, 8);
    assert_eq!(
      own.spans(),
      &[11..14, 15..16],
      "a request that took no record notes nothing"
    );
    own.prune(13);
    assert_eq!(
      own.spans(),
      &[15..16],
      "everything at or before 13 is confirmed"
    );
    for at in 0..20u64 {
      own.record(20 + 2 * at, 21 + 2 * at, 3);
    }
    assert_eq!(
      own.spans().len(),
      3,
      "no more spans than the journal can still be asked about"
    );
    assert_eq!(own.spans().last(), Some(&(59..60)), "the newest stay");
  }

  mod fetch_points {
    // proptest's strategy types carry `Arc` (D-8's harness exception).
    #![allow(clippy::disallowed_types)]
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use proptest::prelude::*;

    use super::super::FetchPoints;

    /// Shape: the bytes of the one file the model tracks — past every generated offset and length.
    const MODEL_BYTES: u64 = 96;
    /// Shape: the inode the history works on.
    const INO: u64 = 7;
    /// Shape: the point the file's first writable open is at.
    const OPENED_AT: u64 = 3;

    /// One generated fetch: a range and the point it was fetched at.
    fn fetch() -> impl Strategy<Value = (u64, u64, u64)> {
      (0..80u64, 1..24u64, 4..40u64)
    }

    proptest! {
      #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
        cases: slates_test_seeds::cases(2000),
        .. ProptestConfig::default()
      }))]

      /// The dirty-page write-back rule's fetch points, against a per-byte model of the last fetch of each byte: after
      /// any history of fetches, the parts of any range cover it in order, without a gap or an overlap, and every byte's
      /// part carries that byte's last fetch point, or the open's point for a byte never fetched.
      #[test]
      fn parts_carry_each_bytes_last_fetch(
        fetches in prop::collection::vec(fetch(), 0..16),
        query in (0..MODEL_BYTES, 1..MODEL_BYTES),
      ) {
        let mut points = FetchPoints::default();
        points.opened(INO, OPENED_AT);
        let mut model = vec![OPENED_AT; usize::try_from(MODEL_BYTES).unwrap()];
        for (at, len, point) in &fetches {
          let end = (at + len).min(MODEL_BYTES);
          points.fetched(INO, *at..end, *point, usize::MAX);
          for byte in *at..end {
            model[usize::try_from(byte).unwrap()] = *point;
          }
        }
        let (from, len) = query;
        let to = (from + len).min(MODEL_BYTES);
        let parts = points.parts(INO, from..to, 0).unwrap();
        let mut cursor = from;
        for (range, point) in &parts {
          prop_assert_eq!(range.start, cursor, "the parts are contiguous: {:?}", parts);
          prop_assert!(range.start < range.end);
          for byte in range.clone() {
            prop_assert_eq!(*point, model[usize::try_from(byte).unwrap()], "byte {} in {:?}", byte, parts);
          }
          cursor = range.end;
        }
        prop_assert_eq!(cursor, to, "the parts cover the range: {:?}", parts);
      }
    }

    /// A file not tracked is judged wholly from the point the caller gives; a file whose ranges passed the bound is
    /// refused, not guessed; and a dropped file is untracked again.
    #[test]
    fn untracked_lost_and_dropped_files() {
      let mut points = FetchPoints::default();
      assert_eq!(
        points.parts(INO, 0..10, 2),
        Some(vec![(0..10, 2)]),
        "untracked"
      );
      points.opened(INO, OPENED_AT);
      points.fetched(INO, 0..4, 5, 2);
      points.fetched(INO, 8..12, 6, 2);
      assert!(
        points.parts(INO, 0..12, 2).is_some(),
        "two ranges are within a bound of two"
      );
      points.fetched(INO, 20..24, 7, 2);
      assert_eq!(
        points.parts(INO, 0..12, 2),
        None,
        "a third range passes the bound: lost"
      );
      points.dropped(INO);
      assert_eq!(
        points.parts(INO, 0..12, 2),
        Some(vec![(0..12, 2)]),
        "dropped: untracked again"
      );
    }
  }
}
