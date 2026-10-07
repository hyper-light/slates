//! The daemon's end of the anchor's hold (A-61, A-113): the channel over which it hands the supervising anchor a
//! duplicate of each FUSE device it mounts and each NFS loopback connection it accepts, and what the anchor handed
//! back at this daemon's start. With it a daemon's death leaves its mounts and its kernel clients' connections open;
//! the next daemon takes them back (`crate::fuse::adopt_held`, `crate::nfs::adopt_held`).
//!
//! The channel is adopted once at start ([`adopt`]) into a process-lifetime cell: every shard sends on it, and the
//! channel keeps each send whole whichever thread makes it (`slates_anchor::held`). The descriptors handed back are
//! adopted here, once, before anything else in the process could claim their numbers, and every one is made
//! close-on-exec: the anchor cleared that flag to hand them across its spawn, and nothing this daemon starts may hold
//! a copy (an unaccounted copy of a connection would keep it open after its release).
//!
//! A daemon without a supervising anchor (a test harness, a standalone start) has no channel: its sends are answered
//! `None`, and its mounts and connections end with it, as before A-61 and A-113.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::OnceLock;

use slates_anchor::held::{self, ENV_CONNECTIONS, ENV_DEVICES, HoldRefusal, Outgoing};

/// The channel to the anchor, adopted once at start; unset without a supervising anchor.
static CHANNEL: OnceLock<OwnedFd> = OnceLock::new();

/// A device the anchor handed back: its attachment, the device, and the session bytes its connection negotiated.
#[derive(Debug)]
pub(crate) struct HandedDevice {
  /// The attachment the device served.
  pub(crate) attachment: u64,
  /// The device.
  pub(crate) device: OwnedFd,
  /// What its connection negotiated, when the anchor had that.
  pub(crate) session: Option<Vec<u8>>,
}

/// A connection the anchor handed back: its id and the socket.
#[derive(Debug)]
pub(crate) struct HandedConnection {
  /// The connection's id, kept for its life.
  pub(crate) connection: u64,
  /// The connected socket.
  pub(crate) socket: OwnedFd,
}

/// Everything the anchor handed this daemon at its start.
#[derive(Debug, Default)]
pub(crate) struct Handed {
  /// The held FUSE devices.
  pub(crate) devices: Vec<HandedDevice>,
  /// The held NFS connections.
  pub(crate) connections: Vec<HandedConnection>,
}

/// Adopts what the anchor handed this daemon in [`ENV_DEVICES`] and [`ENV_CONNECTIONS`]: the channel, every held
/// device and every held connection. A value that does not parse adopts nothing of its kind and is reported, and a
/// connection whose descriptor number the devices' variable also names is not adopted (its number has another owner);
/// the daemon then runs without what it could not account for. Called once, at start.
pub(crate) fn adopt() -> Handed {
  let mut handed = Handed::default();
  let Ok(value) = std::env::var(ENV_DEVICES) else {
    return handed;
  };
  let inherited = match held::parse_env(&value) {
    Ok(inherited) => inherited,
    Err(refusal) => {
      eprintln!("slates-server: the anchor's device handoff was refused: {refusal}");
      return handed;
    }
  };
  let mut taken: Vec<RawFd> = vec![inherited.channel];
  taken.extend(inherited.devices.iter().map(|device| device.fd));
  let channel = adopted(inherited.channel);
  if CHANNEL.set(channel).is_err() {
    eprintln!("slates-server: the anchor's hold channel was adopted twice; the second is closed");
  }
  handed.devices = inherited
    .devices
    .into_iter()
    .map(|device| HandedDevice {
      attachment: device.attachment,
      device: adopted(device.fd),
      session: device.session,
    })
    .collect();
  let connections = std::env::var(ENV_CONNECTIONS).unwrap_or_default();
  match held::parse_connections_env(&connections) {
    Ok(connections) => {
      for connection in connections {
        if taken.contains(&connection.fd) {
          eprintln!(
            "slates-server: held connection {:x} names a descriptor the device handoff owns; it is not adopted",
            connection.connection
          );
          continue;
        }
        handed.connections.push(HandedConnection {
          connection: connection.connection,
          socket: adopted(connection.fd),
        });
      }
    }
    Err(refusal) => {
      eprintln!("slates-server: the anchor's connection handoff was refused: {refusal}");
    }
  }
  handed
}

/// Takes ownership of an inherited descriptor and marks it close-on-exec (a failure to mark is reported; the
/// descriptor is still owned and closed on drop).
fn adopted(raw: RawFd) -> OwnedFd {
  let owned = crate::fleet::inherited_descriptor(raw);
  if let Err(errno) = rustix::io::fcntl_setfd(&owned, rustix::io::FdFlags::CLOEXEC) {
    eprintln!("slates-server: an inherited descriptor could not be made close-on-exec: {errno}");
  }
  owned
}

/// Sends `message` to the anchor: `None` without an anchor, else whether it was sent. The channel is blocking, so a
/// refusal means the anchor's end is gone, and with it every descriptor the anchor held: a refused release
/// (`Release`, `ReleaseConnection`) leaves nothing held, which is why its callers need not retry it.
pub(crate) fn send(message: &Outgoing<'_>) -> Option<Result<(), HoldRefusal>> {
  let channel = CHANNEL.get()?;
  Some(held::send(channel.as_fd(), message))
}

/// Asks the anchor to hold `socket` as `connection` (A-113).
pub(crate) fn hold_connection(
  connection: u64,
  socket: BorrowedFd<'_>,
) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::HoldConnection { connection, socket })
}

/// Tells the anchor `connection` ended, so it closes its copy (A-113).
pub(crate) fn release_connection(connection: u64) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::ReleaseConnection { connection })
}

/// Whether a supervising anchor holds for this daemon.
pub(crate) fn anchored() -> bool {
  CHANNEL.get().is_some()
}
