//! The op log (§4.5, "Journal"): every mutation appends a declared operation with its path(s),
//! inode, epoch, offset, length and the inode's version before it; bytes never enter the journal.
//! Retention is a byte budget derived from the measured mutation rate and the longest subscriber
//! lag; the oldest records leave first and the log says how many it dropped.

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
    let paths = self.path.len()
      + match &self.op {
        Op::Rename { from } | Op::Redirect { from } => from.len(),
        Op::SetXattr { name } | Op::RemoveXattr { name } => name.len(),
        _ => 0,
      };
    std::mem::size_of::<OpRecord>() + paths
  }
}

/// The log.
#[derive(Debug)]
pub struct OpLog {
  records: VecDeque<OpRecord>,
  bytes: usize,
  budget_bytes: usize,
  next_seq: u64,
  dropped: u64,
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
    }
  }

  /// Appends a record, evicting the oldest past the budget; returns the sequence assigned.
  pub fn append(
    &mut self,
    op: Op,
    path: &str,
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
      self.dropped += 1;
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
    let most = (self.budget_bytes / std::mem::size_of::<OpRecord>()).max(len.saturating_add(1));
    let target = len.saturating_mul(2).max(1).min(most);
    self.records.reserve_exact(target.saturating_sub(len));
  }

  /// Records since `seq` (exclusive), oldest first. Sequences are contiguous (each append takes the next, and
  /// retention only drops the oldest), so the first record after `seq` is found by its distance from the oldest
  /// retained one: the cost is the records returned, not the log (A-96: a filter over the whole retained log was a
  /// quarter of a FUSE create's daemon time, since every reply asks for the changes since its cursor).
  pub fn since(&self, seq: u64) -> impl Iterator<Item = &OpRecord> {
    let oldest = self
      .records
      .front()
      .map_or(self.next_seq, |record| record.seq);
    let skip = usize::try_from(seq.saturating_add(1).saturating_sub(oldest))
      .unwrap_or(usize::MAX)
      .min(self.records.len());
    self.records.range(skip..)
  }

  /// The newest sequence assigned.
  pub fn head_seq(&self) -> u64 {
    self.next_seq.saturating_sub(1)
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn records_are_sequenced_and_the_oldest_leave_at_the_budget() {
    let mut log = OpLog::new(3 * std::mem::size_of::<OpRecord>() + 30);
    for i in 0..5u64 {
      log.append(
        Op::Create,
        &format!("f{i}"),
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
