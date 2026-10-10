//! The daemon's NFSv4.1 call records (A-127; `slates_bridge_nfs::v4::replay` holds the records and their rules): where
//! a kept compound's calls are answered from what a dead daemon did, recorded, made durable with their effects,
//! restored after a restart, and purged.
//!
//! - **Answer, or refuse, before serving.** A call of a recorded compound ([`RecordedCall`]) whose key this shard's
//!   records hold for that same compound is answered from them and served nothing ([`answered`]); one that would
//!   record past the shard's bound is refused before it runs ([`refused`]).
//! - **Record after serving**, with whether the call changed what a commit makes durable ([`record`]). The record then
//!   rides that commit: a file-state change's transaction carries it ([`committed_op`], A-37), and a barrier's
//!   publication carries every durable record changed since the last one ([`delta`]); a checkpoint carries every
//!   durable record ([`checkpoint`]).
//! - **Restore** at a restart from both: the shard image's records and the partition's, the later or longer of two
//!   copies kept ([`restore`]).
//! - **Purge** a dropped client's and a destroyed session's records; the purge is acknowledged only once a
//!   publication carries it, so a restart never brings back a record its partition already cleared ([`purge`]).

use slates_bridge_nfs::v4::replay::{CallIdentity, CallKey, RecordKey, is_bulk};
use slates_bridge_nfs::v4::types::SessionId;
use slates_db::Op;
use slates_db::catalog::NfsCallRecord;
use slates_vfs::recover::HeldCall;

use crate::state::ShardState;

/// Counter: calls of a compound retried after a restart that were answered from their records instead of being served
/// again (A-127).
pub(crate) const REPLAY_ANSWERED: &str = "nfs4.replay.answered";

/// Counter: calls refused before they ran because the shard's call records filled their bound (A-127); the client is
/// told `NFS4ERR_DELAY`.
pub(crate) const RECORDS_FULL: &str = "nfs4.replay.records_full";

/// Counter: call records a restarted shard took back from its image and its partition (A-127).
pub(crate) const RECORDS_RESTORED: &str = "nfs4.replay.restored";

/// Counter: kept call records a restarted shard could not decode, left out (A-127).
pub(crate) const RECORDS_UNREADABLE: &str = "nfs4.replay.unreadable";

/// Counter: purges of call records whose publication was refused, so the purge was refused and stays queued at its
/// sender (A-127).
pub(crate) const PURGE_UNPUBLISHED: &str = "nfs4.replay.purge_unpublished";

/// Counter: call records dropped by purges, a dropped client's or a destroyed session's (A-127).
pub(crate) const RECORDS_PURGED: &str = "nfs4.replay.purged";

/// A call of a recorded compound: its compound and its key, riding the call to the shard that serves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordedCall {
  /// The compound.
  pub(crate) identity: CallIdentity,
  /// The call's key in it.
  pub(crate) key: CallKey,
}

/// Counter: calls of a retried compound that met an entry of the same procedure and occurrence but another digest, so
/// they were served again (A-127): a call whose arguments change across a retry, which must never be silent.
pub(crate) const REPLAY_DIVERGED: &str = "nfs4.replay.diverged";

/// The recorded result `call` is answered with, when this shard served it for the same compound before a restart;
/// counted, as is a call that met its procedure and occurrence under another digest.
pub(crate) fn answered(s: &mut ShardState, call: &RecordedCall) -> Option<Vec<u8>> {
  let Some(answer) = s
    .nfs_calls
    .answer(&call.identity, &call.key)
    .map(<[u8]>::to_vec)
  else {
    if s.nfs_calls.diverged(&call.identity, &call.key) {
      // Counted every time and logged once per shard, naming the procedure, so a retry that takes another path is
      // found from the log without an unbounded one.
      let seen = s.refusals.entry(REPLAY_DIVERGED).or_insert(0);
      *seen = seen.saturating_add(1);
      if *seen == 1 {
        eprintln!(
          "slates-server: partition {}: a retried compound's call met its record under another digest (procedure {}, \
           occurrence {}); served again (first on this shard; later ones are counted as {REPLAY_DIVERGED})",
          s.partition, call.key.procedure, call.key.occurrence
        );
      }
    }
    return None;
  };
  s.count(REPLAY_ANSWERED, 1);
  Some(answer)
}

/// Whether `call` must be refused before it runs because recording it would pass the shard's bound; counted.
pub(crate) fn refused(s: &mut ShardState, call: &RecordedCall) -> bool {
  let refuses = s.nfs_calls.refuses(&call.identity);
  if refuses {
    s.count(RECORDS_FULL, 1);
  }
  refuses
}

/// Records `call` of `procedure` with its `reply` (by its key alone for a bulk read); `published` when it changed what
/// the shard's next publication carries, which the record then rides.
pub(crate) fn record(
  s: &mut ShardState,
  call: &RecordedCall,
  procedure: u32,
  reply: &[u8],
  published: bool,
) {
  let kept = (!is_bulk(procedure)).then_some(reply);
  s.nfs_calls
    .record(&call.identity, call.key, kept, published);
}

/// The partition operation that carries `call`'s slot record in the transaction of the file-state change the call
/// made.
pub(crate) fn committed_op(s: &ShardState, call: &RecordedCall) -> Option<Op> {
  let (key, record) = s
    .nfs_calls
    .committed(&call.identity.session, call.identity.slot)?;
  Some(Op::NfsCallSet {
    record: NfsCallRecord {
      key,
      clientid: call.identity.clientid,
      record,
    },
  })
}

/// What the shard's next delta carries: the durable records changed since its last publication, and the slots purged
/// since.
pub(crate) fn delta(s: &ShardState) -> (Vec<HeldCall>, Vec<RecordKey>) {
  let (changed, removed) = s.nfs_calls.delta();
  (held(changed), removed)
}

/// What a checkpoint of the shard carries: every durable record.
pub(crate) fn checkpoint(s: &ShardState) -> Vec<HeldCall> {
  held(s.nfs_calls.checkpoint())
}

/// Records encoded under their keys, as an image holds them.
fn held(records: Vec<(RecordKey, Vec<u8>)>) -> Vec<HeldCall> {
  records
    .into_iter()
    .map(|(key, record)| HeldCall { key, record })
    .collect()
}

/// Takes back the records a restarted shard kept: those its recovered `image` carries and those its partition holds;
/// counted.
pub(crate) fn restore(s: &mut ShardState, image: Vec<HeldCall>) {
  let from_partition: Vec<(RecordKey, Vec<u8>)> = s
    .db
    .partition()
    .nfs_calls()
    .map(|record| (record.key, record.record.clone()))
    .collect();
  let kept = image
    .into_iter()
    .map(|held| (held.key, held.record))
    .chain(from_partition);
  let refused = s.nfs_calls.restore(kept);
  let restored = s.nfs_calls.len();
  s.count(
    RECORDS_RESTORED,
    u64::try_from(restored).unwrap_or(u64::MAX),
  );
  if refused > 0 {
    s.count(
      RECORDS_UNREADABLE,
      u64::try_from(refused).unwrap_or(u64::MAX),
    );
  }
}

/// What a purge drops.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Purged {
  /// Every record of a dropped client (its partition copies go with its state, `Op::NfsClientStateCleared`).
  Client(u64),
  /// Every record of a destroyed session.
  Session(SessionId),
}

/// Drops the records `what` names, here and in the partition, and publishes the shard so its image drops them too:
/// `false` when the partition or the publication refused, so the purge is refused and its sender keeps it queued. A
/// purge whose records are already gone here still publishes while an earlier purge's removal is owed to the image,
/// so a retried purge is acknowledged only once the image dropped them (a restart must never bring back a record a
/// partition already cleared).
pub(crate) fn purge(s: &mut ShardState, what: Purged) -> bool {
  let before = s.nfs_calls.len();
  let op = match what {
    Purged::Client(clientid) => {
      s.nfs_calls.purge_client(clientid);
      Op::NfsClientStateCleared { clientid }
    }
    Purged::Session(sessionid) => {
      s.nfs_calls.purge_session(&sessionid);
      Op::NfsCallsCleared { sessionid }
    }
  };
  let dropped = before.saturating_sub(s.nfs_calls.len());
  if dropped > 0 {
    s.count(RECORDS_PURGED, u64::try_from(dropped).unwrap_or(u64::MAX));
    if !crate::nfs_state::commit_ops(s, &[op]) {
      return false;
    }
  }
  if !s.nfs_calls.owes_removals() {
    return true;
  }
  let published = crate::verbs::publish_shard(s).is_ok();
  if !published {
    s.count(PURGE_UNPUBLISHED, 1);
  }
  published
}
