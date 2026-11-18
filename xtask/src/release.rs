//! The release lane's tasks (Appendix B.2, AC-9.5, Phase 9's fresh-machine install test; `docs/publish.md`
//! "The binary release").
//!
//! - `cargo run -p xtask --bin release -- smoke --binary PATH --target TRIPLE --record FILE` runs one built `slates`
//!   binary the way a person installing it would: it says its version, which must be the workspace's; it starts as the
//!   anchor (the full machine profile, no test switches), the daemon serves, the consensus group is bootstrapped, a
//!   volume is provisioned from the command line, a file is written and read back by one MCP client, read again by a
//!   second client, the volume is destroyed, and when the anchor is stopped the daemon goes with it. Every step is
//!   timed and the binary is digested; the record names the bytes that were tested, so the release can refuse any
//!   other bytes.
//! - `cargo run -p xtask --bin release -- artifacts --dist DIR --rebuild DIR --records DIR --notes DIR (--tag TAG |
//!   --dry-run) --commit SHA --run-url URL` gathers the records of every target, refuses a release missing a target,
//!   carrying bytes no smoke test ran, or whose second build differs from its first (AC-9.5), writes `SHA256SUMS` and
//!   the provenance document beside the binaries, and the draft's notes, title and kind and the publish plan into the
//!   notes directory. It stages; it never signs or publishes. A tag's run hands what it staged to the release job,
//!   which signs it and attaches it to a draft. A dry run (`--dry-run`: the lane run with no tag) stages the same files
//!   labelled a dry run, and its plan says what the release tag would do with them; nothing after it signs or attaches
//!   anything (A-320).
//!
//! The write and the read go through one MCP client because a `slates attach` and a `slates write` made by two
//! processes race the reaper, which removes a departed client's attachments within about a second (GAPS, "A CLI
//! attachment ends about a second after the `slates attach` that made it"). One client holds its attachment for the
//! whole exchange, as an agent does; the second client only reads, which needs no attachment.
//!
//! A tag is either a release (`v` + the workspace version) or a test of this lane (`v` + the version + `-test.N`).
//! The release lane takes both and makes a draft of each; a test's draft is a pre-release titled as a test, and the
//! SDK publish lanes never start on one. A run with no tag is a dry run: everything up to the signature, and the
//! staged files kept as the run's `release-staged` artifact.
//!
//! Why the dry run stages the whole release (A-320): before it, a run with no tag built and smoke-tested every target
//! and stopped, so the checksums, the comparison of each target's two builds and the provenance ran only on a tag. The
//! two test tags of 2026-11-05 were the only runs that ever checked them. A dry run now runs every check a tag's run
//! does, and stops only at the two steps that leave a public record: the signature (on a public repository Sigstore
//! writes it to its public transparency log) and the release.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Failure;
use crate::tag::{TagKind, release_tag, tag_kind};
use crate::targets::ASSETS;

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
/// Format: the publish plan in the `--notes` directory: every staged file with its size and digest, and what follows
/// the staging, step by step. The staging job shows it as its summary (A-320).
const PLAN: &str = "plan.md";
/// Format: the switch that makes the artifacts task stage a dry run, a run of the lane with no tag (A-320).
const DRY_RUN: &str = "--dry-run";
/// Format: the workflow artifact the staging job keeps every staged file in, and the release job takes them from
/// (`release.yml`).
const STAGED_ARTIFACT: &str = "release-staged";
/// Format: the name the release job gives the attestation bundle among the draft's files (`release.yml`).
const ATTESTATION_BUNDLE: &str = "slates-attestation.sigstore.json";
/// Format: the provenance document's schema; a reader keys on it.
const PROVENANCE_FORMAT: &str = "slates-release-provenance/1";
/// Format: a record's file name is the asset's name followed by this.
const RECORD_SUFFIX: &str = ".record.json";
/// Format: the record of the same target's second build, on a separate runner (AC-9.5), is named so.
const REBUILD_SUFFIX: &str = ".rebuild.record.json";
/// Shape: how many differing offsets a refused rebuild names; enough to tell a timestamp's few bytes from code that
/// differs, and short enough to read in a CI log.
const DIFFERENCES_SHOWN: usize = 8;

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
const SMOKE_USAGE: &str = "release smoke --binary PATH --target TRIPLE --record FILE";
/// Format: the artifacts task's usage.
const ARTIFACTS_USAGE: &str = "release artifacts --dist DIR --rebuild DIR --records DIR --notes DIR (--tag TAG | \
   --dry-run) --commit SHA --run-url URL";

/// The smoke task: `cargo run -p xtask --bin release -- smoke`.
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

/// The artifacts task: `cargo run -p xtask --bin release -- artifacts`.
pub(crate) fn artifacts(args: &[String]) -> Result<(), Failure> {
  let dist = PathBuf::from(required(args, "--dist", ARTIFACTS_USAGE)?);
  let records_dir = PathBuf::from(required(args, "--records", ARTIFACTS_USAGE)?);
  let rebuild_dir = PathBuf::from(required(args, "--rebuild", ARTIFACTS_USAGE)?);
  let notes_dir = PathBuf::from(required(args, "--notes", ARTIFACTS_USAGE)?);
  let commit = required(args, "--commit", ARTIFACTS_USAGE)?;
  let run_url = required(args, "--run-url", ARTIFACTS_USAGE)?;
  let version = env!("CARGO_PKG_VERSION");
  let staging = staging_of(args, version)?;
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
    let rebuild_path = records_dir.join(format!("{asset}{REBUILD_SUFFIX}"));
    let rebuild_text = std::fs::read_to_string(&rebuild_path).map_err(|e| {
      Failure(format!(
        "release-artifacts: no rebuild record for {asset} ({}: {e}); every target is built twice (AC-9.5)",
        rebuild_path.display()
      ))
    })?;
    let rebuild_bytes = std::fs::read(rebuild_dir.join(asset)).map_err(|e| {
      Failure(format!(
        "release-artifacts: no rebuilt binary {asset} in {}: {e}",
        rebuild_dir.display()
      ))
    })?;
    records.push(Gathered {
      asset,
      record,
      bytes,
      rebuild: serde_json::from_str(&rebuild_text)?,
      rebuild_bytes,
    });
  }
  let documents = release_documents(
    &records,
    &Release {
      staging: &staging,
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
    (notes_dir.join(PLAN), &documents.plan),
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
  println!("{}", documents.plan);
  println!(
    "release-artifacts: {} binaries, {CHECKSUMS} and {PROVENANCE} in {}; \"{}\" (pre-release: {prerelease}) and \
     {PLAN} in {}; staged only: this task signs and publishes nothing",
    records.len(),
    dist.display(),
    documents.title,
    notes_dir.display()
  );
  Ok(())
}

/// What a run of the release lane stages its files for (A-320).
#[derive(Debug, PartialEq, Eq)]
enum Staging<'a> {
  /// A tag's run. The release job signs what was staged and attaches it to a draft; `kind` says whether the tag
  /// releases the workspace version or tests the lane.
  Tag {
    /// The tag, as pushed.
    tag: &'a str,
    /// What the tag names.
    kind: TagKind,
  },
  /// A run with no tag (`workflow_dispatch`): every file and check of a tag's run, labelled a dry run. Nothing is
  /// signed, attached or published.
  DryRun,
}

/// What the artifacts task stages for: `--tag TAG`, which must name the workspace `version`, or `--dry-run`. Exactly
/// one of them, so a run can never be both and never neither.
fn staging_of<'a>(args: &'a [String], version: &str) -> Result<Staging<'a>, Failure> {
  let dry_run = args.iter().any(|arg| arg == DRY_RUN);
  match (value(args, "--tag"), dry_run) {
    (Some(tag), false) => tag_kind(tag, version)
      .map(|kind| Staging::Tag { tag, kind })
      .map_err(Failure),
    (None, true) => Ok(Staging::DryRun),
    (Some(tag), true) => Err(Failure(format!(
      "{ARTIFACTS_USAGE}: `--tag {tag}` and `{DRY_RUN}` name two kinds of run; give one"
    ))),
    (None, false) => Err(Failure(format!(
      "{ARTIFACTS_USAGE}: `--tag` or `{DRY_RUN}` is missing"
    ))),
  }
}

/// One target's smoke record and the binary bytes the release would ship for it, and the same of its second build
/// on a separate runner.
struct Gathered {
  asset: &'static str,
  record: Value,
  bytes: Vec<u8>,
  rebuild: Value,
  rebuild_bytes: Vec<u8>,
}

/// Where two builds of one target differ: the count of differing bytes (a length difference counts every byte past
/// the shorter) and the first [`DIFFERENCES_SHOWN`] offsets.
fn differences(first: &[u8], second: &[u8]) -> (usize, Vec<usize>) {
  let mut count = first.len().abs_diff(second.len());
  let mut shown = Vec::new();
  for (offset, (a, b)) in first.iter().zip(second).enumerate() {
    if a != b {
      count = count.saturating_add(1);
      if shown.len() < DIFFERENCES_SHOWN {
        shown.push(offset);
      }
    }
  }
  (count, shown)
}

/// Refuses a record that does not name `asset` and `bytes` (its smoke test ran other bytes), saying which `build`.
fn check_record(asset: &str, build: &str, record: &Value, bytes: &[u8]) -> Result<String, Failure> {
  let sha256 = sha256_hex(bytes);
  let recorded = |key: &str| record.get(key).and_then(Value::as_str);
  if recorded("asset") != Some(asset) {
    return Err(Failure(format!(
      "release-artifacts: {asset}'s {build} record names asset {:?}",
      recorded("asset")
    )));
  }
  if recorded("sha256") != Some(sha256.as_str()) {
    return Err(Failure(format!(
      "release-artifacts: {asset} ({build}) is not the binary its smoke test ran (sha256 {sha256}, the record {:?}); \
       only tested bytes ship",
      recorded("sha256")
    )));
  }
  Ok(sha256)
}

/// What the release is: what the run stages for, the version, the commit and the run that built it.
struct Release<'a> {
  staging: &'a Staging<'a>,
  version: &'a str,
  commit: &'a str,
  run_url: &'a str,
}

impl Release<'_> {
  /// The tag the staged files ship under: the pushed tag, or for a dry run the tag that would release the version.
  fn tag(&self) -> String {
    match self.staging {
      Staging::Tag { tag, .. } => (*tag).to_owned(),
      Staging::DryRun => release_tag(self.version),
    }
  }

  /// Whether the draft is a pre-release: a test of the lane, a dry run, or a version with a pre-release part
  /// (`0.2.0-rc.1`).
  fn prerelease(&self) -> bool {
    !matches!(
      self.staging,
      Staging::Tag {
        kind: TagKind::Release,
        ..
      }
    ) || self.version.contains('-')
  }
}

/// The documents written beside the binaries, the draft's title and kind, and the publish plan.
struct Documents {
  checksums: String,
  provenance: String,
  notes: String,
  title: String,
  /// See [`Release::prerelease`].
  prerelease: bool,
  plan: String,
}

/// One target's binary once both its builds are checked: its asset name, its size and its SHA-256.
struct Checked {
  asset: &'static str,
  size: usize,
  sha256: String,
}

/// Checks every record against the bytes it names and writes the documents. Pure: the tests drive it.
fn release_documents(gathered: &[Gathered], release: &Release<'_>) -> Result<Documents, Failure> {
  let binaries = gathered
    .iter()
    .map(checked_build)
    .collect::<Result<Vec<_>, _>>()?;
  let checksums: String = binaries
    .iter()
    .map(|binary| format!("{}  {}\n", binary.sha256, binary.asset))
    .collect();
  let (tag, test) = match release.staging {
    Staging::Tag {
      tag,
      kind: TagKind::Release,
    } => (Some(*tag), None),
    Staging::Tag {
      tag,
      kind: TagKind::Test { number },
    } => (Some(*tag), Some(*number)),
    Staging::DryRun => (None, None),
  };
  let assets: Vec<Value> = gathered
    .iter()
    .map(|entry| {
      let mut asset = entry.record.clone();
      if let Some(fields) = asset.as_object_mut() {
        fields.insert("rebuild".to_owned(), entry.rebuild.clone());
      }
      asset
    })
    .collect();
  let provenance = serde_json::to_string_pretty(&json!({
    "format": PROVENANCE_FORMAT,
    "tag": tag,
    "test": test,
    "dry_run": matches!(release.staging, Staging::DryRun),
    "version": release.version,
    "commit": release.commit,
    "run": release.run_url,
    "assets": assets,
  }))?;
  let title = title(release);
  let documents = [(CHECKSUMS, checksums.len()), (PROVENANCE, provenance.len())];
  let plan = plan(release, &title, &binaries, &documents);
  Ok(Documents {
    notes: notes(release),
    prerelease: release.prerelease(),
    checksums,
    provenance,
    title,
    plan,
  })
}

/// Checks one target's two builds against their records and against each other: only bytes a smoke test ran ship,
/// and only bytes a second runner built the same (AC-9.5).
fn checked_build(entry: &Gathered) -> Result<Checked, Failure> {
  let sha256 = check_record(entry.asset, "build", &entry.record, &entry.bytes)?;
  let rebuilt = check_record(entry.asset, "rebuild", &entry.rebuild, &entry.rebuild_bytes)?;
  if rebuilt != sha256 {
    let (count, shown) = differences(&entry.bytes, &entry.rebuild_bytes);
    return Err(Failure(format!(
      "release-artifacts: {} is not reproducible (AC-9.5): its rebuild on a separate runner differs in {count} of {} \
       bytes (sha256 {sha256} and {rebuilt}), first at offsets {shown:?}",
      entry.asset,
      entry.bytes.len()
    )));
  }
  Ok(Checked {
    asset: entry.asset,
    size: entry.bytes.len(),
    sha256,
  })
}

/// Format: the platforms table in the draft's notes, one row per shipped asset; a test holds it to
/// [`ASSETS`].
const PLATFORMS: &str = "| Platform | File |\n|---|---|\n| macOS arm64 | `slates-macos-arm64` |\n| Linux x86_64 \
   (glibc) | `slates-linux-x64` |\n| Linux arm64 (glibc) | `slates-linux-arm64` |\n| Linux x86_64 (static, musl) | \
   `slates-linux-x64-musl` |\n| Linux arm64 (static, musl) | `slates-linux-arm64-musl` |\n| Windows x86_64 | \
   `slates-windows-x64.exe` |\n| Windows arm64 | `slates-windows-arm64.exe` |\n\n";

/// The title of the release of `version`.
fn release_title(version: &str) -> String {
  format!("slates {version}")
}

/// The draft's title: a release's is [`release_title`]; a test's and a dry run's say what they are.
fn title(release: &Release<'_>) -> String {
  match release.staging {
    Staging::Tag {
      tag,
      kind: TagKind::Test { number },
    } => format!("TEST {tag} — test {number} of the release lane, not a release"),
    Staging::Tag {
      kind: TagKind::Release,
      ..
    } => release_title(release.version),
    Staging::DryRun => format!(
      "DRY RUN — slates {} from {}, not a release",
      release.version, release.commit
    ),
  }
}

/// The headline of a release's notes.
fn release_headline(release: &Release<'_>) -> String {
  format!(
    "slates {} — one `slates` binary per platform, the same binary for the anchor, the daemon and every client verb. \
     Built from {}.\n\n",
    release.version, release.commit
  )
}

/// The draft's notes: what the run is, the platforms, how each binary was tested and how to verify a download. A dry
/// run's notes are the notes a release of its bytes would carry, under a headline saying they are not one.
fn notes(release: &Release<'_>) -> String {
  let mut notes = match release.staging {
    Staging::Tag {
      kind: TagKind::Test { number },
      ..
    } => format!(
      "**A test of the release lane (test {number}), not a release.** Built from {} at version {}; never to be \
       published.\n\n",
      release.commit, release.version
    ),
    Staging::Tag {
      kind: TagKind::Release,
      ..
    } => release_headline(release),
    Staging::DryRun => format!(
      "**A dry run of the release lane, not a release.** Built from {} at version {} with no tag and kept as the \
       run's `{STAGED_ARTIFACT}` artifact; nothing was signed, attached or published. Below are the notes a release \
       of these bytes would carry.\n\n{}",
      release.commit,
      release.version,
      release_headline(release)
    ),
  };
  notes.push_str(PLATFORMS);
  notes.push_str(&format!(
    "Each binary was started on its own platform before it was attached here: the anchor and daemon served, a \
     volume provisioned, a file was written and read back by one client and read again by another, and the daemon \
     left with its anchor (`cargo run -p xtask --bin release -- smoke`; each step's time is in `{PROVENANCE}`). \
     Each was also built a second time on a separate runner, from the same tag, and the two builds are byte for byte \
     the same.\n\nVerify a \
     download with `sha256sum -c {CHECKSUMS} --ignore-missing` (on macOS `shasum -a 256 -c`), and its build \
     provenance with `gh attestation verify <file> --repo hyper-light/slates`. Built by {}.\n",
    release.run_url
  ));
  notes
}

/// The publish plan (A-320): every staged file with its size and digest, then what follows the staging. The staging
/// job shows it as its summary, so a dry run says exactly what its release tag would do with the same files.
fn plan(
  release: &Release<'_>,
  title: &str,
  binaries: &[Checked],
  documents: &[(&str, usize)],
) -> String {
  let tag = release.tag();
  let mut plan = match release.staging {
    Staging::Tag {
      kind: TagKind::Release,
      ..
    } => format!("## Staged for the release tag `{tag}`: {title}\n\n"),
    Staging::Tag {
      kind: TagKind::Test { .. },
      ..
    } => format!("## Staged for the test tag `{tag}`: a test of the lane, not a release\n\n"),
    Staging::DryRun => format!(
      "## Dry run of slates {}: staged, nothing signed or published\n\n",
      release.version
    ),
  };
  plan.push_str(&format!(
    "Built from `{}` by {}. Every target was built twice, each copy on a runner of its own, and the two copies are \
     the same bytes; each was started on its own platform before it was staged (the smoke test). The run's \
     `{STAGED_ARTIFACT}` artifact holds these files and the draft's notes:\n\n| File | Bytes | SHA-256 |\n|---|---|---|\n",
    release.commit, release.run_url
  ));
  for binary in binaries {
    plan.push_str(&format!(
      "| `{}` | {} | `{}` |\n",
      binary.asset, binary.size, binary.sha256
    ));
  }
  for (name, size) in documents {
    plan.push_str(&format!("| `{name}` | {size} | |\n"));
  }
  plan.push('\n');
  plan.push_str(&next_steps(release, &tag));
  plan
}

/// What follows the staging, by the kind of run: the signature and the draft on a tag, nothing on a dry run, whose
/// plan says what its release tag would do instead.
fn next_steps(release: &Release<'_>, tag: &str) -> String {
  let sign = format!(
    "signs SLSA build provenance for the binaries named in `{CHECKSUMS}` (GitHub's artifact attestation, keyless \
     through Sigstore with the job's own identity; on a public repository Sigstore also records the signature in its \
     public transparency log)"
  );
  let carries = format!("the binaries, `{CHECKSUMS}`, `{PROVENANCE}` and `{ATTESTATION_BUNDLE}`");
  let latest = if release.version.contains('-') {
    ""
  } else {
    " --latest"
  };
  let publish = format!("`gh release edit {tag} --draft=false{latest}`");
  match release.staging {
    Staging::Tag {
      kind: TagKind::Release,
      ..
    } => format!(
      "**Next in this run:** the `draft release` job, the only job holding write permissions, {sign}, then creates \
       the draft release `{tag}` with {carries}. A draft is seen by the repository's maintainers only.\n\n**The \
       publish** is a maintainer's act: check the draft, then {publish}. The same tag starts `publish-python.yml` and \
       `publish-node.yml` too, which publish the SDKs to PyPI and npm with no draft (`docs/publish.md`, \"The \
       publish step\").\n"
    ),
    Staging::Tag {
      kind: TagKind::Test { .. },
      ..
    } => format!(
      "**Next in this run:** the `draft release` job {sign}, then creates the draft pre-release `{tag}` with \
       {carries}. A test's draft is never published: delete it with `gh release delete {tag} --cleanup-tag --yes`. \
       No SDK lane starts on a test tag.\n"
    ),
    Staging::DryRun => format!(
      "**Nothing follows in this run.** A dry run holds no write permission, signs nothing and makes no release.\n\n\
       **What the release tag `{tag}` does**, pushed by a maintainer at a commit whose dry run passed \
       (`docs/publish.md`, \"The publish step\"): this lane builds, tests and stages the files again from the tag, \
       and its `draft release` job {sign}, then creates the draft release `{tag}`, titled \"{}\", with {carries}. \
       **The publish** is then a maintainer's act: check the draft, then {publish}. The tag also starts \
       `publish-python.yml` and `publish-node.yml`, which publish the SDKs to PyPI and npm with no draft.\n",
      release_title(release.version)
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn gathered(asset: &'static str, bytes: &[u8], recorded_sha: Option<String>) -> Gathered {
    let sha256 = recorded_sha.unwrap_or_else(|| sha256_hex(bytes));
    Gathered {
      asset,
      record: json!({ "asset": asset, "sha256": sha256, "smoke": [] }),
      bytes: bytes.to_vec(),
      rebuild: json!({ "asset": asset, "sha256": sha256_hex(bytes), "smoke": [] }),
      rebuild_bytes: bytes.to_vec(),
    }
  }

  /// The release tag `v0.1.0`'s staging.
  const RELEASE_TAG: Staging<'static> = Staging::Tag {
    tag: "v0.1.0",
    kind: TagKind::Release,
  };

  fn release<'a>(staging: &'a Staging<'a>) -> Release<'a> {
    Release {
      staging,
      version: "0.1.0",
      commit: "0123abcd",
      run_url: "https://github.com/hyper-light/slates/actions/runs/1",
    }
  }

  /// AC-9.5, A-320: do gather a target whose second build differs from its first in its 4-byte link timestamp and its
  /// 16-byte PDB GUID (the 20 bytes MSVC's linker varied between two runners without `/Brepro`, run 42046848954), on a
  /// tag and on a dry run; expect both refused, naming the asset, the 20 differing bytes and their first offsets. Do
  /// gather two identical builds; expect the release made, with the rebuild in the provenance.
  #[test]
  fn a_target_whose_rebuild_differs_is_not_reproducible() {
    let first: Vec<u8> = (0..4096u32).map(|at| (at % 251) as u8).collect();
    let mut second = first.clone();
    for offset in (256..260).chain(3000..3016) {
      second[offset] ^= 0xff;
    }
    for staging in [RELEASE_TAG, Staging::DryRun] {
      let mut entry = gathered("slates-windows-x64.exe", &first, None);
      entry.rebuild = json!({ "asset": entry.asset, "sha256": sha256_hex(&second) });
      entry.rebuild_bytes = second.clone();
      match release_documents(&[entry], &release(&staging)) {
        Ok(_) => panic!("a differing rebuild was accepted ({staging:?})"),
        Err(Failure(message)) => {
          assert!(message.contains("slates-windows-x64.exe"), "{message}");
          assert!(message.contains("differs in 20 of 4096 bytes"), "{message}");
          assert!(
            message.contains("[256, 257, 258, 259, 3000, 3001, 3002, 3003]"),
            "{message}"
          );
        }
      }
    }
    let same = gathered("slates-windows-x64.exe", &first, None);
    let Ok(documents) = release_documents(&[same], &release(&RELEASE_TAG)) else {
      panic!("identical builds were refused")
    };
    let provenance: Value = serde_json::from_str(&documents.provenance).unwrap_or(Value::Null);
    assert_eq!(
      provenance["assets"][0]["rebuild"]["sha256"],
      sha256_hex(&first)
    );
  }

  /// Do gather two binaries whose records name their bytes; expect `SHA256SUMS` in `sha256sum`'s format with the
  /// FIPS 180-2 digest of "abc" (golden vector), the provenance naming the tag, the test and both records, and
  /// notes that say it is a test.
  #[test]
  fn the_checksums_and_provenance_name_the_tested_bytes() {
    let staging = Staging::Tag {
      tag: "v0.1.0-test.1",
      kind: TagKind::Test { number: 1 },
    };
    let entries = [
      gathered("slates-linux-x64", b"abc", None),
      gathered("slates-macos-arm64", b"", None),
    ];
    let documents = release_documents(&entries, &release(&staging)).ok();
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
    assert_eq!(provenance["dry_run"], false);
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
    assert!(
      documents
        .plan
        .contains("`gh release delete v0.1.0-test.1 --cleanup-tag --yes`"),
      "{}",
      documents.plan
    );
  }

  /// A-320: do stage the same two binaries as a dry run and for the release tag `v0.1.0`; expect the same
  /// `SHA256SUMS` byte for byte, since a dry run stages the bytes a release would ship. Expect the dry run's documents
  /// to say what they are: its provenance has no tag and `dry_run` true, its title and notes say "not a release" (the
  /// notes then carry the release's own), and it is never a plain release. Expect its plan to name every staged file
  /// with its digest, that nothing follows in the run, and the release tag `v0.1.0` with the exact publish command; and
  /// the tag's plan to name the signature and the draft as the run's next steps.
  #[test]
  fn a_dry_run_stages_a_releases_bytes_and_says_what_its_tag_would_do() {
    let entries = [
      gathered("slates-linux-x64", b"abc", None),
      gathered("slates-macos-arm64", b"", None),
    ];
    let Ok(dry) = release_documents(&entries, &release(&Staging::DryRun)) else {
      panic!("the dry run was refused")
    };
    let Ok(tagged) = release_documents(&entries, &release(&RELEASE_TAG)) else {
      panic!("the release was refused")
    };
    assert_eq!(dry.checksums, tagged.checksums);
    let provenance: Value = serde_json::from_str(&dry.provenance).unwrap_or(Value::Null);
    assert_eq!(provenance["dry_run"], true);
    assert_eq!(provenance["tag"], Value::Null);
    assert_eq!(provenance["test"], Value::Null);
    assert_eq!(provenance["commit"], "0123abcd");
    let released: Value = serde_json::from_str(&tagged.provenance).unwrap_or(Value::Null);
    assert_eq!(released["dry_run"], false);
    assert_eq!(released["tag"], "v0.1.0");
    assert!(dry.title.starts_with("DRY RUN"), "{}", dry.title);
    assert!(dry.title.contains("not a release"), "{}", dry.title);
    assert_eq!(tagged.title, "slates 0.1.0");
    assert!(
      dry
        .notes
        .starts_with("**A dry run of the release lane, not a release.**"),
      "{}",
      dry.notes
    );
    assert!(
      dry
        .notes
        .contains(&release_headline(&release(&RELEASE_TAG))),
      "{}",
      dry.notes
    );
    assert!(dry.prerelease, "a dry run is never a plain release");
    assert!(!tagged.prerelease, "0.1.0 is a plain release");
    for line in [
      "| `slates-linux-x64` | 3 | `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad` |",
      "| `slates-macos-arm64` | 0 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |",
    ] {
      assert!(dry.plan.contains(line), "{line} missing from {}", dry.plan);
      assert!(
        tagged.plan.contains(line),
        "{line} missing from {}",
        tagged.plan
      );
    }
    for document in [
      format!("| `{CHECKSUMS}` | {} | |", dry.checksums.len()),
      format!("| `{PROVENANCE}` | {} | |", dry.provenance.len()),
    ] {
      assert!(
        dry.plan.contains(&document),
        "{document} missing from {}",
        dry.plan
      );
    }
    for promised in [
      "**Nothing follows in this run.**",
      "**What the release tag `v0.1.0` does**",
      "titled \"slates 0.1.0\"",
      "`gh release edit v0.1.0 --draft=false --latest`",
      "`publish-python.yml` and `publish-node.yml`",
    ] {
      assert!(
        dry.plan.contains(promised),
        "{promised} missing from {}",
        dry.plan
      );
    }
    for promised in [
      "## Staged for the release tag `v0.1.0`: slates 0.1.0",
      "**Next in this run:**",
      ATTESTATION_BUNDLE,
      "`gh release edit v0.1.0 --draft=false --latest`",
    ] {
      assert!(
        tagged.plan.contains(promised),
        "{promised} missing from {}",
        tagged.plan
      );
    }
    let candidate = Release {
      version: "0.2.0-rc.1",
      ..release(&Staging::DryRun)
    };
    let Ok(candidate) = release_documents(&entries, &candidate) else {
      panic!("the candidate's dry run was refused")
    };
    assert!(
      candidate
        .plan
        .contains("`gh release edit v0.2.0-rc.1 --draft=false`"),
      "a pre-release version is published without --latest: {}",
      candidate.plan
    );
  }

  /// A-320: do give the artifacts task a tag of the workspace version, a dry run, both, neither, another version's tag,
  /// and a `--tag` whose value is the dry run's switch; expect the first two staged as asked and every other refused,
  /// so a run is never both a release and a dry run, and never neither.
  #[test]
  fn the_artifacts_task_stages_for_a_tag_or_a_dry_run_never_both() {
    let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let release_args = args(&["--tag", "v0.1.0"]);
    assert_eq!(staging_of(&release_args, "0.1.0").ok(), Some(RELEASE_TAG));
    let dry_args = args(&["--dist", "d", DRY_RUN]);
    assert_eq!(staging_of(&dry_args, "0.1.0").ok(), Some(Staging::DryRun));
    for refused in [
      args(&["--tag", "v0.1.0", DRY_RUN]),
      args(&["--dist", "d"]),
      args(&["--tag", "v0.2.0"]),
      args(&["--tag", DRY_RUN]),
    ] {
      match staging_of(&refused, "0.1.0") {
        Ok(staging) => panic!("{refused:?} was staged as {staging:?}"),
        Err(Failure(message)) => assert!(!message.is_empty()),
      }
    }
  }

  /// Do write the notes of every kind of run; expect each to name every shipped asset (`targets.rs`) in exactly one
  /// row of its platforms table, and no other row, so the notes cannot drift from what the lane builds.
  #[test]
  fn the_notes_name_every_shipped_asset_once() {
    let test = Staging::Tag {
      tag: "v0.1.0-test.1",
      kind: TagKind::Test { number: 1 },
    };
    for staging in [RELEASE_TAG, test, Staging::DryRun] {
      let notes = notes(&release(&staging));
      let rows: Vec<&str> = notes
        .lines()
        .filter(|line| line.starts_with("| ") && !line.starts_with("| Platform"))
        .collect();
      assert_eq!(rows.len(), ASSETS.len(), "{notes}");
      for (_, asset) in ASSETS {
        let named = format!("`{asset}`");
        assert_eq!(
          rows.iter().filter(|row| row.contains(&named)).count(),
          1,
          "{asset} in {notes}"
        );
      }
    }
  }

  /// Do gather a binary whose bytes differ from the ones its smoke test digested, on a tag and on a dry run; expect
  /// each refused, naming the asset — only tested bytes ship, and only tested bytes are staged.
  #[test]
  fn bytes_no_smoke_test_ran_are_refused() {
    for staging in [RELEASE_TAG, Staging::DryRun] {
      let entries = [gathered(
        "slates-linux-x64",
        b"rebuilt",
        Some(sha256_hex(b"tested")),
      )];
      match release_documents(&entries, &release(&staging)) {
        Ok(_) => panic!("untested bytes were accepted ({staging:?})"),
        Err(Failure(message)) => assert!(message.contains("slates-linux-x64"), "{message}"),
      }
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
}
