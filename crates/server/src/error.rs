//! The daemon's refusals: the wire taxonomy for what a client sees, and the crate errors of
//! what it composes for what the operator sees.

use std::fmt;

use slates_anchor::AnchorError;
use slates_db::DbError;
use slates_ipc::IpcError;
use slates_ipc::protocol::{DatabaseRefusal, Refusal, TargetRefusal, VolumeRefusal};
use slates_mem::MemError;
use slates_rt::RtError;
use slates_vfs::error::VfsError;

/// A typed refusal from the daemon; never a panic. (`Eq` is not derived: a [`Refusal`] may carry measured
/// probabilities.)
#[derive(Debug, Clone, PartialEq)]
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
  /// The grant-issuer secret could not be minted at start (§4.13): the crypto provider's secure random
  /// refused. Fail-closed — a daemon with no issuer authority would make every landing un-grantable while
  /// looking healthy — so the daemon does not start; the provider's reason is carried.
  IssuerSecret(String),
  /// The client session secret could not be minted at the first start under an anchor (`session_key`): the crypto
  /// provider's secure random refused. Fail-closed: a daemon with no secret could not tell a client resuming its
  /// session from a stranger naming its id, so it does not start; the provider's reason is carried.
  SessionSecret(String),
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
      Self::IssuerSecret(reason) => {
        write!(f, "the grant-issuer secret could not be minted: {reason}")
      }
      Self::SessionSecret(reason) => {
        write!(f, "the client session secret could not be minted: {reason}")
      }
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
    // A snapshot's retention, typed apart from the quota (§4.2 `RetentionBudgetExceeded`; until 2026-11-18 `NoSpace`).
    VfsError::RetentionExceeded => volume(VolumeRefusal::RetentionExceeded),
    VfsError::RecoveryIncomplete | VfsError::Integrity => Refusal::ContentUnavailable,
    // The wire's taxonomy names one kind for a name the volume will not take, whatever the reason.
    VfsError::InvalidName | VfsError::NameTooLong => Refusal::InvalidName,
    VfsError::Destroying => Refusal::Destroying,
    VfsError::Archived => Refusal::Archived,
    VfsError::PolicyMismatch => Refusal::PolicyMismatch,
    // A read-only attachment's or a snapshot view's change, on the wire as the read-only volume it is.
    VfsError::ReadOnly => Refusal::ReadOnlyVolume,
    VfsError::BaseUnavailable(errno) => Refusal::BaseUnavailable {
      path: String::new(),
      errno: *errno,
    },
    VfsError::Memory(MemError::BudgetExceeded { available, .. }) => Refusal::BudgetExceeded {
      available: *available,
    },
    // The OS would not lock a strict volume's block (its locked-memory limit fell below what was admitted, or another
    // process took the wire limit): typed as the lock-capacity refusal, with what was locked when it refused.
    VfsError::Memory(MemError::LockRefused { locked, .. }) => Refusal::BudgetExceeded {
      available: u64::try_from(*locked).unwrap_or(u64::MAX),
    },
    VfsError::DigestNotClean => Refusal::DigestNotClean,
    VfsError::DigestUnverified => Refusal::DigestUnverified,
    // Refused whole, retryable under the same id: the arena waits on the shard's next publication (A-64).
    VfsError::PublishNeeded => Refusal::Unpublished {
      reason: "the arena's freed blocks wait on the shard's next recovery publication".to_owned(),
    },
    // Retryable under the same id: the file is delegated to an NFSv4 client, whose recall was asked for (A-79).
    VfsError::Delegated => Refusal::Unpublished {
      reason: "the file is delegated to an NFSv4 client; its recall is under way".to_owned(),
    },
    // Retryable under the same id: an S3 base's fetch of what the request reads is under way (A-524).
    VfsError::BasePending(_) => Refusal::Unpublished {
      reason: "the overlay's base is fetching what the request reads".to_owned(),
    },
    VfsError::TreeTooDeep { limit } => Refusal::Unsupported {
      feature: format!("archiving a tree deeper than {limit} path components"),
    },
    VfsError::BarrierIncomplete {
      attachment,
      generation,
    } => Refusal::BarrierIncomplete {
      attachment: *attachment,
      generation: *generation,
    },
    // Every other refusal of the volume core, typed by its class (A-299): until 2026-11-18 each reached a client as
    // `BadRequest` carrying its name. No catch-all: a new volume-core refusal does not compile until it is named.
    VfsError::Memory(_) => volume(VolumeRefusal::Memory),
    VfsError::NotDirectory => volume(VolumeRefusal::NotDirectory),
    VfsError::IsDirectory => volume(VolumeRefusal::IsDirectory),
    VfsError::NotEmpty => volume(VolumeRefusal::NotEmpty),
    VfsError::Invalid => volume(VolumeRefusal::Invalid),
    VfsError::NoAttribute => volume(VolumeRefusal::NoAttribute),
    VfsError::NotPermitted => volume(VolumeRefusal::NotPermitted),
    VfsError::LinkProtected => volume(VolumeRefusal::LinkProtected),
    VfsError::SpecialFileOperation => volume(VolumeRefusal::SpecialFileOperation),
    VfsError::TooManyLinks => volume(VolumeRefusal::TooManyLinks),
    VfsError::FileTooLarge => volume(VolumeRefusal::FileTooLarge),
    VfsError::CrossVolumeMove => volume(VolumeRefusal::CrossVolumeMove),
    VfsError::StaleHandle => volume(VolumeRefusal::StaleHandle),
    VfsError::Pinned => volume(VolumeRefusal::Pinned),
    VfsError::DaxMapped => volume(VolumeRefusal::DaxMapped),
    VfsError::BaseDrift => volume(VolumeRefusal::BaseDrift),
    VfsError::NotOverlay => volume(VolumeRefusal::NotOverlay),
    VfsError::DigestCacheFull => volume(VolumeRefusal::DigestCacheFull),
    VfsError::JournalIncomplete { needed, oldest } => volume(VolumeRefusal::JournalIncomplete {
      needed: *needed,
      oldest: *oldest,
    }),
    VfsError::RecoveryVersion { found, expected } => volume(VolumeRefusal::RecoveryVersion {
      found: *found,
      expected: *expected,
    }),
  }
}

/// A volume-core refusal no other variant names, typed (A-299).
fn volume(refusal: VolumeRefusal) -> Refusal {
  Refusal::Volume { refusal }
}

/// The wire refusal for a memory refusal met outside the volume core: a reservation's at admission (bytes, version
/// slots, the metadata ledger) or a registry's (§4.2). The volume core's own table types it, as it types
/// `VfsError::Memory`: the budget's refusal names what is available, the lock's what it locked, and every other is the
/// typed memory refusal. Until 2026-11-18 eleven admission arms and a full registry answered in words, `BadRequest`
/// carrying the error's text (A-299's sibling sweep).
pub fn refusal_of_mem(e: &MemError) -> Refusal {
  refusal_of_vfs(&VfsError::Memory(e.clone()))
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
    // Typed (A-238): a full table is the partition's bound, not a malformed request.
    DbError::Capacity { table } => Refusal::TableAtCapacity {
      table: (*table).to_owned(),
    },
    // Nothing of the verb is durable — the transaction was rolled back (`Unpublished`), or a record
    // could not be appended for want of log space a snapshot did not release (`LogFull` reaching
    // here), or the segment refused the publication outright (`Anchor`). A durability failure is not
    // a malformed request: it is typed as such so a client retries under the same id rather than
    // treating its request as wrong.
    unpublished @ (DbError::Unpublished { .. } | DbError::LogFull { .. } | DbError::Anchor(_)) => {
      Refusal::Unpublished {
        reason: format!("{unpublished}"),
      }
    }
    DbError::Memory(e) => refusal_of_mem(e),
    // Every other database refusal, typed by its class (A-299): until 2026-11-18 each reached a client as `BadRequest`
    // carrying its words. No catch-all: a new database refusal does not compile until it is named.
    DbError::Corrupt { seq, .. } => database(DatabaseRefusal::Corrupt { seq: *seq }),
    DbError::NfsInstanceRegressed { current } => {
      database(DatabaseRefusal::NfsInstanceRegressed { current: *current })
    }
    DbError::CheckpointOutOfRange {
      version,
      held,
      head,
    } => database(DatabaseRefusal::CheckpointOutOfRange {
      version: *version,
      held: *held,
      head: *head,
    }),
    DbError::SealKeyMalformed { len } => database(DatabaseRefusal::SealKeyMalformed {
      len: u64::try_from(*len).unwrap_or(u64::MAX),
    }),
    DbError::Wire(_) => database(DatabaseRefusal::Encoding),
    // A moved volume's release refused while it holds attachment records (A-291): the move's own refusal of an
    // attached volume, since a writer must detach before the volume leaves.
    DbError::Attached { .. } => Refusal::MoveRefused {
      reason: slates_ipc::protocol::MoveRefusal::Attached,
    },
    // A repository's namespace binds greens only, under non-empty names without NUL bytes (§4.17.1, A-515).
    DbError::NotGreen => Refusal::NotGreen,
    DbError::InvalidName => Refusal::InvalidName,
  }
}

/// A database refusal no other variant names, typed (A-299).
fn database(refusal: DatabaseRefusal) -> Refusal {
  Refusal::Database { refusal }
}

/// The wire refusal for a target the verb cannot use (§4.15 step 4, §4.6), typed (A-299): until 2026-11-18 each was
/// `TargetUnavailable` carrying words.
pub fn target(refusal: TargetRefusal) -> Refusal {
  Refusal::Target { refusal }
}

/// A host's refusal of a target path (a landing's directory, a mount point), typed.
pub fn target_of_host(e: &slates_vfs::host::HostError) -> TargetRefusal {
  use slates_vfs::host::HostError as H;
  match e {
    H::NotFound => TargetRefusal::Missing,
    H::NotDirectory => TargetRefusal::NotDirectory,
    H::NotFile => TargetRefusal::NotFile,
    H::StaleHandle => TargetRefusal::Stale,
    H::Unavailable(errno) | H::OwnMount(errno) => TargetRefusal::Host { errno: *errno },
    // The reply's one number field holds a Win32 code as its own (A-527; typing it is the wire's own piece, GAPS).
    H::Win32(code) => TargetRefusal::Host {
      errno: i32::try_from(*code).unwrap_or(i32::MAX),
    },
    // A target is a directory host's, which never answers a pending fetch (an S3 base's, A-524); were one to, it is a
    // refusal no errno names.
    H::Pending(_) => TargetRefusal::Host { errno: 0 },
  }
}

/// Why a mount was not recorded as this daemon's own (A-227), typed.
#[cfg(unix)]
pub fn target_of_unrecorded(why: slates_base::own_mounts::Unrecorded) -> TargetRefusal {
  use slates_base::own_mounts::Unrecorded;
  match why {
    Unrecorded::NotMounted => TargetRefusal::NotMounted,
    Unrecorded::Full(full) => records_full(full),
  }
}

/// The refusal of a mount past the most this daemon records as its own (A-227).
pub fn records_full(full: slates_base::own_mounts::Full) -> TargetRefusal {
  TargetRefusal::RecordsFull {
    capacity: u64::try_from(full.capacity).unwrap_or(u64::MAX),
  }
}

#[cfg(test)]
mod tests {
  use slates_ipc::protocol::{Refusal, VolumeRefusal};
  use slates_mem::MemError;

  use super::{refusal_of_db, refusal_of_mem};
  use slates_db::DbError;
  use slates_ipc::protocol::DatabaseRefusal;

  /// A-299's sibling sweep (2026-11-18): do map a memory refusal of each shape admission meets (the budget's, the
  /// lock's, a full slab or registry, an exhausted arena, an OS refusal). Expect each typed: the budget's with what is
  /// available, the lock's with what it locked, the rest the volume core's memory refusal; none in words.
  #[test]
  fn every_memory_refusal_at_admission_is_typed() {
    assert_eq!(
      refusal_of_mem(&MemError::BudgetExceeded {
        requested: 6,
        available: 7
      }),
      Refusal::BudgetExceeded { available: 7 }
    );
    assert_eq!(
      refusal_of_mem(&MemError::LockRefused {
        requested: 8,
        locked: 9,
        code: None
      }),
      Refusal::BudgetExceeded { available: 9 }
    );
    for memory in [
      MemError::SlabFull { capacity: 3 },
      MemError::ArenaExhausted {
        requested: 4,
        largest_free: 5,
      },
      MemError::OsRefused {
        call: "mmap",
        code: None,
      },
    ] {
      assert_eq!(
        refusal_of_mem(&memory),
        Refusal::Volume {
          refusal: VolumeRefusal::Memory
        },
        "{memory:?}"
      );
    }
  }

  /// A-299's sibling sweep (2026-11-18): do map each database refusal `refusal_of_db` answered in words before (a
  /// corrupt record, a regressed NFSv4 instance, a checkpoint out of range, a malformed sealing key, a refused
  /// encoding). Expect each typed as `Refusal::Database` with its fields, none `BadRequest`.
  #[test]
  fn every_database_refusal_is_typed() {
    let cases = [
      (
        DbError::Corrupt {
          seq: 3,
          reason: "checksum",
        },
        DatabaseRefusal::Corrupt { seq: 3 },
      ),
      (
        DbError::NfsInstanceRegressed { current: 4 },
        DatabaseRefusal::NfsInstanceRegressed { current: 4 },
      ),
      (
        DbError::CheckpointOutOfRange {
          version: 5,
          held: 6,
          head: 7,
        },
        DatabaseRefusal::CheckpointOutOfRange {
          version: 5,
          held: 6,
          head: 7,
        },
      ),
      (
        DbError::SealKeyMalformed { len: 8 },
        DatabaseRefusal::SealKeyMalformed { len: 8 },
      ),
      (
        DbError::Wire(slates_wire::error::WireError::Truncated { needed: 9 }),
        DatabaseRefusal::Encoding,
      ),
    ];
    for (error, typed) in cases {
      assert_eq!(
        refusal_of_db(&error),
        Refusal::Database { refusal: typed },
        "{error:?}"
      );
    }
  }
}
