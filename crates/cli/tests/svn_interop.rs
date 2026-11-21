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
//! Every platform: on a host without stock Subversion 1.15 (jupiter, until Ada decides on installing it) each leg skips
//! loudly; the control's tunnel runs through `sh`, and the SSH leg's through OpenSSH's `ssh`.
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

/// Shape: whether the anchor names its issuer surface through the environment: on every platform since A-754 (macOS
/// and Windows the segment's name, Linux its issuer route and secret; `tests/cli.rs`, `PRINTS_ISSUER_SURFACE`).
const PRINTS_ISSUER_SURFACE: bool = true;

/// Format: why a leg that mints a credential skips where the anchor names no issuer surface ([`PRINTS_ISSUER_SURFACE`]).
const NO_ISSUER_SURFACE: &str = "this platform publishes no issuer surface through the environment, so a separate \
  process cannot mint a credential";

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
  /// The directory its daemon's changeset store lives in (`--vcs-store`; C2: a daemon without one makes no Subversion
  /// repository), removed after the anchor is killed; none where the caller named its own.
  _store: Option<Scratch>,
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
  // A Subversion repository's history lands on the daemon's changeset store (C2): a store directory in a scratch of
  // its own, unless the caller named one.
  let store = (!edges.contains(&"--vcs-store")).then(|| Scratch::new("vcs-store"));
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
    .args(
      store
        .iter()
        .flat_map(|store| ["--vcs-store", store.path.as_str()]),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let mut anchor = Anchor {
    child,
    drain: None,
    _store: store,
  };
  let (lines, issuer) = lines_until_serving(&mut stderr, instance);
  anchor.drain = Some(std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  }));
  settle(instance, anchor.child.id());
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

/// Waits until the daemon answers [`STABLE_STREAK`] verbs in a row, while the tree under its anchor `anchor` gains
/// CPU ([`process_wait::Judged`]; until 2026-11-20 within [`PATIENCE`]).
fn settle(instance: &str, anchor: u32) {
  let mut judged = process_wait::Judged::of(anchor);
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if slates(instance, &["volume", "list"]).0 == 0 {
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

/// The svn:// listener's port the daemon named in `lines`; a test fails when it named none.
fn svn_listener_port(lines: &[String]) -> u16 {
  port_after(lines, VCS_SVN_LINE).expect("the daemon named its svn:// listener's port")
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
  let port = svn_listener_port(&lines);
  let Some(issuer) = issuer_or_skip(issuer, "stock svn over svn://") else {
    return;
  };
  create_repository(&instance);
  let (id, token) = minted_token(&instance, &issuer);
  let scratch = Scratch::new("svn-protocol");
  let svn = Svn::new(&scratch, "trip", credentials(id, token));
  round_trip(&svn, &format!("svn://127.0.0.1:{port}/{REPOSITORY}"));
}

/// P7's finding (1), main's interop case of 2026-11-21 (10:55, condition 1): a copy from an older revision, made by the
/// server from the head and set back from the records, as stock `svn` 1.15 sees it. Do: start an anchor with its
/// `svn://` listener; as stock `svn`, commit a directory `d` holding [`FILE`] (`one`) as revision 1; change the file to
/// `two` (revision 2); add `d/later` (revision 3); then copy `d` at revision 1 to `e` (revision 4). Expect: the copy
/// committed as revision 4; `e` listing [`FILE`] alone, its text `one`; `d`'s text still `two`; and revision 4's log
/// naming `/e` added from `/d:1`, as `svnserve` names a copy. Skips loudly without stock Subversion 1.15.
#[test]
fn stock_svn_copies_a_directory_from_an_older_revision_over_svn_protocol() {
  if !stock_svn() {
    eprintln!(
      "skipping stock svn's copy from an older revision: no stock Subversion 1.15 here (svn on the PATH)"
    );
    return;
  }
  let instance = format!("svn-old-copy-{}", std::process::id());
  let (_anchor, lines, issuer) = start_anchor(&instance, &["--vcs-svn", "127.0.0.1:0"]);
  let port = svn_listener_port(&lines);
  let Some(issuer) = issuer_or_skip(issuer, "stock svn's copy from an older revision") else {
    return;
  };
  create_repository(&instance);
  let (id, token) = minted_token(&instance, &issuer);
  let scratch = Scratch::new("svn-old-copy");
  let svn = Svn::new(&scratch, "copy", credentials(id, token));
  let url = format!("svn://127.0.0.1:{port}/{REPOSITORY}");
  let copy = svn.copy("work");
  svn.ok(&svn.scratch, &["checkout", &url, &copy]);
  svn.ok(&copy, &["mkdir", "d"]);
  let within = format!("{copy}/d");
  put(&within, "one\n");
  svn.ok(&within, &["add", FILE]);
  expect_line(
    &svn.ok(&copy, &["commit", "--message", "d"]),
    "Committed revision 1.",
  );
  put(&within, "two\n");
  expect_line(
    &svn.ok(&copy, &["commit", "--message", "two"]),
    "Committed revision 2.",
  );
  svn.ok(&within, &["mkdir", "later"]);
  expect_line(
    &svn.ok(&copy, &["commit", "--message", "later"]),
    "Committed revision 3.",
  );
  let copied = svn.ok(
    &svn.scratch,
    &[
      "copy",
      "--revision",
      "1",
      &format!("{url}/d"),
      &format!("{url}/e"),
      "--message",
      "from 1",
    ],
  );
  expect_line(&copied, "Committed revision 4.");
  let listed = svn.ok(&svn.scratch, &["list", &format!("{url}/e")]);
  assert_eq!(listed, format!("{FILE}\n"), "e's listing");
  let text = svn.ok(&svn.scratch, &["cat", &format!("{url}/e/{FILE}")]);
  assert_eq!(text, "one\n", "e's text");
  let head = svn.ok(&svn.scratch, &["cat", &format!("{url}/d/{FILE}")]);
  assert_eq!(head, "two\n", "d's text");
  let logged = svn.ok(&svn.scratch, &["log", "--verbose", "--revision", "4", &url]);
  expect_line(&logged, "   A /e (from /d:1)");
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

/// The svn:// leg of a revision-property test: the anchor, the scratch, the client and the URL, revision 1 committed;
/// none (the test skipped loudly) without stock Subversion 1.15 or where the platform names no issuer surface. The
/// anchor is given a store directory in the scratch (`--vcs-store`, A-767's standing grant), where the daemon mounts a
/// repository's FSFS view for its hooks (A-645 (a)): [`STORE`].
fn revprop_leg(instance: &str) -> Option<(Anchor, Scratch, Svn, String)> {
  if !stock_svn() {
    eprintln!(
      "skipping the revision property tests: no stock Subversion 1.15 here (svn on the PATH)"
    );
    return None;
  }
  let scratch = Scratch::new("svn-revprop");
  let store = format!("{}/{STORE}", scratch.path);
  made_directory(&store);
  let (anchor, lines, issuer) = start_anchor(
    instance,
    &["--vcs-svn", "127.0.0.1:0", "--vcs-store", &store],
  );
  let port = svn_listener_port(&lines);
  let issuer = issuer_or_skip(issuer, "the revision property tests")?;
  create_repository(instance);
  let (id, token) = minted_token(instance, &issuer);
  let svn = Svn::new(&scratch, "revprop", credentials(id, token));
  let url = format!("svn://127.0.0.1:{port}/{REPOSITORY}");
  let first = svn.copy("first");
  svn.ok(&svn.scratch, &["checkout", &url, &first]);
  put(&first, "first\n");
  svn.ok(&first, &["add", FILE]);
  expect_line(
    &svn.ok(&first, &["commit", "--message", "first"]),
    "Committed revision 1.",
  );
  Some((anchor, scratch, svn, url))
}

/// Format: the store directory a revision-property test's anchor is given, in its scratch (`--vcs-store`).
const STORE: &str = "store";

/// Writes a hook program at `path` that reads `REPOS-PATH`, its first argument, with stock `svnlook youngest` by the
/// absolute path `svnlook` (a Subversion hook starts with an empty environment, so no `PATH`), writes what it read to
/// `out`, and allows what it is asked where the read succeeded.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)]
fn reading_hook(path: &str, svnlook: &str, out: &str) {
  use std::os::unix::fs::PermissionsExt;
  std::fs::write(
    path,
    format!("#!/bin/sh\n\"{svnlook}\" youngest \"$1\" > \"{out}\" || exit 1\nexit 0\n"),
  )
  .unwrap();
  std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Stock `svnlook`'s absolute path, found on this process's `PATH`; none where it is not.
#[cfg(unix)]
fn svnlook_path() -> Option<String> {
  let path = std::env::var_os("PATH")?;
  std::env::split_paths(&path)
    .map(|directory| directory.join("svnlook"))
    .find(|candidate| candidate.is_file())
    .map(|found| found.to_string_lossy().into_owned())
}

/// A-622, A-702 part 2 and A-645 (a) (`slates repository policy`; `hooks.c` `svn_repos__hooks_pre_revprop_change`;
/// `REPOS-PATH`). Do: over the svn:// listener of an anchor given a store directory, with revision 1 committed as stock
/// `svn` 1.15, configure a pre-revprop-change hook that reads its `REPOS-PATH` with stock `svnlook youngest`
/// (`slates repository policy project --hook pre-revprop-change=PROGRAM`, the owner's verb), change revision 1's
/// `svn:log` with stock `svn propset --revprop`, and read it; then unmount the view. Expect the verb's answer naming the
/// hook at policy version 1, the change taken, the value read back, and the hook's `svnlook` reading revision 1 as the
/// youngest: `REPOS-PATH` is the repository's FSFS view, mounted in the store directory. Red on main before A-645 (a):
/// the hook was refused (E200007, "its root mount is not yet published") and the change with it.
// Unix alone: hooks run where the hook driver does.
#[cfg(unix)]
#[test]
fn a_repository_policys_hook_lets_stock_svn_change_a_revision_property() {
  let instance = format!("svn-revprop-{}", std::process::id());
  let Some(svnlook) = svnlook_path() else {
    eprintln!("skipping the revision property hook test: no stock svnlook here");
    return;
  };
  let Some((_anchor, scratch, svn, url)) = revprop_leg(&instance) else {
    return;
  };
  let hook = format!("{}/allow-revprop-change", scratch.path);
  let read = format!("{}/youngest-read", scratch.path);
  reading_hook(&hook, &svnlook, &read);
  let given = format!("pre-revprop-change={hook}");
  let (code, out, err) = slates(
    &instance,
    &["repository", "policy", REPOSITORY, "--hook", &given],
  );
  let changed = svn.run(
    &svn.scratch,
    &["propset", "--revprop", "-r1", "svn:log", "changed", &url],
  );
  let value = svn.ok(
    &svn.scratch,
    &["propget", "--revprop", "-r1", "svn:log", &url],
  );
  let youngest = std::fs::read_to_string(&read).unwrap_or_default();
  let view = format!("{}/{STORE}/{REPOSITORY}.svn", scratch.path);
  let unmounted = slates(&instance, &["unmount", &view]);
  assert_eq!(
    (
      (code, out.contains("version: 1"), out.contains(&given)),
      changed.0,
      value.trim(),
      youngest.trim(),
      unmounted.0,
    ),
    ((0, true, true), 0, "changed", "1", 0),
    "policy: {out}{err}; changed: {changed:?}; unmount: {unmounted:?}"
  );
}

/// Sends one HTTP/1.1 request to the edge at `port` and reads its whole answer: `method` at `path`, a bearer `token`,
/// the request's other `headers` and `body`; the answer's text.
fn edge_request(
  port: u16,
  (method, path): (&str, &str),
  token: &str,
  headers: &[&str],
  body: &[u8],
) -> String {
  use std::io::{Read, Write};
  let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let mut request = format!(
    "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {}\r\n",
    body.len()
  )
  .into_bytes();
  for header in headers {
    request.extend_from_slice(header.as_bytes());
    request.extend_from_slice(b"\r\n");
  }
  request.extend_from_slice(b"\r\n");
  request.extend_from_slice(body);
  stream.write_all(&request).unwrap();
  let mut answer = Vec::new();
  let _ = stream.read_to_end(&mut answer);
  String::from_utf8_lossy(&answer).into_owned()
}

/// A-645 (a), the view's mount by itself, on every Unix (Linux's FUSE mount, macOS's NFS mount), with no stock client:
/// the DAV site runs start-commit at a commit's POST (A-733). Do: start an anchor with its HTTP edge and a store
/// directory (`--vcs-store`); make a Subversion repository and a token; configure a start-commit hook that copies
/// `$1/db/current` and `$1/format` (its `REPOS-PATH`'s FSFS files) to a file and refuses; POST `( create-txn )` to
/// `!svn/me`; then unmount the view. Expect the hook to have read `0` and `5` (revision 0 the youngest, repository
/// format 5) through the view mounted at `<store>/project.svn`, the commit refused by the hook, and the unmount to
/// succeed. Red before A-645 (a): the hook was refused unrun, the file never written.
#[cfg(unix)]
#[test]
#[allow(clippy::disallowed_methods)]
fn a_hooks_repos_path_is_the_repositorys_view_mounted_in_the_store_directory() {
  use std::os::unix::fs::PermissionsExt;
  let instance = format!("svn-view-{}", std::process::id());
  let scratch = Scratch::new("svn-view");
  let store = format!("{}/{STORE}", scratch.path);
  made_directory(&store);
  let (_anchor, lines, issuer) = start_anchor(
    &instance,
    &["--vcs-http", "127.0.0.1:0", "--vcs-store", &store],
  );
  let port = http_edge_port(&lines);
  let Some(issuer) = issuer_or_skip(issuer, "the view's mount") else {
    return;
  };
  create_repository(&instance);
  let (_, token) = minted_token(&instance, &issuer);
  let read = format!("{}/view-read", scratch.path);
  let hook = format!("{}/start-commit", scratch.path);
  std::fs::write(
    &hook,
    format!("#!/bin/sh\ncat \"$1/db/current\" \"$1/format\" > \"{read}\"\nexit 1\n"),
  )
  .unwrap();
  std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
  let given = format!("start-commit={hook}");
  let (code, out, err) = slates(
    &instance,
    &["repository", "policy", REPOSITORY, "--hook", &given],
  );
  assert_eq!(code, 0, "policy: {out}{err}");
  let answer = edge_request(
    port,
    ("POST", &format!("/{REPOSITORY}/!svn/me")),
    &token,
    &["Content-Type: application/vnd.svn-skel"],
    b"( create-txn )",
  );
  let seen = std::fs::read_to_string(&read).unwrap_or_default();
  let view = format!("{store}/{REPOSITORY}.svn");
  let unmounted = slates(&instance, &["unmount", &view]);
  assert_eq!(
    (seen.as_str(), unmounted.0),
    ("0\n5\n", 0),
    "answer: {answer}; unmount: {unmounted:?}"
  );
}

/// Shape: the revisions of the repository a hook's first view is laid out for: git/git's newest 2,000 first-parent
/// commits as a dump were the repository whose hooks the owner shard's one liveness budget refused (w-interop's svn
/// matrix, release, Mac Studio, 2026-11-20: "its owner shard did not answer in time"), and a debug build lays each out
/// slower than that release did.
const MANY_REVISIONS: u64 = 2_000;

/// Shape: the files of the one directory every revision of [`many_revisions_dump`] changes a file of: as many as
/// git/git's tree holds at f7de81456940 (4,872), the dump the matrix met the refusal with. Every revision's file in an
/// FSFS view carries the listing of each directory on a changed path, so each revision costs the view as much as this
/// listing; a history of one file lays 2,000 revisions out inside a liveness budget even in a debug build (this test
/// with one file passed on main a51023f57, 22.8 s).
const MANY_REVISIONS_FILES: u64 = 4_872;

/// A dump of `revisions` revisions under `trunk/`: the first adds [`MANY_REVISIONS_FILES`] files, each later one
/// changes one of them: the history a hook's first view must lay out a revision at a time.
fn many_revisions_dump(revisions: u64) -> Vec<u8> {
  let props = |log: &str| format!("K 7\nsvn:log\nV {}\n{log}\nPROPS-END\n", log.len());
  let file = |path: String, action: &str, body: &str| {
    format!(
      "Node-path: {path}\nNode-kind: file\nNode-action: {action}\nText-content-length: {0}\nContent-length: {0}\n\n\
       {body}\n\n",
      body.len()
    )
  };
  let mut out = String::from(
    "SVN-fs-dump-format-version: 2\n\nUUID: 00000000-0000-4000-8000-0000000064a6\n\nRevision-number: 0\n\
     Prop-content-length: 10\nContent-length: 10\n\nPROPS-END\n\n",
  );
  for revision in 1..=revisions {
    let log = props(&format!("r{revision}"));
    out.push_str(&format!(
      "Revision-number: {revision}\nProp-content-length: {0}\nContent-length: {0}\n\n{log}\n",
      log.len()
    ));
    if revision == 1 {
      out.push_str(
        "Node-path: trunk\nNode-kind: dir\nNode-action: add\nProp-content-length: 10\nContent-length: 10\n\n\
         PROPS-END\n\n",
      );
      for at in 0..MANY_REVISIONS_FILES {
        out.push_str(&file(format!("trunk/f{at:05}"), "add", &format!("{at}\n")));
      }
    } else {
      let at = revision % MANY_REVISIONS_FILES;
      out.push_str(&file(
        format!("trunk/f{at:05}"),
        "change",
        &format!("{at} r{revision}\n"),
      ));
    }
  }
  out.into_bytes()
}

/// A-645 (a), a hook's first view of a long history (w-interop's svn matrix, 2026-11-21). Do: start an anchor with its
/// HTTP edge and a store directory; import a repository of [`MANY_REVISIONS`] revisions (`slates repository import`);
/// configure a start-commit hook that copies `$1/db/current` (its `REPOS-PATH`'s youngest) to a file and refuses; POST
/// `( create-txn )` to `!svn/me`, so the hook asks for the repository's first view; then unmount the view. Expect the
/// hook to have read the youngest revision through the view and the unmount to succeed: the wait for the view is
/// judged on its progress, however long its history. Red on main a51023f57: the first view had one liveness budget of
/// the owner shard's time, so the hook was refused unrun ("its owner shard did not answer in time"), the file never
/// written.
#[cfg(unix)]
#[test]
#[allow(clippy::disallowed_methods)]
fn a_hooks_first_view_of_a_long_history_is_waited_for_while_it_is_laid_out() {
  use std::os::unix::fs::PermissionsExt;
  let instance = format!("svn-long-view-{}", std::process::id());
  let scratch = Scratch::new("svn-long-view");
  let store = format!("{}/{STORE}", scratch.path);
  made_directory(&store);
  let (_anchor, lines, issuer) = start_anchor(
    &instance,
    &["--vcs-http", "127.0.0.1:0", "--vcs-store", &store],
  );
  let port = http_edge_port(&lines);
  let Some(issuer) = issuer_or_skip(issuer, "a long history's first view") else {
    return;
  };
  create_repository(&instance);
  let dump = format!("{}/many.dump", scratch.path);
  std::fs::write(&dump, many_revisions_dump(MANY_REVISIONS)).unwrap();
  let imported = slates(&instance, &["repository", "import", REPOSITORY, &dump]);
  assert_eq!(imported.0, 0, "import: {imported:?}");
  let (_, token) = minted_token(&instance, &issuer);
  let read = format!("{}/view-read", scratch.path);
  let hook = format!("{}/start-commit", scratch.path);
  std::fs::write(
    &hook,
    format!("#!/bin/sh\ncat \"$1/db/current\" > \"{read}\"\nexit 1\n"),
  )
  .unwrap();
  std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
  let given = format!("start-commit={hook}");
  let (code, out, err) = slates(
    &instance,
    &["repository", "policy", REPOSITORY, "--hook", &given],
  );
  assert_eq!(code, 0, "policy: {out}{err}");
  let answer = edge_request(
    port,
    ("POST", &format!("/{REPOSITORY}/!svn/me")),
    &token,
    &["Content-Type: application/vnd.svn-skel"],
    b"( create-txn )",
  );
  let seen = std::fs::read_to_string(&read).unwrap_or_default();
  let view = format!("{store}/{REPOSITORY}.svn");
  let unmounted = slates(&instance, &["unmount", &view]);
  assert_eq!(
    (seen.as_str(), unmounted.0),
    (format!("{MANY_REVISIONS}\n").as_str(), 0),
    "answer: {answer}; unmount: {unmounted:?}"
  );
}

/// A-702 part 2 (`fs-wrap.c` `svn_repos_fs_change_rev_prop4`: a repository with no pre-revprop-change hook refuses every
/// revision property change). Do: over the svn:// listener, with revision 1 committed and no hook configured, change
/// its `svn:log` with stock `svn propset --revprop` (ra_svn's `change-rev-prop2`). Expect it refused in svnserve's words
/// (E165006) and the old value kept. Red by use (2026-11-20, Mac Studio) until the listener stopped writing a turn's
/// trivial authentication request twice: stock svn read the second as the change's answer, a success (`property
/// 'svn:log' set on repository revision 1`, exit 0), the refusal after it unread.
#[test]
fn a_revision_property_change_without_a_hook_is_refused_as_svnserve_refuses_it() {
  let instance = format!("svn-nohook-{}", std::process::id());
  let Some((_anchor, _scratch, svn, url)) = revprop_leg(&instance) else {
    return;
  };
  let refused = svn.run(
    &svn.scratch,
    &["propset", "--revprop", "-r1", "svn:log", "changed", &url],
  );
  let read = svn.ok(
    &svn.scratch,
    &["propget", "--revprop", "-r1", "svn:log", &url],
  );
  assert_eq!(
    (refused.0 != 0 && refused.2.contains("E165006"), read.trim()),
    (true, "first"),
    "{refused:?}"
  );
}

/// Shape: the measured commit's file: 500 MB, the large text main's intake measurement names (§4.17.1 "A push's
/// intake").
const LARGE_COMMIT_BYTES: usize = 500 << 20;

/// Shape: how long the measured commit may run before it is killed: a 500 MB text over loopback in a debug or release
/// daemon, with room for a slow host.
const LARGE_COMMIT_PATIENCE: Duration = Duration::from_secs(1800);

/// Writes [`LARGE_COMMIT_BYTES`] of noisy bytes, which no window shortens, as working copy `copy`'s [`FILE`], a piece at
/// a time: the test writes the file stock `svn` commits, inside the scratch directory it removes.
#[allow(clippy::disallowed_methods)]
fn put_large(copy: &str) {
  let mut file = std::fs::File::create(format!("{copy}/{FILE}")).unwrap();
  let mut state = 1u64;
  let mut piece = vec![0u8; 1 << 20];
  for _ in 0..LARGE_COMMIT_BYTES / piece.len() {
    for byte in &mut piece {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      *byte = state.to_le_bytes()[0];
    }
    file.write_all(&piece).unwrap();
  }
}

/// The resident bytes of instance `instance`'s daemon processes (every `slates` process naming the instance but its
/// anchor), from `ps`, in KiB.
fn daemon_rss_kib(instance: &str) -> u64 {
  let listed = Command::new("ps")
    .args(["-A", "-o", "rss=,command="])
    .output()
    .unwrap();
  String::from_utf8_lossy(&listed.stdout)
    .lines()
    .filter(|line| line.contains(instance) && !line.contains(" anchor"))
    .filter_map(|line| line.split_whitespace().next()?.parse::<u64>().ok())
    .sum()
}

/// Commits working copy `copy` with `svn`, sampling the daemon's resident bytes while it runs: the exit code, both
/// outputs, the seconds it took, and the most resident KiB seen.
fn commit_measured(svn: &Svn, copy: &str, instance: &str) -> (i32, String, String, f64, u64) {
  let mut child = Command::new("svn")
    .args(&svn.options)
    .args(["commit", "--message", "large"])
    .current_dir(copy)
    .env("LC_ALL", "C")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let out = read_on_a_thread(child.stdout.take().unwrap());
  let err = read_on_a_thread(child.stderr.take().unwrap());
  let started = Instant::now();
  let mut peak = 0;
  let code = loop {
    peak = peak.max(daemon_rss_kib(instance));
    if let Some(status) = child.try_wait().unwrap() {
      break status.code().unwrap_or(-1);
    }
    if started.elapsed() > LARGE_COMMIT_PATIENCE {
      let _ = child.kill();
      let _ = child.wait();
      break -2;
    }
    pause();
  };
  let seconds = started.elapsed().as_secs_f64();
  (
    code,
    out.join().unwrap(),
    err.join().unwrap(),
    seconds,
    peak,
  )
}

/// §4.17.1 "A push's intake" for Subversion (a measurement, not a gate; main's brief 2026-11-20). Do: over svn:// and
/// over the HTTP edge in turn, check out the empty repository, add a [`LARGE_COMMIT_BYTES`] file and commit it with
/// stock `svn` 1.15, sampling the daemon's resident bytes. Expect nothing: print each scheme's commit answer, seconds,
/// the daemon's resident bytes before and at its peak against the text's size, and the daemon's status lines naming
/// rooms, budgets and refusals.
#[test]
#[ignore = "a measurement: cargo test --release -p slates-cli --test svn_interop -- --ignored a_large_commit_measured --nocapture"]
fn a_large_commit_measured() {
  if !stock_svn() {
    eprintln!(
      "skipping the large commit's measurement: no stock Subversion 1.15 here (svn on the PATH)"
    );
    return;
  }
  for scheme in ["svn", "http"] {
    let instance = format!("svn-large-{scheme}-{}", std::process::id());
    let edge = format!("--vcs-{scheme}");
    let (_anchor, lines, issuer) = start_anchor(&instance, &[&edge, "127.0.0.1:0"]);
    let Some(issuer) = issuer_or_skip(issuer, "the large commit's measurement") else {
      return;
    };
    create_repository(&instance);
    let (id, token) = minted_token(&instance, &issuer);
    let (user, url) = match scheme {
      "svn" => (
        id,
        format!("svn://127.0.0.1:{}/{REPOSITORY}", svn_listener_port(&lines)),
      ),
      _ => (
        USER.to_owned(),
        format!("http://127.0.0.1:{}/{REPOSITORY}", http_edge_port(&lines)),
      ),
    };
    let scratch = Scratch::new(&format!("svn-large-{scheme}"));
    let svn = Svn::new(&scratch, "large", credentials(user, token));
    let copy = svn.copy("copy");
    svn.ok(&svn.scratch, &["checkout", &url, &copy]);
    put_large(&copy);
    svn.ok(&copy, &["add", FILE]);
    let before = daemon_rss_kib(&instance);
    let (code, out, err, seconds, peak) = commit_measured(&svn, &copy, &instance);
    let (_, status, _) = slates(&instance, &["status"]);
    let named: Vec<&str> = status
      .lines()
      .filter(|line| {
        ["room", "budget", "refus", "admit", "svn"]
          .iter()
          .any(|word| line.contains(word))
      })
      .collect();
    println!(
      "a_large_commit_measured {scheme}: exit {code} in {seconds:.1} s; daemon RSS {before} KiB before, peak {peak} KiB, \
       text {} KiB\n  svn: {}{}\n  status: {named:?}",
      LARGE_COMMIT_BYTES >> 10,
      out.trim(),
      err.trim()
    );
  }
}

/// P7 of Ada's close list (A-772's daemon peak against the dump; a measurement, not a gate). Do: start an anchor whose
/// daemon holds a changeset store, create a Subversion repository, and import the dump `SLATES_SVN_IMPORT_DUMP` names
/// through the CLI (`slates repository import`), sampling the daemon's resident bytes while it runs; then ask the daemon
/// its memory by kind (`slates status --memory`). Expect nothing: print the import's answer and seconds, the daemon's
/// resident bytes before and at its peak against the dump's bytes, and the memory report's lines. The real daemon
/// binary, so its own allocator (a large allocation a mapping of its own, given back when freed), not a test binary's.
/// With `SLATES_SVN_IMPORT_HOLD` naming a path, the daemon is held after the import for main's heap attribution (P7's
/// finding (2)): its pid written to the path, and the daemon kept until the path is deleted, at most [`PATIENCE`].
#[test]
#[ignore = "a measurement: SLATES_SVN_IMPORT_DUMP=... cargo test --release -p slates-cli --test svn_interop -- --ignored an_import_measured --nocapture"]
#[allow(clippy::disallowed_methods)] // test code: the hold's pid file, at the path the measurer names
fn an_import_measured() {
  let Ok(dump) = std::env::var("SLATES_SVN_IMPORT_DUMP") else {
    eprintln!("skipping the import's measurement: SLATES_SVN_IMPORT_DUMP names no dump");
    return;
  };
  let dump_bytes = std::fs::metadata(&dump).unwrap().len();
  let instance = format!("svn-import-{}", std::process::id());
  let (_anchor, _lines, _issuer) = start_anchor(&instance, &[]);
  create_repository(&instance);
  let before = daemon_rss_kib(&instance);
  let mut child = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args([
      "--instance",
      &instance,
      "repository",
      "import",
      REPOSITORY,
      &dump,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let out = read_on_a_thread(child.stdout.take().unwrap());
  let err = read_on_a_thread(child.stderr.take().unwrap());
  let started = Instant::now();
  let mut peak = 0;
  let code = loop {
    peak = peak.max(daemon_rss_kib(&instance));
    if let Some(status) = child.try_wait().unwrap() {
      break status.code().unwrap_or(-1);
    }
    pause();
  };
  let seconds = started.elapsed().as_secs_f64();
  let after = daemon_rss_kib(&instance);
  let (_, memory, _) = slates(&instance, &["status", "--memory"]);
  let kinds: Vec<&str> = memory
    .lines()
    .filter(|line| {
      [
        "heap", "arena", "segment", "content", "region", "memory", "budget",
      ]
      .iter()
      .any(|word| line.contains(word))
    })
    .collect();
  println!(
    "an_import_measured: exit {code} in {seconds:.1} s; dump {} KiB; daemon RSS {before} KiB before, peak {peak} KiB, \
     after {after} KiB\n  import: {}{}\n  memory: {kinds:#?}",
    dump_bytes >> 10,
    out.join().unwrap().trim(),
    err.join().unwrap().trim()
  );
  if let Ok(hold) = std::env::var("SLATES_SVN_IMPORT_HOLD") {
    let pid = daemon_pid(&instance).expect("the daemon's pid");
    std::fs::write(&hold, format!("{pid}\n")).unwrap();
    println!("an_import_measured: held, daemon pid {pid} in {hold}; delete it to end");
    let held = Instant::now();
    while std::path::Path::new(&hold).exists() && held.elapsed() < PATIENCE {
      pause();
    }
    let _ = std::fs::remove_file(&hold);
  }
}

/// The pid of instance `instance`'s daemon (not its anchor), as `ps` lists it.
fn daemon_pid(instance: &str) -> Option<u32> {
  let listed = Command::new("ps")
    .args(["-A", "-o", "pid=,command="])
    .output()
    .ok()?;
  String::from_utf8_lossy(&listed.stdout)
    .lines()
    .filter(|line| line.contains(instance) && !line.contains(" anchor"))
    .find_map(|line| line.split_whitespace().next()?.parse().ok())
}

/// P7 of Ada's close list (A-772: svnsync over the matrix took 524 s over HTTP and 1,068 s over svn://; a measurement,
/// not a gate). Do: for svn:// and the HTTP edge in turn, start an anchor whose daemon holds a changeset store, create a
/// Subversion repository and import the dump `SLATES_SVN_SYNC_DUMP` names through the CLI; then mirror it with stock
/// `svnsync` 1.15 into a local FSFS repository (`svnadmin create`, a pre-revprop-change hook allowing every change,
/// `svnsync init`, then `svnsync sync` timed), sampling the daemon's resident bytes. Expect nothing: print each scheme's
/// import seconds, the sync's answer, seconds and seconds a revision, the mirror's youngest revision, and the daemon's
/// resident bytes before and at its peak. Skips loudly without stock Subversion 1.15 or the dump.
#[test]
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // test code: the mirror's hook, in the test's own scratch directory
#[ignore = "a measurement: SLATES_SVN_SYNC_DUMP=... cargo test --release -p slates-cli --test svn_interop -- --ignored an_svnsync_measured --nocapture"]
fn an_svnsync_measured() {
  use std::os::unix::fs::PermissionsExt;
  let Ok(dump) = std::env::var("SLATES_SVN_SYNC_DUMP") else {
    eprintln!("skipping svnsync's measurement: SLATES_SVN_SYNC_DUMP names no dump");
    return;
  };
  if !stock_svn() || !stock_svnserve() {
    eprintln!(
      "skipping svnsync's measurement: no stock Subversion 1.15 here (svn, svnadmin, svnsync)"
    );
    return;
  }
  for scheme in ["svn", "http"] {
    let instance = format!("svn-sync-{scheme}-{}", std::process::id());
    let edge = format!("--vcs-{scheme}");
    let (_anchor, lines, issuer) = start_anchor(&instance, &[&edge, "127.0.0.1:0"]);
    let Some(issuer) = issuer_or_skip(issuer, "svnsync's measurement") else {
      return;
    };
    create_repository(&instance);
    let importing = Instant::now();
    let (code, out, err) = slates(&instance, &["repository", "import", REPOSITORY, &dump]);
    assert_eq!(code, 0, "repository import: {out}{err}");
    let imported = importing.elapsed().as_secs_f64();
    let (id, token) = minted_token(&instance, &issuer);
    let (user, url) = match scheme {
      "svn" => (
        id,
        format!("svn://127.0.0.1:{}/{REPOSITORY}", svn_listener_port(&lines)),
      ),
      _ => (
        USER.to_owned(),
        format!("http://127.0.0.1:{}/{REPOSITORY}", http_edge_port(&lines)),
      ),
    };
    let scratch = Scratch::new(&format!("svn-sync-{scheme}"));
    let mirror = format!("{}/mirror", scratch.path);
    let made = Command::new("svnadmin")
      .args(["create", &mirror])
      .output()
      .unwrap();
    assert!(made.status.success(), "svnadmin create: {made:?}");
    let hook = format!("{mirror}/hooks/pre-revprop-change");
    std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mirror_url = format!("file://{mirror}");
    let options = [
      "--non-interactive".to_owned(),
      "--no-auth-cache".to_owned(),
      "--config-dir".to_owned(),
      format!("{}/svn-config", scratch.path),
      "--source-username".to_owned(),
      user,
      "--source-password".to_owned(),
      token,
    ];
    let init = Command::new("svnsync")
      .args(["init", &mirror_url, &url])
      .args(&options)
      .env("LC_ALL", "C")
      .output()
      .unwrap();
    assert!(init.status.success(), "svnsync init: {init:?}");
    let before = daemon_rss_kib(&instance);
    let mut child = Command::new("svnsync")
      .args(["sync", &mirror_url])
      .args(&options)
      .env("LC_ALL", "C")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .unwrap();
    let err = read_on_a_thread(child.stderr.take().unwrap());
    let started = Instant::now();
    let mut peak = 0;
    let code = loop {
      peak = peak.max(daemon_rss_kib(&instance));
      if let Some(status) = child.try_wait().unwrap() {
        break status.code().unwrap_or(-1);
      }
      pause();
    };
    let seconds = started.elapsed().as_secs_f64();
    let youngest = Command::new("svnlook")
      .args(["youngest", &mirror])
      .output()
      .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
      .unwrap_or_default();
    let revisions = youngest.parse::<f64>().unwrap_or(0.0).max(1.0);
    println!(
      "an_svnsync_measured {scheme}: import {imported:.1} s; svnsync exit {code} in {seconds:.1} s, {:.1} ms a revision; \
       mirror at r{youngest}; daemon RSS {before} KiB before, peak {peak} KiB\n  svnsync: {}",
      seconds * 1e3 / revisions,
      err.join().unwrap().trim()
    );
  }
}

/// Format: the verbs [`a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve`] runs where
/// `SLATES_SVN_USE_VERBS` names none: every stock client verb it compares (goal 93's correctness by use, main's lane of
/// 2026-11-21).
const USE_VERBS: &[&str] = &[
  "log", "info", "checkout", "update", "switch", "blame", "diff", "revprops", "dump", "svnsync",
  "commit", "copy", "merge", "propset",
];

/// Shape: the revisions a verb that reads one revision at a time is run at, where `SLATES_SVN_USE_SAMPLES` names no
/// count: spread evenly over the history, its first and last among them, so a long history is read at its start,
/// middle and end without reading every revision.
const USE_SAMPLES: u64 = 16;

/// Shape: how long one stock tool's run of the whole history may take (an `svnsync` or an `svnrdump dump` of a large
/// dump): a guard against a server that never answers, far above what stock `svnserve` takes for git-2000.dump, never a
/// measure of either.
const USE_PATIENCE: Duration = Duration::from_secs(3600);

/// Format: what stands for each server's URL in the outputs compared, so the two servers' answers are compared, not
/// their addresses.
const USE_URL: &str = "<repository>";

/// Runs stock `program` with `args` in `dir`, killed past [`USE_PATIENCE`]: its exit code and both outputs.
fn tool(program: &str, dir: &str, args: &[&str]) -> (i32, String, String) {
  let mut child = Command::new(program)
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
  let started = Instant::now();
  let code = loop {
    if let Some(status) = child.try_wait().unwrap() {
      break status.code().unwrap_or(-1);
    }
    if started.elapsed() > USE_PATIENCE {
      let _ = child.kill();
      let _ = child.wait();
      panic!("{program} {args:?} in {dir} ran past {USE_PATIENCE:?}");
    }
    pause();
  };
  (code, out.join().unwrap(), err.join().unwrap())
}

/// One server of the comparison: its client (options and credentials) and its repository's URL.
struct UseLeg {
  name: &'static str,
  svn: Svn,
  url: String,
}

impl UseLeg {
  /// `text` with this leg's URL replaced by [`USE_URL`] and each XML tag's attributes in one order
  /// ([`canonical_tags`]).
  fn plain(&self, text: &str) -> String {
    canonical_tags(&text.replace(&self.url, USE_URL))
  }

  /// `svn args` against this leg (each `{url}` in `args` its URL), in `dir`: its standard output with the URL made
  /// plain, or its failure's words, so a verb one server refuses and the other serves shows as a difference.
  fn answer(&self, dir: &str, args: &[&str]) -> String {
    let args: Vec<String> = args
      .iter()
      .map(|arg| arg.replace("{url}", &self.url))
      .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, out, err) = self.svn.run(dir, &args);
    if code == 0 {
      self.plain(&out)
    } else {
      format!("exit {code}: {}", self.plain(&err))
    }
  }
}

/// `text` with each XML tag's attributes sorted by name and written on one line, the rest of `text` as it was: stock
/// `svn` writes a tag's attributes from a hash (`svn_xml_make_open_tag_hash`), so their order is no part of an answer
/// (found by the first run of this test: `svn log -v --xml`'s `<path>` attributes in two orders, the same four values).
fn canonical_tags(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(open) = rest.find('<') {
    out.push_str(&rest[..open]);
    let Some(close) = rest[open..].find('>') else {
      break;
    };
    let tag = &rest[open + 1..open + close];
    rest = &rest[open + close + 1..];
    let (body, empty) = match tag.strip_suffix('/') {
      Some(body) => (body, "/"),
      None => (tag, ""),
    };
    let mut words = body.split_whitespace();
    let name = words.next().unwrap_or_default();
    let mut attributes: Vec<String> = Vec::new();
    let joined: Vec<&str> = words.collect();
    let mut pending = String::new();
    for word in joined {
      if !pending.is_empty() {
        pending.push(' ');
      }
      pending.push_str(word);
      if pending.matches('"').count() % 2 == 0 {
        attributes.push(std::mem::take(&mut pending));
      }
    }
    if !pending.is_empty() {
      attributes.push(pending);
    }
    attributes.sort();
    out.push('<');
    out.push_str(name);
    for attribute in attributes {
      out.push(' ');
      out.push_str(&attribute);
    }
    out.push_str(empty);
    out.push('>');
  }
  out.push_str(rest);
  out
}

/// Every file of working copy `copy`, by its path beneath it, and its bytes; `.svn` left out.
#[allow(clippy::disallowed_methods)] // test code: reads the working copies stock `svn` wrote
fn tree_of(copy: &str) -> std::collections::BTreeMap<String, Vec<u8>> {
  let mut files = std::collections::BTreeMap::new();
  let mut stack = vec![std::path::PathBuf::from(copy)];
  while let Some(dir) = stack.pop() {
    for entry in std::fs::read_dir(&dir).unwrap() {
      let entry = entry.unwrap();
      let path = entry.path();
      if entry.file_name() == ".svn" {
        continue;
      }
      if entry.file_type().unwrap().is_dir() {
        stack.push(path);
      } else {
        let relative = path
          .strip_prefix(copy)
          .unwrap()
          .to_string_lossy()
          .into_owned();
        files.insert(relative, std::fs::read(&path).unwrap());
      }
    }
  }
  files
}

/// What `copy`, a working copy of `leg`, holds: every file's bytes and every path's properties (`svn proplist -v -R`).
fn held(leg: &UseLeg, copy: &str) -> String {
  let mut text = String::new();
  for (path, bytes) in tree_of(copy) {
    text.push_str(&format!("{path} {} {}\n", bytes.len(), fingerprint(&bytes)));
  }
  text.push_str(&leg.answer(copy, &["proplist", "-v", "-R", "--xml", "."]));
  text
}

/// A short fingerprint of `bytes`, so a tree's listing names each file's content without holding it.
fn fingerprint(bytes: &[u8]) -> String {
  use std::hash::{Hash, Hasher};
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  bytes.hash(&mut hasher);
  format!("{:016x}", hasher.finish())
}

/// Format: the lines of each side a difference is shown with, before and after its first differing line.
const USE_CONTEXT: usize = 3;

/// The first line where `control` and `slates` differ, with its number and [`USE_CONTEXT`] lines of each side around
/// it, for a failure's words and the red test it becomes.
fn first_difference(control: &str, slates: &str) -> String {
  let (a, b): (Vec<&str>, Vec<&str>) = (control.lines().collect(), slates.lines().collect());
  let Some(at) = (0..a.len().max(b.len())).find(|line| a.get(*line) != b.get(*line)) else {
    return "none".to_owned();
  };
  let from = at.saturating_sub(USE_CONTEXT);
  let window = |side: &[&str]| {
    side
      .iter()
      .skip(from)
      .take(2 * USE_CONTEXT + 1)
      .copied()
      .collect::<Vec<_>>()
      .join("\n      ")
  };
  format!(
    "line {}:\n    stock:\n      {}\n    slates:\n      {}",
    at + 1,
    window(&a),
    window(&b)
  )
}

/// Collects `verb`'s difference between the two legs' answers `control` and `slates`, at `at`, into `failed`.
fn compare(failed: &mut Vec<String>, verb: &str, at: &str, control: &str, slates: &str) {
  if control != slates {
    failed.push(format!(
      "{verb} {at}: {}",
      first_difference(control, slates)
    ));
  }
}

/// Goal 93's Subversion correctness by use (main's lane of 2026-11-21; ignored: main runs its rounds, CLAUDE.md §8).
/// Do: load the dump `SLATES_SVN_USE_DUMP` names into a stock FSFS repository (`svnadmin load`) served by stock
/// `svnserve -t`, cut to its first `SLATES_SVN_USE_REVISIONS` revisions where that is set (`svnadmin dump -r`), and
/// import the same revisions into a slates repository (`slates repository import`) served by the daemon's svn://
/// listener; then run each verb `SLATES_SVN_USE_VERBS` names (default [`USE_VERBS`]) as stock `svn` 1.15 against both,
/// reading verbs at [`USE_SAMPLES`] revisions spread over the history (`SLATES_SVN_USE_SAMPLES`), and the writing verbs
/// (commit, copy, merge, propset of a revision property) as the same edits on both, then their logs. Expect every
/// answer byte for byte alike once each server's URL is made plain ([`USE_URL`]; a written revision's author and date,
/// which the servers stamp, left out): log -v, info, a checkout's files and properties, `update -r`, `switch`, blame,
/// `diff -c`, revision properties, `svnrdump dump`, and an `svnsync` mirror's `svnadmin dump`. A failure names each verb,
/// its revision and its first differing line. Skips loudly without stock Subversion 1.15 (`svn`, `svnadmin`,
/// `svnserve`, `svnrdump`, `svnsync`), the dump, or an issuer surface.
#[test]
#[ignore = "correctness by use, main's rounds: SLATES_SVN_USE_DUMP=git-2000.dump [SLATES_SVN_USE_REVISIONS=N] [SLATES_SVN_USE_VERBS=log,diff] cargo test --release -p slates-cli --test svn_interop -- --ignored a_dump_reads_and_writes_alike --nocapture"]
#[allow(clippy::disallowed_methods)] // test code: the control's repository, hook and the cut dump, in the scratch
fn a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve() {
  let Ok(dump) = std::env::var("SLATES_SVN_USE_DUMP") else {
    eprintln!("skipping svn correctness by use: SLATES_SVN_USE_DUMP names no dump");
    return;
  };
  let tools_here = stock_svn()
    && stock_svnserve()
    && ["svnrdump", "svnsync"].iter().all(|tool| {
      Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    });
  if !tools_here {
    eprintln!(
      "skipping svn correctness by use: no stock Subversion 1.15 here (svn, svnadmin, svnserve, svnrdump, svnsync)"
    );
    return;
  }
  let verbs: Vec<String> = std::env::var("SLATES_SVN_USE_VERBS").map_or_else(
    |_| USE_VERBS.iter().map(|verb| (*verb).to_owned()).collect(),
    |named| named.split(',').map(str::to_owned).collect(),
  );
  let samples: u64 = std::env::var("SLATES_SVN_USE_SAMPLES")
    .ok()
    .and_then(|count| count.parse().ok())
    .unwrap_or(USE_SAMPLES)
    .max(1);
  let scratch = Scratch::new("svn-use");
  // The control: a stock FSFS repository loaded with the dump, its revision property changes allowed by a hook.
  let root = format!("{}/repositories", scratch.path);
  made_directory(&root);
  let control_repository = format!("{root}/control");
  let made = tool("svnadmin", &scratch.path, &["create", &control_repository]);
  assert_eq!(made.0, 0, "svnadmin create: {made:?}");
  let loaded = Command::new("svnadmin")
    .args(["load", "-q", &control_repository])
    .stdin(std::fs::File::open(&dump).unwrap())
    .output()
    .unwrap();
  assert!(loaded.status.success(), "svnadmin load: {loaded:?}");
  let cut = format!("{}/cut.dump", scratch.path);
  let mut range = vec!["dump", "-q", control_repository.as_str()];
  let revisions = std::env::var("SLATES_SVN_USE_REVISIONS").ok();
  let upper = revisions.as_ref().map(|n| format!("0:{n}"));
  if let Some(upper) = upper.as_deref() {
    range.extend(["-r", upper]);
  }
  let dumped = Command::new("svnadmin").args(&range).output().unwrap();
  assert!(dumped.status.success(), "svnadmin dump: {dumped:?}");
  std::fs::write(&cut, &dumped.stdout).unwrap();
  if revisions.is_some() {
    let again = format!("{root}/control-cut");
    let made = tool("svnadmin", &scratch.path, &["create", &again]);
    assert_eq!(made.0, 0, "svnadmin create: {made:?}");
    let loaded = Command::new("svnadmin")
      .args(["load", "-q", &again])
      .stdin(std::fs::File::open(&cut).unwrap())
      .output()
      .unwrap();
    assert!(
      loaded.status.success(),
      "svnadmin load of the cut: {loaded:?}"
    );
    std::fs::remove_dir_all(&control_repository).unwrap();
    std::fs::rename(&again, &control_repository).unwrap();
  }
  let hook = format!("{control_repository}/hooks/pre-revprop-change");
  std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
  }
  let control = UseLeg {
    name: "stock svnserve",
    svn: Svn::new(
      &scratch,
      "control",
      vec![
        "--config-option".to_owned(),
        format!(
          "config:tunnels:{CONTROL_TUNNEL}=sh -c 'exec svnserve -t -r {root} --tunnel-user={TUNNEL_USER}'"
        ),
      ],
    ),
    url: format!("svn+{CONTROL_TUNNEL}://localhost/control"),
  };
  // Slates: the same revisions imported, served by the svn:// listener.
  let instance = format!("svn-use-{}", std::process::id());
  let (_anchor, lines, issuer) = start_anchor(&instance, &["--vcs-svn", "127.0.0.1:0"]);
  let port = svn_listener_port(&lines);
  let Some(issuer) = issuer_or_skip(issuer, "svn correctness by use") else {
    return;
  };
  create_repository(&instance);
  let imported = slates(&instance, &["repository", "import", REPOSITORY, &cut]);
  assert_eq!(imported.0, 0, "import: {imported:?}");
  let (id, token) = minted_token(&instance, &issuer);
  let allowing = format!("{}/allow-revprop-change", scratch.path);
  std::fs::write(&allowing, "#!/bin/sh\nexit 0\n").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&allowing, std::fs::Permissions::from_mode(0o755)).unwrap();
  }
  let given = format!("pre-revprop-change={allowing}");
  let policy = slates(
    &instance,
    &["repository", "policy", REPOSITORY, "--hook", &given],
  );
  assert_eq!(policy.0, 0, "policy: {policy:?}");
  let slates_leg = UseLeg {
    name: "slates",
    svn: Svn::new(&scratch, "slates", credentials(id, token)),
    url: format!("svn://127.0.0.1:{port}/{REPOSITORY}"),
  };
  let legs = [&control, &slates_leg];
  let youngest: u64 = control
    .answer(&scratch.path, &["info", "--show-item", "revision", "{url}"])
    .trim()
    .parse()
    .unwrap();
  let stride = youngest.div_ceil(samples).max(1);
  let mut sampled: Vec<u64> = (1..=youngest)
    .step_by(usize::try_from(stride).unwrap())
    .collect();
  if sampled.last() != Some(&youngest) && youngest > 0 {
    sampled.push(youngest);
  }
  let mut failed = Vec::new();
  let mut ran = Vec::new();
  for verb in &verbs {
    ran.push(verb.clone());
    match verb.as_str() {
      "log" => {
        let args = ["log", "-v", "--xml", "-r", "0:HEAD", "{url}"];
        let [a, b] = legs.map(|leg| leg.answer(&scratch.path, &args));
        compare(&mut failed, verb, "0:HEAD", &a, &b);
      }
      "info" => {
        for at in &sampled {
          let target = format!("{{url}}@{at}");
          let args = ["info", "-R", "--xml", target.as_str()];
          let [a, b] = legs.map(|leg| leg.answer(&scratch.path, &args));
          compare(&mut failed, verb, &format!("r{at}"), &a, &b);
        }
      }
      "checkout" | "update" | "switch" | "blame" => {
        for at in &sampled {
          let rev = at.to_string();
          let mut held_by = Vec::new();
          for leg in legs {
            let copy = leg.svn.copy(&format!("{verb}-{at}"));
            match verb.as_str() {
              "checkout" | "blame" => {
                leg.answer(
                  &scratch.path,
                  &["checkout", "-q", "-r", &rev, "{url}", &copy],
                );
              }
              "update" => {
                leg.answer(&scratch.path, &["checkout", "-q", "{url}", &copy]);
                leg.answer(&copy, &["update", "-q", "-r", &rev]);
              }
              _ => {
                // A switch from the youngest root to revision `at`'s root, as a working copy moved between URLs.
                leg.answer(
                  &scratch.path,
                  &["checkout", "-q", "--depth", "empty", "{url}", &copy],
                );
                leg.answer(
                  &copy,
                  &["switch", "-q", "--ignore-ancestry", "-r", &rev, "{url}"],
                );
                leg.answer(
                  &copy,
                  &["update", "-q", "--set-depth", "infinity", "-r", &rev],
                );
              }
            }
            held_by.push(if verb == "blame" {
              let files: Vec<String> = tree_of(&copy).into_keys().collect();
              files
                .iter()
                .map(|file| leg.answer(&copy, &["blame", "-v", "-r", &rev, file]))
                .collect::<Vec<_>>()
                .join("\n")
            } else {
              held(leg, &copy)
            });
            let _ = std::fs::remove_dir_all(&copy);
          }
          compare(
            &mut failed,
            verb,
            &format!("r{at}"),
            &held_by[0],
            &held_by[1],
          );
        }
      }
      "diff" => {
        for at in &sampled {
          let change = at.to_string();
          let args = ["diff", "-c", change.as_str(), "{url}"];
          let [a, b] = legs.map(|leg| leg.answer(&scratch.path, &args));
          compare(&mut failed, verb, &format!("r{at}"), &a, &b);
        }
      }
      "revprops" => {
        for at in &sampled {
          let rev = at.to_string();
          let args = [
            "proplist",
            "--revprop",
            "-v",
            "--xml",
            "-r",
            rev.as_str(),
            "{url}",
          ];
          let [a, b] = legs.map(|leg| leg.answer(&scratch.path, &args));
          compare(&mut failed, verb, &format!("r{at}"), &a, &b);
        }
      }
      "dump" => {
        let [a, b] = legs.map(|leg| {
          let mut args = leg.svn.options.clone();
          args.extend(["dump".to_owned(), "-q".to_owned(), leg.url.clone()]);
          let args: Vec<&str> = args.iter().map(String::as_str).collect();
          let (code, out, err) = tool("svnrdump", &scratch.path, &args);
          if code == 0 {
            out
          } else {
            format!("exit {code}: {}", leg.plain(&err))
          }
        });
        compare(&mut failed, "svnrdump dump", "0:HEAD", &a, &b);
      }
      "svnsync" => {
        let [a, b] = legs.map(|leg| {
          let mirror = format!("{root}/mirror-of-{}", leg.svn.session);
          tool("svnadmin", &scratch.path, &["create", &mirror]);
          let hook = format!("{mirror}/hooks/pre-revprop-change");
          std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
          #[cfg(unix)]
          {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
          }
          let mirror_url = format!("file://{mirror}");
          let mut options = leg.svn.options.clone();
          options.push("--allow-non-empty".to_owned());
          let init: Vec<String> = ["init".to_owned(), mirror_url.clone(), leg.url.clone()]
            .into_iter()
            .chain(leg.svn.options.iter().cloned())
            .collect();
          let init: Vec<&str> = init.iter().map(String::as_str).collect();
          let (code, out, err) = tool("svnsync", &scratch.path, &init);
          if code != 0 {
            return format!("init exit {code}: {}{}", out, leg.plain(&err));
          }
          let sync: Vec<String> = ["sync".to_owned(), mirror_url]
            .into_iter()
            .chain(leg.svn.options.iter().cloned())
            .collect();
          let sync: Vec<&str> = sync.iter().map(String::as_str).collect();
          let (code, _, err) = tool("svnsync", &scratch.path, &sync);
          if code != 0 {
            return format!("sync exit {code}: {}", leg.plain(&err));
          }
          // The mirror's own sync properties name its source: left out, as each names its own server.
          let (_, out, _) = tool("svnadmin", &scratch.path, &["dump", "-q", &mirror]);
          out
            .lines()
            .filter(|line| !line.contains("svn:sync-"))
            .collect::<Vec<_>>()
            .join("\n")
        });
        compare(&mut failed, "svnsync", "0:HEAD", &a, &b);
      }
      "commit" | "copy" | "merge" | "propset" => {}
      other => failed.push(format!("{other}: no such verb in the comparison")),
    }
  }
  // The writing verbs, as one session of edits on each server, then each written revision's log, the servers' own
  // stamps (author, date) left out.
  let writes: Vec<&str> = ["commit", "copy", "merge", "propset"]
    .into_iter()
    .filter(|verb| verbs.iter().any(|named| named == verb))
    .collect();
  if !writes.is_empty() {
    let logs = legs.map(|leg| {
      let copy = leg.svn.copy("writes");
      leg.answer(
        &scratch.path,
        &["checkout", "-q", "--depth", "immediates", "{url}", &copy],
      );
      let mut said = String::new();
      if writes.contains(&"commit") {
        std::fs::write(format!("{copy}/use-written"), "written by use\n").unwrap();
        said.push_str(&leg.answer(&copy, &["add", "-q", "use-written"]));
        said.push_str(&leg.answer(&copy, &["commit", "-q", "-m", "by use: a commit"]));
      }
      if writes.contains(&"copy") {
        said.push_str(&leg.answer(
          &copy,
          &[
            "copy",
            "-q",
            "-m",
            "by use: a copy",
            "{url}",
            "{url}/use-branch",
          ],
        ));
      }
      if writes.contains(&"merge") {
        said.push_str(&leg.answer(&copy, &["update", "-q"]));
        std::fs::write(format!("{copy}/use-merged"), "merged by use\n").unwrap();
        said.push_str(&leg.answer(&copy, &["add", "-q", "use-merged"]));
        said.push_str(&leg.answer(&copy, &["commit", "-q", "-m", "by use: to merge"]));
        said.push_str(&leg.answer(&copy, &["update", "-q"]));
        said.push_str(&leg.answer(&copy, &["merge", "-q", "-c", "HEAD", "{url}", "."]));
        said.push_str(&leg.answer(&copy, &["status"]));
        said.push_str(&leg.answer(&copy, &["revert", "-q", "-R", "."]));
      }
      if writes.contains(&"propset") {
        said.push_str(&leg.answer(
          &scratch.path,
          &[
            "propset",
            "--revprop",
            "-r",
            "HEAD",
            "svn:log",
            "by use: a changed log",
            "{url}",
          ],
        ));
      }
      let log = leg.answer(&scratch.path, &["log", "-v", "--xml", "-l", "4", "{url}"]);
      let unstamped: String = log
        .lines()
        .filter(|line| !line.contains("<author>") && !line.contains("<date>"))
        .collect::<Vec<_>>()
        .join("\n");
      format!("{said}\n{unstamped}")
    });
    compare(&mut failed, &writes.join("+"), "HEAD", &logs[0], &logs[1]);
  }
  println!(
    "svn correctness by use: {} revisions, sampled {:?}; verbs {ran:?}; {} differences between {} and {}",
    youngest,
    sampled,
    failed.len(),
    control.name,
    slates_leg.name
  );
  assert!(failed.is_empty(), "differences:\n{}", failed.join("\n"));
}

/// The comparison's own rule ([`canonical_tags`]). Do: put the two attribute orders the first run of
/// [`a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve`] met for one `<path>` through it, and a value with
/// a space. Expect one text for both orders, the value whole, and text outside tags as it was.
#[test]
fn an_xml_tags_attribute_order_is_no_part_of_an_answer() {
  let stock = "<paths>\n<path\n   action=\"A\"\n   prop-mods=\"false\"\n   text-mods=\"false\"\n   kind=\"dir\">/branches</path>";
  let slates = "<paths>\n<path\n   prop-mods=\"false\"\n   text-mods=\"false\"\n   kind=\"dir\"\n   action=\"A\">/branches</path>";
  assert_eq!(canonical_tags(stock), canonical_tags(slates));
  assert_eq!(
    canonical_tags("x <e b=\"two words\" a=\"1\"/> y"),
    "x <e a=\"1\" b=\"two words\"/> y"
  );
}
