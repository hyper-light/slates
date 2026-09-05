//! `slates-land` — the landing engine (design §4.15, D-26; Phase 1 tasks 11–13): the only
//! code in the workspace that writes a host path, and only under a grant a human issued for
//! the exact manifest it writes.
//!
//! The pieces: [`manifest`] plans what would be written from a volume's diverged entries and
//! hashes the plan canonically; [`verdict`] is the pure per-entry decision of §4.15's table
//! (apply, skip, accept by identity, conflict); [`grant`] holds grants and the single-holder
//! landing lease as in-process records (database records from Phase 2); [`ramp`] is the online
//! concurrency policy; [`engine`] runs the state machine (plan, present, grant, lease and
//! validate, write by class, sync, advance, report) over the write seam of the volume crate,
//! with crash resume and the stage-and-exchange alternative; [`os`] implements that seam over
//! the operating system. Everything but [`os`] runs against the simulated host, which is how
//! the landing oracle exercises outsider edits and crashes at every write instruction.
//!
//! Invariants (D-26): nothing is written before the grant that names the manifest's hash and
//! the lease on the target; nothing is written while any entry's verdict is a conflict; every
//! written entry is old or new, never torn (a temporary is linked or exchanged only after its
//! data sync); a compare-and-swap lost to an outsider is undone and reported; a re-run is
//! idempotent by hash; a directory rename is one rename; the work is proportional to the delta.

pub mod engine;
pub mod grant;
pub mod manifest;
pub mod ramp;
pub mod verdict;

#[cfg(unix)]
pub mod os;
