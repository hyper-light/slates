//! The op log (§4.5, "Journal"): every mutation appends a declared operation with its path(s),
//! inode, epoch, offset, length and the inode's version before it; bytes never enter the journal.
//! Retention is a byte budget derived from the measured mutation rate and the longest subscriber
//! lag; the oldest records leave first and the log says how many it dropped.
//!
//! The log measures what its consumers find waiting ([`OpLog::note_round`]). §4.5 derives the budget as the mutation
//! rate times the longest subscriber lag times the record size. A round's backlog is that product measured directly:
//! by Little's law, the records waiting are the rate they arrived at times how long they waited, whatever the arrival
//! pattern. So each round records its backlog in records and bytes, and how long its oldest record had waited. The
//! log keeps the maxima since it was made; they are window-free, so the measurement adds no tuning constant (R3).

use std::collections::VecDeque;

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

/// The log.
#[derive(Debug)]
pub struct OpLog {
  records: VecDeque<OpRecord>,
  bytes: usize,
  budget_bytes: usize,
  next_seq: u64,
  dropped: u64,
  measure: JournalMeasure,
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
    }
  }

  /// Records what a consuming round finds waiting after `cursor` at monotonic time `now_ns`, on the same clock the
  /// records were stamped with, and returns it. Costs the records after the cursor, which the round walks anyway.
  pub fn note_round(&mut self, cursor: u64, now_ns: u64) -> Backlog {
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

  /// Appends a record, evicting the oldest past the budget; returns the sequence assigned.
  pub fn append(
    &mut self,
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
    while self.bytes.saturating_add(incoming) > self.budget_bytes {
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

  #[test]
  fn records_are_sequenced_and_the_oldest_leave_at_the_budget() {
    let mut log = OpLog::new(3 * std::mem::size_of::<OpRecord>() + 30);
    for i in 0..5u64 {
      log.append(
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
      log.append(Op::Create, "", Some(InodeNo(i)), Epoch(0), i, 0);
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

  /// §4.5 (the budget's inputs): do append five records with known path lengths and times, then run a round from the
  /// second; expect a backlog of the three after it with their exact charges, and the wait of the oldest of them.
  #[test]
  fn a_round_within_retention_records_its_backlog_exactly() {
    let mut log = OpLog::new(64 * std::mem::size_of::<OpRecord>());
    for i in 0..5u64 {
      let path = "x".repeat(usize::try_from(i).unwrap());
      log.append(Op::Create, path, Some(InodeNo(i)), Epoch(0), 100 * i, 0);
    }
    let backlog = log.note_round(2, 1_000);
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
      log.append(Op::Create, "", Some(InodeNo(i)), Epoch(0), 100 * i, 0);
    }
    assert_eq!(log.oldest_seq(), 8, "three retained");
    let backlog = log.note_round(1, 2_000);
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
    assert_eq!(log.note_round(0, 5), Backlog::default(), "nothing appended");
    for i in 0..6u64 {
      log.append(Op::Create, "", Some(InodeNo(i)), Epoch(0), 10 * i, 0);
    }
    let large = log.note_round(0, 100);
    let small = log.note_round(5, 1_000);
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
            log.append(Op::Create, "p".repeat(path), None, Epoch(0), now, 0);
            model.append(now, fixed() + charge(path));
            proptest::prop_assert_eq!(log.len(), model.all.len() - model.oldest);
            proptest::prop_assert_eq!(log.oldest_seq(), model.all.get(model.oldest).map_or(charge(model.all.len()) + 1, |r| r.0));
            proptest::prop_assert_eq!(charge(log.retained_bytes()), model.retained);
          }
          Step::Round { behind, after_ns } => {
            now += after_ns;
            let cursor = log.head_seq().saturating_sub(behind);
            proptest::prop_assert_eq!(log.note_round(cursor, now), model.round(cursor, now));
            proptest::prop_assert_eq!(log.measure(), model.measure);
          }
        }
      }
    }
  }

  /// §4.2: do append far past the budget; expect the ring's allocation never to exceed the records the budget holds
  /// (doubling would reach twice that), with retention as before.
  #[test]
  fn the_ring_never_allocates_past_its_budget() {
    let budget = 1000 * std::mem::size_of::<OpRecord>();
    let mut log = OpLog::new(budget);
    for i in 0..10_000u64 {
      log.append(Op::Create, "", Some(InodeNo(i)), Epoch(0), i, 0);
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
