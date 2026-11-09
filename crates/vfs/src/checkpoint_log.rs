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
use crate::recover::{
  CommittedSlot, HELD_CALL_KEY_BYTES, HELD_PIECE_KEY_BYTES, HeldCall, HeldNfsReply, HeldNfsStates,
  HeldPiece, HeldReply, ImageRead, ImageWrite, ShardImage, StreamedImage,
};

/// Format: a delta's magic, checked before anything else on decode.
const DELTA_MAGIC: u32 = u32::from_le_bytes(*b"SLD1");
/// The delta format's history: 1 (A-68, 2026-10-04); 2 (A-126, 2026-10-09) a file changed only by writes to its chunked
/// body is carried as its written windows (`delta::InodePatch`), not its whole image; 3 (A-127, 2026-10-09) the call
/// records changed and purged since the last publication; 4 (A-128, 2026-10-09) the held NFSv3 replies, whole, when they
/// changed; 5 (A-231, 2026-11-05) the held connections' byte states; 6 (A-270, 2026-11-08) what the shard holds for other
/// owners as the pieces put and the keys removed since the last publication, never the whole held image.
/// Format: the delta format's version, bumped with any change to the delta's types.
pub const DELTA_VERSION: u16 = 6;
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
  /// The held pieces put since the last publication (A-270), in key order, each whole.
  pub held_put: Vec<HeldPiece>,
  /// The held pieces removed since the last publication, in key order.
  pub held_removed: Vec<[u8; HELD_PIECE_KEY_BYTES]>,
  /// The undelivered barrier replies now, when they changed.
  pub replies: Option<Vec<HeldReply>>,
  /// The call records changed since the last publication (A-127), in key order, each whole.
  pub calls: Vec<HeldCall>,
  /// The call records purged since the last publication, in key order.
  pub calls_removed: Vec<[u8; HELD_CALL_KEY_BYTES]>,
  /// The held NFSv3 replies now, when they changed (A-128).
  pub nfs_replies: Option<Vec<HeldNfsReply>>,
  /// The held connections' byte states now, when they changed (A-231).
  pub nfs_states: Option<Vec<HeldNfsStates>>,
}

impl ShardDelta {
  /// Starts a streamed shard delta's encoding in `out` (for [`ShardDelta::encode_finish`]): what `ShardDelta::new`
  /// then `encode` writes before the volumes, with their count to be patched in; returns where it goes. The caller
  /// then appends each changed volume as its key's 16 bytes followed by `Volume::encode_publication`, in key order.
  pub fn encode_start(out: &mut Vec<u8>) -> usize {
    DELTA_MAGIC.encode(out);
    DELTA_VERSION.encode(out);
    let at = out.len();
    slates_wire::codec::encode_len(0, out);
    at
  }

  /// Ends a streamed shard delta: the volume count patched in at `at`, then the volumes gone (sorted here), the held
  /// replicas' image, the replies, the call records changed and purged (each sorted here), and the held NFSv3 replies
  /// and the held connections' byte states when they changed, as `ShardDelta::new(volumes, removed, held, replies)
  /// .with_calls(calls, calls_removed).with_nfs_replies(nfs_replies).with_nfs_states(nfs_states).encode` writes them.
  #[allow(clippy::too_many_arguments)] // one delta's whole tail, each part written in the format's order
  pub fn encode_finish(
    out: &mut Vec<u8>,
    at: usize,
    volumes: usize,
    removed: &mut [[u8; 16]],
    (held_put, held_removed): (&mut [HeldPiece], &mut [[u8; HELD_PIECE_KEY_BYTES]]),
    replies: Option<&[HeldReply]>,
    (calls, calls_removed): (&mut [HeldCall], &mut [[u8; HELD_CALL_KEY_BYTES]]),
    (nfs_replies, nfs_states): (Option<&mut [HeldNfsReply]>, Option<&mut [HeldNfsStates]>),
  ) -> Result<(), VfsError> {
    if !slates_wire::codec::patch_len(out, at, volumes) {
      return Err(VfsError::RecoveryIncomplete);
    }
    removed.sort_unstable();
    slates_wire::codec::encode_len(removed.len(), out);
    for key in removed.iter() {
      key.encode(out);
    }
    encode_held(out, held_put, held_removed);
    match replies {
      None => out.push(slates_wire::codec::OPTION_NONE),
      Some(replies) => {
        out.push(slates_wire::codec::OPTION_SOME);
        slates_wire::codec::encode_len(replies.len(), out);
        for reply in replies {
          reply.encode(out);
        }
      }
    }
    calls.sort_unstable_by_key(|call| call.key);
    slates_wire::codec::encode_len(calls.len(), out);
    for call in calls.iter() {
      call.encode(out);
    }
    calls_removed.sort_unstable();
    slates_wire::codec::encode_len(calls_removed.len(), out);
    for key in calls_removed.iter() {
      key.encode(out);
    }
    match nfs_replies {
      None => out.push(slates_wire::codec::OPTION_NONE),
      Some(nfs_replies) => {
        nfs_replies.sort_unstable_by_key(|reply| (reply.connection, reply.xid));
        out.push(slates_wire::codec::OPTION_SOME);
        slates_wire::codec::encode_len(nfs_replies.len(), out);
        for reply in nfs_replies.iter() {
          reply.encode(out);
        }
      }
    }
    match nfs_states {
      None => out.push(slates_wire::codec::OPTION_NONE),
      Some(nfs_states) => {
        nfs_states.sort_unstable_by_key(|states| (states.connection, states.volume));
        out.push(slates_wire::codec::OPTION_SOME);
        slates_wire::codec::encode_len(nfs_states.len(), out);
        for states in nfs_states.iter() {
          states.encode(out);
        }
      }
    }
    Ok(())
  }

  /// A delta of these changes.
  pub fn new(
    mut volumes: Vec<KeyedRecord>,
    mut removed: Vec<[u8; 16]>,
    (mut held_put, mut held_removed): (Vec<HeldPiece>, Vec<[u8; HELD_PIECE_KEY_BYTES]>),
    replies: Option<Vec<HeldReply>>,
  ) -> ShardDelta {
    volumes.sort_by_key(|volume| volume.key);
    removed.sort_unstable();
    held_put.sort_unstable_by_key(|piece| piece.key);
    held_removed.sort_unstable();
    ShardDelta {
      magic: DELTA_MAGIC,
      version: DELTA_VERSION,
      volumes,
      removed,
      held_put,
      held_removed,
      replies,
      calls: Vec::new(),
      calls_removed: Vec::new(),
      nfs_replies: None,
      nfs_states: None,
    }
  }

  /// This delta carrying the call records changed (`calls`) and purged (`calls_removed`) since the last publication
  /// (A-127), each sorted by key.
  pub fn with_calls(
    self,
    mut calls: Vec<HeldCall>,
    mut calls_removed: Vec<[u8; HELD_CALL_KEY_BYTES]>,
  ) -> ShardDelta {
    calls.sort_unstable_by_key(|call| call.key);
    calls_removed.sort_unstable();
    ShardDelta {
      calls,
      calls_removed,
      ..self
    }
  }

  /// This delta carrying the held NFSv3 replies now (A-128), sorted by (connection, xid).
  pub fn with_nfs_replies(self, mut nfs_replies: Vec<HeldNfsReply>) -> ShardDelta {
    nfs_replies.sort_unstable_by_key(|reply| (reply.connection, reply.xid));
    ShardDelta {
      nfs_replies: Some(nfs_replies),
      ..self
    }
  }
}

impl ShardDelta {
  /// This delta carrying the held connections' byte states now (A-231), sorted by (connection, volume).
  pub fn with_nfs_states(self, mut nfs_states: Vec<HeldNfsStates>) -> ShardDelta {
    nfs_states.sort_unstable_by_key(|states| (states.connection, states.volume));
    ShardDelta {
      nfs_states: Some(nfs_states),
      ..self
    }
  }
}

/// Writes a delta's held pieces put and keys removed (A-270's format), each sorted by key, as `ShardDelta`'s `encode`
/// writes its `held_put` and `held_removed`.
fn encode_held(
  out: &mut Vec<u8>,
  held_put: &mut [HeldPiece],
  held_removed: &mut [[u8; HELD_PIECE_KEY_BYTES]],
) {
  held_put.sort_unstable_by_key(|piece| piece.key);
  slates_wire::codec::encode_len(held_put.len(), out);
  for piece in held_put.iter() {
    piece.encode(out);
  }
  held_removed.sort_unstable();
  slates_wire::codec::encode_len(held_removed.len(), out);
  for key in held_removed.iter() {
    key.encode(out);
  }
}

impl ShardImage {
  /// Folds a delta's held pieces into this image (A-270): each piece put replaces or joins the one under its key, then
  /// each key removed drops its piece. A delta never puts and removes one key.
  fn fold_held(&mut self, delta: &ShardDelta) {
    for piece in &delta.held_put {
      match self.held.binary_search_by_key(&piece.key, |held| held.key) {
        Ok(at) => {
          if let Some(held) = self.held.get_mut(at) {
            held.bytes.clone_from(&piece.bytes);
          }
        }
        Err(at) => self.held.insert(at, piece.clone()),
      }
    }
    self
      .held
      .retain(|held| delta.held_removed.binary_search(&held.key).is_err());
  }

  /// Applies a delta (the module doc): afterwards this image is the shard's image at the delta's publication. A
  /// volume delta over a volume this image does not hold is refused [`VfsError::RecoveryIncomplete`].
  pub fn apply(&mut self, delta: &ShardDelta) -> Result<(), VfsError> {
    if delta.magic != DELTA_MAGIC {
      return Err(VfsError::RecoveryIncomplete);
    }
    if delta.version != DELTA_VERSION {
      return Err(VfsError::RecoveryVersion {
        found: delta.version,
        expected: DELTA_VERSION,
      });
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
    self.fold_held(delta);
    if let Some(replies) = &delta.replies {
      self.replies.clone_from(replies);
    }
    for call in &delta.calls {
      match self.calls.binary_search_by_key(&call.key, |held| held.key) {
        Ok(at) => {
          if let Some(held) = self.calls.get_mut(at) {
            held.record.clone_from(&call.record);
          }
        }
        Err(at) => self.calls.insert(at, call.clone()),
      }
    }
    self
      .calls
      .retain(|held| delta.calls_removed.binary_search(&held.key).is_err());
    if let Some(nfs_replies) = &delta.nfs_replies {
      self.nfs_replies.clone_from(nfs_replies);
    }
    if let Some(nfs_states) = &delta.nfs_states {
      self.nfs_states.clone_from(nfs_states);
    }
    Ok(())
  }
}

/// A publisher's view of its journal (the module doc): the committed checkpoint, where the log's tail is, how much
/// has been logged since the checkpoint, and how far each memory may hold pages written since they were last given
/// back (A-226). Only the one process publishing into the memories may keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
  /// How far into the committed slot pages may be resident: its checkpoint's bytes, or more where the slot held a
  /// longer image that was never given back. [`UNKNOWN_EXTENT`] for a journal opened over memories a predecessor wrote.
  committed_written: usize,
  /// How far into the stale slot pages may be resident since it was last given back ([`Journal::dead_ranges`]).
  stale_written: usize,
  /// How far into the log pages may be resident since the log past the tail was last given back: the furthest any
  /// frame, or the zeroed header after it, reached.
  log_written: usize,
}

/// The extent of a memory whose writes this journal did not see (a predecessor's, or a torn first checkpoint's): all
/// of it, clipped to the memory's length where a range is asked for. Format: a sentinel, not a size.
const UNKNOWN_EXTENT: usize = usize::MAX;

impl Default for Journal {
  /// A journal that has committed nothing, over memories that may hold anything: a fresh object reads zeros, but one a
  /// predecessor wrote before its first checkpoint committed holds a torn image, so both slots and the log are given
  /// back whole the first time ([`Journal::dead_ranges`]).
  fn default() -> Self {
    Journal {
      committed: None,
      generation: 0,
      tail: 0,
      checkpoint_bytes: 0,
      logged_bytes: 0,
      committed_written: UNKNOWN_EXTENT,
      stale_written: UNKNOWN_EXTENT,
      log_written: UNKNOWN_EXTENT,
    }
  }
}

/// What of a shard's recovery memory no restart reads and was written since it was last given back (A-117, A-226),
/// each as `(offset, len)` within its own region: the checkpoint slot older than the committed one, and the delta log
/// past its tail, each only as far as it was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeadRanges {
  /// The stale checkpoint slot from its start, as far as it was written, within the checkpoint memory.
  pub stale_slot: (usize, usize),
  /// The log from the next frame's start, as far as it was written, within the log memory.
  pub log_past_tail: (usize, usize),
}

impl Journal {
  /// What no restart reads and was written since it was last given back (A-117, A-226), for checkpoint memory
  /// `checkpoint_total` and log memory `log_total` bytes long; `None` before the first checkpoint commits. A restart
  /// takes the newer CRC-valid slot, then the log's frames from its start while each is CRC-valid and names that
  /// checkpoint, so the other slot and every byte past the tail are dead: frames there are torn or name an older base.
  /// Returning their pages leaves them reading zeros, which fail the same checks. Only what was written since the last
  /// give-back ([`Journal::given_back_from`]) can be resident, so the ranges stop there: a give-back costs what the
  /// publications since wrote, not the memories' length (18 GB a shard on a Mac Studio, whose mark of it took 18–21 ms
  /// of the shard's CPU at every publication before A-226).
  pub fn dead_ranges(&self, checkpoint_total: usize, log_total: usize) -> Option<DeadRanges> {
    let committed = self.committed?;
    let tail = self.tail.min(log_total);
    let (stale_offset, stale_len) = committed.stale_slot(checkpoint_total);
    Some(DeadRanges {
      stale_slot: (stale_offset, stale_len.min(self.stale_written)),
      log_past_tail: (tail, self.log_written.min(log_total).saturating_sub(tail)),
    })
  }

  /// Records what of the ranges [`Journal::dead_ranges`] last named was given back, each from the top down so a
  /// give-back may take them a slice at a time: of the stale slot, everything from `stale_from` bytes into it; of the
  /// log, everything from offset `log_from` (never below the tail). `None` records nothing for that region: a range the
  /// memory refused stays owed, asked for at the next give-back.
  pub fn given_back_from(&mut self, stale_from: Option<usize>, log_from: Option<usize>) {
    if let Some(from) = stale_from {
      self.stale_written = self.stale_written.min(from);
    }
    if let Some(from) = log_from {
      self.log_written = self.log_written.min(from.max(self.tail));
    }
  }

  /// Test support: what a restart leaves: every byte of the dead regions may be resident, so the next give-back covers
  /// them whole (A-226).
  pub fn forget_what_was_given_back(&mut self) {
    self.stale_written = UNKNOWN_EXTENT;
    self.log_written = UNKNOWN_EXTENT;
  }

  /// The journal after a checkpoint of `bytes` committed into what was the stale slot: that slot is now the committed
  /// one, written as far as this image or the longer one it held and never gave back; the slot that was committed is
  /// the stale one, written as far as its image reached; the log restarts, its old frames still written.
  fn checkpointed(&self, committed: CommittedSlot, bytes: usize) -> Journal {
    Journal {
      committed: Some(committed),
      generation: committed.generation,
      tail: 0,
      checkpoint_bytes: bytes,
      logged_bytes: 0,
      committed_written: bytes.max(self.stale_written),
      stale_written: if self.committed.is_some() {
        self.committed_written
      } else {
        // Nothing was committed, so neither slot's earlier writes are known: the other slot is given back whole.
        UNKNOWN_EXTENT
      },
      log_written: self.log_written,
    }
  }

  /// The generation of the last committed publication: what a transport's write log since it is stamped with.
  pub fn generation(&self) -> u64 {
    self.generation
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
    *self = self.checkpointed(committed, bytes);
    Ok(bytes)
  }

  /// Starts a checkpoint streamed into the free slot of `checkpoints` (staging in `stage`): the caller encodes the shard
  /// image into it (`ShardImage::encode_start`, each volume, `encode_finish`) and hands it to
  /// [`Journal::finish_checkpoint`], so the publisher never holds the image, nor its encoding, whole. Dropping the
  /// stream instead abandons the checkpoint: the committed slot is untouched and the journal unchanged.
  pub fn begin_checkpoint<'s, S: ImageWrite + ?Sized>(
    &self,
    checkpoints: &'s mut S,
    stage: &'s mut Vec<u8>,
  ) -> Result<StreamedImage<'s, S>, VfsError> {
    let known = self
      .committed
      .map(|committed| committed.advanced_to(self.generation));
    StreamedImage::begin(checkpoints, known, stage)
  }

  /// Commits a checkpoint begun by [`Journal::begin_checkpoint`] and restarts the log; the journal is unchanged on any
  /// refusal (the slot could not take the image: [`VfsError::NoSpace`]).
  pub fn finish_checkpoint<S: ImageWrite + ?Sized>(
    &mut self,
    image: StreamedImage<'_, S>,
  ) -> Result<usize, VfsError> {
    let (bytes, committed) = image.finish()?;
    *self = self.checkpointed(committed, bytes);
    Ok(bytes)
  }

  /// Appends `delta` to `log` over the committed checkpoint, encoding it in `scratch` (cleared first; the caller keeps
  /// it, so a barrier's delta reuses one buffer where its own grew by doubling: one allocation and seven reallocations
  /// per create, 2026-10-06 `create_heap`). Refuses [`VfsError::NoSpace`] when the frame does not fit the log's room
  /// (the caller checkpoints instead), and refuses before writing anything when no checkpoint is committed; the journal
  /// is unchanged on any refusal.
  pub fn append<S: ImageWrite + ?Sized>(
    &mut self,
    log: &mut S,
    delta: &ShardDelta,
    scratch: &mut Vec<u8>,
  ) -> Result<usize, VfsError> {
    if self.committed.is_none() {
      return Err(VfsError::RecoveryIncomplete);
    }
    scratch.clear();
    delta.encode(scratch);
    self.append_encoded(log, scratch)
  }

  /// [`Journal::append`] for a shard delta already encoded (`ShardDelta::encode_start` … `encode_finish`): the daemon
  /// streams each volume's publication into its scratch, never building the delta as a value.
  pub fn append_encoded<S: ImageWrite + ?Sized>(
    &mut self,
    log: &mut S,
    body: &[u8],
  ) -> Result<usize, VfsError> {
    let Some(committed) = self.committed else {
      return Err(VfsError::RecoveryIncomplete);
    };
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
    let crc = crc32c_append(crc32c(&prefix), body);
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
    log.image_write(payload_at.saturating_add(LOG_PREFIX), body)?;
    // The next frame's length word reads zero until a frame is written there, so a replay stops at this tail.
    let mut written = end;
    if end.saturating_add(LOG_HEADER) <= log.image_len() {
      log.image_write(end, &[0u8; LOG_HEADER])?;
      written = end.saturating_add(LOG_HEADER);
    }
    log.image_write(self.tail, &header)?;
    self.generation = generation;
    self.tail = end;
    self.logged_bytes = self.logged_bytes.saturating_add(total);
    self.log_written = self.log_written.max(written);
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
      // A predecessor wrote these memories, as far as this journal cannot tell: each is given back whole once.
      committed_written: UNKNOWN_EXTENT,
      stale_written: UNKNOWN_EXTENT,
      log_written: UNKNOWN_EXTENT,
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
  // A CRC-valid frame of another build's delta version is refused by its version, never read as torn: ending the
  // replay there would recover the checkpoint without the acknowledged deltas after it.
  if let Some((magic, version)) = crate::recover::read_header(&body)
    && magic == DELTA_MAGIC
    && version != DELTA_VERSION
  {
    return Err(VfsError::RecoveryVersion {
      found: version,
      expected: DELTA_VERSION,
    });
  }
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
