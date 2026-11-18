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
use slates_server::config::{CLOSED_LOOP_CYCLE_NS, PROVISION_TAIL, loop_revisit_cycles};
use slates_server::daemon::OBSERVE_BUDGET_NS;
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: three shards, ubuntu-24.04-arm's derived count where R9's floor alone failed. The loop goes round the two that
/// do not serve the client, a cycle on each in turn, so the client's own shard only reads requests and writes replies
/// and the loop stays as tight as the build allows. Three shards and the client fit a four-CPU runner, so the shards a
/// hold keeps polling never take a CPU from the loop itself.
const TEST_SHARDS: u16 = 3;
/// Shape: the loop's measured cycles, each a create and its destroy on one of the two owners: a hundred returns to each,
/// so R9's tail of one in a hundred is a count.
const LOOP_CYCLES: usize = 200;
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

/// The create a cycle makes for the volume `name`.
fn spec(name: String) -> CreateSpec {
  CreateSpec {
    name,
    size: SizeClass::Bounded {
      limit: VOLUME_BYTES,
    },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// One cycle: create the volume `spec` names, then destroy it.
fn run_cycle(client: &mut Client, spec: &CreateSpec) {
  let volume = client.create(spec).unwrap();
  client.destroy(volume).unwrap();
}

/// Waits `span` on the test thread, however early a park returns.
fn wait(span: Duration) {
  let started = Instant::now();
  while started.elapsed() < span {
    std::thread::park_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// A-326, R9 (AC-2.1), §4.7. Do: on a three-shard daemon, run one client's closed loop of creates and destroys, a cycle
/// on each of the two shards that do not serve the client in turn, first long enough for the loop's run to reach its
/// full hold, then [`LOOP_CYCLES`] cycles measured; then pause, and run [`PACED_CYCLES`] cycles a millisecond apart over
/// all three shards, the client's own among them. Expect, in the loop: holds asked of the two owners (the non-vacuity
/// count), and at most R9's tail of parks on them past those the run's own returns explain, judged against the loop's
/// measured period; in the paced cycles: no hold asked of any shard, though a destroy on the client's own shard leaves
/// work running through the pause (judged by the shard's last work, 3 of 24 paced cycles looked like a loop's, 2026-11-18),
/// and the owners parking between cycles (R9's floor alone, as A-223's window rule leaves a paced client). At R9's floor
/// alone, before A-326, the owners parked 460 times over 200 returns (2026-11-18, the Mac Studio, a debug cycle 272 µs).
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
  // Enough cycles that the loop's run has passed its full hold (two requests a cycle), then the measured loop over the
  // owners, then the paced cycles over every shard, the client's own among them: a destroy there leaves its slices
  // running through the pause, background work that must not look like a loop's (A-326).
  let warm = usize::try_from(loop_revisit_cycles(TEST_SHARDS)).unwrap();
  let per_owner = (warm + LOOP_CYCLES + PACED_CYCLES).div_ceil(owners.len());
  let looped_specs: Vec<Vec<CreateSpec>> = owners
    .iter()
    .map(|owner| names_on(*owner, per_owner).into_iter().map(spec).collect())
    .collect();
  let own_specs: Vec<CreateSpec> = names_on(origin, PACED_CYCLES.div_ceil(shards))
    .into_iter()
    .map(spec)
    .collect();
  let looped = |cycle: usize| &looped_specs[cycle % owners.len()][cycle / owners.len()];
  for n in 0..warm {
    run_cycle(&mut client, looped(n));
  }
  let before = parks_and_holds(&daemon);
  let mut last_visit: Vec<Option<Instant>> = vec![None; owners.len()];
  let (mut late_returns, mut longest_return) = (0, Duration::ZERO);
  let looping = Instant::now();
  for n in warm..warm + LOOP_CYCLES {
    let owner = n % owners.len();
    if let Some(last) = last_visit[owner] {
      let gap = last.elapsed();
      longest_return = longest_return.max(gap);
      if gap > hold {
        late_returns += 1;
      }
    }
    run_cycle(&mut client, looped(n));
    last_visit[owner] = Some(Instant::now());
  }
  let cycle_time = looping.elapsed() / u32::try_from(LOOP_CYCLES).unwrap();
  let after_loop = parks_and_holds(&daemon);
  wait(hold.saturating_mul(2));
  let rested = parks_and_holds(&daemon);
  // The paced cycles go round every shard: on an owner through a forward, on the client's own shard served there.
  let mut next_owner_cycle = warm + LOOP_CYCLES;
  for n in 0..PACED_CYCLES {
    wait(PACE);
    if n % shards == 0 {
      run_cycle(&mut client, &own_specs[n / shards]);
    } else {
      run_cycle(&mut client, looped(next_owner_cycle));
      next_owner_cycle += 1;
    }
  }
  let paced = parks_and_holds(&daemon);
  drop(client);
  daemon.stop();
  println!(
    "closed loop: {TEST_SHARDS} shards, a cycle {cycle_time:?}, hold {hold:?}, returns at most {longest_return:?} \
     apart and {late_returns} later than the hold; parks and holds before {before:?}, after the loop {after_loop:?}, \
     rested {rested:?}, paced {paced:?}"
  );
  judge_loop(
    gained(&before, &after_loop, &owners),
    (late_returns, longest_return),
    hold,
  );
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
/// loop's returns past those the run explains itself, judged against the loop's measured period: a return later than the
/// hold; for every forward that held nothing (a request read later than F after the client's last reply ends the loop,
/// and the first forward after it holds nothing), the returns its ramp back can still miss, as many as cycles of 2F fit
/// in the longest return the loop measured; and one park an owner for the counters' own question, which reaches a
/// parked shard. Fewer than half the forwards held means the build ran slower than R9's closed loop at least as often
/// as within it, so the run says so and judges nothing.
fn judge_loop(
  (parks, holds): (u64, u64),
  (late_returns, longest_return): (u64, Duration),
  hold: Duration,
) {
  let returns = u64::try_from(LOOP_CYCLES).unwrap();
  let forwards = returns * 2;
  if holds * 2 < forwards {
    eprintln!(
      "SKIP the closed loop's verdict: {holds} of {forwards} forwards held, so this build's loop ran slower than R9's \
       closed loop as often as within it ({parks} parks over {returns} returns)"
    );
    return;
  }
  let unheld = forwards - holds;
  let ramp = u64::try_from(longest_return.as_nanos())
    .unwrap()
    .div_ceil(CLOSED_LOOP_CYCLE_NS);
  let asked = u64::from(TEST_SHARDS) - 1;
  let explained = late_returns + unheld * ramp + asked;
  let tail = returns.div_ceil(PROVISION_TAIL);
  assert!(
    parks <= explained + tail,
    "the owners parked {parks} times over {returns} returns of the loop, past the {explained} its own returns explain \
     ({late_returns} later than the hold of {hold:?}; {unheld} of {forwards} forwards unheld, each missing at most \
     {ramp} returns of the longest, {longest_return:?}; {asked} questions) and R9's tail of {tail}"
  );
}
