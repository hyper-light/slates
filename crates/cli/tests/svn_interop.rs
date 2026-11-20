//! Stock Subversion against a real `slates anchor` and its daemon: §4.17.4 item 14's differential run, its first
//! scripted sessions (UI goal 93). Stock `svn` 1.15 checks a repository out, commits, updates and reads its log, and
//! locks and unlocks where the route serves locks, over each route slates serves Subversion on: `svn://` (the daemon's
//! svn:// listener, `--vcs-svn`), `http://` (the WebDAV site on the shared HTTP edge, `--vcs-http`) and `svn+ssh://`
//! (`svnserve -t` behind the SSH edge, `--vcs-ssh`).
//!
//! **One session, two servers.** A session is a function of a client and a URL ([`round_trip`], [`locking`]), and the
//! first test runs both against stock `svnserve` 1.15 over scratch FSFS repositories, so what each step expects is
//! stock Subversion's own behaviour, checked on every run where the tools are present, and a slates leg that fails a
//! step fails it for slates' sake. A step expects what a user sees: the revision each commit names, the bytes each
//! update brings, the log's revisions, messages and changed paths, one author for one principal's commits, and a lock
//! that refuses another working copy's commit, the head left where it was, until it is released. Measured against
//! `svnserve -t` 1.15.0 on the Mac Studio (2026-11-19) before these tests were written: the commit under the lock is
//! refused `E160037` ("Cannot verify lock on path '/file'; no matching lock-token available"), exit 1; the sessions
//! check its exit and the head alone, which every route answers alike.
//!
//! **What waits.** Each slates leg is ignored naming the pieces it waits on and their agents, and the last of them to
//! land un-ignores it as its integration test. The `svn://` and `svn+ssh://` legs lock nothing: ra_svn's lock verbs are
//! answered as commands `svnserve` does not know (210001, `slates_svn::serve`), and no piece serves them yet.
//!
//! Stock `svn`, `svnadmin` and `svnserve` are found on the PATH, and a host without them, or whose `svn` is not of the
//! 1.15 line, skips loudly. Each test's working copies, `svn`'s configuration, its SSH key and the control's
//! repositories live in a scratch directory in the build output (`CARGO_TARGET_TMPDIR`), named with the process id and
//! removed at its end; `svn` reads no user's configuration and caches no credential. A slates leg mints its credential
//! (`slates credential token|key`, the issuer's verbs) with the anchor's issuer surface, which the anchor names on macOS
//! and Windows; on Linux the surface is a descriptor only the anchor's children hold, so those legs skip loudly there.
//! Unix only, as `tests/cli.rs` is: the control's tunnel runs through `sh`, and the SSH leg's through OpenSSH's `ssh`.
#![cfg(unix)]
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

/// Format: the start of `svn --version --quiet`'s answer on the Subversion line these tests hold slates to, 1.15
/// (1.15.0 recorded the fixtures under `crates/svn/fixtures`).
const SVN_LINE: &str = "1.15.";

/// Format: the scheme of the control's tunnel (`svn+control://`), which `svn` runs as its `config:tunnels` entry says.
const CONTROL_TUNNEL: &str = "control";

/// Format: the user stock `svnserve -t` takes the control's tunnel's to be (`--tunnel-user`): the author of the
/// control's every revision and the owner of its every lock.
const TUNNEL_USER: &str = "slates-test";

/// Format: the user name a slates leg gives where its route asks for one (Basic's, the SSH login); slates takes the
/// principal from the credential, never from this name.
const USER: &str = "slates";

/// Format: the repository each slates leg makes, which the first segment of each route's URL names.
const REPOSITORY: &str = "project";

/// Format: the file each session commits, at the repository's root.
const FILE: &str = "file";

/// Shape: the shards of each test's daemon, as `tests/cli.rs` starts one.
const SHARDS: &str = "2";

/// Shape: consecutive answered verbs before a test's daemon counts as settled, as `tests/cli.rs`'s start waits: an
/// anchor restarts a daemon whose first heartbeat lapsed, and a verb sent in that window is not answered.
const STABLE_STREAK: u32 = 10;

/// Shape: how long a daemon gets to answer verbs steadily once it serves, and one stock `svn` command to end: a guard
/// against a server that never answers, far above what either took on the Mac Studio (each of the control's commands
/// well under a second), never a measure of either.
const PATIENCE: Duration = Duration::from_secs(120);

/// Shape: the pause between polls of a settling daemon and of a running `svn`.
const POLL: Duration = Duration::from_millis(20);

/// Format: the start of the line a daemon prints once it serves (`crates/api/src/role/daemon.rs`); its instance
/// follows.
const SERVING_LINE: &str = "slates daemon: instance ";

/// Format: the anchor's line when its supervision finds its daemon in a crash loop (`crates/api/src/role/anchor.rs`,
/// A-119): a daemon that can never serve shows here, never as the anchor's exit.
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";

/// Format: the start of the daemon's line naming where its HTTP edge serves cleartext, on the loopback address a test
/// asks for (`crates/api/src/role/daemon.rs`, A-625 part 3); the port follows.
const VCS_HTTP_LINE: &str =
  "slates daemon: the version-control HTTP edge serves cleartext on 127.0.0.1:";

/// Format: the start of the daemon's line naming where its svn:// listener serves (A-689, as g234's branch prints it);
/// the port follows.
const VCS_SVN_LINE: &str = "slates daemon: the svn:// listener serves on 127.0.0.1:";

/// Format: the start of the daemon's line naming where its SSH edge serves: assumed in the HTTP edge's words, as the
/// Mercurial interop tests assume it, until A-684 (g299) prints one; the port's digits are read, and what follows them
/// (the host key's fingerprint) is left.
const VCS_SSH_LINE: &str = "slates daemon: the version-control SSH edge serves on 127.0.0.1:";

/// Format: the start of the anchor's line naming its issuer surface (`crates/api/src/role/anchor.rs`): the variables a
/// separate process exports to prove the human's authority, `NAME=VALUE` pairs separated by spaces. The issuer's verbs
/// (`credential token|key`) refuse without them.
const ISSUER_SURFACE_LINE: &str = "slates anchor: issuer surface: export ";

/// Shape: whether the anchor names its issuer surface through the environment: on macOS and Windows; on Linux the
/// surface is a descriptor only its children hold (`tests/cli.rs`, `PRINTS_ISSUER_SURFACE`, which records why this is
/// a platform fact and never read from the order of two processes' lines).
const PRINTS_ISSUER_SURFACE: bool = cfg!(not(target_os = "linux"));

/// Format: why a leg that mints a credential skips where the anchor names no issuer surface ([`PRINTS_ISSUER_SURFACE`]).
const NO_ISSUER_SURFACE: &str = "this platform publishes no issuer surface through the environment, so a separate \
  process cannot mint a credential (on Linux it is a descriptor only the anchor's children hold)";

/// A scratch directory in the build output (`CARGO_TARGET_TMPDIR`, under `target/`), named with the process id and a
/// count, removed when dropped, even when an assertion fails (A-50: a test's host files live in the build output, never
/// `/tmp`). Made by `mkdir`, as `tests/cli.rs` makes its own.
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
      path: std::fs::canonicalize(&raw)
        .unwrap()
        .to_string_lossy()
        .into_owned(),
    }
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = Command::new("rm").args(["-rf", &self.path]).output();
  }
}

/// Makes directory `path` and any parent it lacks, by `mkdir -p`.
fn made_directory(path: &str) {
  let made = Command::new("mkdir").args(["-p", path]).output().unwrap();
  assert!(made.status.success(), "mkdir -p {path}: {made:?}");
}

/// Whether this host runs stock Subversion's `svn` of the 1.15 line ([`SVN_LINE`]).
fn stock_svn() -> bool {
  Command::new("svn")
    .args(["--version", "--quiet"])
    .output()
    .is_ok_and(|output| {
      output.status.success() && String::from_utf8_lossy(&output.stdout).starts_with(SVN_LINE)
    })
}

/// Whether this host runs stock `svnadmin` and `svnserve` too, which the control's repositories and server are.
fn stock_svnserve() -> bool {
  ["svnadmin", "svnserve"].iter().all(|tool| {
    Command::new(tool)
      .arg("--version")
      .output()
      .is_ok_and(|output| output.status.success())
  })
}

/// Whether this host runs OpenSSH's `ssh`, which `svn` runs as its `svn+ssh` tunnel.
fn openssh() -> bool {
  Command::new("ssh")
    .arg("-V")
    .output()
    .is_ok_and(|output| output.status.success())
}

/// Stock `svn` as one session's client: its working copies in the test's scratch directory, each name beginning with
/// the session's; its own configuration directory there, so no user's configuration is read and no credential is
/// cached (`--no-auth-cache`); never a prompt (`--non-interactive`); the route's own options (its credentials, its
/// tunnel); and C's messages (`LC_ALL`), which the steps read.
struct Svn {
  /// The scratch directory.
  scratch: String,
  /// The session's name.
  session: String,
  /// The options every command takes before its subcommand.
  options: Vec<String>,
}

impl Svn {
  /// The client of session `session` in `scratch`, with `route`'s options.
  fn new(scratch: &Scratch, session: &str, route: Vec<String>) -> Svn {
    let mut options = vec![
      "--config-dir".to_owned(),
      format!("{}/svn-config", scratch.path),
      "--non-interactive".to_owned(),
      "--no-auth-cache".to_owned(),
    ];
    options.extend(route);
    Svn {
      scratch: scratch.path.clone(),
      session: session.to_owned(),
      options,
    }
  }

  /// The path of the session's working copy `name`.
  fn copy(&self, name: &str) -> String {
    format!("{}/{}-{name}", self.scratch, self.session)
  }

  /// Runs `svn` with `args` in `dir`, killed past [`PATIENCE`]: its exit code and both outputs.
  fn run(&self, dir: &str, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new("svn")
      .args(&self.options)
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
    let code = exit_within(&mut child, &format!("svn {args:?} in {dir}"));
    (code, out.join().unwrap(), err.join().unwrap())
  }

  /// Runs `svn` as [`Svn::run`] does, failing the test unless it succeeds: its standard output.
  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let (code, out, err) = self.run(dir, args);
    assert_eq!(code, 0, "svn {args:?} in {dir}: {out}{err}");
    out
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

/// Writes `text` as working copy `copy`'s [`FILE`]: the test writes the file stock `svn` commits, inside the scratch
/// directory it removes (R1's wall is shipped code's).
#[allow(clippy::disallowed_methods)]
fn put(copy: &str, text: &str) {
  std::fs::write(format!("{copy}/{FILE}"), text).unwrap();
}

/// Fails the test unless working copy `copy`'s [`FILE`] holds `text`.
fn expect_text(copy: &str, text: &str) {
  let held = std::fs::read_to_string(format!("{copy}/{FILE}")).unwrap();
  assert_eq!(held, text, "{copy}/{FILE}");
}

/// Fails the test unless `output` holds `line` as one of its lines.
fn expect_line(output: &str, line: &str) {
  assert!(
    output.lines().any(|held| held == line),
    "expected {line:?} in {output:?}"
  );
}

/// One revision of `svn log --xml --verbose`'s answer: its number, its author where it names one, its message and its
/// changed paths, each as its action and its path (`A /file`).
#[derive(Debug, PartialEq, Eq)]
struct Logged {
  revision: u64,
  author: Option<String>,
  message: String,
  paths: Vec<String>,
}

/// The revisions of `svn log --xml --verbose`'s answer `xml`, in its order, newest first. Read by its tags as `svn`
/// writes them (each attribute on a line of its own), not by an XML reader: the values the sessions read hold no
/// markup.
fn logged(xml: &str) -> Vec<Logged> {
  xml
    .split("<logentry")
    .skip(1)
    .map(|entry| Logged {
      revision: between(entry, "revision=\"", "\"")
        .and_then(|revision| revision.parse().ok())
        .unwrap_or_else(|| panic!("a revision without its number: {entry}")),
      author: between(entry, "<author>", "</author>").map(str::to_owned),
      message: between(entry, "<msg>", "</msg>")
        .unwrap_or_default()
        .to_owned(),
      paths: changed_paths(entry),
    })
    .collect()
}

/// The changed paths of one revision of `svn log --xml --verbose`'s answer, each as its action and its path.
fn changed_paths(entry: &str) -> Vec<String> {
  entry
    .split("<path")
    .skip(1)
    .filter(|path| path.starts_with(char::is_whitespace))
    .map(|path| {
      let action = between(path, "action=\"", "\"").unwrap_or_default();
      format!(
        "{action} {}",
        between(path, ">", "</path>").unwrap_or_default()
      )
    })
    .collect()
}

/// The text of `text` between the first `start` and the first `end` after it.
fn between<'t>(text: &'t str, start: &str, end: &str) -> Option<&'t str> {
  Some(text.split_once(start)?.1.split_once(end)?.0)
}

/// `url`'s youngest revision, as `svn info --show-item revision` answers it.
fn head(svn: &Svn, url: &str) -> String {
  svn
    .ok(&svn.scratch, &["info", "--show-item", "revision", url])
    .trim()
    .to_owned()
}

/// The author of `url`'s youngest revision, from its log.
fn author_of_head(svn: &Svn, url: &str) -> String {
  let log = svn.ok(&svn.scratch, &["log", "--xml", "--revision", "HEAD", url]);
  logged(&log)
    .into_iter()
    .next()
    .and_then(|revision| revision.author)
    .unwrap_or_else(|| panic!("the head names its author: {log}"))
}

/// Reads `url`'s log and fails the test unless it holds the round trip's two revisions, newest first, each with its
/// message and its changed path, both by one author.
fn expect_round_trip_log(svn: &Svn, url: &str) {
  let revisions = logged(&svn.ok(&svn.scratch, &["log", "--xml", "--verbose", url]));
  let author = revisions.first().and_then(|newest| newest.author.clone());
  assert!(author.is_some(), "the log names an author: {revisions:?}");
  let expected =
    [(2, "second", "M"), (1, "first", "A")].map(|(revision, message, action)| Logged {
      revision,
      author: author.clone(),
      message: message.to_owned(),
      paths: vec![format!("{action} /{FILE}")],
    });
  assert_eq!(revisions, expected, "the log");
}

/// The round trip, run as `svn` against `url`, a fresh repository: checked out at revision 0 into a first working
/// copy; a file added there and committed as revision 1; checked out into a second copy, which holds it; an edit
/// committed from the second as revision 2; the first updated to it; the log read. Fails the test unless each step
/// answers as stock Subversion does: the revision each names, the bytes each copy holds, and the log
/// ([`expect_round_trip_log`]).
fn round_trip(svn: &Svn, url: &str) {
  let first = svn.copy("first");
  let checked_out = svn.ok(&svn.scratch, &["checkout", url, &first]);
  expect_line(&checked_out, "Checked out revision 0.");
  put(&first, "first\n");
  svn.ok(&first, &["add", FILE]);
  let committed = svn.ok(&first, &["commit", "--message", "first"]);
  expect_line(&committed, "Committed revision 1.");
  let second = svn.copy("second");
  let checked_out = svn.ok(&svn.scratch, &["checkout", url, &second]);
  expect_line(&checked_out, "Checked out revision 1.");
  expect_text(&second, "first\n");
  put(&second, "first\nsecond\n");
  let committed = svn.ok(&second, &["commit", "--message", "second"]);
  expect_line(&committed, "Committed revision 2.");
  expect_line(&svn.ok(&first, &["update"]), "Updated to revision 2.");
  expect_text(&first, "first\nsecond\n");
  expect_round_trip_log(svn, url);
}

/// The lock session's start, run as `svn` against `url`, a fresh repository: a file committed as revision 1 from a
/// first working copy, and a second copy checked out. Fails the test unless the commit makes revision 1: the two
/// copies' paths.
fn one_file_in_two_copies(svn: &Svn, url: &str) -> (String, String) {
  let first = svn.copy("first");
  svn.ok(&svn.scratch, &["checkout", url, &first]);
  put(&first, "first\n");
  svn.ok(&first, &["add", FILE]);
  let committed = svn.ok(&first, &["commit", "--message", "first"]);
  expect_line(&committed, "Committed revision 1.");
  let second = svn.copy("second");
  svn.ok(&svn.scratch, &["checkout", url, &second]);
  (first, second)
}

/// The lock session, run as `svn` against `url`, a fresh repository ([`one_file_in_two_copies`]): the file locked in
/// the first copy; an edit committed from the second; the lock released; the same commit again. Fails the test unless
/// the lock is taken in the name of the user who committed revision 1, the commit under it is refused (exit 1) with the
/// head left at revision 1, the release answers, and the commit after it makes revision 2.
fn locking(svn: &Svn, url: &str) {
  let (first, second) = one_file_in_two_copies(svn, url);
  let author = author_of_head(svn, url);
  let locked = svn.ok(&first, &["lock", "--message", "held", FILE]);
  expect_line(&locked, &format!("'{FILE}' locked by user '{author}'."));
  put(&second, "first\nsecond\n");
  let (code, out, err) = svn.run(&second, &["commit", "--message", "under the lock"]);
  assert_eq!(
    code, 1,
    "a commit under another copy's lock is refused: {out}{err}"
  );
  assert_eq!(head(svn, url), "1", "the refused commit made no revision");
  expect_line(
    &svn.ok(&first, &["unlock", FILE]),
    &format!("'{FILE}' unlocked."),
  );
  let committed = svn.ok(&second, &["commit", "--message", "after the lock"]);
  expect_line(&committed, "Committed revision 2.");
}

/// §4.17.4 item 14, the differential run's control. Do: make two FSFS repositories with stock `svnadmin create` in a
/// scratch directory; run the round trip against one and the lock session against the other as stock `svn` 1.15,
/// through stock `svnserve -t` started by `svn`'s own tunnel (`config:tunnels`, the scheme `svn+control`) as
/// [`TUNNEL_USER`]. Expect: every step as each session states. That makes the sessions stock Subversion's own
/// behaviour, so each slates leg below is held to what `svnserve` does. Skips loudly without stock Subversion 1.15
/// (`svn`, `svnadmin` and `svnserve` on the PATH).
#[test]
fn stock_svn_sessions_hold_against_stock_svnserve() {
  if !stock_svn() || !stock_svnserve() {
    eprintln!(
      "skipping the control's sessions: no stock Subversion 1.15 here (svn, svnadmin and svnserve on the PATH)"
    );
    return;
  }
  let scratch = Scratch::new("svn-control");
  let root = format!("{}/repositories", scratch.path);
  made_directory(&root);
  for name in ["trip", "locks"] {
    let made = Command::new("svnadmin")
      .args(["create", &format!("{root}/{name}")])
      .output()
      .unwrap();
    assert!(made.status.success(), "svnadmin create {name}: {made:?}");
  }
  let tunnel = vec![
    "--config-option".to_owned(),
    format!(
      "config:tunnels:{CONTROL_TUNNEL}=sh -c 'exec svnserve -t -r {root} --tunnel-user={TUNNEL_USER}'"
    ),
  ];
  let trip = Svn::new(&scratch, "trip", tunnel.clone());
  round_trip(&trip, &format!("svn+{CONTROL_TUNNEL}://localhost/trip"));
  let locks = Svn::new(&scratch, "locks", tunnel);
  locking(&locks, &format!("svn+{CONTROL_TUNNEL}://localhost/locks"));
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

/// Runs `slates --instance INSTANCE ARGS`: its exit code and both outputs.
fn slates(instance: &str, args: &[&str]) -> (i32, String, String) {
  slates_as_issuer(instance, &[], args)
}

/// Runs `slates --instance INSTANCE ARGS` with the issuer surface's variables `issuer` in its environment, as the
/// issuer's verbs need: its exit code and both outputs.
fn slates_as_issuer(
  instance: &str,
  issuer: &[(String, String)],
  args: &[&str],
) -> (i32, String, String) {
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

/// The anchor's issuer surface, where this platform names one: the variables the issuer's verbs run with.
type IssuerSurface = Option<Vec<(String, String)>>;

/// Starts `slates anchor` whose daemon is asked for `edges` (a flag and a loopback address whose port the system
/// picks), reads what the anchor says until its daemon serves and, where this platform names one, the anchor has named
/// its issuer surface ([`lines_until_serving`]), waits until the daemon answers verbs steadily, and makes the root
/// consensus group as a first start does (`slates bootstrap root`): the anchor, the lines read, which name the edges'
/// ports, and the issuer surface.
fn start_anchor(instance: &str, edges: &[&str]) -> (Anchor, Vec<String>, IssuerSurface) {
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
  let (lines, issuer) = lines_until_serving(&mut stderr, instance);
  anchor.drain = Some(std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  }));
  settle(instance);
  let (code, out, err) = slates(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap root: {out}{err}");
  (anchor, lines, issuer)
}

/// Reads the anchor's lines until its daemon says it serves and, where this platform names one, the anchor has named its
/// issuer surface, the two processes' lines taken in whichever order they arrive, echoing each to this process's
/// standard error, where they went before it was piped: the lines read, and the issuer surface. Ends the test where the
/// anchor ends first or reports its daemon in a crash loop, the anchor's own bound on a daemon that cannot serve.
fn lines_until_serving(stderr: &mut impl BufRead, instance: &str) -> (Vec<String>, IssuerSurface) {
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

/// The issuer surface `issuer`, or `None` where this platform names none, which is said for `leg` as its skip.
fn issuer_or_skip(issuer: IssuerSurface, leg: &str) -> IssuerSurface {
  if issuer.is_none() {
    eprintln!("skipping {leg}: {NO_ISSUER_SURFACE}");
  }
  issuer
}

/// Waits until the daemon answers [`STABLE_STREAK`] verbs in a row, within [`PATIENCE`].
fn settle(instance: &str) {
  let started = Instant::now();
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if slates(instance, &["volume", "list"]).0 == 0 {
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

/// Makes Subversion repository [`REPOSITORY`] through the CLI (`slates repository create`, A-685).
fn create_repository(instance: &str) {
  let (code, out, err) = slates(
    instance,
    &["repository", "create", REPOSITORY, "--tool", "svn"],
  );
  assert_eq!(code, 0, "repository create: {out}{err}");
}

/// A token `slates credential token` mints for this process's own principal, run with the anchor's issuer surface
/// `issuer`, as the issuer's verbs must be: its id and the token, as the verb prints them.
fn minted_token(instance: &str, issuer: &[(String, String)]) -> (String, String) {
  let (code, out, err) = slates_as_issuer(instance, issuer, &["credential", "token"]);
  assert_eq!(code, 0, "credential token: {out}{err}");
  (value_of(&out, "id"), value_of(&out, "token"))
}

/// `svn`'s options giving `user` and `password` wherever the route asks for credentials.
fn credentials(user: String, password: String) -> Vec<String> {
  vec![
    "--username".to_owned(),
    user,
    "--password".to_owned(),
    password,
  ]
}

/// One slates leg: the anchor its daemon serves from, the scratch directory, the client and the repository's URL.
/// Dropped in that order: the daemon ends with its anchor, then the scratch directory goes.
struct Leg {
  _anchor: Anchor,
  _scratch: Scratch,
  svn: Svn,
  url: String,
}

/// The HTTP leg of session `session`: an anchor with its HTTP edge on loopback (`--vcs-http 127.0.0.1:0`), the
/// repository, a token, and `svn` giving the token as Basic's password at `http://127.0.0.1:PORT/project`; `None` where
/// this platform names no issuer surface to mint the token with, which is said as the leg's skip.
fn http_leg(session: &str) -> Option<Leg> {
  let instance = format!("svn-http-{session}-{}", std::process::id());
  let (anchor, lines, issuer) = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&lines);
  let issuer = issuer_or_skip(issuer, &format!("the HTTP leg of {session}"))?;
  create_repository(&instance);
  let (_, token) = minted_token(&instance, &issuer);
  let scratch = Scratch::new(&format!("svn-http-{session}"));
  let svn = Svn::new(&scratch, session, credentials(USER.to_owned(), token));
  Some(Leg {
    _anchor: anchor,
    _scratch: scratch,
    svn,
    url: format!("http://127.0.0.1:{port}/{REPOSITORY}"),
  })
}

/// §4.17.4 item 14 over `svn://` (item 5's svn://: the daemon's svn:// listener running A-656's connection). Do: start
/// an anchor with its svn:// listener on loopback (`--vcs-svn 127.0.0.1:0`); make the repository and a token (`slates
/// credential token`); run the round trip as stock `svn` 1.15 at `svn://127.0.0.1:PORT/project`, with the token's id
/// and the token as the user and password `svn` answers CRAM-MD5 with, as A-692 proves a token's principal (the token's
/// record keeps its CRAM-MD5 keyed contexts; `slates_server::credentials::cram_md5_key`). Expect: every step as
/// against `svnserve` ([`stock_svn_sessions_hold_against_stock_svnserve`]). Skips loudly without stock Subversion
/// 1.15. Its waits, as they ended: the listener (A-689) answered every URL "No repository found" until A-695 looked the
/// repository up, and its commits landed through the owner's step since 7bedc3a88.
#[test]
fn stock_svn_checks_out_commits_updates_and_logs_over_svn_protocol() {
  if !stock_svn() {
    eprintln!("skipping stock svn over svn://: no stock Subversion 1.15 here (svn on the PATH)");
    return;
  }
  let instance = format!("svn-protocol-{}", std::process::id());
  let (_anchor, lines, issuer) = start_anchor(&instance, &["--vcs-svn", "127.0.0.1:0"]);
  let port = port_after(&lines, VCS_SVN_LINE).expect("the daemon named its svn:// listener's port");
  let Some(issuer) = issuer_or_skip(issuer, "stock svn over svn://") else {
    return;
  };
  create_repository(&instance);
  let (id, token) = minted_token(&instance, &issuer);
  let scratch = Scratch::new("svn-protocol");
  let svn = Svn::new(&scratch, "trip", credentials(id, token));
  round_trip(&svn, &format!("svn://127.0.0.1:{port}/{REPOSITORY}"));
}

/// §4.17.4 item 14 over `http://` (item 5's HTTP: the WebDAV site on the shared HTTP edge, a repository under the
/// URL's first segment, as `SVNParentPath` serves one). Do: start an anchor with its HTTP edge on loopback
/// (`--vcs-http 127.0.0.1:0`); make the repository and a token; run the round trip as stock `svn` 1.15 at
/// `http://127.0.0.1:PORT/project`, the token as Basic's password. Expect: every step as against `svnserve`. Skips
/// loudly without stock Subversion 1.15. Its waits, as they ended: the edge's Subversion site (A-681's reads, then the
/// daemon's routing), the update-report, and HTTPv2's commits with the edge's `204` (A-681 part C); serf sends Basic
/// credentials only after a Basic challenge, which the loopback edge offers first, and takes, since A-686 (g258).
#[test]
fn stock_svn_checks_out_commits_updates_and_logs_over_the_vcs_http_edge() {
  if !stock_svn() {
    eprintln!("skipping stock svn over HTTP: no stock Subversion 1.15 here (svn on the PATH)");
    return;
  }
  let Some(leg) = http_leg("trip") else {
    return;
  };
  round_trip(&leg.svn, &leg.url);
}

/// §4.17.4 item 14's lock session over `http://` (item 7's locks: `LOCK`, `UNLOCK` and the lock discovery `svn`
/// reads). Do: start the HTTP leg as the HTTP round trip does; run the lock session. Expect: every step as against
/// `svnserve`. Skips loudly without stock Subversion 1.15. Its waits, as they ended: A-681's commits, then its `LOCK`
/// and `UNLOCK` over A-627's bodies and A-602's lock table, landed as A-693 part 2's `Record::Locks`, with a commit's
/// lock tokens (the `If` header, the MERGE's `S:lock-token-list`); the edge's Basic, offered first and taken on loopback
/// (A-686, g258).
#[test]
fn stock_svn_locks_and_unlocks_over_the_vcs_http_edge() {
  if !stock_svn() {
    eprintln!(
      "skipping stock svn's locks over HTTP: no stock Subversion 1.15 here (svn on the PATH)"
    );
    return;
  }
  let Some(leg) = http_leg("locks") else {
    return;
  };
  locking(&leg.svn, &leg.url);
}

/// A fresh Ed25519 key `ssh-keygen` makes in `scratch`, with no passphrase: its private half's path and its public
/// line, or `None` where this host has no `ssh-keygen`.
fn ssh_key(scratch: &Scratch) -> Option<(String, String)> {
  let key = format!("{}/key", scratch.path);
  let made = Command::new("ssh-keygen")
    .args([
      "-q",
      "-t",
      "ed25519",
      "-N",
      "",
      "-C",
      "slates-svn-test",
      "-f",
      &key,
    ])
    .output()
    .ok()?;
  assert!(made.status.success(), "ssh-keygen: {made:?}");
  let public = std::fs::read_to_string(format!("{key}.pub")).unwrap();
  Some((key, public.trim().to_owned()))
}

/// §4.17.4 item 14 over `svn+ssh://` (piece 12: `svnserve -t` behind the SSH edge, the tunnel's user the principal the
/// client's key proved). Do: make an Ed25519 key; start an anchor with its SSH edge on loopback (`--vcs-ssh
/// 127.0.0.1:0`); make the repository and enroll the key (`slates credential key`); run the round trip as stock `svn`
/// 1.15 at `svn+ssh://slates@127.0.0.1/project`, its `ssh` tunnel OpenSSH holding that key alone at the edge's port,
/// reading no user's configuration and keeping no host key. Expect: every step as against `svnserve`. Skips loudly
/// without stock Subversion 1.15, `ssh` or `ssh-keygen`. Its waits, as they ended: the daemon's SSH edge (A-684), and
/// `svnserve -t` on it over its shard's records, committing through the owner's step (fc79d36ae).
#[test]
fn stock_svn_checks_out_commits_updates_and_logs_over_svn_ssh() {
  if !stock_svn() || !openssh() {
    eprintln!(
      "skipping stock svn over svn+ssh: no stock Subversion 1.15 or OpenSSH here (svn, ssh on the PATH)"
    );
    return;
  }
  let scratch = Scratch::new("svn-ssh");
  let Some((key, public)) = ssh_key(&scratch) else {
    eprintln!("skipping stock svn over svn+ssh: no ssh-keygen here");
    return;
  };
  let instance = format!("svn-ssh-{}", std::process::id());
  let (_anchor, lines, issuer) = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
  let port = port_after(&lines, VCS_SSH_LINE).expect("the daemon named its SSH edge's port");
  let Some(issuer) = issuer_or_skip(issuer, "stock svn over svn+ssh") else {
    return;
  };
  create_repository(&instance);
  let (code, out, err) = slates_as_issuer(&instance, &issuer, &["credential", "key", &public]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let tunnel = format!(
    "config:tunnels:ssh=ssh -q -F /dev/null -p {port} -i {key} -o IdentitiesOnly=yes -o BatchMode=yes \
     -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null --"
  );
  let svn = Svn::new(&scratch, "trip", vec!["--config-option".to_owned(), tunnel]);
  round_trip(&svn, &format!("svn+ssh://{USER}@127.0.0.1/{REPOSITORY}"));
}
