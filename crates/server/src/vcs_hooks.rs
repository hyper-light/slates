//! The hooks' protocol (§4.17.1 "Policy: protected branches, access and hooks"; shared piece 7, A-597): which hook
//! runs at which point and over what, what it is given, what it decides, how long it may take, and the runner that
//! holds one run to all of that.
//!
//! **The points** ([`HookPoint`]). Admission hooks judge a change before the verdict and may refuse it with words the
//! client prints: git's `pre-receive` (once a push, every command on its standard input; non-zero refuses every ref)
//! and `update` (once a ref, three arguments; non-zero refuses that ref) [B: `githooks.adoc` at git e1ec09b23];
//! Mercurial's `pretxnchangegroup` ("A non-zero status will cause the transaction to be rolled back") [B: `config.txt`
//! "hooks", Mercurial 7.2.4]; Subversion's `pre-commit` and `pre-revprop-change` [C: `libsvn_repos/hooks.c` at
//! fd4e82b00]. Observing hooks run after the commit and change no answer: git's `post-receive` ("does not affect the
//! outcome"), Mercurial's `changegroup`, Subversion's `post-commit`. An admission point with no hook lets the change
//! through, except Subversion's `pre-revprop-change`, which then refuses every revision-property change ("default to
//! MASSIVE PARANOIA", `hooks.c`). Several hooks at one point run in the order the tool's configuration gives
//! (Mercurial's `priority.` prefix, then the order written), and at an admission point the first that does not accept
//! decides and the rest do not run, as each tool stops at its first refusal.
//!
//! **What a hook runs over.** A read-only attachment of the change, never the green itself: an admission hook sees the
//! work's post-state ([`PendingView`]: git's quarantine, Mercurial's pending transaction named by `HG_PENDING`,
//! Subversion's transaction through `svnlook -t`), an observing hook the version the commit made ([`CommittedView`]),
//! and `pre-revprop-change` the revision whose property would change. Each tool's server makes that view (git's
//! bare-repository view, Mercurial's revlog view, Subversion's `svnlook`, their own pieces); this module names it.
//!
//! **What a hook is given** ([`HookInput`]), in each tool's own terms: git's `<old-oid> SP <new-oid> SP <ref-name> LF`
//! lines and its push options as `GIT_PUSH_OPTION_<i>` and `GIT_PUSH_OPTION_COUNT`, unset when none were negotiated
//! (`builtin/receive-pack.c` `run_receive_hook`); Mercurial's `HG_*` environment (`HG_HOOKTYPE`, `HG_HOOKNAME`,
//! `HG_NODE`, `HG_NODE_LAST`, `HG_SOURCE`, `HG_URL`, and `HG_PENDING` for a hook inside the transaction); Subversion's
//! arguments and standard input as `hooks.c` builds them (`LOCK-TOKENS:` and a `path|token` line a token, then a blank
//! line; a property's new value). A hook's words are what it writes: git and Mercurial take both of its streams (git
//! joins a hook's standard output to its standard error, `run_hooks_opt`), Subversion standard error alone (it sends
//! standard output to the null device, `run_hook_cmd`).
//!
//! **How long a hook may take** ([`HookTimes`]). Each configured hook's deadline is learned from its own runs, as
//! `crate::learned_wait` learns a wait (RFC 6298's estimator; Karn's rule; the back-off), and never a constant (§4.17.1's
//! rule, R3). Before any run has finished, a run has no deadline but its client's connection: it ends when the client
//! leaves, and the first run that finishes is the first sample (§4.17.1). A run cut short, at its bound or by its
//! client leaving, is no sample, since its true length is unknown; one cut at its bound doubles the next bound, with
//! no ceiling of the runner's own, as every run still ends when its client leaves.
//!
//! **The runner** ([`HookRun`]). A state machine with no I/O: its driver starts the hook's process (in its own process
//! group or job, on Linux inside the launcher's namespace, §4.6), reports what happens ([`Event`]), and does what each
//! answer says ([`Step`]): wait, with the timer at the learned deadline; kill the hook and what it started; or take the
//! [`Outcome`]: accepted, refused with the hook's words and exit, timed out at its learned bound, abandoned when its
//! client left, or unstartable with the cause. A hook that exits while something it started still holds its output is
//! judged by its own exit, and that leftover is ended at the deadline or when the client leaves. A run never outlives
//! its client (§2 ban 9: no task without an owner); git lets a `post-receive` run on after its client has gone, and a
//! notification that must outlive its push belongs to a repository's own queue (§4.17.3's observing hooks), not here.

use crate::learned_wait::LearnedWait;

/// When a hook runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
  /// Before the verdict: it may refuse the change.
  Admission,
  /// After the commit: it changes no answer.
  Observing,
}

/// The streams a hook's words come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WordsFrom {
  /// Standard output joined to standard error (git's `run_hooks_opt` sets `stdout_to_stderr`; Mercurial shows a
  /// hook's output to the client whole).
  BothStreams,
  /// Standard error alone, standard output going to the null device (Subversion's `run_hook_cmd`).
  ErrorStream,
}

/// What a point with no hook configured answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unconfigured {
  /// The change goes on.
  Accepted,
  /// The change is refused with these words (Subversion's `pre-revprop-change`).
  Refused(&'static str),
}

/// The words Subversion refuses a revision-property change with when no `pre-revprop-change` hook exists
/// (`svn_repos__hooks_pre_revprop_change`, `hooks.c` at fd4e82b00, `SVN_ERR_REPOS_DISABLED_FEATURE`).
const NO_REVPROP_HOOK: &str = "Repository has not been enabled to accept revision propchanges;\nask the administrator to create a pre-revprop-change hook";

/// A hook point this protocol serves (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookPoint {
  /// git's `pre-receive`.
  GitPreReceive,
  /// git's `update`.
  GitUpdate,
  /// git's `post-receive`.
  GitPostReceive,
  /// Mercurial's `pretxnchangegroup`.
  HgPretxnchangegroup,
  /// Mercurial's `changegroup`.
  HgChangegroup,
  /// Subversion's `pre-commit`.
  SvnPreCommit,
  /// Subversion's `pre-revprop-change`.
  SvnPreRevpropChange,
  /// Subversion's `post-commit`.
  SvnPostCommit,
}

impl HookPoint {
  /// When it runs.
  pub(crate) fn phase(self) -> Phase {
    match self {
      Self::GitPreReceive
      | Self::GitUpdate
      | Self::HgPretxnchangegroup
      | Self::SvnPreCommit
      | Self::SvnPreRevpropChange => Phase::Admission,
      Self::GitPostReceive | Self::HgChangegroup | Self::SvnPostCommit => Phase::Observing,
    }
  }

  /// Its name in its tool.
  pub(crate) fn name(self) -> &'static str {
    match self {
      Self::GitPreReceive => "pre-receive",
      Self::GitUpdate => "update",
      Self::GitPostReceive => "post-receive",
      Self::HgPretxnchangegroup => "pretxnchangegroup",
      Self::HgChangegroup => "changegroup",
      Self::SvnPreCommit => "pre-commit",
      Self::SvnPreRevpropChange => "pre-revprop-change",
      Self::SvnPostCommit => "post-commit",
    }
  }

  /// The streams its words come from.
  pub(crate) fn words_from(self) -> WordsFrom {
    match self {
      Self::SvnPreCommit | Self::SvnPreRevpropChange | Self::SvnPostCommit => {
        WordsFrom::ErrorStream
      }
      _ => WordsFrom::BothStreams,
    }
  }

  /// What it answers with no hook configured.
  pub(crate) fn unconfigured(self) -> Unconfigured {
    match self {
      Self::SvnPreRevpropChange => Unconfigured::Refused(NO_REVPROP_HOOK),
      _ => Unconfigured::Accepted,
    }
  }
}

/// A read-only attachment of a change's post-state, made by the tool's server for an admission hook: git's quarantine,
/// Mercurial's pending transaction, Subversion's transaction. Named by the attachment the server made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingView(pub(crate) u64);

/// A read-only attachment of a committed version, made by the tool's server for an observing hook (and for
/// `pre-revprop-change`, the revision whose property would change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommittedView(pub(crate) u64);

/// The read-only attachment a hook runs over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
  /// A change's post-state, before the verdict.
  Pending(PendingView),
  /// A committed version.
  Committed(CommittedView),
}

/// A value of a hook's environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnvValue {
  /// These bytes.
  Bytes(Vec<u8>),
  /// The root of the hook's view as the hook sees it (Mercurial's `HG_PENDING`), which the driver knows.
  ViewRoot,
  /// Removed from the environment (git's `GIT_PUSH_OPTION_COUNT` when no push options were negotiated).
  Unset,
}

/// What a hook is started with (the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HookInput {
  /// Its point.
  pub(crate) point: HookPoint,
  /// Its arguments, after the program.
  pub(crate) args: Vec<Vec<u8>>,
  /// Its environment's changes, in order.
  pub(crate) env: Vec<(String, EnvValue)>,
  /// Its standard input, whole.
  pub(crate) stdin: Vec<u8>,
  /// What it runs over.
  pub(crate) view: View,
}

/// One command of a git push, as `receive-pack` holds it and its hooks read it: object names in hex, the old one all
/// zeroes for a ref the push creates, the new one for a ref it deletes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefUpdate {
  /// The ref's value the client saw.
  pub(crate) old: String,
  /// The value it asks for.
  pub(crate) new: String,
  /// The ref's full name.
  pub(crate) name: String,
}

/// The lines `pre-receive` and `post-receive` read: `<old-oid> SP <new-oid> SP <ref-name> LF` each (`githooks.adoc`).
fn update_lines(updates: &[RefUpdate]) -> Vec<u8> {
  updates
    .iter()
    .flat_map(|update| format!("{} {} {}\n", update.old, update.new, update.name).into_bytes())
    .collect()
}

/// The push options as `run_receive_hook` sets them: `GIT_PUSH_OPTION_<i>` each and `GIT_PUSH_OPTION_COUNT`, or the
/// count unset when the push negotiated none.
fn push_option_env(options: Option<&[Vec<u8>]>) -> Vec<(String, EnvValue)> {
  let Some(options) = options else {
    return vec![("GIT_PUSH_OPTION_COUNT".to_owned(), EnvValue::Unset)];
  };
  let mut env: Vec<(String, EnvValue)> = options
    .iter()
    .enumerate()
    .map(|(at, option)| {
      (
        format!("GIT_PUSH_OPTION_{at}"),
        EnvValue::Bytes(option.clone()),
      )
    })
    .collect();
  env.push((
    "GIT_PUSH_OPTION_COUNT".to_owned(),
    EnvValue::Bytes(options.len().to_string().into_bytes()),
  ));
  env
}

/// git's `pre-receive`: every command of the push on standard input, the push options in the environment.
pub(crate) fn git_pre_receive(
  view: PendingView,
  updates: &[RefUpdate],
  options: Option<&[Vec<u8>]>,
) -> HookInput {
  HookInput {
    point: HookPoint::GitPreReceive,
    args: Vec::new(),
    env: push_option_env(options),
    stdin: update_lines(updates),
    view: View::Pending(view),
  }
}

/// git's `update` for one command: the ref's name, its old value and its new one, as arguments.
pub(crate) fn git_update(view: PendingView, update: &RefUpdate) -> HookInput {
  HookInput {
    point: HookPoint::GitUpdate,
    args: vec![
      update.name.clone().into_bytes(),
      update.old.clone().into_bytes(),
      update.new.clone().into_bytes(),
    ],
    env: Vec::new(),
    stdin: Vec::new(),
    view: View::Pending(view),
  }
}

/// git's `post-receive`: on standard input, the commands that updated their refs (`run_receive_hook` skips the
/// refused ones), the push options in the environment.
pub(crate) fn git_post_receive(
  view: CommittedView,
  applied: &[RefUpdate],
  options: Option<&[Vec<u8>]>,
) -> HookInput {
  HookInput {
    point: HookPoint::GitPostReceive,
    args: Vec::new(),
    env: push_option_env(options),
    stdin: update_lines(applied),
    view: View::Committed(view),
  }
}

/// A Mercurial push's changesets, as its hooks read them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Changegroup {
  /// The first new changeset's node, in hex (`HG_NODE`).
  pub(crate) first: String,
  /// The last one's (`HG_NODE_LAST`).
  pub(crate) last: String,
  /// Where the changes came from (`HG_URL`), as the transport names it (`remote:http:...`, `remote:ssh:...`).
  pub(crate) url: String,
}

/// Format: `HG_SOURCE` for every change a remote client pushes ("Source is "serve" for all remote access (http or
/// ssh)", `hgext/acl.py`).
const HG_SOURCE_SERVE: &str = "serve";

/// The `HG_*` environment a Mercurial hook reads for a changegroup: its type and configured name, the changesets and
/// where they came from.
fn changegroup_env(
  point: HookPoint,
  hook_name: &str,
  changes: &Changegroup,
) -> Vec<(String, EnvValue)> {
  let bytes = |text: &str| EnvValue::Bytes(text.as_bytes().to_vec());
  vec![
    ("HG_HOOKTYPE".to_owned(), bytes(point.name())),
    ("HG_HOOKNAME".to_owned(), bytes(hook_name)),
    ("HG_NODE".to_owned(), bytes(&changes.first)),
    ("HG_NODE_LAST".to_owned(), bytes(&changes.last)),
    ("HG_SOURCE".to_owned(), bytes(HG_SOURCE_SERVE)),
    ("HG_URL".to_owned(), bytes(&changes.url)),
  ]
}

/// Mercurial's `pretxnchangegroup`, the hook configured as `hook_name` (`pretxnchangegroup.acl`, say): the
/// changegroup's environment and `HG_PENDING` naming the view, so the hook's own `hg` reads the pending changesets.
pub(crate) fn hg_pretxnchangegroup(
  view: PendingView,
  hook_name: &str,
  changes: &Changegroup,
) -> HookInput {
  let mut env = changegroup_env(HookPoint::HgPretxnchangegroup, hook_name, changes);
  env.push(("HG_PENDING".to_owned(), EnvValue::ViewRoot));
  HookInput {
    point: HookPoint::HgPretxnchangegroup,
    args: Vec::new(),
    env,
    stdin: Vec::new(),
    view: View::Pending(view),
  }
}

/// Mercurial's `changegroup`, the hook configured as `hook_name`: the changegroup's environment, over the committed
/// version.
pub(crate) fn hg_changegroup(
  view: CommittedView,
  hook_name: &str,
  changes: &Changegroup,
) -> HookInput {
  HookInput {
    point: HookPoint::HgChangegroup,
    args: Vec::new(),
    env: changegroup_env(HookPoint::HgChangegroup, hook_name, changes),
    stdin: Vec::new(),
    view: View::Committed(view),
  }
}

/// One lock token a Subversion commit presents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LockToken {
  /// The locked path, URI-escaped as `svn_path_uri_autoescape` escapes it.
  pub(crate) path: String,
  /// The token.
  pub(crate) token: String,
}

/// Format: the first line of `pre-commit`'s standard input (`lock_token_content`, `hooks.c`).
const LOCK_TOKENS_HEADER: &str = "LOCK-TOKENS:\n";

/// Subversion's `pre-commit`: the repository's directory and the transaction's name as arguments; on standard input
/// `LOCK-TOKENS:`, a `path|token` line a token, and a blank line (`lock_token_content`).
pub(crate) fn svn_pre_commit(
  view: PendingView,
  repository: &str,
  transaction: &str,
  tokens: &[LockToken],
) -> HookInput {
  let mut stdin = LOCK_TOKENS_HEADER.as_bytes().to_vec();
  for token in tokens {
    stdin.extend_from_slice(format!("{}|{}\n", token.path, token.token).as_bytes());
  }
  stdin.push(b'\n');
  HookInput {
    point: HookPoint::SvnPreCommit,
    args: vec![
      repository.as_bytes().to_vec(),
      transaction.as_bytes().to_vec(),
    ],
    env: Vec::new(),
    stdin,
    view: View::Pending(view),
  }
}

/// How a revision property would change: added, modified or deleted (`pre-revprop-change`'s action letter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RevpropAction {
  /// `A`.
  Added,
  /// `M`.
  Modified,
  /// `D`.
  Deleted,
}

impl RevpropAction {
  /// Its letter.
  fn letter(self) -> &'static [u8] {
    match self {
      Self::Added => b"A",
      Self::Modified => b"M",
      Self::Deleted => b"D",
    }
  }
}

/// Subversion's `pre-revprop-change`: the repository's directory, the revision, the author (empty for none), the
/// property's name and the action as arguments; the new value on standard input, nothing for a deletion.
pub(crate) fn svn_pre_revprop_change(
  view: CommittedView,
  repository: &str,
  revision: u64,
  author: &str,
  property: &str,
  action: RevpropAction,
  new_value: &[u8],
) -> HookInput {
  HookInput {
    point: HookPoint::SvnPreRevpropChange,
    args: vec![
      repository.as_bytes().to_vec(),
      revision.to_string().into_bytes(),
      author.as_bytes().to_vec(),
      property.as_bytes().to_vec(),
      action.letter().to_vec(),
    ],
    env: Vec::new(),
    stdin: new_value.to_vec(),
    view: View::Committed(view),
  }
}

/// Subversion's `post-commit`: the repository's directory, the new revision and the transaction's name as arguments.
pub(crate) fn svn_post_commit(
  view: CommittedView,
  repository: &str,
  revision: u64,
  transaction: &str,
) -> HookInput {
  HookInput {
    point: HookPoint::SvnPostCommit,
    args: vec![
      repository.as_bytes().to_vec(),
      revision.to_string().into_bytes(),
      transaction.as_bytes().to_vec(),
    ],
    env: Vec::new(),
    stdin: Vec::new(),
    view: View::Committed(view),
  }
}

/// The back-off's ceiling: none of the runner's own, since every run ends when its client leaves (§4.17.2's table,
/// "the push's connection: a hook ends when its client leaves"); the type's largest value.
const NO_CEILING_NS: u64 = u64::MAX;

/// One configured hook's runs, learned (the module doc).
#[derive(Debug, Default)]
pub(crate) struct HookTimes {
  /// The runs that finished, and the back-off of those cut at their bound since.
  runs: LearnedWait,
  /// How many runs finished on their own: each one a sample.
  samples: u64,
  /// How many runs were cut at their bound: each one a back-off.
  cuts: u64,
}

impl HookTimes {
  /// Derived: the bound the next run is held to: none before any run has finished, since until then a run's only bound
  /// is its client's connection; then the tail of the finished runs plus a liveness window, doubled for each run since
  /// the last finished one that was cut at its bound (`LearnedWait::bound_ns`), with no ceiling of its own.
  pub(crate) fn bound_ns(&self) -> Option<u64> {
    (self.samples > 0).then(|| self.runs.bound_ns(NO_CEILING_NS))
  }

  /// How many runs finished on their own (the samples; a non-vacuity counter for the learned deadline).
  pub(crate) fn finished_runs(&self) -> u64 {
    self.samples
  }

  /// How many runs were cut at their bound (the back-offs).
  pub(crate) fn cut_runs(&self) -> u64 {
    self.cuts
  }

  /// A run that finished `ran_ns` after it started: a sample, and the end of any back-off.
  fn finished(&mut self, ran_ns: u64) {
    self.runs.completed(ran_ns);
    self.samples = self.samples.saturating_add(1);
  }

  /// A run cut at its bound: no sample, and the bound backed off.
  fn ran_out(&mut self) {
    self.runs.ran_out();
    self.cuts = self.cuts.saturating_add(1);
  }
}

/// How a hook's process ended, as its driver reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitStatus {
  /// It exited with this code.
  Code(i32),
  /// This signal ended it.
  Signal(i32),
}

/// Why a hook's program could not be started, as its driver reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartRefused {
  /// There is no such program.
  NotFound,
  /// It may not be run.
  NotPermitted,
  /// The machine had no room for it: processes, memory or descriptors.
  NoRoom,
  /// Another failure, by the operating system's number.
  Other(i32),
}

/// What a hook wrote: kept up to a cap its caller derives (the connection's share of the shard's admittable memory,
/// as §4.17.4 caps a client's text), the bytes past it counted, never held.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Words {
  /// The bytes kept, in the order written.
  pub(crate) kept: Vec<u8>,
  /// How many bytes were written past the cap.
  pub(crate) dropped: u64,
}

impl Words {
  /// `bytes` written: kept while there is room under `cap`, counted past it.
  fn append(&mut self, bytes: &[u8], cap: usize) {
    let room = cap.saturating_sub(self.kept.len()).min(bytes.len());
    let Some((kept, past)) = bytes.split_at_checked(room) else {
      return;
    };
    self.kept.extend_from_slice(kept);
    self.dropped = self
      .dropped
      .saturating_add(u64::try_from(past.len()).unwrap_or(u64::MAX));
  }
}

/// How one run of a hook ended (the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
  /// It exited 0: the change goes on (for `update`, that ref's).
  Accepted {
    /// What it wrote.
    words: Words,
  },
  /// It exited non-zero, or a signal the runner did not send ended it: an admission point refuses, naming the hook,
  /// with its words; an observing point counts it.
  Refused {
    /// How it ended.
    status: ExitStatus,
    /// What it wrote.
    words: Words,
  },
  /// It ran past the bound learned from its own runs and was killed: an admission point refuses, naming the hook and
  /// the bound (§4.17.1's failure matrix).
  TimedOut {
    /// The bound it ran past.
    bound_ns: u64,
    /// What it wrote.
    words: Words,
  },
  /// Its client left before it finished: it was killed, and no one is left to answer.
  Abandoned,
  /// Its program could not be started: an admission point refuses, naming the hook and the cause.
  Unstartable {
    /// Why.
    cause: StartRefused,
  },
}

impl Outcome {
  /// The outcome of a hook that ended `status` on its own, having written `words`.
  fn of_exit(status: ExitStatus, words: Words) -> Self {
    match status {
      ExitStatus::Code(0) => Self::Accepted { words },
      _ => Self::Refused { status, words },
    }
  }
}

/// What the driver reports about a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event<'a> {
  /// The hook wrote these bytes to its words stream.
  Output(&'a [u8]),
  /// Its words stream ended: every process holding it closed it.
  OutputEnded,
  /// Its process exited, at `at_ns` on the shard's monotonic clock.
  Exited {
    /// How it ended.
    status: ExitStatus,
    /// When.
    at_ns: u64,
  },
  /// The timer the runner asked for fired, at `now_ns`.
  Timer {
    /// The shard's monotonic clock.
    now_ns: u64,
  },
  /// The client whose change the hook judges left.
  ClientLeft,
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
  /// Wait for the next event, with the timer armed for `wake_at_ns` when there is one.
  Wait {
    /// The run's deadline, none before its hook has a learned bound.
    wake_at_ns: Option<u64>,
  },
  /// Kill the hook's process and every process it started, then report its exit.
  Kill,
  /// The run is over: close the hook's words stream, after killing what it started when `end_leftovers`.
  Done {
    /// How it ended.
    outcome: Outcome,
    /// Whether processes it started still hold its words stream and must be ended.
    end_leftovers: bool,
  },
}

/// Why the runner killed a hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kill {
  /// It ran past this bound.
  PastBound(u64),
  /// Its client left.
  ClientLeft,
}

/// Where a run stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunState {
  /// Its process runs.
  Running,
  /// Its process exited on its own with this status, and something it started still holds its words stream.
  Draining(ExitStatus),
  /// It was asked to die, for this reason.
  Killing(Kill),
  /// It is over.
  Done,
}

/// One run of one hook (the module doc).
#[derive(Debug)]
pub(crate) struct HookRun {
  /// Where it stands.
  state: RunState,
  /// What it wrote.
  words: Words,
  /// How many bytes of words are kept.
  words_cap: usize,
  /// When its process started, on the shard's monotonic clock.
  started_ns: u64,
  /// The bound it is held to: none while its hook has no learned bound.
  bound_ns: Option<u64>,
  /// Whether its words stream is open.
  output_open: bool,
}

impl HookRun {
  /// A run whose process its driver started at `started_ns`, held to the bound `times` has learned, its words kept to
  /// `words_cap` bytes; and what the driver does first: wait, with the timer at the deadline when there is one.
  pub(crate) fn started(started_ns: u64, times: &HookTimes, words_cap: usize) -> (Self, Step) {
    let run = Self {
      state: RunState::Running,
      words: Words::default(),
      words_cap,
      started_ns,
      bound_ns: times.bound_ns(),
      output_open: true,
    };
    let step = run.wait();
    (run, step)
  }

  /// The outcome of a hook whose program could not be started: no run, so no sample.
  pub(crate) fn unstartable(cause: StartRefused) -> Outcome {
    Outcome::Unstartable { cause }
  }

  /// What `event` changes, `times` taught what the run shows of its hook, and what the driver does next. A run that is
  /// over answers every later event with nothing to do.
  pub(crate) fn on(&mut self, event: Event<'_>, times: &mut HookTimes) -> Step {
    match self.state {
      RunState::Running => self.running(event, times),
      RunState::Draining(status) => self.draining(status, event),
      RunState::Killing(kill) => self.killing(kill, event),
      RunState::Done => Step::Wait { wake_at_ns: None },
    }
  }

  /// The deadline: its start plus its bound.
  fn deadline_ns(&self) -> Option<u64> {
    self
      .bound_ns
      .map(|bound_ns| self.started_ns.saturating_add(bound_ns))
  }

  /// Wait for the next event, the timer at the deadline.
  fn wait(&self) -> Step {
    Step::Wait {
      wake_at_ns: self.deadline_ns(),
    }
  }

  /// An event while the hook's process runs.
  fn running(&mut self, event: Event<'_>, times: &mut HookTimes) -> Step {
    match event {
      Event::Output(bytes) => {
        self.words.append(bytes, self.words_cap);
        self.wait()
      }
      Event::OutputEnded => {
        self.output_open = false;
        self.wait()
      }
      Event::Exited { status, at_ns } => self.exited(status, at_ns, times),
      Event::Timer { now_ns } => self.timer(now_ns, times),
      Event::ClientLeft => self.kill(Kill::ClientLeft),
    }
  }

  /// The hook's process exited on its own at `at_ns`: a sample of its hook's runs, and its outcome once its words
  /// stream has ended.
  fn exited(&mut self, status: ExitStatus, at_ns: u64, times: &mut HookTimes) -> Step {
    times.finished(at_ns.saturating_sub(self.started_ns));
    if self.output_open {
      self.state = RunState::Draining(status);
      return self.wait();
    }
    self.finish_exit(status, false)
  }

  /// The timer fired at `now_ns`: at or past the deadline, the run is cut at its bound (no sample, the bound backed
  /// off) and its hook killed; before it, a wait for the deadline still.
  fn timer(&mut self, now_ns: u64, times: &mut HookTimes) -> Step {
    match (self.bound_ns, self.deadline_ns()) {
      (Some(bound_ns), Some(deadline_ns)) if now_ns >= deadline_ns => {
        times.ran_out();
        self.kill(Kill::PastBound(bound_ns))
      }
      _ => self.wait(),
    }
  }

  /// Ask the driver to kill the hook, for `why`.
  fn kill(&mut self, why: Kill) -> Step {
    self.state = RunState::Killing(why);
    Step::Kill
  }

  /// An event after the hook exited `status` on its own while its words stream stayed open.
  fn draining(&mut self, status: ExitStatus, event: Event<'_>) -> Step {
    match event {
      Event::Output(bytes) => {
        self.words.append(bytes, self.words_cap);
        self.wait()
      }
      Event::OutputEnded => {
        self.output_open = false;
        self.finish_exit(status, false)
      }
      Event::Timer { now_ns }
        if self
          .deadline_ns()
          .is_some_and(|deadline| now_ns >= deadline) =>
      {
        self.finish_exit(status, true)
      }
      Event::ClientLeft => self.finish(Outcome::Abandoned, true),
      Event::Exited { .. } | Event::Timer { .. } => self.wait(),
    }
  }

  /// An event after the runner asked the driver to kill the hook, for `why`.
  fn killing(&mut self, why: Kill, event: Event<'_>) -> Step {
    match event {
      Event::Output(bytes) => {
        self.words.append(bytes, self.words_cap);
        Step::Wait { wake_at_ns: None }
      }
      Event::OutputEnded => {
        self.output_open = false;
        Step::Wait { wake_at_ns: None }
      }
      Event::Exited { .. } => {
        let outcome = match why {
          Kill::PastBound(bound_ns) => Outcome::TimedOut {
            bound_ns,
            words: std::mem::take(&mut self.words),
          },
          Kill::ClientLeft => Outcome::Abandoned,
        };
        self.finish(outcome, false)
      }
      Event::ClientLeft => {
        self.state = RunState::Killing(Kill::ClientLeft);
        Step::Wait { wake_at_ns: None }
      }
      Event::Timer { .. } => Step::Wait { wake_at_ns: None },
    }
  }

  /// The run is over with the outcome of its hook's own exit, `status`, and the words it wrote.
  fn finish_exit(&mut self, status: ExitStatus, end_leftovers: bool) -> Step {
    let words = std::mem::take(&mut self.words);
    self.finish(Outcome::of_exit(status, words), end_leftovers)
  }

  /// The run is over with `outcome`.
  fn finish(&mut self, outcome: Outcome, end_leftovers: bool) -> Step {
    self.state = RunState::Done;
    Step::Done {
      outcome,
      end_leftovers,
    }
  }
}

#[cfg(test)]
mod tests {
  use proptest::prelude::*;

  use super::{
    Changegroup, CommittedView, EnvValue, Event, ExitStatus, HookPoint, HookRun, HookTimes,
    LockToken, Outcome, PendingView, Phase, RefUpdate, RevpropAction, StartRefused, Step,
    Unconfigured, View, Words, WordsFrom, git_post_receive, git_pre_receive, git_update,
    hg_changegroup, hg_pretxnchangegroup, svn_post_commit, svn_pre_commit, svn_pre_revprop_change,
  };
  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: a hook run as a quick check takes it, 30 ms.
  const QUICK_NS: u64 = 30_000_000;
  /// Shape: a cap on a hook's words, larger than any test's output but where one test passes it.
  const CAP: usize = 64;
  /// Shape: when the tests' runs start, an arbitrary point on the shard's clock.
  const START_NS: u64 = 1_000_000_000;
  /// Shape: the zero object name of a SHA-1 repository, which a creating command's old value is.
  const ZERO: &str = "0000000000000000000000000000000000000000";

  /// A hook's times after eight quick runs finished.
  fn learned() -> HookTimes {
    let mut times = HookTimes::default();
    for _ in 0..8 {
      let (mut run, _) = HookRun::started(START_NS, &times, CAP);
      let step = run.on(Event::OutputEnded, &mut times);
      assert_eq!(
        step,
        Step::Wait {
          wake_at_ns: run.deadline_ns()
        }
      );
      run.on(
        Event::Exited {
          status: ExitStatus::Code(0),
          at_ns: START_NS + QUICK_NS,
        },
        &mut times,
      );
    }
    times
  }

  /// Words holding `bytes`, nothing dropped.
  fn words(bytes: &[u8]) -> Words {
    Words {
      kept: bytes.to_vec(),
      dropped: 0,
    }
  }

  /// A-597 (the points, as each tool's documentation and code have them). Do: ask every point its phase, name, words
  /// streams and what it answers unconfigured. Expect: the admission hooks are git's pre-receive and update,
  /// Mercurial's pretxnchangegroup and Subversion's pre-commit and pre-revprop-change; the observing ones post-receive,
  /// changegroup and post-commit; Subversion's words are standard error alone; and only pre-revprop-change refuses when
  /// absent, with `hooks.c`'s words.
  #[test]
  fn every_point_is_its_tools_own() {
    let points = [
      (
        HookPoint::GitPreReceive,
        Phase::Admission,
        "pre-receive",
        WordsFrom::BothStreams,
      ),
      (
        HookPoint::GitUpdate,
        Phase::Admission,
        "update",
        WordsFrom::BothStreams,
      ),
      (
        HookPoint::GitPostReceive,
        Phase::Observing,
        "post-receive",
        WordsFrom::BothStreams,
      ),
      (
        HookPoint::HgPretxnchangegroup,
        Phase::Admission,
        "pretxnchangegroup",
        WordsFrom::BothStreams,
      ),
      (
        HookPoint::HgChangegroup,
        Phase::Observing,
        "changegroup",
        WordsFrom::BothStreams,
      ),
      (
        HookPoint::SvnPreCommit,
        Phase::Admission,
        "pre-commit",
        WordsFrom::ErrorStream,
      ),
      (
        HookPoint::SvnPreRevpropChange,
        Phase::Admission,
        "pre-revprop-change",
        WordsFrom::ErrorStream,
      ),
      (
        HookPoint::SvnPostCommit,
        Phase::Observing,
        "post-commit",
        WordsFrom::ErrorStream,
      ),
    ];
    for (point, phase, name, from) in points {
      assert_eq!(point.phase(), phase, "{name}");
      assert_eq!(point.name(), name);
      assert_eq!(point.words_from(), from, "{name}");
      let expected = if point == HookPoint::SvnPreRevpropChange {
        Unconfigured::Refused(
          "Repository has not been enabled to accept revision propchanges;\nask the administrator to create a pre-revprop-change hook",
        )
      } else {
        Unconfigured::Accepted
      };
      assert_eq!(point.unconfigured(), expected, "{name}");
    }
  }

  /// A-597 (git's inputs, `githooks.adoc` and `run_receive_hook`). Do: build pre-receive for a push creating one ref and
  /// moving another with two push options, the same without push options, update for one command, and post-receive
  /// for the one command that applied. Expect: one `old new ref` line a command (the created ref's old value all
  /// zeroes), the options numbered from 0 with their count, the count unset without them, update's three arguments,
  /// and post-receive's line for the applied command alone.
  #[test]
  fn gits_hooks_get_what_receive_pack_gives_them() {
    let create = RefUpdate {
      old: ZERO.to_owned(),
      new: "1111111111111111111111111111111111111111".to_owned(),
      name: "refs/heads/topic".to_owned(),
    };
    let moved = RefUpdate {
      old: "2222222222222222222222222222222222222222".to_owned(),
      new: "3333333333333333333333333333333333333333".to_owned(),
      name: "refs/heads/main".to_owned(),
    };
    let options = [b"ci.skip".to_vec(), b"reviewer=ada".to_vec()];
    let pre = git_pre_receive(
      PendingView(7),
      &[create.clone(), moved.clone()],
      Some(&options),
    );
    assert_eq!(pre.point, HookPoint::GitPreReceive);
    assert!(pre.args.is_empty());
    assert_eq!(pre.view, View::Pending(PendingView(7)));
    assert_eq!(
      pre.stdin,
      format!(
        "{ZERO} 1111111111111111111111111111111111111111 refs/heads/topic\n\
         2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 refs/heads/main\n"
      )
      .into_bytes()
    );
    assert_eq!(
      pre.env,
      vec![
        (
          "GIT_PUSH_OPTION_0".to_owned(),
          EnvValue::Bytes(b"ci.skip".to_vec())
        ),
        (
          "GIT_PUSH_OPTION_1".to_owned(),
          EnvValue::Bytes(b"reviewer=ada".to_vec())
        ),
        (
          "GIT_PUSH_OPTION_COUNT".to_owned(),
          EnvValue::Bytes(b"2".to_vec())
        ),
      ]
    );
    let bare = git_pre_receive(PendingView(7), &[moved.clone()], None);
    assert_eq!(
      bare.env,
      vec![("GIT_PUSH_OPTION_COUNT".to_owned(), EnvValue::Unset)]
    );
    let update = git_update(PendingView(7), &moved);
    assert_eq!(
      update.args,
      vec![
        b"refs/heads/main".to_vec(),
        b"2222222222222222222222222222222222222222".to_vec(),
        b"3333333333333333333333333333333333333333".to_vec(),
      ]
    );
    assert!(update.stdin.is_empty() && update.env.is_empty());
    let post = git_post_receive(CommittedView(8), &[moved], None);
    assert_eq!(post.point, HookPoint::GitPostReceive);
    assert_eq!(post.view, View::Committed(CommittedView(8)));
    assert_eq!(
      post.stdin,
      b"2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 refs/heads/main\n".to_vec()
    );
  }

  /// A-597 (Mercurial's inputs, `config.txt` "hooks" and `hook.py`'s `HG_PENDING`). Do: build pretxnchangegroup and
  /// changegroup for one push. Expect: both carry the hook type and configured name, the first and last new nodes,
  /// `serve` as the source and the client's URL; only the admission hook carries `HG_PENDING`, naming its view's root.
  #[test]
  fn mercurials_hooks_get_its_hook_environment() {
    let changes = Changegroup {
      first: "aa".repeat(20),
      last: "bb".repeat(20),
      url: "remote:ssh:192.0.2.7".to_owned(),
    };
    let pending = hg_pretxnchangegroup(PendingView(3), "pretxnchangegroup.check", &changes);
    let committed = hg_changegroup(CommittedView(4), "changegroup.notify", &changes);
    let value = |input: &super::HookInput, name: &str| {
      input
        .env
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
    };
    let bytes = |text: &str| Some(EnvValue::Bytes(text.as_bytes().to_vec()));
    assert_eq!(value(&pending, "HG_HOOKTYPE"), bytes("pretxnchangegroup"));
    assert_eq!(
      value(&pending, "HG_HOOKNAME"),
      bytes("pretxnchangegroup.check")
    );
    assert_eq!(value(&pending, "HG_NODE"), bytes(&"aa".repeat(20)));
    assert_eq!(value(&pending, "HG_NODE_LAST"), bytes(&"bb".repeat(20)));
    assert_eq!(value(&pending, "HG_SOURCE"), bytes("serve"));
    assert_eq!(value(&pending, "HG_URL"), bytes("remote:ssh:192.0.2.7"));
    assert_eq!(value(&pending, "HG_PENDING"), Some(EnvValue::ViewRoot));
    assert_eq!(value(&committed, "HG_HOOKTYPE"), bytes("changegroup"));
    assert_eq!(value(&committed, "HG_PENDING"), None);
    assert_eq!(committed.view, View::Committed(CommittedView(4)));
  }

  /// A-597 (Subversion's inputs, `hooks.c`). Do: build pre-commit with two lock tokens and with none,
  /// pre-revprop-change for a modification and a deletion, and post-commit. Expect: pre-commit's arguments are the
  /// repository and the transaction, its input `LOCK-TOKENS:`, a `path|token` line a token and a blank line;
  /// pre-revprop-change's five arguments end in the action's letter, with the new value on input and nothing for a
  /// deletion; post-commit's are the repository, the revision and the transaction.
  #[test]
  fn subversions_hooks_get_what_hooks_c_gives_them() {
    let tokens = [
      LockToken {
        path: "/trunk/a%20b.c".to_owned(),
        token: "opaquelocktoken:1".to_owned(),
      },
      LockToken {
        path: "/trunk/c.c".to_owned(),
        token: "opaquelocktoken:2".to_owned(),
      },
    ];
    let pre = svn_pre_commit(PendingView(1), "/srv/repos/x", "41-1a", &tokens);
    assert_eq!(pre.args, vec![b"/srv/repos/x".to_vec(), b"41-1a".to_vec()]);
    assert_eq!(
      pre.stdin,
      b"LOCK-TOKENS:\n/trunk/a%20b.c|opaquelocktoken:1\n/trunk/c.c|opaquelocktoken:2\n\n".to_vec()
    );
    assert_eq!(
      svn_pre_commit(PendingView(1), "/srv/repos/x", "41-1a", &[]).stdin,
      b"LOCK-TOKENS:\n\n".to_vec()
    );
    let modified = svn_pre_revprop_change(
      CommittedView(2),
      "/srv/repos/x",
      40,
      "ada",
      "svn:log",
      RevpropAction::Modified,
      b"a better message",
    );
    assert_eq!(
      modified.args,
      vec![
        b"/srv/repos/x".to_vec(),
        b"40".to_vec(),
        b"ada".to_vec(),
        b"svn:log".to_vec(),
        b"M".to_vec(),
      ]
    );
    assert_eq!(modified.stdin, b"a better message".to_vec());
    let deleted = svn_pre_revprop_change(
      CommittedView(2),
      "/srv/repos/x",
      40,
      "",
      "svn:log",
      RevpropAction::Deleted,
      b"",
    );
    assert_eq!(deleted.args.get(2), Some(&Vec::new()));
    assert_eq!(deleted.args.get(4), Some(&b"D".to_vec()));
    assert!(deleted.stdin.is_empty());
    let added = svn_pre_revprop_change(
      CommittedView(2),
      "/srv/repos/x",
      40,
      "ada",
      "ci:status",
      RevpropAction::Added,
      b"green",
    );
    assert_eq!(added.args.get(4), Some(&b"A".to_vec()));
    assert_eq!(added.stdin, b"green".to_vec());
    let post = svn_post_commit(CommittedView(3), "/srv/repos/x", 41, "40-1a");
    assert_eq!(
      post.args,
      vec![b"/srv/repos/x".to_vec(), b"41".to_vec(), b"40-1a".to_vec()]
    );
  }

  /// A-597 (§4.17.1: "Before a hook has run, its deadline is its push's connection ... and that run is the first
  /// sample"). Do: run a hook with nothing learned; it writes, ends its output and exits 0. Expect: no timer at any
  /// step, accepted with its words, and the hook now holds a learned bound past the run's own length.
  #[test]
  fn a_hooks_first_run_has_no_deadline_but_its_client_and_becomes_its_first_sample() {
    let mut times = HookTimes::default();
    assert_eq!(times.bound_ns(), None);
    let (mut run, step) = HookRun::started(START_NS, &times, CAP);
    assert_eq!(step, Step::Wait { wake_at_ns: None });
    assert_eq!(
      run.on(Event::Output(b"checked\n"), &mut times),
      Step::Wait { wake_at_ns: None }
    );
    assert_eq!(
      run.on(Event::OutputEnded, &mut times),
      Step::Wait { wake_at_ns: None }
    );
    let ran_ns = 5 * LIVENESS_BUDGET_NS;
    assert_eq!(
      run.on(
        Event::Exited {
          status: ExitStatus::Code(0),
          at_ns: START_NS + ran_ns,
        },
        &mut times
      ),
      Step::Done {
        outcome: Outcome::Accepted {
          words: words(b"checked\n")
        },
        end_leftovers: false,
      }
    );
    assert!(
      times.bound_ns().is_some_and(|bound| bound > ran_ns),
      "a first run five windows long is waited for past its length next time: {:?}",
      times.bound_ns()
    );
  }

  /// A-597 (the learned deadline, RFC 6298 with Karn's rule and the back-off). Do: after eight quick runs, run the hook
  /// again and let the timer fire just before, then at, its deadline; it writes while being killed, then exits by the
  /// kill. Then run it once more and let it finish. Expect: the timer is armed at the start plus the learned bound;
  /// nothing happens before it; at it the hook is killed and timed out naming the bound, with its words; the next
  /// bound is doubled; and a run that finishes ends the back-off.
  #[test]
  fn a_run_past_its_learned_bound_is_killed_timed_out_and_the_bound_backs_off() {
    let mut times = learned();
    let bound = times.bound_ns().expect("learned");
    assert!(
      bound > LIVENESS_BUDGET_NS + QUICK_NS && bound < LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 10,
      "quick runs put the bound past their own length and a liveness window, by little: {bound} ns"
    );
    let (mut run, step) = HookRun::started(START_NS, &times, CAP);
    let deadline = START_NS + bound;
    assert_eq!(
      step,
      Step::Wait {
        wake_at_ns: Some(deadline)
      }
    );
    assert_eq!(
      run.on(
        Event::Timer {
          now_ns: deadline - 1
        },
        &mut times
      ),
      Step::Wait {
        wake_at_ns: Some(deadline)
      }
    );
    assert_eq!(
      run.on(Event::Timer { now_ns: deadline }, &mut times),
      Step::Kill
    );
    run.on(Event::Output(b"still checking"), &mut times);
    assert_eq!(
      run.on(
        Event::Exited {
          status: ExitStatus::Signal(9),
          at_ns: deadline + 1,
        },
        &mut times
      ),
      Step::Done {
        outcome: Outcome::TimedOut {
          bound_ns: bound,
          words: words(b"still checking"),
        },
        end_leftovers: false,
      }
    );
    assert_eq!(
      times.bound_ns(),
      Some(2 * bound),
      "a run cut at its bound doubles the next"
    );
    assert_eq!((times.finished_runs(), times.cut_runs()), (8, 1));
    let (mut run, _) = HookRun::started(START_NS, &times, CAP);
    run.on(Event::OutputEnded, &mut times);
    run.on(
      Event::Exited {
        status: ExitStatus::Code(0),
        at_ns: START_NS + QUICK_NS,
      },
      &mut times,
    );
    assert!(
      times.bound_ns().is_some_and(|after| after < 2 * bound),
      "a finished run ends the back-off: {:?}",
      times.bound_ns()
    );
  }

  /// A-597 (a refusing hook). Do: run a hook that writes and exits 1, and one ended by a signal the runner did not send.
  /// Expect: each refused with its exit and its words, and each run a sample of its hook (it finished on its own).
  #[test]
  fn a_hook_that_exits_non_zero_or_dies_refuses_with_its_words() {
    for status in [ExitStatus::Code(1), ExitStatus::Signal(11)] {
      let mut times = HookTimes::default();
      let (mut run, _) = HookRun::started(START_NS, &times, CAP);
      run.on(Event::Output(b"refs/heads/main is protected\n"), &mut times);
      run.on(Event::OutputEnded, &mut times);
      assert_eq!(
        run.on(
          Event::Exited {
            status,
            at_ns: START_NS + QUICK_NS,
          },
          &mut times
        ),
        Step::Done {
          outcome: Outcome::Refused {
            status,
            words: words(b"refs/heads/main is protected\n"),
          },
          end_leftovers: false,
        }
      );
      assert!(times.bound_ns().is_some());
    }
  }

  /// A-597 (§4.17.1: "the run ends when the client that pushed leaves"; Karn's rule). Do: with a learned bound, let the
  /// client leave mid-run, then the kill's exit arrive; and on a first run, the same. Expect: the hook is killed and
  /// the run abandoned; the learned bound is what it was (no sample, no back-off) and the first run teaches nothing.
  #[test]
  fn a_client_that_leaves_ends_the_run_and_teaches_its_hook_nothing() {
    let mut times = learned();
    let before = times.bound_ns();
    let (mut run, _) = HookRun::started(START_NS, &times, CAP);
    assert_eq!(run.on(Event::ClientLeft, &mut times), Step::Kill);
    assert_eq!(
      run.on(
        Event::Exited {
          status: ExitStatus::Signal(9),
          at_ns: START_NS + 4 * QUICK_NS,
        },
        &mut times
      ),
      Step::Done {
        outcome: Outcome::Abandoned,
        end_leftovers: false,
      }
    );
    assert_eq!(times.bound_ns(), before);
    assert_eq!((times.finished_runs(), times.cut_runs()), (8, 0));
    let mut first = HookTimes::default();
    let (mut run, _) = HookRun::started(START_NS, &first, CAP);
    run.on(Event::ClientLeft, &mut first);
    run.on(
      Event::Exited {
        status: ExitStatus::Signal(9),
        at_ns: START_NS + QUICK_NS,
      },
      &mut first,
    );
    assert_eq!(
      first.bound_ns(),
      None,
      "a run its client cut short is no sample"
    );
  }

  /// A-597 (bounded words). Do: with a cap of 64 bytes, let a hook write 40 bytes, then 40 more, then exit 1. Expect:
  /// the first 64 bytes kept in order and the other 16 counted as dropped.
  #[test]
  fn a_hooks_words_are_kept_to_their_cap_and_the_rest_counted() {
    let mut times = HookTimes::default();
    let (mut run, _) = HookRun::started(START_NS, &times, CAP);
    run.on(Event::Output(&[b'a'; 40]), &mut times);
    run.on(Event::Output(&[b'b'; 40]), &mut times);
    run.on(Event::OutputEnded, &mut times);
    let Step::Done {
      outcome: Outcome::Refused { words, .. },
      ..
    } = run.on(
      Event::Exited {
        status: ExitStatus::Code(1),
        at_ns: START_NS + QUICK_NS,
      },
      &mut times,
    )
    else {
      panic!("refused");
    };
    let mut expected = vec![b'a'; 40];
    expected.extend_from_slice(&[b'b'; 24]);
    assert_eq!(words.kept, expected);
    assert_eq!(words.dropped, 16);
  }

  /// A-597 (a hook that leaves a process holding its output: git's and Subversion's servers would wait on that pipe for
  /// ever). Do: with a learned bound, let the hook exit 0 while its output stays open, write more, and let the timer
  /// reach the deadline; and in another run let the client leave instead. Expect: its exit is its verdict, accepted
  /// with every word up to the deadline and the leftovers ended; the run was a sample at its exit; and a client that
  /// leaves while it drains abandons it, leftovers ended.
  #[test]
  fn a_hook_whose_children_hold_its_output_is_judged_by_its_exit_and_its_leftovers_ended() {
    let mut times = learned();
    let bound = times.bound_ns().expect("learned");
    let (mut run, _) = HookRun::started(START_NS, &times, CAP);
    run.on(Event::Output(b"ok "), &mut times);
    let exit = Event::Exited {
      status: ExitStatus::Code(0),
      at_ns: START_NS + QUICK_NS,
    };
    assert_eq!(
      run.on(exit, &mut times),
      Step::Wait {
        wake_at_ns: Some(START_NS + bound)
      }
    );
    run.on(Event::Output(b"daemon started"), &mut times);
    assert_eq!(
      run.on(
        Event::Timer {
          now_ns: START_NS + bound
        },
        &mut times
      ),
      Step::Done {
        outcome: Outcome::Accepted {
          words: words(b"ok daemon started"),
        },
        end_leftovers: true,
      }
    );
    assert_eq!(
      (times.finished_runs(), times.cut_runs()),
      (9, 0),
      "its exit was a sample; the leftovers' end was no cut"
    );
    let (mut run, _) = HookRun::started(START_NS, &times, CAP);
    run.on(exit, &mut times);
    assert_eq!(
      run.on(Event::ClientLeft, &mut times),
      Step::Done {
        outcome: Outcome::Abandoned,
        end_leftovers: true,
      }
    );
  }

  /// A-597. Do: report a hook's program as not found, not permitted, short of room, and failing with another error.
  /// Expect: each unstartable by its own cause, and no run, so nothing taught.
  #[test]
  fn a_hook_that_cannot_start_is_unstartable_by_its_cause() {
    for cause in [
      StartRefused::NotFound,
      StartRefused::NotPermitted,
      StartRefused::NoRoom,
      StartRefused::Other(5),
    ] {
      assert_eq!(HookRun::unstartable(cause), Outcome::Unstartable { cause });
    }
  }

  /// One generated event, its times as offsets from the run's start.
  #[derive(Clone, Debug)]
  enum Generated {
    /// Output of this many bytes.
    Output(usize),
    /// The output ended.
    OutputEnded,
    /// The process exited 0 this long after its start.
    Exited(u64),
    /// The timer fired this long after the start.
    Timer(u64),
    /// The client left.
    ClientLeft,
  }

  /// A generated event, its offsets spread over a few liveness windows, around a learned bound.
  fn generated() -> impl Strategy<Value = Generated> {
    let span = 4 * LIVENESS_BUDGET_NS;
    prop_oneof![
      (1_usize..40).prop_map(Generated::Output),
      Just(Generated::OutputEnded),
      (0..span).prop_map(Generated::Exited),
      (0..span).prop_map(Generated::Timer),
      Just(Generated::ClientLeft),
    ]
  }

  /// What first took a run out of running: its exit, the timer at its deadline, or its client leaving.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum Decisive {
    /// The process exited on its own.
    Exited,
    /// The timer reached the deadline.
    Timer,
    /// The client left.
    Client,
  }

  /// A run being driven by a test, as a driver would: each exit reported once, the clock never going back.
  struct Driven {
    /// The run.
    run: HookRun,
    /// Its hook's times.
    times: HookTimes,
    /// The deadline the run was started with.
    deadline: Option<u64>,
    /// Whether the process has exited.
    exited: bool,
    /// The clock, as an offset from the start.
    clock: u64,
    /// The bytes written before the run ended.
    written: usize,
    /// What first took the run out of running.
    decisive: Option<Decisive>,
    /// Every step the run answered.
    steps: Vec<Step>,
  }

  impl Driven {
    /// Reports `event`, recording the step, and the bytes written while the run is not over.
    fn report(&mut self, event: Event<'_>) {
      let over = self
        .steps
        .iter()
        .any(|step| matches!(step, Step::Done { .. }));
      if let (Event::Output(bytes), false) = (event, over) {
        self.written += bytes.len();
      }
      let step = self.run.on(event, &mut self.times);
      self.steps.push(step);
    }

    /// Reports one generated event, noting what first takes the run out of running.
    fn generated(&mut self, event: &Generated) {
      match *event {
        Generated::Output(bytes) => self.report(Event::Output(&vec![b'w'; bytes])),
        Generated::OutputEnded => self.report(Event::OutputEnded),
        Generated::Exited(_) if self.exited => {}
        Generated::Exited(offset) => {
          self.exited = true;
          self.clock = self.clock.max(offset);
          self.decisive.get_or_insert(Decisive::Exited);
          self.report(Event::Exited {
            status: ExitStatus::Code(0),
            at_ns: START_NS + self.clock,
          });
        }
        Generated::Timer(offset) => {
          self.clock = self.clock.max(offset);
          let now_ns = START_NS + self.clock;
          if self.deadline.is_some_and(|deadline| now_ns >= deadline) {
            self.decisive.get_or_insert(Decisive::Timer);
          }
          self.report(Event::Timer { now_ns });
        }
        Generated::ClientLeft => {
          self.decisive.get_or_insert(Decisive::Client);
          self.report(Event::ClientLeft);
        }
      }
    }

    /// Ends the run as a driver would at the last: the client leaves, the kill's exit arrives, the output ends.
    fn end(&mut self) {
      self.decisive.get_or_insert(Decisive::Client);
      self.report(Event::ClientLeft);
      if !self.exited {
        self.exited = true;
        self.report(Event::Exited {
          status: ExitStatus::Signal(9),
          at_ns: START_NS + self.clock,
        });
      }
      self.report(Event::OutputEnded);
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(if cfg!(miri) { 8 } else { 1024 }),
      ..ProptestConfig::default()
    }))]

    /// A-597 (the runner's invariants, over any order a driver reports in). Do: drive runs, with nothing learned and
    /// with a learned bound, through generated events (each exit reported once, the clock never going back), then end
    /// each by its client leaving, the kill's exit and the output's end. Expect: a run ends exactly once and answers
    /// nothing after; its hook gains a sample exactly when the process exited while the run was running, a back-off
    /// exactly when the timer reached the deadline first, and neither when the client left first (Karn's rule); and its
    /// kept words are the first bytes written, up to the cap, with the rest counted as dropped.
    #[test]
    fn a_run_ends_once_learns_only_from_how_it_ended_and_keeps_its_first_words(
      events in prop::collection::vec(generated(), 0..12),
      was_learned in any::<bool>(),
    ) {
      let times = if was_learned { learned() } else { HookTimes::default() };
      let (finished, cut) = (times.finished_runs(), times.cut_runs());
      let (run, _) = HookRun::started(START_NS, &times, CAP);
      let deadline = times.bound_ns().map(|bound| START_NS + bound);
      let mut driven = Driven {
        run,
        times,
        deadline,
        exited: false,
        clock: 0,
        written: 0,
        decisive: None,
        steps: Vec::new(),
      };
      for event in &events {
        driven.generated(event);
      }
      driven.end();
      let mut outcomes = Vec::new();
      for step in &driven.steps {
        prop_assert!(
          outcomes.is_empty() || *step == Step::Wait { wake_at_ns: None },
          "nothing after the end: {:?}",
          driven.steps
        );
        if let Step::Done { outcome, .. } = step {
          outcomes.push(outcome.clone());
        }
      }
      prop_assert_eq!(outcomes.len(), 1, "a run ends exactly once: {:?}", driven.steps);
      let sampled = u64::from(driven.decisive == Some(Decisive::Exited));
      let backed_off = u64::from(driven.decisive == Some(Decisive::Timer));
      prop_assert_eq!(driven.times.finished_runs(), finished + sampled);
      prop_assert_eq!(driven.times.cut_runs(), cut + backed_off);
      if let Some(Outcome::Accepted { words } | Outcome::Refused { words, .. } | Outcome::TimedOut { words, .. }) =
        outcomes.first()
      {
        prop_assert_eq!(words.kept.len(), driven.written.min(CAP));
        prop_assert!(words.kept.iter().all(|&byte| byte == b'w'));
        prop_assert_eq!(words.kept.len() as u64 + words.dropped, driven.written as u64);
      }
    }
  }
}
