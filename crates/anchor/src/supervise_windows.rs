//! Windows supervision and held-handle transfer (A-198, §2.5 and §4.6): the same supervisor state machine as Unix,
//! with a child created through the explicit Win32 handle list. The job is assigned at creation, so a failed spawn
//! leaves no child outside the anchor's lifetime. Hold requests are acknowledged only after duplication into the
//! anchor; each successor gets a fresh channel, so a predecessor's unread acknowledgement cannot reach it.

use std::ffi::{OsStr, OsString};
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;

use slates_ipc::win_spawn::{Process, Spawn, StandardOutput, environment_block};
use windows_sys::Win32::Foundation::HANDLE;

use crate::handles::{HandleMessage, HandleRefusal};
use crate::held_windows::{Channel, Handoff, Held, channel};

/// Maps an IPC refusal into the process seam's OS error without losing its original cause.
fn io(error: slates_ipc::IpcError) -> std::io::Error {
  match error {
    slates_ipc::IpcError::OsRefused {
      code: Some(code), ..
    } => std::io::Error::from_raw_os_error(code),
    error => std::io::Error::other(error),
  }
}

/// The process operations the shared supervisor uses, with the same signatures as `std::process::Child` on Unix.
pub(super) struct Child {
  process: Process,
}

impl Child {
  pub(super) fn id(&self) -> u32 {
    self.process.id()
  }

  pub(super) fn raw(&self) -> HANDLE {
    self.process.raw_handle()
  }

  pub(super) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
    self
      .process
      .try_wait()
      .map(|status| status.map(|code| ExitStatus::from_raw(u32::from_ne_bytes(code.to_ne_bytes()))))
      .map_err(io)
  }

  pub(super) fn wait(&mut self) -> std::io::Result<ExitStatus> {
    let status = self.process.wait().map_err(io)?;
    status
      .map(|code| ExitStatus::from_raw(u32::from_ne_bytes(code.to_ne_bytes())))
      .ok_or_else(|| std::io::Error::other("a completed child wait returned no exit status"))
  }

  pub(super) fn kill(&mut self) -> std::io::Result<()> {
    self.process.kill().map_err(io)
  }
}

/// Starts the child with only the named handles, plus copies of its standard three. `job` is assigned by
/// `PROC_THREAD_ATTRIBUTE_JOB_LIST` at creation, before the child can run.
pub(super) fn spawn(
  program: &str,
  args: &[String],
  env: &[(String, String)],
  handoff: Option<&Handoff>,
  job: HANDLE,
) -> Result<Child, slates_ipc::IpcError> {
  let args: Vec<OsString> = args.iter().map(OsString::from).collect();
  let env: Vec<(&OsStr, &OsStr)> = env
    .iter()
    .map(|(name, value)| (OsStr::new(name), OsStr::new(value)))
    .collect();
  let env = environment_block(&env);
  let handles = handoff.map_or_else(Vec::new, Handoff::handles);
  let process = slates_ipc::win_spawn::spawn(&Spawn {
    program: OsStr::new(program),
    args: &args,
    environment: Some(&env),
    inherited: &handles,
    output: StandardOutput::Inherited,
    job: Some(job),
  })?;
  Ok(Child { process })
}

/// The anchor's channel and bounded holdings. At most one reply is pending, retained when the peer's quota is full.
pub(super) struct Hold {
  anchor: Channel,
  daemon: Channel,
  held: Held,
  pending: Option<HandleMessage>,
}

impl Hold {
  pub(super) fn new(bound: usize) -> Result<Hold, HandleRefusal> {
    let (anchor, daemon) = channel()?;
    Ok(Hold {
      anchor,
      daemon,
      held: Held::new(bound),
      pending: None,
    })
  }

  /// A new generation of the channel, with the existing holdings. The predecessor is already reaped before a spawn;
  /// no unread request or acknowledgement from it may enter the successor's protocol.
  pub(super) fn handoff(&mut self) -> Result<Handoff, HandleRefusal> {
    let (anchor, daemon) = channel()?;
    self.anchor = anchor;
    self.daemon = daemon;
    self.pending = None;
    self.held.handoff(&self.daemon)
  }

  pub(super) fn len(&self) -> usize {
    self.held.len()
  }
  pub(super) fn refusals(&self) -> u64 {
    self.held.refusals()
  }

  /// A drain admits at most a hold and release per slot. Reads and reply writes have zero wait; a full reply quota
  /// leaves the one pending acknowledgement for the next turn instead of blocking supervision.
  pub(super) fn drain(&mut self, daemon: HANDLE) -> Result<usize, HandleRefusal> {
    let mut applied = 0usize;
    for _ in 0..self.held.bound().saturating_mul(MESSAGES_PER_ENTRY).max(1) {
      if !self.reply()? {
        break;
      }
      let Some(message) = self.anchor.recv(0)? else {
        break;
      };
      self.pending = self.held.apply(daemon, message);
      applied = applied.saturating_add(1);
    }
    self.reply()?;
    Ok(applied)
  }

  fn reply(&mut self) -> Result<bool, HandleRefusal> {
    let Some(message) = self.pending.as_ref() else {
      return Ok(true);
    };
    match self.anchor.send(message, 0) {
      Ok(()) => {
        self.pending = None;
        Ok(true)
      }
      Err(HandleRefusal::TimedOut) => Ok(false),
      Err(error) => Err(error),
    }
  }
}

/// Derived: one hold and one release per entry admitted by the attachment-derived bound.
const MESSAGES_PER_ENTRY: usize = 2;
