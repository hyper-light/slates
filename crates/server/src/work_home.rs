//! A green's works are served where they live; only the verdict is the green's (§4.16, A-247).
//!
//! A work lives on the partition its name hashes to and its verbs arrive there. Everything about a work but its base
//! and its verdict is on that shard, and is served as any verb is, in one synchronous step. Two things are the
//! green's: the base's bytes of a file the work has not changed, and the verdict. When the green lives on another
//! shard a verb needing either is parked here at `dispatch`'s gate, the ask runs as a task of this shard, and the
//! answer resumes the verb under its own completion key (the `lease_wait` pattern), so no verb awaits and no await
//! holds a partition's state.
//!
//! **Order.** While one verb of a work is parked, every later verb of that work parks behind it, and they resume in
//! arrival order. So an edit never overtakes a submit pipelined after it, and no edit lands during a submit's verdict.
//!
//! **A green on this shard** is asked directly, by the same functions (`verbs::judge_submission` and its siblings), in
//! the verb's own step: the laptop's one-shard daemon runs none of this module's parking (R8: the degenerate of the
//! same code, `f = 0`, one shard).
//!
//! **Exactly once.** The owner records a submit's completion under the request's key, so a retry after a crash on the
//! work's shard answers the first verdict, whatever the work then holds; the work's shard settles the answer and
//! records it again on its own partition, in one transaction with the work's reset, so a retry there meets that. A
//! re-seal under a new request id is answered by the engine's identity cache (`Green::accepted`).
//!
//! **Bounds.** A shard's parked verbs are bounded by its client credit, as `lease_wait`'s are. A batch of asks waits
//! for room on its owner's channel and for the owner's answer in the owner's own time (A-426); a verb whose verdict the
//! owner answers at its quorum commit, or that waits for its work's route, is held at most the node's failover delay,
//! a held reply's total (A-354, A-375, A-434; [`park_bound_ns`]); past either, the verb answers `Overloaded`,
//! unrecorded, and its retry runs it again.
//!
//! **Asks are batched.** A shard keeps at most two batches of asks out to each owner at a time ([`Outbox`],
//! [`BATCHES_OUT`]), each carrying what gathered while the others were out, at most a client region's credit, and a batch
//! waits for room on the owner's channel rather than failing. One message per ask flooded the owner's and the clients'
//! channels at 3,000 clients on one node: about half the asks were refused at once, their retries refused in turn, and
//! replies to resumed verbs were dropped (23 to 62 a run), so every run stalled. Batched, ten of ten runs finished with
//! nothing refused or dropped. One batch out at a time made an ask wait for the batch before it to come back before it
//! left, about half of its way to the owner at 1,000 clients; with two, one waits at the owner while the other is served
//! (A-388).
//!
//! **A submit's route first (A-298).** A version's merge record carries its submit's completion, and A-255 ships it only
//! once the submitting work's catalog register, which names the green, is committed: after a takeover that register is
//! the only route a resend has to the completion. The register is the work's, held here, and the owner's partition has
//! no record of a work on another shard, so the owner's own gate cannot see it. So the work's shard asks the owner for
//! a verdict only once the register is committed: a submit waits here for it, the register ships at once, and the
//! record plane's next pass after its commit runs the submit again ([`routes_placed`]). On a laptop the route is
//! committed at once and nothing waits.
//!
//! **A creation holds its id and name.** A work's id is the sequence of the transaction that creates it, and a verb
//! parked for its owner commits nothing, so creations parked in one step were all given one id, queued behind it and
//! looped a shard. A creation's own transaction now records it pending (`Op::WorkCreationPending`), which consumes the
//! id and holds the name until the owner's answer is settled.
//!
//! The base a verb fetched is held for that verb alone ([`crate::work_files::FetchedBase`]): nothing of the green's is
//! cached on a work's shard, so a base's bytes never need invalidating and the work's charge is unchanged.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use slates_db::catalog::{Principal, Role, VolumeId as DbVolumeId};
use slates_db::op::Op;
use slates_ipc::protocol::{EditBytes, ReadAt, Refusal, ReplyBody, RequestBody, WorkOp};
use slates_merge::increment::VolumeOp;
use slates_vfs::clock::Clock;
use slates_wire::request::RequestId;

use crate::lease_wait::ResumedVerb;
use crate::merge_service::{ReplyRoute, canonical_path, page_room, require_work};
use crate::state::{self, ShardState};
use crate::work_files::{Base, FetchedBase, WorkFiles};

/// Format: the counter a work's shard moves for each ask of its green's owner; the non-vacuity count of this module.
pub(crate) const ASKED_THE_OWNER: &str = "merge.work.asked_owner";
/// Format: a deferred verdict resumed its parked verb when its record committed at the quorum (AUD-11), the non-vacuity
/// count of [`answered_late`].
pub(crate) const ANSWERED_LATE: &str = "merge.work.answered_late";
/// Format: a work's pending submit settled ahead of another verb of the work (A-247), the non-vacuity count of
/// [`settle_pending_first`].
pub(crate) const PENDING_SETTLED_FIRST: &str = "merge.work.pending_settled_first";
/// Format: a pending submit run again that did not reach the owner, so the verb behind it was answered retryable.
const PENDING_STUCK: &str = "merge.work.pending_stuck";
/// Format: a pending submit whose request could not be decoded; the work's verbs then run as though it had none.
const PENDING_UNREADABLE: &str = "merge.work.pending_unreadable";
/// Format: a work's admission asked of its green's owner on another node (A-697 part a), the non-vacuity count of
/// [`admit_over_fleet`].
pub(crate) const ASKED_THE_OWNER_NODE: &str = "merge.work.asked_owner_node";
/// Format: a base read of a work on a holder node answered by the node's replica of its green (A-697 part a), the
/// non-vacuity count of [`answer_on_owner`]'s replica reads.
pub(crate) const READ_FROM_REPLICA: &str = "merge.work.read_from_replica";
/// Format: a base read the node's replica could not answer, holding no such green or not yet the work's base version
/// (A-697 part a): answered retryable, unrecorded.
pub(crate) const REPLICA_BEHIND: &str = "merge.work.replica_behind";
/// Format: a floor of a work on a holder node moved or dropped at its green's owner on another node (A-697 part b), the
/// non-vacuity count of [`tell_floor_over_fleet`].
pub(crate) const FLOOR_TOLD_OWNER_NODE: &str = "merge.work.floor_told_owner_node";
/// Format: an admission over the fleet whose creation was not committed when its ask was to leave: not asked.
const ADMISSION_UNSTAGED: &str = "merge.work.admission_unstaged";
/// Format: a work's increment composed on its own shard (A-247), the non-vacuity count of `composed`.
pub(crate) const COMPOSED_ON_WORK: &str = "merge.work.composed_on_work";
/// Counter: a work's journal holding a copy (A-626 part 4) composed by its green's owner, against `Green::base_for`,
/// which puts each copied tree at its destination. The work's own shard composes none: the entries it fetches are a
/// path's each, never a tree (A-247), so it fetches nothing for such a journal and leaves its composition to the owner.
pub(crate) const COPY_COMPOSED_BY_OWNER: &str = "merge.work.copy_composed_by_owner";

/// Whether a work's journal declares a copy (A-626 part 4).
pub(crate) fn holds_copy(journal: &[VolumeOp]) -> bool {
  journal.iter().any(|op| matches!(op, VolumeOp::Copy { .. }))
}
/// Format: a submit held on its work's shard until the work's catalog register, its route, was committed, before the
/// green's owner was asked for a verdict (A-298), the non-vacuity count of [`await_route`].
pub(crate) const AWAITED_ROUTE: &str = "merge.work.awaited_route";
/// Format: a work's increment left to its green's owner to compose: its journal names a base it could not assemble, or
/// does not compose (the owner then refuses it, typed).
const COMPOSED_BY_OWNER: &str = "merge.work.composed_by_owner";
/// Format: base entries a work fetched and could not keep, its charge refused; the verb is refused `BudgetExceeded`.
const ENTRIES_REFUSED: &str = "merge.work.entries_refused";
/// Format: a verb refused because the shard's parked verbs are at their bound.
const PARK_FULL: &str = "merge.work.park_full";
/// Format: an ask of the green's owner unanswered within its deadline; the verb answers `Overloaded` and is retried.
const ASK_UNANSWERED: &str = "merge.work.ask_unanswered";
/// Format: a floor move or release that never reached the green's owner; the floor stays where it was, which only
/// keeps more history, and the next accept or the next start's reconcile corrects it.
const FLOOR_NOT_SENT: &str = "merge.work.floor_not_sent";

/// What a work moves to its green's owner to be judged or rebased: everything its increment is composed from.
pub(crate) struct SealedWork {
  /// The work.
  pub(crate) work: DbVolumeId,
  /// Its green.
  pub(crate) green: DbVolumeId,
  /// The version it is based on.
  pub(crate) base_version: u64,
  /// Its declared operations.
  pub(crate) journal: Vec<VolumeOp>,
  /// The files it changed.
  pub(crate) files: WorkFiles,
  /// The increment composed from them on the work's shard, when it was (A-247): the owner then only judges it, and the
  /// journal and files are not sent.
  pub(crate) composed: Option<slates_merge::engine::Increment>,
  /// The verdict of `composed` a holder computed on its replica, in its wire form (A-658 part 3a), when the work lives
  /// on a node that holds a replica of a green another node owns (A-697 part c): the owner takes it by position.
  pub(crate) computed: Option<Vec<u8>>,
}

/// What a clean rebase leaves a work, or the answer when it was not clean.
pub(crate) enum RebaseAnswer {
  /// The work moves to `version`, holding `changed` with `journal`.
  Rebased {
    /// The green's head the work is now based on.
    version: u64,
    /// The files that still differ from the head.
    changed: BTreeMap<String, Option<Vec<u8>>>,
    /// The operations restated over the head.
    journal: Vec<VolumeOp>,
    /// The base entries at the head of every path the restated journal names, for a work that composes its own
    /// increment (A-247), so its next submit does not fetch them; empty for a work on the green's shard.
    entries: BTreeMap<String, slates_merge::increment::Base>,
  },
  /// A conflict or a refusal: the reply, with the work unchanged.
  Reply(ReplyBody),
}

/// The owner's answer to a work's creation: the work's id, minted before the ask, and its base.
pub(crate) struct Admitted {
  /// The work's id.
  pub(crate) id: DbVolumeId,
  /// The green's head, which the work is based on.
  pub(crate) base: u64,
}

/// One verb parked on this shard.
struct ParkedVerb {
  verb: ResumedVerb,
  fetched: Option<FetchedBase>,
  verdict: Option<ReplyBody>,
  rebased: Option<RebaseAnswer>,
  admitted: Option<Result<Admitted, Refusal>>,
  named: Option<Result<(), Refusal>>,
}

/// A work's parked verbs, oldest first, and whether the oldest's ask is out.
#[derive(Default)]
struct Queue {
  waiting: VecDeque<ParkedVerb>,
  asking: bool,
  /// When the owner took the oldest verb's verdict to answer at its quorum commit, or the verb waits for its work's
  /// route (A-298): the instant (the host's monotonic clock) past which the verb stops waiting, is answered
  /// `Overloaded`, and the work is released (its retry joins the owner, or meets the route again).
  deferred_until: Option<u64>,
}

/// One work whose log could not be replayed at its shard's start because its green lives on another shard (A-247):
/// replayed through the green's owner once every partition has recovered (`rebuild_remote_works`). The record stays on
/// the work's shard until the replay's outcome is installed there (`verbs::install_replayed_work`), so an ask whose
/// answer was lost finds it again (A-513).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unbuilt {
  /// The work.
  pub(crate) work: DbVolumeId,
  /// Its green.
  pub(crate) green: DbVolumeId,
  /// The version it was created at.
  pub(crate) created_at: u64,
  /// Whether its green is another node's (A-697 part e): a work this node made as a holder, replayed over this node's
  /// replica of the green ([`holder_replay`]) rather than through an owner shard of this node.
  pub(crate) holder: bool,
}

/// A shard's parked work verbs and the answer for the verb now running.
#[derive(Default)]
pub(crate) struct WorkHome {
  queues: BTreeMap<DbVolumeId, Queue>,
  by_request: BTreeMap<(u64, u32, u32), DbVolumeId>,
  parked: usize,
  /// The work whose parked verb is being resumed now: it may park again at the front of its own queue.
  resuming: Option<DbVolumeId>,
  /// The pending submits already run again in this resume pass: one that ran and is still pending was not answered by
  /// the owner, and running it again in the same pass would loop.
  settling: Vec<(u64, u32, u32)>,
  /// What the owner answered for the verb now running: a base the verb reads.
  pub(crate) fetched: Option<FetchedBase>,
  /// … a verdict the verb settles.
  pub(crate) verdict: Option<ReplyBody>,
  /// … a rebase the verb settles.
  pub(crate) rebased: Option<RebaseAnswer>,
  /// … a work's admission.
  pub(crate) admitted: Option<Result<Admitted, Refusal>>,
  /// … the reservation of a new work's name at the partition it hashes to, when the work is made on another (A-400).
  pub(crate) named: Option<Result<(), Refusal>>,
  /// The works whose log waits for their green's owner (A-247), each until its replay's outcome is installed here
  /// (A-513). Bounded by the shard's recovered works.
  pub(crate) unbuilt: Vec<Unbuilt>,
  /// Whether this shard's deadline timer for deferred verdicts is running.
  timer_armed: bool,
  /// The asks waiting to go to each owner shard (A-247).
  outboxes: BTreeMap<u16, Outbox>,
  /// The works whose oldest verb waits for the work's route (A-298): a subset of `queues`' keys, so bounded as the
  /// parked verbs are, and each leaves it when its queue is resolved ([`resolve`]).
  routing: BTreeSet<DbVolumeId>,
  /// Test support (`Daemon::lose_next_owner_answer`): the next verdict or admission an ask brings back is lost.
  pub(crate) lose_next_answer: bool,
  /// Test support (`Daemon::serve_works_on_holders`; A-697, until its last piece puts the rule that a node holding a
  /// green's replica serves its works in its place): this node serves the works of greens another node owns
  /// ([`green_elsewhere`]). Never set from the wire; off, nothing here changes.
  pub(crate) on_holders: bool,
  /// On the control shard, which holds the node's replicas: the submits of works on this holder node whose versions the
  /// owner committed before the replica held them (A-697 part d), each answered once the replica does. Bounded by the
  /// node's parked verbs: one a parked submit, each let go past its verb's bound ([`hold_until_replica`]).
  replica_waits: Vec<ReplicaWait>,
  /// On the control shard: the greens another node owns whose replica the replay of a recovered holder work waited for
  /// (A-697 part e), each with the shards whose works wait; once the replica is installed each shard asks again
  /// ([`replica_reached`]). Bounded by the greens this node replicates times its shards.
  replays_owed: BTreeMap<slates_db::register::ObjectId, BTreeSet<u16>>,
  /// The holder creations a restart caught waiting for their admission (A-697 part e2), by name: each keeps its pending
  /// record, so its name and id stay held, until its green's owner has released the floor it may have recorded for the
  /// work ([`release_admissions`]); `true` while a release is out. Bounded by the partition's pending creations.
  pub(crate) releasing: BTreeMap<String, bool>,
}

/// A submit of a work on this holder node whose version its green's owner committed before this node's replica held it
/// (A-697 part d): its answer, held until the replica holds the version, so the work it settles reads its new base here.
struct ReplicaWait {
  /// The green.
  green: slates_db::register::ObjectId,
  /// The version the owner committed.
  version: u64,
  /// The work's shard, where the submit is parked.
  shard: u16,
  /// The submit's completion key: its origin.
  origin: u64,
  /// … and its request.
  id: RequestId,
  /// The owner's answer.
  reply: ReplyBody,
  /// Past this instant (the host's monotonic clock) the parked submit has been answered retryable and released.
  until_ns: u64,
  /// When it was held, on the same clock: what [`SUBMIT_SETTLE`] times.
  held_ns: u64,
}

/// What [`gate`] decided for a verb.
pub(crate) enum Gate {
  /// Run it now, here.
  Run(RequestBody),
  /// It was parked (or refused); this is the reply `dispatch` returns.
  Answer(ReplyBody),
}

/// What a verb needs of the owner. A sealed work is boxed: it is far larger than any other need.
enum Need {
  Files(Vec<String>),
  Page {
    path: String,
    offset: u64,
    want: u64,
  },
  Sizes,
  Verdict {
    evidence: Vec<[u8; 32]>,
    sealed: Box<SealedWork>,
    /// The work's merge mode and the launcher's approval, if any (A-285): the green's owner holds no record of the work.
    policy: crate::verbs::SubmitPolicy,
  },
  Rebase {
    sealed: Box<SealedWork>,
  },
  Admit {
    green: DbVolumeId,
    id: DbVolumeId,
    name: String,
  },
  /// The name of a work made on this shard, held at the partition it hashes to by an alias of the work's id (A-400,
  /// A-236's reservation), asked of that partition's shard before the work is admitted.
  ReserveName {
    name: String,
    id: DbVolumeId,
  },
  /// The work's route (A-255, A-298): its catalog register, which names its green, committed at its quorum. Met on the
  /// work's own shard, which holds the register, before the owner is asked for a verdict, so the merge record that
  /// carries the submit's completion never exists before its route does.
  Route,
}

fn key(origin: u64, id: RequestId) -> (u64, u32, u32) {
  (origin, id.client, id.sequence)
}

/// The most verbs a shard parks: its client credit (`clients_per_shard × slots`), as `lease_wait`.
fn bound(state: &ShardState) -> usize {
  state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1))
}

/// The work a verb acts on, when it is a work on this shard whose green lives on another: `None` for every other verb,
/// which `dispatch` runs as ever.
fn remote_work(state: &ShardState, body: &RequestBody) -> Option<(DbVolumeId, DbVolumeId)> {
  let volume = crate::verbs::to_db_volume(body.volume()?);
  let record = state.db.partition().volume(volume)?;
  let Role::Work { green, .. } = record.policy.role else {
    return None;
  };
  (!state.greens.contains_key(&green)).then_some((volume, green))
}

/// Whether `green` is owned by another node while this node serves works over it (A-697 part a): only with the test hook
/// on ([`WorkHome::on_holders`]) until A-697's last piece puts the real rule ("replicated here") in its place. The owner
/// is the one this node's routing names, else the green's creator; a green on this shard never is.
pub(crate) fn green_elsewhere(state: &ShardState, green: DbVolumeId) -> bool {
  state.work_home.on_holders && owned_elsewhere(state, green)
}

/// Whether `green` is owned by another node, as this node's routing names its owner, else by its creator (A-697): a
/// green this shard holds never is. Unlike [`green_elsewhere`] it holds whatever the test hook says, so a work this node
/// made as a holder is recovered as one after a restart, which starts with the hook off (part e).
pub(crate) fn owned_elsewhere(state: &ShardState, green: DbVolumeId) -> bool {
  if state.greens.contains_key(&green) {
    return false;
  }
  let object = slates_db::register::ObjectId(green.bytes);
  state
    .fleet
    .object_owner(object)
    .unwrap_or_else(|| object.creator())
    != state.fleet.host()
}

/// Whether the caller's rights let the verb reach the owner; a verb refused here runs, and its own check answers.
fn may_ask(state: &ShardState, principal: &Principal, body: &RequestBody) -> bool {
  let Some(volume) = body.volume() else {
    return false;
  };
  match body {
    RequestBody::Submit { .. }
    | RequestBody::SubmitSigned { .. }
    | RequestBody::SubmitHeld { .. }
    | RequestBody::Rebase { .. } => require_work(state, principal, volume, false, "ask").is_ok(),
    // An approval reaches the verdict only once it verifies (A-285): the owner trusts what it is sent, so a consumer's
    // or a forged approval runs here, where `verbs::submit_approved` refuses it, and nothing reaches the green.
    RequestBody::SubmitApproved {
      work,
      identity,
      proof,
      ..
    } => {
      require_work(state, principal, volume, false, "ask").is_ok()
        && crate::verbs::approval_verifies(state, principal, *work, (identity, proof))
    }
    RequestBody::Edit { .. }
    | RequestBody::EditStaged { .. }
    | RequestBody::EditSubmit { .. }
    | RequestBody::EditSubmitHeld { .. }
    | RequestBody::Declare { .. } => require_work(state, principal, volume, true, "ask").is_ok(),
    _ => crate::merge_service::find_record(state, volume)
      .is_ok_and(|record| crate::verbs::rights_of(&record, principal).read),
  }
}

/// What the verb needs of the owner that it does not already have, given what the verb now holds in `state.work_home`.
fn need_of(state: &ShardState, work: DbVolumeId, body: &RequestBody) -> Option<Need> {
  let w = state.works.get(&work)?;
  let home = &state.work_home;
  let fetched = home.fetched.is_some();
  let file_if_unheld = |path: &str| {
    let key = canonical_path(path);
    (!fetched && !w.files.holds(key)).then(|| Need::Files(vec![key.to_owned()]))
  };
  match body {
    RequestBody::Edit { path, .. } | RequestBody::EditStaged { path, .. } => file_if_unheld(path),
    RequestBody::ReadRange {
      path,
      at: ReadAt::Head,
      offset,
      max,
      ..
    } => page_if_unheld(w, fetched, path, *offset, (*max).min(page_room())),
    RequestBody::ReadWindow {
      path,
      at: ReadAt::Head,
      offset,
      max,
      ..
    } => page_if_unheld(
      w,
      fetched,
      path,
      *offset,
      (*max).min(page_room().max(crate::verbs::read_window_bytes(state))),
    ),
    RequestBody::ReadDir {
      at: ReadAt::Head, ..
    }
    | RequestBody::Status { .. } => (!fetched).then_some(Need::Sizes),
    RequestBody::Declare { op, .. } => declare_need(fetched, op),
    RequestBody::Submit { .. }
    | RequestBody::SubmitSigned { .. }
    | RequestBody::SubmitHeld { .. }
    | RequestBody::SubmitApproved { .. } => {
      if home.verdict.is_some() {
        return None;
      }
      let (evidence, approval, signature) = submitted(body)?;
      let authorship = body.authorship().cloned().unwrap_or_default();
      Some(verdict_need(
        state,
        work,
        seal(state, work),
        (evidence, approval, signature, authorship),
      ))
    }
    RequestBody::Rebase { .. } => {
      if home.rebased.is_some() {
        return None;
      }
      let sealed = seal(state, work);
      Some(
        base_missing(state, &sealed).unwrap_or_else(|| Need::Rebase {
          sealed: Box::new(composed(state, sealed, Vec::new())),
        }),
      )
    }
    RequestBody::EditSubmit {
      path,
      at,
      delete_len,
      insert,
      evidence,
      ..
    }
    | RequestBody::EditSubmitHeld {
      path,
      at,
      delete_len,
      insert,
      evidence,
      ..
    } => {
      if let Some(need) = file_if_unheld(path) {
        return Some(need);
      }
      if home.verdict.is_some() {
        return None;
      }
      let approval = if matches!(body, RequestBody::EditSubmitHeld { .. }) {
        crate::verbs::Approval::Held
      } else {
        crate::verbs::Approval::None
      };
      sealed_with_edit(state, work, (path, *at, *delete_len, insert)).map(|sealed| {
        let authorship = body.authorship().cloned().unwrap_or_default();
        verdict_need(state, work, sealed, (evidence, approval, None, authorship))
      })
    }
    _ => None,
  }
}

/// What a submit verb brings its verdict (A-285, A-321): the evidence it names, its approval and its signature's bytes.
type Submitted<'body> = (&'body [[u8; 32]], crate::verbs::Approval, Option<Vec<u8>>);

/// What a submit verb brings its verdict: its evidence, its approval and its signature (A-285, A-321); `None` for a
/// verb that is not a submit.
fn submitted(body: &RequestBody) -> Option<Submitted<'_>> {
  use crate::verbs::Approval;
  match body {
    RequestBody::Submit { evidence, .. } => Some((evidence, Approval::None, None)),
    RequestBody::SubmitSigned {
      evidence,
      signature,
      ..
    } => Some((evidence, Approval::None, Some(signature.clone()))),
    RequestBody::SubmitHeld {
      evidence,
      signature,
      ..
    } => Some((evidence, Approval::Held, signature.clone())),
    RequestBody::SubmitApproved {
      evidence,
      identity,
      signature,
      ..
    } => Some((evidence, Approval::Of(*identity), signature.clone())),
    _ => None,
  }
}

/// What a submit of `sealed` needs of the owner, once it has what it reads: its work's route first (A-298), then the
/// base entries its composition lacks, then the verdict, carrying the work's merge mode and signed-work rules, read here
/// where the work's record is, and the approval and the signature the submit carries (A-285, A-321).
fn verdict_need(
  state: &ShardState,
  work: DbVolumeId,
  sealed: SealedWork,
  (evidence, approval, signature, authorship): (
    &[[u8; 32]],
    crate::verbs::Approval,
    Option<Vec<u8>>,
    slates_ipc::protocol::Authorship,
  ),
) -> Need {
  if !route_committed(state, work) {
    return Need::Route;
  }
  base_missing(state, &sealed).unwrap_or_else(|| Need::Verdict {
    evidence: evidence.to_vec(),
    sealed: Box::new(composed(state, sealed, evidence.to_vec())),
    policy: crate::verbs::SubmitPolicy::of_work(state, work, (approval, signature, authorship)),
  })
}

fn page_if_unheld(
  w: &crate::state::WorkState,
  fetched: bool,
  path: &str,
  offset: u64,
  want: u64,
) -> Option<Need> {
  let key = canonical_path(path);
  (!fetched && !w.files.holds(key)).then(|| Need::Page {
    path: key.to_owned(),
    offset,
    want,
  })
}

/// A declaration reads the base's file only where it removes or moves one.
fn declare_need(fetched: bool, op: &WorkOp) -> Option<Need> {
  if fetched {
    return None;
  }
  match op {
    WorkOp::Unlink { path } => Some(Need::Files(vec![canonical_path(path).to_owned()])),
    WorkOp::Rename { from, to } if from != to => {
      Some(Need::Files(vec![canonical_path(from).to_owned()]))
    }
    _ => None,
  }
}

/// The paths a sealed work's journal names whose base the work does not hold, to fetch before it composes (A-247): a
/// path whose base entries it has not fetched, or one it neither holds nor has fetched the bytes of. `None` when it has
/// all it needs, and after this verb's fetch.
fn base_missing(state: &ShardState, sealed: &SealedWork) -> Option<Need> {
  // A journal holding a copy is composed by the owner, which holds the copied trees (`COPY_COMPOSED_BY_OWNER`).
  if state.work_home.fetched.is_some() || holds_copy(&sealed.journal) {
    return None;
  }
  let w = state.works.get(&sealed.work)?;
  let named: std::collections::BTreeSet<&str> = sealed
    .journal
    .iter()
    .flat_map(VolumeOp::names)
    .flatten()
    .collect();
  let missing: Vec<String> = named
    .into_iter()
    .filter(|path| !w.entries.contains_key(*path) || !sealed.files.holds(path))
    .map(str::to_owned)
    .collect();
  (!missing.is_empty()).then_some(Need::Files(missing))
}

/// `sealed` with its increment composed on this shard, from the entries the work fetched and its files (over the bytes
/// this verb fetched, for a file it does not hold), and its journal and files left behind: the owner only judges it.
/// Composed by the function the owner composes by (`verbs::compose_increment`), so the increment is the same.
fn composed(state: &ShardState, mut sealed: SealedWork, evidence: Vec<[u8; 32]>) -> SealedWork {
  let Some(w) = state.works.get(&sealed.work) else {
    return sealed;
  };
  if holds_copy(&sealed.journal) {
    return sealed;
  }
  let named: std::collections::BTreeSet<&str> = sealed
    .journal
    .iter()
    .flat_map(VolumeOp::names)
    .flatten()
    .collect();
  let Some(base) = slates_merge::increment::Base::for_names(&named, &w.entries) else {
    return sealed;
  };
  let files_base = Base {
    green: None,
    fetched: state.work_home.fetched.as_ref(),
    version: sealed.base_version,
  };
  let Ok(inc) = crate::verbs::compose_increment(
    (sealed.work, sealed.base_version),
    &base,
    (&sealed.files, files_base),
    &sealed.journal,
    evidence,
  ) else {
    // A journal that does not compose is the owner's to refuse, typed, as it always was.
    return sealed;
  };
  sealed.composed = Some(inc);
  sealed.journal = Vec::new();
  sealed.files = WorkFiles::default();
  sealed
}

/// A copy of the work as it now stands, for the owner. A copy, not a move: an ask can go unanswered, and the work must
/// still be whole when it does.
fn seal(state: &ShardState, work: DbVolumeId) -> SealedWork {
  let w = state.works.get(&work);
  SealedWork {
    work,
    green: w.map_or(work, |w| w.green),
    base_version: w.map_or(0, |w| w.base_version),
    journal: w.map(|w| w.journal.clone()).unwrap_or_default(),
    files: w.map(|w| w.files.clone()).unwrap_or_default(),
    composed: None,
    computed: None,
  }
}

/// The work as an edit-and-submit's edit would leave it (A-225): the edit is applied to what goes to the owner, not to
/// the work, so nothing of it is durable until the answer. `None` when the edit cannot be made (a staged buffer not
/// there, a base unreadable): the verb then runs and its own edit answers.
fn sealed_with_edit(
  state: &ShardState,
  work: DbVolumeId,
  (path, at, delete_len, insert): (&str, u64, u64, &EditBytes),
) -> Option<SealedWork> {
  let mut sealed = seal(state, work);
  let key = canonical_path(path);
  let staged;
  let bytes: &[u8] = match insert {
    EditBytes::Inline { bytes } => bytes,
    EditBytes::Staged { token } => {
      staged = state.staging.peek(work, *token)?;
      staged
    }
  };
  let base = Base {
    green: None,
    fetched: state.work_home.fetched.as_ref(),
    version: sealed.base_version,
  };
  let old = sealed.files.get(base, key).ok()?;
  let inserted = u64::try_from(bytes.len()).ok()?;
  let ops = crate::work_log::edit_ops(key, old.as_deref(), (at, delete_len, inserted));
  drop(old);
  crate::work_log::apply_edit(
    (&mut sealed.files, &mut sealed.journal),
    base,
    (key, at, delete_len, bytes),
    ops,
  )
  .ok()?;
  Some(sealed)
}

/// The gate of `dispatch` for works whose green lives on another shard, and for the creation of a work there.
pub(crate) fn gate(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> Gate {
  if let Some(refusal) = unbuilt_refusal(state, &body) {
    return Gate::Answer(crate::verbs::refused(refusal));
  }
  if let RequestBody::CreateWork { green, name } = &body {
    return match create_need(state, crate::verbs::to_db_volume(*green), name) {
      Ok(None) => Gate::Run(body),
      Ok(Some(need)) => park(state, client_id, principal, body, None, Some(need)),
      Err(refusal) => Gate::Answer(crate::verbs::refused(refusal)),
    };
  }
  let Some((work, _green)) = remote_work(state, &body) else {
    return Gate::Run(body);
  };
  if let Some(gate) = settle_pending_first(state, (client_id, principal), &body, work) {
    return gate;
  }
  let queued = state.work_home.resuming != Some(work) && state.work_home.queues.contains_key(&work);
  if let Some(refusal) = reset_refusal(state, work, &body) {
    return Gate::Answer(crate::verbs::refused(refusal));
  }
  let need = if may_ask(state, principal, &body) {
    let started = slates_machine::clock::monotonic_ns();
    let need = need_of(state, work, &body);
    // A submit's composition is timed (A-697's measured breakdown): it is most of what a verdict need costs here.
    if matches!(need, Some(Need::Verdict { .. })) {
      let composed_ns = slates_machine::clock::monotonic_ns().saturating_sub(started);
      crate::merge_service::count_phase(state, SUBMIT_COMPOSE, composed_ns);
    }
    need
  } else {
    None
  };
  if !queued && need.is_none() {
    return Gate::Run(body);
  }
  park(state, client_id, principal, body, Some(work), need)
}

/// Format: the refusal-ledger name of a verb of a work recorded unbuilt, answered `Unpublished` (A-533): the
/// non-vacuity count of [`unbuilt_refusal`].
const UNBUILT_UNPUBLISHED: &str = "merge.work_unbuilt_unpublished";

/// A verb of a work this shard recovered and has not yet rebuilt through its green's owner (its record in `unbuilt`,
/// A-247, A-513), answered `Unpublished`: the protocol's "not completed, retry under the same id", never recorded as
/// the request's completion (`verbs::not_started`), so the resend runs the verb once the work is rebuilt (§4.16
/// "Recovery": the work "is unserved (`Unpublished`) until it has"). Until A-533 it ran, and answered `NotFound`, as if
/// the work were gone. Every verb naming the work is answered so, whatever shard holds its green.
fn unbuilt_refusal(state: &mut ShardState, body: &RequestBody) -> Option<Refusal> {
  let volume = crate::verbs::to_db_volume(body.volume()?);
  if !state
    .work_home
    .unbuilt
    .iter()
    .any(|unbuilt| unbuilt.work == volume)
  {
    return None;
  }
  state.count(UNBUILT_UNPUBLISHED, 1);
  Some(Refusal::Unpublished {
    reason: "the work is rebuilt from its log once its green's owner answers".to_owned(),
  })
}

/// A submit's reset must fit the work's log before anything is asked of the owner, as before the verdict when the two
/// were one step (A-207): a submit that cannot be recorded is refused without the green moving.
fn reset_refusal(state: &mut ShardState, work: DbVolumeId, body: &RequestBody) -> Option<Refusal> {
  if !matches!(
    body,
    RequestBody::Submit { .. }
      | RequestBody::SubmitSigned { .. }
      | RequestBody::SubmitHeld { .. }
      | RequestBody::SubmitApproved { .. }
      | RequestBody::EditSubmit { .. }
      | RequestBody::EditSubmitHeld { .. }
  ) || state.work_home.verdict.is_some()
  {
    return None;
  }
  let now = state.clock.monotonic_ns();
  crate::verbs::reset_would_fit(state, work, now).err()
}

/// What a work's creation on this shard needs before it runs (A-247, A-400): its name held at the partition the name
/// hashes to, by an alias of the work's id when that is another partition (asked of that partition's shard first); then
/// its green's admission when the green lives elsewhere. With both at hand, or the name already held here, it runs, and
/// its own checks answer. A refused reservation is the creation's answer: its pending record goes with it.
fn create_need(
  state: &mut ShardState,
  green: DbVolumeId,
  name: &str,
) -> Result<Option<Need>, Refusal> {
  let owner = crate::verbs::owner_of(crate::verbs::to_wire_volume(green));
  let named_here = crate::verbs::owner_of_name(name, state.shards.len()) == state.partition;
  // A green another node owns is admitted by that node (A-697 part a), whatever partition its id names here.
  let green_here = !green_elsewhere(state, green)
    && (state.greens.contains_key(&green)
      || crate::verbs::shard_of_partition(state, owner).is_none());
  if let Some(Err(refusal)) = &state.work_home.named {
    let refusal = refusal.clone();
    crate::verbs::settle_creation(state, name).map_err(|e| crate::error::refusal_of_db(&e))?;
    return Err(refusal);
  }
  let reserved = named_here || matches!(state.work_home.named, Some(Ok(())));
  if reserved && (green_here || state.work_home.admitted.is_some()) {
    return Ok(None);
  }
  let Some((origin, id)) = state.current_request else {
    return Ok(None);
  };
  let key = (origin, id.client, id.sequence);
  // This request's own creation: its name reserved now, or an answer lost or its daemon restarted, asked again under
  // the same id (the reservation is answered again for the id it holds).
  if let Some(pending) = state.db.partition().pending_creation(name) {
    // A creation a restart caught waiting for a holder's admission is released at the owner first (A-697 part e2): its
    // resend is answered retryable meanwhile, and asks for the release again.
    if state.work_home.releasing.contains_key(name) {
      let pending = pending.clone();
      start_release(state, pending);
      return Err(Refusal::Overloaded {
        shard: state.partition,
      });
    }
    if (pending.origin, pending.client, pending.sequence) != key {
      return Ok(None);
    }
    let work = pending.work;
    return Ok(Some(if reserved {
      Need::Admit {
        green,
        id: work,
        name: name.to_owned(),
      }
    } else {
      Need::ReserveName {
        name: name.to_owned(),
        id: work,
      }
    }));
  }
  if crate::verbs::name_held(state, name).is_some() {
    return Ok(None);
  }
  // The id is this transaction's own sequence, which the pending record's entry consumes, so no other volume of the
  // partition is given it while the owner is asked (two creations parked in one step were given one id, and their
  // queue looped: 9.7 cores for 900 s at 3,000 clients).
  let work = crate::verbs::fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?;
  let record = slates_db::catalog::PendingCreationRecord {
    name: name.to_owned(),
    work,
    origin,
    client: id.client,
    sequence: id.sequence,
    green,
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, &Op::WorkCreationPending { record }, now)
    .map_err(|e| crate::error::refusal_of_db(&e))?;
  Ok(Some(if named_here {
    Need::Admit {
      green,
      id: work,
      name: name.to_owned(),
    }
  } else {
    Need::ReserveName {
      name: name.to_owned(),
      id: work,
    }
  }))
}

/// Parks the running verb, behind its work's other verbs unless it is being resumed, and starts its ask when it is the
/// oldest and has one. Anything that cannot be parked or asked is answered `Overloaded`, unrecorded: its retry runs it.
fn park(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
  work: Option<DbVolumeId>,
  need: Option<Need>,
) -> Gate {
  let Some((origin, id)) = state.current_request else {
    return Gate::Answer(refused_overloaded(state));
  };
  // A work's creation has no queue of its own yet: the id it was minted carries it.
  let queue_key = match (&need, work) {
    (Some(Need::Admit { id, .. } | Need::ReserveName { id, .. }), _) => *id,
    (_, Some(work)) => work,
    _ => return refuse_unrecorded(state),
  };
  // A verb resumed by its own queue parks again at the front of it: a work's verb, or a creation whose name was reserved
  // and whose admission is next (A-400).
  let resuming = state.work_home.resuming == Some(queue_key);
  if !resuming && state.work_home.parked >= bound(state) {
    state.count(PARK_FULL, 1);
    return refuse_unrecorded(state);
  }
  let asks = need.is_some() && (resuming || !state.work_home.queues.contains_key(&queue_key));
  // A route is the work's own, met here once its queue holds the verb (A-298): nothing goes to the owner for it.
  let routing = asks && matches!(need, Some(Need::Route));
  if asks
    && matches!(need, Some(Need::Verdict { .. }))
    && stage_pending(state, queue_key, (origin, id, client_id), principal, &body).is_err()
  {
    return refuse_unrecorded(state);
  }
  if asks && !routing && !start_ask(state, queue_key, origin, id, principal, need) {
    return refuse_unrecorded(state);
  }
  let parked = ParkedVerb {
    verb: ResumedVerb {
      route: state.reply_route.take(),
      origin,
      id,
      client_id,
      principal: principal.clone(),
      body,
    },
    fetched: state.work_home.fetched.take(),
    verdict: state.work_home.verdict.take(),
    rebased: state.work_home.rebased.take(),
    admitted: state.work_home.admitted.take(),
    named: state.work_home.named.take(),
  };
  let home = &mut state.work_home;
  let queue = home.queues.entry(queue_key).or_default();
  if resuming {
    queue.waiting.push_front(parked);
  } else {
    queue.waiting.push_back(parked);
  }
  queue.asking |= asks;
  home.by_request.insert(key(origin, id), queue_key);
  home.parked = home.parked.saturating_add(1);
  if routing {
    await_route(state, queue_key);
  }
  state.acceptance_deferred = true;
  Gate::Answer(crate::lease_wait::parked_reply())
}

/// Answers the running verb `Overloaded` with no completion record, so its retry runs it: `run_recorded` records
/// whatever `dispatch` returns, so the reply goes out here and the verb is marked deferred. A verb forwarded from
/// another node has no route, and its exchange's retry is its answer.
fn refuse_unrecorded(state: &mut ShardState) -> Gate {
  if let Some(route) = state.reply_route.take() {
    let reply = refused_overloaded(state);
    crate::lease_wait::send_reply(state, route, reply);
  }
  state.acceptance_deferred = true;
  Gate::Answer(crate::lease_wait::parked_reply())
}

/// Records, in the running verb's transaction, that `work`'s submit is going to the green's owner and is to be settled
/// on the work under its own key (A-247), unless it already is. Until it is settled every other verb of the work waits
/// behind it ([`settle_pending_first`]), so nothing lands on the work between the owner's verdict and the work's reset,
/// whether the answer comes, is lost, or a restart falls between.
fn stage_pending(
  state: &mut ShardState,
  work: DbVolumeId,
  (origin, id, client_id): (u64, RequestId, u32),
  principal: &Principal,
  body: &RequestBody,
) -> Result<(), slates_db::DbError> {
  let record = slates_db::catalog::PendingSubmitRecord {
    work,
    origin,
    client: id.client,
    sequence: id.sequence,
    client_id,
    principal: principal.clone(),
    body: slates_wire::Wire::to_bytes(body),
  };
  if state.db.partition().pending_submit(work) == Some(&record) {
    return Ok(());
  }
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, &Op::WorkSubmitPending { record }, now)
    .map(|_| ())
}

/// When `work` has a submit to settle that is not the running request (A-247), parks the running verb behind it and
/// puts that submit at the head of the work's queue, to be run again under its own key once the owner answers it: the
/// verdict the owner recorded, or the one it gives now if it never had the request. `None` when there is nothing to
/// settle first.
fn settle_pending_first(
  state: &mut ShardState,
  (client_id, principal): (u32, &Principal),
  body: &RequestBody,
  work: DbVolumeId,
) -> Option<Gate> {
  let pending = state.db.partition().pending_submit(work)?.clone();
  let pending_key = (pending.origin, pending.client, pending.sequence);
  let running = state
    .current_request
    .map(|(origin, id)| (origin, id.client, id.sequence));
  if running == Some(pending_key) || state.work_home.by_request.contains_key(&pending_key) {
    return None;
  }
  // Run again in this pass and still pending: it did not reach the owner (its ask refused, or its verb refused before
  // asking). The verb is answered retryable, unrecorded, rather than parked behind it again, which would run the two
  // in turn for as long as the pass lasts; the next verb of the work tries again.
  if state.work_home.settling.contains(&pending_key) {
    state.count(PENDING_STUCK, 1);
    return Some(refuse_unrecorded(state));
  }
  let Ok(pending_body) = <RequestBody as slates_wire::Wire>::from_bytes(&pending.body) else {
    state.count(PENDING_UNREADABLE, 1);
    return None;
  };
  let gate = park(state, client_id, principal, body.clone(), Some(work), None);
  if !matches!(gate, Gate::Answer(_)) || !state.work_home.queues.contains_key(&work) {
    return Some(gate);
  }
  let id = RequestId {
    client: pending.client,
    sequence: pending.sequence,
  };
  let settle = ParkedVerb {
    verb: ResumedVerb {
      route: None,
      origin: pending.origin,
      id,
      client_id: pending.client_id,
      principal: pending.principal,
      body: pending_body,
    },
    fetched: None,
    verdict: None,
    rebased: None,
    admitted: None,
    named: None,
  };
  let home = &mut state.work_home;
  home.by_request.insert(pending_key, work);
  home.parked = home.parked.saturating_add(1);
  if let Some(queue) = home.queues.get_mut(&work) {
    queue.waiting.push_front(settle);
  }
  state.count(PENDING_SETTLED_FIRST, 1);
  // Resumed after this verb's transaction, as an answer from the owner would be.
  if let Ok(task) = slates_rt::futures::spawn(async move {
    let _ = state::with_state_counted(|s| resume(s, work));
  }) {
    let _ = slates_rt::futures::detach(task);
  }
  Some(gate)
}

fn refused_overloaded(state: &ShardState) -> ReplyBody {
  crate::verbs::refused(Refusal::Overloaded {
    shard: state.partition,
  })
}

/// A retry of a parked request joins it: it is answered when the parked verb resolves. Returns whether it was parked.
pub(crate) fn join(state: &mut ShardState, origin: u64, id: RequestId) -> bool {
  let Some(work) = state.work_home.by_request.get(&key(origin, id)).copied() else {
    return false;
  };
  if let Some(route) = state.reply_route.take()
    && let Some(parked) = state.work_home.queues.get_mut(&work).and_then(|queue| {
      queue
        .waiting
        .iter_mut()
        .find(|parked| parked.verb.origin == origin && parked.verb.id == id)
    })
  {
    parked.verb.route = Some(route);
  }
  true
}

/// What an ask brought back.
enum Answered {
  Base(FetchedBase),
  Verdict(ReplyBody),
  Rebased(RebaseAnswer),
  Admitted(Result<Admitted, Refusal>),
  /// The name's reservation at the partition it hashes to (A-400).
  Named(Result<(), Refusal>),
  /// The work's route is committed (A-298): nothing to hand the verb, which runs its gate again.
  Route,
}

/// One ask waiting in a shard's outbox to its green's owner.
struct AskItem {
  queue: DbVolumeId,
  need: Need,
  green: DbVolumeId,
  version: u64,
  origin: u64,
  id: RequestId,
  principal: Principal,
  back: ReplyRoute,
}

/// A shard's asks to one owner shard: those waiting, and the batches out. At most [`BATCHES_OUT`] are out at a time, so a
/// burst of asks rides a few messages, each carrying what gathered while the others were out, and an owner's channel
/// holds at most that many messages from each asking shard (A-388).
#[derive(Default)]
pub(crate) struct Outbox {
  waiting: VecDeque<AskItem>,
  /// The batches out to the owner now, one a sender task ([`send_batches`]).
  out: usize,
}

/// Shape: the batches a shard keeps out to one owner at once (A-388): two, so one waits at the owner while the one
/// before it is served, and the owner never waits for an asking shard's round trip to have its next batch (double
/// buffering). With one out, an ask made while a batch was out waited for that batch's whole round trip before it left:
/// at 1,000 clients on a Mac Studio that wait was about half of an ask's way to the owner, at the median and in the
/// slowest 1% (the g083 trace, BENCHMARKS 2026-11-18). The owner's channel holds at most two messages from each asking
/// shard, the bound one batch at a time gave it doubled, never unbounded.
const BATCHES_OUT: usize = 2;

/// Format: the status counter of the most batch senders this shard has had out to one owner at once (A-388), the
/// non-vacuity count of [`BATCHES_OUT`]: a shard that never had two out never tested the second.
pub(crate) const BATCHES_OUT_PEAK: &str = "merge.work.batches_out_peak";

/// The most asks one batch carries: one client region's request credit (`slots`), what one client may hold in flight,
/// so the owner's step for a batch is about as long as the step it gives one client's requests.
fn batch_bound(state: &ShardState) -> usize {
  usize::try_from(state.config.region.slots)
    .unwrap_or(1)
    .max(1)
}

/// Counts where a verdict's or a rebase's sealed work was composed: on the work's shard, or by the green's owner.
fn count_composed(state: &mut ShardState, need: &Need) {
  if let Need::Verdict { sealed, .. } | Need::Rebase { sealed } = need {
    state.count(
      if sealed.composed.is_some() {
        COMPOSED_ON_WORK
      } else {
        COMPOSED_BY_OWNER
      },
      1,
    );
  }
}

/// Queues `need`'s ask to the green's owner, sending a batch at once if fewer than [`BATCHES_OUT`] are out (A-388).
/// Returns whether it was queued; its answer resumes the queue.
fn start_ask(
  state: &mut ShardState,
  queue: DbVolumeId,
  origin: u64,
  id: RequestId,
  principal: &Principal,
  need: Option<Need>,
) -> bool {
  let Some(need) = need else {
    return true;
  };
  count_composed(state, &need);
  // A green another node owns admits its work there, by a forward (A-697 part a).
  if let Need::Admit {
    green,
    id: work,
    name,
  } = &need
    && green_elsewhere(state, *green)
  {
    return admit_over_fleet(state, queue, id, principal, (*green, *work, name.clone()));
  }
  // A work on a holder node computes its verdict on the node's replica and sends it to the owner (A-697 part c).
  let need = match need {
    Need::Verdict {
      sealed,
      policy,
      evidence,
    } if green_elsewhere(state, sealed.green) => {
      return verdict_over_fleet(
        state,
        queue,
        (origin, id),
        principal,
        (*sealed, policy, evidence),
      );
    }
    need => need,
  };
  // Every ask goes to its green's owner but a new work's name's, which goes to the partition the name hashes to (A-400).
  let (green, target) = match &need {
    Need::Verdict { sealed, .. } | Need::Rebase { sealed } => (sealed.green, None),
    Need::Admit { green, .. } => (*green, None),
    Need::ReserveName { name, .. } => (
      queue,
      Some(crate::verbs::owner_of_name(name, state.shards.len())),
    ),
    Need::Files(_) | Need::Page { .. } | Need::Sizes => match state.works.get(&queue) {
      Some(w) => (w.green, None),
      None => return false,
    },
    // The owner holds nothing of a work's route (A-298): `park` meets it on this shard.
    Need::Route => return false,
  };
  let version = state.works.get(&queue).map_or(0, |w| w.base_version);
  let owner_partition =
    target.unwrap_or_else(|| crate::verbs::owner_of(crate::verbs::to_wire_volume(green)));
  // A base read and a rebase of a work on a holder node go to the node's control shard, which holds its replica of the
  // green (A-697); its verdict went to the owner above.
  let owner = if target.is_none() && green_elsewhere(state, green) {
    if matches!(need, Need::Verdict { .. }) {
      return false;
    }
    state.shards.first().copied()
  } else {
    crate::verbs::shard_of_partition(state, owner_partition)
  };
  let Some(owner) = owner else {
    return false;
  };
  let Some(here) = crate::verbs::shard_of_partition(state, state.partition) else {
    return false;
  };
  let back = ReplyRoute {
    shard: here,
    client_index: 0,
    request: id.word(),
    through_work: true,
  };
  let outbox = state.work_home.outboxes.entry(owner).or_default();
  outbox.waiting.push_back(AskItem {
    queue,
    need,
    green,
    version,
    origin,
    id,
    principal: principal.clone(),
    back,
  });
  // A batch out takes what waits when its answer is back; past the bound this ask waits for one.
  let full = outbox.out >= BATCHES_OUT;
  if !full {
    outbox.out = outbox.out.saturating_add(1);
  }
  let out = outbox.out;
  state.count(ASKED_THE_OWNER, 1);
  if full {
    return true;
  }
  let peak = state.refusals.entry(BATCHES_OUT_PEAK).or_insert(0);
  *peak = (*peak).max(u64::try_from(out).unwrap_or(u64::MAX));
  match slates_rt::futures::spawn(send_batches(here, owner)) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      true
    }
    Err(_) => {
      // This sender was not started: another out takes what waits when its answer is back.
      let others = state
        .work_home
        .outboxes
        .get_mut(&owner)
        .map_or(0, |outbox| {
          outbox.out = outbox.out.saturating_sub(1);
          outbox.out
        });
      if others > 0 {
        return true;
      }
      // Nothing will send what waits: every ask in this outbox is answered unanswered now.
      let left = state
        .work_home
        .outboxes
        .remove(&owner)
        .map(|outbox| outbox.waiting)
        .unwrap_or_default();
      for item in left {
        if item.queue != queue {
          resolve(state, item.queue, None);
        }
      }
      false
    }
  }
}

/// Asks `green`'s owner on another node to admit the work `work` this shard is making (A-697 part a): `AdmitWork`,
/// forwarded from the control shard, where the fleet's sessions live, under the creation's own request id, so the owner's
/// completion record answers a resend once. Asked once the creation's pending record is committed, as an outbox's ask
/// is ([`staged`]). The answer resumes the creation: `WorkAdmitted` its admission, a refusal the owner gave its answer;
/// no answer within the parked verb's bound, or one that says the ask was not started, answers it retryable,
/// unrecorded, and its resend asks again. Returns whether the ask was started.
fn admit_over_fleet(
  state: &mut ShardState,
  queue: DbVolumeId,
  id: RequestId,
  principal: &Principal,
  (green, work, name): (DbVolumeId, DbVolumeId, String),
) -> bool {
  let (Some(here), Some(control)) = (
    crate::verbs::shard_of_partition(state, state.partition),
    state.shards.first().copied(),
  ) else {
    return false;
  };
  let region = state
    .node_regions
    .get(&state.fleet.host())
    .map_or(0, |region| region.0);
  let wire_green = crate::verbs::to_wire_volume(green);
  let request = crate::verbs::forwarded_request(
    state,
    id.word(),
    principal.clone(),
    RequestBody::AdmitWork {
      green: wire_green,
      work: crate::verbs::to_wire_volume(work),
    },
  );
  let deadline = park_bound_ns(state);
  state.count(ASKED_THE_OWNER_NODE, 1);
  let asked = slates_rt::futures::spawn(async move {
    let staged = state::with_state_counted(|s| {
      let staged = s
        .db
        .partition()
        .pending_creation(&name)
        .is_some_and(|pending| pending.work == work);
      if !staged {
        s.count(ADMISSION_UNSTAGED, 1);
        resolve(s, queue, None);
      }
      staged
    });
    if staged != Some(true) {
      return;
    }
    let reply = crate::xshard::task_within(
      here,
      control,
      move || async move {
        crate::verbs::resolve_and_forward(wire_green, region, None, vec![request])
          .await
          .0
          .first()
          .clone()
      },
      deadline,
    )
    .await;
    let _ = state::with_state_counted(|s| {
      // Test support (`Daemon::lose_next_owner_answer`): the admission is lost, as a crash between the owner's step and
      // this one would lose it.
      let answer = if std::mem::take(&mut s.work_home.lose_next_answer) {
        None
      } else {
        admitted_over_fleet(reply)
      };
      resolve(s, queue, answer);
    });
  });
  match asked {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      true
    }
    Err(_) => false,
  }
}

/// Format: a submit of a work on a holder node whose version the owner committed before the node's replica held it, held
/// until the replica does (A-697 part d), the non-vacuity count of [`hold_until_replica`].
pub(crate) const SETTLE_AWAITS_REPLICA: &str = "merge.work.settle_awaits_replica";
/// Format: such a submit answered once the replica held its version (A-697 part d), the non-vacuity count of
/// [`replica_reached`].
pub(crate) const SETTLED_ON_REPLICA: &str = "merge.work.settled_on_replica";
/// Format: such an answer that could not be handed to the work's shard; the submit's bound answers it retryable.
const SETTLE_UNHANDED: &str = "merge.work.settle_unhanded";

/// Counter pairs of a holder node's own submit (A-697's measured breakdown; `merge_service::SUBMIT_JUDGE` and its
/// siblings): its increment composed on the work's shard (with what the verdict need reads), its verdict computed on
/// the replica, the forward to the owner and back (the owner's whole step and its quorum commit within it), and the
/// wait for the replica to hold the version.
pub(crate) const SUBMIT_COMPOSE: (&str, &str) = ("merge.submit.compose", "merge.submit.compose_ns");
/// See [`SUBMIT_COMPOSE`].
pub(crate) const SUBMIT_COMPUTE: (&str, &str) = ("merge.submit.compute", "merge.submit.compute_ns");
/// See [`SUBMIT_COMPOSE`].
pub(crate) const SUBMIT_FORWARD: (&str, &str) = ("merge.submit.forward", "merge.submit.forward_ns");
/// See [`SUBMIT_COMPOSE`].
pub(crate) const SUBMIT_SETTLE: (&str, &str) = ("merge.submit.settle", "merge.submit.settle_ns");

/// Format: a verdict a holder computed on its replica of a green another node owns, to send its owner (A-697 part c),
/// the non-vacuity count of [`verdict_over_fleet`].
pub(crate) const COMPUTED_ON_HOLDER: &str = "merge.work.computed_on_holder";

/// Commits a submit of a work on a holder node (A-697 part c; A-658 part 3b): once the submit's pending record is
/// committed (as an outbox's ask, [`staged`]), the node's control shard computes the composed increment's verdict on its
/// replica of the green (`Green::compute`, a pure function of the versions it is judged over) and forwards the increment
/// and the verdict's wire form to the green's owner as `SubmitComputed`, under the submit's own request id, so the
/// owner's completion record answers a resend once. The owner's answer is the verdict the submit settles; no answer
/// within the parked verb's bound, or one that says it was not started, answers it retryable, unrecorded, and its resend
/// runs it again. Returns whether the ask was started.
fn verdict_over_fleet(
  state: &mut ShardState,
  queue: DbVolumeId,
  (origin, id): (u64, RequestId),
  principal: &Principal,
  (sealed, policy, evidence): (SealedWork, crate::verbs::SubmitPolicy, Vec<[u8; 32]>),
) -> bool {
  let (Some(here), Some(control)) = (
    crate::verbs::shard_of_partition(state, state.partition),
    state.shards.first().copied(),
  ) else {
    return false;
  };
  let region = state
    .node_regions
    .get(&state.fleet.host())
    .map_or(0, |region| region.0);
  let (work, green) = (sealed.work, sealed.green);
  let carried = slates_ipc::protocol::ComputedSubmit {
    work_mode: crate::landing::wire_mode(policy.work_mode),
    held: matches!(policy.approval, crate::verbs::Approval::Held),
    approved: match policy.approval {
      crate::verbs::Approval::Of(identity) => Some(identity),
      crate::verbs::Approval::None | crate::verbs::Approval::Held => None,
    },
    signature: policy.signature,
    signers: policy.work_signed.signers,
    signed_required: policy.work_signed.required,
    authorship: policy.authorship,
  };
  let principal = principal.clone();
  let deadline = park_bound_ns(state);
  state.count(ASKED_THE_OWNER_NODE, 1);
  let asked = slates_rt::futures::spawn(async move {
    let staged = state::with_state_counted(|s| {
      let staged = s
        .db
        .partition()
        .pending_submit(work)
        .is_some_and(|pending| {
          (pending.origin, pending.client, pending.sequence) == (origin, id.client, id.sequence)
        });
      if !staged {
        resolve(s, queue, None);
      }
      staged
    });
    if staged != Some(true) {
      return;
    }
    let reply = crate::xshard::task_within(
      here,
      control,
      move || async move {
        let built =
          state::with_state(|s| computed_request(s, (sealed, evidence), (id, principal), carried));
        let forward_start = slates_machine::clock::monotonic_ns();
        let reply = match built {
          Some(Ok(request)) => crate::verbs::resolve_and_forward(
            crate::verbs::to_wire_volume(green),
            region,
            None,
            vec![request],
          )
          .await
          .0
          .first()
          .clone(),
          Some(Err(reply)) => *reply,
          None => crate::verbs::refused(Refusal::Overloaded { shard: control }),
        };
        let forward_ns = slates_machine::clock::monotonic_ns().saturating_sub(forward_start);
        // The work settles on its new base only once this node's replica holds it (A-697 part d).
        let held = state::with_state(|s| {
          crate::merge_service::count_phase(s, SUBMIT_FORWARD, forward_ns);
          hold_until_replica(s, (green, &reply), (here, origin, id), deadline)
        })
        .unwrap_or(false);
        (reply, held)
      },
      deadline,
    )
    .await;
    let _ = state::with_state_counted(|s| match reply {
      Some((_, true)) => defer_oldest(s, queue),
      Some((reply, false)) => resolve(s, queue, verdict_over_fleet_answer(Some(reply))),
      None => resolve(s, queue, None),
    });
  });
  match asked {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      true
    }
    Err(_) => false,
  }
}

/// On the control shard: the `SubmitComputed` forward of `inc`, its verdict computed on this node's replica of `green`
/// (A-697 part c); or the reply the submit gets without one: retryable while the replica lacks the work's base, and a
/// base or a copy the verdict refuses refused by its kind, as the owner would.
fn computed_request(
  state: &mut ShardState,
  (sealed, evidence): (SealedWork, Vec<[u8; 32]>),
  (id, principal): (RequestId, Principal),
  carried: slates_ipc::protocol::ComputedSubmit,
) -> Result<Vec<u8>, Box<ReplyBody>> {
  use slates_merge::engine::ComputeRefused;
  let (work, green) = (sealed.work, sealed.green);
  // A work holding a copy leaves its increment to be composed where the green's bytes are: here, over the replica.
  let inc = match sealed.composed {
    Some(inc) => inc,
    None => crate::verbs::compose_on_replica(state, &sealed, evidence)
      .map_err(|refusal| Box::new(crate::verbs::refused(refusal)))?,
  };
  let behind = crate::verbs::refused(Refusal::Overloaded { shard: state.shard });
  let Some(replica) = state
    .merge
    .replicas
    .get(&slates_db::register::ObjectId(green.bytes))
  else {
    state.count(REPLICA_BEHIND, 1);
    return Err(Box::new(behind));
  };
  let compute_start = slates_machine::clock::monotonic_ns();
  let computed = replica.compute(&inc);
  let compute_ns = slates_machine::clock::monotonic_ns().saturating_sub(compute_start);
  crate::merge_service::count_phase(state, SUBMIT_COMPUTE, compute_ns);
  let computed = match computed {
    Ok(computed) => computed,
    Err(ComputeRefused::BaseAhead { .. }) => {
      state.count(REPLICA_BEHIND, 1);
      return Err(Box::new(behind));
    }
    Err(ComputeRefused::BaseUnavailable { base, .. }) => {
      return Err(Box::new(crate::verbs::refused(Refusal::UnknownBase {
        green: crate::verbs::to_wire_volume(green),
        version: base,
      })));
    }
    Err(ComputeRefused::CopyRefused { path, case }) => {
      return Err(Box::new(crate::verbs::refused(crate::verbs::copy_refused(
        path, case,
      ))));
    }
  };
  state.count(COMPUTED_ON_HOLDER, 1);
  let form = computed.encode(&inc);
  Ok(crate::verbs::forwarded_request(
    state,
    id.word(),
    principal,
    RequestBody::SubmitComputed {
      work: crate::verbs::to_wire_volume(work),
      green: crate::verbs::to_wire_volume(green),
      increment: inc.encode(),
      computed: form,
      carried,
    },
  ))
}

/// What the green's owner on another node answered a `SubmitComputed` (A-697 part c): the verdict the submit settles,
/// or `None` (answered retryable, unrecorded) for no answer and one that says the ask was not started or did not reach
/// the owner.
fn verdict_over_fleet_answer(reply: Option<ReplyBody>) -> Option<Answered> {
  match reply? {
    ReplyBody::Refused {
      refusal:
        Refusal::Overloaded { .. }
        | Refusal::Unpublished { .. }
        | Refusal::OwnerUnreachable { .. }
        | Refusal::HomedElsewhere { .. },
    } => None,
    reply => Some(Answered::Verdict(reply)),
  }
}

/// On the control shard: holds `reply`, the owner's acceptance of a version this node's replica of `green` does not hold
/// yet, until it does (A-697 part d), so the submit it answers settles the work on a base this node can read; the
/// parked submit waits under its own bound (`defer_oldest`) and is answered retryable past it, its resend answered by
/// the owner's record. Whether it was held; expired holds are let go first, so the holds stay within the parked verbs.
fn hold_until_replica(
  state: &mut ShardState,
  (green, reply): (DbVolumeId, &ReplyBody),
  (shard, origin, id): (u16, u64, RequestId),
  bound_ns: u64,
) -> bool {
  let ReplyBody::Submitted {
    version: Some(version),
    ..
  } = reply
  else {
    return false;
  };
  let object = slates_db::register::ObjectId(green.bytes);
  if state
    .merge
    .replicas
    .get(&object)
    .is_some_and(|replica| replica.head() >= *version)
  {
    return false;
  }
  let now = slates_machine::clock::monotonic_ns();
  state
    .work_home
    .replica_waits
    .retain(|wait| wait.until_ns >= now);
  state.work_home.replica_waits.push(ReplicaWait {
    green: object,
    version: *version,
    shard,
    origin,
    id,
    reply: reply.clone(),
    until_ns: now.saturating_add(bound_ns),
    held_ns: now,
  });
  state.count(SETTLE_AWAITS_REPLICA, 1);
  true
}

/// On the control shard, once this node's replica of `object` took a version (A-697 part d;
/// `merge_service::recompute`): every held submit whose version the replica now holds is answered on its work's shard
/// (`answered_late`, which finds it by its key, so one already released is not answered twice).
pub(crate) fn replica_reached(state: &mut ShardState, object: slates_db::register::ObjectId) {
  replays_resumed(state, object);
  if state.work_home.replica_waits.is_empty() {
    return;
  }
  let Some(head) = state
    .merge
    .replicas
    .get(&object)
    .map(slates_merge::engine::Green::head)
  else {
    return;
  };
  let (ready, waiting): (Vec<ReplicaWait>, Vec<ReplicaWait>) =
    std::mem::take(&mut state.work_home.replica_waits)
      .into_iter()
      .partition(|wait| wait.green == object && wait.version <= head);
  state.work_home.replica_waits = waiting;
  let now = slates_machine::clock::monotonic_ns();
  for wait in ready {
    state.count(SETTLED_ON_REPLICA, 1);
    crate::merge_service::count_phase(state, SUBMIT_SETTLE, now.saturating_sub(wait.held_ns));
    let ReplicaWait {
      shard,
      origin,
      id,
      reply,
      ..
    } = wait;
    let handed = crate::xshard::hand_reply_home(shard, move || {
      let _ = state::with_state_counted(|s| answered_late(s, origin, id, reply));
    });
    if handed.is_err() {
      state.count(SETTLE_UNHANDED, 1);
    }
  }
}

/// Format: a recovered holder work's replay that waits for this node's replica of its green (A-697 part e), the
/// non-vacuity count of [`holder_replay`]'s wait.
pub(crate) const REPLAY_AWAITS_REPLICA: &str = "merge.work.replay_awaits_replica";
/// Format: a recovered holder work replayed over this node's replica of its green (A-697 part e).
pub(crate) const REPLAYED_ON_REPLICA: &str = "merge.work.replayed_on_replica";
/// Format: a shard asked to replay its recovered holder works again once the replica was installed, by a message that
/// could not be kept: they answer `Unpublished` until the daemon's next start.
const REPLAY_ASK_UNHANDED: &str = "merge.work.replay_ask_unhanded";

/// What [`holder_replay`] did.
pub(crate) enum HolderReplay {
  /// Replayed over the replica, or refused (`None`, the log did not replay): the outcome to install on the work's shard.
  Replayed(Option<(u64, WorkFiles, Vec<VolumeOp>)>),
  /// The replica is not here yet: its rebuild is under way, or waits for the green's next record. The work stays
  /// recorded unbuilt, answering `Unpublished`, and its shard asks again once the replica is installed.
  Later,
}

/// On the control shard: replays a recovered holder work's log (A-697 part e; A-207, A-247) over this node's replica of
/// its green, which a restart leaves to be rebuilt from the records this node holds (A-313): the rebuild is started
/// here when it is not, and while it is under way the replay waits for it ([`replica_reached`] asks `shard` again).
pub(crate) fn holder_replay(
  state: &mut ShardState,
  (green, created_at): (DbVolumeId, u64),
  entries: &[Vec<u8>],
  shard: u16,
) -> HolderReplay {
  let object = slates_db::register::ObjectId(green.bytes);
  if !state.merge.replicas.contains_key(&object)
    && !matches!(
      crate::merge_origin::rebuild_replica(state, object),
      crate::merge_origin::Rebuild::Installed(_)
    )
  {
    state.count(REPLAY_AWAITS_REPLICA, 1);
    state
      .work_home
      .replays_owed
      .entry(object)
      .or_default()
      .insert(shard);
    return HolderReplay::Later;
  }
  state.count(REPLAYED_ON_REPLICA, 1);
  HolderReplay::Replayed(
    crate::work_log::replay(state.merge.replicas.get(&object), created_at, entries).ok(),
  )
}

/// The replays of recovered holder works that waited for `object`'s replica, asked again on their shards now that it is
/// installed (A-697 part e): each shard asks for every work it still records unbuilt (`verbs::ask_owners_to_replay`).
fn replays_resumed(state: &mut ShardState, object: slates_db::register::ObjectId) {
  let Some(shards) = state.work_home.replays_owed.remove(&object) else {
    return;
  };
  for shard in shards {
    let handed = crate::xshard::hand_reply_home(shard, || {
      let _ = state::with_state_counted(crate::verbs::ask_owners_to_replay);
    });
    if handed.is_err() {
      state.count(REPLAY_ASK_UNHANDED, 1);
    }
  }
}

/// Format: a holder creation a restart caught waiting for its admission, held until its admission is released at the
/// green's owner (A-697 part e2), the non-vacuity count of [`hold_for_release`].
pub(crate) const ADMISSION_HELD_FOR_RELEASE: &str = "merge.work.admission_held_for_release";
/// Format: such an admission released at the owner and its creation settled here (A-697 part e2).
pub(crate) const ADMISSION_RELEASED: &str = "merge.work.admission_released";
/// Format: a release the owner did not answer `WorkFloorMoved`; asked again at the creation's resend or the next start.
const ADMISSION_RELEASE_UNANSWERED: &str = "merge.work.admission_release_unanswered";

/// At recovery, on a work's shard (A-697 part e2): whether the pending creation `pending` is a holder's, whose green
/// another node owns and whose owner may have recorded a floor for the work at its admission. Such a creation is not
/// dropped as another is: it keeps its record, so its name and id stay held, and is released at the owner first
/// ([`release_admissions`]); dropped, the floor at the owner would hold its green's history for a work never made.
pub(crate) fn hold_for_release(
  state: &mut ShardState,
  pending: &slates_db::catalog::PendingCreationRecord,
) -> bool {
  if !owned_elsewhere(state, pending.green) {
    return false;
  }
  state
    .work_home
    .releasing
    .insert(pending.name.clone(), false);
  state.count(ADMISSION_HELD_FOR_RELEASE, 1);
  true
}

/// As clients are let in (A-697 part e2; as A-533 asks for unbuilt works): every holder creation held for its release
/// asks the green's owner to drop the floor its admission may have recorded.
pub(crate) fn release_admissions(state: &mut ShardState) {
  let held: Vec<slates_db::catalog::PendingCreationRecord> = state
    .work_home
    .releasing
    .keys()
    .filter_map(|name| state.db.partition().pending_creation(name).cloned())
    .collect();
  for pending in held {
    start_release(state, pending);
  }
}

/// Asks the green's owner on another node to drop `pending`'s work's floor (`MoveWorkFloor` with no base, monotone
/// there: a floor never recorded stays none), from the control shard; on `WorkFloorMoved` the creation is settled here,
/// its name and id given back, and the client's resend makes the work afresh. One release out at a time a creation; one
/// unanswered is asked again at the creation's next resend or the next start (no timer; A-533's pattern).
fn start_release(state: &mut ShardState, pending: slates_db::catalog::PendingCreationRecord) {
  match state.work_home.releasing.get_mut(&pending.name) {
    Some(out) if !*out => *out = true,
    _ => return,
  }
  let (Some(here), Some(control)) = (
    crate::verbs::shard_of_partition(state, state.partition),
    state.shards.first().copied(),
  ) else {
    return;
  };
  let region = state
    .node_regions
    .get(&state.fleet.host())
    .map_or(0, |region| region.0);
  let principal = Principal::Uid {
    host: state.origin_anchor,
    uid: 0,
  };
  let wire_green = crate::verbs::to_wire_volume(pending.green);
  let request = crate::verbs::forwarded_request(
    state,
    0,
    principal,
    RequestBody::MoveWorkFloor {
      green: wire_green,
      work: crate::verbs::to_wire_volume(pending.work),
      base: None,
    },
  );
  let deadline = park_bound_ns(state);
  let (name, work) = (pending.name, pending.work);
  let answered_name = name.clone();
  let spawned = slates_rt::futures::spawn(async move {
    let reply = crate::xshard::task_within(
      here,
      control,
      move || async move {
        crate::verbs::resolve_and_forward(wire_green, region, None, vec![request])
          .await
          .0
          .first()
          .clone()
      },
      deadline,
    )
    .await;
    let _ = state::with_state_counted(|s| released(s, (&answered_name, work), reply.as_ref()));
  });
  match spawned {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
    }
    Err(_) => {
      if let Some(out) = state.work_home.releasing.get_mut(&name) {
        *out = false;
      }
      state.count(ADMISSION_RELEASE_UNANSWERED, 1);
    }
  }
}

/// A release's answer, on the creation's shard: the creation settled once the owner dropped the floor.
fn released(state: &mut ShardState, (name, work): (&str, DbVolumeId), reply: Option<&ReplyBody>) {
  let still = state
    .db
    .partition()
    .pending_creation(name)
    .is_some_and(|pending| pending.work == work);
  if !matches!(reply, Some(ReplyBody::WorkFloorMoved)) || !still {
    if let Some(out) = state.work_home.releasing.get_mut(name) {
      *out = false;
    }
    if still {
      state.count(ADMISSION_RELEASE_UNANSWERED, 1);
    }
    return;
  }
  state.db.begin();
  let settled = crate::verbs::settle_creation(state, name);
  if settled.is_err() || state.db.commit(&mut state.segment).is_err() {
    crate::verbs::reconcile_unpublished_effects(state);
    if let Some(out) = state.work_home.releasing.get_mut(name) {
      *out = false;
    }
    state.count(ADMISSION_RELEASE_UNANSWERED, 1);
    return;
  }
  state.work_home.releasing.remove(name);
  state.count(ADMISSION_RELEASED, 1);
}

/// What the green's owner on another node answered an `AdmitWork` (A-697 part a): the admission, or the refusal it gave;
/// `None` (answered retryable, unrecorded) for no answer, one that says the ask was not started or did not reach the
/// owner, and any other reply.
fn admitted_over_fleet(reply: Option<ReplyBody>) -> Option<Answered> {
  match reply? {
    ReplyBody::WorkAdmitted { id, base } => Some(Answered::Admitted(Ok(Admitted {
      id: crate::verbs::to_db_volume(id),
      base,
    }))),
    ReplyBody::Refused {
      refusal:
        Refusal::Overloaded { .. }
        | Refusal::Unpublished { .. }
        | Refusal::OwnerUnreachable { .. }
        | Refusal::HomedElsewhere { .. },
    } => None,
    ReplyBody::Refused { refusal } => Some(Answered::Admitted(Err(refusal))),
    _ => None,
  }
}

/// What the owner answered for one ask of a batch.
#[allow(
  clippy::large_enum_variant,
  reason = "an answer is the common outcome: boxing it would allocate per ask, to shrink the rare Deferred"
)]
enum Outcome {
  Answered(Answered),
  /// The owner took the verdict and answers when it commits.
  Deferred,
  /// Nothing to answer yet: a holder's replica without the work's base version (A-697 part a). The verb is answered
  /// retryable, unrecorded.
  Unanswered,
}

/// Sends `here`'s outbox to `owner` a batch at a time until it is empty: one of the at most [`BATCHES_OUT`] senders out
/// to `owner`, each awaiting its own batch's answers (A-388).
async fn send_batches(here: u16, owner: u16) {
  loop {
    let batch = state::with_state(|s| take_batch(s, owner)).unwrap_or_default();
    if batch.is_empty() {
      return;
    }
    let keys: Vec<DbVolumeId> = batch.iter().map(|item| item.queue).collect();
    let budget = crate::lease::lease_bound_ns();
    let answers = crate::xshard::call_in_shard_time_given_room(
      here,
      owner,
      move |s| {
        batch
          .into_iter()
          .map(|item| answer_on_owner(s, item))
          .collect::<Vec<_>>()
      },
      budget,
    )
    .await;
    let _ = state::with_state_counted(|s| match answers {
      Some(answers) => {
        for (queue, outcome) in keys.into_iter().zip(answers) {
          settle_outcome(s, queue, outcome);
        }
      }
      None => {
        for queue in keys {
          resolve(s, queue, None);
        }
      }
    });
  }
}

/// The next batch of `owner`'s outbox, each ask checked to be durable first: a submit's pending record and a
/// creation's committed before its ask goes (`stage_pending`, `create_need`); one whose verb's transaction did not
/// commit is answered unanswered. An empty batch ends the sending.
fn take_batch(state: &mut ShardState, owner: u16) -> Vec<AskItem> {
  let bound = batch_bound(state);
  let Some(outbox) = state.work_home.outboxes.get_mut(&owner) else {
    return Vec::new();
  };
  let take = outbox.waiting.len().min(bound);
  let items: Vec<AskItem> = outbox.waiting.drain(..take).collect();
  if items.is_empty() {
    // This sender ends: its batch is no longer out.
    outbox.out = outbox.out.saturating_sub(1);
    return items;
  }
  let (ready, unstaged): (Vec<AskItem>, Vec<AskItem>) =
    items.into_iter().partition(|item| staged(state, item));
  for item in unstaged {
    resolve(state, item.queue, None);
  }
  ready
}

/// Whether an ask's verb left what the owner's answer will be settled against: a submit's pending record, a creation's.
fn staged(state: &ShardState, item: &AskItem) -> bool {
  let partition = state.db.partition();
  match &item.need {
    Need::Verdict { sealed, .. } => partition
      .pending_submit(sealed.work)
      .is_some_and(|pending| {
        (pending.origin, pending.client, pending.sequence)
          == (item.origin, item.id.client, item.id.sequence)
      }),
    Need::Admit { id, name, .. } => partition
      .pending_creation(name)
      .is_some_and(|pending| pending.work == *id),
    _ => true,
  }
}

/// One ask answered on the owner's shard.
fn answer_on_owner(state: &mut ShardState, item: AskItem) -> Outcome {
  let AskItem {
    need,
    green,
    version,
    origin,
    id,
    principal,
    back,
    ..
  } = item;
  if matches!(need, Need::Files(_) | Need::Page { .. } | Need::Sizes)
    && state.work_home.on_holders
    && !state.greens.contains_key(&green)
  {
    // A holder's base read, answered by its replica of the green (A-697 part a), once that holds the work's base.
    let holds = state
      .merge
      .replicas
      .get(&slates_db::register::ObjectId(green.bytes))
      .is_some_and(|replica| replica.head() >= version);
    if !holds {
      state.count(REPLICA_BEHIND, 1);
      return Outcome::Unanswered;
    }
    state.count(READ_FROM_REPLICA, 1);
  }
  match need {
    Need::Files(paths) => Outcome::Answered(Answered::Base(crate::verbs::owner_files(
      state, green, version, &paths,
    ))),
    Need::Page { path, offset, want } => Outcome::Answered(Answered::Base(
      crate::verbs::owner_page(state, green, version, path, (offset, want)),
    )),
    Need::Sizes => Outcome::Answered(Answered::Base(crate::verbs::owner_sizes(
      state, green, version,
    ))),
    Need::Verdict {
      evidence,
      sealed,
      policy,
    } => match crate::verbs::owner_judge(state, (origin, id), (*sealed, evidence, policy), back) {
      Some(reply) => Outcome::Answered(Answered::Verdict(reply)),
      None => Outcome::Deferred,
    },
    // A holder's work is rebased over this node's replica (A-697); any other through the green.
    Need::Rebase { sealed } if state.work_home.on_holders && !state.greens.contains_key(&green) => {
      Outcome::Answered(Answered::Rebased(crate::verbs::replica_rebase(
        state, &sealed,
      )))
    }
    Need::Rebase { sealed } => Outcome::Answered(Answered::Rebased(crate::verbs::owner_rebase(
      state, *sealed,
    ))),
    Need::Admit {
      green, id: work, ..
    } => Outcome::Answered(Answered::Admitted(crate::verbs::owner_admit(
      state, &principal, green, work,
    ))),
    Need::ReserveName { name, id: work } => {
      Outcome::Answered(Answered::Named(reserve_name(state, name, work)))
    }
    // Never asked (`start_ask` refuses it); answered, it only runs the work's gate again, which checks the route there.
    Need::Route => Outcome::Answered(Answered::Route),
  }
}

/// Holds `name` here, the partition it hashes to, for `work`, a work made on another shard (A-400, A-236's reservation):
/// answered again for the work it already holds (an answer lost, or its shard restarted), refused `AlreadyExists` for
/// any other holder.
fn reserve_name(state: &mut ShardState, name: String, work: DbVolumeId) -> Result<(), Refusal> {
  if state
    .db
    .partition()
    .name_alias(&name)
    .is_some_and(|alias| alias.volume == work)
  {
    return Ok(());
  }
  crate::verbs::alias_step(
    state,
    &Op::NameAliasReserved {
      record: slates_db::catalog::NameAliasRecord {
        name,
        volume: work,
        confirmed: false,
      },
    },
  )
}

/// Settles one ask's outcome on the work's shard.
fn settle_outcome(state: &mut ShardState, queue: DbVolumeId, outcome: Outcome) {
  match outcome {
    Outcome::Answered(Answered::Verdict(_) | Answered::Admitted(_))
      if std::mem::take(&mut state.work_home.lose_next_answer) =>
    {
      resolve(state, queue, None);
    }
    Outcome::Answered(answered) => resolve(state, queue, Some(answered)),
    // `answered_late` resumes the verb when its record commits at the quorum, and the deadline releases the work if it
    // never does.
    Outcome::Deferred => defer_oldest(state, queue),
    Outcome::Unanswered => resolve(state, queue, None),
  }
}

/// The owner answered a deferred verdict: its version committed at the quorum (AUD-11). Resumes the verb parked under
/// the request.
pub(crate) fn answered_late(state: &mut ShardState, origin: u64, id: RequestId, reply: ReplyBody) {
  let Some(work) = state.work_home.by_request.get(&key(origin, id)).copied() else {
    return;
  };
  state.count(ANSWERED_LATE, 1);
  resolve(state, work, Some(Answered::Verdict(reply)));
}

/// Whether `work`'s route is committed (A-255, A-298): its catalog register, which names its green and is the only route
/// a resend of its submit has after a takeover, is ready at its quorum. Read on this shard, which holds the work's
/// record; at once on a laptop, which has no remote candidate.
fn route_committed(state: &ShardState, work: DbVolumeId) -> bool {
  crate::fleet::catalog_committed(state, work, state.fleet.host())
}

/// Holds `work`'s oldest verb, a submit, until the work's route is committed (A-298). The work's catalog register ships
/// now rather than at the coordinator's period, and the record plane's next pass on this shard after it commits runs
/// the verb's gate again ([`routes_placed`]). Past the failover delay ([`park_bound_ns`]) the verb is answered
/// `Overloaded`, unrecorded, so its retry runs it again: nothing went to the owner, so nothing waits to be settled.
fn await_route(state: &mut ShardState, work: DbVolumeId) {
  state.count(AWAITED_ROUTE, 1);
  state.work_home.routing.insert(work);
  state.merge.works_owing_catalog.insert(work);
  crate::merge_service::nudge_record_plane(state);
  defer_oldest(state, work);
}

/// Runs again the gate of each work's oldest verb that waits for its route, once the route is committed (A-298). Called
/// at every pass of the record plane on this shard (`merge_service::next_merge_work`), which a catalog's commit nudges.
pub(crate) fn routes_placed(state: &mut ShardState) {
  if state.work_home.routing.is_empty() {
    return;
  }
  let placed: Vec<DbVolumeId> = state
    .work_home
    .routing
    .iter()
    .copied()
    .filter(|work| route_committed(state, *work))
    .collect();
  for work in placed {
    resolve(state, work, Some(Answered::Route));
  }
}

/// Derived: how long a verb of a work is held for a quorum commit — its verdict's, which the owner answers once its
/// version commits at the quorum (AUD-11), or its work's route's (A-298) — before it is answered `Overloaded` and
/// retried (A-434): the node's failover delay (`ShardState::failover_ns`, A-363), a held reply's total (A-354, A-375,
/// `live_ship`'s durable spans read the same). A commit at the quorum waits, when a holder is lost, for the fleet to fail
/// over, so a verb given up sooner only comes back to wait again. Before A-434 it was the owner lease's bound (about
/// 0.9 s), the bound a verb waits for its own lease to confirm (`lease_wait`), which a commit is not.
fn park_bound_ns(state: &ShardState) -> u64 {
  state.failover_ns.max(1)
}

/// The oldest verb of `work` waits for its quorum commit, or for its route (A-298), up to [`park_bound_ns`].
fn defer_oldest(state: &mut ShardState, work: DbVolumeId) {
  let deadline = slates_machine::clock::monotonic_ns().saturating_add(park_bound_ns(state));
  if let Some(queue) = state.work_home.queues.get_mut(&work) {
    queue.deferred_until = Some(deadline);
  }
  if state.work_home.timer_armed {
    return;
  }
  match slates_rt::futures::spawn(deferred_timer()) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      state.work_home.timer_armed = true;
    }
    Err(_) => state.count(ASK_UNANSWERED, 1),
  }
}

/// One task per shard while verdicts are deferred: sleeps to the earliest deadline, releases what passed it, and goes on
/// while any is deferred.
async fn deferred_timer() {
  loop {
    let wait = state::with_state(|s| {
      let now = slates_machine::clock::monotonic_ns();
      let earliest = s
        .work_home
        .queues
        .values()
        .filter_map(|queue| queue.deferred_until)
        .min();
      match earliest {
        Some(deadline) => Some(deadline.saturating_sub(now)),
        None => {
          s.work_home.timer_armed = false;
          None
        }
      }
    })
    .flatten();
    let Some(wait) = wait else {
      return;
    };
    // A refused sleep (the timer wheel full) still checks now; the next turn sleeps again.
    let _ = slates_rt::futures::sleep(wait.max(1)).await;
    let _ = state::with_state_counted(release_expired);
  }
}

/// Answers `Overloaded` to every deferred verb past its deadline, releasing its work.
fn release_expired(state: &mut ShardState) {
  release_expired_at(state, slates_machine::clock::monotonic_ns());
}

/// [`release_expired`] at `now`, on the host's monotonic clock.
fn release_expired_at(state: &mut ShardState, now: u64) {
  let expired: Vec<DbVolumeId> = state
    .work_home
    .queues
    .iter()
    .filter(|(_, queue)| queue.deferred_until.is_some_and(|until| until <= now))
    .map(|(work, _)| *work)
    .collect();
  for work in expired {
    resolve(state, work, None);
  }
}

/// An ask finished: gives the oldest verb its answer and resumes the work's queue, or, unanswered, refuses it.
fn resolve(state: &mut ShardState, work: DbVolumeId, answer: Option<Answered>) {
  state.work_home.routing.remove(&work);
  let Some(queue) = state.work_home.queues.get_mut(&work) else {
    return;
  };
  queue.asking = false;
  queue.deferred_until = None;
  match answer {
    Some(answered) => {
      if let Some(head) = queue.waiting.front_mut() {
        match answered {
          Answered::Base(fetched) => head.fetched = Some(fetched),
          Answered::Verdict(reply) => head.verdict = Some(reply),
          Answered::Rebased(rebased) => head.rebased = Some(rebased),
          Answered::Admitted(admitted) => head.admitted = Some(admitted),
          Answered::Named(named) => head.named = Some(named),
          Answered::Route => {}
        }
      }
    }
    None => {
      if let Some(head) = queue.waiting.pop_front() {
        state.count(ASK_UNANSWERED, 1);
        answer_overloaded(state, work, head);
      }
    }
  }
  resume(state, work);
}

/// Answers a verb whose ask went unanswered, recording nothing, so its retry runs it again.
fn answer_overloaded(state: &mut ShardState, _work: DbVolumeId, head: ParkedVerb) {
  let verb = head.verb;
  state
    .work_home
    .by_request
    .remove(&key(verb.origin, verb.id));
  state.work_home.parked = state.work_home.parked.saturating_sub(1);
  let Some(route) = verb.route else {
    return;
  };
  let reply = refused_overloaded(state);
  crate::lease_wait::send_reply(state, route, reply);
}

/// Runs a work's parked verbs from the oldest, until one needs the owner again or none is left.
fn resume(state: &mut ShardState, work: DbVolumeId) {
  resume_pass(state, work);
  state.work_home.settling.clear();
}

/// One pass of [`resume`].
fn resume_pass(state: &mut ShardState, work: DbVolumeId) {
  loop {
    let Some(queue) = state.work_home.queues.get_mut(&work) else {
      return;
    };
    if queue.asking {
      return;
    }
    let Some(head) = queue.waiting.pop_front() else {
      state.work_home.queues.remove(&work);
      return;
    };
    let ParkedVerb {
      verb,
      fetched,
      verdict,
      rebased,
      admitted,
      named,
    } = head;
    let home = &mut state.work_home;
    home.by_request.remove(&key(verb.origin, verb.id));
    home.parked = home.parked.saturating_sub(1);
    let verb_key = (verb.origin, verb.id.client, verb.id.sequence);
    if state
      .db
      .partition()
      .pending_submit(work)
      .is_some_and(|pending| (pending.origin, pending.client, pending.sequence) == verb_key)
    {
      state.work_home.settling.push(verb_key);
    }
    let refused = keep_entries(state, work, fetched.as_ref());
    let home = &mut state.work_home;
    home.fetched = fetched;
    home.verdict = verdict;
    home.rebased = rebased;
    home.admitted = admitted;
    home.named = named;
    home.resuming = Some(work);
    crate::lease_wait::run_resumed(state, verb, refused, None);
    let home = &mut state.work_home;
    home.resuming = None;
    home.fetched = None;
    home.verdict = None;
    home.rebased = None;
    home.admitted = None;
    home.named = None;
  }
}

/// Keeps the base entries a verb fetched with the work (A-247), charged as the work's: the refusal its verb answers when
/// the budget cannot hold them.
fn keep_entries(
  state: &mut ShardState,
  work: DbVolumeId,
  fetched: Option<&FetchedBase>,
) -> Option<ReplyBody> {
  let fetched = fetched?;
  let new: Vec<(String, slates_merge::increment::Base)> = {
    let w = state.works.get(&work)?;
    fetched
      .entries
      .iter()
      .filter(|(path, _)| !w.entries.contains_key(*path))
      .map(|(path, base)| (path.clone(), base.clone()))
      .collect()
  };
  let bytes: u64 = new
    .iter()
    .map(|(path, base)| crate::work_charge::base_footprint(path, base))
    .fold(0, u64::saturating_add);
  if let Err(available) = crate::work_charge::grow(state, work, bytes) {
    state.count(ENTRIES_REFUSED, 1);
    return Some(crate::verbs::refused(Refusal::BudgetExceeded { available }));
  }
  if let Some(w) = state.works.get_mut(&work) {
    w.entries.extend(new);
  }
  None
}

/// Tells the green's owner, once the verb's transaction has committed, that `work` now holds version `base`
/// (`Some`) or is gone (`None`). A green on this shard takes the record in the verb's own transaction. Elsewhere it is
/// a task that first checks the change really took (so a rolled-back verb sends nothing) and whose loss only keeps
/// more history (A-247).
pub(crate) fn tell_floor(
  state: &mut ShardState,
  green: DbVolumeId,
  work: DbVolumeId,
  base: Option<u64>,
) {
  if green_elsewhere(state, green) {
    tell_floor_over_fleet(state, green, work, base);
    return;
  }
  if state.greens.contains_key(&green) {
    let now = state.clock.monotonic_ns();
    let op = match base {
      Some(base) => Op::WorkFloorSet { green, work, base },
      None => Op::WorkFloorDropped { green, work },
    };
    if state.db.mutate(&mut state.segment, &op, now).is_err() {
      state.count(FLOOR_NOT_SENT, 1);
    }
    let _ = crate::merge_service::settle_green_retention(state, green);
    crate::merge_service::checkpoint_green(state, green);
    return;
  }
  let owner_partition = crate::verbs::owner_of(crate::verbs::to_wire_volume(green));
  let shards = (
    crate::verbs::shard_of_partition(state, state.partition),
    crate::verbs::shard_of_partition(state, owner_partition),
  );
  let (Some(here), Some(owner)) = shards else {
    state.count(FLOOR_NOT_SENT, 1);
    return;
  };
  let budget = crate::lease::lease_bound_ns();
  let spawned = slates_rt::futures::spawn(async move {
    let took = state::with_state(|s| match base {
      Some(base) => s.works.get(&work).is_some_and(|w| w.base_version == base),
      None => s.db.partition().volume(work).is_none(),
    })
    .unwrap_or(false);
    if !took {
      return;
    }
    let sent = crate::xshard::call_in_shard_time_given_room(
      here,
      owner,
      move |s| crate::verbs::owner_floor(s, green, work, base),
      budget,
    )
    .await;
    if sent.is_none() {
      let _ = state::with_state_counted(|s| s.count(FLOOR_NOT_SENT, 1));
    }
  });
  match spawned {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
    }
    Err(_) => state.count(FLOOR_NOT_SENT, 1),
  }
}

/// [`tell_floor`] for a work on a holder node (A-697 part b): once the verb's transaction has committed, and only if its
/// change took, `MoveWorkFloor` is forwarded from the control shard to the green's owner on another node. The move is
/// monotone and idempotent there, so it goes with no completion record; one not answered within the parked verb's bound
/// is counted not sent, and its loss only keeps more history, as a lost floor message on one node does.
fn tell_floor_over_fleet(
  state: &mut ShardState,
  green: DbVolumeId,
  work: DbVolumeId,
  base: Option<u64>,
) {
  let (Some(here), Some(control)) = (
    crate::verbs::shard_of_partition(state, state.partition),
    state.shards.first().copied(),
  ) else {
    state.count(FLOOR_NOT_SENT, 1);
    return;
  };
  let region = state
    .node_regions
    .get(&state.fleet.host())
    .map_or(0, |region| region.0);
  let request_word = state.current_request.map_or(0, |(_, id)| id.word());
  // The node's own principal: the owner judges the move by the node it came from, never by an account's rights.
  let principal = Principal::Uid {
    host: state.origin_anchor,
    uid: 0,
  };
  let wire_green = crate::verbs::to_wire_volume(green);
  let request = crate::verbs::forwarded_request(
    state,
    request_word,
    principal,
    RequestBody::MoveWorkFloor {
      green: wire_green,
      work: crate::verbs::to_wire_volume(work),
      base,
    },
  );
  let deadline = park_bound_ns(state);
  let spawned = slates_rt::futures::spawn(async move {
    let took = state::with_state(|s| match base {
      Some(base) => s.works.get(&work).is_some_and(|w| w.base_version == base),
      None => s.db.partition().volume(work).is_none(),
    })
    .unwrap_or(false);
    if !took {
      return;
    }
    let reply = crate::xshard::task_within(
      here,
      control,
      move || async move {
        crate::verbs::resolve_and_forward(wire_green, region, None, vec![request])
          .await
          .0
          .first()
          .clone()
      },
      deadline,
    )
    .await;
    let _ = state::with_state_counted(|s| {
      s.count(
        if matches!(reply, Some(ReplyBody::WorkFloorMoved)) {
          FLOOR_TOLD_OWNER_NODE
        } else {
          FLOOR_NOT_SENT
        },
        1,
      );
    });
  });
  match spawned {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
    }
    Err(_) => state.count(FLOOR_NOT_SENT, 1),
  }
}

#[cfg(test)]
mod tests {
  use super::{DbVolumeId, ParkedVerb, Queue, defer_oldest, release_expired_at};

  /// The verb a test parks: a status asked by a local client, with nowhere to reply (answered `Overloaded` at its release,
  /// nothing is sent).
  fn parked_verb() -> ParkedVerb {
    ParkedVerb {
      verb: crate::lease_wait::ResumedVerb {
        route: None,
        origin: 0,
        id: slates_wire::request::RequestId::from_word(1),
        client_id: 1,
        principal: slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 1234,
        },
        body: slates_ipc::protocol::RequestBody::DaemonStatus,
      },
      fetched: None,
      verdict: None,
      rebased: None,
      admitted: None,
      named: None,
    }
  }

  /// A-434 (R3's class B: a verb held for its quorum commit). Do: on one shard whose failover delay is held at three of
  /// the owner lease's bounds, park a verb at the front of a work's queue and defer it as a verdict its owner answers at
  /// its quorum commit; release what has expired once the owner lease's bound has passed since the deferral, then once
  /// the failover delay has. Expect: the verb still held past the lease's bound, and released at the failover delay.
  /// Before, it was answered `Overloaded` at the lease's bound, though a commit at the quorum waits out a failover when
  /// a holder is lost, as a held reply does.
  #[test]
  fn a_verb_held_for_its_quorum_commit_waits_the_failover_delay() {
    crate::daemon::audit_on_shard(|state| {
      let lease = crate::lease::lease_bound_ns();
      state.failover_ns = 3 * lease;
      let work = DbVolumeId { bytes: [7; 16] };
      let mut queue = Queue::default();
      queue.waiting.push_back(parked_verb());
      queue.asking = true;
      state.work_home.queues.insert(work, queue);
      state.work_home.parked = 1;
      defer_oldest(state, work);
      let deferred = slates_machine::clock::monotonic_ns();
      let held = |state: &crate::state::ShardState| {
        state
          .work_home
          .queues
          .get(&work)
          .is_some_and(|queue| !queue.waiting.is_empty())
      };
      release_expired_at(state, deferred + lease);
      assert!(
        held(&*state),
        "the verb is still held once the owner lease's bound has passed"
      );
      release_expired_at(state, deferred + 3 * lease);
      assert!(!held(&*state), "the verb is released at the failover delay");
    });
  }

  /// How many journals holding a copy their green's owner composed, on `state` (`COPY_COMPOSED_BY_OWNER`).
  fn composed_by_owner(state: &crate::state::ShardState) -> u64 {
    state
      .refusals
      .get(super::COPY_COMPOSED_BY_OWNER)
      .copied()
      .unwrap_or(0)
  }

  /// A sealed work of `green` based at version 0 declaring `journal`, its increment composed nowhere (A-247).
  fn sealed_at_0(green: DbVolumeId, journal: Vec<super::VolumeOp>) -> super::SealedWork {
    super::SealedWork {
      work: DbVolumeId { bytes: [7; 16] },
      green,
      base_version: 0,
      journal,
      files: super::WorkFiles::default(),
      composed: None,
      computed: None,
    }
  }

  /// What the owner answered a rebase: the journal it restated, or the reply it refused with.
  fn answered(answer: &super::RebaseAnswer) -> String {
    match answer {
      super::RebaseAnswer::Rebased { journal, .. } => format!("rebased to {journal:?}"),
      super::RebaseAnswer::Reply(reply) => format!("answered {reply:?}"),
    }
  }

  /// A-626 part 4 (main's ruling: the green's owner composes a journal holding a copy, where a refusal would answer
  /// nothing): the owner counts each such composition, `merge.work.copy_composed_by_owner`. Do: on one shard, make a
  /// green; hand the owner's rebase step a sealed work whose journal copies the root at version 0 to `b`, its increment
  /// composed nowhere; then one whose journal makes a directory. Expect: the first counts once, whatever the owner
  /// answers it (the work model resolving copies is owed, GAPS); the second rebases clean and counts nothing more.
  #[test]
  fn a_journal_holding_a_copy_is_composed_by_the_owner_and_counted() {
    let (copy_answer, directory_rebased, counts) = crate::daemon::audit_on_shard(|state| {
      let principal = slates_db::catalog::Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let created = crate::verbs::dispatch(
        state,
        1,
        &principal,
        slates_ipc::protocol::RequestBody::CreateGreen {
          name: "copies".to_owned(),
          require_evidence: false,
          base: None,
        },
      );
      let slates_ipc::protocol::ReplyBody::GreenCreated { id } = created else {
        panic!("no green: {created:?}")
      };
      let green = crate::verbs::to_db_volume(id);
      let before = composed_by_owner(state);
      let copy = super::VolumeOp::Copy {
        from: String::new(),
        to: "b".to_owned(),
        version: 0,
        replace: false,
      };
      let copied = crate::verbs::owner_rebase(state, sealed_at_0(green, vec![copy]));
      let after_copy = composed_by_owner(state);
      let mkdir = super::VolumeOp::Mkdir {
        path: "d".to_owned(),
      };
      let made = crate::verbs::owner_rebase(state, sealed_at_0(green, vec![mkdir]));
      (
        answered(&copied),
        matches!(made, super::RebaseAnswer::Rebased { .. }),
        [before, after_copy, composed_by_owner(state)],
      )
    });
    eprintln!("the owner's answer to the copy: {copy_answer}");
    assert!(
      directory_rebased,
      "the owner rebased the work making a directory"
    );
    assert_eq!(
      counts,
      [0, 1, 1],
      "the count before, after the copy ({copy_answer}), after the directory"
    );
  }
}
