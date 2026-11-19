//! The fetch driver of an S3 base (§4.15 "An S3 base", A-555): the task of the volume's owner shard that runs its host's
//! fetches against the store and hands each answer back, so the requests parked on a fetch are woken when it lands.
//!
//! - **It wakes when a fetch is wanted.** The host is sans-io: a call that misses asks for a fetch and is answered
//!   pending ([`super::host`]). An idle driver waits on the host itself
//!   ([`super::host::S3Host::next_fetch_or_wait`]), which wakes
//!   it the moment a miss wants a fetch, so a fetch starts with the request that needs it and no driver ever polls.
//! - **One fetch at a time.** It takes the next fetch the host wants, awaits the store's answer and hands it back,
//!   timed, so the host learns the path's round trip from it; then the next. The fetches under way are one per volume,
//!   on one kept-alive connection (the client's, A-471); a request parks on its own fetch meanwhile, whatever is ahead
//!   of it in the queue. Fetches side by side, within the requests that wait on them, are a measurement's to decide.
//! - **It lives as long as its host.** A host dropped (its volume destroyed, its base replaced) wakes a driver waiting
//!   on it, and the driver, finding no host, ends; an answer that arrives after its host is gone is handed to nothing.
//!   So the driver needs no other owner to end it, and it never outlives what it serves.
//! - **The same code over any store and any home.** The store is the network client in the daemon and the simulated one
//!   in a test ([`super::fetch::Store`]); the host lives in the shard's volume slot in the daemon and in a test's own
//!   cell here ([`HostCell`]), where the daemon's hands each answer to the requests' waits (`base_wait::land`).

use std::task::{Poll, Waker};

use super::client::S3Error;
use super::fetch::{Fetch, Fetched, Store};

/// Where a driver finds its host and hands each answer back.
pub trait HostCell {
  /// The next fetch the host wants with its ticket, `Pending` with `waker` kept for the host to wake when one is
  /// wanted, or `Ready(None)` once the host is gone.
  fn next_fetch(&mut self, waker: &Waker) -> Poll<Option<(u64, Fetch)>>;
  /// Hands the store's `answer` to the fetch `ticket`, which took `elapsed_ns`, to the host, and wakes what waits on
  /// the fetch it settles; nothing once the host is gone.
  fn land(&mut self, ticket: u64, answer: Result<Fetched, S3Error>, elapsed_ns: u64);
}

/// Runs the fetches of the host `cell` holds against `store`, one at a time, each timed on `clock` (nanoseconds), until
/// the host is gone.
pub async fn drive<S: Store, C: HostCell>(mut store: S, mut cell: C, clock: fn() -> u64) {
  loop {
    let Some((ticket, fetch)) = std::future::poll_fn(|cx| cell.next_fetch(cx.waker())).await else {
      return;
    };
    let began = clock();
    let answer = store.run(&fetch).await;
    cell.land(ticket, answer, clock().saturating_sub(began));
  }
}

#[cfg(test)]
mod tests {
  use std::cell::{Cell, RefCell};
  use std::pin::pin;
  use std::task::Context;

  use slates_vfs::host::{FetchId, HostError, HostFs};

  use super::*;
  use crate::s3::client::Style;
  use crate::s3::host::{HostConfig, S3Host};
  use crate::s3::name::S3Base;
  use crate::s3::sim::SimS3;

  /// Shape: a test's chunk, 4 KiB.
  const CHUNK: u64 = 4_096;
  /// Shape: the cache, chunks enough for every listing and file the tests read.
  const CACHE_CHUNKS: u64 = 64;
  /// Shape: the clock the store and the host read, 2023-11-14T22:13:20Z; it stands still.
  const NOW_NS: i64 = 1_700_000_000_000_000_000;
  /// Shape: each fetch's measured time, 5 ms, a near store's round trip.
  const ROUND_TRIP_NS: u64 = 5_000_000;

  /// The store's and the host's clock.
  fn wall() -> i64 {
    NOW_NS
  }

  /// The driver's clock: each reading a round trip past the last, so every fetch takes one.
  fn ticking() -> u64 {
    thread_local! {
      static TICKS: Cell<u64> = const { Cell::new(0) };
    }
    TICKS.with(|ticks| {
      ticks.set(ticks.get() + ROUND_TRIP_NS);
      ticks.get()
    })
  }

  /// A waker that counts its wakes, so a test sees an idle driver woken, and woken once.
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

  /// A counting waker's count, and the `Waker` over it.
  #[allow(clippy::disallowed_types)] // as above: the `Waker` is built from an `Arc` (D-8's test-harness exception).
  fn counting_waker() -> (std::sync::Arc<CountingWaker>, Waker) {
    let counter = std::sync::Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(std::sync::Arc::clone(&counter));
    (counter, waker)
  }

  /// A host of an S3 base over the prefix `base/`, nothing fetched yet; and a store holding `keys`, each its own name's
  /// bytes.
  fn host_and_store(keys: &[&str]) -> (S3Host, SimS3) {
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
        cache_bytes: CACHE_CHUNKS * CHUNK,
        owner: (501, 20),
        wall_ns: wall,
      },
    )
    .unwrap();
    let mut store = SimS3::new(false, wall);
    for key in keys {
      store.put(key, key.as_bytes());
    }
    (host, store)
  }

  /// A test's home for the host: the host while it lives, and what each landing settled.
  struct TestCell<'a> {
    host: &'a RefCell<Option<S3Host>>,
    settled: &'a RefCell<Vec<Option<FetchId>>>,
  }

  impl HostCell for TestCell<'_> {
    fn next_fetch(&mut self, waker: &Waker) -> Poll<Option<(u64, Fetch)>> {
      match self.host.borrow_mut().as_mut() {
        None => Poll::Ready(None),
        Some(host) => match host.next_fetch_or_wait(waker) {
          Some(next) => Poll::Ready(Some(next)),
          None => Poll::Pending,
        },
      }
    }

    fn land(&mut self, ticket: u64, answer: Result<Fetched, S3Error>, elapsed_ns: u64) {
      let settled = self
        .host
        .borrow_mut()
        .as_mut()
        .and_then(|host| host.landed(ticket, answer, elapsed_ns));
      self.settled.borrow_mut().push(settled);
    }
  }

  /// A store that holds every fetch open until its gate opens, then answers as `SimS3` does.
  struct Gated<'a> {
    store: SimS3,
    gate: &'a Gate,
  }

  /// Whether held fetches may be answered, and the waker of the one held.
  #[derive(Default)]
  struct Gate {
    open: Cell<bool>,
    held: RefCell<Option<Waker>>,
  }

  impl Gate {
    /// Opens the gate, waking the fetch it held.
    fn open(&self) {
      self.open.set(true);
      if let Some(held) = self.held.borrow_mut().take() {
        held.wake();
      }
    }
  }

  impl Store for Gated<'_> {
    fn run(&mut self, fetch: &Fetch) -> impl Future<Output = Result<Fetched, S3Error>> {
      let answer = self.store.answer(fetch);
      let gate = self.gate;
      let mut answer = Some(answer);
      std::future::poll_fn(move |cx| {
        if !gate.open.get() {
          *gate.held.borrow_mut() = Some(cx.waker().clone());
          return Poll::Pending;
        }
        answer.take().map_or(Poll::Pending, Poll::Ready)
      })
    }
  }

  /// The root's listing as a request sees it now: its length, or the refusal (a dropped host's handle is stale).
  fn listed(host: &RefCell<Option<S3Host>>) -> Result<usize, HostError> {
    let mut host = host.borrow_mut();
    let host = host.as_mut().ok_or(HostError::StaleHandle)?;
    let root = host.root();
    host.list(root).map(|entries| entries.len())
  }

  /// A-555 (an idle driver sleeps until a fetch is wanted). Do start a driver over a host nothing has missed on, then
  /// let a request miss on the root's listing. Expect the driver asleep and unwoken at first; woken once by the miss;
  /// then to run the listing's fetch, settle it and answer the request when it asks again.
  #[test]
  fn an_idle_driver_sleeps_until_a_miss_wants_a_fetch_then_runs_it() {
    let (host, store) = host_and_store(&["base/a.txt", "base/b.txt"]);
    let (host, settled) = (RefCell::new(Some(host)), RefCell::new(Vec::new()));
    let (counter, waker) = counting_waker();
    let mut context = Context::from_waker(&waker);
    let mut driver = pin!(drive(
      store,
      TestCell {
        host: &host,
        settled: &settled
      },
      ticking
    ));
    assert!(driver.as_mut().poll(&mut context).is_pending());
    assert_eq!(counter.wakes(), 0, "nothing is wanted, so nothing wakes it");
    let Err(HostError::Pending(fetch)) = listed(&host) else {
      panic!("the root's first listing is a miss");
    };
    assert_eq!(counter.wakes(), 1, "the miss woke the driver");
    assert!(driver.as_mut().poll(&mut context).is_pending());
    assert_eq!(*settled.borrow(), [Some(fetch)]);
    assert_eq!(listed(&host), Ok(2));
  }

  /// A-555 (a fetch lands when its store answers, and not before). Do let a request miss through a driver whose store
  /// holds every fetch open, poll the driver, then open the store and poll again. Expect nothing settled and the
  /// request still pending while the store holds the fetch; once it answers, the fetch settled and the request answered.
  #[test]
  fn a_fetch_the_store_holds_open_lands_only_once_answered() {
    let (host, store) = host_and_store(&["base/a.txt"]);
    let (host, settled, gate) = (
      RefCell::new(Some(host)),
      RefCell::new(Vec::new()),
      Gate::default(),
    );
    let (_counter, waker) = counting_waker();
    let mut context = Context::from_waker(&waker);
    let store = Gated { store, gate: &gate };
    let cell = TestCell {
      host: &host,
      settled: &settled,
    };
    let mut driver = pin!(drive(store, cell, ticking));
    let Err(HostError::Pending(fetch)) = listed(&host) else {
      panic!("the root's first listing is a miss");
    };
    assert!(driver.as_mut().poll(&mut context).is_pending());
    assert!(
      settled.borrow().is_empty(),
      "the store holds the fetch open"
    );
    assert_eq!(listed(&host), Err(HostError::Pending(fetch)));
    gate.open();
    assert!(driver.as_mut().poll(&mut context).is_pending());
    assert_eq!(*settled.borrow(), [Some(fetch)]);
    assert_eq!(listed(&host), Ok(1));
  }

  /// A-555 (the driver lives as long as its host). Do start a driver, let it sleep, then drop its host. Expect the
  /// driver woken by the host's dropping, and ended when polled.
  #[test]
  fn a_driver_ends_when_its_host_is_dropped() {
    let (host, store) = host_and_store(&["base/a.txt"]);
    let (host, settled) = (RefCell::new(Some(host)), RefCell::new(Vec::new()));
    let (counter, waker) = counting_waker();
    let mut context = Context::from_waker(&waker);
    let mut driver = pin!(drive(
      store,
      TestCell {
        host: &host,
        settled: &settled
      },
      ticking
    ));
    assert!(driver.as_mut().poll(&mut context).is_pending());
    host.borrow_mut().take();
    assert_eq!(counter.wakes(), 1, "the host's dropping woke its driver");
    assert!(driver.as_mut().poll(&mut context).is_ready());
  }
}
