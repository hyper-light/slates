//! `slates-anchor` — the anchor process's library (design §2.5, §2.6, §4.8 "Recovery", D-18;
//! Phase 2 task 1). The anchor is the tiny process that outlives the daemon: it owns one shared
//! memory segment (created without a filesystem entry, `slates_mem::SharedObject`) holding the
//! machine profile, the per-partition operation logs, the catalog snapshots, the audit log and
//! the manifests of landings in flight, and it supervises the daemon: starts it with the
//! segment handed over, observes its heartbeat, restarts it when it exits, and stops restarting
//! when the daemon fails faster than it can recover (a typed `CrashLoop`, never a busy loop).
//!
//! What lives where (`layout`): a header with the magic, the layout version, the machine
//! identity, a generation word and the geometry; a supervision block the anchor and the daemon
//! share through atomic words (pid, heartbeat, generation, restarts, state); the profile as a
//! seqlock-published JSON payload; one log region per partition (head, tail, capacity words and
//! a byte ring; the daemon is its single writer, the record format is the database crate's);
//! two snapshot slots per partition (alternating, generation-tagged, so a crash inside one
//! leaves the other whole); the audit ring; and the landing slots. Every size is the daemon's
//! derivation, persisted in the header so a restarted daemon attaches with the same geometry.
//!
//! Invariants: the anchor never reads a log or a snapshot (it holds them); the daemon never
//! resizes the segment (a new geometry is a new segment, which the anchor creates only when no
//! daemon is attached); every word two processes touch is read and written through an atomic
//! view; a torn header or profile is refused, never served (the seqlock rule of §4.1).

pub mod error;
pub mod layout;
pub mod segment;
pub mod supervise;

pub use error::AnchorError;
pub use layout::{Geometry, RegionKind, RegionSpec};
pub use segment::{AnchorSegment, Supervision};
pub use supervise::{RestartPolicy, Step, Supervisor};
