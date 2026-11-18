//! A CLI attachment kept alive by its holder (§4.7, A-230, A-384; the coordinator's ruling of 2026-11-18: a lease is
//! renewed by whatever holds it). A [`Hold`] renews its attachment from a thread of its own, over a client of its own, at
//! the cadence each renewal's answer names (`ReplyBody::Renewed::renew_every_ns`: a third of the shorter of the term and
//! the daemon's liveness window), until the hold is dropped or the daemon refuses a renewal. While it lives, the
//! attachment never lapses for want of another verb naming it; once it is dropped, or its process ends however it ends,
//! the renewals stop and the attachment lapses at its holder's own pace, the term the daemon learned from these renewals
//! (`verbs::attachment_term`: the tail of their gaps and a liveness window, about four thirds of a window).
//!
//! Its own client, because a renewal must not wait behind the holder's other calls (a long read, a landing), and a
//! [`Client`] is one channel served in order. Its own thread, owned and joined on drop (CLAUDE.md §2 item 9), waiting on a
//! channel with a timeout between renewals, never sleeping (D-9): dropping the hold drops the channel's sender, which
//! ends the wait at once.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use slates_ipc::IpcError;

use crate::client::{Client, Deadlines};
use crate::error::ClientError;

/// A CLI attachment held alive (A-384): renewed by a thread of the hold's own until the hold is dropped, or until the
/// daemon refuses a renewal (the attachment lapsed, or was detached elsewhere), which [`Hold::lost`] then reports.
#[derive(Debug)]
pub struct Hold {
  attachment: u64,
  /// The term the first renewal named, nanoseconds.
  term_ns: u64,
  /// Dropped to end the renewer's wait (its receiver then reads `Disconnected`).
  stop: Option<std::sync::mpsc::Sender<()>>,
  /// The renewer, until it is joined; its result is the refusal that ended it, if one did.
  renewer: Option<std::thread::JoinHandle<Option<ClientError>>>,
  /// The renewer's result once joined.
  ended: Option<Option<ClientError>>,
}

impl Hold {
  /// Holds `attachment` from a client of the hold's own, connected to `instance` under `deadlines`: renewed once before
  /// this returns (so a refusal, an attachment already lapsed or not this principal's, comes back here), then at the
  /// cadence each renewal names, until the hold is dropped.
  pub fn new(instance: &str, deadlines: Deadlines, attachment: u64) -> Result<Hold, ClientError> {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let (first, first_renewal) =
      std::sync::mpsc::sync_channel::<Result<(u64, u64), ClientError>>(1);
    let instance = instance.to_owned();
    let renewer = std::thread::Builder::new()
      .name("slates-hold".to_owned())
      .spawn(move || {
        let mut client = match Client::connect(&instance, deadlines) {
          Ok(client) => client,
          Err(error) => {
            // The holder reads this at once; a holder gone already wants no answer.
            let _unread = first.send(Err(error));
            return None;
          }
        };
        let (term_ns, every_ns) = match client.renew(attachment) {
          Ok(renewed) => renewed,
          Err(error) => {
            let _unread = first.send(Err(error));
            return None;
          }
        };
        let _unread = first.send(Ok((term_ns, every_ns)));
        renew_until_stopped(&mut client, attachment, every_ns, &stopped)
      })
      .map_err(|error| {
        ClientError::Ipc(IpcError::OsRefused {
          call: "spawn the hold's renewer",
          code: error.raw_os_error(),
        })
      })?;
    let mut hold = Hold {
      attachment,
      term_ns: 0,
      stop: Some(stop),
      renewer: Some(renewer),
      ended: None,
    };
    match first_renewal.recv() {
      Ok(Ok((term_ns, _))) => {
        hold.term_ns = term_ns;
        Ok(hold)
      }
      Ok(Err(error)) => Err(error),
      // The renewer ended without a word: only a panic does that (the process aborts in a release build).
      Err(_) => Err(ClientError::UnexpectedReply { verb: "renew" }),
    }
  }

  /// The attachment held.
  pub fn attachment(&self) -> u64 {
    self.attachment
  }

  /// The term the hold's first renewal named, nanoseconds: how long after its last renewal the attachment lapses once
  /// the hold ends (the daemon refines it as the renewals' gaps are measured).
  pub fn term_ns(&self) -> u64 {
    self.term_ns
  }

  /// The refusal that ended the hold's renewals, once they ended on their own (the attachment lapsed, or was detached
  /// elsewhere); `None` while they go on. Never waits.
  pub fn lost(&mut self) -> Option<&ClientError> {
    if self.ended.is_none()
      && self
        .renewer
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
    {
      self.ended = Some(self.join());
    }
    self.ended.as_ref().and_then(Option::as_ref)
  }

  /// Waits until the hold's renewals end on their own, and returns the refusal that ended them: for a process whose only
  /// work is to hold (`slates attach --hold`). Never returns while the daemon renews the attachment; the process ending
  /// ends the hold.
  pub fn wait_lost(mut self) -> Option<ClientError> {
    if let Some(ended) = self.ended.take() {
      return ended;
    }
    self.join()
  }

  /// Joins the renewer: its result, or `None` when it was joined already or ended without one.
  fn join(&mut self) -> Option<ClientError> {
    self
      .renewer
      .take()
      .and_then(|renewer| renewer.join().ok())
      .flatten()
  }
}

impl Drop for Hold {
  /// Ends the renewals: the stop channel's sender dropped ends the renewer's wait at once, and the renewer is joined. The
  /// attachment then lapses at its holder's pace unless something else renews or detaches it.
  fn drop(&mut self) {
    self.stop.take();
    // A renewer that panicked has nothing to report here; the hold is ending either way.
    let _ended = self.join();
  }
}

/// The renewer's loop: waits `every_ns` on `stopped`, then renews, at the cadence each renewal names, until the hold is
/// dropped (`None`) or the daemon refuses a renewal (that refusal).
fn renew_until_stopped(
  client: &mut Client,
  attachment: u64,
  first_every_ns: u64,
  stopped: &Receiver<()>,
) -> Option<ClientError> {
  let mut every_ns = first_every_ns;
  loop {
    match stopped.recv_timeout(Duration::from_nanos(every_ns)) {
      Err(RecvTimeoutError::Timeout) => {}
      // A stop sent, or the hold dropped.
      Ok(()) | Err(RecvTimeoutError::Disconnected) => return None,
    }
    match client.renew(attachment) {
      Ok((_, next_every_ns)) => every_ns = next_every_ns,
      Err(error) => return Some(error),
    }
  }
}
