//! The control shard's membership task (A-67 H-2b; `docs/wip/transport-quic.md` §6): it drives the node's one
//! failure detector and the sealed datagram plane it probes over (`slates_cluster::member_plane`). It owns the plane's
//! UDP socket, the probe port of the node's port block, and replaces the per-peer probe tasks that held one detector
//! per peer over a QUIC probe session each.
//!
//! **Epochs.** A pair's plane keys come from its **canonical** connection: the record session the member with the
//! lower anchor dials. Its dialer announces the epoch, a per-peer counter that only rises, together with its member id
//! and boot nonce, as one record-plane exchange ([`PLANE_EPOCH_STREAM`]):
//! - the acceptor validates that identity against the session certificate's anchor (learn-on-contact), installs the
//!   epoch, and answers with its own identity;
//! - the dialer validates that answer, then installs the epoch too.
//!
//! So neither end seals under keys the other cannot open, and a restarted peer's new member id is learned on its
//! authenticated record session before any of its datagrams is opened. Every other record session announces its
//! dialer's identity too, with no epoch ([`IDENTITY_ONLY`]), so learn-on-contact validates whoever dials, refusing and
//! counting a forged identity, whatever the pair's keying. The secret is the session's TLS exporter
//! under hyper-datagram's label (RFC 8446 §7.5): a new connection always brings new keys.
//!
//! **What the fleet decides** ([`StateFleet`]), as the probe sessions did:
//! - learn-on-contact for every announced identity;
//! - the owner lease's holder side, recorded with each answer;
//! - the deaf-peer test fault;
//! - relays only for a target kept in direct contact.
//!
//! **What it folds:**
//! - each acknowledgement's round trip into the path estimate;
//! - its standing into the owner lease;
//! - its first arrival into the formed mesh;
//! - the detector's view into every shard's `FleetNode`, which stays the authority: retirement goes through the
//!   council (D-14).

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use hyper_datagram::{EXPORTER_LABEL, ExporterSecret, Role};
use rustls::pki_types::CertificateDer;
use slates_cluster::member_plane::{Fleet, MemberPlane, PlaneEvent};
use slates_cluster::membership::MemberState;
use slates_db::register::HostId;
use slates_rt::error::ReceiveRefusal;
use slates_rt::futures;
use slates_rt::shard::Kept;
use slates_rt::udp::{SocketAddrV4, UdpSocket};
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;

use crate::deploy::{NodeAddress, Plane};
use crate::dns::Resolver;
use crate::state::{self, ShardState};

/// Format: the record-plane stream the canonical dialer announces its plane epoch on: the kind after the fleet's
/// sixteen (`fleet.rs`, `takeover.rs`, `discovery.rs`, `owner_location.rs`, `merge_service.rs`).
pub(crate) const PLANE_EPOCH_STREAM: u64 = 17;

/// Format: the epoch of an announcement that carries identity only: a record session that is not its pair's canonical
/// one announces who dialed (learn-on-contact) and keys nothing. Plane epochs start at one.
const IDENTITY_ONLY: u32 = 0;

/// Format: an announcement's epoch field, a little-endian `u32`.
const EPOCH_BYTES: usize = size_of::<u32>();
/// Format: an announcement's member id and boot nonce fields, each a little-endian `u64`.
const WORD_BYTES: usize = size_of::<u64>();
/// Format: an announcement's bytes: the epoch, the announcer's member id and its boot nonce.
const ANNOUNCEMENT_BYTES: usize = EPOCH_BYTES + WORD_BYTES + WORD_BYTES;

/// Shape: the largest datagram the plane's socket reads: UDP's largest payload, so no datagram is truncated whatever
/// a peer's path allows (hyper-datagram `MAX_DATAGRAM_BYTES`).
const RECEIVE_BYTES: usize = hyper_datagram::MAX_DATAGRAM_BYTES;

/// Refusal counters (`slates status`).
const EPOCH_REFUSED: &str = "fleet.plane.epoch_refused";
/// Counter: an epoch announcement that was not one (bytes that do not decode).
const EPOCH_REFUSED_MALFORMED: &str = "fleet.plane.epoch_refused.malformed";
/// Counter: an announced identity the session's anchor and boot nonce do not derive.
const EPOCH_REFUSED_FORGED: &str = "fleet.plane.epoch_refused.forged";
/// Counter: an epoch announced on a session that is not the pair's canonical one (this node the lower anchor).
const EPOCH_REFUSED_NOT_CANONICAL: &str = "fleet.plane.epoch_refused.not_canonical";
/// Counter: an epoch the plane would not install (not newer than the peer's last, or the peer table full).
const EPOCH_REFUSED_INSTALL: &str = "fleet.plane.epoch_refused.install";
/// Counter: an announced epoch not newer than the peer's last, taken as the start of its restarted sequence.
const EPOCH_RESTARTED: &str = "fleet.plane.epoch_restarted";
/// Counter: an acceptor's answer that was empty or named another epoch.
const EPOCH_REFUSED_ANSWER: &str = "fleet.plane.epoch_refused.answer";
const EPOCH_UNANSWERED: &str = "fleet.plane.epoch_unanswered";
const SEND_REFUSED: &str = "fleet.plane.send_refused";
/// Refused receives the plane read past, by kind (`slates_rt::error::ReceiveRefusal`), and the one that ended it (the
/// socket gone).
const RECEIVE_REFUSED_EARLIER_SEND: &str = "fleet.plane.receive_refused.earlier_send";
const RECEIVE_REFUSED_RESOURCES: &str = "fleet.plane.receive_refused.resources";
const RECEIVE_REFUSED_OTHER: &str = "fleet.plane.receive_refused.other";
const RECEIVE_SOCKET_GONE: &str = "fleet.plane.receive_socket_gone";
const ADDRESS_UNRESOLVED: &str = "fleet.plane.address_unresolved";
const FOLD_REFUSED: &str = "fleet.plane.fold_refused";
/// Event counters (`slates status`): record sessions lent to the announcement pass, and announcements sent on them. A
/// session is lent only when its connection owes an announcement, so the two move together; lends outrunning sends is
/// the pass borrowing sessions that owe nothing.
const EPOCH_LENT: &str = "fleet.plane.announce_lends";
const EPOCH_SENT: &str = "fleet.plane.announcements_sent";

/// One side's identity in an epoch exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Announcement {
  /// The epoch the dialer announces (echoed by the acceptor).
  pub epoch: u32,
  /// The announcer's member id.
  pub member: HostId,
  /// The announcer's boot nonce, which its member id is validated against.
  pub boot_nonce: u64,
}

impl Announcement {
  /// The announcement's wire bytes.
  pub(crate) fn encode(&self) -> [u8; ANNOUNCEMENT_BYTES] {
    let mut bytes = [0u8; ANNOUNCEMENT_BYTES];
    let (epoch, rest) = bytes.split_at_mut(EPOCH_BYTES);
    let (member, boot) = rest.split_at_mut(WORD_BYTES);
    epoch.copy_from_slice(&self.epoch.to_le_bytes());
    member.copy_from_slice(&self.member.0.to_le_bytes());
    boot.copy_from_slice(&self.boot_nonce.to_le_bytes());
    bytes
  }

  /// An announcement from exactly [`ANNOUNCEMENT_BYTES`] bytes, or `None`.
  pub(crate) fn decode(bytes: &[u8]) -> Option<Announcement> {
    let bytes: &[u8; ANNOUNCEMENT_BYTES] = bytes.try_into().ok()?;
    let (epoch, rest) = bytes.split_first_chunk::<EPOCH_BYTES>()?;
    let (member, boot) = rest.split_first_chunk::<WORD_BYTES>()?;
    let boot: &[u8; WORD_BYTES] = boot.try_into().ok()?;
    Some(Announcement {
      epoch: u32::from_le_bytes(*epoch),
      member: HostId(u64::from_le_bytes(*member)),
      boot_nonce: u64::from_le_bytes(*boot),
    })
  }
}

/// A peer's plane address, as the fleet's peers name it.
#[derive(Clone, Debug)]
pub(crate) struct PlanePeer {
  /// Its seed member id: what its record session is keyed by before it announces itself.
  pub seed: HostId,
  /// Its probe-plane address from the manifest (or discovery).
  pub address: NodeAddress,
  /// The certificate its sessions authenticate with: what discovery's addresses are keyed by.
  pub certificate: CertificateDer<'static>,
  /// The address the plane last resolved for it.
  pub resolved: Option<SocketAddrV4>,
  /// Whether `resolved` is owed a fresh resolution (a new connection, or a suspicion): it is still used until the
  /// fresh one succeeds, so re-resolving never drops a datagram (`forget_address`).
  pub stale: bool,
  /// Whether its seed id has been retired: once its real member id is learned, the manifest's placeholder is folded
  /// dead, once.
  pub seed_retired: bool,
}

/// The plane's bookkeeping on the control shard.
#[derive(Default)]
pub(crate) struct PlaneState {
  /// The detector and the sealed plane, once the membership task started.
  pub plane: Option<MemberPlane>,
  /// Per peer anchor, its plane address.
  pub peers: BTreeMap<HostId, PlanePeer>,
  /// Per peer anchor, the last epoch this node announced as a canonical dialer.
  pub announced: BTreeMap<HostId, u32>,
  /// Per record session (by the member it is keyed under), the connection this node last announced on: each connection
  /// announces once, its identity and, if canonical, the pair's epoch.
  pub announced_on: BTreeMap<HostId, slates_transport::endpoint::ConnectionId>,
  /// Per member, the liveness last folded into the fleet: a member whose state is unchanged costs no fold.
  pub folded: BTreeMap<HostId, MemberState>,
  /// Members keyed by an installed epoch, and whether each has been joined to the detector: a member joins once it is
  /// also addressable (`MemberPlane::join`).
  pub keyed: BTreeMap<HostId, bool>,
  /// Per member, a fingerprint of the session it last announced itself on to this node (the first bytes of that
  /// session's exporter secret, which differs per connection): a member announcing on another session reconnected, its
  /// process perhaps restarted under the same id ([`note_reconnected`]). Bounded by the fleet's peer capacity.
  pub announced_from: BTreeMap<HostId, [u8; RECONNECT_FINGERPRINT_BYTES]>,
  /// Members that reconnected, whose pair this node (their canonical dialer) announces again on the session it holds
  /// ([`note_reconnected`]): answered, the pair is keyed afresh; unanswered, that session led to a process that is gone
  /// and is dropped so the link dials anew. Bounded by the fleet's peer capacity, as `announced_from` is.
  pub reannouncing: std::collections::BTreeSet<HostId>,
}

/// Format: the bytes of a session's exporter secret kept to tell one connection from the next: enough that two
/// connections' secrets, uniformly random, coincide with probability 2^-128.
pub(crate) const RECONNECT_FINGERPRINT_BYTES: usize = 16;

impl std::fmt::Debug for PlaneState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PlaneState")
      .field("peers", &self.peers.len())
      .field("announced", &self.announced)
      .finish_non_exhaustive()
  }
}

/// The anchor a member id belongs to: the anchor whose learned id it is, or whose seed id it is (`FleetPeer::host`,
/// what its record session is keyed by before it announces itself).
pub(crate) fn anchor_of(state: &ShardState, member: HostId) -> Option<HostId> {
  state
    .learned_members
    .iter()
    .find(|(_, learned)| learned.host == member)
    .map(|(anchor, _)| *anchor)
    .or_else(|| {
      state
        .plane
        .peers
        .iter()
        .find(|(_, peer)| peer.seed == member)
        .map(|(anchor, _)| *anchor)
    })
}

/// The acceptor side of an epoch exchange, from the record serve loop: the session authenticated `anchor`, and
/// `secret` is its exported plane secret. The announced identity must be the one `anchor` and the boot nonce derive
/// (learn-on-contact). This node must be the higher anchor (the canonical rule, so one epoch sequence keys a pair).
/// Then the epoch is installed and this node's identity answered. Refused, the answer is empty and counted.
pub(crate) fn accept_announcement(
  state: &mut ShardState,
  anchor: HostId,
  secret: &[u8; hyper_datagram::SECRET_BYTES],
  request: &[u8],
) -> Vec<u8> {
  let refuse = |state: &mut ShardState, why: &'static str| {
    crate::fleet::count_refusal_in(state, EPOCH_REFUSED);
    crate::fleet::count_refusal_in(state, why);
    Vec::new()
  };
  let Some(announced) = Announcement::decode(request) else {
    return refuse(state, EPOCH_REFUSED_MALFORMED);
  };
  // Every dialer's identity is learned on contact (or refused, counted as forged), whatever the session keys.
  if crate::fleet::learn_member(state, anchor, announced.boot_nonce, announced.member)
    == crate::fleet::LearnedOutcome::Forged
  {
    return refuse(state, EPOCH_REFUSED_FORGED);
  }
  note_returned(state, announced.member);
  note_reconnected(state, anchor, announced.member, secret);
  let answer = Announcement {
    epoch: announced.epoch,
    member: state.fleet.host(),
    boot_nonce: state.member_boot_nonce,
  };
  // Epoch zero announces identity only: a session that is not the pair's canonical one keys nothing.
  if announced.epoch == IDENTITY_ONLY {
    return answer.encode().to_vec();
  }
  if anchor >= state.origin_anchor {
    return refuse(state, EPOCH_REFUSED_NOT_CANONICAL);
  }
  let exported = ExporterSecret::new(*secret);
  let mut installed =
    state.plane.plane.as_mut().map(|plane| {
      plane.install_epoch(announced.member, announced.epoch, &exported, Role::Acceptor)
    });
  // The pair's canonical dialer announces once per connection, each epoch above the last, so an authenticated
  // announcement that is not newer comes from a dialer whose sequence started over: its process restarted under the
  // same member id (an anchor's warm restart keeps the id). Refused, the pair stayed unkeyed for good: the restarted
  // daemon held no probed peer, and the acceptor refused its epochs 344 times in 40 s
  // (`a_candidates_daemon_succeeded_under_its_anchor_rejoins_the_fleet` in `crates/cli/tests/live_chaos.rs`). The
  // sequence starts over here too; its keys come from this session's exporter secret.
  if matches!(
    installed,
    Some(Err(
      slates_cluster::member_plane::PlaneRefusal::EpochNotNewer
    ))
  ) {
    crate::fleet::count_refusal_in(state, EPOCH_RESTARTED);
    installed = state.plane.plane.as_mut().map(|plane| {
      plane.restart_epochs(announced.member, announced.epoch, &exported, Role::Acceptor)
    });
  }
  if !matches!(installed, Some(Ok(()))) {
    return refuse(state, EPOCH_REFUSED_INSTALL);
  }
  state.plane.keyed.insert(announced.member, false);
  forget_address(state, anchor);
  answer.encode().to_vec()
}

/// Notes that `member`, which this node holds dead, has just announced itself over an authenticated session: its process
/// is alive under the same id (an anchor's warm restart), so contact with it is kept (`fleet::direct_contact`) until its
/// probes refute the death. Without it, the pair's canonical dialer, holding the member dead, never dialed it again, the
/// pair's plane was never keyed, and the member could never hear of its death to refute it: the restarted daemon held
/// no probed peer for the whole 40 s wait (`a_candidates_daemon_succeeded_under_its_anchor_rejoins_the_fleet`).
fn note_returned(state: &mut ShardState, member: HostId) {
  let Some(belief) = state.fleet.membership().state(member) else {
    return;
  };
  if belief.liveness != slates_cluster::membership::Liveness::Dead {
    state.returned_members.remove(&member);
    return;
  }
  if state.returned_members.contains_key(&member)
    || state.returned_members.len() < state.config.fleet_peer_capacity
  {
    state.returned_members.insert(member, belief.incarnation);
    crate::fleet::count_refusal_in(state, MEMBER_RETURNED);
  }
}

/// Notes that `member` (of `anchor`) announced itself on `secret`'s session. Announced before on another session, it
/// reconnected: a redial of a live process, or a process restarted under the same id within the moment it took the
/// others to suspect it (5 of 6 anchored rejoins under load on macOS CI, diag run 42342444105, successor up in 50 to
/// 90 ms). Where this node is the pair's canonical dialer (the lower anchor) it announces the pair's next epoch again
/// on the session it holds ([`announce_on`]): a live process answers and the pair is keyed afresh; a session to a
/// process that is gone goes unanswered and only then is dropped, so the link dials the new one. Dropping the session
/// at every reconnect (f31f6b21) tore down a live pair's keyed session each time its member's record link redialed
/// under load: a five-node fleet's node held 3 of 4 peers probed for a whole fleet wait with
/// `fleet.plane.peer_reconnected` 5 (raft-chaos, CI diag-raft-chaos-final-37).
fn note_reconnected(
  state: &mut ShardState,
  anchor: HostId,
  member: HostId,
  secret: &[u8; hyper_datagram::SECRET_BYTES],
) {
  let mut fingerprint = [0u8; RECONNECT_FINGERPRINT_BYTES];
  for (kept, byte) in fingerprint.iter_mut().zip(secret.iter()) {
    *kept = *byte;
  }
  if !state.plane.announced_from.contains_key(&member)
    && state.plane.announced_from.len() >= state.config.fleet_peer_capacity
  {
    return;
  }
  let previous = state.plane.announced_from.insert(member, fingerprint);
  let reconnected = previous.is_some_and(|previous| previous != fingerprint);
  if reconnected && anchor > state.origin_anchor {
    state.plane.announced_on.remove(&member);
    state.plane.reannouncing.insert(member);
    crate::fleet::count_refusal_in(state, PEER_RECONNECTED);
  }
}

/// Counter: a member that announced itself on a new session, whose pair this node (its canonical dialer) announces again.
const PEER_RECONNECTED: &str = "fleet.plane.peer_reconnected";

/// Counter: a reconnected member's session this node held went unanswered at its re-announcement and was dropped to be
/// dialed afresh.
const PEER_REDIALED: &str = "fleet.plane.peer_redialed";

/// Counter: an announcement from a member this node held dead, whose contact is kept so it can refute the death.
const MEMBER_RETURNED: &str = "fleet.plane.member_returned";

/// Marks `anchor`'s resolved plane address stale so the next turn resolves it afresh: a new connection may come from a
/// new address (a replaced pod keeps its name and certificate, not its IP). The address stays the manifest's or
/// discovery's, never a datagram's source, which only path validation could vouch for. The old address is kept until
/// the fresh one resolves: clearing it dropped the datagrams sealed in between, and a lost first probe wedged
/// hyper-swim's detectors (each waits for a message only another's probe would send; 7 of 10 three-node formations
/// hung, 0 of 6 without the drop, 2026-10-04; reported to hyper-raft).
fn forget_address(state: &mut ShardState, anchor: HostId) {
  if let Some(peer) = state.plane.peers.get_mut(&anchor) {
    peer.stale = true;
  }
}

/// The dialer side: every record session this node dialed announces its identity once per connection, so the peer
/// learns who dialed (or refuses and counts a forged identity) whatever the pair's keying. The **canonical** one (this
/// node the lower anchor) also announces the pair's next epoch, installed once the acceptor answers with a valid
/// identity; any other announces [`IDENTITY_ONLY`].
///
/// Only a session whose connection has not announced yet is owed, decided in the state's borrow from the session
/// still in its slot, and each owed session is borrowed alone for its own exchange and returned before the next. The
/// membership loop runs this on every turn: borrowing every session on each turn, all of them held while one
/// announcement waited, kept the whole record plane lent about a hundred times a second per node and held every
/// session for up to 3.1 s behind one unanswered announcement (diagnostic run at 10% loss, 2026-10-09).
async fn announce_epochs() {
  // Decided in one borrow: `take_sessions` runs its filter inside the state's borrow, so the filter must not borrow the
  // state again.
  let owed: Vec<(HostId, bool)> = state::with_state(|state| {
    let origin = state.origin_anchor;
    let anchors: BTreeMap<HostId, HostId> = state
      .record_sessions
      .keys()
      .filter_map(|member| Some((*member, anchor_of(state, *member)?)))
      .collect();
    let announced_on = &state.plane.announced_on;
    state
      .record_sessions
      .iter_mut()
      .filter_map(|(member, link)| {
        let anchor = anchors.get(member)?;
        let connection = link.endpoint.as_mut()?.connection_id().ok()?;
        (announced_on.get(member) != Some(&connection)).then_some((*member, *anchor > origin))
      })
      .collect()
  })
  .unwrap_or_default();
  for (member, canonical) in owed {
    let mut sessions =
      crate::fleet::take_sessions("fleet.lent_to.announce_epochs", |host| host == member);
    let lent = u64::try_from(sessions.len()).unwrap_or(u64::MAX);
    let _ = state::with_state_counted(|s| s.count(EPOCH_LENT, lent));
    for (_, endpoint) in &mut sessions {
      announce_on(member, endpoint, canonical).await;
    }
    crate::fleet::return_sessions(sessions);
  }
}

/// One announcement on one borrowed session, once per connection: the pair's next epoch on its `canonical` session,
/// identity only on any other.
async fn announce_on(member: HostId, endpoint: &mut Endpoint, canonical: bool) {
  let Ok(connection) = endpoint.connection_id() else {
    return;
  };
  let Ok(secret) = endpoint.export_secret(EXPORTER_LABEL) else {
    return;
  };
  let Some((anchor, epoch, request)) = state::with_state(|state| {
    if state.plane.announced_on.get(&member) == Some(&connection) {
      return None;
    }
    let anchor = anchor_of(state, member)?;
    let epoch = if canonical {
      state
        .plane
        .announced
        .get(&anchor)
        .copied()
        .unwrap_or(IDENTITY_ONLY)
        .checked_add(1)?
    } else {
      IDENTITY_ONLY
    };
    let request = Announcement {
      epoch,
      member: state.fleet.host(),
      boot_nonce: state.member_boot_nonce,
    };
    Some((anchor, epoch, request))
  })
  .flatten() else {
    return;
  };
  crate::fleet::count_refusal(EPOCH_SENT);
  let budget = crate::fleet::record_exchange_budget_ns();
  let before = endpoint.last_exchange();
  let answered = futures::within(
    budget,
    endpoint.request(PLANE_EPOCH_STREAM, Priority::Control, &request.encode()),
  )
  .await;
  let reply = match answered {
    Ok(Some(Ok(reply))) => reply,
    _ => {
      endpoint.abandon_begun_since(before);
      crate::fleet::count_refusal(EPOCH_UNANSWERED);
      // A re-announcement after the member reconnected that goes unanswered: the session leads to a process that is
      // gone, so it is dropped and the link dials the member's new one ([`note_reconnected`]).
      let _ = state::with_state(|state| {
        if state.plane.reannouncing.remove(&member) {
          state.record_sessions.remove(&member);
          crate::fleet::wake_link_waiter_of(state, member);
          crate::fleet::count_refusal_in(state, PEER_REDIALED);
        }
      });
      return;
    }
  };
  let _ = state::with_state_counted(|state| {
    let Some(answer) = Announcement::decode(&reply).filter(|answer| answer.epoch == epoch) else {
      crate::fleet::count_refusal_in(state, EPOCH_REFUSED);
      crate::fleet::count_refusal_in(state, EPOCH_REFUSED_ANSWER);
      return;
    };
    if crate::fleet::learn_member(state, anchor, answer.boot_nonce, answer.member)
      == crate::fleet::LearnedOutcome::Forged
    {
      return;
    }
    state.plane.announced_on.insert(member, connection);
    state.plane.reannouncing.remove(&member);
    if epoch == IDENTITY_ONLY {
      return;
    }
    let installed = state.plane.plane.as_mut().map(|plane| {
      plane.install_epoch(
        answer.member,
        epoch,
        &ExporterSecret::new(secret),
        Role::Initiator,
      )
    });
    if matches!(installed, Some(Ok(()))) {
      state.plane.announced.insert(anchor, epoch);
      state.plane.keyed.insert(answer.member, false);
      forget_address(state, anchor);
    } else {
      crate::fleet::count_refusal_in(state, EPOCH_REFUSED);
      crate::fleet::count_refusal_in(state, EPOCH_REFUSED_INSTALL);
    }
  });
}

/// The fleet's decisions over the control shard's state, while the plane is taken out of it.
struct StateFleet<'a>(&'a mut ShardState);

impl Fleet for StateFleet<'_> {
  fn announced_version(&self) -> u64 {
    self
      .0
      .lease
      .known_version(self.0.fleet.configuration().version)
  }

  fn answer(
    &mut self,
    prober: HostId,
    boot_nonce: u64,
    configuration_version: u64,
  ) -> Option<(u64, Option<u64>)> {
    let state = &mut *self.0;
    let anchor = anchor_of(state, prober)?;
    if crate::fleet::learn_member(state, anchor, boot_nonce, prober)
      == crate::fleet::LearnedOutcome::Forged
    {
      return None;
    }
    // The holder side of the owner lease (§4.8 "Leases and reads"; AUD-08): when this node answered, and the
    // configuration version the prober announced.
    let now = slates_machine::clock::monotonic_ns();
    state.answers_given.answered_alive(prober, now);
    state.answers_given.announced(prober, configuration_version);
    let regional = state.council.configuration();
    Some((regional.version, regional.standing_of(prober)))
  }

  fn hears(&self, from: HostId) -> bool {
    // Test support: nothing from a peer this node is deaf to reaches the plane, the probe-plane loss a test imposes.
    !self.0.probe_deaf_to.contains(&from)
  }

  fn admit(&mut self, from: HostId, boot_nonce: u64) -> bool {
    let state = &mut *self.0;
    anchor_of(state, from).is_some_and(|anchor| {
      crate::fleet::learn_member(state, anchor, boot_nonce, from)
        != crate::fleet::LearnedOutcome::Forged
    })
  }

  fn relays_to(&self, target: HostId) -> bool {
    crate::fleet::keeps_direct_contact_with(self.0, target)
  }

  fn lease_holders(&self, into: &mut Vec<HostId>) {
    self.0.fleet.configuration().lease_holders_into(into);
  }
}

/// Folds the events a datagram produced: each acknowledgement's round trip, owner-lease standing and first arrival.
fn fold_events(state: &mut ShardState, events: &[PlaneEvent]) {
  for event in events {
    if let PlaneEvent::Acked {
      from,
      configuration_version,
      standing,
      sent_ns,
      rtt_ns,
      ..
    } = *event
    {
      crate::fleet::sample_path(state, from, rtt_ns);
      state.probe_windows.acknowledged = state.probe_windows.acknowledged.saturating_add(1);
      state.formed_probe_peers.insert(from);
      // Only an answer from this node's own region bears on its lease: its holders are all there, and another
      // region's answer carries that region's council version and no standing for this node — another counter,
      // which read as newer superseded the owner for good (found 2026-10-07: region 1 at version 5, region 0 at 4).
      if !crate::fleet::same_region(state, from) {
        continue;
      }
      let installed = state.fleet.configuration();
      let (own, installed_version) = (installed.standing(), installed.version);
      state.lease.answered(
        from,
        sent_ns,
        configuration_version,
        standing,
        own,
        installed_version,
      );
    }
  }
}

/// The members whose detector state changed since the last fold, with that state.
fn changed_members(state: &mut ShardState) -> Vec<(HostId, MemberState)> {
  let Some(plane) = state.plane.plane.as_ref() else {
    return Vec::new();
  };
  let mut changed = Vec::new();
  for (member, belief) in plane.detector().membership().after(None) {
    let member = HostId(member.0);
    if member == state.fleet.host() {
      continue;
    }
    if state.plane.folded.get(&member) != Some(&belief) {
      changed.push((member, belief));
    }
  }
  for (member, belief) in &changed {
    state.plane.folded.insert(*member, *belief);
    // A member that stopped answering may have moved: its address is resolved afresh.
    if belief.liveness == slates_cluster::membership::Liveness::Suspect
      && let Some(anchor) = anchor_of(state, *member)
    {
      forget_address(state, anchor);
    }
  }
  changed
}

/// Applies the detector's changed `beliefs` to the control shard's `FleetNode`, under the fleet's admission rule
/// (AC-8.1 / T-8.12), and returns those admitted, for the other shards:
/// - gossip about a member the fleet has never seen enrolls nothing (a stranger needs learn-on-contact);
/// - an `Alive` belief is taken only for an authenticated member id, so a replaced identity (a restarted peer's old id)
///   or a forged one is never revived;
/// - a death or a refutation of a known member is folded under the incarnation order, so stale alive gossip cannot
///   resurrect a death.
///
/// A member that leaves direct contact drops out of the formed mesh, and its pending discovery exchange re-checks at
/// once.
pub(crate) fn admit_beliefs(
  state: &mut ShardState,
  beliefs: &[(HostId, MemberState)],
) -> Vec<(HostId, MemberState)> {
  let mut admitted = Vec::with_capacity(beliefs.len());
  for &(member, belief) in beliefs {
    if state.fleet.membership().state(member).is_none() {
      continue;
    }
    if belief.liveness == slates_cluster::membership::Liveness::Alive
      && !state.authenticated_members.contains(&member)
    {
      continue;
    }
    if slates_cluster::fleet::apply_peer_state(&mut state.fleet, member, Some(belief)) {
      crate::fleet::wake_link_waiter_of(state, member);
    }
    if !crate::fleet::keeps_direct_contact_with(state, member) {
      state.formed_probe_peers.remove(&member);
    }
    admitted.push((member, belief));
  }
  admitted
}

/// The deaths the fleet holds that the detector has not reached (an injected death, a restarted peer's old id folded
/// by learn-on-contact), told to the detector so it gossips them and a live peer can refute them (A-15's rejoin by
/// refutation, on the plane).
fn tell_detector_of_deaths(state: &mut ShardState) {
  let Some(plane) = state.plane.plane.as_mut() else {
    return;
  };
  for (member, belief) in state.fleet.membership().dead() {
    let held = plane.belief(member);
    let reached = held.is_some_and(|held| {
      held.incarnation > belief.incarnation
        || (held.incarnation == belief.incarnation
          && held.liveness == slates_cluster::membership::Liveness::Dead)
    });
    if !reached {
      plane.apply(member, belief);
    }
  }
}

/// The manifest seed ids whose anchors are now known under another member id, folded dead on this shard (each once) and
/// returned for the other shards. The seed is the placeholder the manifest names a peer by before it announces itself
/// (`FleetPeer::host`); once learn-on-contact knows the peer's real id it would otherwise stay alive forever (the role
/// the per-peer probe task's `follow_current_id` had). Its path estimate goes with it.
fn retire_learned_seeds(state: &mut ShardState) -> Vec<(HostId, MemberState)> {
  let mut retired = Vec::new();
  let learned: Vec<(HostId, HostId)> = state
    .learned_members
    .iter()
    .map(|(anchor, member)| (*anchor, member.host))
    .collect();
  for (anchor, host) in learned {
    let Some(peer) = state.plane.peers.get_mut(&anchor) else {
      continue;
    };
    if peer.seed_retired || peer.seed == host {
      continue;
    }
    peer.seed_retired = true;
    let seed = peer.seed;
    let incarnation = state
      .fleet
      .membership()
      .state(seed)
      .map_or(0, |belief| belief.incarnation);
    let death = MemberState {
      liveness: slates_cluster::membership::Liveness::Dead,
      incarnation,
    };
    slates_cluster::fleet::apply_peer_state(&mut state.fleet, seed, Some(death));
    state.formed_probe_peers.remove(&seed);
    let _ = state.peer_paths.remove(&seed);
    crate::fleet::wake_link_waiter_of(state, seed);
    retired.push((seed, death));
  }
  retired
}

/// The control shard's turn of [`fold_membership`]: retires the manifest seeds now known by their real ids, tells the
/// detector the deaths the fleet learned outside it, and folds the detector's changed beliefs into this shard's
/// `FleetNode` under [`admit_beliefs`]. Returns what this shard folded, for the other shards to apply. A belief the
/// admission rule did not take — an id not yet learned on contact — is not recorded as folded, so it is offered again
/// next turn: learn-on-contact makes the id alive on this shard alone, and the detector's belief, unchanged, is then
/// the only way it reaches the others (before 2026-11-04 it never did, and a survivor's other shard held its live
/// peer nowhere).
fn fold_on_control(state: &mut ShardState) -> Vec<(HostId, MemberState)> {
  let mut folded = retire_learned_seeds(state);
  tell_detector_of_deaths(state);
  let changed = changed_members(state);
  let admitted = admit_beliefs(state, &changed);
  for (member, _) in changed.iter().filter(|belief| !admitted.contains(belief)) {
    state.plane.folded.remove(member);
  }
  folded.extend(admitted);
  folded
}

/// Folds the detector's changed beliefs into every shard's `FleetNode` (the control shard's first, then each other
/// owner's, D-7) under [`admit_beliefs`], after telling the detector the deaths the fleet learned outside it, and
/// retires the manifest seeds of peers now known by their real ids.
async fn fold_membership(origin: u16, shards: &[u16]) {
  let changed = state::with_state(fold_on_control).unwrap_or_default();
  if changed.is_empty() {
    return;
  }
  for shard in shards.iter().copied().filter(|shard| *shard != origin) {
    let for_shard = changed.clone();
    let folded = crate::xshard::run_on(origin, shard, move |state| {
      for (member, belief) in &for_shard {
        slates_cluster::fleet::apply_peer_state(&mut state.fleet, *member, Some(*belief));
      }
    });
    // A shard that refused the fold (its admission bound) is counted, and the beliefs are forgotten as folded so the
    // next turn folds them again (idempotent on the shards that took them): a refusal delays a shard's view, it never
    // leaves it stale.
    if folded.is_err() {
      crate::fleet::count_refusal(FOLD_REFUSED);
      let _ = state::with_state_counted(|state| {
        for (member, _) in &changed {
          state.plane.folded.remove(member);
        }
      });
    }
  }
}

/// Resolves every peer's plane address the plane has not resolved yet or holds stale (discovery first, then the
/// manifest's address, through the resolver for a name). A refused re-resolution keeps the stale address, retried
/// next turn.
async fn resolve_peers(resolver: Option<Kept<Resolver>>) {
  let unresolved: Vec<(HostId, NodeAddress, CertificateDer<'static>)> =
    state::with_state(|state| {
      state
        .plane
        .peers
        .iter()
        .filter(|(_, peer)| peer.resolved.is_none() || peer.stale)
        .map(|(anchor, peer)| (*anchor, peer.address.clone(), peer.certificate.clone()))
        .collect()
    })
    .unwrap_or_default();
  for (anchor, address, certificate) in unresolved {
    let resolved =
      crate::fleet::resolve_peer_address(&address, Plane::Probe, &certificate, resolver).await;
    let _ =
      state::with_state_counted(
        |state| match (resolved, state.plane.peers.get_mut(&anchor)) {
          (Some(resolved), Some(peer)) => {
            peer.resolved = Some(resolved);
            peer.stale = false;
          }
          _ => crate::fleet::count_refusal_in(state, ADDRESS_UNRESOLVED),
        },
      );
  }
}

/// Joins to the detector every keyed member whose address is resolved, once (`MemberPlane::join`): a member is judged
/// only once a probe of it can be sent.
fn join_addressable() {
  let _ = state::with_state_counted(|state| {
    let ready: Vec<HostId> = state
      .plane
      .keyed
      .iter()
      .filter(|(_, joined)| !**joined)
      .map(|(member, _)| *member)
      .filter(|member| {
        anchor_of(state, *member)
          .and_then(|anchor| state.plane.peers.get(&anchor))
          .is_some_and(|peer| peer.resolved.is_some())
      })
      .collect();
    let Some(plane) = state.plane.plane.as_mut() else {
      return;
    };
    for member in ready {
      // The keying session's measured round trip, when the session is in hand and has a sample.
      let handshake_rtt = state
        .record_sessions
        .get(&member)
        .and_then(|link| link.endpoint.as_ref())
        .map(slates_transport::endpoint::Endpoint::smoothed_rtt)
        .filter(|rtt| *rtt > 0)
        .map(std::time::Duration::from_nanos);
      plane.join(member, handshake_rtt);
      state.plane.keyed.insert(member, true);
    }
  });
}

/// Steps the detector at `now_ns` (`received`: a datagram to feed first, stamped `now_ns`), and returns what the plane
/// sealed, addressed.
fn drive(now_ns: u64, received: Option<&mut [u8]>) -> Vec<(SocketAddrV4, Vec<u8>)> {
  state::with_state(|state| {
    let mut plane = state.plane.plane.take()?;
    let mut events = Vec::new();
    {
      let mut fleet = StateFleet(state);
      if let Some(datagram) = received {
        plane.receive(datagram, now_ns, &mut fleet, &mut events);
      }
      plane.step(now_ns, &mut fleet);
    }
    fold_events(state, &events);
    let mut sealed: Vec<(HostId, Vec<u8>)> = Vec::new();
    plane.flush(|to, bytes| sealed.push((to, bytes.to_vec())));
    state.plane.plane = Some(plane);
    let mut addressed = Vec::with_capacity(sealed.len());
    for (member, bytes) in sealed {
      let resolved = anchor_of(state, member)
        .and_then(|anchor| state.plane.peers.get(&anchor))
        .and_then(|peer| peer.resolved);
      match resolved {
        Some(address) => addressed.push((address, bytes)),
        None => crate::fleet::count_refusal_in(state, ADDRESS_UNRESOLVED),
      }
    }
    Some(addressed)
  })
  .flatten()
  .unwrap_or_default()
}

/// Sends what the plane sealed; a refused send is a lost datagram, which the detector measures, and is counted.
fn send(socket: &UdpSocket, addressed: &[(SocketAddrV4, Vec<u8>)]) {
  for (address, bytes) in addressed {
    if !matches!(socket.try_send_to(bytes, *address), Ok(Some(_))) {
      crate::fleet::count_refusal(SEND_REFUSED);
    }
  }
}

/// How this node renews its owner lease (§4.8 "Leases and reads"; `slates_cluster::lease_renewal`): each holder probed
/// at least once a coordinator period, the cadence the lease bound is stated in (nine periods: `lease::horizon_ns`), so
/// eight renewals in a row can be lost before a live, reachable owner's lease lapses; an answer counts within the bound.
fn lease_renewal() -> slates_cluster::lease_renewal::LeaseRenewal {
  slates_cluster::lease_renewal::LeaseRenewal {
    interval_ns: crate::daemon::HEARTBEAT_NS,
    bound_ns: crate::lease::lease_bound_ns(),
  }
}

/// The membership task: for the daemon's life, the plane over `socket`. Each turn it announces any canonical epoch owed,
/// resolves new peers, steps the detector and sends, then waits for a datagram until the detector's wake (or a
/// heartbeat, so new sessions are keyed promptly), feeds it in, and folds the detector's view into the fleet.
pub(crate) async fn run(
  socket: UdpSocket,
  local: HostId,
  boot_nonce: u64,
  members: NonZeroUsize,
  resolver: Option<Kept<Resolver>>,
) {
  let installed = state::with_state(|state| {
    MemberPlane::new(
      local,
      boot_nonce,
      members,
      std::time::Duration::from_nanos(slates_machine::clock::resolution_ns()),
      lease_renewal(),
      std::time::Duration::from_nanos(crate::lease::horizon_ns()),
    )
    .map(|plane| state.plane.plane = Some(plane))
    .is_ok()
  })
  .unwrap_or(false);
  if !installed {
    crate::fleet::count_refusal(EPOCH_REFUSED);
    return;
  }
  let (origin, shards) = state::with_state(|s| (s.shard, s.shards.clone())).unwrap_or_default();
  let mut buffer = vec![0u8; RECEIVE_BYTES];
  loop {
    announce_epochs().await;
    resolve_peers(resolver).await;
    join_addressable();
    let now = slates_machine::clock::monotonic_ns();
    send(&socket, &drive(now, None));
    fold_membership(origin, &shards).await;
    let wake =
      state::with_state(|state| state.plane.plane.as_ref().and_then(MemberPlane::wake)).flatten();
    let wait = wake
      .map_or(crate::daemon::HEARTBEAT_NS, |at| at.saturating_sub(now))
      .min(crate::daemon::HEARTBEAT_NS);
    match futures::within(wait, socket.recv_from(&mut buffer)).await {
      Ok(Some(Ok((length, _)))) => {
        let stamp = slates_machine::clock::monotonic_ns();
        if let Some(datagram) = buffer.get_mut(..length) {
          send(&socket, &drive(stamp, Some(datagram)));
          fold_membership(origin, &shards).await;
        }
      }
      // Only the socket gone ends the plane (counted); any other refused receive is counted by its kind and the
      // plane reads on. Before 2026-11-07 a gone socket was counted and read again every turn, without end.
      Ok(Some(Err(refusal))) => match refusal.receive_refusal() {
        ReceiveRefusal::SocketGone => {
          crate::fleet::count_refusal(RECEIVE_SOCKET_GONE);
          slates_machine::log_line!(
            "slates-server: fleet: the membership plane's socket is gone: {refusal}"
          );
          return;
        }
        ReceiveRefusal::EarlierSend => {
          crate::fleet::count_refusal(RECEIVE_REFUSED_EARLIER_SEND);
        }
        ReceiveRefusal::Resources => {
          crate::fleet::count_refusal(RECEIVE_REFUSED_RESOURCES);
        }
        ReceiveRefusal::Other => {
          crate::fleet::count_refusal(RECEIVE_REFUSED_OTHER);
        }
      },
      // The detector's wake, or the heartbeat: the next turn steps it.
      Ok(None) => {}
      Err(_) => return,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// §4.8 "Leases and reads" across regions (found 2026-10-07 on two Docker networks: region 1's council at version 5,
  /// region 0's at 4, and region 0's owner refused its own volume `LeaseUnconfirmed` for good, counting
  /// `lease.refused.superseded`). Do: fold a probe acknowledgement from a member of another region announcing a newer
  /// configuration version and no standing for this node, then the same from a member of this node's region. Expect:
  /// the other region's answer leaves the lease alone — its council's versions are another counter — and only the
  /// same-region answer supersedes it.
  #[test]
  fn another_regions_configuration_version_never_supersedes_the_owner_lease() {
    let (foreign_superseded, own_superseded) = crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let foreign = HostId(local.0 ^ 0x11);
      let neighbour = HostId(local.0 ^ 0x22);
      state
        .node_regions
        .insert(local, slates_db::register::RegionId(0));
      state
        .node_regions
        .insert(foreign, slates_db::register::RegionId(1));
      state
        .node_regions
        .insert(neighbour, slates_db::register::RegionId(0));
      let installed = state.fleet.configuration().version;
      let newer = installed.saturating_add(1);
      let answer = |from| PlaneEvent::Acked {
        from,
        boot_nonce: 0,
        configuration_version: newer,
        standing: None,
        sent_ns: 0,
        rtt_ns: 1,
      };
      fold_events(state, &[answer(foreign)]);
      let foreign_superseded = state.lease.known_version(installed) != installed;
      fold_events(state, &[answer(neighbour)]);
      let own_superseded = state.lease.known_version(installed) != installed;
      (foreign_superseded, own_superseded)
    });
    assert!(
      !foreign_superseded,
      "another region's version left the lease alone"
    );
    assert!(
      own_superseded,
      "a same-region holder's newer version supersedes, as before"
    );
  }
  use slates_cluster::membership::Liveness;

  fn state_of(liveness: Liveness, incarnation: u64) -> MemberState {
    MemberState {
      liveness,
      incarnation,
    }
  }

  /// AC-8.1 / T-8.12 (ported from the per-peer probe task's test to the membership plane's fold, A-67 H-2b). Do: fold
  /// a third member's death the detector learned from a neighbour's gossip, a stranger's alive report, a stale alive
  /// report, then a refutation and a later death. Expect: the death reaches the council's failure view and stale gossip
  /// cannot resurrect it, the stranger is never enrolled, and the refutation then the later death are folded in order.
  #[test]
  fn a_neighbors_gossip_retires_a_known_third_member_without_enrolling_strangers() {
    let (lost_alive_after_death, stranger_known, refuted, dead_again) =
      crate::daemon::audit_on_shard(|state| {
        let local = state.fleet.host();
        let lost = HostId(local.0.wrapping_add(2));
        let stranger = HostId(local.0.wrapping_add(3));
        state.fleet.observe(lost, state_of(Liveness::Alive, 0));
        state.authenticated_members.insert(lost);
        admit_beliefs(
          state,
          &[
            (lost, state_of(Liveness::Dead, 1)),
            (stranger, state_of(Liveness::Alive, 0)),
          ],
        );
        admit_beliefs(state, &[(lost, state_of(Liveness::Alive, 0))]);
        let lost_alive_after_death = state.fleet.membership().alive().contains(&lost);
        let stranger_known = state.fleet.membership().state(stranger).is_some();
        admit_beliefs(state, &[(lost, state_of(Liveness::Alive, 2))]);
        let refuted = state.fleet.membership().alive().contains(&lost);
        admit_beliefs(state, &[(lost, state_of(Liveness::Dead, 2))]);
        let dead_again = !state.fleet.membership().alive().contains(&lost);
        (lost_alive_after_death, stranger_known, refuted, dead_again)
      });
    assert!(
      !lost_alive_after_death,
      "a third member's death reaches the failure view; stale gossip cannot revive it"
    );
    assert!(!stranger_known, "gossip does not authorize enrollment");
    assert!(
      refuted,
      "the member's own higher incarnation refutes the death"
    );
    assert!(dead_again, "a later death at that incarnation is folded");
  }

  /// AC-8.1 / T-8.12 (ported, A-67 H-2b). Do: learn a peer's identity, then its restart under a new member id, and fold
  /// a high-incarnation alive belief about the replaced id. Expect: the replaced id stays out of the alive set.
  #[test]
  fn gossip_cannot_revive_a_replaced_identity() {
    let old_alive = crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let anchor = HostId(local.0.wrapping_add(1));
      let old = crate::deploy::member_id(anchor, 1);
      let current = crate::deploy::member_id(anchor, 2);
      crate::fleet::learn_member(state, anchor, 1, old);
      crate::fleet::learn_member(state, anchor, 2, current);
      admit_beliefs(state, &[(old, state_of(Liveness::Alive, 10))]);
      state.fleet.membership().alive().contains(&old)
    });
    assert!(
      !old_alive,
      "a restarted peer's old id is never revived by gossip"
    );
  }

  /// A-67 H-2b. Do: encode an announcement and decode it, then decode a truncated and an overlong one. Expect: the
  /// same announcement back, and `None` for both malformed lengths.
  #[test]
  fn an_announcement_round_trips_and_a_wrong_length_is_refused() {
    let announced = Announcement {
      epoch: 9,
      member: HostId(0x0102_0304_0506_0708),
      boot_nonce: u64::MAX,
    };
    let bytes = announced.encode();
    assert_eq!(Announcement::decode(&bytes), Some(announced));
    assert_eq!(Announcement::decode(&bytes[..ANNOUNCEMENT_BYTES - 1]), None);
    let mut long = bytes.to_vec();
    long.push(0);
    assert_eq!(Announcement::decode(&long), None);
  }

  /// §4.8 "Membership" (the bug record of 2026-11-04, CI run 41774055498): every shard of a node holds a live peer
  /// alive, whichever comes first, the detector's word or learn-on-contact. Do: on the control shard, let the detector
  /// hold a peer alive before learn-on-contact has authenticated its id, fold; then learn the peer on contact and fold
  /// again with the detector's belief unchanged. Expect: the second fold hands the peer, alive, to the other shards.
  /// Before the fix the first fold recorded the belief folded though it admitted nothing, so the peer was alive on the
  /// control shard alone, and a survivor's other shard reported itself the only member after a third node died.
  #[test]
  fn a_peer_the_detector_knew_before_learn_on_contact_reaches_every_shard() {
    let (first, second) = crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let plane = MemberPlane::new(
        local,
        0,
        NonZeroUsize::MIN.saturating_add(2),
        std::time::Duration::from_nanos(slates_machine::clock::resolution_ns()),
        lease_renewal(),
        std::time::Duration::from_nanos(crate::lease::horizon_ns()),
      )
      .unwrap();
      state.plane.plane = Some(plane);
      let anchor = HostId(local.0.wrapping_add(1));
      let peer = crate::deploy::member_id(anchor, 1);
      let alive = state_of(Liveness::Alive, 0);
      state.plane.plane.as_mut().unwrap().apply(peer, alive);
      let first = fold_on_control(state);
      crate::fleet::learn_member(state, anchor, 1, peer);
      let second = fold_on_control(state);
      (
        first.contains(&(peer, alive)),
        second.contains(&(peer, alive)),
      )
    });
    assert!(
      !first,
      "an id not yet learned on contact is not folded alive anywhere"
    );
    assert!(
      second,
      "once learned on contact, the detector's unchanged belief reaches the other shards"
    );
  }
}
