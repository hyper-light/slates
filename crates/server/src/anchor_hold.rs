//! The daemon's end of the anchor's hold (A-61, A-113, A-197, A-365): the channel over which it hands the supervising
//! anchor a duplicate of each FUSE device it mounts, each NFS loopback connection it accepts, each writable direct map's
//! copy it hands a client and each Shards stream's DAX object it hands a device, and what the anchor handed back at this
//! daemon's start. With it a daemon's death leaves its
//! mounts, its kernel clients' connections and its clients' writable copies alive; the next daemon takes them back
//! (`crate::fuse::adopt_held`, `crate::nfs::adopt_held`, `crate::direct_map::adopt_inherited`).
//!
//! The channel is adopted once at start ([`adopt`]) into a process-lifetime cell: every shard sends on it, and the
//! channel keeps each send whole whichever thread makes it (`slates_anchor::held`). The descriptors handed back are
//! adopted here, once, before anything else in the process could claim their numbers, and every one is made
//! close-on-exec: the anchor cleared that flag to hand them across its spawn, and nothing this daemon starts may hold
//! a copy (an unaccounted copy of a connection would keep it open after its release). What the anchor handed back comes
//! in one table, read from the sealed object [`ENV_HELD`] names, never from the environment itself, which a table at the
//! bounds would pass (A-462: Linux's `MAX_ARG_STRLEN`, macOS's `ARG_MAX`).
//!
//! A daemon without a supervising anchor (a test harness, a standalone start) has no channel: its sends are answered
//! `None`, and its mounts and connections end with it, as before A-61 and A-113.

use std::os::fd::{AsFd, OwnedFd, RawFd};
use std::sync::OnceLock;

use slates_anchor::held::{
  self, ENV_HELD, HoldRefusal, Inherited, InheritedConnection, InheritedCopy, Outgoing,
};

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

/// A copy the anchor handed back: a writable direct map's (A-197, Linux), its id the map id its client holds; or a Shards
/// stream's DAX object (A-365), its id its backing's handle. The object, and its record, the daemon's own bytes, which
/// say which (`crate::held_copy::CopyRecord`, `crate::shards::DaxRecord`).
#[derive(Debug)]
pub(crate) struct HandedCopy {
  /// The copy's id.
  pub(crate) copy: u64,
  /// The object.
  pub(crate) object: OwnedFd,
  /// Its record.
  pub(crate) record: Vec<u8>,
}

/// Everything the anchor handed this daemon at its start.
#[derive(Debug, Default)]
pub(crate) struct Handed {
  /// The held FUSE devices.
  pub(crate) devices: Vec<HandedDevice>,
  /// The held NFS connections.
  pub(crate) connections: Vec<HandedConnection>,
  /// The held copies: writable direct maps' (Linux) and Shards streams' DAX objects.
  pub(crate) copies: Vec<HandedCopy>,
}

/// The table's parts as the anchor handed them (A-462): the devices, read whole or the table refused, and the
/// connections and the copies, each refused alone.
struct HandedParts {
  /// The channel and every held device.
  devices: Inherited,
  /// Every held connection, or why the part was refused.
  connections: Result<Vec<InheritedConnection>, HoldRefusal>,
  /// Every held copy, or why the part was refused.
  copies: Result<Vec<InheritedCopy>, HoldRefusal>,
}

/// The parts of the table `pointer` names (A-462): its descriptor and length checked against what the anchor may hold
/// for this daemon (`config`'s bounds, as the anchor derives them) before a byte is read; the object taken as this
/// process's own and closed once read; its checksum and lines checked; and refused whole where a part names the table's
/// own descriptor again. Nothing a refused table names is taken.
fn handed_table(
  pointer: &str,
  config: &crate::config::DaemonConfig,
) -> Result<HandedParts, HoldRefusal> {
  let bounds = held::TableBounds {
    devices: config.held_device_bound(),
    connections: config.held_connection_bound(),
    copies: config.held_copy_bound(),
  };
  let (table_fd, len) = held::parse_pointer(pointer, bounds)?;
  let table = held::decode_table(&held::read_table(adopted(table_fd), len)?)?;
  let devices = held::parse_devices(&table.devices)?;
  let connections = held::parse_connections(&table.connections);
  let copies = held::parse_copies(&table.copies);
  held::check_table_owner(
    table_fd,
    &devices,
    connections.as_deref().unwrap_or_default(),
    copies.as_deref().unwrap_or_default(),
  )?;
  Ok(HandedParts {
    devices,
    connections,
    copies,
  })
}

/// Adopts what the anchor handed this daemon in the table [`ENV_HELD`] names: the channel, every held device, every held
/// connection and every held copy. A table that does not read adopts nothing and is reported; a part that does not
/// parse adopts nothing of its kind and is reported, and a connection or copy whose descriptor number another part also
/// names is not adopted (its number has another owner); the daemon then runs without what it could not account for.
/// Called once, at start.
pub(crate) fn adopt(config: &crate::config::DaemonConfig) -> Handed {
  let mut handed = Handed::default();
  let Ok(pointer) = std::env::var(ENV_HELD) else {
    return handed;
  };
  let HandedParts {
    devices: inherited,
    connections,
    copies,
  } = match handed_table(&pointer, config) {
    Ok(parts) => parts,
    Err(refusal) => {
      slates_machine::log_line!("slates-server: the anchor's handoff table was refused: {refusal}");
      return handed;
    }
  };
  let mut taken: Vec<RawFd> = vec![inherited.channel];
  taken.extend(inherited.devices.iter().map(|device| device.fd));
  let channel = adopted(inherited.channel);
  if CHANNEL.set(channel).is_err() {
    slates_machine::log_line!(
      "slates-server: the anchor's hold channel was adopted twice; the second is closed"
    );
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
  match connections {
    Ok(connections) => {
      for connection in connections {
        if taken.contains(&connection.fd) {
          slates_machine::log_line!(
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
      slates_machine::log_line!(
        "slates-server: the anchor's connection handoff was refused: {refusal}"
      );
    }
  }
  adopt_copies(&mut handed, copies, taken);
  handed
}

/// Adopts the copies the anchor handed back in the table's copies part (A-197, A-365), none of whose descriptor numbers
/// may be one the devices' or connections' parts named (`taken`).
fn adopt_copies(
  handed: &mut Handed,
  copies: Result<Vec<InheritedCopy>, HoldRefusal>,
  mut taken: Vec<RawFd>,
) {
  taken.extend(handed.connections.iter().map(|connection| {
    use std::os::fd::AsRawFd;
    connection.socket.as_raw_fd()
  }));
  match copies {
    Ok(copies) => {
      for copy in copies {
        if taken.contains(&copy.fd) {
          slates_machine::log_line!(
            "slates-server: held copy {:x} names a descriptor another handoff owns; it is not adopted",
            copy.copy
          );
          continue;
        }
        handed.copies.push(HandedCopy {
          copy: copy.copy,
          object: adopted(copy.fd),
          record: copy.record,
        });
      }
    }
    Err(refusal) => {
      slates_machine::log_line!("slates-server: the anchor's copy handoff was refused: {refusal}");
    }
  }
}

/// Takes ownership of an inherited descriptor and marks it close-on-exec (a failure to mark is reported; the
/// descriptor is still owned and closed on drop).
fn adopted(raw: RawFd) -> OwnedFd {
  let owned = crate::fleet::inherited_descriptor(raw);
  if let Err(errno) = rustix::io::fcntl_setfd(&owned, rustix::io::FdFlags::CLOEXEC) {
    slates_machine::log_line!(
      "slates-server: an inherited descriptor could not be made close-on-exec: {errno}"
    );
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

/// Asks the anchor to hold `stream`'s socket as `connection` (A-113).
pub(crate) fn hold_connection(
  connection: u64,
  stream: &slates_rt::tcp::TcpStream,
) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::HoldConnection {
    connection,
    socket: stream.as_fd(),
  })
}

/// Tells the anchor `connection` ended, so it closes its copy (A-113).
pub(crate) fn release_connection(connection: u64) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::ReleaseConnection { connection })
}

/// Asks the anchor to hold `object`, a writable direct map's copy (A-197) or a Shards stream's DAX object (A-365), as
/// `copy` with `record`.
pub(crate) fn hold_copy(
  copy: u64,
  record: &[u8],
  object: std::os::fd::BorrowedFd<'_>,
) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::HoldCopy {
    copy,
    record,
    object,
  })
}

/// Tells the anchor `copy` was let go, so it closes its copy of it (A-197, A-365).
pub(crate) fn release_copy(copy: u64) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::ReleaseCopy { copy })
}

/// Whether a supervising anchor holds for this daemon.
pub(crate) fn anchored() -> bool {
  CHANNEL.get().is_some()
}
