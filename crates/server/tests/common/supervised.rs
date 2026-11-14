//! A test that plays the anchor across a spawn (A-232, §2.6 boot step 2, §4.14 `daemon.alive`): the test binary
//! re-invoked as a role runs the test's body beside a daemon attached from the environment (`SegmentSource::FromEnv`),
//! while this process holds the segment and supervises the child through the anchor's own [`Supervisor`]. Only such a
//! daemon is supervised, since only another process can end it: a daemon over a segment it made (`Create`) or one an
//! anchor in its own process handed over (`Handoff`) ends its own process on its watchdog's stuck verdict. So a test
//! that keeps a shard stuck past a window while it watches the daemon go on (its beat stop, an observation's deadline,
//! a peer's refusal) runs its body here, where the daemon, like an anchored one, withholds its beat and lives until its
//! anchor ends it; this anchor never kills on a lapse, so the body runs to its end. The role passes or fails as a test
//! does, and the parent asserts its exit code.
//!
//! The parent measures the profile once, publishes it into the segment, and the child derives its configuration from
//! that same profile, as `slates daemon` does under `slates anchor` (`crates/cli/src/daemon.rs`, `published_profile`),
//! so the geometry the segment was made with is the geometry the daemon expects.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::unwrap_in_result
)]

use std::time::{Duration, Instant};

use slates_anchor::supervise::{RestartPolicy, Step, Supervisor};
use slates_anchor::{AnchorSegment, RegionKind};
use slates_machine::MachineProfile;
use slates_machine::facts::Facts;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the environment variables that name the role a child runs, its instance and its shard count.
const ROLE_ENV: &str = "SLATES_SUPERVISED_ROLE";
const INSTANCE_ENV: &str = "SLATES_SUPERVISED_INSTANCE";
const SHARDS_ENV: &str = "SLATES_SUPERVISED_SHARDS";
/// Shape: how long a role may run: a hundred liveness windows, far past any role's own bounds, so a role that hangs
/// fails here by name rather than holding the suite.
const ROLE_WAIT: Duration = Duration::from_nanos(100 * slates_server::daemon::LIVENESS_BUDGET_NS);
/// Shape: the parent's pause between supervision steps: a tenth of a beat.
const STEP_PAUSE: Duration = Duration::from_nanos(slates_server::daemon::HEARTBEAT_NS / 10);

/// Runs this test binary's ignored test `role` as a supervised daemon's process of `shards` shards, the test playing its
/// anchor, and asserts the role passed. `name` names the instance and the segment.
pub(crate) fn run(role: &str, name: &str, shards: u16) {
  let profile = super::machine_profile();
  let instance = format!("sup-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(shards));
  let mut segment = AnchorSegment::create(
    &super::fixture_object("slates-seg", name),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &super::fixture_object("slates-con", name),
    config.content_bytes(),
  )
  .unwrap();
  segment
    .publish(RegionKind::Profile, profile.to_json().unwrap().as_bytes())
    .unwrap();
  let args: Vec<String> = ["--ignored", "--exact", role, "--nocapture"]
    .into_iter()
    .map(str::to_owned)
    .collect();
  // No restart: the role runs once, and its exit is the test's verdict.
  let policy = RestartPolicy {
    window_ns: u64::MAX,
    max_restarts: 0,
  };
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
  supervisor.hold_environment(ROLE_ENV, role.to_owned());
  supervisor.hold_environment(INSTANCE_ENV, instance);
  supervisor.hold_environment(SHARDS_ENV, shards.to_string());
  let clock = Instant::now();
  let now_ns = || u64::try_from(clock.elapsed().as_nanos()).unwrap_or(u64::MAX);
  supervisor.start(now_ns()).unwrap();
  let exit = loop {
    match supervisor.step(now_ns()).unwrap() {
      Step::Restarted { exit_code, .. } | Step::CrashLoop { exit_code, .. } => break exit_code,
      _ if clock.elapsed() > ROLE_WAIT => {
        let _ = supervisor.kill();
        panic!("the supervised role {role} was still running after {ROLE_WAIT:?}");
      }
      _ => {
        // The harness paces its supervision; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(STEP_PAUSE);
      }
    }
  };
  assert_eq!(
    exit,
    Some(0),
    "the supervised role {role} passed (its output is above)"
  );
}

/// The daemon a supervised role runs beside, when this process is that role's child: attached from the environment its
/// anchor gave it, derived from the profile the anchor published; with its instance. `None` in any other process, so a
/// role run directly does nothing.
pub(crate) fn daemon(role: &str) -> Option<(Daemon, String)> {
  daemon_with(role, |config| config)
}

/// [`daemon`], its configuration adjusted by `adjust` before it starts (a boot test's fault).
pub(crate) fn daemon_with(
  role: &str,
  adjust: impl FnOnce(DaemonConfig) -> DaemonConfig,
) -> Option<(Daemon, String)> {
  if std::env::var(ROLE_ENV).ok().as_deref() != Some(role) {
    return None;
  }
  let instance = std::env::var(INSTANCE_ENV).unwrap();
  let shards: u16 = std::env::var(SHARDS_ENV).unwrap().parse().unwrap();
  let identity = Facts::query().identity;
  let published = AnchorSegment::attach_from_env(&identity)
    .unwrap()
    .read_published(RegionKind::Profile)
    .unwrap()
    .expect("the anchor published its profile");
  let profile = MachineProfile::from_json(&String::from_utf8(published).unwrap()).unwrap();
  let config = adjust(DaemonConfig::derive(&profile, &instance, Some(shards)));
  let daemon = Daemon::start(&profile, config, SegmentSource::FromEnv).unwrap();
  Some((daemon, instance))
}
