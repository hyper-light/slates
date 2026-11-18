//! A Shards stream's resync channel on Windows, the attach's half (§4.6, the Shards form; A-427, for A-316's channel and
//! A-418's wire): the counterpart of `super::resync`'s Unix I/O, over the anchor's Windows hold (A-198, A-423) and the
//! handle route (A-175).
//!
//! **At the attach** ([`establish`]): a message-mode named pipe (`slates_ipc::stream::message_pair`, the kind the
//! anchor's own hold channel is), each message one read whole; the attachment's resync token, 128 bits from the daemon's
//! secure random, never its mount capability; and the `Attached` message written on the daemon's end before the device
//! has its own, so it is the first thing the device reads. The device's end goes to the attaching client beside the
//! stream's, by the handle route (`ShardsHanded`'s `resync`).
//!
//! **The hold, before anything is served** ([`hold`]). Windows' hold is an exchange: the anchor duplicates the end out
//! of this process and answers (`crate::anchor_hold`), where Unix sends the end one way. A transaction cannot wait on it,
//! so the record commits first and the stream's serve task asks the hold before it reads anything on the stream: no
//! request is answered under a hold that is not settled. A death between the two leaves a record no hold names, which
//! recovery removes as it removes every guest's, or a hold no record names, which the next daemon releases (A-198's
//! rule). Once the anchor answers, the daemon closes its own copy, so the device reads the channel's end exactly when
//! the anchor lets the end go. A hold refused (no anchor, the anchor's bound) closes the end at once: the device reads
//! `Attached` and then the end, and knows its share ends with this daemon (Shards' option (a)). A hold whose answer did
//! not come may still have been taken, so it is released.
//!
//! **The session** ([`tell`]): what a successor must keep, the token, the owner view the guest names, the form its
//! kernel takes at `INIT` (A-378), and the process holding the stream ([`Holder`]: the attaching client, then each
//! process its client names with `ShardsHolder`, A-181, never a DAX claimant), is the hold's record
//! ([`handle_session_bytes`]). Each change moves the session's version ([`changed`], [`name_holder`]); the serve task tells
//! the anchor (A-423's `Record`) before it sends a reply that rests on the change (an `INIT`'s, an owner word's, a
//! RESTORE's), and at once when a holder is named outside it, which wakes it ([`owed_or_wake_me`]). A refused update
//! gives the resync up: a successor must never show the guest another owner, misread its requests, or hand the fresh
//! end to a process that does not hold the stream.
//!
//! **Giving it up** ([`give_up`]): at the attachment's end; at its guest's first DAX map, since the guest's stores
//! through a section live where only the device maps them once the daemon is gone and the anchor holds no Windows DAX
//! section (A-365 is Unix's); at a refused update; and where the device's end was not handed to the client. The
//! anchor's release is an exchange too, so it runs as a task of the shard; a refused one is counted, and the next
//! daemon releases the end as one no record claims.

use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};

use slates_bridge_fuse::shards_resync::{
  RESYNC_HANDLE_MESSAGE_BYTES, RESYNC_MESSAGE_BYTES, ResyncKind, ResyncMessage, TOKEN_BYTES,
};
use slates_rt::futures;

use super::ShardsStream;
use super::resync::{Holder, Kept, Unmade, handle_session_bytes};
use crate::state::{self, ShardState};

/// Format: the bytes the channel's pipe buffers toward the device: the attach's message and one Windows `Resync`
/// (A-418), so a successor's write is taken whether or not the device has read `Attached` yet. A second successor's,
/// to a device that has read nothing at all, is refused, and that share ends (Shards' option (a)).
const CHANNEL_BYTES: usize = RESYNC_MESSAGE_BYTES + RESYNC_HANDLE_MESSAGE_BYTES;

/// Where a stream's hold of its resync end stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
  /// Not settled: the serve task asks it before it reads anything on the stream.
  Asking,
  /// The anchor holds the end, its record the session at the version last told.
  Held,
  /// Never held, or given up: no successor resyncs the stream.
  Off,
}

/// A Windows stream's resync channel as its daemon keeps it.
#[derive(Debug)]
pub(super) struct HandleResync {
  /// The attachment's resync token, which only the anchor's hold and the device know besides.
  token: [u8; TOKEN_BYTES],
  /// The daemon's end of the channel, kept until the anchor has answered its hold (it duplicates the end out of this
  /// process meanwhile), then closed: the anchor's copy is then the only one.
  end: Option<OwnedHandle>,
  /// Where the hold stands.
  hold: Hold,
  /// The process a successor hands a fresh stream to.
  holder: Holder,
  /// The session's version: one more at each change a successor must keep.
  version: u64,
  /// The version the anchor keeps.
  told: u64,
}

impl HandleResync {
  /// Whether the anchor must be told the session before the next reply goes out.
  fn owed(&self) -> bool {
    self.hold == Hold::Held && self.told < self.version
  }

  /// Notes a change of what a successor must keep, while the resync stands.
  fn changed(&mut self) {
    if self.hold != Hold::Off {
      self.version = self.version.saturating_add(1);
    }
  }
}

/// What [`establish`] made: the device's end, to hand out with the stream's, and the channel as the daemon keeps it.
pub(super) struct Established {
  /// The device's end.
  pub(super) device: OwnedHandle,
  /// The channel as the daemon keeps it, its hold to be asked.
  pub(super) resync: HandleResync,
}

/// Makes `attachment`'s resync channel and writes its `Attached` (see the module's notes); its hold is the serve task's
/// to ask ([`hold`]). Refused `NoToken` when the secure random gives none, `Refused` when the OS refuses the pipe or the
/// message.
pub(super) fn establish(attachment: u64) -> Result<Established, Unmade> {
  let mut token = [0u8; TOKEN_BYTES];
  slates_transport::handshake::secure_random(&mut token).map_err(|_| Unmade::NoToken)?;
  let (daemon, device) =
    slates_ipc::stream::message_pair(CHANNEL_BYTES).map_err(|_| Unmade::Refused)?;
  let attached = ResyncMessage {
    kind: ResyncKind::Attached,
    attachment,
    token,
  }
  .to_bytes();
  // The pipe buffers the message, so the write is taken without its reader.
  match slates_ipc::stream::write_waiting(daemon.as_handle(), &attached) {
    Ok(written) if written == attached.len() => {}
    _ => return Err(Unmade::Refused),
  }
  Ok(Established {
    device,
    resync: HandleResync {
      token,
      end: Some(daemon),
      hold: Hold::Asking,
      holder: Holder::default(),
      version: 0,
      told: 0,
    },
  })
}

/// The anchor's record for `stream`'s resync end: its session as it stands now.
fn record(stream: &ShardsStream, resync: &HandleResync) -> Vec<u8> {
  let kept = Kept {
    owners: stream.owners,
    setxattr_ext: stream.setxattr_ext,
  };
  handle_session_bytes(&resync.token, kept, resync.holder).to_vec()
}

/// Asks the anchor to hold `attachment`'s resync end with its session, once, before its serve task reads anything on
/// the stream, and settles what the answer says (see the module's notes). Nothing to ask, nothing done.
pub(super) async fn hold(attachment: u64) {
  let asked = state::with_state(|s| {
    let stream = s.shards_streams.get(&attachment)?;
    let resync = stream
      .resync
      .as_ref()
      .filter(|resync| resync.hold == Hold::Asking)?;
    let end = resync.end.as_ref()?;
    Some((
      slates_anchor::held_windows::handle_value(end.as_raw_handle()),
      record(stream, resync),
      resync.version,
    ))
  })
  .flatten();
  let Some((value, record, version)) = asked else {
    return;
  };
  let answered = crate::anchor_hold::hold_resync(attachment, value, record).await;
  let may_hold = match &answered {
    Ok(()) => true,
    Err(refusal) => refusal.may_hold(),
  };
  // Whatever the anchor may hold for a stream that is not resynced is let go; with no state left to settle, so is it.
  let release =
    state::with_state_counted(|s| settle_hold(s, attachment, may_hold, answered.is_ok(), version));
  if release.unwrap_or(may_hold) {
    released(attachment).await;
  }
}

/// Settles `attachment`'s hold by the anchor's answer (`held`, or not, when the anchor `may_hold` it all the same), the
/// session it holds being `version`'s: held, the daemon's own copy closed; refused, the end closed and the stream served
/// without a resync, counted. Whether a release is owed: the anchor holds, or may hold, an end no stream keeps
/// resyncing (its answer did not come; the stream ended or gave its resync up meanwhile).
fn settle_hold(
  s: &mut ShardState,
  attachment: u64,
  may_hold: bool,
  held: bool,
  version: u64,
) -> bool {
  let Some(resync) = s
    .shards_streams
    .get_mut(&attachment)
    .and_then(|stream| stream.resync.as_mut())
  else {
    return may_hold;
  };
  resync.end = None;
  if resync.hold != Hold::Asking {
    return may_hold;
  }
  if held {
    resync.hold = Hold::Held;
    resync.told = version;
    return false;
  }
  resync.hold = Hold::Off;
  s.count(super::RESYNC_UNHELD, 1);
  may_hold
}

/// Whether `stream`'s resync session must reach the anchor before its next reply goes out.
pub(super) fn owed(stream: &ShardsStream) -> bool {
  !stream.ending && stream.resync.as_ref().is_some_and(HandleResync::owed)
}

/// Whether `attachment`'s stream owes the anchor its session; if not, `waker` is kept in the stream, so a holder named
/// outside the serve task wakes it ([`name_holder`]).
pub(super) fn owed_or_wake_me(attachment: u64, waker: &std::task::Waker) -> bool {
  state::with_state(|s| {
    s.shards_streams.get_mut(&attachment).is_some_and(|stream| {
      if owed(stream) {
        return true;
      }
      stream.waker = Some(waker.clone());
      false
    })
  })
  .unwrap_or(false)
}

/// Notes a change of `stream`'s session a successor must keep: its owner view or its kernel's form.
pub(super) fn changed(stream: &mut ShardsStream) {
  if let Some(resync) = stream.resync.as_mut() {
    resync.changed();
  }
}

/// Names `pid` the process a successor hands the fresh stream of `attachment` to (A-418): the attaching client at its
/// hand-off, then each process its client names with `ShardsHolder`. Its creation time is read now, so a successor tells
/// it from a later process given the same id; a process whose time cannot be read is not named, counted, and the
/// session keeps the holder it had. The serve task is woken to tell the anchor.
pub(super) fn name_holder(s: &mut ShardState, attachment: u64, pid: u32) {
  let standing = s
    .shards_streams
    .get(&attachment)
    .and_then(|stream| stream.resync.as_ref())
    .is_some_and(|resync| resync.hold != Hold::Off);
  if !standing {
    return;
  }
  let Ok(started) = slates_ipc::handle_route::process_started(pid) else {
    s.count(super::RESYNC_HOLDER_UNNAMED, 1);
    return;
  };
  let Some(stream) = s.shards_streams.get_mut(&attachment) else {
    return;
  };
  if let Some(resync) = stream.resync.as_mut() {
    resync.holder = Holder { pid, started };
    resync.changed();
  }
  if let Some(waker) = stream.waker.take() {
    waker.wake();
  }
}

/// Tells the anchor `attachment`'s session as it stands, where it is owed (the serve task runs it before a reply that
/// rests on a change, and when woken for a holder named outside it). A refused update gives the resync up, counted.
pub(super) async fn tell(attachment: u64) {
  let owed = state::with_state(|s| {
    let stream = s.shards_streams.get(&attachment)?;
    let resync = stream.resync.as_ref().filter(|resync| resync.owed())?;
    Some((record(stream, resync), resync.version))
  })
  .flatten();
  let Some((record, version)) = owed else {
    return;
  };
  let told = crate::anchor_hold::record_resync(attachment, record).await;
  let release = state::with_state_counted(|s| settle_tell(s, attachment, told.is_ok(), version))
    .unwrap_or(false);
  if release {
    released(attachment).await;
  }
}

/// Settles an update of `attachment`'s session at `version`: told, the anchor keeps it; refused, the resync is given up,
/// counted. Whether a release is owed.
fn settle_tell(s: &mut ShardState, attachment: u64, told: bool, version: u64) -> bool {
  let Some(resync) = s
    .shards_streams
    .get_mut(&attachment)
    .and_then(|stream| stream.resync.as_mut())
    .filter(|resync| resync.hold == Hold::Held)
  else {
    return false;
  };
  if told {
    resync.told = resync.told.max(version);
    return false;
  }
  resync.hold = Hold::Off;
  s.count(super::RESYNC_GIVEN_UP, 1);
  true
}

/// Gives `stream`'s resync up (see the module's notes): an end the anchor holds is let go by a task of the shard, since
/// the anchor's release is an exchange; one not yet held is closed now, and a hold still in flight is released when it
/// settles. Whether there was a resync to give up, for the caller's count.
pub(super) fn give_up(s: &mut ShardState, stream: &mut ShardsStream) -> bool {
  let Some(resync) = stream.resync.as_mut() else {
    return false;
  };
  let was = std::mem::replace(&mut resync.hold, Hold::Off);
  resync.end = None;
  match was {
    Hold::Off => false,
    Hold::Asking => true,
    Hold::Held => {
      release_later(s, stream.attachment);
      true
    }
  }
}

/// Lets `attachment`'s held resync end go by a task of this shard; one the runtime refuses is counted, and the next daemon
/// releases the end as one no record claims.
fn release_later(s: &mut ShardState, attachment: u64) {
  if futures::spawn(released(attachment))
    .and_then(futures::detach)
    .is_err()
  {
    s.count(super::RESYNC_RELEASE_REFUSED, 1);
  }
}

/// Tells the anchor to let `attachment`'s resync end go, counting a refusal: the anchor then keeps the end until the next
/// daemon releases it as one no record claims, so the device reads the channel's end only then.
async fn released(attachment: u64) {
  if crate::anchor_hold::release_resync(attachment)
    .await
    .is_err()
  {
    let _ = state::with_state_counted(|s| s.count(super::RESYNC_RELEASE_REFUSED, 1));
  }
}
