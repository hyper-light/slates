//! What a landing lands is the state it was based on (§4.15, D-26; A-48): a landing's verdict is a pure
//! function of the witnessed base, the disk now and the overlay, so the witnessed base must be the one the
//! landed state was based on — the snapshot's, for a snapshot or a clone of one, never the head's.
//!
//! Until 2026-09-30 the base plane's witnesses were the head's only, and a clone of an older snapshot
//! inherited them: after the head rewitnessed an entry an outsider had changed, a clone of a snapshot from
//! before judged its old-based bytes against the newer witness, saw "disk unchanged", and landed over the
//! outsider's change without a conflict.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_land::engine::{LandingRefusal, LandingTarget};
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

mod common;
use common::{LARGE, Session, Setup, config, request, store, write_file};

/// Shape: the host clock's step past the timestamp granularity, so an outsider's replacement is never
/// inside the racy window of the witness it drifts from.
const LATER_NS: i64 = 1_000_000_000;

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    config(),
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

/// A-48 (the clone sibling of AUD-29-02). Do: an agent edits `/f` over the base's "base" (its witness W1),
/// snapshots, and after the snapshot an outsider replaces `/f` with "outsider" and the head rewitnesses it;
/// then a clone of the snapshot — whose `/f` is the agent's edit of "base" — lands into the base directory.
/// Expect: the landing is refused as a conflict and the disk keeps the outsider's bytes. Before 2026-09-30
/// the clone judged its edit against the head's newer witness and replaced the outsider's file.
#[test]
fn a_clone_of_a_snapshot_from_before_a_rewitness_conflicts_rather_than_overwrite_the_outsider() {
  let mut host = SimHost::new();
  host.replace_file("/f", b"base");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(
    &mut vol,
    &mut host,
    &mut store,
    "/f",
    b"agent's edit of base",
  );
  let snapshot = vol.snapshot(&mut store).unwrap();
  host.advance_ns(LATER_NS);
  host.replace_file("/f", b"outsider");
  vol.with_host(&mut host).status(&mut store).unwrap();
  let rewitnessed = vol
    .with_host(&mut host)
    .rewitness(&mut store, None)
    .unwrap();
  assert_eq!(
    rewitnessed,
    vec!["/f".to_owned()],
    "the head took the outsider's file in"
  );
  let mut clone = Volume::clone_of(
    &store,
    &mut vol,
    snapshot,
    VolumeConfig {
      prefix: 8,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  let target = LandingTarget {
    dir: host.root(),
    key: "/".into(),
  };
  let mut session = Session::new();
  let landed = Setup {
    host: &mut host,
    target: &target,
    vol: &mut clone,
    store: &mut store,
    session: &mut session,
  }
  .land(request(1));
  println!("the clone's landing: {landed:?}");
  assert!(
    matches!(landed, Err(LandingRefusal::Conflict(_))),
    "the clone's edit was based on \"base\", not the outsider's file: {landed:?}"
  );
  assert_eq!(
    host.bytes("/f").unwrap(),
    b"outsider",
    "the outsider's change survives"
  );
}
