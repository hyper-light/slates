#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The FSKit shim codec, exercised by use (R5): requests round-trip through their bytes, hostile
//! messages are typed refusals, and `serve` drives the read path against a real `VolumeBridge` over an
//! in-memory scratch volume — the same seam the FUSE and NFS bridges dispatch onto. No socket, no
//! mount, no Swift: the Rust half of the FSKit bridge is confirmable on any host, exactly as the NFS
//! wire codec was built and confirmed first.

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_fskit::{ShimRequest, ShimWireError, serve};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
// The reply status bytes, mirrored from the codec's wire (a test reads them directly).
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;
// The ShimError tag a missing object maps to (NotFound is the first variant).
const SHIM_NOT_FOUND: u8 = 0;

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

fn write_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0x11; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

/// Each request round-trips through its canonical bytes unchanged — the wire is stable.
#[test]
fn a_request_round_trips_through_its_bytes() {
  let requests = [
    ShimRequest::Lookup {
      parent: oid(1),
      name: "hello".to_owned(),
    },
    ShimRequest::GetAttr { object: oid(42) },
    ShimRequest::Read {
      object: oid(42),
      offset: 4096,
      size: 512,
    },
  ];
  for request in requests {
    let bytes = request.encode();
    assert_eq!(
      ShimRequest::decode(&bytes),
      Ok(request),
      "round-trip is identity"
    );
  }
}

/// A truncated, unknown, over-claiming or trailing-byte message is a typed refusal — never a panic or
/// an over-read (the message crossed the sandboxed extension boundary).
#[test]
fn a_hostile_message_is_a_typed_refusal() {
  // Empty: no operation tag.
  assert_eq!(ShimRequest::decode(&[]), Err(ShimWireError::Truncated));
  // An unknown operation tag.
  assert_eq!(
    ShimRequest::decode(&[0xff]),
    Err(ShimWireError::UnknownOp { tag: 0xff })
  );
  // GetAttr with a truncated object (needs 16 bytes, has 4).
  assert_eq!(
    ShimRequest::decode(&[2, 1, 2, 3, 4]),
    Err(ShimWireError::Truncated)
  );
  // A well-formed GetAttr with extra trailing bytes.
  let mut trailing = ShimRequest::GetAttr { object: oid(1) }.encode();
  trailing.push(0);
  assert_eq!(
    ShimRequest::decode(&trailing),
    Err(ShimWireError::TrailingBytes)
  );
  // A Lookup whose name length claims more than the 255-byte cap.
  let mut oversized = vec![1u8];
  oversized.extend_from_slice(&1u64.to_le_bytes()); // parent inode
  oversized.extend_from_slice(&0u64.to_le_bytes()); // parent generation
  oversized.extend_from_slice(&300u32.to_le_bytes()); // name length past the cap
  assert_eq!(
    ShimRequest::decode(&oversized),
    Err(ShimWireError::BadLength)
  );
}

/// `serve` drives the read path against a real bridge: after creating a file, a Lookup finds it, a
/// GetAttr on its inode returns an OK attribute reply, and a Read returns its bytes — the whole path
/// with no ring and no mount.
#[test]
fn serve_drives_the_read_path_over_a_real_bridge() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root = bridge.root(&cx).unwrap();
  let created = bridge.create(oid(root), &cx, "hello", 0o644, 0).unwrap();
  let (attr, fh) = created;
  bridge.write(oid(attr.ino), &cx, 0, b"world").unwrap();
  bridge.release(oid(attr.ino), &cx, fh).ok();

  // Lookup finds the child and returns an OK attribute reply naming its inode.
  let lookup = ShimRequest::Lookup {
    parent: oid(root),
    name: "hello".to_owned(),
  }
  .encode();
  let reply = serve(&lookup, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "lookup succeeded");
  let mut ino_bytes = [0u8; 8];
  ino_bytes.copy_from_slice(&reply[1..9]);
  assert_eq!(
    u64::from_le_bytes(ino_bytes),
    attr.ino,
    "the reply names the child inode"
  );

  // GetAttr on the child returns an OK attribute reply.
  let getattr = ShimRequest::GetAttr {
    object: oid(attr.ino),
  }
  .encode();
  let reply = serve(&getattr, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "getattr succeeded");

  // Read returns the file's bytes after the OK status and a u32 length.
  let read = ShimRequest::Read {
    object: oid(attr.ino),
    offset: 0,
    size: 16,
  }
  .encode();
  let reply = serve(&read, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "read succeeded");
  let mut len_bytes = [0u8; 4];
  len_bytes.copy_from_slice(&reply[1..5]);
  let len = usize::try_from(u32::from_le_bytes(len_bytes)).unwrap();
  assert_eq!(
    &reply[5..5 + len],
    b"world",
    "the read returned the written bytes"
  );
}

/// A refusal from the bridge becomes an error reply carrying the mapped ShimError tag — the read of a
/// nonexistent object is NotFound, not a crash.
#[test]
fn serve_maps_a_refusal_to_an_error_reply() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();

  let getattr = ShimRequest::GetAttr { object: oid(9_999) }.encode();
  let reply = serve(&getattr, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_ERR, "a missing object is an error reply");
  assert_eq!(reply[1], SHIM_NOT_FOUND, "the refusal is NotFound");
}
