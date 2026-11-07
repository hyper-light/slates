//! A green's history retains what each version changed, not a copy of every file it changed (W2; §4.16 "splices
//! accepted ops into a new version by extent-list surgery without copying bytes"; GAPS "Memory per client,
//! attributed"): the engine kept a whole copy of a file's new value in its history at every version that changed it,
//! and admission charged the whole previous file to each submit, so a one-byte overwrite of a large file cost the
//! file. Behind a reader that holds the floor, the green's retention cap then refused submits after cap / file size
//! versions, though each changed a few bytes: the 3,000-client rung's 2,4xx `BudgetExceeded` per run.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use slates_client::{Client, Submitted, VolumeId};
use slates_ipc::protocol::ReadAt;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: one shard, so the green, its works and its history are on one partition.
const TEST_SHARDS: u16 = 1;
/// Shape: the green's retention cap (the partition's merge byte budget, which bounds the retention too): sixteen
/// copies of [`FILE_BYTES`], so a history that copies the file at every version meets it within about sixteen.
const RETENTION_CAP_BYTES: usize = 16 << 20;
/// Shape: the one file's size: large against each version's edit.
const FILE_BYTES: usize = 1 << 20;
/// Shape: the bytes each version overwrites.
const EDIT_BYTES: usize = 64;
/// Shape: the versions the test submits after the file's creation: four times what [`RETENTION_CAP_BYTES`] holds of
/// whole copies of the file.
const EDITS: usize = 64;
/// Shape: the distance between two versions' edits, so each overwrites its own range of the file.
const EDIT_STRIDE: usize = FILE_BYTES / EDITS;

/// Shape: the period of the file's byte pattern: a prime, so no two edits' ranges hold the same bytes before them.
const PATTERN_PERIOD: usize = 251;

/// The file's bytes at its creation: distinct per position within the period, so a misplaced range reads wrong.
fn original() -> Vec<u8> {
  (0..FILE_BYTES)
    .map(|n| u8::try_from(n % PATTERN_PERIOD).unwrap())
    .collect()
}

/// The bytes version `n`'s edit writes.
fn edit_of(n: usize) -> Vec<u8> {
  vec![u8::try_from(n % 256).unwrap(); EDIT_BYTES]
}

/// The file after the first `edits` edits.
fn after(edits: usize) -> Vec<u8> {
  let mut bytes = original();
  for n in 0..edits {
    let at = n * EDIT_STRIDE;
    bytes[at..at + EDIT_BYTES].copy_from_slice(&edit_of(n));
  }
  bytes
}

/// Submits the edits from `work`, each overwriting its own [`EDIT_BYTES`] of the file: the versions accepted, and the
/// first answer that was not an acceptance.
fn submit_edits(client: &mut Client, work: VolumeId) -> (Vec<u64>, Option<String>) {
  let mut versions = Vec::new();
  for n in 0..EDITS {
    let at = u64::try_from(n * EDIT_STRIDE).unwrap();
    let len = u64::try_from(EDIT_BYTES).unwrap();
    if let Err(e) = client.edit(work, "big", at, len, &edit_of(n)) {
      return (versions, Some(format!("edit {n}: {e}")));
    }
    match client.submit(work) {
      Ok(Submitted::Accepted(version)) => versions.push(version),
      other => return (versions, Some(format!("submit {n}: {other:?}"))),
    }
  }
  (versions, None)
}

/// W2, §4.16, §4.2. Do: on a one-shard daemon whose merge byte budget is 16 MiB, make a green and a work, create a
/// 1 MiB file, then make a second work, left at that version, which holds the reachable floor there; submit 64
/// versions from the first work, each overwriting 64 bytes of its own range of the file; then read the file through
/// the second work, at the 33rd edit's version by number, and at the head. Expect: all 64 accepted, though a history
/// that copies the file at each version holds 16 of them in the budget and refuses the rest `BudgetExceeded`; the held
/// work reads the file as created; the 33rd edit's version reads the first 33 edits; the head reads all 64.
#[test]
fn a_green_behind_a_held_floor_retains_what_its_versions_changed_not_the_file() {
  let profile = common::machine_profile();
  let instance = format!("srv-retention-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  config.caps.green_chain_bytes = RETENTION_CAP_BYTES;
  let daemon = Daemon::start(
    &profile,
    config,
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-retention-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("retention-green", false).unwrap();
  let (work, _) = client.create_work(green, "retention-work").unwrap();
  client.edit(work, "big", 0, 0, &original()).unwrap();
  let created = client.submit(work);
  let (reader, _) = client.create_work(green, "retention-reader").unwrap();
  let (versions, refused) = submit_edits(&mut client, work);
  let held = client.read(reader, "big", ReadAt::Head);
  let middle = versions
    .get(EDITS / 2)
    .map(|version| client.read(green, "big", ReadAt::Version { version: *version }));
  let head = client.read(green, "big", ReadAt::Head);
  drop(client);
  daemon.stop();
  assert!(
    matches!(created, Ok(Submitted::Accepted(1))),
    "the file is created: {created:?}"
  );
  assert_eq!(
    refused,
    None,
    "every edit accepted ({} of {EDITS})",
    versions.len()
  );
  assert!(
    held.unwrap() == original(),
    "the held work reads the file as created"
  );
  assert!(
    middle.unwrap().unwrap() == after(EDITS / 2 + 1),
    "the middle version reads its edits"
  );
  assert!(head.unwrap() == after(EDITS), "the head reads every edit");
}
