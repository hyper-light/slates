//! The anchor's start judgment on Windows through the real command (§2.6 boot step 1; the anchor restart loop of
//! 2026-11-07, `docs/bugs/2026-11-07-the-anchor-killed-every-start-a-busy-machine-starved.md`): `slates anchor`
//! supervising a real `slates daemon`, whose successor the test starves or stops. A starting daemon that keeps making
//! progress, however slowly, is never killed; one that makes none is killed and replaced. The anchor judges a start in
//! the daemon's own CPU time, which Windows reports as `QueryProcessCycleTime`'s cycles
//! (`slates_machine::process_clock`).
//!
//! The Unix form is `crates/cli/tests/cli.rs`'s (`a_starved_start_that_makes_progress_is_not_killed`,
//! `a_stuck_start_is_killed_and_restarted`), a file that is Unix as a whole, which stops and continues the successor
//! with signals. Here the successor is suspended and resumed whole (`NtSuspendProcess`/`NtResumeProcess`), as
//! `winfsp.rs` suspends its anchor, and the anchor's own restart line names it ("restarted as pid N"). Windows only,
//! and gated on `SLATES_TEST_CLI=1` (the runner to itself); without it each test skips loudly and passes.
#![cfg(windows)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::BufRead;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
  OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE,
  PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

/// Shape: shards for the test daemon.
const SHARDS: &str = "2";
/// Shape: how long the anchor and its daemon get to come up, and a successor to answer (`cli.rs`' `START_WAIT`).
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: the pause between polls of a start.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answers to `volume list` a start waits for, so the anchor's one startup restart has settled.
const STABLE_STREAK: u32 = 10;
/// Shape: the most `volume list` attempts a start makes after the daemon's serving line (`crates/cli/tests/cli.rs`
/// `START_ATTEMPTS` says why attempts, not wall time).
const START_ATTEMPTS: u32 = 60;
/// Format: the start of the line the daemon prints once it serves (`crates/cli/src/daemon.rs`).
const SERVING_LINE: &str = "slates daemon: instance ";
/// Format: the anchor's restart line names the daemon it started after this (`crates/cli/src/anchor.rs`).
const RESTARTED_AS: &str = "restarted as pid ";
/// Format: the start of the anchor's line when it kills a starting daemon (`crates/cli/src/anchor.rs`).
const START_KILL_LINE: &str = "slates anchor: the starting daemon";
/// Derived: how long the successor is starved, three of the anchor's former fixed start budgets
/// (`RECOVERY_BUDGET_NS`), as `cli.rs`' `STARVED_FOR`: a start judged by a fixed wall second is killed at least twice.
const STARVED_FOR: Duration = Duration::from_nanos(3 * slates_db::replay::RECOVERY_BUDGET_NS);
/// Derived: the duty cycle's period, one heartbeat (`HEARTBEAT_NS`), as `cli.rs`' `STARVED_PERIOD`.
const STARVED_PERIOD: Duration = Duration::from_nanos(slates_server::daemon::HEARTBEAT_NS);
/// Shape: the successor runs one sixty-fourth of each period, the share a start got under 144 spinners on 18 CPUs
/// (16-18 ms of CPU in about 1.15 s; `cli.rs`' `STARVED_RUNS_ONE_IN`).
const STARVED_RUNS_ONE_IN: u32 = 64;

#[link(name = "ntdll")]
unsafe extern "system" {
  fn NtSuspendProcess(process: HANDLE) -> i32;
  fn NtResumeProcess(process: HANDLE) -> i32;
}

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a client verb; (exit code, stdout, stderr).
fn run(instance: &str, args: &[&str]) -> (i32, String, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// The serving process, through the command's public status response; absent while no daemon answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, text, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  let after = text.split("\"pid\":").nth(1)?;
  after
    .trim_start()
    .split(|character: char| !character.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// The daemon that answers, other than `replaced`, within [`START_WAIT`].
fn await_daemon(instance: &str, replaced: Option<u32>) -> u32 {
  let started = Instant::now();
  loop {
    if let Some(pid) = daemon_pid(instance).filter(|pid| Some(*pid) != replaced) {
      return pid;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "no daemon other than {replaced:?} answered at {instance} within {START_WAIT:?}"
    );
    pause();
  }
}

/// The anchor process, killed and reaped on drop, so a failed assertion leaves no daemon behind (the daemon leaves
/// when its anchor dies); its standard error is echoed, and each line also sent to [`AnchorProcess::lines`].
struct AnchorProcess {
  child: Child,
  lines: Receiver<String>,
  drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AnchorProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    if let Some(drain) = self.drain.take() {
      let _ = drain.join();
    }
  }
}

impl AnchorProcess {
  /// The next line the anchor writes, within [`START_WAIT`].
  fn next_line(&self) -> String {
    match self.lines.recv_timeout(START_WAIT) {
      Ok(line) => line,
      Err(RecvTimeoutError::Timeout) => panic!("the anchor wrote nothing in {START_WAIT:?}"),
      Err(RecvTimeoutError::Disconnected) => panic!("the anchor ended (its lines are above)"),
    }
  }

  /// The pid the anchor's next restart line names: the successor it started.
  fn next_restart(&self) -> u32 {
    loop {
      let line = self.next_line();
      if let Some(after) = line.split(RESTARTED_AS).nth(1) {
        return after
          .split(|character: char| !character.is_ascii_digit())
          .next()
          .and_then(|pid| pid.parse().ok())
          .unwrap_or_else(|| panic!("no pid in the restart line {line:?}"));
      }
    }
  }

  /// The anchor's lines written so far that kill a starting daemon.
  fn start_kills(&self) -> usize {
    self
      .lines
      .try_iter()
      .filter(|line| line.contains(START_KILL_LINE))
      .count()
  }
}

/// Starts `slates --instance INSTANCE anchor --quick --shards SHARDS`, waits for its daemon to serve and answer a
/// streak of verbs, and bootstraps its consensus group.
fn start_anchor(instance: &str) -> AnchorProcess {
  let mut child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let (send, lines) = std::sync::mpsc::channel();
  let drain = std::thread::spawn(move || {
    for line in stderr.lines() {
      let Ok(line) = line else { break };
      eprintln!("{line}");
      if send.send(line).is_err() {
        break;
      }
    }
  });
  let anchor = AnchorProcess {
    child,
    lines,
    drain: Some(drain),
  };
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  while !anchor.next_line().starts_with(&serving) {}
  let mut streak = 0u32;
  for _ in 0..START_ATTEMPTS {
    let (code, _, _) = run(instance, &["volume", "list"]);
    streak = if code == 0 { streak + 1 } else { 0 };
    if streak >= STABLE_STREAK {
      let (code, _, error) = run(instance, &["bootstrap", "root"]);
      assert_eq!(code, 0, "explicit first-time bootstrap: {error}");
      return anchor;
    }
    pause();
  }
  panic!(
    "the serving daemon answered {STABLE_STREAK} verbs in a row within {START_ATTEMPTS} attempts"
  );
}

/// A process this test opened by pid, to suspend, resume, end or watch.
struct Opened(OwnedHandle);

impl Opened {
  fn of(pid: u32) -> Opened {
    // SAFETY: the pid came from this test's anchor; only these rights are asked, with no inheritance.
    let process = unsafe {
      OpenProcess(
        PROCESS_SUSPEND_RESUME
          | PROCESS_TERMINATE
          | PROCESS_SYNCHRONIZE
          | PROCESS_QUERY_LIMITED_INFORMATION,
        0,
        pid,
      )
    };
    assert!(
      !process.is_null(),
      "OpenProcess({pid}): {}",
      std::io::Error::last_os_error()
    );
    // SAFETY: the unique owned handle just returned by `OpenProcess`.
    Opened(unsafe { OwnedHandle::from_raw_handle(process) })
  }

  fn suspend(&self) -> bool {
    // SAFETY: a live process handle this test owns, for the call.
    unsafe { NtSuspendProcess(self.0.as_raw_handle()) == 0 }
  }

  fn resume(&self) -> bool {
    // SAFETY: a live process handle this test owns, for the call.
    unsafe { NtResumeProcess(self.0.as_raw_handle()) == 0 }
  }

  /// Ends the process and observes its exit within [`START_WAIT`].
  fn terminate(&self) {
    // SAFETY: a live process handle this test owns; the exit code carries no pointer.
    assert_ne!(unsafe { TerminateProcess(self.0.as_raw_handle(), 1) }, 0);
    assert!(self.exits_within(START_WAIT), "the killed daemon exited");
  }

  /// Whether the process has exited by the end of `wait`.
  fn exits_within(&self, wait: Duration) -> bool {
    let millis = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: a live process handle this test owns, for the bounded wait.
    unsafe { WaitForSingleObject(self.0.as_raw_handle(), millis) == WAIT_OBJECT_0 }
  }
}

/// Ends the anchor's daemon `killed` and returns the successor the anchor starts, opened: the pid its restart line
/// names.
fn successor_of(anchor: &AnchorProcess, killed: u32) -> (u32, Opened) {
  Opened::of(killed).terminate();
  let successor = anchor.next_restart();
  (successor, Opened::of(successor))
}

/// The first daemon's pid, retried within [`START_ATTEMPTS`]: a status that lands in the anchor's one startup restart
/// is refused, and refused without effect.
fn first_daemon(instance: &str) -> u32 {
  for _ in 0..START_ATTEMPTS {
    if let Some(pid) = daemon_pid(instance) {
      return pid;
    }
    pause();
  }
  panic!("the daemon answered no status within {START_ATTEMPTS} attempts");
}

fn gated(flow: &str) -> bool {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the {flow} flow: set SLATES_TEST_CLI=1 to run it (an anchor and its daemon)"
    );
    return false;
  }
  true
}

/// A busy machine never kills a starting daemon that is making progress (the anchor restart loop, 2026-11-07). Do:
/// start an anchor, end its daemon, and starve the successor from as near its first instruction as the restart line
/// allows: suspended all but one sixty-fourth of every heartbeat, for three of the anchor's former start budgets, then
/// resumed. Expect: the starved successor is the one that answers, and the anchor never killed a start.
#[test]
#[allow(clippy::disallowed_methods)] // the test starves a process by a duty cycle of its own: the sleeps are its periods
fn a_starved_start_that_makes_progress_is_not_killed() {
  if !gated("starved-start") {
    return;
  }
  let instance = format!("cli-starved-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let killed = first_daemon(&instance);
  let (successor, process) = successor_of(&anchor, killed);
  let runs = STARVED_PERIOD / STARVED_RUNS_ONE_IN;
  let starving = Instant::now();
  while starving.elapsed() < STARVED_FOR {
    if !process.suspend() {
      break;
    }
    std::thread::sleep(STARVED_PERIOD - runs);
    let _ = process.resume();
    std::thread::sleep(runs);
  }
  let _ = process.resume();
  let answered = await_daemon(&instance, Some(killed));
  let start_kills = anchor.start_kills();
  drop(anchor);
  assert_eq!(
    start_kills, 0,
    "the anchor killed a starved start {start_kills} times"
  );
  assert_eq!(
    answered, successor,
    "the starved successor is the daemon that answers"
  );
}

/// The other side of the starved-start rule: a start that makes no progress at all is still killed, within the
/// liveness window and the suite's start wait. Do: start an anchor, end its daemon, and suspend the successor for good
/// as soon as the restart line names it. Expect: another daemon answers, the suspended one is gone, and the anchor
/// said it killed a starting daemon.
#[test]
fn a_stuck_start_is_killed_and_restarted() {
  if !gated("stuck-start") {
    return;
  }
  let instance = format!("cli-stuck-start-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let killed = first_daemon(&instance);
  let (stuck, process) = successor_of(&anchor, killed);
  assert!(process.suspend(), "NtSuspendProcess({stuck})");
  let stopped = Instant::now();
  let answered = await_daemon(&instance, Some(stuck));
  let replaced_after = stopped.elapsed();
  let stuck_gone = process.exits_within(Duration::ZERO);
  if !stuck_gone {
    // Leave it reapable whatever the assertions say.
    let _ = process.resume();
  }
  let start_kills = anchor.start_kills();
  drop(anchor);
  assert_ne!(answered, stuck);
  assert!(stuck_gone, "the stuck start was killed");
  assert!(
    start_kills >= 1,
    "the anchor killed the stuck start as a start, not at a heartbeat lapse"
  );
  assert!(
    replaced_after <= Duration::from_nanos(slates_server::daemon::LIVENESS_BUDGET_NS) + START_WAIT,
    "killed within the window and replaced: after {replaced_after:?}"
  );
}
