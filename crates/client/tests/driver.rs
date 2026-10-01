//! The async client's driver (`slates_client::driver`; AUD-29-19, AUD-29-20) against real daemons: a ticker
//! loop stands in for an SDK's event loop — it pumps, ticks when the driver asks, and records its longest
//! single step — while calls overflow admission, a daemon restarts under them, a daemon dies for good, and
//! a call is cancelled. Every call must end, and the loop must never be held.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use slates_anchor::AnchorSegment;
use slates_client::driver::{Driver, Event, Ticket};
use slates_client::{Client, ClientError, Deadlines};
use slates_ipc::protocol::ReplyBody;
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the probe budget of the quick profile these tests measure (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon.
const TEST_SHARDS: u16 = 2;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures")
}

/// The product's own deadlines.
fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

/// The ticker: how each call ended, and the longest single step the loop took.
#[derive(Default)]
struct Ticker {
  ended: BTreeMap<Ticket, Result<(), ClientError>>,
  longest_step: Duration,
}

impl Ticker {
  /// Runs the loop until every one of `tickets` has ended or `within` passes: pump, tick when the driver
  /// asks, end each call by its event, and time every step.
  fn run(
    &mut self,
    client: &mut Client,
    driver: &mut Driver,
    tickets: &[Ticket],
    within: Duration,
  ) {
    let started = Instant::now();
    let mut next_tick = Instant::now();
    while tickets
      .iter()
      .any(|ticket| !self.ended.contains_key(ticket))
      && started.elapsed() < within
    {
      let step = Instant::now();
      let mut events = driver.pump(client);
      if Instant::now() >= next_tick {
        events.extend(driver.tick(client));
      }
      for event in events {
        match event {
          Event::Ready { ticket, word } => {
            let reply = client.poll_reply_word(word);
            driver.finish(ticket);
            self.ended.insert(
              ticket,
              reply.map(|body| assert!(matches!(body, Some(ReplyBody::Listed { .. })))),
            );
          }
          Event::Failed { ticket, error } => {
            self.ended.insert(ticket, Err(error));
          }
        }
      }
      next_tick = Instant::now()
        + Duration::from_nanos(driver.next_wake_ns(client).unwrap_or(deadlines().reply_ns));
      self.longest_step = self.longest_step.max(step.elapsed());
      std::hint::spin_loop();
    }
  }
}

/// Derived: the longest single step an event loop may take under the driver — a tenth of the reply
/// deadline, the longest pause the reconnect pacing itself allows (`min(2 × pause, budget / 10)`); a step
/// that waited for a reply, a ring slot or a daemon would exceed it.
fn step_bound() -> Duration {
  Duration::from_nanos(deadlines().reply_ns / 10)
}

/// The test's daemons over one anchor segment, so a second daemon recovers the first's state.
struct Anchor {
  profile: MachineProfile,
  config: DaemonConfig,
  segment: AnchorSegment,
}

impl Anchor {
  fn new(instance: &str) -> Anchor {
    let profile = profile();
    let config = DaemonConfig::derive(&profile, instance, Some(TEST_SHARDS));
    let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
      * 2
      * usize::from(config.geometry.partitions.max(1));
    let segment = AnchorSegment::create(
      &format!("slates-seg-{instance}"),
      &profile.facts.identity,
      config.geometry,
    )
    .unwrap()
    .with_content(&format!("slates-con-{instance}"), content_bytes)
    .unwrap();
    Anchor {
      profile,
      config,
      segment,
    }
  }

  fn start(&self) -> Daemon {
    let (handoff, len) = self.segment.handoff().unwrap();
    let content = self.segment.content_handoff().unwrap();
    let daemon = Daemon::start(
      &self.profile,
      self.config.clone(),
      SegmentSource::Handoff {
        handoff,
        len,
        content,
      },
    )
    .unwrap();
    daemon
      .bootstrap(true)
      .expect("the fixture explicitly creates its local consensus group");
    daemon
  }
}

/// Submits `count` list calls, returning the tickets admitted and how many were refused.
fn submit_lists(client: &mut Client, driver: &mut Driver, count: usize) -> (Vec<Ticket>, usize) {
  let mut admitted = Vec::new();
  let mut refused = 0;
  for _ in 0..count {
    match driver.submit(client, Box::new(|client: &mut Client| client.list_begin())) {
      Ok(ticket) => admitted.push(ticket),
      Err(ClientError::TooManyOutstanding { .. }) => refused += 1,
      Err(e) => panic!("{e}"),
    }
  }
  (admitted, refused)
}

/// Connects from the loop: the claim begun, then polled at the pacing it asks for, every step timed.
fn connect_from_the_loop(instance: &str, ticker: &mut Ticker) -> Client {
  let started = Instant::now();
  loop {
    let step = Instant::now();
    match Client::begin_connect(instance, deadlines()) {
      Ok(mut connecting) => loop {
        let step = Instant::now();
        let polled = connecting.poll();
        ticker.longest_step = ticker.longest_step.max(step.elapsed());
        match polled {
          Ok(Some(client)) => return client,
          Ok(None) => {
            let next = Instant::now() + Duration::from_nanos(connecting.next_poll_ns());
            while Instant::now() < next {
              std::hint::spin_loop();
            }
          }
          Err(e) => panic!("{e}"),
        }
      },
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        ticker.longest_step = ticker.longest_step.max(step.elapsed());
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// Phase one: three bounds' worth of calls submitted, the daemon stopped under them and a second started
/// over the same segment. Every admitted call is answered after the restart; the overflow is refused at
/// once. Returns the second daemon.
fn overflow_then_restart(
  anchor: &Anchor,
  first: Daemon,
  client: &mut Client,
  driver: &mut Driver,
  ticker: &mut Ticker,
) -> Daemon {
  let limit = client.outstanding_limit();
  let (admitted, refused) = submit_lists(client, driver, limit * 3);
  first.stop();
  let second = anchor.start();
  ticker.run(client, driver, &admitted, budgets() * 2);
  assert_eq!(
    refused,
    limit * 3 - admitted.len(),
    "the overflow is refused at once"
  );
  assert!(refused > 0, "non-vacuous: the overflow met the bounds");
  assert!(
    client.reconnects() >= 1,
    "non-vacuous: the calls were recovered by a reconnect, not answered before the stop"
  );
  assert!(admitted.len() >= limit, "a ring's worth is admitted");
  for ticket in &admitted {
    assert_eq!(
      ticker.ended.get(ticket),
      Some(&Ok(())),
      "admitted call {ticket} answered after the restart"
    );
  }
  second
}

/// Phase two: the daemon stopped for good and a ring's worth submitted; every call ends `DaemonGone`.
fn death(daemon: Daemon, client: &mut Client, driver: &mut Driver, ticker: &mut Ticker) {
  daemon.stop();
  let (doomed, _) = submit_lists(client, driver, client.outstanding_limit());
  ticker.run(client, driver, &doomed, budgets() * 2);
  for ticket in &doomed {
    assert!(
      matches!(
        ticker.ended.get(ticket),
        Some(Err(ClientError::DaemonGone { .. }))
      ),
      "call {ticket} after the daemon's death ended DaemonGone: {:?}",
      ticker.ended.get(ticket)
    );
  }
  assert!(driver.is_idle());
}

/// The reply and reconnect budgets together: the longest a recovered call may take.
fn budgets() -> Duration {
  Duration::from_nanos(deadlines().reply_ns + deadlines().reconnect_ns)
}

/// AUD-29-19 and AUD-29-20 (§4.7, R6, banned item 9): an event loop driving the client is never held, and
/// every call ends. Do, with a ticker loop timing every step:
/// 1. connect from the loop (the claim begun, then polled);
/// 2. submit three bounds' worth of calls and stop the daemon under them, then start a second daemon over
///    the same anchor segment;
/// 3. stop that daemon for good and submit more;
/// 4. submit a call and cancel it.
///
/// Expect: the calls past the client's and the queue's bounds refused at once; every admitted call of the
/// first history answered by the restarted daemon (recovery reconnects and resends under the calls' own ids);
/// every call of the second failed `DaemonGone` within the reply and reconnect budgets; the cancelled call
/// leaving nothing outstanding; and no loop step longer than [`step_bound`].
#[test]
fn every_call_ends_and_the_loop_is_never_held_across_restart_and_death() {
  let instance = format!("cl-driver-{}", std::process::id());
  let anchor = Anchor::new(&instance);
  let first = anchor.start();
  let mut ticker = Ticker::default();
  let mut client = connect_from_the_loop(&instance, &mut ticker);
  let mut driver = Driver::new(&client);

  let second = overflow_then_restart(&anchor, first, &mut client, &mut driver, &mut ticker);
  death(second, &mut client, &mut driver, &mut ticker);

  let third = anchor.start();
  let cancelled = driver
    .submit(
      &mut client,
      Box::new(|client: &mut Client| client.list_begin()),
    )
    .unwrap();
  driver.cancel(&mut client, cancelled);
  assert!(driver.is_idle(), "a cancelled call leaves the driver");
  assert!(
    client.outstanding().is_empty(),
    "a cancelled call leaves nothing outstanding"
  );
  third.stop();
  assert!(
    ticker.longest_step < step_bound(),
    "the longest loop step was {:?}, bound {:?}",
    ticker.longest_step,
    step_bound()
  );
}
