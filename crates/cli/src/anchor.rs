//! `slates anchor` (§2.5 "The anchor process", §2.6 step 1, §4.14 `daemon.alive`): measure
//! the profile, create the segment, publish the profile, supervise the daemon.
//!
//! The loop is the supervisor's `step` at the heartbeat cadence with two observations the
//! supervisor itself does not make: a daemon that never beats inside the recovery budget is
//! killed (a start that is not a start), and a daemon whose heartbeat lapses past the
//! liveness budget is killed (wedged, so a failure like a crash; the policy then decides).
//! The restart policy is re-derived from the measured daemon start: the longest start seen
//! is the p99 until enough starts exist to take a real one.

use std::time::Duration;

use slates_anchor::{AnchorSegment, RegionKind, RestartPolicy, Supervisor};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_machine::MachineProfile;
use slates_server::DaemonConfig;
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS};
use slates_vfs::clock::{Clock, HostClock};

use crate::args::ProcessOptions;
use crate::daemon::{measure, segment_name};
use crate::{Failure, signal};

/// What the anchor waits between observations: the daemon's heartbeat cadence, so a lapse
/// is seen within one beat of the budget.
fn observation_pause() -> Duration {
  Duration::from_nanos(HEARTBEAT_NS)
}

/// Binds the NFS loopback listener the anchor holds across daemon restarts (§4.6) and hands it to
/// `supervisor`, which passes its descriptor to every daemon it spawns. A bind failure is non-fatal:
/// the daemon then binds its own ephemeral listener (its port is not stable across a restart, but the
/// mount still works). Unix only — NFS is the macOS/Linux bridge; Windows uses WinFsp.
#[cfg(unix)]
fn hold_nfs_listener(supervisor: &mut Supervisor) {
  use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
  /// Shape: pending NFS connections the kernel queues before the accept loop takes them (matched to
  /// the daemon's own backlog); the OS clamps it to the system maximum anyway.
  const NFS_BACKLOG: i32 = 16;
  let Ok(listener) = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), NFS_BACKLOG)
  else {
    return;
  };
  let fd = listener.into_fd();
  // Clear close-on-exec so the daemon inherits the descriptor across the spawn (the anchor keeps its
  // own copy, so the socket outlives any one daemon).
  let _ = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty());
  supervisor.hold_nfs_listener(fd);
}

fn failed(what: &str, e: impl std::fmt::Display) -> Failure {
  Failure::Failed(format!("{what}: {e}"))
}

/// Runs the anchor until a stop is requested or the daemon loops.
pub(crate) fn run(options: &ProcessOptions) -> Result<(), Failure> {
  signal::install().map_err(Failure::Failed)?;
  let profile: MachineProfile = measure(options.quick);
  let mut config = DaemonConfig::derive(&profile, &options.instance);
  if let Some(shards) = options.shards {
    config = config.with_shards(shards);
  }
  // The anchor-owned content object that holds each shard's recovery image (§4.8): one shard's
  // reserve times the partitions, lazily backed so its unused tail costs no RAM. The anchor holds
  // it across daemon restarts and hands it off, so an agent's writes survive a restart (BUG-11).
  let seg_name = segment_name(&options.instance);
  let content_name = seg_name.replacen("slates-seg-", "slates-con-", 1);
  let content_bytes = usize::try_from(config.reserve_per_shard)
    .unwrap_or(usize::MAX)
    .saturating_mul(usize::from(config.geometry.partitions.max(1)));
  let mut segment = AnchorSegment::create(&seg_name, &profile.facts.identity, config.geometry)
    .and_then(|s| s.with_content(&content_name, content_bytes))
    .map_err(|e| failed("segment", e))?;
  let json = profile.to_json().map_err(|e| failed("profile", e))?;
  segment
    .publish(RegionKind::Profile, json.as_bytes())
    .map_err(|e| failed("publish", e))?;
  let exe = std::env::current_exe()
    .map_err(|e| failed("current_exe", e))?
    .to_string_lossy()
    .into_owned();
  let mut args = vec![
    "--instance".to_owned(),
    options.instance.clone(),
    "daemon".to_owned(),
  ];
  if options.quick {
    args.push("--quick".to_owned());
  }
  if let Some(shards) = options.shards {
    args.push("--shards".to_owned());
    args.push(shards.to_string());
  }
  // Until a start is measured, the budget holds one start: a daemon that fails once
  // before it ever beat is restarted once.
  let policy = RestartPolicy::derive(RECOVERY_BUDGET_NS, RECOVERY_BUDGET_NS);
  let mut supervisor = Supervisor::new(segment, &exe, &args, policy.get());
  // Hold the NFS loopback listener in the anchor so its port survives a daemon restart (§4.6): bind
  // it once and hand it to every daemon the supervisor spawns; the daemon adopts it.
  #[cfg(unix)]
  hold_nfs_listener(&mut supervisor);
  let mut clock = HostClock::new();
  let now = clock.monotonic_ns();
  supervisor.start(now).map_err(|e| failed("start", e))?;
  eprintln!(
    "slates anchor: instance {} up; daemon pid {}; {} shards",
    options.instance,
    supervisor
      .segment()
      .supervision()
      .map(|s| s.pid())
      .unwrap_or(0),
    config.runtime.shards
  );
  observe(&mut supervisor, &mut clock, now)
}

/// The observation loop.
fn observe(
  supervisor: &mut Supervisor,
  clock: &mut HostClock,
  started: u64,
) -> Result<(), Failure> {
  let mut starting_since = Some(started);
  let mut longest_start_ns = 0u64;
  loop {
    if signal::stop_requested() {
      supervisor.stop().map_err(|e| failed("stop", e))?;
      eprintln!("slates anchor: stopped");
      return Ok(());
    }
    let now = clock.monotonic_ns();
    match supervisor.step(now).map_err(|e| failed("step", e))? {
      slates_anchor::Step::Running => {
        let (heartbeat_ns, alive) = {
          let sup = supervisor
            .segment()
            .supervision()
            .map_err(|e| failed("supervision", e))?;
          (sup.heartbeat_ns(), sup.alive(now, LIVENESS_BUDGET_NS).0)
        };
        match starting_since {
          Some(since) if heartbeat_ns >= since => {
            longest_start_ns = longest_start_ns.max(now.saturating_sub(since));
            supervisor
              .set_policy(RestartPolicy::derive(RECOVERY_BUDGET_NS, longest_start_ns).get());
            starting_since = None;
          }
          Some(since) if now.saturating_sub(since) > RECOVERY_BUDGET_NS => {
            eprintln!(
              "slates anchor: the daemon never beat inside the recovery budget; killing it"
            );
            supervisor.kill().map_err(|e| failed("kill", e))?;
            starting_since = None;
          }
          Some(_) => {}
          None if !alive => {
            eprintln!("slates anchor: the daemon's heartbeat lapsed; killing it");
            supervisor.kill().map_err(|e| failed("kill", e))?;
          }
          None => {}
        }
        std::thread::park_timeout(observation_pause());
      }
      slates_anchor::Step::Restarted {
        exit_code,
        in_window,
      } => {
        eprintln!(
          "slates anchor: daemon exited ({exit_code:?}); restarted ({in_window} in the window)"
        );
        starting_since = Some(clock.monotonic_ns());
      }
      slates_anchor::Step::CrashLoop { exit_code } => {
        return Err(Failure::Failed(format!(
          "the daemon is in a crash loop (last exit {exit_code:?}); giving up"
        )));
      }
      slates_anchor::Step::Stopped => return Ok(()),
    }
  }
}
