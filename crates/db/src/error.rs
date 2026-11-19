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
  /// A volume released to the member it moved to (A-291) still has attachment records: it leaves only whole, so none
  /// is left naming a volume this node no longer holds. Never entered in the log.
  Attached {
    /// How many attachment records it has.
    attachments: u32,
  },
  /// An NFSv4 instance advance named an instance at or below the current one (§4.6 A-37): ids minted
  /// under it could repeat ids an earlier life minted.
  NfsInstanceRegressed {
    /// The partition's current instance.
    current: u32,
  },
  /// A green's checkpoint (W3; §4.16) at a version it cannot stand for: not past the checkpoint the green holds, or
  /// past the green's head (its checkpoint's version and the increments held after it). Never entered in the log.
  CheckpointOutOfRange {
    /// The version the checkpoint named.
    version: u64,
    /// The version of the checkpoint the green holds (0: none).
    held: u64,
    /// The green's head.
    head: u64,
  },
  /// A sealing key record of other than hyper-seal's fixed length (A-92): never entered in the log.
  SealKeyMalformed {
    /// The record's length.
    len: usize,
  },
  /// A transaction could not be made durable — its record was not appended and the snapshot that
  /// would have carried its effects was not published — so the partition was **rolled back** to its
  /// durable state: the applied effects and the completion record are gone, exactly as a restart
  /// would leave them (§4.8 transactions, AC-2.3; AUD-06). A retry of the same request re-executes
  /// it; it is never answered from memory. `cause` is the refusal that stopped the publication.
  Unpublished {
    /// The sequence the record would have taken.
    seq: u64,
    /// What stopped the publication.
    cause: Box<DbError>,
  },
  /// The encoding refused.
  Wire(WireError),
  /// The segment refused.
  Anchor(AnchorError),
  /// The memory crate refused.
  Memory(MemError),
  /// A repository's namespace names greens only (§4.17.1, A-515): the volume named is not one.
  NotGreen,
  /// A repository's or a branch's name is empty or holds a NUL byte (§4.17.1, A-515).
  InvalidName,
  /// A review record's change out of its order (§4.17.1 shared piece 6, A-596): an outdated comment moved or outdated
  /// again, a comment or approval changed at a version not after the one it stands on, an approval bound back to an
  /// earlier version, or a new record that is already outdated or stale. Never entered in the log.
  ReviewOutOfOrder,
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
      Self::Attached { attachments } => {
        write!(f, "the volume still has {attachments} attachment records")
      }
      Self::NfsInstanceRegressed { current } => {
        write!(f, "NFSv4 instance must advance past {current}")
      }
      Self::CheckpointOutOfRange {
        version,
        held,
        head,
      } => write!(
        f,
        "a green checkpoint at version {version}, outside ({held}, {head}]"
      ),
      Self::SealKeyMalformed { len } => write!(f, "a sealing key record of {len} bytes"),
      Self::Unpublished { seq, cause } => {
        write!(
          f,
          "record {seq} not published, transaction rolled back: {cause}"
        )
      }
      Self::Wire(e) => write!(f, "encoding: {e}"),
      Self::Anchor(e) => write!(f, "segment: {e}"),
      Self::Memory(e) => write!(f, "memory: {e}"),
      Self::NotGreen => f.write_str("not a green volume"),
      Self::InvalidName => f.write_str("an empty name, or one holding a NUL byte"),
      Self::ReviewOutOfOrder => f.write_str("a review record changed out of its order"),
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
