//! `slates-store` — the version-control store (A-767): one unit, the changeset. A change in flight is held in RAM as it
//! arrived; once the verdict accepts it, it lands as one record appended to its repository's changeset log, an
//! append-only file in the store's directory under the repository's standing grant (`slates_land::store_root`), made
//! durable by one flush of the log before it is acknowledged. Everything else (the head tree, each tool's objects and
//! wire forms, blame, checkpoints of full texts) is derived from the log and cached by identity; the rows that index it
//! live in mantle's engine, vendored (`vendor/mantle`).
//!
//! [`log`] is the changeset log: its record format, a record appended in slices and visible only once its commit mark
//! is durable, and recovery to the last committed record.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod log;
