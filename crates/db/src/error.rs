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
  /// A green that live members of its lineage were made from cannot be destroyed while they live: each would lose the
  /// version it was made at, which its rebuild branches from again (§4.17.1 item 4, A-575). Never entered in the log.
  BranchedFrom {
    /// The live members made from it.
    members: u64,
  },
  /// An entry of a repository's register made for a position that is not the register's next, or naming a version of
  /// a green that is not the one after its head (§4.17.1 shared piece 2, A-603): it was made against another state of
  /// the register than the one it would follow. Never entered in the log.
  OutOfOrder {
    /// The position or version the entry named.
    named: u64,
    /// The one the register holds next.
    expected: u64,
  },
  /// An entry's check does not hold at the entry's register position (A-603): what it checks moved since it was read.
  /// Never entered in the log.
  CheckFailed {
    /// The check's index among the entry's checks.
    index: u32,
  },
  /// An entry's change of its repository's own state was made against another state (A-603): a phase root it adds is
  /// one already, one it removes is not, or a marker it adds is held. Never entered in the log.
  StateChanged {
    /// The change's index among the entry's state changes.
    index: u32,
  },
  /// A review record's change the review rules refuse (§4.17.1 shared piece 6, A-596), by the rule
  /// (`crate::review`). Never entered in the log.
  Review(ReviewRefusal),
  /// A repository policy's setting named a version its policy has moved past, or a repository created already holding a
  /// policy, which only its own setting gives (A-622). Read the policy again and set it from there. Never entered in the
  /// log.
  PolicyChanged {
    /// The policy's version now.
    current: u64,
  },
  /// A repository policy's rule the log must never record (A-622): its pattern names what the repository's tool does
  /// not have, is empty, holds a NUL, or is a Subversion path not from the root. Never entered in the log.
  PolicyRuleMalformed {
    /// The rule's index among the policy's rules.
    index: u32,
  },
  /// A repository policy's hook the log must never record (A-622): at another tool's point, named other than its point
  /// allows or named twice there, or running a program that is not an absolute path or holds a NUL. Never entered in
  /// the log.
  PolicyHookMalformed {
    /// The hook's index among the policy's hooks.
    index: u32,
  },
  /// A repository policy's hooks-env section the log must never record (A-648): in a repository that is not
  /// Subversion's, named by nothing or holding a NUL, or with a variable that is not one variable of a hook's
  /// environment (a name empty or holding `=` or a NUL, a value holding a NUL). Never entered in the log.
  PolicyHooksEnvMalformed {
    /// The section's index among the policy's hooks-env sections.
    index: u32,
  },
}

/// The rule a review record's change broke (§4.17.1 "Works over works", shared piece 6; A-596; `crate::review`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewRefusal {
  /// The comment is outdated: it never moves, and is never outdated again.
  Outdated,
  /// The approval is stale already.
  Stale,
  /// The change names a version not after the one the record stands on: a record moves forward only.
  NotAfter,
  /// The approval names a version before its approver's standing one, or before the change that made it stale: an
  /// approval binds to the version its approver saw, never back.
  Behind,
  /// A new record that is already outdated or stale.
  Ended,
  /// A comment's lines span no bytes: an empty range, or one whose end is past the largest byte position.
  Empty,
}

impl fmt::Display for ReviewRefusal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Outdated => "the comment is outdated",
      Self::Stale => "the approval is stale",
      Self::NotAfter => "the version is not after the record's",
      Self::Behind => "the approval is behind its approver's standing one",
      Self::Ended => "a new record already outdated or stale",
      Self::Empty => "the comment's lines span no bytes",
    })
  }
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
      Self::BranchedFrom { members } => write!(
        f,
        "{members} live members of its lineage were made from this green"
      ),
      Self::Review(rule) => write!(f, "review: {rule}"),
      Self::OutOfOrder { named, expected } => write!(
        f,
        "a register entry naming {named} where the register holds {expected} next"
      ),
      Self::CheckFailed { index } => {
        write!(
          f,
          "a register entry's check {index} does not hold at its position"
        )
      }
      Self::StateChanged { index } => write!(
        f,
        "a register entry's state change {index} was made against another state"
      ),
      Self::PolicyChanged { current } => write!(
        f,
        "a repository policy set from a version it has moved past (version {current} now)"
      ),
      Self::PolicyRuleMalformed { index } => {
        write!(f, "a repository policy's rule {index} is malformed")
      }
      Self::PolicyHookMalformed { index } => {
        write!(f, "a repository policy's hook {index} is malformed")
      }
      Self::PolicyHooksEnvMalformed { index } => {
        write!(
          f,
          "a repository policy's hooks-env section {index} is malformed"
        )
      }
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
