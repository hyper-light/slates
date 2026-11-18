//! Version 0 of a green over a base, placed at `f > 0` a slice at a time (§4.16 "Commit" and "Apply on holders"; A-217,
//! A-286; §4.3 "Bounded work everywhere").
//!
//! A green over a base names its origin as version 0's inputs: the canonical encoding of every file, directory, mode
//! and link of the base snapshot (`slates_merge::origin`), cut into the inputs archive the merge record names by its
//! manifest identity. At `f > 0` the owner made those inputs in one poll — the held snapshot walked again, encoded and
//! archived — at the green's creation, and again on every pass of the record plane until version 0 placed; it shipped
//! them whole in one message; and each holder restored, decoded and seeded them into its replica in one poll. Over the
//! whole Linux kernel (97,229 files, 1.67 GB) each of those is seconds of one shard's poll, past the watchdog's
//! verdict on a shard that holds the CPU that long (A-232): one primary and two replicas could not set the kernel up
//! at all (GAPS, "The many-client load test from 10,000 to 100,000 clients", 2026-11-17). None of it ran under any
//! test, since every fleet test's green was a scratch one.
//!
//! Now:
//! - The owner makes the inputs once, a share at a time, in a task of the green's shard ([`OriginInputsJob`]): the
//!   held snapshot walked a unit at a time (`merge_service::HeldWalk`), its canonical bytes written a share at a time
//!   (`slates_merge::origin::EncodeCursor`) into an archive cut and hashed as they arrive ([`InputsBuilder`]), and the
//!   origin's identity hashed from the same bytes and checked against the one the green recorded (A-217). A taken-over
//!   green's recorded origin bytes are cut the same way. The archive is kept until every candidate holds version 0,
//!   and version 0's record is enqueued once it is made; nothing of the green ships before.
//! - The record plane ships at most a shipment's bound of the chunks a target lacks (`merge_service`), and a target
//!   that cannot accept the record yet answers with what it holds of the inputs, so a kernel-sized origin crosses in
//!   many bounded shipments.
//! - Each holder seeds its replica a share at a time ([`ReplicaSeed`]): the held chunks gathered one at a time
//!   (`ContentHold::held_chunk`, never the whole archive copied out in one call), the origin decoded a share at a time
//!   (`slates_merge::origin::DecodeCursor`), the engine seeded a share at a time (`slates_merge::engine::OriginLoad`),
//!   and the head identity compared with the record's. Its first slice runs as the record arrives, so an origin that
//!   seeds within one slice is acknowledged in the exchange that carried it, as before.
//!
//! Inputs archives are chunked at the archive's largest chunk ([`inputs_chunk_bytes`]): an origin's manifest is then
//! about 1,600 extents for the kernel (some 90 KB), small enough to offer with every bounded shipment and for a holder
//! to decode in a slice, where the host page made it 100,000 to 400,000 extents (6 to 23 MB).
//!
//! **A restarted holder rebuilds its replica the same way (A-313).** A warm restart keeps the merge records a holder
//! accepted (its acceptor, §4.8 "Recovery") and the inputs it held (AUD-29-59), but not the replica, the engine every
//! later version is recomputed on, which lived in memory alone: the holder answered every later record
//! `merge.out_of_order`, for good, and the green went on at `f + 1` only while its other holders answered. Now a holder
//! with records and no replica rebuilds it from them ([`rebuild_replica`]): version 0 seeded as above (or the empty
//! engine of a green with no origin), then each version it accepted recomputed from the increment it holds, an extent
//! gathered or a version recomputed a unit, every version's head compared with its record's identity. The records are
//! read a version at a time (`Acceptor::value_at`), so nothing of a long chain is decoded up front.
//!
//! At `f = 0` none of it runs: the local append is the placement (R8).

use slates_archive::Archive;
use slates_archive::format::{Chunk, Encoding, MAX_CHUNK_BYTES};
use slates_archive::manifest::{Entry, Extent, Node, NodeMeta};
use slates_db::catalog::VolumeId as DbVolumeId;
use slates_db::register::{HostId, ObjectId, Record};
use slates_ipc::protocol::Refusal;
use slates_merge::engine::{Green, Increment, OriginLoad, Outcome};
use slates_merge::origin::{DecodeCursor, EncodeCursor, Origin};

use crate::merge_service::{
  HeldWalk, INPUTS_ENTRY_NAME, INPUTS_UNDECODABLE, INPUTS_UNHELD, MergeRecordValue,
  RECOMPUTE_MISMATCH,
};
use crate::state::ShardState;

/// Counted on an owner each time version 0's inputs of a green were made whole (A-286): the non-vacuity counter of the
/// made-once path, one a green.
/// Format: a refusal name in the daemon's status report.
pub(crate) const ORIGIN_INPUTS_MADE: &str = "merge.origin_inputs.made";
/// Counted on an owner for each slice of making a green's version 0 inputs: more than one shows they were made in
/// slices, not one poll.
/// Format: a refusal name in the daemon's status report.
pub(crate) const ORIGIN_INPUTS_SLICES: &str = "merge.origin_inputs.slices";
/// Counted on an owner whose version 0 inputs could not be made: the held snapshot's walk refused, or it did not give
/// the origin the green recorded. The green's records never ship from here (fatal-and-loud, logged).
/// Format: a refusal name in the daemon's status report.
pub(crate) const ORIGIN_INPUTS_REFUSED: &str = "merge.origin_inputs.refused";
/// Counted on an owner whose task to make version 0's inputs could not be spawned (its task arena full): the job stays
/// owed, and the record plane's next pass over the shard starts it again.
/// Format: a refusal name in the daemon's status report.
pub(crate) const ORIGIN_INPUTS_UNSPAWNED: &str = "merge.origin_inputs.unspawned";
/// Counted on a holder each time it seeded a green's replica at version 0 from the inputs it holds (A-286).
/// Format: a refusal name in the daemon's status report.
pub(crate) const REPLICA_SEEDED: &str = "merge.replica.seeded";
/// Counted on a holder for each slice of building a replica (seeding version 0, A-286, or rebuilding a chain, A-313):
/// more than one shows it was built in slices.
/// Format: a refusal name in the daemon's status report.
pub(crate) const REPLICA_SEED_SLICES: &str = "merge.replica.seed_slices";
/// Counted on a holder asked to accept a green's record while it is still building the replica: no acknowledgement
/// yet; the owner ships the record again.
/// Format: a refusal name in the daemon's status report.
pub(crate) const REPLICA_SEEDING: &str = "merge.replica.seeding";
/// Counted on a holder whose building task could not be spawned (its task arena full): the record is answered with no
/// acknowledgement, and the owner's next shipment starts the build again.
/// Format: a refusal name in the daemon's status report.
pub(crate) const REPLICA_SEED_UNSPAWNED: &str = "merge.replica.seed_unspawned";
/// Counted on a holder each time it rebuilt a green's replica past version 0 from the records it accepted and the
/// inputs it holds (A-313): the non-vacuity counter of a restarted holder's rebuild, one a rebuild.
/// Format: a refusal name in the daemon's status report.
pub(crate) const REPLICA_REBUILT: &str = "merge.replica.rebuilt";
/// Counted on a holder for each version after 0 a rebuild recomputed from the increment it holds (A-313).
/// Format: a refusal name in the daemon's status report.
pub(crate) const REPLICA_REBUILT_VERSIONS: &str = "merge.replica.rebuilt_versions";
/// Counted on a holder whose rebuild met a version, between 0 and the newest it accepted, that its records lack: no
/// replica is installed, and the green's later records stay unacknowledged here (A-313).
/// Format: a refusal name in the daemon's status report.
pub(crate) const REPLICA_RECORDS_SHORT: &str = "merge.replica.records_short";

/// Derived: the most raw bytes one chunk of an inputs archive holds — the archive's largest chunk, the most any reader
/// admits (`slates_archive::format::MAX_CHUNK_BYTES`, AUD-29-13; 1 MiB). An inputs archive is one stream that no other
/// content shares a chunk with, so the largest chunk gives the fewest extents: the kernel's origin (1.67 GB) is about
/// 1,600 extents and a 90 KB manifest, where the host page made it 100,000 to 400,000 extents and 6 to 23 MB, too large
/// to offer with every bounded shipment or for a holder to read in one slice.
pub(crate) fn inputs_chunk_bytes() -> usize {
  usize::try_from(MAX_CHUNK_BYTES).unwrap_or(usize::MAX)
}

/// An inputs archive made as its bytes arrive (§4.16 "Commit", A-286): the bytes cut into raw chunks of
/// [`inputs_chunk_bytes`], each hashed as it fills, so an origin's encoding is archived a share at a time and never
/// gathered whole. A stream pushed in any cut gives the archive its whole bytes give
/// (`merge_service::MergeShardState::inputs_archive`), chunk for chunk.
#[derive(Debug, Default)]
pub(crate) struct InputsBuilder {
  /// The bytes past the last whole chunk.
  tail: Vec<u8>,
  /// The chunks cut so far, in order.
  chunks: Vec<Chunk>,
  /// The inputs file's extents so far, one a chunk.
  extents: Vec<Extent>,
  /// The bytes cut into chunks so far: the next chunk's offset in the file.
  offset: u64,
}

impl InputsBuilder {
  /// Takes `bytes`, the next of the stream, cutting every chunk they fill.
  pub(crate) fn push(&mut self, mut bytes: &[u8]) {
    let chunk_bytes = inputs_chunk_bytes();
    while !bytes.is_empty() {
      let room = chunk_bytes.saturating_sub(self.tail.len()).max(1);
      let (taken, rest) = bytes.split_at_checked(room).unwrap_or((bytes, &[][..]));
      bytes = rest;
      if self.tail.is_empty() && taken.len() == chunk_bytes {
        // A whole chunk straight from the stream: one copy, sized exactly.
        self.cut(taken.to_vec());
        continue;
      }
      if self.tail.is_empty() {
        // Sized to a whole chunk at once, so a stream of small writes never regrows it.
        self.tail.reserve_exact(chunk_bytes);
      }
      self.tail.extend_from_slice(taken);
      if self.tail.len() >= chunk_bytes {
        let whole = std::mem::take(&mut self.tail);
        self.cut(whole);
      }
    }
  }

  /// Takes the whole of `bytes` as the stream, every chunk cut exactly from it: what a caller with the whole inputs in
  /// hand (a version's increment) builds with.
  pub(crate) fn whole(bytes: &[u8]) -> InputsBuilder {
    let mut builder = InputsBuilder::default();
    for piece in bytes.chunks(inputs_chunk_bytes().max(1)) {
      builder.cut(piece.to_vec());
    }
    builder
  }

  /// Cuts `piece` into the next chunk and its extent.
  fn cut(&mut self, piece: Vec<u8>) {
    let chunk = Archive::raw_chunk(piece);
    self.extents.push(Extent {
      offset: self.offset,
      len: chunk.raw_len,
      chunk: chunk.identity,
      chunk_offset: 0,
    });
    self.offset = self.offset.saturating_add(chunk.raw_len);
    self.chunks.push(chunk);
  }

  /// The archive: a manifest of one file named `INPUTS_ENTRY_NAME` over the chunks in order, with `page` and
  /// `created_unix` in its header (informational: neither is in the manifest identity). The last chunk is the bytes
  /// past the last whole one, sized exactly; an empty stream has no chunk and no extent.
  pub(crate) fn finish(mut self, created_unix: u64, page: u32) -> Archive {
    if !self.tail.is_empty() {
      let mut last = std::mem::take(&mut self.tail);
      last.shrink_to_fit();
      self.cut(last);
    }
    let chunk_max = u32::try_from(MAX_CHUNK_BYTES).unwrap_or(u32::MAX);
    Archive {
      base_page_size: page,
      chunk_min: chunk_max,
      chunk_max,
      created_unix,
      volume_id: 0,
      snapshot_id: 0,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: NodeMeta::default(),
      manifest: Node::Directory(vec![Entry {
        name: INPUTS_ENTRY_NAME.to_owned(),
        meta: NodeMeta {
          size: self.offset,
          ..NodeMeta::default()
        },
        node: Node::File(self.extents),
      }]),
      chunks: self.chunks,
    }
  }
}

/// Version 0's inputs of a green on its owner shard at `f > 0` (`MergeShardState::origin_inputs`, A-286).
pub(crate) enum OriginInputs {
  /// A job no task runs: just made, or left when its task could not be spawned; the record plane starts it
  /// ([`start_every_owed`]).
  Owed(Box<OriginInputsJob>),
  /// A task of this shard is making them; it owns the job.
  Making,
  /// Made: the archive version 0's record names by its manifest identity, kept until every candidate holds version 0.
  Made {
    /// The inputs archive.
    archive: Archive,
  },
  /// They could not be made: the held snapshot's walk refused, or it did not give the origin the green recorded (a
  /// changed or corrupt base). The green's records never ship from this owner: fatal-and-loud for its placement here,
  /// as a recomputation mismatch is for the green on a holder.
  Refused,
}

/// Whether `green`'s version 0 has inputs an [`OriginInputsJob`] makes: its origin held as a reference to its base
/// snapshot (A-217), or recorded as bytes (a green taken over from another host). A scratch green's has none.
pub(crate) fn has_origin(state: &ShardState, green: DbVolumeId) -> bool {
  let partition = state.db.partition();
  partition.green_origin_ref(green).is_some() || partition.green_origin(green).is_some()
}

/// Whether nothing of `object` may ship yet because its version 0's inputs are not made (owed, being made, or
/// refused): a holder takes a green's records in version order, so a later version shipped first would be refused.
pub(crate) fn inputs_pending(state: &ShardState, object: ObjectId) -> bool {
  matches!(
    state.merge.origin_inputs.get(&object),
    Some(OriginInputs::Owed(_) | OriginInputs::Making | OriginInputs::Refused)
  )
}

/// The made archive of `object`'s version 0 inputs, while it is kept.
pub(crate) fn made_archive(state: &ShardState, object: ObjectId) -> Option<&Archive> {
  match state.merge.origin_inputs.get(&object) {
    Some(OriginInputs::Made { archive }) => Some(archive),
    _ => None,
  }
}

/// Starts making `green`'s version 0 inputs (A-286), whose record is `record` with no inputs named yet: walked from the
/// snapshot it holds, else cut from the origin bytes it records. Once they are made, the record is enqueued naming
/// them. Nothing is started for a green whose inputs are already owed, being made, made or refused.
pub(crate) fn make_origin_inputs(
  state: &mut ShardState,
  green: DbVolumeId,
  record: MergeRecordValue,
) {
  let object = ObjectId(green.bytes);
  if state.merge.origin_inputs.contains_key(&object) {
    return;
  }
  let stage = match state.db.partition().green_origin_ref(green).copied() {
    Some(held) => match HeldWalk::begin(state, &held) {
      Ok(walk) => MakeStage::Walk(Box::new(walk)),
      Err(refusal) => {
        state
          .merge
          .origin_inputs
          .insert(object, OriginInputs::Making);
        refuse(state, object, &refusal);
        return;
      }
    },
    None => MakeStage::Recorded { offset: 0 },
  };
  let job = OriginInputsJob {
    object,
    green,
    stage,
    builder: InputsBuilder::default(),
    record: Some(record),
  };
  state
    .merge
    .origin_inputs
    .insert(object, OriginInputs::Owed(Box::new(job)));
  start_owed(state, object);
}

/// Spawns the task that makes `object`'s owed inputs. The task takes the job from the state when it first runs, so a
/// task that could not be spawned leaves the job owed, counted, for [`start_every_owed`]; a second task spawned before
/// the first ran finds nothing to take and ends.
fn start_owed(state: &mut ShardState, object: ObjectId) {
  if !matches!(
    state.merge.origin_inputs.get(&object),
    Some(OriginInputs::Owed(_))
  ) {
    return;
  }
  if slates_rt::futures::spawn(run_origin_inputs(object))
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    state.count(ORIGIN_INPUTS_UNSPAWNED, 1);
  }
}

/// Starts every job this shard owes (each pass of the record plane over the shard calls it): a job whose task could
/// not be spawned is not left behind.
pub(crate) fn start_every_owed(state: &mut ShardState) {
  let owed: Vec<ObjectId> = state
    .merge
    .origin_inputs
    .iter()
    .filter(|(_, inputs)| matches!(inputs, OriginInputs::Owed(_)))
    .map(|(object, _)| *object)
    .collect();
  for object in owed {
    start_owed(state, object);
  }
}

/// Takes `object`'s owed job, marking its inputs as being made; `None` when there is none to take.
fn take_owed(state: &mut ShardState, object: ObjectId) -> Option<Box<OriginInputsJob>> {
  match state.merge.origin_inputs.remove(&object)? {
    OriginInputs::Owed(job) => {
      state
        .merge
        .origin_inputs
        .insert(object, OriginInputs::Making);
      Some(job)
    }
    other => {
      state.merge.origin_inputs.insert(object, other);
      None
    }
  }
}

/// Records that `object`'s version 0 inputs could not be made (`refusal`), counted and logged; nothing for a green
/// destroyed meanwhile, whose entry is gone.
fn refuse(state: &mut ShardState, object: ObjectId, refusal: &Refusal) {
  if !state.merge.origin_inputs.contains_key(&object) {
    return;
  }
  state
    .merge
    .origin_inputs
    .insert(object, OriginInputs::Refused);
  state.count(ORIGIN_INPUTS_REFUSED, 1);
  slates_machine::log_line!(
    "slates-server: version 0's inputs of green {} could not be made ({refusal:?}); its merge records do not ship",
    hex(&object.0)
  );
}

/// Whether the shard's state is gone for good on this thread (its shutdown, or a fenced shard), so a task stepping a
/// job there ends rather than waiting on it.
fn state_gone() -> bool {
  matches!(
    crate::state::try_with_state(|_| ()),
    Err(crate::state::StateAccess::Absent | crate::state::StateAccess::Fenced)
  )
}

/// The task that makes `object`'s version 0 inputs a slice at a time (A-286), owned by the inputs' entry: it takes the
/// job when it first runs, steps it a slice a turn (`crate::image::sliced`), and ends once the job records its outcome
/// in the entry (made, or refused) or the entry is gone (the green destroyed), or the shard's state is.
async fn run_origin_inputs(object: ObjectId) {
  let Some(mut job) = crate::state::with_state_counted(|s| take_owed(s, object)).flatten() else {
    return;
  };
  loop {
    // A borrow refused after the slice ran discards only the result: the job's own stage says whether it ended.
    let _ = crate::state::with_state_counted(|s| {
      s.count(ORIGIN_INPUTS_SLICES, 1);
      crate::image::sliced(s, |s| job.step(s))
    });
    if job.ended() || state_gone() {
      return;
    }
    slates_rt::futures::yield_now().await;
  }
}

/// A green's version 0 inputs being made a share at a time on its owner shard (A-286).
pub(crate) struct OriginInputsJob {
  /// The green's object.
  object: ObjectId,
  /// The green.
  green: DbVolumeId,
  /// Where the making has reached.
  stage: MakeStage,
  /// The archive so far.
  builder: InputsBuilder,
  /// Version 0's record, every field but its inputs' identity, until it is enqueued.
  record: Option<MergeRecordValue>,
}

/// Where making a green's version 0 inputs has reached.
enum MakeStage {
  /// The held snapshot being walked into the origin (A-217).
  Walk(Box<HeldWalk>),
  /// The origin's canonical bytes being written into the archive, the origin's identity hashed from the same bytes.
  /// Boxed: a BLAKE3 hasher's state is about two kilobytes.
  Encode(Box<OriginEncode>),
  /// A taken-over green's recorded origin bytes being cut into the archive, from this offset.
  Recorded {
    /// The bytes already cut.
    offset: usize,
  },
  /// Ended: the inputs made or refused, or nothing left to make them for.
  Ended,
}

/// A walked origin's canonical bytes being written into the inputs archive ([`MakeStage::Encode`]).
struct OriginEncode {
  /// The walked origin.
  origin: Origin,
  /// Where the encoding has reached.
  cursor: EncodeCursor,
  /// The hash of the bytes written so far.
  hasher: blake3::Hasher,
  /// The identity the green recorded, which the bytes must hash to.
  identity: [u8; 32],
}

impl OriginInputsJob {
  /// Whether the job has ended.
  fn ended(&self) -> bool {
    matches!(self.stage, MakeStage::Ended)
  }

  /// One unit, its outcome recorded in the inputs' entry when it ends the job; `Some` once ended. Never refuses its
  /// caller: a refusal is recorded ([`refuse`]) and ends the job.
  fn step(&mut self, state: &mut ShardState) -> Result<Option<()>, Refusal> {
    match self.unit(state) {
      Ok(None) => Ok(None),
      Ok(Some(())) => {
        self.stage = MakeStage::Ended;
        Ok(Some(()))
      }
      Err(refusal) => {
        refuse(state, self.object, &refusal);
        self.stage = MakeStage::Ended;
        Ok(Some(()))
      }
    }
  }

  /// One unit: a directory listed or a file read of the walk, or a share of the encoding or of the recorded bytes cut;
  /// `Some` once the inputs are made, or there is nothing left to make them for.
  fn unit(&mut self, state: &mut ShardState) -> Result<Option<()>, Refusal> {
    // The green destroyed meanwhile: its entry is gone, and nothing is made for it.
    if !matches!(
      state.merge.origin_inputs.get(&self.object),
      Some(OriginInputs::Making)
    ) {
      return Ok(Some(()));
    }
    match std::mem::replace(&mut self.stage, MakeStage::Ended) {
      MakeStage::Walk(walk) => self.walk(state, walk),
      MakeStage::Encode(encode) => self.encode(state, encode),
      MakeStage::Recorded { offset } => self.cut_recorded(state, offset),
      MakeStage::Ended => Ok(Some(())),
    }
  }

  /// One unit of the held snapshot's walk; once walked, the encoding begins.
  fn walk(&mut self, state: &ShardState, mut walk: Box<HeldWalk>) -> Result<Option<()>, Refusal> {
    if !walk.step(state)? {
      self.stage = MakeStage::Walk(walk);
      return Ok(None);
    }
    let (origin, identity) = walk.into_walked()?;
    // The walk canonicalizes what it gives, so a cursor is always had here.
    let cursor = EncodeCursor::new(&origin).ok_or(Refusal::ContentUnavailable)?;
    self.stage = MakeStage::Encode(Box::new(OriginEncode {
      origin,
      cursor,
      hasher: blake3::Hasher::new(),
      identity,
    }));
    Ok(None)
  }

  /// One share of the origin's encoding, into the archive and the identity's hash; once all of it is written, the
  /// identity is checked and the inputs are made.
  fn encode(
    &mut self,
    state: &mut ShardState,
    mut encode: Box<OriginEncode>,
  ) -> Result<Option<()>, Refusal> {
    let share = hash_share(state);
    let OriginEncode {
      origin,
      cursor,
      hasher,
      ..
    } = &mut *encode;
    let builder = &mut self.builder;
    let written = cursor.step(origin, share, &mut |bytes| {
      hasher.update(bytes);
      builder.push(bytes);
    });
    if !written {
      self.stage = MakeStage::Encode(encode);
      return Ok(None);
    }
    // The snapshot must still give the origin the green was created from (A-217): its identity is the hash of exactly
    // these bytes.
    if *encode.hasher.finalize().as_bytes() != encode.identity {
      return Err(Refusal::ContentUnavailable);
    }
    self.made(state);
    Ok(Some(()))
  }

  /// One share of a taken-over green's recorded origin bytes cut into the archive, from `offset`; once all are cut, the
  /// inputs are made.
  fn cut_recorded(&mut self, state: &mut ShardState, offset: usize) -> Result<Option<()>, Refusal> {
    let share = hash_share(state);
    let (end, total) = {
      let bytes = state
        .db
        .partition()
        .green_origin(self.green)
        .ok_or(Refusal::NotFound)?;
      let end = offset.saturating_add(share).min(bytes.len());
      self
        .builder
        .push(bytes.get(offset..end).ok_or(Refusal::ContentUnavailable)?);
      (end, bytes.len())
    };
    if end < total {
      self.stage = MakeStage::Recorded { offset: end };
      return Ok(None);
    }
    self.made(state);
    Ok(Some(()))
  }

  /// The inputs made: the archive kept for the record plane, and version 0's record enqueued naming it.
  fn made(&mut self, state: &mut ShardState) {
    let builder = std::mem::take(&mut self.builder);
    let archive = builder.finish(0, crate::merge_service::archive_page(state));
    let manifest = archive.manifest_identity();
    state
      .merge
      .origin_inputs
      .insert(self.object, OriginInputs::Made { archive });
    state.count(ORIGIN_INPUTS_MADE, 1);
    if let Some(mut record) = self.record.take() {
      record.inputs = Some(manifest);
      crate::merge_service::insert_pending(state, self.object, record, None);
    }
  }
}

/// The bytes one unit of hash-bound work takes on this shard now (an encoding share, whose bytes are hashed twice — the
/// identity and the chunks — and an engine's seeding share): its archive slice, at least one byte.
fn hash_share(state: &ShardState) -> usize {
  usize::try_from(state.config.archive_slice_bytes())
    .unwrap_or(usize::MAX)
    .max(1)
}

/// The bytes one unit of copy-bound work takes on this shard now (a decoding share): its copy slice, at least one byte.
fn copy_share(state: &ShardState) -> usize {
  usize::try_from(state.config.copy_slice_bytes())
    .unwrap_or(usize::MAX)
    .max(1)
}

/// Accepts version 0's record of a green whose inputs this holder holds (§4.16 "Apply on holders", A-286): seeds its
/// replica a share at a time from them ([`ReplicaSeed`]). The seed's first slice runs now, so a replica seeded within
/// it is accepted in this exchange, as before; else a task of this shard goes on seeding, owned by the green's entry in
/// `seeding`, and the record is answered with no acknowledgement — as is every record of the green until the seed
/// ends, after which the owner's next shipment is acknowledged. The holder's inputs answer
/// (`merge_service::inputs_held`) tells the owner meanwhile that the inputs are held, so it ships no more of them.
pub(crate) fn seed_version_zero(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
  value: &MergeRecordValue,
  manifest: [u8; 32],
) -> Vec<u8> {
  let object = record.object;
  if state.merge.seeding.contains(&object) {
    state.count(REPLICA_SEEDING, 1);
    return Vec::new();
  }
  if !state.held_content.holds_manifest(object, &manifest) {
    state.count(INPUTS_UNHELD, 1);
    return Vec::new();
  }
  let ended = build(state, ReplicaSeed::new(object, manifest, value.identity));
  if ended && state.merge.replicas.contains_key(&object) {
    return crate::fleet::accept_held_record(state, local, peer_host, record);
  }
  Vec::new()
}

/// What a holder with no replica of a green made of it when one of the green's records arrived ([`rebuild_replica`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rebuild {
  /// The replica is installed, at this head version.
  Installed(u64),
  /// Not now: a task of this shard goes on building it, or the build ended without one (counted why). The record is
  /// answered with no acknowledgement, and the owner ships it again.
  Later,
  /// This holder accepted no record of the green: there is nothing to rebuild from.
  Nothing,
}

/// Rebuilds `object`'s replica on a holder that accepted the green's records but holds no replica of it (§4.16 "Apply
/// on holders", §4.8 "Recovery", A-313): a warm restart keeps the records a holder accepted and the inputs it held
/// (AUD-29-59), not the engine, so every later record was refused `merge.out_of_order` for good. Version 0 is seeded from
/// its held inputs, or is the empty engine of a green with no origin; then every later version this holder accepted, up
/// to the newest, is recomputed from the increment it holds and compared with its record's identity ([`ReplicaSeed`]).
/// The first slice runs now, so a short chain is rebuilt in the exchange that asked; else a task of this shard goes on,
/// owned by the green's entry in `seeding`.
pub(crate) fn rebuild_replica(state: &mut ShardState, object: ObjectId) -> Rebuild {
  if state.merge.seeding.contains(&object) {
    state.count(REPLICA_SEEDING, 1);
    return Rebuild::Later;
  }
  let Some(through) = newest_accepted(state, object) else {
    return Rebuild::Nothing;
  };
  let Some(zero) = accepted_value(state, object, 0) else {
    state.count(REPLICA_RECORDS_SHORT, 1);
    return Rebuild::Later;
  };
  build(
    state,
    ReplicaSeed::rebuild(object, zero.inputs, zero.identity, through),
  );
  match state.merge.replicas.get(&object) {
    Some(replica) => Rebuild::Installed(replica.head()),
    None => Rebuild::Later,
  }
}

/// The newest version of `object` this holder accepted (its acceptor's highest position), if it accepted any.
fn newest_accepted(state: &ShardState, object: ObjectId) -> Option<u64> {
  state
    .holder_records
    .get(&object)?
    .highest(object)
    .map(|(version, _)| version)
}

/// The merge record this holder accepted for `object` at `version`, if its acceptor holds that position and it is that
/// version's: one lookup, never a walk of the green's whole ledger.
fn accepted_value(state: &ShardState, object: ObjectId, version: u64) -> Option<MergeRecordValue> {
  let bytes = state
    .holder_records
    .get(&object)?
    .value_at(object, version)?;
  MergeRecordValue::from_record_bytes(bytes).filter(|value| value.version == version)
}

/// Runs `seed`'s first slice now; if the seed does not end in it, a task of this shard goes on with it, owned by the
/// green's entry in `seeding`. Whether the seed ended now.
fn build(state: &mut ShardState, mut seed: ReplicaSeed) -> bool {
  let object = seed.object;
  state.count(REPLICA_SEED_SLICES, 1);
  // The seed never refuses its caller: its outcome is in its stage.
  let _ = crate::image::sliced(state, |s| seed.step(s));
  if seed.ended() {
    return true;
  }
  if slates_rt::futures::spawn(run_seed(seed))
    .and_then(slates_rt::futures::detach)
    .is_ok()
  {
    state.merge.seeding.insert(object);
  } else {
    state.count(REPLICA_SEED_UNSPAWNED, 1);
  }
  false
}

/// The task that goes on building a replica after its first slice (A-286, A-313), owned by the green's entry in
/// `seeding`, which it removes once the build ends or the shard's state is gone.
async fn run_seed(mut seed: ReplicaSeed) {
  let object = seed.object;
  loop {
    // A borrow refused after the slice ran discards only the result: the seed's own stage says whether it ended.
    let _ = crate::state::with_state_counted(|s| {
      s.count(REPLICA_SEED_SLICES, 1);
      crate::image::sliced(s, |s| seed.step(s))
    });
    if seed.ended() || state_gone() {
      break;
    }
    slates_rt::futures::yield_now().await;
  }
  let _ = crate::state::with_state_counted(|s| s.merge.seeding.remove(&object));
}

/// A holder's replica of a green being built a share at a time from what the holder holds (§4.16 "Apply on holders"):
/// version 0 seeded from its inputs — the chunks gathered one at a time from the content hold, the origin decoded a
/// share at a time, the engine seeded a share at a time (A-286) — or the empty engine of a green with no origin; then,
/// for a restarted holder's rebuild, every later version it accepted recomputed from the increment it holds (A-313).
/// Each version's head identity is compared with its record's. A build borrows nothing between its units: each finds
/// the held inputs and the accepted records again.
pub(crate) struct ReplicaSeed {
  /// The green's object.
  object: ObjectId,
  /// Version 0's inputs manifest identity, as its record names it; none for a green with no origin, whose version 0 is
  /// the empty engine.
  manifest: Option<[u8; 32]>,
  /// The green's head identity at version 0, as its record names it.
  identity: [u8; 32],
  /// The newest version to build: 0 for a seed of version 0 alone (A-286), else the newest this holder accepted, each
  /// version after 0 recomputed from its accepted record's held increment (A-313).
  through: u64,
  /// The newest inputs the build read, which the hold must still hold for the replica to be installed: a green
  /// forgotten meanwhile (its tombstone, a takeover's reclaim) forgot them too. None while the build has read none.
  held: Option<[u8; 32]>,
  /// The version the build is at, for its log line.
  reached: u64,
  /// Where the build has reached.
  stage: SeedStage,
}

/// Where building a replica has reached.
enum SeedStage {
  /// Nothing read yet.
  Begin,
  /// Version 0's held chunks being gathered into the origin's bytes, a chunk a unit.
  Gather(Gather),
  /// The origin being decoded from the gathered bytes.
  Decode {
    /// The origin's canonical bytes.
    bytes: Vec<u8>,
    /// Where the decoding has reached.
    cursor: DecodeCursor,
  },
  /// The replica being seeded from the decoded origin. Boxed: it holds the engine being seeded.
  Load(Box<OriginLoad>),
  /// Version 0 built: the later versions being recomputed on it from the increments this holder holds (A-313). Boxed:
  /// it holds the engine.
  Chain(Box<ChainStep>),
  /// Ended: the replica built, or the build refused (counted).
  Ended,
}

/// A rebuild's recomputation of the version after its replica's head ([`SeedStage::Chain`]).
struct ChainStep {
  /// The replica so far, at the version before `version`.
  replica: Green,
  /// The version being recomputed.
  version: u64,
  /// Its record's head identity and its increment's bytes gathered so far; none until its record is read.
  reading: Option<([u8; 32], Gather)>,
}

/// One held inputs file being gathered into its bytes, an extent a unit ([`SeedStage::Gather`], [`ChainStep`]).
struct Gather {
  /// The inputs' manifest identity: the held archive whose chunks are read.
  manifest: [u8; 32],
  /// The inputs file's extents still to read, the next last.
  extents: Vec<Extent>,
  /// The bytes gathered so far.
  bytes: Vec<u8>,
  /// The inputs file's size, as its manifest declares it.
  size: u64,
}

impl Gather {
  /// Reads `object`'s held inputs `manifest`: the inputs file's extents to gather, and room for its declared size. Each
  /// extent holds at most one chunk's bytes, so the size is checked against what the extents can hold before anything is
  /// reserved for it, and a reservation the allocator refuses is the build's refusal, never an abort.
  fn of(state: &ShardState, object: ObjectId, manifest: [u8; 32]) -> Result<Gather, &'static str> {
    let archive = state
      .held_content
      .held_manifest(state.store.content.arena(), object, &manifest)
      .ok_or(INPUTS_UNHELD)?;
    let (size, mut extents) = inputs_file(archive).ok_or(INPUTS_UNDECODABLE)?;
    let most = u64::try_from(extents.len())
      .unwrap_or(u64::MAX)
      .saturating_mul(MAX_CHUNK_BYTES);
    if size > most {
      return Err(INPUTS_UNDECODABLE);
    }
    let mut bytes = Vec::new();
    bytes
      .try_reserve_exact(usize::try_from(size).map_err(|_| INPUTS_UNDECODABLE)?)
      .map_err(|_| INPUTS_UNDECODABLE)?;
    extents.reverse();
    Ok(Gather {
      manifest,
      extents,
      bytes,
      size,
    })
  }

  /// Gathers the next extent's bytes from the hold; once every extent is in, the inputs' bytes if they are the declared
  /// size, with a test's corruption fault applied (`merge_service::corrupt_if_faulted`).
  fn step(
    &mut self,
    state: &mut ShardState,
    object: ObjectId,
  ) -> Result<Option<Vec<u8>>, &'static str> {
    let Some(extent) = self.extents.pop() else {
      if u64::try_from(self.bytes.len()).ok() != Some(self.size) {
        return Err(INPUTS_UNDECODABLE);
      }
      let mut bytes = std::mem::take(&mut self.bytes);
      crate::merge_service::corrupt_if_faulted(state, &mut bytes);
      return Ok(Some(bytes));
    };
    gather_extent(state, object, &self.manifest, &extent, &mut self.bytes)?;
    Ok(None)
  }
}

impl ReplicaSeed {
  /// A seed of `object`'s replica at version 0 from the inputs `manifest` names, which must reproduce the head
  /// `identity` (A-286).
  fn new(object: ObjectId, manifest: [u8; 32], identity: [u8; 32]) -> ReplicaSeed {
    ReplicaSeed::rebuild(object, Some(manifest), identity, 0)
  }

  /// A rebuild of `object`'s replica through version `through` (A-313): version 0 from the inputs `manifest` names (none
  /// for a green with no origin), which must reproduce the head `identity`, then every later version from the records
  /// this holder accepted.
  fn rebuild(
    object: ObjectId,
    manifest: Option<[u8; 32]>,
    identity: [u8; 32],
    through: u64,
  ) -> ReplicaSeed {
    ReplicaSeed {
      object,
      manifest,
      identity,
      through,
      held: manifest,
      reached: 0,
      stage: SeedStage::Begin,
    }
  }

  /// Whether the build has ended.
  fn ended(&self) -> bool {
    matches!(self.stage, SeedStage::Ended)
  }

  /// One unit, its outcome recorded when it ends the build: `Some` once ended. Never refuses its caller.
  fn step(&mut self, state: &mut ShardState) -> Result<Option<()>, Refusal> {
    match self.unit(state) {
      Ok(None) => Ok(None),
      Ok(Some(())) => {
        self.stage = SeedStage::Ended;
        Ok(Some(()))
      }
      Err(reason) => {
        self.fail(state, reason);
        self.stage = SeedStage::Ended;
        Ok(Some(()))
      }
    }
  }

  /// Counts why the build ended without a replica; a recomputation mismatch refuses the green on this holder for good,
  /// loudly (§4.16 "mismatch fatal-and-loud"), as a mismatch of a record arriving does.
  fn fail(&self, state: &mut ShardState, reason: &'static str) {
    state.count(reason, 1);
    if reason == RECOMPUTE_MISMATCH {
      state.merge.refused.insert(self.object);
      state.merge.replicas.remove(&self.object);
      slates_machine::log_line!(
        "slates-server: merge record for green {} version {}: the rebuilt identity does not match the record; the green is refused on this holder",
        hex(&self.object.0),
        self.reached
      );
    }
  }

  /// One unit: a manifest read, a chunk gathered, a share decoded or seeded, or a later version's record read, its
  /// increment's chunk gathered or the version recomputed; `Some` once the replica is installed. `Err` names the counter
  /// for why it could not be.
  fn unit(&mut self, state: &mut ShardState) -> Result<Option<()>, &'static str> {
    match std::mem::replace(&mut self.stage, SeedStage::Ended) {
      SeedStage::Begin => self.begin(state),
      SeedStage::Gather(gather) => self.gather(state, gather),
      SeedStage::Decode { bytes, cursor } => self.decode(state, bytes, cursor),
      SeedStage::Load(load) => self.load(state, load),
      SeedStage::Chain(step) => self.chain(state, step),
      SeedStage::Ended => Ok(Some(())),
    }
  }

  /// Reads version 0's held inputs' manifest, or builds the empty engine of a green with no origin.
  fn begin(&mut self, state: &mut ShardState) -> Result<Option<()>, &'static str> {
    let Some(manifest) = self.manifest else {
      return self.zero_built(state, Green::new());
    };
    self.stage = SeedStage::Gather(Gather::of(state, self.object, manifest)?);
    Ok(None)
  }

  /// Gathers the next extent of version 0's inputs; once they are whole, the decoding begins.
  fn gather(
    &mut self,
    state: &mut ShardState,
    mut gather: Gather,
  ) -> Result<Option<()>, &'static str> {
    self.stage = match gather.step(state, self.object)? {
      Some(bytes) => SeedStage::Decode {
        bytes,
        cursor: DecodeCursor::new(),
      },
      None => SeedStage::Gather(gather),
    };
    Ok(None)
  }

  /// Decodes a share of the origin; once decoded, the bytes go and the seeding begins.
  fn decode(
    &mut self,
    state: &ShardState,
    bytes: Vec<u8>,
    mut cursor: DecodeCursor,
  ) -> Result<Option<()>, &'static str> {
    let decoded = cursor
      .step(&bytes, copy_share(state))
      .map_err(|_| INPUTS_UNDECODABLE)?;
    self.stage = match decoded {
      Some(origin) => SeedStage::Load(Box::new(OriginLoad::new(origin))),
      None => SeedStage::Decode { bytes, cursor },
    };
    Ok(None)
  }

  /// Seeds a share of the replica; once seeded, version 0 is built.
  fn load(
    &mut self,
    state: &mut ShardState,
    mut load: Box<OriginLoad>,
  ) -> Result<Option<()>, &'static str> {
    let Some(replica) = load.step(hash_share(state)) else {
      self.stage = SeedStage::Load(load);
      return Ok(None);
    };
    self.zero_built(state, replica)
  }

  /// Version 0 is built: checked against its record, then installed, or the later versions' recomputation begins.
  fn zero_built(
    &mut self,
    state: &mut ShardState,
    mut replica: Green,
  ) -> Result<Option<()>, &'static str> {
    self.check(state, &mut replica, self.identity)?;
    if self.through == 0 {
      self.install(state, replica);
      return Ok(Some(()));
    }
    self.reached = 1;
    self.stage = SeedStage::Chain(Box::new(ChainStep {
      replica,
      version: 1,
      reading: None,
    }));
    Ok(None)
  }

  /// One unit of recomputing the version after the replica's head (A-313): its accepted record read and its increment's
  /// gathering begun, an extent of the increment gathered, or the increment decoded, submitted as exactly that version
  /// and its head checked against the record's identity. The replica is installed once the newest version is.
  fn chain(
    &mut self,
    state: &mut ShardState,
    mut step: Box<ChainStep>,
  ) -> Result<Option<()>, &'static str> {
    let Some((identity, mut gather)) = step.reading.take() else {
      let value = accepted_value(state, self.object, step.version).ok_or(REPLICA_RECORDS_SHORT)?;
      let manifest = value.inputs.ok_or(INPUTS_UNDECODABLE)?;
      step.reading = Some((value.identity, Gather::of(state, self.object, manifest)?));
      self.stage = SeedStage::Chain(step);
      return Ok(None);
    };
    let Some(bytes) = gather.step(state, self.object)? else {
      step.reading = Some((identity, gather));
      self.stage = SeedStage::Chain(step);
      return Ok(None);
    };
    let increment = Increment::decode(&bytes).map_err(|_| INPUTS_UNDECODABLE)?;
    match step.replica.submit(&increment) {
      Outcome::Accepted { version } if version == step.version => {}
      // The owner accepted it, and this holder too before its restart; anything else has diverged.
      _ => return Err(RECOMPUTE_MISMATCH),
    }
    self.held = Some(gather.manifest);
    self.check(state, &mut step.replica, identity)?;
    state.count(REPLICA_REBUILT_VERSIONS, 1);
    if step.version >= self.through {
      self.install(state, step.replica);
      return Ok(Some(()));
    }
    step.version = step.version.saturating_add(1);
    self.reached = step.version;
    self.stage = SeedStage::Chain(step);
    Ok(None)
  }

  /// Whether `replica`'s head is `identity` on a green this holder still backs. A green whose newest inputs the build
  /// read are no longer held was forgotten meanwhile (a tombstone or a takeover's reclaim forgot everything held for it
  /// and dropped its replica while this build was past reading the hold): unheld, and never refused, so it is checked
  /// before the identity; a replica installed after that is a whole origin nothing removes (CLAUDE.md §2 item 8). A
  /// different identity is a recomputation mismatch.
  fn check(
    &self,
    state: &ShardState,
    replica: &mut Green,
    identity: [u8; 32],
  ) -> Result<(), &'static str> {
    if let Some(held) = self.held
      && !state.held_content.holds_manifest(self.object, &held)
    {
      return Err(INPUTS_UNHELD);
    }
    if replica.head_identity() != identity {
      return Err(RECOMPUTE_MISMATCH);
    }
    Ok(())
  }

  /// Installs the built `replica`: a green this holder refused meanwhile, or replicates already, keeps what it has.
  fn install(&self, state: &mut ShardState, replica: Green) {
    if state.merge.refused.contains(&self.object) {
      return;
    }
    state.merge.replicas.entry(self.object).or_insert(replica);
    if self.manifest.is_some() {
      state.count(REPLICA_SEEDED, 1);
    }
    if self.through > 0 {
      state.count(REPLICA_REBUILT, 1);
    }
  }
}

/// The inputs file of an inputs archive's manifest: its declared size and its extents. `None` for any other shape:
/// an inputs archive is one file named `INPUTS_ENTRY_NAME`, and nothing else is read as one.
fn inputs_file(manifest: Archive) -> Option<(u64, Vec<Extent>)> {
  let Node::Directory(entries) = manifest.manifest else {
    return None;
  };
  let [entry] = <[Entry; 1]>::try_from(entries).ok()?;
  match entry.node {
    Node::File(extents) if entry.name == INPUTS_ENTRY_NAME => Some((entry.meta.size, extents)),
    _ => None,
  }
}

/// The all-zero chunk identity a hole extent names, which reads as zero bytes.
const HOLE: [u8; 32] = [0; 32];

/// Appends `extent`'s bytes to `bytes`, read from the chunk the hold keeps for `object`'s `manifest`. The extents must
/// follow each other from the file's start, each within its chunk: anything else is not an inputs archive this
/// holder's owner made, and is refused.
fn gather_extent(
  state: &ShardState,
  object: ObjectId,
  manifest: &[u8; 32],
  extent: &Extent,
  bytes: &mut Vec<u8>,
) -> Result<(), &'static str> {
  if u64::try_from(bytes.len()).ok() != Some(extent.offset) || extent.len > MAX_CHUNK_BYTES {
    return Err(INPUTS_UNDECODABLE);
  }
  let len = usize::try_from(extent.len).map_err(|_| INPUTS_UNDECODABLE)?;
  if extent.chunk == HOLE {
    let end = bytes.len().checked_add(len).ok_or(INPUTS_UNDECODABLE)?;
    bytes.resize(end, 0);
    return Ok(());
  }
  let chunk = state
    .held_content
    .held_chunk(state.store.content.arena(), object, manifest, &extent.chunk)
    .ok_or(INPUTS_UNHELD)?;
  // The hold verified every chunk against its identity as it was staged, so a raw chunk's payload is its content; an
  // encoded one is decoded, and verified again by the decode.
  let content = match chunk.encoding {
    Encoding::Raw => chunk.payload,
    Encoding::Lz4 | Encoding::Zstd => Archive::content(&chunk).map_err(|_| INPUTS_UNDECODABLE)?,
  };
  let from = usize::try_from(extent.chunk_offset).map_err(|_| INPUTS_UNDECODABLE)?;
  let piece = from
    .checked_add(len)
    .and_then(|to| content.get(from..to))
    .ok_or(INPUTS_UNDECODABLE)?;
  bytes.extend_from_slice(piece);
  Ok(())
}

/// Lowercase hexadecimal, for a log line.
fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The streams pushed in [`a_stream_pushed_in_any_cut_makes_the_archive_its_whole_bytes_make`]: about three and a
  /// half chunks, so whole chunks, a chunk's boundary and a short last chunk are all met.
  fn stream() -> Vec<u8> {
    let chunk = inputs_chunk_bytes();
    (0..chunk.saturating_mul(7) / 2)
      .map(|at| u8::try_from(at % 251).unwrap_or(0))
      .collect()
  }

  /// §4.16 "Commit" (A-286): the owner archives an origin's encoding as the encoding is written, a share at a time, and
  /// the record names the archive by its manifest identity, so the streamed archive must be the one the whole bytes
  /// make. Do: push one stream into a builder in cuts of 1 byte, of a few bytes, of a chunk less one, of exactly a
  /// chunk, of a chunk and one, and whole; and an empty stream. Expect: every cut makes the archive byte for byte
  /// that the whole bytes make, with whole chunks of the inputs chunk size and the short one last; the empty stream
  /// has no chunk.
  #[test]
  fn a_stream_pushed_in_any_cut_makes_the_archive_its_whole_bytes_make() {
    let bytes = stream();
    let chunk = inputs_chunk_bytes();
    let whole = InputsBuilder::whole(&bytes).finish(0, 4096);
    let sizes: Vec<u64> = whole.chunks.iter().map(|chunk| chunk.raw_len).collect();
    let full = u64::try_from(chunk).unwrap();
    assert_eq!(
      sizes,
      [full, full, full, full / 2],
      "whole chunks, then the short one"
    );
    for cut in [1, 7, chunk - 1, chunk, chunk + 1, bytes.len()] {
      // The one-byte cut walks a whole chunk a byte at a time; once is enough to show it.
      let stream = if cut == 1 {
        bytes.get(..chunk + 3).unwrap()
      } else {
        &bytes[..]
      };
      let mut builder = InputsBuilder::default();
      for piece in stream.chunks(cut) {
        builder.push(piece);
      }
      assert_eq!(
        builder.finish(0, 4096).encode(),
        InputsBuilder::whole(stream).finish(0, 4096).encode(),
        "pushed in cuts of {cut} bytes"
      );
    }
    let empty = InputsBuilder::default().finish(0, 4096);
    assert!(empty.chunks.is_empty(), "an empty stream has no chunk");
    assert_eq!(
      empty.manifest_identity(),
      InputsBuilder::whole(&[])
        .finish(0, 4096)
        .manifest_identity()
    );
  }

  /// §4.16 "Apply on holders" (A-286), CLAUDE.md §4 hostile input: a holder reads an inputs archive's one file and
  /// nothing else. Do: read the file of a made archive, of a manifest with a second entry, of one whose entry has
  /// another name, and of a directory entry. Expect: the made one gives its size and extents; every other shape none.
  #[test]
  fn only_an_inputs_archives_one_file_is_read_as_inputs() {
    let made = InputsBuilder::whole(&stream()).finish(0, 4096);
    let (size, extents) = inputs_file(made.clone()).unwrap();
    assert_eq!(size, u64::try_from(stream().len()).unwrap());
    assert_eq!(extents.len(), made.chunks.len());
    let Node::Directory(entries) = made.manifest.clone() else {
      panic!("a made archive is a directory of one file");
    };
    let entry = entries[0].clone();
    let mut two = made.clone();
    two.manifest = Node::Directory(vec![entry.clone(), entry.clone()]);
    assert_eq!(inputs_file(two), None, "a second entry");
    let mut renamed = made.clone();
    renamed.manifest = Node::Directory(vec![Entry {
      name: "other".to_owned(),
      ..entry.clone()
    }]);
    assert_eq!(inputs_file(renamed), None, "another name");
    let mut nested = made;
    nested.manifest = Node::Directory(vec![Entry {
      node: Node::Directory(Vec::new()),
      ..entry
    }]);
    assert_eq!(inputs_file(nested), None, "a directory");
  }

  /// An origin whose encoding spans several inputs chunks: three files of most of a chunk each, in a directory.
  fn origin_over_chunks() -> Origin {
    let chunk = inputs_chunk_bytes();
    let mut origin = Origin::default();
    for file in 0..3u8 {
      origin
        .files
        .push((format!("dir/file-{file}"), vec![file; chunk - 1000]));
    }
    origin.dirs.push("dir".to_owned());
    origin.modes.push(("dir/file-0".to_owned(), 0o644));
    origin.canonicalize();
    origin
  }

  /// Holds `origin`'s inputs archive for `object` in `state`'s content hold, as a placement would: its manifest identity.
  fn hold_inputs(state: &mut ShardState, object: ObjectId, origin: &Origin) -> [u8; 32] {
    hold_encoded(state, object, &origin.encode())
  }

  /// Holds the inputs archive of `bytes` (an encoded origin or increment) for `object` in `state`'s content hold, as a
  /// placement would: its manifest identity.
  fn hold_encoded(state: &mut ShardState, object: ObjectId, bytes: &[u8]) -> [u8; 32] {
    let archive = InputsBuilder::whole(bytes).finish(0, 4096);
    state
      .held_content
      .hold(
        &mut crate::content_holder::hold_space(&mut state.store),
        object,
        slates_cluster::content::Placed::default(),
        archive,
      )
      .unwrap()
  }

  /// An increment based on version `base` that creates `path` holding `bytes`, under the identity `[id; 32]`.
  fn creating(path: &str, bytes: &[u8], id: u8, base: u64) -> Increment {
    use slates_merge::ops_doc::{Op, OpKind, OpsDoc};
    let mut doc = OpsDoc::default();
    let path = doc.paths.intern(path);
    doc.ops.push(Op {
      kind: OpKind::Create,
      flags: 0,
      path,
      at: 0,
      len: 0,
      src: u64::MAX,
    });
    doc.ops.push(Op {
      kind: OpKind::Insert,
      flags: 0,
      path,
      at: 0,
      len: u64::try_from(bytes.len()).unwrap(),
      src: 0,
    });
    Increment {
      id: [id; 32],
      base,
      doc,
      post_state: bytes.to_vec(),
      evidence: Vec::new(),
    }
  }

  /// Gives `state` a holder's acceptor of `object` that accepted each of `records` (its version, its inputs manifest —
  /// none for a scratch green's version 0 — and the head identity after it), as a holder that recomputed them did.
  fn accept_records(
    state: &mut ShardState,
    object: ObjectId,
    records: &[(u64, Option<[u8; 32]>, [u8; 32])],
  ) {
    let owner = HostId(2);
    let mut acceptor = slates_db::register::Acceptor::new(
      HostId(1),
      slates_db::register::Authority {
        generation: 0,
        owner,
      },
    );
    for (version, inputs, identity) in records {
      let value = MergeRecordValue {
        version: *version,
        increment: [0; 32],
        base: version.saturating_sub(1),
        inputs: *inputs,
        identity: *identity,
        evidence: Vec::new(),
        name: "green".to_owned(),
        require_evidence: false,
        owner: slates_db::catalog::Principal::Uid {
          host: owner,
          uid: 0,
        },
        completion: None,
      };
      acceptor
        .accept(&Record {
          owner,
          object,
          sequence: *version,
          epoch: slates_db::HostEpoch(1),
          generation: 0,
          value: value.to_record_bytes(),
        })
        .unwrap();
    }
    state.holder_records.insert(object, acceptor);
  }

  /// Steps `seed` a unit at a time until it ends: the units it took.
  fn seed_to_end(state: &mut ShardState, seed: &mut ReplicaSeed) -> usize {
    let mut units = 0;
    while !seed.ended() {
      let _ = seed.step(state);
      units += 1;
    }
    units
  }

  /// The count of `name` on `state`.
  fn count_of(state: &ShardState, name: &str) -> u64 {
    state.refusals.get(name).copied().unwrap_or(0)
  }

  /// §4.16 "Apply on holders", D-27 "mismatch fatal-and-loud" (A-286): a holder seeds a green's replica at version 0
  /// from the inputs it holds a unit at a time, and installs only a replica whose identity is the record's. Do: hold an
  /// origin's inputs archive spanning several chunks and seed it a unit at a time against the identity of the green the
  /// whole origin seeds; seed the same inputs held for another green against a wrong identity; seed a third green whose
  /// inputs are not held. Expect: the first replica is installed with the whole seeding's identity, over more units than
  /// the inputs have chunks, counted seeded once; the second installs nothing and refuses its green for good, counted a
  /// mismatch; the third installs nothing, counted unheld.
  #[test]
  fn a_replica_is_seeded_a_unit_at_a_time_and_only_with_its_records_identity() {
    let origin = origin_over_chunks();
    let chunks = InputsBuilder::whole(&origin.encode())
      .finish(0, 4096)
      .chunks
      .len();
    let (identical, units, mismatched, unheld, counts) =
      crate::daemon::audit_on_shard(move |state| {
        let expected = Green::with_origin(origin.clone()).head_identity();
        let (good, bad, absent) = (ObjectId([31; 16]), ObjectId([32; 16]), ObjectId([33; 16]));
        let manifest = hold_inputs(state, good, &origin);
        let other = hold_inputs(state, bad, &origin);
        let mut seed = ReplicaSeed::new(good, manifest, expected);
        let units = seed_to_end(state, &mut seed);
        let identical = state
          .merge
          .replicas
          .get_mut(&good)
          .map(Green::head_identity)
          == Some(expected);
        let mut wrong = ReplicaSeed::new(bad, other, [7; 32]);
        seed_to_end(state, &mut wrong);
        let mismatched = (
          state.merge.replicas.contains_key(&bad),
          state.merge.refused.contains(&bad),
        );
        let mut missing = ReplicaSeed::new(absent, manifest, expected);
        seed_to_end(state, &mut missing);
        let unheld = state.merge.replicas.contains_key(&absent);
        let counts = [
          count_of(state, REPLICA_SEEDED),
          count_of(state, RECOMPUTE_MISMATCH),
          count_of(state, INPUTS_UNHELD),
        ];
        (identical, units, mismatched, unheld, counts)
      });
    assert!(chunks > 1, "the inputs span several chunks ({chunks})");
    assert!(
      identical,
      "the seeded replica's identity is the whole seeding's"
    );
    assert!(units > chunks, "seeded a unit at a time ({units} units)");
    assert_eq!(
      mismatched,
      (false, true),
      "a wrong identity installs nothing and refuses the green"
    );
    assert!(!unheld, "inputs not held seed nothing");
    assert_eq!(counts, [1, 1, 1], "seeded, mismatched, unheld: each once");
  }

  /// §4.16 "Apply on holders", CLAUDE.md §2 item 8 (A-286): a replica is seeded only for a green this holder still
  /// backs. A tombstone or a takeover's reclaim forgets everything held for the green and drops its replica
  /// (`crate::tombstone::on_held`, `crate::takeover`'s reclaim) while a seed may be past reading the hold, decoding or
  /// loading what it gathered; a seed that installed after that would keep a whole origin's replica that nothing
  /// removes. Do: seed a green's replica from the inputs held for it until the seed has gathered them, forget everything
  /// held for the green as a tombstone does, and step the seed to its end. Expect: no replica installed, counted unheld
  /// and never seeded, and the green not refused.
  #[test]
  fn a_seed_whose_green_is_forgotten_meanwhile_installs_nothing() {
    let origin = origin_over_chunks();
    let (installed, refused, counts) = crate::daemon::audit_on_shard(move |state| {
      let expected = Green::with_origin(origin.clone()).head_identity();
      let object = ObjectId([34; 16]);
      let manifest = hold_inputs(state, object, &origin);
      let mut seed = ReplicaSeed::new(object, manifest, expected);
      while !matches!(
        seed.stage,
        SeedStage::Decode { .. } | SeedStage::Load(_) | SeedStage::Ended
      ) {
        let _ = seed.step(state);
      }
      state.held_content.forget_object(
        &mut crate::content_holder::hold_space(&mut state.store),
        object,
      );
      seed_to_end(state, &mut seed);
      (
        state.merge.replicas.contains_key(&object),
        state.merge.refused.contains(&object),
        [
          count_of(state, REPLICA_SEEDED),
          count_of(state, INPUTS_UNHELD),
        ],
      )
    });
    assert!(!installed, "a forgotten green's replica is never installed");
    assert!(
      !refused,
      "a forgotten green is not refused: nothing of it is kept"
    );
    assert_eq!(counts, [0, 1], "counted unheld, never seeded");
  }

  /// §4.16 "Apply on holders", §4.8 "Recovery" (A-313): a holder restarted over its segment keeps the records it
  /// accepted and the inputs it held, not its replica, and rebuilds the replica from them a unit at a time. Do: hold a
  /// based green's origin (several chunks) and two increments, give the holder the records of versions 0 to 2 with the
  /// identities a serial engine reaches, and rebuild through version 2 a unit at a time. Expect the replica at version 2
  /// with the serial engine's identity, over more units than the inputs have chunks and versions, counted seeded once,
  /// rebuilt once and two versions recomputed.
  #[test]
  fn a_restarted_holders_replica_is_rebuilt_from_the_records_and_inputs_it_kept() {
    let origin = origin_over_chunks();
    let chunks = InputsBuilder::whole(&origin.encode())
      .finish(0, 4096)
      .chunks
      .len();
    let first = creating("new-1", b"one", 1, 0);
    let second = creating("new-2", b"two", 2, 1);
    let (rebuilt, units, counts) = crate::daemon::audit_on_shard(move |state| {
      let mut serial = Green::with_origin(origin.clone());
      let zero = serial.head_identity();
      let accepted_first = matches!(serial.submit(&first), Outcome::Accepted { version: 1 });
      let one = serial.head_identity();
      let accepted_second = matches!(serial.submit(&second), Outcome::Accepted { version: 2 });
      let two = serial.head_identity();
      let object = ObjectId([35; 16]);
      let origin_inputs = hold_inputs(state, object, &origin);
      let first_inputs = hold_encoded(state, object, &first.encode());
      let second_inputs = hold_encoded(state, object, &second.encode());
      accept_records(
        state,
        object,
        &[
          (0, Some(origin_inputs), zero),
          (1, Some(first_inputs), one),
          (2, Some(second_inputs), two),
        ],
      );
      let mut seed = ReplicaSeed::rebuild(object, Some(origin_inputs), zero, 2);
      let units = seed_to_end(state, &mut seed);
      let rebuilt = state
        .merge
        .replicas
        .get_mut(&object)
        .map(|replica| (replica.head(), replica.head_identity()));
      (
        accepted_first && accepted_second && rebuilt == Some((2, two)),
        units,
        [
          count_of(state, REPLICA_SEEDED),
          count_of(state, REPLICA_REBUILT),
          count_of(state, REPLICA_REBUILT_VERSIONS),
        ],
      )
    });
    assert!(
      rebuilt,
      "the rebuilt replica is at version 2 with the serial engine's identity"
    );
    assert!(
      units > chunks + 2,
      "rebuilt a unit at a time ({units} units for {chunks} chunks and two versions)"
    );
    assert_eq!(counts, [1, 1, 2], "seeded, rebuilt, versions recomputed");
  }

  /// §4.16 "Apply on holders", D-27 "mismatch fatal-and-loud" (A-313): a rebuild installs a replica only from the whole
  /// run of records its holder accepted and only at their identities. Do: ask a holder with no records of a green to
  /// rebuild it; rebuild a scratch green whose holder accepted versions 0 and 2 but not 1; rebuild one whose version 2
  /// record names another identity than its increment reaches. Expect: nothing to rebuild from; the first installs
  /// nothing and is not refused, counted short; the second installs nothing and refuses its green for good, counted a
  /// mismatch.
  #[test]
  fn a_rebuild_installs_nothing_short_of_its_records_or_off_their_identities() {
    let first = creating("new-1", b"one", 1, 0);
    let second = creating("new-2", b"two", 2, 1);
    let (nothing, short, wrong, counts) = crate::daemon::audit_on_shard(move |state| {
      let mut serial = Green::new();
      let zero = serial.head_identity();
      let _ = serial.submit(&first);
      let one = serial.head_identity();
      let _ = serial.submit(&second);
      let two = serial.head_identity();
      let (absent, gap, diverged) = (ObjectId([36; 16]), ObjectId([37; 16]), ObjectId([38; 16]));
      let nothing = rebuild_replica(state, absent);
      let gap_second = hold_encoded(state, gap, &second.encode());
      accept_records(state, gap, &[(0, None, zero), (2, Some(gap_second), two)]);
      let mut seed = ReplicaSeed::rebuild(gap, None, zero, 2);
      seed_to_end(state, &mut seed);
      let short = (
        state.merge.replicas.contains_key(&gap),
        state.merge.refused.contains(&gap),
      );
      let diverged_first = hold_encoded(state, diverged, &first.encode());
      let diverged_second = hold_encoded(state, diverged, &second.encode());
      accept_records(
        state,
        diverged,
        &[
          (0, None, zero),
          (1, Some(diverged_first), one),
          (2, Some(diverged_second), [9; 32]),
        ],
      );
      let mut seed = ReplicaSeed::rebuild(diverged, None, zero, 2);
      seed_to_end(state, &mut seed);
      let wrong = (
        state.merge.replicas.contains_key(&diverged),
        state.merge.refused.contains(&diverged),
      );
      let counts = [
        count_of(state, REPLICA_RECORDS_SHORT),
        count_of(state, RECOMPUTE_MISMATCH),
        count_of(state, REPLICA_REBUILT),
      ];
      (nothing, short, wrong, counts)
    });
    assert_eq!(
      nothing,
      Rebuild::Nothing,
      "a holder with no records has nothing to rebuild"
    );
    assert_eq!(
      short,
      (false, false),
      "a missing version installs nothing and refuses nothing"
    );
    assert_eq!(
      wrong,
      (false, true),
      "another identity installs nothing and refuses the green"
    );
    assert_eq!(counts, [1, 1, 0], "short, mismatched, never rebuilt");
  }
}
