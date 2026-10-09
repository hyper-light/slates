//! Delegation recalls in the daemon (RFC 8881 §10.2, §10.4.4–10.4.5; §4.6 A-79): after each operation a shard serves,
//! the recalls its changes asked for are sent, and delegations not returned within a lease of their recall are
//! revoked.
//!
//! A change to a delegated file by anyone but its holder is refused at the volume core's recall gate
//! (`slates_vfs::recall_gate`) and its inode queued there with the change's actor; an NFSv4 open that conflicts with
//! another holder's delegation is answered `NFS4ERR_DELAY` at the file state, which queues its recall (A-80).
//! [`drain`] runs on the owner shard after the operation, takes both queues, and recalls each file's other holders'
//! delegations: a `CB_RECALL` on the holder's back channel, sent by a detached
//! task of this shard that records the outcome (never a silent drop). A holder with no answering back channel on
//! this shard cannot be told to return its delegation, so it is revoked at once; the holder learns it on its next
//! `SEQUENCE` (`SEQ4_STATUS_RECALLABLE_STATE_REVOKED`). A holder that does not return a recalled delegation within a
//! lease loses it the same way (§10.4.5). The refused change is retried by its caller and proceeds once the file
//! leaves the gate.
//!
//! The gate's set follows the file state: `nfs_state::record_files` updates the holders of each inode whose
//! delegations changed (never a rebuild over every delegation, A-80), so every revocation and return here is persisted
//! and reflected in the gate by the record call that ends [`drain`].
//!
//! A session's back channel has one slot (RFC 8881 §2.10.6), so its recalls wait in a queue per session and one sender
//! task sends them in turn through `callback::call_on_slot`, as Linux knfsd serializes its callbacks on the slot
//! (`nfsd41_cb_get_slot`). A Linux client takes two operations per callback compound (`NFS4_MAX_BACK_CHANNEL_OPS`),
//! so recalls cannot share one. Until 2026-10-08 each recall was sent at once in its own task: of a burst of 153 (one
//! outsider's edit beneath a delegated directory), 152 were refused busy and the slot's sequence ran ahead of the
//! client's. Bounds: a queue holds each delegation at most once (by its state id), so it is bounded by the delegations
//! its client holds; a session has at most one sender, which ends when its queue is empty.

use std::collections::VecDeque;

use slates_bridge_nfs::v4::callback;
use slates_bridge_nfs::v4::delegation::Recall;
use slates_bridge_nfs::v4::types::SessionId;
use slates_rt::futures;

use crate::state::{self, ShardState};

/// Counter: recalls sent on a holder's back channel.
const RECALL_SENT: &str = "nfs4.recall.sent";
/// Counter: recalls the holder answered `NFS4_OK` (it will return the delegation).
const RECALL_ANSWERED: &str = "nfs4.recall.answered";
/// Counter: recalls the holder refused or did not answer (the delegation is revoked when its lease passes).
const RECALL_UNANSWERED: &str = "nfs4.recall.unanswered";
/// Counter: delegations revoked at once because their holder has no answering back channel here.
const REVOKED_UNREACHABLE: &str = "nfs4.delegation.revoked_unreachable";
/// Counter: delegations revoked because their recall's lease passed without a return (§10.4.5).
const REVOKED_LAPSED: &str = "nfs4.delegation.revoked_lapsed";

/// Sends the recalls this shard's changes asked for and revokes lapsed delegations (the module doc). Called after
/// each operation the shard serves; cheap when nothing is delegated.
pub(crate) fn drain(s: &mut ShardState) {
  // With no delegation at all, neither the gate nor a conflicting open can have asked for a recall.
  if s.store.recall_gate.is_open() {
    return;
  }
  let requested = s.store.recall_gate.take_requested();
  let now = futures::now_ns();
  let lease = s.config.failover_slo_ns;
  let Some(files) = s.nfs_v4_files.as_mut() else {
    return;
  };
  let quiet = files.delegation_quiet();
  let lapsed = files.revoke_lapsed(now, lease, quiet);
  // The recalls conflicting NFSv4 opens began at the file state, then those the gate's refused changes ask for.
  let mut sends: Vec<Recall> = files.take_recalls();
  for (inode, actor) in requested {
    sends.extend(files.recall_inode(inode, actor, now).send);
  }
  for _ in &lapsed {
    s.count(REVOKED_LAPSED, 1);
  }
  for recall in sends {
    match recall_target(s, recall.clientid) {
      Some(sessionid) => enqueue(s, sessionid, recall),
      None => {
        s.count(REVOKED_UNREACHABLE, 1);
        if let Some(files) = s.nfs_v4_files.as_mut() {
          files.revoke_delegation(&recall.stateid.other);
        }
      }
    }
  }
  crate::nfs_state::record_files(s);
}

/// The session a recall to `clientid` goes to: a session of the client held on this shard whose back channel has
/// answered.
fn recall_target(s: &mut ShardState, clientid: u64) -> Option<SessionId> {
  s.nfs_v4
    .as_ref()?
    .sessions
    .session_with_back_channel(clientid)
}

/// The recalls waiting for one session's back channel, and whether its sender runs.
#[derive(Debug, Default)]
pub(crate) struct RecallQueue {
  /// The recalls not yet sent, oldest first; each delegation at most once.
  pending: VecDeque<Recall>,
  /// Whether a sender task is draining the queue.
  sending: bool,
}

/// Queues `recall` for `sessionid`'s back channel (once per delegation) and starts the session's sender when none runs.
fn enqueue(s: &mut ShardState, sessionid: SessionId, recall: Recall) {
  let queue = s.recall_queues.entry(sessionid).or_default();
  if !queue
    .pending
    .iter()
    .any(|queued| queued.stateid.other == recall.stateid.other)
  {
    queue.pending.push_back(recall);
  }
  if queue.sending {
    return;
  }
  queue.sending = true;
  match futures::spawn(send_recalls(sessionid)) {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => {
      // No task to send them: the queue waits for the next drain to start one, and the delegations lapse to
      // revocation if none does (§10.4.5). Counted, never silent.
      if let Some(queue) = s.recall_queues.get_mut(&sessionid) {
        queue.sending = false;
      }
      s.count(RECALL_SENDER_REFUSED, 1);
    }
  }
}

/// Counter: back channels a lost recall marked down (the client is told `SEQ4_STATUS_CB_PATH_DOWN_SESSION`).
const CALLBACK_PATH_DOWN: &str = "nfs4.callback.path_down";

/// Counter: recall senders the shard's task arena refused (their recalls wait for the next drain).
const RECALL_SENDER_REFUSED: &str = "nfs4.recall.sender_refused";

/// Sends `sessionid`'s queued recalls one at a time until the queue is empty, counting each answered or not.
async fn send_recalls(sessionid: SessionId) {
  loop {
    let next = state::with_state(|s| {
      let queue = s.recall_queues.get_mut(&sessionid)?;
      let recall = queue.pending.pop_front();
      if recall.is_none() {
        s.recall_queues.remove(&sessionid);
      }
      recall
    })
    .flatten();
    let Some(recall) = next else {
      return;
    };
    let _ = state::with_state_counted(|s| s.count(RECALL_SENT, 1));
    let answered = crate::callback::call_on_slot(
      sessionid,
      |next| {
        callback::recall(
          (&sessionid, next.minor, next.sequence),
          &recall.stateid,
          false,
          &recall.fh.0,
        )
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    // A callback the transport lost (no reply, no carrier, a refused RPC) marks the back channel down, so the client is
    // told on its next SEQUENCE and can bind it again (RFC 8881 §18.46.3); one the client answered, whatever its
    // status, proves the channel works.
    let path_down = matches!(
      answered,
      Err(
        crate::callback::CallbackError::Timeout
          | crate::callback::CallbackError::Lost
          | crate::callback::CallbackError::NoCarrier
          | crate::callback::CallbackError::Refused
      )
    );
    let answered = answered.ok().and_then(|results| callback::status(&results))
      == Some(slates_bridge_nfs::v4::Nfsstat4::Ok as u32);
    let _ = state::with_state_counted(|s| {
      if path_down && let Some(server) = s.nfs_v4.as_mut() {
        server.sessions.set_callback_state(
          &sessionid,
          slates_bridge_nfs::v4::session::CallbackState::Down,
        );
        s.count(CALLBACK_PATH_DOWN, 1);
      }
      s.count(
        if answered {
          RECALL_ANSWERED
        } else {
          RECALL_UNANSWERED
        },
        1,
      );
    });
  }
}
