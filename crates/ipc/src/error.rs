//! The closed refusal taxonomy of the IPC crate (§4.7's failure matrix, §4.9's refusals).

use std::fmt;

use slates_mem::MemError;

/// A typed refusal from the IPC crate; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
  /// The daemon is not reachable at the endpoint.
  DaemonUnavailable {
    /// The endpoint tried.
    endpoint: String,
  },
  /// The peer's credentials are not the daemon's user.
  PeerRefused {
    /// The peer's uid (or the SID hash on Windows).
    uid: u32,
  },
  /// The ring is full: the client blocks on credit, it never drops; a caller that cannot
  /// block sees this.
  RingFull,
  /// A slot's kind or length is not one of ours.
  BadSlot {
    /// What was wrong.
    reason: &'static str,
  },
  /// A payload larger than a slot carries and no bulk region to spill into.
  PayloadTooLarge {
    /// The bytes offered.
    offered: usize,
    /// The bytes a slot carries.
    capacity: usize,
  },
  /// The region's header is not one of ours.
  Layout {
    /// What was wrong.
    reason: &'static str,
  },
  /// The OS refused; the call is named.
  OsRefused {
    /// The call.
    call: &'static str,
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// The shared memory object refused.
  Memory(MemError),
  /// The rendezvous form is not available on this platform yet.
  Unsupported {
    /// The feature.
    feature: &'static str,
  },
  /// A wait ended at its deadline.
  DeadlineExceeded,
  /// The daemon's derived client bound is reached (§4.7 admission, AC-2.6).
  TooManyClients {
    /// The bound.
    limit: usize,
  },
}

impl fmt::Display for IpcError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::DaemonUnavailable { endpoint } => write!(f, "daemon unavailable at {endpoint}"),
      Self::PeerRefused { uid } => write!(f, "peer refused (uid {uid})"),
      Self::RingFull => f.write_str("ring full"),
      Self::BadSlot { reason } => write!(f, "bad slot: {reason}"),
      Self::PayloadTooLarge { offered, capacity } => {
        write!(
          f,
          "payload of {offered} bytes exceeds the slot's {capacity}"
        )
      }
      Self::Layout { reason } => write!(f, "region layout: {reason}"),
      Self::OsRefused { call, code } => write!(f, "{call} refused (code {code:?})"),
      Self::Memory(e) => write!(f, "shared memory: {e}"),
      Self::Unsupported { feature } => write!(f, "unsupported here: {feature}"),
      Self::DeadlineExceeded => f.write_str("deadline exceeded"),
      Self::TooManyClients { limit } => write!(f, "too many clients (the bound is {limit})"),
    }
  }
}

impl std::error::Error for IpcError {}

impl From<MemError> for IpcError {
  fn from(e: MemError) -> Self {
    Self::Memory(e)
  }
}
