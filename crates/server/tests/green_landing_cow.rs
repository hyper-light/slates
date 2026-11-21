//! A green's landing copies none of its head (A-774 part 3; §4.15 over §4.16): presenting a G-byte green's landing and
//! landing it under a grant each add a fixed term resident, never a copy of the head; the landing view reads the green
//! where it lies and copies each file to disk a window at a time. Its own test binary, so no other test's allocations
//! fall inside its measurements.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use slates_client::{Client, Submitted};
use slates_ipc::protocol::VolumeId;
use slates_machine::facts::Facts;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: the green's files; with [`FILE_BYTES`] a 64 MiB green (`SLATES_MOUNT_COW_FILES` overrides it).
const FILES: usize = 1024;
/// Shape: one file's bytes.
const FILE_BYTES: usize = 64 * 1024;
/// Bound: what the run may add once, whatever the landing does (the allocator's page granularity, a map's growth).
const FIXED_BYTES: usize = 8 * 1024 * 1024;

fn named(key: &str, default: usize) -> usize {
  std::env::var(key)
    .ok()
    .map_or(default, |value| value.parse().unwrap())
}

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-green-land-{tag}-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(1)),
    slates_server::SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("green-land-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn path(index: usize) -> String {
  format!("dir-{:02}/file-{index:05}", index % 64)
}

/// A green named `name` of `files` files of [`FILE_BYTES`] each, landed by one seed work.
fn populated_green(client: &mut Client, name: &str, files: usize) -> VolumeId {
  let green = client.create_green(name, false).unwrap();
  let (seed, _) = client.create_work(green, &format!("{name}-seed")).unwrap();
  let mut bytes = vec![0u8; FILE_BYTES];
  for index in 0..files {
    bytes.fill(u8::try_from(index % 251).unwrap());
    client.edit(seed, &path(index), 0, 0, &bytes).unwrap();
  }
  match client.submit(seed).unwrap() {
    Submitted::Accepted(_) => green,
    other => panic!("the seed's submit was not accepted: {other:?}"),
  }
}

fn resident() -> usize {
  usize::try_from(Facts::resident_now().expect("this OS reports resident memory")).unwrap()
}

/// Bound: what a landing of the green may add resident besides a fixed term: the windows its writer copies through
/// (one window a file in flight) and its plan's entries. A copy of the head (64 MiB) is far past it.
const LANDING_BYTES: usize = 8 * 1024 * 1024;

/// A-774 part 3 (§4.15 over §4.16). Do: over a 64 MiB green of 1,024 files, present its landing into a fresh directory,
/// then grant and land it. Expect: the presentation and the landing each add at most a fixed term resident, never a copy
/// of the head; every landed file is the green's.
#[test]
fn presenting_and_landing_a_green_copy_none_of_its_head() {
  let files = named("SLATES_MOUNT_COW_FILES", FILES);
  let (daemon, instance) = daemon("land");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let large = populated_green(&mut client, "cow-land", files);
  let target = common::target::target_dir();

  let presenting = resident();
  let grant = common::landing::approve(&mut client, &secret, large, None, &target.path);
  let presented = resident().saturating_sub(presenting);
  let landing = resident();
  match client.land(
    large,
    None,
    &target.path,
    slates_ipc::protocol::Filter::default(),
    Some(grant),
  ) {
    Ok(slates_client::Landing::Landed(outcome)) => assert_eq!(outcome.state, "done"),
    other => panic!("the granted landing of the green: {other:?}"),
  }
  let landed = resident().saturating_sub(landing);
  println!(
    "land_cow: green {} B in {files} files; presenting +{presented} B, landing +{landed} B, bound {} B",
    files * FILE_BYTES,
    FIXED_BYTES + LANDING_BYTES
  );
  for index in [0, files / 2, files - 1] {
    let bytes = std::fs::read(format!("{}/{}", target.path, path(index))).unwrap();
    assert_eq!(bytes.len(), FILE_BYTES);
    assert!(
      bytes
        .iter()
        .all(|byte| *byte == u8::try_from(index % 251).unwrap())
    );
  }
  assert!(
    presented <= FIXED_BYTES + LANDING_BYTES,
    "presenting the green's landing added {presented} resident bytes: a copy of its head"
  );
  assert!(
    landed <= FIXED_BYTES + LANDING_BYTES,
    "landing the green added {landed} resident bytes: a copy of its head"
  );
  drop(client);
  daemon.stop();
}
