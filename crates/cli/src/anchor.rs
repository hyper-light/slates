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

use slates_anchor::{AnchorSegment, RegionKind, RestartPolicy, Stopping, Supervisor};
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
  // own copy, so the socket outlives any one daemon). If that is refused the listener is not held at
  // all: handing the daemon a descriptor number it will not inherit would have it adopt whatever that
  // number names in its own table. The daemon then binds its own listener, as when the bind failed.
  if let Err(e) = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()) {
    eprintln!(
      "slates anchor: the NFS listener cannot be made inheritable ({e}); the daemon binds its own"
    );
    return;
  }
  supervisor.hold_nfs_listener(fd);
}

/// Takes up the fleet node's two serve sockets at its plan's addresses (bound, or adopted when this
/// anchor was itself handed them) and gives them to `supervisor` to hand to every daemon it spawns (§4.8,
/// [`slates_anchor::ENV_FLEET_SERVE`]). Each descriptor is made inheritable across the spawn; the
/// supervisor keeps its own copy for its life.
#[cfg(unix)]
fn hold_fleet_serve(
  supervisor: &mut Supervisor,
  selection: &crate::args::FleetSelection,
) -> Result<(), Failure> {
  let plan = crate::fleet::load(selection)?;
  let serve = plan
    .transport
    .serve
    .bind()
    .map_err(|e| failed("fleet serve", e))?;
  let inheritable = |socket: slates_rt::udp::UdpSocket, plane: &str| {
    let fd = socket
      .into_owned()
      .map_err(|e| failed("fleet serve", format!("the {plane} socket: {e:?}")))?;
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).map_err(|e| {
      failed(
        "fleet serve",
        format!("the {plane} socket cannot be made inheritable: {e}"),
      )
    })?;
    Ok::<_, Failure>(fd)
  };
  let probe = inheritable(serve.probe, "probe")?;
  let record = inheritable(serve.record, "record")?;
  // The network export's listener (§4.6, AUD-29-75), held and handed over the same way when the plan has one.
  let export = match serve.export {
    Some(listener) => {
      let fd = listener.into_fd();
      rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).map_err(|e| {
        failed(
          "fleet serve",
          format!("the export listener cannot be made inheritable: {e}"),
        )
      })?;
      Some(fd)
    }
    None => None,
  };
  supervisor.hold_fleet_serve(probe, record, export);
  Ok(())
}

fn failed(what: &str, e: impl std::fmt::Display) -> Failure {
  Failure::Failed(format!("{what}: {e}"))
}

/// Runs the anchor until a stop is requested or the daemon loops.
pub(crate) fn run(options: &ProcessOptions) -> Result<(), Failure> {
  // Before the segment holds a byte: no core dump may carry it (AUD-29-41).
  crate::dumps::exclude_from_dumps()?;
  signal::install().map_err(Failure::Failed)?;
  let profile: MachineProfile = measure(options.quick)?;
  let config = DaemonConfig::derive(&profile, &options.instance, options.shards);
  // The anchor-owned content object that holds each shard's recovery image (§4.8): one shard's
  // reserve times the partitions, lazily backed so its unused tail costs no RAM. The anchor holds
  // it across daemon restarts and hands it off, so an agent's writes survive a restart (BUG-11).
  let seg_name = segment_name(&options.instance);
  let content_name = seg_name.replacen("slates-seg-", "slates-con-", 1);
  let content_bytes = config.content_bytes();
  let mut segment = AnchorSegment::create(&seg_name, &profile.facts.identity, config.geometry)
    .and_then(|s| s.with_content(&content_name, content_bytes))
    .map_err(|e| failed("segment", e))?;
  // The page the node's sealing root lives on stays locked between daemons too (A-92): the anchor maps it all along.
  // A refusal leaves it to the daemon, which then reports sealing unavailable rather than publish a root there.
  if let Err(e) = segment.protect_seal_page() {
    eprintln!("slates anchor: the sealing page could not be locked: {e}");
  }
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
  // A fleet node's daemon joins the fleet; the anchor only passes the selection through (the daemon
  // reads the manifest, so a restart re-reads an operator's edit).
  if let Some(fleet) = &options.fleet {
    args.push("--fleet".to_owned());
    args.push(fleet.manifest.clone());
    args.push("--node".to_owned());
    args.push(fleet.node.clone());
  }
  // Until a start is measured, the budget holds one start: a daemon that fails once
  // before it ever beat is restarted once.
  let policy = RestartPolicy::derive(RECOVERY_BUDGET_NS, RECOVERY_BUDGET_NS);
  let mut supervisor = Supervisor::new(segment, &exe, &args, policy.get());
  // Hold the NFS loopback listener in the anchor so its port survives a daemon restart (§4.6): bind
  // it once and hand it to every daemon the supervisor spawns; the daemon adopts it.
  #[cfg(unix)]
  hold_nfs_listener(&mut supervisor);
  // A fleet node's serve sockets are held the same way (§4.8): bound once here, at the plan's addresses,
  // and handed to every daemon, so the manifest's ports are never free between a stop and a restart. A
  // socket that cannot be bound refuses the anchor's start by name — a fleet node that cannot serve its
  // planned ports must not run.
  #[cfg(unix)]
  if let Some(selection) = &options.fleet {
    hold_fleet_serve(&mut supervisor, selection)?;
  }
  // The FUSE devices daemons mount are held the same way (A-61): a daemon sends each to the anchor, and the
  // next daemon takes back the mounts it recovers. A channel that cannot be opened refuses the anchor's start:
  // a mount would then die with its daemon, which the anchor exists to prevent.
  #[cfg(target_os = "linux")]
  supervisor
    .hold_devices(config.held_device_bound())
    .map_err(|e| failed("device channel", e))?;
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
  announce_issuer_surface(&supervisor);
  observe(&mut supervisor, &mut clock, now)
}

/// The issuer surface (§4.13 "Grants"; `slates grant`, `enroll`, `revoke`, `run`): a command that
/// attaches this segment from its environment proves the human's authority under the issuer secret
/// the daemon publishes there. On macOS and Windows the handoff is the object's name — the object's
/// mode and per-user name are the authentication, so any process of this user may open it — and it is
/// printed for the anchor's shell to export.
#[cfg(not(target_os = "linux"))]
fn announce_issuer_surface(supervisor: &Supervisor) {
  if let Ok(handoff) = supervisor.segment().handoff_env() {
    let exports: Vec<String> = handoff
      .iter()
      .map(|(name, value)| format!("{name}={value}"))
      .collect();
    eprintln!(
      "slates anchor: issuer surface: export {}",
      exports.join(" ")
    );
  }
}

/// On Linux the handoff is a descriptor (a memfd) only this process's children hold; there is
/// nothing a shell could export, so nothing is printed.
#[cfg(target_os = "linux")]
fn announce_issuer_surface(_supervisor: &Supervisor) {}

/// Stops the daemon through the graceful protocol (`slates_anchor::layout::SUP_STOP`): asks it to stop, then
/// steps the stop each heartbeat until it exits — on its own after handing off any consensus leadership it
/// held, or killed when it does not acknowledge within the liveness budget, lets its heartbeat lapse, or
/// outruns the deadline it declared ([`Supervisor::stop_step`]). Bounded by those three.
fn stop_gracefully(supervisor: &mut Supervisor, clock: &mut HostClock) -> Result<(), Failure> {
  supervisor
    .request_stop(clock.monotonic_ns())
    .map_err(|e| failed("stop", e))?;
  loop {
    match supervisor
      .stop_step(clock.monotonic_ns(), LIVENESS_BUDGET_NS)
      .map_err(|e| failed("stop", e))?
    {
      Stopping::Draining => std::thread::park_timeout(Duration::from_nanos(HEARTBEAT_NS)),
      Stopping::Exited { exit_code, killed } => {
        eprintln!(
          "slates anchor: stopped (daemon exit {exit_code:?}{})",
          if killed { ", killed" } else { "" }
        );
        return Ok(());
      }
    }
  }
}

/// The observation loop.
fn observe(
  supervisor: &mut Supervisor,
  clock: &mut HostClock,
  started: u64,
) -> Result<(), Failure> {
  let mut starting_since = Some(started);
  let mut longest_start_ns = 0u64;
  let mut watch = Watch::default();
  loop {
    if signal::stop_requested() {
      stop_gracefully(supervisor, clock)?;
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
          let (dead, next) = lapsed(watch, sup.heartbeat_ns(), now, LIVENESS_BUDGET_NS);
          watch = next;
          // Running and beaten at least once, as the segment judges it; the age is the watch's, which does not count
          // a span the anchor itself did not observe.
          (sup.heartbeat_ns(), sup.alive(now, u64::MAX).0 && !dead)
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

/// What the observation loop knows between turns to judge a heartbeat fairly: when it last observed, and since when
/// it may hold the daemon to the budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Watch {
  /// The anchor's clock at its previous observation.
  last_observed_ns: u64,
  /// The moment the anchor resumed after a gap of its own longer than the budget: the daemon's heartbeat is aged
  /// from no earlier than this.
  resumed_ns: u64,
}

/// Whether the daemon's heartbeat, last beaten at `beat_ns`, has lapsed by the anchor's clock `now_ns`, and the watch
/// for the next turn (§4.14 `daemon.alive`). A watchdog may not blame its subject for time it did not watch: when the
/// anchor itself went more than `budget_ns` without observing — the host slept (both clocks include a suspend), the
/// whole machine was paused, or the anchor was starved — the daemon went unobserved for that span too, and its stale
/// heartbeat says nothing about it. The heartbeat is then aged from the anchor's resumption, so a daemon is killed
/// only after a whole budget the anchor watched without a beat. Before 2026-10-04 the age ran from the beat alone,
/// so a laptop waking from sleep killed a healthy daemon whenever the anchor ran before the daemon's next beat.
fn lapsed(watch: Watch, beat_ns: u64, now_ns: u64, budget_ns: u64) -> (bool, Watch) {
  let resumed_ns =
    if watch.last_observed_ns > 0 && now_ns.saturating_sub(watch.last_observed_ns) > budget_ns {
      now_ns
    } else {
      watch.resumed_ns
    };
  let age = now_ns.saturating_sub(beat_ns.max(resumed_ns));
  (
    age > budget_ns,
    Watch {
      last_observed_ns: now_ns,
      resumed_ns,
    },
  )
}

#[cfg(test)]
mod lapse_tests {
  use super::*;

  /// Shape: the budget the tests judge by (the anchor's, a second).
  const BUDGET: u64 = 1_000_000_000;
  /// Shape: the observation cadence (a tenth of the budget).
  const TURN: u64 = BUDGET / 10;

  /// §4.14: do observe a daemon that beats every turn, then one that stops beating while the anchor keeps watching;
  /// expect no lapse while it beats, and a lapse once a whole watched budget passes without a beat.
  #[test]
  fn a_daemon_that_stops_beating_under_a_watching_anchor_lapses() {
    let mut watch = Watch::default();
    let mut now = BUDGET;
    for _ in 0..20 {
      let (dead, next) = lapsed(watch, now, now, BUDGET);
      assert!(!dead, "a beating daemon never lapses");
      watch = next;
      now += TURN;
    }
    let last_beat = now - TURN;
    let mut killed_at = None;
    for _ in 0..20 {
      let (dead, next) = lapsed(watch, last_beat, now, BUDGET);
      watch = next;
      if dead {
        killed_at = Some(now);
        break;
      }
      now += TURN;
    }
    let killed_at = killed_at.expect("the silent daemon lapsed");
    assert!(killed_at - last_beat > BUDGET && killed_at - last_beat <= BUDGET + TURN);
  }

  /// §4.14 (2026-10-04): do suspend the whole host for an hour between two observations — the daemon's last beat
  /// just before it — and observe on wake before the daemon beats again; expect no lapse, and a lapse only if the
  /// daemon then stays silent for a whole budget of the anchor's watching.
  #[test]
  fn a_host_that_slept_does_not_kill_the_daemon_it_could_not_watch() {
    let (_, watch) = lapsed(Watch::default(), BUDGET, BUDGET, BUDGET);
    let slept = 3_600 * BUDGET;
    let wake = BUDGET + slept;
    let (dead, watch) = lapsed(watch, BUDGET, wake, BUDGET);
    assert!(
      !dead,
      "the anchor did not watch the sleep, so the stale beat is not the daemon's lapse"
    );
    let (dead, watch) = lapsed(watch, BUDGET, wake + BUDGET / 2, BUDGET);
    assert!(!dead, "half a watched budget after the wake");
    let (dead, _) = lapsed(watch, BUDGET, wake + BUDGET + TURN, BUDGET);
    assert!(dead, "a whole watched budget after the wake without a beat");
  }
}
