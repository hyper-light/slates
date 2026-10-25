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

use slates_wire::request::RequestId;

use crate::merge_service::ReplyRoute;
use crate::state::ShardState;

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

  /// Format: the bits a copy's handle index is shifted by in its mapping id, above the handle's generation.
  const MAPPING_ID_INDEX_SHIFT: u32 = 32;

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
    /// Resolves the path, makes the file's copy and keeps it in the store's backings, nothing filled yet.
    fn begin(
      state: &mut ShardState,
      volume: Handle<VolumeSlot>,
      client_id: u32,
      key: &str,
      request: (u64, RequestId),
    ) -> Result<Fill, Begun> {
      let served_here = state.clients.iter_mut_all().any(|(_, client)| {
        client.client_id == client_id && !client.retiring && client.control.is_some()
      });
      let name = format!("slates-map-{}", state.config.instance);
      let ShardState { volumes, store, .. } = state;
      let slot = volumes.get(volume).map_err(|_| Begun::Unavailable)?;
      // An overlay's untouched file follows the disk, which no write boundary sees; and a client served by another
      // shard cannot be handed a descriptor from this one yet.
      if slot.host.is_some() || !served_here {
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
      let object = PeerObject::create(&name, size).map_err(|_| Begun::FellBack)?;
      let object_bytes = object.len();
      let backing = store
        .shards_backings
        .insert_peer(object, slot.volume.prefix(), located.inode)
        .map_err(|refusal| match refusal {
          PeerRefusal::Budget { .. } | PeerRefusal::Full(_) => Begun::FellBack,
        })?;
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
      let mapping_id =
        (u64::from(backing.index()) << MAPPING_ID_INDEX_SHIFT) | u64::from(backing.generation());
      match control.send_mapping(
        copy.object().descriptor(),
        mapping_id,
        mapping_id,
        0,
        file_len,
        self.object_bytes,
      ) {
        Ok(()) => {
          client.direct_maps.push(backing);
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
