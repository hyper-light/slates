//! A-113: an NFS loopback connection outlives its daemon. The real `slates anchor` supervises the daemon and holds
//! every connection the daemon accepts; a `SIGKILL` of the daemon must not close the connection — its client sees a
//! slow reply, and the next daemon answers every request the dead one left unanswered, on the same socket, with each
//! reply whole. This is what keeps a kernel NFS client out of its disconnect and reconnect path, where macOS's client
//! panicked the machine twice on 2026-10-06 (`docs/bugs/2026-10-06-macos-nfs-client-panics-when-its-server-restarts.md`).
//!
//! The client here is a userspace ONC RPC client over TCP (no kernel mount, so the test can never panic the host): one
//! connection, a burst of pipelined `FILE_SYNC` WRITEs of every size up to the transfer ceiling in flight, the daemon
//! killed at a varying moment inside the burst. Every request must be answered `NFS3_OK` (a reply sent by the dead
//! daemon may arrive again from its successor — RFC 5531 lets a client drop a reply whose xid it no longer awaits, and
//! this client counts them), the connection must never close, a READ after each kill must be answered by a new daemon,
//! and the file must end as the model of every write says, byte for byte.
//!
//! Gated: `SLATES_TEST_CLI=1` (it starts an anchor and its daemons). Linux does not hold connections yet — its kernel
//! may take part of a send, so a successor could resume mid-record (GAPS, A-113) — and skips loudly there.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

#[path = "../../server/tests/common/nfs.rs"]
#[allow(dead_code)]
mod nfs;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long the daemon may take to come up, and to be replaced after a kill.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: the poll interval of the waits.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answered verbs before the daemon counts as settled (the anchor may restart it once at
/// startup, cli.rs `start_anchor_with_environment`).
const STABLE_STREAK: u32 = 10;
/// Shape: daemon kills, each inside a burst.
const KILLS: usize = 6;
/// Shape: the WRITEs one burst pipelines.
const BURST: usize = 12;
/// Shape: the sizes the WRITEs cycle through: a page, a typical client transfer, and the transfer ceiling (the
/// largest record a connection sends or receives whole).
const SIZES: [usize; 3] = [4096, 65_536, 262_144];
/// Format: NFSPROC3_WRITE and NFSPROC3_READ (RFC 1813).
const WRITE: u32 = 7;
/// See [`WRITE`].
const READ: u32 = 6;
/// Format: `stable_how` FILE_SYNC (RFC 1813 §3.3.7).
const FILE_SYNC: u32 = 2;

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

fn run(instance: &str, args: &[&str]) -> (i32, String, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn pause() {
  #[allow(clippy::disallowed_methods)] // the test paces its polls
  std::thread::sleep(POLL);
}

/// The anchor, killed and reaped on drop, so a failed assertion leaves no daemon behind (the daemon follows its
/// anchor out).
struct Anchor(Child);

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

fn start_anchor(instance: &str) -> Anchor {
  let child = slates()
    .args(["--instance", instance, "anchor", "--quick", "--shards", "2"])
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  let anchor = Anchor(child);
  let started = Instant::now();
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if run(instance, &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    assert!(started.elapsed() < START_WAIT, "the daemon came up");
    pause();
  }
  let (code, _, error) = run(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap: {error}");
  anchor
}

/// The daemon's pid, from `status --json`, or `None` while no daemon answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, out, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  let after = out.split("\"pid\":").nth(1)?;
  after
    .trim_start()
    .split(|c: char| !c.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

fn json_field(text: &str, key: &str) -> String {
  let needle = format!("\"{key}\":");
  let after = text
    .split(&needle)
    .nth(1)
    .unwrap_or_else(|| panic!("{key} in {text}"));
  let after = after.trim_start();
  if let Some(quoted) = after.strip_prefix('"') {
    quoted.split('"').next().unwrap().to_owned()
  } else {
    after.split([',', '}']).next().unwrap().trim().to_owned()
  }
}

/// One framed ONC RPC call (AUTH_NONE) of NFS version 3 `procedure`.
fn framed_call(procedure: u32, args: &[u8], xid: u32) -> Vec<u8> {
  let mut body = Vec::new();
  for field in [xid, 0, 2, nfs::NFS_PROGRAM, 3, procedure, 0, 0, 0, 0] {
    body.extend_from_slice(&field.to_be_bytes());
  }
  body.extend_from_slice(args);
  let mut framed = (0x8000_0000u32 | u32::try_from(body.len()).unwrap())
    .to_be_bytes()
    .to_vec();
  framed.extend_from_slice(&body);
  framed
}

fn write_args(file: &[u8], offset: u64, data: &[u8]) -> Vec<u8> {
  let mut args = Vec::new();
  nfs::opaque(file, &mut args);
  args.extend_from_slice(&offset.to_be_bytes());
  args.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
  args.extend_from_slice(&FILE_SYNC.to_be_bytes());
  nfs::opaque(data, &mut args);
  args
}

fn read_args(file: &[u8], offset: u64, count: usize) -> Vec<u8> {
  let mut args = Vec::new();
  nfs::opaque(file, &mut args);
  args.extend_from_slice(&offset.to_be_bytes());
  args.extend_from_slice(&u32::try_from(count).unwrap().to_be_bytes());
  args
}

/// One reply off the connection: its xid and the procedure's results (after the accept status, asserted
/// `SUCCESS`). Panics, naming the cause, if the connection closed or the record is not a whole RPC reply.
fn next_reply(stream: &mut TcpStream) -> (u32, Vec<u8>) {
  let mut marker = [0u8; 4];
  stream
    .read_exact(&mut marker)
    .unwrap_or_else(|e| panic!("the connection stayed open: {e}"));
  let marker = u32::from_be_bytes(marker);
  assert!(
    marker & 0x8000_0000 != 0,
    "each reply is one whole fragment"
  );
  let len = (marker & 0x7fff_ffff) as usize;
  let mut reply = vec![0u8; len];
  stream
    .read_exact(&mut reply)
    .unwrap_or_else(|e| panic!("the connection stayed open mid-record: {e}"));
  let xid = u32::from_be_bytes(reply[0..4].try_into().unwrap());
  assert_eq!(
    u32::from_be_bytes(reply[4..8].try_into().unwrap()),
    1,
    "a reply"
  );
  let verf_len = u32::from_be_bytes(reply[16..20].try_into().unwrap()) as usize;
  let accept = 20 + verf_len + (4 - verf_len % 4) % 4;
  assert_eq!(
    u32::from_be_bytes(reply[accept..accept + 4].try_into().unwrap()),
    0,
    "RPC accepted"
  );
  (xid, reply[accept + 4..].to_vec())
}

/// The payload of write `index` of `round`: every byte a function of both, so a lost, torn or misplaced write shows.
fn payload(round: usize, index: usize) -> Vec<u8> {
  let size = SIZES[index % SIZES.len()];
  (0..size)
    .map(|k| u8::try_from((round * 31 + index * 7 + k) % 251).unwrap() + 1)
    .collect()
}

/// READs `file` whole, in transfer-ceiling pieces, over `stream`.
fn read_whole(stream: &mut TcpStream, file: &[u8], length: usize, first_xid: u32) -> Vec<u8> {
  let mut out = Vec::new();
  let mut xid = first_xid;
  while out.len() < length {
    let count = (length - out.len()).min(SIZES[SIZES.len() - 1]);
    stream
      .write_all(&framed_call(
        READ,
        &read_args(file, out.len() as u64, count),
        xid,
      ))
      .unwrap();
    let (answered, results) = loop {
      let (answered, results) = next_reply(stream);
      if answered == xid {
        break (answered, results);
      }
    };
    assert_eq!(answered, xid);
    assert_eq!(nfs::status(&results), 0, "READ at {}", out.len());
    // READ3resok: status, post_op_attr (follows + 84), count, eof, data.
    let attributes = if u32::from_be_bytes(results[4..8].try_into().unwrap()) == 1 {
      84
    } else {
      0
    };
    let (data, _) = nfs::read_opaque(&results, 8 + attributes + 8);
    assert!(!data.is_empty(), "READ at {} returned bytes", out.len());
    out.extend_from_slice(&data);
    xid += 1;
  }
  out
}

/// A-113, T-4.12. Do: under `slates anchor`, open one NFS connection to a volume's export, then six times pipeline a
/// burst of FILE_SYNC WRITEs (4 KiB to 256 KiB) and `kill -9` the daemon at a different moment inside each burst; read
/// every reply off the same socket, then READ once more. Expect: the connection never closes, every request is
/// answered `NFS3_OK` (duplicates allowed and counted), the READ after each kill is answered by a new daemon, and the
/// file reads back as the model of all the writes.
#[test]
fn an_nfs_connection_outlives_its_daemons_kill_and_every_request_is_answered() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the held-connection flow: set SLATES_TEST_CLI=1 to run it (an anchor and its daemons)"
    );
    return;
  }
  if cfg!(target_os = "linux") {
    eprintln!(
      "skipping the held-connection flow: Linux does not hold NFS connections yet (its kernel may take part of a send; \
       A-113, GAPS)"
    );
    return;
  }
  let instance = format!("nfs-held-{}", std::process::id());
  let _anchor = start_anchor(&instance);
  let mut held = HeldSession::open(&instance);
  let mut longest_wait = Duration::ZERO;
  // Kills that landed with requests still unsent, so a successor answered requests the dead daemon never read: the
  // non-vacuity evidence that a request outlived a daemon in the connection, not merely that the connection did.
  let mut kills_inside_a_burst = 0usize;
  let mut pid = daemon_pid(&instance).expect("a daemon answers");
  for round in 0..KILLS {
    let (waited, inside) = held.kill_inside_a_burst(round, pid);
    longest_wait = longest_wait.max(waited);
    kills_inside_a_burst += usize::from(inside);
    pid = await_new_daemon(&instance, pid, round);
  }
  let whole = read_whole(&mut held.stream, &held.file, held.model.len(), held.xid);
  eprintln!(
    "held connection: {KILLS} kills ({kills_inside_a_burst} with requests still unsent), {} requests, {} duplicate \
     replies, longest kill-to-answer {longest_wait:?}",
    KILLS * BURST,
    held.duplicates
  );
  assert!(
    whole == held.model,
    "the file is the model of every write, byte for byte"
  );
  assert!(
    kills_inside_a_burst > 0,
    "at least one kill landed while the burst was still being sent"
  );
}

/// One NFS connection to a fresh volume's export, its file, and the model of what was written to it.
struct HeldSession {
  stream: TcpStream,
  file: Vec<u8>,
  model: Vec<u8>,
  xid: u32,
  duplicates: usize,
}

impl HeldSession {
  /// Creates a volume at `instance`, exports it, connects to the export's port, mounts it and creates the file.
  fn open(instance: &str) -> HeldSession {
    let (code, out, err) = run(
      instance,
      &["volume", "create", "held", "--bounded", "64MiB"],
    );
    assert_eq!(code, 0, "create: {err}");
    let id = out
      .lines()
      .find_map(|line| line.strip_prefix("id: ").map(str::to_owned))
      .unwrap_or_else(|| panic!("an id in {out}"));
    let (code, out, err) = run(instance, &["export", &id, "--json"]);
    assert_eq!(code, 0, "export: {err}");
    let (path, port) = (json_field(&out, "path"), json_field(&out, "port"));
    let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    stream.set_read_timeout(Some(START_WAIT)).unwrap();
    let root = nfs::mount(&mut stream, &path, 1);
    let file = nfs::create(&mut stream, &root, "burst", 2);
    HeldSession {
      stream,
      file,
      model: Vec::new(),
      xid: 100,
      duplicates: 0,
    }
  }

  /// The framed WRITEs of `round`'s burst, appended to the model, and the xids they carry.
  fn burst(&mut self, round: usize) -> (Vec<u8>, BTreeMap<u32, ()>) {
    let mut burst = Vec::new();
    let mut expected = BTreeMap::new();
    for index in 0..BURST {
      let data = payload(round, index);
      let offset = self.model.len();
      self.model.extend_from_slice(&data);
      burst.extend_from_slice(&framed_call(
        WRITE,
        &write_args(&self.file, offset as u64, &data),
        self.xid,
      ));
      expected.insert(self.xid, ());
      self.xid += 1;
    }
    (burst, expected)
  }

  /// Sends `round`'s burst from a writer thread, kills daemon `pid` partway in, reads every reply, then READs once more:
  /// how long from the kill until that READ was answered, and whether the kill landed with requests still unsent.
  fn kill_inside_a_burst(&mut self, round: usize, pid: u32) -> (Duration, bool) {
    let (burst, expected) = self.burst(round);
    // The writer sends the burst while the daemon is killed: a burst larger than the connection's buffers waits on the
    // server, so it cannot be sent from the thread that kills.
    let mut writer = self.stream.try_clone().unwrap();
    let sending = std::thread::spawn(move || writer.write_all(&burst).map(|()| Instant::now()));
    // A different moment inside each burst: from at once to a few milliseconds in.
    #[allow(clippy::disallowed_methods)] // the test places the kill inside the burst
    std::thread::sleep(Duration::from_micros(((round * 1_337) % 4_000) as u64));
    let killed_at = Instant::now();
    let killed = Command::new("kill")
      .args(["-9", &pid.to_string()])
      .status()
      .unwrap();
    assert!(killed.success(), "kill the daemon");
    self.read_burst_replies(&expected);
    let sent_at = sending.join().unwrap().unwrap();
    // A READ after the kill: only a daemon started after it can answer, on the connection the dead one left.
    let probe_length = SIZES[0].min(self.model.len());
    let probe = read_whole(&mut self.stream, &self.file, probe_length, self.xid);
    self.xid += 1;
    assert_eq!(
      probe,
      self.model[..probe.len()],
      "the probe READ after kill {round}"
    );
    (killed_at.elapsed(), sent_at > killed_at)
  }

  /// Reads replies until every xid in `expected` is answered `NFS3_OK`, counting replies that arrive twice.
  fn read_burst_replies(&mut self, expected: &BTreeMap<u32, ()>) {
    let mut answered: BTreeMap<u32, ()> = BTreeMap::new();
    while answered.len() < expected.len() {
      let (reply_xid, results) = next_reply(&mut self.stream);
      assert!(
        expected.contains_key(&reply_xid),
        "a reply to a request of this burst: {reply_xid}"
      );
      assert_eq!(
        nfs::status(&results),
        0,
        "WRITE {reply_xid} answered NFS3_OK"
      );
      if answered.insert(reply_xid, ()).is_some() {
        self.duplicates += 1;
      }
    }
  }
}

/// Waits, bounded by the start wait, for a daemon other than `killed` to answer at `instance`; its pid.
fn await_new_daemon(instance: &str, killed: u32, round: usize) -> u32 {
  let started = Instant::now();
  loop {
    if let Some(next) = daemon_pid(instance).filter(|next| *next != killed) {
      return next;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "a new daemon after kill {round}"
    );
    pause();
  }
}
