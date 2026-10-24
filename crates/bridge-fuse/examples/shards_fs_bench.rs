//! Profiling pass on the mediated (non-DAX) Shards fs path (§4.6, A-161). Measures the daemon's work for one frame:
//! [`split_frame`] taking a framed FUSE request from the bytes read, then [`answer`] dispatching it through the slates
//! bridge and writing the reply frame — over bytes in memory, no socket, so the number is the framing, the dispatch and
//! the VFS, not the OS (the stream's reads and writes are the daemon's serve task's). This is a Shards microVM's non-DAX
//! latency floor; DAX, once a window over an attachment's own backing exists, replaces the per-op round trip with a
//! page-table access (`crates/vfs/examples/dax_bench.rs` measured that at tens of ns). Best-of-N per op.
//!
//! Until 2026-10-24 this measured the blocking `serve_one`, since removed, which dispatched without `answer`'s read
//! ceiling and reply framing; the 2026-10-18 rows in `docs/wip/BENCHMARKS.md` were taken that way.
//!
//! Run (release, quiesced): `cargo run -p slates-bridge-fuse --example shards_fs_bench --release`

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::panic
)]

use std::time::{Duration, Instant};

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
const REGION_PAGES: usize = 4096;
/// Best-of-N iterations per op — the minimum is the least-perturbed sample (the machine's floor).
const N: usize = 20_000;
/// The file the READ bench reads: one page, so READ moves a page per call.
const FILE_BYTES: usize = 4096;
/// How many files the CREATE row makes, one per timed call — the same count `docs/wip/bench/fuse/creates.py`
/// drives through a real mount, so the two are comparable: each create lands in a directory that already holds
/// every earlier one, as a real workload's do.
const CREATES: usize = 2_000;
/// Format: `fuse_create_in`'s open flags — `O_WRONLY | O_CREAT | O_TRUNC` as Linux numbers them, what
/// `open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)` sends.
const CREATE_FLAGS: u32 = 0o1 | 0o100 | 0o1000;

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
  let mut frame = u32::try_from(request.len()).unwrap().to_le_bytes().to_vec();
  frame.extend_from_slice(request);
  frame
}

/// One frame as the daemon serves it from the bytes it read: split, then answered into `frame`, the reply frame at its
/// head. The reply frame's length, its length word included.
fn serve(
  framed_request: &[u8],
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  frame: &mut Vec<u8>,
) -> usize {
  let Ok(Split::Whole(Frame::Request(request), _)) = split_frame(framed_request, FRAME_BYTES)
  else {
    panic!("a whole request frame");
  };
  answer(request, bridge, cx, frame).frame
}

/// Serves `framed_request` once and returns the reply's bytes, without the length word.
fn once(
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  framed_request: &[u8],
  frame: &mut Vec<u8>,
) -> Vec<u8> {
  let used = serve(framed_request, bridge, cx, frame);
  frame[LENGTH_BYTES..used].to_vec()
}

/// Best-of-N wall-clock of serving one framed request.
fn best(
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  framed_request: &[u8],
  frame: &mut Vec<u8>,
) -> Duration {
  let mut best = Duration::MAX;
  for _ in 0..N {
    let start = Instant::now();
    serve(framed_request, bridge, cx, frame);
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

  let mut frame = Vec::new();

  // INIT once (negotiate), then LOOKUP + OPEN to get a nodeid and a file handle for the READ bench.
  let mut init = Vec::new();
  for v in [7u32, 38, 0, 0] {
    init.extend_from_slice(&v.to_le_bytes());
  }
  init.resize(64, 0);

  let _ = once(
    &mut bridge,
    &cx,
    &framed(&message(Opcode::Init as u32, 1, 0, &init)),
    &mut frame,
  );
  let look = once(
    &mut bridge,
    &cx,
    &framed(&message(Opcode::Lookup as u32, 2, 1, b"hello\0")),
    &mut frame,
  );
  let nodeid = u64::from_le_bytes(look[OUT_HEADER_LEN..OUT_HEADER_LEN + 8].try_into().unwrap());
  let mut open_in = 0u32.to_le_bytes().to_vec();
  open_in.extend_from_slice(&[0u8; 4]);
  let opened = once(
    &mut bridge,
    &cx,
    &framed(&message(Opcode::Open as u32, 3, nodeid, &open_in)),
    &mut frame,
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
    "shards_fs mediated serve latency (best of {N}, split_frame + answer over bytes in memory; no OS I/O):"
  );
  println!(
    "  GETATTR (root)      {:?}",
    best(&mut bridge, &cx, &getattr, &mut frame)
  );
  println!(
    "  LOOKUP  (hello)     {:?}",
    best(&mut bridge, &cx, &lookup, &mut frame)
  );
  println!(
    "  READ    ({FILE_BYTES} B)      {:?}",
    best(&mut bridge, &cx, &read, &mut frame)
  );

  // CREATE is the write path's head and the slowest op a real mount shows (276.9 µs p50 through Linux FUSE,
  // `BENCHMARKS.md` 2026-10-11, against a 3.9 µs in-process VFS create+unlink). Every call needs its own name,
  // so the messages are built first and only the serve is timed; the distribution is reported as p50/p99 rather
  // than best-of-N because that is what `creates.py` reports and the point is to compare them.
  let creates: Vec<Vec<u8>> = (0..CREATES)
    .map(|i| {
      let mut body = Vec::with_capacity(16 + 16);
      body.extend_from_slice(&CREATE_FLAGS.to_le_bytes());
      body.extend_from_slice(&0o644u32.to_le_bytes());
      body.extend_from_slice(&0u32.to_le_bytes());
      body.extend_from_slice(&0u32.to_le_bytes());
      body.extend_from_slice(format!("f{i}\0").as_bytes());
      framed(&message(
        Opcode::Create as u32,
        100 + u64::try_from(i).unwrap_or(0),
        1,
        &body,
      ))
    })
    .collect();
  let mut took: Vec<Duration> = Vec::with_capacity(CREATES);
  for request in &creates {
    let at = Instant::now();
    serve(request, &mut bridge, &cx, &mut frame);
    took.push(at.elapsed());
  }
  took.sort_unstable();
  // Integer quantile index: the last element scaled by num/den, so no float cast rounds the position.
  let at = |num: usize, den: usize| took[took.len().saturating_sub(1).saturating_mul(num) / den];
  println!(
    "  CREATE  ({CREATES} files)  p50 {:?}  p99 {:?}  max {:?}",
    at(1, 2),
    at(99, 100),
    took[took.len().saturating_sub(1)]
  );
}
