//! The client's closed refusal taxonomy (§4.4's refusals as the daemon sends them, plus the
//! channel's own: a stall, a lost daemon, a session another client holds).

use std::fmt;

use slates_ipc::IpcError;
use slates_ipc::protocol::Refusal;

/// A typed refusal from the client; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
  /// The daemon refused the verb.
  Refused(Refusal),
  /// The channel refused.
  Ipc(IpcError),
  /// The daemon answered with a body the verb does not expect (a protocol bug, never silent).
  UnexpectedReply {
    /// The verb.
    verb: &'static str,
  },
  /// The daemon is alive but has not answered inside the deadline.
  Stalled {
    /// The deadline that passed, in nanoseconds.
    after_ns: u64,
  },
  /// The daemon went away and did not come back inside the reconnect budget.
  DaemonGone {
    /// The budget that passed, in nanoseconds.
    after_ns: u64,
  },
  /// The session's client id is held by a live client; the daemon assigned another.
  SessionTaken {
    /// The id the daemon assigned instead.
    assigned: u32,
  },
}

impl fmt::Display for ClientError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Refused(refusal) => write!(f, "refused: {refusal:?}"),
      Self::Ipc(e) => write!(f, "channel: {e}"),
      Self::UnexpectedReply { verb } => write!(f, "unexpected reply to {verb}"),
      Self::Stalled { after_ns } => write!(f, "no reply within {after_ns} ns"),
      Self::DaemonGone { after_ns } => write!(f, "daemon gone for {after_ns} ns"),
      Self::SessionTaken { assigned } => {
        write!(f, "session held by a live client; assigned {assigned}")
      }
    }
  }
}

impl std::error::Error for ClientError {}

impl From<IpcError> for ClientError {
  fn from(e: IpcError) -> Self {
    ClientError::Ipc(e)
  }
}
