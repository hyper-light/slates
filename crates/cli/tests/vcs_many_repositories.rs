//! Goal 93 correctness for one server serving many repositories (A-724, §4.17.5; main's order of 2026-11-21): N git, N
//! Mercurial and N Subversion repositories on one daemon at once, stock clients pushing and fetching all of them
//! concurrently, and each repository then seen to hold exactly its own history.
//!
//! **What each repository is given.** Repository `i` of a tool gets two pushes from its own client: the first a
//! changeset holding `who.txt` (its own name, the marker no other repository has) and `shared.txt` (the same bytes in
//! every repository of every tool, so content is shared by identity across them), with a branch and a tag of its own
//! name (git: `b-<i>` and `t-<i>`; hg: the bookmark `bm-<i>` and the tag `t-<i>`); the second a changeset adding
//! `two.txt`. Subversion has no branches or tags here: its two commits are revisions 1 and 2.
//!
//! **The phases.** (1) Every repository's first push, all at once. (2) All at once again: every repository's second
//! push; a fetch of every repository (a fresh clone, or checkout) beside it; for each tool, a repository created and
//! given its first push (`late-<tool>`); and for each tool, a repository given its first push before the load and
//! destroyed during it (`doomed-<tool>`, by `slates repository destroy`). (3) Each repository checked alone, against
//! the history its own client made: git `fsck --strict`, every commit and exactly its refs (`ls-remote`: `main`, its
//! branch, its tag, at its client's ids), `who.txt` its own; hg `hg verify`, exactly its changesets, its bookmark and
//! its tag; svn exactly two revisions with its own log messages and paths, and its history, replayed by `svnrdump dump`
//! into a local repository, passing `svnadmin verify`. The late repository is checked the same way; the destroyed one
//! must be gone (a fetch refused) while every other repository is whole.
//!
//! **A measurement main runs** (CLAUDE.md §8): ignored, its rounds in `run-requests.md`. N from `SLATES_MANY_N`
//! (spaces between; default `2`), the tools from `SLATES_MANY_TOOLS` (default `git hg svn`), a fresh daemon each N.
//! A tool whose stock client is missing (git; hg 7.2 as `SLATES_TEST_HG` names it; svn, svnrdump and svnadmin 1.15)
//! is skipped loudly. The harness is `vcs_concurrency.rs`'s.
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

use std::collections::BTreeSet;
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Barrier;
use std::time::{Duration, Instant};

/// Shape: the shards of the test's daemon, as `vcs_concurrency.rs` starts one.
const SHARDS: &str = "2";
/// Shape: consecutive answered verbs before the daemon counts as settled (`hg_interop.rs`).
const STABLE_STREAK: u32 = 10;
/// Shape: a guard against a client or server that never answers, far above what any command here takes.
const PATIENCE: Duration = Duration::from_secs(300);
/// Shape: the pause between polls.
const POLL: Duration = Duration::from_millis(20);
/// Shape: the variable naming the repository counts a tool, spaces between.
const MANY_N: &str = "SLATES_MANY_N";
/// Shape: the variable naming the tools, spaces between (default all three).
const MANY_TOOLS: &str = "SLATES_MANY_TOOLS";
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
/// Shape: the lines of `shared.txt`, the file every repository holds byte for byte (so its content is one identity):
/// enough to span more than one of the store's windows.
const SHARED_LINES: usize = 4096;
/// Format: the fixed instants (seconds since the epoch) the changesets are made at, so a client's ids are its own
/// content's alone.
const FIRST_AT: u64 = 1_700_000_000;
/// Format: the second changeset's instant.
const SECOND_AT: u64 = 1_700_000_060;

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
}

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
}

// ------------------------------------------------------------------------------------------------ the clients

/// A tool the test drives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
  Git,
  Hg,
  Svn,
}

impl Tool {
  fn name(self) -> &'static str {
    match self {
      Tool::Git => "git",
      Tool::Hg => "hg",
      Tool::Svn => "svn",
    }
  }
}

/// Everything a client of any tool needs: the daemon's ports and token, stock hg's words, and the scratch it works in.
struct Clients<'s> {
  slates: &'s Slates,
  hg: Option<Vec<String>>,
  base: String,
}

/// The text of `shared.txt`, the same in every repository.
fn shared_text() -> String {
  (0..SHARED_LINES)
    .map(|line| format!("shared line {line}: the same bytes in every repository of every tool\n"))
    .collect()
}

/// A result's failure, named: `what` and the command's output, or nothing.
fn failed(what: &str, (code, out, err): (i32, String, String)) -> Result<String, String> {
  if code == 0 {
    Ok(out)
  } else {
    Err(format!(
      "{what}: exit {code}: {} {}",
      out.trim(),
      err.trim()
    ))
  }
}

impl Clients<'_> {
  fn url(&self, tool: Tool, name: &str) -> String {
    match tool {
      Tool::Git | Tool::Hg => format!("http://127.0.0.1:{}/{name}", self.slates.http),
      Tool::Svn => format!("svn://127.0.0.1:{}/{name}", self.slates.svn),
    }
  }

  fn home(&self) -> String {
    let home = format!("{}/home", self.base);
    made_directory(&home);
    home
  }

  /// Stock git with no configuration of the user's or the system's, never prompting, one committer at instant `at`,
  /// and the token as a bearer credential.
  fn git(&self, dir: &str, args: &[&str], at: u64) -> (i32, String, String) {
    let header = format!(
      "http.extraHeader=Authorization: Bearer {}",
      self.slates.token
    );
    let all: Vec<String> = ["-c", &header]
      .iter()
      .chain(args)
      .map(|word| (*word).to_owned())
      .collect();
    let home = self.home();
    let date = format!("{at} +0000");
    run(
      "git",
      &all,
      dir,
      &[
        ("HOME", &home),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        // Configuration a parent passes through the environment (signing, here) is not the test's.
        ("GIT_CONFIG_PARAMETERS", ""),
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ("GIT_AUTHOR_DATE", &date),
        ("GIT_COMMITTER_DATE", &date),
      ],
    )
  }

  fn hg_client(&self) -> Hg {
    let port = self.slates.http;
    Hg {
      words: self.hg.clone().unwrap_or_default(),
      config: vec![
        "--config".to_owned(),
        format!("auth.s.prefix=http://127.0.0.1:{port}/"),
        "--config".to_owned(),
        format!("auth.s.username={USER}"),
        "--config".to_owned(),
        format!("auth.s.password={}", self.slates.token),
      ],
    }
  }

  fn svn_client(&self) -> Svn {
    let config = format!("{}/svn-config", self.base);
    made_directory(&config);
    Svn {
      options: vec![
        "--non-interactive".to_owned(),
        "--no-auth-cache".to_owned(),
        "--config-dir".to_owned(),
        config,
        "--username".to_owned(),
        self.slates.token_id.clone(),
        "--password".to_owned(),
        self.slates.token.clone(),
      ],
    }
  }

  /// The local copy repository `name`'s client works in.
  fn local(&self, name: &str) -> String {
    format!("{}/{name}-local", self.base)
  }

  /// Repository `name`'s first push: its marker, the shared file, its branch and tag (module docs).
  fn push_first(&self, tool: Tool, name: &str) -> Result<(), String> {
    let (url, local) = (self.url(tool, name), self.local(name));
    let at = FIRST_AT.to_string();
    match tool {
      Tool::Git => {
        failed(
          "git init",
          self.git(&self.base, &["init", "-q", "-b", "main", &local], FIRST_AT),
        )?;
        write(&format!("{local}/who.txt"), &format!("{name}\n"));
        write(&format!("{local}/shared.txt"), &shared_text());
        failed("git add", self.git(&local, &["add", "-A"], FIRST_AT))?;
        failed(
          "git commit",
          self.git(
            &local,
            &["commit", "-q", "-m", &format!("{name} one")],
            FIRST_AT,
          ),
        )?;
        failed(
          "git branch",
          self.git(&local, &["branch", &format!("b-{name}")], FIRST_AT),
        )?;
        failed(
          "git tag",
          self.git(&local, &["tag", &format!("t-{name}")], FIRST_AT),
        )?;
        let tag = format!("refs/tags/t-{name}");
        failed(
          &format!("git push {name}"),
          self.git(
            &local,
            &["push", "-q", &url, "main", &format!("b-{name}"), &tag],
            FIRST_AT,
          ),
        )?;
      }
      Tool::Hg => {
        let hg = self.hg_client();
        failed("hg init", hg.run(&self.base, &["init", &local]))?;
        write(&format!("{local}/who.txt"), &format!("{name}\n"));
        write(&format!("{local}/shared.txt"), &shared_text());
        let date = format!("{at} 0");
        failed(
          "hg commit",
          hg.run(
            &local,
            &["commit", "-A", "-d", &date, "-m", &format!("{name} one")],
          ),
        )?;
        failed(
          "hg tag",
          hg.run(&local, &["tag", "-d", &date, &format!("t-{name}")]),
        )?;
        failed(
          "hg bookmark",
          hg.run(&local, &["bookmark", &format!("bm-{name}")]),
        )?;
        failed(
          &format!("hg push {name}"),
          hg.run(&local, &["push", "-B", &format!("bm-{name}"), &url]),
        )?;
      }
      Tool::Svn => {
        let svn = self.svn_client();
        failed(
          &format!("svn checkout {name}"),
          svn.run(&self.base, &["checkout", &url, &local]),
        )?;
        write(&format!("{local}/who.txt"), &format!("{name}\n"));
        write(&format!("{local}/shared.txt"), &shared_text());
        failed("svn add", svn.run(&local, &["add", "--force", "."]))?;
        failed(
          &format!("svn commit {name}"),
          svn.run(&local, &["commit", "-m", &format!("{name} one")]),
        )?;
      }
    }
    Ok(())
  }

  /// Repository `name`'s second push: `two.txt` added on its main line.
  fn push_second(&self, tool: Tool, name: &str) -> Result<(), String> {
    let (url, local) = (self.url(tool, name), self.local(name));
    write(&format!("{local}/two.txt"), &format!("{name} two\n"));
    match tool {
      Tool::Git => {
        failed("git add", self.git(&local, &["add", "-A"], SECOND_AT))?;
        failed(
          "git commit",
          self.git(
            &local,
            &["commit", "-q", "-m", &format!("{name} two")],
            SECOND_AT,
          ),
        )?;
        failed(
          &format!("git push {name} two"),
          self.git(&local, &["push", "-q", &url, "main"], SECOND_AT),
        )?;
      }
      Tool::Hg => {
        let hg = self.hg_client();
        let date = format!("{SECOND_AT} 0");
        failed(
          "hg commit",
          hg.run(
            &local,
            &["commit", "-A", "-d", &date, "-m", &format!("{name} two")],
          ),
        )?;
        failed(
          &format!("hg push {name} two"),
          hg.run(&local, &["push", &url]),
        )?;
      }
      Tool::Svn => {
        let svn = self.svn_client();
        failed("svn add", svn.run(&local, &["add", "two.txt"]))?;
        failed(
          &format!("svn commit {name} two"),
          svn.run(&local, &["commit", "-m", &format!("{name} two")]),
        )?;
      }
    }
    Ok(())
  }

  /// A fresh copy of repository `name` into a directory tagged `tag`: its path, or the refusal.
  fn fetch(&self, tool: Tool, name: &str, tag: &str) -> Result<String, String> {
    let (url, copy) = (self.url(tool, name), format!("{}/{name}-{tag}", self.base));
    match tool {
      Tool::Git => failed(
        &format!("git clone {name}"),
        self.git(&self.base, &["clone", "-q", &url, &copy], FIRST_AT),
      )?,
      Tool::Hg => failed(
        &format!("hg clone {name}"),
        self
          .hg_client()
          .run(&self.base, &["clone", "-U", &url, &copy]),
      )?,
      Tool::Svn => failed(
        &format!("svn checkout {name}"),
        self
          .svn_client()
          .run(&self.base, &["checkout", &url, &copy]),
      )?,
    };
    Ok(copy)
  }

  /// Repository `name` checked alone against the history its client made (module docs): its failures.
  fn check(&self, tool: Tool, name: &str) -> Vec<String> {
    let copy = match self.fetch(tool, name, "checked") {
      Ok(copy) => copy,
      Err(refusal) => return vec![refusal],
    };
    let result = match tool {
      Tool::Git => self.check_git(name, &copy),
      Tool::Hg => self.check_hg(name, &copy),
      Tool::Svn => self.check_svn(name, &copy),
    };
    result.err().into_iter().collect()
  }

  fn check_git(&self, name: &str, copy: &str) -> Result<(), String> {
    let local = self.local(name);
    failed(
      &format!("git fsck --strict {name}"),
      self.git(copy, &["fsck", "--strict", "--no-progress"], FIRST_AT),
    )?;
    let commits = |dir: &str| -> Result<BTreeSet<String>, String> {
      Ok(
        failed(
          "git rev-list",
          self.git(dir, &["rev-list", "--all"], FIRST_AT),
        )?
        .lines()
        .map(str::to_owned)
        .collect(),
      )
    };
    let (theirs, ours) = (commits(copy)?, commits(&local)?);
    if theirs != ours {
      return Err(format!(
        "{name}: the server's commits {theirs:?} are not its client's {ours:?}"
      ));
    }
    let wanted: BTreeSet<String> = [
      "refs/heads/main".to_owned(),
      format!("refs/heads/b-{name}"),
      format!("refs/tags/t-{name}"),
    ]
    .into_iter()
    .collect();
    let local_refs: BTreeSet<String> =
      failed("git show-ref", self.git(&local, &["show-ref"], FIRST_AT))?
        .lines()
        .filter(|line| {
          line
            .split(' ')
            .nth(1)
            .is_some_and(|reference| wanted.contains(reference))
        })
        .map(|line| line.replacen(' ', "\t", 1))
        .collect();
    let remote_refs: BTreeSet<String> = failed(
      "git ls-remote",
      self.git(&local, &["ls-remote", &self.url(Tool::Git, name)], FIRST_AT),
    )?
    .lines()
    .filter(|line| !line.ends_with("\tHEAD"))
    .map(str::to_owned)
    .collect();
    if remote_refs != local_refs {
      return Err(format!(
        "{name}: the server's refs {remote_refs:?} are not exactly its client's {local_refs:?}"
      ));
    }
    let who = failed(
      "git show who.txt",
      self.git(copy, &["show", "main:who.txt"], FIRST_AT),
    )?;
    if who != format!("{name}\n") {
      return Err(format!("{name}: who.txt reads {who:?}"));
    }
    Ok(())
  }

  fn check_hg(&self, name: &str, copy: &str) -> Result<(), String> {
    let (hg, local) = (self.hg_client(), self.local(name));
    failed(&format!("hg verify {name}"), hg.run(copy, &["verify"]))?;
    let listed = |dir: &str, args: &[&str]| -> Result<BTreeSet<String>, String> {
      Ok(
        failed("hg", hg.run(dir, args))?
          .lines()
          .filter(|line| !line.starts_with("tip "))
          .map(str::to_owned)
          .collect(),
      )
    };
    for what in [
      &["log", "-r", "all()", "-T", "{node} {p1.node} {branch}\\n"][..],
      &["bookmarks", "-T", "{bookmark} {node}\\n"][..],
      &["tags", "-T", "{tag} {node}\\n"][..],
    ] {
      let (theirs, ours) = (listed(copy, what)?, listed(&local, what)?);
      if theirs != ours {
        return Err(format!(
          "{name}: hg {} on the server {theirs:?}, its client's {ours:?}",
          what[0]
        ));
      }
    }
    let who = failed(
      "hg cat who.txt",
      hg.run(copy, &["cat", "-r", "default", "who.txt"]),
    )?;
    if who != format!("{name}\n") {
      return Err(format!("{name}: who.txt reads {who:?}"));
    }
    Ok(())
  }

  fn check_svn(&self, name: &str, copy: &str) -> Result<(), String> {
    let (svn, url) = (self.svn_client(), self.url(Tool::Svn, name));
    let head = failed(
      "svn info",
      svn.run(copy, &["info", "--show-item", "revision", &url]),
    )?;
    if head.trim() != "2" {
      return Err(format!(
        "{name}: the youngest revision is {}, its client made 2",
        head.trim()
      ));
    }
    let log = failed(
      "svn log",
      svn.run(
        copy,
        &["log", "-q", "--xml", "--with-revprop", "svn:log", &url],
      ),
    )?;
    for message in [format!("{name} one"), format!("{name} two")] {
      if !log.contains(&format!(">{message}<")) {
        return Err(format!("{name}: no revision says {message:?} in {log}"));
      }
    }
    let paths = failed("svn ls", svn.run(copy, &["ls", "-R", &url]))?;
    if paths.lines().collect::<Vec<_>>() != ["shared.txt", "two.txt", "who.txt"] {
      return Err(format!("{name}: the paths are {paths:?}"));
    }
    let who = failed(
      "svn cat",
      svn.run(copy, &["cat", &format!("{url}/who.txt")]),
    )?;
    if who != format!("{name}\n") {
      return Err(format!("{name}: who.txt reads {who:?}"));
    }
    let (dump, replay) = (
      format!("{}/{name}.dump", self.base),
      format!("{}/{name}-replay", self.base),
    );
    let mut options = svn.options.clone();
    options.extend([
      "dump".to_owned(),
      "-q".to_owned(),
      url.clone(),
      "--file".to_owned(),
      dump.clone(),
    ]);
    failed(
      "svnrdump dump",
      run("svnrdump", &options, &self.base, &[("LC_ALL", "C")]),
    )?;
    failed(
      "svnadmin create",
      run(
        "svnadmin",
        &["create".to_owned(), replay.clone()],
        &self.base,
        &[],
      ),
    )?;
    failed(
      "svnadmin load",
      run(
        "svnadmin",
        &[
          "load".to_owned(),
          "-q".to_owned(),
          replay.clone(),
          "--file".to_owned(),
          dump,
        ],
        &self.base,
        &[],
      ),
    )?;
    failed(
      &format!("svnadmin verify {name}"),
      run(
        "svnadmin",
        &["verify".to_owned(), "-q".to_owned(), replay],
        &self.base,
        &[],
      ),
    )?;
    Ok(())
  }
}

/// The tools this run asks for whose stock clients are here; each one missing is skipped loudly.
fn tools(hg: Option<&Vec<String>>) -> Vec<Tool> {
  let asked = std::env::var(MANY_TOOLS).unwrap_or_else(|_| "git hg svn".to_owned());
  let mut tools = Vec::new();
  for word in asked.split_whitespace() {
    let (tool, here) = match word {
      "git" => (
        Tool::Git,
        Command::new("git")
          .arg("--version")
          .output()
          .is_ok_and(|out| out.status.success()),
      ),
      "hg" => (Tool::Hg, hg.is_some()),
      "svn" => (Tool::Svn, stock_svn()),
      other => panic!("{MANY_TOOLS} names {other:?}: git, hg or svn"),
    };
    if here {
      tools.push(tool);
    } else {
      eprintln!("skipping {word} in the many-repositories round: its stock client is not here");
    }
  }
  tools
}

fn counts() -> Vec<usize> {
  std::env::var(MANY_N)
    .unwrap_or_else(|_| "2".to_owned())
    .split_whitespace()
    .map(|n| n.parse().expect("a repository count"))
    .collect()
}

/// A repository created through the CLI, or its refusal.
fn created(slates_: &Slates, name: &str, tool: Tool) -> Result<(), String> {
  failed(
    &format!("repository create {name}"),
    slates(
      &slates_.instance,
      &[],
      &["repository", "create", name, "--tool", tool.name()],
    ),
  )
  .map(drop)
}

/// One round at `n` repositories a tool on a fresh daemon (module docs): its failures.
fn round(n: usize, tools: &[Tool], hg: Option<&Vec<String>>) -> Vec<String> {
  let Some(slates_) = start_slates(&format!("many-{n}")) else {
    return Vec::new();
  };
  let scratch = Scratch::new(&format!("many-{n}"));
  let clients = Clients {
    slates: &slates_,
    hg: hg.cloned(),
    base: scratch.path.clone(),
  };
  let names =
    |tool: Tool| -> Vec<String> { (0..n).map(|i| format!("{}{i}", tool.name())).collect() };
  let mut failures = Vec::new();
  for &tool in tools {
    for name in names(tool)
      .iter()
      .chain([format!("doomed-{}", tool.name())].iter())
    {
      failures.extend(created(&slates_, name, tool).err());
    }
    failures.extend(
      clients
        .push_first(tool, &format!("doomed-{}", tool.name()))
        .err(),
    );
  }
  // (1) Every repository's first push, all at once.
  let first: Vec<Box<dyn FnOnce() -> Result<(), String> + Send + '_>> = tools
    .iter()
    .flat_map(|&tool| names(tool).into_iter().map(move |name| (tool, name)))
    .map(|(tool, name)| {
      let clients = &clients;
      Box::new(move || clients.push_first(tool, &name))
        as Box<dyn FnOnce() -> Result<(), String> + Send + '_>
    })
    .collect();
  failures.extend(at_once(first).into_iter().filter_map(Result::err));
  // (2) Second pushes, fetches, a creation and a destroy a tool, all at once.
  let mut second: Vec<Box<dyn FnOnce() -> Result<(), String> + Send + '_>> = Vec::new();
  for &tool in tools {
    for name in names(tool) {
      let clients = &clients;
      let fetched = name.clone();
      second.push(Box::new(move || clients.push_second(tool, &name)));
      second.push(Box::new(move || {
        clients.fetch(tool, &fetched, "fetched").map(drop)
      }));
    }
    let (clients, slates_) = (&clients, &slates_);
    second.push(Box::new(move || {
      let late = format!("late-{}", tool.name());
      created(slates_, &late, tool)?;
      clients.push_first(tool, &late)?;
      clients.push_second(tool, &late)
    }));
    second.push(Box::new(move || {
      failed(
        &format!("repository destroy doomed-{} (mid-load)", tool.name()),
        slates(
          &slates_.instance,
          &[],
          &["repository", "destroy", &format!("doomed-{}", tool.name())],
        ),
      )
      .map(drop)
    }));
  }
  failures.extend(at_once(second).into_iter().filter_map(Result::err));
  // (3) Each repository alone against its own client's history; the late one too; the destroyed one gone.
  for &tool in tools {
    for name in names(tool)
      .into_iter()
      .chain([format!("late-{}", tool.name())])
    {
      failures.extend(clients.check(tool, &name));
    }
    let doomed = format!("doomed-{}", tool.name());
    if clients.fetch(tool, &doomed, "after-destroy").is_ok() {
      failures.push(format!("{doomed}: still served after its destroy"));
    }
  }
  eprintln!("MANY N={n} tools {tools:?}: {} failures", failures.len());
  failures
    .into_iter()
    .map(|failure| format!("N={n}: {failure}"))
    .collect()
}

/// Goal 93 correctness, one server serving many repositories (module docs; A-724, §4.17.5; main's order of
/// 2026-11-21). Do: on one daemon, N repositories of each tool asked for, each pushed twice by its own stock client,
/// all at once, with every repository fetched during the second pushes, one repository a tool created and one destroyed
/// mid-load. Expect: every push and fetch accepted; each repository verifies and holds exactly its own client's history
/// (no other repository's commits, refs, bookmarks, tags, revisions or marker), its shared file's bytes beside the
/// others'; the late repository whole; the destroyed one gone and no other disturbed. Ignored: main runs it.
#[test]
#[ignore = "a round main runs: SLATES_MANY_N='2 16 64' cargo test --release -p slates-cli --test vcs_many_repositories -- --ignored --nocapture"]
fn many_repositories_on_one_daemon_each_hold_exactly_their_own_history() {
  let hg = stock_hg();
  let tools = tools(hg.as_ref());
  if tools.is_empty() {
    eprintln!(
      "skipping the many-repositories round: no stock client of any tool asked for is here"
    );
    return;
  }
  let failures: Vec<String> = counts()
    .into_iter()
    .flat_map(|n| round(n, &tools, hg.as_ref()))
    .collect();
  assert!(
    failures.is_empty(),
    "{} failures:\n{}",
    failures.len(),
    failures.join("\n")
  );
}
