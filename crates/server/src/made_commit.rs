//! The deferred-submit hook: a commit the server makes of a submitted version (a Subversion repository's revision,
//! `vcs_store::svn_made`) is answered only once its changeset has landed on the node's store and the version and the
//! register's entry naming it have committed, in one durable step (main's landing order, inbox
//! `w-svn-from-main-c2-made.md`: check, the store lands, the register commits naming the sequence, the answer).
//!
//! **Why.** A server-made commit was made by a task after its submit's step: the submit made its version durable and
//! answered in its own step, and the task landed the revision's changeset and committed the register's entry later. The
//! client was answered before the revision stood, and a `kill -9` between the version's commit and the revision's left
//! the version durable with no revision (the green past its records' youngest revision, so every later `svn commit`
//! was refused) or, retried, made twice.
//!
//! **How.**
//! 1. **The turn first.** A recorded submit to a Subversion repository's green takes the repository's landing turn
//!    (`svn_commit::landing::Turn`) before its verdict applies ([`take_turn`]), so no other landing (a client's commit,
//!    another submit) runs between the verdict and its revision. Where the turn is held, the submit parks ([`park`]) and
//!    a task waiting for the turn runs it again (`verbs::resume_all`) holding it.
//! 2. **The verdict held, never shown** (A-805; main 02:37). The verdict is judged and held (`Green::judge`), nothing of
//!    it applied: every reader of the green sees the head before it until the one step below. The revision is read
//!    from a preview of the held verdict, a member of the green's lineage of its own, made at the head in O(1) and
//!    retired in the submit's step (`Lineage::preview`, `Greens::preview`). The submit's step takes the request's route,
//!    marks it deferred, and hands the version's durable parts (`verbs::AcceptedParts`) to the made task ([`defer`]).
//! 3. **One step after the landing.** The task lands the revision's changeset on the store at the register's next
//!    sequence, then, in one step ([`commit`]): the verdict applied to the green as its chain entry records it
//!    (`verbs::apply_accepted`, the replay a recovery runs), the version's chain entry and what goes with it
//!    (`verbs::commit_accepted`), the register's entry naming the sequence (the tool's commit), the local work's settle
//!    (`verbs::settle_submit`) and the request's completion, in one transaction. Then the answer goes on the request's
//!    route.
//! 4. **A failed landing.** A store that refused the changeset leaves nothing applied or durable, so no reader saw
//!    anything of it: the secured retention is credited back and the request is answered `Overloaded`, unrecorded, so
//!    its retry runs ([`abandon`]). A revision the check refused (a version not made, counted `UNMADE`) commits the
//!    version alone, as before.
//!
//! **A crash.** Before the step's commit nothing of the version is durable and the register names no new sequence, so
//! recovery comes back to the state before the submit (recovery applies only what the register names); the changeset
//! at the unnamed sequence is the next landing's place, which replaces it, so the retry lands once
//! (`repository::svn_commit::tests`, the kill between the landing and the commit).
//!
//! **Not covered** (GAPS): a submit dispatched outside a recorded request (no answer waits on it) keeps the made task
//! after its step; a Mercurial repository's made changeset (`vcs_store::hg_made`, A-807) takes the hook but lands
//! nothing on the store before its step (its record is the register's entry), and a client's hg push takes no turn;
//! git's adopts this hook (w-git). The verdict's mapped files are made twice, once in the preview and once at the step (owed,
//! performance).

use std::collections::{BTreeMap, BTreeSet};

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{Refusal, ReplyBody};
use slates_wire::request::RequestId;

use crate::repository::svn_commit::landing::{self, Turn};
use crate::state::ShardState;
use crate::verbs::{AcceptedParts, ParkedSubmit};

/// Counter: a submit parked for its repository's landing turn. Format: a counter name in the daemon's status report.
const TURN_PARKED: &str = "vcs.made.turn_parked";
/// Counter: a submit refused `Overloaded` at the parked bound. Format: a counter name in the daemon's status report.
const TURN_PARK_FULL: &str = "vcs.made.turn_park_full";
/// Counter: a made commit's answer not handed to its client's shard; its completion answers the resend. Format: a
/// counter name in the daemon's status report.
const UNDELIVERED: &str = "vcs.made.undelivered";
/// Counter: a made commit abandoned before its step (its landing refused, or its green made again meanwhile): nothing
/// durable, answered `Overloaded` unrecorded. Format: a counter name in the daemon's status report.
pub(crate) const ABANDONED: &str = "vcs.made.abandoned";

/// The shard's made commits: the turn the running submit holds, the submits parked for a turn, and the turns a waiting
/// task took for them.
#[derive(Default)]
pub(crate) struct MadeCommits {
  /// The turn the running submit took before its verdict.
  held: Option<Held>,
  /// Whether the running submit's acceptance went to a made task (its work settles there).
  deferred: bool,
  /// A turn a waiting task took, for the parked submits it runs again.
  ready: BTreeMap<RepositoryId, Turn>,
  /// The submits parked for each repository's turn, in the order they came.
  parked: BTreeMap<RepositoryId, Vec<ParkedSubmit>>,
  /// The repositories a task waits on the turn of.
  waiting: BTreeSet<RepositoryId>,
}

/// A landing turn the running submit holds.
pub(crate) struct Held {
  /// The repository.
  repository: RepositoryId,
  /// Its turn.
  turn: Turn,
  /// The work, when the submit runs on its shard (`verbs::submit`); none for the owner's step of a work elsewhere,
  /// whose shard settles it when it is answered.
  local: Option<DbVolumeId>,
  /// The repository's tool: whose made commit the task makes.
  tool: Tool,
}

/// The tools whose made commits take the hook (module docs): git's adopts it (w-git).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
  /// A Subversion repository's revision (`vcs_store::svn_made`): landed on the node's store, then committed.
  Subversion,
  /// A Mercurial repository's changeset (`vcs_store::hg_made`): its record still held in the register's entry (no store
  /// landing yet; GAPS C2), committed in the one step.
  Mercurial,
}

/// The repository `green` is the green of and its tool, where its made commits take the hook.
fn made_repository_of(state: &ShardState, green: DbVolumeId) -> Option<(RepositoryId, Tool)> {
  if let Some(repository) = crate::vcs_store::svn_made::subversion_repository_of(state, green) {
    return Some((repository, Tool::Subversion));
  }
  crate::vcs_store::hg_made::mercurial_repository_of(state, green)
    .map(|repository| (repository, Tool::Mercurial))
}

/// Whether the running submit took its repository's turn ([`take_turn`]).
pub(crate) enum Taken {
  /// No turn is taken: not a Subversion or Mercurial repository's green, or no recorded request runs.
  Unhooked,
  /// Taken.
  Held,
  /// Held by another landing: the submit parks.
  Busy(RepositoryId),
}

/// Takes the landing turn of the Subversion or Mercurial repository `green` is the green of, for the running submit, before its
/// verdict (module docs, 1): a turn a waiting task took for parked submits first, else the turn where it is free.
/// `recorded`: whether a recorded request runs, whose answer can wait.
pub(crate) fn take_turn(
  state: &mut ShardState,
  green: DbVolumeId,
  local: Option<DbVolumeId>,
  recorded: bool,
) -> Taken {
  if !recorded {
    return Taken::Unhooked;
  }
  let Some((repository, tool)) = made_repository_of(state, green) else {
    return Taken::Unhooked;
  };
  let turn = match state.made_commits.ready.remove(&repository) {
    Some(turn) => Some(turn),
    None => Turn::take_now(state, repository),
  };
  match turn {
    Some(turn) => {
      state.made_commits.held = Some(Held {
        repository,
        turn,
        local,
        tool,
      });
      Taken::Held
    }
    None => Taken::Busy(repository),
  }
}

/// Gives back the turn the running submit took where its verdict did not use it (a refusal, a conflict).
pub(crate) fn release(state: &mut ShardState) {
  if let Some(held) = state.made_commits.held.take() {
    held.turn.give_back(state);
  }
}

/// Whether the running submit's acceptance went to a made task, cleared: its work settles there, not in its step.
pub(crate) fn take_deferred(state: &mut ShardState) -> bool {
  std::mem::take(&mut state.made_commits.deferred)
}

/// Whether the running recorded submit holds the landing turn of `green`'s repository: its verdict is then judged and
/// held, never applied in its step (module docs, 2).
pub(crate) fn holds(state: &ShardState, green: DbVolumeId) -> bool {
  state.current_request.is_some()
    && state.made_commits.held.as_ref().is_some_and(|held| {
      made_repository_of(state, green).map(|(repository, _)| repository) == Some(held.repository)
    })
}

/// The turn the running recorded submit holds for `green`'s repository, taken for its accept.
pub(crate) fn take_held(state: &mut ShardState, green: DbVolumeId) -> Option<Held> {
  state.current_request?;
  let (repository, _) = made_repository_of(state, green)?;
  if state
    .made_commits
    .held
    .as_ref()
    .is_none_or(|held| held.repository != repository)
  {
    return None;
  }
  state.made_commits.held.take()
}

/// An accept whose durable step waits for its commit's landing: its request, where its answer goes, its work when local,
/// and the version's durable parts.
pub(crate) struct Pending {
  /// The completion key's origin.
  origin: u64,
  /// The request.
  id: RequestId,
  /// Where its answer goes; none where the request carried no route (its completion answers it).
  route: Option<crate::merge_service::ReplyRoute>,
  /// The work, when local ([`Held::local`]).
  local: Option<DbVolumeId>,
  /// The version's durable parts.
  parts: AcceptedParts,
}

/// Hands the running submit's accept of `parts` to the made task holding `held`'s turn (module docs, 2): its route
/// taken and its request deferred, so its step commits nothing of it and answers nothing. The reply its step returns,
/// which no one is sent.
pub(crate) fn defer(
  state: &mut ShardState,
  held: Held,
  parts: AcceptedParts,
  (copies, judged): (
    Vec<crate::vcs_store::svn_made::MadeCopy>,
    slates_merge::engine::Judged,
  ),
) -> ReplyBody {
  let Some((origin, id)) = state.current_request else {
    held.turn.give_back(state);
    return crate::verbs::refused(Refusal::Overloaded { shard: state.shard });
  };
  let pending = Pending {
    origin,
    id,
    route: state.reply_route.take(),
    local: held.local,
    parts,
  };
  state.acceptance_deferred = true;
  state.made_commits.deferred = true;
  match held.tool {
    Tool::Subversion => crate::vcs_store::svn_made::make_deferred(
      state,
      (held.repository, held.turn),
      pending,
      (copies, judged),
    ),
    // Its changeset is made at the one step, from the green the verdict was applied to there: nothing lands before it,
    // so the held verdict is dropped here (its chain entry's form applies it, `verbs::apply_accepted`).
    Tool::Mercurial => {
      drop(judged);
      crate::vcs_store::hg_made::make_deferred(state, held.turn, pending, copies);
    }
  }
  crate::lease_wait::parked_reply()
}

/// A pending accept's request: its completion key and where its answer goes.
pub(crate) type Request = (u64, RequestId, Option<crate::merge_service::ReplyRoute>);

impl Pending {
  /// Its request.
  pub(crate) fn request(&self) -> Request {
    (self.origin, self.id, self.route)
  }

  /// The green and the version.
  pub(crate) fn version(&self) -> (DbVolumeId, u64) {
    (self.parts.green, self.parts.version)
  }

  /// The commit's message and identities.
  pub(crate) fn authorship(&self) -> &slates_ipc::protocol::Authorship {
    &self.parts.authorship
  }
}

/// Commits `pending` in one transaction with the tool's register commit `tool` (module docs, 3), then answers it on its
/// route: the version's parts first, then `tool` (refused, the version stands alone, as a version no commit was made
/// of does), then the local work's settle and the completion. `tool`'s answer; none where the version's parts were
/// refused or the green was made again meanwhile (then nothing of the tool's ran).
pub(crate) fn commit<T, E>(
  state: &mut ShardState,
  pending: Pending,
  tool: impl FnOnce(&mut ShardState) -> Result<T, E>,
) -> Option<Result<T, E>> {
  let (green, version) = pending.version();
  // The verdict was held, not applied (module docs, 2): it applies now, at the head it was judged at, as its chain
  // entry records it (`Green::apply_recorded`, what a recovery replays). A green that moved meanwhile commits nothing.
  if !crate::verbs::apply_accepted(state, &pending.parts) {
    abandon(state, (pending.request(), green));
    return None;
  }
  let Pending {
    origin,
    id,
    route,
    local,
    parts,
  } = pending;
  state.db.begin();
  state.current_request = Some((origin, id));
  state.reply_route = route;
  let (reply, made) = match crate::verbs::commit_accepted(state, parts) {
    Err(refusal) => (crate::verbs::refused(refusal), None),
    Ok(()) => {
      let made = tool(state);
      let object = slates_db::register::ObjectId(green.bytes);
      if crate::merge_service::placed_version(state, object).is_none_or(|placed| placed < version) {
        crate::merge_service::defer_acceptance(state, green, version);
      }
      let reply = crate::merge_service::accepted_reply(state, green, version);
      let reply = match local {
        Some(work) => crate::verbs::settle_submit(state, work, green, reply),
        None => reply,
      };
      (reply, Some(made))
    }
  };
  state.current_request = None;
  let route = state.reply_route.take();
  let deferred = std::mem::take(&mut state.acceptance_deferred);
  let reply = crate::verbs::recorded_unless_deferred(state, (origin, id), deferred, reply);
  let reply = match state.db.commit(&mut state.segment) {
    Ok(_) => reply,
    Err(error) => {
      crate::verbs::reconcile_unpublished_effects(state);
      crate::verbs::refused(crate::error::refusal_of_db(&error))
    }
  };
  if matches!(reply, ReplyBody::Refused { .. }) {
    // A refused step left the green past its durable head: made again from its chain.
    crate::verbs::reconcile_unpublished_effects(state);
  }
  if !deferred || matches!(reply, ReplyBody::Refused { .. }) {
    answer(state, route, (origin, id), reply);
  }
  made
}

/// Gives up a pending accept of `request` to `green` before its step (module docs, 4): nothing of it was applied or made
/// durable, so no reader saw it; the bytes its verdict secured are credited back, and the request answered
/// `Overloaded`, unrecorded, so its retry runs.
pub(crate) fn abandon(state: &mut ShardState, ((origin, id, route), green): (Request, DbVolumeId)) {
  let _ = crate::merge_service::settle_green_retention(state, green);
  state.count(ABANDONED, 1);
  let reply = crate::verbs::refused(Refusal::Overloaded { shard: state.shard });
  answer(state, route, (origin, id), reply);
}

/// Answers `reply` on `route`; counted where it was not handed on.
fn answer(
  state: &mut ShardState,
  route: Option<crate::merge_service::ReplyRoute>,
  key: (u64, RequestId),
  reply: ReplyBody,
) {
  if let Some(route) = route
    && !crate::verbs::answered_on(route, key, reply)
  {
    state.count(UNDELIVERED, 1);
  }
}

/// Parks `submit` for `repository`'s landing turn (module docs, 1), a task waiting for it started where none is: a
/// retry of a parked request takes its place. Refused `Overloaded` at the bound (the shard's client seats, as a whole
/// build's parked submits are) or where the task cannot start.
pub(crate) fn park(
  state: &mut ShardState,
  repository: RepositoryId,
  submit: ParkedSubmit,
) -> Result<(), Refusal> {
  let shard = state.shard;
  let bound = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  if let Some(joined) = state
    .made_commits
    .parked
    .get_mut(&repository)
    .and_then(|parked| {
      parked
        .iter_mut()
        .find(|parked| parked.key() == submit.key())
    })
  {
    *joined = submit;
    return Ok(());
  }
  let held: usize = state.made_commits.parked.values().map(Vec::len).sum();
  if held >= bound {
    state.count(TURN_PARK_FULL, 1);
    return Err(Refusal::Overloaded { shard });
  }
  if !state.made_commits.waiting.contains(&repository) {
    let started = slates_rt::futures::spawn(waiter(repository))
      .and_then(slates_rt::futures::detach)
      .is_ok();
    if !started {
      return Err(Refusal::Overloaded { shard });
    }
    state.made_commits.waiting.insert(repository);
  }
  state
    .made_commits
    .parked
    .entry(repository)
    .or_default()
    .push(submit);
  state.count(TURN_PARKED, 1);
  Ok(())
}

/// [`park`]'s task: `repository`'s turn once it is free, then the submits parked for it run again holding it, the
/// first to accept keeping it; a turn none kept is given back.
async fn waiter(repository: RepositoryId) {
  let turn = landing::turn(repository).await;
  let _ = crate::state::with_state(move |state| {
    state.made_commits.waiting.remove(&repository);
    let parked = state
      .made_commits
      .parked
      .remove(&repository)
      .unwrap_or_default();
    if let Some(turn) = turn {
      state.made_commits.ready.insert(repository, turn);
    }
    crate::verbs::resume_all(state, parked);
    if let Some(turn) = state.made_commits.ready.remove(&repository) {
      turn.give_back(state);
    }
  });
}
