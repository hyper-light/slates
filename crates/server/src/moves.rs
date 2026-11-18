//! The planned move of a volume to another fleet member (§4.10 "Ownership follows the writer": "ownership migrates there
//! by the planned handoff: the current owner seals, the delta ships by identity to the new host …, the regional group
//! advances that object's ownership generation and names the new owner, the new owner runs phase one, and the old
//! owner's later writes are refused"; "operators keep an explicit move"; A-291).
//!
//! The handoff is place, switch, adopt, release. The switch is one committed configuration change, and the old owner
//! keeps its copy until the new owner has placed the volume under its own placement, so the volume is never held by
//! fewer than `f + 1` hosts and every failure on either side of the switch is a case the takeover already handles:
//!
//! - **The old owner freezes and places.** A move is asked of the volume's owner shard ([`move_verb`]). The volume
//!   must be a plain live volume with no attachment or lease, no clone, lineage or green that depends on it, and no
//!   snapshot but its head (a move carries the head; older snapshots stay with this owner and would be lost), and the
//!   target must be another member of this owner's neighbourhood (it holds the volume's sealing keys), each refused by
//!   its own `MoveRefusal`. The verb hands
//!   over the head: the head epoch itself when nothing changed since, else a snapshot taken now. It freezes the
//!   volume ([`MoveOut`]), so every other verb on it is told to come back, and answers `Moving`; the client asks
//!   again until it is answered `Moved`.
//! - **The report.** Each period the control shard ([`drive_outgoing`]) waits until the head and the catalog are
//!   placed at `f + 1` of this owner's cohort (a live-shipped volume's log durable at its head too), then captures its
//!   own newest records of both registers from the owner's acceptor and reports `Move` to the council with the cohort
//!   the records sit in, each period until the configuration shows the switch. The target's neighbourhood must cover
//!   that cohort, so that the target's takeover, should it die before adopting, recovers the volume from it
//!   (`MoveRefusal::Neighbourhood` otherwise, before anything is reported). An unreported move whose records are not
//!   placed within the failover SLO is refused `MoveRefusal::Unplaced` and thaws. From the first report the move is irrevocable,
//!   since a reported move can commit later, unless the target leaves the region first: no entry can then apply it (a
//!   move's target must be a member, and a retired id never returns), and the volume thaws.
//! - **The switch.** The council commits the exception naming the new owner, the member it left and the cohort. Every
//!   node routes the volume to the new owner from then on ([`crate::owner_location`], the forward); every holder of
//!   its copies follows ([`follow_moved_objects`]) and takes the new owner's authority at its next reconcile, so a
//!   record from the old owner is refused. The old owner keeps its frozen copy.
//! - **The new owner adopts** ([`drive_adoptions`]): phase one by name over the recorded cohort ([`MOVE_STREAM`],
//!   answered by [`serve_move_prepare`] through `slates_db::takeover::answer_move`): the member the move left answers
//!   from the records it captured (it may be one of the `f + 1` that placed them, and it writes no more), every other
//!   member from its copy. Once the cohort's recovery quorum has answered, the newest record of each register is
//!   re-committed under the new owner's placement at the round's epoch (`takeover::adopt_at`, the takeover's own
//!   adoption) and the volume is materialized as a takeover's successor materializes it: content by identity from the
//!   head's recorded holders, a live-shipped volume from its log. Then the new owner seals the volume under its own
//!   placement ([`seal_adopted`]), and once that seal is placed it drops the adopted snapshot it superseded and reports
//!   `MoveAdopted`.
//! - **The release.** Once the exception no longer names the member it left (adopted, or followed through the new
//!   owner's takeover), the old owner releases its copy whole ([`release`]: `Op::VolumeReleased`, no tombstone, since
//!   the registers are the new owner's; its memory in the destroy's slices) and forgets its captured records, and every
//!   holder outside the new owner's placement drops its stale copy.
//!
//! If the new owner dies before it adopts, its takeover follows the exception to its successor, which adopts the volume
//! in that takeover's round from the copies the holders keep under the dead owner's authority, and the old owner then
//! releases. When that successor is the old owner itself, its takeover's adoption supersedes the frozen copy
//! ([`supersede_frozen`]), since the new owner may have taken writes before it died; a move once seen switched is
//! never thawed. If the old owner dies after the switch, the new owner's round counts the rest of the cohort. If it dies
//! before, the switch never happens and the volume is recovered by its takeover as any of its volumes is.
//!
//! **R8.** At one node there is no other member, so every move is refused `MoveRefusal::Target`.

use std::collections::{BTreeMap, BTreeSet};

use slates_cluster::{CommitBudget, broadcast};
use slates_db::catalog::{BaseRecord, Principal, Role, VolumeId as DbVolumeId, VolumeState};
use slates_db::op::Op;
use slates_db::register::{
  Accepted, Acceptor, HostEpoch, HostId, HostPrepare, HostPromise, ObjectId, RegionalConfiguration,
  RegisterError, encode_refusal,
};
use slates_db::takeover::{CountingRule, Heard, Readiness, answer_move, readiness};
use slates_ipc::protocol::{MoveRefusal, Refusal, ReplyBody, VolumeId};
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;
use slates_vfs::clock::Clock;

use crate::fleet::{Dispatch, LateReplies, return_sessions, take_sessions};
use crate::state::{self, ShardState};

/// Format: the stream a moved object's new owner asks its cohort on, by name ([`serve_move_prepare`]): distinct from
/// every other stream a record session multiplexes (1 to 24, and 25, which the live log's batches take).
pub(crate) const MOVE_STREAM: u64 = 26;

/// Counter: moves a shard began (the volume frozen with its head handed over).
const MOVE_BEGUN: &str = "fleet.move.begun";
/// Counter: move reports this node sent to the council.
const MOVE_REPORTED: &str = "fleet.move.reported";
/// Counter: unreported moves refused and thawed (the neighbourhood rule, or records not placed in time).
const MOVE_REFUSED: &str = "fleet.move.refused";
/// Counter: moves whose target left the region before the switch, the volume thawed.
const MOVE_THAWED: &str = "fleet.move.thawed";
/// Counter: reports held back because the owner's acceptor did not hold both registers' records.
const MOVE_UNCAPTURED: &str = "fleet.move.uncaptured";
/// Counter: volumes this node released to the member a move named.
const MOVE_RELEASED: &str = "fleet.move.released";
/// Counter: frozen copies a takeover's adoption of the same volume superseded (the target died; this node its successor).
const MOVE_SUPERSEDED: &str = "fleet.move.superseded";
/// Counter: releases the database refused (the volume keeps its record, retried next period).
const MOVE_RELEASE_REFUSED: &str = "fleet.move.release_refused";
/// Counter: held copies whose routing a move took to its new owner.
const MOVE_FOLLOWED: &str = "fleet.move.followed";
/// Counter: held copies dropped once a move was adopted by an owner whose placement leaves this holder out.
const MOVE_RECLAIMED: &str = "fleet.move.reclaimed";
/// Counter: move questions a holder refused (malformed, or not from the session's peer).
const MOVE_PREPARE_REFUSED: &str = "fleet.move.prepare_refused";
/// Counter: named objects a holder refused to answer (the configuration did not move them to the asker).
const MOVE_NAMED_REFUSED: &str = "fleet.move.named_refused";
/// Counter: answers held back because the shard's publish of their raised fences was refused.
const MOVE_PROMISE_UNPUBLISHED: &str = "fleet.move.promise_unpublished";
/// Counter: questions a holder did not answer because the round's configuration is not the one it installed.
const MOVE_PREPARE_UNMATCHED: &str = "fleet.move.prepare_unmatched";
/// Counter: adoption periods skipped because this node's placement is behind its council's configuration.
const MOVE_ROUND_BEHIND: &str = "fleet.move.round_behind";
/// Counter: adoption periods whose questions went unsent (no session to any cohort member).
const MOVE_QUESTIONS_UNSENT: &str = "fleet.move.questions_unsent";
/// Counter: answers the new owner could not bind to its question.
const MOVE_ANSWER_UNBOUND: &str = "fleet.move.answer_unbound";
/// Counter: rounds whose epoch could not be raised past a holder's fence (`u64::MAX`).
const MOVE_EPOCH_EXHAUSTED: &str = "fleet.move.epoch_exhausted";
/// Counter: registers whose cohort has fewer members left than its recovery quorum.
const MOVE_LOST: &str = "fleet.move.lost";
/// Counter: moved volumes this node adopted whole (head and catalog), to be served.
const MOVE_ADOPTED: &str = "fleet.move.adopted";
/// Counter: seals of an adopted volume the shard refused this period (retried next period).
const MOVE_SEAL_REFUSED: &str = "fleet.move.seal_refused";
/// Counter: adopted snapshots a new owner could not drop once its own seal was placed (kept; counted).
const MOVE_DROP_REFUSED: &str = "fleet.move.drop_refused";
/// Counter: adoption reports this node sent to the council (`MoveAdopted`).
const MOVE_ADOPTION_REPORTED: &str = "fleet.move.adoption_reported";
/// Counter: destroyed moved volumes this node reported to the council (`MovedForgotten`).
const MOVE_FORGOTTEN_REPORTED: &str = "fleet.move.forgotten_reported";

// ------------------------------------------------------------------------------------------------ the old owner

/// A move a shard has begun for one of its volumes (A-291): the member it moves to, the head epoch whose placement it
/// waits for, when it began, and whether it was reported (irrevocable from then on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MoveOut {
  /// The member the volume moves to.
  pub(crate) to: HostId,
  /// The volume's head epoch the move hands over: the head that must be placed before the report.
  pub(crate) epoch: u64,
  /// The shard clock when the move began, for the bound on an unreported move (`MoveRefusal::Unplaced`).
  pub(crate) begun_ns: u64,
  /// Whether the move has been reported to the council.
  pub(crate) reported: bool,
  /// Whether this node has seen the switch: the configuration naming another owner of the volume. From then on the
  /// move is known to have committed, so the copy is let go however the exception later changes (its new owner's
  /// takeover handing it back to this node, or its new owner's destroy dropping it), and never thawed.
  pub(crate) switched: bool,
  /// The refusal that ended the move before any report, answered to the next ask and then dropped.
  pub(crate) refused: Option<MoveRefusal>,
}

/// The moves a shard holds its volumes frozen for, by volume id, for every verb's gate ([`frozen`]). Bounded by the
/// shard's volumes.
pub(crate) type MovesOut = BTreeMap<DbVolumeId, MoveOut>;

/// Whether `volume` is frozen for a move on this shard: every verb but its status and the move itself is answered
/// `Overloaded` (retryable) while it is, and forwarded to its new owner from the switch on.
pub(crate) fn frozen(state: &ShardState, volume: VolumeId) -> bool {
  state
    .moves_out
    .get(&DbVolumeId {
      bytes: volume.bytes,
    })
    .is_some_and(|out| out.refused.is_none())
}

/// `Move` (§4.10, A-291): moves `volume`, which this shard holds, to the member `to`. Answered `Moved` once this node's
/// routing follows the configuration to `to`, `Moving` while the move waits for its placement or its switch, and a
/// typed refusal for a volume that cannot move. The client asks again until `Moved`.
pub(crate) fn move_verb(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  to: u64,
) -> ReplyBody {
  let to = HostId(to);
  let object = ObjectId(volume.bytes);
  let id = DbVolumeId {
    bytes: volume.bytes,
  };
  // The switch is the answer, on any node: once the routing follows the configuration to `to`, the move is done,
  // whether or not the member it left has let its copy go yet.
  if state.fleet.object_owner(object) == Some(to) {
    return ReplyBody::Moved { to: to.0 };
  }
  let (_, record) = match crate::verbs::find(state, volume) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  if !crate::verbs::rights_of(&record, principal).admin {
    return crate::verbs::forbidden("move");
  }
  if let Some(out) = state.moves_out.get(&id).copied() {
    if let Some(reason) = out.refused {
      state.moves_out.remove(&id);
      return refused(reason);
    }
    if out.to != to {
      return refused(MoveRefusal::Pending);
    }
    return ReplyBody::Moving { to: to.0 };
  }
  if let Some(reason) = unmovable(state, &record, to) {
    return refused(reason);
  }
  let epoch = match crate::verbs::head_for_move(state, volume) {
    Ok(epoch) => epoch,
    Err(reply) => return *reply,
  };
  let begun_ns = state.clock.monotonic_ns();
  state.moves_out.insert(
    id,
    MoveOut {
      to,
      epoch,
      begun_ns,
      reported: false,
      switched: false,
      refused: None,
    },
  );
  state.count(MOVE_BEGUN, 1);
  ReplyBody::Moving { to: to.0 }
}

/// The refusal a move gives for `reason`.
fn refused(reason: MoveRefusal) -> ReplyBody {
  ReplyBody::Refused {
    refusal: Refusal::MoveRefused { reason },
  }
}

/// Why `record`'s volume cannot move to `to` now, from this shard's own facts, or `None` when it can.
fn unmovable(
  state: &ShardState,
  record: &slates_db::catalog::VolumeRecord,
  to: HostId,
) -> Option<MoveRefusal> {
  let local = state.fleet.host();
  if to == local || !state.fleet.members().contains(&to) {
    return Some(MoveRefusal::Target);
  }
  if !state.fleet.configuration().neighbourhood.contains(&to) {
    return Some(MoveRefusal::Neighbourhood);
  }
  if !matches!(record.base, BaseRecord::Scratch)
    || !matches!(record.policy.role, Role::Plain)
    || record.state != VolumeState::Live
  {
    return Some(MoveRefusal::NotPlain);
  }
  let partition = state.db.partition();
  if !partition.attachments_of(record.id).is_empty() || record.lease.is_some() {
    return Some(MoveRefusal::Attached);
  }
  if partition.lineage(record.id).is_some()
    || partition.has_clones(record.id)
    || partition
      .green_origin_refs()
      .any(|(_, held)| held.volume == record.id)
  {
    return Some(MoveRefusal::Lineage);
  }
  if partition
    .snapshots_of(record.id)
    .iter()
    .any(|snapshot| snapshot.id != record.head)
  {
    return Some(MoveRefusal::Snapshots);
  }
  None
}

/// One outgoing move as its owner shard sees it this period, for the control shard's decision ([`step_for`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Outgoing {
  /// The volume's object id.
  pub(crate) object: ObjectId,
  /// The member it moves to.
  pub(crate) to: HostId,
  /// Whether it was reported.
  pub(crate) reported: bool,
  /// Whether this node has seen its switch ([`MoveOut::switched`]).
  pub(crate) switched: bool,
  /// Whether its head and catalog are placed (and a live log durable at its head), so it may be reported.
  pub(crate) ready: bool,
  /// Whether it has waited past the failover SLO unreported.
  pub(crate) overdue: bool,
  /// The cohort its records are placed at `f + 1` of: this owner's current cohort for it, in id order.
  pub(crate) cohort: Vec<HostId>,
}

/// The moves this shard has begun and not ended, for the control shard's period ([`drive_outgoing`]).
fn outgoing(state: &mut ShardState) -> Vec<Outgoing> {
  let quorum = state.fleet.configuration().quorum;
  let now = state.clock.monotonic_ns();
  let bound = state.config.failover_slo_ns;
  let begun: Vec<(DbVolumeId, MoveOut)> = state
    .moves_out
    .iter()
    .filter(|(_, out)| out.refused.is_none())
    .map(|(id, out)| (*id, *out))
    .collect();
  begun
    .into_iter()
    .map(|(id, out)| {
      let object = ObjectId(id.bytes);
      let ready = out.reported || ready_to_report(state, id, out.epoch, quorum);
      let mut cohort = state.fleet.configuration().place_content(object).candidates;
      cohort.sort_unstable_by_key(|host| host.0);
      Outgoing {
        object,
        to: out.to,
        reported: out.reported,
        switched: out.switched,
        ready,
        overdue: !out.reported && now.saturating_sub(out.begun_ns) > bound,
        cohort,
      }
    })
    .collect()
}

/// Whether the volume `id`'s records hold everything it took before its move: its head at `epoch` and its catalog at
/// its current version, each placed at `f + 1`, and a live-shipped volume's log durable at its head.
fn ready_to_report(
  state: &mut ShardState,
  id: DbVolumeId,
  epoch: u64,
  quorum: slates_db::register::Quorum,
) -> bool {
  let object = ObjectId(id.bytes);
  let Some(record) = state.db.partition().volume(id) else {
    return false;
  };
  let catalog_version = record.catalog_version;
  let placed = |register: ObjectId, sequence: u64| {
    state
      .placed_heads
      .get(&register)
      .is_some_and(|head| head.sequence >= sequence && head.placement.placed(quorum))
  };
  let records_placed = placed(object, epoch) && placed(object.catalog(), catalog_version);
  let live_durable = match crate::live_ship::durable_target(state, id) {
    None => true,
    Some(target) => matches!(
      crate::live_ship::durability(state, id, target, None),
      crate::live_ship::Durability::Durable
    ),
  };
  records_placed && live_durable
}

/// What the control shard does with one outgoing move this period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
  /// Nothing yet: its records are not placed.
  Wait,
  /// Report it (again): the council has not switched it.
  Report,
  /// Refuse it, unreported, for this reason: the volume thaws.
  Refuse(MoveRefusal),
  /// Thaw it: its target left the region before any switch could apply it.
  Thaw,
  /// The configuration switched it to `owner`, who has not adopted it yet: route its verbs there and keep the copy.
  Switched {
    /// The member that owns it now.
    owner: HostId,
  },
  /// The new owner adopted it (or its takeover's successor did, this node among them), or destroyed it: release the
  /// copy.
  Release {
    /// The member that owns it now.
    owner: HostId,
  },
}

/// The step for `out`, a move this node `local` began, from the committed regional configuration alone (pure).
pub(crate) fn step_for(regional: &RegionalConfiguration, local: HostId, out: &Outgoing) -> Step {
  if let Some(moved) = regional.moved.get(&out.object.placement_key())
    && moved.owner != local
  {
    // Its line starts elsewhere now: at the member the move named, or along that member's takeover.
    let owner = regional.owner_of(out.object).unwrap_or(moved.owner);
    return if moved.from == Some(local) {
      Step::Switched { owner }
    } else {
      Step::Release { owner }
    };
  }
  if out.switched {
    // The move committed, and no exception names another owner now: the target's takeover handed the volume back to
    // this node (its successor, whose adoption serves the state the target left, writes included), or the target
    // destroyed it. Either way the frozen copy is stale: let it go, never thaw it.
    return Step::Release { owner: local };
  }
  if !regional.members.contains(&out.to) {
    return Step::Thaw;
  }
  if out.reported {
    return Step::Report;
  }
  if out.overdue {
    return Step::Refuse(MoveRefusal::Unplaced);
  }
  if !out.ready {
    return Step::Wait;
  }
  let covered = regional
    .neighbourhoods
    .get(&out.to)
    .is_some_and(|neighbourhood| {
      out
        .cohort
        .iter()
        .all(|host| neighbourhood.hosts.contains(host))
    });
  if covered {
    Step::Report
  } else {
    Step::Refuse(MoveRefusal::Neighbourhood)
  }
}

/// This owner's newest records of `object`'s head and catalog registers, from its own acceptor: what it answers the
/// new owner's phase one with ([`serve_move_prepare`]). `None` until it holds both.
fn capture(owner_acceptor: &Acceptor, object: ObjectId) -> Option<BTreeMap<ObjectId, Accepted>> {
  let head = owner_acceptor.highest_accepted(object)?;
  let catalog = owner_acceptor.highest_accepted(object.catalog())?;
  Some(BTreeMap::from([
    (object, head),
    (object.catalog(), catalog),
  ]))
}

/// One period of this node's outgoing moves, on the control shard: each shard's moves are decided by the committed
/// configuration ([`step_for`]): reported until the switch (their own records captured from `owner_acceptor` first),
/// routed to the new owner once switched, released once adopted, refused or thawed.
pub(crate) async fn drive_outgoing(
  origin: u16,
  local: HostId,
  owner_acceptor: &mut Acceptor,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let shards = state::with_state(|s| s.shards.clone()).unwrap_or_default();
  let mut moves: Vec<(u16, Outgoing)> = Vec::new();
  for shard in shards {
    let outgoing = crate::xshard::call_within(origin, shard, outgoing, crate::daemon::HEARTBEAT_NS)
      .await
      .unwrap_or_default();
    moves.extend(outgoing.into_iter().map(|out| (shard, out)));
  }
  for (shard, out) in moves {
    let Some(step) = state::with_state(|s| step_for(s.council.configuration(), local, &out)) else {
      continue;
    };
    let object = out.object;
    match step {
      Step::Wait => {}
      Step::Refuse(reason) => crate::xshard::run_on_counted(origin, shard, move |s| {
        refuse_unreported(s, object, reason);
      }),
      Step::Thaw => crate::xshard::run_on_counted(origin, shard, move |s| thaw(s, object)),
      Step::Report => {
        let Some(records) = capture(owner_acceptor, object) else {
          let _ = state::with_state_counted(|s| s.count(MOVE_UNCAPTURED, 1));
          continue;
        };
        let _ = state::with_state_counted(|s| {
          s.move_records.insert(object, records);
          s.count(MOVE_REPORTED, 1);
        });
        if !out.reported {
          crate::xshard::run_on_counted(origin, shard, move |s| note_reported(s, object));
        }
        crate::fleet::send_council_report(
          slates_cluster::config_group::ConfigCommand::Move {
            object,
            from: local,
            to: out.to,
            cohort: out.cohort,
          },
          local,
          budget,
          in_flight,
        )
        .await;
      }
      Step::Switched { owner } => {
        if !out.switched {
          crate::xshard::run_on_counted(origin, shard, move |s| note_switched(s, object));
        }
        let _ = state::with_state_counted(|s| {
          if s.fleet.object_owner(object) != Some(owner) {
            crate::takeover::track_owner_on_every_shard(s, object, owner);
            crate::takeover::track_owner_on_every_shard(s, object.catalog(), owner);
          }
        });
      }
      Step::Release { owner } => {
        // Handed back to this node by the target's takeover: the adoption's materialization lets the frozen copy go
        // as it serves the adopted state (`supersede_frozen`), and nothing here may touch the placement it recorded.
        let adopted_back = owner == local
          && state::with_state_counted(|s| {
            s.pending_materializations.contains_key(&object)
              || s.pending_catalogs.contains_key(&object)
          })
          .unwrap_or(true);
        if !adopted_back {
          crate::xshard::run_on_counted(origin, shard, move |s| release(s, object, owner));
          owner_acceptor.forget_object(object);
          owner_acceptor.forget_object(object.catalog());
        }
        let _ = state::with_state_counted(|s| {
          s.move_records.remove(&object);
          if owner != local && s.fleet.object_owner(object) != Some(owner) {
            crate::takeover::track_owner_on_every_shard(s, object, owner);
            crate::takeover::track_owner_on_every_shard(s, object.catalog(), owner);
          }
        });
      }
    }
  }
}

/// Marks the move of `object` reported on its shard: irrevocable from now on.
fn note_reported(state: &mut ShardState, object: ObjectId) {
  if let Some(out) = state.moves_out.get_mut(&DbVolumeId { bytes: object.0 }) {
    out.reported = true;
  }
}

/// Marks the move of `object` switched on its shard: the configuration was seen naming another owner.
fn note_switched(state: &mut ShardState, object: ObjectId) {
  if let Some(out) = state.moves_out.get_mut(&DbVolumeId { bytes: object.0 }) {
    out.switched = true;
  }
}

/// Ends the unreported move of `object` on its shard for `reason`, answered to the client's next ask: the volume
/// thaws.
fn refuse_unreported(state: &mut ShardState, object: ObjectId, reason: MoveRefusal) {
  if let Some(out) = state.moves_out.get_mut(&DbVolumeId { bytes: object.0 })
    && !out.reported
    && out.refused.is_none()
  {
    out.refused = Some(reason);
    state.count(MOVE_REFUSED, 1);
  }
}

/// Ends the move of `object` whose target left the region before the switch: the volume is this node's again.
fn thaw(state: &mut ShardState, object: ObjectId) {
  if state
    .moves_out
    .remove(&DbVolumeId { bytes: object.0 })
    .is_some()
  {
    state.count(MOVE_THAWED, 1);
  }
}

/// Releases `object`'s frozen copy on its shard to `owner`, who adopted the volume (A-291): its routing names the new
/// owner so a verb forwards there, and the copy goes ([`let_go`]). Nothing but the routing when no copy is frozen here
/// any more (a takeover's adoption superseded it, [`supersede_frozen`]), so a volume served here is never released.
fn release(state: &mut ShardState, object: ObjectId, owner: HostId) {
  let id = DbVolumeId { bytes: object.0 };
  if owner != state.fleet.host() {
    state.fleet.track_object_owner(object, owner);
    state.fleet.track_object_owner(object.catalog(), owner);
  }
  if state.moves_out.contains_key(&id) && let_go(state, id, Gone::ToAnother) {
    state.count(MOVE_RELEASED, 1);
  }
}

/// Why a frozen copy goes ([`let_go`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gone {
  /// Another member owns the volume now: everything of it here goes, its name's alias on another partition too.
  ToAnother,
  /// This node's own takeover adopted the volume and serves it next, under the same id and name: the copy goes, but
  /// the placements the adoption recorded here and the name's alias, which names the same id, stay.
  Superseded,
}

/// Lets a takeover's adoption of `id` supersede the copy this node froze for a move (A-291; `verbs::materialize_taken_over`
/// asks): `true` when such a copy was here and is gone now, so the adopted state can be served; `false` when the volume
/// served here is no frozen copy (nothing to supersede) or the database refused the release (asked again next period).
pub(crate) fn supersede_frozen(state: &mut ShardState, id: DbVolumeId) -> bool {
  let superseded = state.moves_out.contains_key(&id) && let_go(state, id, Gone::Superseded);
  if superseded {
    state.count(MOVE_SUPERSEDED, 1);
  }
  superseded
}

/// Lets the frozen copy of `id` go: its record leaves whole (`Op::VolumeReleased`, no tombstone, since its registers are
/// another owner's or the adoption's), its slot is freed in the destroy's slices, and its snapshot views, pending seals,
/// live log shipper and move are forgotten, with its placements and name alias when it went to another member (`gone`).
/// `false`, everything kept, when the database refuses the release.
fn let_go(state: &mut ShardState, id: DbVolumeId, gone: Gone) -> bool {
  let object = ObjectId(id.bytes);
  let Some(handle) = state.by_id.get(&id).copied() else {
    state.moves_out.remove(&id);
    return true;
  };
  let name = state
    .db
    .partition()
    .volume(id)
    .map(|record| record.name.clone());
  let now = state.clock.monotonic_ns();
  if state
    .db
    .mutate(&mut state.segment, &Op::VolumeReleased { id }, now)
    .is_err()
  {
    state.count(MOVE_RELEASE_REFUSED, 1);
    return false;
  }
  state.by_id.remove(&id);
  crate::snapshot_view::end_all_of(state, id);
  state.releasing.push(handle);
  // The serve round steps the release's slices (`verbs::step_destroys`); woken, it keeps running until they are done.
  if let Some(task) = state.server_task {
    slates_rt::registry::wake(task.0);
  }
  state.moves_out.remove(&id);
  state.seals.remove(&object);
  state.mirror_seals.remove(&object);
  crate::live_ship::forget(state, id);
  if gone == Gone::ToAnother {
    state.placed_heads.remove(&object);
    state.placed_heads.remove(&object.catalog());
    if let Some(name) = name {
      crate::verbs::release_name_alias(state, id, &name);
    }
  }
  true
}

// ------------------------------------------------------------------------------------------------ every holder

/// Brings this holder's copies of every moved object (§4.8 "Authority scope") to the configuration, on the control
/// shard at each period before the held acceptors' authority is reconciled: a copy's routing follows its exception's
/// owner, so the acceptors take that owner's authority at the next reconcile (`fleet::reconcile_held_authority`) and a
/// record from the old owner is refused; and once the move is adopted (the exception names no member it left), a copy
/// outside the owner's placement is dropped, since the owner's records reach only its own cohort from then on.
pub(crate) fn follow_moved_objects(state: &mut ShardState) {
  let regional = state.council.configuration().clone();
  let local = state.fleet.host();
  let held: Vec<ObjectId> = state
    .holder_records
    .keys()
    .copied()
    .filter(|object| regional.moved.contains_key(&object.placement_key()))
    .collect();
  let (mut followed, mut reclaimed): (u64, u64) = (0, 0);
  for object in held {
    let Some(owner) = regional.owner_of(object) else {
      continue;
    };
    if !regional.members.contains(&owner) || owner == local {
      continue;
    }
    let adopted = regional
      .moved
      .get(&object.placement_key())
      .is_some_and(|moved| moved.from.is_none() && moved.owner == owner);
    let outside = regional
      .configuration_for(owner)
      .is_some_and(|placement| !placement.place(object).candidates.contains(&local));
    if adopted && outside {
      crate::takeover::drop_copy(state, object);
      reclaimed = reclaimed.saturating_add(1);
    } else if state.fleet.object_owner(object) != Some(owner) {
      crate::takeover::track_owner_on_every_shard(state, object, owner);
      followed = followed.saturating_add(1);
    }
  }
  if followed > 0 {
    state.count(MOVE_FOLLOWED, followed);
  }
  if reclaimed > 0 {
    state.count(MOVE_RECLAIMED, reclaimed);
  }
}

/// Answers a moved object's new owner on [`MOVE_STREAM`] (A-291): after checking the asker is the session's peer and the
/// round runs under the configuration this holder installed, each named object is answered alone through
/// `slates_db::takeover::answer_move`, which answers only an owner the configuration's exception names: from this
/// holder's copy, or, where this node is the member the move left, from the records it captured as the object's owner.
/// The promises stand behind the shard's publish, as a takeover's do.
pub(crate) fn serve_move_prepare(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Ok(prepare) = HostPrepare::decode(request) else {
    state.count(MOVE_PREPARE_REFUSED, 1);
    return Vec::new();
  };
  if prepare.owner != peer_host || prepare.departed != peer_host || prepare.named.is_empty() {
    state.count(MOVE_PREPARE_REFUSED, 1);
    return Vec::new();
  }
  let installed = state.fleet.configuration().version;
  if prepare.generation != installed || state.council.configuration().version != installed {
    state.count(MOVE_PREPARE_UNMATCHED, 1);
    if prepare.generation > installed {
      state.config_refresh_wanted = true;
      return Vec::new();
    }
    return encode_refusal(&RegisterError::ConfigurationStale { version: installed });
  }
  let regional = state.council.configuration().clone();
  let own: BTreeMap<ObjectId, Accepted> = prepare
    .named
    .iter()
    .filter_map(|object| {
      state
        .move_records
        .get(&object.placement_key())
        .and_then(|records| records.get(object))
        .map(|accepted| (*object, accepted.clone()))
    })
    .collect();
  let answer = answer_move(
    local,
    &prepare,
    &regional,
    &mut state.holder_records,
    &own,
    crate::takeover::page_budget(),
  );
  if answer.refused > 0 {
    state.count(MOVE_NAMED_REFUSED, u64::from(answer.refused));
  }
  if crate::verbs::publish_shard(state).is_err() {
    state.count(MOVE_PROMISE_UNPUBLISHED, 1);
    return Vec::new();
  }
  answer.promise.encode()
}

/// Reports to the council each destroyed volume this node owned under a moved-object exception, once its tombstone
/// retired, so the exception never outlives its object (`MovedForgotten`), each period until the configuration drops it.
/// The set is filled by the tombstone's retirement ([`note_destroyed`]) and bounded by the exceptions naming this node.
pub(crate) async fn report_forgotten(
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let owed: Vec<ObjectId> = state::with_state(|s| {
    let regional = s.council.configuration().clone();
    s.moves_forgotten
      .retain(|object| regional.forgets_moved(*object, local));
    s.moves_forgotten.iter().copied().collect()
  })
  .unwrap_or_default();
  for object in owed {
    let _ = state::with_state_counted(|s| s.count(MOVE_FORGOTTEN_REPORTED, 1));
    crate::fleet::send_council_report(
      slates_cluster::config_group::ConfigCommand::MovedForgotten {
        object,
        owner: local,
      },
      local,
      budget,
      in_flight,
    )
    .await;
  }
}

/// Notes on the control shard that `object`'s volume, which this node owned, is destroyed and its tombstone retired:
/// if an exception names this node its owner, it is reported forgotten ([`report_forgotten`]).
pub(crate) fn note_destroyed(state: &mut ShardState, object: ObjectId, local: HostId) {
  let key = object.placement_key();
  if state.council.configuration().forgets_moved(key, local) {
    state.moves_forgotten.insert(key);
  }
}

// ------------------------------------------------------------------------------------------------ the new owner

/// One adoption of a volume the configuration moved to this node (A-291), kept on the control shard until the move is
/// adopted. Bounded by the moved-object exceptions naming this node; an entry leaves with its exception's `from`.
#[derive(Debug)]
pub(crate) struct MoveAdoption {
  /// The configuration version the round runs under; an install of another starts it over.
  generation: u64,
  /// The epoch the adopted records are written under: above every epoch the cohort's copies hold.
  epoch: HostEpoch,
  /// Per register, the records its holders reported.
  learned: BTreeMap<ObjectId, BTreeMap<HostId, Accepted>>,
  /// Per register, the members that answered holding nothing of it.
  named_nothing: BTreeMap<ObjectId, BTreeSet<HostId>>,
  /// The highest fence a holder reported at or above the epoch; the next round runs above it.
  fenced: Option<HostEpoch>,
  /// The registers adopted.
  settled: BTreeSet<ObjectId>,
  /// The head epoch of the seal this node took once it served the volume, which must be placed under its own placement
  /// before the move is reported adopted, and the adopted snapshot it superseded.
  sealed: Option<(slates_db::catalog::SnapshotId, u64)>,
}

impl MoveAdoption {
  fn new(generation: u64, epoch: HostEpoch) -> MoveAdoption {
    MoveAdoption {
      generation,
      epoch,
      learned: BTreeMap::new(),
      named_nothing: BTreeMap::new(),
      fenced: None,
      settled: BTreeSet::new(),
      sealed: None,
    }
  }

  /// Starts the round over at `generation` and `epoch`: every promise was for the old round. What was adopted stays
  /// adopted.
  fn restart(&mut self, generation: u64, epoch: HostEpoch) {
    self.generation = generation;
    self.epoch = epoch;
    self.learned.clear();
    self.named_nothing.clear();
    self.fenced = None;
  }

  /// Whether both registers are adopted.
  fn adopted(&self, key: ObjectId) -> bool {
    registers(key)
      .iter()
      .all(|register| self.settled.contains(register))
  }
}

/// The registers one moved volume's adoption covers: its head register and its catalog's.
fn registers(key: ObjectId) -> [ObjectId; 2] {
  [key, key.catalog()]
}

/// A move this node owes the adoption of: its object and the cohort its records sit in.
struct Owed {
  key: ObjectId,
  cohort: Vec<HostId>,
}

/// The moves the configuration made to this node that it has not reported adopted: each exception naming this node its
/// owner and still naming the member it left. Drops the adoptions of exceptions that no longer do.
fn owed_adoptions(state: &mut ShardState, local: HostId) -> Vec<Owed> {
  let regional = state.council.configuration();
  let owed: Vec<Owed> = regional
    .moved
    .iter()
    .filter(|(key, moved)| {
      moved.owner == local && moved.from.is_some() && regional.owner_of(**key) == Some(local)
    })
    .map(|(key, moved)| Owed {
      key: *key,
      cohort: moved.cohort.clone(),
    })
    .collect();
  state
    .move_adoptions
    .retain(|key, _| owed.iter().any(|owed| owed.key == *key));
  owed
}

/// One period of the moves this node adopts, on the control shard: for each volume the configuration moved here and
/// not yet reported adopted, the round asks its recorded cohort by name, folds the answers and adopts each register its
/// cohort's recovery quorum promised; once both are adopted and the volume is served, it is sealed under this node's
/// own placement, and once that seal is placed the move is reported adopted.
pub(crate) async fn drive_adoptions(
  origin: u16,
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let owed = state::with_state(|s| owed_adoptions(s, local)).unwrap_or_default();
  for Owed { key, cohort } in owed {
    let adopted = state::with_state(|s| {
      s.move_adoptions
        .get(&key)
        .is_some_and(|adoption| adoption.adopted(key))
    })
    .unwrap_or(false);
    if adopted {
      settle_adopted(origin, local, key, budget, in_flight).await;
      continue;
    }
    let Some(requests) = state::with_state(|s| begin_round(s, key, local, &cohort)).flatten()
    else {
      continue;
    };
    if !requests.is_empty() {
      in_flight.extend(ask(key, requests, budget).await);
    }
    let ready = state::with_state(|s| ready_registers(s, key, &cohort)).unwrap_or_default();
    for (register, accepted, epoch) in ready {
      let (dispatch, placed) =
        crate::takeover::adopt_at(local, register, accepted, epoch, local, budget).await;
      in_flight.extend(dispatch);
      if placed {
        let _ = state::with_state_counted(|s| note_adopted(s, key, register));
      }
    }
  }
}

/// What an adopted volume's owner shard reports of its seal ([`seal_adopted`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sealing {
  /// Not served yet: its materialization is still under way.
  NotServed,
  /// Sealed at this head epoch, superseding this adopted snapshot; not yet placed.
  Sealed(slates_db::catalog::SnapshotId, u64),
  /// The seal is placed under this node's placement: the move may be reported adopted.
  Placed,
  /// The shard refused the seal this period (a writer's request in flight across the barrier): asked again.
  Refused,
}

/// Once the volume is served, seals it under this node's own placement and, once that seal is placed, drops the
/// adopted snapshot it superseded and reports the move adopted, each period until the configuration shows it.
async fn settle_adopted(
  origin: u16,
  local: HostId,
  key: ObjectId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let partition = crate::verbs::owner_of(VolumeId { bytes: key.0 });
  let Some((shard, sealed)) = state::with_state(|s| {
    let shard = s.shards.get(usize::from(partition)).copied()?;
    let sealed = s
      .move_adoptions
      .get(&key)
      .and_then(|adoption| adoption.sealed);
    Some((shard, sealed))
  })
  .flatten() else {
    return;
  };
  let progress = crate::xshard::call_within(
    origin,
    shard,
    move |s| seal_adopted(s, key, sealed),
    crate::daemon::HEARTBEAT_NS,
  )
  .await;
  match progress {
    Some(Sealing::NotServed) | None => {}
    Some(Sealing::Refused) => {
      let _ = state::with_state_counted(|s| s.count(MOVE_SEAL_REFUSED, 1));
    }
    Some(Sealing::Sealed(superseded, epoch)) => {
      let _ = state::with_state_counted(|s| {
        if let Some(adoption) = s.move_adoptions.get_mut(&key) {
          adoption.sealed = Some((superseded, epoch));
        }
      });
    }
    Some(Sealing::Placed) => {
      let _ = state::with_state_counted(|s| s.count(MOVE_ADOPTION_REPORTED, 1));
      crate::fleet::send_council_report(
        slates_cluster::config_group::ConfigCommand::MoveAdopted {
          object: key,
          owner: local,
        },
        local,
        budget,
        in_flight,
      )
      .await;
    }
  }
}

/// On the adopted volume's owner shard: its seal's progress. Not served yet; else, with no seal taken, a seal now (a
/// snapshot the record plane seals and places under this node's placement); else whether that seal's head and the
/// volume's catalog are placed at `f + 1`, dropping the adopted snapshot it superseded once they are.
pub(crate) fn seal_adopted(
  state: &mut ShardState,
  key: ObjectId,
  sealed: Option<(slates_db::catalog::SnapshotId, u64)>,
) -> Sealing {
  let id = DbVolumeId { bytes: key.0 };
  let Some(catalog_version) = state
    .db
    .partition()
    .volume(id)
    .map(|record| record.catalog_version)
  else {
    return Sealing::NotServed;
  };
  let Some((superseded, epoch)) = sealed else {
    return match crate::verbs::seal_adopted_head(state, id) {
      Ok((superseded, epoch)) => Sealing::Sealed(superseded, epoch),
      Err(_) => Sealing::Refused,
    };
  };
  let quorum = state.fleet.configuration().quorum;
  let placed = |register: ObjectId, sequence: u64| {
    state
      .placed_heads
      .get(&register)
      .is_some_and(|head| head.sequence >= sequence && head.placement.placed(quorum))
  };
  if !(placed(key, epoch) && placed(key.catalog(), catalog_version)) {
    return Sealing::Sealed(superseded, epoch);
  }
  let still_held = state
    .db
    .partition()
    .snapshots_of(id)
    .iter()
    .any(|snapshot| snapshot.id == superseded);
  if still_held && crate::verbs::drop_snapshot(state, id, superseded).is_err() {
    state.count(MOVE_DROP_REFUSED, 1);
  }
  Sealing::Placed
}

/// Readies this period's round of `key`'s adoption over `cohort`, returning each cohort member's question (the
/// registers it has not answered for), or `None` while this node's placement is behind its council's or no epoch can
/// exceed a fence. This node answers itself first, from its own copies, when it is in the cohort.
fn begin_round(
  state: &mut ShardState,
  key: ObjectId,
  local: HostId,
  cohort: &[HostId],
) -> Option<Vec<(HostId, HostPrepare)>> {
  let regional = state.council.configuration().clone();
  if state.fleet.configuration().version != regional.version {
    state.count(MOVE_ROUND_BEHIND, 1);
    return None;
  }
  let first_epoch = initial_epoch(state, key)?;
  let adoption = state
    .move_adoptions
    .entry(key)
    .or_insert_with(|| MoveAdoption::new(regional.version, first_epoch));
  if adoption.generation != regional.version {
    let epoch = adoption.epoch.max(first_epoch);
    adoption.restart(regional.version, epoch);
  }
  if let Some(fence) = adoption.fenced {
    let Some(above) = fence.0.checked_add(1) else {
      state.count(MOVE_EPOCH_EXHAUSTED, 1);
      return None;
    };
    let epoch = HostEpoch(above).max(adoption.epoch);
    adoption.restart(regional.version, epoch);
  }
  let (epoch, generation) = (adoption.epoch, adoption.generation);
  if cohort.contains(&local) {
    answer_self(state, key, local, epoch, generation);
  }
  let adoption = state.move_adoptions.get(&key)?;
  let requests = cohort
    .iter()
    .copied()
    .filter(|host| *host != local && regional.members.contains(host))
    .filter_map(|host| {
      let named = unanswered(adoption, key, host);
      (!named.is_empty()).then_some((
        host,
        HostPrepare {
          departed: local,
          owner: local,
          epoch,
          generation,
          after: None,
          named,
        },
      ))
    })
    .collect();
  Some(requests)
}

/// The registers of `key` that `host` has not answered for in this round and that are not adopted.
fn unanswered(adoption: &MoveAdoption, key: ObjectId, host: HostId) -> Vec<ObjectId> {
  registers(key)
    .into_iter()
    .filter(|register| {
      !adoption.settled.contains(register)
        && !adoption
          .learned
          .get(register)
          .is_some_and(|reports| reports.contains_key(&host))
        && !adoption
          .named_nothing
          .get(register)
          .is_some_and(|hosts| hosts.contains(&host))
    })
    .collect()
}

/// The epoch a new round of `key`'s adoption starts at: above every epoch this node's copies of its registers were
/// written at, and at least this node's own fencing epoch. A holder's fence above it sends the round higher. `None`
/// when a copy was written at `u64::MAX`.
fn initial_epoch(state: &mut ShardState, key: ObjectId) -> Option<HostEpoch> {
  let own = state.fleet.configuration().host_epoch;
  let highest = registers(key)
    .into_iter()
    .filter_map(|register| state.holder_records.get(&register))
    .flat_map(|acceptor| acceptor.persisted().1)
    .try_fold(own, |highest, (_, _, epoch, _)| {
      Some(highest.max(HostEpoch(epoch.0.checked_add(1)?)))
    });
  if highest.is_none() {
    state.count(MOVE_EPOCH_EXHAUSTED, 1);
  }
  highest
}

/// This node answers its own round by name, from its own copies, as any cohort member does.
fn answer_self(
  state: &mut ShardState,
  key: ObjectId,
  local: HostId,
  epoch: HostEpoch,
  generation: u64,
) {
  let Some(adoption) = state.move_adoptions.get(&key) else {
    return;
  };
  let named = unanswered(adoption, key, local);
  if named.is_empty() {
    return;
  }
  let prepare = HostPrepare {
    departed: local,
    owner: local,
    epoch,
    generation,
    after: None,
    named,
  };
  let regional = state.council.configuration().clone();
  let answer = answer_move(
    local,
    &prepare,
    &regional,
    &mut state.holder_records,
    &BTreeMap::new(),
    usize::MAX,
  );
  if answer.refused > 0 {
    state.count(MOVE_NAMED_REFUSED, u64::from(answer.refused));
  }
  fold(state, key, vec![(local, answer.promise)]);
}

/// Sends each cohort member its question over [`MOVE_STREAM`] and folds every binding answer into `key`'s round.
async fn ask(
  key: ObjectId,
  requests: Vec<(HostId, HostPrepare)>,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let wanted: BTreeMap<HostId, HostPrepare> = requests.into_iter().collect();
  let sessions = take_sessions("fleet.lent_to.move_ask", |host| wanted.contains_key(&host));
  if sessions.is_empty() {
    let _ = state::with_state_counted(|s| s.count(MOVE_QUESTIONS_UNSENT, 1));
    return None;
  }
  let sent: Vec<HostId> = sessions.iter().map(|(host, _)| *host).collect();
  let outgoing: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
    .into_iter()
    .filter_map(|(host, endpoint)| {
      wanted
        .get(&host)
        .map(|prepare| (host, prepare.encode(), endpoint))
    })
    .collect();
  let (replied, stragglers) = broadcast(outgoing, MOVE_STREAM, Priority::Control, budget).await;
  let mut recovered = Vec::with_capacity(replied.len());
  let mut answers = Vec::new();
  let mut stale = None;
  let mut unbound: u64 = 0;
  for (host, reply, endpoint) in replied {
    if let Some(slates_db::register::Refusal::ConfigurationStale { version }) =
      slates_db::register::decode_refusal(&reply.bytes)
    {
      stale = stale.max(Some(version));
    }
    if let Ok(promise) = HostPromise::decode(&reply.bytes)
      && promise.holder == host
      && let Some(prepare) = wanted.get(&host)
      && promise.binds(prepare)
    {
      answers.push((host, promise));
    } else {
      unbound = unbound.saturating_add(1);
    }
    recovered.push((host, endpoint));
  }
  let dispatch = Dispatch::new(sent, &recovered, stragglers, LateReplies::Discard);
  return_sessions(recovered);
  state::with_state(|s| {
    if stale.is_some_and(|version| version > s.council.configuration().version) {
      s.config_refresh_wanted = true;
    }
    if unbound > 0 {
      s.count(MOVE_ANSWER_UNBOUND, unbound);
    }
    fold(s, key, answers);
  });
  Some(dispatch)
}

/// Folds members' answers into `key`'s round: each reported record learned under its holder, each register held by no
/// one noted for its holder, and a fence at or above the round's epoch kept, so the next round runs above it.
fn fold(state: &mut ShardState, key: ObjectId, answers: Vec<(HostId, HostPromise)>) {
  let Some(adoption) = state.move_adoptions.get_mut(&key) else {
    return;
  };
  for (host, promise) in answers {
    if let Some(fence) = promise.fenced
      && fence >= adoption.epoch
    {
      adoption.fenced = Some(adoption.fenced.map_or(fence, |seen| seen.max(fence)));
      continue;
    }
    for (register, highest) in promise.entries {
      if !adoption.settled.contains(&register) {
        adoption
          .learned
          .entry(register)
          .or_default()
          .insert(host, highest);
      }
    }
    for register in promise.nothing {
      if !adoption.settled.contains(&register) {
        adoption
          .named_nothing
          .entry(register)
          .or_default()
          .insert(host);
      }
    }
  }
}

/// The registers of `key` ready to adopt, each with the newest record reported and the round's epoch: the recorded
/// cohort's recovery quorum promised it ([`readiness`] under the named-answer rule, the cohort's members only). A
/// register no promising member reported a record of waits: its records were placed before the move was reported, so a
/// quorum saying nothing is a cohort that lost them, counted lost, never adopted as empty. A cohort with fewer members
/// left than the quorum is counted lost too and waits (the move stays the new owner's, its records where they are).
fn ready_registers(
  state: &mut ShardState,
  key: ObjectId,
  cohort: &[HostId],
) -> Vec<(ObjectId, Accepted, HostEpoch)> {
  let regional = state.council.configuration().clone();
  let Some(adoption) = state.move_adoptions.get_mut(&key) else {
    return Vec::new();
  };
  let mut ready = Vec::new();
  let mut lost: u64 = 0;
  let empty = BTreeMap::new();
  let none = BTreeSet::new();
  for register in registers(key) {
    if adoption.settled.contains(&register) {
      continue;
    }
    let reports = adoption.learned.get(&register).unwrap_or(&empty);
    let heard = Heard {
      reports,
      complete: &none,
      named_nothing: adoption.named_nothing.get(&register).unwrap_or(&none),
      local: None,
    };
    match readiness(
      &[cohort.to_vec()],
      |host| regional.members.contains(&host),
      regional.quorum,
      &heard,
      CountingRule::NamedAnswer,
    ) {
      Readiness::Ready => match reports.values().cloned().reduce(newest) {
        Some(newest) => ready.push((register, newest, adoption.epoch)),
        None => lost = lost.saturating_add(1),
      },
      Readiness::Lost { .. } => lost = lost.saturating_add(1),
      Readiness::Waiting => {}
    }
  }
  if lost > 0 {
    state.count(MOVE_LOST, lost);
  }
  ready
}

/// The newer of two reported records by phase one's order (sequence, then epoch).
fn newest(best: Accepted, next: Accepted) -> Accepted {
  if next.newer_than(&best) { next } else { best }
}

/// Records `register` of `key` adopted: the takeover's adoption placed it under this node (one short of its quorum is
/// counted there and asked again next period).
fn note_adopted(state: &mut ShardState, key: ObjectId, register: ObjectId) {
  let Some(adoption) = state.move_adoptions.get_mut(&key) else {
    return;
  };
  let before = adoption.adopted(key);
  adoption.settled.insert(register);
  adoption.learned.remove(&register);
  if !before && adoption.adopted(key) {
    state.count(MOVE_ADOPTED, 1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_db::register::Quorum;

  /// Shape: the scatter width the regions of these tests are formed with (every member a neighbour of every other).
  const SCATTER: u64 = 0;

  /// A three-member region at `f = 1`, every member in every neighbourhood.
  fn region() -> RegionalConfiguration {
    RegionalConfiguration::formed(
      (1..=3).map(HostId).collect(),
      Quorum { f: 1 },
      BTreeMap::new(),
      SCATTER,
      false,
    )
  }

  fn begun(object: ObjectId, to: HostId) -> Outgoing {
    Outgoing {
      object,
      to,
      reported: false,
      switched: false,
      ready: false,
      overdue: false,
      cohort: vec![HostId(1), HostId(2), HostId(3)],
    }
  }

  /// A-291, the old owner's decision, by the configuration alone. Do: decide host 1's move of its object to host 2 as
  /// it is unplaced, placed, overdue, and then reported; commit the move and decide again; report it adopted and decide
  /// again; drop the exception as its new owner's destroy does and decide again; then decide a second move whose target
  /// retired before any switch, and a move whose cohort the target's neighbourhood does not cover. Expect: wait while
  /// unplaced; report once placed and again until the switch; refused `Unplaced` when overdue unreported; switched to
  /// host 2 (keeping the copy) once committed; released once adopted; released to itself (the copy let go, never
  /// thawed) once the switched move's exception is gone; thawed when the target left with no switch; refused
  /// `Neighbourhood` for an uncovered cohort.
  #[test]
  fn the_old_owner_reports_until_the_switch_keeps_its_copy_until_adopted_and_thaws_only_without_a_switch()
   {
    let mut regional = region();
    let (from, to) = (HostId(1), HostId(2));
    let object = ObjectId::new(from, 7);
    let mut out = begun(object, to);
    assert_eq!(step_for(&regional, from, &out), Step::Wait);
    out.overdue = true;
    assert_eq!(
      step_for(&regional, from, &out),
      Step::Refuse(MoveRefusal::Unplaced)
    );
    out.overdue = false;
    out.ready = true;
    assert_eq!(step_for(&regional, from, &out), Step::Report);
    out.reported = true;
    out.overdue = true;
    assert_eq!(
      step_for(&regional, from, &out),
      Step::Report,
      "a reported move is never refused: it can still commit"
    );
    assert!(regional.move_object(object, from, to, &out.cohort));
    assert_eq!(
      step_for(&regional, from, &out),
      Step::Switched { owner: to }
    );
    out.switched = true;
    assert!(regional.adopt_move(object, to));
    assert_eq!(step_for(&regional, from, &out), Step::Release { owner: to });
    let mut gone = regional.clone();
    assert!(gone.forget_moved(object, to));
    assert_eq!(
      step_for(&gone, from, &out),
      Step::Release { owner: from },
      "a switched move whose exception is gone (destroyed by its new owner, or handed back by its takeover) lets the \
       frozen copy go, never thaws it"
    );

    let mut leaving = region();
    let other = ObjectId::new(from, 8);
    let mut out = begun(other, HostId(3));
    out.ready = true;
    out.reported = true;
    assert!(leaving.retire(HostId(3), SCATTER));
    assert_eq!(
      step_for(&leaving, from, &out),
      Step::Thaw,
      "no entry can move it to a host that left"
    );

    let mut out = begun(ObjectId::new(from, 9), to);
    out.ready = true;
    out.cohort = vec![from, HostId(9)];
    assert_eq!(
      step_for(&region(), from, &out),
      Step::Refuse(MoveRefusal::Neighbourhood),
      "the target's takeover could not reach a cohort its neighbourhood leaves out"
    );
  }
}
