//! A client's ring wakes only the shard that serves it (A-381; §4.7, D-10). On macOS and Windows a client rings the
//! bootstrap object's daemon-wide doorbell when its shard has announced it is going idle, and the daemon's doorbell
//! thread kicks shards. Until 2026-11-18 it kicked every shard at every ring: at 1,000 clients over 23 shards on a Mac
//! Studio each shard parked and woke about 7,500 times a second, alike on shards serving one connection and two, so each
//! request sent to a parked shard woke the 22 others too. A ring now marks the shard its client's region names, and the
//! thread kicks the shards marked. On Linux a client writes its shard's own kick descriptor, so the same holds by
//! construction there.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, NamePolicy, SizeClass};
use slates_server::daemon::OBSERVE_BUDGET_NS;
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: three shards: the client's own, the control shard (which a create may reach for the catalog), and at least
/// one shard that is neither and that nothing of the client's touches, the one the rings must leave asleep.
const TEST_SHARDS: u16 = 3;
/// Shape: the client's paced cycles, each a create and a destroy of a volume its own shard owns: a ring a cycle, enough
/// that one shard woken by every ring stands far above one woken only by its own timers.
const CYCLES: usize = 100;
/// Shape: the pause before each cycle, a millisecond: twenty times R9's floor F, the idle window, so the client's shard
/// has announced itself idle and the cycle's create rings the doorbell.
const PACE: Duration = Duration::from_millis(1);
/// Shape: the bounded volume each cycle creates and destroys.
const VOLUME_BYTES: u64 = 1 << 20;
/// Shape: names tried for the ones the client's shard owns: about a third of names land on each of three partitions.
const NAME_TRIES: usize = 8 * CYCLES;
/// Shape: the share of the client's own shard's wakes past which a shard nothing asked is counted woken by the rings:
/// one tenth. Woken by every ring it gains as many as the client's shard, and by its own timers alone a handful.
const ASLEEP_SHARE: u64 = 10;

/// The shard (by position, its partition) that serves the client `client_id`.
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

/// Each shard's parks so far, by position.
fn parks(daemon: &Daemon) -> Vec<u64> {
  daemon
    .shards()
    .iter()
    .map(|shard| {
      daemon
        .observation(Some(*shard), OBSERVE_BUDGET_NS, |_: &mut ShardState| {
          slates_rt::registry::with_current(|ctx| ctx.counters().waits).unwrap_or_default()
        })
        .unwrap()
        .wait()
        .unwrap()
    })
    .collect()
}

/// `count` names the partition at `partition` owns.
fn names_on(partition: usize, count: usize) -> Vec<String> {
  (0..NAME_TRIES)
    .map(|n| format!("ring-{n}"))
    .filter(|name| {
      usize::from(slates_server::verbs::owner_of_name(
        name,
        usize::from(TEST_SHARDS),
      )) == partition
    })
    .take(count)
    .collect()
}

/// Waits `span` on the test thread, however early a park returns.
fn wait(span: Duration) {
  let started = Instant::now();
  while started.elapsed() < span {
    std::thread::park_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// One cycle: create the volume `name` names on the client's own shard, then destroy it.
fn run_cycle(client: &mut Client, name: String) {
  let volume = client
    .create(&CreateSpec {
      name,
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

/// A-381 (§4.7, D-10; AC-2.6's wake strategy). Do: on a three-shard daemon, one client runs [`CYCLES`] cycles a
/// millisecond apart, each a create and a destroy of a volume its own shard owns, so its shard idles between cycles and
/// each cycle's create rings the doorbell. Expect: the client's shard parked and was woken at least once a cycle (the
/// rings happened); and every shard that is neither the client's nor the control shard woke fewer times than a tenth of
/// that. Before A-381, on macOS and Windows, every ring kicked every shard, and such a shard woke once a ring.
#[test]
fn a_clients_ring_wakes_only_the_shard_that_serves_it() {
  let profile = common::machine_profile();
  let instance = format!("srv-doorbell-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "doorbell"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = common::client::connect(&instance);
  let own = serving(&daemon, client.client_id());
  let mut names = names_on(own, CYCLES + 1).into_iter();
  run_cycle(&mut client, names.next().unwrap());
  let before = parks(&daemon);
  for name in names {
    wait(PACE);
    run_cycle(&mut client, name);
  }
  let after = parks(&daemon);
  drop(client);
  daemon.stop();
  let gained: Vec<u64> = before
    .iter()
    .zip(&after)
    .map(|(before, after)| after - before)
    .collect();
  println!(
    "doorbell: {TEST_SHARDS} shards, the client's shard {own}, {CYCLES} paced cycles, parks gained by shard {gained:?}"
  );
  assert!(
    gained[own] >= u64::try_from(CYCLES).unwrap(),
    "the client's shard parked between its cycles, so each cycle rang: {gained:?}"
  );
  // Position 0 is the control shard, which a create may reach.
  let asleep: Vec<usize> = (1..usize::from(TEST_SHARDS))
    .filter(|shard| *shard != own)
    .collect();
  assert!(!asleep.is_empty(), "three shards leave one nothing asks");
  for shard in asleep {
    assert!(
      gained[shard] * ASLEEP_SHARE < gained[own],
      "shard {shard}, which nothing of the client's asked, woke {} times against the client's shard's {}: the rings \
       kicked it ({gained:?})",
      gained[shard],
      gained[own]
    );
  }
}
