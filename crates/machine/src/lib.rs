//! `slates-machine` — boot calibration: the machine profile every tunable is derived from
//! (design §4.1, D-11).
//!
//! slates carries no tuning literal. It carries a formula and an anchor: the daemon measures the
//! machine once at boot (page sizes, cache line, cores and their classes, memory and lock
//! capacity, fault cost per page class, syscall cost, park/unpark latency, core-to-core ring
//! round trip, memcpy and hash and codec throughput), derives every constant from those
//! measurements by a stated formula, logs each derived value with its inputs, and re-measures the
//! cheap subset when the machine's power state changes. hyperscale's ~140 hard-coded constants
//! were its weakest point [C: survey-hyperscale.md §8.4]; page sizes differ 4x across our targets
//! and fault costs 100x between page classes [A: Panwar ASPLOS'19], so nothing here is assumed.
//!
//! Measurement follows lmbench's methodology and Kalibera and Jones's stopping rule [A: McVoy
//! USENIX'96; A: Kalibera & Jones ISMM'13]: repeat until the bootstrapped interval around the
//! median is narrow, report the interval, never a bare number, and stop at a wall-time bound so a
//! slow machine still boots (the bound makes the profile "quick", which the profile says).
//!
//! Modules:
//! - [`derived`] — the [`Derived`] value type and the `derived!` macro: a value with its formula
//!   and anchors, which is what "no magic numbers" means in code.
//! - [`stats`] — medians, percentiles by integer rationals, bootstrap intervals with a seeded
//!   generator, and the stopping rule.
//! - [`bench`] — the measurement harness with batching for sub-timer-resolution operations.
//! - [`facts`] — the fixed facts queried from the OS (pages, cache line, cores, memory, power).
//! - [`probes`] — the microbenchmarks.
//! - [`profile`] — the [`MachineProfile`] assembled from facts and probes, its JSON export with
//!   derivations, and the derived-constants table of §4.1.
//! - [`segment`] — the RAM-only cache of the profile keyed by host identity (a memory object that
//!   creates no filesystem entry), which the anchor process owns from Phase 2.
//!
//! This crate reads the kernel's pseudo-files (`/proc`, `/sys`) as queries on Linux; it never
//! writes a host path (the structural test lists it under R1's allowed sites for that reason).

pub mod bench;
pub mod derived;
pub mod error;
pub mod facts;
pub mod probes;
pub mod profile;
pub mod segment;
pub mod stats;

pub use derived::Derived;
pub use error::MachineError;
pub use profile::{DerivedConstants, MachineProfile, ProfileOptions};
