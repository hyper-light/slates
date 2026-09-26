//! Test fixtures shared by the daemon's integration tests. Each integration test is its own crate and
//! compiles this module afresh, so a helper one test does not use is dead code there — allowed here.
#![allow(dead_code)]

pub(crate) mod nfs;
pub(crate) mod target;
pub(crate) mod trace;
pub(crate) mod wait;

use std::sync::OnceLock;
use std::time::Duration;

use slates_machine::{MachineError, MachineProfile, ProfileOptions};

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds). The profile is
/// measured once per test process ([`machine_profile`]), so the budget is paid once.
const PROBE_MS: u64 = 5;

/// The machine profile a test binary's daemons derive from, measured once per process (§4.1).
/// Production measures the machine once, at the anchor's boot, and derives every configuration from
/// that one profile; the fixture keeps that shape. A helper that measured afresh for each daemon ran
/// the wake probe beside the other tests' probes and daemons, where it measured their load rather than
/// the machine, or measured nothing and was refused `MeasurementTimeout`
/// (`docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`).
pub(crate) fn machine_profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure(ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures")
}
