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
//! after its step; a client's hg push takes no turn (a Mercurial made changeset lands on the store before its step
//! since A-767 C1, `vcs_store::hg_made`). The verdict's mapped files are made twice, once in the preview and once at
//! the step (owed, performance).
//!
//! **git** (C5 2b piece 3, `vcs_store::commits::make_deferred`): the preview is kept across turns (A-816), the version's
//! map stepped over it, the commit's objects homed in the region and landed on the node's store as one changeset, and
//! the record naming it (version 5) written and committed with the version in the one step.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{Refusal, ReplyBody, VcsTool};
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
  /// Whether a made commit's one step runs ([`committing`]).
  committing: bool,
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
  /// A git repository's commit (`vcs_store::commits`, C5 2b piece 3): its map stepped over the held verdict's preview,
  /// its objects homed and landed on the node's store as one changeset, then recorded and committed in the one step.
  Git,
}

/// The repository `green` is the green of and its tool, where its made commits take the hook.
fn made_repository_of(state: &ShardState, green: DbVolumeId) -> Option<(RepositoryId, Tool)> {
  if let Some(repository) = crate::vcs_store::svn_made::subversion_repository_of(state, green) {
    return Some((repository, Tool::Subversion));
  }
  if let Some(repository) = crate::vcs_store::hg_made::mercurial_repository_of(state, green) {
    return Some((repository, Tool::Mercurial));
  }
  crate::vcs_store::paths::git_repository_of(state, green).map(|repository| (repository, Tool::Git))
}

/// Whether the running submit took its repository's turn ([`take_turn`]).
pub(crate) enum Taken {
  /// No turn is taken: not a Subversion, Mercurial or git repository's green, or no recorded request runs.
  Unhooked,
  /// Taken.
  Held,
  /// Held by another landing: the submit parks.
  Busy(RepositoryId),
  /// A Subversion repository's green, with no recorded request running (a bare dispatch, which no answer can wait on):
  /// its version, its revision on the node's store and the register's entry are one step whose answer waits for the
  /// store, so the submit is refused `RecordedSubmitRequired`, nothing judged (main's rulings of 2026-11-23: the store
  /// is the truth, with no second path).
  Unrecorded(VcsTool),
}

/// Takes the landing turn of the Subversion, Mercurial or git repository `green` is the green of, for the running submit, before its
/// verdict (module docs, 1): a turn a waiting task took for parked submits first, else the turn where it is free.
/// `recorded`: whether a recorded request runs, whose answer can wait.
pub(crate) fn take_turn(
  state: &mut ShardState,
  green: DbVolumeId,
  local: Option<DbVolumeId>,
  recorded: bool,
) -> Taken {
  let Some((repository, tool)) = made_repository_of(state, green) else {
    return Taken::Unhooked;
  };
  if !recorded {
    return match tool {
      Tool::Subversion => Taken::Unrecorded(VcsTool::Subversion),
      // GAPS (main's sweep of 2026-11-23): git's and Mercurial's made commits outside a recorded request, their
      // owners'.
      Tool::Mercurial | Tool::Git => Taken::Unhooked,
    };
  }
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

/// The tool of the repository `green` is the green of, where its made commits take the hook: what a held verdict with
/// no turn to commit it under is refused naming (`verbs::accepted_answer`).
pub(crate) fn tool_of(state: &ShardState, green: DbVolumeId) -> Option<VcsTool> {
  made_repository_of(state, green).map(|(_, tool)| match tool {
    Tool::Subversion => VcsTool::Subversion,
    Tool::Mercurial => VcsTool::Mercurial,
    Tool::Git => VcsTool::Git,
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
  /// The held verdict's preview, where the tool keeps one across turns ([`hold_preview`]): what its made commit is built
  /// against while the verdict is held, retired in the one step ([`commit`]) or when the accept is given up.
  preview: HeldPreview,
}

/// A held verdict's preview kept across turns (P2, main's ruling of 2026-11-21 07:30: git's made commit builds its path
/// map, a slice a turn, against the version the verdict makes, before the one step applies it). Given back in the step
/// that ends the accept; one dropped otherwise (a task ended early) gives its scratch member back as it drops, where the
/// shard's state is not borrowed then.
#[derive(Default)]
struct HeldPreview {
  /// The preview.
  preview: Option<crate::greens::Preview>,
  /// Whether it has been lent after the submit's step: the counter's.
  lent_later: bool,
}

impl HeldPreview {
  /// Retires the preview in this step, once.
  fn retire(&mut self, state: &mut ShardState) {
    if let Some(preview) = self.preview.take() {
      state.greens.retire_preview(preview);
    }
  }
}

impl Drop for HeldPreview {
  fn drop(&mut self) {
    if let Some(preview) = self.preview.take() {
      let _ = crate::state::with_state_counted(|state| state.greens.retire_preview(preview));
    }
  }
}

/// Counter: a held verdict's preview lent after the submit's step that made it, i.e. kept across a turn or more (P2's
/// non-vacuity counter for main's 07:30 ruling). Format: a counter name in the daemon's status report.
pub(crate) const PREVIEW_HELD_ACROSS_TURNS: &str = "vcs.made.preview_held_across_turns";

/// Keeps a preview of `judged`, the accept's held verdict, with `pending` across turns: the green made as the verdict
/// would make it, while the green itself shows nothing of it (`Greens::preview`, A-805). For a tool whose made commit is
/// built a slice a turn against the version (git's path map). Whether one is held.
pub(crate) fn hold_preview(
  state: &mut ShardState,
  pending: &mut Pending,
  judged: slates_merge::engine::Judged,
) -> bool {
  pending.preview.retire(state);
  let green = pending.parts.green;
  pending.preview.preview = state.greens.preview(&green, judged).ok();
  pending.preview.preview.is_some()
}

/// `read` over `pending`'s held preview, on any turn until its one step: the green as its held verdict makes it. The
/// first lend counts [`PREVIEW_HELD_ACROSS_TURNS`]. None where no preview is held or its lineage is busy.
#[cfg_attr(not(test), allow(dead_code))] // Its caller outside tests is git's made commit (w-git, C5 2b piece 3).
pub(crate) fn with_held_preview<R>(
  state: &mut ShardState,
  pending: &mut Pending,
  read: impl FnOnce(&slates_merge::engine::Green) -> R,
) -> Option<R> {
  let preview = pending.preview.preview.as_ref()?;
  let lent = state.greens.with_preview(preview, read).ok()?;
  if !pending.preview.lent_later {
    pending.preview.lent_later = true;
    state.count(PREVIEW_HELD_ACROSS_TURNS, 1);
  }
  Some(lent)
}

/// `read` over `pending`'s held preview with the shard's store and the repositories' stores lent beside it, on any turn
/// until its one step: what git's made commit steps its version's map with (`vcs_store::paths`, which keeps the map in
/// the repository's store). The first lend counts [`PREVIEW_HELD_ACROSS_TURNS`]. None where no preview is held or its
/// lineage is busy.
pub(crate) fn with_held_preview_and_stores<R>(
  state: &mut ShardState,
  pending: &mut Pending,
  read: impl FnOnce(
    &slates_merge::engine::Green,
    (
      &mut slates_vfs::volume::Store,
      &mut crate::vcs_store::VcsStores,
    ),
  ) -> R,
) -> Option<R> {
  let preview = pending.preview.preview.as_ref()?;
  let ShardState {
    greens,
    store,
    vcs_stores,
    ..
  } = &mut *state;
  let lent = greens
    .with_preview(preview, |engine| read(engine, (store, vcs_stores)))
    .ok()?;
  if !pending.preview.lent_later {
    pending.preview.lent_later = true;
    state.count(PREVIEW_HELD_ACROSS_TURNS, 1);
  }
  Some(lent)
}

/// Whether the running step is a made commit's one step ([`commit`]): the tool's own commit runs in it, so the version's
/// commit is not made again by the accept's after-steps (`vcs_store::commits::after_submit`).
pub(crate) fn committing(state: &ShardState) -> bool {
  state.made_commits.committing
}

/// Gives `pending` up before its step: its preview retired, then [`abandon`].
pub(crate) fn give_up(state: &mut ShardState, mut pending: Pending) {
  pending.preview.retire(state);
  let green = pending.parts.green;
  abandon(state, (pending.request(), green));
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
    preview: HeldPreview::default(),
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
    // Its changeset is made from a preview of the held verdict and landed on the store before the one step (A-767 C1).
    Tool::Mercurial => {
      crate::vcs_store::hg_made::make_deferred(state, held.turn, pending, (copies, judged));
    }
    // Its map is stepped over a preview of the held verdict kept across turns (A-816), then its objects landed on the
    // store before the one step (C5 2b piece 3).
    Tool::Git => crate::vcs_store::commits::make_deferred(state, held.turn, pending, judged),
  }
  crate::lease_wait::parked_reply()
}

impl Pending {
  /// A pending accept of `parts` with no request (a test's), answered nowhere.
  #[cfg(test)]
  pub(crate) fn for_test(parts: AcceptedParts) -> Pending {
    Pending {
      origin: 0,
      id: RequestId::default(),
      route: None,
      local: None,
      parts,
      preview: HeldPreview::default(),
    }
  }
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
  mut pending: Pending,
  tool: impl FnOnce(&mut ShardState) -> Result<T, E>,
) -> Option<Result<T, E>> {
  // The held preview, if any, lives until this step and is retired in it, whatever the step does.
  let mut preview = std::mem::take(&mut pending.preview);
  let made = commit_held(state, pending, tool);
  preview.retire(state);
  made
}

/// [`commit`]'s step, its preview held by the caller.
fn commit_held<T, E>(
  state: &mut ShardState,
  mut pending: Pending,
  tool: impl FnOnce(&mut ShardState) -> Result<T, E>,
) -> Option<Result<T, E>> {
  let (green, version) = pending.version();
  // The verdict was held, not applied (module docs, 2): it applies now, at the head it was judged at, as its chain
  // entry records it (`Green::apply_recorded`, what a recovery replays). A green that moved meanwhile commits nothing.
  if !crate::verbs::apply_accepted(state, &mut pending.parts) {
    abandon(state, (pending.request(), green));
    return None;
  }
  let Pending {
    origin,
    id,
    route,
    local,
    parts,
    ..
  } = pending;
  state.db.begin();
  state.current_request = Some((origin, id));
  state.reply_route = route;
  state.made_commits.committing = true;
  let accepted = crate::verbs::commit_accepted(state, parts);
  state.made_commits.committing = false;
  let (reply, made) = match accepted {
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
  let reply = match crate::verbs::commit_settled(state) {
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

#[cfg(test)]
mod teardown_tests {
  /// Main's order of 2026-11-23 (CLAUDE.md section 2 item 6: no product panic in a destructor). A shard's state stays
  /// installed until its thread ends (`state::install`; nothing takes it), so a daemon's stop drops it in the thread's
  /// local storage teardown, and a landing turn it holds gives itself back from its `Drop` (`landing::Turn`) through
  /// `state::with_state_counted`. Do: on a stored daemon, take a Subversion repository's landing turn and leave it held
  /// in the shard's state (a waiting task's ready turn), then stop the daemon. Expect: the daemon stops. Failing first:
  /// the turn's give-back read the state's thread-local during its own destruction, which panicked in a destructor
  /// ("cannot access a Thread Local Storage value during or after destruction"), aborting the process.
  #[test]
  fn a_daemon_stopped_with_a_landing_turn_held_ends_without_a_panic() {
    use crate::repository::svn_commit::tests::{on, stored, subversion};
    stored(|| async move {
      let (svn, _) = subversion("held-turn").await;
      on(|state| {
        let turn = super::Turn::take_now(state, svn).expect("the repository's turn free");
        state.made_commits.ready.insert(svn, turn);
      });
    });
  }
}
