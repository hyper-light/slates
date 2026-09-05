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
use std::process::{Child, Command};

use slates_machine::{Derived, derived};

use crate::error::AnchorError;
use crate::segment::AnchorSegment;

/// Format: the environment variable carrying the anchor's process id to the daemon, so the
/// daemon can watch its supervisor and leave with it (a daemon outliving a dead anchor would
/// hold a segment nobody supervises).
pub const ENV_ANCHOR_PID: &str = "SLATES_ANCHOR_PID";

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
    let starts = recovery_budget_ns / daemon_start_p99_ns.max(1);
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
    }
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

  /// Stops the daemon (the owner asked): kills it and records the stop.
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
