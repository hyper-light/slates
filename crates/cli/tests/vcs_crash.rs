//! Crash consistency of the version-control servers (UI goal 93, correctness; §4.17, §4.8 "persistence before
//! reply"): the daemon is killed (`kill -9`, as a crash ends it) at points spread through a stock client's push and
//! clone, and after its anchor restarts it every repository is exactly as it was before the operation or after it,
//! never between; the client that was interrupted ends with an error rather than hanging; and the same operation tried
//! again succeeds.
//!
//! **How a point is chosen.** A reference run of the same operation measures how long it takes from the client's
//! spawn to its end, `T`; point `k` of `N` kills the daemon `k·T/(N+1)` after the client's spawn, so the kills fall
//! through the operation's whole span, from its first request to its last reply, whatever the machine's speed. Each
//! point runs against a repository of its own, so its "before" state is known exactly.
//!
//! **What is judged, per point.**
//! - The repository's branch names its commit before the operation or its commit after it, never another, and a mirror
//!   of it passes `git fsck --strict` (every object it names is whole and reachable).
//! - An operation the client saw succeed is never lost: a push answered success is there after the restart.
//! - The client ends within [`PATIENCE`], with success or an error, never a hang.
//! - The same operation, tried again once the daemon serves, succeeds, and leaves the repository after it.
//!
//! A point that breaks any of these is printed as a `VIOLATION` line with what was seen, and the test fails at its
//! end naming every one; each line is what an owner's red test reproduces. Every point prints a `POINT` line, so a run's
//! log is its record.
//!
//! These are sweeps, run on demand (`cargo test --release -p slates-cli --test vcs_crash -- --ignored --nocapture`),
//! each skipping loudly without its stock client. Each test's scratch lives in the build output, named with the
//! process id and removed at its end (A-50).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::unwrap_in_result,
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

/// Shape: the variable naming how many kill points each sweep makes; unset, [`DEFAULT_POINTS`].
const POINTS_VARIABLE: &str = "SLATES_CRASH_POINTS";

/// Shape: the kill points of a sweep when [`POINTS_VARIABLE`] is unset: twelve, so the kills fall about every twelfth of
/// the operation's span, through its requests and its replies, at a cost of about a minute a point.
const DEFAULT_POINTS: usize = 12;

/// Shape: the files of the large commit each push carries and each clone reads, and the bytes of each: 256 files of
/// 32 KiB, 8 MiB of incompressible content, so the operation lasts long enough on any machine for kills to fall inside
/// its pack's transfer and its storing, not only around them.
const PAYLOAD_FILES: usize = 256;
/// Shape: see [`PAYLOAD_FILES`].
const PAYLOAD_FILE_BYTES: usize = 32 * 1024;

/// Format: the repository name prefix each point's repository takes; the point's number follows.
const REPOSITORY_PREFIX: &str = "crash";

/// Format: the branch every repository serves: the one `slates repository create` binds a git repository's root to.
const BRANCH: &str = "main";

/// Shape: the shards of each test's daemon, as `tests/cli.rs` starts one.
const SHARDS: &str = "2";

/// Shape: consecutive answered verbs before a daemon counts as settled, as `tests/cli.rs`'s start waits.
const STABLE_STREAK: u32 = 10;

/// Shape: how long one stock client command may run before it counts as hung: a guard far above what any of these
/// operations takes, never a measure of one.
const PATIENCE: Duration = Duration::from_secs(180);

/// Shape: the pause between polls of a settling daemon and of a running client.
const POLL: Duration = Duration::from_millis(20);

/// Format: the start of the line a daemon prints once it serves (`crates/api/src/role/daemon.rs`).
const SERVING_LINE: &str = "slates daemon: instance ";

/// Format: the anchor's line when its daemon is in a crash loop (`crates/api/src/role/anchor.rs`).
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";

/// Format: the start of the anchor's issuer-surface line (`crates/api/src/role/anchor.rs`).
const ISSUER_SURFACE_LINE: &str = "slates anchor: issuer surface: export ";

/// Format: the start of the daemon's line naming its HTTP edge's cleartext port on loopback (A-625 part 3).
const VCS_HTTP_LINE: &str =
  "slates daemon: the version-control HTTP edge serves cleartext on 127.0.0.1:";

/// Format: the start of the daemon's line naming its svn:// listener's port on loopback (A-689).
const VCS_SVN_LINE: &str = "slates daemon: the svn:// listener serves on 127.0.0.1:";

/// Format: the user name stock hg gives the HTTP edge's Basic route, the token its password; slates takes the
/// principal from the token.
const HG_USER: &str = "slates";

/// Format: the start of `git --version`'s answer.
const GIT_VERSION: &str = "git version ";

/// A scratch directory in the build output, named with the process id and a count, removed when dropped.
struct Scratch {
  path: String,
}

impl Scratch {
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
      path: std::fs::canonicalize(&raw)
        .unwrap()
        .to_string_lossy()
        .into_owned(),
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
  std::fs::create_dir_all(path).unwrap();
}

/// Writes `bytes` at `path`, inside the test's scratch directory.
#[allow(clippy::disallowed_methods)]
fn write_file(path: &str, bytes: &[u8]) {
  std::fs::write(path, bytes).unwrap();
}

/// The sweep's kill points: [`POINTS_VARIABLE`] when set, else [`DEFAULT_POINTS`].
fn points() -> usize {
  std::env::var(POINTS_VARIABLE)
    .ok()
    .and_then(|value| value.parse().ok())
    .unwrap_or(DEFAULT_POINTS)
}

/// `count` pseudo-random bytes from `seed` (xorshift64): incompressible, so a pack carries them whole.
fn noise(seed: u64, count: usize) -> Vec<u8> {
  let mut state = seed | 1;
  (0..count)
    .map(|_| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state.to_le_bytes()[0]
    })
    .collect()
}

/// Reads `from` to its end on a thread of its own.
fn read_on_a_thread(mut from: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
  std::thread::spawn(move || {
    let mut bytes = Vec::new();
    let _ = from.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
  })
}

fn pause(for_how_long: Duration) {
  // The harness paces its polls and its kills; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(for_how_long);
}

/// A running stock client: its process and its outputs' readers.
struct Running {
  child: Child,
  out: std::thread::JoinHandle<String>,
  err: std::thread::JoinHandle<String>,
  started: Instant,
}

/// How a client ended: its exit code (`None` when it ran past [`PATIENCE`] and was killed), both outputs, and how long
/// it ran.
#[derive(Debug)]
struct Ended {
  code: Option<i32>,
  out: String,
  err: String,
  took: Duration,
}

impl Running {
  /// Waits for the client to end within [`PATIENCE`], killing it past that.
  fn end(mut self) -> Ended {
    let code = loop {
      if let Some(status) = self.child.try_wait().unwrap() {
        break Some(status.code().unwrap_or(-1));
      }
      if self.started.elapsed() > PATIENCE {
        let _ = self.child.kill();
        let _ = self.child.wait();
        break None;
      }
      pause(POLL);
    };
    Ended {
      code,
      out: self.out.join().unwrap(),
      err: self.err.join().unwrap(),
      took: self.started.elapsed(),
    }
  }
}

/// Stock git in a scratch home: no system's or user's configuration, never a prompt, one committer, C's messages, and
/// the bearer credential on every request.
struct Git {
  program: Vec<String>,
  home: String,
  config: Vec<String>,
}

impl Git {
  /// Stock git, or `None` without it, which the caller skips loudly.
  fn stock(scratch: &Scratch) -> Option<Git> {
    let line = std::env::var("SLATES_TEST_GIT").unwrap_or_else(|_| "git".to_owned());
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
    let home = format!("{}/home", scratch.path);
    made_directory(&home);
    Some(Git {
      program: words,
      home,
      config: Vec::new(),
    })
  }

  /// Starts git with `args` in `dir`.
  fn spawn(&self, dir: &str, args: &[&str]) -> Running {
    let (program, leading) = self.program.split_first().unwrap();
    let mut child = Command::new(program)
      .args(leading)
      .args(&self.config)
      .args(args)
      .current_dir(dir)
      .envs([
        ("HOME", self.home.as_str()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
      ])
      .env_remove("GIT_ASKPASS")
      .env_remove("SSH_ASKPASS")
      .env_remove("GIT_DIR")
      .env_remove("GIT_WORK_TREE")
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let out = read_on_a_thread(child.stdout.take().unwrap());
    let err = read_on_a_thread(child.stderr.take().unwrap());
    Running {
      child,
      out,
      err,
      started: Instant::now(),
    }
  }

  /// Runs git with `args` in `dir` to its end.
  fn run(&self, dir: &str, args: &[&str]) -> Ended {
    self.spawn(dir, args).end()
  }

  /// Runs git, failing the test unless it succeeds: its standard output.
  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let ended = self.run(dir, args);
    assert_eq!(ended.code, Some(0), "git {args:?} in {dir}: {ended:?}");
    ended.out
  }

  /// The commit the branch `refs/heads/BRANCH` names at `url`, as `git ls-remote` answers, or `None` when it names
  /// none; `Err` with what git said when it could not answer.
  fn remote_head(&self, dir: &str, url: &str) -> Result<Option<String>, String> {
    let ended = self.run(dir, &["ls-remote", url]);
    if ended.code != Some(0) {
      return Err(format!("{ended:?}"));
    }
    let wanted = format!("refs/heads/{BRANCH}");
    Ok(ended.out.lines().find_map(|line| {
      let (commit, name) = line.split_once('\t')?;
      (name == wanted).then(|| commit.to_owned())
    }))
  }
}

/// The anchor process, killed and reaped when dropped; what it says after its daemon served is copied to this
/// process's standard error by `drain`.
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

/// A started anchor with its HTTP edge: the process, its instance, the edge's port, the issuer surface, and the
/// changeset store (declared after the anchor, so the daemon is gone before it is removed).
struct Served {
  anchor: Anchor,
  instance: String,
  port: u16,
  issuer: Vec<(String, String)>,
  _store: Scratch,
}

/// Runs `slates --instance INSTANCE ARGS` with `issuer`'s variables: its exit code and both outputs.
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

/// The listener a sweep's client reaches the daemon on.
#[derive(Clone, Copy)]
enum Edge {
  /// The HTTP edge (`--vcs-http`), git's and hg's route.
  Http,
  /// The svn:// listener (`--vcs-svn`), Subversion's own protocol.
  Svn,
}

impl Edge {
  /// The anchor's flag that asks for the listener.
  fn flag(self) -> &'static str {
    match self {
      Edge::Http => "--vcs-http",
      Edge::Svn => "--vcs-svn",
    }
  }

  /// The start of the daemon's line naming the listener's port.
  fn line(self) -> &'static str {
    match self {
      Edge::Http => VCS_HTTP_LINE,
      Edge::Svn => VCS_SVN_LINE,
    }
  }
}

/// Starts `slates anchor` with listener `edge` on loopback and a changeset store, waits until its daemon serves and
/// answers verbs steadily, and makes the root consensus group.
fn serve(instance: &str, edge: Edge) -> Served {
  let store = Scratch::new("vcs-crash-store");
  let mut child = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
      edge.flag(),
      "127.0.0.1:0",
      "--vcs-store",
      &store.path,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let mut anchor = Anchor { child, drain: None };
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (mut port, mut issuer, mut served) = (None, None, false);
  while !served || issuer.is_none() || port.is_none() {
    let mut line = String::new();
    let read = stderr.read_line(&mut line).unwrap();
    assert!(read > 0, "the anchor ended before its daemon served");
    let _ = std::io::stderr().write_all(line.as_bytes());
    assert!(
      !line.starts_with(CRASH_LOOP_LINE),
      "the daemon never served: {line}"
    );
    served |= line.starts_with(&serving);
    if let Some(rest) = line.strip_prefix(edge.line()) {
      port = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok());
    }
    if let Some(pairs) = line.trim().strip_prefix(ISSUER_SURFACE_LINE) {
      issuer = Some(
        pairs
          .split(' ')
          .filter_map(|pair| pair.split_once('='))
          .map(|(name, value)| (name.to_owned(), value.to_owned()))
          .collect(),
      );
    }
  }
  anchor.drain = Some(std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  }));
  settle(instance, anchor.child.id());
  let (code, out, err) = slates(instance, &[], &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap root: {out}{err}");
  Served {
    anchor,
    instance: instance.to_owned(),
    port: port.unwrap(),
    issuer: issuer.unwrap(),
    _store: store,
  }
}

/// Waits until the daemon answers [`STABLE_STREAK`] verbs in a row, while the tree under `anchor` gains CPU.
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
    pause(POLL);
  }
}

/// The daemon's pid as `slates status --json` names it.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (_, out, _) = slates(instance, &[], &["status", "--json"]);
  out
    .split("\"pid\":")
    .nth(1)?
    .trim_start()
    .split(|character: char| !character.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Ends process `pid` at once, as a crash ends it.
fn kill(pid: u32) {
  let status = Command::new("kill").args(["-9", &pid.to_string()]).status();
  assert!(status.unwrap().success(), "the daemon {pid} was killed");
}

impl Served {
  /// Makes repository `name` for `tool` (`git`, `hg` or `svn`).
  fn create(&self, name: &str, tool: &str) {
    let (code, out, err) = slates(
      &self.instance,
      &[],
      &["repository", "create", name, "--tool", tool],
    );
    assert_eq!(code, 0, "repository create {name}: {out}{err}");
  }

  /// A token for this process's principal, the repositories' owner: its id and the token.
  fn token(&self) -> (String, String) {
    let (code, out, err) = slates(&self.instance, &self.issuer, &["credential", "token"]);
    assert_eq!(code, 0, "credential token: {out}{err}");
    (value_of(&out, "id"), value_of(&out, "token"))
  }

  /// The URL of repository `name` on the HTTP edge.
  fn url(&self, name: &str) -> String {
    format!("http://127.0.0.1:{}/{name}", self.port)
  }

  /// The URL of repository `name` on the svn:// listener.
  fn svn_url(&self, name: &str) -> String {
    format!("svn://127.0.0.1:{}/{name}", self.port)
  }

  /// Kills the daemon after `delay` from `since`, then waits until the anchor's successor serves steadily: the pid
  /// killed and the successor's.
  fn crash_at(&self, since: Instant, delay: Duration, pid: u32) -> (u32, Option<u32>) {
    let elapsed = since.elapsed();
    if delay > elapsed {
      pause(delay - elapsed);
    }
    kill(pid);
    settle(&self.instance, self.anchor.child.id());
    (pid, daemon_pid(&self.instance))
  }
}

/// A local repository holding two commits on [`BRANCH`]: `before`, one small file, and `after`, on top of it, the
/// large payload ([`PAYLOAD_FILES`] of [`PAYLOAD_FILE_BYTES`]).
struct Work {
  dir: String,
  before: String,
  after: String,
}

/// Writes the large payload under `dir/payload`: [`PAYLOAD_FILES`] files of [`PAYLOAD_FILE_BYTES`] incompressible bytes.
fn write_payload(dir: &str) {
  made_directory(&format!("{dir}/payload"));
  for file in 0..PAYLOAD_FILES {
    write_file(
      &format!("{dir}/payload/{file:04}"),
      &noise(file as u64 + 1, PAYLOAD_FILE_BYTES),
    );
  }
}

/// The violations a sweep found, each printed as it is found and named again when the test fails at its end.
struct Faults {
  sweep: &'static str,
  found: Vec<String>,
}

impl Faults {
  fn new(sweep: &'static str) -> Faults {
    Faults {
      sweep,
      found: Vec::new(),
    }
  }

  /// Records a violation at point `point`, killed `delay` after the client started.
  fn add(&mut self, point: usize, delay: Duration, what: String) {
    eprintln!("VIOLATION {} point {point}: {what}", self.sweep);
    self.found.push(format!(
      "{} point {point} (kill at {delay:?}): {what}",
      self.sweep
    ));
  }

  /// Records each of `checks` found (its flag true) as a violation at point `point`, killed `delay` after the client
  /// started.
  fn add_each(&mut self, point: usize, delay: Duration, checks: Vec<(bool, String)>) {
    for (found, what) in checks {
      if found {
        self.add(point, delay, what);
      }
    }
  }

  /// Fails the test naming every violation, if any.
  fn assert_none(self) {
    assert!(
      self.found.is_empty(),
      "{} violations:\n{}",
      self.found.len(),
      self.found.join("\n")
    );
  }
}

/// What a check refused, or nothing where it passed.
fn failure_of(checked: &Result<(), String>) -> &str {
  checked.as_ref().err().map_or("", String::as_str)
}

/// The delay of point `point` of `count` into an operation of span `span`: `point·span/(count+1)`.
fn delay_of(span: Duration, point: usize, count: usize) -> Duration {
  span.mul_f64(point as f64 / (count + 1) as f64)
}

/// Makes [`Work`] in `scratch`.
fn work(git: &Git, scratch: &Scratch) -> Work {
  let dir = format!("{}/work", scratch.path);
  made_directory(&dir);
  git.ok(&dir, &["init", "-q", "-b", BRANCH]);
  write_file(&format!("{dir}/first"), b"before\n");
  git.ok(&dir, &["add", "first"]);
  git.ok(&dir, &["commit", "-q", "-m", "before"]);
  let before = git.ok(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
  write_payload(&dir);
  git.ok(&dir, &["add", "payload"]);
  git.ok(&dir, &["commit", "-q", "-m", "after"]);
  let after = git.ok(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
  Work { dir, before, after }
}

/// Fails unless a mirror of `url` passes `git fsck --strict`: `Ok` or what fsck said.
fn fsck_mirror(git: &Git, scratch: &Scratch, url: &str, name: &str) -> Result<(), String> {
  let mirror = format!("{}/{name}.git", scratch.path);
  let cloned = git.run(&scratch.path, &["clone", "-q", "--mirror", url, &mirror]);
  if cloned.code != Some(0) {
    return Err(format!("mirror clone: {cloned:?}"));
  }
  let fsck = git.run(&mirror, &["fsck", "--strict", "--no-dangling"]);
  if fsck.code != Some(0) {
    return Err(format!("fsck: {fsck:?}"));
  }
  Ok(())
}

/// The last line of `text` that has any words: what a client said last, for a point's record.
fn last_words(text: &str) -> &str {
  text
    .lines()
    .rev()
    .find(|line| !line.trim().is_empty())
    .unwrap_or("")
}

/// UI goal 93 correctness, crash consistency of a git push (§4.17.2 "A push"; §4.8 "persistence before reply"). Do:
/// an anchor with its HTTP edge and a changeset store; a local repository with a small commit (`before`) and the large
/// payload on top (`after`); measure a reference push of both; then for each point `k` of `N`, a repository of its own
/// holding `before`, a push of `after` with the daemon killed `k·T/(N+1)` after the push starts. Expect, after the
/// anchor's restart: the branch names `before` or `after`; a mirror passes `git fsck --strict`; a push the client saw
/// succeed is `after`; the client ended within [`PATIENCE`]; and a second push succeeds and leaves `after`.
#[test]
#[ignore = "a crash sweep: cargo test --release -p slates-cli --test vcs_crash -- --ignored --nocapture"]
fn a_git_push_killed_at_any_point_leaves_the_repository_before_or_after() {
  let scratch = Scratch::new("vcs-crash-git-push");
  let Some(mut git) = Git::stock(&scratch) else {
    eprintln!("skipping the git push sweep: no stock git here (set SLATES_TEST_GIT)");
    return;
  };
  let served = serve(
    &format!("crash-git-push-{}", std::process::id()),
    Edge::Http,
  );
  git.config = vec![
    "-c".to_owned(),
    format!(
      "http.extraHeader=Authorization: Bearer {}",
      served.token().1
    ),
  ];
  let work = work(&git, &scratch);
  let reference = format!("{REPOSITORY_PREFIX}-reference");
  served.create(&reference, "git");
  let url = served.url(&reference);
  git.ok(
    &work.dir,
    &[
      "push",
      "-q",
      &url,
      &format!("{}:refs/heads/{BRANCH}", work.before),
    ],
  );
  let measured = git.run(&work.dir, &["push", "-q", &url, BRANCH]);
  assert_eq!(measured.code, Some(0), "the reference push: {measured:?}");
  let span = measured.took;
  let count = points();
  eprintln!(
    "REFERENCE push of {PAYLOAD_FILES} x {PAYLOAD_FILE_BYTES} B took {span:?}; {count} points"
  );
  let mut faults = Faults::new("push");
  for point in 1..=count {
    let name = format!("{REPOSITORY_PREFIX}-push-{point}");
    served.create(&name, "git");
    let url = served.url(&name);
    git.ok(
      &work.dir,
      &[
        "push",
        "-q",
        &url,
        &format!("{}:refs/heads/{BRANCH}", work.before),
      ],
    );
    let delay = span.mul_f64(point as f64 / (count + 1) as f64);
    let pid = daemon_pid(&served.instance).expect("the daemon's pid");
    let push = git.spawn(&work.dir, &["push", "-q", &url, BRANCH]);
    let since = push.started;
    let (killed, successor) = served.crash_at(since, delay, pid);
    let ended = push.end();
    let head = git.remote_head(&scratch.path, &url);
    let fsck = fsck_mirror(&git, &scratch, &url, &format!("{name}-after-crash"));
    let retry = git.run(&work.dir, &["push", "-q", &url, BRANCH]);
    let head_after_retry = git.remote_head(&scratch.path, &url);
    let fsck_after_retry = fsck_mirror(&git, &scratch, &url, &format!("{name}-after-retry"));
    let state = match &head {
      Ok(Some(commit)) if *commit == work.before => "before",
      Ok(Some(commit)) if *commit == work.after => "after",
      Ok(Some(_)) => "OTHER",
      Ok(None) => "NO-BRANCH",
      Err(_) => "UNREADABLE",
    };
    eprintln!(
      "POINT push {point}/{count} kill at {delay:?} (pid {killed} -> {successor:?}): client {:?} in {:?} \
       ({:?}); state {state}; fsck {}; retry {:?} -> {}",
      ended.code,
      ended.took,
      last_words(&ended.err),
      fsck.as_ref().map_or("FAILED", |()| "ok"),
      retry.code,
      head_after_retry
        .as_ref()
        .ok()
        .and_then(|head| head.as_deref())
        .map_or("?", |commit| if commit == work.after {
          "after"
        } else {
          "OTHER"
        }),
    );
    let after_retry = head_after_retry
      .as_ref()
      .ok()
      .and_then(|head| head.as_deref());
    faults.add_each(
      point,
      delay,
      vec![
        (
          !matches!(state, "before" | "after"),
          format!("the branch is neither before nor after: {head:?}"),
        ),
        (
          fsck.is_err(),
          format!(
            "the repository failed fsck after the crash: {}",
            failure_of(&fsck)
          ),
        ),
        (
          ended.code == Some(0) && state != "after",
          format!("the client saw its push succeed, and the branch is {state}"),
        ),
        (
          ended.code.is_none(),
          format!("the client hung past {PATIENCE:?}: {ended:?}"),
        ),
        (
          retry.code != Some(0),
          format!("the retried push failed: {retry:?}"),
        ),
        (
          after_retry != Some(work.after.as_str()),
          format!("after the retry the branch is not after: {head_after_retry:?}"),
        ),
        (
          fsck_after_retry.is_err(),
          format!(
            "the repository failed fsck after the retry: {}",
            failure_of(&fsck_after_retry)
          ),
        ),
      ],
    );
  }
  faults.assert_none();
}

/// UI goal 93 correctness, crash consistency of a git clone (§4.17.2 "A fetch"). Do: an anchor as the push sweep
/// starts one; a repository holding `before` and `after`; measure a reference clone; then for each point `k` of `N`, a
/// clone with the daemon killed `k·T/(N+1)` after it starts. Expect, after the anchor's restart: the repository's branch
/// still names `after` and a mirror passes `git fsck --strict`; a clone the client saw succeed checked out `after`; the
/// client ended within [`PATIENCE`]; and a second clone succeeds and checks out `after`.
#[test]
#[ignore = "a crash sweep: cargo test --release -p slates-cli --test vcs_crash -- --ignored --nocapture"]
fn a_git_clone_killed_at_any_point_leaves_the_repository_whole_and_a_retry_clones() {
  let scratch = Scratch::new("vcs-crash-git-clone");
  let Some(mut git) = Git::stock(&scratch) else {
    eprintln!("skipping the git clone sweep: no stock git here (set SLATES_TEST_GIT)");
    return;
  };
  let served = serve(
    &format!("crash-git-clone-{}", std::process::id()),
    Edge::Http,
  );
  git.config = vec![
    "-c".to_owned(),
    format!(
      "http.extraHeader=Authorization: Bearer {}",
      served.token().1
    ),
  ];
  let work = work(&git, &scratch);
  let name = format!("{REPOSITORY_PREFIX}-clone");
  served.create(&name, "git");
  let url = served.url(&name);
  git.ok(&work.dir, &["push", "-q", &url, BRANCH]);
  let reference_dir = format!("{}/reference", scratch.path);
  let measured = git.run(&scratch.path, &["clone", "-q", &url, &reference_dir]);
  assert_eq!(measured.code, Some(0), "the reference clone: {measured:?}");
  let span = measured.took;
  let count = points();
  eprintln!("REFERENCE clone took {span:?}; {count} points");
  let mut violations = Vec::new();
  for point in 1..=count {
    let delay = span.mul_f64(point as f64 / (count + 1) as f64);
    let pid = daemon_pid(&served.instance).expect("the daemon's pid");
    let into = format!("{}/clone-{point}", scratch.path);
    let clone = git.spawn(&scratch.path, &["clone", "-q", &url, &into]);
    let since = clone.started;
    let (killed, successor) = served.crash_at(since, delay, pid);
    let ended = clone.end();
    let checked_out =
      (ended.code == Some(0)).then(|| git.run(&into, &["rev-parse", "HEAD"]).out.trim().to_owned());
    let head = git.remote_head(&scratch.path, &url);
    let fsck = fsck_mirror(&git, &scratch, &url, &format!("clone-{point}-after-crash"));
    let again = format!("{}/clone-{point}-again", scratch.path);
    let retry = git.run(&scratch.path, &["clone", "-q", &url, &again]);
    let retried_head = (retry.code == Some(0)).then(|| {
      git
        .run(&again, &["rev-parse", "HEAD"])
        .out
        .trim()
        .to_owned()
    });
    eprintln!(
      "POINT clone {point}/{count} kill at {delay:?} (pid {killed} -> {successor:?}): client {:?} in {:?} \
       ({:?}); branch {}; fsck {}; retry {:?}",
      ended.code,
      ended.took,
      last_words(&ended.err),
      head
        .as_ref()
        .ok()
        .and_then(|head| head.as_deref())
        .map_or("?", |commit| if commit == work.after {
          "after"
        } else {
          "OTHER"
        }),
      fsck.as_ref().map_or("FAILED", |()| "ok"),
      retry.code,
    );
    let mut fault = |what: String| {
      eprintln!("VIOLATION clone point {point}: {what}");
      violations.push(format!("clone point {point} (kill at {delay:?}): {what}"));
    };
    if head.as_ref().ok().and_then(|head| head.as_deref()) != Some(work.after.as_str()) {
      fault(format!("the repository's branch moved: {head:?}"));
    }
    if let Err(fsck) = &fsck {
      fault(format!(
        "the repository failed fsck after the crash: {fsck}"
      ));
    }
    if let Some(commit) = &checked_out
      && *commit != work.after
    {
      fault(format!(
        "the clone succeeded and checked out {commit}, not after"
      ));
    }
    if ended.code.is_none() {
      fault(format!("the client hung past {PATIENCE:?}: {ended:?}"));
    }
    if retried_head.as_deref() != Some(work.after.as_str()) {
      fault(format!(
        "the retried clone did not check out after: {retry:?} {retried_head:?}"
      ));
    }
  }
  assert!(
    violations.is_empty(),
    "{} violations:\n{}",
    violations.len(),
    violations.join("\n")
  );
}

/// Stock hg outside any user's configuration (`HGRCPATH` empty), in its plain words (`HGPLAIN`), committing as one test
/// user, with the HTTP edge's credential as `--config` words after every command.
struct Hg {
  program: Vec<String>,
  config: Vec<String>,
}

/// Format: the words stock hg says, exiting 1, where a push finds nothing to send: a retried push whose changesets
/// the crashed one already stored.
const HG_NOTHING_TO_PUSH: &str = "no changes found";

impl Hg {
  /// Stock hg, or `None` without it, which the caller skips loudly.
  fn stock() -> Option<Hg> {
    let line = std::env::var("SLATES_TEST_HG").unwrap_or_else(|_| "hg".to_owned());
    let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    let (program, leading) = words.split_first()?;
    let version = Command::new(program)
      .args(leading)
      .arg("--version")
      .env("HGPLAIN", "1")
      .env("HGRCPATH", "")
      .output()
      .ok()?;
    let text = String::from_utf8_lossy(&version.stdout).into_owned();
    if !version.status.success() || !text.contains("(version ") {
      return None;
    }
    eprintln!("stock hg: {}", text.lines().next().unwrap_or(""));
    Some(Hg {
      program: words,
      config: Vec::new(),
    })
  }

  fn spawn(&self, dir: &str, args: &[&str]) -> Running {
    let (program, leading) = self.program.split_first().unwrap();
    let mut child = Command::new(program)
      .args(leading)
      .args(args)
      .args(&self.config)
      .current_dir(dir)
      .env("HGPLAIN", "1")
      .env("HGRCPATH", "")
      .env("HGUSER", "Test <test@example.com>")
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let out = read_on_a_thread(child.stdout.take().unwrap());
    let err = read_on_a_thread(child.stderr.take().unwrap());
    Running {
      child,
      out,
      err,
      started: Instant::now(),
    }
  }

  fn run(&self, dir: &str, args: &[&str]) -> Ended {
    self.spawn(dir, args).end()
  }

  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let ended = self.run(dir, args);
    assert_eq!(ended.code, Some(0), "hg {args:?} in {dir}: {ended:?}");
    ended.out
  }

  /// The changeset `default` names at `url`, in full (`hg identify --debug`); `Err` with what hg said when it could not
  /// answer.
  fn remote_head(&self, dir: &str, url: &str) -> Result<String, String> {
    let ended = self.run(dir, &["identify", "--debug", "-r", "default", url]);
    if ended.code != Some(0) {
      return Err(format!("{ended:?}"));
    }
    Ok(ended.out.split_whitespace().next().unwrap_or("").to_owned())
  }

  /// `Ok` when a clone of `url` passes `hg verify`, else what failed.
  fn verified(&self, scratch: &Scratch, url: &str, name: &str) -> Result<(), String> {
    let clone = format!("{}/{name}", scratch.path);
    let cloned = self.run(&scratch.path, &["clone", "-U", url, &clone]);
    if cloned.code != Some(0) {
      return Err(format!("clone: {cloned:?}"));
    }
    let verify = self.run(&clone, &["verify"]);
    if verify.code != Some(0) {
      return Err(format!("verify: {verify:?}"));
    }
    Ok(())
  }

  /// Whether `ended`, a push, succeeded or found nothing to push.
  fn pushed(ended: &Ended) -> bool {
    ended.code == Some(0)
      || (ended.code == Some(1)
        && format!("{}{}", ended.out, ended.err).contains(HG_NOTHING_TO_PUSH))
  }
}

/// A local hg repository holding `before`, one small file, and `after` on top, the large payload.
fn hg_work(hg: &Hg, scratch: &Scratch) -> Work {
  let dir = format!("{}/hg-work", scratch.path);
  made_directory(&dir);
  hg.ok(&dir, &["init"]);
  write_file(&format!("{dir}/first"), b"before\n");
  hg.ok(&dir, &["add", "first"]);
  hg.ok(&dir, &["commit", "-m", "before"]);
  let before = hg.ok(&dir, &["log", "-r", ".", "--template", "{node}"]);
  write_payload(&dir);
  hg.ok(&dir, &["add", "payload"]);
  hg.ok(&dir, &["commit", "-m", "after"]);
  let after = hg.ok(&dir, &["log", "-r", ".", "--template", "{node}"]);
  Work { dir, before, after }
}

/// The anchor's HTTP edge and an hg client carrying a token for it as Basic's password.
fn hg_served(hg: &mut Hg, sweep: &str) -> Served {
  let served = serve(
    &format!("crash-hg-{sweep}-{}", std::process::id()),
    Edge::Http,
  );
  let prefix = format!("http://127.0.0.1:{}/", served.port);
  hg.config = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={}", served.token().1),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  served
}

/// UI goal 93 correctness, crash consistency of an hg push (§4.17.3; §4.8 "persistence before reply"). Do: as the git
/// push sweep, with stock hg over the HTTP edge: a repository per point holding `before`, a push of `after` with the
/// daemon killed `k·T/(N+1)` after it starts. Expect, after the restart: `default` names `before` or `after`; a clone
/// passes `hg verify`; a push the client saw succeed is `after`; the client ended within [`PATIENCE`]; and a second
/// push succeeds (or finds nothing to push) and leaves `after`.
#[test]
#[ignore = "a crash sweep: cargo test --release -p slates-cli --test vcs_crash -- --ignored --nocapture"]
fn an_hg_push_killed_at_any_point_leaves_the_repository_before_or_after() {
  let scratch = Scratch::new("vcs-crash-hg-push");
  let Some(mut hg) = Hg::stock() else {
    eprintln!("skipping the hg push sweep: no stock hg here (set SLATES_TEST_HG)");
    return;
  };
  let served = hg_served(&mut hg, "push");
  let work = hg_work(&hg, &scratch);
  let reference = format!("{REPOSITORY_PREFIX}-hg-reference");
  served.create(&reference, "hg");
  let url = served.url(&reference);
  hg.ok(&work.dir, &["push", "-r", &work.before, &url]);
  let measured = hg.run(&work.dir, &["push", &url]);
  assert_eq!(measured.code, Some(0), "the reference push: {measured:?}");
  let span = measured.took;
  let count = points();
  eprintln!("REFERENCE hg push took {span:?}; {count} points");
  let mut faults = Faults::new("hg push");
  for point in 1..=count {
    let name = format!("{REPOSITORY_PREFIX}-hg-push-{point}");
    served.create(&name, "hg");
    let url = served.url(&name);
    hg.ok(&work.dir, &["push", "-r", &work.before, &url]);
    let delay = delay_of(span, point, count);
    let pid = daemon_pid(&served.instance).expect("the daemon's pid");
    let push = hg.spawn(&work.dir, &["push", &url]);
    let (killed, successor) = served.crash_at(push.started, delay, pid);
    let ended = push.end();
    let head = hg.remote_head(&scratch.path, &url);
    let verified = hg.verified(&scratch, &url, &format!("{name}-after-crash"));
    let retry = hg.run(&work.dir, &["push", &url]);
    let head_after_retry = hg.remote_head(&scratch.path, &url);
    let verified_after_retry = hg.verified(&scratch, &url, &format!("{name}-after-retry"));
    let state = match &head {
      Ok(node) if *node == work.before => "before",
      Ok(node) if *node == work.after => "after",
      Ok(_) => "OTHER",
      Err(_) => "UNREADABLE",
    };
    eprintln!(
      "POINT hg push {point}/{count} kill at {delay:?} (pid {killed} -> {successor:?}): client {:?} in {:?} \
       ({:?}); state {state}; verify {}; retry {:?}",
      ended.code,
      ended.took,
      last_words(&ended.err),
      verified.as_ref().map_or("FAILED", |()| "ok"),
      retry.code,
    );
    faults.add_each(
      point,
      delay,
      vec![
        (
          !matches!(state, "before" | "after"),
          format!("default is neither before nor after: {head:?}"),
        ),
        (
          verified.is_err(),
          format!(
            "the repository failed hg verify after the crash: {}",
            failure_of(&verified)
          ),
        ),
        (
          ended.code == Some(0) && state != "after",
          format!("the client saw its push succeed, and default is {state}"),
        ),
        (
          ended.code.is_none(),
          format!("the client hung past {PATIENCE:?}: {ended:?}"),
        ),
        (
          !Hg::pushed(&retry),
          format!("the retried push failed: {retry:?}"),
        ),
        (
          head_after_retry.as_deref().ok() != Some(work.after.as_str()),
          format!("after the retry default is not after: {head_after_retry:?}"),
        ),
        (
          verified_after_retry.is_err(),
          format!(
            "the repository failed hg verify after the retry: {}",
            failure_of(&verified_after_retry)
          ),
        ),
      ],
    );
  }
  faults.assert_none();
}

/// UI goal 93 correctness, crash consistency of an hg clone (§4.17.3). Do: a repository holding `before` and `after`;
/// a reference clone measured; then a clone per point with the daemon killed `k·T/(N+1)` after it starts. Expect,
/// after the restart: `default` still names `after` and a clone passes `hg verify`; a clone the client saw succeed is
/// at `after`; the client ended within [`PATIENCE`]; and a second clone succeeds at `after`.
#[test]
#[ignore = "a crash sweep: cargo test --release -p slates-cli --test vcs_crash -- --ignored --nocapture"]
fn an_hg_clone_killed_at_any_point_leaves_the_repository_whole_and_a_retry_clones() {
  let scratch = Scratch::new("vcs-crash-hg-clone");
  let Some(mut hg) = Hg::stock() else {
    eprintln!("skipping the hg clone sweep: no stock hg here (set SLATES_TEST_HG)");
    return;
  };
  let served = hg_served(&mut hg, "clone");
  let work = hg_work(&hg, &scratch);
  let name = format!("{REPOSITORY_PREFIX}-hg-clone");
  served.create(&name, "hg");
  let url = served.url(&name);
  hg.ok(&work.dir, &["push", &url]);
  let measured = hg.run(
    &scratch.path,
    &["clone", "-U", &url, &format!("{}/reference", scratch.path)],
  );
  assert_eq!(measured.code, Some(0), "the reference clone: {measured:?}");
  let span = measured.took;
  let count = points();
  eprintln!("REFERENCE hg clone took {span:?}; {count} points");
  let mut faults = Faults::new("hg clone");
  for point in 1..=count {
    let delay = delay_of(span, point, count);
    let pid = daemon_pid(&served.instance).expect("the daemon's pid");
    let into = format!("{}/clone-{point}", scratch.path);
    let clone = hg.spawn(&scratch.path, &["clone", "-U", &url, &into]);
    let (killed, successor) = served.crash_at(clone.started, delay, pid);
    let ended = clone.end();
    let cloned_at = (ended.code == Some(0)).then(|| {
      hg.run(&into, &["log", "-r", "default", "--template", "{node}"])
        .out
    });
    let head = hg.remote_head(&scratch.path, &url);
    let verified = hg.verified(&scratch, &url, &format!("clone-{point}-after-crash"));
    let again = format!("{}/clone-{point}-again", scratch.path);
    let retry = hg.run(&scratch.path, &["clone", "-U", &url, &again]);
    let retried_at = (retry.code == Some(0)).then(|| {
      hg.run(&again, &["log", "-r", "default", "--template", "{node}"])
        .out
    });
    eprintln!(
      "POINT hg clone {point}/{count} kill at {delay:?} (pid {killed} -> {successor:?}): client {:?} in {:?} \
       ({:?}); default {}; verify {}; retry {:?}",
      ended.code,
      ended.took,
      last_words(&ended.err),
      if head.as_deref().ok() == Some(work.after.as_str()) {
        "after"
      } else {
        "OTHER"
      },
      verified.as_ref().map_or("FAILED", |()| "ok"),
      retry.code,
    );
    if head.as_deref().ok() != Some(work.after.as_str()) {
      faults.add(
        point,
        delay,
        format!("the repository's default moved: {head:?}"),
      );
    }
    if let Err(why) = &verified {
      faults.add(
        point,
        delay,
        format!("the repository failed hg verify after the crash: {why}"),
      );
    }
    if let Some(node) = &cloned_at
      && *node != work.after
    {
      faults.add(
        point,
        delay,
        format!("the clone succeeded at {node}, not after"),
      );
    }
    if ended.code.is_none() {
      faults.add(
        point,
        delay,
        format!("the client hung past {PATIENCE:?}: {ended:?}"),
      );
    }
    if retried_at.as_deref() != Some(work.after.as_str()) {
      faults.add(
        point,
        delay,
        format!("the retried clone is not at after: {retry:?} {retried_at:?}"),
      );
    }
  }
  faults.assert_none();
}

/// Stock Subversion's `svn` with no configuration of its own (`--config-dir` in the scratch directory), no credential
/// cached, never a prompt, the token as its credential, and C's messages.
struct Svn {
  options: Vec<String>,
  credential: (String, String),
}

/// Format: the youngest revision of each svn sweep's states: `before` commits revision 1 and `after` revision 2.
const SVN_BEFORE: u64 = 1;
/// Format: see [`SVN_BEFORE`].
const SVN_AFTER: u64 = 2;

impl Svn {
  /// Stock `svn`, or `None` without it, which the caller skips loudly.
  fn stock(scratch: &Scratch) -> Option<Svn> {
    let version = Command::new("svn")
      .args(["--version", "--quiet"])
      .output()
      .ok()?;
    if !version.status.success() {
      return None;
    }
    eprintln!(
      "stock svn: {}",
      String::from_utf8_lossy(&version.stdout).trim()
    );
    Some(Svn {
      options: vec![
        "--config-dir".to_owned(),
        format!("{}/svn-config", scratch.path),
        "--non-interactive".to_owned(),
        "--no-auth-cache".to_owned(),
      ],
      credential: (String::new(), String::new()),
    })
  }

  /// Runs `program` (`svn` or `svnrdump`) with the options, the credential and `args`.
  fn spawn_tool(&self, program: &str, dir: &str, args: &[&str]) -> Running {
    let mut child = Command::new(program)
      .args(&self.options)
      .args([
        "--username",
        &self.credential.0,
        "--password",
        &self.credential.1,
      ])
      .args(args)
      .current_dir(dir)
      .env("LC_ALL", "C")
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let out = read_on_a_thread(child.stdout.take().unwrap());
    let err = read_on_a_thread(child.stderr.take().unwrap());
    Running {
      child,
      out,
      err,
      started: Instant::now(),
    }
  }

  fn spawn(&self, dir: &str, args: &[&str]) -> Running {
    self.spawn_tool("svn", dir, args)
  }

  fn run(&self, dir: &str, args: &[&str]) -> Ended {
    self.spawn(dir, args).end()
  }

  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let ended = self.run(dir, args);
    assert_eq!(ended.code, Some(0), "svn {args:?} in {dir}: {ended:?}");
    ended.out
  }

  /// The youngest revision at `url`; `Err` with what svn said when it could not answer.
  fn youngest(&self, dir: &str, url: &str) -> Result<u64, String> {
    let ended = self.run(dir, &["info", "--show-item", "revision", url]);
    if ended.code != Some(0) {
      return Err(format!("{ended:?}"));
    }
    ended.out.trim().parse().map_err(|_| format!("{ended:?}"))
  }

  /// How many files `url/payload` lists, `None` where it lists nothing or cannot be read.
  fn payload_files(&self, dir: &str, url: &str) -> Option<usize> {
    let ended = self.run(dir, &["list", &format!("{url}/payload")]);
    (ended.code == Some(0)).then(|| ended.out.lines().filter(|line| !line.is_empty()).count())
  }

  /// `Ok` when the history at `url`, dumped by stock `svnrdump` and loaded into a fresh FSFS repository by stock
  /// `svnadmin`, passes `svnadmin verify`; `Ok` too, said loudly, where the host has no `svnrdump` or `svnadmin`.
  fn verified(&self, scratch: &Scratch, url: &str, name: &str) -> Result<(), String> {
    let tools = ["svnrdump", "svnadmin"].iter().all(|tool| {
      Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    });
    if !tools {
      eprintln!("unverified: no svnrdump or svnadmin here");
      return Ok(());
    }
    // The dump holds the payload's bytes as they are, so it is read as bytes, never as text.
    let dumped = Command::new("svnrdump")
      .args(&self.options)
      .args([
        "--username",
        &self.credential.0,
        "--password",
        &self.credential.1,
      ])
      .args(["dump", "-q", url])
      .current_dir(&scratch.path)
      .env("LC_ALL", "C")
      .stdin(Stdio::null())
      .output()
      .unwrap();
    if !dumped.status.success() {
      return Err(format!(
        "svnrdump: {:?}: {}",
        dumped.status.code(),
        last_words(&String::from_utf8_lossy(&dumped.stderr))
      ));
    }
    let local = format!("{}/{name}", scratch.path);
    let created = Command::new("svnadmin")
      .args(["create", &local])
      .output()
      .unwrap();
    if !created.status.success() {
      return Err(format!("svnadmin create: {created:?}"));
    }
    let mut load = Command::new("svnadmin")
      .args(["load", "-q", &local])
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    load
      .stdin
      .take()
      .unwrap()
      .write_all(&dumped.stdout)
      .unwrap();
    let loaded = load.wait_with_output().unwrap();
    if !loaded.status.success() {
      return Err(format!(
        "svnadmin load: {}",
        String::from_utf8_lossy(&loaded.stderr).trim()
      ));
    }
    let verify = Command::new("svnadmin")
      .args(["verify", "-q", &local])
      .output()
      .unwrap();
    if !verify.status.success() {
      return Err(format!(
        "svnadmin verify: {}",
        String::from_utf8_lossy(&verify.stderr).trim()
      ));
    }
    Ok(())
  }
}

/// The anchor's svn:// listener, and the svn client carrying a token for it.
fn svn_served(svn: &mut Svn, sweep: &str) -> Served {
  let served = serve(
    &format!("crash-svn-{sweep}-{}", std::process::id()),
    Edge::Svn,
  );
  svn.credential = served.token();
  served
}

/// A working copy of repository `url` in `dir` at revision [`SVN_BEFORE`]: one small file committed. The payload is
/// written and added there, uncommitted.
fn svn_before(svn: &Svn, scratch: &Scratch, url: &str, dir: &str) {
  svn.ok(&scratch.path, &["checkout", "-q", url, dir]);
  write_file(&format!("{dir}/first"), b"before\n");
  svn.ok(dir, &["add", "-q", "first"]);
  svn.ok(dir, &["commit", "-q", "-m", "before"]);
  write_payload(dir);
  svn.ok(dir, &["add", "-q", "payload"]);
}

/// UI goal 93 correctness, crash consistency of an svn commit (§4.17.4; §4.8 "persistence before reply"). Do: an
/// anchor with its svn:// listener; a repository per point at revision 1 (one small file) with the payload added in a
/// working copy; its commit, with the daemon killed `k·T/(N+1)` after it starts. Expect, after the restart: the
/// youngest revision is 1 or 2; the history, dumped by `svnrdump` and loaded into a fresh repository, passes
/// `svnadmin verify`; a commit the client saw succeed is revision 2; the client ended within [`PATIENCE`]; and the
/// commit retried from a fresh checkout (nothing to do where revision 2 landed) leaves revision 2 with every payload
/// file.
#[test]
#[ignore = "a crash sweep: cargo test --release -p slates-cli --test vcs_crash -- --ignored --nocapture"]
fn an_svn_commit_killed_at_any_point_leaves_the_repository_before_or_after() {
  let scratch = Scratch::new("vcs-crash-svn-commit");
  let Some(mut svn) = Svn::stock(&scratch) else {
    eprintln!("skipping the svn commit sweep: no stock svn here (svn on the PATH)");
    return;
  };
  let served = svn_served(&mut svn, "commit");
  let reference = format!("{REPOSITORY_PREFIX}-svn-reference");
  served.create(&reference, "svn");
  let reference_dir = format!("{}/reference", scratch.path);
  svn_before(&svn, &scratch, &served.svn_url(&reference), &reference_dir);
  let measured = svn.run(&reference_dir, &["commit", "-q", "-m", "after"]);
  assert_eq!(measured.code, Some(0), "the reference commit: {measured:?}");
  let span = measured.took;
  let count = points();
  eprintln!("REFERENCE svn commit took {span:?}; {count} points");
  let mut faults = Faults::new("svn commit");
  for point in 1..=count {
    let name = format!("{REPOSITORY_PREFIX}-svn-commit-{point}");
    served.create(&name, "svn");
    let url = served.svn_url(&name);
    let dir = format!("{}/commit-{point}", scratch.path);
    svn_before(&svn, &scratch, &url, &dir);
    let delay = delay_of(span, point, count);
    let pid = daemon_pid(&served.instance).expect("the daemon's pid");
    let commit = svn.spawn(&dir, &["commit", "-q", "-m", "after"]);
    let (killed, successor) = served.crash_at(commit.started, delay, pid);
    let ended = commit.end();
    let youngest = svn.youngest(&scratch.path, &url);
    let verified = svn.verified(&scratch, &url, &format!("{name}-after-crash"));
    let again = format!("{}/commit-{point}-again", scratch.path);
    let checked_out = svn.run(&scratch.path, &["checkout", "-q", &url, &again]);
    let retry = if youngest == Ok(SVN_BEFORE) {
      write_payload(&again);
      let added = svn.run(&again, &["add", "-q", "payload"]);
      if added.code == Some(0) {
        svn.run(&again, &["commit", "-q", "-m", "after"])
      } else {
        added
      }
    } else {
      checked_out
    };
    let youngest_after_retry = svn.youngest(&scratch.path, &url);
    let files = svn.payload_files(&scratch.path, &url);
    let verified_after_retry = svn.verified(&scratch, &url, &format!("{name}-after-retry"));
    eprintln!(
      "POINT svn commit {point}/{count} kill at {delay:?} (pid {killed} -> {successor:?}): client {:?} in {:?} \
       ({:?}); youngest {youngest:?}; verify {}; retry {:?} -> {youngest_after_retry:?}, {files:?} files",
      ended.code,
      ended.took,
      last_words(&ended.err),
      verified.as_ref().map_or("FAILED", |()| "ok"),
      retry.code,
    );
    faults.add_each(
      point,
      delay,
      vec![
        (
          !matches!(youngest, Ok(SVN_BEFORE | SVN_AFTER)),
          format!("the youngest revision is neither 1 nor 2: {youngest:?}"),
        ),
        (
          verified.is_err(),
          format!(
            "the history failed svnadmin verify after the crash: {}",
            failure_of(&verified)
          ),
        ),
        (
          ended.code == Some(0) && youngest != Ok(SVN_AFTER),
          format!("the client saw its commit succeed, and the youngest is {youngest:?}"),
        ),
        (
          ended.code.is_none(),
          format!("the client hung past {PATIENCE:?}: {ended:?}"),
        ),
        (
          retry.code != Some(0),
          format!("the retried commit failed: {retry:?}"),
        ),
        (
          youngest_after_retry != Ok(SVN_AFTER) || files != Some(PAYLOAD_FILES),
          format!(
            "after the retry the youngest is {youngest_after_retry:?} with {files:?} payload files"
          ),
        ),
        (
          verified_after_retry.is_err(),
          format!(
            "the history failed svnadmin verify after the retry: {}",
            failure_of(&verified_after_retry)
          ),
        ),
      ],
    );
  }
  faults.assert_none();
}

/// UI goal 93 correctness, crash consistency of an svn checkout (§4.17.4). Do: a repository at revision 2 (the
/// payload committed); a reference checkout measured; then a checkout per point with the daemon killed `k·T/(N+1)`
/// after it starts. Expect, after the restart: the youngest revision is still 2 and the history passes `svnadmin
/// verify`; the client ended within [`PATIENCE`]; and a second checkout succeeds at revision 2 with every payload file.
#[test]
#[ignore = "a crash sweep: cargo test --release -p slates-cli --test vcs_crash -- --ignored --nocapture"]
fn an_svn_checkout_killed_at_any_point_leaves_the_repository_whole_and_a_retry_checks_out() {
  let scratch = Scratch::new("vcs-crash-svn-checkout");
  let Some(mut svn) = Svn::stock(&scratch) else {
    eprintln!("skipping the svn checkout sweep: no stock svn here (svn on the PATH)");
    return;
  };
  let served = svn_served(&mut svn, "checkout");
  let name = format!("{REPOSITORY_PREFIX}-svn-checkout");
  served.create(&name, "svn");
  let url = served.svn_url(&name);
  let source = format!("{}/source", scratch.path);
  svn_before(&svn, &scratch, &url, &source);
  svn.ok(&source, &["commit", "-q", "-m", "after"]);
  let measured = svn.run(
    &scratch.path,
    &[
      "checkout",
      "-q",
      &url,
      &format!("{}/reference", scratch.path),
    ],
  );
  assert_eq!(
    measured.code,
    Some(0),
    "the reference checkout: {measured:?}"
  );
  let span = measured.took;
  let count = points();
  eprintln!("REFERENCE svn checkout took {span:?}; {count} points");
  let mut faults = Faults::new("svn checkout");
  for point in 1..=count {
    let delay = delay_of(span, point, count);
    let pid = daemon_pid(&served.instance).expect("the daemon's pid");
    let into = format!("{}/checkout-{point}", scratch.path);
    let checkout = svn.spawn(&scratch.path, &["checkout", "-q", &url, &into]);
    let (killed, successor) = served.crash_at(checkout.started, delay, pid);
    let ended = checkout.end();
    let youngest = svn.youngest(&scratch.path, &url);
    let verified = svn.verified(&scratch, &url, &format!("checkout-{point}-after-crash"));
    let again = format!("{}/checkout-{point}-again", scratch.path);
    let retry = svn.run(&scratch.path, &["checkout", "-q", &url, &again]);
    let files = svn.payload_files(&scratch.path, &url);
    eprintln!(
      "POINT svn checkout {point}/{count} kill at {delay:?} (pid {killed} -> {successor:?}): client {:?} in {:?} \
       ({:?}); youngest {youngest:?}; verify {}; retry {:?}, {files:?} files",
      ended.code,
      ended.took,
      last_words(&ended.err),
      verified.as_ref().map_or("FAILED", |()| "ok"),
      retry.code,
    );
    if youngest != Ok(SVN_AFTER) {
      faults.add(
        point,
        delay,
        format!("the youngest revision moved: {youngest:?}"),
      );
    }
    if let Err(why) = &verified {
      faults.add(
        point,
        delay,
        format!("the history failed svnadmin verify after the crash: {why}"),
      );
    }
    if ended.code.is_none() {
      faults.add(
        point,
        delay,
        format!("the client hung past {PATIENCE:?}: {ended:?}"),
      );
    }
    if retry.code != Some(0) || files != Some(PAYLOAD_FILES) {
      faults.add(
        point,
        delay,
        format!("the retried checkout failed: {retry:?}, {files:?} payload files"),
      );
    }
  }
  faults.assert_none();
}
