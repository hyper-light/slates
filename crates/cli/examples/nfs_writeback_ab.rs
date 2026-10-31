//! What a caching NFS client's write-backs cost (§4.6 "NFS v3"; A-192, the A/B Ada asked for on 2026-10-31 between
//! sync-point coherence and refusing a stale write-back). An in-process daemon serves one volume over its NFS export,
//! and one connection plays a caching client: it READs a page and writes it back whole with a store, over and over,
//! across 32 files of 1 MiB. Phases alternate between quiet and contended. In a contended phase a second connection
//! writes pages of 32 other files the first has cached, as fast as it can: every such write is a change the server must
//! account against the first client's cache, and none of them overlaps what the first writes back, so no write-back is
//! refused for a reason.
//!
//! Rows are `nfs_wb\t<phase>\t<mode>\t<read p50>\t<p90>\t<p99>\t<write p50>\t<p90>\t<p99>\t<write-backs/s>\t<refused>\t
//! <other writes>\t<load>` in nanoseconds, then `nfs_wb_cost\t<process cpu ns>\t<peak rss kib>\t<wall ns>`. Nothing here
//! gates; the A/B in `docs/wip/BENCHMARKS.md` is two builds of this example, one per arm, run interleaved.
//!
//! The writes land in the volume, in RAM; the bench writes nothing to disk (R1).
//!
//! Run: `cargo build --release -p slates-cli --example nfs_writeback_ab && target/release/examples/nfs_writeback_ab`
// Bench harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_bridge_nfs::client::{Credentials, fetch_root_handle};
use slates_bridge_nfs::rpc::{read_record, write_record};
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_client::{Client, CreateSpec, Deadlines, Intent};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_ipc::protocol::{NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the page a client writes back whole: Apple silicon's 16 KiB.
const PAGE: usize = 16 * 1024;
/// Shape: files each connection works on.
const FILES: usize = 32;
/// Shape: pages per file, so each is 1 MiB.
const PAGES: usize = 64;
/// Shape: write-backs timed per phase — 2,000 samples put a p99 on 20 of them.
const WRITEBACKS: usize = 2000;
/// Shape: phases each way, alternating, so drift in the machine's load falls on both.
const PHASES: usize = 4;
/// Shape: the volume's quota — room for both sets of files twice over.
const VOLUME_BYTES: u64 = 256 << 20;
/// Shape: the machine profile's budget per probe, as `write_round_ab` takes it.
const PROBE_BUDGET: Duration = Duration::from_millis(5);
/// Shape: how long the daemon may take to accept the first client, and an RPC its answer.
const WAIT: Duration = Duration::from_secs(30);
/// Format: the NFS program, version 3, and the procedures the bench calls (RFC 1813).
const NFS: (u32, u32) = (100_003, 3);
/// Format: see [`NFS`].
const READ: u32 = 6;
/// Format: see [`NFS`].
const WRITE: u32 = 7;
/// Format: see [`NFS`].
const CREATE: u32 = 8;
/// Format: `FILE_SYNC`, a WRITE's stability.
const FILE_SYNC: u32 = 2;

/// One NFS connection, AUTH_NONE.
struct Rpc {
  stream: TcpStream,
  xid: u32,
}

impl Rpc {
  fn connect(port: u16) -> Rpc {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_nodelay(true).unwrap();
    Rpc { stream, xid: 1 }
  }

  /// One call: the procedure's results, after the accepted reply's header.
  fn call(&mut self, procedure: u32, args: &[u8]) -> Vec<u8> {
    self.xid = self.xid.wrapping_add(1);
    let mut call = XdrWriter::new();
    call.u32(self.xid);
    call.u32(0); // CALL
    call.u32(2); // RPC version
    call.u32(NFS.0);
    call.u32(NFS.1);
    call.u32(procedure);
    for _ in 0..2 {
      call.u32(0); // AUTH_NONE credential, then verifier
      call.u32(0);
    }
    call.fixed(args);
    self
      .stream
      .write_all(&write_record(&call.into_bytes()))
      .unwrap();
    let mut received = Vec::new();
    let mut chunk = vec![0u8; PAGE * 2];
    let message = loop {
      if let Ok((message, _)) = read_record(&received) {
        break message;
      }
      let read = self.stream.read(&mut chunk).unwrap();
      assert!(read > 0, "the export closed the connection");
      received.extend_from_slice(&chunk[..read]);
    };
    let mut reply = XdrReader::new(&message);
    assert_eq!(reply.u32().unwrap(), self.xid, "the reply's xid");
    assert_eq!(reply.u32().unwrap(), 1, "a reply");
    assert_eq!(reply.u32().unwrap(), 0, "accepted");
    let _flavor = reply.u32().unwrap();
    let _verifier = reply.opaque(400).unwrap();
    assert_eq!(reply.u32().unwrap(), 0, "the procedure ran");
    reply.rest().to_vec()
  }

  /// CREATE `name` in `dir` (UNCHECKED, mode 0644): the new file's handle.
  fn create(&mut self, dir: &[u8], name: &str) -> Vec<u8> {
    let mut args = XdrWriter::new();
    args.opaque(dir);
    args.opaque(name.as_bytes());
    args.u32(0); // UNCHECKED
    args.u32(1); // set_mode
    args.u32(0o644);
    for _ in 0..5 {
      args.u32(0); // uid, gid, size unset; times unchanged
    }
    let results = self.call(CREATE, &args.into_bytes());
    let mut reply = XdrReader::new(&results);
    assert_eq!(reply.u32().unwrap(), 0, "CREATE {name}");
    assert_eq!(reply.u32().unwrap(), 1, "CREATE returned a handle");
    reply.opaque(64).unwrap().to_vec()
  }

  /// READ a page at `offset`: its bytes.
  fn read(&mut self, fh: &[u8], offset: u64) -> Vec<u8> {
    let mut args = XdrWriter::new();
    args.opaque(fh);
    args.u64(offset);
    args.u32(u32::try_from(PAGE).unwrap());
    let results = self.call(READ, &args.into_bytes());
    let mut reply = XdrReader::new(&results);
    assert_eq!(reply.u32().unwrap(), 0, "READ");
    if reply.u32().unwrap() == 1 {
      reply.fixed(84).unwrap();
    }
    let _count = reply.u32().unwrap();
    let _eof = reply.u32().unwrap();
    reply.opaque(PAGE).unwrap().to_vec()
  }

  /// WRITE `data` at `offset`, `FILE_SYNC`: the status.
  fn write(&mut self, fh: &[u8], offset: u64, data: &[u8]) -> u32 {
    let mut args = XdrWriter::new();
    args.opaque(fh);
    args.u64(offset);
    args.u32(u32::try_from(data.len()).unwrap());
    args.u32(FILE_SYNC);
    args.opaque(data);
    let results = self.call(WRITE, &args.into_bytes());
    XdrReader::new(&results).u32().unwrap()
  }
}

/// The 50th, 90th and 99th percentiles of `samples`, nanoseconds, tab-separated.
fn quantiles(mut samples: Vec<u64>) -> String {
  samples.sort_unstable();
  let at = |permille: usize| {
    let last = samples.len().saturating_sub(1);
    samples.get(last * permille / 1000).copied().unwrap_or(0)
  };
  format!("{}\t{}\t{}", at(500), at(900), at(990))
}

/// The machine's load averages, as the system reports them.
fn load() -> String {
  std::process::Command::new("sysctl")
    .args(["-n", "vm.loadavg"])
    .output()
    .ok()
    .filter(|out| out.status.success())
    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    .or_else(|| {
      #[allow(clippy::disallowed_methods)] // a read of procfs, not a write
      std::fs::read_to_string("/proc/loadavg").ok()
    })
    .unwrap_or_default()
    .split_whitespace()
    .filter(|word| *word != "{" && *word != "}")
    .take(3)
    .collect::<Vec<_>>()
    .join(" ")
}

/// This process's CPU time, all threads, in nanoseconds.
fn cpu_ns() -> u64 {
  let now = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
  u64::try_from(now.tv_sec).unwrap() * 1_000_000_000 + u64::try_from(now.tv_nsec).unwrap()
}

/// This process's peak resident set, in KiB, as `ps` reports the current one at the end.
fn rss_kib() -> u64 {
  std::process::Command::new("ps")
    .args(["-o", "rss=", "-p", &std::process::id().to_string()])
    .output()
    .ok()
    .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
    .unwrap_or(0)
}

fn main() {
  let wall = Instant::now();
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: PROBE_BUDGET,
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("slates-nfswb-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-nfswb-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
  let started = Instant::now();
  let mut sdk = loop {
    match Client::connect(&instance, deadlines) {
      Ok(client) => break client,
      Err(e) => assert!(started.elapsed() < WAIT, "connect: {e}"),
    }
  };
  let volume = sdk
    .create(&CreateSpec {
      name: "nfswb".to_owned(),
      size: SizeClass::Bounded {
        limit: VOLUME_BYTES,
      },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let _ = sdk.attach(volume, None, Intent::Write).unwrap();
  let port = daemon.nfs_port().expect("the daemon serves NFS");
  let path = daemon.mount_capability("nfswb").unwrap().unwrap();
  let caller = Credentials {
    uid: 0,
    gid: 0,
    gids: Vec::new(),
  };
  let root = fetch_root_handle(port, &caller, &path, WAIT).unwrap().0;
  let mut client = Rpc::connect(port);
  let mut other = Rpc::connect(port);
  // The client's files and the other connection's, each 1 MiB of its own byte; the client reads every page of both
  // sets once, so its cache holds them all.
  let mine: Vec<Vec<u8>> = (0..FILES)
    .map(|k| client.create(&root, &format!("mine-{k}")))
    .collect();
  let theirs: Vec<Vec<u8>> = (0..FILES)
    .map(|k| client.create(&root, &format!("theirs-{k}")))
    .collect();
  for fh in mine.iter().chain(&theirs) {
    for page in 0..PAGES {
      assert_eq!(client.write(fh, (page * PAGE) as u64, &[b'A'; PAGE]), 0);
    }
    for page in 0..PAGES {
      let _ = client.read(fh, (page * PAGE) as u64);
    }
  }
  let cpu_before = cpu_ns();
  for phase in 0..PHASES * 2 {
    let contended = phase % 2 == 1;
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (reads, writes, refused, others, took) = std::thread::scope(|scope| {
      let writer = contended.then(|| {
        let (stop, theirs, other) = (&stop, &theirs, &mut other);
        scope.spawn(move || {
          let mut written = 0usize;
          while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let fh = &theirs[written % FILES];
            let page = (written / FILES) % PAGES;
            assert_eq!(
              other.write(fh, (page * PAGE) as u64, &[b'C'; PAGE]),
              0,
              "the other connection's write"
            );
            written += 1;
          }
          written
        })
      });
      let mut reads = Vec::with_capacity(WRITEBACKS);
      let mut writes = Vec::with_capacity(WRITEBACKS);
      let mut refused = 0usize;
      let phase_started = Instant::now();
      for k in 0..WRITEBACKS {
        let fh = &mine[(k * 7) % FILES];
        let offset = (((k * 13) % PAGES) * PAGE) as u64;
        let started = Instant::now();
        let mut page = client.read(fh, offset);
        reads.push(u64::try_from(started.elapsed().as_nanos()).unwrap());
        page[0] = u8::try_from(k % 251).unwrap();
        let started = Instant::now();
        if client.write(fh, offset, &page) != 0 {
          refused += 1;
        }
        writes.push(u64::try_from(started.elapsed().as_nanos()).unwrap());
      }
      let took = phase_started.elapsed();
      stop.store(true, std::sync::atomic::Ordering::Relaxed);
      let others = writer.map_or(0, |writer| writer.join().unwrap());
      (reads, writes, refused, others, took)
    });
    let rate = (WRITEBACKS as f64 / took.as_secs_f64()) as u64;
    println!(
      "nfs_wb\t{phase}\t{}\t{}\t{}\t{rate}\t{refused}\t{others}\t{}",
      if contended { "contended" } else { "quiet" },
      quantiles(reads),
      quantiles(writes),
      load()
    );
  }
  let cpu = cpu_ns().saturating_sub(cpu_before);
  println!(
    "nfs_wb_cost\t{cpu}\t{}\t{}",
    rss_kib(),
    wall.elapsed().as_nanos()
  );
  drop(client);
  drop(other);
  drop(sdk);
  daemon.stop();
}
