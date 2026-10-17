//! Profiling pass on the mediated (non-DAX) shards fs path (§4.6). Measures how long `shards_fs::serve_one` takes to
//! read a framed FUSE request, dispatch it through the slates bridge, and write the framed reply — over an in-memory
//! duplex (no socket), so the number is the framing + dispatch, not the OS. This is a shards microVM's non-DAX FS
//! latency floor (DAX, once a shards cache window exists, replaces the per-op round-trip with a page-table access —
//! `crates/vfs/examples/dax_bench.rs` measured that at tens of ns). Best-of-N per op.
//!
//! Run (release, quiesced): `cargo run -p slates-bridge-fuse --example shards_fs_bench --release`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use slates_bridge_core::{Attachments, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::shards_fs::{serve_one, write_shards_frame};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
/// Best-of-N iterations per op — the minimum is the least-perturbed sample (the machine's floor).
const N: usize = 20_000;
/// The file the READ bench reads: one page, so READ moves a page per call.
const FILE_BYTES: usize = 4096;

/// A reusable in-memory duplex (see `tests/shards_fs.rs`): `serve_one` reads the request from `inbox` and appends the
/// reply to `outbox`; `load` resets it with the next framed request so each timed call starts clean.
#[derive(Default)]
struct MemDuplex {
  inbox: Vec<u8>,
  pos: usize,
  outbox: Vec<u8>,
}

impl MemDuplex {
  fn load(&mut self, framed_request: &[u8]) {
    self.inbox.clear();
    self.inbox.extend_from_slice(framed_request);
    self.pos = 0;
    self.outbox.clear();
  }
}

impl Read for MemDuplex {
  fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
    let avail = self.inbox.get(self.pos..).unwrap_or_default();
    let n = avail.len().min(buf.len());
    buf
      .get_mut(..n)
      .unwrap_or_default()
      .copy_from_slice(avail.get(..n).unwrap_or_default());
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

fn framed(request: &[u8]) -> Vec<u8> {
  let mut v = Vec::new();
  write_shards_frame(&mut v, request).unwrap();
  v
}

/// Serves `request` once and returns the reply's bytes (unframed).
fn once(
  duplex: &mut MemDuplex,
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  framed_request: &[u8],
  reply: &mut [u8],
) -> Vec<u8> {
  duplex.load(framed_request);
  serve_one(duplex, bridge, cx, reply).unwrap();
  let frame = &duplex.outbox;
  let len = u32::from_le_bytes(
    frame
      .get(0..4)
      .unwrap_or(&[0; 4])
      .try_into()
      .unwrap_or([0; 4]),
  ) as usize;
  frame.get(4..4 + len).unwrap_or_default().to_vec()
}

/// Best-of-N wall-clock of serving one framed request.
fn best(
  duplex: &mut MemDuplex,
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  framed_request: &[u8],
  reply: &mut [u8],
) -> Duration {
  let mut best = Duration::MAX;
  for _ in 0..N {
    duplex.load(framed_request);
    let start = Instant::now();
    serve_one(duplex, bridge, cx, reply).unwrap();
    best = best.min(start.elapsed());
  }
  best
}

fn main() {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  let mut store = Store::new(
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
  );
  let vid = VolumeId { bytes: [0; 16] };
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
  let root = vol.root_inode(&store).unwrap();
  let hello = vol
    .create_file_no(&mut store, root, "hello", 0o644)
    .unwrap();
  vol
    .write(&mut store, hello, 0, &vec![0xC7u8; FILE_BYTES])
    .unwrap();
  let mut bridge = VolumeBridge::new(vid, &mut vol, &mut store);

  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      vid,
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  let cx: OpContext = attachments.context(id).unwrap();

  let mut reply = vec![0u8; (1 << 20) + (1 << 16)];

  // INIT once (negotiate), then LOOKUP + OPEN to get a nodeid and a file handle for the READ bench.
  let mut init = Vec::new();
  for v in [7u32, 38, 0, 0] {
    init.extend_from_slice(&v.to_le_bytes());
  }
  init.resize(64, 0);

  let mut duplex = MemDuplex::default();
  let _ = once(
    &mut duplex,
    &mut bridge,
    &cx,
    &framed(&message(Opcode::Init as u32, 1, 0, &init)),
    &mut reply,
  );
  let look = once(
    &mut duplex,
    &mut bridge,
    &cx,
    &framed(&message(Opcode::Lookup as u32, 2, 1, b"hello\0")),
    &mut reply,
  );
  let nodeid = u64::from_le_bytes(look[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap());
  let mut open_in = 0u32.to_le_bytes().to_vec();
  open_in.extend_from_slice(&[0u8; 4]);
  let opened = once(
    &mut duplex,
    &mut bridge,
    &cx,
    &framed(&message(Opcode::Open as u32, 3, nodeid, &open_in)),
    &mut reply,
  );
  let fh = u64::from_le_bytes(
    opened[OUT_HEADER_LEN..OUT_HEADER_LEN + 8]
      .try_into()
      .unwrap(),
  );

  let getattr = framed(&message(Opcode::GetAttr as u32, 10, 1, &[0u8; 16]));
  let lookup = framed(&message(Opcode::Lookup as u32, 11, 1, b"hello\0"));
  let mut read_in = vec![0u8; 24];
  read_in[0..8].copy_from_slice(&fh.to_le_bytes());
  read_in[16..20].copy_from_slice(&u32::try_from(FILE_BYTES).unwrap().to_le_bytes());
  let read = framed(&message(Opcode::Read as u32, 12, nodeid, &read_in));

  println!(
    "shards_fs mediated serve latency (best of {N}, in-memory duplex; framing + dispatch, no OS I/O):"
  );
  println!(
    "  GETATTR (root)      {:?}",
    best(&mut duplex, &mut bridge, &cx, &getattr, &mut reply)
  );
  println!(
    "  LOOKUP  (hello)     {:?}",
    best(&mut duplex, &mut bridge, &cx, &lookup, &mut reply)
  );
  println!(
    "  READ    ({FILE_BYTES} B)      {:?}",
    best(&mut duplex, &mut bridge, &cx, &read, &mut reply)
  );
}
