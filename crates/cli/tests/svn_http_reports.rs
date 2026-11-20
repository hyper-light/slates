//! Stock Subversion's `svn blame` and `svn diff` against a real `slates anchor` over `http://` (UI goal 93; A-726: the
//! DAV reports those two commands ask the WebDAV site on the shared HTTP edge, `--vcs-http`). One history is committed
//! by stock `svn` 1.15 to a stock FSFS repository through stock `svnserve -t` (the control, run by `svn`'s own tunnel)
//! and to a slates repository over HTTP, then blamed and diffed on each; every answer must be the same but for the
//! author's name, which the two routes take from different users.
//!
//! The harness (the anchor, its settling, the token, the scratch directory and the `svn` runner) is
//! `tests/svn_interop.rs`'s, copied here so this file does not touch that one while its own pieces land; folding the
//! two into one shared module is owed (GAPS). Stock `svn`, `svnadmin` and `svnserve` are found on the PATH, and a host
//! without them, or whose `svn` is not of the 1.15 line, skips loudly; so does a platform whose anchor names no issuer
//! surface (Linux). Unix only: the control's tunnel runs through `sh`.
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

/// The text of `text` between the first `start` and the first `end` after it.
fn between<'t>(text: &'t str, start: &str, end: &str) -> Option<&'t str> {
  Some(text.split_once(start)?.1.split_once(end)?.0)
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

/// The author of `url`'s youngest revision, from its log's `author` element.
fn author_of_head(svn: &Svn, url: &str) -> String {
  let log = svn.ok(&svn.scratch, &["log", "--xml", "--revision", "HEAD", url]);
  between(&log, "<author>", "</author>")
    .unwrap_or_else(|| panic!("the head names its author: {log}"))
    .to_owned()
}

/// The history session, run as `svn` against `url`, a fresh repository: [`FILE`] added as revision 1, a line changed
/// and one added as revision 2, the first line changed as revision 3, each from one working copy. What stock `svn`
/// then answers, each blame's lines as revision and text alone ([`blamed_lines`]): `svn blame` of the file's URL,
/// `svn diff -r 1:3` and `svn diff -c 2` of the repository's URL, `svn blame -r 2` (a blame that stops short of the
/// head, its peg traced back), `svn diff --summarize`, a
/// diff of the file's URL at two pegs, and `svn cat -r 1`, each with `url` written `URL`.
fn history_answers(svn: &Svn, url: &str) -> Vec<String> {
  let copy = svn.copy("history");
  svn.ok(&svn.scratch, &["checkout", url, &copy]);
  put(&copy, "one\ntwo\nthree\n");
  svn.ok(&copy, &["add", FILE]);
  svn.ok(&copy, &["commit", "--message", "one"]);
  put(&copy, "one\nTWO\nthree\nfour\n");
  svn.ok(&copy, &["commit", "--message", "two"]);
  put(&copy, "ONE\nTWO\nthree\nfour\n");
  svn.ok(&copy, &["commit", "--message", "three"]);
  let author = author_of_head(svn, url);
  let file = format!("{url}/{FILE}");
  let (at_two, at_three) = (format!("{file}@2"), format!("{file}@3"));
  [
    vec!["blame", file.as_str()],
    vec!["diff", "-r", "1:3", url],
    vec!["diff", "-c", "2", url],
    vec!["blame", "-r", "2", file.as_str()],
    vec!["diff", "--summarize", "-r", "1:3", url],
    vec!["diff", at_two.as_str(), at_three.as_str()],
    vec!["cat", "-r", "1", file.as_str()],
  ]
  .iter()
  .map(|args| {
    let answer = svn.ok(&svn.scratch, args).replace(url, "URL");
    if args.first() == Some(&"blame") {
      blamed_lines(&answer, &author)
    } else {
      answer
    }
  })
  .collect()
}

/// Writes `text` as working copy `copy`'s file `name`, inside the scratch directory the test removes (R1's wall is
/// shipped code's).
#[allow(clippy::disallowed_methods)]
fn put_named(copy: &str, name: &str, text: &str) {
  std::fs::write(format!("{copy}/{name}"), text).unwrap();
}

/// The merge session, run as `svn` against `url`, a fresh repository: [`FILE`] added as revision 1; copied by URL to
/// `branch` as revision 2; `branch` edited as revision 3; then `svn merge ^/branch` into [`FILE`] committed as revision
/// 4. What stock `svn` answers, each with `url` written `URL`, each blame's lines as revision and text alone: the
/// merge's own report, `svn blame -g` of the file (merged revisions asked for: the `mergeinfo-report`, then the
/// `file-revs-report` with them), `svn mergeinfo` of the branch into the file, and `svn propget svn:mergeinfo` of the
/// file.
fn merge_answers(svn: &Svn, url: &str) -> Vec<String> {
  let copy = svn.copy("merge");
  svn.ok(&svn.scratch, &["checkout", url, &copy]);
  put(&copy, "one\ntwo\n");
  svn.ok(&copy, &["add", FILE]);
  svn.ok(&copy, &["commit", "--message", "one"]);
  let (file, branch) = (format!("{url}/{FILE}"), format!("{url}/branch"));
  svn.ok(
    &svn.scratch,
    &["copy", &file, &branch, "--message", "branch"],
  );
  svn.ok(&copy, &["update"]);
  put_named(&copy, "branch", "one\nTWO\n");
  svn.ok(&copy, &["commit", "--message", "on the branch"]);
  let merged = svn.ok(&copy, &["merge", "^/branch", FILE]);
  svn.ok(&copy, &["commit", "--message", "merged"]);
  let author = author_of_head(svn, url);
  vec![
    merged,
    blamed_lines(
      &svn
        .ok(&svn.scratch, &["blame", "-g", &file])
        .replace(url, "URL"),
      &author,
    ),
    svn
      .ok(&svn.scratch, &["mergeinfo", &branch, &file])
      .replace(url, "URL"),
    svn
      .ok(&svn.scratch, &["propget", "svn:mergeinfo", &file])
      .replace(url, "URL"),
  ]
}

/// `svn blame`'s answer with each line's author checked and dropped (`%6ld %10s %s`: the author cut to its column's
/// ten characters, or padded to them): each line as its revision and its text. Fails the test unless every line names
/// the start of `author`.
fn blamed_lines(answer: &str, author: &str) -> String {
  answer
    .lines()
    .map(|line| {
      // `svn blame -g` marks a merged line `G` before its revision.
      let (merged, line) = match line.strip_prefix('G') {
        Some(rest) => ("G ", rest),
        None => ("", line),
      };
      let (revision, rest) = line.trim_start().split_once(' ').unwrap();
      let (named, text) = rest.trim_start().split_once(' ').unwrap();
      assert!(
        author.starts_with(named),
        "the blamed line's author: {line:?}"
      );
      format!("{merged}{revision} {text}\n")
    })
    .collect()
}

/// UI goal 93's blame and diff over `http://` (A-726). Do: run the history session ([`history_answers`]) as stock
/// `svn` 1.15 against a stock FSFS repository through stock `svnserve -t` (the control's tunnel, as
/// [`TUNNEL_USER`]), and again against the HTTP leg. Expect: every answer the same, byte for byte, but for the author's
/// name. Skips loudly without stock Subversion 1.15.
#[test]
fn stock_svn_blames_and_diffs_over_the_vcs_http_edge_as_against_svnserve() {
  if !stock_svn() || !stock_svnserve() {
    eprintln!(
      "skipping blame and diff over HTTP: no stock Subversion 1.15 here (svn, svnadmin and svnserve on the PATH)"
    );
    return;
  }
  let scratch = Scratch::new("svn-history-control");
  let root = format!("{}/repositories", scratch.path);
  made_directory(&root);
  let made = Command::new("svnadmin")
    .args(["create", &format!("{root}/history")])
    .output()
    .unwrap();
  assert!(made.status.success(), "svnadmin create history: {made:?}");
  let tunnel = vec![
    "--config-option".to_owned(),
    format!(
      "config:tunnels:{CONTROL_TUNNEL}=sh -c 'exec svnserve -t -r {root} --tunnel-user={TUNNEL_USER}'"
    ),
  ];
  let control = Svn::new(&scratch, "history", tunnel);
  let expected = history_answers(
    &control,
    &format!("svn+{CONTROL_TUNNEL}://localhost/history"),
  );
  let Some(leg) = http_leg("history") else {
    return;
  };
  let answered = history_answers(&leg.svn, &leg.url);
  assert_eq!(
    answered, expected,
    "blame and diff over HTTP as against svnserve"
  );
}

/// UI goal 93's merged blame over `http://` (A-726 part 2: the `mergeinfo-report`). Do: run the merge session
/// ([`merge_answers`]) as stock `svn` 1.15 against a stock FSFS repository through stock `svnserve -t`, and again
/// against the HTTP leg. Expect: every answer the same, byte for byte, but for the author's name. Skips loudly without
/// stock Subversion 1.15.
#[test]
fn stock_svn_blames_merged_revisions_over_the_vcs_http_edge_as_against_svnserve() {
  if !stock_svn() || !stock_svnserve() {
    eprintln!(
      "skipping merged blame over HTTP: no stock Subversion 1.15 here (svn, svnadmin and svnserve on the PATH)"
    );
    return;
  }
  let scratch = Scratch::new("svn-merge-control");
  let root = format!("{}/repositories", scratch.path);
  made_directory(&root);
  let made = Command::new("svnadmin")
    .args(["create", &format!("{root}/merge")])
    .output()
    .unwrap();
  assert!(made.status.success(), "svnadmin create merge: {made:?}");
  let tunnel = vec![
    "--config-option".to_owned(),
    format!(
      "config:tunnels:{CONTROL_TUNNEL}=sh -c 'exec svnserve -t -r {root} --tunnel-user={TUNNEL_USER}'"
    ),
  ];
  let control = Svn::new(&scratch, "merge", tunnel);
  let control_url = format!("svn+{CONTROL_TUNNEL}://localhost/merge");
  let expected = merge_answers(&control, &control_url);
  let Some(leg) = http_leg("merge") else {
    return;
  };
  let answered = merge_answers(&leg.svn, &leg.url);
  assert_eq!(
    answered, expected,
    "merged blame over HTTP as against svnserve"
  );
}
