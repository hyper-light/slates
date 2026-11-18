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

use slates_bridge_core::durable_bound::{DurableBound, GaveUp, Heard, Spans};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use slates_db::live::{
  Admitted, Bounds, Entry, Head, Install, LiveError, Next, Replica, Shipper, decode_reply,
  encode_reply,
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
/// seal's archive is placed then (`fleet::advance_seal`).
const BODY_CLEAR: u8 = 0;

/// Counter: a live volume's publication that could not be shipped as an entry, because its content could not be read,
/// sealed or appended; its next publication ships the whole volume.
const LIVE_REFUSED: &str = "fleet.live.refused";
/// Counter: whole records appended to a live log, each a copy of the volume's content.
const LIVE_WHOLE: &str = "fleet.live.whole";
/// Counter: the bytes of the whole records appended.
const LIVE_WHOLE_BYTES: &str = "fleet.live.whole_bytes";
/// Counter: the bytes of the changes (delta records) appended.
const LIVE_CHANGE_BYTES: &str = "fleet.live.change_bytes";
/// Counter: install images made from a publication's state (or a clean volume's, on the record period) for a holder
/// behind the retained tail, beside the log rather than on it ([`refresh_image`]).
const LIVE_IMAGE: &str = "fleet.live.install_image";
/// Counter: an install built to send a holder, the volume's whole state copied on the owner's shard.
const LIVE_INSTALL_BUILT: &str = "fleet.live.install_built";
/// Counter: an install a holder applied (its log replaced by the owner's whole state).
const LIVE_INSTALL_TAKEN: &str = "fleet.live.install_taken";
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
  state.live.adopted.remove(&id.bytes);
  // Its kept log goes with it (A-344): the next publication removes the piece.
  state.live.resumed.remove(&id.bytes);
  state.live.owned_dirty.insert(id.bytes);
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

/// The sealing of the whole entry `object`'s live log begins with on this holder, when it holds that log sealed: what a
/// fellow candidate hands the volume's successor its key from (A-295), never a sealing the asker sends.
pub(crate) fn held_sealing(
  state: &ShardState,
  object: ObjectId,
) -> Option<crate::head::HeadSealing> {
  let held = state.live.held.get(&object)?;
  sealed_body(held.entries.front()?)?.sealing
}

/// The position of the newest entry of the live log this node holds for `object` as a candidate, or `None` while it holds
/// none (`Daemon::fleet_live_held`).
pub(crate) fn held_position(state: &ShardState, object: ObjectId) -> Option<u64> {
  state
    .live
    .held
    .get(&object)?
    .replica
    .head()
    .map(|head| head.position)
}

pub(crate) fn let_go(state: &mut ShardState, object: ObjectId) {
  if let Some(held) = state.live.held.remove(&object) {
    state.store.budget.credit_replicated(held.charged);
    state.count(LIVE_LET_GO, 1);
  }
  // The claim its shard reserved goes at the next reconciliation, which finds no copy naming it (§4.2, g229).
  state.live.claims.remove(&object);
  state.live.claims_dirty.insert(partition_of(object));
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
  /// Acknowledgements that moved this volume's durability forward: a holder's acknowledged position advancing
  /// ([`fold_into`]), or its head's or catalog's placement gaining an acknowledgement or a newer sequence
  /// (`fleet::record_acks_in`). A reply held for durability gives up only after a whole wait of
  /// [`DURABLE_WAIT_NS`] in which this did not move ([`DurableBound`]).
  heard: u64,
  /// When this owner first asked a holder something on this volume's durability path since [`LiveVolume::heard`] last
  /// moved: a lane opened to send it entries or an install ([`dispatch`]), or its head's or catalog's placement asked
  /// (`fleet::ship_head`, through [`asked`]). `None` while nothing asked is unanswered. A held reply's silent window is
  /// given up only once this is a window old (A-301): the owner's own delay in asking is not its holders' silence.
  unanswered_since_ns: Option<u64>,
  /// A whole entry being sealed in turns, and the publications made since, queued to follow it in order ([`WholeBuild`],
  /// A-377): while it is here nothing is appended to the log.
  building: Option<Box<WholeBuild>>,
  /// When the publication being built reached the shipper, on the host's monotonic clock: taken by the entry appended for
  /// it ([`LiveVolume::newest`]), the start of its hops (`fleet.live.hop.*`).
  publishing_ns: Option<u64>,
  /// The newest entry appended, by position, with when its publication reached the shipper: the round its durability
  /// ends is timed hop by hop when the answer that makes it durable is folded ([`count_hops`]).
  newest: Option<(u64, u64)>,
  /// When the fold that made the newest entry durable ran, on the host's monotonic clock: a reply held for it times its
  /// own wake from there ([`wait_durable`], `fleet.live.hop.reply`).
  resolved_ns: Option<u64>,
  /// How long this volume's entries take from their publication to their durability (A-396, [`crate::live_hold`]): a
  /// reply held for the newest one holds this shard's window until it is expected ([`hold_for_answer`]).
  answers: crate::live_hold::AnswerWindow,
}

/// One candidate's copy of a live log as its shard's recovery image keeps it (A-200; §4.8 "Recovery": a warm restart
/// keeps every replica the node acknowledged): its owner, its fence, the head it holds and the sealed entries since the
/// last whole one.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldLive {
  object: [u8; 16],
  owner: u64,
  fence: u64,
  head: Option<HeldHead>,
  /// The slot of the first entry (A-270).
  first: u64,
  /// The entries, from `first`, in slot order. A head piece carries none; recovery joins them from their own pieces.
  entries: Vec<Vec<u8>>,
}

impl HeldLive {
  /// The object this copy is of.
  pub(crate) fn object(&self) -> [u8; 16] {
    self.object
  }

  /// The slot of its first entry.
  pub(crate) fn first(&self) -> u64 {
    self.first
  }

  /// This copy with `entries`, from its first slot, joined to it.
  pub(crate) fn with_entries(self, entries: Vec<Vec<u8>>) -> HeldLive {
    HeldLive { entries, ..self }
  }
}

/// A held copy's head, as [`HeldLive`] keeps it.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeldHead {
  epoch: u64,
  position: u64,
  identity: [u8; 32],
}

/// What an owner shard's recovery image keeps of a live log it ships (A-344; §4.10, §4.8 "Recovery"), one held piece a
/// volume (`content_holder`'s owned kind): the epoch the log ships under, the position its next entry takes, and the
/// later epoch a holder fenced this owner at. Every position below `next` may have been shipped, and `next` itself when
/// the publication carrying this piece was appended after it committed (an entry is appended only once its publication
/// commits, [`published`]), so a restarted owner resumes past `next`, under `epoch`: no position it may have shipped is
/// used again. Before A-344 the shipper lived in memory alone, and a restarted owner began again at position 0 under the
/// same epoch, a second chain at positions the first had used.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnedLive {
  /// The epoch the log ships under: a successor's is its promotion epoch, above its host's.
  pub(crate) epoch: u64,
  /// The position the next entry takes.
  pub(crate) next: u64,
  /// The later epoch a holder answered with: this owner was superseded (`None`: it was not).
  pub(crate) fenced: Option<u64>,
}

/// Where a live volume's shipper begins when it is made ([`planned`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
  /// A log never shipped from here: position 0.
  New,
  /// A takeover's adopted head (A-329): the first entry is one past it.
  Adopted(Head),
  /// This owner's own log before its restart (A-344): every position up to this one may have been shipped.
  Resumed(u64),
}

impl Start {
  /// The position the first entry takes.
  fn next(self) -> u64 {
    match self {
      Start::New => 0,
      Start::Adopted(head) => head.position.saturating_add(1),
      Start::Resumed(past) => past.saturating_add(1),
    }
  }
}

/// The epoch `id`'s shipper ships under and where it begins, were it made now: the greater of the host's epoch, the
/// epoch the volume's head was placed under (`placed_heads`: a successor's promotion epoch) and the epoch its own log
/// shipped under before a restart, which its kept piece holds where a restart forgets `placed_heads`; from the head a
/// takeover adopted, else past the log this owner shipped before its restart, else from nothing (A-329, A-344). The one
/// rule [`published`] makes a shipper by and [`owned_piece`] keeps before it.
fn planned(state: &ShardState, id: slates_db::catalog::VolumeId) -> (HostEpoch, Start) {
  let object = ObjectId(id.bytes);
  let mut epoch = state.fleet.configuration().host_epoch;
  if let Some(placed) = state.placed_heads.get(&object) {
    epoch = epoch.max(placed.epoch);
  }
  if let Some(head) = state.live.adopted.get(&id.bytes) {
    return (epoch, Start::Adopted(*head));
  }
  match state.live.resumed.get(&id.bytes) {
    Some(resumed) => (
      epoch.max(HostEpoch(resumed.epoch)),
      Start::Resumed(resumed.next),
    ),
    None => (epoch, Start::New),
  }
}

/// The piece this shard's recovery image is to keep of `volume`'s live log (A-344, [`OwnedLive`]), `captured` when the
/// publication being made carries the volume's record, so its entry follows once it commits: then the position that
/// entry takes, the shipper's next or, for a log not made again yet, the first the one [`planned`] would make takes;
/// otherwise the position the kept piece already holds, since no entry follows. The epoch and the fence as they stand.
/// `None` for a volume this shard ships no log of, or one with no kept position and nothing captured.
fn owned_piece(state: &ShardState, volume: [u8; 16], captured: bool) -> Option<OwnedLive> {
  let kept = state.live.kept.get(&volume).map(|kept| kept.next);
  if let Some(live) = state.live.shipping.get(&volume) {
    let next = if captured {
      // Past a whole entry being sealed and the publications queued behind it, which take their positions first (A-377).
      live
        .shipper
        .next_position()?
        .saturating_add(pending_entries(live))
    } else {
      kept?
    };
    return Some(OwnedLive {
      epoch: live.shipper.epoch().0,
      next,
      fenced: live.fenced.map(|fence| fence.0),
    });
  }
  let id = slates_db::catalog::VolumeId { bytes: volume };
  if !is_live(state, id) {
    return None;
  }
  let (epoch, start) = planned(state, id);
  let next = if captured { start.next() } else { kept? };
  Some(OwnedLive {
    epoch: epoch.0,
    next,
    fenced: state
      .live
      .resumed
      .get(&volume)
      .and_then(|resumed| resumed.fenced),
  })
}

/// What a publication carries of the kept pieces (A-344): for each volume it captured (its entry follows) and each one
/// marked since the last ([`LiveState::owned_dirty`]), its piece now when that differs from the one committed, or
/// `None` to remove a committed piece of a log let go. Only the changed volumes, never every live volume a write.
pub(crate) fn owned_changes(state: &ShardState) -> Vec<([u8; 16], Option<OwnedLive>)> {
  let captured: std::collections::BTreeSet<[u8; 16]> =
    state.live.captured.iter().map(|(id, _)| id.bytes).collect();
  captured
    .iter()
    .chain(state.live.owned_dirty.iter())
    .copied()
    .collect::<std::collections::BTreeSet<[u8; 16]>>()
    .into_iter()
    .filter_map(|volume| {
      let piece = owned_piece(state, volume, captured.contains(&volume));
      let committed = state.live.kept.get(&volume).copied();
      (piece != committed).then_some((volume, piece))
    })
    .collect()
}

/// Every kept piece a checkpoint carries (A-344): each committed one, as [`owned_changes`] changes it.
pub(crate) fn owned_pieces(state: &ShardState) -> Vec<([u8; 16], OwnedLive)> {
  let mut pieces = state.live.kept.clone();
  for (volume, piece) in owned_changes(state) {
    match piece {
      Some(piece) => {
        pieces.insert(volume, piece);
      }
      None => {
        pieces.remove(&volume);
      }
    }
  }
  pieces.into_iter().collect()
}

/// A committed publication carried `owned` (A-344): the kept pieces become them (`everything`, a checkpoint's whole set)
/// or change by them (a delta's), and the marks are spent.
pub(crate) fn owned_committed(
  state: &mut ShardState,
  owned: Vec<([u8; 16], Option<OwnedLive>)>,
  everything: bool,
) {
  if everything {
    state.live.kept.clear();
  }
  for (volume, piece) in owned {
    match piece {
      Some(piece) => {
        state.live.kept.insert(volume, piece);
      }
      None => {
        state.live.kept.remove(&volume);
      }
    }
  }
  state.live.owned_dirty.clear();
}

/// Takes back, after a warm restart, the logs this shard shipped before it (A-344): each is kept as committed and made
/// again past its kept position at its next publication ([`published`]); one fenced before stays fenced.
pub(crate) fn restore_owned(state: &mut ShardState, owned: Vec<([u8; 16], OwnedLive)>) {
  for (volume, kept) in owned {
    state.live.kept.insert(volume, kept);
    state.live.resumed.insert(volume, kept);
    state.count(LIVE_RESUMED, 1);
  }
}

/// Counter: live logs a warm restart took back from the recovery image, each resumed past its kept position (A-344).
const LIVE_RESUMED: &str = "fleet.live.resumed";

/// Holds again, after a warm restart, the copies a recovered image kept (`RecoveredHeld`'s live copies, in
/// [`crate::content_holder::restore_held_registers`]), each with its fence.
pub(crate) fn restore_copies(state: &mut ShardState, copies: Vec<HeldLive>) {
  for copy in copies {
    let object = ObjectId(copy.object);
    let head = copy.head.map(|head| Head {
      epoch: HostEpoch(head.epoch),
      position: head.position,
      identity: head.identity,
    });
    let bytes = copy
      .entries
      .iter()
      .map(Vec::len)
      .fold(0usize, usize::saturating_add);
    let mut held = Held {
      replica: Replica::recovered(HostId(copy.owner), object, HostEpoch(copy.fence), head),
      entries: copy.entries.into(),
      bytes,
      first: copy.first,
      arrivals: crate::live_hold::StreamWindow::default(),
      charged: 0,
    };
    // A copy made after the restart starts above every slot the recovered image carried.
    let (_, end) = held.slots();
    state.live.slot_floor = state.live.slot_floor.max(end);
    // Its claim went with the daemon (§4.2, g229): the log is charged whole here until the next record period admits the
    // claim again ([`reconcile_claims`]). Held all the same when the shard has no room for it: it was acknowledged, and
    // a successor may need it; counted.
    let mut room = Room {
      store: &mut state.store,
      claimed: 0,
    };
    let kept = held.bytes;
    let charged = room.settle(&mut held, kept).is_ok();
    state.live.claims_dirty.insert(partition_of(object));
    state.live.held.insert(object, held);
    state.count(LIVE_RESTORED, 1);
    if !charged {
      state.count(LIVE_RESTORED_UNCHARGED, 1);
    }
  }
}

/// Counter: live copies a warm restart held again from the recovery image.
const LIVE_RESTORED: &str = "fleet.live.restored";
/// Counter: a copy a warm restart held again though this shard had no room to charge its log (§4.2, g229): kept, since it
/// was acknowledged; its next entry is charged or refused `NoRoom`.
/// Format: a counter name in the daemon's status report.
const LIVE_RESTORED_UNCHARGED: &str = "fleet.live.restored_uncharged";

/// Counter: a holder's answer withheld because the publication that would keep what it acknowledged did not commit;
/// the owner re-ships it.
const LIVE_UNPUBLISHED: &str = "fleet.live.unpublished";

/// Commits this shard's recovery image before a holder acknowledges what it now keeps (§4.8 "Recovery", as content and
/// register holders do): `reply`, or nothing when the publication did not commit, which the owner reads as no answer
/// and re-ships. A warm restart must hold again everything acknowledged, and its fence.
fn acknowledged(state: &mut ShardState, reply: Vec<u8>) -> Vec<u8> {
  if crate::verbs::publish_shard(state).is_err() {
    state.count(LIVE_UNPUBLISHED, 1);
    return Vec::new();
  }
  reply
}

/// What a candidate keeps of one live volume.
#[derive(Debug)]
pub(crate) struct Held {
  replica: Replica,
  /// The sealed payloads since the last whole one (that one first), in order.
  entries: VecDeque<Vec<u8>>,
  /// Their bytes.
  bytes: usize,
  /// The slot of the first entry (A-270): entry `i` is slot `first + i`, its held piece's position. Entries are only
  /// pushed after the last or all cleared ([`Held::clear_entries`], which moves it past them), so a slot never names
  /// two entries, and a delta carries a copy's new slots and the ones it dropped.
  first: u64,
  /// The owner's entries as a stream (A-396): the gaps between those this copy took, and its run, which say how long
  /// this holder's shard holds its idle window for the next one ([`serve_taken`]). Memory only: a restarted holder
  /// learns it again.
  arrivals: crate::live_hold::StreamWindow,
  /// The bytes of these entries past the claim this node reserved for the volume (§4.2, g229; [`Room`]), charged on
  /// this control shard as content held for another owner (`ShardBudget::charge_replicated`). The entries within the
  /// claim are charged against it: before a takeover the claim's room holds the log, after it the volume.
  charged: u64,
}

impl Held {
  /// Drops every entry: the next one pushed takes the slot after the last dropped.
  fn clear_entries(&mut self) {
    let dropped = u64::try_from(self.entries.len()).unwrap_or(u64::MAX);
    self.first = self.first.saturating_add(dropped);
    self.entries.clear();
    self.bytes = 0;
  }

  /// The slots its entries hold: `first..end`.
  pub(crate) fn slots(&self) -> (u64, u64) {
    let held = u64::try_from(self.entries.len()).unwrap_or(u64::MAX);
    (self.first, self.first.saturating_add(held))
  }

  /// The entry at `slot`, when it holds one.
  pub(crate) fn entry_at(&self, slot: u64) -> Option<&[u8]> {
    let index = usize::try_from(slot.checked_sub(self.first)?).ok()?;
    self.entries.get(index).map(Vec::as_slice)
  }

  /// This copy's head piece's fields (A-270): its owner, fence, head and first slot, with no entries.
  pub(crate) fn head_of(&self, object: ObjectId) -> HeldLive {
    HeldLive {
      object: object.0,
      owner: self.replica.owner().0,
      fence: self.replica.fence().0,
      head: self.replica.head().map(|head| HeldHead {
        epoch: head.epoch.0,
        position: head.position,
        identity: head.identity,
      }),
      first: self.first,
      entries: Vec::new(),
    }
  }
}

#[cfg(test)]
impl Held {
  /// Test support (A-270's oracle): a copy of `object` for `owner` holding nothing, its first slot `first`.
  pub(crate) fn for_test(owner: HostId, object: ObjectId, first: u64) -> Held {
    Held {
      replica: Replica::new(owner, object),
      entries: VecDeque::new(),
      bytes: 0,
      first,
      arrivals: crate::live_hold::StreamWindow::default(),
      charged: 0,
    }
  }

  /// Test support: keeps `payload` after the last entry, as an accepted entry is kept.
  pub(crate) fn push_for_test(&mut self, payload: Vec<u8>) {
    self.bytes = self.bytes.saturating_add(payload.len());
    self.entries.push_back(payload);
  }

  /// Test support: drops every entry, as a whole entry or an install does.
  pub(crate) fn clear_for_test(&mut self) {
    self.clear_entries();
  }
}

/// The node's live-shipping state, on every shard: the volumes this shard owns and ships, and (on the control shard)
/// what this node holds for others.
#[derive(Debug, Default)]
pub(crate) struct LiveState {
  /// The live volumes this shard owns, by volume bytes.
  pub(crate) shipping: BTreeMap<[u8; 16], LiveVolume>,
  /// What this node holds as a candidate, by object. Tracked (`crate::tracked`), so a publication carries only the
  /// copies reached since the last (A-270).
  pub(crate) held: crate::tracked::Tracked<ObjectId, Held>,
  /// The first slot a copy made now takes (A-270): at or above every slot a committed publication carried for any copy,
  /// so a copy dropped and made again never reuses a published entry's key.
  pub(crate) slot_floor: u64,
  /// On the control shard: the bytes each held log's claim holds, by object (§4.2, g229): what the volume's shard
  /// reserved as this node took the log's first entry (`verbs::admit_candidate_claim`), against which the log is charged
  /// ([`Room`]). Learned at the admission and at each reconciliation ([`reconcile_claims`]); bounded by the copies held.
  pub(crate) claims: BTreeMap<ObjectId, u64>,
  /// On the control shard: the claims' admissions running, by object, each with when it began ([`admit_claim`]): one at
  /// a time an object, and one older than its call's deadline was a dropped attempt's, which the next takes over.
  /// Bounded by the copies held.
  admitting: BTreeMap<ObjectId, u64>,
  /// On the control shard: the partitions whose claims the next record period reconciles ([`reconcile_claims`]): one
  /// whose copy was let go, one whose admission went unanswered, and each a warm restart's copies route to. Bounded by
  /// the partitions.
  claims_dirty: BTreeSet<u16>,
  /// On the control shard: the claims admitted at an owner's ask before it turned the volume's live shipping on
  /// ([`serve_admit`]), by object, each with when: kept by every reconciliation until the log's first entry is held, or
  /// for the failover delay at most, past which the owner refused its own live-ship (another candidate had no room) or
  /// was lost, and a successor that ships asks again. Bounded by the claims.
  promised: BTreeMap<ObjectId, u64>,
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
  /// The asynchronous outcomes whose reply may wait for a live volume's durability (a mapped file's write-back, a
  /// checkout), each seated among the held replies when its task began ([`take_seat`]), by request: counted against
  /// [`awaiting_bound`] from then, so the bound is met before the task changes anything, never after its effects
  /// commit. A seat becomes the outcome's wait ([`defer`]) or is given back when it completes without one
  /// ([`release_seat`]). Bounded with [`LiveState::awaiting`].
  seats: std::collections::BTreeSet<(u64, u32, u32)>,
  /// What the transport barrier that just ran on this shard published of a live volume, for its caller to wait on
  /// ([`note_barrier`], [`take_barrier`]): set and taken within one synchronous serve.
  barrier: Option<(slates_db::catalog::VolumeId, u64)>,
  /// The transport replies parked until a volume is durable, by volume: their wakers, woken as it advances.
  parked: BTreeMap<[u8; 16], Vec<std::task::Waker>>,
  /// On the control shard: dispatches whose holders were still in flight at their return, each with the owner shard and
  /// the volume, holding those holders' sessions until they return ([`drain_stragglers`]). Bounded by the sessions: each
  /// holds at least one.
  stragglers: Vec<(u16, ObjectId, slates_cluster::Stragglers)>,
  /// The head each taken-over live volume's rebuild was last refused at, and why ([`next_fetch`]), on the control
  /// shard: its log is not fetched again until phase one adopts another head. One entry a pending materialization, removed
  /// when it is served.
  pub(crate) refused_rebuilds: BTreeMap<ObjectId, Refused>,
  /// Test support (`Daemon::inject_live_period_quantum`, never set in production): the quantum the period's slices are
  /// cut by in place of the shard's step quantum ([`period_slice`]).
  pub(crate) period_quantum_ns: Option<u64>,
  /// The head each live volume this shard took over was adopted at (A-329), until the volume's shipper is made at its
  /// first publication, which starts from it ([`published`]). One entry a taken-over live volume not yet published,
  /// removed then or when the volume is destroyed ([`forget`]), so bounded by the volumes this shard serves.
  adopted: BTreeMap<[u8; 16], Head>,
  /// The live logs this shard's recovery image says it shipped before a restart, by volume (A-344): restored at
  /// recovery ([`restore_owned`]) and taken when the volume's shipper is made again ([`published`]), which resumes past
  /// each. One entry a live volume this shard owns, so bounded by the volumes it serves.
  resumed: BTreeMap<[u8; 16], OwnedLive>,
  /// The live volumes whose kept piece the next publication must carry beside the ones it captures (A-344,
  /// [`owned_changes`]): a fence learned, or the volume let go and its piece removed. Cleared when a publication
  /// commits ([`owned_committed`]); bounded by the volumes this shard serves.
  owned_dirty: std::collections::BTreeSet<[u8; 16]>,
  /// The kept pieces the committed journal holds, by volume (A-344): what a restart would read back. Changed only when a
  /// publication commits ([`owned_committed`]), so a refused one leaves it as the journal is. Bounded by the volumes this
  /// shard serves.
  kept: BTreeMap<[u8; 16], OwnedLive>,
  /// On the control shard: the dispatches it carries as a stream (A-396), whose window it holds after each one's lanes end
  /// so the next of a closed loop finds it polling ([`send_from_control`]).
  pub(crate) dispatches: crate::live_hold::StreamWindow,
}

/// Notes, after a transport's barrier published `volume`, the position its reply must wait for ([`wait_durable`]).
pub(crate) fn note_barrier(state: &mut ShardState, volume: slates_db::catalog::VolumeId) {
  state.live.barrier = durable_target(state, volume).map(|target| (volume, target));
}

/// Takes what [`note_barrier`] noted.
pub(crate) fn take_barrier(state: &mut ShardState) -> Option<(slates_db::catalog::VolumeId, u64)> {
  state.live.barrier.take()
}

/// Derived: how long a transport's reply waits for durability with nothing heard before it is answered that it is not
/// durable (NFS `JUKEBOX`, FUSE and virtio-fs `EIO`): the daemon's liveness budget, past which a holder that has not
/// answered is not a live one to wait for. A window, not a deadline: each one in which the volume's durability moved
/// forward starts another ([`DurableBound`]).
const DURABLE_WAIT_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;

/// Format: nanoseconds in a millisecond, for the milliseconds a give-up's record reads in.
pub(crate) const NS_PER_MS: u64 = 1_000_000;

/// `object`'s volume heard an acknowledgement that moved its durability forward: its head's or its catalog's placement
/// gained an acknowledgement or a newer sequence (`fleet::record_acks_in`). Counted on the volume's shipper
/// ([`LiveVolume::heard`]); nothing for a volume this shard does not ship.
pub(crate) fn heard(state: &mut ShardState, object: ObjectId) {
  let volume = object.placement_key().0;
  if let Some(live) = state.live.shipping.get_mut(&volume) {
    note_heard(live);
  }
}

/// `live` heard something that moved its durability forward: counted ([`LiveVolume::heard`]), and every ask before it
/// is answered ([`LiveVolume::unanswered_since_ns`]).
fn note_heard(live: &mut LiveVolume) {
  live.heard = live.heard.saturating_add(1);
  live.unanswered_since_ns = None;
}

/// Whether this shard ships `object`'s volume live, so that an ask of its holders is noted for its held replies
/// ([`asked`]).
pub(crate) fn ships(state: &ShardState, object: ObjectId) -> bool {
  state.live.shipping.contains_key(&object.placement_key().0)
}

/// This owner is asking `object`'s holders to place its volume's head or catalog (`fleet::ship_head`, a round about to
/// be sent): noted on the volume's shipper as an ask the volume's held replies may hold its holders to (A-301); nothing
/// for a volume this shard does not ship.
pub(crate) fn asked(state: &mut ShardState, object: ObjectId) {
  let volume = object.placement_key().0;
  if let Some(live) = state.live.shipping.get_mut(&volume) {
    note_asked(live, slates_rt::futures::now_ns());
  }
}

/// `live`'s owner asked a holder something at `now_ns`: the oldest ask unanswered, unless an older one is
/// ([`LiveVolume::unanswered_since_ns`]).
fn note_asked(live: &mut LiveVolume, now_ns: u64) {
  live.unanswered_since_ns.get_or_insert(now_ns);
}

/// Derived: how far apart the looks are that confirm a held reply's silent window
/// (`slates_bridge_core::durable_bound::Spans::look_ns`): the heartbeat, the cadence the control shard beats the
/// anchor's word at (§4.14), so a control shard that runs waits in its driver at least once between two looks.
const DURABLE_LOOK_NS: u64 = crate::daemon::HEARTBEAT_NS;

/// The spans a held reply's wait is judged in on this shard: windows of [`DURABLE_WAIT_NS`], looks [`DURABLE_LOOK_NS`]
/// apart while a silence is confirmed, and at most the failover delay from the hold, this node's live estimate (A-363;
/// the operator's 10 s placeholder before; the supervisor's landing condition of 2026-11-11, banned item 8).
fn durable_spans(state: &ShardState) -> Spans {
  Spans {
    window_ns: DURABLE_WAIT_NS,
    total_ns: state.failover_ns,
    look_ns: DURABLE_LOOK_NS,
  }
}

/// The first window of a wait for `volume`'s durability beginning at `now_ns`, under the one rule every transport judges
/// a held durable reply by (`slates_bridge_core::durable_bound`). Under load on adacachy2 a healthy `fsync`'s bytes were
/// already held by a candidate while the volume's head placement was in flight, and the old single deadline answered
/// `EIO` (3 of 10 runs, every run that reached it).
pub(crate) fn begin_durable_bound(
  state: &ShardState,
  volume: slates_db::catalog::VolumeId,
  now_ns: u64,
) -> DurableBound {
  DurableBound::begin(heard_now(state, volume), now_ns, durable_spans(state))
}

/// Whether, and why, `bound`'s wait for `volume` has given up at `now_ns`, judged against what the volume has heard on
/// its durability path and how often its receiver has idled ([`heard_now`]).
pub(crate) fn judge_durable_bound(
  state: &ShardState,
  volume: slates_db::catalog::VolumeId,
  bound: &mut DurableBound,
  now_ns: u64,
) -> Option<GaveUp> {
  bound.judge(heard_now(state, volume), now_ns)
}

/// What a reply held for `volume`'s durability is judged against now: the acknowledgements the volume has heard
/// ([`heard_of`]), the waits the daemon's control shard has entered in its driver, each with nothing ready to run, and
/// the owner's oldest unanswered ask ([`LiveVolume::unanswered_since_ns`]). The control shard holds the sessions every
/// holder's acknowledgement arrives on (`send_from_control`), and hands each one to the volume's shard as a task there;
/// so once it has waited twice after a moment, every acknowledgement that reached it before then has been read and
/// handed on (the rule's own reasoning, `slates_bridge_core::durable_bound`). Read off its registry pulse, with no shard
/// round trip.
pub(crate) fn heard_now(state: &ShardState, volume: slates_db::catalog::VolumeId) -> Heard {
  let control = state.shards.first().copied().unwrap_or(state.shard);
  let live = state.live.shipping.get(&volume.bytes);
  Heard {
    acknowledged: heard_of(state, volume),
    receiver_idles: slates_rt::registry::with_entry(control, |entry| entry.pulse.waits())
      .unwrap_or(0),
    unanswered_since_ns: live.and_then(|live| live.unanswered_since_ns),
  }
}

/// [`LiveVolume::heard`] of `volume`, or zero when this shard does not ship it.
fn heard_of(state: &ShardState, volume: slates_db::catalog::VolumeId) -> u64 {
  state
    .live
    .shipping
    .get(&volume.bytes)
    .map_or(0, |live| live.heard)
}

/// Waits on this (the volume's owner) shard, without blocking it, until `volume`'s log is durable at `target`, or a
/// window of [`DURABLE_WAIT_NS`] has passed with nothing heard ([`DurableBound`]): whether it became durable. The
/// caller answers a stable write or `COMMIT` only then, and otherwise tells the client to try again: a barrier never
/// claims what `f` candidates do not hold.
pub(crate) async fn wait_durable(volume: slates_db::catalog::VolumeId, target: u64) -> bool {
  let Some(mut bound) =
    state::with_state(|s| begin_durable_bound(s, volume, slates_rt::futures::now_ns()))
  else {
    return false;
  };
  let mut gave_up = None;
  // A window at a time ([`DurableBound`]): one that ends with the volume's durability moved forward is followed by
  // another, one that ends with nothing heard gives up.
  loop {
    let parked = std::future::poll_fn(|cx| {
      let settled = state::with_state(|s| durability(s, volume, target, Some(cx.waker())))
        .unwrap_or(Durability::Never);
      if settled == Durability::Pending {
        std::task::Poll::Pending
      } else {
        std::task::Poll::Ready(())
      }
    });
    let left = bound
      .next_look_ns()
      .saturating_sub(slates_rt::futures::now_ns());
    match slates_rt::futures::within(left, parked).await {
      Ok(Some(())) | Err(_) => break,
      Ok(None) => {
        let given_up = state::with_state(|s| {
          judge_durable_bound(s, volume, &mut bound, slates_rt::futures::now_ns())
        })
        .unwrap_or(Some(GaveUp::Silent));
        if let Some(why) = given_up {
          gave_up = Some(why);
          break;
        }
      }
    }
  }
  let durable = state::with_state(|s| {
    let durable = durability(s, volume, target, None) == Durability::Durable;
    if !durable {
      // What durability still lacked when the wait ended (2026-11-09): the client is told its write is not durable.
      slates_machine::log_line!(
        "slates-server: a held durable reply gave up after {} windows of {} ms, {} (shard {}): {}",
        bound.windows(),
        DURABLE_WAIT_NS / NS_PER_MS,
        gave_up.map_or("its wait ended", GaveUp::named),
        s.shard,
        durability_report(s, volume, target)
      );
    }
    durable
  })
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
    // The reply's own wake, from the fold that made the volume's newest entry durable ([`HOP_REPLY`]).
    let resolved = s
      .live
      .shipping
      .get_mut(&volume.bytes)
      .and_then(|live| live.resolved_ns.take());
    if let (true, Some(resolved_ns)) = (durable, resolved) {
      count_hop(
        s,
        HOP_REPLY,
        slates_machine::clock::monotonic_ns().saturating_sub(resolved_ns),
      );
    }
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

/// What a reply waiting for `volume`'s log to be durable at `target` is still waiting on, as one line for a log: the
/// owner's head and durable position, the last whole entry, each holder's acknowledged head, the lanes in flight and how
/// long each has run, the holders found unreachable, each condition [`answerable`] asks, and how long the owner's oldest
/// unanswered ask has waited. Asked when a held reply gives up, so a refusal says which part of durability it lacked.
pub(crate) fn durability_report(
  state: &ShardState,
  volume: slates_db::catalog::VolumeId,
  target: u64,
) -> String {
  let now = slates_rt::futures::now_ns();
  let placed = head_placed(state, volume.bytes);
  let recorded = active_set_recorded(state, volume.bytes);
  let covered = covers_candidates(state, volume.bytes);
  let Some(live) = state.live.shipping.get(&volume.bytes) else {
    return format!("target {target}: no shipper; head placed {placed}");
  };
  let holders: Vec<String> = live
    .shipper
    .holders()
    .iter()
    .map(|holder| {
      format!(
        "{}@{:?}",
        holder.0,
        live.shipper.held_by(*holder).map(|head| head.position)
      )
    })
    .collect();
  let lanes: Vec<String> = live
    .lanes
    .iter()
    .map(|(holder, began)| format!("{}:{}ms", holder.0, now.saturating_sub(*began) / NS_PER_MS))
    .collect();
  let unreachable: Vec<u64> = live.unreachable.iter().map(|holder| holder.0).collect();
  // How long the owner's oldest unanswered ask has waited (A-301): none, when the owner had asked nothing unanswered.
  let unanswered_ms = live
    .unanswered_since_ns
    .map(|asked| now.saturating_sub(asked) / NS_PER_MS);
  format!(
    "target {target}; head {:?}; durable {:?}; whole {:?}; owes whole {}; holders {holders:?}; lanes {lanes:?}; \
     unreachable {unreachable:?}; head placed {placed}; active set recorded {recorded} (at {:?}); covers candidates \
     {covered}; fenced {:?}; oldest unanswered ask {unanswered_ms:?} ms",
    live.shipper.head().map(|head| head.position),
    live.shipper.durable(),
    live.whole_entry.map(|head| head.position),
    live.owes_whole,
    live.recorded,
    live.fenced,
  )
}

/// Derived: the window a held reply's wait for durability is judged in (`slates_bridge_core::durable_bound`) before its
/// transport answers that it is not durable (NFS `JUKEBOX`, FUSE and virtio-fs `EIO`, WinFsp
/// `STATUS_UNEXPECTED_IO_ERROR`): [`DURABLE_WAIT_NS`]. Asked by those transports for their give-up records, so compiled
/// where they are.
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
  hold_for_answer(state, volume);
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
  /// When the reply was held, on the shard's clock: past the failover SLO from then the verb is refused
  /// ([`judge_held`], A-354), and how long it was held is told with it.
  held_since_ns: u64,
}

impl Awaiting {
  /// A reply held from `now_ns`.
  fn held((route, origin, id, reply): HeldReply, now_ns: u64) -> Awaiting {
    Awaiting {
      route,
      origin,
      id,
      reply,
      held_since_ns: now_ns,
    }
  }
}

/// What a held reply is made of ([`Awaiting::held`]): where to send it, its client's completion key and the reply.
type HeldReply = (
  Option<crate::merge_service::ReplyRoute>,
  u64,
  slates_wire::request::RequestId,
  slates_ipc::protocol::ReplyBody,
);

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
/// Counter: an asynchronous outcome seated among the held replies as its task began ([`take_seat`]).
const LIVE_SEATED: &str = "fleet.live.seated";
/// Counter: an outcome held past the bound because it began with no seat, its volume turned live-shipped while it ran
/// ([`defer`]): held rather than refused after its effects committed.
const LIVE_HELD_PAST_BOUND: &str = "fleet.live.held_past_bound";

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
  if live.building.is_some() {
    // Nothing is appended while a whole entry seals (A-377): it takes the next position, each publication queued behind
    // it one after, and one refused meanwhile the whole state the volume then owes after them.
    return Some(
      live
        .shipper
        .next_position()?
        .saturating_add(pending_entries(live))
        .saturating_sub(1)
        .saturating_add(u64::from(live.owes_whole)),
    );
  }
  owed_position(
    live.shipper.head().map(|head| head.position),
    live.owes_whole,
    live.fenced.is_some(),
    answerable(state, volume.bytes),
  )
}

/// The decision of [`durable_target`], over what it reads of the volume's shipper: its `head` position, whether it
/// `owes_whole` a record for a publication that could not become an entry, whether this owner is `fenced`, and the
/// position its writers may be answered at (`answerable`, `None` while the fleet does not know the volume: a takeover
/// could not find it, so nothing written to it is durable). An owed record is waited for whether the volume's head is
/// placed or not: before 2026-11-18 the unplaced case returned the head as it stood, so a reply waited for nothing when
/// nothing had shipped and one position short of its write otherwise.
fn owed_position(
  head: Option<u64>,
  owes_whole: bool,
  fenced: bool,
  answerable: Option<u64>,
) -> Option<u64> {
  if fenced {
    // Nothing this owner holds becomes durable again: the caller defers, and the deferral refuses.
    return Some(head.unwrap_or(0));
  }
  let target = if owes_whole {
    head.map_or(Some(0), |head| head.checked_add(1))?
  } else {
    head?
  };
  if !owes_whole && answerable.is_some_and(|durable| durable >= target) {
    return None;
  }
  Some(target)
}

/// The refusal a verb about to run on this (its owner) shard meets before it runs, when its reply could be held here
/// and the shard already holds its bound of such replies ([`awaiting_bound`]): `Overloaded`, counted. Judged before
/// the verb, so a refused verb changed nothing and its resend under the same id runs it once, as on a plain volume. A
/// verb could be held when it changes a live-shipped volume ([`defer`]), or destroys one while the fleet places
/// tombstones (`f > 0`, [`defer_destroy`]). Until 2026-11-18 the bound was judged only in [`defer`], after the verb's
/// effects had committed with no completion record: the resend ran the verb again over them, so a directory's resend
/// was refused `AlreadyExists` and an append landed twice (g232's finding).
pub(crate) fn refuse_over_bound(
  state: &mut ShardState,
  body: &slates_ipc::protocol::RequestBody,
) -> Option<slates_ipc::protocol::ReplyBody> {
  if held_replies(state) < awaiting_bound(state) {
    return None;
  }
  let volume = body.volume().map(crate::verbs::to_db_volume)?;
  let destroy = matches!(body, slates_ipc::protocol::RequestBody::Destroy { .. })
    && state.fleet.configuration().quorum.f > 0;
  let live = body.mutates_shard_image() && is_live(state, volume);
  if !(destroy || live) {
    return None;
  }
  state.count(LIVE_OVER_BOUND, 1);
  Some(slates_ipc::protocol::ReplyBody::Refused {
    refusal: slates_ipc::protocol::Refusal::Overloaded { shard: state.shard },
  })
}

/// Holds a verb's reply until `volume`'s log is durable at `target` (A-200): its effects are committed, its completion
/// is recorded and its reply delivered then ([`resolve`]): `None`. The shard's bound on such replies was met before the
/// effects: a recorded verb's by [`refuse_over_bound`], in the turn that runs it; an asynchronous outcome's by the seat
/// its task took as it began ([`take_seat`]), which this wait now fills. So nothing is refused here for room: until
/// 2026-11-18 an asynchronous outcome met the bound here, after its effects had committed, so a mapped file's sync
/// refused `Overloaded` had written its stores (GAPS). An outcome that began with no seat (a map's release that found no
/// room, [`seat_if_room`], or one whose volume turned live-shipped while it ran) is held past the bound, counted, rather
/// than refused after its commit; each is a request in flight, so the excess is bounded by them. The reply to give now
/// instead, `StaleEpoch`, when this owner was superseded and nothing it holds becomes durable.
pub(crate) fn defer(
  state: &mut ShardState,
  (volume, target): (slates_db::catalog::VolumeId, u64),
  (origin, id): (u64, slates_wire::request::RequestId),
  route: Option<crate::merge_service::ReplyRoute>,
  reply: slates_ipc::protocol::ReplyBody,
) -> Option<slates_ipc::protocol::ReplyBody> {
  let seated = state.live.seats.remove(&(origin, id.client, id.sequence));
  if let Some(current) = fenced(state, volume.bytes) {
    state.count(LIVE_FENCED, 1);
    return Some(stale_epoch(current));
  }
  if !seated && held_replies(state) >= awaiting_bound(state) {
    state.count(LIVE_HELD_PAST_BOUND, 1);
  }
  let held = Awaiting::held((route, origin, id, reply), slates_rt::futures::now_ns());
  state
    .live
    .awaiting
    .entry((volume.bytes, target))
    .or_default()
    .push(held);
  state
    .live
    .awaiting_by_request
    .insert((origin, id.client, id.sequence), (volume.bytes, target));
  state.count(LIVE_AWAITED, 1);
  hold_for_answer(state, volume.bytes);
  None
}

/// Asks this (the owner) shard to hold its idle window until the newest entry of `volume` is expected durable (A-396): a
/// reply held for it then finds the shard polling when the holders' answer is folded in, not parked behind a wake. The
/// window is the volume's own, learned from its entries' rounds, publication to durability on the host's clock
/// ([`LiveVolume::answers`], [`count_hops`]); nothing before the first is timed. The shard keeps the hold only where it
/// pays (A-387, [`crate::live_hold::hold`]).
fn hold_for_answer(state: &mut ShardState, volume: [u8; 16]) {
  let Some(live) = state.live.shipping.get(&volume) else {
    return;
  };
  let (Some((_, published_ns)), Some(window)) = (live.newest, live.answers.window()) else {
    return;
  };
  let left =
    crate::live_hold::remaining(published_ns, window, slates_machine::clock::monotonic_ns());
  count_hold(state, LIVE_HOLD_ANSWER, crate::live_hold::hold(left));
}

/// Counts a live hold asked at `site` (A-396) by what the shard did with it (`held`, [`crate::live_hold::hold`]): the
/// site's counter for a hold asked of the shard, and [`LIVE_HOLD_DECLINED`] as well when the shard declined it, its
/// measured idle turn no shorter than its wake (A-387). Nothing for nothing asked.
fn count_hold(state: &mut ShardState, site: &'static str, held: Option<bool>) {
  let Some(kept) = held else {
    return;
  };
  state.count(site, 1);
  if !kept {
    state.count(LIVE_HOLD_DECLINED, 1);
  }
}

/// Counter: a reply held for a live volume's durability asked its owner shard to hold its idle window until the answer
/// was expected (A-396, [`hold_for_answer`]).
const LIVE_HOLD_ANSWER: &str = "fleet.live.hold.answer";
/// Counter: the control shard was asked to hold its idle window through a lane's exchange until its holder's answer was
/// expected (A-396, [`ship_lane`]).
const LIVE_HOLD_EXCHANGE: &str = "fleet.live.hold.exchange";
/// Counter: the control shard was asked to hold its idle window after a dispatch for the next one of its stream (A-396,
/// [`send_from_control`]).
const LIVE_HOLD_DISPATCHES: &str = "fleet.live.hold.dispatches";
/// Counter: a holder was asked to hold its idle window after an entry for the next one of its owner's stream (A-396,
/// [`serve_taken`]).
const LIVE_HOLD_ENTRIES: &str = "fleet.live.hold.entries";
/// Counter: of the live holds asked (every `fleet.live.hold.*` site), those the shard declined, its measured idle turn no
/// shorter than its wake estimate (A-387's predicate; A-396): the holds kept are the sites' sum less these.
const LIVE_HOLD_DECLINED: &str = "fleet.live.hold.declined";

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
    let first = state
      .live
      .destroys
      .get(&volume)
      .and_then(|(_, waiting)| waiting.first())
      .map(|first| (first.origin, first.id, first.reply.clone()));
    if let (Some((first_origin, first_id, reply)), Some(route)) = (first, route) {
      let joined = Awaiting::held(
        (Some(route), first_origin, first_id, reply),
        slates_rt::futures::now_ns(),
      );
      if let Some((_, waiting)) = state.live.destroys.get_mut(&volume) {
        waiting.push(joined);
      }
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
  let first = state
    .live
    .awaiting
    .get(&key)
    .and_then(|waiting| waiting.first())
    .map(|first| (first.origin, first.id, first.reply.clone()));
  if let (Some((first_origin, first_id, reply)), Some(route)) = (first, route) {
    let joined = Awaiting::held(
      (Some(route), first_origin, first_id, reply),
      slates_rt::futures::now_ns(),
    );
    if let Some(waiting) = state.live.awaiting.get_mut(&key) {
      waiting.push(joined);
    }
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

/// The replies this shard holds for durability or a tombstone's placement, and the seats taken for asynchronous
/// outcomes that may wait so ([`take_seat`]), which [`awaiting_bound`] bounds together.
fn held_replies(state: &ShardState) -> usize {
  let live: usize = state.live.awaiting.values().map(Vec::len).sum();
  let destroys: usize = state
    .live
    .destroys
    .values()
    .map(|(_, waiting)| waiting.len())
    .sum();
  live
    .saturating_add(destroys)
    .saturating_add(state.live.seats.len())
}

/// Seats the asynchronous outcome of request `(origin, id)` on `volume` among the replies this shard may hold for
/// durability, before its task changes anything (A-200; a mapped file's write-back, a checkout): `Ok` with no seat when
/// the volume is not live-shipped, whose outcome never waits; `Ok` seated; or `Overloaded`, counted and nothing seated,
/// when the shard already holds its bound ([`awaiting_bound`]), so the task never starts, the request changed nothing
/// and its resend under the same id runs it once, as a recorded verb refused by [`refuse_over_bound`] does. The seat is
/// filled by the outcome's wait ([`defer`]) or given back when it completes without one ([`release_seat`]).
pub(crate) fn take_seat(
  state: &mut ShardState,
  volume: slates_db::catalog::VolumeId,
  request: (u64, slates_wire::request::RequestId),
) -> Result<(), slates_ipc::protocol::Refusal> {
  match seat(state, volume, request) {
    Seat::Unneeded | Seat::Taken => Ok(()),
    Seat::Full => {
      state.count(LIVE_OVER_BOUND, 1);
      Err(slates_ipc::protocol::Refusal::Overloaded { shard: state.shard })
    }
  }
}

/// Seats an asynchronous outcome that must run whatever the bound ([`take_seat`] otherwise): a mapped file's release,
/// which lets its copy go whatever its write-back answers (`slates_client::Client::release_map_write`), so it is never
/// refused for room. With no room it runs seatless, and its wait, if it waits, is held past the bound ([`defer`]).
pub(crate) fn seat_if_room(
  state: &mut ShardState,
  volume: slates_db::catalog::VolumeId,
  request: (u64, slates_wire::request::RequestId),
) {
  let _ = seat(state, volume, request);
}

/// What seating an asynchronous outcome found ([`seat`]).
enum Seat {
  /// The volume is not live-shipped: the outcome never waits for durability.
  Unneeded,
  /// Seated among the held replies.
  Taken,
  /// The shard holds its bound already: nothing seated.
  Full,
}

/// Seats request `(origin, id)`'s outcome on `volume` when it may wait and there is room ([`take_seat`]).
fn seat(
  state: &mut ShardState,
  volume: slates_db::catalog::VolumeId,
  (origin, id): (u64, slates_wire::request::RequestId),
) -> Seat {
  if !is_live(state, volume) {
    return Seat::Unneeded;
  }
  if held_replies(state) >= awaiting_bound(state) {
    return Seat::Full;
  }
  state.live.seats.insert((origin, id.client, id.sequence));
  state.count(LIVE_SEATED, 1);
  Seat::Taken
}

/// Gives back the seat of request `(origin, id)`'s asynchronous outcome ([`take_seat`]): it completed without waiting
/// for durability (refused, or its volume durable already or no longer live-shipped), or its task could not start.
/// Nothing when it holds none.
pub(crate) fn release_seat(
  state: &mut ShardState,
  (origin, id): (u64, slates_wire::request::RequestId),
) {
  state.live.seats.remove(&(origin, id.client, id.sequence));
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
/// committed; its completion is recorded and its reply delivered then ([`resolve_destroys`]). The shard's bound of held
/// replies was judged before the destroy ran ([`refuse_over_bound`]), so it is never refused here, after its effects.
pub(crate) fn defer_destroy(
  state: &mut ShardState,
  (volume, sequence): (slates_db::catalog::VolumeId, u64),
  (origin, id): (u64, slates_wire::request::RequestId),
  route: Option<crate::merge_service::ReplyRoute>,
  reply: slates_ipc::protocol::ReplyBody,
) {
  let held = Awaiting::held((route, origin, id, reply), slates_rt::futures::now_ns());
  let (target, waiting) = state
    .live
    .destroys
    .entry(volume.bytes)
    .or_insert_with(|| (sequence, Vec::new()));
  *target = (*target).max(sequence);
  waiting.push(held);
  state
    .live
    .destroys_by_request
    .insert((origin, id.client, id.sequence), volume.bytes);
  state.count(DESTROY_AWAITED, 1);
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

/// Answers each verb whose reply `volume`'s log has held past the failover SLO [`Refusal::DurabilityPending`] (A-354):
/// the total bound of the rule every transport judges a held reply by (`slates_bridge_core::durable_bound`), the
/// operator's bound on how long an object may go unserved before a takeover serves it, past which a write not yet held
/// is past what the fleet promises. Through a shorter silence the reply still waits, as the 2026-11-05 sweep chose for
/// the slates protocol (a writer waits, never a false success; a transport answers its own failure earlier). The
/// refusal is recorded as the verb's completion, as a fence's is ([`refuse_awaiting`]), so a retry under the same id
/// meets it (§4.9): the verb's effects stay committed on the owner, nothing claims them durable, and a consumer runs the
/// verb again under a new id. Judged at the period's visit to the volume ([`period_volume`]). Until 2026-11-18 a held
/// reply ended only durable, fenced or destroyed, so a log that could never be held (its holders' catalog refused for
/// good, A-353's case) kept its writer waiting without end.
///
/// [`Refusal::DurabilityPending`]: slates_ipc::protocol::Refusal::DurabilityPending
fn judge_held(state: &mut ShardState, volume: [u8; 16]) {
  let id = slates_db::catalog::VolumeId { bytes: volume };
  let now = slates_rt::futures::now_ns();
  // The rule's own total ([`durable_spans`]): the failover delay, so a held verb is bounded as the transports' held
  // replies are and follows the failover estimate wherever it is published, never a number of its own.
  let bound_ns = durable_spans(state).total_ns;
  let keys: Vec<([u8; 16], u64)> = state
    .live
    .awaiting
    .range((volume, 0)..=(volume, u64::MAX))
    .map(|(key, _)| *key)
    .collect();
  for key in keys {
    let Some(waiting) = state.live.awaiting.remove(&key) else {
      continue;
    };
    let (past, kept) = held_past(waiting, now, bound_ns);
    if !kept.is_empty() {
      state.live.awaiting.insert(key, kept);
    }
    for (entry, held_ns) in past {
      slates_machine::log_line!(
        "slates-server: a held reply was refused after {} ms, the failover SLO (shard {}): {}",
        held_ns / NS_PER_MS,
        state.shard,
        durability_report(state, id, key.1)
      );
      state.count(LIVE_HELD_GAVE_UP, 1);
      answer(state, entry, durability_pending(held_ns));
    }
  }
}

/// Answers each destroy this shard has held past the failover SLO for its tombstone's placement
/// [`Refusal::DurabilityPending`] (A-354), as [`judge_held`] answers a write: the same bound, the same refusal, recorded
/// as the destroy's completion. Judged once a record period on each shard ([`period`]); bounded by the shard's held
/// replies ([`awaiting_bound`]).
///
/// [`Refusal::DurabilityPending`]: slates_ipc::protocol::Refusal::DurabilityPending
fn judge_held_destroys(state: &mut ShardState) {
  let now = slates_rt::futures::now_ns();
  // The rule's own total, as [`judge_held`] reads it.
  let bound_ns = durable_spans(state).total_ns;
  let volumes: Vec<[u8; 16]> = state.live.destroys.keys().copied().collect();
  for volume in volumes {
    let Some((sequence, waiting)) = state.live.destroys.remove(&volume) else {
      continue;
    };
    let (past, kept) = held_past(waiting, now, bound_ns);
    if !kept.is_empty() {
      state.live.destroys.insert(volume, (sequence, kept));
    }
    for (entry, held_ns) in past {
      slates_machine::log_line!(
        "slates-server: a held destroy was refused after {} ms, the failover SLO (shard {}): volume {}'s tombstone at \
         sequence {} not placed at f + 1",
        held_ns / NS_PER_MS,
        state.shard,
        crate::verbs::hex_of(&volume),
        sequence
      );
      state.count(DESTROY_HELD_GAVE_UP, 1);
      answer(state, entry, durability_pending(held_ns));
    }
  }
}

/// `waiting` split at `now_ns` into the entries of the requests held at least `bound_ns`, each with how long its request
/// was held, and the rest, each in its order. A request's entries (a retry that joined its wait, [`join`]) are judged by
/// its first hold, so all of them are answered at once with one reply, the one outcome its id records (§4.9).
fn held_past(
  waiting: Vec<Awaiting>,
  now_ns: u64,
  bound_ns: u64,
) -> (Vec<(Awaiting, u64)>, Vec<Awaiting>) {
  let mut first: BTreeMap<(u64, u32, u32), u64> = BTreeMap::new();
  for entry in &waiting {
    first
      .entry((entry.origin, entry.id.client, entry.id.sequence))
      .and_modify(|since| *since = (*since).min(entry.held_since_ns))
      .or_insert(entry.held_since_ns);
  }
  let mut past = Vec::new();
  let mut kept = Vec::new();
  for entry in waiting {
    let held_ns = first
      .get(&(entry.origin, entry.id.client, entry.id.sequence))
      .map_or(0, |since| now_ns.saturating_sub(*since));
    if held_ns >= bound_ns {
      past.push((entry, held_ns));
    } else {
      kept.push(entry);
    }
  }
  (past, kept)
}

/// The refusal a reply held `held_ns` past the failover SLO is answered with ([`judge_held`]).
fn durability_pending(held_ns: u64) -> slates_ipc::protocol::ReplyBody {
  slates_ipc::protocol::ReplyBody::Refused {
    refusal: slates_ipc::protocol::Refusal::DurabilityPending { held_ns },
  }
}

/// Counter: a reply held for a live volume's durability, refused at the failover SLO ([`judge_held`], A-354).
/// Format: a counter name in the daemon's status report.
const LIVE_HELD_GAVE_UP: &str = "fleet.live.held_gave_up";

/// Counter: a destroy held for its tombstone's placement, refused at the failover SLO ([`judge_held_destroys`], A-354).
/// Format: a counter name in the daemon's status report.
const DESTROY_HELD_GAVE_UP: &str = "fleet.destroy.held_gave_up";

/// The refusal a superseded owner gives a write to the volume it lost.
fn stale_epoch(current: HostEpoch) -> slates_ipc::protocol::ReplyBody {
  slates_ipc::protocol::ReplyBody::Refused {
    refusal: slates_ipc::protocol::Refusal::StaleEpoch { current: current.0 },
  }
}

/// Whether this node's ownership of volume `id` was superseded (A-338): its log fenced at a later epoch by a holder that
/// took a successor's takeover, so the volume is the successor's now and this node's verbs on it go there
/// (`verbs::owned_by_another_host`).
pub(crate) fn superseded(state: &ShardState, id: slates_db::catalog::VolumeId) -> bool {
  fenced(state, id.bytes).is_some()
}

/// The epoch `volume`'s owner here was fenced at, if it was.
fn fenced(state: &ShardState, volume: [u8; 16]) -> Option<HostEpoch> {
  match state.live.shipping.get(&volume) {
    Some(live) => live.fenced,
    // Not made again since a restart: fenced as its kept piece says (A-344).
    None => state
      .live
      .resumed
      .get(&volume)
      .and_then(|resumed| resumed.fenced)
      .map(HostEpoch),
  }
}

/// Whether this owner lost `volume` to a successor: its log fenced at a later epoch ([`fenced`]), or yielded when its
/// node was admitted again under its id (`ShardState::yielded`, A-334). Such a log ships nothing and does no period
/// work (A-349, A-409).
fn lost(state: &ShardState, volume: [u8; 16]) -> bool {
  fenced(state, volume).is_some()
    || state
      .yielded
      .contains(&slates_db::catalog::VolumeId { bytes: volume })
}

/// This node yielded `id` at its re-admission (A-334, `readmission::yield_catalogued`): its log ships nothing more
/// ([`lost`]), and every verb waiting on its durability is answered `DurabilityPending` now, held as long as it was, as
/// the failover SLO's bound answers one (A-354): the write it waits for was made on a copy its successor's writes have
/// passed, so it never becomes durable, and the same request run again goes to the successor. Its transports' parked
/// replies are woken to meet the yield at their gates. Before, its writers waited out the bound, and its log went on
/// shipping to holders that refused it as not the volume's acting owner, which never fenced it: an install of the whole
/// volume to the creator 1,714 times in one run (A-409).
pub(crate) fn yielded(state: &mut ShardState, id: slates_db::catalog::VolumeId) {
  let now = slates_rt::futures::now_ns();
  let destroys = state
    .live
    .destroys
    .remove(&id.bytes)
    .map(|(_, waiting)| waiting)
    .unwrap_or_default();
  for entry in take_awaiting(state, id.bytes).into_iter().chain(destroys) {
    let held_ns = now.saturating_sub(entry.held_since_ns);
    state.count(LIVE_YIELD_REFUSED, 1);
    answer(state, entry, durability_pending(held_ns));
  }
  wake_parked(state, id.bytes);
}

/// Counter: a verb waiting on a live volume's durability, answered `DurabilityPending` when its node yielded the volume
/// at its re-admission ([`yielded`], A-409).
/// Format: a counter name in the daemon's status report.
const LIVE_YIELD_REFUSED: &str = "fleet.live.yield_refused";

/// A later epoch holds `volume` (a holder answered with its fence): this owner was superseded on it. It ships no more of
/// it and tells every write waiting on it so; a fence learned now is kept before anyone is told, as a holder keeps a
/// fence it raised, so a restart does not forget it (A-344; a refused publication leaves the mark for the next); and
/// its mounts of the volume learn of it at once (A-346): a FUSE mount ends as a revoked one does
/// (`crate::fuse::superseded`), and an NFS mount answers `NFS3ERR_STALE` at its next request (`crate::nfs`'s gate reads
/// [`superseded`]). Nothing for a volume this shard ships no log of.
fn learn_superseded(state: &mut ShardState, volume: [u8; 16], current: HostEpoch) {
  let Some(live) = state.live.shipping.get_mut(&volume) else {
    return;
  };
  let learned = live.fenced.is_none_or(|known| known < current);
  live.fenced = Some(current);
  if learned {
    state.live.owned_dirty.insert(volume);
    if crate::verbs::publish_shard(state).is_err() {
      state.count(LIVE_FENCE_UNPUBLISHED, 1);
    }
    #[cfg(target_os = "linux")]
    crate::fuse::superseded(state, slates_db::catalog::VolumeId { bytes: volume });
  }
  refuse_awaiting(state, volume, current);
  wake_parked(state, volume);
}

/// Test support (`Daemon::inject_superseded`, never reachable from the wire): this owner is superseded on `id` at
/// `epoch`, `learned` now, as a holder's answer with a later fence finds it ([`learn_superseded`], its mounts told), or
/// else as a restart over a kept fence finds it (A-344: the fence kept, no mount told, each mount's own turn finds it).
/// A volume this shard ships no log of is marked by a kept fence either way. So a mount's answer to a takeover is driven
/// without a fleet, each path alone.
pub(crate) fn inject_superseded(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  epoch: u64,
  learned: bool,
) {
  if learned && state.live.shipping.contains_key(&id.bytes) {
    learn_superseded(state, id.bytes, HostEpoch(epoch));
    return;
  }
  match state.live.shipping.get_mut(&id.bytes) {
    Some(live) => live.fenced = Some(HostEpoch(epoch)),
    None => {
      state.live.resumed.insert(
        id.bytes,
        OwnedLive {
          epoch,
          next: 0,
          fenced: Some(epoch),
        },
      );
    }
  }
  #[cfg(target_os = "linux")]
  if learned {
    crate::fuse::superseded(state, id);
  }
}

/// Counter: a superseded owner's fence that its learning could not publish at once (A-344); the next publication keeps
/// it.
/// Format: a counter name in the daemon's status report.
const LIVE_FENCE_UNPUBLISHED: &str = "fleet.live.fence_unpublished";

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
  /// When it was made on the owner shard, on the host's monotonic clock: where its round's hop to the control shard
  /// begins ([`LaneStamps`]).
  made_ns: u64,
}

/// Whether the volume with `id` is live-shipped, by its policy on this partition.
fn is_live(state: &ShardState, id: slates_db::catalog::VolumeId) -> bool {
  state
    .db
    .partition()
    .volume(id)
    .is_some_and(|volume| volume.policy.live_shipped)
}

/// Records, on the owner shard a live volume was just taken over on, the head its takeover adopted (A-329): the volume's
/// shipper, made at its first publication ([`published`]), starts from it.
pub(crate) fn adopt_head(state: &mut ShardState, id: slates_db::catalog::VolumeId, head: Head) {
  state.live.adopted.insert(id.bytes, head);
  // A takeover's phase one met every position durable before it, this node's own earlier log's included (A-344).
  state.live.resumed.remove(&id.bytes);
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
    let shipped = state.live.shipping.remove(&id.bytes).is_some();
    let resumed = state.live.resumed.remove(&id.bytes).is_some();
    if shipped || resumed {
      // Its kept log goes with it (A-344).
      state.live.owned_dirty.insert(id.bytes);
    }
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
  let quorum = configuration.quorum;
  let reaches_size = kept_reaches_size(state, id);
  if !state.live.shipping.contains_key(&id.bytes) {
    // The volume's epoch is its head's: a successor ships under the epoch its takeover placed the head at, which is
    // above the dead owner's, whatever its own host's epoch (heads use the same rule, `fleet::owed_record`), and keeps it
    // across its own restart (A-344). A successor's log goes on from the head its takeover adopted (A-329): its first
    // entry is numbered past every position made durable under the dead owner's epoch (Continuity, T-8.6). Until
    // 2026-11-18 every shipper started from nothing, so a successor's first entry was position 0 at genesis (`live_chaos`:
    // 5 flows, positions 1 against 72 to 1,000). And an owner restarted over its own log resumes past every position it
    // may have shipped (A-344): begun again at position 0 under the same epoch, its new chain and its old were two values
    // at the same positions, and a takeover adopted the old one's greater head over the new one's durable writes.
    let (epoch, start) = planned(state, id);
    // Nothing is known of the volume's whole state before its first whole record, which is this log's first entry
    // (`owes_whole`): admitted with nothing in flight, it sets the bounds (`Bounds::of_whole`, below).
    let made = match start {
      Start::New => Ok(Shipper::new(
        (local, object, epoch),
        quorum,
        &holders,
        None,
        Bounds::default(),
      )),
      Start::Adopted(head) => Ok(Shipper::new(
        (local, object, epoch),
        quorum,
        &holders,
        Some(head),
        Bounds::default(),
      )),
      Start::Resumed(past) => Shipper::resumed(
        (local, object, epoch),
        quorum,
        &holders,
        Some(past),
        Bounds::default(),
      ),
    };
    let Ok(shipper) = made else {
      refuse(state, id, "fleet.live.refused.no_position");
      return;
    };
    state.live.adopted.remove(&id.bytes);
    state.live.resumed.remove(&id.bytes);
    state
      .live
      .shipping
      .insert(id.bytes, new_live_volume(shipper));
  }
  follow_candidates(state, id.bytes);
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return;
  };
  // Where this publication's hops begin: the entry appended for it takes the stamp ([`append_entry`]).
  live.publishing_ns = Some(slates_machine::clock::monotonic_ns());
  let whole = live.owes_whole || record.is_none() || reaches_size;
  // A whole entry being sealed in turns holds the chain: this publication is captured now, as it stands, and queued
  // behind it (A-377).
  if live.building.is_some() {
    queue_behind_build(state, id, whole, record);
    return;
  }
  let built = if whole {
    whole_record(state, id)
  } else {
    record.map(<[u8]>::to_vec)
  };
  let Some(record) = built else {
    refuse(state, id, "fleet.live.refused.no_record");
    return;
  };
  let Some(body) = plain_body(state, id, record) else {
    refuse(state, id, "fleet.live.refused.no_payload");
    return;
  };
  // A whole entry of more than one piece is sealed in turns of this shard (A-377): sealed here in one turn, its AEAD and
  // keyed names a byte held the shard about a second a GiB (g251's trace: a taken-over volume's first publication,
  // 1.06 s in one turn).
  if whole && sealed_in_pieces(state, &body) {
    begin_whole_build(state, id, body);
    return;
  }
  let Some((payload, anchors)) = sealed_payload(state, id, whole, body) else {
    refuse(state, id, "fleet.live.refused.no_payload");
    return;
  };
  if append_payload(state, id, (whole, true), payload, anchors) {
    ship_appended(state, id);
  }
}

/// Appends `payload` (`whole`, and whether its install image may be refreshed from the volume's state now) to `id`'s
/// log ([`append_entry`]); refused for credit, the next publication owes a whole state. Whether it was appended. The
/// caller ships it ([`ship_appended`]).
fn append_payload(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  (whole, image_now): (bool, bool),
  payload: Vec<u8>,
  anchors: Option<Vec<u64>>,
) -> bool {
  match append_entry(state, id, (whole, image_now), payload, anchors) {
    Ok(()) => true,
    Err(NotAppended::Gone) => false,
    Err(NotAppended::Unsealed) => {
      refuse(state, id, "fleet.live.refused.no_payload");
      false
    }
    Err(NotAppended::Refused(_)) => {
      refuse(state, id, "fleet.live.refused.no_credit");
      false
    }
  }
}

/// Why an entry was not appended ([`append_entry`], [`append_ready`]).
enum NotAppended {
  /// The volume ships no log here any more.
  Gone,
  /// Its body could not be sealed under the volume's keys.
  Unsealed,
  /// The shipper refused the entry: for credit, or past the last position.
  Refused(LiveError),
}

/// Appends `payload` to `id`'s log and keeps what follows from it: the bounds and the last whole state at a whole entry,
/// the bytes a candidate keeps, the counters. A refusal changes nothing, so a queued publication refused for credit can
/// wait in its place ([`append_ready`]). A whole entry is the only entry that pays an owed whole state: a delta of a queue
/// may follow a refusal it was captured before (A-377).
fn append_entry(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  (whole, image_now): (bool, bool),
  payload: Vec<u8>,
  anchors: Option<Vec<u64>>,
) -> Result<(), NotAppended> {
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return Err(NotAppended::Gone);
  };
  // A whole record is a checkpoint and may go one past the credit (`Shipper::append_checkpoint`); a delta waits for it.
  let appended = if whole {
    live.shipper.append_checkpoint(payload.clone())
  } else {
    live.shipper.append(payload.clone())
  };
  let entry = appended.map_err(NotAppended::Refused)?;
  let published_ns = live
    .publishing_ns
    .take()
    .unwrap_or_else(slates_machine::clock::monotonic_ns);
  live.newest = Some((entry.head().position, published_ns));
  let shipped = u64::try_from(payload.len()).unwrap_or(u64::MAX);
  if whole {
    live.owes_whole = false;
  }
  live.kept = if whole {
    payload.len()
  } else {
    live.kept.saturating_add(payload.len())
  };
  // An image is made from the volume's state only where that state is this entry's head: never in a queue's drain,
  // where later publications are captured past it (A-377); the record period makes it once the volume is clean.
  let image_owed = live.owes_image && !whole && image_now;
  if whole {
    let head = entry.head();
    live.shipper.mark_checkpoint(head);
    // Derived (AC-8.9, A-200 amended 2026-11-17): the credit and the retained tail are both this whole record's
    // size, the install a laggard would be sent instead: the deltas outstanding never cost more than the whole
    // record a turned-away one leads to, and shipping a laggard the entries it lacks never costs more than
    // installing it (`Bounds::of_whole`). The fixed credit of two entries had made a third publication inside one
    // round trip ship the whole volume.
    live.shipper.set_bounds(Bounds::of_whole(payload.len()));
    live.whole = Some((head, payload));
    live.owes_image = false;
    live.whole_anchors = anchors;
    live.whole_entry = Some(head);
    live.image_anchors = None;
  }
  state.count(LIVE_APPENDED, 1);
  if whole {
    state.count(LIVE_WHOLE, 1);
    state.count(LIVE_WHOLE_BYTES, shipped);
  } else {
    state.count(LIVE_CHANGE_BYTES, shipped);
  }
  if image_owed {
    // The volume's state now is this publication's, so its image is exactly the head just appended.
    refresh_image(state, id);
  }
  Ok(())
}

/// After entries were appended to `id`'s log: answers the writers whose wait they met and sends each holder what it
/// lacks.
fn ship_appended(state: &mut ShardState, id: slates_db::catalog::VolumeId) {
  resolve(state, id.bytes);
  let sends = pending_sends(state, id.bytes);
  dispatch(state, sends);
}

/// A whole entry sealed in turns of its owner shard (A-377; §4.10 "Live shipping", CLAUDE.md "Bounded work everywhere").
/// Its plain body is captured in the publication's own turn (the image and every extent copied, a memcpy a byte), and its
/// pieces are sealed a quantum of the shard's clock a turn by a task of the shard ([`whole_build_task`]), one piece at
/// least, yielding between turns; the publications made meanwhile are captured in their own turns and queued behind it,
/// and once it is sealed it is appended and they follow in order. Positions are still taken at the append, in
/// publication order, so the barriers made meanwhile wait for the positions their entries will take
/// ([`durable_target`]) and a restart's kept position counts them ([`owned_piece`]). Until 2026-11-18 a whole entry was
/// sealed in the one turn that published it, its AEAD and keyed names a byte: about a second a GiB, the watchdog's window.
struct WholeBuild {
  /// The publication's plain body (`LivePublication` bytes), captured in its turn; emptied once its entry is appended.
  body: Vec<u8>,
  /// The offset in `body` of the next piece to seal.
  next: usize,
  /// The pieces sealed so far, in order.
  sealed: Vec<SealedPiece>,
  /// The volume's lineage key wrapped for its neighbours, as the publication's turn found it, and the anchors it covers.
  sealing: Option<crate::head::HeadSealing>,
  /// See [`WholeBuild::sealing`].
  anchors: Vec<u64>,
  /// Whether its whole entry is on the log: what is left is the queue behind it, waiting for credit ([`append_ready`]).
  appended: bool,
  /// The publications made while it seals, each captured in its own turn, in order.
  queued: VecDeque<Queued>,
  /// The bytes of the deltas in `queued`: held to the credit, as their entries would be.
  queued_bytes: usize,
}

impl std::fmt::Debug for WholeBuild {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("WholeBuild")
      .field("body_bytes", &self.body.len())
      .field("next", &self.next)
      .field("sealed", &self.sealed.len())
      .field("appended", &self.appended)
      .field("queued", &self.queued.len())
      .field("queued_bytes", &self.queued_bytes)
      .finish()
  }
}

/// A publication captured while a whole entry seals ([`WholeBuild::queued`]): whether it ships whole, and its plain body.
struct Queued {
  whole: bool,
  body: Vec<u8>,
}

/// Where one turn of a [`WholeBuild`] left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Advance {
  /// Pieces remain: another turn follows.
  More,
  /// Every piece is sealed and it is appended, or waits for credit, or it ended (its volume gone, superseded, or its keys
  /// refused): no turn follows.
  Done,
}

/// Whether a whole entry of plain `body` is sealed in turns: this node seals, and the body spans more than one piece.
fn sealed_in_pieces(state: &ShardState, body: &[u8]) -> bool {
  state.seal_root.is_some()
    && usize::try_from(slates_archive::format::MAX_CHUNK_BYTES).is_ok_and(|cap| body.len() > cap)
}

/// Whether a whole entry of `volume`'s log is being sealed in turns, or the publications queued behind one wait.
fn building(state: &ShardState, volume: [u8; 16]) -> bool {
  state
    .live
    .shipping
    .get(&volume)
    .is_some_and(|live| live.building.is_some())
}

/// The entries `live`'s log will take before the next publication's: a whole entry being sealed and each publication
/// queued behind it.
fn pending_entries(live: &LiveVolume) -> u64 {
  live.building.as_ref().map_or(0, |build| {
    u64::try_from(build.queued.len())
      .unwrap_or(u64::MAX)
      .saturating_add(u64::from(!build.appended))
  })
}

/// Begins sealing the whole entry of plain `body` in turns ([`WholeBuild`]); its first turn runs now, and a task of this
/// shard runs the rest.
fn begin_whole_build(state: &mut ShardState, id: slates_db::catalog::VolumeId, body: Vec<u8>) {
  begin_whole_build_with(state, id, body, (VecDeque::new(), 0));
}

/// [`begin_whole_build`] with the publications already queued behind it and their delta bytes (a queue's drain met a
/// whole state of its own, [`append_ready`]).
fn begin_whole_build_with(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  body: Vec<u8>,
  (queued, queued_bytes): (VecDeque<Queued>, usize),
) {
  let Some((sealing, anchors)) = whole_sealing(state, id, true) else {
    drop_queue(state, id, queued.len(), "fleet.live.refused.no_payload");
    return;
  };
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return;
  };
  // The build is the whole state the volume owed: a barrier made now waits for its position, not one after it.
  live.owes_whole = false;
  live.building = Some(Box::new(WholeBuild {
    body,
    next: 0,
    sealed: Vec::new(),
    sealing,
    anchors,
    appended: false,
    queued,
    queued_bytes,
  }));
  state.count(LIVE_WHOLE_BUILDS, 1);
  if advance_whole_build(state, id) == Advance::Done {
    return;
  }
  let spawned =
    slates_rt::futures::spawn(whole_build_task(id)).and_then(slates_rt::futures::detach);
  if spawned.is_err() {
    drop_build(state, id, "fleet.live.refused.no_build_task");
  }
}

/// The task that seals a [`WholeBuild`] a turn at a time: it yields, then runs one turn, until every piece is sealed or
/// the build has ended. Detached: nothing joins it, and its slot is reaped when it ends; a destroyed volume ends it at its
/// next turn. A sealed build the credit does not admit yet is appended later by what frees the credit ([`fold_into`]) or
/// the record period, not by this task.
async fn whole_build_task(id: slates_db::catalog::VolumeId) {
  loop {
    slates_rt::futures::yield_now().await;
    match state::with_state(|s| advance_whole_build(s, id)) {
      Some(Advance::More) => {}
      Some(Advance::Done) | None => return,
    }
  }
}

/// One turn of `id`'s [`WholeBuild`]: its next pieces sealed until the seal turns' share of the shard's quantum has
/// passed (`config::wrap_slice_ns`, as a seal's wrap and a takeover's open are held, A-323, A-347), one piece at least,
/// so every turn moves it on; once every piece is sealed, what the credit admits is appended ([`append_build`]). A
/// build whose owner was superseded, or whose keys are refused, ends here.
fn advance_whole_build(state: &mut ShardState, id: slates_db::catalog::VolumeId) -> Advance {
  let superseded = match state.live.shipping.get(&id.bytes) {
    Some(live) if live.building.is_some() => live.fenced.is_some(),
    _ => return Advance::Done,
  };
  if superseded {
    // Superseded: every writer waiting on the volume was already told so (`learn_superseded`).
    drop_build(state, id, LIVE_BUILD_SUPERSEDED);
    return Advance::Done;
  }
  let Some((lineage, namer, segment)) = body_keys(state, id) else {
    drop_build(state, id, "fleet.live.refused.no_payload");
    return Advance::Done;
  };
  let quantum = crate::config::wrap_slice_ns(crate::fleet::seal_quantum_ns(state));
  let cap = usize::try_from(slates_archive::format::MAX_CHUNK_BYTES)
    .unwrap_or(usize::MAX)
    .max(1);
  state.count(LIVE_WHOLE_BUILD_TURNS, 1);
  let began = slates_rt::futures::now_ns();
  let Some(build) = state
    .live
    .shipping
    .get_mut(&id.bytes)
    .and_then(|live| live.building.as_mut())
  else {
    return Advance::Done;
  };
  let mut refused = false;
  while let Some(piece) = build
    .body
    .get(build.next..build.next.saturating_add(cap).min(build.body.len()))
    .filter(|piece| !piece.is_empty())
  {
    let chunk = slates_archive::Archive::raw_chunk(piece.to_vec());
    let Ok(sealed) = slates_cluster::sealed::seal_chunk(&lineage, &namer, &chunk, segment) else {
      refused = true;
      break;
    };
    build.next = build.next.saturating_add(piece.len());
    build.sealed.push(SealedPiece {
      name: sealed.name.to_vec(),
      bytes: sealed.bytes,
    });
    if slates_rt::futures::now_ns().saturating_sub(began) >= quantum {
      break;
    }
  }
  let sealed_whole = build.next >= build.body.len();
  if refused {
    drop_build(state, id, "fleet.live.refused.no_payload");
    return Advance::Done;
  }
  if !sealed_whole {
    return Advance::More;
  }
  append_build(state, id);
  Advance::Done
}

/// Appends what `id`'s [`WholeBuild`] holds ready ([`append_ready`]) and ships it.
fn append_build(state: &mut ShardState, id: slates_db::catalog::VolumeId) {
  if append_ready(state, id) {
    ship_appended(state, id);
  }
}

/// Appends what `id`'s [`WholeBuild`] holds ready, in order: its whole entry once every piece is sealed and the credit
/// admits a checkpoint (`Shipper::admits_checkpoint`), then each publication queued behind it as the credit admits it, a
/// delta sealed now (it is small) and a whole state begun as a build of its own with the rest queued behind it. What the
/// credit does not admit yet waits in its place, in order, for the acknowledgement that frees it ([`fold_into`]) or the
/// record period: every publication given a position in the queue gets its entry there, so the writers waiting on that
/// position are answered. Until 2026-11-18 a refusal dropped the rest of the queue into one whole state owed later, which
/// took one position for all of them, and the writers waiting on the others were refused `DurabilityPending` at the
/// failover SLO (`a_live_whole_entry_is_sealed_in_turns_and_writes_made_meanwhile_follow_it`: 4 of 8 writes, CI diag
/// 44352143242). The build ends once its queue is empty. Whether anything was appended.
fn append_ready(state: &mut ShardState, id: slates_db::catalog::VolumeId) -> bool {
  let mut appended_any = false;
  loop {
    let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
      return appended_any;
    };
    let admits = live.shipper.admits_checkpoint();
    let Some(build) = live.building.as_mut() else {
      return appended_any;
    };
    if !build.appended {
      if build.next < build.body.len() {
        return appended_any;
      }
      if !admits {
        state.count(LIVE_BUILD_WAITED, 1);
        return appended_any;
      }
      let mut payload = vec![KIND_WHOLE, BODY_SEALED];
      payload.extend_from_slice(
        &SealedBody {
          sealing: build.sealing.take(),
          pieces: std::mem::take(&mut build.sealed),
        }
        .to_bytes(),
      );
      let anchors = std::mem::take(&mut build.anchors);
      build.body = Vec::new();
      build.appended = true;
      match append_entry(state, id, (true, false), payload, Some(anchors)) {
        Ok(()) => appended_any = true,
        Err(_) => {
          drop_build(state, id, "fleet.live.refused.no_position");
          return appended_any;
        }
      }
      continue;
    }
    let Some(next) = build.queued.pop_front() else {
      live.building = None;
      return appended_any;
    };
    if !next.whole {
      build.queued_bytes = build.queued_bytes.saturating_sub(next.body.len());
    }
    if next.whole && sealed_in_pieces(state, &next.body) {
      let rest = state
        .live
        .shipping
        .get_mut(&id.bytes)
        .and_then(|live| live.building.take())
        .map_or((VecDeque::new(), 0), |build| {
          (build.queued, build.queued_bytes)
        });
      begin_whole_build_with(state, id, next.body, rest);
      return appended_any;
    }
    let sealed = sealed_payload(state, id, next.whole, next.body.clone());
    let outcome = match sealed {
      Some((payload, anchors)) => append_entry(state, id, (next.whole, false), payload, anchors),
      None => Err(NotAppended::Unsealed),
    };
    match outcome {
      Ok(()) => appended_any = true,
      Err(refused) => {
        // Back in its place: refused for credit, it waits there and the writers behind it keep their positions; refused
        // otherwise (its body not sealed, the last position), it is dropped with the rest, counted.
        let waits = matches!(refused, NotAppended::Refused(LiveError::NoCredit { .. }));
        if let Some(build) = state
          .live
          .shipping
          .get_mut(&id.bytes)
          .and_then(|live| live.building.as_mut())
        {
          if !next.whole {
            build.queued_bytes = build.queued_bytes.saturating_add(next.body.len());
          }
          build.queued.push_front(next);
        }
        if waits {
          state.count(LIVE_QUEUE_WAITED, 1);
        } else {
          drop_build(state, id, "fleet.live.refused.no_payload");
        }
        return appended_any;
      }
    }
  }
}

/// Ends `id`'s [`WholeBuild`] unappended (`why` counted): it and the publications queued behind it follow in a whole
/// state the volume now owes ([`drop_queue`]).
fn drop_build(state: &mut ShardState, id: slates_db::catalog::VolumeId, why: &'static str) {
  let unappended = state
    .live
    .shipping
    .get_mut(&id.bytes)
    .and_then(|live| live.building.take())
    .map_or(0, |build| build.queued.len());
  drop_queue(state, id, unappended, why);
}

/// Counts `dropped` queued publications that will follow in the whole state `id` now owes (`why` counted), and moves
/// every writer of `id` waiting past that whole state's position onto it: the positions the dropped entries would have
/// taken are not taken now, and the one whole state carries all their changes. A transport's barrier keeps the
/// position it read and waits for the log to pass it, or for its bound (`DurableBound`).
fn drop_queue(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  dropped: usize,
  why: &'static str,
) {
  state.count(
    LIVE_QUEUED_DROPPED,
    u64::try_from(dropped).unwrap_or(u64::MAX),
  );
  refuse(state, id, why);
  let owed = state
    .live
    .shipping
    .get(&id.bytes)
    .and_then(|live| live.shipper.next_position());
  if let Some(owed) = owed {
    fold_waiting_onto(state, id.bytes, owed);
  }
}

/// Moves every writer of `volume` waiting for a position past `position` to wait for `position` instead.
fn fold_waiting_onto(state: &mut ShardState, volume: [u8; 16], position: u64) {
  let Some(first) = position.checked_add(1) else {
    return;
  };
  let later: Vec<([u8; 16], u64)> = state
    .live
    .awaiting
    .range((volume, first)..=(volume, u64::MAX))
    .map(|(key, _)| *key)
    .collect();
  for key in later {
    let Some(waiting) = state.live.awaiting.remove(&key) else {
      continue;
    };
    for entry in &waiting {
      state.live.awaiting_by_request.insert(
        (entry.origin, entry.id.client, entry.id.sequence),
        (volume, position),
      );
    }
    state
      .live
      .awaiting
      .entry((volume, position))
      .or_default()
      .extend(waiting);
  }
}

/// Captures a publication of `id` made while a whole entry seals, or while the queue behind one waits, and queues it
/// last (A-377): its record, the volume's whole image when it ships whole, and its content runs read now, as they stand.
/// A delta is queued while the deltas queued stay within the credit the whole entry ahead of them sets (its own size,
/// `Bounds::of_whole`, or the shipper's credit if that is greater), as their entries will be held to it; past it,
/// refused for credit, it follows in a whole state the volume owes.
fn queue_behind_build(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  whole: bool,
  record: Option<&[u8]>,
) {
  let built = if whole {
    whole_record(state, id)
  } else {
    record.map(<[u8]>::to_vec)
  };
  let Some(record) = built else {
    refuse(state, id, "fleet.live.refused.no_record");
    return;
  };
  let Some(body) = plain_body(state, id, record) else {
    refuse(state, id, "fleet.live.refused.no_payload");
    return;
  };
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return;
  };
  let credit = live.shipper.bounds().credit;
  let Some(build) = live.building.as_mut() else {
    return;
  };
  let credit = credit.max(build.body.len());
  if !whole && build.queued_bytes > 0 && build.queued_bytes.saturating_add(body.len()) > credit {
    refuse(state, id, "fleet.live.refused.no_credit");
    return;
  }
  if !whole {
    build.queued_bytes = build.queued_bytes.saturating_add(body.len());
  }
  build.queued.push_back(Queued { whole, body });
  state.count(LIVE_QUEUED, 1);
}

/// Counter: a whole entry begun in turns ([`begin_whole_build`], A-377).
/// Format: a counter name in the daemon's status report.
const LIVE_WHOLE_BUILDS: &str = "fleet.live.whole_builds";
/// Counter: one turn of a whole entry's sealing ([`advance_whole_build`]).
/// Format: a counter name in the daemon's status report.
const LIVE_WHOLE_BUILD_TURNS: &str = "fleet.live.whole_build_turns";
/// Counter: a publication captured while a whole entry sealed and queued behind it ([`queue_behind_build`]).
/// Format: a counter name in the daemon's status report.
const LIVE_QUEUED: &str = "fleet.live.queued";
/// Counter: a queued publication that followed in a later whole state instead of its own entry ([`drop_queue`]).
/// Format: a counter name in the daemon's status report.
const LIVE_QUEUED_DROPPED: &str = "fleet.live.queued_dropped";
/// Counter: a whole entry being sealed when its owner was superseded ([`advance_whole_build`]).
/// Format: a counter name in the daemon's status report.
const LIVE_BUILD_SUPERSEDED: &str = "fleet.live.build_superseded";
/// Counter: a sealed whole entry that waited for the credit to admit it ([`append_ready`]).
/// Format: a counter name in the daemon's status report.
const LIVE_BUILD_WAITED: &str = "fleet.live.build_waited";
/// Counter: a queued publication that waited in its place for the credit ([`append_ready`]).
/// Format: a counter name in the daemon's status report.
const LIVE_QUEUE_WAITED: &str = "fleet.live.queue_waited";

/// A live volume's state on its owner shard as its shipper is made: its first publication ships the whole volume.
fn new_live_volume(shipper: Shipper) -> LiveVolume {
  LiveVolume {
    shipper,
    owes_whole: true,
    owes_image: false,
    whole: None,
    whole_anchors: None,
    fenced: None,
    kept: 0,
    lanes: BTreeMap::new(),
    unreachable: Vec::new(),
    catalogued: Vec::new(),
    recorded: None,
    recording: None,
    whole_entry: None,
    image_anchors: None,
    heard: 0,
    unanswered_since_ns: None,
    building: None,
    publishing_ns: None,
    newest: None,
    resolved_ns: None,
    answers: crate::live_hold::AnswerWindow::default(),
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
/// follows the image (`Shipper::set_bounds`), so a laggard is sent the entries it lacks while they cost less than
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
  live.shipper.set_bounds(Bounds::of_whole(image.len()));
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
  let body = plain_body(state, id, record)?;
  sealed_payload(state, id, whole, body)
}

/// The plain body of a publication of `record` (A-377 split it from [`payload`]): the record and its content runs, read
/// from the volume now, so it is captured in the publication's own turn whenever it is sealed.
fn plain_body(
  state: &ShardState,
  id: slates_db::catalog::VolumeId,
  record: Vec<u8>,
) -> Option<Vec<u8>> {
  let runs = content_runs(state, id, &record)?;
  Some(LivePublication { record, runs }.to_bytes())
}

/// The entry payload of a captured `body`: its kind and body byte, then the body, sealed under the volume's lineage key
/// where this node seals; and, for a sealed whole entry, the anchors its copies of the lineage key are wrapped for
/// (`None` for a clear body). The whole body is sealed in this call: a whole entry spanning more than one piece is sealed
/// in turns instead ([`begin_whole_build`]).
fn sealed_payload(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  whole: bool,
  body: Vec<u8>,
) -> Option<(Vec<u8>, Option<Vec<u64>>)> {
  let kind = if whole { KIND_WHOLE } else { KIND_DELTA };
  if state.seal_root.is_none() {
    let mut payload = vec![kind, BODY_CLEAR];
    payload.extend_from_slice(&body);
    return Some((payload, None));
  }
  let (lineage, namer, segment) = body_keys(state, id)?;
  let pieces = seal_body(&lineage, &namer, body, segment)?;
  let (sealing, anchors) = whole_sealing(state, id, whole)?;
  let mut payload = vec![kind, BODY_SEALED];
  payload.extend_from_slice(&SealedBody { sealing, pieces }.to_bytes());
  Some((payload, Some(anchors)))
}

/// The keys a sealed body of `id`'s log is sealed under: its lineage key and namer, and the segment its pieces are sized
/// by.
fn body_keys(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
) -> Option<(hyper_seal::keys::WrappingKey, hyper_seal::name::Namer, u32)> {
  let account = crate::seal_keys::tenant_of(&state.db.partition().volume(id)?.owner);
  let lineage = crate::seal_keys::lineage(state, id, account).ok()?;
  let namer = crate::seal_keys::namer(state, id, account).ok()?;
  let segment = u32::try_from(state.config.page).ok()?;
  Some((lineage, namer, segment))
}

/// What a sealed entry of `id`'s log carries for a successor: on a whole entry the volume's lineage key wrapped for each
/// neighbour (the head's sealing) and the anchors it covers; nothing on a delta.
fn whole_sealing(
  state: &ShardState,
  id: slates_db::catalog::VolumeId,
  whole: bool,
) -> Option<(Option<crate::head::HeadSealing>, Vec<u64>)> {
  let sealing = if whole {
    let owner = state.db.partition().volume(id)?.owner.clone();
    crate::seal_keys::head_sealing(state, id, &owner)
  } else {
    None
  };
  let anchors = sealing.as_ref().map_or_else(Vec::new, |sealing| {
    sealing.keys.iter().map(|key| key.anchor).collect()
  });
  Some((sealing, anchors))
}

/// The anchor of `candidate` if this shard's pair to that anchor was accepted by `candidate` itself, the incarnation now
/// at the anchor: only then can it unwrap a key wrapped for the anchor. A node replaced at its own anchor comes back
/// under a new member id with none of its predecessor's keys. Counted by anchor alone, the predecessor's acceptance
/// covered the replacement, writes went durable on a key it never received, and as successor it could not open the
/// log (A-271; the store-destroyed takeover, 2026-11-09).
fn covered_anchor(
  candidate: HostId,
  anchors: &BTreeMap<HostId, u64>,
  delivered: &BTreeMap<u64, HostId>,
) -> Option<u64> {
  let anchor = *anchors.get(&candidate)?;
  (delivered.get(&anchor) == Some(&candidate)).then_some(anchor)
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
  // Each candidate's anchor only once this shard's pair to it was accepted by that very candidate ([`covered_anchor`]).
  let wanted: Vec<Option<u64>> = remote_candidates(state, volume)
    .map(|candidate| covered_anchor(candidate, &state.member_anchors, &state.pairs_delivered))
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
    covered_anchor(candidate, &state.member_anchors, &state.pairs_delivered).is_some()
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
  // A superseded owner ships nothing more of the volume it lost (A-349): every holder refuses its log `StaleEpoch`, and
  // each send held that holder's record session for an exchange that could never land. Until 2026-11-18 the record
  // period sent the entries a holder lacked again every period: an owner admitted again after its takeover had its
  // sessions out on that loan at the start of every period, where its own takeover round of another departure met
  // them, never asked the other survivor and gave up two objects within tolerance (the chaos nemesis, seed
  // 6876545201569229162: `fleet.live.install_built` about 2,240 after its fence, `fleet.takeover.pages_unsent` 213 to
  // 928). Nor one its node yielded at its re-admission (A-409): its node knows it lost the volume before any holder
  // tells it, and such an owner, never fenced, sent the creator an install of its whole volume 1,714 times in one run.
  if lost(state, volume) {
    return Vec::new();
  }
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
            made_ns: slates_machine::clock::monotonic_ns(),
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
/// its way ([`reached_volume`]): one sent now would land the holder behind the tail. `None` and stale when there is no
/// image, or the holder already holds what it carries.
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
    made_ns: slates_machine::clock::monotonic_ns(),
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
      // The holder is asked now: a held reply may hold it to an answer from a window hence (A-301).
      note_asked(live, now);
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
/// for a fresh image ([`reached_volume`]). Each shard's live volumes are walked in turns of its step quantum
/// ([`reached_slice`]) by a task of this shard, one a shard, as the period walks them (A-324): until 2026-11-18 one turn
/// of each shard visited every live volume it owns and built every message the holder lacked, an install a copy of the
/// whole volume, however many volumes that was.
pub(crate) fn holder_reached(holder: HostId) {
  let Some((origin, shards)) = state::with_state(|s| (s.shard, s.shards.clone())) else {
    return;
  };
  for shard in shards {
    let walk = slates_rt::futures::spawn(reached_walk(origin, shard, holder))
      .and_then(slates_rt::futures::detach);
    if walk.is_err() {
      let _ = state::with_state_counted(|s| s.count(LIVE_REACHED_UNWALKED, 1));
    }
  }
}

/// Walks `shard`'s live volumes for `holder`'s return a turn at a time ([`reached_slice`]), from a task of the control
/// shard (`origin`), each turn awaited within the live exchange's longest budget and a turn of the shard of its own,
/// the control shard's too ([`crate::xshard::Turns`]). Bounded: a turn always moves the walk
/// on, the volumes are the shard's, and a turn the shard does not answer within its budget ends the walk (the record
/// period sends the holder what it lacks then). Detached by [`holder_reached`]: nothing joins it, and its slot is reaped
/// when it ends.
async fn reached_walk(origin: u16, shard: u16, holder: HostId) {
  let deadline = crate::fleet::live_budget().max_deadline_ns();
  let mut turns = crate::xshard::Turns::new(origin, shard);
  let mut after = None;
  loop {
    let next = turns
      .call(move |s| reached_slice(s, holder, after), deadline)
      .await;
    match next {
      Some(Some(last)) => after = Some(last),
      _ => break,
    }
  }
}

/// One turn of [`reached_walk`]: the shard's live volumes after `after` in key order, each visited for `holder`
/// ([`reached_volume`]) and what it lacks of those that had given it up sent, until the shard's step quantum is spent
/// (the period's own, [`period_slice`]); the last volume visited when any remain after it, `None` once none does. The
/// first volume is always visited, so a turn always moves the walk on.
fn reached_slice(s: &mut ShardState, holder: HostId, after: Option<[u8; 16]>) -> Option<[u8; 16]> {
  s.count(LIVE_REACHED_SLICES, 1);
  let quantum = s
    .live
    .period_quantum_ns
    .or_else(slates_rt::futures::step_budget_ns)
    .unwrap_or(s.config.runtime.step_budget_ns);
  let began = slates_rt::futures::now_ns();
  let mut cursor = after;
  let mut sends = Vec::new();
  let remaining = loop {
    let next = match cursor {
      Some(last) => s
        .live
        .shipping
        .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
        .next()
        .map(|(volume, _)| *volume),
      None => s.live.shipping.keys().next().copied(),
    };
    let Some(volume) = next else {
      break None;
    };
    if reached_volume(s, volume, holder) {
      sends.extend(
        pending_sends(s, volume)
          .into_iter()
          .filter(|send| send.holder == holder),
      );
    }
    cursor = Some(volume);
    if slates_rt::futures::now_ns().saturating_sub(began) >= quantum {
      break cursor;
    }
  };
  dispatch(s, sends);
  remaining
}

/// `holder`'s return to one live volume: no longer unreachable there. Whether the volume had given it up and is not
/// fenced, so it is sent what it lacks now. A holder owed an install is sent the last whole state while the retained
/// tail reaches past it (A-428): installed there, it takes the entries after it from the tail. Only a whole state the tail
/// no longer reaches past would leave the holder owing a second install, so only then does it wait for an image made at
/// the log's head: the next publication makes it, or the record period for an idle writer ([`refresh_image`]). The
/// image a holder fell behind on was a median of 1,332 and 1,395 entries old when last built, while an install's own
/// transit added 0 or 1, so it landed past the retained tail and took a second whole install (`live_chaos`'s rolling
/// partition, diag-live-durability-54). Until A-428 an image was owed for any whole state behind the log's head, and
/// after an owner's warm restart nothing shipped again: its first whole entry, sealed before its pairs were delivered
/// again, was behind the write appended after it; the image waited behind the whole record owed to cover the
/// candidates' keys ([`refresh_image`] makes none while one is owed); and that record waited for credit only the
/// holders' installs could free (A-344's warm-restart test, refused `DurabilityPending` at its 20 s bound).
fn reached_volume(s: &mut ShardState, volume: [u8; 16], holder: HostId) -> bool {
  let Some(live) = s.live.shipping.get_mut(&volume) else {
    return false;
  };
  let behind_the_tail = live.whole.as_ref().is_some_and(|(head, _)| {
    Some(*head) != live.shipper.head() && !live.shipper.retains_after(head.position)
  });
  if behind_the_tail && live.shipper.pending(holder) == Next::Install {
    live.owes_image = true;
  }
  let was = live.unreachable.contains(&holder);
  live
    .unreachable
    .retain(|unreachable| *unreachable != holder);
  was && live.fenced.is_none()
}

/// Counter: one owner-shard turn of a returned holder's walk over the shard's live volumes ([`reached_slice`], A-324).
const LIVE_REACHED_SLICES: &str = "fleet.live.reached_slices";
/// Counter: a returned holder's walk over one shard that could not be spawned ([`holder_reached`]); the record period
/// sends the holder what it lacks instead.
const LIVE_REACHED_UNWALKED: &str = "fleet.live.reached_unwalked";

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
      // Where the round's hop to the control shard ends ([`LaneStamps`]); and this dispatch is a message of the control
      // shard's stream of them (A-396), whose window is held after the lanes end, so the next dispatch of a closed loop
      // finds the shard polling.
      let began_ns = slates_machine::clock::monotonic_ns();
      let window = state::with_state(|s| s.live.dispatches.arrived(began_ns)).unwrap_or(0);
      drain_stragglers();
      let mut lanes: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>> = Vec::new();
      for (holder, sends) in by_holder(sends) {
        lanes.push(Box::pin(ship_to_holder(
          origin,
          (holder, began_ns),
          sends,
          budget,
        )));
      }
      every(lanes).await;
      let left =
        crate::live_hold::remaining(began_ns, window, slates_machine::clock::monotonic_ns());
      let held = crate::live_hold::hold(left);
      let _ = state::with_state_counted(|s| count_hold(s, LIVE_HOLD_DISPATCHES, held));
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
  (holder, began_ns): (HostId, u64),
  sends: Vec<Send>,
  budget: slates_cluster::CommitBudget,
) {
  let mut objects: Vec<ObjectId> = Vec::new();
  for send in &sends {
    if !objects.contains(&send.object) {
      objects.push(send.object);
    }
  }
  let answered = ship_lane(origin, (holder, began_ns), sends, budget).await;
  // After the lane's own folds, so the owner sends what the holder lacks given its last answer.
  let control = state::with_state(|s| s.shard).unwrap_or(origin);
  crate::xshard::run_on_counted(control, origin, move |s| {
    lane_ended(s, holder, objects, answered);
  });
}

/// Asks this (the control) shard to hold its idle window through an exchange with `holder` until its answer is expected
/// (A-396): the window its record link learned from that holder's answers; nothing before the first is timed. The shard
/// keeps the hold only where it pays (A-387, [`crate::live_hold::hold`]).
fn hold_for_exchange(state: &mut ShardState, holder: HostId) {
  let window = state
    .record_sessions
    .get(&holder)
    .and_then(|link| link.live_answers.window())
    .unwrap_or(0);
  count_hold(state, LIVE_HOLD_EXCHANGE, crate::live_hold::hold(window));
}

/// The messages of one lane in turn ([`ship_to_holder`]): whether the holder answered every one it was sent. The entries
/// of one volume that follow each other go as one batch while it fits the session's congestion window
/// ([`take_batch`]): sent one exchange an entry, a burst of `n` entries reached the holder in `n` round trips, and a
/// holder back from a stall caught up on the retained tail a round trip an entry.
async fn ship_lane(
  origin: u16,
  (holder, began_ns): (HostId, u64),
  sends: Vec<Send>,
  budget: slates_cluster::CommitBudget,
) -> bool {
  let mut queue: VecDeque<Send> = sends.into();
  // When this lane was free to take its next message: the dispatch's arrival, then each exchange's end.
  let mut ready_ns = began_ns;
  while let Some(first) = queue.pop_front() {
    let sessions = borrow_session(holder, budget.max_deadline_ns()).await;
    let send = take_batch(first, &mut queue, &sessions);
    // Each message under its own budget: the liveness budget plus its transfer on the session it rides, so an install of
    // the whole volume is not given up the moment an entry would be (`exchange_budget`).
    let exchange = exchange_budget(send.bytes.len(), &sessions);
    // The control shard polls through the exchange until its holder's answer is expected (A-396): parked, it took a
    // wake to read the answer.
    let _ = state::with_state(|s| hold_for_exchange(s, holder));
    let sent_ns = slates_machine::clock::monotonic_ns();
    let exchanged =
      slates_cluster::live::exchange((send.stream, &send.bytes), sessions, exchange, |_, _| true)
        .await;
    let answered_ns = slates_machine::clock::monotonic_ns();
    let stamps = LaneStamps {
      made_ns: send.made_ns,
      began_ns: ready_ns.max(send.made_ns),
      sent_ns,
      answered_ns,
    };
    ready_ns = answered_ns;
    let (replies, stragglers) = settle_sessions(exchanged);
    let answered = !replies.is_empty();
    if answered {
      // How long this holder took to answer: the window the next exchange with it is held for (A-396).
      let _ = state::with_state(|s| {
        if let Some(link) = s.record_sessions.get_mut(&holder) {
          link
            .live_answers
            .answered(answered_ns.saturating_sub(sent_ns));
        }
      });
    }
    if !answered {
      let _ = state::with_state(|s| s.count(LIVE_UNANSWERED, 1));
    }
    let replies: Vec<(ObjectId, HostId, Vec<u8>)> = replies
      .into_iter()
      .map(|(host, bytes)| (send.object, host, bytes))
      .collect();
    if !replies.is_empty() {
      let control = state::with_state(|s| s.shard).unwrap_or(origin);
      crate::xshard::run_on_counted(control, origin, move |s| {
        fold_into(s, replies, Some(stamps));
      });
    }
    // After the answers are on their way to the owner, so durability waits for no straggler.
    finish_stragglers(origin, send.object, stragglers).await;
    if !answered {
      return false;
    }
  }
  true
}

/// Counter: batches of entries sent to a holder in one exchange ([`take_batch`]).
const LIVE_BATCHES: &str = "fleet.live.batches";
/// Counter: entries sent in those batches.
const LIVE_BATCHED: &str = "fleet.live.batched";

/// `first` with the entries of its volume that follow it in `queue`, taken from it as one batch
/// (`slates_db::live::encode_batch`, [`slates_cluster::live::LIVE_ENTRIES_STREAM`]) while the batch's bytes stay within
/// the session's congestion window; `first` alone when nothing follows it that fits, or it is not an entry. Derived: a
/// batch is at most one window (the bytes the session may have in flight a round trip, RFC 9002 §7), so it rides the
/// `Control` class without holding it past a round of the window, and the first entry always goes, whatever its size.
fn take_batch(
  first: Send,
  queue: &mut VecDeque<Send>,
  sessions: &[(HostId, slates_transport::endpoint::Endpoint)],
) -> Send {
  if first.stream != slates_cluster::live::LIVE_ENTRY_STREAM {
    return first;
  }
  let window = sessions.first().map_or(0, |(_, endpoint)| {
    usize::try_from(endpoint.congestion_window()).unwrap_or(usize::MAX)
  });
  let mut bytes = first.bytes.len();
  let following = queue
    .iter()
    .take_while(|next| {
      let fits = next.stream == slates_cluster::live::LIVE_ENTRY_STREAM
        && next.object == first.object
        && bytes.saturating_add(next.bytes.len()) <= window;
      if fits {
        bytes = bytes.saturating_add(next.bytes.len());
      }
      fits
    })
    .count();
  if following == 0 {
    return first;
  }
  let encoded: Vec<&[u8]> = std::iter::once(first.bytes.as_slice())
    .chain(
      queue
        .iter()
        .take(following)
        .map(|next| next.bytes.as_slice()),
    )
    .collect();
  let Some(batch) = slates_db::live::encode_batch(&encoded) else {
    return first;
  };
  queue.drain(..following.min(queue.len()));
  let carried = u64::try_from(following).map_or(u64::MAX, |following| following.saturating_add(1));
  let _ = state::with_state_counted(|s| {
    s.count(LIVE_BATCHES, 1);
    s.count(LIVE_BATCHED, carried);
  });
  Send {
    object: first.object,
    holder: first.holder,
    stream: slates_cluster::live::LIVE_ENTRIES_STREAM,
    bytes: batch,
    made_ns: first.made_ns,
  }
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
    crate::xshard::run_on_counted(control, origin, move |s| fold_into(s, replies, None));
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

/// One message's way through a lane, on the host's monotonic clock (one origin across shards and processes,
/// `slates_machine::clock`): made on the owner shard, its lane free to take it on the control shard, sent, and answered.
/// The fold that makes the newest entry durable reads its round from them, hop by hop ([`count_hops`]).
#[derive(Clone, Copy, Debug)]
struct LaneStamps {
  made_ns: u64,
  began_ns: u64,
  sent_ns: u64,
  answered_ns: u64,
}

/// Counter pairs (how many were timed, and their nanoseconds) of a live write's round, hop by hop (§4.10 "Live shipping";
/// the record's owed breakdown of A-200's 2026-11-05 measurement). Each pair is counted once a write, by the fold that
/// makes its entry durable, so over a run the five hops' nanoseconds add up to the round's exactly, and a mean is
/// nanoseconds over count. On the owner node.
/// The entry built on the owner shard: from its publication reaching the shipper to its message made.
const HOP_BUILD: (&str, &str) = ("fleet.live.hop.build", "fleet.live.hop.build_ns");
/// From the message made on the owner shard to its lane on the control shard free to take it: the cross-shard hop and
/// the wait behind a lane's earlier message.
const HOP_TO_CONTROL: (&str, &str) = ("fleet.live.hop.to_control", "fleet.live.hop.to_control_ns");
/// On the control shard, from the lane free to the message sent: the session's borrow, the batch and its budget.
const HOP_LANE: (&str, &str) = ("fleet.live.hop.lane", "fleet.live.hop.lane_ns");
/// The exchange, from the message sent to the holder's answer read on the control shard.
const HOP_EXCHANGE: (&str, &str) = ("fleet.live.hop.exchange", "fleet.live.hop.exchange_ns");
/// From the answer read on the control shard to its fold running on the owner shard: the cross-shard hop back.
const HOP_TO_OWNER: (&str, &str) = ("fleet.live.hop.to_owner", "fleet.live.hop.to_owner_ns");
/// The whole round, from the publication reaching the shipper to the fold that made its entry durable: the five hops
/// above, end to end.
const HOP_ROUND: (&str, &str) = ("fleet.live.hop.round", "fleet.live.hop.round_ns");
/// From that fold to the reply held for it seeing it durable ([`wait_durable`]: a stable NFS write's or `COMMIT`'s, a
/// Shards stream's `FSYNC`); a reply that saw it durable at once counts the time since the fold.
const HOP_REPLY: (&str, &str) = ("fleet.live.hop.reply", "fleet.live.hop.reply_ns");
/// On a holder: an entry or batch served, from its arrival to its answer, the publication that keeps it included.
const HOP_HOLDER: (&str, &str) = ("fleet.live.hop.holder", "fleet.live.hop.holder_ns");

/// Counts one timed hop of `ns` nanoseconds ([`HOP_BUILD`] and its siblings).
fn count_hop(state: &mut ShardState, (count, sum): (&'static str, &'static str), ns: u64) {
  state.count(count, 1);
  state.count(sum, ns);
}

/// The round of `volume`'s newest entry, hop by hop, once an answer made it durable: its publication's arrival at the
/// shipper and the lane's `stamps` partition it into the build, the hop to the control shard, the lane, the exchange and
/// the hop back, each a difference of consecutive stamps on one clock, so they add up to the round. A fold with no
/// stamps (a straggler's late answer) times no hop; the entry is let go either way, and its round, publication to
/// durability, is what the volume's answer window learns (A-396, [`hold_for_answer`]). The fold's own time is kept for the
/// reply's wake ([`HOP_REPLY`]).
fn count_hops(state: &mut ShardState, volume: [u8; 16], stamps: Option<LaneStamps>) {
  let now = slates_machine::clock::monotonic_ns();
  let Some(live) = state.live.shipping.get_mut(&volume) else {
    return;
  };
  let Some((position, published_ns)) = live.newest else {
    return;
  };
  if live
    .shipper
    .durable()
    .is_none_or(|durable| durable < position)
  {
    return;
  }
  live.newest = None;
  live.resolved_ns = Some(now);
  // The newest entry's round, publication to durability, is what a reply held for the next one holds the owner's window
  // by (A-396, [`hold_for_answer`]).
  live.answers.answered(now.saturating_sub(published_ns));
  let Some(stamps) = stamps else {
    return;
  };
  let hops = [
    (HOP_BUILD, stamps.made_ns.saturating_sub(published_ns)),
    (
      HOP_TO_CONTROL,
      stamps.began_ns.saturating_sub(stamps.made_ns),
    ),
    (HOP_LANE, stamps.sent_ns.saturating_sub(stamps.began_ns)),
    (
      HOP_EXCHANGE,
      stamps.answered_ns.saturating_sub(stamps.sent_ns),
    ),
    (HOP_TO_OWNER, now.saturating_sub(stamps.answered_ns)),
    (HOP_ROUND, now.saturating_sub(published_ns)),
  ];
  for (hop, ns) in hops {
    count_hop(state, hop, ns);
  }
}

/// Folds replies into the owner shard's shippers and ships what the holders still lack. `stamps` are the lane's, when the
/// replies came from one exchange of a lane ([`ship_lane`]); a straggler's come with none.
fn fold_into(
  state: &mut ShardState,
  replies: Vec<(ObjectId, HostId, Vec<u8>)>,
  stamps: Option<LaneStamps>,
) {
  let mut touched = Vec::new();
  for (object, host, bytes) in replies {
    let Some(live) = state.live.shipping.get_mut(&object.0) else {
      continue;
    };
    let before = live.shipper.durable();
    let held_before = live.shipper.held_by(host);
    let reply = decode_reply(&bytes);
    if let Next::Fenced(current) = live.shipper.on_reply(host, reply) {
      learn_superseded(state, object.0, current);
      continue;
    }
    // Only an answer that moved the holder forward on the chain is followed at once; anything else (a refusal, a holder
    // that does not hold the volume's catalog yet, one off the chain) is left to the period. Following every answer
    // looped at the network's speed: 982 and 5,501 refusals on the two holders of one five-write run, and later a holder
    // installed at a checkpoint the tail had passed, re-installed there without end (2026-11-05, `live_fleet`).
    let progressed = live.shipper.held_by(host) != held_before;
    if progressed {
      note_heard(live);
    }
    if live.shipper.durable() > before {
      count_hops(state, object.0, stamps);
      state.count(LIVE_DURABLE, 1);
      resolve(state, object.0);
      // What became durable left the credit: a sealed whole entry or a queued publication waiting for it goes on now.
      append_build(state, slates_db::catalog::VolumeId { bytes: object.0 });
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
    .is_some_and(|live| live.owes_whole && live.shipper.admits_checkpoint())
}

/// Counter: one owner-shard turn of the record period's live work ([`period_slice`]).
const LIVE_PERIOD_SLICES: &str = "fleet.live.period_slices";
/// Counter: a record period's live work on one shard ([`period`]), however many turns it took.
const LIVE_PERIODS: &str = "fleet.live.periods";

/// The record period's turn on `shard` (run from the control shard): re-ships what each holder of each live volume
/// there still lacks, so a lost message or a holder that was away is caught up without waiting for the next write.
/// The volumes are visited in slices, each one turn of the owner shard of about a step quantum ([`period_slice`]) and a
/// turn of its own, the control shard's too ([`crate::xshard::Turns`]), so a shard owning many live volumes is never
/// held for all of them in one turn (CLAUDE.md "Bounded work everywhere"):
/// until 2026-11-17 one turn visited every one, a cost that grew with the volumes the shard owned while every client
/// of the shard waited behind it.
pub(crate) async fn period(origin: u16, shard: u16) {
  drain_stragglers();
  let deadline = crate::fleet::live_budget().max_deadline_ns();
  let mut turns = crate::xshard::Turns::new(origin, shard);
  let mut after = None;
  let mut first = true;
  loop {
    let counted = std::mem::take(&mut first);
    let next = turns
      .call(
        move |s| {
          if counted {
            s.count(LIVE_PERIODS, 1);
            judge_held_destroys(s);
          }
          period_slice(s, after)
        },
        deadline,
      )
      .await;
    match next {
      Some(Some(last)) => after = Some(last),
      _ => break,
    }
  }
}

/// One slice of [`period`]: the shard's live volumes after `after` in key order, each visited ([`period_volume`]) and
/// what its holders lack sent, until the shard's step quantum is spent (`futures::step_budget_ns`, what a destroy's or
/// an archive's slice is sized by); the last volume visited when any remain after it, `None` once none does. The first
/// volume is always visited, so a slice always moves the period on.
fn period_slice(s: &mut ShardState, after: Option<[u8; 16]>) -> Option<[u8; 16]> {
  s.count(LIVE_PERIOD_SLICES, 1);
  let quantum = s
    .live
    .period_quantum_ns
    .or_else(slates_rt::futures::step_budget_ns)
    .unwrap_or(s.config.runtime.step_budget_ns);
  let began = slates_rt::futures::now_ns();
  let mut cursor = after;
  let mut sends = Vec::new();
  let remaining = loop {
    let next = match cursor {
      Some(last) => s
        .live
        .shipping
        .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
        .next()
        .map(|(volume, _)| *volume),
      None => s.live.shipping.keys().next().copied(),
    };
    let Some(volume) = next else {
      break None;
    };
    period_volume(s, volume);
    sends.extend(pending_sends(s, volume));
    cursor = Some(volume);
    if slates_rt::futures::now_ns().saturating_sub(began) >= quantum {
      break cursor;
    }
  };
  dispatch(s, sends);
  remaining
}

/// The period's visit to one live volume: follows its candidates, retries holders whose last lane went unanswered,
/// answers writers whose wait a condition met, and owes or ships the whole record or image it is due.
fn period_volume(s: &mut ShardState, volume: [u8; 16]) {
  // A superseded owner's log does no more period work (A-349): no holder takes it ([`pending_sends`]), its writers were
  // refused when it learned its fence ([`learn_superseded`]), and following its candidates would record a holder set,
  // or make a whole record or an image, for a volume that is not its own any more.
  if fenced(s, volume).is_some() {
    return;
  }
  // Nor a log its node yielded at its re-admission (A-409): its writers were refused at the yield ([`yielded`]); one
  // made after it is judged against its bound all the same, and nothing else is done.
  if lost(s, volume) {
    judge_held(s, volume);
    return;
  }
  let id = slates_db::catalog::VolumeId { bytes: volume };
  follow_candidates(s, volume);
  // The period is what retries a holder whose last lane went unanswered ([`LiveVolume::unreachable`]).
  if let Some(live) = s.live.shipping.get_mut(&volume) {
    live.unreachable.clear();
  }
  // A writer may wait on a condition that changes with no acknowledgement to fold (the head placed once its
  // neighbourhood settles, a pair delivered): answered here at the latest, and refused here once it has waited the
  // failover SLO (A-354).
  resolve(s, volume);
  judge_held(s, volume);
  // A whole entry being sealed in turns, or the queue behind one waiting for credit, is the whole state the volume owes,
  // and its volume's state is past its head until it and its queue are appended (A-377): the period appends what the
  // credit admits now and makes nothing beside it. Judged before the candidates' keys, which would owe another whole
  // state for a log whose first whole entry is still sealing, and each publication meanwhile would be queued whole.
  if building(s, volume) {
    append_build(s, id);
    return;
  }
  // A log whose last whole entry no successor among the candidates could open is checkpointed again once this shard has
  // delivered the pairs that would let it ([`covers_candidates`]).
  if !covers_candidates(s, volume)
    && coverable_now(s, volume)
    && let Some(live) = s.live.shipping.get_mut(&volume)
  {
    live.owes_whole = true;
  }
  // A volume that owes a whole record ships one once its credit allows: a burst that outran the credit left its last
  // writes unshipped until another write came, and none might (three of five writes, `live_fleet`).
  if owes_whole_within_credit(s, volume) {
    published(s, id, None);
  } else if owes_image(s, volume) && published_whole(s, id) {
    // No publication is coming to make the image a holder behind the tail waits for (the writer is idle).
    refresh_image(s, id);
  }
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
  let epoch = slates_db::live::epoch_of(request);
  serve_taken(state, (local, peer), (request, epoch), |request, bound| {
    Entry::decode(request, bound).map(|entry| vec![entry])
  })
}

/// The one way a candidate takes entries from `peer`, alone ([`serve_entry`]) or as a batch ([`serve_entries`]), so a
/// batch is served exactly as its entries would be: authorized and bounded ([`admitted`]), decoded by `decode` within
/// the bound, taken in order ([`take_entries`]), counted, and answered only once this shard's recovery image keeps what
/// was taken ([`acknowledged`]). `epoch` is the request's, read from its header, for the refusal of an unauthorized one.
/// Until 2026-11-17 a batch was served beside the single entry's path, and main's acknowledgement rule (A-270) reached
/// the single entry alone when the two met at a merge.
fn serve_taken(
  state: &mut ShardState,
  (local, peer): (HostId, HostId),
  (request, epoch): (&[u8], Option<HostEpoch>),
  decode: impl FnOnce(&[u8], usize) -> Result<Vec<Entry>, LiveError>,
) -> Vec<u8> {
  let arrived_ns = slates_machine::clock::monotonic_ns();
  let object = slates_db::live::object_of(request);
  let bound = match admitted(state, local, peer, object) {
    Ok(bound) => bound,
    Err(why) => {
      count_refused(state, why);
      return encode_reply(&Err(not_served(state, object, epoch)));
    }
  };
  let (reply, taken) = match decode(request, bound) {
    Ok(entries) => take_entries(state, peer, &entries, bound),
    Err(refusal) => (Err(refusal), 0),
  };
  match &reply {
    Err(error) => {
      count_refused(state, refused_counter(error));
      encode_reply(&reply)
    }
    Ok(_) => {
      state.count(LIVE_HELD, taken);
      // The owner's entries are a stream (A-396): one that took something is a message of it, whose window this shard
      // holds once it has answered, so the owner's next entry of a closed loop finds it polling. Read with the copy
      // already reached for the entries taken, so the stream marks no piece of its own (A-270).
      let window = if taken > 0 {
        object
          .and_then(|object| state.live.held.get_mut(&object))
          .map_or(0, |held| held.arrivals.arrived(arrived_ns))
      } else {
        0
      };
      let answer = acknowledged(state, encode_reply(&reply));
      let now = slates_machine::clock::monotonic_ns();
      count_hop(state, HOP_HOLDER, now.saturating_sub(arrived_ns));
      let held = crate::live_hold::hold(crate::live_hold::remaining(arrived_ns, window, now));
      count_hold(state, LIVE_HOLD_ENTRIES, held);
      answer
    }
  }
}

/// One entry taken into a candidate's copy (guard-then-apply, `Replica::check` then `commit`): applied and kept within
/// `bound` when it extends the head, passed over when already held; the head held after it, or why not.
fn take_entry(
  held: &mut Held,
  room: &mut Room<'_>,
  entry: &Entry,
  bound: usize,
) -> Result<Option<Head>, LiveError> {
  match held.replica.check(entry)? {
    Admitted::Apply(head) => {
      let whole = entry.payload.first() == Some(&KIND_WHOLE);
      keep(held, room, &entry.payload, bound, whole)?;
      held.replica.commit(entry.epoch, head);
      Ok(Some(head))
    }
    Admitted::Held => Ok(held.replica.head()),
  }
}

/// A candidate's answer to a batch of entries from `peer` (A-200 amended 2026-11-17, [`ship_lane`]): authorized and
/// bounded as [`serve_entry`] does one entry, then each entry checked, kept and committed in order exactly as one entry
/// is, until one is neither applied nor already held (a gap, a fence, the bound): answered once, with the head held
/// after the last, or the first refusal when no entry was taken. Each entry taken counts `fleet.live.held`, as one sent
/// alone does.
pub(crate) fn serve_entries(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let epoch = slates_db::live::epoch_of_batch(request);
  serve_taken(
    state,
    (local, peer),
    (request, epoch),
    slates_db::live::decode_batch,
  )
}

/// A batch's entries taken in order ([`take_entry`]) until one is not: the head held after the last taken, or the first
/// refusal when none was; and how many were taken. A refusal that stops the batch after some were taken is counted
/// here, since the answer is then the head reached.
fn take_entries(
  state: &mut ShardState,
  peer: HostId,
  entries: &[Entry],
  bound: usize,
) -> (Result<Option<Head>, LiveError>, u64) {
  let mut reached = None;
  let mut taken: u64 = 0;
  for entry in entries {
    let claimed = state.live.claims.get(&entry.object).copied().unwrap_or(0);
    let outcome = {
      let ShardState { live, store, .. } = &mut *state;
      let mut room = Room { store, claimed };
      take_entry(
        held_entry_in(live, peer, entry.object),
        &mut room,
        entry,
        bound,
      )
    };
    match outcome {
      Ok(head) => {
        taken = taken.saturating_add(1);
        reached = Some(head);
      }
      Err(refusal) => {
        if reached.is_some() {
          count_refused(state, refused_counter(&refusal));
        }
        return (reached.ok_or(refusal), taken);
      }
    }
  }
  (reached.ok_or(LiveError::Malformed), taken)
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
    let claimed = state.live.claims.get(&install.object).copied().unwrap_or(0);
    let ShardState { live, store, .. } = &mut *state;
    let mut room = Room { store, claimed };
    let held = held_entry_in(live, peer, install.object);
    match held.replica.check_install(&install)? {
      Admitted::Apply(head) => {
        keep(held, &mut room, &install.state, bound, true)?;
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
    Err(error) => {
      count_refused(state, refused_counter(error));
      encode_reply(&reply)
    }
    Ok(_) => {
      state.count(LIVE_HELD, 1);
      acknowledged(state, encode_reply(&reply))
    }
  }
}

/// Why a holder will not serve a message about `object` made under `epoch`: below the highest fence it holds the
/// object at, its live copy's or its record copy's ([`known_fence`]), it is `StaleEpoch`, which tells a superseded
/// owner to stop (`Shipper::on_reply` answers `Fenced`) and refuse what waits on it; otherwise `Unauthorized`. Refused
/// as a stranger, a resumed old owner could not tell its loss from a passing refusal and held its write unanswered for
/// the fence flow's whole 40 s wait (2026-11-05, `live_fleet`). The record copy's fence counts since A-409: in one run
/// a re-admitted old owner's log, never fenced, sent the volume's creator an install 1,714 times, each refused
/// `not_the_acting_owner`; why no refusal fenced it is not read from that run's counters, and the fence a holder knows
/// from either copy is the most it can tell (the yield now stops such a log by itself, [`yielded`]).
fn not_served(state: &ShardState, object: Option<ObjectId>, epoch: Option<HostEpoch>) -> LiveError {
  refusal_for(object.and_then(|object| known_fence(state, object)), epoch)
}

/// The highest fence this holder holds `object` at: its live copy's (a successor's phase one raised it) and its record
/// copy's (the takeover's page, an accepted record or an install raised it), each in the one epoch order the owners
/// ship under (a successor ships at the epoch its takeover placed the head at, `live_ship::planned`).
fn known_fence(state: &ShardState, object: ObjectId) -> Option<HostEpoch> {
  let live = state
    .live
    .held
    .get(&object)
    .map(|held| held.replica.fence());
  let record = state
    .holder_records
    .get(&object)
    .map(slates_db::register::Acceptor::fence);
  live.max(record)
}

/// [`not_served`]'s decision: `StaleEpoch` at `fence` for a message under an `epoch` below it, `Unauthorized` otherwise
/// (no fence known, no epoch read, or an epoch at or past the fence).
fn refusal_for(fence: Option<HostEpoch>, epoch: Option<HostEpoch>) -> LiveError {
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
    state.holder_records.as_map(),
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
    LiveError::NoRoom { .. } => "fleet.live.hold_refused.no_room",
  }
}

/// The holder's copy of `object`, kept for `owner`: the configuration names `owner` the volume's acting owner (the caller
/// checked it, [`admitted`]), so a successor takes the copy over; the fence still refuses any stale epoch.
fn held_entry_in(live: &mut LiveState, owner: HostId, object: ObjectId) -> &mut Held {
  let first = live.slot_floor;
  let held = live.held.entry(object).or_insert_with(|| Held {
    replica: Replica::new(owner, object),
    entries: VecDeque::new(),
    bytes: 0,
    first,
    arrivals: crate::live_hold::StreamWindow::default(),
    charged: 0,
  });
  held.replica.reassign(owner);
  held
}

/// A candidate's answer to a successor's takeover fetch (A-200), laid out in turns of this shard (g251): authorized as a
/// read of the object (`fleet::content_authorized`: the acting owner, a candidate, or a recovery cohort), the fence raised
/// to the successor's epoch and published (a promise that outlives a restart before it is told), and then the head and
/// the sealed entries kept, appended to the answer a slice of [`fetch_slice_bytes`] a turn ([`FetchAnswer`]). A stale
/// successor is refused by the fence. Laid out in one turn, the answer cloned every entry and then copied them all into
/// one buffer grown as it went: 34,814 entries of a 406 MB log in a nemesis run (seed 18189587465523915115), with a
/// control shard's single 557 ms poll beside it. Run on its own task of the serve loop, so the turns hold no other
/// request of the peer.
pub(crate) async fn serve_fetch(local: HostId, peer: HostId, request: Vec<u8>) -> Vec<u8> {
  let begun = state::with_state(|s| begin_fetch_answer(s, local, peer, &request));
  let mut answer = match begun {
    Some(Ok(answer)) => answer,
    Some(Err(refused)) => return refused,
    // The control shard's state out of reach (its shutdown): nothing is answered.
    None => return Vec::new(),
  };
  loop {
    let laid = state::with_state(|s| {
      let slice_bytes = fetch_slice_bytes(s);
      let whole = answer.lay_out(s, slice_bytes);
      s.count(LIVE_FETCH_SLICES, 1);
      whole
    });
    match laid {
      Some(true) => return answer.into_reply(),
      Some(false) => slates_rt::futures::yield_now().await,
      None => return Vec::new(),
    }
  }
}

/// The first turn of a fetch's answer ([`serve_fetch`]): the request decoded and authorized, the copy fenced at the
/// successor's epoch and handed to it, and the fence published before anything is told. `Err` with the whole reply when
/// it is a refusal (an empty one when the publication was refused, which the successor reads as no answer).
fn begin_fetch_answer(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Result<FetchAnswer, Vec<u8>> {
  let Ok(fetch) = slates_db::live::Fetch::decode(request) else {
    count_refused(state, REFUSED_MALFORMED);
    return Err(slates_db::live::fetched_reply(
      &Err(LiveError::Malformed),
      0,
    ));
  };
  let regional = state.council.configuration();
  let reading = slates_cluster::content::ContentAccess::Read;
  if !crate::fleet::content_authorized(
    regional,
    state.holder_records.as_map(),
    local,
    peer,
    reading,
    fetch.object,
  ) {
    count_refused(state, "fleet.live.hold_refused.fetch_unauthorized");
    return Err(slates_db::live::fetched_reply(
      &Err(LiveError::Unauthorized),
      0,
    ));
  }
  let answer = match fenced_copy(state, peer, fetch.object, fetch.epoch) {
    Ok(held) => {
      held.replica.reassign(peer);
      FetchAnswer::of(held, fetch.object, fetch.epoch)
    }
    Err(refusal) => return Err(slates_db::live::fetched_reply(&Err(refusal), 0)),
  };
  state.count(LIVE_FETCHED, 1);
  // The fence this raised is a promise too: it outlives a restart before it is told.
  if crate::verbs::publish_shard(state).is_err() {
    state.count(LIVE_UNPUBLISHED, 1);
    return Err(Vec::new());
  }
  Ok(answer)
}

/// Counter: the turns a takeover fetch's answer took to lay out on this node, a candidate's for a successor or a
/// successor's of its own copy (g251, [`FetchAnswer`]).
const LIVE_FETCH_SLICES: &str = "fleet.live.fetch_slices";

/// The bytes of entries one turn of a fetch's answer lays out on this shard: the copy-bound slice of the turn's quantum
/// at the machine's measured memcpy rate (`DaemonConfig::copy_slice_bytes_within`), an entry at least, the quantum
/// being the one a takeover's other turns are cut by (`fleet::seal_quantum_ns`), so a test that cuts it lays out an
/// entry a turn.
fn fetch_slice_bytes(state: &ShardState) -> usize {
  let slice = state
    .config
    .copy_slice_bytes_within(crate::fleet::seal_quantum_ns(state));
  usize::try_from(slice).unwrap_or(usize::MAX)
}

/// A takeover fetch's answer while it is laid out (g251): the copy's head when it was fenced, its entries from slot
/// `next` to `end` still to append, and the answer so far (`slates_db::live::fetched_reply`, room for every entry
/// reserved). Under the fence nothing ships to the copy, so its entries hold still between the turns; a copy that moved
/// all the same (a later successor's fence above the epoch, another head, entries gone, the copy let go) is answered as
/// a fetch arriving then would be, and the successor that asked fetches again or stands down.
pub(crate) struct FetchAnswer {
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
  head: Option<Head>,
  next: u64,
  end: u64,
  reply: Vec<u8>,
}

impl FetchAnswer {
  /// The answer for `held`, the copy of `object` fenced at `epoch`: its head, its slots, room for its entries.
  fn of(held: &Held, object: ObjectId, epoch: slates_db::register::HostEpoch) -> FetchAnswer {
    let head = held.replica.head();
    let (next, end) = held.slots();
    let entries = held.entries.len();
    let room = held
      .bytes
      .saturating_add(entries.saturating_mul(slates_db::live::FETCHED_PREFIX_BYTES));
    FetchAnswer {
      object,
      epoch,
      head,
      next,
      end,
      reply: slates_db::live::fetched_reply(&Ok(head), room),
    }
  }

  /// One turn: the copy's next entries appended until `slice_bytes` of them are laid out, an entry at least; `true`
  /// once the answer is whole. A copy that moved since the answer began makes it that copy's answer now, whole.
  fn lay_out(&mut self, state: &ShardState, slice_bytes: usize) -> bool {
    let held = state.live.held.get(&self.object);
    let Some(held) = held.filter(|held| {
      held.replica.fence() <= self.epoch
        && held.replica.head() == self.head
        && held.slots().0 <= self.next
    }) else {
      self.reply = moved_answer(held, self.epoch);
      return true;
    };
    let mut laid = 0usize;
    while self.next < self.end {
      let Some(entry) = held.entry_at(self.next) else {
        self.reply = moved_answer(Some(held), self.epoch);
        return true;
      };
      slates_db::live::push_fetched(&mut self.reply, entry);
      self.next = self.next.saturating_add(1);
      laid = laid.saturating_add(entry.len());
      if laid >= slice_bytes {
        break;
      }
    }
    self.next >= self.end
  }

  /// The answer, laid out whole.
  fn into_reply(self) -> Vec<u8> {
    self.reply
  }
}

/// What a fetch would be told by a copy that moved while its answer was laid out: the fence above `epoch` refuses it;
/// otherwise the head held now, without entries, which the successor finds is not the head it adopted.
fn moved_answer(held: Option<&Held>, epoch: slates_db::register::HostEpoch) -> Vec<u8> {
  match held {
    Some(held) if held.replica.fence() > epoch => slates_db::live::fetched_reply(
      &Err(LiveError::StaleEpoch {
        current: held.replica.fence(),
      }),
      0,
    ),
    Some(held) => slates_db::live::fetched_reply(&Ok(held.replica.head()), 0),
    None => slates_db::live::fetched_reply(&Ok(None), 0),
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
    state.holder_records.as_map(),
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
      // The fence this raised is a promise: it outlives a restart before it is told (as a fetch's, `serve_fetch`).
      let reply = slates_db::live::encode_reported(&Ok(head), kept);
      acknowledged(state, reply)
    }
    Err(refusal) => slates_db::live::encode_reported(&Err(refusal), 0),
  }
}

/// Counter: a takeover's phase one that did not count this node's own report, because the publication that keeps its
/// raised fence was refused (A-344); asked again at the next attempt.
/// Format: a counter name in the daemon's status report.
const LIVE_OWN_FENCE_UNPUBLISHED: &str = "fleet.live.own_fence_unpublished";

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
  let first = state.live.slot_floor;
  let held = state.live.held.entry(object).or_insert_with(|| Held {
    replica: Replica::new(successor, object),
    entries: VecDeque::new(),
    bytes: 0,
    first,
    arrivals: crate::live_hold::StreamWindow::default(),
    charged: 0,
  });
  held.replica.prepare(epoch)?;
  Ok(held)
}

/// A candidate's answer on the whole-state kind in one turn: a report or an install, by its tag. A fetch never comes
/// here: the serve loop answers it in turns on a task of its own ([`serve_fetch`]).
pub(crate) fn serve_state(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  if slates_db::live::is_report(request) {
    serve_report(state, local, peer, request)
  } else {
    serve_install(state, local, peer, request)
  }
}

/// Keeps `payload` after what is held, or in place of all of it when `replace` (a whole entry, or an install's state),
/// within `bound` and charged against the volume's claim ([`Room`]); guard-then-apply, so a refusal leaves the copy as
/// it was. Until 2026-11-18 a whole payload past the bound was refused after the copy had already dropped its entries.
fn keep(
  held: &mut Held,
  room: &mut Room<'_>,
  payload: &[u8],
  bound: usize,
  replace: bool,
) -> Result<(), LiveError> {
  let before = if replace { 0 } else { held.bytes };
  let bytes = before
    .checked_add(payload.len())
    .ok_or(LiveError::Malformed)?;
  if bytes > bound {
    return Err(LiveError::Malformed);
  }
  room.settle(held, bytes)?;
  if replace {
    held.clear_entries();
  }
  held.entries.push_back(payload.to_vec());
  held.bytes = bytes;
  Ok(())
}

// ── A candidate's admission (§4.2, g229) ────────────────────────────────────────────────────────────────────────────

/// What a held log is charged against (§4.2, g229): the claim this node reserved for the volume on the shard its id
/// routes to ([`admit_claim`]), and this control shard's budget for the bytes past it ([`Held::charged`]). The log and
/// the volume a successor makes of it are never held at once beyond the takeover's own work, so the claim covers the
/// log until a takeover and the volume after it; a log past its claim (its whole state and the deltas since, up to
/// twice the volume's bound) is charged here as content held for another owner, from what this shard has unpromised.
struct Room<'a> {
  store: &'a mut slates_vfs::volume::Store,
  claimed: u64,
}

impl Room<'_> {
  /// Charges or credits back what `held`'s log costs past the claim once it holds `bytes`, before the log changes:
  /// refused `NoRoom` with what this shard could admit, nothing changed, when it cannot take the growth.
  fn settle(&mut self, held: &mut Held, bytes: usize) -> Result<(), LiveError> {
    let wanted = u64::try_from(bytes)
      .unwrap_or(u64::MAX)
      .saturating_sub(self.claimed);
    let more = wanted.saturating_sub(held.charged);
    if more > 0 {
      self.store.make_room(more);
      if self.store.budget.charge_replicated(more).is_err() {
        return Err(LiveError::NoRoom {
          available: self.store.admittable(),
        });
      }
    } else {
      self
        .store
        .budget
        .credit_replicated(held.charged.saturating_sub(wanted));
    }
    held.charged = wanted;
    Ok(())
  }
}

/// The partition `object`'s id routes to: the shard a successor would restore the volume on, which holds its claim.
fn partition_of(object: ObjectId) -> u16 {
  crate::verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 })
}

/// The newest catalog record this node holds for `object`'s volume, decoded: its size and whether it must live in
/// locked RAM, the admission its claim makes. `None` while the catalog is not held here.
fn held_catalog(state: &ShardState, object: ObjectId) -> Option<crate::catalog::CatalogValue> {
  let catalog = object.catalog();
  let (_, value) = state.holder_records.get(&catalog)?.highest(catalog)?;
  crate::catalog::CatalogValue::from_record_bytes(value)
}

/// What a candidate's admission of a live log's claim came to ([`admit_claim`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
  /// The claim is held, or the message is one its own serve refuses or takes as before: serve it.
  Serve,
  /// Refused for room: answer the owner `NoRoom` with what could be admitted, and take nothing.
  NoRoom(u64),
  /// Not decided now (another admission of the object runs, or its shard did not answer within the call's deadline):
  /// answer nothing, which the owner ships again at its period.
  NotNow,
}

impl Admission {
  /// The answer the owner gets instead of the message's serve, or `None` to serve it.
  pub(crate) fn refusal(self) -> Option<Vec<u8>> {
    match self {
      Admission::Serve => None,
      Admission::NoRoom(available) => Some(encode_reply(&Err(LiveError::NoRoom { available }))),
      Admission::NotNow => Some(Vec::new()),
    }
  }
}

/// Derived: an admission's call to the volume's shard waits one record period, the cadence the owner ships again at, so
/// the holder answers no later than the owner would ask again; an admission older than it was a dropped attempt's.
const ADMISSION_WAIT_NS: u64 = crate::daemon::HEARTBEAT_NS;

/// Counter: a live message answered nothing because its claim's admission was still running, or unanswered.
/// Format: a counter name in the daemon's status report.
const CLAIM_NOT_NOW: &str = "fleet.live.claim_not_now";
/// Counter: a live message answered `NoRoom`: the volume's shard could not admit its claim (§4.2).
/// Format: a counter name in the daemon's status report.
const CLAIM_NO_ROOM: &str = "fleet.live.claim_no_room";
/// Counter: a held log's charge that the claim, once learned, could not settle (it cannot grow, so this is a bug signal).
/// Format: a counter name in the daemon's status report.
const CLAIM_UNSETTLED: &str = "fleet.live.claim_unsettled";
/// Counter: a reconciliation of a shard's claims that its call did not answer within the deadline; kept for the next.
/// Format: a counter name in the daemon's status report.
const CLAIM_RECONCILE_UNANSWERED: &str = "fleet.live.claim_reconcile_unanswered";

/// §4.2 ("a remote holder makes the same admission against its own machine before acknowledging placement"; g229):
/// before this candidate takes a message that puts a live log's entries in a copy (an entry, a batch, an install) of a
/// volume it holds no claim for, it reserves the claim a successor's restore takes on the shard the volume's id routes
/// to (`verbs::admit_candidate_claim`): so it never acknowledges a log it could not serve if it succeeds the owner. The
/// message is served only then ([`Admission::Serve`]); refused for room, the owner is answered `NoRoom` and nothing is
/// taken. A message naming no object, from a sender this node would not serve, or of a volume whose catalog it does not
/// hold, is left to its own serve, which refuses it as before. Run on the control shard by the record plane.
pub(crate) async fn admit_claim(
  origin: u16,
  local: HostId,
  peer: HostId,
  object: Option<ObjectId>,
) -> Admission {
  let Some(object) = object else {
    return Admission::Serve;
  };
  let begun = state::with_state(|s| begin_admission(s, local, peer, object));
  let (target, size, require_locked) = match begun {
    Some(Ok(Some(begun))) => begun,
    Some(Ok(None)) => return Admission::Serve,
    Some(Err(admission)) => return admission,
    None => return Admission::NotNow,
  };
  let id = slates_db::catalog::VolumeId { bytes: object.0 };
  let answer = crate::xshard::call_within(
    origin,
    target,
    move |s| crate::verbs::admit_candidate_claim(s, id, size, require_locked),
    ADMISSION_WAIT_NS,
  )
  .await;
  state::with_state(|s| finish_admission(s, object, answer)).unwrap_or(Admission::NotNow)
}

/// Whether `object`'s live message needs its claim admitted first ([`admit_claim`]): `Ok(None)` when it does not (a claim
/// held, or a message its serve refuses), the admission to make (the volume's shard, size and locked guarantee), or
/// `Err(NotNow)` while another admission of it runs. Marks the admission begun.
fn begin_admission(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  object: ObjectId,
) -> Result<Option<(u16, slates_ipc::protocol::SizeClass, bool)>, Admission> {
  if state.live.claims.contains_key(&object) || admitted(state, local, peer, Some(object)).is_err()
  {
    return Ok(None);
  }
  let Some(catalog) = held_catalog(state, object) else {
    return Ok(None);
  };
  let Some(&target) = state.shards.get(usize::from(partition_of(object))) else {
    return Ok(None);
  };
  let now = slates_rt::futures::now_ns();
  if state
    .live
    .admitting
    .get(&object)
    .is_some_and(|began| now.saturating_sub(*began) < ADMISSION_WAIT_NS)
  {
    state.count(CLAIM_NOT_NOW, 1);
    return Err(Admission::NotNow);
  }
  state.live.admitting.insert(object, now);
  Ok(Some((
    target,
    crate::verbs::taken_over_size(&catalog),
    catalog.require_locked,
  )))
}

/// What an admission's answer from the volume's shard comes to ([`admit_claim`]): the claim learned and the copy's charge
/// settled against it; a refusal for room counted; no answer left to the shard's reconciliation, which releases a claim
/// the call made that no copy names ([`reconcile_claims`]).
fn finish_admission(
  state: &mut ShardState,
  object: ObjectId,
  answer: Option<Result<u64, slates_ipc::protocol::Refusal>>,
) -> Admission {
  state.live.admitting.remove(&object);
  match answer {
    Some(Ok(bytes)) => {
      learn_claim(state, object, bytes);
      Admission::Serve
    }
    Some(Err(refusal)) => {
      state.count(CLAIM_NO_ROOM, 1);
      let available = match refusal {
        slates_ipc::protocol::Refusal::BudgetExceeded { available } => available,
        _ => 0,
      };
      Admission::NoRoom(available)
    }
    None => {
      state.live.claims_dirty.insert(partition_of(object));
      state.count(CLAIM_NOT_NOW, 1);
      Admission::NotNow
    }
  }
}

/// Learns that `object`'s claim holds `bytes`, and settles its copy's charge against it: what the claim now covers is
/// credited back here.
fn learn_claim(state: &mut ShardState, object: ObjectId, bytes: u64) {
  state.live.claims.insert(object, bytes);
  let settled = {
    let ShardState { live, store, .. } = &mut *state;
    let Some(held) = live.held.get_mut(&object) else {
      return;
    };
    let mut room = Room {
      store,
      claimed: bytes,
    };
    let kept = held.bytes;
    room.settle(held, kept).is_ok()
  };
  if !settled {
    state.count(CLAIM_UNSETTLED, 1);
  }
}

/// Reconciles the claims of each partition marked ([`LiveState::claims_dirty`]) with the copies this node holds (§4.2,
/// g229): the partition's shard releases a claim that no copy holding entries and no running admission names, and says
/// where each named one stands ([`crate::verbs::reconcile_candidate_claims`]); a copy whose claim is missing (a warm
/// restart's) has it admitted again. A shard that does not answer within the call's deadline stays marked for the
/// next period. Run by the control shard's record period; costs nothing while no copy was let go and no admission
/// went unanswered.
pub(crate) async fn reconcile_claims(origin: u16) {
  let read_ns = slates_rt::futures::now_ns();
  let marked = state::with_state(|s| {
    settle_promised(s);
    std::mem::take(&mut s.live.claims_dirty)
      .into_iter()
      .filter_map(|partition| {
        let target = s.shards.get(usize::from(partition)).copied()?;
        Some((partition, target, claims_kept(s, partition)))
      })
      .collect::<Vec<_>>()
  })
  .unwrap_or_default();
  for (partition, target, kept) in marked {
    let answer = crate::xshard::call_within(
      origin,
      target,
      move |s| crate::verbs::reconcile_candidate_claims(s, &kept, read_ns),
      ADMISSION_WAIT_NS,
    )
    .await;
    let missing = state::with_state(|s| match answer {
      Some(found) => apply_reconciled(s, found),
      None => {
        s.live.claims_dirty.insert(partition);
        s.count(CLAIM_RECONCILE_UNANSWERED, 1);
        Vec::new()
      }
    })
    .unwrap_or_default();
    for object in missing {
      readmit(origin, object).await;
    }
  }
}

/// The volumes routed to `partition` whose claims a reconciliation keeps ([`reconcile_claims`]): each copy this node holds
/// entries of, each admission running, and each claim an owner's ask made that still awaits its log ([`serve_admit`]).
fn claims_kept(state: &ShardState, partition: u16) -> BTreeSet<slates_db::catalog::VolumeId> {
  state
    .live
    .held
    .iter()
    .filter(|(_, held)| !held.entries.is_empty())
    .map(|(object, _)| *object)
    .chain(state.live.admitting.keys().copied())
    .chain(state.live.promised.keys().copied())
    .filter(|object| partition_of(*object) == partition)
    .map(|object| slates_db::catalog::VolumeId { bytes: object.0 })
    .collect()
}

/// Lets go of the claims an owner's ask made ([`serve_admit`]) whose log has come, which a held copy keeps from now, and
/// of those whose log has not come within the failover delay, whose partitions the next reconciliation then releases them
/// from ([`reconcile_claims`]).
fn settle_promised(state: &mut ShardState) {
  let now = slates_rt::futures::now_ns();
  let bound = state.failover_ns;
  let settled: Vec<ObjectId> = state
    .live
    .promised
    .iter()
    .filter(|(object, admitted)| {
      state
        .live
        .held
        .get(object)
        .is_some_and(|held| !held.entries.is_empty())
        || now.saturating_sub(**admitted) >= bound
    })
    .map(|(object, _)| *object)
    .collect();
  for object in settled {
    state.live.promised.remove(&object);
    let holds_log = state
      .live
      .held
      .get(&object)
      .is_some_and(|held| !held.entries.is_empty());
    if !holds_log {
      state.live.claims.remove(&object);
      state.live.claims_dirty.insert(partition_of(object));
      state.count(CLAIM_PROMISE_LAPSED, 1);
    }
  }
}

/// Counter: a claim an owner's ask made whose log did not come within the failover delay, released (§4.2, A-401).
/// Format: a counter name in the daemon's status report.
const CLAIM_PROMISE_LAPSED: &str = "fleet.live.claim_promise_lapsed";
/// Counter: an owner's admission ask this candidate admitted (§4.2, A-401).
/// Format: a counter name in the daemon's status report.
const CLAIM_ASK_ADMITTED: &str = "fleet.live.claim_ask_admitted";

/// A candidate's answer to an owner's ask that it make a volume's admission before the owner turns live shipping on
/// (§4.2, A-401; [`slates_db::live::Admit`]): authorized as a content placement is (`fleet::content_authority`, the
/// volume's acting owner placing at a candidate; no catalog is needed, the ask names the claim), the claim reserved on
/// the shard the volume's id routes to as a first entry's is ([`admit_claim`]), and kept until the log's first entry
/// comes or the failover delay passes ([`settle_promised`]). Answered with the empty head when the claim is held,
/// `NoRoom` when it cannot be, and nothing for a sender this node does not serve or an admission it could not finish
/// within its call. Every ask is answered by the volume's shard, whose admission answers a claim it holds already, and
/// not by this shard's view of its claims ([`LiveState::claims`]), which a reconciliation may have left behind the
/// shard; so an admitted answer always names a claim the shard holds, and its promise starts again at each ask. Run on
/// the control shard by the record plane.
pub(crate) async fn serve_admit(
  origin: u16,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Ok(admit) = slates_db::live::Admit::decode(request) else {
    let _ = state::with_state_counted(|s| count_refused(s, REFUSED_MALFORMED));
    return Vec::new();
  };
  let object = admit.object;
  let begun = state::with_state(|s| {
    let authority = crate::fleet::content_authority(
      s.council.configuration(),
      s.holder_records.as_map(),
      local,
      peer,
      slates_cluster::content::ContentAccess::Place,
      object,
    );
    if let Err(why) = authority {
      count_refused(s, not_owner_counter(why));
      return Err(Vec::new());
    }
    let target = s
      .shards
      .get(usize::from(partition_of(object)))
      .copied()
      .ok_or_else(Vec::new)?;
    let now = slates_rt::futures::now_ns();
    if s
      .live
      .admitting
      .get(&object)
      .is_some_and(|began| now.saturating_sub(*began) < ADMISSION_WAIT_NS)
    {
      s.count(CLAIM_NOT_NOW, 1);
      return Err(Vec::new());
    }
    s.live.admitting.insert(object, now);
    Ok(target)
  });
  let target = match begun {
    Some(Ok(target)) => target,
    Some(Err(answer)) => return answer,
    None => return Vec::new(),
  };
  let id = slates_db::catalog::VolumeId { bytes: object.0 };
  let size = crate::verbs::wire_size(admit.size);
  let require_locked = admit.require_locked;
  let answer = crate::xshard::call_within(
    origin,
    target,
    move |s| crate::verbs::admit_candidate_claim(s, id, size, require_locked),
    ADMISSION_WAIT_NS,
  )
  .await;
  state::with_state(|s| match finish_admission(s, object, answer) {
    Admission::Serve => {
      s.live.promised.insert(object, slates_rt::futures::now_ns());
      s.count(CLAIM_ASK_ADMITTED, 1);
      encode_reply(&Ok(None))
    }
    refused => refused.refusal().unwrap_or_default(),
  })
  .unwrap_or_default()
}

/// What an owner's admission asks came to ([`admission_verdict`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Asked {
  /// Every candidate that answered holds the claim. `unanswered` gave no answer within the exchange; each makes its
  /// admission as it takes the log's first entry ([`admit_claim`]).
  Admitted {
    /// The candidates that gave no answer.
    unanswered: usize,
  },
  /// A candidate has no room for the claim: the least any refusing candidate could admit.
  NoRoom(u64),
}

/// The verdict of an owner's admission asks to `candidates` from their `answers` (§4.2, A-401): `NoRoom` with the least
/// room a refusing candidate named when any refused, else admitted, counting those that gave no answer.
pub(crate) fn admission_verdict(candidates: &[HostId], answers: &[(HostId, Vec<u8>)]) -> Asked {
  let mut least: Option<u64> = None;
  let mut answered = 0usize;
  for host in candidates {
    let reply = answers
      .iter()
      .find(|(from, _)| from == host)
      .map(|(_, bytes)| decode_reply(bytes));
    match reply {
      Some(Ok(_)) => answered = answered.saturating_add(1),
      Some(Err(LiveError::NoRoom { available })) => {
        answered = answered.saturating_add(1);
        least = Some(least.map_or(available, |least| least.min(available)));
      }
      _ => {}
    }
  }
  match least {
    Some(available) => Asked::NoRoom(available),
    None => Asked::Admitted {
      unanswered: candidates.len().saturating_sub(answered),
    },
  }
}

/// Asks each of `candidates` to make `admit`'s admission (§4.2, A-401), over this control shard's record sessions within
/// the live exchange's budget: each answer, as its candidate gave it. The stragglers are finished here, as a lane's are
/// ([`finish_stragglers`]), but a late answer is dropped ([`drop_late_answers`]): the verdict is taken without it, and an
/// admission's answer is no holder's reply to a shipper, so it must not reach [`fold_into`]. Only a shard that can no
/// longer time the wait, at its shutdown, leaves the rest to the drain ([`drain_stragglers`]).
pub(crate) async fn ask_admissions(
  admit: slates_db::live::Admit,
  candidates: Vec<HostId>,
) -> Vec<(HostId, Vec<u8>)> {
  let sessions = crate::fleet::take_sessions("fleet.lent_to.live_admission", |host| {
    candidates.contains(&host)
  });
  let request = admit.encode();
  let exchanged = slates_cluster::live::exchange(
    (slates_cluster::live::LIVE_STATE_STREAM, &request),
    sessions,
    crate::fleet::live_budget(),
    |_, _| false,
  )
  .await;
  let (replies, mut stragglers) = settle_sessions(exchanged);
  let (late, done) = stragglers.settle().await;
  drop_late_answers(late);
  if !done {
    settle_stragglers(
      state::with_state(|s| s.shard).unwrap_or_default(),
      admit.object,
      stragglers,
    );
  }
  replies
}

/// Counter: an admission ask's answer that came after its exchange gave up, dropped (§4.2, A-401; [`ask_admissions`]).
/// Format: a counter name in the daemon's status report.
const LIVE_ADMISSION_LATE: &str = "fleet.live.admission_late";

/// Puts an admission exchange's late stragglers' sessions back (on the control shard): each answered holder's session to
/// the pool, each unanswered holder's link dropped to be dialed afresh, as [`deliver_late`] does; their answers are
/// counted and dropped, since the verdict they would have joined is taken.
fn drop_late_answers(
  late: Vec<(
    HostId,
    slates_cluster::TimedReply,
    slates_transport::endpoint::Endpoint,
  )>,
) {
  let mut sessions = Vec::new();
  let mut lost = Vec::new();
  for (host, reply, endpoint) in late {
    if reply.round_trip_ns.is_none() {
      lost.push(host);
    } else {
      sessions.push((host, endpoint));
    }
  }
  let dropped = u64::try_from(sessions.len()).unwrap_or(u64::MAX);
  lose_links(&lost);
  crate::fleet::return_sessions(sessions);
  if dropped > 0 {
    let _ = state::with_state_counted(|s| s.count(LIVE_ADMISSION_LATE, dropped));
  }
}

/// Counter: a `live-ship` refused `BudgetExceeded` because a candidate had no room for the volume's claim (A-401).
/// Format: a counter name in the daemon's status report.
const LIVE_SHIP_NO_ROOM: &str = "fleet.live.ship_refused.no_room";
/// Counter: candidates that gave no answer to a `live-ship`'s admission ask; each makes its admission at the first entry.
/// Format: a counter name in the daemon's status report.
const LIVE_SHIP_UNANSWERED: &str = "fleet.live.ship_unanswered";

/// What a `live-ship` that turns `volume`'s live shipping on answers once its candidates' admissions are judged (§4.2,
/// A-401): the flag recorded when none refused, the candidates that gave no answer counted; `BudgetExceeded` with the
/// least room a candidate named otherwise, the flag left as it was. The same whether the verdict came from a task's asks
/// ([`admit_then_ship`]) or there was no candidate to ask (`verbs::live_ship`).
pub(crate) fn ship_after(
  state: &mut ShardState,
  volume: slates_ipc::protocol::VolumeId,
  verdict: Asked,
) -> slates_ipc::protocol::ReplyBody {
  match verdict {
    Asked::Admitted { unanswered } => {
      if unanswered > 0 {
        state.count(
          LIVE_SHIP_UNANSWERED,
          u64::try_from(unanswered).unwrap_or(u64::MAX),
        );
      }
      crate::verbs::set_live_shipping(state, volume, true)
    }
    Asked::NoRoom(available) => {
      state.count(LIVE_SHIP_NO_ROOM, 1);
      slates_ipc::protocol::ReplyBody::Refused {
        refusal: slates_ipc::protocol::Refusal::BudgetExceeded { available },
      }
    }
  }
}

/// The task a `live-ship` that turns a volume's live shipping on runs (§4.2, A-401; `verbs::live_ship`): on its owner
/// shard `origin`, it asks every candidate of the volume's placement to make the claim's admission, from the control
/// shard `control`, which holds the sessions ([`ask_admissions`]), then completes the request as one atom
/// (`image::complete`): the flag recorded when no candidate refused, else `BudgetExceeded` with the least room a
/// candidate named, nothing recorded but the outcome ([`ship_after`]). So nothing is published before every candidate
/// that answered holds its claim. The asks are awaited in the control shard's own time (A-408's rule for a wait on
/// another shard of this daemon, `xshard::task_in_shard_time`) over the exchange's own bound: a busy or starved control
/// shard is waited for, and only one that ran nothing for a whole window is given up, its candidates then counted as
/// unanswered.
pub(crate) async fn admit_then_ship(
  (origin, control): (u16, u16),
  request: (u64, slates_wire::request::RequestId),
  volume: slates_ipc::protocol::VolumeId,
  admit: slates_db::live::Admit,
  candidates: Vec<HostId>,
) {
  let asked = candidates.clone();
  let answers = crate::xshard::task_in_shard_time(
    origin,
    control,
    move || ask_admissions(admit, asked),
    crate::fleet::live_budget().max_deadline_ns(),
  )
  .await
  .unwrap_or_default();
  let verdict = admission_verdict(&candidates, &answers);
  let completed =
    state::with_state(|s| crate::image::complete(s, request, |s| ship_after(s, volume, verdict)));
  if let Some((reply, route)) = completed {
    let _ = state::with_state_counted(|s| crate::image::deliver(s, reply, route));
  }
}

/// Applies a partition's answer to a reconciliation ([`reconcile_claims`]): a claim held there is learned, one taken
/// over by the volume this node now serves is left as it was learned, and the copies whose claim is missing are returned
/// for admission.
fn apply_reconciled(
  state: &mut ShardState,
  found: Vec<(slates_db::catalog::VolumeId, crate::verbs::ClaimFound)>,
) -> Vec<ObjectId> {
  let mut missing = Vec::new();
  for (id, found) in found {
    let object = ObjectId(id.bytes);
    match found {
      crate::verbs::ClaimFound::Claimed(bytes) => learn_claim(state, object, bytes),
      crate::verbs::ClaimFound::Served => {}
      crate::verbs::ClaimFound::Missing => {
        if state.live.held.contains_key(&object) {
          state.live.claims.remove(&object);
          missing.push(object);
        }
      }
    }
  }
  missing
}

/// Admits again the claim of a copy this node holds whose claim is missing (a warm restart's; [`reconcile_claims`]): as
/// [`admit_claim`] does for a message, with no sender to authorize, since the copy is held already. Refused for room, the
/// copy is kept and its log stays charged here whole; counted, and tried again when its partition is next reconciled.
async fn readmit(origin: u16, object: ObjectId) {
  let begun = state::with_state(|s| {
    let catalog = held_catalog(s, object)?;
    let target = s.shards.get(usize::from(partition_of(object))).copied()?;
    s.live
      .admitting
      .insert(object, slates_rt::futures::now_ns());
    Some((
      target,
      crate::verbs::taken_over_size(&catalog),
      catalog.require_locked,
    ))
  })
  .flatten();
  let Some((target, size, require_locked)) = begun else {
    return;
  };
  let id = slates_db::catalog::VolumeId { bytes: object.0 };
  let answer = crate::xshard::call_within(
    origin,
    target,
    move |s| crate::verbs::admit_candidate_claim(s, id, size, require_locked),
    ADMISSION_WAIT_NS,
  )
  .await;
  let _ = state::with_state_counted(|s| {
    if let Admission::NoRoom(_) = finish_admission(s, object, answer) {
      s.count(LIVE_RESTORED_UNCHARGED, 1);
      s.live.claims_dirty.insert(partition_of(object));
    }
  });
}

// ── Takeover from a live log (A-200 "Takeover") ─────────────────────────────────────────────────────────────────────

/// Counter: a takeover of a live volume that could not rebuild it from the adopted log (its first entry not whole, an
/// entry that would not open or apply, or content missing for an extent): the volume is not served from it.
const LIVE_TAKEOVER_REFUSED: &str = "fleet.live.takeover_refused";
/// Counter: a takeover refused because the lineage key the log's whole entry carries could not be adopted.
const REFUSED_LINEAGE: &str = "fleet.live.takeover_refused.lineage";
/// Counter: a takeover refused because an entry's payload would not open (its seal, or its bytes).
const REFUSED_OPEN: &str = "fleet.live.takeover_refused.open";
/// Counter: a takeover refused because the log is sealed and this node holds no lineage or naming key for it: its own
/// key record, not the log's bytes ([`next_fetch`] waits for the key).
const REFUSED_NO_KEY: &str = "fleet.live.takeover_refused.no_key";
/// Counter: a takeover refused because an opened entry's record would not decode.
const REFUSED_RECORD: &str = "fleet.live.takeover_refused.record";
/// Counter: a takeover refused because a delta would not apply to the image before it.
const REFUSED_APPLY: &str = "fleet.live.takeover_refused.apply";
/// Counter: a takeover refused because the log's first entry is not a whole record (or there is none).
const REFUSED_NOT_WHOLE_FIRST: &str = "fleet.live.takeover_refused.not_whole_first";
/// Counter: a takeover refused because the rebuilt image could not be laid out as an archive (no root, a cycle,
/// content missing for an extent).
const REFUSED_LAYOUT: &str = "fleet.live.takeover_refused.layout";
/// Counter: a takeover's rebuild stopped at a payload whose framing in the fetched answer ran past the answer or over the
/// hold's cap (g251: the payloads are read in place, each as it is applied). The answer's, not the log's: it is not
/// among the [`REBUILD_REFUSALS`] [`note_rebuild`] keeps, so the next attempt fetches the log again, as a fetch whose
/// answer did not decode always has.
const REFUSED_FETCHED: &str = "fleet.live.takeover_refused.fetched";
/// Counter: a taken-over live volume rebuilt from the adopted log.
const LIVE_TAKEN_OVER: &str = "fleet.live.taken_over";
/// Counter: a live takeover's phase one adopted past tolerance (A-309): fewer than `f + 1` recorded holders were still
/// members, so every surviving member that may hold the log reported first, and the greatest head among them was
/// adopted. Counted per phase one that adopted, a refetch included. Zero is the reading of a region within tolerance.
const LIVE_TAKEN_OVER_PAST_TOLERANCE: &str = "fleet.live.taken_over_past_tolerance";

/// What a takeover fetched of a live volume: the adopted head's payloads (a whole one first), and the
/// successor's copy of the lineage key the first carries (A-92 piece 4c), unwrapped on this control shard.
pub(crate) struct Fetched {
  /// The head adopted, which the payloads end at.
  pub(crate) head: Head,
  /// The payloads, in place in the answer that carried them (g251).
  pub(crate) log: slates_db::live::FetchedLog,
  pub(crate) lineage: Option<([u8; 16], Vec<u8>)>,
  pub(crate) naming: Option<crate::head::HeadNaming>,
  /// The first payload's sealing, when the log is sealed: kept with a refusal for a missing key ([`Refused`]).
  pub(crate) sealing: Option<crate::head::HeadSealing>,
  /// Set when phase one adopted the head past tolerance (A-309, A-332): `f + 1` of the recorded holders, and how many of
  /// them were members ([`live_shortfall`]). The volume's loss report names it.
  pub(crate) past_tolerance: Option<slates_db::catalog::RecoveryShortfall>,
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
///
/// Past tolerance, when fewer than `f + 1` of the recorded holders may still hold the log (`slates_db::live::past_tolerance`:
/// members, and retired hosts this node never saw restart, which may return with their copies, A-345), that count can
/// never be met, and the takeover waited for good (g240's finding at `f = 1`: {survivor, destroyed node} recorded, the
/// replacement that caught up on the log not among them). Then the members that exist now recover it (A-309): every
/// recorded holder that may still hold the log and every surviving member of `cohort_hosts` (where the retired owner
/// placed the volume, settled and current, so where a joint transition shipped its log) must report, and the greatest
/// head among all the reports is adopted (`slates_db::live::adopt_past_tolerance`), counted
/// [`LIVE_TAKEN_OVER_PAST_TOLERANCE`]. This node's own report is known whatever it holds ([`past_tolerance_wait`]).
pub(crate) async fn fetch(
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
  candidates: Vec<HostId>,
  holders: Vec<HostId>,
  cohort_hosts: Vec<HostId>,
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
  // its quorum without end (370 and 371 waits under load, 2026-11-05; `a_successor_that_holds_no_copy_…`). Its fence is
  // a promise as a remote candidate's is (`serve_report`), kept by a publication before it counts (A-344): unpublished,
  // a warm restart brought the copy back at its old fence (1 after a takeover at 2, a Mac Studio, 2026-11-18), where a
  // superseded owner resumed could have its entries taken.
  let own = state::with_state(|s| {
    let held = fenced_copy(s, local, object, epoch).ok()?;
    held.replica.reassign(local);
    let head = held.replica.head();
    if crate::verbs::publish_shard(s).is_err() {
      s.count(LIVE_OWN_FENCE_UNPUBLISHED, 1);
      return None;
    }
    Some(head)
  })
  .flatten();
  if let Some(head) = own {
    reports.push((local, head));
  }
  // Past tolerance (A-309): the recorded holders' count can never be met, so every surviving member that may hold the
  // log is waited for instead.
  let (members, restarted): (Vec<HostId>, BTreeSet<HostId>) =
    state::with_state(|s| (s.council.configuration().members.clone(), restarted_ids(s)))
      .unwrap_or_default();
  let wait = past_tolerance_wait(
    &holders,
    &cohort_hosts,
    (&members, &restarted),
    local,
    quorum,
  );
  let past_tolerance = wait
    .as_ref()
    .map(|_| live_shortfall(&holders, &members, quorum));
  let others = others_to_ask(candidates, &holders, wait.as_deref(), local);
  // Phase one proper: every other candidate and recorded holder reports its head and the bytes it keeps, a few bytes
  // each, within the liveness budget whatever the path.
  for (host, bytes) in ask_reports(object, epoch, &others, crate::fleet::live_budget()).await {
    if let Ok((Ok(head), bytes)) = slates_db::live::decode_reported(&bytes) {
      reports.push((host, head));
      kept.insert(host, bytes);
    }
  }
  let Some((holder, head)) = phase_one_choice(&reports, &holders, wait.as_deref(), quorum)? else {
    return Ok(None);
  };
  let refused = state::with_state(|s| s.live.refused_rebuilds.get(&object).cloned()).flatten();
  // The hosts that answered phase one, this node left out: the fellow candidates a key this node lacks is asked of.
  let answered: Vec<HostId> = reports
    .iter()
    .map(|(host, _)| *host)
    .filter(|host| *host != local)
    .collect();
  // A key handed over for an earlier attempt at this log, kept with its refusal (A-295).
  let mut handed = refused.as_ref().and_then(|refused| refused.handed.clone());
  let key_here = handed.is_some()
    || refused
      .as_ref()
      .and_then(|refused| refused.sealing.as_ref())
      .is_some_and(|sealing| {
        state::with_state(|s| crate::seal_keys::successor_lineage(s, sealing).is_some())
          .unwrap_or(false)
      });
  let mut decision = next_fetch(
    refused.as_ref(),
    head,
    slates_rt::futures::now_ns(),
    key_here,
  );
  // Only the owner's shard delivers a pair, so with the owner lost the key a successor lacks can come only from a fellow
  // candidate that holds it (A-295): asked once an attempt while the wait lasts.
  if decision == NextFetch::WaitsKey
    && let Some(sealing) = refused
      .as_ref()
      .and_then(|refused| refused.sealing.as_ref())
    && let Some(key) = handed_lineage(object, sealing, &answered).await
  {
    handed = Some(key);
    decision = NextFetch::Fetch;
  }
  match decision {
    NextFetch::Fetch => {
      if key_here
        && refused
          .as_ref()
          .is_some_and(|refused| refused.why == REFUSED_NO_KEY)
      {
        let _ = state::with_state_counted(|s| s.count(LIVE_TAKEOVER_KEY_ARRIVED, 1));
      }
    }
    NextFetch::WaitsNewHead => return Err(LIVE_TAKEOVER_WAITS_NEW_HEAD),
    NextFetch::WaitsKey => return Err(LIVE_TAKEOVER_WAITS_KEY),
    NextFetch::KeyExhausted => return Err(LIVE_TAKEOVER_NO_KEY_EXHAUSTED),
  }
  let log = if holder == local {
    own_log(object, epoch, head, cap).await?
  } else {
    let bytes = kept.get(&holder).copied().unwrap_or(0);
    fetch_log(object, epoch, (holder, head), bytes, cap).await?
  };
  let sealing = log
    .first()
    .and_then(sealed_body)
    .and_then(|body| body.sealing);
  let lineage = lineage_for_rebuild(object, sealing.as_ref(), handed, &answered).await;
  let naming = sealing.as_ref().and_then(|sealing| sealing.naming.clone());
  Ok(Some(Fetched {
    head,
    log,
    lineage,
    naming,
    sealing,
    past_tolerance,
  }))
}

/// The shortfall a live phase one past tolerance recovered at (A-332): the `f + 1` of the recorded `holders` its count
/// needs, and how many of them are `members`.
fn live_shortfall(
  holders: &[HostId],
  members: &[HostId],
  quorum: slates_db::register::Quorum,
) -> slates_db::catalog::RecoveryShortfall {
  let remaining = holders
    .iter()
    .filter(|holder| members.contains(holder))
    .count();
  slates_db::catalog::RecoveryShortfall {
    needed: u64::from(quorum.f).saturating_add(1),
    remaining: u64::try_from(remaining).unwrap_or(u64::MAX),
  }
}

/// The lineage key a rebuild opens the log sealed under `sealing` with: this node's own entry; else the key a fellow
/// candidate handed over for that lineage (`handed`), or asked now of the hosts that answered phase one (`answered`), so a
/// successor that holds no entry of its own is served at its first attempt, not a refusal later (A-295); none for a log
/// shipped in the clear.
async fn lineage_for_rebuild(
  object: ObjectId,
  sealing: Option<&crate::head::HeadSealing>,
  handed: Option<([u8; 16], Vec<u8>)>,
  answered: &[HostId],
) -> Option<([u8; 16], Vec<u8>)> {
  let sealing = sealing?;
  if let Some(own) =
    state::with_state(|s| crate::seal_keys::successor_lineage(s, sealing)).flatten()
  {
    return Some(own);
  }
  match handed.filter(|(id, _)| *id == sealing.lineage) {
    Some(handed) => Some(handed),
    None => handed_lineage(object, sealing, answered).await,
  }
}

/// The lineage key of the log `sealing` belongs to, handed to this successor by a fellow candidate (A-295): asked of the
/// hosts that answered phase one (`fleet::ask_handed_lineage`), counted, and kept with the log's refusal when it has
/// one, so a later attempt fetches with it rather than asking again. `None` when none handed one that checked.
async fn handed_lineage(
  object: ObjectId,
  sealing: &crate::head::HeadSealing,
  answered: &[HostId],
) -> Option<([u8; 16], Vec<u8>)> {
  let key = crate::fleet::ask_handed_lineage(object, sealing, answered).await?;
  let kept = key.clone();
  let _ = state::with_state_counted(|s| {
    if let Some(refused) = s.live.refused_rebuilds.get_mut(&object) {
      refused.handed = Some(kept);
    }
    s.count(LIVE_TAKEOVER_KEY_HANDED, 1);
  });
  Some(key)
}

/// A rebuild refused at a head ([`note_rebuild`]): what decides whether, and when, the log is fetched again
/// ([`next_fetch`]). The whole entry's sealing is kept (a few kilobytes) so a refusal for a missing key can tell, with no
/// fetch, when the key has come.
#[derive(Clone, Debug)]
pub(crate) struct Refused {
  /// The head the log was refused at.
  pub(crate) head: Head,
  /// The refusal's counter name.
  pub(crate) why: &'static str,
  /// When it was first refused at that head for that reason.
  pub(crate) since: u64,
  /// The log's whole entry's sealing, when it was sealed.
  pub(crate) sealing: Option<crate::head::HeadSealing>,
  /// A key a fellow candidate handed this node for the log while it waited for one (A-295): the key's id and the key
  /// wrapped under this node's root, as `seal_keys::successor_lineage` returns one. Kept with the refusal it answers, so
  /// it lives exactly as long as the takeover waiting for it.
  pub(crate) handed: Option<([u8; 16], Vec<u8>)>,
}

/// What a successor does about a live volume's log at the head phase one adopted ([`next_fetch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextFetch {
  /// Fetch it.
  Fetch,
  /// Not again at this head: its bytes decided the refusal.
  WaitsNewHead,
  /// Not yet: this node holds no key for it, and none has come.
  WaitsKey,
  /// Not at this head: no key came within [`NO_KEY_WAIT_NS`].
  KeyExhausted,
}

/// Counter: a takeover holding no key for the adopted log, waiting for one at its head ([`next_fetch`]).
/// Format: a counter name in the daemon's status report.
const LIVE_TAKEOVER_WAITS_KEY: &str = "fleet.live.takeover_waits_key";
/// Counter: a takeover that fetched the log again because the key it lacked has come ([`next_fetch`]): once per arrival.
/// Format: a counter name in the daemon's status report.
const LIVE_TAKEOVER_KEY_ARRIVED: &str = "fleet.live.takeover_key_arrived";
/// Counter: a takeover handed the log's key by a fellow candidate, at its first fetch or while it waited for the key
/// (A-295): once a key handed.
/// Format: a counter name in the daemon's status report.
const LIVE_TAKEOVER_KEY_HANDED: &str = "fleet.live.takeover_key_handed";
/// Counter: a takeover whose key for the adopted log did not come within [`NO_KEY_WAIT_NS`]: no longer waited for at
/// that head, answered typed.
/// Format: a counter name in the daemon's status report.
const LIVE_TAKEOVER_NO_KEY_EXHAUSTED: &str = "fleet.live.takeover_no_key_exhausted";
/// Derived: how long a successor waits at one head for the key to a log it holds no key for: ten liveness budgets
/// (`daemon::OBSERVE_BUDGET_NS`), past a pair's redelivery (once a record period) many times over.
const NO_KEY_WAIT_NS: u64 = crate::daemon::OBSERVE_BUDGET_NS;

/// The hosts a live phase one waits for past tolerance (A-309), `None` within it. A recorded holder may still hold the log
/// while it is one of the `members`, or while it is not but this node never saw it restart (`restarted`, A-345): a host
/// retired on a death verdict that was false (a stopped process, a partition that healed) refutes with its copy whole,
/// so it still counts toward tolerance, and within tolerance A-240's count waits for its report. A host seen restarted
/// came back under another member id with its RAM copy gone. Past tolerance, when fewer than `f + 1` of the recorded
/// `holders` may still hold the log (`slates_db::live::past_tolerance`), every recorded holder that may and every member
/// of `cohort_hosts` (where the retired owner placed the volume) is waited for, `local` aside, since this node's own copy
/// is known whatever it holds. Before A-345 a retired holder counted as gone: the fresh-member flow's owner died while
/// the holder with its newest entries was stopped and retired, and the members that exist now adopted a stale head
/// without it (chaos-166..169 lost; CI diag run 44288273676, Linux x86_64, 2026-11-18).
fn past_tolerance_wait(
  holders: &[HostId],
  cohort_hosts: &[HostId],
  (members, restarted): (&[HostId], &BTreeSet<HostId>),
  local: HostId,
  quorum: slates_db::register::Quorum,
) -> Option<Vec<HostId>> {
  let may_hold = |host: HostId| members.contains(&host) || !restarted.contains(&host);
  if !slates_db::live::past_tolerance(holders, may_hold, quorum) {
    return None;
  }
  let mut wait: Vec<HostId> = holders
    .iter()
    .copied()
    .filter(|host| may_hold(*host))
    .chain(
      cohort_hosts
        .iter()
        .copied()
        .filter(|host| members.contains(host)),
    )
    .filter(|host| *host != local)
    .collect();
  wait.sort_unstable_by_key(|host| host.0);
  wait.dedup();
  Some(wait)
}

/// The member ids this node saw end at a restart of their anchor (`state::LearnedMember::previous`, A-345): each came
/// back under another id, its RAM copies gone with its process.
fn restarted_ids(state: &ShardState) -> BTreeSet<HostId> {
  state
    .learned_members
    .values()
    .filter_map(|learned| learned.previous)
    .collect()
}

/// The hosts phase one asks besides this node (`local`): every candidate, then every recorded holder and every host a
/// phase one past tolerance waits for (`wait`, A-309) not among them, so a holder outside the placement is asked too.
fn others_to_ask(
  candidates: Vec<HostId>,
  holders: &[HostId],
  wait: Option<&[HostId]>,
  local: HostId,
) -> Vec<HostId> {
  let mut others: Vec<HostId> = candidates
    .into_iter()
    .filter(|host| *host != local)
    .collect();
  for holder in holders.iter().chain(wait.into_iter().flatten()) {
    if *holder != local && !others.contains(holder) {
      others.push(*holder);
    }
  }
  others
}

/// Phase one's choice from `reports` (A-240, A-309): within tolerance (`wait` is `None`), the greatest head once `f + 1`
/// of the recorded `holders` reported (`slates_db::live::adopt`), else the takeover waits for its quorum; past it, the
/// greatest head once every host of `wait` reported (`slates_db::live::adopt_past_tolerance`), counted
/// [`LIVE_TAKEN_OVER_PAST_TOLERANCE`], else the takeover waits for them. `Ok(None)` when nobody holds any of the log.
fn phase_one_choice(
  reports: &[(HostId, Option<Head>)],
  holders: &[HostId],
  wait: Option<&[HostId]>,
  quorum: slates_db::register::Quorum,
) -> Result<Option<(HostId, Head)>, &'static str> {
  let Some(wait) = wait else {
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
    return Ok(slates_db::live::adopt(reports, holders, quorum));
  };
  match slates_db::live::adopt_past_tolerance(reports, wait) {
    slates_db::live::PastTolerance::Waiting => Err("fleet.live.takeover_waits_past_tolerance"),
    slates_db::live::PastTolerance::Adopt(adopted) => {
      let _ = state::with_state_counted(|s| s.count(LIVE_TAKEN_OVER_PAST_TOLERANCE, 1));
      Ok(adopted)
    }
  }
}

/// Counter: a taken-over live volume's log was not fetched again, its rebuild having been refused at the head phase
/// one adopted now ([`next_fetch`]).
/// Format: a counter name in the daemon's status report.
const LIVE_TAKEOVER_WAITS_NEW_HEAD: &str = "fleet.live.takeover_waits_new_head";

/// What a successor does about the log at `adopted`, given its last refusal (`None`: none) and whether the key that
/// refusal lacked can be unwrapped here now (`key_here`, read from the refused whole entry's sealing; no fetch). A
/// refusal the log's bytes decide is final for that head (its first payload's sealing, its records and its deltas are
/// fixed by the chain identity), so the log is fetched again only once phase one adopts another head. The lineage
/// refusal is this node's own key record, retried. A refusal for a key this node does not hold waits for the key, an
/// event, and the log is fetched once when it has come, never each period: the successor that could not open the log
/// fetched it 104 to 204 times a run (2026-11-09). Bounded by [`NO_KEY_WAIT_NS`], then answered typed.
fn next_fetch(refused: Option<&Refused>, adopted: Head, now: u64, key_here: bool) -> NextFetch {
  match refused {
    None => NextFetch::Fetch,
    Some(refused) if refused.why == REFUSED_LINEAGE || refused.head != adopted => NextFetch::Fetch,
    Some(refused) if refused.why == REFUSED_NO_KEY => {
      if now.saturating_sub(refused.since) >= NO_KEY_WAIT_NS {
        NextFetch::KeyExhausted
      } else if key_here {
        NextFetch::Fetch
      } else {
        NextFetch::WaitsKey
      }
    }
    Some(_) => NextFetch::WaitsNewHead,
  }
}

/// Records on the control shard that `object`'s rebuild from the log at `head` was refused for `why`, so its log is not
/// fetched again at that head ([`next_fetch`]); forgets it once the volume is served (`why` is `None`).
pub(crate) fn note_rebuild(
  object: ObjectId,
  head: Head,
  why: Option<&'static str>,
  sealing: Option<crate::head::HeadSealing>,
) {
  let _ = state::with_state_counted(|s| match why {
    // Only a refusal of the log itself: one from the volume's restore says nothing about the log's bytes.
    Some(why) if REBUILD_REFUSALS.contains(&why) => {
      let now = slates_rt::futures::now_ns();
      let previous = s.live.refused_rebuilds.get(&object);
      // The first refusal at this head for this reason starts the clock its bound runs from; a repeat keeps it.
      let since = match previous {
        Some(refused) if refused.head == head && refused.why == why => refused.since,
        _ => now,
      };
      // A key handed over stays while it is for the log's lineage (A-295).
      let handed = previous
        .and_then(|refused| refused.handed.clone())
        .filter(|(id, _)| {
          sealing
            .as_ref()
            .is_some_and(|sealing| sealing.lineage == *id)
        });
      s.live.refused_rebuilds.insert(
        object,
        Refused {
          head,
          why,
          since,
          sealing,
          handed,
        },
      );
    }
    Some(_) => {}
    None => {
      s.live.refused_rebuilds.remove(&object);
    }
  });
}

/// The refusals of a rebuild from a live log ([`Rebuilding`]) that its bytes decide, each its counter's name.
const REBUILD_REFUSALS: [&str; 7] = [
  REFUSED_LINEAGE,
  REFUSED_NO_KEY,
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
) -> Result<slates_db::live::FetchedLog, &'static str> {
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
  // Its reply alone is read here; the payloads stay in place in the answer, each read when the rebuild applies it.
  match slates_db::live::FetchedLog::decode(fetched.1, cap) {
    Ok((Some(fetched_head), log)) if fetched_head == head => Ok(log),
    _ => Err("fleet.live.takeover_waits_log"),
  }
}

/// The successor's own copy of the log, when phase one adopted it (g251): laid out as a candidate lays out its answer to
/// a fetch ([`FetchAnswer`]), a slice a turn of this (control) shard, and read in place as a fetched log is. Cloned
/// whole in one turn, it was a copy of the whole log in the coordinator's turn. Refused as a fetch from another holder
/// is ("fleet.live.takeover_waits_log") when the copy moved before it was whole.
async fn own_log(
  object: ObjectId,
  epoch: slates_db::register::HostEpoch,
  head: Head,
  cap: usize,
) -> Result<slates_db::live::FetchedLog, &'static str> {
  let begun = state::with_state(|s| {
    s.live
      .held
      .get(&object)
      .map(|held| FetchAnswer::of(held, object, epoch))
  })
  .flatten();
  let mut answer = begun.ok_or("fleet.live.takeover_waits_log")?;
  loop {
    let laid = state::with_state(|s| {
      let slice_bytes = fetch_slice_bytes(s);
      let whole = answer.lay_out(s, slice_bytes);
      s.count(LIVE_FETCH_SLICES, 1);
      whole
    })
    .ok_or("fleet.live.takeover_waits_state")?;
    if laid {
      break;
    }
    slates_rt::futures::yield_now().await;
  }
  match slates_db::live::FetchedLog::decode(answer.into_reply(), cap) {
    Ok((Some(laid_head), log)) if laid_head == head => Ok(log),
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

/// Whether a rebuild may open the log's payloads: a sealed log only under keys this node has recorded, adopted from the
/// log's own whole entry or held before, never under a key made on the spot ([`crate::seal_keys::recorded_lineage`]).
/// Without them it is refused [`REFUSED_NO_KEY`] and waits for the key ([`next_fetch`]). Before, the rebuild made a key,
/// failed to open the log under it and was refused `open`, final for the head: the successor waited for a head that
/// could not move with its owner dead (`fleet.live.takeover_waits_new_head` 368 to 381 times a run, 2026-11-09), and the
/// key it made stayed recorded in the way of the right one.
fn keys_for_rebuild(sealed: bool, keys_recorded: bool) -> Result<(), &'static str> {
  if sealed && !keys_recorded {
    return Err(REFUSED_NO_KEY);
  }
  Ok(())
}

/// Counts a live takeover's rebuild refused, in total and by why (`fleet.live.takeover_refused.*`), as a holder's refusals
/// are counted: one counter for nine refusals hid which step a 1,000-write log failed at on KIND (runs 42178128960,
/// 42318867890: 1,512 refusals). The refusal, for its caller to return.
pub(crate) fn rebuild_refused(state: &mut ShardState, why: &'static str) -> &'static str {
  state.count(LIVE_TAKEOVER_REFUSED, 1);
  state.count(why, 1);
  why
}

/// Counts a live volume taken over (`fleet.live.taken_over`): made from its rebuilt log and published, in the turn that
/// makes it servable (A-343), so a count read means a volume a restart rebuilds.
pub(crate) fn taken_over(state: &mut ShardState) {
  state.count(LIVE_TAKEN_OVER, 1);
}

/// A live log's rebuild on its volume's new owner shard, its payloads opened, decoded and applied in order a slice at a
/// time (g251, CLAUDE.md "Bounded work everywhere"). A takeover rebuilt the whole
/// log inside the one owner-shard turn that made the volume: on a Mac Studio the nemesis's volumes, 16,737 to 27,430
/// sealed payloads of 152 to 306 MB, took 0.24 to 0.62 s to open and 0.09 to 0.44 s to apply, and with the restore and
/// the publication after them the turn ran past the watchdog's spin window, which ended the successor's process
/// (`live_chaos`'s nemesis, seeds 17036665960917068132 and 2120743995065985386). Each payload is the unit: opened under
/// the keys the rebuild began with, decoded, applied to the image a whole record began (a delta before any whole one is
/// refused), and its content runs kept, a later run at the same place over an earlier one.
pub(crate) struct Rebuilding {
  /// The lineage key and namer the payloads are opened under, when the log is sealed.
  keys: Option<(hyper_seal::keys::WrappingKey, hyper_seal::name::Namer)>,
  /// The payloads, in place in the answer that carried them; the next one taken is the next to apply.
  payloads: slates_db::live::FetchedLog,
  /// The image the payloads applied so far describe; `None` before the first whole record.
  image: Option<slates_vfs::recover::VolumeImage>,
  /// The content runs they name, by inode and offset.
  runs: BTreeMap<(u64, u64), Vec<u8>>,
  /// The volume's per-creator number, the archive header's field (`ObjectId::local`).
  volume_local: u64,
  /// The daemon's base page, the archive header's.
  page: usize,
}

/// The lineage key a takeover fetched for a live volume (its id and wrapped bytes), with the head's naming.
type AdoptedKey = (Option<([u8; 16], Vec<u8>)>, Option<crate::head::HeadNaming>);

/// A rebuild's image so far and the runs it keeps beside it.
type RebuildState<'a> = (
  &'a mut Option<slates_vfs::recover::VolumeImage>,
  &'a mut BTreeMap<(u64, u64), Vec<u8>>,
);

impl Rebuilding {
  /// Begins the rebuild of the live log `payloads` of volume `id`, owned by `owner`: the lineage key adopted when the
  /// takeover fetched one (`lineage`, with the head's `naming`), and the keys recorded here taken when the log is sealed.
  /// Refused [`REFUSED_LINEAGE`] when the key will not adopt, and as [`keys_for_rebuild`] refuses a sealed log with no
  /// key recorded.
  pub(crate) fn begin(
    state: &mut ShardState,
    id: slates_db::catalog::VolumeId,
    owner: &slates_db::catalog::Principal,
    (lineage, naming): AdoptedKey,
    payloads: slates_db::live::FetchedLog,
  ) -> Result<Rebuilding, &'static str> {
    if let Some(lineage) = lineage {
      crate::seal_keys::adopt_lineage(state, id, owner, lineage, naming.as_ref())
        .map_err(|_| REFUSED_LINEAGE)?;
    }
    // Opened only under keys recorded here, never made ([`seal_keys::recorded_lineage`]).
    let sealed = payloads
      .first()
      .is_some_and(|payload| sealed_body(payload).is_some());
    let account = crate::seal_keys::tenant_of(owner);
    let keys = if sealed {
      crate::seal_keys::recorded_lineage(state, id, account)
        .ok()
        .flatten()
        .zip(
          crate::seal_keys::recorded_namer(state, id, account)
            .ok()
            .flatten(),
        )
    } else {
      None
    };
    keys_for_rebuild(sealed, keys.is_some())?;
    Ok(Rebuilding {
      keys,
      payloads,
      image: None,
      runs: BTreeMap::new(),
      volume_local: ObjectId(id.bytes).local(),
      page: state.config.page,
    })
  }

  /// Applies the next payloads, one at least, until `spent` says the turn's share is spent; `Ok(true)` once every
  /// payload is applied. Refused as the whole rebuild is: [`REFUSED_OPEN`], [`REFUSED_RECORD`], [`REFUSED_APPLY`],
  /// [`REFUSED_NOT_WHOLE_FIRST`].
  pub(crate) fn advance(&mut self, mut spent: impl FnMut() -> bool) -> Result<bool, &'static str> {
    loop {
      let Some(payload) = self.payloads.next_payload() else {
        return Ok(true);
      };
      // Its framing broken in transit, not the log's bytes: fetched again ([`REFUSED_FETCHED`]).
      let payload = payload.map_err(|_| REFUSED_FETCHED)?;
      apply_payload(
        self.keys.as_ref(),
        (&mut self.image, &mut self.runs),
        payload,
      )?;
      if self.payloads.is_spent() {
        return Ok(true);
      }
      if spent() {
        return Ok(false);
      }
    }
  }

  /// The payloads applied so far.
  pub(crate) fn applied(&self) -> usize {
    self.payloads.taken()
  }

  /// The rebuilt volume laid out as the archive `materialize_taken_over` restores; refused
  /// [`REFUSED_NOT_WHOLE_FIRST`] when no whole record began the log.
  pub(crate) fn archive(self) -> Result<slates_archive::Archive, &'static str> {
    let image = self.image.ok_or(REFUSED_NOT_WHOLE_FIRST)?;
    archive_of(&image, &self.runs, self.volume_local, self.page)
  }
}

/// One payload of a live log opened under `keys`, decoded and applied to the rebuild's image, its runs kept: a whole
/// record begins the image again, a delta applies to it, and a delta before any whole one is refused.
fn apply_payload(
  keys: Option<&(hyper_seal::keys::WrappingKey, hyper_seal::name::Namer)>,
  (image, runs): RebuildState<'_>,
  payload: &[u8],
) -> Result<(), &'static str> {
  let publication = open_payload(keys, payload).ok_or(REFUSED_OPEN)?;
  let record =
    <VolumeRecord as Wire>::decode(&mut &publication.record[..]).map_err(|_| REFUSED_RECORD)?;
  match (record, image.as_mut()) {
    (VolumeRecord::Full { image: whole }, _) => {
      *image = Some(whole);
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
  Ok(())
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
  keys: Option<&(hyper_seal::keys::WrappingKey, hyper_seal::name::Namer)>,
  payload: &[u8],
) -> Option<LivePublication> {
  let (header, body) = payload.split_at_checked(2)?;
  let bytes = if header.get(1) == Some(&BODY_SEALED) {
    let sealed = <SealedBody as Wire>::decode(&mut &body[..]).ok()?;
    let (lineage, namer) = keys?;
    open_body(lineage, namer, sealed.pieces)?
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

/// An inode's manifest metadata: its attributes with its kind's type bits, and a device node's number (A-294), as the
/// export writes them.
fn meta_of(inode: &InodeImage) -> slates_archive::NodeMeta {
  use slates_vfs::export::{
    MODE_BLOCK_DEVICE, MODE_CHAR_DEVICE, MODE_DIRECTORY, MODE_FIFO, MODE_FILE, MODE_SOCKET,
    MODE_SYMLINK, permissions_of_mode,
  };
  use slates_vfs::recover::KindImage;
  let type_bits = match inode.kind {
    KindImage::File => MODE_FILE,
    KindImage::Dir => MODE_DIRECTORY,
    KindImage::Symlink => MODE_SYMLINK,
    KindImage::Fifo => MODE_FIFO,
    KindImage::Socket => MODE_SOCKET,
    KindImage::BlockDevice => MODE_BLOCK_DEVICE,
    KindImage::CharDevice => MODE_CHAR_DEVICE,
  };
  let (device_major, device_minor) = match inode.body {
    BodyImage::Device { major, minor } => (major, minor),
    _ => (0, 0),
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
    device_major,
    device_minor,
    xattrs: Vec::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-409 (§4.8 "Promotion and takeover"; the install storm): do refuse a message under each epoch against each fence
  /// a holder may know. Expect `StaleEpoch` at the fence for an epoch below it, which fences the sender
  /// (`Shipper::on_reply`), and `Unauthorized` for an epoch at or past it, or when no fence or no epoch is known.
  #[test]
  fn a_refused_sender_below_the_fence_a_holder_knows_is_told_that_fence() {
    let fence = Some(HostEpoch(3));
    assert_eq!(
      refusal_for(fence, Some(HostEpoch(2))),
      LiveError::StaleEpoch {
        current: HostEpoch(3)
      },
      "an old owner below the fence learns it"
    );
    for epoch in [Some(HostEpoch(3)), Some(HostEpoch(4)), None] {
      assert_eq!(
        refusal_for(fence, epoch),
        LiveError::Unauthorized,
        "epoch {epoch:?} at or past the fence, or unread: not a stale owner"
      );
    }
    assert_eq!(
      refusal_for(None, Some(HostEpoch(2))),
      LiveError::Unauthorized,
      "no fence known: nothing to tell"
    );
  }

  /// A-200, every transport's durability claim: do ask the position a reply must wait for when the publication behind it
  /// could not become an entry (the credit full with every candidate stopped), so the volume owes a whole record, while
  /// the fleet does not yet know the volume (its head unplaced). Expect the position that record will be appended at:
  /// the next one after the head, or the first when nothing was shipped yet. Before, the unplaced case returned the head
  /// as it stood, ignoring the owed record: with nothing shipped a stable NFS write, a FUSE or virtio-fs `fsync`, a
  /// WinFsp flush or a map sync waited for nothing and answered durable; with a head it waited one position short, so
  /// once the head was placed the reply was answered with the write it claimed held only in the record still owed.
  #[test]
  fn a_reply_whose_publication_owes_a_whole_record_waits_for_that_record_placed_or_not() {
    // Unplaced, nothing is answerable (`answerable` is `None`); placed, the head the holders hold durably.
    for (placed, answerable_at_head) in [(false, None), (true, Some(4))] {
      assert_eq!(
        owed_position(None, true, false, None),
        Some(0),
        "nothing shipped yet, placed {placed}: the owed record's position is the first"
      );
      assert_eq!(
        owed_position(Some(4), true, false, answerable_at_head),
        Some(5),
        "head 4, placed {placed}: the owed record lands at 5"
      );
      assert_eq!(
        owed_position(Some(4), false, true, answerable_at_head),
        Some(4),
        "fenced, placed {placed}: the head, which the deferral refuses"
      );
    }
    assert_eq!(
      owed_position(Some(4), false, false, None),
      Some(4),
      "nothing owed, unplaced: the head"
    );
    assert_eq!(
      owed_position(Some(4), false, false, Some(4)),
      None,
      "durable and answerable at the head: nothing to wait for"
    );
    assert_eq!(
      owed_position(None, false, false, None),
      None,
      "nothing published and nothing owed: nothing to wait for"
    );
  }

  /// Format: the head a test's takeover adopted: the dead owner's epoch and a position it had made durable.
  const ADOPTED: Head = Head {
    epoch: HostEpoch(1),
    position: 142,
    identity: [9; 32],
  };

  /// A live-shipped volume named `name` made on `state`'s shard through the verbs, with `adopted` recorded as the head
  /// its takeover adopted before it ships anything; then published once ([`published`]). Its log's first entry.
  fn first_entry_of(state: &mut ShardState, name: &str, adopted: Option<Head>) -> Option<Head> {
    let principal = slates_db::catalog::Principal::Uid {
      host: HostId(0),
      uid: 1234,
    };
    let created = crate::verbs::dispatch(
      state,
      1,
      &principal,
      slates_ipc::protocol::RequestBody::Create {
        name: name.to_owned(),
        size: slates_ipc::protocol::SizeClass::Dynamic { max: 1 << 20 },
        names: slates_ipc::protocol::NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    let slates_ipc::protocol::ReplyBody::Created { id } = created else {
      return None;
    };
    let volume = slates_db::catalog::VolumeId { bytes: id.bytes };
    if let Some(head) = adopted {
      adopt_head(state, volume, head);
    }
    let shipped = crate::verbs::dispatch(
      state,
      1,
      &principal,
      slates_ipc::protocol::RequestBody::LiveShip {
        volume: id,
        live_shipped: true,
      },
    );
    if !matches!(
      shipped,
      slates_ipc::protocol::ReplyBody::LiveShippingSet { live_shipped: true }
    ) {
      return None;
    }
    published(state, volume, None);
    state
      .live
      .shipping
      .get(&id.bytes)
      .and_then(|live| live.whole_entry)
  }

  /// A-329 (Continuity, T-8.6: "the successor's first head is past every position durable under an earlier epoch"): do
  /// make two live-shipped volumes on one shard, record for one the head its takeover adopted (the dead owner's epoch 1,
  /// position 142), and publish both. Expect the taken-over volume's first entry numbered 143, past the adopted head,
  /// and the other's at 0, as a new volume's log begins. Before, every shipper started from nothing, so a successor's
  /// first entry was position 0 at genesis, below every position the dead owner had made durable (`live_chaos`: five
  /// takeover flows judged position 1 against 72 to 1,000).
  #[test]
  fn a_successors_first_entry_is_numbered_past_the_head_its_takeover_adopted() {
    let (taken, fresh) = crate::daemon::audit_on_shard(|state| {
      (
        first_entry_of(state, "taken-over", Some(ADOPTED)),
        first_entry_of(state, "made-here", None),
      )
    });
    assert_eq!(
      taken.map(|head| head.position),
      Some(ADOPTED.position + 1),
      "the successor's first entry follows the head it adopted: {taken:?}"
    );
    assert_eq!(
      fresh.map(|head| head.position),
      Some(0),
      "a volume made here begins its log at genesis: {fresh:?}"
    );
  }

  /// A-345, amending A-309 piece 3 (NoLoss through a falsely retired holder): the decision behind the fresh-member flow's
  /// data loss (`crates/cli/tests/live_chaos.rs`, CI diag run 44288273676, Linux x86_64, 2026-11-18). Do: with the owner
  /// gone, recorded holders b (this node, its copy stale at 167) and c (the newest entries, at 170), c retired on a death
  /// verdict while it was stopped and never seen restarted, and the members b, d and e (which never held the log)
  /// answering, decide phase one; let c, back, report 170 and decide again; then decide with c seen restarted instead.
  /// Expect: phase one waits for c rather than adopting without it; once c reports, c's 170 is adopted; and with c seen
  /// restarted (its copy gone with its process, A-309's store-destroyed case) phase one is past tolerance, waits for the
  /// members and adopts the greatest head they hold, b's 167. Before A-345 the first decision was already past tolerance
  /// and adopted b's 167, losing every entry only c held.
  #[test]
  fn a_retired_holder_never_seen_restarted_is_waited_for_and_its_newer_head_adopted() {
    let quorum = slates_db::register::Quorum { f: 1 };
    let [b, c, d, e] = [HostId(2), HostId(3), HostId(4), HostId(5)];
    let holders = [c, b];
    let cohort_hosts = [b, c, d, e];
    let members = [b, d, e];
    let head = |position| {
      Some(Head {
        epoch: HostEpoch(1),
        position,
        identity: [u8::try_from(position % 251).unwrap(); 32],
      })
    };
    let adopted = |chosen: Result<Option<(HostId, Head)>, &'static str>| {
      chosen.map(|adopted| adopted.map(|(holder, head)| (holder, head.position)))
    };
    let without_c = [(b, head(167)), (d, None), (e, None)];
    let with_c = [(b, head(167)), (d, None), (e, None), (c, head(170))];

    let never_seen = BTreeSet::new();
    let wait = past_tolerance_wait(&holders, &cohort_hosts, (&members, &never_seen), b, quorum);
    assert_eq!(
      wait, None,
      "c, retired but never seen restarted, may return: within tolerance"
    );
    assert_eq!(
      adopted(phase_one_choice(
        &without_c,
        &holders,
        wait.as_deref(),
        quorum
      )),
      Err("fleet.live.takeover_waits_quorum"),
      "phase one waits for c rather than adopting b's stale head without it"
    );
    assert_eq!(
      adopted(phase_one_choice(&with_c, &holders, wait.as_deref(), quorum)),
      Ok(Some((c, 170))),
      "once c reports, its newer head is adopted"
    );

    let seen_restarted = BTreeSet::from([c]);
    let wait = past_tolerance_wait(
      &holders,
      &cohort_hosts,
      (&members, &seen_restarted),
      b,
      quorum,
    );
    assert_eq!(
      wait.as_deref(),
      Some(&[d, e][..]),
      "with c's copy gone, past tolerance waits for the members that exist now"
    );
    assert_eq!(
      adopted(phase_one_choice(
        &without_c,
        &holders,
        wait.as_deref(),
        quorum
      )),
      Ok(Some((b, 167))),
      "and adopts the greatest head they hold"
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

  /// Shape: the whole state a resumed owner's scene ships first: a page, so the credit it sets (its own size,
  /// `Bounds::of_whole`) admits the short writes after it.
  const RESUMED_WHOLE_BYTES: usize = 4096;

  /// A live-shipped volume named `name` made on `state`'s shard through the verbs (`Create`, then `LiveShip`).
  fn live_volume_here(state: &mut ShardState, name: &str) -> Option<slates_db::catalog::VolumeId> {
    let principal = slates_db::catalog::Principal::Uid {
      host: HostId(0),
      uid: 1234,
    };
    let created = crate::verbs::dispatch(
      state,
      1,
      &principal,
      slates_ipc::protocol::RequestBody::Create {
        name: name.to_owned(),
        size: slates_ipc::protocol::SizeClass::Dynamic { max: 1 << 20 },
        names: slates_ipc::protocol::NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    let slates_ipc::protocol::ReplyBody::Created { id } = created else {
      return None;
    };
    let shipped = crate::verbs::dispatch(
      state,
      1,
      &principal,
      slates_ipc::protocol::RequestBody::LiveShip {
        volume: id,
        live_shipped: true,
      },
    );
    matches!(
      shipped,
      slates_ipc::protocol::ReplyBody::LiveShippingSet { live_shipped: true }
    )
    .then_some(slates_db::catalog::VolumeId { bytes: id.bytes })
  }

  /// A-344's warm restart as the owner's log stands when its holders come back (A-428): a live volume made here, its log a
  /// shipper resumed past position 5 that ships to two holders at `f = 1` and has heard from neither, the catalog placed
  /// at both, its first whole entry at 6 sealed under no candidate's key (its pairs not yet delivered again) and `writes`
  /// entries after it. With `trimmed`, the retained tail is cut to its newest entry, as an owner writing past the retain
  /// bound leaves it. Then both holders return ([`reached_volume`]). What the owner sends them now ([`pending_sends`]):
  /// each install's holder and the position of the whole state it carries; `None` when the scene could not be made.
  fn installs_after_a_resumed_owners_holders_return(
    state: &mut ShardState,
    name: &str,
    writes: usize,
    trimmed: bool,
  ) -> Option<Vec<(HostId, Option<u64>)>> {
    let id = live_volume_here(state, name)?;
    let local = state.fleet.configuration().owner;
    let holders = [
      HostId(local.0.wrapping_add(1)),
      HostId(local.0.wrapping_add(2)),
    ];
    let shipper = Shipper::resumed(
      (local, ObjectId(id.bytes), HostEpoch(1)),
      slates_db::register::Quorum { f: 1 },
      &holders,
      Some(5),
      Bounds::default(),
    )
    .ok()?;
    let mut live = new_live_volume(shipper);
    live.catalogued = holders.to_vec();
    state.live.shipping.insert(id.bytes, live);
    let whole = vec![b'w'; RESUMED_WHOLE_BYTES];
    if !append_payload(state, id, (true, false), whole, Some(Vec::new())) {
      return None;
    }
    for write in 0..writes {
      let delta = format!("write {write}").into_bytes();
      if !append_payload(state, id, (false, false), delta, None) {
        return None;
      }
    }
    if trimmed {
      let live = state.live.shipping.get_mut(&id.bytes)?;
      live.shipper.set_bounds(Bounds {
        credit: RESUMED_WHOLE_BYTES.saturating_mul(2),
        retain: 0,
      });
    }
    for holder in holders {
      reached_volume(state, id.bytes, holder);
    }
    Some(
      pending_sends(state, id.bytes)
        .into_iter()
        .filter(|send| send.stream == slates_cluster::live::LIVE_STATE_STREAM)
        .map(|send| {
          let at = Install::decode(&send.bytes, usize::MAX)
            .ok()
            .map(|install| install.head.position);
          (send.holder, at)
        })
        .collect(),
    )
  }

  /// A-428 (§4.10 live shipping; A-344's warm restart, the regression main bisected to A-376): do make a resumed owner's
  /// log whose first whole entry, at 6, is followed by one write at 7 that both holders still lack, let both holders
  /// return, and ask what the owner sends them; then the same past two writes with the retained tail cut to its newest
  /// entry. Expect each holder sent an install of the whole state at 6 in the first case, since it takes 7 from the tail
  /// after it; and no install in the second, where the whole state at 6 is behind the tail, so the holders wait for an
  /// image made at the log's head, as before. Before, the first case sent nothing either: the holders waited for an image
  /// that no period made while a whole record was owed to cover the candidates' keys, and that record waited for credit
  /// only their installs could free; after an owner's warm restart every write was refused `DurabilityPending` at the
  /// held-reply bound (main's and g230's runs of the fleet test: `owes whole true; holders [..@None, ..@None]; lanes [];
  /// covers candidates false`).
  #[test]
  fn a_returned_holder_owed_an_install_is_sent_the_last_whole_state_while_the_tail_reaches_past_it()
  {
    let (reaches, behind) = crate::daemon::audit_on_shard(|state| {
      (
        installs_after_a_resumed_owners_holders_return(state, "resumed-reaches", 1, false),
        installs_after_a_resumed_owners_holders_return(state, "resumed-behind", 2, true),
      )
    });
    let reaches = reaches.expect("the first scene was made");
    assert_eq!(
      reaches.len(),
      2,
      "each returned holder is sent an install: {reaches:?}"
    );
    assert!(
      reaches.iter().all(|(_, at)| *at == Some(6)),
      "each install carries the whole state at 6, whose following write the tail still holds: {reaches:?}"
    );
    assert_eq!(
      behind,
      Some(Vec::new()),
      "a whole state behind the tail is not sent: the holders wait for an image at the log's head"
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
    let refused = |why| Refused {
      head: refused_at,
      why,
      since: 0,
      sealing: None,
      handed: None,
    };
    for kind in [
      REFUSED_OPEN,
      REFUSED_RECORD,
      REFUSED_APPLY,
      REFUSED_NOT_WHOLE_FIRST,
      REFUSED_LAYOUT,
    ] {
      assert_eq!(
        next_fetch(Some(&refused(kind)), refused_at, 0, false),
        NextFetch::WaitsNewHead,
        "{kind} at the same head is refused again by the same bytes"
      );
      assert_eq!(
        next_fetch(Some(&refused(kind)), later, 0, false),
        NextFetch::Fetch,
        "{kind} at another head is fetched"
      );
    }
    assert_eq!(
      next_fetch(Some(&refused(REFUSED_LINEAGE)), refused_at, 0, false),
      NextFetch::Fetch,
      "a lineage refusal is this node's own record, retried"
    );
    assert_eq!(
      next_fetch(None, refused_at, 0, false),
      NextFetch::Fetch,
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
        work_of: None,
        write: None,
        write_version: 0,
        signers: Vec::new(),
      }
      .to_record_bytes()
    };
    assert!(names_holder(&value(Some(vec![3, 7])), local));
    assert!(!names_holder(&value(Some(vec![3, 9])), local));
    assert!(!names_holder(&value(None), local));
    assert!(!names_holder(&[0xff, 1, 2], local));
  }

  /// A-271: do ask whether a rebuild may open a log's payloads, sealed or not, with and without keys recorded here.
  /// Expect a sealed log without keys refused `no_key`, the rest let through. Before, the rebuild made a key and was
  /// refused `open` at the first payload, final for the head.
  #[test]
  fn a_sealed_log_this_node_holds_no_key_for_is_refused_for_the_missing_key() {
    assert_eq!(
      keys_for_rebuild(true, false),
      Err(REFUSED_NO_KEY),
      "sealed, no keys here"
    );
    assert_eq!(
      keys_for_rebuild(true, true),
      Ok(()),
      "sealed, keys here: opened"
    );
    assert_eq!(
      keys_for_rebuild(false, false),
      Ok(()),
      "in the clear: no key needed"
    );
  }

  /// A-271: a refusal for a missing key waits for the key, an event, not a timer re-fetch. Do ask what a successor does
  /// at the refused head with no key yet, once the key has come, past the bound, and at a later head. Expect it to wait
  /// for the key without fetching, fetch once the key is here, answer `no_key_exhausted` past
  /// [`NO_KEY_WAIT_NS`], and fetch at a later head. Before, the refusal (counted `open`) waited for a new head at once
  /// for good; retried each period instead, it fetched the whole log 104 to 204 times a run.
  #[test]
  fn a_missing_key_is_waited_for_and_the_log_fetched_once_it_comes() {
    let at = Head {
      epoch: slates_db::register::HostEpoch(1),
      position: 1_056,
      identity: [3; 32],
    };
    let later = Head {
      position: 1_057,
      ..at
    };
    let since = 5_000;
    let refused = Refused {
      head: at,
      why: REFUSED_NO_KEY,
      since,
      sealing: None,
      handed: None,
    };
    let inside = since + NO_KEY_WAIT_NS - 1;
    assert_eq!(
      next_fetch(Some(&refused), at, inside, false),
      NextFetch::WaitsKey,
      "no key yet: no fetch"
    );
    assert_eq!(
      next_fetch(Some(&refused), at, inside, true),
      NextFetch::Fetch,
      "the key has come: fetched"
    );
    assert_eq!(
      next_fetch(Some(&refused), at, since + NO_KEY_WAIT_NS, false),
      NextFetch::KeyExhausted,
      "past the bound: typed"
    );
    assert_eq!(
      next_fetch(Some(&refused), later, inside, false),
      NextFetch::Fetch,
      "a later head: fetched"
    );
  }

  /// A-271: a pair accepted by a node's predecessor at the same anchor does not cover the node. Do ask whether a
  /// candidate is covered when this shard's pair to its anchor was accepted by an earlier member id at that anchor, by
  /// the candidate itself, or by no one. Expect only its own acceptance to cover it. Before, the anchor alone counted:
  /// a node replaced at its anchor (store destroyed, new member id) was taken for covered, writes went durable on a key
  /// it never received, and as successor it could not open the log (2026-11-09: no pair recorded, current or retired, in
  /// 5 of 5 failing runs).
  #[test]
  fn a_pair_accepted_by_a_predecessor_at_the_same_anchor_does_not_cover_its_replacement() {
    let (predecessor, replacement, anchor) = (HostId(11), HostId(22), 7_u64);
    let anchors = BTreeMap::from([(replacement, anchor)]);
    assert_eq!(
      covered_anchor(
        replacement,
        &anchors,
        &BTreeMap::from([(anchor, predecessor)])
      ),
      None,
      "the predecessor accepted: not covered"
    );
    assert_eq!(
      covered_anchor(
        replacement,
        &anchors,
        &BTreeMap::from([(anchor, replacement)])
      ),
      Some(anchor),
      "its own acceptance: covered"
    );
    assert_eq!(
      covered_anchor(replacement, &anchors, &BTreeMap::new()),
      None,
      "no pair: not covered"
    );
    assert_eq!(
      covered_anchor(
        HostId(33),
        &anchors,
        &BTreeMap::from([(anchor, replacement)])
      ),
      None,
      "no anchor known"
    );
  }

  /// A-271's retention invariant: a holder's log begins at the newest whole entry it holds, so a successor reads the
  /// newest entry's key from the first payload it fetches. Do keep two deltas, then a whole entry, then a delta. Expect
  /// the whole entry first and the earlier deltas gone. If this ever changes, a fetch must read the newest whole entry
  /// that names this node, not the first payload (A-271's named fallback).
  #[test]
  fn a_holders_log_begins_at_the_newest_whole_entry_it_holds() {
    let entries = crate::daemon::audit_on_shard(|state| {
      let mut held = Held::for_test(HostId(1), ObjectId([4; 16]), 0);
      let mut room = Room {
        store: &mut state.store,
        claimed: 1 << 20,
      };
      for payload in [
        vec![KIND_DELTA, 1],
        vec![KIND_DELTA, 2],
        vec![KIND_WHOLE, 3],
        vec![KIND_DELTA, 4],
      ] {
        let whole = payload.first() == Some(&KIND_WHOLE);
        keep(&mut held, &mut room, &payload, 1 << 20, whole).unwrap();
      }
      held.entries.iter().cloned().collect::<Vec<_>>()
    });
    assert_eq!(
      entries,
      vec![vec![KIND_WHOLE, 3], vec![KIND_DELTA, 4]],
      "the newest whole entry first, what came before it dropped"
    );
  }

  /// §4.2 (A-401): an owner's admission asks are judged by each candidate's answer. Any `NoRoom` refuses the live-ship
  /// with the least room named; otherwise the candidates that gave no admission answer are counted, and they make
  /// their admission at the log's first entry. Do judge no candidates, every one admitted, one silent, one answering
  /// nothing it can act on, two refusing, one refusing beside one admitted, and a refusal from a host not asked.
  /// Expect admitted with nothing unanswered, the same, one unanswered twice, the least room, that one's room, and the
  /// stranger ignored.
  #[test]
  fn an_owners_admission_asks_are_judged_by_the_least_room_and_the_unanswered() {
    let (b, c) = (HostId(2), HostId(3));
    let admitted = encode_reply(&Ok(None));
    let no_room = |available| encode_reply(&Err(LiveError::NoRoom { available }));
    assert_eq!(
      admission_verdict(&[], &[]),
      Asked::Admitted { unanswered: 0 }
    );
    assert_eq!(
      admission_verdict(&[b, c], &[(b, admitted.clone()), (c, admitted.clone())]),
      Asked::Admitted { unanswered: 0 }
    );
    assert_eq!(
      admission_verdict(&[b, c], &[(b, admitted.clone())]),
      Asked::Admitted { unanswered: 1 }
    );
    assert_eq!(
      admission_verdict(&[b, c], &[(b, admitted.clone()), (c, Vec::new())]),
      Asked::Admitted { unanswered: 1 }
    );
    assert_eq!(
      admission_verdict(&[b, c], &[(b, no_room(7)), (c, no_room(3))]),
      Asked::NoRoom(3)
    );
    assert_eq!(
      admission_verdict(&[b, c], &[(b, admitted), (c, no_room(9))]),
      Asked::NoRoom(9)
    );
    assert_eq!(
      admission_verdict(&[b], &[(c, no_room(1))]),
      Asked::Admitted { unanswered: 1 }
    );
  }

  /// Shape: the bytes of each payload the charge tests keep.
  const CHARGE_PAYLOAD: usize = 4096;

  /// §4.2 (g229): a held log is charged against the claim its candidate reserved, and only its bytes past the claim are
  /// charged on the control shard. Do: with a claim of three payloads, keep five deltas, then a whole entry of one
  /// payload, then let the copy's charge go to nothing past a claim of five. Expect the shard's replicated charge to read
  /// nothing while the log is within the claim, two payloads at five, nothing again once the whole entry replaced the
  /// log, and the copy's own charge to match the shard's each time.
  #[test]
  fn a_held_log_is_charged_only_past_the_claim_its_candidate_reserved() {
    let readings = crate::daemon::audit_on_shard(|state| {
      let before = state.store.budget.replicated();
      let mut held = Held::for_test(HostId(1), ObjectId([5; 16]), 0);
      let mut readings = Vec::new();
      let payload = |kind: u8| {
        let mut bytes = vec![0u8; CHARGE_PAYLOAD];
        if let Some(first) = bytes.first_mut() {
          *first = kind;
        }
        bytes
      };
      let claimed = u64::try_from(3 * CHARGE_PAYLOAD).unwrap();
      for _ in 0..5 {
        let mut room = Room {
          store: &mut state.store,
          claimed,
        };
        keep(&mut held, &mut room, &payload(KIND_DELTA), 1 << 20, false).unwrap();
        readings.push((
          state.store.budget.replicated().saturating_sub(before),
          held.charged,
        ));
      }
      let mut room = Room {
        store: &mut state.store,
        claimed,
      };
      keep(&mut held, &mut room, &payload(KIND_WHOLE), 1 << 20, true).unwrap();
      readings.push((
        state.store.budget.replicated().saturating_sub(before),
        held.charged,
      ));
      readings
    });
    let page = u64::try_from(CHARGE_PAYLOAD).unwrap();
    assert_eq!(
      readings,
      vec![
        (0, 0),
        (0, 0),
        (0, 0),
        (page, page),
        (2 * page, 2 * page),
        (0, 0)
      ],
      "charged past the claim alone, and credited back as the whole entry replaced the log"
    );
  }

  /// §4.2 (g229): a holder whose shard has no room for the bytes of its log past the claim refuses the entry `NoRoom`,
  /// naming what it could admit, and keeps its copy as it was (guard-then-apply). Do: keep one delta with no claim, then
  /// reserve every byte the shard could admit, then keep a second. Expect the second refused `NoRoom`, the copy still
  /// holding the first alone at its charge.
  #[test]
  fn a_holder_with_no_room_for_its_log_refuses_the_entry_and_keeps_its_copy() {
    let (refused, entries, charged, kept) = crate::daemon::audit_on_shard(|state| {
      let mut held = Held::for_test(HostId(1), ObjectId([6; 16]), 0);
      let delta = vec![KIND_DELTA; CHARGE_PAYLOAD];
      let mut room = Room {
        store: &mut state.store,
        claimed: 0,
      };
      keep(&mut held, &mut room, &delta, 1 << 20, false).unwrap();
      let all = state.store.admittable();
      let _filled = state
        .store
        .reserve(all)
        .expect("everything admittable reserved");
      let mut room = Room {
        store: &mut state.store,
        claimed: 0,
      };
      let refused = keep(&mut held, &mut room, &delta, 1 << 20, false);
      (refused, held.entries.len(), held.charged, held.bytes)
    });
    assert!(
      matches!(refused, Err(LiveError::NoRoom { .. })),
      "refused for room: {refused:?}"
    );
    assert_eq!(
      (entries, charged, kept),
      (1, u64::try_from(CHARGE_PAYLOAD).unwrap(), CHARGE_PAYLOAD),
      "the copy as it was"
    );
  }

  /// g251 (the nemesis's control-shard poll of 557 ms beside a 406 MB log): a takeover fetch's answer laid out an entry a
  /// turn is the answer laid out at once. Do keep a copy of six entries (a whole one, then deltas) at a head, fence it at
  /// epoch 2, and lay its answer out at a slice of nothing and at a slice of everything; then begin again and, between
  /// two turns, raise the fence past the epoch, and once more let the copy go. Expect six turns and one, the same bytes,
  /// read back in place in order at the head held; the raised fence's answer its stale-epoch refusal; and the let-go
  /// copy's the empty head, without entries, which a successor finds is not the head it adopted.
  #[test]
  fn a_fetch_answered_an_entry_a_turn_is_the_answer_laid_out_at_once() {
    crate::daemon::audit_on_shard(|s| {
      let object = ObjectId([7; 16]);
      let head = Head {
        epoch: HostEpoch(1),
        position: 5,
        identity: [3; 32],
      };
      let payloads: Vec<Vec<u8>> = (0..6u8)
        .map(|n| vec![if n == 0 { KIND_WHOLE } else { KIND_DELTA }, n, n])
        .collect();
      let mut held = Held::for_test(HostId(1), object, 0);
      let mut room = Room {
        store: &mut s.store,
        claimed: 1 << 20,
      };
      for payload in &payloads {
        let whole = payload.first() == Some(&KIND_WHOLE);
        keep(&mut held, &mut room, payload, 1 << 20, whole).unwrap();
      }
      held.replica.commit(HostEpoch(1), head);
      held.replica.prepare(HostEpoch(2)).unwrap();
      s.live.held.insert(object, held);
      let laid_out = |s: &ShardState, slice_bytes: usize| {
        let mut answer = FetchAnswer::of(s.live.held.get(&object).unwrap(), object, HostEpoch(2));
        let mut turns = 0;
        while !answer.lay_out(s, slice_bytes) {
          turns += 1;
        }
        (turns + 1, answer.into_reply())
      };
      let (entry_a_turn, by_entry) = laid_out(s, 0);
      let (at_once, whole) = laid_out(s, usize::MAX);
      assert_eq!((entry_a_turn, at_once), (payloads.len(), 1));
      assert_eq!(by_entry, whole, "the same answer");
      let (answered, mut log) = slates_db::live::FetchedLog::decode(by_entry, 1 << 20).unwrap();
      assert_eq!(answered, Some(head));
      let read: Vec<Vec<u8>> =
        std::iter::from_fn(|| log.next_payload().map(|payload| payload.unwrap().to_vec()))
          .collect();
      assert_eq!(read, payloads, "every entry, in order");
      let mut raised = FetchAnswer::of(s.live.held.get(&object).unwrap(), object, HostEpoch(2));
      assert!(!raised.lay_out(s, 0));
      s.live
        .held
        .get_mut(&object)
        .unwrap()
        .replica
        .prepare(HostEpoch(3))
        .unwrap();
      assert!(raised.lay_out(s, 0), "a moved copy ends the answer");
      assert_eq!(
        slates_db::live::FetchedLog::decode(raised.into_reply(), 1 << 20).map(|(head, _)| head),
        Err(LiveError::StaleEpoch {
          current: HostEpoch(3)
        })
      );
      let mut gone = FetchAnswer::of(s.live.held.get(&object).unwrap(), object, HostEpoch(3));
      assert!(!gone.lay_out(s, 0));
      s.live.held.remove(&object);
      assert!(gone.lay_out(s, 0));
      let (answered, mut log) =
        slates_db::live::FetchedLog::decode(gone.into_reply(), 1 << 20).unwrap();
      assert_eq!(answered, None, "the let-go copy's empty head");
      assert!(log.next_payload().is_none(), "and no entries");
    });
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
