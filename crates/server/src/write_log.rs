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
//!
//! **The slice's marks** (A-926; goal 93, main's ruling of 2026-11-25). The region's last [`MARKS_BYTES`] bytes hold
//! the high-water marks of the shard slice's written memories ([`Marked`]: this log's region, the journal's two
//! checkpoint slots and its delta log): its version word, then each mark, bytes from its memory's start. A mark is
//! raised, its new end rounded up to a page, before the write that passes it (this log's append, and every checkpoint
//! and delta write through `verbs::ContentSlots`), with a release fence between, so a daemon killed between the two
//! leaves it ahead of the bytes, never behind; it falls after its memory is given back ([`WriteLog::purge`], the
//! journal's give-back). A successor reads them as it opens, so it knows how far each memory may hold pages: a
//! residency sweep and a give-back read that far, never a memory whole. Before them a fresh 1-shard audit daemon's sweep
//! walked 12,582,958 pages, 8,388,609 of them its slots and log it had never written. Layout version 3 carries the
//! block; version 2's log has none, and its records may run to the region's end: it is read as before, every memory's
//! mark whole (the assumption before A-926), until its first clear uncovers its records, writes the block, then the
//! version 3 word. A slice no daemon opened (every header word zero) has every mark zero.

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
/// Format: the layout this daemon writes, "WLOG" and version 3 (A-926, 2026-11-25): version 2's records and header, and
/// the slice's marks block at the region's end ([`MARKS_BYTES`]), which the record area stops before. Version 2 (every
/// record names its volume, 2026-11-18) had no marks block; the layout before it had no such word, and its first record
/// began where this word sits, its kind (1 or 2) in the low half; this word's low half is no record kind, so such a log
/// is never read as this one's.
const LAYOUT: u64 = u64::from_le_bytes(*b"WLOG\x03\x00\x00\x00");
/// Format: version 2's layout word ([`LAYOUT`]'s history): its records are read as this one's, its region has no marks
/// block, and its record area runs to the region's end.
const LAYOUT_V2: u64 = u64::from_le_bytes(*b"WLOG\x02\x00\x00\x00");
/// Format: the marks block's version word, "MARK" and version 1 (A-926).
const MARKS_VERSION: u64 = u64::from_le_bytes(*b"MARK\x01\x00\x00\x00");
/// Format: the marks block at the write log region's end (A-926): its version word, then the high-water mark of each
/// [`Marked`] memory of the shard's slice, in that order, each a little-endian `u64` of bytes from the memory's start.
const MARKS_BYTES: usize = (1 + MARKED) * size_of::<u64>();
/// Format: the memories the marks block keeps a mark for ([`Marked`]).
const MARKED: usize = 4;
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

/// The memories of a shard's slice whose high-water marks the marks block keeps (A-926; goal 93, main's ruling of
/// 2026-11-25): how far into each pages may be resident, as far as any write reached since it was last given back, so a
/// successor knows what its predecessor wrote instead of taking every memory as written whole. A mark is raised, the new
/// end rounded up to a page, before the write that passes it, so a crash leaves it ahead of the data, never behind; it
/// falls when its memory is given back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marked {
  /// The write log's region: its header and record area.
  WriteLog = 0,
  /// The checkpoint memory's first slot.
  FirstSlot = 1,
  /// Its second slot.
  SecondSlot = 2,
  /// The delta log.
  Deltas = 3,
}

/// How a log's marks block was found as it opened (A-926): the migration's record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarksFound {
  /// A slice no daemon opened before: every memory unwritten.
  Fresh,
  /// A version 3 log's marks.
  Read,
  /// A log of version 2 or older, or a block that did not read: every memory taken as written whole, once.
  Adopted,
}

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
  /// furthest any record reached, as far as the marks a predecessor left say (A-926).
  written: usize,
  /// The slice's high-water marks ([`Marked`]), bytes from each memory's start, as the marks block holds them.
  marks: [usize; MARKED],
  /// Whether the marks block is in force: a version 3 log. A version 2 log's records may run into the block's bytes, so
  /// its marks stay whole and are not written until its first clear makes it version 3.
  marks_live: bool,
  /// The page the marks are raised in steps of.
  page: usize,
  /// How the marks were found as the log opened.
  found: MarksFound,
  /// Marks raised since the log opened: one a page a memory grows by, against the writes ([`WriteLog::appends`]).
  raises: u64,
  /// Records appended since the log opened.
  appends: u64,
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
  /// it holds to replay. Its marks block read as `page`-stepped high-water marks (A-926): a version 2 log's read as every
  /// memory whole (`journal_whole` for the journal's memories), once, its records read as far as the region's end until
  /// its first clear writes version 3. `None` when `bytes` cannot hold the header and the marks block.
  pub(crate) fn open(
    object: &mut SparseObject,
    (start, bytes): (usize, usize),
    image_generation: u64,
    (page, journal_whole): (usize, slates_vfs::checkpoint_log::WrittenExtents),
  ) -> Option<(WriteLog, Recovered)> {
    let whole_area = bytes.checked_sub(HEADER_BYTES)?;
    let capacity = whole_area.checked_sub(MARKS_BYTES)?;
    let [first, second] = journal_whole.slots;
    let (found, marks) = read_marks(
      object,
      (start, bytes),
      [bytes, first, second, journal_whole.log],
    );
    let live = found == MarksFound::Read;
    let mut log = WriteLog {
      start,
      // A version 2 log's records may run to the region's end: read as far, until its first clear.
      capacity: if found == MarksFound::Adopted {
        whole_area
      } else {
        capacity
      },
      written: marks.first().map_or(whole_area, |mark| {
        mark.saturating_sub(HEADER_BYTES).min(whole_area)
      }),
      used: 0,
      stamp: image_generation,
      overflowed: false,
      restamp_owed: false,
      marks,
      marks_live: live,
      page: page.max(1),
      found,
      raises: 0,
      appends: 0,
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
    if used > 0 && layout != LAYOUT && layout != LAYOUT_V2 {
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
    // The region's mark covers the record before any byte of it is written (A-926).
    let reach = HEADER_BYTES.checked_add(end).ok_or(Refused::Full)?;
    self
      .raise_mark(object, Marked::WriteLog, reach)
      .map_err(|_| Refused::Unwritten)?;
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
    #[cfg(test)]
    if STOP_AFTER_RECORD.with(core::cell::Cell::get) {
      // The crash point A-926's red holds: the record's bytes written, nothing after.
      return Err(Refused::Unwritten);
    }
    let covered = u64::try_from(end).map_err(|_| Refused::Full)?;
    self
      .store(object, AT_USED, &covered.to_le_bytes())
      .map_err(|_| Refused::Unwritten)?;
    self.used = end;
    self.written = self.written.max(end);
    self.appends = self.appends.saturating_add(1);
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
    self.store(object, AT_LAYOUT, &self.layout().to_le_bytes())?;
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
    let bytes = self.marks_at()?.checked_add(MARKS_BYTES)?;
    let whole = slates_vfs::checkpoint_log::WrittenExtents::default();
    WriteLog::open(
      object,
      (self.start, bytes),
      image_generation,
      (self.page, whole),
    )
    .map(|(_, recovered)| recovered)
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
    // A log not yet version 3 becomes it here (A-926): its records uncovered first, so the marks block may take the
    // bytes they reached, then the block, then the layout word that puts it in force.
    let header = self
      .store(object, AT_USED, &0u64.to_le_bytes())
      .and(self.store(object, AT_OVERFLOW, &0u64.to_le_bytes()))
      .and(self.put_marks_in_force(object))
      .and(self.store(object, AT_LAYOUT, &self.layout().to_le_bytes()))
      .and(self.store(object, AT_STAMP, &stamp.to_le_bytes()));
    // As far as the record area now reaches: a version 2 log's records past it lay where its marks block now is, which
    // their bytes are overwritten by (A-926).
    let logged = logged.min(self.capacity);
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
  /// The bytes of the log's range, from its start, whose pages may be resident: its header and the record area as far
  /// as any record reached since it was last given back ([`WriteLog::purge`]), the whole area for a log opened over a
  /// predecessor's memory. What a residency sweep need read of it (A-744, main's ruling on w-sizing's option (a)).
  pub(crate) fn written_bytes(&self) -> usize {
    HEADER_BYTES.saturating_add(self.written)
  }

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
    // The mark falls with what was given back (A-926), after it: a crash between leaves it ahead, never behind.
    let _lowered = self.lower_mark(object, Marked::WriteLog, HEADER_BYTES.saturating_add(from));
    Purged {
      bytes,
      walked: piece,
      more: from > 0,
    }
  }

  /// The layout word this log writes: version 3 once its marks block is in force, version 2 until then ([`LAYOUT`]).
  fn layout(&self) -> u64 {
    if self.marks_live { LAYOUT } else { LAYOUT_V2 }
  }

  /// Where the marks block sits in the log's region: its last [`MARKS_BYTES`] bytes.
  fn marks_at(&self) -> Option<usize> {
    HEADER_BYTES
      .checked_add(self.capacity)?
      .checked_add(if self.marks_live || self.found != MarksFound::Adopted {
        MARKS_BYTES
      } else {
        0
      })?
      .checked_sub(MARKS_BYTES)
  }

  /// Writes the marks block whole (its version word and every mark) where it is not yet in force, a version 2 log's
  /// first clear (A-926): the record area then stops before it.
  fn put_marks_in_force(&mut self, object: &mut SparseObject) -> Result<(), Unwritten> {
    if self.marks_live {
      return Ok(());
    }
    let at = self.marks_at().ok_or(Unwritten)?;
    let mut block = Vec::with_capacity(MARKS_BYTES);
    block.extend_from_slice(&MARKS_VERSION.to_le_bytes());
    for mark in self.marks {
      block.extend_from_slice(&u64::try_from(mark).unwrap_or(u64::MAX).to_le_bytes());
    }
    self.store(object, at, &block)?;
    self.capacity = at.checked_sub(HEADER_BYTES).ok_or(Unwritten)?;
    self.marks_live = true;
    if self.found == MarksFound::Adopted {
      // The record area now stops before the block: what was written past it is the block's page.
      self.written = self.written.min(self.capacity);
    }
    Ok(())
  }

  /// Raises `which`'s mark to cover `end` bytes from its memory's start, rounded up to a page, before the write that
  /// reaches there (A-926): one store a page a memory grows by, its order with the write kept by a release fence. Nothing
  /// where the mark covers it already, or where the marks block is not in force (a version 2 log's marks are whole).
  pub(crate) fn raise_mark(
    &mut self,
    object: &mut SparseObject,
    which: Marked,
    end: usize,
  ) -> Result<(), Unwritten> {
    let index = which as usize;
    let Some(&mark) = self.marks.get(index) else {
      return Err(Unwritten);
    };
    if !self.marks_live || end <= mark {
      return Ok(());
    }
    #[cfg(test)]
    if MARKS_OFF.with(core::cell::Cell::get) {
      return Ok(());
    }
    let raised = end
      .checked_next_multiple_of(self.page)
      .unwrap_or(usize::MAX);
    self.store_mark(object, index, raised)?;
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    self.raises = self.raises.saturating_add(1);
    Ok(())
  }

  /// Lowers `which`'s mark to `end` where it is above it: once its memory past `end` was given back (A-926).
  pub(crate) fn lower_mark(
    &mut self,
    object: &mut SparseObject,
    which: Marked,
    end: usize,
  ) -> Result<(), Unwritten> {
    let index = which as usize;
    match self.marks.get(index) {
      Some(&mark) if self.marks_live && end < mark => self.store_mark(object, index, end),
      _ => Ok(()),
    }
  }

  /// Stores mark `index` as `value` in the block and keeps it.
  fn store_mark(
    &mut self,
    object: &mut SparseObject,
    index: usize,
    value: usize,
  ) -> Result<(), Unwritten> {
    let at = self
      .marks_at()
      .and_then(|at| at.checked_add(size_of::<u64>()))
      .and_then(|at| at.checked_add(index.checked_mul(size_of::<u64>())?))
      .ok_or(Unwritten)?;
    let word = u64::try_from(value).map_err(|_| Unwritten)?;
    self.store(object, at, &word.to_le_bytes())?;
    if let Some(mark) = self.marks.get_mut(index) {
      *mark = value;
    }
    Ok(())
  }

  /// The journal's memories as far as their marks say pages may be resident (A-926): what the shard's journal opens over.
  pub(crate) fn journal_extents(&self) -> slates_vfs::checkpoint_log::WrittenExtents {
    let mark = |which: Marked| self.marks.get(which as usize).copied().unwrap_or(0);
    slates_vfs::checkpoint_log::WrittenExtents {
      slots: [mark(Marked::FirstSlot), mark(Marked::SecondSlot)],
      log: mark(Marked::Deltas),
    }
  }

  /// Lowers the journal's memories' marks to `written`, the journal's extents after a give-back (A-926).
  pub(crate) fn lower_journal_marks(
    &mut self,
    object: &mut SparseObject,
    written: slates_vfs::checkpoint_log::WrittenExtents,
  ) -> Result<(), Unwritten> {
    let [first, second] = written.slots;
    self
      .lower_mark(object, Marked::FirstSlot, first)
      .and(self.lower_mark(object, Marked::SecondSlot, second))
      .and(self.lower_mark(object, Marked::Deltas, written.log))
  }

  /// How the marks were found as the log opened, the marks raised since, and the records appended since (A-926).
  pub(crate) fn marks_counts(&self) -> (MarksFound, u64, u64) {
    (self.found, self.raises, self.appends)
  }

  /// The marks block's byte range in the log's region, `(offset, len)`, while it is in force: a page a residency sweep
  /// reads beside the record area (A-926).
  pub(crate) fn marks_block(&self) -> Option<(usize, usize)> {
    self
      .marks_live
      .then(|| self.marks_at().map(|at| (at, MARKS_BYTES)))
      .flatten()
  }

  fn store(&self, object: &mut SparseObject, offset: usize, bytes: &[u8]) -> Result<(), Unwritten> {
    let at = self.start.checked_add(offset).ok_or(Unwritten)?;
    object.write(at, bytes).map_err(|_| Unwritten)
  }
}

#[cfg(test)]
std::thread_local! {
  /// Test support (A-926's crash red): an append stops right after its record's bytes are written, as a daemon killed
  /// there leaves its log.
  static STOP_AFTER_RECORD: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
  /// Test support (A-926's cost measure): marks never raised, the write path as it was before them.
  static MARKS_OFF: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// The marks a log region `(start, bytes)` of `object` holds (A-926), and how they were found: none written on a slice
/// no daemon opened (every header word zero); a version 3 log's block, each mark no more than its memory (`whole`, the
/// region's and the journal's memories' lengths); else every memory whole.
fn read_marks(
  object: &SparseObject,
  (start, bytes): (usize, usize),
  whole: [usize; MARKED],
) -> (MarksFound, [usize; MARKED]) {
  let at = |offset: usize| start.checked_add(offset).and_then(|at| word(object, at));
  let header = [AT_STAMP, AT_USED, AT_OVERFLOW, AT_LAYOUT].map(at);
  if header.iter().all(|word| *word == Some(0)) {
    return (MarksFound::Fresh, [0; MARKED]);
  }
  let Some(block) = bytes.checked_sub(MARKS_BYTES) else {
    return (MarksFound::Adopted, whole);
  };
  if header.get(3).copied().flatten() != Some(LAYOUT) || at(block) != Some(MARKS_VERSION) {
    return (MarksFound::Adopted, whole);
  }
  let mut marks = whole;
  for (index, mark) in marks.iter_mut().enumerate() {
    let read = index
      .checked_add(1)
      .and_then(|word_index| word_index.checked_mul(size_of::<u64>()))
      .and_then(|offset| block.checked_add(offset))
      .and_then(at)
      .and_then(|value| usize::try_from(value).ok());
    match read {
      Some(value) => *mark = value.min(*mark),
      None => return (MarksFound::Adopted, whole),
    }
  }
  (MarksFound::Read, marks)
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
  /// Shape: the page the test log's marks are raised in steps of.
  const PAGE: usize = 4096;

  /// The log of [`LOG_BYTES`] at [`LOG_AT`] in `object` against a recovered image of `image_generation`, its marks in
  /// pages of the test object's ([`PAGE`]), the journal's memories none.
  fn open_at(object: &mut SparseObject, image_generation: u64) -> Option<(WriteLog, Recovered)> {
    WriteLog::open(
      object,
      (LOG_AT, LOG_BYTES),
      image_generation,
      (PAGE, slates_vfs::checkpoint_log::WrittenExtents::default()),
    )
  }

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
    let (mut log, _) = open_at(&mut shared, 1).unwrap();
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
    let (_, recovered) = open_at(&mut shared, 2).unwrap();
    assert!(recovered.records.is_empty(), "and replays nothing");
  }

  /// Writes a version 2 log's empty header at [`LOG_AT`] stamped `stamp`, as a daemon before A-926 left it.
  fn version_2_header(object: &mut SparseObject, stamp: u64) {
    let mut header = Vec::new();
    for word in [stamp, 0, 0, LAYOUT_V2] {
      header.extend_from_slice(&word.to_le_bytes());
    }
    object.write(LOG_AT, &header).unwrap();
  }

  /// A-226's sibling (bounded work): do append one small write to a log of many pages, clear it as a publication does,
  /// and give its pages back; expect the give-back to take no more than the pages the record reached, never the log's
  /// whole capacity (which a 128 GiB machine sizes in gigabytes, walked in one step of the idle shard). A log opened
  /// over a version 2 predecessor's object gives back what it cannot know was not written, once (A-926: its marks
  /// whole); a slice no daemon opened gives back nothing.
  #[test]
  fn a_purge_gives_back_only_what_the_log_wrote_since_its_last() {
    let mut fresh = object("purge-fresh");
    let (mut unwritten, _) = open_at(&mut fresh, 1).unwrap();
    assert_eq!(
      unwritten.purge(&mut fresh, usize::MAX),
      Purged::default(),
      "a fresh slice's log gives back nothing"
    );
    let mut shared = object("purge-extent");
    version_2_header(&mut shared, 1);
    let (mut log, _) = open_at(&mut shared, 1).unwrap();
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
    let (mut log, _) = open_at(&mut shared, 7).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"alpha").unwrap();
    log.append(&mut shared, (VOLUME, 2), 4096, b"beta").unwrap();
    log.append(&mut shared, (VOLUME, 1), 5, b"gamma").unwrap();
    let (mut next, recovered) = open_at(&mut shared, 7).unwrap();
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
    let (_, again) = open_at(&mut shared, 7).unwrap();
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
    let (mut log, _) = open_at(&mut shared, 4).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"alpha").unwrap();
    let forgotten = Forgotten {
      volume: OTHER,
      attachment: 9,
      inode: 1,
      count: 3,
    };
    log.append_forget(&mut shared, forgotten).unwrap();
    log.append(&mut shared, (VOLUME, 2), 0, b"beta").unwrap();
    let (_, recovered) = open_at(&mut shared, 4).unwrap();
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
    let (_, torn) = open_at(&mut shared, 4).unwrap();
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
    let (mut log, _) = open_at(&mut shared, 7).unwrap();
    log
      .append(&mut shared, (VOLUME, 1), 0, b"in the image already")
      .unwrap();
    let (_, recovered) = open_at(&mut shared, 8).unwrap();
    assert_eq!(recovered, Recovered::default());
    let (_, after) = open_at(&mut shared, 8).unwrap();
    assert!(after.records.is_empty());
  }

  /// A-63, crash and capacity. Do: a record stored past the length that covers it (a daemon killed mid-append); a
  /// write larger than the room left; an overflow marked. Expect: the uncovered record is not replayed; the large
  /// write is refused `Full`; the overflow reads as "everything lost".
  #[test]
  fn an_uncovered_record_is_ignored_a_full_log_refuses_and_an_overflow_is_reported() {
    let mut shared = object("edges");
    let (mut log, _) = open_at(&mut shared, 3).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"covered").unwrap();
    let uncovered = LOG_AT + HEADER_BYTES + RECORD_HEAD + VOLUME_BYTES + b"covered".len();
    shared.write(uncovered, &9u64.to_le_bytes()).unwrap();
    let (mut next, recovered) = open_at(&mut shared, 3).unwrap();
    assert_eq!(recovered.records.len(), 1);
    assert_eq!(
      next.append(&mut shared, (VOLUME, 1), 0, &vec![0u8; LOG_BYTES]),
      Err(Refused::Full)
    );
    next.overflow(&mut shared).unwrap();
    let (_, overflowed) = open_at(&mut shared, 3).unwrap();
    assert!(overflowed.everything_lost);
  }

  /// A-63, hostile input. Do: a length in use past the capacity. Expect: "everything lost", never a short replay.
  #[test]
  fn an_impossible_length_in_use_reports_everything_lost() {
    let mut shared = object("hostile");
    let (_, _) = open_at(&mut shared, 5).unwrap();
    shared
      .write(LOG_AT + AT_USED, &u64::MAX.to_le_bytes())
      .unwrap();
    let (_, recovered) = open_at(&mut shared, 5).unwrap();
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
    let (_, recovered) = open_at(&mut shared, 6).unwrap();
    assert!(
      recovered.records.is_empty() && recovered.everything_lost,
      "an old layout's records are reported lost: {recovered:?}"
    );
    let mut empty = object("old-layout-empty");
    empty.write(LOG_AT, &6u64.to_le_bytes()).unwrap();
    let (_, nothing) = open_at(&mut empty, 6).unwrap();
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
    let (mut log, _) = open_at(&mut shared, 3).unwrap();
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
    let (mut next, recovered) = open_at(&mut shared, 4).unwrap();
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
    let (_, again) = open_at(&mut shared, 4).unwrap();
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
    let (mut log, _) = open_at(&mut shared, 5).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"owed").unwrap();
    log.owe_restamp(&mut shared).unwrap();
    let (mut next, lost) = open_at(&mut shared, 6).unwrap();
    assert!(lost.everything_lost && lost.records.is_empty(), "{lost:?}");
    next.append(&mut shared, (VOLUME, 2), 0, b"after").unwrap();
    let (_, later) = open_at(&mut shared, 6).unwrap();
    assert_eq!(writes(&later), [(VOLUME, 2, b"after".to_vec())]);
    assert!(later.everything_lost, "the loss is reported again");

    let mut before_commit = object("owed-uncommitted");
    let (mut log, _) = open_at(&mut before_commit, 5).unwrap();
    log
      .append(&mut before_commit, (VOLUME, 1), 0, b"owed")
      .unwrap();
    log.owe_restamp(&mut before_commit).unwrap();
    let (_, kept) = open_at(&mut before_commit, 5).unwrap();
    assert_eq!(writes(&kept), [(VOLUME, 1, b"owed".to_vec())]);
    assert!(!kept.everything_lost, "{kept:?}");

    let mut forgiven = object("owed-forgiven");
    let (mut log, _) = open_at(&mut forgiven, 5).unwrap();
    log
      .append(&mut forgiven, (VOLUME, 1), 0, b"in the image")
      .unwrap();
    log.owe_restamp(&mut forgiven).unwrap();
    log.forgive_restamp(&mut forgiven).unwrap();
    let (_, stale) = open_at(&mut forgiven, 6).unwrap();
    assert_eq!(stale, Recovered::default());
  }

  /// A-422: an overflow marked before a publication that leaves a volume out stays marked through its restamp. Do: log
  /// a write, mark the log overflowed (an acknowledged write went unlogged), owe the restamp and keep the write's
  /// volume under generation 2; open at 2. Expect: the write replayed and the loss still reported, since the unlogged
  /// write may have been the left-out volume's.
  #[test]
  fn an_overflow_marked_before_a_restamp_stays_reported() {
    let mut shared = object("retain-overflow");
    let (mut log, _) = open_at(&mut shared, 1).unwrap();
    log.append(&mut shared, (VOLUME, 1), 0, b"kept").unwrap();
    log.overflow(&mut shared).unwrap();
    log.owe_restamp(&mut shared).unwrap();
    log
      .retain(&mut shared, 2, |volume| *volume == VOLUME)
      .unwrap();
    let (_, recovered) = open_at(&mut shared, 2).unwrap();
    assert_eq!(writes(&recovered), [(VOLUME, 1, b"kept".to_vec())]);
    assert!(recovered.everything_lost, "{recovered:?}");
  }

  /// The journal's memories whole for the A-926 tests: a checkpoint memory and a delta log of a few pages each.
  fn journal_whole() -> slates_vfs::checkpoint_log::WrittenExtents {
    slates_vfs::checkpoint_log::WrittenExtents::whole(8 * PAGE, 4 * PAGE)
  }

  /// A version 2 log at [`LOG_AT`] stamped `stamp`, its one write record through [`VOLUME`] filling its record area to
  /// the region's very end (the bytes a marks block takes in version 3 among them): the record's bytes.
  fn version_2_full(object: &mut SparseObject, stamp: u64) -> Vec<u8> {
    let area = LOG_BYTES - HEADER_BYTES;
    let bytes: Vec<u8> = (0..area - RECORD_HEAD - VOLUME_BYTES)
      .map(|at| (at % 251) as u8)
      .collect();
    let mut record = Vec::new();
    record.extend_from_slice(&KIND_WRITE.to_le_bytes());
    record.extend_from_slice(&3u64.to_le_bytes());
    record.extend_from_slice(&0u64.to_le_bytes());
    record.extend_from_slice(
      &u32::try_from(VOLUME_BYTES + bytes.len())
        .unwrap()
        .to_le_bytes(),
    );
    record.extend_from_slice(&VOLUME);
    record.extend_from_slice(&bytes);
    assert_eq!(
      record.len(),
      area,
      "the record fills the area to the region's end"
    );
    object.write(LOG_AT + HEADER_BYTES, &record).unwrap();
    let mut header = Vec::new();
    for word in [stamp, area as u64, 0, LAYOUT_V2] {
      header.extend_from_slice(&word.to_le_bytes());
    }
    object.write(LOG_AT, &header).unwrap();
    bytes
  }

  /// A-926, the migration (main's ruling of 2026-11-25). Do: leave a version 2 log whose one record fills its region to
  /// the very end, the last 40 bytes among it; open it with this code against its image's generation; append a write;
  /// clear it as a publication does; open it again. Expect the record replayed byte for byte; the log adopted (counted),
  /// every memory read whole (the log's region and the journal's memories as given) until its first clear; the append
  /// refused full, which forces that clear; and after it a version 3 log whose marks block reads back, its marks the
  /// extents it kept.
  #[test]
  fn a_version_2_log_filled_to_its_end_is_replayed_whole_then_cleared_into_version_3() {
    let mut shared = object("v2-full");
    let bytes = version_2_full(&mut shared, 5);
    let (mut log, recovered) =
      WriteLog::open(&mut shared, (LOG_AT, LOG_BYTES), 5, (PAGE, journal_whole())).unwrap();
    let Some(Logged::Write(record)) = recovered.records.first() else {
      panic!("the record replayed: {recovered:?}");
    };
    assert_eq!(
      (record.inode, &record.bytes),
      (3, &bytes),
      "replayed byte for byte"
    );
    assert_eq!(
      log.marks_counts().0,
      MarksFound::Adopted,
      "adopted, counted"
    );
    assert_eq!(log.written_bytes(), LOG_BYTES, "the log's region whole");
    assert_eq!(
      log.journal_extents(),
      journal_whole(),
      "the journal's memories whole"
    );
    assert_eq!(
      log.marks_block(),
      None,
      "no marks block in force before the first clear"
    );
    assert_eq!(
      log.append(&mut shared, (VOLUME, 4), 0, b"one more"),
      Err(Refused::Full),
      "the append that does not fit forces the clear"
    );
    log.clear(&mut shared, 6).unwrap();
    assert_eq!(
      log.marks_block(),
      Some((LOG_BYTES - MARKS_BYTES, MARKS_BYTES)),
      "version 3, its marks block in force"
    );
    log
      .append(&mut shared, (VOLUME, 4), 0, b"one more")
      .unwrap();
    let (again, replayed) =
      WriteLog::open(&mut shared, (LOG_AT, LOG_BYTES), 6, (PAGE, journal_whole())).unwrap();
    assert_eq!(
      again.marks_counts().0,
      MarksFound::Read,
      "its marks read back"
    );
    assert_eq!(
      again.journal_extents(),
      journal_whole(),
      "the extents it kept"
    );
    assert_eq!(
      replayed.records.len(),
      1,
      "the record after the clear replayed"
    );
  }

  /// A-926, the crash point (main's ruling of 2026-11-25: raised before the write, a crash leaves the mark ahead of the
  /// data, never behind). Do: on a fresh slice, append records until the log passes a page, the last stopped right
  /// after its bytes are written, as a daemon killed there; open the log again. Expect the successor's mark covering
  /// every byte the stopped record wrote. Red with the mark raised after the record's bytes: the mark a page short.
  #[test]
  fn a_crash_after_a_records_bytes_leaves_the_mark_covering_them() {
    let mut shared = object("crash-mark");
    let (mut log, _) = open_at(&mut shared, 1).unwrap();
    let write = vec![5u8; 700];
    let mut reached = HEADER_BYTES;
    while reached <= PAGE {
      log.append(&mut shared, (VOLUME, 1), 0, &write).unwrap();
      reached += RECORD_HEAD + VOLUME_BYTES + write.len();
    }
    STOP_AFTER_RECORD.with(|stop| stop.set(true));
    let stopped = log.append(&mut shared, (VOLUME, 1), 0, &vec![6u8; 2 * PAGE]);
    STOP_AFTER_RECORD.with(|stop| stop.set(false));
    assert_eq!(
      stopped,
      Err(Refused::Unwritten),
      "the append stopped after its bytes"
    );
    let wrote_to = reached + RECORD_HEAD + VOLUME_BYTES + 2 * PAGE;
    let (successor, _) = open_at(&mut shared, 1).unwrap();
    assert!(
      successor.written_bytes() >= wrote_to,
      "the successor's mark {} covers the stopped record's bytes, to {wrote_to}",
      successor.written_bytes()
    );
  }

  /// A-926, the raises' cost (main's ruling of 2026-11-25: one raise a page, the write path's cost unchanged). Do: on a
  /// fresh log, append small records until it has grown by several pages, then clear it, five rounds, alternating
  /// rounds with the marks raised and with them off (the write path before A-926), each round timed. Expect one raise a
  /// page grown each marked round (the pages the records reached), against far more appends; prints the best round's
  /// time an append of each.
  #[test]
  fn a_growing_log_raises_its_mark_once_a_page() {
    /// Shape: the rounds of each arm.
    const ROUNDS: usize = 5;
    let write = [3u8; 100];
    let record = RECORD_HEAD + VOLUME_BYTES + write.len();
    let appends = (LOG_BYTES - HEADER_BYTES - MARKS_BYTES) / record;
    let pages = (HEADER_BYTES + appends * record).div_ceil(PAGE) as u64;
    let mut shared = object("raises");
    let (mut log, _) = open_at(&mut shared, 1).unwrap();
    let (mut marked_ns, mut unmarked_ns) = (u128::MAX, u128::MAX);
    for round in 0..2 * ROUNDS {
      let off = round % 2 == 1;
      MARKS_OFF.with(|marks_off| marks_off.set(off));
      log.clear(&mut shared, 1).unwrap();
      // The marks fall with the pages the clear gave back, so each marked round raises them again.
      let _ = log.lower_mark(&mut shared, Marked::WriteLog, HEADER_BYTES);
      let raised = log.marks_counts().1;
      let began = std::time::Instant::now();
      for _ in 0..appends {
        log.append(&mut shared, (VOLUME, 1), 0, &write).unwrap();
      }
      let ns = began.elapsed().as_nanos() / appends as u128;
      if off {
        unmarked_ns = unmarked_ns.min(ns);
        assert_eq!(log.marks_counts().1, raised, "no raise with the marks off");
      } else {
        marked_ns = marked_ns.min(ns);
        assert_eq!(
          log.marks_counts().1 - raised,
          pages,
          "one raise a page grown"
        );
      }
    }
    MARKS_OFF.with(|marks_off| marks_off.set(false));
    println!(
      "write_log: {appends} appends a round raised the mark once a page over {pages} pages; best of {ROUNDS}: {marked_ns} \
       ns an append marked, {unmarked_ns} ns with the marks off"
    );
  }
}
