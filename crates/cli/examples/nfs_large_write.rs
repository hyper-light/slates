//! What a large file written through the NFS export costs slates' own write path (§4.6; VFS plan step 11), with no
//! kernel client and no privilege. An in-process daemon serves one volume over its NFS export, and one connection plays
//! a kernel client flushing a large new file: [`IN_FLIGHT`] UNSTABLE WRITEs of [`TRANSFER`] bytes kept pipelined, in
//! offset order, then one COMMIT, as Linux's client flushes a file at `fsync` (its slot table's worth in flight, its
//! `wsize` a WRITE). Each round writes a new file of [`FILE_MIB`] MiB over NFSv3, then another over NFSv4.2 (an
//! NFSv4.1 session of [`IN_FLIGHT`] slots, each WRITE a `SEQUENCE, PUTFH, WRITE` compound under the anonymous state
//! id, as the kernel client's NFSv4.2 mount sends them), and removes both.
//!
//! Rows are `nfs_large_write\t<v3|v4>\t<round>\t<MiB/s>\t<process cpu ns per MiB>\t<load>`, then per version
//! `nfs_large_write_best\t<v3|v4>\t<MiB/s>\t<every round's MiB/s>`. The process CPU is the daemon's and the client thread's
//! together; the client's share is the same in every arm. Nothing here gates; an A/B in `docs/wip/BENCHMARKS.md` is two
//! builds of this example, one per arm, run interleaved.
//!
//! The writes land in the volume, in RAM; the bench writes nothing to disk (R1).
//!
//! Run: `cargo build --release -p slates-cli --example nfs_large_write && target/release/examples/nfs_large_write`
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
use slates_bridge_nfs::rpc::{RecordReader, write_record};
use slates_bridge_nfs::v4::compound::op;
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_client::{Client, CreateSpec, Deadlines, Intent};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_ipc::protocol::{NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: one WRITE's data, the server's offered transfer (`procedures::MAX_TRANSFER`) and Linux's `wsize` against it.
const TRANSFER: usize = 256 * 1024;
/// Shape: WRITEs kept in flight, about what Linux's client pipelines on one connection while it flushes.
const IN_FLIGHT: usize = 16;
/// Shape: the file each round writes, in MiB: large enough that a round lasts a quarter second or more.
const FILE_MIB: usize = 512;
/// Shape: rounds; every round is shown and the best reported (BENCHMARKS.md best-of-N).
const ROUNDS: usize = 7;
/// Shape: the volume's quota — room for one round's file twice over.
const VOLUME_BYTES: u64 = 2 << 30;
/// Shape: the machine profile's budget per probe, as `nfs_writeback_ab` takes it.
const PROBE_BUDGET: Duration = Duration::from_millis(5);
/// Shape: how long the daemon may take to accept the first client, and an RPC its answer.
const WAIT: Duration = Duration::from_secs(60);
/// Format: the NFS program and version 3 (RFC 1813).
const NFS: (u32, u32) = (100_003, 3);
/// Format: NFSPROC3_WRITE.
const WRITE: u32 = 7;
/// Format: NFSPROC3_CREATE.
const CREATE: u32 = 8;
/// Format: NFSPROC3_REMOVE.
const REMOVE: u32 = 12;
/// Format: NFSPROC3_COMMIT.
const COMMIT: u32 = 21;
/// Format: NFSv4's version, its COMPOUND procedure and the minor version every compound speaks (RFC 8881, RFC 7862).
const V4: (u32, u32, u32) = (4, 1, 2);
/// Format: a session id's bytes, and a state id's (its seqid and its 12-byte other).
const SESSIONID_BYTES: usize = 16;
/// Format: see [`SESSIONID_BYTES`].
const STATEID_OTHER_BYTES: usize = 12;
/// Shape: the request and reply sizes the session asks for: one transfer and room for its compound.
const CHANNEL_BYTES: u32 = 1 << 20;
/// Shape: the most operations one compound of the bench carries, with room.
const CHANNEL_OPERATIONS: u32 = 8;
/// Format: `UNSTABLE`, a WRITE's stability.
const UNSTABLE: u32 = 0;
/// Format: one MiB.
const MIB: usize = 1 << 20;
/// Format: nanoseconds per second.
const NS_PER_SECOND: f64 = 1e9;

/// One NFS connection, AUTH_NONE, with replies read as records arrive.
struct Rpc {
  stream: TcpStream,
  xid: u32,
  reader: RecordReader,
  received: Vec<u8>,
  chunk: Vec<u8>,
}

impl Rpc {
  fn connect(port: u16) -> Rpc {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_nodelay(true).unwrap();
    Rpc {
      stream,
      xid: 1,
      reader: RecordReader::default(),
      received: Vec::new(),
      chunk: vec![0u8; TRANSFER],
    }
  }

  /// Sends one NFSv3 call; its xid.
  fn send(&mut self, procedure: u32, args: &[u8]) -> u32 {
    self.send_as(NFS.1, procedure, args)
  }

  /// Sends one call of NFS version `version`; its xid.
  fn send_as(&mut self, version: u32, procedure: u32, args: &[u8]) -> u32 {
    self.xid = self.xid.wrapping_add(1);
    let mut call = XdrWriter::with_capacity(args.len() + 64);
    call.u32(self.xid);
    call.u32(0); // CALL
    call.u32(2); // RPC version
    call.u32(NFS.0);
    call.u32(version);
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
    self.xid
  }

  /// The next reply: its xid and the procedure's results, after the accepted reply's header.
  fn receive(&mut self) -> (u32, Vec<u8>) {
    let message = loop {
      if !self.received.is_empty() {
        let (message, used) = self.reader.read(&self.received).unwrap();
        self.received.drain(..used);
        if let Some(message) = message {
          break message;
        }
      }
      let read = self.stream.read(&mut self.chunk).unwrap();
      assert!(read > 0, "the export closed the connection");
      self.received.extend_from_slice(&self.chunk[..read]);
    };
    let mut reply = XdrReader::new(&message);
    let xid = reply.u32().unwrap();
    assert_eq!(reply.u32().unwrap(), 1, "a reply");
    assert_eq!(reply.u32().unwrap(), 0, "accepted");
    let _flavor = reply.u32().unwrap();
    let _verifier = reply.opaque(400).unwrap();
    assert_eq!(reply.u32().unwrap(), 0, "the procedure ran");
    (xid, reply.rest().to_vec())
  }

  /// One NFSv4 COMPOUND and its results.
  fn call_v4(&mut self, args: &[u8]) -> Vec<u8> {
    let xid = self.send_as(V4.0, V4.1, args);
    let (answered, results) = self.receive();
    assert_eq!(answered, xid, "the reply's xid");
    results
  }

  /// One call and its results.
  fn call(&mut self, procedure: u32, args: &[u8]) -> Vec<u8> {
    let xid = self.send(procedure, args);
    let (answered, results) = self.receive();
    assert_eq!(answered, xid, "the reply's xid");
    results
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

  /// REMOVE `name` from `dir`.
  fn remove(&mut self, dir: &[u8], name: &str) {
    let mut args = XdrWriter::new();
    args.opaque(dir);
    args.opaque(name.as_bytes());
    let results = self.call(REMOVE, &args.into_bytes());
    assert_eq!(XdrReader::new(&results).u32().unwrap(), 0, "REMOVE {name}");
  }

  /// The WRITE arguments for `data` at `offset`, UNSTABLE.
  fn write_args(fh: &[u8], offset: u64, data: &[u8]) -> Vec<u8> {
    let mut args = XdrWriter::with_capacity(data.len() + 128);
    args.opaque(fh);
    args.u64(offset);
    args.u32(u32::try_from(data.len()).unwrap());
    args.u32(UNSTABLE);
    args.opaque(data);
    args.into_bytes()
  }

  /// Writes `bytes` bytes of `data` (repeated) into `fh` from offset 0, [`IN_FLIGHT`] WRITEs pipelined, then COMMITs.
  fn flush(&mut self, fh: &[u8], data: &[u8], bytes: usize) {
    let writes = bytes / TRANSFER;
    let (mut sent, mut answered) = (0usize, 0usize);
    while answered < writes {
      while sent < writes && sent - answered < IN_FLIGHT {
        let args = Rpc::write_args(fh, (sent * TRANSFER) as u64, data);
        self.send(WRITE, &args);
        sent += 1;
      }
      let (_, results) = self.receive();
      assert_eq!(XdrReader::new(&results).u32().unwrap(), 0, "WRITE");
      answered += 1;
    }
    let mut args = XdrWriter::new();
    args.opaque(fh);
    args.u64(0);
    args.u32(0); // to the end of the file
    let results = self.call(COMMIT, &args.into_bytes());
    assert_eq!(XdrReader::new(&results).u32().unwrap(), 0, "COMMIT");
  }
}

/// A COMPOUND's frame: an empty tag, minor version 2 and the operation count.
fn frame(count: u32) -> XdrWriter {
  let mut args = XdrWriter::with_capacity(TRANSFER + 256);
  args.opaque(b"");
  args.u32(V4.2);
  args.u32(count);
  args
}

/// A COMPOUND4res read past its header: the reader at its first result, after checking the compound succeeded.
fn compound_ok(results: &[u8]) -> XdrReader<'_> {
  let mut reply = XdrReader::new(results);
  assert_eq!(reply.u32().unwrap(), 0, "the compound succeeded");
  reply.opaque(1024).unwrap();
  let _count = reply.u32().unwrap();
  reply
}

/// Skips a SEQUENCE result (its op, status, session id and five words).
fn skip_sequence(reply: &mut XdrReader<'_>) {
  assert_eq!(reply.u32().unwrap(), op::SEQUENCE);
  assert_eq!(reply.u32().unwrap(), 0, "SEQUENCE");
  reply.fixed(SESSIONID_BYTES + 5 * 4).unwrap();
}

/// One NFSv4.1 session on its own connection: its id and each slot's last sequence id.
struct Session {
  rpc: Rpc,
  sessionid: [u8; SESSIONID_BYTES],
  slots: Vec<u32>,
}

impl Session {
  fn open(port: u16) -> Session {
    let mut rpc = Rpc::connect(port);
    let mut args = frame(1);
    args.u32(op::EXCHANGE_ID);
    args.fixed(&[1; 8]);
    args.opaque(format!("nfs_large_write-{}", std::process::id()).as_bytes());
    args.u32(0); // flags
    args.u32(0); // SP4_NONE
    args.u32(0); // no implementation id
    let results = rpc.call_v4(&args.into_bytes());
    let mut reply = compound_ok(&results);
    assert_eq!(reply.u32().unwrap(), op::EXCHANGE_ID);
    assert_eq!(reply.u32().unwrap(), 0, "EXCHANGE_ID");
    let clientid = reply.u64().unwrap();
    let sequenceid = reply.u32().unwrap();
    let mut args = frame(1);
    args.u32(op::CREATE_SESSION);
    args.u64(clientid);
    args.u32(sequenceid);
    args.u32(0); // flags
    for requests in [u32::try_from(IN_FLIGHT).unwrap(), 1] {
      args.u32(0); // header pad
      args.u32(CHANNEL_BYTES);
      args.u32(CHANNEL_BYTES);
      args.u32(CHANNEL_BYTES);
      args.u32(CHANNEL_OPERATIONS);
      args.u32(requests);
      args.u32(0); // no RDMA attributes
    }
    args.u32(0x4000_0000); // callback program (no callbacks are taken)
    args.u32(1); // one security parameter
    args.u32(0); // AUTH_NONE
    let results = rpc.call_v4(&args.into_bytes());
    let mut reply = compound_ok(&results);
    assert_eq!(reply.u32().unwrap(), op::CREATE_SESSION);
    assert_eq!(reply.u32().unwrap(), 0, "CREATE_SESSION");
    let mut sessionid = [0u8; SESSIONID_BYTES];
    sessionid.copy_from_slice(reply.fixed(SESSIONID_BYTES).unwrap());
    let _sequence = reply.u32().unwrap();
    let _flags = reply.u32().unwrap();
    for _ in 0..5 {
      let _ = reply.u32().unwrap(); // pad and sizes
    }
    let granted = usize::try_from(reply.u32().unwrap()).unwrap();
    assert!(granted >= 1, "the session has a slot");
    Session {
      rpc,
      sessionid,
      slots: vec![0; granted.min(IN_FLIGHT)],
    }
  }

  /// A compound of `count` further operations, opening with SEQUENCE on `slot`.
  fn sequenced(&mut self, slot: usize, count: u32) -> XdrWriter {
    self.slots[slot] = self.slots[slot].wrapping_add(1);
    let mut args = frame(count + 1);
    args.u32(op::SEQUENCE);
    args.fixed(&self.sessionid);
    args.u32(self.slots[slot]);
    args.u32(u32::try_from(slot).unwrap());
    args.u32(u32::try_from(self.slots.len() - 1).unwrap());
    args.u32(0); // sa_cachethis: false, as Linux sends a WRITE
    args
  }

  /// The handle of `name` in the volume the mount `capability` names.
  fn lookup(&mut self, capability: &str, name: &str) -> Vec<u8> {
    let mut args = self.sequenced(0, 4);
    args.u32(op::PUTROOTFH);
    args.u32(op::LOOKUP);
    args.opaque(capability.trim_start_matches('/').as_bytes());
    args.u32(op::LOOKUP);
    args.opaque(name.as_bytes());
    args.u32(op::GETFH);
    let results = self.rpc.call_v4(&args.into_bytes());
    let mut reply = compound_ok(&results);
    skip_sequence(&mut reply);
    for opnum in [op::PUTROOTFH, op::LOOKUP, op::LOOKUP, op::GETFH] {
      assert_eq!(reply.u32().unwrap(), opnum);
      assert_eq!(reply.u32().unwrap(), 0, "op {opnum}");
    }
    reply.opaque(128).unwrap().to_vec()
  }

  /// Writes `bytes` bytes of `data` (repeated) into `fh` from offset 0, a compound a slot pipelined, then COMMITs.
  fn flush(&mut self, fh: &[u8], data: &[u8], bytes: usize) {
    let writes = bytes / TRANSFER;
    let mut free: Vec<usize> = (0..self.slots.len()).rev().collect();
    let mut by_xid: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    let (mut sent, mut answered) = (0usize, 0usize);
    while answered < writes {
      while sent < writes
        && let Some(slot) = free.pop()
      {
        let mut args = self.sequenced(slot, 2);
        args.u32(op::PUTFH);
        args.opaque(fh);
        args.u32(op::WRITE);
        args.u32(0); // the anonymous state id: seqid 0, other all zero
        args.fixed(&[0u8; STATEID_OTHER_BYTES]);
        args.u64((sent * TRANSFER) as u64);
        args.u32(UNSTABLE);
        args.opaque(data);
        let xid = self.rpc.send_as(V4.0, V4.1, &args.into_bytes());
        by_xid.insert(xid, slot);
        sent += 1;
      }
      let (xid, results) = self.rpc.receive();
      let mut reply = compound_ok(&results);
      skip_sequence(&mut reply);
      free.push(by_xid.remove(&xid).expect("a reply to a WRITE sent"));
      answered += 1;
    }
    let mut args = self.sequenced(0, 2);
    args.u32(op::PUTFH);
    args.opaque(fh);
    args.u32(op::COMMIT);
    args.u64(0);
    args.u32(0);
    let results = self.rpc.call_v4(&args.into_bytes());
    let mut reply = compound_ok(&results);
    skip_sequence(&mut reply);
  }
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

fn main() {
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: PROBE_BUDGET,
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("slates-nfslw-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, None);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-nfslw-{}", std::process::id()),
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
      name: "nfslw".to_owned(),
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
  let path = daemon.mount_capability("nfslw").unwrap().unwrap();
  let caller = Credentials {
    uid: 0,
    gid: 0,
    gids: Vec::new(),
  };
  let root = fetch_root_handle(port, &caller, &path, WAIT).unwrap().0;
  let mut client = Rpc::connect(port);
  // One transfer of bytes that are not all alike, as a real file's are.
  let data: Vec<u8> = (0..TRANSFER)
    .map(|at| u8::try_from((at.wrapping_mul(2_654_435_761) >> 13) & 0xff).unwrap())
    .collect();
  let mut session = Session::open(port);
  let mut rates: [Vec<f64>; 2] = [Vec::with_capacity(ROUNDS), Vec::with_capacity(ROUNDS)];
  for round in 0..ROUNDS {
    for (version, rates) in ["v3", "v4"].into_iter().zip(rates.iter_mut()) {
      let name = format!("large-{version}-{round}");
      let fh = client.create(&root, &name);
      let v4_fh = (version == "v4").then(|| session.lookup(&path, &name));
      let cpu_before = cpu_ns();
      let began = Instant::now();
      match &v4_fh {
        Some(fh) => session.flush(fh, &data, FILE_MIB * MIB),
        None => client.flush(&fh, &data, FILE_MIB * MIB),
      }
      let took = began.elapsed().as_nanos() as f64 / NS_PER_SECOND;
      let cpu = cpu_ns() - cpu_before;
      let rate = FILE_MIB as f64 / took;
      rates.push(rate);
      println!(
        "nfs_large_write\t{version}\t{round}\t{rate:.1}\t{}\t{}",
        cpu / FILE_MIB as u64,
        load()
      );
      client.remove(&root, &name);
    }
  }
  for (version, rates) in ["v3", "v4"].into_iter().zip(rates.iter()) {
    let best = rates.iter().copied().fold(0.0f64, f64::max);
    let all: Vec<String> = rates.iter().map(|rate| format!("{rate:.1}")).collect();
    println!("nfs_large_write_best\t{version}\t{best:.1}\t{}", all.join(" "));
  }
  drop(session);
  drop(client);
  drop(sdk);
  daemon.stop();
}
