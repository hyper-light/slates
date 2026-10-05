//! NFSv4 file state at the file's owner (§4.6 A-36): the opens of the files a shard owns, with their
//! share reservations, and their lock states (RFC 8881 §9). All of one file's state lives in one
//! place, the owner shard of its volume, so a share or lock check sees every open and lock of the file
//! whichever listener or connection made them; the listener keeps only the client's sessions and an
//! index of the state ids it holds ([`crate::v4::compound`]).
//!
//! The v4 front end reaches this state through the state extension procedures
//! (`crate::procedures::extension`), which route by the file handle as every v3 call does; each carries
//! the client id the listener's session names, which the owner checks every state id against.
//!
//! A state id's `other` is the owner's own name for the state: a 48-bit counter (its top bit marks a
//! lock state) followed by the owner's tag (its partition and boot instance), so ids minted by two
//! owners, or by one owner before and after a restart, never collide.

use crate::handle::identity;
use std::collections::BTreeMap;

use super::Nfsstat4;
use super::lock::{Denied, LockKind, LockTable, Other, Range};
use super::types::{OPAQUE_LIMIT, OTHER_SIZE, Stateid};
use crate::nfs::Nfsfh3;
use crate::xdr::{XdrReader, XdrWriter};

/// Format: `OPEN4_SHARE_ACCESS_*` and `OPEN4_SHARE_DENY_*` bits (RFC 8881 §18.16.1).
pub mod share {
  /// Format: read.
  pub const READ: u32 = 1;
  /// Format: write.
  pub const WRITE: u32 = 2;
  /// Format: both.
  pub const BOTH: u32 = 3;
}

/// Format: `OP_LOCK`, `OP_LOCKT` and `OP_LOCKU`, the operation a forwarded lock request is (RFC 7863).
pub mod lock_op {
  /// Format: `OP_LOCK`.
  pub const LOCK: u32 = 12;
  /// Format: `OP_LOCKT`.
  pub const LOCKT: u32 = 13;
  /// Format: `OP_LOCKU`.
  pub const LOCKU: u32 = 14;
}

/// An open's share: the access it holds and the access it denies to others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
  /// `OPEN4_SHARE_ACCESS_*`.
  pub access: u32,
  /// `OPEN4_SHARE_DENY_*`.
  pub deny: u32,
}

/// One open: the client and open-owner that hold it, the file it opened, the share it holds and its
/// state id's current seqid. An open-owner's opens of one file share one state id (§9.1.4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Open {
  clientid: u64,
  owner: Vec<u8>,
  fh: Nfsfh3,
  share: Share,
  seqid: u32,
}

/// The kind of I/O a state id is presented for (RFC 8881 §9.1.2): a READ, a write-type operation (a
/// WRITE, or a SETATTR that sets the size), or a SETATTR of other attributes, which needs no access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoWant {
  /// A READ.
  Read,
  /// A WRITE, or a SETATTR that sets the size.
  Write,
  /// A SETATTR that leaves the size.
  Attributes,
}

/// What authorizes an I/O beyond the caller's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoAuthority {
  /// The object's mode, checked against the caller: an NFSv3 call, which carries no open (the owner
  /// may write its own file whatever the bits say, standing in for the open the protocol cannot
  /// show), or an NFSv4 special state id.
  Mode,
  /// An NFSv4 open whose access mode allows the I/O: it checked the caller's permission when it was
  /// made, and a descriptor keeps its access whatever the mode becomes (POSIX).
  Open,
}

/// The key an open is found by from its file: the file first, so every open of one file is one range.
type OpenKey = (Vec<u8>, u64, Vec<u8>);

/// Format: the bytes of a state id's counter.
const COUNTER_BYTES: usize = 6;

/// Format: an owner's tag, the last six bytes of every state id it mints: its partition (2) and its
/// boot instance (4).
pub type OwnerTag = [u8; OTHER_SIZE - COUNTER_BYTES];
/// Format: the counter bit that marks a lock state id.
pub const LOCK_ID_BIT: u64 = 1 << 47;

/// The owner's tag, the last bytes of every state id it mints: its partition and its boot instance.
pub fn owner_tag(partition: u16, boot: u32) -> OwnerTag {
  let mut tag: OwnerTag = [0u8; OTHER_SIZE - COUNTER_BYTES];
  tag[..2].copy_from_slice(&partition.to_be_bytes());
  tag[2..].copy_from_slice(&boot.to_be_bytes());
  tag
}

/// The owner partition a state id's `other` names (the first two bytes of its owner's tag): where
/// an operation that names only the state id is routed (D-14, ids route to owners).
pub fn owner_of(other: &Other) -> u16 {
  u16::from_be_bytes([other[COUNTER_BYTES], other[COUNTER_BYTES + 1]])
}

/// Serves one of the id-only state procedures on `files` (A-36): TEST_STATEID and FREE_STATEID of a
/// state id, and a client's purge, each routed to the owner by the id itself or sent to every owner.
/// Arguments: the client id, then (TEST, FREE) the state id. Result: the status, then (FREE) the count
/// and each `other` the owner no longer holds.
pub fn serve_by_id(files: &mut FileState, procedure: u32, args: &mut XdrReader<'_>) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  match by_id(files, procedure, args) {
    Ok(body) => {
      writer.u32(Nfsstat4::Ok.wire());
      writer.fixed(&body);
    }
    Err(status) => writer.u32(status.wire()),
  }
  writer.into_bytes()
}

/// The body of an id-only state procedure, or its refusal.
fn by_id(
  files: &mut FileState,
  procedure: u32,
  args: &mut XdrReader<'_>,
) -> Result<Vec<u8>, Nfsstat4> {
  use crate::procedures::extension;
  let clientid = args.u64().map_err(|_| Nfsstat4::Badxdr)?;
  let mut body = XdrWriter::new();
  match procedure {
    extension::STATE_PURGE => files.purge(clientid),
    extension::STATE_TEST => {
      let stateid = Stateid::decode(args).map_err(|_| Nfsstat4::Badxdr)?;
      let status = files.test(&stateid.other, clientid);
      if status != Nfsstat4::Ok {
        return Err(status);
      }
    }
    extension::STATE_FREE => {
      let stateid = Stateid::decode(args).map_err(|_| Nfsstat4::Badxdr)?;
      let gone = files.free(&stateid.other, clientid)?.gone;
      body.u32(u32::try_from(gone.len()).unwrap_or(u32::MAX));
      for other in &gone {
        body.fixed(other);
      }
    }
    _ => return Err(Nfsstat4::Serverfault),
  }
  Ok(body.into_bytes())
}

/// A state id's `other` from a counter and the owner's tag.
pub fn mint(counter: u64, tag: OwnerTag) -> Other {
  let mut other = [0u8; OTHER_SIZE];
  other[..COUNTER_BYTES].copy_from_slice(&counter.to_be_bytes()[2..]);
  other[COUNTER_BYTES..].copy_from_slice(&tag);
  other
}

/// Why a lock request was refused: a conflict, which the reply describes, or a status.
#[derive(Debug, PartialEq, Eq)]
pub enum LockRefused {
  /// `NFS4ERR_DENIED` with the conflicting lock.
  Denied(Denied),
  /// Any other status.
  Status(Nfsstat4),
}

impl From<Nfsstat4> for LockRefused {
  fn from(status: Nfsstat4) -> LockRefused {
    LockRefused::Status(status)
  }
}

/// A decoded LOCK, LOCKT or LOCKU.
#[derive(Debug, PartialEq, Eq)]
pub enum LockRequest {
  /// LOCK by a lock-owner new to the open or by an existing lock state.
  Lock {
    /// The lock's kind.
    kind: LockKind,
    /// Its range.
    range: Range,
    /// Who locks.
    locker: Locker,
  },
  /// LOCKT for `owner`.
  Test {
    /// The kind tested.
    kind: LockKind,
    /// The range tested.
    range: Range,
    /// The lock-owner tested for.
    owner: Vec<u8>,
  },
  /// LOCKU of the lock state `stateid`.
  Unlock {
    /// The range unlocked.
    range: Range,
    /// The lock state.
    stateid: Stateid,
  },
}

/// A LOCK's `locker4`.
#[derive(Debug, PartialEq, Eq)]
pub enum Locker {
  /// The first lock of `owner` under the open `open` (the lock-owner's client id is the session's,
  /// §18.10.3, so the one on the wire is ignored, as are the seqids).
  New {
    /// The open.
    open: Stateid,
    /// The lock-owner.
    owner: Vec<u8>,
  },
  /// A further lock of an existing lock state.
  Existing {
    /// The lock state.
    stateid: Stateid,
  },
}

impl LockRequest {
  /// Reads the arguments of `opnum` (one of [`lock_op`]) as the client sent them. An undefined lock
  /// type is `NFS4ERR_BADXDR`; a zero length or an end past the largest offset is `NFS4ERR_INVAL`; a
  /// reclaim is `NFS4ERR_NO_GRACE` (no lock state survives a restart to be reclaimed, §8.4.2).
  pub fn decode(opnum: u32, reader: &mut XdrReader<'_>) -> Result<LockRequest, Nfsstat4> {
    let bad = |_| Nfsstat4::Badxdr;
    let kind = LockKind::from_wire(reader.u32().map_err(bad)?).ok_or(Nfsstat4::Badxdr)?;
    match opnum {
      lock_op::LOCK => {
        let reclaim = reader.bool().map_err(bad)?;
        let range = Self::range(reader)?;
        let locker = if reader.bool().map_err(bad)? {
          let _open_seqid = reader.u32().map_err(bad)?;
          let open = Stateid::decode(reader).map_err(bad)?;
          let _lock_seqid = reader.u32().map_err(bad)?;
          let _clientid = reader.u64().map_err(bad)?;
          let owner = reader.opaque(OPAQUE_LIMIT).map_err(bad)?.to_vec();
          Locker::New { open, owner }
        } else {
          let stateid = Stateid::decode(reader).map_err(bad)?;
          let _lock_seqid = reader.u32().map_err(bad)?;
          Locker::Existing { stateid }
        };
        if reclaim {
          return Err(Nfsstat4::NoGrace);
        }
        Ok(LockRequest::Lock {
          kind,
          range,
          locker,
        })
      }
      lock_op::LOCKT => {
        let range = Self::range(reader)?;
        let _clientid = reader.u64().map_err(bad)?;
        let owner = reader.opaque(OPAQUE_LIMIT).map_err(bad)?.to_vec();
        Ok(LockRequest::Test { kind, range, owner })
      }
      _ => {
        let _seqid = reader.u32().map_err(bad)?;
        let stateid = Stateid::decode(reader).map_err(bad)?;
        let range = Self::range(reader)?;
        Ok(LockRequest::Unlock { range, stateid })
      }
    }
  }

  /// An `offset4` and `length4`.
  fn range(reader: &mut XdrReader<'_>) -> Result<Range, Nfsstat4> {
    let offset = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
    let length = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
    Range::of(offset, length).ok_or(Nfsstat4::Inval)
  }
}

/// The seqid after `seqid`: it wraps from the largest back to 1, never to 0, which names "the current
/// one" (RFC 8881 §8.2.2).
fn next_seqid(seqid: u32) -> u32 {
  seqid.checked_add(1).unwrap_or(1)
}

/// The state ids a CLOSE or FREE_STATEID ended: the state named, and the lock states that went with an
/// open, so the listener's index forgets them too.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Released {
  /// The state id to answer with (a CLOSE's advanced one).
  pub stateid: Stateid,
  /// Every `other` the owner no longer holds.
  pub gone: Vec<Other>,
}

/// An open as a durable record carries it (§4.6 A-37).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRecord {
  /// The state id's `other`.
  pub other: Other,
  /// The client holding it.
  pub clientid: u64,
  /// The open-owner.
  pub owner: Vec<u8>,
  /// The file handle.
  pub fh: Vec<u8>,
  /// The share it holds.
  pub share: Share,
  /// The state id's current seqid.
  pub seqid: u32,
}

/// A lock state as a durable record carries it (§4.6 A-37).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockRecord {
  /// The state id's `other`.
  pub other: Other,
  /// The client holding it.
  pub clientid: u64,
  /// The lock-owner.
  pub owner: Vec<u8>,
  /// The file handle.
  pub fh: Vec<u8>,
  /// The open it was created from.
  pub open: Other,
  /// The state id's current seqid.
  pub seqid: u32,
  /// Its ranges, ascending.
  pub ranges: Vec<(Range, LockKind)>,
}

/// One change to the file state, as the owner's partition records it (§4.6 A-37).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChange {
  /// An open was recorded or changed.
  OpenSet(OpenRecord),
  /// An open was closed or freed.
  OpenCleared(Other),
  /// A lock state was recorded or changed.
  LockSet(LockRecord),
  /// A lock state was freed or went with its open.
  LockCleared(Other),
  /// Every open and lock state of a client was dropped.
  ClientCleared(u64),
  /// A delegation was granted or changed (A-78).
  DelegationSet(super::delegation::DelegationRecord),
  /// A delegation was returned or revoked.
  DelegationCleared(Other),
}

/// One owner shard's NFSv4 file state: its opens and lock states, bounded.
pub struct FileState {
  tag: OwnerTag,
  next: u64,
  max_opens: usize,
  opens: BTreeMap<Other, Open>,
  by_file: BTreeMap<OpenKey, Other>,
  locks: LockTable,
  /// The files' delegations (A-78).
  delegations: super::delegation::Delegations,
  /// Delegations revoked from their holders and not yet freed by them (RFC 8881 §10.4.5), with each holder: a holder
  /// with one is told on every SEQUENCE (`SEQ4_STATUS_RECALLABLE_STATE_REVOKED`) until it frees it. Never more than
  /// the delegation bound.
  revoked: BTreeMap<Other, u64>,
  /// How long after a recall a file is not delegated again (the lease; zero for a standalone server).
  quiet_ns: u64,
  /// Recalls begun by a conflicting open or read ([`FileState::check_open_conflicts`],
  /// [`FileState::check_read_conflicts`]) and not yet taken by the daemon to send. Each delegation's recall begins once, so this never holds more than the delegation bound.
  outbox: Vec<super::delegation::Recall>,
  /// The changes since the last [`FileState::take_changes`], for the owner's partition to record, when
  /// the state is durable (`None` for a standalone server, which records nothing). Drained after
  /// every operation, so it holds one operation's changes at most.
  changes: Option<Vec<FileChange>>,
}

impl FileState {
  /// Empty state for an owner named by `tag` ([`owner_tag`]), holding at most `max_opens` opens and
  /// `max_locks` lock ranges.
  pub fn new(tag: OwnerTag, max_opens: usize, max_locks: usize) -> FileState {
    FileState {
      tag,
      next: 1,
      max_opens,
      opens: BTreeMap::new(),
      by_file: BTreeMap::new(),
      locks: LockTable::new(tag, max_locks),
      delegations: super::delegation::Delegations::new(max_opens),
      revoked: BTreeMap::new(),
      quiet_ns: 0,
      outbox: Vec::new(),
      changes: None,
    }
  }

  /// Durable state (§4.6 A-37): an owner that records every change in its partition, rebuilt from the
  /// records `opens` and `locks` it kept (before a restart, or after a record that failed and rolled the
  /// partition back). Every kept record comes back, even past bounds derived smaller since: it was
  /// admitted, and only new state is refused until the tables are back under their bounds. New state
  /// ids carry the new `tag`, so they never collide with the kept ones.
  pub fn restore(
    tag: OwnerTag,
    max_opens: usize,
    max_locks: usize,
    opens: Vec<OpenRecord>,
    locks: Vec<LockRecord>,
    delegations: Vec<super::delegation::DelegationRecord>,
  ) -> FileState {
    let mut state = FileState::new(tag, max_opens, max_locks);
    for record in opens {
      let fh = Nfsfh3(record.fh);
      state.by_file.insert(
        (identity(&fh), record.clientid, record.owner.clone()),
        record.other,
      );
      state.opens.insert(
        record.other,
        Open {
          clientid: record.clientid,
          owner: record.owner,
          fh,
          share: record.share,
          seqid: record.seqid,
        },
      );
    }
    for record in locks {
      state.locks.restore_kept(
        record.other,
        super::lock::LockState {
          clientid: record.clientid,
          owner: record.owner,
          fh: Nfsfh3(record.fh),
          open: record.open,
          seqid: record.seqid,
          ranges: super::lock::OwnerRanges::from_ranges(&record.ranges),
        },
      );
    }
    for record in delegations {
      state.delegations.restore(record);
    }
    state.changes = Some(Vec::new());
    state
  }

  /// The changes since the last call, for the owner's partition to record (§4.6 A-37); none for a
  /// standalone server.
  pub fn take_changes(&mut self) -> Vec<FileChange> {
    self
      .changes
      .as_mut()
      .map(std::mem::take)
      .unwrap_or_default()
  }

  /// Journals the current record of the open `other`, or its clearing.
  fn journal_open(&mut self, other: &Other) {
    let Some(changes) = self.changes.as_mut() else {
      return;
    };
    changes.push(match self.opens.get(other) {
      Some(open) => FileChange::OpenSet(OpenRecord {
        other: *other,
        clientid: open.clientid,
        owner: open.owner.clone(),
        fh: open.fh.0.clone(),
        share: open.share,
        seqid: open.seqid,
      }),
      None => FileChange::OpenCleared(*other),
    });
  }

  /// Journals the current record of the lock state `other`, or its clearing.
  fn journal_lock(&mut self, other: &Other) {
    let Some(changes) = self.changes.as_mut() else {
      return;
    };
    changes.push(match self.locks.state(other) {
      Some(state) => FileChange::LockSet(LockRecord {
        other: *other,
        clientid: state.clientid,
        owner: state.owner.clone(),
        fh: state.fh.0.clone(),
        open: state.open,
        seqid: state.seqid,
        ranges: state.ranges.ranges().to_vec(),
      }),
      None => FileChange::LockCleared(*other),
    });
  }

  /// The state of a standalone server (the examples and tests): owner partition 0, boot 0.
  pub fn standalone() -> FileState {
    /// Shape: opens and lock ranges the standalone server records.
    const STANDALONE_STATES: usize = 4096;
    FileState::new(owner_tag(0, 0), STANDALONE_STATES, STANDALONE_STATES)
  }

  /// How many opens are recorded.
  pub fn open_count(&self) -> usize {
    self.opens.len()
  }

  /// How many lock states are recorded.
  pub fn lock_state_count(&self) -> usize {
    self.locks.state_count()
  }

  /// OPEN's state (§18.16): an open of `fh` by `owner` of `clientid` holding `share`. The owner's
  /// existing open of the file is upgraded (its share joined with the new one) and its state id
  /// advanced; a share that conflicts with another owner's open is `NFS4ERR_SHARE_DENIED`; a new open
  /// past the bound is `NFS4ERR_NOSPC` (OPEN's exhaustion status, RFC 8881 §15.2).
  pub fn open(
    &mut self,
    clientid: u64,
    owner: Vec<u8>,
    fh: &Nfsfh3,
    share: Share,
  ) -> Result<Stateid, Nfsstat4> {
    let file = identity(fh);
    let key: OpenKey = (file.clone(), clientid, owner);
    let conflicts = self
      .by_file
      .range((file.clone(), 0, Vec::new())..)
      .take_while(|((held, _, _), _)| *held == file)
      .filter(|(other_key, _)| **other_key != key)
      .filter_map(|(_, other)| self.opens.get(other))
      .any(|other| share.access & other.share.deny != 0 || share.deny & other.share.access != 0);
    if conflicts {
      return Err(Nfsstat4::ShareDenied);
    }
    if let Some(other) = self.by_file.get(&key).copied() {
      let open = self.opens.get_mut(&other).ok_or(Nfsstat4::Serverfault)?;
      open.share.access |= share.access;
      open.share.deny |= share.deny;
      open.seqid = next_seqid(open.seqid);
      let seqid = open.seqid;
      self.journal_open(&other);
      return Ok(Stateid { seqid, other });
    }
    if self.opens.len() >= self.max_opens {
      return Err(Nfsstat4::Nospc);
    }
    let other = mint(self.next, self.tag);
    self.next += 1;
    self.opens.insert(
      other,
      Open {
        clientid,
        owner: key.2.clone(),
        fh: fh.clone(),
        share,
        seqid: 1,
      },
    );
    self.by_file.insert(key, other);
    self.journal_open(&other);
    Ok(Stateid { seqid: 1, other })
  }

  /// Whether `stateid` names an open this owner recorded of `fh` for `clientid` at a current seqid (0
  /// is "the current one"; an earlier one is `NFS4ERR_OLD_STATEID`, anything else
  /// `NFS4ERR_BAD_STATEID`, §8.2.2).
  fn check_open(&self, stateid: &Stateid, fh: &Nfsfh3, clientid: u64) -> Result<&Open, Nfsstat4> {
    let open = self.opens.get(&stateid.other).ok_or(Nfsstat4::BadStateid)?;
    if identity(&open.fh) != identity(fh) || open.clientid != clientid {
      return Err(Nfsstat4::BadStateid);
    }
    match stateid.seqid {
      0 => Ok(open),
      seqid if seqid == open.seqid => Ok(open),
      seqid if seqid < open.seqid => Err(Nfsstat4::OldStateid),
      _ => Err(Nfsstat4::BadStateid),
    }
  }

  /// Whether `stateid` may serve I/O of kind `want` on `fh` for `clientid` (RFC 8881 §9.1.2), and what
  /// authorizes it:
  /// - an open or a lock state of that file for that client at a current seqid (§8.2.2), whose access
  ///   mode (a lock state's is its open's) must allow a write-type operation (`NFS4ERR_OPENMODE`); a
  ///   READ is allowed on a write-only open, as clients' write paths read, but not past another open's
  ///   DENY_READ. Such an open authorizes the I/O: it checked the caller's permission when it was made,
  ///   and a descriptor keeps its access whatever the mode becomes ([`IoAuthority::Open`]);
  /// - a special state id, which holds no share reservation, so every other open's deny applies
  ///   (`NFS4ERR_LOCKED`) and the object's mode decides ([`IoAuthority::Mode`]). The READ bypass id
  ///   bypasses byte-range locks, which are advisory here, and not share reservations.
  pub fn check_io(
    &self,
    stateid: &Stateid,
    fh: &Nfsfh3,
    clientid: u64,
    want: IoWant,
  ) -> Result<IoAuthority, Nfsstat4> {
    if stateid.is_special() {
      self.refuse_denied(fh, None, want)?;
      return Ok(IoAuthority::Mode);
    }
    // A delegation's state id serves its holder's I/O (§10.4.1): reads under a read delegation, and writes only under
    // a write delegation.
    if self.delegations.holder(&stateid.other).is_some() {
      let write = self.delegations.check(stateid, clientid, fh)?;
      return match want {
        IoWant::Write if !write => Err(Nfsstat4::Openmode),
        IoWant::Read | IoWant::Write => Ok(IoAuthority::Open),
        IoWant::Attributes => Ok(IoAuthority::Mode),
      };
    }
    let (open_other, open) = if self.locks.contains(&stateid.other) {
      let lock = self.locks.get(stateid, fh, Some(clientid))?;
      let open = self.opens.get(&lock.open).ok_or(Nfsstat4::BadStateid)?;
      (lock.open, open)
    } else {
      (stateid.other, self.check_open(stateid, fh, clientid)?)
    };
    match want {
      IoWant::Write if open.share.access & share::WRITE == 0 => Err(Nfsstat4::Openmode),
      IoWant::Read if open.share.access & share::READ == 0 => {
        self.refuse_denied(fh, Some(&open_other), want)?;
        Ok(IoAuthority::Open)
      }
      IoWant::Read | IoWant::Write => Ok(IoAuthority::Open),
      IoWant::Attributes => Ok(IoAuthority::Mode),
    }
  }

  /// `NFS4ERR_LOCKED` when an open of `fh` other than `except` denies the access `want` asks for.
  fn refuse_denied(
    &self,
    fh: &Nfsfh3,
    except: Option<&Other>,
    want: IoWant,
  ) -> Result<(), Nfsstat4> {
    let denied = match want {
      IoWant::Read => share::READ,
      IoWant::Write => share::WRITE,
      IoWant::Attributes => return Ok(()),
    };
    let file = identity(fh);
    let refused = self
      .by_file
      .range((file.clone(), 0, Vec::new())..)
      .take_while(|((held, _, _), _)| *held == file)
      .filter(|(_, other)| Some(*other) != except)
      .filter_map(|(_, other)| self.opens.get(other))
      .any(|open| open.share.deny & denied != 0);
    if refused {
      Err(Nfsstat4::Locked)
    } else {
      Ok(())
    }
  }

  /// CLOSE (§18.2): the open's state released, with its lock states holding no lock;
  /// `NFS4ERR_LOCKS_HELD` while a lock-owner under it holds one.
  pub fn close(
    &mut self,
    stateid: &Stateid,
    fh: &Nfsfh3,
    clientid: u64,
  ) -> Result<Released, Nfsstat4> {
    if stateid.is_special() {
      return Err(Nfsstat4::BadStateid);
    }
    let seqid = next_seqid(self.check_open(stateid, fh, clientid)?.seqid);
    if self.locks.held_under(&stateid.other) {
      return Err(Nfsstat4::LocksHeld);
    }
    let mut gone = self.locks.drop_under(&stateid.other);
    self.remove_open(&stateid.other);
    self.journal_gone(&gone, &stateid.other);
    gone.push(stateid.other);
    Ok(Released {
      stateid: Stateid {
        seqid,
        other: stateid.other,
      },
      gone,
    })
  }

  /// OPEN_DOWNGRADE (§18.18): the open's share narrowed to a subset of what it holds (anything else is
  /// `NFS4ERR_INVAL`); its state id advances.
  pub fn downgrade(
    &mut self,
    stateid: &Stateid,
    fh: &Nfsfh3,
    clientid: u64,
    share: Share,
  ) -> Result<Stateid, Nfsstat4> {
    if stateid.is_special() {
      return Err(Nfsstat4::BadStateid);
    }
    self.check_open(stateid, fh, clientid)?;
    let open = self
      .opens
      .get_mut(&stateid.other)
      .ok_or(Nfsstat4::BadStateid)?;
    if share.access == 0
      || share.access & !open.share.access != 0
      || share.deny & !open.share.deny != 0
    {
      return Err(Nfsstat4::Inval);
    }
    open.share = share;
    open.seqid = next_seqid(open.seqid);
    let seqid = open.seqid;
    self.journal_open(&stateid.other);
    Ok(Stateid {
      seqid,
      other: stateid.other,
    })
  }

  /// A LOCK, LOCKT or LOCKU on `fh` for `clientid`: the state id to answer with (none for LOCKT), or
  /// why it was refused.
  pub fn lock(
    &mut self,
    request: LockRequest,
    fh: &Nfsfh3,
    clientid: u64,
  ) -> Result<Option<Stateid>, LockRefused> {
    match request {
      LockRequest::Lock {
        kind,
        range,
        locker,
      } => {
        let other = self.lock_state(locker, fh, clientid, kind)?;
        let state = self
          .locks
          .state(&other)
          .ok_or(LockRefused::Status(Nfsstat4::BadStateid))?;
        let owner = state.owner.clone();
        let next = state.ranges.locked(range, kind);
        if let Some(denied) = self.locks.conflict(fh, clientid, &owner, range, kind) {
          // A lock state the request created stays (its owner may lock again), so it is recorded.
          self.journal_lock(&other);
          return Err(LockRefused::Denied(denied));
        }
        let stateid = self.locks.set_ranges(&other, next)?;
        self.journal_lock(&other);
        Ok(Some(stateid))
      }
      LockRequest::Test { kind, range, owner } => {
        match self.locks.conflict(fh, clientid, &owner, range, kind) {
          Some(denied) => Err(LockRefused::Denied(denied)),
          None => Ok(None),
        }
      }
      LockRequest::Unlock { range, stateid } => {
        if stateid.is_special() {
          return Err(LockRefused::Status(Nfsstat4::BadStateid));
        }
        let next = self
          .locks
          .get(&stateid, fh, Some(clientid))?
          .ranges
          .unlocked(range);
        let advanced = self.locks.set_ranges(&stateid.other, next)?;
        self.journal_lock(&stateid.other);
        Ok(Some(advanced))
      }
    }
  }

  /// The lock state a LOCK runs under: an existing one named by its state id, or the lock-owner's on
  /// this file created from the open. The open must allow the lock (§18.10.4, POSIX): a write lock
  /// needs the file open for writing, a read lock open for reading (`NFS4ERR_OPENMODE`).
  fn lock_state(
    &mut self,
    locker: Locker,
    fh: &Nfsfh3,
    clientid: u64,
    kind: LockKind,
  ) -> Result<Other, Nfsstat4> {
    let (open_other, lock_other) = match locker {
      Locker::New { open, owner } => {
        if open.is_special() {
          return Err(Nfsstat4::BadStateid);
        }
        self.check_open(&open, fh, clientid)?;
        let other = self.locks.state_for(clientid, owner, fh, open.other)?;
        (open.other, other)
      }
      Locker::Existing { stateid } => {
        if stateid.is_special() {
          return Err(Nfsstat4::BadStateid);
        }
        let state = self.locks.get(&stateid, fh, Some(clientid))?;
        (state.open, stateid.other)
      }
    };
    let access = self
      .opens
      .get(&open_other)
      .map(|open| open.share.access)
      .ok_or(Nfsstat4::BadStateid)?;
    let needed = match kind {
      LockKind::Read => share::READ,
      LockKind::Write => share::WRITE,
    };
    if access & needed == 0 {
      return Err(Nfsstat4::Openmode);
    }
    Ok(lock_other)
  }

  /// TEST_STATEID's answer for one state id of `clientid` (§18.48): valid, or `NFS4ERR_BAD_STATEID`.
  pub fn test(&self, other: &Other, clientid: u64) -> Nfsstat4 {
    if self.revoked.get(other) == Some(&clientid) {
      return Nfsstat4::DelegRevoked;
    }
    let held = self
      .opens
      .get(other)
      .map(|open| open.clientid)
      .or_else(|| self.locks.state(other).map(|state| state.clientid))
      .or_else(|| self.delegations.holder(other));
    if held == Some(clientid) {
      Nfsstat4::Ok
    } else {
      Nfsstat4::BadStateid
    }
  }

  /// FREE_STATEID (§18.38): a lock state holding no lock, or an open none of whose lock-owners holds
  /// one (with its lock states); `NFS4ERR_LOCKS_HELD` otherwise.
  pub fn free(&mut self, other: &Other, clientid: u64) -> Result<Released, Nfsstat4> {
    // A revoked delegation is freed by its holder once it has seen the revocation (§18.38).
    if self.revoked.get(other) == Some(&clientid) {
      self.revoked.remove(other);
      return Ok(Released {
        stateid: Stateid::default(),
        gone: vec![*other],
      });
    }
    if self.test(other, clientid) != Nfsstat4::Ok {
      return Err(Nfsstat4::BadStateid);
    }
    if self.locks.contains(other) {
      self.locks.free(other)?;
      self.journal_lock(other);
      return Ok(Released {
        stateid: Stateid::default(),
        gone: vec![*other],
      });
    }
    if self.locks.held_under(other) {
      return Err(Nfsstat4::LocksHeld);
    }
    let mut gone = self.locks.drop_under(other);
    self.remove_open(other);
    self.journal_gone(&gone, other);
    gone.push(*other);
    Ok(Released {
      stateid: Stateid::default(),
      gone,
    })
  }

  /// Drops every open and lock state of `clientid` (its lease lapsed, it rebooted, or it was
  /// destroyed).
  pub fn purge(&mut self, clientid: u64) {
    let opens: Vec<Other> = self
      .opens
      .iter()
      .filter(|(_, open)| open.clientid == clientid)
      .map(|(other, _)| *other)
      .collect();
    for other in &opens {
      self.remove_open(other);
    }
    self.locks.purge(|holder| holder != clientid);
    // The partition's client purge clears its delegation records with its opens and locks.
    self.delegations.purge(clientid);
    self.revoked.retain(|_, holder| *holder != clientid);
    if let Some(changes) = self.changes.as_mut() {
      changes.push(FileChange::ClientCleared(clientid));
    }
  }

  /// A read delegation of `fh` for `clientid` when one is due (RFC 8881 §10.4; A-78): never while another client
  /// holds the file open for writing or denying reads, nor when [`super::delegation::Delegations::grant_read`]
  /// refuses (another's write delegation, a recent recall, the bound). Journaled when granted.
  pub fn delegate_read(
    &mut self,
    clientid: u64,
    fh: &Nfsfh3,
    now_ns: u64,
    quiet_ns: u64,
  ) -> Option<Stateid> {
    let file = identity(fh);
    let contended = self
      .by_file
      .range((file.clone(), 0, Vec::new())..)
      .take_while(|((held, _, _), _)| *held == file)
      .filter_map(|(_, other)| self.opens.get(other))
      .any(|open| {
        open.clientid != clientid
          && (open.share.access & share::WRITE != 0 || open.share.deny & share::READ != 0)
      });
    if contended {
      return None;
    }
    let other = mint(self.next, self.tag);
    let granted = self
      .delegations
      .grant_read((clientid, fh), other, now_ns, quiet_ns)?;
    if granted.other == other {
      self.next += 1;
      self.journal_delegation(&other);
    }
    Some(granted)
  }

  /// A write delegation of `fh` for `clientid` when one is due (RFC 8881 §10.4; A-80): never while another client
  /// holds the file open, nor when [`super::delegation::Delegations::grant_write`] refuses (another's delegation, a
  /// recent recall, the bound). Journaled when granted.
  pub fn delegate_write(
    &mut self,
    clientid: u64,
    fh: &Nfsfh3,
    now_ns: u64,
    quiet_ns: u64,
  ) -> Option<Stateid> {
    let file = identity(fh);
    let opened_elsewhere = self
      .by_file
      .range((file.clone(), 0, Vec::new())..)
      .take_while(|((held, _, _), _)| *held == file)
      .any(|((_, holder, _), _)| *holder != clientid);
    if opened_elsewhere {
      return None;
    }
    let other = mint(self.next, self.tag);
    let granted = self
      .delegations
      .grant_write((clientid, fh), other, now_ns, quiet_ns)?;
    if granted.other == other {
      self.next += 1;
      self.journal_delegation(&other);
    }
    Some(granted)
  }

  /// Whether an open of `fh` by `clientid` with `share` may proceed now (RFC 8881 §10.4.4: a conflicting open is a
  /// recall event). `NFS4ERR_DELAY` while another holder's delegation the open conflicts with
  /// ([`super::delegation::Conflict::Open`]) is outstanding; the recalls that begin now wait in the outbox for the
  /// daemon ([`FileState::take_recalls`]).
  pub fn check_open_conflicts(
    &mut self,
    clientid: u64,
    fh: &Nfsfh3,
    share: Share,
    now_ns: u64,
  ) -> Result<(), Nfsstat4> {
    let conflict = super::delegation::Conflict::Open {
      access: share.access,
      deny: share.deny,
    };
    let plan = self
      .delegations
      .recall(fh, Some(clientid), conflict, now_ns);
    self.outbox.extend(plan.send);
    if plan.waiting {
      return Err(Nfsstat4::Delay);
    }
    Ok(())
  }

  /// Whether a read of `fh` by `clientid` (its `GETATTR`) must wait for another holder's write delegation (RFC 8881
  /// §10.4.3: the holder may have changed the file, and the server "MAY simply recall the delegation" rather than ask
  /// it with `CB_GETATTR`; A-80). The recalls that begin now wait in the outbox, as a conflicting open's do.
  pub fn check_read_conflicts(&mut self, fh: &Nfsfh3, clientid: u64, now_ns: u64) -> bool {
    let plan = self.delegations.recall(
      fh,
      Some(clientid),
      super::delegation::Conflict::Read,
      now_ns,
    );
    self.outbox.extend(plan.send);
    plan.waiting
  }

  /// The recalls conflicting opens began since the last call, for the daemon to send.
  pub fn take_recalls(&mut self) -> Vec<super::delegation::Recall> {
    std::mem::take(&mut self.outbox)
  }

  /// `DELEGRETURN` (§18.6) of `stateid` by `clientid` on `fh`; journaled.
  pub fn return_delegation(
    &mut self,
    stateid: &Stateid,
    clientid: u64,
    fh: &Nfsfh3,
  ) -> Result<(), Nfsstat4> {
    self.delegations.give_back(stateid, clientid, fh)?;
    self.journal_delegation(&stateid.other);
    Ok(())
  }

  /// The delegations an operation by `actor` on `fh` must wait for, and the recalls to send now
  /// ([`super::delegation::Delegations::recall`]).
  pub fn recall(
    &mut self,
    fh: &Nfsfh3,
    actor: Option<u64>,
    conflict: super::delegation::Conflict,
    now_ns: u64,
  ) -> super::delegation::RecallPlan {
    self.delegations.recall(fh, actor, conflict, now_ns)
  }

  /// Revokes every delegation not returned within `lease_ns` of its recall; journaled. Their `other`s and holders.
  pub fn revoke_lapsed(&mut self, now_ns: u64, lease_ns: u64, quiet_ns: u64) -> Vec<(Other, u64)> {
    let revoked = self.delegations.revoke_lapsed(now_ns, lease_ns, quiet_ns);
    for (other, holder) in &revoked {
      self.remember_revoked(*other, *holder);
      self.journal_delegation(other);
    }
    revoked
  }

  /// How many delegations are held.
  pub fn delegation_count(&self) -> usize {
    self.delegations.len()
  }

  /// Sets how long after a recall a file is not delegated again (the owner's lease).
  pub fn set_delegation_quiet(&mut self, quiet_ns: u64) {
    self.quiet_ns = quiet_ns;
  }

  /// How long after a recall a file is not delegated again.
  pub fn delegation_quiet(&self) -> u64 {
    self.quiet_ns
  }

  /// The holders of the delegations of the file whose inode is `inode`, for the store's recall gate.
  pub fn holders_of_inode(&self, inode: u64) -> std::collections::BTreeSet<u64> {
    self.delegations.holders_of_inode(inode)
  }

  /// The inodes whose delegation holders changed since the last call, for the store's recall gate to update.
  pub fn take_changed_inodes(&mut self) -> Vec<u64> {
    self.delegations.take_changed_inodes()
  }

  /// The holders of each delegated file by its inode, for the store's recall gate.
  pub fn delegated_holders(
    &self,
  ) -> std::collections::BTreeMap<u64, std::collections::BTreeSet<u64>> {
    self.delegations.holders_by_inode()
  }

  /// The recall a change to `inode` by `actor` (the NFSv4 client acting, `None` for any other path) asked for: every
  /// other holder's delegation of that file, and the recalls to send now.
  pub fn recall_inode(
    &mut self,
    inode: u64,
    actor: Option<u64>,
    now_ns: u64,
  ) -> super::delegation::RecallPlan {
    match self.delegations.handle_of_inode(inode) {
      Some(fh) => self
        .delegations
        .recall(&fh, actor, super::delegation::Conflict::Change, now_ns),
      None => super::delegation::RecallPlan::default(),
    }
  }

  /// Revokes delegation `other` now (its recall could not be sent); journaled, and remembered for its holder.
  pub fn revoke_delegation(&mut self, other: &Other) {
    if let Some(holder) = self.delegations.revoke(other) {
      self.remember_revoked(*other, holder);
      self.journal_delegation(other);
    }
  }

  /// Whether `clientid` has a revoked delegation it has not freed (`SEQ4_STATUS_RECALLABLE_STATE_REVOKED`).
  pub fn has_revoked(&self, clientid: u64) -> bool {
    self.revoked.values().any(|holder| *holder == clientid)
  }

  fn remember_revoked(&mut self, other: Other, holder: u64) {
    if self.revoked.len() < self.max_opens {
      self.revoked.insert(other, holder);
    }
  }

  /// Journals the current record of delegation `other`, or its clearing.
  fn journal_delegation(&mut self, other: &Other) {
    let record = self.delegations.record(other);
    if let Some(changes) = self.changes.as_mut() {
      changes.push(match record {
        Some(record) => FileChange::DelegationSet(record),
        None => FileChange::DelegationCleared(*other),
      });
    }
  }

  /// Journals the clearing of an open and the lock states that went with it.
  fn journal_gone(&mut self, locks: &[Other], open: &Other) {
    for other in locks {
      self.journal_lock(other);
    }
    self.journal_open(open);
  }

  fn remove_open(&mut self, other: &Other) {
    if let Some(open) = self.opens.remove(other) {
      self
        .by_file
        .remove(&(identity(&open.fh), open.clientid, open.owner));
    }
  }
}

/// A state id's encoding.
pub fn encode_stateid(stateid: &Stateid) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  stateid.encode(&mut writer);
  writer.into_bytes()
}
