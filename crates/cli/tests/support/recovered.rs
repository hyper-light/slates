//! A test's wait for the anchor's verdict that a daemon has recovered (A-311), before the test ends that daemon on
//! purpose.
//!
//! The anchor counts the exit of a daemon that had not yet served as long as its start took as a failed start, and
//! backs off the second in a row, as it backs off a daemon that cannot stay up. A test that ends daemons one after
//! another to prove that something outlives a kill is not testing a crash loop, so it ends each daemon only once the
//! anchor has judged it recovered, which `slates status` reports as `recovered`. Before A-311 these tests waited only
//! for a new daemon to answer, and a kill that came a moment after a start was restarted at once whatever its timing.

use std::process::Command;
use std::time::{Duration, Instant};

/// Format: the status JSON's key for the answering daemon's pid (`slates_mcp::daemon_json`).
const PID_KEY: &str = "\"pid\":";
/// Format: the status JSON's key for the anchor's verdict.
const RECOVERED_KEY: &str = "\"recovered\":";

/// The pid of a daemon of `instance` that the anchor has judged recovered and that `wanted` accepts, asked of
/// `slates status --json` every `poll` for at most `wait`; the test fails naming what it last saw once `wait` passes.
pub(crate) fn await_recovered(
  instance: &str,
  wanted: &dyn Fn(u32) -> bool,
  poll: Duration,
  wait: Duration,
) -> u32 {
  let started = Instant::now();
  let mut last = String::from("no status asked yet");
  loop {
    let output = Command::new(env!("CARGO_BIN_EXE_slates"))
      .args(["--instance", instance, "status", "--json"])
      .output()
      .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
      let pid = after(&text, PID_KEY).and_then(|rest| {
        rest
          .split(|c: char| !c.is_ascii_digit())
          .next()?
          .parse::<u32>()
          .ok()
      });
      let recovered = after(&text, RECOVERED_KEY).is_some_and(|rest| rest.starts_with("true"));
      if let Some(pid) = pid.filter(|pid| recovered && wanted(*pid)) {
        return pid;
      }
      last = format!("pid {pid:?}, recovered {recovered}");
    } else {
      last = format!("status exited {:?}", output.status.code());
    }
    assert!(
      started.elapsed() < wait,
      "no daemon of {instance} the test wanted was judged recovered within {wait:?} (last: {last})"
    );
    // The test paces its asks; shipped code parks on its driver (D-9).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(poll);
  }
}

/// What follows `key`'s first appearance in `text`, its leading spaces skipped.
fn after<'a>(text: &'a str, key: &str) -> Option<&'a str> {
  Some(text.split_once(key)?.1.trim_start())
}
