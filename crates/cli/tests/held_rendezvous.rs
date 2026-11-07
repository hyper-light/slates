//! A-114: the client rendezvous outlives its daemon. The real `slates anchor` holds the rendezvous (the listening socket
//! on Linux, the bootstrap object on macOS and Windows) as it holds the NFS listener; a client that connects as the
//! daemon dies waits for the next daemon and is answered by it. Before, the restarted daemon unlinked the bootstrap
//! object (macOS) or rebound the socket (Linux), and a CLI command started at the kill waited out the one-second claim
//! wait and failed "no daemon" (exit 3), measured 1,014 and 1,016 ms on 2026-10-06, though the restart takes about 7 ms.
//!
//! Gated: `SLATES_TEST_CLI=1` (it starts an anchor and its daemons).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Shape: how long the daemon may take to come up.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: the poll interval of the waits.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answered verbs before the daemon counts as settled.
const STABLE_STREAK: u32 = 10;
/// Shape: kills, each followed at once by a client's connect, spaced by [`BETWEEN_KILLS`] so they stay inside the
/// anchor's restart policy (at most `recovery budget / start p99` restarts a window; ten kills in two seconds is, by
/// design, a crash loop the anchor stops restarting).
const KILLS: usize = 6;
/// Shape: the pause between rounds.
const BETWEEN_KILLS: Duration = Duration::from_millis(300);
/// Format: the client's claim wait (`slates_ipc::rendezvous::CLAIM_WAIT_NS`): what a connect to a dead daemon waited
/// before failing; a connect answered by the next daemon finishes well inside it.
const CLAIM_WAIT: Duration = Duration::from_nanos(slates_ipc::rendezvous::CLAIM_WAIT_NS);

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

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

fn pause() {
  #[allow(clippy::disallowed_methods)] // the test paces its polls
  std::thread::sleep(POLL);
}

/// Format: the start of the line a daemon prints once it serves (`crates/cli/src/daemon.rs`); the instance follows.
const SERVING_LINE: &str = "slates daemon: instance ";

/// The anchor, killed and reaped on drop, so a failed assertion leaves no daemon behind; each of its daemons' serving
/// lines arrives on `served` with the moment it was read.
struct Anchor {
  child: Child,
  served: Receiver<Instant>,
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

fn start_anchor(instance: &str) -> Anchor {
  let mut child = slates()
    .args(["--instance", instance, "anchor", "--quick", "--shards", "2"])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (send, served) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    for line in std::io::BufRead::lines(stderr).map_while(Result::ok) {
      if line.starts_with(&serving) && send.send(Instant::now()).is_err() {
        break;
      }
    }
  });
  let anchor = Anchor { child, served };
  let started = Instant::now();
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if run(instance, &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    assert!(started.elapsed() < START_WAIT, "the daemon came up");
    pause();
  }
  anchor
}

/// Ends process `pid` at once, as `kill -9` does: `SIGKILL` on Unix, `TerminateProcess` on Windows (where there is no
/// `kill` to run; until 2026-11-07 this test was run on Unix only for that reason).
#[cfg(unix)]
fn kill_hard(pid: u32) {
  let status = Command::new("kill")
    .args(["-9", &pid.to_string()])
    .status()
    .unwrap();
  assert!(status.success(), "kill the daemon");
}

/// See the Unix form.
#[cfg(windows)]
fn kill_hard(pid: u32) {
  use windows_sys::Win32::Foundation::CloseHandle;
  use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
  // SAFETY: the pid came from the daemon's own status; only the right to end it is asked, with no inheritance.
  let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
  assert!(
    !process.is_null(),
    "OpenProcess({pid}): {}",
    std::io::Error::last_os_error()
  );
  // SAFETY: the live handle just opened, used once and closed here.
  let ended = unsafe { TerminateProcess(process, 1) };
  // SAFETY: the same handle, closed once.
  unsafe { CloseHandle(process) };
  assert_ne!(ended, 0, "kill the daemon");
}

/// The daemon's pid, from `status --json`, or `None` while no daemon answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, out, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  let after = out.split("\"pid\":").nth(1)?;
  after
    .trim_start()
    .split(|c: char| !c.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// A-114, T-2.16. Do: under `slates anchor`, six times end the daemon at once (`kill -9`, `TerminateProcess`) and at
/// once run `slates status --json`. Expect: every one exits 0, answered by a daemon other than the one killed, within a
/// claim wait of that daemon's serving (before A-114 each waited the whole claim wait and exited 3).
///
/// The bound is counted from the successor's serving line, not from the kill: a client of a live anchor waits out the
/// restart, however long the machine makes it. Counted from the kill, beside two spinners a CPU on windows-latest
/// (diag-liveness-14) the first round was answered by the next daemon in 1.05-1.28 s, five runs in five, against a
/// 1 s claim wait, while quiet it was 43-47 ms. A client that ran out a claim wait and retried would still be caught:
/// it is answered more than a claim wait after the successor serves.
#[test]
fn a_client_that_connects_as_its_daemon_dies_is_answered_by_the_next_daemon() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the held-rendezvous flow: set SLATES_TEST_CLI=1 to run it (an anchor and its daemons)"
    );
    return;
  }
  let instance = format!("held-rdv-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let mut longest = Duration::ZERO;
  for round in 0..KILLS {
    let killed = daemon_pid(&instance).expect("a daemon answers");
    while anchor.served.try_recv().is_ok() {}
    kill_hard(killed);
    let started = Instant::now();
    let (code, out, err) = run(&instance, &["status", "--json"]);
    let took = started.elapsed();
    longest = longest.max(took);
    assert_eq!(
      code, 0,
      "round {round}: the connect at the kill was answered: {err}"
    );
    let answered_by: u32 = out
      .split("\"pid\":")
      .nth(1)
      .and_then(|after| {
        after
          .trim_start()
          .split(|c: char| !c.is_ascii_digit())
          .next()
      })
      .and_then(|pid| pid.parse().ok())
      .expect("status names its daemon");
    assert_ne!(
      answered_by, killed,
      "round {round}: the next daemon answered"
    );
    let served_after = anchor
      .served
      .recv_timeout(START_WAIT)
      .expect("the next daemon said it serves")
      .saturating_duration_since(started);
    assert!(
      took < served_after + CLAIM_WAIT,
      "round {round}: answered in {took:?}, the next daemon serving after {served_after:?}: within a claim wait of it"
    );
    #[allow(clippy::disallowed_methods)] // the test keeps its kills inside the restart policy
    std::thread::sleep(BETWEEN_KILLS);
  }
  eprintln!(
    "held rendezvous: {KILLS} connects at a kill, all answered by the next daemon, longest {longest:?}"
  );
}
