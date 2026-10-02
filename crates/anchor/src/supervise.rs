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
use std::process::{Child, Command};

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
/// daemon without it binds its own. The daemon adopts it through `slates_rt::tcp::TcpListener::from_fd`.
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
  /// The daemon exited and the policy refused another restart.
  CrashLoop {
    /// The exit code.
    exit_code: Option<i32>,
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
  program: String,
  args: Vec<String>,
  policy: RestartPolicy,
  child: Option<Child>,
  restarts_at_ns: VecDeque<u64>,
  crash_loop: bool,
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
  /// A fleet node's two serve sockets (probe, record) and its network export's listener when the plan has
  /// one, if this daemon is a fleet node (§4.8): bound once by the anchor and handed to each daemon it spawns
  /// (via [`ENV_FLEET_SERVE`]), so the manifest's ports are never free between a daemon's stop and its
  /// restart. Inheritable, as the NFS listener is.
  #[cfg(unix)]
  fleet_serve: Option<(OwnedFd, OwnedFd, Option<OwnedFd>)>,
}

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
  /// A supervisor over `segment` for the daemon command `program args`.
  pub fn new(
    segment: AnchorSegment,
    program: &str,
    args: &[String],
    policy: RestartPolicy,
  ) -> Supervisor {
    Supervisor {
      segment,
      program: program.to_owned(),
      args: args.to_vec(),
      policy,
      child: None,
      restarts_at_ns: VecDeque::new(),
      crash_loop: false,
      lifetime: lifetime::Tie::new(),
      #[cfg(unix)]
      nfs_listener: None,
      #[cfg(unix)]
      fleet_serve: None,
    }
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
    let child = Command::new(&self.program)
      .args(&self.args)
      .envs(env)
      .spawn()
      .map_err(|e| AnchorError::Spawn {
        code: e.raw_os_error(),
      })?;
    self.lifetime.tie(&child)?;
    let restart = self.segment.supervision()?.generation() > 0;
    self
      .segment
      .supervision()?
      .record_start(u64::from(child.id()), now_ns, restart);
    self.child = Some(child);
    Ok(())
  }

  /// Polls the daemon once: running, restarted after an exit, or refused as a crash loop.
  pub fn step(&mut self, now_ns: u64) -> Result<Step, AnchorError> {
    if self.crash_loop {
      return Ok(Step::CrashLoop { exit_code: None });
    }
    let Some(child) = self.child.as_mut() else {
      return Ok(Step::Stopped);
    };
    let status = match child.try_wait() {
      Ok(Some(status)) => status,
      Ok(None) => return Ok(Step::Running),
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
      self.crash_loop = true;
      self.segment.supervision()?.record_exit(true);
      return Ok(Step::CrashLoop { exit_code });
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

  /// The policy.
  pub fn policy(&self) -> RestartPolicy {
    self.policy
  }
}

#[cfg(windows)]
mod lifetime {
  //! Windows: a job object with `KILL_ON_JOB_CLOSE`; the daemon is assigned to it right after
  //! the spawn, so the anchor's death (the last handle closing) ends the daemon.

  use std::process::Child;

  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
  use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
  };

  use crate::error::AnchorError;

  /// The job object, created once per supervisor.
  pub(super) struct Tie {
    job: HANDLE,
  }

  fn spawn_error() -> AnchorError {
    AnchorError::Spawn {
      code: std::io::Error::last_os_error().raw_os_error(),
    }
  }

  impl Tie {
    pub(super) fn new() -> Tie {
      Tie {
        job: std::ptr::null_mut(),
      }
    }

    fn create(&mut self) -> Result<(), AnchorError> {
      if !self.job.is_null() {
        return Ok(());
      }
      // SAFETY: an anonymous job object; the handle is checked before use and closed on drop.
      let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
      if job.is_null() {
        return Err(spawn_error());
      }
      let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
        // SAFETY: a plain-data record the call fills; every zero is a valid field value.
        unsafe { std::mem::zeroed() };
      limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
      let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0);
      // SAFETY: `limits` is the record the information class names, of the size passed.
      let ok = unsafe {
        SetInformationJobObject(
          job,
          JobObjectExtendedLimitInformation,
          std::ptr::from_ref(&limits).cast(),
          size,
        )
      };
      if ok == 0 {
        // SAFETY: a handle this function created and nobody else holds.
        unsafe { CloseHandle(job) };
        return Err(spawn_error());
      }
      self.job = job;
      Ok(())
    }

    pub(super) fn tie(&mut self, child: &Child) -> Result<(), AnchorError> {
      use std::os::windows::io::AsRawHandle;
      self.create()?;
      // SAFETY: both handles are live: the job is ours and the child's is held by `child`.
      let ok = unsafe { AssignProcessToJobObject(self.job, child.as_raw_handle().cast()) };
      if ok == 0 {
        return Err(spawn_error());
      }
      Ok(())
    }
  }

  impl Drop for Tie {
    fn drop(&mut self) {
      if !self.job.is_null() {
        // SAFETY: the handle this object created; closing the last handle ends the job's
        // processes, which is the point.
        unsafe { CloseHandle(self.job) };
      }
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
