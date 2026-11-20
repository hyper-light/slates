//! Stock Mercurial 7.2 against a real `slates anchor` and its daemon (UI goal 93; §4.17.3): stock hg clones, pushes and
//! pulls a repository over each route slates serves Mercurial on, the daemon's HTTP edge (`--vcs-http`, A-680) and its
//! SSH edge (`--vcs-ssh`, A-679, A-684, A-696), and is refused in its own words where it brings no credential.
//!
//! Moved here from `tests/cli.rs` (2026-11-20, main's "all functionality, every platform"), whose Unix-only gate kept
//! these legs off Windows though nothing in them is Unix's: on jupiter stock hg is Mercurial 7.2 run by `uvx`
//! (`SLATES_TEST_HG="uvx --quiet --python 3.12 --from mercurial==7.2 hg"`, as on the Mac Studio), and `ssh` the OpenSSH
//! the PATH names first. The harness is `tests/git_interop.rs`'s: the anchor started and its lines read until its
//! daemon serves, a credential minted with the anchor's issuer surface (named on macOS and Windows; on Linux a
//! descriptor only the anchor's children hold, so a leg that needs one skips loudly there), and each test's copies and
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
    _anchor: anchor,
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

/// A-654 (`hg push -B`: a bookmark exported to the server, which keeps it in the repository's namespace). Do: over the
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
  for space in ["bookmarks", "namespaces", "phases"] {
    let keys = hg_in(&hg, &local, &with_config(&["--debug", "debugpushkey", &url, space], &config));
    eprintln!("DIAG debugpushkey {space}: {keys:?}");
  }
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
