//! The anchor role, `slates anchor` or a host's own executable (§2.5 "The anchor process", §2.6 step 1, §4.14
//! `daemon.alive`, A-278): measure the profile, create the segment, publish the profile, supervise the daemon.
//!
//! The loop is the supervisor's `step` at the heartbeat cadence with two observations the
//! supervisor itself does not make: a starting daemon that stops making progress before its
//! watchdog takes the beat over is killed (a start that is not a start), and a daemon whose heartbeat lapses past
//! the liveness budget is killed (wedged, so a failure like a crash; the policy then decides).
//! A start is judged in the daemon's own CPU time, read from the OS
//! (`slates_machine::process_clock`), and its beats (A-312: the starting thread beats after each unit of its work), by
//! the rule its watchdog judges each shard by once it runs (`slates_server::watchdog::ShardWatch`), each beat a step:
//! a window with no beat, no CPU progress and never runnable is a start that is stuck, judged by its silence since its
//! last beat, and a window of CPU on one thread with no beat is one that spins; a start that keeps beating or
//! progressing, however slowly and however long in all, is waited for, starved when held off its CPUs, and so is one
//! whose process waits on memory while the host reports a memory stall (A-336: `slates_machine::memory_stall`, and the
//! process's threads read as the watchdog reads a shard's, `slates_rt::process_run_reading`). The window is this
//! anchor's (A-461, [`anchor_window_ns`]): the liveness window raised to how late the anchor's own timed waits come
//! back where that is longer, the watchdog's rule over the anchor's measure. Until 2026-11-07 a start that had not
//! beaten within the fixed recovery budget, a wall second, was killed, and every restart started
//! over: on this Mac at 144 spinners on 18 CPUs the anchor killed 153 starts of 153, each
//! 1.07-1.22 s after its spawn, after it had used 16-18 ms of CPU.
//! The restart policy is the supervisor's (A-119, A-311): a failed start is restarted at once, a
//! second in a row is a crash loop backed off from its own life, doubling to the operator's
//! failover SLO. Until 2026-11-17 the policy was re-derived from the longest start measured, and a
//! start that outlasted its window of a second was never a crash loop (`slates_anchor::supervise`).

use std::time::Duration;

use slates_anchor::{AnchorSegment, RegionKind, RestartPolicy, StopBounds, Stopping, Supervisor};
use slates_machine::MachineProfile;
use slates_server::DaemonConfig;
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS};
#[cfg(test)]
use slates_server::watchdog::Want;
use slates_server::watchdog::{LATENESS_SAMPLES, Look, ShardWatch, Verdict};
use slates_vfs::clock::{Clock, HostClock};

use super::daemon::{measure, segment_name};
use super::{Failure, ProcessOptions, Role, signal};

/// What the anchor waits between observations: the daemon's heartbeat cadence, so a lapse
/// is seen within one beat of the budget.
fn observation_pause() -> Duration {
  Duration::from_nanos(HEARTBEAT_NS)
}

/// The exit watch on the daemon the anchor runs now, rebuilt when the pid changes (a restart).
#[cfg(unix)]
#[derive(Default)]
struct DaemonWatch {
  watched: Option<(u32, slates_ipc::exit_watch::ExitWatch)>,
}

#[cfg(unix)]
impl DaemonWatch {
  /// The watch on `pid`, taken now if the daemon changed since the last call; `None` without a daemon or a watch.
  fn on(&mut self, pid: Option<u32>) -> Option<&slates_ipc::exit_watch::ExitWatch> {
    let pid = pid?;
    if self.watched.as_ref().map(|(watched, _)| *watched) != Some(pid) {
      self.watched = slates_ipc::exit_watch::ExitWatch::on(pid)
        .ok()
        .map(|watch| (pid, watch));
    }
    self.watched.as_ref().map(|(_, watch)| watch)
  }
}

/// Waits out `pause`, or less: when a daemon sends a hold (A-113), or when the daemon exits. Both descriptors are
/// polled with the pause as the timeout, so a hold is taken as it arrives and a dead daemon is restarted the moment it
/// dies — a kill cost up to a whole pause (100 ms) of detection before its restart could begin, time every kernel client
/// of a held mount waited. A signal ends the wait early, as it ended the park.
#[cfg(unix)]
fn await_observation(supervisor: &mut Supervisor, watch: &mut DaemonWatch, pause: Duration) {
  use std::os::fd::BorrowedFd;
  let exit = watch
    .on(supervisor.child_pid())
    .and_then(|watch| watch.readiness());
  let descriptors: Vec<BorrowedFd<'_>> =
    supervisor.hold_channel().into_iter().chain(exit).collect();
  if descriptors.is_empty() {
    std::thread::park_timeout(pause);
    return;
  }
  let mut interest: Vec<rustix::event::PollFd<'_>> = descriptors
    .iter()
    .map(|fd| rustix::event::PollFd::new(fd, rustix::event::PollFlags::IN))
    .collect();
  let timeout = rustix::event::Timespec::try_from(pause).ok();
  // Readable, timed out or interrupted, the loop's next turn drains the channel and observes the daemon; an error
  // here only shortens the wait.
  let _ = rustix::event::poll(&mut interest, timeout.as_ref());
}

/// See the Unix arm: off Unix the anchor parks for its pause.
///
/// Braces rather than a unit struct, deliberately: the shared call site builds it with
/// `DaemonWatch::default()`, and `clippy::default_constructed_unit_structs` denies that for a unit struct —
/// which made the Windows binary's lint red while the Unix one, where this type has a field, was fine.
/// Empty braces keep one call site for both without an `allow`.
#[cfg(not(unix))]
#[derive(Default)]
struct DaemonWatch {}

/// See the Unix arm: the Windows supervisor waits itself ([`Supervisor::await_work`]).
#[cfg(windows)]
fn await_observation(supervisor: &mut Supervisor, _watch: &mut DaemonWatch, pause: Duration) {
  supervisor.await_work(pause);
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
  let fd = listener.into_owned();
  // Clear close-on-exec so the daemon inherits the descriptor across the spawn (the anchor keeps its
  // own copy, so the socket outlives any one daemon). If that is refused the listener is not held at
  // all: handing the daemon a descriptor number it will not inherit would have it adopt whatever that
  // number names in its own table. The daemon then binds its own listener, as when the bind failed.
  if let Err(e) = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()) {
    slates_machine::log_line!(
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
  selection: &super::FleetSelection,
) -> Result<(), Failure> {
  let plan = super::fleet::load(selection)?;
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
      let fd = listener.into_owned();
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

/// Binds the version-control servers' shared HTTP edge's listener at the address the operator gave (`--vcs-http`; A-625
/// part 3), as the daemon binds its own (`slates_server::daemon::bind_vcs_http_listener`), and gives it to `supervisor`
/// to hand to every daemon it spawns ([`slates_anchor::ENV_VCS_HTTP`]), so the edge's port never drops while a daemon
/// restarts: a client that connects then waits in the listener's queue for the next daemon. An address that cannot be
/// bound refuses the anchor's start by name, as a fleet node's serve sockets do: the operator asked for the edge there.
/// One that cannot be made inheritable is not held, and each daemon binds its own, as for the NFS listener.
#[cfg(unix)]
fn hold_vcs_http_listener(
  supervisor: &mut Supervisor,
  config: &DaemonConfig,
  address: slates_rt::tcp::SocketAddrV4,
) -> Result<(), Failure> {
  let listener = slates_server::daemon::bind_vcs_http_listener(config, address)
    .map_err(|e| failed("vcs-http", format!("{address}: {e}")))?;
  let fd = listener.into_owned();
  if let Err(e) = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()) {
    slates_machine::log_line!(
      "slates anchor: the version-control edge's listener cannot be made inheritable ({e}); each daemon binds its own"
    );
    return Ok(());
  }
  supervisor.hold_vcs_http_listener(fd);
  Ok(())
}

/// Binds the version-control servers' SSH edge's listener at the address the operator gave (`--vcs-ssh`; A-684), as the
/// daemon binds its own (`slates_server::daemon::bind_vcs_ssh_listener`), and gives it to `supervisor` to hand to every
/// daemon it spawns ([`slates_anchor::ENV_VCS_SSH`]), as [`hold_vcs_http_listener`] does the HTTP edge's: an address that
/// cannot be bound refuses the anchor's start by name, and one that cannot be made inheritable is not held, each daemon
/// then binding its own.
#[cfg(unix)]
fn hold_vcs_ssh_listener(
  supervisor: &mut Supervisor,
  config: &DaemonConfig,
  address: slates_rt::tcp::SocketAddrV4,
) -> Result<(), Failure> {
  let listener = slates_server::daemon::bind_vcs_ssh_listener(config, address)
    .map_err(|e| failed("vcs-ssh", format!("{address}: {e}")))?;
  let fd = listener.into_owned();
  if let Err(e) = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()) {
    slates_machine::log_line!(
      "slates anchor: the SSH edge's listener cannot be made inheritable ({e}); each daemon binds its own"
    );
    return Ok(());
  }
  supervisor.hold_vcs_ssh_listener(fd);
  Ok(())
}

/// Makes the SSH edge's Ed25519 host key from the system's generator and publishes its seed in `segment`
/// (`slates_anchor::layout::SUP_SSH_HOST_SEED`; A-684), before any daemon reads the segment: every daemon under this
/// anchor serves the edge with it, so a restart keeps the key a client pinned, for as long as the anchor lives. A draw
/// that fails refuses the anchor's start by name, as an address that cannot be bound does: the operator asked for the
/// edge. The copy here is wiped once published.
fn publish_ssh_host_key(segment: &mut AnchorSegment) -> Result<(), Failure> {
  let mut seed = [0u8; slates_anchor::layout::SSH_HOST_SEED_BYTES];
  slates_transport::handshake::secure_random(&mut seed)
    .map_err(|e| failed("vcs-ssh host key", e))?;
  let published = segment
    .publish_ssh_host_seed(&seed)
    .map_err(|e| failed("vcs-ssh host key", e));
  seed.fill(0);
  std::hint::black_box(&seed);
  published
}

fn failed(what: &str, e: impl std::fmt::Display) -> Failure {
  Failure::Failed(format!("{what}: {e}"))
}

/// Runs the anchor until a stop is requested or the daemon loops: [`supervise`] over this executable's `prefix`, then the
/// daemon role's arguments for the same options ([`Role::args`]): `slates --instance NAME daemon ...`, or
/// `kiln slates-role --instance NAME daemon ...` (A-278).
pub(crate) fn run(prefix: &[String], options: &ProcessOptions) -> Result<(), Failure> {
  let mut args = prefix.to_vec();
  args.extend(Role::Daemon(options.clone()).args());
  supervise(options, &args)
}

/// The anchor itself: measure, create the segment, publish the profile, and supervise this very executable started
/// with `daemon_args` until a stop is requested. [`run`] passes the daemon role's arguments after the executable's
/// prefix; a test of this crate passes its own binary's role arguments (`super::liveness_tests`), so the anchor that
/// judges the daemon there is this one, not a copy.
pub(crate) fn supervise(options: &ProcessOptions, daemon_args: &[String]) -> Result<(), Failure> {
  #[cfg(windows)]
  slates_ipc::process_access::confine_current_holder()
    .map_err(|error| failed("process handle authority", error))?;
  // Before the segment holds a byte: no core dump may carry it (AUD-29-41).
  super::dumps::exclude_from_dumps()?;
  signal::install().map_err(Failure::Failed)?;
  // The launcher's secret (A-299): what the starter handed on standard input, read first, so a starter that meant to
  // hand one and failed is refused by its fault before anything is measured, held or created — never left running an
  // instance it can never approve for.
  let launcher = crate::launcher::from_standard_input()
    .map_err(|fault| Failure::Refused(format!("the launcher's secret: {fault}")))?;
  let profile: MachineProfile = measure(options.quick)?;
  // This build's cost, measured here, before any start is judged by it ([`start_spin_window_ns`]), and said once.
  slates_machine::log_line!(
    "slates anchor: a start's busiest thread is judged spinning past {} ms of CPU without a beat at least (the liveness \
     window held to this build, A-318; the window is longer where this anchor's own waits run later, A-461)",
    start_spin_window_ns(LIVENESS_BUDGET_NS) / NS_PER_MS
  );
  let config = DaemonConfig::derive(&profile, &options.instance, options.shards);
  // The owner first (A-254, the rule of A-176 and A-178): a second anchor of a live instance is refused here, having
  // created nothing. On macOS a segment's creation unlinks any holder of its name and its drop unlinks the name again,
  // so a refused start that had created one left the live anchor's next daemon nothing to attach. The client rendezvous
  // is then held the same way as the segment (A-114): a client that connects while a daemon restarts waits for the next
  // one instead of failing "no daemon". Kept for the anchor's life; the daemon adopts it from the environment.
  let rendezvous = slates_ipc::rendezvous::HeldRendezvous::hold(&options.instance)
    .map_err(|e| failed("rendezvous", e))?;
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
    slates_machine::log_line!("slates anchor: the sealing page could not be locked: {e}");
  }
  let json = profile.to_json().map_err(|e| failed("profile", e))?;
  segment
    .publish(RegionKind::Profile, json.as_bytes())
    .map_err(|e| failed("publish", e))?;
  // The launcher's secret (A-299), published before any daemon reads the segment.
  if let Some(secret) = &launcher {
    segment
      .publish_launcher_secret(secret.bytes())
      .map_err(|e| failed("launcher secret", e))?;
  }
  // The SSH edge's host key (A-684), made here at the anchor's start and published the same way, on the page locked
  // above where the OS allows, so every daemon serves the edge with one key and a restart keeps it beside the port held
  // below. A fleet node's edge presents the node's own key, read from its plan by each daemon, so its anchor makes none.
  if options.vcs_ssh.is_some() && options.fleet.is_none() {
    publish_ssh_host_key(&mut segment)?;
  }
  // A crash loop's backoff doubles to its cap (A-311, [`restart_policy`]), logged with its derivation as the daemon's
  // derived constants are.
  let policy = restart_policy(&config);
  slates_machine::log_line!(
    "slates anchor: a crash loop backs off from its failed start's own life, doubling to {} ms: {} (anchors {:?})",
    policy.get().backoff_cap_ns / NS_PER_MS,
    policy.formula,
    policy.anchors
  );
  // The daemon is this very executable, restarted from the image the anchor runs (A-145), never from what its path holds
  // after an upgrade in place.
  let mut supervisor =
    Supervisor::new(segment, daemon_args, policy.get()).map_err(|e| failed("supervise", e))?;
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
  // The version-control edge's listener is held the same way (A-625 part 3): bound once here, at the address the
  // operator gave, and handed to every daemon, so a restart never drops the edge's port.
  #[cfg(unix)]
  if let Some(address) = options.vcs_http {
    hold_vcs_http_listener(&mut supervisor, &config, address)?;
  }
  // And the SSH edge's (A-684), whose host key the segment already holds.
  #[cfg(unix)]
  if let Some(address) = options.vcs_ssh {
    hold_vcs_ssh_listener(&mut supervisor, &config, address)?;
  }
  // The FUSE devices daemons mount are held the same way (A-61): a daemon sends each to the anchor, and the
  // next daemon takes back the mounts it recovers. A channel that cannot be opened refuses the anchor's start:
  // a mount would then die with its daemon, which the anchor exists to prevent.
  #[cfg(unix)]
  supervisor
    .hold_descriptors(
      config.held_device_bound(),
      config.held_connection_bound(),
      config.held_copy_bound(),
    )
    .map_err(|e| failed("hold channel", e))?;
  // Windows holds each class of entry to its own bound: the attachments' mounts and resync ends, and the Shards streams'
  // DAX sections (A-448).
  #[cfg(windows)]
  supervisor
    .hold_handles(slates_anchor::handles::HoldBounds {
      attachments: config.held_device_bound(),
      sections: config.held_dax_bound(),
    })
    .map_err(|error| failed("hold channel", error))?;
  // The anchor holds a duplicate of every device and connection it is sent, so it needs the descriptors its bounds
  // name: raise its soft limit to the hard one, as the daemon does (no privilege needed).
  if let Some((was, now)) = slates_server::daemon::raise_descriptor_limit() {
    slates_machine::log_line!("slates anchor: descriptor limit raised from {was} to {now}");
  }
  supervisor.hold_environment(
    slates_ipc::rendezvous::ENV_RENDEZVOUS,
    rendezvous.adoption().env_value().to_owned(),
  );
  let mut clock = HostClock::new();
  let now = clock.monotonic_ns();
  supervisor.start(now).map_err(|e| failed("start", e))?;
  slates_machine::log_line!(
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
    slates_machine::log_line!(
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
/// held, or killed when it lets its heartbeat lapse, or has beaten this anchor's window past the request without
/// acknowledging it or past the deadline it declared ([`Supervisor::stop_step`]). Each bound is derived from what it
/// waits for (A-461): the lapse is the bound this anchor holds a running daemon's beat to (its liveness window plus its
/// own measured lateness, `lateness`), and the acknowledgement and the overrun are judged in the daemon's own beating
/// against this anchor's window ([`anchor_window_ns`]), so a daemon held off its CPUs mid-drain is waited for while it
/// beats. Bounded by those three.
fn stop_gracefully(
  supervisor: &mut Supervisor,
  clock: &mut HostClock,
  lateness: &Lateness,
) -> Result<(), Failure> {
  supervisor
    .request_stop(clock.monotonic_ns())
    .map_err(|e| failed("stop", e))?;
  let bounds = StopBounds {
    lapse_ns: lateness.lapse_bound_ns(),
    window_ns: anchor_window_ns(lateness),
  };
  loop {
    match supervisor
      .stop_step(clock.monotonic_ns(), bounds)
      .map_err(|e| failed("stop", e))?
    {
      Stopping::Draining => std::thread::park_timeout(Duration::from_nanos(HEARTBEAT_NS)),
      Stopping::Exited { exit_code, killed } => {
        slates_machine::log_line!(
          "slates anchor: stopped (daemon exit {exit_code:?}{})",
          if killed { ", killed" } else { "" }
        );
        return Ok(());
      }
    }
  }
}

/// Publishes `bound`, the age past which this anchor takes its daemon's beat for lapsed, in the supervision block
/// (`slates_anchor::layout::SUP_LAPSE_BOUND`, A-574) when it differs from the one `published` last: the daemon reads it as
/// its anchor's part of its failover detection (a stuck daemon is found by its own watchdog and killed once its withheld
/// beat is this old), so a laptop's failover delay follows what this anchor measures of its own lateness. A block that
/// cannot be read now is written at a later turn.
fn publish_lapse_bound(supervisor: &Supervisor, bound: u64, published: &mut Option<u64>) {
  if *published == Some(bound) {
    return;
  }
  if let Ok(supervision) = supervisor.segment().supervision() {
    supervision.record_lapse_bound(bound);
    *published = Some(bound);
  }
}

/// Derived: the window this anchor judges a daemon's process by (A-461): the liveness window raised to the scheduling
/// delay this anchor measures of its own waits, its lateness p99 over its last timed waits (`lateness`), and never past
/// the watchdog's ceiling: the rule the daemon's watchdog judges its shards by (A-455,
/// `slates_server::watchdog::watch_window_ns`), over this anchor's own measure, since a starting daemon's shards have
/// measured nothing yet and a silent watchdog's are not to be read. A daemon's threads are scheduled by the host that
/// delays this anchor's, so a host that holds this anchor's waits past the liveness window holds a start's that long
/// too, and its process can show no progress sooner; a quiet host's waits come back within a tick, so its window is the
/// liveness window. Before, every start was judged by the bare liveness window whatever the host.
fn anchor_window_ns(lateness: &Lateness) -> u64 {
  slates_server::watchdog::watch_window_ns(lateness.p99_ns())
}

/// One look at the starting daemon in its own CPU time (`slates_machine::process_clock`) and its beats (A-312): its
/// progress, its beat word `beat_ns` and its busiest thread's CPU, judged by the watchdog's rule over this anchor's
/// window `window_ns` ([`anchor_window_ns`], A-461). A beat is its step, so it is alive at a beat and while its CPU
/// progresses, judged spinning once its busiest thread has run the window of CPU held to the build without a beat, and
/// starved, not stuck, while its CPU stands still with one of its threads runnable (`slates_rt::process_run_reading`,
/// A-228's rule), or with none runnable while the host reports a memory stall and its threads' waits are ones memory
/// can show as (A-336: on Linux one held in the kernel, `D`; elsewhere any). Wedged is its silence: a window since its
/// last beat or CPU rise in which it was never runnable. With no clock reading (the process already gone) the wall
/// window stands alone, as for a shard with no thread clock.
fn judge_start(
  supervisor: &Supervisor,
  watch: &mut ShardWatch,
  (now, beat_ns): (u64, u64),
  (window_ns, watcher_late_ns): (u64, u64),
) -> Verdict {
  judge_start_of(
    supervisor.child_pid(),
    watch,
    (now, beat_ns, slates_machine::memory_stall::read()),
    (window_ns, watcher_late_ns),
  )
}

/// [`judge_start`] for the process `pid` names, at the anchor's clock `now`, its beat word `beat_ns`, and the host's
/// memory stall at this look `memory` (`slates_machine::memory_stall::read`), over the window `window_ns`, by an anchor
/// whose own looks run `watcher_late_ns` late at their p99 ([`Lateness::p99_ns`], A-474).
fn judge_start_of(
  pid: Option<u32>,
  watch: &mut ShardWatch,
  (now, beat_ns, memory): (u64, u64, Option<slates_machine::memory_stall::StallReading>),
  (window_ns, watcher_late_ns): (u64, u64),
) -> Verdict {
  // The spinning rule is a thread's: a window of CPU on one thread without a step, so the CPU judged is the busiest
  // thread's. The process's sum is no such reading: a start whose shards begin in parallel sums past a window while no
  // thread nears one (every start of 23 shards on a 32-CPU Studio was killed, 2026-11-08). Where the threads cannot be
  // read nothing is judged spinning, and the progress alone judges a start wedged.
  let clock = || {
    pid
      .and_then(slates_machine::process_clock::process_cpu)
      .map(|reading| (reading.busiest_thread_ns.unwrap_or(0), reading.progress))
  };
  // Before 2026-11-08 a start had no scheduler reading, so a start held off its CPUs for a window was killed as stuck
  // ("made no progress for 1014 ms; killing it" in `held_rendezvous` on CI's Windows runner). Before A-336 a process
  // waiting on memory, every thread off the CPU, was stuck the same way, a daemon paging whole killed at its lapse.
  judge_start_by(
    watch,
    (now, memory, beat_ns),
    (window_ns, start_spin_window_ns(window_ns), watcher_late_ns),
    clock,
    || pid.and_then(slates_rt::process_run_reading),
  )
}

/// The CPU a start's busiest thread may run without a beat before the start is judged spinning: the window `window_ns`
/// ([`anchor_window_ns`]) held to this build (`slates_server::watchdog::spin_window_ns`, A-318), the build measured once
/// in this process (`slates_machine::build_cost::build_cost`). The anchor runs the executable it starts its daemon from
/// (the roles' prefix, A-278), so its build is its daemon's. Before A-318 the anchor judged a start at the bare budget
/// whatever the build, while A-318 had held the daemon's own watchdog to it: under ThreadSanitizer a start's unit of
/// work runs its slowdown's worth of CPU (17.9 to 35.3 times machine code on CI's runners), as a poll does, and a unit
/// of a millisecond or two natively, a pair ring, a registration, is a few tens of milliseconds there, the beat after
/// each unit (A-312) notwithstanding.
fn start_spin_window_ns(window_ns: u64) -> u64 {
  slates_server::watchdog::spin_window_ns(slates_machine::build_cost::build_cost(), window_ns)
}

/// The start's rule over its readings, taken in this order (g243's rule for an observation, which the watchdog's look
/// shares): the process's `clock`, what the `scheduler` says of its threads, and, when that answer is a block, the clock
/// again. A process that ran between the first read and the answer has moved, whatever the answer says, and the look
/// takes the second read; with no rise, the answer stands for the window, since a thread leaves the run queue for a
/// block only by running, which the clock would show. With the first read alone, a start blocked in a call for most of a
/// window whose call returned just then ran and blocked again before the scheduler looked, and was judged stuck, the
/// window still and never runnable; the anchor kills a start on that verdict. A runnable answer is starved, not stuck,
/// whatever the clock, so it needs no second read.
/// The daemon's beat word `beat_ns` is the start's step (A-312, a beat after each unit of its work, A-461): a start that
/// beats is alive at that look and its CPU without a beat is counted from it, so a start is judged by its silence since
/// its last beat, however long it has run in all. The silence is judged against `window_ns` and the busiest thread's CPU
/// without a beat against `spin_window_ns` ([`start_spin_window_ns`]). A gap in the anchor's own looks counts as a span
/// it did not watch only past the window and its own lateness p99, `watcher_late_ns` (A-474,
/// `slates_server::watchdog::slept_past_ns`), never for its own late looks.
fn judge_start_by(
  watch: &mut ShardWatch,
  (now, memory, beat_ns): (u64, Option<slates_machine::memory_stall::StallReading>, u64),
  (window_ns, spin_window_ns, watcher_late_ns): (u64, u64, u64),
  mut clock: impl FnMut() -> Option<(u64, u64)>,
  scheduler: impl FnOnce() -> Option<slates_rt::RunReading>,
) -> Verdict {
  let first = clock();
  let run = scheduler();
  let blocked = run.is_some_and(|run| run.state == slates_rt::RunState::Blocked);
  let cpu = match (first, blocked.then(&mut clock).flatten()) {
    (Some((_, before)), Some(again)) if again.1 > before => Some(again),
    (None, again @ Some(_)) => again,
    _ => first,
  };
  let look = Look {
    steps: beat_ns,
    parked: false,
    cpu,
    run,
    memory,
    now_ns: now,
    watcher_late_ns,
  };
  let (verdict, next) = watch.judge(look, window_ns, spin_window_ns);
  *watch = next;
  verdict
}

/// A running daemon whose beat lapsed while its watchdog had stopped looking (A-232): its process, judged by the start's
/// rule from when the looks stopped, and whether this episode's spare was said.
#[derive(Default)]
struct Quiet {
  watch: ShardWatch,
  said_spared: bool,
}

impl Quiet {
  /// Starts afresh while the daemon beats: the next silence is judged from its own start.
  fn reset_while(&mut self, alive: bool) {
    if alive {
      *self = Quiet::default();
    }
  }
}

/// The observation loop's next pause: `planned`, or a beat when `look_every_beat`. A killed daemon is waited on until
/// it is reaped, and a spared one is looked at every beat: its beat, already lapsed, says nothing of when to look next.
fn next_pause(planned: Duration, look_every_beat: bool) -> Duration {
  if look_every_beat {
    observation_pause()
  } else {
    planned
  }
}

/// What the anchor saw of a running daemon whose beat lapsed, at its clock `now`: the beat, the watchdog's last look
/// (`SUP_LOOKED`), when the anchor last resumed after a gap of its own (neither word is aged from before it), and the
/// lapse bound.
struct LapseSeen {
  now: u64,
  heartbeat_ns: u64,
  looked_ns: Option<u64>,
  resumed_ns: u64,
  bound: u64,
}

/// Acts on a lapsed beat (A-232, [`judge_lapse`]): kills a daemon whose watchdog withheld the beat while looking, or
/// whose process is stuck by the start's rule while its watchdog is silent, and spares one whose process is starved,
/// saying so once an episode ([`Quiet`]). Whether a kill was sent.
fn on_lapse(
  supervisor: &mut Supervisor,
  seen: &LapseSeen,
  lateness: &Lateness,
  quiet: &mut Quiet,
) -> Result<bool, Failure> {
  let now = seen.now;
  let beat_age = now.saturating_sub(seen.heartbeat_ns);
  let looked_age = now.saturating_sub(seen.looked_ns.unwrap_or(0).max(seen.resumed_ns));
  let pid = supervisor.child_pid();
  // A silent watchdog's process is judged by the start's rule over this anchor's window (A-461), its beat its step.
  let window = anchor_window_ns(lateness);
  let watchdog_looking = seen.looked_ns.is_some() && looked_age <= seen.bound;
  match judge_lapse(watchdog_looking, window, || {
    judge_start_of(
      pid,
      &mut quiet.watch,
      (now, seen.heartbeat_ns, slates_machine::memory_stall::read()),
      (window, lateness.p99_ns()),
    )
  }) {
    Lapse::Withheld => {
      slates_machine::log_line!(
        "slates anchor: the daemon's heartbeat lapsed ({} ms old, past {} ms: the {} ms window plus this anchor's \
         lateness p99 of {} ms over {} waits, at most {} ms); killing it",
        beat_age / NS_PER_MS,
        seen.bound / NS_PER_MS,
        LIVENESS_BUDGET_NS / NS_PER_MS,
        lateness.p99_ns() / NS_PER_MS,
        lateness.len(),
        slates_server::watchdog::WINDOW_CEILING_NS / NS_PER_MS
      );
      supervisor.kill().map_err(|e| failed("kill", e))?;
      Ok(true)
    }
    Lapse::Stuck(said) => {
      slates_machine::log_line!(
        "slates anchor: the daemon's watchdog has not looked for {} ms, its heartbeat {} ms old; its process {said}; \
         killing it",
        looked_age / NS_PER_MS,
        beat_age / NS_PER_MS
      );
      supervisor.kill().map_err(|e| failed("kill", e))?;
      Ok(true)
    }
    Lapse::Starved => {
      if !quiet.said_spared {
        quiet.said_spared = true;
        slates_machine::log_line!(
          "slates anchor: the daemon's heartbeat is {} ms old, but its watchdog has not looked for {} ms either, so its \
           process is judged in its own CPU time by the start's rule: waited for while starved, killed once frozen or \
           spinning for a window",
          beat_age / NS_PER_MS,
          looked_age / NS_PER_MS
        );
      }
      Ok(false)
    }
  }
}

/// What the anchor does about a running daemon whose beat has lapsed (A-232).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Lapse {
  /// The watchdog looked within the lapse bound and withheld the beat: its verdict (a stuck shard, or a control loop
  /// that ended). The daemon is killed.
  Withheld,
  /// The watchdog has not looked within the bound and the daemon's process is stuck by the start's rule, as the words
  /// say (no CPU progress and never runnable for a window, as a frozen process; or a window of CPU on one thread while
  /// the watchdog never looked). The daemon is killed.
  Stuck(String),
  /// The watchdog has not looked within the bound, and the daemon's process is starved or alive by that rule: its
  /// watchdog was kept off its CPU with the rest of it. A starved daemon is not a stuck one; it is waited for.
  Starved,
}

/// What a lapsed beat means (A-232), pure so the table is tested on every host: `watchdog_looking` whether the watchdog
/// marked a look within the lapse bound; `process` judges the daemon's process by the start's rule over the window
/// `window_ns` ([`anchor_window_ns`]), asked only when the watchdog did not look. Before A-232 every lapse killed: at
/// `cpu.weight` 1 beside default-weight spinners on CI the watchdog thread itself was held off past the window and the
/// anchor's lateness p99, and the anchor killed a starved daemon in 3 of 20 runs (`diag-a232-starved-1`, beats 3.3 to
/// 4.1 s old against bounds of 2.5 to 3.3 s).
fn judge_lapse(watchdog_looking: bool, window_ns: u64, process: impl FnOnce() -> Verdict) -> Lapse {
  if watchdog_looking {
    return Lapse::Withheld;
  }
  match start_stuck(process(), window_ns) {
    Some(said) => Lapse::Stuck(said),
    None => Lapse::Starved,
  }
}

/// What a start's verdict says when it is to be killed, naming the window it was judged by: a wedge's, `window_ns`
/// ([`anchor_window_ns`]), and a spin's, the window of CPU the verdict carries. `None` while it is alive.
fn start_stuck(verdict: Verdict, window_ns: u64) -> Option<String> {
  match verdict {
    // A starved start, runnable with its CPU standing still, is not stuck (A-228).
    Verdict::Alive | Verdict::Starved { .. } => None,
    // Its silence: no beat, no CPU progress and never runnable. The words before each number are the ones the CLI's
    // tests read (`crates/cli/tests/cli.rs`, `STUCK_WORDS` and `frozen_kill_of`).
    Verdict::Wedged { for_ns } => Some(format!(
      "made no progress for {} ms (its {} ms window)",
      for_ns / NS_PER_MS,
      window_ns / NS_PER_MS
    )),
    Verdict::Spinning {
      cpu_ns,
      window_ns: spin_window_ns,
    } => Some(format!(
      "ran {} ms of CPU on one thread without beating (its {} ms window of CPU)",
      cpu_ns / NS_PER_MS,
      spin_window_ns / NS_PER_MS
    )),
  }
}

/// Says once a daemon that it has recovered (A-311): it has served as long as its start took, so its exit from here on
/// is no failed start and is restarted at once. `said` is the daemon whose verdict was said last.
fn say_recovered(supervisor: &Supervisor, recovered: bool, said: &mut Option<u32>) {
  let pid = supervisor.child_pid();
  if !recovered || pid.is_none() || *said == pid {
    return;
  }
  *said = pid;
  slates_machine::log_line!(
    "slates anchor: daemon pid {} recovered: it has served as long as its start took ({} ms), so its exit is no failed \
     start",
    pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
    supervisor.start_took_ns().unwrap_or(0) / NS_PER_MS
  );
}

/// The observation loop.
fn observe(
  supervisor: &mut Supervisor,
  clock: &mut HostClock,
  started: u64,
) -> Result<(), Failure> {
  let mut starting_since = Some(started);
  // The start under way, judged in the daemon's own CPU time and its beats until its watchdog takes the beat over
  // (the module doc, A-461).
  let mut start_watch = ShardWatch::default();
  let mut watch = Watch::default();
  let mut daemon_watch = DaemonWatch::default();
  let mut lateness = Lateness::default();
  slates_machine::log_line!(
    "slates anchor: a daemon's beat is lapsed once older than the {} ms liveness window plus this anchor's own measured \
     lateness p99, over its last {LATENESS_SAMPLES} timed waits, at most {} ms (the daemon's observation budget); a \
     running daemon is looked at again within that bound less the lateness p99 and a {} ms beat, at least a beat apart \
     (A-467)",
    LIVENESS_BUDGET_NS / NS_PER_MS,
    slates_server::watchdog::WINDOW_CEILING_NS / NS_PER_MS,
    HEARTBEAT_NS / NS_PER_MS
  );
  // A kill already sent to the daemon that has not been reaped yet: it is not sent again, and the loop waits on the
  // daemon's exit (its exit watch ends the wait) rather than judging the same stale heartbeat each turn. Before
  // 2026-10-10 a lapsed daemon was killed and logged again every pause until reaped.
  slates_machine::log_line!(
    "slates anchor: a starting daemon is judged until its watchdog takes the beat over, each of its beats a step: killed \
     once it has neither beaten nor made CPU progress for its window, or one of its threads has run that window of CPU \
     held to this build without a beat, in its own CPU time ({}); its window is the {} ms liveness window, raised to how \
     late this anchor's own waits run where that is longer (A-461)",
    match supervisor
      .child_pid()
      .and_then(slates_machine::process_clock::process_cpu)
    {
      Some(cpu) if cpu.busiest_thread_ns.is_some() => "read from the OS, each thread's",
      Some(_) => "read from the OS; its threads not, so nothing is judged spinning",
      None => "none read here: the wall window stands alone",
    },
    LIVENESS_BUDGET_NS / NS_PER_MS
  );
  let mut kill_sent = false;
  let mut quiet = Quiet::default();
  // The daemon whose recovery the log has said (A-311), so each daemon's is said once.
  let mut said_recovered = None;
  // Its starter asked it to stop if no daemon ever serves (A-366, `StartSpec::until_served`).
  let until_served = crate::start::until_served_asked();
  // The lapse bound last published in the segment (A-574), so a turn writes only a change.
  let mut published_bound = None;
  loop {
    if signal::stop_requested() {
      stop_gracefully(supervisor, clock, &lateness)?;
      return Ok(());
    }
    let now = clock.monotonic_ns();
    let bound = lateness.lapse_bound_ns();
    publish_lapse_bound(supervisor, bound, &mut published_bound);
    // The window a start, and a silent watchdog's process, is judged by at this look (A-461).
    let window = anchor_window_ns(&lateness);
    match supervisor.step(now).map_err(|e| failed("step", e))? {
      slates_anchor::Step::Running => {
        let (heartbeat_ns, looked_ns, alive) = {
          let sup = supervisor
            .segment()
            .supervision()
            .map_err(|e| failed("supervision", e))?;
          let (dead, next) = lapsed(watch, sup.heartbeat_ns(), now, bound);
          watch = next;
          // Running and beaten at least once, as the segment judges it; the age is the watch's, which does not count
          // a span the anchor itself did not observe.
          (
            sup.heartbeat_ns(),
            sup.looked_ns(),
            sup.alive(now, u64::MAX).0 && !dead,
          )
        };
        quiet.reset_while(alive);
        // A starting daemon is looked at every beat, since its start is judged by its CPU and its beats until its
        // watchdog takes the beat over, and so is one not yet judged recovered (A-311), so that verdict comes within a
        // beat of being due; a recovered one only when its heartbeat could next be found lapsed (`lapse_wait`).
        let recovered = supervisor
          .has_recovered()
          .map_err(|e| failed("supervision", e))?;
        say_recovered(supervisor, recovered, &mut said_recovered);
        let pause = if starting_since.is_some() || !recovered {
          observation_pause()
        } else {
          Duration::from_nanos(lapse_wait(
            watch,
            heartbeat_ns,
            now,
            (bound, lateness.observation_gap_ns()),
          ))
        };
        match starting_since {
          // The start ends when its watchdog has looked (`SUP_LOOKED`, zeroed at each start): the daemon then judges its
          // own shards, and its beat is the watchdog's verdict (A-232). Until then each beat is a step of the start's
          // (A-312), so a start is judged by its silence since its last beat (A-461). Before, the start's judgment ended
          // at its first beat, and a start wedged after it was found only once its beat had lapsed and its process had
          // stood still a whole window more.
          Some(_) if looked_ns.is_some() => {
            starting_since = None;
          }
          Some(since) if !kill_sent => {
            if let Some(said) = start_stuck(
              judge_start(
                supervisor,
                &mut start_watch,
                (now, heartbeat_ns),
                (window, lateness.p99_ns()),
              ),
              window,
            ) {
              slates_machine::log_line!(
                "slates anchor: the starting daemon (pid {}, spawned {} ms ago) {said}; killing it",
                supervisor
                  .child_pid()
                  .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
                now.saturating_sub(since) / NS_PER_MS
              );
              supervisor.kill().map_err(|e| failed("kill", e))?;
              kill_sent = true;
              starting_since = None;
            }
          }
          Some(_) => {}
          None if !alive && !kill_sent => {
            kill_sent = on_lapse(
              supervisor,
              &LapseSeen {
                now,
                heartbeat_ns,
                looked_ns,
                resumed_ns: watch.resumed_ns,
                bound,
              },
              &lateness,
              &mut quiet,
            )?;
          }
          None => {}
        }
        let pause = next_pause(pause, kill_sent || (!alive && starting_since.is_none()));
        let asked = u64::try_from(pause.as_nanos()).unwrap_or(u64::MAX);
        let waited_from = clock.monotonic_ns();
        await_observation(supervisor, &mut daemon_watch, pause);
        lateness.record(asked, clock.monotonic_ns().saturating_sub(waited_from));
      }
      slates_anchor::Step::Restarted {
        exit_code,
        failed_in_a_row,
      } => {
        slates_machine::log_line!(
          "slates anchor: daemon exited ({exit_code:?}); restarted as pid {} ({failed_in_a_row} failed starts in a row)",
          supervisor
            .child_pid()
            .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string())
        );
        starting_since = Some(clock.monotonic_ns());
        start_watch = ShardWatch::default();
        kill_sent = false;
      }
      slates_anchor::Step::CrashLoop {
        exit_code,
        retry_in_ns,
      } => {
        // A starter that asked for it (A-366) is told the instance never served: no daemon of this anchor reached its
        // serving mark, so no client can hold a mount on it, and the loop would only keep its starter waiting.
        if until_served && !supervisor.ever_served() {
          slates_machine::log_line!(
            "slates anchor: the daemon is in a crash loop (last exit {exit_code:?}) and none of its starts has served; \
             its starter asked the anchor to stop then (A-366)"
          );
          supervisor.stop().map_err(|e| failed("stop", e))?;
          return Err(Failure::NeverServed);
        }
        // Never given up on (A-119): every hard mount waits for the daemon. The loop is recorded for the health
        // plane and the restart comes after the backoff.
        slates_machine::log_line!(
          "slates anchor: the daemon is in a crash loop (last exit {exit_code:?}); restarting it in {} ms",
          retry_in_ns / NS_PER_MS
        );
        kill_sent = false;
      }
      slates_anchor::Step::BackingOff { .. } => {
        await_observation(supervisor, &mut daemon_watch, observation_pause());
      }
      slates_anchor::Step::Stopped => return Ok(()),
    }
  }
}

/// Format: nanoseconds in a millisecond, for the anchor's log lines.
const NS_PER_MS: u64 = 1_000_000;

/// How late the anchor's own timed waits come back (§4.14, A-228): for each of its last [`LATENESS_SAMPLES`] waits that
/// ran its whole pause, how much longer than the pause it took (`slates_server::watchdog::LatenessTail`, the readings
/// the daemon's watchdog keeps of its own looks too, A-474). A machine busy enough to starve the daemon's watchdog
/// starves the anchor too, so the anchor holds the daemon's beat to the liveness window plus what its own clock shows of
/// the machine's lateness, never to a fixed second. A wait that ended early (an exit, a hold, a signal) says nothing of
/// lateness and is not kept.
#[derive(Debug, Default)]
struct Lateness {
  tail: slates_server::watchdog::LatenessTail,
}

impl Lateness {
  /// Keeps a wait that asked for `asked_ns` and took `took_ns`, if it ran its whole pause.
  fn record(&mut self, asked_ns: u64, took_ns: u64) {
    if took_ns < asked_ns {
      return;
    }
    self.tail.record(took_ns.saturating_sub(asked_ns));
  }

  /// The p99 of the kept samples (nearest rank), zero before any.
  fn p99_ns(&self) -> u64 {
    self.tail.p99_ns()
  }

  /// How many samples are kept.
  fn len(&self) -> usize {
    self.tail.len()
  }

  /// Derived: the age past which a daemon's beat is lapsed: the liveness window plus the anchor's measured lateness p99
  /// (A-228), never past the ceiling the daemon's watchdog holds its own windows to (A-467; A-455's
  /// `slates_server::watchdog::WINDOW_CEILING_NS`, the daemon's observation budget). A live daemon's beat is refreshed
  /// every beat, and goes stale only by as much as its watchdog's thread is held off its CPU, which the anchor's own
  /// lateness measures, since the host that delays one delays the other. Without the ceiling a runaway reading (a host
  /// paused twice among the last hundred waits) would hold every lapse past the pause, and the anchor would be blind to a
  /// wedged daemon for that long; a daemon whose beat outlives the ceiling while it is alive is still spared, since a
  /// lapse with its watchdog silent is judged by its process (A-232).
  fn lapse_bound_ns(&self) -> u64 {
    LIVENESS_BUDGET_NS
      .saturating_add(self.p99_ns())
      .min(slates_server::watchdog::WINDOW_CEILING_NS)
  }

  /// Derived: the longest the observation loop waits before it looks at a running daemon again (A-467): the lapse bound
  /// less this anchor's own lateness p99 and a beat, and never less than a beat, the daemon's own cadence. A look that
  /// comes later than the bound after the one before reads as the host having slept, and restarts the daemon's silence
  /// ([`lapsed`]), so the loop's own gap, its wait and its lateness, must stay inside the bound: at its p99 it does,
  /// with a beat to spare. Where the bound leaves no such room (a lateness past the ceiling) the loop looks every beat.
  /// Before, the loop waited half the bound, whatever its lateness: past a lateness of a window the gap passed the bound
  /// at every look, each look was taken for a sleep, and a wedged daemon was never found lapsed.
  fn observation_gap_ns(&self) -> u64 {
    self
      .lapse_bound_ns()
      .saturating_sub(self.p99_ns())
      .saturating_sub(HEARTBEAT_NS)
      .max(HEARTBEAT_NS)
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

/// How long the observation loop may wait before it must look at a running daemon again (§4.14): until the first
/// instant [`lapsed`] could find its heartbeat (last beaten at `beat_ns`) lapsed against `budget_ns`, and never more
/// than `gap_ns`, the longest gap the anchor's own lateness leaves inside the budget ([`Lateness::observation_gap_ns`],
/// A-467). A daemon beating every heartbeat is looked at about once a window on a quiet host; one that stops is found
/// lapsed at the budget's end, not up to a beat later. Exits, holds and signals end the wait sooner
/// ([`await_observation`]). Before 2026-10-10 the loop looked every heartbeat, eleven wakeups a second at rest; until
/// A-467 it waited half the budget, whatever its own lateness.
fn lapse_wait(watch: Watch, beat_ns: u64, now_ns: u64, (budget_ns, gap_ns): (u64, u64)) -> u64 {
  let lapses_at = beat_ns
    .max(watch.resumed_ns)
    .saturating_add(budget_ns)
    .saturating_add(1);
  lapses_at.saturating_sub(now_ns).min(gap_ns)
}

/// The policy this anchor restarts its daemon by (A-311): a crash loop's backoff doubles from the failed start's own life
/// to the NFSv4 lease the daemon `config` derives grants its clients (A-525, `RestartPolicy::derive`).
fn restart_policy(config: &DaemonConfig) -> slates_machine::Derived<RestartPolicy> {
  RestartPolicy::derive(config.nfs_v4.lease_ns)
}

#[cfg(test)]
mod lapse_tests {
  use super::*;

  /// A-525 (A-311's cap; R3, A-414's owed list: the failover placeholder's readers). Do: derive a daemon configuration
  /// as the anchor does, from this machine's profile, and take the anchor's restart policy for it. Expect the crash
  /// loop's cap to be the NFSv4 lease that daemon grants its clients (A-425: an idle Linux client's renewal floor and a
  /// liveness window): restarted at least once a lease, a crash-looping daemon is back before a client renewing at its
  /// floor finds its lease run out once the cause clears. Before, the cap was the operator's ten-second placeholder,
  /// past the six-second lease, so a client's state could lapse between two starts.
  #[test]
  fn a_crash_loops_backoff_is_capped_at_the_nfs_v4_lease_its_daemon_grants() {
    let profile = MachineProfile::measure_patiently(&slates_machine::ProfileOptions {
      budget_per_probe: Duration::from_nanos(HEARTBEAT_NS),
      codecs: false,
      core_matrix: false,
    })
    .unwrap();
    let config = DaemonConfig::derive(&profile, "restart-policy", Some(1));
    let policy = restart_policy(&config);
    assert_eq!(
      policy.get().backoff_cap_ns,
      config.nfs_v4.lease_ns,
      "a crash loop's doubling stops at the NFSv4 lease: {} ({:?})",
      policy.formula,
      policy.anchors
    );
  }

  /// g243's sibling (`docs/bugs/2026-11-18-an-observation-read-the-clock-before-the-scheduler.md`), A-228's start rule.
  /// Do: judge a start blocked in a call, its clock still and never runnable, look by look for most of a window; at the
  /// look that ends the window, script its reads in the order the rule takes them: the clock still, the scheduler
  /// finding it blocked, and the clock read again after that answer, risen (the call returned and the start ran between
  /// the first read and the answer). Expect: alive. With the first read alone the start was stuck, still for the window
  /// and never runnable, and the anchor kills a stuck start. A second read that shows no rise stands with the answer:
  /// stuck, as before.
  #[test]
  fn a_start_that_runs_between_its_clock_and_the_schedulers_answer_has_moved() {
    let blocked = || {
      Some(slates_rt::RunReading {
        state: slates_rt::RunState::Blocked,
        waited_ns: None,
        uninterruptible: None,
      })
    };
    let looks = LIVENESS_BUDGET_NS / HEARTBEAT_NS;
    let watched = |watch: &mut ShardWatch| {
      for look in 1..=looks {
        let verdict = judge_start_by(
          watch,
          (look * HEARTBEAT_NS, None, 0),
          (LIVENESS_BUDGET_NS, LIVENESS_BUDGET_NS, 0),
          || Some((10, 10)),
          blocked,
        );
        assert_eq!(
          verdict,
          Verdict::Alive,
          "look {look}: still for less than a window"
        );
      }
    };
    let ends = (looks + 1) * HEARTBEAT_NS;
    let mut moved = ShardWatch::default();
    watched(&mut moved);
    let mut reads = [Some((10, 10)), Some((12, 12))].into_iter();
    let verdict = judge_start_by(
      &mut moved,
      (ends, None, 0),
      (LIVENESS_BUDGET_NS, LIVENESS_BUDGET_NS, 0),
      || reads.next().flatten(),
      blocked,
    );
    assert_eq!(
      verdict,
      Verdict::Alive,
      "it ran between its clock and the answer"
    );
    let mut still = ShardWatch::default();
    watched(&mut still);
    let verdict = judge_start_by(
      &mut still,
      (ends, None, 0),
      (LIVENESS_BUDGET_NS, LIVENESS_BUDGET_NS, 0),
      || Some((10, 10)),
      blocked,
    );
    assert!(
      matches!(verdict, Verdict::Wedged { .. }),
      "still at both reads and blocked: stuck a window after it last moved, {verdict:?}"
    );
  }

  /// A-228's start rule with its condition put in place (the "inject the product's own condition" rule): what
  /// `a_start_held_off_its_cpus_is_starved_not_stuck` makes of a real process on a host it can starve, which a big idle
  /// one cannot (jupiter, 32 cores, 2026-11-18: "did not starve", the premise failing, not the rule). Do: judge a start
  /// look by look while its clock stands still for two windows and three beats and the scheduler answers its threads
  /// runnable, and again with the scheduler answering them blocked. Expect: runnable, never stuck, and starved once its
  /// clock has stood still a window; blocked, stuck a window after it last moved, which the runnable answer alone
  /// excuses.
  #[test]
  fn a_start_whose_clock_stands_still_while_runnable_is_starved_not_stuck() {
    let reading = |state| {
      move || {
        Some(slates_rt::RunReading {
          state,
          waited_ns: None,
          uninterruptible: Some(false),
        })
      }
    };
    let looks = (2 * LIVENESS_BUDGET_NS + 3 * HEARTBEAT_NS) / HEARTBEAT_NS;
    let judged = |state| {
      let mut watch = ShardWatch::default();
      (1..=looks)
        .map(|look| {
          judge_start_by(
            &mut watch,
            (look * HEARTBEAT_NS, None, 0),
            (LIVENESS_BUDGET_NS, LIVENESS_BUDGET_NS, 0),
            || Some((10, 10)),
            reading(state),
          )
        })
        .collect::<Vec<Verdict>>()
    };
    let runnable = judged(slates_rt::RunState::Runnable);
    assert!(
      runnable
        .iter()
        .all(|verdict| start_stuck(*verdict, LIVENESS_BUDGET_NS).is_none()),
      "a start held off its CPUs, its clock still and its threads runnable, is never stuck: {runnable:?}"
    );
    assert!(
      runnable
        .iter()
        .any(|verdict| matches!(verdict, Verdict::Starved { .. })),
      "non-vacuity: its clock stood still past a window, so it was starved: {runnable:?}"
    );
    let blocked = judged(slates_rt::RunState::Blocked);
    assert!(
      blocked
        .iter()
        .any(|verdict| start_stuck(*verdict, LIVENESS_BUDGET_NS).is_some()),
      "the same stillness with its threads blocked is stuck: {blocked:?}"
    );
  }

  /// A-318's sibling, §4.14. Do: judge a start whose busiest thread is on a whole CPU without a beat, look by look for
  /// four budgets, with a spin window three budgets long (a build measured three times as slow as machine code, as an
  /// unoptimized one is, 3.4 times on a Mac Studio; ThreadSanitizer's 17.9 to 35.3). Expect: alive while its thread's CPU
  /// is under the spin window, though past one budget (a unit of work this build runs slower, no spin); spinning once it
  /// reaches the spin window. Before, the anchor judged every start at the bare budget, whatever the build A-318 had held
  /// the daemon's own watchdog to.
  #[test]
  fn a_starts_thread_is_judged_spinning_only_past_the_window_held_to_its_build() {
    let spin_window = 3 * LIVENESS_BUDGET_NS;
    let runnable = || {
      Some(slates_rt::RunReading {
        state: slates_rt::RunState::Runnable,
        waited_ns: None,
        uninterruptible: Some(false),
      })
    };
    let mut watch = ShardWatch::default();
    let mut first_spinning = None;
    for look in 1..=(4 * LIVENESS_BUDGET_NS / HEARTBEAT_NS) {
      let cpu = look * HEARTBEAT_NS;
      let verdict = judge_start_by(
        &mut watch,
        (cpu, None, 0),
        (LIVENESS_BUDGET_NS, spin_window, 0),
        || Some((cpu, cpu)),
        runnable,
      );
      match verdict {
        Verdict::Spinning { cpu_ns, window_ns } => {
          first_spinning.get_or_insert((look, cpu_ns, window_ns));
        }
        other => assert_eq!(
          other,
          Verdict::Alive,
          "look {look}: on the CPU, so alive until the spin window"
        ),
      }
    }
    let (look, cpu_ns, window_ns) =
      first_spinning.expect("the start was judged spinning past the spin window");
    assert!(
      cpu_ns >= spin_window && cpu_ns < spin_window + HEARTBEAT_NS,
      "look {look}: {cpu_ns} ns of CPU"
    );
    assert_eq!(
      window_ns, spin_window,
      "the verdict names the window it judged"
    );
  }

  /// What the scheduler says of a thread off the CPU and not ready, asleep (Linux `S`).
  fn asleep() -> Option<slates_rt::RunReading> {
    Some(slates_rt::RunReading {
      state: slates_rt::RunState::Blocked,
      waited_ns: None,
      uninterruptible: Some(false),
    })
  }

  /// A-461, A-312, §4.14. Do: judge a start look by look, a look a beat, on a quiet anchor (its window the liveness
  /// window): its process asleep and its clock still between its beats (a start whose units wait on the host, so its CPU
  /// shows nothing between them), beating every three fifths of a window for five windows, and then never again.
  /// Expect: never stuck while it beats, though it has run five times the old bound in all, since each beat is a step of
  /// the start's; and wedged by its silence, a window after the look that saw its last beat, never later. Before, the
  /// start's beats were no step: the rule saw a start whose clock stood still and never ran, and judged it wedged a window
  /// after its first look.
  #[test]
  fn a_start_that_beats_is_waited_for_and_a_wedged_one_is_judged_by_its_silence() {
    let window = anchor_window_ns(&Lateness::default());
    let beat_every = 3 * window / 5;
    let beating_until = 5 * window;
    let mut watch = ShardWatch::default();
    let mut beat_ns = 0;
    let mut wedged = None;
    for look in 1..=(beating_until + 3 * window) / HEARTBEAT_NS {
      let now = look * HEARTBEAT_NS;
      if now <= beating_until {
        // Its latest beat by now: a step the look sees once it has come.
        beat_ns = now / beat_every * beat_every;
      }
      let verdict = judge_start_by(
        &mut watch,
        (now, None, beat_ns),
        (window, window, 0),
        || Some((10, 10)),
        asleep,
      );
      if now <= beating_until {
        assert_eq!(
          start_stuck(verdict, window),
          None,
          "look {look}: it beats, so it is waited for: {verdict:?}"
        );
      }
      if let Verdict::Wedged { for_ns } = verdict {
        wedged = Some((now, for_ns));
        break;
      }
    }
    assert_eq!(
      wedged,
      Some((beat_ns + window, window)),
      "wedged by its silence: a window after the look that saw its last beat, at {beat_ns} ns"
    );
  }

  /// A-461, A-455, §4.14. Do: derive the anchor's window from its own waits' lateness, each wait come back on time, two
  /// and a half liveness windows late, and an hour late (a host paused); and judge by each a start whose process sleeps,
  /// its clock still and no beat, a look a beat. Expect: on time, the liveness window, the start wedged a window after
  /// its first look, as before; two and a half windows late, a window of two and a half, the start waited for past the
  /// old bound and wedged at its window; an hour late, the ceiling the daemon's watchdog holds its own windows to, so a
  /// stale reading never blinds the anchor. Before, every start was judged by the bare liveness window whatever the host.
  #[test]
  fn a_start_is_judged_by_the_window_its_anchors_own_waits_measure() {
    let lateness_of = |late_ns: u64| {
      let mut lateness = Lateness::default();
      for _ in 0..LATENESS_SAMPLES {
        lateness.record(HEARTBEAT_NS, HEARTBEAT_NS.saturating_add(late_ns));
      }
      lateness
    };
    let wedged_after = |window: u64| {
      let mut watch = ShardWatch::default();
      (1..=(slates_server::watchdog::WINDOW_CEILING_NS / HEARTBEAT_NS + 2)).find_map(|look| {
        match judge_start_by(
          &mut watch,
          (look * HEARTBEAT_NS, None, 0),
          (window, window, 0),
          || Some((10, 10)),
          asleep,
        ) {
          Verdict::Wedged { for_ns } => Some(for_ns),
          _ => None,
        }
      })
    };
    let budget = LIVENESS_BUDGET_NS;
    for (late_ns, expected) in [
      (0, budget),
      (5 * budget / 2, 5 * budget / 2),
      (3_600 * budget, slates_server::watchdog::WINDOW_CEILING_NS),
    ] {
      let window = anchor_window_ns(&lateness_of(late_ns));
      assert_eq!(
        window, expected,
        "waits come back {late_ns} ns late: the window"
      );
      assert_eq!(
        wedged_after(window),
        Some(expected),
        "waits come back {late_ns} ns late: a silent start is wedged at its window"
      );
    }
  }

  /// A-232, §4.14. Do: judge a lapsed beat with the watchdog still looking, and with it silent while the daemon's process
  /// is alive, starved, wedged (no CPU, never runnable: frozen) or spinning (a window of CPU on one thread). Expect: a
  /// looking watchdog's lapse is its verdict and kills without asking about the process; a silent one kills only a
  /// wedged or spinning process and spares a starved or alive one, since a starved daemon is not a stuck one.
  #[test]
  fn a_lapse_kills_a_withheld_beat_or_a_stuck_process_and_spares_a_starved_one() {
    let window = LIVENESS_BUDGET_NS;
    assert_eq!(
      judge_lapse(true, window, || panic!(
        "a looking watchdog's lapse asks nothing of the process"
      )),
      Lapse::Withheld
    );
    for spared in [
      Verdict::Alive,
      Verdict::Starved {
        for_ns: window,
        of: Want::Cpu,
      },
      Verdict::Starved {
        for_ns: window,
        of: Want::Memory,
      },
    ] {
      assert_eq!(
        judge_lapse(false, window, || spared),
        Lapse::Starved,
        "{spared:?}"
      );
    }
    for stuck in [
      Verdict::Wedged { for_ns: window },
      Verdict::Spinning {
        cpu_ns: window,
        window_ns: window,
      },
    ] {
      assert!(
        matches!(judge_lapse(false, window, || stuck), Lapse::Stuck(_)),
        "{stuck:?}"
      );
    }
  }

  /// Format: the environment variable under which [`a_held_process_role`] holds its process; unset, it returns at once.
  const HELD_ROLE_ENV: &str = "SLATES_ANCHOR_HELD_PROCESS_ROLE";
  /// Format: [`a_held_process_role`]'s name for libtest.
  const HELD_ROLE: &str = "role::anchor::lapse_tests::a_held_process_role";
  /// The role: holds this process where a process waiting on memory is, until its standard input ends (its parent
  /// closes it, or ends): on Linux its test thread held in the kernel past signals ([`hold_in_the_kernel`]); elsewhere
  /// every thread blocked, the test's in a read of standard input.
  #[test]
  #[ignore = "a role: run by a_process_waiting_on_memory_is_spared_while_the_host_stalls_and_stuck_a_window_after"]
  fn a_held_process_role() {
    if std::env::var_os(HELD_ROLE_ENV).is_none() {
      return;
    }
    hold_in_the_kernel();
  }

  /// Shape: the stack the held thread's child runs its two calls on (a read and an exit).
  #[cfg(target_os = "linux")]
  const CHILD_STACK: usize = 64 * 1024;

  /// The child of [`hold_in_the_kernel`]: reads one byte of standard input (or its end), then exits. It runs on its own
  /// stack in the parent's memory, so it makes no allocation and takes no lock: two system calls.
  #[cfg(target_os = "linux")]
  extern "C" fn read_standard_input_then_exit(_: *mut libc::c_void) -> libc::c_int {
    let mut byte = 0_u8;
    // SAFETY: `byte` is this frame's own; descriptor 0 is the process's standard input.
    unsafe {
      libc::syscall(
        libc::SYS_read,
        libc::c_long::from(libc::STDIN_FILENO),
        (&raw mut byte).cast::<libc::c_void>(),
        1_usize,
      );
      libc::_exit(0)
    }
  }

  /// Holds the calling thread where the kernel holds a thread waiting on a page-in, in a wait past signals (`D`): it
  /// clones a child that shares its memory and makes it wait (`CLONE_VM | CLONE_VFORK`, as `vfork` does), so the kernel
  /// keeps it in `wait_for_vfork_done` until the child has read standard input and exited; then reaps the child. As
  /// `slates_rt`'s `thread_clock` tests hold one (`hold_in_the_kernel` there).
  #[cfg(target_os = "linux")]
  fn hold_in_the_kernel() {
    let mut stack = vec![0_u8; CHILD_STACK];
    let top = stack.as_mut_ptr().wrapping_add(CHILD_STACK);
    // SAFETY: the child runs `read_standard_input_then_exit` on `stack`, which outlives it (the call returns only once
    // the child has exited, `CLONE_VFORK`); it shares this process's memory and touches nothing but its own stack.
    let child = unsafe {
      libc::clone(
        read_standard_input_then_exit,
        top.cast(),
        libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD,
        std::ptr::null_mut(),
      )
    };
    assert!(child > 0, "the child was cloned");
    let mut status = 0;
    // SAFETY: reaps this thread's own child; `status` is this frame's own.
    unsafe { libc::waitpid(child, &raw mut status, 0) };
    drop(stack);
  }

  /// Holds the calling thread blocked in a read of standard input, as every blocked wait reads where the platform does
  /// not tell a memory wait from another; the read ends at standard input's end.
  #[cfg(not(target_os = "linux"))]
  fn hold_in_the_kernel() {
    use std::io::Read;
    let mut byte = [0_u8];
    let _ = std::io::stdin().read(&mut byte);
  }

  /// A started process killed and reaped when dropped, however the test ends.
  struct Held(std::process::Child);

  impl Drop for Held {
    fn drop(&mut self) {
      let _ = self.0.kill();
      let _ = self.0.wait();
    }
  }

  /// What [`judge_held`] saw: when (since its first look's base) the process was found stuck, if it was; the verdicts
  /// in order; and the largest gap between two of its looks, the lateness a busy machine adds to a verdict's time.
  struct Judged {
    stuck_at: Option<Duration>,
    verdicts: Vec<Verdict>,
    largest_gap: Duration,
  }

  /// Judges the held process `pid` as the anchor judges a daemon whose watchdog stopped looking, a look a beat, on the
  /// clock `base` (the watch's), from now until it is found stuck or `span` passes, with the host's memory reading
  /// `memory`.
  fn judge_held(
    pid: u32,
    (watch, base): (&mut ShardWatch, std::time::Instant),
    span: Duration,
    memory: Option<slates_machine::memory_stall::StallReading>,
  ) -> Judged {
    let from = std::time::Instant::now();
    let mut judged = Judged {
      stuck_at: None,
      verdicts: Vec::new(),
      largest_gap: Duration::ZERO,
    };
    let mut last_look = from;
    while from.elapsed() < span {
      let (_hold, look) = std::sync::mpsc::channel::<()>();
      let _ = look.recv_timeout(Duration::from_nanos(HEARTBEAT_NS));
      judged.largest_gap = judged.largest_gap.max(last_look.elapsed());
      last_look = std::time::Instant::now();
      let now = u64::try_from(base.elapsed().as_nanos()).unwrap_or(u64::MAX);
      let verdict = judge_start_of(Some(pid), watch, (now, 0, memory), (LIVENESS_BUDGET_NS, 0));
      judged.verdicts.push(verdict);
      if start_stuck(verdict, LIVENESS_BUDGET_NS).is_some() {
        judged.stuck_at = Some(from.elapsed());
        break;
      }
    }
    judged
  }

  /// A-336, §4.14, the anchor's lapse and start judgment. Do: start a process held where a process waiting on memory is
  /// (this binary re-invoked as [`a_held_process_role`]: on Linux a thread held in the kernel past signals, elsewhere
  /// every thread blocked); once the scheduler reads it so, judge it as the anchor judges a daemon whose watchdog
  /// stopped looking, a look a beat, for three windows while the host reports a memory stall, then for up to three more
  /// with the stall cleared. Expect: never stuck while the host stalls, and starved for memory once a window has
  /// passed; stuck within a window and three looks of the stall clearing. Before, the process was stuck a window after
  /// it stopped, and the anchor killed a daemon paging whole at its lapse.
  #[test]
  #[cfg(not(miri))]
  fn a_process_waiting_on_memory_is_spared_while_the_host_stalls_and_stuck_a_window_after() {
    let window = Duration::from_nanos(LIVENESS_BUDGET_NS);
    let look = Duration::from_nanos(HEARTBEAT_NS);
    let held = Held(
      std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", HELD_ROLE])
        .env(HELD_ROLE_ENV, "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap(),
    );
    let pid = held.0.id();
    let platform_tells = cfg!(target_os = "linux");
    let settled = std::time::Instant::now();
    loop {
      let reading = slates_rt::process_run_reading(pid);
      let held_as_wanted = reading.is_some_and(|reading| {
        reading.state == slates_rt::RunState::Blocked
          && (!platform_tells || reading.uninterruptible == Some(true))
      });
      if held_as_wanted {
        break;
      }
      assert!(
        settled.elapsed() < window * 10,
        "the role never settled where a process waiting on memory is: {reading:?}"
      );
      let (_hold, pause) = std::sync::mpsc::channel::<()>();
      let _ = pause.recv_timeout(look);
    }
    let mut watch = ShardWatch::default();
    let base = std::time::Instant::now();
    let pressed = Some(slates_machine::memory_stall::StallReading::Level { pressed: true });
    let during = judge_held(pid, (&mut watch, base), window * 3, pressed);
    let clear = Some(slates_machine::memory_stall::StallReading::Level { pressed: false });
    let after = judge_held(pid, (&mut watch, base), window * 3, clear);
    drop(held);
    let for_memory = |verdict: &&Verdict| {
      matches!(
        verdict,
        Verdict::Starved {
          of: Want::Memory,
          ..
        }
      )
    };
    println!(
      "a process waiting on memory: stuck while the host stalled {:?}; starved for memory at {} of {} looks; stuck {:?} \
       after the stall cleared, the looks' largest gap {:?}",
      during.stuck_at,
      during.verdicts.iter().filter(for_memory).count(),
      during.verdicts.len(),
      after.stuck_at,
      after.largest_gap
    );
    assert_eq!(
      during.stuck_at, None,
      "stuck while the host stalled: {:?}",
      during.verdicts
    );
    assert!(
      during.verdicts.iter().any(|verdict| for_memory(&verdict)),
      "starved for memory once a window had passed: {:?}",
      during.verdicts
    );
    let stuck_after = after.stuck_at.expect("stuck once the stall cleared");
    // The last look that saw the stall came at most one look's gap before the clear, and the verdict comes at the first
    // look a window past it.
    assert!(
      stuck_after <= window + after.largest_gap * 2,
      "stuck {stuck_after:?} after the stall cleared, past a window and two of the looks' largest gap ({:?})",
      after.largest_gap
    );
  }

  /// A-228, A-467, §4.14. Do: feed the anchor's lateness window a quiet history (waits on time), then a late one (each
  /// wait 300 ms over its pause), with early-ended waits between, and more waits than it keeps; then a runaway one (each
  /// wait an hour over, a host paused). Expect: a quiet anchor holds a beat to the window alone; a late one to the window
  /// plus its lateness p99; early waits change nothing; the window keeps only its last hundred samples; and a runaway
  /// reading holds a beat to the ceiling the daemon's watchdog holds its windows to, never to the hour, which would have
  /// left the anchor blind to a wedged daemon for that long.
  #[test]
  fn the_lapse_bound_is_the_window_plus_the_anchors_own_lateness() {
    let mut lateness = Lateness::default();
    assert_eq!(lateness.lapse_bound_ns(), LIVENESS_BUDGET_NS);
    for _ in 0..50 {
      lateness.record(TURN, TURN);
      lateness.record(TURN, TURN / 2);
    }
    assert_eq!(lateness.len(), 50, "an early-ended wait is not kept");
    assert_eq!(lateness.lapse_bound_ns(), LIVENESS_BUDGET_NS);
    for _ in 0..LATENESS_SAMPLES {
      lateness.record(TURN, TURN + 300_000_000);
    }
    assert_eq!(lateness.len(), LATENESS_SAMPLES);
    assert_eq!(lateness.lapse_bound_ns(), LIVENESS_BUDGET_NS + 300_000_000);
    for _ in 0..LATENESS_SAMPLES {
      lateness.record(TURN, TURN + 3_600 * LIVENESS_BUDGET_NS);
    }
    assert_eq!(
      lateness.lapse_bound_ns(),
      slates_server::watchdog::WINDOW_CEILING_NS,
      "a runaway reading never holds a lapse past the ceiling"
    );
  }

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

  /// §4.14 (2026-10-10), A-467: the loop waits for the lapse, not for the next beat. Do: follow `lapse_wait` from turn to
  /// turn over a daemon that beats every tenth of a budget, then stops, with a quiet anchor's gap. Expect every wait at
  /// most that gap while it beats, and the lapse found exactly one budget and a nanosecond after its last beat.
  #[test]
  fn the_loop_waits_at_most_its_gap_and_finds_a_lapse_at_the_budgets_end() {
    let gap = Lateness::default().observation_gap_ns();
    let mut watch = Watch::default();
    let mut now = BUDGET;
    let mut last_beat = now;
    for _ in 0..30 {
      if now.saturating_sub(last_beat) >= TURN {
        last_beat = now - (now - last_beat) % TURN;
      }
      let (dead, next) = lapsed(watch, last_beat, now, BUDGET);
      assert!(!dead, "a beating daemon never lapses");
      watch = next;
      let wait = lapse_wait(watch, last_beat, now, (BUDGET, gap));
      assert!(wait <= gap, "{wait}");
      now += wait;
    }
    let stopped_at = last_beat;
    let mut found = None;
    for _ in 0..10 {
      let (dead, next) = lapsed(watch, stopped_at, now, BUDGET);
      watch = next;
      if dead {
        found = Some(now);
        break;
      }
      now += lapse_wait(watch, stopped_at, now, (BUDGET, gap));
    }
    assert_eq!(
      found,
      Some(stopped_at + BUDGET + 1),
      "found at the budget's end"
    );
  }

  /// A-467, A-228, §4.14. Do: follow the observation loop over a daemon that beats every turn for ten windows and then
  /// never again, the anchor's every look coming back twice a window late, as its own lateness p99 says (a host that
  /// starves the anchor so); the bound and the loop's wait each the anchor's own, from that lateness. Expect: never
  /// lapsed while it beats, and found lapsed once its silence passes the bound, within the anchor's lateness of it.
  /// Before, the loop waited half the bound whatever its lateness: its own gap then passed the bound at every look, each
  /// look was taken for the host having slept, the daemon's silence restarted, and it was never found lapsed.
  #[test]
  fn a_wedged_daemon_is_found_lapsed_though_the_anchors_own_looks_run_twice_a_window_late() {
    let late_ns = 2 * LIVENESS_BUDGET_NS;
    let mut lateness = Lateness::default();
    for _ in 0..LATENESS_SAMPLES {
      lateness.record(TURN, TURN + late_ns);
    }
    let (bound, gap) = (lateness.lapse_bound_ns(), lateness.observation_gap_ns());
    let stops_at = 11 * BUDGET;
    // The beat word at the anchor's clock `now`: the daemon's last turn, until it stops.
    let beat_at = |now: u64| now.min(stops_at) / TURN * TURN;
    let mut watch = Watch::default();
    let mut now = BUDGET;
    let mut found = None;
    for _ in 0..1_000 {
      let (dead, next) = lapsed(watch, beat_at(now), now, bound);
      watch = next;
      if dead {
        found = Some(now);
        break;
      }
      now += lapse_wait(watch, beat_at(now), now, (bound, gap)) + late_ns;
    }
    let found = found.expect("the silent daemon was found lapsed");
    assert!(
      found > stops_at + bound,
      "lapsed at {found} ns, before its silence passed the {bound} ns bound from {stops_at} ns"
    );
    assert!(
      found <= stops_at + bound + 1 + late_ns,
      "lapsed at {found} ns, later than the anchor's own lateness after its bound ({bound} ns from {stops_at} ns)"
    );
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

/// The start judgment against a real starved process. Linux and Windows only: macOS will not hold a process off its CPU
/// on demand (`crates/server/tests/liveness.rs` measured 1.1 s at best).
#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests {
  // Test harness code: an unwrap here is a failed test, which is what it should be.
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
  use super::*;

  /// Waits `span` on a channel nothing sends on (the lint wall bans `std::thread::sleep`).
  fn wait(span: Duration) {
    let (_hold, rx) = std::sync::mpsc::channel::<()>();
    let _ = rx.recv_timeout(span);
  }

  /// Shape: a beat, how often the anchor judges a start (its look cadence).
  const LOOK: Duration = Duration::from_nanos(HEARTBEAT_NS);
  /// Shape: the liveness window a start is judged over.
  const WINDOW: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);

  /// A started process killed and reaped when dropped, however the test ends.
  struct Child(std::process::Child);

  impl Drop for Child {
    fn drop(&mut self) {
      let _ = self.0.kill();
      let _ = self.0.wait();
    }
  }

  /// Format: the kernel's scheduling weight of nice 0 to 19 (`sched_prio_to_weight`, kernel/sched/core.c).
  #[cfg(target_os = "linux")]
  const NICE_WEIGHT: [u64; 20] = [
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
  ];
  /// Shape: spinners at nice 0 that hold a SCHED_IDLE thread off its CPU for seconds (63, `crates/server/tests/liveness.rs`).
  #[cfg(target_os = "linux")]
  const SPINNERS_AT_NICE_ZERO: u64 = 63;

  /// The first CPU this process may run on: where the starved start and its spinners are put.
  #[cfg(target_os = "linux")]
  fn a_cpu() -> usize {
    let allowed = rustix::thread::sched_getaffinity(None).unwrap();
    (0..rustix::thread::CpuSet::MAX_CPU)
      .find(|cpu| allowed.is_set(*cpu))
      .unwrap()
  }

  /// A start held off `cpu`: a busy shell at SCHED_IDLE, pinned there (`chrt`, `taskset`, util-linux).
  #[cfg(target_os = "linux")]
  fn starved_start(cpu: usize) -> Child {
    Child(
      std::process::Command::new("chrt")
        .args(["--idle", "0", "taskset", "-c", &cpu.to_string()])
        .args(["sh", "-c", "while :; do :; done"])
        .spawn()
        .unwrap(),
    )
  }

  /// Threads that outrank the start on `cpu` until `running` falls: at nice 0, or the nearest this process may take,
  /// and enough of them by the kernel's weight of that nice to hold a SCHED_IDLE process off for seconds.
  #[cfg(target_os = "linux")]
  fn outranking(
    cpu: usize,
    running: &'static std::sync::atomic::AtomicBool,
  ) -> Vec<std::thread::JoinHandle<()>> {
    let inherited = rustix::process::getpriority_process(None).unwrap();
    let weight = |nice: i32| {
      usize::try_from(nice)
        .ok()
        .and_then(|nice| NICE_WEIGHT.get(nice))
        .copied()
        .unwrap_or(NICE_WEIGHT[0])
    };
    let count = (SPINNERS_AT_NICE_ZERO * NICE_WEIGHT[0]).div_ceil(weight(inherited));
    (0..count)
      .map(|_| {
        std::thread::Builder::new()
          .stack_size(64 * 1024)
          .spawn(move || {
            let mut set = rustix::thread::CpuSet::new();
            set.set(cpu);
            rustix::thread::sched_setaffinity(None, &set).unwrap();
            let _ = rustix::process::setpriority_process(Some(rustix::thread::gettid()), 0);
            while running.load(std::sync::atomic::Ordering::Relaxed) {
              std::hint::spin_loop();
            }
          })
          .unwrap()
      })
      .collect()
  }

  /// A start held off CPU 0: a busy PowerShell that puts itself there at the Idle priority class.
  #[cfg(windows)]
  fn starved_start(_cpu: usize) -> Child {
    Child(
      std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command"])
        .arg(
          "$p=[System.Diagnostics.Process]::GetCurrentProcess(); $p.ProcessorAffinity=[IntPtr]1; \
           $p.PriorityClass='Idle'; while($true){}",
        )
        .spawn()
        .unwrap(),
    )
  }

  #[cfg(windows)]
  fn a_cpu() -> usize {
    0
  }

  /// Shape: busy processes at the High class that hold an Idle-class one off CPU 0 (four TIME_CRITICAL threads held an
  /// IDLE thread off for all of 8.9 s, `crates/server/tests/liveness.rs`).
  #[cfg(windows)]
  const OUTRANKING_PROCESSES: usize = 4;

  /// Processes that outrank the start on CPU 0, ended when the returned guards drop.
  #[cfg(windows)]
  fn outranking_processes() -> Vec<Child> {
    (0..OUTRANKING_PROCESSES)
      .map(|_| {
        Child(
          std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(
              "$p=[System.Diagnostics.Process]::GetCurrentProcess(); $p.ProcessorAffinity=[IntPtr]1; \
               $p.PriorityClass='High'; while($true){}",
            )
            .spawn()
            .unwrap(),
        )
      })
      .collect()
  }

  /// A-228's sibling in the anchor's start judgment (§2.6, §4.14). Do: start a busy process and hold it off its CPU with
  /// higher-priority spinners there, as a start is held off on a loaded machine; judge it as the anchor judges a start,
  /// a look a beat, until its CPU clock has stood still for two windows and three beats. Expect: never stuck. Before, a
  /// start had no scheduler reading, so one whose CPU stood still a window was "made no progress"; on CI's Windows
  /// runner `held_rendezvous` was killed so ("made no progress for 1014 ms; killing it"). Linux and Windows must see the
  /// flat window or the test fails saying it did not starve.
  #[test]
  fn a_start_held_off_its_cpus_is_starved_not_stuck() {
    {
      let cpu = a_cpu();
      let start = starved_start(cpu);
      let pid = start.0.id();
      let running: &'static std::sync::atomic::AtomicBool =
        Box::leak(Box::new(std::sync::atomic::AtomicBool::new(true)));
      #[cfg(target_os = "linux")]
      let spinners = outranking(cpu, running);
      #[cfg(windows)]
      let spinners = outranking_processes();
      // The busy start is judged from its first look once its clock reads.
      let began = std::time::Instant::now();
      while slates_machine::process_clock::process_cpu(pid).is_none() {
        assert!(began.elapsed() < WINDOW * 10, "the start's clock reads");
        wait(LOOK);
      }
      let flat_enough = WINDOW * 2 + LOOK * 3;
      let watched = std::time::Instant::now();
      let mut watch = ShardWatch::default();
      let mut stuck = Vec::new();
      let mut last = slates_machine::process_clock::process_cpu(pid).map(|cpu| cpu.progress);
      let mut moved = std::time::Instant::now();
      let mut longest_flat = Duration::ZERO;
      let mut starved_looks = 0u32;
      while watched.elapsed() < WINDOW * 10
        && (watched.elapsed() < WINDOW * 3 || longest_flat < flat_enough)
      {
        wait(LOOK);
        let now = u64::try_from(watched.elapsed().as_nanos()).unwrap();
        let verdict = judge_start_of(
          Some(pid),
          &mut watch,
          (now, 0, None),
          (LIVENESS_BUDGET_NS, 0),
        );
        if matches!(verdict, Verdict::Starved { .. }) {
          starved_looks += 1;
        }
        if let Some(said) = start_stuck(verdict, LIVENESS_BUDGET_NS) {
          stuck.push(said);
        }
        let progress = slates_machine::process_clock::process_cpu(pid).map(|cpu| cpu.progress);
        if progress != last {
          last = progress;
          moved = std::time::Instant::now();
        }
        longest_flat = longest_flat.max(moved.elapsed());
      }
      running.store(false, std::sync::atomic::Ordering::Relaxed);
      drop(spinners);
      drop(start);
      println!(
        "a start held off its CPU: its clock stood still {longest_flat:?} at most in {:?}; starved at {starved_looks} \
         looks; stuck {stuck:?}",
        watched.elapsed()
      );
      assert!(
        stuck.is_empty(),
        "a start held off its CPU was judged stuck: {stuck:?} (its clock stood still {longest_flat:?})"
      );
      // The host decides whether the start starves: a big idle one runs it elsewhere however the spinners outrank it
      // (jupiter, 32 cores, 2026-11-18). The rule is held with its condition put in place on every host
      // (`a_start_whose_clock_stands_still_while_runnable_is_starved_not_stuck`); this test reads the real scheduler,
      // and says so where it could not starve the start rather than fail on the host.
      if longest_flat < flat_enough {
        println!(
          "skipping the starved half: this host did not hold the start off its CPU (its clock stood still \
           {longest_flat:?} at most, under {flat_enough:?}); judged never stuck"
        );
      }
    }
  }
}
