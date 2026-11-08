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
//! **Every other durability claim waits for the same position.** A transport's sync reply is held on [`durability`]
//! (FUSE `fsync`, a virtio-fs or vhost-user `FSYNC`, a WinFsp flush, a Shards stream's `FSYNC`) or [`wait_durable`] (an NFS stable write or `COMMIT`, v3 and v4 alike),
//! answered "not durable" past the liveness budget; a deferred verb that changes the volume (a writable map's
//! `MapSync`, a checkout) completes through [`defer`] as a recorded verb does.
//!
//! Not yet: a holder's replicas do not survive its own daemon's restart.

use std::collections::{BTreeMap, VecDeque};

use slates_db::live::{
  Admitted, Entry, Head, Install, LiveError, Next, Replica, Shipper, decode_reply, encode_reply,
};
use slates_db::register::{HostEpoch, HostId, ObjectId};
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
/// Counter: whole records appended to a live log, each a copy of the volume's content.
const LIVE_WHOLE: &str = "fleet.live.whole";
/// Counter: install images made from a publication's state (or a clean volume's, on the record period) for a holder
/// behind the retained tail, beside the log rather than on it ([`refresh_image`]).
const LIVE_IMAGE: &str = "fleet.live.install_image";
/// Counter: an install built to send a holder, the volume's whole state copied on the owner's shard.
const LIVE_INSTALL_BUILT: &str = "fleet.live.install_built";
/// Counter: an install a holder applied (its log replaced by the owner's whole state).
const LIVE_INSTALL_TAKEN: &str = "fleet.live.install_taken";
/// Counter: an install a watch ended before its deadline, its holder lost and silent.
const LIVE_INSTALL_CUT: &str = "fleet.live.install_cut";
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
/// The owner's side of a destroy (A-200): the destroyed volume's shipper goes, with the whole copy and the entries it
/// retained; every verb still waiting on its durability is refused `NotFound` (the volume is gone, and what it wrote
/// never will be durable), and every parked transport reply is woken to answer "not durable" ([`wait_durable`]). Kept
/// before, each destroyed live volume's shipper held a whole copy of it on its owner for good.
pub(crate) fn forget(state: &mut ShardState, id: slates_db::catalog::VolumeId) {
  if state.live.shipping.remove(&id.bytes).is_none() {
    return;
  }
  let gone = slates_ipc::protocol::ReplyBody::Refused {
    refusal: slates_ipc::protocol::Refusal::NotFound,
  };
  for entry in take_awaiting(state, id.bytes) {
    answer(state, entry, gone.clone());
  }
  wake_parked(state, id.bytes);
}

/// The sealing of the last whole entry of each live log this node holds for another owner: what a successor opens that
/// log through (`crate::seal_keys::sealed_under` keeps the pair keys these name).
pub(crate) fn held_sealings(
  state: &ShardState,
) -> impl Iterator<Item = crate::head::HeadSealing> + '_ {
  state.live.held.values().filter_map(|held| {
    held
      .entries
      .front()
      .and_then(|first| sealed_body(first))
      .and_then(|body| body.sealing)
  })
}

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

/// A sealed body: its pieces, each a sealed chunk's keyed name and bytes (`slates_cluster::sealed`), and on a whole entry
/// the volume's lineage key wrapped for each neighbour (the `sealing` a head carries, A-92 piece 4c), so a successor can
/// open the log whether or not the volume ever sealed a snapshot.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct SealedBody {
  /// The lineage key for successors, on a whole entry.
  sealing: Option<crate::head::HeadSealing>,
  /// The body's pieces in order ([`seal_body`]).
  pieces: Vec<SealedPiece>,
}

/// One piece of a sealed body: a sealed chunk of at most an archive chunk's bytes.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct SealedPiece {
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
  /// A holder behind the retained tail already holds the last whole state ([`pending_sends`]), so an install needs a
  /// newer one: made from the next publication's state, at that publication's head, beside the log ([`refresh_image`]).
  /// Unlike `owes_whole`, no write waits on it: the log itself is whole, only that holder is behind.
  owes_image: bool,
  /// The last whole state's payload and its head: the last whole entry, or an image a publication's state made at its
  /// own head ([`refresh_image`]). What an install sends.
  whole: Option<(Head, Vec<u8>)>,
  /// The anchors whose copy of the volume's lineage key the last whole entry carries (`None`: its body is in the
  /// clear, which any successor opens): a successor opens the log only through its own key entry there, so the log
  /// is durable only once that entry covers every remote candidate ([`covers_candidates`]).
  whole_anchors: Option<Vec<u64>>,
  /// The later epoch a holder answered with: this owner was superseded (a takeover placed the volume elsewhere), ships
  /// no more of it, and refuses every write waiting on it or made after (`Refusal::StaleEpoch`). Kept, not dropped with
  /// the shipper: dropped, the writes waiting on it were never answered (the resumed old owner held one for the fence
  /// flow's whole 40 s wait), and a stable NFS write parked there read "no shipper" as nothing to wait for.
  fenced: Option<HostEpoch>,
  /// The bytes a candidate keeps of this log: the last whole entry and every entry since ([`keep`]). Once it reaches
  /// the volume's size the next publication ships whole, which clears them ([`kept_reaches_size`]).
  kept: usize,
  /// The holders a lane is shipping to now, each with when it began ([`dispatch`]): at most one lane a holder, so the
  /// messages a holder lacks queue here, not as tasks waiting for its one session.
  lanes: BTreeMap<HostId, u64>,
  /// The holders whose last lane ended unanswered: nothing is built for them until the record period retries or their
  /// session is back ([`holder_reached`]). A publication built their messages anyway, and the lane ended at once for want
  /// of a session, so it was never busy: an install a write, a whole-volume clone on this shard each (a median of 2,396
  /// and 2,769 a run, 10.9 GB, against about 3.5 served, `live_chaos`'s rolling partition, diag-live-durability-54).
  unreachable: Vec<HostId>,
  /// For each holder, the image head of the last install a watch cut short ([`watch_install`]). One entry a holder it ships
  /// to: kept only for its targets ([`pending_sends`]), so at most the holders and the transition's target.
  cut_installs: BTreeMap<HostId, Head>,
  /// The holders known to hold a version of the volume's catalog, which a holder's bound is derived from
  /// ([`hold_bound`]): every holder any of the catalog's placements named, kept. Bounded by the candidates.
  catalogued: Vec<HostId>,
  /// The catalog version at which this shipper's epoch and holders were recorded (A-240, [`holders_placed`]), or `None`
  /// until the record is made. Nothing ships before that version is placed at `f + 1`, so a takeover's phase one always
  /// knows which hosts hold the log and counts its quorum among them alone.
  recorded: Option<u64>,
  /// A transition target the catalog has been asked to record, and the catalog version that records it: the shipper
  /// activates it once that version is placed ([`follow_candidates`]).
  recording: Option<(u64, Vec<HostId>)>,
  /// The head of the last whole entry on the log: what A-240's checks read ([`target_recordable`],
  /// [`covers_candidates`]). Unlike [`LiveVolume::whole`], never an install image made beside the log.
  whole_entry: Option<Head>,
  /// The anchors the install image now in [`LiveVolume::whole`] is sealed for, when that is an image made beside the log
  /// ([`refresh_image`]); `None` while it is the last whole entry. A holder installed from the image keeps it as its log's
  /// first payload, so a successor adopting that holder must open it: [`covers_candidates`] judges it as well as the
  /// whole entry. Pairs only accumulate, but one retired between the entry and the image (a node back at its anchor)
  /// could leave the image short of a key the entry's check passed.
  image_anchors: Option<Vec<u64>>,
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
  /// Destroys whose reply waits for their volume's tombstone to be placed at `f + 1` ([`defer_destroy`]): by volume, the
  /// tombstone's sequence and the verbs. Any volume's, live-shipped or not. Bounded with [`LiveState::awaiting`].
  destroys: BTreeMap<[u8; 16], (u64, Vec<Awaiting>)>,
  /// The waiting destroys by request, so a retry joins its wait ([`join`]).
  destroys_by_request: BTreeMap<(u64, u32, u32), [u8; 16]>,
  /// What the transport barrier that just ran on this shard published of a live volume, for its caller to wait on
  /// ([`note_barrier`], [`take_barrier`]): set and taken within one synchronous serve.
  barrier: Option<(slates_db::catalog::VolumeId, u64)>,
  /// The transport replies parked until a volume is durable, by volume: their wakers, woken as it advances.
  parked: BTreeMap<[u8; 16], Vec<std::task::Waker>>,
  /// On the control shard: dispatches whose holders were still in flight at their return, each with the owner shard and
  /// the volume, holding those holders' sessions until they return ([`drain_stragglers`]). Bounded by the sessions: each
  /// holds at least one.
  stragglers: Vec<(u16, ObjectId, slates_cluster::Stragglers)>,
  /// The head each taken-over live volume's rebuild was last refused at, and why ([`owes_refetch`]), on the control
  /// shard: its log is not fetched again until phase one adopts another head. One entry a pending materialization, removed
  /// when it is served.
  pub(crate) refused_rebuilds: BTreeMap<ObjectId, (Head, &'static str)>,
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
    let settled = state::with_state(|s| durability(s, volume, target, Some(cx.waker())))
      .unwrap_or(Durability::Never);
    if settled == Durability::Pending {
      std::task::Poll::Pending
    } else {
      std::task::Poll::Ready(())
    }
  });
  let durable =
    matches!(
      slates_rt::futures::within(DURABLE_WAIT_NS, parked).await,
      Ok(Some(()))
    ) && state::with_state(|s| durability(s, volume, target, None) == Durability::Durable)
      .unwrap_or(false);
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

/// Where a reply waiting for a live volume's log to be durable at a position stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durability {
  /// The owner and `f` candidates hold it (or the volume is no longer live-shipped).
  Durable,
  /// Not yet: the waker given was parked on the volume, woken as its log advances.
  Pending,
  /// It never will be here: the volume was destroyed, or this owner was superseded.
  Never,
}

/// Whether `volume`'s log is durable at `target` on this (its owner) shard; when it is not yet and `waker` is given,
/// parks it on the volume. Shared by every transport that holds a reply for durability: an NFS stable write or `COMMIT`
/// and a Shards stream's `FSYNC` ([`wait_durable`]), a FUSE `fsync` (`crate::fuse`) and a virtio-fs `FSYNC`
/// (`crate::virtiofs`, `DurableWait`, Unix only).
pub(crate) fn durability(
  state: &mut ShardState,
  volume: slates_db::catalog::VolumeId,
  target: u64,
  waker: Option<&std::task::Waker>,
) -> Durability {
  if !state.by_id.contains_key(&volume) || fenced(state, volume.bytes).is_some() {
    // Destroyed while it waited ([`forget`]), or superseded: nothing of it becomes durable here.
    return Durability::Never;
  }
  let durable = head_placed(state, volume.bytes)
    && state.live.shipping.get(&volume.bytes).is_none_or(|live| {
      !live.owes_whole && answerable(state, volume.bytes).is_some_and(|durable| durable >= target)
    });
  if durable {
    return Durability::Durable;
  }
  if let Some(waker) = waker {
    done_or_park(state, volume.bytes, false, waker);
  }
  Durability::Pending
}

/// Derived: how long a held reply waits for durability before its transport answers that it is not (NFS `JUKEBOX`,
/// FUSE and virtio-fs `EIO`, WinFsp `STATUS_UNEXPECTED_IO_ERROR`): [`DURABLE_WAIT_NS`]. Asked by those transports, so
/// compiled where they are.
pub(crate) const fn durable_wait_ns() -> u64 {
  DURABLE_WAIT_NS
}

/// A wait on this (the volume's owner) shard for `volume`'s log to be durable at `target`, or never to be ([`durability`]):
/// ready then, or once the shard's state is out of reach (its shutdown). Parked on the volume as every held reply is, so
/// a wait dropped early leaves at most a waker the volume's next advance wakes and drops ([`wake_parked`]). The virtio-fs
/// loop's wait ([`crate::virtiofs`]).
#[cfg(unix)]
pub(crate) struct DurableWait {
  volume: slates_db::catalog::VolumeId,
  target: u64,
}

#[cfg(unix)]
impl DurableWait {
  /// The wait for `volume`'s log at `target`.
  pub(crate) fn new(volume: slates_db::catalog::VolumeId, target: u64) -> DurableWait {
    DurableWait { volume, target }
  }
}

#[cfg(unix)]
impl std::future::Future for DurableWait {
  type Output = ();

  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
    let (volume, target) = (self.volume, self.target);
    match state::with_state(|s| durability(s, volume, target, Some(cx.waker()))) {
      Some(Durability::Pending) => std::task::Poll::Pending,
      Some(Durability::Durable | Durability::Never) | None => std::task::Poll::Ready(()),
    }
  }
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
      .is_some_and(|head| head.placed_under(configuration, ObjectId(volume)))
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

/// Derived: the replies one FUSE or WinFsp mount may hold for durability: the bound a shard's held verbs use
/// ([`awaiting_bound`]). Compiled where those transports are, its only callers.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn reply_hold_bound(state: &ShardState) -> usize {
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
  if live.fenced.is_some() {
    // Nothing this owner holds becomes durable again: the caller defers, and the deferral refuses.
    return Some(head.unwrap_or(0));
  }
  if !head_placed(state, volume.bytes) {
    // The fleet does not know the volume yet: a takeover could not find it, so nothing written to it is durable.
    return head;
  }
  let target = if live.owes_whole {
    head.map_or(Some(0), |head| head.checked_add(1))?
  } else {
    head?
  };
  if !live.owes_whole && answerable(state, volume.bytes).is_some_and(|durable| durable >= target) {
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
  if let Some(current) = fenced(state, volume.bytes) {
    state.count(LIVE_FENCED, 1);
    return Some(stale_epoch(current));
  }
  if held_replies(state) >= awaiting_bound(state) {
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
  if let Some(volume) = state
    .live
    .destroys_by_request
    .get(&(origin, id.client, id.sequence))
    .copied()
  {
    let route = state.reply_route.take();
    if let (Some((_, waiting)), Some(route)) = (state.live.destroys.get_mut(&volume), route)
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
    return true;
  }
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
  let Some(durable) = answerable(state, volume) else {
    count_held_back(state, volume);
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
      let reply = entry.reply.clone();
      answer(state, entry, reply);
    }
  }
}

/// The replies this shard holds for durability or a tombstone's placement, which [`awaiting_bound`] bounds together.
fn held_replies(state: &ShardState) -> usize {
  let live: usize = state.live.awaiting.values().map(Vec::len).sum();
  let destroys: usize = state
    .live
    .destroys
    .values()
    .map(|(_, waiting)| waiting.len())
    .sum();
  live.saturating_add(destroys)
}

/// The tombstone sequence a destroy of `volume` that just committed must see placed at `f + 1` before it answers, or
/// `None` when it need not wait: at `f = 0` (a laptop's tombstone retires with its destroy, R8), or when the volume has
/// no tombstone to place. Until its tombstone is region-placed a destroy is not durable: a takeover after the owner's
/// loss adopts the last live head and serves the volume again (§4.4 destroy, AUD-29-43), so a destroy answered before
/// then told its client what the takeover undid (2026-11-06, `crates/cli/tests/live_chaos.rs`: both candidates stopped,
/// the destroy answered done, the owner killed, a survivor listed the volume and served its file).
pub(crate) fn tombstone_target(
  state: &ShardState,
  volume: slates_db::catalog::VolumeId,
) -> Option<u64> {
  let sequence = state.db.partition().tombstone_sequence(volume)?;
  (!tombstone_placed(state, volume.bytes, sequence)).then_some(sequence)
}

/// Whether `volume`'s register is region-placed at `sequence` or past it (always at `f = 0`).
fn tombstone_placed(state: &ShardState, volume: [u8; 16], sequence: u64) -> bool {
  let configuration = state.fleet.configuration();
  configuration.quorum.f == 0
    || state
      .placed_heads
      .get(&ObjectId(volume))
      .is_some_and(|head| {
        head.sequence >= sequence && head.placed_under(configuration, ObjectId(volume))
      })
}

/// Holds a destroy's reply until its volume's tombstone is placed at `f + 1` ([`tombstone_target`]): its effects are
/// committed; its completion is recorded and its reply delivered then ([`resolve_destroys`]): `None`. `Overloaded` now
/// instead (counted, nothing recorded, so a retry runs the verb again) at the shard's bound of held replies.
pub(crate) fn defer_destroy(
  state: &mut ShardState,
  (volume, sequence): (slates_db::catalog::VolumeId, u64),
  (origin, id): (u64, slates_wire::request::RequestId),
  route: Option<crate::merge_service::ReplyRoute>,
  reply: slates_ipc::protocol::ReplyBody,
) -> Option<slates_ipc::protocol::ReplyBody> {
  if held_replies(state) >= awaiting_bound(state) {
    state.count(LIVE_OVER_BOUND, 1);
    return Some(slates_ipc::protocol::ReplyBody::Refused {
      refusal: slates_ipc::protocol::Refusal::Overloaded { shard: state.shard },
    });
  }
  let (target, waiting) = state
    .live
    .destroys
    .entry(volume.bytes)
    .or_insert_with(|| (sequence, Vec::new()));
  *target = (*target).max(sequence);
  waiting.push(Awaiting {
    route,
    origin,
    id,
    reply,
  });
  state
    .live
    .destroys_by_request
    .insert((origin, id.client, id.sequence), volume.bytes);
  state.count(DESTROY_AWAITED, 1);
  None
}

/// Answers the destroys waiting on `volume` once its tombstone is region-placed ([`defer_destroy`]).
pub(crate) fn resolve_destroys(state: &mut ShardState, volume: [u8; 16]) {
  let Some((sequence, _)) = state.live.destroys.get(&volume) else {
    return;
  };
  if !tombstone_placed(state, volume, *sequence) {
    return;
  }
  let Some((_, waiting)) = state.live.destroys.remove(&volume) else {
    return;
  };
  for entry in waiting {
    let reply = entry.reply.clone();
    answer(state, entry, reply);
  }
}

/// Counter: destroys whose reply waited for their tombstone's placement at `f + 1`.
const DESTROY_AWAITED: &str = "fleet.destroy.awaited";

/// Refuses every verb waiting on `volume` `StaleEpoch` once a holder has fenced its owner at `current`: each refusal is
/// recorded as the verb's completion, so a retry meets it, and delivered as a resolved reply is.
fn refuse_awaiting(state: &mut ShardState, volume: [u8; 16], current: HostEpoch) {
  let destroys = state
    .live
    .destroys
    .remove(&volume)
    .map(|(_, waiting)| waiting)
    .unwrap_or_default();
  for entry in take_awaiting(state, volume).into_iter().chain(destroys) {
    state.count(LIVE_FENCED, 1);
    answer(state, entry, stale_epoch(current));
  }
}

/// Takes every verb waiting on `volume`, at any position.
fn take_awaiting(state: &mut ShardState, volume: [u8; 16]) -> Vec<Awaiting> {
  let keys: Vec<([u8; 16], u64)> = state
    .live
    .awaiting
    .range((volume, 0)..=(volume, u64::MAX))
    .map(|(key, _)| *key)
    .collect();
  keys
    .into_iter()
    .filter_map(|key| state.live.awaiting.remove(&key))
    .flatten()
    .collect()
}

/// The refusal a superseded owner gives a write to the volume it lost.
fn stale_epoch(current: HostEpoch) -> slates_ipc::protocol::ReplyBody {
  slates_ipc::protocol::ReplyBody::Refused {
    refusal: slates_ipc::protocol::Refusal::StaleEpoch { current: current.0 },
  }
}

/// The epoch `volume`'s owner here was fenced at, if it was.
fn fenced(state: &ShardState, volume: [u8; 16]) -> Option<HostEpoch> {
  state
    .live
    .shipping
    .get(&volume)
    .and_then(|live| live.fenced)
}

/// Counter: a write refused `StaleEpoch` because its owner here was superseded.
const LIVE_FENCED: &str = "fleet.live.fenced";

/// Answers one waiting verb with `reply`: its completion recorded on this partition (one durable step, so a retry after
/// meets it) and the reply delivered to its client's shard.
fn answer(state: &mut ShardState, entry: Awaiting, reply: slates_ipc::protocol::ReplyBody) {
  state
    .live
    .awaiting_by_request
    .remove(&(entry.origin, entry.id.client, entry.id.sequence));
  state
    .live
    .destroys_by_request
    .remove(&(entry.origin, entry.id.client, entry.id.sequence));
  state.db.begin();
  let recorded = crate::verbs::record_completion(state, entry.origin, entry.id, reply);
  if state.db.commit(&mut state.segment).is_err() {
    state.count(LIVE_UNRECORDED, 1);
    return;
  }
  state.count(LIVE_RESOLVED, 1);
  let Some(route) = entry.route else {
    return;
  };
  if crate::xshard::hand_reply_home(route.shard, move || {
    crate::state::deliver(route.client_index, route.request, recorded, true);
  })
  .is_err()
  {
    state.count(LIVE_UNDELIVERED, 1);
  }
}

/// Keeps, of the live records taken for a committed checkpoint (`verbs::capture_live_changes`), those of the volumes it
/// carried: a volume it left out stays dirty and publishes again, record and all.
pub(crate) fn note_checkpoint(state: &mut ShardState, volumes: &[slates_db::catalog::VolumeId]) {
  state.live.captured.retain(|(id, _)| volumes.contains(id));
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
  /// For an install, the image head it carries.
  image: Option<Head>,
  /// Whether its exchange may end once the holder is lost and silent ([`slates_cluster::live::Watch`]): every install
  /// but a re-send of one a watch already cut ([`LiveVolume::cut_installs`]).
  watched: bool,
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
  if fenced(state, id.bytes).is_some() {
    // Superseded here: nothing more ships, and the write that published this is refused (`defer`).
    return;
  }
  let object = ObjectId(id.bytes);
  let configuration = state.fleet.configuration();
  let local = configuration.owner;
  // A new shipper starts from the holder set the volume's catalog records, less this node, when it records one: a
  // successor's is the set its takeover counted (the dead owner's record, kept by `verbs::materialize_taken_over`), and
  // an owner restarted at its epoch finds its own. Those hosts hold the log; the configuration's candidates may be a set
  // the old owner was still moving to, holding none of it, and recorded at once they would leave the next takeover
  // counting them. The shipper moves to the configuration by a transition of its own ([`follow_candidates`]). With no
  // record, nothing was ever shipped, and it starts from the configuration.
  let recorded: Option<Vec<HostId>> = state
    .db
    .partition()
    .volume(id)
    .and_then(|volume| volume.policy.live_holders.as_ref())
    .map(|recorded| {
      recorded
        .holders
        .iter()
        .copied()
        .map(HostId)
        .filter(|host| *host != local)
        .collect()
    });
  let holders: Vec<HostId> = recorded.unwrap_or_else(|| {
    configuration
      .place(object)
      .candidates
      .into_iter()
      .filter(|host| *host != local)
      .collect()
  });
  // The volume's epoch is its head's: a successor ships under the epoch its takeover placed the head at, which is above
  // the dead owner's, whatever its own host's epoch (heads use the same rule, `fleet::owed_record`).
  let placed = state.placed_heads.get(&object).map(|placed| placed.epoch);
  let epoch = placed.map_or(configuration.host_epoch, |placed| {
    placed.max(configuration.host_epoch)
  });
  let quorum = configuration.quorum;
  let reaches_size = kept_reaches_size(state, id);
  state
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
      owes_image: false,
      whole: None,
      whole_anchors: None,
      fenced: None,
      kept: 0,
      lanes: BTreeMap::new(),
      unreachable: Vec::new(),
      cut_installs: BTreeMap::new(),
      catalogued: Vec::new(),
      recorded: None,
      recording: None,
      whole_entry: None,
      image_anchors: None,
    });
  follow_candidates(state, id.bytes);
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return;
  };
  let whole = live.owes_whole || record.is_none() || reaches_size;
  let built = if whole {
    whole_record(state, id)
  } else {
    record.map(<[u8]>::to_vec)
  };
  let Some(record) = built else {
    refuse(state, id, "fleet.live.refused.no_record");
    return;
  };
  let Some((payload, anchors)) = payload(state, id, whole, record) else {
    refuse(state, id, "fleet.live.refused.no_payload");
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
      live.kept = if whole {
        payload.len()
      } else {
        live.kept.saturating_add(payload.len())
      };
      let image_owed = live.owes_image && !whole;
      if whole {
        let head = entry.head();
        live.shipper.mark_checkpoint(head);
        // Derived: the retained tail's byte bound is this whole record's size, the install a laggard would be sent
        // instead: shipping it the entries it lacks never costs more than installing it (`Shipper::retain_up_to`).
        live.shipper.retain_up_to(payload.len());
        live.whole = Some((head, payload));
        live.owes_image = false;
        live.whole_anchors = anchors;
        live.whole_entry = Some(head);
        live.image_anchors = None;
      }
      state.count(LIVE_APPENDED, 1);
      if whole {
        state.count(LIVE_WHOLE, 1);
      }
      if image_owed {
        // The volume's state now is this publication's, so its image is exactly the head just appended.
        refresh_image(state, id);
      }
      resolve(state, id.bytes);
      let sends = pending_sends(state, id.bytes);
      dispatch(state, sends);
    }
    Err(_) => refuse(state, id, "fleet.live.refused.no_credit"),
  }
}

/// Whether what a candidate keeps of `id`'s log has reached the volume's size, so its next publication ships whole: a
/// candidate keeps a whole entry and the entries since, within twice the volume's size ([`hold_bound`]), and a whole
/// entry clears them. Shipped at the volume's size, the entry that crosses it still fits under the bound whatever it
/// is, since no publication is larger than the volume. Never the case for a volume this shard does not know.
fn kept_reaches_size(state: &ShardState, id: slates_db::catalog::VolumeId) -> bool {
  let Some(kept) = state.live.shipping.get(&id.bytes).map(|live| live.kept) else {
    return false;
  };
  let Some(volume) = state.db.partition().volume(id) else {
    return false;
  };
  let size = match volume.policy.size {
    slates_db::catalog::SizeClass::Bounded { limit } => limit,
    slates_db::catalog::SizeClass::Dynamic { max } => max,
  };
  u64::try_from(kept).unwrap_or(u64::MAX) >= size
}

/// Makes the install image a holder behind the retained tail is owed ([`LiveVolume::owes_image`]): the volume's whole
/// state, sealed as a whole payload, at the shipper's head, which it must equal, so this runs only where the volume's
/// state is its head's: right after a publication is appended ([`published`]), or on the record period for a volume with
/// nothing unpublished ([`period`]). The log is untouched: a holder on the chain goes on taking the changes, and only the
/// holder behind is installed. Until 2026-11-07 such a holder set `owes_whole`, so the next write's record carried the
/// whole volume to every holder and every write behind it waited for the up-to-date candidate to take it: 20–45 MB a
/// time, 100–600 ms each, and writes of 1.2–1.9 s (CI, `diag-live-durability-14`). The retained tail's byte bound
/// follows the image (`Shipper::retain_up_to`), so a laggard is sent the entries it lacks while they cost less than
/// installing the volume as it is now, not as it was at its last whole record. A volume whose image cannot be made
/// stays owing it, and the next publication or period tries again.
fn refresh_image(state: &mut ShardState, id: slates_db::catalog::VolumeId) {
  // A volume owing a whole record lost a change from its log (a refused publication): its state is past its head, so
  // an image now would not be the head's. The whole record that follows is the newer state an install sends.
  if state
    .live
    .shipping
    .get(&id.bytes)
    .is_none_or(|live| live.owes_whole)
  {
    return;
  }
  let Some(record) = whole_record(state, id) else {
    return;
  };
  let Some((image, anchors)) = payload(state, id, true, record) else {
    return;
  };
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return;
  };
  let Some(head) = live.shipper.head() else {
    return;
  };
  live.shipper.mark_checkpoint(head);
  live.shipper.retain_up_to(image.len());
  live.whole = Some((head, image));
  // The entry's anchors stay what A-240 judges the log by (`whole_anchors`); the image's are judged beside them.
  // An image sealed under no keys (a volume shipped in the clear) has none to judge.
  live.image_anchors = anchors;
  live.owes_image = false;
  state.count(LIVE_IMAGE, 1);
}

/// Whether `id`'s volume has nothing unpublished: its state is the one its live log's head records.
fn published_whole(state: &ShardState, id: slates_db::catalog::VolumeId) -> bool {
  state
    .by_id
    .get(&id)
    .and_then(|handle| state.volumes.get(*handle).ok())
    .is_some_and(|slot| slot.volume.is_clean(&state.store))
}

/// Moves `volume`'s log to its other candidates as this shard's configuration names them now, by a joint transition
/// (`Shipper::begin_transition`, counted `fleet.live.holders_transition`): the shipper ships to the old and the new set
/// and counts a write durable only once `f` of each hold it, and the new set becomes the holders once it holds every
/// durable entry (`Shipper::activate`, counted `fleet.live.holders_activated`). The configuration grows while a fleet
/// forms (a bootstrap node names only itself until the others are admitted) and moves at a takeover or a neighbourhood
/// change; a shipper that kept the holders of its first publication shipped to nobody when that set was empty (the
/// Linux conformance pair, diag run 42325923902: head=2 durable=none for a whole fsx run, nothing sent to either
/// candidate), and one that replaced them outright could leave a takeover counting a set that held none of the durable
/// writes (the model, `slates_db`'s `tests/live.rs`).
///
/// The record the activation waits on is A-240's: once [`target_recordable`] holds, the target is committed to the
/// volume's catalog as its holder set (`Op::LiveHoldersCommitted`), and the shipper activates it once that catalog
/// version is placed at `f + 1`, so a takeover's phase one never counts a set the shipper does not count durability
/// over.
fn follow_candidates(state: &mut ShardState, volume: [u8; 16]) {
  let configuration = state.fleet.configuration();
  let local = configuration.owner;
  let candidates: Vec<HostId> = configuration
    .place(ObjectId(volume))
    .candidates
    .into_iter()
    .filter(|host| *host != local)
    .collect();
  let Some(live) = state.live.shipping.get_mut(&volume) else {
    return;
  };
  let began = live.shipper.begin_transition(&candidates);
  if began {
    state.count(LIVE_HOLDERS_TRANSITION, 1);
  }
  if !target_recordable(state, volume) {
    // A transition whose target cannot be recorded yet (short of the holders its count needs, or of a whole entry they
    // can open) holds every write on the joint count: named each period, as `answerable`'s reasons name the active
    // set's (raft's review, 2026-11-07).
    if state
      .live
      .shipping
      .get(&volume)
      .is_some_and(|live| live.shipper.transition().is_some())
    {
      state.count(LIVE_HOLDERS_SHORT, 1);
    }
    return;
  }
  let Some(live) = state.live.shipping.get(&volume) else {
    return;
  };
  let Some(target) = live.shipper.transition().map(<[HostId]>::to_vec) else {
    return;
  };
  match live.recording.clone() {
    Some((version, named)) if named == target => {
      if !catalog_placed_at(state, volume, version) {
        return;
      }
      let activated = state.live.shipping.get_mut(&volume).is_some_and(|live| {
        let activated = live.shipper.activate(&named);
        if activated {
          live.recorded = Some(version);
          live.recording = None;
        }
        activated
      });
      if activated {
        state.count(LIVE_HOLDERS_ACTIVATED, 1);
      }
    }
    // The target is kept from here until it is activated (`Shipper::begin_transition`), so a record made for it stays
    // the one to wait on; a refused record is made again at the next period.
    _ => {
      if let Some(version) = commit_holders(state, volume, &target)
        && let Some(live) = state.live.shipping.get_mut(&volume)
      {
        live.recording = Some((version, target));
      }
    }
  }
}

/// Whether the record may name `volume`'s transition target: the target holds every durable write
/// (`Shipper::transferred`), and, once anything is durable, the last whole entry carries a key for every candidate
/// ([`covers_candidates`]) and is itself durable, so `f` of the target hold a whole entry they can open. A target that
/// held the log under a key it lacked could not serve it: raft-chaos saw a successor outside the shipper's holders
/// refuse every rebuild for want of the lineage key (`fleet.live.takeover_refused.open` 814, CI diag-raft-chaos-kind-14,
/// 2026-11-07). The period checkpoints the log again once this shard has delivered the missing pairs ([`period`]).
fn target_recordable(state: &ShardState, volume: [u8; 16]) -> bool {
  let Some(live) = state.live.shipping.get(&volume) else {
    return false;
  };
  if !live.shipper.transferred() {
    return false;
  }
  let Some(durable) = live.shipper.durable() else {
    return true;
  };
  let whole = live.whole_entry.map(|head| head.position);
  covers_candidates(state, volume) && whole.is_some_and(|whole| whole <= durable)
}

/// Counter: a live volume began moving its holders to a changed set of candidates.
const LIVE_HOLDERS_TRANSITION: &str = "fleet.live.holders_transition";
/// Counter: a record period in which a live volume's transition target could not be recorded yet.
const LIVE_HOLDERS_SHORT: &str = "fleet.live.holders_short";
/// Counter: a live volume's new holder set held every durable entry and became its holders.
const LIVE_HOLDERS_ACTIVATED: &str = "fleet.live.holders_activated";

/// A publication that could not become an entry: counted, and the volume owes a whole record next.
fn refuse(state: &mut ShardState, id: slates_db::catalog::VolumeId, why: &'static str) {
  state.count(LIVE_REFUSED, 1);
  state.count(why, 1);
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
/// under the volume's lineage key where this node seals; and, for a sealed whole entry, the anchors its copies of the
/// lineage key are wrapped for (`None` for a clear body).
fn payload(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  whole: bool,
  record: Vec<u8>,
) -> Option<(Vec<u8>, Option<Vec<u64>>)> {
  let runs = content_runs(state, id, &record)?;
  let body = LivePublication { record, runs }.to_bytes();
  let kind = if whole { KIND_WHOLE } else { KIND_DELTA };
  if state.seal_root.is_none() {
    let mut payload = vec![kind, BODY_CLEAR];
    payload.extend_from_slice(&body);
    return Some((payload, None));
  }
  let account = crate::seal_keys::tenant_of(&state.db.partition().volume(id)?.owner);
  let lineage = crate::seal_keys::lineage(state, id, account).ok()?;
  let namer = crate::seal_keys::namer(state, id, account).ok()?;
  let segment = u32::try_from(state.config.page).ok()?;
  let pieces = seal_body(&lineage, &namer, body, segment)?;
  let sealing = if whole {
    let owner = state.db.partition().volume(id)?.owner.clone();
    crate::seal_keys::head_sealing(state, id, &owner)
  } else {
    None
  };
  let anchors = sealing.as_ref().map_or_else(Vec::new, |sealing| {
    sealing.keys.iter().map(|key| key.anchor).collect()
  });
  let mut payload = vec![kind, BODY_SEALED];
  payload.extend_from_slice(&SealedBody { sealing, pieces }.to_bytes());
  Some((payload, Some(anchors)))
}

/// Whether a successor among `volume`'s remote candidates (those still in the neighbourhood) can open its log: its
/// last whole entry is in the clear, or carries a copy of the lineage key for each one's anchor (A-92 piece 4c), as a
/// sealed head must before it ships (`fleet::sealing_covers`). Every candidate at once, since any may succeed. A whole
/// entry sealed before a candidate's pair was delivered (the candidate stopped as the volume was made) carried none for
/// it, and that candidate, the successor, refused the rebuild 145 times and never served the volume (2026-11-05;
/// `a_live_volume_made_while_its_successor_was_stopped_is_served_by_it` in `crates/cli/tests/live_fleet.rs`).
fn covers_candidates(state: &ShardState, volume: [u8; 16]) -> bool {
  let Some(live) = state.live.shipping.get(&volume) else {
    return true;
  };
  let wanted: Vec<Option<u64>> = remote_candidates(state, volume)
    .map(|candidate| state.member_anchors.get(&candidate).copied())
    .collect();
  log_covers(
    &wanted,
    live.whole_entry.is_some(),
    live.whole_anchors.as_deref(),
    live.image_anchors.as_deref(),
  )
}

/// Whether a successor among the candidates could open the log, given each remote candidate's anchor (`wanted`; `None`
/// for one this shard knows none for): the last whole entry's anchors must hold a key for every one, and so must the
/// install image's when there is one, since a holder installed from it keeps it as its log's first payload. With no
/// anchors recorded (a log shipped in the clear), a whole entry is enough. Before, only the entry was judged, and a pair
/// retired between the entry and an image could leave the image short of a key the entry's check passed.
fn log_covers(
  wanted: &[Option<u64>],
  has_whole_entry: bool,
  whole: Option<&[u64]>,
  image: Option<&[u64]>,
) -> bool {
  let covers = |anchors: &[u64]| {
    wanted
      .iter()
      .all(|anchor| anchor.is_some_and(|anchor| anchors.contains(&anchor)))
  };
  let image_covers = image.is_none_or(covers);
  match whole {
    None => has_whole_entry && image_covers,
    Some(anchors) => covers(anchors) && image_covers,
  }
}

/// `volume`'s candidates other than this node that are still in its neighbourhood.
fn remote_candidates(state: &ShardState, volume: [u8; 16]) -> impl Iterator<Item = HostId> + '_ {
  let configuration = state.fleet.configuration();
  let local = configuration.owner;
  configuration
    .place(ObjectId(volume))
    .candidates
    .into_iter()
    .filter(move |candidate| *candidate != local && configuration.neighbourhood.contains(candidate))
}

/// Whether a whole entry made now would cover every remote candidate that the last one does not: this shard has
/// delivered its pair to each one's anchor (`seal_keys::head_sealing` wraps the key for exactly those). The period
/// then owes one ([`period`]), so the log is checkpointed again once the pairs are there.
fn coverable_now(state: &ShardState, volume: [u8; 16]) -> bool {
  remote_candidates(state, volume).all(|candidate| {
    state
      .member_anchors
      .get(&candidate)
      .is_some_and(|anchor| state.pairs_delivered.contains_key(anchor))
  })
}

/// The position up to which `volume`'s log is durable for its writers here: the shipper's durable position, once the
/// fleet knows the volume ([`head_placed`]), the holder set its durability counts is recorded in the catalog at
/// `f + 1` ([`active_set_recorded`], A-240), and a successor could open the log at it ([`covers_candidates`]: the last
/// whole entry covers every candidate, and is itself durable). `None` until then. The record gate also holds by
/// construction (nothing ships before it, `pending_sends`); it is stated here, where a write is answered, so a later
/// change that ships first cannot answer under an unrecorded set unnoticed (raft's review, 2026-11-07).
fn answerable(state: &ShardState, volume: [u8; 16]) -> Option<u64> {
  let live = state.live.shipping.get(&volume)?;
  let durable = live.shipper.durable()?;
  if !head_placed(state, volume)
    || !active_set_recorded(state, volume)
    || !covers_candidates(state, volume)
  {
    return None;
  }
  let whole = live.whole_entry.map(|head| head.position)?;
  (durable >= whole).then_some(durable)
}

/// Counts why `volume`'s log, durable on its holders, does not yet answer its writers ([`answerable`]): the fleet does not
/// know the volume yet (`placement`), its last whole entry does not carry every candidate's key (`keys`), or that entry
/// is not itself durable yet (`whole`). Nothing is counted while the shipper holds nothing durable.
fn count_held_back(state: &mut ShardState, volume: [u8; 16]) {
  let Some(live) = state.live.shipping.get(&volume) else {
    return;
  };
  let Some(durable) = live.shipper.durable() else {
    return;
  };
  let whole = live.whole_entry.map(|head| head.position);
  let why = if !head_placed(state, volume) {
    LIVE_HELD_BACK_PLACEMENT
  } else if !active_set_recorded(state, volume) {
    LIVE_HELD_BACK_HOLDERS
  } else if !covers_candidates(state, volume) {
    LIVE_HELD_BACK_KEYS
  } else if whole.is_none_or(|whole| durable < whole) {
    LIVE_HELD_BACK_WHOLE
  } else {
    return;
  };
  state.count(why, 1);
}

/// Counter: a durable live log whose writers wait because the fleet does not know the volume yet (its head unplaced).
const LIVE_HELD_BACK_PLACEMENT: &str = "fleet.live.held_back.placement";
/// Counter: a durable live log whose writers wait because the holder set its durability counts is not yet recorded in
/// the volume's catalog at `f + 1` (A-240).
const LIVE_HELD_BACK_HOLDERS: &str = "fleet.live.held_back.holders";

/// Whether the holder set `volume`'s shipper counts durability over is recorded in its catalog and that version is placed
/// at `f + 1` (A-240; at `f = 0` the owner's commit places it, [`catalog_placed_at`]).
fn active_set_recorded(state: &ShardState, volume: [u8; 16]) -> bool {
  state
    .live
    .shipping
    .get(&volume)
    .and_then(|live| live.recorded)
    .is_some_and(|version| catalog_placed_at(state, volume, version))
}

/// Counter: a durable live log whose writers wait because its last whole entry lacks a candidate's key.
const LIVE_HELD_BACK_KEYS: &str = "fleet.live.held_back.keys";
/// Counter: a durable live log whose writers wait because its last whole entry is not durable yet.
const LIVE_HELD_BACK_WHOLE: &str = "fleet.live.held_back.whole";

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

/// Counter: a shipper's holder set could not be recorded in its volume's catalog (A-240); retried at its next send.
/// Format: a counter name in the daemon's status report.
const LIVE_HOLDERS_UNRECORDED: &str = "fleet.live.holders_unrecorded";
/// Counter: a shipper recorded its epoch and holders in its volume's catalog (A-240).
/// Format: a counter name in the daemon's status report.
const LIVE_HOLDERS_RECORDED: &str = "fleet.live.holders_recorded";

/// Whether `volume`'s shipper may ship (A-240): its epoch and holders are recorded in the volume's catalog and that
/// catalog version is placed at `f + 1`, as a head is (at `f = 0` the owner's own commit places it, [`head_placed`]).
/// Records them first when this shipper has not yet. A takeover's phase one counts its `f + 1` reports among the
/// holders the adopted catalog names; a shipper that shipped before its holders were placed could leave the log on
/// hosts no successor counts. In the model a fresh member counted in a holder's place let a stale head commit (10 of
/// 400 seeds, `crates/db/tests/live.rs`), and on KIND a holder kept fence 1 after a takeover at epoch 2 (runs
/// 42326682216, `wan-loss` and `ceiling`, and 42331191530, `ceiling`).
fn holders_placed(state: &mut ShardState, volume: [u8; 16]) -> bool {
  let Some(recorded) = state.live.shipping.get(&volume).map(|live| live.recorded) else {
    return false;
  };
  let Some(recorded) = recorded.or_else(|| record_holders(state, volume)) else {
    return false;
  };
  catalog_placed_at(state, volume, recorded)
}

/// Records `volume`'s shipper's epoch and holders in its catalog (`Op::LiveHoldersCommitted`, A-240) and returns the
/// catalog version that holds them; the record plane ships it as any catalog change (`fleet::unplaced_heads`). `None`
/// when the database refused the record, counted; the next send tries again.
fn record_holders(state: &mut ShardState, volume: [u8; 16]) -> Option<u64> {
  let holders = state
    .live
    .shipping
    .get(&volume)
    .map(|live| live.shipper.holders().to_vec())?;
  let version = commit_holders(state, volume, &holders)?;
  if let Some(live) = state.live.shipping.get_mut(&volume) {
    live.recorded = Some(version);
  }
  Some(version)
}

/// Commits `holders` as `volume`'s shipper's holder set under its epoch (`Op::LiveHoldersCommitted`, A-240) and returns
/// the catalog version that holds them; `None` when the database refused the record, counted.
fn commit_holders(state: &mut ShardState, volume: [u8; 16], holders: &[HostId]) -> Option<u64> {
  let id = slates_db::catalog::VolumeId { bytes: volume };
  let epoch = state.live.shipping.get(&volume)?.shipper.epoch().0;
  let holders = slates_db::catalog::LiveHolders {
    epoch,
    holders: holders.iter().map(|host| host.0).collect(),
  };
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  let op = slates_db::op::Op::LiveHoldersCommitted { id, holders };
  if state.db.mutate(&mut state.segment, &op, now).is_err() {
    state.count(LIVE_HOLDERS_UNRECORDED, 1);
    return None;
  }
  let version = state.db.partition().volume(id)?.catalog_version;
  state.count(LIVE_HOLDERS_RECORDED, 1);
  Some(version)
}

/// Whether `volume`'s catalog is placed at `f + 1` at `version` or later (at `f = 0`, always: the owner's commit places
/// it, R8), its acknowledgements judged under the configuration now (`PlacedHead::placed_under`). Judged under the
/// round's own joint cohorts, a bootstrap node's catalog kept its one-host cohort and never read placed, so the shipper
/// shipped nothing (the bootstrap-node live flow after A-240's merge: both candidates holding nothing, macOS CI
/// diag-live-fixes-17).
fn catalog_placed_at(state: &ShardState, volume: [u8; 16], version: u64) -> bool {
  let configuration = state.fleet.configuration();
  let catalog = ObjectId(volume).catalog();
  configuration.quorum.f == 0
    || state
      .placed_heads
      .get(&catalog)
      .is_some_and(|head| head.sequence >= version && head.placed_under(configuration, catalog))
}

/// A catalog register's placement moved (`fleet::record_acks_in`): when it is a live volume's and now places that
/// shipper's holders (A-240, [`holders_placed`]), ships what the holders lack at once, rather than at the next record
/// period, so the shipper's first entry waits one catalog round and no more.
pub(crate) fn catalog_placed(state: &mut ShardState, catalog: ObjectId) {
  if !catalog.is_catalog() {
    return;
  }
  let volume = catalog.placement_key().0;
  if !state.live.shipping.contains_key(&volume) {
    return;
  }
  let sends = pending_sends(state, volume);
  dispatch(state, sends);
}

/// What each holder of the volume lacks, as messages: entries, or an install from the last whole entry. An install
/// owed at a checkpoint the holder already holds cannot move it, so instead the volume is marked to owe a fresh whole
/// record (its next publication or the period appends one, and the install is made from that).
fn pending_sends(state: &mut ShardState, volume: [u8; 16]) -> Vec<Send> {
  if !holders_placed(state, volume) {
    return Vec::new();
  }
  let object = ObjectId(volume);
  let placed = catalogued_holders(state, object);
  let (now, expiry) = (slates_rt::futures::now_ns(), lane_expiry_ns());
  let Some(live) = state.live.shipping.get_mut(&volume) else {
    return Vec::new();
  };
  // Kept, not replaced: a round that placed the catalog at `f + 1` while a candidate was away names only the others,
  // and the candidate, which holds an earlier version, was shipped nothing more (a healed candidate left at 228 against
  // the owner's 554 for good, `live_chaos`, 2026-11-06).
  // Only holders this shipper ships to (its targets, during a transition too): the catalog's placements may name others,
  // whose acknowledgements say nothing about this log.
  let shippable: Vec<HostId> = live.shipper.targets().collect();
  for holder in placed {
    if shippable.contains(&holder) && !live.catalogued.contains(&holder) {
      live.catalogued.push(holder);
    }
  }
  let catalogued = live.catalogued.clone();
  // A holder a lane is shipping to is sent its messages when the lane ends ([`lane_ended`]): nothing is built for it
  // now, an install least of all (a clone of the whole volume each: 1,631 built in one run, 2.3 GB copied on the owner
  // shard, for lanes that sent none of them).
  let busy: Vec<HostId> = live
    .lanes
    .iter()
    .filter(|(_, began)| now.saturating_sub(**began) < expiry)
    .map(|(holder, _)| *holder)
    .collect();
  let mut sends = Vec::new();
  let mut stale_checkpoint = false;
  let mut installs_built: u64 = 0;
  let targets: Vec<HostId> = live.shipper.targets().collect();
  live
    .cut_installs
    .retain(|holder, _| targets.contains(holder));
  for holder in &targets {
    // A holder that has not acknowledged the volume's catalog refuses the log (its bound is the catalog's size,
    // `hold_bound`): the record plane places the catalog there first, re-shipped each period, and the next dispatch after
    // that ships it the log.
    if !catalogued.contains(holder) || busy.contains(holder) || live.unreachable.contains(holder) {
      continue;
    }
    match live.shipper.pending(*holder) {
      Next::Entries(entries) => {
        for entry in entries {
          sends.push(Send {
            object,
            holder: *holder,
            stream: slates_cluster::live::LIVE_ENTRY_STREAM,
            bytes: entry.encode(),
            image: None,
            watched: false,
          });
        }
      }
      Next::Install => {
        let (send, stale) = install_for(live, object, *holder);
        stale_checkpoint |= stale;
        installs_built = installs_built.saturating_add(u64::from(send.is_some()));
        sends.extend(send);
      }
      Next::Nothing | Next::Fenced(_) => {}
    }
  }
  if stale_checkpoint {
    live.owes_image = true;
  }
  if installs_built > 0 {
    state.count(LIVE_INSTALL_BUILT, installs_built);
  }
  sends
}

/// The install to send `holder`, and whether the image is too old for it. `None` and not stale while a fresh image is on
/// its way ([`send_to_reached`]): one sent now would land the holder behind the tail. `None` and stale when there is no
/// image, or the holder already holds what it carries.
/// Whether an install of `image` may be cut by a watch, given the image of the last install one cut for this holder:
/// at most once an image, so a holder silenced through every attempt is cut once and then sent the image unwatched, to
/// finish or end at the deadline its bytes set, as before the watch. A re-send that a watch could cut again would start
/// from nothing each time, so a large enough install to a holder starved over and over would never finish.
fn watch_install(cut: Option<Head>, image: Head) -> bool {
  cut != Some(image)
}

fn install_for(live: &LiveVolume, object: ObjectId, holder: HostId) -> (Option<Send>, bool) {
  if live.owes_image {
    return (None, false);
  }
  let Some((head, payload)) = &live.whole else {
    return (None, true);
  };
  if live
    .shipper
    .held_by(holder)
    .is_some_and(|held| held.rank() >= head.rank())
  {
    return (None, true);
  }
  let install = Install {
    owner: live.shipper.owner(),
    object,
    epoch: live.shipper.epoch(),
    head: *head,
    state: payload.clone(),
  };
  let send = Send {
    object,
    holder,
    stream: slates_cluster::live::LIVE_STATE_STREAM,
    bytes: install.encode(),
    image: Some(*head),
    watched: watch_install(live.cut_installs.get(&holder).copied(), *head),
  };
  (Some(send), false)
}

/// The candidates the latest placement of `object`'s catalog record names: holders that take its live log ([`hold_bound`]
/// is derived from the catalog), folded into the volume's `catalogued` by [`pending_sends`]. Shipped to every candidate before, a candidate the record plane had not yet placed the
/// catalog on refused each entry and each install (`fleet.live.hold_refused.no_catalog`, 116 and 119 refusals on the two
/// candidates of one run), and its installs, the whole volume each, held its one session against the record plane's
/// re-ship of the very catalog it lacked: one candidate held the log alone, and partitioning it stalled every write
/// (`live_chaos`'s rolling partition, 2026-11-06: 1,785 installs, 2.3 GB, in one run).
fn catalogued_holders(state: &ShardState, object: ObjectId) -> Vec<HostId> {
  state
    .placed_heads
    .get(&object.catalog())
    .map(|placed| placed.placement.acked.clone())
    .unwrap_or_default()
}

/// Ships `sends` from the control shard and folds the replies on this (the owner) shard, in a task of this shard: the
/// barrier that called it does not wait for it (the barrier's reply waits on [`durability`] instead). The task is
/// bounded by the dispatch's budget and detached, so nothing joins it and its slot is reaped when it ends.
///
/// One lane a holder (A-200): a message for a holder a lane is already shipping to is not sent now. The lane, when it
/// ends, sends what the holder lacks then ([`lane_ended`]), so a holder's messages queue in its shipper, never as tasks
/// waiting for its one record session. Before 2026-11-06 every publication, every folded answer and every record period
/// sent each holder its own lane: under `live_chaos`'s rolling partition at load 45 a lane waited 100–1,000 ms for its
/// holder's session against 1–5 ms for its exchange (one borrow waited 1,208 timer ticks, all of them behind other live
/// lanes), writes took a second with both holders up, and a healed holder fell further behind. A lane whose task ended
/// without reporting (its shard's shutdown) is passed over once the dispatch's deadline has gone by.
fn dispatch(state: &mut ShardState, sends: Vec<Send>) {
  let now = slates_rt::futures::now_ns();
  let expiry = lane_expiry_ns();
  let mut opened: Vec<(ObjectId, HostId)> = Vec::new();
  let sends: Vec<Send> = sends
    .into_iter()
    .filter(|send| {
      let Some(live) = state.live.shipping.get_mut(&send.object.0) else {
        return false;
      };
      let busy = live
        .lanes
        .get(&send.holder)
        .is_some_and(|began| now.saturating_sub(*began) < expiry)
        && !opened.contains(&(send.object, send.holder));
      if busy {
        return false;
      }
      live.lanes.insert(send.holder, now);
      if !opened.contains(&(send.object, send.holder)) {
        opened.push((send.object, send.holder));
      }
      true
    })
    .collect();
  if sends.is_empty() {
    return;
  }
  let (origin, control) = (
    state.shard,
    state.shards.first().copied().unwrap_or(state.shard),
  );
  let spawned = slates_rt::futures::spawn(send_from_control(origin, control, sends));
  if let Ok(task) = spawned {
    let _ = slates_rt::futures::detach(task);
  }
}

/// Derived: how long a lane is taken to be shipping before it is passed over: the dispatch's own deadline (twice the
/// live exchange's longest budget, [`send_from_control`]), past which its task cannot still be running.
fn lane_expiry_ns() -> u64 {
  crate::fleet::live_budget()
    .max_deadline_ns()
    .saturating_mul(2)
}

/// A lane to `holder` has ended for `objects`: each volume's lane is closed, and what the holder lacks now is sent at
/// once when it `answered` ([`dispatch`]). A holder that did not answer is left unreachable until the record period
/// retries it or its session is back ([`holder_reached`]): sent again at once, a holder with no link (dropped to be
/// dialed afresh) would end each new lane at once and loop between the shards at their speed.
fn lane_ended(state: &mut ShardState, holder: HostId, objects: Vec<ObjectId>, answered: bool) {
  let mut sends = Vec::new();
  for object in objects {
    let Some(live) = state.live.shipping.get_mut(&object.0) else {
      continue;
    };
    live.lanes.remove(&holder);
    live
      .unreachable
      .retain(|unreachable| *unreachable != holder);
    if !answered {
      live.unreachable.push(holder);
    }
    // Only the ended holder's own messages, and only when it answered: every other holder is sent its own when its lane
    // ends, a publication comes or the record period runs. Sent what every other holder lacked, two holders with no link
    // (both stopped) each ended its lane at once and sent the other's, back and forth at the shards' speed (1,658
    // unanswered sends in one stop, CI macos-latest, `diag-live-durability-23`).
    if live.fenced.is_none() && answered {
      sends.extend(
        pending_sends(state, object.0)
          .into_iter()
          .filter(|send| send.holder == holder),
      );
    }
  }
  dispatch(state, sends);
}

/// `holder`'s record session is established again (on the control shard): every shard sends it what it lacks at once,
/// not at the next record period, so a resumed holder's catch-up does not wait on the period. An install it is owed waits
/// for a fresh image ([`send_to_reached`]).
pub(crate) fn holder_reached(holder: HostId) {
  let Some((origin, shards)) = state::with_state(|s| (s.shard, s.shards.clone())) else {
    return;
  };
  for shard in shards {
    crate::xshard::run_on_counted(origin, shard, move |s| send_to_reached(s, holder));
  }
}

/// Sends `holder` what this shard's live volumes that had given it up owe it ([`holder_reached`]). A holder owed an
/// install while the image is behind the log waits for one made at the log's head: the next publication makes it, or the
/// record period for an idle writer ([`refresh_image`]). The image a holder fell behind on was a median of 1,332 and
/// 1,395 entries old when last built, while an install's own transit added 0 or 1, so it landed past the retained tail
/// and took a second whole install (`live_chaos`'s rolling partition, diag-live-durability-54).
fn send_to_reached(state: &mut ShardState, holder: HostId) {
  let volumes: Vec<[u8; 16]> = state
    .live
    .shipping
    .iter_mut()
    .filter_map(|(volume, live)| {
      let stale = live
        .whole
        .as_ref()
        .is_some_and(|(head, _)| Some(*head) != live.shipper.head());
      if stale && live.shipper.pending(holder) == Next::Install {
        live.owes_image = true;
      }
      let was = live.unreachable.contains(&holder);
      live
        .unreachable
        .retain(|unreachable| *unreachable != holder);
      (was && live.fenced.is_none()).then_some(*volume)
    })
    .collect();
  let mut sends = Vec::new();
  for volume in volumes {
    sends.extend(
      pending_sends(state, volume)
        .into_iter()
        .filter(|send| send.holder == holder),
    );
  }
  dispatch(state, sends);
}

/// Sends each message over the control shard's session to its holder, the holders at once: one lane per holder, its
/// messages in order on its session, every lane polled together, and each lane's replies handed to the owner shard
/// (`origin`) the moment its exchange ends. So a write is durable as soon as the fastest `f` holders answer, and a
/// holder that does not answer delays no other: before 2026-11-06 the holders were asked in turn, each within up to a
/// liveness budget for its session and again for its reply, and with one candidate stopped a write waited for it before
/// the healthy one was asked (`live_chaos`'s rolling partition: writes at 1.0–6.6 s, at or past the budget, in 3 of 3
/// runs). The stragglers' replies follow through [`fold_into`] when they come.
async fn send_from_control(origin: u16, control: u16, sends: Vec<Send>) {
  let budget = crate::fleet::live_budget();
  let deadline = budget.max_deadline_ns().saturating_mul(2);
  let _ = crate::xshard::task_within(
    origin,
    control,
    move || async move {
      drain_stragglers();
      let mut lanes: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>> = Vec::new();
      for (holder, sends) in by_holder(sends) {
        lanes.push(Box::pin(ship_to_holder(origin, holder, sends, budget)));
      }
      every(lanes).await;
    },
    deadline,
  )
  .await;
}

/// `sends` grouped by holder, each holder's in the order they were made (the order its session carries them).
fn by_holder(sends: Vec<Send>) -> Vec<(HostId, Vec<Send>)> {
  let mut lanes: Vec<(HostId, Vec<Send>)> = Vec::new();
  for send in sends {
    match lanes.iter_mut().find(|(holder, _)| *holder == send.holder) {
      Some((_, lane)) => lane.push(send),
      None => lanes.push((send.holder, vec![send])),
    }
  }
  lanes
}

/// Runs every future in `lanes` to its end, polling them together on this task: the lanes of one dispatch, one a holder.
/// Each finishes within its exchanges' budget, so this does too.
async fn every(mut lanes: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>) {
  std::future::poll_fn(|cx| {
    lanes.retain_mut(|lane| lane.as_mut().poll(cx).is_pending());
    if lanes.is_empty() {
      std::task::Poll::Ready(())
    } else {
      std::task::Poll::Pending
    }
  })
  .await;
}

/// One holder's lane of a dispatch, on the control shard: each message over the holder's session in turn, each answer
/// folded on the owner shard as it comes. A message the holder did not answer ends the lane: the rest would each wait
/// out the same budget, and the link it rode is dropped to be dialed afresh ([`lose_links`]), after which the next
/// dispatch or the record period sends what the holder lacks.
async fn ship_to_holder(
  origin: u16,
  holder: HostId,
  sends: Vec<Send>,
  budget: slates_cluster::CommitBudget,
) {
  let mut objects: Vec<ObjectId> = Vec::new();
  for send in &sends {
    if !objects.contains(&send.object) {
      objects.push(send.object);
    }
  }
  let (answered, cuts) = ship_lane(origin, holder, sends, budget).await;
  // After the lane's own folds, so the owner sends what the holder lacks given its last answer.
  let control = state::with_state(|s| s.shard).unwrap_or(origin);
  crate::xshard::run_on_counted(control, origin, move |s| {
    lane_ended(s, holder, objects, answered);
    for (object, image) in cuts {
      if let Some(live) = s.live.shipping.get_mut(&object.0) {
        live.cut_installs.insert(holder, image);
      }
    }
  });
}

/// The messages of one lane in turn ([`ship_to_holder`]): whether the holder answered every one it was sent.
async fn ship_lane(
  origin: u16,
  holder: HostId,
  sends: Vec<Send>,
  budget: slates_cluster::CommitBudget,
) -> (bool, Vec<(ObjectId, Head)>) {
  let mut cuts = Vec::new();
  for send in sends {
    let sessions = borrow_session(holder, budget.max_deadline_ns()).await;
    // Each message under its own budget: the liveness budget plus its transfer on the session it rides, so an install of
    // the whole volume is not given up the moment an entry would be (`exchange_budget`).
    let exchange = exchange_budget(send.bytes.len(), &sessions);
    let watch = send.watched.then_some(slates_cluster::live::Watch {
      lost: holder_lost,
      silence_ns: crate::daemon::LIVENESS_BUDGET_NS,
    });
    let exchanged = slates_cluster::live::exchange_watched(
      (send.stream, &send.bytes),
      sessions,
      exchange,
      watch,
      |_, _| true,
    )
    .await;
    if let Some(image) = send.image
      && exchanged.cut.contains(&holder)
    {
      cuts.push((send.object, image));
      let _ = state::with_state(|s| s.count(LIVE_INSTALL_CUT, 1));
    }
    let (replies, stragglers) = settle_sessions(exchanged);
    let answered = !replies.is_empty();
    if !answered {
      let _ = state::with_state(|s| s.count(LIVE_UNANSWERED, 1));
    }
    let replies: Vec<(ObjectId, HostId, Vec<u8>)> = replies
      .into_iter()
      .map(|(host, bytes)| (send.object, host, bytes))
      .collect();
    if !replies.is_empty() {
      let control = state::with_state(|s| s.shard).unwrap_or(origin);
      crate::xshard::run_on_counted(control, origin, move |s| fold_into(s, replies));
    }
    // After the answers are on their way to the owner, so durability waits for no straggler.
    finish_stragglers(origin, send.object, stragglers).await;
    if !answered {
      return (false, cuts);
    }
  }
  (true, cuts)
}

/// Whether this node has stopped keeping contact with `holder` (on the control shard, where membership lives): what
/// lets a lane's install end before its deadline ([`slates_cluster::live::Watch`]).
fn holder_lost(holder: HostId) -> bool {
  state::with_state(|s| !crate::fleet::keeps_direct_contact_with(s, holder)).unwrap_or(false)
}

/// The budget one message to one holder gets: the liveness budget a reply gets, plus the time its bytes take at the
/// session's window-limited rate. Derived: `bytes × smoothed RTT / congestion window` (one window a round trip, the
/// slowest a sender in congestion avoidance moves), so an install of the whole volume is given its transfer and an
/// entry, a few kilobytes, about nothing more. Under the liveness budget alone an install of 1.5 MB never finished
/// over a session a partition had left at an initial window and a 100–240 ms smoothed RTT: each was cut off, its link
/// dropped and dialed afresh, at an initial window again, and the whole volume sent again, 1,391 times (2.06 GB) while
/// both candidates already held the owner's head, and no write became durable (2026-11-06, `live_chaos.rs`).
fn exchange_budget(
  bytes: usize,
  sessions: &[(HostId, slates_transport::endpoint::Endpoint)],
) -> slates_cluster::CommitBudget {
  let live = crate::fleet::live_budget();
  let transfer_ns = sessions.first().map_or(0, |(_, endpoint)| {
    // Before any sample the smoothed RTT reads zero; the probe timeout, twice the initial RTT then, stands in.
    let round_trip = match endpoint.smoothed_rtt() {
      0 => endpoint.pto(),
      measured => measured,
    };
    u64::try_from(bytes)
      .unwrap_or(u64::MAX)
      .saturating_mul(round_trip)
      .checked_div(endpoint.congestion_window().max(1))
      .unwrap_or(0)
  });
  slates_cluster::CommitBudget::hard(
    live.deadline_ns.saturating_add(transfer_ns),
    live.poll_interval_ns,
  )
}

/// Borrows `holder`'s record session, waiting for it while another dispatch has it out, one timer tick at a time,
/// within `deadline_ns`: empty when it never came back (counted, and the period re-ships). Dropped at the first miss, a
/// write's message waited for whichever later dispatch carried it, at worst the next record period: two in three sends of
/// a stable-write run found the session out, and the rare write took about 100 ms (2026-11-05, BENCHMARKS "What live
/// shipping adds to a stable NFS write"). The forward path learned the same (`fleet::forward_over_leader_session`).
async fn borrow_session(
  holder: HostId,
  deadline_ns: u64,
) -> Vec<(HostId, slates_transport::endpoint::Endpoint)> {
  let began = slates_rt::futures::now_ns();
  let mut missed = false;
  loop {
    let sessions = crate::fleet::take_sessions("fleet.lent_to.live_ship", |host| host == holder);
    let present = state::with_state(|s| s.record_sessions.contains_key(&holder)).unwrap_or(false);
    // A session out on loan comes back; no link at all (never formed, or dropped to be dialed afresh) is left to the
    // period.
    if !sessions.is_empty()
      || !present
      || slates_rt::futures::now_ns().saturating_sub(began) >= deadline_ns
    {
      if sessions.is_empty() || missed {
        let _ = state::with_state(|s| s.count(LIVE_NO_SESSION, 1));
      }
      return sessions;
    }
    missed = true;
    // Shape: one tick of the shard's timer wheel, the finest park there is (`sleep` rounds up to it).
    if slates_rt::futures::sleep(1).await.is_err() {
      return Vec::new();
    }
  }
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
    if !recover_stragglers(origin, object, &mut stragglers) {
      kept.push((origin, object, stragglers));
    }
  }
  let _ = state::with_state(|s| s.live.stragglers.extend(kept));
}

/// Takes what `stragglers` have returned: each answered holder's session back to the pool and its reply to the volume's
/// owner shard (`origin`), each unanswered holder's link dropped to be dialed afresh. Whether every straggler has
/// returned. On the control shard.
fn recover_stragglers(
  origin: u16,
  object: ObjectId,
  stragglers: &mut slates_cluster::Stragglers,
) -> bool {
  let (late, done) = stragglers.recover_replies();
  deliver_late(origin, object, late);
  done
}

/// Hands what stragglers brought back where it belongs (on the control shard): each answered holder's session to the
/// pool and its reply to the volume's owner shard (`origin`), each unanswered holder's link dropped to be dialed afresh.
fn deliver_late(
  origin: u16,
  object: ObjectId,
  late: Vec<(
    HostId,
    slates_cluster::TimedReply,
    slates_transport::endpoint::Endpoint,
  )>,
) {
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
}

/// Waits, on the lane's own task, for an exchange's stragglers to report, and recovers them ([`recover_stragglers`]).
/// An exchange's requests are children of the task that made it (`slates_cluster::live::exchange`), and a task's end
/// cancels its children: a request still in flight when its lane's task ended was dropped with its holder's session in
/// it, never reported, and its slot stayed lent for good, so the owner shipped that holder nothing again (both of an
/// owner's sessions `lent=true` with its log stuck two short of its head: `live_chaos` at load 118, 3 of 10 runs stalled
/// without this and 0 of 10 with it at load 41-58; and the KIND leg's 80 ms WAN in 50-100 writes, raft-chaos run
/// 42178128960, 2026-11-06). Each request is bounded by the exchange's own deadline, so this waits at most the gap
/// between the exchange giving up and its requests doing so: a timer tick or two. A shard that can no longer time the
/// wait leaves them to the drain ([`drain_stragglers`]).
async fn finish_stragglers(
  origin: u16,
  object: ObjectId,
  mut stragglers: slates_cluster::Stragglers,
) {
  // Held by `a_lane_that_ends_with_a_holder_unanswered_gets_every_session_back` (crates/cluster/tests/live.rs).
  let (late, done) = stragglers.settle().await;
  deliver_late(origin, object, late);
  if !done {
    settle_stragglers(origin, object, stragglers);
  }
}

/// Puts one live exchange's sessions where they belong, on the control shard where the links live: each answering
/// holder's session back to its link ([`crate::fleet::return_sessions`]), and each unanswered holder's link dropped to be
/// dialed afresh ([`lose_links`]). The replies and the stragglers still in flight are handed back, the stragglers for
/// the caller to finish or queue. Every caller of `slates_cluster::live::exchange` goes through it, so none can leave a
/// holder's slot lent with no session behind it: a timed-out request's session is dropped inside the exchange, and
/// the takeover's phase one and log fetch read only the answered sessions, so the slot of a holder whose report missed
/// the liveness budget stayed lent for good and every later phase one skipped it (KIND wan-loss: 425 waits for the
/// quorum with the holder alive at the head, CI diag-raft-chaos-final-41;
/// `docs/bugs/2026-11-08-a-takeover-question-that-timed-out-left-its-holders-link-lent-for-good.md`).
#[must_use]
fn settle_sessions(
  exchanged: slates_cluster::live::Exchanged,
) -> (Vec<(HostId, Vec<u8>)>, slates_cluster::Stragglers) {
  lose_links(&exchanged.unanswered);
  crate::fleet::return_sessions(exchanged.reusable);
  (exchanged.replies, exchanged.stragglers)
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
    if let Next::Fenced(current) = live.shipper.on_reply(host, reply) {
      // A later epoch holds the volume: this owner was superseded, ships no more of it, and tells every write waiting
      // on it so.
      live.fenced = Some(current);
      refuse_awaiting(state, object.0, current);
      wake_parked(state, object.0);
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
    // A write the credit turned away waits for the whole record it left owed; the answer that freed the credit appends
    // it. Left to the next write or the record period, the last turned-away write of a burst waited up to that period:
    // 270 of 2,285 publications turned away and 2 writes over 50 ms in a 2,000-write run (2026-11-05, BENCHMARKS "What
    // live shipping adds to a stable NFS write"). `published` ships what the holders lack itself.
    if owes_whole_within_credit(state, volume) {
      published(state, slates_db::catalog::VolumeId { bytes: volume }, None);
      continue;
    }
    sends.extend(pending_sends(state, volume));
  }
  dispatch(state, sends);
}

/// Whether `volume` owes an install image ([`LiveVolume::owes_image`]).
fn owes_image(state: &ShardState, volume: [u8; 16]) -> bool {
  state
    .live
    .shipping
    .get(&volume)
    .is_some_and(|live| live.owes_image)
}

/// Whether `volume` owes a whole record and its credit allows one now (a checkpoint may go one past it,
/// `Shipper::append_checkpoint`).
fn owes_whole_within_credit(state: &ShardState, volume: [u8; 16]) -> bool {
  state
    .live
    .shipping
    .get(&volume)
    .is_some_and(|live| live.owes_whole && live.shipper.in_flight() <= CREDIT)
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
        let id = slates_db::catalog::VolumeId { bytes: *volume };
        follow_candidates(s, *volume);
        // The period is what retries a holder whose last lane went unanswered ([`LiveVolume::unreachable`]).
        if let Some(live) = s.live.shipping.get_mut(volume) {
          live.unreachable.clear();
        }
        // A writer may wait on a condition that changes with no acknowledgement to fold (the head placed once its
        // neighbourhood settles, a pair delivered): answered here at the latest.
        resolve(s, *volume);
        // A log whose last whole entry no successor among the candidates could open is checkpointed again once this
        // shard has delivered the pairs that would let it ([`covers_candidates`]).
        if !covers_candidates(s, *volume)
          && coverable_now(s, *volume)
          && let Some(live) = s.live.shipping.get_mut(volume)
        {
          live.owes_whole = true;
        }
        if owes_whole_within_credit(s, *volume) {
          published(s, id, None);
        } else if owes_image(s, *volume) && published_whole(s, id) {
          // No publication is coming to make the image a holder behind the tail waits for (the writer is idle).
          refresh_image(s, id);
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
        head_epoch: live.shipper.head().map(|head| head.epoch.0),
        head_identity: live.shipper.head().map(|head| head.identity),
        fence: live.shipper.epoch().0,
        holders: sorted_ids(live.shipper.holders()),
        moving_to: live
          .shipper
          .transition()
          .map(sorted_ids)
          .unwrap_or_default(),
        recorded: recorded_ids(state, *volume),
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
      head_epoch: held.replica.head().map(|head| head.epoch.0),
      head_identity: held.replica.head().map(|head| head.identity),
      fence: held.replica.fence().0,
      holders: Vec::new(),
      moving_to: Vec::new(),
      recorded: recorded_ids(state, object.0),
    });
  shipping.chain(held).collect()
}

/// The member ids of the holder set `volume`'s catalog records (A-240), ascending; empty when it records none.
fn recorded_ids(state: &ShardState, volume: [u8; 16]) -> Vec<u64> {
  let mut ids: Vec<u64> = state
    .db
    .partition()
    .volume(slates_db::catalog::VolumeId { bytes: volume })
    .and_then(|record| record.policy.live_holders.as_ref())
    .map(|recorded| recorded.holders.clone())
    .unwrap_or_default();
  ids.sort_unstable();
  ids
}

/// `hosts`' member ids, ascending.
fn sorted_ids(hosts: &[HostId]) -> Vec<u64> {
  let mut ids: Vec<u64> = hosts.iter().map(|host| host.0).collect();
  ids.sort_unstable();
  ids
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
  let object = slates_db::live::object_of(request);
  let bound = match admitted(state, local, peer, object) {
    Ok(bound) => bound,
    Err(why) => {
      count_refused(state, why);
      return encode_reply(&Err(not_served(
        state,
        object,
        slates_db::live::epoch_of(request),
      )));
    }
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
  match &reply {
    Err(error) => count_refused(state, refused_counter(error)),
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
  let object = slates_db::live::object_of_state(request);
  let bound = match admitted(state, local, peer, object) {
    Ok(bound) => bound,
    Err(why) => {
      count_refused(state, why);
      return encode_reply(&Err(not_served(
        state,
        object,
        slates_db::live::epoch_of_install(request),
      )));
    }
  };
  let reply = Install::decode(request, bound).and_then(|install| {
    let held = held_entry(state, peer, install.object);
    match held.replica.check_install(&install)? {
      Admitted::Apply(head) => {
        held.entries.clear();
        held.bytes = 0;
        keep(held, &install.state, bound)?;
        held.replica.commit(install.epoch, head);
        Ok((Some(head), true))
      }
      Admitted::Held => Ok((held.replica.head(), false)),
    }
  });
  if matches!(reply, Ok((_, true))) {
    state.count(LIVE_INSTALL_TAKEN, 1);
  }
  let reply = reply.map(|(head, _)| head);
  match &reply {
    Err(error) => count_refused(state, refused_counter(error)),
    Ok(_) => state.count(LIVE_HELD, 1),
  }
  encode_reply(&reply)
}

/// Why a holder will not serve a message about `object` made under `epoch`: below the fence its copy has promised it is
/// `StaleEpoch`, which tells a superseded owner to stop (`Shipper::on_reply` answers `Fenced`) and refuse what waits on
/// it; otherwise `Unauthorized`. Refused as a stranger, a resumed old owner could not tell its loss from a passing
/// refusal and held its write unanswered for the fence flow's whole 40 s wait (2026-11-05, `live_fleet`).
fn not_served(state: &ShardState, object: Option<ObjectId>, epoch: Option<HostEpoch>) -> LiveError {
  let fence = object
    .and_then(|object| state.live.held.get(&object))
    .map(|held| held.replica.fence());
  match (fence, epoch) {
    (Some(current), Some(epoch)) if epoch < current => LiveError::StaleEpoch { current },
    _ => LiveError::Unauthorized,
  }
}

/// The bound for a message naming `object`, when `peer` may place it on this node; otherwise the counter naming why
/// not: the message names no object, `peer` is not the volume's acting owner here, or this node does not hold the
/// volume's catalog yet (the bound is derived from it).
fn admitted(
  state: &ShardState,
  local: HostId,
  peer: HostId,
  object: Option<ObjectId>,
) -> Result<usize, &'static str> {
  let object = object.ok_or(REFUSED_MALFORMED)?;
  let regional = state.council.configuration();
  let placing = slates_cluster::content::ContentAccess::Place;
  let authority = crate::fleet::content_authority(
    regional,
    &state.holder_records,
    local,
    peer,
    placing,
    object,
  );
  let catalog = object.catalog();
  let recorded = state
    .holder_records
    .get(&catalog)
    .and_then(|acceptor| acceptor.highest(catalog))
    .is_some_and(|(_, bytes)| names_holder(bytes, local));
  live_admission(authority, recorded).map_err(not_owner_counter)?;
  hold_bound(state, object).ok_or(REFUSED_NO_CATALOG)
}

/// A live holder's admission of its acting owner's entry: the placement authority's answer, except that a node the
/// volume's newest catalog record names among the log's holders (`recorded`) takes the entry though its own
/// configuration has not placed it (`fleet::CONTENT_NOT_A_CANDIDATE`). Its view lags the owner's, or the owner keeps a
/// holder its configuration dropped, since a log never moves to a set of `f` or fewer (`Shipper::begin_transition`);
/// phase one counts the recorded holders, so the copy counts. Refused, every write waited for the other holder alone
/// (live-durability's rolling partition flow: 2,086 refusals in one run while that holder was stopped, a write held
/// 2.34 s). Every other refusal stands: the sender is not a member, or not the acting owner.
fn live_admission(authority: Result<(), &'static str>, recorded: bool) -> Result<(), &'static str> {
  match authority {
    Err(crate::fleet::CONTENT_NOT_A_CANDIDATE) if recorded => Ok(()),
    other => other,
  }
}

/// Whether a catalog record's value names `local` among the live log's holders (A-240's record).
fn names_holder(value: &[u8], local: HostId) -> bool {
  crate::catalog::CatalogValue::from_record_bytes(value)
    .and_then(|value| value.live_holders)
    .is_some_and(|recorded| recorded.holders.contains(&local.0))
}

/// The holder's counter for a placement the configuration does not authorize, by `fleet::content_authority`'s reason.
fn not_owner_counter(why: &'static str) -> &'static str {
  match why {
    "content.refused.not_a_member" => "fleet.live.hold_refused.not_a_member",
    "content.refused.no_acting_owner" => "fleet.live.hold_refused.no_acting_owner",
    "content.refused.not_the_acting_owner" => "fleet.live.hold_refused.not_the_acting_owner",
    crate::fleet::CONTENT_NOT_A_CANDIDATE => "fleet.live.hold_refused.not_a_candidate",
    _ => "fleet.live.hold_refused.not_owner",
  }
}
/// Counter: a holder refused a message about a volume whose catalog it does not hold yet.
const REFUSED_NO_CATALOG: &str = "fleet.live.hold_refused.no_catalog";
/// Counter: a holder refused bytes that are not a well-formed message, or a payload over its bound.
const REFUSED_MALFORMED: &str = "fleet.live.hold_refused.malformed";

/// Counts a holder's refusal, in total and by why (`fleet.live.hold_refused.*`).
fn count_refused(state: &mut ShardState, why: &'static str) {
  state.count(LIVE_HOLD_REFUSED, 1);
  state.count(why, 1);
}

/// The counter naming a replica's refusal.
fn refused_counter(error: &LiveError) -> &'static str {
  match error {
    LiveError::StaleEpoch { .. } => "fleet.live.hold_refused.stale_epoch",
    LiveError::Behind { .. } => "fleet.live.hold_refused.behind",
    LiveError::Unauthorized => "fleet.live.hold_refused.other_owner",
    LiveError::ForeignObject => "fleet.live.hold_refused.foreign_object",
    LiveError::Malformed => REFUSED_MALFORMED,
    LiveError::NoCredit { .. } => "fleet.live.hold_refused.no_credit",
  }
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
    count_refused(state, REFUSED_MALFORMED);
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
    count_refused(state, "fleet.live.hold_refused.fetch_unauthorized");
    return slates_db::live::encode_fetched(&Err(LiveError::Unauthorized), &[]);
  }
  match fenced_copy(state, peer, fetch.object, fetch.epoch) {
    Ok(held) => {
      held.replica.reassign(peer);
      let head = held.replica.head();
      let entries: Vec<Vec<u8>> = held.entries.iter().cloned().collect();
      state.count(LIVE_FETCHED, 1);
      slates_db::live::encode_fetched(&Ok(head), &entries)
    }
    Err(refusal) => slates_db::live::encode_fetched(&Err(refusal), &[]),
  }
}

/// A candidate's answer to phase one's report (A-200, A-240): authorized as a fetch is, the fence raised to the
/// successor's epoch, and the head and the bytes kept, never the entries (the adopted holder alone is fetched).
pub(crate) fn serve_report(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Ok(report) = slates_db::live::Report::decode(request) else {
    count_refused(state, REFUSED_MALFORMED);
    return slates_db::live::encode_reported(&Err(LiveError::Malformed), 0);
  };
  let regional = state.council.configuration();
  let reading = slates_cluster::content::ContentAccess::Read;
  if !crate::fleet::content_authorized(
    regional,
    &state.holder_records,
    local,
    peer,
    reading,
    report.object,
  ) {
    count_refused(state, "fleet.live.hold_refused.fetch_unauthorized");
    return slates_db::live::encode_reported(&Err(LiveError::Unauthorized), 0);
  }
  match fenced_copy(state, peer, report.object, report.epoch) {
    Ok(held) => {
      held.replica.reassign(peer);
      let head = held.replica.head();
      let kept = u64::try_from(held.bytes).unwrap_or(u64::MAX);
      state.count(LIVE_REPORTED, 1);
      slates_db::live::encode_reported(&Ok(head), kept)
    }
    Err(refusal) => slates_db::live::encode_reported(&Err(refusal), 0),
  }
}

/// Counter: phase-one reports this candidate answered (A-240).
/// Format: a counter name in the daemon's status report.
const LIVE_REPORTED: &str = "fleet.live.reported";

/// This candidate's copy of `object` with its fence raised to a takeover's `epoch` (phase one): the copy it holds, or
/// an empty one made now for `successor`. An empty report counts toward phase one's `f + 1` (a durable entry is held by
/// `W` of the recorded set's `n` holders, and `W + f + 1 > n`, so any `f + 1` of them include one that holds it), so it
/// must leave the same fence a full one does, or a superseded owner resumed could still ship to this candidate after it reported and commit outside the
/// adopted head. The nemesis model decides both: counted with its fence, every invariant holds; counted without one,
/// StaleNeverCommits fails (`crates/db/tests/live.rs`, `an_empty_report_without_a_fence_lets_a_superseded_owner_commit`,
/// 10 seeds). Refused, with the fence, when `epoch` is below it.
fn fenced_copy(
  state: &mut ShardState,
  successor: HostId,
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
) -> Result<&mut Held, LiveError> {
  let held = state.live.held.entry(object).or_insert_with(|| Held {
    replica: Replica::new(successor, object),
    entries: VecDeque::new(),
    bytes: 0,
  });
  held.replica.prepare(epoch)?;
  Ok(held)
}

/// A candidate's answer on the whole-state kind: a fetch or an install, by its tag.
pub(crate) fn serve_state(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  if slates_db::live::is_report(request) {
    serve_report(state, local, peer, request)
  } else if slates_db::live::is_fetch(request) {
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
/// Counter: a takeover refused because the lineage key the log's whole entry carries could not be adopted.
const REFUSED_LINEAGE: &str = "fleet.live.takeover_refused.lineage";
/// Counter: a takeover refused because an entry's payload would not open (its seal, or its bytes).
const REFUSED_OPEN: &str = "fleet.live.takeover_refused.open";
/// Counter: a takeover refused because an opened entry's record would not decode.
const REFUSED_RECORD: &str = "fleet.live.takeover_refused.record";
/// Counter: a takeover refused because a delta would not apply to the image before it.
const REFUSED_APPLY: &str = "fleet.live.takeover_refused.apply";
/// Counter: a takeover refused because the log's first entry is not a whole record (or there is none).
const REFUSED_NOT_WHOLE_FIRST: &str = "fleet.live.takeover_refused.not_whole_first";
/// Counter: a takeover refused because the rebuilt image could not be laid out as an archive (no root, a cycle,
/// content missing for an extent).
const REFUSED_LAYOUT: &str = "fleet.live.takeover_refused.layout";
/// Counter: a taken-over live volume rebuilt from the adopted log.
const LIVE_TAKEN_OVER: &str = "fleet.live.taken_over";

/// What a takeover fetched of a live volume: the adopted head's payloads (a whole one first), and the
/// successor's copy of the lineage key the first carries (A-92 piece 4c), unwrapped on this control shard.
pub(crate) struct Fetched {
  /// The head adopted, which the payloads end at.
  pub(crate) head: Head,
  pub(crate) payloads: Vec<Vec<u8>>,
  pub(crate) lineage: Option<([u8; 16], Vec<u8>)>,
  pub(crate) naming: Option<crate::head::HeadNaming>,
}

/// Phase one's question, on the control shard: each of `others` with a session in its link is asked to raise its copy
/// of `object`'s fence to `epoch` and report its head and kept bytes (`slates_db::live::Report`), under `budget`. The
/// replies, each with the host that sent it; a host with no session to ride, or none that answered, is not among them.
pub(crate) async fn ask_reports(
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
  others: &[HostId],
  budget: slates_cluster::CommitBudget,
) -> Vec<(HostId, Vec<u8>)> {
  let sessions =
    crate::fleet::take_sessions("fleet.lent_to.live_takeover", |host| others.contains(&host));
  let request = slates_db::live::Report { object, epoch }.encode();
  let exchanged = slates_cluster::live::exchange(
    (slates_cluster::live::LIVE_STATE_STREAM, &request),
    sessions,
    budget,
    |_, _| false,
  )
  .await;
  let (replies, stragglers) = settle_sessions(exchanged);
  settle_stragglers(
    state::with_state(|s| s.shard).unwrap_or_default(),
    object,
    stragglers,
  );
  replies
}

/// Phase one for a live volume (A-200), on the control shard: raises this node's own copy's fence to `epoch`, asks every
/// other candidate in `candidates` to do the same and report its head and kept bytes (`slates_db::live::Report`), adopts
/// the greatest head among the answers (`slates_db::live::adopt`), and then fetches the adopted holder's log alone
/// ([`fetch_log`]). The `f + 1` it needs are counted among `holders` alone, the hosts the shipper being
/// taken over recorded in the volume's catalog before it shipped (A-240): a host outside them (a member that joined
/// since) holds none of the log, and counted in a holder's place it let a head the log had passed be adopted. A host
/// asked that is not a holder still reports, and its head is adopted when it is the greatest. `Ok(None)` when no
/// candidate holds any of the volume's log (it was never shipped); `Err` with what the takeover waits for when too few
/// holders answered.
pub(crate) async fn fetch(
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
  candidates: Vec<HostId>,
  holders: Vec<HostId>,
) -> Result<Option<Fetched>, &'static str> {
  let local = state::with_state(|s| s.fleet.configuration().owner)
    .ok_or("fleet.live.takeover_waits_state")?;
  let quorum = state::with_state(|s| s.fleet.configuration().quorum)
    .ok_or("fleet.live.takeover_waits_state")?;
  let cap = state::with_state(|s| hold_bound(s, object))
    .flatten()
    .ok_or("fleet.live.takeover_waits_catalog")?;
  let mut reports: Vec<(HostId, Option<Head>)> = Vec::new();
  let mut kept: BTreeMap<HostId, u64> = BTreeMap::new();
  // The successor's own report, as any candidate's: an empty one counts too, fenced ([`fenced_copy`]). Counted only
  // when it held a copy, a successor holding nothing could not reach `f + 1` once the owner was gone and waited for
  // its quorum without end (370 and 371 waits under load, 2026-11-05; `a_successor_that_holds_no_copy_…`).
  let own = state::with_state(|s| {
    let held = fenced_copy(s, local, object, epoch).ok()?;
    held.replica.reassign(local);
    Some(held.replica.head())
  })
  .flatten();
  if let Some(head) = own {
    reports.push((local, head));
  }
  let mut others: Vec<HostId> = candidates
    .into_iter()
    .filter(|host| *host != local)
    .collect();
  for holder in &holders {
    if *holder != local && !others.contains(holder) {
      others.push(*holder);
    }
  }
  // Phase one proper: every other candidate and recorded holder reports its head and the bytes it keeps, a few bytes
  // each, within the liveness budget whatever the path.
  for (host, bytes) in ask_reports(object, epoch, &others, crate::fleet::live_budget()).await {
    if let Ok((Ok(head), bytes)) = slates_db::live::decode_reported(&bytes) {
      reports.push((host, head));
      kept.insert(host, bytes);
    }
  }
  let needed = usize::try_from(quorum.f)
    .ok()
    .and_then(|f| f.checked_add(1))
    .unwrap_or(usize::MAX);
  let counted = reports
    .iter()
    .filter(|(host, _)| holders.contains(host))
    .count();
  if counted < needed {
    return Err("fleet.live.takeover_waits_quorum");
  }
  let Some((holder, head)) = slates_db::live::adopt(&reports, &holders, quorum) else {
    return Ok(None);
  };
  let refused = state::with_state(|s| s.live.refused_rebuilds.get(&object).copied()).flatten();
  if !owes_refetch(refused.as_ref(), head) {
    return Err(LIVE_TAKEOVER_WAITS_NEW_HEAD);
  }
  let payloads = if holder == local {
    state::with_state(|s| {
      s.live
        .held
        .get(&object)
        .map(|held| held.entries.iter().cloned().collect::<Vec<_>>())
    })
    .flatten()
    .unwrap_or_default()
  } else {
    let bytes = kept.get(&holder).copied().unwrap_or(0);
    fetch_log(object, epoch, (holder, head), bytes, cap).await?
  };
  let sealing = payloads
    .first()
    .and_then(|first| sealed_body(first))
    .and_then(|body| body.sealing);
  let lineage = sealing.as_ref().and_then(|sealing| {
    state::with_state(|s| crate::seal_keys::successor_lineage(s, sealing)).flatten()
  });
  let naming = sealing.and_then(|sealing| sealing.naming);
  Ok(Some(Fetched {
    head,
    payloads,
    lineage,
    naming,
  }))
}

/// Counter: a taken-over live volume's log was not fetched again, its rebuild having been refused at the head phase
/// one adopted now ([`owes_refetch`]).
/// Format: a counter name in the daemon's status report.
const LIVE_TAKEOVER_WAITS_NEW_HEAD: &str = "fleet.live.takeover_waits_new_head";

/// Whether a successor fetches the adopted holder's log for a rebuild, given the head and kind its last rebuild was
/// refused at (`None`: none was). A refusal the log's bytes decide is final for that head (its first payload's
/// sealing, its records and its deltas are fixed by the chain identity), so the log is fetched again only once phase
/// one adopts another head. The lineage refusal is this node's own key record (`seal_keys::adopt_lineage`), which may
/// succeed next time, so it is retried. Every refusal refetched the whole log each record period before: 389 fetches
/// of one holder for a successor that could never open the log (CI, diag-14 run 3, 2026-11-07).
fn owes_refetch(refused: Option<&(Head, &'static str)>, adopted: Head) -> bool {
  match refused {
    None => true,
    Some((_, kind)) if *kind == REFUSED_LINEAGE => true,
    Some((head, _)) => *head != adopted,
  }
}

/// Records on the control shard that `object`'s rebuild from the log at `head` was refused for `why`, so its log is not
/// fetched again at that head ([`owes_refetch`]); forgets it once the volume is served (`why` is `None`).
pub(crate) fn note_rebuild(object: ObjectId, head: Head, why: Option<&'static str>) {
  let _ = state::with_state(|s| match why {
    // Only a refusal of the log itself: one from the volume's restore says nothing about the log's bytes.
    Some(why) if REBUILD_REFUSALS.contains(&why) => {
      s.live.refused_rebuilds.insert(object, (head, why));
    }
    Some(_) => {}
    None => {
      s.live.refused_rebuilds.remove(&object);
    }
  });
}

/// The refusals of a rebuild from a live log ([`rebuild`]), each its counter's name.
const REBUILD_REFUSALS: [&str; 6] = [
  REFUSED_LINEAGE,
  REFUSED_OPEN,
  REFUSED_RECORD,
  REFUSED_APPLY,
  REFUSED_NOT_WHOLE_FIRST,
  REFUSED_LAYOUT,
];

/// The second step of a live takeover (A-240): the log of the one holder phase one adopted, `bytes` long as it
/// reported, fetched under a budget those bytes derive ([`exchange_budget`]), so a log of any size is given its transfer
/// over the path to that holder. The holder is fenced at `epoch` already, so its head cannot have moved; one that
/// answers another head (a later takeover fenced it higher) or nothing is waited for, and phase one runs again.
async fn fetch_log(
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
  (holder, head): (HostId, Head),
  bytes: u64,
  cap: usize,
) -> Result<Vec<Vec<u8>>, &'static str> {
  let session = crate::fleet::take_sessions("fleet.lent_to.live_takeover", |host| host == holder);
  if session.is_empty() {
    return Err("fleet.live.takeover_waits_log_session");
  }
  let budget = exchange_budget(usize::try_from(bytes).unwrap_or(usize::MAX), &session);
  let request = slates_db::live::Fetch { object, epoch }.encode();
  let exchanged = slates_cluster::live::exchange(
    (slates_cluster::live::LIVE_STATE_STREAM, &request),
    session,
    budget,
    |_, _| false,
  )
  .await;
  let (replies, stragglers) = settle_sessions(exchanged);
  settle_stragglers(
    state::with_state(|s| s.shard).unwrap_or_default(),
    object,
    stragglers,
  );
  let fetched = replies
    .into_iter()
    .find(|(host, _)| *host == holder)
    .ok_or("fleet.live.takeover_waits_log")?;
  match slates_db::live::decode_fetched(&fetched.1, cap) {
    Ok((Some(fetched_head), payloads)) if fetched_head == head => Ok(payloads),
    _ => Err("fleet.live.takeover_waits_log"),
  }
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
    Err(why) => {
      // In total and by why (`fleet.live.takeover_refused.*`), as a holder's refusals are counted: one counter for nine
      // refusals hid which step a 1,000-write log failed at on KIND (runs 42178128960, 42318867890: 1,512 refusals).
      state.count(LIVE_TAKEOVER_REFUSED, 1);
      state.count(why, 1);
    }
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
      .map_err(|_| REFUSED_LINEAGE)?;
  }
  let mut publications = Vec::new();
  for payload in &fetched.payloads {
    publications.push(open_payload(state, id, owner, payload).ok_or(REFUSED_OPEN)?);
  }
  let mut image = None;
  let mut runs: BTreeMap<(u64, u64), Vec<u8>> = BTreeMap::new();
  for publication in publications {
    let record =
      <VolumeRecord as Wire>::decode(&mut &publication.record[..]).map_err(|_| REFUSED_RECORD)?;
    match (record, image.as_mut()) {
      (VolumeRecord::Full { image: whole }, _) => {
        image = Some(whole);
        runs.clear();
      }
      (VolumeRecord::Delta { delta }, Some(image)) => {
        slates_vfs::recover::VolumeImage::apply(image, &delta).map_err(|_| REFUSED_APPLY)?;
      }
      (VolumeRecord::Delta { .. }, None) => return Err(REFUSED_NOT_WHOLE_FIRST),
    }
    for run in publication.runs {
      runs.insert((run.inode, run.offset), run.bytes);
    }
  }
  let image = image.ok_or(REFUSED_NOT_WHOLE_FIRST)?;
  archive_of(&image, &runs, ObjectId(id.bytes).local(), state.config.page)
}

/// A publication's body sealed under the volume's lineage key, in pieces of at most an archive chunk's bytes
/// (`slates_archive::format::MAX_CHUNK_BYTES`, 1 MiB), since no chunk opens past it (`Archive::content`). Sealed as one
/// chunk, a body past it sealed and shipped but never opened: a whole record carries the volume's content, so a live
/// volume holding more than 1 MiB could not be taken over (KIND runs 42326682216 and 42331191530: every refusal
/// `fleet.live.takeover_refused.open`, a 1,000-write volume never served).
fn seal_body(
  lineage: &hyper_seal::keys::WrappingKey,
  namer: &hyper_seal::name::Namer,
  body: Vec<u8>,
  segment: u32,
) -> Option<Vec<SealedPiece>> {
  let cap = usize::try_from(slates_archive::format::MAX_CHUNK_BYTES).ok()?;
  body
    .chunks(cap.max(1))
    .map(|piece| {
      let chunk = slates_archive::Archive::raw_chunk(piece.to_vec());
      let sealed = slates_cluster::sealed::seal_chunk(lineage, namer, &chunk, segment).ok()?;
      Some(SealedPiece {
        name: sealed.name.to_vec(),
        bytes: sealed.bytes,
      })
    })
    .collect()
}

/// A body [`seal_body`] sealed, opened piece by piece and joined: the publication's bytes. `None` when any piece will
/// not open, or there are none.
fn open_body(
  lineage: &hyper_seal::keys::WrappingKey,
  namer: &hyper_seal::name::Namer,
  pieces: Vec<SealedPiece>,
) -> Option<Vec<u8>> {
  if pieces.is_empty() {
    return None;
  }
  let mut body = Vec::new();
  for piece in pieces {
    let chunk = slates_cluster::sealed::SealedChunk {
      name: piece.name.as_slice().try_into().ok()?,
      bytes: piece.bytes,
    };
    body.extend_from_slice(
      &slates_cluster::sealed::open_chunk(lineage, namer, &chunk)
        .ok()?
        .payload,
    );
  }
  Some(body)
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
    open_body(&lineage, &namer, sealed.pieces)?
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
/// per-creator number (`ObjectId::local`), informational to a restore; `base_page` is the header's base page, the
/// daemon's (`DaemonConfig::page`, measured once at boot).
fn archive_of(
  image: &slates_vfs::recover::VolumeImage,
  runs: &BTreeMap<(u64, u64), Vec<u8>>,
  volume_id: u64,
  base_page: usize,
) -> Result<slates_archive::Archive, &'static str> {
  let inodes: BTreeMap<u64, &InodeImage> =
    image.inodes.iter().map(|inode| (inode.no, inode)).collect();
  let mut chunks = Vec::new();
  let root = inodes.get(&image.root_no).ok_or(REFUSED_LAYOUT)?;
  let root_meta = meta_of(root);
  let mut visiting = Vec::new();
  let manifest = node_of(root, &inodes, runs, &mut chunks, &mut visiting)?;
  let chunk_max = chunks
    .iter()
    .map(|chunk: &slates_archive::Chunk| u32::try_from(chunk.raw_len).unwrap_or(u32::MAX))
    .max()
    .unwrap_or(0);
  Ok(slates_archive::Archive {
    base_page_size: u32::try_from(base_page).unwrap_or(u32::MAX),
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
        return Err(REFUSED_LAYOUT);
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
        let bytes = runs.get(&(inode.no, offset)).ok_or(REFUSED_LAYOUT)?;
        if u64::try_from(bytes.len()).ok() != Some(len) {
          return Err(REFUSED_LAYOUT);
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

#[cfg(test)]
mod tests {
  use super::*;

  /// A-200: a holder's install may be cut by a watch at most once an image. Do judge an image with no cut, the same image
  /// once cut, and a newer image after an older one was cut. Expect watched, not watched, watched: a holder silenced
  /// through every attempt is sent the image it was cut on unwatched, so it finishes or ends at its deadline instead of
  /// starting from nothing each time.
  #[test]
  fn an_install_is_cut_at_most_once_an_image() {
    let image = |position: u64| Head {
      epoch: slates_db::register::HostEpoch(1),
      position,
      identity: [u8::try_from(position).unwrap_or(0); 32],
    };
    assert!(watch_install(None, image(4)), "never cut: watched");
    assert!(
      !watch_install(Some(image(4)), image(4)),
      "cut on this image: sent unwatched"
    );
    assert!(
      watch_install(Some(image(4)), image(9)),
      "a newer image: watched again"
    );
  }

  /// A-240 with an install image beside the log: do ask whether the log covers candidates a and b when the last whole
  /// entry's anchors hold both keys and the install image's hold only a's. Expect not: a holder installed from the image
  /// keeps it as its log's first payload, and a successor adopting that holder could not open it for want of b's key. An
  /// image holding both keys covers, and so does the entry with no image. Before, only the entry was judged, and the same
  /// inputs answered that the log was covered (raft-chaos's review of the merged stack, 2026-11-07).
  #[test]
  fn an_install_image_short_of_a_candidates_key_does_not_cover_the_log() {
    let (a, b) = (11_u64, 22_u64);
    let wanted = [Some(a), Some(b)];
    assert!(
      log_covers(&wanted, true, Some(&[a, b]), None),
      "the whole entry alone covers both"
    );
    assert!(
      !log_covers(&wanted, true, Some(&[a, b]), Some(&[a])),
      "an image short of b's key does not cover the log"
    );
    assert!(
      log_covers(&wanted, true, Some(&[a, b]), Some(&[a, b])),
      "an image with both keys covers"
    );
    assert!(
      log_covers(&wanted, true, None, None),
      "a log shipped in the clear needs no key"
    );
  }

  /// A refused live takeover (A-200, A-240): do ask whether a successor fetches the adopted holder's log again, after
  /// each kind of rebuild refusal, at the head it was refused at and at a later one. Expect a refusal the log's bytes
  /// decide (open, record, apply, not whole first, layout) to stop the refetch until phase one adopts another head, and
  /// the lineage refusal (this node's own key record) to retry. Before, every refusal refetched the whole log every
  /// record period: one holder answered 389 fetches for a successor that could never open the log (CI, diag-14 run 3,
  /// 2026-11-07), and earlier runs moved 4.2 GB that way.
  #[test]
  fn a_rebuild_refused_at_a_head_is_not_fetched_again_until_the_head_moves() {
    let refused_at = Head {
      epoch: slates_db::register::HostEpoch(1),
      position: 184,
      identity: [7; 32],
    };
    let later = Head {
      position: 185,
      identity: [8; 32],
      ..refused_at
    };
    for kind in [
      REFUSED_OPEN,
      REFUSED_RECORD,
      REFUSED_APPLY,
      REFUSED_NOT_WHOLE_FIRST,
      REFUSED_LAYOUT,
    ] {
      assert!(
        !owes_refetch(Some(&(refused_at, kind)), refused_at),
        "{kind} at the same head is refused again by the same bytes"
      );
      assert!(
        owes_refetch(Some(&(refused_at, kind)), later),
        "{kind} at another head is fetched"
      );
    }
    assert!(
      owes_refetch(Some(&(refused_at, REFUSED_LINEAGE)), refused_at),
      "a lineage refusal is this node's own record, retried"
    );
    assert!(
      owes_refetch(None, refused_at),
      "nothing refused yet: fetched"
    );
  }

  /// A-200 with A-240's record: a live holder takes its acting owner's entry when the volume's record names it, though
  /// its own configuration does not place it; any other refusal stands. Do judge every authority answer with the record
  /// naming this node and without. Expect: placed, admitted either way; not a candidate, admitted only when named;
  /// not a member, not the acting owner and no acting owner, refused either way.
  #[test]
  fn a_recorded_holder_takes_its_owners_entry_though_its_configuration_lags() {
    for recorded in [false, true] {
      assert_eq!(live_admission(Ok(()), recorded), Ok(()));
      for refusal in [
        "content.refused.not_a_member",
        "content.refused.not_the_acting_owner",
        "content.refused.no_acting_owner",
      ] {
        assert_eq!(live_admission(Err(refusal), recorded), Err(refusal));
      }
    }
    assert_eq!(
      live_admission(Err(crate::fleet::CONTENT_NOT_A_CANDIDATE), true),
      Ok(()),
      "named by the record: taken"
    );
    assert_eq!(
      live_admission(Err(crate::fleet::CONTENT_NOT_A_CANDIDATE), false),
      Err(crate::fleet::CONTENT_NOT_A_CANDIDATE),
      "not named: refused"
    );
  }

  /// A-240's record read for a holder: do encode catalog values naming holders and not, and one without a record.
  /// Expect only the value naming this node to name it, and bytes of another register class to name nobody.
  #[test]
  fn a_catalog_record_names_its_live_holders() {
    let local = HostId(7);
    let value = |holders: Option<Vec<u64>>| {
      crate::catalog::CatalogValue {
        name: "v".to_owned(),
        size: slates_db::catalog::SizeClass::Bounded { limit: 1 << 20 },
        names: slates_db::catalog::NamePolicy::Exact,
        owner: slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 501,
        },
        require_locked: false,
        access: Vec::new(),
        live_shipped: true,
        live_holders: holders.map(|holders| slates_db::catalog::LiveHolders { epoch: 1, holders }),
      }
      .to_record_bytes()
    };
    assert!(names_holder(&value(Some(vec![3, 7])), local));
    assert!(!names_holder(&value(Some(vec![3, 9])), local));
    assert!(!names_holder(&value(None), local));
    assert!(!names_holder(&[0xff, 1, 2], local));
  }

  /// Format: the key generation the tests' keys carry.
  const GENERATION: u32 = 1;
  /// Shape: keys the tests may hold at once in the process's locked region (as `crates/cluster/tests/sealed.rs`).
  const KEYS: usize = 256;

  /// A-200 takeover of a live volume holding more than one chunk's worth: do seal publication bodies of sizes up to
  /// and past an archive chunk's cap (`slates_archive::format::MAX_CHUNK_BYTES`, 1 MiB) and open each under the same
  /// keys. Expect every body back byte for byte. A whole record carries the volume's content, so on KIND a 1,000-write
  /// volume's could not be opened by its successor: every refusal was `fleet.live.takeover_refused.open` (runs
  /// 42326682216, 42331191530), and the volume was never served.
  #[test]
  fn a_body_past_one_chunk_opens_as_it_was_sealed() {
    // The process's locked key region may already be set by another test; that is the same region.
    let _ = hyper_seal::lock_keys(KEYS);
    let tenant = hyper_seal::keys::WrappingKey::generate(GENERATION).unwrap();
    let (naming, _) = tenant.make_child().unwrap();
    let namer = hyper_seal::name::Namer::new(&naming).unwrap();
    let lineage = hyper_seal::keys::WrappingKey::generate(GENERATION).unwrap();
    let cap = usize::try_from(slates_archive::format::MAX_CHUNK_BYTES).unwrap();
    for len in [1, cap - 1, cap, cap + 1, 3 * cap + 17] {
      let body: Vec<u8> = (0..len).map(|at| u8::try_from(at % 251).unwrap()).collect();
      let pieces = seal_body(&lineage, &namer, body.clone(), 16_384).unwrap();
      assert_eq!(
        open_body(&lineage, &namer, pieces).as_ref(),
        Some(&body),
        "a body of {len} bytes opens as it was sealed"
      );
    }
  }
}
