//! The daemon serves a volume to a Shards microVM's fs device over an anonymous stream (§4.6, the Shards route; A-161).
//! A single-shard daemon is started, a client provisions a volume through the real rendezvous, and the test attaches
//! the volume in-process (`Daemon::attach_shards_stream`), takes the stream end a VMM would be handed, and speaks to it
//! exactly as Shards' fs device does: each guest FUSE request framed as a little-endian length then the request, one
//! reply frame read back per request, and the device's own control asks framed with the length word's top bit. The
//! host side of every check goes through the daemon's own NFS loopback port, the host mount path: one VFS, two
//! transports, byte for byte both ways.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result,
  clippy::panic_in_result_fn
)]
// The Shards transport rides the runtime's Unix descriptor readiness.
#![cfg(unix)]

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::reply::EntryOut;
use slates_db::catalog::Principal;
use slates_ipc::protocol::{
  Direction, Intent, NamePolicy, Refusal, ReplyBody, RequestBody, SizeClass, VolumeId, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_server::shards::{ShardsAsk, ShardsAttached};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;
use common::anchor::{anchor_segment, source_of};
use common::nfs::{create, lookup, lookup_status, mkdir, mount, read, remove_status, write};

/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon, a full ring, an attach's answer or a stream's reply before the test
/// fails.
const CREDIT_WAIT: Duration = Duration::from_secs(5);
/// Format: the length word's bit that marks Shards' own control frames (`shards_fs::CONTROL_FRAME`).
const CONTROL: u32 = 1 << 31;
/// Format: Shards' control asks (`shards_fs::ASK_*`).
const ASK_SAVE: u8 = 1;
const ASK_RESTORE: u8 = 2;
const ASK_DESCRIPTOR: u8 = 3;
/// Format: Linux errnos a reply carries, negated.
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EOPNOTSUPP: i32 = 95;
/// Format: `FATTR_SIZE`, a SETATTR's bit for the size.
const FATTR_SIZE: u32 = 1 << 3;
/// Format: `O_RDWR`, an open's access mode.
const O_RDWR: u32 = 2;
/// Format: `O_RDONLY`.
const O_RDONLY: u32 = 0;
/// Format: the FUSE node id of the attachment's root.
const ROOT: u64 = 1;
/// Format: the FUSE protocol version a guest kernel offers (7.31, Linux 5.4's).
const MAJOR: u32 = 7;
const MINOR: u32 = 31;
/// Format: the `fuse_init_out` offset of `max_write`: major, minor, max_readahead, flags, max_background (u16),
/// congestion_threshold (u16), then max_write.
const INIT_MAX_WRITE_AT: usize = 20;
/// Shape: the bytes of the file a guest holds when its stream closes: several chunk windows, so their release is
/// plain in the volume's referenced bytes.
const HELD_BYTES: usize = 256 * 1024;
/// The bytes a guest writes and the host reads back.
const FROM_GUEST: &[u8] =
  b"written by a Shards guest through the stream, read back by the host over NFS\n";
/// The bytes the host writes and a guest reads back.
const FROM_HOST: &[u8] =
  b"written by the host over NFS, read back by a Shards guest through the stream\n";

// --- The client side of the daemon's own rendezvous (as in tests/virtiofs.rs). ---

struct Client {
  end: ClientEnd,
  client: u32,
  sequence: u32,
}

impl Client {
  fn connect(instance: &str) -> Client {
    let started = Instant::now();
    loop {
      match connect(instance) {
        Ok(connected) => {
          let client = connected.region.client_id();
          return Client {
            end: ClientEnd::connected(connected),
            client,
            sequence: 0,
          };
        }
        Err(IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
          std::hint::spin_loop();
        }
        Err(e) => panic!("{e}"),
      }
    }
  }

  fn call(&mut self, body: &RequestBody) -> ReplyBody {
    self.sequence += 1;
    let id = RequestId {
      client: self.client,
      sequence: self.sequence,
    };
    let index = self.end.next_request_index();
    let slot = pack(
      self.end.region_mut(),
      Direction::Request,
      index,
      id.word(),
      body,
    )
    .unwrap();
    let started = Instant::now();
    loop {
      match self.end.send(&slot) {
        Ok(()) => break,
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::hint::spin_loop(),
        Err(e) => panic!("{e}"),
      }
    }
    let reply = self.end.wait(Some(DEADLINE_NS)).unwrap();
    unpack(self.end.region(), reply.kind, &reply.payload).unwrap()
  }
}

fn single_shard_daemon(name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn scratch(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// The uid this test runs as: the principal the daemon's rendezvous established for the client, hence the volume's
/// owner.
fn my_uid() -> u32 {
  rustix::process::getuid().as_raw()
}

/// Attaches `volume` over a stream as this test's user, presenting `subtree` with `intent`: the attachment, or the
/// typed refusal.
fn attach(
  daemon: &Daemon,
  volume: VolumeId,
  subtree: Option<&str>,
  intent: Intent,
) -> Result<ShardsAttached, Refusal> {
  let (tx, rx) = channel();
  daemon
    .attach_shards_stream(
      ShardsAsk {
        principal: Principal::Uid { uid: my_uid() },
        volume,
        subtree: subtree.map(str::to_owned),
        intent,
      },
      Box::new(move |attached| {
        let _ = tx.send(attached);
      }),
    )
    .unwrap();
  rx.recv_timeout(CREDIT_WAIT).expect("the attach answered")
}

// --- Shards' fs device, as seen from its socket. ---

/// The VMM side of a stream: Shards' framing over the end it was handed.
struct Guest {
  stream: UnixStream,
  unique: u64,
}

impl Guest {
  fn over(attached: ShardsAttached) -> Guest {
    let stream = UnixStream::from(attached.stream);
    stream.set_read_timeout(Some(CREDIT_WAIT)).unwrap();
    Guest { stream, unique: 0 }
  }

  /// Sends one guest request and returns its reply's bytes (empty for a request that takes none).
  fn ask(&mut self, opcode: Opcode, nodeid: u64, body: &[u8]) -> Vec<u8> {
    self.unique += 1;
    let request = message(opcode.to_wire(), self.unique, nodeid, body);
    self.send_frame(0, &request);
    self.reply_frame().expect("the daemon answered").1
  }

  /// Writes one frame: the length word (with `flag` set in it) and `bytes`.
  fn send_frame(&mut self, flag: u32, bytes: &[u8]) {
    let word = u32::try_from(bytes.len()).unwrap() | flag;
    self.stream.write_all(&word.to_le_bytes()).unwrap();
    self.stream.write_all(bytes).unwrap();
  }

  /// Reads one frame: whether it is a control frame, and its bytes; `None` at the stream's end.
  fn reply_frame(&mut self) -> Option<(bool, Vec<u8>)> {
    let mut word = [0u8; 4];
    match self.stream.read_exact(&mut word) {
      Ok(()) => {}
      Err(e) if e.kind() == ErrorKind::UnexpectedEof => return None,
      Err(e) => panic!("reading a reply frame: {e}"),
    }
    let word = u32::from_le_bytes(word);
    let mut bytes = vec![0u8; usize::try_from(word & !CONTROL).unwrap()];
    self.stream.read_exact(&mut bytes).unwrap();
    Some((word & CONTROL != 0, bytes))
  }

  /// Whether the daemon has ended the stream: its next read answers the end (nothing is owed to this guest).
  fn ended(&mut self) -> bool {
    let mut byte = [0u8; 1];
    matches!(self.stream.read(&mut byte), Ok(0))
  }

  fn init(&mut self) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&MAJOR.to_le_bytes());
    body.extend_from_slice(&MINOR.to_le_bytes());
    body.extend_from_slice(&(128u32 << 10).to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let reply = self.ask(Opcode::Init, 0, &body);
    assert_eq!(errno(&reply), 0, "INIT answered");
    reply
  }

  fn lookup(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
    let reply = self.ask(Opcode::Lookup, parent, &named(name));
    match errno(&reply) {
      0 => Ok(u64_at(&reply, OUT_HEADER_LEN)),
      error => Err(error),
    }
  }

  /// Creates `name` in `parent` open for reading and writing: its node and handle, or the errno.
  fn create(&mut self, parent: u64, name: &str) -> Result<(u64, u64), i32> {
    let mut body = Vec::new();
    body.extend_from_slice(&O_RDWR.to_le_bytes());
    body.extend_from_slice(&0o644u32.to_le_bytes());
    body.extend_from_slice(&[0u8; 8]);
    body.extend_from_slice(&named(name));
    let reply = self.ask(Opcode::Create, parent, &body);
    match errno(&reply) {
      0 => Ok((
        u64_at(&reply, OUT_HEADER_LEN),
        u64_at(&reply, OUT_HEADER_LEN + EntryOut::LEN),
      )),
      error => Err(error),
    }
  }

  fn mkdir(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
    let mut body = Vec::new();
    body.extend_from_slice(&0o755u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&named(name));
    let reply = self.ask(Opcode::MkDir, parent, &body);
    match errno(&reply) {
      0 => Ok(u64_at(&reply, OUT_HEADER_LEN)),
      error => Err(error),
    }
  }

  fn open(&mut self, node: u64, flags: u32) -> Result<u64, i32> {
    let mut body = flags.to_le_bytes().to_vec();
    body.extend_from_slice(&0u32.to_le_bytes());
    let reply = self.ask(Opcode::Open, node, &body);
    match errno(&reply) {
      0 => Ok(u64_at(&reply, OUT_HEADER_LEN)),
      error => Err(error),
    }
  }

  fn write(&mut self, node: u64, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
    let mut body = Vec::new();
    body.extend_from_slice(&fh.to_le_bytes());
    body.extend_from_slice(&offset.to_le_bytes());
    body.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0u8; 20]);
    body.extend_from_slice(data);
    let reply = self.ask(Opcode::Write, node, &body);
    match errno(&reply) {
      0 => Ok(u32::from_le_bytes(
        reply[OUT_HEADER_LEN..OUT_HEADER_LEN + 4]
          .try_into()
          .unwrap(),
      )),
      error => Err(error),
    }
  }

  fn read(&mut self, node: u64, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
    let mut body = Vec::new();
    body.extend_from_slice(&fh.to_le_bytes());
    body.extend_from_slice(&offset.to_le_bytes());
    body.extend_from_slice(&size.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&O_RDONLY.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let reply = self.ask(Opcode::Read, node, &body);
    match errno(&reply) {
      0 => Ok(reply[OUT_HEADER_LEN..].to_vec()),
      error => Err(error),
    }
  }

  /// A `close`: the flush every close sends, then the release.
  fn close(&mut self, node: u64, fh: u64) -> (i32, i32) {
    let mut flush = fh.to_le_bytes().to_vec();
    flush.extend_from_slice(&[0u8; 16]);
    let flushed = errno(&self.ask(Opcode::Flush, node, &flush));
    let mut release = fh.to_le_bytes().to_vec();
    release.extend_from_slice(&[0u8; 16]);
    let released = errno(&self.ask(Opcode::Release, node, &release));
    (flushed, released)
  }

  /// A SETATTR of the size alone (`FATTR_SIZE`): the errno.
  fn truncate(&mut self, node: u64, size: u64) -> i32 {
    // `fuse_setattr_in`: valid, padding, fh, size, lock_owner, atime, mtime, ctime, three nsec words, mode, unused,
    // uid, gid, unused.
    let mut body = Vec::new();
    body.extend_from_slice(&FATTR_SIZE.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&size.to_le_bytes());
    body.extend_from_slice(&[0u8; 8 * 4 + 4 * 4 + 4 * 4]);
    errno(&self.ask(Opcode::SetAttr, node, &body))
  }

  fn rename(&mut self, parent: u64, from: &str, to_parent: u64, to: &str) -> i32 {
    let mut body = to_parent.to_le_bytes().to_vec();
    body.extend_from_slice(&named(from));
    body.extend_from_slice(&named(to));
    errno(&self.ask(Opcode::Rename, parent, &body))
  }

  fn unlink(&mut self, parent: u64, name: &str) -> i32 {
    errno(&self.ask(Opcode::Unlink, parent, &named(name)))
  }

  fn forget(&mut self, node: u64, lookups: u64) {
    self.unique += 1;
    let request = message(
      Opcode::Forget.to_wire(),
      self.unique,
      node,
      &lookups.to_le_bytes(),
    );
    self.send_frame(0, &request);
    let (control, reply) = self.reply_frame().expect("the daemon answered");
    assert!(!control);
    assert!(reply.is_empty(), "a FORGET takes no reply: an empty frame");
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

/// A name as a FUSE request carries it: its bytes, then a NUL.
fn named(name: &str) -> Vec<u8> {
  let mut bytes = name.as_bytes().to_vec();
  bytes.push(0);
  bytes
}

/// The errno a reply carries, positive; zero for success.
fn errno(reply: &[u8]) -> i32 {
  -i32::from_le_bytes(reply[4..8].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
  u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The host's side: the daemon's NFS loopback export of volume `name`, mounted.
fn host_mount(daemon: &Daemon, name: &str) -> (TcpStream, Vec<u8>) {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  // Every mount presents a capability (AUD-01); the daemon mints the owner's for the test.
  let capability = daemon
    .mount_capability(name)
    .expect("the owner shard answers")
    .expect("the volume is served");
  let root = mount(&mut stream, &capability, 1);
  (stream, root)
}

/// Waits until the daemon has counted `kind` at least `at_least` times on any shard, within the credit wait.
fn wait_for_count(daemon: &Daemon, kind: &str, at_least: u64) -> u64 {
  let started = Instant::now();
  loop {
    let counted = daemon
      .refusals_on_every_shard()
      .unwrap()
      .get(kind)
      .copied()
      .unwrap_or(0);
    if counted >= at_least || started.elapsed() > CREDIT_WAIT {
      return counted;
    }
    // The test thread polls the daemon's answer; no runtime wheel serves this thread (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(1));
  }
}

/// A single-shard daemon with one scratch volume, `shared`, and a client of its rendezvous.
fn provisioned(name: &str) -> (Daemon, Client, VolumeId) {
  let (daemon, instance) = single_shard_daemon(name);
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("shared")) else {
    panic!("the volume was not created");
  };
  (daemon, client, id)
}

/// Asserts the daemon counted none of `kinds` on any shard.
fn none_counted(daemon: &Daemon, kinds: &[&str]) {
  let refusals = daemon.refusals_on_every_shard().unwrap();
  for kind in kinds {
    assert_eq!(refusals.get(kind), None, "{kind}: {refusals:?}");
  }
}

/// Polls `done` until it holds, failing the test past the credit wait.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
  let started = Instant::now();
  while !done() {
    assert!(started.elapsed() < CREDIT_WAIT, "{what}");
    // The test thread polls the daemon's answer; no runtime wheel serves this thread (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(1));
  }
}

/// The guest creates `name` in `parent`, writes `bytes` and closes it, every reply a success: the file's node.
fn guest_writes(guest: &mut Guest, parent: u64, name: &str, bytes: &[u8]) -> u64 {
  let (file, fh) = guest.create(parent, name).unwrap();
  assert_eq!(
    guest.write(file, fh, 0, bytes),
    Ok(u32::try_from(bytes.len()).unwrap())
  );
  assert_eq!(guest.close(file, fh), (0, 0));
  file
}

/// The `max_write` an `INIT` reply names.
fn max_write_of(init: &[u8]) -> u32 {
  let at = OUT_HEADER_LEN + INIT_MAX_WRITE_AT;
  u32::from_le_bytes(init[at..at + 4].try_into().unwrap())
}

/// The guest reads through the stream, then forgets, the file `name` the host wrote over NFS.
fn the_guest_reads_what_the_host_wrote(guest: &mut Guest, host: &mut TcpStream, root: &[u8]) {
  let from_host = create(host, root, "from-host.txt", 4);
  write(host, &from_host, FROM_HOST, 5);
  let node = guest.lookup(ROOT, "from-host.txt").unwrap();
  let fh = guest.open(node, O_RDONLY).unwrap();
  assert_eq!(
    guest.read(node, fh, 0, 4096).unwrap(),
    FROM_HOST,
    "the guest reads through the stream the bytes the host wrote over NFS"
  );
  assert_eq!(guest.close(node, fh), (0, 0));
  guest.forget(node, 1);
}

/// A rename and an unlink in the guest are what the host then sees, and the guest sees its own.
fn the_guests_rename_and_unlink_are_the_hosts(
  guest: &mut Guest,
  host: &mut TcpStream,
  root: &[u8],
) {
  assert_eq!(guest.rename(ROOT, "from-guest.txt", ROOT, "renamed.txt"), 0);
  assert_eq!(guest.lookup(ROOT, "from-guest.txt"), Err(ENOENT));
  assert_eq!(lookup_status(host, root, "from-guest.txt", 6), Err(2));
  let renamed = lookup(host, root, "renamed.txt", 7);
  assert_eq!(read(host, &renamed, 8), FROM_GUEST);
  assert_eq!(guest.unlink(ROOT, "renamed.txt"), 0);
  assert_eq!(lookup_status(host, root, "renamed.txt", 9), Err(2));
  assert_eq!(guest.lookup(ROOT, "renamed.txt"), Err(ENOENT));
}

/// AC (Shards part 1, step 1), by use: a guest's history over the stream (INIT, CREATE, WRITE, close, LOOKUP, OPEN,
/// READ, FORGET, RENAME, UNLINK) is served by the daemon; the host reads the guest's bytes back over NFS and the guest
/// reads the host's; a rename and an unlink in the guest are what the host then sees; a FORGET is answered with an empty
/// frame, so Shards' asker waits on nothing.
#[test]
fn a_guest_history_over_the_stream_is_the_volume_the_host_sees() {
  let (daemon, client, id) = provisioned("shards-history");
  let attached = attach(&daemon, id, None, Intent::Write).expect("the attach was served");
  assert!(
    attached.lease_epoch.is_some(),
    "a write intent holds the lease"
  );
  let mut guest = Guest::over(attached);
  assert_eq!(
    max_write_of(&guest.init()),
    slates_bridge_fuse::init::MAX_WRITE,
    "the guest is told the largest write a frame carries"
  );
  guest_writes(&mut guest, ROOT, "from-guest.txt", FROM_GUEST);
  let (mut host, root) = host_mount(&daemon, "shared");
  let host_file = lookup(&mut host, &root, "from-guest.txt", 2);
  assert_eq!(
    read(&mut host, &host_file, 3),
    FROM_GUEST,
    "the host reads back over NFS the bytes the guest wrote through the stream"
  );
  the_guest_reads_what_the_host_wrote(&mut guest, &mut host, &root);
  the_guests_rename_and_unlink_are_the_hosts(&mut guest, &mut host, &root);
  none_counted(
    &daemon,
    &[
      "shards.barrier_refused",
      "shards.frame_refused",
      "shards.stream_failed",
    ],
  );
  drop(guest);
  drop(host);
  drop(client);
  drop(daemon);
}

/// Every change a read-only guest asks for is refused: a create, a directory, an unlink, a rename, a truncating
/// SETATTR, and a write through a handle opened for reading or for writing (`EPERM`, the shared bridge's answer).
fn every_change_is_refused(guest: &mut Guest) {
  assert!(guest.create(ROOT, "new.txt").is_err());
  assert!(guest.mkdir(ROOT, "new-dir").is_err());
  assert_ne!(guest.unlink(ROOT, "kept.txt"), 0);
  assert_ne!(guest.rename(ROOT, "kept.txt", ROOT, "moved.txt"), 0);
  let node = guest.lookup(ROOT, "kept.txt").unwrap();
  assert_ne!(
    guest.truncate(node, 0),
    0,
    "a truncating SETATTR is refused"
  );
  let writable = guest.open(node, O_RDWR).unwrap();
  assert_eq!(guest.write(node, writable, 0, b"no"), Err(EPERM));
  assert_eq!(guest.close(node, writable), (0, 0));
  let fh = guest.open(node, O_RDONLY).unwrap();
  assert_eq!(guest.write(node, fh, 0, b"no"), Err(EPERM));
  assert_eq!(guest.read(node, fh, 0, 4096).unwrap(), FROM_HOST);
}

/// AC (Shards part 1, step 1), by use: an attachment for reading refuses every change (CREATE, MKDIR, UNLINK, RENAME,
/// a WRITE through any handle it opens, a truncating SETATTR) and nothing lands in the volume, while it reads what the
/// host wrote. A handle opened for writing is granted and every write through it refused `EPERM`, as on every FUSE
/// transport (the shared bridge's answer, recorded in GAPS: a read-only filesystem would answer `EROFS` at the open).
#[test]
fn a_read_only_stream_refuses_every_change() {
  let (daemon, client, id) = provisioned("shards-read-only");
  let (mut host, root) = host_mount(&daemon, "shared");
  let host_file = create(&mut host, &root, "kept.txt", 2);
  write(&mut host, &host_file, FROM_HOST, 3);

  let attached = attach(&daemon, id, None, Intent::Read).expect("the attach was served");
  assert_eq!(attached.lease_epoch, None, "a read intent takes no lease");
  let mut guest = Guest::over(attached);
  guest.init();
  every_change_is_refused(&mut guest);
  for (name, xid) in [("new.txt", 4), ("new-dir", 5), ("moved.txt", 6)] {
    assert_eq!(lookup_status(&mut host, &root, name, xid), Err(2), "{name}");
  }
  let kept = lookup(&mut host, &root, "kept.txt", 7);
  assert_eq!(read(&mut host, &kept, 8), FROM_HOST, "nothing changed");
  drop(guest);
  drop(host);
  drop(client);
  drop(daemon);
}

/// A scope must name a directory that exists: a file and a missing name are refused, typed.
fn a_scope_must_name_a_directory(daemon: &Daemon, id: VolumeId) {
  assert!(
    matches!(
      attach(daemon, id, Some("outside.txt"), Intent::Write),
      Err(Refusal::BadRequest { .. })
    ),
    "a scope must be a directory"
  );
  assert!(
    attach(daemon, id, Some("missing"), Intent::Write).is_err(),
    "a scope must exist"
  );
}

/// A scoped guest finds nothing outside its directory, and its `..` is the directory itself, as a volume root's is,
/// never the directory above: the node `..` named.
fn nothing_outside_the_scope(guest: &mut Guest) -> u64 {
  assert_eq!(guest.lookup(ROOT, "outside.txt"), Err(ENOENT));
  let up = guest.lookup(ROOT, "..").unwrap();
  assert_eq!(guest.lookup(up, "outside.txt"), Err(ENOENT));
  assert_eq!(guest.lookup(up, ".."), Ok(up));
  up
}

/// AC (Shards part 1, step 1), by use: an attachment presenting one directory serves that directory as its root, puts
/// what its guest creates inside it, and reaches nothing outside it, not even by `..`.
#[test]
fn a_scoped_stream_stays_in_its_directory() {
  let (daemon, client, id) = provisioned("shards-scoped");
  let (mut host, root) = host_mount(&daemon, "shared");
  let inside = mkdir(&mut host, &root, "inside", 2);
  let outside = create(&mut host, &root, "outside.txt", 3);
  write(&mut host, &outside, FROM_HOST, 4);

  a_scope_must_name_a_directory(&daemon, id);
  let mut guest = Guest::over(attach(&daemon, id, Some("inside"), Intent::Write).unwrap());
  guest.init();
  let up = nothing_outside_the_scope(&mut guest);
  let file = guest_writes(&mut guest, ROOT, "made-inside.txt", FROM_GUEST);
  assert_eq!(
    guest.lookup(up, "made-inside.txt"),
    Ok(file),
    "`..` named the scope"
  );
  let made = lookup(&mut host, &inside, "made-inside.txt", 5);
  assert_eq!(read(&mut host, &made, 6), FROM_GUEST);
  assert_eq!(
    lookup_status(&mut host, &root, "made-inside.txt", 7),
    Err(2),
    "nothing landed at the volume's root"
  );
  drop(guest);
  drop(host);
  drop(client);
  drop(daemon);
}

/// The volume's status as its owner reports it. Each call's completion is acknowledged at once, so a test that polls
/// status never meets the daemon's bound on the completions a client leaves unacknowledged.
fn status(client: &mut Client, volume: VolumeId) -> slates_ipc::protocol::StatusReport {
  let report = match client.call(&RequestBody::Status { volume }) {
    ReplyBody::Status { report } => *report,
    other => panic!("status: {other:?}"),
  };
  let up_to = client.sequence;
  assert!(matches!(
    client.call(&RequestBody::Acknowledge { up_to }),
    ReplyBody::Acknowledged
  ));
  report
}

/// The peer closing its end ends the attachment: the volume's status counts it gone, the lease it held is released, and
/// a detach of it is refused `NotFound`.
fn a_closed_end_ends_its_attachment(daemon: &Daemon, client: &mut Client, id: VolumeId) {
  let attached = attach(daemon, id, None, Intent::Write).unwrap();
  let closed = attached.attachment;
  let mut guest = Guest::over(attached);
  guest.init();
  let file = guest_writes(&mut guest, ROOT, "held.txt", FROM_GUEST);
  assert_eq!(guest.lookup(ROOT, "held.txt"), Ok(file));
  let before = status(client, id);
  assert_eq!(before.attachments, 1);
  assert!(before.lease_epoch.is_some(), "the stream holds the lease");
  drop(guest);
  wait_until("the closed stream's attachment outlived it", || {
    status(client, id).attachments == 0
  });
  assert_eq!(
    status(client, id).lease_epoch,
    None,
    "the holder's last attachment released the lease"
  );
  assert!(matches!(
    client.call(&RequestBody::Detach { attachment: closed }),
    ReplyBody::Refused {
      refusal: Refusal::NotFound
    }
  ));
}

/// AC (Shards part 1, step 1), by use: the peer closing its end ends the attachment as a recorded operation (the
/// volume's status counts it gone, its write lease is released, and a detach of it is refused `NotFound`); a detach of
/// a live one ends its stream (the guest's next read is the end).
#[test]
fn a_closed_end_ends_the_attachment_and_a_detach_ends_the_stream() {
  let (daemon, mut client, id) = provisioned("shards-ends");
  a_closed_end_ends_its_attachment(&daemon, &mut client, id);
  let attached = attach(&daemon, id, None, Intent::Write).unwrap();
  let live = attached.attachment;
  let mut guest = Guest::over(attached);
  guest.init();
  assert!(guest.lookup(ROOT, "held.txt").is_ok());
  assert!(matches!(
    client.call(&RequestBody::Detach { attachment: live }),
    ReplyBody::Detached
  ));
  assert!(guest.ended(), "a detach ends the stream");
  none_counted(
    &daemon,
    &[
      "shards.sweep_refused",
      "shards.record_end_refused",
      "shards.end_refused",
    ],
  );
  drop(client);
  drop(daemon);
}

/// AUD-29-82 (D-18, §4.8) over the stream. Do: under a daemon whose anchor segment the test holds, a guest creates a
/// file, writes bytes and closes it (FLUSH, then RELEASE) through the stream, every reply a success; stop the daemon
/// and start a second over the same segment. Expect: the second daemon reads the guest's bytes back over NFS, because
/// the guest learned of the CREATE and the FLUSH only after the owner's barrier captured the volume; and it counts the
/// stream's attachment no longer, since the stream was the first process's own.
#[test]
fn a_guests_acknowledged_close_survives_a_daemon_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-shards-durable-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = anchor_segment("shards-durable", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("durable")) else {
    panic!("the volume was not created");
  };
  let before = status(&mut client, id).attachments;
  let mut guest = Guest::over(attach(&first, id, None, Intent::Write).unwrap());
  guest.init();
  let (file, fh) = guest.create(ROOT, "kept.txt").unwrap();
  assert_eq!(
    guest.write(file, fh, 0, FROM_GUEST),
    Ok(u32::try_from(FROM_GUEST.len()).unwrap())
  );
  assert_eq!(guest.close(file, fh), (0, 0));
  assert_eq!(status(&mut client, id).attachments, before + 1);
  let refusals = first.refusals_on_every_shard().unwrap();
  assert_eq!(refusals.get("shards.barrier_refused"), None, "{refusals:?}");
  drop(client);
  first.stop();
  assert!(guest.ended(), "the stop ended the stream");

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let mut client = Client::connect(&instance);
  assert_eq!(
    status(&mut client, id).attachments,
    before,
    "no stream outlives its process"
  );
  let capability = second
    .mount_capability("durable")
    .expect("the owner shard answers")
    .expect("the volume was recovered");
  let port = second.nfs_port().expect("the daemon is serving NFS");
  let mut host = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut host, &capability, 1);
  let kept = lookup(&mut host, &root, "kept.txt", 2);
  assert_eq!(
    read(&mut host, &kept, 3),
    FROM_GUEST,
    "the bytes the guest closed survived the restart"
  );
  drop(client);
  drop(host);
  second.stop();
  drop(segment);
}

/// §3 the teardown sweep, by use. Do: the host writes a file over NFS; a guest looks it up through the stream (a
/// reference it never forgets); the host removes it. Expect: its bytes stay referenced while the guest holds it, as an
/// open-but-unlinked file's do, and are released once the guest's stream closes, because the end swept the references
/// the guest never gave back.
#[test]
fn a_closed_stream_gives_back_what_its_guest_held() {
  let (daemon, instance) = single_shard_daemon("shards-sweep");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("shared")) else {
    panic!("the volume was not created");
  };
  let (mut host, root) = host_mount(&daemon, "shared");
  let empty = status(&mut client, id).referenced_bytes;
  let doomed = create(&mut host, &root, "doomed.bin", 2);
  let held = vec![7u8; HELD_BYTES];
  write(&mut host, &doomed, &held, 3);
  let written = status(&mut client, id).referenced_bytes;
  assert!(
    written >= empty + u64::try_from(HELD_BYTES).unwrap(),
    "{empty} -> {written}"
  );

  let before = status(&mut client, id).attachments;
  let mut guest = Guest::over(attach(&daemon, id, None, Intent::Read).unwrap());
  guest.init();
  guest.lookup(ROOT, "doomed.bin").unwrap();
  assert_eq!(remove_status(&mut host, &root, b"doomed.bin", 4), 0);
  assert_eq!(
    status(&mut client, id).referenced_bytes,
    written,
    "the guest's reference keeps the removed file's bytes"
  );
  drop(guest);
  wait_until("the stream outlived its end", || {
    status(&mut client, id).attachments == before
  });
  let released = status(&mut client, id).referenced_bytes;
  assert!(
    released <= written - u64::try_from(HELD_BYTES).unwrap(),
    "the end swept the guest's reference and the file's bytes went with it: {written} -> {released}"
  );
  drop(host);
  drop(client);
  drop(daemon);
}

/// Shards' control asks are answered in the forms its device reads, and the stream goes on: a snapshot's, a restore's
/// and an unknown ask by a control frame whose first byte is not zero and whose rest is the reason; a DAX descriptor ask
/// by a `shards_ipc` message of a nonzero kind carrying Linux's `EOPNOTSUPP`, little-endian.
fn control_asks_are_refused_in_shards_forms(hostile: &mut Guest) {
  for ask in [ASK_SAVE, ASK_RESTORE, 0x7f] {
    hostile.send_frame(CONTROL, &[ask, 1, 2, 3]);
    let (control, answer) = hostile.reply_frame().unwrap();
    assert!(control, "a control ask is answered by a control frame");
    assert_ne!(answer[0], 0, "and refused");
    assert!(
      std::str::from_utf8(&answer[1..]).is_ok_and(|why| !why.is_empty()),
      "with its reason in words"
    );
  }
  let mut descriptor = vec![ASK_DESCRIPTOR];
  descriptor.extend_from_slice(&ROOT.to_le_bytes());
  descriptor.push(1);
  hostile.send_frame(CONTROL, &descriptor);
  let mut message = [0u8; 9];
  hostile.stream.read_exact(&mut message).unwrap();
  assert_ne!(message[0], 0, "a refused descriptor's shards_ipc kind");
  assert_eq!(u32::from_be_bytes(message[1..5].try_into().unwrap()), 4);
  assert_eq!(
    i32::from_le_bytes(message[5..9].try_into().unwrap()),
    EOPNOTSUPP
  );
}

/// Malformed requests are answered and the stream goes on: no FUSE header gets an empty frame, a body too short for its
/// opcode `EIO`, a zero-length request an empty frame.
fn malformed_requests_are_answered(hostile: &mut Guest) {
  hostile.send_frame(0, &[1, 2, 3]);
  assert_eq!(
    hostile.reply_frame(),
    Some((false, Vec::new())),
    "no header, no reply: an empty frame"
  );
  hostile.unique += 1;
  let short_read = message(Opcode::Read.to_wire(), hostile.unique, ROOT, &[0u8; 4]);
  hostile.send_frame(0, &short_read);
  let (_, reply) = hostile.reply_frame().unwrap();
  assert_eq!(errno(&reply), EIO, "a body too short for its opcode");
  assert_eq!(hostile.lookup(ROOT, "nothing"), Err(ENOENT));
  hostile.send_frame(0, &[]);
  let (_, empty) = hostile.reply_frame().unwrap();
  assert!(empty.is_empty(), "a zero-length request: an empty frame");
}

/// Writes `word` as a frame's length on `guest`'s stream: a length the protocol refuses ends the stream, counted.
fn a_refused_length_ends_its_stream(daemon: &Daemon, guest: &mut Guest, word: u32, refused: u64) {
  guest.stream.write_all(&word.to_le_bytes()).unwrap();
  assert!(guest.ended(), "a refused frame ends its stream");
  assert_eq!(
    wait_for_count(daemon, "shards.frame_refused", refused),
    refused
  );
}

/// AC (Shards part 1, step 1), hostile input: Shards' control asks are refused in the form its device reads (a
/// control answer with a reason; a descriptor ask's `shards_ipc` errno) and the stream goes on; a request too short for
/// a FUSE header gets an empty frame, and one too short for its opcode `EIO`; a length past the largest frame and a
/// control frame with no ask end only their own stream, counted, while another stream on the volume keeps serving and
/// the daemon never panics.
#[test]
fn hostile_frames_are_refused_and_end_only_their_own_stream() {
  let (daemon, client, id) = provisioned("shards-hostile");
  let mut bystander = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  bystander.init();
  let mut hostile = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  hostile.init();
  control_asks_are_refused_in_shards_forms(&mut hostile);
  malformed_requests_are_answered(&mut hostile);
  // A length past the largest frame, refused before anything is allocated for it.
  a_refused_length_ends_its_stream(&daemon, &mut hostile, !CONTROL, 1);
  let mut second = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  second.init();
  // A control frame with no ask.
  a_refused_length_ends_its_stream(&daemon, &mut second, CONTROL, 2);
  assert_eq!(bystander.lookup(ROOT, "nothing"), Err(ENOENT));
  guest_writes(&mut bystander, ROOT, "still-served.txt", FROM_GUEST);
  assert!(
    wait_for_count(&daemon, "shards.control_refused", 4) >= 4,
    "each control ask is counted"
  );
  drop(bystander);
  drop(client);
  drop(daemon);
}
