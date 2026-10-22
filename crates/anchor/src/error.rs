//! The closed refusal taxonomy of the anchor.

use std::fmt;

use slates_mem::MemError;

/// A typed refusal from the anchor; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorError {
  /// A process stopped while replacing a payload; a separate completed slot may be recovered.
  PublicationInProgress,
  /// The complete Raft publication exceeded its geometry-derived memory budget.
  ConsensusCapacity {
    /// Bytes in the attempted publication.
    offered: usize,
    /// Bytes available in one publication slot.
    capacity: usize,
  },
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
  /// The anchor could not tell which executable it runs, so it cannot restart that one (A-145).
  OwnExecutable {
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// The executable the anchor was started from is gone or now another file, as after an upgrade in place, and the
  /// platform has no link to the running image to restart instead (A-145). Restarting from the path would run another
  /// build than the anchor's, so the restart is refused; restarting the anchor runs the new build.
  ExecutableReplaced {
    /// The path the anchor was started from.
    path: String,
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
      Self::PublicationInProgress => f.write_str("a payload publication is unfinished"),
      Self::ConsensusCapacity { offered, capacity } => write!(
        f,
        "consensus publication of {offered} bytes exceeds its {capacity}-byte budget"
      ),
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
      Self::OwnExecutable { code } => {
        write!(
          f,
          "the anchor's own executable cannot be read (code {code:?})"
        )
      }
      Self::ExecutableReplaced { path } => write!(
        f,
        "the executable at {path} was replaced since the anchor started; restart the anchor to run the new build"
      ),
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
