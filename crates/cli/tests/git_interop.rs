//! Stock git against a real `slates anchor` and its daemon (UI goal 93; §4.17.2 "A fetch", "A push" and "Transports"
//! items 1 and 2): stock git clones a repository, pushes to it and fetches from it over each route slates serves git
//! on, the daemon's HTTP edge (`--vcs-http`, A-625 part 3) and its SSH edge (`--vcs-ssh`, A-684), and is refused in
//! its own words where it brings no credential.
//!
//! **One session, two servers.** The session is a function of a client and a URL ([`round_trip`]), and the first test
//! runs it against stock git's own server, a bare repository reached over git's local transport, so what each step
//! expects is stock git's own behaviour, checked on every run where git is present, and a slates leg that fails a step
//! fails it for slates' sake. A step expects what a user sees: each command's success, the commit each copy's branch
//! names, the branch a clone checks out, the bytes a checkout holds and the history's length.
//!
//! **What waits.** Each slates leg is ignored naming the pieces it waits on and their agents, and the last of them to
//! land un-ignores it as its integration test. The refusal runs now: the edge refuses a request that brings no
//! credential before it looks for any repository.
//!
//! Stock git is `git` on the PATH, or the words `SLATES_TEST_GIT` names, and a host without it skips loudly; the SSH
//! leg needs OpenSSH's `ssh` and `ssh-keygen` too. git reads no system's or user's configuration, its home is the
//! test's scratch directory, it never prompts (`GIT_TERMINAL_PROMPT=0`, no askpass program), and its messages are C's.
//! Each test's copies, its key and git's home live in a scratch directory in the build output (`CARGO_TARGET_TMPDIR`),
//! named with the process id and removed at its end. A credential is minted with the anchor's issuer surface, which
//! the anchor prints on every platform (on Linux its issuer route and secret since A-754); were it to print none, a
//! leg that needs a credential skips loudly there (`tests/cli.rs`, `PRINTS_ISSUER_SURFACE`). Every platform: on Windows
//! (jupiter) stock git is Git for Windows and `ssh` the OpenSSH the PATH names first (MSYS2's there), which read the
//! scratch directory's forward-slashed paths and `/dev/null` as on Unix.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

#[path = "support/process_wait.rs"]
mod process_wait;

use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: the variable naming the command that runs stock git, its words split at spaces; unset, `git` on the PATH.
/// On the Mac Studio that is git 2.50.1, the version slates-git's golden vectors were made by.
const STOCK_GIT: &str = "SLATES_TEST_GIT";

/// Format: the start of `git --version`'s answer.
const GIT_VERSION: &str = "git version ";

/// Format: the user name a slates leg gives where its route asks for one (Basic's, the SSH login); slates takes the
/// principal from the credential, never from this name.
const USER: &str = "git";

/// Format: the repository each slates leg makes, which each route's URL names.
const REPOSITORY: &str = "project";

/// Format: the branch the round trip commits on: the one `slates repository create` binds a git repository's root to
/// when none is named (A-685), and the one the control's bare repository is made with.
const BRANCH: &str = "main";

/// Format: the file each session commits, at the repository's root.
const FILE: &str = "file";

/// Format: the status git's `die` exits with (`usage.c`: `exit(128)`).
const GIT_DIE: i32 = 128;

/// Format: why git, given no terminal to prompt on, could not read the user name a `401` asked it for (`prompt.c`,
/// `git_prompt`).
const PROMPTS_DISABLED: &str = "terminal prompts disabled";

/// Shape: the shards of each test's daemon, as `tests/cli.rs` starts one.
const SHARDS: &str = "2";

/// Shape: consecutive answered verbs before a test's daemon counts as settled, as `tests/cli.rs`'s start waits: an
/// anchor restarts a daemon whose first heartbeat lapsed, and a verb sent in that window is not answered.
const STABLE_STREAK: u32 = 10;

/// Shape: how long a daemon gets to answer verbs steadily once it serves, and one stock git command to end: a guard
/// against a server that never answers, far above what any command of these sessions takes, never a measure of one.
const PATIENCE: Duration = Duration::from_secs(120);

/// Shape: the pause between polls of a settling daemon and of a running git.
const POLL: Duration = Duration::from_millis(20);

/// Format: the start of the line a daemon prints once it serves (`crates/api/src/role/daemon.rs`); its instance
/// follows.
const SERVING_LINE: &str = "slates daemon: instance ";

/// Format: the anchor's line when its supervision finds its daemon in a crash loop (`crates/api/src/role/anchor.rs`,
/// A-119): a daemon that can never serve shows here, never as the anchor's exit.
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";

/// Format: the start of the anchor's line naming its issuer surface (`crates/api/src/role/anchor.rs`): the variables a
/// separate process exports to prove the human's authority, `NAME=VALUE` pairs separated by spaces.
const ISSUER_SURFACE_LINE: &str = "slates anchor: issuer surface: export ";

/// Shape: whether the anchor names its issuer surface through the environment: on every platform since A-754 (macOS
/// and Windows the segment's name, Linux its issuer route and secret; `tests/cli.rs`, `PRINTS_ISSUER_SURFACE`).
const PRINTS_ISSUER_SURFACE: bool = true;

/// Format: the start of the daemon's line naming where its HTTP edge serves cleartext, on the loopback address a test
/// asks for (`crates/api/src/role/daemon.rs`, A-625 part 3); the port follows.
const VCS_HTTP_LINE: &str =
  "slates daemon: the version-control HTTP edge serves cleartext on 127.0.0.1:";

/// Format: the start of the daemon's line naming where its SSH edge serves (A-684, `crates/api/src/role/daemon.rs`);
/// the port follows, then [`VCS_SSH_KEY_WORDS`] and the host key's SHA256 fingerprint.
const VCS_SSH_LINE: &str = "slates daemon: the version-control SSH edge serves on 127.0.0.1:";

/// Format: the words between the port and the host key's fingerprint on the daemon's SSH edge line ([`VCS_SSH_LINE`]):
/// the key's type as OpenSSH writes it in a fingerprint's line, before the fingerprint.
const VCS_SSH_KEY_WORDS: &str = " with the ED25519 host key ";

/// Format: the key type `ssh-keyscan` asks the SSH edge for (`-t`), the type [`VCS_SSH_KEY_WORDS`] names.
const VCS_SSH_KEY_TYPE: &str = "ed25519";

/// A scratch directory in the build output (`CARGO_TARGET_TMPDIR`, under `target/`), named with the process id and a
/// count, removed when dropped, even when an assertion fails (A-50: a test's host files live in the build output, never
/// `/tmp`). Its path is written with forward slashes, which every stock client on every platform reads (Git for Windows,
/// MSYS2's OpenSSH and Mercurial's `ssh` command on Windows among them), and without Windows' `\\?\` prefix, which
/// they do not.
struct Scratch {
  path: String,
}

impl Scratch {
  /// A fresh scratch directory whose name begins with `prefix`.
  fn new(prefix: &str) -> Scratch {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let raw = format!(
      "{}/{prefix}-{}-{}",
      env!("CARGO_TARGET_TMPDIR"),
      std::process::id(),
      NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    made_directory(&raw);
    Scratch {
      path: portable(&std::fs::canonicalize(&raw).unwrap()),
    }
  }
}

impl Drop for Scratch {
  // The test's own scratch directory, inside the build output (R1's wall is shipped code's).
  #[allow(clippy::disallowed_methods)]
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

/// Makes directory `path` and any parent it lacks, inside the test's build output.
#[allow(clippy::disallowed_methods)]
fn made_directory(path: &str) {
  std::fs::create_dir_all(path).unwrap_or_else(|error| panic!("making {path}: {error}"));
}

/// `path` as every stock client reads it: forward slashes, and on Windows no `\\?\` verbatim prefix, which
/// `canonicalize` adds and Git for Windows and MSYS2's tools refuse.
fn portable(path: &std::path::Path) -> String {
  let text = path.to_string_lossy();
  text
    .strip_prefix(r"\\?\")
    .unwrap_or(&text)
    .replace('\\', "/")
}

/// Stock git's command, or `None` when this host runs none, which the caller skips loudly: the words [`STOCK_GIT`]
/// names, else `git`, answering `--version` as git does. The version is printed, so a run's log says which git it met.
fn stock_git() -> Option<Vec<String>> {
  let line = std::env::var(STOCK_GIT).unwrap_or_else(|_| "git".to_owned());
  let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
  let (program, leading) = words.split_first()?;
  let version = Command::new(program)
    .args(leading)
    .arg("--version")
    .output()
    .ok()?;
  let text = String::from_utf8_lossy(&version.stdout).into_owned();
  if !version.status.success() || !text.starts_with(GIT_VERSION) {
    return None;
  }
  eprintln!("stock git: {}", text.trim());
  Some(words)
}

/// Whether this host runs OpenSSH's `ssh`, which git runs for an `ssh://` URL.
fn openssh() -> bool {
  Command::new("ssh")
    .arg("-V")
    .output()
    .is_ok_and(|output| output.status.success())
}

/// Stock git as one session's client: its copies in the test's scratch directory, each name beginning with the
/// session's; its home there too, so no user's configuration, `.netrc` or SSH configuration is read; no system's or
/// user's configuration file at all, so no credential helper; never a prompt; one committer; C's messages; and the
/// route's own `-c` words (its credential, its SSH command) before every command.
struct Git {
  /// The command that runs stock git, its words.
  program: Vec<String>,
  /// The scratch directory.
  scratch: String,
  /// The session's name.
  session: String,
  /// git's home for the session.
  home: String,
  /// The `-c` words every command takes before its subcommand.
  config: Vec<String>,
}

impl Git {
  /// The client of session `session` in `scratch`, run as `program`, with the route's `config` words.
  fn new(program: &[String], scratch: &Scratch, session: &str, config: Vec<String>) -> Git {
    let home = format!("{}/{session}-home", scratch.path);
    made_directory(&home);
    Git {
      program: program.to_vec(),
      scratch: scratch.path.clone(),
      session: session.to_owned(),
      home,
      config,
    }
  }

  /// The path of the session's copy `name`.
  fn copy(&self, name: &str) -> String {
    format!("{}/{}-{name}", self.scratch, self.session)
  }

  /// Runs git with `args` in `dir`, killed past [`PATIENCE`]: its exit code and both outputs.
  fn run(&self, dir: &str, args: &[&str]) -> (i32, String, String) {
    let (program, leading) = self.program.split_first().expect("a command");
    let mut child = Command::new(program)
      .args(leading)
      .args(&self.config)
      .args(args)
      .current_dir(dir)
      .envs(Self::environment(&self.home))
      .env_remove("GIT_ASKPASS")
      .env_remove("SSH_ASKPASS")
      .env_remove("SSH_AUTH_SOCK")
      .env_remove("GIT_SSH")
      .env_remove("GIT_SSH_COMMAND")
      .env_remove("GIT_CONFIG_PARAMETERS")
      .env_remove("GIT_DIR")
      .env_remove("GIT_WORK_TREE")
      .env_remove("GIT_PROTOCOL")
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let out = read_on_a_thread(child.stdout.take().unwrap());
    let err = read_on_a_thread(child.stderr.take().unwrap());
    let code = exit_within(&mut child, &format!("git {args:?} in {dir}"));
    (code, out.join().unwrap(), err.join().unwrap())
  }

  /// The variables every command runs with (the type's doc), git's home `home`.
  fn environment(home: &str) -> [(&'static str, String); 10] {
    [
      ("HOME", home.to_owned()),
      ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
      ("GIT_CONFIG_GLOBAL", "/dev/null".to_owned()),
      ("GIT_CONFIG_COUNT", "0".to_owned()),
      ("GIT_TERMINAL_PROMPT", "0".to_owned()),
      ("LC_ALL", "C".to_owned()),
      ("GIT_AUTHOR_NAME", "Test".to_owned()),
      ("GIT_AUTHOR_EMAIL", "test@example.com".to_owned()),
      ("GIT_COMMITTER_NAME", "Test".to_owned()),
      ("GIT_COMMITTER_EMAIL", "test@example.com".to_owned()),
    ]
  }

  /// Runs git as [`Git::run`] does, failing the test unless it succeeds: its standard output.
  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let (code, out, err) = self.run(dir, args);
    assert_eq!(code, 0, "git {args:?} in {dir}: {out}{err}");
    out
  }

  /// The commit `name` names in the repository at `dir`, as `git rev-parse` answers.
  fn commit_of(&self, dir: &str, name: &str) -> String {
    self.ok(dir, &["rev-parse", name]).trim().to_owned()
  }
}

/// Reads `from` to its end on a thread of its own, so a child that fills one pipe never waits on the other's reader.
fn read_on_a_thread(mut from: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
  std::thread::spawn(move || {
    let mut bytes = Vec::new();
    let _ = from.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
  })
}

/// Waits for `child` to end within [`PATIENCE`], killing it and failing the test past that: its exit code, -1 where a
/// signal ended it.
fn exit_within(child: &mut Child, what: &str) -> i32 {
  let started = Instant::now();
  loop {
    if let Some(status) = child.try_wait().unwrap() {
      return status.code().unwrap_or(-1);
    }
    if started.elapsed() > PATIENCE {
      let _ = child.kill();
      let _ = child.wait();
      panic!("{what} ran past {PATIENCE:?} and was killed");
    }
    pause();
  }
}

fn pause() {
  // The harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

/// Writes `text` as copy `copy`'s [`FILE`]: the test writes the file stock git commits, inside the scratch directory it
/// removes (R1's wall is shipped code's).
#[allow(clippy::disallowed_methods)]
fn put(copy: &str, text: &str) {
  std::fs::write(format!("{copy}/{FILE}"), text).unwrap();
}

/// Fails the test unless copy `copy`'s [`FILE`] holds `text`.
fn expect_text(copy: &str, text: &str) {
  let held = std::fs::read_to_string(format!("{copy}/{FILE}")).unwrap();
  assert_eq!(held, text, "{copy}/{FILE}");
}

/// The round trip's first step, run as git against `url`, an empty repository: a fresh repository made on [`BRANCH`]
/// in a first copy, [`FILE`] committed there, and the branch pushed, the repository's first push. The commit.
fn first_push(git: &Git, url: &str) -> String {
  let first = git.copy("first");
  git.ok(&git.scratch, &["init", "-q", "-b", BRANCH, &first]);
  put(&first, "first\n");
  git.ok(&first, &["add", FILE]);
  git.ok(&first, &["commit", "-q", "-m", "first"]);
  git.ok(&first, &["push", "-q", url, BRANCH]);
  git.commit_of(&first, "HEAD")
}

/// The round trip's second step: `url` cloned into a second copy. Fails the test unless the clone checks out
/// [`BRANCH`] at `pushed`, the first push's commit, and holds the file that commit wrote.
fn cloned(git: &Git, url: &str, pushed: &str) {
  let second = git.copy("second");
  git.ok(&git.scratch, &["clone", "-q", url, &second]);
  let branch = git.ok(&second, &["symbolic-ref", "HEAD"]);
  let checked_out = (branch.trim().to_owned(), git.commit_of(&second, "HEAD"));
  let expected = (format!("refs/heads/{BRANCH}"), pushed.to_owned());
  assert_eq!(
    checked_out, expected,
    "the clone checks out the pushed branch"
  );
  expect_text(&second, "first\n");
}

/// The round trip's third step: an edit committed in the second copy and pushed to the branch it was cloned from, with
/// no URL or branch named, as a clone's push goes. The new commit.
fn pushed_from_the_clone(git: &Git) -> String {
  let second = git.copy("second");
  put(&second, "first\nsecond\n");
  git.ok(&second, &["commit", "-q", "-a", "-m", "second"]);
  git.ok(&second, &["push", "-q"]);
  git.commit_of(&second, "HEAD")
}

/// The round trip's last step: [`BRANCH`] fetched from `url` into the first copy, which fast-forwards to it. Fails the
/// test unless the fetch brings `pushed`, the clone's push, and the first copy then stands at it, two commits long,
/// holding the clone's edit.
fn fetched_back(git: &Git, url: &str, pushed: &str) {
  let first = git.copy("first");
  git.ok(&first, &["fetch", "-q", url, BRANCH]);
  let fetched = git.commit_of(&first, "FETCH_HEAD");
  git.ok(&first, &["merge", "-q", "--ff-only", "FETCH_HEAD"]);
  let length = git.ok(&first, &["rev-list", "--count", "HEAD"]);
  let stands = (
    fetched,
    git.commit_of(&first, "HEAD"),
    length.trim().to_owned(),
  );
  let expected = (pushed.to_owned(), pushed.to_owned(), "2".to_owned());
  assert_eq!(stands, expected, "the fetch brings the clone's push back");
  expect_text(&first, "first\nsecond\n");
}

/// The round trip, run as git against `url`, an empty repository whose default branch is [`BRANCH`]: a first commit
/// pushed from a fresh repository ([`first_push`]); cloned into a second copy ([`cloned`]); an edit pushed from the
/// clone ([`pushed_from_the_clone`]); fetched back into the first copy ([`fetched_back`]). Fails the test unless each
/// step answers as stock git's own server answers it.
fn round_trip(git: &Git, url: &str) {
  let first = first_push(git, url);
  cloned(git, url, &first);
  let second = pushed_from_the_clone(git);
  fetched_back(git, url, &second);
}

/// §4.17.2's sessions, their control. Do: make a bare repository whose default branch is [`BRANCH`] with stock
/// `git init --bare` in a scratch directory; run the round trip against it as stock git over git's local transport
/// (`file://`). Expect: every step as [`round_trip`] states. That makes the session stock git's own behaviour, so each
/// slates leg below is held to what git's own server does. Skips loudly without stock git ([`STOCK_GIT`]).
#[test]
fn stock_git_round_trip_holds_against_stock_git() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping the control's round trip: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let scratch = Scratch::new("git-control");
  let git = Git::new(&program, &scratch, "control", Vec::new());
  let bare = format!("{}/bare.git", scratch.path);
  git.ok(
    &scratch.path,
    &["init", "-q", "--bare", "-b", BRANCH, &bare],
  );
  round_trip(&git, &file_url(&bare));
}

/// Git's local URL of the directory at `path`, a forward-slashed absolute path: `file:///C:/…` for a Windows path
/// (RFC 8089's form, which Git for Windows reads), `file:///…` for a Unix one.
fn file_url(path: &str) -> String {
  if path.starts_with('/') {
    format!("file://{path}")
  } else {
    format!("file:///{path}")
  }
}

/// The anchor process, killed and reaped when dropped, so a failed assertion leaves no daemon behind; what it says
/// after its daemon served is copied to this process's standard error by `drain`, which the drop joins.
struct Anchor {
  child: Child,
  drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    if let Some(drain) = self.drain.take() {
      let _ = drain.join();
    }
  }
}

/// A started anchor: the process, the lines it and its daemon said before the daemon served (which name the edges'
/// ports), and its issuer surface's variables, where this platform names them ([`PRINTS_ISSUER_SURFACE`]).
struct Started {
  anchor: Anchor,
  /// The daemon's changeset store (`--vcs-store`), which a git repository needs (C5; main's ruling of 2026-11-20: a
  /// daemon without one refuses to create one); declared after the anchor, so the daemon is gone before it is removed.
  _store: Scratch,
  lines: Vec<String>,
  issuer: Option<Vec<(String, String)>>,
}

/// Runs `slates --instance INSTANCE ARGS` with `issuer`'s variables in its environment: its exit code and both
/// outputs.
fn slates(instance: &str, issuer: &[(String, String)], args: &[&str]) -> (i32, String, String) {
  let output = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args(["--instance", instance])
    .args(args)
    .envs(issuer.iter().map(|(name, value)| (name, value)))
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// The value on `text`'s line `key: value`.
fn value_of(text: &str, key: &str) -> String {
  let prefix = format!("{key}: ");
  text
    .lines()
    .find_map(|line| line.strip_prefix(&prefix))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .trim()
    .to_owned()
}

/// Starts `slates anchor` whose daemon is asked for `edges` (a flag and a loopback address whose port the system
/// picks), reads what the anchor says until its daemon serves and, where this platform names one, the anchor has named
/// its issuer surface ([`lines_until_served`]), waits until the daemon answers verbs steadily, and makes the root
/// consensus group as a first start does (`slates bootstrap root`).
fn start_anchor(instance: &str, edges: &[&str]) -> Started {
  let store = Scratch::new("vcs-store");
  let mut child = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .args(edges)
    .args(["--vcs-store", &store.path])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let mut anchor = Anchor { child, drain: None };
  let (lines, issuer) = lines_until_served(&mut stderr, instance);
  anchor.drain = Some(std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  }));
  settle(instance, anchor.child.id());
  let (code, out, err) = slates(instance, &[], &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap root: {out}{err}");
  Started {
    anchor,
    _store: store,
    lines,
    issuer,
  }
}

/// Reads the anchor's lines until its daemon says it serves and, where this platform names one, the anchor has named
/// its issuer surface, taking the two processes' lines in whichever order they arrive, and echoing each to this
/// process's standard error, where they went before it was piped: the lines read, and the surface's variables. Ends
/// the test where the anchor ends first or reports its daemon in a crash loop, the anchor's own bound on a daemon that
/// cannot serve.
fn lines_until_served(
  stderr: &mut impl BufRead,
  instance: &str,
) -> (Vec<String>, Option<Vec<(String, String)>>) {
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (mut lines, mut issuer, mut served) = (Vec::new(), None, false);
  while !served || (PRINTS_ISSUER_SURFACE && issuer.is_none()) {
    let mut line = String::new();
    let read = stderr.read_line(&mut line).unwrap();
    assert!(
      read > 0,
      "the anchor ended before its daemon served (its lines are above)"
    );
    let _ = std::io::stderr().write_all(line.as_bytes());
    assert!(
      !line.starts_with(CRASH_LOOP_LINE),
      "the daemon never served: {line}"
    );
    served |= line.starts_with(&serving);
    issuer = issuer.or_else(|| issuer_surface(&line));
    lines.push(line);
  }
  (lines, issuer)
}

/// The variables of the anchor's issuer-surface line ([`ISSUER_SURFACE_LINE`]), or `None` for any other line.
fn issuer_surface(line: &str) -> Option<Vec<(String, String)>> {
  let pairs = line.trim().strip_prefix(ISSUER_SURFACE_LINE)?;
  Some(
    pairs
      .split(' ')
      .filter_map(|pair| pair.split_once('='))
      .map(|(name, value)| (name.to_owned(), value.to_owned()))
      .collect(),
  )
}

/// Waits until the daemon answers [`STABLE_STREAK`] verbs in a row, while the tree under its anchor `anchor` gains
/// CPU ([`process_wait::Judged`]; until 2026-11-20 within [`PATIENCE`]).
fn settle(instance: &str, anchor: u32) {
  let mut judged = process_wait::Judged::of(anchor);
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if slates(instance, &[], &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    judged.look("the daemon answered verbs in a row");
    pause();
  }
}

/// The port on the first of `lines` that begins with `prefix`: the digits right after it.
fn port_after(lines: &[String], prefix: &str) -> Option<u16> {
  lines.iter().find_map(|line| {
    let rest = line.strip_prefix(prefix)?;
    rest
      .split(|c: char| !c.is_ascii_digit())
      .next()?
      .parse()
      .ok()
  })
}

/// The HTTP edge's port the daemon named in `lines`; a test fails when it named none.
fn http_edge_port(lines: &[String]) -> u16 {
  port_after(lines, VCS_HTTP_LINE).expect("the daemon named its HTTP edge's port")
}

/// Makes git repository [`REPOSITORY`] through the CLI, in A-685's words: its object format SHA-1 and its default
/// branch [`BRANCH`], the verb's defaults.
fn create_repository(instance: &str) {
  let (code, out, err) = slates(
    instance,
    &[],
    &["repository", "create", REPOSITORY, "--tool", "git"],
  );
  assert_eq!(code, 0, "repository create: {out}{err}");
}

/// The issuer surface `started` named, or `None` where this platform names none, which the caller skips loudly.
fn issuer_of(started: &Started, leg: &str) -> Option<Vec<(String, String)>> {
  if started.issuer.is_none() {
    eprintln!(
      "skipping {leg}: the anchor printed no issuer surface, so no \
       separate process can mint a credential"
    );
  }
  started.issuer.clone()
}

/// A token `slates credential token` mints, under the issuer surface `issuer`, for this process's own principal, the
/// repository's owner.
fn minted_token(instance: &str, issuer: &[(String, String)]) -> String {
  let (code, out, err) = slates(instance, issuer, &["credential", "token"]);
  assert_eq!(code, 0, "credential token: {out}{err}");
  value_of(&out, "token")
}

/// One HTTP leg: the anchor its daemon serves from, the edge's port, and a token for the repository's owner.
struct HttpLeg {
  _anchor: Anchor,
  instance: String,
  port: u16,
  token: String,
}

/// The HTTP leg of session `session`: an anchor with its HTTP edge on loopback (`--vcs-http 127.0.0.1:0`), repository
/// [`REPOSITORY`] made, and a token minted; `None` where this platform names no issuer surface, which [`issuer_of`]
/// says.
fn http_leg(session: &str) -> Option<HttpLeg> {
  let instance = format!("git-http-{session}-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let issuer = issuer_of(&started, "the HTTP leg")?;
  create_repository(&instance);
  let token = minted_token(&instance, &issuer);
  Some(HttpLeg {
    _anchor: started.anchor,
    instance,
    port,
    token,
  })
}

/// §4.17.2 "Transports" item 1 and A-625 part 3 (the edge refuses a request with no credential `401` before it looks
/// for any repository). Do: start an anchor with its HTTP edge on loopback (`--vcs-http 127.0.0.1:0`); ask stock git,
/// given no credential and no terminal, for the refs of [`REPOSITORY`] there (`git ls-remote`). Expect it refused in its
/// own words, git's `die` status (128) and `could not read Username for 'http://127.0.0.1:PORT': terminal prompts
/// disabled`: git met the edge's `401`, asked for credentials as it asks any server's, and had no way to read one. Skips
/// loudly without stock git ([`STOCK_GIT`]).
#[test]
fn stock_git_without_credentials_is_refused_in_its_own_words() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping stock git's refusal: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let instance = format!("git-refused-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let scratch = Scratch::new("git-refused");
  let git = Git::new(&program, &scratch, "refused", Vec::new());
  let site = format!("http://127.0.0.1:{port}");
  let (code, out, err) = git.run(
    &scratch.path,
    &["ls-remote", &format!("{site}/{REPOSITORY}")],
  );
  let words = format!("fatal: could not read Username for '{site}': {PROMPTS_DISABLED}");
  assert_eq!(
    (code, err.lines().any(|line| line == words)),
    (GIT_DIE, true),
    "{out}{err}"
  );
}

/// §4.17.2 "A fetch", "A push" and "Transports" item 1, with A-682 (upload-pack served from the repository's store on
/// its owner shard) and A-654 part 3 (receive-pack through the push driver). Do: start an anchor with its HTTP edge on
/// loopback; make repository [`REPOSITORY`] through the CLI (`slates repository create`, A-685) and a token for its
/// owner (`slates credential token`); run the round trip as stock git at `http://127.0.0.1:PORT/project`, every
/// request carrying the token as a bearer credential (`http.extraHeader`). Expect every step as [`round_trip`] states,
/// as against stock git's own server. Skips loudly without stock git ([`STOCK_GIT`]), and on a platform whose issuer
/// surface a separate process cannot use.
#[test]
fn stock_git_clones_pushes_and_fetches_over_the_vcs_http_edge_with_a_bearer_token() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping stock git over the HTTP edge: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let Some(leg) = http_leg("bearer") else {
    return;
  };
  let scratch = Scratch::new("git-http-bearer");
  let header = format!("http.extraHeader=Authorization: Bearer {}", leg.token);
  let git = Git::new(&program, &scratch, "bearer", vec!["-c".to_owned(), header]);
  round_trip(&git, &format!("http://127.0.0.1:{}/{REPOSITORY}", leg.port));
}

/// §4.17.2 "A push" and "A fetch" (gitrevisions(7): a ref may name any object; git-tag(1): a tag may tag a blob, a
/// tree or another tag). Found by use: git/git's `junio-gpg-pub` tags a blob and `v1.0rc1`..`v1.0rc6` name neither a
/// commit nor a tree; pushing git/git whole to slates refused them ("names neither a commit nor a tree", `! [remote
/// rejected] v1.0rc3 -> v1.0rc3 (ref names no tree)`) and every whole clone after it failed "possible repository
/// corruption on the remote side" (w-interop's A/B on main a8ee99bf7, 2026-11-21), where stock git daemon took and
/// served them. Do: over the HTTP edge, in one push, push a branch with an annotated tag of a blob, a lightweight tag of
/// the same blob, an annotated tag of that tag, and an annotated tag of a tree; then clone the repository as a mirror.
/// Expect the push taken whole, the mirror's refs naming exactly the objects the source's do, and `git fsck --full`
/// clean in the mirror, as against stock git's own server.
#[test]
fn stock_git_pushes_and_clones_tags_of_a_blob_a_tree_and_a_tag() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping stock git's tags of a blob and of a tag: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let Some(leg) = http_leg("tags") else {
    return;
  };
  let scratch = Scratch::new("git-http-tags");
  let header = format!("http.extraHeader=Authorization: Bearer {}", leg.token);
  let git = Git::new(&program, &scratch, "tags", vec!["-c".to_owned(), header]);
  let url = format!("http://127.0.0.1:{}/{REPOSITORY}", leg.port);
  let source = git.copy("source");
  git.ok(&git.scratch, &["init", "-q", "-b", BRANCH, &source]);
  put(&source, "first\n");
  git.ok(&source, &["add", FILE]);
  git.ok(&source, &["commit", "-q", "-m", "first"]);
  let blob = git
    .ok(&source, &["hash-object", "-w", FILE])
    .trim()
    .to_owned();
  git.ok(&source, &["tag", "-a", "blob-tag", "-m", "a blob", &blob]);
  git.ok(&source, &["tag", "light-blob", &blob]);
  git.ok(
    &source,
    &[
      "tag",
      "-a",
      "tag-of-tag",
      "-m",
      "a tag of a tag",
      "blob-tag",
    ],
  );
  git.ok(
    &source,
    &["tag", "-a", "tree-tag", "-m", "a tree", "HEAD^{tree}"],
  );
  let pushed = git.run(&source, &["push", &url, BRANCH, "--tags"]);
  let refs = |dir: &str| {
    let listed = git.ok(
      dir,
      &[
        "for-each-ref",
        "--format=%(objectname) %(objecttype) %(refname)",
      ],
    );
    let mut lines: Vec<String> = listed.lines().map(str::to_owned).collect();
    lines.sort();
    lines
  };
  let mirror = git.copy("mirror");
  let cloned = git.run(&git.scratch, &["clone", "-q", "--mirror", &url, &mirror]);
  let (mirror_refs, fsck) = if cloned.0 == 0 {
    (refs(&mirror), git.run(&mirror, &["fsck", "--full"]).0)
  } else {
    (Vec::new(), -1)
  };
  assert_eq!(
    (pushed.0, cloned.0, mirror_refs, fsck),
    (0, 0, refs(&source), 0),
    "push: {}{}; clone: {}{}",
    pushed.1,
    pushed.2,
    cloned.1,
    cloned.2
  );
}

/// Format: the kinds of mapped memory `slates status --json` reports under `memory` (§4.14, A-232's gotcha): the anchor
/// segment, the anchor content object (the shards' arenas among its bytes), the shards' arenas, the clients' ring
/// regions, and the heap.
const MEMORY_KINDS: [&str; 5] = ["segment", "content", "arenas", "regions", "heap"];

/// §4.14 and A-232's gotcha (the bug record of 2026-11-20, the anchor's kills of w-hg's daemon): the daemon reports its
/// own memory by kind, read in process with nothing suspended, so no inspector need suspend it (`vmmap`, `heap`), which
/// past the anchor's window gets it killed by design. Do: start an anchor with its HTTP edge on loopback; make
/// repository [`REPOSITORY`] and a token; push stock git's first commit there while a thread asks `slates status --json
/// --memory` (the request that asks for the memory report, main's ruling on g228's finding) again and again until the
/// push ends. Expect every answer to report `memory` with each of [`MEMORY_KINDS`] once, each
/// kind's resident bytes within its reserved bytes, the segment's, the content object's and the heap's reserved bytes
/// above zero; the push to succeed; and the anchor never to have restarted the daemon (the same pid and `restarts` 0
/// at the end). Skips loudly as the round trips do.
#[test]
fn a_status_during_a_push_reports_the_daemons_memory_by_kind_while_the_anchor_keeps_it() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping the memory report during a push: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let Some(leg) = http_leg("memory") else {
    return;
  };
  let status = |instance: &str| -> serde_json::Value {
    let (code, out, err) = slates(instance, &[], &["status", "--json", "--memory"]);
    assert_eq!(code, 0, "status --json --memory: {out}{err}");
    serde_json::from_str(&out).unwrap()
  };
  let before = status(&leg.instance);
  let scratch = Scratch::new("git-http-memory");
  let header = format!("http.extraHeader=Authorization: Bearer {}", leg.token);
  let git = Git::new(&program, &scratch, "memory", vec!["-c".to_owned(), header]);
  let url = format!("http://127.0.0.1:{}/{REPOSITORY}", leg.port);
  let pushing = std::sync::atomic::AtomicBool::new(true);
  let reports = std::thread::scope(|scope| {
    let asking = scope.spawn(|| {
      let mut reports = vec![status(&leg.instance)];
      while pushing.load(std::sync::atomic::Ordering::Acquire) {
        reports.push(status(&leg.instance));
      }
      reports
    });
    first_push(&git, &url);
    pushing.store(false, std::sync::atomic::Ordering::Release);
    asking.join().unwrap()
  });
  let after = status(&leg.instance);
  for report in reports.iter().chain([&after]) {
    let memory = report["memory"]
      .as_array()
      .unwrap_or_else(|| panic!("status reports the daemon's memory: {report}"));
    for kind in MEMORY_KINDS {
      let found: Vec<_> = memory.iter().filter(|row| row["kind"] == kind).collect();
      assert_eq!(found.len(), 1, "one {kind} row: {memory:?}");
      let (reserved, resident) = (
        found[0]["reserved_bytes"].as_u64().unwrap(),
        found[0]["resident_bytes"].as_u64().unwrap(),
      );
      assert!(
        resident <= reserved,
        "{kind}: resident within reserved: {memory:?}"
      );
      if ["segment", "content", "heap"].contains(&kind) {
        assert!(reserved > 0, "{kind} reserves bytes: {memory:?}");
      }
    }
  }
  assert_eq!(
    after["pid"], before["pid"],
    "the anchor kept the daemon it started"
  );
  assert_eq!(
    after["restarts"], 0,
    "the anchor restarted nothing: {after}"
  );
}

/// Shape: the variable naming a real repository whose cut the memory test pushes in place of its made history (a
/// git/git mirror, say), with [`HISTORY_CUT_VARIABLE`]; unset, the made history of [`HISTORY_COMMITS`] commits.
#[cfg(unix)]
const HISTORY_SOURCE_VARIABLE: &str = "SLATES_TEST_GIT_SOURCE";

/// Shape: the variable naming the revision of [`HISTORY_SOURCE_VARIABLE`]'s repository the memory test pushes
/// (`v2.0.0`, the cut w-interop's 7,421 MiB was measured on); its commit is pushed as [`BRANCH`].
#[cfg(unix)]
const HISTORY_CUT_VARIABLE: &str = "SLATES_TEST_GIT_CUT";

/// Shape: the files of the memory test's histories, in [`HISTORY_DIRECTORIES`] directories: a source tree's count
/// (git/git v2.0.0 holds about 2,500), so each version's tree, and whatever a server keeps per path or per object,
/// is large beside the change one commit brings.
#[cfg(unix)]
const HISTORY_FILES: usize = 2_000;

/// Shape: the directories the memory test's files lie in, each its own tree, as a source tree nests its files.
#[cfg(unix)]
const HISTORY_DIRECTORIES: usize = 20;

/// Shape: the lines of each of the memory test's files, [`HISTORY_LINE_BYTES`] each (half a KiB a file).
#[cfg(unix)]
const HISTORY_FILE_LINES: usize = 16;

/// Shape: the bytes of each line of the memory test's files (`line NN of file NNNNN` written to this width).
#[cfg(unix)]
const HISTORY_LINE_BYTES: usize = 32;

/// Shape: the commits of the memory test's measured history after its first, which adds every file: each rewrites one
/// line of one file, as most commits of a source history do, so the pack stock git sends grows by a delta a commit
/// while the history's versions, trees and objects grow by a whole tree's worth of paths each.
#[cfg(unix)]
const HISTORY_COMMITS: usize = 200;

/// Shape: the commits of the memory test's warm-up history after its first, the same shape: what a push and a clone of
/// the tree cost the daemon whatever the history brings (the edge's buffers, a repository's store, the shards' first
/// pages, one version of the tree).
#[cfg(unix)]
const WARM_COMMITS: usize = 2;

/// Makes repository `name` in the memory test's daemon (`slates repository create`, A-685).
#[cfg(unix)]
fn create_named(instance: &str, name: &str) {
  let (code, out, err) = slates(
    instance,
    &[],
    &["repository", "create", name, "--tool", "git"],
  );
  assert_eq!(code, 0, "repository create {name}: {out}{err}");
}

/// Line `line` of file `file` as commit `commit` (`None` for the first) left it, [`HISTORY_LINE_BYTES`] wide.
#[cfg(unix)]
fn history_line(file: usize, line: usize, commit: Option<usize>) -> String {
  let words = match commit {
    Some(commit) => format!("line {line:02} of file {file:05} in {commit:05}"),
    None => format!("line {line:02} of file {file:05}"),
  };
  format!("{words:<width$}\n", width = HISTORY_LINE_BYTES - 1)
}

/// The path of file `file` in the memory test's tree: `dNN/fNNNNN`, spread over [`HISTORY_DIRECTORIES`].
#[cfg(unix)]
fn history_path(file: usize) -> String {
  format!("d{:02}/f{file:05}", file % HISTORY_DIRECTORIES)
}

/// Makes a history on [`BRANCH`] in a fresh repository at `dir` with stock `git fast-import`: a first commit adding
/// [`HISTORY_FILES`] files ([`history_path`], [`HISTORY_FILE_LINES`] lines each), then `commits` commits each
/// rewriting one line of one file ([`history_line`]); then packs it with `git repack -a -d`: the bytes of the pack
/// stock git made of the history, which a push sends near enough (both deltify each version against the one before).
#[allow(clippy::disallowed_methods)]
#[cfg(unix)]
fn made_history(git: &Git, dir: &str, commits: usize) -> u64 {
  git.ok(&git.scratch, &["init", "-q", "-b", BRANCH, dir]);
  let (program, leading) = git.program.split_first().expect("a command");
  let mut importer = Command::new(program)
    .args(leading)
    .args(["fast-import", "--quiet"])
    .current_dir(dir)
    .envs(Git::environment(&git.home))
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stream = std::io::BufWriter::new(importer.stdin.take().unwrap());
  let mut files: Vec<Vec<String>> = (0..HISTORY_FILES)
    .map(|file| {
      (0..HISTORY_FILE_LINES)
        .map(|line| history_line(file, line, None))
        .collect()
    })
    .collect();
  for commit in 0..=commits {
    let changed: Vec<usize> = if commit == 0 {
      (0..HISTORY_FILES).collect()
    } else {
      let file = commit.wrapping_mul(7919) % HISTORY_FILES;
      let line = commit % HISTORY_FILE_LINES;
      files[file][line] = history_line(file, line, Some(commit));
      vec![file]
    };
    let message = format!("commit {commit}\n");
    write!(
      stream,
      "commit refs/heads/{BRANCH}\ncommitter Test <test@example.com> {} +0000\ndata {}\n{message}",
      1_700_000_000 + commit,
      message.len()
    )
    .unwrap();
    for file in changed {
      let content = files[file].concat();
      write!(
        stream,
        "M 100644 inline {}\ndata {}\n{content}\n",
        history_path(file),
        content.len()
      )
      .unwrap();
    }
  }
  drop(stream);
  let err = read_on_a_thread(importer.stderr.take().unwrap());
  let code = exit_within(&mut importer, "git fast-import");
  assert_eq!(code, 0, "git fast-import: {}", err.join().unwrap());
  git.ok(dir, &["repack", "-a", "-d", "-q"]);
  let packs = format!("{dir}/.git/objects/pack");
  std::fs::read_dir(&packs)
    .unwrap()
    .filter_map(Result::ok)
    .filter(|entry| {
      entry
        .path()
        .extension()
        .is_some_and(|extension| extension == "pack")
    })
    .map(|entry| entry.metadata().unwrap().len())
    .sum()
}

/// The daemon's pid, as `slates status --json` names it.
#[cfg(unix)]
fn daemon_pid(instance: &str) -> String {
  let (code, out, err) = slates(instance, &[], &["status", "--json"]);
  assert_eq!(code, 0, "status --json: {out}{err}");
  let report: serde_json::Value = serde_json::from_str(&out).unwrap();
  report["pid"]
    .as_u64()
    .unwrap_or_else(|| panic!("status names the daemon's pid: {report}"))
    .to_string()
}

/// The bytes of the pack stock git makes of everything `cut` reaches in the repository at `dir` (`git pack-objects
/// --revs --stdout`), which a first push of `cut` sends near enough.
#[cfg(unix)]
fn pack_bytes(git: &Git, dir: &str, cut: &str) -> u64 {
  let (program, leading) = git.program.split_first().expect("a command");
  let mut packer = Command::new(program)
    .args(leading)
    .args(["pack-objects", "--revs", "--stdout", "-q"])
    .current_dir(dir)
    .envs(Git::environment(&git.home))
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let mut asked = packer.stdin.take().unwrap();
  writeln!(asked, "{cut}").unwrap();
  drop(asked);
  let mut pack = packer.stdout.take().unwrap();
  let counted = std::thread::spawn(move || std::io::copy(&mut pack, &mut std::io::sink()).unwrap());
  let code = exit_within(&mut packer, "git pack-objects");
  assert_eq!(code, 0, "git pack-objects --revs of {cut} in {dir}");
  counted.join().unwrap()
}

/// Process `pid`'s resident set, in bytes, as `ps` reports it.
#[cfg(unix)]
fn resident_of(pid: &str) -> u64 {
  let output = Command::new("ps")
    .args(["-o", "rss=", "-p", pid])
    .output()
    .unwrap();
  let kib: u64 = String::from_utf8_lossy(&output.stdout)
    .trim()
    .parse()
    .unwrap();
  kib * 1024
}

/// The daemon `pid`'s resident set once it has settled: read until two reads in a row agree (the shards' idle
/// give-back runs between them), within [`PATIENCE`].
#[cfg(unix)]
fn settled_resident(pid: &str) -> u64 {
  let started = Instant::now();
  let mut last = resident_of(pid);
  loop {
    pause();
    let now = resident_of(pid);
    if now == last || started.elapsed() > PATIENCE {
      return now;
    }
    last = now;
  }
}

/// Runs `work` while a thread reads the daemon `pid`'s resident set every [`POLL`]: what `work` returned, and the
/// most resident read. A sampled peak can only read under the true one, so a peak past a bound is past it.
#[cfg(unix)]
fn peak_while<T>(pid: &str, work: impl FnOnce() -> T) -> (T, u64) {
  let working = std::sync::atomic::AtomicBool::new(true);
  std::thread::scope(|scope| {
    let sampler = scope.spawn(|| {
      let mut most = resident_of(pid);
      while working.load(std::sync::atomic::Ordering::Acquire) {
        most = most.max(resident_of(pid));
        pause();
      }
      most.max(resident_of(pid))
    });
    let done = work();
    working.store(false, std::sync::atomic::Ordering::Release);
    (done, sampler.join().unwrap())
  })
}

/// Prints the daemon's memory gauges (`slates status --memory`'s `mem.gauge.*` rows, `crate::memory_gauges` of
/// slates-server) labelled `when`: what holds its memory by its own accounting at each reading, so a resident set past the
/// bound names its holders (main 01a20182's ask of 2026-11-22 06:28).
#[cfg(unix)]
fn print_gauges(instance: &str, when: &str) {
  let (_, report, _) = slates(instance, &[], &["status", "--memory"]);
  let rows: Vec<&str> = report
    .lines()
    .filter(|line| line.contains("mem.gauge.") || line.starts_with("memory: "))
    .collect();
  eprintln!("gauges at {when}:\n  {}", rows.join("\n  "));
}

/// What one round of the memory test measured: the pack's bytes, the daemon's peak resident set while its push and its
/// clone ran, and its settled resident set after each.
#[cfg(unix)]
struct Measured {
  pack: u64,
  peak: u64,
  pushed: u64,
  cloned: u64,
}

/// Ada's MEMORY SIZING ruling (2026-11-20; A-830, A-767 item 1: "changes in RAM, the repository's history on disk"):
/// the git server's resident memory follows the change it is handed, never the history it keeps. w-interop's release
/// run (r2, base 26c98c76d) held 7,421 MiB at its peak against git/git v2.0.0's 51.3 MiB pack. Do: start an anchor with
/// its HTTP edge on loopback; make a warm-up repository and push and clone a history of [`WARM_COMMITS`] commits of the
/// measured shape ([`made_history`]), reading the daemon's resident set at the start, while they run ([`peak_while`])
/// and settled after ([`settled_resident`]): `W`, the range it moved over, from its least to its peak, is what a push
/// and a clone cost and the noise of reading them, whatever they bring. Then make [`REPOSITORY`] and push to it a
/// history of [`HISTORY_FILES`] files and [`HISTORY_COMMITS`] commits each rewriting one line of one file, or, where
/// [`HISTORY_SOURCE_VARIABLE`] and [`HISTORY_CUT_VARIABLE`] name them, a real repository's cut (its pack `N` bytes),
/// and clone it back. Expect: the push and the clone succeed, the clone at the pushed head; and the daemon's resident
/// set at its peak while they ran, settled after the push and settled after the clone, each at most the warm-up's end
/// plus `2N + W`: the pack held once as the change in flight and once more as derived data cached by identity (A-767
/// item 1). Red where the daemon keeps the history expanded, or something per object or per path of every version. The
/// made history is a focused run's size (passed on current main, this Mac, debug, 2026-11-21: N 258,770 bytes, the peak
/// and the settled sets at most 68.2 MiB against a bound of 90.5 MiB); git/git's cut is main's measurement run
/// (run-requests.md). Unix (`ps`); skips loudly without stock git.
#[cfg(unix)]
#[test]
fn the_daemons_resident_memory_after_a_push_and_a_clone_follows_the_pack_not_the_history() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping the git server's memory bound: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let Some(leg) = http_leg("resident") else {
    return;
  };
  let scratch = Scratch::new("git-http-resident");
  let header = format!("http.extraHeader=Authorization: Bearer {}", leg.token);
  let git = Git::new(
    &program,
    &scratch,
    "resident",
    vec!["-c".to_owned(), header],
  );
  let site = format!("http://127.0.0.1:{}", leg.port);
  let pid = daemon_pid(&leg.instance);
  let real = std::env::var(HISTORY_SOURCE_VARIABLE)
    .ok()
    .zip(std::env::var(HISTORY_CUT_VARIABLE).ok());
  let round = |name: &str, commits: usize, real: Option<&(String, String)>| -> Measured {
    if name != REPOSITORY {
      create_named(&leg.instance, name);
    }
    let url = format!("{site}/{name}");
    let (source, pack, refspec) = match real {
      Some((source, cut)) => (
        source.clone(),
        pack_bytes(&git, source, cut),
        format!("{cut}^{{commit}}:refs/heads/{BRANCH}"),
      ),
      None => {
        let source = git.copy(&format!("{name}-source"));
        let pack = made_history(&git, &source, commits);
        (source, pack, BRANCH.to_owned())
      }
    };
    let ((), push_peak) = peak_while(&pid, || {
      git.ok(&source, &["push", "-q", &url, &refspec]);
    });
    let pushed = settled_resident(&pid);
    print_gauges(&leg.instance, &format!("{name} after the push"));
    let clone = git.copy(&format!("{name}-clone"));
    let ((), clone_peak) = peak_while(&pid, || {
      git.ok(&git.scratch, &["clone", "-q", &url, &clone]);
    });
    let pushed_head = match real {
      Some((_, cut)) => git.commit_of(&source, &format!("{cut}^{{commit}}")),
      None => git.commit_of(&source, "HEAD"),
    };
    assert_eq!(
      git.commit_of(&clone, "HEAD"),
      pushed_head,
      "the clone of {name} stands at the pushed head"
    );
    Measured {
      pack,
      peak: push_peak.max(clone_peak),
      pushed,
      cloned: {
        let cloned = settled_resident(&pid);
        print_gauges(&leg.instance, &format!("{name} after the clone"));
        cloned
      },
    }
  };
  let start = settled_resident(&pid);
  print_gauges(&leg.instance, "the start");
  let warm = round("warm", WARM_COMMITS, None);
  // W: the range the warm-up's resident set moved over, from its least (the start or its settled end, whichever the
  // idle give-back left lower) to its peak: what a push and a clone cost, and the noise of reading them, whatever
  // they bring.
  let warm_cost = warm.peak.saturating_sub(start.min(warm.cloned));
  let measured = round(REPOSITORY, HISTORY_COMMITS, real.as_ref());
  let bound = warm.cloned + 2 * measured.pack + warm_cost;
  let mib = |bytes: u64| format!("{}.{}", bytes >> 20, ((bytes % (1 << 20)) * 10) >> 20);
  eprintln!(
    "resident MiB: start {}; the warm-up's pack {} bytes, peak {}, after {} (W {}); the pack N = {} bytes, peak {}, \
     after the push {}, after the clone {}; the bound {}",
    mib(start),
    warm.pack,
    mib(warm.peak),
    mib(warm.cloned),
    mib(warm_cost),
    measured.pack,
    mib(measured.peak),
    mib(measured.pushed),
    mib(measured.cloned),
    mib(bound)
  );
  assert!(
    measured.pushed <= bound && measured.cloned <= bound && measured.peak <= bound,
    "the daemon held {} MiB after the push, {} MiB after the clone and {} MiB at its peak, past {} MiB (the \
     warm-up's end plus 2N + W): it holds the history, not the change",
    mib(measured.pushed),
    mib(measured.cloned),
    mib(measured.peak),
    mib(bound)
  );
}

/// §4.17.2 "Transports" item 1 and A-686 (Basic with the token as its password on the loopback edge, Basic's challenge
/// first): the round trip as [`stock_git_clones_pushes_and_fetches_over_the_vcs_http_edge_with_a_bearer_token`] runs
/// it, the token given as Basic's password in the URL (`http://git:TOKEN@127.0.0.1:PORT/project`), as a user gives a
/// password to stock git, which sends it once the edge's `401` offers Basic. Expect every step as [`round_trip`]
/// states. Skips loudly as that test does. Its one wait ended with A-686: before it the cleartext edge's `401` offered
/// Bearer alone, and stock git answered `fatal: Authentication failed` (g309's kiln r426).
#[test]
fn stock_git_clones_pushes_and_fetches_over_the_vcs_http_edge_with_basic() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping stock git over the HTTP edge with Basic: no stock git here (set {STOCK_GIT} to the command that runs \
       it)"
    );
    return;
  };
  let Some(leg) = http_leg("basic") else {
    return;
  };
  let scratch = Scratch::new("git-http-basic");
  let git = Git::new(&program, &scratch, "basic", Vec::new());
  let url = format!(
    "http://{USER}:{}@127.0.0.1:{}/{REPOSITORY}",
    leg.token, leg.port
  );
  round_trip(&git, &url);
}

/// A fresh Ed25519 key made by `ssh-keygen` in `scratch`, with no passphrase: its private key's path and its public
/// key's line; `None` where this host has no `ssh-keygen`, which the caller skips loudly.
fn fresh_key(scratch: &Scratch) -> Option<(String, String)> {
  let key = format!("{}/key", scratch.path);
  let made = Command::new("ssh-keygen")
    .args([
      "-q",
      "-t",
      "ed25519",
      "-N",
      "",
      "-C",
      "slates-git-test",
      "-f",
      &key,
    ])
    .output()
    .ok()?;
  assert!(made.status.success(), "ssh-keygen: {made:?}");
  let public = std::fs::read_to_string(format!("{key}.pub")).unwrap();
  Some((key, public.trim().to_owned()))
}

/// The port and host key fingerprint `line` names, when it is the daemon's SSH edge line ([`VCS_SSH_LINE`]).
fn ssh_edge_of(line: &str) -> Option<(u16, String)> {
  let rest = line.strip_prefix(VCS_SSH_LINE)?;
  let (port, fingerprint) = rest.trim().split_once(VCS_SSH_KEY_WORDS)?;
  Some((port.parse().ok()?, fingerprint.to_owned()))
}

/// A `known_hosts` file in `scratch` holding the host key the SSH edge at loopback `port` presents, made as a user
/// makes one: OpenSSH's `ssh-keyscan` reads the key, and `ssh-keygen -l` fingerprints the file it wrote. Fails the
/// test unless that fingerprint is `fingerprint`, the one the daemon printed, so the client that trusts this file
/// trusts the key the daemon named and no other. The file's path.
#[allow(clippy::disallowed_methods)]
fn known_hosts(scratch: &Scratch, port: u16, fingerprint: &str) -> String {
  let path = format!("{}/known_hosts", scratch.path);
  let scanned = Command::new("ssh-keyscan")
    .args(["-p", &port.to_string(), "-t", VCS_SSH_KEY_TYPE])
    .args(["-T", &PATIENCE.as_secs().to_string(), "127.0.0.1"])
    .output()
    .unwrap();
  let lines = String::from_utf8_lossy(&scanned.stdout).into_owned();
  assert!(
    scanned.status.success() && !lines.trim().is_empty(),
    "ssh-keyscan read no host key at port {port}: {scanned:?}"
  );
  std::fs::write(&path, &lines).unwrap();
  let listed = Command::new("ssh-keygen")
    .args(["-l", "-E", "sha256", "-f", &path])
    .output()
    .unwrap();
  let listing = String::from_utf8_lossy(&listed.stdout).into_owned();
  let fingerprints: Vec<&str> = listing
    .lines()
    .filter_map(|line| line.split_whitespace().nth(1))
    .collect();
  assert_eq!(
    fingerprints,
    [fingerprint],
    "the key ssh-keyscan read at port {port} is the one the daemon named: {lines}"
  );
  path
}

/// §4.17.2 "A fetch", "A push" and "Transports" item 2 (slates' own SSH server running `git-upload-pack '<repo>'` and
/// `git-receive-pack '<repo>'`). Do: start an anchor with its SSH edge on loopback (`--vcs-ssh 127.0.0.1:0`); make
/// repository [`REPOSITORY`] through the CLI and enroll a fresh key for its owner (`slates credential key`); run the
/// round trip as stock git at `ssh://git@127.0.0.1:PORT/project`, git running OpenSSH with that key alone and no
/// configuration of its own (`core.sshCommand`), trusting only the host key the daemon named ([`known_hosts`], checked
/// strictly). Expect every step as [`round_trip`] states. Skips loudly without stock
/// git ([`STOCK_GIT`]), OpenSSH's `ssh` or `ssh-keygen`, and on a platform whose issuer surface a separate process
/// cannot use.
#[test]
fn stock_git_clones_pushes_and_fetches_over_the_ssh_edge() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping stock git over the SSH edge: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  if !openssh() {
    eprintln!("skipping stock git over the SSH edge: no OpenSSH ssh here");
    return;
  }
  let scratch = Scratch::new("git-ssh");
  let Some((key, public)) = fresh_key(&scratch) else {
    eprintln!("skipping stock git over the SSH edge: no ssh-keygen here");
    return;
  };
  let instance = format!("git-ssh-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
  let (port, fingerprint) = started
    .lines
    .iter()
    .find_map(|line| ssh_edge_of(line))
    .expect("the daemon named its SSH edge's port and host key");
  let Some(issuer) = issuer_of(&started, "the SSH leg") else {
    return;
  };
  let known_hosts = known_hosts(&scratch, port, &fingerprint);
  create_repository(&instance);
  let (code, out, err) = slates(&instance, &issuer, &["credential", "key", &public]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let ssh = format!(
    "core.sshCommand=ssh -F /dev/null -i {key} -o IdentitiesOnly=yes -o BatchMode=yes \
     -o StrictHostKeyChecking=yes -o UserKnownHostsFile={known_hosts} \
     -o GlobalKnownHostsFile=/dev/null"
  );
  let git = Git::new(&program, &scratch, "ssh", vec!["-c".to_owned(), ssh]);
  round_trip(&git, &format!("ssh://{USER}@127.0.0.1:{port}/{REPOSITORY}"));
}

/// A-575 (`slates repository branch create|retire|bind`, the owner's branch verbs). Do: over the HTTP edge, push a
/// first commit to [`BRANCH`] as stock git; make branch `topic` from it with the CLI; list the heads with stock `git
/// ls-remote`; retire `topic` and list them; bind `topic` again to the green the create printed and list them. Expect
/// `topic` at the pushed commit after the create, gone after the retire, and back at the same commit after the bind:
/// stock git sees the namespace the CLI changes (A-743: every name the namespace binds is a ref git's site serves).
#[test]
fn stock_git_sees_the_branches_the_cli_makes_retires_and_binds() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping the branch verbs under stock git: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let Some(leg) = http_leg("branches") else {
    return;
  };
  let instance = format!("git-http-branches-{}", std::process::id());
  let scratch = Scratch::new("git-branches");
  let git = Git::new(&program, &scratch, "branches", Vec::new());
  let url = format!(
    "http://{USER}:{}@127.0.0.1:{}/{REPOSITORY}",
    leg.token, leg.port
  );
  let pushed = first_push(&git, &url);
  let verb = |args: &[&str]| {
    let (code, out, err) = slates(&instance, &[], args);
    assert_eq!(code, 0, "{args:?}: {out}{err}");
    out
  };
  let heads = || git.ok(&git.scratch, &["ls-remote", "--heads", &url]);
  let made = verb(&[
    "repository",
    "branch",
    "create",
    REPOSITORY,
    "topic",
    "--from",
    BRANCH,
  ]);
  let green = value_of(&made, "id");
  let after_create = heads();
  verb(&["repository", "branch", "retire", REPOSITORY, "topic"]);
  let after_retire = heads();
  verb(&["repository", "branch", "bind", REPOSITORY, "topic", &green]);
  let after_bind = heads();
  let topic = format!("{pushed}\trefs/heads/topic");
  assert_eq!(
    (
      after_create.contains(&topic),
      after_retire.contains("refs/heads/topic"),
      after_bind.contains(&topic),
    ),
    (true, false, true),
    "created: {after_create}; retired: {after_retire}; bound: {after_bind}"
  );
}

/// Format: the variable naming the client counts goal 93's concurrency tests run, comma-separated (`2,10,50`); unset,
/// [`FOCUSED_CLIENTS`] alone.
const CONCURRENCY: &str = "SLATES_GIT_CONCURRENCY";
/// Shape: the client count a concurrency test runs when [`CONCURRENCY`] names none: two, the fewest that race.
const FOCUSED_CLIENTS: usize = 2;
/// Shape: the mirrors each concurrency test clones at its end to hold every clone's refs and objects identical: three,
/// so one clone that differs shows against two that agree.
const MIRRORS: usize = 3;
/// Shape: one concurrent reader for this many pushing clients, and never fewer than [`LEAST_READERS`]: readers enough
/// to meet the pushes mid-flight without outnumbering them.
const CLIENTS_PER_READER: usize = 5;
/// Shape: the fewest concurrent readers a distinct-branches test runs: two, so a clone and a fetch overlap each other
/// as well as the pushes.
const LEAST_READERS: usize = 2;
/// Format: the words git puts on a ref a push could not move (`transport.c`, `print_ref_status`): its own `[rejected]`
/// and the server's `[remote rejected]`.
const REJECTED_WORDS: [&str; 2] = ["[rejected]", "[remote rejected]"];
/// Format: why git says a ref was refused when another push moved it first. Lost before the advertisement, git judges
/// it itself (`transport.c`; `push.c`'s advice): the tip behind (`fetch first`, `non-fast-forward`), a tag already
/// there (`already exists`), or a lease that went stale. Lost after it, at the ref's compare-and-swap, the server
/// refuses it `[remote rejected] … (failed to update ref)`, as stock git's own receive-pack does: two pushes at once to
/// a bare repository over `file://`, 6 of 6 rounds one refused so, with `cannot lock ref 'refs/heads/main': is at …
/// but expected …` (git 2.50.1, 2026-11-21).
const LOST_RACE_WORDS: [&str; 5] = [
  "fetch first",
  "non-fast-forward",
  "already exists",
  "stale info",
  "failed to update ref",
];
/// Format: words in a refused push's output that mean the server broke rather than refused cleanly: a dropped
/// connection, a failed RPC over HTTP, a 5xx answer, or a pack the server could not unpack.
const BROKEN_WORDS: [&str; 5] = [
  "hung up",
  "RPC failed",
  "HTTP 5",
  "unpack",
  "internal server error",
];

/// The client counts [`CONCURRENCY`] names, or [`FOCUSED_CLIENTS`] alone.
fn client_counts() -> Vec<usize> {
  std::env::var(CONCURRENCY).map_or_else(
    |_| vec![FOCUSED_CLIENTS],
    |line| {
      line
        .split(',')
        .map(|count| count.trim().parse().expect("a client count"))
        .collect()
    },
  )
}

/// One concurrency session: its HTTP leg (an anchor, repository [`REPOSITORY`], a token for its owner), stock git
/// carrying the token, the repository's URL, and the scratch directory, removed after the anchor stops.
struct Racing {
  _leg: HttpLeg,
  git: Git,
  url: String,
  _scratch: Scratch,
}

/// The concurrency session `session`, or `None` (said loudly) where this host runs no stock git or names no issuer
/// surface.
fn racing(session: &str) -> Option<Racing> {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping {session}: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return None;
  };
  let leg = http_leg(session)?;
  let scratch = Scratch::new(&format!("git-race-{session}"));
  let header = format!("http.extraHeader=Authorization: Bearer {}", leg.token);
  let git = Git::new(&program, &scratch, session, vec!["-c".to_owned(), header]);
  let url = format!("http://127.0.0.1:{}/{REPOSITORY}", leg.port);
  Some(Racing {
    _leg: leg,
    git,
    url,
    _scratch: scratch,
  })
}

/// Writes `text` as file `name` in copy `copy` (the test's own scratch directory, as [`put`] writes).
#[allow(clippy::disallowed_methods)]
fn put_named(copy: &str, name: &str, text: &str) {
  std::fs::write(format!("{copy}/{name}"), text).unwrap();
}

/// `count` clones of `url`, copies `PREFIX-0` and on: their paths.
fn clones(git: &Git, url: &str, prefix: &str, count: usize) -> Vec<String> {
  (0..count)
    .map(|index| {
      let path = git.copy(&format!("{prefix}-{index}"));
      git.ok(&git.scratch, &["clone", "-q", url, &path]);
      path
    })
    .collect()
}

/// A commit in copy `copy` adding a file of its own, named for `what` and `index`: the commit.
fn commit_own(git: &Git, copy: &str, what: &str, index: usize) -> String {
  let name = format!("{what}-{index}");
  put_named(copy, &name, &format!("{what} {index}\n"));
  git.ok(copy, &["add", &name]);
  git.ok(copy, &["commit", "-q", "-m", &name]);
  git.commit_of(copy, "HEAD")
}

/// Runs each of `jobs` (a directory and git's words) on a thread of its own, all released at once by one barrier: each
/// one's exit code and both outputs, in order.
fn at_once(git: &Git, jobs: &[(String, Vec<String>)]) -> Vec<(i32, String, String)> {
  let barrier = std::sync::Barrier::new(jobs.len());
  let barrier = &barrier;
  std::thread::scope(|scope| {
    let running: Vec<_> = jobs
      .iter()
      .map(|(dir, words)| {
        scope.spawn(move || {
          let words: Vec<&str> = words.iter().map(String::as_str).collect();
          barrier.wait();
          git.run(dir, &words)
        })
      })
      .collect();
    running.into_iter().map(|job| job.join().unwrap()).collect()
  })
}

/// `words` as owned strings, a job's words for [`at_once`].
fn words(words: &[&str]) -> Vec<String> {
  words.iter().map(|word| (*word).to_owned()).collect()
}

/// Whether a push refused with `err` was refused cleanly: git names the ref rejected ([`REJECTED_WORDS`]) because the
/// race was lost ([`LOST_RACE_WORDS`]), and nothing says the server broke ([`BROKEN_WORDS`]).
fn refused_cleanly(err: &str) -> bool {
  REJECTED_WORDS.iter().any(|words| err.contains(words))
    && LOST_RACE_WORDS.iter().any(|words| err.contains(words))
    && !BROKEN_WORDS.iter().any(|words| err.contains(words))
}

/// The refs `url` serves, as `git ls-remote` lists them, without its `HEAD` or an annotated tag's peeled line
/// (`refs/tags/NAME^{}`): each ref's name and object.
fn served_refs(git: &Git, url: &str) -> std::collections::BTreeMap<String, String> {
  git
    .ok(&git.scratch, &["ls-remote", url])
    .lines()
    .filter_map(|line| line.split_once('\t'))
    .filter(|(_, name)| *name != "HEAD" && !name.ends_with("^{}"))
    .map(|(object, name)| (name.to_owned(), object.to_owned()))
    .collect()
}

/// The refs of the repository at `dir` under `under`, each name with `under` replaced by `as_`, and their objects.
fn refs_under(
  git: &Git,
  dir: &str,
  under: &str,
  as_: &str,
) -> std::collections::BTreeMap<String, String> {
  git
    .ok(
      dir,
      &["for-each-ref", "--format=%(objectname) %(refname)", under],
    )
    .lines()
    .filter_map(|line| line.split_once(' '))
    .filter(|(_, name)| !name.ends_with("/HEAD"))
    .map(|(object, name)| (name.replacen(under, as_, 1), object.to_owned()))
    .collect()
}

/// The checks every concurrency test ends on, `label` naming the run: the refs `url` serves equal `expected`, the
/// accepted pushes; then each of [`MIRRORS`] mirror clones passes `git fsck --full --strict`, holds exactly `expected`,
/// and lists the same refs and the same objects as the others.
fn ends_consistent(
  git: &Git,
  url: &str,
  expected: &std::collections::BTreeMap<String, String>,
  label: &str,
) {
  assert_eq!(
    &served_refs(git, url),
    expected,
    "{label}: the refs served are the accepted pushes"
  );
  let mut held = Vec::new();
  for mirror in 0..MIRRORS {
    let path = git.copy(&format!("{label}-mirror-{mirror}"));
    git.ok(&git.scratch, &["clone", "-q", "--mirror", url, &path]);
    git.ok(&path, &["fsck", "--full", "--strict", "--no-dangling"]);
    let refs = refs_under(git, &path, "refs/", "refs/");
    assert_eq!(
      &refs, expected,
      "{label}: mirror {mirror} holds the accepted pushes"
    );
    let mut objects: Vec<String> = git
      .ok(&path, &["cat-file", "--batch-all-objects", "--batch-check"])
      .lines()
      .map(str::to_owned)
      .collect();
    objects.sort();
    held.push((refs, objects));
  }
  assert!(
    held.windows(2).all(|pair| pair[0] == pair[1]),
    "{label}: every mirror lists the same refs and objects"
  );
}

/// Goal 93's concurrency, one branch (a correctness round main runs; Ada 2026-11-21, "complete the servers, ensure
/// correctness"). Do: for each client count N [`CONCURRENCY`] names, start an anchor with its HTTP edge and repository
/// [`REPOSITORY`]; push a first commit to [`BRANCH`]; clone it N times, commit a file of its own in each clone, and push
/// all N to [`BRANCH`] at once, released by one barrier. Expect exactly one push to win; every other to be refused in
/// git's own words for a lost race ([`LOST_RACE_WORDS`]: `fetch first` or `non-fast-forward` before the
/// advertisement, `[remote rejected] … (failed to update ref)` after it, as stock git's receive-pack answers) and never
/// a broken server ([`BROKEN_WORDS`]); and then the branch at the winner's commit, served and in every mirror, each
/// mirror fsck clean and identical ([`ends_consistent`]).
#[test]
#[ignore = "goal 93's concurrency round, main's to run: SLATES_GIT_CONCURRENCY=2,10,50 cargo test --release -p slates-cli --test git_interop -- --ignored concurrent_pushes --nocapture"]
fn concurrent_pushes_to_one_branch_land_one_and_refuse_the_rest_cleanly() {
  for count in client_counts() {
    let Some(race) = racing(&format!("one-branch-{count}")) else {
      return;
    };
    first_push(&race.git, &race.url);
    let copies = clones(&race.git, &race.url, "clone", count);
    let heads: Vec<String> = copies
      .iter()
      .enumerate()
      .map(|(index, copy)| commit_own(&race.git, copy, "same", index))
      .collect();
    let jobs: Vec<_> = copies
      .iter()
      .map(|copy| (copy.clone(), words(&["push", "-q", "origin", BRANCH])))
      .collect();
    let results = at_once(&race.git, &jobs);
    let won: Vec<usize> = (0..count).filter(|&index| results[index].0 == 0).collect();
    let unclean: Vec<&(i32, String, String)> = results
      .iter()
      .filter(|(code, _, err)| *code != 0 && !refused_cleanly(err))
      .collect();
    eprintln!(
      "{count} pushes to one branch: won {won:?}; refused uncleanly {}",
      unclean.len()
    );
    assert_eq!(
      won.len(),
      1,
      "{count} pushes to one branch: exactly one wins: {results:?}"
    );
    assert!(
      unclean.is_empty(),
      "{count} pushes to one branch: every loser refused cleanly: {unclean:?}"
    );
    let expected =
      std::collections::BTreeMap::from([(format!("refs/heads/{BRANCH}"), heads[won[0]].clone())]);
    ends_consistent(
      &race.git,
      &race.url,
      &expected,
      &format!("one-branch-{count}"),
    );
  }
}

/// What one concurrent reader saw in one pass: whether its clone succeeded, the branches it cloned with their
/// commits, whether that clone fscked clean, whether its fetch into a standing clone succeeded, and the outputs of
/// whatever failed.
struct ReaderPass {
  cloned: bool,
  branches: std::collections::BTreeMap<String, String>,
  fsck_clean: bool,
  fetched: bool,
  said: String,
}

/// Reader `reader`'s passes until `stop`, at least one: a fresh clone of `url` (no checkout), its branches and its
/// fsck, then a fetch of every branch into the reader's standing clone.
fn read_until(
  git: &Git,
  url: &str,
  reader: usize,
  stop: &std::sync::atomic::AtomicBool,
) -> Vec<ReaderPass> {
  let standing = git.copy(&format!("reader-{reader}"));
  git.ok(
    &git.scratch,
    &["clone", "-q", "--no-checkout", url, &standing],
  );
  let mut passes = Vec::new();
  for pass in 0.. {
    let path = git.copy(&format!("reader-{reader}-{pass}"));
    let (code, _, err) = git.run(&git.scratch, &["clone", "-q", "--no-checkout", url, &path]);
    let (branches, fsck) = if code == 0 {
      let branches = refs_under(git, &path, "refs/remotes/origin/", "refs/heads/");
      (
        branches,
        git.run(&path, &["fsck", "--full", "--no-dangling"]),
      )
    } else {
      (
        std::collections::BTreeMap::new(),
        (code, String::new(), err.clone()),
      )
    };
    let fetch = git.run(
      &standing,
      &[
        "fetch",
        "-q",
        "origin",
        "+refs/heads/*:refs/remotes/origin/*",
      ],
    );
    passes.push(ReaderPass {
      cloned: code == 0,
      branches,
      fsck_clean: fsck.0 == 0,
      fetched: fetch.0 == 0,
      said: format!(
        "clone: {err}; fsck: {}{}; fetch: {}",
        fsck.1, fsck.2, fetch.2
      ),
    });
    if stop.load(std::sync::atomic::Ordering::Acquire) {
      break;
    }
  }
  passes
}

/// Goal 93's concurrency, distinct branches with readers (a correctness round main runs). Do: for each client count N
/// [`CONCURRENCY`] names, start an anchor with its HTTP edge and repository [`REPOSITORY`]; push a first commit to
/// [`BRANCH`]; clone it N times and commit on a branch of its own in each clone; then push all N branches at once while
/// N / [`CLIENTS_PER_READER`] readers (at least [`LEAST_READERS`]) clone, fsck and fetch in a loop until the pushes
/// end. Expect every push to land; every reader's clone and fetch to succeed, each clone fsck clean and holding each
/// branch either not at all or at its pushed commit (never one half-applied); and then every branch served and in
/// every mirror, each mirror fsck clean and identical ([`ends_consistent`]).
#[test]
#[ignore = "goal 93's concurrency round, main's to run: SLATES_GIT_CONCURRENCY=2,10,50 cargo test --release -p slates-cli --test git_interop -- --ignored concurrent_pushes --nocapture"]
fn concurrent_pushes_to_distinct_branches_all_land_while_readers_see_consistent_refs() {
  for count in client_counts() {
    let Some(race) = racing(&format!("branches-{count}")) else {
      return;
    };
    let base = first_push(&race.git, &race.url);
    let copies = clones(&race.git, &race.url, "clone", count);
    let mut expected = std::collections::BTreeMap::from([(format!("refs/heads/{BRANCH}"), base)]);
    for (index, copy) in copies.iter().enumerate() {
      let branch = format!("refs/heads/b{index}");
      race.git.ok(copy, &["symbolic-ref", "HEAD", &branch]);
      expected.insert(branch, commit_own(&race.git, copy, "branch", index));
    }
    let jobs: Vec<_> = copies
      .iter()
      .enumerate()
      .map(|(index, copy)| {
        (
          copy.clone(),
          words(&["push", "-q", "origin", &format!("b{index}")]),
        )
      })
      .collect();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (results, reads) = std::thread::scope(|scope| {
      let readers: Vec<_> = (0..(count / CLIENTS_PER_READER).max(LEAST_READERS))
        .map(|reader| {
          let (git, url, stop) = (&race.git, &race.url, &stop);
          scope.spawn(move || read_until(git, url, reader, stop))
        })
        .collect();
      let results = at_once(&race.git, &jobs);
      stop.store(true, std::sync::atomic::Ordering::Release);
      let reads: Vec<ReaderPass> = readers
        .into_iter()
        .flat_map(|reader| reader.join().unwrap())
        .collect();
      (results, reads)
    });
    let failed: Vec<&(i32, String, String)> =
      results.iter().filter(|(code, _, _)| *code != 0).collect();
    eprintln!(
      "{count} pushes to distinct branches: {} failed; {} reader passes",
      failed.len(),
      reads.len()
    );
    assert!(
      failed.is_empty(),
      "{count} pushes to distinct branches: all land: {failed:?}"
    );
    for read in &reads {
      assert!(
        read.cloned && read.fetched && read.fsck_clean,
        "{count} distinct branches: a reader's clone, fsck and fetch succeed: {}",
        read.said
      );
      let torn: Vec<_> = read
        .branches
        .iter()
        .filter(|(name, object)| expected.get(*name) != Some(object))
        .collect();
      assert!(
        torn.is_empty(),
        "{count} distinct branches: a reader saw each branch absent or at its pushed commit: {torn:?}"
      );
    }
    ends_consistent(
      &race.git,
      &race.url,
      &expected,
      &format!("branches-{count}"),
    );
  }
}

/// Goal 93's concurrency, tags (a correctness round main runs). Do: for each client count N [`CONCURRENCY`] names,
/// start an anchor with its HTTP edge and repository [`REPOSITORY`]; push a first commit to [`BRANCH`]; clone it N
/// times; in each clone make an annotated tag `vI` and a lightweight tag `lI` of its own and push all N clones' tags at
/// once; then in each clone commit a file of its own, tag that commit `race` (annotated), and push all N `race` tags at
/// once. Expect every distinct tag to land; exactly one `race` to win and every other to be refused in git's own words
/// ([`LOST_RACE_WORDS`]: `already exists`, or `failed to update ref` past the advertisement), never a broken server;
/// and then every tag served and in every mirror, each mirror fsck clean and identical ([`ends_consistent`]).
#[test]
#[ignore = "goal 93's concurrency round, main's to run: SLATES_GIT_CONCURRENCY=2,10,50 cargo test --release -p slates-cli --test git_interop -- --ignored concurrent_pushes --nocapture"]
fn concurrent_pushes_of_tags_land_and_one_raced_tag_name_lands_once() {
  for count in client_counts() {
    let Some(race) = racing(&format!("tags-{count}")) else {
      return;
    };
    let base = first_push(&race.git, &race.url);
    let copies = clones(&race.git, &race.url, "clone", count);
    let mut expected =
      std::collections::BTreeMap::from([(format!("refs/heads/{BRANCH}"), base.clone())]);
    for (index, copy) in copies.iter().enumerate() {
      let (annotated, light) = (format!("v{index}"), format!("l{index}"));
      race
        .git
        .ok(copy, &["tag", "-a", &annotated, "-m", &annotated]);
      race.git.ok(copy, &["tag", &light]);
      expected.insert(
        format!("refs/tags/{annotated}"),
        race.git.commit_of(copy, &format!("refs/tags/{annotated}")),
      );
      expected.insert(format!("refs/tags/{light}"), base.clone());
    }
    let jobs: Vec<_> = copies
      .iter()
      .enumerate()
      .map(|(index, copy)| {
        let tags = [format!("refs/tags/v{index}"), format!("refs/tags/l{index}")];
        (
          copy.clone(),
          words(&["push", "-q", "origin", &tags[0], &tags[1]]),
        )
      })
      .collect();
    let results = at_once(&race.git, &jobs);
    let failed: Vec<&(i32, String, String)> =
      results.iter().filter(|(code, _, _)| *code != 0).collect();
    assert!(
      failed.is_empty(),
      "{count} pushes of distinct tags: all land: {failed:?}"
    );
    let racers: Vec<String> = copies
      .iter()
      .enumerate()
      .map(|(index, copy)| {
        commit_own(&race.git, copy, "race", index);
        race.git.ok(copy, &["tag", "-a", "race", "-m", "race"]);
        race.git.commit_of(copy, "refs/tags/race")
      })
      .collect();
    let jobs: Vec<_> = copies
      .iter()
      .map(|copy| {
        (
          copy.clone(),
          words(&["push", "-q", "origin", "refs/tags/race"]),
        )
      })
      .collect();
    let results = at_once(&race.git, &jobs);
    let won: Vec<usize> = (0..count).filter(|&index| results[index].0 == 0).collect();
    let unclean: Vec<&(i32, String, String)> = results
      .iter()
      .filter(|(code, _, err)| *code != 0 && !refused_cleanly(err))
      .collect();
    eprintln!(
      "{count} pushes of one tag name: won {won:?}; refused uncleanly {}",
      unclean.len()
    );
    assert_eq!(
      won.len(),
      1,
      "{count} pushes of one tag name: exactly one wins: {results:?}"
    );
    assert!(
      unclean.is_empty(),
      "{count} pushes of one tag name: every loser refused cleanly: {unclean:?}"
    );
    expected.insert("refs/tags/race".to_owned(), racers[won[0]].clone());
    ends_consistent(&race.git, &race.url, &expected, &format!("tags-{count}"));
  }
}

/// Format: the variable naming the repository the whole-repository session carries (a clone of git/git for main's
/// rounds); unset, the session skips loudly.
const DATASET: &str = "SLATES_TEST_GIT_DATASET";

/// Format: the variable naming the edges the whole-repository session runs over, comma-separated (`http`, `ssh`);
/// unset, both.
const EDGES: &str = "SLATES_TEST_GIT_EDGES";

/// Format: the refs the whole-repository session compares: every branch and every tag, peeled lines included.
const COMPARED_REFS: [&str; 2] = ["refs/heads/", "refs/tags/"];

/// Format: a tag object carrying a PGP signature block, made with `git mktag` so no signing key is needed: slates must
/// carry its bytes as they are, signature and all.
const SIGNED_TAG_BODY: &str =
  "-----BEGIN PGP SIGNATURE-----\n\niQEzBAABCAAdFiEE\n=test\n-----END PGP SIGNATURE-----\n";

/// The lines of `listing` (a `git ls-remote` or an advertisement's ref lines, "OBJECT\tREF") that name a branch or a
/// tag ([`COMPARED_REFS`]), sorted.
fn compared(listing: &str) -> Vec<String> {
  let mut lines: Vec<String> = listing
    .lines()
    .filter(|line| {
      line
        .split('\t')
        .nth(1)
        .is_some_and(|name| COMPARED_REFS.iter().any(|prefix| name.starts_with(prefix)))
    })
    .map(str::to_owned)
    .collect();
  lines.sort();
  lines
}

/// A bare mirror of `dataset` at `source`, given tags of every kind on its default branch's tip: lightweight, annotated,
/// a lightweight and an annotated tag of the tip's tree, a tag of a blob, an annotated tag of an annotated tag, and a
/// tag object carrying a signature block ([`SIGNED_TAG_BODY`], by `git mktag`).
fn tagged_source(git: &Git, dataset: &str, source: &str) {
  git.ok(&git.scratch, &["clone", "-q", "--mirror", dataset, source]);
  let tip = git.commit_of(source, "HEAD");
  let tree = git.commit_of(source, "HEAD^{tree}");
  let blob = git
    .ok(source, &["ls-tree", "-r", "HEAD"])
    .lines()
    .find_map(|line| {
      let mut words = line.split_whitespace();
      (words.nth(1) == Some("blob")).then(|| words.next().unwrap().to_owned())
    })
    .expect("the dataset's tip holds a file");
  git.ok(source, &["tag", "slates-light", &tip]);
  git.ok(
    source,
    &["tag", "-a", "-m", "annotated", "slates-annotated", &tip],
  );
  git.ok(source, &["tag", "slates-tree", &tree]);
  git.ok(
    source,
    &["tag", "-a", "-m", "a tree", "slates-annotated-tree", &tree],
  );
  git.ok(source, &["tag", "slates-blob", &blob]);
  git.ok(
    source,
    &[
      "tag",
      "-a",
      "-m",
      "a tag of a tag",
      "slates-nested",
      "slates-annotated",
    ],
  );
  let signed = format!(
    "object {tip}\ntype commit\ntag slates-signed\ntagger Test <test@example.com> 1700000000 +0000\n\nsigned\n{SIGNED_TAG_BODY}"
  );
  let made = mktag(git, source, &signed);
  git.ok(source, &["update-ref", "refs/tags/slates-signed", &made]);
}

/// The tag object `git mktag` makes from `text` in the repository at `dir`.
fn mktag(git: &Git, dir: &str, text: &str) -> String {
  let (program, leading) = git.program.split_first().expect("a command");
  let mut child = Command::new(program)
    .args(leading)
    .args(["mktag"])
    .current_dir(dir)
    .envs(Git::environment(&git.home))
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child
    .stdin
    .take()
    .unwrap()
    .write_all(text.as_bytes())
    .unwrap();
  let out = child.wait_with_output().unwrap();
  assert!(out.status.success(), "git mktag: {out:?}");
  String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// `git for-each-ref` over the branches and tags of the repository at `dir`: "OBJECT REF" lines, sorted.
fn refs_of(git: &Git, dir: &str) -> String {
  git.ok(
    dir,
    &[
      "for-each-ref",
      "--format=%(objectname) %(refname)",
      "refs/heads",
      "refs/tags",
    ],
  )
}

/// The bytes of `git archive` of `commit` in the repository at `dir`, written to `out`.
#[allow(clippy::disallowed_methods)]
fn archive(git: &Git, dir: &str, commit: &str, out: &str) -> Vec<u8> {
  git.ok(dir, &["archive", "--format=tar", "-o", out, commit]);
  std::fs::read(out).unwrap()
}

/// The ref lines of a v0 upload-pack advertisement `body` (pkt-lines): each data line's text, the first ref line's
/// capabilities (after its NUL) cut, the service line and flushes dropped, as "OBJECT\tREF" lines.
fn advertised(body: &[u8]) -> String {
  let mut lines = Vec::new();
  let mut at = 0;
  while at + 4 <= body.len() {
    let length =
      usize::from_str_radix(std::str::from_utf8(&body[at..at + 4]).unwrap(), 16).unwrap();
    if length < 4 {
      at += 4;
      continue;
    }
    let text = String::from_utf8_lossy(&body[at + 4..at + length]).into_owned();
    at += length;
    let text = text
      .split('\0')
      .next()
      .unwrap()
      .trim_end_matches('\n')
      .to_owned();
    if text.starts_with("# service=") {
      continue;
    }
    lines.push(text.replacen(' ', "\t", 1));
  }
  lines.join("\n")
}

/// Stock git's own `http-backend`, run as the CGI program a web server would run, answering the v0 advertisement of
/// `git-upload-pack` for the repository `name` under `root`: the body after its headers.
fn http_backend_advertisement(git: &Git, root: &str, name: &str) -> Vec<u8> {
  let (program, leading) = git.program.split_first().expect("a command");
  let out = Command::new(program)
    .args(leading)
    .arg("http-backend")
    .envs(Git::environment(&git.home))
    .env("GIT_PROJECT_ROOT", root)
    .env("GIT_HTTP_EXPORT_ALL", "1")
    .env("REQUEST_METHOD", "GET")
    .env("PATH_INFO", format!("/{name}/info/refs"))
    .env("QUERY_STRING", "service=git-upload-pack")
    .env_remove("GIT_PROTOCOL")
    .output()
    .unwrap();
  assert!(out.status.success(), "git http-backend: {out:?}");
  let split = out
    .stdout
    .windows(4)
    .position(|window| window == b"\r\n\r\n")
    .map(|at| at + 4)
    .or_else(|| {
      out
        .stdout
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|at| at + 2)
    })
    .expect("http-backend's headers end");
  out.stdout[split..].to_vec()
}

/// slates' HTTP edge's v0 advertisement of `git-upload-pack` for [`REPOSITORY`] at `port`, read with curl and the
/// bearer `token`: the body; `None` where this host runs no curl, which the caller skips loudly.
fn edge_advertisement(port: u16, token: &str) -> Option<Vec<u8>> {
  let out = Command::new("curl")
    .args(["-s", "-f", "-H", &format!("Authorization: Bearer {token}")])
    .arg(format!(
      "http://127.0.0.1:{port}/{REPOSITORY}/info/refs?service=git-upload-pack"
    ))
    .output()
    .ok()?;
  assert!(
    out.status.success(),
    "curl the edge's advertisement: {out:?}"
  );
  Some(out.stdout)
}

/// Format: the daemon's counters of the paths a clone's objects took (main 01a20182's order of 2026-11-22 04:53): the
/// fetch stream's (`vcs_store::upload::STREAM_COUNTED`) and the encoding made ahead's (`vcs_store::git_encode`'s).
const CLONE_PATHS: [&str; 21] = [
  "vcs_store.git_upload.written_bytes",
  "vcs_store.git_upload.parts_copied",
  "vcs_store.git_upload.entries_copied",
  "vcs_store.git_upload.headers_rewritten",
  "vcs_store.git_upload.made_whole",
  "vcs_store.git_upload.loose_whole",
  "vcs_store.git_upload.loose_deltas",
  "vcs_store.git_upload.loose_unsearched",
  "vcs_store.git_upload.reused_whole",
  "vcs_store.git_upload.reused_deltas",
  "vcs_store.git_upload.reuse_refused",
  "vcs_store.git_upload.reuse_no_entry",
  "vcs_store.git_upload.reuse_unread",
  "vcs_store.git_upload.reuse_crc_missed",
  "vcs_store.git_upload.reuse_rebuilt",
  "vcs_store.git_encode.reused_whole",
  "vcs_store.git_encode.reused_deltas",
  "vcs_store.git_encode.reuse_refused",
  "vcs_store.git_encode.reuse_no_entry",
  "vcs_store.git_encode.reuse_unread",
  "vcs_store.git_encode.reuse_crc_missed",
];

/// The values of counters `names` in the daemon's status report (`slates status --json`, each counter an object
/// `{"count":N,"kind":"name"}`, one a shard that counted it), read once: each its sum, zero where none counted; none
/// where the status could not be read.
fn counters_of(instance: &str, names: &[&str]) -> Option<Vec<u64>> {
  let (code, status, _) = slates(instance, &[], &["status", "--json"]);
  (code == 0).then(|| {
    names
      .iter()
      .map(|name| {
        let kind = format!("\"kind\":\"{name}\"");
        status
          .match_indices(&kind)
          .filter_map(|(at, _)| {
            let object = status.get(status.get(..at)?.rfind('{')?..at)?;
            let count = object.get(object.find("\"count\":")? + "\"count\":".len()..)?;
            count
              .chars()
              .take_while(char::is_ascii_digit)
              .collect::<String>()
              .parse::<u64>()
              .ok()
          })
          .sum()
      })
      .collect()
  })
}

/// The bytes of the packs a repository at `dir` keeps (`objects/pack/*.pack`), and of the one `index-pack` is still
/// receiving (`tmp_pack_*`): a fresh mirror clone's, the pack its server sent; one cut short, what it had sent.
fn pack_file_bytes(dir: &str) -> u64 {
  std::fs::read_dir(format!("{dir}/objects/pack"))
    .map(|entries| {
      entries
        .filter_map(Result::ok)
        .filter(|entry| {
          entry.path().extension().is_some_and(|ext| ext == "pack")
            || entry.file_name().to_string_lossy().starts_with("tmp_pack_")
        })
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
    })
    .unwrap_or(0)
}

/// What a whole-repository mirror clone over one edge took (main 01a20182's order of 2026-11-22 04:53), said on stderr
/// when dropped, so a clone killed past [`PATIENCE`] says it too, before its daemon goes: the time, the pack bytes slates
/// sent beside stock's, and each of [`CLONE_PATHS`]'s counts the clone added.
struct CloneReport<'a> {
  /// The edge's daemon's instance.
  instance: &'a str,
  /// The mirror the clone makes.
  mirror: &'a str,
  /// The bytes of stock git's own clone of the source ([`stock_clone_bytes`]).
  stock: u64,
  /// The counters before the clone.
  before: Option<Vec<u64>>,
  /// When it began.
  began: Instant,
}

impl Drop for CloneReport<'_> {
  fn drop(&mut self) {
    let after = counters_of(self.instance, &CLONE_PATHS);
    let paths: Vec<String> = match (&self.before, &after) {
      (Some(before), Some(after)) => CLONE_PATHS
        .iter()
        .zip(before.iter().zip(after))
        .map(|(name, (before, after))| format!("{name}={}", after.saturating_sub(*before)))
        .collect(),
      _ => vec!["(the daemon's status could not be read)".to_owned()],
    };
    eprintln!(
      "whole-repository mirror clone over {}: {:?}; pack bytes slates {} stock {}; {}",
      self.instance,
      self.began.elapsed(),
      pack_file_bytes(self.mirror),
      self.stock,
      paths.join(" ")
    );
  }
}

/// The bytes of the pack stock git's own upload-pack sends a mirror clone of `source` (`git clone --mirror --no-local`
/// over `file://`), to say beside slates' ([`CloneReport`]).
fn stock_clone_bytes(git: &Git, source: &str) -> u64 {
  let stock = git.copy("stock.git");
  git.ok(
    &git.scratch,
    &[
      "clone",
      "-q",
      "--mirror",
      "--no-local",
      &file_url(source),
      &stock,
    ],
  );
  pack_file_bytes(&stock)
}

/// The whole-repository session against `url`, an empty repository on the daemon of `instance`, from `source`
/// ([`tagged_source`]): every branch and tag pushed; the listing equal to the source's; a mirror clone that `fsck --full
/// --strict` passes, its refs the source's, its pack bytes and paths said beside `stock`'s bytes ([`CloneReport`]); a
/// push from a work clone fetched back into the mirror, the listings equal again; a shallow clone one commit deep,
/// deepened by one; a partial clone (`--filter=blob:none`) that checks its tip out, its archive's bytes the source's.
fn whole_session(git: &Git, source: &str, (url, instance, stock): (&str, &str, u64)) {
  git.ok(
    source,
    &[
      "push",
      "-q",
      url,
      "refs/heads/*:refs/heads/*",
      "refs/tags/*:refs/tags/*",
    ],
  );
  let listed = git.ok(&git.scratch, &["ls-remote", url]);
  let local = git.ok(&git.scratch, &["ls-remote", source]);
  assert_eq!(
    compared(&listed),
    compared(&local),
    "the pushed listing is the source's"
  );
  let mirror = git.copy("mirror.git");
  {
    let _report = CloneReport {
      instance,
      mirror: &mirror,
      stock,
      before: counters_of(instance, &CLONE_PATHS),
      began: Instant::now(),
    };
    git.ok(&git.scratch, &["clone", "-q", "--mirror", url, &mirror]);
  }
  git.ok(&mirror, &["fsck", "--full", "--strict", "--no-progress"]);
  assert_eq!(
    refs_of(git, &mirror),
    refs_of(git, source),
    "the mirror clone's refs are the source's"
  );
  let work = git.copy("work");
  git.ok(&git.scratch, &["clone", "-q", url, &work]);
  let branch = git
    .ok(&work, &["symbolic-ref", "--short", "HEAD"])
    .trim()
    .to_owned();
  put(&work, "carried\n");
  git.ok(&work, &["add", FILE]);
  git.ok(&work, &["commit", "-q", "-m", "carried"]);
  git.ok(
    &work,
    &[
      "push",
      "-q",
      "origin",
      &branch,
      "HEAD:refs/heads/slates-topic",
    ],
  );
  git.ok(&mirror, &["fetch", "-q", "--prune", "origin"]);
  let listed = git.ok(&git.scratch, &["ls-remote", url]);
  let mirrored = git.ok(&git.scratch, &["ls-remote", &mirror]);
  assert_eq!(
    compared(&listed),
    compared(&mirrored),
    "the fetch brings the work clone's pushes"
  );
  git.ok(&mirror, &["fsck", "--full", "--strict", "--no-progress"]);
  let shallow = git.copy("shallow");
  git.ok(
    &git.scratch,
    &["clone", "-q", "--depth", "1", url, &shallow],
  );
  assert_eq!(
    git.ok(&shallow, &["rev-list", "--count", "HEAD"]).trim(),
    "1",
    "one commit deep"
  );
  git.ok(&shallow, &["fsck", "--no-progress"]);
  git.ok(&shallow, &["fetch", "-q", "--deepen", "1"]);
  assert_eq!(
    git.ok(&shallow, &["rev-list", "--count", "HEAD"]).trim(),
    "2",
    "deepened by one"
  );
  let partial = git.copy("partial");
  git.ok(
    &git.scratch,
    &["clone", "-q", "--filter=blob:none", url, &partial],
  );
  git.ok(&partial, &["fsck", "--no-progress"]);
  let tip = git.commit_of(&partial, "HEAD");
  let ours = archive(git, &partial, &tip, &git.copy("partial.tar"));
  let theirs = archive(git, &mirror, &tip, &git.copy("mirror.tar"));
  assert!(
    ours == theirs,
    "the partial clone's checkout is the full one's, byte for byte"
  );
}

/// UI goal 93's git correctness by use (main's lane, 2026-11-21; CLAUDE.md §8: an interop round, run by main). Do: make
/// a bare mirror of the repository [`DATASET`] names and give it tags of every kind ([`tagged_source`]); on each edge
/// [`EDGES`] names (HTTP with a bearer token, SSH with an enrolled key), run [`whole_session`] against an empty
/// repository; over HTTP, also read the edge's `git-upload-pack` advertisement right after the push and compare its ref
/// lines byte for byte with stock git's own `http-backend` serving the source. Expect every step as stated, every
/// listing, ref and archive equal, and the advertisements' ref lines identical; each edge's mirror clone says its time,
/// its pack bytes beside stock git's own clone's and the paths its objects took ([`CloneReport`], said a clone killed past
/// [`PATIENCE`] too). Skips loudly without stock git, the
/// dataset, OpenSSH (the SSH edge) or curl (the advertisement), as the round trips do.
#[test]
#[ignore = "an interop round over a whole repository (CLAUDE.md §8: main runs it): SLATES_TEST_GIT_DATASET=<repo> cargo test --release -p slates-cli --test git_interop -- --ignored --exact stock_git_carries_a_whole_repository_over_each_edge"]
fn stock_git_carries_a_whole_repository_over_each_edge() {
  let Some(program) = stock_git() else {
    eprintln!("skipping the whole-repository session: no stock git here (set {STOCK_GIT})");
    return;
  };
  let Ok(dataset) = std::env::var(DATASET) else {
    eprintln!(
      "skipping the whole-repository session: no dataset (set {DATASET} to a repository, a git/git clone)"
    );
    return;
  };
  let edges = std::env::var(EDGES).unwrap_or_else(|_| "http,ssh".to_owned());
  let scratch = Scratch::new("git-whole");
  let plain = Git::new(&program, &scratch, "source", Vec::new());
  let source = format!("{}/source.git", scratch.path);
  tagged_source(&plain, &dataset, &source);
  let stock = stock_clone_bytes(&plain, &source);
  if edges.split(',').any(|edge| edge == "http") {
    let Some(leg) = http_leg("whole") else {
      return;
    };
    let header = format!("http.extraHeader=Authorization: Bearer {}", leg.token);
    let git = Git::new(&program, &scratch, "http", vec!["-c".to_owned(), header]);
    let url = format!("http://127.0.0.1:{}/{REPOSITORY}", leg.port);
    git.ok(
      &source,
      &[
        "push",
        "-q",
        &url,
        "refs/heads/*:refs/heads/*",
        "refs/tags/*:refs/tags/*",
      ],
    );
    match edge_advertisement(leg.port, &leg.token) {
      Some(edge) => {
        let stock = http_backend_advertisement(&plain, &scratch.path, "source.git");
        assert_eq!(
          compared(&advertised(&edge)),
          compared(&advertised(&stock)),
          "the edge advertises the refs stock http-backend does, byte for byte"
        );
      }
      None => eprintln!("skipping the advertisement's byte compare: no curl here"),
    }
    whole_session(&git, &source, (&url, &leg.instance, stock));
    eprintln!("whole-repository session over HTTP: every step held");
  }
  if edges.split(',').any(|edge| edge == "ssh") {
    if !openssh() {
      eprintln!("skipping the whole-repository session over SSH: no OpenSSH ssh here");
      return;
    }
    let Some((key, public)) = fresh_key(&scratch) else {
      eprintln!("skipping the whole-repository session over SSH: no ssh-keygen here");
      return;
    };
    let instance = format!("git-whole-ssh-{}", std::process::id());
    let started = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
    let (port, fingerprint) = started
      .lines
      .iter()
      .find_map(|line| ssh_edge_of(line))
      .expect("the daemon named its SSH edge's port and host key");
    let Some(issuer) = issuer_of(&started, "the whole-repository session over SSH") else {
      return;
    };
    let known_hosts = known_hosts(&scratch, port, &fingerprint);
    create_repository(&instance);
    let (code, out, err) = slates(&instance, &issuer, &["credential", "key", &public]);
    assert_eq!(code, 0, "credential key: {out}{err}");
    let ssh = format!(
      "core.sshCommand=ssh -F /dev/null -i {key} -o IdentitiesOnly=yes -o BatchMode=yes \
       -o StrictHostKeyChecking=yes -o UserKnownHostsFile={known_hosts} -o GlobalKnownHostsFile=/dev/null"
    );
    let git = Git::new(&program, &scratch, "ssh", vec!["-c".to_owned(), ssh]);
    whole_session(
      &git,
      &source,
      (
        &format!("ssh://{USER}@127.0.0.1:{port}/{REPOSITORY}"),
        &instance,
        stock,
      ),
    );
    eprintln!("whole-repository session over SSH: every step held");
  }
}

/// Format: the Windows-paths fixture's checkout-safe files ([`windows_paths_source`]): each path and its bytes. They
/// stress a Windows checkout without being refused by it: line endings under every kind of attribute (a text file
/// converted to CRLF, one held at LF, a file committed with CRLF in its blob, a file mixing both, a binary), a path
/// longer than Windows' 260-character `MAX_PATH`, two names that differ only by Unicode normal form, and spaces and
/// parentheses.
const SAFE_PATHS: [(&str, &[u8]); 10] = [
  (
    ".gitattributes",
    b"*.txt text eol=crlf\n*.sh text eol=lf\n*.bin binary\n*.auto text=auto\n",
  ),
  ("crlf.txt", b"one\ntwo\nthree\n"),
  ("lf.sh", b"#!/bin/sh\necho lf\n"),
  ("committed-crlf.auto", b"crlf in the blob\r\nsecond\r\n"),
  ("mixed.auto", b"lf line\ncrlf line\r\nlf again\n"),
  ("bytes.bin", b"\x00\x01\r\n\n\r\xff"),
  (LONG_PATH, b"a path longer than MAX_PATH\n"),
  ("unicode/caf\u{e9}.txt", b"NFC e-acute\n"),
  (
    "unicode/cafe\u{301}-nfd.txt",
    b"NFD e and a combining acute\n",
  ),
  ("spaces and (parens)/x y.txt", b"spaces\n"),
];

/// Format: a path past Windows' 260-character `MAX_PATH`: thirty 10-character directories under `deep`, 347 characters.
const LONG_PATH: &str = "deep/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/\
abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/\
abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefghij/\
abcdefghij/abcdefghij/past-260.txt";

/// Format: the Windows-paths fixture's names a Windows checkout refuses: reserved device names (as a file, with an
/// extension, as a directory), two names equal but for case, and a trailing dot. They live only on the `hostile`
/// branch, compared as objects, never checked out.
const HOSTILE_PATHS: [(&str, &[u8]); 7] = [
  ("CON.txt", b"reserved: CON\n"),
  ("aux.c", b"reserved: aux\n"),
  ("com1/inner.txt", b"reserved directory com1\n"),
  ("nul", b"reserved: nul\n"),
  ("Readme.md", b"case one\n"),
  ("README.md", b"case two\n"),
  ("trailing.dot.", b"trailing dot\n"),
];

/// Makes the Windows-paths fixture as a bare repository at `source`, with git's plumbing so no name touches this
/// host's disk: branch `main` holds [`SAFE_PATHS`]; branch `hostile`, a child of it, adds [`HOSTILE_PATHS`]; a
/// lightweight tag names `main` and an annotated tag names `hostile`. Each blob is stored as its bytes
/// (`hash-object --no-filters`), so the line endings are the fixture's own.
#[allow(clippy::disallowed_methods)] // the fixture's blobs are written in the test's scratch directory
fn windows_paths_source(git: &Git, source: &str) {
  let build = git.copy("paths-build");
  git.ok(&git.scratch, &["init", "-q", "--bare", source]);
  git.ok(&git.scratch, &["init", "-q", &build]);
  let blob_file = git.copy("paths-blob");
  let add = |path: &str, bytes: &[u8]| {
    std::fs::write(&blob_file, bytes).unwrap();
    let id = git.ok(&build, &["hash-object", "-w", "--no-filters", &blob_file]);
    git.ok(
      &build,
      &[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{},{path}", id.trim()),
      ],
    );
  };
  for (path, bytes) in SAFE_PATHS {
    add(path, bytes);
  }
  let tree = git.ok(&build, &["write-tree"]);
  let main = git.ok(
    &build,
    &[
      "commit-tree",
      tree.trim(),
      "-m",
      "windows paths: checkout-safe",
    ],
  );
  for (path, bytes) in HOSTILE_PATHS {
    add(path, bytes);
  }
  let tree = git.ok(&build, &["write-tree"]);
  let hostile = git.ok(
    &build,
    &[
      "commit-tree",
      tree.trim(),
      "-p",
      main.trim(),
      "-m",
      "windows paths: names a Windows disk refuses",
    ],
  );
  git.ok(
    &build,
    &[
      "push",
      "-q",
      source,
      &format!("{}:refs/heads/main", main.trim()),
      &format!("{}:refs/heads/hostile", hostile.trim()),
    ],
  );
  git.ok(source, &["tag", "paths-light", "main"]);
  git.ok(
    source,
    &["tag", "-a", "-m", "annotated", "paths-annotated", "hostile"],
  );
}

/// Every object reachable from the branches and tags of the repository at `dir`, sorted.
fn reachable_objects(git: &Git, dir: &str) -> Vec<String> {
  let mut objects: Vec<String> = git
    .ok(dir, &["rev-list", "--branches", "--tags", "--objects"])
    .lines()
    .map(|line| line.split(' ').next().unwrap_or_default().to_owned())
    .collect();
  objects.sort();
  objects.dedup();
  objects
}

/// Every file under `root`, `.git` left out, by its path relative to `root` and its bytes, sorted by path.
#[allow(clippy::disallowed_methods)] // reads the test's own checkouts
fn tree_bytes(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
  let mut files = Vec::new();
  let mut pending = vec![root.to_path_buf()];
  while let Some(dir) = pending.pop() {
    for entry in std::fs::read_dir(&dir).unwrap() {
      let path = entry.unwrap().path();
      if path.file_name().is_some_and(|name| name == ".git") {
        continue;
      }
      if path.is_dir() {
        pending.push(path);
      } else {
        let relative = path
          .strip_prefix(root)
          .unwrap()
          .to_string_lossy()
          .replace('\\', "/");
        files.push((relative, std::fs::read(&path).unwrap()));
      }
    }
  }
  files.sort();
  files
}

/// UI goal 93's git correctness on Windows' own names, by use (main's lane for jupiter, 2026-11-21; CLAUDE.md §8: an
/// interop round, run by main). Do: make the Windows-paths fixture ([`windows_paths_source`]); start an anchor with its
/// HTTP edge and its version-control store; push every branch and tag to an empty repository with stock git; then read
/// it back: the listing, a mirror clone (`fsck --full --strict`, its refs and every reachable object the source's), and
/// a checkout of `main` under `core.autocrlf` false and true, each against a checkout of the source made with the same
/// setting, and `git status` in it. Expect every listing, ref and object equal, the fsck clean, each checkout's files
/// the source checkout's byte for byte (line endings, the long path, both Unicode forms), and the status clean.
/// `core.longPaths` is on, as Git for Windows needs for the long path; elsewhere git ignores it. Skips loudly without
/// stock git.
#[test]
#[ignore = "an interop round (CLAUDE.md §8: main runs it, on Windows at jupiter): cargo test --release -p slates-cli --test git_interop -- --ignored --exact stock_git_carries_windows_paths_byte_identically"]
fn stock_git_carries_windows_paths_byte_identically() {
  let Some(program) = stock_git() else {
    eprintln!("skipping the Windows-paths session: no stock git here (set {STOCK_GIT})");
    return;
  };
  let scratch = Scratch::new("git-windows-paths");
  let long_paths = vec!["-c".to_owned(), "core.longPaths=true".to_owned()];
  let plain = Git::new(&program, &scratch, "source", long_paths.clone());
  let source = format!("{}/source.git", scratch.path);
  windows_paths_source(&plain, &source);
  let Some(leg) = http_leg("windows-paths") else {
    return;
  };
  let mut config = long_paths;
  config.extend([
    "-c".to_owned(),
    format!("http.extraHeader=Authorization: Bearer {}", leg.token),
  ]);
  let git = Git::new(&program, &scratch, "http", config);
  let url = format!("http://127.0.0.1:{}/{REPOSITORY}", leg.port);
  git.ok(
    &source,
    &[
      "push",
      "-q",
      &url,
      "refs/heads/*:refs/heads/*",
      "refs/tags/*:refs/tags/*",
    ],
  );
  assert_eq!(
    compared(&git.ok(&git.scratch, &["ls-remote", &url])),
    compared(&plain.ok(&plain.scratch, &["ls-remote", &source])),
    "the pushed listing is the source's"
  );
  let mirror = git.copy("mirror.git");
  git.ok(&git.scratch, &["clone", "-q", "--mirror", &url, &mirror]);
  git.ok(&mirror, &["fsck", "--full", "--strict", "--no-progress"]);
  assert_eq!(
    refs_of(&git, &mirror),
    refs_of(&plain, &source),
    "the mirror clone's refs are the source's"
  );
  assert_eq!(
    reachable_objects(&git, &mirror),
    reachable_objects(&plain, &source),
    "the mirror clone holds every object the source's branches and tags reach, and no other"
  );
  for autocrlf in ["false", "true"] {
    let ours = git.copy(&format!("checkout-{autocrlf}"));
    let theirs = plain.copy(&format!("checkout-{autocrlf}"));
    let setting = format!("core.autocrlf={autocrlf}");
    git.ok(
      &git.scratch,
      &["-c", &setting, "clone", "-q", "-b", "main", &url, &ours],
    );
    plain.ok(
      &plain.scratch,
      &[
        "-c",
        &setting,
        "clone",
        "-q",
        "-b",
        "main",
        &file_url(&source),
        &theirs,
      ],
    );
    let checked_out = tree_bytes(std::path::Path::new(&ours));
    assert_eq!(
      checked_out.len(),
      SAFE_PATHS.len(),
      "every file of main is checked out (autocrlf {autocrlf})"
    );
    assert!(
      checked_out == tree_bytes(std::path::Path::new(&theirs)),
      "main's checkout from slates is the source's, byte for byte (autocrlf {autocrlf})"
    );
    assert_eq!(
      git.ok(&ours, &["-c", &setting, "status", "--porcelain"]),
      "",
      "git status is clean in the checkout from slates (autocrlf {autocrlf})"
    );
  }
  eprintln!("Windows-paths session over HTTP: every step held");
}
