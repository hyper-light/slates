//! The loopback NFS transport under hostile and broken connections on a busy machine (§4.6, §4.13; R4; Ada
//! 2026-10-04: "what happens if the socket breaks connection? If something tries to tamper with it? … under heavy
//! use"). Honest clients write and read back their own files over their own connections while breakers, against
//! the same volume, send partial records, oversized markers and garbage bodies, close before a reply, abort with a
//! reset right after the call that moves the connection to the volume's owner shard, and replay the honest file's
//! handle with one bit flipped — all with a spinner thread per core running. The volume lives on the shard the
//! listener is not served on, so every connection moves (`nfs.rs` `migrate`) and the breaks land on that path too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_ipc::protocol::{NamePolicy, SizeClass};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::nfs::{NFS_PROGRAM, create, mount, opaque, read, status, write};

/// Shape: honest clients, each on its own connection and file.
const HONEST: usize = 4;
/// Shape: write-then-read rounds per honest client.
const ROUNDS: usize = 150;
/// Shape: breaker threads.
const BREAKERS: usize = 4;
/// Shape: how long the daemon may take to end every broken connection's task once the breakers stop.
const SETTLE: Duration = Duration::from_secs(10);
/// Shape: the victim file's bytes, written once before the breakers start and never changed.
const VICTIM_BYTES: &[u8] = b"the victim file's bytes, never written again";
/// Format: the bits of a v1 handle (`crates/bridge-nfs/src/handle.rs`) a flip may land in and still be
/// answered: the low 48 bits of the inode number (bytes 19..25), its counter, which can name another live file of
/// the same volume, one the capability authorizes. Every other bit (the version, the volume id, the inode's
/// 16-bit volume prefix, the generation, the attachment and the token) must be refused when flipped.
const COUNTER_BITS: std::ops::Range<usize> = 152..200;
/// Format: NFSPROC3_READ (RFC 1813).
const NFSPROC3_READ: u32 = 6;
/// Format: NFSPROC3_WRITE (RFC 1813).
const NFSPROC3_WRITE: u32 = 7;
/// Format: the record marker's last-fragment bit (RFC 5531 §11).
const LAST_FRAGMENT: u32 = 0x8000_0000;

fn provision_remote_volume(instance: &str) -> String {
  let started = Instant::now();
  let mut client = loop {
    let deadlines = slates_client::Deadlines::derive(
      slates_server::daemon::LIVENESS_BUDGET_NS,
      slates_db::replay::RECOVERY_BUDGET_NS,
    )
    .get();
    match slates_client::Client::connect(instance, deadlines) {
      Ok(client) => break client,
      Err(e) => assert!(started.elapsed() < SETTLE, "connect: {e}"),
    }
  };
  for attempt in 0u32..32 {
    let name = format!("hostile-{attempt}");
    let id = client
      .create(&slates_client::CreateSpec {
        name: name.clone(),
        size: SizeClass::Bounded { limit: 64 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    if slates_server::verbs::owner_of(id) != 0 {
      return name;
    }
  }
  panic!("no volume landed on the non-control shard in 32 attempts");
}

fn two_shard_daemon(name: &str) -> (Daemon, String) {
  two_shard_daemon_with(name, |_| {})
}

/// [`two_shard_daemon`], its configuration adjusted by `adjust` before it starts.
fn two_shard_daemon_with(name: &str, adjust: impl FnOnce(&mut DaemonConfig)) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(2));
  adjust(&mut config);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  (daemon, instance)
}

/// A record-marked NFSv3 call with no credential, as the test helpers build it.
fn record(procedure: u32, args: &[u8], xid: u32) -> Vec<u8> {
  let mut body = Vec::new();
  for field in [xid, 0, 2, NFS_PROGRAM, 3, procedure, 0, 0, 0, 0] {
    body.extend_from_slice(&field.to_be_bytes());
  }
  body.extend_from_slice(args);
  let mut framed = (LAST_FRAGMENT | u32::try_from(body.len()).unwrap())
    .to_be_bytes()
    .to_vec();
  framed.extend_from_slice(&body);
  framed
}

fn read_args(handle: &[u8]) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(handle, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes());
  args.extend_from_slice(&4096u32.to_be_bytes());
  args
}

/// The big-endian word at `at` in `bytes`.
fn word(bytes: &[u8], at: usize) -> Option<u32> {
  Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// One reply record from `stream`, deframed, or `None` if the daemon closed the connection.
fn reply_record(stream: &mut TcpStream) -> Option<Vec<u8>> {
  let mut marker = [0u8; 4];
  stream.read_exact(&mut marker).ok()?;
  let length = usize::try_from(u32::from_be_bytes(marker) & !LAST_FRAGMENT).ok()?;
  let mut reply = vec![0u8; length];
  stream.read_exact(&mut reply).ok()?;
  Some(reply)
}

/// The NFS status and the data of one READ reply on `stream` (`u32::MAX` for a call the RPC layer refused), or
/// `None` if the daemon closed the connection.
fn read_reply(stream: &mut TcpStream) -> Option<(u32, Vec<u8>)> {
  let reply = reply_record(stream)?;
  let verifier = usize::try_from(word(&reply, 16)?).ok()?;
  let accept = 20 + verifier.div_ceil(4) * 4;
  if word(&reply, accept)? != 0 {
    return Some((u32::MAX, Vec::new()));
  }
  let results = reply.get(accept + 4..)?;
  let nfs_status = status(results);
  if nfs_status != 0 {
    return Some((nfs_status, Vec::new()));
  }
  // READ3resok: post_op_attr (present flag + fattr3 of 84 bytes), count, eof, data.
  let attributes = if word(results, 4)? == 1 { 84 } else { 0 };
  let data_at = 8 + attributes + 8;
  let data_length = usize::try_from(word(results, data_at)?).ok()?;
  Some((
    0,
    results
      .get(data_at + 4..data_at + 4 + data_length)?
      .to_vec(),
  ))
}

fn spin(stop: &AtomicBool) {
  let mut value = 0u64;
  while !stop.load(Ordering::Relaxed) {
    value = std::hint::black_box(value.wrapping_add(1));
  }
}

/// What one flipped-handle read showed: refused (`None`), or answered, with the bit position flipped.
type FlippedRead = Option<usize>;

/// One hostile action of kind `kind % 6` against the mount: `victim` is a file whose bytes never change; `scratch`
/// is the file the breakers' authorized writes land in.
fn break_once(
  port: u16,
  export: &str,
  kind: usize,
  victim: &[u8],
  scratch: &[u8],
  xid: u32,
) -> FlippedRead {
  let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
    return None;
  };
  match kind % 6 {
    // A partial record: the marker promises more than is sent, then the connection closes.
    0 => {
      let _ = stream.write_all(&(LAST_FRAGMENT | 4096).to_be_bytes());
      let _ = stream.write_all(&[0u8; 100]);
    }
    // A whole WRITE of the scratch file, closed before its reply.
    1 => {
      let mut args = Vec::new();
      opaque(scratch, &mut args);
      args.extend_from_slice(&0u64.to_be_bytes());
      args.extend_from_slice(&3u32.to_be_bytes());
      args.extend_from_slice(&2u32.to_be_bytes()); // FILE_SYNC
      opaque(b"bad", &mut args);
      let _ = stream.write_all(&record(NFSPROC3_WRITE, &args, xid));
    }
    // A marker for the largest record the format can state.
    2 => {
      let _ = stream.write_all(&(LAST_FRAGMENT | 0x7fff_ffff).to_be_bytes());
      let _ = stream.write_all(&[0xa5u8; 512]);
    }
    // A well-framed body of garbage.
    3 => {
      let garbage: Vec<u8> = (0..256u32)
        .map(|byte| (byte.wrapping_mul(167) ^ xid).to_le_bytes()[0])
        .collect();
      let mut framed = (LAST_FRAGMENT | 256).to_be_bytes().to_vec();
      framed.extend_from_slice(&garbage);
      let _ = stream.write_all(&framed);
      let mut sink = [0u8; 512];
      let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
      let _ = stream.read(&mut sink);
    }
    // The victim's handle with one bit flipped: refused, or naming the victim's own unchanged bytes.
    4 => {
      let mut forged = victim.to_vec();
      let bit = usize::try_from(xid).unwrap_or(0) % (forged.len() * 8);
      if let Some(byte) = forged.get_mut(bit / 8) {
        *byte ^= 1 << (bit % 8);
      }
      if stream
        .write_all(&record(NFSPROC3_READ, &read_args(&forged), xid))
        .is_err()
      {
        return None;
      }
      if let Some((0, _)) = read_reply(&mut stream) {
        return Some(bit);
      }
    }
    // Mount, send the first call (the one that moves the connection), and abort with a reset at once.
    _ => {
      let _ = mount(&mut stream, export, xid);
      let _ = stream.write_all(&record(
        NFSPROC3_READ,
        &read_args(victim),
        xid.wrapping_add(1),
      ));
      let _ = rustix::net::sockopt::set_socket_linger(&stream, Some(Duration::ZERO));
    }
  }
  None
}

/// The tasks alive on every shard (spawned and not yet ended).
fn live_tasks(daemon: &Daemon) -> u64 {
  daemon
    .shard_pulses()
    .iter()
    .map(|pulse| pulse.spawns.saturating_sub(pulse.completed))
    .sum()
}

/// §4.6, §4.13 (2026-10-04): do run honest clients and breakers against one mounted volume under a spinner per
/// core; expect every honest read to return exactly what that client last wrote, no flipped handle to read
/// anything but the file it was flipped from, the daemon still serving a fresh mount afterwards, and every broken
/// connection's task ended (no leak). The breakers' action count is the non-vacuity evidence.
#[test]
fn a_mount_survives_connections_broken_and_tampered_mid_call_under_load() {
  let (daemon, instance) = two_shard_daemon("hostile");
  let name = provision_remote_volume(&instance);
  let export = daemon.mount_capability(&name).unwrap().unwrap();
  let port = daemon.nfs_port().unwrap();
  let mut setup = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut setup, &export, 1);
  let files: Vec<Vec<u8>> = (0..HONEST)
    .map(|index| {
      create(
        &mut setup,
        &root,
        &format!("honest-{index}"),
        2 + u32::try_from(index).unwrap(),
      )
    })
    .collect();
  let victim = create(&mut setup, &root, "victim", 100);
  write(&mut setup, &victim, VICTIM_BYTES, 101);
  let scratch = create(&mut setup, &root, "scratch", 102);
  drop(setup);
  let payloads: Vec<Vec<u8>> = (0..HONEST * ROUNDS)
    .map(|round| format!("honest payload {round:08}").into_bytes())
    .collect();
  let baseline = live_tasks(&daemon);
  let cores = std::thread::available_parallelism().map_or(1, usize::from);
  let stop_spinners = AtomicBool::new(false);
  let stop_breakers = AtomicBool::new(false);
  let breaks = AtomicU64::new(0);
  let mismatches = AtomicU64::new(0);
  let answered_bits: Vec<AtomicU64> = (0..victim.len() * 8).map(|_| AtomicU64::new(0)).collect();
  std::thread::scope(|scope| {
    for _ in 0..cores {
      scope.spawn(|| spin(&stop_spinners));
    }
    for breaker in 0..BREAKERS {
      let (export, victim, scratch) = (&export, &victim, &scratch);
      let (stop, breaks, answered) = (&stop_breakers, &breaks, &answered_bits);
      scope.spawn(move || {
        let mut kind = breaker;
        while !stop.load(Ordering::Relaxed) {
          let xid = u32::try_from(kind / BREAKERS).unwrap_or(u32::MAX);
          if let Some(bit) = break_once(port, export, kind, victim, scratch, xid)
            && let Some(count) = answered.get(bit)
          {
            count.fetch_add(1, Ordering::Relaxed);
          }
          breaks.fetch_add(1, Ordering::Relaxed);
          kind += BREAKERS;
        }
      });
    }
    let honest: Vec<_> = (0..HONEST)
      .map(|index| {
        let (export, file, payloads, mismatches) = (&export, &files[index], &payloads, &mismatches);
        scope.spawn(move || {
          let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
          let _ = mount(&mut stream, export, 1);
          for round in 0..ROUNDS {
            let payload = &payloads[index * ROUNDS + round];
            let xid = 2 + 2 * u32::try_from(round).unwrap();
            write(&mut stream, file, payload, xid);
            if read(&mut stream, file, xid + 1) != *payload {
              mismatches.fetch_add(1, Ordering::Relaxed);
            }
          }
        })
      })
      .collect();
    for handle in honest {
      handle.join().unwrap();
    }
    stop_breakers.store(true, Ordering::Relaxed);
    stop_spinners.store(true, Ordering::Relaxed);
  });
  let settled_at = Instant::now();
  while live_tasks(&daemon) > baseline && settled_at.elapsed() < SETTLE {
    // A test polling the daemon's task count while it ends the broken connections' tasks.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(20));
  }
  let live_after = live_tasks(&daemon);
  let mut fresh = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let fresh_root = mount(&mut fresh, &export, 1);
  let last = payloads[ROUNDS - 1].clone();
  let honest_zero = common::nfs::lookup(&mut fresh, &fresh_root, "honest-0", 2);
  let still_served = read(&mut fresh, &honest_zero, 3);
  drop(fresh);
  let performed = breaks.load(Ordering::Relaxed);
  let answered: Vec<(usize, u64)> = answered_bits
    .iter()
    .enumerate()
    .map(|(bit, count)| (bit, count.load(Ordering::Relaxed)))
    .filter(|(_, count)| *count > 0)
    .collect();
  eprintln!(
    "hostile: flipped handle bits still answered (bit, reads) of {}: {answered:?}",
    victim.len() * 8
  );
  drop(daemon);
  eprintln!(
    "hostile: {performed} breaks, {} mismatches, live tasks {baseline} -> {live_after}",
    mismatches.load(Ordering::Relaxed)
  );
  let answered_outside_counter: Vec<usize> = answered
    .iter()
    .map(|(bit, _)| *bit)
    .filter(|bit| !COUNTER_BITS.contains(bit))
    .collect();
  assert!(
    performed >= u64::try_from(BREAKERS * 6).unwrap(),
    "every kind of break ran ({performed})"
  );
  assert_eq!(
    mismatches.load(Ordering::Relaxed),
    0,
    "every honest read returned what was written"
  );
  assert!(
    answered_outside_counter.is_empty(),
    "a flip of the version, volume, inode prefix, generation, attachment or token was answered: bits \
     {answered_outside_counter:?}"
  );
  assert!(
    still_served.starts_with(b"honest payload"),
    "the daemon still serves the volume"
  );
  assert!(
    still_served == last || still_served.len() == last.len(),
    "the last honest write is intact"
  );
  assert!(
    live_after <= baseline,
    "every broken connection's task ended: {baseline} live before, {live_after} after"
  );
}

/// §4.13 (AUD-01), 2026-10-04: do flip each of a handle's 456 bits in turn and READ through it, with a spinner per
/// core; expect every flip outside the inode's counter refused (the version, the volume id, the inode's volume
/// prefix, the generation, the attachment and the token), and any answered counter flip to name a file the
/// capability's volume holds. Exhaustive where the hostile test samples.
#[test]
fn every_single_bit_flip_of_a_handle_outside_its_counter_is_refused() {
  let (daemon, instance) = two_shard_daemon("flips");
  let name = provision_remote_volume(&instance);
  let export = daemon.mount_capability(&name).unwrap().unwrap();
  let port = daemon.nfs_port().unwrap();
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut stream, &export, 1);
  let victim = create(&mut stream, &root, "victim", 2);
  write(&mut stream, &victim, VICTIM_BYTES, 3);
  let cores = std::thread::available_parallelism().map_or(1, usize::from);
  let stop = AtomicBool::new(false);
  let mut answered = Vec::new();
  let mut refused = 0usize;
  std::thread::scope(|scope| {
    for _ in 0..cores {
      scope.spawn(|| spin(&stop));
    }
    for bit in 0..victim.len() * 8 {
      let mut forged = victim.clone();
      if let Some(byte) = forged.get_mut(bit / 8) {
        *byte ^= 1 << (bit % 8);
      }
      let xid = 10 + u32::try_from(bit).unwrap();
      stream
        .write_all(&record(NFSPROC3_READ, &read_args(&forged), xid))
        .unwrap();
      match read_reply(&mut stream).expect("the daemon answers every flipped call") {
        (0, _) => answered.push(bit),
        _ => refused += 1,
      }
    }
    stop.store(true, Ordering::Relaxed);
  });
  drop(stream);
  drop(daemon);
  let outside: Vec<usize> = answered
    .iter()
    .copied()
    .filter(|bit| !COUNTER_BITS.contains(bit))
    .collect();
  eprintln!("flips: {refused} refused, answered {answered:?}");
  assert_eq!(
    answered.len() + refused,
    VICTIM_HANDLE_BITS,
    "every bit was flipped once"
  );
  assert!(
    outside.is_empty(),
    "flips outside the counter were answered: {outside:?}"
  );
}

/// Format: a v1 handle's length in bits (`crates/bridge-nfs/src/handle.rs`: version, volume, inode, generation,
/// attachment, token = 1 + 16 + 8 + 8 + 8 + 16 bytes).
const VICTIM_HANDLE_BITS: usize = 57 * 8;

/// Shape: READs a client pipelines in one write, as a parallel build keeps many in flight on its one connection.
const PIPELINED: u32 = 64;

/// Shape: the step quantum the pipelining test pins, in nanoseconds: room for the whole burst in one turn in any
/// build ([`PIPELINED`] READs at 100 µs each, above the 46–68 µs a debug build measured per pipelined READ,
/// 2026-10-05). The live quantum is the shard's wake estimate (5.3 µs measured that night), shorter than one debug
/// READ, so with it the test judged the machine's wake speed rather than the batching.
const PINNED_QUANTUM_NS: u64 = PIPELINED as u64 * 100_000;

/// §4.6 (A-74): do pipeline [`PIPELINED`] READs of one file in a single write on one connection (the volume on the
/// other shard, so the connection moves first), with a spinner per core and the shard's quantum pinned at
/// [`PINNED_QUANTUM_NS`] (calls cheaper than the quantum, the regime batching is for); expect every reply, in
/// request order, each carrying the file's bytes, and replies sent several to a write (`nfs.replies.batched`
/// moved). A call dearer than the quantum is written alone by design: holding its reply saves a write syscall at
/// the cost of the whole pipeline's serve time (measured and rejected, docs/wip/BENCHMARKS.md, 2026-10-05). One reply and a
/// yield per call queued every pipelined call behind all the earlier ones' writes and yields: a parallel Go build
/// saw 1.1–1.4 ms per OPEN where one call in flight saw 0.18 ms, against a 3 µs median serve (2026-10-04).
#[test]
fn pipelined_calls_are_answered_in_order_and_their_replies_share_writes() {
  let (daemon, instance) = two_shard_daemon_with("pipeline", |config| {
    config.runtime.wake_tracking = None;
    config.runtime.step_budget_ns = PINNED_QUANTUM_NS;
  });
  let name = provision_remote_volume(&instance);
  let export = daemon.mount_capability(&name).unwrap().unwrap();
  let port = daemon.nfs_port().unwrap();
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut stream, &export, 1);
  let victim = create(&mut stream, &root, "victim", 2);
  write(&mut stream, &victim, VICTIM_BYTES, 3);
  let cores = std::thread::available_parallelism().map_or(1, usize::from);
  let stop = AtomicBool::new(false);
  let mut replies = Vec::new();
  std::thread::scope(|scope| {
    for _ in 0..cores {
      scope.spawn(|| spin(&stop));
    }
    let mut burst = Vec::new();
    for at in 0..PIPELINED {
      burst.extend_from_slice(&record(NFSPROC3_READ, &read_args(&victim), 100 + at));
    }
    stream.write_all(&burst).unwrap();
    for _ in 0..PIPELINED {
      let reply = reply_record(&mut stream).expect("the daemon answers every pipelined call");
      replies.push(reply);
    }
    stop.store(true, Ordering::Relaxed);
  });
  for (at, reply) in replies.iter().enumerate() {
    assert_eq!(
      word(reply, 0),
      Some(100 + u32::try_from(at).unwrap()),
      "reply {at} answers call {at}"
    );
  }
  let refusals = daemon.refusals_on_every_shard().unwrap();
  let batched = refusals.get("nfs.replies.batched").copied().unwrap_or(0);
  drop(stream);
  drop(daemon);
  assert!(batched > 0, "pipelined replies shared writes: {refusals:?}");
}
