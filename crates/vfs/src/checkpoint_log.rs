//! The shard's recovery journal (A-68; §4.8, D-18): the double-buffered checkpoint of `crate::recover` plus an
//! append-only log of deltas over it, so a barrier publishes what changed instead of the whole shard.
//!
//! **Layout.** Two regions of anchor RAM: the checkpoint memory (the existing two generation-tagged slots, one
//! committed and one being written) and the delta log. A checkpoint is a whole [`ShardImage`] framed into the free
//! slot, exactly as before. A delta frame is appended at the log's tail: a little-endian length and a CRC-32C of
//! the payload, then the payload — the frame's generation, the generation of the checkpoint it applies to, and the
//! Wire [`ShardDelta`]. The commit of a delta *is* its CRC becoming valid, as a checkpoint's is.
//!
//! **Recovery.** The committed checkpoint, then every log frame from the start, in order, while each is CRC-valid,
//! names that checkpoint as its base and carries the next generation. The first frame that does not stops the
//! replay: a frame torn by a crash was never acknowledged (its barrier's reply waits for the commit), and a frame
//! naming another base is left over from before the last checkpoint (a checkpoint restarts the log at its start
//! and its generation is new, so no old frame can pass as one of its deltas).
//!
//! **When to checkpoint.** At the first publication; when the next delta would not fit the log; and when the bytes
//! of deltas written since the last checkpoint reach that checkpoint's own size. The last rule bounds the work: a
//! delta is paid once when written and at most once more as part of the next checkpoint, so total publication work
//! stays proportional to the changes (the amortization a log-structured merge relies on; Rosenblum and
//! Ousterhout's checkpoint and log), and a recovery replays at most a checkpoint's worth of deltas.

use slates_wire::Wire;
use slates_wire::crc32c::{crc32c, crc32c_append};

use crate::delta::VolumeRecord;
use crate::error::VfsError;
use crate::recover::{CommittedSlot, HeldReply, ImageRead, ImageWrite, ShardImage};

/// Format: a delta's magic, checked before anything else on decode.
const DELTA_MAGIC: u32 = u32::from_le_bytes(*b"SLD1");
/// Format: the delta format's version.
const DELTA_VERSION: u16 = 1;
/// Format: a log frame's header — a little-endian payload length then a CRC-32C of the payload.
const LOG_HEADER: usize = 2 * size_of::<u32>();
/// Format: a log frame's payload prefix — its generation and its base checkpoint's generation.
const LOG_PREFIX: usize = 2 * size_of::<u64>();

/// One volume's publication in a delta, under its owner's routing key.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct KeyedRecord {
  /// The owner's routing key (a volume id's bytes).
  pub key: [u8; 16],
  /// The volume's full image or its changes.
  pub record: VolumeRecord,
}

/// What changed in a shard since the last publication: the volumes changed (in key order), the volumes gone, and
/// the held replicas' image and the barrier replies when they changed.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct ShardDelta {
  /// The format magic.
  pub magic: u32,
  /// The format version.
  pub version: u16,
  /// The changed volumes, in key order.
  pub volumes: Vec<KeyedRecord>,
  /// The volumes no longer held, in key order.
  pub removed: Vec<[u8; 16]>,
  /// The held replicas' image now, when it changed.
  pub held: Option<Vec<u8>>,
  /// The undelivered barrier replies now, when they changed.
  pub replies: Option<Vec<HeldReply>>,
}

impl ShardDelta {
  /// A delta of these changes.
  pub fn new(
    mut volumes: Vec<KeyedRecord>,
    mut removed: Vec<[u8; 16]>,
    held: Option<Vec<u8>>,
    replies: Option<Vec<HeldReply>>,
  ) -> ShardDelta {
    volumes.sort_by_key(|volume| volume.key);
    removed.sort_unstable();
    ShardDelta {
      magic: DELTA_MAGIC,
      version: DELTA_VERSION,
      volumes,
      removed,
      held,
      replies,
    }
  }
}

impl ShardImage {
  /// Applies a delta (the module doc): afterwards this image is the shard's image at the delta's publication. A
  /// volume delta over a volume this image does not hold is refused [`VfsError::RecoveryIncomplete`].
  pub fn apply(&mut self, delta: &ShardDelta) -> Result<(), VfsError> {
    if delta.magic != DELTA_MAGIC || delta.version != DELTA_VERSION {
      return Err(VfsError::RecoveryIncomplete);
    }
    for keyed in &delta.volumes {
      let at = self
        .volumes
        .binary_search_by_key(&keyed.key, |volume| volume.key);
      match (&keyed.record, at) {
        (VolumeRecord::Full { image }, Ok(at)) => {
          if let Some(slot) = self.volumes.get_mut(at) {
            slot.image = image.clone();
          }
        }
        (VolumeRecord::Full { image }, Err(at)) => self.volumes.insert(
          at,
          crate::recover::KeyedImage {
            key: keyed.key,
            image: image.clone(),
          },
        ),
        (VolumeRecord::Delta { delta }, Ok(at)) => {
          self
            .volumes
            .get_mut(at)
            .ok_or(VfsError::RecoveryIncomplete)?
            .image
            .apply(delta)?;
        }
        (VolumeRecord::Delta { .. }, Err(_)) => return Err(VfsError::RecoveryIncomplete),
      }
    }
    self
      .volumes
      .retain(|volume| delta.removed.binary_search(&volume.key).is_err());
    if let Some(held) = &delta.held {
      self.held.clone_from(held);
    }
    if let Some(replies) = &delta.replies {
      self.replies.clone_from(replies);
    }
    Ok(())
  }
}

/// A publisher's view of its journal (the module doc): the committed checkpoint, where the log's tail is, and how
/// much has been logged since the checkpoint. Only the one process publishing into the memories may keep it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Journal {
  /// The committed checkpoint, as the last checkpoint publish returned it.
  committed: Option<CommittedSlot>,
  /// The generation of the last committed publication (checkpoint or delta).
  generation: u64,
  /// Where the next delta frame starts in the log.
  tail: usize,
  /// The bytes of the last checkpoint's frame.
  checkpoint_bytes: usize,
  /// The bytes of delta frames written since it.
  logged_bytes: usize,
}

impl Journal {
  /// The generation of the last committed publication: what a transport's write log since it is stamped with.
  pub fn generation(&self) -> u64 {
    self.generation
  }

  /// What the next checkpoint's image is expected to take: the last checkpoint's bytes and every delta's since (each
  /// delta's changes are in it, as additions or rewrites). A publisher reserves its encoding buffer at this once, where
  /// a buffer grown by doubling reached twice the image and left each outgrown one in the allocator's footprint.
  pub fn expected_checkpoint_bytes(&self) -> usize {
    self.checkpoint_bytes.saturating_add(self.logged_bytes)
  }

  /// Whether the next publication must be a checkpoint before any delta is even built: nothing committed yet, or
  /// the deltas since the last checkpoint already reached its size.
  pub fn wants_checkpoint(&self) -> bool {
    self.committed.is_none() || self.logged_bytes >= self.checkpoint_bytes
  }

  /// Publishes `image` as a checkpoint into the free slot of `checkpoints` and restarts the log. Refuses
  /// [`VfsError::NoSpace`] if a slot cannot hold it; the journal is unchanged on any refusal.
  pub fn checkpoint<S: ImageWrite + ?Sized>(
    &mut self,
    checkpoints: &mut S,
    image: &ShardImage,
  ) -> Result<usize, VfsError> {
    let known = self
      .committed
      .map(|committed| committed.advanced_to(self.generation));
    let (bytes, committed) = image.write_after(checkpoints, known)?;
    *self = Journal {
      committed: Some(committed),
      generation: committed.generation,
      tail: 0,
      checkpoint_bytes: bytes,
      logged_bytes: 0,
    };
    Ok(bytes)
  }

  /// [`Journal::checkpoint`] for a shard image already encoded (`ShardImage::encode_start` … `encode_finish`), so the
  /// publisher never holds the image as a value; the journal is unchanged on any refusal.
  pub fn checkpoint_encoded<S: ImageWrite + ?Sized>(
    &mut self,
    checkpoints: &mut S,
    encoded: &[u8],
  ) -> Result<usize, VfsError> {
    let known = self
      .committed
      .map(|committed| committed.advanced_to(self.generation));
    let (bytes, committed) = ShardImage::write_encoded_after(encoded, checkpoints, known)?;
    *self = Journal {
      committed: Some(committed),
      generation: committed.generation,
      tail: 0,
      checkpoint_bytes: bytes,
      logged_bytes: 0,
    };
    Ok(bytes)
  }

  /// Appends `delta` to `log` over the committed checkpoint. Refuses [`VfsError::NoSpace`] when the frame does not
  /// fit the log's room (the caller checkpoints instead), and refuses before writing anything when no checkpoint is
  /// committed; the journal is unchanged on any refusal.
  pub fn append<S: ImageWrite + ?Sized>(
    &mut self,
    log: &mut S,
    delta: &ShardDelta,
  ) -> Result<usize, VfsError> {
    let Some(committed) = self.committed else {
      return Err(VfsError::RecoveryIncomplete);
    };
    let body = delta.to_bytes();
    let generation = self
      .generation
      .checked_add(1)
      .ok_or(VfsError::FileTooLarge)?;
    let payload_len = LOG_PREFIX
      .checked_add(body.len())
      .ok_or(VfsError::FileTooLarge)?;
    let total = LOG_HEADER
      .checked_add(payload_len)
      .ok_or(VfsError::FileTooLarge)?;
    let end = self.tail.checked_add(total).ok_or(VfsError::FileTooLarge)?;
    if end > log.image_len() {
      return Err(VfsError::NoSpace);
    }
    let mut prefix = [0u8; LOG_PREFIX];
    let (gen_field, base_field) = prefix.split_at_mut(size_of::<u64>());
    gen_field.copy_from_slice(&generation.to_le_bytes());
    base_field.copy_from_slice(&committed.generation.to_le_bytes());
    let crc = crc32c_append(crc32c(&prefix), &body);
    let mut header = [0u8; LOG_HEADER];
    let (len_field, crc_field) = header.split_at_mut(size_of::<u32>());
    len_field.copy_from_slice(
      &u32::try_from(payload_len)
        .map_err(|_| VfsError::FileTooLarge)?
        .to_le_bytes(),
    );
    crc_field.copy_from_slice(&crc.to_le_bytes());
    // The payload first and the header last, so the frame becomes CRC-valid only once all of it is written.
    let payload_at = self.tail.saturating_add(LOG_HEADER);
    log.image_write(payload_at, &prefix)?;
    log.image_write(payload_at.saturating_add(LOG_PREFIX), &body)?;
    // The next frame's length word reads zero until a frame is written there, so a replay stops at this tail.
    if end.saturating_add(LOG_HEADER) <= log.image_len() {
      log.image_write(end, &[0u8; LOG_HEADER])?;
    }
    log.image_write(self.tail, &header)?;
    self.generation = generation;
    self.tail = end;
    self.logged_bytes = self.logged_bytes.saturating_add(total);
    Ok(total)
  }

  /// The journal a restarted publisher resumes from: the committed checkpoint and the log replayed over it, its
  /// image (`None` when nothing ever committed), and the journal positioned after the last valid frame.
  pub fn recover<C: ImageRead + ?Sized, L: ImageRead + ?Sized>(
    checkpoints: &C,
    log: &L,
  ) -> Result<(Option<ShardImage>, Journal), VfsError> {
    // The committed checkpoint found, read and checked once (it was found by checking both slots, then found and
    // read again).
    let Some((committed, mut image)) = ShardImage::read_committed(checkpoints)? else {
      return Ok((None, Journal::default()));
    };
    let mut journal = Journal {
      committed: Some(committed),
      generation: committed.generation,
      tail: 0,
      // A recovered checkpoint's size is not known without re-encoding it; zero makes the next publication a
      // checkpoint, which re-establishes the size.
      checkpoint_bytes: 0,
      logged_bytes: 0,
    };
    while let Some((generation, delta, total)) =
      read_frame(log, journal.tail, committed.generation)?
    {
      if generation != journal.generation.saturating_add(1) {
        break;
      }
      image.apply(&delta)?;
      journal.generation = generation;
      journal.tail = journal.tail.saturating_add(total);
      journal.logged_bytes = journal.logged_bytes.saturating_add(total);
    }
    Ok((Some(image), journal))
  }
}

/// The generation of the last committed publication in a journal's memories — the committed checkpoint's, advanced
/// over every valid delta frame after it — without decoding a delta: what a transport's write log is stamped
/// against at startup (A-63). `0` when nothing committed.
pub fn committed_generation<C: ImageRead + ?Sized, L: ImageRead + ?Sized>(
  checkpoints: &C,
  log: &L,
) -> u64 {
  let Some(committed) = crate::recover::committed_slot(checkpoints) else {
    return 0;
  };
  let (mut generation, mut at) = (committed.generation, 0usize);
  while let Ok(Some((next, total))) = frame_bounds(log, at, committed.generation) {
    if next != generation.saturating_add(1) {
      break;
    }
    generation = next;
    at = at.saturating_add(total);
  }
  generation
}

/// The generation and whole length of the CRC-valid log frame at `at` over checkpoint `base`, without decoding its
/// delta; `None` where a replay would stop.
fn frame_bounds<L: ImageRead + ?Sized>(
  log: &L,
  at: usize,
  base: u64,
) -> Result<Option<(u64, usize)>, VfsError> {
  Ok(read_raw(log, at, base)?.map(|(generation, _, total)| (generation, total)))
}

/// The log frame at `at` over checkpoint `base`: its generation, its delta and its whole length; `None` at an empty
/// slot, a torn frame (length past the log, CRC mismatch), a frame over another base, or a delta that does not
/// decode — each ends the replay (the module doc).
fn read_frame<L: ImageRead + ?Sized>(
  log: &L,
  at: usize,
  base: u64,
) -> Result<Option<(u64, ShardDelta, usize)>, VfsError> {
  let Some((generation, body, total)) = read_raw(log, at, base)? else {
    return Ok(None);
  };
  let Ok(delta) = ShardDelta::from_bytes(&body) else {
    return Ok(None);
  };
  Ok(Some((generation, delta, total)))
}

/// The CRC-valid log frame at `at` over checkpoint `base`: its generation, its delta's bytes and its whole length;
/// `None` where a replay would stop (an empty slot, a torn frame, another base).
fn read_raw<L: ImageRead + ?Sized>(
  log: &L,
  at: usize,
  base: u64,
) -> Result<Option<(u64, Vec<u8>, usize)>, VfsError> {
  let Some(header_end) = at.checked_add(LOG_HEADER) else {
    return Ok(None);
  };
  if header_end > log.image_len() {
    return Ok(None);
  }
  let mut header = [0u8; LOG_HEADER];
  log.image_read(at, &mut header)?;
  let (len_field, crc_field) = header.split_at(size_of::<u32>());
  let word = |field: &[u8]| -> u32 {
    let mut bytes = [0u8; size_of::<u32>()];
    bytes.copy_from_slice(field);
    u32::from_le_bytes(bytes)
  };
  let len = usize::try_from(word(len_field)).unwrap_or(usize::MAX);
  if len < LOG_PREFIX {
    return Ok(None);
  }
  let Some(end) = header_end.checked_add(len) else {
    return Ok(None);
  };
  if end > log.image_len() {
    return Ok(None);
  }
  let mut payload = vec![0u8; len];
  log.image_read(header_end, &mut payload)?;
  if crc32c(&payload) != word(crc_field) {
    return Ok(None);
  }
  let (prefix, body) = payload.split_at(LOG_PREFIX);
  let (gen_field, base_field) = prefix.split_at(size_of::<u64>());
  let read_u64 = |field: &[u8]| -> u64 {
    let mut bytes = [0u8; size_of::<u64>()];
    bytes.copy_from_slice(field);
    u64::from_le_bytes(bytes)
  };
  if read_u64(base_field) != base {
    return Ok(None);
  }
  Ok(Some((
    read_u64(gen_field),
    body.to_vec(),
    LOG_HEADER.saturating_add(len),
  )))
}
