//! A client in R9's closed loop finds the shards it goes round still polling, and a paced client holds none (A-326;
//! §4.7). A closed-loop client sends its next request within F of its last reply, so R9's floor keeps its own shard
//! polling (A-223's window rule). But its creates hash over the shards and each later verb goes to its volume's owner,
//! so with three or more shards an owner hears from the loop only after it has gone round the others, and at R9's floor
//! alone the owner had parked by then: on ubuntu-24.04-arm's three shards the quiet single-client p99 was 116.4 µs
//! against main's 27.1, its shards parking 144 to 331 times per 1,000 creates (step 6 of the A-223 plan, diag-liveness-42
//! and -43). A forward made for a request that continues the loop now holds its owner's window for the loop's return at
//! R9's percentile (`config::closed_loop_hold`), never longer than the loop had already run (`verbs::loop_hold`). The
//! owner keeps the hold only while the idle turn a request would land in, as it has learned its own turns, is shorter
//! than its wake estimate (A-387, `slates_rt::held_turn`): a macOS shard's zero-timeout `kevent` naps 12–15 µs a turn
//! (A-218), longer than a kicked park's wake, so there the owners decline and park between the loop's returns.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, NamePolicy, SizeClass};
use slates_server::config::{
  CLOSED_LOOP_CYCLE_NS, PROVISION_TAIL, closed_loop_hold, loop_revisit_cycles,
};
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

/// One shard's counters at a moment: its parks, the holds it kept and those it declined, the idle turn a request meets
/// as it has learned its own, and its wake estimate (A-326, A-387).
#[derive(Clone, Copy, Debug, Default)]
struct Reading {
  parks: u64,
  holds: u64,
  declined: u64,
  turn_ns: u64,
  wake_ns: u64,
}

impl Reading {
  /// Whether the shard's learned idle turn was no shorter than its wake estimate: a hold asked now is declined.
  fn naps(&self) -> bool {
    self.turn_ns >= self.wake_ns
  }
}

/// Each shard's reading now, by position.
fn readings(daemon: &Daemon) -> Vec<Reading> {
  daemon
    .shards()
    .iter()
    .map(|shard| {
      daemon
        .observation(Some(*shard), OBSERVE_BUDGET_NS, |_: &mut ShardState| {
          slates_rt::registry::with_current(|ctx| {
            let counters = ctx.counters();
            Reading {
              parks: counters.waits,
              holds: counters.idle_holds,
              declined: counters.idle_holds_declined,
              turn_ns: counters.held_turn_ns,
              wake_ns: counters.wake_cost_ns,
            }
          })
          .unwrap_or_default()
        })
        .unwrap()
        .wait()
        .unwrap()
    })
    .collect()
}

/// What the shards in `owners` gained between two readings, summed: parks, holds kept and holds declined.
#[derive(Clone, Copy, Debug, Default)]
struct Gained {
  parks: u64,
  holds: u64,
  declined: u64,
}

fn gained(before: &[Reading], after: &[Reading], owners: &[usize]) -> Gained {
  owners.iter().fold(Gained::default(), |sum, owner| Gained {
    parks: sum.parks + (after[*owner].parks - before[*owner].parks),
    holds: sum.holds + (after[*owner].holds - before[*owner].holds),
    declined: sum.declined + (after[*owner].declined - before[*owner].declined),
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

/// What one run of the loop measured from the client: each owner's returns (the time from its last cycle's end to its
/// next one's start, the least the owner sat without the loop), and the mean cycle.
struct LoopRun {
  returns: Vec<Duration>,
  cycle: Duration,
}

impl LoopRun {
  /// The returns later than `bound`.
  fn later_than(&self, bound: Duration) -> u64 {
    u64::try_from(self.returns.iter().filter(|gap| **gap > bound).count()).unwrap()
  }

  /// The longest return.
  fn longest(&self) -> Duration {
    self.returns.iter().copied().max().unwrap_or_default()
  }
}

impl std::fmt::Debug for LoopRun {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let cycle_bound = Duration::from_nanos(CLOSED_LOOP_CYCLE_NS);
    f.debug_struct("LoopRun")
      .field("returns", &self.returns.len())
      .field("later_than_2f", &self.later_than(cycle_bound))
      .field("longest", &self.longest())
      .field("cycle", &self.cycle)
      .finish()
  }
}

/// Runs the loop's `cycles`, cycle `n` on owner `n % owners` with its `n / owners`th spec, and measures each owner's
/// returns.
fn run_loop(
  client: &mut Client,
  specs: &[Vec<CreateSpec>],
  cycles: std::ops::Range<usize>,
) -> LoopRun {
  let mut last_visit: Vec<Option<Instant>> = vec![None; specs.len()];
  let mut returns = Vec::with_capacity(cycles.len());
  let (count, started) = (cycles.len(), Instant::now());
  for n in cycles {
    let owner = n % specs.len();
    if let Some(gap) = last_visit[owner].map(|last| last.elapsed()) {
      returns.push(gap);
    }
    run_cycle(client, &specs[owner][n / specs.len()]);
    last_visit[owner] = Some(Instant::now());
  }
  let cycle = started.elapsed() / u32::try_from(count.max(1)).unwrap();
  LoopRun { returns, cycle }
}

/// Runs [`PACED_CYCLES`] cycles [`PACE`] apart, round every shard: on the client's own shard (`own`) every `shards`th
/// cycle, and on the owners in turn (`looped`, from cycle `from` on) between.
fn run_paced(
  client: &mut Client,
  (own, looped): (&[CreateSpec], &[Vec<CreateSpec>]),
  from: usize,
  shards: usize,
) {
  let mut next = from;
  for n in 0..PACED_CYCLES {
    wait(PACE);
    if n % shards == 0 {
      run_cycle(client, &own[n / shards]);
    } else {
      run_cycle(client, &looped[next % looped.len()][next / looped.len()]);
      next += 1;
    }
  }
}

/// Waits `span` on the test thread, however early a park returns.
fn wait(span: Duration) {
  let started = Instant::now();
  while started.elapsed() < span {
    std::thread::park_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// What the scenario measured: the loop's own run, the shards that own its volumes, and each shard's reading before
/// the loop, after it, after the rest and after the paced cycles; and the daemon's hold for a closed loop.
struct Scenario {
  looped: LoopRun,
  owners: Vec<usize>,
  before: Vec<Reading>,
  after_loop: Vec<Reading>,
  rested: Vec<Reading>,
  paced: Vec<Reading>,
  hold: Duration,
}

/// Runs the scenario every platform's test judges: on a three-shard daemon, one client's closed loop of creates and
/// destroys, a cycle on each of the two shards that do not serve the client in turn, first long enough for the loop's
/// run to reach its full hold, then [`LOOP_CYCLES`] cycles measured; then a rest, and [`PACED_CYCLES`] cycles a
/// millisecond apart over all three shards, the client's own among them. The client spins for its reply as long as a
/// loop's return at R9's percentile (`config::closed_loop_hold`), whatever its owners keep, so its own wake never
/// stretches a cycle and the loop is R9's closed loop on every platform.
fn run_scenario() -> Scenario {
  let profile = common::machine_profile();
  let instance = format!("srv-closed-loop-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let hold = Duration::from_nanos(config.closed_loop_hold_ns);
  let spin_ns = closed_loop_hold(TEST_SHARDS).get();
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
  client.spin_for(Some(spin_ns));
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
  run_loop(&mut client, &looped_specs, 0..warm);
  let before = readings(&daemon);
  let looped = run_loop(&mut client, &looped_specs, warm..warm + LOOP_CYCLES);
  let after_loop = readings(&daemon);
  // The rest outlasts any hold the loop asked for, so the paced cycles start with every shard back to its own window.
  wait(Duration::from_nanos(spin_ns).saturating_mul(2));
  let rested = readings(&daemon);
  run_paced(
    &mut client,
    (&own_specs, &looped_specs),
    warm + LOOP_CYCLES,
    shards,
  );
  let paced = readings(&daemon);
  drop(client);
  daemon.stop();
  println!(
    "closed loop: {TEST_SHARDS} shards, {looped:?}, hold {hold:?}; owners {owners:?}; readings before {before:?}, after \
     the loop {after_loop:?}, rested {rested:?}, paced {paced:?}"
  );
  Scenario {
    looped,
    owners,
    before,
    after_loop,
    rested,
    paced,
    hold,
  }
}

/// A-326, A-387, R9 (AC-2.1), §4.7. Do: run the scenario ([`run_scenario`]). Expect, in the loop: holds asked of the
/// two owners, kept or declined (the non-vacuity count), each owner deciding by what it measured. An owner whose
/// learned idle turn stayed at least its wake estimate through the loop declined holds, and one whose turn stayed
/// shorter kept them: a macOS shard's zero-timeout `kevent` naps 12–15 µs a turn (A-218), longer than a kicked park's
/// wake, where a Linux or Windows shard's poll returns at once. One whose turn crossed its wake during the loop is
/// reported and not judged. Where the owners kept no hold, they parked at least once for every return of the loop later
/// than a cycle of R9's closed loop (2F), R9's tail aside; where they kept holds, at most R9's tail of parks past those
/// the run's own returns explain (A-326's verdict). In the paced cycles: no hold asked of any shard, though a destroy
/// on the client's own shard leaves work running through the pause (judged by the shard's last work, 1 to 3 of 24
/// paced cycles looked like a loop's, 2026-11-18), and the owners parking between cycles. At R9's floor alone, before
/// A-326, the owners parked 460 times over 200 returns (the Mac Studio, a debug cycle 272 µs). On the Mac Studio with
/// every hold kept (main before A-387) the owners kept 261 and 164 holds and parked 173 and 315 times over 198 returns
/// later than 2F; with no hold asked (A-387's first form) they parked 416 to 419 times over the same 198 (2026-11-18,
/// debug, load 25 to 31). With the decision (debug, load 14): the owners' learned turns 14.7 and 16.2 µs against wakes
/// of 5.7 and 5.8 µs, 188 and 192 holds declined and none kept in the loop, 416 parks over 198 late returns; with the
/// decision forced to hold, they kept 192 and 196 holds while their turns stayed over their wakes, and the test failed.
#[test]
fn a_closed_loops_owners_hold_only_while_their_idle_turns_poll_faster_than_they_wake() {
  let scenario = run_scenario();
  let in_loop = gained(&scenario.before, &scenario.after_loop, &scenario.owners);
  assert!(
    in_loop.holds + in_loop.declined > 0,
    "the loop's forwards asked no hold of its owners: {in_loop:?}"
  );
  for owner in &scenario.owners {
    let (before, after) = (scenario.before[*owner], scenario.after_loop[*owner]);
    let (kept, declined) = (after.holds - before.holds, after.declined - before.declined);
    if before.naps() != after.naps() {
      eprintln!(
        "owner {owner}'s learned idle turn crossed its wake during the loop ({before:?} to {after:?}): its {kept} kept \
         and {declined} declined holds are not judged"
      );
    } else if after.naps() {
      assert!(
        declined > 0,
        "owner {owner}'s idle turn a request meets, {} ns, stayed no shorter than its wake, {} ns, yet it declined no \
         hold ({kept} kept): {after:?}",
        after.turn_ns,
        after.wake_ns
      );
    } else {
      assert!(
        kept > 0,
        "owner {owner}'s idle turn a request meets, {} ns, stayed shorter than its wake, {} ns, yet it kept no hold \
         ({declined} declined): {after:?}",
        after.turn_ns,
        after.wake_ns
      );
    }
  }
  if in_loop.holds == 0 {
    judge_unheld_loop(in_loop, &scenario.looped);
  } else {
    judge_loop(in_loop, &scenario.looped, scenario.hold);
  }
  judge_paced(&scenario);
}

/// The verdict of a loop whose owners kept no hold (A-387): they parked at least once for every return of the loop
/// later than a cycle of R9's closed loop (2F), R9's tail aside, since a return that late left its owner idle past its
/// own window.
fn judge_unheld_loop(in_loop: Gained, looped: &LoopRun) {
  let late = looped.later_than(Duration::from_nanos(CLOSED_LOOP_CYCLE_NS));
  let tail = u64::try_from(LOOP_CYCLES).unwrap().div_ceil(PROVISION_TAIL);
  assert!(
    in_loop.parks + tail >= late,
    "the owners kept no hold and parked {} times over the loop's {late} returns later than 2F (R9's tail {tail}): \
     {in_loop:?}",
    in_loop.parks
  );
}

/// The paced cycles' verdict, on every platform: no hold asked of any shard, and the owners parking between cycles.
fn judge_paced(scenario: &Scenario) {
  let paced = gained(&scenario.rested, &scenario.paced, &scenario.owners);
  let all: Vec<usize> = (0..usize::from(TEST_SHARDS)).collect();
  let everywhere = gained(&scenario.rested, &scenario.paced, &all);
  assert_eq!(
    everywhere.holds + everywhere.declined,
    0,
    "a paced client's requests asked no shard for a hold: {:?} against {:?}",
    scenario.paced,
    scenario.rested
  );
  assert!(
    paced.parks > 0,
    "the owners parked between a paced client's cycles: {:?} against {:?}",
    scenario.paced,
    scenario.rested
  );
}

/// The closed loop's verdict: its forwards asked their owners for holds, and the owners parked at most R9's tail of the
/// loop's returns past those the run explains itself, judged against the loop's measured period: a return later than the
/// hold; for every forward that held nothing (a request read later than F after the client's last reply ends the loop,
/// and the first forward after it holds nothing), the returns its ramp back can still miss, as many as cycles of 2F fit
/// in the longest return the loop measured; and one park an owner for the counters' own question, which reaches a
/// parked shard. Fewer than half the forwards held means the build ran slower than R9's closed loop at least as often
/// as within it, so the run says so and judges nothing.
fn judge_loop(in_loop: Gained, looped: &LoopRun, hold: Duration) {
  let (parks, holds) = (in_loop.parks, in_loop.holds);
  let (late_returns, longest_return) = (looped.later_than(hold), looped.longest());
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
