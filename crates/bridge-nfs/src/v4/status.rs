//! The NFSv4 status codes slates returns (RFC 7863 `nfsstat4`), and their mapping from the NFSv3
//! statuses the semantic layer answers with (the two protocols share the POSIX values; the
//! v4-specific ones are v4's own).

use crate::nfs::Nfsstat3;

/// An NFSv4 status (RFC 7863 `nfsstat4`). The wire value is the discriminant.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nfsstat4 {
  /// Format: `NFS4_OK` (RFC 7863 `nfsstat4`).
  Ok = 0,
  /// Format: `NFS4ERR_PERM` (RFC 7863 `nfsstat4`).
  Perm = 1,
  /// Format: `NFS4ERR_NOENT` (RFC 7863 `nfsstat4`).
  Noent = 2,
  /// Format: `NFS4ERR_IO` (RFC 7863 `nfsstat4`).
  Io = 5,
  /// Format: `NFS4ERR_NXIO` (RFC 7863 `nfsstat4`).
  Nxio = 6,
  /// Format: `NFS4ERR_ACCESS` (RFC 7863 `nfsstat4`).
  Access = 13,
  /// Format: `NFS4ERR_EXIST` (RFC 7863 `nfsstat4`).
  Exist = 17,
  /// Format: `NFS4ERR_XDEV` (RFC 7863 `nfsstat4`).
  Xdev = 18,
  /// Format: `NFS4ERR_NOTDIR` (RFC 7863 `nfsstat4`).
  Notdir = 20,
  /// Format: `NFS4ERR_ISDIR` (RFC 7863 `nfsstat4`).
  Isdir = 21,
  /// Format: `NFS4ERR_INVAL` (RFC 7863 `nfsstat4`).
  Inval = 22,
  /// Format: `NFS4ERR_FBIG` (RFC 7863 `nfsstat4`).
  Fbig = 27,
  /// Format: `NFS4ERR_NOSPC` (RFC 7863 `nfsstat4`).
  Nospc = 28,
  /// Format: `NFS4ERR_ROFS` (RFC 7863 `nfsstat4`).
  Rofs = 30,
  /// Format: `NFS4ERR_MLINK` (RFC 7863 `nfsstat4`).
  Mlink = 31,
  /// Format: `NFS4ERR_NAMETOOLONG` (RFC 7863 `nfsstat4`).
  Nametoolong = 63,
  /// Format: `NFS4ERR_NOTEMPTY` (RFC 7863 `nfsstat4`).
  Notempty = 66,
  /// Format: `NFS4ERR_DQUOT` (RFC 7863 `nfsstat4`).
  Dquot = 69,
  /// Format: `NFS4ERR_STALE` (RFC 7863 `nfsstat4`).
  Stale = 70,
  /// Format: `NFS4ERR_BADHANDLE` (RFC 7863 `nfsstat4`).
  Badhandle = 10001,
  /// Format: `NFS4ERR_BAD_COOKIE` (RFC 7863 `nfsstat4`).
  BadCookie = 10003,
  /// Format: `NFS4ERR_NOTSUPP` (RFC 7863 `nfsstat4`).
  Notsupp = 10004,
  /// Format: `NFS4ERR_TOOSMALL` (RFC 7863 `nfsstat4`).
  Toosmall = 10005,
  /// Format: `NFS4ERR_SERVERFAULT` (RFC 7863 `nfsstat4`).
  Serverfault = 10006,
  /// Format: `NFS4ERR_BADTYPE` (RFC 7863 `nfsstat4`).
  Badtype = 10007,
  /// Format: `NFS4ERR_DELAY` (RFC 7863 `nfsstat4`).
  Delay = 10008,
  /// Format: `NFS4ERR_SAME` (RFC 7863 `nfsstat4`).
  Same = 10009,
  /// Format: `NFS4ERR_DENIED` (RFC 7863 `nfsstat4`).
  Denied = 10010,
  /// Format: `NFS4ERR_EXPIRED` (RFC 7863 `nfsstat4`).
  Expired = 10011,
  /// Format: `NFS4ERR_LOCKED` (RFC 7863 `nfsstat4`).
  Locked = 10012,
  /// Format: `NFS4ERR_GRACE` (RFC 7863 `nfsstat4`).
  Grace = 10013,
  /// Format: `NFS4ERR_SHARE_DENIED` (RFC 7863 `nfsstat4`).
  ShareDenied = 10015,
  /// Format: `NFS4ERR_WRONGSEC` (RFC 7863 `nfsstat4`).
  Wrongsec = 10016,
  /// Format: `NFS4ERR_CLID_INUSE` (RFC 7863 `nfsstat4`).
  ClidInuse = 10017,
  /// Format: `NFS4ERR_RESOURCE` (RFC 7863 `nfsstat4`).
  Resource = 10018,
  /// Format: `NFS4ERR_NOFILEHANDLE` (RFC 7863 `nfsstat4`).
  Nofilehandle = 10020,
  /// Format: `NFS4ERR_MINOR_VERS_MISMATCH` (RFC 7863 `nfsstat4`).
  MinorVersMismatch = 10021,
  /// Format: `NFS4ERR_STALE_CLIENTID` (RFC 7863 `nfsstat4`).
  StaleClientid = 10022,
  /// Format: `NFS4ERR_STALE_STATEID` (RFC 7863 `nfsstat4`).
  StaleStateid = 10023,
  /// Format: `NFS4ERR_OLD_STATEID` (RFC 7863 `nfsstat4`).
  OldStateid = 10024,
  /// Format: `NFS4ERR_BAD_STATEID` (RFC 7863 `nfsstat4`).
  BadStateid = 10025,
  /// Format: `NFS4ERR_BAD_SEQID` (RFC 7863 `nfsstat4`).
  BadSeqid = 10026,
  /// Format: `NFS4ERR_NOT_SAME` (RFC 7863 `nfsstat4`).
  NotSame = 10027,
  /// Format: `NFS4ERR_LOCK_RANGE` (RFC 7863 `nfsstat4`).
  LockRange = 10028,
  /// Format: `NFS4ERR_SYMLINK` (RFC 7863 `nfsstat4`).
  Symlink = 10029,
  /// Format: `NFS4ERR_RESTOREFH` (RFC 7863 `nfsstat4`).
  Restorefh = 10030,
  /// Format: `NFS4ERR_ATTRNOTSUPP` (RFC 7863 `nfsstat4`).
  Attrnotsupp = 10032,
  /// Format: `NFS4ERR_NO_GRACE` (RFC 7863 `nfsstat4`).
  NoGrace = 10033,
  /// Format: `NFS4ERR_BADXDR` (RFC 7863 `nfsstat4`).
  Badxdr = 10036,
  /// Format: `NFS4ERR_LOCKS_HELD` (RFC 7863 `nfsstat4`).
  LocksHeld = 10037,
  /// Format: `NFS4ERR_OPENMODE` (RFC 7863 `nfsstat4`).
  Openmode = 10038,
  /// Format: `NFS4ERR_BADOWNER` (RFC 7863 `nfsstat4`).
  Badowner = 10039,
  /// Format: `NFS4ERR_BADCHAR` (RFC 7863 `nfsstat4`).
  Badchar = 10040,
  /// Format: `NFS4ERR_BADNAME` (RFC 7863 `nfsstat4`).
  Badname = 10041,
  /// Format: `NFS4ERR_BAD_RANGE` (RFC 7863 `nfsstat4`).
  BadRange = 10042,
  /// Format: `NFS4ERR_LOCK_NOTSUPP` (RFC 7863 `nfsstat4`).
  LockNotsupp = 10043,
  /// Format: `NFS4ERR_OP_ILLEGAL` (RFC 7863 `nfsstat4`).
  OpIllegal = 10044,
  /// Format: `NFS4ERR_DEADLOCK` (RFC 7863 `nfsstat4`).
  Deadlock = 10045,
  /// Format: `NFS4ERR_FILE_OPEN` (RFC 7863 `nfsstat4`).
  FileOpen = 10046,
  /// Format: `NFS4ERR_BADSESSION` (RFC 7863 `nfsstat4`).
  Badsession = 10052,
  /// Format: `NFS4ERR_BADSLOT` (RFC 7863 `nfsstat4`).
  Badslot = 10053,
  /// Format: `NFS4ERR_COMPLETE_ALREADY` (RFC 7863 `nfsstat4`).
  CompleteAlready = 10054,
  /// Format: `NFS4ERR_CONN_NOT_BOUND_TO_SESSION` (RFC 7863 `nfsstat4`).
  ConnNotBoundToSession = 10055,
  /// Format: `NFS4ERR_SEQ_MISORDERED` (RFC 7863 `nfsstat4`).
  SeqMisordered = 10063,
  /// Format: `NFS4ERR_SEQUENCE_POS` (RFC 7863 `nfsstat4`).
  SequencePos = 10064,
  /// Format: `NFS4ERR_REQ_TOO_BIG` (RFC 7863 `nfsstat4`).
  ReqTooBig = 10065,
  /// Format: `NFS4ERR_REP_TOO_BIG` (RFC 7863 `nfsstat4`).
  RepTooBig = 10066,
  /// Format: `NFS4ERR_REP_TOO_BIG_TO_CACHE` (RFC 7863 `nfsstat4`).
  RepTooBigToCache = 10067,
  /// Format: `NFS4ERR_RETRY_UNCACHED_REP` (RFC 7863 `nfsstat4`).
  RetryUncachedRep = 10068,
  /// Format: `NFS4ERR_TOO_MANY_OPS` (RFC 7863 `nfsstat4`).
  TooManyOps = 10070,
  /// Format: `NFS4ERR_OP_NOT_IN_SESSION` (RFC 7863 `nfsstat4`).
  OpNotInSession = 10071,
  /// Format: `NFS4ERR_CLIENTID_BUSY` (RFC 7863 `nfsstat4`).
  ClientidBusy = 10074,
  /// Format: `NFS4ERR_SEQ_FALSE_RETRY` (RFC 7863 `nfsstat4`).
  SeqFalseRetry = 10076,
  /// Format: `NFS4ERR_BAD_HIGH_SLOT` (RFC 7863 `nfsstat4`).
  BadHighSlot = 10077,
  /// Format: `NFS4ERR_DEADSESSION` (RFC 7863 `nfsstat4`).
  Deadsession = 10078,
  /// Format: `NFS4ERR_NOT_ONLY_OP` (RFC 7863 `nfsstat4`).
  NotOnlyOp = 10081,
  /// Format: `NFS4ERR_WRONG_CRED` (RFC 7863 `nfsstat4`).
  WrongCred = 10082,
  /// Format: `NFS4ERR_WRONG_TYPE` (RFC 7863 `nfsstat4`).
  WrongType = 10083,
  /// Format: `NFS4ERR_UNION_NOTSUPP` (RFC 7863 `nfsstat4`).
  UnionNotsupp = 10090,
}

impl Nfsstat4 {
  /// The wire value.
  pub const fn wire(self) -> u32 {
    self as u32
  }

  /// The v4 status for a v3 status the semantic layer answered with: the POSIX values carry over; a
  /// v3-only one maps to its v4 meaning.
  pub fn of_v3(status: Nfsstat3) -> Nfsstat4 {
    match status {
      Nfsstat3::Ok => Nfsstat4::Ok,
      Nfsstat3::Perm => Nfsstat4::Perm,
      Nfsstat3::Noent => Nfsstat4::Noent,
      Nfsstat3::Io => Nfsstat4::Io,
      Nfsstat3::Acces => Nfsstat4::Access,
      Nfsstat3::Exist => Nfsstat4::Exist,
      Nfsstat3::Notdir => Nfsstat4::Notdir,
      Nfsstat3::Isdir => Nfsstat4::Isdir,
      Nfsstat3::Inval => Nfsstat4::Inval,
      Nfsstat3::Nospc => Nfsstat4::Nospc,
      Nfsstat3::Rofs => Nfsstat4::Rofs,
      Nfsstat3::Nametoolong => Nfsstat4::Nametoolong,
      Nfsstat3::Notempty => Nfsstat4::Notempty,
      Nfsstat3::Stale => Nfsstat4::Stale,
      Nfsstat3::Badhandle => Nfsstat4::Badhandle,
      // A v3 write verifier mismatch has no v4 counterpart: the v4 WRITE carries its own.
      Nfsstat3::NotSync => Nfsstat4::Inval,
      Nfsstat3::BadCookie => Nfsstat4::BadCookie,
      Nfsstat3::Notsupp => Nfsstat4::Notsupp,
      Nfsstat3::Toosmall => Nfsstat4::Toosmall,
      Nfsstat3::ServerFault => Nfsstat4::Serverfault,
      Nfsstat3::Badtype => Nfsstat4::Badtype,
      Nfsstat3::Jukebox => Nfsstat4::Delay,
    }
  }
}
