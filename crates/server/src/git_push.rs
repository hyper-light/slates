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
//! `Answering::Hook` for a hook the edge runs off the shard's step and hands back through `ReceivePack::hook_ran`
//! (`hooks`, Unix, where the hook driver runs). Each call is one step on the owner shard; between steps the driver holds only its push's sealed quarantine
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
//! (A-654 part 1), and appended as A-635's new branch made at that version, owing the push that moves it to the pushed
//! tree (P2, A-823: `BranchChange::CreateDeferred`, made when first reached through `crate::green_ready`). While the
//! default branch is unborn, a create's push is over an empty green, made from the default branch's green, which must be
//! empty at its version 0: the default branch itself owes the push as its next version (`BranchChange::UpdateDeferred`),
//! any other ref is a new branch made at that empty version. A default branch that holds files no git commit names
//! refuses it, typed: its commit is A-664's. A command reaches greens only through the door: one that owes its push is
//! made first, the command waiting a turn at a time.
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
use slates_git::store::hook_view::Quantum;
use slates_git::store::learn::Clock;
use slates_git::store::views::{IndexedObjects, PushedObjects};
use slates_ipc::protocol::{DatabaseRefusal, Refusal};
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
pub(crate) mod intake;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod words;

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

/// Slates' words: the reason every command of a push is given while a git hook is configured for its repository and
/// this daemon holds no standing grant to show the hook its view under (A-645 (a): `--vcs-store`).
pub(crate) const ROOT_UNPUBLISHED: &[u8] = b"no standing grant to show a hook its view";

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
  /// receive-pack refused the exchange: a request git's protocol does not allow, or one past the push's room while the
  /// room was all the shard could admit.
  Exchange(receive::Refusal),
  /// The push needed more than its room where its room was its repository's share of the shard, other repositories'
  /// intakes under way (A-724 piece 3b, §4.17.5 "Admission and backpressure"): retryable once theirs end, so answered as
  /// such, naming the share.
  PastShare {
    /// What the push asked and what its room had left.
    past: slates_git::PastAdmission,
    /// The repository's share of the contested resource, in bytes.
    share: u64,
    /// What the repository held of it already.
    held: u64,
  },
  /// The push needed more than its room where its room was its partition's snapshot slot's room left (A-724 piece 8a):
  /// retryable once the slot is laid out larger at the next planned handoff, naming the room.
  PastSnapshot {
    /// What the push asked and what its room had left.
    past: slates_git::PastAdmission,
    /// The slot's room left when the push began.
    room: u64,
  },
  /// `answer` asked of a push not yet read whole and checked, or already answered.
  Unchecked,
  /// The push's objects were not homed in the region and landed on the node's changeset store (a homing short, its pack
  /// not held, no store, the store busy, gone or its write failed; A-767 C5, part 2b): nothing of it is in the
  /// register, so the push is refused whole, for its client's retry.
  Unlanded,
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
  /// What bound the push's room when it began (`crate::vcs_share::Bound`): a refusal past it is typed by it.
  room_bound: crate::vcs_share::Bound,
  /// The client's bytes this push read, in all: what it brought, learned as its repository's demand once it is answered
  /// (A-724 piece 2, `slates_vcs::Rates`).
  read_bytes: u64,
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
  /// The commands' names, kept once the request is read for a client that asked a report but not side-band-64k, and
  /// charged to the push's room: what a refusal after the push's body answers each command with
  /// ([`ReceivePack::refused_report`]), since the request itself moves on through the answer. None under side-band,
  /// whose refusal is band 3's words alone.
  refusal_names: Option<Vec<Vec<u8>>>,
}

/// Counter: a member green checkpointed in a push's turn before its append, making the chains room for the append's
/// entry, a green a turn (A-238 moved off the append's step). Format: a counter name.
pub(crate) const ROOM_CHECKPOINTED: &str = "git_push.room_checkpointed";

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
  /// The admission of the command under way's move, a turn at a time (A-722 part 4c).
  admitting: Option<slates_git::store::Admitting>,
  /// The push's stage made again a turn at a time where a landing overtook it (A-722 part 4e).
  restaging: Option<slates_git::store::Staging>,
  /// The command under way's work, built a unit at a time over the turns before its verdict (A-752).
  preparing: Option<slates_git::receive::works::Preparing>,
  /// Where the let-go window the command under way's peel last stopped for lies (A-864's push readers, r18).
  peel_away: Option<u64>,
  /// The repository's member greens still to checkpoint, a green a turn, to make the chains room for the command under
  /// way's entry before its append (A-238 moved off the append's step); none while the chains have room.
  making_room: Option<Vec<DbVolumeId>>,
  /// The green owing its push that the command under way waits at the door for (P2, [`ServerJudge::door`]).
  door: Option<DbVolumeId>,
  /// The push's names homed in the region before its record names them (A-724 piece 8c, [`Homing`]).
  homing: Homing,
  /// The push's hooks, `post-receive` due once it is decided; none where no hook runs.
  #[cfg(unix)]
  then: Option<hooks::PushHooks>,
}

/// Where the homing of a push's names and pack, and their landing on the node's changeset store, stands (A-724 piece
/// 8c; A-722's record v4; A-767 C5; main's rulings of 2026-11-20: content durable at `f + 1` in the region before the
/// record that names it, and the changeset landed durable before the register's entry names its sequence). The
/// decision's first turn puts the names the record would carry as one content and asks the edge to home them
/// ([`ReceivePack::take_homing`]); the decision waits, a turn at a time, until the edge hands back the outcome
/// ([`ReceivePack::homed`]); then it asks the same of the pack its staging put in the store; both homed, the edge lands
/// them on the node's store as one changeset (`Names` and `Payload`, the bytes the homing gave back moved in), its
/// sequence taken now; landed, the record is written as version 5 naming the contents and the sequence. A push that
/// brings no object the repository lacks homes nothing and names none. Any shortfall (a homing short, a pack not held, no
/// store, a refused landing, an ask no edge took by the turn after) refuses the push ([`PushFailed::Unlanded`], C5 part
/// 2b), never a record naming its objects inline.
#[derive(Debug)]
enum Homing {
  /// Not yet asked.
  Due,
  /// Asked: the names' rows, for the edge to take, their identity and count.
  Asked(Vec<u8>, (slates_vcs::ContentId, u64)),
  /// Taken by the edge, its outcome awaited.
  Waiting((slates_vcs::ContentId, u64)),
  /// The names homed, their rows given back for the landing: the pack is asked next.
  PackDue(Vec<u8>),
  /// Asked: the pack's bytes, for the edge to take, its identity and length (held for the record), and the names' rows.
  PackAsked(Vec<u8>, (slates_vcs::ContentId, u64), Vec<u8>),
  /// The pack taken by the edge, its outcome awaited, with the names' rows.
  PackWaiting((slates_vcs::ContentId, u64), Vec<u8>),
  /// Both homed: the changeset to ask of the edge at the decision's next turn.
  LandDue(Box<Landing>),
  /// Asked: the changeset for the edge to land.
  LandAsked(Box<Landing>),
  /// The changeset taken by the edge, the store's answer awaited, at its sequence.
  LandWaiting(u64),
  /// The store refused the changeset: the push is refused.
  Unlanded,
  /// Done, or never asked: the decision goes on.
  Done,
}

/// A push's changeset as the edge lands it on the node's changeset store (A-767 C5): the store's handle, the header, and
/// the sections, their bytes moved in.
#[derive(Debug)]
pub(crate) struct Landing {
  /// The node's store.
  disk: slates_store::owner::StoreHandle,
  /// The changeset's header: the repository, its sequence.
  header: slates_store::log::Header,
  /// Its `Names` and its `Payload`.
  parts: Vec<slates_store::owner::Part>,
}

impl Landing {
  /// The sequence the changeset lands at: what the record names once it is landed.
  pub(crate) fn sequence(&self) -> u64 {
    self.header.sequence
  }
}

/// What a push asks its edge to carry off the shard's step ([`ReceivePack::take_homing`]).
#[derive(Debug)]
pub(crate) enum Carry {
  /// Content to home in the region (its names, then its pack), held there for the push's attempt (A-727 (d1a)).
  Home(Vec<u8>, slates_cluster::homes::HomeOwner),
  /// Its changeset to land on the node's store.
  Land(Box<Landing>),
}

/// What the edge hands back of a [`Carry`] ([`ReceivePack::homed`]).
#[derive(Debug)]
pub(crate) enum Carried {
  /// The homing's outcome, and its bytes given back.
  Homed(crate::vcs_store::homes::Homed),
  /// The store's answer.
  Landed(Result<slates_store::log::Located, slates_store::owner::OwnerError>),
}

/// Carries `carry` off the shard's step, asked from shard `origin` of the node's `shards`: content homed in the region
/// (`crate::vcs_store::homes::home_in_region`), or a changeset landed on the node's store, answered once durable. The one
/// way every edge (HTTP, SSH, the tests' driver) runs a push's homing.
pub(crate) async fn carry(origin: u16, shards: &[u16], carry: Carry) -> Carried {
  match carry {
    Carry::Home(bytes, owner) => Carried::Homed(
      crate::vcs_store::homes::home_in_region(
        origin,
        shards,
        (bytes, owner),
        crate::fleet::live_budget(),
      )
      .await,
    ),
    Carry::Land(landing) => {
      let Landing {
        disk,
        header,
        parts,
      } = *landing;
      Carried::Landed(disk.land(header, parts).await)
    }
  }
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
  /// The repository pushed to.
  pub(crate) fn repository(&self) -> RepositoryId {
    self.repository
  }

  /// Whether the client asked its answer multiplexed (`side-band-64k`), once its request is read.
  pub(crate) fn side_band(&self) -> bool {
    self.session.side_band()
  }

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
    let begun = slates_rt::futures::now_ns();
    state
      .vcs_usage
      .begun(&repository, principal, Work::Intake, begun);
    PHASE_LONGEST.with(|longest| {
      if let Ok(mut longest) = longest.try_borrow_mut() {
        longest.entry(repository).or_default().begun_at = Some(begun);
      }
    });
    let room = crate::vcs_share::room(state, repository);
    Ok(ReceivePack {
      repository,
      principal: principal.clone(),
      session,
      admission: Admission::new(room.bytes),
      _live: crate::vcs_share::Live::begin(repository),
      room_bound: room.bound,
      read_bytes: 0,
      _decoding: crate::vcs_share::place::Decoding::begin(state.shard),
      quarantine: GitPush::begin(&state.vcs_stores, repository),
      unadmitted: None,
      refusal_names: None,
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

  /// `failed` typed by what bound the push's room (A-724 piece 3b): a refusal past its admission becomes
  /// [`PushFailed::PastShare`] or [`PushFailed::PastSnapshot`] where its room was a share or the snapshot's room, and is
  /// left as it is where its room was all the shard could admit.
  fn bounded(&self, failed: PushFailed) -> PushFailed {
    let past = match failed {
      PushFailed::Exchange(
        receive::Refusal::PastAdmission(past)
        | receive::Refusal::PktLine(slates_git::pktline::Refusal::PastAdmission(past)),
      ) => past,
      other => return other,
    };
    match self.room_bound {
      crate::vcs_share::Bound::Share { share, held } => PushFailed::PastShare { past, share, held },
      crate::vcs_share::Bound::Snapshot { room } => PushFailed::PastSnapshot { past, room },
      crate::vcs_share::Bound::Shard => failed,
    }
  }

  /// Hands the push the client's next `bytes`, its pack's objects admitted to the quarantine as
  /// they resolve (A-719 part 3): whether it is now read whole, its checks to make. A refusal of the
  /// exchange ends the push.
  pub(crate) fn input(&mut self, state: &mut ShardState, bytes: &[u8]) -> Result<bool, PushFailed> {
    let began = slates_rt::futures::now_ns();
    let read = self.intake(state, bytes, |session, site, admission| {
      session.input_into(bytes, site, admission)
    });
    let bytes_in = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    self.read_bytes = self.read_bytes.saturating_add(bytes_in);
    charge_step(state, self.repository, began, bytes_in);
    let read = read.map_err(|failed| self.bounded(failed))?;
    self
      .keep_refusal_names()
      .map_err(|failed| self.bounded(failed))?;
    Ok(read)
  }

  /// Keeps the commands' names for a refusal's report (`refusal_names`) once the request is read, charged to the push's
  /// room, for a client that asked a report but not side-band-64k; nothing otherwise, or once kept.
  fn keep_refusal_names(&mut self) -> Result<(), PushFailed> {
    if self.refusal_names.is_some() {
      return Ok(());
    }
    let Some(request) = self.session.request() else {
      return Ok(());
    };
    let requested = &request.requested;
    if requested.side_band_64k || !(requested.report_status || requested.report_status_v2) {
      return Ok(());
    }
    let bytes = request.commands.iter().fold(0u64, |total, command| {
      total.saturating_add(u64::try_from(command.name.len()).unwrap_or(u64::MAX))
    });
    self
      .admission
      .charge(bytes)
      .map_err(|past| PushFailed::Exchange(receive::Refusal::PastAdmission(past)))?;
    self.refusal_names = Some(
      request
        .commands
        .iter()
        .map(|command| command.name.clone())
        .collect(),
    );
    Ok(())
  }

  /// The report of a push refused after its body was read, `why`, for a client that asked a report but not
  /// side-band-64k: `unpack <why>` and each command `ng` with it (`slates_git::receive::write_refused_report`); none
  /// for any other client, or before the request was read.
  pub(crate) fn refused_report(&self, why: &str) -> Option<Vec<u8>> {
    let names = self.refusal_names.as_ref()?;
    let mut report = Vec::new();
    receive::write_refused_report(names, why.as_bytes(), &mut report).ok()?;
    Some(report)
  }

  /// The client's bytes have ended: whether the push is now read whole. A request cut short ends the
  /// push; a pack cut short is checked as git answers a short pack.
  pub(crate) fn end(&mut self, state: &mut ShardState) -> Result<bool, PushFailed> {
    let began = slates_rt::futures::now_ns();
    let ended = self.intake(state, &[], |session, site, admission| {
      session.end_into(site, admission)
    });
    charge_step(state, self.repository, began, 0);
    let ended = ended.map_err(|failed| self.bounded(failed))?;
    self
      .keep_refusal_names()
      .map_err(|failed| self.bounded(failed))?;
    Ok(ended)
  }

  /// Runs `call` of the session over the push's intake site, read only over the repository's index
  /// and the quarantine, then keeps the pack `input` brought and admits what it resolved to the quarantine (`intake`'s
  /// module docs). A store's refusal is kept to answer every command, and nothing more is admitted after it.
  fn intake(
    &mut self,
    state: &mut ShardState,
    input: &[u8],
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
          repository: &git.repository,
        },
        queue: BTreeMap::new(),
      };
      let called = call(session, &mut site, admission);
      Ok::<_, PushFailed>((called, site.queue))
    })?;
    match (quarantine.as_mut(), self.unadmitted.is_some()) {
      (Some(quarantine), false) => {
        quarantine.keep_pack_to(session.pack_len(), input);
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
    // The pack's waiting deltas resolve a turn at a time first (A-724's starvation fix: resolved whole in the last input
    // step, git/git v1.5.0's held its shard 1.4 s, starving the small repositories there), what each turn resolves
    // admitted to the quarantine as an input's is.
    if self.session.is_resolving() {
      let ns = state.config.step_quantum_ns();
      self.intake(state, &[], |session, site, admission| {
        let mut clock = ShardClock;
        let quantum = Quantum {
          clock: &mut clock,
          ns,
        };
        session.resolve_turn_into(site, admission, quantum)
      })?;
      return Ok(false);
    }
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
        repository: &git.repository,
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
    let checked = self.session.checked().ok_or(PushFailed::Unchecked)?;
    if let Some(hook) = git_hook(&record).filter(|_| !can_present) {
      self.give_back(state);
      let answering = self.declined(&mut Declining::hook(hook, state.hook_views.shows()), out)?;
      return Ok(Begun::Answering(Box::new(answering)));
    }
    let quarantine = (self.quarantine.take(), self.unadmitted.take());
    match prepare(state, &record, checked, quarantine)? {
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
      Ok(SealTurn::Sealed(sealed)) => *sealed,
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
      admitting: None,
      restaging: None,
      preparing: None,
      peel_away: None,
      making_room: None,
      door: None,
      homing: Homing::Due,
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
    if !self.homing_turn(state, &mut decision)? {
      self.answer = AnswerState::Deciding(Box::new(decision));
      return Ok(None);
    }
    let done = {
      let DecidingPush {
        record,
        sealed,
        name,
        verdicts,
        deciding,
        admitting,
        restaging,
        preparing,
        peel_away,
        making_room,
        door,
        ..
      } = &mut decision;
      let mut judge = ServerJudge {
        state: &mut *state,
        push: sealed,
        record,
        principal: &self.principal,
        name: *name,
        hooks: verdicts,
        admitting,
        restaging,
        preparing,
        peel_away,
        making_room,
        door,
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

impl<P> ReceivePack<P> {
  /// The homing of the decision's names and pack and their landing ([`Homing`]): whether the decision may go on. Due,
  /// the names the record would carry are put as one content (held for the record) and asked of the edge; a push that
  /// brings no object, or whose names are not put, goes on as version 2. Refused where the store refused the landing.
  fn homing_turn(
    &mut self,
    state: &mut ShardState,
    decision: &mut DecidingPush,
  ) -> Result<bool, PushFailed> {
    match core::mem::replace(&mut decision.homing, Homing::Done) {
      Homing::Done => Ok(true),
      Homing::Due => {
        let names = {
          let lent = crate::vcs_store::Lent::new(&mut state.store, &mut state.vcs_stores);
          decision.sealed.names_now(lent.stores).ok()
        };
        // A push that brings no object the repository lacks homes and lands nothing: its records name none (C5 part
        // 2b). One that brings some must home and land them, or it is refused.
        let Some((bytes, count)) = names.filter(|(_, count)| *count > 0) else {
          return Ok(true);
        };
        let Some(identity) = crate::vcs_store::put_names(state, &bytes) else {
          return Err(PushFailed::Unlanded);
        };
        // The attempt its holds in the region are for until its record commits (A-727 (d1a)).
        let _ = decision.sealed.home_owner(state);
        decision.homing = Homing::Asked(bytes, (identity, count));
        Ok(false)
      }
      // Asked and not taken by the turn after: no edge carries it (a driver that never asks), so the content is given
      // back and the push refused (C5 part 2b), never a decision waiting for a homing nobody runs, nor a record naming
      // its objects inline.
      Homing::Asked(_, (identity, _)) | Homing::PackAsked(_, (identity, _), _) => {
        decision.sealed.release_unhomed(state, &identity);
        crate::vcs_store::homes::count_untaken(state);
        decision.sealed.release_homed(state);
        Err(PushFailed::Unlanded)
      }
      // The pack the staging put in the store, held once more for the record (A-722's record v4), and asked of the edge;
      // a push whose pack is not held is refused (C5 part 2b).
      Homing::PackDue(names) => match decision.sealed.pack_to_home() {
        Some((bytes, packed)) if crate::vcs_store::hold_for_record(state, &packed.0) => {
          decision.homing = Homing::PackAsked(bytes, packed, names);
          Ok(false)
        }
        _ => {
          decision.sealed.release_homed(state);
          Err(PushFailed::Unlanded)
        }
      },
      // The changeset asked of the edge, as the names and the pack are, in the turn before the edge takes it.
      Homing::LandDue(landing) => {
        decision.homing = Homing::LandAsked(landing);
        Ok(false)
      }
      // A changeset no edge took: no edge lands it, so the push is refused (C5 part 2b).
      Homing::LandAsked(_) => {
        crate::vcs_store::homes::count_untaken(state);
        decision.sealed.release_homed(state);
        Err(PushFailed::Unlanded)
      }
      Homing::Unlanded => {
        decision.sealed.release_homed(state);
        Err(PushFailed::Unlanded)
      }
      waiting @ (Homing::Waiting(_) | Homing::PackWaiting(..) | Homing::LandWaiting(_)) => {
        decision.homing = waiting;
        Ok(false)
      }
    }
  }

  /// What a decision asks the edge to carry off the shard's step ([`Homing`], [`Carry`]), once: the edge awaits
  /// [`carry`] and hands the outcome to [`Self::homed`].
  pub(crate) fn take_homing(&mut self) -> Option<Carry> {
    let AnswerState::Deciding(decision) = &mut self.answer else {
      return None;
    };
    match core::mem::replace(&mut decision.homing, Homing::Done) {
      Homing::Asked(bytes, named) => {
        let owner = decision.sealed.owner_now()?;
        decision.homing = Homing::Waiting(named);
        Some(Carry::Home(bytes, owner))
      }
      Homing::PackAsked(bytes, packed, names) => {
        let owner = decision.sealed.owner_now()?;
        decision.homing = Homing::PackWaiting(packed, names);
        Some(Carry::Home(bytes, owner))
      }
      Homing::LandAsked(landing) => {
        decision.homing = Homing::LandWaiting(landing.header.sequence);
        Some(Carry::Land(landing))
      }
      other => {
        decision.homing = other;
        None
      }
    }
  }

  /// What the edge carried of what [`Self::take_homing`] gave: the names homed under their identity, the pack is asked
  /// next; the pack homed too, the changeset is asked of the edge where the daemon holds a store, its sequence taken now,
  /// else the record is written as version 5 naming no changeset; landed, it names the sequence. A homing that fell
  /// short gives its content's hold back and the record is version 2; a refused landing refuses the push.
  pub(crate) fn homed(&mut self, state: &mut ShardState, carried: Carried) {
    let repository = self.repository;
    let AnswerState::Deciding(decision) = &mut self.answer else {
      return;
    };
    match (
      core::mem::replace(&mut decision.homing, Homing::Done),
      carried,
    ) {
      // A homing that fell short refuses the push (C5 part 2b: no record names objects inline).
      (Homing::Waiting((identity, count)), Carried::Homed(homed)) => {
        if homed.outcome.as_ref().is_ok_and(|homed| *homed == identity) {
          decision.sealed.homed_as((identity, count));
          decision.homing = Homing::PackDue(homed.bytes);
          return;
        }
        decision.sealed.release_unhomed(state, &identity);
        decision.homing = Homing::Unlanded;
      }
      (Homing::PackWaiting((identity, len), names), Carried::Homed(homed)) => {
        if !homed.outcome.as_ref().is_ok_and(|homed| *homed == identity) {
          decision.sealed.release_unhomed(state, &identity);
          decision.homing = Homing::Unlanded;
          return;
        }
        decision.sealed.pack_homed_as((identity, len));
        decision.homing = match landing_of(state, repository, (names, homed.bytes)) {
          Some(landing) => Homing::LandDue(Box::new(landing)),
          None => Homing::Unlanded,
        };
      }
      (Homing::LandWaiting(sequence), Carried::Landed(landed)) => {
        let ok = landed.is_ok();
        state.count(if ok { PUSH_LANDED } else { PUSH_UNLANDED }, 1);
        if ok {
          decision.sealed.landed_as(sequence);
        } else {
          decision.homing = Homing::Unlanded;
        }
      }
      (other, _) => decision.homing = other,
    }
  }
}

/// Counter: git pushes landed as one changeset on the node's changeset store before their entry (A-767 C5). Format: a
/// counter name.
pub(crate) const PUSH_LANDED: &str = "git_push.landed";
/// Counter: git pushes whose changeset the node's store refused: each refused whole, nothing of it in the register.
/// Format: a counter name.
pub(crate) const PUSH_UNLANDED: &str = "git_push.unlanded";

/// Repository `repository`'s push's changeset as the edge lands it (A-767 C5): `Names` the names' rows and `Payload` the
/// pack, both the bytes their homing gave back, moved in; its sequence the repository's next, taken now so two pushes
/// landing side by side never share one. None where the daemon holds no store, or the repository no store here. A git
/// commit the server makes lands the same way (`vcs_store::commits::make_deferred`, C5 2b piece 3).
pub(crate) fn landing_of(
  state: &mut ShardState,
  repository: RepositoryId,
  (names, pack): (Vec<u8>, Vec<u8>),
) -> Option<Landing> {
  let disk = state.vcs_disk.clone()?;
  let sequence = state.vcs_stores.next_changeset(repository)?;
  let header = slates_store::log::Header {
    tool: slates_store::log::Tool::Git,
    repository: u128::from_be_bytes(repository.bytes),
    sequence,
    judged_over: sequence.saturating_sub(1),
    grant: disk.grant(),
    work: 0,
    time_ns: slates_rt::futures::now_ns(),
    sections: 0,
  };
  Some(Landing {
    disk,
    header,
    parts: vec![
      (slates_store::log::SectionKind::Names, names),
      (slates_store::log::SectionKind::Payload, pack),
    ],
  })
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
    let stepped = self
      .answer_turn(state, out)
      .map_err(|failed| self.bounded(failed));
    charge_answer(state, (self.repository, self.read_bytes), began, &stepped);
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
    let stepped = self
      .answer_turn(state, out)
      .map_err(|failed| self.bounded(failed));
    charge_answer(state, (self.repository, self.read_bytes), began, &stepped);
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
      AnswerState::Sealing(sealing) => Some(Held::Sealing(Box::new(sealing.sealing))),
      AnswerState::Deciding(decision) => {
        let decision = *decision;
        Some(Held::Sealed(Box::new((
          decision.sealed,
          decision.restaging,
        ))))
      }
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
        Held::Sealed(held) => {
          let (sealed, restaging) = *held;
          if let Some(staging) = restaging {
            SealedPush::abandon_staging(state, staging);
          }
          sealed.abandon(state);
        }
      });
      if given.is_none() {
        slates_machine::log_line!(
          "slates-server: a git push dropped mid-seal or mid-decision could not give back its push: the \
           shard's state was not lent to its drop"
        );
      }
    }
    // Goal 93 item 2b: the landed packs its entries' commits owed a let-go, let go now that it decides no more refs
    // (where nothing else of the repository is under way; else what ends last, or its going idle). A drop the state is
    // not lent to leaves them owed to the repository's going idle.
    let repository = self.repository;
    let _ = crate::state::with_state(|state| {
      crate::vcs_store::let_go_owed_as_push_ends(state, repository);
    });
  }
}

/// What a push dropped mid-answer still holds of the shard.
enum Held {
  /// Its seal under way (boxed, as each of them: the two differ by hundreds of bytes).
  Sealing(Box<SealingPush>),
  /// Its sealed push, its commands being decided, and its stage being made again (A-722 part 4e).
  Sealed(Box<(SealedPush, Option<slates_git::store::Staging>)>),
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
    .member_keyed(repository, key)
    .map(|(green, _)| green)
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
  let push = if judged {
    quarantine
  } else {
    quarantine.abandon(state);
    GitPush::begin(&state.vcs_stores, record.id).ok_or(PushFailed::NoStore)?
  };
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
  charge_phase(
    state,
    repository,
    began,
    bytes_in,
    if bytes_in > 0 { PHASE_INPUT } else { PHASE_END },
  );
}

/// Format: a push step's phase in [`PhaseLongest`]: a run of the body read.
const PHASE_INPUT: usize = 0;
/// Format: the step that ends the body.
const PHASE_END: usize = 1;
/// Format: a turn of the pack's checks (A-719), an answer step that answers `Turned`.
const PHASE_CHECK: usize = 2;
/// Format: the answer's last step (seal, hooks' views, verdicts, commit and report), one that answers `Answered`.
const PHASE_ANSWER: usize = 3;
/// Derived: the phases a push's steps are counted in, the last one's index and one.
const PHASES: usize = PHASE_ANSWER + 1;

/// Each phase's steps of a git push on this shard, per repository (A-724's profile: where a push holds its shard, and
/// how long it waits between its steps): indexed by phase [input, end, check turn, answer].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PhaseSteps {
  /// The longest step of each phase, nanoseconds.
  pub(crate) longest: [u64; PHASES],
  /// Steps of each phase.
  pub(crate) steps: [u64; PHASES],
  /// The nanoseconds those steps ran.
  pub(crate) ns: [u64; PHASES],
  /// The nanoseconds from the repository's previous push step's end to each step's start, summed by the step's phase:
  /// before a check turn or an answer step, the time the push waited for its shard (it was ready); before an input step,
  /// that and the client's bytes.
  pub(crate) gap_ns: [u64; PHASES],
  /// The nanoseconds from each push's beginning (its session made, `vcs_usage`'s `begun`) to its first step.
  pub(crate) first_gap_ns: u64,
  /// The shard clock at the repository's latest push step's end; zero before its first.
  last_end: u64,
  /// The shard clock at a push's beginning while its first step is still to come.
  begun_at: Option<u64>,
}

/// [`PhaseSteps`] by repository on this shard: one entry a repository with a push stepped here, so bounded by the
/// partition's repositories.
pub(crate) type PhaseLongest = std::collections::BTreeMap<RepositoryId, PhaseSteps>;

std::thread_local! {
  /// This shard's [`PhaseLongest`].
  static PHASE_LONGEST: std::cell::RefCell<PhaseLongest> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// `repository`'s push steps by phase on this shard ([`PhaseSteps`]): zeros where none ran.
pub(crate) fn phase_longest(repository: RepositoryId) -> PhaseSteps {
  PHASE_LONGEST.with(|longest| {
    longest
      .try_borrow()
      .ok()
      .and_then(|longest| longest.get(&repository).copied())
      .unwrap_or_default()
  })
}

/// Counts a step of phase `phase` ([`PhaseSteps`]) on the repository's usage row and its phase's steps.
fn charge_phase(
  state: &mut ShardState,
  repository: RepositoryId,
  began: u64,
  bytes_in: u64,
  phase: usize,
) {
  let now = slates_rt::futures::now_ns();
  let step_ns = now.saturating_sub(began);
  state
    .vcs_usage
    .stepped(&repository, Work::Intake, (step_ns, bytes_in), now);
  PHASE_LONGEST.with(|longest| {
    if let Ok(mut longest) = longest.try_borrow_mut() {
      let entry = longest.entry(repository).or_default();
      let gap = match entry.begun_at.take() {
        Some(begun) => {
          entry.first_gap_ns = entry
            .first_gap_ns
            .saturating_add(began.saturating_sub(begun));
          0
        }
        None if entry.last_end == 0 => 0,
        None => began.saturating_sub(entry.last_end),
      };
      entry.last_end = now;
      if let (Some(most), Some(steps), Some(ns), Some(gaps)) = (
        entry.longest.get_mut(phase),
        entry.steps.get_mut(phase),
        entry.ns.get_mut(phase),
        entry.gap_ns.get_mut(phase),
      ) {
        *most = (*most).max(step_ns);
        *steps = steps.saturating_add(1);
        *ns = ns.saturating_add(step_ns);
        *gaps = gaps.saturating_add(gap);
      }
    }
  });
}

/// Counts a step of a push's answer that began at `began` (A-724 piece 1), and the push as answered when the step
/// answered it whole, its `read_bytes` learned as the repository's demand (A-724 piece 2).
fn charge_answer(
  state: &mut ShardState,
  (repository, read_bytes): (RepositoryId, u64),
  began: u64,
  stepped: &Result<Answering, PushFailed>,
) {
  let phase = if matches!(stepped, Ok(Answering::Answered(_))) {
    PHASE_ANSWER
  } else {
    PHASE_CHECK
  };
  charge_phase(state, repository, began, 0, phase);
  if let Ok(Answering::Answered(_)) = stepped {
    state.vcs_usage.answered(
      &repository,
      Work::Intake,
      read_bytes,
      slates_rt::futures::now_ns(),
    );
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
  /// "Answering", item 1): before this daemon has published its root mount (`root_published` false) in words naming
  /// that, since a hook is shown its view beneath it (A-645 P3, main's ruling 1); after, `hook not run`.
  fn hook(hook: &HookConfig, root_published: bool) -> Declining {
    if !root_published {
      let why = format!(
        "the repository's {} hook cannot run here: this daemon holds no standing grant to show it its view (--vcs-store)",
        hook.name
      );
      return Declining {
        reason: ROOT_UNPUBLISHED,
        why: why.into_bytes(),
      };
    }
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

/// The turns pushes' work builds took on this shard (A-752; status `push.build.*`): each turn of a command's build
/// before its verdict, timed on the shard's clock.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PushTurns {
  /// The build turns taken.
  pub(crate) turns: u64,
  /// The turns that ran past their budget by more than their build's longest unit and the scheduler's own overrun: the
  /// pacing's failure, zero in every correct use.
  pub(crate) past_budget: u64,
  /// The longest turn, nanoseconds.
  pub(crate) longest_ns: u64,
  /// The longest unit of any build, nanoseconds.
  pub(crate) longest_unit_ns: u64,
  /// The longest opening of a turn, nanoseconds: what it did before its first unit (the admission's turn, the views).
  pub(crate) longest_opening_ns: u64,
  /// The quanta turns took past their first because no one waited (A-783): the non-vacuity count of the extension.
  pub(crate) more_quanta: u64,
  /// The units the turns ran (A-752: a build works a unit at a time, at least one a file it changes, whatever the
  /// quantum the shard keeps).
  pub(crate) units: u64,
  /// The turns that yielded to someone waiting at a quantum's end (A-783).
  pub(crate) yielded: u64,
}

/// The shard's pacing environment (A-783): its clock, its probe of whether anyone else waits, its step quantum, its
/// ceiling (the watchdog's look) and a unit estimate's target error (the scheduler's own measured jitter over the
/// quantum). Every paced path on a shard steps under it: the push build, a checkpoint's identity fold (A-579).
pub(crate) fn shard_pace_env(state: &ShardState) -> slates_pace::Env {
  let quantum = state.config.step_quantum_ns();
  slates_pace::Env {
    clock: slates_rt::futures::now_ns,
    waiting: slates_rt::futures::others_waiting,
    quantum_ns: quantum,
    ceiling_ns: slates_rt::futures::step_ceiling_ns().unwrap_or(quantum),
    target_ppm: target_ppm(slates_rt::futures::scheduler_overrun_ns(), quantum),
  }
}

/// A unit estimate's target relative error, parts per million (A-783, `slates_pace::Env::target_ppm`): the scheduler's
/// own measured jitter over the quantum, at least one part, at most the whole.
pub(crate) fn target_ppm(jitter_ns: u64, quantum_ns: u64) -> u64 {
  // Format: parts per million.
  const MILLION: u64 = 1_000_000;
  u64::try_from(
    u128::from(jitter_ns)
      .saturating_mul(u128::from(MILLION))
      .checked_div(u128::from(quantum_ns.max(1)))
      .unwrap_or(0),
  )
  .unwrap_or(u64::MAX)
  .clamp(1, MILLION)
}

impl PushTurns {
  /// A turn that took `took` nanoseconds against `budget`: its opening, its build's longest unit so far and its tail.
  fn counted(&mut self, took: u64, (budget, (opening, unit, tail)): (u64, (u64, u64, u64))) {
    self.turns = self.turns.saturating_add(1);
    self.longest_ns = self.longest_ns.max(took);
    self.longest_unit_ns = self.longest_unit_ns.max(unit);
    self.longest_opening_ns = self.longest_opening_ns.max(opening);
    let unit = opening.saturating_add(unit).saturating_add(tail);
    let noise =
      slates_rt::registry::with_current(|context| context.counters().scheduler_overrun_ns)
        .unwrap_or(0);
    if took.saturating_sub(budget) > unit.saturating_add(noise) {
      self.past_budget = self.past_budget.saturating_add(1);
    }
  }

  /// What a turn's step did (A-783): the quanta it took past its first, and whether it yielded to someone waiting.
  fn stepped(&mut self, report: Option<slates_pace::Report>) {
    let Some(report) = report else {
      return;
    };
    self.more_quanta = self
      .more_quanta
      .saturating_add(report.quanta.saturating_sub(1));
    self.units = self.units.saturating_add(report.units);
    if report.stop == Some(slates_pace::Stop::Waiting) {
      self.yielded = self.yielded.saturating_add(1);
    }
  }

  /// Its status rows.
  pub(crate) fn rows(&self) -> [(&'static str, u64); 8] {
    [
      ("push.build.units", self.units),
      ("push.build.more_quanta", self.more_quanta),
      ("push.build.yielded", self.yielded),
      ("push.build.turns", self.turns),
      ("push.build.past_budget", self.past_budget),
      ("push.build.longest_ns", self.longest_ns),
      ("push.build.longest_unit_ns", self.longest_unit_ns),
      ("push.build.longest_opening_ns", self.longest_opening_ns),
    ]
  }
}

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
  /// The admission of the command under way's move, a turn at a time (A-722 part 4c).
  admitting: &'j mut Option<slates_git::store::Admitting>,
  /// The push's stage made again a turn at a time where a landing overtook it (A-722 part 4e).
  restaging: &'j mut Option<slates_git::store::Staging>,
  /// The command under way's work, built a unit at a time before its verdict (A-752).
  preparing: &'j mut Option<slates_git::receive::works::Preparing>,
  /// Where the let-go window the command under way's peel last stopped for lies (A-864's push readers, r18).
  peel_away: &'j mut Option<u64>,
  /// The member greens still to checkpoint before the command under way's append ([`DecidingPush::making_room`]).
  making_room: &'j mut Option<Vec<DbVolumeId>>,
  /// The green owing its push that the command under way waits at the door for (P2).
  door: &'j mut Option<DbVolumeId>,
}

impl Judge for ServerJudge<'_> {
  type Accepted = Entry;

  /// A push without `atomic` decides its commands oldest commit first (A-850): each create's search back for its base
  /// then meets the move below it, so git/git's tags pushed whole into an empty repository bring about its history and
  /// its tags, not tags times history. A delete ranks first; its outcome, as every command's, is reported in the order
  /// sent.
  fn rank(&mut self, command: &Command, _pushed: &Pushed<'_>) -> u64 {
    if command.is_delete() {
      return 0;
    }
    self.push.rank(self.state, command.new)
  }

  /// A turn of the admission of `command`'s move (A-722 part 4c), for the shard's step quantum and a unit at least: the
  /// walk of what it brings, which its append then takes whole ([`SealedPush::admit`]). A delete needs none.
  fn ready(&mut self, command: &Command, pushed: &Pushed<'_>) -> bool {
    let ns = self.state.config.step_quantum_ns();
    // A stage another push's landing overtook is made again first, a turn at a time, so the append's commit adopts it
    // rather than staging the push again whole in its step (A-722 part 4e).
    if !self.push.restage_turn(self.state, (self.restaging, ns)) {
      return false;
    }
    if command.is_delete() {
      // A delete builds nothing; it reads only its own green, made first if it owes its push (P2's door).
      return match bound(self.state, self.record.id, &command.name)
        .filter(|green| self.state.greens.is_owing(green))
      {
        Some(green) => self.door(green),
        None => {
          *self.door = None;
          true
        }
      };
    }
    let repository = self.record.id;
    let green = {
      let state = &*self.state;
      let Some(store) = state.vcs_stores.git_ref(repository) else {
        return true;
      };
      born(state, (repository, &command.name), store).unwrap_or_else(|| unheld_key(repository))
    };
    let began = slates_rt::futures::now_ns();
    self
      .push
      .admission_turn(self.state, (green, command.new), (self.admitting, ns))
      && self.build_turn(command, pushed, began)
      && self.room_turn()
  }

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
  /// A turn of making the chains room for the command under way's entry before its append (A-238, moved off the
  /// append's step: git/git's whole-history push had an append meet `green_chains` at its cap and checkpoint the
  /// repository's every member green in its one step, about a thousand folds, 1,142 ms, and the watchdog killed the
  /// daemon): where the chains have less room than its built work's entry can take (`entry_bound`, as a Subversion
  /// commit sizes its own, `svn_commit`), one member green checkpointed, counted [`ROOM_CHECKPOINTED`], and another turn
  /// due; ready once there is room, or once every member is checkpointed (the append's guard then judges the entry,
  /// `repository::push`'s A-238, which finds nothing left to fold).
  fn room_turn(&mut self) -> bool {
    let Some(bound) = self
      .preparing
      .as_ref()
      .and_then(slates_git::receive::works::Preparing::entry_bound)
    else {
      *self.making_room = None;
      return true;
    };
    if self.state.db.partition().green_chain_room() >= bound {
      *self.making_room = None;
      return true;
    }
    let repository = self.record.id;
    let members = self.making_room.get_or_insert_with(|| {
      self
        .state
        .db
        .partition()
        .repositories()
        .members_of(repository)
        .map(|member| member.green)
        .collect()
    });
    let Some(green) = members.pop() else {
      *self.making_room = None;
      return true;
    };
    self.state.count(ROOM_CHECKPOINTED, 1);
    crate::merge_service::checkpoint_green_when(
      self.state,
      green,
      crate::merge_service::Doubling::Now,
    );
    false
  }

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
      preparing,
      ..
    } = self;
    let (entry, examined) = with_room(state, |state| {
      let Some(store) = state.vcs_stores.git_ref(record.id) else {
        return (
          Err(refused_declined(&command.name, words(&GitRefused::NoStore))),
          0,
        );
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
        owing: std::cell::Cell::new(None),
        examined: std::cell::Cell::new(0),
      };
      let entry = Verdicts::new(&greens, &indexed, &objects, *name, room)
        .prepared(preparing.take())
        .judge(command, pushed);
      (entry, greens.examined.get())
    });
    state.count(HOLDING_EXAMINED, examined);
    entry
  }

  /// P2's door for the command under way, at `green`, which owes its push: whether the command may go on to its verdict.
  /// The first time, the green's materialization starts (`crate::green_ready`) and the command waits a turn at a time
  /// while it runs. Once a build it waited for ended without making the green (refused past its room, counted there),
  /// the command goes on, and its verdict, which lends no owing green, declines it in git's words: never a wait that
  /// starts the build again every turn.
  fn door(&mut self, green: DbVolumeId) -> bool {
    if *self.door == Some(green) && !crate::green_ready::is_building(self.state, green) {
      *self.door = None;
      return true;
    }
    *self.door = Some(green);
    crate::green_ready::green_ready(self.state, green);
    false
  }

  /// A turn of `command`'s work build (A-752), begun at `began` on the shard's clock: units until half the step quantum
  /// is spent, the build kept for the next turn. Whether it is built (or needs none), so this step judges it. The turn
  /// is counted ([`PushTurns`]): one that ran past its budget by more than its longest unit and the scheduler's own
  /// overrun is past budget.
  fn build_turn(&mut self, command: &Command, pushed: &Pushed<'_>, began: u64) -> bool {
    let Self {
      state,
      push,
      record,
      name,
      preparing,
      peel_away,
      ..
    } = self;
    let quantum = state.config.step_quantum_ns();
    // A-783: the turn yields at its quantum when someone waits on the shard, and goes on to the shard's ceiling (the
    // watchdog's look) when no one does; a unit's estimate aims at the scheduler's own measured jitter over the
    // quantum, its target relative error.
    let env = shard_pace_env(state);
    // A unit's bytes: the shard's copy and hash slices, derived from its measured memcpy and BLAKE3 rates (R3).
    let slices = slates_merge::pace::Slices {
      copy_bytes: state.config.copy_slice_bytes_within(quantum).max(1),
      hash_bytes: state.config.archive_slice_bytes_within(quantum).max(1),
    };
    let (built, owing, examined) = with_room(state, |state| {
      let Some(store) = state.vcs_stores.git_ref(record.id) else {
        return Ok::<_, Declined>((true, None, 0));
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
        owing: std::cell::Cell::new(None),
        examined: std::cell::Cell::new(0),
      };
      let verdicts = Verdicts::new(&greens, &indexed, &objects, *name, room).peeled(**peel_away);
      let built = verdicts.prepare(command, pushed, preparing, (began, env), slices);
      **peel_away = verdicts.away_seen();
      Ok((built, greens.owing.get(), greens.examined.get()))
    })
    .unwrap_or((true, None, 0));
    state.count(HOLDING_EXAMINED, examined);
    // A green the command reached that owes its push is made first, through the door, and the command waits for it a
    // turn at a time (P2): its verdict reads it. A turn that only found it builds nothing and is not counted.
    if let Some(green) = owing {
      return self.door(green);
    }
    *self.door = None;
    let ended = slates_rt::futures::now_ns();
    // What the turn may pass its budget by: its opening (the admission's turn and the views set up before its first
    // unit, which always runs), one unit, and what it does after its last ask (the views given back). A turn is those
    // three and the units before its deadline, measured on one clock.
    let (unit, opening, asked_at, step) =
      preparing.as_ref().map_or((0, 0, began, None), |prepared| {
        (
          prepared.longest_unit_ns(),
          prepared.opening_ns(),
          prepared.asked_at_ns(),
          prepared.step_report(),
        )
      });
    let tail = ended.saturating_sub(asked_at.max(began));
    // Its budget: what its step was allowed (to the end of its last quantum, or its ceiling).
    let budget = step.map_or(quantum, |report| report.allowed_ns.max(quantum));
    state
      .push_turns
      .counted(ended.saturating_sub(began), (budget, (opening, unit, tail)));
    state.push_turns.stepped(step);
    built
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
      Entry::Create { name, from, push } => {
        created(state, self.record, (&name, from, push), (command.new, at))
      }
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
  (name, from, push): (&[u8], Option<(u64, u64)>, slates_merge::push::DeferredPush),
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
        // The unborn default's first version owes the push too (P2): the green named already, so an update that defers.
        let change = BranchChange::UpdateDeferred { green: empty, push };
        return Ok((change, (Target::Green(empty), Some(new))));
      }
      (empty, 0)
    }
  };
  // The created green owes its push, its content derived when first reached (P2, `crate::green_ready`).
  let change = BranchChange::CreateDeferred {
    name: namespace.clone(),
    from,
    version,
    push,
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
  /// The first green the push reached that owes its push (P2): lent as none, so the push waits at the door for it
  /// (`crate::green_ready`) rather than judge without it.
  owing: std::cell::Cell<Option<DbVolumeId>>,
  /// The greens [`ShardGreens::holding`] examined, counted [`HOLDING_EXAMINED`] once the reader is done.
  examined: std::cell::Cell<u64>,
}

/// Format: the status counter of the greens a push's search for the green holding a commit examined
/// (`ShardGreens::holding`): each one lent or whose versions were read.
pub(crate) const HOLDING_EXAMINED: &str = "git_push.holding.examined";

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

  fn holding_head<T>(
    &self,
    head: &ObjectId,
    with: impl FnOnce(GreenRead<'_>, Version) -> T,
  ) -> Option<T> {
    let (green, version) = self.holding(head)?;
    self.lent(green, |read| with(read, version))
  }
}

impl ShardGreens<'_, '_> {
  /// Runs `with` over `green` lent for one call as the push reads it: its key, its log's position and its engine.
  fn lent<T>(&self, green: DbVolumeId, with: impl FnOnce(GreenRead<'_>) -> T) -> Option<T> {
    let position = self.state.db.partition().green_head(green);
    if self.state.greens.is_owing(&green) && self.owing.get().is_none() {
      self.owing.set(Some(green));
    }
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

  /// The tree the object `head` names (module docs), as git peels a ref (A-836: a ref may name any object): a commit's,
  /// a tree itself, the end of a chain of annotated tags followed to its end, and the empty tree for a blob (a ref naming
  /// no tree holds no files). The chain's tags are distinct objects the store holds, each named by its content, so it
  /// ends.
  fn tree_of(&self, head: &ObjectId) -> Option<ObjectId> {
    let (content, git) = self.store;
    let mut target = *head;
    let mut row = git.repository.object(content, &target)?;
    while row.kind == Kind::Tag {
      let (_, bytes) = self.indexed.object(&target)?;
      let tag = Tag::parse(bytes, &mut Admission::new(u64::MAX)).ok()?;
      target = tag.object(head.format())?;
      row = git.repository.object(content, &target)?;
    }
    match row.kind {
      Kind::Commit => git.repository.id_at(content, row.commit?.tree),
      Kind::Tree => Some(target),
      Kind::Blob | Kind::Tag => slates_git::object_name::empty_tree(head.format()),
    }
  }

  /// The green a create whose first parent is `parent` is made from, and its version (module docs, "A create"): the
  /// newest of the store's versions of a member whose head is `parent`, where a branch can still be made at it, the
  /// default branch's first, then the members as they joined. A member other than the default branch that owes its
  /// push is passed over (P2): a create made from it would make it at the push, as git/git's refs at commits other refs
  /// hold would make most of them; the create owes its own push from the next base instead, its chain entry the push's
  /// record either way. The default branch, the base every create falls back to, is made first through the door.
  fn holding(&self, parent: &ObjectId) -> Option<(DbVolumeId, Version)> {
    let (content, git) = self.store;
    let repositories = self.state.db.partition().repositories();
    let default = self.default_green();
    // Goal 93: only the greens holding `parent` are asked, from the store's holders' map, each placed by the rank it
    // joined at (the default branch first): what the search costs follows the greens at the commit, never the
    // repository's greens, where it lent each member and read each one's versions.
    let mut held: Vec<(Option<u64>, DbVolumeId, u64, usize)> = git
      .holders(content, parent)
      .into_iter()
      .filter_map(|(key, at)| {
        if let Some(green) = default.filter(|green| green_key(*green) == key) {
          return Some((None, green, key, at));
        }
        let (green, rank) = repositories.member_keyed(self.record.id, key)?;
        Some((Some(rank), green, key, at))
      })
      .collect();
    held.sort_unstable_by_key(|(rank, ..)| *rank);
    held.into_iter().find_map(|(_, green, key, at)| {
      self.examined.set(self.examined.get().saturating_add(1));
      if Some(green) != default && self.state.greens.is_owing(&green) {
        return None;
      }
      let (number, tree) = git.version_at(content, key, at)?;
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
    // A green owing its push is reached here too (P2's door): whether it can be branched at reads its engine.
    if self.state.greens.is_owing(&green) && self.owing.get().is_none() {
      self.owing.set(Some(green));
    }
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
