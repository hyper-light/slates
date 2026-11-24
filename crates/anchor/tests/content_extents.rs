//! The content pool's extent objects across a content object's life (macOS, where each extent is a shared-memory object
//! of its own, `slates_mem::extent_handoff`): a fresh content object has none, so any object of its extents' names is a
//! dead run's; and the extents of a content object a segment created go with it. main 01a20182's r633 (2026-11-23, the
//! big Studio): `provision_bench` names its segment the same every run, and a claim met the extent a run before had made
//! at another length, refused `memory: offset 524288 is misaligned or past the 2097152-byte object` (`make_extent` found
//! the object at 512 KiB where the claim wanted 2 MiB); an object of the same length would have been adopted with the
//! dead run's bytes.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Extents are objects of their own on macOS alone: elsewhere they are ranges of the content object.
#![cfg(target_os = "macos")]

use slates_anchor::AnchorSegment;
use slates_anchor::layout::Geometry;
use slates_machine::facts::Identity;
use slates_mem::Handoff;

/// Shape: the content object's length, a few pages.
const CONTENT_BYTES: usize = 1 << 20;
/// Shape: an extent's length as a dead run made it.
const STALE_BYTES: usize = 512 * 1024;
/// Shape: the extent's length as this run's layout wants it, other than [`STALE_BYTES`].
const FRESH_BYTES: usize = 2 * 1024 * 1024;

fn identity() -> Identity {
  Identity {
    cpu: "test".into(),
    os: "test".into(),
    arch: "test".into(),
    cores: 4,
    memory: 1,
    page: 4096,
  }
}

fn geometry() -> Geometry {
  Geometry {
    partitions: 2,
    page: 4096,
    profile_bytes: 8192,
    log_bytes: 65_536,
    snapshot_bytes: 8192,
    landing_slots: 2,
    landing_slot_bytes: 4096,
  }
}

/// A segment named for `test` and this process, with its content object: the segment and the handoff of extent `id`.
fn with_content(test: &str, id: u16) -> (AnchorSegment, Handoff) {
  let name = format!("slates-seg-{test}-{}", std::process::id());
  let content = format!("slates-con-{test}-{}", std::process::id());
  let segment = AnchorSegment::create(&name, &identity(), geometry())
    .unwrap()
    .with_content(&content, CONTENT_BYTES)
    .unwrap();
  let (handoff, _) = segment.content_handoff().unwrap().unwrap();
  let extent = slates_mem::extent_handoff(&handoff, id).expect("macOS names its extents");
  (segment, extent)
}

/// Do: create a segment with its content object and drop it; make extent 3 of its name at 512 KiB, as a run that died
/// would leave it; create the segment again under the same names; make extent 3 at 2 MiB, as this run's layout wants.
/// Expect: the extent made at 2 MiB, the dead run's object gone with the fresh content object. Failing first: refused
/// `OutOfRange` (the object at 512 KiB, kept).
#[test]
fn a_fresh_content_object_clears_a_dead_runs_extent_names() {
  let (first, extent) = with_content("ext-fresh", 3);
  drop(first);
  slates_mem::make_extent(&extent, STALE_BYTES).unwrap();
  let (second, again) = with_content("ext-fresh", 3);
  assert_eq!(again, extent, "the same names");
  let made = slates_mem::make_extent(&extent, FRESH_BYTES);
  slates_mem::unlink_handoff(&extent).unwrap();
  drop(second);
  assert!(made.is_ok(), "the dead run's extent cleared: {made:?}");
}

/// Do: create a segment with its content object; make its extent 5 at 512 KiB, as a shard does at its start; drop the
/// segment; make extent 5 at 2 MiB. Expect: made, its name gone with the content object that created it. Failing
/// first: refused `OutOfRange` (the name outlived its creator, with its pages).
#[test]
fn a_created_content_objects_extents_go_with_it() {
  let (segment, extent) = with_content("ext-gone", 5);
  slates_mem::make_extent(&extent, STALE_BYTES).unwrap();
  drop(segment);
  let made = slates_mem::make_extent(&extent, FRESH_BYTES);
  slates_mem::unlink_handoff(&extent).unwrap();
  assert!(
    made.is_ok(),
    "the extent's name went with its content object: {made:?}"
  );
}
