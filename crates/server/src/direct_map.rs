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
//! (`slates_ipc::route::confined_read_only`: read alone, and exactly the authorized length). Windows passes no
//! descriptor: the copy is an unnamed pagefile-backed section whose security descriptor grants no one any right
//! (`OWNER RIGHTS` may read it alone), and the daemon duplicates a read-only handle of it into the client's process,
//! judged first (`slates_ipc::handle_route::hand_section`: exactly the authorized size, and a duplicate that writes, one
//! that may rewrite the descriptor and a writable view each refused) and the process checked to run as the daemon's
//! user (A-175). The copy is kept in the
//! store's backings ([`slates_vfs::shards_backing`]), so the volume's single write boundary writes every later change
//! into it before the change returns: the client's map is a live view of the file within its length, as a shared map of
//! a file is.
//!
//! **Running.** The verb checks what it can at once (the uid, the view, the volume, the right, the client's channel,
//! the path), makes the copy, and defers its reply to a task of this shard, as an image build does (A-160). The task
//! fills the copy a slice at a time ([`crate::config::DaemonConfig::copy_slice_page_bytes`], the slice of a DAX
//! backing's fill, A-163), yielding between slices, so the shard serves its clients while a file of any size is copied.
//! A range is marked filled before its slice reads the volume, so a change written meanwhile lands on it and the rest
//! of the fill reads the volume as changed. Holes and pages of zeros are passed over: the new copy reads zero there
//! already. The fill done, the copy is handed: on Linux its descriptor goes over the client's control socket, then the
//! reply goes on its ring; on macOS the descriptor is offered on the daemon's socket route (A-161) behind a fresh
//! single-use secret that the reply carries, and the client claims it; on Windows the reply names the handle's value
//! in the client's process. The reply's completion is recorded as every verb's is, a Windows handle's value left out
//! ([`recorded_form`]).
//!
//! **What reads by copy instead** (`MapUnavailable`, which the client answers with its paged read): a peer of another
//! uid, a view other than the head, a merge volume, an overlay (an untouched file follows the disk, which no write
//! boundary sees), a client served by another shard (a descriptor does not cross shards yet), and a copy the fill
//! budget, the backings' capacity or the OS refuses.
//!
//! **Lifetime and bounds (A-172).** A copy is held on its client's slot from the moment it is made until the client
//! releases it (`ReleaseMap`) or is reaped (`crate::reap`). Every way a copy goes is [`let_go`], which withdraws an
//! unclaimed offer of it too (macOS), so an offer never outlives its copy. A volume's destroy or a file's last unlink
//! only detaches its copies from the write-through index: the client's map keeps their pages, so they stay held and
//! counted. A client maps within its share of the shard's capacity ([`share_of`], [`within_share`]). A copy whose
//! hand-off did not happen (its client gone, its channel refusing, the task ended early) is let go at once.

use std::collections::BTreeMap;

use slates_ipc::protocol::{Refusal, ReplyBody};
use slates_mem::{Encoded, Handle};
use slates_vfs::shards_backing::ShardsBacking;
use slates_wire::request::RequestId;

use crate::merge_service::ReplyRoute;
use crate::state::ShardState;
use crate::verbs::refused;

/// Counter: a direct map its client released (`ReleaseMap`): the copy let go and its bytes given back.
const RELEASED: &str = "dax.local_map_released";
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
/// is written through no more and its bytes go back to the fill budget; the client unmapped its own view first.
/// `NotFound` for a map this client does not hold: another client's, one released already, or one from before a
/// restart.
pub(crate) fn release_map(state: &mut ShardState, client_id: u32, mapping_id: u64) -> ReplyBody {
  let backing = backing_of(mapping_id);
  let Some(client) = state
    .clients
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
  if let_go(state, backing) {
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

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) use served::map_read;

/// The platforms that hand a client its copy: Linux over the client's control socket, macOS over the socket route,
/// Windows by duplicating a read-only handle into the client's process. The fill is one; the hand-off is a paired
/// function.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod served {
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
        let _released = crate::state::with_state(|s| super::let_go(s, backing));
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
          client.client_id == client_id && !client.retiring && can_be_handed_a_descriptor(client)
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

    /// Hands the filled copy to its client over its control socket (Linux), the backing held on the client's slot since
    /// the copy was made, so its reap lets it go. The reply; `MapUnavailable` (counted) where the volume, the file or
    /// the client went, the mapping id does not fit, or the channel refused.
    #[cfg(target_os = "linux")]
    fn hand_off(&mut self, state: &mut ShardState) -> ReplyBody {
      let Some(backing) = self.backing else {
        return refused(Refusal::MapUnavailable);
      };
      let partition = state.partition;
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
      let (Some(file_len), Some(client), Some(copy), Some(mapping_id)) = (
        size,
        client,
        store.shards_backings.get(backing),
        super::mapping_id_of(partition, backing),
      ) else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
      let Some(control) = client.control.as_ref() else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
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

    /// Offers the filled copy's read-only descriptor on the socket route (macOS), judged first to authorize reading
    /// the copy alone (`slates_ipc::route::confined_read_only`), behind a fresh single-use secret the reply carries; the
    /// backing held on the client's slot since the copy was made. The reply; `MapUnavailable` (counted) where the
    /// volume, the file or the copy went, the mapping id does not fit, the descriptor is unconfined, no secret can be
    /// drawn or no offer kept.
    #[cfg(target_os = "macos")]
    fn hand_off(&mut self, state: &mut ShardState) -> ReplyBody {
      use std::os::fd::AsFd;

      let Some(backing) = self.backing else {
        return refused(Refusal::MapUnavailable);
      };
      // An offer nobody will claim is not made: the client must still be served here.
      let served = state
        .clients
        .iter_mut_all()
        .any(|(_, client)| client.client_id == self.client_id && !client.retiring);
      if !served {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      }
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
      if slates_ipc::route::confined_read_only(descriptor.as_fd(), self.object_bytes).is_err() {
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
      // Held on the client's slot since the copy was made: the fill lets go of it no more.
      self.backing = None;
      state.count(HANDED, 1);
      ReplyBody::MapOffered {
        mapping_id,
        file_len,
        block_len: self.object_bytes,
        secret,
      }
    }

    /// Duplicates the filled copy's read-only handle into the client's process (Windows), judged first to authorize
    /// reading the copy alone and the process to run as the daemon's user (`slates_ipc::handle_route::hand_section`);
    /// the backing held on the client's slot since the copy was made. The reply names the handle's value there.
    /// `MapUnavailable` (counted) where the volume, the file or the copy went, the client went, the mapping id does not
    /// fit, the handle is unconfined, or the client's process is not the daemon's user's or refused the duplicate.
    #[cfg(windows)]
    fn hand_off(&mut self, state: &mut ShardState) -> ReplyBody {
      use std::os::windows::io::AsHandle;

      let Some(backing) = self.backing else {
        return refused(Refusal::MapUnavailable);
      };
      let pid = state
        .clients
        .iter_mut_all()
        .find(|(_, client)| client.client_id == self.client_id && !client.retiring)
        .map(|(_, client)| client.pid);
      let size = state
        .volumes
        .get(self.volume)
        .ok()
        .and_then(|slot| slot.volume.stat(&state.store, self.inode).ok())
        .map(|attrs| attrs.size);
      let read_only = state
        .store
        .shards_backings
        .get(backing)
        .and_then(|copy| copy.object().read_only_handle().ok());
      let (Some(pid), Some(file_len), Some(read_only), Some(mapping_id)) = (
        pid,
        size,
        read_only,
        super::mapping_id_of(state.partition, backing),
      ) else {
        state.count(FELL_BACK, 1);
        return refused(Refusal::MapUnavailable);
      };
      match slates_ipc::handle_route::hand_section(read_only.as_handle(), self.object_bytes, pid) {
        Ok(handle) => {
          // Held on the client's slot since the copy was made: the fill lets go of it no more.
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
          let _released = super::let_go(state, backing);
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

  /// Records the reply's completion as one transaction, in the form the record keeps ([`super::recorded_form`]), and
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
    let kept = super::recorded_form(&reply);
    let recorded = crate::verbs::record_completion(state, origin, id, kept.clone());
    let reply = match state.db.commit(&mut state.segment) {
      // The record holds the reply as it keeps it, so the client gets it whole, a Windows handle's value included.
      Ok(_) if recorded == kept => reply,
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
