//! `slates anchor` (§2.5 "The anchor process", §2.6 step 1, §4.14 `daemon.alive`): measure
//! the profile, create the segment, publish the profile, supervise the daemon.
//!
//! The loop is the supervisor's `step` at the heartbeat cadence with two observations the
//! supervisor itself does not make: a starting daemon that stops making progress before its
//! first beat is killed (a start that is not a start), and a daemon whose heartbeat lapses past
//! the liveness budget is killed (wedged, so a failure like a crash; the policy then decides).
//! A start is judged in the daemon's own CPU time, read from the OS
//! (`slates_machine::process_clock`), by the rule its watchdog judges each shard by once it runs
//! (`slates_server::watchdog::ShardWatch`): no CPU progress for a liveness window is a start
//! that is stuck, and a window of CPU with no first beat is one that spins; a start that keeps
//! progressing, however slowly, is starved and waited for. Until 2026-11-07 a start that had not
//! beaten within the fixed recovery budget, a wall second, was killed, and every restart started
//! over: on this Mac at 144 spinners on 18 CPUs the anchor killed 153 starts of 153, each
//! 1.07-1.22 s after its spawn, after it had used 16-18 ms of CPU.
//! The restart policy is re-derived from the measured daemon start: the longest start seen
//! is the p99 until enough starts exist to take a real one.

use std::time::Duration;

use slates_anchor::{AnchorSegment, RegionKind, RestartPolicy, Stopping, Supervisor};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_machine::MachineProfile;
use slates_server::DaemonConfig;
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS};
use slates_server::watchdog::{Look, ShardWatch, Verdict};
use slates_vfs::clock::{Clock, HostClock};

use crate::args::ProcessOptions;
use crate::daemon::{measure, segment_name};
use crate::{Failure, signal};

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

fn failed(what: &str, e: impl std::fmt::Display) -> Failure {
  Failure::Failed(format!("{what}: {e}"))
}

/// Runs the anchor until a stop is requested or the daemon loops: [`supervise`] over the `slates daemon` arguments
/// `options` name.
pub(crate) fn run(options: &ProcessOptions) -> Result<(), Failure> {
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
  supervise(options, &args)
}

/// The anchor itself: measure, create the segment, publish the profile, and supervise this very executable started
/// with `daemon_args` until a stop is requested. [`run`] passes the `slates daemon` arguments; a test of this crate
/// passes its own binary's role arguments (`crate::liveness_tests`), so the anchor that judges the daemon there is this
/// one, not a copy.
pub(crate) fn supervise(options: &ProcessOptions, daemon_args: &[String]) -> Result<(), Failure> {
  #[cfg(windows)]
  slates_ipc::process_access::confine_current_holder()
    .map_err(|error| failed("process handle authority", error))?;
  // Before the segment holds a byte: no core dump may carry it (AUD-29-41).
  crate::dumps::exclude_from_dumps()?;
  signal::install().map_err(Failure::Failed)?;
  let profile: MachineProfile = measure(options.quick)?;
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
  // Until a start is measured, the budget holds one start: a daemon that fails once
  // before it ever beat is restarted once.
  let policy = RestartPolicy::derive(RECOVERY_BUDGET_NS, RECOVERY_BUDGET_NS);
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
  #[cfg(windows)]
  supervisor
    .hold_handles(config.held_device_bound())
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
        slates_machine::log_line!(
          "slates anchor: stopped (daemon exit {exit_code:?}{})",
          if killed { ", killed" } else { "" }
        );
        return Ok(());
      }
    }
  }
}

/// One look at the starting daemon in its own CPU time (`slates_machine::process_clock`): its progress and its busiest
/// thread's CPU, judged by the watchdog's rule over the liveness window ([`LIVENESS_BUDGET_NS`]; A-232 derives it for both): a start's first instructions are
/// its one step, so it is alive while its CPU progresses and has run less than a window of it, and starved, not stuck,
/// while its CPU stands still with one of its threads runnable (`slates_rt::process_runnable`, A-228's rule). With no
/// clock reading (the process already gone) the wall window stands alone, as for a shard with no thread clock.
fn judge_start(supervisor: &Supervisor, watch: &mut ShardWatch, now: u64) -> Verdict {
  judge_start_of(supervisor.child_pid(), watch, now)
}

/// [`judge_start`] for the process `pid` names.
fn judge_start_of(pid: Option<u32>, watch: &mut ShardWatch, now: u64) -> Verdict {
  // The spinning rule is a thread's: a window of CPU on one thread without a step, so the CPU judged is the busiest
  // thread's. The process's sum is no such reading: a start whose shards begin in parallel sums past a window while no
  // thread nears one (every start of 23 shards on a 32-CPU Studio was killed, 2026-11-08). Where the threads cannot be
  // read nothing is judged spinning, and the progress alone judges a start wedged.
  let cpu = pid
    .and_then(slates_machine::process_clock::process_cpu)
    .map(|reading| (reading.busiest_thread_ns.unwrap_or(0), reading.progress));
  // Before 2026-11-08 a start had no scheduler reading, so a start held off its CPUs for a window was killed as stuck
  // ("made no progress for 1014 ms; killing it" in `held_rendezvous` on CI's Windows runner).
  let run = pid
    .and_then(slates_rt::process_runnable)
    .map(|runnable| slates_rt::RunReading {
      state: if runnable {
        slates_rt::RunState::Runnable
      } else {
        slates_rt::RunState::Blocked
      },
      waited_ns: None,
    });
  let look = Look {
    steps: 0,
    parked: false,
    cpu,
    run,
    now_ns: now,
  };
  let (verdict, next) = watch.judge(look, LIVENESS_BUDGET_NS);
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
  match judge_lapse(seen.looked_ns.is_some() && looked_age <= seen.bound, || {
    judge_start_of(pid, &mut quiet.watch, now)
  }) {
    Lapse::Withheld => {
      slates_machine::log_line!(
        "slates anchor: the daemon's heartbeat lapsed ({} ms old, past {} ms: the {} ms window plus this anchor's \
         lateness p99 of {} ms over {} waits); killing it",
        beat_age / NS_PER_MS,
        seen.bound / NS_PER_MS,
        LIVENESS_BUDGET_NS / NS_PER_MS,
        lateness.p99_ns() / NS_PER_MS,
        lateness.len()
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
/// marked a look within the lapse bound; `process` judges the daemon's process by the start's rule, asked only when the
/// watchdog did not look. Before A-232 every lapse killed: at `cpu.weight` 1 beside default-weight spinners on CI the
/// watchdog thread itself was held off past the window and the anchor's lateness p99, and the anchor killed a starved
/// daemon in 3 of 20 runs (`diag-a232-starved-1`, beats 3.3 to 4.1 s old against bounds of 2.5 to 3.3 s).
fn judge_lapse(watchdog_looking: bool, process: impl FnOnce() -> Verdict) -> Lapse {
  // MUTANT (d), diagnostics only: every lapse kills, as before A-232 step 5, the looks ignored.
  if watchdog_looking || true {
    return Lapse::Withheld;
  }
  match start_stuck(process()) {
    Some(said) => Lapse::Stuck(said),
    None => Lapse::Starved,
  }
}

/// What a start's verdict says when it is to be killed: `None` while it is alive.
fn start_stuck(verdict: Verdict) -> Option<String> {
  match verdict {
    // A starved start, runnable with its CPU standing still, is not stuck (A-228).
    Verdict::Alive | Verdict::Starved { .. } => None,
    Verdict::Wedged { for_ns } => Some(format!(
      "made no progress for {} ms (the {} ms liveness window)",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    )),
    Verdict::Spinning { cpu_ns } => Some(format!(
      "ran {} ms of CPU on one thread without beating (the {} ms liveness window)",
      cpu_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    )),
  }
}

/// The observation loop.
fn observe(
  supervisor: &mut Supervisor,
  clock: &mut HostClock,
  started: u64,
) -> Result<(), Failure> {
  let mut starting_since = Some(started);
  // The start under way, judged in the daemon's own CPU time until its first beat (the module doc).
  let mut start_watch = ShardWatch::default();
  let mut longest_start_ns = 0u64;
  let mut watch = Watch::default();
  let mut daemon_watch = DaemonWatch::default();
  let mut lateness = Lateness::default();
  slates_machine::log_line!(
    "slates anchor: a daemon's beat is lapsed once older than the {} ms liveness window plus this anchor's own measured \
     lateness p99, over its last {LATENESS_SAMPLES} timed waits",
    LIVENESS_BUDGET_NS / NS_PER_MS
  );
  // A kill already sent to the daemon that has not been reaped yet: it is not sent again, and the loop waits on the
  // daemon's exit (its exit watch ends the wait) rather than judging the same stale heartbeat each turn. Before
  // 2026-10-10 a lapsed daemon was killed and logged again every pause until reaped.
  slates_machine::log_line!(
    "slates anchor: a starting daemon is killed once it makes no CPU progress for the {} ms liveness window, or one of \
     its threads runs that much CPU without its first beat, in its own CPU time ({})",
    LIVENESS_BUDGET_NS / NS_PER_MS,
    match supervisor
      .child_pid()
      .and_then(slates_machine::process_clock::process_cpu)
    {
      Some(cpu) if cpu.busiest_thread_ns.is_some() => "read from the OS, each thread's",
      Some(_) => "read from the OS; its threads not, so nothing is judged spinning",
      None => "none read here: the wall window stands alone",
    }
  );
  let mut kill_sent = false;
  let mut quiet = Quiet::default();
  loop {
    if signal::stop_requested() {
      stop_gracefully(supervisor, clock)?;
      return Ok(());
    }
    let now = clock.monotonic_ns();
    let bound = lateness.lapse_bound_ns();
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
        // A starting daemon is looked at every beat (its first beat times its start, which sets the restart policy);
        // a running one only when its heartbeat could next be found lapsed (`lapse_wait`).
        let pause = if starting_since.is_some() {
          observation_pause()
        } else {
          Duration::from_nanos(lapse_wait(watch, heartbeat_ns, now, bound))
        };
        match starting_since {
          Some(since) if heartbeat_ns >= since => {
            longest_start_ns = longest_start_ns.max(now.saturating_sub(since));
            supervisor
              .set_policy(RestartPolicy::derive(RECOVERY_BUDGET_NS, longest_start_ns).get());
            starting_since = None;
          }
          Some(_) if !kill_sent => {
            if let Some(said) = start_stuck(judge_start(supervisor, &mut start_watch, now)) {
              slates_machine::log_line!(
                "slates anchor: the starting daemon (pid {}) {said}; killing it",
                supervisor
                  .child_pid()
                  .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string())
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
        in_window,
      } => {
        slates_machine::log_line!(
          "slates anchor: daemon exited ({exit_code:?}); restarted as pid {} ({in_window} in the window)",
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

/// Shape: the anchor's own lateness samples kept (A-228): a hundred, the fewest whose nearest-rank p99 is a single
/// reading, as the fleet's put-latency window keeps (`slates_server::fleet`, `PUT_LATENCY_SEALS`).
const LATENESS_SAMPLES: usize = 100;
/// Format: the percentile the anchor's lateness is read at, and the hundred it is out of (the p99).
const LATENESS_PERCENTILE: usize = 99;
const PERCENT: usize = 100;

/// How late the anchor's own timed waits come back (§4.14, A-228): for each of its last [`LATENESS_SAMPLES`] waits that
/// ran its whole pause, how much longer than the pause it took. A machine busy enough to starve the daemon's watchdog
/// starves the anchor too, so the anchor holds the daemon's beat to the liveness window plus what its own clock shows of
/// the machine's lateness, never to a fixed second. A wait that ended early (an exit, a hold, a signal) says nothing of
/// lateness and is not kept. Bounded: the oldest sample leaves as the newest comes.
#[derive(Debug, Default)]
struct Lateness {
  samples: std::collections::VecDeque<u64>,
}

impl Lateness {
  /// Keeps a wait that asked for `asked_ns` and took `took_ns`, if it ran its whole pause.
  fn record(&mut self, asked_ns: u64, took_ns: u64) {
    if took_ns < asked_ns {
      return;
    }
    if self.samples.len() >= LATENESS_SAMPLES {
      self.samples.pop_front();
    }
    self.samples.push_back(took_ns.saturating_sub(asked_ns));
  }

  /// The p99 of the kept samples (nearest rank), zero before any.
  fn p99_ns(&self) -> u64 {
    let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
    sorted.sort_unstable();
    let last = sorted.len().saturating_sub(1);
    let rank = last
      .saturating_mul(LATENESS_PERCENTILE)
      .checked_div(PERCENT)
      .unwrap_or(0);
    sorted.get(rank).copied().unwrap_or(0)
  }

  /// How many samples are kept.
  fn len(&self) -> usize {
    self.samples.len()
  }

  /// Derived: the age past which a daemon's beat is lapsed: the liveness window plus the anchor's measured lateness p99
  /// (A-228).
  fn lapse_bound_ns(&self) -> u64 {
    LIVENESS_BUDGET_NS.saturating_add(self.p99_ns())
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

/// Shape: the observation loop waits at most the liveness budget divided by this (half): its own gap then stays under
/// the budget through any scheduling delay shorter than the other half, since a longer gap reads as the host having
/// slept and restarts the budget ([`lapsed`]).
const OBSERVATION_GAP_PARTS: u64 = 2;

/// How long the observation loop may wait before it must look at a running daemon again (§4.14): until the first
/// instant [`lapsed`] could find its heartbeat (last beaten at `beat_ns`) lapsed, and never more than the budget over
/// [`OBSERVATION_GAP_PARTS`]. A daemon beating every heartbeat is looked at twice a budget; one that stops is found
/// lapsed at the budget's end, not up to a beat later. Exits, holds and signals end the wait sooner
/// ([`await_observation`]). Before 2026-10-10 the loop looked every heartbeat, eleven wakeups a second at rest.
fn lapse_wait(watch: Watch, beat_ns: u64, now_ns: u64, budget_ns: u64) -> u64 {
  let lapses_at = beat_ns
    .max(watch.resumed_ns)
    .saturating_add(budget_ns)
    .saturating_add(1);
  lapses_at
    .saturating_sub(now_ns)
    .min(budget_ns / OBSERVATION_GAP_PARTS)
}

#[cfg(test)]
mod lapse_tests {
  use super::*;

  /// A-232, §4.14. Do: judge a lapsed beat with the watchdog still looking, and with it silent while the daemon's process
  /// is alive, starved, wedged (no CPU, never runnable: frozen) or spinning (a window of CPU on one thread). Expect: a
  /// looking watchdog's lapse is its verdict and kills without asking about the process; a silent one kills only a
  /// wedged or spinning process and spares a starved or alive one, since a starved daemon is not a stuck one.
  #[test]
  fn a_lapse_kills_a_withheld_beat_or_a_stuck_process_and_spares_a_starved_one() {
    let window = LIVENESS_BUDGET_NS;
    assert_eq!(
      judge_lapse(true, || panic!(
        "a looking watchdog's lapse asks nothing of the process"
      )),
      Lapse::Withheld
    );
    for spared in [Verdict::Alive, Verdict::Starved { for_ns: window }] {
      assert_eq!(judge_lapse(false, || spared), Lapse::Starved, "{spared:?}");
    }
    for stuck in [
      Verdict::Wedged { for_ns: window },
      Verdict::Spinning { cpu_ns: window },
    ] {
      assert!(
        matches!(judge_lapse(false, || stuck), Lapse::Stuck(_)),
        "{stuck:?}"
      );
    }
  }

  /// A-228, §4.14. Do: feed the anchor's lateness window a quiet history (waits on time), then a late one (each wait
  /// 300 ms over its pause), with early-ended waits between, and more waits than it keeps. Expect: a quiet anchor holds a
  /// beat to the window alone; a late one to the window plus its lateness p99; early waits change nothing; and the window
  /// keeps only its last hundred samples.
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

  /// §4.14 (2026-10-10): the loop waits for the lapse, not for the next beat. Do: follow `lapse_wait` from turn to turn
  /// over a daemon that beats every tenth of a budget, then stops. Expect every wait at most half a budget while it
  /// beats, and the lapse found exactly one budget and a nanosecond after its last beat.
  #[test]
  fn the_loop_wakes_twice_a_budget_and_finds_a_lapse_at_the_budgets_end() {
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
      let wait = lapse_wait(watch, last_beat, now, BUDGET);
      assert!(wait <= BUDGET / 2, "{wait}");
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
      now += lapse_wait(watch, stopped_at, now, BUDGET);
    }
    assert_eq!(
      found,
      Some(stopped_at + BUDGET + 1),
      "found at the budget's end"
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
        let verdict = judge_start_of(Some(pid), &mut watch, now);
        if matches!(verdict, Verdict::Starved { .. }) {
          starved_looks += 1;
        }
        if let Some(said) = start_stuck(verdict) {
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
        longest_flat >= flat_enough,
        "did not starve: the start's CPU clock stood still for {longest_flat:?} at most, under {flat_enough:?}"
      );
      assert!(
        stuck.is_empty(),
        "a start held off its CPU was judged stuck: {stuck:?} (its clock stood still {longest_flat:?})"
      );
    }
  }
}
