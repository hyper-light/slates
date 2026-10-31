//! The FUSE transport's coherence state beyond the delivery rounds (§4.6 "Cache posture"): the journal records its
//! own requests took, the files open for writing, the byte states of the dirty-page write-back rule, and the names the
//! kernel holds. The rounds themselves (`Coherence`, `Delivered`) are every transport's and live in
//! `slates_bridge_core::delivery`. The rules are pure, cfg-free and unit-tested on every host; the Linux transport
//! (`crate::channel`) runs them against a real mount.

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
/// can dirty a page, so only its file needs byte states ([`PageStates`]). Bounded by the handles the kernel holds, since
/// each is forgotten at its release.
///
/// A handle is known by its value and its file together. A daemon that takes a mount over (A-61) starts a fresh handle
/// store and reissues the values its predecessor issued, so the kernel can hold one value for a file it opened before
/// the takeover and for another opened after (A-147). A `RELEASE` names its file as well as its handle, so each finds
/// its own entry.
#[derive(Debug, Default)]
pub struct WritableOpens {
  /// The handles, each with the inode it opens.
  handles: std::collections::BTreeSet<(u64, u64)>,
  /// Inode → how many writable handles it has.
  per_inode: std::collections::BTreeMap<u64, usize>,
}

impl WritableOpens {
  /// Notes handle `fh` granted for writing on `ino`; whether it is the inode's first writable handle on this mount.
  pub fn opened(&mut self, fh: u64, ino: u64) -> bool {
    if !self.handles.insert((fh, ino)) {
      return false;
    }
    let count = self.per_inode.entry(ino).or_insert(0);
    *count = count.saturating_add(1);
    *count == 1
  }

  /// Forgets handle `fh` of `ino`: the inode, if that was its last writable handle.
  pub fn released(&mut self, fh: u64, ino: u64) -> Option<u64> {
    if !self.handles.remove(&(fh, ino)) {
      return None;
    }
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

  /// Whether handle `fh` of `ino` is one this mount has noted.
  pub fn knows(&self, fh: u64, ino: u64) -> bool {
    self.handles.contains(&(fh, ino))
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
/// - **another path's record** at `s` makes bytes with an older point stale;
/// - **an ambiguous read** at head `h` (A-144) — a READ through a handle opened `O_DIRECT` — leaves its fresh bytes as
///   they are and makes the rest (`h`, unknown). It is a direct read, which leaves the kernel's copy as it was, or a
///   fill of that copy (a fault on a map of the handle, a buffered fallback), and FUSE carries nothing that tells them
///   apart. Counted as a fetch it once marked a stale cached page fresh, and that page's write-back undid another
///   path's write. A write-back over an unknown byte is refused, as one the states cannot tell; a fetch or the mount's
///   own record makes the byte fresh again, and another path's record makes it stale, as both readings are then.
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
/// volume is busy keeps one range per page.
///
/// The ranges are charged to the shard's metadata ledger (§4.2, the metadata dimension), [`RANGE_BYTES`] each, as the
/// journal charges a record its fixed size: the owner grows the states' room before an update that can add ranges
/// ([`PageStates::grow_room`]), and a file whose update would take the ranges held past the room is lost, and counted.
/// So a file is lost only when the ledger refuses, and no range is held uncharged. Until 2026-10-23 each file's bound
/// was the journal's capacity, which the journal's derived budget would have shrunk to about 46 ranges on a quiet
/// volume.
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
  /// The ranges every tracked file holds between them; a lost file holds none.
  held: usize,
  /// The ranges the shard's metadata ledger has admitted for these states ([`PageStates::grow_room`]).
  room: usize,
  /// Files lost because an update found no room, since this was last read ([`PageStates::take_lost_for_room`]).
  lost_for_room: u64,
}

/// Format: what one byte-state range is charged against the shard's metadata ledger: its start and its end, point and
/// state, as the journal charges a record its fixed size (`size_of::<OpRecord>()`).
pub const RANGE_BYTES: usize = size_of::<u64>() + size_of::<(u64, u64, Byte)>();

/// Shape: the most ranges one update of one range can add to a file: a split at each of its ends.
pub const MOST_ADDED_BY_AN_UPDATE: usize = 2;

/// What the mount knows of one byte of its kernel's cached copy, beside the point that copy is as of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Byte {
  /// No other path changed it since the point.
  Fresh,
  /// Another path changed it since the point: the kernel's copy predates that change.
  Stale,
  /// One of the two, which the mount cannot tell: an ambiguous read (A-144) left it stale if the read was direct, and
  /// fresh if it filled the cache.
  Unknown,
}

/// One tracked file's byte states.
#[derive(Debug, Clone)]
struct FileStates {
  /// Start → (end, point, state), disjoint and covering every byte; touching neighbours of one state are merged.
  ranges: std::collections::BTreeMap<u64, (u64, u64, Byte)>,
  /// The file needed ranges the ledger would not admit, or the journal stopped reaching back for it: what its kernel
  /// holds can no longer be told.
  lost: bool,
  /// Listed in [`PageStates::touched`].
  touched: bool,
}

impl PageStates {
  /// The ranges every tracked file holds between them.
  pub fn held(&self) -> usize {
    self.held
  }

  /// The ranges the ledger has admitted for these states.
  pub fn room(&self) -> usize {
    self.room
  }

  /// The ledger has admitted `ranges` more ranges for these states.
  pub fn grow_room(&mut self, ranges: usize) {
    self.room = self.room.saturating_add(ranges);
  }

  /// The files lost for want of room since this was last asked, and zero afterwards. Zero is the healthy reading.
  pub fn take_lost_for_room(&mut self) -> u64 {
    std::mem::take(&mut self.lost_for_room)
  }

  /// The most ranges an ambiguous read of `ino` can add when `pending` changes are not yet consumed: one update per
  /// run the read covers, and a run per range the file holds and per split the changes make, plus one.
  pub fn most_added_by_an_ambiguous_read(&self, ino: u64, pending: usize) -> usize {
    let held = self.files.get(&ino).map_or(0, |file| file.ranges.len());
    held
      .saturating_add(pending.saturating_mul(MOST_ADDED_BY_AN_UPDATE))
      .saturating_add(1)
      .saturating_mul(MOST_ADDED_BY_AN_UPDATE)
  }

  /// Charges the change in `ino`'s ranges from `before`, and loses the file if the ranges held then pass the room: the
  /// ledger did not admit them. A lost file holds no ranges.
  fn settle(&mut self, ino: u64, before: usize) {
    let Some(file) = self.files.get_mut(&ino) else {
      return;
    };
    let after = file.ranges.len();
    self.held = self.held.saturating_sub(before).saturating_add(after);
    if self.held > self.room {
      self.held = self.held.saturating_sub(after);
      file.ranges.clear();
      file.lost = true;
      self.lost_for_room = self.lost_for_room.saturating_add(1);
    }
  }

  /// The file `ino` is opened for writing for the first time on this mount, at journal point `point`, the kernel told
  /// to drop its cached pages: every byte is fresh as of `point`. A file already tracked keeps its states.
  pub fn opened(&mut self, ino: u64, point: u64) {
    if self.files.contains_key(&ino) {
      return;
    }
    self.files.insert(ino, FileStates::fresh(point));
    self.settle(ino, 0);
  }

  /// The file `ino` was open for writing in the kernel before this mount served it — a mount taken over from a daemon
  /// that died (A-61) — and a request has just named it (A-147). It is tracked from here on: every byte fresh as of
  /// `start`, the point the mount began serving, with the journal's `changes` to it since then applied in order, each
  /// with whether it is the mount's own. Those changes were consumed while the file was untracked, so without them a
  /// byte another path changed in between would read as fresh. `None` for the changes means the journal no longer
  /// reaches back to `start`: the file is tracked as lost, and its write-backs are refused, as an untracked file's were.
  /// A file already tracked keeps its states.
  pub fn adopted(
    &mut self,
    ino: u64,
    start: u64,
    changes: Option<&[(std::ops::Range<u64>, u64, bool)]>,
  ) {
    if self.files.contains_key(&ino) {
      return;
    }
    let mut file = FileStates::fresh(start);
    match changes {
      Some(changes) => {
        for (range, seq, own) in changes {
          file.change(range.clone(), *seq, *own);
        }
        file.touch(ino, &mut self.touched);
      }
      None => {
        file.ranges.clear();
        file.lost = true;
      }
    }
    self.files.insert(ino, file);
    self.settle(ino, 0);
  }

  /// Whether `ino` is tracked.
  pub fn tracks(&self, ino: u64) -> bool {
    self.files.contains_key(&ino)
  }

  /// Bytes `range` of `ino` were fetched at head `point`: fresh as of it. Nothing for a file not tracked.
  pub fn fetched(&mut self, ino: u64, range: std::ops::Range<u64>, point: u64) {
    let Some(file) = self.files.get_mut(&ino) else {
      return;
    };
    let before = file.ranges.len();
    file.update(range, |_, _| (point, Byte::Fresh));
    file.touch(ino, &mut self.touched);
    self.settle(ino, before);
  }

  /// Bytes `range` of `ino` were read at head `point` through a handle that may bypass the kernel's page cache
  /// (`O_DIRECT`; A-144): a direct read, which leaves the kernel's copy as it was, or a fill of that copy (a fault on a
  /// map of the handle, a buffered fallback), which the mount cannot tell apart. A byte fresh after the not yet
  /// consumed `pending` changes is fresh either way and stays so; any other becomes unknown as of `point`, since the
  /// two readings disagree on it. Nothing for a file not tracked or lost.
  pub fn read_ambiguously(
    &mut self,
    ino: u64,
    range: std::ops::Range<u64>,
    point: u64,
    pending: &[(std::ops::Range<u64>, u64, bool)],
  ) {
    let Some(file) = self.files.get_mut(&ino) else {
      return;
    };
    let before = file.ranges.len();
    let mut scratch = file.clipped(range.clone());
    for (changed, seq, own) in pending {
      scratch.change(changed.clone(), *seq, *own);
    }
    for part in scratch.not_fresh_within(range) {
      file.update(part, |_, _| (point, Byte::Unknown));
    }
    file.touch(ino, &mut self.touched);
    self.settle(ino, before);
  }

  /// A journal change to `ino`'s bytes `range` at sequence `seq`: the mount's own (`own`), which refreshes the bytes
  /// fetched before it, or another path's, which leaves them stale. Nothing for a file not tracked. Applying one twice is
  /// harmless: a refresh only raises a point, and a byte already past `seq` is untouched.
  pub fn changed(&mut self, ino: u64, range: std::ops::Range<u64>, seq: u64, own: bool) {
    let Some(file) = self.files.get_mut(&ino) else {
      return;
    };
    let before = file.ranges.len();
    file.change(range, seq, own);
    file.touch(ino, &mut self.touched);
    self.settle(ino, before);
  }

  /// The mount has consumed every journal record through `through`: each was applied with [`PageStates::changed`]. Any
  /// record still to come is later, so the points at or below `through` can no longer be told apart; they are raised to
  /// it, and the ranges they split merge. Only the files changed since the last call are walked, since the others hold
  /// one point already.
  pub fn consumed(&mut self, through: u64) {
    let touched = std::mem::take(&mut self.touched);
    for ino in touched {
      let Some(file) = self.files.get_mut(&ino) else {
        continue;
      };
      let before = file.ranges.len();
      file.touched = false;
      file.raise_points(through);
      self.settle(ino, before);
    }
  }

  /// The stale bytes of `range` of `ino`, after the not yet consumed `pending` changes to it (each with whether it is the
  /// mount's own) applied in order to a scratch copy: ascending and disjoint. A file not tracked is judged as fresh at
  /// `untracked`. `None` when it cannot be told: the file's states were lost, or a byte of `range` is unknown (A-144).
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
      scratch.change(changed.clone(), *seq, *own);
    }
    scratch.known_stale_within(range)
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
    self.held = 0;
  }

  /// The file `ino` has no writable open left on this mount: its states go. No write-back of it can follow, since a
  /// write-back holds its file open until it is answered, so the file's release comes after.
  pub fn dropped(&mut self, ino: u64) {
    let Some(file) = self.files.remove(&ino) else {
      return;
    };
    self.held = self.held.saturating_sub(file.ranges.len());
    if file.touched {
      self.touched.retain(|&listed| listed != ino);
    }
  }
}

impl FileStates {
  /// Every byte fresh as of `point`.
  fn fresh(point: u64) -> FileStates {
    let mut ranges = std::collections::BTreeMap::new();
    ranges.insert(0, (u64::MAX, point, Byte::Fresh));
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
    let mut raised: std::collections::BTreeMap<u64, (u64, u64, Byte)> =
      std::collections::BTreeMap::new();
    let mut run: Option<(u64, (u64, u64, Byte))> = None;
    for (&start, &(end, point, byte)) in &self.ranges {
      let state = (end, point.max(floor), byte);
      run = match run {
        Some((run_start, (run_end, run_point, run_byte)))
          if run_end == start && run_point == state.1 && run_byte == byte =>
        {
          Some((run_start, (end, run_point, run_byte)))
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

  /// A journal change at `seq`: bytes in `range` with an older point are refreshed (`own`) or left stale. An unknown byte
  /// is either reading of it, and the change settles both alike: the mount's own write refreshed the kernel's copy, and
  /// another path's made it stale whether the ambiguous read had filled it or not.
  fn change(&mut self, range: std::ops::Range<u64>, seq: u64, own: bool) {
    self.update(range, |point, byte| {
      if point >= seq {
        (point, byte)
      } else if own {
        (seq, Byte::Fresh)
      } else {
        (point, Byte::Stale)
      }
    });
  }

  /// Gives every byte of `range` the state `f` makes of its current one, then merges touching neighbours of one state:
  /// at most [`MOST_ADDED_BY_AN_UPDATE`] ranges more. Its owner charges them ([`PageStates::settle`]).
  fn update(&mut self, range: std::ops::Range<u64>, f: impl Fn(u64, Byte) -> (u64, Byte)) {
    if self.lost || range.start >= range.end {
      return;
    }
    self.split_at(range.start);
    self.split_at(range.end);
    let inside: Vec<(u64, (u64, u64, Byte))> = self
      .ranges
      .range(range.start..range.end)
      .map(|(&start, &state)| (start, state))
      .collect();
    for (start, (end, point, byte)) in inside {
      let (point, byte) = f(point, byte);
      self.ranges.insert(start, (end, point, byte));
    }
    self.merge_around(range);
  }

  /// Splits the range containing `at` so one starts there.
  fn split_at(&mut self, at: u64) {
    if let Some((&start, &(end, point, byte))) = self.ranges.range(..=at).next_back()
      && start < at
      && at < end
    {
      self.ranges.insert(start, (at, point, byte));
      self.ranges.insert(at, (end, point, byte));
    }
  }

  /// Merges touching neighbours of one state from just before `range` to just past it; a range ending before it starts
  /// has nothing around it to merge.
  fn merge_around(&mut self, range: std::ops::Range<u64>) {
    if range.start > range.end {
      return;
    }
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
      let Some(&(end, point, byte)) = self.ranges.get(&start) else {
        continue;
      };
      if let Some(&(next_end, next_point, next_byte)) = self.ranges.get(&end)
        && next_point == point
        && next_byte == byte
      {
        self.ranges.remove(&end);
        self.ranges.insert(start, (next_end, point, byte));
      }
    }
  }

  /// A copy of the states over `range` alone; an empty range holds no bytes, and nothing of them. Until A-192 an empty
  /// range searched the map from one past its own end, which `BTreeMap` refuses with a panic: an NFS client's WRITE of no
  /// bytes took its shard down.
  fn clipped(&self, range: std::ops::Range<u64>) -> FileStates {
    let mut ranges = std::collections::BTreeMap::new();
    if range.start >= range.end {
      return FileStates {
        ranges,
        lost: self.lost,
        touched: false,
      };
    }
    let reaching = self
      .ranges
      .range(..=range.start)
      .next_back()
      .filter(|(_, (end, _, _))| *end > range.start);
    let inside = self.ranges.range(range.start.saturating_add(1)..range.end);
    for (&start, &(end, point, byte)) in reaching.into_iter().chain(inside) {
      let (start, end) = (start.max(range.start), end.min(range.end));
      if start < end {
        ranges.insert(start, (end, point, byte));
      }
    }
    FileStates {
      ranges,
      lost: self.lost,
      touched: false,
    }
  }

  /// The bytes within `range` whose state `wanted` accepts, ascending and disjoint, touching runs joined.
  fn bytes_within(
    &self,
    range: std::ops::Range<u64>,
    wanted: impl Fn(Byte) -> bool,
  ) -> Vec<std::ops::Range<u64>> {
    let mut found: Vec<std::ops::Range<u64>> = Vec::new();
    for (&start, &(end, _, byte)) in &self.ranges {
      let (start, end) = (start.max(range.start), end.min(range.end));
      if !wanted(byte) || start >= end {
        continue;
      }
      match found.last_mut() {
        Some(last) if last.end == start => last.end = end,
        _ => found.push(start..end),
      }
    }
    found
  }

  /// The stale bytes within `range`, ascending and disjoint; `None` when one of its bytes is unknown, so which are stale
  /// cannot be told.
  fn known_stale_within(&self, range: std::ops::Range<u64>) -> Option<Vec<std::ops::Range<u64>>> {
    if !self
      .bytes_within(range.clone(), |byte| byte == Byte::Unknown)
      .is_empty()
    {
      return None;
    }
    Some(self.bytes_within(range, |byte| byte == Byte::Stale))
  }

  /// The bytes within `range` that are stale or unknown: those an ambiguous read leaves unknown.
  fn not_fresh_within(&self, range: std::ops::Range<u64>) -> Vec<std::ops::Range<u64>> {
    self.bytes_within(range, |byte| byte != Byte::Fresh)
  }
}

/// What this mount's kernel holds of the volume (§4.6 "Cache posture"; A-141): every node it references, with the
/// lookups it holds on it, and every name it was handed out under. When the mount falls behind the journal's retention
/// it is told [`slates_bridge_core::Invalidation::Everything`] and invalidates exactly these
/// (`docs/bugs/2026-10-22-a-mount-that-falls-behind-the-journal-never-hears-of-the-changes-it-missed.md`).
///
/// It follows the FUSE lookup protocol. Every entry reply the kernel received adds a lookup on its node and the name it
/// came under, and `FORGET` gives lookups back; at none left the node and its names go. A name also goes when the
/// mount's own unlink or rename takes it away, or when an invalidation of it reached the kernel, since the kernel has
/// then dropped or expired it and a later use is a new lookup that names it again. The set errs one way only. A name
/// kept too long costs one redundant notification, but a name the kernel holds is never missing, because every name it
/// holds came in a reply.
///
/// Bounded by the volume: every node id is a volume inode, so nodes are at most its inode allowance; and the names are
/// its entries naming referenced nodes, plus names in flight until their invalidation reaches the kernel.
#[derive(Debug, Default)]
pub struct KernelHolds {
  /// Node id → the lookups the kernel holds on it.
  lookups: std::collections::BTreeMap<u64, u64>,
  /// (parent node id, name) → the node it names.
  names: std::collections::BTreeMap<(u64, String), u64>,
  /// Node id → the names it was handed out under: the reverse of `names`, so a forgotten node's names go with it.
  named: std::collections::BTreeMap<u64, std::collections::BTreeSet<(u64, String)>>,
}

impl KernelHolds {
  /// The kernel took one lookup on `node`, handed out as `name` in `parent` (an entry reply or a READDIRPLUS entry), or
  /// under no name (an `O_TMPFILE`). Node id 0 is a negative entry and takes nothing.
  pub fn handed_out(&mut self, parent: u64, name: Option<&str>, node: u64) {
    if node == 0 {
      return;
    }
    let held = self.lookups.entry(node).or_insert(0);
    *held = held.saturating_add(1);
    if let Some(name) = name {
      self.name(parent, name, node);
    }
  }

  /// The kernel gave back `count` lookups on `node`. At none left, the node and every name it was handed out under go.
  pub fn forgot(&mut self, node: u64, count: u64) {
    let Some(held) = self.lookups.get_mut(&node) else {
      return;
    };
    *held = held.saturating_sub(count);
    if *held > 0 {
      return;
    }
    self.lookups.remove(&node);
    for key in self.named.remove(&node).unwrap_or_default() {
      if self.names.get(&key) == Some(&node) {
        self.names.remove(&key);
      }
    }
  }

  /// The name `name` in `parent` is no longer the kernel's: the mount's own unlink or rmdir removed it, or an
  /// invalidation of it reached the kernel.
  pub fn unnamed(&mut self, parent: u64, name: &str) {
    let key = (parent, name.to_owned());
    if let Some(node) = self.names.remove(&key)
      && let Some(set) = self.named.get_mut(&node)
    {
      set.remove(&key);
    }
  }

  /// Whether the kernel was handed `name` in `parent` and has not given it back. A name it never got cannot be in its
  /// cache, so the kernel's `ENOENT` for an invalidation of it is the expected answer, not a refusal.
  pub fn holds_name(&self, parent: u64, name: &str) -> bool {
    self.names.contains_key(&(parent, name.to_owned()))
  }

  /// The mount's own rename moved `from` to `to`, or swapped the two when `exchange` is set (`RENAME_EXCHANGE`).
  pub fn renamed(&mut self, from: (u64, &str), to: (u64, &str), exchange: bool) {
    let moved = self.names.get(&(from.0, from.1.to_owned())).copied();
    let displaced = self.names.get(&(to.0, to.1.to_owned())).copied();
    self.unnamed(from.0, from.1);
    self.unnamed(to.0, to.1);
    if let Some(node) = moved {
      self.name(to.0, to.1, node);
    }
    if let (true, Some(node)) = (exchange, displaced) {
      self.name(from.0, from.1, node);
    }
  }

  /// Every node the kernel references, and every name it holds: what an `Everything` invalidates.
  pub fn everything(&self) -> (Vec<u64>, Vec<(u64, String)>) {
    (
      self.lookups.keys().copied().collect(),
      self.names.keys().cloned().collect(),
    )
  }

  /// What an `Everything` invalidates ([`KernelHolds::everything`]), taking the names away as it goes: once the round
  /// is written the kernel has dropped or expired each, and a later use is a new lookup that names it again. The nodes
  /// stay, since an invalidation takes no lookup away. Taken when the round is built, not when it returns: a lookup
  /// answered meanwhile then adds its name back, so a name the kernel holds is never missing.
  pub fn invalidate_everything(&mut self) -> (Vec<u64>, Vec<(u64, String)>) {
    let held = self.everything();
    self.names.clear();
    for set in self.named.values_mut() {
      set.clear();
    }
    held
  }

  /// Notes that `name` in `parent` names `node`, taking the name from whatever node it named before.
  fn name(&mut self, parent: u64, name: &str, node: u64) {
    let key = (parent, name.to_owned());
    if let Some(before) = self.names.insert(key.clone(), node)
      && before != node
      && let Some(set) = self.named.get_mut(&before)
    {
      set.remove(&key);
    }
    self.named.entry(node).or_default().insert(key);
  }
}

#[cfg(test)]
mod tests {
  use super::{OwnRecords, PageStates};

  /// A-192, hostile input: an NFS client sends a WRITE of no bytes, and one at the very end of the offset range, which no
  /// FUSE kernel does. Do: on a tracked file, judge, fetch and change empty ranges and ranges ending at `u64::MAX`.
  /// Expect: each answered — no stale bytes in an empty range, the fetched and changed bytes as said — and no panic;
  /// before the fix the empty range's judgment searched the states from past its own end, which `BTreeMap` refuses with
  /// a panic, and the shard serving it went down.
  #[test]
  fn empty_ranges_and_ranges_at_the_end_are_judged_without_a_panic() {
    let mut states = PageStates::default();
    states.grow_room(64);
    states.opened(7, 1);
    for empty in [0..0, 5..5, u64::MAX..u64::MAX] {
      assert_eq!(
        states.stale_after(7, empty.clone(), 1, &[]),
        Some(Vec::new())
      );
      states.fetched(7, empty.clone(), 2);
      states.changed(7, empty, 3, false);
    }
    states.changed(7, (u64::MAX - 1)..u64::MAX, 4, false);
    assert_eq!(
      states.stale_after(7, (u64::MAX - 10)..u64::MAX, 1, &[]),
      Some(vec![(u64::MAX - 1)..u64::MAX]),
      "the changed last byte is stale"
    );
  }

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

    use super::super::{MOST_ADDED_BY_AN_UPDATE, PageStates};

    /// Grows `states`' room to hold `more` ranges beyond those it holds, as the channel asks the ledger before an update.
    fn make_room(states: &mut PageStates, more: usize) {
      let short = states
        .held()
        .saturating_add(more)
        .saturating_sub(states.room());
      states.grow_room(short);
    }

    /// Shape: the bytes of the one file the model tracks — past every generated offset and length.
    const MODEL_BYTES: u64 = 96;
    /// Shape: the inode the history works on.
    const INO: u64 = 7;
    /// Shape: the point the file's first writable open is at.
    const OPENED_AT: u64 = 3;

    /// One generated event: a fetch at the head, an ambiguous read at the head (A-144), the mount's own record, another
    /// path's record (each with its range), or the mount consuming the journal through the head; and how far the journal
    /// moved before it (records of other files).
    #[derive(Clone, Debug)]
    enum Event {
      Fetch(u64, u64),
      Ambiguous(u64, u64),
      Own(u64, u64),
      Other(u64, u64),
      Consume,
    }

    fn event() -> impl Strategy<Value = (Event, u64)> {
      (0..5u8, 0..80u64, 1..24u64, 0..3u64).prop_map(|(kind, at, len, gap)| {
        let event = match kind {
          0 => Event::Fetch(at, at + len),
          1 => Event::Ambiguous(at, at + len),
          2 => Event::Own(at, at + len),
          3 => Event::Other(at, at + len),
          _ => Event::Consume,
        };
        (event, gap)
      })
    }

    /// What the model knows of one byte of the kernel's copy, written apart from the implementation's own type.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Known {
      Fresh,
      Stale,
      Unknown,
    }

    /// The model's rule for one byte, as the design states it: what `event` at `at` (the head, for a read; the record's
    /// sequence, otherwise) makes of the byte's (point, state). An ambiguous read is a direct read or a fill: a byte the
    /// two leave alike (fresh) keeps its state, and any other becomes unknown.
    fn rule(event: &Event, (point, known): (u64, Known), at: u64) -> (u64, Known) {
      match event {
        Event::Fetch(..) => (at, Known::Fresh),
        Event::Ambiguous(..) if known != Known::Fresh => (at, Known::Unknown),
        Event::Own(..) if point < at => (at, Known::Fresh),
        Event::Other(..) if point < at => (point, Known::Stale),
        Event::Ambiguous(..) | Event::Own(..) | Event::Other(..) | Event::Consume => (point, known),
      }
    }

    proptest! {
      #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
        cases: slates_test_seeds::cases(2000),
        .. ProptestConfig::default()
      }))]

      /// The dirty-page write-back rule's byte states, against a per-byte model of (point, state) stated as the design's
      /// rule: a fetch at the head makes its bytes fresh at the head; an ambiguous read at the head leaves fresh bytes
      /// alone and makes the others unknown at the head; a record at sequence `s` (the journal moves on by one) refreshes
      /// the bytes read before it when it is the mount's own and leaves them stale when it is another path's. The model
      /// never raises a point; the states raise them whenever the mount has consumed every record so far, which must
      /// change no answer. After any history, with the last few records held back as pending and applied to a scratch
      /// copy, a range holding an unknown byte cannot be told, and any other range's stale bytes equal the model's. The
      /// room is grown before each event by exactly the most that event can add, as the channel grows it from the
      /// ledger, so a bound that understated an update's growth would lose the file and fail the comparison; no file is
      /// lost for want of room.
      #[test]
      fn stale_bytes_follow_the_rule(
        events in prop::collection::vec(event(), 0..20),
        held_back in 0..4usize,
        query in (0..MODEL_BYTES, 1..MODEL_BYTES),
      ) {
        let mut states = PageStates::default();
        make_room(&mut states, 1);
        states.opened(INO, OPENED_AT);
        let mut model = vec![(OPENED_AT, Known::Fresh); usize::try_from(MODEL_BYTES).unwrap()];
        let mut head = OPENED_AT;
        let split = events.len().saturating_sub(held_back);
        let mut pending = Vec::new();
        for (index, (event, gap)) in events.iter().enumerate() {
          head += gap;
          let (from, to) = match event {
            Event::Fetch(from, to)
            | Event::Ambiguous(from, to)
            | Event::Own(from, to)
            | Event::Other(from, to) => (*from, (*to).min(MODEL_BYTES)),
            // Only while nothing is held back: a pending record lies at or below the head and is not consumed yet.
            Event::Consume => {
              if index < split {
                states.consumed(head);
              }
              continue;
            }
          };
          if matches!(event, Event::Own(..) | Event::Other(..)) {
            head += 1;
          }
          for byte in from..to {
            let cell = &mut model[usize::try_from(byte).unwrap()];
            *cell = rule(event, *cell, head);
          }
          match event {
            Event::Fetch(..) => {
              make_room(&mut states, MOST_ADDED_BY_AN_UPDATE);
              states.fetched(INO, from..to, head);
            }
            // An ambiguous read is judged with the records held back so far, as the mount judges it.
            Event::Ambiguous(..) => {
              let most = states.most_added_by_an_ambiguous_read(INO, pending.len());
              make_room(&mut states, most);
              states.read_ambiguously(INO, from..to, head, &pending);
            }
            Event::Own(..) | Event::Other(..) if index < split => {
              make_room(&mut states, MOST_ADDED_BY_AN_UPDATE);
              states.changed(INO, from..to, head, matches!(event, Event::Own(..)));
            }
            Event::Own(..) | Event::Other(..) => {
              pending.push((from..to, head, matches!(event, Event::Own(..))));
            }
            Event::Consume => {}
          }
        }
        prop_assert_eq!(states.take_lost_for_room(), 0, "no update passed the room it was given");
        // A read held back behind pending records would be out of order: reads are applied as they happen.
        let (from, len) = query;
        let to = (from + len).min(MODEL_BYTES);
        let answer = states.stale_after(INO, from..to, 0, &pending);
        let queried = &model[usize::try_from(from).unwrap()..usize::try_from(to).unwrap()];
        if queried.iter().any(|(_, known)| *known == Known::Unknown) {
          prop_assert_eq!(answer, None, "an unknown byte in {}..{}: which are stale cannot be told", from, to);
          return Ok(());
        }
        prop_assert!(answer.is_some(), "no unknown byte in {}..{}: it can be told", from, to);
        let stale = answer.unwrap();
        let mut got = vec![false; usize::try_from(MODEL_BYTES).unwrap()];
        for range in &stale {
          prop_assert!(range.start >= from && range.end <= to && range.start < range.end, "{:?}", stale);
          for byte in range.clone() {
            got[usize::try_from(byte).unwrap()] = true;
          }
        }
        for byte in from..to {
          let index = usize::try_from(byte).unwrap();
          prop_assert_eq!(got[index], model[index].1 == Known::Stale, "byte {} in {:?}", byte, stale);
        }
        prop_assert!(stale.windows(2).all(|pair| pair[0].end < pair[1].start), "{:?}", stale);
      }
    }

    /// A-144, the ambiguous read, case by case. Do: open a file (fresh at 3); another path changes bytes 0..4 at 5; an
    /// ambiguous read covers 2..8 at head 6. Expect: bytes 0..2 stay known stale and 4..8 stay fresh, but a range over
    /// 2..4 cannot be told. Then a fetch of 2..4 at 7 makes them known fresh; another ambiguous read of 0..2 at 8 makes
    /// those unknown, and another path's change of byte 0 at 9 makes it stale, as both readings are, while byte 1 stays
    /// unknown. Last, an ambiguous read made while another path's change is still pending makes the bytes it covers
    /// unknown, as the mount judges with the records it has not consumed.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // a list of one stale range, not the numbers inside it
    fn an_ambiguous_read_makes_stale_bytes_unknown_until_a_fetch_or_a_change() {
      /// Shape: the room the ledger admits: ranges enough for every split below.
      const BOUND: usize = 16;
      let mut states = PageStates::default();
      states.grow_room(BOUND);
      states.opened(INO, OPENED_AT);
      states.changed(INO, 0..4, 5, false);
      states.read_ambiguously(INO, 2..8, 6, &[]);
      assert_eq!(
        states.stale_after(INO, 0..2, 0, &[]),
        Some(vec![0..2]),
        "stale bytes the read did not cover stay known stale"
      );
      assert_eq!(
        states.stale_after(INO, 4..8, 0, &[]),
        Some(vec![]),
        "fresh bytes the read covered stay fresh"
      );
      assert_eq!(
        states.stale_after(INO, 0..8, 0, &[]),
        None,
        "a write-back over the unknown bytes cannot be told"
      );
      states.fetched(INO, 2..4, 7);
      assert_eq!(
        states.stale_after(INO, 0..8, 0, &[]),
        Some(vec![0..2]),
        "a fetch makes them known again"
      );
      states.read_ambiguously(INO, 0..2, 8, &[]);
      states.changed(INO, 0..1, 9, false);
      assert_eq!(
        states.stale_after(INO, 0..1, 0, &[]),
        Some(vec![0..1]),
        "another path's later change makes an unknown byte stale either way"
      );
      assert_eq!(
        states.stale_after(INO, 1..2, 0, &[]),
        None,
        "the byte it did not change stays unknown"
      );
      let pending = vec![(10..12, 10, false)];
      states.read_ambiguously(INO, 8..16, 11, &pending);
      assert_eq!(
        states.stale_after(INO, 8..16, 0, &pending),
        None,
        "a pending change counts: the bytes it made stale are unknown after the read"
      );
      assert_eq!(
        states.stale_after(INO, 12..16, 0, &pending),
        Some(vec![]),
        "and the bytes it did not touch stay fresh"
      );
    }

    /// An untracked file is judged as fresh at the point the caller gives; a file whose ranges passed the room is
    /// refused, not guessed, and consuming the journal does not bring it back; losing every file refuses them all; a
    /// dropped file is untracked again.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // a list of one stale range, not the numbers inside it
    fn untracked_lost_and_dropped_files() {
      /// Shape: the room the ledger admits: the most ranges the file may hold here.
      const BOUND: usize = 4;
      let mut states = PageStates::default();
      states.grow_room(BOUND);
      let other = vec![(0..4, 10, false)];
      assert_eq!(
        states.stale_after(INO, 0..8, 5, &other),
        Some(vec![0..4]),
        "untracked, fresh at 5"
      );
      states.opened(INO, OPENED_AT);
      // Two fetches at their own heads leave four ranges: fetched, as opened, fetched, as opened.
      states.fetched(INO, 0..4, 5);
      states.fetched(INO, 8..12, 6);
      assert!(
        states.stale_after(INO, 0..12, 0, &[]).is_some(),
        "four ranges: within the room"
      );
      states.fetched(INO, 20..24, 7);
      assert_eq!(
        states.stale_after(INO, 0..12, 0, &[]),
        None,
        "six ranges: past the room, lost"
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

    /// Step 2 of "Journal budget derived" (the byte states charged to the metadata ledger): the room is the mount's,
    /// shared by its files, and an update that would pass it loses only the file being updated. Do: open two files in a
    /// room of exactly their two ranges, then let another path change bytes in the middle of the second (three ranges
    /// where it had one); drop the first, then the lost second. Expect the second lost and counted once, the first still
    /// judged meanwhile, a lost file holding no ranges, and nothing held once both are dropped.
    #[test]
    fn a_full_room_loses_only_the_file_being_updated() {
      /// Shape: the second file.
      const OTHER: u64 = INO + 1;
      let mut states = PageStates::default();
      states.grow_room(2);
      states.opened(INO, OPENED_AT);
      states.opened(OTHER, OPENED_AT);
      assert_eq!(states.held(), 2, "one range a file at its open");
      states.changed(OTHER, 10..20, OPENED_AT + 1, false);
      assert_eq!(
        states.take_lost_for_room(),
        1,
        "one file lost for want of room"
      );
      assert_eq!(states.take_lost_for_room(), 0, "and read once");
      assert_eq!(
        states.stale_after(OTHER, 0..30, 0, &[]),
        None,
        "the updated file is lost"
      );
      assert_eq!(
        states.stale_after(INO, 0..30, 0, &[]),
        Some(vec![]),
        "the other file is still judged"
      );
      assert_eq!(states.held(), 1, "a lost file holds no ranges");
      states.dropped(INO);
      states.dropped(OTHER);
      assert_eq!(states.held(), 0, "nothing held once both are dropped");
    }

    /// A long-lived file faulted in a page at a time while the volume is busy, as a guest's DAX window is: each fetch at
    /// a head of its own, another file's record moving the journal between them. Do: with a room of eight ranges, fetch
    /// eighty pages one by one, the mount consuming the journal after each as its rounds do; then another path changes
    /// ten bytes of page 40. Expect: the file is never lost, and exactly the ten bytes are stale. Without the raise each
    /// fetch keeps a range of its own and the file is lost by the eighth.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // a list of one stale range, not the numbers inside it
    fn pages_fetched_one_by_one_under_activity_stay_within_the_room() {
      /// Shape: the room the ledger admits: the most ranges the file may hold here.
      const BOUND: usize = 8;
      /// Shape: a page.
      const PAGE: u64 = 4096;
      /// Shape: ten times the room's pages.
      const PAGES: u64 = 80;
      let mut states = PageStates::default();
      states.grow_room(BOUND);
      states.opened(INO, OPENED_AT);
      let mut head = OPENED_AT;
      for page in 0..PAGES {
        head += 1;
        states.fetched(INO, page * PAGE..(page + 1) * PAGE, head);
        states.consumed(head);
        assert!(
          states.stale_after(INO, 0..PAGES * PAGE, 0, &[]).is_some(),
          "lost at page {page}"
        );
      }
      head += 1;
      let changed = 40 * PAGE + 10..40 * PAGE + 20;
      states.changed(INO, changed.clone(), head, false);
      states.consumed(head);
      assert_eq!(
        states.stale_after(INO, 0..PAGES * PAGE, 0, &[]),
        Some(vec![changed])
      );
    }

    /// A-147: a file a restored mount adopts is judged as an untracked file was until the kernel re-fetches it, and
    /// precisely after. Do: another path changes bytes 0..10 at 2 and the mount's own write changes 20..30 at 3, both
    /// after the mount began at 1, while the file is untracked; the file is then adopted with those changes. Expect a
    /// write-back over 0..40 to keep back exactly 0..10, as the untracked judgment does. Then the kernel re-fetches
    /// 0..10 at 4. Expect nothing kept back, where the untracked judgment still keeps 0..10: a store made after the
    /// re-fetch is the later write, and landing it is the fix.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // a list of one stale range, not the numbers inside it
    fn an_adopted_file_is_judged_as_untracked_until_a_refetch_and_precisely_after() {
      /// Shape: the room the ledger admits: ranges enough for every split below.
      const BOUND: usize = 16;
      /// Shape: where the mount began serving.
      const STARTED_AT: u64 = 1;
      let changes = vec![(0..10, 2, false), (20..30, 3, true)];
      let untracked = PageStates::default();
      let mut states = PageStates::default();
      states.grow_room(BOUND);
      states.adopted(INO, STARTED_AT, Some(&changes));
      assert!(states.tracks(INO), "adopted");
      assert_eq!(
        states.stale_after(INO, 0..40, STARTED_AT, &[]),
        untracked.stale_after(INO, 0..40, STARTED_AT, &changes),
        "until a re-fetch, judged as the untracked file was"
      );
      assert_eq!(
        states.stale_after(INO, 0..40, STARTED_AT, &[]),
        Some(vec![0..10]),
        "another path's bytes stale, the mount's own fresh"
      );
      states.fetched(INO, 0..10, 4);
      assert_eq!(
        states.stale_after(INO, 0..40, STARTED_AT, &[]),
        Some(vec![]),
        "after the re-fetch, nothing over those bytes is kept back"
      );
      assert_eq!(
        untracked.stale_after(INO, 0..40, STARTED_AT, &changes),
        Some(vec![0..10]),
        "where the untracked judgment kept them back: the lost store"
      );
    }

    /// A-147: a file adopted after the journal stopped reaching back to the mount's start cannot be judged. Do: adopt
    /// it without its changes. Expect its write-backs refused, as an untracked file's were then, and a second adoption
    /// to keep it lost rather than make it fresh.
    #[test]
    fn a_file_adopted_past_the_journals_reach_is_lost_until_it_is_dropped() {
      /// Shape: the room the ledger admits: ranges enough for the file.
      const BOUND: usize = 4;
      let mut states = PageStates::default();
      states.grow_room(BOUND);
      states.adopted(INO, OPENED_AT, None);
      assert_eq!(
        states.stale_after(INO, 0..8, 0, &[]),
        None,
        "refused, not guessed"
      );
      states.adopted(INO, OPENED_AT, Some(&[]));
      assert_eq!(
        states.stale_after(INO, 0..8, 0, &[]),
        None,
        "a later adoption keeps the states it found"
      );
      states.dropped(INO);
      assert!(!states.tracks(INO), "dropped at its last release");
    }
  }

  mod writable_opens {
    use super::super::WritableOpens;

    /// A-147: a daemon that takes a mount over reissues the handle values its predecessor issued, so the kernel can hold
    /// one value for two files. Do: note handle 7 of file 1, and handle 7 of file 2; release handle 7 of file 1. Expect
    /// file 1's last handle gone and file 2's still held, each release finding its own entry.
    #[test]
    fn a_reissued_handle_value_is_told_apart_by_its_file() {
      let mut opens = WritableOpens::default();
      assert!(opens.opened(7, 1), "file 1's first writable handle");
      assert!(opens.opened(7, 2), "file 2's first, with the same value");
      assert!(opens.knows(7, 1) && opens.knows(7, 2));
      assert_eq!(opens.released(7, 1), Some(1), "file 1's last handle");
      assert!(!opens.holds(1) && !opens.knows(7, 1));
      assert!(
        opens.holds(2) && opens.knows(7, 2),
        "file 2's handle is untouched"
      );
      assert_eq!(
        opens.released(7, 1),
        None,
        "a release of a handle already gone is nothing"
      );
      assert_eq!(opens.released(7, 2), Some(2));
    }
  }

  mod kernel_holds {
    // proptest's strategy types carry `Arc` (D-8's harness exception).
    #![allow(clippy::disallowed_types)]
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::super::KernelHolds;

    /// One generated event of the FUSE lookup protocol, over a few directories, names and nodes (node 0 is a negative
    /// entry): an entry reply received, lookups given back, the mount's own unlink, or its own rename or exchange.
    #[derive(Clone, Debug)]
    enum Event {
      HandOut(u64, u8, u64),
      Forget(u64, u64),
      Unname(u64, u8),
      Rename((u64, u8), (u64, u8), bool),
    }

    fn name(byte: u8) -> String {
      format!("n{byte}")
    }

    fn event() -> impl Strategy<Value = Event> {
      prop_oneof![
        (1..4u64, 0..4u8, 0..6u64)
          .prop_map(|(parent, name, node)| Event::HandOut(parent, name, node)),
        (0..6u64, 1..3u64).prop_map(|(node, count)| Event::Forget(node, count)),
        (1..4u64, 0..4u8).prop_map(|(parent, name)| Event::Unname(parent, name)),
        ((1..4u64, 0..4u8), (1..4u64, 0..4u8), any::<bool>())
          .prop_map(|(from, to, swap)| Event::Rename(from, to, swap)),
      ]
    }

    /// The protocol's rule, as a reference: lookups per node; each name the node it names. A forgotten node's names
    /// go; a rename moves a name, an exchange swaps two.
    #[derive(Default)]
    struct Model {
      lookups: BTreeMap<u64, u64>,
      names: BTreeMap<(u64, String), u64>,
    }

    impl Model {
      fn apply(&mut self, event: &Event) {
        match event {
          Event::HandOut(_, _, 0) => {}
          Event::HandOut(parent, byte, node) => {
            *self.lookups.entry(*node).or_insert(0) += 1;
            self.names.insert((*parent, name(*byte)), *node);
          }
          Event::Forget(node, count) => {
            if let Some(held) = self.lookups.get_mut(node) {
              *held = held.saturating_sub(*count);
              if *held == 0 {
                self.lookups.remove(node);
                self.names.retain(|_, named| named != node);
              }
            }
          }
          Event::Unname(parent, byte) => {
            self.names.remove(&(*parent, name(*byte)));
          }
          Event::Rename((from_parent, from_name), (to_parent, to_name), exchange) => {
            let from = (*from_parent, name(*from_name));
            let to = (*to_parent, name(*to_name));
            let moved = self.names.remove(&from);
            let displaced = self.names.remove(&to);
            if let Some(node) = moved {
              self.names.insert(to.clone(), node);
            }
            if let (true, Some(node)) = (exchange, displaced) {
              self.names.insert(from, node);
            }
          }
        }
      }
    }

    proptest! {
      #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
        cases: slates_test_seeds::cases(2000),
        .. ProptestConfig::default()
      }))]

      /// What a mount's kernel holds (A-141), against the protocol's rule kept as a reference model: after every event
      /// of a generated history the nodes and names an `Everything` would invalidate are exactly the model's — every
      /// node with a lookup left, every name the kernel was handed and has not lost.
      #[test]
      fn the_kernels_holdings_follow_the_lookup_protocol(events in prop::collection::vec(event(), 0..40)) {
        let mut holds = KernelHolds::default();
        let mut model = Model::default();
        for (step, event) in events.iter().enumerate() {
          model.apply(event);
          match event {
            Event::HandOut(parent, byte, node) => holds.handed_out(*parent, Some(&name(*byte)), *node),
            Event::Forget(node, count) => holds.forgot(*node, *count),
            Event::Unname(parent, byte) => holds.unnamed(*parent, &name(*byte)),
            Event::Rename((from_parent, from_name), (to_parent, to_name), exchange) => holds.renamed(
              (*from_parent, &name(*from_name)),
              (*to_parent, &name(*to_name)),
              *exchange,
            ),
          }
          let (nodes, names) = holds.everything();
          prop_assert_eq!(&nodes, &model.lookups.keys().copied().collect::<Vec<_>>(), "nodes after step {}", step);
          prop_assert_eq!(&names, &model.names.keys().cloned().collect::<Vec<_>>(), "names after step {}", step);
        }
      }
    }

    /// A node handed out under no name (an `O_TMPFILE`) is still the kernel's until it is forgotten, and a node id of
    /// zero (a negative entry) is never held.
    #[test]
    fn an_unnamed_node_is_held_and_a_negative_entry_is_not() {
      let mut holds = KernelHolds::default();
      holds.handed_out(1, None, 9);
      holds.handed_out(1, Some("missing"), 0);
      assert_eq!(holds.everything(), (vec![9], Vec::new()));
      holds.forgot(9, 1);
      assert_eq!(holds.everything(), (Vec::new(), Vec::new()));
    }
  }
}
