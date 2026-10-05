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
//! The gate's set follows the file state: `nfs_state::record_files` rebuilds it whenever a delegation changes, so
//! every revocation and return here is persisted and reflected in the gate by the record call that ends [`drain`].

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
    *s.refusals.entry(REVOKED_LAPSED).or_insert(0) += 1;
  }
  for recall in sends {
    match recall_target(s, recall.clientid) {
      Some((sessionid, next)) => {
        *s.refusals.entry(RECALL_SENT).or_insert(0) += 1;
        spawn_recall(sessionid, next, recall);
      }
      None => {
        *s.refusals.entry(REVOKED_UNREACHABLE).or_insert(0) += 1;
        if let Some(files) = s.nfs_v4_files.as_mut() {
          files.revoke_delegation(&recall.stateid.other);
        }
      }
    }
  }
  crate::nfs_state::record_files(s);
}

/// The session a recall to `clientid` goes to, and that callback's program, credential, minor version and sequence:
/// a session of the client held on this shard whose back channel has answered.
fn recall_target(
  s: &mut ShardState,
  clientid: u64,
) -> Option<(SessionId, slates_bridge_nfs::v4::session::NextCallback)> {
  let sessions = &mut s.nfs_v4.as_mut()?.sessions;
  let sessionid = sessions.session_with_back_channel(clientid)?;
  let next = sessions.next_callback(&sessionid)?;
  Some((sessionid, next))
}

/// Sends `CB_RECALL` for `recall` on `sessionid`'s back channel in a detached task, counting whether it was answered.
fn spawn_recall(
  sessionid: SessionId,
  next: slates_bridge_nfs::v4::session::NextCallback,
  recall: Recall,
) {
  let task = futures::spawn(async move {
    let args = callback::recall(
      (&sessionid, next.minor, next.sequence),
      &recall.stateid,
      false,
      &recall.fh.0,
    );
    let answered = crate::callback::call(
      sessionid,
      (next.program, &next.credential),
      &args,
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await
    .ok()
    .and_then(|results| callback::status(&results))
      == Some(slates_bridge_nfs::v4::Nfsstat4::Ok as u32);
    let _ = state::with_state(|s| {
      let counter = if answered {
        RECALL_ANSWERED
      } else {
        RECALL_UNANSWERED
      };
      *s.refusals.entry(counter).or_insert(0) += 1;
    });
  });
  match task {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => {
      let _ = state::with_state(|s| *s.refusals.entry(RECALL_UNANSWERED).or_insert(0) += 1);
    }
  }
}
