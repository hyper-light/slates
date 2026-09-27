//! The NFSv4.1/4.2 `COMPOUND` (RFC 8881 §16.2, §18): the frame, the session gating, the current and
//! saved file handles, and each operation served through the NFSv3 procedure that means the same thing
//! (A-35, [`crate::v4`]).
//!
//! A compound runs its operations in order and stops at the first that fails; the reply carries every
//! result up to and including it. Outside a session only the operations that create or tear one down
//! may appear, and each must be the only one (`NFS4ERR_NOT_ONLY_OP`); anything else is
//! `NFS4ERR_OP_NOT_IN_SESSION`. A compound that opens with `SEQUENCE` runs under that session: a new
//! request runs and its whole reply is kept on its slot; a retry is answered with the kept reply.
//!
//! Open state (`OPEN`, `CLOSE`) is recorded in a bounded table of state ids; `READ`, `WRITE` and
//! `SETATTR` accept a recorded state id or a special one, and refuse any other (`NFS4ERR_BAD_STATEID`).
//! Byte-range locks are `NFS4ERR_NOTSUPP` for now (a Linux client mounted with `local_lock=all` keeps
//! them itself); delegations are never granted.

use std::collections::BTreeMap;
use std::future::Future;

use super::Nfsstat4;
use super::attr::{self, FsFigures};
use super::lock::{LockKind, LockTable, Range};
use super::session::{CreateSession, ExchangeId, Limits, Sequence, Sequenced, Sessions};
use super::types::{
  Bitmap, ChannelAttrs, FHSIZE, OPAQUE_LIMIT, OTHER_SIZE, SessionId, Stateid, VERIFIER_SIZE,
  decode_sessionid,
};
use super::v3call::{self, Sattr3};
use super::{MINOR_HIGHEST, MINOR_LOWEST};
use crate::nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, Nfstime3};
use crate::procedures::{
  NFSPROC3_ACCESS, NFSPROC3_COMMIT, NFSPROC3_CREATE, NFSPROC3_FSSTAT, NFSPROC3_GETATTR,
  NFSPROC3_LINK, NFSPROC3_LOOKUP, NFSPROC3_MKDIR, NFSPROC3_PATHCONF, NFSPROC3_READ,
  NFSPROC3_READDIRPLUS, NFSPROC3_READLINK, NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR,
  NFSPROC3_SETATTR, NFSPROC3_SYMLINK, NFSPROC3_WRITE,
};
use crate::xdr::{XdrReader, XdrWriter};

/// What serves the v3 procedures a compound's operations become, and knows the connection.
pub trait Backend {
  /// Serves NFSv3 `procedure` with encoded `args`, returning its encoded result.
  fn call_v3(&mut self, procedure: u32, args: Vec<u8>) -> impl Future<Output = Vec<u8>>;
  /// The pseudo-filesystem root `PUTROOTFH` names: the synthetic root, scoped to the connection's
  /// authority.
  fn root_handle(&self) -> Nfsfh3;
  /// The principal the connection runs as (its `AUTH_SYS` uid, or the TLS identity's).
  fn principal(&self) -> u32;
  /// The clock, in nanoseconds.
  fn now_ns(&self) -> u64;
  /// Runs `f` on the v4 server state. The state is borrowed only for the closure, never across an
  /// await, so compounds on other connections served by the same owner interleave with this one.
  /// `NFS4ERR_SERVERFAULT` if the state cannot be reached (a daemon shard that is shutting down).
  fn with_v4<R>(&mut self, f: impl FnOnce(&mut Server) -> R) -> Result<R, Nfsstat4>;
}

/// Format: the operation numbers (RFC 7863 `nfs_opnum4`).
pub mod op {
  /// Format: `OP_ACCESS`.
  pub const ACCESS: u32 = 3;
  /// Format: `OP_CLOSE`.
  pub const CLOSE: u32 = 4;
  /// Format: `OP_COMMIT`.
  pub const COMMIT: u32 = 5;
  /// Format: `OP_CREATE`.
  pub const CREATE: u32 = 6;
  /// Format: `OP_DELEGPURGE`.
  pub const DELEGPURGE: u32 = 7;
  /// Format: `OP_DELEGRETURN`.
  pub const DELEGRETURN: u32 = 8;
  /// Format: `OP_GETATTR`.
  pub const GETATTR: u32 = 9;
  /// Format: `OP_GETFH`.
  pub const GETFH: u32 = 10;
  /// Format: `OP_LINK`.
  pub const LINK: u32 = 11;
  /// Format: `OP_LOCK`.
  pub const LOCK: u32 = 12;
  /// Format: `OP_LOCKT`.
  pub const LOCKT: u32 = 13;
  /// Format: `OP_LOCKU`.
  pub const LOCKU: u32 = 14;
  /// Format: `OP_LOOKUP`.
  pub const LOOKUP: u32 = 15;
  /// Format: `OP_LOOKUPP`.
  pub const LOOKUPP: u32 = 16;
  /// Format: `OP_NVERIFY`.
  pub const NVERIFY: u32 = 17;
  /// Format: `OP_OPEN`.
  pub const OPEN: u32 = 18;
  /// Format: `OP_OPENATTR`.
  pub const OPENATTR: u32 = 19;
  /// Format: `OP_OPEN_DOWNGRADE`.
  pub const OPEN_DOWNGRADE: u32 = 21;
  /// Format: `OP_PUTFH`.
  pub const PUTFH: u32 = 22;
  /// Format: `OP_PUTPUBFH`.
  pub const PUTPUBFH: u32 = 23;
  /// Format: `OP_PUTROOTFH`.
  pub const PUTROOTFH: u32 = 24;
  /// Format: `OP_READ`.
  pub const READ: u32 = 25;
  /// Format: `OP_READDIR`.
  pub const READDIR: u32 = 26;
  /// Format: `OP_READLINK`.
  pub const READLINK: u32 = 27;
  /// Format: `OP_REMOVE`.
  pub const REMOVE: u32 = 28;
  /// Format: `OP_RENAME`.
  pub const RENAME: u32 = 29;
  /// Format: `OP_RESTOREFH`.
  pub const RESTOREFH: u32 = 31;
  /// Format: `OP_SAVEFH`.
  pub const SAVEFH: u32 = 32;
  /// Format: `OP_SECINFO`.
  pub const SECINFO: u32 = 33;
  /// Format: `OP_SETATTR`.
  pub const SETATTR: u32 = 34;
  /// Format: `OP_VERIFY`.
  pub const VERIFY: u32 = 37;
  /// Format: `OP_WRITE`.
  pub const WRITE: u32 = 38;
  /// Format: `OP_BIND_CONN_TO_SESSION`.
  pub const BIND_CONN_TO_SESSION: u32 = 41;
  /// Format: `OP_EXCHANGE_ID`.
  pub const EXCHANGE_ID: u32 = 42;
  /// Format: `OP_CREATE_SESSION`.
  pub const CREATE_SESSION: u32 = 43;
  /// Format: `OP_DESTROY_SESSION`.
  pub const DESTROY_SESSION: u32 = 44;
  /// Format: `OP_FREE_STATEID`.
  pub const FREE_STATEID: u32 = 45;
  /// Format: `OP_SECINFO_NO_NAME`.
  pub const SECINFO_NO_NAME: u32 = 52;
  /// Format: `OP_SEQUENCE`.
  pub const SEQUENCE: u32 = 53;
  /// Format: `OP_TEST_STATEID`.
  pub const TEST_STATEID: u32 = 55;
  /// Format: `OP_DESTROY_CLIENTID`.
  pub const DESTROY_CLIENTID: u32 = 57;
  /// Format: `OP_RECLAIM_COMPLETE`.
  pub const RECLAIM_COMPLETE: u32 = 58;
  /// Format: `OP_ALLOCATE` (RFC 7862).
  pub const ALLOCATE: u32 = 59;
  /// Format: `OP_COPY`.
  pub const COPY: u32 = 60;
  /// Format: `OP_DEALLOCATE`.
  pub const DEALLOCATE: u32 = 62;
  /// Format: `OP_IO_ADVISE`.
  pub const IO_ADVISE: u32 = 63;
  /// Format: `OP_READ_PLUS`.
  pub const READ_PLUS: u32 = 68;
  /// Format: `OP_SEEK`.
  pub const SEEK: u32 = 69;
  /// Format: `OP_CLONE` (RFC 7862).
  pub const CLONE: u32 = 71;
  /// Format: `OP_GETXATTR` (RFC 8276).
  pub const GETXATTR: u32 = 72;
  /// Format: `OP_SETXATTR`.
  pub const SETXATTR: u32 = 73;
  /// Format: `OP_LISTXATTRS`.
  pub const LISTXATTRS: u32 = 74;
  /// Format: `OP_REMOVEXATTR`, the highest operation number NFSv4.2 defines (with RFC 8276).
  pub const REMOVEXATTR: u32 = 75;
  /// Format: `OP_ILLEGAL`: the reply's opnum for an operation number no operation has.
  pub const ILLEGAL: u32 = 10044;
}

/// Format: the flavor `SECINFO_NO_NAME` offers: `AUTH_SYS` (RFC 5531).
const AUTH_SYS: u32 = 1;
/// Format: `SECINFO_STYLE4_CURRENT_FH`, the only style whose argument this server reads.
const OPEN_DELEGATE_NONE: u32 = 0;
/// Format: `OPEN4_RESULT_LOCKTYPE_POSIX`: this server's locks follow POSIX (RFC 8881 §18.16.3).
const OPEN4_RESULT_LOCKTYPE_POSIX: u32 = 4;
/// Format: `opentype4` `OPEN4_CREATE`.
const OPEN4_CREATE: u32 = 1;
/// Format: `createmode4` values (RFC 8881 §18.16.1).
mod createmode4 {
  /// Format: `UNCHECKED4`.
  pub(super) const UNCHECKED: u32 = 0;
  /// Format: `GUARDED4`.
  pub(super) const GUARDED: u32 = 1;
  /// Format: `EXCLUSIVE4`.
  pub(super) const EXCLUSIVE: u32 = 2;
  /// Format: `EXCLUSIVE4_1`.
  pub(super) const EXCLUSIVE_1: u32 = 3;
}
/// Format: `open_claim_type4` values (RFC 8881 §18.16.1).
mod claim {
  /// Format: `CLAIM_NULL`: open a name in the current directory.
  pub(super) const NULL: u32 = 0;
  /// Format: `CLAIM_FH`: open the current file handle itself (NFSv4.1).
  pub(super) const FH: u32 = 4;
}
/// Format: `createtype4` values the CREATE operation accepts (`nfs_ftype4`).
mod ftype4 {
  /// Format: `NF4LNK`.
  pub(super) const LNK: u32 = 5;
  /// Format: `NF4DIR`.
  pub(super) const DIR: u32 = 2;
}
/// Format: `channel_dir_from_server4` `CDFS4_FORE`: a bound connection carries the fore channel only.
const CDFS4_FORE: u32 = 1;
/// Format: the READDIR cookies 0, 1 and 2 are reserved (RFC 8881 §18.23.3), so a v3 cookie is shifted
/// past them.
const COOKIE_SHIFT: u64 = 2;
/// Format: the size of one READDIR entry's fixed fields (value-follows, cookie, name length, the
/// attribute bitmap's length and the attribute list's length), the least an entry costs.
const ENTRY_FIXED: u32 = 4 + 8 + 4 + 4 + 4;
/// Format: `settime4` `SET_TO_CLIENT_TIME4`.
const SET_TO_CLIENT_TIME4: u32 = 1;
/// Format: the attribute numbers SETATTR accepts: size, mode, owner, group and the two settable times.
mod settable {
  /// Format: `FATTR4_SIZE`.
  pub(super) const SIZE: u32 = 4;
  /// Format: `FATTR4_MODE`.
  pub(super) const MODE: u32 = 33;
  /// Format: `FATTR4_OWNER`.
  pub(super) const OWNER: u32 = 36;
  /// Format: `FATTR4_OWNER_GROUP`.
  pub(super) const OWNER_GROUP: u32 = 37;
  /// Format: `FATTR4_TIME_ACCESS_SET`.
  pub(super) const TIME_ACCESS_SET: u32 = 48;
  /// Format: `FATTR4_TIME_MODIFY_SET`.
  pub(super) const TIME_MODIFY_SET: u32 = 54;
}

/// Format: the bytes one compound's frame and operations may add to a READ or WRITE of the transfer
/// ceiling: the tag, the minor version and count, and a handful of operations each carrying at most a
/// v4 file handle (`NFS4_FHSIZE`) and its fixed fields; a compound that needs more is refused
/// `NFS4ERR_REQ_TOO_BIG` by the size, never read past.
pub const COMPOUND_HEADER_BYTES: u32 = 4 * 1024;
/// Format: the smallest encoded operation: its four-byte number and a four-byte argument.
pub const MIN_OPERATION_BYTES: u32 = 8;

/// The v4 server's state: sessions, open and lock state, and the figures its attributes report.
pub struct Server {
  /// Client ids and sessions.
  pub sessions: Sessions,
  opens: Opens,
  locks: LockTable,
  limits: Limits,
  boot: u32,
}

/// One open: the client and open-owner that hold it, the file it opened, the share it holds and its
/// state id's current seqid. An open-owner's opens of one file share one state id (RFC 8881 §9.1.4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Open {
  clientid: u64,
  owner: Vec<u8>,
  fh: Nfsfh3,
  access: u32,
  deny: u32,
  seqid: u32,
}

/// The key an open is found by from its file: the file first, so every open of one file is one range.
type OpenKey = (Vec<u8>, u64, Vec<u8>);

/// The open state ids, bounded, and the index from (file, client, owner) to each.
struct Opens {
  max: usize,
  next: u64,
  table: BTreeMap<[u8; OTHER_SIZE], Open>,
  by_file: BTreeMap<OpenKey, [u8; OTHER_SIZE]>,
}

impl Opens {
  /// Removes the open named `other`, from the table and the index.
  fn remove(&mut self, other: &[u8; OTHER_SIZE]) -> Option<Open> {
    let open = self.table.remove(other)?;
    self
      .by_file
      .remove(&(open.fh.0.clone(), open.clientid, open.owner.clone()));
    Some(open)
  }

  /// Drops every open whose client `live` no longer holds (expired, replaced or destroyed).
  fn purge(&mut self, live: impl Fn(u64) -> bool) {
    let orphaned: Vec<[u8; OTHER_SIZE]> = self
      .table
      .iter()
      .filter(|(_, open)| !live(open.clientid))
      .map(|(other, _)| *other)
      .collect();
    for other in &orphaned {
      self.remove(other);
    }
  }
}

impl Server {
  /// A server instance named `boot`, under `limits`, recording at most `max_opens` opens and
  /// `max_locks` lock ranges at once. WRITE and COMMIT carry the write verifier of the v3 layer that
  /// holds the data, so a restart of that layer is what tells a client to re-send its unstable writes
  /// (RFC 8881 §18.32.3).
  pub fn new(boot: u32, limits: Limits, max_opens: usize, max_locks: usize) -> Server {
    Server {
      sessions: Sessions::new(boot, limits),
      locks: LockTable::new(boot, max_locks),
      opens: Opens {
        max: max_opens,
        next: 1,
        table: BTreeMap::new(),
        by_file: BTreeMap::new(),
      },
      limits,
      boot,
    }
  }

  /// The state of the standalone server (the examples and tests, one connection at a time): one slot,
  /// since a blocking connection serves one request at a time; requests and replies up to the v3
  /// transfer ceiling plus one compound's header; a lease long enough never to lapse under a test.
  pub fn standalone() -> Server {
    /// Shape: the standalone server's clients and each client's sessions (a test drives a handful).
    const CLIENTS: usize = 64;
    /// Shape: sessions per client in the standalone server.
    const SESSIONS: usize = 4;
    /// Shape: opens the standalone server records.
    const OPENS: usize = 4096;
    /// Shape: lock ranges the standalone server records.
    const LOCKS: usize = 4096;
    /// Shape: the standalone server's lease (an hour: a test never outlives it).
    const LEASE_NS: u64 = 3_600_000_000_000;
    let size = crate::procedures::MAX_TRANSFER + COMPOUND_HEADER_BYTES;
    Server::new(
      0,
      Limits {
        max_clients: CLIENTS,
        max_sessions_per_client: SESSIONS,
        offer: ChannelAttrs {
          header_pad: 0,
          max_request: size,
          max_response: size,
          max_response_cached: size,
          max_operations: size / MIN_OPERATION_BYTES,
          max_requests: 1,
        },
        lease_ns: LEASE_NS,
      },
      OPENS,
      LOCKS,
    )
  }

  /// How many opens are recorded.
  pub fn open_count(&self) -> usize {
    self.opens.table.len()
  }

  /// The bounds and offers the server runs under.
  pub fn limits(&self) -> Limits {
    self.limits
  }

  /// How many lock states are recorded.
  pub fn lock_state_count(&self) -> usize {
    self.locks.state_count()
  }

  /// Drops the opens and locks of every client the session table no longer holds.
  fn purge_opens(&mut self) {
    let sessions = &self.sessions;
    self.opens.purge(|clientid| sessions.holds_client(clientid));
    self.locks.purge(|clientid| sessions.holds_client(clientid));
  }
}

/// A compound's running state: the file handles and the client it runs for.
pub(super) struct Frame {
  pub(super) current: Option<Nfsfh3>,
  pub(super) saved: Option<Nfsfh3>,
  pub(super) clientid: Option<u64>,
  /// The `LOCK4denied` body of a LOCK or LOCKT refused `NFS4ERR_DENIED`, which the result carries.
  denied: Option<Vec<u8>>,
}

/// What an operation produced: its result body on success, or the status it failed with.
pub(super) type Outcome = Result<Vec<u8>, Nfsstat4>;

/// Serves one `COMPOUND` call's arguments against `server` and `backend`, returning its encoded
/// `COMPOUND4res`.
pub async fn serve<B: Backend>(backend: &mut B, args: &[u8]) -> Vec<u8> {
  let mut reader = XdrReader::new(args);
  let Ok(tag) = reader.opaque(OPAQUE_LIMIT).map(<[u8]>::to_vec) else {
    return reply(Nfsstat4::Badxdr, &[], 0, &[]);
  };
  let Ok(minor) = reader.u32() else {
    return reply(Nfsstat4::Badxdr, &tag, 0, &[]);
  };
  if !(MINOR_LOWEST..=MINOR_HIGHEST).contains(&minor) {
    return reply(Nfsstat4::MinorVersMismatch, &tag, 0, &[]);
  }
  let Ok(count) = reader.u32() else {
    return reply(Nfsstat4::Badxdr, &tag, 0, &[]);
  };
  let max_operations = match backend.with_v4(|server| server.limits.offer.max_operations) {
    Ok(max_operations) => max_operations,
    Err(status) => return reply(status, &tag, 0, &[]),
  };
  if count > max_operations {
    return reply(Nfsstat4::TooManyOps, &tag, 0, &[]);
  }
  let mut frame = Frame {
    current: None,
    saved: None,
    clientid: None,
    denied: None,
  };
  let mut results = XdrWriter::new();
  let mut done = 0u32;
  let mut last = Nfsstat4::Ok;
  let mut slot: Option<(SessionId, u32)> = None;
  for index in 0..count {
    let Ok(opnum) = reader.u32() else {
      last = Nfsstat4::Badxdr;
      break;
    };
    // An operation the compound's minor version does not define is illegal in it (RFC 8881 §16.2.3):
    // NFSv4.2's operations (RFC 7862) in a 4.1 compound.
    let opnum = if minor < MINOR_HIGHEST && opnum > op::RECLAIM_COMPLETE && opnum <= op::REMOVEXATTR
    {
      op::ILLEGAL
    } else {
      opnum
    };
    let outcome = if index == 0 {
      if opnum == op::SEQUENCE {
        match sequence(backend, &mut reader) {
          Ok(SequenceOutcome::Replay(kept)) => return kept,
          Ok(SequenceOutcome::New {
            body,
            sessionid,
            slotid,
            clientid,
          }) => {
            slot = Some((sessionid, slotid));
            frame.clientid = Some(clientid);
            Ok(body)
          }
          Err(status) => Err(status),
        }
      } else {
        outside_session(backend, opnum, count, &mut reader)
      }
    } else {
      match opnum {
        op::SEQUENCE => Err(Nfsstat4::SequencePos),
        op::EXCHANGE_ID | op::CREATE_SESSION | op::DESTROY_SESSION | op::DESTROY_CLIENTID => {
          Err(Nfsstat4::NotOnlyOp)
        }
        _ => operation(backend, opnum, &mut reader, &mut frame).await,
      }
    };
    done += 1;
    if let Err(status) = record(&mut results, opnum, outcome, &mut frame) {
      last = status;
      break;
    }
  }
  let encoded = reply(last, &tag, done, results.as_slice());
  if let Some((sessionid, slotid)) = slot {
    // A reply that cannot be kept for a retry is not sent as if it had been: the client is told the
    // server failed rather than promised exactly-once (RFC 8881 §2.10.6.1).
    if let Err(status) =
      backend.with_v4(|server| server.sessions.store_reply(&sessionid, slotid, &encoded))
    {
      return reply(status, &tag, 0, &[]);
    }
  }
  encoded
}

/// Appends one operation's result: its number (`OP_ILLEGAL` for an unknown one), its status, and its
/// body — on success, or the `LOCK4denied` of a refused LOCK or LOCKT. Returns the status that ends the
/// compound, if it failed.
fn record(
  results: &mut XdrWriter,
  opnum: u32,
  outcome: Outcome,
  frame: &mut Frame,
) -> Result<(), Nfsstat4> {
  results.u32(if is_operation(opnum) {
    opnum
  } else {
    op::ILLEGAL
  });
  match outcome {
    Ok(body) => {
      results.u32(Nfsstat4::Ok.wire());
      results.fixed(&body);
      Ok(())
    }
    Err(status) => {
      results.u32(status.wire());
      if status == Nfsstat4::Denied
        && let Some(denied) = frame.denied.take()
      {
        results.fixed(&denied);
      }
      Err(status)
    }
  }
}

/// Whether `opnum` names an operation (anything else is answered as `OP_ILLEGAL`).
fn is_operation(opnum: u32) -> bool {
  (op::ACCESS..=op::REMOVEXATTR).contains(&opnum)
}

/// A `COMPOUND4res`: the last status, the tag, the result count and the results.
fn reply(status: Nfsstat4, tag: &[u8], count: u32, results: &[u8]) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  writer.u32(status.wire());
  writer.opaque(tag);
  writer.u32(count);
  writer.fixed(results);
  writer.into_bytes()
}

/// The first operation of a compound with no `SEQUENCE`: only the session and client operations, each
/// alone.
fn outside_session<B: Backend>(
  backend: &mut B,
  opnum: u32,
  count: u32,
  reader: &mut XdrReader<'_>,
) -> Outcome {
  let alone_only = matches!(
    opnum,
    op::EXCHANGE_ID
      | op::CREATE_SESSION
      | op::DESTROY_SESSION
      | op::DESTROY_CLIENTID
      | op::BIND_CONN_TO_SESSION
  );
  if !alone_only {
    return Err(if is_operation(opnum) {
      Nfsstat4::OpNotInSession
    } else {
      Nfsstat4::OpIllegal
    });
  }
  if count > 1 {
    return Err(Nfsstat4::NotOnlyOp);
  }
  match opnum {
    op::EXCHANGE_ID => exchange_id(backend, reader),
    op::CREATE_SESSION => create_session(backend, reader),
    op::DESTROY_SESSION => {
      let sessionid = decode_sessionid(reader).map_err(|_| Nfsstat4::Badxdr)?;
      backend
        .with_v4(|server| server.sessions.destroy_session(&sessionid))?
        .map(|()| Vec::new())
    }
    op::DESTROY_CLIENTID => {
      let clientid = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
      backend
        .with_v4(|server| {
          let destroyed = server.sessions.destroy_clientid(clientid);
          server.purge_opens();
          destroyed
        })?
        .map(|()| Vec::new())
    }
    _ => bind_conn(backend, reader),
  }
}

/// `EXCHANGE_ID` (§18.35): the arguments read, the client id granted, the server's identity returned.
fn exchange_id<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let mut verifier = [0u8; VERIFIER_SIZE];
  verifier.copy_from_slice(reader.fixed(VERIFIER_SIZE).map_err(bad)?);
  let owner = reader.opaque(OPAQUE_LIMIT).map_err(bad)?.to_vec();
  let _flags = reader.u32().map_err(bad)?;
  let protect = reader.u32().map_err(bad)?;
  if protect != 0 {
    // Only SP4_NONE: state protection needs RPCSEC_GSS, which this server does not offer yet.
    return Err(Nfsstat4::Inval);
  }
  let impl_count = reader.u32().map_err(bad)?;
  if impl_count > 1 {
    return Err(Nfsstat4::Badxdr);
  }
  for _ in 0..impl_count {
    reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // nii_domain
    reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // nii_name
    reader.u64().map_err(bad)?; // nii_date seconds
    reader.u32().map_err(bad)?; // nii_date nseconds
  }
  let args = ExchangeId {
    verifier,
    owner,
    principal: backend.principal(),
  };
  let now = backend.now_ns();
  let (granted, boot) = backend.with_v4(|server| {
    let granted = server.sessions.exchange_id(&args, now);
    server.purge_opens();
    granted.map(|granted| (granted, server.boot))
  })??;
  let mut body = XdrWriter::new();
  body.u64(granted.clientid);
  body.u32(granted.sequenceid);
  body.u32(granted.flags);
  body.u32(0); // SP4_NONE
  // server_owner4: the minor id, then the major id naming this server instance.
  body.u64(0);
  body.opaque(&boot.to_be_bytes());
  body.opaque(b"slates"); // server scope
  body.u32(0); // no implementation id
  Ok(body.into_bytes())
}

/// `CREATE_SESSION` (§18.36).
fn create_session<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let clientid = reader.u64().map_err(bad)?;
  let sequence = reader.u32().map_err(bad)?;
  let _flags = reader.u32().map_err(bad)?;
  let fore = ChannelAttrs::decode(reader).map_err(bad)?;
  let back = ChannelAttrs::decode(reader).map_err(bad)?;
  let _cb_program = reader.u32().map_err(bad)?;
  skip_callback_sec(reader)?;
  let args = CreateSession {
    clientid,
    sequence,
    fore,
    back,
  };
  let now = backend.now_ns();
  let granted = backend.with_v4(|server| server.sessions.create_session(&args, now))??;
  let mut body = XdrWriter::new();
  body.fixed(&granted.sessionid);
  body.u32(granted.sequence);
  body.u32(0); // no persistence, no back channel on this connection
  granted.fore.encode(&mut body);
  granted.back.encode(&mut body);
  Ok(body.into_bytes())
}

/// Reads past a `callback_sec_parms4<>` (the flavors this server would call back with; it makes no
/// callbacks yet).
fn skip_callback_sec(reader: &mut XdrReader<'_>) -> Result<(), Nfsstat4> {
  /// Format: the most callback flavors read (a client offers one or two).
  const MAX_FLAVORS: u32 = 8;
  /// Format: `AUTH_NONE`, `AUTH_SYS` and `RPCSEC_GSS` flavor numbers.
  const AUTH_NONE: u32 = 0;
  const RPCSEC_GSS: u32 = 6;
  /// Format: the most supplementary gids an `authsys_parms` carries (RFC 5531).
  const MAX_GIDS: u32 = 16;
  let bad = |_| Nfsstat4::Badxdr;
  let count = reader.u32().map_err(bad)?;
  if count > MAX_FLAVORS {
    return Err(Nfsstat4::Badxdr);
  }
  for _ in 0..count {
    match reader.u32().map_err(bad)? {
      AUTH_NONE => {}
      AUTH_SYS => {
        reader.u32().map_err(bad)?; // stamp
        reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // machine name
        reader.u32().map_err(bad)?; // uid
        reader.u32().map_err(bad)?; // gid
        let gids = reader.u32().map_err(bad)?;
        if gids > MAX_GIDS {
          return Err(Nfsstat4::Badxdr);
        }
        for _ in 0..gids {
          reader.u32().map_err(bad)?;
        }
      }
      RPCSEC_GSS => {
        reader.u32().map_err(bad)?; // service
        reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // handle from server
        reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // handle from client
      }
      _ => return Err(Nfsstat4::Badxdr),
    }
  }
  Ok(())
}

/// `BIND_CONN_TO_SESSION` (§18.34): the connection carries the session's fore channel.
fn bind_conn<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>) -> Outcome {
  let sessionid = decode_sessionid(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let _dir = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let _rdma = reader.bool().map_err(|_| Nfsstat4::Badxdr)?;
  backend
    .with_v4(|server| server.sessions.client_of(&sessionid))?
    .ok_or(Nfsstat4::Badsession)?;
  let mut body = XdrWriter::new();
  body.fixed(&sessionid);
  body.u32(CDFS4_FORE);
  body.bool(false);
  Ok(body.into_bytes())
}

/// How a compound's `SEQUENCE` came out.
enum SequenceOutcome {
  /// A new request: its result body, its slot and its client.
  New {
    body: Vec<u8>,
    sessionid: SessionId,
    slotid: u32,
    clientid: u64,
  },
  /// A retry: the kept reply, to send as it is.
  Replay(Vec<u8>),
}

/// `SEQUENCE` (§18.46).
fn sequence<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
) -> Result<SequenceOutcome, Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  let sessionid = decode_sessionid(reader).map_err(bad)?;
  let sequenceid = reader.u32().map_err(bad)?;
  let slotid = reader.u32().map_err(bad)?;
  let highest_slotid = reader.u32().map_err(bad)?;
  let _cachethis = reader.bool().map_err(bad)?;
  let args = Sequence {
    sessionid,
    sequenceid,
    slotid,
    highest_slotid,
  };
  let now = backend.now_ns();
  match backend.with_v4(|server| server.sessions.sequence(&args, now))?? {
    Sequenced::Replay(kept) => Ok(SequenceOutcome::Replay(kept)),
    Sequenced::New {
      clientid,
      highest_slotid: table_highest,
    } => {
      let mut body = XdrWriter::new();
      body.fixed(&sessionid);
      body.u32(sequenceid);
      body.u32(slotid);
      body.u32(table_highest);
      body.u32(table_highest); // target highest slot
      body.u32(0); // status flags
      Ok(SequenceOutcome::New {
        body: body.into_bytes(),
        sessionid,
        slotid,
        clientid,
      })
    }
  }
}

/// One operation inside a session: the file-handle operations here, the rest by family.
async fn operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::PUTROOTFH | op::PUTPUBFH => {
      frame.current = Some(backend.root_handle());
      Ok(Vec::new())
    }
    op::PUTFH => {
      let fh = reader.opaque(FHSIZE).map_err(|_| Nfsstat4::Badxdr)?;
      frame.current = Some(Nfsfh3(fh.to_vec()));
      Ok(Vec::new())
    }
    op::GETFH => {
      let mut body = XdrWriter::new();
      body.opaque(&current(frame)?.0);
      Ok(body.into_bytes())
    }
    op::SAVEFH => {
      frame.saved = Some(current(frame)?.clone());
      Ok(Vec::new())
    }
    op::RESTOREFH => {
      frame.current = Some(frame.saved.clone().ok_or(Nfsstat4::Restorefh)?);
      Ok(Vec::new())
    }
    op::LOOKUP => lookup_operation(backend, reader, frame).await,
    op::LOOKUPP => {
      let dir = current(frame)?.clone();
      frame.current = Some(lookup(backend, &dir, "..").await?.0);
      Ok(Vec::new())
    }
    _ => object_operation(backend, opnum, reader, frame).await,
  }
}

/// LOOKUP. At the pseudo root, `<name>@<attachment>.<token>` presents a mount capability, as an NFSv3
/// MNT path does (§4.13; AUD-01): the root is re-scoped to it and the bare name looked up.
async fn lookup_operation<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let name = component(reader)?;
  let mut dir = current(frame)?.clone();
  let name = match name.rsplit_once('@') {
    Some((bare, capability)) if crate::multi::is_root_handle(&dir) => {
      let capability = crate::multi::parse_capability(capability).ok_or(Nfsstat4::Noent)?;
      dir = crate::multi::root_handle_with(capability);
      bare.to_owned()
    }
    _ => name,
  };
  frame.current = Some(lookup(backend, &dir, &name).await?.0);
  Ok(Vec::new())
}

/// The operations on the current object's attributes, data and namespace.
async fn object_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::GETATTR => {
      let requested = Bitmap::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
      let fh = current(frame)?.clone();
      getattr(backend, &fh, &requested).await
    }
    op::ACCESS => access(backend, reader, frame).await,
    op::SECINFO_NO_NAME => secinfo_no_name(reader, frame),
    op::READLINK => readlink(backend, frame).await,
    op::READ => read(backend, reader, frame).await,
    op::WRITE => write(backend, reader, frame).await,
    op::COMMIT => commit(backend, reader, frame).await,
    op::READDIR => readdir(backend, reader, frame).await,
    _ => namespace_operation(backend, opnum, reader, frame).await,
  }
}

/// The operations that open, create, rename and remove names, and change attributes.
async fn namespace_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::OPEN => open(backend, reader, frame).await,
    op::SETATTR => setattr(backend, reader, frame).await,
    op::CREATE => create(backend, reader, frame).await,
    op::REMOVE => remove(backend, reader, frame).await,
    op::RENAME => rename(backend, reader, frame).await,
    op::LINK => link(backend, reader, frame).await,
    op::LOCK | op::LOCKT | op::LOCKU => lock_operation(backend, opnum, reader, frame).await,
    op::SEEK
    | op::READ_PLUS
    | op::COPY
    | op::IO_ADVISE
    | op::GETXATTR
    | op::SETXATTR
    | op::LISTXATTRS
    | op::REMOVEXATTR => super::v42::operation(backend, opnum, reader, frame).await,
    _ => backend.with_v4(|server| state_operation(server, opnum, reader, frame))?,
  }
}

/// SECINFO_NO_NAME: one flavor, AUTH_SYS. The current file handle is consumed (RFC 8881 §18.45.3).
fn secinfo_no_name(reader: &mut XdrReader<'_>, frame: &mut Frame) -> Outcome {
  let _style = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  current(frame)?;
  frame.current = None;
  let mut body = XdrWriter::new();
  body.u32(1);
  body.u32(AUTH_SYS);
  Ok(body.into_bytes())
}

/// READLINK.
async fn readlink<B: Backend>(backend: &mut B, frame: &Frame) -> Outcome {
  let fh = current(frame)?.clone();
  let result = backend
    .call_v3(NFSPROC3_READLINK, v3call::handle_only(&fh))
    .await;
  let target = v3(v3call::readlink(&result))?;
  let mut body = XdrWriter::new();
  body.opaque(target.as_bytes());
  Ok(body.into_bytes())
}

/// Format: the ACCESS bits (RFC 8881 §18.1; RFC 8276 §8.5).
mod access4 {
  /// Format: `ACCESS4_READ`.
  pub(super) const READ: u32 = 0x1;
  /// Format: `ACCESS4_MODIFY`.
  pub(super) const MODIFY: u32 = 0x4;
  /// Format: the six bits NFSv3's ACCESS also defines (READ through EXECUTE).
  pub(super) const V3_BITS: u32 = 0x3f;
  /// Format: `ACCESS4_XAREAD`.
  pub(super) const XAREAD: u32 = 0x40;
  /// Format: `ACCESS4_XAWRITE`.
  pub(super) const XAWRITE: u32 = 0x80;
  /// Format: `ACCESS4_XALIST`.
  pub(super) const XALIST: u32 = 0x100;
}

/// ACCESS: the bits this server evaluates are reported supported, and each granted as the file's
/// permissions allow.
async fn access<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let asked = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  // RFC 8276 §8.5: the extended attribute bits follow the file's permissions as the attribute
  // operations do — reading a value needs read (`ACCESS4_READ`), changing one needs modify, and
  // listing needs only the handle.
  let mut v3_asked = asked & access4::V3_BITS;
  if asked & access4::XAREAD != 0 {
    v3_asked |= access4::READ;
  }
  if asked & access4::XAWRITE != 0 {
    v3_asked |= access4::MODIFY;
  }
  let result = backend
    .call_v3(NFSPROC3_ACCESS, v3call::access_args(&fh, v3_asked))
    .await;
  let granted3 = v3(v3call::access(&result))?;
  let mut granted = granted3 & asked & access4::V3_BITS;
  if granted3 & access4::READ != 0 {
    granted |= asked & access4::XAREAD;
  }
  if granted3 & access4::MODIFY != 0 {
    granted |= asked & access4::XAWRITE;
  }
  granted |= asked & access4::XALIST;
  let supported = asked & (access4::V3_BITS | access4::XAREAD | access4::XAWRITE | access4::XALIST);
  let mut body = XdrWriter::new();
  body.u32(supported);
  body.u32(granted);
  Ok(body.into_bytes())
}

/// The operations on the v4 state alone (opens, stateids, reclaim), and the refusals.
fn state_operation(
  server: &mut Server,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::CLOSE => close(server, reader, frame),
    op::OPEN_DOWNGRADE => open_downgrade(server, reader, frame),
    op::RECLAIM_COMPLETE => {
      let _one_fs = reader.bool().map_err(|_| Nfsstat4::Badxdr)?;
      let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
      server
        .sessions
        .reclaim_complete(clientid)
        .map(|()| Vec::new())
    }
    op::TEST_STATEID => test_stateid(server, reader),
    op::FREE_STATEID => {
      let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
      server.free_stateid(&stateid.other).map(|()| Vec::new())
    }
    op::ILLEGAL => Err(Nfsstat4::OpIllegal),
    _ if is_operation(opnum) => Err(Nfsstat4::Notsupp),
    _ => Err(Nfsstat4::OpIllegal),
  }
}

/// The current file handle, or `NFS4ERR_NOFILEHANDLE`.
pub(super) fn current(frame: &Frame) -> Result<&Nfsfh3, Nfsstat4> {
  frame.current.as_ref().ok_or(Nfsstat4::Nofilehandle)
}

/// A v3 result, its status mapped to v4.
pub(super) fn v3<T>(result: Result<Result<T, Nfsstat3>, v3call::Malformed>) -> Result<T, Nfsstat4> {
  match result {
    Ok(Ok(value)) => Ok(value),
    Ok(Err(status)) => Err(Nfsstat4::of_v3(status)),
    Err(v3call::Malformed) => Err(Nfsstat4::Serverfault),
  }
}

/// A `component4` (a UTF-8 name), refused `NFS4ERR_INVAL` when empty, `NFS4ERR_BADNAME` for `.` or `..`
/// or a name with a `/` or NUL (RFC 8881 §14.5).
fn component(reader: &mut XdrReader<'_>) -> Result<String, Nfsstat4> {
  const MAX_NAME: usize = slates_vfs::names::NAME_MAX;
  let name = reader.opaque(MAX_NAME + 1).map_err(|_| Nfsstat4::Badxdr)?;
  if name.is_empty() {
    return Err(Nfsstat4::Inval);
  }
  if name.len() > MAX_NAME {
    return Err(Nfsstat4::Nametoolong);
  }
  let name = std::str::from_utf8(name).map_err(|_| Nfsstat4::Inval)?;
  if name == "." || name == ".." || name.contains(['/', '\0']) {
    return Err(Nfsstat4::Badname);
  }
  Ok(name.to_owned())
}

/// A v3 LOOKUP of `name` in `dir`.
async fn lookup<B: Backend>(
  backend: &mut B,
  dir: &Nfsfh3,
  name: &str,
) -> Result<(Nfsfh3, Option<Fattr3>), Nfsstat4> {
  let result = backend
    .call_v3(NFSPROC3_LOOKUP, v3call::dir_name(dir, name))
    .await;
  v3(v3call::lookup(&result))
}

/// The v3 attributes of `fh`.
pub(super) async fn attrs_of<B: Backend>(backend: &mut B, fh: &Nfsfh3) -> Result<Fattr3, Nfsstat4> {
  let result = backend
    .call_v3(NFSPROC3_GETATTR, v3call::handle_only(fh))
    .await;
  v3(v3call::getattr(&result))
}

/// The filesystem figures an attribute set needs, from the v3 FSSTAT and PATHCONF of `fh` when
/// `requested` names any of them.
async fn figures<B: Backend>(
  backend: &mut B,
  fh: &Nfsfh3,
  attrs: &Fattr3,
  requested: &Bitmap,
) -> Result<FsFigures, Nfsstat4> {
  /// Format: nanoseconds per second.
  const NS_PER_SECOND: u64 = 1_000_000_000;
  let limits = backend.with_v4(|server| server.limits)?;
  let mut figures = FsFigures {
    fsid: (attrs.fsid, 0),
    lease_seconds: u32::try_from(limits.lease_ns / NS_PER_SECOND).unwrap_or(u32::MAX),
    max_file_size: u64::MAX,
    max_link: u32::MAX,
    max_name: u32::try_from(slates_vfs::names::NAME_MAX).unwrap_or(u32::MAX),
    max_io: u64::from(limits.offer.max_response),
    ..FsFigures::default()
  };
  if attr::needs_fs_figures(requested) {
    let result = backend
      .call_v3(NFSPROC3_FSSTAT, v3call::handle_only(fh))
      .await;
    let stat = v3(v3call::fsstat(&result))?;
    figures.space = (stat.bytes.2, stat.bytes.1, stat.bytes.0);
    figures.files = (stat.files.2, stat.files.1, stat.files.0);
  }
  if [
    attr::number::CASE_INSENSITIVE,
    attr::number::MAXLINK,
    attr::number::MAXNAME,
  ]
  .iter()
  .any(|bit| requested.has(*bit))
  {
    let result = backend
      .call_v3(NFSPROC3_PATHCONF, v3call::handle_only(fh))
      .await;
    let conf = v3(v3call::pathconf(&result))?;
    figures.case_insensitive = conf.case_insensitive;
    figures.max_link = conf.link_max;
    figures.max_name = conf.name_max;
  }
  Ok(figures)
}

/// `GETATTR` (§18.7).
async fn getattr<B: Backend>(backend: &mut B, fh: &Nfsfh3, requested: &Bitmap) -> Outcome {
  let attrs = attrs_of(backend, fh).await?;
  let figures = figures(backend, fh, &attrs, requested).await?;
  let mut body = XdrWriter::new();
  attr::encode(requested, &attrs, fh, &figures, &mut body);
  Ok(body.into_bytes())
}

/// Whether `stateid` may serve I/O on `fh` for `clientid`: a special state id, or an open or a lock
/// state this server recorded of that file for that client at a current seqid (RFC 8881 §8.2.2,
/// §9.1.4: READ, WRITE and SETATTR take either).
pub(super) fn check_stateid(
  server: &Server,
  stateid: &Stateid,
  fh: &Nfsfh3,
  clientid: Option<u64>,
) -> Result<(), Nfsstat4> {
  if stateid.is_special() {
    return Ok(());
  }
  if server.locks.contains(&stateid.other) {
    return server.locks.get(stateid, fh, clientid).map(|_| ());
  }
  check_open_stateid(server, stateid, fh, clientid)
}

/// Whether `stateid` names an open this server recorded of `fh` for `clientid` whose seqid is current
/// (0 means "the current one"); an earlier seqid is `NFS4ERR_OLD_STATEID`, anything else
/// `NFS4ERR_BAD_STATEID` (RFC 8881 §8.2.2).
fn check_open_stateid(
  server: &Server,
  stateid: &Stateid,
  fh: &Nfsfh3,
  clientid: Option<u64>,
) -> Result<(), Nfsstat4> {
  let open = server
    .opens
    .table
    .get(&stateid.other)
    .ok_or(Nfsstat4::BadStateid)?;
  if open.fh != *fh || Some(open.clientid) != clientid {
    return Err(Nfsstat4::BadStateid);
  }
  match stateid.seqid {
    0 => Ok(()),
    seqid if seqid == open.seqid => Ok(()),
    seqid if seqid < open.seqid => Err(Nfsstat4::OldStateid),
    _ => Err(Nfsstat4::BadStateid),
  }
}

/// `READ` (§18.22).
async fn read<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let offset = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
  let count = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  backend.with_v4(|server| check_stateid(server, &stateid, &fh, frame.clientid))??;
  let result = backend
    .call_v3(NFSPROC3_READ, v3call::read_args(&fh, offset, count))
    .await;
  let (eof, data) = v3(v3call::read(&result))?;
  let mut body = XdrWriter::new();
  body.bool(eof);
  body.opaque(&data);
  Ok(body.into_bytes())
}

/// `WRITE` (§18.32).
async fn write<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let offset = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
  let stable = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let limit = usize::try_from(backend.with_v4(|server| server.limits.offer.max_request)?)
    .unwrap_or(usize::MAX);
  let data = reader.opaque(limit).map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  backend.with_v4(|server| check_stateid(server, &stateid, &fh, frame.clientid))??;
  let result = backend
    .call_v3(
      NFSPROC3_WRITE,
      v3call::write_args(&fh, offset, stable, data),
    )
    .await;
  let (count, committed, verifier) = v3(v3call::write(&result))?;
  let mut body = XdrWriter::new();
  body.u32(count);
  body.u32(committed);
  body.fixed(&verifier);
  Ok(body.into_bytes())
}

/// `COMMIT` (§18.3).
async fn commit<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let offset = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
  let count = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  let result = backend
    .call_v3(NFSPROC3_COMMIT, v3call::commit_args(&fh, offset, count))
    .await;
  let verifier = v3(v3call::commit(&result))?;
  let mut body = XdrWriter::new();
  body.fixed(&verifier);
  Ok(body.into_bytes())
}

/// `READDIR` (§18.23): a v3 READDIRPLUS, the dot entries dropped, the cookies shifted past the
/// reserved values, each entry's attributes those asked for, the whole bounded by `maxcount`.
async fn readdir<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let cookie = reader.u64().map_err(bad)?;
  let mut verf = [0u8; VERIFIER_SIZE];
  verf.copy_from_slice(reader.fixed(VERIFIER_SIZE).map_err(bad)?);
  let dircount = reader.u32().map_err(bad)?;
  let maxcount = reader.u32().map_err(bad)?;
  let requested = Bitmap::decode(reader).map_err(bad)?;
  if cookie == 1 || cookie == 2 {
    return Err(Nfsstat4::BadCookie);
  }
  let dir = current(frame)?.clone();
  let v3_cookie = cookie.saturating_sub(COOKIE_SHIFT);
  let result = backend
    .call_v3(
      NFSPROC3_READDIRPLUS,
      v3call::readdirplus_args(&dir, v3_cookie, verf, dircount.max(maxcount), maxcount),
    )
    .await;
  let max_entries = usize::try_from(maxcount / ENTRY_FIXED).unwrap_or(0).max(1);
  let listing = v3(v3call::readdirplus(&result, max_entries.saturating_mul(2)))?;
  let dir_attrs = attrs_of(backend, &dir).await?;
  let figures = figures(backend, &dir, &dir_attrs, &requested).await?;
  let mut entries = XdrWriter::new();
  let mut returned = 0usize;
  let mut all = true;
  let budget = usize::try_from(maxcount).unwrap_or(usize::MAX);
  for entry in &listing.entries {
    if entry.name == "." || entry.name == ".." {
      continue;
    }
    let (Some(attrs), Some(fh)) = (&entry.attrs, &entry.fh) else {
      continue;
    };
    let mut one = XdrWriter::new();
    one.bool(true);
    one.u64(entry.cookie.saturating_add(COOKIE_SHIFT));
    one.opaque(entry.name.as_bytes());
    attr::encode(&requested, attrs, fh, &figures, &mut one);
    if entries.len() + one.len() + 2 * size_of::<u32>() + VERIFIER_SIZE > budget {
      all = false;
      break;
    }
    entries.fixed(one.as_slice());
    returned += 1;
  }
  if returned == 0 && !all {
    return Err(Nfsstat4::Toosmall);
  }
  let mut body = XdrWriter::new();
  body.fixed(&listing.verf);
  body.fixed(entries.as_slice());
  body.bool(false); // no more entries in this reply
  body.bool(listing.eof && all);
  Ok(body.into_bytes())
}

/// `OPEN` (§18.16): a name in the current directory (`CLAIM_NULL`), created if asked, or the current
/// file itself (`CLAIM_FH`); an existing file's permission checked against the access asked; the share
/// checked against the file's other opens; a state id recorded; no delegation.
async fn open<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let _seqid = reader.u32().map_err(bad)?;
  // The low bits are the share; the high bits of the access word are delegation wishes (§18.16.3),
  // which this server never grants.
  let access = reader.u32().map_err(bad)? & share::BOTH;
  let deny = reader.u32().map_err(bad)?;
  if access == 0 || deny > share::BOTH {
    return Err(Nfsstat4::Inval);
  }
  // The owner's client id is the session's (§18.16.3: the server uses the session's client).
  let _owner_clientid = reader.u64().map_err(bad)?;
  let owner = reader.opaque(OPAQUE_LIMIT).map_err(bad)?.to_vec();
  let create = match reader.u32().map_err(bad)? {
    OPEN4_CREATE => Some(open_createhow(reader)?),
    _ => None,
  };
  let claim_type = reader.u32().map_err(bad)?;
  let dir = current(frame)?.clone();
  let opened = match claim_type {
    claim::NULL => {
      let name = component(reader)?;
      open_by_name(backend, &dir, &name, create, access).await?
    }
    claim::FH => {
      check_open_kind(backend, &dir).await?;
      check_open_access(backend, &dir, access).await?;
      Opened {
        fh: dir,
        set: Sattr3::default(),
      }
    }
    _ => return Err(Nfsstat4::Notsupp),
  };
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  let share = Share { access, deny };
  let stateid =
    backend.with_v4(|server| server.record_open(clientid, owner, &opened.fh, share))??;
  let mut body = XdrWriter::new();
  stateid.encode(&mut body);
  body.fixed(&change_info());
  body.u32(OPEN4_RESULT_LOCKTYPE_POSIX);
  set_bits(&opened.set).encode(&mut body);
  body.u32(OPEN_DELEGATE_NONE);
  frame.current = Some(opened.fh);
  Ok(body.into_bytes())
}

/// Format: `OPEN4_SHARE_ACCESS_*` and `OPEN4_SHARE_DENY_*` bits (RFC 8881 §18.16.1).
mod share {
  /// Format: read.
  pub(super) const READ: u32 = 1;
  /// Format: write.
  pub(super) const WRITE: u32 = 2;
  /// Format: both.
  pub(super) const BOTH: u32 = 3;
}

/// Format: the v3 ACCESS bits an open's share needs: read data (`ACCESS3_READ`) and change it
/// (`ACCESS3_MODIFY`), RFC 1813 §3.3.4.
mod access3 {
  /// Format: `ACCESS3_READ`.
  pub(super) const READ: u32 = 0x1;
  /// Format: `ACCESS3_MODIFY`.
  pub(super) const MODIFY: u32 = 0x4;
}

/// An open's share: the access it holds and the access it denies to others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Share {
  access: u32,
  deny: u32,
}

/// What `OPEN` opened: the file, and the attributes the open itself set (its `attrset`).
struct Opened {
  fh: Nfsfh3,
  set: Sattr3,
}

/// Opens `name` in `dir`. A create that makes the file sets its attributes and needs no permission on
/// the new file (POSIX: `open(O_CREAT|O_WRONLY)` of mode 0444 succeeds). An `UNCHECKED` create of a name
/// that exists opens it, applying only the size (the truncate `O_TRUNC` asks for) after checking the
/// access asked; a guarded or exclusive create of a name that exists is `NFS4ERR_EXIST`.
async fn open_by_name<B: Backend>(
  backend: &mut B,
  dir: &Nfsfh3,
  name: &str,
  create: Option<(u32, Sattr3)>,
  access: u32,
) -> Result<Opened, Nfsstat4> {
  let existing = match lookup(backend, dir, name).await {
    Ok((fh, _)) => Some(fh),
    Err(Nfsstat4::Noent) if create.is_some() => None,
    Err(status) => return Err(status),
  };
  let (fh, set) = match (existing, create) {
    (Some(_), Some((mode, _))) if mode == v3call::createmode::GUARDED => {
      return Err(Nfsstat4::Exist);
    }
    (Some(fh), create) => {
      check_open_kind(backend, &fh).await?;
      check_open_access(backend, &fh, access).await?;
      let truncate = Sattr3 {
        size: create.and_then(|(_, attrs)| attrs.size),
        ..Sattr3::default()
      };
      if truncate.size.is_some() {
        let result = backend
          .call_v3(NFSPROC3_SETATTR, v3call::setattr_args(&fh, &truncate))
          .await;
        let status = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
        if status != Nfsstat3::Ok {
          return Err(Nfsstat4::of_v3(status));
        }
      }
      (fh, truncate)
    }
    (None, Some((_, attrs))) => {
      // A guarded create: a racing creator of the same name makes this `NFS4ERR_EXIST`, which an
      // unchecked open reports as it would any other failure of its single attempt.
      let result = backend
        .call_v3(
          NFSPROC3_CREATE,
          v3call::create_args(dir, name, v3call::createmode::GUARDED, &attrs),
        )
        .await;
      let fh = match v3(v3call::created(&result))? {
        (Some(fh), _) => fh,
        (None, _) => lookup(backend, dir, name).await?.0,
      };
      (fh, attrs)
    }
    (None, None) => return Err(Nfsstat4::Noent),
  };
  Ok(Opened { fh, set })
}

/// Refuses an open of anything but a regular file: `NFS4ERR_ISDIR`, `NFS4ERR_SYMLINK`, or
/// `NFS4ERR_WRONG_TYPE` (RFC 8881 §18.16.3).
pub(super) async fn check_open_kind<B: Backend>(
  backend: &mut B,
  fh: &Nfsfh3,
) -> Result<(), Nfsstat4> {
  match attrs_of(backend, fh).await?.kind {
    Ftype3::Reg => Ok(()),
    Ftype3::Dir => Err(Nfsstat4::Isdir),
    Ftype3::Lnk => Err(Nfsstat4::Symlink),
    _ => Err(Nfsstat4::WrongType),
  }
}

/// Refuses `NFS4ERR_ACCESS` unless the connection's principal may read (share READ) and change (share
/// WRITE) the file, as the v3 ACCESS procedure evaluates it.
async fn check_open_access<B: Backend>(
  backend: &mut B,
  fh: &Nfsfh3,
  access: u32,
) -> Result<(), Nfsstat4> {
  let mut needed = 0;
  if access & share::READ != 0 {
    needed |= access3::READ;
  }
  if access & share::WRITE != 0 {
    needed |= access3::MODIFY;
  }
  let result = backend
    .call_v3(NFSPROC3_ACCESS, v3call::access_args(fh, needed))
    .await;
  let granted = v3(v3call::access(&result))?;
  if granted & needed == needed {
    Ok(())
  } else {
    Err(Nfsstat4::Access)
  }
}

/// An `OPEN`'s `createhow4`: the v3 create mode and the attributes to set. The exclusive modes are served
/// as a guarded create: a verifier made an exclusive create idempotent across retransmission, which a
/// session's slot reply cache now guarantees for every request (RFC 8881 §2.10.6), so a retried create
/// is answered from the cache and a fresh one on an existing name is `NFS4ERR_EXIST`.
fn open_createhow(reader: &mut XdrReader<'_>) -> Result<(u32, Sattr3), Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  match reader.u32().map_err(bad)? {
    createmode4::UNCHECKED => Ok((v3call::createmode::UNCHECKED, decode_fattr_set(reader)?)),
    createmode4::GUARDED => Ok((v3call::createmode::GUARDED, decode_fattr_set(reader)?)),
    createmode4::EXCLUSIVE => {
      reader.fixed(VERIFIER_SIZE).map_err(bad)?;
      Ok((v3call::createmode::GUARDED, Sattr3::default()))
    }
    createmode4::EXCLUSIVE_1 => {
      reader.fixed(VERIFIER_SIZE).map_err(bad)?;
      Ok((v3call::createmode::GUARDED, decode_fattr_set(reader)?))
    }
    _ => Err(Nfsstat4::Badxdr),
  }
}

impl Server {
  /// Records an open of `fh` by `owner` of `clientid` holding `share`. The owner's existing open of the
  /// file is upgraded (its share joined with the new one) and its state id advanced; a share that
  /// conflicts with another owner's open of the file is `NFS4ERR_SHARE_DENIED`; a new open past the
  /// table's bound is `NFS4ERR_RESOURCE`.
  fn record_open(
    &mut self,
    clientid: u64,
    owner: Vec<u8>,
    fh: &Nfsfh3,
    share: Share,
  ) -> Result<Stateid, Nfsstat4> {
    let key: OpenKey = (fh.0.clone(), clientid, owner);
    let conflicts = self
      .opens
      .by_file
      .range((fh.0.clone(), 0, Vec::new())..)
      .take_while(|((file, _, _), _)| *file == fh.0)
      .filter(|(other_key, _)| **other_key != key)
      .filter_map(|(_, other)| self.opens.table.get(other))
      .any(|other| share.access & other.deny != 0 || share.deny & other.access != 0);
    if conflicts {
      return Err(Nfsstat4::ShareDenied);
    }
    if let Some(other) = self.opens.by_file.get(&key).copied() {
      let open = self
        .opens
        .table
        .get_mut(&other)
        .ok_or(Nfsstat4::Serverfault)?;
      open.access |= share.access;
      open.deny |= share.deny;
      open.seqid = next_seqid(open.seqid);
      return Ok(Stateid {
        seqid: open.seqid,
        other,
      });
    }
    // `NFS4ERR_NOSPC` at the table's bound: `NFS4ERR_RESOURCE` is not valid in NFSv4.1 (RFC 7863),
    // and NOSPC is the exhaustion status OPEN allows (RFC 8881 §15.2).
    if self.opens.table.len() >= self.opens.max {
      return Err(Nfsstat4::Nospc);
    }
    let mut other = [0u8; OTHER_SIZE];
    other[..8].copy_from_slice(&self.opens.next.to_be_bytes());
    other[8..].copy_from_slice(&self.boot.to_be_bytes());
    self.opens.next = self.opens.next.saturating_add(1);
    self.opens.table.insert(
      other,
      Open {
        clientid,
        owner: key.2.clone(),
        fh: fh.clone(),
        access: share.access,
        deny: share.deny,
        seqid: 1,
      },
    );
    self.opens.by_file.insert(key, other);
    Ok(Stateid { seqid: 1, other })
  }
}

/// The seqid after `seqid`: it wraps from the largest back to 1, never to 0, which names "the current
/// one" (RFC 8881 §8.2.2).
fn next_seqid(seqid: u32) -> u32 {
  seqid.checked_add(1).unwrap_or(1)
}

/// The open `stateid` names, for `clientid` on `fh`, with a current seqid; else the refusal
/// [`check_open_stateid`] states.
fn open_of<'a>(
  server: &'a mut Server,
  stateid: &Stateid,
  fh: &Nfsfh3,
  clientid: Option<u64>,
) -> Result<&'a mut Open, Nfsstat4> {
  check_open_stateid(server, stateid, fh, clientid)?;
  server
    .opens
    .table
    .get_mut(&stateid.other)
    .ok_or(Nfsstat4::BadStateid)
}

/// `CLOSE` (§18.2): the open's state released; the reply's state id is the closed one advanced.
fn close(server: &mut Server, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let _seqid = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  if stateid.is_special() {
    return Err(Nfsstat4::BadStateid);
  }
  let seqid = next_seqid(open_of(server, &stateid, &fh, frame.clientid)?.seqid);
  // An open whose lock-owners still hold locks is not closed (§18.2.4, `NFS4ERR_LOCKS_HELD`); its
  // lock states holding none go with it.
  if server.locks.held_under(&stateid.other) {
    return Err(Nfsstat4::LocksHeld);
  }
  server.locks.drop_under(&stateid.other);
  server.opens.remove(&stateid.other);
  let mut body = XdrWriter::new();
  Stateid {
    seqid,
    other: stateid.other,
  }
  .encode(&mut body);
  Ok(body.into_bytes())
}

/// `OPEN_DOWNGRADE` (§18.18): the open's share narrowed to a subset of what it holds (anything else is
/// `NFS4ERR_INVAL`); its state id advances.
fn open_downgrade(server: &mut Server, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let _seqid = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let access = reader.u32().map_err(|_| Nfsstat4::Badxdr)? & share::BOTH;
  let deny = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  if stateid.is_special() {
    return Err(Nfsstat4::BadStateid);
  }
  let open = open_of(server, &stateid, &fh, frame.clientid)?;
  if access == 0 || access & !open.access != 0 || deny & !open.deny != 0 {
    return Err(Nfsstat4::Inval);
  }
  open.access = access;
  open.deny = deny;
  open.seqid = next_seqid(open.seqid);
  let mut body = XdrWriter::new();
  Stateid {
    seqid: open.seqid,
    other: stateid.other,
  }
  .encode(&mut body);
  Ok(body.into_bytes())
}

impl Server {
  /// `FREE_STATEID` (§18.38): a lock state holding no lock, or an open none of whose lock-owners holds
  /// one (with its lock states); `NFS4ERR_LOCKS_HELD` otherwise.
  fn free_stateid(&mut self, other: &[u8; OTHER_SIZE]) -> Result<(), Nfsstat4> {
    if self.locks.contains(other) {
      return self.locks.free(other);
    }
    if !self.opens.table.contains_key(other) {
      return Err(Nfsstat4::BadStateid);
    }
    if self.locks.held_under(other) {
      return Err(Nfsstat4::LocksHeld);
    }
    self.locks.drop_under(other);
    self.opens.remove(other);
    Ok(())
  }
}

/// `LOCK`, `LOCKT` and `LOCKU` (§18.10–18.12): the current file must be a regular file; the operation
/// then runs on the lock table.
async fn lock_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let request = LockRequest::decode(opnum, reader)?;
  let fh = current(frame)?.clone();
  check_open_kind(backend, &fh).await?;
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  match backend.with_v4(|server| server.lock_request(request, &fh, clientid))? {
    Ok(body) => Ok(body),
    Err(LockRefused::Denied(denied)) => {
      frame.denied = Some(denied.encode());
      Err(Nfsstat4::Denied)
    }
    Err(LockRefused::Status(status)) => Err(status),
  }
}

/// A decoded LOCK, LOCKT or LOCKU.
enum LockRequest {
  /// LOCK by a lock-owner new to the open (`open_to_lock_owner4`) or by an existing lock state.
  Lock {
    kind: LockKind,
    range: Range,
    locker: Locker,
  },
  /// LOCKT for `owner`.
  Test {
    kind: LockKind,
    range: Range,
    owner: Vec<u8>,
  },
  /// LOCKU of the lock state `stateid`.
  Unlock { range: Range, stateid: Stateid },
}

/// A LOCK's `locker4`.
enum Locker {
  /// The first lock of `owner` under the open `open` (the lock-owner's client id is the session's,
  /// §18.10.3, so the one on the wire is ignored, as are the seqids).
  New { open: Stateid, owner: Vec<u8> },
  /// A further lock of an existing lock state.
  Existing { stateid: Stateid },
}

/// Why a lock request was refused: a conflict (whose description the result carries) or a status.
enum LockRefused {
  Denied(super::lock::Denied),
  Status(Nfsstat4),
}

impl From<Nfsstat4> for LockRefused {
  fn from(status: Nfsstat4) -> LockRefused {
    LockRefused::Status(status)
  }
}

impl LockRequest {
  /// Reads the arguments of `opnum`. An undefined lock type is `NFS4ERR_BADXDR` (an enum out of
  /// range); a zero length or an end past the largest offset is `NFS4ERR_INVAL` (§18.10.3).
  fn decode(opnum: u32, reader: &mut XdrReader<'_>) -> Result<LockRequest, Nfsstat4> {
    let bad = |_| Nfsstat4::Badxdr;
    let kind = LockKind::from_wire(reader.u32().map_err(bad)?).ok_or(Nfsstat4::Badxdr)?;
    match opnum {
      op::LOCK => {
        // A reclaim has no grace period to run in: this server keeps no lock state across a restart
        // that could be reclaimed (§8.4.2, `NFS4ERR_NO_GRACE`).
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
      op::LOCKT => {
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

impl Server {
  /// Runs a lock request on `fh` for `clientid`: its result body, or why it was refused.
  fn lock_request(
    &mut self,
    request: LockRequest,
    fh: &Nfsfh3,
    clientid: u64,
  ) -> Result<Vec<u8>, LockRefused> {
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
        let stateid = self.locks.set_ranges(&other, next)?;
        let mut body = XdrWriter::new();
        stateid.encode(&mut body);
        Ok(body.into_bytes())
      }
      LockRequest::Test { kind, range, owner } => {
        match self.locks.conflict(fh, clientid, &owner, range, kind) {
          Some(denied) => Err(LockRefused::Denied(denied)),
          None => Ok(Vec::new()),
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
        let stateid = self.locks.set_ranges(&stateid.other, next)?;
        let mut body = XdrWriter::new();
        stateid.encode(&mut body);
        Ok(body.into_bytes())
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
  ) -> Result<[u8; OTHER_SIZE], Nfsstat4> {
    let (open_other, lock_other) = match locker {
      Locker::New { open, owner } => {
        if open.is_special() {
          return Err(Nfsstat4::BadStateid);
        }
        check_open_stateid(self, &open, fh, Some(clientid))?;
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
      .table
      .get(&open_other)
      .map(|open| open.access)
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
}

/// `TEST_STATEID` (§18.48): each state id's validity.
fn test_stateid(server: &Server, reader: &mut XdrReader<'_>) -> Outcome {
  /// Format: the most state ids one TEST_STATEID reads (a client tests a handful).
  const MAX_TESTED: u32 = 1024;
  let count = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  if count > MAX_TESTED {
    return Err(Nfsstat4::Badxdr);
  }
  let mut body = XdrWriter::new();
  body.u32(count);
  for _ in 0..count {
    let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
    let valid =
      server.opens.table.contains_key(&stateid.other) || server.locks.contains(&stateid.other);
    body.u32(if valid {
      Nfsstat4::Ok.wire()
    } else {
      Nfsstat4::BadStateid.wire()
    });
  }
  Ok(body.into_bytes())
}

/// Derived: the largest encoded values of the attributes this server sets — size (8), mode (4), owner
/// and group (a length word and up to `OPAQUE_LIMIT` bytes each), and the access and modification
/// times (a `time_how4` word and an `nfstime4` of 12 bytes each; RFC 7863).
const SETTABLE_VALUES_BYTES: usize = derived_settable_values_bytes();

/// The sum stated on [`SETTABLE_VALUES_BYTES`].
const fn derived_settable_values_bytes() -> usize {
  let word = size_of::<u32>();
  let time = word + size_of::<u64>() + size_of::<u32>();
  size_of::<u64>() + word + 2 * (word + OPAQUE_LIMIT) + 2 * (word + time)
}

/// Reads an `fattr4` of attributes to set, as a v3 `sattr3`: size, mode, owner, group, the access and
/// modification times. Any other attribute is `NFS4ERR_ATTRNOTSUPP`; an owner that is not a numeric id
/// is `NFS4ERR_BADOWNER` (this server keeps numeric ids, RFC 8881 §5.9).
fn decode_fattr_set(reader: &mut XdrReader<'_>) -> Result<Sattr3, Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  let bitmap = Bitmap::decode(reader).map_err(bad)?;
  let values = reader.opaque(SETTABLE_VALUES_BYTES).map_err(bad)?;
  let mut values = XdrReader::new(values);
  let mut attrs = Sattr3::default();
  for bit in bitmap.bits() {
    match bit {
      settable::SIZE => attrs.size = Some(values.u64().map_err(bad)?),
      settable::MODE => attrs.mode = Some(values.u32().map_err(bad)?),
      settable::OWNER => attrs.uid = Some(numeric_id(&mut values)?),
      settable::OWNER_GROUP => attrs.gid = Some(numeric_id(&mut values)?),
      settable::TIME_ACCESS_SET => attrs.atime = Some(settime(&mut values)?),
      settable::TIME_MODIFY_SET => attrs.mtime = Some(settime(&mut values)?),
      _ => return Err(Nfsstat4::Attrnotsupp),
    }
  }
  Ok(attrs)
}

/// A numeric owner or group string (`"501"`, or `"501@domain"`), as an id.
fn numeric_id(values: &mut XdrReader<'_>) -> Result<u32, Nfsstat4> {
  let text = values.string(OPAQUE_LIMIT).map_err(|_| Nfsstat4::Badxdr)?;
  let id = text.split('@').next().unwrap_or(text);
  id.parse().map_err(|_| Nfsstat4::Badowner)
}

/// A `settime4`: `None` for the server's time, `Some(t)` for a client time.
fn settime(values: &mut XdrReader<'_>) -> Result<Option<Nfstime3>, Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  if values.u32().map_err(bad)? != SET_TO_CLIENT_TIME4 {
    return Ok(None);
  }
  let seconds = values.u64().map_err(bad)?;
  let nseconds = values.u32().map_err(bad)?;
  Ok(Some(Nfstime3 {
    seconds: u32::try_from(seconds).map_err(|_| Nfsstat4::Inval)?,
    nseconds,
  }))
}

/// `SETATTR` (§18.30).
async fn setattr<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let attrs = decode_fattr_set(reader)?;
  let fh = current(frame)?.clone();
  backend.with_v4(|server| check_stateid(server, &stateid, &fh, frame.clientid))??;
  let result = backend
    .call_v3(NFSPROC3_SETATTR, v3call::setattr_args(&fh, &attrs))
    .await;
  let status = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
  if status != Nfsstat3::Ok {
    return Err(Nfsstat4::of_v3(status));
  }
  let mut body = XdrWriter::new();
  set_bits(&attrs).encode(&mut body);
  Ok(body.into_bytes())
}

/// The attribute bits a `sattr3` sets, as a SETATTR or OPEN reply's `attrsset`.
fn set_bits(attrs: &Sattr3) -> Bitmap {
  let mut set = Vec::new();
  for (present, bit) in [
    (attrs.size.is_some(), settable::SIZE),
    (attrs.mode.is_some(), settable::MODE),
    (attrs.uid.is_some(), settable::OWNER),
    (attrs.gid.is_some(), settable::OWNER_GROUP),
    (attrs.atime.is_some(), settable::TIME_ACCESS_SET),
    (attrs.mtime.is_some(), settable::TIME_MODIFY_SET),
  ] {
    if present {
      set.push(bit);
    }
  }
  Bitmap::of(&set)
}

/// `CREATE` (§18.4): a directory or a symbolic link in the current directory, which becomes the new
/// object.
async fn create<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let kind = reader.u32().map_err(bad)?;
  let target = if kind == ftype4::LNK {
    Some(
      reader
        .string(crate::procedures::NFS_MAXPATHLEN)
        .map_err(bad)?
        .to_owned(),
    )
  } else {
    None
  };
  let name = component(reader)?;
  let attrs = decode_fattr_set(reader)?;
  let dir = current(frame)?.clone();
  let result = match (kind, &target) {
    (ftype4::DIR, _) => {
      backend
        .call_v3(NFSPROC3_MKDIR, v3call::mkdir_args(&dir, &name, &attrs))
        .await
    }
    (ftype4::LNK, Some(target)) => {
      backend
        .call_v3(
          NFSPROC3_SYMLINK,
          v3call::symlink_args(&dir, &name, &attrs, target),
        )
        .await
    }
    _ => return Err(Nfsstat4::Badtype),
  };
  let fh = match v3(v3call::created(&result))? {
    (Some(fh), _) => fh,
    (None, _) => lookup(backend, &dir, &name).await?.0,
  };
  frame.current = Some(fh);
  let mut body = XdrWriter::new();
  body.bool(false); // change_info4: not atomic
  body.u64(0);
  body.u64(0);
  Bitmap::default().encode(&mut body);
  Ok(body.into_bytes())
}

/// `REMOVE` (§18.25): a file or an empty directory.
async fn remove<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let name = component(reader)?;
  let dir = current(frame)?.clone();
  let result = backend
    .call_v3(NFSPROC3_REMOVE, v3call::dir_name(&dir, &name))
    .await;
  let mut status = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
  if status == Nfsstat3::Isdir {
    let result = backend
      .call_v3(NFSPROC3_RMDIR, v3call::dir_name(&dir, &name))
      .await;
    status = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
  }
  if status != Nfsstat3::Ok {
    return Err(Nfsstat4::of_v3(status));
  }
  Ok(change_info())
}

/// `RENAME` (§18.26): the saved file handle's directory to the current one's.
async fn rename<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let from = component(reader)?;
  let to = component(reader)?;
  let from_dir = frame.saved.clone().ok_or(Nfsstat4::Nofilehandle)?;
  let to_dir = current(frame)?.clone();
  let result = backend
    .call_v3(
      NFSPROC3_RENAME,
      v3call::rename_args(&from_dir, &from, &to_dir, &to),
    )
    .await;
  let status = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
  if status != Nfsstat3::Ok {
    return Err(Nfsstat4::of_v3(status));
  }
  let mut body = change_info();
  body.extend(change_info());
  Ok(body)
}

/// `LINK` (§18.9): the saved file handle linked into the current directory.
async fn link<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let name = component(reader)?;
  let file = frame.saved.clone().ok_or(Nfsstat4::Nofilehandle)?;
  let dir = current(frame)?.clone();
  let result = backend
    .call_v3(NFSPROC3_LINK, v3call::link_args(&file, &dir, &name))
    .await;
  let status = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
  if status != Nfsstat3::Ok {
    return Err(Nfsstat4::of_v3(status));
  }
  Ok(change_info())
}

/// A `change_info4` that is not atomic: the directory's change attribute is not known to this layer
/// before and after, so the client revalidates the directory.
pub(super) fn change_info() -> Vec<u8> {
  let mut body = XdrWriter::new();
  body.bool(false);
  body.u64(0);
  body.u64(0);
  body.into_bytes()
}
