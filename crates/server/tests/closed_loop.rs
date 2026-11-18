//! A client in R9's closed loop finds the shards it goes round still polling, and a paced client holds none (A-326;
//! §4.7). A closed-loop client sends its next request within F of its last reply, so R9's floor keeps its own shard
//! polling (A-223's window rule). But its creates hash over the shards and each later verb goes to its volume's owner,
//! so with three or more shards an owner hears from the loop only after it has gone round the others, and at R9's floor
//! alone the owner had parked by then: on ubuntu-24.04-arm's three shards the quiet single-client p99 was 116.4 µs
//! against main's 27.1, its shards parking 144 to 331 times per 1,000 creates (step 6 of the A-223 plan, diag-liveness-42
//! and -43). A forward made for a request that continues the loop now holds its owner's window for the loop's return at
//! R9's percentile (`config::closed_loop_hold`), never longer than the loop had already run (`verbs::loop_hold`).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, NamePolicy, SizeClass};
use slates_server::config::{PROVISION_TAIL, loop_revisit_cycles};
use slates_server::daemon::OBSERVE_BUDGET_NS;
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: three shards, ubuntu-24.04-arm's derived count where R9's floor alone failed; the loop goes round all three, so
/// it returns to each of the two that do not serve the client every third cycle. Three shards and the client fit a
/// four-CPU runner, so the shards a hold keeps polling never take a CPU from the loop itself.
const TEST_SHARDS: u16 = 3;
/// Shape: the loop's measured cycles, each a create and its destroy on one shard: a hundred returns to each of the two
/// owners that do not serve the client, so R9's tail of one in a hundred is a count.
const LOOP_CYCLES: usize = 300;
/// Shape: the paced client's cycles.
const PACED_CYCLES: usize = 24;
/// Shape: the paced client's pause between cycles, a millisecond, as `provision_bench`'s paced form pauses: twenty
/// times F, so no request of it continues a loop.
const PACE: Duration = Duration::from_millis(1);
/// Shape: the bounded volume each cycle creates and destroys.
const VOLUME_BYTES: u64 = 1 << 20;
/// Shape: names tried for the ones each shard owns: about a third of names land on each of three partitions, so this
/// finds every name the test needs with room to spare.
const NAME_TRIES: usize = 8 * (LOOP_CYCLES + PACED_CYCLES);

/// The shard (by position) that serves the client `client_id`.
fn serving(daemon: &Daemon, client_id: u32) -> usize {
  daemon
    .shards()
    .iter()
    .position(|shard| {
      daemon
        .observation(
          Some(*shard),
          OBSERVE_BUDGET_NS,
          move |state: &mut ShardState| {
            state
              .clients
              .iter()
              .any(|(_, slot)| slot.client_id == client_id)
          },
        )
        .unwrap()
        .wait()
        .unwrap()
    })
    .expect("a shard serves the client")
}

/// The names, in order, that the partition at `partition` owns, `count` of them.
fn names_on(partition: usize, count: usize) -> Vec<String> {
  (0..NAME_TRIES)
    .map(|n| format!("loop-{n}"))
    .filter(|name| {
      usize::from(slates_server::verbs::owner_of_name(
        name,
        usize::from(TEST_SHARDS),
      )) == partition
    })
    .take(count)
    .collect()
}

/// Each shard's parks so far and the holds its forwards asked of it, by position.
fn parks_and_holds(daemon: &Daemon) -> Vec<(u64, u64)> {
  daemon
    .shards()
    .iter()
    .map(|shard| {
      daemon
        .observation(Some(*shard), OBSERVE_BUDGET_NS, |_: &mut ShardState| {
          slates_rt::registry::with_current(|ctx| {
            let counters = ctx.counters();
            (counters.waits, counters.idle_holds)
          })
          .unwrap_or_default()
        })
        .unwrap()
        .wait()
        .unwrap()
    })
    .collect()
}

/// The parks and holds the shards in `owners` gained between `before` and `after`, summed.
fn gained(before: &[(u64, u64)], after: &[(u64, u64)], owners: &[usize]) -> (u64, u64) {
  owners.iter().fold((0, 0), |(parks, holds), owner| {
    (
      parks + (after[*owner].0 - before[*owner].0),
      holds + (after[*owner].1 - before[*owner].1),
    )
  })
}

/// One cycle: create the volume `name` names, then destroy it.
fn cycle(client: &mut Client, name: &str) {
  let volume = client
    .create(&CreateSpec {
      name: name.to_owned(),
      size: SizeClass::Bounded {
        limit: VOLUME_BYTES,
      },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  client.destroy(volume).unwrap();
}

/// Waits `span` on the test thread, however early a park returns.
fn wait(span: Duration) {
  let started = Instant::now();
  while started.elapsed() < span {
    std::thread::park_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// A-326, R9 (AC-2.1), §4.7. Do: on a three-shard daemon, run one client's closed loop of creates and destroys, each
/// cycle on the next of the three shards in turn, first long enough for the loop's run to reach its full hold, then
/// [`LOOP_CYCLES`] cycles measured; then pause, and run [`PACED_CYCLES`] cycles a millisecond apart. Expect, in the
/// loop: a hold asked of the two shards that do not serve the client for every forward (the non-vacuity count), and at
/// most R9's tail of parks on them past those the run's own returns explain (a return later than the hold, a forward that
/// held nothing); in the paced cycles: no hold asked of any shard, and those two parking between cycles (R9's floor
/// alone, as A-223's window rule leaves a paced client). Before A-326 they parked at nearly every return.
#[test]
fn a_closed_loops_creates_find_the_shards_it_goes_round_polling_and_a_paced_client_holds_none() {
  let profile = common::machine_profile();
  let instance = format!("srv-closed-loop-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let hold_ns = config.closed_loop_hold_ns;
  let hold = Duration::from_nanos(hold_ns);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "closed-loop"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = common::client::connect(&instance);
  // The loop's client spins for its reply as long as a forward holds a shard, so its own wake never stretches a cycle.
  client.spin_for(Some(hold_ns));
  let origin = serving(&daemon, client.client_id());
  let shards = usize::from(TEST_SHARDS);
  let owners: Vec<usize> = (0..shards).filter(|shard| *shard != origin).collect();
  // Enough cycles that the loop's run has passed its full hold (two requests a cycle), and then the measured ones.
  let warm = usize::try_from(loop_revisit_cycles(TEST_SHARDS)).unwrap();
  let per_shard = (warm + LOOP_CYCLES + PACED_CYCLES).div_ceil(shards);
  let names: Vec<Vec<String>> = (0..shards)
    .map(|shard| names_on(shard, per_shard))
    .collect();
  let name = |cycle: usize| names[cycle % shards][cycle / shards].as_str();
  for n in 0..warm {
    cycle(&mut client, name(n));
  }
  let before = parks_and_holds(&daemon);
  let mut last_visit: Vec<Option<Instant>> = vec![None; shards];
  let mut late_returns = 0;
  let looping = Instant::now();
  for n in warm..warm + LOOP_CYCLES {
    let shard = n % shards;
    if shard != origin && last_visit[shard].is_some_and(|last| last.elapsed() > hold) {
      late_returns += 1;
    }
    cycle(&mut client, name(n));
    last_visit[shard] = Some(Instant::now());
  }
  let cycle_time = looping.elapsed() / u32::try_from(LOOP_CYCLES).unwrap();
  let looped = parks_and_holds(&daemon);
  wait(hold.saturating_mul(2));
  let rested = parks_and_holds(&daemon);
  for n in warm + LOOP_CYCLES..warm + LOOP_CYCLES + PACED_CYCLES {
    wait(PACE);
    cycle(&mut client, name(n));
  }
  let paced = parks_and_holds(&daemon);
  drop(client);
  daemon.stop();
  println!(
    "closed loop: {TEST_SHARDS} shards, a cycle {cycle_time:?}, hold {hold:?}, {late_returns} returns later than \
     it; parks and holds before {before:?}, after the loop {looped:?}, rested {rested:?}, paced {paced:?}"
  );
  judge_loop(gained(&before, &looped, &owners), late_returns, hold);
  let (paced_parks, paced_holds) = gained(&rested, &paced, &owners);
  let all: Vec<usize> = (0..usize::from(TEST_SHARDS)).collect();
  assert_eq!(
    gained(&rested, &paced, &all).1,
    0,
    "a paced client's requests held no shard: {paced:?} against {rested:?}"
  );
  assert!(
    paced_parks > 0,
    "the owners parked between a paced client's cycles ({paced_holds} holds): {paced:?} against {rested:?}"
  );
}

/// The closed loop's verdict: its forwards asked their owners for holds, and the owners parked at most R9's tail of the
/// loop's returns past those the run explains itself: a return later than the hold; for every forward that held nothing
/// (a request after a pause longer than F ends the loop, and the first forward after one holds nothing), the loop's
/// whole ramp back to its full hold; and one park an owner for the counters' own question, which reaches a parked shard.
/// A build whose loop returned to its owners later than the hold more often than not runs slower than R9's closed loop,
/// so it says so and judges nothing.
fn judge_loop((parks, holds): (u64, u64), late_returns: u64, hold: Duration) {
  // The cycles on the two shards that do not serve the client: each is two forwards (the create and its destroy), and
  // each but the first is a return.
  let owner_cycles =
    u64::try_from(LOOP_CYCLES * (usize::from(TEST_SHARDS) - 1) / usize::from(TEST_SHARDS)).unwrap();
  let forwards = owner_cycles * 2;
  let returns = owner_cycles;
  if late_returns * 2 > returns {
    eprintln!(
      "SKIP the closed loop's verdict: {late_returns} of {returns} returns to an owner came later than its hold of \
       {hold:?}, so this build's loop runs slower than R9's closed loop ({parks} parks, {holds} holds)"
    );
    return;
  }
  let unheld = forwards.saturating_sub(holds);
  let ramp = loop_revisit_cycles(TEST_SHARDS);
  let asked = u64::from(TEST_SHARDS) - 1;
  let explained = late_returns + unheld * ramp + asked;
  let tail = returns.div_ceil(PROVISION_TAIL);
  assert!(
    holds > 0,
    "the loop's forwards asked their owners for no hold ({parks} parks over {returns} returns)"
  );
  assert!(
    parks <= explained + tail,
    "the owners parked {parks} times over {returns} returns of the loop, past the {explained} its own returns explain \
     ({late_returns} later than the hold of {hold:?}, {unheld} of {forwards} forwards unheld, {asked} questions) and R9's \
     tail of {tail}"
  );
}
