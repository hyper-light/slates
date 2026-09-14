//! `slates-conformance`: the evidence surface for AC-9.7/T-9.1 and GAP-A9-15 (design §4.6 "OS
//! bridges", Part 6 "Conformance" and "Real workloads", Part 6 example 8 "Hermeticity";
//! `docs/wip/EQUIVALENCE.md` §7–§8). The design promises, per offered transport, that the
//! POSIX conformance suites (pjdfstest, fsx, fsstress) pass over a mount with a reviewed
//! expected-failure list that only shrinks, that real workloads run inside the mount
//! byte-identical to the host, and that a filesystem-write tracer sees zero writes by slates
//! processes outside a granted landing target. GAP-A9-15 records that those promises have no
//! transport-specific evidence and that "historical stages overstate coverage".
//!
//! This crate is the pure half of the answer. It holds the typed **run record** one harness run
//! produces for one (transport × suite) cell ([`record`]), the **evidence matrix** rendered from
//! a set of records ([`matrix`]) that `docs/wip/conformance.md` carries as a doc-truth block (a
//! test compares the tracked records with the document; `--ignored regenerate` rewrites it), the
//! **reviewed expected-failure list** and the rule that it only shrinks ([`expected`]), the
//! **capability table** that says on which host each cell can run at all ([`capability`]), and
//! the parsers of the suites' outputs: pjdfstest's TAP ([`tap`]), fsx and fsstress ([`exerciser`]),
//! strace and fs_usage logs with the containment judgement ([`trace`]), and the workload roster
//! with its byte-identity comparison ([`workload`]).
//!
//! The invariants the types hold: a cell without a run is never rendered as anything but `OWED`
//! or `SKIPPED(reason)` — there is no way to write a `RAN` cell without counts, a date, a host and
//! the exact command; a `LIMITED` cell must name its adapter and what the adapter does not cover;
//! a parser of external bytes checks length against its cap before it allocates and never
//! panics on any input (Part 6 "Hostile input"); and the containment judgement is a closed
//! taxonomy — inside the granted target, a RAM-only kernel object, the process's own standard
//! streams, or a violation — so an unclassified write is a violation, never silence.
//!
//! No I/O lives here (the crate is under the R1 lint wall with no allowance): mounting, running
//! the suite binaries, tracing and writing record files are `cargo xtask conformance`'s, a
//! development tool. Evidence for the shape: pjdfstest's TAP is the suite's own protocol
//! (`tests/misc.sh` prints `ok N` / `not ok N - tried ...`), fsx's success line is its source's
//! (`All operations completed A-OK!`), the fs_usage row layout is Apple's `fs_usage.c`
//! (`print_open`, `format_print`; evidence C), and strace's `-y` descriptor decoration is the
//! `strace(1)` manual's (evidence B).

pub mod capability;
pub mod civil;
pub mod exerciser;
pub mod expected;
pub mod matrix;
pub mod record;
pub mod tap;
pub mod trace;
pub mod workload;

pub use record::{Counts, Host, Outcome, Privilege, Record, SkipReason, Suite, Transport};
