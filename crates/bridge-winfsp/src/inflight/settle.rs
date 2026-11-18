//! The receive worker's protocol (A-198), apart from its Win32 edges. One owner receives a batch into the journal,
//! answers each request in driver order, saves each response before the driver sees it, and acknowledges it before
//! taking the next; after a death, the batch the journal kept is settled before any new receive. The Windows receive
//! worker (`crate::dispatcher`) runs this code over the driver's transaction, the DLL's operations and the owner shard;
//! the crash oracle in this module's tests runs the same code over a modelled driver and shard, crashing it after every
//! store to held memory (audit step 7). WinFsp v2.1 sys/volume.c, sys/ioq.c and dll/fs.c define the order kept here.

use super::capture::{Batch, Capture, Receiving};
use super::{CLEANUP_KIND, CLOSE_KIND, CREATE_KIND, Refusal, Request, Scratch};

/// What the protocol asks of the world outside the journal: the driver, the DLL's operation table and the owner shard.
pub trait Edges {
  /// The refusal the edges and the journal both end the worker with.
  type Error: From<Refusal>;
  /// Receives the next batch into `buffer`, the journal's direct-output buffer, and answers the bytes the driver
  /// reported; `None` when the owner is stopping, which keeps a batch taken meanwhile unsettled for the successor.
  fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>, Self::Error>;
  /// Hands one complete, saved response to the driver.
  fn send(&mut self, response: &[u8]) -> Result<(), Self::Error>;
  /// Runs `request`'s operation, writing its response into `response`, and answers the response's length. `replaying`
  /// is set while recovery runs a dead daemon's captured Cleanup or Close again, so the shard can find its effect done.
  fn operate(
    &mut self,
    request: &Request<'_>,
    response: &mut [u8],
    replaying: bool,
  ) -> Result<usize, Self::Error>;
  /// Tells the shard the driver accepted the open `context` names, without waiting for it to apply that: the shard's
  /// FIFO channel applies it ahead of this owner's next request (audit step 7, cut (c)).
  fn post_confirm(&mut self, context: u64) -> Result<(), Self::Error>;
  /// Answers once the shard has applied the confirmation last posted, if one is outstanding; refuses one it refused.
  fn confirmed(&mut self) -> Result<(), Self::Error>;
  /// Finishes the durable name updates of a successful rename before its saved success reaches the driver.
  fn rename_open_names(&mut self, from: &str, to: &str) -> Result<(), Self::Error>;
}

/// What a batch's settling came to: the requests answered, and of those the replayed Cleanups and Closes the file
/// system refused (recovery only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settled {
  /// Requests answered.
  pub answered: u64,
  /// Replayed Cleanups and Closes the file system refused.
  pub refused: u64,
}

/// Settles the batch a dead owner left before this owner's first receive; `None` when the journal holds none.
pub fn recover<'a, E: Edges>(
  capture: Capture<'a>,
  scratch: &'a mut Scratch,
  edges: &mut E,
  response: &mut [u8],
) -> Result<Option<Settled>, E::Error> {
  match capture.resume(scratch)? {
    Some(mut batch) => settle(&mut batch, true, edges, response).map(Some),
    None => Ok(None),
  }
}

/// Receives one batch and settles it. Answers what the batch filled, for the next [`Capture::begin`], or `None` when the
/// owner is stopping and the batch is left for the successor.
pub fn receive<'a, E: Edges>(
  mut receiving: Receiving<'a>,
  scratch: &'a mut Scratch,
  edges: &mut E,
  response: &mut [u8],
) -> Result<Option<usize>, E::Error> {
  let Some(reported) = edges.receive(receiving.buffer_mut())? else {
    return Ok(None);
  };
  let filled = super::capture::filled(reported)?;
  let mut batch = receiving.received(reported, scratch)?;
  settle(&mut batch, false, edges, response)?;
  Ok(Some(filled))
}

/// The saved successful open a dead owner left, published or settled, read from a copy of its journal before the ledger
/// settles its one unconfirmed open (`crate::opens::Ledger::settle_pending`). Read whatever the phase: a settled response
/// outlives its batch until the next save, so an open whose confirmation the shard had not applied is kept.
pub fn saved_open(journal: &mut [u8]) -> Result<Option<(u64, u64)>, Refusal> {
  Capture::new(journal)?.saved_open()
}

/// Whether a response's status is a refusal: any status but success.
fn refused(response: &[u8]) -> bool {
  response
    .get(super::STATUS_AT..super::STATUS_AT.saturating_add(size_of::<u32>()))
    .is_some_and(|status| status.iter().any(|byte| *byte != 0))
}

fn settle<E: Edges>(
  batch: &mut Batch<'_>,
  recovered: bool,
  edges: &mut E,
  response: &mut [u8],
) -> Result<Settled, E::Error> {
  let mut settled = Settled::default();
  while let Some(request) = batch.next_request()? {
    if recovered {
      if replay(batch, &request, edges, response)? {
        settled.refused = settled.refused.saturating_add(1);
      }
    } else {
      let length = edges.operate(&request, response, false)?;
      let answer = response.get(..length).ok_or(Refusal::Length)?;
      // The settled response this save overwrites keeps the last posted open until the shard has applied its
      // confirmation; by now this request's own shard call has normally carried it there.
      edges.confirmed()?;
      batch.record_response(&request, answer)?;
      if let Some((from, to)) = request.successful_rename(answer)? {
        edges.rename_open_names(&from, &to)?;
      }
      edges.send(answer)?;
      if let Some((_, context)) = super::opened_context(answer)? {
        edges.post_confirm(context)?;
      }
    }
    batch.acknowledge(request.hint())?;
    settled.answered = settled.answered.saturating_add(1);
  }
  Ok(settled)
}

/// Answers one request a dead owner had taken. Only Create's saved result is needed to preserve a possibly issued
/// context, and a successful rename's to finish its names; read completion can reference an old process mapping
/// (sys/read.c), so every other buffer-bearing operation cancels without dereferencing it. A captured Cleanup or Close
/// runs again. Answers whether the file system refused a replayed Cleanup or Close.
fn replay<E: Edges>(
  batch: &mut Batch<'_>,
  request: &Request<'_>,
  edges: &mut E,
  response: &mut [u8],
) -> Result<bool, E::Error> {
  let saved = batch.response_for(request)?;
  if let Some(saved) = saved {
    if request.kind() == CREATE_KIND {
      edges.send(saved)?;
      return Ok(false);
    }
    if let Some((from, to)) = request.successful_rename(saved)? {
      edges.rename_open_names(&from, &to)?;
      edges.send(saved)?;
      return Ok(false);
    }
  }
  let unsaved = saved.is_none();
  if matches!(request.kind(), CLEANUP_KIND | CLOSE_KIND) {
    let length = edges.operate(request, response, true)?;
    let answer = response.get(..length).ok_or(Refusal::Length)?;
    if unsaved {
      batch.record_response(request, answer)?;
    }
    edges.send(answer)?;
    return Ok(refused(answer));
  }
  let cancelled = request.cancelled_response()?;
  if unsaved {
    batch.record_response(request, &cancelled)?;
  }
  edges.send(&cancelled)?;
  Ok(false)
}
