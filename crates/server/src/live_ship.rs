//! Live shipping in the daemon (§4.10, A-200 "In the daemon"): each publication of a live-shipped volume is appended
//! to the volume's log on its owner shard and shipped to its candidates; the candidates keep the log sealed.
//!
//! **Owner.** The volume's own shard keeps its [`Shipper`] ([`LiveState::shipping`]), so positions are assigned in
//! publication order inside the barrier, where nothing can reorder them. [`published`] is called by the barrier once a
//! publication has committed locally: it builds the entry (the volume's publication record and the content of every
//! extent it names, read in the clear through `Volume::read` and sealed under the volume's lineage key, A-92), appends
//! it, and hands what each holder lacks to a task that ships it over the control shard's sessions (the only shard that
//! holds them, `fleet::take_sessions`). The replies come back to the owner shard and are folded there. The record
//! period re-ships what a holder still lacks ([`period`]).
//!
//! **Holder.** A candidate keeps, per live volume, a [`Replica`] (the fence and the head) and the sealed entries since
//! the last whole state, on the control shard ([`LiveState::held`]). It never opens one. An entry or install that does
//! not pass the chain or the fence is answered with the head held, which the owner reads as what to send next.
//!
//! A delta applies only to the record before it, so a publication that could not be appended (no credit, a read or a
//! seal refused) leaves the volume owing a whole record: its next publication ships the volume's whole image, and that
//! entry is the checkpoint an install is made from ([`Shipper::mark_checkpoint`]).
//!
//! **The barrier (A-200).** A recorded verb on a live volume commits its effects but neither records its completion nor
//! answers until the log is durable where its publication put it ([`durable_target`], [`defer`]); then both follow
//! ([`resolve`]), and a retry meanwhile joins the wait ([`join`]), so no client reads a success that `f` candidates
//! do not hold. Holders that never answer make the writer wait: backpressure, never a false success.
//!
//! Not yet: NFS `COMMIT` and FUSE `fsync` replies do not wait yet, a holder's replicas do not survive its own daemon's
//! restart, and a takeover does not read them.

use std::collections::{BTreeMap, VecDeque};

use slates_db::live::{
  Admitted, Entry, Head, Install, LiveError, Next, Replica, Shipper, decode_reply, encode_reply,
};
use slates_db::register::{HostId, ObjectId};
use slates_vfs::delta::VolumeRecord;
use slates_vfs::ids::InodeNo;
use slates_vfs::recover::{BodyImage, ExtentSourceImage, InodeImage};
use slates_wire::Wire;

use crate::state::{self, ShardState};

/// Format: an entry's first byte when its record is the volume's whole image (an install may be made from it, and a
/// holder drops what it held before it).
const KIND_WHOLE: u8 = 1;
/// Format: an entry's first byte when its record is a delta over the record before it.
const KIND_DELTA: u8 = 0;
/// Format: the second byte: the body is sealed under the volume's lineage key (A-92).
const BODY_SEALED: u8 = 1;
/// Format: the second byte: the body is in the clear, where this node cannot seal at all (no sealing root), as a
/// seal's archive is placed then (`fleet::envelope_of`).
const BODY_CLEAR: u8 = 0;
/// Derived: the entries a volume's shipper holds that are not yet durable, at most. Shape: one publication in flight
/// and one behind it, the pipelining of a writer that does not wait (A-200's barrier slice derives it from the measured
/// apply rate and round trip; until then a writer that outruns two is shipped a whole record next).
const CREDIT: u64 = 2;
/// Shape: the entries the owner retains for a holder that lags, beyond which it is installed: as many as the credit
/// lets be in flight, twice over.
const RETAIN: usize = 4;

/// Counter: a live volume's publication that could not be shipped as an entry, because its content could not be read,
/// sealed or appended; its next publication ships the whole volume.
const LIVE_REFUSED: &str = "fleet.live.refused";
/// Counter: entries appended to a live volume's log on this owner shard.
const LIVE_APPENDED: &str = "fleet.live.appended";
/// Counter: entries a holder refused to keep (over its bound, unauthorized, malformed).
const LIVE_HOLD_REFUSED: &str = "fleet.live.hold_refused";
/// Counter: a message for a holder this node had no session to (lent, or not formed): left to the period.
const LIVE_NO_SESSION: &str = "fleet.live.no_session";
/// Counter: a message a holder did not answer within the exchange's budget (or had no session to ride).
const LIVE_UNANSWERED: &str = "fleet.live.unanswered";
/// Counter: a record link dropped because its holder answered nothing ([`lose_links`]).
const LIVE_LINK_LOST: &str = "fleet.live.link_lost";
/// Counter: a destroyed volume's live log a candidate let go.
const LIVE_LET_GO: &str = "fleet.live.let_go";

/// Lets this candidate's copy of `object`'s live log go: its tombstone was accepted, so no owner will ship to it again.
/// Kept, it was a store that only grew (found 2026-11-05 by the destroy flow, `live_fleet`).
pub(crate) fn let_go(state: &mut ShardState, object: ObjectId) {
  if state.live.held.remove(&object).is_some() {
    state.count(LIVE_LET_GO, 1);
  }
}

/// Counter: takeover fetches this node answered with what it holds.
const LIVE_FETCHED: &str = "fleet.live.fetched";
/// Counter: entries and installs this node kept as a candidate.
const LIVE_HELD: &str = "fleet.live.held";
/// Counter: advances of a live volume's durable position on this owner shard (held by the owner and `f` candidates).
const LIVE_DURABLE: &str = "fleet.live.durable";

/// A live volume's content runs: the bytes a record's extents name, by inode and file offset.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct LiveRun {
  /// The inode.
  inode: u64,
  /// The file offset.
  offset: u64,
  /// The bytes.
  bytes: Vec<u8>,
}

/// One shipped publication: the record as the owner's journal holds it, and the content its extents name.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct LivePublication {
  /// The record (`VolumeRecord`'s wire bytes).
  record: Vec<u8>,
  /// The content.
  runs: Vec<LiveRun>,
}

/// A sealed body: the sealed chunk's keyed name and bytes (`slates_cluster::sealed`), and on a whole entry the volume's
/// lineage key wrapped for each neighbour (the `sealing` a head carries, A-92 piece 4c), so a successor can open the log
/// whether or not the volume ever sealed a snapshot.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct SealedBody {
  /// The lineage key for successors, on a whole entry.
  sealing: Option<crate::head::HeadSealing>,
  /// The keyed name.
  name: Vec<u8>,
  /// The sealed bytes.
  bytes: Vec<u8>,
}

/// A live volume on its owner shard.
#[derive(Debug)]
pub(crate) struct LiveVolume {
  shipper: Shipper,
  /// Its next publication must ship the whole volume (a delta would not apply to what the holders hold).
  owes_whole: bool,
  /// The last whole entry's payload, and its head: what an install sends.
  whole: Option<(Head, Vec<u8>)>,
}

/// What a candidate keeps of one live volume.
#[derive(Debug)]
pub(crate) struct Held {
  replica: Replica,
  /// The sealed payloads since the last whole one (that one first), in order.
  entries: VecDeque<Vec<u8>>,
  /// Their bytes.
  bytes: usize,
}

/// The node's live-shipping state, on every shard: the volumes this shard owns and ships, and (on the control shard)
/// what this node holds for others.
#[derive(Debug, Default)]
pub(crate) struct LiveState {
  /// The live volumes this shard owns, by volume bytes.
  pub(crate) shipping: BTreeMap<[u8; 16], LiveVolume>,
  /// What this node holds as a candidate, by object.
  pub(crate) held: BTreeMap<ObjectId, Held>,
  /// The live volumes the publication in progress recorded, with their record (`None`: the publication was a
  /// checkpoint, so the whole image ships): shipped once it commits ([`ship_captured`]), dropped if it is refused.
  pub(crate) captured: Vec<(slates_db::catalog::VolumeId, Option<Vec<u8>>)>,
  /// Verbs whose reply waits for their volume's log to be durable at a position (A-200's barrier): by volume and
  /// position, each with where to reply, its completion key and the reply. Bounded as the told waits are
  /// ([`awaiting_bound`]).
  awaiting: BTreeMap<([u8; 16], u64), Vec<Awaiting>>,
  /// The awaiting verbs by request, so a retry joins its wait ([`join`]).
  awaiting_by_request: BTreeMap<(u64, u32, u32), ([u8; 16], u64)>,
  /// What the transport barrier that just ran on this shard published of a live volume, for its caller to wait on
  /// ([`note_barrier`], [`take_barrier`]): set and taken within one synchronous serve.
  barrier: Option<(slates_db::catalog::VolumeId, u64)>,
  /// The transport replies parked until a volume is durable, by volume: their wakers, woken as it advances.
  parked: BTreeMap<[u8; 16], Vec<std::task::Waker>>,
  /// On the control shard: dispatches whose holders were still in flight at their return, each with the owner shard and
  /// the volume, holding those holders' sessions until they return ([`drain_stragglers`]). Bounded by the sessions: each
  /// holds at least one.
  stragglers: Vec<(u16, ObjectId, slates_cluster::Stragglers)>,
}

/// Notes, after a transport's barrier published `volume`, the position its reply must wait for ([`wait_durable`]).
pub(crate) fn note_barrier(state: &mut ShardState, volume: slates_db::catalog::VolumeId) {
  state.live.barrier = durable_target(state, volume).map(|target| (volume, target));
}

/// Takes what [`note_barrier`] noted.
pub(crate) fn take_barrier(state: &mut ShardState) -> Option<(slates_db::catalog::VolumeId, u64)> {
  state.live.barrier.take()
}

/// Derived: how long a transport's reply waits for durability before it is answered "try again" (NFS `JUKEBOX`): the
/// daemon's liveness budget, past which a holder that has not answered is not a live one to wait for.
const DURABLE_WAIT_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;

/// Waits on this (the volume's owner) shard, without blocking it, until `volume`'s log is durable at `target`, or
/// [`DURABLE_WAIT_NS`] has passed: whether it became durable. The caller answers a stable write or `COMMIT` only then,
/// and otherwise tells the client to try again: a barrier never claims what `f` candidates do not hold.
pub(crate) async fn wait_durable(volume: slates_db::catalog::VolumeId, target: u64) -> bool {
  let parked = std::future::poll_fn(|cx| {
    let done = state::with_state(|s| {
      let durable = head_placed(s, volume.bytes)
        && s.live.shipping.get(&volume.bytes).is_none_or(|live| {
          !live.owes_whole
            && live
              .shipper
              .durable()
              .is_some_and(|durable| durable >= target)
        });
      done_or_park(s, volume.bytes, durable, cx.waker())
    })
    .unwrap_or(true);
    if done {
      std::task::Poll::Ready(())
    } else {
      std::task::Poll::Pending
    }
  });
  let durable = matches!(
    slates_rt::futures::within(DURABLE_WAIT_NS, parked).await,
    Ok(Some(()))
  );
  let _ = state::with_state(|s| {
    s.count(
      if durable {
        LIVE_RESOLVED
      } else {
        LIVE_DURABLE_LATE
      },
      1,
    );
  });
  durable
}

/// Counter: a transport reply whose volume did not become durable within [`DURABLE_WAIT_NS`]; it was answered "try
/// again".
const LIVE_DURABLE_LATE: &str = "fleet.live.durable_late";

/// Whether the wait is over (`durable`); if not, parks `waker` on the volume, within the bound on parked replies (past
/// it the reply is not parked, so it waits out [`DURABLE_WAIT_NS`] and answers "try again").
fn done_or_park(
  state: &mut ShardState,
  volume: [u8; 16],
  durable: bool,
  waker: &std::task::Waker,
) -> bool {
  if durable {
    return true;
  }
  let held: usize = state.live.parked.values().map(Vec::len).sum();
  let parked = state.live.parked.entry(volume).or_default();
  if !parked.iter().any(|known| known.will_wake(waker)) {
    if held >= awaiting_bound_of(&state.config) {
      return false;
    }
    parked.push(waker.clone());
  }
  false
}

/// Wakes every reply parked on `volume`; each asks again and parks again if its position is not durable yet.
fn wake_parked(state: &mut ShardState, volume: [u8; 16]) {
  if let Some(parked) = state.live.parked.remove(&volume) {
    for waker in parked {
      waker.wake();
    }
  }
}

/// Whether the fleet knows `volume` exists: its head is placed at `f + 1` (at `f = 0` every head is, R8). Until then a
/// takeover cannot find it, so a write to it is not durable however many candidates hold the write: an owner that died
/// within the first record period lost a never-placed volume with every write it had acknowledged (found 2026-11-05 by
/// the takeover flow, which then waited for the placement by hand).
fn head_placed(state: &ShardState, volume: [u8; 16]) -> bool {
  let configuration = state.fleet.configuration();
  configuration.quorum.f == 0
    || state
      .placed_heads
      .get(&ObjectId(volume))
      .is_some_and(|head| configuration.region_placed(&head.placement))
}

/// A verb's reply held until its volume's log is durable where its publication put it.
#[derive(Debug)]
struct Awaiting {
  route: Option<crate::merge_service::ReplyRoute>,
  origin: u64,
  id: slates_wire::request::RequestId,
  reply: slates_ipc::protocol::ReplyBody,
}

/// Counter: verbs whose reply waited for durability.
const LIVE_AWAITED: &str = "fleet.live.awaited";
/// Counter: verbs answered once durable.
const LIVE_RESOLVED: &str = "fleet.live.resolved";
/// Counter: a resolved verb's completion that could not be recorded (its retry runs the verb again).
const LIVE_UNRECORDED: &str = "fleet.live.unrecorded";
/// Counter: a resolved verb's reply that could not be handed to its client's shard (the completion answers the retry).
const LIVE_UNDELIVERED: &str = "fleet.live.undelivered";
/// Counter: a verb refused `Overloaded` because the replies waiting for durability were at their bound.
const LIVE_OVER_BOUND: &str = "fleet.live.over_bound";

/// Derived: the replies a shard may hold for durability: one per request its clients may have in flight, the bound the
/// told waits use (`told::held_bound`: clients per shard times ring slots).
fn awaiting_bound(state: &ShardState) -> usize {
  awaiting_bound_of(&state.config)
}

fn awaiting_bound_of(config: &crate::config::DaemonConfig) -> usize {
  config
    .clients_per_shard
    .saturating_mul(usize::try_from(config.region.slots).unwrap_or(1))
}

/// The position `volume`'s log must be durable at before a verb that just published it may answer, or `None` when it
/// need not wait (not live-shipped, or durable there already). A publication that owed a whole record waits for the
/// next position, where that record will be appended.
pub(crate) fn durable_target(
  state: &ShardState,
  volume: slates_db::catalog::VolumeId,
) -> Option<u64> {
  let live = state.live.shipping.get(&volume.bytes)?;
  let head = live.shipper.head().map(|head| head.position);
  if !head_placed(state, volume.bytes) {
    // The fleet does not know the volume yet: a takeover could not find it, so nothing written to it is durable.
    return head;
  }
  let target = if live.owes_whole {
    head.map_or(Some(0), |head| head.checked_add(1))?
  } else {
    head?
  };
  let durable = live.shipper.durable();
  if !live.owes_whole && durable.is_some_and(|durable| durable >= target) {
    return None;
  }
  Some(target)
}

/// Holds a verb's reply until `volume`'s log is durable at `target` (A-200): its effects are committed, its completion
/// is recorded and its reply delivered then ([`resolve`]): `None`. The reply to give now instead, `Overloaded`
/// (counted, nothing recorded, so a retry runs the verb again), when the shard already holds its bound of such
/// replies.
pub(crate) fn defer(
  state: &mut ShardState,
  (volume, target): (slates_db::catalog::VolumeId, u64),
  (origin, id): (u64, slates_wire::request::RequestId),
  route: Option<crate::merge_service::ReplyRoute>,
  reply: slates_ipc::protocol::ReplyBody,
) -> Option<slates_ipc::protocol::ReplyBody> {
  let held: usize = state.live.awaiting.values().map(Vec::len).sum();
  if held >= awaiting_bound(state) {
    state.count(LIVE_OVER_BOUND, 1);
    return Some(slates_ipc::protocol::ReplyBody::Refused {
      refusal: slates_ipc::protocol::Refusal::Overloaded { shard: state.shard },
    });
  }
  state
    .live
    .awaiting
    .entry((volume.bytes, target))
    .or_default()
    .push(Awaiting {
      route,
      origin,
      id,
      reply,
    });
  state
    .live
    .awaiting_by_request
    .insert((origin, id.client, id.sequence), (volume.bytes, target));
  state.count(LIVE_AWAITED, 1);
  None
}

/// A retry of a verb whose reply still waits for durability joins the wait, answered with the same reply when it comes.
pub(crate) fn join(
  state: &mut ShardState,
  origin: u64,
  id: slates_wire::request::RequestId,
) -> bool {
  let Some(key) = state
    .live
    .awaiting_by_request
    .get(&(origin, id.client, id.sequence))
    .copied()
  else {
    return false;
  };
  let route = state.reply_route.take();
  if let (Some(waiting), Some(route)) = (state.live.awaiting.get_mut(&key), route)
    && let Some(first) = waiting.first()
  {
    let (origin, id, reply) = (first.origin, first.id, first.reply.clone());
    waiting.push(Awaiting {
      route: Some(route),
      origin,
      id,
      reply,
    });
  }
  true
}

/// Answers every verb waiting on `volume` at or below its durable position: its completion recorded on this partition
/// (one durable step, so a retry after meets it) and its reply delivered to its client's shard, as a green's accepted
/// submit is (`merge_service::resolve_accepted`).
pub(crate) fn resolve(state: &mut ShardState, volume: [u8; 16]) {
  wake_parked(state, volume);
  if !head_placed(state, volume) {
    return;
  }
  let Some(durable) = state
    .live
    .shipping
    .get(&volume)
    .and_then(|live| live.shipper.durable())
  else {
    return;
  };
  let keys: Vec<([u8; 16], u64)> = state
    .live
    .awaiting
    .range((volume, 0)..=(volume, durable))
    .map(|(key, _)| *key)
    .collect();
  for key in keys {
    let Some(waiting) = state.live.awaiting.remove(&key) else {
      continue;
    };
    for entry in waiting {
      state
        .live
        .awaiting_by_request
        .remove(&(entry.origin, entry.id.client, entry.id.sequence));
      state.db.begin();
      let recorded = crate::verbs::record_completion(state, entry.origin, entry.id, entry.reply);
      if state.db.commit(&mut state.segment).is_err() {
        state.count(LIVE_UNRECORDED, 1);
        continue;
      }
      state.count(LIVE_RESOLVED, 1);
      let Some(route) = entry.route else {
        continue;
      };
      let delivery = slates_rt::task::SpawnRequest::new(
        Box::pin(async move {
          crate::state::deliver(route.client_index, route.request, recorded, true);
        }),
        None,
      );
      if slates_rt::registry::send_control(
        route.shard,
        slates_rt::control::Control::Spawn(Box::new(delivery)),
      )
      .is_err()
      {
        state.count(LIVE_UNDELIVERED, 1);
      }
    }
  }
}

/// Notes each live-shipped volume of a committed checkpoint, whose whole image ships ([`ship_captured`]).
pub(crate) fn note_checkpoint(state: &mut ShardState, volumes: &[slates_db::catalog::VolumeId]) {
  for id in volumes {
    if is_live(state, *id) {
      state.live.captured.push((*id, None));
    }
  }
}

/// Ships what the publication that just committed recorded of each live-shipped volume ([`published`]).
pub(crate) fn ship_captured(state: &mut ShardState) {
  for (id, record) in std::mem::take(&mut state.live.captured) {
    published(state, id, record.as_deref());
  }
}

/// What to send one holder, built on the owner shard and sent from the control shard.
struct Send {
  object: ObjectId,
  holder: HostId,
  stream: u64,
  bytes: Vec<u8>,
}

/// Whether the volume with `id` is live-shipped, by its policy on this partition.
fn is_live(state: &ShardState, id: slates_db::catalog::VolumeId) -> bool {
  state
    .db
    .partition()
    .volume(id)
    .is_some_and(|volume| volume.policy.live_shipped)
}

/// The barrier's hook (A-200): `id`'s publication committed locally; `record` is its encoded record as the journal
/// took it, or `None` when the publication was a checkpoint (the whole image is built here). Does nothing for a volume
/// that is not live-shipped, or on a node with no fleet.
pub(crate) fn published(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  record: Option<&[u8]>,
) {
  if !is_live(state, id) {
    state.live.shipping.remove(&id.bytes);
    return;
  }
  let object = ObjectId(id.bytes);
  let configuration = state.fleet.configuration();
  let local = configuration.owner;
  let holders: Vec<HostId> = configuration
    .place(object)
    .candidates
    .into_iter()
    .filter(|host| *host != local)
    .collect();
  // The volume's epoch is its head's: a successor ships under the epoch its takeover placed the head at, which is above
  // the dead owner's, whatever its own host's epoch (heads use the same rule, `fleet::owed_record`).
  let placed = state.placed_heads.get(&object).map(|placed| placed.epoch);
  let epoch = placed.map_or(configuration.host_epoch, |placed| {
    placed.max(configuration.host_epoch)
  });
  let quorum = configuration.quorum;
  let live = state
    .live
    .shipping
    .entry(id.bytes)
    .or_insert_with(|| LiveVolume {
      shipper: Shipper::new(
        (local, object, epoch),
        quorum,
        &holders,
        None,
        (CREDIT, RETAIN),
      ),
      owes_whole: true,
      whole: None,
    });
  let whole = live.owes_whole || record.is_none();
  let built = if whole {
    whole_record(state, id)
  } else {
    record.map(<[u8]>::to_vec)
  };
  let Some(record) = built else {
    refuse(state, id);
    return;
  };
  let Some(payload) = payload(state, id, whole, record) else {
    refuse(state, id);
    return;
  };
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return;
  };
  // A whole record is a checkpoint and may go one past the credit (`Shipper::append_checkpoint`); a delta waits for it.
  let appended = if whole {
    live.shipper.append_checkpoint(payload.clone())
  } else {
    live.shipper.append(payload.clone())
  };
  match appended {
    Ok(entry) => {
      live.owes_whole = false;
      if whole {
        let head = entry.head();
        live.shipper.mark_checkpoint(head);
        live.whole = Some((head, payload));
      }
      state.count(LIVE_APPENDED, 1);
      resolve(state, id.bytes);
      let sends = pending_sends(state, id.bytes);
      dispatch(state, sends);
    }
    Err(_) => refuse(state, id),
  }
}

/// A publication that could not become an entry: counted, and the volume owes a whole record next.
fn refuse(state: &mut ShardState, id: slates_db::catalog::VolumeId) {
  state.count(LIVE_REFUSED, 1);
  if let Some(live) = state.live.shipping.get_mut(&id.bytes) {
    live.owes_whole = true;
  }
}

/// The volume's whole image as a record's wire bytes.
fn whole_record(state: &mut ShardState, id: slates_db::catalog::VolumeId) -> Option<Vec<u8>> {
  let handle = *state.by_id.get(&id)?;
  let ShardState { volumes, store, .. } = state;
  let slot = volumes.get_mut(handle).ok()?;
  let host = slot
    .host
    .as_mut()
    .map(|host| host as &mut dyn slates_vfs::host::HostFs);
  let image = slot.volume.to_image(store, host).ok()?;
  Some(VolumeRecord::Full { image }.to_bytes())
}

/// The entry payload of `record`: its kind and body byte, then the body (the record and its content runs), sealed
/// under the volume's lineage key where this node seals.
fn payload(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  whole: bool,
  record: Vec<u8>,
) -> Option<Vec<u8>> {
  let runs = content_runs(state, id, &record)?;
  let body = LivePublication { record, runs }.to_bytes();
  let kind = if whole { KIND_WHOLE } else { KIND_DELTA };
  if state.seal_root.is_none() {
    let mut payload = vec![kind, BODY_CLEAR];
    payload.extend_from_slice(&body);
    return Some(payload);
  }
  let account = crate::seal_keys::tenant_of(&state.db.partition().volume(id)?.owner);
  let lineage = crate::seal_keys::lineage(state, id, account).ok()?;
  let namer = crate::seal_keys::namer(state, id, account).ok()?;
  let chunk = slates_archive::Archive::raw_chunk(body);
  let segment = u32::try_from(rustix::param::page_size()).ok()?;
  let sealed = slates_cluster::sealed::seal_chunk(&lineage, &namer, &chunk, segment).ok()?;
  let sealing = if whole {
    let owner = state.db.partition().volume(id)?.owner.clone();
    crate::seal_keys::head_sealing(state, id, &owner)
  } else {
    None
  };
  let mut payload = vec![kind, BODY_SEALED];
  payload.extend_from_slice(
    &SealedBody {
      sealing,
      name: sealed.name.to_vec(),
      bytes: sealed.bytes,
    }
    .to_bytes(),
  );
  Some(payload)
}

/// The content of every extent `record`'s inode images name, read through the volume (sealed chunks open into the
/// buffer, A-99). `None` when a read is refused: the entry would be missing bytes, so it is not made.
fn content_runs(
  state: &ShardState,
  id: slates_db::catalog::VolumeId,
  record: &[u8],
) -> Option<Vec<LiveRun>> {
  let decoded = <VolumeRecord as Wire>::decode(&mut &record[..]).ok()?;
  let inodes: Vec<&InodeImage> = match &decoded {
    VolumeRecord::Full { image } => image.inodes.iter().collect(),
    VolumeRecord::Delta { delta } => delta
      .inodes
      .iter()
      .chain(delta.patches.iter().map(|patch| &patch.inode))
      .collect(),
  };
  let handle = *state.by_id.get(&id)?;
  let slot = state.volumes.get(handle).ok()?;
  let mut runs = Vec::new();
  for inode in inodes {
    let BodyImage::Chunked { extents, open } = &inode.body else {
      continue;
    };
    let mut spans: Vec<(u64, u64)> = extents
      .iter()
      .filter(|extent| matches!(extent.source, ExtentSourceImage::Chunk { .. }))
      .map(|extent| (extent.offset, extent.len))
      .collect();
    if let Some(open) = open {
      spans.push((open.offset, open.len));
    }
    for (offset, len) in spans {
      let mut bytes = vec![0u8; usize::try_from(len).ok()?];
      let read = slot
        .volume
        .read(&state.store, InodeNo(inode.no), offset, &mut bytes)
        .ok()?;
      bytes.truncate(read);
      runs.push(LiveRun {
        inode: inode.no,
        offset,
        bytes,
      });
    }
  }
  Some(runs)
}

/// What each holder of the volume lacks, as messages: entries, or an install from the last whole entry. An install
/// owed at a checkpoint the holder already holds cannot move it, so instead the volume is marked to owe a fresh whole
/// record (its next publication or the period appends one, and the install is made from that).
fn pending_sends(state: &mut ShardState, volume: [u8; 16]) -> Vec<Send> {
  let Some(live) = state.live.shipping.get_mut(&volume) else {
    return Vec::new();
  };
  let object = ObjectId(volume);
  let mut sends = Vec::new();
  let mut stale_checkpoint = false;
  for holder in live.shipper.holders() {
    match live.shipper.pending(*holder) {
      Next::Entries(entries) => {
        for entry in entries {
          sends.push(Send {
            object,
            holder: *holder,
            stream: slates_cluster::live::LIVE_ENTRY_STREAM,
            bytes: entry.encode(),
          });
        }
      }
      Next::Install => {
        let Some((head, payload)) = &live.whole else {
          stale_checkpoint = true;
          continue;
        };
        if live
          .shipper
          .held_by(*holder)
          .is_some_and(|held| held.rank() >= head.rank())
        {
          stale_checkpoint = true;
          continue;
        }
        let install = Install {
          owner: live.shipper.owner(),
          object,
          epoch: live.shipper.epoch(),
          head: *head,
          state: payload.clone(),
        };
        sends.push(Send {
          object,
          holder: *holder,
          stream: slates_cluster::live::LIVE_STATE_STREAM,
          bytes: install.encode(),
        });
      }
      Next::Nothing | Next::Fenced(_) => {}
    }
  }
  if stale_checkpoint {
    live.owes_whole = true;
  }
  sends
}

/// Ships `sends` from the control shard and folds the replies on this (the owner) shard, in a task of this shard: the
/// barrier that called it does not wait (A-200's next slice makes it). The task is bounded by the dispatch's budget and
/// detached, so nothing joins it and its slot is reaped when it ends.
fn dispatch(state: &ShardState, sends: Vec<Send>) {
  if sends.is_empty() {
    return;
  }
  let (origin, control) = (
    state.shard,
    state.shards.first().copied().unwrap_or(state.shard),
  );
  let spawned = slates_rt::futures::spawn(async move {
    let replies = send_from_control(origin, control, sends).await;
    let _ = state::with_state(|s| fold_into(s, replies));
  });
  if let Ok(task) = spawned {
    let _ = slates_rt::futures::detach(task);
  }
}

/// Sends each message over the control shard's session to its holder and brings back every reply that came in the
/// dispatch's budget; the stragglers' replies follow through [`fold_into`] when they come.
async fn send_from_control(
  origin: u16,
  control: u16,
  sends: Vec<Send>,
) -> Vec<(ObjectId, HostId, Vec<u8>)> {
  let budget = crate::fleet::live_budget();
  let deadline = budget.max_deadline_ns().saturating_mul(2);
  crate::xshard::task_within(
    origin,
    control,
    move || async move {
      drain_stragglers();
      let mut replies = Vec::new();
      for send in sends {
        let sessions =
          crate::fleet::take_sessions("fleet.lent_to.live_ship", |host| host == send.holder);
        if sessions.is_empty() {
          let _ = state::with_state(|s| s.count(LIVE_NO_SESSION, 1));
        }
        let exchanged =
          slates_cluster::live::exchange((send.stream, &send.bytes), sessions, budget, |_, _| true)
            .await;
        if exchanged.replies.is_empty() {
          let _ = state::with_state(|s| s.count(LIVE_UNANSWERED, 1));
        }
        lose_links(&exchanged.unanswered);
        crate::fleet::return_sessions(exchanged.reusable);
        replies.extend(
          exchanged
            .replies
            .into_iter()
            .map(|(host, bytes)| (send.object, host, bytes)),
        );
        settle_stragglers(origin, send.object, exchanged.stragglers);
      }
      replies
    },
    deadline,
  )
  .await
  .unwrap_or_default()
}

/// Queues a dispatch's stragglers on this (the control) shard, where they hold their holders' sessions until each
/// returns ([`drain_stragglers`]). Not a task per dispatch: a task the shard's arena refused dropped the stragglers and
/// the sessions inside them, so the links stayed lent with nothing behind them and those holders were never sent to
/// again (one of two candidates held nothing in five of eight flows, 2026-11-05, `live_fleet`, 416 sends with no
/// session).
fn settle_stragglers(origin: u16, object: ObjectId, stragglers: slates_cluster::Stragglers) {
  let _ = state::with_state(|s| s.live.stragglers.push((origin, object, stragglers)));
}

/// Recovers what queued stragglers have finished: each answered holder's session back to the pool and its reply to the
/// volume's owner shard, each unanswered holder's link dropped to be dialed afresh. On the control shard, outside any
/// borrow of its state (returning a session borrows it). Called at every dispatch and every record period.
pub(crate) fn drain_stragglers() {
  let Some(queued) = state::with_state(|s| std::mem::take(&mut s.live.stragglers)) else {
    return;
  };
  let mut kept = Vec::new();
  for (origin, object, mut stragglers) in queued {
    let (late, done) = stragglers.recover_replies();
    let mut replies = Vec::new();
    let mut sessions = Vec::new();
    let mut lost = Vec::new();
    for (host, reply, endpoint) in late {
      if reply.round_trip_ns.is_none() {
        lost.push(host);
        continue;
      }
      replies.push((object, host, reply.bytes));
      sessions.push((host, endpoint));
    }
    lose_links(&lost);
    crate::fleet::return_sessions(sessions);
    if !replies.is_empty() {
      let control = state::with_state(|s| s.shard).unwrap_or(origin);
      crate::xshard::run_on_counted(control, origin, move |s| fold_into(s, replies));
    }
    if !done {
      kept.push((origin, object, stragglers));
    }
  }
  let _ = state::with_state(|s| s.live.stragglers.extend(kept));
}

/// Drops the record links of holders that answered nothing (on the control shard, where the links live): their link
/// tasks dial afresh, as an unanswered discovery exchange's does (`fleet::exchange_discovery`). Returned to the pool, a
/// session dead at a resumed peer's end was lent to every later message and none was ever answered: a candidate stopped
/// and resumed stayed at its old head for the whole of a 40 s wait (2026-11-05, `live_fleet`, 1,478 sends that found no
/// session and 1,483 unanswered).
fn lose_links(hosts: &[HostId]) {
  if hosts.is_empty() {
    return;
  }
  let _ = state::with_state(|s| {
    for host in hosts {
      s.record_sessions.remove(host);
      s.count(LIVE_LINK_LOST, 1);
    }
  });
}

/// Folds replies into the owner shard's shippers and ships what the holders still lack.
fn fold_into(state: &mut ShardState, replies: Vec<(ObjectId, HostId, Vec<u8>)>) {
  let mut touched = Vec::new();
  for (object, host, bytes) in replies {
    let Some(live) = state.live.shipping.get_mut(&object.0) else {
      continue;
    };
    let before = live.shipper.durable();
    let held_before = live.shipper.held_by(host);
    let reply = decode_reply(&bytes);
    if let Next::Fenced(_) = live.shipper.on_reply(host, reply) {
      // A later epoch holds the volume: this owner was superseded and ships no more of it.
      state.live.shipping.remove(&object.0);
      continue;
    }
    // Only an answer that moved the holder forward on the chain is followed at once; anything else (a refusal, a holder
    // that does not hold the volume's catalog yet, one off the chain) is left to the period. Following every answer
    // looped at the network's speed: 982 and 5,501 refusals on the two holders of one five-write run, and later a holder
    // installed at a checkpoint the tail had passed, re-installed there without end (2026-11-05, `live_fleet`).
    let progressed = live.shipper.held_by(host) != held_before;
    if live.shipper.durable() > before {
      state.count(LIVE_DURABLE, 1);
      resolve(state, object.0);
    }
    if progressed && !touched.contains(&object.0) {
      touched.push(object.0);
    }
  }
  let mut sends = Vec::new();
  for volume in touched {
    sends.extend(pending_sends(state, volume));
  }
  dispatch(state, sends);
}

/// The record period's turn on `shard` (run from the control shard): re-ships what each holder of each live volume
/// there still lacks, so a lost message or a holder that was away is caught up without waiting for the next write.
pub(crate) async fn period(origin: u16, shard: u16) {
  drain_stragglers();
  let _ = crate::xshard::call_within(
    origin,
    shard,
    |s| {
      let volumes: Vec<[u8; 16]> = s.live.shipping.keys().copied().collect();
      // A volume that owes a whole record ships one once its credit allows: a burst that outran the credit left its
      // last writes unshipped until another write came, and none might (three of five writes, `live_fleet`).
      for volume in &volumes {
        let owed = s
          .live
          .shipping
          .get(volume)
          .is_some_and(|live| live.owes_whole && live.shipper.in_flight() <= CREDIT);
        if owed {
          published(s, slates_db::catalog::VolumeId { bytes: *volume }, None);
        }
      }
      let mut sends = Vec::new();
      for volume in volumes {
        sends.extend(pending_sends(s, volume));
      }
      dispatch(s, sends);
    },
    crate::fleet::live_budget().max_deadline_ns(),
  )
  .await;
}

/// This shard's live logs for `status` (A-200): each it ships, then each it holds as a candidate.
pub(crate) fn reports(state: &ShardState) -> Vec<slates_ipc::protocol::LiveReport> {
  let shipping =
    state
      .live
      .shipping
      .iter()
      .map(|(volume, live)| slates_ipc::protocol::LiveReport {
        volume: slates_ipc::protocol::VolumeId { bytes: *volume },
        holding: false,
        head: live.shipper.head().map(|head| head.position),
        durable: live.shipper.durable(),
        owed: live.owes_whole,
      });
  let held = state
    .live
    .held
    .iter()
    .map(|(object, held)| slates_ipc::protocol::LiveReport {
      volume: slates_ipc::protocol::VolumeId { bytes: object.0 },
      holding: true,
      head: held.replica.head().map(|head| head.position),
      durable: None,
      owed: false,
    });
  shipping.chain(held).collect()
}

/// The bound on what a candidate keeps of one volume: its whole record and the deltas after it are each at most the
/// volume's size, and the owner ships a whole record again once its own journal takes a checkpoint, which it does once
/// the deltas since the last reach the checkpoint's size (`publish_captured`).
/// Derived: twice the volume's size limit from its catalog record; `None` (refused) for a volume whose catalog this
/// node does not hold yet (the period ships again once the catalog record arrives).
fn hold_bound(state: &ShardState, object: ObjectId) -> Option<usize> {
  let catalog = object.catalog();
  let (_, value) = state.holder_records.get(&catalog)?.highest(catalog)?;
  let catalog = crate::catalog::CatalogValue::from_record_bytes(value)?;
  let limit = match catalog.size {
    slates_db::catalog::SizeClass::Bounded { limit } => limit,
    slates_db::catalog::SizeClass::Dynamic { max } => max,
  };
  usize::try_from(limit.saturating_mul(2)).ok()
}

/// A candidate's answer to an entry from `peer` (A-200): authorized as a content placement (`fleet::content_authorized`),
/// checked against the replica's chain and fence, kept within its bound, answered with the head held.
pub(crate) fn serve_entry(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Some(bound) = admitted(state, local, peer, slates_db::live::object_of(request)) else {
    state.count(LIVE_HOLD_REFUSED, 1);
    return encode_reply(&Err(LiveError::Unauthorized));
  };
  let reply = Entry::decode(request, bound).and_then(|entry| {
    let held = held_entry(state, peer, entry.object);
    match held.replica.check(&entry)? {
      Admitted::Apply(head) => {
        keep(held, &entry.payload, bound)?;
        held.replica.commit(entry.epoch, head);
        Ok(Some(head))
      }
      Admitted::Held => Ok(held.replica.head()),
    }
  });
  match reply {
    Err(_) => state.count(LIVE_HOLD_REFUSED, 1),
    Ok(_) => state.count(LIVE_HELD, 1),
  }
  encode_reply(&reply)
}

/// A candidate's answer to an install from `peer`, as [`serve_entry`]: the install's state replaces what it held.
pub(crate) fn serve_install(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Some(bound) = admitted(
    state,
    local,
    peer,
    slates_db::live::object_of_state(request),
  ) else {
    state.count(LIVE_HOLD_REFUSED, 1);
    return encode_reply(&Err(LiveError::Unauthorized));
  };
  let reply = Install::decode(request, bound).and_then(|install| {
    let held = held_entry(state, peer, install.object);
    match held.replica.check_install(&install)? {
      Admitted::Apply(head) => {
        held.entries.clear();
        held.bytes = 0;
        keep(held, &install.state, bound)?;
        held.replica.commit(install.epoch, head);
        Ok(Some(head))
      }
      Admitted::Held => Ok(held.replica.head()),
    }
  });
  match reply {
    Err(_) => state.count(LIVE_HOLD_REFUSED, 1),
    Ok(_) => state.count(LIVE_HELD, 1),
  }
  encode_reply(&reply)
}

/// The bound for a message naming `object`, when `peer` may place it on this node.
fn admitted(
  state: &ShardState,
  local: HostId,
  peer: HostId,
  object: Option<ObjectId>,
) -> Option<usize> {
  let object = object?;
  let regional = state.council.configuration();
  let placing = slates_cluster::content::ContentAccess::Place;
  crate::fleet::content_authorized(
    regional,
    &state.holder_records,
    local,
    peer,
    placing,
    object,
  )
  .then(|| hold_bound(state, object))
  .flatten()
}

/// The holder's copy of `object`, kept for `owner`: the configuration names `owner` the volume's acting owner (the caller
/// checked it, [`admitted`]), so a successor takes the copy over; the fence still refuses any stale epoch.
fn held_entry(state: &mut ShardState, owner: HostId, object: ObjectId) -> &mut Held {
  let held = state.live.held.entry(object).or_insert_with(|| Held {
    replica: Replica::new(owner, object),
    entries: VecDeque::new(),
    bytes: 0,
  });
  held.replica.reassign(owner);
  held
}

/// A candidate's answer to a successor's takeover fetch (A-200): authorized as a read of the object
/// (`fleet::content_authorized`: the acting owner, a candidate, or a recovery cohort), the fence raised to the
/// successor's epoch, and the head and the sealed entries kept. A stale successor is refused by the fence.
pub(crate) fn serve_fetch(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Ok(fetch) = slates_db::live::Fetch::decode(request) else {
    state.count(LIVE_HOLD_REFUSED, 1);
    return slates_db::live::encode_fetched(&Err(LiveError::Malformed), &[]);
  };
  let regional = state.council.configuration();
  let reading = slates_cluster::content::ContentAccess::Read;
  if !crate::fleet::content_authorized(
    regional,
    &state.holder_records,
    local,
    peer,
    reading,
    fetch.object,
  ) {
    state.count(LIVE_HOLD_REFUSED, 1);
    return slates_db::live::encode_fetched(&Err(LiveError::Unauthorized), &[]);
  }
  let Some(held) = state.live.held.get_mut(&fetch.object) else {
    return slates_db::live::encode_fetched(&Ok(None), &[]);
  };
  match held.replica.prepare(fetch.epoch) {
    Ok(head) => {
      held.replica.reassign(peer);
      let entries: Vec<Vec<u8>> = held.entries.iter().cloned().collect();
      state.count(LIVE_FETCHED, 1);
      slates_db::live::encode_fetched(&Ok(head), &entries)
    }
    Err(refusal) => slates_db::live::encode_fetched(&Err(refusal), &[]),
  }
}

/// A candidate's answer on the whole-state kind: a fetch or an install, by its tag.
pub(crate) fn serve_state(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  if slates_db::live::is_fetch(request) {
    serve_fetch(state, local, peer, request)
  } else {
    serve_install(state, local, peer, request)
  }
}

/// Keeps `payload` after what is held (a whole one first drops what came before it), within `bound`.
fn keep(held: &mut Held, payload: &[u8], bound: usize) -> Result<(), LiveError> {
  if payload.first() == Some(&KIND_WHOLE) {
    held.entries.clear();
    held.bytes = 0;
  }
  let bytes = held
    .bytes
    .checked_add(payload.len())
    .ok_or(LiveError::Malformed)?;
  if bytes > bound {
    return Err(LiveError::Malformed);
  }
  held.entries.push_back(payload.to_vec());
  held.bytes = bytes;
  Ok(())
}

// ── Takeover from a live log (A-200 "Takeover") ─────────────────────────────────────────────────────────────────────

/// Counter: a takeover of a live volume that could not rebuild it from the adopted log (its first entry not whole, an
/// entry that would not open or apply, or content missing for an extent): the volume is not served from it.
const LIVE_TAKEOVER_REFUSED: &str = "fleet.live.takeover_refused";
/// Counter: a taken-over live volume rebuilt from the adopted log.
const LIVE_TAKEN_OVER: &str = "fleet.live.taken_over";

/// What a takeover fetched of a live volume: the adopted head's payloads (a whole one first), and the
/// successor's copy of the lineage key the first carries (A-92 piece 4c), unwrapped on this control shard.
pub(crate) struct Fetched {
  pub(crate) payloads: Vec<Vec<u8>>,
  pub(crate) lineage: Option<([u8; 16], Vec<u8>)>,
  pub(crate) naming: Option<crate::head::HeadNaming>,
}

/// Phase one for a live volume (A-200), on the control shard: raises this node's own copy's fence to `epoch`, asks every
/// other candidate in `candidates` to do the same over its session, and adopts the greatest head among the answers
/// (`slates_db::live::adopt`, at least `f + 1` of them). `Ok(None)` when no candidate holds any of the volume's log
/// (it was never shipped); `Err` with what the takeover waits for when too few answered.
pub(crate) async fn fetch(
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
  candidates: Vec<HostId>,
) -> Result<Option<Fetched>, &'static str> {
  let local = state::with_state(|s| s.fleet.configuration().owner)
    .ok_or("fleet.live.takeover_waits_state")?;
  let quorum = state::with_state(|s| s.fleet.configuration().quorum)
    .ok_or("fleet.live.takeover_waits_state")?;
  let cap = state::with_state(|s| hold_bound(s, object))
    .flatten()
    .ok_or("fleet.live.takeover_waits_catalog")?;
  let mut reports: Vec<(HostId, Option<Head>)> = Vec::new();
  let mut kept: BTreeMap<HostId, Vec<Vec<u8>>> = BTreeMap::new();
  if let Some(Some(own)) = state::with_state(|s| {
    let held = s.live.held.get_mut(&object)?;
    let head = held.replica.prepare(epoch).ok()?;
    held.replica.reassign(local);
    Some((head, held.entries.iter().cloned().collect::<Vec<_>>()))
  }) {
    reports.push((local, own.0));
    kept.insert(local, own.1);
  }
  let others: Vec<HostId> = candidates
    .into_iter()
    .filter(|host| *host != local)
    .collect();
  let sessions =
    crate::fleet::take_sessions("fleet.lent_to.live_takeover", |host| others.contains(&host));
  let request = slates_db::live::Fetch { object, epoch }.encode();
  let exchanged = slates_cluster::live::exchange(
    (slates_cluster::live::LIVE_STATE_STREAM, &request),
    sessions,
    crate::fleet::live_budget(),
    |_, _| false,
  )
  .await;
  crate::fleet::return_sessions(exchanged.reusable);
  settle_stragglers(
    state::with_state(|s| s.shard).unwrap_or_default(),
    object,
    exchanged.stragglers,
  );
  for (host, bytes) in exchanged.replies {
    if let Ok((head, payloads)) = slates_db::live::decode_fetched(&bytes, cap) {
      reports.push((host, head));
      kept.insert(host, payloads);
    }
  }
  let needed = usize::try_from(quorum.f)
    .ok()
    .and_then(|f| f.checked_add(1))
    .unwrap_or(usize::MAX);
  if reports.len() < needed {
    return Err("fleet.live.takeover_waits_quorum");
  }
  let Some((holder, _)) = slates_db::live::adopt(&reports, quorum) else {
    return Ok(None);
  };
  let payloads = kept.remove(&holder).unwrap_or_default();
  let sealing = payloads
    .first()
    .and_then(|first| sealed_body(first))
    .and_then(|body| body.sealing);
  let lineage = sealing.as_ref().and_then(|sealing| {
    state::with_state(|s| crate::seal_keys::successor_lineage(s, sealing)).flatten()
  });
  let naming = sealing.and_then(|sealing| sealing.naming);
  Ok(Some(Fetched {
    payloads,
    lineage,
    naming,
  }))
}

/// A payload's sealed body, when it is sealed.
fn sealed_body(payload: &[u8]) -> Option<SealedBody> {
  let (header, body) = payload.split_at_checked(2)?;
  if header.get(1) != Some(&BODY_SEALED) {
    return None;
  }
  <SealedBody as Wire>::decode(&mut &body[..]).ok()
}

/// The volume a live log describes, rebuilt as a takeover's archive on the volume's new owner shard (A-200): the
/// lineage key adopted, every payload opened, the deltas applied in order to the whole image, and the result laid out
/// as `materialize_taken_over` restores (directories and files, a symlink as a file of its target, as the export lays
/// it out). Refused, counted, when the log cannot be rebuilt whole: the volume is then not served from it.
pub(crate) fn archive_from_live(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  owner: &slates_db::catalog::Principal,
  fetched: &Fetched,
) -> Result<slates_archive::Archive, &'static str> {
  let rebuilt = rebuild(state, id, owner, fetched);
  match &rebuilt {
    Ok(_) => state.count(LIVE_TAKEN_OVER, 1),
    Err(_) => state.count(LIVE_TAKEOVER_REFUSED, 1),
  }
  rebuilt
}

fn rebuild(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  owner: &slates_db::catalog::Principal,
  fetched: &Fetched,
) -> Result<slates_archive::Archive, &'static str> {
  if let Some(lineage) = fetched.lineage.clone() {
    crate::seal_keys::adopt_lineage(state, id, owner, lineage, fetched.naming.as_ref())
      .map_err(|_| LIVE_TAKEOVER_REFUSED)?;
  }
  let mut publications = Vec::new();
  for payload in &fetched.payloads {
    publications.push(open_payload(state, id, owner, payload).ok_or(LIVE_TAKEOVER_REFUSED)?);
  }
  let mut image = None;
  let mut runs: BTreeMap<(u64, u64), Vec<u8>> = BTreeMap::new();
  for publication in publications {
    let record = <VolumeRecord as Wire>::decode(&mut &publication.record[..])
      .map_err(|_| LIVE_TAKEOVER_REFUSED)?;
    match (record, image.as_mut()) {
      (VolumeRecord::Full { image: whole }, _) => {
        image = Some(whole);
        runs.clear();
      }
      (VolumeRecord::Delta { delta }, Some(image)) => {
        slates_vfs::recover::VolumeImage::apply(image, &delta)
          .map_err(|_| LIVE_TAKEOVER_REFUSED)?;
      }
      (VolumeRecord::Delta { .. }, None) => return Err(LIVE_TAKEOVER_REFUSED),
    }
    for run in publication.runs {
      runs.insert((run.inode, run.offset), run.bytes);
    }
  }
  let image = image.ok_or(LIVE_TAKEOVER_REFUSED)?;
  archive_of(&image, &runs, ObjectId(id.bytes).local())
}

/// One payload's body, opened under the volume's lineage key when sealed.
fn open_payload(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  owner: &slates_db::catalog::Principal,
  payload: &[u8],
) -> Option<LivePublication> {
  let (header, body) = payload.split_at_checked(2)?;
  let bytes = if header.get(1) == Some(&BODY_SEALED) {
    let sealed = <SealedBody as Wire>::decode(&mut &body[..]).ok()?;
    let account = crate::seal_keys::tenant_of(owner);
    let lineage = crate::seal_keys::lineage(state, id, account).ok()?;
    let namer = crate::seal_keys::namer(state, id, account).ok()?;
    let chunk = slates_cluster::sealed::SealedChunk {
      name: sealed.name.as_slice().try_into().ok()?,
      bytes: sealed.bytes,
    };
    slates_cluster::sealed::open_chunk(&lineage, &namer, &chunk)
      .ok()?
      .payload
  } else {
    body.to_vec()
  };
  // A sealed body was sealed from a raw chunk of the publication's bytes (`payload`), so the opened chunk's payload is
  // those bytes.
  <LivePublication as Wire>::decode(&mut &bytes[..]).ok()
}

/// A volume image and the content its extents name, laid out as an archive (the export's layout,
/// `slates_vfs::export`): each directory's entries under it, each file's extents over chunks of their bytes, a
/// symlink as a file of its target; holes stay holes. `volume_id` is the archive's header field: the volume's
/// per-creator number (`ObjectId::local`), informational to a restore.
fn archive_of(
  image: &slates_vfs::recover::VolumeImage,
  runs: &BTreeMap<(u64, u64), Vec<u8>>,
  volume_id: u64,
) -> Result<slates_archive::Archive, &'static str> {
  let inodes: BTreeMap<u64, &InodeImage> =
    image.inodes.iter().map(|inode| (inode.no, inode)).collect();
  let mut chunks = Vec::new();
  let root = inodes.get(&image.root_no).ok_or(LIVE_TAKEOVER_REFUSED)?;
  let root_meta = meta_of(root);
  let mut visiting = Vec::new();
  let manifest = node_of(root, &inodes, runs, &mut chunks, &mut visiting)?;
  let chunk_max = chunks
    .iter()
    .map(|chunk: &slates_archive::Chunk| u32::try_from(chunk.raw_len).unwrap_or(u32::MAX))
    .max()
    .unwrap_or(0);
  Ok(slates_archive::Archive {
    base_page_size: u32::try_from(rustix::param::page_size()).unwrap_or(u32::MAX),
    chunk_min: chunk_max,
    chunk_max,
    created_unix: 0,
    volume_id,
    snapshot_id: 0,
    name_policy_id: 0,
    unicode_version: 0,
    root_meta,
    manifest,
    chunks,
  })
}

/// The manifest node of `inode`, its children walked; a directory reached again along its own path (a cycle no valid
/// image holds) is refused.
fn node_of(
  inode: &InodeImage,
  inodes: &BTreeMap<u64, &InodeImage>,
  runs: &BTreeMap<(u64, u64), Vec<u8>>,
  chunks: &mut Vec<slates_archive::Chunk>,
  visiting: &mut Vec<u64>,
) -> Result<slates_archive::Node, &'static str> {
  match &inode.body {
    BodyImage::Directory { entries, .. } => {
      if visiting.contains(&inode.no) {
        return Err(LIVE_TAKEOVER_REFUSED);
      }
      visiting.push(inode.no);
      let mut children = Vec::new();
      for entry in entries {
        let Some(child) = entry.child.and_then(|no| inodes.get(&no)) else {
          continue;
        };
        children.push(slates_archive::Entry {
          name: entry.name.clone(),
          meta: meta_of(child),
          node: node_of(child, inodes, runs, chunks, visiting)?,
        });
      }
      visiting.pop();
      Ok(slates_archive::Node::Directory(children))
    }
    BodyImage::Inline { bytes } => Ok(slates_archive::Node::File(whole_file(
      bytes.clone(),
      chunks,
    ))),
    BodyImage::Symlink { target } | BodyImage::BaseLink { target } => Ok(
      slates_archive::Node::File(whole_file(target.as_bytes().to_vec(), chunks)),
    ),
    BodyImage::Chunked { extents, open } => {
      let mut laid = Vec::new();
      let spans = extents
        .iter()
        .filter(|extent| matches!(extent.source, ExtentSourceImage::Chunk { .. }))
        .map(|extent| (extent.offset, extent.len))
        .chain(open.iter().map(|open| (open.offset, open.len)));
      for (offset, len) in spans {
        let bytes = runs.get(&(inode.no, offset)).ok_or(LIVE_TAKEOVER_REFUSED)?;
        if u64::try_from(bytes.len()).ok() != Some(len) {
          return Err(LIVE_TAKEOVER_REFUSED);
        }
        let chunk = slates_archive::Archive::raw_chunk(bytes.clone());
        laid.push(slates_archive::Extent {
          offset,
          len,
          chunk: chunk.identity,
          chunk_offset: 0,
        });
        chunks.push(chunk);
      }
      Ok(slates_archive::Node::File(laid))
    }
    _ => Ok(slates_archive::Node::File(Vec::new())),
  }
}

/// A file of `bytes` from offset zero, one chunk (none when empty).
fn whole_file(
  bytes: Vec<u8>,
  chunks: &mut Vec<slates_archive::Chunk>,
) -> Vec<slates_archive::Extent> {
  if bytes.is_empty() {
    return Vec::new();
  }
  let chunk = slates_archive::Archive::raw_chunk(bytes);
  let extent = slates_archive::Extent {
    offset: 0,
    len: chunk.raw_len,
    chunk: chunk.identity,
    chunk_offset: 0,
  };
  chunks.push(chunk);
  vec![extent]
}

/// An inode's manifest metadata: its attributes with its kind's type bits, as the export writes them.
fn meta_of(inode: &InodeImage) -> slates_archive::NodeMeta {
  use slates_vfs::export::{
    MODE_DIRECTORY, MODE_FIFO, MODE_FILE, MODE_SOCKET, MODE_SYMLINK, permissions_of_mode,
  };
  use slates_vfs::recover::KindImage;
  let type_bits = match inode.kind {
    KindImage::File => MODE_FILE,
    KindImage::Dir => MODE_DIRECTORY,
    KindImage::Symlink => MODE_SYMLINK,
    KindImage::Fifo => MODE_FIFO,
    KindImage::Socket => MODE_SOCKET,
  };
  let attrs = &inode.attrs;
  slates_archive::NodeMeta {
    ino: inode.no,
    mode: permissions_of_mode(attrs.mode) | type_bits,
    atime_ns: attrs.atime,
    mtime_ns: attrs.mtime,
    ctime_ns: attrs.ctime,
    btime_ns: attrs.btime,
    size: attrs.size,
    nlink: attrs.nlink,
    uid: attrs.uid,
    gid: attrs.gid,
    xattrs: Vec::new(),
  }
}
