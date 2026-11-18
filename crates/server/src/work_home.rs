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
//! **Bounds.** A shard's parked verbs are bounded by its client credit, as `lease_wait`'s are; each ask has a deadline
//! (the lease bound), past which the verb answers `Overloaded`, unrecorded, and its retry runs it again.
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
/// Format: a work's increment composed on its own shard (A-247), the non-vacuity count of `composed`.
pub(crate) const COMPOSED_ON_WORK: &str = "merge.work.composed_on_work";
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
  /// route (A-298): the instant (the lease's clock) past which the verb stops waiting, is answered `Overloaded`, and the
  /// work is released (its retry joins the owner, or meets the route again).
  deferred_until: Option<u64>,
}

/// One work whose log could not be replayed at its shard's start because its green lives on another shard (A-247):
/// replayed through the green's owner once every partition has recovered (`rebuild_remote_works`).
pub(crate) struct Unbuilt {
  /// The work.
  pub(crate) work: DbVolumeId,
  /// Its green.
  pub(crate) green: DbVolumeId,
  /// The version it was created at.
  pub(crate) created_at: u64,
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
  /// The works whose log waits for their green's owner (A-247).
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
}

/// What [`gate`] decided for a verb.
pub(crate) enum Gate {
  /// Run it now, here.
  Run(RequestBody),
  /// It was parked (or refused); this is the reply `dispatch` returns.
  Answer(ReplyBody),
}

/// What a verb needs of the owner.
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
    sealed: SealedWork,
    /// The work's merge mode and the launcher's approval, if any (A-285): the green's owner holds no record of the work.
    policy: crate::verbs::SubmitPolicy,
  },
  Rebase {
    sealed: SealedWork,
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
      Some(verdict_need(
        state,
        work,
        seal(state, work),
        (evidence, approval, signature),
      ))
    }
    RequestBody::Rebase { .. } => {
      if home.rebased.is_some() {
        return None;
      }
      let sealed = seal(state, work);
      Some(
        base_missing(state, &sealed).unwrap_or_else(|| Need::Rebase {
          sealed: composed(state, sealed, Vec::new()),
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
      sealed_with_edit(state, work, (path, *at, *delete_len, insert))
        .map(|sealed| verdict_need(state, work, sealed, (evidence, approval, None)))
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
  (evidence, approval, signature): (&[[u8; 32]], crate::verbs::Approval, Option<Vec<u8>>),
) -> Need {
  if !route_committed(state, work) {
    return Need::Route;
  }
  base_missing(state, &sealed).unwrap_or_else(|| Need::Verdict {
    evidence: evidence.to_vec(),
    sealed: composed(state, sealed, evidence.to_vec()),
    policy: crate::verbs::SubmitPolicy::of_work(state, work, (approval, signature)),
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
  if state.work_home.fetched.is_some() {
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
    need_of(state, work, &body)
  } else {
    None
  };
  if !queued && need.is_none() {
    return Gate::Run(body);
  }
  park(state, client_id, principal, body, Some(work), need)
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
  let green_here =
    state.greens.contains_key(&green) || crate::verbs::shard_of_partition(state, owner).is_none();
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
  if let Need::Verdict { sealed, .. } | Need::Rebase { sealed } = &need {
    state.count(
      if sealed.composed.is_some() {
        COMPOSED_ON_WORK
      } else {
        COMPOSED_BY_OWNER
      },
      1,
    );
  }
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
  let Some(owner) = crate::verbs::shard_of_partition(state, owner_partition) else {
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

/// What the owner answered for one ask of a batch.
#[allow(
  clippy::large_enum_variant,
  reason = "an answer is the common outcome: boxing it would allocate per ask, to shrink the rare Deferred"
)]
enum Outcome {
  Answered(Answered),
  /// The owner took the verdict and answers when it commits.
  Deferred,
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
    let deadline = crate::lease::lease_bound_ns();
    let answers = crate::xshard::call_within_room(
      here,
      owner,
      move |s| {
        batch
          .into_iter()
          .map(|item| answer_on_owner(s, item))
          .collect::<Vec<_>>()
      },
      deadline,
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
    } => match crate::verbs::owner_judge(state, (origin, id), (sealed, evidence, policy), back) {
      Some(reply) => Outcome::Answered(Answered::Verdict(reply)),
      None => Outcome::Deferred,
    },
    Need::Rebase { sealed } => {
      Outcome::Answered(Answered::Rebased(crate::verbs::owner_rebase(state, sealed)))
    }
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
/// the verb's gate again ([`routes_placed`]). Past the lease bound the verb is answered `Overloaded`, unrecorded, so its
/// retry runs it again: nothing went to the owner, so nothing waits to be settled.
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

/// The oldest verb of `work` waits for its quorum commit, or for its route (A-298), up to the lease bound.
fn defer_oldest(state: &mut ShardState, work: DbVolumeId) {
  let deadline =
    slates_machine::clock::monotonic_ns().saturating_add(crate::lease::lease_bound_ns());
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
  let now = slates_machine::clock::monotonic_ns();
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
  let deadline = crate::lease::lease_bound_ns();
  let spawned = slates_rt::futures::spawn(async move {
    let took = state::with_state(|s| match base {
      Some(base) => s.works.get(&work).is_some_and(|w| w.base_version == base),
      None => s.db.partition().volume(work).is_none(),
    })
    .unwrap_or(false);
    if !took {
      return;
    }
    let sent = crate::xshard::call_within_room(
      here,
      owner,
      move |s| crate::verbs::owner_floor(s, green, work, base),
      deadline,
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
