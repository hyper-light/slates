//! Supervision of the daemon (§2.5 "supervises and restarts the daemon", §2.6 step 1, §4.14
//! "the anchor observes the daemon"): start it with the segment handed over, observe its exit,
//! restart it, and stop when it fails faster than it can recover.
//!
//! The restart bound is derived, not a retry count: a daemon that has failed more times inside
//! the recovery budget than that budget holds daemon starts is not recovering, and restarting
//! it again is a busy loop; supervision then records `CrashLoop` in the segment and refuses.
//! The supervisor is driven by its owner (the `slates anchor` command loop): `step` polls the
//! child once and never blocks, so the owner interleaves it with heartbeat observation and its
//! control channel.

use std::collections::VecDeque;
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

/// Format: the longest a crash loop's backoff grows to: the major timeout of a hard Linux NFS mount over TCP
/// (`nfs(5)`: `timeo` defaults to 600 deciseconds, 60 s), the cadence a mounted client retries a silent server at, so
/// a longer wait gains it nothing (A-119).
pub const RETRY_CAP_NS: u64 = 60_000_000_000;

/// The restart policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
  /// The window over which restarts are counted: the recovery budget.
  pub window_ns: u64,
  /// Restarts allowed inside the window.
  pub max_restarts: u32,
}

impl RestartPolicy {
  /// Derived: the window is the recovery budget; the restarts allowed inside it are how many
  /// daemon starts (at the measured start p99) the budget holds, at least one, so a daemon
  /// failing faster than it can start is a loop and one that recovers is not.
  pub fn derive(recovery_budget_ns: u64, daemon_start_p99_ns: u64) -> Derived<RestartPolicy> {
    let starts = recovery_budget_ns
      .checked_div(daemon_start_p99_ns.max(1))
      .unwrap_or(0);
    let max_restarts = u32::try_from(starts).unwrap_or(u32::MAX).max(1);
    derived!(
      RestartPolicy {
        window_ns: recovery_budget_ns,
        max_restarts,
      },
      "max_restarts = max(1, recovery_budget_ns / daemon_start_p99_ns); window_ns = recovery_budget_ns",
      ["recovery_budget_ns", "daemon_start_p99_ns"]
    )
  }
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
    /// Restarts inside the window after this one.
    in_window: u32,
  },
  /// The daemon exited faster than it could recover, past the policy's bound: a crash loop, recorded for the health
  /// plane, with a restart scheduled after a backoff (A-119; supervision never gives up, since every hard mount waits
  /// on the daemon).
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
  /// The daemon is still stopping: it acknowledged the request and is inside its declared deadline, or the
  /// liveness budget has not yet passed without an acknowledgement.
  Draining,
  /// The daemon has exited and the stop is recorded (nothing restarts it).
  Exited {
    /// The exit code, when the OS reports one (a signal death has none).
    exit_code: Option<i32>,
    /// Whether the anchor had to kill it: no acknowledgement within the liveness budget, a heartbeat that
    /// lapsed, or a declared deadline overrun by the budget.
    killed: bool,
  },
}

/// The supervisor.
pub struct Supervisor {
  segment: AnchorSegment,
  /// The path this process was started from: every daemon's `argv[0]`, so `ps` and `pgrep -f` show it as before, and
  /// where there is no link to the running image, the file each start is checked against (A-145).
  program: String,
  /// What each start checks the path against, where the platform needs it ([`own_image`]).
  image: own_image::Identity,
  args: Vec<String>,
  policy: RestartPolicy,
  child: Option<Child>,
  restarts_at_ns: VecDeque<u64>,
  /// When a crash loop's backed-off restart is due (A-119); `None` outside a crash loop.
  retry_at_ns: Option<u64>,
  /// The backoff the next crash loop waits: the window, doubled by each crash loop to [`RETRY_CAP_NS`], and back to
  /// the window once a daemon outlives it.
  backoff_ns: u64,
  /// When the current daemon was started.
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
  /// The hold refusals last written to the segment (`layout::SUP_HOLD_REFUSALS`), so a step writes only a change.
  published_refusals: u64,
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
    Ok(Supervisor {
      segment,
      program,
      image,
      args: args.to_vec(),
      policy,
      child: None,
      restarts_at_ns: VecDeque::new(),
      retry_at_ns: None,
      backoff_ns: policy.window_ns,
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
      published_refusals: 0,
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

  /// Opens the Windows hold channel before the first child is spawned. `bound` is the number of live attachments
  /// this daemon can admit, as for Unix's device bound (A-198). Acknowledgement follows duplication into the anchor.
  #[cfg(windows)]
  pub fn hold_handles(&mut self, bound: usize) -> Result<(), crate::handles::HandleRefusal> {
    if self.child.is_some() || self.windows_handles.is_some() {
      return Err(crate::handles::HandleRefusal::Malformed {
        reason: "the Windows hold must be configured once before starting",
      });
    }
    self.windows_handles = Some(windows::Hold::new(bound)?);
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

  /// Writes [`Supervisor::hold_refusals`] to the segment when it moved since the last write. A segment whose
  /// supervision block cannot be reached is left as it is; the next step tries again.
  fn publish_hold_refusals(&mut self) {
    let refusals = self.hold_refusals();
    if refusals == self.published_refusals {
      return;
    }
    if let Ok(supervision) = self.segment.supervision() {
      supervision.record_hold_refusals(refusals);
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

  /// The environment handing the device channel and every held device to a daemon about to be spawned, each
  /// descriptor made inheritable. A descriptor that cannot be made inheritable is left out (and counted): handing
  /// a number the daemon does not inherit would have it adopt whatever that number names in its own table.
  #[cfg(unix)]
  fn device_env(&mut self) -> Vec<(String, String)> {
    self.drain_devices();
    let Some(hold) = self.devices.as_mut() else {
      return Vec::new();
    };
    let inheritable = |fd: &OwnedFd| rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty());
    if let Err(e) = inheritable(&hold.daemon_end) {
      hold.refuse(&crate::held::HoldRefusal::Os {
        call: "fcntl(F_SETFD) on the channel",
        code: e.raw_os_error(),
      });
      return Vec::new();
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
    let value = hold
      .held
      .env_value(std::os::fd::AsFd::as_fd(&hold.daemon_end));
    vec![
      (crate::held::ENV_DEVICES.to_owned(), value),
      (
        crate::held::ENV_CONNECTIONS.to_owned(),
        hold.held.connections_env_value(),
      ),
      (
        crate::held::ENV_COPIES.to_owned(),
        hold.held.copies_env_value(),
      ),
    ]
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

  /// Replaces the policy (the anchor re-derives it as daemon starts are measured).
  pub fn set_policy(&mut self, policy: RestartPolicy) {
    // A backoff under way keeps its length; the next one starts from the new window.
    if self.retry_at_ns.is_none() {
      self.backoff_ns = policy.window_ns;
    }
    self.policy = policy;
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
    // And the hold channel with every FUSE device (A-61) and NFS connection (A-113) held: the daemon takes back the
    // mounts it recovers and answers the connections the dead daemon left.
    #[cfg(unix)]
    env.extend(self.device_env());
    env.extend(self.environment.iter().cloned());
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

  /// Polls the daemon once: running, restarted after an exit, or refused as a crash loop.
  pub fn step(&mut self, now_ns: u64) -> Result<Step, AnchorError> {
    // Every step takes what the daemon sent, so a device it established is held within a step of its send,
    // and everything a dead daemon sent is applied before its successor starts.
    #[cfg(unix)]
    self.drain_devices();
    #[cfg(windows)]
    self.drain_handles();
    self.publish_hold_refusals();
    if let Some(retry_at_ns) = self.retry_at_ns {
      if now_ns < retry_at_ns {
        return Ok(Step::BackingOff { retry_at_ns });
      }
      self.retry_at_ns = None;
      self.start(now_ns)?;
      return Ok(Step::Restarted {
        exit_code: None,
        in_window: u32::try_from(self.restarts_at_ns.len()).unwrap_or(u32::MAX),
      });
    }
    let Some(child) = self.child.as_mut() else {
      return Ok(Step::Stopped);
    };
    let status = match child.try_wait() {
      Ok(Some(status)) => status,
      Ok(None) => {
        // A daemon that outlived the window recovered: the next crash loop backs off from the window again.
        if now_ns.saturating_sub(self.started_at_ns) > self.policy.window_ns {
          self.backoff_ns = self.policy.window_ns;
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
    self.restarts_at_ns.push_back(now_ns);
    while self
      .restarts_at_ns
      .front()
      .is_some_and(|t| now_ns.saturating_sub(*t) > self.policy.window_ns)
    {
      self.restarts_at_ns.pop_front();
    }
    let in_window = u32::try_from(self.restarts_at_ns.len()).unwrap_or(u32::MAX);
    if in_window > self.policy.max_restarts {
      // Not given up on (A-119): the loop is recorded, and the daemon restarted after a backoff that doubles each time
      // to the cap, as Kubernetes backs off a crash-looping container. Every hard NFS mount on this daemon waits for
      // it; a supervisor that stopped restarting left them hung for good (2026-10-08: two kills 0.3 s apart on a
      // loaded host, whose recovery had stretched to 160-490 ms, ended supervision).
      self.segment.supervision()?.record_exit(true);
      let retry_in_ns = self.backoff_ns;
      self.backoff_ns = self.backoff_ns.saturating_mul(2).min(RETRY_CAP_NS);
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
      in_window,
    })
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
  /// so no later step restarts it. Otherwise the anchor **kills** the daemon, and reports it killed, when it
  /// has not acknowledged the request within `liveness_budget_ns`, when its heartbeat is older than that
  /// budget (a wedged daemon), or when it has run past its own declared deadline by that budget; while none
  /// of these holds it is `Draining`. Every bound is one the daemon declared or the liveness budget the
  /// anchor already holds it to — none is new.
  pub fn stop_step(
    &mut self,
    now_ns: u64,
    liveness_budget_ns: u64,
  ) -> Result<Stopping, AnchorError> {
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
      let (alive, _) = supervision.alive(now_ns, liveness_budget_ns);
      let overdue = match supervision.stop_by() {
        None => now_ns.saturating_sub(requested) > liveness_budget_ns,
        Some(deadline) => now_ns > deadline.saturating_add(liveness_budget_ns),
      };
      !alive || overdue
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
      eprintln!("slates anchor: a hold message was refused: {refusal}");
    }
  }
}
