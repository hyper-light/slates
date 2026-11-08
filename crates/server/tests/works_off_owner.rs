//! A green's works are served where they live; only the verdict is the green's (§4.16, A-247). One history is run on a
//! daemon with one shard, where a work and its green share a partition, and on a daemon with many, where a green's
//! works are spread over the shards by their names. Every reply and every file must be the same, and the counter of
//! verbs a work's shard asked the green's owner for proves the many-shard run really went there.
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
use slates_client::{Client, ClientError, Rebased, Refusal, RequestId, Submitted};
use slates_ipc::protocol::{ReadAt, ReplyBody, RequestBody, VolumeId, WorkOp};
use slates_server::daemon::OBSERVE_BUDGET_NS;
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
    Ok(Submitted::Accepted(version)) => format!("accepted {version}"),
    Ok(Submitted::Conflict(windows)) => format!("conflict {windows:?}"),
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
fn history(client: &mut Client) -> Vec<String> {
  let mut said = Vec::new();
  let green = client.create_green("spread-green", false).unwrap();
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
    let said = history(&mut client);
    let asked = daemon
      .refusals_on_every_shard()
      .unwrap()
      .get(ASKED_THE_OWNER)
      .copied()
      .unwrap_or(0);
    drop(client);
    daemon.stop();
    (said, asked)
  };
  let (on_one, asked_on_one) = run(ONE_SHARD, "one");
  let (on_many, asked_on_many) = run(MANY_SHARDS, "many");
  for (at, (one, many)) in on_one.iter().zip(&on_many).enumerate() {
    assert_eq!(one, many, "step {at} differs between one shard and many");
  }
  assert_eq!(on_one.len(), on_many.len());
  assert_eq!(asked_on_one, 0, "one shard asks nobody");
  assert!(
    asked_on_many > 0,
    "the many-shard run is not vacuous: its works' shards asked the green's owner"
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

/// A name of `prefix` that hashes to a partition other than `green`'s, so a work of that name is off its owner.
fn off_owner_name(green: VolumeId, prefix: &str) -> String {
  (0u32..)
    .map(|suffix| format!("{prefix}-{suffix}"))
    .find(|name| owner_of_name(name, usize::from(MANY_SHARDS)) != owner_of(green))
    .unwrap()
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
  let green = client.create_green("floor-green", false).unwrap();
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
  let green = client.create_green("restart-green", false).unwrap();
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
    .begin(&RequestBody::Submit {
      work,
      evidence: Vec::new(),
    })
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
  let green = client.create_green("order-green", false).unwrap();
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
fn green_and_edited_work(client: &mut Client, tag: &str) -> (VolumeId, VolumeId) {
  let green = client.create_green(&format!("{tag}-green"), false).unwrap();
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
  let request = RequestBody::Submit {
    work,
    evidence: Vec::new(),
  };
  let id = client.begin(&request).unwrap();
  await_overloaded(client, id);
  assert_eq!(client.versions(green).unwrap(), 2, "the owner moved");
  (id, request)
}

const ACCEPTED_AT_TWO: ReplyBody = ReplyBody::Submitted {
  version: Some(2),
  conflicts: Vec::new(),
};

/// A-247 "Exactly once". Do: submit a work off its green's shard with the owner's answer lost, so the green advanced
/// and the work did not, and the verb is answered `Overloaded`; retry it under its id. Expect: the retry answers the one
/// version the owner recorded, the green is one version ahead and not two, and the work settled at it.
#[test]
fn a_submit_whose_answer_was_lost_is_answered_by_the_owners_record_once() {
  let (daemon, instance) = daemon(MANY_SHARDS, "lost");
  let mut client = connect(&instance);
  let (green, work) = green_and_edited_work(&mut client, "lost");
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
  let green = client.create_green("lost-conflict-green", false).unwrap();
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
  let request = RequestBody::Submit {
    work,
    evidence: Vec::new(),
  };
  let id = client.begin(&request).unwrap();
  await_overloaded(&mut client, id);
  client.edit(other, "f", 32, 4, b"BBBB").unwrap();
  assert_eq!(client.submit(other).unwrap(), Submitted::Accepted(3));
  let ReplyBody::Submitted { version, conflicts } = client.retry(id, &request).unwrap() else {
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
  let (green, work) = green_and_edited_work(&mut client, "lost-then-edit");
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
  let (green, work) = green_and_edited_work(&mut client, "lost-restart");
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
  let green = client.create_green("orphan-green", false).unwrap();
  let (seed, _) = client.create_work(green, "orphan-seed").unwrap();
  client.edit(seed, "file", 0, 0, &version_bytes(1)).unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  client.destroy(seed).unwrap();
  let orphan = off_owner_name(green, "orphan");
  first
    .lose_next_owner_answer(owner_of_name(&orphan, usize::from(MANY_SHARDS)))
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
  let green = client.create_green("behind-green", false).unwrap();
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
    .begin(&RequestBody::Submit {
      work,
      evidence: Vec::new(),
    })
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
  green: VolumeId,
  works: Vec<VolumeId>,
}

impl Side {
  fn begin(&mut self, name: &str) {
    self.green = self.client.create_green(name, false).unwrap();
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
      green: VolumeId::default(),
      works: Vec::new(),
    },
    Side {
      client: connect(&many_instance),
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
