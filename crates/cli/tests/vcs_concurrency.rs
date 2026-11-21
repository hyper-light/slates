//! Goal 93 correctness (main's piece for w-forming, 2026-11-21): many stock clients at once against one slates
//! repository, each scenario run first against the stock server (`hg serve`, `svnserve`) as the control, so what slates
//! must do is stock's own behaviour where the race leaves it one.
//!
//! - **Mercurial** (stock hg 7.2 over the daemon's HTTP edge): N clones commit on the same head and push at once
//!   (exactly one accepted, every other refused in stock's own words for a race on heads); N clones push N new named
//!   branches at once (all accepted); N clones push N bookmarks on new branches at once (all accepted, each where its
//!   client put it). After each, a fresh clone passes `hg verify` and holds the heads and bookmarks accepted.
//! - **Subversion** (stock svn 1.15 over the svn:// listener): N working copies change the same file and commit at
//!   once (exactly one accepted, every other refused out of date); N working copies add N paths and commit at once (all
//!   accepted); the revisions are numbered without gaps, one a commit accepted, and the history, replayed into a local
//!   FSFS repository (`svnrdump dump` into `svnadmin load`), passes `svnadmin verify`.
//!
//! These are interop rounds main runs (CLAUDE.md §8): each test is ignored and run by its recorded command
//! (`run-requests.md`), N from `SLATES_CONC_N` (spaces between; default `2`) and the scenarios from
//! `SLATES_CONC_SCENARIOS` (default all). Without stock hg 7.2 (`SLATES_TEST_HG`, as `hg_interop.rs` finds it) or stock
//! svn, svnserve, svnadmin and svnrdump on the PATH, a test skips loudly. The harness is `hg_interop.rs`'s and
//! `svn_interop.rs`'s: an anchor started and its lines read until its daemon serves, a token minted with its issuer
//! surface, each test's copies in a scratch directory in the build output, removed at its end.
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

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Barrier;
use std::time::{Duration, Instant};

/// Shape: the shards of the test's daemon, as `hg_interop.rs` starts one.
const SHARDS: &str = "2";
/// Shape: consecutive answered verbs before the daemon counts as settled (`hg_interop.rs`).
const STABLE_STREAK: u32 = 10;
/// Shape: a guard against a client or server that never answers, far above what any command here takes.
const PATIENCE: Duration = Duration::from_secs(300);
/// Shape: the pause between polls.
const POLL: Duration = Duration::from_millis(20);
/// Shape: how long a stock control server is given to listen before the clients start.
const CONTROL_START: Duration = Duration::from_secs(3);
/// Shape: the variable naming the client counts, spaces between.
const CONC_N: &str = "SLATES_CONC_N";
/// Shape: the variable naming the scenarios to run, spaces between (default all).
const CONC_SCENARIOS: &str = "SLATES_CONC_SCENARIOS";
/// Shape: the variable naming the command that runs stock Mercurial 7.2 (`hg_interop.rs`'s).
const STOCK_HG: &str = "SLATES_TEST_HG";
/// Format: the user name given over Basic and to svn; the token proves the principal.
const USER: &str = "slates";

/// Format: the daemon's line once it serves; its instance follows.
const SERVING_LINE: &str = "slates daemon: instance ";
/// Format: the anchor's crash-loop line (A-119).
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";
/// Format: the anchor's issuer-surface line.
const ISSUER_SURFACE_LINE: &str = "slates anchor: issuer surface: export ";
/// Format: the daemon's HTTP edge line; the port follows.
const VCS_HTTP_LINE: &str =
  "slates daemon: the version-control HTTP edge serves cleartext on 127.0.0.1:";
/// Format: the daemon's svn:// listener line; the port follows.
const VCS_SVN_LINE: &str = "slates daemon: the svn:// listener serves on 127.0.0.1:";

/// Format: stock hg 7.2's words refusing a push that lost a race on the remote's heads: the push's discovery saw the new
/// head (`mercurial/discovery.py`), or the server's `check:heads` saw it (`mercurial/exchange.py`).
const HG_HEAD_RACE_REFUSALS: [&str; 2] =
  ["abort: push creates new remote head", "abort: push failed:"];
/// Format: stock svn 1.15's error codes refusing a commit of a file changed since its base (`E160028` out of date at
/// the server, `E155011` at the client, `E160024` a conflict, `E170004` an out-of-date item).
const SVN_OUT_OF_DATE: [&str; 4] = ["E160028", "E155011", "E160024", "E170004"];

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
        .replace('\\', "/"),
    }
  }
}

impl Drop for Scratch {
  #[allow(clippy::disallowed_methods)]
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

#[allow(clippy::disallowed_methods)]
fn made_directory(path: &str) {
  std::fs::create_dir_all(path).unwrap_or_else(|error| panic!("making {path}: {error}"));
}

#[allow(clippy::disallowed_methods)]
fn write(path: &str, text: &str) {
  std::fs::write(path, text).unwrap();
}

#[allow(clippy::disallowed_methods)]
fn append(path: &str, text: &str) {
  let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
  file.write_all(text.as_bytes()).unwrap();
}

fn read_on_a_thread(mut from: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
  std::thread::spawn(move || {
    let mut bytes = Vec::new();
    let _ = from.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
  })
}

fn pause() {
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

/// Runs `program` with `args` in `dir` and `env`, no terminal, killed past [`PATIENCE`]: exit code and both outputs
/// (-9 for a kill).
fn run(program: &str, args: &[String], dir: &str, env: &[(&str, &str)]) -> (i32, String, String) {
  let mut child = Command::new(program)
    .args(args)
    .current_dir(dir)
    .envs(env.iter().copied())
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap_or_else(|error| panic!("{program}: {error}"));
  let out = read_on_a_thread(child.stdout.take().unwrap());
  let err = read_on_a_thread(child.stderr.take().unwrap());
  let started = Instant::now();
  let code = loop {
    if let Some(status) = child.try_wait().unwrap() {
      break status.code().unwrap_or(-1);
    }
    if started.elapsed() > PATIENCE {
      let _ = child.kill();
      let _ = child.wait();
      break -9;
    }
    pause();
  };
  (code, out.join().unwrap(), err.join().unwrap())
}

/// Runs every closure at the same instant (behind one barrier) on scoped threads of their own: their results, in order.
fn at_once<T: Send>(jobs: Vec<Box<dyn FnOnce() -> T + Send + '_>>) -> Vec<T> {
  let barrier = Barrier::new(jobs.len());
  std::thread::scope(|scope| {
    let handles: Vec<_> = jobs
      .into_iter()
      .map(|job| {
        let barrier = &barrier;
        scope.spawn(move || {
          barrier.wait();
          job()
        })
      })
      .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
  })
}

/// The refusal line of a failed client's output: its last `abort:` or `svn: E` line.
fn refusal_line(out: &str, err: &str) -> String {
  err
    .lines()
    .chain(out.lines())
    .rfind(|line| line.starts_with("abort:") || line.starts_with("svn: E"))
    .unwrap_or("")
    .to_owned()
}

/// The client counts and scenarios this run asks for.
fn counts() -> Vec<usize> {
  std::env::var(CONC_N)
    .unwrap_or_else(|_| "2".to_owned())
    .split_whitespace()
    .map(|n| n.parse().expect("a client count"))
    .collect()
}

fn wanted(scenario: &str) -> bool {
  std::env::var(CONC_SCENARIOS).map_or(true, |list| list.split_whitespace().any(|s| s == scenario))
}

/// A free loopback port for a control server: bound, read, let go.
fn free_port() -> u16 {
  std::net::TcpListener::bind("127.0.0.1:0")
    .unwrap()
    .local_addr()
    .unwrap()
    .port()
}

/// A stock server, killed and reaped when dropped.
struct Control(Child);

impl Drop for Control {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

// ------------------------------------------------------------------------------------------------ the slates anchor

/// The anchor process, killed and reaped when dropped.
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

/// A started anchor serving both edges: its instance, the HTTP edge's and the svn:// listener's ports, and a token's id
/// and secret minted with its issuer surface.
struct Slates {
  _anchor: Anchor,
  _store: Scratch,
  instance: String,
  http: u16,
  svn: u16,
  token_id: String,
  token: String,
}

fn slates(instance: &str, env: &[(String, String)], args: &[&str]) -> (i32, String, String) {
  let output = Command::new(env!("CARGO_BIN_EXE_slates"))
    .args(["--instance", instance])
    .args(args)
    .envs(env.iter().map(|(name, value)| (name, value)))
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn value_of(text: &str, key: &str) -> String {
  let prefix = format!("{key}: ");
  text
    .lines()
    .find_map(|line| line.strip_prefix(&prefix))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .trim()
    .to_owned()
}

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

/// Starts an anchor serving the HTTP edge and the svn:// listener on loopback with a changeset store, waits until its
/// daemon answers verbs steadily, makes the root group and mints a token. `None`, skipped loudly, where the platform
/// names no issuer surface.
fn start_slates(name: &str) -> Option<Slates> {
  let instance = format!("{name}-{}", std::process::id());
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
    .args(["--vcs-http", "127.0.0.1:0", "--vcs-svn", "127.0.0.1:0"])
    .args(["--vcs-store", &store.path])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let mut anchor = Anchor { child, drain: None };
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (mut lines, mut issuer, mut served) = (Vec::new(), None, false);
  while !served || issuer.is_none() || port_after(&lines, VCS_SVN_LINE).is_none() {
    let mut line = String::new();
    assert!(
      stderr.read_line(&mut line).unwrap() > 0,
      "the anchor ended before its daemon served"
    );
    let _ = std::io::stderr().write_all(line.as_bytes());
    assert!(
      !line.starts_with(CRASH_LOOP_LINE),
      "the daemon never served: {line}"
    );
    served |= line.starts_with(&serving);
    if let Some(pairs) = line.trim().strip_prefix(ISSUER_SURFACE_LINE) {
      issuer = Some(
        pairs
          .split(' ')
          .filter_map(|pair| pair.split_once('='))
          .map(|(n, v)| (n.to_owned(), v.to_owned()))
          .collect::<Vec<_>>(),
      );
    }
    lines.push(line);
  }
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
  let Some(issuer) = issuer else {
    eprintln!(
      "skipping: the anchor printed no issuer surface, so no separate process can mint a credential"
    );
    return None;
  };
  let (code, out, err) = slates(&instance, &issuer, &["credential", "token"]);
  assert_eq!(code, 0, "credential token: {out}{err}");
  Some(Slates {
    http: port_after(&lines, VCS_HTTP_LINE).expect("the HTTP edge's port"),
    svn: port_after(&lines, VCS_SVN_LINE).expect("the svn:// listener's port"),
    token_id: value_of(&out, "id"),
    token: value_of(&out, "token"),
    instance,
    _anchor: anchor,
    _store: store,
  })
}

fn create_repository(slates_: &Slates, name: &str, tool: &str) {
  let (code, out, err) = slates(
    &slates_.instance,
    &[],
    &["repository", "create", name, "--tool", tool],
  );
  assert_eq!(
    code, 0,
    "repository create {name} --tool {tool}: {out}{err}"
  );
}

// ------------------------------------------------------------------------------------------------ Mercurial

/// Stock hg 7.2's command words, or `None` (skipped loudly by the caller).
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

/// One stock hg client: its command words and the `--config` words its route takes.
#[derive(Clone)]
struct Hg {
  words: Vec<String>,
  config: Vec<String>,
}

impl Hg {
  fn run(&self, dir: &str, args: &[&str]) -> (i32, String, String) {
    let (program, leading) = self.words.split_first().unwrap();
    let all: Vec<String> = leading
      .iter()
      .cloned()
      .chain(args.iter().map(|a| (*a).to_owned()))
      .chain(self.config.iter().cloned())
      .collect();
    run(
      program,
      &all,
      dir,
      &[
        ("HGPLAIN", "1"),
        ("HGRCPATH", ""),
        ("HGUSER", "Test <test@example.com>"),
      ],
    )
  }

  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let (code, out, err) = self.run(dir, args);
    assert_eq!(code, 0, "hg {args:?} in {dir}: {out}{err}");
    out
  }
}

/// What a fresh clone of `url` shows: whether `hg verify` passed, its heads (`branch node`), its bookmarks
/// (`name node`) and its changesets' count.
struct HgState {
  verified: bool,
  heads: Vec<String>,
  bookmarks: BTreeMap<String, String>,
  changesets: usize,
}

fn hg_state(hg: &Hg, base: &str, url: &str, tag: &str) -> HgState {
  let clone = format!("{base}/verify-{tag}");
  hg.ok(base, &["clone", url, &clone]);
  let verified = hg.run(&clone, &["verify"]).0 == 0;
  let heads = hg
    .ok(&clone, &["heads", "--template", "{branch} {node}\\n"])
    .lines()
    .map(str::to_owned)
    .collect();
  let bookmarks = hg
    .ok(&clone, &["bookmarks", "--template", "{bookmark} {node}\\n"])
    .lines()
    .filter_map(|line| line.split_once(' '))
    .map(|(name, node)| (name.to_owned(), node.to_owned()))
    .collect();
  let changesets = hg
    .ok(&clone, &["log", "--template", "{node}\\n"])
    .lines()
    .count();
  HgState {
    verified,
    heads,
    bookmarks,
    changesets,
  }
}

/// A fresh repository at `url` given its first changeset from a working copy in `base`.
fn hg_seed(hg: &Hg, base: &str, url: &str) {
  let seed = format!("{base}/seed");
  hg.ok(base, &["init", &seed]);
  write(&format!("{seed}/file"), "seed\n");
  hg.ok(&seed, &["commit", "--addremove", "--message", "seed"]);
  hg.ok(&seed, &["push", url]);
}

fn hg_clones(hg: &Hg, base: &str, url: &str, n: usize, tag: &str) -> Vec<String> {
  (0..n)
    .map(|i| {
      let copy = format!("{base}/{tag}{i}");
      hg.ok(base, &["clone", url, &copy]);
      copy
    })
    .collect()
}

/// Every clone in `copies` pushes with `args` (and `per` of its index) at once: the results in order.
fn hg_push_at_once(
  hg: &Hg,
  copies: &[String],
  url: &str,
  per: impl Fn(usize) -> Vec<String>,
) -> Vec<(i32, String, String)> {
  at_once(
    copies
      .iter()
      .enumerate()
      .map(|(i, copy)| {
        let (hg, copy, url) = (hg.clone(), copy.clone(), url.to_owned());
        let mut args: Vec<String> = vec!["push".to_owned()];
        args.extend(per(i));
        args.push(url);
        Box::new(move || {
          let refs: Vec<&str> = args.iter().map(String::as_str).collect();
          hg.run(&copy, &refs)
        }) as Box<dyn FnOnce() -> (i32, String, String) + Send>
      })
      .collect(),
  )
}

/// Same head: every clone commits on the one head and pushes at once; stock takes one, refusing the rest in its words. What failed goes to `failed`, what was seen to `seen`.
fn hg_same_head(
  hg: &Hg,
  (base, url, n): (&str, &str, usize),
  failed: &mut Vec<String>,
  seen: &mut Vec<String>,
) {
  let copies = hg_clones(hg, base, url, n, "same");
  for (i, copy) in copies.iter().enumerate() {
    append(&format!("{copy}/file"), &format!("same {i}\n"));
    hg.ok(copy, &["commit", "--message", &format!("same {i}")]);
  }
  let pushed = hg_push_at_once(hg, &copies, url, |_| Vec::new());
  let accepted = pushed.iter().filter(|r| r.0 == 0).count();
  let refusals: BTreeSet<String> = pushed
    .iter()
    .filter(|r| r.0 != 0)
    .map(|r| refusal_line(&r.1, &r.2))
    .collect();
  let state = hg_state(hg, base, url, "same");
  seen.push(format!(
    "same-head: {accepted} of {n} accepted, refusals {refusals:?}, verify {}, {} heads, {} changesets",
    state.verified,
    state.heads.len(),
    state.changesets
  ));
  if accepted != 1 {
    failed.push(format!(
      "same-head: {accepted} of {n} pushes accepted, one expected"
    ));
  }
  for refusal in &refusals {
    if !HG_HEAD_RACE_REFUSALS
      .iter()
      .any(|words| refusal.starts_with(words))
    {
      failed.push(format!(
        "same-head: a refusal not in stock's words: {refusal:?}"
      ));
    }
  }
  if !state.verified || state.heads.len() != 1 || state.changesets != 1 + accepted {
    failed.push(format!(
      "same-head: verify {}, {} heads (one expected), {} changesets ({} expected)",
      state.verified,
      state.heads.len(),
      state.changesets,
      1 + accepted
    ));
  }
}

/// Branches: every clone commits on a branch of its own and pushes it at once; every branch lands. What failed goes to `failed`, what was seen to `seen`.
fn hg_branches(
  hg: &Hg,
  (base, url, n): (&str, &str, usize),
  failed: &mut Vec<String>,
  seen: &mut Vec<String>,
) {
  let copies = hg_clones(hg, base, url, n, "branch");
  for (i, copy) in copies.iter().enumerate() {
    hg.ok(copy, &["branch", &format!("b{i}")]);
    write(&format!("{copy}/b{i}"), &format!("branch {i}\n"));
    hg.ok(
      copy,
      &["commit", "--addremove", "--message", &format!("branch {i}")],
    );
  }
  let pushed = hg_push_at_once(hg, &copies, url, |_| vec!["--new-branch".to_owned()]);
  let accepted = pushed.iter().filter(|r| r.0 == 0).count();
  let state = hg_state(hg, base, url, "branch");
  let landed: BTreeSet<String> = state
    .heads
    .iter()
    .filter_map(|head| head.split(' ').next())
    .map(str::to_owned)
    .collect();
  let missing: Vec<String> = (0..n)
    .map(|i| format!("b{i}"))
    .filter(|branch| !landed.contains(branch))
    .collect();
  let refusals: BTreeSet<String> = pushed
    .iter()
    .filter(|r| r.0 != 0)
    .map(|r| refusal_line(&r.1, &r.2))
    .collect();
  seen.push(format!(
    "branches: {accepted} of {n} accepted, refusals {refusals:?}, verify {}, missing {missing:?}",
    state.verified
  ));
  if accepted != n || !missing.is_empty() || !state.verified {
    failed.push(format!(
      "branches: {accepted} of {n} accepted (all expected), missing {missing:?}, verify {}, refusals {refusals:?}",
      state.verified
    ));
  }
}

/// Bookmarks: every clone commits on a branch of its own with a bookmark and pushes both at once; every bookmark names its node. What failed goes to `failed`, what was seen to `seen`.
fn hg_bookmarks(
  hg: &Hg,
  (base, url, n): (&str, &str, usize),
  failed: &mut Vec<String>,
  seen: &mut Vec<String>,
) {
  let copies = hg_clones(hg, base, url, n, "mark");
  let mut nodes = BTreeMap::new();
  for (i, copy) in copies.iter().enumerate() {
    hg.ok(copy, &["branch", &format!("m{i}")]);
    write(&format!("{copy}/m{i}"), &format!("mark {i}\n"));
    hg.ok(
      copy,
      &["commit", "--addremove", "--message", &format!("mark {i}")],
    );
    hg.ok(copy, &["bookmark", &format!("k{i}")]);
    nodes.insert(
      format!("k{i}"),
      hg.ok(copy, &["log", "-r", ".", "--template", "{node}"]),
    );
  }
  let pushed = hg_push_at_once(hg, &copies, url, |i| {
    vec!["--new-branch".to_owned(), "-B".to_owned(), format!("k{i}")]
  });
  let accepted = pushed.iter().filter(|r| r.0 == 0).count();
  let state = hg_state(hg, base, url, "mark");
  let wrong: Vec<&String> = nodes
    .iter()
    .filter(|(name, node)| state.bookmarks.get(*name) != Some(*node))
    .map(|(name, _)| name)
    .collect();
  seen.push(format!(
    "bookmarks: {accepted} of {n} accepted, verify {}, wrong or missing {wrong:?}",
    state.verified
  ));
  if accepted != n || !wrong.is_empty() || !state.verified {
    failed.push(format!(
      "bookmarks: {accepted} of {n} accepted (all expected), wrong or missing {wrong:?}, verify {}",
      state.verified
    ));
  }
}

/// The Mercurial scenarios against the repository at `url`, its first changeset already pushed: a list of what failed
/// (empty when every expectation held), and what was seen, for the report.
fn hg_scenarios(hg: &Hg, base: &str, url: &str, n: usize) -> (Vec<String>, Vec<String>) {
  let (mut failed, mut seen) = (Vec::new(), Vec::new());
  if wanted("same-head") {
    hg_same_head(hg, (base, url, n), &mut failed, &mut seen);
  }
  if wanted("branches") {
    hg_branches(hg, (base, url, n), &mut failed, &mut seen);
  }
  if wanted("bookmarks") {
    hg_bookmarks(hg, (base, url, n), &mut failed, &mut seen);
  }
  (failed, seen)
}

/// Goal 93, Mercurial: N stock hg 7.2 clients at once (N from [`CONC_N`]), against stock `hg serve` and then slates'
/// HTTP edge, three scenarios ([`hg_scenarios`]). Expect every expectation to hold against `hg serve` (the control:
/// a failure there is the test's, and fails it as such) and against slates. Red: the line naming the scenario, N and
/// what differed, `slates` in it.
#[test]
#[ignore = "an interop round main runs: SLATES_CONC_N='2 10 50' cargo test --release -p slates-cli --test vcs_concurrency -- --ignored concurrent_stock_hg --nocapture"]
fn concurrent_stock_hg_pushes_hold() {
  let Some(words) = stock_hg() else {
    eprintln!("skipping the hg concurrency round: no Mercurial 7.2 here (set {STOCK_HG})");
    return;
  };
  let Some(slates_) = start_slates("hg-conc") else {
    return;
  };
  let mut failures = Vec::new();
  for n in counts() {
    let scratch = Scratch::new(&format!("hg-conc-{n}"));
    // The control: stock hg serve, anonymous pushes allowed over plain HTTP.
    let control_repo = format!("{}/control-repo", scratch.path);
    let control_hg = Hg {
      words: words.clone(),
      config: Vec::new(),
    };
    control_hg.ok(&scratch.path, &["init", &control_repo]);
    let port = free_port();
    let (program, leading) = words.split_first().unwrap();
    let _control = Control(
      Command::new(program)
        .args(leading)
        .args([
          "serve",
          "-R",
          &control_repo,
          "-p",
          &port.to_string(),
          "-a",
          "127.0.0.1",
        ])
        .args([
          "--config",
          "web.push_ssl=False",
          "--config",
          "web.allow_push=*",
        ])
        .env("HGPLAIN", "1")
        .env("HGRCPATH", "")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(CONTROL_START);
    let control_url = format!("http://127.0.0.1:{port}/");
    let control_base = format!("{}/control", scratch.path);
    made_directory(&control_base);
    hg_seed(&control_hg, &control_base, &control_url);
    let (control_failed, control_seen) = hg_scenarios(&control_hg, &control_base, &control_url, n);
    // slates.
    let name = format!("conc{n}");
    create_repository(&slates_, &name, "hg");
    let prefix = format!("http://127.0.0.1:{}/", slates_.http);
    let slates_hg = Hg {
      words: words.clone(),
      config: [
        format!("auth.s.prefix={prefix}"),
        format!("auth.s.username={USER}"),
        format!("auth.s.password={}", slates_.token),
      ]
      .into_iter()
      .flat_map(|item| ["--config".to_owned(), item])
      .collect(),
    };
    let slates_base = format!("{}/slates", scratch.path);
    made_directory(&slates_base);
    let url = format!("{prefix}{name}");
    hg_seed(&slates_hg, &slates_base, &url);
    let (slates_failed, slates_seen) = hg_scenarios(&slates_hg, &slates_base, &url, n);
    for line in control_seen {
      eprintln!("CONC hg N={n} hg serve: {line}");
    }
    for line in slates_seen {
      eprintln!("CONC hg N={n} slates: {line}");
    }
    failures.extend(
      control_failed
        .into_iter()
        .map(|f| format!("N={n} hg serve (the control): {f}")),
    );
    failures.extend(
      slates_failed
        .into_iter()
        .map(|f| format!("N={n} slates: {f}")),
    );
  }
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ------------------------------------------------------------------------------------------------ Subversion

fn stock_svn() -> bool {
  ["svn", "svnserve", "svnadmin", "svnrdump"]
    .iter()
    .all(|tool| {
      Command::new(tool)
        .arg("--version")
        .arg("--quiet")
        .output()
        .is_ok_and(|out| {
          out.status.success() && String::from_utf8_lossy(&out.stdout).starts_with("1.15")
        })
    })
}

/// One stock svn client: the options every command takes (its configuration directory and credentials).
#[derive(Clone)]
struct Svn {
  options: Vec<String>,
}

impl Svn {
  fn run(&self, dir: &str, args: &[&str]) -> (i32, String, String) {
    let all: Vec<String> = self
      .options
      .iter()
      .cloned()
      .chain(args.iter().map(|a| (*a).to_owned()))
      .collect();
    run("svn", &all, dir, &[("LC_ALL", "C")])
  }

  fn ok(&self, dir: &str, args: &[&str]) -> String {
    let (code, out, err) = self.run(dir, args);
    assert_eq!(code, 0, "svn {args:?} in {dir}: {out}{err}");
    out
  }
}

/// Every working copy in `copies` commits at once: the results in order.
fn svn_commit_at_once(svn: &Svn, copies: &[String]) -> Vec<(i32, String, String)> {
  at_once(
    copies
      .iter()
      .map(|copy| {
        let (svn, copy) = (svn.clone(), copy.clone());
        Box::new(move || svn.run(&copy, &["commit", "--message", "concurrent"]))
          as Box<dyn FnOnce() -> (i32, String, String) + Send>
      })
      .collect(),
  )
}

/// The Subversion scenarios against the empty repository at `url`: what failed and what was seen.
fn svn_scenarios(
  svn: &Svn,
  base: &str,
  url: &str,
  n: usize,
  credentials: &[String],
) -> (Vec<String>, Vec<String>) {
  let (mut failed, mut seen) = (Vec::new(), Vec::new());
  let first = format!("{base}/first");
  svn.ok(base, &["checkout", url, &first]);
  write(&format!("{first}/file"), "seed\n");
  svn.ok(&first, &["add", "file"]);
  svn.ok(&first, &["commit", "--message", "seed"]);
  let mut accepted = 1;
  if wanted("same-file") {
    let copies: Vec<String> = (0..n)
      .map(|i| {
        let copy = format!("{base}/same{i}");
        svn.ok(base, &["checkout", url, &copy]);
        append(&format!("{copy}/file"), &format!("same {i}\n"));
        copy
      })
      .collect();
    let committed = svn_commit_at_once(svn, &copies);
    let won = committed.iter().filter(|r| r.0 == 0).count();
    accepted += won;
    let refusals: BTreeSet<String> = committed
      .iter()
      .filter(|r| r.0 != 0)
      .map(|r| refusal_line(&r.1, &r.2))
      .collect();
    seen.push(format!(
      "same-file: {won} of {n} accepted, refusals {refusals:?}"
    ));
    if won != 1 {
      failed.push(format!(
        "same-file: {won} of {n} commits accepted, one expected"
      ));
    }
    for refusal in &refusals {
      if !SVN_OUT_OF_DATE.iter().any(|code| refusal.contains(code)) {
        failed.push(format!(
          "same-file: a refusal other than out of date: {refusal:?}"
        ));
      }
    }
  }
  if wanted("paths") {
    let copies: Vec<String> = (0..n)
      .map(|i| {
        let copy = format!("{base}/path{i}");
        svn.ok(base, &["checkout", url, &copy]);
        write(&format!("{copy}/p{i}"), &format!("path {i}\n"));
        svn.ok(&copy, &["add", &format!("p{i}")]);
        copy
      })
      .collect();
    let committed = svn_commit_at_once(svn, &copies);
    let landed = committed.iter().filter(|r| r.0 == 0).count();
    accepted += landed;
    let refusals: BTreeSet<String> = committed
      .iter()
      .filter(|r| r.0 != 0)
      .map(|r| refusal_line(&r.1, &r.2))
      .collect();
    seen.push(format!(
      "paths: {landed} of {n} accepted, refusals {refusals:?}"
    ));
    if landed != n {
      failed.push(format!(
        "paths: {landed} of {n} commits accepted, all expected: {refusals:?}"
      ));
    }
  }
  // The history: revisions without gaps, one a commit accepted, and a replay that svnadmin verifies.
  let log = svn.ok(base, &["log", "--quiet", url]);
  let mut revisions: Vec<u64> = log
    .lines()
    .filter_map(|line| line.strip_prefix('r')?.split(' ').next()?.parse().ok())
    .collect();
  revisions.sort_unstable();
  let head = revisions.last().copied().unwrap_or(0);
  let gapless = revisions == (1..=head).collect::<Vec<u64>>();
  let replay = format!("{base}/replayed");
  let dump = run(
    "sh",
    &[
      "-c".to_owned(),
      format!(
        "svnrdump dump --quiet --non-interactive --no-auth-cache {} '{url}' > '{base}/dump' && svnadmin create \
         '{replay}' && svnadmin load --quiet '{replay}' < '{base}/dump' && svnadmin verify --quiet '{replay}'",
        credentials
          .iter()
          .map(|word| format!("'{word}'"))
          .collect::<Vec<_>>()
          .join(" ")
      ),
    ],
    base,
    &[("LC_ALL", "C")],
  );
  seen.push(format!(
    "history: head r{head}, gapless {gapless}, {accepted} commits accepted, dump-load-verify exit {}",
    dump.0
  ));
  if !gapless || u64::try_from(accepted).unwrap() != head {
    failed.push(format!(
      "history: revisions {revisions:?}, gapless {gapless}, head r{head} for {accepted} commits accepted"
    ));
  }
  if dump.0 != 0 {
    failed.push(format!(
      "history: dump, load or verify failed: {}{}",
      dump.1, dump.2
    ));
  }
  (failed, seen)
}

/// Goal 93, Subversion: N stock svn 1.15 clients at once (N from [`CONC_N`]), against stock `svnserve` and then slates'
/// svn:// listener, the scenarios of [`svn_scenarios`]. Expect every expectation to hold against svnserve (the control)
/// and against slates. Red: the line naming the scenario, N and what differed, `slates` in it.
#[test]
#[ignore = "an interop round main runs: SLATES_CONC_N='2 10 50' cargo test --release -p slates-cli --test vcs_concurrency -- --ignored concurrent_stock_svn --nocapture"]
fn concurrent_stock_svn_commits_hold() {
  if !stock_svn() {
    eprintln!(
      "skipping the svn concurrency round: no stock Subversion 1.15 (svn, svnserve, svnadmin, svnrdump) here"
    );
    return;
  }
  let Some(slates_) = start_slates("svn-conc") else {
    return;
  };
  let mut failures = Vec::new();
  for n in counts() {
    let scratch = Scratch::new(&format!("svn-conc-{n}"));
    let config = format!("{}/config", scratch.path);
    made_directory(&config);
    let base_options = vec![
      "--non-interactive".to_owned(),
      "--no-auth-cache".to_owned(),
      "--config-dir".to_owned(),
      config,
    ];
    // The control: stock svnserve over a scratch FSFS repository, anonymous writes allowed.
    let roots = format!("{}/roots", scratch.path);
    made_directory(&roots);
    let control_repo = format!("{roots}/project");
    assert_eq!(
      run(
        "svnadmin",
        &["create".to_owned(), control_repo.clone()],
        &scratch.path,
        &[]
      )
      .0,
      0,
      "svnadmin create"
    );
    write(
      &format!("{control_repo}/conf/svnserve.conf"),
      "[general]\nanon-access = write\n",
    );
    let port = free_port();
    let _control = Control(
      Command::new("svnserve")
        .args(["-d", "--foreground", "--listen-host", "127.0.0.1"])
        .args(["--listen-port", &port.to_string(), "-r", &roots])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(CONTROL_START);
    let control_base = format!("{}/control", scratch.path);
    made_directory(&control_base);
    let control_svn = Svn {
      options: base_options.clone(),
    };
    let (control_failed, control_seen) = svn_scenarios(
      &control_svn,
      &control_base,
      &format!("svn://127.0.0.1:{port}/project"),
      n,
      &[],
    );
    // slates.
    let name = format!("conc{n}");
    create_repository(&slates_, &name, "svn");
    let credentials = vec![
      "--username".to_owned(),
      slates_.token_id.clone(),
      "--password".to_owned(),
      slates_.token.clone(),
    ];
    let slates_svn = Svn {
      options: base_options
        .iter()
        .cloned()
        .chain(credentials.iter().cloned())
        .collect(),
    };
    let slates_base = format!("{}/slates", scratch.path);
    made_directory(&slates_base);
    let (slates_failed, slates_seen) = svn_scenarios(
      &slates_svn,
      &slates_base,
      &format!("svn://127.0.0.1:{}/{name}", slates_.svn),
      n,
      &credentials,
    );
    for line in control_seen {
      eprintln!("CONC svn N={n} svnserve: {line}");
    }
    for line in slates_seen {
      eprintln!("CONC svn N={n} slates: {line}");
    }
    failures.extend(
      control_failed
        .into_iter()
        .map(|f| format!("N={n} svnserve (the control): {f}")),
    );
    failures.extend(
      slates_failed
        .into_iter()
        .map(|f| format!("N={n} slates: {f}")),
    );
  }
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}
