//! A Shards stream's resync channel, its daemon's half (§4.6, the Shards form; A-316), with this test as the anchor.
//! The test makes the anchor's hold channel and hands the daemon its end as the anchor does, through the handoff table
//! `SLATES_ANCHOR_HELD` names (A-462), which a daemon adopts once per process into a process-wide cell
//! (`crate::anchor_hold`):
//! hence a binary of its own with one test, so no other test's daemon adopts the channel. The client attaches through
//! the daemon's real route, so both ends reach it the way they reach `slates shards`: Linux's control socket, and on
//! macOS the socket route, where the resync end rides beside the stream's (A-339).

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
use slates_bridge_fuse::shards_resync::{
  RESYNC_MESSAGE_BYTES, ResyncKind, ResyncMessage, TOKEN_BYTES,
};
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, ShardsStream, SizeClass,
};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: how long the test waits for the daemon, its client, a message to the anchor or a frame's answer.
const WAIT: Duration = Duration::from_secs(20);
/// Shape: how long the attaches' holds wait in the anchor's channel while another thread opens and closes sockets:
/// thousands of runs of XNU's collector of descriptors in flight, which flushed an end whose every copy rode a message
/// in 17 to 40 of 48 claims taken in well under a millisecond each (A-161's record).
const HOLDS_IN_FLIGHT: Duration = Duration::from_millis(200);
/// Format: a FUSE request's in-header (`struct fuse_in_header`), and `FUSE_SETXATTR`'s and `FUSE_INIT`'s opcodes.
const IN_HEADER_LEN: usize = 40;
const SETXATTR: u32 = 21;
const INIT: u32 = 26;
/// Format: the bytes before a reply's errno in its out-header (its length word).
const OUT_LEN_BYTES: usize = 4;
/// Format: `FUSE_POSIX_ACL` and `FUSE_SETXATTR_EXT` in `fuse_init_in.flags` (`include/uapi/linux/fuse.h`), which a Linux
/// guest's kernel offers.
const INIT_POSIX_ACL: u32 = 1 << 20;
const INIT_SETXATTR_EXT: u32 = 1 << 29;
/// Format: the length word's bit that marks Shards' own control frames (`shards_fs::CONTROL_FRAME`), and its SAVE and
/// RESTORE asks (`shards_fs::ASK_*`).
const CONTROL: u32 = 1 << 31;
const ASK_SAVE: u8 = 1;
const ASK_RESTORE: u8 = 2;

/// The product's own deadlines, as the recovery oracle uses them.
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

/// The anchor's next message from the daemon, waited for up to [`WAIT`].
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

/// The message the device's resync end brings next, parsed, and the descriptors it carried; every read on the end is
/// bounded by [`WAIT`], so a message that never comes fails the test rather than hangs it.
fn next_resync(end: &OwnedFd) -> (ResyncMessage, usize) {
  rustix::net::sockopt::set_socket_timeout(end, rustix::net::sockopt::Timeout::Recv, Some(WAIT))
    .unwrap();
  let mut bytes = [0u8; RESYNC_MESSAGE_BYTES];
  let (read, descriptors) = slates_ipc::stream::recv_message(end.as_fd(), &mut bytes).unwrap();
  (
    ResyncMessage::parse(&bytes[..read]).unwrap(),
    descriptors.len(),
  )
}

/// A frame of Shards' framing: the request's length, little-endian, then the request.
fn frame(request: &[u8]) -> Vec<u8> {
  let mut frame = u32::try_from(request.len()).unwrap().to_le_bytes().to_vec();
  frame.extend_from_slice(request);
  frame
}

/// A FUSE request of `opcode` under `unique` on the share's root, from the guest's root (uid 0), carrying `body`.
fn on_the_root(opcode: u32, unique: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut request = vec![0u8; IN_HEADER_LEN];
  request[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  request[4..8].copy_from_slice(&opcode.to_le_bytes());
  request[8..16].copy_from_slice(&unique.to_le_bytes());
  request[16..24].copy_from_slice(&1u64.to_le_bytes());
  request.extend_from_slice(body);
  request
}

/// The SETXATTR of Shards' owner word `value` on the share's root, from the guest's root (uid 0), as Shards' init sends it.
fn owner_word(value: &[u8]) -> Vec<u8> {
  let mut body = u32::try_from(value.len()).unwrap().to_le_bytes().to_vec();
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(b"trusted.shards.owner\0");
  body.extend_from_slice(value);
  on_the_root(SETXATTR, 7, &body)
}

/// The INIT a Linux guest's kernel sends (`fuse_init_in`: major 7, minor 41, max_readahead, flags), offering
/// `FUSE_POSIX_ACL` and `FUSE_SETXATTR_EXT`.
fn init_offering_the_extended_form() -> Vec<u8> {
  let mut body = Vec::new();
  for word in [7u32, 41, 128 << 10, INIT_POSIX_ACL | INIT_SETXATTR_EXT] {
    body.extend_from_slice(&word.to_le_bytes());
  }
  on_the_root(INIT, 8, &body)
}

/// Reads one reply frame from `stream` and returns its errno, as Linux names it.
fn reply_errno(stream: &mut UnixStream) -> i32 {
  let mut length = [0u8; 4];
  stream.read_exact(&mut length).unwrap();
  let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(length)).unwrap()];
  stream.read_exact(&mut reply).unwrap();
  -i32::from_le_bytes(reply[OUT_LEN_BYTES..OUT_LEN_BYTES + 4].try_into().unwrap())
}

/// Sets its flag when dropped: the churn's stop, on every exit of the attaches, a failed assertion included.
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

/// One Shards attach as the anchor sees it: the device's resync end of `shards` reads `Attached` naming its attachment
/// and a token, with no descriptor; the anchor is given the daemon's end by a hold for that attachment, live (a flush in
/// flight zeroes its receive buffer, A-339), then a session of the same token and the identity view. Returns the token
/// and the anchor's held end.
fn held_with_its_token(anchor: &OwnedFd, shards: &ShardsStream) -> ([u8; TOKEN_BYTES], OwnedFd) {
  let attachment = shards.attachment.attachment;
  let resync = shards
    .resync
    .as_ref()
    .expect("the daemon hands a resync end (Linux, macOS)");
  let (attached, carried) = next_resync(resync);
  assert_eq!(
    (attached.kind, attached.attachment, carried),
    (ResyncKind::Attached, attachment, 0),
    "the device first reads its attachment's token, with no descriptor"
  );
  assert_ne!(attached.token, [0; TOKEN_BYTES], "a token is minted");
  let Incoming::Hold {
    attachment: held,
    device,
  } = next_held(anchor)
  else {
    panic!("the daemon's end was not held first");
  };
  assert_eq!(held, attachment, "the hold names its attachment");
  assert_ne!(
    rustix::net::sockopt::socket_recv_buffer_size(&device).unwrap(),
    0,
    "the daemon's end arrives live: the daemon kept a copy while it rode its hold, so no collector took it for garbage"
  );
  let Incoming::Session {
    attachment: told,
    session,
  } = next_held(anchor)
  else {
    panic!("the hold's session did not follow it");
  };
  assert_eq!(told, attachment);
  assert_eq!(
    session[..TOKEN_BYTES],
    attached.token,
    "the anchor keeps the token the device was told"
  );
  assert!(
    session[TOKEN_BYTES..].iter().all(|byte| *byte == 0),
    "and the identity view and the short SETXATTR form, until the guest names its owner or its INIT keeps the extended \
     form: {session:?}"
  );
  (attached.token, device)
}

/// Format: where a session keeps its connection's form (a little-endian `u64` past the token and the owner view's four
/// words), and the form's bit for the extended SETXATTR (A-378).
const SESSION_FORM_AT: usize = TOKEN_BYTES + 16;
const FORM_SETXATTR_EXT: u64 = 1;

/// The anchor's next message, a session for `attachment` keeping its `token`: the swap it carries (the volume's uid,
/// then the run's uid and gid) and whether it keeps the extended SETXATTR form.
fn session_told(
  anchor: &OwnedFd,
  attachment: u64,
  token: [u8; TOKEN_BYTES],
) -> ((u32, u32, u32), bool) {
  let Incoming::Session {
    attachment: told,
    session,
  } = next_held(anchor)
  else {
    panic!("no session followed");
  };
  assert_eq!(told, attachment);
  assert_eq!(session[..TOKEN_BYTES], token, "the token stays");
  let word = |at: usize| u32::from_le_bytes(session[at..at + 4].try_into().unwrap());
  let form = u64::from_le_bytes(
    session[SESSION_FORM_AT..SESSION_FORM_AT + 8]
      .try_into()
      .unwrap(),
  );
  (
    (
      word(TOKEN_BYTES),
      word(TOKEN_BYTES + 8),
      word(TOKEN_BYTES + 12),
    ),
    form & FORM_SETXATTR_EXT != 0,
  )
}

/// The guest's root names its run's owner `1000:1001` on `stream` (Shards' owner word, A-281): the reply succeeds, and
/// the anchor is told a session for `attachment` that keeps its `token` and carries the swap, the root's owner shown
/// as the run's 1000:1001.
fn owner_word_updates_the_session(
  anchor: &OwnedFd,
  stream: &mut UnixStream,
  attachment: u64,
  token: [u8; TOKEN_BYTES],
) {
  stream.write_all(&frame(&owner_word(b"1000:1001"))).unwrap();
  assert_eq!(
    reply_errno(stream),
    0,
    "the guest's root names its run's owner"
  );
  assert_eq!(
    session_told(anchor, attachment, token),
    ((rustix::process::getuid().as_raw(), 1000, 1001), false),
    "the swap: the root's owner shown as the run's 1000:1001"
  );
}

/// A-378. The guest's kernel sends an INIT offering `FUSE_POSIX_ACL` and `FUSE_SETXATTR_EXT` on `stream`, after its
/// root named its run's owner: the reply succeeds, and the anchor is told a session for `attachment` that keeps its
/// `token` and its swap and adds the extended form, so a successor parses the guest's SETXATTRs as this daemon does.
fn init_keeps_the_extended_form_in_the_session(
  anchor: &OwnedFd,
  stream: &mut UnixStream,
  attachment: u64,
  token: [u8; TOKEN_BYTES],
) {
  stream
    .write_all(&frame(&init_offering_the_extended_form()))
    .unwrap();
  assert_eq!(reply_errno(stream), 0, "the guest's INIT is answered");
  assert_eq!(
    session_told(anchor, attachment, token),
    ((rustix::process::getuid().as_raw(), 1000, 1001), true),
    "the swap kept, the extended form added"
  );
}

/// One of Shards' control asks on `stream`: `ask` and `body` in a control frame; whether the answer granted it, and
/// what it brought.
fn control(stream: &mut UnixStream, ask: u8, body: &[u8]) -> (bool, Vec<u8>) {
  let mut asked = vec![ask];
  asked.extend_from_slice(body);
  let word = u32::try_from(asked.len()).unwrap() | CONTROL;
  stream.write_all(&word.to_le_bytes()).unwrap();
  stream.write_all(&asked).unwrap();
  let mut length = [0u8; 4];
  stream.read_exact(&mut length).unwrap();
  let length = u32::from_le_bytes(length);
  assert_ne!(
    length & CONTROL,
    0,
    "a control ask is answered by a control frame"
  );
  let mut answer = vec![0u8; usize::try_from(length & !CONTROL).unwrap()];
  stream.read_exact(&mut answer).unwrap();
  (answer.first() == Some(&0), answer[1..].to_vec())
}

/// A-378, and the restore's half of A-281. The guest of `saved` is saved (SAVE), and its tables restored on `fresh`, a
/// stream of the same volume that has sent nothing (RESTORE as its first frame): both granted, and the anchor is told a
/// session for `fresh`'s attachment that keeps its own token and takes the saved guest's swap and extended form, so a
/// successor shows the restored guest what this daemon does and parses its SETXATTRs as this daemon does. Before, a
/// restored stream's held session kept the identity view it was attached with.
fn a_restore_tells_the_anchor_the_saved_guests_session(
  anchor: &OwnedFd,
  saved: &mut UnixStream,
  fresh: &mut UnixStream,
  (attachment, token): (u64, [u8; TOKEN_BYTES]),
) {
  let (granted, tables) = control(saved, ASK_SAVE, &[]);
  assert!(
    granted,
    "the SAVE is granted: {:?}",
    String::from_utf8_lossy(&tables)
  );
  let (granted, why) = control(fresh, ASK_RESTORE, &tables);
  assert!(
    granted,
    "the RESTORE is granted: {:?}",
    String::from_utf8_lossy(&why)
  );
  assert_eq!(
    session_told(anchor, attachment, token),
    ((rustix::process::getuid().as_raw(), 1000, 1001), true),
    "the restored stream's session is the saved guest's"
  );
}

/// A-316 (the coordinator's condition 4a, slates' half), A-378. Do: as the anchor, hand a daemon the hold channel;
/// attach a volume twice in the Shards form through the client; read each stream's resync end and what the daemon sent
/// the anchor; have the first stream's guest root name its run's owner, then its kernel INIT offering the extended
/// SETXATTR form; SAVE the first stream and RESTORE its tables as the second's first frame; write on the anchor's held
/// end; then detach the first attachment and let the anchor close its copy as told. Expect each attach to hand a resync
/// end whose first message is `Attached`, naming its attachment and a token, no descriptor with it, the two tokens
/// different; the anchor to be given each daemon end by a hold for its attachment, then a session of that token, the
/// identity view and the short form; the owner word to bring a session naming the swap (the root's owner shown as
/// 1000:1001); the INIT a session keeping the swap and adding the extended form; the RESTORE a session for the second
/// attachment, its own token with the saved guest's swap and form; the anchor's held end to be the very channel the
/// device reads; and the detach to release the hold, after which the device reads the channel's end. The holds wait in
/// the anchor's channel while another thread opens and closes sockets, and each held end arrives live (A-339: on macOS a
/// daemon that closed its copy at once left the hold carrying the end's only one).
#[test]
fn a_streams_resync_end_is_held_by_the_anchor_with_its_token_and_owners_and_released_at_its_end() {
  let (anchor, daemon_end) = held::channel().unwrap();
  // SAFETY: this binary holds this one test, which sets the variable before any daemon starts and never again; the
  // daemon reads it once, at its start, which follows.
  unsafe {
    std::env::set_var(
      held::ENV_HELD,
      held::hand_in_process(&held::Table {
        devices: daemon_end.into_raw_fd().to_string(),
        ..held::Table::default()
      })
      .unwrap(),
    );
  }
  let profile = common::machine_profile();
  let instance = format!("srv-shards-resync-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "shards-resync"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: "resync".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  // The holds wait in the anchor's channel while sockets churn (A-339), as a busy machine's would.
  let stop = AtomicBool::new(false);
  let (first, second) = std::thread::scope(|scope| {
    scope.spawn(|| churn(&stop));
    let _stop = StopOnDrop(&stop);
    let first = client.attach_shards(volume, Intent::Write, None).unwrap();
    let second = client.attach_shards(volume, Intent::Write, None).unwrap();
    // The test thread holds the anchor's receive off while the churn runs; no runtime serves it (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(HOLDS_IN_FLIGHT);
    (first, second)
  });
  let (first_token, held_end) = held_with_its_token(&anchor, &first);
  let (second_token, _) = held_with_its_token(&anchor, &second);
  assert_ne!(
    first_token, second_token,
    "each attachment has its own token"
  );

  let mut stream = UnixStream::from(first.stream);
  stream.set_read_timeout(Some(WAIT)).unwrap();
  owner_word_updates_the_session(
    &anchor,
    &mut stream,
    first.attachment.attachment,
    first_token,
  );
  init_keeps_the_extended_form_in_the_session(
    &anchor,
    &mut stream,
    first.attachment.attachment,
    first_token,
  );
  let mut second_stream = UnixStream::from(second.stream);
  second_stream.set_read_timeout(Some(WAIT)).unwrap();
  a_restore_tells_the_anchor_the_saved_guests_session(
    &anchor,
    &mut stream,
    &mut second_stream,
    (second.attachment.attachment, second_token),
  );

  let first_resync = first.resync.unwrap();
  let from_the_anchor = b"from the anchor's copy";
  slates_ipc::stream::send(held_end.as_fd(), from_the_anchor).unwrap();
  let mut through = [0u8; 22];
  let (read, _) = slates_ipc::stream::recv_message(first_resync.as_fd(), &mut through).unwrap();
  assert_eq!(
    &through[..read],
    from_the_anchor,
    "the anchor holds the very channel the device reads"
  );

  client.detach(first.attachment.attachment).unwrap();
  let Incoming::Release { attachment } = next_held(&anchor) else {
    panic!("the attachment's end did not release its hold");
  };
  assert_eq!(attachment, first.attachment.attachment);
  drop(held_end);
  let (read, _) = slates_ipc::stream::recv_message(first_resync.as_fd(), &mut through).unwrap();
  assert_eq!(
    read, 0,
    "the device reads its channel's end: no resync will come"
  );
  drop(stream);
  drop(second_stream);
  drop(second.resync);
  drop(client);
  daemon.stop();
}
