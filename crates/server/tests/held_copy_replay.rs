//! A writable direct map's copy (A-197) outlives its daemon with the store its client made after a logged write (§4.6;
//! A-389), with this test as the anchor. The test makes the anchor's hold channel and hands it to each daemon as the
//! anchor does, the held copy to the second (the handoff table `SLATES_ANCHOR_HELD` names, A-462), which a daemon adopts
//! once per process into a process-wide cell: hence a binary of its own with one test. A daemon's death is a fenced
//! stop, which ends no record and releases no hold, as a killed process does. Linux alone: the anchor holds a writable
//! copy there alone (A-197).

#![cfg(target_os = "linux")]
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::{Read, Write};
use std::os::fd::{AsFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use slates_anchor::held::{self, Incoming};
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, ReadAt, SizeClass,
};
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::anchor::{held_anchor, source_of};

/// Shape: how long the test waits for a daemon, its client, a message to the anchor, a counter or a frame's answer.
const WAIT: Duration = Duration::from_secs(20);
/// Format: the FUSE opcodes the stream's guest sends (`include/uapi/linux/fuse.h`).
const LOOKUP: u32 = 1;
const OPEN: u32 = 14;
const WRITE: u32 = 16;
/// Format: a FUSE request's in-header, and the reply's out-header (length, errno, unique).
const IN_HEADER_LEN: usize = 40;
const OUT_HEADER_LEN: usize = 16;
/// Format: `open(2)`'s `O_RDWR`.
const READ_WRITE: u32 = 0o2;
/// Shape: the file's bytes before it is mapped: two pages, so every offset the test touches lies inside the file.
const FILE_BYTES: usize = 8192;
/// Shape: where the stream writes and the client then stores, inside the file.
const AT: usize = 4096;
/// Shape: the bytes the stream writes and the client then stores over, at [`AT`].
const SPAN: usize = 48;
/// What the stream writes after the last publication: logged, never published before the death.
const LOGGED: &[u8] = &[b'L'; SPAN];
/// What the client then stores over it through its map, never synced before the death: every byte differs from
/// [`LOGGED`]'s, so a write-back of the store writes all of them.
const STORED: &[u8] = &[b's'; SPAN];
/// Shape: where another client writes after the takeover, inside the file and apart from [`AT`]'s span.
const AFTER_AT: usize = 6000;
/// What another client writes there after the takeover.
const AFTER: &[u8] = &[b'a'; SPAN];

fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < WAIT =>
      {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// The anchor's next message from a daemon, waited for up to [`WAIT`].
fn next_held(anchor: &OwnedFd) -> Incoming {
  let started = Instant::now();
  loop {
    if let Some(message) = held::receive(anchor.as_fd()).unwrap() {
      return message;
    }
    assert!(
      started.elapsed() < WAIT,
      "the daemon told the anchor nothing"
    );
    std::thread::yield_now();
  }
}

/// What the anchor keeps of the writable copy: its id, record and object.
struct HeldCopy {
  copy: u64,
  record: Vec<u8>,
  object: OwnedFd,
}

/// Waits until `daemon` has counted `kind` at least `at_least` times, or [`WAIT`] passes: the count then.
fn counted(daemon: &Daemon, kind: &str, at_least: u64) -> u64 {
  let started = Instant::now();
  loop {
    let count = daemon
      .refusals_on_every_shard()
      .unwrap()
      .get(kind)
      .copied()
      .unwrap_or(0);
    if count >= at_least || started.elapsed() > WAIT {
      return count;
    }
    std::thread::yield_now();
  }
}

/// A copy of `fd` the daemon about to start may own.
fn copy(fd: &OwnedFd) -> i32 {
  rustix::io::fcntl_dupfd_cloexec(fd, 3)
    .unwrap()
    .into_raw_fd()
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A FUSE request: the in-header (length, opcode, unique, node; the caller's ids zero) and `body`.
fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let mut request = vec![0u8; IN_HEADER_LEN];
  request[0..4].copy_from_slice(
    &u32::try_from(IN_HEADER_LEN + body.len())
      .unwrap()
      .to_le_bytes(),
  );
  request[4..8].copy_from_slice(&opcode.to_le_bytes());
  request[8..16].copy_from_slice(&unique.to_le_bytes());
  request[16..24].copy_from_slice(&nodeid.to_le_bytes());
  request.extend_from_slice(body);
  request
}

/// The guest's end of a Shards stream: Shards' framing over it.
struct Guest {
  stream: UnixStream,
  unique: u64,
}

impl Guest {
  fn over(end: OwnedFd) -> Guest {
    let stream = UnixStream::from(end);
    stream.set_read_timeout(Some(WAIT)).unwrap();
    Guest { stream, unique: 0 }
  }

  /// Sends `opcode` on `node` with `body`, under the next unique, and reads its reply: its errno (as Linux names it)
  /// and its body.
  fn ask(&mut self, opcode: u32, node: u64, body: &[u8]) -> (i32, Vec<u8>) {
    self.unique += 1;
    let message = request(opcode, self.unique, node, body);
    let mut frame = u32::try_from(message.len()).unwrap().to_le_bytes().to_vec();
    frame.extend_from_slice(&message);
    self.stream.write_all(&frame).unwrap();
    let mut length = [0u8; 4];
    self.stream.read_exact(&mut length).unwrap();
    let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(length)).unwrap()];
    self.stream.read_exact(&mut reply).unwrap();
    let errno = -i32::from_le_bytes(reply[4..8].try_into().unwrap());
    (errno, reply[OUT_HEADER_LEN..].to_vec())
  }
}

/// Before the death: a Shards stream's guest writes [`LOGGED`] at [`AT`] of the mapped file, acknowledged after the
/// last publication and so logged (A-63), and the client's map shows it, written through by the volume's write
/// boundary. The stream and its resync end are returned, kept open until the death: a stream's end ends its attachment,
/// which publishes.
fn a_stream_writes_after_the_last_publication(
  client: &mut Client,
  volume: slates_client::VolumeId,
  anchor: &OwnedFd,
  mapped: &slates_client::MappedWrite,
) -> (Guest, OwnedFd) {
  let shards = client.attach_shards(volume, Intent::Write, None).unwrap();
  // The stream's hold and session at the anchor: taken, and never handed to the successor, so the stream ends there.
  assert!(matches!(next_held(anchor), Incoming::Hold { .. }));
  assert!(matches!(next_held(anchor), Incoming::Session { .. }));
  let mut guest = Guest::over(shards.stream);
  let (errno, body) = guest.ask(LOOKUP, 1, b"mapped\0");
  assert_eq!(errno, 0, "the stream finds the mapped file");
  let node = u64::from_le_bytes(body[0..8].try_into().unwrap());
  let (errno, body) = guest.ask(OPEN, node, &[READ_WRITE.to_le_bytes(), [0; 4]].concat());
  assert_eq!(errno, 0, "the stream opens it");
  let fh = u64::from_le_bytes(body[0..8].try_into().unwrap());
  let mut write = Vec::new();
  write.extend_from_slice(&fh.to_le_bytes());
  write.extend_from_slice(&u64::try_from(AT).unwrap().to_le_bytes());
  write.extend_from_slice(&u32::try_from(LOGGED.len()).unwrap().to_le_bytes());
  write.extend_from_slice(&[0u8; 20]);
  write.extend_from_slice(LOGGED);
  let (errno, _) = guest.ask(WRITE, node, &write);
  assert_eq!(errno, 0, "the stream's write is acknowledged");
  let mut shown = vec![0u8; LOGGED.len()];
  mapped
    .read_at(u64::try_from(AT).unwrap(), &mut shown)
    .unwrap();
  assert_eq!(
    shown, LOGGED,
    "the client's map shows the stream's write, written through"
  );
  (
    guest,
    shards
      .resync
      .expect("the daemon hands a resync end (Linux)"),
  )
}

/// A-389 (A-197's copy through the write log's replay). Do: as the anchor, hand a daemon the hold channel; write a file
/// through the SDK, map it writable from a client, and take the copy the daemon holds at the anchor; write over a range
/// of it through a Shards stream (acknowledged after the last publication, so logged) and see the client's map show
/// that write; store over the same range through the map, never syncing; kill the daemon (a fenced stop); start a
/// second daemon over the same segment with the held copy handed back; then sync the map from the same client, read the
/// file through another and write it whole there with a third span. Expect: the copy taken back; the client's map still
/// holding its store after the successor's recovery replayed the logged write beneath it; the sync writing the store;
/// the file holding the store, the later of the two writes; and the other client's write after the takeover in the map,
/// the copy woken after the replay. Before A-389 the copy was taken back awake before the replay, which wrote the
/// logged bytes over the store in the client's own map, so the sync found nothing to write and the store was lost.
#[test]
fn a_clients_store_after_a_logged_write_outlives_that_writes_replay() {
  let (anchor, daemon_end) = held::channel().unwrap();
  // SAFETY: this binary holds this one test; the variable is set before any daemon starts.
  unsafe {
    std::env::set_var(
      held::ENV_HELD,
      held::hand_in_process(&held::Table {
        devices: daemon_end
          .as_fd()
          .try_clone_to_owned()
          .unwrap()
          .into_raw_fd()
          .to_string(),
        ..held::Table::default()
      })
      .unwrap(),
    );
  }
  let profile = common::machine_profile();
  let instance = format!("srv-held-copy-replay-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = held_anchor("held-copy-replay", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: "held-copy-replay".to_owned(),
      size: SizeClass::Bounded { limit: 64 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "/mapped", &[b'w'; FILE_BYTES], 0o644)
    .unwrap();
  let mapped = client
    .map_write(
      (volume, attachment),
      "/mapped",
      slates_client::page_bytes().unwrap(),
    )
    .unwrap();
  let Incoming::HoldCopy {
    copy: copy_id,
    record,
    object,
  } = next_held(&anchor)
  else {
    panic!("the writable copy was not held");
  };
  let held = HeldCopy {
    copy: copy_id,
    record,
    object,
  };
  let (stream, resync) =
    a_stream_writes_after_the_last_publication(&mut client, volume, &anchor, &mapped);
  assert_eq!(
    mapped.write_at(u64::try_from(AT).unwrap(), STORED).unwrap(),
    STORED.len(),
    "the map takes the store"
  );
  first.inject_consensus_failure().unwrap();
  first.stop();

  let devices = copy(&daemon_end).to_string();
  let copies = format!(
    "{:x}:{}:{}",
    held.copy,
    copy(&held.object),
    hex(&held.record)
  );
  // SAFETY: this binary holds this one test; it sets the variables only between daemons, each of which reads them once
  // at its start, and no other thread of this process reads the environment meanwhile.
  unsafe {
    std::env::set_var(
      held::ENV_HELD,
      held::hand_in_process(&held::Table {
        devices,
        connections: String::new(),
        copies,
      })
      .unwrap(),
    );
  }
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  assert_eq!(
    counted(&second, "dax.local_map_adopted", 1),
    1,
    "the copy is taken back"
  );
  let mut shown = vec![0u8; STORED.len()];
  mapped
    .read_at(u64::try_from(AT).unwrap(), &mut shown)
    .unwrap();
  assert_eq!(
    shown, STORED,
    "the client's map still holds its store after the successor replayed the logged write beneath it"
  );
  assert_eq!(
    client.map_sync(&mapped).unwrap(),
    u64::try_from(STORED.len()).unwrap(),
    "the sync writes the store over the replayed write"
  );
  let mut reader = connect(&instance);
  let mut file = reader.read(volume, "/mapped", ReadAt::Head).unwrap();
  assert_eq!(
    &file[AT..AT + STORED.len()],
    STORED,
    "the file holds the store, the later of the two writes"
  );
  file[AFTER_AT..AFTER_AT + AFTER.len()].copy_from_slice(AFTER);
  let writer = reader
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  reader
    .fs_write((volume, writer), "/mapped", &file, 0o644)
    .unwrap();
  let mut shown = vec![0u8; AFTER.len()];
  mapped
    .read_at(u64::try_from(AFTER_AT).unwrap(), &mut shown)
    .unwrap();
  assert_eq!(
    shown, AFTER,
    "another client's write after the takeover reaches the map: the copy woke after the replay"
  );
  reader.detach(writer).unwrap();
  drop(reader);
  drop(mapped);
  drop(client);
  drop(stream);
  drop(resync);
  second.stop();
  drop(held);
  drop(segment);
  drop(anchor);
}
