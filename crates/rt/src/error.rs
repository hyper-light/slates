//! The closed refusal taxonomy of the runtime (§4.3, failure matrix).

use std::fmt;

use slates_mem::MemError;

/// A typed refusal from the runtime; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtError {
  /// The shard's task arena is full; admission refused.
  TooManyTasks {
    /// The arena's capacity in tasks.
    capacity: usize,
  },
  /// The task id names a slot whose generation moved on, or never existed.
  StaleTask {
    /// The slot.
    slot: u32,
    /// The generation the id carried.
    generation: u32,
  },
  /// The process has registered every shard id it may.
  TooManyShards {
    /// The bound.
    max: u16,
  },
  /// The OS refused a driver call.
  DriverRefused {
    /// The call.
    call: &'static str,
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// The driver died while the shard waited on it (a simulation injection, or the OS closing
  /// the descriptor beneath us); the shard cancels its tasks and exits.
  DriverLost,
  /// A call that needs the current shard was made from a thread that runs none.
  NotOnShardThread,
  /// A configuration value the runtime cannot honour.
  BadConfig {
    /// Which value.
    what: &'static str,
  },
  /// A memory refusal beneath the runtime.
  Mem(MemError),
}

impl fmt::Display for RtError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::TooManyTasks { capacity } => write!(f, "too many tasks: the arena holds {capacity}"),
      Self::StaleTask { slot, generation } => {
        write!(f, "stale task: slot {slot} generation {generation}")
      }
      Self::TooManyShards { max } => write!(f, "too many shards: at most {max} per process"),
      Self::DriverRefused { call, code } => write!(f, "the OS refused {call} (code {code:?})"),
      Self::DriverLost => f.write_str("the driver was lost while waiting"),
      Self::NotOnShardThread => f.write_str("not on a shard thread"),
      Self::BadConfig { what } => write!(f, "bad configuration: {what}"),
      Self::Mem(e) => write!(f, "memory: {e}"),
    }
  }
}

impl std::error::Error for RtError {}

impl From<MemError> for RtError {
  fn from(e: MemError) -> Self {
    Self::Mem(e)
  }
}

impl RtError {
  /// Captures the current OS error for a refused driver call.
  pub fn os(call: &'static str) -> Self {
    Self::DriverRefused {
      call,
      code: std::io::Error::last_os_error().raw_os_error(),
    }
  }
}
