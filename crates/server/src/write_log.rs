//! The FUSE writes acknowledged since the shard's last publication, kept whole in anchor-owned RAM (A-63; A-61;
//! §4.6 "Linux"; §4.8; D-18).
//!
//! A plain `write` through a FUSE mount is answered before the shard's recovery image is published: the image is
//! the shard's whole state, re-imaged at each barrier, far too costly per write. Before A-61 a daemon's death ended
//! its mounts, so a writer saw the loss at once. Once the anchor holds a mount across the death the writer runs on,
//! and the writes since the last publication must not vanish under it. So each write's bytes are appended here,
//! in a region of the shard's slice of the anchor's content object, before its reply: one copy into shared memory,
//! no system call. A publication that captured every volume empties the log and stamps it with that publication's
//! generation. A restarted daemon replays the log over the volumes its image rebuilt, but only when the log's
//! stamp equals the recovered image's generation: a log stamped earlier belongs to a publication that committed
//! after it (a death between a publication's commit and the clear), whose image already carries its writes, and
//! replaying them could resurrect bytes a later truncate removed.
//!
//! A log that cannot take a write forces a publication, which empties it (`crate::fuse`). Only when that is refused
//! does the log record that it overflowed: the next daemon then cannot replay every write, and reports every file a
//! taken-over mount had open as having lost writes (`slates_bridge_core::LostWrites`), never a silent hole.
//!
//! Layout (little-endian): the stamp (`u64`), the record bytes in use (`u64`), the overflow flag (`u32`), padding,
//! then the records, each the inode (`u64`), the offset (`u64`), the length (`u32`) and the bytes. A record is
//! written before the length that covers it, so a daemon killed mid-append leaves it uncovered, never half-read.

use slates_mem::SparseObject;

/// Format: where the stamp sits in the header.
const AT_STAMP: usize = 0;
/// Format: where the bytes in use sit.
const AT_USED: usize = size_of::<u64>();
/// Format: where the overflow flag sits.
const AT_OVERFLOW: usize = 2 * size_of::<u64>();
/// Format: the header's bytes, the three fields padded to a whole word.
const HEADER_BYTES: usize = 3 * size_of::<u64>();
/// Format: a record's head: the inode, the offset and the length.
const RECORD_HEAD: usize = 2 * size_of::<u64>() + size_of::<u32>();

/// The content object refused a span of the log (it would not back the memory): the caller counts it, and treats
/// the write as unlogged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unwritten;

/// Why a write was not appended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
  /// The log has no room for it: a publication empties it.
  Full,
  /// The content object refused the span.
  Unwritten,
}

/// One logged write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
  /// The file.
  pub(crate) inode: u64,
  /// Where the write began.
  pub(crate) offset: u64,
  /// The bytes written.
  pub(crate) bytes: Vec<u8>,
}

/// What a restarted daemon finds in the log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Recovered {
  /// The writes to replay, in the order they were acknowledged, when the log's stamp is the recovered image's.
  pub(crate) records: Vec<Record>,
  /// Whether some acknowledged write could not be logged (the log overflowed, or the header reads wrong), so any
  /// file a taken-over mount had open may have lost writes.
  pub(crate) everything_lost: bool,
}

/// The shard's write log: where it sits in the content object, its capacity, and the bytes in use.
#[derive(Debug)]
pub(crate) struct WriteLog {
  start: usize,
  capacity: usize,
  used: usize,
  stamp: u64,
}

fn word(object: &SparseObject, at: usize) -> Option<u64> {
  let mut bytes = [0u8; size_of::<u64>()];
  object.read(at, &mut bytes).ok()?;
  Some(u64::from_le_bytes(bytes))
}

impl WriteLog {
  /// The log of `bytes` at `start` in `object`, continued from what the previous daemon left when its stamp is
  /// `image_generation` (the writes since that publication, still owed), else emptied and stamped with it; with what
  /// it holds to replay. `None` when `bytes` cannot hold the header.
  pub(crate) fn open(
    object: &mut SparseObject,
    start: usize,
    bytes: usize,
    image_generation: u64,
  ) -> Option<(WriteLog, Recovered)> {
    let capacity = bytes.checked_sub(HEADER_BYTES)?;
    let mut log = WriteLog {
      start,
      capacity,
      used: 0,
      stamp: image_generation,
    };
    let recovered = log.read(object, image_generation);
    if recovered.records.is_empty() && !recovered.everything_lost {
      let _ = log.clear(object, image_generation);
    }
    Some((log, recovered))
  }

  /// Reads the previous daemon's records when its stamp is `image_generation`, leaving `used` at their end so new
  /// writes follow them.
  fn read(&mut self, object: &SparseObject, image_generation: u64) -> Recovered {
    let lost = Recovered {
      records: Vec::new(),
      everything_lost: true,
    };
    let at = |offset: usize| self.start.checked_add(offset);
    let (Some(stamp), Some(used), Some(overflow)) = (
      at(AT_STAMP).and_then(|at| word(object, at)),
      at(AT_USED).and_then(|at| word(object, at)),
      at(AT_OVERFLOW).and_then(|at| word(object, at)),
    ) else {
      return lost;
    };
    if stamp != image_generation {
      // A log from before the recovered publication: that image already carries its writes.
      return Recovered::default();
    }
    let Some(used) = usize::try_from(used)
      .ok()
      .filter(|used| *used <= self.capacity)
    else {
      return lost;
    };
    let mut records = Vec::new();
    let mut cursor = 0usize;
    while cursor < used {
      match self.record_at(object, cursor, used) {
        Some((record, next)) => {
          records.push(record);
          cursor = next;
        }
        None => return lost,
      }
    }
    self.used = used;
    Recovered {
      records,
      everything_lost: overflow != 0,
    }
  }

  /// The record at `cursor` of the records region and where the next begins, when it lies whole within `used`.
  fn record_at(
    &self,
    object: &SparseObject,
    cursor: usize,
    used: usize,
  ) -> Option<(Record, usize)> {
    let base = self.start.checked_add(HEADER_BYTES)?.checked_add(cursor)?;
    let inode = word(object, base)?;
    let offset = word(object, base.checked_add(size_of::<u64>())?)?;
    let mut length = [0u8; size_of::<u32>()];
    object
      .read(base.checked_add(2 * size_of::<u64>())?, &mut length)
      .ok()?;
    let length = usize::try_from(u32::from_le_bytes(length)).ok()?;
    let next = cursor.checked_add(RECORD_HEAD)?.checked_add(length)?;
    if next > used {
      return None;
    }
    let mut bytes = vec![0u8; length];
    object
      .read(base.checked_add(RECORD_HEAD)?, &mut bytes)
      .ok()?;
    Some((
      Record {
        inode,
        offset,
        bytes,
      },
      next,
    ))
  }

  /// Appends a write of `bytes` to `inode` at `offset`, before its reply: the record, then the length that covers it.
  pub(crate) fn append(
    &mut self,
    object: &mut SparseObject,
    inode: u64,
    offset: u64,
    bytes: &[u8],
  ) -> Result<(), Refused> {
    let length = u32::try_from(bytes.len()).map_err(|_| Refused::Full)?;
    let record = RECORD_HEAD.checked_add(bytes.len()).ok_or(Refused::Full)?;
    let end = self.used.checked_add(record).ok_or(Refused::Full)?;
    if end > self.capacity {
      return Err(Refused::Full);
    }
    let mut head = Vec::with_capacity(RECORD_HEAD);
    head.extend_from_slice(&inode.to_le_bytes());
    head.extend_from_slice(&offset.to_le_bytes());
    head.extend_from_slice(&length.to_le_bytes());
    let at = HEADER_BYTES.checked_add(self.used).ok_or(Refused::Full)?;
    self
      .store(object, at, &head)
      .map_err(|_| Refused::Unwritten)?;
    self
      .store(
        object,
        at.checked_add(RECORD_HEAD).ok_or(Refused::Full)?,
        bytes,
      )
      .map_err(|_| Refused::Unwritten)?;
    let covered = u64::try_from(end).map_err(|_| Refused::Full)?;
    self
      .store(object, AT_USED, &covered.to_le_bytes())
      .map_err(|_| Refused::Unwritten)?;
    self.used = end;
    Ok(())
  }

  /// Records that an acknowledged write could not be logged, so the next daemon reports every file as having lost
  /// writes rather than replaying an incomplete log.
  pub(crate) fn overflow(&mut self, object: &mut SparseObject) -> Result<(), Unwritten> {
    self.store(object, AT_OVERFLOW, &1u64.to_le_bytes())
  }

  /// Empties the log after a publication of generation `stamp` that captured every volume: every write before it is
  /// in that image now.
  pub(crate) fn clear(&mut self, object: &mut SparseObject, stamp: u64) -> Result<(), Unwritten> {
    self.used = 0;
    self.stamp = stamp;
    self.store(object, AT_USED, &0u64.to_le_bytes())?;
    self.store(object, AT_OVERFLOW, &0u64.to_le_bytes())?;
    self.store(object, AT_STAMP, &stamp.to_le_bytes())
  }

  fn store(&self, object: &mut SparseObject, offset: usize, bytes: &[u8]) -> Result<(), Unwritten> {
    let at = self.start.checked_add(offset).ok_or(Unwritten)?;
    object.write(at, bytes).map_err(|_| Unwritten)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a content object of a few pages, the log in its second page onwards.
  const OBJECT_BYTES: usize = 64 * 1024;
  /// Shape: the log's bytes.
  const LOG_BYTES: usize = 16 * 1024;
  /// Shape: where the log starts.
  const LOG_AT: usize = 4096;

  fn object(tag: &str) -> SparseObject {
    SparseObject::create(
      &format!("slates-write-log-test-{tag}-{}", std::process::id()),
      OBJECT_BYTES,
      slates_mem::Words::new(),
    )
    .unwrap()
  }

  /// A-63. Do: append three writes under the image of generation 7; open the log as a restarted daemon whose image is
  /// generation 7. Expect: the three writes back, in order, and new writes appended after them.
  #[test]
  fn the_writes_since_the_recovered_publication_are_replayed_in_order() {
    let mut shared = object("replay");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    log.append(&mut shared, 1, 0, b"alpha").unwrap();
    log.append(&mut shared, 2, 4096, b"beta").unwrap();
    log.append(&mut shared, 1, 5, b"gamma").unwrap();
    let (mut next, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    let written: Vec<(u64, u64, &[u8])> = recovered
      .records
      .iter()
      .map(|record| (record.inode, record.offset, record.bytes.as_slice()))
      .collect();
    assert_eq!(
      written,
      [(1, 0, &b"alpha"[..]), (2, 4096, b"beta"), (1, 5, b"gamma")]
    );
    assert!(!recovered.everything_lost);
    next.append(&mut shared, 3, 0, b"delta").unwrap();
    let (_, again) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    assert_eq!(
      again.records.len(),
      4,
      "a new write follows the replayed ones"
    );
  }

  /// A-63. Do: log writes under generation 7, then open as a daemon whose image is generation 8 (a publication that
  /// committed before the clear). Expect: nothing to replay — that image carries the writes — and the log emptied.
  #[test]
  fn a_log_from_before_the_recovered_publication_is_not_replayed() {
    let mut shared = object("stale");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    log
      .append(&mut shared, 1, 0, b"in the image already")
      .unwrap();
    let (_, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 8).unwrap();
    assert_eq!(recovered, Recovered::default());
    let (_, after) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 8).unwrap();
    assert!(after.records.is_empty());
  }

  /// A-63, crash and capacity. Do: a record stored past the length that covers it (a daemon killed mid-append); a
  /// write larger than the room left; an overflow marked. Expect: the uncovered record is not replayed; the large
  /// write is refused `Full`; the overflow reads as "everything lost".
  #[test]
  fn an_uncovered_record_is_ignored_a_full_log_refuses_and_an_overflow_is_reported() {
    let mut shared = object("edges");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 3).unwrap();
    log.append(&mut shared, 1, 0, b"covered").unwrap();
    let uncovered = LOG_AT + HEADER_BYTES + RECORD_HEAD + b"covered".len();
    shared.write(uncovered, &9u64.to_le_bytes()).unwrap();
    let (mut next, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 3).unwrap();
    assert_eq!(recovered.records.len(), 1);
    assert_eq!(
      next.append(&mut shared, 1, 0, &vec![0u8; LOG_BYTES]),
      Err(Refused::Full)
    );
    next.overflow(&mut shared).unwrap();
    let (_, overflowed) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 3).unwrap();
    assert!(overflowed.everything_lost);
  }

  /// A-63, hostile input. Do: a length in use past the capacity. Expect: "everything lost", never a short replay.
  #[test]
  fn an_impossible_length_in_use_reports_everything_lost() {
    let mut shared = object("hostile");
    let (_, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 5).unwrap();
    shared
      .write(LOG_AT + AT_USED, &u64::MAX.to_le_bytes())
      .unwrap();
    let (_, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 5).unwrap();
    assert!(recovered.everything_lost);
  }
}
