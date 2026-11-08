//! A volume listing at the scale a daemon holds (§4.4 `slates.list`, §4.3 "Bounded work everywhere"): every volume the
//! caller may read, however many there are. One reply is one bulk chunk, so a listing longer than one chunk is answered
//! in pages the client walks. Before 2026-11-08 the whole listing was one reply: `slates volume list` was refused
//! "reply too large for the bulk area" at 100 volumes on a two-shard daemon (found by liveness in its restart harness).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, NamePolicy, SizeClass, VolumeId};
use slates_ipc::protocol::{ReplyBody, RequestBody};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;

/// Shape: two shards, so the listing is gathered across shards, as the CLI's daemon's is.
const TEST_SHARDS: u16 = 2;
/// Shape: the volumes listed: thousands, the scale liveness's harness and an operator's server reach, and about thirty
/// replies' worth of summaries at these names.
const VOLUMES: usize = 3_000;
/// Shape: each volume's quota, the smallest bounded one the store admits room for at this count.
const VOLUME_BYTES: u64 = 1 << 20;
/// Shape: how long the asynchronous listing may take to answer every page, and the destroys to complete.
const ASYNC_WAIT: Duration = Duration::from_secs(60);

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-volume-list-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-volume-list-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn spec(n: usize) -> CreateSpec {
  CreateSpec {
    name: format!("listed-{n:05}"),
    size: SizeClass::Bounded {
      limit: VOLUME_BYTES,
    },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// Creates [`VOLUMES`] volumes; their ids.
fn populate(client: &mut Client) -> BTreeSet<VolumeId> {
  (0..VOLUMES)
    .map(|n| {
      client
        .create(&spec(n))
        .unwrap_or_else(|e| panic!("volume {n}: {e}"))
    })
    .collect()
}

/// Lists until the listing is exactly `survivors`, within [`ASYNC_WAIT`]: a destroy is answered before its teardown's
/// slices end (§4.4 `destroy`), and the volume is listed until then, while every survivor is listed throughout.
fn await_listing_of(client: &mut Client, survivors: &BTreeSet<VolumeId>) {
  let started = Instant::now();
  loop {
    let listed: BTreeSet<VolumeId> = client.list().unwrap().iter().map(|v| v.id).collect();
    if listed == *survivors {
      return;
    }
    assert!(
      listed.is_superset(survivors) && started.elapsed() < ASYNC_WAIT,
      "every survivor is listed while the destroys complete, and only the survivors within {ASYNC_WAIT:?}"
    );
    std::hint::spin_loop();
  }
}

/// §4.4, §4.3. Do: on a two-shard daemon create 3,000 volumes, then list them through the client's list, its
/// asynchronous list (begin, then poll to the end), and once more after destroying every tenth. Expect: each listing
/// holds every volume exactly once, in name order, and the last holds exactly the survivors. Before 2026-11-08 each
/// was refused `BadRequest { reason: "reply too large for the bulk area" }`.
#[test]
fn a_listing_of_thousands_of_volumes_holds_each_once_in_name_order() {
  let (daemon, instance) = daemon("thousands");
  let mut client = connect(&instance);
  let made = populate(&mut client);
  let listed = client.list().expect("the listing of every volume");
  let ids: Vec<VolumeId> = listed.iter().map(|v| v.id).collect();
  assert_eq!(ids.len(), made.len(), "every volume, each once");
  assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>(), made);
  assert!(
    listed.windows(2).all(|pair| pair[0].name <= pair[1].name),
    "in name order"
  );
  let word = client.list_begin().unwrap().word();
  let started = Instant::now();
  let walked = loop {
    if let Some(volumes) = client.list_poll(word).expect("the asynchronous listing") {
      break volumes;
    }
    assert!(
      started.elapsed() < ASYNC_WAIT,
      "the asynchronous listing answered within {ASYNC_WAIT:?}"
    );
    std::hint::spin_loop();
  };
  assert_eq!(walked, listed, "the asynchronous listing is the same");
  let mut survivors = made.clone();
  for (n, id) in made.iter().enumerate() {
    if n % 10 == 0 {
      client.destroy(*id).unwrap();
      survivors.remove(id);
    }
  }
  await_listing_of(&mut client, &survivors);
  drop(client);
  daemon.stop();
}

/// The volumes of one listing page and its cursor, through the wire as the client walks it.
fn page(client: &mut Client, after: Option<VolumeId>) -> (Vec<VolumeId>, Option<VolumeId>) {
  match client.call(&RequestBody::List { after }).unwrap() {
    ReplyBody::Listed { volumes, next } => (volumes.iter().map(|v| v.id).collect(), next),
    other => panic!("a listing page: {other:?}"),
  }
}

/// §4.4. Do: create 3,000 volumes and walk the listing a page at a time; before each page after the first, destroy
/// one volume not yet listed and create a new one. Expect: more than one page (the listing pages at all), every
/// volume that existed for the whole walk listed exactly once, no volume listed twice, and the pages in increasing id
/// order. A destroyed volume may still be listed: a destroy is answered before its teardown's slices end, and the
/// volume is listed until then (under load 198 on this Mac, one was, 2026-11-08).
#[test]
fn a_volume_that_lives_through_a_paged_walk_is_listed_exactly_once() {
  let (daemon, instance) = daemon("churn");
  let mut client = connect(&instance);
  let made = populate(&mut client);
  let mut destroyed = BTreeSet::new();
  let mut listed: Vec<VolumeId> = Vec::new();
  let mut after = None;
  let mut pages = 0usize;
  loop {
    if pages > 0 {
      let unlisted = made
        .iter()
        .rev()
        .find(|id| !destroyed.contains(*id) && after.is_some_and(|after| **id > after))
        .copied();
      if let Some(id) = unlisted {
        client.destroy(id).unwrap();
        destroyed.insert(id);
      }
      client.create(&spec(VOLUMES + pages)).unwrap();
    }
    let (ids, next) = page(&mut client, after);
    pages += 1;
    assert!(
      ids.windows(2).all(|pair| pair[0] < pair[1])
        && after.is_none_or(|after| ids.first().is_none_or(|first| *first > after)),
      "page {pages} continues in id order"
    );
    listed.extend(ids);
    match next {
      Some(next) => after = Some(next),
      None => break,
    }
  }
  eprintln!(
    "walked {} volumes in {pages} pages ({} destroyed and {} created during the walk)",
    listed.len(),
    destroyed.len(),
    pages - 1
  );
  assert!(pages > 1, "the listing took {pages} page(s): it must page");
  let seen: BTreeSet<VolumeId> = listed.iter().copied().collect();
  assert_eq!(seen.len(), listed.len(), "no volume listed twice");
  for id in made.difference(&destroyed) {
    assert!(
      seen.contains(id),
      "a volume that lived through the walk is listed"
    );
  }
  drop(client);
  daemon.stop();
}

/// §4.4, §4.3. Do: create a volume with the longest name the daemon admits (found by asking: one byte more is refused,
/// by the client before sending or by the daemon `InvalidName`), and list. Expect: the listing holds it: every volume a
/// client can create, a client can list. Before 2026-11-08 a `create` carried names up to 4,062 bytes, and a listing
/// holding one was refused "reply too large for the bulk area".
#[test]
fn the_longest_name_a_create_carries_is_listed() {
  let (daemon, instance) = daemon("longest");
  let mut client = connect(&instance);
  let (mut fits, mut refused) = (1usize, 1usize << 16);
  while refused - fits > 1 {
    let probe = (fits + refused) / 2;
    match client.create(&CreateSpec {
      name: "n".repeat(probe),
      ..spec(0)
    }) {
      Ok(id) => {
        client.destroy(id).unwrap();
        fits = probe;
      }
      Err(
        slates_client::ClientError::Ipc(_)
        | slates_client::ClientError::Refused(slates_client::Refusal::InvalidName),
      ) => refused = probe,
      Err(other) => panic!("a name of {probe} bytes: {other}"),
    }
  }
  // A fresh name of the same length: a destroyed probe's name is held while its destroy completes.
  let longest = client
    .create(&CreateSpec {
      name: "m".repeat(fits),
      ..spec(0)
    })
    .unwrap();
  for n in 1..=3 {
    client.create(&spec(n)).unwrap();
  }
  let listed = client
    .list()
    .unwrap_or_else(|e| panic!("a listing holding a {fits}-byte name: {e}"));
  assert!(
    listed
      .iter()
      .any(|v| v.id == longest && v.name.len() == fits),
    "the {fits}-byte name is listed"
  );
  assert_eq!(listed.len(), 4, "and the others");
  eprintln!("the longest name admitted and listed: {fits} bytes");
  drop(client);
  daemon.stop();
}
