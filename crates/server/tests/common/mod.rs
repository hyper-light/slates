//! Test fixtures shared by the daemon's integration tests. Each integration test is its own crate and
//! compiles this module afresh, so a helper one test does not use is dead code there — allowed here.
#![allow(dead_code)]

#[cfg(unix)]
pub(crate) mod anchor;
#[cfg(unix)]
pub(crate) mod guest;
// The landing plane's fixtures drive the Unix landing (the daemon lands nothing on Windows yet).
#[cfg(unix)]
pub(crate) mod landing;
#[cfg(unix)]
pub(crate) mod lease;
pub(crate) mod nfs;
#[cfg(unix)]
pub(crate) mod nfs4;
pub(crate) mod target;
pub(crate) mod trace;
pub(crate) mod wait;

use std::sync::OnceLock;
use std::time::Duration;

use slates_machine::{MachineError, MachineProfile, ProfileOptions};

/// Shape: the per-probe budgets this binary's profile is measured within — the quick bound this suite has
/// always used, then the full profile's own (`slates_machine::bench::PROBE_WALL_BUDGET`). One bound is a
/// guess about the machine, and the cheap one is wrong on a busy machine: the wake probe cannot answer in
/// five milliseconds beside the rest of a `cargo test --workspace`, which refused it twice on 2026-10-18.
/// The scope stays quick — no codecs, no core matrix — and only the patience grows.
const PROFILE_BUDGETS: [std::time::Duration; 2] = [
  Duration::from_millis(PROBE_MS),
  slates_machine::bench::PROBE_WALL_BUDGET,
];

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds). The profile is
/// measured once per test process ([`machine_profile`]), so the budget is paid once.
const PROBE_MS: u64 = 5;

/// Shape: the most shards a server test asks for by name (`TEST_SHARDS`, `POOL_SHARDS`, the fleet's `TWO_SHARDS`). The
/// key region is made for the larger of this and the profile's default, so a daemon a test starts with more shards than
/// the machine's default still finds its keys. Before 2026-10-10 the region followed the default alone, and in a 1–2
/// CPU container a 2-shard test daemon ran unsealed (128 keys held against 256 needed).
const MOST_NAMED_TEST_SHARDS: u16 = 2;

/// The machine profile a test binary's daemons derive from, measured once per process (§4.1).
/// Production measures the machine once, at the anchor's boot, and derives every configuration from
/// that one profile; the fixture keeps that shape. A helper that measured afresh for each daemon ran
/// the wake probe beside the other tests' probes and daemons, where it measured their load rather than
/// the machine, or measured nothing and was refused `MeasurementTimeout`
/// (`docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`).
///
/// It also makes the process's one key region (A-92, A-99), sized for the largest daemon a test of this binary starts
/// (the larger of the profile's default shard count and [`MOST_NAMED_TEST_SHARDS`]), before any daemon starts.
/// Production runs one daemon per process, so its region is always its own; a test process runs many, and whichever
/// started first sized the region for its own shards, so a later daemon with more shards found sealing unavailable
/// depending on test order (2026-10-05: 512 slots held against 1024 needed, one run in three of the recovery suite).
pub(crate) fn machine_profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      let profile = MachineProfile::measure_within(
        &ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      },
        &PROFILE_BUDGETS,
      )?;
      let default = slates_server::config::DaemonConfig::derive(&profile, "key-region", None);
      let shards = default.runtime.shards.max(MOST_NAMED_TEST_SHARDS);
      let largest =
        slates_server::config::DaemonConfig::derive(&profile, "key-region", Some(shards));
      // A region the OS will not lock leaves sealing unavailable in every daemon alike, which they report.
      let _ = hyper_seal::lock_keys(largest.seal_key_slots);
      Ok(profile)
    })
    .clone()
    .expect("the machine profile measures")
}
