//! The VolumeBridge's tests (Phase 3 task 1; §4.6): the whole stack — the codec, the dispatch,
//! the VolumeBridge, and the volume core — driven against an in-memory scratch volume, so a
//! FUSE CREATE/WRITE/LOOKUP/GETATTR/READ/READDIR round trip is exercised on every host without
//! a mount. This is the read-and-write path the `cargo build` workload leans on.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_bridge_core::{Attachments, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};

/// A read-write current-view context, minted through the attachment registry (the only way to
/// build an `OpContext`), so the dispatch call sites below stay unchanged.
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

/// Drives the crate's dispatch with a real read-write context.
fn dispatch(message: &[u8], bridge: &mut VolumeBridge<'_>, out: &mut [u8]) -> usize {
  slates_bridge_fuse::bridge::dispatch(message, bridge, &test_cx(), out)
}
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Format: `ENOENT`.
const ENOENT: i32 = 2;

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

/// Whether a LOOKUP reply says the name is absent. This context revalidates (it has no invalidation channel), so a
/// miss has no lifetime to cache and is `ENOENT`; a negative entry is for a context the kernel can be told about
/// (`dispatch.rs`, and a real kernel in `coherence_mount.rs`), §4.6 "Cache posture".
fn absent(out: &[u8]) -> bool {
  i32::from_le_bytes(out[4..8].try_into().unwrap()) == -ENOENT
}

/// The node id a CREATE or LOOKUP reply names (the start of `fuse_entry_out`).
fn reply_nodeid(out: &[u8]) -> u64 {
  u64::from_le_bytes(out[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap())
}

/// AC-3.10 / A-26: Linux MKNOD vectors create real FIFO/socket names and reject a device.
#[test]
fn mknod_reports_ipc_types_and_refuses_device_nodes() {
  let mut store = store();
  let mut volume = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut volume, &mut store);
  let mut out = vec![0; OUT_HEADER_LEN + EntryOut::LEN];
  for (name, mode) in [("pipe", 0o010640u32), ("socket", 0o140640u32)] {
    // Linux fuse_mknod_in: mode, rdev, umask, padding; followed by the name.
    let mut body = mode.to_le_bytes().to_vec();
    body.extend_from_slice(&[0; 12]);
    body.extend_from_slice(&name_body(name));
    dispatch(&message(8, 1, 1, &body), &mut bridge, &mut out);
    assert!(ok(&out), "{name} creation");
    // fuse_entry_out has a 40-byte prefix; fuse_attr.mode is at byte 60.
    let offset = OUT_HEADER_LEN + 40 + 60;
    assert_eq!(
      u32::from_le_bytes(out[offset..offset + 4].try_into().unwrap()),
      mode
    );
    dispatch(&message(1, 2, 1, &name_body(name)), &mut bridge, &mut out);
    assert!(ok(&out), "{name} lookup");
    assert_eq!(
      u32::from_le_bytes(out[offset..offset + 4].try_into().unwrap()),
      mode
    );
  }
  let mut body = 0o060600u32.to_le_bytes().to_vec();
  body.extend_from_slice(&[0; 12]);
  body.extend_from_slice(&name_body("device"));
  dispatch(&message(8, 3, 1, &body), &mut bridge, &mut out);
  assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), -95);
  dispatch(
    &message(1, 4, 1, &name_body("device")),
    &mut bridge,
    &mut out,
  );
  assert!(absent(&out), "the refused device node was never made");
}

/// T-3.1 / A-26: truncated MKNOD bodies cannot create a name, including a missing NUL terminator.
#[test]
fn truncated_mknod_never_changes_the_namespace() {
  let mut store = store();
  let mut volume = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut volume, &mut store);
  let mut body = 0o010600u32.to_le_bytes().to_vec();
  body.extend_from_slice(&[0; 12]);
  body.extend_from_slice(&name_body("pipe"));
  let mut out = vec![0; OUT_HEADER_LEN + EntryOut::LEN];
  for end in 0..body.len() {
    dispatch(&message(8, 1, 1, &body[..end]), &mut bridge, &mut out);
    assert!(!ok(&out), "truncation at {end}");
    dispatch(&message(1, 2, 1, &name_body("pipe")), &mut bridge, &mut out);
    assert!(absent(&out), "truncation at {end} made no name");
  }
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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
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

/// A lookup of a name that does not exist is ENOENT; a read of an inode the volume does not have
/// is ENOENT — reads are addressed by inode now, not an open handle, so a handle value is never
/// consulted for the read.
#[test]
fn missing_names_and_absent_inodes_are_typed_errnos() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let mut out = vec![0u8; 4096];
  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 1, 1, &name_body("nope")),
    &mut bridge,
    &mut out,
  );
  assert_eq!(n, OUT_HEADER_LEN, "an error reply: nothing to cache");
  assert!(
    absent(&out),
    "a missing name is ENOENT in a revalidating context"
  );

  let mut r = vec![0u8; 24];
  r[16..20].copy_from_slice(&16u32.to_le_bytes()); // size; the handle word is unused for a read
  dispatch(
    &message(Opcode::Read.to_wire(), 2, 2, &r),
    &mut bridge,
    &mut out,
  );
  assert_eq!(
    i32::from_le_bytes(out[4..8].try_into().unwrap()),
    -2,
    "ENOENT: the volume has no such inode"
  );
}

/// pjdfstest `*/02.t` (POSIX.1-2017 §2.3 "ENAMETOOLONG"). Do: send a name one byte past `NAME_MAX` to a lookup,
/// a mkdir, a create and an unlink, and a `NAME_MAX` name to a mkdir. Expect: each over-long name is refused
/// `ENAMETOOLONG` (-36), never `EINVAL` or `ENOENT`, and nothing is made; the `NAME_MAX` name is made. The Linux
/// kernel's FUSE client sends a name up to its own 1,024-byte bound, so the server must judge 256..=1,024
/// (before 2026-10-01 this bridge answered `EINVAL` for a creation and `ENOENT` for a lookup; found by
/// pjdfstest through the Linux container lane).
#[test]
fn a_name_past_name_max_is_refused_enametoolong_on_every_path() {
  const ENAMETOOLONG: i32 = -36;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let mut out = vec![0u8; 4096];
  let longest = "x".repeat(slates_vfs::names::NAME_MAX);
  let past = "y".repeat(slates_vfs::names::NAME_MAX + 1);
  let error = |out: &[u8]| i32::from_le_bytes(out[4..8].try_into().unwrap());
  let calls: [(&str, u32, Vec<u8>); 4] = [
    ("lookup", Opcode::Lookup.to_wire(), name_body(&past)),
    (
      "mkdir",
      Opcode::MkDir.to_wire(),
      mkdir_body(0o040_755, &past),
    ),
    (
      "create",
      Opcode::Create.to_wire(),
      create_body(0o100_644, &past),
    ),
    ("unlink", Opcode::Unlink.to_wire(), name_body(&past)),
  ];
  for (unique, (call, opcode, body)) in (1u64..).zip(calls) {
    dispatch(&message(opcode, unique, 1, &body), &mut bridge, &mut out);
    assert_eq!(
      error(&out),
      ENAMETOOLONG,
      "{call} of a {}-byte name",
      past.len()
    );
  }
  dispatch(
    &message(Opcode::Lookup.to_wire(), 10, 1, &name_body(&past)),
    &mut bridge,
    &mut out,
  );
  assert_eq!(
    error(&out),
    ENAMETOOLONG,
    "nothing was made under the long name"
  );
  dispatch(
    &message(
      Opcode::MkDir.to_wire(),
      11,
      1,
      &mkdir_body(0o040_755, &longest),
    ),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "a NAME_MAX name is made");
}

/// A `fuse_mkdir_in` body: mode, umask, then the name.
fn mkdir_body(mode: u32, name: &str) -> Vec<u8> {
  let mut b = vec![0u8; 8];
  b[0..4].copy_from_slice(&mode.to_le_bytes());
  b.extend_from_slice(&name_body(name));
  b
}

/// A `fuse_rename_in` body: newdir, then oldname NUL newname NUL.
fn rename_body(newdir: u64, old: &str, new: &str) -> Vec<u8> {
  let mut b = newdir.to_le_bytes().to_vec();
  b.extend_from_slice(&name_body(old));
  b.extend_from_slice(&name_body(new));
  b
}

/// A `fuse_setattr_in` body setting the size: valid (FATTR_SIZE), padding, fh, size, then the
/// rest zero.
fn setattr_size_body(size: u64) -> Vec<u8> {
  let mut b = vec![0u8; 88];
  b[0..4].copy_from_slice(&(1u32 << 3).to_le_bytes()); // FATTR_SIZE
  b[16..24].copy_from_slice(&size.to_le_bytes()); // size (after valid, padding, fh)
  b
}

/// mkdir then create a file inside it, then rmdir refuses (not empty), unlink the file, rmdir.
#[test]
fn mkdir_unlink_and_rmdir_dispatch_to_the_volume() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let mut out = vec![0u8; 1 << 16];

  dispatch(
    &message(Opcode::MkDir.to_wire(), 1, 1, &mkdir_body(0o040_755, "sub")),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "mkdir ok");
  let sub = reply_nodeid(&out);

  // create a file in the subdirectory.
  dispatch(
    &message(
      Opcode::Create.to_wire(),
      2,
      sub,
      &create_body(0o100_644, "f"),
    ),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "create in sub ok");

  // rmdir the non-empty subdirectory is refused.
  dispatch(
    &message(Opcode::RmDir.to_wire(), 3, 1, &name_body("sub")),
    &mut bridge,
    &mut out,
  );
  assert_eq!(
    i32::from_le_bytes(out[4..8].try_into().unwrap()),
    -39,
    "ENOTEMPTY"
  );

  // unlink the file, then rmdir succeeds.
  dispatch(
    &message(Opcode::Unlink.to_wire(), 4, sub, &name_body("f")),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "unlink ok");
  dispatch(
    &message(Opcode::RmDir.to_wire(), 5, 1, &name_body("sub")),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "rmdir ok");
}

/// rename moves a file; setattr truncates it; statfs answers.
#[test]
fn rename_setattr_and_statfs_dispatch_to_the_volume() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let mut out = vec![0u8; 1 << 16];

  let (file, fh) = create(&mut bridge, &mut out);
  write(&mut bridge, file, fh, b"0123456789", &mut out);

  // rename build.rs -> main.rs within the root.
  dispatch(
    &message(
      Opcode::Rename.to_wire(),
      1,
      1,
      &rename_body(1, "build.rs", "main.rs"),
    ),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "rename ok");
  dispatch(
    &message(Opcode::Lookup.to_wire(), 2, 1, &name_body("main.rs")),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "the renamed name resolves");
  assert_eq!(reply_nodeid(&out), file);

  // setattr truncates to 4 bytes.
  dispatch(
    &message(Opcode::SetAttr.to_wire(), 3, file, &setattr_size_body(4)),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out), "setattr ok");
  assert_eq!(getattr_size(&mut bridge, file, &mut out), 4);

  // statfs answers with a page block size.
  let n = dispatch(
    &message(Opcode::StatFs.to_wire(), 4, 1, &[]),
    &mut bridge,
    &mut out,
  );
  assert!(ok(&out) && n > OUT_HEADER_LEN, "statfs ok");
}

/// A READDIR body (`fuse_read_in`): fh, the resume offset, the size, then fields the dispatch skips.
fn readdir_body(offset: u64, size: u32) -> Vec<u8> {
  let mut b = vec![0u8; 40];
  b[8..16].copy_from_slice(&offset.to_le_bytes());
  b[16..20].copy_from_slice(&size.to_le_bytes());
  b
}

/// The `(off, name)` of each `fuse_dirent` in a READDIR reply of `n` bytes.
fn dirents(out: &[u8], n: usize) -> Vec<(u64, String)> {
  /// Format: `fuse_dirent`'s head (ino, off, namelen, type) and its alignment.
  const HEAD: usize = 24;
  const ALIGN: usize = 8;
  let mut at = OUT_HEADER_LEN;
  let mut found = Vec::new();
  while at + HEAD <= n {
    let off = u64::from_le_bytes(out[at + 8..at + 16].try_into().unwrap());
    let len = usize::try_from(u32::from_le_bytes(
      out[at + 16..at + 20].try_into().unwrap(),
    ))
    .unwrap();
    let name = String::from_utf8(out[at + HEAD..at + HEAD + len].to_vec()).unwrap();
    found.push((off, name));
    at += (HEAD + len).next_multiple_of(ALIGN);
  }
  found
}

/// Shape: the files of the paged directory and a READDIR size that holds four of their entries.
const LISTED_FILES: usize = 40;
const SMALL_READDIR: u32 = 128;

/// AUD-29-86. Do: through the dispatch, page a 40-file root with READDIRs of 128 bytes, each resuming from
/// the `off` of the last entry the previous reply carried, until a reply carries none. Expect: `.`, `..` and
/// every file exactly once — the kernel's resume offsets are the entries' cookies, and a page holds what fits.
#[test]
fn readdir_pages_resume_from_the_cookies_the_kernel_passes_back() {
  use slates_bridge_core::Bridge;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = test_cx();
  let root = bridge.root(&cx).unwrap();
  for n in 0..LISTED_FILES {
    bridge
      .create(
        slates_bridge_core::ObjectId::new(root, 0),
        &cx,
        &format!("file-{n:02}"),
        0o644,
        0,
      )
      .unwrap();
  }
  let mut out = vec![0u8; 4096];
  let mut offset = 0;
  let mut names = Vec::new();
  for unique in 1.. {
    let n = dispatch(
      &message(
        Opcode::ReadDir.to_wire(),
        unique,
        1,
        &readdir_body(offset, SMALL_READDIR),
      ),
      &mut bridge,
      &mut out,
    );
    assert!(ok(&out), "READDIR answered an error");
    let page = dirents(&out, n);
    let Some((last, _)) = page.last() else {
      break;
    };
    offset = *last;
    names.extend(page.into_iter().map(|(_, name)| name));
  }
  let unique: std::collections::BTreeSet<&String> = names.iter().collect();
  assert_eq!(names.len(), LISTED_FILES + 2, "no entry repeated");
  assert_eq!(unique.len(), LISTED_FILES + 2, "every entry listed");
}

/// Format: `EOVERFLOW` (Linux).
const EOVERFLOW: i32 = 75;

/// AUD-29-86. Do: create two names whose cookies are equal (found by search over the volume's name hash)
/// among 20 others, then READDIR from just before them with room for one of their entries. Expect:
/// `EOVERFLOW` — the page cannot hold the group a resume could not split, so it refuses rather than return
/// one and skip the other.
#[test]
fn a_page_too_small_for_a_shared_cookie_answers_eoverflow() {
  use slates_bridge_core::Bridge;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = test_cx();
  let root = bridge.root(&cx).unwrap();
  let root_id = slates_bridge_core::ObjectId::new(root, 0);
  let mut by_cookie = std::collections::HashMap::new();
  let (first, second) = (0u64..)
    .find_map(|n| {
      let name = format!("c{n}");
      let cookie = slates_vfs::dir_cookie(NameEquivalence::Exact.hash(&name));
      by_cookie
        .insert(cookie, name.clone())
        .map(|first| (first, name))
    })
    .unwrap();
  for name in [first.clone(), second.clone()]
    .into_iter()
    .chain((0..20).map(|n| format!("fill-{n}")))
  {
    bridge.create(root_id, &cx, &name, 0o644, 0).unwrap();
  }
  let whole = bridge.readdir(root_id, &cx, 0, 0, usize::MAX).unwrap();
  let at = whole
    .iter()
    .position(|e| e.name == first || e.name == second)
    .unwrap();
  let before = whole[at - 1].cookie;
  let one_entry = u32::try_from(slates_bridge_fuse::reply::DirBuffer::dirent_len(
    first.len().max(second.len()),
  ))
  .unwrap();
  let mut out = vec![0u8; 4096];
  let n = dispatch(
    &message(
      Opcode::ReadDir.to_wire(),
      1,
      1,
      &readdir_body(before, one_entry),
    ),
    &mut bridge,
    &mut out,
  );
  assert_eq!(n, OUT_HEADER_LEN);
  assert_eq!(
    i32::from_le_bytes(out[4..8].try_into().unwrap()),
    -EOVERFLOW
  );
}
