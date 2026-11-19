//! Supervision of the daemon (§2.5 "supervises and restarts the daemon", §2.6 step 1, §4.14
//! "the anchor observes the daemon"): start it with the segment handed over, observe its exit,
//! restart it, and back off when it fails faster than it can recover.
//!
//! **The rule (A-119, A-311).** A start fails when its daemon exits before it has served as long
//! as its start took: its start runs from the spawn to the step that first sees its serving mark
//! (`layout::SUP_SERVING`, which the daemon writes once its start, recovery included, is done),
//! and its service from there. A daemon that served that long recovered: the supervisor publishes
//! the verdict (`layout::SUP_RECOVERED`, which `slates status` reports), its exit is restarted at
//! once, and the count starts again. The first failed start is restarted at once. A second in a
//! row is a crash loop: recorded as `CrashLoop` in the segment, and restarted after a backoff of at
//! least the failed daemon's own life, doubling with each further failure up to the policy's cap
//! ([`RestartPolicy`], the NFSv4 lease its daemon grants, A-525), and never shorter than the failed start took.
//! So a loop spends at most half its time starting, however long each start takes and whether or
//! not each daemon served a moment first, and supervision never gives up: every hard mount waits
//! on the daemon.
//!
//! Until 2026-11-17 the bound was a count of restarts inside a window of the recovery budget (a
//! second) against the starts that budget holds. A start that took longer than the window fell
//! in a window of its own every time, so it was never a crash loop, and was restarted at once for
//! good: B10's Linux gate on adacachy2 (load 67-73) restarted a 407-shard start every second for
//! 27 minutes, each killed after 1,009-1,037 ms of one thread's CPU. And any exit after the
//! serving mark was restarted at once, however soon it came, so a daemon that died a moment after
//! it served was never backed off.
//!
//! The supervisor is driven by its owner (the `slates anchor` command loop): `step` polls the
//! child once and never blocks, so the owner interleaves it with heartbeat observation and its
//! control channel.

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(unix)]
use std::process::Child;
#[cfg(windows)]
use windows::Child;
#[cfg(windows)]
#[path = "supervise_windows.rs"]
mod windows;

use slates_machine::{Derived, derived};

use crate::error::AnchorError;
use crate::layout::RegionKind;
use crate::segment::AnchorSegment;

/// Format: the environment variable carrying the anchor's process id to the daemon, so the
/// daemon can watch its supervisor and leave with it (a daemon outliving a dead anchor would
/// hold a segment nobody supervises).
pub const ENV_ANCHOR_PID: &str = "SLATES_ANCHOR_PID";

/// Format: the environment variable carrying the descriptor of the NFS loopback listener the anchor
/// holds (§4.6, "One TCP loopback listener held by the anchor"), inherited by the daemon across the
/// spawn so its loopback port is stable across a restart — a live mount survives it. Set only when the
/// anchor holds a listener (a Unix concern: NFS is the macOS/Linux bridge, Windows uses WinFsp), and a
/// daemon without it binds its own. The daemon adopts it through `slates_rt::tcp::TcpListener::from_owned`.
pub const ENV_NFS_LISTENER: &str = "SLATES_ANCHOR_NFS";

/// Format: the environment variable carrying a fleet node's two serve sockets (§4.8 "Deployment") as
/// `PROBE_FD,RECORD_FD`, and its network export's listener as a third number when the plan has one
/// (`PROBE_FD,RECORD_FD,EXPORT_FD`; §4.6 AUD-29-75), inherited across the spawn. A supervisor that holds its daemon's fleet ports binds
/// them once and passes them to every daemon it spawns, so no port is released between a daemon's stop
/// and its restart — or between a test harness learning a port and the daemon serving on it
/// (`docs/bugs/2026-09-28-a-released-test-port-was-taken-before-the-daemon-bound-it.md`). The daemon
/// adopts them through `slates_server::FleetTransport::bind` and refuses a socket bound elsewhere than
/// its plan says. Unix, as the NFS listener's descriptor is.
pub const ENV_FLEET_SERVE: &str = "SLATES_ANCHOR_FLEET_SERVE";

/// Shape: the failed starts in a row that make a crash loop (A-311): two, the first repeat. One failure is an event, and
/// restarting it at once is the quickest recovery there is; a second in a row says the next start may fail the same way,
/// so it waits.
const CRASH_LOOP_FAILURES: u32 = 2;

/// The restart policy (A-119, A-311): how long a crash loop's backoff may grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
  /// The longest a crash loop's doubling takes the backoff to. A backoff is never shorter than the failed start's own
  /// life, which may exceed this.
  pub backoff_cap_ns: u64,
}

impl RestartPolicy {
  /// Derived: the cap is the NFSv4 lease the daemon grants every client (`lease_ns`: the server's
  /// `config::NfsV4Caps::lease_ns`, A-425, an idle Linux client's renewal floor and a liveness window, in whole seconds).
  /// Capped there, a crash-looping daemon is started again within one lease of its last failure (and never sooner than
  /// the failed start took), so once the cause clears it serves again before a client that renews at its floor has seen
  /// its lease run out, and the client keeps its opens, locks and delegations instead of losing them to the outage. The
  /// lease is the term the daemon's own clients hold it to; nothing about the cap is a setting. Until A-525 the cap was
  /// the operator's ten-second failover placeholder, past the lease, and until 2026-11-17 60 s, a hard Linux NFS
  /// mount's major timeout (`nfs(5)`, `timeo` 600), which no mount slates makes sets.
  pub fn derive(lease_ns: u64) -> Derived<RestartPolicy> {
    derived!(
      RestartPolicy {
        backoff_cap_ns: lease_ns.max(1),
      },
      "backoff_cap_ns = nfs_v4_lease_ns: a crash loop's doubling stops at the NFSv4 lease the daemon grants, so a \
       client renewing at its floor finds the daemon back before its lease runs out",
      ["nfs_v4_lease_ns"]
    )
  }
}

/// When a daemon whose start ran from `started_ns` to `serving_seen_ns` (the step that first saw its serving mark) has
/// served as long as its start took (A-311): its exit from then on is no failed start. Pure, so every case tests on any
/// host.
pub fn recovered_at(started_ns: u64, serving_seen_ns: u64) -> u64 {
  serving_seen_ns.saturating_add(serving_seen_ns.saturating_sub(started_ns))
}

/// The backoff before the next start of a crash loop (A-311): the failed daemon's own `life_ns` for the loop's first,
/// then twice the last backoff, doubled no further than `cap_ns`, and never shorter than the failed start's own life. So
/// the loop spends at most half its time starting, however long each start takes. Pure, so every case tests on any host.
pub fn crash_loop_backoff(life_ns: u64, last_backoff_ns: Option<u64>, cap_ns: u64) -> u64 {
  last_backoff_ns
    .map_or(life_ns, |last| last.saturating_mul(2))
    .min(cap_ns)
    .max(life_ns)
    .max(1)
}

/// The bounds a graceful stop holds its daemon to ([`Supervisor::stop_step`], A-461), each derived by the owner from what
/// it waits for; none is a fixed second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopBounds {
  /// The age past which the daemon's beat has lapsed and it is killed: the owner's lapse bound for a running daemon
  /// (the `slates anchor` loop's: its liveness window plus its own measured lateness, A-228).
  pub lapse_ns: u64,
  /// How much of its own beating the daemon is given to acknowledge the request, and to exit once past the deadline it
  /// declared ([`stop_overdue`]): the owner's liveness window.
  pub window_ns: u64,
}

/// Whether a stopping daemon is overdue (A-461), judged by its own beating, not by the owner's clock: it has beaten a
/// window (`window_ns`) past the request (`requested_ns`) without acknowledging it, or a window past the deadline it
/// declared (`stop_by`); `beat_ns` is its last beat. A daemon's control loop reads the request at each of its turns, and
/// the turns are what its beats show, so one that has beaten a window since the request has had a window of turns to
/// see it and is deaf; one held off its CPUs beats late and is waited for, and one that stops beating is the lapse's to
/// end ([`StopBounds::lapse_ns`]), so every stop ends. Before, both were counted on the owner's clock from the request
/// (or the deadline) by the bare liveness budget, and a daemon held off its CPUs past it was killed mid-drain, a planned
/// handoff of its leadership cut short. Pure, so every case tests on any host.
pub fn stop_overdue(requested_ns: u64, stop_by: Option<u64>, beat_ns: u64, window_ns: u64) -> bool {
  beat_ns > stop_by.unwrap_or(requested_ns).saturating_add(window_ns)
}

/// What one supervision step found.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
  /// The daemon is running.
  Running,
  /// The daemon exited and was restarted.
  Restarted {
    /// The exit code, when the OS reports one (a signal death has none).
    exit_code: Option<i32>,
    /// The starts that have failed in a row, the one that ended now included: 0 after a daemon that served, 1 after a
    /// first failure, restarted at once.
    failed_in_a_row: u32,
  },
  /// A second start failed in a row: a crash loop, recorded for the health plane, with a restart scheduled after a
  /// backoff (A-119, A-311; supervision never gives up, since every hard mount waits on the daemon).
  CrashLoop {
    /// The exit code.
    exit_code: Option<i32>,
    /// The backoff before the next restart.
    retry_in_ns: u64,
  },
  /// In a crash loop, waiting out the backoff before the restart due at `retry_at_ns`.
  BackingOff {
    /// When the restart is due.
    retry_at_ns: u64,
  },
  /// Nothing to supervise (stopped by the owner).
  Stopped,
}

/// What one step of a graceful stop found ([`Supervisor::stop_step`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Stopping {
  /// The daemon is still stopping: it beats, and it is neither deaf to the request nor past its declared deadline by a
  /// window of its own beating ([`stop_overdue`]).
  Draining,
  /// The daemon has exited and the stop is recorded (nothing restarts it).
  Exited {
    /// The exit code, when the OS reports one (a signal death has none).
    exit_code: Option<i32>,
    /// Whether the anchor had to kill it: a heartbeat that lapsed, or a window of its own beating with no
    /// acknowledgement or past its declared deadline ([`StopBounds`]).
    killed: bool,
  },
}

/// The supervisor.
pub struct Supervisor {
  segment: AnchorSegment,
  /// The profile the anchor published before supervision began, published again before every start: a daemon
  /// republishes the profile at its start, and one killed inside that left it unfinished, which refused every daemon
  /// after it (`PublicationInProgress`; a crash loop under two spinners a core, an 18-core Mac, 2026-11-07).
  profile: Option<Vec<u8>>,
  /// The path this process was started from: every daemon's `argv[0]`, so `ps` and `pgrep -f` show it as before, and
  /// where there is no link to the running image, the file each start is checked against (A-145).
  program: String,
  /// What each start checks the path against, where the platform needs it ([`own_image`]).
  image: own_image::Identity,
  args: Vec<String>,
  policy: RestartPolicy,
  child: Option<Child>,
  /// The starts that have failed in a row: exits before the daemon served as long as its start took (A-311).
  failed_in_a_row: u32,
  /// When a step first saw the current daemon's serving mark, on this supervisor's clock: its start's end. `None` until
  /// then, and again at every start, since a start zeroes the mark.
  serving_seen_ns: Option<u64>,
  /// Whether any daemon this supervisor started has reached its serving mark (`layout::SUP_SERVING`): from then a
  /// client may hold a mount on the instance, so supervision never gives up on it (A-119). Never cleared.
  ever_served: bool,
  /// When a crash loop's backed-off restart is due (A-119); `None` outside a crash loop.
  retry_at_ns: Option<u64>,
  /// The backoff the last crash loop waited, doubled by the next ([`crash_loop_backoff`]); `None` until a crash loop,
  /// and again once a daemon serves.
  backoff_ns: Option<u64>,
  /// When the current daemon was started: its life, at its exit, is what a crash loop's backoff starts from.
  started_at_ns: u64,
  /// Ties the daemon's life to the anchor's where the OS offers it (Windows: a job object
  /// that kills its processes when its last handle closes; Linux: the daemon asks for the
  /// parent-death signal itself; macOS: the daemon watches its parent's id).
  lifetime: lifetime::Tie,
  /// The NFS loopback listener the anchor holds, if any (§4.6): a bound, listening socket whose
  /// descriptor is handed to each daemon it spawns (via [`ENV_NFS_LISTENER`], inherited across the
  /// spawn) so the loopback port survives a restart. The anchor owns it for its whole life, past any
  /// one daemon; the descriptor must be inheritable (not close-on-exec) — the caller clears that
  /// before [`Supervisor::hold_nfs_listener`]. Unix only: NFS is the macOS/Linux bridge (Windows uses
  /// WinFsp), and the descriptor type is Unix's.
  #[cfg(unix)]
  nfs_listener: Option<OwnedFd>,
  /// Environment variables every daemon this supervisor spawns receives: the handoffs of endpoints the owner holds for
  /// the daemon's successors (the client rendezvous, A-114), whose objects the owner keeps alive itself.
  environment: Vec<(String, String)>,
  /// A fleet node's two serve sockets (probe, record) and its network export's listener when the plan has
  /// one, if this daemon is a fleet node (§4.8): bound once by the anchor and handed to each daemon it spawns
  /// (via [`ENV_FLEET_SERVE`]), so the manifest's ports are never free between a daemon's stop and its
  /// restart. Inheritable, as the NFS listener is.
  #[cfg(unix)]
  fleet_serve: Option<(OwnedFd, OwnedFd, Option<OwnedFd>)>,
  /// The FUSE devices the anchor holds across daemon restarts, and the channel daemons send them on (A-61;
  /// [`crate::held`]). Linux only, as FUSE is.
  #[cfg(unix)]
  devices: Option<DeviceHold>,
  /// Windows handles acknowledged into the anchor and handed only to its own successors (A-198).
  #[cfg(windows)]
  windows_handles: Option<windows::Hold>,
  /// The hold refusals and refused log lines last written to the segment (`layout::SUP_HOLD_REFUSALS`,
  /// `SUP_LOG_REFUSALS`), so a step writes only a change.
  published_refusals: (u64, u64),
}

/// The device channel's two ends, what has been received over it, and the refusals counted.
#[cfg(unix)]
#[derive(Debug)]
struct DeviceHold {
  anchor_end: OwnedFd,
  daemon_end: OwnedFd,
  held: crate::held::Held,
  refused: u64,
  /// Which refusals the log has said, so that the log stays bounded while the count goes on (A-213).
  logged: crate::handles::RefusalLog<crate::held::HoldRefusal>,
}

/// What a spawn is handed of the anchor's holdings (A-462): the environment naming the handoff table, and the table's
/// sealed object, which the caller keeps until the spawn holds its own copy of its descriptor. Nothing of either where
/// the anchor holds no device channel.
#[cfg(unix)]
#[derive(Debug, Default)]
struct HeldHandoff {
  /// The variables to set: the table's pointer.
  env: Vec<(String, String)>,
  /// The table's object.
  table: Option<crate::held::TableObject>,
}

/// The most messages one drain applies per device the bound allows: a hold, its session and its release are
/// every message an attachment's life sends, so a drain is bounded whatever a daemon sends, and a daemon at
/// the bound with every attachment changing state at once is drained in one call.
#[cfg(unix)]
/// Derived: one hold + one session + one release per attachment.
const MESSAGES_PER_DEVICE: usize = 3;
/// The most messages one drain applies per connection the bound allows, as `MESSAGES_PER_DEVICE` is for devices (both Unix's).
#[cfg(unix)]
/// Derived: one hold + one release per connection (A-113).
const MESSAGES_PER_CONNECTION: usize = 2;
/// The most messages one drain applies per copy the bound allows, as `MESSAGES_PER_DEVICE` is for devices.
#[cfg(unix)]
/// Derived: one hold + one release per writable copy (A-197).
const MESSAGES_PER_COPY: usize = 2;

impl std::fmt::Debug for Supervisor {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Supervisor")
      .field("program", &self.program)
      .field("policy", &self.policy)
      .field("running", &self.child.is_some())
      .finish()
  }
}

impl Supervisor {
  /// A supervisor over `segment` whose daemons run this process's own executable with `args` (A-145): each start runs
  /// the image this process runs, never whatever the path it was started from holds by then, so an upgrade in place
  /// neither kills the instance at its next restart nor restarts another build under this anchor. Refused when the
  /// process cannot read its own executable's path, or, where a start is checked against the file, that file.
  pub fn new(
    segment: AnchorSegment,
    args: &[String],
    policy: RestartPolicy,
  ) -> Result<Supervisor, AnchorError> {
    let program = std::env::current_exe()
      .map_err(|e| AnchorError::OwnExecutable {
        code: e.raw_os_error(),
      })?
      .to_string_lossy()
      .into_owned();
    let image = own_image::identity(&program)?;
    let profile = segment.read_published(RegionKind::Profile)?;
    Ok(Supervisor {
      segment,
      profile,
      program,
      image,
      args: args.to_vec(),
      policy,
      child: None,
      failed_in_a_row: 0,
      serving_seen_ns: None,
      ever_served: false,
      retry_at_ns: None,
      backoff_ns: None,
      started_at_ns: 0,
      lifetime: lifetime::Tie::new(),
      environment: Vec::new(),
      #[cfg(unix)]
      nfs_listener: None,
      #[cfg(unix)]
      fleet_serve: None,
      #[cfg(unix)]
      devices: None,
      #[cfg(windows)]
      windows_handles: None,
      published_refusals: (0, 0),
    })
  }

  /// Holds the FUSE devices (A-61), NFS loopback connections (A-113) and writable direct maps' copies (A-197) daemons
  /// send, at most `device_bound`, `connection_bound` and `copy_bound` at once: opens the hold channel, whose daemon end
  /// every daemon this supervisor spawns inherits, with every descriptor held so far.
  #[cfg(unix)]
  pub fn hold_descriptors(
    &mut self,
    device_bound: usize,
    connection_bound: usize,
    copy_bound: usize,
  ) -> Result<(), crate::held::HoldRefusal> {
    let (anchor_end, daemon_end) = crate::held::channel()?;
    self.devices = Some(DeviceHold {
      anchor_end,
      daemon_end,
      held: crate::held::Held::new(device_bound, connection_bound, copy_bound),
      refused: 0,
      logged: crate::handles::RefusalLog::default(),
    });
    Ok(())
  }

  /// Opens the Windows hold channel before the first child is spawned. `bounds` holds each class of entry to its own
  /// bound: the live attachments this daemon can admit, as for Unix's device bound (A-198), and the DAX sections its
  /// shards' fill budgets can count, as for Unix's copy bound (A-365, A-448). Acknowledgement follows duplication into
  /// the anchor.
  #[cfg(windows)]
  pub fn hold_handles(
    &mut self,
    bounds: crate::handles::HoldBounds,
  ) -> Result<(), crate::handles::HandleRefusal> {
    if self.child.is_some() || self.windows_handles.is_some() {
      return Err(crate::handles::HandleRefusal::Malformed {
        reason: "the Windows hold must be configured once before starting",
      });
    }
    self.windows_handles = Some(windows::Hold::new(bounds)?);
    Ok(())
  }

  /// Applies a bounded turn of Windows hold messages without waiting; the number applied. A reply whose quota is full
  /// remains pending. A message the channel refuses (short, a checksum that does not match, an unknown shape), or a
  /// reply it cannot write, is counted in `handle_refusals` and dropped, as the Unix device hold counts its refusals:
  /// what a daemon sends never ends supervision. Before 2026-11-03 one malformed message made `step` fail, which ended
  /// the anchor and, through its kill-on-close job, the daemon and every held drive.
  #[cfg(windows)]
  pub fn drain_handles(&mut self) -> usize {
    match (&mut self.windows_handles, &self.child) {
      (Some(hold), Some(child)) => hold.drain(child.raw()),
      _ => 0,
    }
  }

  /// Waits out `pause` between supervision steps, or less (A-213): until a daemon sends on the hold channel or the
  /// daemon exits, as the Unix anchor polls its channel and the daemon's exit. The channel's wake is the event of a read
  /// the hold keeps posted; the exit's is the daemon's process handle. Before 2026-11-04 the Windows anchor parked the
  /// whole pause (up to half the liveness budget, 500 ms), so each WinFsp mount's hold and each unmount's release
  /// waited for it. With nothing to wait on, or a wait the OS refuses, the pause is parked out, so the loop never spins.
  #[cfg(windows)]
  pub fn await_work(&mut self, pause: std::time::Duration) {
    use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED};
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};
    /// Format: nanoseconds in a millisecond, to round the pause up to the wait's unit.
    const NS_PER_MS: u128 = 1_000_000;
    let hold = self.windows_handles.as_mut().and_then(windows::Hold::wake);
    let exit = self.child.as_ref().map(Child::raw);
    let waits: Vec<HANDLE> = hold.into_iter().chain(exit).collect();
    let count = u32::try_from(waits.len()).unwrap_or(0);
    // Rounded up, so a pause under a millisecond is not a wait of none; below INFINITE, which never times out.
    let timeout = u32::try_from(pause.as_nanos().div_ceil(NS_PER_MS))
      .unwrap_or(u32::MAX)
      .min(INFINITE.saturating_sub(1));
    if count == 0 {
      std::thread::park_timeout(pause);
      return;
    }
    // SAFETY: `waits` holds `count` live handles this supervisor owns (the posted read's event, the daemon's process
    // handle), and nothing here closes them during the call; any one of them signalled ends the wait.
    let waited = unsafe { WaitForMultipleObjects(count, waits.as_ptr(), 0, timeout) };
    if waited == WAIT_FAILED {
      std::thread::park_timeout(pause);
    }
  }

  /// Entries held for the Windows daemon's successors, observed through the same hold that the spawn uses.
  #[cfg(windows)]
  pub fn held_handles(&self) -> usize {
    self.windows_handles.as_ref().map_or(0, windows::Hold::len)
  }

  /// Windows holds refused by shape, source-handle validation or the attachment-derived bound, and the channel's own
  /// refusals: a message it could not read whole or decode, or a reply it could not write.
  #[cfg(windows)]
  pub fn handle_refusals(&self) -> u64 {
    self
      .windows_handles
      .as_ref()
      .map_or(0, windows::Hold::refusals)
  }

  /// Every message the hold channel refused over the anchor's life: [`Supervisor::device_refusals`] on Unix,
  /// `Supervisor::handle_refusals` on Windows. Each step publishes it in the segment (`layout::SUP_HOLD_REFUSALS`),
  /// where the daemon's status reads it (A-213): the log says each distinct refusal once, so this count is where an
  /// operator sees the rest.
  #[cfg(unix)]
  pub fn hold_refusals(&self) -> u64 {
    self.device_refusals()
  }

  /// See the Unix arm: the Windows hold's refusals, the channel's and the holdings' own.
  #[cfg(windows)]
  pub fn hold_refusals(&self) -> u64 {
    self.handle_refusals()
  }

  /// Writes [`Supervisor::hold_refusals`] and the log lines this process could not write
  /// (`slates_machine::log::log_lines_refused`) to the segment when either moved since the last write. A segment whose
  /// supervision block cannot be reached is left as it is; the next step tries again.
  fn publish_refusals(&mut self) {
    let refusals = (
      self.hold_refusals(),
      slates_machine::log::log_lines_refused(),
    );
    if refusals == self.published_refusals {
      return;
    }
    if let Ok(supervision) = self.segment.supervision() {
      supervision.record_hold_refusals(refusals.0);
      supervision.record_log_refusals(refusals.1);
      self.published_refusals = refusals;
    }
  }

  /// The anchor's end of the hold channel, to wait on: readable when a daemon sent a message, so the owner's loop
  /// takes a hold at once rather than at its next pause (a daemon's send waits while the channel is full).
  #[cfg(unix)]
  pub fn hold_channel(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
    self
      .devices
      .as_ref()
      .map(|hold| std::os::fd::AsFd::as_fd(&hold.anchor_end))
  }

  /// Applies every device message queued on the channel, up to [`MESSAGES_PER_DEVICE`] per device the bound
  /// allows; returns the messages applied. A refused message (malformed, or a hold past the bound) is counted
  /// ([`Supervisor::device_refusals`]) and its device closed; it never stops the drain or the supervisor.
  #[cfg(unix)]
  pub fn drain_devices(&mut self) -> usize {
    let Some(hold) = self.devices.as_mut() else {
      return 0;
    };
    let most = hold
      .held_bound()
      .saturating_mul(MESSAGES_PER_DEVICE)
      .saturating_add(
        hold
          .held
          .connection_bound()
          .saturating_mul(MESSAGES_PER_CONNECTION),
      )
      .saturating_add(hold.held.copy_bound().saturating_mul(MESSAGES_PER_COPY))
      .max(1);
    let mut applied: usize = 0;
    for _ in 0..most {
      match crate::held::receive(std::os::fd::AsFd::as_fd(&hold.anchor_end)) {
        Ok(None) => break,
        Ok(Some(message)) => match hold.held.apply(message) {
          Ok(()) => applied = applied.saturating_add(1),
          Err(refusal) => hold.refuse(&refusal),
        },
        Err(refusal) => hold.refuse(&refusal),
      }
    }
    applied
  }

  /// The devices held now.
  #[cfg(unix)]
  pub fn held_devices(&self) -> usize {
    self.devices.as_ref().map_or(0, |hold| hold.held.len())
  }

  /// The NFS connections held now (A-113).
  #[cfg(unix)]
  pub fn held_connections(&self) -> usize {
    self
      .devices
      .as_ref()
      .map_or(0, |hold| hold.held.connections())
  }

  /// The hold messages refused so far.
  #[cfg(unix)]
  pub fn device_refusals(&self) -> u64 {
    self.devices.as_ref().map_or(0, |hold| hold.refused)
  }

  /// The handoff to a daemon about to be spawned (A-462): the device channel and every held device, connection and copy,
  /// each descriptor made inheritable, in one table written into a sealed, read-only object whose descriptor the spawn
  /// inherits too ([`crate::held::TableObject`]), and the environment naming it ([`crate::held::ENV_HELD`]). The caller
  /// keeps the object until the spawn holds its own copy. A descriptor that cannot be made inheritable is left out (and
  /// counted): handing a number the daemon does not inherit would have it adopt whatever that number names in its own
  /// table. A table that cannot be made refuses the spawn ([`AnchorError::Handoff`]): a daemon started without it would
  /// take every held entry for none.
  #[cfg(unix)]
  fn device_env(&mut self) -> Result<HeldHandoff, AnchorError> {
    self.drain_devices();
    let Some(hold) = self.devices.as_mut() else {
      return Ok(HeldHandoff::default());
    };
    let inheritable = |fd: &OwnedFd| rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty());
    if let Err(e) = inheritable(&hold.daemon_end) {
      hold.refuse(&crate::held::HoldRefusal::Os {
        call: "fcntl(F_SETFD) on the channel",
        code: e.raw_os_error(),
      });
      return Ok(HeldHandoff::default());
    }
    let mut lost = Vec::new();
    for (attachment, held) in hold.held.iter() {
      if inheritable(&held.device).is_err() {
        lost.push(attachment);
      }
    }
    for attachment in lost {
      let _ = hold
        .held
        .apply(crate::held::Incoming::Release { attachment });
      hold.refuse(&crate::held::HoldRefusal::Malformed {
        reason: "a held device could not be made inheritable",
      });
    }
    let mut lost_connections = Vec::new();
    for (connection, socket) in hold.held.iter_connections() {
      if inheritable(socket).is_err() {
        lost_connections.push(connection);
      }
    }
    for connection in lost_connections {
      hold.held.drop_connection(connection);
      hold.refuse(&crate::held::HoldRefusal::Malformed {
        reason: "a held connection could not be made inheritable",
      });
    }
    let mut lost_copies = Vec::new();
    for (copy, held) in hold.held.iter_copies() {
      if inheritable(&held.object).is_err() {
        lost_copies.push(copy);
      }
    }
    for copy in lost_copies {
      hold.held.drop_copy(copy);
      hold.refuse(&crate::held::HoldRefusal::Malformed {
        reason: "a held copy could not be made inheritable",
      });
    }
    let table = crate::held::Table {
      devices: hold
        .held
        .devices_part(std::os::fd::AsFd::as_fd(&hold.daemon_end)),
      connections: hold.held.connections_part(),
      copies: hold.held.copies_part(),
    };
    let object = crate::held::TableObject::new(&table).map_err(AnchorError::Handoff)?;
    rustix::io::fcntl_setfd(object.descriptor(), rustix::io::FdFlags::empty()).map_err(|e| {
      AnchorError::Handoff(crate::held::HoldRefusal::Os {
        call: "fcntl(F_SETFD) on the handoff table",
        code: e.raw_os_error(),
      })
    })?;
    Ok(HeldHandoff {
      env: vec![(crate::held::ENV_HELD.to_owned(), object.pointer())],
      table: Some(object),
    })
  }

  /// Hands `name=value` to every daemon this supervisor spawns: the handoff of an endpoint the owner holds across
  /// restarts (the client rendezvous, A-114). The owner keeps the endpoint itself alive, and any descriptor it names
  /// inheritable.
  pub fn hold_environment(&mut self, name: &str, value: String) {
    self.environment.push((name.to_owned(), value));
  }

  /// Holds `fd` — a bound, listening loopback socket — as the NFS listener handed to every daemon this
  /// supervisor spawns, so its port is stable across restarts (§4.6). The descriptor must be
  /// inheritable (the caller clears close-on-exec, since it is passed to the daemon across the spawn);
  /// the supervisor owns it for its life, past any one daemon.
  #[cfg(unix)]
  pub fn hold_nfs_listener(&mut self, fd: OwnedFd) {
    self.nfs_listener = Some(fd);
  }

  /// Holds a fleet node's two serve sockets — `probe` and `record`, bound where the node's plan says — and its
  /// network export's listener when the plan has one, and hands them to every daemon this supervisor spawns
  /// (§4.8; §4.6 AUD-29-75). Each must be inheritable (the caller clears close-on-exec); the supervisor owns
  /// them for its life, past any one daemon.
  #[cfg(unix)]
  pub fn hold_fleet_serve(&mut self, probe: OwnedFd, record: OwnedFd, export: Option<OwnedFd>) {
    self.fleet_serve = Some((probe, record, export));
  }

  /// The segment.
  pub fn segment(&self) -> &AnchorSegment {
    &self.segment
  }

  /// The segment, mutably.
  pub fn segment_mut(&mut self) -> &mut AnchorSegment {
    &mut self.segment
  }

  /// Starts the daemon with the segment handed over in its environment.
  pub fn start(&mut self, now_ns: u64) -> Result<(), AnchorError> {
    if self.child.is_some() {
      return Ok(());
    }
    self.started_at_ns = now_ns;
    self.serving_seen_ns = None;
    let mut env = self.segment.handoff_env()?;
    env.push((ENV_ANCHOR_PID.to_owned(), std::process::id().to_string()));
    // Hand the held NFS listener's descriptor over: it is inheritable (not close-on-exec), so the
    // spawned daemon inherits it at the same number and adopts it, keeping the loopback port (§4.6).
    #[cfg(unix)]
    if let Some(fd) = &self.nfs_listener {
      env.push((ENV_NFS_LISTENER.to_owned(), fd.as_raw_fd().to_string()));
    }
    // And the held fleet serve sockets, the same way: the daemon adopts them rather than binding (§4.8).
    #[cfg(unix)]
    if let Some((probe, record, export)) = &self.fleet_serve {
      let mut numbers = format!("{},{}", probe.as_raw_fd(), record.as_raw_fd());
      if let Some(export) = export {
        numbers.push_str(&format!(",{}", export.as_raw_fd()));
      }
      env.push((ENV_FLEET_SERVE.to_owned(), numbers));
    }
    // And the hold channel with every FUSE device (A-61), NFS connection (A-113) and copy (A-197, A-365) held, in one
    // table the spawn inherits (A-462): the daemon takes back the mounts it recovers and answers the connections the
    // dead daemon left. The table's object is kept until the spawn holds its own copy.
    #[cfg(unix)]
    let HeldHandoff {
      env: held_env,
      table: _table,
    } = self.device_env()?;
    #[cfg(unix)]
    env.extend(held_env);
    env.extend(self.environment.iter().cloned());
    // The daemon before this one may have died inside its republication of the profile; the anchor's is whole again
    // before the next reads it. No daemon runs now, so nothing else writes it.
    if let Some(profile) = &self.profile {
      self.segment.publish(RegionKind::Profile, profile)?;
    }
    let child = self.spawn_child(env)?;
    let restart = self.segment.supervision()?.generation() > 0;
    self
      .segment
      .supervision()?
      .record_start(u64::from(child.id()), now_ns, restart);
    self.child = Some(child);
    Ok(())
  }

  /// Unix retains its descriptor-based spawn and the existing executable-identity check (A-145).
  #[cfg(unix)]
  fn spawn_child(&mut self, env: Vec<(String, String)>) -> Result<Child, AnchorError> {
    let child = own_image::command(&self.program, self.image)?
      .args(&self.args)
      .envs(env)
      .spawn()
      .map_err(|error| AnchorError::Spawn {
        code: error.raw_os_error(),
      })?;
    self.lifetime.tie(&child)?;
    Ok(child)
  }

  /// Windows replaces broad inheritance with one explicit handle list, including its job at creation (A-198).
  #[cfg(windows)]
  fn spawn_child(&mut self, mut env: Vec<(String, String)>) -> Result<Child, AnchorError> {
    // Windows' own-image identity remains unchecked as A-145 records; this transport change adds no new check.
    let _identity = self.image;
    let handoff = self
      .windows_handles
      .as_mut()
      .map(windows::Hold::handoff)
      .transpose()?;
    env.push((
      crate::handles::ENV_HANDLES.to_owned(),
      handoff
        .as_ref()
        .map_or("", crate::held_windows::Handoff::env)
        .to_owned(),
    ));
    let job = self.lifetime.job()?;
    windows::spawn(&self.program, &self.args, &env, handoff.as_ref(), job)
      .map_err(|error| AnchorError::Hold(crate::handles::HandleRefusal::Ipc(error)))
  }

  /// Polls the daemon once: running, restarted after an exit, or backed off as a crash loop (the module's rule).
  pub fn step(&mut self, now_ns: u64) -> Result<Step, AnchorError> {
    // Every step takes what the daemon sent, so a device it established is held within a step of its send,
    // and everything a dead daemon sent is applied before its successor starts.
    #[cfg(unix)]
    self.drain_devices();
    #[cfg(windows)]
    self.drain_handles();
    self.publish_refusals();
    if let Some(retry_at_ns) = self.retry_at_ns {
      if now_ns < retry_at_ns {
        return Ok(Step::BackingOff { retry_at_ns });
      }
      self.retry_at_ns = None;
      self.start(now_ns)?;
      return Ok(Step::Restarted {
        exit_code: None,
        failed_in_a_row: self.failed_in_a_row,
      });
    }
    let Some(child) = self.child.as_mut() else {
      return Ok(Step::Stopped);
    };
    let status = match child.try_wait() {
      Ok(Some(status)) => status,
      Ok(None) => {
        // A daemon that served as long as its start took has recovered: the next failure starts a new count, and a new
        // loop backs off from its own failed start again.
        if self.recovered(now_ns)? {
          self.failed_in_a_row = 0;
          self.backoff_ns = None;
        }
        return Ok(Step::Running);
      }
      Err(e) => {
        return Err(AnchorError::Spawn {
          code: e.raw_os_error(),
        });
      }
    };
    self.child = None;
    let exit_code = status.code();
    // Only a daemon that exited before it served as long as its start took failed: one that served that long had
    // recovered, so restarting it is no busy loop, and one that died sooner spent more of its life starting than serving.
    // Its serving mark (`layout::SUP_SERVING`) ends its start, not its first beat, since a recovering daemon beats (its
    // recovery runs in slices). Until 2026-11-09 every exit counted, and a daemon killed while serving more often than
    // the window held starts was backed off as a crash loop (the WinFsp drive test, backed off 16 s at its twentieth kill
    // on both Windows runners); until 2026-11-17 every exit after the mark was restarted at once, however soon it came.
    if self.recovered(now_ns)? {
      self.failed_in_a_row = 0;
      self.backoff_ns = None;
      self.segment.supervision()?.record_exit(false);
      self.start(now_ns)?;
      return Ok(Step::Restarted {
        exit_code,
        failed_in_a_row: 0,
      });
    }
    self.failed_in_a_row = self.failed_in_a_row.saturating_add(1);
    if self.failed_in_a_row >= CRASH_LOOP_FAILURES {
      // Not given up on (A-119): the loop is recorded, and the daemon restarted after a backoff that starts from the
      // failed start's own life and doubles to the cap (A-311), as Kubernetes backs off a crash-looping container. Every
      // hard NFS mount on this daemon waits for it; a supervisor that stopped restarting left them hung for good
      // (2026-10-08: two kills 0.3 s apart on a loaded host, whose recovery had stretched to 160-490 ms, ended
      // supervision).
      let life_ns = now_ns.saturating_sub(self.started_at_ns);
      let retry_in_ns = crash_loop_backoff(life_ns, self.backoff_ns, self.policy.backoff_cap_ns);
      self.backoff_ns = Some(retry_in_ns);
      self.segment.supervision()?.record_exit(true);
      self.retry_at_ns = Some(now_ns.saturating_add(retry_in_ns));
      return Ok(Step::CrashLoop {
        exit_code,
        retry_in_ns,
      });
    }
    self.segment.supervision()?.record_exit(false);
    self.start(now_ns)?;
    Ok(Step::Restarted {
      exit_code,
      failed_in_a_row: self.failed_in_a_row,
    })
  }

  /// Whether the current daemon has served as long as its start took (A-311), on this supervisor's clock: its start
  /// from the spawn to the step that first saw its serving mark, its service from that step to `now_ns`
  /// ([`recovered_at`]). The first time it holds, the verdict is published (`layout::SUP_RECOVERED`). A step that finds
  /// no mark forgets any it saw, since a start zeroes the mark.
  fn recovered(&mut self, now_ns: u64) -> Result<bool, AnchorError> {
    let supervision = self.segment.supervision()?;
    if supervision.serving_since().is_none() {
      self.serving_seen_ns = None;
      return Ok(false);
    }
    self.ever_served = true;
    if supervision.recovered_since().is_some() {
      return Ok(true);
    }
    let seen = *self.serving_seen_ns.get_or_insert(now_ns);
    let recovered = now_ns >= recovered_at(self.started_at_ns, seen);
    if recovered {
      supervision.mark_recovered(now_ns);
    }
    Ok(recovered)
  }

  /// Whether any daemon this supervisor started has reached its serving mark, its exit step included (a daemon that
  /// marked and died at once is seen at its exit). From then a client may have attached to the instance and hold a
  /// mount on it, which waits on a daemon, so nothing gives the instance up (A-119); before, none could have, and an
  /// owner whose starter asked for it may stop the anchor when its daemons crash-loop without ever serving (A-366).
  pub fn ever_served(&self) -> bool {
    self.ever_served
  }

  /// Whether the current daemon has been judged recovered (`layout::SUP_RECOVERED`): until then its owner looks at it
  /// every beat, so the verdict comes within a beat of being due.
  pub fn has_recovered(&self) -> Result<bool, AnchorError> {
    Ok(self.segment.supervision()?.recovered_since().is_some())
  }

  /// How long the current daemon's start took, spawn to the step that first saw its serving mark; `None` before.
  pub fn start_took_ns(&self) -> Option<u64> {
    self
      .serving_seen_ns
      .map(|seen| seen.saturating_sub(self.started_at_ns))
  }

  /// Kills the daemon without taking it (the anchor found its heartbeat lapsed, §4.14
  /// `daemon.alive`): the next `step` sees the exit and applies the policy, so a wedged
  /// daemon counts as a failure like a crashed one.
  pub fn kill(&mut self) -> Result<(), AnchorError> {
    if let Some(child) = self.child.as_mut() {
      child.kill().map_err(|e| AnchorError::Spawn {
        code: e.raw_os_error(),
      })?;
    }
    Ok(())
  }

  /// Asks the daemon to stop **gracefully** (`layout::SUP_STOP`): the daemon hands off any consensus
  /// leadership it holds, declares the deadline it will have exited by (`layout::SUP_STOP_BY`), and exits.
  /// Returns at once; the owner then drives [`stop_step`](Supervisor::stop_step) each tick until it reports
  /// the exit. A supervisor with no daemon has nothing to ask.
  pub fn request_stop(&mut self, now_ns: u64) -> Result<(), AnchorError> {
    if self.child.is_some() {
      self.segment.supervision()?.request_stop(now_ns);
    }
    Ok(())
  }

  /// One step of a graceful stop, never blocking. `Exited` once the daemon has exited — the stop is recorded,
  /// so no later step restarts it. Otherwise the anchor **kills** the daemon, and reports it killed, when its
  /// heartbeat is older than `bounds.lapse_ns` (a wedged daemon), or when it has beaten `bounds.window_ns` past the
  /// request without acknowledging it or past its own declared deadline ([`stop_overdue`]); while none of these holds
  /// it is `Draining`. Every bound is one the daemon declared or one the owner derives (A-461) — none is new.
  pub fn stop_step(&mut self, now_ns: u64, bounds: StopBounds) -> Result<Stopping, AnchorError> {
    // A stop ends a crash loop's backoff too: nothing restarts the daemon after it.
    self.retry_at_ns = None;
    let Some(child) = self.child.as_mut() else {
      self.segment.supervision()?.record_exit(false);
      return Ok(Stopping::Exited {
        exit_code: None,
        killed: false,
      });
    };
    match child.try_wait() {
      Ok(Some(status)) => {
        self.child = None;
        self.segment.supervision()?.record_exit(false);
        return Ok(Stopping::Exited {
          exit_code: status.code(),
          killed: false,
        });
      }
      Ok(None) => {}
      Err(e) => {
        return Err(AnchorError::Spawn {
          code: e.raw_os_error(),
        });
      }
    }
    let give_up = {
      let supervision = self.segment.supervision()?;
      let requested = supervision.stop_requested_at().unwrap_or(now_ns);
      let (alive, _) = supervision.alive(now_ns, bounds.lapse_ns);
      !alive
        || stop_overdue(
          requested,
          supervision.stop_by(),
          supervision.heartbeat_ns(),
          bounds.window_ns,
        )
    };
    if !give_up {
      return Ok(Stopping::Draining);
    }
    let exit_code = match self.child.take() {
      Some(mut child) => {
        let _ = child.kill();
        child.wait().ok().and_then(|status| status.code())
      }
      None => None,
    };
    self.segment.supervision()?.record_exit(false);
    Ok(Stopping::Exited {
      exit_code,
      killed: true,
    })
  }

  /// Stops the daemon at once (the owner asked, or a caller wants a crash-like stop): kills it and records
  /// the stop. The graceful path is [`request_stop`](Supervisor::request_stop) then
  /// [`stop_step`](Supervisor::stop_step).
  pub fn stop(&mut self) -> Result<(), AnchorError> {
    self.retry_at_ns = None;
    if let Some(mut child) = self.child.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
    self.segment.supervision()?.record_exit(false);
    Ok(())
  }

  /// Whether the daemon is running.
  pub fn running(&self) -> bool {
    self.child.is_some()
  }

  /// The running daemon's pid, if one runs: what the owner watches for its exit, so a death is restarted as it happens
  /// rather than at the next observation.
  pub fn child_pid(&self) -> Option<u32> {
    self.child.as_ref().map(Child::id)
  }

  /// The policy.
  pub fn policy(&self) -> RestartPolicy {
    self.policy
  }
}

#[cfg(windows)]
mod lifetime {
  //! Windows' kill-on-close job (§2.5, A-198), passed in the creation attribute list. The job is assigned before the
  //! child can run; its non-inheritable handle stays owned by the anchor.

  use crate::{AnchorError, handles::HandleRefusal};
  use slates_ipc::win_spawn::{ClosedOnDrop, kill_on_close_job};
  use windows_sys::Win32::Foundation::HANDLE;

  pub(super) struct Tie {
    job: Option<ClosedOnDrop>,
  }

  impl Tie {
    pub(super) fn new() -> Tie {
      Tie { job: None }
    }

    pub(super) fn job(&mut self) -> Result<HANDLE, AnchorError> {
      if let Some(job) = &self.job {
        return Ok(job.raw());
      }
      let job =
        kill_on_close_job().map_err(|error| AnchorError::Hold(HandleRefusal::Ipc(error)))?;
      let raw = job.raw();
      self.job = Some(job);
      Ok(raw)
    }
  }
}

#[cfg(not(windows))]
mod lifetime {
  //! Unix: nothing to tie here; the daemon watches its parent (`ENV_ANCHOR_PID`) and, on
  //! Linux, asks the kernel for the parent-death signal.

  use std::process::Child;

  use crate::error::AnchorError;

  /// Nothing held.
  pub(super) struct Tie;

  impl Tie {
    pub(super) fn new() -> Tie {
      Tie
    }

    pub(super) fn tie(&mut self, _child: &Child) -> Result<(), AnchorError> {
      Ok(())
    }
  }
}

/// How a supervisor runs its own image (A-145; `docs/bugs/2026-10-22-an-anchor-restarted-its-daemon-by-a-path-an-upgrade-can-replace.md`).
/// A path names whatever file is there now, and an upgrade in place deletes or replaces the one an anchor was started
/// from: measured 2026-10-22, a daemon killed after that was restarted from the new file (a crash loop, when that file
/// was not a daemon) or refused `ENOENT` (the anchor then ended, the instance with it). One function per platform, one
/// signature.
mod own_image {
  #[cfg(unix)]
  use std::process::Command;

  use crate::error::AnchorError;

  /// Format: the link Linux keeps from every process to the image it runs (`proc(5)`); exec through it reaches that
  /// image even when the path it was started from is gone or holds another file.
  #[cfg(target_os = "linux")]
  const RUNNING_IMAGE: &str = "/proc/self/exe";

  /// What each start is checked against: nothing on Linux, where the link reaches the running image itself.
  #[cfg(target_os = "linux")]
  #[derive(Clone, Copy, Debug)]
  pub(super) struct Identity;

  /// What each start is checked against on the other Unix systems: the device and inode of the file at the path when
  /// the supervisor was made.
  #[cfg(all(unix, not(target_os = "linux")))]
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub(super) struct Identity {
    device: rustix::fs::Dev,
    inode: u64,
  }

  /// Nothing yet on Windows: a running executable can be renamed aside there (not deleted or overwritten), and std
  /// has no stable file identity to check against; the check is owed (GAPS).
  #[cfg(windows)]
  #[derive(Clone, Copy, Debug)]
  pub(super) struct Identity;

  #[cfg(target_os = "linux")]
  pub(super) fn identity(_path: &str) -> Result<Identity, AnchorError> {
    Ok(Identity)
  }

  #[cfg(all(unix, not(target_os = "linux")))]
  pub(super) fn identity(path: &str) -> Result<Identity, AnchorError> {
    let stat = rustix::fs::stat(path).map_err(|e| AnchorError::OwnExecutable {
      code: Some(e.raw_os_error()),
    })?;
    Ok(Identity {
      device: stat.st_dev,
      inode: stat.st_ino,
    })
  }

  #[cfg(windows)]
  pub(super) fn identity(_path: &str) -> Result<Identity, AnchorError> {
    Ok(Identity)
  }

  /// Linux: the running image, through its link, named `path` in the daemon's `argv[0]`.
  #[cfg(target_os = "linux")]
  pub(super) fn command(path: &str, _identity: Identity) -> Result<Command, AnchorError> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(RUNNING_IMAGE);
    command.arg0(path);
    Ok(command)
  }

  /// The other Unix systems: `path`, while it is still the file the supervisor was made from; gone or another file, the
  /// start is refused `ExecutableReplaced`, so another build is never run under this anchor. A replacement made between
  /// this check and the spawn is not caught: no portable call spawns a file by its descriptor there.
  #[cfg(all(unix, not(target_os = "linux")))]
  pub(super) fn command(path: &str, identity: Identity) -> Result<Command, AnchorError> {
    let replaced = || AnchorError::ExecutableReplaced {
      path: path.to_owned(),
    };
    let now = rustix::fs::stat(path).map_err(|_| replaced())?;
    if (now.st_dev, now.st_ino) != (identity.device, identity.inode) {
      return Err(replaced());
    }
    Ok(Command::new(path))
  }
}

#[cfg(unix)]
impl DeviceHold {
  /// The bound the held set was given.
  fn held_bound(&self) -> usize {
    self.held.bound()
  }

  /// Counts every refusal, and says so on the anchor's log the first time each distinct refusal is met
  /// ([`crate::handles::RefusalLog`]), as the Windows hold does, so a daemon sending garbage or a call the OS keeps
  /// refusing cannot grow the log without bound (A-213). Before 2026-11-04 every refusal wrote a line: 20 bad device
  /// messages logged 20 lines.
  fn refuse(&mut self, refusal: &crate::held::HoldRefusal) {
    self.refused = self.refused.saturating_add(1);
    if self.logged.first_sighting(refusal) {
      slates_machine::log_line!("slates anchor: a hold message was refused: {refusal}");
    }
  }
}

#[cfg(test)]
mod backoff_tests {
  use super::*;

  /// Shape: the lease the tests derive their cap from: ten seconds, many lives long.
  const LEASE_NS: u64 = 10_000_000_000;
  /// Shape: a failed start's life, many times under the lease: 300 ms.
  const LIFE_NS: u64 = 300_000_000;

  /// A-311, §2.6 step 1. Do: follow a crash loop's backoffs over failed starts of one life, under the cap the anchor
  /// derives from a lease many lives long. Expect the first backoff the failed start's own life, each next twice
  /// the last, and the cap once the doubling passes it, held there.
  #[test]
  fn a_crash_loops_backoff_starts_at_the_failed_starts_life_and_doubles_to_the_cap() {
    let cap = RestartPolicy::derive(LEASE_NS).get().backoff_cap_ns;
    let mut last = None;
    let mut backoffs = Vec::new();
    for _ in 0..8 {
      let backoff = crash_loop_backoff(LIFE_NS, last, cap);
      backoffs.push(backoff);
      last = Some(backoff);
    }
    assert_eq!(
      backoffs,
      [
        LIFE_NS,
        2 * LIFE_NS,
        4 * LIFE_NS,
        8 * LIFE_NS,
        16 * LIFE_NS,
        32 * LIFE_NS,
        LEASE_NS,
        LEASE_NS
      ]
    );
  }

  /// A-311. Do: ask when daemons have served as long as their starts took: a start of a life from the clock's zero, one
  /// from later, one of no time, and one whose serving mark the clock saw before its start (a clock that went back).
  /// Expect each recovered a start's length after its mark was seen, at the mark itself for a start of no time, and
  /// never before the mark.
  #[test]
  fn a_daemon_has_recovered_once_it_has_served_as_long_as_its_start_took() {
    assert_eq!(recovered_at(0, LIFE_NS), 2 * LIFE_NS);
    assert_eq!(
      recovered_at(LEASE_NS, LEASE_NS + LIFE_NS),
      LEASE_NS + 2 * LIFE_NS
    );
    assert_eq!(recovered_at(LEASE_NS, LEASE_NS), LEASE_NS);
    assert_eq!(recovered_at(LEASE_NS, LIFE_NS), LIFE_NS);
  }

  /// A-311. Do: back off failed starts longer than the cap, and a loop whose starts grow longer mid-way. Expect every
  /// backoff at least the failed start's own life, so a loop never spends more than half its time starting, whatever
  /// the cap; and a start that takes no measurable time still waits a nanosecond, never none.
  #[test]
  fn a_backoff_is_never_shorter_than_the_failed_starts_own_life() {
    let cap = RestartPolicy::derive(LEASE_NS).get().backoff_cap_ns;
    let long = 3 * LEASE_NS;
    assert_eq!(crash_loop_backoff(long, None, cap), long);
    assert_eq!(crash_loop_backoff(long, Some(long), cap), long);
    assert_eq!(crash_loop_backoff(long, Some(LIFE_NS), cap), long);
    assert_eq!(crash_loop_backoff(0, None, cap), 1);
    assert_eq!(
      RestartPolicy::derive(0).get().backoff_cap_ns,
      1,
      "no lease still caps at a nanosecond, never at zero"
    );
  }
}

#[cfg(test)]
mod stop_tests {
  use super::*;

  /// Shape: the window the stop is judged by in these cases: a second, the liveness window on a quiet anchor.
  const WINDOW_NS: u64 = 1_000_000_000;
  /// Shape: the request's time on the host's clock.
  const ASKED_NS: u64 = 50 * WINDOW_NS;
  /// Shape: the deadline a daemon declares, a few windows past the request.
  const DEADLINE_NS: u64 = ASKED_NS + 3 * WINDOW_NS;

  /// A-461, §2.5 "supervises and restarts the daemon". Do: judge a stopping daemon by its last beat: one that has not
  /// acknowledged and was held off its CPUs, so its last beat came just after the request while the owner's clock ran
  /// two windows on; one that beat a window past the request and is deaf; one that acknowledged and beats inside its
  /// deadline and its window after it; and one beating past both. Expect: the held daemon waited for (the owner's clock
  /// alone, as before, killed it mid-drain), the deaf one overdue, the draining one waited for, the overrunning one
  /// overdue.
  #[test]
  fn a_stopping_daemon_is_overdue_by_its_own_beating_not_by_the_owners_clock() {
    let held_beat = ASKED_NS + WINDOW_NS / 10;
    assert!(
      !stop_overdue(ASKED_NS, None, held_beat, WINDOW_NS),
      "a daemon held off its CPUs since the request has beaten no window of it"
    );
    assert!(
      stop_overdue(ASKED_NS, None, ASKED_NS + WINDOW_NS + 1, WINDOW_NS),
      "a window of its own beating with no acknowledgement is deaf"
    );
    assert!(
      !stop_overdue(
        ASKED_NS,
        Some(DEADLINE_NS),
        DEADLINE_NS + WINDOW_NS,
        WINDOW_NS
      ),
      "inside its deadline and a window past it, it drains"
    );
    assert!(
      stop_overdue(
        ASKED_NS,
        Some(DEADLINE_NS),
        DEADLINE_NS + WINDOW_NS + 1,
        WINDOW_NS
      ),
      "a window of its own beating past the deadline it declared is overdue"
    );
  }
}
