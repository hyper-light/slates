//! The anchor's hold on this daemon's FUSE devices (§4.6 "Linux": "restore from the anchor's held fd"; A-61;
//! AC-3.4). A FUSE mount's device is the daemon's own descriptor, so without the anchor it dies with the
//! process. With it, the daemon hands the anchor a duplicate of each device it mounts, over the channel the
//! anchor gave it ([`slates_anchor::held`]), and the next daemon gets the devices back and serves their
//! mounts again (`crate::fuse::adopt_held`).
//!
//! The channel and the handed devices are adopted once at start by [`crate::anchor_hold`], which the NFS connection
//! hold (A-113) shares; this module splits the devices by the partition that owns their attachment (an attachment id
//! carries its owner in its high bits), so each shard receives its own at initialization, by move. A device whose
//! attachment no partition owns is closed and released at once.
//!
//! A daemon without a supervising anchor (a test harness, a standalone start) has no channel: its sends are
//! answered `None`, and its mounts end with it, as before A-61.

use std::os::fd::{BorrowedFd, OwnedFd};

use slates_anchor::held::{HoldRefusal, Outgoing};
use slates_bridge_fuse::session::Session;

use crate::anchor_hold::{HandedDevice, send};

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

/// Splits the devices the anchor handed back ([`crate::anchor_hold::adopt`]) by the partition that owns their
/// attachment (`partitions` lists, indexed by partition). A session that does not decode is `None`, so its mount is
/// ended rather than served on a guess.
pub(crate) fn split(handed: Vec<HandedDevice>, partitions: usize) -> Vec<Vec<HeldDevice>> {
  let mut split: Vec<Vec<HeldDevice>> = (0..partitions).map(|_| Vec::new()).collect();
  for handed in handed {
    let session = handed
      .session
      .as_deref()
      .and_then(|bytes| Session::from_bytes(bytes).ok());
    let held = HeldDevice {
      attachment: handed.attachment,
      device: handed.device,
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

/// Asks the anchor to hold `device` for `attachment`.
pub(crate) fn hold(attachment: u64, device: BorrowedFd<'_>) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::Hold { attachment, device })
}

/// Tells the anchor what `attachment`'s connection negotiated.
pub(crate) fn session(attachment: u64, session: Session) -> Option<Result<(), HoldRefusal>> {
  let bytes = session.to_bytes();
  send(&Outgoing::Session {
    attachment,
    session: &bytes,
  })
}

/// Tells the anchor `attachment` ended, so it closes its device.
pub(crate) fn release(attachment: u64) -> Option<Result<(), HoldRefusal>> {
  send(&Outgoing::Release { attachment })
}
