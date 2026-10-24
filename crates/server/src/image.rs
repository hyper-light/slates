//! The daemon's `image build` and `image merge` (§4.6, A-160; the volume's side is [`slates_vfs::image`]): a snapshot
//! rendered as an ext2 image file in a new volume, for a VMM that shares no folder with its guest (Firecracker), and
//! the image a guest has written merged into a new clone of the snapshot it was built from.
//!
//! **Running.** Each verb checks what it can at once — the volume, the right, the name, the snapshot — and defers its
//! reply to a task of the volume's owner shard, as a granted landing does (AUD-29-03). The task steps the work a slice
//! at a time, yielding between slices, so the shard serves its clients while an image of any size is built or merged
//! (§4.3). A slice is bounded against the clock, half the live step quantum
//! ([`crate::config::DaemonConfig::copy_slice_ns`]), one unit at a time — an entry walked, a block written, a chunk
//! compared — since a unit's cost is not its bytes. Measured on 2026-10-24, slices bounded by 256 KiB of work ran
//! 230 µs to 1.5 ms, ten to sixty times the box's 24 µs. A retry while it runs joins it.
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
  BaseRecord, PolicyRecord, Principal, Role, SizeClass as DbSizeClass, SnapshotId as DbSnapshotId,
  VolumeId as DbVolumeId, VolumeRecord, VolumeState,
};
use slates_image::{Builder, Identity, Progress, Timestamp, Walk, Walked};
use slates_ipc::protocol::{
  IMAGE_FILE, ImageMergeReport, NamePolicy, Refusal, ReplyBody, SizeClass, SnapshotId, VolumeId,
};
use slates_mem::Handle;
use slates_vfs::clock::{Clock, HostClock};
use slates_vfs::ids::InodeNo;
use slates_vfs::image::{
  BuildIo, ImageContent, ImageError, Merge, MergeReport, SnapshotFile, SnapshotTree, SnapshotWalk,
};
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
fn sliced<T>(
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

/// The image builds and merges running on this shard (§4.6, A-160).
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
    ImageError::NodeUnsupported { path } => {
      format!("a device at {path}, which a volume cannot hold")
    }
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
fn with<R>(step: impl FnOnce(&mut ShardState) -> Result<R, Refusal>) -> Result<R, Refusal> {
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
fn defer(state: &mut ShardState, task: impl Future<Output = ()> + 'static) -> ReplyBody {
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
fn complete(
  state: &mut ShardState,
  (origin, id): (u64, RequestId),
  produce: impl FnOnce(&mut ShardState) -> ReplyBody,
) -> (ReplyBody, Option<ReplyRoute>) {
  let route = state
    .image
    .in_flight
    .remove(&(origin, id.client, id.sequence))
    .flatten();
  state.db.begin();
  let reply = produce(state);
  let recorded = crate::verbs::record_completion(state, origin, id, reply);
  let reply = match state.db.commit(&mut state.segment) {
    Ok(_) => recorded,
    Err(e) => {
      // Nothing of the outcome is durable: counted, never recorded as the completion, so a retry runs again. The new
      // volume, whose record rolled back, is torn down with every other effect the rollback left unrecorded.
      let refusal = refusal_of_db(&e);
      state.count(crate::verbs::refusal_name(&refusal), 1);
      crate::verbs::reconcile_unpublished_effects(state);
      refused(refusal)
    }
  };
  (reply, route)
}

/// Hands a task's reply to where its request waits, on its client's shard. A request with no route (a forward from
/// another node) reads its completion record instead.
fn deliver(state: &mut ShardState, reply: ReplyBody, route: Option<ReplyRoute>) {
  let Some(route) = route else {
    return;
  };
  let delivery = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      crate::state::deliver(route.client_index, route.request, reply, true);
    }),
    None,
  );
  if slates_rt::registry::send_control(
    route.shard,
    slates_rt::control::Control::Spawn(Box::new(delivery)),
  )
  .is_err()
  {
    state.count(REPLY_UNDELIVERED, 1);
  }
}

/// Publishes the shard so the new volume's image precedes its completion (§4.8, AUD-05), as `dispatch` does for a
/// create; refused when the publication did not capture it.
fn publish_new_volume(state: &mut ShardState, id: DbVolumeId) -> Result<(), Refusal> {
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
  if let Some(existing) = state.db.partition().volume_by_name(&name) {
    return refused(Refusal::AlreadyExists {
      existing: wire_id(existing.id),
    });
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
    inodes: Vec::new(),
    builder: None,
    admitted: None,
    file: InodeNo(0),
    image: None,
  };
  defer(state, run_build(job))
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
  inodes: Vec<InodeNo>,
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
    let id = fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?;
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

/// Builds the image a slice at a time; then commits its volume, or the refusal, as one atom and delivers the reply.
async fn run_build(mut job: BuildJob) {
  let outcome = build_steps(&mut job).await;
  let request = job.request;
  let delivered = crate::state::with_state(|s| {
    let (reply, route) = complete(s, request, |s| {
      match outcome.and_then(|written| job.publish(s, written)) {
        Ok(reply) => reply,
        Err(refusal) => {
          job.release(s);
          refused(refusal)
        }
      }
    });
    deliver(s, reply, route);
  });
  if delivered.is_none() {
    // The shard's state was unreachable (its shutdown): no record committed, so the client's retry, on the shard that
    // succeeds it, runs the verb again.
  }
}

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
  if let Some(existing) = state.db.partition().volume_by_name(&name) {
    return refused(Refusal::AlreadyExists {
      existing: wire_id(existing.id),
    });
  }
  if !state.by_id.contains_key(&base) {
    return refused(Refusal::NotFound);
  }
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "an image verb outside a recorded request".to_owned(),
    });
  };
  let job = MergeJob {
    request,
    principal: principal.clone(),
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
  defer(state, run_merge(job))
}

/// A merge between its slices: what it holds, released by [`MergeJob::release`] on a refusal or by its drop.
struct MergeJob {
  request: (u64, RequestId),
  principal: Principal,
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
        self.base_tree = Some(tree);
        self.base_walk = None;
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
        Walked::Done(tree) => Ok(Some(tree)),
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
    self.image_walk = None;
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
    let id = crate::verbs::publish_clone(
      state,
      &self.principal,
      origin,
      &self.base_record,
      (self.snapshot, &self.name),
      clone,
    )?;
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

/// Merges the image a slice at a time; then commits the clone, or the refusal, as one atom and delivers the reply.
async fn run_merge(mut job: MergeJob) {
  let outcome = merge_steps(&mut job).await;
  let request = job.request;
  let delivered = crate::state::with_state(|s| {
    let (reply, route) = complete(s, request, |s| {
      match outcome.and_then(|report| job.publish(s, report)) {
        Ok(reply) => reply,
        Err(refusal) => {
          job.release(s);
          refused(refusal)
        }
      }
    });
    job.drop_image_snapshot(s);
    deliver(s, reply, route);
  });
  if delivered.is_none() {
    // The shard's state was unreachable (its shutdown): no record committed, so the client's retry, on the shard that
    // succeeds it, runs the verb again.
  }
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
