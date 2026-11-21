//! A green's works are served where they live; only the verdict is the green's (§4.16, A-247). One history is run on a
//! daemon with one shard, where a work and its green share a partition, and on a daemon with many, where a work lives on
//! the shard of the client that made it (A-400) and its green on another, its maker's ([`green_off`]). Every reply and
//! every file must be the same, and the counter of verbs a work's shard asked the green's owner for proves the many-shard
//! run really went there.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use slates_client::{
  Client, ClientError, PolicySubject, Rebased, Refusal, RequestId, Submitted, WriteAct, WriteMode,
  WritePolicy,
};
use slates_ipc::protocol::{ReadAt, ReplyBody, RequestBody, VolumeId, WorkOp};
use slates_server::daemon::OBSERVE_BUDGET_NS;
use slates_server::landing::{enroll_proof, merge_approval_proof, policy_proof};
use slates_server::state::ShardState;
use slates_server::verbs::{owner_of, owner_of_name};
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: the daemon where a work and its green are always on one partition.
const ONE_SHARD: u16 = 1;
/// Shape: enough shards that a green's works spread over several whatever the names hash to.
const MANY_SHARDS: u16 = 8;
/// Shape: the works of the history, more than the shards, so the spread is not an accident of a few names.
const WORKS: usize = 24;
/// Shape: the base files, long enough that each work has its own slot in them.
const FILE_BYTES: usize = 1024;
/// Shape: the slot one work overwrites in a shared file.
const SLOT_BYTES: usize = 8;
/// Format: the counters of a work's increment composed on its own shard, and of one left to the green's owner.
const COMPOSED_ON_WORK: &str = "merge.work.composed_on_work";
/// Format: see [`COMPOSED_ON_WORK`].
const COMPOSED_BY_OWNER: &str = "merge.work.composed_by_owner";
/// Format: the counter a work's shard moves for each verb it asked the green's owner to serve.
const ASKED_THE_OWNER: &str = "merge.work.asked_owner";

fn daemon(shards: u16, tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-works-off-{tag}-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(shards)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-works-off-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn submitted(client: &mut Client, work: VolumeId) -> String {
  match client.submit(work) {
    Ok(Submitted::Accepted(version) | Submitted::Identical(version)) => {
      format!("accepted {version}")
    }
    Ok(Submitted::Conflict(windows)) => format!("conflict {windows:?}"),
    Ok(Submitted::ApprovalRequired { identity, paths }) => {
      format!("approval of {identity:?} over {paths} paths required")
    }
    Err(error) => format!("refused {error:?}"),
  }
}

fn rebased(client: &mut Client, work: VolumeId) -> String {
  match client.rebase(work) {
    Ok(Rebased::Rebased(version)) => format!("rebased {version}"),
    Ok(Rebased::Conflict(windows)) => format!("conflict {windows:?}"),
    Err(error) => format!("refused {error:?}"),
  }
}

fn file(client: &mut Client, volume: VolumeId, path: &str) -> String {
  match client.read(volume, path, ReadAt::Head) {
    Ok(bytes) => format!("{path}: {} {}", bytes.len(), blake3::hash(&bytes).to_hex()),
    Err(error) => format!("{path}: refused {error:?}"),
  }
}

/// The history: seeded base files; works that edit their own slots, a shared slot, a length change followed by an edit
/// past it, a file of their own and a file they only read; their submits in order; a conflict, its rebase and its
/// resolution; every reply and file written down.
fn history(client: &mut Client, (instance, shards): (&str, u16)) -> Vec<String> {
  let mut said = Vec::new();
  let green = green_off(client, instance, shards, "spread-green");
  let (seed, base) = client.create_work(green, "spread-seed").unwrap();
  said.push(format!("seed based at {base}"));
  for name in ["shared", "shifted", "untouched"] {
    client
      .edit(seed, name, 0, 0, &[name.as_bytes()[0]; FILE_BYTES])
      .unwrap();
  }
  said.push(submitted(client, seed));
  let mut works = Vec::with_capacity(WORKS);
  for index in 0..WORKS {
    let (work, base) = client
      .create_work(green, &format!("spread-work-{index}"))
      .unwrap();
    said.push(format!("work {index} based at {base}"));
    works.push(work);
  }
  for (index, work) in works.iter().enumerate() {
    let slot = u64::try_from(index * SLOT_BYTES).unwrap();
    let delete = u64::try_from(SLOT_BYTES).unwrap();
    let mark = [u8::try_from(index).unwrap(); SLOT_BYTES];
    client.edit(*work, "shared", slot, delete, &mark).unwrap();
    client
      .edit(*work, &format!("own-{index}"), 0, 0, &mark)
      .unwrap();
    said.push(file(client, *work, "untouched"));
    said.push(file(client, *work, "shared"));
    if index % 2 == 0 {
      said.push(submitted(client, *work));
      said.push(file(client, *work, "shared"));
    }
  }
  for (index, work) in works.iter().enumerate().filter(|(i, _)| i % 2 == 1) {
    let mark = [u8::try_from(index).unwrap(); 2];
    // A delete at the front of the file, then a write past where the file now ends in base coordinates.
    client.edit(*work, "shifted", 0, 2, b"").unwrap();
    let late = u64::try_from(FILE_BYTES - 4).unwrap();
    client.edit(*work, "shifted", late, 2, &mark).unwrap();
    said.push(file(client, *work, "shifted"));
    said.push(submitted(client, *work));
    said.push(rebased(client, *work));
    said.push(submitted(client, *work));
  }
  let (left, _) = client.create_work(green, "spread-left").unwrap();
  let (right, _) = client.create_work(green, "spread-right").unwrap();
  client.edit(left, "untouched", 8, 8, b"LLLLLLLL").unwrap();
  client.edit(right, "untouched", 8, 8, b"RRRRRRRR").unwrap();
  said.push(submitted(client, left));
  said.push(submitted(client, right));
  said.push(rebased(client, right));
  // The resolution: take the green's bytes over the window, so the second pass finds them identical.
  client.edit(right, "untouched", 8, 8, b"LLLLLLLL").unwrap();
  said.push(rebased(client, right));
  said.push(submitted(client, right));
  for name in ["shared", "shifted", "untouched"] {
    said.push(file(client, green, name));
  }
  for (index, work) in works.iter().enumerate() {
    said.push(file(client, green, &format!("own-{index}")));
    said.push(file(client, *work, &format!("own-{index}")));
  }
  client.destroy(left).unwrap();
  said.push(file(client, right, "untouched"));
  said
}

/// §4.16, A-247. Do: run one history of 24 works over a green — own slots, a conflict and its rebase, a length change
/// and an edit past it, reads of files they never changed, submits interleaved — on one shard and on eight. Expect: the
/// same replies and the same bytes of every file, and on eight shards the work's shards asked the green's owner: its
/// works are not on its partition.
#[test]
fn a_greens_works_spread_over_shards_answer_as_they_do_on_one() {
  let run = |shards: u16, tag: &str| {
    let (daemon, instance) = daemon(shards, tag);
    let mut client = connect(&instance);
    let said = history(&mut client, (&instance, shards));
    let counts = daemon.refusals_on_every_shard().unwrap();
    let asked = counts.get(ASKED_THE_OWNER).copied().unwrap_or(0);
    let composed = (
      counts.get(COMPOSED_ON_WORK).copied().unwrap_or(0),
      counts.get(COMPOSED_BY_OWNER).copied().unwrap_or(0),
    );
    drop(client);
    daemon.stop();
    (said, asked, composed)
  };
  let (on_one, asked_on_one, _) = run(ONE_SHARD, "one");
  let (on_many, asked_on_many, (on_work, by_owner)) = run(MANY_SHARDS, "many");
  for (at, (one, many)) in on_one.iter().zip(&on_many).enumerate() {
    assert_eq!(one, many, "step {at} differs between one shard and many");
  }
  assert_eq!(on_one.len(), on_many.len());
  assert_eq!(asked_on_one, 0, "one shard asks nobody");
  assert!(
    asked_on_many > 0,
    "the many-shard run is not vacuous: its works' shards asked the green's owner"
  );
  assert!(
    on_work > 0 && by_owner == 0,
    "every remote increment was composed on its work's shard: {on_work} there, {by_owner} left to the owner"
  );
}

/// Shape: how long a test waits for what the daemon does in its own time (a floor's message, a restart).
const WAIT: Duration = Duration::from_secs(60);
/// Shape: the chain cap of the floor test: small, so a held floor fills it within a few dozen versions.
const CHAIN_CAP_BYTES: usize = 256 << 10;
/// Shape: what each version overwrites in the floor test's file: well above an increment's framing.
const VERSION_BYTES: usize = 8 << 10;
/// Shape: the versions the floor test goes on to once released: several times what the cap holds.
const VERSIONS: u64 = 120;

/// A name of `prefix` that hashes to a partition other than `green`'s: a work of that name made on another shard holds
/// it there by an alias (A-400).
fn off_owner_name(green: VolumeId, prefix: &str) -> String {
  (0u32..)
    .map(|suffix| format!("{prefix}-{suffix}"))
    .find(|name| owner_of_name(name, usize::from(MANY_SHARDS)) != owner_of(green))
    .unwrap()
}

/// A green made by a client of another shard than `client`'s, on a daemon of `shards` shards (A-400: a work lives on
/// the shard of the client that makes it, and a green on its maker's), so every work `client` makes is off its green's
/// shard. On one shard there is no other, and the green is `client`'s own.
fn green_off(client: &mut Client, instance: &str, shards: u16, name: &str) -> VolumeId {
  if shards < 2 {
    return client.create_green(name, false).unwrap();
  }
  let here = client.client_id() % u32::from(shards);
  loop {
    let mut maker = connect(instance);
    if maker.client_id() % u32::from(shards) != here {
      return maker.create_green(name, false).unwrap();
    }
  }
}

/// The partition `client` is served on on a daemon of [`MANY_SHARDS`] shards (its id's residue), where every work it
/// makes lives (A-400).
fn client_partition(client: &Client) -> u16 {
  u16::try_from(client.client_id() % u32::from(MANY_SHARDS)).unwrap()
}

/// The bytes version `n` writes over the file: distinct per version.
fn version_bytes(n: u64) -> Vec<u8> {
  format!("version-{n:06}-")
    .bytes()
    .cycle()
    .take(VERSION_BYTES)
    .collect()
}

fn asked_of_the_owner(daemon: &Daemon) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(ASKED_THE_OWNER)
    .copied()
    .unwrap_or(0)
}

/// Submits `work` until it answers, for a chain whose floor is on its way down from another shard.
fn submit_when_room(client: &mut Client, work: VolumeId) -> Submitted {
  let started = Instant::now();
  loop {
    match client.submit(work) {
      Err(ClientError::Refused(Refusal::TableAtCapacity { table })) if table == "green_chains" => {
        assert!(started.elapsed() < WAIT, "the chain's floor is released");
        std::thread::yield_now();
      }
      other => return other.unwrap(),
    }
  }
}

/// Advances the green from `writer` until its chain refuses, a held floor having kept it from folding; the version the
/// refused submit would have been, its edit still in the work.
fn fill_the_chain(client: &mut Client, writer: VolumeId, delete: u64) -> u64 {
  for next in 2..=VERSIONS {
    client
      .edit(writer, "file", 0, delete, &version_bytes(next))
      .unwrap();
    match client.submit(writer) {
      Ok(Submitted::Accepted(version)) => assert_eq!(version, next),
      Err(ClientError::Refused(Refusal::TableAtCapacity { table })) if table == "green_chains" => {
        return next;
      }
      other => panic!("submit {next}: {other:?}"),
    }
  }
  panic!("the held floor fills the chain within {VERSIONS} versions");
}

/// A-247, W3. Do: over a green with a small chain cap, on eight shards, leave two works off the green's shard based on
/// version 1, and advance the green from a third until its chain is full. Expect: the chain is refused typed rather
/// than folded past the works; the works still read their base and are judged at it; and once one is rebased and both
/// are destroyed the chain folds and the green goes on, with no version refused.
#[test]
fn a_checkpoint_never_folds_past_a_work_on_another_shard() {
  let profile = common::machine_profile();
  let instance = format!("srv-works-floor-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(MANY_SHARDS));
  config.caps.green_chain_bytes = CHAIN_CAP_BYTES;
  let daemon = Daemon::start(
    &profile,
    config,
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-works-floor-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "floor-green");
  let (seed, _) = client.create_work(green, "floor-seed").unwrap();
  let first = version_bytes(1);
  client.edit(seed, "file", 0, 0, &first).unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  // The seed would hold version 1 for good; only the two works under test may.
  client.destroy(seed).unwrap();
  let (rebased_later, _) = client
    .create_work(green, &off_owner_name(green, "floor-a"))
    .unwrap();
  let (destroyed, _) = client
    .create_work(green, &off_owner_name(green, "floor-b"))
    .unwrap();
  client.edit(rebased_later, "own", 0, 0, b"mine").unwrap();
  let (writer, _) = client
    .create_work(green, &off_owner_name(green, "floor-w"))
    .unwrap();
  let delete = u64::try_from(VERSION_BYTES).unwrap();
  let full_at = fill_the_chain(&mut client, writer, delete);
  assert_eq!(
    client.read(destroyed, "file", ReadAt::Head).unwrap(),
    first,
    "a work reads its base across the chain the others filled"
  );
  assert_eq!(
    client.rebase(rebased_later).unwrap(),
    Rebased::Rebased(full_at - 1),
    "the base is still there to be judged from"
  );
  client.destroy(rebased_later).unwrap();
  client.destroy(destroyed).unwrap();
  assert_eq!(
    submit_when_room(&mut client, writer),
    Submitted::Accepted(full_at)
  );
  for n in full_at + 1..=VERSIONS {
    client
      .edit(writer, "file", 0, delete, &version_bytes(n))
      .unwrap();
    assert_eq!(
      submit_when_room(&mut client, writer),
      Submitted::Accepted(n)
    );
  }
  assert_eq!(
    client.read(green, "file", ReadAt::Head).unwrap(),
    version_bytes(VERSIONS)
  );
  drop(client);
  daemon.stop();
}

/// A-247, A-207, D-18. Do: on eight shards, edit works off the green's shard (first touches of a base file and a file
/// of their own), advance the green past their base, and restart the daemon over the same anchor. Expect: each work
/// reads what it acknowledged; each is still based where it was, so its submit is judged and accepted in order; and the
/// second daemon asked the green's owner, so it was the off-owner path that served it.
#[test]
fn a_work_off_its_greens_shard_keeps_its_edits_and_its_base_across_a_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-works-restart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(MANY_SHARDS));
  let anchor = common::anchor::held_anchor("works-restart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "restart-green");
  let (seed, _) = client.create_work(green, "restart-seed").unwrap();
  client
    .edit(seed, "shared", 0, 0, &[b'.'; FILE_BYTES])
    .unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let mut works = Vec::new();
  for index in 0..WORKS {
    let (work, _) = client
      .create_work(green, &off_owner_name(green, &format!("restart-{index}")))
      .unwrap();
    let slot = u64::try_from(index * SLOT_BYTES).unwrap();
    let mark = [u8::try_from(index).unwrap() + 1; SLOT_BYTES];
    client
      .edit(
        work,
        "shared",
        slot,
        u64::try_from(SLOT_BYTES).unwrap(),
        &mark,
      )
      .unwrap();
    client
      .edit(work, &format!("mine-{index}"), 0, 0, &mark)
      .unwrap();
    works.push(work);
  }
  // The green moves on past every work's base, from a file none of them touch.
  client.edit(seed, "other", 0, 0, b"moved on").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(2));
  let before: Vec<_> = works
    .iter()
    .map(|work| client.read(*work, "shared", ReadAt::Head).unwrap())
    .collect();
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  let asked_before = asked_of_the_owner(&second);
  for (index, work) in works.iter().enumerate() {
    assert_eq!(
      client.read(*work, "shared", ReadAt::Head).unwrap(),
      before[index],
      "work {index} reads what it acknowledged"
    );
    let submitted = client.submit(*work).unwrap();
    let version = u64::try_from(index).unwrap() + 3;
    assert_eq!(submitted, Submitted::Accepted(version), "work {index}");
  }
  assert!(
    asked_of_the_owner(&second) > asked_before,
    "the works were served off their green's shard"
  );
  let merged = client.read(green, "shared", ReadAt::Head).unwrap();
  for index in 0..WORKS {
    let mark = [u8::try_from(index).unwrap() + 1; SLOT_BYTES];
    assert_eq!(&merged[index * SLOT_BYTES..(index + 1) * SLOT_BYTES], mark);
  }
  drop(client);
  second.stop();
  drop(anchor);
}

/// Waits for request `id`'s reply.
fn reply_of(client: &mut Client, id: RequestId) -> ReplyBody {
  let started = Instant::now();
  loop {
    if let Some(reply) = client.poll_reply(id).unwrap() {
      return reply;
    }
    assert!(started.elapsed() < WAIT, "request {id:?} is answered");
    std::thread::yield_now();
  }
}

/// One round of the pipelined-order test: a new work, four verbs sent without waiting, and what they left.
fn pipelined_round(client: &mut Client, green: VolumeId, round: u8) {
  let name = off_owner_name(green, &format!("order-{round}"));
  let (work, base) = client.create_work(green, &name).unwrap();
  let edit = |path: &str, fill: u8| RequestBody::Edit {
    work,
    path: path.to_owned(),
    at: 0,
    delete_len: 4,
    bytes: vec![fill; 4],
  };
  let first = client.begin(&edit("first", b'A' + round)).unwrap();
  let submit = client
    .begin(&RequestBody::submit(work, Vec::new()))
    .unwrap();
  let second = client.begin(&edit("second", b'c' + round)).unwrap();
  // This one needs nothing of the owner (its file is the work's own by now), so only the work's order holds it back
  // from running during the verdict and being lost to the accept's reset.
  let held_back = client.begin(&edit("first", b'P' + round)).unwrap();
  assert_eq!(reply_of(client, first), ReplyBody::Edited);
  let accepted = u64::from(round) + 2;
  assert_eq!(
    reply_of(client, submit),
    ReplyBody::Submitted {
      version: Some(accepted),
      conflicts: Vec::new(),
      conflicts_elided: 0,
      verdict: 0,
      identical: false,
    },
    "based at {base}"
  );
  assert_eq!(reply_of(client, second), ReplyBody::Edited);
  assert_eq!(reply_of(client, held_back), ReplyBody::Edited);
  assert_eq!(
    client
      .read(green, "first", ReadAt::Version { version: accepted })
      .unwrap(),
    vec![b'A' + round; 4],
    "the submit holds the edit before it"
  );
  assert_ne!(
    client
      .read(green, "second", ReadAt::Version { version: accepted })
      .unwrap(),
    vec![b'c' + round; 4],
    "and not the edit after it"
  );
  assert_eq!(
    client.read(work, "second", ReadAt::Head).unwrap(),
    vec![b'c' + round; 4],
    "which the work still holds"
  );
  assert_eq!(
    client.read(work, "first", ReadAt::Head).unwrap(),
    vec![b'P' + round; 4],
    "an edit behind the submit lands on what the submit left"
  );
  client.destroy(work).unwrap();
}

/// A-247 "A work's order". Do: pipeline on a work off its green's shard an edit of a base file (which must ask the
/// owner), a submit, and an edit of another base file, without waiting between them. Expect: the submit holds the
/// first edit and not the second: the green has the first, and the work still holds the second for its next submit.
#[test]
fn verbs_pipelined_on_a_work_off_its_greens_shard_keep_their_order() {
  let (daemon, instance) = daemon(MANY_SHARDS, "order");
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "order-green");
  let (seed, _) = client.create_work(green, "order-seed").unwrap();
  client.edit(seed, "first", 0, 0, b"aaaa").unwrap();
  client.edit(seed, "second", 0, 0, b"bbbb").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  for round in 0..16u8 {
    pipelined_round(&mut client, green, round);
  }
  assert!(asked_of_the_owner(&daemon) > 0);
  drop(client);
  daemon.stop();
}

/// Waits for request `id` to be refused `Overloaded`, the answer a lost owner's reply leaves.
fn await_overloaded(client: &mut Client, id: RequestId) {
  let started = Instant::now();
  loop {
    match client.poll_reply(id) {
      Ok(None) => {
        assert!(started.elapsed() < WAIT, "the lost answer is refused");
        std::thread::yield_now();
      }
      Err(ClientError::Refused(Refusal::Overloaded { .. })) => return,
      other => panic!("the lost answer is a refusal the client retries: {other:?}"),
    }
  }
}

/// A green at version 1 and a work off its shard with one edit waiting.
fn green_and_edited_work(client: &mut Client, instance: &str, tag: &str) -> (VolumeId, VolumeId) {
  let green = green_off(client, instance, MANY_SHARDS, &format!("{tag}-green"));
  let (seed, _) = client.create_work(green, &format!("{tag}-seed")).unwrap();
  client.edit(seed, "f", 0, 0, b"....").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let (work, _) = client
    .create_work(green, &off_owner_name(green, tag))
    .unwrap();
  client.edit(work, "f", 0, 1, b"X").unwrap();
  (green, work)
}

/// Submits `work` with the owner's answer lost: the green moved, the verb is refused `Overloaded`, and the id and the
/// request are returned to retry.
fn submit_with_the_answer_lost(
  daemon: &Daemon,
  client: &mut Client,
  green: VolumeId,
  work: VolumeId,
) -> (RequestId, RequestBody) {
  client.acknowledge_all().unwrap();
  daemon.lose_next_owner_answer(owner_of(work)).unwrap();
  let request = RequestBody::submit(work, Vec::new());
  let id = client.begin(&request).unwrap();
  await_overloaded(client, id);
  assert_eq!(client.versions(green).unwrap(), 2, "the owner moved");
  (id, request)
}

const ACCEPTED_AT_TWO: ReplyBody = ReplyBody::Submitted {
  version: Some(2),
  conflicts: Vec::new(),
  conflicts_elided: 0,
  verdict: 0,
  identical: false,
};

/// A-247 "Exactly once". Do: submit a work off its green's shard with the owner's answer lost, so the green advanced
/// and the work did not, and the verb is answered `Overloaded`; retry it under its id. Expect: the retry answers the one
/// version the owner recorded, the green is one version ahead and not two, and the work settled at it.
#[test]
fn a_submit_whose_answer_was_lost_is_answered_by_the_owners_record_once() {
  let (daemon, instance) = daemon(MANY_SHARDS, "lost");
  let mut client = connect(&instance);
  let (green, work) = green_and_edited_work(&mut client, &instance, "lost");
  let (id, request) = submit_with_the_answer_lost(&daemon, &mut client, green, work);
  assert_eq!(client.retry(id, &request).unwrap(), ACCEPTED_AT_TWO);
  assert_eq!(client.versions(green).unwrap(), 2, "one version");
  assert_eq!(
    client.read(work, "f", ReadAt::Head).unwrap(),
    client.read(green, "f", ReadAt::Head).unwrap(),
    "the work settled at the version the owner recorded"
  );
  assert_eq!(client.read(green, "f", ReadAt::Head).unwrap(), b"X...");
  drop(client);
  daemon.stop();
}

/// A-247 "Exactly once". Do: a work edits two regions of a file; the green changes the first, so its submit conflicts
/// there, with the owner's answer lost; the green then changes the second; retry under the same id. Expect: the first
/// verdict, one window, from the owner's record. Judged again it would name two; the engine's identity cache keeps
/// only accepts' verdicts, so only the record can answer this.
#[test]
fn a_conflict_whose_answer_was_lost_is_answered_as_it_was_first_judged() {
  let (daemon, instance) = daemon(MANY_SHARDS, "lost-conflict");
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "lost-conflict-green");
  let (other, _) = client.create_work(green, "lost-conflict-other").unwrap();
  client.edit(other, "f", 0, 0, &[b'.'; 64]).unwrap();
  assert_eq!(client.submit(other).unwrap(), Submitted::Accepted(1));
  let (work, _) = client
    .create_work(green, &off_owner_name(green, "lost-conflict"))
    .unwrap();
  client.edit(work, "f", 0, 4, b"WWWW").unwrap();
  client.edit(work, "f", 32, 4, b"WWWW").unwrap();
  client.edit(other, "f", 0, 4, b"AAAA").unwrap();
  assert_eq!(client.submit(other).unwrap(), Submitted::Accepted(2));
  client.acknowledge_all().unwrap();
  daemon.lose_next_owner_answer(owner_of(work)).unwrap();
  let request = RequestBody::submit(work, Vec::new());
  let id = client.begin(&request).unwrap();
  await_overloaded(&mut client, id);
  client.edit(other, "f", 32, 4, b"BBBB").unwrap();
  assert_eq!(client.submit(other).unwrap(), Submitted::Accepted(3));
  let ReplyBody::Submitted {
    version, conflicts, ..
  } = client.retry(id, &request).unwrap()
  else {
    panic!("the retry answers a submit");
  };
  assert_eq!(version, None);
  assert_eq!(
    conflicts.len(),
    1,
    "the verdict the owner recorded: {conflicts:?}"
  );
  drop(client);
  daemon.stop();
}

/// A-247 "Exactly once". Do: submit a work off its green's shard with the owner's answer lost, so the green took the
/// increment and the work does not know; then, before retrying, edit another file of the work, which is acknowledged;
/// then retry the submit under its id. Expect: the retry answers the version the owner recorded, and the acknowledged
/// edit is still in the work and goes into the next version. Settling the lost accept at the retry reset the work and
/// dropped the edit made after it.
#[test]
fn an_edit_after_a_lost_answer_is_kept_when_the_submit_is_retried() {
  let (daemon, instance) = daemon(MANY_SHARDS, "lost-then-edit");
  let mut client = connect(&instance);
  let (green, work) = green_and_edited_work(&mut client, &instance, "lost-then-edit");
  let (id, request) = submit_with_the_answer_lost(&daemon, &mut client, green, work);
  client.edit(work, "later", 0, 0, b"kept").unwrap();
  assert_eq!(client.retry(id, &request).unwrap(), ACCEPTED_AT_TWO);
  assert_eq!(
    client.read(work, "later", ReadAt::Head).unwrap(),
    b"kept",
    "the acknowledged edit is still in the work"
  );
  assert_eq!(client.submit(work).unwrap(), Submitted::Accepted(3));
  assert_eq!(client.read(green, "later", ReadAt::Head).unwrap(), b"kept");
  assert_eq!(client.read(green, "f", ReadAt::Head).unwrap(), b"X...");
  drop(client);
  daemon.stop();
}

/// A-247 "Exactly once", across a restart. Do: as above, but restart the daemon before the retry, so the work comes
/// back from its log still holding the edit the green already has. Expect: the same one version, the edit once, and a
/// next edit and submit accepted on top of it.
#[test]
fn a_submit_whose_answer_was_lost_is_answered_once_across_a_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-works-lost-restart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(MANY_SHARDS));
  let anchor = common::anchor::held_anchor("works-lost-restart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let (green, work) = green_and_edited_work(&mut client, &instance, "lost-restart");
  let (id, request) = submit_with_the_answer_lost(&first, &mut client, green, work);
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  assert_eq!(client.retry(id, &request).unwrap(), ACCEPTED_AT_TWO);
  assert_eq!(client.versions(green).unwrap(), 2, "one version");
  assert_eq!(client.read(green, "f", ReadAt::Head).unwrap(), b"X...");
  client.edit(work, "f", 3, 1, b"Y").unwrap();
  assert_eq!(client.submit(work).unwrap(), Submitted::Accepted(3));
  assert_eq!(client.read(green, "f", ReadAt::Head).unwrap(), b"X..Y");
  drop(client);
  second.stop();
  drop(anchor);
}

/// A-247 "Floors". Do: over a green with a small chain cap, lose the owner's answer to a work's creation, as a crash
/// between the two steps would, leaving a floor at version 1 with no work; fill the chain; restart. Expect: the orphan
/// floor is held against the chain before the restart (the chain refuses) and released by the start's reconcile (the
/// next submit folds it and is accepted).
#[test]
fn a_floor_whose_work_was_never_made_is_released_at_the_next_start() {
  let profile = common::machine_profile();
  let instance = format!("srv-works-orphan-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(MANY_SHARDS));
  config.caps.green_chain_bytes = CHAIN_CAP_BYTES;
  let anchor = common::anchor::held_anchor("works-orphan", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "orphan-green");
  let (seed, _) = client.create_work(green, "orphan-seed").unwrap();
  client.edit(seed, "file", 0, 0, &version_bytes(1)).unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  client.destroy(seed).unwrap();
  let orphan = off_owner_name(green, "orphan");
  first
    .lose_next_owner_answer(client_partition(&client))
    .unwrap();
  let orphaned = client.create_work(green, &orphan);
  assert!(
    matches!(
      orphaned,
      Err(ClientError::Refused(Refusal::Overloaded { .. }))
    ),
    "the lost admission is a refusal: {orphaned:?}"
  );
  let (writer, _) = client
    .create_work(green, &off_owner_name(green, "orphan-w"))
    .unwrap();
  let full_at = fill_the_chain(&mut client, writer, u64::try_from(VERSION_BYTES).unwrap());
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  assert_eq!(
    submit_when_room(&mut client, writer),
    Submitted::Accepted(full_at),
    "the start released the orphan's floor"
  );
  drop(client);
  second.stop();
  drop(anchor);
}

/// Shape: how long the green's owner is held while a submit waits on it: long enough for the verbs behind it to reach the
/// work's shard on a loaded host.
const OWNER_HOLD: Duration = Duration::from_millis(400);

/// A-247 "A work's order". Do: hold the green's owner shard; submit a work off that shard, so the verdict waits; send
/// an edit of a file the work already holds, which needs nothing of the owner; look at it near the hold's end; then
/// let the owner go. Expect: the edit unanswered while the verdict is out, then the submit accepted with what the work
/// held before it, and the edit standing on what the submit left. Without the order it ran during the verdict, was
/// answered, and the accept's reset wiped it.
#[test]
fn an_edit_behind_a_submit_waiting_on_its_owner_lands_after_the_verdict() {
  static HELD: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon(MANY_SHARDS, "behind");
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "behind-green");
  let (seed, _) = client.create_work(green, "behind-seed").unwrap();
  client.edit(seed, "f", 0, 0, b"....").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let (work, _) = client
    .create_work(green, &off_owner_name(green, "behind"))
    .unwrap();
  client.edit(work, "f", 0, 4, b"AAAA").unwrap();
  let owner = daemon.shards()[usize::from(owner_of(green))];
  let holding = daemon
    .observation(Some(owner), OBSERVE_BUDGET_NS, |_: &mut ShardState| {
      HELD.store(true, Ordering::Release);
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(OWNER_HOLD);
    })
    .unwrap();
  let waiter = std::thread::spawn(move || holding.wait().unwrap());
  while !HELD.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  let submit = client
    .begin(&RequestBody::submit(work, Vec::new()))
    .unwrap();
  let edit = client
    .begin(&RequestBody::Edit {
      work,
      path: "f".to_owned(),
      at: 0,
      delete_len: 4,
      bytes: b"BBBB".to_vec(),
    })
    .unwrap();
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(OWNER_HOLD / 2);
  let early = client.poll_reply(edit);
  waiter.join().unwrap();
  assert_eq!(
    reply_of(&mut client, submit),
    ReplyBody::Submitted {
      version: Some(2),
      conflicts: Vec::new(),
      conflicts_elided: 0,
      verdict: 0,
      identical: false,
    }
  );
  assert!(
    matches!(early, Ok(None)),
    "the edit waits behind the verdict: {early:?}"
  );
  assert_eq!(reply_of(&mut client, edit), ReplyBody::Edited);
  assert_eq!(client.read(green, "f", ReadAt::Head).unwrap(), b"AAAA");
  assert_eq!(client.read(work, "f", ReadAt::Head).unwrap(), b"BBBB");
  drop(client);
  daemon.stop();
}

/// Format: the status counter of the most batches of asks a shard had out to one owner at once (A-388).
const BATCHES_OUT_PEAK: &str = "merge.work.batches_out_peak";

/// A-388 (§4.16 "Asks are batched per owner"; A-247). Do: two works of one client, so on its shard, not their green's
/// (A-400); hold the
/// green's owner shard; send, without waiting, an edit of a base file from each, so each asks the owner and the first
/// ask's batch is out, unanswered, when the second is made; then let the owner go. Expect: both edits answered, and the
/// works' shard had two batches out to the owner at once. With one batch out at a time the second ask waited in the
/// shard for the first batch's round trip before it left (a peak of 1).
#[test]
fn a_shards_second_ask_leaves_while_its_first_batch_is_still_out() {
  static HELD: AtomicBool = AtomicBool::new(false);
  let (daemon, instance) = daemon(MANY_SHARDS, "two-out");
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "two-out-green");
  let (seed, _) = client.create_work(green, "two-out-seed").unwrap();
  client.edit(seed, "first", 0, 0, b"....").unwrap();
  client.edit(seed, "second", 0, 0, b"....").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let partition = client_partition(&client);
  let (first_work, _) = client.create_work(green, "two-out-first").unwrap();
  let (second_work, _) = client.create_work(green, "two-out-second").unwrap();
  let owner = daemon.shards()[usize::from(owner_of(green))];
  let holding = daemon
    .observation(Some(owner), OBSERVE_BUDGET_NS, |_: &mut ShardState| {
      HELD.store(true, Ordering::Release);
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(OWNER_HOLD);
    })
    .unwrap();
  let waiter = std::thread::spawn(move || holding.wait().unwrap());
  while !HELD.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  let edit = |work: VolumeId, path: &str| RequestBody::Edit {
    work,
    path: path.to_owned(),
    at: 0,
    delete_len: 4,
    bytes: b"AAAA".to_vec(),
  };
  let first = client.begin(&edit(first_work, "first")).unwrap();
  let second = client.begin(&edit(second_work, "second")).unwrap();
  waiter.join().unwrap();
  assert_eq!(reply_of(&mut client, first), ReplyBody::Edited);
  assert_eq!(reply_of(&mut client, second), ReplyBody::Edited);
  let peak = daemon
    .observation(
      Some(daemon.shards()[usize::from(partition)]),
      OBSERVE_BUDGET_NS,
      |state: &mut ShardState| state.refusals.get(BATCHES_OUT_PEAK).copied().unwrap_or(0),
    )
    .unwrap()
    .wait()
    .unwrap();
  assert!(
    peak >= 2,
    "the works' shard had {peak} batch(es) out to the owner at most: the second ask waited for the first's answer"
  );
  drop(client);
  daemon.stop();
}

/// The answer request `id` came to within [`WAIT`], a refusal included; `None` if none came.
fn answer_or_refusal(
  client: &mut Client,
  id: RequestId,
) -> Option<Result<Option<ReplyBody>, ClientError>> {
  let started = Instant::now();
  while started.elapsed() < WAIT {
    match client.poll_reply(id) {
      Ok(None) => std::thread::yield_now(),
      other => return Some(other),
    }
  }
  None
}

/// Shape: the creations sent at once in the concurrent-creation test: below a client's 32 requests in flight with the
/// repeated name beside them, and enough that several are parked together.
const AT_ONCE: usize = 24;

/// A-247 "Creation". Do: send, without waiting, `AT_ONCE` creations of works whose names all hash to one partition that
/// is not the green's, then once more a name already sent. Expect: every creation answered with its own id and every
/// work usable (an edit and a submit accepted), the repeated name refused `AlreadyExists`. An id minted for an
/// admission was this transaction's sequence, which a parked verb never consumed, so creations parked in one step were
/// given one id, queued behind it, and looped the shard.
#[test]
fn works_created_at_once_off_their_greens_shard_get_their_own_ids() {
  let (daemon, instance) = daemon(MANY_SHARDS, "at-once");
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "at-once-green");
  let (seed, _) = client.create_work(green, "at-once-seed").unwrap();
  client.edit(seed, "f", 0, 0, b"....").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let partition = owner_of_name(&off_owner_name(green, "at-once"), usize::from(MANY_SHARDS));
  let names: Vec<String> = (0u32..)
    .map(|n| format!("at-once-{n}"))
    .filter(|name| owner_of_name(name, usize::from(MANY_SHARDS)) == partition)
    .take(AT_ONCE)
    .collect();
  let sent: Vec<RequestId> = names
    .iter()
    .chain(names.first())
    .map(|name| {
      client
        .begin(&RequestBody::CreateWork {
          green,
          name: name.clone(),
        })
        .unwrap()
    })
    .collect();
  let (repeated, created) = sent.split_last().unwrap();
  let replies: Vec<ReplyBody> = created
    .iter()
    .map(|id| reply_of(&mut client, *id))
    .collect();
  let repeat = answer_or_refusal(&mut client, *repeated);
  let mut works = Vec::new();
  for reply in &replies {
    let ReplyBody::WorkCreated { id, .. } = reply else {
      panic!("a creation answers its work: {reply:?}");
    };
    works.push(*id);
  }
  let distinct: std::collections::BTreeSet<_> = works.iter().map(|work| work.bytes).collect();
  assert_eq!(distinct.len(), AT_ONCE, "every work has its own id");
  assert!(
    matches!(
      repeat,
      Some(Err(ClientError::Refused(Refusal::AlreadyExists { .. })))
    ),
    "a name sent twice is held once: {repeat:?}"
  );
  for (index, work) in works.iter().enumerate() {
    client
      .edit(*work, &format!("own-{index}"), 0, 0, b"mine")
      .unwrap();
    assert!(matches!(
      client.submit(*work).unwrap(),
      Submitted::Accepted(_)
    ));
  }
  drop(client);
  daemon.stop();
}

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
  NewWork {
    slot: usize,
  },
  Edit {
    slot: usize,
    file: usize,
    at: u64,
    delete_len: u64,
    bytes: Vec<u8>,
  },
  EditAndSubmit {
    slot: usize,
    file: usize,
    at: u64,
    delete_len: u64,
    bytes: Vec<u8>,
  },
  Submit {
    slot: usize,
  },
  Rebase {
    slot: usize,
  },
  Read {
    slot: usize,
    file: usize,
  },
  Unlink {
    slot: usize,
    file: usize,
  },
  Rename {
    slot: usize,
    from: usize,
    to: usize,
  },
  List {
    slot: usize,
  },
}

/// Shape: the base files and new names a history touches, few enough that its works collide.
const GENERATED_FILES: usize = 4;
/// Shape: the work slots of a generated history.
const GENERATED_WORKS: usize = 4;
/// Format: a generated file's name.
fn generated_path(file: usize) -> String {
  format!("g{file}")
}

fn generated_step() -> impl Strategy<Value = Step> {
  let slot = 0..GENERATED_WORKS;
  let file = 0..GENERATED_FILES;
  (
    0u8..12,
    slot,
    file.clone(),
    file,
    0u64..10,
    0u64..4,
    proptest::collection::vec(b'A'..=b'Z', 0..4),
  )
    .prop_map(
      |(draw, slot, file, other, at, delete_len, bytes)| match draw {
        0 => Step::NewWork { slot },
        1..=3 => Step::Edit {
          slot,
          file,
          at,
          delete_len,
          bytes,
        },
        4..=5 => Step::EditAndSubmit {
          slot,
          file,
          at,
          delete_len,
          bytes,
        },
        6 => Step::Submit { slot },
        7 => Step::Rebase { slot },
        8 => Step::Read { slot, file },
        9 => Step::Unlink { slot, file },
        10 => Step::Rename {
          slot,
          from: file,
          to: other,
        },
        _ => Step::List { slot },
      },
    )
}

/// One daemon's side of the differential.
struct Side {
  client: Client,
  /// The daemon's instance and shards, for a green made off the client's shard (A-400).
  daemon: (String, u16),
  green: VolumeId,
  works: Vec<VolumeId>,
}

impl Side {
  fn begin(&mut self, name: &str) {
    let (instance, shards) = self.daemon.clone();
    self.green = green_off(&mut self.client, &instance, shards, name);
    let (seed, _) = self
      .client
      .create_work(self.green, &format!("{name}-seed"))
      .unwrap();
    for file in 0..GENERATED_FILES {
      self
        .client
        .edit(seed, &generated_path(file), 0, 0, b"0123456789")
        .unwrap();
    }
    assert_eq!(self.client.submit(seed).unwrap(), Submitted::Accepted(1));
    self.works = (0..GENERATED_WORKS)
      .map(|slot| {
        self
          .client
          .create_work(self.green, &format!("{name}-w{slot}"))
          .unwrap()
          .0
      })
      .collect();
  }

  fn run(&mut self, step: &Step, name: &str) -> String {
    let client = &mut self.client;
    match step {
      Step::NewWork { slot } => {
        let (work, base) = client.create_work(self.green, name).unwrap();
        self.works[*slot] = work;
        format!("created at {base}")
      }
      Step::Edit {
        slot,
        file,
        at,
        delete_len,
        bytes,
      } => format!(
        "{:?}",
        client.edit(
          self.works[*slot],
          &generated_path(*file),
          *at,
          *delete_len,
          bytes
        )
      ),
      Step::EditAndSubmit {
        slot,
        file,
        at,
        delete_len,
        bytes,
      } => format!(
        "{:?}",
        client.edit_and_submit(
          (self.works[*slot], &generated_path(*file)),
          *at,
          *delete_len,
          bytes,
          &[],
        )
      ),
      Step::Submit { slot } => submitted(client, self.works[*slot]),
      Step::Rebase { slot } => rebased(client, self.works[*slot]),
      Step::Read { slot, file } => file_of(client, self.works[*slot], &generated_path(*file)),
      Step::Unlink { slot, file } => format!(
        "{:?}",
        client.declare(
          self.works[*slot],
          WorkOp::Unlink {
            path: generated_path(*file)
          }
        )
      ),
      Step::Rename { slot, from, to } => format!(
        "{:?}",
        client.declare(
          self.works[*slot],
          WorkOp::Rename {
            from: generated_path(*from),
            to: generated_path(*to)
          },
        )
      ),
      Step::List { slot } => format!("{:?}", client.list_dir(self.works[*slot], "", ReadAt::Head)),
    }
  }

  fn head(&mut self) -> Vec<String> {
    let mut said = vec![format!("{:?}", self.client.versions(self.green))];
    for file in 0..GENERATED_FILES {
      said.push(file_of(&mut self.client, self.green, &generated_path(file)));
    }
    said
  }
}

fn file_of(client: &mut Client, volume: VolumeId, path: &str) -> String {
  file(client, volume, path)
}

/// A-247's oracle: do run generated histories of works over one green (new works, edits, edit-and-submits, submits,
/// rebases, reads, unlinks, renames and listings, a length change among them) on one shard and on eight; expect every
/// answer and every file of the green's head to agree, and the eight-shard side to have asked the green's owner.
#[test]
fn generated_histories_answer_the_same_on_one_shard_and_on_many() {
  let (one_daemon, one_instance) = daemon(ONE_SHARD, "gen-one");
  let (many_daemon, many_instance) = daemon(MANY_SHARDS, "gen-many");
  let sides = RefCell::new((
    Side {
      client: connect(&one_instance),
      daemon: (one_instance.clone(), ONE_SHARD),
      green: VolumeId::default(),
      works: Vec::new(),
    },
    Side {
      client: connect(&many_instance),
      daemon: (many_instance.clone(), MANY_SHARDS),
      green: VolumeId::default(),
      works: Vec::new(),
    },
  ));
  let histories = RefCell::new(0u64);
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(32),
    ..Config::default()
  }));
  let outcome = runner.run(
    &proptest::collection::vec(generated_step(), 1..32),
    |steps| {
      let mut sides = sides.borrow_mut();
      let (one, many) = &mut *sides;
      let history = {
        let mut histories = histories.borrow_mut();
        *histories += 1;
        *histories
      };
      one.begin(&format!("h{history}"));
      many.begin(&format!("h{history}"));
      for (at, step) in steps.iter().enumerate() {
        let name = format!("h{history}n{at}");
        prop_assert_eq!(
          one.run(step, &name),
          many.run(step, &name),
          "step {} {:?}",
          at,
          step
        );
      }
      prop_assert_eq!(one.head(), many.head(), "the greens after the history");
      Ok(())
    },
  );
  if let Err(failure) = outcome {
    panic!("{failure}");
  }
  assert!(asked_of_the_owner(&many_daemon) > 0);
  assert_eq!(asked_of_the_owner(&one_daemon), 0);
  drop(sides);
  one_daemon.stop();
  many_daemon.stop();
}

/// A-247, the generated oracle's first find (2026-11-09): do, in a work off its green's shard, rename one base file over
/// another, rebase, edit-and-submit an empty edit of a third, then submit two other works; on one shard and on eight.
/// Expect: the same answers and the same green, as the oracle demands of every history.
#[test]
fn a_rename_rebased_then_submitted_off_the_owner_merges_as_on_it() {
  let (one_daemon, one_instance) = daemon(ONE_SHARD, "rename-one");
  let (many_daemon, many_instance) = daemon(MANY_SHARDS, "rename-many");
  let history = [
    Step::Rename {
      slot: 3,
      from: 1,
      to: 3,
    },
    Step::Rebase { slot: 3 },
    Step::EditAndSubmit {
      slot: 3,
      file: 2,
      at: 5,
      delete_len: 0,
      bytes: Vec::new(),
    },
    Step::Submit { slot: 2 },
    Step::Submit { slot: 1 },
  ];
  let mut said = Vec::new();
  for (instance, tag, shards) in [
    (&one_instance, "one", ONE_SHARD),
    (&many_instance, "many", MANY_SHARDS),
  ] {
    let mut side = Side {
      client: connect(instance),
      daemon: (instance.clone(), shards),
      green: VolumeId::default(),
      works: Vec::new(),
    };
    side.begin(&format!("rename-{tag}"));
    let mut answers: Vec<String> = history
      .iter()
      .enumerate()
      .map(|(at, step)| side.run(step, &format!("rename-{tag}-{at}")))
      .collect();
    answers.extend(side.head());
    said.push(answers);
  }
  one_daemon.stop();
  many_daemon.stop();
  assert_eq!(said[0], said[1]);
}

/// Sets `subject`'s write policy as the launcher does, under the daemon's issuer secret, in place of the version it
/// holds, merging as `merge` says.
fn merging(client: &mut Client, secret: &[u8; 32], subject: PolicySubject, merge: WriteMode) {
  let policy = WritePolicy {
    merge,
    land: WriteMode::Ask,
    require_signed_merges: false,
  };
  let (_, version) = client.policy(subject).unwrap();
  client
    .set_policy(
      subject,
      (policy, version),
      policy_proof(secret, &subject, &policy, version),
    )
    .unwrap();
}

/// A-285 under A-247: a merge follows its write policy for a work off its green's shard too, where the green's owner
/// holds no record of the work and the work's shard none of the green. Do, on a daemon with many shards, with works
/// named onto other shards than their green's: forbid the green's merges and submit a work; set a work's own mode to
/// ask, submit it, approve another increment, then from an agent's consumer channel, then with a forged proof, then the
/// launcher's approval of the one asked; ask on the green and edit-and-submit a work. Expect: the green's forbid
/// refuses `PolicyForbids` naming the green; the work's ask answers its identity and one path; `GrantMismatch`,
/// `Forbidden` and `GrantIssuerUnverified` with nothing merged; the launcher's approval accepted; the green's ask
/// answers the edit-and-submit with its identity, the edit kept in the work, and an approval of it accepted; and the
/// agent's own session asking (its consumer's policy), its merge of its own work off the green's shard held with
/// nothing merged. The owner was asked for each verdict, so each ran across the shards.
#[test]
fn a_merge_off_its_greens_shard_follows_both_volumes_write_policy() {
  let (daemon, instance) = daemon(MANY_SHARDS, "policy");
  let secret = daemon.segment().issuer_secret().unwrap();
  let mut client = connect(&instance);
  let asked_before = asked_of_the_owner(&daemon);

  expect_the_greens_forbid_refused(&mut client, &instance, &secret);

  let (green, work) = green_and_edited_work(&mut client, &instance, "policy-ask");
  merging(
    &mut client,
    &secret,
    PolicySubject::Volume { volume: work },
    WriteMode::Ask,
  );
  let Ok(Submitted::ApprovalRequired { identity, paths }) = client.submit(work) else {
    panic!("the work's ask answers an approval request");
  };
  assert_eq!(paths, 1);
  let mut other = identity;
  other[0] ^= 1;
  let mismatched = client.submit_approved(
    work,
    &[],
    other,
    merge_approval_proof(&secret, work, &other),
  );
  assert!(
    matches!(
      mismatched,
      Err(ClientError::Refused(Refusal::GrantMismatch))
    ),
    "an approval of another increment: {mismatched:?}"
  );
  let account = rustix::process::getuid().as_raw();
  let (consumer, capability) = client
    .enroll(account, enroll_proof(&secret, account))
    .unwrap();
  let mut agent = connect(&instance);
  agent.attest(consumer, &capability).unwrap();
  let by_agent = agent.submit_approved(
    work,
    &[],
    identity,
    merge_approval_proof(&secret, work, &identity),
  );
  assert!(
    matches!(
      by_agent,
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "an agent's own approval: {by_agent:?}"
  );
  expect_a_forged_proof_refused_and_the_launchers_accepted(
    &mut client,
    &secret,
    (green, work),
    (identity, other),
  );

  expect_the_greens_ask_to_hold_an_edit_and_submit(&mut client, &instance, &secret);
  // The agent's own session asks (A-285's consumer policy): its merge of its own work off the green's shard is held
  // there too, with nothing merged, though both volumes would merge at once.
  merging(
    &mut client,
    &secret,
    PolicySubject::Consumer { consumer },
    WriteMode::Ask,
  );
  expect_the_sessions_ask_to_hold_its_agents_merge(&mut agent, &instance, (consumer, &capability));
  assert!(
    asked_of_the_owner(&daemon) > asked_before,
    "the verdicts were asked of the green's owner"
  );
  drop(agent);
  drop(client);
  daemon.stop();
}

/// The green's forbid in `a_merge_off_its_greens_shard_follows_both_volumes_write_policy`. Do: forbid a green's merges
/// with `secret`'s authority and submit a work of it. Expect `PolicyForbids` naming the green, and nothing merged.
fn expect_the_greens_forbid_refused(client: &mut Client, instance: &str, secret: &[u8; 32]) {
  let (green, work) = green_and_edited_work(client, instance, "policy-forbid");
  merging(
    client,
    secret,
    PolicySubject::Volume { volume: green },
    WriteMode::Forbid,
  );
  let refused = client.submit(work);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::PolicyForbids {
        act: WriteAct::Merge,
        by: PolicySubject::Volume { volume },
      })) if volume == green
    ),
    "the green's forbid: {refused:?}"
  );
  assert_eq!(client.versions(green).unwrap(), 1, "nothing merged");
}

/// The approvals of `a_merge_off_its_greens_shard_follows_both_volumes_write_policy`'s asked work. Do: approve the
/// increment `identity` of `work` (over `green`) with a proof made for `other`, then with the launcher's proof of it.
/// Expect the forged proof refused `GrantIssuerUnverified` with nothing merged, and the launcher's accepted as version 2.
fn expect_a_forged_proof_refused_and_the_launchers_accepted(
  client: &mut Client,
  secret: &[u8; 32],
  (green, work): (VolumeId, VolumeId),
  (identity, other): ([u8; 32], [u8; 32]),
) {
  let forged = client.submit_approved(
    work,
    &[],
    identity,
    merge_approval_proof(secret, work, &other),
  );
  assert!(
    matches!(
      forged,
      Err(ClientError::Refused(Refusal::GrantIssuerUnverified))
    ),
    "a proof of another increment: {forged:?}"
  );
  assert_eq!(
    client.versions(green).unwrap(),
    1,
    "nothing merged before the approval"
  );
  assert_eq!(
    client.submit_approved(
      work,
      &[],
      identity,
      merge_approval_proof(secret, work, &identity)
    ),
    Ok(Submitted::Accepted(2))
  );
}

/// The green's ask in `a_merge_off_its_greens_shard_follows_both_volumes_write_policy`. Do: set a green's merges to ask
/// with `secret`'s authority, edit-and-submit a work of it, then approve the increment asked. Expect the submit
/// answered with its identity, nothing merged and the edit kept in the work; then the approval accepted.
fn expect_the_greens_ask_to_hold_an_edit_and_submit(
  client: &mut Client,
  instance: &str,
  secret: &[u8; 32],
) {
  let (green, work) = green_and_edited_work(client, instance, "policy-green-ask");
  merging(
    client,
    secret,
    PolicySubject::Volume { volume: green },
    WriteMode::Ask,
  );
  let Ok(Submitted::ApprovalRequired { identity, .. }) =
    client.edit_and_submit((work, "f"), 1, 1, b"Y", &[])
  else {
    panic!("the green's ask answers an approval request");
  };
  assert_eq!(client.versions(green).unwrap(), 1, "nothing merged");
  assert_eq!(
    client.read(work, "f", ReadAt::Head).unwrap(),
    b"XY..".to_vec(),
    "the edit is kept"
  );
  assert_eq!(
    client.submit_approved(
      work,
      &[],
      identity,
      merge_approval_proof(secret, work, &identity)
    ),
    Ok(Submitted::Accepted(2))
  );
}

/// The session's ask in `a_merge_off_its_greens_shard_follows_both_volumes_write_policy` (A-285's consumer policy). Do:
/// as `agent`, whose consumer's own policy asks, make a green and a work named off the green's shard, edit the work and
/// submit it. Expect the merge held for approval with its one path, and nothing merged, though both volumes would merge
/// at once.
fn expect_the_sessions_ask_to_hold_its_agents_merge(
  agent: &mut Client,
  instance: &str,
  (consumer, capability): (u64, &slates_client::Capability),
) {
  // The agent's green, made by a client of another shard bound to the same consumer, so the agent's work is off it.
  let here = client_partition(agent);
  let green = loop {
    let mut maker = connect(instance);
    if client_partition(&maker) != here {
      maker.attest(consumer, capability).unwrap();
      break maker.create_green("policy-session-green", false).unwrap();
    }
  };
  let (work, _) = agent
    .create_work(green, &off_owner_name(green, "policy-session"))
    .unwrap();
  agent.edit(work, "f", 0, 0, b"agent's").unwrap();
  let held = agent.submit(work);
  assert!(
    matches!(held, Ok(Submitted::ApprovalRequired { paths: 1, .. })),
    "the session's ask holds its agent's merge: {held:?}"
  );
  assert_eq!(agent.versions(green).unwrap(), 0, "nothing merged");
}

/// D-O17 for a work off its green's shard (A-776, A-247; close item C1). Do: on eight shards, seed a green made by
/// another shard's client with a file of 200 short functions; from two works of this client (so off the green's shard,
/// composed on their own), rewrite the file whole, one changing a line near the top and one a line near the end;
/// submit both; read the head. Expect: both accepted and the head holding both edits, the increments composed on the
/// works' shard. Until the work's shard asked its owner for the base bytes of a file it replaced whole, it composed
/// the whole replacement unrefined, and the second submit was an overlap conflict.
#[test]
fn whole_file_rewrites_on_works_off_the_greens_shard_merge_as_their_edits() {
  let (daemon, instance) = daemon(MANY_SHARDS, "rewrites");
  let mut client = connect(&instance);
  let green = green_off(&mut client, &instance, MANY_SHARDS, "rewrites-green");
  let base: String = (0..200)
    .map(|n| format!("fn f{n}(a: u32) -> u32 {{\n    a + {n}\n}}\n"))
    .collect();
  let first = base.replacen("a + 3\n", "a * 3\n", 1);
  let second = base.replacen("a + 190\n", "a - 190\n", 1);
  let (seed, _) = client.create_work(green, "rewrites-seed").unwrap();
  client.edit(seed, "f", 0, 0, base.as_bytes()).unwrap();
  let seeded = submitted(&mut client, seed);
  let (one, _) = client.create_work(green, "rewrites-one").unwrap();
  let (two, _) = client.create_work(green, "rewrites-two").unwrap();
  let whole = u64::try_from(base.len()).unwrap();
  client.edit(one, "f", 0, whole, first.as_bytes()).unwrap();
  client.edit(two, "f", 0, whole, second.as_bytes()).unwrap();
  let answers = (submitted(&mut client, one), submitted(&mut client, two));
  let head = client.read(green, "f", ReadAt::Head).unwrap();
  let composed = daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(COMPOSED_ON_WORK)
    .copied()
    .unwrap_or(0);
  drop(client);
  daemon.stop();
  assert_eq!(seeded, "accepted 1");
  assert_eq!(answers, ("accepted 2".to_owned(), "accepted 3".to_owned()));
  assert_eq!(
    String::from_utf8(head).unwrap(),
    first.replacen("a + 190\n", "a - 190\n", 1)
  );
  assert!(
    composed >= 2,
    "the works composed on their own shard: {composed}"
  );
}
