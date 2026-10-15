//! A-114: the client rendezvous outlives its daemon. The real `slates anchor` holds the rendezvous (the listening socket
//! on Linux, the bootstrap object on macOS and Windows) as it holds the NFS listener; a client that connects as the
//! daemon dies waits for the next daemon and is answered by it. Before, the restarted daemon unlinked the bootstrap
//! object (macOS) or rebound the socket (Linux), and a CLI command started at the kill waited out the one-second claim
//! wait and failed "no daemon" (exit 3), measured 1,014 and 1,016 ms on 2026-10-06, though the restart takes about 7 ms.
//!
//! Gated: `SLATES_TEST_CLI=1` (it starts an anchor and its daemons).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command, Stdio};
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

/// The anchor, killed and reaped on drop, so a failed assertion leaves no daemon behind.
struct Anchor(Child);

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

fn start_anchor(instance: &str) -> Anchor {
  let child = slates()
    .args(["--instance", instance, "anchor", "--quick", "--shards", "2"])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let anchor = Anchor(child);
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

/// A-114, T-2.16. Do: under `slates anchor`, six times `kill -9` the daemon and at once run `slates status --json`.
/// Expect: every one exits 0, answered by a daemon other than the one killed, inside the claim wait (before A-114
/// each waited the whole claim wait and exited 3).
#[test]
fn a_client_that_connects_as_its_daemon_dies_is_answered_by_the_next_daemon() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the held-rendezvous flow: set SLATES_TEST_CLI=1 to run it (an anchor and its daemons)"
    );
    return;
  }
  let instance = format!("held-rdv-{}", std::process::id());
  let _anchor = start_anchor(&instance);
  let mut longest = Duration::ZERO;
  for round in 0..KILLS {
    let killed = daemon_pid(&instance).expect("a daemon answers");
    let status = Command::new("kill")
      .args(["-9", &killed.to_string()])
      .status()
      .unwrap();
    assert!(status.success(), "kill the daemon");
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
    assert!(
      took < CLAIM_WAIT,
      "round {round}: answered in {took:?}, inside the claim wait"
    );
    #[allow(clippy::disallowed_methods)] // the test keeps its kills inside the restart policy
    std::thread::sleep(BETWEEN_KILLS);
  }
  eprintln!(
    "held rendezvous: {KILLS} connects at a kill, all answered by the next daemon, longest {longest:?}"
  );
}
