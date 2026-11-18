//! A Shards stream's acknowledged write outlives its daemon's death through the shard's write log (§4.6, the Shards
//! form; A-63, A-316, A-402), on every platform: the test holds the anchor's segment and content object across two
//! in-process daemons, attaches a volume in the Shards form through a client as a Shards spawner does, and speaks FUSE
//! through the stream end it was handed. A daemon's death is a fenced stop, which publishes nothing, as a killed process
//! does. Before A-402 Windows kept no write log, so a stream's write acknowledged after the last publication was lost
//! there at a death.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::reply::EntryOut;
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, ReadAt, SizeClass,
};
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::anchor::{held_anchor, source_of};

/// Shape: how long the test waits for a daemon, its client or a frame's answer.
const WAIT: Duration = Duration::from_secs(20);
/// Format: `open(2)`'s `O_RDWR | O_CREAT | O_EXCL` and `S_IFREG | 0644`, a CREATE's flags and mode.
const CREATE_EXCLUSIVE: u32 = 0o302;
const FILE_MODE: u32 = 0o100_644;
/// Format: the FUSE node id of the attachment's root.
const ROOT: u64 = 1;
/// The bytes the guest writes after the last publication: acknowledged, never published before the death.
const WRITTEN: &[u8] = b"acknowledged by the stream after the last publication, before the death";

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

/// The end the client received, as Shards' device reads and writes it: a Unix socket with the test's read bound.
#[cfg(unix)]
type Wire = std::os::unix::net::UnixStream;

/// The end the client received on Windows: the client end of the attachment's pipe, opened for overlapped I/O, so each
/// read and write waits on an `OVERLAPPED` of its own (`slates_ipc::stream::read_waiting`, `write_waiting`).
#[cfg(windows)]
struct Wire(slates_client::StreamEnd);

#[cfg(windows)]
impl Read for Wire {
  fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::windows::io::AsHandle;
    slates_ipc::stream::read_waiting(self.0.as_handle(), buf).map_err(std::io::Error::other)
  }
}

#[cfg(windows)]
impl Write for Wire {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    use std::os::windows::io::AsHandle;
    slates_ipc::stream::write_waiting(self.0.as_handle(), buf).map_err(std::io::Error::other)
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

/// The received end as a [`Wire`].
#[cfg(unix)]
fn wire(end: slates_client::StreamEnd) -> Wire {
  let stream = std::os::unix::net::UnixStream::from(end);
  stream.set_read_timeout(Some(WAIT)).unwrap();
  stream
}

/// The received end as a [`Wire`].
#[cfg(windows)]
fn wire(end: slates_client::StreamEnd) -> Wire {
  Wire(end)
}

/// Shards' fs device over the end the client received: its framing, a little-endian length then the request.
struct Guest {
  stream: Wire,
  unique: u64,
}

impl Guest {
  /// Sends `opcode` on `node` with `body` under the next unique and reads its reply: its errno (as Linux names it) and
  /// its body.
  fn ask(&mut self, opcode: Opcode, node: u64, body: &[u8]) -> (i32, Vec<u8>) {
    self.unique += 1;
    let total = IN_HEADER_LEN + body.len();
    let mut request = vec![0u8; IN_HEADER_LEN];
    request[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
    request[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
    request[8..16].copy_from_slice(&self.unique.to_le_bytes());
    request[16..24].copy_from_slice(&node.to_le_bytes());
    request.extend_from_slice(body);
    let mut frame = u32::try_from(request.len()).unwrap().to_le_bytes().to_vec();
    frame.extend_from_slice(&request);
    self.stream.write_all(&frame).unwrap();
    let mut length = [0u8; 4];
    self.stream.read_exact(&mut length).unwrap();
    let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(length)).unwrap()];
    self.stream.read_exact(&mut reply).unwrap();
    let errno = -i32::from_le_bytes(reply[4..8].try_into().unwrap());
    (errno, reply[OUT_HEADER_LEN..].to_vec())
  }
}

/// Before the death: the guest's kernel answers INIT, creates `kept` (whose barrier publishes it) and writes
/// [`WRITTEN`] into it, acknowledged after that last publication.
fn the_guest_writes_after_the_last_publication(guest: &mut Guest) {
  let mut init = Vec::new();
  for word in [7u32, 31, 128 << 10, 0] {
    init.extend_from_slice(&word.to_le_bytes());
  }
  assert_eq!(guest.ask(Opcode::Init, 0, &init).0, 0, "INIT");
  let mut create = Vec::new();
  for word in [CREATE_EXCLUSIVE, FILE_MODE, 0, 0] {
    create.extend_from_slice(&word.to_le_bytes());
  }
  create.extend_from_slice(b"kept\0");
  let (errno, body) = guest.ask(Opcode::Create, ROOT, &create);
  assert_eq!(errno, 0, "the guest creates the file");
  let node = u64::from_le_bytes(body[0..8].try_into().unwrap());
  let fh = u64::from_le_bytes(body[EntryOut::LEN..EntryOut::LEN + 8].try_into().unwrap());
  let mut write = Vec::new();
  write.extend_from_slice(&fh.to_le_bytes());
  write.extend_from_slice(&0u64.to_le_bytes());
  write.extend_from_slice(&u32::try_from(WRITTEN.len()).unwrap().to_le_bytes());
  write.extend_from_slice(&[0u8; 20]);
  write.extend_from_slice(WRITTEN);
  assert_eq!(
    guest.ask(Opcode::Write, node, &write).0,
    0,
    "the write is acknowledged"
  );
}

/// A-402 (A-63 and A-316 on every platform). Do: as the anchor, hold the segment and its content object; start a daemon
/// over them and attach a volume in the Shards form through a client; through the stream's end create a file, its
/// barrier publishing it, and write into it, acknowledged after that last publication; kill the daemon (a fenced
/// stop: nothing published); start a second daemon over the same segment and read the file through a client. Expect the
/// write read back, replayed from the shard's write log. Before A-402 Windows kept no log, so the file came back as its
/// create's publication left it, empty.
#[test]
fn a_streams_acknowledged_write_outlives_its_daemon_through_the_write_log() {
  let profile = common::machine_profile();
  let instance = format!("srv-stream-write-log-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = held_anchor("stream-write-log", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: "stream-write-log".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let shards = client.attach_shards(volume, Intent::Write, None).unwrap();
  // The device's end of the stream's resync channel, where the platform hands one: kept open until the death.
  let resync = shards.resync;
  let mut guest = Guest {
    stream: wire(shards.stream),
    unique: 0,
  };
  the_guest_writes_after_the_last_publication(&mut guest);
  first.inject_consensus_failure().unwrap();
  first.stop();
  drop(guest);
  drop(resync);
  drop(client);

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let mut reader = connect(&instance);
  assert_eq!(
    reader.read(volume, "/kept", ReadAt::Head).unwrap(),
    WRITTEN,
    "the acknowledged write outlived the daemon, replayed from the write log"
  );
  drop(reader);
  second.stop();
  drop(segment);
}
