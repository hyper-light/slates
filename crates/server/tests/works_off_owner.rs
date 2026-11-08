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

use slates_client::{Client, Rebased, Submitted};
use slates_ipc::protocol::{ReadAt, VolumeId};
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
  for index in 0..WORKS {
    said.push(file(client, green, &format!("own-{index}")));
    said.push(file(client, works[index], &format!("own-{index}")));
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
