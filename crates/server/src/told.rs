//! A writer outside FUSE is answered once every FUSE mount of its volume has been told of its change (AC-3.3; §4.6
//! "Cache posture").
//!
//! **What it closes.** A FUSE mount's kernel keeps names, attributes and pages for as long as the daemon tells it when
//! they change (`Invalidated`, §4.6), and since 2026-10-21 a mount's task tells its kernel as soon as another path
//! changes the volume. That was still after the writer's reply: a shared map read an SDK write's bytes 52–193 µs after
//! the reply, median 82 µs (Linux, 51 replacements;
//! `docs/bugs/2026-10-21-a-fuse-mount-hears-of-another-paths-change-only-on-its-next-request.md`). So a reader of the
//! mount right after the write returned could see the old bytes, which AC-3.3 forbids.
//!
//! **The rule.** A writer that holds no kernel lock notes its volume's journal head before it runs ([`mark`]). That is
//! a verb over the typed channel (the SDKs, the CLI, MCP, and the merge plane's verbs, all run by `verbs::run_recorded`,
//! or late by `crate::lease_wait` once their lease confirms), an NFS call, or a direct map's write-back. Once the writer
//! has run and published, its reply owes every FUSE mount of
//! the volume the newest record it made ([`owed`]). Each mount must have been told through that record: the
//! notification round carrying it written to the mount's device and back
//! (`slates_bridge_fuse::channel::ServeState::told_through`). The reply waits for that on the volume's owner shard,
//! where the volume and all its FUSE mounts live, parked in the shard's [`ToldWaiters`]. A mount's task wakes the
//! parked writers whenever its kernel is told further or the mount ends (`crate::fuse`). The shard serves on
//! meanwhile:
//! - a verb's reply is held and sent by a task of the shard ([`hold_until_told`]);
//! - an NFS call's task awaits [`wait`] before its reply is queued;
//! - a verb forwarded from another node awaits [`wait`] on the owner before its exchange answers;
//! - a guest's request through slates' own virtio-fs device is held by its device, which serves the guest's later
//!   requests meanwhile and waits on `parked`, a Unix item, while idle (`crate::virtiofs`; A-196);
//! - a guest's request over a Shards stream, whose VMM asks one request at a time, waits in its stream's task for
//!   [`wait`] before its reply is sent (`crate::shards`; A-196).
//!
//! **Why a FUSE writer does not wait.** A request through one FUSE mount holds its kernel's locks on what it touches —
//! the pages of a read or a write, the directory of a lookup — until the daemon answers it. Another mount's notification
//! needs that mount's own locks. So two mounts each with a request in flight on one file, each request's reply waiting
//! for the other mount's notification, would wait on each other until the bound: the deadlock the 2026-10-17 amendment
//! found on one mount, across two. A write through a FUSE mount is answered at once, and the other mounts of its volume
//! are told just after, as before: AC-3.3's text states that window.
//!
//! **The bound.** A mount whose round cannot complete — its notification waiting in its kernel on a request the shard
//! cannot answer yet, a kernel that never processes it — must not hold its writers for ever (A-141 weighed exactly this).
//! A writer waits at most [`bound_ns`], then is answered anyway, its change already applied and published, and the late
//! mount counted (`fuse.told_overdue`, zero the healthy reading). The bound is learned (A-431), as RFC 6298 learns a
//! retransmission timeout: the tail of the told waits the shard measured, plus a liveness window, the lateness a live
//! party may show before it is judged (A-228); a wait cut short at its bound is not a sample (Karn's rule) but doubles the
//! bound for the next (RFC 6298 §5.5), up to the node's failover delay, the most a reply is held (A-354, A-375); the next
//! wait that completes ends the back-off. So a machine whose mounts are told slowly is waited for at its own pace, and a
//! mount that never answers holds each writer at most a failover delay. Until A-431 the bound was half a liveness window
//! on the wall, so that a writer held to it kept half its client's reply deadline; since 2026-11-13 a client waits for a
//! live daemon's answer however long (A-232).
//!
//! **What waits on nothing.** A writer that made no record, a volume with no FUSE mount, a mount whose kernel holds
//! nothing yet, and every platform but Linux (its FUSE mounts are the only kernel caches told this way; [`told`]).
//!
//! **Measured** (fedora-2, Linux 6.19, 2026-10-30; `crates/server/tests/fuse_told.rs`, `docs/wip/BENCHMARKS.md`). With
//! two FUSE mounts mapping one file, no read after an SDK write or an NFS WRITE returned was stale in 200 of each;
//! without the wait 7 of 200 and 58 of 200 were. The wait costs a write to a volume a FUSE mount reads about 12 µs at
//! the median (p50 28 → 40 µs, p90 35 → 53 µs) and a write to a volume with none nothing.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_db::catalog::VolumeId;
use slates_ipc::protocol::ReplyBody;

use crate::merge_service::ReplyRoute;
use crate::state::{self, ShardState};

/// The told waits a shard measured (AC-3.3, A-431): the shared learner (`crate::learned_wait::LearnedWait`, A-442),
/// per shard, so a busy host's mounts, told late but told, are waited for at their pace.
pub(crate) type ToldTimes = crate::learned_wait::LearnedWait;

/// The bound a writer on this shard waits for its volume's FUSE mounts now (`LearnedWait::bound_ns`), backed off no
/// further than the node's failover delay (`ShardState::failover_ns`, A-363): a reply held for its mounts is a held
/// reply, whose total is the failover delay (A-354, A-375). Read by every writer's wait ([`wait`]), a guest device's held
/// reply (`crate::virtiofs`, A-196) and a revocation's wait for its fenced mounts (`crate::verbs`).
pub(crate) fn bound_ns(s: &ShardState) -> u64 {
  s.told_times.bound_ns(s.failover_ns)
}

/// Format: the counter of writers whose reply waited for their volume's FUSE mounts to be told (AC-3.3): the wait's
/// non-vacuity count.
const WAITED: &str = "fuse.told_waits";
/// Format: the counter of waits that reached their bound ([`bound_ns`]) with a FUSE mount still untold, the writer answered
/// anyway. Zero is the healthy reading; movement means a mount's notifications stalled.
const OVERDUE: &str = "fuse.told_overdue";
/// Format: the counter of writers answered without waiting although a FUSE mount had not been told: no task could be
/// spawned, the held replies were at their bound, or no deadline could be armed. Zero is the healthy reading.
const UNWAITED: &str = "fuse.told_unwaited";

/// Where a writer's volume stood in its journal before the writer ran ([`mark`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
  volume: VolumeId,
  before: u64,
}

/// What a writer's reply waits on: its volume, and the newest journal record it made ([`owed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owed {
  volume: VolumeId,
  seq: u64,
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
  /// Every FUSE mount of the volume was told.
  Told,
  /// The bound ([`bound_ns`]) passed first, or no deadline could be armed.
  Overdue,
}

/// The writers parked on this shard's volumes' FUSE mounts (AC-3.3): each wait's waker by volume, and each held reply.
///
/// Bounded. A wait's waker is entered while it waits and removed when it ends, told or overdue, so the waits are bounded
/// by the tasks waiting, each a slot of the shard's bounded task arena. The held replies are bounded by the shard's
/// client credit ([`held_bound`]); a reply past it is answered at once, counted.
#[derive(Debug, Default)]
pub(crate) struct ToldWaiters {
  /// The next id a wait or a held reply takes.
  next: u64,
  /// The waits parked on each volume, by wait id.
  parked: BTreeMap<VolumeId, BTreeMap<u64, Waker>>,
  /// The replies held until their volume's FUSE mounts are told, by the id of the task that sends each.
  held: BTreeMap<u64, (ReplyRoute, ReplyBody)>,
}

impl ToldWaiters {
  /// A fresh id: ids only identify entries while they live, so wrapping is harmless.
  fn take_id(&mut self) -> u64 {
    let id = self.next;
    self.next = self.next.wrapping_add(1);
    id
  }
}

/// The most replies a shard holds for its FUSE mounts: its client credit (`clients_per_shard × slots`), every verb that
/// can be in flight from its clients, the bound the lease waiters and the forwarded rings keep.
fn held_bound(s: &ShardState) -> usize {
  s.config
    .clients_per_shard
    .saturating_mul(usize::try_from(s.config.region.slots).unwrap_or(1))
}

/// Where `volume` stands in its journal now, before a writer runs: `None` when the volume does not live on this shard.
pub(crate) fn mark(s: &ShardState, volume: VolumeId) -> Option<Mark> {
  let &handle = s.by_id.get(&volume)?;
  let slot = s.volumes.get(handle).ok()?;
  Some(Mark {
    volume,
    before: slot.volume.op_log().head_seq(),
  })
}

/// What the reply of a writer that ran since `mark` owes: the newest record of its volume, when the writer made one and a
/// FUSE mount of the volume has not yet been told through it. `None` when nothing is owed.
pub(crate) fn owed(s: &ShardState, mark: Mark) -> Option<Owed> {
  let &handle = s.by_id.get(&mark.volume)?;
  let seq = s.volumes.get(handle).ok()?.volume.op_log().head_seq();
  let owed = Owed {
    volume: mark.volume,
    seq,
  };
  (seq > mark.before && !told(s, owed)).then_some(owed)
}

/// Whether every FUSE mount of the owed volume has been told through its record: a mount that ended, or one whose kernel
/// holds nothing yet, owes nothing.
pub(crate) fn told(s: &ShardState, owed: Owed) -> bool {
  mounts_told(s, owed)
}

/// Whether every FUSE mount of `volume` has been told through journal record `seq` ([`told`]): what a guest device's
/// held reply asks (A-196).
#[cfg(unix)]
pub(crate) fn told_through(s: &ShardState, volume: VolumeId, seq: u64) -> bool {
  told(s, Owed { volume, seq })
}

/// A wait on this shard, the volume's owner, until every FUSE mount of `volume` has been told through journal record
/// `seq`, holding no borrow; dropping it leaves nothing parked. Unbounded: a guest device races it with its held reply's
/// bound (A-196).
#[cfg(unix)]
pub(crate) fn parked(volume: VolumeId, seq: u64) -> Parked {
  Parked {
    owed: Owed { volume, seq },
    id: None,
  }
}

/// Counts a guest device's settled replies on the writers' ledger (A-196): each a wait, and each published at the bound
/// overdue as well, as [`wait`] counts a writer's.
#[cfg(unix)]
pub(crate) fn count_settled(s: &mut ShardState, waited: u64, overdue: u64) {
  s.count(WAITED, waited.saturating_add(overdue));
  if overdue > 0 {
    s.count(OVERDUE, overdue);
    // A device's reply published at the bound backs it off as a writer's overdue wait does; its told replies carry no
    // duration, so they are no samples.
    s.told_times.ran_out();
  }
}

/// Linux: asked of the shard's FUSE mounts.
#[cfg(target_os = "linux")]
fn mounts_told(s: &ShardState, owed: Owed) -> bool {
  crate::fuse::every_mount_told_through(s, owed.volume, owed.seq)
}

/// Elsewhere there is no FUSE mount: nothing is ever owed.
#[cfg(not(target_os = "linux"))]
fn mounts_told(_s: &ShardState, _owed: Owed) -> bool {
  true
}

/// Wakes every writer parked on `volume`: one of its FUSE mounts was told further, or ended. Each woken wait asks again
/// and parks again if its own record is still untold. Called by `crate::fuse`, which only Linux builds.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn wake(s: &ShardState, volume: VolumeId) {
  if let Some(parked) = s.told_waiters.parked.get(&volume) {
    for waker in parked.values() {
      waker.wake_by_ref();
    }
  }
}

/// Waits on this shard — the volume's owner — until every FUSE mount of the owed volume has been told, or the bound
/// ([`bound_ns`]) has passed, without blocking the shard. Counted, and learned from: a told wait is a sample of the
/// shard's told times, an overdue one backs the bound off ([`ToldTimes`]).
pub(crate) async fn wait(owed: Owed) -> Waited {
  let bound = state::with_state(|s| bound_ns(s)).unwrap_or(crate::daemon::LIVENESS_BUDGET_NS);
  let began = slates_rt::futures::now_ns();
  let late = match slates_rt::futures::within(bound, Parked { owed, id: None }).await {
    Ok(Some(())) => None,
    Ok(None) => Some(OVERDUE),
    // No deadline could be armed (off a shard): the writer is answered at once.
    Err(_) => Some(UNWAITED),
  };
  let waited_ns = slates_rt::futures::now_ns().saturating_sub(began);
  let _ = state::with_state_counted(|s| {
    s.count(WAITED, 1);
    match late {
      None => s.told_times.completed(waited_ns),
      Some(OVERDUE) => s.told_times.ran_out(),
      Some(_) => {}
    }
    if let Some(kind) = late {
      s.count(kind, 1);
    }
  });
  match late {
    None => Waited::Told,
    Some(_) => Waited::Overdue,
  }
}

/// Holds `reply` until every FUSE mount of the owed volume has been told, then sends it where `route` names, from a task
/// of this shard, so the shard serves on meanwhile: `None`. When the reply cannot be held — the held replies at their
/// bound, or the shard's task arena refusing the task — the route and the reply come back for the caller to answer at
/// once, counted.
pub(crate) fn hold_until_told(
  s: &mut ShardState,
  owed: Owed,
  route: ReplyRoute,
  reply: ReplyBody,
) -> Option<(ReplyRoute, ReplyBody)> {
  if s.told_waiters.held.len() >= held_bound(s) {
    s.count(UNWAITED, 1);
    return Some((route, reply));
  }
  let id = s.told_waiters.take_id();
  s.told_waiters.held.insert(id, (route, reply));
  let here = s.shard;
  let task = async move {
    let _ = wait(owed).await;
    let held = state::with_state_counted(|s| s.told_waiters.held.remove(&id)).flatten();
    if let Some((route, reply)) = held {
      deliver_to(route, reply, here);
    }
  };
  match slates_rt::futures::spawn(task).and_then(slates_rt::futures::detach) {
    Ok(()) => None,
    Err(_) => {
      s.count(UNWAITED, 1);
      // The entry was inserted above and only the task, which never ran, removes it.
      s.told_waiters.held.remove(&id)
    }
  }
}

/// Delivers a held reply where its request waits: on this shard's ring when its client is served here, else as a task
/// on the client's shard, waiting for room as every reply carried home does (`crate::xshard::send_back`).
pub(crate) fn deliver_to(route: ReplyRoute, reply: ReplyBody, here: u16) {
  if route.shard == here {
    state::deliver(route.client_index, route.request, reply, true);
    return;
  }
  crate::xshard::send_reply_home(route.shard, move || {
    state::deliver(route.client_index, route.request, reply, true);
  });
}

/// One writer's wait: ready once every FUSE mount of the owed volume has been told; parked in the shard's
/// [`ToldWaiters`] under `id` until then, and removed from them when dropped, so a wait that ends at its deadline leaves
/// no waker behind.
pub(crate) struct Parked {
  owed: Owed,
  id: Option<u64>,
}

impl std::future::Future for Parked {
  type Output = ();

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    let (owed, id) = (self.owed, self.id);
    // `None`: the shard's state is out of reach (its shutdown), and there is no mount left to wait for.
    let parked = state::with_state_counted(|s| park(s, owed, id, cx.waker())).flatten();
    match parked {
      Some(id) => {
        self.id = Some(id);
        Poll::Pending
      }
      None => Poll::Ready(()),
    }
  }
}

impl Drop for Parked {
  fn drop(&mut self) {
    if let Some(id) = self.id.take() {
      let volume = self.owed.volume;
      let _ = state::with_state_counted(|s| unpark(s, volume, id));
    }
  }
}

/// Parks `waker` under `id` (a fresh id when `None`) unless every FUSE mount of the owed volume has been told: the id it
/// waits under, or `None` when it need not wait. A wait that was parked is removed when it is dropped ([`Parked`]).
fn park(s: &mut ShardState, owed: Owed, id: Option<u64>, waker: &Waker) -> Option<u64> {
  if told(s, owed) {
    return None;
  }
  let id = id.unwrap_or_else(|| s.told_waiters.take_id());
  s.told_waiters
    .parked
    .entry(owed.volume)
    .or_default()
    .insert(id, waker.clone());
  Some(id)
}

/// Parks `waker` on `volume`'s FUSE mounts under `id` (a fresh id when `None`), to be woken whenever one of them is told
/// further or ends ([`wake`]): the id it waits under. A wait for something else of the mounts than a record told (a
/// revoked mount's end, `crate::fuse::revoked_mounts_ended`), which removes itself with [`unpark`] when dropped.
#[cfg(target_os = "linux")]
pub(crate) fn park_on(s: &mut ShardState, volume: VolumeId, id: Option<u64>, waker: &Waker) -> u64 {
  let id = id.unwrap_or_else(|| s.told_waiters.take_id());
  s.told_waiters
    .parked
    .entry(volume)
    .or_default()
    .insert(id, waker.clone());
  id
}

/// Removes wait `id` from `volume`'s parked writers, and the volume's entry once none is left.
pub(crate) fn unpark(s: &mut ShardState, volume: VolumeId, id: u64) {
  if let Some(parked) = s.told_waiters.parked.get_mut(&volume) {
    parked.remove(&id);
    if parked.is_empty() {
      s.told_waiters.parked.remove(&volume);
    }
  }
}
