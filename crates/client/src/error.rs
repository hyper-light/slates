//! The client's closed refusal taxonomy (§4.4's refusals as the daemon sends them, plus the
//! channel's own: a stall, a lost daemon, a session another client holds).

use std::fmt;

use slates_ipc::IpcError;
use slates_ipc::protocol::Refusal;

/// A typed refusal from the client; never a panic. (`Eq` is not derived: a daemon [`Refusal`] may carry
/// measured probabilities.)
#[derive(Debug, Clone, PartialEq)]
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
  /// This client has issued its last request sequence (`LAST_SEQUENCE`): no fresh request is sent, since
  /// a wrapped sequence would meet the daemon's window as already acknowledged (AUD-29-21). Retries of
  /// issued ids still work; new work goes through a new client — a fresh client id and window.
  SequencesExhausted {
    /// The exhausted client id.
    client: u32,
  },
  /// As many caller-owned operations are outstanding — begun, their replies not yet taken or abandoned — as
  /// the client admits (`limit`, its ring's slots; AUD-29-22): no new one is sent, so no awaited reply is
  /// ever evicted to make room. Take or abandon one, then begin again.
  TooManyOutstanding {
    /// The outstanding operations the client admits.
    limit: usize,
  },
  /// The command ring is full: nothing was sent and no sequence was used; begin again once a reply frees a
  /// slot (AUD-29-19: the async path never waits for one).
  RingFull,
  /// The daemon is gone from this channel: nothing was sent; the caller reconnects
  /// ([`crate::Client::try_reconnect`]) and resends what it still awaits (AUD-29-19, AUD-29-20).
  ChannelLost,
  /// A reconnected channel is still being bound to the consumer (its attest is in flight): nothing was
  /// sent, so no request runs as the account; send again once a reply has been drained.
  Rebinding,
  /// The binding's completion reader failed, or the descriptor it polls closed (AUD-29-20): no reply can be
  /// observed any more, so every call still waiting on one ends with this.
  CompletionLost {
    /// What the reader reported.
    reason: String,
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
      Self::SequencesExhausted { client } => {
        write!(f, "client {client} has issued its last request sequence")
      }
      Self::TooManyOutstanding { limit } => {
        write!(
          f,
          "{limit} operations already outstanding; take or abandon one first"
        )
      }
      Self::RingFull => f.write_str("the command ring is full; nothing was sent"),
      Self::ChannelLost => f.write_str("the daemon is gone from this channel; nothing was sent"),
      Self::Rebinding => {
        f.write_str("the reconnected channel is still being bound; nothing was sent")
      }
      Self::CompletionLost { reason } => write!(f, "the completion reader is lost: {reason}"),
    }
  }
}

impl std::error::Error for ClientError {}

impl From<IpcError> for ClientError {
  fn from(e: IpcError) -> Self {
    ClientError::Ipc(e)
  }
}
