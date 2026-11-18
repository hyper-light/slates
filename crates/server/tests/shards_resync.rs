//! A Shards stream's resync channel, its daemon's half (§4.6, the Shards form; A-316), with this test as the anchor.
//! The test makes the anchor's hold channel and hands the daemon its end as the anchor does, through
//! `SLATES_ANCHOR_DEVICES`, which a daemon adopts once per process into a process-wide cell (`crate::anchor_hold`):
//! hence a binary of its own with one test, so no other test's daemon adopts the channel. The client attaches through
//! the daemon's real Linux control socket, so both ends reach it the way they reach `slates shards`.

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
/// Format: a FUSE request's in-header (`struct fuse_in_header`) and `FUSE_SETXATTR`'s opcode.
const IN_HEADER_LEN: usize = 40;
const SETXATTR: u32 = 21;
/// Format: the bytes before a reply's errno in its out-header (its length word).
const OUT_LEN_BYTES: usize = 4;

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
  let mut bytes = [0u8; 2 * RESYNC_MESSAGE_BYTES];
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

/// The SETXATTR of Shards' owner word `value` on the share's root, from the guest's root (uid 0), as Shards' init sends it.
fn owner_word(value: &[u8]) -> Vec<u8> {
  let mut body = u32::try_from(value.len()).unwrap().to_le_bytes().to_vec();
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(b"trusted.shards.owner\0");
  body.extend_from_slice(value);
  let total = IN_HEADER_LEN + body.len();
  let mut request = vec![0u8; IN_HEADER_LEN];
  request[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  request[4..8].copy_from_slice(&SETXATTR.to_le_bytes());
  request[8..16].copy_from_slice(&7u64.to_le_bytes());
  request[16..24].copy_from_slice(&1u64.to_le_bytes());
  request.extend_from_slice(&body);
  request
}

/// Reads one reply frame from `stream` and returns its errno, as Linux names it.
fn reply_errno(stream: &mut UnixStream) -> i32 {
  let mut length = [0u8; 4];
  stream.read_exact(&mut length).unwrap();
  let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(length)).unwrap()];
  stream.read_exact(&mut reply).unwrap();
  -i32::from_le_bytes(reply[OUT_LEN_BYTES..OUT_LEN_BYTES + 4].try_into().unwrap())
}

/// One Shards attach as the anchor sees it: the device's resync end of `shards` reads `Attached` naming its attachment
/// and a token, with no descriptor; the anchor is given the daemon's end by a hold for that attachment, then a session
/// of the same token and the identity view. Returns the token and the anchor's held end.
fn held_with_its_token(anchor: &OwnedFd, shards: &ShardsStream) -> ([u8; TOKEN_BYTES], OwnedFd) {
  let attachment = shards.attachment.attachment;
  let resync = shards.resync.as_ref().expect("Linux hands a resync end");
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
  assert_eq!(
    session[TOKEN_BYTES..],
    [0u8; 16],
    "and the identity view, until the guest names its owner"
  );
  (attached.token, device)
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
  let Incoming::Session {
    attachment: told,
    session,
  } = next_held(anchor)
  else {
    panic!("the owner word told the anchor nothing");
  };
  assert_eq!(told, attachment);
  assert_eq!(session[..TOKEN_BYTES], token, "the token stays");
  let word = |at: usize| u32::from_le_bytes(session[at..at + 4].try_into().unwrap());
  assert_eq!(
    (
      word(TOKEN_BYTES),
      word(TOKEN_BYTES + 8),
      word(TOKEN_BYTES + 12)
    ),
    (rustix::process::getuid().as_raw(), 1000, 1001),
    "the swap: the root's owner shown as the run's 1000:1001"
  );
}

/// A-316 (the coordinator's condition 4a, slates' half). Do: as the anchor, hand a daemon the hold channel; attach a
/// volume twice in the Shards form through the client; read each stream's resync end and what the daemon sent the
/// anchor; have the first stream's guest root name its run's owner; write on the anchor's held end; then detach the
/// first attachment and let the anchor close its copy as told. Expect each attach to hand a resync end whose first
/// message is `Attached`, naming its attachment and a token, no descriptor with it, the two tokens different; the
/// anchor to be given each daemon end by a hold for its attachment, then a session of that token and the identity
/// view; the owner word to bring a session naming the swap (the root's owner shown as 1000:1001); the anchor's held end
/// to be the very channel the device reads; and the detach to release the hold, after which the device reads the
/// channel's end.
#[test]
fn a_streams_resync_end_is_held_by_the_anchor_with_its_token_and_owners_and_released_at_its_end() {
  let (anchor, daemon_end) = held::channel().unwrap();
  // SAFETY: this binary holds this one test, which sets the variable before any daemon starts and never again; the
  // daemon reads it once, at its start, which follows.
  unsafe {
    std::env::set_var(held::ENV_DEVICES, daemon_end.into_raw_fd().to_string());
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
  let first = client.attach_shards(volume, Intent::Write, None).unwrap();
  let second = client.attach_shards(volume, Intent::Write, None).unwrap();
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

  let first_resync = first.resync.unwrap();
  slates_ipc::stream::send(held_end.as_fd(), b"from the anchor's copy").unwrap();
  let mut through = [0u8; 64];
  let (read, _) = slates_ipc::stream::recv_message(first_resync.as_fd(), &mut through).unwrap();
  assert_eq!(
    &through[..read],
    b"from the anchor's copy",
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
  drop(second);
  drop(client);
  daemon.stop();
}
