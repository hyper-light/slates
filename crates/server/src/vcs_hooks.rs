//! The hooks' protocol (§4.17.1 "Policy: protected branches, access and hooks"; shared piece 7, A-597): which hook
//! runs at which point and over what, what it is given, what it decides, how long it may take, and the runner that
//! holds one run to all of that.
//!
//! **The points** ([`HookPoint`]). Admission hooks judge a change before the verdict and may refuse it with words the
//! client prints: git's `pre-receive` (once a push, every command on its standard input; non-zero refuses every ref)
//! and `update` (once a ref, three arguments; non-zero refuses that ref) [B: `githooks.adoc` at git e1ec09b23];
//! Mercurial's `pretxnchangegroup` ("A non-zero status will cause the transaction to be rolled back") [B: `config.txt`
//! "hooks", Mercurial 7.2.4]; Subversion's `start-commit`, `pre-commit`, `pre-revprop-change`, `pre-lock` and
//! `pre-unlock` [C: `libsvn_repos/hooks.c` at fd4e82b00 and 1.15.0's e49d66ef]. Observing hooks run after the change
//! and change no answer: git's `post-receive` ("does not affect the outcome"), Mercurial's `changegroup`, Subversion's
//! `post-commit`, `post-revprop-change`, `post-lock` and `post-unlock`. An admission point with no hook lets the change
//! through, except Subversion's `pre-revprop-change`, which then refuses every revision-property change ("default to
//! MASSIVE PARANOIA", `hooks.c`). Several hooks at one point run in the order the tool's configuration gives
//! (Mercurial's `priority.` prefix runs a higher priority first, `config.txt` "hooks"), and at an admission point the
//! first that does not accept decides and the rest do not run, as each tool stops at its first refusal.
//!
//! **What a hook runs over.** A read-only attachment of the change, never the green itself: an admission hook sees the
//! work's post-state ([`PendingView`]: git's quarantine, Mercurial's pending transaction named by `HG_PENDING`,
//! Subversion's transaction, which a hook reads with the stock `svnlook -t`: slates ships no `svnlook`, A-623), an
//! observing hook the version the commit made ([`CommittedView`]), and Subversion's revision-property and lock hooks
//! the revision they act on. Each tool's server makes that view (git's bare-repository view, Mercurial's revlog view,
//! Subversion's repository view, their own pieces); this module names it.
//!
//! **What a hook is given** ([`HookInput`]), in each tool's own terms: git's `<old-oid> SP <new-oid> SP <ref-name> LF`
//! lines and its push options as `GIT_PUSH_OPTION_<i>` and `GIT_PUSH_OPTION_COUNT`, unset when none were negotiated
//! (`builtin/receive-pack.c` `run_receive_hook`); Mercurial's `HG_*` environment (`HG_HOOKTYPE`, `HG_HOOKNAME`,
//! `HG_NODE`, `HG_NODE_LAST`, `HG_SOURCE`, `HG_URL`, and `HG_PENDING` for a hook inside the transaction); Subversion's
//! arguments and standard input as `hooks.c` builds them (`LOCK-TOKENS:` and a `path|token` line a token, then a blank
//! line; a property's new or old value; the paths a lock hook acted on), each argument a C string that ends at its first
//! NUL, and as its whole environment its own hooks-env section, else the default section, else nothing, never the
//! server's (A-623, checked against stock `svnserve` 1.15's runs). A hook's words are what it writes: git and Mercurial
//! take both of its streams (git joins a hook's standard output to its standard error, `run_hooks_opt`), Subversion
//! standard error alone (it sends standard output to the null device, `run_hook_cmd`, but for `pre-lock`'s, which is
//! the lock's token).
//!
//! **What Subversion's client is told** ([`svn_told`]): `svnserve`'s own words for each outcome, checked byte for byte
//! against stock 1.15's on the wire (A-623). `check_hook_result`'s "Commit blocked by pre-commit hook (exit code 1)",
//! then " with output:" and the hook's standard error, or " with no output."; for a signal, "did not exit cleanly";
//! post-commit's failure as `commit-info`'s `post-commit-err`, the commit standing; post-lock's and post-unlock's
//! under the error saying the change was made. A hook slates killed past its bound is told as `svnserve` tells of a
//! hook a signal ended, from the status it was reaped with: `svnserve` has no deadline, so its words have no place for
//! the bound. A program that could not be run is told as `svnserve`'s failed `exec`: exit code 255, no output.
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
  /// Subversion's `start-commit`.
  SvnStartCommit,
  /// Subversion's `pre-commit`.
  SvnPreCommit,
  /// Subversion's `pre-revprop-change`.
  SvnPreRevpropChange,
  /// Subversion's `post-commit`.
  SvnPostCommit,
  /// Subversion's `post-revprop-change`.
  SvnPostRevpropChange,
  /// Subversion's `pre-lock`.
  SvnPreLock,
  /// Subversion's `post-lock`.
  SvnPostLock,
  /// Subversion's `pre-unlock`.
  SvnPreUnlock,
  /// Subversion's `post-unlock`.
  SvnPostUnlock,
}

impl HookPoint {
  /// When it runs.
  pub(crate) fn phase(self) -> Phase {
    match self {
      Self::GitPreReceive
      | Self::GitUpdate
      | Self::HgPretxnchangegroup
      | Self::SvnStartCommit
      | Self::SvnPreCommit
      | Self::SvnPreRevpropChange
      | Self::SvnPreLock
      | Self::SvnPreUnlock => Phase::Admission,
      Self::GitPostReceive
      | Self::HgChangegroup
      | Self::SvnPostCommit
      | Self::SvnPostRevpropChange
      | Self::SvnPostLock
      | Self::SvnPostUnlock => Phase::Observing,
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
      Self::SvnStartCommit => "start-commit",
      Self::SvnPreCommit => "pre-commit",
      Self::SvnPreRevpropChange => "pre-revprop-change",
      Self::SvnPostCommit => "post-commit",
      Self::SvnPostRevpropChange => "post-revprop-change",
      Self::SvnPreLock => "pre-lock",
      Self::SvnPostLock => "post-lock",
      Self::SvnPreUnlock => "pre-unlock",
      Self::SvnPostUnlock => "post-unlock",
    }
  }

  /// The streams its words come from.
  pub(crate) fn words_from(self) -> WordsFrom {
    match self {
      Self::SvnStartCommit
      | Self::SvnPreCommit
      | Self::SvnPreRevpropChange
      | Self::SvnPostCommit
      | Self::SvnPostRevpropChange
      | Self::SvnPreLock
      | Self::SvnPostLock
      | Self::SvnPreUnlock
      | Self::SvnPostUnlock => WordsFrom::ErrorStream,
      _ => WordsFrom::BothStreams,
    }
  }

  /// Whether the hook's standard output is read rather than sent to the null device: Subversion's `pre-lock` alone,
  /// whose output is the lock's token, or the server's own when it is empty (`svn_repos__hooks_pre_lock` passes
  /// `run_hook_cmd` a result, `hooks.c`).
  pub(crate) fn output_is_lock_token(self) -> bool {
    matches!(self, Self::SvnPreLock)
  }

  /// Whether it is one of Subversion's points.
  pub(crate) fn is_subversions(self) -> bool {
    matches!(
      self,
      Self::SvnStartCommit
        | Self::SvnPreCommit
        | Self::SvnPreRevpropChange
        | Self::SvnPostCommit
        | Self::SvnPostRevpropChange
        | Self::SvnPreLock
        | Self::SvnPostLock
        | Self::SvnPreUnlock
        | Self::SvnPostUnlock
    )
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
  /// Its environment's changes, in order; or, when `clean_environment`, its whole environment.
  pub(crate) env: Vec<(String, EnvValue)>,
  /// Whether the hook starts with `env` alone and nothing of the server's environment: Subversion's hooks, which
  /// `run_hook_cmd` starts with exactly their hooks-env section (`svn_io_start_cmd3` with `inherit` false, an empty
  /// environment when the section is absent). git's and Mercurial's hooks inherit the server's.
  pub(crate) clean_environment: bool,
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
    clean_environment: false,
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
    clean_environment: false,
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
    clean_environment: false,
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
    clean_environment: false,
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
    clean_environment: false,
    view: View::Committed(view),
  }
}

/// One lock token a Subversion commit presents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LockToken {
  /// The path as the server handed it to the filesystem with its token: over `svn://`, the client's own path, relative
  /// to its commit's base (`serve.c` `add_lock_tokens` gives `svn_fs_access_add_lock_token2` the path as sent, and the
  /// recorded runs show `b.txt` for `/trunk/with space/b.txt`). [`svn_pre_commit`] escapes it.
  pub(crate) path: String,
  /// The token.
  pub(crate) token: String,
}

/// A repository's hook environment: the sections of its `hooks-env` file (`svnserve.conf`'s `hooks-env`, by default
/// `conf/hooks-env`), each a section's variables, read by the server from the file as Subversion's configuration
/// format reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SvnHooksEnv {
  sections: Vec<SvnHooksEnvSection>,
}

/// A variable of a hooks-env section: its name and its value.
pub(crate) type SvnHookVariable = (String, Vec<u8>);

/// A section of a hooks-env file: its name (a hook's, or `default`) and its variables, in file order.
pub(crate) type SvnHooksEnvSection = (String, Vec<SvnHookVariable>);

/// Format: the section that names every hook's environment unless the hook has its own
/// (`SVN_REPOS__HOOKS_ENV_DEFAULT_SECTION`, `repos.h`).
const HOOKS_ENV_DEFAULT_SECTION: &str = "default";

impl SvnHooksEnv {
  /// The environment of the file whose sections are `sections`, each with its variables in file order, as Subversion's
  /// configuration reads them: a section named twice is one section, a variable set twice keeps its last value, and a
  /// section with no variables is no section, so its hook takes the default one (`parse_hooks_env_option` makes a
  /// section's table at its first variable, `hooks.c`; the recorded post-commit, under an empty section of its own, got
  /// the default section).
  pub(crate) fn new(sections: Vec<SvnHooksEnvSection>) -> Self {
    let mut merged: Vec<SvnHooksEnvSection> = Vec::new();
    for (name, variables) in sections {
      if !merged.iter().any(|(held, _)| *held == name) {
        merged.push((name.clone(), Vec::new()));
      }
      let Some((_, kept)) = merged.iter_mut().find(|(held, _)| *held == name) else {
        continue;
      };
      for (variable, value) in variables {
        match kept.iter_mut().find(|(held, _)| *held == variable) {
          Some(slot) => slot.1 = value,
          None => kept.push((variable, value)),
        }
      }
    }
    merged.retain(|(_, variables)| !variables.is_empty());
    Self { sections: merged }
  }

  /// The whole environment `point`'s hook runs with: its own section if the file has one, else the default section,
  /// else nothing (`run_hook_cmd`, `hooks.c`): a hook's own section replaces the default section, it adds nothing to
  /// it, and nothing of the server's environment is passed.
  pub(crate) fn for_point(&self, point: HookPoint) -> Vec<(String, EnvValue)> {
    let section = |name: &str| self.sections.iter().find(|(held, _)| held == name);
    section(point.name())
      .or_else(|| section(HOOKS_ENV_DEFAULT_SECTION))
      .map(|(_, variables)| {
        variables
          .iter()
          .map(|(variable, value)| (variable.clone(), EnvValue::Bytes(value.clone())))
          .collect()
      })
      .unwrap_or_default()
  }
}

/// Format: the first line of `pre-commit`'s standard input when the commit presents lock tokens
/// (`lock_token_content`, `hooks.c`).
const LOCK_TOKENS_HEADER: &str = "LOCK-TOKENS:\n";

/// Format: Subversion's flag argument for a stolen lock or a broken one (`"1"` or `"0"`, `hooks.c`).
fn flag(set: bool) -> Vec<u8> {
  if set { b"1".to_vec() } else { b"0".to_vec() }
}

/// A string as Subversion's C code hands it on: up to its first NUL. `svnserve` reads every name, path, comment and
/// token off the wire as a C string (`vparse_tuple`'s `c`, `marshal.c`), and a hook's argument is one too, so a NUL a
/// client sent ends the string there.
fn c_string(text: &str) -> &[u8] {
  let bytes = text.as_bytes();
  bytes.split(|byte| *byte == 0).next().unwrap_or(bytes)
}

/// Format: the bytes `svn_path_uri_autoescape` escapes, the zeros of `path.c`'s `uri_autoescape_chars` but NUL, which
/// ends a C string: space, `"`, `<`, `>`, `\`, `^`, `` ` ``, `{`, `|` and `}`. Every other byte is kept as it is, `%`,
/// the control bytes and every byte past ASCII among them.
const SVN_AUTOESCAPED: &[u8] = b" \"<>\\^`{|}";

/// `path` as `svn_path_uri_autoescape` writes it: each byte of [`SVN_AUTOESCAPED`] as `%` and two capital hex digits
/// (`uri_escape`'s `"%%%02X"`, `path.c`).
fn svn_uri_autoescape(path: &[u8]) -> Vec<u8> {
  let mut escaped = Vec::with_capacity(path.len());
  for byte in path {
    if SVN_AUTOESCAPED.contains(byte) {
      escaped.extend_from_slice(format!("%{byte:02X}").as_bytes());
    } else {
      escaped.push(*byte);
    }
  }
  escaped
}

/// A Subversion hook's input: its arguments after the program, its standard input, and its whole environment from
/// `hooks_env`.
fn svn_input(
  point: HookPoint,
  hooks_env: &SvnHooksEnv,
  args: Vec<Vec<u8>>,
  stdin: Vec<u8>,
  view: View,
) -> HookInput {
  HookInput {
    point,
    args,
    env: hooks_env.for_point(point),
    clean_environment: true,
    stdin,
    view,
  }
}

/// Paths a line each, every line ended (`svn_cstring_join2` with a trailing separator, `hooks.c`).
fn path_lines(paths: &[&str]) -> Vec<u8> {
  paths
    .iter()
    .flat_map(|path| c_string(path).iter().copied().chain(std::iter::once(b'\n')))
    .collect()
}

/// Subversion's `start-commit`: the repository's directory, the user (empty for none), the client's capabilities
/// joined by `:`, and the transaction's name; no standard input of its own (`svn_repos__hooks_start_commit`).
pub(crate) fn svn_start_commit(
  view: PendingView,
  hooks_env: &SvnHooksEnv,
  repository: &str,
  user: &str,
  capabilities: &[&str],
  transaction: &str,
) -> HookInput {
  let capabilities: Vec<&[u8]> = capabilities.iter().map(|word| c_string(word)).collect();
  let args = vec![
    c_string(repository).to_vec(),
    c_string(user).to_vec(),
    capabilities.join(&b':'),
    c_string(transaction).to_vec(),
  ];
  svn_input(
    HookPoint::SvnStartCommit,
    hooks_env,
    args,
    Vec::new(),
    View::Pending(view),
  )
}

/// Subversion's `pre-commit`: the repository's directory and the transaction's name as arguments. With lock tokens,
/// standard input is `LOCK-TOKENS:`, a `path|token` line a token, its path escaped by [`svn_uri_autoescape`] (so a `|`
/// in it is `%7C`), and a blank line (`lock_token_content`); without any, it is the null device's nothing
/// (`svn_repos__hooks_pre_commit` gives `lock_token_content` only a non-empty set). The lines come in the order given;
/// `svnserve` writes them in its hash table's order, which differs from run to run.
pub(crate) fn svn_pre_commit(
  view: PendingView,
  hooks_env: &SvnHooksEnv,
  repository: &str,
  transaction: &str,
  tokens: &[LockToken],
) -> HookInput {
  let mut stdin = Vec::new();
  if !tokens.is_empty() {
    stdin.extend_from_slice(LOCK_TOKENS_HEADER.as_bytes());
    for token in tokens {
      stdin.extend_from_slice(&svn_uri_autoescape(c_string(&token.path)));
      stdin.push(b'|');
      stdin.extend_from_slice(c_string(&token.token));
      stdin.push(b'\n');
    }
    stdin.push(b'\n');
  }
  let args = vec![
    c_string(repository).to_vec(),
    c_string(transaction).to_vec(),
  ];
  svn_input(
    HookPoint::SvnPreCommit,
    hooks_env,
    args,
    stdin,
    View::Pending(view),
  )
}

/// How a revision property would change: added, modified or deleted (the revision-property hooks' action letter).
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

/// A revision-property change as its hooks are told of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RevpropChange<'a> {
  /// The repository's directory.
  pub(crate) repository: &'a str,
  /// The revision.
  pub(crate) revision: u64,
  /// The user making the change (empty for none).
  pub(crate) author: &'a str,
  /// The property's name.
  pub(crate) property: &'a str,
  /// What happens to it.
  pub(crate) action: RevpropAction,
}

/// The revision-property hooks' arguments: the repository's directory, the revision, the author, the property's name
/// and the action's letter.
fn revprop_args(change: RevpropChange<'_>) -> Vec<Vec<u8>> {
  vec![
    c_string(change.repository).to_vec(),
    change.revision.to_string().into_bytes(),
    c_string(change.author).to_vec(),
    c_string(change.property).to_vec(),
    change.action.letter().to_vec(),
  ]
}

/// Subversion's `pre-revprop-change`: [`revprop_args`]; the new value on standard input, nothing for a deletion.
pub(crate) fn svn_pre_revprop_change(
  view: CommittedView,
  hooks_env: &SvnHooksEnv,
  change: RevpropChange<'_>,
  new_value: &[u8],
) -> HookInput {
  let args = revprop_args(change);
  svn_input(
    HookPoint::SvnPreRevpropChange,
    hooks_env,
    args,
    new_value.to_vec(),
    View::Committed(view),
  )
}

/// Subversion's `post-revprop-change`: [`revprop_args`]; the old value on standard input, nothing for an addition.
pub(crate) fn svn_post_revprop_change(
  view: CommittedView,
  hooks_env: &SvnHooksEnv,
  change: RevpropChange<'_>,
  old_value: &[u8],
) -> HookInput {
  let args = revprop_args(change);
  svn_input(
    HookPoint::SvnPostRevpropChange,
    hooks_env,
    args,
    old_value.to_vec(),
    View::Committed(view),
  )
}

/// Subversion's `post-commit`: the repository's directory, the new revision and the transaction's name as arguments.
pub(crate) fn svn_post_commit(
  view: CommittedView,
  hooks_env: &SvnHooksEnv,
  repository: &str,
  revision: u64,
  transaction: &str,
) -> HookInput {
  let args = vec![
    c_string(repository).to_vec(),
    revision.to_string().into_bytes(),
    c_string(transaction).to_vec(),
  ];
  svn_input(
    HookPoint::SvnPostCommit,
    hooks_env,
    args,
    Vec::new(),
    View::Committed(view),
  )
}

/// Subversion's `pre-lock`: the repository's directory, the path, the user, the comment (empty for none) and whether
/// the lock is stolen (`1` or `0`); no standard input of its own. Its standard output is the lock's token
/// ([`HookPoint::output_is_lock_token`]).
pub(crate) fn svn_pre_lock(
  view: CommittedView,
  hooks_env: &SvnHooksEnv,
  repository: &str,
  path: &str,
  user: &str,
  comment: &str,
  steal: bool,
) -> HookInput {
  let args = vec![
    c_string(repository).to_vec(),
    c_string(path).to_vec(),
    c_string(user).to_vec(),
    c_string(comment).to_vec(),
    flag(steal),
  ];
  svn_input(
    HookPoint::SvnPreLock,
    hooks_env,
    args,
    Vec::new(),
    View::Committed(view),
  )
}

/// Subversion's `post-lock`: the repository's directory and the user as arguments; the locked paths on standard input,
/// a line each.
pub(crate) fn svn_post_lock(
  view: CommittedView,
  hooks_env: &SvnHooksEnv,
  repository: &str,
  user: &str,
  paths: &[&str],
) -> HookInput {
  let args = vec![c_string(repository).to_vec(), c_string(user).to_vec()];
  svn_input(
    HookPoint::SvnPostLock,
    hooks_env,
    args,
    path_lines(paths),
    View::Committed(view),
  )
}

/// Subversion's `pre-unlock`: the repository's directory, the path, the user (empty for none), the token presented
/// (empty for none, as when a lock is broken) and whether the lock is broken (`1` or `0`); no standard input of its
/// own.
pub(crate) fn svn_pre_unlock(
  view: CommittedView,
  hooks_env: &SvnHooksEnv,
  repository: &str,
  path: &str,
  user: &str,
  token: &str,
  break_lock: bool,
) -> HookInput {
  let args = vec![
    c_string(repository).to_vec(),
    c_string(path).to_vec(),
    c_string(user).to_vec(),
    c_string(token).to_vec(),
    flag(break_lock),
  ];
  svn_input(
    HookPoint::SvnPreUnlock,
    hooks_env,
    args,
    Vec::new(),
    View::Committed(view),
  )
}

/// Subversion's `post-unlock`: the repository's directory and the user (empty for none) as arguments; the unlocked
/// paths on standard input, a line each.
pub(crate) fn svn_post_unlock(
  view: CommittedView,
  hooks_env: &SvnHooksEnv,
  repository: &str,
  user: &str,
  paths: &[&str],
) -> HookInput {
  let args = vec![c_string(repository).to_vec(), c_string(user).to_vec()];
  svn_input(
    HookPoint::SvnPostUnlock,
    hooks_env,
    args,
    path_lines(paths),
    View::Committed(view),
  )
}

/// `SVN_ERR_REPOS_HOOK_FAILURE`, "A repository hook failed": every hook failure `check_hook_result` words.
/// Format: the repository category's start, 120,000 + 9 × 5,000 = 165,000, plus 1 (`svn_error_codes.h` 123-124,
/// 928-930).
const SVN_ERR_REPOS_HOOK_FAILURE: u32 = 165_001;

/// `SVN_ERR_REPOS_BAD_ARGS`: a hook its server could not start (`run_hook_cmd`'s "Failed to start '%s' hook").
/// Format: the repository category's start plus 2 (`svn_error_codes.h` 932-934).
const SVN_ERR_REPOS_BAD_ARGS: u32 = 165_002;

/// `SVN_ERR_REPOS_DISABLED_FEATURE`: a revision-property change with no `pre-revprop-change` hook.
/// Format: the repository category's start plus 6 (`svn_error_codes.h` 953-955).
const SVN_ERR_REPOS_DISABLED_FEATURE: u32 = 165_006;

/// `SVN_ERR_REPOS_POST_LOCK_HOOK_FAILED`, over a post-lock hook's failure (`svn_repos_fs_lock_many`, `fs-wrap.c`).
/// Format: the repository category's start plus 8 (`svn_error_codes.h` 962-964).
const SVN_ERR_REPOS_POST_LOCK_HOOK_FAILED: u32 = 165_008;

/// `SVN_ERR_REPOS_POST_UNLOCK_HOOK_FAILED`, over a post-unlock hook's failure (`svn_repos_fs_unlock_many`,
/// `fs-wrap.c`).
/// Format: the repository category's start plus 9 (`svn_error_codes.h` 967-969).
const SVN_ERR_REPOS_POST_UNLOCK_HOOK_FAILED: u32 = 165_009;

/// Format: `apr_exit_why_e`'s `APR_PROC_SIGNAL`, how APR reports a process a signal ended (`apr_thread_proc.h`). A core
/// dumped would add `APR_PROC_SIGNAL_CORE`, which [`ExitStatus`] does not carry.
const APR_PROC_SIGNAL: u32 = 2;

/// Format: the status a child whose `exec` failed exits with, APR's `_exit(-1)` after the failed `execve`, which the
/// kernel keeps as 255 (`apr_proc_create`, `threadproc/unix/proc.c`). `svn_io_start_cmd3` never asks APR to check the
/// program first, so `svnserve` learns of the failure from this status alone, with nothing written (the recorded
/// start-commit that was not executable: "exit code 255 ... with no output.").
const APR_EXEC_FAILED_STATUS: i32 = 255;

/// The words `check_hook_result` puts in place of a hook's standard error that is not UTF-8: the server speaks UTF-8,
/// so `svn_utf_cstring_to_utf8` takes valid UTF-8 as it is and refuses anything else.
const SVN_UNTRANSLATABLE_WORDS: &str =
  "[Error output could not be translated from the native locale to UTF-8.]";

/// `svn_repos__post_commit_error_str`'s words for a post-commit failure with no message (`commit.c`).
const SVN_NO_POST_COMMIT_MESSAGE: &str = "post-commit hook failed with no error message.";

/// The words `strerror` gives `EAGAIN` alike in glibc, macOS and Windows' C runtime.
const FORK_REFUSED_WORDS: &str = "Resource temporarily unavailable";

/// `EAGAIN` as this platform numbers it: what `fork` refuses a full process table with (`fork(2)`); APR's status is
/// the number itself on Unix.
#[cfg(unix)]
fn fork_refused_errno() -> u32 {
  u32::try_from(rustix::io::Errno::AGAIN.raw_os_error()).unwrap_or_default()
}

/// `EAGAIN` as Windows' C runtime numbers it.
/// Format: `errno.h`'s `EAGAIN` in the Universal C Runtime.
#[cfg(not(unix))]
fn fork_refused_errno() -> u32 {
  11
}

/// One error of the chain `svnserve` sends when a command fails (`svn_ra_svn__write_cmd_failure`, outermost first):
/// its number and its message. A release build sends an empty file and a zero line with each, which the protocol's
/// writer adds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SvnError {
  /// Its number: Subversion's (`svn_error_codes.h`) or the operating system's.
  pub(crate) code: u32,
  /// Its message.
  pub(crate) message: String,
}

/// What `svnserve` tells its client of one run of a Subversion hook (A-623).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SvnTold {
  /// Nothing: the hook accepted, or its client has left (an admission point's change still does not go on).
  Nothing,
  /// This failure, outermost error first: the command's (start-commit's at `open-root`, pre-commit's at `close-edit`,
  /// the revision-property hooks' at `change-rev-prop2`, post-lock's and post-unlock's after the `done` that ends
  /// `lock-many`'s and `unlock-many`'s paths), or one path's among those paths (pre-lock's and pre-unlock's).
  Failure(Vec<SvnError>),
  /// The commit stands, and its `commit-info` carries this as `post-commit-err`.
  PostCommitErr(String),
}

/// The action `check_hook_result` names a refusal by: its admission points' (`hooks.c`); none for the observing ones.
fn svn_action(point: HookPoint) -> Option<&'static str> {
  match point {
    HookPoint::SvnStartCommit | HookPoint::SvnPreCommit => Some("Commit"),
    HookPoint::SvnPreRevpropChange => Some("Revprop change"),
    HookPoint::SvnPreLock => Some("Lock"),
    HookPoint::SvnPreUnlock => Some("Unlock"),
    _ => None,
  }
}

/// What `check_hook_result` reports of a hook's standard error: its bytes up to the first NUL (it reads them as a C
/// string), as they are when they are UTF-8, else [`SVN_UNTRANSLATABLE_WORDS`]. Bytes past the runner's cap were never
/// kept, and a character the cap cut in two is left out, so that a cut does not turn the whole into the notice.
fn svn_hook_words(words: &Words) -> String {
  let mut fields = words.kept.split(|byte| *byte == 0);
  let text = fields.next().unwrap_or_default();
  let cut = words.dropped > 0 && fields.next().is_none();
  match std::str::from_utf8(text) {
    Ok(valid) => valid.to_owned(),
    Err(error) if cut && error.error_len().is_none() => text
      .get(..error.valid_up_to())
      .and_then(|whole| std::str::from_utf8(whole).ok())
      .unwrap_or_default()
      .to_owned(),
    Err(_) => SVN_UNTRANSLATABLE_WORDS.to_owned(),
  }
}

/// `check_hook_result`'s error for a hook at `point` that ended `status` having written `words` (`hooks.c`).
fn svn_hook_failed(point: HookPoint, status: ExitStatus, words: &Words) -> SvnError {
  let name = point.name();
  let mut message = match (status, svn_action(point)) {
    (ExitStatus::Signal(signal), _) => format!(
      "'{name}' hook failed (did not exit cleanly: apr_exit_why_e was {APR_PROC_SIGNAL}, \
       exitcode was {signal}).  "
    ),
    (ExitStatus::Code(code), Some(action)) => {
      format!("{action} blocked by {name} hook (exit code {code})")
    }
    (ExitStatus::Code(code), None) => format!("{name} hook failed (exit code {code})"),
  };
  let text = svn_hook_words(words);
  if text.is_empty() {
    message.push_str(" with no output.");
  } else {
    message.push_str(" with output:\n");
    message.push_str(&text);
  }
  SvnError {
    code: SVN_ERR_REPOS_HOOK_FAILURE,
    message,
  }
}

/// What `svnserve` reports of a hook whose program, `program`, could not be run, by the runner's cause: a full process
/// table fails `fork` in the server, which `run_hook_cmd` reports as "Failed to start" over `svn_io_start_cmd3`'s
/// "Can't start process"; any other cause fails the `exec` in the child, which exits [`APR_EXEC_FAILED_STATUS`] having
/// written nothing, and is reported as that exit.
fn svn_unstartable(point: HookPoint, program: &str, cause: StartRefused) -> Vec<SvnError> {
  match cause {
    StartRefused::NoRoom => vec![
      SvnError {
        code: SVN_ERR_REPOS_BAD_ARGS,
        message: format!("Failed to start '{program}' hook"),
      },
      SvnError {
        code: fork_refused_errno(),
        message: format!("Can't start process '{program}': {FORK_REFUSED_WORDS}"),
      },
    ],
    StartRefused::NotFound | StartRefused::NotPermitted | StartRefused::Other(_) => {
      vec![svn_hook_failed(
        point,
        ExitStatus::Code(APR_EXEC_FAILED_STATUS),
        &Words::default(),
      )]
    }
  }
}

/// `chain` under an error saying the change was made, as `fs-wrap.c` wraps a post-lock or post-unlock failure.
fn svn_made_but(code: u32, message: &str, chain: Vec<SvnError>) -> SvnTold {
  let mut errors = vec![SvnError {
    code,
    message: message.to_owned(),
  }];
  errors.extend(chain);
  SvnTold::Failure(errors)
}

/// How `svnserve` sends the failure `chain` of a hook at `point`: post-commit's first message as `commit-info`'s
/// `post-commit-err` (`svn_repos__post_commit_error_str` takes the error under
/// `SVN_ERR_REPOS_POST_COMMIT_HOOK_FAILED`); post-lock's and post-unlock's under the error saying the change was made
/// (`fs-wrap.c`); every other point's as it is.
fn svn_reported(point: HookPoint, chain: Vec<SvnError>) -> SvnTold {
  match point {
    HookPoint::SvnPostCommit => SvnTold::PostCommitErr(chain.into_iter().next().map_or_else(
      || SVN_NO_POST_COMMIT_MESSAGE.to_owned(),
      |first| first.message,
    )),
    HookPoint::SvnPostLock => svn_made_but(
      SVN_ERR_REPOS_POST_LOCK_HOOK_FAILED,
      "Locking succeeded, but post-lock hook failed",
      chain,
    ),
    HookPoint::SvnPostUnlock => svn_made_but(
      SVN_ERR_REPOS_POST_UNLOCK_HOOK_FAILED,
      "Unlock succeeded, but post-unlock hook failed",
      chain,
    ),
    _ => SvnTold::Failure(chain),
  }
}

/// What `svnserve` tells its client of one run of the hook at `point`, the program `program` (its path, which
/// `svnserve`'s own messages name), that ended `outcome`; `None` at another tool's point (A-623). After a commit's own
/// release of its locks `svnserve` logs the unlock hooks' failures and tells the client nothing (`serve.c`
/// `unlock_paths`): that caller sends none of this.
pub(crate) fn svn_told(point: HookPoint, program: &str, outcome: &Outcome) -> Option<SvnTold> {
  if !point.is_subversions() {
    return None;
  }
  let chain = match outcome {
    Outcome::Accepted { .. } | Outcome::Abandoned => return Some(SvnTold::Nothing),
    Outcome::Refused { status, words } | Outcome::TimedOut { status, words, .. } => {
      vec![svn_hook_failed(point, *status, words)]
    }
    Outcome::Unstartable { cause } => svn_unstartable(point, program, *cause),
  };
  Some(svn_reported(point, chain))
}

/// What `svnserve` tells its client when `point` has no hook: `pre-revprop-change`'s refusal of every change
/// (`SVN_ERR_REPOS_DISABLED_FEATURE`), else nothing; `None` at another tool's point.
pub(crate) fn svn_told_unconfigured(point: HookPoint) -> Option<SvnTold> {
  if !point.is_subversions() {
    return None;
  }
  Some(match point.unconfigured() {
    Unconfigured::Refused(words) => SvnTold::Failure(vec![SvnError {
      code: SVN_ERR_REPOS_DISABLED_FEATURE,
      message: words.to_owned(),
    }]),
    Unconfigured::Accepted => SvnTold::Nothing,
  })
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
}

/// What a run teaches its hook's times (A-622 part 3): the runner reports a run that finished and a run cut at its bound
/// to whatever holds them, the hook's times themselves or a [`Lesson`] a driver carries across its awaits (it cannot hold
/// the shard's times there) and teaches them after.
pub(crate) trait Learns {
  /// A run that finished `ran_ns` after it started.
  fn finished(&mut self, ran_ns: u64);
  /// A run cut at its bound.
  fn ran_out(&mut self);
}

impl Learns for HookTimes {
  /// A sample, and the end of any back-off.
  fn finished(&mut self, ran_ns: u64) {
    self.runs.completed(ran_ns);
    self.samples = self.samples.saturating_add(1);
  }

  /// No sample, and the bound backed off.
  fn ran_out(&mut self) {
    self.runs.ran_out();
    self.cuts = self.cuts.saturating_add(1);
  }
}

/// One run's lesson for its hook's times, carried by a driver (A-622 part 3): the finished run's length, or that it was
/// cut at its bound; a run teaches at most one of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Lesson {
  /// The run's length, when it finished on its own.
  ran_ns: Option<u64>,
  /// Whether it was cut at its bound.
  cut: bool,
}

impl Learns for Lesson {
  fn finished(&mut self, ran_ns: u64) {
    self.ran_ns = Some(ran_ns);
  }

  fn ran_out(&mut self) {
    self.cut = true;
  }
}

impl Lesson {
  /// Teaches `times` what the run showed: its length as a sample, or its cut as a back-off.
  pub(crate) fn teach(self, times: &mut HookTimes) {
    if let Some(ran_ns) = self.ran_ns {
      times.finished(ran_ns);
    }
    if self.cut {
      times.ran_out();
    }
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
    /// How its process ended once killed (Subversion tells its client this, A-623).
    status: ExitStatus,
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
    Self::with_bound(started_ns, times.bound_ns(), words_cap)
  }

  /// A run whose process its driver started at `started_ns`, held to `bound_ns` (its hook's learned bound, read by the
  /// driver before its first await), its words kept to `words_cap` bytes; and what the driver does first.
  pub(crate) fn with_bound(
    started_ns: u64,
    bound_ns: Option<u64>,
    words_cap: usize,
  ) -> (Self, Step) {
    let run = Self {
      state: RunState::Running,
      words: Words::default(),
      words_cap,
      started_ns,
      bound_ns,
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
  pub(crate) fn on(&mut self, event: Event<'_>, times: &mut impl Learns) -> Step {
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
  fn running(&mut self, event: Event<'_>, times: &mut impl Learns) -> Step {
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
  fn exited(&mut self, status: ExitStatus, at_ns: u64, times: &mut impl Learns) -> Step {
    times.finished(at_ns.saturating_sub(self.started_ns));
    if self.output_open {
      self.state = RunState::Draining(status);
      return self.wait();
    }
    self.finish_exit(status, false)
  }

  /// The timer fired at `now_ns`: at or past the deadline, the run is cut at its bound (no sample, the bound backed
  /// off) and its hook killed; before it, a wait for the deadline still.
  fn timer(&mut self, now_ns: u64, times: &mut impl Learns) -> Step {
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
      Event::Exited { status, .. } => {
        let outcome = match why {
          Kill::PastBound(bound_ns) => Outcome::TimedOut {
            bound_ns,
            status,
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
  use std::collections::BTreeSet;

  use proptest::prelude::*;

  use super::{
    Changegroup, CommittedView, EnvValue, Event, ExitStatus, HookInput, HookPoint, HookRun,
    HookTimes, LockToken, Outcome, PendingView, Phase, RefUpdate, RevpropAction, RevpropChange,
    StartRefused, Step, SvnError, SvnHooksEnv, SvnTold, Unconfigured, View, Words, WordsFrom,
    git_post_receive, git_pre_receive, git_update, hg_changegroup, hg_pretxnchangegroup,
    svn_post_commit, svn_post_lock, svn_post_revprop_change, svn_post_unlock, svn_pre_commit,
    svn_pre_lock, svn_pre_revprop_change, svn_pre_unlock, svn_start_commit, svn_told,
    svn_told_unconfigured,
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

  /// A-597, A-623 (the points, as each tool's documentation and code have them). Do: ask every point its phase, name,
  /// words streams, tool and what it answers unconfigured. Expect: the admission hooks are git's pre-receive and
  /// update, Mercurial's pretxnchangegroup and Subversion's start-commit, pre-commit, pre-revprop-change, pre-lock and
  /// pre-unlock; the observing ones the rest; Subversion's points, and they alone, take standard error alone, and
  /// pre-lock alone has its output read; and only pre-revprop-change refuses when absent, with `hooks.c`'s words.
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
      (
        HookPoint::SvnStartCommit,
        Phase::Admission,
        "start-commit",
        WordsFrom::ErrorStream,
      ),
      (
        HookPoint::SvnPostRevpropChange,
        Phase::Observing,
        "post-revprop-change",
        WordsFrom::ErrorStream,
      ),
      (
        HookPoint::SvnPreLock,
        Phase::Admission,
        "pre-lock",
        WordsFrom::ErrorStream,
      ),
      (
        HookPoint::SvnPostLock,
        Phase::Observing,
        "post-lock",
        WordsFrom::ErrorStream,
      ),
      (
        HookPoint::SvnPreUnlock,
        Phase::Admission,
        "pre-unlock",
        WordsFrom::ErrorStream,
      ),
      (
        HookPoint::SvnPostUnlock,
        Phase::Observing,
        "post-unlock",
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
      assert_eq!(
        point.output_is_lock_token(),
        point == HookPoint::SvnPreLock,
        "{name}"
      );
      assert_eq!(
        point.is_subversions(),
        from == WordsFrom::ErrorStream,
        "{name}"
      );
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

  /// A-597, A-623 (Subversion's inputs, `hooks.c`). Do: build pre-commit with two lock tokens, one path holding a
  /// space and a `|`, and with none. Expect: its arguments are the repository and the transaction; with tokens its
  /// input is `LOCK-TOKENS:`, a `path|token` line a token, the path escaped (`%20`, `%7C`), and a blank line; with none
  /// it is nothing at all, since `hooks.c` hands the hook the null device then (the recorded runs agree:
  /// `subversions_commit_hooks_get_what_svnserve_gave_them`).
  #[test]
  fn subversions_pre_commit_gets_its_lock_tokens_as_hooks_c_writes_them() {
    let tokens = [
      LockToken {
        path: "/trunk/a b|c.c".to_owned(),
        token: "opaquelocktoken:1".to_owned(),
      },
      LockToken {
        path: "/trunk/c.c".to_owned(),
        token: "opaquelocktoken:2".to_owned(),
      },
    ];
    let env = SvnHooksEnv::default();
    let pre = svn_pre_commit(PendingView(1), &env, "/srv/repos/x", "41-1a", &tokens);
    assert_eq!(pre.args, vec![b"/srv/repos/x".to_vec(), b"41-1a".to_vec()]);
    assert_eq!(
      pre.stdin,
      b"LOCK-TOKENS:\n/trunk/a%20b%7Cc.c|opaquelocktoken:1\n/trunk/c.c|opaquelocktoken:2\n\n"
        .to_vec()
    );
    assert_eq!(
      svn_pre_commit(PendingView(1), &env, "/srv/repos/x", "41-1a", &[]).stdin,
      Vec::<u8>::new(),
      "no tokens, the null device: nothing"
    );
  }

  /// A-597 (Subversion's inputs, `hooks.c`). Do: build pre-revprop-change for a modification, a deletion and an
  /// addition, and post-commit. Expect: pre-revprop-change's five arguments end in the action's letter, with the new
  /// value on input and nothing for a deletion; post-commit's are the repository, the revision and the transaction.
  #[test]
  fn subversions_revprop_and_post_commit_hooks_get_what_hooks_c_gives_them() {
    let env = SvnHooksEnv::default();
    let change = |author, property, action| RevpropChange {
      repository: "/srv/repos/x",
      revision: 40,
      author,
      property,
      action,
    };
    let modified = svn_pre_revprop_change(
      CommittedView(2),
      &env,
      change("ada", "svn:log", RevpropAction::Modified),
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
      &env,
      change("", "svn:log", RevpropAction::Deleted),
      b"",
    );
    assert_eq!(deleted.args.get(2), Some(&Vec::new()));
    assert_eq!(deleted.args.get(4), Some(&b"D".to_vec()));
    assert!(deleted.stdin.is_empty());
    let added = svn_pre_revprop_change(
      CommittedView(2),
      &env,
      change("ada", "ci:status", RevpropAction::Added),
      b"green",
    );
    assert_eq!(added.args.get(4), Some(&b"A".to_vec()));
    assert_eq!(added.stdin, b"green".to_vec());
    let post = svn_post_commit(CommittedView(3), &env, "/srv/repos/x", 41, "40-1a");
    assert_eq!(
      post.args,
      vec![b"/srv/repos/x".to_vec(), b"41".to_vec(), b"40-1a".to_vec()]
    );
  }

  /// One hook run recorded by `crates/svn/fixtures/hooks/make-fixtures.py` from stock `svnserve` 1.15.0 on the Mac
  /// Studio (A-623; each file's SHA-256 in that directory's `SHA256SUMS`): its arguments after its path, each
  /// NUL-ended; its standard input; its environment from `env -0`.
  struct Recorded {
    args: &'static [u8],
    stdin: &'static [u8],
    env: &'static [u8],
  }

  /// The recorded run whose files are named `stem`, `<case>.<n>-<hook>`.
  macro_rules! recorded {
    ($stem:literal) => {
      Recorded {
        args: include_bytes!(concat!("../../svn/fixtures/hooks/runs/", $stem, ".args")),
        stdin: include_bytes!(concat!("../../svn/fixtures/hooks/runs/", $stem, ".stdin")),
        env: include_bytes!(concat!("../../svn/fixtures/hooks/runs/", $stem, ".env")),
      }
    };
  }

  /// What `svnserve` sent its client in the recorded case `name`, every connection in order.
  macro_rules! told {
    ($name:literal) => {
      include_bytes!(concat!(
        "../../svn/fixtures/hooks/told/",
        $name,
        ".server.bin"
      ))
      .as_slice()
    };
  }

  /// The repository's directory as the recorded hooks were given it.
  const RECORDED_REPOSITORY: &str = include_str!("../../svn/fixtures/hooks/repository-path.txt");

  /// What `/bin/sh` adds to a hook's environment on its own: the recorded hook run by hand under `env -i`.
  const SH_OWN_ENV: &[u8] =
    include_bytes!("../../svn/fixtures/hooks/runs/control.1-pre-commit.env");

  /// The recording's hooks-env file (the script's `HOOKS_ENV`) as its sections in file order: a default section named
  /// twice, pre-commit's own, and post-commit's with no variables.
  fn recorded_hooks_env() -> SvnHooksEnv {
    let variable = |name: &str, value: &str| (name.to_owned(), value.as_bytes().to_vec());
    SvnHooksEnv::new(vec![
      ("default".to_owned(), vec![variable("LANG", "en_US.UTF-8")]),
      (
        "pre-commit".to_owned(),
        vec![variable("SLATES_FIXTURE", "pre-commit section")],
      ),
      ("post-commit".to_owned(), Vec::new()),
      (
        "default".to_owned(),
        vec![variable("SLATES_FIXTURE", "default section")],
      ),
    ])
  }

  /// The program a recorded hook at `point` was: the repository's `hooks/<name>`, which `svnserve`'s messages name.
  fn program(point: HookPoint) -> String {
    format!("{RECORDED_REPOSITORY}/hooks/{}", point.name())
  }

  /// NUL-ended fields.
  fn fields(bytes: &[u8]) -> Vec<&[u8]> {
    let mut fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    fields.pop();
    fields
  }

  /// A `NAME=value` field as its name and its value.
  fn variable(field: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut parts = field.splitn(2, |byte| *byte == b'=');
    let name = parts.next().unwrap_or_default().to_vec();
    (name, parts.next().unwrap_or_default().to_vec())
  }

  /// Environment variables, each a name and a value.
  type Variables = BTreeSet<(Vec<u8>, Vec<u8>)>;

  /// A recorded environment's variables, without what `/bin/sh` adds.
  fn svn_environment(env: &[u8]) -> Variables {
    let sh_names: BTreeSet<Vec<u8>> = fields(SH_OWN_ENV)
      .into_iter()
      .map(|field| variable(field).0)
      .collect();
    fields(env)
      .into_iter()
      .map(variable)
      .filter(|(name, _)| !sh_names.contains(name))
      .collect()
  }

  /// `input`'s environment, every value bytes as Subversion's always are.
  fn our_environment(input: &HookInput) -> Variables {
    input
      .env
      .iter()
      .map(|(name, value)| match value {
        EnvValue::Bytes(bytes) => (name.as_bytes().to_vec(), bytes.clone()),
        other => panic!("{other:?}"),
      })
      .collect()
  }

  /// `input` is what the recorded run was given: its arguments and standard input byte for byte, and its whole
  /// environment, compared as a set since `svnserve` passes a section in its hash table's order.
  fn assert_recorded(input: &HookInput, recorded: &Recorded) {
    let args: Vec<&[u8]> = input.args.iter().map(Vec::as_slice).collect();
    assert_eq!(args, fields(recorded.args), "{:?}", input.point);
    assert_eq!(input.stdin, recorded.stdin, "{:?}", input.point);
    assert_eq!(
      our_environment(input),
      svn_environment(recorded.env),
      "{:?}",
      input.point
    );
    assert!(input.clean_environment, "{:?}", input.point);
  }

  /// Bytes' lines, sorted: what a `LOCK-TOKENS:` input holds whatever order its token lines came in.
  fn sorted_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    lines.sort_unstable();
    lines
  }

  /// A-623 (golden: stock svnserve's own hooks). Do: build start-commit, pre-commit and post-commit for the recording's
  /// two `svnmucc` commits, by two users, with no lock token, from the recording's hooks-env file. Expect: each the
  /// recorded run byte for byte: start-commit with the client's capability, pre-commit with nothing on standard input
  /// and its own section alone, and post-commit the default section, merged from its two parts, though post-commit
  /// has an empty section of its own (Subversion makes none of it).
  #[test]
  fn subversions_commit_hooks_get_what_svnserve_gave_them() {
    let env = recorded_hooks_env();
    let commits = [
      (
        "jrandom",
        "0-0",
        1,
        recorded!("commit-by-jrandom.1-start-commit"),
        recorded!("commit-by-jrandom.2-pre-commit"),
        recorded!("commit-by-jrandom.3-post-commit"),
      ),
      (
        "jconstant",
        "1-1",
        2,
        recorded!("commit-by-jconstant.1-start-commit"),
        recorded!("commit-by-jconstant.2-pre-commit"),
        recorded!("commit-by-jconstant.3-post-commit"),
      ),
    ];
    for (user, transaction, revision, start, pre, post) in &commits {
      let repository = RECORDED_REPOSITORY;
      let started = svn_start_commit(
        PendingView(1),
        &env,
        repository,
        user,
        &["mergeinfo"],
        transaction,
      );
      assert_recorded(&started, start);
      assert_recorded(
        &svn_pre_commit(PendingView(1), &env, repository, transaction, &[]),
        pre,
      );
      let committed = svn_post_commit(CommittedView(1), &env, repository, *revision, transaction);
      assert_recorded(&committed, post);
    }
  }

  /// A-623 (golden). Do: build pre-commit for the recording's commit that releases the lock its pre-lock hook chose the
  /// token of, then the unlock hooks `svnserve` ran as it released the lock; and pre-commit for the commit that kept two
  /// locks. Expect: each the recorded run: the token lines name the client's own paths, relative to its commit's base
  /// (`b.txt` for `/trunk/with space/b.txt`), a byte past ASCII kept as it is; the two lines in either order, as
  /// `svnserve` writes them in its hash table's; the release's pre-unlock with the token and not broken.
  #[test]
  fn a_commits_lock_tokens_and_their_release_reach_the_hooks_as_svnserve_gave_them() {
    let env = recorded_hooks_env();
    let (repository, view) = (RECORDED_REPOSITORY, CommittedView(1));
    let token = |path: &str, token: &str| LockToken {
      path: path.to_owned(),
      token: token.to_owned(),
    };
    let released = [token("b.txt", "opaquelocktoken:fixture-b")];
    let pre = svn_pre_commit(PendingView(1), &env, repository, "2-2", &released);
    assert_recorded(&pre, &recorded!("commit-releasing-its-lock.2-pre-commit"));
    let path = "/trunk/with space/b.txt";
    let given_back = svn_pre_unlock(
      view,
      &env,
      repository,
      path,
      "jrandom",
      "opaquelocktoken:fixture-b",
      false,
    );
    assert_recorded(
      &given_back,
      &recorded!("commit-releasing-its-lock.4-pre-unlock"),
    );
    let unlocked = svn_post_unlock(view, &env, repository, "jrandom", &[path]);
    assert_recorded(
      &unlocked,
      &recorded!("commit-releasing-its-lock.5-post-unlock"),
    );
    let kept = [
      token("café.txt", "opaquelocktoken:fixture-c"),
      token("a.txt", "opaquelocktoken:fixture-a"),
    ];
    let keeping = svn_pre_commit(PendingView(1), &env, repository, "3-3", &kept);
    let recorded = recorded!("commit-keeping-its-locks.2-pre-commit");
    assert_eq!(sorted_lines(&keeping.stdin), sorted_lines(recorded.stdin));
    assert!(keeping.stdin.starts_with(b"LOCK-TOKENS:\n") && keeping.stdin.ends_with(b"\n\n"));
    let args: Vec<&[u8]> = keeping.args.iter().map(Vec::as_slice).collect();
    assert_eq!(args, fields(recorded.args));
  }

  /// A-623 (golden). Do: build the lock hooks for the recording's locks by URL: one taken with a two-line comment,
  /// stolen by another user, broken by the first. Expect: each the recorded run byte for byte: pre-lock and pre-unlock
  /// with nothing on standard input, the comment, the token (none, broken) and the steal and break flags as
  /// arguments; post-lock and post-unlock with the path on a line.
  #[test]
  fn subversions_lock_hooks_get_what_svnserve_gave_them() {
    let env = recorded_hooks_env();
    let (view, repository, path) = (CommittedView(1), RECORDED_REPOSITORY, "/trunk/a.txt");
    let comment = "First line of the comment.\nSecond line.";
    let taken = svn_pre_lock(view, &env, repository, path, "jrandom", comment, false);
    assert_recorded(&taken, &recorded!("lock.1-pre-lock"));
    let after_taken = svn_post_lock(view, &env, repository, "jrandom", &[path]);
    assert_recorded(&after_taken, &recorded!("lock.2-post-lock"));
    let stolen = svn_pre_lock(view, &env, repository, path, "jconstant", "Stolen.", true);
    assert_recorded(&stolen, &recorded!("lock-stolen.1-pre-lock"));
    let after_stolen = svn_post_lock(view, &env, repository, "jconstant", &[path]);
    assert_recorded(&after_stolen, &recorded!("lock-stolen.2-post-lock"));
    let broken = svn_pre_unlock(view, &env, repository, path, "jrandom", "", true);
    assert_recorded(&broken, &recorded!("lock-broken.1-pre-unlock"));
    let after_broken = svn_post_unlock(view, &env, repository, "jrandom", &[path]);
    assert_recorded(&after_broken, &recorded!("lock-broken.2-post-unlock"));
  }

  /// A-623 (golden). Do: build the lock hooks for the recording's locks from a working copy: a path with a space, then
  /// two paths one at a time, one past ASCII, each with no comment, and both given back with their tokens. Expect: each
  /// the recorded run byte for byte, the paths unescaped in arguments and on standard input alike.
  #[test]
  fn locks_from_a_working_copy_and_their_return_reach_the_hooks_as_svnserve_gave_them() {
    let env = recorded_hooks_env();
    let (view, repository) = (CommittedView(1), RECORDED_REPOSITORY);
    let (spaced, cafe, a) = ("/trunk/with space/b.txt", "/trunk/café.txt", "/trunk/a.txt");
    let lock = |path| svn_pre_lock(view, &env, repository, path, "jrandom", "", false);
    let locked = |path| svn_post_lock(view, &env, repository, "jrandom", &[path]);
    let unlock =
      |path, token| svn_pre_unlock(view, &env, repository, path, "jrandom", token, false);
    let unlocked = |path| svn_post_unlock(view, &env, repository, "jrandom", &[path]);
    assert_recorded(
      &lock(spaced),
      &recorded!("lock-token-from-the-hook.1-pre-lock"),
    );
    assert_recorded(
      &locked(spaced),
      &recorded!("lock-token-from-the-hook.2-post-lock"),
    );
    assert_recorded(&lock(cafe), &recorded!("lock-two-paths.1-pre-lock"));
    assert_recorded(&locked(cafe), &recorded!("lock-two-paths.2-post-lock"));
    assert_recorded(&lock(a), &recorded!("lock-two-paths.3-pre-lock"));
    assert_recorded(&locked(a), &recorded!("lock-two-paths.4-post-lock"));
    let cafe_back = unlock(cafe, "opaquelocktoken:fixture-c");
    assert_recorded(&cafe_back, &recorded!("unlock-with-tokens.1-pre-unlock"));
    assert_recorded(
      &unlocked(cafe),
      &recorded!("unlock-with-tokens.2-post-unlock"),
    );
    assert_recorded(
      &unlock(a, "opaquelocktoken:fixture-a"),
      &recorded!("unlock-with-tokens.3-pre-unlock"),
    );
    assert_recorded(&unlocked(a), &recorded!("unlock-with-tokens.4-post-unlock"));
  }

  /// A-623 (golden). Do: build the revision-property hooks for the recording's three changes to revision 1: `svn:log`
  /// modified, a property added, then deleted. Expect: each the recorded run byte for byte: pre-revprop-change with the
  /// new value on standard input (nothing for the deletion), post-revprop-change with the old one (nothing for the
  /// addition).
  #[test]
  fn subversions_revision_property_hooks_get_what_svnserve_gave_them() {
    let env = recorded_hooks_env();
    let view = CommittedView(1);
    let change = |property, action| RevpropChange {
      repository: RECORDED_REPOSITORY,
      revision: 1,
      author: "jrandom",
      property,
      action,
    };
    let edited = change("svn:log", RevpropAction::Modified);
    let pre_edit = svn_pre_revprop_change(view, &env, edited, b"Edited log message.");
    assert_recorded(
      &pre_edit,
      &recorded!("revprop-modified.1-pre-revprop-change"),
    );
    let post_edit = svn_post_revprop_change(view, &env, edited, b"First commit.");
    assert_recorded(
      &post_edit,
      &recorded!("revprop-modified.2-post-revprop-change"),
    );
    let added = change("slates:note", RevpropAction::Added);
    let pre_add = svn_pre_revprop_change(view, &env, added, b"added later");
    assert_recorded(&pre_add, &recorded!("revprop-added.1-pre-revprop-change"));
    let post_add = svn_post_revprop_change(view, &env, added, b"");
    assert_recorded(&post_add, &recorded!("revprop-added.2-post-revprop-change"));
    let deleted = change("slates:note", RevpropAction::Deleted);
    let pre_delete = svn_pre_revprop_change(view, &env, deleted, b"");
    assert_recorded(
      &pre_delete,
      &recorded!("revprop-deleted.1-pre-revprop-change"),
    );
    let post_delete = svn_post_revprop_change(view, &env, deleted, b"added later");
    assert_recorded(
      &post_delete,
      &recorded!("revprop-deleted.2-post-revprop-change"),
    );
  }

  /// A-623 (`run_hook_cmd` and `parse_hooks_env_option`, `hooks.c`). Do: ask a hooks-env whose default section is named
  /// twice, one variable set in both parts, with a pre-commit section and an empty post-commit one, for pre-commit,
  /// post-commit and pre-lock; ask an empty one. Expect: pre-commit gets its own section alone; post-commit and pre-lock
  /// the default section, merged, the variable at its last value; and without a file, every hook nothing at all.
  #[test]
  fn a_hooks_own_section_replaces_the_default_one_and_an_empty_one_is_none() {
    let bytes = |text: &str| EnvValue::Bytes(text.as_bytes().to_vec());
    let env = SvnHooksEnv::new(vec![
      (
        "default".to_owned(),
        vec![
          ("LANG".to_owned(), b"C".to_vec()),
          ("PATH".to_owned(), b"/usr/bin".to_vec()),
        ],
      ),
      (
        "pre-commit".to_owned(),
        vec![("ONLY".to_owned(), b"pre-commit".to_vec())],
      ),
      ("post-commit".to_owned(), Vec::new()),
      (
        "default".to_owned(),
        vec![("LANG".to_owned(), b"en_US.UTF-8".to_vec())],
      ),
    ]);
    let own = vec![("ONLY".to_owned(), bytes("pre-commit"))];
    assert_eq!(env.for_point(HookPoint::SvnPreCommit), own);
    let default = vec![
      ("LANG".to_owned(), bytes("en_US.UTF-8")),
      ("PATH".to_owned(), bytes("/usr/bin")),
    ];
    assert_eq!(env.for_point(HookPoint::SvnPostCommit), default);
    assert_eq!(env.for_point(HookPoint::SvnPreLock), default);
    assert!(
      SvnHooksEnv::default()
        .for_point(HookPoint::SvnStartCommit)
        .is_empty()
    );
  }

  /// The bytes `svnserve` writes `told` as (protocol §1's items; a release build's file empty and line zero): a failure
  /// tuple, or the `post-commit-err` that ends `commit-info`.
  fn wire(told: &SvnTold) -> Vec<u8> {
    let mut bytes = Vec::new();
    match told {
      SvnTold::Failure(chain) => {
        bytes.extend_from_slice(b"( failure ( ");
        for error in chain {
          let item = format!(
            "( {} {}:{} 0: 0 ) ",
            error.code,
            error.message.len(),
            error.message
          );
          bytes.extend_from_slice(item.as_bytes());
        }
        bytes.extend_from_slice(b") ) ");
      }
      SvnTold::PostCommitErr(message) => {
        bytes.extend_from_slice(format!("( {}:{message} ) ) ", message.len()).as_bytes());
      }
      SvnTold::Nothing => {}
    }
    bytes
  }

  /// Whether `haystack` holds `needle`, which is not empty.
  fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
      && haystack
        .windows(needle.len())
        .any(|window| window == needle)
  }

  /// A hook that exited `code` having written `stderr`.
  fn refused(code: i32, stderr: &[u8]) -> Outcome {
    Outcome::Refused {
      status: ExitStatus::Code(code),
      words: words(stderr),
    }
  }

  /// A recorded case of what the client was told: what `svnserve` sent, the hook's point, and how its run ended.
  type ToldCase = (&'static [u8], HookPoint, Outcome);

  /// A-623 (golden: what stock svnserve 1.15 tells its client). Do: word each recorded hook refusal and failure from the
  /// hook's outcome: each pre-hook refusing with output and without, a pre-commit killed by a signal, writing bytes
  /// UTF-8 refuses, or a NUL; each post-hook failing; a start-commit and a post-commit that could not be run. Expect:
  /// each failure, or `post-commit-err`, byte for byte in what `svnserve` sent.
  #[test]
  fn each_hooks_refusal_is_told_in_svnserves_words() {
    let not_run = || Outcome::Unstartable {
      cause: StartRefused::NotPermitted,
    };
    let killed = Outcome::Refused {
      status: ExitStatus::Signal(9),
      words: words(b"dying\n"),
    };
    let cases: Vec<ToldCase> = vec![
      (
        told!("start-commit-refuses"),
        HookPoint::SvnStartCommit,
        refused(1, b"start-commit refuses this commit\n"),
      ),
      (
        told!("pre-commit-refuses"),
        HookPoint::SvnPreCommit,
        refused(2, "Refused: café\nsecond line\n".as_bytes()),
      ),
      (
        told!("pre-commit-refuses-silently"),
        HookPoint::SvnPreCommit,
        refused(1, b""),
      ),
      (told!("pre-commit-killed"), HookPoint::SvnPreCommit, killed),
      (
        told!("pre-commit-invalid-utf8"),
        HookPoint::SvnPreCommit,
        refused(1, b"bad \xff\xfe bytes\n"),
      ),
      (
        told!("pre-commit-nul"),
        HookPoint::SvnPreCommit,
        refused(1, b"before\0after\n"),
      ),
      (
        told!("pre-revprop-change-refuses"),
        HookPoint::SvnPreRevpropChange,
        refused(1, b"no revision property changes\n"),
      ),
      (
        told!("post-revprop-change-fails"),
        HookPoint::SvnPostRevpropChange,
        refused(1, b"post-revprop-change failed\n"),
      ),
      (
        told!("pre-lock-refuses"),
        HookPoint::SvnPreLock,
        refused(1, b"no locks on this path\n"),
      ),
      (
        told!("post-lock-fails"),
        HookPoint::SvnPostLock,
        refused(1, b"post-lock failed\n"),
      ),
      (
        told!("pre-unlock-refuses"),
        HookPoint::SvnPreUnlock,
        refused(1, b"no unlocking\n"),
      ),
      (
        told!("post-unlock-fails"),
        HookPoint::SvnPostUnlock,
        refused(1, b"post-unlock failed\n"),
      ),
      (
        told!("post-commit-fails"),
        HookPoint::SvnPostCommit,
        refused(1, b"post-commit failed\n"),
      ),
      (
        told!("start-commit-not-executable"),
        HookPoint::SvnStartCommit,
        not_run(),
      ),
      (
        told!("post-commit-not-executable"),
        HookPoint::SvnPostCommit,
        not_run(),
      ),
    ];
    for (sent, point, outcome) in &cases {
      let told = svn_told(*point, &program(*point), outcome).expect("a Subversion point");
      assert!(holds(sent, &wire(&told)), "{point:?}: {told:?}");
    }
  }

  /// A-623 (golden). Do: word the recorded revision-property change made with no pre-revprop-change hook, and ask the
  /// other points what they answer with none. Expect: `E165006` with `hooks.c`'s words, byte for byte in what
  /// `svnserve` sent; nothing at Subversion's other points; no Subversion words at another tool's.
  #[test]
  fn a_missing_pre_revprop_change_hook_refuses_in_svnserves_words() {
    let told = svn_told_unconfigured(HookPoint::SvnPreRevpropChange).expect("a Subversion point");
    assert!(
      holds(told!("pre-revprop-change-missing"), &wire(&told)),
      "{told:?}"
    );
    assert_eq!(
      svn_told_unconfigured(HookPoint::SvnPostCommit),
      Some(SvnTold::Nothing)
    );
    assert_eq!(svn_told_unconfigured(HookPoint::GitPreReceive), None);
  }

  /// A-623. Do: word a pre-commit that slates killed past its learned bound, reaped as SIGKILL ended it, having written
  /// "dying". Expect: the words `svnserve` sent for the recorded pre-commit a signal killed, byte for byte: it has no
  /// deadline of its own, so its words for a hook killed are its words for a signal.
  #[test]
  fn a_hook_killed_past_its_bound_is_told_as_svnserve_tells_of_a_signal() {
    let timed_out = Outcome::TimedOut {
      bound_ns: QUICK_NS,
      status: ExitStatus::Signal(9),
      words: words(b"dying\n"),
    };
    let point = HookPoint::SvnPreCommit;
    let told = svn_told(point, &program(point), &timed_out).expect("a Subversion point");
    assert!(holds(told!("pre-commit-killed"), &wire(&told)), "{told:?}");
  }

  /// A-623. Do: word an accepted Subversion hook and an abandoned one, and outcomes at git's and Mercurial's points.
  /// Expect: nothing told for the first two; no Subversion words at all at another tool's points.
  #[test]
  fn accepted_and_abandoned_runs_tell_nothing_and_other_tools_points_have_no_subversion_words() {
    let accepted = Outcome::Accepted {
      words: words(b"chatter"),
    };
    assert_eq!(
      svn_told(HookPoint::SvnPreCommit, "/p", &accepted),
      Some(SvnTold::Nothing)
    );
    assert_eq!(
      svn_told(HookPoint::SvnPostCommit, "/p", &Outcome::Abandoned),
      Some(SvnTold::Nothing)
    );
    assert_eq!(
      svn_told(HookPoint::GitPreReceive, "/p", &refused(1, b"no")),
      None
    );
    assert_eq!(
      svn_told(HookPoint::HgChangegroup, "/p", &Outcome::Abandoned),
      None
    );
  }

  /// A pre-commit refusal's single error with `tail` after "(exit code 1)".
  fn refusal_ending(tail: &str) -> Option<SvnTold> {
    Some(SvnTold::Failure(vec![SvnError {
      code: 165_001,
      message: format!("Commit blocked by pre-commit hook (exit code 1){tail}"),
    }]))
  }

  /// A-623 (hostile input: words at the runner's cap). Do: word pre-commit refusals whose kept words end in a character
  /// the cap cut in two, with and without bytes dropped past it; that hold a byte UTF-8 refuses before such a cut; that
  /// hold a NUL before it; and that kept nothing. Expect: the cut character left out and the rest told; the same bytes
  /// with nothing dropped are no cut, and turn into the notice, as `svnserve` would; the notice for the bad byte; the
  /// words before the NUL; and "with no output." for nothing kept.
  #[test]
  fn words_cut_at_the_cap_lose_only_the_cut_character() {
    let told = |kept: &[u8], dropped: u64| {
      let words = Words {
        kept: kept.to_vec(),
        dropped,
      };
      let outcome = Outcome::Refused {
        status: ExitStatus::Code(1),
        words,
      };
      svn_told(HookPoint::SvnPreCommit, "/r/hooks/pre-commit", &outcome)
    };
    let notice =
      " with output:\n[Error output could not be translated from the native locale to UTF-8.]";
    assert_eq!(told(b"caf\xc3", 7), refusal_ending(" with output:\ncaf"));
    assert_eq!(told(b"caf\xc3", 0), refusal_ending(notice));
    assert_eq!(told(b"\xff ok\xc3", 7), refusal_ending(notice));
    assert_eq!(told(b"ok\0caf\xc3", 7), refusal_ending(" with output:\nok"));
    assert_eq!(told(b"", 7), refusal_ending(" with no output."));
  }

  /// A-623 (`run_hook_cmd`, `svn_io_start_cmd3`, APR's `apr_proc_create`). Do: word a pre-commit whose program was
  /// missing, not permitted, or refused for another reason. Expect: each as `svnserve`'s failed `exec`, whose child
  /// exits 255 having written nothing (the recorded start-commit that was not executable).
  #[test]
  fn a_program_that_cannot_be_executed_is_told_as_a_failed_exec() {
    let failed_exec = Some(SvnTold::Failure(vec![SvnError {
      code: 165_001,
      message: "Commit blocked by pre-commit hook (exit code 255) with no output.".to_owned(),
    }]));
    for cause in [
      StartRefused::NotFound,
      StartRefused::NotPermitted,
      StartRefused::Other(8),
    ] {
      let outcome = Outcome::Unstartable { cause };
      assert_eq!(
        svn_told(HookPoint::SvnPreCommit, "/r/hooks/pre-commit", &outcome),
        failed_exec
      );
    }
  }

  /// A-623 (`run_hook_cmd`'s "Failed to start", `svn_io_start_cmd3`'s "Can't start process"). Do: word a pre-commit and
  /// a post-commit the process table had no room for. Expect: pre-commit's as "Failed to start" (`E165002`) over
  /// "Can't start process" with `EAGAIN`'s number and words; post-commit's first message alone, as `post-commit-err`.
  #[test]
  fn a_hook_with_no_room_to_start_is_told_as_svnserve_tells_of_a_failed_fork() {
    let no_room = Outcome::Unstartable {
      cause: StartRefused::NoRoom,
    };
    let told = svn_told(HookPoint::SvnPreCommit, "/r/hooks/pre-commit", &no_room);
    let Some(SvnTold::Failure(chain)) = told else {
      panic!("{told:?}");
    };
    let messages: Vec<&str> = chain.iter().map(|error| error.message.as_str()).collect();
    let expected = [
      "Failed to start '/r/hooks/pre-commit' hook",
      "Can't start process '/r/hooks/pre-commit': Resource temporarily unavailable",
    ];
    assert_eq!(messages, expected);
    let codes: Vec<u32> = chain.iter().map(|error| error.code).collect();
    assert_eq!(codes.first(), Some(&165_002));
    if cfg!(unix) {
      let errno = codes
        .get(1)
        .and_then(|code| i32::try_from(*code).ok())
        .unwrap_or_default();
      let kind = std::io::Error::from_raw_os_error(errno).kind();
      assert_eq!(
        kind,
        std::io::ErrorKind::WouldBlock,
        "EAGAIN by this platform's number: {errno}"
      );
    }
    let post = svn_told(HookPoint::SvnPostCommit, "/r/hooks/post-commit", &no_room);
    let first = "Failed to start '/r/hooks/post-commit' hook".to_owned();
    assert_eq!(post, Some(SvnTold::PostCommitErr(first)));
  }

  /// A-623 (hostile input: a NUL in what a client sent). Do: build pre-lock with a NUL in the path, the user and the
  /// comment; post-lock with one in a path; start-commit with one in a capability; pre-commit with one in a token's path
  /// and in its token. Expect: each string ends at its NUL, as `svnserve`'s C strings do, and no argument holds a NUL,
  /// which no `execve` could pass.
  #[test]
  fn subversions_strings_end_at_their_first_nul() {
    let env = SvnHooksEnv::default();
    let lock = svn_pre_lock(
      CommittedView(1),
      &env,
      "/r",
      "/a\0b",
      "u\0v",
      "kept\0dropped",
      false,
    );
    let expected = vec![
      b"/r".to_vec(),
      b"/a".to_vec(),
      b"u".to_vec(),
      b"kept".to_vec(),
      b"0".to_vec(),
    ];
    assert_eq!(lock.args, expected);
    let locked = svn_post_lock(CommittedView(1), &env, "/r", "u", &["/a\0b", "/c"]);
    assert_eq!(locked.stdin, b"/a\n/c\n".to_vec());
    let start = svn_start_commit(
      PendingView(1),
      &env,
      "/r",
      "u",
      &["merge\0info", "depth"],
      "0-0",
    );
    assert_eq!(start.args.get(2), Some(&b"merge:depth".to_vec()));
    let token = LockToken {
      path: "a\0b".to_owned(),
      token: "opaquelocktoken:1\0x".to_owned(),
    };
    let pre = svn_pre_commit(PendingView(1), &env, "/r", "0-0", &[token]);
    assert_eq!(pre.stdin, b"LOCK-TOKENS:\na|opaquelocktoken:1\n\n".to_vec());
    assert!(
      lock
        .args
        .iter()
        .chain(&start.args)
        .all(|argument| !argument.contains(&0))
    );
  }

  /// A-623 (`svn_path_uri_autoescape`, `path.c`'s `uri_autoescape_chars`). Do: present a token for a path holding every
  /// byte Subversion escapes, then `%`, a control byte and a character past ASCII, which it keeps. Expect: the ten bytes
  /// as `%` and two capital hex digits, `|` among them, so the line still splits once; the others as they are.
  #[test]
  fn pre_commit_escapes_a_token_path_as_svn_path_uri_autoescape_does() {
    let token = LockToken {
      path: "a \"<>\\^`{|}%\u{1}é".to_owned(),
      token: "opaquelocktoken:t".to_owned(),
    };
    let pre = svn_pre_commit(
      PendingView(1),
      &SvnHooksEnv::default(),
      "/r",
      "0-0",
      &[token],
    );
    let expected = "LOCK-TOKENS:\na%20%22%3C%3E%5C%5E%60%7B%7C%7D%\u{1}é|opaquelocktoken:t\n\n";
    assert_eq!(pre.stdin, expected.as_bytes().to_vec());
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
          status: ExitStatus::Signal(9),
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
