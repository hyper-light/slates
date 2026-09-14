//! An opt-in trace of the fleet harness's waits, so a stall says which daemon stopped advancing and which
//! wait never resolved (§4.8 "Slow versus stuck"; `docs/wip/fleet-under-load.md`). Off by default:
//! [`ENV_TRACE`] names a file — under the caller's scratch directory, never the tree — that every wait
//! appends to. For each poll: the site that waits (file and line), the slowest observed coordinator's
//! period count, how many periods it advanced, how long the condition took to ask, and the wall time,
//! sampled once a second and at every change of verdict. For each observed daemon: its coordinator's
//! period count and each of its shards' pulse (steps, driver waits, parked, kicks skipped, ring-full
//! events — `Daemon::shard_pulses`, read directly off the runtime's registry, so a starved shard is
//! reported rather than queried). Each line is one `write_all` in append mode, so two threads' lines never
//! interleave. The test's name (the thread's) prefixes every line.

use std::fmt;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

/// The environment variable naming the trace file; unset (the default) means no trace.
pub(crate) const ENV_TRACE: &str = "SLATES_FLEET_TRACE";

/// The open trace file and the instant it opened (every line is stamped relative to it).
struct Tracer {
  file: std::fs::File,
  opened: Instant,
}

static TRACER: OnceLock<Option<Tracer>> = OnceLock::new();

/// The tracer, opened on first use from [`ENV_TRACE`]; `None` when tracing is off or the file could not
/// be opened (a diagnostic that cannot be written is silently off, never a failed test).
fn tracer() -> Option<&'static Tracer> {
  TRACER
    .get_or_init(|| {
      let path = std::env::var_os(ENV_TRACE)?;
      let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .ok()?;
      let tracer = Tracer {
        file,
        opened: Instant::now(),
      };
      let _ = (&tracer.file).write_all(
        format!(
          "+{:>9.3}s <trace> opened by pid {}\n",
          0.0,
          std::process::id()
        )
        .as_bytes(),
      );
      Some(tracer)
    })
    .as_ref()
}

/// Whether the trace is on, so a caller skips building an expensive description when it is not.
pub(crate) fn enabled() -> bool {
  tracer().is_some()
}

/// Appends one line: the seconds since the trace opened, the test (the current thread's name), the event.
pub(crate) fn record(event: fmt::Arguments<'_>) {
  let Some(tracer) = tracer() else {
    return;
  };
  let thread = std::thread::current();
  let line = format!(
    "+{:>9.3}s {} {}\n",
    tracer.opened.elapsed().as_secs_f64(),
    thread.name().unwrap_or("?"),
    event
  );
  let _ = (&tracer.file).write_all(line.as_bytes());
}
