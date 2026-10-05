//! The daemon's durable NFSv4 state (§4.6 A-37): a file's opens and locks recorded in its owner
//! partition, the listener's clients in its own, so a daemon restart forces no grace period — the
//! client's id and its state ids stay valid, and it re-creates only its session (RFC 8881 §2.10.13).
//!
//! Every call that changes the state commits its changes as one transaction of the partition's log
//! (`slates_db::Db::begin` … `commit`), which the anchor segment keeps across a restart and the
//! partition replicates with its copyset. Memory never runs ahead of the record: a transaction that
//! cannot be committed rolls the partition back to its durable state (the database's own rule), and
//! the state in memory is then rebuilt from those same records — what a restart would build — and the
//! call is answered `NFS4ERR_SERVERFAULT`.

use slates_bridge_nfs::v4::compound::Server as V4Server;
use slates_bridge_nfs::v4::files::{FileChange, FileState, LockRecord, OpenRecord, Share};
use slates_bridge_nfs::v4::lock::{LockKind, Range};
use slates_bridge_nfs::v4::session::{ClientChange, ClientRecord};
use slates_db::Op;
use slates_db::catalog::{NfsClientRecord, NfsLockRange, NfsLockRecord, NfsOpenRecord};

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
  *s.refusals.entry(RECORD_REFUSED).or_insert(0) += 1;
  false
}

/// Counter: delegations granted (or advanced) at this shard's files (A-78).
const DELEGATIONS_GRANTED: &str = "nfs4.delegation.granted";

/// Records the changes the owner's file state made in the call just served. `false` when they could
/// not be recorded; the file state has then been rebuilt from the partition's records.
pub(crate) fn record_files(s: &mut ShardState) -> bool {
  let Some(files) = s.nfs_v4_files.as_mut() else {
    return true;
  };
  let changes = files.take_changes();
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
    *s.refusals.entry(DELEGATIONS_GRANTED).or_insert(0) +=
      u64::try_from(granted).unwrap_or(u64::MAX);
  }
  let ops: Vec<Op> = changes.into_iter().map(file_op).collect();
  if commit(s, &ops) {
    return true;
  }
  s.nfs_v4_files = file_state(s);
  if s.nfs_v4_files.is_none() {
    // No state could be rebuilt: no delegation is held, so the gate holds none either.
    s.store.recall_gate.reset(std::collections::BTreeMap::new());
  }
  false
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
  Some(V4Server::restore(instance, limits, clients(s), now).with_owner(owner))
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

/// The partition operation one file-state change is.
fn file_op(change: FileChange) -> Op {
  match change {
    FileChange::OpenSet(record) => Op::NfsOpenSet {
      record: NfsOpenRecord {
        other: record.other,
        clientid: record.clientid,
        owner: record.owner,
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
  }
}

/// An open from its record.
fn open_of(record: &NfsOpenRecord) -> OpenRecord {
  OpenRecord {
    other: record.other,
    clientid: record.clientid,
    owner: record.owner.clone(),
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
