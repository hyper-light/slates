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
  /// The call the longest step made: a failure names what held the loop (CI 2026-10-01: a 245 ms step on
  /// the macOS runner, unattributed).
  longest_call: &'static str,
}

impl Ticker {
  /// Times one loop step that made `call`, keeping the longest.
  fn note(&mut self, call: &'static str, took: Duration) {
    if took > self.longest_step {
      self.longest_step = took;
      self.longest_call = call;
    }
  }

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
      self.note("pump", step.elapsed());
      if Instant::now() >= next_tick {
        let ticked = Instant::now();
        events.extend(driver.tick(client));
        self.note("tick", ticked.elapsed());
      }
      for event in events {
        match event {
          Event::Ready { ticket, word } => {
            let reply = client.poll_reply_word(word);
            driver.finish(ticket);
            self.ended.insert(
              ticket,
              reply.map(|body| {
                assert!(
                  matches!(body, Some(ReplyBody::Listed { .. })),
                  "a list answered {body:?}"
                );
              }),
            );
          }
          Event::Failed { ticket, error } => {
            self.ended.insert(ticket, Err(error));
          }
        }
      }
      next_tick = Instant::now()
        + Duration::from_nanos(driver.next_wake_ns(client).unwrap_or(deadlines().reply_ns));
      self.note("a whole step: pump, tick and the events", step.elapsed());
      std::hint::spin_loop();
    }
  }
}

/// Shape: the sampler's sleep — a millisecond, far below the bound it qualifies.
const NOISE_SAMPLE: Duration = Duration::from_millis(1);

/// The machine's own scheduling noise while the test runs: a thread that sleeps [`NOISE_SAMPLE`] at a time and
/// records how much longer than asked each sleep took — time the OS kept it off a core after its wake; the
/// loop's thread is kept off as long by the same cause, so a loop step is judged against the bound plus the noise measured
/// in the same window (the pattern of `destroy_rows` in `crates/vfs/examples/vfs_bench.rs`). A step our code
/// held — a blocking wait in the driver — leaves the sampler's gaps short, and still fails.
struct Noise {
  stop: std::sync::mpsc::Sender<()>,
  sampler: std::thread::JoinHandle<Duration>,
}

impl Noise {
  fn start() -> Noise {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let sampler = std::thread::spawn(move || {
      let mut longest = Duration::ZERO;
      loop {
        match stopped.try_recv() {
          Err(std::sync::mpsc::TryRecvError::Empty) => {}
          _ => return longest,
        }
        let asked = Instant::now();
        // The harness's own sampler sleeps, so it costs no core while it measures (D-9 is shipped code's).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(NOISE_SAMPLE);
        longest = longest.max(asked.elapsed().saturating_sub(NOISE_SAMPLE));
      }
    });
    Noise { stop, sampler }
  }

  /// The longest gap the sampler saw.
  fn stop(self) -> Duration {
    let _ = self.stop.send(());
    self.sampler.join().unwrap()
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
    let content_bytes = config.content_bytes();
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
        ticker.note("Connecting::poll", step.elapsed());
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
        ticker.note("Client::begin_connect", step.elapsed());
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
/// leaving nothing outstanding; and no loop step longer than [`step_bound`] plus the scheduling noise a
/// sampler thread measured in the same window ([`Noise`]).
#[test]
fn every_call_ends_and_the_loop_is_never_held_across_restart_and_death() {
  let noise = Noise::start();
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
  let noise = noise.stop();
  assert!(
    ticker.longest_step < step_bound() + noise,
    "the longest loop step was {:?} in {}, bound {:?} plus the machine's own scheduling noise {:?}",
    ticker.longest_step,
    ticker.longest_call,
    step_bound(),
    noise
  );
}

/// Do: submit one list call, wait until its reply is on the ring, then pump twice before finishing it — as a
/// binding's loop does when its pump and its timer's tick fall in one step (the tick pumps first). Expect: the
/// first pump reports the call ready once, the second reports nothing, and the reply is the call's. Before, the
/// second pump reported it again (the client's ready set holds a reply until it is taken), so the loop took the
/// reply on the first event and found nothing on the second — the Linux CI failure of
/// `every_call_ends_and_the_loop_is_never_held_across_restart_and_death` (`a list answered None`), 2026-10-01.
#[test]
fn a_ready_call_is_reported_once_until_it_is_finished() {
  let instance = format!("cl-driver-once-{}", std::process::id());
  let anchor = Anchor::new(&instance);
  let daemon = anchor.start();
  let mut ticker = Ticker::default();
  let mut client = connect_from_the_loop(&instance, &mut ticker);
  let mut driver = Driver::new(&client);
  let ticket = driver
    .submit(
      &mut client,
      Box::new(|client: &mut Client| client.list_begin()),
    )
    .unwrap();
  let started = Instant::now();
  let first = loop {
    let events = driver.pump(&mut client);
    if !events.is_empty() || started.elapsed() > START_WAIT {
      break events;
    }
    std::hint::spin_loop();
  };
  let second = driver.pump(&mut client);
  let [
    Event::Ready {
      ticket: ready,
      word,
    },
  ] = first.as_slice()
  else {
    panic!("one ready event for the call: {first:?}");
  };
  assert_eq!(*ready, ticket);
  assert!(
    second.is_empty(),
    "reported again before it was finished: {second:?}"
  );
  let reply = client.poll_reply_word(*word).unwrap();
  assert!(matches!(reply, Some(ReplyBody::Listed { .. })), "{reply:?}");
  driver.finish(ticket);
  assert!(driver.is_idle());
  daemon.stop();
}
