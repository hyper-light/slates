//! The NFSv4.1 back channel in the daemon (RFC 8881 §2.10.3.1, §20; B-1 of the delegation work): callbacks from the
//! server to a client over the connection the client bound to its session, on the shard that holds the session.
//!
//! RFC 8881 §2.10.3.1 lets a session's back channel ride the fore connection: the server sends RPC CALLs on the same
//! TCP stream it reads the client's calls from, and the client's REPLYs to them arrive interleaved with its calls.
//! So each connection the shard serves registers an outbox here; a session's compound on a connection binds that
//! connection as the session's carrier; a callback is one RPC CALL (`CB_COMPOUND`, AUTH_NONE) queued to the carrier's
//! outbox, whose owning task is woken to write it, and the caller waits for the REPLY with the same xid, routed here
//! by the connection's read path ([`deliver`]).
//!
//! Bounds: one callback in flight per session (the back channel's one slot, slot 0), so the pending table holds at
//! most one entry per carried session and an outbox at most one per session it carries. A second callback on a
//! session is refused [`CallbackError::Busy`], never queued without bound. A connection that ends fails its pending
//! callbacks [`CallbackError::Lost`] (its waiters woken), and a reply that does not come within the caller's deadline
//! is [`CallbackError::Timeout`]; every failure is typed, never silent.
//!
//! The table is shard-local state (no lock, no shared reference): a callback is always made on the shard that holds
//! the session, which is where its connection lives (A-76 moves them together).

use std::collections::{BTreeMap, VecDeque};
use std::task::{Poll, Waker};

use slates_bridge_nfs::v4::types::SessionId;
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};

use crate::state;

/// Format: the RPC message types (RFC 5531 §9): a call and a reply.
const RPC_CALL: u32 = 0;
/// Format: see [`RPC_CALL`].
const RPC_REPLY: u32 = 1;
/// Format: the RPC protocol version (RFC 5531 §9).
const RPC_VERSION: u32 = 2;
/// Format: the callback program's version (RFC 8881 §20: `NFS_CB` version 1).
const CB_VERSION: u32 = 1;
/// Format: `CB_COMPOUND`, procedure 1 of the callback program (RFC 8881 §20.1).
const CB_COMPOUND: u32 = 1;
/// Format: `AUTH_NONE` (RFC 5531 §8.1): the callback's credential and verifier.
const AUTH_NONE: u32 = 0;
/// Format: a reply's `MSG_ACCEPTED` and its `SUCCESS` (RFC 5531 §9).
const ACCEPTED: u32 = 0;

/// Why a callback did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallbackError {
  /// No connection on this shard carries the session's back channel.
  NoCarrier,
  /// The session already has a callback in flight (one slot).
  Busy,
  /// The carrying connection ended before the reply.
  Lost,
  /// No reply within the caller's deadline.
  Timeout,
  /// The reply was not an accepted RPC reply.
  Refused,
  /// Not on a shard thread, or the shard's state is out of reach.
  NoState,
}

/// One connection's outbox: the callback records waiting to be written, and the waker of the task that writes them.
#[derive(Default)]
struct Outbox {
  queue: VecDeque<Vec<u8>>,
  waker: Option<Waker>,
}

/// A callback awaiting its reply.
struct Pending {
  connection: u64,
  sessionid: SessionId,
  reply: Option<Result<Vec<u8>, CallbackError>>,
  waker: Option<Waker>,
}

/// The shard's back-channel table.
#[derive(Default)]
pub(crate) struct Callbacks {
  next_connection: u64,
  next_xid: u32,
  outboxes: BTreeMap<u64, Outbox>,
  carriers: BTreeMap<SessionId, u64>,
  pending: BTreeMap<u32, Pending>,
}

/// Registers a connection served on this shard; its id, for the calls below.
pub(crate) fn register() -> Option<u64> {
  state::with_state(|s| {
    let table = &mut s.callbacks;
    table.next_connection = table.next_connection.saturating_add(1);
    let id = table.next_connection;
    table.outboxes.insert(id, Outbox::default());
    id
  })
}

/// Ends a connection: its outbox and the sessions it carried go, and every callback waiting on it fails `Lost`.
pub(crate) fn unregister(connection: u64) {
  let _ = state::with_state(|s| {
    let table = &mut s.callbacks;
    table.outboxes.remove(&connection);
    table.carriers.retain(|_, carrier| *carrier != connection);
    for pending in table.pending.values_mut() {
      if pending.connection == connection && pending.reply.is_none() {
        pending.reply = Some(Err(CallbackError::Lost));
        if let Some(waker) = pending.waker.take() {
          waker.wake();
        }
      }
    }
  });
}

/// Binds `connection` as the carrier of `sessionid`'s back channel (the connection its compounds arrive on).
pub(crate) fn carry(sessionid: SessionId, connection: u64) {
  let _ = state::with_state(|s| {
    if s.callbacks.outboxes.contains_key(&connection) {
      s.callbacks.carriers.insert(sessionid, connection);
    }
  });
}

/// The callback records queued for `connection`, taken; when none is queued, `waker` is kept so a callback queued
/// later wakes the connection's task.
pub(crate) fn take_outbound(connection: u64, waker: &Waker) -> Vec<Vec<u8>> {
  state::with_state(|s| {
    let Some(outbox) = s.callbacks.outboxes.get_mut(&connection) else {
      return Vec::new();
    };
    if outbox.queue.is_empty() {
      outbox.waker = Some(waker.clone());
      return Vec::new();
    }
    outbox.queue.drain(..).collect()
  })
  .unwrap_or_default()
}

/// Whether a record body is an RPC reply (a callback's answer), not a call.
pub(crate) fn is_reply(body: &[u8]) -> bool {
  let mut reader = XdrReader::new(body);
  reader.u32().is_ok() && reader.u32().is_ok_and(|kind| kind == RPC_REPLY)
}

/// Routes a reply record to the callback with its xid: the results of an accepted, successful reply, or `Refused`.
/// A reply no callback awaits (late, after its deadline) is dropped and counted.
pub(crate) fn deliver(body: &[u8]) {
  let mut reader = XdrReader::new(body);
  let Ok(xid) = reader.u32() else {
    return;
  };
  let outcome = accepted_results(&mut reader).ok_or(CallbackError::Refused);
  let _ = state::with_state(|s| match s.callbacks.pending.get_mut(&xid) {
    Some(pending) if pending.reply.is_none() => {
      pending.reply = Some(outcome);
      if let Some(waker) = pending.waker.take() {
        waker.wake();
      }
    }
    _ => *s.refusals.entry(CALLBACK_REPLY_UNMATCHED).or_insert(0) += 1,
  });
}

/// The results after an accepted, successful reply header (the xid already read).
fn accepted_results(reader: &mut XdrReader<'_>) -> Option<Vec<u8>> {
  if reader.u32().ok()? != RPC_REPLY || reader.u32().ok()? != ACCEPTED {
    return None;
  }
  reader.u32().ok()?; // verifier flavor
  reader.opaque(MAX_VERIFIER).ok()?;
  if reader.u32().ok()? != ACCEPTED {
    return None;
  }
  Some(reader.rest().to_vec())
}

/// Format: the longest RPC verifier body (RFC 5531 §8.2: 400 bytes).
const MAX_VERIFIER: usize = 400;

/// Counter: callback replies no callback awaited (arrived after their deadline, or never sent).
const CALLBACK_REPLY_UNMATCHED: &str = "nfs4.callback.unmatched";

/// Format: `NFS4ERR_DELAY` (RFC 8881 §15.1.1.3), a `CB_COMPOUND` status that asks the server to call again later.
const NFS4ERR_DELAY: u32 = 10008;

/// Derived: how long a callback answered `NFS4ERR_DELAY` waits before it is sent again: the daemon's poll interval
/// (`HEARTBEAT_NS / POLL_PER_PERIOD`, 10 ms at the default cadence). The Linux client answers `CB_SEQUENCE` with it
/// while it is still setting the session up (measured: two of four Docker runs, 2026-10-04), which passes within a
/// round trip; Linux nfsd's own wait (`rpc_delay(task, 2 * HZ)`, two seconds) would leave the back channel unproven
/// past the caller's deadline.
const DELAY_RETRY_NS: u64 = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;

/// Counter: callbacks the client answered `NFS4ERR_DELAY`, sent again (the retry path's non-vacuity count).
const CALLBACK_DELAYED: &str = "nfs4.callback.delayed";

/// Sends `CB_COMPOUND` arguments `args` on `sessionid`'s back channel to the client's callback `program`, and waits
/// up to `deadline_ns` for the reply: its results (the `CB_COMPOUND4res`). A reply of `NFS4ERR_DELAY` is not an
/// answer: the same arguments are sent again after [`DELAY_RETRY_NS`] while the deadline allows — the same slot
/// sequence, since a failed `CB_SEQUENCE` leaves the client's slot unchanged (§20.9.3) — and the last reply is
/// returned when it does not.
pub(crate) async fn call(
  sessionid: SessionId,
  (program, credential): (u32, &[u8]),
  args: &[u8],
  deadline_ns: u64,
) -> Result<Vec<u8>, CallbackError> {
  let began = slates_rt::futures::now_ns();
  loop {
    let elapsed = slates_rt::futures::now_ns().saturating_sub(began);
    let results = call_once(
      sessionid,
      (program, credential),
      args,
      deadline_ns.saturating_sub(elapsed),
    )
    .await?;
    let delayed = slates_bridge_nfs::v4::callback::status(&results) == Some(NFS4ERR_DELAY);
    let elapsed = slates_rt::futures::now_ns().saturating_sub(began);
    if !delayed || elapsed.saturating_add(DELAY_RETRY_NS) >= deadline_ns {
      return Ok(results);
    }
    let _ = state::with_state(|s| *s.refusals.entry(CALLBACK_DELAYED).or_insert(0) += 1);
    if slates_rt::futures::sleep(DELAY_RETRY_NS).await.is_err() {
      return Ok(results);
    }
  }
}

/// One send of [`call`]: the reply's results within `deadline_ns`, whatever their status.
async fn call_once(
  sessionid: SessionId,
  (program, credential): (u32, &[u8]),
  args: &[u8],
  deadline_ns: u64,
) -> Result<Vec<u8>, CallbackError> {
  let xid = queue(sessionid, (program, credential), args)?;
  let waited = slates_rt::futures::within(
    deadline_ns,
    std::future::poll_fn(|cx| {
      let ready = state::with_state(|s| {
        let pending = s.callbacks.pending.get_mut(&xid)?;
        match pending.reply.take() {
          Some(reply) => Some(reply),
          None => {
            pending.waker = Some(cx.waker().clone());
            None
          }
        }
      })
      .flatten();
      match ready {
        Some(reply) => Poll::Ready(reply),
        None => Poll::Pending,
      }
    }),
  )
  .await;
  let _ = state::with_state(|s| s.callbacks.pending.remove(&xid));
  match waited {
    Ok(Some(reply)) => reply,
    Ok(None) => Err(CallbackError::Timeout),
    Err(_) => Err(CallbackError::NoState),
  }
}

/// Queues one callback record for `sessionid`'s carrier and records it pending: its xid.
fn queue(
  sessionid: SessionId,
  (program, credential): (u32, &[u8]),
  args: &[u8],
) -> Result<u32, CallbackError> {
  state::with_state(|s| {
    let table = &mut s.callbacks;
    let connection = *table
      .carriers
      .get(&sessionid)
      .ok_or(CallbackError::NoCarrier)?;
    if table
      .pending
      .values()
      .any(|pending| pending.sessionid == sessionid)
    {
      return Err(CallbackError::Busy);
    }
    let outbox = table
      .outboxes
      .get_mut(&connection)
      .ok_or(CallbackError::NoCarrier)?;
    table.next_xid = table.next_xid.wrapping_add(1);
    let xid = table.next_xid;
    outbox
      .queue
      .push_back(record(xid, (program, credential), args));
    if let Some(waker) = outbox.waker.take() {
      waker.wake();
    }
    table.pending.insert(
      xid,
      Pending {
        connection,
        sessionid,
        reply: None,
        waker: None,
      },
    );
    Ok(xid)
  })
  .unwrap_or(Err(CallbackError::NoState))
}

/// One record-marked RPC CALL of `CB_COMPOUND` to `program` under `credential` (an encoded `opaque_auth`), with an
/// AUTH_NONE verifier.
fn record(xid: u32, (program, credential): (u32, &[u8]), args: &[u8]) -> Vec<u8> {
  let mut body = XdrWriter::new();
  for word in [xid, RPC_CALL, RPC_VERSION, program, CB_VERSION, CB_COMPOUND] {
    body.u32(word);
  }
  body.fixed(credential);
  body.u32(AUTH_NONE);
  body.u32(0);
  body.fixed(args);
  slates_bridge_nfs::rpc::write_record(body.as_slice())
}
