//! The FUSE writes acknowledged since the shard's last publication, kept whole in anchor-owned RAM (A-63; A-61;
//! §4.6 "Linux"; §4.8; D-18): a FUSE mount's (Linux) and a Shards stream's (every platform: A-316, A-339, A-402).
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
//! A publication that leaves a volume out (one it cannot image: an overlay whose base host a landing holds) commits a
//! newer generation that does not carry that volume's writes since its last capture. So the log keeps exactly the
//! records of the volumes it left out, under that publication's stamp, and drops the rest, which its image carries and
//! whose replay over it could undo a later change ([`WriteLog::retain`], A-422). Before A-422 the log was left under
//! its older stamp, a restarted daemon found that stamp older than the committed image and replayed nothing, and the
//! left-out volume lost every write acknowledged since its last capture. The restamp follows the commit, so it is owed
//! from before the commit until it is done ([`WriteLog::owe_restamp`]): a daemon that dies between the two leaves a
//! stamp older than the committed image with the restamp owed, and the next one reports the writes lost rather than
//! drop them in silence.
//!
//! The log holds the `FORGET`s since the last publication too, a FUSE mount's and a Shards stream's whose resync end the
//! anchor holds (A-390): a `FORGET` gives back lookup references the kernel held, takes no reply, and is never resent,
//! so one served after the last publication and lost with its daemon left the references counted for good (GAPS, A-154's
//! sibling). A successor gives back each logged `FORGET` after the
//! writes logged before it. A `FORGET` the log cannot take forces a publication as a write does; a refused one is
//! counted, never marked as an overflow, since no write was lost.
//!
//! Every record names the volume it was written through, and a replay goes to that volume. An inode number does not name
//! its volume: a clone inherits its origin's inode numbers, the origin's prefix in their top bits, so a write a dead
//! daemon logged to a clone's inherited file, routed by its number's prefix, was replayed into the origin (or another clone
//! of it), and the clone lost it (A-166's sibling sweep; proven by
//! `a_replayed_write_to_a_clones_inherited_file_lands_in_the_clone`, 2026-11-18).
//!
//! Layout (little-endian): the stamp (`u64`), the record bytes in use (`u64`), the overflow word (`u64`: [`OVERFLOWED`]
//! and [`RESTAMP_OWED`]), the layout ([`LAYOUT`], `u64`), then the records, each its kind (`u32`), the inode (`u64`),
//! the offset (`u64`), the length (`u32`) and that many bytes: the volume's id (16 bytes), then a write's bytes, or a
//! `FORGET`'s attachment (`u64`) and count (`u64`) with the offset unused. A record is written before the length that
//! covers it, so a daemon killed mid-append leaves it uncovered, never half-read. A daemon reads only the log its own
//! image wrote: the anchor restarts its own executable (A-145); a log of another layout that holds records (an
//! upgrade's) is reported as lost writes, never misread.

use slates_mem::SparseObject;

/// Format: where the stamp sits in the header.
const AT_STAMP: usize = 0;
/// Format: where the bytes in use sit.
const AT_USED: usize = size_of::<u64>();
/// Format: where the overflow word sits.
const AT_OVERFLOW: usize = 2 * size_of::<u64>();
/// Format: the overflow word's bit saying an acknowledged write went unlogged since the last clear (the log overflowed
/// and the publication that would have emptied it was refused, or the content object refused a span): the next daemon
/// reports every taken-over file as having lost writes. The word's only bit before A-422, so an older log reads the
/// same.
const OVERFLOWED: u64 = 1;
/// Format: the overflow word's bit saying a publication that leaves a volume out may have committed past the stamp,
/// and the restamp that keeps that volume's records under the new stamp is not yet done (A-422,
/// [`WriteLog::owe_restamp`]). Read only with a stamp older than the recovered image: the writes the left-out volume
/// owed are then unknown, so they are reported lost. With a current stamp the bit says nothing (the publication was
/// refused, or its restamp was done and the bit not yet cleared).
const RESTAMP_OWED: u64 = 2;
/// Format: where the layout word sits.
const AT_LAYOUT: usize = 3 * size_of::<u64>();
/// Format: the header's bytes, its four words.
const HEADER_BYTES: usize = 4 * size_of::<u64>();
/// Format: the layout this daemon writes, "WLOG" and version 2: every record names its volume (2026-11-18). The layout
/// before it had no such word, and its first record began where this word sits, its kind (1 or 2) in the low half; this
/// word's low half is no record kind, so such a log is never read as this one's.
const LAYOUT: u64 = u64::from_le_bytes(*b"WLOG\x02\x00\x00\x00");
/// Format: a record's head: the kind, the inode, the offset and the length.
const RECORD_HEAD: usize = 2 * size_of::<u32>() + 2 * size_of::<u64>();
/// Format: a write record's kind.
const KIND_WRITE: u32 = 1;
/// Format: a `FORGET` record's kind.
const KIND_FORGET: u32 = 2;
/// Format: a volume's id in a record, its catalog id's bytes.
const VOLUME_BYTES: usize = 16;
/// Format: a `FORGET` record's bytes: the volume, the attachment and the count.
const FORGET_BYTES: usize = VOLUME_BYTES + 2 * size_of::<u64>();

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

/// What one [`WriteLog::purge`] gave back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Purged {
  /// The bytes the OS took (whole pages).
  pub(crate) bytes: usize,
  /// The bytes of the log walked, the slice's cost.
  pub(crate) walked: usize,
  /// Whether some of the log is still to give back.
  pub(crate) more: bool,
}

/// One logged write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
  /// The volume it was written through, its catalog id: where it is replayed.
  pub(crate) volume: [u8; VOLUME_BYTES],
  /// The file.
  pub(crate) inode: u64,
  /// Where the write began.
  pub(crate) offset: u64,
  /// The bytes written.
  pub(crate) bytes: Vec<u8>,
}

/// One logged `FORGET`: lookups the kernel gave back on a file through an attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Forgotten {
  /// The volume the lookups were given back on, its catalog id.
  pub(crate) volume: [u8; VOLUME_BYTES],
  /// The attachment, by its durable id: the owner of the references given back.
  pub(crate) attachment: u64,
  /// The file.
  pub(crate) inode: u64,
  /// The lookups given back.
  pub(crate) count: u64,
}

/// One record of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Logged {
  /// A write's bytes.
  Write(Record),
  /// A `FORGET`'s lookups.
  Forget(Forgotten),
}

/// What a restarted daemon finds in the log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Recovered {
  /// The writes and `FORGET`s to replay, in the order they were served, when the log's stamp is the recovered image's.
  pub(crate) records: Vec<Logged>,
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
  /// Whether an acknowledged write has gone unlogged since the last clear: then only a publication makes it survive.
  /// Read by the flush rule ([`WriteLog::holds_every_write`]) of a FUSE mount (Linux) and of a Shards stream (A-399).
  overflowed: bool,
  /// Whether a publication that leaves a volume out is about to commit, or has, and its restamp is not yet done
  /// ([`RESTAMP_OWED`], A-422).
  restamp_owed: bool,
  /// How far into the record area pages may be resident since they were last given back ([`WriteLog::purge`]): the
  /// furthest any record reached, or the whole area for a log opened over memory a predecessor may have written.
  written: usize,
}

fn word(object: &SparseObject, at: usize) -> Option<u64> {
  let mut bytes = [0u8; size_of::<u64>()];
  object.read(at, &mut bytes).ok()?;
  Some(u64::from_le_bytes(bytes))
}

/// A record's head as the log holds it: its kind, the inode and offset it names, and the length of the bytes after it.
struct RecordHead {
  kind: u32,
  inode: u64,
  offset: u64,
  length: usize,
}

/// The little-endian `u32` at `at`.
fn half_word(object: &SparseObject, at: usize) -> Option<u32> {
  let mut bytes = [0u8; size_of::<u32>()];
  object.read(at, &mut bytes).ok()?;
  Some(u32::from_le_bytes(bytes))
}

/// The record head at `base`, laid out as [`RECORD_HEAD`] counts it: kind, inode, offset, length.
fn head_at(object: &SparseObject, base: usize) -> Option<RecordHead> {
  let at_inode = base.checked_add(size_of::<u32>())?;
  let at_offset = at_inode.checked_add(size_of::<u64>())?;
  let at_length = at_offset.checked_add(size_of::<u64>())?;
  Some(RecordHead {
    kind: half_word(object, base)?,
    inode: word(object, at_inode)?,
    offset: word(object, at_offset)?,
    length: usize::try_from(half_word(object, at_length)?).ok()?,
  })
}

/// The record `head` introduces, holding `bytes`. `None` for a kind this daemon does not write or a `FORGET` of the
/// wrong length.
fn logged_of(head: &RecordHead, bytes: Vec<u8>) -> Option<Logged> {
  let (volume, rest) = bytes.split_at_checked(VOLUME_BYTES)?;
  let volume: [u8; VOLUME_BYTES] = volume.try_into().ok()?;
  match head.kind {
    KIND_WRITE => Some(Logged::Write(Record {
      volume,
      inode: head.inode,
      offset: head.offset,
      bytes: rest.to_vec(),
    })),
    KIND_FORGET if head.length == FORGET_BYTES => {
      let (attachment, count) = rest.split_at_checked(size_of::<u64>())?;
      Some(Logged::Forget(Forgotten {
        volume,
        attachment: u64::from_le_bytes(attachment.try_into().ok()?),
        inode: head.inode,
        count: u64::from_le_bytes(count.try_into().ok()?),
      }))
    }
    _ => None,
  }
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
      written: capacity,
      used: 0,
      stamp: image_generation,
      overflowed: false,
      restamp_owed: false,
    };
    let (recovered, restamp_lost) = log.read(object, image_generation);
    log.overflowed = recovered.everything_lost;
    if restamp_lost {
      // The records are of an image older than the recovered one, which left a volume out and committed before they
      // were restamped (A-422): unknown now, so reported lost. The log starts again under the recovered image's stamp,
      // the loss kept until a publication captures every volume, so a further restart reports it too.
      let _restarted = log.clear(object, image_generation);
      let _marked = log.overflow(object);
    } else if recovered.records.is_empty() && !recovered.everything_lost {
      let _ = log.clear(object, image_generation);
    }
    Some((log, recovered))
  }

  /// Reads the previous daemon's records when its stamp is `image_generation`, leaving `used` at their end so new
  /// writes follow them; with whether the stamp was older and its restamp owed ([`RESTAMP_OWED`], A-422), when the
  /// records are reported lost and the log must start again.
  fn read(&mut self, object: &SparseObject, image_generation: u64) -> (Recovered, bool) {
    let lost = Recovered {
      records: Vec::new(),
      everything_lost: true,
    };
    let at = |offset: usize| self.start.checked_add(offset);
    let (Some(stamp), Some(used), Some(overflow), Some(layout)) = (
      at(AT_STAMP).and_then(|at| word(object, at)),
      at(AT_USED).and_then(|at| word(object, at)),
      at(AT_OVERFLOW).and_then(|at| word(object, at)),
      at(AT_LAYOUT).and_then(|at| word(object, at)),
    ) else {
      return (lost, false);
    };
    if stamp != image_generation {
      // A log from before the recovered publication: that image already carries its writes, unless it left a volume out
      // and its daemon died before the log kept that volume's records under its stamp (A-422).
      return if overflow & RESTAMP_OWED == 0 {
        (Recovered::default(), false)
      } else {
        (lost, true)
      };
    }
    let Some(used) = usize::try_from(used)
      .ok()
      .filter(|used| *used <= self.capacity)
    else {
      return (lost, false);
    };
    if used > 0 && layout != LAYOUT {
      // Records of another layout (a daemon before every record named its volume): never misread as this one's.
      return (lost, false);
    }
    let mut records = Vec::new();
    let mut cursor = 0usize;
    while cursor < used {
      match self.record_at(object, cursor, used) {
        Some((record, next)) => {
          records.push(record);
          cursor = next;
        }
        None => return (lost, false),
      }
    }
    self.used = used;
    // A restamp owed under a current stamp says nothing: its publication was refused, or the restamp was done and its
    // bit not yet cleared (A-422).
    (
      Recovered {
        records,
        everything_lost: overflow & OVERFLOWED != 0,
      },
      false,
    )
  }

  /// The record at `cursor` of the records region and where the next begins, when it lies whole within `used` and is
  /// of a kind this daemon writes.
  fn record_at(
    &self,
    object: &SparseObject,
    cursor: usize,
    used: usize,
  ) -> Option<(Logged, usize)> {
    let base = self.start.checked_add(HEADER_BYTES)?.checked_add(cursor)?;
    let head = head_at(object, base)?;
    let next = cursor.checked_add(RECORD_HEAD)?.checked_add(head.length)?;
    if next > used {
      return None;
    }
    let mut bytes = vec![0u8; head.length];
    object
      .read(base.checked_add(RECORD_HEAD)?, &mut bytes)
      .ok()?;
    Some((logged_of(&head, bytes)?, next))
  }

  /// Appends a write of `bytes` to `inode` at `offset` through `volume`, before its reply: the record, then the length
  /// that covers it.
  pub(crate) fn append(
    &mut self,
    object: &mut SparseObject,
    (volume, inode): ([u8; VOLUME_BYTES], u64),
    offset: u64,
    bytes: &[u8],
  ) -> Result<(), Refused> {
    self.append_record(object, KIND_WRITE, (volume, inode, offset), bytes)
  }

  /// Appends a `FORGET` of `count` lookups on `inode` through `attachment`, once it is served.
  pub(crate) fn append_forget(
    &mut self,
    object: &mut SparseObject,
    forgotten: Forgotten,
  ) -> Result<(), Refused> {
    let mut bytes = [0u8; FORGET_BYTES - VOLUME_BYTES];
    let (attachment, count) = bytes.split_at_mut(size_of::<u64>());
    attachment.copy_from_slice(&forgotten.attachment.to_le_bytes());
    count.copy_from_slice(&forgotten.count.to_le_bytes());
    self.append_record(
      object,
      KIND_FORGET,
      (forgotten.volume, forgotten.inode, 0),
      &bytes,
    )
  }

  /// Appends a record of `kind` for `(volume, inode, offset)` with `bytes` after the volume's id: the record, then the
  /// length that covers it.
  fn append_record(
    &mut self,
    object: &mut SparseObject,
    kind: u32,
    (volume, inode, offset): ([u8; VOLUME_BYTES], u64, u64),
    bytes: &[u8],
  ) -> Result<(), Refused> {
    let payload = VOLUME_BYTES.checked_add(bytes.len()).ok_or(Refused::Full)?;
    let length = u32::try_from(payload).map_err(|_| Refused::Full)?;
    let record = RECORD_HEAD.checked_add(payload).ok_or(Refused::Full)?;
    let end = self.used.checked_add(record).ok_or(Refused::Full)?;
    if end > self.capacity {
      return Err(Refused::Full);
    }
    let mut head = Vec::with_capacity(RECORD_HEAD);
    head.extend_from_slice(&kind.to_le_bytes());
    head.extend_from_slice(&inode.to_le_bytes());
    head.extend_from_slice(&offset.to_le_bytes());
    head.extend_from_slice(&length.to_le_bytes());
    let at = HEADER_BYTES.checked_add(self.used).ok_or(Refused::Full)?;
    let at_volume = at.checked_add(RECORD_HEAD).ok_or(Refused::Full)?;
    self
      .store(object, at, &head)
      .map_err(|_| Refused::Unwritten)?;
    self
      .store(object, at_volume, &volume)
      .map_err(|_| Refused::Unwritten)?;
    self
      .store(
        object,
        at_volume.checked_add(VOLUME_BYTES).ok_or(Refused::Full)?,
        bytes,
      )
      .map_err(|_| Refused::Unwritten)?;
    let covered = u64::try_from(end).map_err(|_| Refused::Full)?;
    self
      .store(object, AT_USED, &covered.to_le_bytes())
      .map_err(|_| Refused::Unwritten)?;
    self.used = end;
    self.written = self.written.max(end);
    Ok(())
  }

  /// Records that an acknowledged write could not be logged, so the next daemon reports every file as having lost
  /// writes rather than replaying an incomplete log.
  pub(crate) fn overflow(&mut self, object: &mut SparseObject) -> Result<(), Unwritten> {
    self.overflowed = true;
    self.store(object, AT_OVERFLOW, &self.overflow_word().to_le_bytes())
  }

  /// Owes the log its restamp before a publication that leaves a volume out commits (A-422): until [`WriteLog::retain`]
  /// keeps that volume's records under the publication's stamp, or [`WriteLog::forgive_restamp`] says it was refused, a
  /// daemon that dies past the commit leaves its successor a stamp older than the committed image with the restamp
  /// owed, and the successor reports the writes lost instead of finding the log merely stale and dropping them.
  pub(crate) fn owe_restamp(&mut self, object: &mut SparseObject) -> Result<(), Unwritten> {
    self.restamp_owed = true;
    self.store(object, AT_OVERFLOW, &self.overflow_word().to_le_bytes())
  }

  /// The publication that owed the restamp was refused (A-422): nothing committed, so the log stands as it was.
  pub(crate) fn forgive_restamp(&mut self, object: &mut SparseObject) -> Result<(), Unwritten> {
    self.restamp_owed = false;
    self.store(object, AT_OVERFLOW, &self.overflow_word().to_le_bytes())
  }

  /// The overflow word this log's flags make ([`OVERFLOWED`], [`RESTAMP_OWED`]).
  fn overflow_word(&self) -> u64 {
    let overflowed = if self.overflowed { OVERFLOWED } else { 0 };
    let owed = if self.restamp_owed { RESTAMP_OWED } else { 0 };
    overflowed | owed
  }

  /// Keeps only the records of the volumes `kept` names, in their order, and stamps the log with `stamp`: what a
  /// publication of generation `stamp` that left those volumes out leaves owed to the next daemon (A-422). Every other
  /// record's write or `FORGET` is in that image, and replaying a write over it could undo a later change (a truncate
  /// after it); a left-out volume's are in no image since its last capture. In place, front to back: each kept record
  /// is copied down over the dropped ones before it, then the length that covers the kept ones is stored, the layout,
  /// then the stamp, and the restamp owed is cleared last. A daemon that dies before the stamp leaves it older than the
  /// committed image with the restamp owed ([`WriteLog::owe_restamp`]), which its successor reports as lost writes,
  /// never a half-moved log replayed. The bytes past the kept records (the dropped records and the old places of the
  /// moved ones) are then scrubbed and their whole pages given back, as a clear's are (condition 9, A-99). An unlogged
  /// write marked before stays marked: it may be a left-out volume's. Bounded by the bytes in use, walked once. A span
  /// the content object refuses before the stamp is stored leaves no record covered and the overflow marked, so nothing
  /// half-moved is ever replayed and the loss is reported.
  pub(crate) fn retain(
    &mut self,
    object: &mut SparseObject,
    stamp: u64,
    kept: impl Fn(&[u8; VOLUME_BYTES]) -> bool,
  ) -> Result<(), Unwritten> {
    let logged = self.used;
    let restamped = match self.compact(object, &kept) {
      Ok(to) => self.restamp(object, to, stamp).map(|()| to),
      Err(unwritten) => Err(unwritten),
    };
    let to = match restamped {
      Ok(to) => to,
      Err(unwritten) => {
        // The first refusal is the one returned: uncovering the records and marking the loss are what can still be done
        // after it, and either refused as well tells the caller nothing more.
        self.used = 0;
        let _uncovered = self.store(object, AT_USED, &0u64.to_le_bytes());
        let _marked = self.overflow(object);
        return Err(unwritten);
      }
    };
    self.restamp_owed = false;
    let settled = self.store(object, AT_OVERFLOW, &self.overflow_word().to_le_bytes());
    let scrubbed = match logged.checked_sub(to) {
      Some(tail) if tail > 0 => self
        .start
        .checked_add(HEADER_BYTES)
        .and_then(|at| at.checked_add(to))
        .ok_or(Unwritten)
        .and_then(|at| object.zero(at, tail).map_err(|_| Unwritten)),
      _ => Ok(()),
    };
    settled.and(scrubbed)
  }

  /// [`WriteLog::retain`]'s walk: each record `kept` names copied down over the dropped ones before it, in order; the
  /// bytes the kept records take. The header is not touched.
  fn compact(
    &self,
    object: &mut SparseObject,
    kept: &impl Fn(&[u8; VOLUME_BYTES]) -> bool,
  ) -> Result<usize, Unwritten> {
    let (mut from, mut to) = (0usize, 0usize);
    let mut record = Vec::new();
    while from < self.used {
      let base = self
        .start
        .checked_add(HEADER_BYTES)
        .and_then(|at| at.checked_add(from))
        .ok_or(Unwritten)?;
      let head = head_at(object, base).ok_or(Unwritten)?;
      let whole = RECORD_HEAD.checked_add(head.length).ok_or(Unwritten)?;
      let next = from
        .checked_add(whole)
        .filter(|next| *next <= self.used)
        .ok_or(Unwritten)?;
      record.clear();
      record.resize(whole, 0);
      object.read(base, &mut record).map_err(|_| Unwritten)?;
      let volume = record
        .get(RECORD_HEAD..)
        .and_then(|payload| payload.get(..VOLUME_BYTES))
        .and_then(|volume| <[u8; VOLUME_BYTES]>::try_from(volume).ok())
        .ok_or(Unwritten)?;
      if kept(&volume) {
        if to != from {
          let at = HEADER_BYTES.checked_add(to).ok_or(Unwritten)?;
          self.store(object, at, &record)?;
        }
        to = to.checked_add(whole).ok_or(Unwritten)?;
      }
      from = next;
    }
    Ok(to)
  }

  /// [`WriteLog::retain`]'s header: the length that covers the `to` bytes kept, the layout, then `stamp`, in that
  /// order.
  fn restamp(&mut self, object: &mut SparseObject, to: usize, stamp: u64) -> Result<(), Unwritten> {
    let covered = u64::try_from(to).map_err(|_| Unwritten)?;
    self.store(object, AT_USED, &covered.to_le_bytes())?;
    self.used = to;
    self.store(object, AT_LAYOUT, &LAYOUT.to_le_bytes())?;
    self.store(object, AT_STAMP, &stamp.to_le_bytes())?;
    self.stamp = stamp;
    Ok(())
  }

  /// The log as a restarted daemon finds it ([`WriteLog::open`] over this log's region) against a recovered image of
  /// `image_generation`: what it would replay. Test support: a unit test reads what recovery reads without a restart.
  #[cfg(test)]
  pub(crate) fn reopened(
    &self,
    object: &mut SparseObject,
    image_generation: u64,
  ) -> Option<Recovered> {
    let bytes = self.capacity.checked_add(HEADER_BYTES)?;
    WriteLog::open(object, self.start, bytes, image_generation).map(|(_, recovered)| recovered)
  }

  /// Whether every write acknowledged since the last publication is in the log, so a successor replays them all: then
  /// a `close`'s `flush` owes no publication of its own, a FUSE mount's (A-63) or a Shards stream's (A-399).
  pub(crate) fn holds_every_write(&self) -> bool {
    !self.overflowed
  }

  /// Empties the log after a publication of generation `stamp` that captured every volume: every write before it is
  /// in that image now. The records are scrubbed, not only uncovered: they are the plaintext of every logged write, and
  /// once the image holds them nothing needs them, while left in place they stayed in the anchor's RAM after the
  /// volume's content was sealed (condition 9, A-99; `a_cleared_log_keeps_none_of_the_bytes_it_logged`). The length
  /// goes to zero first, so a daemon killed mid-scrub replays nothing rather than half-zeroed records. The records'
  /// whole pages go back to the OS ([`SparseObject::zero`]), so a cleared log costs no RAM until it is written again.
  pub(crate) fn clear(&mut self, object: &mut SparseObject, stamp: u64) -> Result<(), Unwritten> {
    let logged = self.used;
    self.used = 0;
    self.stamp = stamp;
    self.overflowed = false;
    self.restamp_owed = false;
    // The header first (a crash mid-scrub then replays nothing), but the scrub runs whatever the header's writes
    // answered: the records are plaintext, and a refused header write that returned early left them in place while
    // this log already counted itself empty, so nothing would scrub them (2026-10-07). The first refusal is returned.
    let header = self
      .store(object, AT_USED, &0u64.to_le_bytes())
      .and(self.store(object, AT_OVERFLOW, &0u64.to_le_bytes()))
      .and(self.store(object, AT_LAYOUT, &LAYOUT.to_le_bytes()))
      .and(self.store(object, AT_STAMP, &stamp.to_le_bytes()));
    let scrubbed = if logged > 0 {
      self
        .start
        .checked_add(HEADER_BYTES)
        .ok_or(Unwritten)
        .and_then(|at| object.zero(at, logged).map_err(|_| Unwritten))
    } else {
      Ok(())
    };
    header.and(scrubbed)
  }

  /// Gives the record area's pages back to the OS while the log is empty (A-105): run by an idle shard, so a log that
  /// a burst of writes filled stops costing RAM once the burst is published and the shard goes quiet, while a busy
  /// shard keeps its pages for the next appends. Only as far as records reached since the last give-back (A-226's
  /// rule), and at most `budget` bytes a call, from the top down, so an idle shard walks a slice a step, never the log's
  /// whole capacity (gigabytes on a large machine). The bytes the OS took, the bytes walked, and whether any is left.
  /// A range the object refuses stays owed.
  pub(crate) fn purge(&mut self, object: &mut SparseObject, budget: usize) -> Purged {
    if self.used > 0 || self.written == 0 {
      return Purged::default();
    }
    let piece = self.written.min(budget);
    let from = self.written.saturating_sub(piece);
    let Some(bytes) = self
      .start
      .checked_add(HEADER_BYTES)
      .and_then(|at| at.checked_add(from))
      .and_then(|at| object.discard(at, piece).ok())
    else {
      return Purged::default();
    };
    self.written = from;
    Purged {
      bytes,
      walked: piece,
      more: from > 0,
    }
  }

  fn store(&self, object: &mut SparseObject, offset: usize, bytes: &[u8]) -> Result<(), Unwritten> {
    let at = self.start.checked_add(offset).ok_or(Unwritten)?;
    object.write(at, bytes).map_err(|_| Unwritten)
  }
}

/// Keeps one record with `append`, an acknowledged write's or a `FORGET`'s through `volume`, before its reply (A-63):
/// whether it is kept, by the log or by an image; `None` without a log (no content object, so nothing outlives the
/// daemon). A log with no room forces a publication, which settles the record by its own volume (A-436). One that
/// captured the volume carries the record, the effect already applied. One that left the volume out kept that
/// volume's records under its stamp and gave back the room the others' took (A-422), so the record is appended again.
/// Before A-436 any volume left out counted the record unkept, so a captured volume's write marked the log overflowed
/// and every file a taken-over mount had open was reported as having lost writes, though the image held it.
fn keep_record(
  s: &mut crate::state::ShardState,
  volume: slates_db::catalog::VolumeId,
  mut append: impl FnMut(&mut WriteLog, &mut SparseObject) -> Result<(), Refused>,
) -> Option<bool> {
  let appended = match (s.write_log.as_mut(), s.content.as_mut()) {
    (Some(log), Some(object)) => append(log, object),
    _ => return None,
  };
  Some(match appended {
    Ok(()) => true,
    Err(Refused::Full) => match crate::verbs::publish_shard(s) {
      Ok(published) if published.captured(volume) => true,
      Ok(_) => match (s.write_log.as_mut(), s.content.as_mut()) {
        (Some(log), Some(object)) => append(log, object).is_ok(),
        _ => false,
      },
      Err(_) => false,
    },
    Err(Refused::Unwritten) => false,
  })
}

/// Logs a write the volume took before its reply (A-63), `data` at `offset` of `inode` through `volume`, where a replay
/// puts it (an inode number alone names the origin of a clone's inherited file): a FUSE mount's and a Shards stream's.
/// Logged before the reply, so a write applied and never answered is resent, not replayed twice over a later write. One
/// neither the log nor an image keeps ([`keep_record`]: a refused publication, or a span the content object refused)
/// marks the log overflowed, so the next daemon reports the loss rather than replay around it; counted.
pub(crate) fn log_acknowledged_write(
  s: &mut crate::state::ShardState,
  volume: slates_db::catalog::VolumeId,
  (inode, offset): (u64, u64),
  data: &[u8],
) {
  let kept = keep_record(s, volume, |log, object| {
    log.append(object, (volume.bytes, inode), offset, data)
  });
  if kept != Some(false) {
    return;
  }
  s.count(crate::verbs::WRITE_LOG_UNWRITTEN, 1);
  if let (Some(log), Some(object)) = (s.write_log.as_mut(), s.content.as_mut())
    && log.overflow(object).is_err()
  {
    s.count(crate::verbs::WRITE_LOG_UNWRITTEN, 1);
  }
}

/// Format: the counter of a `FORGET` the write log could not take and no publication carried: its lookups stay
/// counted across a restart, a bounded leak (GAPS, A-154's sibling), never a lost write. Every platform's, as
/// `log_forgets` is (A-433).
const FORGET_UNLOGGED: &str = "write_log.forget_unlogged";

/// Logs the lookups `forgotten` (each a file and a count) gave back through `attachment` on `volume` in the shard's
/// write log (GAPS, A-154's sibling): a FUSE mount's `FORGET`s, and a Shards stream's whose resync end the anchor holds
/// (A-390). A `FORGET` takes no reply and is never resent, so one served after the last publication would otherwise
/// leave its references counted in the image for good. A log with no room forces a publication, which carries the
/// `FORGET` (already applied) when it captured the volume, and gives back room for it again when it left the volume out
/// ([`keep_record`], A-436); one still unkept is counted, and never marks the log overflowed, which would report every
/// taken-over file as having lost writes. A FUSE mount is Linux's; a stream whose resync end the anchor holds is every
/// platform's (Windows since A-433).
pub(crate) fn log_forgets(
  s: &mut crate::state::ShardState,
  (volume, attachment): (slates_db::catalog::VolumeId, u64),
  forgotten: Vec<(u64, u64)>,
) {
  for (inode, count) in forgotten {
    let forgotten = Forgotten {
      volume: volume.bytes,
      attachment,
      inode,
      count,
    };
    match keep_record(s, volume, |log, object| {
      log.append_forget(object, forgotten)
    }) {
      None => return,
      Some(true) => {}
      Some(false) => s.count(FORGET_UNLOGGED, 1),
    }
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
  /// Shape: the volume a test's records are written through.
  const VOLUME: [u8; VOLUME_BYTES] = [7; VOLUME_BYTES];
  /// Shape: another volume, a test's `FORGET`'s.
  const OTHER: [u8; VOLUME_BYTES] = [9; VOLUME_BYTES];

  fn object(tag: &str) -> SparseObject {
    SparseObject::create(
      &format!("slates-write-log-test-{tag}-{}", std::process::id()),
      OBJECT_BYTES,
      slates_mem::Words::new(),
    )
    .unwrap()
  }

  /// Condition 9 (A-99: plaintext at rest only where it is being written). Do: append a write carrying a marker, then
  /// clear the log as a publication that captured it does; read the log's whole region. Expect: the marker is gone.
  /// Before 2026-10-06 `clear` reset the counters only, and every logged write's plaintext stayed in the anchor's
  /// content object after its publication, after the volume's content was sealed, until a later append happened to
  /// overwrite it (found by scanning a live daemon's content memfd under a FUSE mount: 800 copies of a marker 25 s idle).
  #[test]
  fn a_cleared_log_keeps_none_of_the_bytes_it_logged() {
    const MARKER: &[u8] = b"PLAINTEXT-MARKER-AT-REST";
    let mut shared = object("scrub");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 1).unwrap();
    log
      .append(&mut shared, (VOLUME, 1), 0, &MARKER.repeat(8))
      .unwrap();
    log.clear(&mut shared, 2).unwrap();
    let mut region = vec![0u8; LOG_BYTES];
    shared.read(LOG_AT, &mut region).unwrap();
    assert!(
      !region.windows(MARKER.len()).any(|window| window == MARKER),
      "a cleared log keeps no logged byte"
    );
    let (_, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 2).unwrap();
    assert!(recovered.records.is_empty(), "and replays nothing");
  }

  /// A-226's sibling (bounded work): do append one small write to a log of many pages, clear it as a publication does,
  /// and give its pages back; expect the give-back to take no more than the pages the record reached, never the log's
  /// whole capacity (which a 128 GiB machine sizes in gigabytes, walked in one step of the idle shard). A log opened
  /// over a predecessor's object gives back what it cannot know was not written, once.
  #[test]
  fn a_purge_gives_back_only_what_the_log_wrote_since_its_last() {
    let mut shared = object("purge-extent");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 1).unwrap();
    // The first give-back after opening covers what a predecessor may have written, a page a call.
    let page = 4096;
    let mut calls = 0;
    loop {
      let purged = log.purge(&mut shared, page);
      assert!(purged.walked <= page, "a call walks at most its budget");
      calls += 1;
      if !purged.more {
        break;
      }
    }
    assert!(
      calls > 1,
      "the predecessor's log was given back a slice at a time"
    );
    log
      .append(&mut shared, (VOLUME, 1), 0, b"one small write")
      .unwrap();
    log.clear(&mut shared, 2).unwrap();
    let returned = log.purge(&mut shared, usize::MAX);
    assert!(
      returned.walked <= page,
      "a purge after one small record walked {} bytes, more than the page it reached",
      returned.walked
    );
    assert_eq!(
      log.purge(&mut shared, usize::MAX),
      Purged::default(),
      "and nothing more until the next append"
    );
  }

  /// A-63. Do: append three writes under the image of generation 7; open the log as a restarted daemon whose image is
  /// generation 7. Expect: the three writes back, in order, and new writes appended after them.
  #[test]
  fn the_writes_since_the_recovered_publication_are_replayed_in_order() {
    let mut shared = object("replay");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"alpha").unwrap();
    log.append(&mut shared, (VOLUME, 2), 4096, b"beta").unwrap();
    log.append(&mut shared, (VOLUME, 1), 5, b"gamma").unwrap();
    let (mut next, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    let written: Vec<([u8; VOLUME_BYTES], u64, u64, &[u8])> = recovered
      .records
      .iter()
      .filter_map(|logged| match logged {
        Logged::Write(record) => Some((
          record.volume,
          record.inode,
          record.offset,
          record.bytes.as_slice(),
        )),
        Logged::Forget(_) => None,
      })
      .collect();
    assert_eq!(
      written,
      [
        (VOLUME, 1, 0, &b"alpha"[..]),
        (VOLUME, 2, 4096, b"beta"),
        (VOLUME, 1, 5, b"gamma")
      ]
    );
    assert!(!recovered.everything_lost);
    next.append(&mut shared, (VOLUME, 3), 0, b"delta").unwrap();
    let (_, again) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    assert_eq!(
      again.records.len(),
      4,
      "a new write follows the replayed ones"
    );
  }

  /// GAPS (A-154's sibling, 2026-11-08). Do: under generation 4, append a write, a `FORGET` of three lookups on its
  /// file through attachment 9, and another write; open the log as a restarted daemon whose image is generation 4.
  /// Expect: the three records back in the order they were served, the `FORGET` whole; and a `FORGET` record whose
  /// length is not a `FORGET`'s, a hostile or torn one, refused as everything lost rather than read short.
  #[test]
  fn a_forget_is_replayed_in_its_place_among_the_writes() {
    let mut shared = object("forget");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 4).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"alpha").unwrap();
    let forgotten = Forgotten {
      volume: OTHER,
      attachment: 9,
      inode: 1,
      count: 3,
    };
    log.append_forget(&mut shared, forgotten).unwrap();
    log.append(&mut shared, (VOLUME, 2), 0, b"beta").unwrap();
    let (_, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 4).unwrap();
    assert_eq!(
      recovered.records,
      vec![
        Logged::Write(Record {
          volume: VOLUME,
          inode: 1,
          offset: 0,
          bytes: b"alpha".to_vec(),
        }),
        Logged::Forget(forgotten),
        Logged::Write(Record {
          volume: VOLUME,
          inode: 2,
          offset: 0,
          bytes: b"beta".to_vec(),
        }),
      ]
    );
    let forget_length =
      LOG_AT + HEADER_BYTES + RECORD_HEAD + VOLUME_BYTES + b"alpha".len() + RECORD_HEAD
        - size_of::<u32>();
    shared.write(forget_length, &1u32.to_le_bytes()).unwrap();
    let (_, torn) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 4).unwrap();
    assert!(
      torn.everything_lost,
      "a FORGET of the wrong length is not read short"
    );
  }

  /// A-63. Do: log writes under generation 7, then open as a daemon whose image is generation 8 (a publication that
  /// committed before the clear). Expect: nothing to replay — that image carries the writes — and the log emptied.
  #[test]
  fn a_log_from_before_the_recovered_publication_is_not_replayed() {
    let mut shared = object("stale");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 7).unwrap();
    log
      .append(&mut shared, (VOLUME, 1), 0, b"in the image already")
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
    log.append(&mut shared, (VOLUME, 1), 0, b"covered").unwrap();
    let uncovered = LOG_AT + HEADER_BYTES + RECORD_HEAD + VOLUME_BYTES + b"covered".len();
    shared.write(uncovered, &9u64.to_le_bytes()).unwrap();
    let (mut next, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 3).unwrap();
    assert_eq!(recovered.records.len(), 1);
    assert_eq!(
      next.append(&mut shared, (VOLUME, 1), 0, &vec![0u8; LOG_BYTES]),
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

  /// The record layout's change (2026-11-18: every record names its volume). Do: lay down a log as the layout before it
  /// wrote one, a header of three words and a write record right after it, under generation 6; open it as a daemon whose
  /// image is generation 6. Expect: reported as lost writes, never read as records of this layout; and an empty log of
  /// that layout reported nothing lost.
  #[test]
  fn a_log_of_the_layout_before_is_reported_lost_never_misread() {
    let mut shared = object("old-layout");
    let mut old = Vec::new();
    old.extend_from_slice(&6u64.to_le_bytes());
    let record_bytes = u64::try_from(RECORD_HEAD + b"old".len()).unwrap();
    old.extend_from_slice(&record_bytes.to_le_bytes());
    old.extend_from_slice(&0u64.to_le_bytes());
    old.extend_from_slice(&KIND_WRITE.to_le_bytes());
    old.extend_from_slice(&1u64.to_le_bytes());
    old.extend_from_slice(&0u64.to_le_bytes());
    old.extend_from_slice(&u32::try_from(b"old".len()).unwrap().to_le_bytes());
    old.extend_from_slice(b"old");
    shared.write(LOG_AT, &old).unwrap();
    let (_, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 6).unwrap();
    assert!(
      recovered.records.is_empty() && recovered.everything_lost,
      "an old layout's records are reported lost: {recovered:?}"
    );
    let mut empty = object("old-layout-empty");
    empty.write(LOG_AT, &6u64.to_le_bytes()).unwrap();
    let (_, nothing) = WriteLog::open(&mut empty, LOG_AT, LOG_BYTES, 6).unwrap();
    assert_eq!(nothing, Recovered::default(), "an empty one loses nothing");
  }

  /// The writes `recovered` hands back, as (volume, inode, bytes).
  fn writes(recovered: &Recovered) -> Vec<([u8; VOLUME_BYTES], u64, Vec<u8>)> {
    recovered
      .records
      .iter()
      .filter_map(|logged| match logged {
        Logged::Write(record) => Some((record.volume, record.inode, record.bytes.clone())),
        Logged::Forget(_) => None,
      })
      .collect()
  }

  /// A-422. Do: under generation 3, log a write through a kept volume, a `FORGET` and a write through another volume
  /// (the latter carrying a marker), and two more writes through the kept one; keep the kept volume's records under
  /// generation 4, as a publication that left it out does; open the log as a restarted daemon whose image is generation
  /// 4; then append one more write and open again. Expect: the kept volume's three writes, in order, nothing else and
  /// nothing lost; the other volume's marker scrubbed from the log's region; the new write after the kept ones.
  #[test]
  fn a_log_keeps_the_left_out_volumes_records_in_order_under_the_new_stamp() {
    const MARKER: &[u8] = b"CARRIED-BY-THE-IMAGE-MARKER";
    let mut shared = object("retain");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 3).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"alpha").unwrap();
    log
      .append_forget(
        &mut shared,
        Forgotten {
          volume: OTHER,
          attachment: 9,
          inode: 1,
          count: 2,
        },
      )
      .unwrap();
    log.append(&mut shared, (OTHER, 2), 0, MARKER).unwrap();
    log.append(&mut shared, (VOLUME, 3), 7, b"beta").unwrap();
    log.append(&mut shared, (VOLUME, 1), 5, b"gamma").unwrap();
    log.owe_restamp(&mut shared).unwrap();
    log
      .retain(&mut shared, 4, |volume| *volume == VOLUME)
      .unwrap();
    let (mut next, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 4).unwrap();
    assert!(!recovered.everything_lost, "{recovered:?}");
    assert_eq!(
      recovered.records.len(),
      3,
      "only the kept volume's records: {recovered:?}"
    );
    assert_eq!(
      writes(&recovered),
      [
        (VOLUME, 1, b"alpha".to_vec()),
        (VOLUME, 3, b"beta".to_vec()),
        (VOLUME, 1, b"gamma".to_vec())
      ]
    );
    let mut region = vec![0u8; LOG_BYTES];
    shared.read(LOG_AT, &mut region).unwrap();
    assert!(
      !region.windows(MARKER.len()).any(|window| window == MARKER),
      "a dropped record's bytes are scrubbed"
    );
    next.append(&mut shared, (OTHER, 4), 0, b"delta").unwrap();
    let (_, again) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 4).unwrap();
    assert_eq!(
      writes(&again).last(),
      Some(&(OTHER, 4, b"delta".to_vec())),
      "a new write follows the kept ones"
    );
    assert_eq!(again.records.len(), 4);
  }

  /// A-422, the death between a publication's commit and the log's restamp. Do: log a write under generation 5 and owe
  /// the restamp, as a publication that leaves a volume out does before it commits; open the log as a restarted daemon
  /// whose image is generation 6 (the publication committed, the restamp never ran), then log a write and open again at
  /// 6; and, apart, owe the restamp and open at 5 (the daemon died before the commit), and owe it, forgive it (the
  /// publication was refused) and open at 6 (a later publication that captured every volume committed before its
  /// clear). Expect: at 6 with the restamp owed, the write reported lost and none replayed; the next write replayed
  /// after it under the new stamp, the loss still reported until a publication captures every volume; at 5 the write
  /// replayed and nothing lost; forgiven and stale, nothing replayed and nothing lost, as before A-422.
  #[test]
  fn a_restamp_owed_at_a_death_reports_the_left_out_writes_lost_never_drops_them_in_silence() {
    let mut shared = object("owed");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 5).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"owed").unwrap();
    log.owe_restamp(&mut shared).unwrap();
    let (mut next, lost) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 6).unwrap();
    assert!(lost.everything_lost && lost.records.is_empty(), "{lost:?}");
    next.append(&mut shared, (VOLUME, 2), 0, b"after").unwrap();
    let (_, later) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 6).unwrap();
    assert_eq!(writes(&later), [(VOLUME, 2, b"after".to_vec())]);
    assert!(later.everything_lost, "the loss is reported again");

    let mut before_commit = object("owed-uncommitted");
    let (mut log, _) = WriteLog::open(&mut before_commit, LOG_AT, LOG_BYTES, 5).unwrap();
    log
      .append(&mut before_commit, (VOLUME, 1), 0, b"owed")
      .unwrap();
    log.owe_restamp(&mut before_commit).unwrap();
    let (_, kept) = WriteLog::open(&mut before_commit, LOG_AT, LOG_BYTES, 5).unwrap();
    assert_eq!(writes(&kept), [(VOLUME, 1, b"owed".to_vec())]);
    assert!(!kept.everything_lost, "{kept:?}");

    let mut forgiven = object("owed-forgiven");
    let (mut log, _) = WriteLog::open(&mut forgiven, LOG_AT, LOG_BYTES, 5).unwrap();
    log
      .append(&mut forgiven, (VOLUME, 1), 0, b"in the image")
      .unwrap();
    log.owe_restamp(&mut forgiven).unwrap();
    log.forgive_restamp(&mut forgiven).unwrap();
    let (_, stale) = WriteLog::open(&mut forgiven, LOG_AT, LOG_BYTES, 6).unwrap();
    assert_eq!(stale, Recovered::default());
  }

  /// A-422: an overflow marked before a publication that leaves a volume out stays marked through its restamp. Do: log
  /// a write, mark the log overflowed (an acknowledged write went unlogged), owe the restamp and keep the write's
  /// volume under generation 2; open at 2. Expect: the write replayed and the loss still reported, since the unlogged
  /// write may have been the left-out volume's.
  #[test]
  fn an_overflow_marked_before_a_restamp_stays_reported() {
    let mut shared = object("retain-overflow");
    let (mut log, _) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 1).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"kept").unwrap();
    log.overflow(&mut shared).unwrap();
    log.owe_restamp(&mut shared).unwrap();
    log
      .retain(&mut shared, 2, |volume| *volume == VOLUME)
      .unwrap();
    let (_, recovered) = WriteLog::open(&mut shared, LOG_AT, LOG_BYTES, 2).unwrap();
    assert_eq!(writes(&recovered), [(VOLUME, 1, b"kept".to_vec())]);
    assert!(recovered.everything_lost, "{recovered:?}");
  }
}
