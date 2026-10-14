//! The shards virtio-fs backend (`slates_bridge_fuse::shards_fs`) serves a slates volume to a `shards` microVM over
//! shards' own Unix-socket framing (a little-endian u32 length, then the FUSE bytes), so a shards guest's virtio-fs
//! device — pointed at this socket — mounts a slates volume with no kernel NFS or 9p (§4.6; `docs/wip/shards/`). The
//! non-DAX path: shards' frame carries a FUSE request, not a `FUSE_SETUPMAPPING`. Proven by use over a
//! `UnixStream::pair`: a guest GETATTR, framed as shards frames it, is read, dispatched through the same slates FUSE
//! bridge the kernel mount uses, and answered framed back; and a closed socket ends serving at EOF, not an error.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::os::unix::net::UnixStream;

use slates_bridge_core::{Attachments, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::shards_fs::{read_shards_frame, serve_one, write_shards_frame};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;
const REGION_PAGES: usize = 1024;

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

fn test_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
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

/// A FUSE request: the 40-byte in-header (`len`, `opcode`, `unique`, `nodeid`, …) then `body`.
fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// The errno a FUSE reply carries (the out-header's `error`, a signed little-endian i32 at offset 4).
fn errno(out: &[u8]) -> i32 {
  i32::from_le_bytes(out[4..8].try_into().unwrap())
}

/// §4.6 (the shards integration). Do: serve a slates volume over shards' fs framing on one end of a socket pair;
/// from the other end, frame a guest GETATTR of the root as shards frames it, and read the framed reply; then close.
/// Expect: the request is served (`serve_one` → true), the reply's errno is 0 and it carries the root's attributes
/// (more than a bare header), and the closed socket ends serving at EOF (`serve_one` → false), never an error — so a
/// shards microVM's virtio-fs device, pointed here, reaches a slates volume through the one FUSE bridge.
#[test]
fn a_shards_microvm_fs_frame_is_served_by_the_slates_bridge() {
  let mut store = store();
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = test_cx();

  let (mut client, mut server) = UnixStream::pair().unwrap();
  let mut reply = vec![0u8; 1 << 16];

  // A guest GETATTR of the root (nodeid 1), framed as shards' virtio-fs device frames it: le32 length, then the bytes.
  let getattr = message(Opcode::GetAttr as u32, 1, 1, &[0u8; 16]);
  write_shards_frame(&mut client, &getattr).unwrap();
  assert!(
    serve_one(&mut server, &mut bridge, &cx, &mut reply).unwrap(),
    "a framed request is read, dispatched and answered"
  );
  let answered = read_shards_frame(&mut client, 1 << 16).unwrap().expect("a framed reply");
  assert_eq!(errno(&answered), 0, "slates answered the guest's GETATTR over shards' le32 framing");
  assert!(answered.len() > OUT_HEADER_LEN, "the reply carries the root's attributes, not a bare error header");

  // The guest's fs device closes the socket: serving ends at EOF, cleanly.
  drop(client);
  assert!(
    !serve_one(&mut server, &mut bridge, &cx, &mut reply).unwrap(),
    "a closed socket ends serving at EOF, not an error"
  );
}
