//! An overlay volume's drift at the scale a base reaches (§4.4 `status`, A-267): the status stays one reply, carrying
//! the first drifted entries that fit and the count of the rest, and a paged read returns every drifted entry. Before
//! 2026-11-08 the whole drift list rode in the status, and a volume with 600 drifted files had its status refused
//! "reply too large for the bulk area".
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result,
  clippy::disallowed_methods
)]
// An overlay over a real host directory: macOS and Linux.
#![cfg(unix)]

use std::time::{Duration, Instant};

use slates_client::{CreateSpec, NamePolicy, SizeClass};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;
use common::target::target_dir;

/// Shape: one shard, the overlay's owner.
const TEST_SHARDS: u16 = 1;
/// Shape: the base's files, every one changed on disk after it is pinned: several replies' worth of drift.
const DRIFTED: usize = 600;
/// Shape: how long the base's watcher may take to see every change.
const WATCH_WAIT: Duration = Duration::from_secs(30);

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-drift-pages-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-drift-pages-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn file(n: usize) -> String {
  format!("drifting-file-{n:05}.txt")
}

/// §4.4, A-267. Do: overlay a host directory of 600 files, pin them all, change every file on disk, and ask the
/// volume's status until the drift is seen; then read the drift in pages. Expect: the status answers (one reply), the
/// drifted entries it carries plus the count it elides make 600, and the paged read holds all 600 once each, the
/// status's entries its first. Before 2026-11-08 the status was refused
/// `BadRequest { reason: "reply too large for the bulk area" }`.
#[test]
fn a_volume_with_more_drift_than_one_reply_holds_still_answers_its_status() {
  let (daemon, instance) = daemon("status");
  let mut client = connect(&instance);
  let base = target_dir();
  for n in 0..DRIFTED {
    base.seed(&file(n), b"before");
  }
  let volume = client
    .create(&CreateSpec {
      name: "drifting".into(),
      size: SizeClass::Dynamic { max: 1 << 26 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: Some(base.path.clone()),
    })
    .unwrap();
  assert_eq!(client.pin(volume, None).unwrap(), DRIFTED as u64);
  for n in 0..DRIFTED {
    base.seed(&file(n), b"after, and longer");
  }
  let started = Instant::now();
  loop {
    let report = client
      .status(volume)
      .expect("the status of a volume with this much drift");
    let seen = report.drifted.len() + report.drifted_elided as usize;
    if seen >= DRIFTED {
      assert_eq!(seen, DRIFTED, "every drifted file, carried or counted");
      assert!(
        report.drifted_elided > 0,
        "the status carried {} and elided none: the test must outgrow one reply",
        report.drifted.len()
      );
      the_paged_read_holds_every_entry(&mut client, volume, &report.drifted);
      break;
    }
    assert!(
      started.elapsed() < WATCH_WAIT,
      "the drift is seen within {WATCH_WAIT:?}: {seen}"
    );
    std::thread::sleep(Duration::from_millis(100));
  }
  drop(client);
  daemon.stop();
}

/// The paged drift read of `volume` holds every drifted file once, and `carried` (the status's entries) is its start.
fn the_paged_read_holds_every_entry(
  client: &mut slates_client::Client,
  volume: slates_client::VolumeId,
  carried: &[String],
) {
  let (entries, oversized) = client.drifted(volume).expect("every drifted entry");
  assert_eq!(oversized, 0, "no entry here is too long for a page");
  assert_eq!(
    entries.len(),
    DRIFTED,
    "the paged read holds every drifted entry"
  );
  for n in 0..DRIFTED {
    assert_eq!(
      entries
        .iter()
        .filter(|entry| entry.contains(&file(n)))
        .count(),
      1,
      "{} once",
      file(n)
    );
  }
  assert!(
    carried
      .iter()
      .zip(&entries)
      .all(|(carried, read)| carried == read),
    "the status carries the first entries of the full list"
  );
}
