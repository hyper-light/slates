//! The daemon's `image build` and `image merge` (§4.6, A-160; the volume's side is [`slates_vfs::image`]): a snapshot
//! rendered as an ext2 image file in a new volume, for a VMM that shares no folder with its guest (Firecracker), and
//! the image a guest has written merged into a new clone of the snapshot it was built from. And `archive export`
//! (§4.11, A-441): a snapshot rendered as its content-addressed archive file in a new volume, for a consumer that keeps
//! a snapshot's bytes elsewhere (Mantle, kiln's object store) and reads the file a page at a time. It runs as a build
//! does: checked at once, walked in slices by a task of the volume's owner shard, its volume and the request's
//! completion committed as one atom. And its way back, `archive import` (A-453): the archive file a consumer wrote into
//! a volume of its own, its length admitted before a byte is read, then read a slice at a time into the decoder that
//! verifies it (`slates_archive::Decoding`, fed a slice a unit and never holding the file whole, A-494), restored and
//! laid into a new volume, each a slice a turn on that volume's owner shard (`Planning`, `Restoring`, the takeover's
//! `verbs::Populating`), the new volume and the completion one atom as an export's are.
//!
//! **Running.** Each verb checks what it can at once — the volume, the right, the name, the snapshot — and defers its
//! reply to a task of the volume's owner shard, as a granted landing does (AUD-29-03). The task steps the work a slice
//! at a time, yielding between slices, so the shard serves its clients while an image of any size is built or merged
//! (§4.3). A slice is bounded against the clock, half the live step quantum
//! ([`crate::config::DaemonConfig::copy_slice_ns`]), one unit at a time — an entry walked, a block written, a page
//! compared — since a unit's cost is not its bytes. Measured on 2026-10-24, slices bounded by 256 KiB of work ran
//! 230 µs to 1.5 ms, ten to sixty times the box's 24 µs. No unit's work follows a directory, a tree or a file (A-160),
//! so a slice overruns by a small unit at most: the longest measured was 18.5 µs at 100,000 nodes on the box. A retry
//! while it runs joins it.
//!
//! **Release.** After its reply, a job releases what it made — the walks' leftovers, the builder's tree, the merge's
//! tables and trees — in slices, [`SHED_PIECES`] pieces a unit ([`shed_in_slices`]), never in one drop: a finished
//! merge and its builder took one drop of 4.4–14.9 ms at 100,000 nodes before (2026-10-24).
//!
//! **One atom.** The new volume is not in the registry while it is made, so no client attaches a half-built image or
//! a half-merged clone. Its record, its publication and the request's completion record commit together, and then
//! the reply goes where the request waits. A refusal at any step discards the new volume and gives back its credits
//! and the pin it held. So does a task that ends early, through its job's drop.
//!
//! **Pins.** An image volume's catalog base (`BaseRecord::Image`) names the snapshot it was built from, which it pins
//! as a clone pins its origin. So the merge can clone that snapshot however long the guest runs. Recovery counts these
//! pins with the clones', and the image volume's destroy releases its pin. The merge's clone carries a lineage edge to
//! the snapshot, as every clone does.
//!
//! **What a merge reads.** The image volume's file at a snapshot taken when the merge starts, so a guest still
//! running cannot change the image under it; an image its guest has not cleanly unmounted is refused by the reader.
//! The snapshot is the volume core's alone, recorded nowhere, and destroyed when the merge ends (a restart's recovery
//! trims one a crash left).

use std::collections::BTreeMap;

use slates_db::catalog::{
  BaseRecord, NamePolicy as DbNamePolicy, PolicyRecord, Principal, Role, SizeClass as DbSizeClass,
  SnapshotId as DbSnapshotId, VolumeId as DbVolumeId, VolumeRecord, VolumeState,
};
use slates_image::{Builder, Identity, Progress, Segments, Timestamp, Walk, Walked};
use slates_ipc::protocol::{
  ARCHIVE_FILE, ArchiveFault, IMAGE_FILE, ImageMergeReport, NamePolicy, Refusal, ReplyBody,
  SizeClass, SnapshotId, VolumeId,
};
use slates_mem::Handle;
use slates_vfs::clock::{Clock, HostClock};
use slates_vfs::dir::Child;
use slates_vfs::ids::InodeNo;
use slates_vfs::image::{
  BuildIo, ImageContent, ImageError, Merge, MergeReport, SnapshotFile, SnapshotTree, SnapshotWalk,
};
use slates_vfs::names::NameEquivalence;
use slates_wire::request::RequestId;

use crate::error::{refusal_of_db, refusal_of_vfs};
use crate::merge_service::{ReplyRoute, StoreVerb, find_store_backed};
use crate::state::{ShardState, VolumeSlot};
use crate::verbs::{
  Admitted, core_snapshot, forbidden, fresh_volume_id, refused, rights_of, to_db_snapshot, wire_id,
};

/// Format: the image file's permissions: its owner's read and write alone, since it holds the whole snapshot.
const IMAGE_MODE: u32 = 0o600;
/// Format: nanoseconds in a second, between a volume clock's wall time and an image's creation time.
const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Counter: an image task's reply that could not be handed to its client's shard; the client's retry reads its
/// completion record.
const REPLY_UNDELIVERED: &str = "image.reply_undelivered";
/// Counter: a unit of image work (a directory opened, an entry, a block written, a chunk compared) that alone took
/// longer than a whole slice: a directory or a chunk past what one step should carry, the signal of a unit to split.
const UNITS_PAST_BUDGET: &str = "image.units_past_budget";
/// Format: the byte budget each unit's call is given: one, so every call does exactly one unit, and [`sliced`]
/// decides against the clock when the slice ends.
const ONE_UNIT: u64 = 1;

/// Runs `unit` — one unit of a phase, `Some` once the phase is whole — until the phase is whole or the slice's time is
/// spent: [`crate::config::DaemonConfig::copy_slice_ns`], half the live step quantum, as a granted landing's slice. The
/// clock is read after every unit, so a slice overruns its budget by its last unit at most.
pub(crate) fn sliced<T>(
  state: &mut ShardState,
  mut unit: impl FnMut(&mut ShardState) -> Result<Option<T>, Refusal>,
) -> Result<Option<T>, Refusal> {
  let budget = state.config.copy_slice_ns().max(1);
  let began = state.clock.monotonic_ns();
  loop {
    let unit_began = state.clock.monotonic_ns();
    let done = unit(state)?;
    let now = state.clock.monotonic_ns();
    if now.saturating_sub(unit_began) > budget {
      state.count(UNITS_PAST_BUDGET, 1);
    }
    if done.is_some() || now.saturating_sub(began) >= budget {
      return Ok(done);
    }
  }
}

/// The image builds and merges running on this shard (§4.6, A-160), and the green checkouts, which defer the same way
/// (§4.16 A-201, `crate::checkout`).
#[derive(Default)]
pub(crate) struct ImageState {
  /// The requests a build or merge runs for, by origin, client and sequence, with where its reply goes: the newest
  /// attempt's route, since a retry comes from where its client now waits. One entry per task, so bounded by the
  /// shard's task arena.
  pub(crate) in_flight: BTreeMap<(u64, u32, u32), Option<ReplyRoute>>,
}

/// A retry of a build or merge still running joins it: the verb does not run again, and the reply goes where this
/// attempt came from when it came from somewhere. Whether the request was running.
pub(crate) fn join_in_flight(state: &mut ShardState, origin: u64, id: RequestId) -> bool {
  let Some(route) = state
    .image
    .in_flight
    .get_mut(&(origin, id.client, id.sequence))
  else {
    return false;
  };
  if let Some(newest) = state.reply_route.take() {
    *route = Some(newest);
  }
  true
}

/// The refusal an image error is answered with.
fn refusal_of_image(error: ImageError) -> Refusal {
  let reason = match error {
    ImageError::Volume(error) => return refusal_of_vfs(&error),
    ImageError::Image(refusal) => format!("{refusal:?}"),
    ImageError::OverHostDirectory => {
      "a volume over a host directory: its snapshot may still read the disk".to_owned()
    }
    ImageError::NameNotUtf8 { path } => format!("a name not UTF-8 at {path}"),
    ImageError::TargetNotUtf8 { path } => format!("a symlink target not UTF-8 at {path}"),
    ImageError::DeviceNumberUnencodable { path, major, minor } => format!(
      "a device numbered {major}:{minor} at {path}, past the image's 12-bit major and 20-bit minor"
    ),
    ImageError::Inconsistent { reason } => format!("the merge disagreed with itself: {reason}"),
  };
  Refusal::ImageRefused { reason }
}

/// The refusal of an image step that found its shard's state unreachable, counted.
fn unreachable() -> Refusal {
  Refusal::Unsupported {
    feature: "an image step with its shard's state unreachable".to_owned(),
  }
}

/// Runs `step` on the shard's state, or refuses when the state is unreachable.
pub(crate) fn with<R>(
  step: impl FnOnce(&mut ShardState) -> Result<R, Refusal>,
) -> Result<R, Refusal> {
  match crate::state::with_state(step) {
    Some(result) => result,
    None => {
      // No state to count in; the refusal names the defect to the caller's completion instead.
      Err(unreachable())
    }
  }
}

/// The reply a deferred verb returns where its real reply comes later: never seen, since `run_recorded` sends nothing
/// for a verb that deferred its acceptance.
fn deferred() -> ReplyBody {
  ReplyBody::Acknowledged
}

/// Defers the current request's reply to `task`, spawned as an owned task of this shard: the request becomes
/// in flight, so a retry joins it. `Overloaded` when the shard's task arena is full.
pub(crate) fn defer(state: &mut ShardState, task: impl Future<Output = ()> + 'static) -> ReplyBody {
  let Some((origin, id)) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "an image verb outside a recorded request".to_owned(),
    });
  };
  let route = state.reply_route.take();
  match slates_rt::futures::spawn(task).and_then(slates_rt::futures::detach) {
    Ok(()) => {
      state
        .image
        .in_flight
        .insert((origin, id.client, id.sequence), route);
      state.acceptance_deferred = true;
      deferred()
    }
    Err(_) => refused(Refusal::Overloaded { shard: state.shard }),
  }
}

/// Commits a task's outcome as one atom — its effects (`produce`), the publication they need, and the request's
/// completion record — and returns the reply with where it goes.
pub(crate) fn complete(
  state: &mut ShardState,
  request: (u64, RequestId),
  produce: impl FnOnce(&mut ShardState) -> ReplyBody,
) -> (ReplyBody, Option<ReplyRoute>) {
  let route = take_route(state, request);
  let (reply, _) = commit_outcome(state, request, None, produce);
  (reply, route)
}

/// Commits a task's outcome that changed `volume` as [`complete`] does, except on a live-shipped volume (A-200): there
/// the effects commit now, but the completion is recorded and the reply delivered only once the volume's log is durable
/// on `f` candidates where the outcome's publication put it, by the deferral a recorded verb on such a volume takes
/// (`crate::live_ship::defer`, answered by `crate::live_ship::resolve`): `None` then. A refusal, or an outcome on a
/// volume that need not wait, is completed at once, as [`complete`] completes it.
pub(crate) fn complete_once_durable(
  state: &mut ShardState,
  request: (u64, RequestId),
  volume: DbVolumeId,
  produce: impl FnOnce(&mut ShardState) -> ReplyBody,
) -> Option<(ReplyBody, Option<ReplyRoute>)> {
  let route = take_route(state, request);
  match commit_outcome(state, request, Some(volume), produce) {
    (reply, Some(target)) => {
      crate::live_ship::defer(state, (volume, target), request, route, reply)
        .map(|refusal| (refusal, route))
    }
    (reply, None) => {
      // Answered without waiting for durability: the seat its task took, if any, is given back
      // (`crate::live_ship::take_seat`).
      crate::live_ship::release_seat(state, request);
      Some((reply, route))
    }
  }
}

/// Where the reply of in-flight `request` goes, taken, for a task that hands the request on to another deferral (a
/// bypass presentation's granted landing, A-285) rather than completing it here.
pub(crate) fn take_in_flight(
  state: &mut ShardState,
  request: (u64, RequestId),
) -> Option<ReplyRoute> {
  take_route(state, request)
}

/// Where the reply of in-flight `request` goes, taken: it is answered once, from here.
fn take_route(state: &mut ShardState, (origin, id): (u64, RequestId)) -> Option<ReplyRoute> {
  state
    .image
    .in_flight
    .remove(&(origin, id.client, id.sequence))
    .flatten()
}

/// One transaction: the outcome's effects (`produce`) and, unless the reply must wait for `live`'s durability, the
/// request's completion record. The reply, and the log position it waits for when it does.
fn commit_outcome(
  state: &mut ShardState,
  (origin, id): (u64, RequestId),
  live: Option<DbVolumeId>,
  produce: impl FnOnce(&mut ShardState) -> ReplyBody,
) -> (ReplyBody, Option<u64>) {
  state.db.begin();
  let reply = produce(state);
  let target = live
    .filter(|_| !matches!(reply, ReplyBody::Refused { .. }))
    .and_then(|volume| crate::live_ship::durable_target(state, volume));
  let reply = if target.is_some() {
    reply
  } else {
    crate::verbs::record_completion(state, origin, id, reply)
  };
  match state.db.commit(&mut state.segment) {
    Ok(_) => (reply, target),
    Err(e) => {
      // Nothing of the outcome is durable: counted, never recorded as the completion, so a retry runs again. The new
      // volume, whose record rolled back, is torn down with every other effect the rollback left unrecorded.
      let refusal = refusal_of_db(&e);
      state.count(crate::verbs::refusal_name(&refusal), 1);
      crate::verbs::reconcile_unpublished_effects(state);
      (refused(refusal), None)
    }
  }
}

/// Hands a task's reply to where its request waits, on its client's shard. A request with no route (a forward from
/// another node) reads its completion record instead.
pub(crate) fn deliver(state: &mut ShardState, reply: ReplyBody, route: Option<ReplyRoute>) {
  let Some(route) = route else {
    return;
  };
  if crate::xshard::hand_reply_home(route.shard, move || {
    crate::state::deliver(route.client_index, route.request, reply, true);
  })
  .is_err()
  {
    state.count(REPLY_UNDELIVERED, 1);
  }
}

/// Publishes the shard so the new volume's image precedes its completion (§4.8, AUD-05), as `dispatch` does for a
/// create; refused when the publication did not capture it.
pub(crate) fn publish_new_volume(state: &mut ShardState, id: DbVolumeId) -> Result<(), Refusal> {
  match crate::verbs::publish_shard(state) {
    Ok(published) if !published.captured(id) && !published.destroying(id) => {
      Err(refusal_of_vfs(&slates_vfs::VfsError::RecoveryIncomplete))
    }
    Ok(_) => Ok(()),
    Err(error) => Err(refusal_of_vfs(&error)),
  }
}

/// A volume's wall time as an image's creation time.
fn timestamp(nanos: i64) -> Timestamp {
  Timestamp {
    secs: nanos.div_euclid(NANOS_PER_SECOND),
    nanos: u32::try_from(nanos.rem_euclid(NANOS_PER_SECOND)).unwrap_or(0),
  }
}

/// A record's size class, as the protocol states it.
fn size_of_record(record: &VolumeRecord) -> SizeClass {
  match record.policy.size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  }
}

/// The slot of volume `id` on this shard.
fn slot_of(state: &ShardState, id: DbVolumeId) -> Result<Handle<VolumeSlot>, Refusal> {
  state.by_id.get(&id).copied().ok_or(Refusal::NotFound)
}

/// The volume a merge clones and the record it clones under: the image volume's base, which must be on this shard.
fn image_base(record: &VolumeRecord) -> Result<(DbVolumeId, DbSnapshotId), Refusal> {
  match &record.base {
    BaseRecord::Image { volume, snapshot } => Ok((*volume, *snapshot)),
    BaseRecord::Scratch | BaseRecord::Path { .. } => Err(Refusal::ImageRefused {
      reason: "not an image volume: it was not built from a snapshot".to_owned(),
    }),
  }
}

/// `ImageBuild` (§4.6, A-160): checks the volume, the right, the name and the snapshot at once, then builds the image
/// in a task of this shard and defers the reply to it.
pub(crate) fn build_verb(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: SnapshotId,
  name: String,
  capacity: u64,
) -> ReplyBody {
  if !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
  }
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Image) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  // An image pins its snapshot as a clone does, and copies the volume's content into a volume of the caller's: the
  // right a clone needs.
  if !rights_of(&record, principal).write {
    return forbidden("image_build");
  }
  // The name as its own partition holds it too: a volume another partition made under an alias there (A-236).
  if let Some(existing) = crate::verbs::name_held(state, &name) {
    return refused(Refusal::AlreadyExists { existing });
  }
  if matches!(record.base, BaseRecord::Path { .. }) {
    return refused(refusal_of_image(ImageError::OverHostDirectory));
  }
  let known = state
    .db
    .partition()
    .snapshot(record.id, to_db_snapshot(snapshot))
    .is_some();
  let present = state
    .volumes
    .get(handle)
    .is_ok_and(|slot| slot.volume.snapshot_info(core_snapshot(snapshot)).is_ok());
  if !known || !present {
    return refused(Refusal::NotFound);
  }
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "an image verb outside a recorded request".to_owned(),
    });
  };
  let (alias, image) = match name_alias(state, &name) {
    Ok(minted) => minted,
    Err(refusal) => return refused(refusal),
  };
  let job = BuildJob {
    request,
    principal: principal.clone(),
    source: record.id,
    record,
    snapshot,
    name,
    capacity,
    pinned: None,
    walk: None,
    inodes: Segments::new(),
    builder: None,
    admitted: None,
    file: InodeNo(0),
    image,
  };
  defer(state, run_build(job, alias))
}

/// A build between its slices: what it holds, released by [`BuildJob::release`] on a refusal or by its drop.
struct BuildJob {
  request: (u64, RequestId),
  principal: Principal,
  /// The volume imaged.
  source: DbVolumeId,
  /// Its record, whose size policy the image volume takes.
  record: VolumeRecord,
  snapshot: SnapshotId,
  name: String,
  capacity: u64,
  /// The volume slot whose snapshot this build pinned.
  pinned: Option<Handle<VolumeSlot>>,
  walk: Option<SnapshotWalk>,
  /// For each node of the tree written, its inode in the snapshot.
  inodes: Segments<InodeNo>,
  builder: Option<Builder>,
  /// The image volume, admitted and not yet recorded.
  admitted: Option<Admitted>,
  /// The image file in it.
  file: InodeNo,
  /// The image volume's id, minted when its image is planned (the image's identity carries it).
  image: Option<DbVolumeId>,
}

impl Drop for BuildJob {
  fn drop(&mut self) {
    if self.admitted.is_some() || self.pinned.is_some() {
      // A task that ended early: what it holds goes back. With the state unreachable (a shutdown) nothing can be,
      // and the process's end returns it.
      if crate::state::with_state(|s| self.release(s)).is_none() {
        self.admitted = None;
        self.pinned = None;
      }
    }
  }
}

impl BuildJob {
  /// Pins the snapshot and starts the walk, its entries held to the source's own entry allowance.
  fn start(&mut self, state: &mut ShardState) -> Result<(), Refusal> {
    let handle = slot_of(state, self.source)?;
    let entries = crate::verbs::entry_allowance(size_of_record(&self.record));
    let ShardState { volumes, store, .. } = state;
    let slot = volumes.get_mut(handle).map_err(|_| Refusal::NotFound)?;
    slot
      .volume
      .pin(core_snapshot(self.snapshot))
      .map_err(|error| refusal_of_vfs(&error))?;
    self.pinned = Some(handle);
    let walk = SnapshotWalk::new(
      &slot.volume,
      store,
      core_snapshot(self.snapshot),
      usize::try_from(entries).unwrap_or(usize::MAX),
    )
    .map_err(refusal_of_image)?;
    self.walk = Some(walk);
    Ok(())
  }

  /// The image volume's size class: the source's kind at `limit` bytes.
  fn size(&self, limit: u64) -> SizeClass {
    match self.record.policy.size {
      DbSizeClass::Bounded { .. } => SizeClass::Bounded { limit },
      DbSizeClass::Dynamic { .. } => SizeClass::Dynamic { max: limit },
    }
  }

  /// One slice of the walk; the tree once whole.
  fn walk_step(&mut self, state: &mut ShardState) -> Result<Option<SnapshotTree>, Refusal> {
    let source = self.source;
    let walk = self.walk.as_mut().ok_or_else(unreachable)?;
    sliced(state, |s| {
      let handle = slot_of(s, source)?;
      let slot = s.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
      walk
        .step(&slot.volume, &s.store, ONE_UNIT)
        .map_err(refusal_of_image)
    })
  }

  /// Plans the image of `tree` and admits its volume, sized to hold the image whole: its bytes rounded up to whole
  /// chunks, since a write is charged its chunk window (§4.5), so the guest can fill every block. The file is made at
  /// the image's length, which costs nothing until written. The image's identity is the image volume's id, with the
  /// host's wall time as its creation.
  fn prepare(&mut self, state: &mut ShardState, tree: SnapshotTree) -> Result<(), Refusal> {
    // The id its name's alias already names (A-236), or one minted here.
    let id = match self.image {
      Some(id) => id,
      None => fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?,
    };
    let SnapshotTree { tree, inodes } = tree;
    let identity = Identity {
      uuid: id.bytes,
      created: timestamp(HostClock::new().wall_ns()),
    };
    let builder = Builder::new(tree, self.capacity, identity)
      .map_err(|refusal| refusal_of_image(ImageError::Image(refusal)))?;
    let image_bytes = builder.image_bytes();
    let chunk = u64::try_from(state.store.content.chunk_bytes().max(1)).unwrap_or(u64::MAX);
    let size = self.size(image_bytes.div_ceil(chunk).saturating_mul(chunk));
    let admitted = crate::verbs::admit(
      state,
      &self.principal,
      size,
      NamePolicy::Exact,
      self.record.policy.require_locked,
      None,
    )
    .map_err(|reply| crate::verbs::reply_refusal(*reply))?;
    // Held from here, so a refusal below discards it.
    let volume = &mut self.admitted.insert(admitted).volume;
    let store = &mut state.store;
    let root = volume
      .root_inode(store)
      .map_err(|error| refusal_of_vfs(&error))?;
    let file = volume
      .create_file_no(store, root, IMAGE_FILE, IMAGE_MODE)
      .map_err(|error| refusal_of_vfs(&error))?;
    let owner = volume
      .stat(store, root)
      .map_err(|error| refusal_of_vfs(&error))?;
    volume
      .chown(store, file, owner.uid, owner.gid)
      .map_err(|error| refusal_of_vfs(&error))?;
    volume
      .truncate(store, file, image_bytes)
      .map_err(|error| refusal_of_vfs(&error))?;
    self.builder = Some(builder);
    self.inodes = inodes;
    self.file = file;
    self.image = Some(id);
    Ok(())
  }

  /// One slice of the writing; what the image holds once written.
  fn write_step(
    &mut self,
    state: &mut ShardState,
  ) -> Result<Option<slates_image::Written>, Refusal> {
    let (source, snapshot, file) = (self.source, core_snapshot(self.snapshot), self.file);
    let admitted = self.admitted.as_mut().ok_or_else(unreachable)?;
    let builder = self.builder.as_mut().ok_or_else(unreachable)?;
    let inodes = &self.inodes;
    sliced(state, |s| {
      let handle = slot_of(s, source)?;
      let ShardState { volumes, store, .. } = s;
      let slot = volumes.get(handle).map_err(|_| Refusal::NotFound)?;
      let mut io = BuildIo {
        store,
        source: &slot.volume,
        snapshot,
        inodes,
        image: &mut admitted.volume,
        file,
      };
      match builder
        .step(&mut io, ONE_UNIT)
        .map_err(|failed| refusal_of_image(failed.into()))?
      {
        Progress::More => Ok(None),
        Progress::Done(written) => Ok(Some(written)),
      }
    })
  }

  /// Records the image volume — its record naming the snapshot as its base, which keeps the build's pin — moves it
  /// into the registry and publishes it; the reply.
  fn publish(
    &mut self,
    state: &mut ShardState,
    written: slates_image::Written,
  ) -> Result<ReplyBody, Refusal> {
    let id = self.image.ok_or_else(unreachable)?;
    let admitted = self.admitted.take().ok_or_else(unreachable)?;
    let limit = admitted.volume.capacity_bytes();
    let record = VolumeRecord {
      id,
      name: self.name.clone(),
      owner_shard: state.partition,
      policy: PolicyRecord {
        size: crate::verbs::db_size(self.size(limit)),
        names: slates_db::catalog::NamePolicy::Exact,
        require_locked: self.record.policy.require_locked,
        role: Role::Plain,
        live_shipped: false,
        live_holders: None,
        write: None,
        write_version: 0,
        signers: Vec::new(),
      },
      base: BaseRecord::Image {
        volume: self.source,
        snapshot: to_db_snapshot(self.snapshot),
      },
      head: DbSnapshotId::default(),
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: self.principal.clone(),
      access: Vec::new(),
      created_ns: state.clock.monotonic_ns(),
      catalog_version: 0,
      taken_over: None,
    };
    let Admitted {
      volume,
      host,
      credits,
    } = admitted;
    let reply = crate::verbs::publish_created_volume(
      state,
      id,
      volume,
      host,
      (
        credits.reservation,
        credits.version,
        credits.metadata,
        credits.lock,
      ),
      record,
    );
    if let ReplyBody::Refused { refusal } = reply {
      return Err(refusal);
    }
    // The pin is the record's now: recovery counts it, and the image volume's destroy releases it.
    self.pinned = None;
    publish_new_volume(state, id)?;
    Ok(ReplyBody::ImageBuilt {
      id: wire_id(id),
      image_bytes: written.image_bytes,
      blocks_used: written.blocks_used,
      inodes_used: written.inodes_used,
    })
  }

  /// Releases one piece of the trees the build made — its builder's, its inode table, its walk's — and says whether
  /// anything is left ([`shed_in_slices`]).
  fn shed(&mut self) -> bool {
    self.builder.as_mut().is_some_and(Builder::shed)
      || self.inodes.shed()
      || self.walk.as_mut().is_some_and(SnapshotWalk::shed)
  }

  /// Gives back what the build holds: the image volume, discarded with its credits, and the snapshot's pin.
  fn release(&mut self, state: &mut ShardState) {
    if let Some(admitted) = self.admitted.take() {
      crate::verbs::discard_admitted(state, admitted);
    }
    if let Some(handle) = self.pinned.take() {
      crate::verbs::unpin_origin(state, handle, self.snapshot);
    }
  }
}

/// Builds the image a slice at a time; then commits its volume, or the refusal, as one atom and delivers the reply;
/// then releases what the build made, in slices.
async fn run_build(mut job: BuildJob, alias: Option<NameAlias>) {
  let reserved = reserve(alias.as_ref()).await;
  let outcome = match &reserved {
    Ok(()) => build_steps(&mut job).await,
    Err(refusal) => Err(refusal.clone()),
  };
  let request = job.request;
  let completed = crate::state::with_state(|s| {
    complete(s, request, |s| {
      match outcome.and_then(|written| job.publish(s, written)) {
        Ok(reply) => reply,
        Err(refusal) => {
          job.release(s);
          refused(refusal)
        }
      }
    })
  });
  let delivered = match completed {
    Some((reply, route)) => {
      settle(
        alias,
        reserved,
        matches!(reply, ReplyBody::ImageBuilt { .. }),
      )
      .await;
      crate::state::with_state(|s| deliver(s, reply, route))
    }
    None => None,
  };
  if delivered.is_none() {
    // The shard's state was unreachable (its shutdown): no record committed, so the client's retry, on the shard that
    // succeeds it, runs the verb again; the job's drop frees what it made.
    return;
  }
  shed_in_slices(|| job.shed()).await;
}

/// A volume an image verb makes under a name another partition holds (A-236, `slates_db::alias::CLONE_STEPS`): the
/// volume lives here beside its source, for copy-on-write, and the name's partition holds an alias of it, reserved
/// before the volume is made and confirmed (or released) before the reply, as a clone's is.
struct NameAlias {
  /// This shard, which asks.
  this: u16,
  /// The name's shard.
  name_shard: u16,
  /// The name.
  name: String,
  /// The volume's id, minted before the reservation names it.
  id: DbVolumeId,
}

/// The alias an image verb's `name` needs and the id it names, or neither when the name is this partition's (`None`,
/// `None`). Refused when the name's partition has no shard here or the volume ids are spent.
fn name_alias(
  state: &mut ShardState,
  name: &str,
) -> Result<(Option<NameAlias>, Option<DbVolumeId>), Refusal> {
  let name_partition = crate::verbs::owner_of_name(name, state.shards.len());
  if name_partition == state.partition {
    return Ok((None, None));
  }
  let name_shard =
    crate::verbs::shard_of_partition(state, name_partition).ok_or(Refusal::NotFound)?;
  let id = fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?;
  Ok((
    Some(NameAlias {
      this: state.shard,
      name_shard,
      name: name.to_owned(),
      id,
    }),
    Some(id),
  ))
}

/// Reserves `alias` at its name's partition: the name held there from now, resolving nothing until confirmed. `Ok` for
/// a verb whose name is its own partition's.
async fn reserve(alias: Option<&NameAlias>) -> Result<(), Refusal> {
  let Some(alias) = alias else {
    return Ok(());
  };
  let reservation = slates_db::Op::NameAliasReserved {
    record: slates_db::catalog::NameAliasRecord {
      name: alias.name.clone(),
      volume: alias.id,
      confirmed: false,
    },
  };
  crate::xshard::call_in_shard_time(
    alias.this,
    alias.name_shard,
    move |s| crate::verbs::alias_step(s, &reservation),
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await
  .unwrap_or_else(|| {
    Err(Refusal::Unpublished {
      reason: "the name's shard did not answer its reservation".to_owned(),
    })
  })
}

/// Confirms a reserved `alias` when its volume was made (`made`), else releases it; nothing for a verb whose name is its
/// own partition's or whose reservation was refused. A step the name's shard does not take is counted, and the next
/// recovery's to settle.
async fn settle(alias: Option<NameAlias>, reserved: Result<(), Refusal>, made: bool) {
  let Some(alias) = alias else {
    return;
  };
  if reserved.is_err() {
    return;
  }
  let step = if made {
    slates_db::Op::NameAliasConfirmed { name: alias.name }
  } else {
    slates_db::Op::NameAliasReleased { name: alias.name }
  };
  let settled = crate::xshard::call_in_shard_time(
    alias.this,
    alias.name_shard,
    move |s| crate::verbs::alias_step(s, &step),
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await;
  if !matches!(settled, Some(Ok(()))) {
    let _ = crate::state::with_state(|s| s.count(crate::verbs::ALIAS_STEP_UNTAKEN, 1));
  }
}

/// Releases what a finished build or merge made a piece a unit, in slices after its reply: its trees go back to the
/// allocator a name or a node at a time, as they were made, never in one drop (A-160; §4.3). Before (2026-10-24,
/// `image_bench`): a finished merge and its builder were freed in one drop of 4.5–15 ms at 100,000 nodes. With the
/// shard's state unreachable (its shutdown), the job's drop frees the rest.
async fn shed_in_slices(mut shed: impl FnMut() -> bool) {
  loop {
    let sliced = crate::state::with_state(|s| {
      sliced(s, |_| {
        let more = (0..SHED_PIECES).all(|_| shed());
        Ok((!more).then_some(()))
      })
    });
    if !matches!(sliced, Some(Ok(None))) {
      return;
    }
    slates_rt::futures::yield_now().await;
  }
}

/// Shape: the pieces one unit of a finished job's release frees ([`shed_in_slices`]). A piece is one free (a name, a
/// node, a segment of a plain table), about 60 ns with the unit's own overhead (`image_bench`'s release phase, 700,000
/// pieces of a 100,000-node merge in 73 ms, Apple M5 Max, 2026-10-24), so 64 make a unit of about 4 µs, a sixth of the
/// slice, and the clock reads around each unit fall to a sixty-fourth of the release's time.
const SHED_PIECES: usize = 64;

/// The build's slices: pin and walk the snapshot, plan the image and admit its volume, write it.
async fn build_steps(job: &mut BuildJob) -> Result<slates_image::Written, Refusal> {
  with(|s| job.start(s))?;
  let tree = loop {
    if let Some(tree) = with(|s| job.walk_step(s))? {
      break tree;
    }
    slates_rt::futures::yield_now().await;
  };
  with(|s| job.prepare(s, tree))?;
  loop {
    if let Some(written) = with(|s| job.write_step(s))? {
      return Ok(written);
    }
    slates_rt::futures::yield_now().await;
  }
}

/// `ImageMerge` (§4.6, A-160): checks the image volume, its base, the rights and the name at once, then merges the
/// image into a new clone of the base in a task of this shard and defers the reply to it.
pub(crate) fn merge_verb(
  state: &mut ShardState,
  principal: &Principal,
  image: VolumeId,
  name: String,
) -> ReplyBody {
  if !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
  }
  let (_, record) = match find_store_backed(state, image, StoreVerb::Image) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  if !rights_of(&record, principal).read {
    return forbidden("image_merge");
  }
  let (base, snapshot) = match image_base(&record) {
    Ok(base) => base,
    Err(refusal) => return refused(refusal),
  };
  let Some(base_record) = state.db.partition().volume(base).cloned() else {
    return refused(Refusal::NotFound);
  };
  // The merge clones the base's snapshot: the right a clone needs.
  if !rights_of(&base_record, principal).write {
    return forbidden("image_merge");
  }
  // The name as its own partition holds it too: a volume another partition made under an alias there (A-236).
  if let Some(existing) = crate::verbs::name_held(state, &name) {
    return refused(Refusal::AlreadyExists { existing });
  }
  if !state.by_id.contains_key(&base) {
    return refused(Refusal::NotFound);
  }
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "an image verb outside a recorded request".to_owned(),
    });
  };
  let (alias, id) = match name_alias(state, &name) {
    Ok(minted) => minted,
    Err(refusal) => return refused(refusal),
  };
  let job = MergeJob {
    request,
    principal: principal.clone(),
    id,
    image: record.id,
    base,
    base_record,
    snapshot: SnapshotId {
      value: snapshot.value,
    },
    name,
    image_snapshot: None,
    file: InodeNo(0),
    len: 0,
    clone: None,
    origin: None,
    base_walk: None,
    base_tree: None,
    reader: None,
    image_walk: None,
    merge: None,
  };
  defer(state, run_merge(job, alias))
}

/// A merge between its slices: what it holds, released by [`MergeJob::release`] on a refusal or by its drop.
struct MergeJob {
  request: (u64, RequestId),
  principal: Principal,
  /// The merged volume's id, when its name's alias names it already (A-236); minted at its publication otherwise.
  id: Option<DbVolumeId>,
  /// The image volume.
  image: DbVolumeId,
  /// The volume the image was built from, and its record.
  base: DbVolumeId,
  base_record: VolumeRecord,
  /// The snapshot the image was built from.
  snapshot: SnapshotId,
  name: String,
  /// The image volume's snapshot the merge reads, taken when it starts.
  image_snapshot: Option<slates_vfs::ids::SnapshotId>,
  /// The image file, and its length at that snapshot.
  file: InodeNo,
  len: u64,
  /// The clone, admitted and not yet recorded, and the base's slot whose snapshot it pins.
  clone: Option<Admitted>,
  origin: Option<Handle<VolumeSlot>>,
  base_walk: Option<SnapshotWalk>,
  base_tree: Option<SnapshotTree>,
  reader: Option<slates_image::Image>,
  image_walk: Option<Walk>,
  merge: Option<Merge>,
}

impl Drop for MergeJob {
  fn drop(&mut self) {
    if self.clone.is_some() || self.image_snapshot.is_some() {
      // A task that ended early: what it holds goes back. With the state unreachable (a shutdown) nothing can be,
      // and the process's end returns it.
      if crate::state::with_state(|s| self.release(s)).is_none() {
        self.clone = None;
        self.image_snapshot = None;
      }
    }
  }
}

impl MergeJob {
  /// The entries a walk may read: the clone's entry allowance, since the clone cannot hold more.
  fn most(&self) -> usize {
    usize::try_from(crate::verbs::entry_allowance(size_of_record(
      &self.base_record,
    )))
    .unwrap_or(usize::MAX)
  }

  /// Snapshots the image volume, finds its image file, admits the clone of the base's snapshot (pinning it) and
  /// starts the walk of that snapshot.
  fn start(&mut self, state: &mut ShardState) -> Result<(), Refusal> {
    let image = slot_of(state, self.image)?;
    {
      let ShardState { volumes, store, .. } = &mut *state;
      let slot = volumes.get_mut(image).map_err(|_| Refusal::NotFound)?;
      let snapshot = slot
        .volume
        .snapshot(store)
        .map_err(|error| refusal_of_vfs(&error))?;
      self.image_snapshot = Some(snapshot);
      let located = slot
        .volume
        .resolve_in(store, snapshot, IMAGE_FILE)
        .map_err(|_| Refusal::ImageRefused {
          reason: format!("the image volume holds no {IMAGE_FILE}"),
        })?;
      self.file = located.inode;
      self.len = slot
        .volume
        .stat_in(store, snapshot, located.inode)
        .map_err(|error| refusal_of_vfs(&error))?
        .size;
    }
    let origin = slot_of(state, self.base)?;
    let admitted = crate::verbs::admit_clone(state, origin, &self.base_record, self.snapshot)
      .map_err(|reply| crate::verbs::reply_refusal(*reply))?;
    self.clone = Some(admitted);
    self.origin = Some(origin);
    let most = self.most();
    let slot = state.volumes.get(origin).map_err(|_| Refusal::NotFound)?;
    self.base_walk = Some(
      SnapshotWalk::new(
        &slot.volume,
        &state.store,
        core_snapshot(self.snapshot),
        most,
      )
      .map_err(refusal_of_image)?,
    );
    Ok(())
  }

  /// One slice of the base's walk; whether it is whole.
  fn base_step(&mut self, state: &mut ShardState) -> Result<bool, Refusal> {
    let base = self.base;
    let walk = self.base_walk.as_mut().ok_or_else(unreachable)?;
    let walked = sliced(state, |s| {
      let origin = slot_of(s, base)?;
      let slot = s.volumes.get(origin).map_err(|_| Refusal::NotFound)?;
      walk
        .step(&slot.volume, &s.store, ONE_UNIT)
        .map_err(refusal_of_image)
    })?;
    match walked {
      Some(tree) => {
        // The walk stays, for its leftovers to be released in slices at the end.
        self.base_tree = Some(tree);
        Ok(true)
      }
      None => Ok(false),
    }
  }

  /// Opens the image at the merge's snapshot of its volume and starts the walk of its tree.
  fn open(&mut self, state: &mut ShardState) -> Result<(), Refusal> {
    let image = slot_of(state, self.image)?;
    let slot = state.volumes.get(image).map_err(|_| Refusal::NotFound)?;
    let snapshot = self.image_snapshot.ok_or_else(unreachable)?;
    let mut source = SnapshotFile {
      volume: &slot.volume,
      store: &state.store,
      snapshot,
      file: self.file,
      len: self.len,
    };
    let mut reader =
      slates_image::Image::open(&mut source).map_err(|failed| refusal_of_image(failed.into()))?;
    let walk = Walk::new(&mut reader, &mut source, self.most())
      .map_err(|failed| refusal_of_image(failed.into()))?;
    self.reader = Some(reader);
    self.image_walk = Some(walk);
    Ok(())
  }

  /// One slice of the image's walk; once whole, the merge planned over both trees.
  fn image_step(&mut self, state: &mut ShardState) -> Result<bool, Refusal> {
    let (image, file, len) = (self.image, self.file, self.len);
    let snapshot = self.image_snapshot.ok_or_else(unreachable)?;
    let reader = self.reader.as_mut().ok_or_else(unreachable)?;
    let walk = self.image_walk.as_mut().ok_or_else(unreachable)?;
    let walked = sliced(state, |s| {
      let slot = s
        .volumes
        .get(slot_of(s, image)?)
        .map_err(|_| Refusal::NotFound)?;
      let mut source = SnapshotFile {
        volume: &slot.volume,
        store: &s.store,
        snapshot,
        file,
        len,
      };
      match walk
        .step(reader, &mut source, ONE_UNIT)
        .map_err(|failed| refusal_of_image(failed.into()))?
      {
        Walked::More => Ok(None),
        Walked::Done(tree) => Ok(Some(*tree)),
      }
    })?;
    let Some(tree) = walked else {
      return Ok(false);
    };
    let reader = self.reader.as_ref().ok_or_else(unreachable)?;
    // The image is the build's when its file system's identity is the image volume's own id, which the build gave
    // it; any other image is merged by names and bytes alone.
    let trusted = reader.uuid() == self.image.bytes;
    let base = self.base_tree.take().ok_or_else(unreachable)?;
    let clone = self.clone.as_ref().ok_or_else(unreachable)?;
    self.merge = Some(Merge::new(base, tree, &clone.volume, &state.store, trusted));
    // The walk stays, for its leftovers (the inodes it met) to be released in slices at the end.
    Ok(true)
  }

  /// One slice of the merge; its report once the clone holds the image's tree.
  fn merge_step(&mut self, state: &mut ShardState) -> Result<Option<MergeReport>, Refusal> {
    let (image, file, len) = (self.image, self.file, self.len);
    let snapshot = self.image_snapshot.ok_or_else(unreachable)?;
    let reader = self.reader.as_mut().ok_or_else(unreachable)?;
    let clone = self.clone.as_mut().ok_or_else(unreachable)?;
    let merge = self.merge.as_mut().ok_or_else(unreachable)?;
    sliced(state, |s| {
      let handle = slot_of(s, image)?;
      let ShardState { volumes, store, .. } = s;
      let slot = volumes.get(handle).map_err(|_| Refusal::NotFound)?;
      let mut content = ImageContent {
        image: reader,
        volume: &slot.volume,
        snapshot,
        file,
        len,
      };
      merge
        .step(&mut clone.volume, store, &mut content, ONE_UNIT)
        .map_err(refusal_of_image)
    })
  }

  /// Records the clone — its record and its lineage to the base's snapshot — moves it into the registry and publishes
  /// it; the reply.
  fn publish(&mut self, state: &mut ShardState, report: MergeReport) -> Result<ReplyBody, Refusal> {
    let clone = self.clone.take().ok_or_else(unreachable)?;
    let origin = self.origin.take().ok_or_else(unreachable)?;
    let id = match self.id {
      Some(id) => crate::verbs::publish_clone_as(
        state,
        &self.principal,
        origin,
        &self.base_record,
        (self.snapshot, &self.name),
        clone,
        id,
      )?,
      None => crate::verbs::publish_clone(
        state,
        &self.principal,
        origin,
        &self.base_record,
        (self.snapshot, &self.name),
        clone,
      )?,
    };
    publish_new_volume(state, id)?;
    Ok(ReplyBody::ImageMerged {
      id: wire_id(id),
      report: ImageMergeReport {
        kept: report.kept,
        created: report.created,
        moved: report.moved,
        linked: report.linked,
        removed: report.removed,
        names_removed: report.names_removed,
        staged: report.staged,
        bytes_compared: report.bytes_compared,
        bytes_written: report.bytes_written,
        attributes_set: report.attributes_set,
      },
    })
  }

  /// Releases one piece of what the merge made — the merge's tables and trees, the base's tree, the walks' leftovers,
  /// the reader's tables — and says whether anything is left ([`shed_in_slices`]).
  fn shed(&mut self) -> bool {
    self.merge.as_mut().is_some_and(Merge::shed)
      || self.base_tree.as_mut().is_some_and(SnapshotTree::shed)
      || self.base_walk.as_mut().is_some_and(SnapshotWalk::shed)
      || self.image_walk.as_mut().is_some_and(Walk::shed)
      || self.reader.as_mut().is_some_and(slates_image::Image::shed)
  }

  /// Gives back what the merge holds: the clone, discarded with its credits and its pin on the base's snapshot, and
  /// the image volume's snapshot.
  fn release(&mut self, state: &mut ShardState) {
    if let (Some(clone), Some(origin)) = (self.clone.take(), self.origin.take()) {
      crate::verbs::discard_clone(state, origin, self.snapshot, clone);
    }
    self.drop_image_snapshot(state);
  }

  /// Destroys the image volume's snapshot the merge read, if it still stands.
  fn drop_image_snapshot(&mut self, state: &mut ShardState) {
    let Some(snapshot) = self.image_snapshot.take() else {
      return;
    };
    let Ok(image) = slot_of(state, self.image) else {
      return;
    };
    let destroyed = {
      let ShardState { volumes, store, .. } = &mut *state;
      volumes
        .get_mut(image)
        .map(|slot| slot.volume.destroy_snapshot(store, snapshot))
    };
    if let Ok(destroyed) = destroyed {
      // The snapshot is recorded nowhere; one left by a refused destroy is trimmed at the next recovery.
      crate::verbs::count_kept(state, IMAGE_SNAPSHOT_KEPT, destroyed);
    }
  }
}

/// Counter: an image merge's snapshot of its image volume that could not be destroyed when the merge ended.
const IMAGE_SNAPSHOT_KEPT: &str = "image.snapshot_kept";

/// Merges the image a slice at a time; then commits the clone, or the refusal, as one atom and delivers the reply; then
/// releases what the merge made, in slices.
async fn run_merge(mut job: MergeJob, alias: Option<NameAlias>) {
  let reserved = reserve(alias.as_ref()).await;
  let outcome = match &reserved {
    Ok(()) => merge_steps(&mut job).await,
    Err(refusal) => Err(refusal.clone()),
  };
  let request = job.request;
  let completed = crate::state::with_state(|s| {
    let completed = complete(s, request, |s| {
      match outcome.and_then(|report| job.publish(s, report)) {
        Ok(reply) => reply,
        Err(refusal) => {
          job.release(s);
          refused(refusal)
        }
      }
    });
    job.drop_image_snapshot(s);
    completed
  });
  let delivered = match completed {
    Some((reply, route)) => {
      settle(
        alias,
        reserved,
        matches!(reply, ReplyBody::ImageMerged { .. }),
      )
      .await;
      crate::state::with_state(|s| deliver(s, reply, route))
    }
    None => None,
  };
  if delivered.is_none() {
    // The shard's state was unreachable (its shutdown): no record committed, so the client's retry, on the shard that
    // succeeds it, runs the verb again; the job's drop frees what it made.
    return;
  }
  shed_in_slices(|| job.shed()).await;
}

/// The merge's slices: snapshot the image and admit the clone, walk the base, open and walk the image, merge.
async fn merge_steps(job: &mut MergeJob) -> Result<MergeReport, Refusal> {
  with(|s| job.start(s))?;
  while !with(|s| job.base_step(s))? {
    slates_rt::futures::yield_now().await;
  }
  with(|s| job.open(s))?;
  while !with(|s| job.image_step(s))? {
    slates_rt::futures::yield_now().await;
  }
  loop {
    if let Some(report) = with(|s| job.merge_step(s))? {
      return Ok(report);
    }
    slates_rt::futures::yield_now().await;
  }
}

/// Format: the archive file's permissions: its owner's read and write alone, since it holds the whole snapshot.
const ARCHIVE_MODE: u32 = 0o600;
/// Counter: one emptying of a streamed export's stage into its archive file (A-459): with the exports, how many pieces
/// they were written in; an export larger than its stage's threshold is written in more than one.
/// Format: a counter name in the daemon's status report.
const ARCHIVE_STAGE_FLUSHES: &str = "image.archive_stage_flushes";
/// Counter: a streamed export whose stage held more than its bound at some point (A-459): an item larger than the
/// bound's derivation allows. Zero is the healthy reading.
/// Format: a counter name in the daemon's status report.
const ARCHIVE_STAGE_PAST_BOUND: &str = "image.archive_stage_past_bound";

/// `ArchiveExport` (§4.11 "Archive", D-17; Mantle's CP-T10.4, A-441): checks the volume, the right, the name and the
/// snapshot at once, then walks the snapshot into its archive and writes it into a new volume in a task of this shard,
/// and defers the reply to it, as an image build does. An archive copies only what a reader can read, and the snapshot
/// is pinned only while the export runs, so `read` is the right it needs; the archive volume is the caller's own.
pub(crate) fn archive_verb(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: SnapshotId,
  name: String,
) -> ReplyBody {
  if !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
  }
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Archive) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  if !rights_of(&record, principal).read {
    return forbidden("archive_export");
  }
  // The name as its own partition holds it too: a volume another partition made under an alias there (A-236).
  if let Some(existing) = crate::verbs::name_held(state, &name) {
    return refused(Refusal::AlreadyExists { existing });
  }
  let known = state
    .db
    .partition()
    .snapshot(record.id, to_db_snapshot(snapshot))
    .is_some();
  let present = state
    .volumes
    .get(handle)
    .is_ok_and(|slot| slot.volume.snapshot_info(core_snapshot(snapshot)).is_ok());
  if !known || !present {
    return refused(Refusal::NotFound);
  }
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "an archive export outside a recorded request".to_owned(),
    });
  };
  let (alias, archive) = match name_alias(state, &name) {
    Ok(minted) => minted,
    Err(refusal) => return refused(refusal),
  };
  let job = ArchiveJob {
    request,
    principal: principal.clone(),
    source: record.id,
    record,
    snapshot,
    name,
    pinned: None,
    codec: None,
    stamp: (0, 0),
    archiver: None,
    measure: None,
    writer: None,
    walking: false,
    threshold: 1,
    bound: 1,
    length: 0,
    manifest: [0; 32],
    admitted: None,
    file: InodeNo(0),
    archive,
  };
  defer(state, run_archive(job, alias))
}

/// An archive export between its slices: what it holds, released by [`ArchiveJob::release`] on a refusal or by its
/// drop. The archive is streamed (A-459): a first walk of the snapshot measures each chunk's record and drops its
/// bytes, the archive is laid out from it and its volume admitted exactly, and a second walk writes it into the file
/// through a stage emptied at a copy slice, so neither the snapshot's content nor the archive is ever held whole.
struct ArchiveJob {
  request: (u64, RequestId),
  principal: Principal,
  /// The volume exported.
  source: DbVolumeId,
  /// Its record, whose size policy's kind and locking the archive volume takes.
  record: VolumeRecord,
  snapshot: SnapshotId,
  name: String,
  /// The volume slot whose snapshot this export pinned.
  pinned: Option<Handle<VolumeSlot>>,
  /// The codec policy both walks store chunks under, cloned once, so the second walk compresses as the first did.
  codec: Option<slates_archive::CodecPolicy>,
  /// The archive header's volume identity and creation time, the same for both walks.
  stamp: (u64, u64),
  /// The walk under way: the first, measuring, then the second, writing (`slates_vfs::export`).
  archiver: Option<slates_vfs::export::SnapshotArchiver>,
  /// The first walk's measurements, until the archive is laid out.
  measure: Option<slates_archive::Measure>,
  /// The second walk's writer, from the layout until the file holds the archive.
  writer: Option<slates_archive::StreamWriter>,
  /// Whether the second walk is still cutting chunks.
  walking: bool,
  /// The stage's threshold: it is emptied into the file once it holds this many bytes.
  threshold: usize,
  /// The stage's bound for the archive's chunks: the threshold and the largest item staged whole after it.
  bound: usize,
  /// The archive's length and manifest identity, from its layout.
  length: u64,
  manifest: [u8; 32],
  /// The archive volume, admitted and not yet recorded.
  admitted: Option<Admitted>,
  /// The archive file in it.
  file: InodeNo,
  /// The archive volume's id, minted when its archive is laid out (or by its name's alias).
  archive: Option<DbVolumeId>,
}

impl Drop for ArchiveJob {
  fn drop(&mut self) {
    if self.admitted.is_some() || self.pinned.is_some() {
      // A task that ended early: what it holds goes back. With the state unreachable (a shutdown) nothing can be,
      // and the process's end returns it.
      if crate::state::with_state(|s| self.release(s)).is_none() {
        self.admitted = None;
        self.pinned = None;
      }
    }
  }
}

impl ArchiveJob {
  /// Pins the snapshot and starts its first walk. The archive header's volume identity is the exported volume's own
  /// suffix (as the fleet's archives carry), and its creation time the host's wall clock: both informational, never
  /// part of the manifest's identity, and taken once for both walks.
  fn start(&mut self, state: &mut ShardState) -> Result<(), Refusal> {
    let handle = slot_of(state, self.source)?;
    let created_unix =
      u64::try_from(HostClock::new().wall_ns().div_euclid(NANOS_PER_SECOND)).unwrap_or(0);
    let local = self
      .source
      .bytes
      .get(size_of::<u64>()..)
      .and_then(|low| <[u8; size_of::<u64>()]>::try_from(low).ok())
      .map_or(0, u64::from_be_bytes);
    self.stamp = (local, created_unix);
    self.codec = Some(state.config.codec.clone());
    let slot = state
      .volumes
      .get_mut(handle)
      .map_err(|_| Refusal::NotFound)?;
    slot
      .volume
      .pin(core_snapshot(self.snapshot))
      .map_err(|error| refusal_of_vfs(&error))?;
    self.pinned = Some(handle);
    self.archiver = Some(self.walk(state)?);
    self.measure = Some(slates_archive::Measure::default());
    Ok(())
  }

  /// A walk of the pinned snapshot from its root, under the export's codec and stamp.
  fn walk(&self, state: &ShardState) -> Result<slates_vfs::export::SnapshotArchiver, Refusal> {
    let handle = slot_of(state, self.source)?;
    let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
    let codec = self.codec.clone().ok_or_else(unreachable)?;
    slates_vfs::export::SnapshotArchiver::new(
      &slot.volume,
      &state.store,
      core_snapshot(self.snapshot),
      self.stamp.0,
      self.stamp.1,
      codec,
    )
    .map_err(|error| refusal_of_archive(&error))
  }

  /// The archive volume's size class: the source's kind at `limit` bytes.
  fn size(&self, limit: u64) -> SizeClass {
    match self.record.policy.size {
      DbSizeClass::Bounded { .. } => SizeClass::Bounded { limit },
      DbSizeClass::Dynamic { .. } => SizeClass::Dynamic { max: limit },
    }
  }

  /// One slice of the first walk, each chunk measured and its bytes dropped; the walk's archive once whole, its chunks
  /// none.
  fn measure_step(
    &mut self,
    state: &mut ShardState,
  ) -> Result<Option<slates_archive::Archive>, Refusal> {
    let source = self.source;
    let (Some(archiver), Some(measure)) = (self.archiver.as_mut(), self.measure.as_mut()) else {
      return Err(unreachable());
    };
    sliced(state, |s| {
      let handle = slot_of(s, source)?;
      let slot = s.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
      match archiver
        .advance_into(&slot.volume, &s.store, ONE_UNIT, measure)
        .map_err(|error| refusal_of_archive(&error))?
      {
        slates_vfs::export::Progress::More => Ok(None),
        slates_vfs::export::Progress::Done(archive) => Ok(Some(archive)),
      }
    })
  }

  /// Lays the archive out from the first walk (`shell` is its archive, its chunks none), admits its volume sized to
  /// it — its length rounded up to whole chunks, since a write is charged its chunk window (§4.5), and at least one
  /// chunk — makes the archive file empty and owned as the root is, and starts the second walk behind the staged
  /// header. The stage is emptied at a copy slice (`DaemonConfig::copy_slice_bytes`: half the live step quantum at the
  /// measured memcpy rate, since emptying it is a copy into the file), so it holds at most that and one record of the
  /// largest chunk ([`slates_archive::StreamWriter::bound`]).
  fn prepare(
    &mut self,
    state: &mut ShardState,
    shell: &slates_archive::Archive,
  ) -> Result<(), Refusal> {
    // The id its name's alias already names (A-236), or one minted here.
    let id = match self.archive {
      Some(id) => id,
      None => fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?,
    };
    let measure = self.measure.take().ok_or_else(unreachable)?;
    let layout = slates_archive::Layout::new(shell, measure)
      .map_err(|_| refusal_of_vfs(&slates_vfs::VfsError::RecoveryIncomplete))?;
    self.manifest = layout.manifest_identity();
    self.length = layout.len();
    let chunk_bytes = state.store.content.chunk_bytes().max(1);
    let chunk = u64::try_from(chunk_bytes).unwrap_or(u64::MAX);
    let size = self.size(self.length.div_ceil(chunk).max(1).saturating_mul(chunk));
    let admitted = crate::verbs::admit(
      state,
      &self.principal,
      size,
      NamePolicy::Exact,
      self.record.policy.require_locked,
      None,
    )
    .map_err(|reply| crate::verbs::reply_refusal(*reply))?;
    // Held from here, so a refusal below discards it.
    let volume = &mut self.admitted.insert(admitted).volume;
    let store = &mut state.store;
    let root = volume
      .root_inode(store)
      .map_err(|error| refusal_of_vfs(&error))?;
    let file = volume
      .create_file_no(store, root, ARCHIVE_FILE, ARCHIVE_MODE)
      .map_err(|error| refusal_of_vfs(&error))?;
    let owner = volume
      .stat(store, root)
      .map_err(|error| refusal_of_vfs(&error))?;
    volume
      .chown(store, file, owner.uid, owner.gid)
      .map_err(|error| refusal_of_vfs(&error))?;
    self.file = file;
    self.archive = Some(id);
    self.threshold = usize::try_from(state.config.copy_slice_bytes())
      .unwrap_or(usize::MAX)
      .max(1);
    self.bound = slates_archive::StreamWriter::bound(
      self.threshold,
      u32::try_from(chunk_bytes).unwrap_or(u32::MAX),
    );
    self.writer = Some(slates_archive::StreamWriter::new(layout));
    self.archiver = Some(self.walk(state)?);
    self.walking = true;
    Ok(())
  }

  /// One slice of the writing, a unit at a time, each what the writer asks next
  /// ([`slates_archive::StreamWriter::next`]): the stage written into the file once it holds the threshold (or the
  /// archive's last bytes), else the second walk's next unit, else the tail's next piece; `Some` once the file holds
  /// the whole archive.
  fn write_step(&mut self, state: &mut ShardState) -> Result<Option<()>, Refusal> {
    let (source, file, threshold) = (self.source, self.file, self.threshold);
    let ArchiveJob {
      admitted,
      writer,
      archiver,
      walking,
      ..
    } = self;
    let (Some(admitted), Some(writer)) = (admitted.as_mut(), writer.as_mut()) else {
      return Err(unreachable());
    };
    sliced(state, |s| {
      let next = writer
        .next(threshold, *walking)
        .map_err(|_| refusal_of_vfs(&slates_vfs::VfsError::RecoveryIncomplete))?;
      match next {
        slates_archive::Next::Write => {
          let (at, staged) = writer.pending();
          let wrote = admitted
            .volume
            .write(&mut s.store, file, at, staged)
            .map_err(|error| refusal_of_vfs(&error))?;
          if wrote == 0 {
            // A write that lands nothing would loop for good: the volume's own refusal, which a short write stands
            // for.
            return Err(refusal_of_vfs(&slates_vfs::VfsError::NoSpace));
          }
          writer.consume(wrote);
          s.count(ARCHIVE_STAGE_FLUSHES, 1);
          Ok(None)
        }
        slates_archive::Next::Walk => {
          let handle = slot_of(s, source)?;
          let slot = s.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
          let archiver = archiver.as_mut().ok_or_else(unreachable)?;
          if let slates_vfs::export::Progress::Done(_) = archiver
            .advance_into(&slot.volume, &s.store, ONE_UNIT, writer)
            .map_err(|error| refusal_of_archive(&error))?
          {
            *walking = false;
          }
          Ok(None)
        }
        slates_archive::Next::Done => Ok(Some(())),
      }
    })
  }

  /// Records the archive volume — a plain volume of its own, naming nothing of its source — moves it into the registry
  /// and publishes it, then gives back the snapshot's pin; the reply. A stage that ever held past its bound is counted,
  /// the signal of an item larger than the bound's derivation allows.
  fn publish(&mut self, state: &mut ShardState) -> Result<ReplyBody, Refusal> {
    let id = self.archive.ok_or_else(unreachable)?;
    let admitted = self.admitted.take().ok_or_else(unreachable)?;
    if self
      .writer
      .as_ref()
      .is_some_and(|writer| writer.high_water() > self.bound)
    {
      state.count(ARCHIVE_STAGE_PAST_BOUND, 1);
    }
    let limit = admitted.volume.capacity_bytes();
    let record = VolumeRecord {
      id,
      name: self.name.clone(),
      owner_shard: state.partition,
      policy: PolicyRecord {
        size: crate::verbs::db_size(self.size(limit)),
        names: slates_db::catalog::NamePolicy::Exact,
        require_locked: self.record.policy.require_locked,
        role: Role::Plain,
        live_shipped: false,
        live_holders: None,
        write: None,
        write_version: 0,
        signers: Vec::new(),
      },
      base: BaseRecord::Scratch,
      head: DbSnapshotId::default(),
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: self.principal.clone(),
      access: Vec::new(),
      created_ns: state.clock.monotonic_ns(),
      catalog_version: 0,
      taken_over: None,
    };
    let Admitted {
      volume,
      host,
      credits,
    } = admitted;
    let reply = crate::verbs::publish_created_volume(
      state,
      id,
      volume,
      host,
      (
        credits.reservation,
        credits.version,
        credits.metadata,
        credits.lock,
      ),
      record,
    );
    if let ReplyBody::Refused { refusal } = reply {
      return Err(refusal);
    }
    publish_new_volume(state, id)?;
    // The archive is the volume's own bytes now: the source's snapshot is free to go.
    if let Some(handle) = self.pinned.take() {
      crate::verbs::unpin_origin(state, handle, self.snapshot);
    }
    Ok(ReplyBody::ArchiveExported {
      id: wire_id(id),
      archive_bytes: self.length,
      manifest: self.manifest,
    })
  }

  /// Releases what the export made: the writer's layout and what is left of a walk, at once, since each is a few
  /// allocations or a walk already spent ([`shed_in_slices`] takes one unit). Nothing is left after.
  fn shed(&mut self) -> bool {
    self.writer = None;
    self.measure = None;
    self.archiver = None;
    false
  }

  /// Gives back what the export holds: the archive volume, discarded with its credits, and the snapshot's pin.
  fn release(&mut self, state: &mut ShardState) {
    if let Some(admitted) = self.admitted.take() {
      crate::verbs::discard_admitted(state, admitted);
    }
    if let Some(handle) = self.pinned.take() {
      crate::verbs::unpin_origin(state, handle, self.snapshot);
    }
  }
}

/// The refusal a snapshot's walk into its archive is answered with: a base-backed entry, whose bytes are on the disk
/// and which an archive cannot carry (`slates_vfs::export`), refused by its kind with the way out; any other, the
/// volume core's own.
fn refusal_of_archive(error: &slates_vfs::VfsError) -> Refusal {
  match error {
    slates_vfs::VfsError::RecoveryIncomplete => Refusal::Unsupported {
      feature: "an archive of a snapshot whose entries still read the disk: capture the overlay's base first \
                (capture_base) and export the captured snapshot"
        .to_owned(),
    },
    other => refusal_of_vfs(other),
  }
}

/// Exports the archive a slice at a time; then commits its volume, or the refusal, as one atom and delivers the reply;
/// then releases what the export made.
async fn run_archive(mut job: ArchiveJob, alias: Option<NameAlias>) {
  let reserved = reserve(alias.as_ref()).await;
  let outcome = match &reserved {
    Ok(()) => archive_steps(&mut job).await,
    Err(refusal) => Err(refusal.clone()),
  };
  let request = job.request;
  let completed = crate::state::with_state(|s| {
    complete(s, request, |s| {
      match outcome.and_then(|()| job.publish(s)) {
        Ok(reply) => reply,
        Err(refusal) => {
          job.release(s);
          refused(refusal)
        }
      }
    })
  });
  let delivered = match completed {
    Some((reply, route)) => {
      settle(
        alias,
        reserved,
        matches!(reply, ReplyBody::ArchiveExported { .. }),
      )
      .await;
      crate::state::with_state(|s| deliver(s, reply, route))
    }
    None => None,
  };
  if delivered.is_none() {
    // The shard's state was unreachable (its shutdown): no record committed, so the client's retry, on the shard that
    // succeeds it, runs the verb again; the job's drop frees what it made.
    return;
  }
  shed_in_slices(|| job.shed()).await;
}

/// The export's slices (A-459): pin the snapshot and measure it in a first walk; lay the archive out, admit its volume
/// exactly and start the second walk; then write the file through the stage, a unit at a time.
async fn archive_steps(job: &mut ArchiveJob) -> Result<(), Refusal> {
  with(|s| job.start(s))?;
  let shell = loop {
    if let Some(shell) = with(|s| job.measure_step(s))? {
      break shell;
    }
    slates_rt::futures::yield_now().await;
  };
  with(|s| job.prepare(s, &shell))?;
  drop(shell);
  loop {
    if with(|s| job.write_step(s))?.is_some() {
      return Ok(());
    }
    slates_rt::futures::yield_now().await;
  }
}

/// Counter: one slice of an archive import (A-453): its file read, decoded, planned, restored or laid into the new
/// volume a slice a turn, never in one long turn; how many turns the imports took.
/// Format: a counter name in the daemon's status report.
const IMPORT_SLICES: &str = "image.archive_import_slices";
/// Counter: a unit of an import's reading that held more of the archive file than its bound (A-494,
/// `slates_archive::Decoding::bound` of the unit's feed slice): its slice and the decoder's one field. Zero unless the
/// reading regressed to holding the file.
/// Format: a counter name in the daemon's status report.
const IMPORT_STAGE_PAST_BOUND: &str = "image.archive_import_stage_past_bound";

/// `ArchiveImport` (§4.11 "Archive", D-17; Mantle's CP-T10.4, A-453): checks the archive volume, the right and the name
/// at once, then reads the archive file, decodes and verifies it, restores its tree and lays it into a new volume in a
/// task of this shard, and defers the reply to it, as an export does. The archive volume is only read, so `read` is the
/// right it needs; the new volume is the caller's own.
pub(crate) fn import_verb(
  state: &mut ShardState,
  principal: &Principal,
  archive: VolumeId,
  name: String,
  (size, require_locked): (SizeClass, bool),
) -> ReplyBody {
  if !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
  }
  let (_, record) = match find_store_backed(state, archive, StoreVerb::ArchiveRead) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  if !rights_of(&record, principal).read {
    return forbidden("archive_import");
  }
  // The name as its own partition holds it too: a volume another partition made under an alias there (A-236).
  if let Some(existing) = crate::verbs::name_held(state, &name) {
    return refused(Refusal::AlreadyExists { existing });
  }
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "an archive import outside a recorded request".to_owned(),
    });
  };
  let (alias, id) = match name_alias(state, &name) {
    Ok(minted) => minted,
    Err(refusal) => return refused(refusal),
  };
  let job = ImportJob {
    request,
    principal: principal.clone(),
    archive: record.id,
    archive_cap: class_cap(crate::verbs::wire_size(record.policy.size)),
    name,
    size,
    require_locked,
    file: InodeNo(0),
    decoding: None,
    slice: Vec::new(),
    stage_high_water: 0,
    manifest: [0; 32],
    names: NamePolicy::Exact,
    admitted: None,
    id,
    decoded: None,
    restored: None,
    shedding: Vec::new(),
  };
  defer(state, run_import(job, alias))
}

/// The bytes a volume of `size` may hold: its bound, or its dynamic maximum.
fn class_cap(size: SizeClass) -> u64 {
  match size {
    SizeClass::Bounded { limit } => limit,
    SizeClass::Dynamic { max } => max,
  }
}

/// An archive import between its slices: what it holds, released by [`ImportJob::release`] on a refusal or by its drop,
/// and shed a piece a unit after its reply ([`ImportJob::shed`]).
struct ImportJob {
  request: (u64, RequestId),
  principal: Principal,
  /// The volume whose root holds the archive file.
  archive: DbVolumeId,
  /// The most that volume may hold: no archive written into it is longer.
  archive_cap: u64,
  /// The new volume's name.
  name: String,
  /// The new volume's size class.
  size: SizeClass,
  /// Whether the new volume's memory must be locked.
  require_locked: bool,
  /// The archive file.
  file: InodeNo,
  /// The decoder of the archive file's bytes, fed them a slice a unit and stepped through its checks
  /// ([`ImportJob::feed_step`], [`ImportJob::decode_step`]); dropped once it gives the archive.
  decoding: Option<slates_archive::Decoding>,
  /// The slice of the archive file a unit reads before it feeds it: no longer than a feed slice, and emptied by the
  /// feed, so the file is never held whole (A-494).
  slice: Vec<u8>,
  /// The most of the archive file the reading held at once: a unit's slice and the decoder's stage beside it.
  stage_high_water: usize,
  /// The archive's manifest identity, verified against its tree.
  manifest: [u8; 32],
  /// The new volume's name policy: its archive's.
  names: NamePolicy,
  /// The new volume, admitted and not yet recorded.
  admitted: Option<Admitted>,
  /// The new volume's id, minted at its admission (or by its name's alias).
  id: Option<DbVolumeId>,
  /// The decoded archive, held while its tree is planned and restored.
  decoded: Option<slates_archive::Archive>,
  /// The restored tree, held while it is laid into the volume.
  restored: Option<slates_archive::Restored>,
  /// The manifest's nodes still to release, an entry a piece ([`ImportJob::shed`]).
  shedding: Vec<slates_archive::Node>,
}

impl Drop for ImportJob {
  fn drop(&mut self) {
    if self.admitted.is_some() {
      // A task that ended early: the volume goes back. With the state unreachable (a shutdown) nothing can be, and
      // the process's end returns it.
      if crate::state::with_state(|s| self.release(s)).is_none() {
        self.admitted = None;
      }
    }
  }
}

impl ImportJob {
  /// Finds the archive file in the archive volume's root and admits its length before a byte of it is read, and starts
  /// its decoder. No longer than the archive volume's class cap: every byte an archive's writer put there was charged
  /// to that volume, so only a file with holes is longer, and no writer of an archive makes one (refused `Malformed`).
  /// And no longer than the room this shard could admit: the file is never held whole (A-494), but the archive it
  /// decodes to, every record's payload and the manifest, is held until the tree is restored (owed in pieces, GAPS
  /// A-453), and that is the file's bytes less its framing. Refused `BudgetExceeded` naming the room.
  fn open(&mut self, state: &mut ShardState) -> Result<(), Refusal> {
    let handle = slot_of(state, self.archive)?;
    let (file, length) = locate_archive(state, handle).map_err(|error| refusal_of_vfs(&error))?;
    if length > self.archive_cap {
      return Err(Refusal::ArchiveRefused {
        fault: ArchiveFault::Malformed,
      });
    }
    let room = state.store.admittable();
    if length > room {
      return Err(Refusal::BudgetExceeded { available: room });
    }
    self.file = file;
    self.decoding = Some(slates_archive::Decoding::new(length));
    Ok(())
  }

  /// One slice of the reading, a feed slice a unit ([`feed_slice_bytes`] of the live configuration, or the test's):
  /// the archive file read from where its decoder stands into the job's slice and fed to the decoder, which keeps what
  /// the archive keeps and stages at most one field, so the reading holds no more of the file than
  /// `Decoding::bound(slice_bytes)` (counted past it). `Some` once the decoder holds the file's admitted length, or
  /// all it held: a file cut short meanwhile reads short, and the decoder refuses it as cut off. A header this daemon
  /// does not read is refused at its first slice, typed.
  fn feed_step(
    &mut self,
    state: &mut ShardState,
    slice_bytes: usize,
  ) -> Result<Option<()>, Refusal> {
    let (archive, file) = (self.archive, self.file);
    let ImportJob {
      decoding,
      slice,
      stage_high_water,
      ..
    } = self;
    let decoding = decoding.as_mut().ok_or_else(unreachable)?;
    let bound = slates_archive::Decoding::bound(slice_bytes);
    state.count(IMPORT_SLICES, 1);
    sliced(state, |s| {
      let piece = usize::try_from(decoding.remaining())
        .unwrap_or(usize::MAX)
        .min(slice_bytes.max(1));
      if piece == 0 {
        return Ok(Some(()));
      }
      let handle = slot_of(s, archive)?;
      slice.resize(piece, 0);
      let read = read_archive(s, handle, file, decoding.fed(), slice.as_mut_slice())
        .map_err(|error| refusal_of_vfs(&error))?;
      decoding
        .feed(slice.get(..read).unwrap_or_default())
        .map_err(|error| refusal_of_import(&error, s.store.admittable()))?;
      // The unit held its slice while the decoder staged a field from it, never more than the stage's largest.
      let held = read.saturating_add(decoding.high_water());
      *stage_high_water = (*stage_high_water).max(held);
      if held > bound {
        s.count(IMPORT_STAGE_PAST_BOUND, 1);
      }
      Ok((read == 0 || decoding.remaining() == 0).then_some(()))
    })
  }

  /// One slice of the decoder's checks once the file is fed, a piece a unit (the trailer and the checksum; a chunk's
  /// payload held to its identity; the manifest): the archive once whole and verified.
  fn decode_step(
    &mut self,
    state: &mut ShardState,
  ) -> Result<Option<slates_archive::Archive>, Refusal> {
    let decoding = self.decoding.as_mut().ok_or_else(unreachable)?;
    state.count(IMPORT_SLICES, 1);
    sliced(state, |_| {
      decoding
        .step(|| true)
        .map_err(|error| refusal_of_import(&error, 0))
    })
  }

  /// Admits the new volume, as a create admits one (every credit reserved, or none), under its archive's name policy:
  /// one this daemon does not hold is refused `Unsupported`. Windows' upcase folding (A-576) is such a policy until the
  /// protocol and the catalog name it (A-576's part 2): the volume core folds by it, and no record of a volume can say so.
  fn admit(&mut self, state: &mut ShardState, name_policy_id: u32) -> Result<(), Refusal> {
    self.names = match slates_vfs::export::name_equivalence_of_id(name_policy_id) {
      Some(NameEquivalence::Exact) => NamePolicy::Exact,
      Some(NameEquivalence::Fold) => NamePolicy::Fold,
      Some(NameEquivalence::Upcase) | None => {
        return Err(Refusal::ArchiveRefused {
          fault: ArchiveFault::Unsupported,
        });
      }
    };
    // The id its name's alias already names (A-236), or one minted here.
    let id = match self.id {
      Some(id) => id,
      None => fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?,
    };
    let admitted = crate::verbs::admit(
      state,
      &self.principal,
      self.size,
      self.names,
      self.require_locked,
      None,
    )
    .map_err(|reply| crate::verbs::reply_refusal(*reply))?;
    self.admitted = Some(admitted);
    self.id = Some(id);
    Ok(())
  }

  /// Records the new volume — a plain scratch volume of the caller's, under its archive's name policy — moves it into
  /// the registry and publishes it; the reply.
  fn publish(&mut self, state: &mut ShardState) -> Result<ReplyBody, Refusal> {
    let id = self.id.ok_or_else(unreachable)?;
    let admitted = self.admitted.take().ok_or_else(unreachable)?;
    let record = VolumeRecord {
      id,
      name: self.name.clone(),
      owner_shard: state.partition,
      policy: PolicyRecord {
        size: crate::verbs::db_size(self.size),
        names: match self.names {
          NamePolicy::Exact => DbNamePolicy::Exact,
          NamePolicy::Fold => DbNamePolicy::Fold,
        },
        require_locked: self.require_locked,
        role: Role::Plain,
        live_shipped: false,
        live_holders: None,
        write: None,
        write_version: 0,
        signers: Vec::new(),
      },
      base: BaseRecord::Scratch,
      head: DbSnapshotId::default(),
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: self.principal.clone(),
      access: Vec::new(),
      created_ns: state.clock.monotonic_ns(),
      catalog_version: 0,
      taken_over: None,
    };
    let Admitted {
      volume,
      host,
      credits,
    } = admitted;
    let reply = crate::verbs::publish_created_volume(
      state,
      id,
      volume,
      host,
      (
        credits.reservation,
        credits.version,
        credits.metadata,
        credits.lock,
      ),
      record,
    );
    if let ReplyBody::Refused { refusal } = reply {
      return Err(refusal);
    }
    publish_new_volume(state, id)?;
    Ok(ReplyBody::ArchiveImported {
      id: wire_id(id),
      manifest: self.manifest,
    })
  }

  /// Releases a piece of what the import made, `true` while more is left: its buffer (one allocation), then the
  /// restored tree an entry a piece, then the archive a chunk a piece, then its manifest an entry a piece.
  fn shed(&mut self) -> bool {
    if self.slice.capacity() > 0 {
      self.slice = Vec::new();
      return true;
    }
    if self.decoding.take().is_some() {
      return true;
    }
    if let Some(restored) = self.restored.as_mut() {
      let more = restored.files.pop_first().is_some()
        || restored.metadata.pop_first().is_some()
        || restored.directories.pop_first().is_some()
        || restored.xattrs.pop_first().is_some();
      if !more {
        self.restored = None;
      }
      return true;
    }
    if let Some(archive) = self.decoded.as_mut() {
      if archive.chunks.pop().is_none() {
        let manifest = std::mem::replace(
          &mut archive.manifest,
          slates_archive::Node::Directory(Vec::new()),
        );
        self.shedding.push(manifest);
        self.decoded = None;
      }
      return true;
    }
    match self.shedding.pop() {
      Some(slates_archive::Node::Directory(mut entries)) => {
        // One entry a piece: its name and metadata freed here, its node released after it.
        if let Some(entry) = entries.pop() {
          self.shedding.push(slates_archive::Node::Directory(entries));
          self.shedding.push(entry.node);
        }
        true
      }
      Some(slates_archive::Node::File(_)) => true,
      None => false,
    }
  }

  /// Gives back what the import holds: the new volume, discarded with its credits.
  fn release(&mut self, state: &mut ShardState) {
    if let Some(admitted) = self.admitted.take() {
      crate::verbs::discard_admitted(state, admitted);
    }
  }
}

/// The archive file in the root of the volume in `handle`, and its length: a regular file, read through the volume's
/// host when it overlays one. A directory at its name is refused `IsDirectory`, a link or a special node `Invalid`.
fn locate_archive(
  state: &mut ShardState,
  handle: Handle<VolumeSlot>,
) -> Result<(InodeNo, u64), slates_vfs::VfsError> {
  let path = format!("/{ARCHIVE_FILE}");
  let ShardState { store, volumes, .. } = state;
  let slot = volumes
    .get_mut(handle)
    .map_err(|_| slates_vfs::VfsError::NotFound)?;
  let (located, observed) = match slot.host.as_mut() {
    Some(host) => {
      let mut overlay = slot.volume.with_host(host);
      let located = overlay.resolve(store, &path)?;
      let observed = overlay.observe(store, located.inode)?;
      (located, observed)
    }
    None => {
      let located = slot.volume.resolve(store, &path)?;
      let observed = slot.volume.observe(store, located.inode)?;
      (located, observed)
    }
  };
  match located.child {
    Child::File(_) => Ok((located.inode, observed.attrs.size)),
    Child::Dir(_) => Err(slates_vfs::VfsError::IsDirectory),
    _ => Err(slates_vfs::VfsError::Invalid),
  }
}

/// Reads the archive file `file` from `at` into `into`, in the volume of `handle`, through its host when it overlays
/// one; the bytes read, fewer at the file's end.
fn read_archive(
  state: &mut ShardState,
  handle: Handle<VolumeSlot>,
  file: InodeNo,
  at: u64,
  into: &mut [u8],
) -> Result<usize, slates_vfs::VfsError> {
  let ShardState { store, volumes, .. } = state;
  let slot = volumes
    .get_mut(handle)
    .map_err(|_| slates_vfs::VfsError::NotFound)?;
  match slot.host.as_mut() {
    Some(host) => slot.volume.with_host(host).read(store, file, at, into),
    None => slot.volume.read(store, file, at, into),
  }
}

/// The refusal an archive the import read is answered with (A-453): the reader's refusal by its kind
/// ([`ArchiveFault`]), each the class a caller acts on alike, and a restore over its budget `BudgetExceeded` naming the
/// `room` the shard could admit for it. Every reader refusal is classed here by name, so a new one must be classed.
fn refusal_of_import(error: &slates_archive::ArchiveError, room: u64) -> Refusal {
  use slates_archive::ArchiveError;
  let fault = match error {
    ArchiveError::OverBudget { .. } => return Refusal::BudgetExceeded { available: room },
    ArchiveError::BadMagic => ArchiveFault::ForeignMagic,
    ArchiveError::UnsupportedMajor { .. } | ArchiveError::UnknownRequiredFlag { .. } => {
      ArchiveFault::Unsupported
    }
    // A stream whose trailer magic is missing is not whole, as one too short for its frame is not.
    ArchiveError::Truncated | ArchiveError::BadTrailer => ArchiveFault::Truncated,
    ArchiveError::ArchiveHashMismatch
    | ArchiveError::ChunkIdentityMismatch { .. }
    | ArchiveError::ManifestHashMismatch => ArchiveFault::Corrupt,
    ArchiveError::BadSeekTable
    | ArchiveError::BadLength
    | ArchiveError::BadPayload { .. }
    | ArchiveError::MissingChunk
    | ArchiveError::ChunkMaxTooLarge { .. }
    | ArchiveError::ChunkTooLarge { .. }
    | ArchiveError::NonCanonicalChunk { .. }
    | ArchiveError::TotalsMismatch
    | ArchiveError::BadExtents
    | ArchiveError::BadName
    | ArchiveError::DuplicatePath
    | ArchiveError::BadLayout => ArchiveFault::Malformed,
  };
  Refusal::ArchiveRefused { fault }
}

/// The refusal laying a restored tree into the new volume is answered with: a tree that does not lay (a file against
/// its metadata, a hard link against its first name, a name count against its link count: `Populating`'s refusals)
/// `Malformed`, since its archive verified whole; the volume's own refusals (its room, its inodes) as it refuses them.
fn refusal_of_laying(error: &slates_vfs::VfsError) -> Refusal {
  match error {
    slates_vfs::VfsError::RecoveryIncomplete | slates_vfs::VfsError::NotFound => {
      Refusal::ArchiveRefused {
        fault: ArchiveFault::Malformed,
      }
    }
    other => refusal_of_vfs(other),
  }
}

/// Derived (A-494, D-11): the bytes of the archive file one unit of an import's reading reads and feeds. Each byte is
/// copied out of the archive volume, folded into the archive's BLAKE3 and copied into the chunk or manifest that keeps
/// it, so a unit of `b` bytes takes `b/memcpy + b/blake3 + b/memcpy` of the shard's time. A unit fits the slice
/// ([`sliced`], half the live step quantum) when `b = 1 / (2/copy_slice + 1/archive_slice)`, where `copy_slice_bytes`
/// and `archive_slice_bytes` are half the quantum at the deployment's measured memcpy and BLAKE3 rates: `c·h / (2h + c)`,
/// at least a byte. The reading's stage is bounded by it (`Decoding::bound`).
fn feed_slice_bytes(config: &crate::DaemonConfig) -> usize {
  let copy = u128::from(config.copy_slice_bytes().max(1));
  let hash = u128::from(config.archive_slice_bytes().max(1));
  let piece = copy
    .saturating_mul(hash)
    .checked_div(hash.saturating_mul(2).saturating_add(copy))
    .unwrap_or(0);
  usize::try_from(piece).unwrap_or(usize::MAX).max(1)
}

/// One slice of the restore's plan, a piece a unit; the plan once whole.
fn plan_step<'tree>(
  state: &mut ShardState,
  planning: &mut slates_archive::Planning<'tree>,
) -> Result<Option<slates_archive::Planned<'tree>>, Refusal> {
  state.count(IMPORT_SLICES, 1);
  sliced(state, |_| {
    planning
      .step(|| true)
      .map_err(|error| refusal_of_import(&error, 0))
  })
}

/// One slice of the restore, a piece a unit; the restored tree once whole. An allocation refused names `room`.
fn restore_step(
  state: &mut ShardState,
  restoring: &mut slates_archive::Restoring<'_>,
  room: u64,
) -> Result<Option<slates_archive::Restored>, Refusal> {
  state.count(IMPORT_SLICES, 1);
  sliced(state, |_| {
    restoring
      .step(|| true)
      .map_err(|error| refusal_of_import(&error, room))
  })
}

/// One slice of laying the restored `tree` into the new volume, a piece a unit, no piece writing or comparing more than
/// a copy slice of a file (a page at least); `true` once every piece is laid.
fn populate_step(
  state: &mut ShardState,
  populating: &mut crate::verbs::Populating,
  tree: &slates_archive::Restored,
  admitted: &mut Admitted,
) -> Result<bool, Refusal> {
  let slice_bytes = usize::try_from(state.config.copy_slice_bytes())
    .unwrap_or(usize::MAX)
    .max(state.config.page);
  state.count(IMPORT_SLICES, 1);
  sliced(state, |s| {
    populating
      .step(
        tree,
        &mut s.store,
        &mut admitted.volume,
        slice_bytes,
        || true,
      )
      .map(|laid| laid.then_some(()))
      .map_err(|error| refusal_of_laying(&error))
  })
  .map(|laid| laid.is_some())
}

/// Imports the archive a slice at a time; then commits its volume, or the refusal, as one atom and delivers the reply;
/// then releases what the import made.
async fn run_import(mut job: ImportJob, alias: Option<NameAlias>) {
  let reserved = reserve(alias.as_ref()).await;
  let outcome = match &reserved {
    Ok(()) => import_steps(&mut job).await,
    Err(refusal) => Err(refusal.clone()),
  };
  let request = job.request;
  let completed = crate::state::with_state(|s| {
    complete(s, request, |s| {
      match outcome.and_then(|()| job.publish(s)) {
        Ok(reply) => reply,
        Err(refusal) => {
          job.release(s);
          refused(refusal)
        }
      }
    })
  });
  let delivered = match completed {
    Some((reply, route)) => {
      settle(
        alias,
        reserved,
        matches!(reply, ReplyBody::ArchiveImported { .. }),
      )
      .await;
      crate::state::with_state(|s| deliver(s, reply, route))
    }
    None => None,
  };
  if delivered.is_none() {
    // The shard's state was unreachable (its shutdown): no record committed, so the client's retry, on the shard that
    // succeeds it, runs the verb again; the job's drop frees what it made.
    return;
  }
  shed_in_slices(|| job.shed()).await;
}

/// The import's slices: the archive file's length admitted, and the file read and fed to its decoder a feed slice a
/// unit, never held whole (A-494); the archive's checks a piece a unit, the checksum first; the new volume admitted
/// under the archive's name policy; the tree planned, admitted against what the volume may hold and the shard could
/// admit, and restored; and laid into the volume.
async fn import_steps(job: &mut ImportJob) -> Result<(), Refusal> {
  with(|s| job.open(s))?;
  while with(|s| {
    let slice = feed_slice_bytes(&s.config);
    job.feed_step(s, slice)
  })?
  .is_none()
  {
    slates_rt::futures::yield_now().await;
  }
  let archive = loop {
    if let Some(archive) = with(|s| job.decode_step(s))? {
      break archive;
    }
    slates_rt::futures::yield_now().await;
  };
  job.manifest = job
    .decoding
    .take()
    .and_then(|decoding| decoding.manifest_identity())
    .ok_or_else(unreachable)?;
  job.slice = Vec::new();
  with(|s| job.admit(s, archive.name_policy_id))?;
  let ImportJob {
    size,
    admitted,
    decoded,
    restored,
    ..
  } = job;
  let archive = &*decoded.insert(archive);
  let mut planning = slates_archive::Planning::new(archive);
  let planned = loop {
    slates_rt::futures::yield_now().await;
    if let Some(planned) = with(|s| plan_step(s, &mut planning))? {
      break planned;
    }
  };
  let own = admitted
    .as_ref()
    .and_then(|volume| volume.credits.reservation)
    .map_or(0, |reservation| reservation.bytes);
  let needed = planned.needed();
  let (budget, room) = with(|s| {
    Ok(crate::verbs::restore_budget(
      s,
      class_cap(*size),
      own,
      needed,
    ))
  })?;
  let mut restoring = planned
    .restoring(budget)
    .map_err(|error| refusal_of_import(&error, room))?;
  let tree = loop {
    slates_rt::futures::yield_now().await;
    if let Some(tree) = with(|s| restore_step(s, &mut restoring, room))? {
      break tree;
    }
  };
  drop(restoring);
  let tree = &*restored.insert(tree);
  let volume = admitted.as_mut().ok_or_else(unreachable)?;
  let mut populating = crate::verbs::Populating::new();
  loop {
    slates_rt::futures::yield_now().await;
    if with(|s| populate_step(s, &mut populating, tree, &mut *volume))? {
      return Ok(());
    }
  }
}

#[cfg(test)]
mod tests {
  use slates_archive::{Archive, Chunk, Decoding, Entry, Extent, Node, NodeMeta};
  use slates_db::catalog::{Principal, VolumeId as DbVolumeId};
  use slates_ipc::protocol::{ARCHIVE_FILE, NamePolicy, ReplyBody, RequestBody, SizeClass};
  use slates_vfs::Volume;
  use slates_vfs::dir::Child;
  use slates_vfs::ids::InodeNo;
  use slates_vfs::inode::Attrs;
  use slates_wire::request::RequestId;

  use super::{ImportJob, class_cap, plan_step, populate_step, restore_step};
  use crate::state::ShardState;

  /// Shape: the chunk the test archives are cut at, one 4 KiB base page as the takeover tests cut theirs, so a file
  /// spans several records.
  const TEST_CHUNK: usize = 4096;

  /// Shape: the size a test volume may grow to: room for every test archive and its tree, many times over.
  const ROOM: u64 = 1 << 26;

  /// The importing principal, the owner of every test volume.
  fn owner() -> Principal {
    Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    }
  }

  /// Noisy bytes no codec shrinks, from a xorshift seeded by `seed` (an odd seed: an even one starts as the odd one
  /// after it).
  fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
      .map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state.to_le_bytes()[0]
      })
      .collect()
  }

  /// A file node of `bytes` numbered `ino`, cut into [`TEST_CHUNK`]-byte chunks (LZ4-coded where that saves room),
  /// each distinct chunk added to `chunks` once.
  fn file(name: &str, ino: u64, bytes: &[u8], chunks: &mut Vec<Chunk>) -> Entry {
    let mut extents = Vec::new();
    for (piece, part) in bytes.chunks(TEST_CHUNK).enumerate() {
      let chunk = Archive::compressed_chunk(part.to_vec());
      extents.push(Extent {
        offset: (piece * TEST_CHUNK) as u64,
        len: part.len() as u64,
        chunk: chunk.identity,
        chunk_offset: 0,
      });
      if !chunks.iter().any(|kept| kept.identity == chunk.identity) {
        chunks.push(chunk);
      }
    }
    Entry {
      name: name.to_owned(),
      meta: NodeMeta {
        ino,
        mode: 0o100_640,
        size: bytes.len() as u64,
        nlink: 1,
        uid: 1234,
        gid: 456,
        mtime_ns: 2_000 + i64::try_from(ino).unwrap(),
        ..NodeMeta::default()
      },
      node: Node::File(extents),
    }
  }

  /// An archive of the root's `entries` over `chunks`, as an export writes one.
  fn archive(entries: Vec<Entry>, chunks: Vec<Chunk>) -> Archive {
    Archive {
      base_page_size: 4096,
      chunk_min: 4096,
      chunk_max: u32::try_from(TEST_CHUNK).unwrap(),
      created_unix: 0,
      volume_id: 1,
      snapshot_id: 1,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: NodeMeta {
        mode: 0o040_751,
        uid: 1234,
        gid: 456,
        mtime_ns: 9_000,
        ..NodeMeta::default()
      },
      manifest: Node::Directory(entries),
      chunks,
    }
  }

  /// The archive the oracle imports: a directory `d` holding a file of noisy bytes spanning several records and a file
  /// of repeated bytes (an LZ4 record), and an empty file in the root.
  fn tree_archive() -> Archive {
    let mut chunks = Vec::new();
    let big = file("big", 3, &noise(1, 3 * TEST_CHUNK + 100), &mut chunks);
    let small = file("small", 4, &[7; 2_000], &mut chunks);
    let empty = file("empty", 5, &[], &mut chunks);
    let directory = Entry {
      name: "d".to_owned(),
      meta: NodeMeta {
        ino: 2,
        mode: 0o040_750,
        nlink: 2,
        uid: 1234,
        gid: 456,
        mtime_ns: 3_000,
        ..NodeMeta::default()
      },
      node: Node::Directory(vec![big, small]),
    };
    archive(vec![directory, empty], chunks)
  }

  /// A volume of the owner's whose root holds `bytes` as the archive file, as a consumer writes one back.
  fn holding(state: &mut ShardState, name: &str, bytes: &[u8]) -> DbVolumeId {
    let reply = crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::Create {
        name: name.to_owned(),
        size: SizeClass::Dynamic { max: ROOM },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    let ReplyBody::Created { id } = reply else {
      panic!("{name}: {reply:?}")
    };
    let id = crate::verbs::to_db_volume(id);
    let handle = *state.by_id.get(&id).unwrap();
    let slot = state.volumes.get_mut(handle).unwrap();
    let root = slot.volume.root_inode(&state.store).unwrap();
    let archive_file = slot
      .volume
      .create_file_no(&mut state.store, root, ARCHIVE_FILE, 0o644)
      .unwrap();
    let mut at = 0;
    while at < bytes.len() {
      at += slot
        .volume
        .write(&mut state.store, archive_file, at as u64, &bytes[at..])
        .unwrap();
    }
    id
  }

  /// An import of the archive file `holder` holds into a volume named `name`, as `import_verb` makes one.
  fn import_of(holder: DbVolumeId, name: &str) -> ImportJob {
    ImportJob {
      request: (
        0,
        RequestId {
          client: 0,
          sequence: 0,
        },
      ),
      principal: owner(),
      archive: holder,
      archive_cap: ROOM,
      name: name.to_owned(),
      size: SizeClass::Bounded { limit: ROOM },
      require_locked: false,
      file: InodeNo(0),
      decoding: None,
      slice: Vec::new(),
      stage_high_water: 0,
      manifest: [0; 32],
      names: NamePolicy::Exact,
      admitted: None,
      id: None,
      decoded: None,
      restored: None,
      shedding: Vec::new(),
    }
  }

  /// Runs `job`'s import on `state` step by step as `import_steps` runs it, its file fed `slice` bytes a unit: the
  /// archive it decoded, its tree laid into the job's volume (admitted, not published).
  fn import_fed(state: &mut ShardState, job: &mut ImportJob, slice: usize) -> Archive {
    job.open(state).unwrap();
    while job.feed_step(state, slice).unwrap().is_none() {}
    let archive = loop {
      if let Some(archive) = job.decode_step(state).unwrap() {
        break archive;
      }
    };
    job.manifest = job
      .decoding
      .take()
      .and_then(|decoding| decoding.manifest_identity())
      .unwrap();
    job.admit(state, archive.name_policy_id).unwrap();
    let mut planning = slates_archive::Planning::new(&archive);
    let planned = loop {
      if let Some(planned) = plan_step(state, &mut planning).unwrap() {
        break planned;
      }
    };
    let own = job
      .admitted
      .as_ref()
      .and_then(|volume| volume.credits.reservation)
      .map_or(0, |reservation| reservation.bytes);
    let (budget, room) =
      crate::verbs::restore_budget(state, class_cap(job.size), own, planned.needed());
    let mut restoring = planned.restoring(budget).unwrap();
    let tree = loop {
      if let Some(tree) = restore_step(state, &mut restoring, room).unwrap() {
        break tree;
      }
    };
    drop(restoring);
    let volume = job.admitted.as_mut().unwrap();
    let mut populating = crate::verbs::Populating::new();
    while !populate_step(state, &mut populating, &tree, volume).unwrap() {}
    archive
  }

  /// What `volume` holds at each of `paths` and at its root: the attributes and a file's bytes, read whole.
  fn view(state: &ShardState, volume: &Volume, paths: &[&str]) -> Vec<(String, Attrs, Vec<u8>)> {
    let root = volume.root_inode(&state.store).unwrap();
    let mut view = vec![(
      String::new(),
      volume.stat(&state.store, root).unwrap(),
      Vec::new(),
    )];
    for path in paths {
      let located = volume.resolve(&state.store, path).unwrap();
      let no = located.inode;
      let attrs = volume.stat(&state.store, no).unwrap();
      let mut bytes = Vec::new();
      if matches!(located.child, Child::File(_)) {
        bytes.resize(usize::try_from(attrs.size).unwrap(), 0);
        let read = volume.read(&state.store, no, 0, &mut bytes).unwrap();
        bytes.truncate(read);
      }
      view.push(((*path).to_owned(), attrs, bytes));
    }
    view
  }

  /// A-494 (the oracle): do write an archive (a directory of a file spanning several records and an LZ4-coded one, and
  /// an empty file) into a volume's root, then import it twice: once its file fed to the decoder whole, one slice of
  /// its length (as the import read it before A-494), and once fed 7 bytes a unit. Expect both imports to decode the
  /// archive the whole-buffer decoder gives and to lay the same volume, byte for byte: every path's attributes (mode,
  /// owner, link count, size, times) and bytes, and the root's; the whole import to have held the file whole and the
  /// sliced one never more than `Decoding::bound(7)` of it (the two paths ran as named).
  #[test]
  fn an_archive_imported_a_slice_at_a_time_lays_the_volume_the_whole_file_lays() {
    crate::daemon::audit_on_shard(|state| {
      let archive = tree_archive();
      let bytes = archive.encode();
      let holder = holding(state, "holder", &bytes);
      let mut whole = import_of(holder, "imported-whole");
      let decoded_whole = import_fed(state, &mut whole, bytes.len());
      let mut sliced = import_of(holder, "imported-sliced");
      let decoded_sliced = import_fed(state, &mut sliced, 7);
      assert_eq!(decoded_whole, Archive::decode(&bytes).unwrap());
      assert_eq!(decoded_sliced, decoded_whole);
      assert_eq!(sliced.manifest, archive.manifest_identity());
      let paths = ["d", "d/big", "d/small", "empty"];
      let laid_whole = view(state, &whole.admitted.as_ref().unwrap().volume, &paths);
      let laid_sliced = view(state, &sliced.admitted.as_ref().unwrap().volume, &paths);
      assert_eq!(laid_sliced, laid_whole, "the same volume either way");
      assert_eq!(laid_whole[2].2, noise(1, 3 * TEST_CHUNK + 100));
      assert_eq!(laid_whole[3].2, vec![7; 2_000]);
      assert!(
        whole.stage_high_water >= bytes.len(),
        "the whole import held the file whole: {} of {}",
        whole.stage_high_water,
        bytes.len()
      );
      assert!(
        sliced.stage_high_water <= Decoding::bound(7),
        "the sliced import held {} of the file at once",
        sliced.stage_high_water
      );
      whole.release(state);
      sliced.release(state);
    });
  }

  /// A-494 (the bound): do import an archive more than a hundred times longer than the reading's bound at a 64-byte
  /// feed slice (eight files of noisy bytes, raw records). Expect its tree laid as the whole-buffer decoder reads it,
  /// and the most of the file the reading held at once within `Decoding::bound(64)`: its slice and one field, never
  /// the file.
  #[test]
  fn an_import_reads_an_archive_many_times_its_bound_within_it() {
    crate::daemon::audit_on_shard(|state| {
      let mut chunks = Vec::new();
      let entries = (0..8u64)
        .map(|number| {
          file(
            &format!("file-{number}"),
            number + 2,
            &noise(2 * number + 1, 3_000),
            &mut chunks,
          )
        })
        .collect();
      let archive = archive(entries, chunks);
      let bytes = archive.encode();
      let slice = 64;
      let bound = Decoding::bound(slice);
      assert!(
        bytes.len() > 100 * bound,
        "{} bytes against a bound of {bound}",
        bytes.len()
      );
      let holder = holding(state, "holder", &bytes);
      let mut import = import_of(holder, "imported-bounded");
      let decoded = import_fed(state, &mut import, slice);
      assert_eq!(decoded, Archive::decode(&bytes).unwrap());
      assert!(
        import.stage_high_water <= bound,
        "the reading held {} of the file at once, past its bound of {bound}",
        import.stage_high_water
      );
      let laid = view(
        state,
        &import.admitted.as_ref().unwrap().volume,
        &["file-7"],
      );
      assert_eq!(laid[1].2, noise(15, 3_000));
      import.release(state);
    });
  }
}
