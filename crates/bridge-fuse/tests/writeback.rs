//! The dirty-page write-back rule's FUSE side, by use over a real volume bridge (§4.6 "Cache posture";
//! `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`): a cache write-back
//! is told from a process's write by its `FUSE_WRITE_CACHE` mark; one is judged whole by the rule every transport shares
//! (A-248: applied, applied over stale bytes equal to the file's, or refused), its stale bytes compared with the
//! volume's bytes however many reads the bridge's cap takes; and a file's first writable open drops the kernel's cache
//! so every page cached after it has a known fetch.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
// Stale ranges are lists of byte ranges; a list holding one range is not the numbers inside it.
#![allow(clippy::single_range_in_vec_init)]

use slates_bridge_core::dax_backing::read_whole;
use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode, WRITE_CACHE, open};
use slates_bridge_fuse::bridge::{
  cache_writeback, clear_keep_cache, created_node, dispatch, granted_handle, opens_for_writing,
};
use slates_bridge_fuse::coherence::{WritebackVerdict, judge_writeback};
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

/// Shape: the judged file's length, twice the volume bridge's 256 KiB read cap, so one stale range over all of it is
/// read back in more than one call.
const JUDGED_BYTES: usize = 512 * 1024;
/// Shape: the bytes the test writes the judged file in at a time.
const WRITE_PIECE: usize = 64 * 1024;

/// One write-back the rule judges: what it is, its stale ranges (`None` when the states cannot tell), its offset and
/// bytes, and the verdict it must get.
type Judged<'a> = (
  &'a str,
  Option<Vec<std::ops::Range<u64>>>,
  u64,
  &'a [u8],
  WritebackVerdict,
);

/// A-248, the shared write-back rule by use over a real volume bridge, its reader the one the FUSE transports use. Do:
/// fill a file of twice the bridge's read cap with `A` and judge write-backs of it. Expect: none stale, applied; stale
/// bytes that differ from the file's (a store over another path's change), a conflict; stale bytes equal to the file's,
/// identical, also when the stale range spans the whole file and so more than one capped read; a stale range past the
/// file's end, which the file cannot show equal, a conflict; and states that cannot tell, unknown. Only the first and
/// the identical ones apply.
#[test]
fn a_write_back_is_judged_whole_against_the_files_bytes_now() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = cx();
  let root = bridge.root(&cx).unwrap();
  let (file, _fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "f", 0o644, 0)
    .unwrap();
  let object = ObjectId::new(file.ino, 0);
  for at in (0..JUDGED_BYTES).step_by(WRITE_PIECE) {
    bridge
      .write(
        object,
        &cx,
        u64::try_from(at).unwrap(),
        &[b'A'; WRITE_PIECE],
      )
      .unwrap();
  }
  let mut reads = 0usize;
  let mut judge = |bridge: &mut VolumeBridge<'_>,
                   stale: Option<&[std::ops::Range<u64>]>,
                   at: u64,
                   data: &[u8]| {
    judge_writeback(stale, at, data, |offset, len, out| {
      reads += 1;
      read_whole(bridge, &cx, object, (offset, len), out).is_ok()
    })
  };
  let page = vec![b'M'; PAGE];
  let mut store_and_stale = vec![b'A'; PAGE];
  store_and_stale[10] = b'M';
  let whole = vec![b'A'; JUDGED_BYTES];
  let whole_end = u64::try_from(JUDGED_BYTES).unwrap();
  let cases: [Judged<'_>; 6] = [
    (
      "nothing stale",
      Some(vec![]),
      0,
      &page,
      WritebackVerdict::Apply,
    ),
    (
      "a store over stale bytes another path changed",
      Some(vec![100..200]),
      0,
      &page,
      WritebackVerdict::Conflict,
    ),
    (
      "stale bytes equal to the file's",
      Some(vec![100..200]),
      0,
      &store_and_stale,
      WritebackVerdict::Identical,
    ),
    (
      "one stale range past the read cap",
      Some(vec![0..whole_end]),
      0,
      &whole,
      WritebackVerdict::Identical,
    ),
    (
      "a stale range past the file's end",
      Some(vec![whole_end..whole_end + 100]),
      whole_end,
      &[b'A'; 100],
      WritebackVerdict::Conflict,
    ),
    (
      "states that cannot tell",
      None,
      0,
      &page,
      WritebackVerdict::Unknown,
    ),
  ];
  for (case, stale, at, data, expected) in cases {
    let verdict = judge(&mut bridge, stale.as_deref(), at, data);
    assert_eq!(verdict, expected, "{case}");
    assert_eq!(
      verdict.applies(),
      matches!(
        expected,
        WritebackVerdict::Apply | WritebackVerdict::Identical
      ),
      "{case}: applied exactly when no stale byte differs"
    );
  }
  assert!(
    reads >= 4,
    "non-vacuous: the stale bytes were read back, {reads} time(s)"
  );
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
