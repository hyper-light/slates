//! The glue's tests (A-661): its choices by themselves, then a hook run end to end, the test binary re-invoked as the
//! hook (`crate::hook_driver`'s roles), its role named in the record's hooks-env, since a Subversion hook gets no other
//! environment.

use std::path::PathBuf;
use std::sync::mpsc::channel;

use slates_db::catalog::{
  HookConfig, HookPointName, HooksEnvSection, HooksEnvVariable, RepositoryPolicy,
};
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};

use super::{SvnHookRan, SvnHookSite, answer, configured, run, unconfigured};
use crate::hook_driver::Ended;
use crate::vcs_hooks::{
  CommittedView, ExitStatus, HookInput, HookPoint, Lesson, Outcome, PendingView, SvnError,
  SvnHooksEnv, SvnLockToken, SvnTold, Words, svn_pre_commit, svn_pre_lock,
};

/// Format: the variable that makes this test binary, re-invoked, a hook of the role it names (`hook_driver`'s tests).
const ROLE: &str = "SLATES_HOOK_ROLE";
/// Format: the libtest name of the role test the binary is re-invoked with (`hook_driver`'s tests).
const ROLE_TEST: &str = "hook_driver::tests::a_hook_role";
/// Format: the tag before each line a role writes (`hook_driver`'s tests).
const TAG: &str = "slates-hook:";
/// Format: the exit code `hook_driver`'s refusing role exits with.
const REFUSED: i32 = 3;
/// Shape: how many bytes of a hook's words the tests keep.
const CAP: usize = 1 << 16;
/// Shape: the repository's directory the tests name as REPOS.
const REPOS: &str = "/srv/svn/repository";
/// Shape: how long a test lets a run take before giving up on it: ten liveness windows (`hook_driver`'s tests), so a
/// hook that never ends fails its test rather than hanging the suite.
const TEST_BOUND_NS: u64 = 10 * crate::daemon::LIVENESS_BUDGET_NS;

/// A hook at `point`, named for it, running `program`.
fn hook(point: HookPointName, program: &str) -> HookConfig {
  HookConfig {
    point,
    name: point.name().to_owned(),
    program: program.to_owned(),
  }
}

/// A Subversion policy of `hooks`, its hooks-env one default section of `variables`.
fn policy(hooks: Vec<HookConfig>, variables: &[(&str, &str)]) -> RepositoryPolicy {
  RepositoryPolicy {
    version: 1,
    rules: Vec::new(),
    hooks,
    hooks_env: vec![HooksEnvSection {
      name: "default".to_owned(),
      variables: variables
        .iter()
        .map(|(name, value)| HooksEnvVariable {
          name: (*name).to_owned(),
          value: value.as_bytes().to_vec(),
        })
        .collect(),
    }],
  }
}

/// A run that ended `outcome`, its standard output `standard_output`.
fn ended(outcome: Outcome, standard_output: Option<&[u8]>) -> Ended {
  Ended {
    outcome,
    lesson: Lesson::default(),
    standard_output: standard_output.map(|bytes| Words {
      kept: bytes.to_vec(),
      dropped: 0,
    }),
  }
}

/// A hook that exited `code` having written `words`.
fn exited(code: i32, words: &[u8]) -> Outcome {
  Outcome::Refused {
    status: ExitStatus::Code(code),
    words: Words {
      kept: words.to_vec(),
      dropped: 0,
    },
  }
}

/// A-661 (Subversion keeps one hook a point). Do: ask a policy with a pre-commit and a post-commit hook for each, for
/// pre-lock, and ask an empty one for pre-commit. Expect: each its own hook; none at pre-lock or in the empty policy.
#[test]
fn a_points_hook_is_the_one_the_policy_configures_there() {
  let policy = policy(
    vec![
      hook(HookPointName::SvnPreCommit, "/hooks/pre-commit"),
      hook(HookPointName::SvnPostCommit, "/hooks/post-commit"),
    ],
    &[],
  );
  let program = |point| configured(&policy, point).map(|hook| hook.program.as_str());
  assert_eq!(program(HookPoint::SvnPreCommit), Some("/hooks/pre-commit"));
  assert_eq!(
    program(HookPoint::SvnPostCommit),
    Some("/hooks/post-commit")
  );
  assert_eq!(program(HookPoint::SvnPreLock), None);
  let empty = RepositoryPolicy::default();
  assert!(configured(&empty, HookPoint::SvnPreCommit).is_none());
}

/// A-661 (a point with no hook, `hooks.c`). Do: answer pre-revprop-change and post-commit with no hook. Expect:
/// pre-revprop-change refuses every change, `E165006` in `hooks.c`'s words; post-commit lets the change go on; nothing
/// is learned and no token is made.
#[test]
fn a_point_with_no_hook_answers_as_svnserve_does_without_running_anything() {
  let revprop = unconfigured(HookPoint::SvnPreRevpropChange);
  let words = "Repository has not been enabled to accept revision propchanges;\n\
               ask the administrator to create a pre-revprop-change hook";
  assert_eq!(
    revprop.verdict(),
    Some(Err(vec![(165_006, words.to_owned())]))
  );
  let post = unconfigured(HookPoint::SvnPostCommit);
  assert_eq!(post.verdict(), Some(Ok(())));
  assert_eq!((post.lesson, post.token), (None, None));
}

/// A-661 (what a run answers). Do: answer a pre-commit that exited 1 having written "no"; a post-commit that did the
/// same; a pre-lock that accepted having written a token; and a pre-commit whose client left. Expect: the pre-commit's
/// verdict its refusal in `svnserve`'s words; the post-commit's words its `post-commit-err`, no refusal; the pre-lock's
/// token; and no verdict at all once the client left. Each run's lesson is handed back.
#[test]
fn a_runs_answer_is_svnserves_words_its_lesson_and_pre_locks_token() {
  let refused = answer(
    HookPoint::SvnPreCommit,
    "/h/pre-commit",
    ended(exited(1, b"no\n"), None),
  );
  let message = "Commit blocked by pre-commit hook (exit code 1) with output:\nno\n".to_owned();
  assert_eq!(refused.verdict(), Some(Err(vec![(165_001, message)])));
  assert_eq!(refused.lesson, Some(Lesson::default()));
  let post = answer(
    HookPoint::SvnPostCommit,
    "/h/post-commit",
    ended(exited(1, b"no\n"), None),
  );
  let warned = "post-commit hook failed (exit code 1) with output:\nno\n".to_owned();
  assert_eq!(
    (post.told.clone(), post.verdict()),
    (SvnTold::PostCommitErr(warned), Some(Ok(())))
  );
  let accepted = Outcome::Accepted {
    words: Words::default(),
  };
  let locked = answer(
    HookPoint::SvnPreLock,
    "/h/pre-lock",
    ended(accepted, Some(b"opaquelocktoken:t".as_slice())),
  );
  assert_eq!(
    locked.token,
    Some(SvnLockToken::Given(b"opaquelocktoken:t".to_vec()))
  );
  let left = answer(
    HookPoint::SvnPreCommit,
    "/h/pre-commit",
    ended(Outcome::Abandoned, None),
  );
  assert_eq!((left.verdict(), left.client_left), (None, true));
}

/// A one-shard runtime for a run.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
    ring_entries: 8,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .expect("a local runtime")
}

/// `input` with libtest's arguments before its own, so the re-invoked binary runs its role test and the role reads
/// the hook's arguments after `--`.
fn as_role(mut input: HookInput) -> HookInput {
  let mut args: Vec<Vec<u8>> = [ROLE_TEST, "--exact", "--nocapture", "--"]
    .iter()
    .map(|arg| arg.as_bytes().to_vec())
    .collect();
  args.append(&mut input.args);
  input.args = args;
  input
}

/// Runs the hook `policy` configures at `input`'s point to its answer, the test binary as its program, with no
/// learned bound and a client that stays; the test gives up past [`TEST_BOUND_NS`].
fn ran(policy: RepositoryPolicy, input: HookInput) -> SvnHookRan {
  let runtime = runtime();
  let (sent, got) = channel();
  let program = std::env::current_exe().expect("the test binary");
  let view: PathBuf = program.parent().expect("its directory").to_path_buf();
  runtime
    .spawn(async move {
      let site = SvnHookSite {
        policy: &policy,
        view_root: &view,
        words_cap: CAP,
      };
      let running = run(&site, &input, None, std::future::pending());
      let answered = slates_rt::futures::within(TEST_BOUND_NS, running).await;
      let _ = sent.send(answered.ok().flatten());
    })
    .expect("the run's task");
  runtime.run_until_idle();
  got
    .recv()
    .ok()
    .flatten()
    .expect("the run answered within the test's bound")
}

/// The test binary's path, as a policy names its program.
fn this_binary() -> String {
  std::env::current_exe()
    .expect("the test binary")
    .to_string_lossy()
    .into_owned()
}

/// A-661 (end to end). Do: configure the test binary as a Subversion repository's pre-commit hook, its role the
/// refusing one by the record's hooks-env, and run it for a transaction. Expect: the commit refused in `svnserve`'s
/// words, the hook's standard error its output, as the drive's verdict.
#[test]
fn a_configured_pre_commit_that_refuses_refuses_the_commit_in_svnserves_words() {
  let policy = policy(
    vec![hook(HookPointName::SvnPreCommit, &this_binary())],
    &[(ROLE, "refuse")],
  );
  let env = SvnHooksEnv::of(&policy);
  let input = as_role(svn_pre_commit(PendingView(0), &env, REPOS, "0-0", &[]));
  let answered = ran(policy, input);
  let message = format!(
    "Commit blocked by pre-commit hook (exit code {REFUSED}) with output:\n\
     {TAG}refused by the hook\n"
  );
  let refusal = SvnTold::Failure(vec![SvnError {
    code: 165_001,
    message,
  }]);
  assert_eq!(answered.told, refusal);
  assert!(answered.lesson.is_some());
}

/// A-661 (end to end). Do: configure the test binary as pre-lock, its role the one that writes a token to standard
/// output and a line to standard error, and run it for a path. Expect: the lock may go on, and its token is what the
/// hook wrote to standard output (after libtest's own banner, which the re-invoked binary writes first).
#[test]
fn a_configured_pre_lock_hands_its_standard_output_back_as_the_token() {
  let policy = policy(
    vec![hook(HookPointName::SvnPreLock, &this_binary())],
    &[(ROLE, "token")],
  );
  let env = SvnHooksEnv::of(&policy);
  let input = svn_pre_lock(
    CommittedView(0),
    &env,
    REPOS,
    "/trunk/a.txt",
    "jrandom",
    "",
    false,
  );
  let answered = ran(policy, as_role(input));
  assert_eq!(answered.verdict(), Some(Ok(())));
  let token = format!("{TAG}opaquelocktoken:from-the-hook");
  match answered.token {
    Some(SvnLockToken::Given(given)) => assert!(given.ends_with(token.as_bytes()), "{given:?}"),
    other => panic!("no token: {other:?}"),
  }
}
