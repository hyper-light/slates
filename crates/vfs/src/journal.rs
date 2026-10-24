//! The op log (§4.5, "Journal"): every mutation appends a declared operation with its path(s),
//! inode, epoch, offset, length and the inode's version before it; bytes never enter the journal.
//!
//! Retention keeps what a live reader still needs (A-157). The budget starts at the floor its owner gives it (a
//! page), and the oldest records leave first while it is full. A record leaves only if every open reader has read past
//! it ([`OpLog::open_reader`]), or if it has waited longer than the patience of the reader still owing it: that reader
//! is presumed gone, and overruns if it returns. Otherwise the budget grows by what the incoming record needs, reserved
//! from the shard's metadata ledger. A refused growth keeps the budget, drops the oldest, and is counted. Writers never
//! wait. The budget never shrinks while the log lives; its growth is returned at the volume's teardown ([`OpLog::grown`]).
//! So the budget is the design's formula realized at the moment of need: the largest backlog a live reader ever had,
//! which by Little's law is the mutation rate times that reader's lag times the record size. The Disruptor's producers
//! track their consumers' sequences so the ring never wraps past an unread entry; this ring grows instead, since §4.5's
//! writers never wait on a mount (A-141). Until 2026-10-23 the budget was a fixed 10‰ of the quota.
//!
//! The log measures what its consumers find waiting ([`OpLog::note_round`]). §4.5 derives the budget as the mutation
//! rate times the longest subscriber lag times the record size. A round's backlog is that product measured directly:
//! by Little's law, the records waiting are the rate they arrived at times how long they waited, whatever the arrival
//! pattern. So each round records its backlog in records and bytes, and how long its oldest record had waited. The
//! log keeps the maxima since it was made; they are window-free, so the measurement adds no tuning constant (R3).

use std::collections::{BTreeMap, VecDeque};

use slates_mem::budget::MetadataBudget;

use crate::ids::{Epoch, InodeNo};

/// A declared operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
  /// A file created at `path`.
  Create,
  /// A FIFO/socket namespace entry created (A-26); no stream state is journaled.
  Mknod,
  /// A directory created.
  Mkdir,
  /// An entry unlinked.
  Unlink,
  /// A directory removed.
  Rmdir,
  /// A rename; `from` is the old path.
  Rename {
    /// The old path.
    from: Box<str>,
  },
  /// A hard link created at `path` to the inode.
  Link,
  /// A symlink created.
  Symlink,
  /// Bytes overwritten in place at `at` for `len`.
  Overwrite {
    /// Offset.
    at: u64,
    /// Length.
    len: u64,
  },
  /// Bytes appended past the old end: `at` is the old length, `len` the bytes added.
  Extend {
    /// The old length.
    at: u64,
    /// Bytes added.
    len: u64,
  },
  /// Truncated to `len`.
  Truncate {
    /// The new length.
    len: u64,
  },
  /// An SDK insert: `len` bytes inserted at `at`, shifting the rest.
  Insert {
    /// Offset.
    at: u64,
    /// Length.
    len: u64,
  },
  /// An SDK delete: `len` bytes removed at `at`, shifting the rest.
  Delete {
    /// Offset.
    at: u64,
    /// Length.
    len: u64,
  },
  /// Mode or ownership changed.
  Setattr,
  /// The extended attribute `name` set on the inode (§4.5 "Extended attributes").
  SetXattr {
    /// The attribute name.
    name: Box<[u8]>,
  },
  /// The extended attribute `name` removed from the inode.
  RemoveXattr {
    /// The attribute name.
    name: Box<[u8]>,
  },
  /// A whiteout written over a base-backed name.
  Whiteout,
  /// A base entry witnessed (copied up).
  Witness,
  /// A base directory renamed; `from` is the origin path on the base.
  Redirect {
    /// The origin.
    from: Box<str>,
  },
  /// Drift detected on a witnessed entry.
  Drift,
  /// A snapshot taken; the epoch is the frozen one.
  Snapshot,
}

/// One journal record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRecord {
  /// Sequence, monotonic per volume.
  pub seq: u64,
  /// The operation.
  pub op: Op,
  /// The path (the new path for a rename).
  pub path: Box<str>,
  /// The inode, when one is involved.
  pub inode: Option<InodeNo>,
  /// The head epoch when applied.
  pub epoch: Epoch,
  /// Monotonic nanoseconds.
  pub at: u64,
  /// The inode's version before the operation (0 for namespace operations without one).
  pub prev_version: u64,
}

impl OpRecord {
  /// The record's size for the retention budget: the fixed part plus its paths.
  fn bytes(&self) -> usize {
    let paths = self.path.len().saturating_add(match &self.op {
      Op::Rename { from } | Op::Redirect { from } => from.len(),
      Op::SetXattr { name } | Op::RemoveXattr { name } => name.len(),
      _ => 0,
    });
    std::mem::size_of::<OpRecord>().saturating_add(paths)
  }
}

/// What one consuming round found waiting after its cursor (§4.5: the budget's mutation rate × lag × record size,
/// measured directly as the records that waited).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backlog {
  /// Records after the round's cursor: the head less the cursor, exact even when retention dropped some, since
  /// sequences are contiguous.
  pub records: u64,
  /// Their charge to the budget in bytes: the retained ones' own charges, and the dropped ones at the log's mean charge.
  pub bytes: u64,
  /// How long the oldest of them had waited when the round ran, in nanoseconds. A lower bound when the round was an
  /// overrun: its oldest record was dropped, and this is the oldest retained one's wait.
  pub lag_ns: u64,
  /// Whether retention had dropped records after the cursor before the round ran (A-141's overrun).
  pub overrun: bool,
}

/// What the log's consuming rounds have found since it was made: the inputs §4.5 derives the budget from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalMeasure {
  /// Rounds that consumed the log.
  pub rounds: u64,
  /// Rounds that found their cursor behind retention.
  pub overruns: u64,
  /// The round that found the most bytes waiting.
  pub largest: Backlog,
  /// The longest any round's oldest record had waited, in nanoseconds.
  pub longest_lag_ns: u64,
}

/// An open reader of the log ([`OpLog::open_reader`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reader {
  /// The point it may still read every record after.
  point: u64,
  /// How long a record may wait for it before it is presumed gone, so the record no longer holds the log's budget
  /// ([`OpLog::append`]).
  patience_ns: u64,
}

/// The log.
#[derive(Debug)]
pub struct OpLog {
  records: VecDeque<OpRecord>,
  bytes: usize,
  budget_bytes: usize,
  next_seq: u64,
  dropped: u64,
  measure: JournalMeasure,
  /// The readers that consume the log, by key ([`OpLog::open_reader`]). One entry per open reader; its owner opens one
  /// only for a live transport attachment and closes it when the attachment ends, so the attachment registry's
  /// capacity bounds the map.
  readers: BTreeMap<u64, Reader>,
  /// The bytes the shard's metadata ledger granted the budget beyond its floor: returned by the volume's owner at its
  /// teardown.
  grown: u64,
  /// Growths the ledger refused, each overrunning the reader that still owed the record dropped.
  growth_refused: u64,
}

impl OpLog {
  /// A log retaining up to `budget_bytes` of records.
  pub fn new(budget_bytes: usize) -> Self {
    Self {
      records: VecDeque::new(),
      bytes: 0,
      budget_bytes,
      next_seq: 1,
      dropped: 0,
      measure: JournalMeasure::default(),
      readers: BTreeMap::new(),
      grown: 0,
      growth_refused: 0,
    }
  }

  /// Opens reader `key` at `point` with `patience_ns`: until it closes, the reader may read every record after `point`,
  /// and each of its rounds moves `point` to the round's start ([`OpLog::note_round`]). A record it has not read holds
  /// the log's budget until it has waited `patience_ns` ([`OpLog::append`]). Opening a key already open moves its point.
  pub fn open_reader(&mut self, key: u64, point: u64, patience_ns: u64) {
    self.readers.insert(key, Reader { point, patience_ns });
  }

  /// Closes reader `key`; a key that is not open is left alone.
  pub fn close_reader(&mut self, key: u64) {
    self.readers.remove(&key);
  }

  /// The readers open.
  pub fn readers(&self) -> usize {
    self.readers.len()
  }

  /// The earliest point an open reader may still read after, or `None` when no reader is open.
  pub fn slowest_reader(&self) -> Option<u64> {
    self.readers.values().map(|reader| reader.point).min()
  }

  /// Whether record `seq`, which has waited `waited_ns`, is still owed to an open reader that is not yet presumed gone:
  /// one that has not read past it, and whose patience the record has not outlasted.
  fn owed(&self, seq: u64, waited_ns: u64) -> bool {
    self
      .readers
      .values()
      .any(|reader| reader.point < seq && waited_ns < reader.patience_ns)
  }

  /// The bytes the shard's metadata ledger granted the budget beyond its floor, which the volume's owner returns at its
  /// teardown.
  pub fn grown(&self) -> u64 {
    self.grown
  }

  /// Growths the ledger refused: each dropped a record a live reader still owed, which that reader overran.
  pub fn growth_refused(&self) -> u64 {
    self.growth_refused
  }

  /// Records what a consuming round of `reader` finds waiting after `cursor` at monotonic time `now_ns`, on the same
  /// clock the records were stamped with, and returns it. An open reader's point moves to `cursor`: a round never
  /// reads before its own start again, since a refused round is gathered again from that start and nothing new is
  /// gathered while one is in flight (§4.5). A reader that is not open is measured and not registered, so no round
  /// opens a reader its owner would have to close. Costs the records after the cursor, which the round walks anyway.
  pub fn note_round(&mut self, reader: u64, cursor: u64, now_ns: u64) -> Backlog {
    if let Some(open) = self.readers.get_mut(&reader) {
      open.point = cursor;
    }
    let backlog = self.backlog_after(cursor, now_ns);
    let measure = &mut self.measure;
    measure.rounds = measure.rounds.saturating_add(1);
    if backlog.overrun {
      measure.overruns = measure.overruns.saturating_add(1);
    }
    if backlog.bytes > measure.largest.bytes {
      measure.largest = backlog;
    }
    measure.longest_lag_ns = measure.longest_lag_ns.max(backlog.lag_ns);
    backlog
  }

  /// What waits after `cursor` at `now_ns` ([`OpLog::note_round`] without recording it).
  fn backlog_after(&self, cursor: u64, now_ns: u64) -> Backlog {
    let records = self.head_seq().saturating_sub(cursor);
    if records == 0 {
      return Backlog::default();
    }
    let first_after = cursor.saturating_add(1);
    let dropped = self.oldest_seq().saturating_sub(first_after).min(records);
    let oldest_at = self.since(cursor).next().map(|record| record.at);
    let retained_bytes = self
      .since(cursor)
      .map(|record| charge(record.bytes()))
      .fold(0, u64::saturating_add);
    Backlog {
      records,
      bytes: retained_bytes.saturating_add(dropped.saturating_mul(self.mean_record_bytes())),
      lag_ns: oldest_at.map_or(0, |at| now_ns.saturating_sub(at)),
      overrun: dropped > 0,
    }
  }

  /// What the log's rounds have found since it was made.
  pub fn measure(&self) -> JournalMeasure {
    self.measure
  }

  /// The mean charge of the records retained, in bytes; a record's fixed size when none is.
  pub fn mean_record_bytes(&self) -> u64 {
    let fixed = charge(std::mem::size_of::<OpRecord>());
    let retained = u64::try_from(self.records.len()).unwrap_or(u64::MAX);
    charge(self.bytes).checked_div(retained).unwrap_or(fixed)
  }

  /// The bytes of records the log may retain.
  pub fn budget_bytes(&self) -> usize {
    self.budget_bytes
  }

  /// The bytes the retained records are charged.
  pub fn retained_bytes(&self) -> usize {
    self.bytes
  }

  /// Appends a record and returns the sequence assigned. While the budget is full the oldest leaves, unless a live
  /// reader still owes it ([`OpLog::open_reader`]): then the budget grows by what the incoming record needs, reserved
  /// from `ledger`, the shard's metadata ledger. A refused growth keeps the budget and drops the oldest, counted once an
  /// append ([`OpLog::growth_refused`]).
  #[allow(clippy::too_many_arguments)] // every field of the record it makes, and the ledger it may grow from
  pub fn append(
    &mut self,
    ledger: &mut MetadataBudget,
    op: Op,
    path: impl Into<Box<str>>,
    inode: Option<InodeNo>,
    epoch: Epoch,
    at: u64,
    prev_version: u64,
  ) -> u64 {
    let seq = self.next_seq;
    self.next_seq = self.next_seq.saturating_add(1);
    let record = OpRecord {
      seq,
      op,
      path: path.into(),
      inode,
      epoch,
      at,
      prev_version,
    };
    let incoming = record.bytes();
    // The oldest leave before the newest lands, so the ring never holds more than the budget (the newest always stays).
    let mut asked = false;
    while self.bytes.saturating_add(incoming) > self.budget_bytes {
      let Some(oldest) = self.records.front() else {
        break;
      };
      if !asked && self.owed(oldest.seq, at.saturating_sub(oldest.at)) {
        asked = true;
        let short = self
          .bytes
          .saturating_add(incoming)
          .saturating_sub(self.budget_bytes);
        if ledger.reserve(charge(short)).is_ok() {
          self.budget_bytes = self.budget_bytes.saturating_add(short);
          self.grown = self.grown.saturating_add(charge(short));
          continue;
        }
        self.growth_refused = self.growth_refused.saturating_add(1);
      }
      let Some(old) = self.records.pop_front() else {
        break;
      };
      self.bytes = self.bytes.saturating_sub(old.bytes());
      self.dropped = self.dropped.saturating_add(1);
    }
    self.bytes = self.bytes.saturating_add(incoming);
    self.reserve_within_budget();
    self.records.push_back(record);
    seq
  }

  /// Grows the ring, when full, by doubling but never past the records the budget can hold: every record is charged
  /// at least its fixed size and the oldest leave before the newest lands, so `budget_bytes / size_of::<OpRecord>()`
  /// records is the most retention ever keeps (one, when a single record is larger than the budget). Plain doubling let the allocation reach twice the budget the
  /// volume is charged for (§4.2; 2026-10-04: a 43 MB budget's ring had grown to a 33.8 MB allocation on its way to
  /// 64 MB).
  fn reserve_within_budget(&mut self) {
    let len = self.records.len();
    if len < self.records.capacity() {
      return;
    }
    let most = self
      .budget_bytes
      .checked_div(std::mem::size_of::<OpRecord>())
      .unwrap_or(0)
      .max(len.saturating_add(1));
    let target = len.saturating_mul(2).max(1).min(most);
    self.records.reserve_exact(target.saturating_sub(len));
  }

  /// Records since `seq` (exclusive), oldest first. Sequences are contiguous (each append takes the next, and
  /// retention only drops the oldest), so the first record after `seq` is found by its distance from the oldest
  /// retained one: the cost is the records returned, not the log (A-96: a filter over the whole retained log was a
  /// quarter of a FUSE create's daemon time, since every reply asks for the changes since its cursor).
  pub fn since(&self, seq: u64) -> impl Iterator<Item = &OpRecord> {
    let oldest = self.oldest_seq();
    let skip = usize::try_from(seq.saturating_add(1).saturating_sub(oldest))
      .unwrap_or(usize::MAX)
      .min(self.records.len());
    self.records.range(skip..)
  }

  /// The newest sequence assigned.
  pub fn head_seq(&self) -> u64 {
    self.next_seq.saturating_sub(1)
  }

  /// The oldest sequence still retained, or the next to be assigned when nothing is: every record from here to
  /// [`OpLog::head_seq`] is held (sequences are contiguous and retention drops only the oldest), and any before it is
  /// gone. A reader that needs every record since a point asks this to know whether it still can.
  pub fn oldest_seq(&self) -> u64 {
    self
      .records
      .front()
      .map_or(self.next_seq, |record| record.seq)
  }

  /// The most records retention ever keeps: every record is charged at least its fixed size, so
  /// `budget_bytes / size_of::<OpRecord>()`, and one when a single record is larger than the budget. A reader that
  /// tracks sequences of its own bounds them by this: more than it are more than the log can still be asked about.
  pub fn capacity(&self) -> usize {
    self
      .budget_bytes
      .checked_div(std::mem::size_of::<OpRecord>())
      .unwrap_or(0)
      .max(1)
  }

  /// Records dropped by retention.
  pub fn dropped(&self) -> u64 {
    self.dropped
  }

  /// Records retained.
  pub fn len(&self) -> usize {
    self.records.len()
  }

  /// Whether nothing is retained.
  pub fn is_empty(&self) -> bool {
    self.records.is_empty()
  }
}

/// A charge in bytes as the measurement's width (`usize` → `u64`, saturating where a `usize` is wider).
fn charge(bytes: usize) -> u64 {
  u64::try_from(bytes).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A ledger that admits every growth: for the tests of what retention keeps with no reader to grow it for.
  fn unbounded() -> MetadataBudget {
    MetadataBudget::new(u64::MAX)
  }

  #[test]
  fn records_are_sequenced_and_the_oldest_leave_at_the_budget() {
    let mut log = OpLog::new(3 * std::mem::size_of::<OpRecord>() + 30);
    for i in 0..5u64 {
      log.append(
        &mut unbounded(),
        Op::Create,
        format!("f{i}"),
        Some(InodeNo(i)),
        Epoch(0),
        i,
        0,
      );
    }
    assert_eq!(log.head_seq(), 5);
    assert!(log.dropped() >= 1, "{}", log.dropped());
    let seqs: Vec<u64> = log.since(0).map(|r| r.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    assert_eq!(seqs.last(), Some(&5));
    assert_eq!(log.since(4).count(), 1);
  }

  /// A-96: do append past the budget so the oldest leave, then ask for the records since every sequence from before
  /// the oldest retained to past the newest; expect exactly the retained records with a later sequence, in order (the
  /// filter the indexed start replaced is the oracle).
  #[test]
  fn records_since_any_sequence_are_the_retained_ones_after_it() {
    let mut log = OpLog::new(40 * std::mem::size_of::<OpRecord>());
    for i in 0..200u64 {
      log.append(
        &mut unbounded(),
        Op::Create,
        "",
        Some(InodeNo(i)),
        Epoch(0),
        i,
        0,
      );
    }
    assert!(log.dropped() > 0, "the oldest left");
    for seq in 0..=log.head_seq() + 2 {
      let indexed: Vec<u64> = log.since(seq).map(|r| r.seq).collect();
      let filtered: Vec<u64> = log
        .records
        .iter()
        .filter(|r| r.seq > seq)
        .map(|r| r.seq)
        .collect();
      assert_eq!(indexed, filtered, "since {seq}");
    }
    assert_eq!(OpLog::new(64).since(0).count(), 0, "an empty log");
  }

  /// The fixed part of every record's charge, as the measurement's width.
  fn fixed() -> u64 {
    charge(std::mem::size_of::<OpRecord>())
  }

  /// Shape: a reader key no test opens, for rounds measured without a reader.
  const UNOPENED: u64 = 0;

  /// §4.5 (the journal's readers): do open two readers at different points, run a round of one, a round of a key
  /// never opened, then close them; expect each round to move only its own reader's point to the round's start, the
  /// slowest point to be the least open one, the unopened key to register nothing, and closing to forget a reader
  /// (twice is harmless).
  #[test]
  fn a_round_moves_only_its_own_readers_point_and_only_an_opened_reader_is_kept() {
    let mut log = OpLog::new(64 * std::mem::size_of::<OpRecord>());
    for i in 0..6u64 {
      log.append(
        &mut unbounded(),
        Op::Create,
        "",
        Some(InodeNo(i)),
        Epoch(0),
        i,
        0,
      );
    }
    log.open_reader(7, 2, u64::MAX);
    log.open_reader(9, 4, u64::MAX);
    assert_eq!((log.readers(), log.slowest_reader()), (2, Some(2)));
    log.note_round(7, 5, 10);
    assert_eq!(
      log.slowest_reader(),
      Some(4),
      "reader 7 moved to its round's start"
    );
    log.note_round(UNOPENED, 1, 10);
    assert_eq!(
      log.readers(),
      2,
      "a round of a reader never opened registers none"
    );
    log.close_reader(9);
    assert_eq!((log.readers(), log.slowest_reader()), (1, Some(5)));
    log.close_reader(9);
    log.close_reader(7);
    assert_eq!((log.readers(), log.slowest_reader()), (0, None));
  }

  /// §4.5 (the budget's inputs): do append five records with known path lengths and times, then run a round from the
  /// second; expect a backlog of the three after it with their exact charges, and the wait of the oldest of them.
  #[test]
  fn a_round_within_retention_records_its_backlog_exactly() {
    let mut log = OpLog::new(64 * std::mem::size_of::<OpRecord>());
    for i in 0..5u64 {
      let path = "x".repeat(usize::try_from(i).unwrap());
      log.append(
        &mut unbounded(),
        Op::Create,
        path,
        Some(InodeNo(i)),
        Epoch(0),
        100 * i,
        0,
      );
    }
    let backlog = log.note_round(UNOPENED, 2, 1_000);
    assert_eq!(
      backlog,
      Backlog {
        records: 3,
        bytes: 3 * fixed() + 2 + 3 + 4,
        lag_ns: 1_000 - 200,
        overrun: false,
      }
    );
    assert_eq!(log.measure().rounds, 1);
    assert_eq!(log.measure().largest, backlog);
    assert_eq!(log.measure().longest_lag_ns, 800);
  }

  /// §4.5 and A-141: do append ten records into a log that keeps three, then run a round from the first; expect a
  /// backlog of the nine after it (contiguous sequences), the six dropped ones at the mean charge, a lag measured from
  /// the oldest retained record as a lower bound, and the round counted as an overrun.
  #[test]
  fn a_round_behind_retention_records_the_whole_backlog_and_marks_the_overrun() {
    let mut log = OpLog::new(3 * std::mem::size_of::<OpRecord>());
    for i in 0..10u64 {
      log.append(
        &mut unbounded(),
        Op::Create,
        "",
        Some(InodeNo(i)),
        Epoch(0),
        100 * i,
        0,
      );
    }
    assert_eq!(log.oldest_seq(), 8, "three retained");
    let backlog = log.note_round(UNOPENED, 1, 2_000);
    assert_eq!(
      backlog,
      Backlog {
        records: 9,
        bytes: 3 * fixed() + 6 * fixed(),
        lag_ns: 2_000 - 700,
        overrun: true,
      }
    );
    assert_eq!(log.measure().overruns, 1);
  }

  /// §4.5: do run a round at the head, then a larger one, then a smaller one; expect the empty round counted with
  /// nothing waiting, the largest backlog kept by bytes, and the longest lag the longest seen.
  #[test]
  fn rounds_keep_the_largest_backlog_and_the_longest_lag() {
    let mut log = OpLog::new(64 * std::mem::size_of::<OpRecord>());
    assert_eq!(
      log.note_round(UNOPENED, 0, 5),
      Backlog::default(),
      "nothing appended"
    );
    for i in 0..6u64 {
      log.append(
        &mut unbounded(),
        Op::Create,
        "",
        Some(InodeNo(i)),
        Epoch(0),
        10 * i,
        0,
      );
    }
    let large = log.note_round(UNOPENED, 0, 100);
    let small = log.note_round(UNOPENED, 5, 1_000);
    assert_eq!(large.records, 6);
    assert_eq!(small.records, 1);
    let measure = log.measure();
    assert_eq!(measure.rounds, 3);
    assert_eq!(measure.largest, large);
    assert_eq!(
      measure.longest_lag_ns,
      1_000 - 50,
      "the small round's oldest record waited longest"
    );
    assert_eq!(measure.overruns, 0);
  }

  /// One step of a generated history: an append with its path length and the time since the last step, or a round
  /// that many records behind the head.
  #[derive(Clone, Debug)]
  enum Step {
    Append { path: usize, after_ns: u64 },
    Round { behind: u64, after_ns: u64 },
  }

  #[allow(clippy::disallowed_types)] // proptest's `prop_oneof!` holds its arms in an `Arc` (D-8's test-harness exception).
  fn step() -> impl proptest::strategy::Strategy<Value = Step> {
    use proptest::prelude::*;
    prop_oneof![
      3 => (0..48usize, 0..1_000u64).prop_map(|(path, after_ns)| Step::Append { path, after_ns }),
      1 => (0..80u64, 0..1_000u64).prop_map(|(behind, after_ns)| Step::Round { behind, after_ns }),
    ]
  }

  /// The log as §4.5 states it, kept independently of [`OpLog`]: every record appended, with its time and charge;
  /// retention drops the oldest while the incoming record would not fit, and the newest always stays.
  struct Model {
    all: Vec<(u64, u64, u64)>,
    oldest: usize,
    retained: u64,
    budget: u64,
    measure: JournalMeasure,
  }

  impl Model {
    fn append(&mut self, at: u64, cost: u64) {
      let seq = charge(self.all.len()) + 1;
      while self.retained + cost > self.budget && self.oldest < self.all.len() {
        self.retained -= self.all[self.oldest].2;
        self.oldest += 1;
      }
      self.retained += cost;
      self.all.push((seq, at, cost));
    }

    fn round(&mut self, cursor: u64, now: u64) -> Backlog {
      let head = charge(self.all.len());
      let records = head.saturating_sub(cursor);
      let backlog = if records == 0 {
        Backlog::default()
      } else {
        let kept = &self.all[self.oldest..];
        let mean = if kept.is_empty() {
          fixed()
        } else {
          self.retained / charge(kept.len())
        };
        let oldest_seq = kept.first().map_or(head + 1, |r| r.0);
        let dropped = oldest_seq.saturating_sub(cursor + 1).min(records);
        let after: Vec<_> = kept.iter().filter(|r| r.0 > cursor).collect();
        Backlog {
          records,
          bytes: after.iter().map(|r| r.2).sum::<u64>() + dropped * mean,
          lag_ns: after.first().map_or(0, |r| now.saturating_sub(r.1)),
          overrun: dropped > 0,
        }
      };
      self.measure.rounds += 1;
      self.measure.overruns += u64::from(backlog.overrun);
      if backlog.bytes > self.measure.largest.bytes {
        self.measure.largest = backlog;
      }
      self.measure.longest_lag_ns = self.measure.longest_lag_ns.max(backlog.lag_ns);
      backlog
    }
  }

  proptest::proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::prelude::ProptestConfig { cases: slates_test_seeds::cases(2000), .. proptest::prelude::ProptestConfig::default() }))]

    /// §4.5 (the budget's inputs), oracle: do any history of appends and rounds against any budget; expect every
    /// round's backlog, and the log's maxima after it, to equal the model's, and retention after every append to keep
    /// the model's records (the measurement changes nothing about what is retained).
    #[test]
    fn every_rounds_backlog_equals_the_models(
      budget_records in 1..12usize,
      extra in 0..std::mem::size_of::<OpRecord>(),
      steps in proptest::collection::vec(step(), 1..120),
    ) {
      let budget = budget_records * std::mem::size_of::<OpRecord>() + extra;
      let mut log = OpLog::new(budget);
      let mut model = Model { all: Vec::new(), oldest: 0, retained: 0, budget: charge(budget), measure: JournalMeasure::default() };
      let mut now = 0u64;
      for step in steps {
        match step {
          Step::Append { path, after_ns } => {
            now += after_ns;
            log.append(&mut unbounded(), Op::Create, "p".repeat(path), None, Epoch(0), now, 0);
            model.append(now, fixed() + charge(path));
            proptest::prop_assert_eq!(log.len(), model.all.len() - model.oldest);
            proptest::prop_assert_eq!(log.oldest_seq(), model.all.get(model.oldest).map_or(charge(model.all.len()) + 1, |r| r.0));
            proptest::prop_assert_eq!(charge(log.retained_bytes()), model.retained);
          }
          Step::Round { behind, after_ns } => {
            now += after_ns;
            let cursor = log.head_seq().saturating_sub(behind);
            proptest::prop_assert_eq!(log.note_round(UNOPENED, cursor, now), model.round(cursor, now));
            proptest::prop_assert_eq!(log.measure(), model.measure);
          }
        }
      }
    }
  }

  /// One step of a generated retention history (A-157): an append with its path length after some time, a reader opened
  /// some records behind the head with a patience, a reader's round moving its point forward, or a reader closed.
  #[derive(Clone, Debug)]
  enum Retain {
    Append {
      path: usize,
      after_ns: u64,
    },
    Open {
      key: u64,
      behind: u64,
      patience_ns: u64,
    },
    Round {
      key: u64,
      forward: u64,
    },
    Close {
      key: u64,
    },
  }

  #[allow(clippy::disallowed_types)] // proptest's `prop_oneof!` holds its arms in an `Arc` (D-8's test-harness exception).
  fn retain() -> impl proptest::strategy::Strategy<Value = Retain> {
    use proptest::prelude::*;
    prop_oneof![
      6 => (0..48usize, 0..400u64).prop_map(|(path, after_ns)| Retain::Append { path, after_ns }),
      1 => (0..3u64, 0..6u64, 1..4_000u64).prop_map(|(key, behind, patience_ns)| Retain::Open { key, behind, patience_ns }),
      2 => (0..3u64, 0..12u64).prop_map(|(key, forward)| Retain::Round { key, forward }),
      1 => (0..3u64).prop_map(|key| Retain::Close { key }),
    ]
  }

  /// A-157's retention rule, kept apart from [`OpLog`]: records (sequence, time, charge) in order; readers by key with
  /// their point and patience; a budget, and a ledger's room. While the incoming record does not fit, the oldest leaves,
  /// unless an open reader that has not read past it is still within its patience of it: then, once an append, the
  /// budget grows by the shortfall if the room holds it, and a refusal is counted.
  struct RetentionModel {
    retained: VecDeque<(u64, u64, u64)>,
    /// Every record appended, (sequence, time), retained or not: what the direct check walks.
    all: Vec<(u64, u64)>,
    next: u64,
    bytes: u64,
    budget: u64,
    room: u64,
    grown: u64,
    refused: u64,
    /// Key → (point, patience, the oldest record retained when it opened): records before that had left already.
    readers: BTreeMap<u64, (u64, u64, u64)>,
  }

  impl RetentionModel {
    fn append(&mut self, at: u64, cost: u64) {
      let seq = self.next;
      self.next += 1;
      let mut asked = false;
      while self.bytes + cost > self.budget {
        let Some(&(oldest, oldest_at, oldest_cost)) = self.retained.front() else {
          break;
        };
        let owed = self
          .readers
          .values()
          .any(|&(point, patience, _)| point < oldest && at - oldest_at < patience);
        if !asked && owed {
          asked = true;
          let short = self.bytes + cost - self.budget;
          if short <= self.room {
            self.room -= short;
            self.budget += short;
            self.grown += short;
            continue;
          }
          self.refused += 1;
        }
        self.retained.pop_front();
        self.bytes -= oldest_cost;
      }
      self.bytes += cost;
      self.retained.push_back((seq, at, cost));
      self.all.push((seq, at));
    }

    fn head(&self) -> u64 {
      self.next - 1
    }
  }

  proptest::proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::prelude::ProptestConfig { cases: slates_test_seeds::cases(2000), .. proptest::prelude::ProptestConfig::default() }))]

    /// A-157 (the journal keeps what a live reader still needs, growing from the ledger), oracle: do any history of
    /// appends, readers opened, advanced and closed, over any floor and any ledger room; expect after every step the
    /// retained records, the budget, the growth and the refusals to equal the model's, and the ledger's committed bytes
    /// to equal the growth. While no growth was refused, no record an open reader has not read and is within its
    /// patience of is gone. Returning the growth at the end leaves the ledger as it began.
    #[test]
    fn retention_keeps_what_a_live_reader_needs_as_the_model_does(
      floor_records in 1..4usize,
      room_records in 0..24u64,
      steps in proptest::collection::vec(retain(), 1..160),
    ) {
      let fixed_size = std::mem::size_of::<OpRecord>();
      let floor = floor_records * fixed_size;
      let room = room_records * fixed();
      let mut ledger = MetadataBudget::new(room);
      let mut log = OpLog::new(floor);
      let mut model = RetentionModel {
        retained: VecDeque::new(),
        all: Vec::new(),
        next: 1,
        bytes: 0,
        budget: charge(floor),
        room,
        grown: 0,
        refused: 0,
        readers: BTreeMap::new(),
      };
      let mut now = 0u64;
      for step in steps {
        match step {
          Retain::Append { path, after_ns } => {
            now += after_ns;
            log.append(&mut ledger, Op::Create, "p".repeat(path), None, Epoch(0), now, 0);
            model.append(now, fixed() + charge(path));
          }
          Retain::Open { key, behind, patience_ns } => {
            let point = log.head_seq().saturating_sub(behind);
            log.open_reader(key, point, patience_ns);
            model.readers.insert(key, (point, patience_ns, log.oldest_seq()));
          }
          // A reader's point only moves forward, as a mount's does: a refused round is gathered again from its start.
          Retain::Round { key, forward } => {
            if let Some(&(point, patience, floor)) = model.readers.get(&key) {
              let cursor = (point + forward).min(model.head());
              log.note_round(key, cursor, now);
              model.readers.insert(key, (cursor, patience, floor));
            }
          }
          Retain::Close { key } => {
            log.close_reader(key);
            model.readers.remove(&key);
          }
        }
        let retained: Vec<u64> = log.since(0).map(|record| record.seq).collect();
        let expected: Vec<u64> = model.retained.iter().map(|record| record.0).collect();
        proptest::prop_assert_eq!(retained, expected);
        proptest::prop_assert_eq!(charge(log.budget_bytes()), model.budget);
        proptest::prop_assert_eq!(log.grown(), model.grown);
        proptest::prop_assert_eq!(log.growth_refused(), model.refused);
        proptest::prop_assert_eq!(ledger.committed(), model.grown);
        // The rule's promise, checked apart from the model: with every growth admitted, no record an open reader has not
        // read, appended while it was open (at or after its floor) and still within its patience, has left. A record within
        // a reader's patience now was within it at every earlier append, and a reader's point only moves forward.
        if model.refused == 0 {
          let oldest = log.oldest_seq();
          for &(point, patience, floor) in model.readers.values() {
            for &(seq, at) in &model.all {
              let owed = seq > point && seq >= floor && now - at < patience;
              proptest::prop_assert!(!owed || seq >= oldest, "record {} owed to a reader at {} has left", seq, point);
            }
          }
        }
      }
      ledger.release(slates_mem::budget::MetadataCredit { bytes: log.grown() });
      proptest::prop_assert_eq!(ledger.committed(), 0);
    }
  }

  /// §4.2: do append far past the budget; expect the ring's allocation never to exceed the records the budget holds
  /// (doubling would reach twice that), with retention as before.
  #[test]
  fn the_ring_never_allocates_past_its_budget() {
    let budget = 1000 * std::mem::size_of::<OpRecord>();
    let mut log = OpLog::new(budget);
    for i in 0..10_000u64 {
      log.append(
        &mut unbounded(),
        Op::Create,
        "",
        Some(InodeNo(i)),
        Epoch(0),
        i,
        0,
      );
      assert!(
        log.records.capacity() <= 1000,
        "capacity {} past the budget's 1000 records",
        log.records.capacity()
      );
    }
    assert_eq!(log.head_seq(), 10_000);
    assert!(log.len() <= 1000 && log.dropped() >= 9_000);
  }
}
