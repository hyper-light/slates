//! The descriptors the anchor holds across daemon restarts: FUSE devices (§4.6 "Linux": "restore from the anchor's
//! held fd"; A-61; AC-3.4) and NFS loopback connections (§4.6, A-113). Neither can be bound by the anchor before the
//! spawn, as the NFS listener is: the daemon opens a device and accepts a connection at run time, so it sends them.
//! The anchor keeps one socketpair for its life ([`channel`]) and hands each daemon its end across the spawn; nothing
//! is named on disk (R1). Over it a daemon sends five messages ([`Outgoing`]):
//!
//! - **Hold**: an attachment's device, duplicated into the anchor by `SCM_RIGHTS`, sent before the attachment
//!   record commits, so a committed record never names a device the anchor was not given;
//! - **Session**: what the connection negotiated, once its `INIT` is answered (opaque bytes here; their meaning
//!   is `slates_bridge_fuse::session`);
//! - **Release**: the attachment ended; the anchor closes its copy;
//! - **Hold connection**: an accepted NFS connection, duplicated by `SCM_RIGHTS` before its first request is read, so
//!   the kernel client never sees it close when a daemon dies — it sees a slow reply, and the next daemon answers it;
//! - **Release connection**: the connection ended (the daemon shut it down first, since the anchor's copy would keep
//!   it open); the anchor closes its copy;
//! - **Hold copy**: a writable direct map's copy (A-186), duplicated by `SCM_RIGHTS` with its record (opaque here, at
//!   most [`SESSION_CAP`] bytes: what the next daemon needs to adopt it, `slates_server`'s to define), sent before the
//!   map is handed to its client, so a client never holds a copy the anchor was not given (A-197);
//! - **Release copy**: the map was let go; the anchor closes its copy.
//!
//! The anchor applies them to [`Held`], each kind under its own bound with a typed refusal past it, and hands what it
//! holds to every daemon it spawns ([`ENV_DEVICES`], [`ENV_CONNECTIONS`], [`ENV_COPIES`]; [`Held::env_value`],
//! [`Held::connections_env_value`], [`Held::copies_env_value`], read back by [`parse_env`], [`parse_connections_env`]
//! and [`parse_copies_env`]), each descriptor inherited as the NFS listener's is. The channel keeps each message whole and in order, and the anchor drains it
//! before any restart, so a release from the dead daemon is never applied after its successor started.
//!
//! Every message is a fixed [`MESSAGE_BYTES`]-byte body, checked by kind, length and descriptor count before
//! anything is kept: the channel crosses a process boundary, so its bytes are external input.
//!
//! The channel is a sequenced-packet socketpair on Linux and a datagram socketpair on macOS, which has no
//! sequenced-packet Unix sockets (`socketpair(AF_UNIX, SOCK_SEQPACKET)` is `EPROTONOSUPPORT`, checked 2026-10-06): a
//! connected Unix datagram pair is reliable and ordered, keeps each message whole, and carries `SCM_RIGHTS`, which is
//! everything the protocol uses. Linux sets close-on-exec at creation and at receipt; macOS sets it with `fcntl`
//! straight after, on the anchor's own thread, before it can spawn.

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

/// Format: the environment variable carrying every held NFS connection to a spawned daemon: `ID_HEX:FD` for each,
/// joined by `;` (empty when none is held). The descriptors are inherited across the spawn; the channel itself rides
/// in [`ENV_DEVICES`].
pub const ENV_CONNECTIONS: &str = "SLATES_ANCHOR_CONNECTIONS";

/// Format: the environment variable carrying every held writable copy to a spawned daemon (A-197): `ID_HEX:FD:RECORD_HEX`
/// for each, joined by `;` (empty when none is held). The descriptors are inherited across the spawn; the channel itself
/// rides in [`ENV_DEVICES`].
pub const ENV_COPIES: &str = "SLATES_ANCHOR_COPIES";

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
/// Format: see [`KIND_HOLD`]; the id field names the connection.
const KIND_HOLD_CONNECTION: u8 = 4;
/// Format: see [`KIND_HOLD_CONNECTION`].
const KIND_RELEASE_CONNECTION: u8 = 5;
/// Format: see [`KIND_HOLD`]; the id field names the copy, and the session field carries its record (A-197).
const KIND_HOLD_COPY: u8 = 6;
/// Format: see [`KIND_HOLD_COPY`].
const KIND_RELEASE_COPY: u8 = 7;
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
pub enum HoldRefusal {
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

impl std::fmt::Display for HoldRefusal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      HoldRefusal::Malformed { reason } => write!(f, "malformed device message: {reason}"),
      HoldRefusal::Bound { bound } => write!(f, "the anchor already holds its {bound} devices"),
      HoldRefusal::Os { call, code } => write!(f, "{call} refused: errno {code}"),
    }
  }
}

fn os(call: &'static str) -> impl Fn(rustix::io::Errno) -> HoldRefusal {
  move |errno| HoldRefusal::Os {
    call,
    code: errno.raw_os_error(),
  }
}

fn malformed(reason: &'static str) -> HoldRefusal {
  HoldRefusal::Malformed { reason }
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
  /// Hold `socket`, an accepted NFS connection, as `connection`.
  HoldConnection {
    /// The connection's id, unique across the anchor's life.
    connection: u64,
    /// The connection; the anchor receives a duplicate.
    socket: BorrowedFd<'a>,
  },
  /// `connection` ended; close the anchor's copy.
  ReleaseConnection {
    /// The connection's id.
    connection: u64,
  },
  /// Hold `object`, a writable direct map's copy (A-197), as `copy`, with `record`.
  HoldCopy {
    /// The copy's id, unique while it lives (the map's id).
    copy: u64,
    /// What the next daemon needs to adopt it, at most [`SESSION_CAP`] bytes and at least one.
    record: &'a [u8],
    /// The copy; the anchor receives a duplicate.
    object: BorrowedFd<'a>,
  },
  /// `copy` was let go; close the anchor's copy.
  ReleaseCopy {
    /// The copy's id.
    copy: u64,
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
  /// Hold `socket` as `connection`.
  HoldConnection {
    /// The connection's id.
    connection: u64,
    /// The anchor's copy of the connection.
    socket: OwnedFd,
  },
  /// `connection` ended.
  ReleaseConnection {
    /// The connection's id.
    connection: u64,
  },
  /// Hold `object` as `copy`, with `record`.
  HoldCopy {
    /// The copy's id.
    copy: u64,
    /// Its record's bytes.
    record: Vec<u8>,
    /// The anchor's copy of the object.
    object: OwnedFd,
  },
  /// `copy` was let go.
  ReleaseCopy {
    /// The copy's id.
    copy: u64,
  },
}

/// The hold channel: the anchor's end and the daemon's end of one socketpair, both close-on-exec (the supervisor
/// clears it on the daemon's end at each spawn). Linux: sequenced packets, close-on-exec at creation.
#[cfg(target_os = "linux")]
pub fn channel() -> Result<(OwnedFd, OwnedFd), HoldRefusal> {
  rustix::net::socketpair(
    AddressFamily::UNIX,
    SocketType::SEQPACKET,
    SocketFlags::CLOEXEC,
    None,
  )
  .map_err(os("socketpair"))
}

/// See the Linux arm. macOS: datagrams (no sequenced-packet Unix sockets there), close-on-exec set straight after.
#[cfg(not(target_os = "linux"))]
pub fn channel() -> Result<(OwnedFd, OwnedFd), HoldRefusal> {
  let (anchor_end, daemon_end) = rustix::net::socketpair(
    AddressFamily::UNIX,
    SocketType::DGRAM,
    SocketFlags::empty(),
    None,
  )
  .map_err(os("socketpair"))?;
  close_on_exec(&anchor_end)?;
  close_on_exec(&daemon_end)?;
  Ok((anchor_end, daemon_end))
}

/// Marks `fd` close-on-exec, where the platform cannot set it as the descriptor is made.
#[cfg(not(target_os = "linux"))]
fn close_on_exec(fd: &OwnedFd) -> Result<(), HoldRefusal> {
  rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).map_err(os("fcntl(F_SETFD)"))
}

/// The flags of a receipt: Linux marks received descriptors close-on-exec as they arrive.
#[cfg(target_os = "linux")]
const RECEIVE_FLAGS: RecvFlags = RecvFlags::CMSG_CLOEXEC.union(RecvFlags::DONTWAIT);
/// See the Linux arm; macOS marks them after ([`received_close_on_exec`]).
#[cfg(not(target_os = "linux"))]
const RECEIVE_FLAGS: RecvFlags = RecvFlags::DONTWAIT;

/// Marks received descriptors close-on-exec where the receipt could not; a descriptor that refuses is closed and the
/// message refused, so no copy the anchor does not account for reaches a daemon it spawns.
#[cfg(not(target_os = "linux"))]
fn received_close_on_exec(fds: &[OwnedFd]) -> Result<(), HoldRefusal> {
  fds.iter().try_for_each(close_on_exec)
}

/// See the macOS arm: Linux marked them at receipt.
#[cfg(target_os = "linux")]
fn received_close_on_exec(_fds: &[OwnedFd]) -> Result<(), HoldRefusal> {
  Ok(())
}

/// The body of `message`, and whether it carries a device.
fn encode(message: &Outgoing<'_>) -> Result<[u8; MESSAGE_BYTES], HoldRefusal> {
  let mut body = [0u8; MESSAGE_BYTES];
  let (kind, attachment, session): (u8, u64, &[u8]) = match message {
    Outgoing::Hold { attachment, .. } => (KIND_HOLD, *attachment, &[]),
    Outgoing::Session {
      attachment,
      session,
    } => (KIND_SESSION, *attachment, session),
    Outgoing::Release { attachment } => (KIND_RELEASE, *attachment, &[]),
    Outgoing::HoldConnection { connection, .. } => (KIND_HOLD_CONNECTION, *connection, &[]),
    Outgoing::ReleaseConnection { connection } => (KIND_RELEASE_CONNECTION, *connection, &[]),
    Outgoing::HoldCopy { copy, record, .. } => (KIND_HOLD_COPY, *copy, record),
    Outgoing::ReleaseCopy { copy } => (KIND_RELEASE_COPY, *copy, &[]),
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
pub fn send(socket: BorrowedFd<'_>, message: &Outgoing<'_>) -> Result<(), HoldRefusal> {
  let body = encode(message)?;
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HOLD_FDS))];
  let mut control = SendAncillaryBuffer::new(&mut space);
  let carried = match message {
    Outgoing::Hold { device, .. } => Some([*device]),
    Outgoing::HoldConnection { socket, .. } => Some([*socket]),
    Outgoing::HoldCopy { object, .. } => Some([*object]),
    _ => None,
  };
  if let Some(carried) = carried.as_ref()
    && !control.push(SendAncillaryMessage::ScmRights(carried))
  {
    return Err(malformed("the descriptor did not fit the control buffer"));
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
fn decode_fields(body: &[u8; MESSAGE_BYTES]) -> Result<(u8, u64, Vec<u8>), HoldRefusal> {
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
fn decode(body: &[u8; MESSAGE_BYTES], mut fds: Vec<OwnedFd>) -> Result<Incoming, HoldRefusal> {
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
    KIND_HOLD_CONNECTION if fds.len() == HOLD_FDS && session.is_empty() => {
      let socket = fds
        .pop()
        .ok_or(malformed("a connection hold without its socket"))?;
      Ok(Incoming::HoldConnection {
        connection: attachment,
        socket,
      })
    }
    KIND_HOLD_CONNECTION => Err(malformed(
      "a connection hold carries exactly one socket and no session",
    )),
    KIND_RELEASE_CONNECTION if fds.is_empty() && session.is_empty() => {
      Ok(Incoming::ReleaseConnection {
        connection: attachment,
      })
    }
    KIND_RELEASE_CONNECTION => Err(malformed("a connection release carries nothing")),
    KIND_HOLD_COPY if fds.len() == HOLD_FDS && !session.is_empty() => {
      let object = fds
        .pop()
        .ok_or(malformed("a copy hold without its object"))?;
      Ok(Incoming::HoldCopy {
        copy: attachment,
        record: session,
        object,
      })
    }
    KIND_HOLD_COPY => Err(malformed(
      "a copy hold carries exactly one object and its record",
    )),
    KIND_RELEASE_COPY if fds.is_empty() && session.is_empty() => {
      Ok(Incoming::ReleaseCopy { copy: attachment })
    }
    KIND_RELEASE_COPY => Err(malformed("a copy release carries nothing")),
    _ => Err(malformed("an unknown kind")),
  }
}

/// Receives one message from the anchor's end `socket` without waiting: `None` when none is queued.
pub fn receive(socket: BorrowedFd<'_>) -> Result<Option<Incoming>, HoldRefusal> {
  let mut body = [0u8; MESSAGE_BYTES];
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HOLD_FDS))];
  let mut control = RecvAncillaryBuffer::new(&mut space);
  let received = match rustix::net::recvmsg(
    socket,
    &mut [IoSliceMut::new(&mut body)],
    &mut control,
    RECEIVE_FLAGS,
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
  received_close_on_exec(&fds)?;
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

/// One held writable copy (A-197).
#[derive(Debug)]
pub struct HeldCopy {
  /// The anchor's copy of the object.
  pub object: OwnedFd,
  /// Its record, as the daemon sent it.
  pub record: Vec<u8>,
}

/// The descriptors the anchor holds: devices by attachment id, connections by connection id and writable copies by
/// copy id, each under its bound.
#[derive(Debug)]
pub struct Held {
  held: BTreeMap<u64, HeldDevice>,
  bound: usize,
  connections: BTreeMap<u64, OwnedFd>,
  connection_bound: usize,
  copies: BTreeMap<u64, HeldCopy>,
  copy_bound: usize,
}

impl Held {
  /// None held, at most `bound` devices, `connection_bound` connections and `copy_bound` copies.
  pub fn new(bound: usize, connection_bound: usize, copy_bound: usize) -> Held {
    Held {
      held: BTreeMap::new(),
      bound,
      connections: BTreeMap::new(),
      connection_bound,
      copies: BTreeMap::new(),
      copy_bound,
    }
  }

  /// Applies a received message. A hold past the bound is refused and its device closed; a second hold for one
  /// attachment replaces the first (the daemon re-established it); a session or release for an attachment not
  /// held changes nothing (its hold was refused, or the release already came).
  pub fn apply(&mut self, message: Incoming) -> Result<(), HoldRefusal> {
    match message {
      Incoming::Hold { attachment, device } => {
        if !self.held.contains_key(&attachment) && self.held.len() >= self.bound {
          return Err(HoldRefusal::Bound { bound: self.bound });
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
      Incoming::HoldConnection { connection, socket } => {
        if !self.connections.contains_key(&connection)
          && self.connections.len() >= self.connection_bound
        {
          return Err(HoldRefusal::Bound {
            bound: self.connection_bound,
          });
        }
        self.connections.insert(connection, socket);
      }
      Incoming::ReleaseConnection { connection } => {
        self.connections.remove(&connection);
      }
      Incoming::HoldCopy {
        copy,
        record,
        object,
      } => {
        if !self.copies.contains_key(&copy) && self.copies.len() >= self.copy_bound {
          return Err(HoldRefusal::Bound {
            bound: self.copy_bound,
          });
        }
        self.copies.insert(copy, HeldCopy { object, record });
      }
      Incoming::ReleaseCopy { copy } => {
        self.copies.remove(&copy);
      }
    }
    Ok(())
  }

  /// The copies held.
  pub fn copies(&self) -> usize {
    self.copies.len()
  }

  /// The copy bound.
  pub fn copy_bound(&self) -> usize {
    self.copy_bound
  }

  /// Each held copy, by id.
  pub fn iter_copies(&self) -> impl Iterator<Item = (u64, &HeldCopy)> {
    self.copies.iter().map(|(copy, held)| (*copy, held))
  }

  /// Forgets `copy` (its descriptor could not be handed over), closing the anchor's copy of it.
  pub fn drop_copy(&mut self, copy: u64) {
    self.copies.remove(&copy);
  }

  /// The [`ENV_COPIES`] value handing every held copy to a daemon.
  pub fn copies_env_value(&self) -> String {
    self
      .copies
      .iter()
      .map(|(copy, held)| {
        format!(
          "{copy:x}:{}:{}",
          held.object.as_fd().as_raw_fd(),
          hex(&held.record)
        )
      })
      .collect::<Vec<_>>()
      .join(";")
  }

  /// The connections held.
  pub fn connections(&self) -> usize {
    self.connections.len()
  }

  /// The connection bound.
  pub fn connection_bound(&self) -> usize {
    self.connection_bound
  }

  /// Each held connection, by id.
  pub fn iter_connections(&self) -> impl Iterator<Item = (u64, &OwnedFd)> {
    self
      .connections
      .iter()
      .map(|(connection, socket)| (*connection, socket))
  }

  /// Forgets `connection` (its descriptor could not be handed over), closing the anchor's copy.
  pub fn drop_connection(&mut self, connection: u64) {
    self.connections.remove(&connection);
  }

  /// The [`ENV_CONNECTIONS`] value handing every held connection to a daemon.
  pub fn connections_env_value(&self) -> String {
    self
      .connections
      .iter()
      .map(|(connection, socket)| format!("{connection:x}:{}", socket.as_fd().as_raw_fd()))
      .collect::<Vec<_>>()
      .join(";")
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
pub fn parse_env(value: &str) -> Result<Inherited, HoldRefusal> {
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

/// One connection handed over in the environment: its id and its inherited descriptor number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InheritedConnection {
  /// The connection's id.
  pub connection: u64,
  /// The descriptor number inherited across the spawn.
  pub fd: RawFd,
}

/// Reads an [`ENV_CONNECTIONS`] value back: each `ID_HEX:FD`. A value that does not parse, or names one id or one
/// descriptor twice, is refused whole, so the daemon adopts no descriptor number it cannot account for (two owners of
/// one number would close it twice).
pub fn parse_connections_env(value: &str) -> Result<Vec<InheritedConnection>, HoldRefusal> {
  let connections = parse_connection_entries(value)?;
  let ids: std::collections::BTreeSet<u64> = connections.iter().map(|c| c.connection).collect();
  let fds: std::collections::BTreeSet<RawFd> = connections.iter().map(|c| c.fd).collect();
  if ids.len() != connections.len() || fds.len() != connections.len() {
    return Err(malformed("a connection id or descriptor named twice"));
  }
  Ok(connections)
}

/// Each `ID_HEX:FD` entry of an [`ENV_CONNECTIONS`] value, parsed.
fn parse_connection_entries(value: &str) -> Result<Vec<InheritedConnection>, HoldRefusal> {
  value
    .split(';')
    .filter(|entry| !entry.is_empty())
    .map(|entry| {
      let (id, fd) = entry
        .split_once(':')
        .ok_or(malformed("a connection entry without its descriptor"))?;
      let connection =
        u64::from_str_radix(id, HEX).map_err(|_| malformed("a connection id that is not hex"))?;
      let fd = fd
        .parse::<RawFd>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or(malformed("a connection descriptor that is not a number"))?;
      Ok(InheritedConnection { connection, fd })
    })
    .collect()
}

/// One writable copy handed over in the environment: its id, its inherited descriptor number and its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedCopy {
  /// The copy's id.
  pub copy: u64,
  /// The descriptor number inherited across the spawn.
  pub fd: RawFd,
  /// Its record's bytes.
  pub record: Vec<u8>,
}

/// Reads an [`ENV_COPIES`] value back: each `ID_HEX:FD:RECORD_HEX`, the record at least one byte and at most
/// [`SESSION_CAP`]. A value that does not parse, or names one id or one descriptor twice, is refused whole, so the
/// daemon adopts no descriptor number it cannot account for.
pub fn parse_copies_env(value: &str) -> Result<Vec<InheritedCopy>, HoldRefusal> {
  let copies = value
    .split(';')
    .filter(|entry| !entry.is_empty())
    .map(|entry| {
      let mut fields = entry.split(':');
      let (Some(copy), Some(fd), Some(record), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
      else {
        return Err(malformed("a copy entry is not ID:FD:RECORD"));
      };
      let copy =
        u64::from_str_radix(copy, HEX).map_err(|_| malformed("a copy id that is not hex"))?;
      let fd = fd
        .parse::<RawFd>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or(malformed("a copy descriptor that is not a number"))?;
      let record = unhex(record)
        .filter(|record| !record.is_empty())
        .ok_or(malformed("a copy's record"))?;
      Ok(InheritedCopy { copy, fd, record })
    })
    .collect::<Result<Vec<_>, _>>()?;
  let ids: std::collections::BTreeSet<u64> = copies.iter().map(|c| c.copy).collect();
  let fds: std::collections::BTreeSet<RawFd> = copies.iter().map(|c| c.fd).collect();
  if ids.len() != copies.len() || fds.len() != copies.len() {
    return Err(malformed("a copy id or descriptor named twice"));
  }
  Ok(copies)
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
    matches!(decode(&body, fds), Err(HoldRefusal::Malformed { .. }))
  }

  /// A-61, hostile input. Do: decode each kind with the wrong descriptors or session. Expect: each refused by
  /// name, its descriptors closed, nothing kept.
  #[test]
  fn a_message_with_the_wrong_descriptors_or_session_is_refused() {
    let cases: [(u8, u8, usize, &str); 11] = [
      (KIND_HOLD, 0, 0, "a hold without its device"),
      (KIND_HOLD, 0, 2, "a hold with two"),
      (KIND_HOLD, 2, 1, "a hold with a session"),
      (KIND_SESSION, 0, 0, "an empty session"),
      (KIND_SESSION, 2, 1, "a session with a device"),
      (KIND_RELEASE, 0, 1, "a release with a device"),
      (
        KIND_HOLD_CONNECTION,
        0,
        0,
        "a connection hold without its socket",
      ),
      (KIND_HOLD_CONNECTION, 0, 2, "a connection hold with two"),
      (
        KIND_HOLD_CONNECTION,
        2,
        1,
        "a connection hold with a session",
      ),
      (
        KIND_RELEASE_CONNECTION,
        0,
        1,
        "a connection release with a socket",
      ),
      (
        KIND_RELEASE_CONNECTION,
        2,
        0,
        "a connection release with a session",
      ),
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
      matches!(encode(&oversized), Err(HoldRefusal::Malformed { .. })),
      "a session past the cap is never sent"
    );
  }

  /// A-61. Do: hold devices up to a bound of two, then a third; hold the first again; session and release one
  /// not held. Expect: the third refused `Bound` (its device closed), the re-hold replaces, the others change
  /// nothing.
  #[test]
  fn held_devices_keep_their_bound_and_ignore_what_they_do_not_hold() {
    let mut held = Held::new(2, 2, 2);
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
      Err(HoldRefusal::Bound { bound: 2 })
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
    let mut held = Held::new(4, 4, 4);
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
        matches!(parse_env(value), Err(HoldRefusal::Malformed { .. })),
        "{value:?} must be refused"
      );
    }
    let long_session = "00".repeat(SESSION_CAP + 1);
    assert!(
      parse_env(&format!("3;1:4:{long_session}")).is_err(),
      "a session past the cap"
    );
  }

  /// A-113. Do: send a connection hold (a pipe's write end standing for the socket) and its release over a fresh
  /// channel, between a device hold and its release. Expect: all four arrive in order, and the held connection is the
  /// very descriptor sent — bytes written through the anchor's copy are read from the daemon's read end.
  #[test]
  fn a_connection_hold_and_its_release_cross_the_channel_in_order() {
    let (anchor, daemon) = channel().unwrap();
    let (reader, writer) = rustix::pipe::pipe().unwrap();
    let device = pipe_end();
    send(
      daemon.as_fd(),
      &Outgoing::Hold {
        attachment: 1,
        device: device.as_fd(),
      },
    )
    .unwrap();
    send(
      daemon.as_fd(),
      &Outgoing::HoldConnection {
        connection: 0x51,
        socket: writer.as_fd(),
      },
    )
    .unwrap();
    send(
      daemon.as_fd(),
      &Outgoing::ReleaseConnection { connection: 0x51 },
    )
    .unwrap();
    send(daemon.as_fd(), &Outgoing::Release { attachment: 1 }).unwrap();
    drop(writer);
    assert!(matches!(
      receive(anchor.as_fd()).unwrap(),
      Some(Incoming::Hold { attachment: 1, .. })
    ));
    let held = receive(anchor.as_fd());
    let Ok(Some(Incoming::HoldConnection {
      connection: 0x51,
      socket,
    })) = held
    else {
      panic!("a connection hold, got {held:?}");
    };
    rustix::io::write(&socket, b"conn").unwrap();
    let mut read = [0u8; 4];
    rustix::io::read(&reader, &mut read).unwrap();
    assert_eq!(
      &read, b"conn",
      "the anchor holds the very connection the daemon sent"
    );
    assert!(
      rustix::io::fcntl_getfd(&socket)
        .unwrap()
        .contains(rustix::io::FdFlags::CLOEXEC),
      "a received copy is close-on-exec until the supervisor hands it over"
    );
    assert!(matches!(
      receive(anchor.as_fd()).unwrap(),
      Some(Incoming::ReleaseConnection { connection: 0x51 })
    ));
    assert!(matches!(
      receive(anchor.as_fd()).unwrap(),
      Some(Incoming::Release { attachment: 1 })
    ));
    assert!(
      receive(anchor.as_fd()).unwrap().is_none(),
      "nothing more is queued"
    );
  }

  /// A-113. Do: hold connections up to a bound of two while devices are held to theirs, then a third connection; hold
  /// the first again; release one never held, then the second. Expect: the third refused `Bound` without touching the
  /// devices, the re-hold replaces, the stray release changes nothing, and the release leaves only the first.
  #[test]
  fn held_connections_keep_their_own_bound() {
    let mut held = Held::new(1, 2, 2);
    held
      .apply(Incoming::Hold {
        attachment: 1,
        device: pipe_end(),
      })
      .unwrap();
    for connection in [10, 11] {
      held
        .apply(Incoming::HoldConnection {
          connection,
          socket: pipe_end(),
        })
        .unwrap();
    }
    assert_eq!(
      held.apply(Incoming::HoldConnection {
        connection: 12,
        socket: pipe_end()
      }),
      Err(HoldRefusal::Bound { bound: 2 })
    );
    held
      .apply(Incoming::HoldConnection {
        connection: 10,
        socket: pipe_end(),
      })
      .unwrap();
    held
      .apply(Incoming::ReleaseConnection { connection: 99 })
      .unwrap();
    held
      .apply(Incoming::ReleaseConnection { connection: 11 })
      .unwrap();
    assert_eq!(
      held
        .iter_connections()
        .map(|(id, _)| id)
        .collect::<Vec<_>>(),
      [10]
    );
    assert_eq!(held.len(), 1, "the devices are untouched");
  }

  /// A-113. Do: write the connections environment value for two held connections and parse it back; parse an empty
  /// one. Expect: both ids with their descriptor numbers, and nothing for the empty value.
  #[test]
  fn the_connections_handoff_round_trips() {
    let mut held = Held::new(1, 4, 4);
    let first = pipe_end();
    let second = pipe_end();
    let (first_fd, second_fd) = (first.as_raw_fd(), second.as_raw_fd());
    held
      .apply(Incoming::HoldConnection {
        connection: 0xA1,
        socket: first,
      })
      .unwrap();
    held
      .apply(Incoming::HoldConnection {
        connection: 0xB2,
        socket: second,
      })
      .unwrap();
    assert_eq!(
      parse_connections_env(&held.connections_env_value()).unwrap(),
      [
        InheritedConnection {
          connection: 0xA1,
          fd: first_fd
        },
        InheritedConnection {
          connection: 0xB2,
          fd: second_fd
        },
      ]
    );
    assert_eq!(parse_connections_env("").unwrap(), []);
  }

  /// A-197. Do: send a copy hold (a pipe's write end, with a record) and its release over a fresh channel; receive
  /// each; send a copy hold with no record and one with two descriptors' worth of nothing. Expect: the hold arrives with
  /// its id and record and the same pipe (bytes written through the received copy are read from the sender's read end),
  /// then the release; a hold with no record is refused as malformed by the anchor.
  #[test]
  fn a_copy_hold_and_its_release_cross_the_channel() {
    let (anchor, daemon) = channel().unwrap();
    let (reader, writer) = rustix::pipe::pipe().unwrap();
    let record = [7u8, 8, 9];
    send(
      daemon.as_fd(),
      &Outgoing::HoldCopy {
        copy: 0xC0FF,
        record: &record,
        object: writer.as_fd(),
      },
    )
    .unwrap();
    send(daemon.as_fd(), &Outgoing::ReleaseCopy { copy: 0xC0FF }).unwrap();
    let Some(Incoming::HoldCopy {
      copy,
      record: received,
      object,
    }) = receive(anchor.as_fd()).unwrap()
    else {
      panic!("the copy hold arrives first");
    };
    assert_eq!((copy, received.as_slice()), (0xC0FF, &record[..]));
    rustix::io::write(&object, b"x").unwrap();
    let mut byte = [0u8; 1];
    rustix::io::read(&reader, &mut byte).unwrap();
    assert_eq!(&byte, b"x", "the received copy is the same object");
    assert!(matches!(
      receive(anchor.as_fd()).unwrap(),
      Some(Incoming::ReleaseCopy { copy: 0xC0FF })
    ));
    send(
      daemon.as_fd(),
      &Outgoing::HoldCopy {
        copy: 1,
        record: &[],
        object: writer.as_fd(),
      },
    )
    .unwrap();
    assert!(
      matches!(receive(anchor.as_fd()), Err(HoldRefusal::Malformed { .. })),
      "a copy hold with no record is refused"
    );
  }

  /// A-197. Do: hold copies up to a bound of two while a device and a connection are held, then a third copy; hold the
  /// first again; release one never held, then the second. Expect: the third refused `Bound` without touching the
  /// others, the re-hold replaces, the stray release changes nothing, and only the first is left.
  #[test]
  fn held_copies_keep_their_own_bound() {
    let mut held = Held::new(1, 1, 2);
    held
      .apply(Incoming::Hold {
        attachment: 1,
        device: pipe_end(),
      })
      .unwrap();
    held
      .apply(Incoming::HoldConnection {
        connection: 5,
        socket: pipe_end(),
      })
      .unwrap();
    for copy in [20, 21] {
      held
        .apply(Incoming::HoldCopy {
          copy,
          record: vec![1],
          object: pipe_end(),
        })
        .unwrap();
    }
    assert_eq!(
      held.apply(Incoming::HoldCopy {
        copy: 22,
        record: vec![1],
        object: pipe_end()
      }),
      Err(HoldRefusal::Bound { bound: 2 })
    );
    held
      .apply(Incoming::HoldCopy {
        copy: 20,
        record: vec![2],
        object: pipe_end(),
      })
      .unwrap();
    held.apply(Incoming::ReleaseCopy { copy: 99 }).unwrap();
    held.apply(Incoming::ReleaseCopy { copy: 21 }).unwrap();
    assert_eq!(
      held
        .iter_copies()
        .map(|(id, copy)| (id, copy.record.clone()))
        .collect::<Vec<_>>(),
      [(20, vec![2])]
    );
    assert_eq!(
      (held.len(), held.connections()),
      (1, 1),
      "the others are untouched"
    );
  }

  /// A-197. Do: write the copies environment value for two held copies and parse it back; parse an empty one. Expect:
  /// both ids with their descriptor numbers and records, and nothing for the empty value.
  #[test]
  fn the_copies_handoff_round_trips() {
    let mut held = Held::new(1, 1, 4);
    let (first, second) = (pipe_end(), pipe_end());
    let (first_fd, second_fd) = (first.as_raw_fd(), second.as_raw_fd());
    held
      .apply(Incoming::HoldCopy {
        copy: 0xA1,
        record: vec![0, 255],
        object: first,
      })
      .unwrap();
    held
      .apply(Incoming::HoldCopy {
        copy: 0xB2,
        record: vec![9],
        object: second,
      })
      .unwrap();
    assert_eq!(
      parse_copies_env(&held.copies_env_value()).unwrap(),
      [
        InheritedCopy {
          copy: 0xA1,
          fd: first_fd,
          record: vec![0, 255]
        },
        InheritedCopy {
          copy: 0xB2,
          fd: second_fd,
          record: vec![9]
        },
      ]
    );
    assert_eq!(parse_copies_env("").unwrap(), []);
  }

  /// A-197, hostile input. Do: parse malformed copies environment values. Expect: each refused whole.
  #[test]
  fn malformed_copy_handoffs_are_refused() {
    let too_long = format!("1:4:{}", "ab".repeat(SESSION_CAP + 1));
    for value in [
      "x",
      "1:4",
      "1:4:",
      "zz:4:00",
      "1:-4:00",
      "1:four:00",
      "1:4:0",
      "1:4:zz",
      "1:4:00;1:5:00",
      "1:4:00;2:4:00",
      "1:4:00:00",
      too_long.as_str(),
    ] {
      assert!(
        matches!(parse_copies_env(value), Err(HoldRefusal::Malformed { .. })),
        "{value:?} must be refused"
      );
    }
  }

  /// A-113, hostile input. Do: parse malformed connections environment values. Expect: each refused whole.
  #[test]
  fn malformed_connection_handoffs_are_refused() {
    for value in [
      "x", "1", "1:", ":4", "zz:4", "1:-4", "1:four", "1:4;1:5", "1:4;2:4", "1:4:5",
    ] {
      assert!(
        matches!(
          parse_connections_env(value),
          Err(HoldRefusal::Malformed { .. })
        ),
        "{value:?} must be refused"
      );
    }
  }
}
