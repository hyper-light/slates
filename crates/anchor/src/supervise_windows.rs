//! Windows supervision and held-handle transfer (A-198, §2.5 and §4.6): the same supervisor state machine as Unix,
//! with a child created through the explicit Win32 handle list. The job is assigned at creation, so a failed spawn
//! leaves no child outside the anchor's lifetime. Hold requests are acknowledged only after duplication into the
//! anchor; each successor gets a fresh channel, so a predecessor's unread acknowledgement cannot reach it.

use std::ffi::{OsStr, OsString};
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;

use slates_ipc::win_spawn::{Process, Spawn, StandardOutput, environment_block};
use windows_sys::Win32::Foundation::HANDLE;

use crate::handles::{HandleMessage, HandleRefusal, RefusalLog};
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
  let process_security = slates_ipc::process_access::ProcessSecurity::handle_owner()?;
  let process = slates_ipc::win_spawn::spawn(&Spawn {
    program: OsStr::new(program),
    args: &args,
    environment: Some(&env),
    inherited: &handles,
    output: StandardOutput::Inherited,
    job: Some(job),
    process_security: Some(&process_security),
  })?;
  Ok(Child { process })
}

/// The anchor's channel and bounded holdings. At most one reply is pending, retained when the peer's quota is full.
pub(super) struct Hold {
  anchor: Channel,
  daemon: Channel,
  held: Held,
  pending: Option<HandleMessage>,
  /// Messages the channel refused (not read whole or not decoded) and replies it could not write, over the anchor's
  /// life; the holdings' own refusals are `held`'s.
  channel_refusals: u64,
  /// Which of those refusals the log has said, so that the log stays bounded while the count goes on.
  logged: RefusalLog<HandleRefusal>,
}

impl Hold {
  pub(super) fn new(bound: usize) -> Result<Hold, HandleRefusal> {
    let (anchor, daemon) = channel()?;
    Ok(Hold {
      anchor,
      daemon,
      held: Held::new(bound),
      pending: None,
      channel_refusals: 0,
      logged: RefusalLog::default(),
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
    self.held.refusals().saturating_add(self.channel_refusals)
  }

  /// A drain admits at most a hold and release per slot. Reads and reply writes have zero wait; a full reply quota
  /// leaves the one pending acknowledgement for the next turn instead of blocking supervision. A message the channel
  /// refuses is counted and dropped, and the drain reads on, since the next message may be whole; any other refusal of a
  /// read is counted and ends this turn's drain. A reply the channel cannot write is counted and dropped: its daemon's
  /// own wait ends at its deadline, a typed refusal there. Nothing a daemon sends ends supervision.
  pub(super) fn drain(&mut self, daemon: HANDLE) -> usize {
    let mut applied = 0usize;
    for _ in 0..self.held.bound().saturating_mul(MESSAGES_PER_ENTRY).max(1) {
      if !self.reply() {
        break;
      }
      match self.anchor.posted_recv() {
        Ok(None) => break,
        Ok(Some(message)) => {
          self.pending = self.held.apply(daemon, message);
          applied = applied.saturating_add(1);
        }
        Err(refusal) => {
          self.refuse(&refusal);
          if !matches!(refusal, HandleRefusal::Malformed { .. }) {
            break;
          }
        }
      }
    }
    self.reply();
    applied
  }

  /// What the anchor's loop waits on for this hold (A-213): the event of the read posted on the channel, signalled when
  /// a message arrives. `None` while a reply is pending: the daemon's read of its last reply is nothing this loop can
  /// wait on, and a message already taken behind it would signal at once and spin the loop, so the loop waits out its
  /// step instead, as before. A read that cannot be posted is counted, and the loop waits out its step too.
  pub(super) fn wake(&mut self) -> Option<HANDLE> {
    if self.pending.is_some() {
      return None;
    }
    match self.anchor.posted_event() {
      Ok(event) => Some(event),
      Err(refusal) => {
        self.refuse(&refusal);
        None
      }
    }
  }

  /// Sends the pending reply, if any: `false` while the peer's quota is full (it stays pending for the next turn).
  fn reply(&mut self) -> bool {
    let Some(message) = self.pending.as_ref() else {
      return true;
    };
    match self.anchor.send(message, 0) {
      Ok(()) => {
        self.pending = None;
        true
      }
      Err(HandleRefusal::TimedOut) => false,
      Err(refusal) => {
        self.pending = None;
        self.refuse(&refusal);
        true
      }
    }
  }

  /// Counts every refusal of the channel, and says so on the anchor's log the first time each distinct refusal is met
  /// ([`RefusalLog`]), so a read the OS keeps refusing or a daemon sending garbage cannot grow the log without bound. The
  /// holdings are untouched.
  fn refuse(&mut self, refusal: &HandleRefusal) {
    self.channel_refusals = self.channel_refusals.saturating_add(1);
    if self.logged.first_sighting(refusal) {
      eprintln!("slates anchor: a hold message was refused: {refusal}");
    }
  }
}

/// Derived: one hold and one release per entry admitted by the attachment-derived bound.
const MESSAGES_PER_ENTRY: usize = 2;
