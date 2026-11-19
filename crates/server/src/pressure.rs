//! The pressure hold's sampler, woken by the host's own reports (§4.2 "the pressure hold", admission.md §5.5; A-570).
//! The control shard samples the host's available memory and the daemon's own resident memory and withholds the hold
//! from admission (`crate::daemon::refresh_pressure_hold`): once at its start, which fixes the baseline, and again each
//! time the host reports a change in its memory pressure (`slates_machine::memory_pressure`: macOS's dispatch source,
//! Linux's pressure-stall trigger, Windows' memory resource notifications). Until A-570 the reap loop's tick sampled
//! once a liveness window on every daemon, whether the host's pressure had changed or not.
//!
//! **A hold is let go by sampling again.** A report says the host's pressure changed; Linux's trigger reports stalls
//! and never their end, and memory can come back between two reports elsewhere too. So while a hold is held, the
//! sampler samples again a retry window after each sample (`crate::deadlines::retry_window_ns`), until the hold is
//! nothing; with no hold it waits for a report alone, and never wakes.
//!
//! **Where the host reports nothing** (a Linux kernel without PSI, or one that refuses an unprivileged trigger), the
//! host's own measured stall (A-336's reading, `slates_machine::memory_stall`) is taken at the control shard's
//! heartbeat, a wake the daemon keeps for its liveness anyway, and a stall that began or ended since the last beat asks
//! the sampler to sample ([`stall_beat`]). No threshold is chosen here, and no wake is added for it.
//!
//! **A test's reading is a report.** `Daemon::inject_available_memory` and `inject_resident_memory` put a reading in
//! the host's place and ask the sampler, as the kernel's report would ([`ask`]); the first such reading replaces the
//! source the baseline was taken from, so the baseline is taken again from it.

use std::task::Poll;

use slates_machine::memory_pressure::{Events, Refused};
use slates_machine::memory_stall::{self, StallReading};
use slates_rt::RtError;

use crate::state::{self, ShardState};

/// Counter: the sampler's wait for the host's next report refused by the runtime; it falls back to the host's measured
/// stall at the heartbeat, as on a host that reports nothing.
const REPORT_WAIT_REFUSED: &str = "pressure.report_wait_refused";

/// The sampler (A-570): one task a shard, made at its start; off the control shard it ends at once. It samples, then
/// waits for the host's next report, a test's reading, the heartbeat's measured stall, or, while a hold is held, the
/// retry window, and samples again.
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
      "slates-server: the host reports no memory-pressure changes to this process ({refused}); the pressure hold \
       follows its measured stall at the heartbeat"
    );
  }
  let reported = reports.is_ok();
  if state::with_state(|state| state.pressure_reported = reported).is_none() {
    return;
  }
  loop {
    let held = crate::daemon::refresh_pressure_hold().await.unwrap_or(0) > 0;
    let woke = if held {
      slates_rt::futures::within(crate::deadlines::retry_window_ns(), next_wake(&mut reports))
        .await
        .map(|woke| woke.unwrap_or(Ok(())))
    } else {
      Ok(next_wake(&mut reports).await)
    };
    match woke {
      Ok(Ok(())) => {}
      Ok(Err(refusal)) | Err(refusal) => {
        slates_machine::log_line!(
          "slates-server: the wait for the host's memory-pressure report was refused ({refusal}); the pressure hold \
           follows its measured stall at the heartbeat"
        );
        reports = Err(Refused::Unsupported);
        if state::with_state(|state| {
          state.pressure_reported = false;
          state.count(REPORT_WAIT_REFUSED, 1);
        })
        .is_none()
        {
          return;
        }
      }
    }
  }
}

/// Asks the sampler to sample now (A-570): a reading a test put in the host's place, or a stall the heartbeat measured.
/// Nothing off the control shard, whose sampler alone samples.
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

/// Waits until the host reports a change in its memory pressure, or the sampler is asked ([`ask`]); a host that reports
/// nothing leaves the asking alone. A refused wait is the runtime's typed refusal.
async fn next_wake(reports: &mut Result<Events, Refused>) -> Result<(), RtError> {
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
      Ok(())
    }
  }
}

/// The host's next report or the sampler's asking, whichever comes first (macOS, Linux: the descriptor
/// `slates_machine::memory_pressure::Events` names, awaited readable); a report is taken, so the descriptor is
/// readable again only at the next. The asking is looked at before the descriptor at each poll, so a wake from it is
/// never taken for a report (A-149).
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
async fn report_or_asked(events: &mut Events) -> Result<(), RtError> {
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
  if reported? {
    events.acknowledge();
  }
  Ok(())
}

/// The host's next report or the sampler's asking, whichever comes first (Windows: the memory resource notification
/// that names the next change, awaited signalled through the system thread pool, `slates_rt::handle_wait`); a report
/// moves the awaited notification to the other side.
#[cfg(all(windows, not(miri)))]
async fn report_or_asked(events: &mut Events) -> Result<(), RtError> {
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
  if reported? {
    events.crossed();
  }
  Ok(())
}

/// Where no report can come (another platform, Miri): the asking alone. `Events::open` refuses there, so this is never
/// reached with a source.
#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
async fn report_or_asked(_events: &mut Events) -> Result<(), RtError> {
  std::future::poll_fn(|_| {
    if asked() {
      Poll::Ready(())
    } else {
      Poll::Pending
    }
  })
  .await;
  Ok(())
}

/// The heartbeat's look at the host's measured memory stall (A-570, A-336's reading), on the control shard while the
/// host reports no changes: a stall that began or ended since the last beat (`stalled`, the verdict the last beat left)
/// asks the sampler to sample ([`stall_changed`]). Returns this beat's reading and verdict, the next beat's `before`
/// and `stalled`; a host that reports its changes is not read at all.
pub(crate) fn stall_beat(
  state: &mut ShardState,
  before: Option<StallReading>,
  stalled: bool,
) -> (Option<StallReading>, bool) {
  if state.pressure_reported || state.pressure_task.is_none() {
    return (before, stalled);
  }
  let now = memory_stall::read();
  let (changed, stalled_now) = stall_changed(before, now, stalled);
  if changed {
    ask(state);
  }
  (now, stalled_now)
}

/// Whether a stall began or ended between the readings `before` and `now`, given the verdict `stalled` the last look
/// left, and the verdict now: the host's own rule (`slates_machine::memory_stall::stalled_between`), so a level pressed
/// now, or a cumulative total risen since `before`, is a stall.
fn stall_changed(
  before: Option<StallReading>,
  now: Option<StallReading>,
  stalled: bool,
) -> (bool, bool) {
  let stalled_now = memory_stall::stalled_between(before, now);
  (stalled_now != stalled, stalled_now)
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

  /// A-570. Do: walk the heartbeat's verdict over a level (macOS, Windows) clear, pressed, pressed, clear, and over a
  /// cumulative total (Linux) unchanged, risen, risen, unchanged, then over no reading. Expect: the sampler asked where
  /// a stall begins and where it ends, never while it holds or while nothing stalls, and nothing asked without a
  /// reading.
  #[test]
  fn the_heartbeat_asks_for_a_sample_where_a_stall_begins_or_ends() {
    use slates_machine::memory_stall::StallReading;
    let walk = |readings: &[Option<StallReading>]| {
      let mut before = None;
      let mut stalled = false;
      let mut asked = Vec::new();
      for now in readings {
        let (changed, stalled_now) = super::stall_changed(before, *now, stalled);
        asked.push(changed);
        before = *now;
        stalled = stalled_now;
      }
      asked
    };
    let level = |pressed| Some(StallReading::Level { pressed });
    assert_eq!(
      walk(&[level(false), level(true), level(true), level(false)]),
      [false, true, false, true]
    );
    let total = |stalled_us| Some(StallReading::Cumulative { stalled_us });
    assert_eq!(
      walk(&[total(10), total(10), total(15), total(20), total(20)]),
      [false, false, true, false, true]
    );
    assert_eq!(walk(&[None, None]), [false, false]);
  }
}
