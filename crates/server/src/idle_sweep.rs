//! The idle sweep at its own deadline (A-99, A-105, A-214; the event-driven reaper's piece 7, A-715): a pass ages each
//! volume's writes, seals the open extents of files idle a whole pass, publishes what the seal moved or a free left
//! deferred, and on an idle shard gives free content, dead recovery memory and the emptied write log back to the OS
//! and refills the write reserve (`crate::daemon::seal_idle_content`). Until A-715 the reap loop's tick ran a pass
//! once a liveness window on every shard, whether it owed one or not.
//!
//! **A pass is owed by what it would act on, and armed then.** Every state borrow closes by asking whether the shard
//! owes a pass ([`after_borrow`]): a write stamped a file for the seal (`Store::sweep_owed`, set at the stamp), the
//! content arena's allocation count or allocated bytes moved since the last pass ended (a write's allocation; a free
//! once its publication committed), or frees are deferred behind the committed image. Then the pass is armed a sweep's
//! interval on ([`interval_ns`]), on the shard's deadline keeper (`crate::deadlines`), which wakes this task at it. A
//! pass that does not settle the shard (a file written but not yet aged, a seal or deferred free that published, a
//! busy arena) arms the next one; a pass that settles it arms nothing, so an idle shard is never woken for the sweep.
//! While a pass is asked for or runs nothing arms another.
//!
//! **A give-back is idle-time work.** A pass's give-back (the free content, the dead recovery memory, the emptied write
//! log) stops at the first step across whose yield another task ran (A-226), and resumes at the shard's next idle
//! point, where the shard's idle work wakes this task ([`idle_work`]); the pass stays under way meanwhile. Until
//! 2026-11-23 it resumed only in the next pass, a whole interval on, which anything running once an interval cut short
//! again at its first step (CI's windows-latest: a test's look once an interval on the same scheduler tick as each
//! pass, one step a pass, a 1.43 GB walk 680 MB short at the 60-interval bound; diag run 45275623386).
//!
//! **The aging rule is unchanged**: a file is sealed one to two intervals after its last write (`Volume::age_writes`),
//! as ZFS commits a transaction group and Linux writes back a page past `dirty_expire_centisecs`; the purge still waits
//! for a pass that finds the arena allocated nothing since the one before (A-105).
//!
//! **The first pass** is armed when the shard starts, a sweep's interval on, for what a restart left: the recovery
//! memory no restart reads, given back by an idle pass (A-117).
//!
//! **Idle base directories close their descriptors (A-838).** A use of an overlay's base directory marks it and owes
//! the sweep a look (`Store::base_dirs_owed`, set by `Overlay::reacquire`), which arms a pass. Each pass looks at every
//! overlay's directories a slice a turn ([`close_idle_directories`]): one used since the last look keeps its
//! descriptor and is marked unused, one unused since closes it and is reacquired by its path at its next use. So a
//! directory's descriptor goes one to two intervals after its last use, and a volume no client uses holds only its
//! root's. A pass that kept any directory (`held`) is not settled and arms the next. Until 2026-11-21 every listed
//! directory kept its descriptor for the volume's life: kiln's restarted daemon on adacachy held 79,038 of them two
//! minutes after its start.

use slates_vfs::clock::Clock;

use crate::daemon::PassEnd;
use crate::state::{self, ShardState};

/// Format: the status count of base directories the idle sweep closed (A-838): each unused a whole interval.
pub(crate) const BASE_DIRS_CLOSED: &str = "base.idle_dirs_closed";

/// Derived: a sweep's interval, how long after what owes it a pass runs: the liveness budget, the reap loop's tick
/// it replaces (A-99's aging unit: a file is sealed one to two intervals after its last write).
pub(crate) fn interval_ns() -> u64 {
  slates_machine::derived!(
    crate::daemon::LIVENESS_BUDGET_NS,
    "the liveness budget (the anchor's question of the daemon, asked of the daemon's clients)",
    ["LIVENESS_BUDGET_NS"]
  )
  .get()
}

/// The idle sweep's task (A-715): one a shard, made at its start. It arms the first pass, then runs a pass each time the
/// keeper asks for one, arming the next while the shard is not settled. A pass whose give-back other work interrupted
/// ([`PassEnd::GiveBackLeft`]) stays under way and resumes the give-back at the shard's next idle point ([`idle_work`]),
/// as often as it is interrupted, until it settles or finds the shard busy.
pub(crate) async fn sweeper() {
  let started = state::with_state(|s| {
    s.idle_sweeper = slates_rt::futures::current_task();
    arm(s);
  });
  if started.is_none() {
    return;
  }
  loop {
    slates_rt::futures::idle().await;
    match state::with_state(|s| s.idle_sweep_running) {
      None => return,
      // Woken for something else: wait for the keeper's ask.
      Some(false) => continue,
      Some(true) => {}
    }
    if state::with_state(|s| s.count(PASSES_STARTED, 1)).is_none() {
      return;
    }
    let mut ended = crate::daemon::seal_idle_content().await;
    while let PassEnd::GiveBackLeft { directories_idle } = ended {
      if state::with_state(|s| s.give_back_waits_idle = true).is_none() {
        return;
      }
      slates_rt::futures::idle().await;
      // Woken at the idle point, or by anything else: either way the give-back goes on, and stops again if other work
      // runs across its next step.
      if state::with_state(|s| s.give_back_waits_idle = false).is_none() {
        return;
      }
      ended = crate::daemon::resume_give_back(directories_idle).await;
    }
    if state::with_state(|s| finish(s, ended == PassEnd::Settled)).is_none() {
      return;
    }
  }
}

/// Counter: give-backs resumed at the shard's idle point after other work interrupted them (A-226): the non-vacuity count
/// of [`idle_work`]'s resume. Format: a counter name in the status report.
pub(crate) const GIVE_BACK_RESUMED: &str = "content.give_back.resumed_at_idle";

/// Counter: sweep passes started, each when the keeper asked for it (A-715). Format: a counter name in the status report.
pub(crate) const PASSES_STARTED: &str = "sweep.passes_started";

#[cfg(test)]
thread_local! {
  /// Test support: a task the keeper wakes beside each pass it starts (`crate::deadlines`), on the shard's thread: the
  /// poller of the tests' hostile-look case, set from the shard.
  pub(crate) static TEST_POLLER: std::cell::Cell<Option<slates_rt::TaskId>> =
    const { std::cell::Cell::new(None) };
}

/// The shard's idle work (`ShardContext::set_idle_work`, the shard's one idle-work slot, registered at its start): what
/// the shard does when it finds nothing ready and its idle spin has run out, before it parks. A pass whose give-back
/// other work interrupted waits for this point, so here the sweep is woken to resume it; otherwise an armed
/// content-range sweep advances a slice (`crate::content_ranges::idle_slice`, A-744). Whether it did anything, so the
/// shard looks for work again rather than park.
///
/// Until 2026-11-23 an interrupted give-back waited a whole sweep's interval for the next pass, which anything running
/// once an interval cut short again at its first step: on CI's windows-latest a test looking once an interval landed on
/// the same 15.6 ms scheduler tick as each pass, so each pass gave back one slice (60 passes of one step, 680 MB of a
/// 1.43 GB walk left at the 60-interval bound; diag run 45275623386), where one uninterrupted pass walks the rest in 2.6
/// to 5.9 ms. A client polling once a second does the same in the field. At the idle point a give-back never runs
/// beside ready work (A-226's rule is unchanged), and under sustained load there is no idle point, so it waits.
pub(crate) fn idle_work() -> bool {
  let resumed = state::with_state(|s| {
    if !std::mem::take(&mut s.give_back_waits_idle) {
      return false;
    }
    let Some(sweeper) = s.idle_sweeper else {
      return false;
    };
    s.count(GIVE_BACK_RESUMED, 1);
    slates_rt::registry::wake(sweeper.0);
    true
  })
  .unwrap_or(false);
  resumed || crate::content_ranges::idle_slice()
}

/// Ends a pass: what the arena holds now is what the next borrow compares against, and a pass that did not settle the
/// shard arms the next.
fn finish(s: &mut ShardState, settled: bool) {
  s.idle_sweep_running = false;
  s.idle_sweep_seen = arena_seen(s);
  if !settled {
    arm(s);
  }
}

/// The content arena's allocation count and allocated bytes.
fn arena_seen(s: &ShardState) -> (u64, usize) {
  let arena = s.store.content.arena();
  (arena.allocations(), arena.allocated_bytes())
}

/// Arms the next pass a sweep's interval from now, unless one is armed or under way.
fn arm(s: &mut ShardState) {
  if s.idle_sweep_at.is_some() || s.idle_sweep_running {
    return;
  }
  let at = s.clock.monotonic_ns().saturating_add(interval_ns());
  s.idle_sweep_at = Some(at);
  crate::deadlines::noted(s, at);
}

/// Closes every state borrow (`state::try_with_state`): a write stamped for the seal, an allocation or a free since the
/// last pass, or a free deferred behind the image arms a pass. Nothing is read while one is armed or under way.
pub(crate) fn after_borrow(s: &mut ShardState) {
  if s.idle_sweep_at.is_some() || s.idle_sweep_running {
    return;
  }
  let written = std::mem::take(&mut s.store.sweep_owed);
  let directories = s.store.base_dirs_owed.take();
  if written
    || directories
    || arena_seen(s) != s.idle_sweep_seen
    || s.store.content.arena().deferred_bytes() > 0
  {
    arm(s);
  }
}

/// A pass's look at every overlay's base directories (A-838): each volume's idle-directory cycle run to its end, a
/// slice a turn, each slice bounded by half the shard's step quantum on its clock, the shard yielding between slices.
/// A volume whose cycle ended this pass is not looked at again in it, so a directory is closed only on a second pass's
/// look. Whether every directory looked at was idle: a pass that kept any is not settled.
pub(crate) async fn close_idle_directories() -> bool {
  let mut done: Vec<slates_mem::Handle<state::VolumeSlot>> = Vec::new();
  let mut kept = 0usize;
  loop {
    let turn = state::with_state(|s| {
      let deadline = s
        .clock
        .monotonic_ns()
        .saturating_add(s.config.step_quantum_ns() / 2);
      let (mut closed, mut held, mut more) = (0usize, 0usize, false);
      let ShardState { volumes, .. } = &mut *s;
      for (handle, slot) in volumes.iter_mut_all() {
        if done.contains(&handle) {
          continue;
        }
        let Some(host) = slot.host.as_mut() else {
          continue;
        };
        // A volume whose plane cannot be swept (no longer an overlay) is passed over, as one with nothing to close.
        let Ok(slice) = slot.volume.with_host(host).close_idle_directories(deadline) else {
          done.push(handle);
          continue;
        };
        closed = closed.saturating_add(slice.closed);
        held = held.saturating_add(slice.held);
        if slice.more {
          more = true;
          break;
        }
        done.push(handle);
      }
      if closed > 0 {
        s.count(BASE_DIRS_CLOSED, u64::try_from(closed).unwrap_or(u64::MAX));
      }
      (held, more)
    });
    let Some((held, more)) = turn else {
      return true;
    };
    kept = kept.saturating_add(held);
    if !more {
      return kept == 0;
    }
    slates_rt::futures::yield_now().await;
  }
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant};

  use slates_vfs::clock::Clock;

  /// Shape: how many sweep intervals a test allows the first pass to settle the shard in: a start leaves the free
  /// content it sized to be given back a slice a pass whenever another task (the control shard's 100 ms heartbeat)
  /// runs during it, 3.2 GB over about ten passes on the Mac Studio; a look returns as soon as it has settled.
  const FIRST_SETTLE_INTERVALS: u32 = 60;

  /// The sweep passes the shard has run so far, of every kind (`crate::daemon`'s pass counters).
  fn passes(state: &mut crate::state::ShardState) -> u64 {
    [
      crate::daemon::TICKS_SEALING,
      crate::daemon::TICKS_DEFERRED,
      crate::daemon::TICKS_BUSY,
      crate::daemon::TICKS_IDLE,
    ]
    .iter()
    .map(|kind| state.refusals.get(kind).copied().unwrap_or(0))
    .sum()
  }

  /// Whether the shard's sweep is settled: no pass armed or under way.
  fn settled(state: &mut crate::state::ShardState) -> bool {
    state.idle_sweep_at.is_none() && !state.idle_sweep_running
  }

  /// Looks at `daemon`'s shard with `look` until it answers true or `bound` passes: whether it did.
  fn look_until(
    daemon: &crate::daemon::Daemon,
    bound: Duration,
    look: impl Fn(&mut crate::state::ShardState) -> bool + Clone + Send + 'static,
  ) -> bool {
    let pause = Duration::from_nanos(super::interval_ns());
    let begun = Instant::now();
    loop {
      if crate::daemon::observe_first(daemon, look.clone()) {
        return true;
      }
      if begun.elapsed() > bound {
        return false;
      }
      // The harness paces its looks a sweep's interval apart: each look is a task on the shard, and a give-back step
      // that sees another task run leaves the rest for the next pass (A-226), so frequent looks keep a pass unsettled.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(pause);
    }
  }

  /// A-715. Do: on a one-shard daemon, wait for its sweep to settle (its first pass, a sweep's interval after its start),
  /// then leave it idle for three intervals. Expect: no pass runs while nothing owes one. Before, the reap loop's tick
  /// ran a pass every interval on every shard, idle or not (three or more here).
  #[test]
  fn an_idle_shard_runs_no_sweep_pass() {
    let interval = Duration::from_nanos(super::interval_ns());
    let daemon = crate::daemon::audit_daemon_of(1);
    // What a start leaves to give back (the recovery memory no restart reads, gigabytes on a large host) is given back
    // first, so the first pass settles the shard rather than leave slices for a pass each heartbeat interrupts.
    let _ = daemon.give_back_recovery_on_every_shard();
    let first = look_until(&daemon, interval * FIRST_SETTLE_INTERVALS, |state| {
      passes(state) > 0 && settled(state)
    });
    let before = crate::daemon::observe_first(&daemon, passes);
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(interval * 3);
    let after = crate::daemon::observe_first(&daemon, passes);
    daemon.stop();
    assert!(first, "the first pass ran and settled the shard");
    assert_eq!(after, before, "no pass ran on the idle shard");
  }

  /// A step counter of the sweep's give-back: its idle steps and its busy ones.
  fn give_back_steps(state: &mut crate::state::ShardState) -> u64 {
    [crate::daemon::TICKS_IDLE, crate::daemon::TICKS_BUSY]
      .iter()
      .map(|kind| state.refusals.get(kind).copied().unwrap_or(0))
      .sum()
  }

  /// The hostile poller: each time the keeper starts a pass it takes turns on the shard until the pass has taken a
  /// give-back step or ended, so it runs across that step's yield and cuts the pass short there, whichever of the two
  /// the shard polls first.
  async fn poller() {
    loop {
      slates_rt::futures::idle().await;
      let Some(before) = super::state::with_state(give_back_steps) else {
        return;
      };
      loop {
        slates_rt::futures::yield_now().await;
        match super::state::with_state(|state| {
          (give_back_steps(state), state.idle_sweep_running)
        }) {
          None => return,
          Some((steps, running)) if steps > before || !running => break,
          Some(_) => {}
        }
      }
    }
  }

  /// A-226, A-715 (main 01a20182's ruling of 2026-11-23; the drift-free case). Do: on a one-shard daemon, set a poller
  /// that the keeper wakes beside each pass it starts and that takes turns on the shard until the pass has taken a
  /// step, so it cuts every pass short at its first step on every platform. A look once an interval did that on
  /// windows-latest, landing on the same 15.6 ms scheduler tick as each pass, and arm64's timing drifted out of it after
  /// a few passes. Then look until the sweep has settled. Expect: the first pass settles the shard by itself, its
  /// give-back resumed at the shard's idle points (counted) and no second pass started. Until 2026-11-23 a cut pass's
  /// give-back resumed only in the next pass, an interval on, which the poller cut again at its first step: one step an
  /// interval, and every pass's first step walks one page, since a fresh shard has learned no rate.
  #[test]
  fn a_pass_cut_short_at_each_start_settles_in_that_pass_at_the_shards_idle_points() {
    let interval = Duration::from_nanos(super::interval_ns());
    let daemon = crate::daemon::audit_daemon_of(1);
    // What a start leaves to give back is given back first, as the tests above do.
    let _ = daemon.give_back_recovery_on_every_shard();
    let polling = crate::daemon::observe_first(&daemon, |_state| {
      let task = slates_rt::futures::spawn(poller()).ok();
      if let Some(task) = task {
        let _ = slates_rt::futures::detach(task);
      }
      super::TEST_POLLER.set(task);
      task.is_some()
    });
    let first = look_until(&daemon, interval * FIRST_SETTLE_INTERVALS, |state| {
      passes(state) > 0 && settled(state)
    });
    let (started, resumed, seen) = crate::daemon::observe_first(&daemon, |state| {
      let count = |state: &crate::state::ShardState, kind: &str| {
        state.refusals.get(kind).copied().unwrap_or(0)
      };
      (
        count(state, super::PASSES_STARTED),
        count(state, super::GIVE_BACK_RESUMED),
        describe(state),
      )
    });
    daemon.stop();
    assert!(polling, "the poller is on the shard");
    assert!(first, "the first pass ran and settled the shard: {seen}");
    assert!(
      resumed > 0,
      "the poller cut the pass, and its give-back resumed at the shard's idle point: {seen}"
    );
    assert_eq!(
      started, 1,
      "one pass settled the shard, its give-back resumed in it rather than in later passes: {seen}"
    );
  }

  /// A-715. Do: on a one-shard daemon whose sweep has settled, create a volume and write a file into it, then look
  /// until a pass has run. Expect: the write armed a pass (the arena allocated, the file was stamped for the seal), and
  /// it ran within a few intervals; on a settled shard nothing else would have run one.
  #[test]
  fn a_write_on_a_settled_shard_arms_a_pass() {
    let interval = Duration::from_nanos(super::interval_ns());
    let daemon = crate::daemon::audit_daemon_of(1);
    // What a start leaves to give back (the recovery memory no restart reads, gigabytes on a large host) is given back
    // first, so the first pass settles the shard rather than leave slices for a pass each heartbeat interrupts.
    let _ = daemon.give_back_recovery_on_every_shard();
    let first = look_until(&daemon, interval * FIRST_SETTLE_INTERVALS, |state| {
      passes(state) > 0 && settled(state)
    });
    let before = crate::daemon::observe_first(&daemon, passes);
    let wrote = crate::daemon::observe_first(&daemon, |state| {
      crate::idle_sweep::tests::write_a_file(state)
    });
    let armed = crate::daemon::observe_first(&daemon, |state| !settled(state));
    let swept = look_until(&daemon, interval * 10, move |state| passes(state) > before);
    let seen = crate::daemon::observe_first(&daemon, describe);
    daemon.stop();
    assert!(first, "the first pass ran and settled the shard");
    assert!(wrote, "the file was written");
    assert!(armed, "the write armed a pass");
    assert!(swept, "the pass ran: {seen}");
  }

  /// Shape: the base directories the A-838 test lists under its overlay's root: enough that a sweep closing none, or
  /// some, cannot pass for one closing all.
  #[cfg(unix)] // its scratch base is git_hooks' ScratchDir, Unix only
  const LISTED_DIRECTORIES: usize = 16;

  /// The descriptors the host of the shard's only overlay holds now, `None` with no overlay on a disk host.
  #[cfg(unix)] // its scratch base is git_hooks' ScratchDir, Unix only
  fn base_descriptors(state: &mut crate::state::ShardState) -> Option<usize> {
    state.volumes.iter_mut_all().find_map(|(_, slot)| {
      slot
        .host
        .as_ref()?
        .as_disk()
        .map(|host| host.open_handles())
    })
  }

  /// Creates an overlay of `base` and lists its root and then each of its [`LISTED_DIRECTORIES`] directories: the
  /// descriptors its host held before any listing (the root's) and after them, `None` where a verb was refused.
  #[cfg(unix)] // its scratch base is git_hooks' ScratchDir, Unix only
  fn overlay_listed(state: &mut crate::state::ShardState, base: String) -> Option<(usize, usize)> {
    use slates_ipc::protocol::{ReadAt, ReplyBody, RequestBody};
    let principal = slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 0,
    };
    let ReplyBody::Created { id } = crate::verbs::dispatch(
      state,
      1,
      &principal,
      RequestBody::Create {
        name: "idle-dirs".to_owned(),
        size: slates_ipc::protocol::SizeClass::Bounded { limit: 1 << 20 },
        names: slates_ipc::protocol::NamePolicy::Exact,
        require_locked: false,
        base: Some(base),
      },
    ) else {
      return None;
    };
    // The root's descriptor alone, before any listing: listing a directory opens each child directory it finds.
    let rooted = base_descriptors(state)?;
    let list = |state: &mut crate::state::ShardState, path: String| {
      !matches!(
        crate::verbs::dispatch(
          state,
          1,
          &principal,
          RequestBody::ReadDir {
            volume: id,
            path,
            at: ReadAt::Head,
            cursor: 0,
          },
        ),
        ReplyBody::Refused { .. }
      )
    };
    if !list(state, "/".to_owned()) {
      return None;
    }
    for directory in 0..LISTED_DIRECTORIES {
      if !list(state, format!("/d{directory}")) {
        return None;
      }
    }
    Some((rooted, base_descriptors(state)?))
  }

  /// A-838. Do: on a one-shard daemon whose sweep has settled, overlay a scratch directory holding
  /// [`LISTED_DIRECTORIES`] directories, each with a file; list the root and then each directory, so the overlay's host
  /// holds a descriptor for each; then leave the volume idle and look until its host holds only what it held before
  /// any listing, the root's. Expect: the listings opened a descriptor each; the sweep closed every one within a few intervals
  /// (one to two after the last use), counting each (`base.idle_dirs_closed`), and the root's stayed. Before, every
  /// listed directory kept its descriptor for the volume's life: kiln's restarted daemon on adacachy held 79,038.
  #[cfg(unix)] // its scratch base is git_hooks' ScratchDir, Unix only
  #[test]
  #[allow(clippy::disallowed_methods)] // test code: the overlay's base, made in its own scratch directory
  fn an_idle_overlays_listed_directories_close_and_its_root_stays_open() {
    let interval = Duration::from_nanos(super::interval_ns());
    let dir = crate::git_hooks::tests::ScratchDir::new();
    for directory in 0..LISTED_DIRECTORIES {
      let path = dir.path.join(format!("d{directory}"));
      std::fs::create_dir(&path).expect("a base directory");
      std::fs::write(path.join("file"), b"base").expect("a base file");
    }
    let daemon = crate::daemon::audit_daemon_of(1);
    let _ = daemon.give_back_recovery_on_every_shard();
    let first = look_until(&daemon, interval * FIRST_SETTLE_INTERVALS, |state| {
      passes(state) > 0 && settled(state)
    });
    let closed_before = crate::daemon::observe_first(&daemon, |state| {
      state
        .refusals
        .get(super::BASE_DIRS_CLOSED)
        .copied()
        .unwrap_or(0)
    });
    let base = dir.path.to_string_lossy().into_owned();
    let listed = crate::daemon::observe_first(&daemon, move |state| overlay_listed(state, base));
    let rooted = listed.map_or(0, |(rooted, _)| rooted);
    let begun = Instant::now();
    let closed = look_until(&daemon, interval * 10, move |state| {
      base_descriptors(state) == Some(rooted)
    });
    let took = begun.elapsed();
    let (held, counted) = crate::daemon::observe_first(&daemon, |state| {
      (
        base_descriptors(state),
        state
          .refusals
          .get(super::BASE_DIRS_CLOSED)
          .copied()
          .unwrap_or(0),
      )
    });
    daemon.stop();
    assert!(first, "the first pass ran and settled the shard");
    let (rooted, opened) = listed.expect("the overlay was made and its directories listed");
    assert_eq!(
      opened,
      rooted + LISTED_DIRECTORIES,
      "each listed directory held a descriptor"
    );
    assert!(
      closed,
      "the idle directories closed within {:?}: the host holds {held:?}, {rooted} before any listing",
      interval * 10
    );
    assert!(
      counted - closed_before >= LISTED_DIRECTORIES as u64,
      "the sweep counted each close: {counted} after {closed_before}"
    );
    println!(
      "{LISTED_DIRECTORIES} idle directories closed {took:?} after their last use (a sweep's interval is {interval:?})"
    );
  }

  /// What the shard's sweep has done and owes, for a failure's message.
  fn describe(state: &mut crate::state::ShardState) -> String {
    format!(
      "{:?} armed in {:?} running {} deferred {} seen {:?} allocations {} owes {}",
      state
        .refusals
        .iter()
        .filter(|(kind, _)| kind.starts_with("content"))
        .collect::<Vec<_>>(),
      state
        .idle_sweep_at
        .map(|at| at.saturating_sub(state.clock.monotonic_ns())),
      state.idle_sweep_running,
      state.store.content.arena().deferred_bytes(),
      state.idle_sweep_seen,
      state.store.content.arena().allocations(),
      state
        .volumes
        .iter_mut_all()
        .any(|(_, slot)| slot.volume.sweep_owes())
    )
  }

  /// Creates a volume on the shard and writes a file into it: whether it was written.
  fn write_a_file(state: &mut crate::state::ShardState) -> bool {
    let principal = slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 0,
    };
    let reply = crate::verbs::dispatch(
      state,
      1,
      &principal,
      slates_ipc::protocol::RequestBody::Create {
        name: "swept".to_owned(),
        size: slates_ipc::protocol::SizeClass::Bounded { limit: 1 << 20 },
        names: slates_ipc::protocol::NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    let slates_ipc::protocol::ReplyBody::Created { id } = reply else {
      return false;
    };
    let Some(handle) = state.by_id.get(&crate::verbs::to_db_volume(id)).copied() else {
      return false;
    };
    let crate::state::ShardState { volumes, store, .. } = state;
    let Ok(slot) = volumes.get_mut(handle) else {
      return false;
    };
    let Ok(root) = slot.volume.root_inode(store) else {
      return false;
    };
    let Ok(file) = slot.volume.create_file_no(store, root, "written", 0o644) else {
      return false;
    };
    slot.volume.write(store, file, 0, &[7u8; 4096]).is_ok()
  }
}
