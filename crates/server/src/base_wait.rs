//! A request that meets a pending base waits for the fetch it met (§4.15 "An S3 base", A-555).
//!
//! **What it closes.** An S3 base's host answers from what it has fetched; a call it cannot answer yet names the fetch
//! it waits on (`HostError::Pending(FetchId)`, `VfsError::BasePending`), and the request is refused having changed
//! nothing. Until A-555 a FUSE request was then answered `EIO` and an NFS call `NFS3ERR_SERVERFAULT`, so a first read of
//! a base file, or a first lookup in a base directory, failed. Now the request waits on the volume's owner shard for
//! that fetch's own landing and is served again then, as a delegated file's NFSv3 call waits for its recall (A-79) and a
//! lease read for its confirmation (`crate::lease_wait`).
//!
//! **One fetch, many waits.** The host asks a fetch once however many calls miss on it and names it alike to each, so
//! every request that met it parks under that one name, keyed by its volume ([`BaseWaits`]), and the fetch's landing
//! ([`land`]) wakes those requests alone, never one parked on another fetch. A woken request runs again from its start:
//! its pending answer changed nothing, so running it again is running it once. Which fetch a request met is noted where
//! the slot's host is lent to it ([`Lens`]), since the transports' bridges answer the refusal on their own wire.
//!
//! **What it finds.** The host holds what a fetch landed while requests are parked on it (`S3Host::hold_for_waiters`
//! from the first wait's parking, `S3Host::release_held` at the last one's leaving), so a woken request finds what it
//! waited for whatever lands meanwhile; a fetch that settled before a request could park holds nothing for it, and the
//! request runs again at once. Without the hold, concurrent misses through a small cache shed each other's landed runs
//! before their requests ran again (GAPS, A-464's owed hold).
//!
//! **The bounds.** The waits a shard parks are bounded by its client credit (`clients_per_shard × slots`), the bound the
//! lease waiters, the told replies and the parked work verbs keep: a request past it is answered at once with its own
//! pending refusal, the transport's retry (`NFS3ERR_JUKEBOX`, `EAGAIN`), counted ([`FULL`]). A request waits at most the
//! node's failover delay in all (`ShardState::failover_ns`, the most a reply is held, A-354, A-375), however many
//! fetches it meets in turn, and is then answered the same, counted ([`OVERDUE`]); each fetch is itself bounded by the
//! store client's learned waits and retries (A-471), so a store that does not answer settles its fetch with a refusal,
//! which the request is answered, well before that.
//!
//! **Who lands a fetch.** The volume's fetch driver (`slates_base::s3::driver`), a task of its owner shard started with
//! the volume ([`spawn_driver`]): it sleeps until the host wants a fetch, runs it against the store, and hands the answer
//! to [`land`]; it ends when the host does.
//!
//! **Where it waits.** On the volume's owner shard, where the host, its fetches and the waits live: an NFS call parks in
//! its connection's task, or in the owner's task of a call forwarded from another shard (`crate::nfs`); a FUSE request
//! is kept off the device by its mount, a task of the shard waiting for its fetch, and dispatched again before the
//! device is read once that fetch lands (`crate::fuse`'s `keep`, Linux), the kernel's other requests served meanwhile.
//! A volume over a host directory never waits: its host never answers pending.

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_base::BaseHost;
use slates_base::s3::client::S3Error;
use slates_base::s3::driver::{HostCell, drive};
use slates_base::s3::fetch::{Fetch, Fetched, Store};
use slates_base::s3::host::S3Host;
use slates_db::catalog::VolumeId;
use slates_vfs::host::{
  BaseEntry, FetchId, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, WatchState,
};
use slates_vfs::inode::Fingerprint;

use crate::state::{self, ShardState};

/// Format: the counter of requests parked on a pending base's fetch: the wait's non-vacuity count.
const PARKED: &str = "base.wait.parked";
/// Format: the counter of parked requests run again once the fetch they waited on landed.
const LANDED: &str = "base.wait.landed";
/// Format: the counter of requests answered their pending refusal at once because the shard's waits were at their bound
/// ([`bound`]). Zero is the healthy reading.
const FULL: &str = "base.wait.full";
/// Format: the counter of requests answered their pending refusal because their wait reached its deadline with a fetch
/// still under way, or no deadline could be armed. Zero is the healthy reading.
const OVERDUE: &str = "base.wait.overdue";

/// What a wait is keyed by: the volume whose base host names the fetch, and the fetch's name in that host.
pub(crate) type Key = (VolumeId, FetchId);

/// The requests a shard has parked on its volumes' base fetches (A-555).
///
/// Bounded: a wait is entered while its request waits and removed when it ends, landed, overdue or dropped, and the
/// waits are at most the shard's client credit ([`bound`]).
#[derive(Debug, Default)]
pub(crate) struct BaseWaits {
  /// Each fetch's parked waits, by wait id.
  parked: BTreeMap<Key, BTreeMap<u64, Waker>>,
  /// The fetches that landed while waits were parked on them, each until the last of those waits has left.
  landed: BTreeSet<Key>,
  /// How many waits are parked: what the bound counts.
  waits: usize,
  /// The id the next wait takes.
  next: u64,
}

/// What a parked wait finds when its request looks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Looked {
  /// Its fetch landed: the request runs again.
  Landed,
  /// Its fetch is still under way: the wait stays parked.
  Waiting,
}

impl BaseWaits {
  /// How many waits are parked now.
  #[cfg(test)]
  pub(crate) fn waits(&self) -> usize {
    self.waits
  }

  /// Whether another wait would pass `bound`.
  pub(crate) fn full(&self, bound: usize) -> bool {
    self.waits >= bound
  }

  /// Parks `waker` on the fetch `key` names as a new wait: its id. The caller has checked the bound
  /// ([`BaseWaits::full`]) and that the fetch is still under way, which asked the host to hold what it lands.
  pub(crate) fn park(&mut self, key: Key, waker: &Waker) -> u64 {
    let id = self.next;
    // Ids only identify a wait while it is parked, so wrapping is harmless.
    self.next = self.next.wrapping_add(1);
    self
      .parked
      .entry(key)
      .or_default()
      .insert(id, waker.clone());
    self.waits = self.waits.saturating_add(1);
    id
  }

  /// Whether wait `id`'s fetch has landed; when it has not, the wait stays parked under `waker`, its newest.
  pub(crate) fn look(&mut self, key: Key, id: u64, waker: &Waker) -> Looked {
    if self.landed.contains(&key) {
      return Looked::Landed;
    }
    if let Some(parked) = self
      .parked
      .get_mut(&key)
      .and_then(|waits| waits.get_mut(&id))
      && !parked.will_wake(waker)
    {
      *parked = waker.clone();
    }
    Looked::Waiting
  }

  /// Removes wait `id` from the fetch `key` names: whether it was that fetch's last, whose leaving lets the host's hold
  /// of what the fetch landed go.
  pub(crate) fn leave(&mut self, key: Key, id: u64) -> bool {
    let Some(waits) = self.parked.get_mut(&key) else {
      return false;
    };
    if waits.remove(&id).is_none() {
      return false;
    }
    self.waits = self.waits.saturating_sub(1);
    if !waits.is_empty() {
      return false;
    }
    self.parked.remove(&key);
    self.landed.remove(&key);
    true
  }

  /// The fetch `key` names landed: every wait parked on it is woken, and finds it landed when it looks: how many were
  /// woken. A wait parked on any other fetch is not woken.
  pub(crate) fn land(&mut self, key: Key) -> usize {
    let Some(waits) = self.parked.get(&key) else {
      return 0;
    };
    self.landed.insert(key);
    for waker in waits.values() {
      waker.wake_by_ref();
    }
    waits.len()
  }
}

/// Whether the shard's waits are at their bound ([`bound`]): a request that meets a pending base now is answered the
/// retry rather than kept (the FUSE transport asks before it keeps a request, A-555).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn full(s: &ShardState) -> bool {
  s.base_waits.full(bound(s))
}

/// The most waits a shard parks: its client credit (`clients_per_shard × slots`), every request that can be in flight
/// from its clients, the bound the lease waiters, the told replies and the parked work verbs keep.
fn bound(s: &ShardState) -> usize {
  s.config
    .clients_per_shard
    .saturating_mul(usize::try_from(s.config.region.slots).unwrap_or(1))
}

/// The shard's waits, and the S3 host of `volume`'s slot when the volume lives on this shard and its base is an S3 base:
/// borrowed apart, so a wait's step reaches both.
fn waits_and_host(s: &mut ShardState, volume: VolumeId) -> (&mut BaseWaits, Option<&mut S3Host>) {
  let ShardState {
    base_waits,
    volumes,
    by_id,
    ..
  } = s;
  let host = by_id
    .get(&volume)
    .and_then(|&handle| volumes.get_mut(handle).ok())
    .and_then(|slot| slot.host.as_mut())
    .and_then(BaseHost::as_s3_mut);
  (base_waits, host)
}

/// The store's `answer` to the fetch `ticket` of `volume`'s base host, which took `elapsed_ns`: handed to the host, and
/// every request parked on the fetch it settled woken to run again (A-555). How many were woken: none when it settled
/// no fetch (a listing's page that asks for the next), when nothing waited on it, or when the volume or its S3 base is
/// gone. The shard's driver of the host's fetches calls it as each answer arrives.
#[allow(dead_code)] // Reached through the fetch driver alone, which the S3 volume's create starts (A-555's next piece).
pub(crate) fn land(
  s: &mut ShardState,
  volume: VolumeId,
  ticket: u64,
  answer: Result<Fetched, S3Error>,
  elapsed_ns: u64,
) -> usize {
  let (waits, host) = waits_and_host(s, volume);
  land_on(waits, host, (volume, ticket), answer, elapsed_ns)
}

/// The shard's home for the fetch driver of `volume`'s S3 base (`slates_base::s3::driver`): the volume's slot holds the
/// host, and each answer goes to [`land`], which wakes the requests waiting on the fetch it settles.
#[allow(dead_code)] // Made by `spawn_driver` alone, which the S3 volume's create calls (A-555's next piece).
struct ShardCell {
  volume: VolumeId,
}

impl HostCell for ShardCell {
  fn next_fetch(&mut self, waker: &Waker) -> Poll<Option<(u64, Fetch)>> {
    let volume = self.volume;
    // `None`: the shard's state is out of reach (its shutdown), or the volume or its S3 base is gone: the driver ends.
    let next = state::with_state(|s| {
      let (_, host) = waits_and_host(s, volume);
      host.map(|host| host.next_fetch_or_wait(waker))
    })
    .flatten();
    match next {
      None => Poll::Ready(None),
      Some(Some(fetch)) => Poll::Ready(Some(fetch)),
      Some(None) => Poll::Pending,
    }
  }

  fn land(&mut self, ticket: u64, answer: Result<Fetched, S3Error>, elapsed_ns: u64) {
    let volume = self.volume;
    let _ = state::with_state_counted(|s| land(s, volume, ticket, answer, elapsed_ns));
  }
}

/// Starts the fetch driver of `volume`'s S3 base on this shard, the volume's owner, over `store` (the volume's store
/// client): it runs the host's fetches, one at a time, until the host is gone, its own end (the host wakes it as it is
/// dropped), so the task is detached and needs no other owner. Refused off a shard, or when the shard's task arena is
/// full.
#[allow(dead_code)] // Its caller is the S3 volume's create and recovery (A-555's next piece, GAPS A-464).
pub(crate) fn spawn_driver<S: Store + 'static>(
  volume: VolumeId,
  store: S,
) -> Result<(), slates_rt::RtError> {
  let driver = drive(
    store,
    ShardCell { volume },
    slates_machine::clock::monotonic_ns,
  );
  slates_rt::futures::spawn(driver).and_then(slates_rt::futures::detach)
}

/// [`land`]'s step on the shard's `waits` and `host`, the S3 host of `volume`'s slot.
fn land_on(
  waits: &mut BaseWaits,
  host: Option<&mut S3Host>,
  (volume, ticket): (VolumeId, u64),
  answer: Result<Fetched, S3Error>,
  elapsed_ns: u64,
) -> usize {
  match host.and_then(|host| host.landed(ticket, answer, elapsed_ns)) {
    Some(fetch) => waits.land((volume, fetch)),
    None => 0,
  }
}

/// How a request's wait for its fetch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
  /// The fetch landed, or had settled before the request could park: the request runs again.
  Landed,
  /// The shard's waits were at their bound: the request is answered its pending refusal now.
  Full,
  /// The deadline passed first, or none could be armed: the request is answered its pending refusal now.
  Overdue,
}

/// Waits on this shard, the volume's owner, until the fetch `key` names lands, or until `until_ns` (on the runtime's
/// clock), without blocking the shard: [`Waited::Landed`] when the request should run again. Counted.
pub(crate) async fn wait(key: Key, until_ns: u64) -> Waited {
  let now = slates_rt::futures::now_ns();
  let waited = if now >= until_ns {
    Waited::Overdue
  } else {
    match slates_rt::futures::within(until_ns.saturating_sub(now), Parked { key, id: None }).await {
      Ok(Some(waited)) => waited,
      // The deadline passed, or no deadline could be armed (off a shard): answered now.
      Ok(None) | Err(_) => Waited::Overdue,
    }
  };
  let counted = match waited {
    Waited::Landed => LANDED,
    Waited::Full => FULL,
    Waited::Overdue => OVERDUE,
  };
  let _ = state::with_state_counted(|s| s.count(counted, 1));
  waited
}

/// One request's wait: ready once the fetch it met has landed, or at once when that fetch had settled before it could
/// park or the shard's waits are at their bound; parked in the shard's [`BaseWaits`] until then and removed from them
/// when it ends or is dropped, letting the host's hold go when it was the fetch's last.
struct Parked {
  key: Key,
  id: Option<u64>,
}

/// What one look at a wait decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
  /// Parked now, under this id.
  Parked(u64),
  /// Still parked.
  Waiting,
  /// Done, with this end.
  Ended(Waited),
}

impl std::future::Future for Parked {
  type Output = Waited;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Waited> {
    let (key, id) = (self.key, self.id);
    // `None`: the shard's state is out of reach (its shutdown); the request runs again and meets what is left.
    let step = state::with_state_counted(|s| look_or_park(s, key, id, cx.waker()))
      .unwrap_or(Step::Ended(Waited::Landed));
    match step {
      Step::Parked(id) => {
        self.id = Some(id);
        Poll::Pending
      }
      Step::Waiting => Poll::Pending,
      Step::Ended(waited) => {
        // A landed wait has already left the shard's waits.
        self.id = None;
        Poll::Ready(waited)
      }
    }
  }
}

impl Drop for Parked {
  fn drop(&mut self) {
    if let Some(id) = self.id.take() {
      let key = self.key;
      let _ = state::with_state_counted(|s| {
        let (waits, host) = waits_and_host(s, key.0);
        leave_releasing(waits, host, key, id);
      });
    }
  }
}

/// [`step`] on this shard: its waits, the S3 host of the key's volume and its bound; a wait parked now counted.
fn look_or_park(s: &mut ShardState, key: Key, id: Option<u64>, waker: &Waker) -> Step {
  let budget = bound(s);
  let (waits, host) = waits_and_host(s, key.0);
  let stepped = step(waits, host, (key, id, waker), budget);
  if matches!(stepped, Step::Parked(_)) {
    s.count(PARKED, 1);
  }
  stepped
}

/// Looks at wait `id` of the fetch `key` names, or parks a new one when `id` is `None`, against `host`, the S3 host of
/// the key's volume: a new wait is parked only within `bound` and while the fetch is still under way, which asks the
/// host to hold what it lands; a wait whose fetch landed leaves, letting that hold go when it was the fetch's last.
fn step(
  waits: &mut BaseWaits,
  host: Option<&mut S3Host>,
  (key, id, waker): (Key, Option<u64>, &Waker),
  bound: usize,
) -> Step {
  if let Some(id) = id {
    return match waits.look(key, id, waker) {
      Looked::Waiting => Step::Waiting,
      Looked::Landed => {
        leave_releasing(waits, host, key, id);
        Step::Ended(Waited::Landed)
      }
    };
  }
  if waits.full(bound) {
    return Step::Ended(Waited::Full);
  }
  if !host.is_some_and(|host| host.hold_for_waiters(key.1)) {
    return Step::Ended(Waited::Landed);
  }
  Step::Parked(waits.park(key, waker))
}

/// Removes wait `id` of the fetch `key` names, letting `host`'s hold of what that fetch landed go when it was the last.
fn leave_releasing(waits: &mut BaseWaits, host: Option<&mut S3Host>, key: Key, id: u64) {
  if waits.leave(key, id)
    && let Some(host) = host
  {
    host.release_held(key.1);
  }
}

/// The slot's base host lent to one request (A-555): every call goes to the host, and the first fetch one of them was
/// answered pending on is noted, so the request parks on that fetch when its answer is the pending refusal. The
/// transports' bridges answer a volume refusal on their own wire, so this is where the fetch's name is still known.
pub(crate) struct Lens<'h> {
  host: &'h mut dyn HostFs,
  met: Option<FetchId>,
}

impl<'h> Lens<'h> {
  /// A lens over `host`, nothing met yet.
  pub(crate) fn over(host: &'h mut dyn HostFs) -> Lens<'h> {
    Lens { host, met: None }
  }

  /// The first fetch a call through the lens was answered pending on.
  pub(crate) fn met(&self) -> Option<FetchId> {
    self.met
  }

  /// Notes the fetch `answer` names when it is the first pending answer, and passes it on.
  fn note<T>(&mut self, answer: Result<T, HostError>) -> Result<T, HostError> {
    if let Err(HostError::Pending(fetch)) = &answer
      && self.met.is_none()
    {
      self.met = Some(*fetch);
    }
    answer
  }
}

impl HostFs for Lens<'_> {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    let answer = self.host.facts(dir);
    self.note(answer)
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    let answer = self.host.fingerprint_dir(dir);
    self.note(answer)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    let answer = self.host.list(dir);
    self.note(answer)
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    let answer = self.host.open_dir(parent, name);
    self.note(answer)
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let answer = self.host.open_file(dir, name);
    self.note(answer)
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    let answer = self.host.fstat(file);
    self.note(answer)
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    let answer = self.host.read_at(file, off, buf);
    self.note(answer)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    let answer = self.host.read_link(dir, name);
    self.note(answer)
  }

  fn close_file(&mut self, file: HostFile) {
    self.host.close_file(file);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.host.close_dir(dir);
  }

  fn watch(&mut self, dir: HostDir) -> WatchState {
    self.host.watch(dir)
  }

  fn hints(&mut self) -> Vec<Hint> {
    self.host.hints()
  }

  fn now_ns(&mut self) -> i64 {
    self.host.now_ns()
  }
}

#[cfg(test)]
mod tests {
  use slates_base::s3::client::Style;
  use slates_base::s3::host::HostConfig;
  use slates_base::s3::name::S3Base;
  use slates_base::s3::sim::SimS3;

  use super::*;

  /// Shape: the volume whose base most tests read, and a second one.
  const FIRST: VolumeId = VolumeId { bytes: [1; 16] };
  /// Shape: see [`FIRST`].
  const SECOND: VolumeId = VolumeId { bytes: [2; 16] };
  /// Shape: a test's chunk, 4 KiB.
  const CHUNK: u64 = 4_096;
  /// Shape: the cache of most tests, chunks enough for every listing and file they read.
  const CACHE_CHUNKS: u64 = 64;
  /// Shape: the shard's bound on its waits in these tests, past the few they park.
  const BOUND: usize = 8;
  /// Shape: the round trip each landing is measured at, 5 ms, a near store's.
  const ROUND_TRIP_NS: u64 = 5_000_000;
  /// Shape: the clock the store and the host read, 2023-11-14T22:13:20Z; it stands still, so no listing outlives its
  /// round trip.
  const NOW_NS: i64 = 1_700_000_000_000_000_000;

  /// The tests' clock.
  fn clock() -> i64 {
    NOW_NS
  }

  /// A waker that counts its wakes, so a test sees a parked request woken, and woken once.
  #[allow(clippy::disallowed_types)] // `std::task::Wake` takes `Arc<Self>` by signature (D-8's test-harness exception).
  struct CountingWaker(std::sync::atomic::AtomicUsize);

  #[allow(clippy::disallowed_types)] // as above: `Wake::wake` receives `self: Arc<Self>`.
  impl std::task::Wake for CountingWaker {
    fn wake(self: std::sync::Arc<Self>) {
      self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
  }

  impl CountingWaker {
    /// The wakes so far.
    fn wakes(&self) -> usize {
      self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
  }

  /// A counting waker, and the `Waker` over it.
  #[allow(clippy::disallowed_types)] // as above: the `Waker` is built from an `Arc` (D-8's test-harness exception).
  fn counting_waker() -> (std::sync::Arc<CountingWaker>, Waker) {
    let counter = std::sync::Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(std::sync::Arc::clone(&counter));
    (counter, waker)
  }

  /// A host of an S3 base whose bucket holds `objects` under the prefix `base/`, its cache `cache_chunks` chunks, nothing
  /// fetched yet; and the store.
  fn s3_base(objects: &[(&str, Vec<u8>)], cache_chunks: u64) -> (S3Host, SimS3) {
    let base = S3Base::new(
      "http://127.0.0.1:9000",
      "garage",
      "bucket",
      "base/",
      Style::Path,
    )
    .unwrap();
    let host = S3Host::new(
      &base,
      HostConfig {
        chunk_bytes: CHUNK,
        cache_bytes: cache_chunks * CHUNK,
        owner: (501, 20),
        wall_ns: clock,
      },
    )
    .unwrap();
    let mut store = SimS3::new(false, clock);
    for (key, bytes) in objects {
      store.put(key, bytes);
    }
    (host, store)
  }

  /// The fetch a request listing `host`'s root meets pending, noted by the lens it is lent through, as a transport's
  /// request is lent the slot's host.
  fn met_listing(host: &mut S3Host) -> FetchId {
    let root = host.root();
    let mut lens = Lens::over(host);
    let answer = lens.list(root).map(|entries| entries.len());
    assert!(
      matches!(answer, Err(HostError::Pending(_))),
      "an unlisted root is pending, not {answer:?}"
    );
    lens.met().unwrap()
  }

  /// A new wait parked on `key` under `waker`, against `host`: its id.
  fn parked(waits: &mut BaseWaits, host: &mut S3Host, key: Key, waker: &Waker) -> u64 {
    match step(waits, Some(host), (key, None, waker), BOUND) {
      Step::Parked(id) => id,
      other => panic!("a request on a fetch under way parks, not {other:?}"),
    }
  }

  /// How wait `id` on `key` stands when its request looks again: its end, or `None` while it stays parked.
  fn looked(
    waits: &mut BaseWaits,
    host: &mut S3Host,
    (key, id): (Key, u64),
    waker: &Waker,
  ) -> Option<Waited> {
    match step(waits, Some(host), (key, Some(id), waker), BOUND) {
      Step::Ended(waited) => Some(waited),
      Step::Waiting => None,
      Step::Parked(_) => panic!("a parked wait does not park again"),
    }
  }

  /// Lands every fetch `host` of `volume` wants, answered by `store`, as the shard's driver does, waking what waits.
  fn drive(waits: &mut BaseWaits, host: &mut S3Host, store: &mut SimS3, volume: VolumeId) {
    while let Some((ticket, fetch)) = host.next_fetch() {
      let answer = store.answer(&fetch);
      land_on(
        waits,
        Some(&mut *host),
        (volume, ticket),
        answer,
        ROUND_TRIP_NS,
      );
    }
  }

  /// A-555 (one fetch however many requests wait on it). Do, over two volumes each over an S3 base, list the first's
  /// root through the lens twice and the second's once, and park each request on the fetch its pending answer named.
  /// Expect both of the first's requests to have met one fetch, all three parked, and that fetch asked once.
  #[test]
  fn the_requests_that_met_one_fetch_park_on_it_and_it_is_asked_once() {
    let (mut first, _store) = s3_base(&[("base/a.txt", b"a".to_vec())], CACHE_CHUNKS);
    let (mut second, _other) = s3_base(&[("base/c.txt", b"c".to_vec())], CACHE_CHUNKS);
    let mut waits = BaseWaits::default();
    let (one, two) = (met_listing(&mut first), met_listing(&mut first));
    assert_eq!(one, two, "the second request met the fetch the first did");
    let three = met_listing(&mut second);
    let (_counter, waker) = counting_waker();
    parked(&mut waits, &mut first, (FIRST, one), &waker);
    parked(&mut waits, &mut first, (FIRST, two), &waker);
    parked(&mut waits, &mut second, (SECOND, three), &waker);
    assert_eq!(waits.waits(), 3);
    assert!(first.next_fetch().is_some());
    assert_eq!(first.next_fetch(), None, "two requests, one fetch");
  }

  /// A-555 (a request is woken when the fetch it met lands, and by nothing else). Do park two requests on the first
  /// volume's root listing and one on the second's; let the store hold the first's fetch open while the second's lands,
  /// and look again; then land the first's. Expect the second's landing to wake its own request alone, the first's two
  /// sleeping on and still waiting when they look; the first's landing to wake each of them once; every request then
  /// to find its fetch landed and leave; and the woken request, run again, to read the root's entries.
  #[test]
  fn a_landing_wakes_only_the_requests_parked_on_its_fetch_and_they_run_again() {
    let (mut first, mut first_store) = s3_base(
      &[
        ("base/a.txt", b"a".to_vec()),
        ("base/sub/b.txt", b"b".to_vec()),
      ],
      CACHE_CHUNKS,
    );
    let (mut second, mut second_store) = s3_base(&[("base/c.txt", b"c".to_vec())], CACHE_CHUNKS);
    let mut waits = BaseWaits::default();
    let (one, other) = (met_listing(&mut first), met_listing(&mut second));
    let [(c1, w1), (c2, w2), (c3, w3)] = [counting_waker(), counting_waker(), counting_waker()];
    let id1 = parked(&mut waits, &mut first, (FIRST, one), &w1);
    let id2 = parked(&mut waits, &mut first, (FIRST, one), &w2);
    let id3 = parked(&mut waits, &mut second, (SECOND, other), &w3);
    // The store holds the first volume's fetch open: asked, not answered.
    let (held_ticket, held_fetch) = first.next_fetch().unwrap();
    drive(&mut waits, &mut second, &mut second_store, SECOND);
    assert_eq!(
      (c1.wakes(), c2.wakes(), c3.wakes()),
      (0, 0, 1),
      "only the landed fetch's request woke"
    );
    assert_eq!(
      looked(&mut waits, &mut first, ((FIRST, one), id1), &w1),
      None,
      "its fetch is still open"
    );
    let answer = first_store.answer(&held_fetch);
    assert_eq!(
      land_on(
        &mut waits,
        Some(&mut first),
        (FIRST, held_ticket),
        answer,
        ROUND_TRIP_NS
      ),
      2
    );
    assert_eq!(
      (c1.wakes(), c2.wakes()),
      (1, 1),
      "each of its requests woken once"
    );
    let ends = [
      looked(&mut waits, &mut first, ((FIRST, one), id1), &w1),
      looked(&mut waits, &mut first, ((FIRST, one), id2), &w2),
      looked(&mut waits, &mut second, ((SECOND, other), id3), &w3),
    ];
    assert_eq!(ends, [Some(Waited::Landed); 3]);
    assert_eq!(waits.waits(), 0, "every request left");
    let root = first.root();
    assert_eq!(
      Lens::over(&mut first)
        .list(root)
        .map(|entries| entries.len()),
      Ok(2)
    );
  }

  /// A-555 (bounded by the shard's waiter budget, with a typed refusal at the bound; a fetch that settled holds no
  /// request). Do park a request within a bound of one, then another on the same fetch; land the fetch and park a third
  /// request on it. Expect the first parked, the second refused at once as the bound's answer, and the third run again
  /// at once, its fetch settled, with no wait added.
  #[test]
  fn a_wait_past_the_bound_is_refused_and_one_on_a_settled_fetch_runs_again_at_once() {
    let (mut host, mut store) = s3_base(&[("base/a.txt", b"a".to_vec())], CACHE_CHUNKS);
    let mut waits = BaseWaits::default();
    let fetch = met_listing(&mut host);
    let (_counter, waker) = counting_waker();
    let first = step(
      &mut waits,
      Some(&mut host),
      ((FIRST, fetch), None, &waker),
      1,
    );
    let refused = step(
      &mut waits,
      Some(&mut host),
      ((FIRST, fetch), None, &waker),
      1,
    );
    assert_eq!(
      (first, refused),
      (Step::Parked(0), Step::Ended(Waited::Full))
    );
    drive(&mut waits, &mut host, &mut store, FIRST);
    let late = step(
      &mut waits,
      Some(&mut host),
      ((FIRST, fetch), None, &waker),
      BOUND,
    );
    assert_eq!(
      late,
      Step::Ended(Waited::Landed),
      "a settled fetch's late request runs again at once"
    );
    assert_eq!(waits.waits(), 1, "only the first request parked");
  }

  /// A-555 (the host holds what a fetch landed until the last request parked on it has run). Do, through a cache of one
  /// chunk, park a request on `a.bin`'s first chunk and one on `b.bin`'s, and land both fetches; let both requests look
  /// and leave; then run `b.bin`'s request again and land what it asks. Expect `b.bin`'s first landing not kept, since
  /// `a.bin`'s chunk is held for its request; and once both have left, `b.bin`'s second landing kept, shedding `a.bin`'s,
  /// and its bytes read.
  #[test]
  fn the_last_request_to_leave_lets_its_fetchs_hold_go() {
    let chunk = vec![7u8; usize::try_from(CHUNK).unwrap()];
    let (mut host, mut store) = s3_base(
      &[("base/a.bin", chunk.clone()), ("base/b.bin", chunk.clone())],
      1,
    );
    let mut waits = BaseWaits::default();
    let (_counter, waker) = counting_waker();
    let root = host.root();
    met_listing(&mut host);
    drive(&mut waits, &mut host, &mut store, FIRST);
    let (a, b) = (
      host.open_file(root, "a.bin").unwrap(),
      host.open_file(root, "b.bin").unwrap(),
    );
    let mut buf = vec![0u8; chunk.len()];
    let (Err(HostError::Pending(for_a)), Err(HostError::Pending(for_b))) =
      (host.read_at(a, 0, &mut buf), host.read_at(b, 0, &mut buf))
    else {
      panic!("unfetched chunks are misses");
    };
    let a_wait = parked(&mut waits, &mut host, (FIRST, for_a), &waker);
    let b_wait = parked(&mut waits, &mut host, (FIRST, for_b), &waker);
    drive(&mut waits, &mut host, &mut store, FIRST);
    assert_eq!(
      host.counts().unkept,
      1,
      "b.bin's chunk found no room beside a.bin's held one"
    );
    looked(&mut waits, &mut host, ((FIRST, for_a), a_wait), &waker);
    looked(&mut waits, &mut host, ((FIRST, for_b), b_wait), &waker);
    assert!(matches!(
      host.read_at(b, 0, &mut buf),
      Err(HostError::Pending(_))
    ));
    drive(&mut waits, &mut host, &mut store, FIRST);
    assert_eq!(
      host.counts().unkept,
      1,
      "with a.bin's hold let go, b.bin's chunk is kept"
    );
    assert_eq!(host.read_at(b, 0, &mut buf), Ok(chunk.len()));
  }
}
