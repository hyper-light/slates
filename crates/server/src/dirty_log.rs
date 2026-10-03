//! The files written since the shard's last publication, kept in anchor-owned RAM (A-61; §4.6 "Linux"; D-18).
//!
//! A plain `write` through a FUSE mount is acknowledged before the shard's recovery image is published: like an
//! NFS `UNSTABLE` write, it is made stable by the `flush` or `fsync` that follows (D-18). Before A-61 a daemon's
//! death ended its mounts, so the writer saw the loss as an error at once. Once the anchor holds a mount's device
//! across the restart, the writer runs on: the writes the dead daemon had acknowledged since the last publication
//! are gone, and without a record of them a later `fsync` would report success over the hole. Found by the
//! in-flight takeover test (2026-10-03): page 99 of a file written through the kill came back wrong while the
//! file's length was right.
//!
//! So the first write to a file after a publication puts the file's inode number here, in a page of the shard's
//! slice of the anchor's content object, which outlives the daemon. A publication that captured every volume
//! empties it. A restarted daemon reads it before anything publishes: each file named lost writes the dead daemon
//! had acknowledged, and its mount answers the next `flush` or `fsync` through a handle opened before the restart
//! `EIO`, as Linux reports a writeback error (`crate::fuse`; `slates_bridge_core::LostWrites`). Inode numbers are
//! unique across a shard's volumes (each volume has its own prefix), so one log serves the shard.
//!
//! The cost is one in-memory set lookup per write and, for the first write to a file in an interval, one small
//! store into shared memory; no system call. A page holds `(page − 8) / 8` files; past it the log records
//! that it overflowed, and every file a restarted mount had open is then treated as having lost writes — the
//! conservative outcome, counted.
//!
//! Layout (little-endian): the entry count (`u32`), the overflow flag (`u32`), then that many inode numbers
//! (`u64`).

use std::collections::BTreeSet;

use slates_mem::SparseObject;

/// Format: the header's bytes: the entry count and the overflow flag.
const HEADER_BYTES: usize = 2 * size_of::<u32>();
/// Format: one entry's bytes, an inode number.
const ENTRY_BYTES: usize = size_of::<u64>();

/// The shared page could not be written (the content object refused the span): the file is then unknown to the
/// next daemon, and the caller counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unwritten;

/// What a restarted daemon learns from the log: the files whose acknowledged writes may be lost.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LostFiles {
  /// The files named.
  pub(crate) inodes: BTreeSet<u64>,
  /// Whether the log overflowed, so any file may have lost writes.
  pub(crate) everything: bool,
}

/// The shard's dirty log: where it sits in the content object, how many files it holds, and the files written
/// since the last publication (so each is logged once per interval).
#[derive(Debug)]
pub(crate) struct DirtyLog {
  start: usize,
  capacity: usize,
  written: BTreeSet<u64>,
  overflowed: bool,
}

impl DirtyLog {
  /// A log of `bytes` at `start` in the content object; `None` when `bytes` cannot hold the header and one entry.
  pub(crate) fn new(start: usize, bytes: usize) -> Option<DirtyLog> {
    let capacity = bytes.checked_sub(HEADER_BYTES)? / ENTRY_BYTES;
    (capacity > 0).then(|| DirtyLog {
      start,
      capacity,
      written: BTreeSet::new(),
      overflowed: false,
    })
  }

  fn word_at(&self, object: &SparseObject, offset: usize) -> Option<u32> {
    let mut bytes = [0u8; size_of::<u32>()];
    object
      .read(self.start.checked_add(offset)?, &mut bytes)
      .ok()?;
    Some(u32::from_le_bytes(bytes))
  }

  /// What the previous daemon left: the files it logged, or everything when it overflowed. A log that cannot be
  /// read, or whose count exceeds its capacity (a torn or foreign page), is treated as overflowed — never as
  /// empty, which would hide a loss.
  pub(crate) fn recovered(&self, object: &SparseObject) -> LostFiles {
    let (Some(count), Some(overflow)) = (
      self.word_at(object, 0),
      self.word_at(object, size_of::<u32>()),
    ) else {
      return LostFiles {
        inodes: BTreeSet::new(),
        everything: true,
      };
    };
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    if overflow != 0 || count > self.capacity {
      return LostFiles {
        inodes: BTreeSet::new(),
        everything: true,
      };
    }
    let mut inodes = BTreeSet::new();
    for index in 0..count {
      let mut bytes = [0u8; ENTRY_BYTES];
      let at = index
        .checked_mul(ENTRY_BYTES)
        .and_then(|offset| offset.checked_add(HEADER_BYTES))
        .and_then(|offset| offset.checked_add(self.start));
      match at.map(|at| object.read(at, &mut bytes)) {
        Some(Ok(())) => {
          inodes.insert(u64::from_le_bytes(bytes));
        }
        _ => {
          return LostFiles {
            inodes: BTreeSet::new(),
            everything: true,
          };
        }
      }
    }
    LostFiles {
      inodes,
      everything: false,
    }
  }

  /// Logs `inode` as written since the last publication, once per interval: the entry is stored before the count
  /// that covers it, so a daemon killed between the two leaves the entry uncounted, never a count past its
  /// entries. Past the capacity the overflow flag is set instead.
  pub(crate) fn mark(&mut self, object: &mut SparseObject, inode: u64) -> Result<(), Unwritten> {
    if self.overflowed || !self.written.insert(inode) {
      return Ok(());
    }
    let index = self.written.len().saturating_sub(1);
    if index >= self.capacity {
      self.overflowed = true;
      return self.store(object, size_of::<u32>(), &1u32.to_le_bytes());
    }
    let entry = index
      .checked_mul(ENTRY_BYTES)
      .and_then(|offset| offset.checked_add(HEADER_BYTES))
      .ok_or(Unwritten)?;
    self.store(object, entry, &inode.to_le_bytes())?;
    let count = u32::try_from(index.saturating_add(1)).map_err(|_| Unwritten)?;
    self.store(object, 0, &count.to_le_bytes())
  }

  /// Empties the log after a publication that captured every volume: nothing written before it can be lost now.
  pub(crate) fn clear(&mut self, object: &mut SparseObject) -> Result<(), Unwritten> {
    self.written.clear();
    self.overflowed = false;
    self.store(object, 0, &[0u8; HEADER_BYTES])
  }

  fn store(&self, object: &mut SparseObject, offset: usize, bytes: &[u8]) -> Result<(), Unwritten> {
    let at = self.start.checked_add(offset).ok_or(Unwritten)?;
    object.write(at, bytes).map_err(|_| Unwritten)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a content object of a few pages, the log in its second page as a shard's would be past others.
  const OBJECT_BYTES: usize = 4 * 4096;
  /// Shape: the log's page.
  const LOG_BYTES: usize = 4096;
  /// Shape: where the log starts.
  const LOG_AT: usize = 4096;

  fn object(tag: &str) -> SparseObject {
    SparseObject::create(
      &format!("slates-dirty-log-test-{tag}-{}", std::process::id()),
      OBJECT_BYTES,
      slates_mem::Words::new(),
    )
    .unwrap()
  }

  /// A-61. Do: log three writes to two files; read the log as a restarted daemon would. Expect: both files, each
  /// once; after a clear, none.
  #[test]
  fn the_files_written_since_a_publication_are_what_a_restarted_daemon_reads() {
    let mut shared = object("marks");
    let mut log = DirtyLog::new(LOG_AT, LOG_BYTES).unwrap();
    for inode in [7, 9, 7] {
      log.mark(&mut shared, inode).unwrap();
    }
    let next = DirtyLog::new(LOG_AT, LOG_BYTES).unwrap();
    assert_eq!(
      next.recovered(&shared),
      LostFiles {
        inodes: BTreeSet::from([7, 9]),
        everything: false
      }
    );
    log.clear(&mut shared).unwrap();
    assert_eq!(next.recovered(&shared), LostFiles::default());
  }

  /// A-61. Do: log one more file than the page holds. Expect: the restarted daemon reads "everything", never a
  /// partial list that would hide the files past the page.
  #[test]
  fn a_log_past_its_page_says_everything_may_be_lost() {
    let mut shared = object("overflow");
    let mut log = DirtyLog::new(LOG_AT, LOG_BYTES).unwrap();
    let past = u64::try_from(log.capacity).unwrap() + 1;
    for inode in 0..past {
      log.mark(&mut shared, inode).unwrap();
    }
    assert!(
      DirtyLog::new(LOG_AT, LOG_BYTES)
        .unwrap()
        .recovered(&shared)
        .everything
    );
  }

  /// A-61, crash and hostile input. Do: an entry stored without the count that covers it (a daemon killed between
  /// the two); then a count past the page's capacity (a torn or foreign page). Expect: the uncounted entry is not
  /// read; the impossible count reads as "everything", never as empty.
  #[test]
  fn an_uncounted_entry_is_ignored_and_an_impossible_count_is_everything() {
    let mut shared = object("torn");
    let mut log = DirtyLog::new(LOG_AT, LOG_BYTES).unwrap();
    log.mark(&mut shared, 5).unwrap();
    shared
      .write(LOG_AT + HEADER_BYTES + ENTRY_BYTES, &6u64.to_le_bytes())
      .unwrap();
    let next = DirtyLog::new(LOG_AT, LOG_BYTES).unwrap();
    assert_eq!(next.recovered(&shared).inodes, BTreeSet::from([5]));
    shared.write(LOG_AT, &u32::MAX.to_le_bytes()).unwrap();
    assert!(next.recovered(&shared).everything);
  }
}
