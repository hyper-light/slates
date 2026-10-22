//! The dirty-page write-back rule's FUSE side, by use over a real volume bridge (§4.6 "Cache posture";
//! `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`): a cache write-back
//! is told from a process's write by its `FUSE_WRITE_CACHE` mark; a masked one keeps the volume's newer bytes and
//! answers the whole size; the parts applied, which are also what the daemon logs, leave out exactly the stale ranges;
//! and a file's first writable open drops the kernel's cache so every page cached after it has a known fetch.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
// Stale ranges are lists of byte ranges; a list holding one range is not the numbers inside it.
#![allow(clippy::single_range_in_vec_init)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode, WRITE_CACHE, open};
use slates_bridge_fuse::bridge::{
  cache_writeback, clear_keep_cache, created_node, dispatch, granted_handle, opens_for_writing,
  serve_cache_write, unmasked_segments,
};
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};

mod common;
use common::{PAGE, store, volume};

/// Format: the reply buffer — far above any reply these tests make.
const REPLY_BYTES: usize = 8192;
/// Format: `O_RDWR` and `O_RDONLY`, an open's access modes.
const O_RDWR: u32 = 2;
const O_RDONLY: u32 = 0;

fn cx() -> OpContext {
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
  // The channel's posture: it delivers invalidations, so the bridge grants forever lifetimes (AUD-29-79).
  attachments.set_coherence(id, slates_bridge_core::CacheCoherence::Invalidated);
  attachments.context(id).unwrap()
}

/// A request as the kernel sends it: the header naming `nodeid`, then `body`.
fn message(opcode: Opcode, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// `fuse_write_in` (fh, offset, size, write_flags, lock_owner, flags, padding), then the data.
fn write_body(offset: u64, write_flags: u32, data: &[u8]) -> Vec<u8> {
  let mut body = vec![0u8; 40];
  body[8..16].copy_from_slice(&offset.to_le_bytes());
  body[16..20].copy_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
  body[20..24].copy_from_slice(&write_flags.to_le_bytes());
  body.extend_from_slice(data);
  body
}

/// `fuse_open_in`: flags, open_flags.
fn open_body(flags: u32) -> Vec<u8> {
  let mut body = vec![0u8; 8];
  body[0..4].copy_from_slice(&flags.to_le_bytes());
  body
}

/// `fuse_create_in`: flags, mode, umask, open_flags, then the name.
fn create_body(flags: u32, name: &str) -> Vec<u8> {
  let mut body = vec![0u8; 16];
  body[0..4].copy_from_slice(&flags.to_le_bytes());
  body[4..8].copy_from_slice(&0o100_644u32.to_le_bytes());
  body.extend_from_slice(name.as_bytes());
  body.push(0);
  body
}

/// An open reply's `open_flags`, read from the reply (`fuse_open_out`: fh, then the flags) at `open_out`.
fn open_flags(reply: &[u8], open_out: usize) -> u32 {
  u32::from_le_bytes(reply[open_out + 8..open_out + 12].try_into().unwrap())
}

/// A write is told by its mark. Do: offer a `WRITE` with `FUSE_WRITE_CACHE`, one without, and a `READ`. Expect the
/// first named as a cache write-back of its node, the others not: a process's `write(2)` is write-through here and is
/// never judged against what its kernel cached.
#[test]
fn a_cache_write_back_is_told_from_a_process_write() {
  let marked = message(Opcode::Write, 1, 42, &write_body(0, WRITE_CACHE, b"page"));
  let plain = message(Opcode::Write, 2, 42, &write_body(0, 0, b"page"));
  let read = message(Opcode::Read, 3, 42, &[0u8; 40]);
  assert_eq!(cache_writeback(&marked), Some(42));
  assert_eq!(cache_writeback(&plain), None);
  assert_eq!(cache_writeback(&read), None);
}

/// The parts a write-back applies, and that its log records. Do: ask for the unmasked parts of a 100-byte write at 1000
/// against stale ranges before, inside, touching and covering it. Expect exactly the bytes outside the stale ranges, as
/// file offsets and data ranges, in order.
#[test]
#[allow(clippy::single_range_in_vec_init)] // a list of one stale range, not the numbers inside it
fn the_applied_parts_leave_out_exactly_the_stale_ranges() {
  assert_eq!(
    unmasked_segments(1000, 100, &[]),
    vec![(1000, 0..100)],
    "nothing stale: all of it"
  );
  assert_eq!(
    unmasked_segments(1000, 100, &[0..10, 1020..1030, 1090..1200]),
    vec![(1000, 0..20), (1030, 30..90)],
    "a range before it changes nothing; inside, it splits; past the end, it trims"
  );
  assert_eq!(
    unmasked_segments(1000, 100, &[1000..1050, 1050..1100]),
    Vec::<(u64, std::ops::Range<usize>)>::new(),
    "touching ranges that cover it leave nothing"
  );
  assert_eq!(
    unmasked_segments(1000, 100, &[900..1000, 1100..1200]),
    vec![(1000, 0..100)],
    "ranges that only touch its ends leave all of it"
  );
}

/// A masked write-back by use. Do: write a page of `A` through the bridge; then serve a cache write-back of a page of
/// `M` held stale over bytes 100–200 (another path changed them after the page was fetched). Expect the reply to answer
/// the whole page, 100 bytes kept back, and the volume to hold `M` everywhere but bytes 100–200, which keep their `A`.
#[test]
fn a_masked_write_back_keeps_the_newer_bytes_and_answers_the_whole_page() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = cx();
  let root = bridge.root(&cx).unwrap();
  let (file, _fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "f", 0o644, 0)
    .unwrap();
  let object = ObjectId::new(file.ino, 0);
  bridge.write(object, &cx, 0, &[b'A'; PAGE]).unwrap();

  let request = message(
    Opcode::Write,
    9,
    file.ino,
    &write_body(0, WRITE_CACHE, &[b'M'; PAGE]),
  );
  let mut out = vec![0u8; REPLY_BYTES];
  let (len, kept_back) = serve_cache_write(&request, &mut bridge, &cx, &[100..200], &mut out);
  assert_eq!(kept_back, 100, "the stale bytes were kept back");
  let error = i32::from_le_bytes(out[4..8].try_into().unwrap());
  let size = u32::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 4].try_into().unwrap());
  assert_eq!(
    (len > 0, error, size),
    (true, 0, u32::try_from(PAGE).unwrap()),
    "the whole page answered"
  );

  let mut held = Vec::new();
  bridge
    .read(object, &cx, 0, u32::try_from(PAGE).unwrap(), &mut held)
    .unwrap();
  let mut expected = vec![b'M'; PAGE];
  expected[100..200].fill(b'A');
  assert_eq!(held, expected, "the newer bytes survived the stale page");
}

/// A file with one forever-cached page, created through `bridge`: its node.
fn file(bridge: &mut VolumeBridge<'_>, cx: &OpContext) -> u64 {
  let root = bridge.root(cx).unwrap();
  let (file, _fh) = bridge
    .create(ObjectId::new(root, 0), cx, "f", 0o644, 0)
    .unwrap();
  file.ino
}

/// A file's first writable open drops the kernel's cache. Do: dispatch a read-write `OPEN` through the bridge and clear
/// `FOPEN_KEEP_CACHE` in its reply, as the channel does for a first writable open. Expect the reply to name its handle and
/// to carry `KEEP_CACHE` before and not after, every other flag kept.
#[test]
fn a_first_writable_opens_reply_drops_keep_cache_and_names_its_handle() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = cx();
  let ino = file(&mut bridge, &cx);
  let open = message(Opcode::Open, 11, ino, &open_body(O_RDWR));
  let mut out = vec![0u8; REPLY_BYTES];
  let len = dispatch(&open, &mut bridge, &cx, &mut out);
  let reply = &mut out[..len];
  assert!(
    granted_handle(Some(Opcode::Open), reply).is_some(),
    "the open names its handle"
  );
  let before = open_flags(reply, OUT_HEADER_LEN);
  assert_ne!(
    before & open::KEEP_CACHE,
    0,
    "a forever-cached file keeps its cache by default"
  );
  clear_keep_cache(Some(Opcode::Open), reply);
  let after = open_flags(reply, OUT_HEADER_LEN);
  assert_eq!(after, before & !open::KEEP_CACHE, "only KEEP_CACHE cleared");
}

/// A `CREATE` opens what it makes. Do: dispatch a read-write `CREATE` of a new file and clear `FOPEN_KEEP_CACHE` in its
/// reply. Expect the reply to name a new node and its handle, and its open reply, after the entry, to be cleared.
#[test]
fn a_creates_reply_names_its_node_and_handle_and_drops_keep_cache_too() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = cx();
  let ino = file(&mut bridge, &cx);
  let root = bridge.root(&cx).unwrap();
  let create = message(Opcode::Create, 12, root, &create_body(O_RDWR, "g"));
  let mut out = vec![0u8; REPLY_BYTES];
  let len = dispatch(&create, &mut bridge, &cx, &mut out);
  let reply = &mut out[..len];
  let node = created_node(Some(Opcode::Create), reply).expect("the create names its node");
  assert_ne!(node, ino, "a new node");
  assert!(
    granted_handle(Some(Opcode::Create), reply).is_some(),
    "the create names its handle"
  );
  clear_keep_cache(Some(Opcode::Create), reply);
  assert_eq!(
    open_flags(reply, OUT_HEADER_LEN + EntryOut::LEN) & open::KEEP_CACHE,
    0,
    "the create's open reply is cleared too"
  );
}

/// What is read as opening for writing, and what is left alone. Do: read the access mode of read-write and read-only
/// opens and of a `READ`; then clear `FOPEN_KEEP_CACHE` in an error reply to an `OPEN` of a missing node. Expect writing
/// only for `O_RDWR`, nothing for a `READ`, and the error reply unchanged and naming no handle.
#[test]
fn an_access_mode_is_read_and_an_error_reply_is_left_alone() {
  assert_eq!(
    opens_for_writing(Some(Opcode::Open), &open_body(O_RDWR)),
    Some(true)
  );
  assert_eq!(
    opens_for_writing(Some(Opcode::Open), &open_body(O_RDONLY)),
    Some(false)
  );
  assert_eq!(
    opens_for_writing(Some(Opcode::Read), &open_body(O_RDWR)),
    None
  );
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = cx();
  let ino = file(&mut bridge, &cx);
  let missing = message(Opcode::Open, 13, ino + 100, &open_body(O_RDWR));
  let mut out = vec![0u8; REPLY_BYTES];
  let len = dispatch(&missing, &mut bridge, &cx, &mut out);
  let refused = out[..len].to_vec();
  clear_keep_cache(Some(Opcode::Open), &mut out[..len]);
  assert_eq!(
    &out[..len],
    &refused[..],
    "an error reply is left as it was"
  );
  assert_eq!(granted_handle(Some(Opcode::Open), &out[..len]), None);
}
