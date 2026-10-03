//! The anchor's hold on this daemon's FUSE devices (§4.6 "Linux": "restore from the anchor's held fd"; A-61;
//! AC-3.4). A FUSE mount's device is the daemon's own descriptor, so without the anchor it dies with the
//! process. With it, the daemon hands the anchor a duplicate of each device it mounts, over the channel the
//! anchor gave it ([`slates_anchor::devices`]), and the next daemon gets the devices back and serves their
//! mounts again (`crate::fuse::adopt_held`).
//!
//! The channel is adopted once at start ([`adopt`]) into a process-lifetime cell: every shard sends on it, and a
//! sequenced-packet socket keeps each send whole whichever thread makes it. The devices handed back are split
//! by the partition that owns their attachment (an attachment id carries its owner in its high bits), so each
//! shard receives its own at initialization, by move. A device whose attachment no partition owns is closed and
//! released at once.
//!
//! A daemon without a supervising anchor (a test harness, a standalone start) has no channel: its sends are
//! answered `None`, and its mounts end with it, as before A-61.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::OnceLock;

use slates_anchor::devices::{self, DeviceRefusal, ENV_DEVICES, Outgoing};
use slates_bridge_fuse::session::Session;

/// The channel to the anchor, adopted once at start; unset without a supervising anchor.
static CHANNEL: OnceLock<OwnedFd> = OnceLock::new();

/// A device the anchor handed back: its attachment, the device, and what its connection negotiated, when the
/// anchor had that (a session that does not decode is `None`, so its mount is ended rather than served on a
/// guess).
#[derive(Debug)]
pub(crate) struct HeldDevice {
  /// The attachment the device served.
  pub(crate) attachment: u64,
  /// The device.
  pub(crate) device: OwnedFd,
  /// What its connection negotiated.
  pub(crate) session: Option<Session>,
}

impl HeldDevice {
  /// Whether this device's mount can be served again: its session arrived and its kernel can resend the
  /// requests the dead daemon read and never answered (without that, those callers would wait forever).
  pub(crate) fn adoptable(&self) -> bool {
    self.session.is_some_and(|session| session.kernel_resends)
  }
}

/// Adopts what the anchor handed this daemon in [`ENV_DEVICES`]: the channel, and every held device split by
/// the partition that owns its attachment (`partitions` lists, indexed by partition). A value that does not
/// parse adopts nothing and is reported; the daemon then runs without the hold, its mounts ending with it.
pub(crate) fn adopt(partitions: usize) -> Vec<Vec<HeldDevice>> {
  let mut split: Vec<Vec<HeldDevice>> = (0..partitions).map(|_| Vec::new()).collect();
  let Ok(value) = std::env::var(ENV_DEVICES) else {
    return split;
  };
  let inherited = match devices::parse_env(&value) {
    Ok(inherited) => inherited,
    Err(refusal) => {
      eprintln!("slates-server: the anchor's device handoff was refused: {refusal}");
      return split;
    }
  };
  let channel = crate::fleet::inherited_descriptor(inherited.channel);
  if CHANNEL.set(channel).is_err() {
    eprintln!("slates-server: the anchor's device channel was adopted twice; the second is closed");
  }
  for handed in inherited.devices {
    let device = crate::fleet::inherited_descriptor(handed.fd);
    let session = handed
      .session
      .as_deref()
      .and_then(|bytes| Session::from_bytes(bytes).ok());
    let held = HeldDevice {
      attachment: handed.attachment,
      device,
      session,
    };
    let partition = usize::from(crate::verbs::owner_of_attachment(handed.attachment));
    match split.get_mut(partition) {
      Some(list) => list.push(held),
      None => {
        let _ = release(held.attachment);
        drop(held);
      }
    }
  }
  split
}

/// Sends `message` to the anchor: `None` without an anchor, else whether it was sent.
fn send(message: &Outgoing<'_>) -> Option<Result<(), DeviceRefusal>> {
  let channel = CHANNEL.get()?;
  Some(devices::send(channel.as_fd(), message))
}

/// Asks the anchor to hold `device` for `attachment`.
pub(crate) fn hold(attachment: u64, device: BorrowedFd<'_>) -> Option<Result<(), DeviceRefusal>> {
  send(&Outgoing::Hold { attachment, device })
}

/// Tells the anchor what `attachment`'s connection negotiated.
pub(crate) fn session(attachment: u64, session: Session) -> Option<Result<(), DeviceRefusal>> {
  let bytes = session.to_bytes();
  send(&Outgoing::Session {
    attachment,
    session: &bytes,
  })
}

/// Tells the anchor `attachment` ended, so it closes its device.
pub(crate) fn release(attachment: u64) -> Option<Result<(), DeviceRefusal>> {
  send(&Outgoing::Release { attachment })
}
