//! The closed refusal taxonomy of the database (§4.4's records part and §4.8's refusals).

use std::fmt;

use slates_anchor::AnchorError;
use slates_mem::MemError;
use slates_wire::error::WireError;

/// A typed refusal from the database; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
  /// A record the operation names does not exist.
  NotFound,
  /// A name or id already exists; the original's id is named.
  AlreadyExists {
    /// The existing record's id.
    existing: [u8; 16],
  },
  /// A lease write carried an epoch below the current one.
  StaleLease {
    /// The current epoch.
    current: u64,
  },
  /// Another holder has the lease.
  LeaseHeld {
    /// The holder's epoch.
    epoch: u64,
  },
  /// A completion for a sequence the client already acknowledged: a stale retry, never
  /// recorded (§4.9 "Exactly-once").
  StaleCompletion {
    /// The highest acknowledged sequence.
    acknowledged_up_to: u32,
  },
  /// The log ring cannot hold the record; the caller snapshots and trims first.
  LogFull {
    /// The bytes the record needs.
    needed: u64,
    /// The bytes free.
    free: u64,
  },
  /// A record in the ring did not verify (the torn tail, or a hostile byte).
  Corrupt {
    /// The sequence expected there.
    seq: u64,
    /// What was wrong.
    reason: &'static str,
  },
  /// The partition's table is at its derived capacity.
  Capacity {
    /// Which table.
    table: &'static str,
  },
  /// The encoding refused.
  Wire(WireError),
  /// The segment refused.
  Anchor(AnchorError),
  /// The memory crate refused.
  Memory(MemError),
}

impl fmt::Display for DbError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NotFound => f.write_str("not found"),
      Self::AlreadyExists { existing } => write!(f, "already exists ({:02x?})", &existing[..4]),
      Self::StaleLease { current } => write!(f, "stale lease (current epoch {current})"),
      Self::LeaseHeld { epoch } => write!(f, "lease held (epoch {epoch})"),
      Self::StaleCompletion { acknowledged_up_to } => {
        write!(
          f,
          "stale completion: the client acknowledged up to {acknowledged_up_to}"
        )
      }
      Self::LogFull { needed, free } => write!(f, "log full: {needed} bytes needed, {free} free"),
      Self::Corrupt { seq, reason } => write!(f, "record {seq} corrupt: {reason}"),
      Self::Capacity { table } => write!(f, "{table} at capacity"),
      Self::Wire(e) => write!(f, "encoding: {e}"),
      Self::Anchor(e) => write!(f, "segment: {e}"),
      Self::Memory(e) => write!(f, "memory: {e}"),
    }
  }
}

impl std::error::Error for DbError {}

impl From<WireError> for DbError {
  fn from(e: WireError) -> Self {
    Self::Wire(e)
  }
}

impl From<AnchorError> for DbError {
  fn from(e: AnchorError) -> Self {
    Self::Anchor(e)
  }
}

impl From<MemError> for DbError {
  fn from(e: MemError) -> Self {
    Self::Memory(e)
  }
}
