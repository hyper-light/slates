//! Stock Mercurial 7.2 against a real `slates anchor` and its daemon (UI goal 93; §4.17.3): stock hg clones, pushes and
//! pulls a repository over each route slates serves Mercurial on, the daemon's HTTP edge (`--vcs-http`, A-680) and its
//! SSH edge (`--vcs-ssh`, A-679, A-684, A-696), and is refused in its own words where it brings no credential.
//!
//! Moved here from `tests/cli.rs` (2026-11-20, main's "all functionality, every platform"), whose Unix-only gate kept
//! these legs off Windows though nothing in them is Unix's: on jupiter stock hg is Mercurial 7.2 run by `uvx`
//! (`SLATES_TEST_HG="uvx --quiet --python 3.12 --from mercurial==7.2 hg"`, as on the Mac Studio), and `ssh` the OpenSSH
//! the PATH names first. The harness is `tests/git_interop.rs`'s: the anchor started and its lines read until its
//! daemon serves, a credential minted with the anchor's issuer surface (the segment's name on macOS and Windows, the
//! issuer route and its secret on Linux, A-754), and each test's copies and
//! key in a scratch directory in the build output, named with the process id and removed at its end, its paths
//! forward-slashed. A host without stock hg 7.2, or without `ssh-keygen` for an SSH leg, skips loudly.
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
  _anchor: Anchor,
  /// The daemon's changeset store (`--vcs-store`), which a Mercurial repository needs (main's ruling of 2026-11-20:
  /// a daemon without one refuses to create one); declared after the anchor, so the daemon is gone before it is
  /// removed.
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
/// consensus group as a first start does (`slates bootstrap root`). The daemon holds a changeset store in a fresh
/// scratch directory (`--vcs-store`), as every daemon serving Mercurial must.
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
    _anchor: anchor,
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

/// Shape: the variable naming the command that runs stock Mercurial 7.2, its words split at spaces: on the Mac Studio
/// `uvx --quiet --python 3.12 --from mercurial==7.2 hg`, as `crates/hg/fixtures/make-fixtures.sh` runs it. Unset, `hg`
/// on the path.
const STOCK_HG: &str = "SLATES_TEST_HG";

/// Format: the user name stock hg gives, over SSH and as Basic's user name. It proves nothing: the key the test
/// enrolled proves the principal over SSH, and the token, Basic's password, over HTTP.
const HG_USER: &str = "slates";

/// Stock Mercurial 7.2's command, or `None` when this host runs none, which the caller skips loudly: the words
/// [`STOCK_HG`] names, answering `--version` with 7.2's line.
fn stock_hg() -> Option<Vec<String>> {
  let line = std::env::var(STOCK_HG).unwrap_or_else(|_| "hg".to_owned());
  let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
  let (program, leading) = words.split_first()?;
  let version = Command::new(program)
    .args(leading)
    .arg("--version")
    .env("HGPLAIN", "1")
    .env("HGRCPATH", "")
    .output()
    .ok()?;
  let text = String::from_utf8_lossy(&version.stdout);
  (version.status.success() && text.contains("(version 7.2")).then_some(words)
}

/// Runs stock hg with `args` in `dir`, outside any user's configuration (`HGRCPATH` empty), in its plain words
/// (`HGPLAIN`), committing as one test user, with no terminal to prompt on, killed past [`PATIENCE`]: its exit code and
/// both outputs.
fn hg_in(hg: &[String], dir: &str, args: &[&str]) -> (i32, String, String) {
  let (program, leading) = hg.split_first().expect("a command");
  let mut child = Command::new(program)
    .args(leading)
    .args(args)
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
  let code = exit_within(&mut child, &format!("hg {args:?} in {dir}"));
  (code, out.join().unwrap(), err.join().unwrap())
}

/// Runs stock hg as [`hg_in`] does, failing the test unless it succeeds: its standard output.
fn hg_ok(hg: &[String], dir: &str, args: &[&str]) -> String {
  let (code, out, err) = hg_in(hg, dir, args);
  assert_eq!(code, 0, "hg {args:?} in {dir}: {out}{err}");
  out
}

/// `args`, then the `--config` words `config` holds.
fn with_config<'a>(args: &[&'a str], config: &'a [String]) -> Vec<&'a str> {
  args
    .iter()
    .copied()
    .chain(config.iter().map(String::as_str))
    .collect()
}

/// The changesets of the working copy at `dir`, newest first.
fn hg_nodes(hg: &[String], dir: &str) -> Vec<String> {
  hg_ok(hg, dir, &["log", "--template", "{node}\\n"])
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Makes a Mercurial repository `name` through the CLI: `slates repository create <name> --tool hg`, A-685 (g305),
/// `CreateRepository`'s fields as its flags (main's ruling, 2026-11-19). Until it lands no client verb sends
/// `CreateRepository`: the server's tests and the Subversion migration alone do.
fn create_hg_repository(instance: &str, name: &str) {
  let (code, out, err) = slates(
    instance,
    &[],
    &["repository", "create", name, "--tool", "hg"],
  );
  assert_eq!(code, 0, "repository create: {out}{err}");
}

/// Stock hg's round trip through slates at `url`, `config` its `--config` words (credentials, the SSH command): a
/// history made in a fresh working copy and pushed, the repository's first; cloned into a second copy; a changeset made
/// there and pushed; pulled back into the first. Fails the test unless every step succeeds and both copies end with the
/// same two changesets.
#[allow(clippy::disallowed_methods)]
fn hg_round_trip(hg: &[String], scratch: &str, url: &str, config: &[String]) {
  let (local, clone) = (format!("{scratch}/local"), format!("{scratch}/clone"));
  hg_ok(hg, scratch, &["init", &local]);
  std::fs::write(format!("{local}/file"), "first\n").unwrap();
  hg_ok(hg, &local, &["commit", "--addremove", "--message", "first"]);
  hg_ok(hg, &local, &with_config(&["push", url], config));
  hg_ok(hg, scratch, &with_config(&["clone", url, &clone], config));
  assert_eq!(
    hg_nodes(hg, &clone),
    hg_nodes(hg, &local),
    "the clone holds what was pushed"
  );
  std::fs::write(format!("{clone}/file"), "first\nsecond\n").unwrap();
  hg_ok(hg, &clone, &["commit", "--message", "second"]);
  hg_ok(hg, &clone, &with_config(&["push"], config));
  hg_ok(hg, &local, &with_config(&["pull", url], config));
  let nodes = hg_nodes(hg, &local);
  assert_eq!(
    (nodes.len(), &nodes),
    (2, &hg_nodes(hg, &clone)),
    "the pull brought the clone's push back"
  );
  // `identify` asks the server's `lookup` (stock 7.2 aborted "remote repository does not support the 'lookup'
  // capability" before g258's fix): the tip it names is the one both copies hold, as its short id.
  let identified = hg_ok(
    hg,
    scratch,
    &with_config(&["identify", "-r", "tip", url], config),
  );
  assert_eq!(
    Some(identified.trim()),
    nodes.first().and_then(|tip| tip.get(..12)),
    "identify named the tip"
  );
}

/// A-650 part 1, A-680 part 2a and A-686 (§4.17.3 "Access"; hgweb answers a client with no credentials `401` with a
/// Basic challenge, which 7.2's client reads). Do: start an anchor with its HTTP edge on loopback; ask stock Mercurial
/// 7.2, given no credentials and no terminal, to clone a repository there. Expect it refused in its own words (`abort:
/// http authorization required for`, `url.py` lines 92-93, exit 255), never with its crash report. Skips loudly without
/// stock hg 7.2 ([`STOCK_HG`]). Its waits, as they ended: the daemon's edge served git's site alone, so hg's request
/// met `404` (`abort: HTTP Error 404: Not Found`; Mac Studio, kiln r174), until A-680 part 2a put Mercurial's site
/// beside it; that site's `401` offered Bearer alone, which crashed 7.2 ("** unknown exception encountered",
/// `ValueError: AbstractDigestAuthHandler does not support the following scheme: 'Bearer'`, raised at Python 3.12's
/// `urllib/request.py` line 1129 through hg's `url.py` line 418; exit 1), since urllib's digest handler reads the first
/// challenge alone, until A-686 put Basic's challenge first on every connection.
#[test]
fn stock_hg_without_credentials_is_refused_in_its_own_words() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's refusal: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-refused-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let scratch = Scratch::new("hg-refused");
  let url = format!("http://127.0.0.1:{port}/r");
  let (code, out, err) = hg_in(&hg, &scratch.path, &["clone", &url, "clone"]);
  assert!(
    !err.contains("unknown exception"),
    "stock hg crashed on the edge's refusal: {err}"
  );
  assert_eq!(code, 255, "{out}{err}");
  assert!(
    err.contains("abort: http authorization required for"),
    "{err}"
  );
}

/// A-650 part 1 and A-686 (§4.17.3 "Access": Basic with a token as the password, over TLS or the loopback edge, main's
/// ruling of 2026-11-19). Do: start an anchor with its HTTP edge on loopback (`--vcs-http 127.0.0.1:0`); make a
/// Mercurial repository through the CLI ([`create_hg_repository`]) and a token (`credential token`, the issuer's verb,
/// run with the anchor's issuer surface: [`start_anchor`]); with the token as Basic's password
/// (`auth.*`), push a fresh history to the repository, clone it, push a changeset from the clone and pull it back
/// ([`hg_round_trip`]). Expect each step done and both copies holding the same two changesets. Skips loudly without
/// stock hg 7.2 ([`STOCK_HG`]), and where the platform names no issuer surface ([`issuer_of`]). Everything
/// it needs is built: `slates repository create` (A-685), the hg site on the daemon's edge (A-680 part 2a), its pushes
/// (`unbundle` through `hg_push`, A-680 part 2b) and the edge's Basic, its challenge first and taken on the loopback
/// edge (A-686, [`stock_hg_without_credentials_is_refused_in_its_own_words`]).
#[test]
fn stock_hg_clones_pulls_and_pushes_over_the_vcs_http_edge() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg over the HTTP edge: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-http-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "stock hg over the HTTP edge") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let scratch = Scratch::new("hg-http");
  hg_round_trip(&hg, &scratch.path, &format!("{prefix}r"), &config);
}

/// Format: stock 7.2's warning where `clone --stream` meets a server listing no stream capability
/// (`mercurial/streamclone.py`, `canperformstreamclone`), before it clones plainly.
const STREAM_DISABLED_WARNING: &str =
  "warning: stream clone requested but server has them disabled";

/// The value of counter `name` in the daemon's status report (`slates status --json`, each counter an object
/// `{"count":N,"kind":"name"}`, one a shard that counted it): their sum, zero where none counted.
fn counter(instance: &str, name: &str) -> u64 {
  let (code, status, err) = slates(instance, &[], &["status", "--json"]);
  assert_eq!(code, 0, "status: {err}");
  let kind = format!("\"kind\":\"{name}\"");
  status
    .match_indices(&kind)
    .filter_map(|(at, _)| {
      let object = &status[status[..at].rfind('{')?..at];
      let count = &object[object.find("\"count\":")? + "\"count\":".len()..];
      count
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse::<u64>()
        .ok()
    })
    .sum()
}

/// Format: the daemon's counter of the slices a push's intake took (`repository::hg_push::INTAKE_SLICES`).
const INTAKE_SLICES: &str = "hg_push.intake_slices";

/// Shape: the most slices the test's one push of two changesets may take paced: the intake's steps run on while no one
/// waits on the shard, so its few units take a slice or two, where a slice a step took one each.
const INTAKE_SLICES_AT_MOST: u64 = 3;

/// Format: the daemon's counter of the turns a fetch's streamed answer took (`vcs_store::hg_view::STREAM_TURNS`).
const STREAM_TURNS: &str = "vcs_store.hg_stream_turns";

/// Format: the daemon's counter of the asks a fetch's answer made of the changeset store, each a batch of rows or of
/// chunks (`crate::vcs_store::hg_view::STORE_ASKS` in `slates-server`): a stream's, and since A-767 C1 step 1a a plain
/// clone's file section's.
const STORE_ASKS: &str = "vcs_store.hg_stream_store_asks";

/// Shape: the most turns the pushed repository's stream clone may take: its store asks end a turn each (a pass's rows,
/// the chunks) and the last turn ends the answer. A turn that yielded at its first quantum took one entry or one
/// chunk, so this repository's stream was a turn a step before A-783's pacing reached the edge.
const STREAM_TURNS_AT_MOST: u64 = 6;

/// Format: stock 7.2's line as a stream clone begins (`mercurial/streamclone.py`, `maybeperformlegacystreamclone`).
const STREAMING_LINE: &str = "streaming all changes";

/// Goal 93 perf (main's ruling of 2026-11-21 02:31) and A-767 C1 step 3. A stream clone sends each revision as the
/// changeset store keeps it, and since C1 step 3 an import lands on the store as segment changesets, each revision its
/// revlog's own delta, as a push's does; so an imported repository streams as a pushed one does (before, it listed no
/// stream capability and stock 7.2 cloned it plainly with its warning). Do: over the HTTP edge, import a two-changeset
/// history (an empty file among its files) into one repository (`slates repository import`) and push the same history
/// into another; `hg clone --stream` each. Expect both clones to stream, with no warning, each holding the source's
/// changesets and passing `hg verify` without a warning (the empty file's log streamed split left its `.d` out of the
/// client's fncache, two warnings on hg-src's prefix); and the pushed one's stream taking at most
/// [`STREAM_TURNS_AT_MOST`] turns (A-783's pacing: a turn a step, about 460,000 for hg-src, took 15.8 s). Then a plain
/// `hg clone` of the pushed one (A-767 C1 step 1a): expect it to hold the source's changesets and verify, its file
/// revisions read from the changeset store ([`STORE_ASKS`] counted). Skips loudly without stock hg 7.2 ([`STOCK_HG`]) or
/// an issuer surface.
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_clone_stream_streams_a_pushed_repository_and_an_imported_one() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's stream clone of an imported repository: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-stream-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "stock hg's stream clone") else {
    return;
  };
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let scratch = Scratch::new("hg-stream");
  let source = format!("{}/source", scratch.path);
  hg_ok(&hg, &scratch.path, &["init", &source]);
  std::fs::write(format!("{source}/file"), "first\n").unwrap();
  // An empty file too: 7.2 sends its revision against the null base as an empty delta, no hunk (hg-src's
  // `mercurial/__init__.py` at its first revision; a stream refused it, the answer's body left empty).
  std::fs::write(format!("{source}/empty"), "").unwrap();
  hg_ok(
    &hg,
    &source,
    &["commit", "--addremove", "--message", "first"],
  );
  std::fs::write(format!("{source}/file"), "first\nsecond\n").unwrap();
  hg_ok(&hg, &source, &["commit", "--message", "second"]);
  create_hg_repository(&instance, "imported");
  let (code, out, err) = slates(
    &instance,
    &[],
    &["repository", "import", "imported", &source],
  );
  assert_eq!(code, 0, "repository import: {out}{err}");
  create_hg_repository(&instance, "pushed");
  hg_ok(
    &hg,
    &source,
    &with_config(&["push", &format!("{prefix}pushed")], &config),
  );
  let clone_stream = |name: &str| {
    let clone = format!("{}/{name}-clone", scratch.path);
    let url = format!("{prefix}{name}");
    let (code, out, err) = hg_in(
      &hg,
      &scratch.path,
      &with_config(&["clone", "--stream", "-U", &url, &clone], &config),
    );
    assert_eq!(code, 0, "clone --stream of {name}: {out}{err}");
    let said = format!("{out}{err}");
    let (verified, verify_out, verify_err) = hg_in(&hg, &clone, &["verify"]);
    let verify_said = format!("{verify_out}{verify_err}");
    (
      said.contains(STREAM_DISABLED_WARNING),
      said.contains(STREAMING_LINE),
      hg_nodes(&hg, &clone) == hg_nodes(&hg, &source),
      (verified == 0 && !verify_said.contains("warning"))
        .then_some(())
        .ok_or(verify_said),
    )
  };
  // The push's intake was paced (A-783): a few slices, where a slice a step took one each of its units.
  let slices = counter(&instance, INTAKE_SLICES);
  assert!(
    (1..=INTAKE_SLICES_AT_MOST).contains(&slices),
    "the push's intake took {slices} slices"
  );
  let imported = clone_stream("imported");
  let turns_before = counter(&instance, STREAM_TURNS);
  let pushed = clone_stream("pushed");
  let turns = counter(&instance, STREAM_TURNS).saturating_sub(turns_before);
  assert_eq!(
    (imported, pushed),
    ((false, true, true, Ok(())), (false, true, true, Ok(()))),
    "(warned, streamed, whole, verified clean) for the imported repository's clone, then the pushed one's"
  );
  assert!(
    (1..=STREAM_TURNS_AT_MOST).contains(&turns),
    "the pushed repository's stream took {turns} turns"
  );
  // A-767 C1 step 1a (main's ruling of 2026-11-21 08:45): a plain clone's file revisions come from the changeset store
  // as it keeps them, asked between turns, never a whole text from the content store.
  let asks_before = counter(&instance, STORE_ASKS);
  let plain = format!("{}/pushed-plain", scratch.path);
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(
      &["clone", "-U", &format!("{prefix}pushed"), &plain],
      &config,
    ),
  );
  let asks = counter(&instance, STORE_ASKS).saturating_sub(asks_before);
  let (verified, out, err) = hg_in(&hg, &plain, &["verify"]);
  assert_eq!(
    (verified, hg_nodes(&hg, &plain) == hg_nodes(&hg, &source)),
    (0, true),
    "the plain clone holds the source's changesets and verifies: {out}{err}"
  );
  assert!(
    asks > 0,
    "the plain clone asked the changeset store {asks} times"
  );
}

/// §4.17.3 decision 1 (a client's `--force` makes a line; main's item 6, reading (a), held to stock 7.2's `push -f`). Do:
/// over the HTTP edge, push a first changeset from copy A; clone it into B, commit and push there, so the line moves;
/// commit in A on the old head and push without `--force`, then with it; clone the repository again into C and pull
/// into B. Expect the plain push refused in 7.2's own words (`abort: push creates new remote head`, exit 255) with
/// nothing landed; the forced one landed; C holding three changesets and two heads on `default`, A's and B's; and B's
/// pull bringing A's head. Skips loudly without stock hg 7.2 ([`STOCK_HG`]) or an issuer surface ([`issuer_of`]).
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_forces_a_new_head_on_a_moved_line_and_every_client_sees_both() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's forced new head: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-force-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "stock hg's forced new head") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let url = format!("{prefix}r");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let scratch = Scratch::new("hg-force");
  let at = |name: &str| format!("{}/{name}", scratch.path);
  let (a, b, c) = (at("a"), at("b"), at("c"));
  hg_ok(&hg, &scratch.path, &["init", &a]);
  std::fs::write(format!("{a}/file"), "first\n").unwrap();
  hg_ok(&hg, &a, &["commit", "--addremove", "--message", "first"]);
  hg_ok(&hg, &a, &with_config(&["push", &url], &config));
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(&["clone", &url, &b], &config),
  );
  std::fs::write(format!("{b}/file"), "first\nfrom b\n").unwrap();
  hg_ok(&hg, &b, &["commit", "--message", "from b"]);
  hg_ok(&hg, &b, &with_config(&["push"], &config));
  std::fs::write(format!("{a}/other"), "from a\n").unwrap();
  hg_ok(&hg, &a, &["commit", "--addremove", "--message", "from a"]);
  let (refused, out, err) = hg_in(&hg, &a, &with_config(&["push", &url], &config));
  assert_eq!(refused, 255, "{out}{err}");
  assert!(err.contains("abort: push creates new remote head"), "{err}");
  hg_ok(&hg, &a, &with_config(&["push", "--force", &url], &config));
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(&["clone", "-U", &url, &c], &config),
  );
  let heads: Vec<String> = hg_ok(&hg, &c, &["heads", "--template", "{branch} {node}\\n"])
    .lines()
    .map(str::to_owned)
    .collect();
  let a_tip = hg_nodes(&hg, &a).first().cloned().unwrap_or_default();
  let b_tip = hg_nodes(&hg, &b).first().cloned().unwrap_or_default();
  assert_eq!(hg_nodes(&hg, &c).len(), 3, "C holds every changeset");
  let mut expected = vec![format!("default {a_tip}"), format!("default {b_tip}")];
  let mut seen = heads.clone();
  expected.sort();
  seen.sort();
  assert_eq!(seen, expected, "both heads on default: {heads:?}");
  hg_ok(&hg, &b, &with_config(&["pull", &url], &config));
  assert!(hg_nodes(&hg, &b).contains(&a_tip), "B pulled A's head");
}

/// The anchor's next lines (its standard error, as `lines` forwards them), read until `instance`'s daemon says it
/// serves: the lines read.
#[cfg(unix)]
fn lines_until(lines: &std::sync::mpsc::Receiver<String>, instance: &str) -> Vec<String> {
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let mut read = Vec::new();
  loop {
    let line = lines
      .recv_timeout(PATIENCE)
      .expect("the restarted daemon served within the test's patience");
    assert!(
      !line.starts_with(CRASH_LOOP_LINE),
      "the daemon never served: {line}"
    );
    let served = line.starts_with(&serving);
    read.push(line);
    if served {
      return read;
    }
  }
}

/// A-767 C4 (main's order of 19:50: hg's push onto the changeset store, red first on the real store). Do: start an
/// anchor whose daemon holds a changeset store in a fresh directory beside the test binary (`--vcs-store`); push two
/// changesets of stock hg 7.2 over the HTTP edge; read the push's landing count; kill the daemon (`SIGKILL`) and, once
/// the anchor's restarted daemon serves, clone the repository afresh. Expect the push landed as a changeset on the
/// store (`hg_push.landed` counted), and the clone after the restart holding both changesets: the recovery read the
/// push's changegroup back from the store, the register holding only its reference. Skips loudly without stock hg 7.2
/// ([`STOCK_HG`]) or an issuer surface.
#[cfg(unix)]
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_pushes_land_on_the_changeset_store_and_a_restarted_daemon_replays_them() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping the changeset store's hg push: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-store-{}", std::process::id());
  let binary = std::env::current_exe().unwrap();
  let store = binary
    .parent()
    .unwrap()
    .join(format!("vcs-store-hg-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&store);
  std::fs::create_dir_all(&store).unwrap();
  let store = store.canonicalize().unwrap();
  let mut child = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args([
      "--instance",
      &instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .args([
      "--vcs-http",
      "127.0.0.1:0",
      "--vcs-store",
      store.to_str().unwrap(),
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let stderr = child.stderr.take().unwrap();
  let (send, lines) = std::sync::mpsc::channel();
  let reader = std::thread::spawn(move || {
    for line in std::io::BufReader::new(stderr)
      .lines()
      .map_while(Result::ok)
    {
      let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
      if send.send(line).is_err() {
        break;
      }
    }
  });
  let _anchor = Anchor {
    child,
    drain: Some(reader),
  };
  let mut first = lines_until(&lines, &instance);
  while PRINTS_ISSUER_SURFACE && !first.iter().any(|line| issuer_surface(line).is_some()) {
    first.push(lines.recv_timeout(PATIENCE).unwrap());
  }
  let issuer = first.iter().find_map(|line| issuer_surface(line));
  settle(&instance, _anchor.child.id());
  let (code, out, err) = slates(&instance, &[], &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap root: {out}{err}");
  let Some(issuer) = issuer else {
    eprintln!("skipping the changeset store's hg push: no issuer surface on this platform");
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let config_for = |port: u16| -> Vec<String> {
    let prefix = format!("http://127.0.0.1:{port}/");
    [
      format!("auth.slates.prefix={prefix}"),
      format!("auth.slates.username={HG_USER}"),
      format!("auth.slates.password={token}"),
    ]
    .into_iter()
    .flat_map(|item| ["--config".to_owned(), item])
    .collect()
  };
  let port = http_edge_port(&first);
  let scratch = Scratch::new("hg-store");
  let local = format!("{}/local", scratch.path);
  hg_ok(&hg, &scratch.path, &["init", &local]);
  std::fs::write(format!("{local}/file"), "first\n").unwrap();
  hg_ok(
    &hg,
    &local,
    &["commit", "--addremove", "--message", "first"],
  );
  std::fs::write(format!("{local}/file"), "first\nsecond\n").unwrap();
  hg_ok(&hg, &local, &["commit", "--message", "second"]);
  let url = format!("http://127.0.0.1:{port}/r");
  hg_ok(
    &hg,
    &local,
    &with_config(&["push", &url], &config_for(port)),
  );
  let (code, status, err) = slates(&instance, &[], &["status", "--json"]);
  assert_eq!(code, 0, "status: {err}");
  assert!(
    status.contains("\"hg_push.landed\""),
    "the push did not land on the store: {status}"
  );
  let daemon = Command::new("pgrep")
    .args(["-f", "--", &format!("--instance {instance} daemon")])
    .output()
    .unwrap();
  let killed = String::from_utf8_lossy(&daemon.stdout)
    .split_whitespace()
    .next()
    .expect("the daemon's pid")
    .to_owned();
  assert!(
    Command::new("kill")
      .args(["-9", &killed])
      .status()
      .unwrap()
      .success()
  );
  let restarted = lines_until(&lines, &instance);
  let port = port_after(&restarted, VCS_HTTP_LINE).unwrap_or(port);
  settle(&instance, _anchor.child.id());
  let clone = format!("{}/clone", scratch.path);
  let url = format!("http://127.0.0.1:{port}/r");
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(&["clone", &url, &clone], &config_for(port)),
  );
  assert_eq!(
    hg_nodes(&hg, &clone),
    hg_nodes(&hg, &local),
    "the restarted daemon replayed the push from the store"
  );
  let _ = std::fs::remove_dir_all(&store);
}

/// A-650 and A-679 (§4.17.3 "The wire surface": SSH version 1, slates' own SSH server reading `hg -R <repo> serve
/// --stdio` in Mercurial's quoting). Do: start an anchor with its SSH edge on loopback (`--vcs-ssh 127.0.0.1:0`); make a
/// Mercurial repository through the CLI ([`create_hg_repository`]); enroll a fresh key (`credential key`, the issuer's
/// verb, run with the anchor's issuer surface: [`start_anchor`]); run [`hg_round_trip`] over `ssh://`
/// with OpenSSH holding that key alone. Expect each step done and both copies holding the same two changesets. Skips
/// loudly without stock hg 7.2 ([`STOCK_HG`]) or `ssh-keygen`, and where the platform names no issuer surface
/// ([`issuer_of`]). Everything it needs is built: the SSH edge (`--vcs-ssh <addr>`, A-684), the edge's
/// Mercurial command (A-679: `hg -R <repo> serve --stdio` through `serve::stdio`) and its pushes (A-696: `unbundle` in
/// the stdio session, through `hg_push` as the HTTP site's are since A-680 part 2b).
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_clones_pulls_and_pushes_over_the_ssh_edge() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg over the SSH edge: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let scratch = Scratch::new("hg-ssh");
  let Some((key, public)) = fresh_key(&scratch) else {
    eprintln!("skipping stock hg over the SSH edge: no ssh-keygen here");
    return;
  };
  let instance = format!("hg-ssh-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
  let (port, fingerprint) = started
    .lines
    .iter()
    .find_map(|line| ssh_edge_of(line))
    .expect("the daemon named its SSH edge's port and host key");
  let Some(issuer) = issuer_of(&started, "stock hg over the SSH edge") else {
    return;
  };
  let known_hosts = known_hosts(&scratch, port, &fingerprint);
  create_hg_repository(&instance, "r");
  let (code, out, err) = slates(&instance, &issuer, &["credential", "key", &public]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let config = vec!["--config".to_owned(), ssh_command(&key, &known_hosts)];
  let url = format!("ssh://{HG_USER}@127.0.0.1:{port}/r");
  hg_round_trip(&hg, &scratch.path, &url, &config);
}

/// Stock hg's SSH command (`ui.ssh`): OpenSSH with `key` alone and no configuration of its own, trusting only the host
/// keys in `known_hosts`, checked strictly.
fn ssh_command(key: &str, known_hosts: &str) -> String {
  format!(
    "ui.ssh=ssh -F /dev/null -i {key} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes \
     -o UserKnownHostsFile={known_hosts} -o GlobalKnownHostsFile=/dev/null"
  )
}

/// A-650, A-679 and A-684 (§4.17.3 "The wire surface": SSH version 1, slates' own SSH server reading `hg -R <repo>
/// serve --stdio`). Do: start an anchor with its SSH edge on loopback (`--vcs-ssh 127.0.0.1:0`); make a Mercurial
/// repository through the CLI ([`create_hg_repository`]); enroll a fresh key for its owner (`credential key`); clone
/// it with stock hg over `ssh://`, OpenSSH holding that key alone and trusting only the host key the daemon named
/// ([`hg_known_hosts`], `StrictHostKeyChecking=yes`); pull into the clone. Expect the clone made, empty (nothing has
/// been pushed: the pushes over `serve --stdio` are A-696, [`stock_hg_clones_pulls_and_pushes_over_the_ssh_edge`]),
/// its default path the edge's URL, and the pull finding no changes. Skips loudly without stock hg 7.2 ([`STOCK_HG`])
/// or `ssh-keygen`, and where the platform names no issuer surface ([`issuer_of`]).
#[test]
fn stock_hg_clones_over_the_ssh_edge() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's clone over the SSH edge: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let scratch = Scratch::new("hg-ssh-clone");
  let Some((key, public)) = fresh_key(&scratch) else {
    eprintln!("skipping stock hg's clone over the SSH edge: no ssh-keygen here");
    return;
  };
  let instance = format!("hg-ssh-clone-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
  let (port, fingerprint) = started
    .lines
    .iter()
    .find_map(|line| ssh_edge_of(line))
    .expect("the daemon named its SSH edge's port and host key");
  let Some(issuer) = issuer_of(&started, "stock hg's clone over the SSH edge") else {
    return;
  };
  let known_hosts = known_hosts(&scratch, port, &fingerprint);
  create_hg_repository(&instance, "r");
  let (code, out, err) = slates(&instance, &issuer, &["credential", "key", &public]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let config = vec!["--config".to_owned(), ssh_command(&key, &known_hosts)];
  let url = format!("ssh://{HG_USER}@127.0.0.1:{port}/r");
  let clone = format!("{}/clone", scratch.path);
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(&["clone", &url, &clone], &config),
  );
  assert_eq!(
    hg_nodes(&hg, &clone),
    Vec::<String>::new(),
    "the clone is empty"
  );
  assert_eq!(hg_ok(&hg, &clone, &["paths", "default"]).trim(), url);
  let (code, out, err) = hg_in(&hg, &clone, &with_config(&["pull"], &config));
  assert!(
    code == 0 && out.contains("no changes found"),
    "hg pull over the SSH edge: {code} {out}{err}"
  );
}

/// A-575 and A-654 (`slates repository branch create|retire|bind` in a Mercurial repository, where a plain name is a
/// bookmark; 62fb88997). Do: over the HTTP edge, push a first changeset to `default` as stock hg 7.2; make `topic` from
/// `default`'s first line with the CLI (`--from branch:default:0`, the namespace's spelling of a named branch's line:
/// a plain name there is a bookmark, so `--from default` names a bookmark `default`, which is refused `NotFound`) and
/// list a fresh clone's bookmarks; retire `topic` and list another fresh clone's; bind `topic`
/// again to the green the create printed and list a third's. Expect `topic` at the pushed changeset after the create,
/// gone after the retire, and back at the same changeset after the bind: stock hg sees the namespace the CLI changes.
/// Skips loudly without stock hg 7.2 ([`STOCK_HG`]) and where the platform names no issuer surface ([`issuer_of`]).
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_sees_the_bookmarks_the_cli_makes_retires_and_binds() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping the branch verbs under stock hg: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-bookmarks-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "the branch verbs under stock hg") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let url = format!("{prefix}r");
  let scratch = Scratch::new("hg-bookmarks");
  let local = format!("{}/local", scratch.path);
  hg_ok(&hg, &scratch.path, &["init", &local]);
  std::fs::write(format!("{local}/file"), "first\n").unwrap();
  hg_ok(
    &hg,
    &local,
    &["commit", "--addremove", "--message", "first"],
  );
  hg_ok(&hg, &local, &with_config(&["push", &url], &config));
  let pushed = hg_nodes(&hg, &local).into_iter().next().unwrap_or_default();
  let verb = |args: &[&str]| {
    let (code, out, err) = slates(&instance, &[], args);
    assert_eq!(code, 0, "{args:?}: {out}{err}");
    out
  };
  let mut clones = 0;
  let mut bookmarks = || {
    clones += 1;
    let copy = format!("{}/copy-{clones}", scratch.path);
    hg_ok(
      &hg,
      &scratch.path,
      &with_config(&["clone", "-U", &url, &copy], &config),
    );
    hg_ok(
      &hg,
      &copy,
      &["bookmarks", "--template", "{bookmark} {node}\\n"],
    )
  };
  let made = verb(&[
    "repository",
    "branch",
    "create",
    "r",
    "topic",
    "--from",
    "branch:default:0",
  ]);
  let green = value_of(&made, "id");
  let after_create = bookmarks();
  verb(&["repository", "branch", "retire", "r", "topic"]);
  let after_retire = bookmarks();
  verb(&["repository", "branch", "bind", "r", "topic", &green]);
  let after_bind = bookmarks();
  let topic = format!("topic {pushed}");
  assert_eq!(
    (
      after_create.contains(&topic),
      after_retire.contains("topic "),
      after_bind.contains(&topic),
    ),
    (true, false, true),
    "created: {after_create}; retired: {after_retire}; bound: {after_bind}"
  );
}

/// A-657 (`hg push -B`: a bookmark exported to the server, which keeps it in the repository's namespace). Do: over the
/// HTTP edge, push a first changeset as stock hg 7.2; make bookmark `pushed-bm` at it and push it (`hg push -B
/// pushed-bm`, answered "no changes found" and "exporting bookmark", exit 1 for no changesets, as hg answers any
/// server); list a fresh clone's bookmarks. Expect `pushed-bm` at the pushed changeset. Skips loudly without stock hg
/// 7.2 ([`STOCK_HG`]) and where the platform names no issuer surface ([`issuer_of`]).
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_pushes_a_bookmark_and_a_clone_sees_it() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's bookmark push: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-pushed-bm-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "stock hg's bookmark push") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let url = format!("{prefix}r");
  let scratch = Scratch::new("hg-pushed-bm");
  let local = format!("{}/local", scratch.path);
  hg_ok(&hg, &scratch.path, &["init", &local]);
  std::fs::write(format!("{local}/file"), "first\n").unwrap();
  hg_ok(
    &hg,
    &local,
    &["commit", "--addremove", "--message", "first"],
  );
  hg_ok(&hg, &local, &with_config(&["push", &url], &config));
  let pushed = hg_nodes(&hg, &local).into_iter().next().unwrap_or_default();
  hg_ok(&hg, &local, &["bookmark", "pushed-bm"]);
  let (code, out, err) = hg_in(
    &hg,
    &local,
    &with_config(&["push", "-B", "pushed-bm", &url], &config),
  );
  let copy = format!("{}/copy", scratch.path);
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(&["clone", "-U", &url, &copy], &config),
  );
  let listed = hg_ok(
    &hg,
    &copy,
    &["bookmarks", "--template", "{bookmark} {node}\\n"],
  );
  assert_eq!(
    (code <= 1, listed.contains(&format!("pushed-bm {pushed}"))),
    (true, true),
    "push -B: {code} {out}{err}; the clone's bookmarks: {listed}"
  );
}

/// Stock hg's listkeys and pushkey legs through slates at `url`, `config` its `--config` words: a changeset pushed and
/// bookmark `pushed-bm` exported at it (`push -B`); the bookmark dropped from the working copy and `hg incoming -B`
/// asked; a local bookmark made and `hg outgoing -B` asked; `hg debugpushkey` listing the `namespaces`, `bookmarks` and
/// `phases` keys, then moving bookmark `moved-bm` by the `pushkey` command: made from nothing, refused from a value it
/// does not hold, deleted from the one it holds; and a key of a namespace slates does not serve. Fails the test unless
/// each answers as stock hg answers a stock server: the incoming and outgoing bookmarks named, every listing holding
/// the server's keys, `True` for each move that holds and `False` for each that does not.
#[allow(clippy::disallowed_methods)]
fn pushkey_legs(hg: &[String], scratch: &str, url: &str, config: &[String]) {
  let local = format!("{scratch}/local");
  hg_ok(hg, scratch, &["init", &local]);
  std::fs::write(format!("{local}/file"), "first\n").unwrap();
  hg_ok(hg, &local, &["commit", "--addremove", "--message", "first"]);
  hg_ok(hg, &local, &with_config(&["push", url], config));
  let node = hg_nodes(hg, &local).into_iter().next().unwrap_or_default();
  hg_ok(hg, &local, &["bookmark", "pushed-bm"]);
  let (code, out, err) = hg_in(
    hg,
    &local,
    &with_config(&["push", "-B", "pushed-bm", url], config),
  );
  assert!(code <= 1, "push -B: {code} {out}{err}");
  hg_ok(hg, &local, &["bookmark", "--delete", "pushed-bm"]);
  let incoming = hg_in(hg, &local, &with_config(&["incoming", "-B", url], config));
  hg_ok(hg, &local, &["bookmark", "local-bm"]);
  let outgoing = hg_in(hg, &local, &with_config(&["outgoing", "-B", url], config));
  let keys = |space: &str| {
    hg_in(
      hg,
      &local,
      &with_config(&["debugpushkey", url, space], config),
    )
  };
  let push = |key: &str, old: &str, new: &str| {
    let (code, out, err) = hg_in(
      hg,
      &local,
      &with_config(&["debugpushkey", url, "bookmarks", key, old, new], config),
    );
    format!(
      "{code} {}{}",
      out.trim(),
      if err.is_empty() { "" } else { " (stderr)" }
    )
  };
  let namespaces = keys("namespaces");
  let bookmarks = keys("bookmarks");
  let phases = keys("phases");
  let made = push("moved-bm", "", &node);
  let after_made = keys("bookmarks").1;
  let stale = push("moved-bm", "0000000000000000000000000000000000000001", "");
  let deleted = push("moved-bm", &node, "");
  let after_deleted = keys("bookmarks").1;
  let (foreign_code, foreign_out, _) = hg_in(
    hg,
    &local,
    &with_config(&["debugpushkey", url, "nonesuch", "k", "", "v"], config),
  );
  let short = node.get(..12).unwrap_or_default();
  assert_eq!(
    (
      (
        incoming.0,
        incoming
          .1
          .lines()
          .any(|line| line.contains("pushed-bm") && line.contains(short))
      ),
      (outgoing.0, outgoing.1.contains("local-bm")),
      ["bookmarks", "namespaces", "phases"].map(|name| namespaces.1.contains(&format!("{name}\t"))),
      bookmarks.1.contains(&format!("pushed-bm\t{node}")),
      phases.1.contains("publishing\tTrue"),
      (
        made.as_str(),
        after_made.contains(&format!("moved-bm\t{node}"))
      ),
      stale.as_str(),
      (deleted.as_str(), after_deleted.contains("moved-bm")),
      (foreign_code, foreign_out.trim()),
    ),
    (
      (0, true),
      (0, true),
      [true; 3],
      true,
      true,
      ("0 True", true),
      "1 False",
      ("0 True", false),
      (
        1,
        "remote: pushkey: no key of namespace 'nonesuch' is moved by this server\nFalse"
      ),
    ),
    "incoming -B: {incoming:?}; outgoing -B: {outgoing:?}; namespaces: {namespaces:?}; bookmarks: {bookmarks:?}; \
     phases: {phases:?}; after the move: {after_made}; after the delete: {after_deleted}"
  );
}

/// A-758 (`listkeys` and `pushkey`, 7.2's `wireprotov1server`, served over the HTTP edge). Do: over `--vcs-http`, run
/// [`pushkey_legs`] as stock hg 7.2. Expect each leg answered as a stock server answers it. Skips loudly without stock
/// hg 7.2 ([`STOCK_HG`]) and where the platform names no issuer surface ([`issuer_of`]).
#[test]
fn stock_hg_lists_and_moves_keys_over_the_vcs_http_edge() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's listkeys and pushkey over HTTP: no Mercurial 7.2 here (set {STOCK_HG} to the command that \
       runs it)"
    );
    return;
  };
  let instance = format!("hg-keys-http-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "stock hg's listkeys and pushkey over HTTP") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let scratch = Scratch::new("hg-keys-http");
  pushkey_legs(&hg, &scratch.path, &format!("{prefix}r"), &config);
}

/// A-758 (`listkeys` and `pushkey` over `hg serve --stdio`, the SSH edge). Do: over `--vcs-ssh`, a fresh key enrolled
/// for the repository's owner, run [`pushkey_legs`] as stock hg 7.2. Expect each leg answered as a stock server
/// answers it. Skips loudly without stock hg 7.2 ([`STOCK_HG`]) or `ssh-keygen`, and where the platform names no
/// issuer surface ([`issuer_of`]).
#[test]
fn stock_hg_lists_and_moves_keys_over_the_ssh_edge() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's listkeys and pushkey over SSH: no Mercurial 7.2 here (set {STOCK_HG} to the command that \
       runs it)"
    );
    return;
  };
  let scratch = Scratch::new("hg-keys-ssh");
  let Some((key, public)) = fresh_key(&scratch) else {
    eprintln!("skipping stock hg's listkeys and pushkey over SSH: no ssh-keygen here");
    return;
  };
  let instance = format!("hg-keys-ssh-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
  let (port, fingerprint) = started
    .lines
    .iter()
    .find_map(|line| ssh_edge_of(line))
    .expect("the daemon named its SSH edge's port and host key");
  let Some(issuer) = issuer_of(&started, "stock hg's listkeys and pushkey over SSH") else {
    return;
  };
  let known_hosts = known_hosts(&scratch, port, &fingerprint);
  create_hg_repository(&instance, "r");
  let (code, out, err) = slates(&instance, &issuer, &["credential", "key", &public]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let config = vec!["--config".to_owned(), ssh_command(&key, &known_hosts)];
  pushkey_legs(
    &hg,
    &scratch.path,
    &format!("ssh://{HG_USER}@127.0.0.1:{port}/r"),
    &config,
  );
}

/// Runs `hg -R r serve --stdio` over the SSH edge at loopback `port` with stock OpenSSH (`key` alone, trusting the host
/// keys in `known_hosts`, as [`ssh_command`]), `input` on its standard input, in `dir`: its exit code and both outputs.
fn ssh_fed(
  dir: &str,
  (key, known_hosts, port): (&str, &str, u16),
  input: &str,
) -> (i32, String, String) {
  let known = format!("UserKnownHostsFile={known_hosts}");
  let mut child = Command::new("ssh")
    .args([
      "-F",
      "/dev/null",
      "-i",
      key,
      "-o",
      "IdentitiesOnly=yes",
      "-o",
      "BatchMode=yes",
    ])
    .args([
      "-o",
      "StrictHostKeyChecking=yes",
      "-o",
      &known,
      "-o",
      "GlobalKnownHostsFile=/dev/null",
    ])
    .args([
      "-p",
      &port.to_string(),
      &format!("{HG_USER}@127.0.0.1"),
      "hg -R r serve --stdio",
    ])
    .current_dir(dir)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child
    .stdin
    .take()
    .unwrap()
    .write_all(input.as_bytes())
    .unwrap();
  let out = read_on_a_thread(child.stdout.take().unwrap());
  let err = read_on_a_thread(child.stderr.take().unwrap());
  let code = exit_within(&mut child, &format!("ssh serve --stdio in {dir}"));
  (code, out.join().unwrap(), err.join().unwrap())
}

/// Runs stock hg as [`hg_in`] does with `input` on its standard input: its exit code and both outputs.
fn hg_fed(hg: &[String], dir: &str, args: &[&str], input: &str) -> (i32, String, String) {
  let (program, leading) = hg.split_first().expect("a command");
  let mut child = Command::new(program)
    .args(leading)
    .args(args)
    .current_dir(dir)
    .env("HGPLAIN", "1")
    .env("HGRCPATH", "")
    .env("HGUSER", "Test <test@example.com>")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child
    .stdin
    .take()
    .unwrap()
    .write_all(input.as_bytes())
    .unwrap();
  let out = read_on_a_thread(child.stdout.take().unwrap());
  let err = read_on_a_thread(child.stderr.take().unwrap());
  let code = exit_within(&mut child, &format!("hg {args:?} in {dir}"));
  (code, out.join().unwrap(), err.join().unwrap())
}

/// Stock hg's batched `pushkey` through slates at `url`, `config` its `--config` words: two changesets pushed, bookmark
/// `batched-bm` made at the first by `hg debugpushkey`, then one `batch` sent by `send` (given the working copy and the
/// two changesets) moving `batched-bm` from the first changeset to the second by a `pushkey` call among its calls.
/// Fails the test unless the batch's client exits `code`, its outputs holding `expected` (given the second changeset),
/// and the server then lists `batched-bm` at the second changeset where `moves`, else still at the first.
#[allow(clippy::disallowed_methods)]
fn batch_legs(
  hg: &[String],
  scratch: &str,
  (url, config): (&str, &[String]),
  send: impl Fn(&str, &str, &str) -> (i32, String, String),
  (code, expected, moves): (i32, impl Fn(&str) -> String, bool),
) {
  let local = format!("{scratch}/local");
  hg_ok(hg, scratch, &["init", &local]);
  std::fs::write(format!("{local}/file"), "first\n").unwrap();
  hg_ok(hg, &local, &["commit", "--addremove", "--message", "first"]);
  hg_ok(hg, &local, &with_config(&["push", url], config));
  let first = hg_nodes(hg, &local).into_iter().next().unwrap_or_default();
  let made = hg_in(
    hg,
    &local,
    &with_config(
      &["debugpushkey", url, "bookmarks", "batched-bm", "", &first],
      config,
    ),
  );
  std::fs::write(format!("{local}/file"), "first\nsecond\n").unwrap();
  hg_ok(hg, &local, &["commit", "--message", "second"]);
  hg_ok(hg, &local, &with_config(&["push", url], config));
  let second = hg_nodes(hg, &local).into_iter().next().unwrap_or_default();
  let batched = send(&local, &first, &second);
  let listed = hg_in(
    hg,
    &local,
    &with_config(&["debugpushkey", url, "bookmarks"], config),
  );
  let at = if moves { &second } else { &first };
  assert_eq!(
    (
      made.0,
      batched.0,
      format!("{}{}", batched.1, batched.2).contains(&expected(&second)),
      listed.1.contains(&format!("batched-bm\t{at}"))
    ),
    (0, code, true, true),
    "debugpushkey: {made:?}; the batch: {batched:?}; the bookmarks after it: {listed:?}"
  );
}

/// A-758 (a batched `pushkey`, 7.2's `wireprotov1server.batch`, over the HTTP edge). 7.2 runs a batched call that pushes
/// only for a request by `POST` (`checkperm`'s "push requires POST request", `405`), and its client sends a batch by
/// `POST` only to a server listing `httppostargs`, which a stock server does not by default and slates does not: so a
/// stock client's batched `pushkey` is refused as a stock server refuses it. Do: send [`batch_legs`]'s batch as stock
/// hg 7.2 over `--vcs-http`. Expect `HTTP Error 405: Method Not Allowed` and the bookmark where it was (the batch by
/// `POST` is held by the edge's own test, `a_batched_pushkey_is_landed_over_post_and_refused_over_get`). Skips loudly
/// without stock hg 7.2 ([`STOCK_HG`]) and where the platform names no issuer surface ([`issuer_of`]).
#[test]
fn stock_hg_batches_a_pushkey_over_the_vcs_http_edge() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's batched pushkey over HTTP: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs \
       it)"
    );
    return;
  };
  let instance = format!("hg-batch-http-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "stock hg's batched pushkey over HTTP") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let scratch = Scratch::new("hg-batch-http");
  let url = format!("{prefix}r");
  // 7.2's batching peer (`hg debugwireproto`'s `batchbegin` and `batchsubmit`): `listkeys`, the `pushkey`, `listkeys`.
  let send = |local: &str, first: &str, second: &str| {
    let script = format!(
      "batchbegin\ncommand listkeys\n    namespace bookmarks\ncommand pushkey\n    namespace bookmarks\n    key \
       batched-bm\n    old {first}\n    new {second}\ncommand listkeys\n    namespace bookmarks\nbatchsubmit\n"
    );
    hg_fed(
      &hg,
      local,
      &with_config(&["debugwireproto", &url], &config),
      &script,
    )
  };
  let expected = |_: &str| "abort: HTTP Error 405: Method Not Allowed".to_owned();
  batch_legs(
    &hg,
    &scratch.path,
    (&url, &config),
    send,
    (255, expected, false),
  );
}

/// A-758 (a batched `pushkey` over `hg serve --stdio`, the SSH edge). Do: a fresh key enrolled for the repository's
/// owner, run [`batch_legs`] as stock hg 7.2 over `--vcs-ssh`. Expect the batch answered and the bookmark moved. Skips
/// loudly without stock hg 7.2 ([`STOCK_HG`]) or `ssh-keygen`, and where the platform names no issuer surface
/// ([`issuer_of`]).
#[test]
fn stock_hg_batches_a_pushkey_over_the_ssh_edge() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's batched pushkey over SSH: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs \
       it)"
    );
    return;
  };
  let scratch = Scratch::new("hg-batch-ssh");
  let Some((key, public)) = fresh_key(&scratch) else {
    eprintln!("skipping stock hg's batched pushkey over SSH: no ssh-keygen here");
    return;
  };
  let instance = format!("hg-batch-ssh-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
  let (port, fingerprint) = started
    .lines
    .iter()
    .find_map(|line| ssh_edge_of(line))
    .expect("the daemon named its SSH edge's port and host key");
  let Some(issuer) = issuer_of(&started, "stock hg's batched pushkey over SSH") else {
    return;
  };
  let known_hosts = known_hosts(&scratch, port, &fingerprint);
  create_hg_repository(&instance, "r");
  let (code, out, err) = slates(&instance, &issuer, &["credential", "key", &public]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let config = vec!["--config".to_owned(), ssh_command(&key, &known_hosts)];
  let url = format!("ssh://{HG_USER}@127.0.0.1:{port}/r");
  // 7.2's `debugwireproto` reaches no `ssh://` peer, so the batch goes as 7.2's SSH peer frames one (`batch`, its
  // dictionary, its `cmds`), through stock OpenSSH to `hg -R r serve --stdio`: the `pushkey`, then `listkeys`.
  let send = |local: &str, first: &str, second: &str| {
    let cmds = format!(
      "pushkey namespace=bookmarks,key=batched-bm,old={first},new={second};listkeys namespace=bookmarks"
    );
    let request = format!("batch\n* 0\ncmds {}\n{cmds}\n", cmds.len());
    ssh_fed(local, (&key, &known_hosts, port), &request)
  };
  // The `listkeys` after the `pushkey` lists the move: the calls ran in order.
  let expected = |second: &str| format!("1\n;batched-bm\t{second}");
  batch_legs(
    &hg,
    &scratch.path,
    (&url, &config),
    send,
    (0, expected, true),
  );
}

/// Format: the daemon's counter of the file revisions a pull sent whole against the null base
/// (`crate::vcs_store::hg_view::PULL_WHOLE_TEXTS` in `slates-server`, A-767 C1 step 1b).
const PULL_WHOLE_TEXTS: &str = "vcs_store.hg_pull_whole_texts";

/// Shape: the lines of the large file [`a_pull_of_one_changeset_sends_its_large_files_stored_delta_as_stock_does`]
/// edits, about 2 MB, so a whole text would dwarf any delta.
const LARGE_FILE_LINES: usize = 40_000;

/// The size of the changegroup part stock hg took in a `pull --debug`'s words (`bundle2-input-part: total payload
/// size N` after the part's `"changegroup"` line).
fn changegroup_payload(said: &str) -> u64 {
  let mut lines = said.lines();
  lines
    .find(|line| line.starts_with("bundle2-input-part: \"changegroup\""))
    .unwrap_or_else(|| panic!("no changegroup part taken: {said}"));
  lines
    .find_map(|line| line.strip_prefix("bundle2-input-part: total payload size "))
    .and_then(|size| size.trim().parse().ok())
    .unwrap_or_else(|| panic!("no changegroup payload size: {said}"))
}

/// A-767 C1 step 1b (main's ruling of 2026-11-21 08:45: measure a pull of one changeset that touches a large file,
/// slates' bytes against stock's). Do: over the HTTP edge, push a first changeset holding a file of
/// [`LARGE_FILE_LINES`] lines, clone it, then push a second changeset changing one line of it; pull the second into the
/// clone with `--debug`; pull the same changeset from the source by stock 7.2 itself into a clone of its first. Expect
/// the slates clone to hold the source's changesets and verify clean, no file revision sent whole
/// ([`PULL_WHOLE_TEXTS`] unmoved), and slates' changegroup within twice stock's; both sizes printed. Skips loudly
/// without stock hg 7.2 ([`STOCK_HG`]) or an issuer surface.
#[test]
#[allow(clippy::disallowed_methods)]
fn a_pull_of_one_changeset_sends_its_large_files_stored_delta_as_stock_does() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping a large file's pull against stock's: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-pull-large-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "a large file's pull against stock's") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let url = format!("{prefix}r");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let scratch = Scratch::new("hg-pull-large");
  let source = format!("{}/source", scratch.path);
  hg_ok(&hg, &scratch.path, &["init", &source]);
  let lines: Vec<String> = (0..LARGE_FILE_LINES)
    .map(|line| format!("line {line} of a large file, long enough to matter\n"))
    .collect();
  std::fs::write(format!("{source}/large"), lines.concat()).unwrap();
  hg_ok(
    &hg,
    &source,
    &["commit", "--addremove", "--message", "first"],
  );
  hg_ok(&hg, &source, &with_config(&["push", &url], &config));
  let ours = format!("{}/ours", scratch.path);
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(&["clone", "-U", &url, &ours], &config),
  );
  let theirs = format!("{}/theirs", scratch.path);
  hg_ok(
    &hg,
    &scratch.path,
    &["clone", "-U", "--rev", "0", &source, &theirs],
  );
  let mut edited = lines;
  if let Some(line) = edited.get_mut(LARGE_FILE_LINES / 2) {
    *line = "a changed line in the middle\n".to_owned();
  }
  std::fs::write(format!("{source}/large"), edited.concat()).unwrap();
  hg_ok(&hg, &source, &["commit", "--message", "second"]);
  hg_ok(&hg, &source, &with_config(&["push", &url], &config));
  let whole_before = counter(&instance, PULL_WHOLE_TEXTS);
  let (code, out, err) = hg_in(
    &hg,
    &ours,
    &with_config(&["pull", "--debug", &url], &config),
  );
  assert_eq!(code, 0, "pull from slates: {out}{err}");
  let slates_bytes = changegroup_payload(&format!("{out}{err}"));
  let whole = counter(&instance, PULL_WHOLE_TEXTS).saturating_sub(whole_before);
  let (code, out, err) = hg_in(&hg, &theirs, &["pull", "--debug", &source]);
  assert_eq!(code, 0, "pull from the source: {out}{err}");
  let stock_bytes = changegroup_payload(&format!("{out}{err}"));
  eprintln!(
    "a one-changeset pull of a {LARGE_FILE_LINES}-line file: slates {slates_bytes} bytes, stock 7.2 {stock_bytes}"
  );
  let (verified, out, err) = hg_in(&hg, &ours, &["verify"]);
  assert_eq!(
    (verified, hg_nodes(&hg, &ours) == hg_nodes(&hg, &source)),
    (0, true),
    "the slates clone holds the source's changesets and verifies: {out}{err}"
  );
  assert_eq!(whole, 0, "file revisions a pull sent whole");
  assert!(
    slates_bytes <= stock_bytes.saturating_mul(2),
    "slates sent {slates_bytes} bytes where stock 7.2 sent {stock_bytes}"
  );
}

/// Format: the daemon's counter of the asks a `lookup` or a `batch` made of the changeset store for the heads'
/// `.hgtags` (`crate::vcs_store::hg_view::LOOKUP_STORE_ASKS` in `slates-server`, A-767 C1).
const LOOKUP_STORE_ASKS: &str = "vcs_store.hg_lookup_store_asks";

/// Stock hg's pulls by a tag through slates at `url` (`config` its `--config` words), with daemon `instance`'s counters:
/// a history of three changesets pushed, the second tagging the first `v1` (`hg tag`); `hg pull -r v1` into an empty
/// repository; then `hg pull -r v1 -r tip` into another, which 7.2 sends as one `batch` of two lookups. Fails the test
/// unless the first holds the tagged changeset alone and the second the whole history, each pull's lookup of `v1` read
/// the tags from the changeset store, and the batch read them in one round as the lone lookup did. The tagged node.
#[allow(clippy::disallowed_methods)] // test code: its own scratch directory
fn tag_pulls(hg: &[String], scratch: &str, url: &str, config: &[String], instance: &str) -> String {
  let source = format!("{scratch}/tagged");
  hg_ok(hg, scratch, &["init", &source]);
  std::fs::write(format!("{source}/file"), "first\n").unwrap();
  hg_ok(
    hg,
    &source,
    &["commit", "--addremove", "--message", "first"],
  );
  hg_ok(hg, &source, &["tag", "--rev", "0", "v1"]);
  std::fs::write(format!("{source}/file"), "third\n").unwrap();
  hg_ok(hg, &source, &["commit", "--message", "third"]);
  hg_ok(hg, &source, &with_config(&["push", url], config));
  let history = hg_nodes(hg, &source);
  let tagged = history.last().cloned().unwrap_or_default();
  let pulled = |name: &str, revs: &[&str]| {
    let copy = format!("{scratch}/{name}");
    hg_ok(hg, scratch, &["init", &copy]);
    let before = counter(instance, LOOKUP_STORE_ASKS);
    let mut args = vec!["pull"];
    for rev in revs {
      args.extend(["-r", rev]);
    }
    args.push(url);
    hg_ok(hg, &copy, &with_config(&args, config));
    (
      hg_nodes(hg, &copy),
      counter(instance, LOOKUP_STORE_ASKS).saturating_sub(before),
    )
  };
  let (lone, lone_asks) = pulled("by-tag", &["v1"]);
  let (batched, batch_asks) = pulled("by-tag-and-tip", &["v1", "tip"]);
  assert_eq!(
    lone,
    vec![tagged.clone()],
    "`pull -r v1` brings the tagged changeset alone"
  );
  assert_eq!(
    batched, history,
    "`pull -r v1 -r tip` brings the whole history"
  );
  assert!(
    lone_asks > 0,
    "the lookup of v1 read the tags from the store"
  );
  assert_eq!(
    batch_asks, lone_asks,
    "the batch read the tags in one round, as the lone lookup did"
  );
  tagged
}

/// A-767 C1 (main's ruling (A) of 2026-11-21: lookup's `.hgtags` through the changeset store, tests by use). Do: over the
/// HTTP edge, run [`tag_pulls`]; push bookmark `mark` at the tagged changeset (a branch is made at a changeset a line
/// or bookmark heads, A-575); then make branch `from-tag` with the owner's verb `--from v1` and list a fresh clone's
/// bookmarks. Expect the pulls as [`tag_pulls`] holds them, and bookmark `from-tag` at the tagged changeset, the verb's
/// door having read the tags from the store ([`LOOKUP_STORE_ASKS`] moved). Red before the change: no counter moved (the
/// tags were read whole from the content store). Skips loudly without stock hg 7.2 ([`STOCK_HG`]) or an issuer surface.
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_pulls_by_a_tag_over_http_and_a_branch_is_made_from_the_tag() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's pull by a tag over HTTP: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let instance = format!("hg-tags-http-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-http", "127.0.0.1:0"]);
  let port = http_edge_port(&started.lines);
  let Some(issuer) = issuer_of(&started, "stock hg's pull by a tag over HTTP") else {
    return;
  };
  create_hg_repository(&instance, "r");
  let token = minted_token(&instance, &issuer);
  let prefix = format!("http://127.0.0.1:{port}/");
  let url = format!("{prefix}r");
  let config: Vec<String> = [
    format!("auth.slates.prefix={prefix}"),
    format!("auth.slates.username={HG_USER}"),
    format!("auth.slates.password={token}"),
  ]
  .into_iter()
  .flat_map(|item| ["--config".to_owned(), item])
  .collect();
  let scratch = Scratch::new("hg-tags-http");
  let tagged = tag_pulls(&hg, &scratch.path, &url, &config, &instance);
  // A branch is made at a changeset a line or bookmark heads (A-575): bookmark `mark` heads the tagged one.
  let source = format!("{}/tagged", scratch.path);
  hg_ok(&hg, &source, &["bookmark", "--rev", "0", "mark"]);
  let (code, out, err) = hg_in(
    &hg,
    &source,
    &with_config(&["push", "-B", "mark", &url], &config),
  );
  assert!(code == 0 || code == 1, "push -B mark: {out}{err}");
  let before = counter(&instance, LOOKUP_STORE_ASKS);
  let (code, out, err) = slates(
    &instance,
    &[],
    &[
      "repository",
      "branch",
      "create",
      "r",
      "from-tag",
      "--from",
      "v1",
    ],
  );
  assert_eq!(code, 0, "branch create --from v1: {out}{err}");
  let asks = counter(&instance, LOOKUP_STORE_ASKS).saturating_sub(before);
  let copy = format!("{}/bookmarks", scratch.path);
  hg_ok(
    &hg,
    &scratch.path,
    &with_config(&["clone", "-U", &url, &copy], &config),
  );
  let bookmarks = hg_ok(
    &hg,
    &copy,
    &["bookmarks", "--template", "{bookmark} {node}\\n"],
  );
  assert!(
    bookmarks.contains(&format!("from-tag {tagged}")),
    "the branch made at v1's changeset: {bookmarks}"
  );
  assert!(asks > 0, "the verb's door read the tags from the store");
}

/// A-767 C1 (main's ruling (A), tests by use). Do: over the SSH edge, run [`tag_pulls`]. Expect the pulls as it holds
/// them. Skips loudly without stock hg 7.2 ([`STOCK_HG`]), `ssh-keygen` or an issuer surface.
#[test]
#[allow(clippy::disallowed_methods)]
fn stock_hg_pulls_by_a_tag_over_ssh() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg's pull by a tag over SSH: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let scratch = Scratch::new("hg-tags-ssh");
  let Some((key, public)) = fresh_key(&scratch) else {
    eprintln!("skipping stock hg's pull by a tag over SSH: no ssh-keygen here");
    return;
  };
  let instance = format!("hg-tags-ssh-{}", std::process::id());
  let started = start_anchor(&instance, &["--vcs-ssh", "127.0.0.1:0"]);
  let (port, fingerprint) = started
    .lines
    .iter()
    .find_map(|line| ssh_edge_of(line))
    .expect("the daemon named its SSH edge's port and host key");
  let Some(issuer) = issuer_of(&started, "stock hg's pull by a tag over SSH") else {
    return;
  };
  let known_hosts = known_hosts(&scratch, port, &fingerprint);
  create_hg_repository(&instance, "r");
  let (code, out, err) = slates(&instance, &issuer, &["credential", "key", &public]);
  assert_eq!(code, 0, "credential key: {out}{err}");
  let config = vec!["--config".to_owned(), ssh_command(&key, &known_hosts)];
  let url = format!("ssh://{HG_USER}@127.0.0.1:{port}/r");
  tag_pulls(&hg, &scratch.path, &url, &config, &instance);
}

/// Format: the environment variable naming the repositories the correctness lane compares on (paths of local Mercurial
/// repositories, separated by `:`), such as hg's own history (`hg clone -U https://www.mercurial-scm.org/repo/hg`).
/// Unset, the lane makes and uses `bench/hg_correctness_fixture.sh`'s repository.
const LANE_SOURCES: &str = "SLATES_TEST_HG_SOURCES";

/// Format: the environment variable bounding each of the lane's steps in seconds (the script's `BOUND`); unset, the
/// script's own.
const LANE_BOUND: &str = "SLATES_TEST_HG_BOUND";

/// Whether `program` is here: it starts at all, whatever it answers.
fn present(program: &str) -> bool {
  Command::new(program)
    .arg("--version")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

/// UI goal 93, correctness by use (main's order of 2026-11-21: stock clients on real repositories get byte-identical
/// results; a measurement of the server, run by main). Do: run `bench/hg_correctness.sh` with stock hg 7.2 and this
/// build's `slates` over the repositories [`LANE_SOURCES`] names, or over the fixture `bench/hg_correctness_fixture.sh`
/// makes (named branches, one closed; merges, one with a conflict resolved by hand; tags, one moved and one removed;
/// bookmarks; draft and secret changesets; binaries of 1, 16 and 64 MiB). The script fills stock `hg serve`, slates over
/// its HTTP edge and slates over its SSH edge alike, then compares their answers: push, clone (`-U`, `-r`, a named
/// branch), `hg verify`, every changeset's node, parents, branch and phase, tags, bookmarks, branches, heads, incoming
/// and outgoing byte for byte, pull, a new head refused without `-f` in stock's words, `-f`, `-B`, a draft made public
/// by a publishing server, a tag, `pull -r` and `pull -B`. Expect every check PASS (the script exits 0); each FAIL line
/// names the server, the verb and the difference. Ignored: main runs it (CLAUDE.md §8). Skips loudly without stock hg
/// 7.2 ([`STOCK_HG`]), bash, perl, python3 or OpenSSH's `ssh-keygen` and `ssh-keyscan`.
#[test]
#[ignore = "a measurement of the server against stock hg serve, run by main (CLAUDE.md §8)"]
fn stock_hg_gets_what_stock_hg_serve_gives_on_every_verb_over_http_and_ssh() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping the hg correctness lane: no Mercurial 7.2 here (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  for tool in ["bash", "perl", "python3", "ssh-keygen", "ssh-keyscan"] {
    if !present(tool) {
      eprintln!("skipping the hg correctness lane: no {tool} here");
      return;
    }
  }
  let bench = concat!(env!("CARGO_MANIFEST_DIR"), "/bench");
  let scratch = Scratch::new("hg-lane");
  let hg_words = hg.join(" ");
  let sources: Vec<String> = match std::env::var(LANE_SOURCES) {
    Ok(listed) => listed
      .split(':')
      .filter(|path| !path.is_empty())
      .map(str::to_owned)
      .collect(),
    Err(_) => {
      let fixture = format!("{}/fixture", scratch.path);
      let made = Command::new("bash")
        .arg(format!("{bench}/hg_correctness_fixture.sh"))
        .args([&hg_words, &fixture])
        .output()
        .unwrap();
      assert!(made.status.success(), "the fixture: {made:?}");
      eprintln!("{}", String::from_utf8_lossy(&made.stdout).trim());
      vec![fixture]
    }
  };
  let mut lane = Command::new("bash");
  lane
    .arg(format!("{bench}/hg_correctness.sh"))
    .arg(&hg_words)
    .arg(format!("{}/work", scratch.path))
    .args(&sources)
    .env("SLATES", env!("CARGO_BIN_EXE_slates"));
  if let Ok(bound) = std::env::var(LANE_BOUND) {
    lane.env("BOUND", bound);
  }
  let ran = lane.output().unwrap();
  let out = String::from_utf8_lossy(&ran.stdout);
  eprintln!("{out}");
  let failed: Vec<&str> = out
    .lines()
    .filter(|line| line.starts_with("FAIL "))
    .collect();
  assert!(
    ran.status.success() && failed.is_empty(),
    "the hg correctness lane over {sources:?}: {} failed:\n{}\n{}",
    failed.len(),
    failed.join("\n"),
    String::from_utf8_lossy(&ran.stderr)
  );
}
