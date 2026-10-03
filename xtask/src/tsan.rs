//! `cargo xtask tsan`: the ThreadSanitizer lane (Part 6 "Concurrency": "TSan nightly"; AUD-29-32; Ada
//! authorized the lane and the nightly schedule 2026-10-03). loom explores the cores' interleavings within a
//! bound and Miri checks their memory model one interleaving at a time; ThreadSanitizer watches the real
//! threads of whole tests — the runtime's shards, the IPC rings and doorbells between a client and a daemon,
//! the client's reply park — and reports any two accesses to one location, one a write, that nothing orders.
//!
//! The task runs in two steps, and fails if either fails:
//!
//! 1. **The canary.** `slates-mem`'s `race_canary` test races two writers on purpose. The task requires
//!    ThreadSanitizer's data-race report from it. Without that, a clean run of the suites could mean the
//!    instrumentation was missing (a toolchain or flag change), and the lane would pass vacuously.
//! 2. **The suites.** Every library and integration test of [`CRATES`], instrumented, with `--no-fail-fast`.
//!    A report makes the test binary exit non-zero (ThreadSanitizer's `exitcode`, 66 by default), so any
//!    race fails the task. Only the tests in [`SKIPPED`] are left out, each with its measured reason.
//! 3. **The server.** Its library and in-process daemon suites ([`SERVER_TARGETS`]), the same way.
//!
//! ThreadSanitizer needs the nightly toolchain (`-Zsanitizer=thread`) and the standard library rebuilt with
//! the same instrumentation (`-Zbuild-std`, so a race through std's own code is seen and std's
//! synchronization is understood). It needs an explicit target, which is the host's. Doctests are not run:
//! two compile-fail doctests in `slates-rt` fail to compile for a different reason under `-Zbuild-std`, and a
//! doctest exercises no threads (measured 2026-10-03).

use std::path::Path;
use std::process::{Command, Stdio};

use crate::Failure;

/// Shape: the crates whose tests run real threads against shared state — the memory primitives (rings,
/// the shared object), the runtime (shards, wakes, the drivers), the IPC between a client and a daemon
/// (rings, doorbells, rendezvous) and the client (reply park, reconnect). Measured clean on 2026-10-03
/// (aarch64 Linux container, nightly 2026-10-02): 68 + 100 + 38 + 14 tests, no report.
const CRATES: [&str; 4] = ["slates-mem", "slates-rt", "slates-ipc", "slates-client"];

/// Shape: the server's targets the lane runs, named rather than whole: its library and the in-process daemon
/// suites — the runtime's shards serving a whole daemon's state, recovery and attachment forms. Its other suites
/// are left to their lanes (the fleet suite must run alone; the mount suites need a kernel mount). Measured clean
/// on 2026-10-03 (aarch64 Linux container, nightly 2026-10-02): 155 + 18 + 14 + 8 tests, no report, about a
/// minute with the build cached.
const SERVER_TARGETS: [&str; 4] = [
  "--lib",
  "--test=daemon",
  "--test=recovery",
  "--test=attach_forms",
];

/// Shape: the tests the lane leaves out, each with why. A skipped test still runs in every other lane;
/// only its assertion is one the instrumentation itself falsifies.
const SKIPPED: [(&str, &str); 4] = [
  (
    "region::tests::locking_a_small_region_is_reported_by_the_os_within_one_page",
    "ThreadSanitizer ignores mlock (its runtime prints `ThreadSanitizer ignores mlock/mlockall/munlock/\
     munlockall` at verbosity 1), so the locked delta is 0; it passes uninstrumented in the same container",
  ),
  (
    "driver::tests::a_zero_timeout_wait_delivers_what_is_ready_and_never_sleeps",
    "it asserts that 256 zero-timeout waits never switch the thread out, and the instrumented runtime's own \
     locking does: the whole library failed it 3 times in 12 instrumented runs and 0 in 10 plain runs \
     (2026-10-03, 18-core container); alone it passed 20 of 20 either way",
  ),
  (
    "a_poll_busy_on_the_cpu_past_the_quantum_is_its_tasks",
    "it classifies each step by its measured CPU time against the quantum; under instrumentation one CI run \
     (37153112465, x86, 2026-10-03) judged an extra step unattributed (2, not 1). Not reproduced here (0 of 10 \
     instrumented, 0 of 10 plain); that the slower instrumented step crossed the quantum is the hypothesis",
  ),
  (
    "a_client_learns_its_wake_from_the_parks_a_reply_ended",
    "it counts parks whose wake was confirmed against a measured sleep; under instrumentation one CI run \
     (37142294661, 2026-10-03) counted 12 of 32 where it needs 16. Not reproduced here (0 of 40 across plain, \
     instrumented, alone and at 4 CPUs); that the slower instrumented client was not yet asleep is the hypothesis",
  ),
];

/// Format: the canary test target and the test it holds (`crates/mem/tests/race_canary.rs`).
const CANARY: (&str, &str) = (
  "race_canary",
  "two_unsynchronized_writers_on_one_word_are_reported_as_a_race",
);
/// Format: the line ThreadSanitizer prints for a data race.
const RACE_REPORT: &str = "WARNING: ThreadSanitizer: data race";
/// Format: the instrumentation flags, for code and for the rebuilt standard library.
const SANITIZER_FLAGS: &str = "-Zsanitizer=thread";
/// Format: report every race in a binary rather than stop at the first, so one run names them all.
const TSAN_OPTIONS: &str = "halt_on_error=0";

/// The host's target triple, from the nightly compiler (`-Zbuild-std` needs an explicit target).
fn host_target() -> Result<String, Failure> {
  let output = Command::new("rustc")
    .args(["+nightly", "-vV"])
    .stdin(Stdio::null())
    .output()
    .map_err(|e| Failure(format!("tsan: running `rustc +nightly -vV`: {e}")))?;
  String::from_utf8_lossy(&output.stdout)
    .lines()
    .find_map(|line| line.strip_prefix("host: "))
    .map(str::to_owned)
    .ok_or_else(|| {
      Failure(
        "tsan: `rustc +nightly -vV` named no host; install the nightly toolchain with `rust-src`"
          .to_owned(),
      )
    })
}

/// An instrumented `cargo +nightly test` for `target`, run at the workspace root.
fn instrumented(root: &Path, target: &str) -> Command {
  let mut command = Command::new("cargo");
  command
    .current_dir(root)
    .stdin(Stdio::null())
    .env("RUSTFLAGS", SANITIZER_FLAGS)
    .env("RUSTDOCFLAGS", SANITIZER_FLAGS)
    .env("TSAN_OPTIONS", TSAN_OPTIONS)
    .args(["+nightly", "test", "-Zbuild-std", "--target", target]);
  command
}

/// Step 1: the canary must be reported as a race.
fn canary(root: &Path, target: &str) -> Result<(), Failure> {
  let (test_target, test) = CANARY;
  eprintln!("tsan: the canary ({test_target}::{test}) must be reported");
  let output = instrumented(root, target)
    .args([
      "-p",
      "slates-mem",
      "--test",
      test_target,
      "--",
      "--ignored",
      "--exact",
      test,
    ])
    .output()
    .map_err(|e| Failure(format!("tsan: running the canary: {e}")))?;
  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);
  let reported = stderr.contains(RACE_REPORT) || stdout.contains(RACE_REPORT);
  let ran = stdout.contains(test);
  if reported && ran && !output.status.success() {
    eprintln!("tsan: the canary was reported as a data race, so the instrumentation is live");
    Ok(())
  } else {
    Err(Failure(format!(
      "tsan: the canary was not reported as a race (ran: {ran}, reported: {reported}, exit: {}); a clean run of \
       the suites would prove nothing\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
      output.status.code().unwrap_or(-1)
    )))
  }
}

/// Step 2: every test of [`CRATES`] but [`SKIPPED`], instrumented; any report fails.
fn suites(root: &Path, target: &str) -> Result<(), Failure> {
  let mut command = instrumented(root, target);
  command.args(["--no-fail-fast", "--lib", "--tests"]);
  for krate in CRATES {
    command.args(["-p", krate]);
  }
  command.arg("--");
  for (test, reason) in SKIPPED {
    eprintln!("tsan: skipping {test}: {reason}");
    command.args(["--skip", test]);
  }
  eprintln!(
    "tsan: the suites of {} under ThreadSanitizer",
    CRATES.join(", ")
  );
  let status = command
    .status()
    .map_err(|e| Failure(format!("tsan: running the suites: {e}")))?;
  if status.success() {
    Ok(())
  } else {
    Err(Failure(format!(
      "tsan: the instrumented suites failed (exit {}); a ThreadSanitizer report above names the race",
      status.code().unwrap_or(-1)
    )))
  }
}

/// Step 3: the server's [`SERVER_TARGETS`], instrumented; any report fails.
fn server(root: &Path, target: &str) -> Result<(), Failure> {
  let mut command = instrumented(root, target);
  command.args(["--no-fail-fast", "-p", "slates-server"]);
  command.args(SERVER_TARGETS);
  eprintln!(
    "tsan: the server's {} under ThreadSanitizer",
    SERVER_TARGETS.join(" ")
  );
  let status = command
    .status()
    .map_err(|e| Failure(format!("tsan: running the server's suites: {e}")))?;
  if status.success() {
    Ok(())
  } else {
    Err(Failure(format!(
      "tsan: the server's instrumented suites failed (exit {}); a ThreadSanitizer report above names the race",
      status.code().unwrap_or(-1)
    )))
  }
}

/// Runs the lane: the canary, then the threaded crates' suites, then the server's.
pub fn run(root: &Path) -> Result<(), Failure> {
  let target = host_target()?;
  canary(root, &target)?;
  suites(root, &target)?;
  server(root, &target)
}
