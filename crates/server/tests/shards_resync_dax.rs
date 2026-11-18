//! A Shards stream whose guest maps a file with DAX outlives its daemon's death (§4.6, the Shards form; A-365), with this
//! test as the anchor and as the device. The test makes the anchor's hold channel and hands it to each daemon as the
//! anchor does, the held resync ends and the held DAX object to the second (the handoff table `SLATES_ANCHOR_HELD`
//! names, A-462), which a daemon adopts once per process into a process-wide cell: hence a binary of its own with one
//! test. A daemon's death is a fenced stop, which ends no record and releases no hold, as a killed process
//! does.

#![cfg(unix)]
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
use slates_bridge_fuse::shards_resync::{RESYNC_MESSAGE_BYTES, ResyncKind, ResyncMessage};
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass, VolumeId,
};
use slates_mem::DAX_RANGE_BYTES;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::anchor::{held_anchor, source_of};

/// Shape: how long the test waits for a daemon, its client, a message, a counter or a frame's answer.
const WAIT: Duration = Duration::from_secs(20);
/// Format: the FUSE opcodes the guest sends (`include/uapi/linux/fuse.h`).
const LOOKUP: u32 = 1;
const OPEN: u32 = 14;
const READ: u32 = 15;
const WRITE: u32 = 16;
const CREATE: u32 = 35;
/// Format: a FUSE request's in-header, and the reply's out-header (length, errno, unique).
const IN_HEADER_LEN: usize = 40;
const OUT_HEADER_LEN: usize = 16;
/// Format: `open(2)`'s `O_RDWR | O_CREAT | O_EXCL`, `O_RDWR`, and `S_IFREG | 0644`.
const CREATE_EXCLUSIVE: u32 = 0o302;
const READ_WRITE: u32 = 0o2;
const FILE_MODE: u32 = 0o100_644;
/// Format: the top bit of a frame's length word marks a control frame, and Shards' DESCRIPTOR ask (`shards_fs`).
const CONTROL: u32 = 1 << 31;
const ASK_DESCRIPTOR: u8 = 3;
/// Shape: the file's bytes before it is mapped: two pages, so every offset the test touches lies inside the file.
const FILE_BYTES: usize = 8192;
/// Shape: where the guest stores through its map, and where another stream writes after the resync, inside the file.
const STORED_AT: usize = 4096;
const AFTER_AT: usize = 6000;
/// The guest's store through its map, never synced before the death.
const STORED: &[u8] = b"stored through the map, never synced";
/// What another stream writes into the file after the resync.
const AFTER: &[u8] = b"written by another stream after the resync";

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

/// What the anchor keeps of one stream: the held resync end and its session.
struct Hold {
  attachment: u64,
  end: OwnedFd,
  session: Vec<u8>,
}

/// What the anchor keeps of one DAX object: its id, record and object.
struct HeldObject {
  copy: u64,
  record: Vec<u8>,
  object: OwnedFd,
}

/// The hold and the session the daemon gives the anchor at a Shards attach.
fn take_hold(anchor: &OwnedFd) -> Hold {
  let Incoming::Hold { attachment, device } = next_held(anchor) else {
    panic!("no hold");
  };
  let Incoming::Session {
    attachment: told,
    session,
  } = next_held(anchor)
  else {
    panic!("no session");
  };
  assert_eq!(told, attachment);
  Hold {
    attachment,
    end: device,
    session,
  }
}

/// The DAX object the daemon gives the anchor as it answers a stream's first ask of a file.
fn take_held_object(anchor: &OwnedFd) -> HeldObject {
  let Incoming::HoldCopy {
    copy,
    record,
    object,
  } = next_held(anchor)
  else {
    panic!("the mapped file's object was not held");
  };
  HeldObject {
    copy,
    record,
    object,
  }
}

/// The message the device's resync end brings next, and the descriptors it carried; bounded by [`WAIT`].
fn next_resync(end: &OwnedFd) -> (ResyncMessage, Vec<OwnedFd>) {
  rustix::net::sockopt::set_socket_timeout(end, rustix::net::sockopt::Timeout::Recv, Some(WAIT))
    .unwrap();
  let mut bytes = [0u8; RESYNC_MESSAGE_BYTES];
  let (read, descriptors) = slates_ipc::stream::recv_message(end.as_fd(), &mut bytes).unwrap();
  (ResyncMessage::parse(&bytes[..read]).unwrap(), descriptors)
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

/// The body of a CREATE of `name` (`fuse_create_in`: flags, mode, umask, open flags, then the name).
fn create_body(name: &str) -> Vec<u8> {
  let mut body = Vec::new();
  for word in [CREATE_EXCLUSIVE, FILE_MODE, 0, 0] {
    body.extend_from_slice(&word.to_le_bytes());
  }
  body.extend_from_slice(name.as_bytes());
  body.push(0);
  body
}

/// A `fuse_write_in` or `fuse_read_in` for `len` bytes at `offset` of the open handle `fh`: the handle, the offset, the
/// size, then the flags and lock owner, all zero.
fn io_in(fh: u64, offset: usize, len: usize) -> Vec<u8> {
  let mut body = Vec::new();
  body.extend_from_slice(&fh.to_le_bytes());
  body.extend_from_slice(&u64::try_from(offset).unwrap().to_le_bytes());
  body.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
  body.extend_from_slice(&[0u8; 20]);
  body
}

/// The guest's end of a stream: Shards' framing over it.
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

  fn send_frame(&mut self, flag: u32, bytes: &[u8]) {
    let word = u32::try_from(bytes.len()).unwrap() | flag;
    self.stream.write_all(&word.to_le_bytes()).unwrap();
    self.stream.write_all(bytes).unwrap();
  }

  /// The next reply: its errno (as Linux names it) and its body.
  fn reply(&mut self) -> (i32, Vec<u8>) {
    let mut length = [0u8; 4];
    self.stream.read_exact(&mut length).unwrap();
    let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(length)).unwrap()];
    self.stream.read_exact(&mut reply).unwrap();
    let errno = -i32::from_le_bytes(reply[4..8].try_into().unwrap());
    (errno, reply[OUT_HEADER_LEN..].to_vec())
  }

  /// Sends `opcode` on `node` with `body`, under the next unique, and reads its reply.
  fn ask(&mut self, opcode: u32, node: u64, body: &[u8]) -> (i32, Vec<u8>) {
    self.unique += 1;
    let message = request(opcode, self.unique, node, body);
    self.send_frame(0, &message);
    self.reply()
  }

  /// Asks for a DAX window's descriptor of `node`, writable, over its first range, as Shards' device does at a
  /// `FUSE_SETUPMAPPING`, and reads the answer as its `shards_ipc::recv` does: a header (kind, big-endian length), the
  /// descriptor riding its first byte, then the payload. Fails the test on a refusal.
  fn descriptor(&mut self, node: u64) -> OwnedFd {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut ask = vec![ASK_DESCRIPTOR];
    ask.extend_from_slice(&node.to_le_bytes());
    ask.push(1);
    ask.extend_from_slice(&0u64.to_le_bytes());
    ask.extend_from_slice(&DAX_RANGE_BYTES.to_le_bytes());
    self.send_frame(CONTROL, &ask);
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
    assert_eq!(header[0], 0, "the descriptor ask was refused: {payload:?}");
    assert_eq!(
      descriptors.len(),
      1,
      "an answered ask carries one descriptor"
    );
    descriptors.remove(0)
  }
}

/// The first range of a DAX object, mapped as Shards' window maps it: shared, read and write. Unmapped when dropped.
struct Window {
  at: *mut u8,
  len: usize,
}

impl Window {
  fn map(object: &OwnedFd) -> Window {
    let len = usize::try_from(DAX_RANGE_BYTES).unwrap();
    // SAFETY: a fresh shared mapping of the object's first range, inside the object, which never shrinks (sealed on
    // Linux, sized once on macOS); the test reads and writes it only through this struct, which unmaps it once.
    let at = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        len,
        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
        rustix::mm::MapFlags::SHARED,
        object,
        0,
      )
    }
    .unwrap();
    Window { at: at.cast(), len }
  }

  /// `len` bytes at `within`, read as a guest reads its window: volatile loads.
  fn load(&self, within: usize, len: usize) -> Vec<u8> {
    assert!(within + len <= self.len);
    // SAFETY: inside this live mapping (asserted above).
    (within..within + len)
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

impl Drop for Window {
  fn drop(&mut self) {
    // SAFETY: the mapping `map` made, unmapped once.
    let _ = unsafe { rustix::mm::munmap(self.at.cast(), self.len) };
  }
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

/// Hands the next daemon what the anchor holds, as the anchor does (A-462): a table whose devices part is its channel
/// end, then each held resync end with its session, and whose copies part is each held object with its record.
fn hand_to_next_daemon(channel: &OwnedFd, holds: &[&Hold], objects: &[&HeldObject]) {
  let mut devices = copy(channel).to_string();
  for hold in holds {
    devices.push_str(&format!(
      ";{:x}:{}:{}",
      hold.attachment,
      copy(&hold.end),
      hex(&hold.session)
    ));
  }
  let copies: Vec<String> = objects
    .iter()
    .map(|held| {
      format!(
        "{:x}:{}:{}",
        held.copy,
        copy(&held.object),
        hex(&held.record)
      )
    })
    .collect();
  // SAFETY: this binary holds this one test; it sets the variables only between daemons, each of which reads them once
  // at its start, and no other thread of this process reads the environment meanwhile.
  unsafe {
    std::env::set_var(
      held::ENV_HELD,
      held::hand_in_process(&held::Table {
        devices,
        connections: String::new(),
        copies: copies.join(";"),
      })
      .unwrap(),
    );
  }
}

/// One Shards attach as the device and the anchor see it: the stream's end, the device's resync end and the
/// `Attached` it read there first, and the hold the anchor was given.
struct Attach {
  stream: OwnedFd,
  resync: OwnedFd,
  attached: ResyncMessage,
  hold: Hold,
}

fn attach_and_hold(client: &mut Client, volume: VolumeId, anchor: &OwnedFd) -> Attach {
  let shards = client.attach_shards(volume, Intent::Write, None).unwrap();
  let hold = take_hold(anchor);
  let resync = shards
    .resync
    .expect("the daemon hands a resync end (Linux, macOS)");
  let (attached, _) = next_resync(&resync);
  assert_eq!(attached.kind, ResyncKind::Attached);
  Attach {
    stream: shards.stream,
    resync,
    attached,
    hold,
  }
}

/// The fresh stream a device's resync end brings after the death: a `Resync` naming the attachment and token its
/// `Attached` named, with one descriptor.
fn resynced(attach: &Attach) -> Guest {
  let (message, mut descriptors) = next_resync(&attach.resync);
  assert!(
    message.resyncs(&attach.attached) && descriptors.len() == 1,
    "a Resync naming the attachment and its token, with one descriptor: {message:?}"
  );
  Guest::over(descriptors.pop().unwrap())
}

/// Before the death: `mapping`'s guest creates and writes the file, maps it with DAX and stores through the map,
/// unsynced. Returns the file's node and the window.
fn map_and_store(
  daemon: &Daemon,
  anchor: &OwnedFd,
  mapping: &mut Guest,
) -> (u64, Window, HeldObject) {
  let (errno, body) = mapping.ask(CREATE, 1, &create_body("mapped"));
  assert_eq!(errno, 0, "the file is created");
  let node = u64::from_le_bytes(body[0..8].try_into().unwrap());
  let fh = u64::from_le_bytes(body[128..136].try_into().unwrap());
  let mut write = io_in(fh, 0, FILE_BYTES);
  write.extend_from_slice(&[b'w'; FILE_BYTES]);
  let (errno, _) = mapping.ask(WRITE, node, &write);
  assert_eq!(errno, 0, "the file is written");
  let object = mapping.descriptor(node);
  let held = take_held_object(anchor);
  assert_eq!(
    counted(daemon, "shards.resync_given_up", 0),
    0,
    "the stream keeps its resync through its DAX map"
  );
  let window = Window::map(&object);
  assert_eq!(window.load(0, 4), b"wwww", "the window holds the file");
  window.store(STORED_AT, STORED);
  (node, window, held)
}

/// After the resync: `other` reads the file through its own stream, where the dead daemon's guest's unsynced store must
/// be (the successor ingested it before serving); `mapping` asks for the file's descriptor again and is given the very
/// object it maps (a store through the old window reads back through a map of the new descriptor); and `other`'s write
/// reaches the old window (the adopted backing takes every change again).
fn kept_and_coherent(
  daemon: &Daemon,
  mapping: &mut Guest,
  other: &mut Guest,
  (node, window): (u64, &Window),
) {
  assert_eq!(
    counted(daemon, "shards.dax_adopted", 1),
    1,
    "the object is taken back"
  );
  // The resynced stream's ingest of the guest's stores runs before that stream serves anything; another stream's turns
  // may come between its slices, so the read waits for the ingest to have written.
  let ingested = counted(daemon, "shards.dax_ingested", 1);
  let ledger: Vec<(&str, u64)> = daemon
    .refusals_on_every_shard()
    .unwrap()
    .into_iter()
    .filter(|(name, _)| name.starts_with("shards.") || name.starts_with("recovery."))
    .collect();
  assert!(
    ingested >= 1,
    "the adopted object's stores were ingested; the successor's ledger: {ledger:?}"
  );
  let (errno, body) = other.ask(LOOKUP, 1, b"mapped\0");
  assert_eq!(errno, 0);
  assert_eq!(u64::from_le_bytes(body[0..8].try_into().unwrap()), node);
  let (errno, body) = other.ask(OPEN, node, &[READ_WRITE.to_le_bytes(), [0; 4]].concat());
  assert_eq!(errno, 0);
  let fh = u64::from_le_bytes(body[0..8].try_into().unwrap());
  let (errno, bytes) = other.ask(READ, node, &io_in(fh, STORED_AT, STORED.len()));
  assert_eq!(
    (errno, bytes.as_slice()),
    (0, STORED),
    "the guest's unsynced store reached the file, ingested before the successor served anything"
  );
  let again = Window::map(&mapping.descriptor(node));
  window.store(100, b"same");
  assert_eq!(
    again.load(100, 4),
    b"same",
    "the device is given the very object it maps"
  );
  assert_eq!(
    again.load(STORED_AT, STORED.len()),
    STORED,
    "and nothing refilled the guest's store away"
  );
  let mut write = io_in(fh, AFTER_AT, AFTER.len());
  write.extend_from_slice(AFTER);
  let (errno, _) = other.ask(WRITE, node, &write);
  assert_eq!(errno, 0);
  assert_eq!(
    window.load(AFTER_AT, AFTER.len()),
    AFTER,
    "another stream's write reaches the object the guest maps"
  );
}

/// A-365 (Shards' coordinator's condition 4b for DAX). Do: as the anchor, hand a daemon the hold channel; attach a volume
/// twice in the Shards form; on the first stream create a file, write two pages, ask for its DAX descriptor writable, map
/// it and store through the map, never syncing; kill the daemon (a fenced stop); start a second daemon over the same
/// segment with the held resync ends and the held object handed back. Expect: the first daemon to hold the mapped
/// file's object at the anchor and keep the stream's resync; the second to resync both streams and take the object
/// back; the guest's unsynced store read back through the other stream (ingested before anything was served); the
/// device's new ask for the file answered with the very object it maps, the store intact; and another stream's write
/// after the resync reaching the guest's window.
#[test]
fn a_streams_dax_map_outlives_its_daemon_with_its_unsynced_stores() {
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
  let instance = format!("srv-shards-resync-dax-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = held_anchor("shards-resync-dax", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: "resync-dax".to_owned(),
      size: SizeClass::Bounded { limit: 64 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let mapping_attach = attach_and_hold(&mut client, volume, &anchor);
  let other_attach = attach_and_hold(&mut client, volume, &anchor);
  let mut old_mapping = Guest::over(mapping_attach.stream.try_clone().unwrap());
  let (node, window, held_object) = map_and_store(&first, &anchor, &mut old_mapping);
  first.inject_consensus_failure().unwrap();
  first.stop();
  drop(client);

  hand_to_next_daemon(
    &daemon_end,
    &[&mapping_attach.hold, &other_attach.hold],
    &[&held_object],
  );
  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  assert_eq!(
    counted(&second, "shards.resynced", 2),
    2,
    "both streams are resynced"
  );
  let mut mapping = resynced(&mapping_attach);
  let mut other = resynced(&other_attach);
  kept_and_coherent(&second, &mut mapping, &mut other, (node, &window));
  drop(window);
  drop(mapping);
  drop(other);
  second.stop();
  drop(old_mapping);
  drop(held_object);
  drop(segment);
  drop(anchor);
}
