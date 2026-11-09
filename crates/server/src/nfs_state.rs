//! The daemon's durable NFSv4 state (§4.6 A-37): a file's opens and locks recorded in its owner
//! partition, the listener's clients and their sessions (A-127) in its own, so a daemon restart forces no grace
//! period and no new session — the client's id, its state ids and its sessions stay valid, and it retries on its own
//! slots (RFC 8881 §2.10.6.5: a server may re-animate a session whose client it kept).
//!
//! Every call that changes the state commits its changes as one transaction of the partition's log
//! (`slates_db::Db::begin` … `commit`), which the anchor segment keeps across a restart and the
//! partition replicates with its copyset. Memory never runs ahead of the record: a transaction that
//! cannot be committed rolls the partition back to its durable state (the database's own rule), and
//! the state in memory is then rebuilt from those same records — what a restart would build — and the
//! call is answered `NFS4ERR_SERVERFAULT`.
//!
//! Every open and delegation also holds its file in the volume (A-260): one reference per open and per delegation,
//! owned by [`RefOwner::NfsState`] of the attachment whose handle made it, taken when its record commits and given
//! back when the record of its end does. A file removed while one is held keeps serving at a link count of zero, and
//! its space comes back at the last end, as POSIX's unlink-while-open has it; the server promises its clients as much
//! with `OPEN4_RESULT_PRESERVE_UNLINKED`, so they send the REMOVE instead of renaming the file aside (`.nfs*`, a name
//! a landing would carry). The journal is these holds' only record: the recovery image carries none, and a recovery
//! takes them again from the journaled opens and delegations before it reclaims the files no one holds
//! ([`hold_recovered`]). An attachment's end gives its holds back ([`release_attachment`]). They are bounded by the
//! state's own bounds, the opens and the delegations it may record.

use slates_bridge_nfs::v4::compound::Server as V4Server;
use slates_bridge_nfs::v4::files::{FileChange, FileState, LockRecord, OpenRecord, Share};
use slates_bridge_nfs::v4::lock::{LockKind, Range};
use slates_bridge_nfs::v4::session::{ClientChange, ClientRecord, Purge, SessionRecord};
use slates_bridge_nfs::v4::types::ChannelAttrs;
use slates_db::Op;
use slates_db::catalog::{
  NfsCallbackRecord, NfsChannelRecord, NfsClientRecord, NfsLockRange, NfsLockRecord, NfsOpenRecord,
  NfsPurgeRecord, NfsSessionRecord,
};

use slates_bridge_nfs::handle::FileHandle;
use slates_bridge_nfs::nfs::Nfsfh3;
use slates_bridge_nfs::v4::files::Holding;
use slates_vfs::ids::{InodeNo, RefOwner};

use crate::state::ShardState;

/// The refusal count a failed record adds to the shard's status report.
/// Format: a refusal name in the status report.
const RECORD_REFUSED: &str = "nfs.v4_record_refused";

/// Commits `ops` as one transaction of the shard's partition. `false` when the commit failed (the
/// database has then rolled the partition back to its durable state) or an operation was refused.
fn commit(s: &mut ShardState, ops: &[Op]) -> bool {
  if ops.is_empty() {
    return true;
  }
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
  s.db.begin();
  let applied = ops
    .iter()
    .all(|op| s.db.mutate(&mut s.segment, op, now).is_ok());
  let committed = s.db.commit(&mut s.segment).is_ok();
  if applied && committed {
    return true;
  }
  s.count(RECORD_REFUSED, 1);
  false
}

/// Commits `ops` as one transaction of the shard's partition, for a caller outside this module (A-127's purges).
pub(crate) fn commit_ops(s: &mut ShardState, ops: &[Op]) -> bool {
  commit(s, ops)
}

/// Counter: delegations granted (or advanced) at this shard's files (A-78).
const DELEGATIONS_GRANTED: &str = "nfs4.delegation.granted";

/// Records the changes the owner's file state made in the call just served. `false` when they could
/// not be recorded; the file state has then been rebuilt from the partition's records. When the call belongs to a
/// recorded compound (A-127, the shard's call in flight) and it changed file state, its `reply` is recorded and the
/// slot's record rides the same transaction, so no restart keeps the change and loses what the call answered.
pub(crate) fn record_files(s: &mut ShardState, reply: Option<&[u8]>) -> bool {
  let Some(files) = s.nfs_v4_files.as_mut() else {
    return true;
  };
  let changes = files.take_changes();
  // Taken with the changes: a call whose changes cannot be recorded never happened, and its holdings go with it.
  let holdings = files.take_holdings();
  // The store's recall gate follows the delegations (A-79): each inode whose holders changed (a grant, return,
  // revocation, or a client's purge) is updated alone (A-80: a whole rebuild walked every delegation on every grant).
  for inode in files.take_changed_inodes() {
    let holders = files.holders_of_inode(inode);
    s.store.recall_gate.set_holders(inode, holders);
  }
  let granted = changes
    .iter()
    .filter(|change| matches!(change, FileChange::DelegationSet(_)))
    .count();
  if granted > 0 {
    s.count(
      DELEGATIONS_GRANTED,
      u64::try_from(granted).unwrap_or(u64::MAX),
    );
  }
  let mut ops: Vec<Op> = changes.into_iter().map(file_op).collect();
  if !ops.is_empty()
    && let (Some(call), Some(reply)) = (s.nfs_call, reply)
  {
    // The record rides this transaction, not a publication: the change is file state (A-127).
    crate::nfs_replay::record(s, &call, call.key.procedure, reply, false);
    ops.extend(crate::nfs_replay::committed_op(s, &call));
  }
  if commit(s, &ops) {
    for holding in holdings {
      hold(s, holding);
    }
    return true;
  }
  s.nfs_v4_files = file_state(s);
  if s.nfs_v4_files.is_none() {
    // No state could be rebuilt: no delegation is held, so the gate holds none either.
    s.store.recall_gate.reset(std::collections::BTreeMap::new());
  }
  false
}

/// Counter: holds an NFSv4 open or delegation could not take or give back in its volume (A-260): the file was gone
/// already (a handle to a reclaimed file, which the next call by it is told is stale), or its volume is not served
/// here. Format: a refusal name in the status report.
const HOLD_REFUSED: &str = "nfs4.hold_refused";

/// Counter: holds taken again from the partition's journaled opens and delegations by a recovery (A-260): the
/// non-vacuity check that the restart oracle's file was held through the recovery, not reclaimed and served by luck.
/// Format: a counter name in the status report.
const HOLDS_RECOVERED: &str = "nfs4.hold_recovered";

/// Takes or gives back the volume reference one NFSv4 open or delegation holds (A-260). A handle of a snapshot
/// attachment names an object of its read-only view, which no REMOVE reaches, so it holds nothing; a refusal is
/// counted, never answered: the open it belongs to was recorded and answered already.
fn hold(s: &mut ShardState, holding: Holding) {
  let (fh, took) = match holding {
    Holding::Took(fh) => (fh, true),
    Holding::Gave(fh) => (fh, false),
  };
  if hold_file(s, &fh, took).is_none() {
    s.count(HOLD_REFUSED, 1);
  }
}

/// The hold of the file `fh` names taken (`took`) or given back, or `None` when it could not be.
fn hold_file(s: &mut ShardState, fh: &Nfsfh3, took: bool) -> Option<()> {
  let handle = FileHandle::from_fh(fh).ok()?;
  let record = s.db.partition().attachment(handle.attachment)?;
  if record.snapshot.is_some() {
    return Some(());
  }
  let slot = *s.by_id.get(&handle.volume)?;
  let ShardState { volumes, store, .. } = s;
  let volume = &mut volumes.get_mut(slot).ok()?.volume;
  let owner = RefOwner::NfsState(handle.attachment);
  let inode = InodeNo(handle.inode);
  if took {
    volume.reference_for(store, inode, owner).ok()
  } else {
    volume.forget_for(store, inode, owner, 1).ok()
  }
}

/// Takes again, for the volume `volume` a recovery just rebuilt, the hold of every NFSv4 open and delegation its
/// partition journaled (A-260), before the recovery settles its references and reclaims the files no one holds: the
/// recovery image carries none of these holds, so a file removed while open keeps serving across the restart, and one
/// whose last open ended before it is reclaimed. Returns the holds taken.
pub(crate) fn hold_recovered(s: &mut ShardState, volume: slates_db::catalog::VolumeId) -> usize {
  let partition = s.db.partition();
  let held: Vec<Nfsfh3> = partition
    .nfs_opens()
    .map(|record| Nfsfh3(record.fh.clone()))
    .chain(
      partition
        .nfs_delegations()
        .map(|record| Nfsfh3(record.fh.clone())),
    )
    .filter(|fh| FileHandle::from_fh(fh).is_ok_and(|handle| handle.volume == volume))
    .collect();
  let mut taken = 0usize;
  for fh in held {
    match hold_file(s, &fh, true) {
      Some(()) => taken = taken.saturating_add(1),
      None => s.count(HOLD_REFUSED, 1),
    }
  }
  s.count(HOLDS_RECOVERED, u64::try_from(taken).unwrap_or(u64::MAX));
  taken
}

/// Gives back every hold the NFSv4 opens and delegations made through the attachment `attachment` have in its volume
/// `volume` (A-260), as the attachment ends: its mount's requests are refused from now on, so nothing it holds open can
/// be read or closed through it. The opens stay in the file state until their client closes them or is purged, and
/// their give-backs then find nothing to give.
pub(crate) fn release_attachment(
  s: &mut ShardState,
  volume: slates_db::catalog::VolumeId,
  attachment: u64,
) {
  let Some(slot) = s.by_id.get(&volume).copied() else {
    return;
  };
  let ShardState { volumes, store, .. } = s;
  let released = volumes.get_mut(slot).map(|slot| {
    slot
      .volume
      .sweep_attachment(store, RefOwner::NfsState(attachment))
  });
  if !matches!(released, Ok(Ok(()))) {
    s.count(HOLD_REFUSED, 1);
  }
}

/// Records the changes the listener's client table made. `false` when they could not be recorded; the
/// listener has then been rebuilt from the partition's records (its clients kept, their sessions
/// gone, which each client recovers from as after a restart).
pub(crate) fn record_clients(s: &mut ShardState) -> bool {
  let Some(listener) = s.nfs_v4.as_mut() else {
    return true;
  };
  let ops: Vec<Op> = listener
    .sessions
    .take_changes()
    .into_iter()
    .map(client_op)
    .collect();
  if commit(s, &ops) {
    return true;
  }
  s.nfs_v4 = server(s);
  false
}

/// The first instance of a partition that holds none (a fresh daemon, or one whose anchor's RAM was lost): 31 random
/// bits of this boot's nonce (never zero), not 1. Client ids carry the instance in their high bits (RFC 8881 §2.4: a
/// server's client ids must not repeat across its restarts, so a client presenting one from a life that ended is
/// told `NFS4ERR_STALE_CLIENTID` rather than taken for another client; Linux nfsd puts its boot time there). A
/// partition that starts at 1 every time repeats them after every loss of its records. Thirty-one bits leave 2³¹
/// restarts of headroom under the instance's forward-only rule, and two lives share a first instance with
/// probability 2⁻³¹.
fn fresh_instance(boot_nonce: u64) -> u32 {
  /// Format: the nonce's bits above the 31 kept.
  const DROPPED: u32 = u64::BITS - (u32::BITS - 1);
  u32::try_from(boot_nonce >> DROPPED).unwrap_or(0).max(1)
}

/// This daemon life's NFSv4 instance on the shard's partition (§4.6 A-37): advanced durably past every
/// earlier life's on first use, so no client, session or state id this life mints repeats one an
/// earlier life minted. `None` if the advance could not be recorded; no id is then minted.
pub(crate) fn instance(s: &mut ShardState) -> Option<u32> {
  if let Some(instance) = s.nfs_v4_instance {
    return Some(instance);
  }
  let current = s.db.partition().nfs_instance();
  let next = if current == 0 {
    fresh_instance(s.member_boot_nonce)
  } else {
    current.checked_add(1)?
  };
  if !commit(s, &[Op::NfsInstanceAdvanced { instance: next }]) {
    return None;
  }
  s.nfs_v4_instance = Some(next);
  Some(next)
}

/// This shard's file state, rebuilt from its partition's records (§4.6 A-37), its new state ids
/// tagged with the shard's partition and this life's instance, bounded by the configuration's derived
/// bounds. `None` when the instance could not be advanced.
pub(crate) fn file_state(s: &mut ShardState) -> Option<FileState> {
  let instance = instance(s)?;
  let caps = s.config.nfs_v4;
  let tag = slates_bridge_nfs::v4::files::owner_tag(s.partition, instance);
  let partition = s.db.partition();
  let restored = FileState::restore(
    tag,
    caps.opens,
    caps.locks,
    partition.nfs_opens().map(open_of).collect(),
    partition.nfs_locks().map(lock_of).collect(),
    partition
      .nfs_delegations()
      .map(
        |record| slates_bridge_nfs::v4::delegation::DelegationRecord {
          other: record.other,
          clientid: record.clientid,
          fh: record.fh.clone(),
          write: record.write,
          seqid: record.seqid,
        },
      )
      .collect(),
  );
  let mut files = restored;
  // A recalled file is not delegated again for a lease (RFC 8881 §10.4, A-78).
  files.set_delegation_quiet(s.config.failover_slo_ns);
  // The gate starts closed on every file a kept delegation names (a restart must not let a change slip past one), set
  // whole once; the restore's own changed inodes are then already reflected.
  s.store.recall_gate.reset(files.delegated_holders());
  files.take_changed_inodes();
  Some(files)
}

/// The listener's NFSv4 server, its client table rebuilt from the partition's records and its new
/// client and session ids minted under this life's instance. `None` when the instance could not be
/// advanced.
pub(crate) fn server(s: &mut ShardState) -> Option<V4Server> {
  let instance = instance(s)?;
  let limits = crate::nfs::v4_limits(s);
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
  let owner = server_owner(s.origin_anchor.0, &s.config.instance);
  Some(
    V4Server::restore(instance, limits, (clients(s), sessions(s), purges(s)), now)
      .with_owner(owner),
  )
}

/// The `server_owner4` major id of the daemon instance `instance` on the host whose anchor identity is `anchor`
/// (RFC 8881 §2.10.5): the same across the instance's restarts (a restarted server is the same server, its clients
/// told by `NFS4ERR_STALE_CLIENTID` that their state is gone), different for every other instance on the host and
/// for every other host, whose instance names may coincide. A keyed hash in its own domain, so it never equals
/// another identity derived from the same anchor.
fn server_owner(anchor: u64, instance: &str) -> u64 {
  let mut hasher = blake3::Hasher::new_derive_key("slates/nfs-server-owner/v1");
  hasher.update(&anchor.to_le_bytes());
  hasher.update(instance.as_bytes());
  let digest = hasher.finalize();
  let mut word = [0; size_of::<u64>()];
  if let Some(head) = digest.as_bytes().get(..size_of::<u64>()) {
    word.copy_from_slice(head);
  }
  u64::from_le_bytes(word)
}

/// The clients this shard's listener keeps, from its partition's records.
pub(crate) fn clients(s: &ShardState) -> Vec<ClientRecord> {
  s.db
    .partition()
    .nfs_clients()
    .map(|record| ClientRecord {
      clientid: record.clientid,
      owner: record.owner.clone(),
      verifier: record.verifier,
      principal: record.principal,
      create_seq: record.create_seq,
    })
    .collect()
}

/// The sessions this shard's listener keeps, from its partition's records (A-127).
/// The purges this shard's table owes its owners, from the partition's records (2026-10-10): what a restart left owed,
/// sent by the next purge round.
fn purges(s: &ShardState) -> Vec<Purge> {
  s.db.partition().nfs_purges().map(purge_of).collect()
}

/// A purge from its record.
fn purge_of(record: &NfsPurgeRecord) -> Purge {
  match record.session {
    Some(sessionid) => Purge::Session(record.clientid, sessionid),
    None => Purge::Client(record.clientid),
  }
}

/// A purge's record.
fn purge_record(purge: Purge) -> NfsPurgeRecord {
  match purge {
    Purge::Client(clientid) => NfsPurgeRecord {
      clientid,
      session: None,
    },
    Purge::Session(clientid, sessionid) => NfsPurgeRecord {
      clientid,
      session: Some(sessionid),
    },
  }
}

pub(crate) fn sessions(s: &ShardState) -> Vec<SessionRecord> {
  s.db
    .partition()
    .nfs_sessions()
    .map(|record| SessionRecord {
      sessionid: record.sessionid,
      clientid: record.clientid,
      sequence: record.sequence,
      fore: channel_of(&record.fore),
      back: channel_of(&record.back),
      callback: record.callback.as_ref().map(|callback| {
        (
          callback.program,
          callback.credential.clone(),
          callback.minor,
        )
      }),
    })
    .collect()
}

/// A channel's attributes from their record.
fn channel_of(record: &NfsChannelRecord) -> ChannelAttrs {
  ChannelAttrs {
    header_pad: record.header_pad,
    max_request: record.max_request,
    max_response: record.max_response,
    max_response_cached: record.max_response_cached,
    max_operations: record.max_operations,
    max_requests: record.max_requests,
  }
}

/// A channel's attributes as their record carries them.
fn channel_record(attrs: &ChannelAttrs) -> NfsChannelRecord {
  NfsChannelRecord {
    header_pad: attrs.header_pad,
    max_request: attrs.max_request,
    max_response: attrs.max_response,
    max_response_cached: attrs.max_response_cached,
    max_operations: attrs.max_operations,
    max_requests: attrs.max_requests,
  }
}

/// The partition operation one file-state change is.
fn file_op(change: FileChange) -> Op {
  match change {
    FileChange::OpenSet(record) => Op::NfsOpenSet {
      record: NfsOpenRecord {
        other: record.other,
        clientid: record.clientid,
        owner: record.owner,
        uid: record.uid,
        fh: record.fh,
        access: record.share.access,
        deny: record.share.deny,
        seqid: record.seqid,
      },
    },
    FileChange::OpenCleared(other) => Op::NfsOpenCleared { other },
    FileChange::LockSet(record) => Op::NfsLockSet {
      record: NfsLockRecord {
        other: record.other,
        clientid: record.clientid,
        owner: record.owner,
        fh: record.fh,
        open: record.open,
        seqid: record.seqid,
        ranges: record
          .ranges
          .iter()
          .map(|(range, kind)| NfsLockRange {
            start: range.start,
            end: range.end,
            write: *kind == LockKind::Write,
          })
          .collect(),
      },
    },
    FileChange::LockCleared(other) => Op::NfsLockCleared { other },
    FileChange::ClientCleared(clientid) => Op::NfsClientStateCleared { clientid },
    FileChange::DelegationSet(record) => Op::NfsDelegationSet {
      record: slates_db::catalog::NfsDelegationRecord {
        other: record.other,
        clientid: record.clientid,
        fh: record.fh,
        write: record.write,
        seqid: record.seqid,
      },
    },
    FileChange::DelegationCleared(other) => Op::NfsDelegationCleared { other },
  }
}

/// The partition operation one client-table change is.
fn client_op(change: ClientChange) -> Op {
  match change {
    ClientChange::Set(record) => Op::NfsClientSet {
      record: NfsClientRecord {
        clientid: record.clientid,
        owner: record.owner,
        verifier: record.verifier,
        principal: record.principal,
        create_seq: record.create_seq,
      },
    },
    ClientChange::Cleared(clientid) => Op::NfsClientCleared { clientid },
    ClientChange::SessionSet(record) => Op::NfsSessionSet {
      record: NfsSessionRecord {
        sessionid: record.sessionid,
        clientid: record.clientid,
        sequence: record.sequence,
        fore: channel_record(&record.fore),
        back: channel_record(&record.back),
        callback: record
          .callback
          .map(|(program, credential, minor)| NfsCallbackRecord {
            program,
            credential,
            minor,
          }),
      },
    },
    ClientChange::SessionCleared(sessionid) => Op::NfsSessionCleared { sessionid },
    ClientChange::PurgeOwed(purge) => Op::NfsPurgeOwed {
      record: purge_record(purge),
    },
    ClientChange::PurgeSettled(purge) => Op::NfsPurgeSettled {
      record: purge_record(purge),
    },
  }
}

/// An open from its record.
fn open_of(record: &NfsOpenRecord) -> OpenRecord {
  OpenRecord {
    other: record.other,
    clientid: record.clientid,
    owner: record.owner.clone(),
    uid: record.uid,
    fh: record.fh.clone(),
    share: Share {
      access: record.access,
      deny: record.deny,
    },
    seqid: record.seqid,
  }
}

/// A lock state from its record.
fn lock_of(record: &NfsLockRecord) -> LockRecord {
  LockRecord {
    other: record.other,
    clientid: record.clientid,
    owner: record.owner.clone(),
    fh: record.fh.clone(),
    open: record.open,
    seqid: record.seqid,
    ranges: record
      .ranges
      .iter()
      .map(|range| {
        let kind = if range.write {
          LockKind::Write
        } else {
          LockKind::Read
        };
        (
          Range {
            start: range.start,
            end: range.end,
          },
          kind,
        )
      })
      .collect(),
  }
}
