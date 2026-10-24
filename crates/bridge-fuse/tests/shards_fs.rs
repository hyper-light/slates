//! The Shards fs backend's frames and answers (`slates_bridge_fuse::shards_fs`), driven the way the daemon drives them
//! (§4.6, the Shards route; A-161): a guest's request arrives as a frame, a little-endian `u32` length then the FUSE
//! request; [`split_frame`] takes it from the bytes read so far, bounded before anything is allocated for it, and
//! [`answer`] dispatches it through the FUSE bridge and writes the reply frame. Here the frames are bytes in memory, no
//! socket and no OS types, so the guest's whole mount and read runs on every platform and architecture, Windows
//! included. The same functions over a real stream end, inside the daemon, are `crates/server/tests/shards.rs`, and a
//! program handed that end by the CLI is `crates/cli/tests/cli.rs`.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::panic
)]

use slates_bridge_core::{Attachments, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::shards_fs::{FRAME_BYTES, Frame, LENGTH_BYTES, Split, answer, split_frame};
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

/// The file bytes a guest reads back; distinctive so the READ reply can only be the file's own content.
const HELLO: &[u8] = b"hello from slates over shards virtio-fs\n";

/// A `u64` at the start of a reply's body (the field `EntryOut.nodeid` and `OpenOut.fh` both begin with).
fn body_u64(reply: &[u8]) -> u64 {
  u64::from_le_bytes(
    reply[OUT_HEADER_LEN..OUT_HEADER_LEN + 8]
      .try_into()
      .unwrap(),
  )
}

/// `request` framed as Shards' device frames it: its length, little-endian, then its bytes.
fn framed(request: &[u8]) -> Vec<u8> {
  let mut frame = u32::try_from(request.len()).unwrap().to_le_bytes().to_vec();
  frame.extend_from_slice(request);
  frame
}

/// Frames `request`, takes it back with [`split_frame`] as the daemon reads a stream, answers it with [`answer`], and
/// returns the reply without its length word, which must name exactly the reply's bytes.
fn exchange(bridge: &mut VolumeBridge<'_>, cx: &OpContext, request: &[u8]) -> Vec<u8> {
  let bytes = framed(request);
  let Split::Whole(Frame::Request(taken), used) = split_frame(&bytes, FRAME_BYTES).unwrap() else {
    panic!("a whole request frame");
  };
  assert_eq!((taken, used), (request, bytes.len()));
  let mut frame = Vec::new();
  let answered = answer(taken, bridge, cx, &mut frame);
  let reply = answered.reply(&frame).to_vec();
  let word = u32::from_le_bytes(frame[..LENGTH_BYTES].try_into().unwrap());
  assert_eq!(usize::try_from(word).unwrap(), reply.len());
  assert_eq!(answered.error, -errno(&reply));
  reply
}

/// §4.6, A-161 (the guest's own mount and read, on every platform and architecture, no socket). Do: put a one-file
/// volume behind the bridge, then drive what a guest kernel's virtio-fs client does to read a file — `INIT`, `LOOKUP`
/// of the name under the root, `OPEN` of the returned inode, `READ` of its bytes — each framed as Shards' device frames
/// it, split and answered as the daemon does. Expect every step answered with errno 0, each reply frame's length word
/// naming exactly its reply, and the `READ` returning the file's own bytes.
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
  let hello = vol
    .create_file_no(&mut store, root, "hello", 0o644)
    .unwrap();
  vol.write(&mut store, hello, 0, HELLO).unwrap();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = test_cx();

  // FUSE_INIT: major 7, minor 38, max_readahead 0, flags 0 — the codec reads what it needs.
  let mut init = Vec::new();
  init.extend_from_slice(&7u32.to_le_bytes());
  init.extend_from_slice(&38u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.resize(64, 0);
  let r = exchange(&mut bridge, &cx, &message(Opcode::Init as u32, 1, 0, &init));
  assert_eq!(errno(&r), 0, "INIT served");

  // LOOKUP "hello" under the root (nodeid 1): the reply's body begins with the file's inode number.
  let r = exchange(
    &mut bridge,
    &cx,
    &message(Opcode::Lookup as u32, 2, 1, b"hello\0"),
  );
  assert_eq!(errno(&r), 0, "LOOKUP of hello served");
  let nodeid = body_u64(&r);
  assert_ne!(nodeid, 0, "the lookup resolved the file to a nodeid");

  // OPEN that inode (fuse_open_in: flags u32, then 4 padding): the reply's body begins with the file handle.
  let mut open_in = 0u32.to_le_bytes().to_vec();
  open_in.extend_from_slice(&[0u8; 4]);
  let r = exchange(
    &mut bridge,
    &cx,
    &message(Opcode::Open as u32, 3, nodeid, &open_in),
  );
  assert_eq!(errno(&r), 0, "OPEN served");
  let fh = body_u64(&r);

  // READ the whole file (fuse_read_in: fh u64, offset u64, size u32, …).
  let mut read_in = vec![0u8; 24];
  read_in[0..8].copy_from_slice(&fh.to_le_bytes());
  read_in[8..16].copy_from_slice(&0u64.to_le_bytes());
  read_in[16..20].copy_from_slice(&u32::try_from(HELLO.len()).unwrap().to_le_bytes());
  let r = exchange(
    &mut bridge,
    &cx,
    &message(Opcode::Read as u32, 4, nodeid, &read_in),
  );
  assert_eq!(errno(&r), 0, "READ served");
  assert_eq!(
    &r[OUT_HEADER_LEN..],
    HELLO,
    "the guest reads the slates file's own bytes through the shards fs backend"
  );
}

/// Hostile input on the frame parser (CLAUDE.md §4: every parser of external bytes): a frame's length is the first four
/// bytes off the stream, the guest's to choose. Do: split a literal `u32::MAX` length, the largest length a request can
/// name, one byte past the caller's bound, a frame that claims 100 bytes and brings 4, and no bytes at all. Expect the
/// first three refused `InvalidData` from the length word alone, before anything is sized by it; the short frame and
/// the empty input partial, naming the bytes they need in all (the daemon then waits for them, and a close before they
/// come is counted as a frame cut short).
#[test]
fn a_hostile_or_truncated_shards_fs_frame_is_refused_before_allocating() {
  use std::io::ErrorKind;
  let bound = 1 << 16;
  for word in [u32::MAX, u32::MAX >> 1, bound + 1] {
    let bytes = word.to_le_bytes();
    assert_eq!(
      split_frame(&bytes, usize::try_from(bound).unwrap())
        .unwrap_err()
        .kind(),
      ErrorKind::InvalidData,
      "length word {word:#x}"
    );
  }
  let mut truncated = 100u32.to_le_bytes().to_vec();
  truncated.extend_from_slice(&[0xAB; 4]);
  assert_eq!(
    split_frame(&truncated, FRAME_BYTES).unwrap(),
    Split::Partial(LENGTH_BYTES + 100)
  );
  assert_eq!(
    split_frame(&[], FRAME_BYTES).unwrap(),
    Split::Partial(LENGTH_BYTES)
  );
}
