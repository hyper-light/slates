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

/// Submits versions `versions` from one work, the file overwritten whole each time: the work is reset at each
/// accepted version, so its base, the green's reachable floor, keeps up with the head. Returns the head and the first
/// refusal met, if any.
fn submit_versions(
  client: &mut Client,
  work: VolumeId,
  versions: std::ops::RangeInclusive<u64>,
) -> (u64, Option<String>) {
  let mut head = 0;
  for n in versions {
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
  let (head, refused) = submit_versions(&mut client, work, 1..=VERSIONS);
  let bytes = client.read(green, "file", ReadAt::Head);
  drop(client);
  daemon.stop();
  assert_eq!(
    refused, None,
    "every version accepted (head reached {head} of {VERSIONS})"
  );
  assert_eq!(head, VERSIONS, "the head is the last version");
  assert_eq!(
    bytes.unwrap(),
    bytes_of(VERSIONS),
    "the head reads the last version's bytes"
  );
}

/// W3, §4.8 recovery. Do: the same 200 versions under a played anchor, then stop the daemon and start a second over the
/// same anchor; read the green, read version 1, and submit one more version from a new work. Expect: the recovered
/// green's head is version 200 with its bytes, every version from the reachable floor on still reads as written,
/// version 1 (folded into the checkpoint) is refused `UnknownBase` rather than read as an absent file (it read
/// `NotFound`), and version 201 is accepted. Recovery rebuilds the green from what the partition holds, so a checkpoint must carry everything a later
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
  let (head, refused) = submit_versions(&mut client, work, 1..=VERSIONS);
  assert_eq!(
    refused, None,
    "every version accepted before the restart (head {head})"
  );
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
  let below = client.read(green, "file", ReadAt::Version { version: 1 });
  let (next, _) = client.create_work(green, "after-restart").unwrap();
  client
    .edit(next, "file", 0, EDIT_BYTES as u64, &bytes_of(VERSIONS + 1))
    .unwrap();
  let submitted = client.submit(next);
  drop(client);
  second.stop();
  assert_eq!(
    recovered_head, VERSIONS,
    "the recovered head is the last version"
  );
  assert_eq!(
    bytes,
    bytes_of(VERSIONS),
    "the recovered head reads its bytes"
  );
  assert_eq!(
    at_head,
    bytes_of(VERSIONS),
    "the head version reads by number"
  );
  assert!(
    matches!(
      below,
      Err(slates_client::ClientError::Refused(
        slates_client::Refusal::UnknownBase { version: 1, .. }
      ))
    ),
    "a version below the checkpoint is unknown, never answered as if the file were absent: {below:?}"
  );
  assert!(
    matches!(submitted, Ok(Submitted::Accepted(v)) if v == VERSIONS + 1),
    "the recovered green accepts the next version: {submitted:?}"
  );
}

/// Shape: the versions after which the partition's snapshot is first measured: past the green's first checkpoints.
const MEASURED_FROM: u64 = 40;
/// Format: what a checkpoint keeps of each accepted increment: its 32-byte identity and its 8-byte version.
const IDENTITY_KEPT_BYTES: usize = 40;
/// Shape: the edits' bytes the chain past a checkpoint may hold when either measurement is taken: under the doubling
/// rule, less than the checkpoint's own size (one file of [`EDIT_BYTES`] and the identities) each time, so at most two
/// files' worth between them, with the same again for the increments' framing.
const UNFOLDED_EDITS: usize = 4;

/// W3, §4.8 recovery. Do: under a played anchor, from a green at version 0, make two works; submit one, accepted as version
/// 1, so its base rises to 1, then the other, accepted as version 2 over base 0 (they touch different files), so its base
/// rises to 2 and the reachable floor to 1. Restart over the same anchor. Expect: the recovered green's head is version 2
/// and both files read their bytes. A chain checkpointed at the floor alone restored the green at version 1 and replayed
/// version 2, whose recorded base 0 is below the restored history: refused `BaseUnavailable` at replay, and the whole green
/// fenced, every acknowledged version unreadable.
#[test]
fn a_checkpoint_leaves_every_later_entry_a_base_recovery_can_replay() {
  let profile = common::machine_profile();
  let instance = format!("srv-chain-below-{}", std::process::id());
  let config = config(&profile, &instance);
  let anchor = common::anchor::held_anchor(
    &format!("chain-below-{}", std::process::id()),
    &profile,
    &config,
  );
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("chain-green", false).unwrap();
  let (first_work, _) = client.create_work(green, "first-work").unwrap();
  let (second_work, _) = client.create_work(green, "second-work").unwrap();
  client
    .edit(first_work, "first", 0, 0, &bytes_of(1))
    .unwrap();
  client
    .edit(second_work, "second", 0, 0, &bytes_of(2))
    .unwrap();
  assert_eq!(client.submit(first_work).unwrap(), Submitted::Accepted(1));
  assert_eq!(client.submit(second_work).unwrap(), Submitted::Accepted(2));
  drop(client);
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let recovered_head = client.versions(green);
  let first_bytes = client.read(green, "first", ReadAt::Head);
  let second_bytes = client.read(green, "second", ReadAt::Head);
  drop(client);
  second.stop();
  assert_eq!(
    recovered_head.ok(),
    Some(2),
    "the recovered head is version 2"
  );
  assert_eq!(first_bytes.ok(), Some(bytes_of(1)), "version 1's file");
  assert_eq!(second_bytes.ok(), Some(bytes_of(2)), "version 2's file");
}

/// W3, §4.8. Do: on a one-shard daemon with its derived caps, submit 40 versions as above and read the bytes the
/// partition's snapshot encodes to; submit 160 more and read it again. Expect: it grew by less than four edits' bytes
/// plus what a checkpoint keeps of each accepted increment (40 bytes): the chain past the checkpoint stays under the
/// checkpoint's own size, so a snapshot holds what the green is. A chain that keeps every increment grows it by each
/// version's 8 KiB, about 1.3 MB over the 160, and every snapshot taken when the log fills writes it whole.
#[test]
fn a_partitions_snapshot_holds_what_its_green_is_not_what_it_has_been() {
  let profile = common::machine_profile();
  let instance = format!("srv-chain-flat-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-chain-flat-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("chain-green", false).unwrap();
  let (work, _) = client.create_work(green, "chain-work").unwrap();
  let early = submit_versions(&mut client, work, 1..=MEASURED_FROM);
  let before = daemon.partition_snapshot_bytes(green).unwrap();
  let late = submit_versions(&mut client, work, MEASURED_FROM + 1..=VERSIONS);
  let after = daemon.partition_snapshot_bytes(green).unwrap();
  drop(client);
  daemon.stop();
  assert_eq!(early, (MEASURED_FROM, None), "the first versions accepted");
  assert_eq!(late, (VERSIONS, None), "the later versions accepted");
  let added = usize::try_from(VERSIONS - MEASURED_FROM).unwrap();
  let bound = UNFOLDED_EDITS * EDIT_BYTES + added * IDENTITY_KEPT_BYTES;
  eprintln!(
    "partition snapshot: {before} bytes at version {MEASURED_FROM}, {after} at {VERSIONS} (bound on growth {bound})"
  );
  assert!(
    after.saturating_sub(before) <= bound,
    "the snapshot grew {} bytes over {added} versions ({before} to {after}), past {bound}",
    after.saturating_sub(before)
  );
}

/// Shape: the cap of the held-chain test: small, so a chain of mode changes, each increment a few hundred bytes, meets
/// it within a few hundred versions.
const HELD_CAP_BYTES: usize = 32 << 10;
/// Shape: the most versions the held-chain test submits: several times what [`HELD_CAP_BYTES`] holds of them.
const HELD_VERSIONS: u64 = 4_000;
/// Format: the two modes the held-chain test alternates between.
const MODES: [u32; 2] = [0o600, 0o644];

/// Submits versions from `work`, each changing one file's mode, until one is refused or [`HELD_VERSIONS`] are
/// accepted: the head reached and the refused submit's answer, if any. A mode change adds an increment to the chain
/// but neither grows the green's state nor retains history in memory, so the chain's cap is the one met.
fn fill_until_refused(
  client: &mut Client,
  work: VolumeId,
) -> (u64, Option<Result<Submitted, slates_client::ClientError>>) {
  let mut head = 0;
  for (_, mode) in (1..=HELD_VERSIONS).zip(MODES.iter().cycle()) {
    let op = slates_client::WorkOp::SetMode {
      path: "file".to_owned(),
      mode: *mode,
    };
    client.declare(work, op).unwrap();
    match client.submit(work) {
      Ok(Submitted::Accepted(version)) => head = version,
      other => return (head, Some(other)),
    }
  }
  (head, None)
}

/// A green with one file, `file`, created at version 1 by the work returned with it.
fn green_with_one_file(client: &mut Client) -> (VolumeId, VolumeId) {
  let green = client.create_green("chain-green", false).unwrap();
  let (work, _) = client.create_work(green, "chain-work").unwrap();
  client.edit(work, "file", 0, 0, b"f").unwrap();
  assert!(
    matches!(client.submit(work), Ok(Submitted::Accepted(1))),
    "the file is created"
  );
  (green, work)
}

/// A one-shard daemon whose green-chain cap is [`HELD_CAP_BYTES`], bootstrapped.
fn held_cap_daemon(instance: &str) -> Daemon {
  let profile = common::machine_profile();
  let mut config = DaemonConfig::derive(&profile, instance, Some(TEST_SHARDS));
  config.caps.green_chain_bytes = HELD_CAP_BYTES;
  let daemon = Daemon::start(
    &profile,
    config,
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-chain-held-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  daemon
}

/// W3 (A-238), §4.8. Do: on a one-shard daemon whose green-chain cap is 32 KiB, make a green and a work, create one
/// file, then make a second work, left at version 1, which holds the reachable floor there; submit versions from the
/// first work, each changing the file's mode, until one is refused; then destroy the second work and submit the
/// refused version again. Expect: the refusal is `TableAtCapacity { table: "green_chains" }`, typed (it reached the
/// client as `BadRequest { reason: "green_chains at capacity" }`); and once the floor is released the same submit is
/// accepted as the next version: the submit checkpoints at the risen floor before it refuses, since no accept could
/// come to take the checkpoint otherwise. Mode changes keep the green's own state and its history in memory small, so
/// only the chain fills.
#[test]
fn a_chain_held_by_a_reader_is_refused_typed_at_its_cap_and_folds_once_released() {
  let instance = format!("srv-chain-held-{}", std::process::id());
  let daemon = held_cap_daemon(&instance);
  let mut client = connect(&instance);
  let (green, work) = green_with_one_file(&mut client);
  let (holder, _) = client.create_work(green, "floor-holder").unwrap();
  let (head, refused) = fill_until_refused(&mut client, work);
  let released = client.destroy(holder);
  let after = client.submit(work);
  drop(client);
  daemon.stop();
  assert!(
    matches!(
      &refused,
      Some(Err(slates_client::ClientError::Refused(slates_client::Refusal::TableAtCapacity { table })))
        if table == "green_chains"
    ),
    "the chain held at its floor is refused typed at its cap (head {head}): {refused:?}"
  );
  assert!(
    released.is_ok(),
    "the floor's work is destroyed: {released:?}"
  );
  assert!(
    matches!(after, Ok(Submitted::Accepted(version)) if version == head + 1),
    "once the floor is released the refused version is accepted next: {after:?}"
  );
}

/// W3, A-257, §4.8 recovery. Do: under a played anchor with the chain capped at 256 KiB, two works take turns writing their
/// own file, 8 KiB a version, 200 versions, each based on its own last version, so every entry's base trails it; restart.
/// Expect: all accepted (the cap holds about 32 such increments, so the chain really folded with bases behind its head), and
/// the recovered green holds every last write. A checkpoint held at or below every retained entry's base would never fold
/// here; recovery re-deciding each entry would fence the green.
#[test]
fn two_clients_taking_turns_keep_folding_and_recover_whole() {
  let profile = common::machine_profile();
  let instance = format!("srv-chain-turns-{}", std::process::id());
  let config = config(&profile, &instance);
  let anchor = common::anchor::held_anchor(
    &format!("chain-turns-{}", std::process::id()),
    &profile,
    &config,
  );
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("chain-green", false).unwrap();
  let works = [
    client.create_work(green, "turn-0").unwrap().0,
    client.create_work(green, "turn-1").unwrap().0,
  ];
  let names = ["turn-file-0", "turn-file-1"];
  let mut last: [Option<Vec<u8>>; 2] = [None, None];
  for n in 1..=VERSIONS {
    let who = usize::try_from(n % 2).unwrap();
    let delete = last[who].as_ref().map_or(0, |_| EDIT_BYTES as u64);
    client
      .edit(works[who], names[who], 0, delete, &bytes_of(n))
      .unwrap();
    assert_eq!(
      client.submit(works[who]).unwrap(),
      Submitted::Accepted(n),
      "version {n} accepted though its base trails the head"
    );
    last[who] = Some(bytes_of(n));
  }
  drop(client);
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let head = client.versions(green);
  let files: Vec<_> = names
    .iter()
    .map(|name| client.read(green, name, ReadAt::Head))
    .collect();
  drop(client);
  second.stop();
  assert_eq!(head.ok(), Some(VERSIONS), "the recovered head");
  for (index, read) in files.into_iter().enumerate() {
    assert_eq!(read.ok(), last[index], "{}'s last write", names[index]);
  }
}
