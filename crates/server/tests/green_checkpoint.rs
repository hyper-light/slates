//! A green's durable chain is checkpointed at the reachable floor (§4.16 "Deltas older than the derived retention fold
//! into checkpoint deltas", §4.8 recovery; GAPS "Memory per client, attributed"): the partition kept every accepted
//! increment in `green_chains` until "checkpointing folds old chain entries (owed)", so each version cost its increment
//! for good, every snapshot taken when the log filled re-encoded the whole chain, and a green whose readers all kept up
//! was still refused `Capacity { table: "green_chains" }` once its history passed the cap. At 3 rounds of 100k clients
//! that is about 300k increments: the 100k rung's wall.
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

/// Shape: one shard, so the green, its work and its chain are on one partition.
const TEST_SHARDS: u16 = 1;
/// Shape: the partition's green-chain cap in these tests: small, so an uncheckpointed chain meets it within a few dozen
/// versions of [`EDIT_BYTES`] each.
const CHAIN_CAP_BYTES: usize = 256 << 10;
/// Shape: the bytes each version overwrites in the one file: well above an increment's fixed framing, so the chain's
/// growth per version is set by them.
const EDIT_BYTES: usize = 8 << 10;
/// Shape: the versions the tests submit: several times what [`CHAIN_CAP_BYTES`] holds at [`EDIT_BYTES`] a version
/// (32), so a chain that is never checkpointed is refused long before the last.
const VERSIONS: u64 = 200;

fn config(profile: &slates_machine::MachineProfile, instance: &str) -> DaemonConfig {
  let mut config = DaemonConfig::derive(profile, instance, Some(TEST_SHARDS));
  config.caps.green_chain_bytes = CHAIN_CAP_BYTES;
  config
}

/// The bytes version `n` writes over the file: distinct per version, so each is a real change.
fn bytes_of(n: u64) -> Vec<u8> {
  let tag = format!("version-{n:06}-");
  tag.bytes().cycle().take(EDIT_BYTES).collect()
}

/// Submits `versions` versions from one work over `green`, the file overwritten whole each time: the work is reset at
/// each accepted version, so its base, the green's reachable floor, keeps up with the head. Returns the head and the
/// first refusal met, if any.
fn submit_versions(
  client: &mut Client,
  green: VolumeId,
  work: VolumeId,
  versions: u64,
) -> (u64, Option<String>) {
  let mut head = 0;
  for n in 1..=versions {
    let delete = if n == 1 { 0 } else { EDIT_BYTES as u64 };
    if let Err(e) = client.edit(work, "file", 0, delete, &bytes_of(n)) {
      return (head, Some(format!("edit {n}: {e}")));
    }
    match client.submit(work) {
      Ok(Submitted::Accepted(version)) => head = version,
      Ok(Submitted::Conflict(windows)) => {
        return (head, Some(format!("submit {n}: conflict {windows:?}")));
      }
      Err(e) => return (head, Some(format!("submit {n}: {e}"))),
    }
  }
  let _ = green;
  (head, None)
}

/// W3, §4.16, §4.2. Do: on a one-shard daemon whose green-chain cap is 256 KiB, make a green and one work, and submit
/// 200 versions from it, each overwriting the file's 8 KiB, the work reset at every accepted version so no reader is
/// left behind. Expect: all 200 accepted, and the green's head reads the last version's bytes. A chain that keeps every
/// increment holds about 32 of them in the cap and refuses the rest `Capacity { table: "green_chains" }`, though nothing
/// can ever read below the floor; a chain checkpointed at the floor holds what the green holds, not what it has been.
#[test]
fn a_green_whose_readers_keep_up_holds_versions_past_its_chain_cap() {
  let profile = common::machine_profile();
  let instance = format!("srv-chain-cap-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    config(&profile, &instance),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-chain-cap-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("chain-green", false).unwrap();
  let (work, _) = client.create_work(green, "chain-work").unwrap();
  let (head, refused) = submit_versions(&mut client, green, work, VERSIONS);
  let bytes = client.read(green, "file", ReadAt::Head);
  drop(client);
  daemon.stop();
  assert_eq!(
    refused, None,
    "every version accepted (head reached {head} of {VERSIONS})"
  );
  assert_eq!(head, VERSIONS, "the head is the last version");
  assert_eq!(bytes.unwrap(), bytes_of(VERSIONS), "the head reads the last version's bytes");
}

/// W3, §4.8 recovery. Do: the same 200 versions under a played anchor, then stop the daemon and start a second over the
/// same anchor; read the green, and submit one more version from a new work. Expect: the recovered green's head is
/// version 200 with its bytes, every version from the reachable floor on still reads as written, and version 201 is
/// accepted. Recovery rebuilds the green from what the partition holds, so a checkpoint must carry everything a later
/// verdict needs, and an increment folded into it is never needed again.
#[test]
fn a_green_recovered_after_its_chain_was_checkpointed_is_the_green_it_was() {
  let profile = common::machine_profile();
  let instance = format!("srv-chain-restart-{}", std::process::id());
  let config = config(&profile, &instance);
  let anchor = common::anchor::held_anchor(
    &format!("chain-restart-{}", std::process::id()),
    &profile,
    &config,
  );
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("chain-green", false).unwrap();
  let (work, _) = client.create_work(green, "chain-work").unwrap();
  let (head, refused) = submit_versions(&mut client, green, work, VERSIONS);
  assert_eq!(refused, None, "every version accepted before the restart (head {head})");
  drop(client);
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let recovered_head = client.versions(green).unwrap();
  let bytes = client.read(green, "file", ReadAt::Head).unwrap();
  let at_head = client
    .read(green, "file", ReadAt::Version { version: VERSIONS })
    .unwrap();
  let (next, _) = client.create_work(green, "after-restart").unwrap();
  client
    .edit(next, "file", 0, EDIT_BYTES as u64, &bytes_of(VERSIONS + 1))
    .unwrap();
  let submitted = client.submit(next);
  drop(client);
  second.stop();
  assert_eq!(recovered_head, VERSIONS, "the recovered head is the last version");
  assert_eq!(bytes, bytes_of(VERSIONS), "the recovered head reads its bytes");
  assert_eq!(at_head, bytes_of(VERSIONS), "the head version reads by number");
  assert!(
    matches!(submitted, Ok(Submitted::Accepted(v)) if v == VERSIONS + 1),
    "the recovered green accepts the next version: {submitted:?}"
  );
}
