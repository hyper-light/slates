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
use std::os::fd::OwnedFd;
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
/// Format: `FUSE_MAP_ALIGNMENT` and `FUSE_INIT_EXT` in `fuse_init_in.flags`, and `FUSE_HAS_INODE_DAX` in its `flags2`
/// (`include/uapi/linux/fuse.h`).
const INIT_MAP_ALIGNMENT: u32 = 1 << 26;
const INIT_EXT: u32 = 1 << 30;
const INIT2_HAS_INODE_DAX: u32 = 1 << 1;
/// Format: `FUSE_ATTR_DAX` in `fuse_attr.flags`.
const ATTR_DAX: u32 = 1 << 1;
/// Format: where `fuse_attr.flags` lies in a LOOKUP reply (`fuse_entry_out`: 40 bytes before its `fuse_attr`, whose
/// last word is `flags`, at 84) and in a GETATTR reply (`fuse_attr_out`: 16 bytes before its `fuse_attr`).
const ENTRY_ATTR_FLAGS_AT: usize = OUT_HEADER_LEN + 40 + 84;
const ATTR_OUT_FLAGS_AT: usize = OUT_HEADER_LEN + 16 + 84;
/// Format: the range a guest maps a file in, and a DAX object's view (`FUSE_DAX_SZ`).
const DAX_RANGE: u64 = slates_mem::DAX_RANGE_BYTES;
/// Format: an opcode no FUSE kernel defines (`include/uapi/linux/fuse.h` ends in the low hundreds).
const UNKNOWN_OPCODE: u32 = 9_999;
/// Format: Linux errnos a reply carries, negated.
const ENOSYS: i32 = 38;
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
/// Linux's `EISDIR`, a descriptor ask for a directory.
const EISDIR: i32 = 21;
/// Linux's `EINVAL`, a descriptor ask of no form the protocol has.
const EINVAL: i32 = 22;
/// Linux's `ENOMEM`, a fill past the shard's DAX budget.
const ENOMEM: i32 = 12;
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

  /// An `INIT` offering the DAX flags a `dax=inode` guest offers: `FUSE_MAP_ALIGNMENT`, and `FUSE_HAS_INODE_DAX` in the
  /// second flags word, which `FUSE_INIT_EXT` announces. The reply.
  fn init_offering_dax(&mut self) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&MAJOR.to_le_bytes());
    body.extend_from_slice(&MINOR.to_le_bytes());
    body.extend_from_slice(&(128u32 << 10).to_le_bytes());
    body.extend_from_slice(&(INIT_MAP_ALIGNMENT | INIT_EXT).to_le_bytes());
    body.extend_from_slice(&INIT2_HAS_INODE_DAX.to_le_bytes());
    body.extend_from_slice(&[0u8; 44]);
    let reply = self.ask(Opcode::Init, 0, &body);
    assert_eq!(errno(&reply), 0, "INIT answered");
    reply
  }

  /// A LOOKUP's whole reply.
  fn lookup_reply(&mut self, parent: u64, name: &str) -> Vec<u8> {
    self.ask(Opcode::Lookup, parent, &named(name))
  }

  /// A GETATTR's whole reply.
  fn getattr_reply(&mut self, node: u64) -> Vec<u8> {
    self.ask(Opcode::GetAttr, node, &[0u8; 16])
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

  /// Asks for a DAX window's descriptor of `node`, as Shards' device does at a `FUSE_SETUPMAPPING`, with the mapping's
  /// range in the agreed form where `range` names one, and reads the answer as its `shards_ipc::recv` does: the
  /// descriptor, or the errno it was refused with. A refusal carrying a descriptor fails the test.
  fn descriptor(
    &mut self,
    node: u64,
    writable: bool,
    range: Option<(u64, u64)>,
  ) -> Result<OwnedFd, i32> {
    let mut ask = vec![ASK_DESCRIPTOR];
    ask.extend_from_slice(&node.to_le_bytes());
    ask.push(u8::from(writable));
    if let Some((offset, len)) = range {
      ask.extend_from_slice(&offset.to_le_bytes());
      ask.extend_from_slice(&len.to_le_bytes());
    }
    self.send_frame(CONTROL, &ask);
    self.descriptor_answer()
  }

  /// Reads a descriptor ask's answer: a `shards_ipc` header (kind, big-endian length), any descriptor riding its first
  /// byte, then the payload.
  fn descriptor_answer(&mut self) -> Result<OwnedFd, i32> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut header = [0u8; 5];
    let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received = rustix::net::recvmsg(
      &self.stream,
      &mut [std::io::IoSliceMut::new(&mut header)],
      &mut control,
      RecvFlags::empty(),
    )
    .unwrap();
    let mut descriptors: Vec<OwnedFd> = Vec::new();
    for message in control.drain() {
      if let RecvAncillaryMessage::ScmRights(fds) = message {
        descriptors.extend(fds);
      }
    }
    self
      .stream
      .read_exact(&mut header[received.bytes..])
      .unwrap();
    let mut payload =
      vec![0u8; usize::try_from(u32::from_be_bytes(header[1..5].try_into().unwrap())).unwrap()];
    self.stream.read_exact(&mut payload).unwrap();
    if header[0] == 0 {
      assert!(payload.is_empty(), "an answered ask carries no payload");
      assert_eq!(
        descriptors.len(),
        1,
        "an answered ask carries one descriptor"
      );
      return Ok(descriptors.remove(0));
    }
    assert!(
      descriptors.is_empty(),
      "a refused ask carries no descriptor"
    );
    Err(i32::from_le_bytes(payload[..4].try_into().unwrap()))
  }
}

/// One range of a DAX object, mapped from a descriptor the daemon answered, as Shards' window maps it: shared, read and
/// write. Unmapped when dropped.
struct WindowRange {
  at: *mut u8,
  len: usize,
}

impl WindowRange {
  fn map(object: &OwnedFd, range: u64) -> WindowRange {
    let len = usize::try_from(DAX_RANGE).unwrap();
    // SAFETY: a fresh shared mapping of a range inside the object, which never shrinks (sealed on Linux, sized once on
    // macOS); the test reads and writes it only through this struct, which unmaps it once.
    let at = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        len,
        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
        rustix::mm::MapFlags::SHARED,
        object,
        range * DAX_RANGE,
      )
    }
    .unwrap();
    WindowRange { at: at.cast(), len }
  }

  /// The range's bytes, read as a guest reads its window: volatile loads.
  fn bytes(&self) -> Vec<u8> {
    // SAFETY: inside this live mapping.
    (0..self.len)
      .map(|index| unsafe { self.at.add(index).read_volatile() })
      .collect()
  }

  /// A guest's store through its window: volatile writes.
  fn store(&self, within: usize, bytes: &[u8]) {
    assert!(within + bytes.len() <= self.len);
    for (index, byte) in bytes.iter().enumerate() {
      // SAFETY: inside this live mapping (asserted above).
      unsafe { self.at.add(within + index).write_volatile(*byte) };
    }
  }
}

impl Drop for WindowRange {
  fn drop(&mut self) {
    // SAFETY: the mapping `map` made, unmapped once.
    let _ = unsafe { rustix::mm::munmap(self.at.cast(), self.len) };
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
/// for the root, a directory, by a `shards_ipc` message of a nonzero kind carrying Linux's `EISDIR`, little-endian.
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
    EISDIR
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

/// Writes `part` of a frame on `guest`'s stream and closes the stream's writing half: the daemon drops the part
/// unanswered and ends the stream, counted as truncated.
fn a_truncated_frame_ends_its_stream(
  daemon: &Daemon,
  guest: &mut Guest,
  part: &[u8],
  truncated: u64,
) {
  guest.stream.write_all(part).unwrap();
  guest.stream.shutdown(std::net::Shutdown::Write).unwrap();
  assert!(guest.ended(), "a truncated frame ends its stream");
  assert_eq!(
    wait_for_count(daemon, "shards.frame_truncated", truncated),
    truncated
  );
}

/// A request with a whole header and an opcode no FUSE kernel defines is answered `ENOSYS`, and the stream goes on.
fn an_unknown_opcode_is_answered_enosys(guest: &mut Guest) {
  guest.unique += 1;
  let garbage = message(UNKNOWN_OPCODE, guest.unique, ROOT, &[0xa5; 64]);
  guest.send_frame(0, &garbage);
  let (_, reply) = guest.reply_frame().unwrap();
  assert_eq!(errno(&reply), ENOSYS);
  assert_eq!(guest.lookup(ROOT, "nothing"), Err(ENOENT));
}

/// AC (Shards part 1, step 1), hostile input: Shards' control asks are refused in the form its device reads (a
/// control answer with a reason; a descriptor ask's `shards_ipc` errno) and the stream goes on; a request too short for
/// a FUSE header gets an empty frame, one too short for its opcode `EIO`, and an unknown opcode `ENOSYS`; a length past
/// the largest frame (`u32::MAX` among them), a control frame with no ask and a frame cut short by the peer's close end
/// only their own stream, each counted by its kind, while another stream on the volume keeps serving and the daemon
/// never panics.
#[test]
fn hostile_frames_are_refused_and_end_only_their_own_stream() {
  let (daemon, client, id) = provisioned("shards-hostile");
  let mut bystander = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  bystander.init();
  let mut hostile = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  hostile.init();
  control_asks_are_refused_in_shards_forms(&mut hostile);
  malformed_requests_are_answered(&mut hostile);
  an_unknown_opcode_is_answered_enosys(&mut hostile);
  // A length past the largest frame, refused before anything is allocated for it: a request's, then a control
  // frame's (`u32::MAX`), then a control frame with no ask.
  a_refused_length_ends_its_stream(&daemon, &mut hostile, !CONTROL, 1);
  for (word, refused) in [(u32::MAX, 2), (CONTROL, 3)] {
    let mut next = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
    next.init();
    a_refused_length_ends_its_stream(&daemon, &mut next, word, refused);
  }
  // A frame cut short by the peer's close: inside its body, then inside its length word.
  let mut cut = 64u32.to_le_bytes().to_vec();
  cut.extend_from_slice(&[0u8; 10]);
  for (part, truncated) in [(&cut[..], 1), (&cut[..2], 2)] {
    let mut next = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
    next.init();
    a_truncated_frame_ends_its_stream(&daemon, &mut next, part, truncated);
  }
  assert_eq!(bystander.lookup(ROOT, "nothing"), Err(ENOENT));
  guest_writes(&mut bystander, ROOT, "still-served.txt", FROM_GUEST);
  assert!(
    wait_for_count(&daemon, "shards.control_refused", 3) >= 3,
    "each refused control ask is counted"
  );
  assert!(
    wait_for_count(&daemon, "shards.dax_not_a_file", 1) >= 1,
    "the descriptor ask for the root is counted by its kind"
  );
  drop(bystander);
  drop(client);
  drop(daemon);
}

// --- Shards part 3, step 2: the DAX object a descriptor ask is answered with. ---

/// A single-shard daemon whose shard reserve is `reserve` bytes where one is given (the fill budget is the reserve),
/// with a client and a volume of `bound` bytes named `name`.
fn dax_provisioned(name: &str, bound: u64, reserve: Option<u64>) -> (Daemon, Client, VolumeId) {
  let profile = common::machine_profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(1));
  if let Some(reserve) = reserve {
    config.reserve_per_shard = reserve;
  }
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
  let mut client = Client::connect(&instance);
  let id = create_volume(&mut client, "dax", bound);
  (daemon, client, id)
}

/// Creates a volume of `bound` bytes named `name`.
fn create_volume(client: &mut Client, name: &str, bound: u64) -> VolumeId {
  let reply = client.call(&RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: bound },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  });
  let ReplyBody::Created { id } = reply else {
    panic!("the volume was not created: {reply:?}");
  };
  id
}

/// The guest creates `name` in the root and writes `bytes` in frames of the largest write, then closes it: its node.
fn guest_writes_large(guest: &mut Guest, name: &str, bytes: &[u8]) -> u64 {
  let (file, fh) = guest.create(ROOT, name).unwrap();
  let piece = usize::try_from(slates_bridge_fuse::init::MAX_WRITE).unwrap();
  for (index, chunk) in bytes.chunks(piece).enumerate() {
    let offset = u64::try_from(index * piece).unwrap();
    assert_eq!(
      guest.write(file, fh, offset, chunk),
      Ok(u32::try_from(chunk.len()).unwrap())
    );
  }
  assert_eq!(guest.close(file, fh), (0, 0));
  file
}

/// `len` bytes of a pattern no other file here repeats: a page index and the seed, so a misplaced page shows.
fn patterned(len: usize, seed: u8) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from(at / 4096 % 251).unwrap() ^ seed ^ u8::try_from(at % 7).unwrap())
    .collect()
}

/// Whether `needle` occurs in `haystack`.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
  haystack
    .windows(needle.len())
    .any(|window| window == needle)
}

/// A marker the neighbouring file of the same volume holds, and one a file of another volume holds.
const NEIGHBOUR_MARKER: &[u8] = b"<<a neighbour's bytes, never in another file's object>>";
const FAR_MARKER: &[u8] = b"<<another volume's bytes, never in this volume's objects>>";

/// Each `(descriptor, range)` maps to the file's bytes there, zeros past its end: the ranges' bytes, for a scan.
fn asked_ranges_are_the_files(content: &[u8], asked: &[(&OwnedFd, u64)]) -> Vec<u8> {
  let range_bytes = usize::try_from(DAX_RANGE).unwrap();
  let mut scanned = Vec::new();
  for (descriptor, range) in asked {
    let mapped = WindowRange::map(descriptor, *range).bytes();
    let start = usize::try_from(*range).unwrap() * range_bytes;
    let file_part = &content[start.min(content.len())..(start + range_bytes).min(content.len())];
    assert_eq!(
      &mapped[..file_part.len()],
      file_part,
      "range {range} is the file's"
    );
    assert!(
      mapped[file_part.len()..].iter().all(|byte| *byte == 0),
      "range {range} is zeros past the file's end"
    );
    scanned.extend(mapped);
  }
  scanned
}

/// Neither marker is in `scanned`, and the same scan finds each in its own file's object (non-vacuous).
fn no_marker_but_its_own(scanned: &[u8], neighbours_object: &OwnedFd, fars_object: &OwnedFd) {
  for marker in [NEIGHBOUR_MARKER, FAR_MARKER] {
    assert!(
      !holds(scanned, marker),
      "no other file's bytes are in the object"
    );
  }
  let neighbours = WindowRange::map(neighbours_object, 0).bytes();
  assert!(
    holds(&neighbours, NEIGHBOUR_MARKER),
    "the scan finds a marker in its own file's object"
  );
  let fars = WindowRange::map(fars_object, 0).bytes();
  assert!(
    holds(&fars, FAR_MARKER),
    "the scan finds the other volume's marker in its own file's object"
  );
}

/// The daemon counted no descriptor refusal of any kind.
fn no_descriptor_refusal_counted(daemon: &Daemon) {
  none_counted(
    daemon,
    &[
      "shards.dax_malformed",
      "shards.dax_unreachable",
      "shards.dax_not_a_file",
      "shards.dax_read_only",
      "shards.dax_budget",
      "shards.dax_object",
    ],
  );
}

/// AC (Shards part 3, step 2), by use. Do: write a 5 MiB file and a neighbouring file holding a marker into a volume,
/// and a file holding another marker into a second volume; ask for the two marked files' descriptors first, then for
/// the large file's for its first range and, in the agreed ranged form, for its third, and once in today's form with no
/// range; map what comes back as Shards' window does. Expect each descriptor to be of an object of 2^63 − 2 MiB whose asked ranges equal the file there
/// (zeros past its end), whose untouched range is zeros, and in which neither marker occurs. Non-vacuous: the same scan
/// finds each marker in its own file's object.
#[test]
fn a_descriptor_answers_with_an_object_holding_only_its_files_bytes() {
  let (daemon, mut client, id) = dax_provisioned("shards-dax-confined", 16 << 20, None);
  let other = create_volume(&mut client, "other", 4 << 20);
  let mut guest = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  guest.init();
  let size = (5 << 20) + 123;
  let content = patterned(size, 0x5a);
  let big = guest_writes_large(&mut guest, "big", &content);
  let mut neighbour_bytes = patterned(64 << 10, 0x11);
  neighbour_bytes[1000..1000 + NEIGHBOUR_MARKER.len()].copy_from_slice(NEIGHBOUR_MARKER);
  let neighbour = guest_writes_large(&mut guest, "neighbour", &neighbour_bytes);
  let mut far_guest = Guest::over(attach(&daemon, other, None, Intent::Write).unwrap());
  far_guest.init();
  let mut far_bytes = patterned(64 << 10, 0x22);
  far_bytes[2000..2000 + FAR_MARKER.len()].copy_from_slice(FAR_MARKER);
  let far = guest_writes_large(&mut far_guest, "far", &far_bytes);
  // The other files' objects are filled first, so any of their bytes misplaced into the large file's object is there
  // when it is scanned.
  let neighbours_object = guest.descriptor(neighbour, false, None).unwrap();
  let fars_object = far_guest.descriptor(far, false, None).unwrap();

  let first = guest.descriptor(big, false, Some((0, DAX_RANGE))).unwrap();
  let object_size = rustix::fs::fstat(&first).unwrap().st_size;
  assert_eq!(
    u64::try_from(object_size).unwrap(),
    slates_mem::DAX_OBJECT_BYTES
  );
  let third = guest
    .descriptor(big, false, Some((2 * DAX_RANGE, DAX_RANGE)))
    .unwrap();
  let whole = guest.descriptor(big, false, None).unwrap();
  let scanned = asked_ranges_are_the_files(&content, &[(&first, 0), (&third, 2), (&whole, 1)]);
  let untouched = WindowRange::map(&first, 1000).bytes();
  assert!(
    untouched.iter().all(|byte| *byte == 0),
    "an untouched range is zeros"
  );
  no_marker_but_its_own(&scanned, &neighbours_object, &fars_object);
  assert_eq!(wait_for_count(&daemon, "shards.dax_answered", 5), 5);
  no_descriptor_refusal_counted(&daemon);
  drop(client);
  drop(daemon);
}

/// AC (Shards part 3, step 2), by use. Do: ask twice for one file's descriptor, map the same range from each answer,
/// and store through the first mapping; then, as the VMM, try to resize the object through a descriptor it holds.
/// Expect the store to be seen through the second mapping, so both asks were answered with one object, and every resize
/// refused with the size unchanged; on Linux the object carries the grow, shrink and seal seals.
#[test]
fn two_asks_for_one_node_answer_one_object_the_peer_cannot_resize() {
  let (daemon, client, id) = dax_provisioned("shards-dax-one", 8 << 20, None);
  let mut guest = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  guest.init();
  let file = guest_writes_large(&mut guest, "file", &patterned(3 << 20, 0x33));
  let one = guest.descriptor(file, true, Some((0, DAX_RANGE))).unwrap();
  let two = guest.descriptor(file, true, Some((0, DAX_RANGE))).unwrap();
  let through_one = WindowRange::map(&one, 0);
  let through_two = WindowRange::map(&two, 0);
  through_one.store(100, b"seen through the other answer");
  assert_eq!(
    &through_two.bytes()[100..100 + 29],
    b"seen through the other answer",
    "both asks were answered with one object"
  );
  #[allow(clippy::disallowed_methods)]
  // the VMM's attempt on a memory object: RAM, not a host path (R1).
  let shrink = rustix::fs::ftruncate(&one, DAX_RANGE);
  assert!(shrink.is_err(), "the VMM cannot shrink the object");
  #[allow(clippy::disallowed_methods)] // as above.
  let grow = rustix::fs::ftruncate(&one, slates_mem::DAX_OBJECT_BYTES + DAX_RANGE);
  assert!(grow.is_err(), "the VMM cannot grow the object");
  let after = rustix::fs::fstat(&two).unwrap().st_size;
  assert_eq!(u64::try_from(after).unwrap(), slates_mem::DAX_OBJECT_BYTES);
  #[cfg(target_os = "linux")]
  {
    let seals = rustix::fs::fcntl_get_seals(&one).unwrap();
    for seal in [
      rustix::fs::SealFlags::GROW,
      rustix::fs::SealFlags::SHRINK,
      rustix::fs::SealFlags::SEAL,
    ] {
      assert!(seals.contains(seal), "{seal:?} in {seals:?}");
    }
  }
  drop(client);
  drop(daemon);
}

/// AC (Shards part 3, step 2), hostile input. Do: send descriptor asks cut short, with a range whose end overflows, with
/// a range past the object, for the root directory, for a node that does not exist, for a node outside a scoped
/// attachment's directory, and a writable ask of a read-only attachment. Expect each refused with its errno and no
/// descriptor, counted by its kind, the stream serving on after each; and the read-only attachment's read-only ask
/// answered.
#[test]
fn hostile_descriptor_asks_are_refused_with_an_errno_and_no_descriptor() {
  let (daemon, client, id) = dax_provisioned("shards-dax-hostile", 8 << 20, None);
  let mut guest = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  guest.init();
  guest.mkdir(ROOT, "inside").unwrap();
  let outside = guest_writes_large(&mut guest, "outside", &patterned(3 << 20, 0x44));
  malformed_asks_are_refused(&mut guest, outside);
  asks_for_no_file_are_refused(&mut guest, outside);
  a_scoped_attachment_reaches_no_node_outside(&daemon, id, outside);
  a_read_only_attachment_may_not_ask_to_write(&daemon, id, outside);
  assert_eq!(wait_for_count(&daemon, "shards.dax_malformed", 3), 3);
  assert_eq!(wait_for_count(&daemon, "shards.dax_not_a_file", 1), 1);
  assert_eq!(wait_for_count(&daemon, "shards.dax_unreachable", 2), 2);
  assert_eq!(wait_for_count(&daemon, "shards.dax_read_only", 1), 1);
  drop(client);
  drop(daemon);
}

/// Asks cut short, with a range whose end overflows, and with a range past the object: each `EINVAL`.
fn malformed_asks_are_refused(guest: &mut Guest, file: u64) {
  guest.send_frame(CONTROL, &[ASK_DESCRIPTOR, 1, 2, 3, 4]);
  assert_eq!(guest.descriptor_answer().err(), Some(EINVAL), "cut short");
  assert_eq!(
    guest
      .descriptor(file, false, Some((u64::MAX - 1, 16)))
      .err(),
    Some(EINVAL),
    "a range whose end overflows"
  );
  let past = Some((slates_mem::DAX_OBJECT_BYTES, DAX_RANGE));
  assert_eq!(
    guest.descriptor(file, false, past).err(),
    Some(EINVAL),
    "a range past the object"
  );
}

/// Asks for the root directory and for a node that does not exist: `EISDIR` and `ENOENT`, the stream serving on.
fn asks_for_no_file_are_refused(guest: &mut Guest, file: u64) {
  assert_eq!(
    guest.descriptor(ROOT, false, None).err(),
    Some(EISDIR),
    "a directory"
  );
  assert_eq!(
    guest.descriptor(999_999, false, None).err(),
    Some(ENOENT),
    "no such node"
  );
  assert_eq!(
    guest.lookup(ROOT, "outside"),
    Ok(file),
    "the stream serves on"
  );
}

/// An attachment scoped to `inside` asks for `outside`: refused with an errno.
fn a_scoped_attachment_reaches_no_node_outside(daemon: &Daemon, id: VolumeId, outside: u64) {
  let mut scoped = Guest::over(attach(daemon, id, Some("inside"), Intent::Write).unwrap());
  scoped.init();
  let refused = scoped.descriptor(outside, false, None).err();
  assert!(
    refused.is_some_and(|errno| errno != 0),
    "a node outside the scoped attachment's directory: {refused:?}"
  );
}

/// A read-only attachment's writable ask is refused `EPERM`, and its read-only ask answered.
fn a_read_only_attachment_may_not_ask_to_write(daemon: &Daemon, id: VolumeId, file: u64) {
  let mut reader = Guest::over(attach(daemon, id, None, Intent::Read).unwrap());
  reader.init();
  assert_eq!(
    reader.descriptor(file, true, Some((0, DAX_RANGE))).err(),
    Some(EPERM),
    "a writable ask of a read-only attachment"
  );
  assert!(
    reader.descriptor(file, false, Some((0, DAX_RANGE))).is_ok(),
    "its read-only ask is answered"
  );
}

/// Whether a reply's `fuse_attr.flags`, at `at`, carries `FUSE_ATTR_DAX`.
fn marked_dax(reply: &[u8], at: usize) -> bool {
  assert_eq!(errno(reply), 0, "the reply is a success");
  u32::from_le_bytes(reply[at..at + 4].try_into().unwrap()) & ATTR_DAX != 0
}

/// AC (Shards part 3, step 2), the DAX offer. Do: `INIT` a guest over the stream offering the flags a `dax=inode` guest
/// offers; look up a 3 MiB file, a small file and a directory, and get the large file's attributes. Expect the `INIT`
/// reply to keep `FUSE_MAP_ALIGNMENT` and `FUSE_HAS_INODE_DAX` and to name the host's page as the alignment, and
/// `FUSE_ATTR_DAX` on the large file's entry and attributes and on neither the small file's nor the directory's.
#[test]
fn the_stream_offers_per_inode_dax_for_files_of_a_range_or_more() {
  let (daemon, client, id) = dax_provisioned("shards-dax-offer", 8 << 20, None);
  let mut guest = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  let init = guest.init_offering_dax();
  let word = |at: usize| {
    u32::from_le_bytes(
      init[OUT_HEADER_LEN + at..OUT_HEADER_LEN + at + 4]
        .try_into()
        .unwrap(),
    )
  };
  assert_ne!(word(12) & INIT_MAP_ALIGNMENT, 0, "FUSE_MAP_ALIGNMENT kept");
  assert_ne!(word(32) & INIT2_HAS_INODE_DAX, 0, "FUSE_HAS_INODE_DAX kept");
  let page = u32::try_from(rustix::param::page_size()).unwrap();
  assert_eq!(
    word(28) >> 16,
    page.trailing_zeros(),
    "the host's page as the alignment"
  );
  guest_writes_large(&mut guest, "large", &patterned(3 << 20, 0x66));
  guest_writes(&mut guest, ROOT, "small", b"a few bytes");
  guest.mkdir(ROOT, "directory").unwrap();
  let large = guest.lookup_reply(ROOT, "large");
  assert!(
    marked_dax(&large, ENTRY_ATTR_FLAGS_AT),
    "a file of a range or more is marked"
  );
  let node = u64_at(&large, OUT_HEADER_LEN);
  assert!(
    marked_dax(&guest.getattr_reply(node), ATTR_OUT_FLAGS_AT),
    "and its attributes say so"
  );
  assert!(
    !marked_dax(&guest.lookup_reply(ROOT, "small"), ENTRY_ATTR_FLAGS_AT),
    "a small file is not"
  );
  assert!(
    !marked_dax(&guest.lookup_reply(ROOT, "directory"), ENTRY_ATTR_FLAGS_AT),
    "nor a directory"
  );
  drop(client);
  drop(daemon);
}

/// AC (Shards part 3, step 2), the budget. Do: on a daemon whose shard reserve, and so its fill budget, is 16 MiB (of
/// which a bounded volume may take 6 MiB, the rest the shard's operation headroom), write a 5.5 MiB file and have
/// three attachments each ask for the whole of it; then have the first forget its node and the third ask again. Expect
/// the first two answered (11 MiB of copies), the third refused `ENOMEM` and counted, since its copy would take the
/// fills to 16.5 MiB; no file offered DAX while the budget is full; and the offer back, and the third answered, once
/// the first's forget gave its 5.5 MiB back.
#[test]
fn a_fill_past_the_shards_budget_is_refused_and_a_forget_gives_its_charge_back() {
  let reserve = 16 << 20;
  let (daemon, client, id) = dax_provisioned("shards-dax-budget", 6 << 20, Some(reserve));
  let mut first = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  first.init();
  let file = guest_writes_large(&mut first, "file", &patterned((11 << 20) / 2, 0x55));
  let mut second = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  second.init();
  assert_eq!(second.lookup(ROOT, "file"), Ok(file));
  let mut third = Guest::over(attach(&daemon, id, None, Intent::Write).unwrap());
  third.init();
  assert_eq!(third.lookup(ROOT, "file"), Ok(file));
  for (which, guest) in [("first", &mut first), ("second", &mut second)] {
    let answered = guest.descriptor(file, false, None);
    assert!(answered.is_ok(), "the {which} copy fits: {answered:?}");
  }
  assert_eq!(
    third.descriptor(file, false, None).err(),
    Some(ENOMEM),
    "a third copy passes the budget"
  );
  assert_eq!(wait_for_count(&daemon, "shards.dax_budget", 1), 1);
  assert!(
    !marked_dax(&second.lookup_reply(ROOT, "file"), ENTRY_ATTR_FLAGS_AT),
    "with the budget full, no file is offered DAX"
  );
  first.forget(file, 1);
  assert!(
    marked_dax(&second.lookup_reply(ROOT, "file"), ENTRY_ATTR_FLAGS_AT),
    "with room again, it is"
  );
  let retried = third.descriptor(file, false, None);
  assert!(
    retried.is_ok(),
    "the first's forget gave its charge back: {retried:?}"
  );
  drop(client);
  drop(daemon);
}

/// macOS: the socket route that hands a client its stream end (A-161, part 1 step 3), driven by hand on the wire so
/// every hostile exchange can be made.
#[cfg(target_os = "macos")]
mod route {
  use slates_ipc::route::{
    Exchange, HELLO_BYTES, Hello, KIND_CHALLENGE, KIND_MALFORMED, KIND_PROOF, NONCE_BYTES,
    SEALED_BYTES,
  };

  use super::*;

  /// Shape: how long past an offer's life the expiry test waits before it claims: the offer's two liveness budgets
  /// (`OFFER_LIFE_NS`), and half a budget for the timer's own wake on a loaded machine.
  const PAST_EXPIRY: Duration = Duration::from_millis(2_500);

  /// Attaches `id` in the Shards form over the ring: the attachment and the secret its offer is claimed with.
  fn offer(client: &mut Client, id: VolumeId) -> (u64, [u8; 32]) {
    match client.call(&RequestBody::Attach {
      volume: id,
      snapshot: None,
      intent: Intent::Write,
      form: slates_ipc::protocol::AttachRequest::ShardsFs { subtree: None },
    }) {
      ReplyBody::ShardsOffered {
        attachment, secret, ..
      } => (attachment, secret),
      other => panic!("the Shards form was not offered: {other:?}"),
    }
  }

  /// A connection to the route of `instance`, its reads bounded.
  fn connect_route(instance: &str) -> UnixStream {
    let stream = UnixStream::connect(slates_ipc::route::path(instance).unwrap()).unwrap();
    stream.set_read_timeout(Some(CREDIT_WAIT)).unwrap();
    stream
  }

  /// Sends a hello for `attachment` with `client_nonce` and reads the daemon's challenge: its nonce.
  fn challenged(
    stream: &mut UnixStream,
    attachment: u64,
    client_nonce: [u8; NONCE_BYTES],
  ) -> [u8; NONCE_BYTES] {
    let hello = Hello {
      attachment,
      client_nonce,
    };
    stream.write_all(&hello.encode()).unwrap();
    let mut challenge = [0u8; 1 + NONCE_BYTES];
    stream.read_exact(&mut challenge).unwrap();
    assert_eq!(challenge[0], KIND_CHALLENGE);
    challenge[1..].try_into().unwrap()
  }

  /// Reads the daemon's refusal: its kind byte, then the end; no descriptor came with it (a plain read takes none).
  fn refused_with(stream: &mut UnixStream, kind: u8) {
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], kind, "the refusal's kind");
    assert_eq!(
      stream.read(&mut byte).unwrap(),
      0,
      "the end follows the refusal"
    );
  }

  /// The proof of a claim of `attachment` under `secret` with both nonces.
  fn proof(
    secret: &[u8; 32],
    attachment: u64,
    client: [u8; NONCE_BYTES],
    daemon: [u8; NONCE_BYTES],
  ) -> [u8; SEALED_BYTES] {
    Exchange::new(secret, attachment, &client, &daemon)
      .unwrap()
      .proof()
      .unwrap()
  }

  /// The ledger count of `kind` on every shard.
  fn counted(daemon: &Daemon, kind: &str) -> u64 {
    daemon
      .refusals_on_every_shard()
      .unwrap()
      .get(kind)
      .copied()
      .unwrap_or(0)
  }

  /// A-161 step 3, hostile exchanges: a wrong secret, a proof replayed from another connection, a message cut short, a
  /// claim with bytes after its proof, a hello of another version and a stranger who says nothing all get no
  /// descriptor and a typed, counted refusal, and leave the offer to its honest claim, which then succeeds once; a
  /// second claim with the same secret finds no offer. The listener serves every honest claim throughout.
  #[test]
  fn hostile_claims_get_no_descriptor_and_the_honest_claim_succeeds_once() {
    let (daemon, instance) = single_shard_daemon("shards-route");
    let mut client = Client::connect(&instance);
    let ReplyBody::Created { id } = client.call(&scratch("shared")) else {
      panic!("the volume was not created");
    };
    let (attachment, secret) = offer(&mut client, id);

    // A stranger with the attachment but another secret.
    let mut stranger = connect_route(&instance);
    let client_nonce = [1u8; NONCE_BYTES];
    let daemon_nonce = challenged(&mut stranger, attachment, client_nonce);
    stranger
      .write_all(&proof(&[0xa5; 32], attachment, client_nonce, daemon_nonce))
      .unwrap();
    refused_with(&mut stranger, KIND_PROOF);

    // A proof made for one connection's nonce, replayed on another: the second daemon nonce differs.
    let mut first = connect_route(&instance);
    let first_nonce = challenged(&mut first, attachment, client_nonce);
    let recorded = proof(&secret, attachment, client_nonce, first_nonce);
    let mut replay = connect_route(&instance);
    let _second_nonce = challenged(&mut replay, attachment, client_nonce);
    replay.write_all(&recorded).unwrap();
    refused_with(&mut replay, KIND_PROOF);
    drop(first);

    // A hello cut short by the client's close; a hello of another version; a right proof with bytes after it.
    let mut short = connect_route(&instance);
    short.write_all(&[1, 2, 3]).unwrap();
    short.shutdown(std::net::Shutdown::Write).unwrap();
    refused_with(&mut short, KIND_MALFORMED);
    let mut other_version = connect_route(&instance);
    let mut hello = Hello {
      attachment,
      client_nonce,
    }
    .encode();
    hello[0] ^= 0xff;
    other_version.write_all(&hello).unwrap();
    refused_with(&mut other_version, KIND_MALFORMED);
    let mut long = connect_route(&instance);
    let long_nonce = challenged(&mut long, attachment, client_nonce);
    let mut oversized = proof(&secret, attachment, client_nonce, long_nonce).to_vec();
    oversized.extend_from_slice(&[0u8; HELLO_BYTES]);
    long.write_all(&oversized).unwrap();
    refused_with(&mut long, KIND_MALFORMED);

    // A stranger who connects and says nothing is dropped at the exchange's deadline, counted.
    let silent = connect_route(&instance);
    let started = Instant::now();
    while counted(&daemon, "shards.route_timed_out") == 0 {
      assert!(
        started.elapsed() < CREDIT_WAIT,
        "a silent connection held its seat"
      );
      // The test thread polls the daemon's answer; no runtime wheel serves this thread (D-9 allows a test).
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_millis(10));
    }
    drop(silent);

    // The honest claim, after all of it, succeeds; a second with the same secret finds no offer.
    let stream = slates_ipc::route::claim(&instance, attachment, &secret, CREDIT_WAIT).unwrap();
    let mut guest = Guest::over(ShardsAttached {
      attachment,
      lease_epoch: None,
      stream,
    });
    guest.init();
    assert_eq!(guest.lookup(ROOT, "nothing"), Err(ENOENT));
    assert!(matches!(
      slates_ipc::route::claim(&instance, attachment, &secret, CREDIT_WAIT),
      Err(IpcError::RouteRefused {
        refusal: slates_ipc::error::RouteRefusal::NoOffer
      })
    ));
    for (kind, at_least) in [
      ("shards.route_proof", 2),
      ("shards.route_malformed", 3),
      ("shards.route_timed_out", 1),
      ("shards.route_no_offer", 1),
      ("shards.route_claimed", 1),
    ] {
      assert!(counted(&daemon, kind) >= at_least, "{kind}");
    }
    assert_eq!(counted(&daemon, "shards.route_claimed"), 1);
    drop(guest);
    drop(client);
    drop(daemon);
  }

  /// Shape: the claims the in-flight test makes: enough that, before the daemon kept its copy of each end until the
  /// client had its own, several ends arrived flushed (2026-10-24: 4 of 20 runs of the hostile test at load 35-46 gave
  /// one; this test's churn makes it likelier per claim).
  const CLAIMS_UNDER_CHURN: usize = 48;

  /// A-161 step 3, by use (`docs/bugs/2026-10-24-a-stream-end-arrived-flushed-by-the-kernel-in-flight.md`): every
  /// stream end handed over on the route arrives live while another thread opens and closes Unix sockets, each close
  /// a chance for XNU's collector of descriptors in flight to run. Do: claim many ends under that churn, and on each
  /// read its receive buffer and run `INIT` through it. Expect every end live: its receive buffer the kernel's
  /// default, never the zero a flush leaves, and its `INIT` answered.
  #[test]
  fn every_end_handed_over_arrives_live_while_sockets_churn() {
    let (daemon, instance) = single_shard_daemon("shards-route-churn");
    let mut client = Client::connect(&instance);
    let ReplyBody::Created { id } = client.call(&scratch("shared")) else {
      panic!("the volume was not created");
    };
    let stop = std::sync::atomic::AtomicBool::new(false);
    let flushed = std::thread::scope(|scope| {
      scope.spawn(|| {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
          drop(UnixStream::pair().unwrap());
        }
      });
      // The churn stops however the claims end, a failed assertion included: the scope waits for its threads, so a
      // churn left running would hold a failed test forever.
      let _stop = StopOnDrop(&stop);
      claims_under_churn(&mut client, id, &instance)
    });
    assert_eq!(
      flushed, 0,
      "{flushed} of {CLAIMS_UNDER_CHURN} ends arrived flushed"
    );
    assert_eq!(
      counted(&daemon, "shards.route_claimed"),
      u64::try_from(CLAIMS_UNDER_CHURN).unwrap()
    );
    drop(client);
    drop(daemon);
  }

  /// Sets its flag when dropped: the churn's stop, on the claims' every exit.
  struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

  impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
      self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
  }

  /// Makes [`CLAIMS_UNDER_CHURN`] offers of `id` and claims each, running `INIT` through every live end: the ends that
  /// arrived flushed, refused by the client (`Flushed`) or, were that check gone, reading a zero receive buffer.
  fn claims_under_churn(client: &mut Client, id: VolumeId, instance: &str) -> usize {
    let mut flushed = 0usize;
    for _ in 0..CLAIMS_UNDER_CHURN {
      let (attachment, secret) = offer(client, id);
      let stream = match slates_ipc::route::claim(instance, attachment, &secret, CREDIT_WAIT) {
        Ok(stream) => stream,
        Err(IpcError::RouteRefused {
          refusal: slates_ipc::error::RouteRefusal::Flushed,
        }) => {
          flushed += 1;
          continue;
        }
        Err(other) => panic!("a claim failed: {other}"),
      };
      if rustix::net::sockopt::socket_recv_buffer_size(&stream).unwrap() == 0 {
        flushed += 1;
        continue;
      }
      let mut guest = Guest::over(ShardsAttached {
        attachment,
        lease_epoch: None,
        stream,
      });
      guest.init();
    }
    flushed
  }

  /// A-161 step 3: an offer unclaimed past its life is dropped by its timer, so a claim then finds no offer, and the
  /// end it held closes, which ends the stream's attachment.
  #[test]
  fn an_unclaimed_offer_expires_and_its_attachment_ends() {
    let (daemon, instance) = single_shard_daemon("shards-route-expiry");
    let mut client = Client::connect(&instance);
    let ReplyBody::Created { id } = client.call(&scratch("shared")) else {
      panic!("the volume was not created");
    };
    let before = status(&mut client, id).attachments;
    let (attachment, secret) = offer(&mut client, id);
    assert_eq!(status(&mut client, id).attachments, before + 1);
    // The test thread waits out the offer's life; no runtime wheel serves this thread (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(PAST_EXPIRY);
    assert!(matches!(
      slates_ipc::route::claim(&instance, attachment, &secret, CREDIT_WAIT),
      Err(IpcError::RouteRefused {
        refusal: slates_ipc::error::RouteRefusal::NoOffer
      })
    ));
    assert_eq!(counted(&daemon, "shards.route_expired"), 1);
    wait_until("the expired offer's attachment outlived it", || {
      status(&mut client, id).attachments == before
    });
    drop(client);
    drop(daemon);
  }

  /// A-161 step 3: the route's socket exists only while its daemon runs, is a socket the user alone may use, in the
  /// user's own temporary directory, and is the only thing there named for the instance.
  #[test]
  fn the_route_socket_lives_with_its_daemon_and_is_the_users_alone() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let (daemon, instance) = single_shard_daemon("shards-route-node");
    let path = slates_ipc::route::path(&instance).unwrap();
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), my_uid());
    let directory = path.parent().unwrap();
    assert_eq!(
      std::fs::metadata(directory).unwrap().permissions().mode() & 0o077,
      0
    );
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let named: Vec<_> = std::fs::read_dir(directory)
      .unwrap()
      .filter_map(Result::ok)
      .filter(|entry| entry.file_name().to_string_lossy().starts_with(&stem))
      .collect();
    assert_eq!(
      named.len(),
      1,
      "only the socket is named for the instance: {named:?}"
    );
    daemon.stop();
    assert!(
      std::fs::symlink_metadata(&path).is_err(),
      "the socket went with its daemon"
    );
  }
}
