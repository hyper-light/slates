//! What one NFSv4 `WRITE` and one `READ` of a full transfer cost the front end in-process (§4.6 A-35/A-36): a session,
//! one file opened, then [`ROUNDS`] rounds of [`OPS`] compounds each of `SEQUENCE, PUTFH, WRITE` and of
//! `SEQUENCE, PUTFH, READ`, served by `serve_compound` over a real `Export` of a scratch volume. It reports the best
//! round's time per operation and the heap bytes allocated per operation (a counting allocator), which a shared
//! machine's scheduling cannot move: every copy of the payload into a fresh buffer shows as allocated bytes.
//!
//! `cargo run --release -p slates-bridge-nfs --example v4_io_bench` prints one CSV row per operation.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use slates_bridge_core::{Rights, VolumeBridge, volume_bridge::new_handle_store};
use slates_bridge_nfs::Export;
use slates_bridge_nfs::v4::backend::serve_compound;
use slates_bridge_nfs::v4::compound::{COMPOUND_HEADER_BYTES, MIN_OPERATION_BYTES, Server, op};
use slates_bridge_nfs::v4::session::Limits;
use slates_bridge_nfs::v4::types::ChannelAttrs;
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Counts every byte the process allocates (a reallocation counts its new size).
struct Counting;

static ALLOCATED: AtomicU64 = AtomicU64::new(0);

thread_local! {
  /// Whether this thread is inside a trace (an allocation the trace itself makes is not traced).
  static TRACING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Shape: an allocation at least this large is a payload copy, traced when `V4_IO_TRACE` is set.
const TRACE_FROM: usize = 200_000;

static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Prints where a payload-sized allocation came from, once per distinct call stack, when asked.
fn trace(size: usize) {
  if size < TRACE_FROM || !*TRACE.get_or_init(|| std::env::var_os("V4_IO_TRACE").is_some()) {
    return;
  }
  if TRACING.with(|flag| flag.replace(true)) {
    return;
  }
  let frames = std::backtrace::Backtrace::force_capture().to_string();
  let ours: Vec<&str> = frames
    .lines()
    .filter(|line| line.contains("slates_") && !line.contains("v4_io_bench"))
    .take(6)
    .collect();
  eprintln!("ALLOC {size}: {}", ours.join(" <- ").replace("  ", ""));
  TRACING.with(|flag| flag.set(false));
}

// SAFETY: forwards every call to the system allocator unchanged, adding only a relaxed count.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
    trace(layout.size());
    // SAFETY: the caller's layout contract is passed through.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: as above.
    unsafe { System.dealloc(ptr, layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOCATED.fetch_add(new_size as u64, Ordering::Relaxed);
    trace(new_size);
    // SAFETY: as above.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Format: the scratch volume's id.
const VOLUME: VolumeId = VolumeId { bytes: [0x11; 16] };
/// Format: the page size.
const PAGE: usize = 4096;
/// Shape: the arena's pages (room for the file and its rewrites).
const REGION_PAGES: usize = 1 << 16;
/// Shape: the transfer each operation moves: the server's full transfer size.
const TRANSFER: usize = slates_bridge_nfs::procedures::MAX_TRANSFER as usize; // a u32 constant: widening
/// Shape: the windows the file spans, which the writes cycle over.
const FILE_TRANSFERS: u64 = 64;
/// Shape: operations per round, and rounds (best reported, every round shown).
const OPS: usize = 2_000;
const ROUNDS: usize = 5;

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

fn frame(count: u32) -> XdrWriter {
  let mut args = XdrWriter::new();
  args.opaque(b"");
  args.u32(1);
  args.u32(count);
  args
}

fn call(service: &mut Export<'_>, server: &mut Server, args: &[u8]) -> Vec<u8> {
  serve_compound(service, server, 0, 0, (args, args.len())).expect("one poll")
}

/// The results after a reply's frame.
fn results(bytes: &[u8]) -> XdrReader<'_> {
  let mut reader = XdrReader::new(bytes);
  assert_eq!(reader.u32().unwrap(), 0, "the compound succeeds");
  reader.opaque(1024).unwrap();
  reader.u32().unwrap();
  reader
}

fn ok(reader: &mut XdrReader<'_>, opnum: u32) {
  assert_eq!(reader.u32().unwrap(), opnum);
  assert_eq!(reader.u32().unwrap(), 0, "op {opnum}");
}

struct Session {
  sessionid: [u8; 16],
  sequence: u32,
}

impl Session {
  fn sequenced(&mut self, count: u32) -> XdrWriter {
    self.sequence += 1;
    let mut args = frame(count + 1);
    args.u32(op::SEQUENCE);
    args.fixed(&self.sessionid);
    args.u32(self.sequence);
    args.u32(0);
    args.u32(0);
    args.bool(false);
    args
  }
}

fn connect(service: &mut Export<'_>, server: &mut Server) -> (u64, Session) {
  let mut args = frame(1);
  args.u32(op::EXCHANGE_ID);
  args.fixed(&[1; 8]);
  args.opaque(b"bench");
  args.u32(0);
  args.u32(0);
  args.u32(0);
  let reply = call(service, server, args.as_slice());
  let mut body = results(&reply);
  ok(&mut body, op::EXCHANGE_ID);
  let clientid = body.u64().unwrap();
  let sequenceid = body.u32().unwrap();
  let mut args = frame(1);
  args.u32(op::CREATE_SESSION);
  args.u64(clientid);
  args.u32(sequenceid);
  args.u32(0);
  let asked = ChannelAttrs {
    header_pad: 0,
    max_request: 1 << 20,
    max_response: 1 << 20,
    max_response_cached: 1 << 20,
    max_operations: 16,
    max_requests: 8,
  };
  asked.encode(&mut args);
  asked.encode(&mut args);
  args.u32(0x4000_0000);
  args.u32(1);
  args.u32(0);
  let reply = call(service, server, args.as_slice());
  let mut body = results(&reply);
  ok(&mut body, op::CREATE_SESSION);
  let mut sessionid = [0u8; 16];
  sessionid.copy_from_slice(body.fixed(16).unwrap());
  (
    clientid,
    Session {
      sessionid,
      sequence: 0,
    },
  )
}

/// Opens (creating) the bench file: its state id and handle.
fn open(
  service: &mut Export<'_>,
  server: &mut Server,
  clientid: u64,
  session: &mut Session,
) -> ([u8; 16], Vec<u8>) {
  let mut args = session.sequenced(3);
  args.u32(op::PUTROOTFH);
  args.u32(op::OPEN);
  args.u32(0);
  args.u32(3); // both
  args.u32(0); // deny none
  args.u64(clientid);
  args.opaque(b"owner");
  args.u32(1); // OPEN4_CREATE
  args.u32(0); // UNCHECKED4
  args.u32(0); // empty bitmap
  args.u32(0); // no attributes
  args.u32(0); // CLAIM_NULL
  args.opaque(b"bench");
  args.u32(op::GETFH);
  let reply = call(service, server, args.as_slice());
  let mut body = results(&reply);
  ok(&mut body, op::SEQUENCE);
  body.fixed(16 + 4 + 4 + 4 + 4 + 4).unwrap();
  ok(&mut body, op::PUTROOTFH);
  ok(&mut body, op::OPEN);
  let mut stateid = [0u8; 16];
  stateid.copy_from_slice(body.fixed(16).unwrap());
  body.fixed(4 + 8 + 8).unwrap(); // change_info
  body.u32().unwrap(); // rflags
  let words = body.u32().unwrap(); // attrset
  body.fixed(4 * words as usize).unwrap();
  // The handle comes from a second compound, which needs no walk past OPEN's delegation.
  let mut args = session.sequenced(3);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.opaque(b"bench");
  args.u32(op::GETFH);
  let reply = call(service, server, args.as_slice());
  let mut body = results(&reply);
  ok(&mut body, op::SEQUENCE);
  body.fixed(16 + 4 + 4 + 4 + 4 + 4).unwrap();
  ok(&mut body, op::PUTROOTFH);
  ok(&mut body, op::LOOKUP);
  ok(&mut body, op::GETFH);
  (stateid, body.opaque(128).unwrap().to_vec())
}

fn round(
  service: &mut Export<'_>,
  server: &mut Server,
  session: &mut Session,
  (stateid, fh): (&[u8; 16], &[u8]),
  write: bool,
  payload: &[u8],
) -> (f64, f64) {
  // Only what the server allocates is counted: the bench's own request building is subtracted.
  let mut served = 0u64;
  let started = Instant::now();
  for index in 0..OPS {
    let offset = (index as u64 % FILE_TRANSFERS) * TRANSFER as u64;
    let mut args = session.sequenced(2);
    args.u32(op::PUTFH);
    args.opaque(fh);
    if write {
      args.u32(op::WRITE);
      args.fixed(stateid);
      args.u64(offset);
      args.u32(0); // UNSTABLE4
      args.opaque(payload);
    } else {
      args.u32(op::READ);
      args.fixed(stateid);
      args.u64(offset);
      args.u32(slates_bridge_nfs::procedures::MAX_TRANSFER);
    }
    let before = ALLOCATED.load(Ordering::Relaxed);
    let reply = call(service, server, args.as_slice());
    served += ALLOCATED.load(Ordering::Relaxed) - before;
    assert_eq!(&reply[..4], &[0, 0, 0, 0], "the compound succeeds");
  }
  let elapsed = started.elapsed().as_nanos() as f64 / OPS as f64;
  let allocated = served as f64 / OPS as f64;
  (elapsed, allocated)
}

fn main() {
  let mut store = store();
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 34 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap();
  let mut handles = new_handle_store();
  let mut bridge = VolumeBridge::attached(VOLUME, &mut vol, &mut store, &mut handles, None);
  let mut service = Export::new(
    &mut bridge,
    VOLUME,
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
  let size = slates_bridge_nfs::procedures::MAX_TRANSFER + COMPOUND_HEADER_BYTES;
  let mut server = Server::new(
    7,
    Limits {
      max_clients: 4,
      max_sessions_per_client: 4,
      offer: ChannelAttrs {
        header_pad: 0,
        max_request: size,
        max_response: size,
        max_response_cached: size,
        max_operations: size / MIN_OPERATION_BYTES,
        max_requests: 4,
      },
      lease_ns: u64::MAX / 2,
    },
  );
  let (clientid, mut session) = connect(&mut service, &mut server);
  let (stateid, fh) = open(&mut service, &mut server, clientid, &mut session);
  let payload = vec![0x5Au8; TRANSFER];
  println!("op,ns_per_op_best,allocated_bytes_per_op,rounds_ns");
  for (label, write) in [("write", true), ("read", false)] {
    let mut rows = Vec::new();
    for _ in 0..ROUNDS {
      rows.push(round(
        &mut service,
        &mut server,
        &mut session,
        (&stateid, &fh),
        write,
        &payload,
      ));
    }
    let best = rows.iter().map(|(ns, _)| *ns).fold(f64::MAX, f64::min);
    let allocated = rows.last().map_or(0.0, |(_, bytes)| *bytes);
    let all: Vec<String> = rows.iter().map(|(ns, _)| format!("{ns:.0}")).collect();
    println!("{label},{best:.0},{allocated:.0},{}", all.join(" "));
  }
}
