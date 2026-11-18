//! A Shards stream's resync channel, the daemon's half (§4.6, the Shards form; A-316): made at the attach beside the
//! stream, so that a successor daemon can hand the device a fresh stream for the same attachment after this daemon dies
//! (`slates_bridge_fuse::shards_resync` holds the wire, the conditions Shards' coordinator set, and their evidence).
//!
//! **At the attach** ([`establish`]): a message pair (`slates_ipc::stream::message_pair`: sequenced packets on Linux,
//! a stream of fixed-length messages on macOS, A-339); the attachment's resync token, 128 bits from the daemon's secure
//! random, never its NFS mount capability; the `Attached` message written on the daemon's end before the device has its
//! own, so it is the first thing the device reads. The daemon's end then goes to the anchor as a held descriptor, keyed
//! by the attachment, with the token and the stream's owner view as its session bytes ([`session_bytes`]), before the
//! attachment's record commits, so a committed record never names a channel the anchor was not given. The anchor needs
//! nothing of its own for it: its hold is the one it keeps for a FUSE device (`slates_anchor::held`), and the next
//! daemon tells the two apart by the form of the attachment's record. A daemon with no anchor, or one whose hold is
//! refused, closes its end at once: the device reads `Attached` and then the channel's end, and knows its share ends
//! with this daemon (Shards' option (a)).
//!
//! **The daemon keeps its own copy of its end while the anchor holds one** ([`Resync::end`], A-339). XNU flushes a Unix
//! socket whose every reference rides a message in flight whenever its collector of such descriptors runs (`unp_gc`,
//! which any Unix socket's close in the system can start): its collector walks only descriptors that are themselves in
//! flight, so a socket waiting in the receive buffer of one never sent (the anchor's end of its channel) is never found
//! reachable. Flushed, the end would read its own end of file for good, and its owner could never again tell the device's
//! close from it. An end sent to the anchor and closed at once is all in flight until the anchor's receive, so the
//! daemon keeps its copy until the attachment's end or [`give_up`], and closes it then with the anchor's: the device
//! reads the channel's end only when both are gone. Linux's collector takes only unreachable cycles (measured not to
//! flush a handed end, A-161's record); the copy is kept there too, so the two platforms run one rule.
//!
//! **While the stream lives**: the owner view the guest's root names (A-281) is told to the anchor as the hold's new
//! session ([`tell_owners`]); a stream whose guest maps a file with DAX keeps its resync while the anchor holds each
//! object it hands the device and the shard's range table records the ranges answered (A-365, `crate::shards::dax_hold`),
//! since the guest's stores through a map live in an object only the device maps once this daemon is gone, and gives it
//! up ([`give_up`]) where either is refused; and the attachment's end releases the hold ([`give_up`] again), so the
//! device reads the channel's end. Each refusal on the way gives the resync up rather than leave a hold that would carry a wrong view: a share
//! that cannot be resynced ends at a daemon's death, as before.
//!
//! **After a death** ([`send_resync`]): recovery keeps a Shards attachment whose resync end the anchor held, and the
//! successor serves it on a fresh stream as the same attachment (its owner view from the hold's session, no `INIT`, no
//! restore), then hands the device the fresh stream's end over the held channel in a `Resync` naming the same token.
//! The successor keeps its own copy of that end until the device shows it holds its own (its first bytes on the
//! stream, or its end of the channel closing, which the stream's serve task watches for meanwhile): the `Resync` waits
//! in the device's channel until the device reads it, and a socket whose every copy rides a message is what macOS's
//! collector flushes (A-339). A request the dead daemon published with its effect and never answered is answered from
//! that reply when the device resends it (A-61's rule, `crate::shards`'s replay). Its DAX objects the anchor held are
//! taken back and their guest's unsynced stores ingested before it serves anything (A-365). Not here yet: Windows
//! (GAPS).

use std::os::fd::{AsFd, OwnedFd};

use slates_anchor::held::Outgoing;
use slates_bridge_core::OwnerView;
use slates_bridge_fuse::shards_resync::{ResyncKind, ResyncMessage, TOKEN_BYTES};

/// Format: the bytes the anchor keeps beside a stream's held resync end, its hold's session: the token, then the owner
/// view's two owners as four little-endian `u32`s (the volume's uid and gid, the run's uid and gid; all zero for the
/// identity, which a swap of an owner with itself is too). Exactly the anchor's cap (`slates_anchor::held::SESSION_CAP`).
pub(super) const SESSION_BYTES: usize = TOKEN_BYTES + 4 * size_of::<u32>();

/// A stream's resync channel as its daemon keeps it.
#[derive(Debug)]
pub(super) struct Resync {
  /// The attachment's resync token, which only the anchor's hold and the device know besides.
  pub(super) token: [u8; TOKEN_BYTES],
  /// The daemon's own copy of its end, kept exactly while the anchor holds one, so no message ever carries the end's
  /// only reference (see the module's notes): none once given up, or when the anchor never held it.
  pub(super) end: Option<OwnedFd>,
}

impl Resync {
  /// Whether the anchor holds the daemon's end: only then can a successor resync the stream.
  pub(super) fn held(&self) -> bool {
    self.end.is_some()
  }
}

/// What [`establish`] made: the device's end, to hand out with the stream's, and the channel as the daemon keeps it.
pub(super) struct Established {
  /// The device's end.
  pub(super) end: OwnedFd,
  /// The channel as the daemon keeps it.
  pub(super) resync: Resync,
}

/// Why no channel was made: the stream is served without one, ending at a daemon's death as before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unmade {
  /// The platform's secure random gave no token.
  NoToken,
  /// The OS refused the pair, or the `Attached` message.
  Refused,
}

/// Makes `attachment`'s resync channel and asks the anchor to hold the daemon's end with `owners` (see the module's
/// notes). Run before the attachment's record commits; [`give_up`] undoes the hold if the attach goes no further.
pub(super) fn establish(attachment: u64, owners: OwnerView) -> Result<Established, Unmade> {
  let mut token = [0u8; TOKEN_BYTES];
  slates_transport::handshake::secure_random(&mut token).map_err(|_| Unmade::NoToken)?;
  let (daemon, device) = slates_ipc::stream::message_pair().map_err(|_| Unmade::Refused)?;
  let attached = ResyncMessage {
    kind: ResyncKind::Attached,
    attachment,
    token,
  }
  .to_bytes();
  match slates_ipc::stream::send(daemon.as_fd(), &attached) {
    Ok(sent) if sent == attached.len() => {}
    _ => return Err(Unmade::Refused),
  }
  let held = hold(attachment, &daemon, &session_bytes(&token, owners));
  // Kept while the anchor holds a copy, closed at once when none does (see the module's notes).
  Ok(Established {
    end: device,
    resync: Resync {
      token,
      end: held.then_some(daemon),
    },
  })
}

/// Asks the anchor to hold `daemon`, `attachment`'s resync end, with `session`: whether it does. A hold the anchor took
/// whose session it refused is released again, so the anchor never keeps an end without its token.
fn hold(attachment: u64, daemon: &OwnedFd, session: &[u8; SESSION_BYTES]) -> bool {
  let held = crate::anchor_hold::send(&Outgoing::Hold {
    attachment,
    device: daemon.as_fd(),
  });
  if !matches!(held, Some(Ok(()))) {
    return false;
  }
  let told = crate::anchor_hold::send(&Outgoing::Session {
    attachment,
    session,
  });
  if matches!(told, Some(Ok(()))) {
    return true;
  }
  let _ = crate::anchor_hold::send(&Outgoing::Release { attachment });
  false
}

/// Tells the anchor `attachment`'s new owner view (A-281), so a successor shows its guest what this daemon does. A
/// refused update gives the resync up ([`give_up`]): a successor must never show the guest another owner. Whether the
/// anchor still holds the end.
pub(super) fn tell_owners(attachment: u64, resync: &mut Resync, owners: OwnerView) -> bool {
  if !resync.held() {
    return false;
  }
  let session = session_bytes(&resync.token, owners);
  let told = crate::anchor_hold::send(&Outgoing::Session {
    attachment,
    session: &session,
  });
  if !matches!(told, Some(Ok(()))) {
    // Whether the release that follows is refused or not, the anchor's end no longer carries this view, and a successor
    // releases an end it cannot adopt; the caller counts the stream given up.
    let _released = give_up(attachment, resync);
  }
  resync.held()
}

/// Sends the device, over `held` (the attachment's resync end the anchor held across the last daemon's death), a
/// `Resync` naming `attachment` and its `token`, with `device` (the device's end of the fresh stream the successor
/// serves the attachment on) riding it: whether it went whole. A device that has gone refuses it, and the attachment is
/// then ended (Shards' option (a)).
pub(super) fn send_resync(
  held: &OwnedFd,
  attachment: u64,
  token: [u8; TOKEN_BYTES],
  device: &OwnedFd,
) -> bool {
  let resync = ResyncMessage {
    kind: ResyncKind::Resync,
    attachment,
    token,
  }
  .to_bytes();
  matches!(
    slates_ipc::stream::send_with_descriptor(held.as_fd(), &resync, device.as_fd()),
    Ok(sent) if sent == resync.len()
  )
}

/// What [`give_up`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GivenUp {
  /// The anchor held no end for the attachment: nothing to release.
  Nothing,
  /// The anchor was told to close its end, which it does in order (it drains its channel before any restart).
  Released,
  /// The release was refused: the anchor keeps the end until the next daemon releases it as one no record of its kind
  /// claims, so the device reads the channel's end only then. The caller counts it.
  Refused,
}

/// Gives `attachment`'s resync up: the anchor closes its end and the daemon its own, so the device reads the channel's
/// end and knows its share ends with this daemon.
pub(super) fn give_up(attachment: u64, resync: &mut Resync) -> GivenUp {
  let Some(own) = resync.end.take() else {
    return GivenUp::Nothing;
  };
  let released = crate::anchor_hold::send(&Outgoing::Release { attachment });
  drop(own);
  match released {
    Some(Ok(())) => GivenUp::Released,
    _ => GivenUp::Refused,
  }
}

/// The session bytes of a stream's held end: its token and its owner view (see [`SESSION_BYTES`]).
pub(super) fn session_bytes(token: &[u8; TOKEN_BYTES], owners: OwnerView) -> [u8; SESSION_BYTES] {
  let ((kept_uid, kept_gid), (run_uid, run_gid)) = owners.swapped().unwrap_or_default();
  let words = [kept_uid, kept_gid, run_uid, run_gid].map(u32::to_le_bytes);
  let mut bytes = [0u8; SESSION_BYTES];
  for (slot, byte) in bytes
    .iter_mut()
    .zip(token.iter().chain(words.iter().flatten()))
  {
    *slot = *byte;
  }
  bytes
}

/// The token and owner view a held end's session bytes name, or none for bytes of another length.
pub(super) fn from_session(bytes: &[u8]) -> Option<([u8; TOKEN_BYTES], OwnerView)> {
  let whole = <&[u8; SESSION_BYTES]>::try_from(bytes).ok()?;
  let (token, owners) = whole.split_first_chunk::<TOKEN_BYTES>()?;
  let (owners, _) = owners.as_chunks::<{ size_of::<u32>() }>();
  let [kept_uid, kept_gid, run_uid, run_gid] = <[[u8; 4]; 4]>::try_from(owners)
    .ok()?
    .map(u32::from_le_bytes);
  let view = if [kept_uid, kept_gid, run_uid, run_gid] == [0; 4] {
    OwnerView::IDENTITY
  } else {
    OwnerView::swapping((kept_uid, kept_gid), (run_uid, run_gid))
  };
  Some((*token, view))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-316. Do: encode the session of a token with the identity view and with a swap of the host's owner (501:20) and
  /// the run's (1000:1001), and read each back; then read sessions one byte short and one long. Expect each token and
  /// view back as made (the identity as the identity, a swap showing and keeping each id as before), and the wrong
  /// lengths refused.
  #[test]
  fn a_held_ends_session_keeps_its_token_and_owner_view() {
    let token = [7u8; TOKEN_BYTES];
    let swap = OwnerView::swapping((501, 20), (1000, 1001));
    for view in [OwnerView::IDENTITY, swap] {
      let bytes = session_bytes(&token, view);
      let (read_token, read_view) = from_session(&bytes).unwrap();
      assert_eq!(read_token, token);
      assert_eq!(read_view.swapped(), view.swapped());
    }
    let swapped = from_session(&session_bytes(&token, swap)).unwrap().1;
    assert_eq!((swapped.shown_uid(501), swapped.kept_gid(1001)), (1000, 20));
    let bytes = session_bytes(&token, swap);
    assert!(from_session(&bytes[..SESSION_BYTES - 1]).is_none());
    let mut long = bytes.to_vec();
    long.push(0);
    assert!(from_session(&long).is_none());
  }
}
