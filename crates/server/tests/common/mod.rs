//! Test fixtures shared by the daemon's integration tests. Each integration test is its own crate and
//! compiles this module afresh, so a helper one test does not use is dead code there — allowed here.
#![allow(dead_code)]

pub(crate) mod anchor;
pub(crate) mod client;
#[cfg(unix)]
pub(crate) mod fuse_map;
#[cfg(unix)]
pub(crate) mod guest;
// The landing plane's fixtures drive the Unix landing (the daemon lands nothing on Windows yet).
#[cfg(unix)]
pub(crate) mod landing;
#[cfg(unix)]
pub(crate) mod lease;
pub(crate) mod nfs;
// The NFSv4.2 client speaks TCP alone, and the daemon serves NFS on Windows too (A-184).
pub(crate) mod nfs4;
pub(crate) mod target;
// The vhost-user front end a VMM speaks, over `guest`'s driver (Linux, where vhost-user is built).
// One shard's thread starved of its CPU while it stays runnable, beside spinners (A-228, A-232).
pub(crate) mod starve;
pub(crate) mod trace;
#[cfg(target_os = "linux")]
pub(crate) mod vhost_front_end;
pub(crate) mod wait;

use std::sync::OnceLock;
use std::time::Duration;

use slates_machine::{MachineError, MachineProfile, ProfileOptions};

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
/// This process's host, as an in-process daemon with no fleet names it (A-237): the machine identity's host id, the
/// anchor `slates_server::daemon::local_anchor` takes. A share to an account of the test's own daemon names it.
pub(crate) fn this_host() -> u64 {
  slates_server::daemon::host_id_of(&machine_profile().facts.identity)
}

pub(crate) fn machine_profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      let profile = MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })?;
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

/// The name of one fixture's shared object (its segment, `slates-seg`, or its content object, `slates-con`), unique to
/// this process and this call: `{prefix}-{name}-{pid}-{n}`. On macOS a shared-memory name is the user's, a creation first
/// unlinks any holder's, and a creator's drop unlinks the name again, so two fixtures under one name, a proptest's
/// successive cases or two processes of one binary, let the first's drop unlink the second's: a loaded run's shard then
/// found its segment gone (`shm_open` `ENOENT`, 2026-11-07). `daemon.rs`'s audit fixtures name one instance per call for
/// the same reason (2026-09-15). The platform's name limit is kept by `slates-mem`, which hashes a long name.
pub(crate) fn fixture_object(prefix: &str, name: &str) -> String {
  static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  format!(
    "{prefix}-{name}-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
  )
}
