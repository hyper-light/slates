//! The shards virtio-fs backend (`slates_bridge_fuse::shards_fs`) serves a slates volume to a `shards` microVM over
//! shards' own socket framing (a little-endian u32 length, then the FUSE bytes), so a shards guest's virtio-fs
//! device — pointed at this socket — mounts a slates volume with no kernel NFS or 9p (§4.6; `docs/wip/shards/`). The
//! non-DAX path: shards' frame carries a FUSE request, not a `FUSE_SETUPMAPPING`.
//!
//! The backend is transport-agnostic (generic over `Read + Write`), so these tests prove it two ways. The full
//! guest mount+read (`INIT`→`LOOKUP`→`OPEN`→`READ`) is driven over an **in-memory duplex** — no socket, no OS types —
//! so it compiles and runs on **every platform and architecture** (Linux, Windows, macOS; amd64, arm64), the CI
//! matrix included. A second test, **Unix-only**, drives it over a real `UnixStream::pair` to prove the socket path
//! shards actually connects (and that a closed socket ends serving at EOF, not an error). Windows reaches the same
//! generic backend over a named pipe / `AF_UNIX`, which the in-memory test already exercises the logic of.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
#[cfg(unix)]
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
#[cfg(unix)]
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

/// A single-threaded in-memory duplex: one `Read + Write` the backend serves on, with no OS socket — so the full
/// mount+read runs identically on every platform and architecture. `serve_one` reads the next framed request from
/// `inbox` (advancing `pos`) and appends the framed reply to `outbox`; `exchange` pushes a request and takes its reply.
#[derive(Default)]
struct MemDuplex {
  inbox: Vec<u8>,
  pos: usize,
  outbox: Vec<u8>,
  taken: usize,
}

impl Read for MemDuplex {
  fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
    let avail = self.inbox.get(self.pos..).unwrap_or_default();
    let n = avail.len().min(buf.len());
    buf.get_mut(..n).unwrap_or_default().copy_from_slice(avail.get(..n).unwrap_or_default());
    self.pos += n;
    Ok(n)
  }
}

impl Write for MemDuplex {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    self.outbox.extend_from_slice(buf);
    Ok(buf.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

/// `request` framed as shards frames it (le32 length + bytes).
fn framed(request: &[u8]) -> Vec<u8> {
  let mut v = Vec::new();
  write_shards_frame(&mut v, request).unwrap();
  v
}

/// Pushes a framed request, serves one, and returns the reply's bytes (unframed), over the in-memory duplex.
fn exchange(duplex: &mut MemDuplex, bridge: &mut VolumeBridge<'_>, cx: &OpContext, request: &[u8]) -> Vec<u8> {
  duplex.inbox.extend_from_slice(&framed(request));
  let mut reply = vec![0u8; 1 << 16];
  assert!(serve_one(duplex, bridge, cx, &mut reply).unwrap(), "the framed request was served");
  // serve_one appended one reply frame (le32 len + bytes); take it from where the previous reply ended.
  let frame = duplex.outbox.get(duplex.taken..).unwrap_or_default();
  let len = u32::from_le_bytes(frame.get(0..4).unwrap_or(&[0; 4]).try_into().unwrap_or([0; 4])) as usize;
  let reply = frame.get(4..4 + len).unwrap_or_default().to_vec();
  duplex.taken += 4 + len;
  reply
}

/// §4.6 (the shards integration — the guest's own mount+read sequence, proven on every platform/arch, no socket).
/// Do: put a one-file volume behind the slates `shards_fs` backend, then drive exactly what a guest kernel's
/// virtio-fs client does to read a file — FUSE_INIT, LOOKUP of the name under the root, OPEN of the returned inode,
/// READ of its bytes — each framed as shards' fs device frames it, over an in-memory duplex (no OS socket). Expect:
/// every step is served with errno 0, and the READ returns the file's own bytes. So a shards microVM mounting tag
/// `shards0` and reading `/hello` reaches slates through the one FUSE bridge, the same on Linux, Windows and macOS
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
  let mut duplex = MemDuplex::default();

  // FUSE_INIT: major 7, minor 38, max_readahead 0, flags 0 — the codec reads what it needs.
  let mut init = Vec::new();
  init.extend_from_slice(&7u32.to_le_bytes());
  init.extend_from_slice(&38u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.resize(64, 0);
  let r = exchange(&mut duplex, &mut bridge, &cx, &message(Opcode::Init as u32, 1, 0, &init));
  assert_eq!(errno(&r), 0, "INIT served");

  // LOOKUP "hello" under the root (nodeid 1): the reply's body begins with the file's inode number.
  let r = exchange(&mut duplex, &mut bridge, &cx, &message(Opcode::Lookup as u32, 2, 1, b"hello\0"));
  assert_eq!(errno(&r), 0, "LOOKUP of hello served");
  let nodeid = body_u64(&r);
  assert_ne!(nodeid, 0, "the lookup resolved the file to a nodeid");

  // OPEN that inode (fuse_open_in: flags u32, then 4 padding): the reply's body begins with the file handle.
  let mut open_in = 0u32.to_le_bytes().to_vec();
  open_in.extend_from_slice(&[0u8; 4]);
  let r = exchange(&mut duplex, &mut bridge, &cx, &message(Opcode::Open as u32, 3, nodeid, &open_in));
  assert_eq!(errno(&r), 0, "OPEN served");
  let fh = body_u64(&r);

  // READ the whole file (fuse_read_in: fh u64, offset u64, size u32, …).
  let mut read_in = vec![0u8; 24];
  read_in[0..8].copy_from_slice(&fh.to_le_bytes());
  read_in[8..16].copy_from_slice(&0u64.to_le_bytes());
  read_in[16..20].copy_from_slice(&u32::try_from(HELLO.len()).unwrap().to_le_bytes());
  let r = exchange(&mut duplex, &mut bridge, &cx, &message(Opcode::Read as u32, 4, nodeid, &read_in));
  assert_eq!(errno(&r), 0, "READ served");
  assert_eq!(
    &r[OUT_HEADER_LEN..],
    HELLO,
    "the guest reads the slates file's own bytes through the shards fs backend"
  );
}
