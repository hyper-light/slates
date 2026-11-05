//! The release lane's tasks (Appendix B.2, AC-9.5, Phase 9's fresh-machine install test; `docs/publish.md`
//! "The binary release").
//!
//! - `cargo xtask release-smoke --binary PATH --asset NAME --target TRIPLE --record FILE` runs one built `slates`
//!   binary the way a person installing it would: it says its version, which must be the workspace's; it starts as the
//!   anchor (the full machine profile, no test switches), the daemon serves, the consensus group is bootstrapped, a
//!   volume is provisioned from the command line, a file is written and read back by one MCP client, read again by a
//!   second client, the volume is destroyed, and when the anchor is stopped the daemon goes with it. Every step is
//!   timed and the binary is digested; the record names the bytes that were tested, so the release can refuse any
//!   other bytes.
//! - `cargo xtask release-artifacts --dist DIR --records DIR --notes DIR --tag TAG --commit SHA --run-url URL`
//!   gathers the records of every target, refuses a release missing a target or carrying bytes no smoke test ran,
//!   writes `SHA256SUMS` and the provenance document beside the binaries, and the draft's notes, title and kind into
//!   the notes directory.
//!
//! The write and the read go through one MCP client because a `slates attach` and a `slates write` made by two
//! processes race the reaper, which removes a departed client's attachments within about a second (GAPS, "A CLI
//! attachment ends about a second after the `slates attach` that made it"). One client holds its attachment for the
//! whole exchange, as an agent does; the second client only reads, which needs no attachment.
//!
//! A tag is either a release (`v` + the workspace version) or a test of this lane (`v` + the version + `-test.N`).
//! The release lane takes both and makes a draft of each; a test's draft is a pre-release titled as a test, and the
//! SDK publish lanes never start on one.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Failure;

/// What a release ships: one raw binary per target, named as Appendix B.2 names it. The release lane's matrix
/// builds exactly these, and `release-artifacts` refuses a release that lacks one or carries another.
pub(crate) const ASSETS: &[(&str, &str)] = &[
  ("aarch64-apple-darwin", "slates-macos-arm64"),
  ("x86_64-unknown-linux-gnu", "slates-linux-x64"),
  ("aarch64-unknown-linux-gnu", "slates-linux-arm64"),
  ("x86_64-unknown-linux-musl", "slates-linux-x64-musl"),
  ("aarch64-unknown-linux-musl", "slates-linux-arm64-musl"),
  ("x86_64-pc-windows-msvc", "slates-windows-x64.exe"),
  ("aarch64-pc-windows-msvc", "slates-windows-arm64.exe"),
];

/// Format: the provenance document's name among the release's files.
const PROVENANCE: &str = "slates-provenance.json";
/// Format: the checksum file's name among the release's files, in `sha256sum`'s format.
const CHECKSUMS: &str = "SHA256SUMS";
/// Format: the files the draft release is created from, in the `--notes` directory (none is a release file): its
/// notes, its title, and whether it is a pre-release (`true` or `false`).
const NOTES: &str = "notes.md";
/// Format: see [`NOTES`].
const TITLE: &str = "title";
/// Format: see [`NOTES`].
const PRERELEASE: &str = "prerelease";
/// Format: the provenance document's schema; a reader keys on it.
const PROVENANCE_FORMAT: &str = "slates-release-provenance/1";
/// Format: a record's file name is the asset's name followed by this.
const RECORD_SUFFIX: &str = ".record.json";
/// Format: the suffix that makes a tag a test of the release lane rather than a release: `v0.1.0-test.3`.
const TEST_TAG_INFIX: &str = "-test.";

/// Shape: how long the anchor may take from its spawn until its daemon says it serves. The full profile is measured
/// at start (seconds; the anchor's greeting came 3.6–5.4 s after the spawn at load 25–110 on an 18-core Mac,
/// `crates/cli/tests/cli.rs`'s `START_ATTEMPTS`); a hosted runner is slower. A hung start fails here, not at the
/// job's six-hour limit.
const SERVE_BOUND: Duration = Duration::from_secs(300);
/// Shape: how long one command-line verb or one MCP answer may take before the smoke test gives up on it; every
/// verb here is a single request to a local daemon (micro- to milliseconds), so this only bounds a hang.
const VERB_BOUND: Duration = Duration::from_secs(60);
/// Shape: consecutive answers to `volume list` awaited after the daemon serves, so the anchor's startup restart (a
/// heartbeat-lapse recovery) has settled before the steps run — `crates/cli/tests/cli.rs`'s `STABLE_STREAK`.
const STABLE_STREAK: u32 = 10;
/// Shape: attempts allowed to reach [`STABLE_STREAK`]: the streak plus the restart retries
/// `crates/cli/tests/cli.rs` allows (`RESTART_RETRIES`, 50).
const STABLE_ATTEMPTS: u32 = STABLE_STREAK + 50;
/// Shape: the pause between polls of a starting or stopping daemon, as `crates/cli/tests/cli.rs` polls.
const POLL: Duration = Duration::from_millis(20);
/// Shape: how long the daemon may outlive its stopped anchor: ten liveness budgets (the daemon watches its anchor
/// once a budget, 1 s, as `crates/cli/tests/cli.rs`'s `WAIT_FOR` allows).
const DEPART_BOUND: Duration = Duration::from_secs(10);
/// Format: the exit code `slates` answers when no daemon answers at the instance (`crates/cli/src/main.rs`).
const EXIT_UNAVAILABLE: i32 = 3;
/// Format: the start of the daemon's line once it serves (`crates/cli/src/daemon.rs`); the instance follows.
const SERVING_LINE: &str = "slates daemon: instance ";
/// Format: the anchor's line when its daemon is in a crash loop (`crates/cli/src/anchor.rs`, A-119): the anchor
/// never gives up on its daemon, so a daemon that can never serve shows here.
const CRASH_LOOP_LINE: &str = "slates anchor: the daemon is in a crash loop";
/// Shape: the smoke volume's bound: room for the one small file, far below any runner's memory.
const VOLUME_SIZE: &str = "64MiB";
/// Format: the file the smoke test writes, read and read again.
const SMOKE_PATH: &str = "/release-smoke.txt";

/// What a tag names.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TagKind {
  /// A release of the workspace version.
  Release,
  /// A test of the release lane at the workspace version: its draft is a pre-release titled as a test.
  Test {
    /// The test's number (`v0.1.0-test.3` is 3); each test run takes a new one.
    number: u64,
  },
}

/// What `tag` names when the workspace version is `version`: `v` + the version is a release, `v` + the version +
/// `-test.N` a test of the lane; anything else is refused with the forms it would accept.
pub(crate) fn tag_kind(tag: &str, version: &str) -> Result<TagKind, Failure> {
  let release = format!("v{version}");
  if tag == release {
    return Ok(TagKind::Release);
  }
  let number = tag
    .strip_prefix(&release)
    .and_then(|rest| rest.strip_prefix(TEST_TAG_INFIX))
    .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    .and_then(|digits| digits.parse::<u64>().ok());
  match number {
    Some(number) => Ok(TagKind::Test { number }),
    None => Err(Failure(format!(
      "tag `{tag}` does not name the workspace version: a release is `{release}` and a test of the release lane \
       `{release}{TEST_TAG_INFIX}N`; bump `[workspace.package] version`, run `cargo xtask version --write`, commit, \
       then tag"
    ))),
  }
}

/// The asset name of `target`, or a refusal naming the targets slates ships.
fn asset_of(target: &str) -> Result<&'static str, Failure> {
  ASSETS
    .iter()
    .find(|(triple, _)| *triple == target)
    .map(|(_, asset)| *asset)
    .ok_or_else(|| {
      let shipped: Vec<&str> = ASSETS.iter().map(|(triple, _)| *triple).collect();
      Failure(format!(
        "release: `{target}` is not a target slates ships ({})",
        shipped.join(", ")
      ))
    })
}

/// The value of `--name` in `args`.
fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
  args
    .iter()
    .position(|a| a == name)
    .and_then(|at| args.get(at.saturating_add(1)))
    .map(String::as_str)
}

/// The value of `--name` in `args`, or a refusal naming the task's usage.
fn required<'a>(args: &'a [String], name: &str, usage: &str) -> Result<&'a str, Failure> {
  value(args, name).ok_or_else(|| Failure(format!("{usage}: `{name}` is missing")))
}

/// Format: the smoke task's usage.
const SMOKE_USAGE: &str = "release-smoke --binary PATH --target TRIPLE --record FILE";
/// Format: the artifacts task's usage.
const ARTIFACTS_USAGE: &str =
  "release-artifacts --dist DIR --records DIR --notes DIR --tag TAG --commit SHA --run-url URL";

/// `cargo xtask release-smoke`.
pub(crate) fn smoke(root: &Path, args: &[String]) -> Result<(), Failure> {
  let binary = PathBuf::from(required(args, "--binary", SMOKE_USAGE)?);
  let target = required(args, "--target", SMOKE_USAGE)?;
  let record_path = PathBuf::from(required(args, "--record", SMOKE_USAGE)?);
  let asset = asset_of(target)?;
  let bytes = std::fs::read(&binary)
    .map_err(|e| Failure(format!("release-smoke: reading {}: {e}", binary.display())))?;
  let instance = format!("release-smoke-{}", std::process::id());
  let mut smoke = Smoke {
    binary: binary.clone(),
    instance,
    steps: Vec::new(),
  };
  let outcome = smoke.run_all();
  for step in &smoke.steps {
    println!(
      "release-smoke: {asset}: {} — {} ms",
      step.name,
      step.elapsed.as_millis()
    );
  }
  outcome?;
  let record = json!({
    "asset": asset,
    "target": target,
    "size": bytes.len(),
    "sha256": sha256_hex(&bytes),
    "blake3": blake3::hash(&bytes).to_hex().to_string(),
    "rustc": capture_text(root, "rustc", &["-vV"])?,
    "runner": runner_facts(),
    "smoke": smoke.steps.iter().map(|step| json!({
      "step": step.name,
      "ms": u64::try_from(step.elapsed.as_millis()).unwrap_or(u64::MAX),
    })).collect::<Vec<_>>(),
  });
  let text = serde_json::to_string_pretty(&record)?;
  #[allow(clippy::disallowed_methods)] // a development tool writing the record its caller named
  std::fs::write(&record_path, text).map_err(|e| {
    Failure(format!(
      "release-smoke: writing {}: {e}",
      record_path.display()
    ))
  })?;
  println!(
    "release-smoke: {asset} ({target}) passed every step; record {}",
    record_path.display()
  );
  Ok(())
}

/// One timed step of the smoke test.
struct Step {
  name: &'static str,
  elapsed: Duration,
}

/// The smoke test's state: the binary, its instance, and the steps done so far.
struct Smoke {
  binary: PathBuf,
  instance: String,
  steps: Vec<Step>,
}

impl Smoke {
  /// Runs `body` as the step `name`, timing it.
  fn step<T>(
    &mut self,
    name: &'static str,
    body: impl FnOnce(&mut Self) -> Result<T, Failure>,
  ) -> Result<T, Failure> {
    let start = Instant::now();
    let value = body(self).map_err(|f| Failure(format!("release-smoke: {name}: {f}")))?;
    self.steps.push(Step {
      name,
      elapsed: start.elapsed(),
    });
    Ok(value)
  }

  /// Every step, the anchor stopped whatever happens once it has started.
  fn run_all(&mut self) -> Result<(), Failure> {
    self.step("the binary names the workspace version", |smoke| {
      let answer = smoke.verb(&["version"], b"")?;
      let expected = format!("slates {}", env!("CARGO_PKG_VERSION"));
      if answer.code == 0 && answer.stdout_text().trim() == expected {
        Ok(())
      } else {
        Err(answer.refusal(&format!("expected `{expected}`")))
      }
    })?;
    let mut anchor = self.step("the anchor starts and its daemon serves", Smoke::start)?;
    let served = self.serve(&mut anchor);
    let stopped = self.step("the daemon departs with its stopped anchor", |smoke| {
      anchor.stop()?;
      smoke.await_departure()
    });
    served.and(stopped)
  }

  /// Everything done while the anchor runs.
  fn serve(&mut self, anchor: &mut Anchor) -> Result<(), Failure> {
    self.step("the daemon answers steadily", |smoke| {
      smoke.await_steady(anchor)
    })?;
    self.step("the consensus group is bootstrapped", |smoke| {
      smoke.verb(&["bootstrap", "root"], b"")?.success().map(drop)
    })?;
    let volume = self.step("a volume provisions", |smoke| {
      let created = smoke
        .verb(
          &[
            "volume",
            "create",
            "release-smoke",
            "--bounded",
            VOLUME_SIZE,
            "--json",
          ],
          b"",
        )?
        .success()?;
      let parsed: Value = serde_json::from_slice(&created.stdout)?;
      parsed
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Failure(format!("no `id` in {parsed}")))
    })?;
    let content = format!(
      "written by the release smoke test through slates' MCP server, instance {}\n",
      self.instance
    );
    self.step(
      "one MCP client attaches, writes and reads the file",
      |smoke| smoke.mcp_round_trip(&volume, &content),
    )?;
    self.step("a second client reads the same bytes", |smoke| {
      let read = smoke.verb(&["read", &volume, SMOKE_PATH], b"")?.success()?;
      if read.stdout == content.as_bytes() {
        Ok(())
      } else {
        Err(Failure(format!(
          "read {} bytes, not the {} written: {:?}",
          read.stdout.len(),
          content.len(),
          read.stdout_text()
        )))
      }
    })?;
    self.step("the volume is destroyed", |smoke| {
      smoke
        .verb(&["volume", "destroy", &volume], b"")?
        .success()
        .map(drop)
    })
  }

  /// Spawns the anchor and waits until its daemon says it serves.
  fn start(&mut self) -> Result<Anchor, Failure> {
    let mut child = Command::new(&self.binary)
      .args(["--instance", &self.instance, "anchor"])
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|e| Failure(format!("spawning {}: {e}", self.binary.display())))?;
    let stderr = child
      .stderr
      .take()
      .ok_or_else(|| Failure("the anchor's stderr was not piped".to_owned()))?;
    let (lines, receiver) = mpsc::channel();
    // The thread reads the anchor's stderr to its end, whether or not anyone still listens for lines: a reader that
    // stopped would close the pipe, and the anchor's next line would meet EPIPE, which `eprintln!` turns into a panic
    // that aborts the anchor (seen here first: 8 of 10 runs, 2026-11-04, GAPS).
    let echo = std::thread::spawn(move || {
      let mut listening = true;
      for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        eprintln!("{line}");
        listening = listening && lines.send(line).is_ok();
      }
    });
    let mut anchor = Anchor {
      child,
      echo: Some(echo),
      lines: Some(receiver),
    };
    let serving = format!("{SERVING_LINE}{} serving on ", self.instance);
    let deadline = Instant::now() + SERVE_BOUND;
    loop {
      let left = deadline.saturating_duration_since(Instant::now());
      let line = anchor
        .lines
        .as_ref()
        .ok_or_else(|| Failure("the anchor's lines are gone".to_owned()))?
        .recv_timeout(left);
      match line {
        Ok(line) if line.starts_with(&serving) => break,
        Ok(line) if line.starts_with(CRASH_LOOP_LINE) => {
          anchor.stop()?;
          return Err(Failure(format!("the daemon never served: {line}")));
        }
        Ok(_) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
          anchor.stop()?;
          return Err(Failure(format!(
            "the daemon did not serve within {} s (its lines are above)",
            SERVE_BOUND.as_secs()
          )));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
          anchor.stop()?;
          return Err(Failure(
            "the anchor ended before its daemon served (its lines are above)".to_owned(),
          ));
        }
      }
    }
    // The rest of the anchor's lines are echoed by the thread alone: dropping the receiver ends only the sends.
    anchor.lines = None;
    Ok(anchor)
  }

  /// Waits for [`STABLE_STREAK`] answers in a row to `volume list` within [`STABLE_ATTEMPTS`].
  fn await_steady(&mut self, anchor: &mut Anchor) -> Result<(), Failure> {
    let mut streak = 0u32;
    for attempt in 1..=STABLE_ATTEMPTS {
      if let Some(status) = anchor.child.try_wait()? {
        return Err(Failure(format!("the anchor exited ({status})")));
      }
      let answer = self.verb(&["volume", "list"], b"")?;
      streak = if answer.code == 0 {
        streak.saturating_add(1)
      } else {
        0
      };
      if streak >= STABLE_STREAK {
        return Ok(());
      }
      if attempt == STABLE_ATTEMPTS {
        return Err(answer.refusal(&format!(
          "{STABLE_STREAK} answers in a row were not given within {STABLE_ATTEMPTS} attempts"
        )));
      }
      pause(POLL);
    }
    Ok(())
  }

  /// Polls `status` until no daemon answers at the instance, within [`DEPART_BOUND`].
  fn await_departure(&mut self) -> Result<(), Failure> {
    let deadline = Instant::now() + DEPART_BOUND;
    loop {
      let answer = self.verb(&["status"], b"")?;
      if answer.code == EXIT_UNAVAILABLE {
        return Ok(());
      }
      if Instant::now() >= deadline {
        return Err(answer.refusal(&format!(
          "the daemon still answered {} s after its anchor was stopped",
          DEPART_BOUND.as_secs()
        )));
      }
      pause(POLL);
    }
  }

  /// Attaches for writing, writes `content` at [`SMOKE_PATH`] and reads it back, all through one `slates mcp`.
  fn mcp_round_trip(&mut self, volume: &str, content: &str) -> Result<(), Failure> {
    let mut mcp = Mcp::spawn(&self.binary, &self.instance)?;
    let outcome = (|| {
      let attached = mcp.call(
        "slates.attach.attach",
        json!({ "volume": volume, "write": true }),
      )?;
      let attachment = attached
        .get("attachment")
        .and_then(Value::as_u64)
        .ok_or_else(|| Failure(format!("no `attachment` in {attached}")))?;
      let written = mcp.call(
        "slates.fs.write",
        json!({ "volume": volume, "attachment": attachment, "path": SMOKE_PATH, "text": content }),
      )?;
      let size = written.get("size").and_then(Value::as_u64);
      if size != u64::try_from(content.len()).ok() {
        return Err(Failure(format!(
          "the write answered {written}, not {} bytes",
          content.len()
        )));
      }
      let read = mcp.call(
        "slates.fs.read",
        json!({ "volume": volume, "path": SMOKE_PATH }),
      )?;
      if read.get("text").and_then(Value::as_str) != Some(content) {
        return Err(Failure(format!("the read answered {read}")));
      }
      mcp
        .call("slates.attach.detach", json!({ "attachment": attachment }))
        .map(drop)
    })();
    let ended = mcp.end();
    outcome.and(ended)
  }

  /// Runs `slates --instance INSTANCE ARGS` with `input` on its stdin, bounded by [`VERB_BOUND`].
  fn verb(&self, args: &[&str], input: &[u8]) -> Result<Answer, Failure> {
    let mut command = Command::new(&self.binary);
    command.args(["--instance", &self.instance]).args(args);
    run_bounded(&mut command, input, VERB_BOUND)
      .map_err(|f| Failure(format!("`slates {}`: {f}", args.join(" "))))
  }
}

/// The anchor process: stopped and reaped when the smoke test ends, whatever happened.
struct Anchor {
  child: Child,
  echo: Option<std::thread::JoinHandle<()>>,
  lines: Option<mpsc::Receiver<String>>,
}

impl Anchor {
  /// Kills the anchor, reaps it and joins the thread echoing its lines (which ends when its stderr closes).
  fn stop(&mut self) -> Result<(), Failure> {
    self.lines = None;
    if self.child.try_wait()?.is_none() {
      self.child.kill()?;
    }
    self.child.wait()?;
    if let Some(echo) = self.echo.take() {
      echo
        .join()
        .map_err(|_| Failure("the anchor's echo thread failed".to_owned()))?;
    }
    Ok(())
  }
}

impl Drop for Anchor {
  fn drop(&mut self) {
    if self.echo.is_some() {
      let _ = self.child.kill();
      let _ = self.child.wait();
    }
  }
}

/// A `slates mcp` process spoken to over stdio, one JSON-RPC message a line.
struct Mcp {
  child: Child,
  input: Option<std::process::ChildStdin>,
  replies: mpsc::Receiver<String>,
  reader: Option<std::thread::JoinHandle<()>>,
  next_id: u64,
}

impl Mcp {
  fn spawn(binary: &Path, instance: &str) -> Result<Self, Failure> {
    let mut child = Command::new(binary)
      .args(["--instance", instance, "mcp"])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .spawn()
      .map_err(|e| Failure(format!("spawning `slates mcp`: {e}")))?;
    let input = child.stdin.take();
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| Failure("`slates mcp`'s stdout was not piped".to_owned()))?;
    let (sender, replies) = mpsc::channel();
    let reader = std::thread::spawn(move || {
      for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if sender.send(line).is_err() {
          break;
        }
      }
    });
    Ok(Self {
      child,
      input,
      replies,
      reader: Some(reader),
      next_id: 1,
    })
  }

  /// Calls the tool `name` with `arguments` and returns its structured result, or the tool's error.
  fn call(&mut self, name: &str, arguments: Value) -> Result<Value, Failure> {
    let id = self.next_id;
    self.next_id = self.next_id.saturating_add(1);
    let request = json!({
      "jsonrpc": "2.0",
      "id": id,
      "method": "tools/call",
      "params": { "name": name, "arguments": arguments },
    });
    let input = self
      .input
      .as_mut()
      .ok_or_else(|| Failure("`slates mcp`'s stdin is closed".to_owned()))?;
    writeln!(input, "{request}")?;
    input.flush()?;
    let line = self.replies.recv_timeout(VERB_BOUND).map_err(|e| {
      Failure(format!(
        "`{name}`: no answer within {} s ({e})",
        VERB_BOUND.as_secs()
      ))
    })?;
    let reply: Value = serde_json::from_str(&line)?;
    if reply.get("id") != Some(&json!(id)) {
      return Err(Failure(format!("`{name}`: answered out of turn: {reply}")));
    }
    let result = reply
      .get("result")
      .ok_or_else(|| Failure(format!("`{name}`: {reply}")))?;
    if result.get("isError").and_then(Value::as_bool) != Some(false) {
      return Err(Failure(format!("`{name}` refused: {result}")));
    }
    result
      .get("structuredContent")
      .cloned()
      .ok_or_else(|| Failure(format!("`{name}`: no structured content in {result}")))
  }

  /// Closes its stdin, which ends the server, and reaps it within [`VERB_BOUND`].
  fn end(mut self) -> Result<(), Failure> {
    self.input = None;
    let status = wait_bounded(&mut self.child, VERB_BOUND)?;
    if let Some(reader) = self.reader.take() {
      reader
        .join()
        .map_err(|_| Failure("the MCP reader thread failed".to_owned()))?;
    }
    if status.success() {
      Ok(())
    } else {
      Err(Failure(format!("`slates mcp` ended {status}")))
    }
  }
}

impl Drop for Mcp {
  fn drop(&mut self) {
    if self.reader.is_some() {
      let _ = self.child.kill();
      let _ = self.child.wait();
    }
  }
}

/// A finished command's exit code and output.
struct Answer {
  code: i32,
  stdout: Vec<u8>,
  stderr: Vec<u8>,
}

impl Answer {
  fn stdout_text(&self) -> String {
    String::from_utf8_lossy(&self.stdout).into_owned()
  }

  /// The answer when it exited 0, else a refusal carrying its code and stderr.
  fn success(self) -> Result<Self, Failure> {
    if self.code == 0 {
      Ok(self)
    } else {
      Err(self.refusal("expected exit 0"))
    }
  }

  fn refusal(&self, what: &str) -> Failure {
    Failure(format!(
      "{what}; exit {}, stdout {:?}, stderr {:?}",
      self.code,
      self.stdout_text(),
      String::from_utf8_lossy(&self.stderr)
    ))
  }
}

/// Runs `command` with `input` on its stdin and its output captured, killing it past `bound`.
fn run_bounded(command: &mut Command, input: &[u8], bound: Duration) -> Result<Answer, Failure> {
  let mut child = command
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
  let stdout = drain(child.stdout.take());
  let stderr = drain(child.stderr.take());
  if let Some(mut stdin) = child.stdin.take() {
    stdin.write_all(input)?;
  }
  let status = wait_bounded(&mut child, bound)?;
  let collect = |thread: std::thread::JoinHandle<Vec<u8>>| {
    thread
      .join()
      .map_err(|_| Failure("an output reader thread failed".to_owned()))
  };
  Ok(Answer {
    code: status.code().unwrap_or(-1),
    stdout: collect(stdout)?,
    stderr: collect(stderr)?,
  })
}

/// Reads `pipe` to its end on a thread of its own, so a child never blocks on a full pipe.
fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
  std::thread::spawn(move || {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
      let _ = pipe.read_to_end(&mut bytes);
    }
    bytes
  })
}

/// Waits for `child` to exit, killing and reaping it past `bound`.
fn wait_bounded(child: &mut Child, bound: Duration) -> Result<std::process::ExitStatus, Failure> {
  let deadline = Instant::now() + bound;
  loop {
    if let Some(status) = child.try_wait()? {
      return Ok(status);
    }
    if Instant::now() >= deadline {
      let _ = child.kill();
      let _ = child.wait();
      return Err(Failure(format!(
        "did not exit within {} s; killed",
        bound.as_secs()
      )));
    }
    pause(POLL);
  }
}

/// Waits between two polls of another process (a command-line tool driving programs; nothing here runs on the
/// daemon's runtime).
fn pause(interval: Duration) {
  #[allow(clippy::disallowed_methods)] // a development tool's poll cadence, not daemon code
  std::thread::sleep(interval);
}

/// `program args` run in `root`, its stdout as text; a refusal when it fails.
fn capture_text(root: &Path, program: &str, args: &[&str]) -> Result<String, Failure> {
  let output = Command::new(program)
    .args(args)
    .current_dir(root)
    .output()
    .map_err(|e| Failure(format!("running `{program}`: {e}")))?;
  if !output.status.success() {
    return Err(Failure(format!(
      "`{program} {}` failed: {}",
      args.join(" "),
      String::from_utf8_lossy(&output.stderr)
    )));
  }
  Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Format: the environment variables a GitHub-hosted runner names its system and image by (GitHub Actions docs,
/// "Variables"; the image's own `ImageOS` and `ImageVersion`). An unset one is recorded as absent.
const RUNNER_VARIABLES: &[&str] = &["RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion"];

/// The runner's system and image, as far as its environment names them.
fn runner_facts() -> Value {
  let mut facts = serde_json::Map::new();
  for name in RUNNER_VARIABLES {
    if let Ok(value) = std::env::var(name) {
      facts.insert((*name).to_owned(), Value::String(value));
    }
  }
  Value::Object(facts)
}

/// The SHA-256 of `bytes`, lower-case hex.
fn sha256_hex(bytes: &[u8]) -> String {
  let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
  digest
    .as_ref()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// `cargo xtask release-artifacts`.
pub(crate) fn artifacts(args: &[String]) -> Result<(), Failure> {
  let dist = PathBuf::from(required(args, "--dist", ARTIFACTS_USAGE)?);
  let records_dir = PathBuf::from(required(args, "--records", ARTIFACTS_USAGE)?);
  let notes_dir = PathBuf::from(required(args, "--notes", ARTIFACTS_USAGE)?);
  let tag = required(args, "--tag", ARTIFACTS_USAGE)?;
  let commit = required(args, "--commit", ARTIFACTS_USAGE)?;
  let run_url = required(args, "--run-url", ARTIFACTS_USAGE)?;
  let version = env!("CARGO_PKG_VERSION");
  let kind = tag_kind(tag, version)?;
  let mut records = Vec::new();
  for (_, asset) in ASSETS {
    let path = records_dir.join(format!("{asset}{RECORD_SUFFIX}"));
    let text = std::fs::read_to_string(&path).map_err(|e| {
      Failure(format!(
        "release-artifacts: no smoke record for {asset} ({}: {e}); a release ships every target or none",
        path.display()
      ))
    })?;
    let record: Value = serde_json::from_str(&text)?;
    let bytes = std::fs::read(dist.join(asset)).map_err(|e| {
      Failure(format!(
        "release-artifacts: no binary {asset} in {}: {e}",
        dist.display()
      ))
    })?;
    records.push(Gathered {
      asset,
      record,
      bytes,
    });
  }
  let documents = release_documents(
    &records,
    &Release {
      tag,
      kind: &kind,
      version,
      commit,
      run_url,
    },
  )?;
  #[allow(clippy::disallowed_methods)]
  // a development tool making the notes directory its caller named
  std::fs::create_dir_all(&notes_dir).map_err(|e| {
    Failure(format!(
      "release-artifacts: making {}: {e}",
      notes_dir.display()
    ))
  })?;
  let prerelease = documents.prerelease.to_string();
  for (path, text) in [
    (dist.join(CHECKSUMS), &documents.checksums),
    (dist.join(PROVENANCE), &documents.provenance),
    (notes_dir.join(NOTES), &documents.notes),
    (notes_dir.join(TITLE), &documents.title),
    (notes_dir.join(PRERELEASE), &prerelease),
  ] {
    #[allow(clippy::disallowed_methods)]
    // a development tool writing the release files its caller asked for
    std::fs::write(&path, text).map_err(|e| {
      Failure(format!(
        "release-artifacts: writing {}: {e}",
        path.display()
      ))
    })?;
  }
  print!("{}", documents.checksums);
  println!(
    "release-artifacts: {} binaries, {CHECKSUMS} and {PROVENANCE} in {}; \"{}\" (pre-release: {prerelease}) in {}",
    records.len(),
    dist.display(),
    documents.title,
    notes_dir.display()
  );
  Ok(())
}

/// One target's smoke record and the binary bytes the release would ship for it.
struct Gathered {
  asset: &'static str,
  record: Value,
  bytes: Vec<u8>,
}

/// What the release is: its tag and what the tag names, the version, the commit and the run that built it.
struct Release<'a> {
  tag: &'a str,
  kind: &'a TagKind,
  version: &'a str,
  commit: &'a str,
  run_url: &'a str,
}

/// The documents written beside the binaries, and the draft's title and kind.
struct Documents {
  checksums: String,
  provenance: String,
  notes: String,
  title: String,
  /// A test of the lane, or a version with a pre-release part (`0.2.0-rc.1`), is a pre-release.
  prerelease: bool,
}

/// Checks every record against the bytes it names and writes the documents. Pure: the tests drive it.
fn release_documents(gathered: &[Gathered], release: &Release<'_>) -> Result<Documents, Failure> {
  let mut checksums = String::new();
  let mut assets = Vec::new();
  for entry in gathered {
    let sha256 = sha256_hex(&entry.bytes);
    let recorded = |key: &str| entry.record.get(key).and_then(Value::as_str);
    if recorded("asset") != Some(entry.asset) {
      return Err(Failure(format!(
        "release-artifacts: {}'s record names asset {:?}",
        entry.asset,
        recorded("asset")
      )));
    }
    if recorded("sha256") != Some(sha256.as_str()) {
      return Err(Failure(format!(
        "release-artifacts: {} is not the binary its smoke test ran (sha256 {sha256}, the record {:?}); only \
         tested bytes ship",
        entry.asset,
        recorded("sha256")
      )));
    }
    checksums.push_str(&format!("{sha256}  {}\n", entry.asset));
    assets.push(entry.record.clone());
  }
  let test = match release.kind {
    TagKind::Release => None,
    TagKind::Test { number } => Some(*number),
  };
  let provenance = json!({
    "format": PROVENANCE_FORMAT,
    "tag": release.tag,
    "test": test,
    "version": release.version,
    "commit": release.commit,
    "run": release.run_url,
    "assets": assets,
  });
  let mut notes = match test {
    Some(number) => format!(
      "**A test of the release lane (test {number}), not a release.** Built from {} at version {}; never to be \
       published.\n\n",
      release.commit, release.version
    ),
    None => format!(
      "slates {} — one `slates` binary per platform, the same binary for the anchor, the daemon and every client \
       verb. Built from {}.\n\n",
      release.version, release.commit
    ),
  };
  notes.push_str(
    "| Platform | File |\n|---|---|\n| macOS arm64 | `slates-macos-arm64` |\n| Linux x86_64 (glibc) | \
     `slates-linux-x64` |\n| Linux arm64 (glibc) | `slates-linux-arm64` |\n| Linux x86_64 (static, musl) | \
     `slates-linux-x64-musl` |\n| Linux arm64 (static, musl) | `slates-linux-arm64-musl` |\n| Windows x86_64 | \
     `slates-windows-x64.exe` |\n| Windows arm64 | `slates-windows-arm64.exe` |\n\n",
  );
  notes.push_str(&format!(
    "Each binary was started on its own platform before it was attached here: the anchor and daemon served, a \
     volume provisioned, a file was written and read back by one client and read again by another, and the daemon \
     left with its anchor (`cargo xtask release-smoke`; each step's time is in `{PROVENANCE}`).\n\nVerify a \
     download with `sha256sum -c {CHECKSUMS} --ignore-missing` (on macOS `shasum -a 256 -c`), and its build \
     provenance with `gh attestation verify <file> --repo hyper-light/slates`. Built by {}.\n",
    release.run_url
  ));
  let title = match test {
    Some(number) => format!(
      "TEST {} — test {number} of the release lane, not a release",
      release.tag
    ),
    None => format!("slates {}", release.version),
  };
  Ok(Documents {
    checksums,
    provenance: serde_json::to_string_pretty(&provenance)?,
    notes,
    title,
    prerelease: test.is_some() || release.version.contains('-'),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do name a tag by the workspace version, expect a release; with `-test.N`, expect test N; expect every other
  /// form refused (another version, a test with no number or a signed one, a stray suffix, no `v`).
  #[test]
  fn a_tag_is_a_release_or_a_numbered_test_of_the_workspace_version() {
    assert_eq!(tag_kind("v0.1.0", "0.1.0").ok(), Some(TagKind::Release));
    assert_eq!(
      tag_kind("v0.1.0-test.3", "0.1.0").ok(),
      Some(TagKind::Test { number: 3 })
    );
    assert_eq!(
      tag_kind("v0.2.0-rc.1-test.12", "0.2.0-rc.1").ok(),
      Some(TagKind::Test { number: 12 })
    );
    for refused in [
      "v0.1.1",
      "0.1.0",
      "v0.1.0-test.",
      "v0.1.0-test.-1",
      "v0.1.0-test.1a",
      "v0.1.0-rc.1",
      "v0.1.0-test",
      "v0.1.0.test.1",
    ] {
      assert!(
        tag_kind(refused, "0.1.0").is_err(),
        "{refused} must be refused"
      );
    }
  }

  fn gathered(asset: &'static str, bytes: &[u8], recorded_sha: Option<String>) -> Gathered {
    let sha256 = recorded_sha.unwrap_or_else(|| sha256_hex(bytes));
    Gathered {
      asset,
      record: json!({ "asset": asset, "sha256": sha256, "smoke": [] }),
      bytes: bytes.to_vec(),
    }
  }

  fn release<'a>(kind: &'a TagKind) -> Release<'a> {
    Release {
      tag: "v0.1.0-test.1",
      kind,
      version: "0.1.0",
      commit: "0123abcd",
      run_url: "https://github.com/hyper-light/slates/actions/runs/1",
    }
  }

  /// Do gather two binaries whose records name their bytes; expect `SHA256SUMS` in `sha256sum`'s format with the
  /// FIPS 180-2 digest of "abc" (golden vector), the provenance naming the tag, the test and both records, and
  /// notes that say it is a test.
  #[test]
  fn the_checksums_and_provenance_name_the_tested_bytes() {
    let kind = TagKind::Test { number: 1 };
    let entries = [
      gathered("slates-linux-x64", b"abc", None),
      gathered("slates-macos-arm64", b"", None),
    ];
    let documents = release_documents(&entries, &release(&kind)).ok();
    let Some(documents) = documents else {
      panic!("the documents were refused")
    };
    assert_eq!(
      documents.checksums,
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  slates-linux-x64\n\
       e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  slates-macos-arm64\n"
    );
    let provenance: Value = serde_json::from_str(&documents.provenance).unwrap_or(Value::Null);
    assert_eq!(provenance["format"], PROVENANCE_FORMAT);
    assert_eq!(provenance["tag"], "v0.1.0-test.1");
    assert_eq!(provenance["test"], 1);
    assert_eq!(provenance["assets"].as_array().map(Vec::len), Some(2));
    assert!(
      documents.notes.contains("not a release"),
      "{}",
      documents.notes
    );
    assert!(
      documents.title.starts_with("TEST v0.1.0-test.1"),
      "{}",
      documents.title
    );
    assert!(documents.prerelease, "a test is a pre-release");
  }

  /// Do gather a binary whose bytes differ from the ones its smoke test digested; expect the release refused, naming
  /// the asset — only tested bytes ship.
  #[test]
  fn bytes_no_smoke_test_ran_are_refused() {
    let kind = TagKind::Release;
    let entries = [gathered(
      "slates-linux-x64",
      b"rebuilt",
      Some(sha256_hex(b"tested")),
    )];
    match release_documents(&entries, &release(&kind)) {
      Ok(_) => panic!("untested bytes were accepted"),
      Err(Failure(message)) => assert!(message.contains("slates-linux-x64"), "{message}"),
    }
  }

  /// Do name a target slates does not ship; expect a refusal listing the ones it does.
  #[test]
  fn only_shipped_targets_have_assets() {
    assert_eq!(
      asset_of("aarch64-pc-windows-msvc").ok(),
      Some("slates-windows-arm64.exe")
    );
    match asset_of("i686-pc-windows-msvc") {
      Ok(asset) => panic!("i686 is not shipped, yet named {asset}"),
      Err(Failure(message)) => assert!(message.contains("x86_64-unknown-linux-gnu"), "{message}"),
    }
  }

  /// Do run the release guard over this tree with a test tag of its version; expect the binary release lane's guard
  /// (`--allow-test-tag`) to accept it and the SDK publish lanes' guard to refuse it, and both to accept the release
  /// tag — so a test of the release lane can never publish an SDK.
  #[test]
  fn only_the_release_lane_accepts_a_test_tag() {
    let Ok(root) = crate::workspace_root() else {
      panic!("no workspace root")
    };
    let version = env!("CARGO_PKG_VERSION");
    let guard = |tag: String, allow_test: bool| {
      crate::version::run(&root, &crate::version::Mode::ExpectTag { tag, allow_test }).is_ok()
    };
    assert!(guard(format!("v{version}-test.1"), true));
    assert!(!guard(format!("v{version}-test.1"), false));
    assert!(guard(format!("v{version}"), false));
    assert!(guard(format!("v{version}"), true));
    assert!(!guard(format!("v{version}-rc.9"), true));
  }
}
