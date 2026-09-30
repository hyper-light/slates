//! An opt-in trace of the fleet harness's waits, so a stall says which daemon stopped advancing and which
//! wait never resolved (§4.8 "Slow versus stuck"; `docs/wip/fleet-under-load.md`). Off by default:
//! [`ENV_TRACE`] set to `1` turns it on. For each poll: the site that waits (file and line), the slowest
//! observed coordinator's period count, how many periods it advanced, how long the condition took to ask,
//! and the wall time, sampled once a second and at every change of verdict. For each observed daemon: its
//! coordinator's period count and each of its shards' pulse (steps, driver waits, parked, kicks skipped,
//! ring-full events — `Daemon::shard_pulses`, read directly off the runtime's registry, so a starved shard
//! is reported rather than queried). The test's name (the thread's) prefixes every line.
//!
//! The trace goes to the test's error stream, never a file (A-50; audit AUD-29-63): libtest holds it in
//! memory per test and prints it with the test's failure, or streams it under `--nocapture`. Each line is
//! one `eprint!`, which holds the stream's lock for the whole line, so two threads' lines never
//! interleave. Until 2026-09-30 the variable named a file the harness created and appended to, silently
//! off when it could not open it.

use std::fmt;
use std::sync::OnceLock;
use std::time::Instant;

/// The environment variable that turns the trace on (`1`); unset (the default) means no trace.
pub(crate) const ENV_TRACE: &str = "SLATES_FLEET_TRACE";
/// Format: the value of [`ENV_TRACE`] that turns the trace on.
const ON: &str = "1";

/// The instant the trace began, when it is on: every line is stamped relative to it.
static OPENED: OnceLock<Option<Instant>> = OnceLock::new();

/// The trace's start, read from [`ENV_TRACE`] on first use; `None` when tracing is off. Any other value
/// than [`ON`] is refused loudly, so a stale `SLATES_FLEET_TRACE=<path>` is not mistaken for a trace.
fn opened() -> Option<Instant> {
  *OPENED.get_or_init(|| {
    let value = std::env::var(ENV_TRACE).ok()?;
    assert_eq!(
      value, ON,
      "{ENV_TRACE} turns the trace on with `1`; it no longer names a file (A-50)"
    );
    eprintln!("+{:>9.3}s <trace> on in pid {}", 0.0, std::process::id());
    Some(Instant::now())
  })
}

/// Whether the trace is on, so a caller skips building an expensive description when it is not.
pub(crate) fn enabled() -> bool {
  opened().is_some()
}

/// Writes one line: the seconds since the trace began, the test (the current thread's name), the event.
pub(crate) fn record(event: fmt::Arguments<'_>) {
  let Some(opened) = opened() else {
    return;
  };
  let thread = std::thread::current();
  eprintln!(
    "+{:>9.3}s {} {}",
    opened.elapsed().as_secs_f64(),
    thread.name().unwrap_or("?"),
    event
  );
}
