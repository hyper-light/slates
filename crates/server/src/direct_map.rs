//! The same-kernel direct map (§4.6; A-135, whose mechanism A-171 replaced): a client process of the daemon's own uid
//! maps one file's bytes read alone and reads them as memory, with no request and no copy per read.
//!
//! **What the client is handed.** A descriptor carries its whole object, read and write, to whoever holds it (A-135;
//! the 2026-10-18 escape handed a peer the shard's whole content arena), so the object handed is one file's copy and
//! nothing else: a [`slates_mem::PeerObject`], the file's page span, filled from the volume. On Linux it is a memfd
//! sealed against every writer but the daemon's own view (`F_SEAL_FUTURE_WRITE`, `F_SEAL_SHRINK`, `F_SEAL_GROW`,
//! `F_SEAL_SEAL`), and whether it may leave is judged where every descriptor leaves the process
//! (`Control::send_mapping`, `confined_for_a_peer`): the object must be what is authorized, and sealed. macOS has no
//! seals, so the client gets a second descriptor of an unlinked shared-memory object, opened read alone, which XNU
//! refuses every way to write through or resize (A-173), judged before it is offered
//! (`slates_ipc::route::confined_map`: read alone, and exactly the authorized length). Windows passes no
//! descriptor: the copy is an unnamed pagefile-backed section whose security descriptor grants no one any right
//! (`OWNER RIGHTS` may read it alone), and the daemon duplicates a read-only handle of it into the client's process,
//! judged first (`slates_ipc::handle_route::hand_section`: exactly the authorized size, and a duplicate that writes, one
//! that may rewrite the descriptor and a writable view each refused) and the process checked to run as the daemon's
//! user (A-175). The copy is kept in the
//! store's backings ([`slates_vfs::shards_backing`]), so the volume's single write boundary writes every later change
//! into it before the change returns: the client's map is a live view of the file within its length, as a shared map of
//! a file is.
//!
//! **Running.** Every map verb names its volume and runs on the volume's shard, whichever shard serves its client
//! (A-188). The verb checks what it can at once (the uid, the view, the volume, the right, the path, and for a client
//! served here its channel), makes the copy, and defers its reply to a task of this shard, as an image build does
//! (A-160). The task
//! fills the copy a slice at a time ([`crate::config::DaemonConfig::copy_slice_page_bytes`], the slice of a DAX
//! backing's fill, A-163), yielding between slices, so the shard serves its clients while a file of any size is copied.
//! A range is marked filled before its slice reads the volume, so a change written meanwhile lands on it and the rest
//! of the fill reads the volume as changed. Holes and pages of zeros are passed over: the new copy reads zero there
//! already. The fill done, the copy is handed: on Linux its descriptor goes over the client's control socket, sent by
//! the shard that serves the client, since a delivery is sealed under the client's own channel there (A-187), and the
//! copy's shard asks it by a cross-shard call before the reply goes on the client's ring; on macOS the descriptor is
//! offered on the daemon's socket route (A-161), on the copy's shard, behind a fresh single-use secret that the reply
//! carries, and the client claims it by the mapping id, whose high bits name that shard; on Windows the handle is
//! duplicated into the client's process, which the client's shard names, and the reply names its value there. The
//! reply's completion is recorded as every verb's is, a Windows handle's value left out ([`recorded_form`]), and is
//! delivered to the client's ring on its own shard.
//!
//! **What reads by copy instead** (`MapUnavailable`, which the client answers with its paged read): a peer of another
//! uid, a view other than the head, a merge volume, an overlay (an untouched file follows the disk, which no write
//! boundary sees), a request forwarded from another host, and a copy the fill budget, the backings' capacity or the OS
//! refuses.
//!
//! **A writable map (A-186).** `MapWrite` names the caller's own write attachment of the volume and resolves the file
//! through it, as a file verb does (`crate::fs_verbs::with_writer`, its scope applied), then makes the same copy
//! writable (`slates_mem::PeerAccess::ReadWrite`): sealed against resizing and new seals but not against writes on
//! Linux, handed read and write on macOS, duplicated with a writable handle on Windows, each judged for that access
//! where it leaves the process. The client's stores land in the copy. They reach the file when the daemon compares the
//! copy back (`slates_bridge_core::dax_backing::write_back_slice`, A-163's compare-back, one implementation for both),
//! up to the file's end, writing each run that differs through the map's attachment, a copy slice a turn, and
//! publishes before it replies. That happens at the client's `MapSync`, at its `ReleaseMap` (whose reply then reports
//! the write-back), and before its reap removes its attachments (`crate::reap`), so a store is not lost to its client's
//! exit, as a store to a shared map outlives its process. A `Detach` of the attachment is refused `MapsHeld` while a
//! writable map made under it is held. A copy detached from its file (the volume destroyed, or the file gone for good)
//! is never compared back, since its inode number may name another file by then: its stores reach nothing, as a
//! deleted file's do. An in-process guest's writable DAX window and a writable map exclude each other, as a Shards
//! backing does (A-163).
//!
//! **Lifetime and bounds (A-172, A-188).** A copy is held where it is, on its volume's shard, in that shard's table
//! under its client ([`DirectMapState::held`], with the principal its write-back is checked under), from the moment it is
//! made until the client releases it (`ReleaseMap`) or is reaped: the client's own shard runs its reap and reaches every
//! shard, writing back the client's writable maps there before its attachments there go, and letting its copies go
//! after ([`write_back_held`], [`let_go_held`]; `crate::reap`). Every way a copy goes is [`let_go`], which withdraws an
//! unclaimed offer of it too (macOS), so an offer never outlives its copy. A volume's destroy or a file's last unlink
//! only detaches its copies from the write-through index: the client's map keeps their pages, so they stay held and
//! counted. A client maps within its share of the shard's capacity ([`share_of`], [`within_share`]), counted over the
//! clients this shard serves, those holding its copies, and the one asking. A copy whose hand-off did not happen (its
//! client gone, its channel refusing, the task ended early) is given back at once.

use std::collections::BTreeMap;

use slates_ipc::protocol::{Refusal, ReplyBody, VolumeId};
use slates_mem::{Encoded, Handle};
use slates_vfs::shards_backing::ShardsBacking;
use slates_wire::request::RequestId;

use crate::merge_service::ReplyRoute;
use crate::state::ShardState;
use crate::verbs::refused;

/// Counter: a direct map its client released (`ReleaseMap`): the copy let go and its bytes given back.
const RELEASED: &str = "dax.local_map_released";

/// A copy a client holds (§4.6): its backing, and for a writable map (A-186) the attachment its stores are written
/// back under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeldMap {
  /// The copy.
  pub(crate) backing: Handle<ShardsBacking>,
  /// For a writable map, its volume and the caller's write attachment it was made under; `None` for a read-only map.
  pub(crate) writer: Option<(VolumeId, u64)>,
}

/// The copies one client holds on this shard (§4.6, A-172, A-188), whichever shard serves the client: its principal,
/// under which its reap writes a writable map back, and its maps, handed or being filled.
pub(crate) struct ClientMaps {
  /// The client's principal, as the request that made its first copy here named it.
  pub(crate) principal: slates_db::catalog::Principal,
  /// Its maps.
  pub(crate) maps: Vec<HeldMap>,
}

/// The writable maps any client holds under `attachment` (§4.6, A-186): what a `Detach` of it is refused for
/// (`MapsHeld`), since their stores reach the file only through it. A map whose copy the store let go already (a fill
/// that ended early) is not one.
pub(crate) fn writable_maps_of(state: &ShardState, attachment: u64) -> u64 {
  state
    .direct_map
    .held
    .values()
    .flat_map(|client| client.maps.iter())
    .filter(|held| held.writer.is_some_and(|(_, writer)| writer == attachment))
    .filter(|held| {
      state
        .store
        .shards_backings
        .peer_bytes(held.backing)
        .is_some()
    })
    .fold(0u64, |count, _| count.saturating_add(1))
}
/// Counter: a copy's offer on the socket route withdrawn unclaimed because its copy was let go (macOS).
#[cfg(target_os = "macos")]
const OFFER_WITHDRAWN: &str = "dax.local_map_offer_withdrawn";

/// Lets go of a copy (§4.6, A-172): it leaves the store's backings, so it is written through no more and its bytes go
/// back to the fill budget, and its offer on the socket route goes with it if still unclaimed (macOS), so no offer keeps
/// an object alive outside its client's share. The one way a copy is let go: a release, a reap, a fill that ended
/// without its hand-off. Whether the copy was still kept.
pub(crate) fn let_go(state: &mut ShardState, backing: Handle<ShardsBacking>) -> bool {
  withdraw_offer(state, backing);
  state.store.shards_backings.remove(backing).is_some()
}

/// Withdraws the unclaimed offer of `backing`'s copy on the socket route, counted.
#[cfg(target_os = "macos")]
fn withdraw_offer(state: &mut ShardState, backing: Handle<ShardsBacking>) {
  let Some(mapping_id) = mapping_id_of(state.partition, backing) else {
    return;
  };
  if crate::shards_route::withdraw(state, (slates_ipc::route::OfferKind::Map, mapping_id)) {
    state.count(OFFER_WITHDRAWN, 1);
  }
}

/// Nothing to withdraw: only macOS offers a copy on a socket route (Linux sends it on the control socket before the
/// reply).
#[cfg(not(target_os = "macos"))]
fn withdraw_offer(_state: &mut ShardState, _backing: Handle<ShardsBacking>) {}

/// The mapping id a client names a copy by: its backing's handle packed with the shard's partition in the high 16 bits,
/// as an attachment id carries it, so macOS's socket route finds the shard that holds the copy's offer
/// (`crate::shards_route`). `None` where the handle does not fit the packing (a slot or a generation past 2^24): that
/// copy is not handed. Process-local, like every handle, so a client's maps end with its daemon's process.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn mapping_id_of(partition: u16, backing: Handle<ShardsBacking>) -> Option<u64> {
  backing.encode(partition).map(Encoded::word)
}

/// The backing a mapping id names; the slab refuses a stale one.
fn backing_of(mapping_id: u64) -> Handle<ShardsBacking> {
  let encoded = Encoded::from_word(mapping_id);
  Handle::from_raw(encoded.slot(), encoded.generation())
}

/// A client's share of a shard's direct maps (§4.6, A-171): the shard's capacity — the copies it may hold and its fill
/// budget's bytes — over the clients it serves now, so one client never takes every copy from the others. A lone
/// client's share is the whole capacity; copies held already are never taken back, so a client that held more before
/// others came maps nothing more until it is within its share again.
// Pure, so tested on every host; Linux and macOS hand copies (Windows' route is owed), so elsewhere only the tests call
// it.
#[cfg_attr(
  not(any(target_os = "linux", target_os = "macos", test)),
  allow(dead_code)
)]
pub(crate) fn share_of(
  (most_copies, budget_bytes): (usize, u64),
  clients_served: usize,
) -> (usize, u64) {
  let clients = clients_served.max(1);
  (
    most_copies.checked_div(clients).unwrap_or(0),
    budget_bytes
      .checked_div(u64::try_from(clients).unwrap_or(u64::MAX))
      .unwrap_or(0),
  )
}

/// Whether a client holding `(maps, bytes)` may take one more copy of `object_bytes` within its `share`; refused
/// `MapShareExhausted` with the counts if not.
// Pure, so tested on every host; Linux and macOS hand copies (Windows' route is owed), so elsewhere only the tests call
// it.
#[cfg_attr(
  not(any(target_os = "linux", target_os = "macos", test)),
  allow(dead_code)
)]
pub(crate) fn within_share(
  (held_maps, held_bytes): (usize, u64),
  object_bytes: u64,
  (most_maps, share_bytes): (usize, u64),
) -> Result<(), Refusal> {
  if held_maps >= most_maps || held_bytes.saturating_add(object_bytes) > share_bytes {
    return Err(Refusal::MapShareExhausted {
      held_maps: u64::try_from(held_maps).unwrap_or(u64::MAX),
      most_maps: u64::try_from(most_maps).unwrap_or(u64::MAX),
      held_bytes,
      share_bytes,
    });
  }
  Ok(())
}

/// `ReleaseMap` (§4.6, A-171): lets go of a direct map this client holds. The copy leaves the store's backings, so it
/// is written through no more and its bytes go back to the fill budget; the client unmapped its own view first. A
/// writable map's stores are written back first (A-186), a slice a turn, and the reply reports that write-back
/// (`MapSynced`, or its refusal); the copy is let go whatever it answers. `NotFound` for a map this client does not
/// hold: another client's, one released already, or one from before a restart.
pub(crate) fn release_map(
  state: &mut ShardState,
  client_id: u32,
  principal: &slates_db::catalog::Principal,
  mapping_id: u64,
) -> ReplyBody {
  let backing = backing_of(mapping_id);
  let Some(held) = held_by(state, client_id, backing) else {
    return refused(Refusal::NotFound);
  };
  if held.writer.is_some() {
    return sync_then(state, client_id, principal, held, AfterSync::Release);
  }
  forget(state, client_id, backing);
  if let_go(state, backing) {
    state.count(RELEASED, 1);
  }
  ReplyBody::MapReleased
}

/// The map `backing` client `client_id` holds on this shard, if it does. A dying client's ring is frozen before its
/// reap begins, so no verb of its reaches here once its maps are being let go.
fn held_by(state: &ShardState, client_id: u32, backing: Handle<ShardsBacking>) -> Option<HeldMap> {
  state
    .direct_map
    .held
    .get(&client_id)?
    .maps
    .iter()
    .find(|held| held.backing == backing)
    .copied()
}

/// Takes `backing` off client `client_id`'s maps on this shard, and the client off the table once it holds none here, so
/// the table holds only clients that hold copies. Whether it was held.
fn forget(state: &mut ShardState, client_id: u32, backing: Handle<ShardsBacking>) -> bool {
  let Some(client) = state.direct_map.held.get_mut(&client_id) else {
    return false;
  };
  let Some(at) = client.maps.iter().position(|held| held.backing == backing) else {
    return false;
  };
  client.maps.swap_remove(at);
  if client.maps.is_empty() {
    state.direct_map.held.remove(&client_id);
  }
  true
}

/// Takes `backing` off client `client_id`'s maps and lets its copy go: a fill that ended without its hand-off. Whether
/// the copy was still kept.
fn give_back(state: &mut ShardState, client_id: u32, backing: Handle<ShardsBacking>) -> bool {
  forget(state, client_id, backing);
  let_go(state, backing)
}

/// Counter: the direct maps' copies a dead client's reap gave back (§4.6, A-171), each kept until then by the client's
/// own map of it.
const REAPED: &str = "dax.local_map_reaped";

/// Lets go of every copy the dead client `client_id` holds on this shard (§4.6, A-172, A-188), once its reap removed its
/// attachments here: its map of each lives on in its process if the process does, no longer kept equal to the file, and
/// each copy's bytes go back to the backings' budget; an offer of one it never claimed (macOS) is withdrawn with it. A
/// writable one's stores were written back first ([`write_back_held`]). The client's own shard runs its reap and reaches
/// this one by a cross-shard call. How many copies were let go, counted (`dax.local_map_reaped`).
pub(crate) fn let_go_held(state: &mut ShardState, client_id: u32) -> usize {
  let Some(client) = state.direct_map.held.remove(&client_id) else {
    return 0;
  };
  let reaped = client
    .maps
    .into_iter()
    .filter(|held| let_go(state, held.backing))
    .count();
  if reaped > 0 {
    state.count(REAPED, u64::try_from(reaped).unwrap_or(u64::MAX));
  }
  reaped
}

/// Counter: a writable map's write-back that finished (`MapSync`, a writable `ReleaseMap`, or a reap; §4.6 A-186), the
/// writable map's non-vacuity counter: a test asserts it moved, so a dead write-back cannot pass as a slow one.
const SYNCED: &str = "dax.local_map_synced";
/// Counter: the bytes the finished write-backs wrote into files: the runs where a copy differed from its file.
const SYNCED_BYTES: &str = "dax.local_map_synced_bytes";
/// Counter: a write-back refused (its attachment or its file gone, the volume refusing a write, a publication that did
/// not capture the volume): that map's stores did not all reach the file.
const SYNC_REFUSED: &str = "dax.local_map_sync_refused";

/// `MapSync` (§4.6, A-186): writes the stores this client made through its writable map `mapping_id` of `volume` into
/// the file, deferring the reply to the write-back ([`Sync`]), which answers `MapSynced`. `NotFound` for a map this
/// client does not hold, a read-only one, or one of another volume.
pub(crate) fn map_sync(
  state: &mut ShardState,
  client_id: u32,
  principal: &slates_db::catalog::Principal,
  (volume, mapping_id): (VolumeId, u64),
) -> ReplyBody {
  match held_by(state, client_id, backing_of(mapping_id)) {
    Some(held) if held.writer.is_some_and(|(of, _)| of == volume) => {
      sync_then(state, client_id, principal, held, AfterSync::Answer)
    }
    _ => refused(Refusal::NotFound),
  }
}

/// What a write-back answers when it ends: `MapSync`'s reply, or a `ReleaseMap`'s, the copy let go after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterSync {
  /// `MapSync`: the map stays held.
  Answer,
  /// `ReleaseMap`: the map is let go whatever the write-back answered.
  Release,
}

/// Defers the current request's reply to a write-back of `held` ([`Sync`]) that answers as `after` says. Refused
/// `ConsensusNotInitialized` before the shard can publish, as every verb that changes its image is, and as
/// [`defer_task`] refuses; the map stays held either way.
fn sync_then(
  state: &mut ShardState,
  client_id: u32,
  principal: &slates_db::catalog::Principal,
  held: HeldMap,
  after: AfterSync,
) -> ReplyBody {
  if !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
  }
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a direct map's write-back outside a recorded request".to_owned(),
    });
  };
  // Where the map's volume stands in its journal before the write-back: its reply waits for the volume's FUSE mounts
  // to be told of what it writes (AC-3.3; `crate::told`).
  let mark = held
    .writer
    .and_then(|(volume, _)| crate::told::mark(state, crate::verbs::to_db_volume(volume)));
  let sync = match Sync::new(state, client_id, held, principal.clone()) {
    Ok(sync) => sync,
    Err(refusal) => return refused(refusal),
  };
  match defer_task(state, request, answer_after(sync, request, (after, mark))) {
    // Never seen: `run_recorded` sends nothing for a verb that deferred its acceptance.
    Ok(()) => ReplyBody::Acknowledged,
    Err(refusal) => refused(refusal),
  }
}

/// Runs `sync` to its end, then answers the request it was for: `MapSynced`, or the write-back's refusal; for a
/// release, the copy let go first, whatever the write-back answered. Records the reply's completion and delivers it,
/// once every FUSE mount of the volume has been told of what the write-back wrote since `mark` (AC-3.3).
async fn answer_after(
  mut sync: Sync,
  request: (u64, RequestId),
  (after, mark): (AfterSync, Option<crate::told::Mark>),
) {
  // The shard's state is unreachable (its shutdown): no reply can be recorded, and the client's retry, on the shard
  // that succeeds this one, finds no map.
  let Some(outcome) = sync.run().await else {
    return;
  };
  let answered = crate::state::with_state(|s| {
    let reply = match outcome {
      Ok(synced) => ReplyBody::MapSynced {
        written: synced.written,
        file_len: synced.file_len,
      },
      Err(refusal) => refused(refusal),
    };
    if after == AfterSync::Release {
      forget(s, sync.client_id, sync.held.backing);
      if let_go(s, sync.held.backing) {
        s.count(RELEASED, 1);
      }
    }
    let (reply, route) = complete(s, request, reply);
    let owed = mark.and_then(|mark| crate::told::owed(s, mark));
    (reply, route, s.shard, owed)
  });
  // Delivered after the state's borrow ends, since delivery takes it again.
  if let Some((reply, Some(route), here, owed)) = answered {
    if let Some(owed) = owed {
      let _ = crate::told::wait(owed).await;
    }
    deliver_to(route, reply, here).await;
  }
}

/// Delivers a deferred reply where its request waits: on this shard's ring when its client is served here, else as a
/// task on the client's shard (A-188), as an image build's reply goes, waiting for room as every reply carried home
/// does (`crate::xshard::send_back`).
async fn deliver_to(route: ReplyRoute, reply: ReplyBody, here: u16) {
  if route.shard == here {
    crate::state::deliver(route.client_index, route.request, reply, true);
    return;
  }
  let delivery = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      crate::state::deliver(route.client_index, route.request, reply, true);
    }),
    None,
  );
  crate::xshard::send_back(route.shard, delivery).await;
}

/// Writes back every writable map the dead client `client_id` holds on this shard (§4.6, A-186, A-188), before its reap
/// removes its attachments here, through which alone the stores reach their files: a store to a shared map outlives
/// its process. Each runs to its end, a slice a turn, under the principal the client's maps here were made under; a
/// refusal is counted and the next goes on, since no one is left to answer. The client's own shard runs its reap and
/// reaches this one through `crate::xshard::task_within`.
pub(crate) async fn write_back_held(client_id: u32) {
  let Some((principal, writable)) = crate::state::with_state(|s| {
    let client = s.direct_map.held.get(&client_id)?;
    let writable: Vec<HeldMap> = client
      .maps
      .iter()
      .filter(|held| held.writer.is_some())
      .copied()
      .collect();
    Some((client.principal.clone(), writable))
  })
  .flatten() else {
    return;
  };
  for held in writable {
    let Some(Ok(mut sync)) =
      crate::state::with_state(|s| Sync::new(s, client_id, held, principal.clone()))
    else {
      continue;
    };
    // Counted by `Sync::run` either way (`dax.local_map_synced`, `dax.local_map_sync_refused`): no one is left to
    // answer.
    let _counted = sync.run().await;
  }
}

/// What a finished write-back did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Synced {
  /// The bytes written: the runs where the copy differed from the file.
  written: u64,
  /// The file's length when the comparison began.
  file_len: u64,
}

/// A writable map's stores being written into its file (§4.6, A-186): the copy compared with the file a copy slice a
/// turn, from its start to the file's end as it stood at the first slice (within the copy), each run that differs
/// written through the map's attachment ([`crate::fs_verbs::with_writer`], whose check runs at every slice, so an
/// attachment that ends meanwhile stops it), by the compare-back a Shards guest's stores take
/// (`slates_bridge_core::dax_backing::write_back_slice`). A copy detached from its file is never compared back. When
/// anything was written, the shard publishes before the write-back answers, as a file verb does.
struct Sync {
  /// The client that holds the map.
  client_id: u32,
  /// The map: its copy, and its volume and attachment.
  held: HeldMap,
  /// The client's principal, which the attachment's check needs.
  principal: slates_db::catalog::Principal,
  /// The file the copy is of, and the copy's size, read when the write-back began.
  inode: u64,
  object_bytes: u64,
  /// The file's length at the first slice: the comparison stops there, within the copy.
  file_len: Option<u64>,
  /// The next byte to compare.
  next: u64,
  /// The bytes written so far.
  written: u64,
  /// The copy's bytes and the file's for the slice being compared, kept across slices.
  stored: Vec<u8>,
  current: Vec<u8>,
}

impl Sync {
  /// A write-back of `held`, which `client_id` holds under `principal`. `NotFound` for a read-only map or a copy that
  /// is gone.
  fn new(
    state: &ShardState,
    client_id: u32,
    held: HeldMap,
    principal: slates_db::catalog::Principal,
  ) -> Result<Sync, Refusal> {
    if held.writer.is_none() {
      return Err(Refusal::NotFound);
    }
    let copy = state
      .store
      .shards_backings
      .get(held.backing)
      .ok_or(Refusal::NotFound)?;
    Ok(Sync {
      client_id,
      held,
      principal,
      inode: copy.inode().0,
      object_bytes: copy.object().len(),
      file_len: None,
      next: 0,
      written: 0,
      stored: Vec::new(),
      current: Vec::new(),
    })
  }

  /// Runs the write-back to its end, a slice a turn: what it did, or why not, counted. `None` when the shard's state
  /// is unreachable (its shutdown).
  async fn run(&mut self) -> Option<Result<Synced, Refusal>> {
    let outcome = loop {
      match crate::state::with_state(|s| self.slice(s))? {
        Ok(true) => break crate::state::with_state(|s| self.finish(s))?,
        Ok(false) => slates_rt::futures::yield_now().await,
        Err(refusal) => break Err(refusal),
      }
    };
    crate::state::with_state(|s| match &outcome {
      Ok(synced) => {
        s.count(SYNCED, 1);
        if synced.written > 0 {
          s.count(SYNCED_BYTES, synced.written);
        }
      }
      Err(_) => s.count(SYNC_REFUSED, 1),
    });
    Some(outcome)
  }

  /// One slice: up to the shard's copy slice of the copy compared with the file and its differing runs written back.
  /// Whether the comparison has reached its end.
  fn slice(&mut self, state: &mut ShardState) -> Result<bool, Refusal> {
    let (volume, attachment) = self.held.writer.ok_or(Refusal::NotFound)?;
    let slice_bytes = state.config.copy_slice_page_bytes();
    let backing = self.held.backing;
    let Sync {
      principal,
      inode,
      object_bytes,
      file_len,
      next,
      written,
      stored,
      current,
      ..
    } = self;
    crate::fs_verbs::with_writer(
      state,
      principal,
      (volume, attachment),
      "map",
      |bridge, cx| {
        // A copy detached from its file (the volume destroyed, the file gone for good) is never compared back: its
        // inode number may name another file by now.
        if !bridge
          .dax_state()
          .is_some_and(|dax| dax.backings.is_attached(backing))
        {
          return Err(Refusal::NotFound);
        }
        let len = match *file_len {
          Some(len) => len,
          None => {
            let len = bridge
              .getattr(slates_bridge_core::ObjectId::new(*inode, 0), cx)
              .map_err(|e| crate::error::refusal_of_vfs(&e))?
              .size;
            *file_len = Some(len);
            len
          }
        };
        let end = len.min(*object_bytes);
        if *next >= end {
          return Ok(true);
        }
        let want = end.saturating_sub(*next).min(slice_bytes);
        let wrote = slates_bridge_core::dax_backing::write_back_slice(
          bridge,
          cx,
          (backing, *inode),
          (*next, want),
          (stored, current),
        )
        .map_err(|failure| refusal_of_failure(&failure))?;
        *written = written.saturating_add(wrote.bytes);
        *next = next.saturating_add(want);
        Ok(*next >= end)
      },
    )?
  }

  /// The write-back's end: when anything was written, the shard publishes, and a publication that did not capture the
  /// volume refuses, as a file verb's does.
  fn finish(&self, state: &mut ShardState) -> Result<Synced, Refusal> {
    let (volume, _) = self.held.writer.ok_or(Refusal::NotFound)?;
    if self.written > 0 {
      let volume = crate::verbs::to_db_volume(volume);
      match crate::verbs::publish_shard(state) {
        Ok(published)
          if state.by_id.contains_key(&volume)
            && !published.captured(volume)
            && !published.destroying(volume) =>
        {
          return Err(crate::error::refusal_of_vfs(
            &slates_vfs::VfsError::RecoveryIncomplete,
          ));
        }
        Ok(_) => {}
        Err(error) => return Err(crate::error::refusal_of_vfs(&error)),
      }
    }
    Ok(Synced {
      written: self.written,
      file_len: self.file_len.unwrap_or(0),
    })
  }
}

/// The refusal a write-back's failure answers: the copy unreadable (the OS refused its view), an attachment that may not
/// write, or the volume's own refusal.
fn refusal_of_failure(failure: &slates_bridge_core::dax_backing::Failure) -> Refusal {
  match failure {
    slates_bridge_core::dax_backing::Failure::Object => Refusal::ContentUnavailable,
    slates_bridge_core::dax_backing::Failure::ReadOnly => Refusal::Forbidden {
      verb: "map".to_owned(),
    },
    slates_bridge_core::dax_backing::Failure::Volume(error) => crate::error::refusal_of_vfs(error),
  }
}

/// Defers the current request's reply to `task`, which answers it through [`complete`] and delivers it to its route,
/// this shard's ring or its client's shard's (A-188): the request becomes in flight, so a retry joins it. Refused
/// `MapUnavailable` for a request with no route, one forwarded from another host, whose client no copy can reach, and
/// `Overloaded` when the shard's task arena is full: nothing runs either way.
fn defer_task(
  state: &mut ShardState,
  (origin, id): (u64, RequestId),
  task: impl Future<Output = ()> + 'static,
) -> Result<(), Refusal> {
  let route = state.reply_route.take();
  if route.is_none() {
    return Err(Refusal::MapUnavailable);
  }
  slates_rt::futures::spawn(task)
    .and_then(slates_rt::futures::detach)
    .map_err(|_| Refusal::Overloaded { shard: state.shard })?;
  state
    .direct_map
    .in_flight
    .insert((origin, id.client, id.sequence), route);
  state.acceptance_deferred = true;
  Ok(())
}

/// Records a deferred reply's completion as one transaction, in the form the record keeps ([`recorded_form`]), and
/// returns the reply with where it goes. A record the database refuses leaves nothing durable: counted, and the
/// refusal is the reply, so a retry runs the verb again.
fn complete(
  state: &mut ShardState,
  (origin, id): (u64, RequestId),
  reply: ReplyBody,
) -> (ReplyBody, Option<ReplyRoute>) {
  let route = state
    .direct_map
    .in_flight
    .remove(&(origin, id.client, id.sequence))
    .flatten();
  state.db.begin();
  let kept = recorded_form(&reply);
  let recorded = crate::verbs::record_completion(state, origin, id, kept.clone());
  let reply = match state.db.commit(&mut state.segment) {
    // The record holds the reply as it keeps it, so the client gets it whole, a Windows handle's value included.
    Ok(_) if recorded == kept => reply,
    Ok(_) => recorded,
    Err(e) => {
      let refusal = crate::error::refusal_of_db(&e);
      state.count(crate::verbs::refusal_name(&refusal), 1);
      crate::verbs::reconcile_unpublished_effects(state);
      refused(refusal)
    }
  };
  (reply, route)
}

/// The direct maps on this shard (§4.6, A-171, A-188): those being filled, and every copy held here, by its client.
#[derive(Default)]
pub(crate) struct DirectMapState {
  /// The requests a fill runs for, by origin, client and sequence, with where its reply goes: the newest attempt's
  /// route, since a retry comes from where its client now waits. One entry per task, so bounded by the shard's task
  /// arena.
  pub(crate) in_flight: BTreeMap<(u64, u32, u32), Option<ReplyRoute>>,
  /// The copies held on this shard, by client id: a map is held where its copy is, on its volume's shard, whichever
  /// shard serves its client (A-188). A client is here only while it holds a copy, so the table is bounded by the
  /// shard's copies (`direct_maps_per_shard`).
  pub(crate) held: BTreeMap<u32, ClientMaps>,
}

/// A retry of a map still being filled joins it: the verb does not run again, and the reply goes where this attempt
/// came from. Whether the request was running.
pub(crate) fn join_in_flight(state: &mut ShardState, origin: u64, id: RequestId) -> bool {
  let Some(route) = state
    .direct_map
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

/// `MapRead` where no route hands a client its copy: no platform slates ships, since Linux, macOS and Windows each
/// have one. The client reads by its paged path, which applies the rights.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn map_read(
  _state: &mut ShardState,
  _client_id: u32,
  _principal: &slates_db::catalog::Principal,
  _volume: slates_ipc::protocol::VolumeId,
  _path: &str,
  _at: slates_ipc::protocol::ReadAt,
) -> slates_ipc::protocol::ReplyBody {
  crate::verbs::refused(slates_ipc::protocol::Refusal::MapUnavailable)
}

/// `MapWrite` where no route hands a client its copy: no platform slates ships. Refused `MapUnavailable`, as a
/// writable map has no paged fallback.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn map_write(
  _state: &mut ShardState,
  _client_id: u32,
  _principal: &slates_db::catalog::Principal,
  _target: (VolumeId, u64),
  _path: &str,
) -> ReplyBody {
  refused(Refusal::MapUnavailable)
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) use served::{map_read, map_write};

/// The platforms that hand a client its copy: Linux over the client's control socket, macOS over the socket route,
/// Windows by duplicating a read-only handle into the client's process. The fill is one; the hand-off is a paired
/// function.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod served {
  use slates_db::catalog::{Principal, Role};
  use slates_ipc::protocol::{ReadAt, Refusal, ReplyBody, VolumeId};
  use slates_mem::{Handle, PeerAccess, PeerObject};
  use slates_vfs::dir::Child;
  use slates_vfs::ids::InodeNo;
  use slates_vfs::shards_backing::{PeerRefusal, ShardsBacking, ShardsBackings};
  use slates_vfs::volume::Seek;
  use slates_wire::request::RequestId;

  #[cfg(target_os = "linux")]
  use std::os::fd::OwnedFd;

  use crate::error::refusal_of_vfs;
  use crate::merge_service::{canonical_path, find_record};
  use crate::state::{ShardState, VolumeSlot};
  use crate::verbs::{find, forbidden, refused, rights_of};

  /// Counter: a direct map handed to its client, the zero-copy path's non-vacuity counter (§4.6, A-171): a test asserts
  /// it moved, so a silently dead map path cannot pass as the copy path.
  const HANDED: &str = "dax.local_map";
  /// Counter: a direct map whose gates passed but whose copy was not handed: the fill budget, the backings' capacity or
  /// the OS refused it, the volume or the file went during the fill, or the client went or its channel refused. Its
  /// client reads by copy.
  const FELL_BACK: &str = "dax.local_map_fallback";
  /// Counter: a descriptor the hand-off judged unconfined (`IpcError::UnconfinedDescriptor`): a copy that is not
  /// sealed or not the size it claims, which must never happen. Its client reads by copy.
  const UNCONFINED: &str = "dax.local_map_unconfined";

  /// Counter: a `MapRead` refused because its client holds its share of the direct maps already (`MapShareExhausted`).
  const SHARE_REFUSED: &str = "dax.local_map_share_refused";

  /// `MapRead` (§4.6, A-171): checks the uid, the view, the volume, the right, the client's channel and the path at
  /// once, makes the file's copy, and defers the reply to the task that fills and hands it over. `MapUnavailable` for
  /// every shape the direct map does not serve, so the client reads by copy.
  pub(crate) fn map_read(
    state: &mut ShardState,
    client_id: u32,
    principal: &Principal,
    volume: VolumeId,
    path: &str,
    at: ReadAt,
  ) -> ReplyBody {
    if !is_the_daemons_user(principal) || !matches!(at, ReadAt::Head) {
      return refused(Refusal::MapUnavailable);
    }
    let record = match find_record(state, volume) {
      Ok(record) => record,
      Err(refusal) => return refused(refusal),
    };
    if !rights_of(&record, principal).read {
      return forbidden("read");
    }
    if !matches!(record.policy.role, Role::Plain) {
      return refused(Refusal::MapUnavailable);
    }
    let (handle, _) = match find(state, volume) {
      Ok(found) => found,
      Err(reply) => return *reply,
    };
    let Some(request) = state.current_request else {
      return refused(Refusal::Unsupported {
        feature: "a direct map outside a recorded request".to_owned(),
      });
    };
    let target = Target::Path(canonical_path(path));
    match Fill::begin(state, handle, (client_id, principal), target, request, None) {
      Ok(fill) => defer(state, fill),
      Err(Begun::Directory) => refused(Refusal::BadRequest {
        reason: "the path is a directory".to_owned(),
      }),
      Err(Begun::Vfs(error)) => refused(refusal_of_vfs(&error)),
      Err(Begun::Unavailable) => refused(Refusal::MapUnavailable),
      Err(Begun::FellBack) => {
        state.count(FELL_BACK, 1);
        refused(Refusal::MapUnavailable)
      }
      Err(Begun::ShareExhausted(refusal)) => {
        state.count(SHARE_REFUSED, 1);
        refused(refusal)
      }
    }
  }

  /// `MapWrite` (§4.6, A-186): checks the uid and the client's channel at once, resolves the file through the caller's
  /// write attachment as a file verb does (`crate::fs_verbs::writable_file`: the attachment the caller's own, of this
  /// volume, for writing, not a snapshot's, its scope applied), makes the file's writable copy, and defers the reply to
  /// the task that fills and hands it over, as `MapRead`'s. `MapUnavailable` for every shape the direct map does not
  /// serve, and for a file an in-process guest maps writable; `Forbidden` or `NotFound` as the attachment's check
  /// answers.
  pub(crate) fn map_write(
    state: &mut ShardState,
    client_id: u32,
    principal: &Principal,
    (volume, attachment): (VolumeId, u64),
    path: &str,
  ) -> ReplyBody {
    if !is_the_daemons_user(principal) {
      return refused(Refusal::MapUnavailable);
    }
    let record = match find_record(state, volume) {
      Ok(record) => record,
      Err(refusal) => return refused(refusal),
    };
    if !matches!(record.policy.role, Role::Plain) {
      return refused(Refusal::MapUnavailable);
    }
    let (handle, _) = match find(state, volume) {
      Ok(found) => found,
      Err(reply) => return *reply,
    };
    let inode = match crate::fs_verbs::writable_file(state, principal, (volume, attachment), path) {
      Ok(inode) => InodeNo(inode),
      Err(refusal) => return refused(refusal),
    };
    // An in-process guest storing into the file's own block bypasses the write boundary, so no copy of the file could
    // be kept equal to it, nor compared back against it: the two exclude each other, as a Shards backing and such a
    // window do (A-163; `Volume::dax_map_write` refuses the other way round).
    if state.store.dax.has_writable_of(inode) {
      return refused(Refusal::MapUnavailable);
    }
    let Some(request) = state.current_request else {
      return refused(Refusal::Unsupported {
        feature: "a direct map outside a recorded request".to_owned(),
      });
    };
    match Fill::begin(
      state,
      handle,
      (client_id, principal),
      Target::Inode(inode),
      request,
      Some((volume, attachment)),
    ) {
      Ok(fill) => defer(state, fill),
      Err(Begun::Directory) => refused(Refusal::BadRequest {
        reason: "the path is a directory".to_owned(),
      }),
      Err(Begun::Vfs(error)) => refused(refusal_of_vfs(&error)),
      Err(Begun::Unavailable) => refused(Refusal::MapUnavailable),
      Err(Begun::FellBack) => {
        state.count(FELL_BACK, 1);
        refused(Refusal::MapUnavailable)
      }
      Err(Begun::ShareExhausted(refusal)) => {
        state.count(SHARE_REFUSED, 1);
        refused(refusal)
      }
    }
  }

  /// The file a direct map copies: a path from the volume's root, resolved here (a read map, which the volume's read
  /// right covers whole), or an inode a writable map's attachment resolved already, its scope applied.
  enum Target<'p> {
    /// A canonical path from the volume's root.
    Path(&'p str),
    /// A regular file's inode.
    Inode(InodeNo),
  }

  /// Why a direct map did not begin.
  enum Begun {
    /// The path is a directory.
    Directory,
    /// The path does not resolve, or its file cannot be read.
    Vfs(slates_vfs::error::VfsError),
    /// A shape the direct map does not serve: an overlay, or a client served here that no descriptor can reach.
    Unavailable,
    /// The copy could not be made: the budget, the backings' capacity or the OS refused it.
    FellBack,
    /// The client holds its share of the direct maps already: the refusal that says so.
    ShareExhausted(Refusal),
  }

  /// A direct map being filled: its copy, where the fill is, and what the hand-off and the reply need.
  struct Fill {
    /// What the client may do with the copy: read it, or write it too (A-186).
    access: PeerAccess,
    /// The volume's slot, which a destroy meanwhile makes stale.
    volume: Handle<VolumeSlot>,
    /// The copy's backing, until the hand-off gives it to the client; given back when the fill ends without one.
    backing: Option<Handle<ShardsBacking>>,
    inode: InodeNo,
    /// The copy's size, fixed when it was made.
    object_bytes: u64,
    /// The next byte of the copy to fill.
    next: u64,
    client_id: u32,
    request: (u64, RequestId),
  }

  impl Drop for Fill {
    fn drop(&mut self) {
      if let Some(backing) = self.backing.take() {
        // A fill that ended without its hand-off: the copy goes back, and off its client's maps. With the state
        // unreachable (a shutdown) it cannot, and the process's end returns it.
        let client_id = self.client_id;
        let _released = crate::state::with_state(|s| super::give_back(s, client_id, backing));
      }
    }
  }

  impl Fill {
    /// Resolves the target, checks the client's share, makes the file's copy (writable where `writer` names the
    /// attachment its stores are written back under, A-186) and keeps it in the store's backings, held from now on in
    /// this shard's table under the client, whichever shard serves it (A-188), so the share counts copies still being
    /// filled as well as those handed.
    fn begin(
      state: &mut ShardState,
      volume: Handle<VolumeSlot>,
      (client_id, principal): (u32, &Principal),
      target: Target<'_>,
      request: (u64, RequestId),
      writer: Option<(VolumeId, u64)>,
    ) -> Result<Fill, Begun> {
      let access = match writer {
        Some(_) => PeerAccess::ReadWrite,
        None => PeerAccess::Read,
      };
      let name = format!("slates-map-{}", state.config.instance);
      let most_copies = state.config.direct_maps_per_shard;
      let ShardState {
        volumes,
        store,
        clients,
        direct_map,
        ..
      } = state;
      // A client served here must be one a descriptor can reach; one another shard serves is checked at the hand-off,
      // where its own shard answers (A-188).
      let here = clients
        .iter()
        .map(|(_, client)| client)
        .find(|client| client.client_id == client_id);
      if here.is_some_and(|client| client.retiring || !can_be_handed_a_descriptor(client)) {
        return Err(Begun::Unavailable);
      }
      // The clients a share is counted over: those this shard serves, those holding its copies, and this one.
      let mut counted: std::collections::BTreeSet<u32> = clients
        .iter()
        .filter(|(_, client)| !client.retiring)
        .map(|(_, client)| client.client_id)
        .collect();
      counted.extend(direct_map.held.keys().copied());
      counted.insert(client_id);
      let share = super::share_of((most_copies, store.shards_backings.budget()), counted.len());
      let slot = volumes.get(volume).map_err(|_| Begun::Unavailable)?;
      // An overlay's untouched file follows the disk, which no write boundary sees.
      if slot.host.is_some() {
        return Err(Begun::Unavailable);
      }
      let inode = match target {
        Target::Path(key) => {
          let located = slot.volume.resolve(store, key).map_err(Begun::Vfs)?;
          if matches!(located.child, Child::Dir(_)) {
            return Err(Begun::Directory);
          }
          located.inode
        }
        Target::Inode(inode) => inode,
      };
      let size = slot.volume.stat(store, inode).map_err(Begun::Vfs)?.size;
      let object_bytes = PeerObject::bytes_for(size).map_err(|_| Begun::FellBack)?;
      let held = holds(
        direct_map
          .held
          .get_mut(&client_id)
          .map(|client| &mut client.maps),
        &store.shards_backings,
      );
      super::within_share(held, object_bytes, share).map_err(Begun::ShareExhausted)?;
      // Within its share, but the shard holds every copy the process's limits leave room for (others mapped before
      // this client came): this client reads by copy until some are released.
      if store.shards_backings.peers_held().0 >= most_copies {
        return Err(Begun::FellBack);
      }
      let object = PeerObject::create(&name, size, access).map_err(|_| Begun::FellBack)?;
      let backing = store
        .shards_backings
        .insert_peer(object, slot.volume.prefix(), inode)
        .map_err(|refusal| match refusal {
          PeerRefusal::Budget { .. } | PeerRefusal::Full(_) => Begun::FellBack,
        })?;
      direct_map
        .held
        .entry(client_id)
        .or_insert_with(|| super::ClientMaps {
          principal: principal.clone(),
          maps: Vec::new(),
        })
        .maps
        .push(super::HeldMap { backing, writer });
      Ok(Fill {
        access,
        volume,
        backing: Some(backing),
        inode,
        object_bytes,
        next: 0,
        client_id,
        request,
      })
    }

    /// One slice of the fill: up to the shard's copy slice of the file's bytes stored into the copy, the ranges it
    /// touches marked filled first. Whether the copy is whole.
    fn slice(&mut self, state: &mut ShardState) -> Result<bool, Refusal> {
      if self.next >= self.object_bytes {
        return Ok(true);
      }
      let take = state
        .config
        .copy_slice_page_bytes()
        .min(self.object_bytes.saturating_sub(self.next));
      let backing = self.backing.ok_or(Refusal::NotFound)?;
      let ShardState { volumes, store, .. } = state;
      let slot = volumes.get(self.volume).map_err(|_| Refusal::NotFound)?;
      let end = self.next.saturating_add(take);
      // Filled from the start of the slice, so a change written before the slice ends, or between slices, lands here.
      let target = store
        .shards_backings
        .get_mut(backing)
        .ok_or(Refusal::NotFound)?;
      let first = self
        .next
        .checked_div(slates_mem::DAX_RANGE_BYTES)
        .unwrap_or(0);
      let last = end
        .saturating_sub(1)
        .checked_div(slates_mem::DAX_RANGE_BYTES)
        .unwrap_or(0);
      for range in first..=last {
        target.mark_filled(range);
      }
      let size = slot
        .volume
        .stat(store, self.inode)
        .map_err(|error| refusal_of_vfs(&error))?
        .size;
      // A hole needs no fill: the copy reads zero there already. A base-backed body counts as data whole.
      let data = slot
        .volume
        .seek(store, self.inode, self.next, Seek::Data)
        .map_err(|error| refusal_of_vfs(&error))?;
      let from = match data {
        Some(data) if data < end.min(size) => data,
        _ => {
          self.next = end;
          return Ok(self.next >= self.object_bytes);
        }
      };
      let stop = end.min(size);
      let mut bytes = vec![0u8; usize::try_from(stop.saturating_sub(from)).unwrap_or(0)];
      let read = slot
        .volume
        .read(store, self.inode, from, &mut bytes)
        .map_err(|error| refusal_of_vfs(&error))?;
      store_pages(
        &mut store.shards_backings,
        backing,
        from,
        bytes.get(..read).unwrap_or_default(),
      )?;
      self.next = end;
      Ok(self.next >= self.object_bytes)
    }

    /// The shard that serves this fill's client: where its reply goes, the newest attempt's route (A-188). macOS offers
    /// the copy on this shard instead, where any client claims it.
    #[cfg(any(target_os = "linux", windows))]
    fn home(&self, state: &ShardState) -> Option<u16> {
      let (origin, id) = self.request;
      state
        .direct_map
        .in_flight
        .get(&(origin, id.client, id.sequence))
        .copied()
        .flatten()
        .map(|route| route.shard)
    }

    /// What the hand-off sends the client's shard (Linux): this shard, the client's shard, a duplicate of the copy's
    /// descriptor, and the mapping it carries. `None` where the volume, the file or the copy went, the mapping id does not
    /// fit, the descriptor cannot be duplicated or the request has no route.
    #[cfg(target_os = "linux")]
    fn prepare(
      &self,
      state: &ShardState,
    ) -> Option<(u16, u16, OwnedFd, slates_ipc::rendezvous::Mapping)> {
      let backing = self.backing?;
      let home = self.home(state)?;
      let file_len = state
        .volumes
        .get(self.volume)
        .ok()
        .and_then(|slot| slot.volume.stat(&state.store, self.inode).ok())?
        .size;
      let mapping_id = super::mapping_id_of(state.partition, backing)?;
      let descriptor = state
        .store
        .shards_backings
        .get(backing)?
        .object()
        .descriptor()
        .try_clone_to_owned()
        .ok()?;
      let mapping = slates_ipc::rendezvous::Mapping {
        request_id: mapping_id,
        mapping_id,
        region_off: 0,
        file_len,
        block_len: self.object_bytes,
        access: self.access,
      };
      Some((state.shard, home, descriptor, mapping))
    }

    /// The reply a hand-off came to (Linux): `Mapped` once the client's shard sent the copy, the copy then the client's
    /// alone to let go; `MapUnavailable` (counted) where it did not, or the call did not reach that shard.
    #[cfg(target_os = "linux")]
    fn handed(
      &mut self,
      state: &mut ShardState,
      delivered: Option<Delivered>,
      mapping: slates_ipc::rendezvous::Mapping,
    ) -> ReplyBody {
      match delivered {
        Some(Delivered::Sent) => {
          // Held in the table since the copy was made: the fill lets go of it no more.
          self.backing = None;
          state.count(HANDED, 1);
          ReplyBody::Mapped {
            mapping_id: mapping.mapping_id,
            region_off: 0,
            file_len: mapping.file_len,
            block_len: mapping.block_len,
          }
        }
        Some(Delivered::Unconfined) => {
          state.count(UNCONFINED, 1);
          state.count(FELL_BACK, 1);
          refused(Refusal::MapUnavailable)
        }
        Some(Delivered::Refused) | None => {
          state.count(FELL_BACK, 1);
          refused(Refusal::MapUnavailable)
        }
      }
    }

    /// Offers the filled copy's descriptor on the socket route (macOS), judged first to authorize the copy alone in the
    /// map's access (`slates_ipc::route::confined_map`), behind a fresh single-use secret the reply carries; the
    /// backing held on the client's slot since the copy was made. The reply; `MapUnavailable` (counted) where the
    /// volume, the file or the copy went, the mapping id does not fit, the descriptor is unconfined, no secret can be
    /// drawn or no offer kept.
    #[cfg(target_os = "macos")]
    fn offer(&mut self, state: &mut ShardState) -> ReplyBody {
      use std::os::fd::AsFd;

      let Some(backing) = self.backing else {
        return refused(Refusal::MapUnavailable);
      };
      // The client may be served by any shard (A-188): it claims the offer by the mapping id, whose high bits name this
      // one. An offer it never claims expires, and its reap lets the copy go before then.
      let size = state
        .volumes
        .get(self.volume)
        .ok()
        .and_then(|slot| slot.volume.stat(&state.store, self.inode).ok())
        .map(|attrs| attrs.size);
      let descriptor = state
        .store
        .shards_backings
        .get(backing)
        .and_then(|copy| copy.object().descriptor().try_clone_to_owned().ok());
      let (Some(file_len), Some(descriptor), Some(mapping_id)) = (
        size,
        descriptor,
        super::mapping_id_of(state.partition, backing),
      ) else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
      if slates_ipc::route::confined_map(descriptor.as_fd(), self.object_bytes, self.access)
        .is_err()
      {
        state.count(UNCONFINED, 1);
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      }
      let Ok(secret) = slates_ipc::route::random::<{ slates_ipc::route::SECRET_BYTES }>() else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
      let key = (slates_ipc::route::OfferKind::Map, mapping_id);
      if !crate::shards_route::offer(state, key, secret, descriptor) {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      }
      // Held in the table since the copy was made: the fill lets go of it no more.
      self.backing = None;
      state.count(HANDED, 1);
      ReplyBody::MapOffered {
        mapping_id,
        file_len,
        block_len: self.object_bytes,
        secret,
      }
    }

    /// Duplicates the filled copy's handle, read-only or for a writable map read and write, into the client's process
    /// (Windows), judged first to authorize the copy alone in that access and the process to run as the daemon's user
    /// (`slates_ipc::handle_route::hand_section`); the backing held in the table since the copy was made. `pid` is the
    /// client's process, as the shard that serves the client named it (A-188). The reply names the handle's value there.
    /// `MapUnavailable` (counted) where the volume, the file or the copy went, the client went, the mapping id does not
    /// fit, the handle is unconfined, or the client's process is not the daemon's user's or refused the duplicate.
    #[cfg(windows)]
    fn duplicate(&mut self, state: &mut ShardState, pid: Option<u32>) -> ReplyBody {
      use std::os::windows::io::AsHandle;

      let Some(backing) = self.backing else {
        return refused(Refusal::MapUnavailable);
      };
      let size = state
        .volumes
        .get(self.volume)
        .ok()
        .and_then(|slot| slot.volume.stat(&state.store, self.inode).ok())
        .map(|attrs| attrs.size);
      let section = state
        .store
        .shards_backings
        .get(backing)
        .and_then(|copy| copy.object().peer_handle().ok());
      let (Some(pid), Some(file_len), Some(section), Some(mapping_id)) = (
        pid,
        size,
        section,
        super::mapping_id_of(state.partition, backing),
      ) else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
      match slates_ipc::handle_route::hand_section(
        section.as_handle(),
        self.object_bytes,
        pid,
        self.access,
      ) {
        Ok(handle) => {
          // Held in the table since the copy was made: the fill lets go of it no more.
          self.backing = None;
          state.count(HANDED, 1);
          ReplyBody::MapHanded {
            mapping_id,
            file_len,
            block_len: self.object_bytes,
            handle,
          }
        }
        Err(refusal) => {
          if matches!(refusal, slates_ipc::IpcError::UnconfinedDescriptor { .. }) {
            state.count(UNCONFINED, 1);
          }
          state.count(FELL_BACK, 1);
          refused(Refusal::MapUnavailable)
        }
      }
    }
  }

  /// What the client's shard did with a copy's descriptor (Linux, A-188).
  #[cfg(target_os = "linux")]
  enum Delivered {
    /// Sent over the client's control socket.
    Sent,
    /// Refused unconfined by the hand-off's own judge (`slates_ipc::rendezvous::Control::send_mapping`).
    Unconfined,
    /// Not sent: the client is gone or retiring, has no control socket, or the socket refused.
    Refused,
  }

  /// On the client's shard: sends client `client_id` the copy's `descriptor` with `mapping` over its control socket, the
  /// delivery sealed under the client's own channel (A-187), which is why it is sent here and not from the copy's shard.
  #[cfg(target_os = "linux")]
  fn send_to_client(
    state: &mut ShardState,
    client_id: u32,
    descriptor: &OwnedFd,
    mapping: slates_ipc::rendezvous::Mapping,
  ) -> Delivered {
    use std::os::fd::AsFd;
    let control = state
      .clients
      .iter()
      .map(|(_, client)| client)
      .find(|client| client.client_id == client_id && !client.retiring)
      .and_then(|client| client.control.as_ref());
    let Some(control) = control else {
      return Delivered::Refused;
    };
    match control.send_mapping(descriptor.as_fd(), mapping) {
      Ok(()) => Delivered::Sent,
      Err(slates_ipc::error::IpcError::UnconfinedDescriptor { .. }) => Delivered::Unconfined,
      Err(_) => Delivered::Refused,
    }
  }

  /// Hands the filled copy to its client over the client's control socket (Linux), from the shard that serves the
  /// client, asked by a cross-shard call, this one's own included (A-188). The reply; `MapUnavailable` (counted) where
  /// the volume, the file or the client went, the mapping id does not fit, or the channel refused.
  #[cfg(target_os = "linux")]
  async fn hand_off(fill: &mut Fill) -> ReplyBody {
    let client_id = fill.client_id;
    let Some(Some((here, home, descriptor, mapping))) =
      crate::state::with_state(|s| fill.prepare(s))
    else {
      let _counted = crate::state::with_state(|s| s.count(FELL_BACK, 1));
      return refused(Refusal::MapUnavailable);
    };
    let delivered = crate::xshard::call_within(
      here,
      home,
      move |s| send_to_client(s, client_id, &descriptor, mapping),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    crate::state::with_state(|s| fill.handed(s, delivered, mapping))
      .unwrap_or_else(|| refused(Refusal::MapUnavailable))
  }

  /// Offers the filled copy on the socket route (macOS), on this shard, whichever shard serves the client (A-188).
  #[cfg(target_os = "macos")]
  async fn hand_off(fill: &mut Fill) -> ReplyBody {
    crate::state::with_state(|s| fill.offer(s)).unwrap_or_else(|| refused(Refusal::MapUnavailable))
  }

  /// On the client's shard: client `client_id`'s process, which its claim named; `None` where it is gone or retiring, or
  /// named none.
  #[cfg(windows)]
  fn pid_of(state: &mut ShardState, client_id: u32) -> Option<u32> {
    state
      .clients
      .iter()
      .map(|(_, client)| client)
      .find(|client| client.client_id == client_id && !client.retiring && client.pid != 0)
      .map(|client| client.pid)
  }

  /// Duplicates the filled copy's handle into its client's process (Windows), the process named by the shard that
  /// serves the client, asked by a cross-shard call, this one's own included (A-188).
  #[cfg(windows)]
  async fn hand_off(fill: &mut Fill) -> ReplyBody {
    let client_id = fill.client_id;
    let Some((here, Some(home))) = crate::state::with_state(|s| (s.shard, fill.home(s))) else {
      let _counted = crate::state::with_state(|s| s.count(FELL_BACK, 1));
      return refused(Refusal::MapUnavailable);
    };
    let pid = crate::xshard::call_within(
      here,
      home,
      move |s| pid_of(s, client_id),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await
    .flatten();
    crate::state::with_state(|s| fill.duplicate(s, pid))
      .unwrap_or_else(|| refused(Refusal::MapUnavailable))
  }

  /// Whether `principal` is the daemon's own user, the only one a copy is handed to (§4.6, A-135's same-uid rule): on
  /// Unix the peer's uid against the daemon's.
  #[cfg(unix)]
  fn is_the_daemons_user(principal: &Principal) -> bool {
    matches!(principal, Principal::Uid { uid } if *uid == rustix::process::geteuid().as_raw())
  }

  /// Whether `principal` may be handed a copy on Windows: a client's own principal (no enrolled consumer), whose
  /// rendezvous carries no user, so the hand-off checks the client process's token against the daemon's instead
  /// (`slates_ipc::handle_route::same_user`).
  #[cfg(windows)]
  fn is_the_daemons_user(principal: &Principal) -> bool {
    matches!(principal, Principal::Uid { .. })
  }

  /// Whether `client` can be handed a copy's descriptor: on Linux, over its control socket, which it must hold.
  #[cfg(target_os = "linux")]
  fn can_be_handed_a_descriptor(client: &crate::state::ClientSlot) -> bool {
    client.control.is_some()
  }

  /// Whether `client` can be handed a copy's descriptor: on macOS any client can, since it claims it on the socket
  /// route, which needs no channel of its own.
  #[cfg(target_os = "macos")]
  fn can_be_handed_a_descriptor(_client: &crate::state::ClientSlot) -> bool {
    true
  }

  /// Whether `client` can be handed a copy's handle: on Windows, by duplication into its process, which its claim named.
  #[cfg(windows)]
  fn can_be_handed_a_descriptor(client: &crate::state::ClientSlot) -> bool {
    client.pid != 0
  }

  /// What a client holds of the direct maps on this shard, `(maps, bytes)`: `held`, its copies handed or being filled,
  /// less those the store let go of (a fill that ended early), which are forgotten here; none where it holds none.
  fn holds(held: Option<&mut Vec<super::HeldMap>>, backings: &ShardsBackings) -> (usize, u64) {
    let Some(held) = held else {
      return (0, 0);
    };
    held.retain(|map| backings.peer_bytes(map.backing).is_some());
    let bytes = held
      .iter()
      .filter_map(|map| backings.peer_bytes(map.backing))
      .fold(0u64, u64::saturating_add);
    (held.len(), bytes)
  }

  /// Stores the pages of `bytes`, the file's from `at`, into the copy `backing` names, passing over pages of zeros: the
  /// new copy reads zero there already, and a page never stored holds no memory.
  fn store_pages(
    backings: &mut ShardsBackings,
    backing: Handle<ShardsBacking>,
    at: u64,
    bytes: &[u8],
  ) -> Result<(), Refusal> {
    // A page: the object a one-byte file is copied into.
    let page_len = PeerObject::bytes_for(1).map_err(|_| Refusal::MapUnavailable)?;
    let page = usize::try_from(page_len).unwrap_or(usize::MAX).max(1);
    let target = backings.get_mut(backing).ok_or(Refusal::NotFound)?;
    let mut offset = at;
    for chunk in bytes.chunks(page) {
      if !chunk.iter().all(|byte| *byte == 0) {
        target
          .object_mut()
          .store(offset, chunk)
          .map_err(|_| Refusal::MapUnavailable)?;
      }
      offset = offset.saturating_add(page_len);
    }
    Ok(())
  }

  /// Defers the current request's reply to the task that fills and hands over `fill` ([`super::defer_task`]). Refused
  /// as that refuses, the copy let go: the task was never spawned, or was dropped inside this borrow of the shard's
  /// state, where the fill's own drop cannot reach the store.
  fn defer(state: &mut ShardState, fill: Fill) -> ReplyBody {
    let request = fill.request;
    let backing = fill.backing;
    let client_id = fill.client_id;
    match super::defer_task(state, request, run(fill)) {
      // Never seen: `run_recorded` sends nothing for a verb that deferred its acceptance.
      Ok(()) => ReplyBody::Acknowledged,
      Err(refusal) => {
        if let Some(backing) = backing {
          let _released = super::give_back(state, client_id, backing);
        }
        if matches!(refusal, Refusal::MapUnavailable) {
          state.count(FELL_BACK, 1);
        }
        refused(refusal)
      }
    }
  }

  /// Fills the copy a slice at a time, yielding between slices; then hands it over, records the reply's completion
  /// and delivers it to its route: this shard's ring, or the client's shard's (A-188).
  async fn run(mut fill: Fill) {
    let filled = loop {
      match crate::state::with_state(|s| fill.slice(s)) {
        Some(Ok(true)) => break Ok(()),
        Some(Ok(false)) => slates_rt::futures::yield_now().await,
        Some(Err(refusal)) => break Err(refusal),
        // The shard's state is unreachable (its shutdown): no reply can be recorded, and the client's retry, on the
        // shard that succeeds this one, runs the verb again.
        None => return,
      }
    };
    let request = fill.request;
    let reply = match filled {
      Ok(()) => hand_off(&mut fill).await,
      Err(_) => {
        let _counted = crate::state::with_state(|s| s.count(FELL_BACK, 1));
        refused(Refusal::MapUnavailable)
      }
    };
    let answered = crate::state::with_state(|s| {
      let (reply, route) = super::complete(s, request, reply);
      (reply, route, s.shard)
    });
    // Delivered after the state's borrow ends, since delivery takes it again.
    if let Some((reply, Some(route), here)) = answered {
      super::deliver_to(route, reply, here).await;
    }
  }
}

/// A direct map's reply as its completion record keeps it (§4.6, A-175): a Windows hand-off without its handle's value
/// (zero), every other reply as it is. The client's process outlives the daemon that duplicated the handle into it, so a
/// reply replayed from the record after a restart must name no handle: a client adopting the old value would map a copy
/// no daemon keeps equal to the file any more. Linux's descriptor and macOS's offer never enter a reply, so theirs are
/// kept whole: replayed, their hand-off finds nothing and the map fails.
#[cfg_attr(
  not(any(target_os = "linux", target_os = "macos", windows)),
  allow(dead_code)
)]
fn recorded_form(reply: &ReplyBody) -> ReplyBody {
  match reply {
    ReplyBody::MapHanded {
      mapping_id,
      file_len,
      block_len,
      ..
    } => ReplyBody::MapHanded {
      mapping_id: *mapping_id,
      file_len: *file_len,
      block_len: *block_len,
      handle: 0,
    },
    other => other.clone(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AC-A-175: a Windows hand-off's completion is recorded without its handle, so a reply replayed after a restart
  /// names none. Do: take the record's form of a hand-off naming handle 0x1234, of a Linux map, of a macOS offer and of
  /// a refusal. Expect: the hand-off kept with every field but its handle, which is zero; the others kept whole.
  #[test]
  fn a_handed_maps_record_names_no_handle() {
    let handed = ReplyBody::MapHanded {
      mapping_id: 7,
      file_len: 5,
      block_len: 4096,
      handle: 0x1234,
    };
    assert_eq!(
      recorded_form(&handed),
      ReplyBody::MapHanded {
        mapping_id: 7,
        file_len: 5,
        block_len: 4096,
        handle: 0
      }
    );
    for whole in [
      ReplyBody::Mapped {
        mapping_id: 7,
        region_off: 0,
        file_len: 5,
        block_len: 4096,
      },
      ReplyBody::MapOffered {
        mapping_id: 7,
        file_len: 5,
        block_len: 4096,
        secret: [9; 32],
      },
      refused(Refusal::MapUnavailable),
    ] {
      assert_eq!(recorded_form(&whole), whole);
    }
  }

  /// AC-A-171 (G-direct step 2): a client's share is the shard's capacity over the clients served now. Do: share a
  /// capacity of 100 copies and 1 MiB among one, four and zero clients. Expect the whole to one client, a quarter each
  /// to four, and the whole when none is counted (a client asking is always served).
  #[test]
  fn a_clients_share_is_the_shards_capacity_over_the_clients_served() {
    let capacity = (100, 1 << 20);
    assert_eq!(share_of(capacity, 1), capacity);
    assert_eq!(share_of(capacity, 4), (25, 1 << 18));
    assert_eq!(share_of(capacity, 0), capacity);
  }

  /// AC-A-171 (G-direct step 2): a copy past a client's share is refused typed with the counts. Do: ask for copies
  /// against a share of two maps and 8 KiB: one within both, one past the count, one past the bytes, one exactly filling
  /// the bytes. Expect the first and last accepted, the others refused `MapShareExhausted` carrying what is held and the
  /// share.
  #[test]
  fn a_copy_past_a_clients_share_is_refused_with_the_counts() {
    let share = (2, 8192);
    assert!(within_share((1, 2048), 4096, share).is_ok(), "within both");
    assert_eq!(
      within_share((2, 4096), 1, share),
      Err(Refusal::MapShareExhausted {
        held_maps: 2,
        most_maps: 2,
        held_bytes: 4096,
        share_bytes: 8192
      }),
      "past the count"
    );
    assert!(
      matches!(
        within_share((1, 4097), 4096, share),
        Err(Refusal::MapShareExhausted {
          held_bytes: 4097,
          ..
        })
      ),
      "past the bytes"
    );
    assert!(
      within_share((1, 4096), 4096, share).is_ok(),
      "exactly the bytes"
    );
  }
}
