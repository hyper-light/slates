//! The closed refusal taxonomy of the anchor.

use std::fmt;

use slates_mem::MemError;

/// A typed refusal from the anchor; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorError {
  /// The shared memory object refused.
  Memory(MemError),
  /// The segment's header is not one of ours, or is torn.
  Layout {
    /// What was wrong.
    reason: &'static str,
  },
  /// The segment was made for another machine.
  Identity {
    /// The identity hash in the segment, hex.
    cached: String,
    /// This machine's identity hash, hex.
    current: String,
  },
  /// The geometry does not fit the mapped length, or a region lies outside it.
  Geometry {
    /// What was wrong.
    reason: &'static str,
  },
  /// A profile payload larger than its region.
  ProfileTooLarge {
    /// The bytes offered.
    offered: usize,
    /// The region's capacity.
    capacity: usize,
  },
  /// The daemon could not be started.
  Spawn {
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// The daemon failed more often inside the window than the policy allows; supervision
  /// stopped restarting it.
  CrashLoop {
    /// Restarts inside the window.
    restarts: u32,
    /// The window, nanoseconds.
    window_ns: u64,
  },
  /// No daemon is running.
  NotRunning,
}

impl fmt::Display for AnchorError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Memory(e) => write!(f, "shared memory: {e}"),
      Self::Layout { reason } => write!(f, "segment layout: {reason}"),
      Self::Identity { cached, current } => {
        write!(f, "segment is for another machine ({cached} != {current})")
      }
      Self::Geometry { reason } => write!(f, "segment geometry: {reason}"),
      Self::ProfileTooLarge { offered, capacity } => {
        write!(
          f,
          "profile of {offered} bytes exceeds its {capacity}-byte region"
        )
      }
      Self::Spawn { code } => write!(f, "daemon spawn refused (code {code:?})"),
      Self::CrashLoop {
        restarts,
        window_ns,
      } => {
        write!(
          f,
          "daemon crash loop: {restarts} restarts inside {window_ns} ns"
        )
      }
      Self::NotRunning => f.write_str("no daemon is running"),
    }
  }
}

impl std::error::Error for AnchorError {}

impl From<MemError> for AnchorError {
  fn from(e: MemError) -> Self {
    Self::Memory(e)
  }
}
