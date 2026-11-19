//! The pressure hold's sampler, woken by the host's own reports (§4.2 "the pressure hold", admission.md §5.5; A-570).
//! The control shard samples the host's available memory and the daemon's own resident memory and withholds the hold
//! from admission (`crate::daemon::refresh_pressure_hold`): once at its start, which fixes the baseline, and again each
//! time the kernel reports a change in its memory pressure (`slates_machine::memory_pressure`: macOS's dispatch source,
//! Linux's pressure-stall trigger, Windows' memory resource notifications). Between reports the hold stands as last
//! sampled. Until A-570 the reap loop's tick sampled once a liveness window on every daemon, whether the host's
//! pressure had changed or not.
//!
//! **A stall's end is a report too.** macOS reports a return to normal and Windows its high-memory notification;
//! Linux's trigger reports stalls alone, so after one the sampler samples again once the trigger has been silent past
//! its own window (`Events::stall_ended_after_ns`), when the kernel has said in its own terms that the stall is over.
//!
//! **A kernel that offers no report** (a Linux without PSI, or one before unprivileged triggers; another platform) has
//! its watch refused by kind at the start: logged, and counted in the shard's status (`pressure.watch_unsupported`,
//! `pressure.watch_refused`). The hold then stands as the start's sample left it, nothing in its place polls (§2: no
//! second path), and the sampler waits for the asking alone.
//!
//! **A test's reading is a report.** `Daemon::inject_available_memory` and `inject_resident_memory` put a reading in
//! the host's place and ask the sampler, as the kernel's report would ([`ask`]); the first such reading replaces the
//! source the baseline was taken from, so the baseline is taken again from it.

use std::task::Poll;

use slates_machine::memory_pressure::{Events, Refused};
use slates_rt::RtError;

use crate::state::{self, ShardState};

/// Counter: the host's memory-pressure watch refused at the start because the platform offers no report to this build
/// (`slates_machine::memory_pressure::Refused::Unsupported`): the hold stands as the start's sample left it.
const WATCH_UNSUPPORTED: &str = "pressure.watch_unsupported";
/// Counter: the host's memory-pressure watch refused at the start by the host (`Refused::Host`: a kernel without PSI,
/// or one before unprivileged triggers): the hold stands as the start's sample left it.
const WATCH_REFUSED: &str = "pressure.watch_refused";
/// Counter: the sampler's wait for the host's next report refused by the runtime: the watch ends there, as a refused
/// one does, and the hold stands as last sampled.
const REPORT_WAIT_REFUSED: &str = "pressure.report_wait_refused";

/// The sampler (A-570): one task a shard, made at its start; off the control shard it ends at once. It samples, then
/// waits for the host's next report (on Linux, after a report, for the trigger's silence past its window too) or a
/// test's reading, and samples again.
pub(crate) async fn sampler() {
  let Some(control) = state::with_state(|state| {
    if !crate::verbs::is_control_shard(state) {
      return false;
    }
    state.pressure_task = slates_rt::futures::current_task();
    true
  }) else {
    return;
  };
  if !control {
    return;
  }
  let mut reports = Events::open();
  if let Err(refused) = &reports {
    slates_machine::log_line!(
      "slates-server: the host's memory-pressure watch is refused ({refused}); the pressure hold stands as its start \
       sample left it"
    );
    let kind = match refused {
      Refused::Unsupported => WATCH_UNSUPPORTED,
      Refused::Host { .. } => WATCH_REFUSED,
    };
    if state::with_state(|state| state.count(kind, 1)).is_none() {
      return;
    }
  }
  // Whether the last wake was the kernel's report: on Linux the trigger's silence after one is the stall's end.
  let mut reported = false;
  loop {
    crate::daemon::refresh_pressure_hold().await;
    let silence = reports
      .as_ref()
      .ok()
      .filter(|_| reported)
      .and_then(Events::stall_ended_after_ns);
    let woke = match silence {
      // The trigger's silence past its window: the stall is over, a report of its own.
      Some(ns) => slates_rt::futures::within(ns, next_wake(&mut reports))
        .await
        .map(|woke| woke.unwrap_or(Ok(false))),
      None => Ok(next_wake(&mut reports).await),
    };
    match woke {
      Ok(Ok(report)) => reported = report,
      Ok(Err(refusal)) | Err(refusal) => {
        slates_machine::log_line!(
          "slates-server: the wait for the host's memory-pressure report was refused ({refusal}); the pressure hold \
           stands as last sampled"
        );
        reports = Err(Refused::Unsupported);
        reported = false;
        if state::with_state(|state| state.count(REPORT_WAIT_REFUSED, 1)).is_none() {
          return;
        }
      }
    }
  }
}

/// Asks the sampler to sample now (A-570): a reading a test put in the host's place, which stands for the kernel's
/// report. Nothing off the control shard, whose sampler alone samples.
pub(crate) fn ask(state: &mut ShardState) {
  state.pressure_asked = true;
  if let Some(task) = state.pressure_task {
    slates_rt::registry::wake(task.0);
  }
}

/// Whether the sampler was asked since it last looked, taken.
fn asked() -> bool {
  state::with_state(|state| std::mem::take(&mut state.pressure_asked)) == Some(true)
}

/// Waits until the host reports a change in its memory pressure, or the sampler is asked ([`ask`]): whether it was the
/// host's report. A refused watch leaves the asking alone. A refused wait is the runtime's typed refusal.
async fn next_wake(reports: &mut Result<Events, Refused>) -> Result<bool, RtError> {
  match reports {
    Ok(events) => report_or_asked(events).await,
    Err(_) => {
      std::future::poll_fn(|_| {
        if asked() {
          Poll::Ready(())
        } else {
          Poll::Pending
        }
      })
      .await;
      Ok(false)
    }
  }
}

/// The host's next report or the sampler's asking, whichever comes first (macOS, Linux: the descriptor
/// `slates_machine::memory_pressure::Events` names, awaited readable); a report is taken, so the descriptor is
/// readable again only at the next. The asking is looked at before the descriptor at each poll, so a wake from it is
/// never taken for a report (A-149).
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
async fn report_or_asked(events: &mut Events) -> Result<bool, RtError> {
  use std::future::Future;
  use std::os::fd::AsRawFd;
  let raw = events.descriptor().as_raw_fd();
  let reported = {
    let mut report = std::pin::pin!(slates_rt::readiness::readable(raw));
    std::future::poll_fn(|cx| {
      if asked() {
        return Poll::Ready(Ok(false));
      }
      report.as_mut().poll(cx).map(|ready| ready.map(|()| true))
    })
    .await
  };
  let reported = reported?;
  if reported {
    events.acknowledge();
  }
  Ok(reported)
}

/// The host's next report or the sampler's asking, whichever comes first (Windows: the memory resource notification
/// that names the next change, awaited signalled through the system thread pool, `slates_rt::handle_wait`); a report
/// moves the awaited notification to the other side.
#[cfg(all(windows, not(miri)))]
async fn report_or_asked(events: &mut Events) -> Result<bool, RtError> {
  use std::future::Future;
  let reported = {
    let mut report = std::pin::pin!(slates_rt::handle_wait::signalled(events.awaited()));
    std::future::poll_fn(|cx| {
      if asked() {
        return Poll::Ready(Ok(false));
      }
      report.as_mut().poll(cx).map(|ready| ready.map(|()| true))
    })
    .await
  };
  let reported = reported?;
  if reported {
    events.crossed();
  }
  Ok(reported)
}

/// Where no report can come (another platform, Miri): the asking alone. `Events::open` refuses there, so this is never
/// reached with a source.
#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
async fn report_or_asked(_events: &mut Events) -> Result<bool, RtError> {
  std::future::poll_fn(|_| {
    if asked() {
      Poll::Ready(())
    } else {
      Poll::Pending
    }
  })
  .await;
  Ok(false)
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant};

  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: the daemon's own resident memory the test pins (any steady reading: the hold subtracts its growth alone).
  const RESIDENT: u64 = 64 << 20;
  /// Shape: the share of the daemon's capacity the host's available memory falls by: an eighth, as the recovery
  /// suite's later drop.
  const FALL_PARTS: u64 = 8;
  /// Shape: how long the test waits for the first sample to fix the baseline: a window and a half, past the reap loop's
  /// first look a window into the start.
  const BASELINE_WITHIN_NS: u64 = LIVENESS_BUDGET_NS / 2 * 3;
  /// Shape: how long the test looks for the hold after the fall: half a window, short of a tick a window on.
  const HELD_WITHIN_NS: u64 = LIVENESS_BUDGET_NS / 2;
  /// Shape: the pause between the test's looks.
  const LOOK_PAUSE_NS: u64 = 1_000_000;

  /// Looks with `look` until it answers true or `within_ns` passes since `since`: how long that took.
  fn look_until(
    since: Instant,
    within_ns: u64,
    mut look: impl FnMut() -> bool,
  ) -> Option<Duration> {
    loop {
      if look() {
        return Some(since.elapsed());
      }
      if since.elapsed() > Duration::from_nanos(within_ns) {
        return None;
      }
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    }
  }

  /// A-570 (the pressure hold off the tick). Do: on a one-shard daemon, put the sampler's readings in the host's place
  /// (the daemon's own resident memory, and the host's available memory equal to its capacity), wait for the first
  /// sample to fix the baseline, then let available fall by an eighth of the capacity. Expect: that eighth withheld
  /// within half a liveness window of the fall, by the sampler the reading woke, as the kernel's report would. The reap
  /// loop's tick sampled once a window, and its sample was what fixed the baseline, so the fall waited a window.
  #[test]
  fn a_reported_fall_in_available_memory_is_held_back_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let capacity = crate::daemon::observe_first(&daemon, |state| state.config.reserve_per_shard);
    daemon.inject_resident_memory(RESIDENT).unwrap();
    daemon.inject_available_memory(capacity).unwrap();
    let baseline = look_until(Instant::now(), BASELINE_WITHIN_NS, || {
      daemon.pressure_baseline() == Ok(Some(capacity))
    });
    let fall = capacity / FALL_PARTS;
    let fell = Instant::now();
    daemon.inject_available_memory(capacity - fall).unwrap();
    let held = look_until(
      fell,
      HELD_WITHIN_NS,
      || matches!(daemon.pressure_withheld(), Ok(Ok(withheld)) if withheld == fall),
    );
    let withheld = daemon.pressure_withheld();
    daemon.stop();
    assert!(baseline.is_some(), "the first sample fixed the baseline");
    let held = held.unwrap_or_else(|| {
      panic!("the fall was held back within half a liveness window: {withheld:?} for {fall}")
    });
    println!("a fall of {fall} bytes in available memory held back {held:?} after it");
  }
}
