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

/// The file bytes a guest reads back; distinctive so the READ reply can only be the file's own content.
const HELLO: &[u8] = b"hello from slates over shards virtio-fs\n";

/// A `u64` at the start of a reply's body (the field `EntryOut.nodeid` and `OpenOut.fh` both begin with).
fn body_u64(reply: &[u8]) -> u64 {
  u64::from_le_bytes(reply[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap())
}

/// Serves one request framed as shards frames it and returns the reply frame (asserting it was served).
fn exchange(
  client: &mut UnixStream,
  server: &mut UnixStream,
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  request: &[u8],
) -> Vec<u8> {
  let mut reply = vec![0u8; 1 << 16];
  write_shards_frame(client, request).unwrap();
  assert!(serve_one(server, bridge, cx, &mut reply).unwrap(), "the framed request was served");
  read_shards_frame(client, 1 << 16).unwrap().expect("a framed reply")
}

/// §4.6 (the shards integration — the guest's own mount+read sequence, proven without a VM). Do: put a one-file
/// volume behind the slates `shards_fs` backend on a socket, then from the other end drive exactly what a guest
/// kernel's virtio-fs client does to read a file — FUSE_INIT, then LOOKUP of the name under the root, OPEN of the
/// returned inode, and READ of its bytes — each framed as shards' fs device frames it. Expect: every step is served
/// with errno 0, and the READ returns the file's own bytes. So a shards microVM mounting tag `shards0` and reading
/// `/hello` reaches slates through the one FUSE bridge — the slates half of the live boot, exercised end to end here
/// (the guest-kernel half is the `shards` E2E boot test `crates/shards/tests/slates_fs.rs`).
#[test]
fn the_slates_backend_answers_a_guests_full_mount_and_read_of_a_file() {
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
  // One file `hello` under the root, with distinctive content, before the bridge borrows the volume.
  let root = vol.root_inode(&store).unwrap();
  let hello = vol.create_file_no(&mut store, root, "hello", 0o644).unwrap();
  vol.write(&mut store, hello, 0, HELLO).unwrap();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = test_cx();
  let (mut client, mut server) = UnixStream::pair().unwrap();

  // FUSE_INIT: major 7, minor 38, max_readahead 0, flags 0 — the codec reads what it needs.
  let mut init = Vec::new();
  init.extend_from_slice(&7u32.to_le_bytes());
  init.extend_from_slice(&38u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.resize(64, 0);
  let r = exchange(&mut client, &mut server, &mut bridge, &cx, &message(Opcode::Init as u32, 1, 0, &init));
  assert_eq!(errno(&r), 0, "INIT served");

  // LOOKUP "hello" under the root (nodeid 1): the reply's body begins with the file's inode number.
  let r = exchange(&mut client, &mut server, &mut bridge, &cx, &message(Opcode::Lookup as u32, 2, 1, b"hello\0"));
  assert_eq!(errno(&r), 0, "LOOKUP of hello served");
  let nodeid = body_u64(&r);
  assert_ne!(nodeid, 0, "the lookup resolved the file to a nodeid");

  // OPEN that inode (fuse_open_in: flags u32, then 4 padding): the reply's body begins with the file handle.
  let mut open_in = 0u32.to_le_bytes().to_vec();
  open_in.extend_from_slice(&[0u8; 4]);
  let r = exchange(&mut client, &mut server, &mut bridge, &cx, &message(Opcode::Open as u32, 3, nodeid, &open_in));
  assert_eq!(errno(&r), 0, "OPEN served");
  let fh = body_u64(&r);

  // READ the whole file (fuse_read_in: fh u64, offset u64, size u32, …).
  let mut read_in = vec![0u8; 24];
  read_in[0..8].copy_from_slice(&fh.to_le_bytes());
  read_in[8..16].copy_from_slice(&0u64.to_le_bytes());
  read_in[16..20].copy_from_slice(&u32::try_from(HELLO.len()).unwrap().to_le_bytes());
  let r = exchange(&mut client, &mut server, &mut bridge, &cx, &message(Opcode::Read as u32, 4, nodeid, &read_in));
  assert_eq!(errno(&r), 0, "READ served");
  assert_eq!(
    &r[OUT_HEADER_LEN..],
    HELLO,
    "the guest reads the slates file's own bytes through the shards fs backend"
  );
}
