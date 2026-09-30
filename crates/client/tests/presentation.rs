//! AUD-29-07 (§4.15 step 3; D-16): a landing presented and never landed expires with the client that
//! presented it — the reaper, retiring a dead client on every owner shard, drops its presentations there. The
//! test binary re-invoked is the presenter (an environment variable selects the role), so the death is a real
//! `SIGKILL` of a real process: an in-process client never reads as gone, because the reaper retires only a
//! silent client whose process or control socket has gone. Its own binary, so no other test's reaping moves
//! the daemon's process-wide counters under it.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slates_client::{Client, Deadlines, Filter, Landing, NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the environment variable selecting the presenter role, and the ones carrying its instance and its
/// landing target.
const ROLE: &str = "SLATES_PRESENTATION_TEST_ROLE";
const ROLE_INSTANCE: &str = "SLATES_PRESENTATION_TEST_INSTANCE";
const ROLE_TARGET: &str = "SLATES_PRESENTATION_TEST_TARGET";
/// Shape: the probe budget of the quick profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: how long to wait for the reaper: five liveness budgets (it retires a client silent for one budget
/// at its next sweep, one budget later).
const WAIT: Duration = Duration::from_nanos(5 * LIVENESS_BUDGET_NS);
/// Shape: the pause between polls.
const POLL_MS: u64 = 20;

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn pause() {
  // The harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

/// The presenter: connects, creates a volume, presents its landing into the target (no grant, so nothing is
/// written), prints the landing id, and waits to be killed.
fn presenter() {
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let target = std::env::var(ROLE_TARGET).unwrap();
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  let volume = client
    .create(&slates_client::CreateSpec {
      name: "presented".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let presented = client
    .land(volume, None, &target, Filter::default(), None)
    .unwrap();
  let Landing::GrantRequired { landing, .. } = presented else {
    panic!("the landing was not presented: {presented:?}");
  };
  println!("presented: {landing}");
  loop {
    std::thread::park();
  }
}

/// Own the presenter through every assertion, including failures before the deliberate kill.
struct Presenter(Child);

impl Drop for Presenter {
  fn drop(&mut self) {
    if self
      .0
      .try_wait()
      .expect("inspect the owned presenter")
      .is_none()
    {
      self.0.kill().expect("kill the owned presenter");
      self.0.wait().expect("reap the owned presenter");
    }
  }
}

/// Spawns the presenter and reads the landing id it presented.
fn start_presenter(instance: &str, target: &str) -> (Presenter, u64) {
  let mut child = Presenter(
    Command::new(std::env::current_exe().unwrap())
      .args([
        "--exact",
        "a_killed_clients_presented_landing_is_abandoned_with_it",
        "--nocapture",
      ])
      .env(ROLE, "presenter")
      .env(ROLE_INSTANCE, instance)
      .env(ROLE_TARGET, target)
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  // The harness prints its own banner first; the presenter's line is tagged.
  let landing = BufReader::new(child.0.stdout.take().unwrap())
    .lines()
    .map(|line| line.expect("read the presenter's report"))
    .find_map(|line| {
      line
        .strip_prefix("presented: ")
        .map(|number| number.trim().parse().unwrap())
    })
    .expect("the presenter printed its landing");
  (child, landing)
}

/// The landings the daemon holds awaiting a grant, over every shard.
fn landings_awaiting(observer: &mut Client) -> u64 {
  observer
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .map(|shard| shard.landings_awaiting)
    .sum()
}

/// AUD-29-07 (§4.15 step 3): a landing presented and never landed expires with the client that presented
/// it. Do: a child process presents a landing (into this crate's directory, opened read-only: nothing is
/// granted, so nothing is written) and is killed. Expect: the daemon counts the presentation awaiting a grant
/// while the child lives, and none once the reaper has retired the child — within the reclaim bound. Before
/// 2026-09-29 a presentation outlived its client, and every landing, for good.
#[test]
fn a_killed_clients_presented_landing_is_abandoned_with_it() {
  if std::env::var_os(ROLE).is_some() {
    presenter();
    return;
  }
  let profile = MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("cl-present-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-present-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut observer = Client::connect(&instance, deadlines()).unwrap();
  let (mut child, _landing) = start_presenter(&instance, env!("CARGO_MANIFEST_DIR"));
  assert_eq!(
    landings_awaiting(&mut observer),
    1,
    "the presentation waits while its client lives"
  );
  child.0.kill().unwrap();
  child.0.wait().unwrap();
  let started = Instant::now();
  while landings_awaiting(&mut observer) != 0 {
    assert!(
      started.elapsed() < WAIT,
      "the killed client's presentation outlived it by {WAIT:?}"
    );
    pause();
  }
  drop(observer);
  drop(daemon);
}
