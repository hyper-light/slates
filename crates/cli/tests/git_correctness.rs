//! Correctness by use for slates' git server (goal 93's CORRECT step; main's piece for w-sessionlog, 2026-11-21): stock
//! git does the same thing to two servers, stock git's own (`git http-backend` over HTTP, hosted by the test's minimal CGI
//! host on loopback) and slates' (its HTTP edge, `--vcs-http`, and its SSH edge, `--vcs-ssh`), and every result a user can see is compared: each command's success, every ref and the
//! object it names (`git ls-remote`), every object a copy holds (`git cat-file --batch-all-objects --batch-check`: git
//! names objects by their bytes, so equal sets are byte-identical content) and `git fsck --strict` on each copy.
//!
//! **Each step a row.** A run does every step whatever an earlier one did, records each as `PASS` or `FAIL` with what
//! differed, prints the table, and fails at the end naming every failing row, so one run reports every divergence (a
//! round's output is the list of reds to send their owner).
//!
//! **Ignored, and main's to run (CLAUDE.md §8).** Both tests are interop rounds: main runs them, on a release build,
//! against the datasets the run request names, and sends each failing row to w-git as a red test. The tag-kind test is
//! small and runs in seconds; it is red on main 4b6df30dc (`! [remote rejected] blobtag -> blobtag (ref names no tree)`,
//! `crates/git/src/receive/works.rs`'s `NO_TREE`; inbox/w-git-from-w-sessionlog-tag-kinds.md). The dataset test takes a
//! real repository (`SLATES_TEST_GIT_DATASET`, a bare git/git mirror or a cut of the Linux tree) and skips loudly
//! without one.
//!
//! Stock git is `git` on the PATH or the words `SLATES_TEST_GIT` names; a host without it skips loudly, as one without
//! OpenSSH skips the SSH rows and one without `git-lfs` the LFS row. git reads no system's or user's configuration, its
//! home is the test's scratch directory, it never prompts, and its messages are C's. Everything lives in a scratch
//! directory in the build output (`CARGO_TARGET_TMPDIR`), removed at the test's end.
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
const STOCK_GIT: &str = "SLATES_TEST_GIT";

/// Shape: the variable naming the dataset the dataset test pushes: a bare repository on this host (a mirror of git/git,
/// say, or a cut of the Linux tree).
const DATASET: &str = "SLATES_TEST_GIT_DATASET";

/// Shape: the variable bounding one stock git command, in seconds (a dataset's whole push takes minutes); unset,
/// [`PATIENCE`].
const STEP_SECONDS: &str = "SLATES_TEST_GIT_STEP_SECONDS";

/// Format: the start of `git --version`'s answer.
const GIT_VERSION: &str = "git version ";

/// Format: the user name the routes give; slates takes the principal from the credential, never from this name.
const USER: &str = "git";

/// Format: the repository each test makes on slates, which each route's URL names.
const REPOSITORY: &str = "project";

/// Format: the branch the tag-kind test's source commits on.
const BRANCH: &str = "main";

/// Shape: the shards of each test's daemon, as `tests/cli.rs` starts one.
const SHARDS: &str = "2";

/// Shape: consecutive answered verbs before a test's daemon counts as settled, as `tests/git_interop.rs` waits.
const STABLE_STREAK: u32 = 10;

/// Shape: the default bound on one stock git command: a guard against a server that never answers, far above what a
/// small repository's command takes, never a measure of one.
const PATIENCE: Duration = Duration::from_secs(600);

/// Shape: the pause between polls of a settling daemon and of a running git.
const POLL: Duration = Duration::from_millis(20);

/// Shape: the commits the dataset test's deepen asks for past a depth-1 shallow clone.
const DEEPEN: &str = "50";

/// Format: the start of the line a daemon prints once it serves; its instance follows.
const SERVING_LINE: &str = "slates daemon: instance ";

/// Format: the anchor's line when its supervision finds its daemon in a crash loop.
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";

/// Format: the start of the anchor's line naming its issuer surface, `NAME=VALUE` pairs separated by spaces.
const ISSUER_SURFACE_LINE: &str = "slates anchor: issuer surface: export ";

/// Format: the start of the daemon's line naming where its HTTP edge serves cleartext; the port follows.
const VCS_HTTP_LINE: &str =
  "slates daemon: the version-control HTTP edge serves cleartext on 127.0.0.1:";

/// Format: the start of the daemon's line naming where its SSH edge serves; the port follows, then
/// [`VCS_SSH_KEY_WORDS`] and the host key's SHA256 fingerprint.
const VCS_SSH_LINE: &str = "slates daemon: the version-control SSH edge serves on 127.0.0.1:";

/// Format: the words between the SSH edge's port and its host key's fingerprint.
const VCS_SSH_KEY_WORDS: &str = " with the ED25519 host key ";

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
    let path = std::fs::canonicalize(&raw)
      .unwrap()
      .to_string_lossy()
      .replace('\\', "/");
    Scratch {
      path: path.strip_prefix("//?/").map(str::to_owned).unwrap_or(path),
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

/// Writes `bytes` to `path`, a file of the test's scratch directory.
#[allow(clippy::disallowed_methods)]
fn written(path: &str, bytes: &[u8]) {
  std::fs::write(path, bytes).unwrap();
}

/// Stock git's command words, or `None` where this host runs none, which the caller skips loudly.
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

/// Whether this host runs OpenSSH's `ssh` and `ssh-keygen`.
fn openssh() -> bool {
  ["ssh", "ssh-keygen"].iter().all(|tool| {
    Command::new(tool)
      .arg("-V")
      .output()
      .is_ok_and(|output| output.status.success() || !output.stderr.is_empty())
  })
}

/// The bound on one stock git command ([`STEP_SECONDS`], else [`PATIENCE`]).
fn step_bound() -> Duration {
  std::env::var(STEP_SECONDS)
    .ok()
    .and_then(|seconds| seconds.parse().ok())
    .map_or(PATIENCE, Duration::from_secs)
}

fn pause() {
  // The harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

/// Reads `from` to its end on a thread of its own.
fn read_on_a_thread(mut from: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
  std::thread::spawn(move || {
    let mut bytes = Vec::new();
    let _ = from.read_to_end(&mut bytes);
    bytes
  })
}

/// What one command did: its exit code (-1 for a signal, -2 for killed past its bound) and both outputs.
struct Ran {
  code: i32,
  out: Vec<u8>,
  err: String,
}

impl Ran {
  fn ok(&self) -> bool {
    self.code == 0
  }

  fn text(&self) -> String {
    String::from_utf8_lossy(&self.out).into_owned()
  }

  /// The last lines of its error output, for a row's reason.
  fn said(&self) -> String {
    let lines: Vec<&str> = self.err.lines().collect();
    let tail = lines.len().saturating_sub(4);
    format!("exit {}: {}", self.code, lines[tail..].join(" | "))
  }
}

/// Stock git for one test: its home and configuration in the scratch directory; route words (a credential, an SSH
/// command) given per command.
struct Git {
  program: Vec<String>,
  home: String,
}

impl Git {
  fn new(program: &[String], scratch: &Scratch) -> Git {
    let home = format!("{}/home", scratch.path);
    made_directory(&home);
    Git {
      program: program.to_vec(),
      home,
    }
  }

  /// Runs git with the route's `config` words and `args` in `dir`, killed past [`step_bound`].
  fn run(&self, dir: &str, config: &[String], args: &[&str]) -> Ran {
    let (program, leading) = self.program.split_first().expect("a command");
    let mut child = Command::new(program)
      .args(leading)
      .args(config)
      .args(args)
      .current_dir(dir)
      .envs([
        ("HOME", self.home.as_str()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        // A runner's own config injected through `GIT_CONFIG_COUNT` (kiln's signs every commit and tag, so a
        // lightweight tag asked for a message) is cleared, as `git_interop.rs` clears it.
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ("GIT_AUTHOR_DATE", "2026-11-21T00:00:00Z"),
        ("GIT_COMMITTER_DATE", "2026-11-21T00:00:00Z"),
      ])
      .env_remove("GIT_ASKPASS")
      .env_remove("SSH_ASKPASS")
      .env_remove("SSH_AUTH_SOCK")
      .env_remove("GIT_SSH")
      .env_remove("GIT_SSH_COMMAND")
      .env_remove("GIT_CONFIG_PARAMETERS")
      .env_remove("GIT_DIR")
      .env_remove("GIT_WORK_TREE")
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let out = read_on_a_thread(child.stdout.take().unwrap());
    let err = read_on_a_thread(child.stderr.take().unwrap());
    let code = exit_within(&mut child, step_bound());
    Ran {
      code,
      out: out.join().unwrap(),
      err: String::from_utf8_lossy(&err.join().unwrap()).into_owned(),
    }
  }

  /// Runs git with no route words, failing the test unless it succeeds: its standard output.
  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let ran = self.run(dir, &[], args);
    assert!(ran.ok(), "git {args:?} in {dir}: {}", ran.said());
    ran.text()
  }
}

/// Waits for `child` within `bound`, killing it past that: its exit code, -1 for a signal, -2 when killed here.
fn exit_within(child: &mut Child, bound: Duration) -> i32 {
  let started = Instant::now();
  loop {
    if let Some(status) = child.try_wait().unwrap() {
      return status.code().unwrap_or(-1);
    }
    if started.elapsed() > bound {
      let _ = child.kill();
      let _ = child.wait();
      return -2;
    }
    pause();
  }
}

/// The anchor process, killed and reaped when dropped; what it says after its daemon served goes to standard error.
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

/// A slates server for one test: the anchor (HTTP and SSH edges on loopback), its store, and each route's URL and the
/// `-c` words a git command on it takes (a bearer token; an SSH command with a fresh enrolled key).
struct Slates {
  _anchor: Anchor,
  _store: Scratch,
  http_url: String,
  http_config: Vec<String>,
  ssh: Option<(String, Vec<String>)>,
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

/// Reads the anchor's lines until its daemon serves and the anchor names its issuer surface.
fn lines_until_served(
  stderr: &mut impl BufRead,
  instance: &str,
) -> (Vec<String>, Vec<(String, String)>) {
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (mut lines, mut issuer, mut served) = (Vec::new(), None, false);
  while !served || issuer.is_none() {
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
    issuer = issuer.or_else(|| {
      let pairs = line.trim().strip_prefix(ISSUER_SURFACE_LINE)?;
      Some(
        pairs
          .split(' ')
          .filter_map(|pair| pair.split_once('='))
          .map(|(name, value)| (name.to_owned(), value.to_owned()))
          .collect::<Vec<_>>(),
      )
    });
    lines.push(line);
  }
  (lines, issuer.unwrap_or_default())
}

/// The port right after `prefix` on the first of `lines` that begins with it.
fn port_after(lines: &[String], prefix: &str) -> Option<u16> {
  lines.iter().find_map(|line| {
    line
      .strip_prefix(prefix)?
      .split(|c: char| !c.is_ascii_digit())
      .next()?
      .parse()
      .ok()
  })
}

/// Starts slates with both edges, makes [`REPOSITORY`] with `default_branch`, mints a token and, where OpenSSH is here,
/// enrolls a fresh key and trusts the host key the daemon named.
fn start_slates(scratch: &Scratch, default_branch: &str) -> Slates {
  let instance = format!("git-correct-{}", std::process::id());
  let store = Scratch::new("vcs-store");
  let mut child = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args([
      "--instance",
      &instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .args(["--vcs-http", "127.0.0.1:0", "--vcs-ssh", "127.0.0.1:0"])
    .args(["--vcs-store", &store.path])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let mut anchor = Anchor { child, drain: None };
  let (lines, issuer) = lines_until_served(&mut stderr, &instance);
  anchor.drain = Some(std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  }));
  let mut judged = process_wait::Judged::of(anchor.child.id());
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if slates(&instance, &[], &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    judged.look("the daemon answered verbs in a row");
    pause();
  }
  let (code, out, err) = slates(&instance, &[], &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap root: {out}{err}");
  let (code, out, err) = slates(
    &instance,
    &[],
    &[
      "repository",
      "create",
      REPOSITORY,
      "--tool",
      "git",
      "--default-branch",
      default_branch,
    ],
  );
  assert_eq!(code, 0, "repository create: {out}{err}");
  let (code, out, err) = slates(&instance, &issuer, &["credential", "token"]);
  assert_eq!(code, 0, "credential token: {out}{err}");
  let token = value_of(&out, "token");
  let port = port_after(&lines, VCS_HTTP_LINE).expect("the daemon named its HTTP edge's port");
  let ssh = if openssh() {
    Some(ssh_route(scratch, &instance, &issuer, &lines))
  } else {
    eprintln!("skipping the SSH rows: no OpenSSH here");
    None
  };
  Slates {
    _anchor: anchor,
    _store: store,
    http_url: format!("http://127.0.0.1:{port}/{REPOSITORY}"),
    http_config: vec![
      "-c".to_owned(),
      format!("http.extraHeader=Authorization: Bearer {token}"),
    ],
    ssh,
  }
}

/// The SSH route: a fresh Ed25519 key enrolled for the repository's owner, the host key the daemon named trusted alone
/// (checked against `ssh-keyscan`), the URL and the `core.sshCommand` words.
fn ssh_route(
  scratch: &Scratch,
  instance: &str,
  issuer: &[(String, String)],
  lines: &[String],
) -> (String, Vec<String>) {
  let (port, fingerprint) = lines
    .iter()
    .find_map(|line| {
      let rest = line.strip_prefix(VCS_SSH_LINE)?;
      let (port, fingerprint) = rest.trim().split_once(VCS_SSH_KEY_WORDS)?;
      Some((port.parse::<u16>().ok()?, fingerprint.to_owned()))
    })
    .expect("the daemon named its SSH edge");
  let key = format!("{}/key", scratch.path);
  let made = Command::new("ssh-keygen")
    .args([
      "-q",
      "-t",
      "ed25519",
      "-N",
      "",
      "-C",
      "slates-git",
      "-f",
      &key,
    ])
    .output()
    .unwrap();
  assert!(made.status.success(), "ssh-keygen: {made:?}");
  let public = std::fs::read_to_string(format!("{key}.pub")).unwrap();
  let (code, out, err) = slates(instance, issuer, &["credential", "key", public.trim()]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let scanned = Command::new("ssh-keyscan")
    .args(["-p", &port.to_string(), "-t", "ed25519", "127.0.0.1"])
    .output()
    .unwrap();
  let known = format!("{}/known_hosts", scratch.path);
  written(&known, &scanned.stdout);
  let listed = Command::new("ssh-keygen")
    .args(["-l", "-E", "sha256", "-f", &known])
    .output()
    .unwrap();
  let listing = String::from_utf8_lossy(&listed.stdout).into_owned();
  assert_eq!(
    listing.split_whitespace().nth(1),
    Some(fingerprint.as_str()),
    "the SSH edge presents the key the daemon named"
  );
  let command = format!(
    "core.sshCommand=ssh -F /dev/null -i {key} -o IdentitiesOnly=yes -o BatchMode=yes \
     -o StrictHostKeyChecking=yes -o UserKnownHostsFile={known} -o GlobalKnownHostsFile=/dev/null"
  );
  (
    format!("ssh://{USER}@127.0.0.1:{port}/{REPOSITORY}"),
    vec!["-c".to_owned(), command],
  )
}

/// One server a step runs against: its name, its URL and the `-c` words its route takes.
struct Server {
  name: &'static str,
  url: String,
  config: Vec<String>,
}

/// The rows of one run: each step's name, and `None` for a pass or what differed.
#[derive(Default)]
struct Rows {
  rows: Vec<(String, Option<String>)>,
}

impl Rows {
  fn record(&mut self, name: &str, failure: Option<String>) {
    match &failure {
      None => eprintln!("PASS {name}"),
      Some(why) => eprintln!("FAIL {name}: {why}"),
    }
    self.rows.push((name.to_owned(), failure));
  }

  /// Prints the table and fails the test naming every failing row.
  fn finish(self) {
    let failed: Vec<String> = self
      .rows
      .iter()
      .filter_map(|(name, failure)| failure.as_ref().map(|why| format!("{name}: {why}")))
      .collect();
    eprintln!(
      "---- {} of {} rows passed",
      self.rows.len().saturating_sub(failed.len()),
      self.rows.len()
    );
    assert!(failed.is_empty(), "failing rows:\n{}", failed.join("\n"));
  }
}

/// Every ref `git ls-remote` lists at `server`, sorted, or why it could not list them.
fn refs_at(git: &Git, dir: &str, server: &Server) -> Result<String, String> {
  let ran = git.run(dir, &server.config, &["ls-remote", &server.url]);
  if !ran.ok() {
    return Err(ran.said());
  }
  let mut lines: Vec<String> = ran.text().lines().map(str::to_owned).collect();
  lines.sort();
  Ok(lines.join("\n"))
}

/// Every object the repository at `dir` holds, `ID KIND SIZE`, sorted: equal sets are byte-identical content.
fn objects_in(git: &Git, dir: &str) -> String {
  let out = git.ok(
    dir,
    &[
      "cat-file",
      "--batch-all-objects",
      "--batch-check",
      "--unordered",
    ],
  );
  let mut lines: Vec<&str> = out.lines().collect();
  lines.sort_unstable();
  lines.join("\n")
}

/// The first line where `stock` and `slates` differ, for a row's reason.
fn first_difference(what: &str, stock: &str, slates: &str) -> String {
  let mut left = stock.lines();
  let mut right = slates.lines();
  loop {
    match (left.next(), right.next()) {
      (Some(a), Some(b)) if a == b => continue,
      (a, b) => {
        return format!(
          "{what} differ: stock {:?} slates {:?} ({} and {} lines)",
          a,
          b,
          stock.lines().count(),
          slates.lines().count()
        );
      }
    }
  }
}

/// A row comparing the refs both servers list now.
fn same_refs(rows: &mut Rows, git: &Git, dir: &str, row: &str, stock: &Server, other: &Server) {
  let failure = match (refs_at(git, dir, stock), refs_at(git, dir, other)) {
    (Ok(a), Ok(b)) if a == b => None,
    (Ok(a), Ok(b)) => Some(first_difference("refs", &a, &b)),
    (Err(why), _) => Some(format!("stock ls-remote: {why}")),
    (_, Err(why)) => Some(format!("{} ls-remote: {why}", other.name)),
  };
  rows.record(row, failure);
}

/// Runs `args` (a push, a fetch) against each server in turn, from copies `stock_dir` and `slates_dir`, the server's URL
/// placed where `{url}` stands: a row passing when both succeed or both fail.
fn same_outcome(rows: &mut Rows, git: &Git, row: &str, runs: [(&Server, &str); 2], args: &[&str]) {
  let outcomes: Vec<Ran> = runs
    .iter()
    .map(|(server, dir)| {
      let args: Vec<&str> = args
        .iter()
        .map(|arg| {
          if *arg == "{url}" {
            server.url.as_str()
          } else {
            arg
          }
        })
        .collect();
      git.run(dir, &server.config, &args)
    })
    .collect();
  let failure = match (outcomes[0].ok(), outcomes[1].ok()) {
    (true, true) | (false, false) => None,
    (true, false) => Some(format!(
      "stock took it, {} did not: {}",
      runs[1].0.name,
      outcomes[1].said()
    )),
    (false, true) => Some(format!(
      "stock refused it ({}), {} took it",
      outcomes[0].said(),
      runs[1].0.name
    )),
  };
  rows.record(row, failure);
}

/// Clones `server` into `dir` with `options`, then checks the copy: `fsck --strict` clean. The clone's success and fsck
/// as one row's reason (`None` for a clean copy).
fn cloned(
  git: &Git,
  scratch: &Scratch,
  server: &Server,
  dir: &str,
  options: &[&str],
) -> Option<String> {
  let mut args = vec!["clone", "-q"];
  args.extend_from_slice(options);
  args.push(&server.url);
  args.push(dir);
  let ran = git.run(&scratch.path, &server.config, &args);
  if !ran.ok() {
    return Some(format!("clone: {}", ran.said()));
  }
  let fsck = git.run(dir, &[], &["fsck", "--strict", "--no-progress"]);
  (!fsck.ok()).then(|| format!("fsck: {}", fsck.said()))
}

/// Clones both servers with `options` into `NAME-stock` and `NAME-SERVER` and compares the copies: both clean and the
/// same objects (`compare_objects`) or the same checked-out index (a partial clone's objects are fetched lazily, so it
/// compares what a checkout holds).
fn same_clones(
  rows: &mut Rows,
  git: &Git,
  scratch: &Scratch,
  row: &str,
  pair: [&Server; 2],
  options: &[&str],
  compare_objects: bool,
) {
  let dirs: Vec<String> = pair
    .iter()
    .map(|server| format!("{}/{row}-{}", scratch.path, server.name))
    .collect();
  let failures: Vec<Option<String>> = pair
    .iter()
    .zip(&dirs)
    .map(|(server, dir)| cloned(git, scratch, server, dir, options))
    .collect();
  let failure = if let Some(why) = &failures[0] {
    Some(format!(
      "the stock side failed, the row proves nothing: {why}"
    ))
  } else if let Some(why) = &failures[1] {
    Some(why.clone())
  } else if compare_objects {
    let (a, b) = (objects_in(git, &dirs[0]), objects_in(git, &dirs[1]));
    (a != b).then(|| first_difference("objects", &a, &b))
  } else {
    let (a, b) = (
      git.ok(&dirs[0], &["ls-files", "-s"]),
      git.ok(&dirs[1], &["ls-files", "-s"]),
    );
    (a != b).then(|| first_difference("checked-out index entries", &a, &b))
  };
  rows.record(row, failure);
}

/// Stock git's own HTTP server for one test: `git http-backend`, the CGI program stock git ships for smart HTTP, run by
/// a minimal CGI host on a loopback port (`serve_cgi`), one connection a thread, each request one run of the program.
/// Dropped, it stops taking connections.
struct HttpBackend {
  port: u16,
  stop: &'static std::sync::atomic::AtomicBool,
}

impl Drop for HttpBackend {
  fn drop(&mut self) {
    self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    // Wakes the accept loop so it sees the stop.
    let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
  }
}

/// One request the CGI host read: its method, path, query and the headers it passes on, and its body with any chunked
/// framing removed.
struct CgiRequest {
  method: String,
  path: String,
  query: String,
  headers: Vec<(String, String)>,
  body: Vec<u8>,
}

impl CgiRequest {
  fn header(&self, name: &str) -> Option<&str> {
    self
      .headers
      .iter()
      .find(|(key, _)| key.eq_ignore_ascii_case(name))
      .map(|(_, value)| value.as_str())
  }
}

/// Reads a chunked body from `reader` to its last chunk: the chunks' bytes in order.
fn read_chunked(reader: &mut impl BufRead) -> Option<Vec<u8>> {
  let mut body = Vec::new();
  loop {
    let mut size = String::new();
    reader.read_line(&mut size).ok()?;
    let size = usize::from_str_radix(size.trim().split(';').next()?, 16).ok()?;
    if size == 0 {
      let mut end = String::new();
      reader.read_line(&mut end).ok()?;
      return Some(body);
    }
    let mut chunk = vec![0; size];
    reader.read_exact(&mut chunk).ok()?;
    body.extend_from_slice(&chunk);
    let mut end = [0; 2];
    reader.read_exact(&mut end).ok()?;
  }
}

/// Reads one HTTP/1.1 request from `reader`: its line, its headers, and its body by `Content-Length` or chunked.
fn read_request(reader: &mut impl BufRead) -> Option<CgiRequest> {
  let mut line = String::new();
  reader.read_line(&mut line).ok()?;
  let mut words = line.split_whitespace();
  let (method, target) = (words.next()?.to_owned(), words.next()?.to_owned());
  let (path, query) = target
    .split_once('?')
    .map_or((target.clone(), String::new()), |(path, query)| {
      (path.to_owned(), query.to_owned())
    });
  let mut headers = Vec::new();
  loop {
    let mut header = String::new();
    reader.read_line(&mut header).ok()?;
    let header = header.trim_end();
    if header.is_empty() {
      break;
    }
    let (name, value) = header.split_once(':')?;
    headers.push((name.trim().to_owned(), value.trim().to_owned()));
  }
  let mut request = CgiRequest {
    method,
    path,
    query,
    headers,
    body: Vec::new(),
  };
  let chunked = request
    .header("Transfer-Encoding")
    .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
  if chunked {
    request.body = read_chunked(reader)?;
  } else if let Some(length) = request.header("Content-Length") {
    let mut body = vec![0; length.parse().ok()?];
    reader.read_exact(&mut body).ok()?;
    request.body = body;
  }
  Some(request)
}

/// Serves one connection: reads its request, runs `git http-backend` on it as a CGI host does (RFC 3875's variables;
/// `REMOTE_USER` set, so receive-pack is enabled as an authenticated user's would be), and answers what the program
/// printed, its `Status` header the status line, then closes.
fn serve_cgi(stream: std::net::TcpStream, program: &[String], root: &str, home: &str) {
  let Ok(read_half) = stream.try_clone() else {
    return;
  };
  let mut reader = std::io::BufReader::new(read_half);
  let Some(request) = read_request(&mut reader) else {
    return;
  };
  let (git, leading) = program.split_first().expect("a command");
  let mut command = Command::new(git);
  command
    .args(leading)
    .arg("http-backend")
    .env("GIT_PROJECT_ROOT", root)
    .env("GIT_HTTP_EXPORT_ALL", "1")
    .env("GATEWAY_INTERFACE", "CGI/1.1")
    .env("REQUEST_METHOD", &request.method)
    .env("PATH_INFO", &request.path)
    .env("QUERY_STRING", &request.query)
    .env("CONTENT_LENGTH", request.body.len().to_string())
    .env("REMOTE_USER", USER)
    .env("REMOTE_ADDR", "127.0.0.1")
    .env("HOME", home)
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_COUNT", "0")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
  for (variable, header) in [
    ("CONTENT_TYPE", "Content-Type"),
    ("GIT_PROTOCOL", "Git-Protocol"),
    ("HTTP_CONTENT_ENCODING", "Content-Encoding"),
  ] {
    if let Some(value) = request.header(header) {
      command.env(variable, value);
    }
  }
  let Ok(mut child) = command.spawn() else {
    return;
  };
  let mut stdin = child.stdin.take().unwrap();
  let body = request.body;
  let feeder = std::thread::spawn(move || {
    let _ = stdin.write_all(&body);
  });
  let printed = read_on_a_thread(child.stdout.take().unwrap())
    .join()
    .unwrap();
  let _ = feeder.join();
  let _ = child.wait();
  let split = printed
    .windows(4)
    .position(|window| window == b"\r\n\r\n")
    .map(|at| (at, 4))
    .or_else(|| {
      printed
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|at| (at, 2))
    });
  let (head, content) = match split {
    Some((at, gap)) => (&printed[..at], &printed[at + gap..]),
    None => (&printed[..0], &printed[..]),
  };
  let mut status = "200 OK".to_owned();
  let mut response = Vec::new();
  for line in String::from_utf8_lossy(head).lines() {
    match line.split_once(':') {
      Some((name, value)) if name.eq_ignore_ascii_case("Status") => {
        status = value.trim().to_owned()
      }
      Some(_) => response.extend_from_slice(format!("{}\r\n", line.trim_end()).as_bytes()),
      None => {}
    }
  }
  let mut out = stream;
  let _ = out.write_all(format!("HTTP/1.1 {status}\r\n").as_bytes());
  let _ = out.write_all(&response);
  let _ = out.write_all(
    format!(
      "Content-Length: {}\r\nConnection: close\r\n\r\n",
      content.len()
    )
    .as_bytes(),
  );
  let _ = out.write_all(content);
}

/// A bare repository stock git serves over HTTP through `git http-backend` ([`HttpBackend`]), made empty: the server
/// and the host, which the caller keeps for the test.
fn stock_server(git: &Git, scratch: &Scratch, default_branch: &str) -> (Server, HttpBackend) {
  let bare = format!("{}/stock.git", scratch.path);
  git.ok(
    &scratch.path,
    &["init", "-q", "--bare", "-b", default_branch, &bare],
  );
  let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
  let port = listener.local_addr().unwrap().port();
  let stop: &'static std::sync::atomic::AtomicBool =
    Box::leak(Box::new(std::sync::atomic::AtomicBool::new(false)));
  let (program, root, home) = (git.program.clone(), scratch.path.clone(), git.home.clone());
  std::thread::spawn(move || {
    for stream in listener.incoming() {
      if stop.load(std::sync::atomic::Ordering::SeqCst) {
        break;
      }
      let Ok(stream) = stream else {
        continue;
      };
      let (program, root, home) = (program.clone(), root.clone(), home.clone());
      std::thread::spawn(move || serve_cgi(stream, &program, &root, &home));
    }
  });
  (
    Server {
      name: "stock",
      url: format!("http://127.0.0.1:{port}/stock.git"),
      config: Vec::new(),
    },
    HttpBackend { port, stop },
  )
}

/// The servers a run compares stock against: slates' HTTP route, and its SSH route where OpenSSH is here.
fn slates_servers(slates: &Slates) -> Vec<Server> {
  let mut servers = vec![Server {
    name: "slates-http",
    url: slates.http_url.clone(),
    config: slates.http_config.clone(),
  }];
  if let Some((url, config)) = &slates.ssh {
    servers.push(Server {
      name: "slates-ssh",
      url: url.clone(),
      config: config.clone(),
    });
  }
  servers
}

/// A small source repository holding every kind of ref a stock repository may hold: a branch, a lightweight tag, an
/// annotated tag of a commit, of a blob (as git/git's `junio-gpg-pub`), of a tree, and of an annotated tag (as git/git's
/// `v1.0rc1`..`v1.0rc6`).
fn tag_kind_source(git: &Git, scratch: &Scratch) -> String {
  let source = format!("{}/source", scratch.path);
  git.ok(&scratch.path, &["init", "-q", "-b", BRANCH, &source]);
  git.ok(&source, &["commit", "-q", "--allow-empty", "-m", "root"]);
  written(&format!("{source}/a.txt"), b"one\n");
  git.ok(&source, &["add", "a.txt"]);
  git.ok(&source, &["commit", "-q", "-m", "one"]);
  written(&format!("{source}/a.txt"), b"one\ntwo\n");
  git.ok(&source, &["commit", "-q", "-a", "-m", "two"]);
  let blob = git.ok(&source, &["rev-parse", "HEAD:a.txt"]);
  let tree = git.ok(&source, &["rev-parse", "HEAD^{tree}"]);
  git.ok(&source, &["tag", "lightweight"]);
  git.ok(
    &source,
    &["tag", "-a", "annotated", "-m", "a commit", "HEAD~1"],
  );
  git.ok(
    &source,
    &["tag", "-a", "of-a-blob", "-m", "a blob", blob.trim()],
  );
  git.ok(
    &source,
    &["tag", "-a", "of-a-tree", "-m", "a tree", tree.trim()],
  );
  git.ok(
    &source,
    &[
      "-c",
      "advice.nestedTag=false",
      "tag",
      "-a",
      "of-a-tag",
      "-m",
      "a tag",
      "annotated",
    ],
  );
  source
}

/// The refspecs a whole push sends: every branch and every tag.
const EVERYTHING: [&str; 2] = ["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"];

/// Goal 93's CORRECT step, the tag kinds (r16's refused refs, reduced). Do: make a source holding every kind of ref
/// ([`tag_kind_source`]); push every branch and tag to stock git's own server and to slates over HTTP and SSH; list both
/// servers' refs; clone each over each route and check the copies. Expect each push taken whole by both, the same refs
/// at both, and each clone `fsck --strict` clean with the same objects as stock's. Red on main 4b6df30dc: slates
/// rejects `of-a-blob` and `of-a-tag` (`ref names no tree`). Main runs it (CLAUDE.md §8); skips loudly without stock
/// git.
#[test]
#[ignore = "an interop round, main's to run (CLAUDE.md §8); red until w-git takes a tag of a blob and a tag of a tag \
            (inbox/w-git-from-w-sessionlog-tag-kinds.md)"]
fn every_kind_of_tag_is_pushed_cloned_and_served_as_stock_git_serves_it() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping the tag kinds: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let scratch = Scratch::new("git-tag-kinds");
  let git = Git::new(&program, &scratch);
  let source = tag_kind_source(&git, &scratch);
  let (stock, _backend) = stock_server(&git, &scratch, BRANCH);
  let slates = start_slates(&scratch, BRANCH);
  let mut rows = Rows::default();
  for server in slates_servers(&slates) {
    let mut push = vec!["push", "{url}"];
    push.extend_from_slice(&EVERYTHING);
    same_outcome(
      &mut rows,
      &git,
      &format!("push-every-kind-{}", server.name),
      [(&stock, source.as_str()), (&server, source.as_str())],
      &push,
    );
    same_refs(
      &mut rows,
      &git,
      &source,
      &format!("refs-{}", server.name),
      &stock,
      &server,
    );
    same_clones(
      &mut rows,
      &git,
      &scratch,
      &format!("clone-{}", server.name),
      [&stock, &server],
      &[],
      true,
    );
  }
  rows.finish();
}

/// Goal 93's CORRECT step on a real repository (main's piece: git/git whole, a shallow and a partial clone of the Linux
/// tree). Do: push every branch and tag of the dataset ([`DATASET`]) to stock git's own server and to slates over HTTP
/// (and SSH, where OpenSSH is here), then run each client verb against both and compare: `ls-remote`; a whole clone
/// (objects and `fsck --strict`); an SSH clone; a fetch and a pull of a new commit pushed from one copy; a branch
/// pushed, force-pushed (non-fast-forward) and deleted; a lightweight tag, an annotated tag, a tag of a blob and a tag
/// of a tag pushed and deleted; a depth-1 shallow clone and its deepen by [`DEEPEN`]; a blob-less partial clone and
/// its checkout; and, where `git-lfs` is here, an LFS file pushed and cloned back. Expect every row as stock's: the same
/// outcome, the same refs, the same objects, every copy clean. Main runs it on a release build (the run request names
/// the datasets); skips loudly without stock git or a dataset.
#[test]
#[ignore = "an interop round on a real repository, main's to run (CLAUDE.md §8): set SLATES_TEST_GIT_DATASET"]
fn stock_git_verbs_on_a_real_repository_match_stock_git() {
  let Some(program) = stock_git() else {
    eprintln!(
      "skipping the dataset round: no stock git here (set {STOCK_GIT} to the command that runs it)"
    );
    return;
  };
  let Ok(dataset) = std::env::var(DATASET) else {
    eprintln!(
      "skipping the dataset round: no dataset (set {DATASET} to a bare repository, a git/git mirror say)"
    );
    return;
  };
  let scratch = Scratch::new("git-dataset");
  let git = Git::new(&program, &scratch);
  let head = git.ok(&dataset, &["symbolic-ref", "--short", "HEAD"]);
  let branch = head.trim().to_owned();
  let (stock, _backend) = stock_server(&git, &scratch, &branch);
  let slates = start_slates(&scratch, &branch);
  let servers = slates_servers(&slates);
  let mut rows = Rows::default();
  let http = &servers[0];

  // The whole push, every branch and tag, to each server.
  let mut push = vec!["push", "{url}"];
  push.extend_from_slice(&EVERYTHING);
  same_outcome(
    &mut rows,
    &git,
    "push-whole",
    [(&stock, dataset.as_str()), (http, dataset.as_str())],
    &push,
  );
  same_refs(&mut rows, &git, &dataset, "ls-remote", &stock, http);
  same_clones(
    &mut rows,
    &git,
    &scratch,
    "clone",
    [&stock, http],
    &[],
    true,
  );
  if let Some(ssh) = servers.get(1) {
    same_clones(
      &mut rows,
      &git,
      &scratch,
      "clone-ssh",
      [&stock, ssh],
      &[],
      true,
    );
  }

  // Working copies of each server, from the clones above.
  let stock_copy = format!("{}/clone-stock", scratch.path);
  let slates_copy = format!("{}/clone-slates-http", scratch.path);
  let copies = [(&stock, stock_copy.as_str()), (http, slates_copy.as_str())];
  let both_have_copies = std::path::Path::new(&slates_copy).join(".git").exists()
    && std::path::Path::new(&stock_copy).join(".git").exists();
  if both_have_copies {
    verbs_on_copies(&mut rows, &git, &scratch, copies, &branch);
  } else {
    rows.record(
      "verbs-on-copies",
      Some("skipped: a clone above failed, so there is no copy to push from".to_owned()),
    );
  }

  // Shallow and partial clones of the server as the whole push left it.
  same_clones(
    &mut rows,
    &git,
    &scratch,
    "shallow",
    [&stock, http],
    &["--depth", "1"],
    true,
  );
  if std::path::Path::new(&format!("{}/shallow-slates-http", scratch.path)).exists() {
    same_outcome(
      &mut rows,
      &git,
      "deepen",
      [
        (&stock, &format!("{}/shallow-stock", scratch.path)),
        (http, &format!("{}/shallow-slates-http", scratch.path)),
      ],
      &["fetch", "-q", "--deepen", DEEPEN],
    );
    let counts: Vec<String> = ["shallow-stock", "shallow-slates-http"]
      .iter()
      .map(|copy| {
        git
          .run(
            &format!("{}/{copy}", scratch.path),
            &[],
            &["rev-list", "--count", "HEAD"],
          )
          .text()
      })
      .collect();
    rows.record(
      "deepen-history",
      (counts[0] != counts[1]).then(|| {
        format!(
          "stock has {} commits, slates {}",
          counts[0].trim(),
          counts[1].trim()
        )
      }),
    );
  }
  same_clones(
    &mut rows,
    &git,
    &scratch,
    "partial",
    [&stock, http],
    &["--filter=blob:none"],
    false,
  );
  lfs_row(&mut rows, &git, &scratch, http);
  rows.finish();
}

/// The verbs a user runs from copies of each server: a fetch and a pull of a new commit, the push shapes, deletes and a
/// force push, each followed by a comparison of both servers' refs.
fn verbs_on_copies(
  rows: &mut Rows,
  git: &Git,
  scratch: &Scratch,
  copies: [(&Server, &str); 2],
  branch: &str,
) {
  let [(stock, stock_copy), (slates, slates_copy)] = copies;
  // A new commit on each copy's branch, the same bytes and dates on both, so the commits are the same object.
  for (_, copy) in copies {
    written(&format!("{copy}/correctness.txt"), b"by use\n");
    git.ok(copy, &["add", "correctness.txt"]);
    git.ok(copy, &["commit", "-q", "-m", "by use"]);
  }
  same_outcome(
    rows,
    git,
    "push-commit",
    copies,
    &["push", "-q", "{url}", "HEAD"],
  );
  same_refs(rows, git, stock_copy, "refs-after-commit", stock, slates);

  // A second copy of each server fetches and pulls it.
  let mut second = Vec::new();
  for (server, _) in copies {
    let dir = format!("{}/second-{}", scratch.path, server.name);
    let ran = git.run(
      &scratch.path,
      &server.config,
      &["clone", "-q", &server.url, &dir],
    );
    second.push((server, dir, ran.ok()));
  }
  if second.iter().all(|(_, _, ok)| *ok) {
    let seconds = [
      (second[0].0, second[0].1.as_str()),
      (second[1].0, second[1].1.as_str()),
    ];
    for (_, copy) in copies {
      written(&format!("{copy}/correctness.txt"), b"by use\nagain\n");
      git.ok(copy, &["commit", "-q", "-a", "-m", "again"]);
    }
    same_outcome(
      rows,
      git,
      "push-again",
      copies,
      &["push", "-q", "{url}", "HEAD"],
    );
    same_outcome(rows, git, "fetch", seconds, &["fetch", "-q", "{url}"]);
    same_outcome(
      rows,
      git,
      "pull",
      seconds,
      &["pull", "-q", "--no-rebase", "{url}", branch],
    );
    let heads: Vec<String> = seconds
      .iter()
      .map(|(_, dir)| git.ok(dir, &["rev-parse", "HEAD"]))
      .collect();
    rows.record(
      "pull-lands-the-same-commit",
      (heads[0] != heads[1])
        .then(|| format!("stock {} slates {}", heads[0].trim(), heads[1].trim())),
    );
  } else {
    rows.record(
      "fetch-and-pull",
      Some(format!(
        "a second clone failed: stock {} slates {}",
        second[0].2, second[1].2
      )),
    );
  }

  // The push shapes: a branch, tags of every kind, a force push, deletes.
  for (_, copy) in copies {
    git.ok(copy, &["branch", "correctness-topic"]);
    git.ok(copy, &["tag", "correctness-light"]);
    git.ok(
      copy,
      &["tag", "-a", "correctness-annotated", "-m", "a commit"],
    );
    let blob = git.ok(copy, &["rev-parse", "HEAD:correctness.txt"]);
    git.ok(
      copy,
      &[
        "tag",
        "-a",
        "correctness-of-a-blob",
        "-m",
        "a blob",
        blob.trim(),
      ],
    );
    git.ok(
      copy,
      &[
        "-c",
        "advice.nestedTag=false",
        "tag",
        "-a",
        "correctness-of-a-tag",
        "-m",
        "a tag",
        "correctness-annotated",
      ],
    );
  }
  for (row, refspec) in [
    ("push-branch", "correctness-topic"),
    ("push-lightweight-tag", "correctness-light"),
    ("push-annotated-tag", "correctness-annotated"),
    ("push-tag-of-a-blob", "correctness-of-a-blob"),
    ("push-tag-of-a-tag", "correctness-of-a-tag"),
  ] {
    same_outcome(rows, git, row, copies, &["push", "-q", "{url}", refspec]);
  }
  same_refs(
    rows,
    git,
    stock_copy,
    "refs-after-push-shapes",
    stock,
    slates,
  );
  for (_, copy) in copies {
    git.ok(copy, &["reset", "-q", "--hard", "HEAD~1"]);
  }
  same_outcome(
    rows,
    git,
    "push-non-fast-forward-refused",
    copies,
    &["push", "-q", "{url}", "HEAD"],
  );
  same_outcome(
    rows,
    git,
    "push-force",
    copies,
    &["push", "-q", "--force", "{url}", "HEAD"],
  );
  same_outcome(
    rows,
    git,
    "delete-branch",
    copies,
    &["push", "-q", "{url}", "--delete", "correctness-topic"],
  );
  same_outcome(
    rows,
    git,
    "delete-tags",
    copies,
    &[
      "push",
      "-q",
      "{url}",
      "--delete",
      "correctness-light",
      "correctness-annotated",
      "correctness-of-a-blob",
      "correctness-of-a-tag",
    ],
  );
  same_refs(
    rows,
    git,
    slates_copy,
    "refs-after-force-and-deletes",
    stock,
    slates,
  );
}

/// Where `git-lfs` is here: a file tracked by LFS pushed to slates over HTTP and cloned back, its bytes the same. Stock
/// git's http-backend serves no LFS, so this row compares slates with the bytes pushed.
fn lfs_row(rows: &mut Rows, git: &Git, scratch: &Scratch, http: &Server) {
  let has_lfs = git.run(&scratch.path, &[], &["lfs", "version"]).ok();
  if !has_lfs {
    eprintln!("skipping the LFS row: no git-lfs here");
    return;
  }
  let copy = format!("{}/clone-slates-http", scratch.path);
  if !std::path::Path::new(&copy).join(".git").exists() {
    rows.record(
      "lfs",
      Some("skipped: no slates copy to push from".to_owned()),
    );
    return;
  }
  let bytes: Vec<u8> = (0..1_048_576u32).map(|n| (n % 251) as u8).collect();
  let steps = [
    git.run(&copy, &[], &["lfs", "install", "--local"]),
    git.run(&copy, &[], &["lfs", "track", "*.lfsbin"]),
  ];
  written(&format!("{copy}/large.lfsbin"), &bytes);
  let added = git.run(&copy, &[], &["add", ".gitattributes", "large.lfsbin"]);
  let committed = git.run(&copy, &[], &["commit", "-q", "-m", "lfs"]);
  let pushed = git.run(&copy, &http.config, &["push", "-q", &http.url, "HEAD"]);
  let back = format!("{}/lfs-back", scratch.path);
  let cloned = git.run(
    &scratch.path,
    &http.config,
    &["clone", "-q", &http.url, &back],
  );
  let failure = steps
    .iter()
    .chain([&added, &committed, &pushed, &cloned])
    .find(|ran| !ran.ok())
    .map(Ran::said)
    .or_else(|| {
      let held = std::fs::read(format!("{back}/large.lfsbin")).unwrap_or_default();
      (held != bytes).then(|| {
        format!(
          "the clone's file holds {} bytes, not the 1 MiB pushed",
          held.len()
        )
      })
    });
  rows.record("lfs", failure);
}
