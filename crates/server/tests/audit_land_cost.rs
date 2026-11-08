//! Measurement (not a gate): a granted landing's time when its volume's owner is not the control shard (§4.15, A-265),
//! the case a landing's audit records cross shards to reach the control partition. Run in release:
//! `cargo test --release -p slates-server --test audit_land_cost -- --ignored --nocapture`.
// Test harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::cast_precision_loss
)]
// A landing writes a real target directory: macOS and Linux.
#![cfg(unix)]

use std::time::Instant;

use slates_client::{CreateSpec, Filter, Landing, NamePolicy, SizeClass};
use slates_ipc::protocol::Intent;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;
use common::target::target_dir;

/// Shape: two shards, so the volume's owner can be the shard that is not the control shard.
const SHARDS: u16 = 2;
/// Shape: the files landed, each an audit record of its own (`entry_written`).
const FILES: usize = 1_000;
/// Shape: each file's bytes: small, so the landing's time is its per-entry work.
const FILE_BYTES: usize = 1_024;
/// Shape: the landings timed; the best is reported, and all.
const ROUNDS: usize = 5;

#[test]
#[ignore = "measurement, not a gate"]
fn a_granted_landing_from_a_shard_that_is_not_the_control_shard() {
  let profile = common::machine_profile();
  let instance = format!("srv-audit-land-cost-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-audit-land-cost-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  // Each round lands a volume of its own: a volume landed once has nothing left to land into a fresh target.
  let mut names = (0..)
    .map(|n| format!("landed-{n}"))
    .filter(|name| slates_server::verbs::owner_of_name(name, usize::from(SHARDS)) != 0);
  let bytes = vec![b'x'; FILE_BYTES];
  let mut times = Vec::new();
  for _ in 0..ROUNDS {
    let volume = client
      .create(&CreateSpec {
        name: names.next().unwrap(),
        size: SizeClass::Bounded { limit: 1 << 26 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    let attachment = client
      .attach(volume, None, Intent::Write)
      .unwrap()
      .attachment;
    for n in 0..FILES {
      client
        .fs_write((volume, attachment), &format!("f{n:05}.txt"), &bytes, 0o644)
        .unwrap();
    }
    let target = target_dir();
    let grant = common::landing::approve(&mut client, &secret, volume, None, &target.path);
    let started = Instant::now();
    let landed = client.land(volume, None, &target.path, Filter::default(), Some(grant));
    let took = started.elapsed();
    assert!(
      matches!(&landed, Ok(Landing::Landed(outcome)) if outcome.written == FILES as u64),
      "{landed:?}"
    );
    times.push(took);
  }
  let best = times.iter().min().unwrap();
  eprintln!(
    "AUDIT-LAND-COST {FILES} files of {FILE_BYTES} B from shard 1: best {best:?} ({:.1} us an entry), all {times:?}",
    best.as_secs_f64() * 1e6 / FILES as f64
  );
  drop(client);
  daemon.stop();
}
