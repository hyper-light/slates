//! The devices the anchor holds across daemon restarts (§4.6 "Linux": "restore from the anchor's held fd";
//! A-61; AC-3.4). A FUSE mount's device is opened by the daemon at run time, so unlike the NFS listener the
//! anchor cannot bind it before the spawn: the daemon sends it. The anchor keeps one `SOCK_SEQPACKET`
//! socketpair for its life ([`channel`]) and hands each daemon its end across the spawn; nothing is named on
//! disk (R1). Over it a daemon sends three messages ([`Outgoing`]):
//!
//! - **Hold**: an attachment's device, duplicated into the anchor by `SCM_RIGHTS`, sent before the attachment
//!   record commits, so a committed record never names a device the anchor was not given;
//! - **Session**: what the connection negotiated, once its `INIT` is answered (opaque bytes here; their meaning
//!   is `slates_bridge_fuse::session`);
//! - **Release**: the attachment ended; the anchor closes its copy.
//!
//! The anchor applies them to [`HeldDevices`], bounded by the daemon's attachment bound with a typed refusal
//! past it, and hands what it holds to every daemon it spawns in [`ENV_DEVICES`] ([`HeldDevices::env_value`],
//! read back by [`parse_env`]), the descriptors inherited as the NFS listener's is. A sequenced-packet socket
//! keeps each message whole and in order, and the anchor drains it before any restart, so a release from the
//! dead daemon is never applied after its successor started.
//!
//! Every message is a fixed [`MESSAGE_BYTES`]-byte body, checked by kind, length and descriptor count before
//! anything is kept: the channel crosses a process boundary, so its bytes are external input.
//!
//! Linux only: FUSE is the Linux bridge (macOS mounts over NFS, whose listener the anchor binds itself), and the
//! channel uses Linux's close-on-exec socketpair and close-on-exec descriptor receipt.

use std::collections::BTreeMap;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use rustix::net::{
  AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
  SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};

/// Format: the environment variable carrying the device channel's daemon end and every held device to a
/// spawned daemon: `CHANNEL_FD` then, for each device, `;ATTACHMENT_HEX:FD:SESSION_HEX` (`-` for a device
/// whose session has not arrived). The descriptors are inherited across the spawn.
pub const ENV_DEVICES: &str = "SLATES_ANCHOR_DEVICES";

/// Format: the most session bytes a message carries. The FUSE session is two bytes
/// (`slates_bridge_fuse::session::SESSION_BYTES`); the channel carries them opaque, with room for that
/// encoding to grow without a channel change.
pub const SESSION_CAP: usize = 32;

/// Format: a message's kind, its first byte.
const KIND_HOLD: u8 = 1;
/// Format: see [`KIND_HOLD`].
const KIND_SESSION: u8 = 2;
/// Format: see [`KIND_HOLD`].
const KIND_RELEASE: u8 = 3;
/// Format: where the attachment id starts (after the kind byte), and its width.
const AT_ATTACHMENT: usize = 1;
/// Format: the attachment id's width.
const ATTACHMENT_BYTES: usize = size_of::<u64>();
/// Format: where the session's length byte sits, after the attachment id.
const AT_SESSION_LEN: usize = AT_ATTACHMENT + ATTACHMENT_BYTES;
/// Format: where the session's bytes start.
const AT_SESSION: usize = AT_SESSION_LEN + 1;
/// Format: every message's length: kind, attachment, session length, and [`SESSION_CAP`] session bytes
/// (zero past the length).
pub const MESSAGE_BYTES: usize = AT_SESSION + SESSION_CAP;
/// Format: the descriptors a hold carries, and the most any message may.
const HOLD_FDS: usize = 1;

/// Why a device message or handoff was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceRefusal {
  /// A message, or the environment's handoff, that is not one of the defined shapes.
  Malformed {
    /// What was wrong.
    reason: &'static str,
  },
  /// The anchor already holds its bound of devices; the new one is closed.
  Bound {
    /// The bound.
    bound: usize,
  },
  /// An operating-system call refused.
  Os {
    /// The call.
    call: &'static str,
    /// Its errno.
    code: i32,
  },
}

impl std::fmt::Display for DeviceRefusal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      DeviceRefusal::Malformed { reason } => write!(f, "malformed device message: {reason}"),
      DeviceRefusal::Bound { bound } => write!(f, "the anchor already holds its {bound} devices"),
      DeviceRefusal::Os { call, code } => write!(f, "{call} refused: errno {code}"),
    }
  }
}

fn os(call: &'static str) -> impl Fn(rustix::io::Errno) -> DeviceRefusal {
  move |errno| DeviceRefusal::Os {
    call,
    code: errno.raw_os_error(),
  }
}

fn malformed(reason: &'static str) -> DeviceRefusal {
  DeviceRefusal::Malformed { reason }
}

/// What a daemon sends the anchor.
#[derive(Debug, Clone, Copy)]
pub enum Outgoing<'a> {
  /// Hold `device` for `attachment`.
  Hold {
    /// The attachment.
    attachment: u64,
    /// The device; the anchor receives a duplicate.
    device: BorrowedFd<'a>,
  },
  /// `attachment`'s connection negotiated `session`.
  Session {
    /// The attachment.
    attachment: u64,
    /// The negotiated session, at most [`SESSION_CAP`] bytes.
    session: &'a [u8],
  },
  /// `attachment` ended; close its device.
  Release {
    /// The attachment.
    attachment: u64,
  },
}

/// What the anchor received.
#[derive(Debug)]
pub enum Incoming {
  /// Hold `device` for `attachment`.
  Hold {
    /// The attachment.
    attachment: u64,
    /// The anchor's copy of the device.
    device: OwnedFd,
  },
  /// `attachment`'s connection negotiated `session`.
  Session {
    /// The attachment.
    attachment: u64,
    /// The session's bytes.
    session: Vec<u8>,
  },
  /// `attachment` ended.
  Release {
    /// The attachment.
    attachment: u64,
  },
}

/// The device channel: the anchor's end and the daemon's end of one sequenced-packet socketpair, both
/// close-on-exec (the supervisor clears it on the daemon's end at each spawn).
pub fn channel() -> Result<(OwnedFd, OwnedFd), DeviceRefusal> {
  rustix::net::socketpair(
    AddressFamily::UNIX,
    SocketType::SEQPACKET,
    SocketFlags::CLOEXEC,
    None,
  )
  .map_err(os("socketpair"))
}

/// The body of `message`, and whether it carries a device.
fn encode(message: &Outgoing<'_>) -> Result<[u8; MESSAGE_BYTES], DeviceRefusal> {
  let mut body = [0u8; MESSAGE_BYTES];
  let (kind, attachment, session): (u8, u64, &[u8]) = match message {
    Outgoing::Hold { attachment, .. } => (KIND_HOLD, *attachment, &[]),
    Outgoing::Session {
      attachment,
      session,
    } => (KIND_SESSION, *attachment, session),
    Outgoing::Release { attachment } => (KIND_RELEASE, *attachment, &[]),
  };
  let length = u8::try_from(session.len())
    .ok()
    .filter(|length| usize::from(*length) <= SESSION_CAP)
    .ok_or(malformed("a session longer than the channel carries"))?;
  let mut fields = body.iter_mut();
  let mut put = |bytes: &[u8]| {
    // `bytes` drives the zip, so a field never consumes a slot past its own last byte.
    for (byte, slot) in bytes.iter().zip(fields.by_ref()) {
      *slot = *byte;
    }
  };
  put(&[kind]);
  put(&attachment.to_le_bytes());
  put(&[length]);
  put(session);
  Ok(body)
}

/// Sends `message` on the daemon's end `socket`. A hold carries its device by `SCM_RIGHTS`.
pub fn send(socket: BorrowedFd<'_>, message: &Outgoing<'_>) -> Result<(), DeviceRefusal> {
  let body = encode(message)?;
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HOLD_FDS))];
  let mut control = SendAncillaryBuffer::new(&mut space);
  let device;
  if let Outgoing::Hold { device: held, .. } = message {
    device = [*held];
    if !control.push(SendAncillaryMessage::ScmRights(&device)) {
      return Err(malformed("the device did not fit the control buffer"));
    }
  }
  let sent = rustix::net::sendmsg(
    socket,
    &[IoSlice::new(&body)],
    &mut control,
    SendFlags::empty(),
  )
  .map_err(os("sendmsg"))?;
  if sent == MESSAGE_BYTES {
    Ok(())
  } else {
    Err(malformed("a message was sent short"))
  }
}

/// Reads the attachment id and the session from a received body.
fn decode_fields(body: &[u8; MESSAGE_BYTES]) -> Result<(u8, u64, Vec<u8>), DeviceRefusal> {
  let kind = body.first().copied().ok_or(malformed("no kind"))?;
  let attachment = body
    .get(AT_ATTACHMENT..AT_SESSION_LEN)
    .and_then(|bytes| <[u8; ATTACHMENT_BYTES]>::try_from(bytes).ok())
    .map(u64::from_le_bytes)
    .ok_or(malformed("no attachment id"))?;
  let length = body
    .get(AT_SESSION_LEN)
    .copied()
    .map(usize::from)
    .filter(|length| *length <= SESSION_CAP)
    .ok_or(malformed("a session length past the cap"))?;
  let session = body
    .get(AT_SESSION..AT_SESSION.saturating_add(length))
    .ok_or(malformed("a session past the body"))?
    .to_vec();
  Ok((kind, attachment, session))
}

/// The message a received body and its descriptors make, or why they make none. Every descriptor that does
/// not become the hold's device is closed (dropped) here.
fn decode(body: &[u8; MESSAGE_BYTES], mut fds: Vec<OwnedFd>) -> Result<Incoming, DeviceRefusal> {
  let (kind, attachment, session) = decode_fields(body)?;
  match kind {
    KIND_HOLD if fds.len() == HOLD_FDS && session.is_empty() => {
      let device = fds.pop().ok_or(malformed("a hold without its device"))?;
      Ok(Incoming::Hold { attachment, device })
    }
    KIND_HOLD => Err(malformed(
      "a hold carries exactly one device and no session",
    )),
    KIND_SESSION if fds.is_empty() && !session.is_empty() => Ok(Incoming::Session {
      attachment,
      session,
    }),
    KIND_SESSION => Err(malformed("a session carries bytes and no device")),
    KIND_RELEASE if fds.is_empty() && session.is_empty() => Ok(Incoming::Release { attachment }),
    KIND_RELEASE => Err(malformed("a release carries nothing")),
    _ => Err(malformed("an unknown kind")),
  }
}

/// Receives one message from the anchor's end `socket` without waiting: `None` when none is queued.
pub fn receive(socket: BorrowedFd<'_>) -> Result<Option<Incoming>, DeviceRefusal> {
  let mut body = [0u8; MESSAGE_BYTES];
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HOLD_FDS))];
  let mut control = RecvAncillaryBuffer::new(&mut space);
  let received = match rustix::net::recvmsg(
    socket,
    &mut [IoSliceMut::new(&mut body)],
    &mut control,
    RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT,
  ) {
    Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Ok(None),
    Err(errno) => return Err(os("recvmsg")(errno)),
    Ok(received) => received,
  };
  let mut fds = Vec::new();
  for message in control.drain() {
    if let RecvAncillaryMessage::ScmRights(received_fds) = message {
      fds.extend(received_fds);
    }
  }
  if received.flags.contains(ReturnFlags::CTRUNC) {
    return Err(malformed(
      "the descriptors were cut short (the anchor's descriptor limit, or more than a hold carries)",
    ));
  }
  if received.flags.contains(ReturnFlags::TRUNC) || received.bytes != MESSAGE_BYTES {
    return Err(malformed("a message of the wrong length"));
  }
  decode(&body, fds).map(Some)
}

/// One held device.
#[derive(Debug)]
pub struct HeldDevice {
  /// The anchor's copy of the device.
  pub device: OwnedFd,
  /// What its connection negotiated, once the daemon sent it.
  pub session: Option<Vec<u8>>,
}

/// The devices the anchor holds, by attachment id, under a bound.
#[derive(Debug)]
pub struct HeldDevices {
  held: BTreeMap<u64, HeldDevice>,
  bound: usize,
}

impl HeldDevices {
  /// None held, at most `bound`.
  pub fn new(bound: usize) -> HeldDevices {
    HeldDevices {
      held: BTreeMap::new(),
      bound,
    }
  }

  /// Applies a received message. A hold past the bound is refused and its device closed; a second hold for one
  /// attachment replaces the first (the daemon re-established it); a session or release for an attachment not
  /// held changes nothing (its hold was refused, or the release already came).
  pub fn apply(&mut self, message: Incoming) -> Result<(), DeviceRefusal> {
    match message {
      Incoming::Hold { attachment, device } => {
        if !self.held.contains_key(&attachment) && self.held.len() >= self.bound {
          return Err(DeviceRefusal::Bound { bound: self.bound });
        }
        self.held.insert(
          attachment,
          HeldDevice {
            device,
            session: None,
          },
        );
      }
      Incoming::Session {
        attachment,
        session,
      } => {
        if let Some(held) = self.held.get_mut(&attachment) {
          held.session = Some(session);
        }
      }
      Incoming::Release { attachment } => {
        self.held.remove(&attachment);
      }
    }
    Ok(())
  }

  /// The bound.
  pub fn bound(&self) -> usize {
    self.bound
  }

  /// The devices held.
  pub fn len(&self) -> usize {
    self.held.len()
  }

  /// Whether none is held.
  pub fn is_empty(&self) -> bool {
    self.held.is_empty()
  }

  /// Each held device, by attachment id.
  pub fn iter(&self) -> impl Iterator<Item = (u64, &HeldDevice)> {
    self
      .held
      .iter()
      .map(|(attachment, held)| (*attachment, held))
  }

  /// The [`ENV_DEVICES`] value handing `channel` (the daemon's end) and every held device to a daemon.
  pub fn env_value(&self, channel: BorrowedFd<'_>) -> String {
    let mut value = channel.as_raw_fd().to_string();
    for (attachment, held) in &self.held {
      let session = held.session.as_deref().map_or_else(|| "-".to_owned(), hex);
      value.push_str(&format!(
        ";{attachment:x}:{}:{session}",
        held.device.as_fd().as_raw_fd()
      ));
    }
    value
  }
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Format: the radix of the environment's attachment ids and session bytes.
const HEX: u32 = 16;
/// Format: the hex digits of one byte.
const HEX_DIGITS_PER_BYTE: usize = 2;

fn unhex(text: &str) -> Option<Vec<u8>> {
  if !text.len().is_multiple_of(HEX_DIGITS_PER_BYTE)
    || text.len() / HEX_DIGITS_PER_BYTE > SESSION_CAP
  {
    return None;
  }
  text
    .as_bytes()
    .chunks(HEX_DIGITS_PER_BYTE)
    .map(|pair| {
      std::str::from_utf8(pair)
        .ok()
        .and_then(|pair| u8::from_str_radix(pair, HEX).ok())
    })
    .collect()
}

/// One device handed over in the environment: its attachment, its inherited descriptor number, and its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedDevice {
  /// The attachment.
  pub attachment: u64,
  /// The descriptor number inherited across the spawn.
  pub fd: RawFd,
  /// The session's bytes, if the anchor had them.
  pub session: Option<Vec<u8>>,
}

/// What [`ENV_DEVICES`] handed a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inherited {
  /// The channel's daemon end.
  pub channel: RawFd,
  /// The held devices.
  pub devices: Vec<InheritedDevice>,
}

/// Parses an [`ENV_DEVICES`] value. Every descriptor number must be non-negative and distinct, and every
/// attachment named once, so no number is adopted twice.
pub fn parse_env(value: &str) -> Result<Inherited, DeviceRefusal> {
  let mut parts = value.split(';');
  let channel = parts
    .next()
    .and_then(|channel| channel.parse::<RawFd>().ok())
    .filter(|channel| *channel >= 0)
    .ok_or(malformed("the channel's descriptor"))?;
  let mut numbers = std::collections::BTreeSet::from([channel]);
  let mut attachments = std::collections::BTreeSet::new();
  let mut devices = Vec::new();
  for part in parts {
    let mut fields = part.split(':');
    let (Some(attachment), Some(fd), Some(session), None) =
      (fields.next(), fields.next(), fields.next(), fields.next())
    else {
      return Err(malformed("a device entry is not ATTACHMENT:FD:SESSION"));
    };
    let attachment =
      u64::from_str_radix(attachment, HEX).map_err(|_| malformed("a device's attachment id"))?;
    let fd = fd
      .parse::<RawFd>()
      .ok()
      .filter(|fd| *fd >= 0)
      .ok_or(malformed("a device's descriptor"))?;
    let session = match session {
      "-" => None,
      bytes => Some(unhex(bytes).ok_or(malformed("a device's session"))?),
    };
    if !numbers.insert(fd) {
      return Err(malformed("a descriptor number handed over twice"));
    }
    if !attachments.insert(attachment) {
      return Err(malformed("an attachment handed over twice"));
    }
    devices.push(InheritedDevice {
      attachment,
      fd,
      session,
    });
  }
  Ok(Inherited { channel, devices })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn pipe_end() -> OwnedFd {
    let (reader, _writer) = rustix::pipe::pipe().unwrap();
    reader
  }

  /// A-61. Do: send a hold (a pipe's write end), a session and a release over a fresh channel; receive each.
  /// Expect: the same kinds and fields arrive in order, and the held descriptor is the same pipe — bytes
  /// written through the received copy are read from the sender's read end.
  #[test]
  fn a_hold_a_session_and_a_release_cross_the_channel_whole_and_in_order() {
    let (anchor, daemon) = channel().unwrap();
    let (reader, writer) = rustix::pipe::pipe().unwrap();
    send(
      daemon.as_fd(),
      &Outgoing::Hold {
        attachment: 7,
        device: writer.as_fd(),
      },
    )
    .unwrap();
    send(
      daemon.as_fd(),
      &Outgoing::Session {
        attachment: 7,
        session: &[1, 3],
      },
    )
    .unwrap();
    send(daemon.as_fd(), &Outgoing::Release { attachment: 7 }).unwrap();
    drop(writer);
    let first = receive(anchor.as_fd());
    let Ok(Some(Incoming::Hold {
      attachment: 7,
      device,
    })) = first
    else {
      panic!("a hold, got {first:?}");
    };
    rustix::io::write(&device, b"held").unwrap();
    let mut read = [0u8; 4];
    rustix::io::read(&reader, &mut read).unwrap();
    assert_eq!(
      &read, b"held",
      "the anchor holds the very device the daemon sent"
    );
    assert!(matches!(
      receive(anchor.as_fd()).unwrap(),
      Some(Incoming::Session { attachment: 7, ref session }) if session == &[1, 3]
    ));
    assert!(matches!(
      receive(anchor.as_fd()).unwrap(),
      Some(Incoming::Release { attachment: 7 })
    ));
    assert!(
      receive(anchor.as_fd()).unwrap().is_none(),
      "nothing more is queued"
    );
  }

  fn body(kind: u8, session_len: u8) -> [u8; MESSAGE_BYTES] {
    let mut body = [0u8; MESSAGE_BYTES];
    if let Some(slot) = body.first_mut() {
      *slot = kind;
    }
    if let Some(slot) = body.get_mut(AT_SESSION_LEN) {
      *slot = session_len;
    }
    body
  }

  fn refused(body: [u8; MESSAGE_BYTES], fds: Vec<OwnedFd>) -> bool {
    matches!(decode(&body, fds), Err(DeviceRefusal::Malformed { .. }))
  }

  /// A-61, hostile input. Do: decode each kind with the wrong descriptors or session. Expect: each refused by
  /// name, its descriptors closed, nothing kept.
  #[test]
  fn a_message_with_the_wrong_descriptors_or_session_is_refused() {
    let cases: [(u8, u8, usize, &str); 6] = [
      (KIND_HOLD, 0, 0, "a hold without its device"),
      (KIND_HOLD, 0, 2, "a hold with two"),
      (KIND_HOLD, 2, 1, "a hold with a session"),
      (KIND_SESSION, 0, 0, "an empty session"),
      (KIND_SESSION, 2, 1, "a session with a device"),
      (KIND_RELEASE, 0, 1, "a release with a device"),
    ];
    for (kind, session_len, fds, what) in cases {
      let fds = (0..fds).map(|_| pipe_end()).collect();
      assert!(refused(body(kind, session_len), fds), "{what}");
    }
  }

  /// A-61, hostile input. Do: decode unknown kinds and session lengths past the cap; encode a session past it.
  /// Expect: each refused by name.
  #[test]
  fn an_unknown_kind_or_an_oversized_session_is_refused() {
    let past_cap = u8::try_from(SESSION_CAP + 1).unwrap();
    for (kind, session_len) in [
      (0, 0),
      (0xFF, 0),
      (KIND_SESSION, past_cap),
      (KIND_SESSION, u8::MAX),
    ] {
      assert!(
        refused(body(kind, session_len), Vec::new()),
        "kind {kind}, length {session_len}"
      );
    }
    let oversized = Outgoing::Session {
      attachment: 1,
      session: &[0; SESSION_CAP + 1],
    };
    assert!(
      matches!(encode(&oversized), Err(DeviceRefusal::Malformed { .. })),
      "a session past the cap is never sent"
    );
  }

  /// A-61. Do: hold devices up to a bound of two, then a third; hold the first again; session and release one
  /// not held. Expect: the third refused `Bound` (its device closed), the re-hold replaces, the others change
  /// nothing.
  #[test]
  fn held_devices_keep_their_bound_and_ignore_what_they_do_not_hold() {
    let mut held = HeldDevices::new(2);
    held
      .apply(Incoming::Hold {
        attachment: 1,
        device: pipe_end(),
      })
      .unwrap();
    held
      .apply(Incoming::Hold {
        attachment: 2,
        device: pipe_end(),
      })
      .unwrap();
    assert_eq!(
      held.apply(Incoming::Hold {
        attachment: 3,
        device: pipe_end()
      }),
      Err(DeviceRefusal::Bound { bound: 2 })
    );
    held
      .apply(Incoming::Hold {
        attachment: 1,
        device: pipe_end(),
      })
      .unwrap();
    held
      .apply(Incoming::Session {
        attachment: 9,
        session: vec![1],
      })
      .unwrap();
    held.apply(Incoming::Release { attachment: 9 }).unwrap();
    assert_eq!(held.len(), 2);
    held.apply(Incoming::Release { attachment: 2 }).unwrap();
    assert_eq!(
      held
        .iter()
        .map(|(attachment, _)| attachment)
        .collect::<Vec<_>>(),
      [1]
    );
  }

  /// A-61. Do: write the environment value for two held devices (one with a session), parse it back. Expect:
  /// the channel, both attachments, their descriptor numbers and sessions.
  #[test]
  fn the_environment_handoff_round_trips() {
    let (_anchor, daemon) = channel().unwrap();
    let mut held = HeldDevices::new(4);
    let first = pipe_end();
    let second = pipe_end();
    let (first_fd, second_fd) = (first.as_raw_fd(), second.as_raw_fd());
    held
      .apply(Incoming::Hold {
        attachment: 0xAB,
        device: first,
      })
      .unwrap();
    held
      .apply(Incoming::Session {
        attachment: 0xAB,
        session: vec![1, 3],
      })
      .unwrap();
    held
      .apply(Incoming::Hold {
        attachment: 0xCD,
        device: second,
      })
      .unwrap();
    let parsed = parse_env(&held.env_value(daemon.as_fd())).unwrap();
    assert_eq!(parsed.channel, daemon.as_raw_fd());
    assert_eq!(
      parsed.devices,
      [
        InheritedDevice {
          attachment: 0xAB,
          fd: first_fd,
          session: Some(vec![1, 3])
        },
        InheritedDevice {
          attachment: 0xCD,
          fd: second_fd,
          session: None
        },
      ]
    );
  }

  /// A-61, hostile input. Do: parse malformed environment values. Expect: each refused, none adopted.
  #[test]
  fn malformed_environment_handoffs_are_refused() {
    for value in [
      "",
      "-1",
      "x",
      "3;",
      "3;1:4",
      "3;1:4:-:extra",
      "3;zz:4:-",
      "3;1:-4:-",
      "3;1:3:-",
      "3;1:4:-;2:4:-",
      "3;1:4:-;1:5:-",
      "3;1:4:abc",
      "3;1:4:zz",
    ] {
      assert!(
        matches!(parse_env(value), Err(DeviceRefusal::Malformed { .. })),
        "{value:?} must be refused"
      );
    }
    let long_session = "00".repeat(SESSION_CAP + 1);
    assert!(
      parse_env(&format!("3;1:4:{long_session}")).is_err(),
      "a session past the cap"
    );
  }
}
