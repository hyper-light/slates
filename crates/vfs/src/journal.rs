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
    self.next_seq += 1;
    let record = OpRecord {
      seq,
      op,
      path: path.into(),
      inode,
      epoch,
      at,
      prev_version,
    };
    self.bytes += record.bytes();
    self.records.push_back(record);
    while self.bytes > self.budget_bytes && self.records.len() > 1 {
      if let Some(old) = self.records.pop_front() {
        self.bytes -= old.bytes();
        self.dropped += 1;
      }
    }
    seq
  }

  /// Records since `seq` (exclusive), oldest first.
  pub fn since(&self, seq: u64) -> impl Iterator<Item = &OpRecord> {
    self.records.iter().filter(move |r| r.seq > seq)
  }

  /// The newest sequence assigned.
  pub fn head_seq(&self) -> u64 {
    self.next_seq - 1
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
}
