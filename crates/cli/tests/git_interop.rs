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
//! the anchor names on macOS and Windows; on Linux the surface is a descriptor only the anchor's children hold, so a
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

/// Shape: whether the anchor names its issuer surface through the environment: on macOS and Windows; on Linux the
/// surface is a descriptor only its children hold (`tests/cli.rs`, `PRINTS_ISSUER_SURFACE`, which records why this is
/// a platform fact and never read from the order of two processes' lines).
const PRINTS_ISSUER_SURFACE: bool = cfg!(not(target_os = "linux"));

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
  settle(instance);
  let (code, out, err) = slates(instance, &[], &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap root: {out}{err}");
  Started {
    anchor,
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

/// Waits until the daemon answers [`STABLE_STREAK`] verbs in a row, within [`PATIENCE`].
fn settle(instance: &str) {
  let started = Instant::now();
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if slates(instance, &[], &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    assert!(
      started.elapsed() < PATIENCE,
      "the daemon answered {STABLE_STREAK} verbs in a row within {PATIENCE:?}"
    );
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
      "skipping {leg}: this platform's issuer surface is a descriptor only the anchor's children hold, so no \
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
