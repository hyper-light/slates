//! git's push on its repository's owner shard (A-654 part 2; §4.17.1 "A push is a work" items 1 to 5 and "Policy";
//! §4.17.2 "A push" items 1 to 7, "Access", "Protected branches, in git's terms" and "Hooks, in git's terms"; A-620's
//! session, A-622's policy, A-635's append, A-638's views, A-642, A-644 and its part 2's check point, A-662's store):
//! the one driver both edges call, the HTTP site (A-625) and the SSH channel (A-651).
//!
//! **A step at a time.** [`ReceivePack::start`] makes one push's session for a principal and writes the advertisement
//! when the edge's transport opens with it (SSH; smart HTTP's `info/refs`, whose session then ends);
//! [`ReceivePack::input`] and [`ReceivePack::end`] hand it the client's bytes as they arrive, a thin pack's bases and
//! its checks read from the repository's index through A-638's view; `ReceivePack::answer_step` makes the pack's checks
//! a turn at a time (A-719: every object checked as git checks a push, then the connectivity walk, each turn the shard's
//! step quantum), decides the checked push and writes git's report to the edge's bytes, a step at a time while its
//! hooks run (A-665 part 3): [`Answering::Turned`] after each turn of the checks or of a hook's view's sealing, and
//! [`Answering::Hook`] for a hook the edge
//! runs off the shard's step and hands back through `ReceivePack::hook_ran` (`hooks`, Unix, where the hook driver
//! runs). Each call is one step on the owner shard; between steps the driver holds only its push's sealed quarantine
//! and a hook's view, which a push dropped between them gives back, so an edge's task keeps the driver across its own
//! awaits. Every read of the store is lent the
//! room the shard can admit now (`vcs_store::with_room`), since A-614's store parses within its content store's room
//! and a settled store has none (A-662); the request, the pack and what reading and answering them builds are charged
//! to the room the edge gives the push, as each request on the HTTP site is (A-625).
//!
//! **What is advertised** (item 1). Each branch whose green holds a git head in the repository's store (A-662), by its
//! ref, in git's order: a namespace name `N` is the ref `refs/heads/N`, a name beginning `refs/` is the ref of that
//! name (a pushed tag `refs/tags/v1` is bound under that name), and a name beginning `refs/heads/` is no git ref, since
//! its ref is another name's; so names and refs map one to one. A green no push gave a git head (a new repository's
//! default branch, a branch the CLI made) is not advertised: git sees an unborn branch. A ref the pusher may not read
//! under the repository's policy (A-622's Read) is hidden as git's `receive.hideRefs` hides one: left out, and a
//! command naming it refused `deny updating a hidden ref`. The policy that hides is the one the push started under, as
//! git reads its configuration once a run.
//!
//! **Answering**, in this order:
//! 1. A git hook configured for the repository (`pre-receive`, `update`, `post-receive`) runs as git runs it, in a
//!    read-only view of the repository (A-665) shown at a path by the push's presenter (`hooks`' module docs):
//!    `pre-receive` once, its refusal answering every command `pre-receive hook declined`; `update` for each command
//!    the policy admits, its refusal answering that command `hook declined`; `post-receive` after the appends, only
//!    observing. Where nothing shows a view (the daemon's [`Unpresented`], until the view is served as a read-only
//!    attachment; or a platform with no hook driver), every command is refused `hook not run` with a line naming the
//!    hook, its quarantine given back: no ref moves that a configured hook was not asked about, and no hook is skipped
//!    without a word.
//! 2. The pushed objects were admitted to the repository's quarantine as the pack arrived (A-719 part 3, `intake`: the
//!    session reads the pack into it, so the push holds none of their bytes) and are sealed here (A-662's `GitPush`); a
//!    push whose every command the protocol's checks refused gives them back and seals none. A store that did not admit
//!    them (the shard's budget) refuses every command, `objects not admitted`, with why.
//! 3. Each command under the policy, through A-644 part 2's check point (`git_protect`): A-644's store answers how the
//!    change moves its ref and so the right it needs (Submit for a create or a fast-forward, Rewrite for a move that is
//!    no fast-forward, Delete for a delete), and A-622's `admit` judges that right and, where a rule imposes them, the
//!    requirements on what the change brings (a linear history, commits a trusted key signed). A refusal is in git's
//!    words (`git_protect::declined`): `non-fast-forward` and `deletion prohibited` as git refuses them itself, every
//!    other as git answers an update hook refusing it. A change whose ref names no born green is judged against a key
//!    no green has, so a create brings every commit its head reaches, as A-644 judges one. The policy is asked before
//!    the work is built, as git's `update()` refuses before it runs its hook: the cheaper refusal first, where
//!    §4.17.2's item 6 lists the work first.
//! 4. Each command's verdict, A-654 part 1's `Verdicts` over the shard's greens ([`ShardGreens`]), the repository's
//!    index and the quarantine, each work built within the room the shard can admit now.
//! 5. The append: each accepted command's entry made A-635's change and A-662's store move and appended through
//!    `push_sealed`, one entry each without `atomic`, all as one entry under it, each green first held to the position
//!    its verdict read (compare and swap, below). A refusal is git's `failed to update ref` (`atomic transaction
//!    failed` under `atomic`): a ref that moved for a conflict, a green moved, or a name taken or gone since the push
//!    read it; the append's own reason, in slates' words, for anything else.
//! 6. The report, in git's words (A-620's writer); then the sealed push is given back, holding only what no append
//!    committed.
//!
//! **A ref's base is its green's head.** A command's old value names a version only when it is the head of the newest
//! of the store's versions of its green and that version is the green's head now. A push that read an older version, or
//! a green that moved without git (a submit), is a ref that moved until A-664's server-made commits give every version
//! a commit, so that a work based on an older version can be composed and its result named. An annotated tag's tree is
//! that of the commit or tree it names.
//!
//! **A create** is made at the version of the member whose head its first parent is, where it can still be branched
//! from (the default branch's first, then the members as they joined), else at the default branch's newest version
//! (A-654 part 1), and appended as A-635's new branch made at that version, its work moving it to the pushed tree.
//! While the default branch is unborn, a create is built over an empty green and made from the default branch's green,
//! which must be empty at its version 0: the default branch itself is moved by the work, any other ref is a new branch
//! made at that empty version. A default branch that holds files no git commit names refuses it, typed: its commit is
//! A-664's.
//!
//! **A push's name** (A-618: two submits of one push carry one name, and every work's identity begins with it) is
//! hashed from its repository and its commands, so a push sent again makes the same works, and A-635 answers each the
//! version it made before.
//!
//! **Tests** (`git_push/tests.rs`): six pushes stock git 2.50.1 made, each advertisement and answer held to the bytes
//! git's server wrote and each ref's green to the tree its commit names; the policy hiding a ref and refusing a delete
//! and a submission in git's words through the check point, and a configured hook that nothing shows a view for
//! refusing typed, each refusal recording nothing; `atomic` all or none, at the verdict and at the append, and a push
//! without it a ref at a time; and (Unix) the hooks run through a presenter that writes each view into a scratch
//! directory: a refusing `pre-receive` answered in git's words with nothing recorded, accepting hooks landing the push
//! with their words on band 2, `post-receive` reading the moved ref in its view after the append, and a refusing
//! `update` refusing its own command alone. A protected name's linear history and signed commits are A-644 part 2's own
//! tests (`git_protect/tests.rs`).
//!
//! **Called by** the HTTP site's receive-pack (`vcs_http::git`'s `receive`, A-654 part 3) and the SSH site's
//! (`vcs_ssh`'s `receive`, A-683), each lending the owner shard's state a step at a time through one seam
//! (`vcs_http::git`'s `ShardLender`). **Owed** (GAPS' §4.17 entry): a hook's view served as a read-only attachment, the
//! daemon's presenter, with the edges' signal of a client gone that ends a hook's run and the hooks' learned times
//! (A-665 part 3's remainder); a push based on an older version (A-664); a principal the policy allows who is not the
//! repository's owner, whom A-635's append refuses `Forbidden` until its owner check gives way to the policy; push
//! options; review requests (`refs/for/`); the new versions' pack fragments encoded off the answer's path (item 8); a
//! tag of a tag.

use core::convert::Infallible;
use std::collections::BTreeMap;

use slates_db::catalog::{
  HookConfig, Principal, RepositoryId, RepositoryRecord, VcsTool, VolumeId as DbVolumeId,
};
use slates_git::Admission;
use slates_git::object_name::{Kind, ObjectId};
use slates_git::objects::tag::Tag;
use slates_git::out::Out;
use slates_git::receive::works::{self, Entry, GreenRead, Unappended, Verdicts, Version};
use slates_git::receive::{
  self, Advertised, Checked, Command, Deciding, Declined, HiddenRefs, Judge, Pushed, Received,
  Reported, Repository as _, Request, Session, write_report,
};
use slates_git::refname;
use slates_git::store::Incoming;
use slates_git::store::hook_view::Quantum;
use slates_git::store::learn::Clock;
use slates_git::store::views::{IndexedObjects, PushedObjects};
use slates_git::unpack::Object;
use slates_ipc::protocol::{DatabaseRefusal, Refusal};
use slates_merge::Increment;
use slates_vcs::ContentStore;
use slates_vcs::Work;

use crate::git_protect::{StoreBrings, declined, right_of};
use crate::repository::push::{
  AppendRefused, BranchChange, ChangeRefusal, ChangeRefused, EntryParts, StoreMove, push_sealed,
};
use crate::state::ShardState;
use crate::vcs_policy::{Asked, Brings, Compiled, Name, PushRefusal, Right, admit, admit_right};
use crate::vcs_store::{
  GitPush, GitRefused, GitStore, SealTurn, SealedPush, SealingPush, Target, green_key, with_room,
};
use crate::verbs::to_wire_volume;

#[cfg(unix)]
mod hooks;
mod intake;
#[cfg(test)]
pub(crate) mod tests;

#[cfg(unix)]
pub(crate) use hooks::{HookAnswer, HookCall};

/// Format: the agent receive-pack advertises (`agent=`), the name upload-pack gives this server too (A-640).
const AGENT: &[u8] = slates_git::upload::AGENT.as_bytes();

/// Format: the BLAKE3 key-derivation context a push's name is hashed under, so it is never another hash's.
const PUSH_NAME_CONTEXT: &str = "slates 2026-11-19 git push name (A-654)";

/// Format: a branch's ref is this and its namespace name (module docs).
pub(crate) const HEADS: &str = "refs/heads/";

/// Format: every other ref a namespace name can be begins with this (git's `check_refname_format` for a pushed ref).
const REFS: &str = "refs/";

/// Slates' words: the reason every command of a push is given while a git hook is configured for
/// its repository and nothing here shows a hook's view (A-665 part 3), and the reason of the
/// commands a hook could not judge, its view not opened, sealed or shown.
pub(crate) const HOOK_NOT_RUN: &[u8] = b"hook not run";

/// Slates' words: the reason every command of a push is given whose objects its repository's store did not admit (the
/// shard's budget, or an object the store refused).
pub(crate) const NOT_ADMITTED: &[u8] = b"objects not admitted";

/// Slates' words: why a create of a ref no namespace name holds is refused (a ref outside `refs/`, not UTF-8, or one
/// the mapping gives another name's place; module docs).
const NOT_A_NAME: &[u8] = b"ref name not held";

/// Slates' words: why a create made while the default branch is unborn is refused when that branch binds no green, or
/// one holding files no git commit names (A-664's to commit).
const NO_EMPTY_DEFAULT: &[u8] = b"the default branch holds files no commit names";

/// Why a push cannot go on, typed: the edge answers each in its transport's words. A command's own refusal is no
/// failure: the report answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PushFailed {
  /// No repository of this id on this shard's partition.
  NotFound,
  /// The repository serves another tool.
  NotGit,
  /// Its git store is not held here: none was made, or a recovery could not make it again (`vcs_store`).
  NoStore,
  /// Its policy holds a rule git's matcher cannot read (A-622 part 2): refused, never judged without the rule.
  Policy(PushRefusal),
  /// receive-pack refused the exchange: a request git's protocol does not allow, or one past the push's room.
  Exchange(receive::Refusal),
  /// `answer` asked of a push not yet read whole and checked, or already answered.
  Unchecked,
}

/// One git push over one connection, on its repository's owner shard (module docs), its hooks'
/// views shown by its presenter `P` (A-665 part 3).
pub(crate) struct ReceivePack<P = Unpresented> {
  /// The repository.
  repository: RepositoryId,
  /// Who pushes.
  principal: Principal,
  /// The push's exchange (A-620).
  session: Session<'static, PolicyHidden>,
  /// The push's room: its request and pack, and what reading and answering them builds.
  admission: Admission,
  /// The push counted live among its repository's intakes until it drops (A-724, `crate::vcs_share`).
  _live: crate::vcs_share::Live,
  /// The push counted decoding on its shard until it drops (A-724, `crate::vcs_share::place`): the load the placement
  /// of A-722's intake off the owner shard reads.
  _decoding: crate::vcs_share::place::Decoding,
  /// What shows a hook's view at a path a hook runs in.
  #[cfg(unix)]
  presenter: P,
  /// The presenter's type alone where no hook runs: nothing is shown.
  #[cfg(not(unix))]
  _presenter: core::marker::PhantomData<P>,
  /// Where its answer stands between the edge's steps (module docs, "A step at a time").
  answer: AnswerState,
  /// The quarantine its pack is read into as it arrives (A-719 part 3), until the answer seals it.
  quarantine: Option<GitPush>,
  /// The store's refusal of an object the pack brought, which answers every command.
  unadmitted: Option<GitRefused>,
}

/// The shard's clock as the store's and the checks' passes read it: the runtime's monotonic
/// nanoseconds (§4.3).
pub(crate) struct ShardClock;

impl Clock for ShardClock {
  fn now_ns(&mut self) -> u64 {
    slates_rt::futures::now_ns()
  }
}

/// What a step of a push's answer leaves its edge to do (module docs, "A step at a time").
#[derive(Debug)]
pub(crate) enum Answering {
  /// The push is answered whole: what was received (boxed: it is most of the step's size).
  Answered(Box<Received>),
  /// A turn of the push's work is done (its checks, A-719, or a hook's view's sealing): yield the
  /// shard, then step again.
  Turned,
  /// A hook is to run in its view: run it off the shard's step, then hand what it answered to
  /// `hook_ran`. Unix alone: no hook driver runs elsewhere, whose configured hooks answer `hook not run`.
  #[cfg(unix)]
  Hook(Box<HookCall>),
}

/// The daemon's presenter until a hook's view is served as a read-only attachment (A-665 part 3,
/// owed): it shows none, so a push whose repository configures a git hook is refused `hook not run`
/// (module docs, "Answering", item 1).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Unpresented;

#[cfg(unix)]
impl crate::git_hooks::PresentView for Unpresented {
  fn can_present(&self) -> bool {
    false
  }

  fn present(
    &mut self,
    _state: &ShardState,
    _view: &crate::git_hooks::GitHookView,
  ) -> Option<std::path::PathBuf> {
    None
  }

  fn withdraw(&mut self, _root: &std::path::Path) {}
}

/// What the answer's first step left.
enum Begun {
  /// The answer, written; boxed, as the hooks' state is, since a received push is far larger.
  Answering(Box<Answering>),
  /// The push's seal begun, a turn at a time.
  Sealing(Box<SealingAnswer>),
}

/// A push's answer while its objects are sealed and staged a turn at a time (A-722 part 4; `AnswerState::Sealing`), so
/// a push of git/git's size never runs past its step before its verdicts.
struct SealingAnswer {
  /// The repository's record as the answer read it.
  record: RepositoryRecord,
  /// The push's seal under way.
  sealing: SealingPush,
  /// The push's name (module docs).
  name: [u8; 32],
}

/// A push's answer while its commands are decided a step at a time (A-719; `AnswerState::Deciding`):
/// one command's verdict and append a step, so a push of a thousand refs never runs past its step.
struct DecidingPush {
  /// The repository's record as the answer read it.
  record: RepositoryRecord,
  /// The push's objects, sealed in quarantine.
  sealed: SealedPush,
  /// The push's name (module docs).
  name: [u8; 32],
  /// What the push's hooks before the verdicts decided of its commands.
  verdicts: HookVerdicts,
  /// The decision under way.
  deciding: Deciding<Entry>,
  /// The push's hooks, `post-receive` due once it is decided; none where no hook runs.
  #[cfg(unix)]
  then: Option<hooks::PushHooks>,
}

/// Where a push's answer stands between its steps.
enum AnswerState {
  /// Not begun.
  Unbegun,
  /// Its objects being sealed and staged a turn at a time.
  Sealing(Box<SealingAnswer>),
  /// Its objects sealed in quarantine, its hooks before the verdicts under way.
  #[cfg(unix)]
  Hooking(Box<hooks::Hooking>),
  /// Decided and reported but for the report's last flush, `post-receive` under way.
  #[cfg(unix)]
  Observing(Box<hooks::Observing>),
  /// Reported whole: the next step answers it.
  #[cfg(unix)]
  Reported(Box<Received>),
  /// Its commands decided a step at a time.
  Deciding(Box<DecidingPush>),
  /// Answered.
  Done,
}

impl<P> ReceivePack<P> {
  /// Starts a push of `principal`'s to `repository` (module docs), its request, its pack and what
  /// reading them builds admitted [`crate::vcs_share::room`], its hooks' views shown by `presenter`; writes the
  /// advertisement to `out` when `advertise`. Refused for a repository not here or not git's, a
  /// store not held, or a policy git's matcher cannot read.
  pub(crate) fn start(
    state: &mut ShardState,
    (principal, repository): (&Principal, RepositoryId),
    advertise: bool,
    out: &mut impl Out,
    presenter: P,
  ) -> Result<ReceivePack<P>, PushFailed> {
    let record = record_of(state, repository)?;
    if record.tool != VcsTool::Git {
      return Err(PushFailed::NotGit);
    }
    let format = state
      .vcs_stores
      .git_ref(repository)
      .map(|(_, git)| git.repository.format())
      .ok_or(PushFailed::NoStore)?;
    let hidden = PolicyHidden::of(&record, principal).map_err(PushFailed::Policy)?;
    let advertised = Advertised {
      format,
      atomic: true,
      ofs_delta: true,
      push_options: false,
      session_id: None,
      agent: AGENT,
    };
    let session = Session::new(&advertised, hidden);
    if advertise {
      let refs = with_room(state, |state| advertised_refs(state, repository));
      let listed: Vec<(ObjectId, &[u8])> = refs
        .iter()
        .map(|(id, name)| (*id, name.as_slice()))
        .collect();
      session
        .advertise(&listed, out)
        .map_err(PushFailed::Exchange)?;
    }
    // A-724 piece 1: the push is counted on its repository's row as it begins.
    state.vcs_usage.begun(
      &repository,
      principal,
      Work::Intake,
      slates_rt::futures::now_ns(),
    );
    Ok(ReceivePack {
      repository,
      principal: principal.clone(),
      session,
      admission: Admission::new(crate::vcs_share::room(state, repository).bytes),
      _live: crate::vcs_share::Live::begin(repository),
      _decoding: crate::vcs_share::place::Decoding::begin(state.shard),
      quarantine: GitPush::begin(&state.vcs_stores, repository),
      unadmitted: None,
      #[cfg(unix)]
      presenter,
      #[cfg(not(unix))]
      _presenter: {
        let _ = presenter;
        core::marker::PhantomData
      },
      answer: AnswerState::Unbegun,
    })
  }

  /// Hands the push the client's next `bytes`, its pack's objects admitted to the quarantine as
  /// they resolve (A-719 part 3): whether it is now read whole, its checks to make. A refusal of the
  /// exchange ends the push.
  pub(crate) fn input(&mut self, state: &mut ShardState, bytes: &[u8]) -> Result<bool, PushFailed> {
    let began = slates_rt::futures::now_ns();
    let read = self.intake(state, |session, site, admission| {
      session.input_into(bytes, site, admission)
    });
    let bytes_in = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    charge_step(state, self.repository, began, bytes_in);
    read
  }

  /// The client's bytes have ended: whether the push is now read whole. A request cut short ends the
  /// push; a pack cut short is checked as git answers a short pack.
  pub(crate) fn end(&mut self, state: &mut ShardState) -> Result<bool, PushFailed> {
    let began = slates_rt::futures::now_ns();
    let ended = self.intake(state, |session, site, admission| {
      session.end_into(site, admission)
    });
    charge_step(state, self.repository, began, 0);
    ended
  }

  /// Runs `call` of the session over the push's intake site, read only over the repository's index
  /// and the quarantine, then admits what it resolved to the quarantine (`intake`'s module docs). A
  /// store's refusal is kept to answer every command, and nothing more is admitted after it.
  fn intake(
    &mut self,
    state: &mut ShardState,
    call: impl FnOnce(
      &mut Session<'static, PolicyHidden>,
      &mut intake::Intake<'_>,
      &mut Admission,
    ) -> Result<bool, receive::Refusal>,
  ) -> Result<bool, PushFailed> {
    let Self {
      repository,
      session,
      admission,
      quarantine,
      ..
    } = self;
    let (called, queue) = with_room(state, |state| {
      let (content, git) = state
        .vcs_stores
        .git_ref(*repository)
        .ok_or(PushFailed::NoStore)?;
      let mut site = intake::Intake {
        indexed: IndexedObjects::new(&git.repository, content, admission.left()),
        quarantined: intake::Quarantined {
          push: quarantine.as_ref().map(GitPush::quarantine),
          store: content,
        },
        queue: BTreeMap::new(),
      };
      let called = call(session, &mut site, admission);
      Ok::<_, PushFailed>((called, site.queue))
    })?;
    match (quarantine.as_mut(), self.unadmitted.is_some()) {
      (Some(quarantine), false) => {
        if let Err(refused) = intake::admit_queued(state, quarantine, queue, admission) {
          self.unadmitted = Some(refused);
        }
      }
      _ => {
        for (_, hold) in queue.into_values() {
          admission.release(hold);
        }
      }
    }
    called.map_err(PushFailed::Exchange)
  }

  /// One turn of the pack's checks (A-719), for the shard's step quantum and at least one object,
  /// its reads lent the room the shard can admit now: whether the push is checked. A push of
  /// deletes, checked as its request ended, answers so at once. A refusal of the exchange ends the
  /// push.
  fn check_turn(&mut self, state: &mut ShardState) -> Result<bool, PushFailed> {
    if !self.session.is_checking() {
      return Ok(self.session.checked().is_some());
    }
    let ns = state.config.step_quantum_ns();
    let Self {
      repository,
      session,
      admission,
      quarantine,
      ..
    } = self;
    with_room(state, |state| {
      let (content, git) = state
        .vcs_stores
        .git_ref(*repository)
        .ok_or(PushFailed::NoStore)?;
      let indexed = IndexedObjects::new(&git.repository, content, admission.left());
      let quarantined = intake::Quarantined {
        push: quarantine.as_ref().map(GitPush::quarantine),
        store: content,
      };
      let mut clock = ShardClock;
      let quantum = Quantum {
        clock: &mut clock,
        ns,
      };
      session
        .check_turn_with((&indexed, &quarantined), admission, quantum)
        .map_err(PushFailed::Exchange)
    })
  }

  /// Gives back the quarantine the push's pack was read into, holding nothing of the shard after.
  fn give_back(&mut self, state: &mut ShardState) {
    if let Some(quarantine) = self.quarantine.take() {
      quarantine.abandon(state);
    }
  }

  /// The answer's first step (module docs, "Answering"): every command refused while a git hook is
  /// configured that cannot run here (`can_present` false); else the push's objects admitted and
  /// sealed, then its hooks before the verdicts due, or the verdicts at once where no git hook is
  /// configured.
  fn begin(
    &mut self,
    state: &mut ShardState,
    can_present: bool,
    out: &mut impl Out,
  ) -> Result<Begun, PushFailed> {
    let record = record_of(state, self.repository)?;
    let pack = self.session.take_pack();
    let checked = self.session.checked().ok_or(PushFailed::Unchecked)?;
    if let Some(hook) = git_hook(&record).filter(|_| !can_present) {
      self.give_back(state);
      let answering = self.declined(&mut Declining::hook(hook), out)?;
      return Ok(Begun::Answering(Box::new(answering)));
    }
    let quarantine = (self.quarantine.take(), self.unadmitted.take());
    match prepare(state, &record, checked, quarantine, pack)? {
      Prepared::Declined(mut declining) => Ok(Begun::Answering(Box::new(
        self.declined(&mut declining, out)?,
      ))),
      Prepared::Sealing { sealing, name } => Ok(Begun::Sealing(Box::new(SealingAnswer {
        record,
        sealing: *sealing,
        name,
      }))),
    }
  }

  /// One turn of the push's seal (A-722 part 4), for the shard's step quantum and a unit at least: another turn due, or,
  /// sealed, its hooks before the verdicts due where a git hook is configured and its verdicts otherwise; every
  /// command refused when the store refused the seal.
  fn seal_turn(
    &mut self,
    state: &mut ShardState,
    answer: SealingAnswer,
  ) -> Result<Sealed, PushFailed> {
    let SealingAnswer {
      record,
      sealing,
      name,
    } = answer;
    let ns = state.config.step_quantum_ns();
    let sealed = match sealing.turn(state, ns) {
      Ok(SealTurn::More(sealing)) => {
        self.answer = AnswerState::Sealing(Box::new(SealingAnswer {
          record,
          sealing: *sealing,
          name,
        }));
        return Ok(Sealed::Turned);
      }
      Ok(SealTurn::Sealed(sealed)) => sealed,
      Err(refused) => return Ok(Sealed::Declined(Declining::unadmitted(&refused))),
    };
    #[cfg(unix)]
    if git_hook(&record).is_some() {
      let checked = self.session.checked().ok_or(PushFailed::Unchecked)?;
      let hooking = hooks::Hooking::new(record, (sealed, name), checked);
      return Ok(Sealed::Hooking(Box::new(hooking)));
    }
    let deciding = self.deciding((record, sealed, name), HookVerdicts::default())?;
    Ok(Sealed::Deciding(Box::new(deciding)))
  }

  /// Every command refused by `declining`, and the report written: what was received.
  fn declined(
    &mut self,
    declining: &mut Declining,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    let Self {
      session, admission, ..
    } = self;
    let received = session
      .answer_with(declining, None, out, admission)
      .map_err(PushFailed::Exchange)?
      .ok_or(PushFailed::Unchecked)?;
    Ok(Answering::Answered(Box::new(received)))
  }

  /// The decision of the sealed push (module docs, "Answering", items 3 to 6), not yet begun, each
  /// command to be first answered as the hooks before the verdicts decided it (`verdicts`): the
  /// checked push taken from the session.
  fn deciding(
    &mut self,
    (record, sealed, name): (RepositoryRecord, SealedPush, [u8; 32]),
    verdicts: HookVerdicts,
  ) -> Result<DecidingPush, PushFailed> {
    let checked = self.session.take_checked().ok_or(PushFailed::Unchecked)?;
    let deciding = Deciding::new(checked, &mut self.admission).map_err(PushFailed::Exchange)?;
    Ok(DecidingPush {
      record,
      sealed,
      name,
      verdicts,
      deciding,
      #[cfg(unix)]
      then: None,
    })
  }

  /// One step of the decision (A-719): one command's verdict and append (under `atomic`, its
  /// verdict, then the appends of all). Undecided, the decision is kept for the next step; decided,
  /// the sealed push is given back, holding only what no append committed: what was received, and
  /// the hooks `post-receive` is due from.
  fn decision_turn(
    &mut self,
    state: &mut ShardState,
    mut decision: DecidingPush,
  ) -> Result<Option<(Received, Then)>, PushFailed> {
    let done = {
      let DecidingPush {
        record,
        sealed,
        name,
        verdicts,
        deciding,
        ..
      } = &mut decision;
      let mut judge = ServerJudge {
        state: &mut *state,
        push: sealed,
        record,
        principal: &self.principal,
        name: *name,
        hooks: verdicts,
      };
      deciding
        .step(None, &mut judge, &mut self.admission)
        .map_err(PushFailed::Exchange)?
    };
    if !done {
      self.answer = AnswerState::Deciding(Box::new(decision));
      return Ok(None);
    }
    decision.sealed.abandon(state);
    #[cfg(unix)]
    let then = decision.then;
    #[cfg(not(unix))]
    let then = ();
    Ok(Some((decision.deciding.received(), then)))
  }
}

/// What follows a push's decision: `post-receive`'s hooks where one runs.
#[cfg(unix)]
type Then = Option<hooks::PushHooks>;
/// What follows a push's decision: its report alone, where no hook runs.
#[cfg(not(unix))]
type Then = ();

#[cfg(unix)]
impl<P: crate::git_hooks::PresentView> ReceivePack<P> {
  /// One step of the checked push's answer (module docs, "A step at a time"), writing what git
  /// writes of it to `out`: answered, a turn of a hook's view sealed, or a hook to run. Refused for
  /// a push not yet checked or answered already, a repository gone, a store not held, or a report
  /// git's protocol cannot carry.
  pub(crate) fn answer_step(
    &mut self,
    state: &mut ShardState,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    let began = slates_rt::futures::now_ns();
    let stepped = self.answer_turn(state, out);
    charge_answer(state, self.repository, began, &stepped);
    stepped
  }

  /// One step of the answer, uncounted ([`Self::answer_step`] counts it).
  fn answer_turn(
    &mut self,
    state: &mut ShardState,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    match core::mem::replace(&mut self.answer, AnswerState::Done) {
      AnswerState::Unbegun if !self.check_turn(state)? => {
        self.answer = AnswerState::Unbegun;
        Ok(Answering::Turned)
      }
      AnswerState::Unbegun => {
        let can_present = self.presenter.can_present();
        match self.begin(state, can_present, out)? {
          Begun::Answering(answering) => Ok(*answering),
          Begun::Sealing(sealing) => self.sealing_step(state, *sealing, out),
        }
      }
      AnswerState::Sealing(sealing) => self.sealing_step(state, *sealing, out),
      AnswerState::Hooking(hooking) => self.hooking(state, *hooking, out),
      AnswerState::Deciding(decision) => self.decide_step(state, *decision, out),
      AnswerState::Observing(observing) => hooks::observe(self, state, *observing, out),
      AnswerState::Reported(received) => Ok(Answering::Answered(received)),
      AnswerState::Done => Err(PushFailed::Unchecked),
    }
  }

  /// One step of the seal ([`Self::seal_turn`]): another step due, every command refused, or the
  /// hooks before the verdicts or the verdicts begun in the same step.
  fn sealing_step(
    &mut self,
    state: &mut ShardState,
    sealing: SealingAnswer,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    match self.seal_turn(state, sealing)? {
      Sealed::Turned => Ok(Answering::Turned),
      Sealed::Declined(mut declining) => self.declined(&mut declining, out),
      Sealed::Hooking(hooking) => self.hooking(state, *hooking, out),
      Sealed::Deciding(decision) => self.decide_step(state, *decision, out),
    }
  }

  /// One step of the decision ([`Self::decision_turn`]): another step due, or git's report
  /// written, held for `post-receive` where its hook is due.
  fn decide_step(
    &mut self,
    state: &mut ShardState,
    decision: DecidingPush,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    match self.decision_turn(state, decision)? {
      None => Ok(Answering::Turned),
      Some((received, Some(hooks))) => hooks::reported(self, state, (received, hooks), out),
      Some((received, None)) => {
        write_report(&received, out).map_err(PushFailed::Exchange)?;
        Ok(Answering::Answered(Box::new(received)))
      }
    }
  }

  /// Hands the push what the hook its last step handed the edge answered, writing what git writes
  /// of it to `out`; the next step goes on. Refused for a push no hook of which runs.
  pub(crate) fn hook_ran(
    &mut self,
    state: &mut ShardState,
    answer: HookAnswer,
    out: &mut impl Out,
  ) -> Result<(), PushFailed> {
    hooks::ran(self, state, answer, out)
  }

  /// A step of the hooks before the verdicts, then the verdicts once they have answered.
  fn hooking(
    &mut self,
    state: &mut ShardState,
    hooking: hooks::Hooking,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    hooks::before(self, state, hooking, out)
  }
}

#[cfg(not(unix))]
impl<P> ReceivePack<P> {
  /// One step of the checked push's answer (module docs, "A step at a time"): with no hook driver
  /// here, the whole answer, every command refused `hook not run` while a git hook is configured.
  /// Refused for a push not yet checked or answered already, a repository gone, a store not held,
  /// or a report git's protocol cannot carry.
  pub(crate) fn answer_step(
    &mut self,
    state: &mut ShardState,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    let began = slates_rt::futures::now_ns();
    let stepped = self.answer_turn(state, out);
    charge_answer(state, self.repository, began, &stepped);
    stepped
  }

  /// One step of the answer, uncounted ([`Self::answer_step`] counts it).
  fn answer_turn(
    &mut self,
    state: &mut ShardState,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    match core::mem::replace(&mut self.answer, AnswerState::Done) {
      AnswerState::Unbegun if !self.check_turn(state)? => {
        self.answer = AnswerState::Unbegun;
        Ok(Answering::Turned)
      }
      AnswerState::Unbegun => match self.begin(state, false, out)? {
        Begun::Answering(answering) => Ok(*answering),
        Begun::Sealing(sealing) => self.sealing_step(state, *sealing, out),
      },
      AnswerState::Sealing(sealing) => self.sealing_step(state, *sealing, out),
      AnswerState::Deciding(decision) => self.decide_step(state, *decision, out),
      AnswerState::Done => Err(PushFailed::Unchecked),
    }
  }

  /// One step of the seal ([`Self::seal_turn`]): another step due, every command refused, or the
  /// verdicts begun in the same step.
  fn sealing_step(
    &mut self,
    state: &mut ShardState,
    sealing: SealingAnswer,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    match self.seal_turn(state, sealing)? {
      Sealed::Turned => Ok(Answering::Turned),
      Sealed::Declined(mut declining) => self.declined(&mut declining, out),
      Sealed::Deciding(decision) => self.decide_step(state, *decision, out),
    }
  }

  /// One step of the decision ([`Self::decision_turn`]): another step due, or git's report written.
  fn decide_step(
    &mut self,
    state: &mut ShardState,
    decision: DecidingPush,
    out: &mut impl Out,
  ) -> Result<Answering, PushFailed> {
    match self.decision_turn(state, decision)? {
      None => Ok(Answering::Turned),
      Some((received, ())) => {
        write_report(&received, out).map_err(PushFailed::Exchange)?;
        Ok(Answering::Answered(Box::new(received)))
      }
    }
  }
}

impl<P> Drop for ReceivePack<P> {
  /// A push dropped between its steps (its edge's task ended, its client gone) gives back what it
  /// holds of the shard: the quarantine its pack was read into, the sealed push and a hook's open
  /// view (module docs, "A step at a time"). A view's showing is its presenter's, which ends what it
  /// still shows when it is dropped.
  fn drop(&mut self) {
    if let Some(quarantine) = self.quarantine.take() {
      let given = crate::state::with_state(|state| quarantine.abandon(state));
      if given.is_none() {
        slates_machine::log_line!(
          "slates-server: a git push dropped mid-read could not give back its quarantine: the \
           shard's state was not lent to its drop"
        );
      }
    }
    let answer = core::mem::replace(&mut self.answer, AnswerState::Done);
    // A push dropped while its seal or its decision is under way gives back its sealing push or its sealed push
    // (A-722 part 4); one dropped mid-hooks gives back what its hooks hold.
    let held = match answer {
      AnswerState::Sealing(sealing) => Some(Held::Sealing(sealing.sealing)),
      AnswerState::Deciding(decision) => Some(Held::Sealed(decision.sealed)),
      #[cfg(unix)]
      other => {
        hooks::dropped(other);
        None
      }
      #[cfg(not(unix))]
      _ => None,
    };
    if let Some(held) = held {
      let given = crate::state::with_state(|state| match held {
        Held::Sealing(sealing) => sealing.abandon(state),
        Held::Sealed(sealed) => sealed.abandon(state),
      });
      if given.is_none() {
        slates_machine::log_line!(
          "slates-server: a git push dropped mid-seal or mid-decision could not give back its push: the \
           shard's state was not lent to its drop"
        );
      }
    }
  }
}

/// What a push dropped mid-answer still holds of the shard.
enum Held {
  /// Its seal under way.
  Sealing(SealingPush),
  /// Its sealed push, its commands being decided.
  Sealed(SealedPush),
}

/// `repository`'s record on this shard's partition.
fn record_of(state: &ShardState, repository: RepositoryId) -> Result<RepositoryRecord, PushFailed> {
  let repositories = state.db.partition().repositories();
  repositories
    .repository(repository)
    .cloned()
    .ok_or(PushFailed::NotFound)
}

/// The refs `repository` advertises (module docs): each branch whose green holds a git head, by its ref, in git's
/// order (by name).
fn advertised_refs(state: &ShardState, repository: RepositoryId) -> Vec<(ObjectId, Vec<u8>)> {
  let Some(store) = state.vcs_stores.git_ref(repository) else {
    return Vec::new();
  };
  let repositories = state.db.partition().repositories();
  let mut refs: Vec<(ObjectId, Vec<u8>)> = repositories
    .branches(repository, "")
    .filter_map(|(name, green)| {
      let name = ref_of(name)?;
      Some((git_head(store, green_key(green))?, name))
    })
    .collect();
  refs.sort_unstable_by(|left, right| left.1.cmp(&right.1));
  refs
}

/// The ref the namespace name `name` is (module docs): none for a name that is no git ref. A new repository's default
/// branch is held to it too (`repository::root_binding`, A-685), and upload-pack's lender names the refs it lends by
/// the same rule (`crate::vcs_store::upload`, A-682), so both services see one ref a name.
pub(crate) fn ref_of(name: &str) -> Option<Vec<u8>> {
  if name.starts_with(HEADS) {
    return None;
  }
  let full = if name.starts_with(REFS) {
    name.as_bytes().to_vec()
  } else {
    [HEADS.as_bytes(), name.as_bytes()].concat()
  };
  refname::is_valid(&full).then_some(full)
}

/// The namespace name the ref `name` is bound under (module docs): none for a ref no namespace name is.
fn namespace_of(name: &[u8]) -> Option<&str> {
  let name = core::str::from_utf8(name).ok()?;
  let namespace = name.strip_prefix(HEADS).unwrap_or(name);
  (ref_of(namespace)?.as_slice() == name.as_bytes()).then_some(namespace)
}

/// The head of the newest of the store's versions of the green keyed `key`: none while it is unborn, or deleted.
fn git_head((content, git): (&ContentStore, &GitStore), key: u64) -> Option<ObjectId> {
  let newest = git.repository.versions(key).checked_sub(1)?;
  git.repository.head(content, key, newest)
}

/// The green the ref `name` names in `repository`'s namespace (module docs).
fn bound(state: &ShardState, repository: RepositoryId, name: &[u8]) -> Option<DbVolumeId> {
  let namespace = namespace_of(name)?;
  state
    .db
    .partition()
    .repositories()
    .branch(repository, namespace)
}

/// The key of the green the ref `name` names in `repository`, where it is born: bound, and holding a git head in the
/// store.
fn born(
  state: &ShardState,
  (repository, name): (RepositoryId, &[u8]),
  store: (&ContentStore, &GitStore),
) -> Option<u64> {
  let key = green_key(bound(state, repository, name)?);
  git_head(store, key).map(|_| key)
}

/// A key no green of `repository` has in its git store, which a change is judged against where its ref names no born
/// green: the repository's own id's, minted by the counter its greens' ids are (`verbs::fresh_family_id_at`), so it is no
/// green's key. A create is so judged against no position of its own, as A-644 judges one: it brings every commit its
/// head reaches.
fn unheld_key(repository: RepositoryId) -> u64 {
  green_key(DbVolumeId {
    bytes: repository.bytes,
  })
}

/// The member of `repository` its git store keys `key` (`vcs_store::green_key`).
fn keyed(state: &ShardState, repository: RepositoryId, key: u64) -> Option<DbVolumeId> {
  let repositories = state.db.partition().repositories();
  repositories
    .members_of(repository)
    .map(|member| member.green)
    .find(|green| green_key(*green) == key)
}

/// The refs a pusher may not read under its repository's policy (module docs, "What is advertised"; A-622's Read).
/// upload-pack's lender hides the same refs from its reader by it (`crate::vcs_store::upload`, A-682), so a ref is
/// hidden alike from both services.
#[derive(Clone, Debug)]
pub(crate) struct PolicyHidden {
  /// The policy the push started under, compiled.
  policy: Compiled,
  /// The repository's record it was read from: its owner holds what no rule grants.
  record: RepositoryRecord,
  /// Who pushes.
  principal: Principal,
}

impl PolicyHidden {
  /// What `record`'s policy hides from `principal`; refused for a policy git's matcher cannot read.
  pub(crate) fn of(
    record: &RepositoryRecord,
    principal: &Principal,
  ) -> Result<PolicyHidden, PushRefusal> {
    Ok(PolicyHidden {
      policy: Compiled::of(record)?,
      record: record.clone(),
      principal: principal.clone(),
    })
  }
}

impl HiddenRefs for PolicyHidden {
  fn is_hidden(&self, name: &[u8]) -> bool {
    let asked = Asked {
      name: Name::Ref(name),
      right: Right::Read,
      brings: None,
    };
    admit_right(&self.policy, &self.record, &self.principal, &asked, 0).is_err()
  }
}

/// What a checked push is decided with (module docs, "Answering", items 1 and 2).
enum Prepared {
  /// Every command refused before any verdict.
  Declined(Declining),
  /// The push's objects' seal begun, a turn at a time (A-722 part 4).
  Sealing {
    /// The seal under way (boxed: it is most of a preparation's size).
    sealing: Box<SealingPush>,
    /// The push's name (module docs).
    name: [u8; 32],
  },
}

/// What a turn of a push's seal left (`ReceivePack::seal_turn`).
enum Sealed {
  /// Another turn is due.
  Turned,
  /// The store refused the seal: every command refused.
  Declined(Declining),
  /// Sealed, its hooks before the verdicts due.
  #[cfg(unix)]
  Hooking(Box<hooks::Hooking>),
  /// Sealed, its commands decided a step at a time.
  Deciding(Box<DecidingPush>),
}

/// Prepares `checked`, a push to `record`'s repository (module docs, "Answering", item 2), its
/// objects already admitted to `quarantine` as its pack arrived (A-719 part 3): sealed (emptied
/// first when the checks refused every command), or every command refused when the store did not
/// admit them (`unadmitted`).
fn prepare(
  state: &mut ShardState,
  record: &RepositoryRecord,
  checked: &Checked,
  (quarantine, unadmitted): (Option<GitPush>, Option<GitRefused>),
  pack: Vec<u8>,
) -> Result<Prepared, PushFailed> {
  let quarantine = match quarantine {
    Some(quarantine) => quarantine,
    None => GitPush::begin(&state.vcs_stores, record.id).ok_or(PushFailed::NoStore)?,
  };
  if let Some(refused) = unadmitted {
    quarantine.abandon(state);
    return Ok(Prepared::Declined(Declining::unadmitted(&refused)));
  }
  let judged = checked.early.iter().any(Option::is_none);
  let mut push = if judged {
    quarantine
  } else {
    quarantine.abandon(state);
    GitPush::begin(&state.vcs_stores, record.id).ok_or(PushFailed::NoStore)?
  };
  push.keep_pack(pack);
  match push.sealing(state) {
    Ok(sealing) => Ok(Prepared::Sealing {
      sealing: Box::new(sealing),
      name: push_name(record.id, &checked.request),
    }),
    Err(refused) => Ok(Prepared::Declined(Declining::unadmitted(&refused))),
  }
}

/// Counts a step of a push of `repository`'s that began at `began` on the shard's clock and read `bytes_in` of the
/// client's bytes on the repository's usage row (A-724 piece 1, `slates_vcs::usage`).
fn charge_step(state: &mut ShardState, repository: RepositoryId, began: u64, bytes_in: u64) {
  let now = slates_rt::futures::now_ns();
  state.vcs_usage.stepped(
    &repository,
    Work::Intake,
    (now.saturating_sub(began), bytes_in),
    now,
  );
}

/// Counts a step of a push's answer that began at `began` (A-724 piece 1), and the push as answered when the step
/// answered it whole.
fn charge_answer(
  state: &mut ShardState,
  repository: RepositoryId,
  began: u64,
  stepped: &Result<Answering, PushFailed>,
) {
  charge_step(state, repository, began, 0);
  if let Ok(Answering::Answered(_)) = stepped {
    state
      .vcs_usage
      .answered(&repository, Work::Intake, slates_rt::futures::now_ns());
  }
}

/// The first git hook `record`'s policy configures, if any (A-622 part 1: one hook a git point).
fn git_hook(record: &RepositoryRecord) -> Option<&HookConfig> {
  record
    .policy
    .hooks
    .iter()
    .find(|hook| hook.point.tool() == VcsTool::Git)
}

/// `object` as the store admits it (A-614's `Incoming`): claimed by the id the pack reader named it by, at the depth of
/// the delta chain the client's pack stored it at.
fn incoming_of(object: &Object) -> Incoming<'_> {
  Incoming {
    kind: object.kind,
    claimed: Some(object.id),
    content: &object.content,
    depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
  }
}

/// The push's name (module docs): its repository, then each command's old and new ids and its ref, the ref's length
/// first, hashed under the push's own context.
fn push_name(repository: RepositoryId, request: &Request) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(PUSH_NAME_CONTEXT);
  hasher.update(&repository.bytes);
  for command in &request.commands {
    let len = u64::try_from(command.name.len()).unwrap_or(u64::MAX);
    hasher.update(command.old.as_bytes());
    hasher.update(command.new.as_bytes());
    hasher.update(&len.to_le_bytes());
    hasher.update(&command.name);
  }
  *hasher.finalize().as_bytes()
}

/// A judge refusing every command it is asked about for one reason, before any verdict (module docs, "Answering",
/// items 1 and 2).
struct Declining {
  /// The reason, in slates' words.
  reason: &'static [u8],
  /// Why, the band-2 line after each command's ref.
  why: Vec<u8>,
}

impl Declining {
  /// Every command refused while `hook` is configured and nothing here shows its view (module docs,
  /// "Answering", item 1).
  fn hook(hook: &HookConfig) -> Declining {
    let why = format!(
      "the repository's {} hook cannot run here yet: its view of the push is not served",
      hook.name
    );
    Declining {
      reason: HOOK_NOT_RUN,
      why: why.into_bytes(),
    }
  }

  /// Every command refused because the store did not admit the push's objects, for `refused`.
  fn unadmitted(refused: &GitRefused) -> Declining {
    Declining {
      reason: NOT_ADMITTED,
      why: words(refused),
    }
  }
}

impl Judge for Declining {
  type Accepted = Infallible;

  fn judge(&mut self, command: &Command, _pushed: &Pushed<'_>) -> Result<Infallible, Declined> {
    let line = [
      command.name.as_slice(),
      b": ".as_slice(),
      self.why.as_slice(),
    ]
    .concat();
    Err(Declined {
      reason: self.reason.to_vec(),
      hook: Vec::new(),
      message: Some(receive::message(&line)),
    })
  }

  fn append(
    &mut self,
    _command: &Command,
    accepted: Infallible,
  ) -> Result<Vec<Reported>, Declined> {
    match accepted {}
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, Infallible)>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    match accepted.into_iter().next() {
      Some((_, accepted)) => match accepted {},
      None => Ok(Vec::new()),
    }
  }
}

/// What a push's hooks before the verdicts decided of its commands (module docs, "Answering", item
/// 1; A-665 part 3): a refusal every command takes, where `pre-receive` refused the push or could
/// not run, and each `update` hook's refusal of its own command, or its not running. Empty where no
/// hook ran.
#[derive(Debug, Default)]
struct HookVerdicts {
  /// The refusal every command takes.
  every: Option<Every>,
  /// Each `update` hook's refusal, by its command's ref.
  each: BTreeMap<Vec<u8>, Declined>,
}

/// The refusal every command of a push takes ([`HookVerdicts::every`]).
#[derive(Debug)]
// Made by the hooks alone, which run where a hook driver does (`hooks`, Unix).
#[cfg_attr(not(unix), allow(dead_code))]
struct Every {
  /// The report's reason, in git's words or slates'.
  reason: &'static [u8],
  /// The hook's words, relayed once, with the first command refused.
  words: Vec<u8>,
  /// Why, in slates' words, the band-2 line after each command's ref; none where git writes no line
  /// of its own.
  why: Option<Vec<u8>>,
}

impl HookVerdicts {
  /// The refusal of the command of ref `name`, where the hooks refused it.
  fn declined(&mut self, name: &[u8]) -> Option<Declined> {
    if let Some(every) = &mut self.every {
      return Some(Declined {
        reason: every.reason.to_vec(),
        hook: core::mem::take(&mut every.words),
        message: every
          .why
          .as_ref()
          .map(|why| receive::message(&[name, b": ".as_slice(), why].concat())),
      });
    }
    self.each.remove(name)
  }
}

/// One accepted command as A-635 appends it: its change, and A-662's store move.
type Planned = (BranchChange, StoreMove);

/// receive-pack's judge on the owner shard (module docs, "Answering", items 3 to 5).
struct ServerJudge<'j> {
  /// The shard's state: its greens, its records and the repository's store.
  state: &'j mut ShardState,
  /// The push's objects, sealed in quarantine.
  push: &'j mut SealedPush,
  /// The repository's record as the answer read it.
  record: &'j RepositoryRecord,
  /// Who pushes.
  principal: &'j Principal,
  /// The push's name.
  name: [u8; 32],
  /// What the push's hooks before the verdicts decided of its commands.
  hooks: &'j mut HookVerdicts,
}

impl Judge for ServerJudge<'_> {
  type Accepted = Entry;

  fn judge(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    if let Some(declined) = self.hooks.declined(&command.name) {
      return Err(declined);
    }
    self.admitted(command)?;
    self.verdict(command, pushed)
  }

  fn append(&mut self, command: &Command, entry: Entry) -> Result<Vec<Reported>, Declined> {
    let declined = |unappended: Unappended| works::append_declined(&command.name, &unappended);
    let planned = self.planned(command, entry, 0).map_err(declined)?;
    self.appended(vec![planned]).map_err(declined)?;
    Ok(Vec::new())
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, Entry)>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    let names: Vec<&[u8]> = accepted
      .iter()
      .map(|&(command, _)| command.name.as_slice())
      .collect();
    let declined = |unappended: Unappended| works::atomic_declined(&names, &unappended);
    let count = accepted.len();
    let mut planned = Vec::with_capacity(count);
    for (at, (command, entry)) in accepted.into_iter().enumerate() {
      planned.push(self.planned(command, entry, at).map_err(declined)?);
    }
    self.appended(planned).map_err(declined)?;
    Ok((0..count).map(|_| Vec::new()).collect())
  }
}

impl ServerJudge<'_> {
  /// The policy's verdict on `command` (module docs, "Answering", item 3): A-644's store answers how the change moves
  /// its ref and so the right it needs, and A-622's check point judges that right and, where a rule imposes one, the
  /// requirements on what the change brings (A-644 part 2's `StoreBrings`: a linear history, signed commits); a
  /// refusal in git's words (`git_protect::declined`).
  fn admitted(&mut self, command: &Command) -> Result<(), Declined> {
    let Self {
      state,
      push,
      record,
      principal,
      ..
    } = self;
    let name = command.name.as_slice();
    with_room(state, |state| {
      let Some(store) = state.vcs_stores.git_ref(record.id) else {
        return Err(refused_declined(name, words(&GitRefused::NoStore)));
      };
      let (content, git) = store;
      let green = born(state, (record.id, name), store).unwrap_or_else(|| unheld_key(record.id));
      let to = (!command.is_delete()).then_some(command.new);
      let change = git
        .repository
        .change(content, push.quarantine(), green, to)
        .map_err(|refused| refused_declined(name, words(&refused)))?;
      let right = right_of(change);
      let brings = to.map(|to| StoreBrings {
        repository: &git.repository,
        store: content,
        push: push.quarantine(),
        green,
        to,
      });
      let asked = [Asked {
        name: Name::Ref(name),
        right,
        brings: brings.as_ref().map(|brings| brings as &dyn Brings),
      }];
      admit(record, principal, &asked).map_err(|refusal| declined(&refusal, name, right))
    })
  }

  /// `command`'s entry (module docs, "Answering", item 4): A-654 part 1's verdicts over the shard's greens, the
  /// repository's index and the quarantine, each work built within the room the shard can admit now.
  fn verdict(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    let Self {
      state,
      push,
      record,
      name,
      ..
    } = self;
    with_room(state, |state| {
      let Some(store) = state.vcs_stores.git_ref(record.id) else {
        return Err(refused_declined(&command.name, words(&GitRefused::NoStore)));
      };
      let (content, git) = store;
      let room = state.store.admittable();
      let indexed = IndexedObjects::new(&git.repository, content, room);
      let objects = PushedObjects::new(&git.repository, content, push.quarantine());
      let greens = ShardGreens {
        state,
        record,
        store,
        indexed: &indexed,
      };
      Verdicts::new(&greens, &indexed, &objects, *name, room).judge(command, pushed)
    })
  }

  /// `entry`, the `at`th of those appended together, as A-635's change and A-662's store move (module docs,
  /// "Answering", item 5), its green still at the position its verdict read; or why not.
  fn planned(&self, command: &Command, entry: Entry, at: usize) -> Result<Planned, Unappended> {
    let state = &*self.state;
    let repository = self.record.id;
    match entry {
      Entry::Work {
        green,
        position,
        increment,
      } => {
        let green = at_position(state, repository, (green, position), at)?;
        let change = BranchChange::Update {
          green,
          work: increment,
        };
        Ok((change, (Target::Green(green), Some(command.new))))
      }
      Entry::Retire { green, position } => {
        let green = at_position(state, repository, (green, position), at)?;
        let repositories = state.db.partition().repositories();
        let (_, name) = repositories
          .binding_of(green)
          .ok_or(Unappended::Moved { at })?;
        let change = BranchChange::Delete {
          name: name.to_owned(),
          version: position,
        };
        Ok((change, (Target::Green(green), None)))
      }
      Entry::Create {
        name,
        from,
        increment,
      } => created(
        state,
        self.record,
        (&name, from, increment),
        (command.new, at),
      ),
    }
  }

  /// Appends `planned` as one entry through A-635's append with the push's objects (A-662's `push_sealed`); why not, at
  /// the change refused.
  fn appended(&mut self, planned: Vec<Planned>) -> Result<(), Unappended> {
    let (changes, moves): (Vec<BranchChange>, Vec<StoreMove>) = planned.into_iter().unzip();
    let greens: Vec<Option<DbVolumeId>> = moves
      .iter()
      .map(|(target, _)| match target {
        Target::Green(green) => Some(*green),
        Target::Created(_) => None,
      })
      .collect();
    let who = (self.principal, self.record.id);
    push_sealed(
      &mut *self.state,
      who,
      changes,
      EntryParts::default(),
      (&mut *self.push, &moves),
    )
    .map(|_| ())
    .map_err(|refused| unappended(&refused, &greens))
  }
}

/// The member keyed `key` while its log is still at `position`, the position its verdict read (compare and swap,
/// module docs): moved otherwise.
fn at_position(
  state: &ShardState,
  repository: RepositoryId,
  (key, position): (u64, u64),
  at: usize,
) -> Result<DbVolumeId, Unappended> {
  keyed(state, repository, key)
    .filter(|green| state.db.partition().green_head(*green) == position)
    .ok_or(Unappended::Moved { at })
}

/// A create's change and move (module docs, "A create"), the `at`th of those appended together: a new branch made at
/// the version its verdict chose; where it chose none, the default branch's empty green moved by the work for the
/// default branch itself, or a new branch made at that green's empty version 0 for any other ref.
fn created(
  state: &ShardState,
  record: &RepositoryRecord,
  (name, from, work): (&[u8], Option<(u64, u64)>, Increment),
  (new, at): (ObjectId, usize),
) -> Result<Planned, Unappended> {
  let namespace = namespace_of(name)
    .ok_or_else(|| refused_at(at, NOT_A_NAME))?
    .to_owned();
  let (from, version) = match from {
    Some((key, version)) => (
      keyed(state, record.id, key).ok_or(Unappended::Moved { at })?,
      version,
    ),
    None => {
      let empty = empty_default(state, record).ok_or_else(|| refused_at(at, NO_EMPTY_DEFAULT))?;
      let repositories = state.db.partition().repositories();
      if repositories.branch(record.id, &namespace) == Some(empty) {
        let change = BranchChange::Update { green: empty, work };
        return Ok((change, (Target::Green(empty), Some(new))));
      }
      (empty, 0)
    }
  };
  let change = BranchChange::Create {
    name: namespace.clone(),
    from,
    version,
    work: Some(work),
  };
  Ok((change, (Target::Created(namespace), Some(new))))
}

/// The default branch's green while it is unborn and empty: no git version in the store, and at its version 0 holding
/// no file (module docs, "A create").
fn empty_default(state: &ShardState, record: &RepositoryRecord) -> Option<DbVolumeId> {
  let name = record.default_branch.as_deref()?;
  let green = state
    .db
    .partition()
    .repositories()
    .branch(record.id, name)?;
  let (_, git) = state.vcs_stores.git_ref(record.id)?;
  let unborn = git.repository.versions(green_key(green)) == 0;
  let empty = state
    .greens
    .get(&green)
    .is_some_and(|engine| engine.head() == 0 && engine.files().next().is_none());
  (unborn && empty).then_some(green)
}

/// A-635's refusal as receive-pack's (module docs, "Answering", item 5), `greens` naming the green each change moves,
/// in order.
fn unappended(refused: &AppendRefused, greens: &[Option<DbVolumeId>]) -> Unappended {
  match refused {
    AppendRefused::Changes(changes) => changes
      .first()
      .map_or_else(|| refused_at(0, &words(refused)), change_unappended),
    AppendRefused::Entry(Refusal::Database {
      refusal: DatabaseRefusal::Moved { green, .. },
    }) => {
      let at = greens
        .iter()
        .position(|held| held.map(to_wire_volume).as_ref() == Some(green))
        .unwrap_or_default();
      Unappended::Moved { at }
    }
    AppendRefused::Entry(_) | AppendRefused::Repository(_) => refused_at(0, &words(refused)),
  }
}

/// One change A-635 refused, as receive-pack's: a conflict is another push first; a green moved, a name taken or a
/// green gone since the push read it is a ref that moved; anything else the append's own reason.
fn change_unappended(change: &ChangeRefused) -> Unappended {
  let at = change.index;
  match &change.why {
    ChangeRefusal::Conflict { windows } => Unappended::Conflict {
      at,
      windows: windows.clone(),
    },
    ChangeRefusal::Moved { .. }
    | ChangeRefusal::Refused(Refusal::AlreadyExists { .. } | Refusal::NotFound) => {
      Unappended::Moved { at }
    }
    why => refused_at(at, &words(why)),
  }
}

/// An append's own refusal of the `at`th entry, `why` in slates' words.
fn refused_at(at: usize, why: &[u8]) -> Unappended {
  Unappended::Refused {
    at,
    why: why.to_vec(),
  }
}

/// The refusal of `name`'s command for a reason of slates' own, `why`: git's `failed to update ref`, `why` on band 2.
fn refused_declined(name: &[u8], why: Vec<u8>) -> Declined {
  works::append_declined(name, &Unappended::Refused { at: 0, why })
}

/// Slates' words for a typed refusal: its kind and fields as its type names them, as Mercurial's push words its own
/// (`repository::hg_push`).
fn words(refusal: &impl core::fmt::Debug) -> Vec<u8> {
  format!("{refusal:?}").into_bytes()
}

/// The repository's greens as one push reads them on the owner shard (module docs): a ref's green by its namespace
/// name, born once the repository's git store holds a version of it, each lent for one call.
struct ShardGreens<'s, 'v> {
  /// The shard's state, read only.
  state: &'s ShardState,
  /// The repository's record.
  record: &'s RepositoryRecord,
  /// The repository's git store, with the content store it is in.
  store: (&'s ContentStore, &'s GitStore),
  /// The repository's index as the push reads it (A-638): where an annotated tag's bytes are read.
  indexed: &'v IndexedObjects<'s>,
}

impl works::Greens for ShardGreens<'_, '_> {
  fn read<T>(&self, name: &[u8], with: impl FnOnce(GreenRead<'_>) -> T) -> Option<T> {
    let green = bound(self.state, self.record.id, name)?;
    git_head(self.store, green_key(green))?;
    self.lent(green, with)
  }

  fn version_of(&self, green: u64, head: &ObjectId) -> Option<Version> {
    let member = keyed(self.state, self.record.id, green)?;
    let (version, newest) = self.newest(member)?;
    let current = self.state.db.partition().green_head(member);
    (newest == *head && version.number == current).then_some(version)
  }

  fn base_of_create<T>(
    &self,
    parent: Option<&ObjectId>,
    with: impl FnOnce(GreenRead<'_>, Version) -> T,
  ) -> Option<T> {
    let (green, version) = parent
      .and_then(|parent| self.holding(parent))
      .or_else(|| self.default_head())?;
    self.lent(green, |read| with(read, version))
  }
}

impl ShardGreens<'_, '_> {
  /// Runs `with` over `green` lent for one call as the push reads it: its key, its log's position and its engine.
  fn lent<T>(&self, green: DbVolumeId, with: impl FnOnce(GreenRead<'_>) -> T) -> Option<T> {
    let position = self.state.db.partition().green_head(green);
    let engine = self.state.greens.get(&green)?;
    Some(with(GreenRead {
      id: green_key(green),
      position,
      green: &engine,
    }))
  }

  /// The newest of the store's versions of `green`: its version and tree, and its head; none while it is unborn or
  /// deleted.
  fn newest(&self, green: DbVolumeId) -> Option<(Version, ObjectId)> {
    let (content, git) = self.store;
    let key = green_key(green);
    let at = git.repository.versions(key).checked_sub(1)?;
    let head = git.repository.head(content, key, at)?;
    let number = *git.versions(key).get(at)?;
    let tree = self.tree_of(&head)?;
    Some((Version { number, tree }, head))
  }

  /// The tree the object `head` names (module docs): a commit's, a tree itself, or that of the commit or tree an
  /// annotated tag names; none for anything else (a tag of a tag is owed).
  fn tree_of(&self, head: &ObjectId) -> Option<ObjectId> {
    let (content, git) = self.store;
    let mut target = *head;
    let mut row = git.repository.object(content, &target)?;
    if row.kind == Kind::Tag {
      let (_, bytes) = self.indexed.object(&target)?;
      let tag = Tag::parse(bytes, &mut Admission::new(u64::MAX)).ok()?;
      target = tag.object(head.format())?;
      row = git.repository.object(content, &target)?;
    }
    match row.kind {
      Kind::Commit => git.repository.id_at(content, row.commit?.tree),
      Kind::Tree => Some(target),
      _ => None,
    }
  }

  /// The green a create whose first parent is `parent` is made from, and its version (module docs, "A create"): the
  /// newest of the store's versions of a member whose head is `parent`, where a branch can still be made at it, the
  /// default branch's first, then the members as they joined.
  fn holding(&self, parent: &ObjectId) -> Option<(DbVolumeId, Version)> {
    let (content, git) = self.store;
    let repositories = self.state.db.partition().repositories();
    let members = repositories
      .members_of(self.record.id)
      .map(|member| member.green);
    self
      .default_green()
      .into_iter()
      .chain(members)
      .find_map(|green| {
        let (number, tree) = git.version_of(content, green_key(green), parent)?;
        self.branchable(green, Version { number, tree })
      })
  }

  /// The default branch's green.
  fn default_green(&self) -> Option<DbVolumeId> {
    let name = self.record.default_branch.as_deref()?;
    self
      .state
      .db
      .partition()
      .repositories()
      .branch(self.record.id, name)
  }

  /// `version` of `green`, where a branch can still be made at it (`repository::branch_version`).
  fn branchable(&self, green: DbVolumeId, version: Version) -> Option<(DbVolumeId, Version)> {
    crate::repository::branch_version(self.state, green, Some(version.number)).ok()?;
    Some((green, version))
  }

  /// The default branch's newest store version, where a branch can still be made at it.
  fn default_head(&self) -> Option<(DbVolumeId, Version)> {
    let green = self.default_green()?;
    let (version, _) = self.newest(green)?;
    self.branchable(green, version)
  }
}
