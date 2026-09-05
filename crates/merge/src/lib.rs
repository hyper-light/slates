//! `slates-merge`: the merge engine (D-27, §4.16; Phase 6). Many agents work on clones of one
//! shared "green" volume; each agent's work is an increment of *declared* operations (not a
//! diff of file states), and the green's merge task maps the increment through everything
//! accepted since the agent's base version and decides a pure verdict: accept, accept-identical,
//! or a byte-exact conflict. There is no inferred merge anywhere — the verdict is arithmetic on
//! declared ranges, never a reconstruction of what changed by comparing bytes (the design's
//! never-diff clause).
//!
//! This module is the verdict — the pure core (§4.16 "The verdict, two pure passes"). Pass one
//! is a sweep line over the increment's ranges and the intervening deltas' effect ranges, per
//! path: disjoint ranges accept, same-range candidates are handed to pass two, everything else
//! is a conflict with its class. Pass two is a memcmp of the same-range candidates' bytes:
//! equal bytes accept-identical, unequal conflict. Neither pass does I/O, reads a clock, draws
//! randomness, or (in its hot comparison) allocates. The content deriver ([`derive`]) composes a
//! path's declared operations into the canonical net op set by interval algebra (never a diff),
//! and the ops document ([`ops_doc`]) serializes an increment's ops into the bytes whose BLAKE3
//! is half its identity; the position mapping, the splice, the chain and the fleet commit are
//! the engine's other pieces, built on these.
//!
//! The fast path: when every path the increment touches was last changed at or before the
//! increment's base version, the verdict is `Accept` with no range work — what a fresh basis
//! buys, and it never decides a conflict.

pub mod derive;
pub mod ops_doc;
pub mod range;
pub mod verdict;

pub use derive::{ContentOp, compose_content};
pub use ops_doc::{Op, OpKind, OpsDoc, PathTable};
pub use range::{Range, RangeSet};
pub use verdict::{
  MergeConflictClass, PathVerdict, Verdict, compare_bytes, fast_path, path_verdict,
};
