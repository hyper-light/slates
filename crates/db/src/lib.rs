//! `slates-db` — the metadata database (design §4.8, D-14, D-15, D-16; Phase 2 task 2): one
//! partition per shard holding the volume catalog, snapshots, lineage, leases with their
//! expiry wheel, attachments, the completion records of exactly-once, grants, landing leases,
//! landing records and the audit log; every mutation an operation applied to the partition and
//! appended to the partition's log ring in the anchor segment; recovery as the newest valid
//! snapshot of the partition plus the records after it.
//!
//! The one invariant everything here serves: the partition in memory equals the partition
//! replayed from the segment after any crash at any instruction (AC-2.3). It holds because a
//! mutation is applied and appended as one step by the partition's single writer, a record is
//! published (its tail word released) only after its bytes and checksum are in place, a reply
//! to a client is sent only after that publish, and recovery stops at the first record that
//! does not verify, which can only be the one being written when the crash came. A snapshot
//! is published into the alternate slot under the seqlock rule and the log's head advances past
//! the snapshot's sequence only after the publish, so a crash inside a snapshot leaves the
//! other slot and the log intact.
//!
//! Modules: [`art`] (the adaptive radix tree of the indexes), [`catalog`] (the records),
//! [`op`] (the operations), [`partition`] (the state and its deterministic `apply`),
//! [`record`] (the log record format over the segment's ring), [`register`] (the register
//! primitives of §4.8 — the quorum, the fence, rendezvous placement, the configuration oracle),
//! [`ledger`] (the fenced ledger register: the register over time, commit at `f + 1`, and takeover
//! by phase-one adoption), [`mirror`] (asynchronous mirroring of the committed prefix to a second
//! region in epoch order with an exposed lag), [`reconfig`] (changing a register's holder set by
//! joint consensus, preserving ReadSafety and NoLoss), [`replay`] (recovery and the snapshot
//! policy), [`error`].

pub mod art;
pub mod catalog;
pub mod error;
pub mod ledger;
pub mod mirror;
pub mod op;
pub mod partition;
pub mod reconfig;
pub mod record;
pub mod register;
pub mod replay;

pub use art::Art;
pub use error::DbError;
pub use ledger::{Cohort, Commit, Owner, Reach, Record, TakeoverError};
pub use mirror::Mirror;
pub use op::Op;
pub use partition::Partition;
pub use reconfig::{Phase, Reconfiguration, RetireError};
pub use register::{Configuration, DurabilityScope, Fence, HostEpoch, HostId, Placement, Quorum};
// The fleet ship-and-collect types (`register::Record`, `register::Holder`,
// `register::commit_over_holders`) are reached through the `register` module path — `Record` is not
// re-exported at the crate root because the replay log has a `Record` of its own.
pub use replay::{Db, Recovered, SnapshotPolicy};
