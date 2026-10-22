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

/// The spans of journal sequences the transport's own requests took (§4.6, the
/// dirty-page write-back rule; `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`).
/// Its own writes refreshed its kernel's copy of what they wrote, so a later write-back is not held stale there
/// ([`PageStates`]). A request is served whole on its shard between two readings of the journal's head, so the records
/// between them are its own and nobody else's.
#[derive(Debug, Default)]
pub struct OwnRecords {
  /// Ascending, disjoint, half-open spans of sequences.
  spans: Vec<std::ops::Range<u64>>,
}

impl OwnRecords {
  /// Notes that a request took the records after `before` up to `after` (nothing, when it took none), merged into the
  /// last span when they follow it. At most `capacity` spans are kept, the oldest leaving first: each names at least one
  /// record, so older spans name records the journal no longer holds, and a walk that would need them is refused
  /// whatever the spans say ([`slates_bridge_core::Bridge::journal_capacity`]).
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

  /// Whether record `seq` is one this transport's own request took: a binary search, since the spans ascend.
  pub fn took(&self, seq: u64) -> bool {
    let after = self.spans.partition_point(|span| span.end <= seq);
    self
      .spans
      .get(after)
      .is_some_and(|span| span.contains(&seq))
  }
}

/// The handles this mount's kernel holds open for writing (§4.6, the dirty-page write-back rule): only a writable open
/// can dirty a page, so only its file needs byte states ([`PageStates`]). Bounded by the handles the bridge's handle
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

/// What this mount's kernel holds of each file open for writing (§4.6, the dirty-page write-back rule;
/// `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`): for every byte, the
/// journal point its cached copy is as of, and whether another path changed it since.
///
/// A cache write-back carries the kernel's copy of a page as fetched, plus the process's stores since, so its bytes are
/// stale exactly where another path changed the file after the fetch. The states are fed as the mount consumes the
/// journal — the records its invalidation rounds already walk ([`slates_bridge_core::Bridge::data_changes`]) — so a fetch
/// of any age is judged:
/// - **a fetch** at head `h` makes its bytes (`h`, fresh);
/// - **the mount's own record** at `s` makes bytes with an older point (`s`, fresh), since its own write refreshed them;
/// - **another path's record** at `s` makes bytes with an older point stale.
///
/// A fetch and an own write may reset bytes because the kernel orders them after every write-back of their pages still
/// in flight. Linux 6.12 keeps each write-back in the inode's tree until the daemon answers it (`fs/fuse/file.c:2124`,
/// removed at `:1931`), and a cache read (`:872`, readahead page by page at `:1020`) and a write (`:1141`) wait on that
/// tree before they are sent. So no older copy of those pages can arrive after them. Nothing else resets a byte. The
/// end of an invalidation round does not: a write-back copied out before it is still in flight after it.
///
/// Once the mount has consumed every record through its cursor ([`PageStates::consumed`]), the points at or below it can
/// no longer be told apart by any record to come, so they are raised to it and their ranges merge. What remains is one
/// range per run of stale or fresh bytes. Without the raise, a long-lived map faulted in a page at a time while the
/// volume is busy keeps one range per page and passes the bound.
///
/// Two designs before this were measured and rejected (the bug record). Judged by invalidation rounds, a write-back
/// failed 9 runs of 10, since Linux 6.12 writes back from a temporary copy an invalidation passes. Judged by walking the
/// journal back from each page's fetch, a long-lived map outlived the journal's retention and its write-backs were
/// refused, 2 runs of 3.
#[derive(Debug, Default)]
pub struct PageStates {
  files: std::collections::BTreeMap<u64, FileStates>,
  /// The files changed since the last [`PageStates::consumed`], each listed once: only their points can need raising.
  /// Bounded by the tracked files.
  touched: Vec<u64>,
}

/// One tracked file's byte states.
#[derive(Debug, Clone)]
struct FileStates {
  /// Start → (end, point, stale), disjoint and covering every byte; touching neighbours of one state are merged.
  ranges: std::collections::BTreeMap<u64, (u64, u64, bool)>,
  /// The file needed more ranges than the bound: what its kernel holds can no longer be told.
  lost: bool,
  /// Listed in [`PageStates::touched`].
  touched: bool,
}

impl PageStates {
  /// The file `ino` is opened for writing for the first time on this mount, at journal point `point`, the kernel told
  /// to drop its cached pages: every byte is fresh as of `point`. A file already tracked keeps its states.
  pub fn opened(&mut self, ino: u64, point: u64) {
    self
      .files
      .entry(ino)
      .or_insert_with(|| FileStates::fresh(point));
  }

  /// Whether `ino` is tracked.
  pub fn tracks(&self, ino: u64) -> bool {
    self.files.contains_key(&ino)
  }

  /// Bytes `range` of `ino` were fetched at head `point`: fresh as of it. Nothing for a file not tracked.
  pub fn fetched(&mut self, ino: u64, range: std::ops::Range<u64>, point: u64, bound: usize) {
    if let Some(file) = self.files.get_mut(&ino) {
      file.update(range, bound, |_, _| (point, false));
      file.touch(ino, &mut self.touched);
    }
  }

  /// A journal change to `ino`'s bytes `range` at sequence `seq`: the mount's own (`own`), which refreshes the bytes
  /// fetched before it, or another path's, which leaves them stale. Nothing for a file not tracked. Applying one twice is
  /// harmless: a refresh only raises a point, and a byte already past `seq` is untouched.
  pub fn changed(
    &mut self,
    ino: u64,
    range: std::ops::Range<u64>,
    seq: u64,
    own: bool,
    bound: usize,
  ) {
    if let Some(file) = self.files.get_mut(&ino) {
      file.change(range, seq, own, bound);
      file.touch(ino, &mut self.touched);
    }
  }

  /// The mount has consumed every journal record through `through`: each was applied with [`PageStates::changed`]. Any
  /// record still to come is later, so the points at or below `through` can no longer be told apart; they are raised to
  /// it, and the ranges they split merge. Only the files changed since the last call are walked, since the others hold
  /// one point already.
  pub fn consumed(&mut self, through: u64) {
    for ino in self.touched.drain(..) {
      if let Some(file) = self.files.get_mut(&ino) {
        file.touched = false;
        file.raise_points(through);
      }
    }
  }

  /// The stale bytes of `range` of `ino`, after the not yet consumed `pending` changes to it (each with whether it is the
  /// mount's own) applied in order to a scratch copy: ascending and disjoint. A file not tracked is judged as fresh at
  /// `untracked`. `None` when the file's states were lost.
  pub fn stale_after(
    &self,
    ino: u64,
    range: std::ops::Range<u64>,
    untracked: u64,
    pending: &[(std::ops::Range<u64>, u64, bool)],
  ) -> Option<Vec<std::ops::Range<u64>>> {
    let mut scratch = match self.files.get(&ino) {
      Some(file) if file.lost => return None,
      Some(file) => file.clipped(range.clone()),
      None => FileStates::fresh(untracked).clipped(range.clone()),
    };
    for (changed, seq, own) in pending {
      scratch.change(changed.clone(), *seq, *own, usize::MAX);
    }
    Some(scratch.stale_within(range))
  }

  /// Every tracked file is lost: the journal no longer holds the records since this mount last consumed it, so what its
  /// kernel holds can no longer be told. Their write-backs are refused until their last writable release.
  pub fn lose_all(&mut self) {
    for file in self.files.values_mut() {
      file.ranges.clear();
      file.lost = true;
      file.touched = false;
    }
    self.touched.clear();
  }

  /// The file `ino` has no writable open left on this mount: its states go. No write-back of it can follow, since a
  /// write-back holds its file open until it is answered, so the file's release comes after.
  pub fn dropped(&mut self, ino: u64) {
    if self.files.remove(&ino).is_some_and(|file| file.touched) {
      self.touched.retain(|&listed| listed != ino);
    }
  }
}

impl FileStates {
  /// Every byte fresh as of `point`.
  fn fresh(point: u64) -> FileStates {
    let mut ranges = std::collections::BTreeMap::new();
    ranges.insert(0, (u64::MAX, point, false));
    FileStates {
      ranges,
      lost: false,
      touched: false,
    }
  }

  /// Lists the file in `touched` the first time it changes after a [`PageStates::consumed`].
  fn touch(&mut self, ino: u64, touched: &mut Vec<u64>) {
    if !self.touched && !self.lost {
      self.touched = true;
      touched.push(ino);
    }
  }

  /// Raises every point below `floor` to it and merges the touching ranges that then hold one state.
  fn raise_points(&mut self, floor: u64) {
    let mut raised: std::collections::BTreeMap<u64, (u64, u64, bool)> =
      std::collections::BTreeMap::new();
    let mut run: Option<(u64, (u64, u64, bool))> = None;
    for (&start, &(end, point, stale)) in &self.ranges {
      let state = (end, point.max(floor), stale);
      run = match run {
        Some((run_start, (run_end, run_point, run_stale)))
          if run_end == start && run_point == state.1 && run_stale == stale =>
        {
          Some((run_start, (end, run_point, run_stale)))
        }
        Some((run_start, run_state)) => {
          raised.insert(run_start, run_state);
          Some((start, state))
        }
        None => Some((start, state)),
      };
    }
    if let Some((run_start, run_state)) = run {
      raised.insert(run_start, run_state);
    }
    self.ranges = raised;
  }

  /// A journal change at `seq`: bytes in `range` with an older point are refreshed (`own`) or left stale.
  fn change(&mut self, range: std::ops::Range<u64>, seq: u64, own: bool, bound: usize) {
    self.update(range, bound, |point, stale| {
      if point >= seq {
        (point, stale)
      } else if own {
        (seq, false)
      } else {
        (point, true)
      }
    });
  }

  /// Gives every byte of `range` the state `f` makes of its current one, then merges touching neighbours of one state.
  /// Past `bound` ranges the file is lost.
  fn update(
    &mut self,
    range: std::ops::Range<u64>,
    bound: usize,
    f: impl Fn(u64, bool) -> (u64, bool),
  ) {
    if self.lost || range.start >= range.end {
      return;
    }
    self.split_at(range.start);
    self.split_at(range.end);
    let inside: Vec<(u64, (u64, u64, bool))> = self
      .ranges
      .range(range.start..range.end)
      .map(|(&start, &state)| (start, state))
      .collect();
    for (start, (end, point, stale)) in inside {
      let (point, stale) = f(point, stale);
      self.ranges.insert(start, (end, point, stale));
    }
    self.merge_around(range);
    if self.ranges.len() > bound {
      self.ranges.clear();
      self.lost = true;
    }
  }

  /// Splits the range containing `at` so one starts there.
  fn split_at(&mut self, at: u64) {
    if let Some((&start, &(end, point, stale))) = self.ranges.range(..=at).next_back()
      && start < at
      && at < end
    {
      self.ranges.insert(start, (at, point, stale));
      self.ranges.insert(at, (end, point, stale));
    }
  }

  /// Merges touching neighbours of one state from just before `range` to just past it.
  fn merge_around(&mut self, range: std::ops::Range<u64>) {
    let first = self
      .ranges
      .range(..range.start)
      .next_back()
      .map_or(range.start, |(&start, _)| start);
    let starts: Vec<u64> = self
      .ranges
      .range(first..=range.end)
      .map(|(&start, _)| start)
      .collect();
    for start in starts {
      let Some(&(end, point, stale)) = self.ranges.get(&start) else {
        continue;
      };
      if let Some(&(next_end, next_point, next_stale)) = self.ranges.get(&end)
        && next_point == point
        && next_stale == stale
      {
        self.ranges.remove(&end);
        self.ranges.insert(start, (next_end, point, stale));
      }
    }
  }

  /// A copy of the states over `range` alone.
  fn clipped(&self, range: std::ops::Range<u64>) -> FileStates {
    let mut ranges = std::collections::BTreeMap::new();
    let reaching = self
      .ranges
      .range(..=range.start)
      .next_back()
      .filter(|(_, (end, _, _))| *end > range.start);
    let inside = self.ranges.range(range.start.saturating_add(1)..range.end);
    for (&start, &(end, point, stale)) in reaching.into_iter().chain(inside) {
      let (start, end) = (start.max(range.start), end.min(range.end));
      if start < end {
        ranges.insert(start, (end, point, stale));
      }
    }
    FileStates {
      ranges,
      lost: self.lost,
      touched: false,
    }
  }

  /// The stale bytes within `range`, ascending and disjoint.
  fn stale_within(&self, range: std::ops::Range<u64>) -> Vec<std::ops::Range<u64>> {
    let mut stale: Vec<std::ops::Range<u64>> = Vec::new();
    for (&start, &(end, _, is_stale)) in &self.ranges {
      let (start, end) = (start.max(range.start), end.min(range.end));
      if !is_stale || start >= end {
        continue;
      }
      match stale.last_mut() {
        Some(last) if last.end == start => last.end = end,
        _ => stale.push(start..end),
      }
    }
    stale
  }
}

#[cfg(test)]
mod tests {
  use super::OwnRecords;

  /// The dirty-page write-back rule's own records, told by sequence. Do: note two consecutive requests (records 11 to
  /// 13), then one after another path's record 14 (record 15), then a request that took nothing; then twenty more
  /// requests, one record each, with room for three spans. Expect: exactly 11, 12, 13 and 15 told as the transport's own
  /// at first, and not 14; at the end only the newest three requests' records, the older forgotten — no more than the
  /// journal can still be asked about.
  #[test]
  fn own_records_are_told_by_sequence_within_the_journals_capacity() {
    let mut own = OwnRecords::default();
    own.record(10, 12, 8);
    own.record(12, 13, 8);
    own.record(14, 15, 8);
    own.record(15, 15, 8);
    let told: Vec<u64> = (9..18).filter(|&seq| own.took(seq)).collect();
    assert_eq!(
      told,
      vec![11, 12, 13, 15],
      "its own requests' records, and not another path's 14"
    );
    for at in 0..20u64 {
      own.record(20 + 2 * at, 21 + 2 * at, 3);
    }
    let told: Vec<u64> = (0..64).filter(|&seq| own.took(seq)).collect();
    assert_eq!(
      told,
      vec![55, 57, 59],
      "the newest three requests' records; the older forgotten"
    );
  }

  mod page_states {
    // proptest's strategy types carry `Arc` (D-8's harness exception).
    #![allow(clippy::disallowed_types)]
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use proptest::prelude::*;

    use super::super::PageStates;

    /// Shape: the bytes of the one file the model tracks — past every generated offset and length.
    const MODEL_BYTES: u64 = 96;
    /// Shape: the inode the history works on.
    const INO: u64 = 7;
    /// Shape: the point the file's first writable open is at.
    const OPENED_AT: u64 = 3;

    /// One generated event: a fetch at the head, the mount's own record, another path's record (each with its range), or
    /// the mount consuming the journal through the head; and how far the journal moved before it (records of other
    /// files).
    #[derive(Clone, Debug)]
    enum Event {
      Fetch(u64, u64),
      Own(u64, u64),
      Other(u64, u64),
      Consume,
    }

    fn event() -> impl Strategy<Value = (Event, u64)> {
      (0..4u8, 0..80u64, 1..24u64, 0..3u64).prop_map(|(kind, at, len, gap)| {
        let event = match kind {
          0 => Event::Fetch(at, at + len),
          1 => Event::Own(at, at + len),
          2 => Event::Other(at, at + len),
          _ => Event::Consume,
        };
        (event, gap)
      })
    }

    /// The model's rule for one byte, as the design states it: what `event` at `at` (the head, for a fetch; the record's
    /// sequence, otherwise) makes of the byte's (point, stale).
    fn rule(event: &Event, (point, stale): (u64, bool), at: u64) -> (u64, bool) {
      match event {
        Event::Fetch(..) => (at, false),
        Event::Own(..) if point < at => (at, false),
        Event::Other(..) if point < at => (point, true),
        Event::Own(..) | Event::Other(..) | Event::Consume => (point, stale),
      }
    }

    proptest! {
      #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
        cases: slates_test_seeds::cases(2000),
        .. ProptestConfig::default()
      }))]

      /// The dirty-page write-back rule's byte states, against a per-byte model of (point, stale) stated as the design's
      /// rule: a fetch at the head makes its bytes fresh at the head; a record at sequence `s` (the journal moves on by
      /// one) refreshes the bytes fetched before it when it is the mount's own and leaves them stale when it is another
      /// path's. The model never raises a point; the states raise them whenever the mount has consumed every record so
      /// far, which must change no answer. After any history, the stale bytes of any range — with the last few events
      /// held back as pending and applied to a scratch copy — equal the model's.
      #[test]
      fn stale_bytes_follow_the_rule(
        events in prop::collection::vec(event(), 0..20),
        held_back in 0..4usize,
        query in (0..MODEL_BYTES, 1..MODEL_BYTES),
      ) {
        let mut states = PageStates::default();
        states.opened(INO, OPENED_AT);
        let mut model = vec![(OPENED_AT, false); usize::try_from(MODEL_BYTES).unwrap()];
        let mut head = OPENED_AT;
        let split = events.len().saturating_sub(held_back);
        let mut pending = Vec::new();
        for (index, (event, gap)) in events.iter().enumerate() {
          head += gap;
          let (from, to) = match event {
            Event::Fetch(from, to) | Event::Own(from, to) | Event::Other(from, to) => (*from, (*to).min(MODEL_BYTES)),
            // Only while nothing is held back: a pending record lies at or below the head and is not consumed yet.
            Event::Consume => {
              if index < split {
                states.consumed(head);
              }
              continue;
            }
          };
          if !matches!(event, Event::Fetch(..)) {
            head += 1;
          }
          for byte in from..to {
            let cell = &mut model[usize::try_from(byte).unwrap()];
            *cell = rule(event, *cell, head);
          }
          match event {
            Event::Fetch(..) => states.fetched(INO, from..to, head, usize::MAX),
            Event::Own(..) | Event::Other(..) if index < split => {
              states.changed(INO, from..to, head, matches!(event, Event::Own(..)), usize::MAX);
            }
            Event::Own(..) | Event::Other(..) => {
              pending.push((from..to, head, matches!(event, Event::Own(..))));
            }
            Event::Consume => {}
          }
        }
        // A fetch held back behind pending records would be out of order: fetches are applied as they happen.
        let (from, len) = query;
        let to = (from + len).min(MODEL_BYTES);
        let stale = states.stale_after(INO, from..to, 0, &pending).unwrap();
        let mut got = vec![false; usize::try_from(MODEL_BYTES).unwrap()];
        for range in &stale {
          prop_assert!(range.start >= from && range.end <= to && range.start < range.end, "{:?}", stale);
          for byte in range.clone() {
            got[usize::try_from(byte).unwrap()] = true;
          }
        }
        for byte in from..to {
          let index = usize::try_from(byte).unwrap();
          prop_assert_eq!(got[index], model[index].1, "byte {} in {:?}", byte, stale);
        }
        prop_assert!(stale.windows(2).all(|pair| pair[0].end < pair[1].start), "{:?}", stale);
      }
    }

    /// An untracked file is judged as fresh at the point the caller gives; a file whose ranges passed the bound is
    /// refused, not guessed, and consuming the journal does not bring it back; losing every file refuses them all; a
    /// dropped file is untracked again.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // a list of one stale range, not the numbers inside it
    fn untracked_lost_and_dropped_files() {
      /// Shape: the most ranges the file may hold here.
      const BOUND: usize = 4;
      let mut states = PageStates::default();
      let other = vec![(0..4, 10, false)];
      assert_eq!(
        states.stale_after(INO, 0..8, 5, &other),
        Some(vec![0..4]),
        "untracked, fresh at 5"
      );
      states.opened(INO, OPENED_AT);
      // Two fetches at their own heads leave four ranges: fetched, as opened, fetched, as opened.
      states.fetched(INO, 0..4, 5, BOUND);
      states.fetched(INO, 8..12, 6, BOUND);
      assert!(
        states.stale_after(INO, 0..12, 0, &[]).is_some(),
        "four ranges: within the bound"
      );
      states.fetched(INO, 20..24, 7, BOUND);
      assert_eq!(
        states.stale_after(INO, 0..12, 0, &[]),
        None,
        "six ranges: past the bound, lost"
      );
      states.consumed(7);
      assert_eq!(
        states.stale_after(INO, 0..12, 0, &[]),
        None,
        "still lost: what the kernel holds was not followed"
      );
      states.dropped(INO);
      states.opened(INO, OPENED_AT);
      states.lose_all();
      assert_eq!(
        states.stale_after(INO, 0..12, 0, &[]),
        None,
        "every file lost"
      );
      states.dropped(INO);
      assert!(!states.tracks(INO), "dropped");
    }

    /// A long-lived file faulted in a page at a time while the volume is busy, as a guest's DAX window is: each fetch at
    /// a head of its own, another file's record moving the journal between them. Do: with a bound of eight ranges, fetch
    /// eighty pages one by one, the mount consuming the journal after each as its rounds do; then another path changes
    /// ten bytes of page 40. Expect: the file is never lost, and exactly the ten bytes are stale. Without the raise each
    /// fetch keeps a range of its own and the file is lost by the eighth.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // a list of one stale range, not the numbers inside it
    fn pages_fetched_one_by_one_under_activity_stay_within_the_bound() {
      /// Shape: the most ranges the file may hold here.
      const BOUND: usize = 8;
      /// Shape: a page.
      const PAGE: u64 = 4096;
      /// Shape: ten times the bound's pages.
      const PAGES: u64 = 80;
      let mut states = PageStates::default();
      states.opened(INO, OPENED_AT);
      let mut head = OPENED_AT;
      for page in 0..PAGES {
        head += 1;
        states.fetched(INO, page * PAGE..(page + 1) * PAGE, head, BOUND);
        states.consumed(head);
        assert!(
          states.stale_after(INO, 0..PAGES * PAGE, 0, &[]).is_some(),
          "lost at page {page}"
        );
      }
      head += 1;
      let changed = 40 * PAGE + 10..40 * PAGE + 20;
      states.changed(INO, changed.clone(), head, false, BOUND);
      states.consumed(head);
      assert_eq!(
        states.stale_after(INO, 0..PAGES * PAGE, 0, &[]),
        Some(vec![changed])
      );
    }
  }
}
