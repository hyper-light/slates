//! The daemon's refusals: the wire taxonomy for what a client sees, and the crate errors of
//! what it composes for what the operator sees.

use std::fmt;

use slates_anchor::AnchorError;
use slates_db::DbError;
use slates_ipc::IpcError;
use slates_ipc::protocol::Refusal;
use slates_mem::MemError;
use slates_rt::RtError;
use slates_vfs::error::VfsError;

/// A typed refusal from the daemon; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
  /// The runtime refused.
  Runtime(RtError),
  /// The database refused.
  Db(DbError),
  /// The IPC refused.
  Ipc(IpcError),
  /// The segment refused.
  Anchor(AnchorError),
  /// The volume core refused.
  Vfs(VfsError),
  /// The memory crate refused.
  Memory(MemError),
  /// A refusal a client sees.
  Refused(Refusal),
  /// The daemon is not on a shard thread where it expected one.
  NotOnShard,
  /// The observability gate is shut: some chokepoint spans have not registered their emitter, so the
  /// health plane refuses to serve (§2.6, §4.14). Fail-closed — a daemon never serves with a silently
  /// missing span source. Names the missing chokepoints so the operator sees which emitter is absent.
  ChokepointsUnregistered {
    /// The dotted names of the chokepoints that did not register (§4.14 roster order).
    missing: Vec<&'static str>,
  },
}

impl fmt::Display for ServerError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Runtime(e) => write!(f, "runtime: {e}"),
      Self::Db(e) => write!(f, "database: {e}"),
      Self::Ipc(e) => write!(f, "ipc: {e}"),
      Self::Anchor(e) => write!(f, "segment: {e}"),
      Self::Vfs(e) => write!(f, "volume: {e:?}"),
      Self::Memory(e) => write!(f, "memory: {e}"),
      Self::Refused(r) => write!(f, "refused: {r:?}"),
      Self::NotOnShard => f.write_str("not on a shard thread"),
      Self::ChokepointsUnregistered { missing } => write!(
        f,
        "observability incomplete: chokepoint spans not registered: {}",
        missing.join(", ")
      ),
    }
  }
}

impl std::error::Error for ServerError {}

impl From<RtError> for ServerError {
  fn from(e: RtError) -> Self {
    Self::Runtime(e)
  }
}

impl From<DbError> for ServerError {
  fn from(e: DbError) -> Self {
    Self::Db(e)
  }
}

impl From<IpcError> for ServerError {
  fn from(e: IpcError) -> Self {
    Self::Ipc(e)
  }
}

impl From<AnchorError> for ServerError {
  fn from(e: AnchorError) -> Self {
    Self::Anchor(e)
  }
}

impl From<VfsError> for ServerError {
  fn from(e: VfsError) -> Self {
    Self::Vfs(e)
  }
}

impl From<MemError> for ServerError {
  fn from(e: MemError) -> Self {
    Self::Memory(e)
  }
}

/// The wire refusal for a volume-core refusal (§4.4's taxonomy).
pub fn refusal_of_vfs(e: &VfsError) -> Refusal {
  match e {
    VfsError::NotFound => Refusal::NotFound,
    VfsError::AlreadyExists => Refusal::AlreadyExists {
      existing: slates_ipc::protocol::VolumeId::default(),
    },
    VfsError::NoSpace => Refusal::NoSpace,
    VfsError::InvalidName => Refusal::InvalidName,
    VfsError::Destroying => Refusal::Destroying,
    VfsError::Archived => Refusal::Archived,
    VfsError::PolicyMismatch => Refusal::PolicyMismatch,
    VfsError::BaseUnavailable(errno) => Refusal::BaseUnavailable {
      path: String::new(),
      errno: *errno,
    },
    VfsError::Memory(MemError::BudgetExceeded { available, .. }) => Refusal::BudgetExceeded {
      available: *available,
    },
    other => Refusal::BadRequest {
      reason: format!("{other:?}"),
    },
  }
}

/// The wire refusal for a database refusal.
pub fn refusal_of_db(e: &DbError) -> Refusal {
  match e {
    DbError::NotFound => Refusal::NotFound,
    DbError::AlreadyExists { existing } => Refusal::AlreadyExists {
      existing: slates_ipc::protocol::VolumeId { bytes: *existing },
    },
    DbError::StaleLease { current } => Refusal::StaleLease { current: *current },
    DbError::LeaseHeld { epoch } => Refusal::LeaseHeld { epoch: *epoch },
    DbError::StaleCompletion { .. } => Refusal::DuplicateRequest,
    DbError::Capacity { table } => Refusal::BadRequest {
      reason: format!("{table} at capacity"),
    },
    other => Refusal::BadRequest {
      reason: format!("{other}"),
    },
  }
}
