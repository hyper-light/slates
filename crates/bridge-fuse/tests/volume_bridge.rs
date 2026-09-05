//! The VolumeBridge's tests (Phase 3 task 1; §4.6): the whole stack — the codec, the dispatch,
//! the VolumeBridge, and the volume core — driven against an in-memory scratch volume, so a
//! FUSE CREATE/WRITE/LOOKUP/GETATTR/READ/READDIR round trip is exercised on every host without
//! a mount. This is the read-and-write path the `cargo build` workload leans on.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::dispatch;
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for the test volume.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;

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

/// A request message: the header then the body, `len` set to the total.
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

/// A NUL-terminated name body.
fn name_body(name: &str) -> Vec<u8> {
  let mut b = name.as_bytes().to_vec();
  b.push(0);
  b
}

/// A `fuse_create_in` body: flags, mode, umask, open_flags, then the name.
fn create_body(mode: u32, name: &str) -> Vec<u8> {
  let mut b = vec![0u8; 16];
  b[4..8].copy_from_slice(&mode.to_le_bytes());
  b.extend_from_slice(&name_body(name));
  b
}

fn ok(out: &[u8]) -> bool {
  u32::from_le_bytes(out[4..8].try_into().unwrap()) == 0
}

/// The node id a CREATE or LOOKUP reply names (the start of `fuse_entry_out`).
fn reply_nodeid(out: &[u8]) -> u64 {
  u64::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap())
}

/// The file handle a CREATE reply names (the `fuse_open_out` after the entry).
fn create_reply_fh(out: &[u8]) -> u64 {
  let at = OUT_HEADER_LEN + EntryOut::LEN;
  u64::from_le_bytes(out[at..at + 8].try_into().unwrap())
}

/// Creates "build.rs" in the root; returns its node id and its handle.
fn create(bridge: &mut VolumeBridge<'_>, out: &mut [u8]) -> (u64, u64) {
  let n = dispatch(
    &message(
      Opcode::Create.to_wire(),
      1,
      1,
      &create_body(0o100_644, "build.rs"),
    ),
    bridge,
    out,
  );
  assert!(ok(out), "create ok");
  assert!(n > OUT_HEADER_LEN);
  (reply_nodeid(out), create_reply_fh(out))
}

/// Writes `data` at offset 0 on `fh` of `file`; asserts the reported count.
fn write(bridge: &mut VolumeBridge<'_>, file: u64, fh: u64, data: &[u8], out: &mut [u8]) {
  let mut w = vec![0u8; 40];
  w[0..8].copy_from_slice(&fh.to_le_bytes());
  w[16..20].copy_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
  w.extend_from_slice(data);
  dispatch(&message(Opcode::Write.to_wire(), 2, file, &w), bridge, out);
  assert!(ok(out), "write ok");
  assert_eq!(
    u32::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 4].try_into().unwrap()),
    u32::try_from(data.len()).unwrap()
  );
}

/// Opens `file` and reads `size` bytes; returns them.
fn open_and_read(bridge: &mut VolumeBridge<'_>, file: u64, size: u32, out: &mut [u8]) -> Vec<u8> {
  let mut open_body = vec![0u8; 8];
  open_body[0..4].copy_from_slice(&0u32.to_le_bytes());
  dispatch(
    &message(Opcode::Open.to_wire(), 5, file, &open_body),
    bridge,
    out,
  );
  assert!(ok(out), "open ok");
  let rfh = u64::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap());
  let mut r = vec![0u8; 24];
  r[0..8].copy_from_slice(&rfh.to_le_bytes());
  r[16..20].copy_from_slice(&size.to_le_bytes());
  let n = dispatch(&message(Opcode::Read.to_wire(), 6, file, &r), bridge, out);
  out[OUT_HEADER_LEN..n].to_vec()
}

/// GETATTR reports the file's size (fuse_attr_out is 16 bytes then fuse_attr; size is its
/// second u64).
fn getattr_size(bridge: &mut VolumeBridge<'_>, file: u64, out: &mut [u8]) -> u64 {
  dispatch(
    &message(Opcode::GetAttr.to_wire(), 4, file, &[]),
    bridge,
    out,
  );
  assert!(ok(out));
  let size_at = OUT_HEADER_LEN + 16 + 8;
  u64::from_le_bytes(out[size_at..size_at + 8].try_into().unwrap())
}

/// A whole FUSE round trip through the volume: create a file, write to it, look it up, stat it,
/// open and read it back, and list the root — every reply decoding to the right value.
#[test]
fn a_fuse_round_trip_drives_the_volume_core() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let mut out = vec![0u8; 1 << 16];
  let data = b"fn main() {}";

  let (file, fh) = create(&mut bridge, &mut out);
  assert!(file > 1);
  write(&mut bridge, file, fh, data, &mut out);

  dispatch(
    &message(Opcode::Lookup.to_wire(), 3, 1, &name_body("build.rs")),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out));
  assert_eq!(reply_nodeid(&out), file);

  assert_eq!(getattr_size(&mut bridge, file, &mut out), data.len() as u64);
  assert_eq!(open_and_read(&mut bridge, file, 64, &mut out), data);

  let mut rd = vec![0u8; 24];
  rd[16..20].copy_from_slice(&4096u32.to_le_bytes());
  let n = dispatch(
    &message(Opcode::ReadDir.to_wire(), 7, 1, &rd),
    &mut bridge,
    &mut out,
  );
  let entries = &out[OUT_HEADER_LEN..n];
  assert!(
    entries.windows(b"build.rs".len()).any(|w| w == b"build.rs"),
    "the directory lists the created file"
  );
}

/// A lookup of a name that does not exist is ENOENT; a read on a stale handle is EINVAL.
#[test]
fn missing_names_and_stale_handles_are_typed_errnos() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let mut out = vec![0u8; 4096];
  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 1, 1, &name_body("nope")),
    &mut bridge,
    &mut out,
  );
  assert_eq!(n, OUT_HEADER_LEN);
  assert_eq!(
    i32::from_le_bytes(out[4..8].try_into().unwrap()),
    -2,
    "ENOENT"
  );

  let mut r = vec![0u8; 24];
  r[0..8].copy_from_slice(&999u64.to_le_bytes()); // a handle never opened
  r[16..20].copy_from_slice(&16u32.to_le_bytes());
  dispatch(
    &message(Opcode::Read.to_wire(), 2, 2, &r),
    &mut bridge,
    &mut out,
  );
  assert_eq!(
    i32::from_le_bytes(out[4..8].try_into().unwrap()),
    -22,
    "EINVAL"
  );
}
