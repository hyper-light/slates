//! A-837 part 3 (the descriptor backstop; Ada 2026-11-21, "a way to avoid descriptor exhaustion"): a daemon out of
//! descriptors refuses a host open typed, counts it, and keeps serving. The test re-invokes its own binary as the
//! process that runs out, so its limit is capped there alone: the daemon runs in that process, both its descriptor
//! limits capped, and the process's own descriptors are spent before a base read.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::time::Duration;

use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, NamePolicy, ReadAt, Refusal, SizeClass,
};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the variable that makes the re-invoked binary the process that runs out.
const ROLE: &str = "SLATES_DESCRIPTOR_EXHAUSTION_ROLE";

/// Format: the tag the role's report carries, found anywhere in a line (libtest's banner precedes it).
const TAG: &str = "A837-EXHAUSTION";

/// Shape: both descriptor limits in the role, far over what a one-shard daemon and its client hold at rest and far
/// under any host's own, so spending the rest takes a moment.
const CAPPED: u64 = 512;

/// Shape: the probe budget of the quick profile (milliseconds), as the client tests use.
const PROBE_MS: u64 = 5;

/// The base file the role reads: this crate's own source, read only.
const BASE_FILE: &str = "/lib.rs";

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

/// The role: caps both limits, starts a one-shard daemon over this crate's `src/` as an overlay's base, spends every
/// descriptor left, then reads a base file (refused typed), asks a verb that opens nothing (answered), gives the
/// descriptors back and reads again (answered); prints what it saw, tagged.
fn run_out() {
  use rustix::process::{Resource, Rlimit, setrlimit};
  setrlimit(
    Resource::Nofile,
    Rlimit {
      current: Some(CAPPED),
      maximum: Some(CAPPED),
    },
  )
  .unwrap();
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .unwrap();
  let instance = format!("a837-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(1)),
    SegmentSource::Create {
      name: format!("slates-seg-a837-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  let base = format!("{}/src", env!("CARGO_MANIFEST_DIR"));
  let volume = client
    .create(&CreateSpec {
      name: "over".to_owned(),
      size: SizeClass::Dynamic { max: 1 << 24 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: Some(base),
    })
    .unwrap();
  // Every descriptor left, spent; held until the reads below are done.
  let mut spent = Vec::new();
  while let Ok(fd) = rustix::fs::open(
    c"/dev/null",
    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
    rustix::fs::Mode::empty(),
  ) {
    spent.push(fd);
  }
  let exhausted = client.read(volume, BASE_FILE, ReadAt::Head);
  let typed = matches!(
    exhausted,
    Err(ClientError::Refused(Refusal::DescriptorsExhausted { .. }))
  );
  let serving = client.list().is_ok();
  drop(spent);
  let again = client
    .read(volume, BASE_FILE, ReadAt::Head)
    .map(|bytes| bytes.len());
  let counted = daemon
    .refusals_on_every_shard()
    .unwrap()
    .get("descriptors_exhausted")
    .copied()
    .unwrap_or(0);
  println!(
    "{TAG} typed={typed} serving={serving} counted={counted} again={again:?} first={exhausted:?}",
    exhausted = exhausted.map(|bytes| bytes.len())
  );
  daemon.stop();
}

/// A-837 part 3. Do: in a re-invoked process with both descriptor limits at 512, start a one-shard daemon with an
/// overlay over this crate's `src/`, spend every descriptor the process has left, and read the base's `lib.rs`; then
/// list the volumes; then give the descriptors back and read again. Expect: the read refused `DescriptorsExhausted`
/// (never an untyped `BaseUnavailable`, never a panic), the refusal counted (`descriptors_exhausted`), the list
/// answered while the process is still out, and the read answered once descriptors are free.
#[test]
fn a837_a_daemon_out_of_descriptors_refuses_a_base_open_typed_and_keeps_serving() {
  if std::env::var_os(ROLE).is_some() {
    run_out();
    return;
  }
  let output = std::process::Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "a837_a_daemon_out_of_descriptors_refuses_a_base_open_typed_and_keeps_serving",
      "--nocapture",
    ])
    .env(ROLE, "1")
    .output()
    .unwrap();
  let stdout = String::from_utf8_lossy(&output.stdout);
  let report = stdout
    .lines()
    .find_map(|line| line.split_once(TAG).map(|(_, rest)| rest.to_owned()))
    .unwrap_or_else(|| {
      panic!(
        "the role reported nothing: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
      )
    });
  assert!(report.contains("typed=true"), "{report}");
  assert!(report.contains("serving=true"), "{report}");
  assert!(!report.contains("counted=0"), "{report}");
  assert!(report.contains("again=Ok("), "{report}");
}
