//! The same-kernel direct map (§4.6; A-135, whose mechanism A-171 replaced): a client process of the daemon's own uid
//! maps one file's bytes read alone and reads them as memory, with no request and no copy per read.
//!
//! **What the client is handed.** A descriptor carries its whole object, read and write, to whoever holds it (A-135;
//! the 2026-10-18 escape handed a peer the shard's whole content arena), so the object handed is one file's copy and
//! nothing else: a [`slates_mem::PeerObject`], the file's page span, filled from the volume and sealed against every
//! writer but the daemon's own view (`F_SEAL_FUTURE_WRITE`, `F_SEAL_SHRINK`, `F_SEAL_GROW`, `F_SEAL_SEAL`). The copy is
//! kept in the store's backings ([`slates_vfs::shards_backing`]), so the volume's single write boundary writes every
//! later change into it before the change returns: the client's map is a live view of the file within its length, as a
//! shared map of a file is. Whether the descriptor may leave is judged where every descriptor leaves the process
//! (`Control::send_mapping`, `confined_for_a_peer`): the object must be what is authorized, and sealed.
//!
//! **Running.** The verb checks what it can at once (the uid, the view, the volume, the right, the client's channel,
//! the path), makes the copy, and defers its reply to a task of this shard, as an image build does (A-160). The task
//! fills the copy a slice at a time ([`crate::config::DaemonConfig::copy_slice_page_bytes`], the slice of a DAX
//! backing's fill, A-163), yielding between slices, so the shard serves its clients while a file of any size is copied.
//! A range is marked filled before its slice reads the volume, so a change written meanwhile lands on it and the rest
//! of the fill reads the volume as changed. Holes and pages of zeros are passed over: the new copy reads zero there
//! already. The fill done, the descriptor goes over the client's control socket, then the reply goes on its ring, and
//! the reply's completion is recorded as every verb's is.
//!
//! **What reads by copy instead** (`MapUnavailable`, which the client answers with its paged read): a peer of another
//! uid, a view other than the head, a merge volume, an overlay (an untouched file follows the disk, which no write
//! boundary sees), a client served by another shard (a descriptor does not cross shards yet), a platform with no route
//! (macOS and Windows, each owed to its own step), and a copy the fill budget, the backings' capacity or the OS
//! refuses.
//!
//! **Lifetime.** The client's slot holds the copy's backing from the hand-off until the slot is reaped (`crate::reap`),
//! and a volume's teardown lets go of its files' copies (`ShardsBackings::remove_volume_peers`). A copy whose hand-off
//! did not happen (its client gone, its channel refusing, the task ended early) is let go at once.

use std::collections::BTreeMap;

use slates_ipc::protocol::{Refusal, ReplyBody};
use slates_mem::Handle;
use slates_vfs::shards_backing::ShardsBacking;
use slates_wire::request::RequestId;

use crate::merge_service::ReplyRoute;
use crate::state::ShardState;
use crate::verbs::refused;

/// Format: the bits a copy's handle index is shifted by in its mapping id, above the handle's generation.
const MAPPING_ID_INDEX_SHIFT: u32 = 32;

/// Counter: a direct map its client released (`ReleaseMap`): the copy let go and its bytes given back.
const RELEASED: &str = "dax.local_map_released";

/// The mapping id a client names a copy by: its backing's handle, index above generation. Process-local, like every
/// handle, so a client's maps end with its daemon's process (a restarted daemon's client holds none). Linux alone hands
/// copies today.
#[cfg(target_os = "linux")]
fn mapping_id_of(backing: Handle<ShardsBacking>) -> u64 {
  (u64::from(backing.index()) << MAPPING_ID_INDEX_SHIFT) | u64::from(backing.generation())
}

/// The backing a mapping id names; the slab refuses a stale one.
fn backing_of(mapping_id: u64) -> Handle<ShardsBacking> {
  let index = u32::try_from(mapping_id >> MAPPING_ID_INDEX_SHIFT).unwrap_or(u32::MAX);
  let generation = u32::try_from(mapping_id & u64::from(u32::MAX)).unwrap_or(u32::MAX);
  Handle::from_raw(index, generation)
}

/// A client's share of a shard's direct maps (§4.6, A-171): the shard's capacity — the copies it may hold and its fill
/// budget's bytes — over the clients it serves now, so one client never takes every copy from the others. A lone
/// client's share is the whole capacity; copies held already are never taken back, so a client that held more before
/// others came maps nothing more until it is within its share again.
// Pure, so tested on every host; only Linux hands a copy today (macOS's and Windows' routes are owed), so elsewhere
// only the tests call it.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
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
// Pure, so tested on every host; only Linux hands a copy today (macOS's and Windows' routes are owed), so elsewhere
// only the tests call it.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
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
/// is written through no more and its bytes go back to the fill budget; the client unmapped its own view first.
/// `NotFound` for a map this client does not hold: another client's, one released already, or one from before a
/// restart.
pub(crate) fn release_map(state: &mut ShardState, client_id: u32, mapping_id: u64) -> ReplyBody {
  let backing = backing_of(mapping_id);
  let ShardState { clients, store, .. } = state;
  let Some(client) = clients
    .iter_mut_all()
    .map(|(_, client)| client)
    .find(|client| client.client_id == client_id && !client.retiring)
  else {
    return refused(Refusal::NotFound);
  };
  let Some(at) = client.direct_maps.iter().position(|held| *held == backing) else {
    return refused(Refusal::NotFound);
  };
  client.direct_maps.swap_remove(at);
  let released = store.shards_backings.remove(backing).is_some();
  if released {
    state.count(RELEASED, 1);
  }
  ReplyBody::MapReleased
}

/// The direct maps being filled on this shard (§4.6, A-171).
#[derive(Default)]
pub(crate) struct DirectMapState {
  /// The requests a fill runs for, by origin, client and sequence, with where its reply goes: the newest attempt's
  /// route, since a retry comes from where its client now waits. One entry per task, so bounded by the shard's task
  /// arena.
  pub(crate) in_flight: BTreeMap<(u64, u32, u32), Option<ReplyRoute>>,
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

/// `MapRead` where no route hands a client a descriptor of a sealed copy yet: macOS's (its descriptor over A-161's
/// socket route) and Windows' (handle duplication) are owed to their own steps. The client reads by its paged path,
/// which applies the rights.
#[cfg(not(target_os = "linux"))]
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

#[cfg(target_os = "linux")]
pub(crate) use linux::map_read;

#[cfg(target_os = "linux")]
mod linux {
  use slates_db::catalog::{Principal, Role};
  use slates_ipc::protocol::{ReadAt, Refusal, ReplyBody, VolumeId};
  use slates_mem::{Handle, PeerObject};
  use slates_vfs::dir::Child;
  use slates_vfs::ids::InodeNo;
  use slates_vfs::shards_backing::{PeerRefusal, ShardsBacking, ShardsBackings};
  use slates_vfs::volume::Seek;
  use slates_wire::request::RequestId;

  use crate::error::{refusal_of_db, refusal_of_vfs};
  use crate::merge_service::{ReplyRoute, canonical_path, find_record};
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
    let same_uid =
      matches!(principal, Principal::Uid { uid } if *uid == rustix::process::geteuid().as_raw());
    if !same_uid || !matches!(at, ReadAt::Head) {
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
    match Fill::begin(state, handle, client_id, canonical_path(path), request) {
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

  /// Why a direct map did not begin.
  enum Begun {
    /// The path is a directory.
    Directory,
    /// The path does not resolve, or its file cannot be read.
    Vfs(slates_vfs::error::VfsError),
    /// A shape the direct map does not serve: an overlay, or a client not served on this shard.
    Unavailable,
    /// The copy could not be made: the budget, the backings' capacity or the OS refused it.
    FellBack,
    /// The client holds its share of the direct maps already: the refusal that says so.
    ShareExhausted(Refusal),
  }

  /// A direct map being filled: its copy, where the fill is, and what the hand-off and the reply need.
  struct Fill {
    /// The volume's slot, which a destroy meanwhile makes stale.
    volume: Handle<VolumeSlot>,
    /// The copy's backing, until the hand-off gives it to the client's slot; let go when the fill ends without one.
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
        // A fill that ended without its hand-off: the copy goes back. With the state unreachable (a shutdown) it
        // cannot, and the process's end returns it.
        let _released = crate::state::with_state(|s| s.store.shards_backings.remove(backing));
      }
    }
  }

  impl Fill {
    /// Resolves the path, checks the client's share, makes the file's copy and keeps it in the store's backings, held
    /// on the client's slot from now on, so the share counts copies still being filled as well as those handed.
    fn begin(
      state: &mut ShardState,
      volume: Handle<VolumeSlot>,
      client_id: u32,
      key: &str,
      request: (u64, RequestId),
    ) -> Result<Fill, Begun> {
      let name = format!("slates-map-{}", state.config.instance);
      let most_copies = state.config.direct_maps_per_shard;
      let ShardState {
        volumes,
        store,
        clients,
        ..
      } = state;
      let served = clients
        .iter_mut_all()
        .filter(|(_, client)| !client.retiring)
        .count();
      let share = super::share_of((most_copies, store.shards_backings.budget()), served);
      // A client served by another shard cannot be handed a descriptor from this one yet.
      let client = clients
        .iter_mut_all()
        .map(|(_, client)| client)
        .find(|client| {
          client.client_id == client_id && !client.retiring && client.control.is_some()
        })
        .ok_or(Begun::Unavailable)?;
      let slot = volumes.get(volume).map_err(|_| Begun::Unavailable)?;
      // An overlay's untouched file follows the disk, which no write boundary sees.
      if slot.host.is_some() {
        return Err(Begun::Unavailable);
      }
      let located = slot.volume.resolve(store, key).map_err(Begun::Vfs)?;
      if matches!(located.child, Child::Dir(_)) {
        return Err(Begun::Directory);
      }
      let size = slot
        .volume
        .stat(store, located.inode)
        .map_err(Begun::Vfs)?
        .size;
      let object_bytes = PeerObject::bytes_for(size).map_err(|_| Begun::FellBack)?;
      let held = holds(&mut client.direct_maps, &store.shards_backings);
      super::within_share(held, object_bytes, share).map_err(Begun::ShareExhausted)?;
      // Within its share, but the shard holds every copy the process's limits leave room for (others mapped before
      // this client came): this client reads by copy until some are released.
      if store.shards_backings.peers_held().0 >= most_copies {
        return Err(Begun::FellBack);
      }
      let object = PeerObject::create(&name, size).map_err(|_| Begun::FellBack)?;
      let backing = store
        .shards_backings
        .insert_peer(object, slot.volume.prefix(), located.inode)
        .map_err(|refusal| match refusal {
          PeerRefusal::Budget { .. } | PeerRefusal::Full(_) => Begun::FellBack,
        })?;
      client.direct_maps.push(backing);
      Ok(Fill {
        volume,
        backing: Some(backing),
        inode: located.inode,
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

    /// Hands the filled copy to its client: the descriptor over the client's control socket, then the backing kept on
    /// the client's slot, so its reap lets it go. The reply; `MapUnavailable` (counted) where the volume, the file or
    /// the client went, or the channel refused.
    fn hand_off(&mut self, state: &mut ShardState) -> ReplyBody {
      let Some(backing) = self.backing else {
        return refused(Refusal::MapUnavailable);
      };
      let ShardState {
        volumes,
        store,
        clients,
        ..
      } = state;
      let size = volumes
        .get(self.volume)
        .ok()
        .and_then(|slot| slot.volume.stat(store, self.inode).ok())
        .map(|attrs| attrs.size);
      let client = clients
        .iter_mut_all()
        .find(|(_, client)| client.client_id == self.client_id && !client.retiring)
        .map(|(_, client)| client);
      let (Some(file_len), Some(client), Some(copy)) =
        (size, client, store.shards_backings.get(backing))
      else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
      let Some(control) = client.control.as_ref() else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
      let mapping_id = super::mapping_id_of(backing);
      match control.send_mapping(
        copy.object().descriptor(),
        mapping_id,
        mapping_id,
        0,
        file_len,
        self.object_bytes,
      ) {
        Ok(()) => {
          // Held on the client's slot since the copy was made: the fill lets go of it no more.
          self.backing = None;
          state.count(HANDED, 1);
          ReplyBody::Mapped {
            mapping_id,
            region_off: 0,
            file_len,
            block_len: self.object_bytes,
          }
        }
        Err(slates_ipc::error::IpcError::UnconfinedDescriptor { .. }) => {
          state.count(UNCONFINED, 1);
          state.count(FELL_BACK, 1);
          refused(Refusal::MapUnavailable)
        }
        Err(_) => {
          state.count(FELL_BACK, 1);
          refused(Refusal::MapUnavailable)
        }
      }
    }
  }

  /// What a client holds of the direct maps, `(maps, bytes)`: `held`, its copies handed or being filled, less those the
  /// store let go of (a fill that ended early), which are forgotten here.
  fn holds(held: &mut Vec<Handle<ShardsBacking>>, backings: &ShardsBackings) -> (usize, u64) {
    held.retain(|backing| backings.peer_bytes(*backing).is_some());
    let bytes = held
      .iter()
      .filter_map(|backing| backings.peer_bytes(*backing))
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
    let page = rustix::param::page_size().max(1);
    let page_len = u64::try_from(page).unwrap_or(u64::MAX);
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

  /// Defers the current request's reply to the task that fills and hands over `fill`: the request becomes in flight,
  /// so a retry joins it. `Overloaded` when the shard's task arena is full, the copy let go.
  fn defer(state: &mut ShardState, fill: Fill) -> ReplyBody {
    let (origin, id) = fill.request;
    let route = state.reply_route.take();
    // The reply goes to the client's ring on this shard: the client is served here (checked as the fill began).
    if route.is_none_or(|route| route.shard != state.shard) {
      state.count(FELL_BACK, 1);
      return refused(Refusal::MapUnavailable);
    }
    let backing = fill.backing;
    match slates_rt::futures::spawn(run(fill)).and_then(slates_rt::futures::detach) {
      Ok(()) => {
        state
          .direct_map
          .in_flight
          .insert((origin, id.client, id.sequence), route);
        state.acceptance_deferred = true;
        // Never seen: `run_recorded` sends nothing for a verb that deferred its acceptance.
        ReplyBody::Acknowledged
      }
      Err(_) => {
        // The task was dropped inside this borrow of the shard's state, where its fill cannot reach the store: the copy
        // goes back here.
        if let Some(backing) = backing {
          let _released = state.store.shards_backings.remove(backing);
        }
        refused(Refusal::Overloaded { shard: state.shard })
      }
    }
  }

  /// Fills the copy a slice at a time, yielding between slices; then hands it over, records the reply's completion
  /// and delivers it to the client's ring.
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
    let answered = crate::state::with_state(|s| {
      let reply = match filled {
        Ok(()) => fill.hand_off(s),
        Err(_) => {
          s.count(FELL_BACK, 1);
          refused(Refusal::MapUnavailable)
        }
      };
      complete(s, request, reply)
    });
    // Delivered after the state's borrow ends, since delivery takes it again; on this shard, where the client is.
    if let Some((reply, Some(route))) = answered {
      crate::state::deliver(route.client_index, route.request, reply, true);
    }
  }

  /// Records the reply's completion as one transaction, and returns the reply with where it goes. A record the
  /// database refuses leaves nothing durable: counted, and the refusal is the reply, so a retry runs the verb again.
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
    let recorded = crate::verbs::record_completion(state, origin, id, reply);
    let reply = match state.db.commit(&mut state.segment) {
      Ok(_) => recorded,
      Err(e) => {
        let refusal = refusal_of_db(&e);
        state.count(crate::verbs::refusal_name(&refusal), 1);
        crate::verbs::reconcile_unpublished_effects(state);
        refused(refusal)
      }
    };
    (reply, route)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
