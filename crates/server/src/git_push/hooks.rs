//! git's hooks as a push runs them on its repository's owner shard (A-665 part 3; §4.17.2 "Hooks,
//! in git's terms"; §4.17.1 "Policy"; A-597's inputs, A-622's policy and driver, A-665 part 2's
//! view; git's `execute_commands`, `update`, `run_receive_hook`, `run_update_hook`,
//! `copy_to_sideband` and the end of `cmd_receive_pack`, v2.50.1).
//!
//! **The points, in git's order.** `pre-receive` runs once, its standard input every command no
//! earlier check refused, in a view showing the push in git's quarantine (a push of deletes alone
//! brings no pack, so no quarantine). Its refusal answers each of those commands `pre-receive hook
//! declined`, after the hook's words, and no later hook runs. `update` runs for each such command
//! the repository's policy admits (A-622 part 2's check point first, as git's `update()` refuses
//! before it runs its hook), the ref, its old and its new value its arguments, in a view showing
//! the push among the repository's packs; its refusal answers that command `hook declined`, after
//! the hook's words and git's line `error: hook declined to update <ref>`. Then the verdicts and
//! the appends, and the report but its last flush. Then `post-receive`, its standard input every
//! applied command, in a view of the refs as the push left them: it only observes, and its words
//! follow the report, before the last flush, as git writes them.
//!
//! **A step at a time.** At each point the view is opened over the repository's store on the owner
//! shard, sealed a turn at a time (`GitHookView::seal_turn`, the edge yielding between turns),
//! shown at a path by the push's presenter, and the hook handed to the edge to run off the shard's
//! step ([`HookCall`]). Its answer comes back through `hook_ran`, which withdraws the view's
//! showing and closes it. An accepted hook's words go to band 2 at once, as git's
//! `copy_to_sideband` relays them while the hook runs; a refusing one's are its refusal's
//! (`receive::protected`). A hook's words are kept up to what the push may still hold (its
//! admission) and charged to it.
//!
//! **A hook that cannot run** (its view not opened or sealed, the presenter not showing it, its
//! program not started, past its bound or its client gone) refuses what it would have judged:
//! `pre-receive` every command, `update` its own, so no command goes through unjudged. At
//! `post-receive`, which only observes, it is counted ([`POST_RECEIVE_UNACCEPTED`]), as is a
//! `post-receive` that exits non-zero, whose status git ignores.
//!
//! **Where it differs from git.** Every `update` hook runs before any append, each view showing the
//! refs as they stood before the push, where git moves each ref right after its hook (without
//! `atomic`), so a later command's hook there sees an earlier ref moved. The band-2 lines of
//! commands refused otherwise (the policy's, the verdicts') follow every hook's words, where git
//! interleaves them command by command.
//!
//! **How long a hook may take.** Each run is held to the bound its hook has learned on the owner
//! shard (`ShardState::hook_times`, A-665 part 4b), none before a run of it finished, and taught
//! what it showed as its answer comes back; it ends too when its client goes (the edge's signal).

use std::path::PathBuf;

use slates_db::catalog::{
  RepositoryId, RepositoryPolicy, RepositoryRecord, VolumeId as DbVolumeId,
};
use slates_git::Admission;
use slates_git::out::Out;
use slates_git::receive::{
  self, Checked, Command, Outcome as Answered, Received, Requested, protected, write_hook_words,
  write_report_end, write_report_held,
};

use super::{
  AnswerState, Answering, Every, HookVerdicts, PushFailed, ReceivePack, ServerJudge, git_head,
  ref_of, words,
};
use crate::git_hooks::{
  GitHookRan, GitHookRefused, GitHookSite, GitHookView, PresentView, Presented, ReadRef, ShownPush,
  configured, run,
};
use crate::state::ShardState;
use crate::vcs_hooks::{
  CommittedView, HookInput, HookPoint, Outcome, PendingView, RefUpdate, git_post_receive,
  git_pre_receive, git_update,
};
use crate::vcs_store::{SealedPush, green_key};

/// Counter: a `post-receive` that did not run (its view not shown, its program not started, its
/// client gone) or did not exit 0, which git ignores and slates counts (the module docs). Format: a
/// counter name in the status report.
pub(crate) const POST_RECEIVE_UNACCEPTED: &str = "git_push.post_receive_unaccepted";

/// The point a push's hooks are at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
  /// `pre-receive`.
  PreReceive,
  /// `update`, for the command at this place among those no earlier check refused.
  Update(usize),
  /// `post-receive`.
  PostReceive,
}

impl At {
  /// The hook point it is.
  fn point(&self) -> HookPoint {
    match self {
      At::PreReceive => HookPoint::GitPreReceive,
      At::Update(_) => HookPoint::GitUpdate,
      At::PostReceive => HookPoint::GitPostReceive,
    }
  }
}

/// A hook's view under way, and where it is shown once sealed.
struct Open {
  /// The view.
  view: GitHookView,
  /// Where its presenter shows it.
  root: Option<PathBuf>,
}

/// The hooks of one push between its answer's steps (the module docs).
pub(super) struct PushHooks {
  /// The repository's policy as the answer read it: its hook at each point.
  policy: RepositoryPolicy,
  /// `HEAD`'s target: the default branch's ref; empty where the repository names none, whose views
  /// refuse it.
  head: Vec<u8>,
  /// The commands no earlier check refused, in the order sent: what `pre-receive` and the `update`
  /// hooks judge.
  judged: Vec<Command>,
  /// The push's options, as git hands its hooks them.
  options: Vec<Vec<u8>>,
  /// What the client asked for: whether a hook's words go to band 2.
  requested: Requested,
  /// Whether the push brought a pack: git keeps it in its quarantine for `pre-receive`.
  packed: bool,
  /// The point under way.
  at: At,
  /// Its view, once opened.
  open: Option<Open>,
  /// The views this push has opened: each one's number, as its hook's input names it.
  views: u64,
}

/// A push's answer while its hooks before the verdicts run (`AnswerState::Hooking`).
pub(super) struct Hooking {
  /// The repository's record as the answer read it.
  record: RepositoryRecord,
  /// The push's objects, sealed in quarantine.
  sealed: SealedPush,
  /// The push's name (the driver's module docs).
  name: [u8; 32],
  /// Its hooks.
  hooks: PushHooks,
  /// What they decided of its commands.
  verdicts: HookVerdicts,
}

/// A push's answer while `post-receive` runs (`AnswerState::Observing`).
pub(super) struct Observing {
  /// What was received, reported but for the report's last flush.
  received: Received,
  /// Its hooks.
  hooks: PushHooks,
}

/// A hook's run handed to the edge (the module docs, "A step at a time"): its point's input and its
/// view as shown.
#[derive(Debug)]
pub(crate) struct HookCall {
  /// The repository's policy: the hook configured at the input's point.
  policy: RepositoryPolicy,
  /// The view, as the hook sees it.
  presented: Presented,
  /// The point's input.
  input: HookInput,
  /// How many bytes of the hook's words are kept: what the push may still hold.
  words_cap: usize,
  /// The bound its hook has learned on this shard, none before a run of it finished.
  bound_ns: Option<u64>,
}

impl HookCall {
  /// Runs the hook until it ends, runs past its learned bound or `client_gone` resolves (A-622
  /// part 3's driver: the module docs).
  pub(crate) async fn run(self, client_gone: impl core::future::Future<Output = ()>) -> HookAnswer {
    let site = GitHookSite {
      policy: &self.policy,
      view: &self.presented,
      words_cap: self.words_cap,
    };
    HookAnswer(run(&site, &self.input, self.bound_ns, client_gone).await)
  }
}

/// What a hook's run answered.
#[derive(Debug)]
pub(crate) struct HookAnswer(Result<GitHookRan, GitHookRefused>);

/// What one turn at a hook's point did.
enum Turn {
  /// A turn of the view's sealing: more to come.
  Sealing,
  /// The view sealed and shown: the hook can run in it.
  Shown(Presented),
  /// The hook cannot run, and why, in slates' words.
  NotRun(Vec<u8>),
}

impl Hooking {
  /// The answer of `record`'s push `sealed`, named `name`, checked as `checked`, its hooks before
  /// the verdicts due.
  pub(super) fn new(
    record: RepositoryRecord,
    (sealed, name): (SealedPush, [u8; 32]),
    checked: &Checked,
  ) -> Hooking {
    let judged = checked
      .request
      .commands
      .iter()
      .zip(&checked.early)
      .filter(|(_, early)| early.is_none())
      .map(|(command, _)| command.clone())
      .collect();
    let head = record
      .default_branch
      .as_deref()
      .and_then(ref_of)
      .unwrap_or_default();
    let hooks = PushHooks {
      policy: record.policy.clone(),
      head,
      judged,
      options: checked.request.options.clone(),
      requested: checked.request.requested.clone(),
      packed: checked.unpacked.is_some(),
      at: At::PreReceive,
      open: None,
      views: 0,
    };
    Hooking {
      record,
      sealed,
      name,
      hooks,
      verdicts: HookVerdicts::default(),
    }
  }
}

impl PushHooks {
  /// One turn of the hook due at the point under way (the module docs): its view opened over
  /// `repository`'s store showing `pushed`, then sealed a turn at a time, then shown by
  /// `presenter`; or why the hook cannot run, the view closed.
  fn turn<P: PresentView>(
    &mut self,
    state: &mut ShardState,
    presenter: &mut P,
    (repository, pushed): (RepositoryId, Option<ShownPush<'_>>),
  ) -> Turn {
    if self.open.is_none() {
      let refs = shown_refs(state, repository);
      let read: Vec<ReadRef<'_>> = refs
        .iter()
        .map(|(name, green, version)| ReadRef {
          name: name.as_slice(),
          green: *green,
          version: *version,
        })
        .collect();
      match GitHookView::open(state, repository, (&read, &self.head), pushed) {
        Ok(view) => {
          self.open = Some(Open { view, root: None });
          self.views = self.views.saturating_add(1);
        }
        Err(refused) => return Turn::NotRun(words(&refused)),
      }
    }
    let Some(open) = self.open.as_mut() else {
      return Turn::NotRun(b"its view was not opened".to_vec());
    };
    let sealed = match open.view.seal_turn(state) {
      Ok(sealed) => sealed,
      Err(refused) => {
        self.close(state, presenter);
        return Turn::NotRun(words(&refused));
      }
    };
    if !sealed {
      return Turn::Sealing;
    }
    // A presentation reads the view's files from the repository's windows, so the shard's chunk store is lent to it.
    let shown = crate::vcs_store::lent_state(state, |state| presenter.present(state, &open.view));
    let Some(root) = shown else {
      self.close(state, presenter);
      return Turn::NotRun(b"nothing shows its view here".to_vec());
    };
    let presented = open.view.presented_at(&root);
    open.root = Some(root);
    Turn::Shown(presented)
  }

  /// Withdraws the showing of the view under way and closes it, giving back what it holds of the
  /// shard.
  fn close<P: PresentView>(&mut self, state: &mut ShardState, presenter: &mut P) {
    if let Some(open) = self.open.take() {
      if let Some(root) = &open.root {
        presenter.withdraw(root);
      }
      open.view.close(state);
    }
  }

  /// The run of `repository`'s hook at `input`'s point in `presented`, its words kept up to
  /// `words_cap`, held to the bound it has learned on this shard (`state`'s `hook_times`).
  fn call(
    &self,
    (state, repository): (&ShardState, RepositoryId),
    presented: Presented,
    input: HookInput,
    words_cap: usize,
  ) -> HookCall {
    let bound_ns = configured(&self.policy, input.point)
      .and_then(|hook| state.hook_times.bound_ns(repository, hook));
    HookCall {
      policy: self.policy.clone(),
      presented,
      input,
      words_cap,
      bound_ns,
    }
  }

  /// Teaches the shard's times of `repository`'s hook at `point` what its run, `answer`, showed:
  /// nothing where no hook ran.
  fn learn(
    &self,
    (state, repository): (&mut ShardState, RepositoryId),
    point: HookPoint,
    answer: &HookAnswer,
  ) {
    let Ok(ran) = &answer.0 else {
      return;
    };
    let (Some(lesson), Some(hook)) = (ran.lesson, configured(&self.policy, point)) else {
      return;
    };
    state
      .hook_times
      .teach(repository, &self.policy, hook, lesson);
  }

  /// Whether a hook is configured at `point`.
  fn configures(&self, point: HookPoint) -> bool {
    configured(&self.policy, point).is_some()
  }
}

/// One step of the hooks before the verdicts (the module docs): a turn of the point's view sealed,
/// its hook handed to the edge, or, past the last, the verdicts, the report but its last flush,
/// then `post-receive`'s first step.
pub(super) fn before<P: PresentView>(
  push: &mut ReceivePack<P>,
  state: &mut ShardState,
  mut hooking: Hooking,
  out: &mut impl Out,
) -> Result<Answering, PushFailed> {
  while let Some(point) = due(push, state, &mut hooking) {
    let pushed = hooking.hooks.packed.then_some(match point {
      HookPoint::GitPreReceive => ShownPush::Quarantined(&hooking.sealed),
      _ => ShownPush::Migrated(&hooking.sealed),
    });
    let repository = hooking.record.id;
    match hooking
      .hooks
      .turn(state, &mut push.presenter, (repository, pushed))
    {
      Turn::Sealing => {
        push.answer = AnswerState::Hooking(Box::new(hooking));
        return Ok(Answering::Turned);
      }
      Turn::Shown(presented) => {
        if let Some(input) = input_before(&hooking.hooks) {
          let call = hooking.hooks.call(
            (&*state, repository),
            presented,
            input,
            room_of(&push.admission),
          );
          push.answer = AnswerState::Hooking(Box::new(hooking));
          return Ok(Answering::Hook(Box::new(call)));
        }
        hooking.hooks.close(state, &mut push.presenter);
        not_run(
          &mut hooking,
          point,
          b"its command is not the push's".to_vec(),
        );
      }
      Turn::NotRun(why) => not_run(&mut hooking, point, why),
    }
  }
  let Hooking {
    record,
    sealed,
    name,
    hooks,
    verdicts,
  } = hooking;
  let mut decision = push.deciding((record, sealed, name), verdicts)?;
  decision.then = Some(hooks);
  push.decide_step(state, decision, out)
}

/// The push decided (`decide_step`'s last step): the report written but its last flush, then
/// `post-receive`'s first step.
pub(super) fn reported<P: PresentView>(
  push: &mut ReceivePack<P>,
  state: &mut ShardState,
  (received, mut hooks): (Received, PushHooks),
  out: &mut impl Out,
) -> Result<Answering, PushFailed> {
  write_report_held(&received, out).map_err(PushFailed::Exchange)?;
  hooks.at = At::PostReceive;
  observe(push, state, Observing { received, hooks }, out)
}

/// The point whose hook is due next before the verdicts, the point under way moved past those with
/// no hook or nothing to judge; `None` once the verdicts are due. An `update` hook is due for a
/// command the policy admits alone, as git's `update()` refuses before it runs its hook; one it
/// refuses is refused again by the verdicts' judge.
fn due<P>(
  push: &ReceivePack<P>,
  state: &mut ShardState,
  hooking: &mut Hooking,
) -> Option<HookPoint> {
  loop {
    match hooking.hooks.at {
      At::PreReceive => {
        let open = hooking.hooks.open.is_some();
        let wanted = hooking.hooks.configures(HookPoint::GitPreReceive);
        if open || (wanted && !hooking.hooks.judged.is_empty()) {
          return Some(HookPoint::GitPreReceive);
        }
        hooking.hooks.at = At::Update(0);
      }
      At::Update(place) => {
        let refused = hooking.verdicts.every.is_some();
        if refused || !hooking.hooks.configures(HookPoint::GitUpdate) {
          return None;
        }
        let command = hooking.hooks.judged.get(place)?.clone();
        if hooking.hooks.open.is_some() || admitted(push, state, hooking, &command) {
          return Some(HookPoint::GitUpdate);
        }
        hooking.hooks.at = At::Update(place.saturating_add(1));
      }
      At::PostReceive => return None,
    }
  }
}

/// Whether the repository's policy admits `command` (A-622 part 2's check point, as the verdicts'
/// judge asks it).
fn admitted<P>(
  push: &ReceivePack<P>,
  state: &mut ShardState,
  hooking: &mut Hooking,
  command: &Command,
) -> bool {
  let mut judge = ServerJudge {
    state,
    push: &mut hooking.sealed,
    record: &hooking.record,
    principal: &push.principal,
    name: hooking.name,
    hooks: &mut HookVerdicts::default(),
    admitting: &mut None,
    preparing: &mut None,
  };
  judge.admitted(command).is_ok()
}

/// The input of the hook due before the verdicts at the point under way: `pre-receive` every
/// command it judges, `update` the one under way.
fn input_before(hooks: &PushHooks) -> Option<HookInput> {
  let view = PendingView(hooks.views);
  match hooks.at {
    At::PreReceive => {
      let updates: Vec<RefUpdate> = hooks.judged.iter().map(ref_update).collect();
      Some(git_pre_receive(view, &updates, Some(&hooks.options)))
    }
    At::Update(place) => {
      let command = hooks.judged.get(place)?;
      Some(git_update(view, &ref_update(command)))
    }
    At::PostReceive => None,
  }
}

/// A hook before the verdicts that cannot run at `point`, for `why` (the module docs):
/// `pre-receive` refuses every command, `update` its own; the point under way moves on.
fn not_run(hooking: &mut Hooking, point: HookPoint, why: Vec<u8>) {
  let line = [
    b"the repository's ".as_slice(),
    point_name(point).as_bytes(),
    b" hook could not run: ",
    why.as_slice(),
  ]
  .concat();
  match hooking.hooks.at {
    At::PreReceive => {
      hooking.verdicts.every = Some(Every {
        reason: super::HOOK_NOT_RUN,
        words: Vec::new(),
        why: Some(line),
      });
      hooking.hooks.at = At::Update(0);
    }
    At::Update(place) => {
      if let Some(command) = hooking.hooks.judged.get(place) {
        let named = [command.name.as_slice(), b": ", line.as_slice()].concat();
        let declined = receive::Declined {
          reason: super::HOOK_NOT_RUN.to_vec(),
          hook: Vec::new(),
          message: Some(receive::message(&named)),
        };
        hooking.verdicts.each.insert(command.name.clone(), declined);
      }
      hooking.hooks.at = At::Update(place.saturating_add(1));
    }
    At::PostReceive => {}
  }
}

/// One step of `post-receive` (the module docs): its view opened at the refs the push left, sealed,
/// shown and its hook handed over; or, with no hook configured or nothing applied, or a view that
/// cannot be shown (counted), the report's last flush and the answer.
pub(super) fn observe<P: PresentView>(
  push: &mut ReceivePack<P>,
  state: &mut ShardState,
  mut observing: Observing,
  out: &mut impl Out,
) -> Result<Answering, PushFailed> {
  let applied = applied(&observing.received);
  if applied.is_empty() || !observing.hooks.configures(HookPoint::GitPostReceive) {
    return finish(observing.received, out);
  }
  match observing
    .hooks
    .turn(state, &mut push.presenter, (push.repository, None))
  {
    Turn::Sealing => {
      push.answer = AnswerState::Observing(Box::new(observing));
      Ok(Answering::Turned)
    }
    Turn::Shown(presented) => {
      let view = CommittedView(observing.hooks.views);
      let input = git_post_receive(view, &applied, Some(&observing.hooks.options));
      let call = observing.hooks.call(
        (&*state, push.repository),
        presented,
        input,
        room_of(&push.admission),
      );
      push.answer = AnswerState::Observing(Box::new(observing));
      Ok(Answering::Hook(Box::new(call)))
    }
    Turn::NotRun(_) => {
      state.count(POST_RECEIVE_UNACCEPTED, 1);
      finish(observing.received, out)
    }
  }
}

/// The report's last flush, and the answer.
fn finish(received: Received, out: &mut impl Out) -> Result<Answering, PushFailed> {
  write_report_end(&received, out).map_err(PushFailed::Exchange)?;
  Ok(Answering::Answered(Box::new(received)))
}

/// Takes what the hook the last step handed the edge answered (the module docs): its view's showing
/// withdrawn and the view closed; before the verdicts, its decision kept and the point under way
/// moved on; at `post-receive`, its words relayed and the report's last flush written. Refused for
/// a push no hook of which runs.
pub(super) fn ran<P: PresentView>(
  push: &mut ReceivePack<P>,
  state: &mut ShardState,
  answer: HookAnswer,
  out: &mut impl Out,
) -> Result<(), PushFailed> {
  match core::mem::replace(&mut push.answer, AnswerState::Done) {
    AnswerState::Hooking(mut hooking) => {
      hooking.hooks.close(state, &mut push.presenter);
      let point = hooking.hooks.at.point();
      hooking
        .hooks
        .learn((&mut *state, push.repository), point, &answer);
      decided(&mut hooking, &mut push.admission, answer, out);
      push.answer = AnswerState::Hooking(hooking);
      Ok(())
    }
    AnswerState::Observing(mut observing) => {
      observing.hooks.close(state, &mut push.presenter);
      observing.hooks.learn(
        (&mut *state, push.repository),
        HookPoint::GitPostReceive,
        &answer,
      );
      let (through, said) = judged(answer, HookPoint::GitPostReceive);
      if !through {
        state.count(POST_RECEIVE_UNACCEPTED, 1);
      }
      let Observing { received, .. } = *observing;
      let requested = &received.request.requested;
      relay((requested, &mut push.admission), &said.hook_words, out);
      write_report_end(&received, out).map_err(PushFailed::Exchange)?;
      push.answer = AnswerState::Reported(Box::new(received));
      Ok(())
    }
    other => {
      push.answer = other;
      Err(PushFailed::Unchecked)
    }
  }
}

/// Keeps what the hook before the verdicts at the point under way decided (the module docs):
/// through, its words relayed at once; refused, every command `pre-receive hook declined` or its
/// own `hook declined`, the words its refusal's.
fn decided(
  hooking: &mut Hooking,
  admission: &mut Admission,
  answer: HookAnswer,
  out: &mut impl Out,
) {
  let (through, said) = judged(answer, hooking.hooks.at.point());
  if through {
    relay((&hooking.hooks.requested, admission), &said.hook_words, out);
  } else {
    let words = kept(admission, said.all());
    match hooking.hooks.at {
      At::PreReceive => {
        hooking.verdicts.every = Some(Every {
          reason: protected::PRE_RECEIVE_HOOK_DECLINED,
          words,
          why: None,
        });
      }
      At::Update(place) => {
        if let Some(command) = hooking.hooks.judged.get(place) {
          let declined = protected::hook_declined(&command.name, words);
          hooking.verdicts.each.insert(command.name.clone(), declined);
        }
      }
      At::PostReceive => {}
    }
  }
  hooking.hooks.at = match hooking.hooks.at {
    At::PreReceive => At::Update(0),
    At::Update(place) => At::Update(place.saturating_add(1)),
    At::PostReceive => At::PostReceive,
  };
}

/// What a hook said, as the push relays it.
struct Said {
  /// Its own words.
  hook_words: Vec<u8>,
  /// slates' line for an end git does not have (past its bound, its client gone, not started, its
  /// view refused).
  ours: Vec<u8>,
}

impl Said {
  /// Its own words, then slates' line.
  fn all(self) -> Vec<u8> {
    [self.hook_words, self.ours].concat()
  }
}

/// Whether the hook at `point` let its commands through, and what it said (the module docs): no
/// hook configured lets them through (A-597), exit 0 does, anything else does not.
fn judged(answer: HookAnswer, point: HookPoint) -> (bool, Said) {
  let said = |hook_words: Vec<u8>, ours: Option<String>| Said {
    hook_words,
    ours: ours.map_or_else(Vec::new, |what| {
      let line = format!("the repository's {} hook {what}", point_name(point));
      receive::message(line.as_bytes())
    }),
  };
  let ran = match answer.0 {
    Ok(ran) => ran,
    Err(refused) => {
      return (
        false,
        said(Vec::new(), Some(format!("could not run: {refused:?}"))),
      );
    }
  };
  match ran.outcome {
    None => (true, said(Vec::new(), None)),
    Some(Outcome::Accepted { words }) => (true, said(words.kept, None)),
    Some(Outcome::Refused { words, .. }) => (false, said(words.kept, None)),
    Some(Outcome::TimedOut {
      bound_ns, words, ..
    }) => (
      false,
      said(
        words.kept,
        Some(format!("ran past its bound of {bound_ns} ns")),
      ),
    ),
    Some(Outcome::Abandoned) => (
      false,
      said(Vec::new(), Some("was ended: its client left".to_owned())),
    ),
    Some(Outcome::Unstartable { cause }) => (
      false,
      said(
        Vec::new(),
        Some(format!("could not be started ({cause:?})")),
      ),
    ),
  }
}

/// `words` relayed on band 2 as git's `copy_to_sideband` relays them, once charged to the push's
/// room; past it, not.
fn relay((requested, admission): (&Requested, &mut Admission), words: &[u8], out: &mut impl Out) {
  let words = kept(admission, words.to_vec());
  write_hook_words(requested, &words, out);
}

/// `words`, charged to the push's room; nothing past it.
fn kept(admission: &mut Admission, words: Vec<u8>) -> Vec<u8> {
  let len = u64::try_from(words.len()).unwrap_or(u64::MAX);
  if admission.charge(len).is_ok() {
    words
  } else {
    Vec::new()
  }
}

/// How many bytes of a hook's words the push may keep: what its room still holds.
fn room_of(admission: &Admission) -> usize {
  usize::try_from(admission.left()).unwrap_or(usize::MAX)
}

/// The applied commands of `received`, as `post-receive` is handed them.
fn applied(received: &Received) -> Vec<RefUpdate> {
  received
    .request
    .commands
    .iter()
    .zip(&received.outcomes)
    .filter(|(_, outcome)| matches!(outcome, Answered::Ok(_)))
    .map(|(command, _)| ref_update(command))
    .collect()
}

/// `command` as git's hooks are handed it: its old and new ids in hex and its ref.
fn ref_update(command: &Command) -> RefUpdate {
  RefUpdate {
    old: command.old.to_string(),
    new: command.new.to_string(),
    name: String::from_utf8_lossy(&command.name).into_owned(),
  }
}

/// git's name of `point`.
fn point_name(point: HookPoint) -> &'static str {
  match point {
    HookPoint::GitPreReceive => "pre-receive",
    HookPoint::GitUpdate => "update",
    HookPoint::GitPostReceive => "post-receive",
    // Another tool's point, which no git push reaches.
    _ => "",
  }
}

/// The refs a hook's view shows (the module docs): each of `repository`'s refs as it stands now, in
/// git's order, by its green and the green's version the newest of the store's versions of it is; a
/// green no push gave a git head shows none, as the advertisement shows none.
fn shown_refs(state: &ShardState, repository: RepositoryId) -> Vec<(Vec<u8>, DbVolumeId, u64)> {
  let Some(store) = state.vcs_stores.git_ref(repository) else {
    return Vec::new();
  };
  let (_, git) = store;
  let repositories = state.db.partition().repositories();
  let mut refs: Vec<(Vec<u8>, DbVolumeId, u64)> = repositories
    .branches(repository, "")
    .filter_map(|(name, green)| {
      let name = ref_of(name)?;
      let key = green_key(green);
      git_head(store, key)?;
      let version = *git.versions(key).last()?;
      Some((name, green, version))
    })
    .collect();
  refs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
  refs
}

/// Gives back what a dropped push's answer held (the module docs, "A step at a time"): its sealed
/// push and a hook's open view, through the shard's state; logged where the state is not lent to
/// the drop.
pub(super) fn dropped(answer: AnswerState) {
  let (view, sealed) = match answer {
    AnswerState::Hooking(hooking) => {
      let Hooking { sealed, hooks, .. } = *hooking;
      (hooks.open.map(|open| open.view), Some(sealed))
    }
    AnswerState::Observing(observing) => (observing.hooks.open.map(|open| open.view), None),
    _ => return,
  };
  if view.is_none() && sealed.is_none() {
    return;
  }
  let given = crate::state::with_state(|state| {
    if let Some(view) = view {
      view.close(state);
    }
    if let Some(sealed) = sealed {
      sealed.abandon(state);
    }
  });
  if given.is_none() {
    slates_machine::log_line!(
      "slates-server: a git push dropped mid-answer could not give back its quarantine or its \
       hook's view: the shard's state was not lent to its drop"
    );
  }
}
