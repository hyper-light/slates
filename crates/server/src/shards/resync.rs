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
//! taken back and their guest's unsynced stores ingested before it serves anything (A-365).
//!
//! **Windows** (A-427) keeps the same token, owner view and form, and one thing more: the process that holds the
//! stream ([`Holder`]), since a successor there duplicates the fresh end into a process rather than sending it on the
//! channel (A-418's wire). The session's bytes there are [`handle_session_bytes`], and the channel's I/O is the child
//! module `resync_handle` of `crate::shards`. A successor's half on Windows is not here yet (GAPS).

#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};

#[cfg(unix)]
use slates_anchor::held::Outgoing;
use slates_bridge_core::OwnerView;
use slates_bridge_fuse::shards_resync::TOKEN_BYTES;
#[cfg(unix)]
use slates_bridge_fuse::shards_resync::{ResyncKind, ResyncMessage};

/// Format: the bytes the anchor keeps beside a stream's held resync end, its hold's session: the token; the owner view's
/// two owners as four little-endian `u32`s (the volume's uid and gid, the run's uid and gid; all zero for the identity,
/// which a swap of an owner with itself is too); and the connection's kept form, a little-endian `u64` of
/// [`FORM_SETXATTR_EXT`] (A-378). Within the anchor's cap (`slates_anchor::held::SESSION_CAP`).
pub(super) const SESSION_BYTES: usize = TOKEN_BYTES + 4 * size_of::<u32>() + size_of::<u64>();
// A session the anchor's message could not carry would refuse every hold: checked as the daemon is built.
#[cfg(unix)]
const _: () = assert!(SESSION_BYTES <= slates_anchor::held::SESSION_CAP);

/// The process that holds a Windows stream (A-427): its id, and its creation time (`GetProcessTimes`' 100-nanosecond
/// count), which together name one process for its whole life, since an id is given again only after its process has
/// ended. A successor duplicates a fresh stream end into it (A-418). The zero holder names none.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Holder {
  /// Its process id.
  pub(super) pid: u32,
  /// Its creation time.
  pub(super) started: u64,
}

/// Format: the bytes of a Windows stream's held session (A-427), the record of its anchor-held resync end: the session's
/// [`SESSION_BYTES`], then the holder's process id (`u32`) and creation time (`u64`), little-endian.
#[cfg(any(windows, test))]
pub(super) const HANDLE_SESSION_BYTES: usize = SESSION_BYTES + size_of::<u32>() + size_of::<u64>();
// A session the Windows hold's record could not carry would refuse every hold: checked as the daemon is built.
#[cfg(any(windows, test))]
const _: () = assert!(HANDLE_SESSION_BYTES <= slates_anchor::handles::RECORD_CAP);
/// Format: the session's form bit of a guest whose kernel sends the extended `fuse_setxattr_in` (A-378).
const FORM_SETXATTR_EXT: u64 = 1;

/// What a stream's held session keeps for its successor: the owner view its guest named and the form its guest's
/// kernel sends (A-281, A-378).
#[derive(Clone, Copy, Debug)]
pub(super) struct Kept {
  /// How the guest is shown the volume's owners.
  pub(super) owners: OwnerView,
  /// Whether its kernel sends the extended `fuse_setxattr_in`.
  pub(super) setxattr_ext: bool,
}

/// A stream's resync channel as its daemon keeps it.
#[cfg(unix)]
#[derive(Debug)]
pub(super) struct Resync {
  /// The attachment's resync token, which only the anchor's hold and the device know besides.
  pub(super) token: [u8; TOKEN_BYTES],
  /// The daemon's own copy of its end, kept exactly while the anchor holds one, so no message ever carries the end's
  /// only reference (see the module's notes): none once given up, or when the anchor never held it.
  pub(super) end: Option<OwnedFd>,
}

#[cfg(unix)]
impl Resync {
  /// Whether the anchor holds the daemon's end: only then can a successor resync the stream.
  pub(super) fn held(&self) -> bool {
    self.end.is_some()
  }
}

/// What [`establish`] made: the device's end, to hand out with the stream's, and the channel as the daemon keeps it.
#[cfg(unix)]
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
#[cfg(unix)]
pub(super) fn establish(attachment: u64, kept: Kept) -> Result<Established, Unmade> {
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
  let held = hold(attachment, &daemon, &session_bytes(&token, kept));
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
#[cfg(unix)]
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

/// Tells the anchor what `attachment`'s successor must keep (A-281, A-378): its new owner view, or the form its guest's
/// kernel took at its `INIT`, so a successor shows its guest what this daemon does and parses its requests as this one
/// does. A refused update gives the resync up ([`give_up`]): a successor must never show the guest another owner or
/// misread its requests. Whether the anchor still holds the end.
#[cfg(unix)]
pub(super) fn tell_session(attachment: u64, resync: &mut Resync, kept: Kept) -> bool {
  if !resync.held() {
    return false;
  }
  let session = session_bytes(&resync.token, kept);
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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

/// The session bytes of a stream's held end: its token, its owner view and its form (see [`SESSION_BYTES`]).
pub(super) fn session_bytes(token: &[u8; TOKEN_BYTES], kept: Kept) -> [u8; SESSION_BYTES] {
  let ((kept_uid, kept_gid), (run_uid, run_gid)) = kept.owners.swapped().unwrap_or_default();
  let words = [kept_uid, kept_gid, run_uid, run_gid].map(u32::to_le_bytes);
  let form = if kept.setxattr_ext {
    FORM_SETXATTR_EXT
  } else {
    0
  }
  .to_le_bytes();
  let mut bytes = [0u8; SESSION_BYTES];
  for (slot, byte) in bytes.iter_mut().zip(
    token
      .iter()
      .chain(words.iter().flatten())
      .chain(form.iter()),
  ) {
    *slot = *byte;
  }
  bytes
}

/// The token and what a held end's session bytes keep, or none for bytes of another length or a form bit this daemon
/// does not know.
pub(super) fn from_session(bytes: &[u8]) -> Option<([u8; TOKEN_BYTES], Kept)> {
  let whole = <&[u8; SESSION_BYTES]>::try_from(bytes).ok()?;
  let (token, rest) = whole.split_first_chunk::<TOKEN_BYTES>()?;
  let (owners, form) = rest.split_first_chunk::<{ size_of::<[u32; 4]>() }>()?;
  let (owners, _) = owners.as_chunks::<{ size_of::<u32>() }>();
  let [kept_uid, kept_gid, run_uid, run_gid] = <[[u8; 4]; 4]>::try_from(owners)
    .ok()?
    .map(u32::from_le_bytes);
  let owners = if [kept_uid, kept_gid, run_uid, run_gid] == [0; 4] {
    OwnerView::IDENTITY
  } else {
    OwnerView::swapping((kept_uid, kept_gid), (run_uid, run_gid))
  };
  let form = u64::from_le_bytes(<[u8; size_of::<u64>()]>::try_from(form).ok()?);
  if form & !FORM_SETXATTR_EXT != 0 {
    return None;
  }
  Some((
    *token,
    Kept {
      owners,
      setxattr_ext: form & FORM_SETXATTR_EXT != 0,
    },
  ))
}

/// The record of a Windows stream's held resync end (A-427): its session's bytes ([`session_bytes`]), then its holder's
/// (see [`HANDLE_SESSION_BYTES`]).
#[cfg(any(windows, test))]
pub(super) fn handle_session_bytes(
  token: &[u8; TOKEN_BYTES],
  kept: Kept,
  holder: Holder,
) -> [u8; HANDLE_SESSION_BYTES] {
  let session = session_bytes(token, kept);
  let (pid, started) = (holder.pid.to_le_bytes(), holder.started.to_le_bytes());
  let mut bytes = [0u8; HANDLE_SESSION_BYTES];
  for (slot, byte) in bytes
    .iter_mut()
    .zip(session.iter().chain(pid.iter()).chain(started.iter()))
  {
    *slot = *byte;
  }
  bytes
}

/// The token, what the session keeps and the holder a Windows stream's held record names, or none for bytes of another
/// length or a session [`from_session`] refuses. A successor reads it (A-433).
#[cfg(any(windows, test))]
pub(super) fn from_handle_session(bytes: &[u8]) -> Option<([u8; TOKEN_BYTES], Kept, Holder)> {
  let whole = <&[u8; HANDLE_SESSION_BYTES]>::try_from(bytes).ok()?;
  let (session, holder) = whole.split_first_chunk::<SESSION_BYTES>()?;
  let (token, kept) = from_session(session)?;
  let (pid, started) = holder.split_first_chunk::<{ size_of::<u32>() }>()?;
  let started = <[u8; size_of::<u64>()]>::try_from(started).ok()?;
  Some((
    token,
    kept,
    Holder {
      pid: u32::from_le_bytes(*pid),
      started: u64::from_le_bytes(started),
    },
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-316, A-378. Do: encode the session of a token with the identity view and with a swap of the host's owner
  /// (501:20) and the run's (1000:1001), each with and without the extended `SETXATTR` form, and read each back; then
  /// read sessions one byte short, one long and one naming a form this daemon does not know. Expect each token, view and
  /// form back as made (the identity as the identity, a swap showing and keeping each id as before), and the rest
  /// refused.
  #[test]
  fn a_held_ends_session_keeps_its_token_and_owner_view() {
    let token = [7u8; TOKEN_BYTES];
    let swap = OwnerView::swapping((501, 20), (1000, 1001));
    for owners in [OwnerView::IDENTITY, swap] {
      for setxattr_ext in [false, true] {
        let bytes = session_bytes(
          &token,
          Kept {
            owners,
            setxattr_ext,
          },
        );
        let (read_token, read) = from_session(&bytes).unwrap();
        assert_eq!(read_token, token);
        assert_eq!(read.owners.swapped(), owners.swapped());
        assert_eq!(
          read.setxattr_ext, setxattr_ext,
          "the kept form reads back (A-378)"
        );
      }
    }
    let kept = Kept {
      owners: swap,
      setxattr_ext: false,
    };
    let swapped = from_session(&session_bytes(&token, kept)).unwrap().1.owners;
    assert_eq!((swapped.shown_uid(501), swapped.kept_gid(1001)), (1000, 20));
    let bytes = session_bytes(&token, kept);
    assert!(from_session(&bytes[..SESSION_BYTES - 1]).is_none());
    let mut long = bytes.to_vec();
    long.push(0);
    assert!(from_session(&long).is_none());
    let mut unknown = bytes;
    unknown[SESSION_BYTES - 1] = 0x80;
    assert!(
      from_session(&unknown).is_none(),
      "a form bit this daemon does not know"
    );
  }

  /// A-427. Do: encode a Windows stream's session (a token, a swap of owners, the extended `SETXATTR` form, and the
  /// holder's id and creation time) and read it back; then read it one byte short, one byte long, with a form bit this
  /// daemon does not know, and the Unix session's bytes alone. Expect: the token, the view, the form and the holder back
  /// as made, the Unix session's bytes first; every other refused.
  #[test]
  fn a_windows_streams_session_keeps_its_holder() {
    let token = [9u8; TOKEN_BYTES];
    let kept = Kept {
      owners: OwnerView::swapping((501, 20), (1000, 1001)),
      setxattr_ext: true,
    };
    let holder = Holder {
      pid: 4321,
      started: 0x01DB_5A6C_0F00_1234,
    };
    let bytes = handle_session_bytes(&token, kept, holder);
    let (read_token, read, read_holder) = from_handle_session(&bytes).unwrap();
    assert_eq!((read_token, read_holder), (token, holder));
    assert_eq!(read.owners.swapped(), kept.owners.swapped());
    assert!(read.setxattr_ext, "the kept form reads back");
    assert_eq!(
      &bytes[..SESSION_BYTES],
      &session_bytes(&token, kept)[..],
      "the Unix session's bytes come first"
    );
    assert!(from_handle_session(&bytes[..HANDLE_SESSION_BYTES - 1]).is_none());
    let mut long = bytes.to_vec();
    long.push(0);
    assert!(from_handle_session(&long).is_none());
    assert!(
      from_handle_session(&session_bytes(&token, kept)).is_none(),
      "a Unix session names no holder"
    );
    let mut unknown = bytes;
    unknown[SESSION_BYTES - 1] = 0x80;
    assert!(
      from_handle_session(&unknown).is_none(),
      "a form bit this daemon does not know"
    );
  }
}
