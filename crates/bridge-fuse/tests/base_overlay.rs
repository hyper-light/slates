//! Base files through the bridge (Phase 3 task 8; §4.6 "Base files", AC-3.9): an overlay
//! volume over a real read-only directory, served through the VolumeBridge with its host, so a
//! LOOKUP and a READ of an untouched base file go to the disk and come back byte-identical.
//! This overlays this crate's own `src` (read only), so it runs on any Unix host without a
//! mount and writes nothing.
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use slates_base::OsHost;
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::dispatch;
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::HostClock;
use slates_vfs::host::HostFs;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;
const REGION_PAGES: usize = 8192;

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 256,
      max_inodes: 1024,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 256,
      dir_cutover: 16,
    },
    arena,
  )
}

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

fn name_body(name: &str) -> Vec<u8> {
  let mut b = name.as_bytes().to_vec();
  b.push(0);
  b
}

fn ok(out: &[u8]) -> bool {
  u32::from_le_bytes(out[4..8].try_into().unwrap()) == 0
}

/// An overlay over this crate's `src` serves `lib.rs` through the bridge: LOOKUP finds it and
/// READ returns its bytes, matching the file read directly from disk (AC-3.9).
#[test]
fn a_base_file_is_read_through_the_bridge_byte_identical() {
  let base = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
  let (mut host, root) = OsHost::open_root(Path::new(base)).unwrap();
  let facts = host.facts(root).unwrap();
  let mut store = store();
  let mut vol = Volume::create_overlay(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: 1 << 20,
    },
  )
  .unwrap();
  let mut bridge = VolumeBridge::with_base(&mut vol, &mut store, host);
  let mut out = vec![0u8; 1 << 20];

  // READDIR the root loads the base listing (a tool lists a directory before opening its
  // files); the entries include the base files.
  let mut rd = vec![0u8; 24];
  rd[16..20].copy_from_slice(&(1u32 << 16).to_le_bytes());
  let n = dispatch(
    &message(Opcode::ReadDir.to_wire(), 0, 1, &rd),
    &mut bridge,
    &mut out,
  );
  let entries = &out[OUT_HEADER_LEN..n];
  assert!(
    entries.windows(b"lib.rs".len()).any(|w| w == b"lib.rs"),
    "the base directory lists lib.rs through the bridge"
  );

  // LOOKUP lib.rs (a base file, never touched in the overlay).
  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 1, 1, &name_body("lib.rs")),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "the base file is found through the bridge");
  assert!(n >= OUT_HEADER_LEN + EntryOut::LEN);
  let nodeid = u64::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap());
  // the size sits in fuse_attr after the 40-byte entry prefix: ino (8) then size (8).
  let size_at = OUT_HEADER_LEN + 40 + 8;
  let size = u64::from_le_bytes(out[size_at..size_at + 8].try_into().unwrap());

  // OPEN then READ the whole file.
  let mut open_body = vec![0u8; 8];
  open_body[0..4].copy_from_slice(&0u32.to_le_bytes());
  dispatch(
    &message(Opcode::Open.to_wire(), 2, nodeid, &open_body),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "open ok");
  let fh = u64::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap());
  let mut r = vec![0u8; 24];
  r[0..8].copy_from_slice(&fh.to_le_bytes());
  r[16..20].copy_from_slice(&u32::try_from(size).unwrap().to_le_bytes());
  let n = dispatch(
    &message(Opcode::Read.to_wire(), 3, nodeid, &r),
    &mut bridge,
    &mut out,
  );
  let read = &out[OUT_HEADER_LEN..n];

  // Byte-identical to the file read straight from disk.
  #[allow(clippy::disallowed_methods)]
  let on_disk = std::fs::read(format!("{base}/lib.rs")).unwrap();
  assert_eq!(read, on_disk.as_slice(), "the base read matches the disk");
}
