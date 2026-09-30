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

use slates_land::engine::{LandingRefusal, LandingRequest, LandingState, LandingTarget};
use slates_land::source::Source;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

mod common;
use common::{LARGE, Session, Setup, config, request, store, unlink, write_file};

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

/// The head's view of `path` through the volume (the overlay's bytes, else the disk's).
fn read(vol: &mut Volume, host: &mut SimHost, store: &mut Store, path: &str) -> Vec<u8> {
  let no = vol.with_host(host).resolve(store, path).unwrap().inode;
  let size = vol.stat(store, no).unwrap().size;
  let mut bytes = vec![0; usize::try_from(size).unwrap()];
  let n = vol.with_host(host).read(store, no, 0, &mut bytes).unwrap();
  bytes.truncate(n);
  bytes
}

/// The paths the head holds diverged.
fn diverged(vol: &Volume, store: &Store) -> Vec<String> {
  vol.diverged(store).into_iter().map(|d| d.path).collect()
}

/// Lands `source` under a fresh single-use grant.
fn land_source(
  host: &mut SimHost,
  vol: &mut Volume,
  store: &mut Store,
  source: Source,
  id: u64,
) -> Result<slates_land::engine::LandingReport, LandingRefusal> {
  let target = LandingTarget {
    dir: host.root(),
    key: "/".into(),
  };
  let mut session = Session::new();
  Setup {
    host,
    target: &target,
    vol,
    store,
    session: &mut session,
  }
  .land(LandingRequest {
    source,
    ..request(id)
  })
}

/// AUD-29-02 (§4.15; A-49). Do: over a base of `/a` and `/b`, edit `/a` and create `/c`, snapshot S; then at
/// the head edit `/a` again, edit `/b` and create `/d`; land S; then land the head. Expect: landing S writes
/// S's `/a` and `/c` and nothing else — `/b` keeps the base's bytes and `/d` is not created — and S's
/// manifest names `/a` and `/c` only; after it the head still reads its own `/a`, `/b` and `/d` privately
/// and `/c` has left the overlay; landing the head then replaces `/a` and `/b` and creates `/d` with no
/// conflict. Before 2026-09-30 a landing of S planned and wrote the head (the server refused it instead).
#[test]
fn a_landing_of_a_snapshot_writes_its_bytes_and_keeps_later_edits_private() {
  let (mut host, mut store, mut vol, s) = edited_past_a_snapshot();
  let landed = land_source(&mut host, &mut vol, &mut store, Source::Snapshot(s), 1).unwrap();
  assert_eq!(landed.state, LandingState::Done, "{landed:?}");
  let mut planned: Vec<&str> = landed.entries.iter().map(|e| e.path.as_ref()).collect();
  planned.sort_unstable();
  assert_eq!(planned, vec!["/a", "/c"], "S's manifest");
  assert_snapshot_on_disk_and_head_private(&mut host, &mut store, &mut vol);
  let head = land_source(&mut host, &mut vol, &mut store, Source::Head, 2).unwrap();
  assert_eq!(head.state, LandingState::Done, "{head:?}");
  assert_eq!(
    host.bytes("/a").unwrap(),
    b"the head's a",
    "a replacement of what S landed"
  );
  assert_eq!(host.bytes("/b").unwrap(), b"the head's b");
  assert_eq!(host.bytes("/d").unwrap(), b"the head's d");
  assert!(diverged(&vol, &store).is_empty());
}

/// Over a base of `/a` and `/b`: `/a` edited and `/c` created, then snapshot S; after it the head edits `/a`
/// again, edits `/b` and creates `/d`.
fn edited_past_a_snapshot() -> (SimHost, Store, Volume, slates_vfs::ids::SnapshotId) {
  let mut host = SimHost::new();
  host.replace_file("/a", b"base a");
  host.replace_file("/b", b"base b");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/a", b"S's a");
  write_file(&mut vol, &mut host, &mut store, "/c", b"S's c");
  let s = vol.snapshot(&mut store).unwrap();
  write_file(&mut vol, &mut host, &mut store, "/a", b"the head's a");
  write_file(&mut vol, &mut host, &mut store, "/b", b"the head's b");
  write_file(&mut vol, &mut host, &mut store, "/d", b"the head's d");
  (host, store, vol, s)
}

/// After S landed: the disk holds S's `/a` and `/c` and the base's `/b`, no `/d`; the head still reads its
/// own `/a`, `/b` and `/d` privately, and `/c` has left the overlay.
fn assert_snapshot_on_disk_and_head_private(
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
) {
  assert_eq!(host.bytes("/a").unwrap(), b"S's a");
  assert_eq!(host.bytes("/c").unwrap(), b"S's c");
  assert_eq!(
    host.bytes("/b").unwrap(),
    b"base b",
    "the head's later edit stayed private"
  );
  assert!(
    host.bytes("/d").is_none(),
    "the head's later create stayed private"
  );
  assert_eq!(read(vol, host, store, "/a"), b"the head's a");
  assert_eq!(read(vol, host, store, "/b"), b"the head's b");
  assert_eq!(read(vol, host, store, "/c"), b"S's c");
  assert_eq!(
    diverged(vol, store),
    vec!["/a", "/b", "/d"],
    "/c left the overlay"
  );
}

/// AUD-29-02 (A-49's advance of a later delete). Do: edit `/a` over the base, snapshot S, delete `/a` at the
/// head; land S; land the head. Expect: S's `/a` lands; the head keeps its deletion private; landing the
/// head then deletes the file S landed, with no conflict.
#[test]
fn a_head_delete_after_the_snapshot_deletes_what_the_snapshot_landed() {
  let mut host = SimHost::new();
  host.replace_file("/a", b"base a");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/a", b"S's a");
  let s = vol.snapshot(&mut store).unwrap();
  unlink(&mut vol, &mut host, &mut store, "/a");

  let landed = land_source(&mut host, &mut vol, &mut store, Source::Snapshot(s), 1).unwrap();
  assert_eq!(landed.state, LandingState::Done, "{landed:?}");
  assert_eq!(host.bytes("/a").unwrap(), b"S's a");
  assert_eq!(
    diverged(&vol, &store),
    vec!["/a"],
    "the head's deletion stayed private"
  );

  let head = land_source(&mut host, &mut vol, &mut store, Source::Head, 2).unwrap();
  assert_eq!(head.state, LandingState::Done, "{head:?}");
  assert!(host.bytes("/a").is_none(), "the head's deletion landed");
}

/// AUD-29-02: a snapshot the volume does not hold is refused before any host write. Do: land a snapshot id
/// the volume never issued. Expect: a typed refusal and the disk unchanged.
#[test]
fn a_snapshot_the_volume_does_not_hold_is_refused_before_any_write() {
  let mut host = SimHost::new();
  host.replace_file("/a", b"base a");
  host.advance_ns(2);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/a", b"edit");
  let s = vol.snapshot(&mut store).unwrap();
  vol.destroy_snapshot(&mut store, s).unwrap();
  let target = LandingTarget {
    dir: host.root(),
    key: "/".into(),
  };
  let mut session = Session::new();
  // Refused at the plan, before any presentation: tried directly, not through a present-then-grant.
  let refused = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .try_land(
    &LandingRequest {
      source: Source::Snapshot(s),
      ..request(1)
    },
    &mut slates_land::engine::Unobserved,
  );
  assert!(
    matches!(refused, Err(LandingRefusal::Volume(_))),
    "{refused:?}"
  );
  assert_eq!(host.bytes("/a").unwrap(), b"base a");
}
