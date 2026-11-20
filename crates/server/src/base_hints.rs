//! Base hints drained when the kernel reports them (§4.5, §4.15; RFC 8881 §10.2; the event-driven reaper, A-709). Each
//! overlay's host directory has a watcher (inotify on Linux, a kqueue on macOS, `slates_base`); the shard enters every
//! watcher's descriptor in one set of its own (`slates_base::HintSet`, an epoll or a kqueue), keyed by its volume's
//! handle, and one task, the hint watcher, awaits that set's readability through the shard's driver. When it is ready
//! the watcher drains the ready volumes' hints (a changed base directory has its listing invalidated, its witnessed
//! entries re-checked for drift, and every delegation held on an untouched entry in it asked to be recalled), then sends
//! the recalls the hints asked for (`crate::delegation::drain`), so an outsider's edit reaches a delegation holder
//! within the kernel's report and a round trip. Until A-709 the reap loop's tick drained every overlay's watcher once a
//! liveness window, whether any had hints or not, and an edit waited up to a window for it.
//!
//! **A key names a volume by its generational handle**, so a volume destroyed after its watcher was reported is a stale
//! handle, refused as one, never another volume's; and its watcher's descriptor, closed with its host, leaves the set
//! with it (the set's module doc). No task waits on a watcher's own descriptor number, which a destroy closes and the
//! kernel may reuse: the reason the per-volume task of the 2026-10-08 plan was rejected.
//!
//! **A watcher reports once, then is armed again once drained.** A volume whose host a landing has lent out
//! (`crate::landing`) cannot be drained, so it is owed: kept here and not armed, and drained as soon as a state borrow
//! finds its host back ([`after_borrow`]). A watcher with hints left after one drain (the kqueue's is drained
//! `DRAIN` events a call) is reported again as soon as it is armed.
//!
//! **Where nothing reports**: a set the kernel refuses is logged and counted (`base.hint_set_refused`), and a watcher it
//! refuses to enter (`base.hint_arm_refused`); that volume's hints are then drained only by the paths that drain on use
//! (`status`, a FUSE or NFS request's coherence check), nothing polling in their place. Windows keeps no watcher (the
//! host reports none, `slates_base`), so there the watcher ends at once.

use std::collections::BTreeSet;

use slates_mem::Handle;

use crate::state::{self, ShardState, VolumeSlot};

/// Counter: overlay volumes whose watcher hints could not be processed (the host refused a re-check); the hints are
/// taken again at the watcher's next report, so none is lost, but a refusal is never silent.
#[cfg(unix)]
const BASE_HINTS_REFUSED: &str = "base.hints_refused";
/// Counter: watcher hints drained, over every overlay volume (the drain's non-vacuity counter).
#[cfg(unix)]
const BASE_HINTS_DRAINED: &str = "base.hints_drained";
/// Counter: delegation recalls an outsider's edit beneath an untouched base entry asked for (A-122).
#[cfg(unix)]
const OUTSIDE_RECALLS: &str = "nfs4.recall.outside_asked";
/// Counter: the shard's hint set refused by the kernel at its start: no watcher's hints are reported on this shard.
#[cfg(unix)]
const HINT_SET_REFUSED: &str = "base.hint_set_refused";
/// Counter: a volume's watcher the kernel refused to enter in the shard's set: its hints are not reported.
#[cfg(unix)]
const HINT_ARM_REFUSED: &str = "base.hint_arm_refused";
/// Counter: the hint watcher's wait on the set refused by the runtime: the watcher ends there, as a refused set does.
#[cfg(unix)]
const HINT_WAIT_REFUSED: &str = "base.hint_wait_refused";

/// The shard's part of the hint watcher (the module doc), kept on the shard's state.
#[derive(Debug, Default)]
pub(crate) struct BaseHints {
  /// The set of the shard's watchers, from the watcher's start; `None` before it or once the kernel refused it.
  #[cfg(unix)]
  set: Option<slates_base::HintSet>,
  /// The hint watcher, woken when an owed volume's host is back.
  task: Option<slates_rt::TaskId>,
  /// The volumes reported ready whose hosts were lent out (a landing's), by handle (index, generation): each once, so
  /// bounded by the shard's volumes; a destroyed one leaves at the next look.
  owed: BTreeSet<(u32, u32)>,
  /// Whether a borrow found an owed volume's host back since the watcher last looked.
  asked: bool,
}

/// The key a volume's watcher is entered under: its handle's index and generation.
#[cfg(unix)]
fn key_of(volume: Handle<VolumeSlot>) -> u64 {
  (u64::from(volume.index()) << u32::BITS) | u64::from(volume.generation())
}

/// The handle a key names (inverse of [`key_of`]).
#[cfg(unix)]
fn volume_of(key: u64) -> Handle<VolumeSlot> {
  let index = u32::try_from(key >> u32::BITS).unwrap_or(u32::MAX);
  let generation = u32::try_from(key & u64::from(u32::MAX)).unwrap_or(u32::MAX);
  Handle::from_raw(index, generation)
}

/// The hint watcher (A-709): one task a shard, made at its start. It opens the shard's set, enters every volume's
/// watcher (those its recovery rebuilt), then drains what each report names until the shard ends.
#[cfg(unix)]
pub(crate) async fn watcher() {
  let Some(Some(raw)) = state::with_state(open) else {
    return;
  };
  loop {
    let woke = {
      use std::future::Future;
      let mut ready = std::pin::pin!(slates_rt::readiness::readable(raw));
      std::future::poll_fn(|cx| {
        if asked() {
          return std::task::Poll::Ready(Ok(()));
        }
        ready.as_mut().poll(cx)
      })
      .await
    };
    if let Err(refusal) = woke {
      slates_machine::log_line!(
        "slates-server: the wait for the base watchers' hints was refused ({refusal}); hints are drained on use alone"
      );
      let _ = state::with_state(|s| s.count(HINT_WAIT_REFUSED, 1));
      return;
    }
    if state::with_state(drain_reported) != Some(true) {
      return;
    }
  }
}

/// No watcher reports on Windows (the module doc): the hint watcher ends at once.
#[cfg(not(unix))]
pub(crate) async fn watcher() {}

/// Opens the shard's set and enters every volume's watcher: the set's descriptor, `None` when the kernel refused it.
#[cfg(unix)]
fn open(s: &mut ShardState) -> Option<i32> {
  use std::os::fd::{AsFd, AsRawFd};
  s.base_hints.task = slates_rt::futures::current_task();
  let set = match slates_base::HintSet::open() {
    Ok(set) => set,
    Err(refusal) => {
      slates_machine::log_line!(
        "slates-server: the shard's base hint set is refused ({refusal}); hints are drained on use alone"
      );
      s.count(HINT_SET_REFUSED, 1);
      return None;
    }
  };
  let raw = set.as_fd().as_raw_fd();
  s.base_hints.set = Some(set);
  let volumes: Vec<Handle<VolumeSlot>> = s.volumes.iter().map(|(volume, _)| volume).collect();
  for volume in volumes {
    enrol(s, volume);
  }
  Some(raw)
}

/// Whether a borrow found an owed volume's host back since the watcher last looked, taken.
#[cfg(unix)]
fn asked() -> bool {
  state::with_state(|s| std::mem::take(&mut s.base_hints.asked)) == Some(true)
}

/// Enters `volume`'s watcher in the shard's set, or arms it again after a report: a volume made, cloned or recovered
/// is entered as it joins the shard's table. Nothing before the watcher opened the set (it enters every volume then),
/// for a volume with no host directory, or on Windows.
#[cfg(unix)]
pub(crate) fn enrol(s: &mut ShardState, volume: Handle<VolumeSlot>) {
  let Some(set) = s.base_hints.set.as_ref() else {
    return;
  };
  let Some(source) = s
    .volumes
    .get(volume)
    .ok()
    .and_then(|slot| slot.host.as_ref())
    .and_then(slates_base::BaseHost::hint_source)
  else {
    return;
  };
  if set.arm(source, key_of(volume)).is_err() {
    s.count(HINT_ARM_REFUSED, 1);
  }
}

/// Nothing on Windows (the module doc).
#[cfg(not(unix))]
pub(crate) fn enrol(_s: &mut ShardState, _volume: Handle<VolumeSlot>) {}

/// Drains the hints of every volume the set reports ready and of every owed one whose host is back, arms each drained
/// watcher again, then sends the recalls the hints asked for: whether the watcher goes on (false once the set refused
/// its read).
#[cfg(unix)]
fn drain_reported(s: &mut ShardState) -> bool {
  let room = s.volumes.len().max(1);
  let reported = match s.base_hints.set.as_ref().map(|set| set.ready(room)) {
    Some(Ok(reported)) => reported,
    None => Vec::new(),
    // A set the kernel will not read stays readable: the watcher ends rather than spin on it.
    Some(Err(refusal)) => {
      slates_machine::log_line!(
        "slates-server: the shard's base hint set refused its read ({refusal}); hints are drained on use alone"
      );
      s.count(HINT_WAIT_REFUSED, 1);
      return false;
    }
  };
  let owed = std::mem::take(&mut s.base_hints.owed);
  let volumes = reported.into_iter().map(volume_of).chain(
    owed
      .into_iter()
      .map(|(index, generation)| Handle::from_raw(index, generation)),
  );
  let mut drained = Drained::default();
  let recalls_before = s.store.recall_gate.outside_recalls();
  for volume in volumes.collect::<Vec<_>>() {
    drain_volume(s, volume, &mut drained);
  }
  let asked = s
    .store
    .recall_gate
    .outside_recalls()
    .saturating_sub(recalls_before);
  if drained.refused > 0 {
    s.count(BASE_HINTS_REFUSED, drained.refused);
  }
  if drained.hints > 0 {
    s.count(BASE_HINTS_DRAINED, drained.hints);
  }
  if asked > 0 {
    s.count(OUTSIDE_RECALLS, asked);
  }
  // The recalls the hints asked for are sent now: nothing else runs the drain while no call is served.
  crate::delegation::drain(s);
  true
}

#[cfg(unix)]
/// What one report's drains did.
#[derive(Debug, Default)]
struct Drained {
  /// Hints drained.
  hints: u64,
  /// Volumes whose drain the host refused.
  refused: u64,
}

/// Drains `volume`'s hints and arms its watcher again; a stale handle (the volume destroyed) is passed over, and a
/// volume whose host is lent out is owed.
#[cfg(unix)]
fn drain_volume(s: &mut ShardState, volume: Handle<VolumeSlot>, drained: &mut Drained) {
  let ShardState {
    volumes,
    store,
    base_hints,
    ..
  } = &mut *s;
  let Ok(slot) = volumes.get_mut(volume) else {
    return;
  };
  let Some(host) = slot.host.as_mut() else {
    base_hints
      .owed
      .insert((volume.index(), volume.generation()));
    return;
  };
  let seq_before = slot.volume.base_plane().map_or(0, |plane| plane.hint_seq());
  if slot.volume.with_host(host).process_hints(store).is_err() {
    drained.refused = drained.refused.saturating_add(1);
  }
  let seq_after = slot.volume.base_plane().map_or(0, |plane| plane.hint_seq());
  drained.hints = drained
    .hints
    .saturating_add(seq_after.saturating_sub(seq_before));
  enrol(s, volume);
}

/// Closes every state borrow (`state::try_with_state`): an owed volume whose host is back (its landing gave it back)
/// wakes the watcher to drain it. One read of an empty set a borrow while nothing is owed.
pub(crate) fn after_borrow(s: &mut ShardState) {
  if s.base_hints.owed.is_empty() || s.base_hints.asked {
    return;
  }
  // A destroyed volume counts as back: the watcher's next look lets it go.
  let back = s.base_hints.owed.iter().any(|(index, generation)| {
    match s.volumes.get(Handle::from_raw(*index, *generation)) {
      Ok(slot) => slot.host.is_some(),
      Err(_) => true,
    }
  });
  if back {
    s.base_hints.asked = true;
    if let Some(task) = s.base_hints.task {
      slates_rt::registry::wake(task.0);
    }
  }
}

#[cfg(all(test, unix))]
mod tests {
  use std::time::{Duration, Instant};

  use slates_ipc::protocol::{ReadAt, ReplyBody, RequestBody};

  /// Shape: how long the test looks for the drain: half a liveness window, short of the reap loop's tick, which drained
  /// every watcher once a window until A-709.
  const DRAINED_WITHIN: Duration = Duration::from_nanos(crate::daemon::LIVENESS_BUDGET_NS / 2);
  /// Shape: the pause between the test's looks, short beside the bound.
  const LOOK_PAUSE: Duration = Duration::from_millis(1);

  /// The principal the test creates and lists as.
  fn principal() -> slates_db::catalog::Principal {
    slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 0,
    }
  }

  /// Creates an overlay of `base` named `name` and lists its root, so its watcher watches the root: its id.
  fn overlay_listed(state: &mut crate::state::ShardState, name: &str, base: String) -> bool {
    let reply = crate::verbs::dispatch(
      state,
      1,
      &principal(),
      RequestBody::Create {
        name: name.to_owned(),
        size: slates_ipc::protocol::SizeClass::Bounded { limit: 1 << 20 },
        names: slates_ipc::protocol::NamePolicy::Exact,
        require_locked: false,
        base: Some(base),
      },
    );
    let ReplyBody::Created { id } = reply else {
      return false;
    };
    let listed = crate::verbs::dispatch(
      state,
      2,
      &principal(),
      RequestBody::ReadDir {
        volume: id,
        path: "/".to_owned(),
        at: ReadAt::Head,
        cursor: 0,
      },
    );
    !matches!(listed, ReplyBody::Refused { .. })
  }

  /// The hints the shard has drained so far.
  fn drained(state: &mut crate::state::ShardState) -> u64 {
    state
      .refusals
      .get(super::BASE_HINTS_DRAINED)
      .copied()
      .unwrap_or(0)
  }

  /// A-709. Do: on a one-shard daemon, overlay a scratch directory, list its root (its watcher now watches it), then
  /// write a file into the directory from outside slates and look until the shard has drained a hint. Expect: drained
  /// within half a liveness window, by the hint watcher at the kernel's report; nothing is served meanwhile, and the
  /// reap loop's tick, which drained every watcher once a window before, no longer does.
  #[test]
  #[allow(clippy::disallowed_methods)] // test code: an outsider's write into its own scratch directory
  fn an_outsiders_edit_is_drained_at_the_kernels_report_without_a_tick() {
    let dir = crate::git_hooks::tests::ScratchDir::new();
    let daemon = crate::daemon::audit_daemon_of(1);
    let base = dir.path.to_string_lossy().into_owned();
    let made =
      crate::daemon::observe_first(&daemon, move |state| overlay_listed(state, "hinted", base));
    let before = crate::daemon::observe_first(&daemon, drained);
    std::fs::write(dir.path.join("outsider.txt"), b"edit").expect("the outsider's write");
    let begun = Instant::now();
    let mut after = before;
    while after == before && begun.elapsed() < DRAINED_WITHIN {
      std::thread::sleep(LOOK_PAUSE);
      after = crate::daemon::observe_first(&daemon, drained);
    }
    daemon.stop();
    assert!(made, "the overlay was made and its root listed");
    assert!(
      after > before,
      "no hint drained within {DRAINED_WITHIN:?} of the outsider's write"
    );
    println!(
      "an outsider's edit drained {:?} after it was written",
      begun.elapsed()
    );
  }
}
