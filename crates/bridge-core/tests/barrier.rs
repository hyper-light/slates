//! The attachment barrier at the seam (AC-3.11/T-3.14; §4.4 "Attachment lifecycle", §4.6
//! "Writeback and snapshot barrier"; GAP-A9-4 "dirty client caches lack a seal barrier"). A
//! snapshot, submit or detach closes the generation of every live attachment of the volume
//! before it publishes, so a write admitted before the barrier and one admitted after belong to
//! different generations and a snapshot between them holds exactly the earlier; the barrier
//! refuses, changing nothing, while a request is still in flight — a consumer lost mid-request
//! is a typed `BarrierIncomplete`, never a clean barrier, until its explicit cleanup. Driven on
//! every host with no mount; the transport's kernel-side flush of dirty pages (the FUSE
//! writeback cache) is the Linux lane's, on top of this.
//!
//! Before this change the registry had no generations and no in-flight accounting: every
//! request carried the attachment's admission epoch only, and nothing could tell a write admitted
//! before a snapshot from one admitted after, or refuse a barrier over a lost consumer.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  AttachmentId, Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for the test volume.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;

const VOLUME: VolumeId = VolumeId { bytes: [7; 16] };
const OTHER_VOLUME: VolumeId = VolumeId { bytes: [9; 16] };

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn rw() -> Rights {
  Rights {
    read: true,
    write: true,
  }
}

fn attach(registry: &mut Attachments, volume: VolumeId) -> AttachmentId {
  registry
    .attach(volume, View::Current, Principal::Uid { uid: 0 }, rw())
    .unwrap()
}

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

/// A barrier closes the generation of every live attachment of the volume — and only that
/// volume: the context admitted before the barrier carries the closed generation, the one
/// admitted after carries the next, and another volume's attachment is untouched.
#[test]
fn a_barrier_closes_every_live_generation_of_the_volume_and_only_that_volume() {
  let mut registry = Attachments::new();
  let a = attach(&mut registry, VOLUME);
  let b = attach(&mut registry, VOLUME);
  let elsewhere = attach(&mut registry, OTHER_VOLUME);

  let before = registry.begin(a).unwrap();
  assert_eq!(before.generation, 1, "the first generation");
  registry.end(a);

  let barrier = registry.barrier(VOLUME).unwrap();
  assert_eq!(
    (barrier.volume, barrier.closed),
    (VOLUME, vec![(a, 1), (b, 1)]),
    "every live attachment of the volume, with the generation it left"
  );
  assert_eq!(
    [
      registry.generation(a),
      registry.generation(b),
      registry.generation(elsewhere)
    ],
    [Some(2), Some(2), Some(1)],
    "the volume's attachments moved on; another volume's did not"
  );
  let after = registry.begin(a).unwrap();
  assert_eq!(after.generation, 2, "admitted into the next generation");
}

/// A barrier refuses while a request is in flight, naming the attachment and its open
/// generation, and changes nothing — the other attachment keeps its generation — until the
/// request ends.
#[test]
fn a_barrier_refuses_while_a_request_is_in_flight_and_changes_nothing() {
  let mut registry = Attachments::new();
  let a = attach(&mut registry, VOLUME);
  let b = attach(&mut registry, VOLUME);
  let _in_flight = registry.begin(a).unwrap();

  assert_eq!(
    registry.barrier(VOLUME),
    Err(VfsError::BarrierIncomplete {
      attachment: a.key(),
      generation: 1,
    })
  );
  assert_eq!(
    [registry.generation(a), registry.generation(b)],
    [Some(1), Some(1)],
    "a refused barrier changes nothing"
  );
  registry.end(a);
  assert_eq!(
    registry.barrier(VOLUME).unwrap().closed,
    vec![(a, 1), (b, 1)]
  );
}

/// A consumer lost mid-request — revoked while its request is in flight, so it will never end
/// it — is a typed incomplete barrier, not a clean one, until the explicit failed-consumer
/// cleanup (`drain`) frees its slot; then the barrier closes the survivors' generations.
#[test]
fn a_consumer_lost_mid_request_is_a_typed_incomplete_barrier_until_its_explicit_cleanup() {
  let mut registry = Attachments::new();
  let survivor = attach(&mut registry, VOLUME);
  let lost = attach(&mut registry, VOLUME);
  let _in_flight = registry.begin(lost).unwrap();
  registry.revoke(lost);

  assert_eq!(
    registry.barrier(VOLUME),
    Err(VfsError::BarrierIncomplete {
      attachment: lost.key(),
      generation: 1,
    }),
    "a lost consumer's outstanding request cannot be reported as flushed"
  );
  assert_eq!(registry.generation(survivor), Some(1), "nothing closed");

  registry.drain(lost);
  let barrier = registry.barrier(VOLUME).unwrap();
  assert_eq!(
    barrier.closed,
    vec![(survivor, 1)],
    "only the survivor is closed"
  );
  assert_eq!(registry.generation(survivor), Some(2));
}

/// By use through the bridge: a write admitted before the barrier and one after belong to
/// different generations, and a snapshot taken between them holds exactly the earlier write —
/// the later one is in the head and not the snapshot, so no view mixes the two.
#[test]
fn writes_before_and_after_a_barrier_belong_to_different_generations_and_the_snapshot_holds_exactly_the_first()
 {
  let mut registry = Attachments::new();
  let mount = attach(&mut registry, VOLUME);
  let mut store = store();
  let mut vol = volume(&mut store);

  // The write admitted before the barrier.
  let earlier = registry.begin(mount).unwrap();
  let ino = {
    let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
    let root = bridge.root(&earlier).unwrap();
    let (attr, fh) = bridge.create(oid(root), &earlier, "f", 0o644, 0).unwrap();
    bridge.write(oid(attr.ino), &earlier, 0, b"one").unwrap();
    bridge.release(oid(attr.ino), &earlier, fh).unwrap();
    attr.ino
  };
  registry.end(mount);

  // The barrier, then the snapshot it guards.
  let barrier = registry.barrier(VOLUME).unwrap();
  assert_eq!(barrier.closed, vec![(mount, earlier.generation)]);
  let snapshot = vol.snapshot(&mut store).unwrap();

  // The write admitted after the barrier: a later generation, in the head only.
  let later: OpContext = registry.begin(mount).unwrap();
  assert_eq!(later.generation, earlier.generation + 1);
  {
    let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
    bridge.write(oid(ino), &later, 0, b"two").unwrap();
  }
  registry.end(mount);

  let mut held = [0u8; 8];
  let n = vol
    .read_in(&store, snapshot, InodeNo(ino), 0, &mut held)
    .unwrap();
  assert_eq!(
    &held[..n],
    b"one",
    "the snapshot holds exactly the earlier generation's write"
  );
  let mut head = [0u8; 8];
  let n = vol.read(&store, InodeNo(ino), 0, &mut head).unwrap();
  assert_eq!(&head[..n], b"two", "the head holds the later");
}
