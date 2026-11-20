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
//! busy arena, a give-back that left slices) arms the next one; a pass that settles it arms nothing, so an idle shard
//! is never woken for the sweep. While a pass is asked for or runs nothing arms another.
//!
//! **The aging rule is unchanged**: a file is sealed one to two intervals after its last write (`Volume::age_writes`),
//! as ZFS commits a transaction group and Linux writes back a page past `dirty_expire_centisecs`; the purge still waits
//! for a pass that finds the arena allocated nothing since the one before (A-105).
//!
//! **The first pass** is armed when the shard starts, a sweep's interval on, for what a restart left: the recovery
//! memory no restart reads, given back by an idle pass (A-117).

use slates_vfs::clock::Clock;

use crate::state::{self, ShardState};

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
/// keeper asks for one, arming the next while the shard is not settled.
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
    let settled = crate::daemon::seal_idle_content().await;
    if state::with_state(|s| finish(s, settled)).is_none() {
      return;
    }
  }
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
  if written || arena_seen(s) != s.idle_sweep_seen || s.store.content.arena().deferred_bytes() > 0 {
    arm(s);
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
