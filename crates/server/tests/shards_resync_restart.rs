//! A Shards stream outlives its daemon's death (§4.6, the Shards form; A-316, Shards' coordinator's conditions 4a and
//! 4b), with this test as the anchor and as the device. The test makes the anchor's hold channel and hands it to each
//! daemon as the anchor does (`SLATES_ANCHOR_DEVICES`, the held resync ends with their sessions for the second), which
//! a daemon adopts once per process into a process-wide cell: hence a binary of its own with one test. A daemon's death
//! is a fenced stop, which ends no record and releases no hold, as a killed process does. Linux and macOS (A-339), where
//! the client claims both ends on the socket route.

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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_anchor::held::{self, Incoming};
use slates_bridge_fuse::shards_resync::{RESYNC_MESSAGE_BYTES, ResyncKind, ResyncMessage};
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass, VolumeId,
};
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::anchor::{held_anchor, source_of};

/// Shape: how long the test waits for a daemon, its client, a message, a counter or a frame's answer.
const WAIT: Duration = Duration::from_secs(20);
/// Format: the FUSE opcodes the guest sends (`include/uapi/linux/fuse.h`).
const LOOKUP: u32 = 1;
const FORGET: u32 = 2;
const UNLINK: u32 = 10;
const READ: u32 = 15;
const WRITE: u32 = 16;
const RELEASE: u32 = 18;
const SETXATTR: u32 = 21;
const GETXATTR: u32 = 22;
const INIT: u32 = 26;
const CREATE: u32 = 35;
/// Format: a FUSE request's in-header, and the reply's out-header (length, errno, unique).
const IN_HEADER_LEN: usize = 40;
const OUT_HEADER_LEN: usize = 16;
/// Format: `open(2)`'s `O_RDWR | O_CREAT | O_EXCL`, and `S_IFREG | 0644`.
const CREATE_EXCLUSIVE: u32 = 0o302;
const FILE_MODE: u32 = 0o100_644;
/// Format: Linux's `EEXIST`.
const EEXIST: i32 = 17;
/// Format: `FUSE_POSIX_ACL` and `FUSE_SETXATTR_EXT` in `fuse_init_in.flags` (`include/uapi/linux/fuse.h`), which a Linux
/// guest's kernel offers; and where the anchor's session of a stream keeps its form (past the token and the owner
/// view's four words), with the form's bit for the extended SETXATTR (A-378).
const INIT_POSIX_ACL: u32 = 1 << 20;
const INIT_SETXATTR_EXT: u32 = 1 << 29;
const SESSION_FORM_AT: usize = slates_bridge_fuse::shards_resync::TOKEN_BYTES + 16;
const FORM_SETXATTR_EXT: u64 = 1;
/// The bytes the second stream writes after the last publication before the death.
const WRITTEN: &[u8] = b"acknowledged after the last publication";
/// Shape: the bytes of the file the writer's guest lets go after the last publication: enough to show in the volume's
/// referenced bytes.
const FORGOTTEN_BYTES: usize = 64 << 10;
/// Shape: how long the successor's `Resync`s wait in the devices' channels while another thread opens and closes
/// sockets: thousands of runs of XNU's collector of descriptors in flight, which flushed the anchor's held end in 3 of 3
/// runs at this wait when its daemon kept no copy (A-339).
const RESYNCS_IN_FLIGHT: Duration = Duration::from_millis(200);

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

/// A CREATE of `name` in the root (`fuse_create_in`: flags, mode, umask, open flags, then the name).
fn create(unique: u64, name: &str) -> Vec<u8> {
  let mut body = Vec::new();
  for word in [CREATE_EXCLUSIVE, FILE_MODE, 0, 0] {
    body.extend_from_slice(&word.to_le_bytes());
  }
  body.extend_from_slice(name.as_bytes());
  body.push(0);
  request(CREATE, unique, 1, &body)
}

/// The guest's end of a stream: Shards' framing over it.
struct Guest {
  stream: UnixStream,
}

impl Guest {
  fn over(end: OwnedFd) -> Guest {
    let stream = UnixStream::from(end);
    stream.set_read_timeout(Some(WAIT)).unwrap();
    Guest { stream }
  }

  fn send(&mut self, request: &[u8]) {
    let mut frame = u32::try_from(request.len()).unwrap().to_le_bytes().to_vec();
    frame.extend_from_slice(request);
    self.stream.write_all(&frame).unwrap();
  }

  /// The next reply: its errno (as Linux names it), its unique and its body.
  fn reply(&mut self) -> (i32, u64, Vec<u8>) {
    let mut length = [0u8; 4];
    self.stream.read_exact(&mut length).unwrap();
    let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(length)).unwrap()];
    self.stream.read_exact(&mut reply).unwrap();
    let errno = -i32::from_le_bytes(reply[4..8].try_into().unwrap());
    let unique = u64::from_le_bytes(reply[8..16].try_into().unwrap());
    (errno, unique, reply[OUT_HEADER_LEN..].to_vec())
  }

  fn ask(&mut self, request: &[u8]) -> (i32, u64, Vec<u8>) {
    self.send(request);
    self.reply()
  }
}

/// The node and handle a successful CREATE's reply names (`fuse_entry_out` begins with the node; `fuse_open_out`, after
/// the entry's 128 bytes, with the handle).
fn created(body: &[u8]) -> (u64, u64) {
  let word = |at: usize| u64::from_le_bytes(body[at..at + 8].try_into().unwrap());
  (word(0), word(128))
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

/// The value the anchor hands a daemon in `SLATES_ANCHOR_DEVICES`: its channel end, then each held end with its session
/// (`slates_anchor::held::Held::env_value`'s form), each a copy the daemon may own.
fn handed_back(channel: &OwnedFd, holds: &[&Hold]) -> String {
  let copy = |fd: &OwnedFd| {
    rustix::io::fcntl_dupfd_cloexec(fd, 3)
      .unwrap()
      .into_raw_fd()
  };
  let mut value = copy(channel).to_string();
  for hold in holds {
    let session: String = hold
      .session
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    value.push_str(&format!(
      ";{:x}:{}:{session}",
      hold.attachment,
      copy(&hold.end)
    ));
  }
  value
}

/// One Shards attach as the device and the anchor see it: the stream's end, the device's resync end and the `Attached`
/// it read there first, and the hold the anchor was given.
struct Attach {
  stream: OwnedFd,
  resync: OwnedFd,
  attached: ResyncMessage,
  hold: Hold,
}

/// Attaches `volume` in the Shards form through `client`, takes the hold the anchor is given, and reads the device's
/// first message, `Attached`.
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

/// Starts the successor over `segment` while another thread opens and closes Unix sockets, and holds the devices' reads
/// of their `Resync`s off for [`RESYNCS_IN_FLIGHT`] once it has sent all `streams` of them (A-339).
fn successor_under_churn(
  profile: &slates_machine::MachineProfile,
  config: DaemonConfig,
  segment: &common::anchor::Anchor,
  streams: u64,
) -> Daemon {
  let stop = AtomicBool::new(false);
  std::thread::scope(|scope| {
    scope.spawn(|| churn(&stop));
    let _stop = StopOnDrop(&stop);
    let second = Daemon::start(profile, config, source_of(segment)).unwrap();
    assert_eq!(
      counted(&second, "shards.resynced", streams),
      streams,
      "each kept stream is resynced"
    );
    // The test thread holds the devices' reads off while the churn runs; no runtime serves it (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(RESYNCS_IN_FLIGHT);
    second
  })
}

/// Sets `SLATES_ANCHOR_DEVICES` for the daemon about to start.
fn hand_to_next_daemon(value: &str) {
  // SAFETY: this binary holds this one test; it sets the variable only between daemons, each of which reads it once at
  // its start, and no other thread of this process reads the environment meanwhile.
  unsafe {
    std::env::set_var(held::ENV_DEVICES, value);
  }
}

/// A `fuse_write_in` or `fuse_read_in` for `len` bytes at offset 0 of the open handle `fh`: the handle, the offset, the
/// size, then the flags and lock owner, all zero.
fn io_in(fh: u64, len: usize) -> Vec<u8> {
  let mut body = Vec::new();
  body.extend_from_slice(&fh.to_le_bytes());
  body.extend_from_slice(&0u64.to_le_bytes());
  body.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
  body.extend_from_slice(&[0u8; 20]);
  body
}

/// Before the death, on `daemon`: `writer` creates and opens a file; the daemon withholds its replies while `creator`
/// sends an exclusive CREATE, applied and published with its reply and never answered; then `writer` writes
/// [`WRITTEN`], acknowledged after that last publication. Returns the file's node and handle.
fn create_unanswered_then_write(
  daemon: &Daemon,
  creator: &mut Guest,
  writer: &mut Guest,
) -> (u64, u64) {
  let (errno, _, body) = writer.ask(&create(11, "data"));
  assert_eq!(errno, 0, "the second stream creates its file");
  let (data, fh) = created(&body);
  daemon.withhold_shards_replies(true).unwrap();
  creator.send(&create(41, "once"));
  assert_eq!(
    counted(daemon, "shards.reply_withheld", 1),
    1,
    "the exclusive CREATE was applied and published, its reply never written"
  );
  daemon.withhold_shards_replies(false).unwrap();
  let mut write = io_in(fh, WRITTEN.len());
  write.extend_from_slice(WRITTEN);
  let (errno, _, _) = writer.ask(&request(WRITE, 12, data, &write));
  assert_eq!(
    errno, 0,
    "the write is acknowledged after the last publication"
  );
  (data, fh)
}

/// Before the death, A-378: `writer`'s kernel sends the INIT a Linux guest's sends, offering `FUSE_POSIX_ACL` and
/// `FUSE_SETXATTR_EXT`; it is answered, and the anchor told a session for `hold`'s attachment keeping the extended form,
/// which becomes the session the anchor hands the successor.
fn init_in_the_extended_form(anchor: &OwnedFd, writer: &mut Guest, hold: &mut Hold) {
  let mut init = Vec::new();
  for word in [7u32, 41, 128 << 10, INIT_POSIX_ACL | INIT_SETXATTR_EXT] {
    init.extend_from_slice(&word.to_le_bytes());
  }
  let (errno, _, _) = writer.ask(&request(INIT, 10, 0, &init));
  assert_eq!(errno, 0, "the guest's INIT is answered");
  let Incoming::Session {
    attachment,
    session,
  } = next_held(anchor)
  else {
    panic!("the INIT told the anchor no session");
  };
  assert_eq!(attachment, hold.attachment);
  let form = u64::from_le_bytes(
    session[SESSION_FORM_AT..SESSION_FORM_AT + 8]
      .try_into()
      .unwrap(),
  );
  assert_eq!(
    form, FORM_SETXATTR_EXT,
    "the session keeps the extended form"
  );
  hold.session = session;
}

/// After the death, A-378: the successor parses `writer`'s SETXATTR in the extended form its kernel took before the
/// death (the size, the flags, the setter's flags and a pad before the name), which no INIT on the fresh stream repeats:
/// a user attribute set on `data` that way is served and read back.
fn the_successor_parses_the_extended_form(writer: &mut Guest, data: u64) {
  let value = b"set in the extended form";
  let mut set = u32::try_from(value.len()).unwrap().to_le_bytes().to_vec();
  set.extend_from_slice(&[0u8; 12]);
  set.extend_from_slice(b"user.form\0");
  set.extend_from_slice(value);
  let (errno, _, _) = writer.ask(&request(SETXATTR, 14, data, &set));
  assert_eq!(errno, 0, "the extended SETXATTR is served after the resync");
  let mut get = 64u32.to_le_bytes().to_vec();
  get.extend_from_slice(&0u32.to_le_bytes());
  get.extend_from_slice(b"user.form\0");
  let (errno, _, read) = writer.ask(&request(GETXATTR, 15, data, &get));
  assert_eq!(
    (errno, read.as_slice()),
    (0, &value[..]),
    "its name and value were found past the extended head"
  );
}

/// Before the last publication, A-390: `writer`'s guest creates a file, writes [`FORGOTTEN_BYTES`] into it, releases it
/// and unlinks it, the unlink's barrier publishing the nameless file its lookup still holds. Returns the file's node.
fn a_nameless_file_held_by_its_lookup(writer: &mut Guest) -> u64 {
  let (errno, _, body) = writer.ask(&create(21, "forgotten"));
  assert_eq!(
    errno, 0,
    "the writer's guest creates the file it will let go"
  );
  let (node, fh) = created(&body);
  let mut write = io_in(fh, FORGOTTEN_BYTES);
  write.extend_from_slice(&vec![0x5a; FORGOTTEN_BYTES]);
  let (errno, _, _) = writer.ask(&request(WRITE, 22, node, &write));
  assert_eq!(errno, 0, "and writes it");
  let mut release = fh.to_le_bytes().to_vec();
  release.extend_from_slice(&[0u8; 16]);
  let (errno, _, _) = writer.ask(&request(RELEASE, 23, node, &release));
  assert_eq!(errno, 0, "and closes it");
  let (errno, _, _) = writer.ask(&request(UNLINK, 24, 1, b"forgotten\0"));
  assert_eq!(errno, 0, "and unlinks it, its lookup still held");
  node
}

/// After the last publication, A-390: `writer`'s guest gives the nameless file's lookup back (`FORGET`, which takes no
/// reply), and the volume lets the file go: its referenced bytes drop by the file's at least. Returns them then.
fn the_guest_lets_it_go(
  client: &mut Client,
  volume: VolumeId,
  writer: &mut Guest,
  node: u64,
) -> u64 {
  let held = client.status(volume).unwrap().referenced_bytes;
  writer.send(&request(FORGET, 25, node, &1u64.to_le_bytes()));
  let started = Instant::now();
  loop {
    let now = client.status(volume).unwrap().referenced_bytes;
    if now.saturating_add(u64::try_from(FORGOTTEN_BYTES).unwrap()) <= held {
      return now;
    }
    assert!(
      started.elapsed() < WAIT,
      "the FORGET let the nameless file go: {now} referenced bytes of {held}"
    );
    std::thread::yield_now();
  }
}

/// After the death: each device's resync end (the first of each pair) brings a `Resync` naming its attachment and the
/// token its `Attached` (the second) named, with one descriptor, the end of the fresh stream it is served on, live (a
/// flush in flight zeroes its receive buffer, A-339). Returns each fresh end, in the pairs' order.
fn resynced<const N: usize>(ends: [(&OwnedFd, &ResyncMessage); N]) -> [OwnedFd; N] {
  ends.map(|(resync, attached)| {
    let (message, mut descriptors) = next_resync(resync);
    assert!(
      message.resyncs(attached) && descriptors.len() == 1,
      "a Resync naming the attachment and its token, with one descriptor: {message:?}, {} descriptors",
      descriptors.len()
    );
    let fresh = descriptors.pop().unwrap();
    assert_ne!(
      rustix::net::sockopt::socket_recv_buffer_size(&fresh).unwrap(),
      0,
      "the fresh end arrives live: its daemon kept a copy while it rode the Resync"
    );
    fresh
  })
}

/// Sets its flag when dropped: the churn's stop, on every exit, a failed assertion included.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
  fn drop(&mut self) {
    self.0.store(true, Ordering::Relaxed);
  }
}

/// Opens and closes Unix socket pairs until `stop`: each close may run XNU's collector of descriptors in flight.
fn churn(stop: &AtomicBool) {
  while !stop.load(Ordering::Relaxed) {
    drop(UnixStream::pair().unwrap());
  }
}

/// The device of `leaving` goes before it takes its fresh stream: it closes the fresh end and its end of the resync
/// channel without a byte. The anchor is then told to release `leaving`'s hold, as the attachment ended (its successor
/// let its own copy of the fresh end go when the channel woke, so the stream saw its peer gone), counted.
fn a_device_gone_before_its_first_frame_ends_its_attachment(
  daemon: &Daemon,
  anchor: &OwnedFd,
  leaving: u64,
  (fresh, resync): (OwnedFd, OwnedFd),
) {
  drop(fresh);
  drop(resync);
  let started = Instant::now();
  loop {
    if let Incoming::Release { attachment } = next_held(anchor)
      && attachment == leaving
    {
      break;
    }
    assert!(
      started.elapsed() < WAIT,
      "the gone device's attachment never ended"
    );
  }
  assert_eq!(counted(daemon, "shards.resync_abandoned", 1), 1);
}

/// On the fresh streams `daemon` serves: `creator` resends the exclusive CREATE whole, under its unique, and is answered
/// success from the dead daemon's published reply (a second application would answer `EEXIST`), counted as replayed,
/// both streams counted resynced; `writer` reads back from `file` (node and handle) what it wrote before the death; and
/// the file is there once, a new exclusive CREATE of it refused `EEXIST`.
fn answered_once_and_kept(
  daemon: &Daemon,
  creator: &mut Guest,
  writer: &mut Guest,
  (data, fh): (u64, u64),
) {
  let (errno, unique, _) = creator.ask(&create(41, "once"));
  assert_eq!(
    (errno, unique),
    (0, 41),
    "the resent CREATE is answered from its published reply, never applied twice"
  );
  assert_eq!(counted(daemon, "shards.replayed", 1), 1);
  let (errno, _, bytes) = writer.ask(&request(READ, 13, data, &io_in(fh, WRITTEN.len())));
  assert_eq!(
    (errno, bytes.as_slice()),
    (0, WRITTEN),
    "the write acknowledged after the last publication outlived the daemon"
  );
  assert_eq!(
    counted(daemon, "shards.resync_taken", 2),
    2,
    "each device that wrote showed it held its fresh end, and its daemon's copy went"
  );
  let (errno, _, _) = creator.ask(&request(LOOKUP, 42, 1, b"once\0"));
  assert_eq!(errno, 0, "the file is there");
  let (errno, _, _) = creator.ask(&create(43, "once"));
  assert_eq!(
    errno, EEXIST,
    "once: a new exclusive CREATE of it is refused"
  );
}

/// A-316 (Shards' coordinator's conditions 4a and 4b, slates' half), A-339 (macOS too), A-378, A-390. Do: as the
/// anchor, hand a daemon the hold channel; attach a volume three times in the Shards form; the second stream's kernel
/// sends an INIT taking the extended SETXATTR form; on the second stream create, write, close and unlink a file, its
/// lookup still held, the unlink's barrier publishing it nameless; create a file and open it on the second stream;
/// withhold the daemon's replies and send an exclusive CREATE on the first stream, whose barrier publishes it with its
/// reply; then write on the second stream, acknowledged after that last publication, and give the nameless file's
/// lookup back there (a FORGET, no publication after it), the volume's referenced bytes dropping by the file's; kill
/// the daemon (a fenced stop: no record ended, no hold released); start a second daemon over the same segment with the
/// held ends handed back, while another thread opens and closes sockets and the devices leave their `Resync`s unread
/// for a while; then the third device goes without a byte on its fresh stream. Expect each device to be handed a fresh
/// stream in a `Resync` that names its attachment and the token its `Attached` named, with one descriptor, live; the
/// volume's referenced bytes where the FORGET left them (the successor gave the logged FORGET back; before A-390 it
/// restored the lookup the image held, and the nameless file stayed referenced until the stream ended); the resent
/// CREATE answered with success under its unique, from the published reply (a second application would answer
/// `EEXIST`), counted as replayed; the acknowledged write read back on the second stream (the write log); a SETXATTR in
/// the extended form served there and read back, parsed as before the death though no INIT repeated it; the file there
/// once, a new exclusive CREATE of it refused `EEXIST`; each writing device counted as having taken its fresh end; and
/// the third attachment ended, its hold released, counted as abandoned.
#[test]
fn a_stream_is_resynced_after_its_daemons_death_and_its_unanswered_create_is_answered_once() {
  let (anchor, daemon_end) = held::channel().unwrap();
  hand_to_next_daemon(
    &daemon_end
      .as_fd()
      .try_clone_to_owned()
      .unwrap()
      .into_raw_fd()
      .to_string(),
  );
  let profile = common::machine_profile();
  let instance = format!("srv-shards-resync-restart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = held_anchor("shards-resync-restart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: "resync-restart".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let [creating, mut writing, leaving] =
    [(); 3].map(|()| attach_and_hold(&mut client, volume, &anchor));

  let mut old_writer = Guest::over(writing.stream);
  let mut old_creator = Guest::over(creating.stream);
  init_in_the_extended_form(&anchor, &mut old_writer, &mut writing.hold);
  let forgotten = a_nameless_file_held_by_its_lookup(&mut old_writer);
  let file = create_unanswered_then_write(&first, &mut old_creator, &mut old_writer);
  let let_go = the_guest_lets_it_go(&mut client, volume, &mut old_writer, forgotten);
  first.inject_consensus_failure().unwrap();
  first.stop();
  drop(client);

  hand_to_next_daemon(&handed_back(
    &daemon_end,
    &[&creating.hold, &writing.hold, &leaving.hold],
  ));
  let second = successor_under_churn(&profile, config, &segment, 3);
  assert_eq!(
    connect(&instance).status(volume).unwrap().referenced_bytes,
    let_go,
    "the successor gave the guest's FORGET after the last publication back: the nameless file stays let go"
  );
  let [creator, writer, leaving_fresh] = resynced([
    (&creating.resync, &creating.attached),
    (&writing.resync, &writing.attached),
    (&leaving.resync, &leaving.attached),
  ]);
  let (mut creator, mut writer) = (Guest::over(creator), Guest::over(writer));
  answered_once_and_kept(&second, &mut creator, &mut writer, file);
  the_successor_parses_the_extended_form(&mut writer, file.0);
  a_device_gone_before_its_first_frame_ends_its_attachment(
    &second,
    &anchor,
    leaving.hold.attachment,
    (leaving_fresh, leaving.resync),
  );
  drop(writer);
  drop(creator);
  second.stop();
  drop(old_writer);
  drop(old_creator);
  drop(leaving.stream);
  drop(segment);
  drop(anchor);
}
