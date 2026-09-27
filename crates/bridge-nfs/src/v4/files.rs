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

/// One owner shard's NFSv4 file state: its opens and lock states, bounded.
pub struct FileState {
  tag: OwnerTag,
  next: u64,
  max_opens: usize,
  opens: BTreeMap<Other, Open>,
  by_file: BTreeMap<OpenKey, Other>,
  locks: LockTable,
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
    }
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
    let key: OpenKey = (fh.0.clone(), clientid, owner);
    let conflicts = self
      .by_file
      .range((fh.0.clone(), 0, Vec::new())..)
      .take_while(|((file, _, _), _)| *file == fh.0)
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
      return Ok(Stateid {
        seqid: open.seqid,
        other,
      });
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
    Ok(Stateid { seqid: 1, other })
  }

  /// Whether `stateid` names an open this owner recorded of `fh` for `clientid` at a current seqid (0
  /// is "the current one"; an earlier one is `NFS4ERR_OLD_STATEID`, anything else
  /// `NFS4ERR_BAD_STATEID`, §8.2.2).
  fn check_open(&self, stateid: &Stateid, fh: &Nfsfh3, clientid: u64) -> Result<&Open, Nfsstat4> {
    let open = self.opens.get(&stateid.other).ok_or(Nfsstat4::BadStateid)?;
    if open.fh != *fh || open.clientid != clientid {
      return Err(Nfsstat4::BadStateid);
    }
    match stateid.seqid {
      0 => Ok(open),
      seqid if seqid == open.seqid => Ok(open),
      seqid if seqid < open.seqid => Err(Nfsstat4::OldStateid),
      _ => Err(Nfsstat4::BadStateid),
    }
  }

  /// Whether `stateid` may serve I/O on `fh` for `clientid`: a special state id, or an open or a lock
  /// state of that file for that client at a current seqid (§8.2.2, §9.1.4).
  pub fn check_io(&self, stateid: &Stateid, fh: &Nfsfh3, clientid: u64) -> Result<(), Nfsstat4> {
    if stateid.is_special() {
      return Ok(());
    }
    if self.locks.contains(&stateid.other) {
      return self.locks.get(stateid, fh, Some(clientid)).map(|_| ());
    }
    self.check_open(stateid, fh, clientid).map(|_| ())
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
    Ok(Stateid {
      seqid: open.seqid,
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
          return Err(LockRefused::Denied(denied));
        }
        Ok(Some(self.locks.set_ranges(&other, next)?))
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
        Ok(Some(self.locks.set_ranges(&stateid.other, next)?))
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
    let held = self
      .opens
      .get(other)
      .map(|open| open.clientid)
      .or_else(|| self.locks.state(other).map(|state| state.clientid));
    if held == Some(clientid) {
      Nfsstat4::Ok
    } else {
      Nfsstat4::BadStateid
    }
  }

  /// FREE_STATEID (§18.38): a lock state holding no lock, or an open none of whose lock-owners holds
  /// one (with its lock states); `NFS4ERR_LOCKS_HELD` otherwise.
  pub fn free(&mut self, other: &Other, clientid: u64) -> Result<Released, Nfsstat4> {
    if self.test(other, clientid) != Nfsstat4::Ok {
      return Err(Nfsstat4::BadStateid);
    }
    if self.locks.contains(other) {
      self.locks.free(other)?;
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
  }

  fn remove_open(&mut self, other: &Other) {
    if let Some(open) = self.opens.remove(other) {
      self.by_file.remove(&(open.fh.0, open.clientid, open.owner));
    }
  }
}

/// A state id's encoding.
pub fn encode_stateid(stateid: &Stateid) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  stateid.encode(&mut writer);
  writer.into_bytes()
}
