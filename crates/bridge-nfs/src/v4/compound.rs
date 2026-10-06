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

use std::future::Future;

use super::Nfsstat4;
use super::attr::{self, FsFigures};
use super::files::{LockRequest, share};
use super::session::{
  CreateSession, ExchangeId, Limits, ReplyLimits, Sequence, Sequenced, Sessions,
};
use super::types::{
  BITMAP_WORDS_MAX, Bitmap, ChannelAttrs, FHSIZE, OPAQUE_LIMIT, SESSIONID_SIZE, SessionId, Stateid,
  VERIFIER_SIZE, decode_sessionid,
};
use super::v3call::{self, Sattr3};
use super::{MINOR_HIGHEST, MINOR_LOWEST};
use crate::nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, Nfstime4, Specdata3, Wcc};
use crate::procedures::{
  NFSPROC3_ACCESS, NFSPROC3_COMMIT, NFSPROC3_CREATE, NFSPROC3_FSSTAT, NFSPROC3_GETATTR,
  NFSPROC3_LINK, NFSPROC3_LOOKUP, NFSPROC3_MKDIR, NFSPROC3_MKNOD, NFSPROC3_PATHCONF,
  NFSPROC3_READDIRPLUS, NFSPROC3_READLINK, NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR,
  NFSPROC3_SETATTR, NFSPROC3_SYMLINK, extension,
};
use crate::xdr::{XdrReader, XdrWriter};

/// What serves the v3 procedures a compound's operations become, and knows the connection.
pub trait Backend {
  /// Serves NFSv3 `procedure` with encoded `args`, returning its encoded result.
  fn call_v3(&mut self, procedure: u32, args: Vec<u8>) -> impl Future<Output = Vec<u8>>;
  /// Serves an id-only file state procedure at the owner partition `owner` (§4.6 A-36: a state id
  /// names its owner, `crate::v4::files::owner_of`), returning its encoded result.
  fn call_owner(
    &mut self,
    owner: u16,
    procedure: u32,
    args: Vec<u8>,
  ) -> impl Future<Output = Vec<u8>>;
  /// Every owner partition this server's files may live at: where a dropped client's state is purged.
  fn owners(&self) -> Vec<u16>;
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
  /// A guest session served a request of `clientid`, whose record is at the session's home (A-76): the home is
  /// due a renewal note. A server whose sessions never leave their table has nothing to send.
  fn renew_home(&mut self, clientid: u64) {
    let _ = clientid;
  }
  /// Whether the file `fh` names is owned by the shard serving this compound (A-78: a delegation is granted only
  /// there). A server with one owner owns every file.
  fn owns_file(&self, fh: &Nfsfh3) -> bool {
    let _ = fh;
    true
  }
  /// Whether `clientid` holds a revoked delegation it has not freed at this shard's files
  /// (`SEQ4_STATUS_RECALLABLE_STATE_REVOKED`, §10.4.5).
  fn revoked_state(&mut self, clientid: u64) -> bool {
    let _ = clientid;
    false
  }
  /// The compound's operations act for `clientid`, the client its `SEQUENCE` named (A-80): the owner tells a
  /// delegation holder's own change from another client's by it. A server that grants no delegations ignores it.
  fn act_for(&mut self, clientid: u64) {
    let _ = clientid;
  }
  /// The compound ended at operation `opnum` with `status` (its last operation's, `NFS4_OK` when every one
  /// succeeded), for the server's counts of which operations its clients are told to retry.
  fn finished(&mut self, opnum: u32, status: Nfsstat4) {
    let _ = (opnum, status);
  }
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
  /// Format: `OP_BACKCHANNEL_CTL`.
  pub const BACKCHANNEL_CTL: u32 = 40;
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
  /// Format: `OP_SET_SSV`.
  pub const SET_SSV: u32 = 54;
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
/// Format: `SEQ4_STATUS_RECALLABLE_STATE_REVOKED` (RFC 8881 §18.46.3).
const SEQ4_STATUS_RECALLABLE_STATE_REVOKED: u32 = 0x0000_0040;
/// Format: `OPEN_DELEGATE_READ` (RFC 8881 §18.16.2).
const OPEN_DELEGATE_READ: u32 = 1;
/// Format: `ACE4_ACCESS_ALLOWED_ACE_TYPE` (RFC 8881 §6.2.1.1).
const ACE4_ACCESS_ALLOWED_ACE_TYPE: u32 = 0;
/// Format: `OPEN4_SHARE_ACCESS_WANT_DELEG_MASK` and `OPEN4_SHARE_ACCESS_WANT_NO_DELEG` (RFC 8881 §18.16.1).
const OPEN4_SHARE_ACCESS_WANT_DELEG_MASK: u32 = 0xFF00;
/// Format: see [`OPEN4_SHARE_ACCESS_WANT_DELEG_MASK`].
const OPEN4_SHARE_ACCESS_WANT_NO_DELEG: u32 = 0x0400;
/// Format: `OPEN4_SHARE_ACCESS_WANT_READ_DELEG` (RFC 8881 §18.16.1): the client wants a read delegation, not a write
/// one.
const OPEN4_SHARE_ACCESS_WANT_READ_DELEG: u32 = 0x0100;
/// Format: `OPEN_DELEGATE_WRITE` (RFC 8881 §18.16.2).
const OPEN_DELEGATE_WRITE: u32 = 2;
/// Format: `NFS_LIMIT_SIZE` (RFC 8881 §18.16.1, `limit_by4`).
const NFS_LIMIT_SIZE: u32 = 1;
/// Format: the `STATE_OPEN` argument's bits naming the delegations an open may be granted (A-80): a read one.
const MAY_DELEGATE_READ: u32 = 1;
/// Format: see [`MAY_DELEGATE_READ`]: a write one.
const MAY_DELEGATE_WRITE: u32 = 2;
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
  /// Format: `CLAIM_DELEGATE_CUR`: open a name in the current directory under a delegation being returned (§10.4.4).
  pub(super) const DELEGATE_CUR: u32 = 2;
  /// Format: `CLAIM_FH`: open the current file handle itself (NFSv4.1).
  pub(super) const FH: u32 = 4;
  /// Format: `CLAIM_DELEG_CUR_FH`: open the current file handle under a delegation being returned.
  pub(super) const DELEG_CUR_FH: u32 = 5;
}
/// Format: `channel_dir_from_server4` `CDFS4_FORE`: a bound connection carries the fore channel only.
const CDFS4_FORE: u32 = 1;
use super::listing::{COOKIE_SHIFT, ENTRY_FIXED, ENTRY_TYPICAL};
/// Format: the most a v3 READDIRPLUS `entryplus3` encodes beyond the v4 `entry4` built from it, their names (encoded
/// alike) aside: the v3 entry's fixed fields (value-follows 4, `fileid` 8, `cookie` 8, the name's length 4), its
/// attributes (present 4, `fattr3` 84) and its handle (present 4, length 4, at most `NFS3_FHSIZE` 64 bytes), less the
/// v4 entry's [`ENTRY_FIXED`].
const V3_ENTRY_EXCESS: u32 = (4 + 8 + 8 + 4) + (4 + 84) + (4 + 4 + 64) - ENTRY_FIXED;
/// Format: a v3 READDIRPLUS reply's fields beside its entries: the status 4, the directory's `post_op_attr` (4 + 84),
/// the cookie verifier 8, and the end-of-list and `eof` booleans (4 + 4).
const V3_REPLY_OVERHEAD: u32 = 4 + (4 + 84) + 8 + 4 + 4;
/// Format: `settime4` `SET_TO_CLIENT_TIME4`.
const SET_TO_CLIENT_TIME4: u32 = 1;

/// Derived: the bytes a READ or WRITE compound adds to its transfer, RPC headers included — the most a
/// request of the transfer ceiling needs beyond its data, so a session sized `MAX_TRANSFER` plus this
/// carries every such request (RFC 8881 §18.36.3: the sizes count the RPC headers). The sum of:
/// - the RPC call header at its largest: six words, then a credential and a verifier each of a flavor,
///   a length and at most `MAX_AUTH_BYTES` (400, RFC 5531 §8.2);
/// - the compound's frame: its tag (a length and at most [`OPAQUE_LIMIT`] bytes), minor version and
///   operation count;
/// - SEQUENCE (session id and four words), PUTFH (the largest v4 handle, [`FHSIZE`]), a WRITE's fixed
///   fields (state id, offset, stability, data length), and a GETATTR of every bitmap word
///   ([`BITMAP_WORDS_MAX`]), each after its operation number.
///
/// A READ or WRITE reply's fixed fields (the RPC reply header, the frame, SEQUENCE's, PUTFH's and
/// READ's results) are fewer. A compound needing more is refused `NFS4ERR_REQ_TOO_BIG`.
pub const COMPOUND_HEADER_BYTES: u32 = derived_compound_header_bytes();

/// The sum stated on [`COMPOUND_HEADER_BYTES`].
const fn derived_compound_header_bytes() -> u32 {
  /// Format: an XDR word.
  const WORD: usize = 4;
  /// Format: `MAX_AUTH_BYTES` (RFC 5531 §8.2), the most a credential or verifier body holds.
  const MAX_AUTH_BYTES: usize = 400;
  /// Format: the RPC call header's fixed words: xid, message type, RPC version, program, version,
  /// procedure.
  const CALL_WORDS: usize = 6;
  /// Format: a state id: a sequence word and twelve bytes.
  const STATEID: usize = WORD + 12;
  let rpc = CALL_WORDS * WORD + 2 * (2 * WORD + MAX_AUTH_BYTES);
  let frame = WORD + OPAQUE_LIMIT + 2 * WORD;
  let sequence = WORD + SESSIONID_SIZE + 4 * WORD;
  let putfh = WORD + WORD + FHSIZE;
  let write = WORD + STATEID + 2 * WORD + 2 * WORD;
  let getattr = WORD + WORD + BITMAP_WORDS_MAX * WORD;
  let total = rpc + frame + sequence + putfh + write + getattr;
  // `u32::try_from` is not const: narrow, and fail the build if the round trip loses anything.
  #[allow(clippy::cast_possible_truncation)]
  let narrowed = total as u32;
  assert!(narrowed as usize == total, "the compound header fits a u32");
  narrowed
}
/// Format: the smallest encoded operation: its four-byte number and a four-byte argument.
pub const MIN_OPERATION_BYTES: u32 = 8;

/// The v4 server's state: sessions, open and lock state, and the figures its attributes report.
pub struct Server {
  /// Client ids and sessions.
  pub sessions: Sessions,
  /// Purges an owner could not take, retried on the next drop: `(client, owner partition)`, never more
  /// than the clients the table held times the owners.
  pending_purges: Vec<(u64, u16)>,
  limits: Limits,
  /// The major id of the `server_owner4` EXCHANGE_ID answers (RFC 8881 §2.10.5): what names this server, the same
  /// across its restarts and different from every other server's. A client takes two servers with one major id (and
  /// the same client id) for one server and shares one client between them (Linux: `nfs41_walk_client_list`), so a
  /// value every server shares merges them (docs/bugs/2026-10-04-every-daemon-announced-one-nfs-server-owner.md).
  owner: u64,
}

impl Server {
  /// A listener's v4 server instance named `boot`, under `limits`: client ids, sessions and leases.
  /// File state lives at the files' owners (§4.6 A-36). WRITE and COMMIT
  /// carry the write verifier of the v3 layer that holds the data, so a restart of that layer is what
  /// tells a client to re-send its unstable writes (RFC 8881 §18.32.3).
  pub fn new(boot: u32, limits: Limits) -> Server {
    Server {
      sessions: Sessions::new(boot, limits),
      pending_purges: Vec::new(),
      limits,
      owner: u64::from(boot),
    }
  }

  /// This server named `owner` in its `server_owner4` (a daemon's instance identity; [`Server::new`] names a
  /// standalone server by its instance alone).
  pub fn with_owner(mut self, owner: u64) -> Server {
    self.owner = owner;
    self
  }

  /// A durable listener (§4.6 A-37): its client table rebuilt from the records kept before a restart
  /// and journaled from now on (`Sessions::restore`).
  pub fn restore(
    boot: u32,
    limits: Limits,
    records: Vec<super::session::ClientRecord>,
    now_ns: u64,
  ) -> Server {
    let mut server = Server::new(boot, limits);
    server.sessions = Sessions::restore(boot, limits, records, now_ns);
    server
  }

  /// The state of the standalone server (the examples and tests, one connection at a time): one slot,
  /// since a blocking connection serves one request at a time; requests and replies up to the v3
  /// transfer ceiling plus one compound's header; a lease long enough never to lapse under a test.
  pub fn standalone() -> Server {
    /// Shape: the standalone server's clients and each client's sessions (a test drives a handful).
    const CLIENTS: usize = 64;
    /// Shape: sessions per client in the standalone server.
    const SESSIONS: usize = 4;
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
    )
  }

  /// The bounds and offers the server runs under.
  pub fn limits(&self) -> Limits {
    self.limits
  }

  /// The clients the session table dropped since the last call (replaced, expired or destroyed), whose
  /// file state every owner must drop.
  fn take_dropped(&mut self) -> Vec<u64> {
    self.sessions.take_dropped()
  }
}

/// A compound's running state: the file handles and the client it runs for.
pub(super) struct Frame {
  /// The compound's minor version.
  pub(super) minor: u32,
  pub(super) current: Option<Nfsfh3>,
  pub(super) saved: Option<Nfsfh3>,
  pub(super) clientid: Option<u64>,
  /// The session the compound's `SEQUENCE` named, when it opened with one.
  pub(super) session: Option<SessionId>,
  /// The `LOCK4denied` body of a LOCK or LOCKT refused `NFS4ERR_DENIED`, which the result carries.
  denied: Option<Vec<u8>>,
}

/// What an operation produced: its result body on success, or the status it failed with.
pub(super) type Outcome = Result<Vec<u8>, Nfsstat4>;

/// Serves one `COMPOUND` call's arguments against `server` and `backend`, returning its encoded
/// `COMPOUND4res`.
pub async fn serve<B: Backend>(backend: &mut B, args: &[u8], request_bytes: usize) -> Vec<u8> {
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
    minor,
    current: None,
    saved: None,
    clientid: None,
    session: None,
    denied: None,
  };
  let mut results = XdrWriter::new();
  let mut done = 0u32;
  let mut last = Nfsstat4::Ok;
  let mut last_op = 0u32;
  let mut slot: Option<(SessionId, u32)> = None;
  let mut limits: Option<ReplyLimits> = None;
  for index in 0..count {
    let Ok(opnum) = reader.u32() else {
      last = Nfsstat4::Badxdr;
      break;
    };
    let opnum = defined_in(minor, opnum);
    last_op = opnum;
    let outcome = if index == 0 {
      if opnum == op::SEQUENCE {
        match sequence(backend, &mut reader, (request_bytes, count)) {
          Ok(SequenceOutcome::Replay(kept)) => return kept,
          Ok(SequenceOutcome::New {
            body,
            sessionid,
            slotid,
            clientid,
            limits: reply_limits,
          }) => {
            slot = Some((sessionid, slotid));
            limits = Some(reply_limits);
            frame.clientid = Some(clientid);
            frame.session = Some(sessionid);
            backend.act_for(clientid);
            Ok(body)
          }
          Err(status) => Err(status),
        }
      } else {
        outside_session(backend, opnum, count, &mut reader).await
      }
    } else {
      later_operation(backend, opnum, &mut reader, &mut frame).await
    };
    done += 1;
    let before = results.len();
    let recorded = record(&mut results, opnum, outcome, &mut frame);
    // The result that would carry the reply past the session's sizes is replaced by the refusal that
    // says so, and the compound ends there (RFC 8881 §2.10.6.4).
    if let Some(too_big) =
      limits.and_then(|limits| oversize(limits, reply_bytes_so_far(&tag, results.len())))
    {
      results.truncate(before);
      let _ = record(&mut results, opnum, Err(too_big), &mut frame);
      last = too_big;
      break;
    }
    if let Err(status) = recorded {
      last = status;
      break;
    }
  }
  backend.finished(last_op, last);
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

/// What a compound must be served with (A-76), read from its first operation without serving it: the session it
/// names (`SEQUENCE`, `DESTROY_SESSION`, `BIND_CONN_TO_SESSION`), the client table (`EXCHANGE_ID`,
/// `CREATE_SESSION`, `DESTROY_CLIENTID`), or nothing in particular (a malformed or empty compound, answered
/// wherever it lands).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
  /// Served where this session is held.
  Session(SessionId),
  /// Served where the client table is (the session's home).
  Clients,
  /// Served wherever it arrived.
  Anywhere,
}

/// The minor version a compound's header names (its `minorversion`), or `None` for a header that does not parse.
pub fn minor_version(args: &[u8]) -> Option<u32> {
  let mut reader = XdrReader::new(args);
  reader.opaque(OPAQUE_LIMIT).ok()?;
  reader.u32().ok()
}

/// The [`Placement`] of the compound whose arguments are `args`.
pub fn placement(args: &[u8]) -> Placement {
  let mut reader = XdrReader::new(args);
  let header = reader
    .opaque(OPAQUE_LIMIT)
    .and_then(|_| reader.u32())
    .and_then(|_| reader.u32());
  let (Ok(count), Ok(opnum)) = (header, reader.u32()) else {
    return Placement::Anywhere;
  };
  if count == 0 {
    return Placement::Anywhere;
  }
  match opnum {
    op::SEQUENCE | op::DESTROY_SESSION | op::BIND_CONN_TO_SESSION => {
      decode_sessionid(&mut reader).map_or(Placement::Anywhere, Placement::Session)
    }
    op::EXCHANGE_ID | op::CREATE_SESSION | op::DESTROY_CLIENTID => Placement::Clients,
    _ => Placement::Anywhere,
  }
}

/// `opnum` as the compound's minor version knows it: an operation the version does not define is
/// illegal in it (RFC 8881 §16.2.3), as NFSv4.2's operations (RFC 7862, RFC 8276) are in a 4.1 compound.
fn defined_in(minor: u32, opnum: u32) -> u32 {
  if minor < MINOR_HIGHEST && opnum > op::RECLAIM_COMPLETE && opnum <= op::REMOVEXATTR {
    op::ILLEGAL
  } else {
    opnum
  }
}

/// An operation after the first: SEQUENCE may only open a compound, and the session and client
/// operations must stand alone.
async fn later_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::SEQUENCE => Err(Nfsstat4::SequencePos),
    op::EXCHANGE_ID | op::CREATE_SESSION | op::DESTROY_SESSION | op::DESTROY_CLIENTID => {
      Err(Nfsstat4::NotOnlyOp)
    }
    _ => operation(backend, opnum, reader, frame).await,
  }
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
async fn outside_session<B: Backend>(
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
    op::EXCHANGE_ID => {
      let granted = exchange_id(backend, reader);
      // A client this replaced, or a lapsed one it made room for, leaves its file state at the owners.
      purge_dropped(backend).await?;
      granted
    }
    op::CREATE_SESSION => create_session(backend, reader),
    op::DESTROY_SESSION => {
      let sessionid = decode_sessionid(reader).map_err(|_| Nfsstat4::Badxdr)?;
      backend
        .with_v4(|server| server.sessions.destroy_session(&sessionid))?
        .map(|()| Vec::new())
    }
    op::DESTROY_CLIENTID => {
      let clientid = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
      backend.with_v4(|server| server.sessions.destroy_clientid(clientid))??;
      purge_dropped(backend).await?;
      Ok(Vec::new())
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
  let (granted, owner) = backend.with_v4(|server| {
    let granted = server.sessions.exchange_id(&args, now);
    granted.map(|granted| (granted, server.owner))
  })??;
  let mut body = XdrWriter::new();
  body.u64(granted.clientid);
  body.u32(granted.sequenceid);
  body.u32(granted.flags);
  body.u32(0); // SP4_NONE
  // server_owner4: the minor id, then the major id naming this server (the same across its restarts, never another
  // server's).
  body.u64(0);
  body.opaque(&owner.to_be_bytes());
  body.opaque(b"slates"); // server scope
  body.u32(0); // no implementation id
  Ok(body.into_bytes())
}

/// Format: `CREATE_SESSION4_FLAG_CONN_BACK_CHAN` (RFC 8881 §18.36.1): the creating connection carries the back
/// channel.
const CREATE_SESSION4_FLAG_CONN_BACK_CHAN: u32 = 0x0000_0002;

/// `CREATE_SESSION` (§18.36).
fn create_session<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let clientid = reader.u64().map_err(bad)?;
  let sequence = reader.u32().map_err(bad)?;
  let flags = reader.u32().map_err(bad)?;
  let fore = ChannelAttrs::decode(reader).map_err(bad)?;
  let back = ChannelAttrs::decode(reader).map_err(bad)?;
  let cb_program = reader.u32().map_err(bad)?;
  // The back channel is granted when the client asks for it on this connection and offers a flavor this server
  // calls back with (AUTH_NONE or AUTH_SYS; an RPCSEC_GSS handle alone cannot be used without GSS).
  let (credential, _) = read_callback_sec(reader)?;
  let args = CreateSession {
    clientid,
    sequence,
    fore,
    back,
    callback: credential
      .filter(|_| flags & CREATE_SESSION4_FLAG_CONN_BACK_CHAN != 0)
      .map(|credential| (cb_program, credential)),
  };
  let now = backend.now_ns();
  let granted = backend.with_v4(|server| server.sessions.create_session(&args, now))??;
  let mut body = XdrWriter::new();
  body.fixed(&granted.sessionid);
  body.u32(granted.sequence);
  // No persistence (sessions are not durable, A-37); the back channel on this connection when granted.
  body.u32(if granted.back_channel {
    CREATE_SESSION4_FLAG_CONN_BACK_CHAN
  } else {
    0
  });
  granted.fore.encode(&mut body);
  granted.back.encode(&mut body);
  Ok(body.into_bytes())
}

/// Reads a `callback_sec_parms4<>` (the flavors the client accepts callbacks under, in its order of preference): the
/// RPC credential (`opaque_auth`, encoded) of the first this server can call back with — `AUTH_NONE`, or `AUTH_SYS`
/// with the client's own `authsys_parms` byte for byte — and whether any entry names an RPCSEC_GSS handle (which needs
/// GSS, not offered here). RFC 8881 §18.36.3: the server calls back under a flavor the client listed; a callback under
/// another is refused (Linux refuses an AUTH_NONE callback after offering AUTH_SYS, measured 2026-10-04).
fn read_callback_sec(reader: &mut XdrReader<'_>) -> Result<(Option<Vec<u8>>, bool), Nfsstat4> {
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
  let mut credential = None;
  let mut names_gss = false;
  for _ in 0..count {
    match reader.u32().map_err(bad)? {
      AUTH_NONE => {
        let mut none = XdrWriter::new();
        none.u32(AUTH_NONE);
        none.opaque(&[]);
        credential.get_or_insert(none.into_bytes());
      }
      AUTH_SYS => {
        let parms = reader.rest();
        let before = reader.remaining();
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
        let used = before.saturating_sub(reader.remaining());
        let mut sys = XdrWriter::new();
        sys.u32(AUTH_SYS);
        sys.opaque(parms.get(..used).ok_or(Nfsstat4::Badxdr)?);
        credential.get_or_insert(sys.into_bytes());
      }
      RPCSEC_GSS => {
        names_gss = true;
        reader.u32().map_err(bad)?; // service
        reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // handle from server
        reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // handle from client
      }
      _ => return Err(Nfsstat4::Badxdr),
    }
  }
  Ok((credential, names_gss))
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
    limits: ReplyLimits,
  },
  /// A retry: the kept reply, to send as it is.
  Replay(Vec<u8>),
}

/// The size a compound reply with `results_len` bytes of results takes on the wire, its RPC reply
/// header included: what the session's response sizes bound.
fn reply_bytes_so_far(tag: &[u8], results_len: usize) -> usize {
  /// Format: an XDR word.
  const WORD: usize = 4;
  let rpc = crate::rpc::reply_bytes(0, crate::rpc::AcceptStatus::Success, &[]).len();
  let tag_padded = tag.len().div_ceil(WORD) * WORD;
  rpc + WORD + WORD + tag_padded + WORD + results_len
}

/// The refusal a reply of `size` bytes earns under `limits`: past the response size,
/// `NFS4ERR_REP_TOO_BIG`; within it but past the cache size the client asked to be kept to,
/// `NFS4ERR_REP_TOO_BIG_TO_CACHE`.
fn oversize(limits: ReplyLimits, size: usize) -> Option<Nfsstat4> {
  if size > limits.max_response {
    Some(Nfsstat4::RepTooBig)
  } else if limits.max_cached.is_some_and(|cached| size > cached) {
    Some(Nfsstat4::RepTooBigToCache)
  } else {
    None
  }
}

/// `SEQUENCE` (§18.46), for a request of `request_bytes` carrying `operations` operations.
fn sequence<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  (request_bytes, operations): (usize, u32),
) -> Result<SequenceOutcome, Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  let sessionid = decode_sessionid(reader).map_err(bad)?;
  let sequenceid = reader.u32().map_err(bad)?;
  let slotid = reader.u32().map_err(bad)?;
  let highest_slotid = reader.u32().map_err(bad)?;
  let cache_this = reader.bool().map_err(bad)?;
  let args = Sequence {
    sessionid,
    sequenceid,
    slotid,
    highest_slotid,
    request_bytes,
    operations,
    cache_this,
  };
  let now = backend.now_ns();
  match backend.with_v4(|server| server.sessions.sequence(&args, now))?? {
    Sequenced::Replay(kept) => Ok(SequenceOutcome::Replay(kept)),
    Sequenced::New {
      clientid,
      highest_slotid: table_highest,
      limits,
      renew_home,
    } => {
      if renew_home {
        backend.renew_home(clientid);
      }
      let mut body = XdrWriter::new();
      body.fixed(&sessionid);
      body.u32(sequenceid);
      body.u32(slotid);
      body.u32(table_highest);
      body.u32(table_highest); // target highest slot
      // A client holding a revoked delegation it has not freed is told on every SEQUENCE until it frees it (§10.4.5).
      body.u32(if backend.revoked_state(clientid) {
        SEQ4_STATUS_RECALLABLE_STATE_REVOKED
      } else {
        0
      });
      Ok(SequenceOutcome::New {
        body: body.into_bytes(),
        sessionid,
        slotid,
        clientid,
        limits,
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
      frame.current = Some(lookup(backend, &dir, "..").await?.fh);
      Ok(Vec::new())
    }
    _ => object_operation(backend, opnum, reader, frame).await,
  }
}

/// LOOKUP. At the pseudo root, `<name>@<attachment>.<token>` presents a mount capability, as an NFSv3
/// MNT path does (§4.13; AUD-01): the root is re-scoped to it and the bare name looked up. A bare
/// `@<attachment>.<token>` enters the root scoped to the capability, as NFSv3's `/@<capability>` does,
/// so a scoped browse lists what the capability authorizes.
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
      if bare.is_empty() {
        frame.current = Some(dir);
        return Ok(Vec::new());
      }
      bare.to_owned()
    }
    _ => name,
  };
  frame.current = Some(lookup(backend, &dir, &name).await?.fh);
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
      getattr(backend, &fh, (&requested, frame.minor)).await
    }
    op::ACCESS => access(backend, reader, frame).await,
    op::READLINK => readlink(backend, frame).await,
    op::READ => read(backend, reader, frame).await,
    op::WRITE => write(backend, reader, frame).await,
    op::COMMIT => commit(backend, reader, frame).await,
    op::READDIR => readdir(backend, reader, frame).await,
    _ => query_operation(backend, opnum, reader, frame).await,
  }
}

/// The operations that ask about the current object without changing it: its security flavors and a
/// comparison of its attributes.
async fn query_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::SECINFO_NO_NAME => secinfo_no_name(reader, frame),
    op::SECINFO => secinfo(backend, reader, frame).await,
    op::VERIFY | op::NVERIFY => verify(backend, opnum, reader, frame).await,
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
    _ => file_state_operation(backend, opnum, reader, frame).await,
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

/// `SECINFO` (§18.29): the flavors that protect `name` in the current directory, which the name must
/// exist in and the caller must be able to look it up in (the same LOOKUP, so its refusal is the
/// same); on success the current file handle is consumed (§2.6.3.1.1.8).
async fn secinfo<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let name = component(reader)?;
  let dir = current(frame)?.clone();
  lookup(backend, &dir, &name).await?;
  frame.current = None;
  let mut body = XdrWriter::new();
  body.u32(1);
  body.u32(AUTH_SYS);
  Ok(body.into_bytes())
}

/// `VERIFY` (§18.31) and `NVERIFY` (§18.15): whether the attributes the client names equal the current
/// object's. The server encodes its own values for the same bitmap and compares the encodings, as the
/// attribute list is defined by them: VERIFY answers `NFS4ERR_NOT_SAME` on a difference, NVERIFY
/// `NFS4ERR_SAME` on none. A write-only attribute or `rdattr_error` is `NFS4ERR_INVAL`, one the server
/// does not support `NFS4ERR_ATTRNOTSUPP`.
async fn verify<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let requested = Bitmap::decode(reader).map_err(bad)?;
  // The list is borrowed from the received request, so the bytes that remain bound it.
  let remaining = reader.rest().len();
  let theirs = reader.opaque(remaining).map_err(bad)?;
  attr::check_readable(&requested)?;
  if requested.has(attr::number::RDATTR_ERROR) {
    return Err(Nfsstat4::Inval);
  }
  if requested.intersect(attr::supported(frame.minor)) != requested {
    return Err(Nfsstat4::Attrnotsupp);
  }
  let fh = current(frame)?.clone();
  let ours = getattr(backend, &fh, (&requested, frame.minor)).await?;
  let mut ours = XdrReader::new(&ours);
  Bitmap::decode(&mut ours).map_err(|_| Nfsstat4::Serverfault)?;
  let encoded = ours.rest().len();
  let same = ours.opaque(encoded).map_err(|_| Nfsstat4::Serverfault)? == theirs;
  match (opnum == op::VERIFY, same) {
    (true, true) | (false, false) => Ok(Vec::new()),
    (true, false) => Err(Nfsstat4::NotSame),
    (false, true) => Err(Nfsstat4::Same),
  }
}

/// `BACKCHANNEL_CTL` (§18.33): the backchannel's program and security. This server makes no callbacks,
/// so it holds no RPCSEC_GSS handle for one to name (`NFS4ERR_NOENT`); an `AUTH_NONE` or `AUTH_SYS`
/// setting is accepted.
fn backchannel_ctl(reader: &mut XdrReader<'_>) -> Outcome {
  let _cb_program = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  if read_callback_sec(reader)?.1 {
    return Err(Nfsstat4::Noent);
  }
  Ok(Vec::new())
}

/// `SET_SSV` (§18.47): refused `NFS4ERR_INVAL`, as it must be for a client that did not choose SP4_SSV
/// state protection — which no client of this server can (EXCHANGE_ID establishes SP4_NONE only).
fn set_ssv(reader: &mut XdrReader<'_>) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // ssa_ssv
  reader.opaque(OPAQUE_LIMIT).map_err(bad)?; // ssa_digest
  Err(Nfsstat4::Inval)
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
    op::RECLAIM_COMPLETE => {
      let _one_fs = reader.bool().map_err(|_| Nfsstat4::Badxdr)?;
      let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
      server
        .sessions
        .reclaim_complete(clientid)
        .map(|()| Vec::new())
    }
    op::BACKCHANNEL_CTL => backchannel_ctl(reader),
    op::SET_SSV => set_ssv(reader),
    op::ILLEGAL => Err(Nfsstat4::OpIllegal),
    _ if is_operation(opnum) => Err(Nfsstat4::Notsupp),
    _ => Err(Nfsstat4::OpIllegal),
  }
}

/// A file state extension to the owner of `fh` (§4.6 A-36): the body after its status, or the status
/// (with the body a `NFS4ERR_DENIED` carries).
async fn state_call<B: Backend>(
  backend: &mut B,
  procedure: u32,
  fh: &Nfsfh3,
  clientid: u64,
  extra: &[u8],
) -> Result<Vec<u8>, (Nfsstat4, Vec<u8>)> {
  let mut args = XdrWriter::new();
  fh.encode(&mut args);
  args.u64(clientid);
  args.fixed(extra);
  let result = backend.call_v3(procedure, args.into_bytes()).await;
  let mut reader = XdrReader::new(&result);
  let status = reader
    .u32()
    .map_err(|_| (Nfsstat4::Serverfault, Vec::new()))?;
  let body = reader.rest().to_vec();
  match Nfsstat4::from_wire(status) {
    Some(Nfsstat4::Ok) => Ok(body),
    Some(refused) => Err((refused, body)),
    None => Err((Nfsstat4::Serverfault, Vec::new())),
  }
}

/// Whether `stateid` may serve I/O of kind `want` (`procedures::io_want`) on `fh` for the compound's
/// client, checked at the file's owner (§4.6 A-36; RFC 8881 §9.1.2): the access mode of an open, and
/// every other open's share deny for a special state id, which holds none of its own.
pub(super) async fn check_state<B: Backend>(
  backend: &mut B,
  fh: &Nfsfh3,
  clientid: Option<u64>,
  stateid: &Stateid,
  want: u32,
) -> Result<(), Nfsstat4> {
  // A special state id names no client's state, so no session client is needed to check it.
  let clientid = match clientid {
    Some(clientid) => clientid,
    None if stateid.is_special() => 0,
    None => return Err(Nfsstat4::OpNotInSession),
  };
  let mut extra = XdrWriter::new();
  stateid.encode(&mut extra);
  extra.u32(want);
  state_call(
    backend,
    extension::STATE_CHECK,
    fh,
    clientid,
    extra.as_slice(),
  )
  .await
  .map(|_| ())
  .map_err(|(status, _)| status)
}

/// A state-carrying I/O at the file's owner (§4.6 A-36): the NFSv3 procedure's arguments with the
/// client id and state id after them; the NFSv3 result, or the state refusal.
pub(super) async fn state_io<B: Backend>(
  backend: &mut B,
  procedure: u32,
  mut v3_args: Vec<u8>,
  clientid: Option<u64>,
  stateid: &Stateid,
) -> Result<Vec<u8>, Nfsstat4> {
  let mut suffix = XdrWriter::new();
  suffix.u64(clientid.unwrap_or(0));
  stateid.encode(&mut suffix);
  v3_args.extend_from_slice(suffix.as_slice());
  let result = backend.call_v3(procedure, v3_args).await;
  let mut reader = XdrReader::new(&result);
  let status = reader.u32().map_err(|_| Nfsstat4::Serverfault)?;
  match Nfsstat4::from_wire(status) {
    Some(Nfsstat4::Ok) => Ok(reader.rest().to_vec()),
    Some(refused) => Err(refused),
    None => Err(Nfsstat4::Serverfault),
  }
}

/// A state id's encoding, as an owner call's extra arguments.
fn stateid_bytes(stateid: &Stateid) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  stateid.encode(&mut writer);
  writer.into_bytes()
}

/// A state id at the head of an owner's reply.
fn stateid_of(reader: &mut XdrReader<'_>) -> Result<Stateid, Nfsstat4> {
  Stateid::decode(reader).map_err(|_| Nfsstat4::Serverfault)
}

/// The operations on file state, served at the files' owners (§4.6 A-36), and the rest by family.
async fn file_state_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  match opnum {
    op::CLOSE => close(backend, reader, frame).await,
    op::DELEGRETURN => delegreturn(backend, reader, frame).await,
    op::OPEN_DOWNGRADE => open_downgrade(backend, reader, frame).await,
    op::LOCK | op::LOCKT | op::LOCKU => lock_operation(backend, opnum, reader, frame).await,
    op::TEST_STATEID => test_stateid(backend, reader, frame).await,
    op::FREE_STATEID => free_stateid(backend, reader, frame).await,
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

/// DELEGRETURN (§18.6) at the file's owner (A-78): the delegation given back, its file's gate opened when it was the
/// last.
async fn delegreturn<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  state_call(
    backend,
    extension::STATE_DELEGRETURN,
    &fh,
    clientid,
    &stateid_bytes(&stateid),
  )
  .await
  .map(|_| Vec::new())
  .map_err(|(status, _)| status)
}

/// CLOSE (§18.2) at the file's owner, which drops the lock states that held no lock with the open.
async fn close<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let _seqid = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  let body = state_call(
    backend,
    extension::STATE_CLOSE,
    &fh,
    clientid,
    &stateid_bytes(&stateid),
  )
  .await
  .map_err(|(status, _)| status)?;
  Ok(stateid_bytes(&stateid_of(&mut XdrReader::new(&body))?))
}

/// OPEN_DOWNGRADE (§18.18) at the file's owner.
async fn open_downgrade<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let _seqid = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let access = reader.u32().map_err(|_| Nfsstat4::Badxdr)? & share::BOTH;
  let deny = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  let mut extra = XdrWriter::new();
  stateid.encode(&mut extra);
  extra.u32(access);
  extra.u32(deny);
  let body = state_call(
    backend,
    extension::STATE_DOWNGRADE,
    &fh,
    clientid,
    extra.as_slice(),
  )
  .await
  .map_err(|(status, _)| status)?;
  Ok(stateid_bytes(&stateid_of(&mut XdrReader::new(&body))?))
}

/// LOCK, LOCKT and LOCKU (§18.10–18.12): the arguments are checked here (an undefined type, an invalid
/// range, a reclaim), then sent as they came to the file's owner, which keeps every lock of the file.
async fn lock_operation<B: Backend>(
  backend: &mut B,
  opnum: u32,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let start = reader.rest();
  LockRequest::decode(opnum, reader)?;
  let raw = start
    .get(..start.len().saturating_sub(reader.rest().len()))
    .unwrap_or_default()
    .to_vec();
  let fh = current(frame)?.clone();
  check_open_kind(backend, &fh).await?;
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  let procedure = match opnum {
    op::LOCK => extension::STATE_LOCK,
    op::LOCKT => extension::STATE_LOCKT,
    _ => extension::STATE_LOCKU,
  };
  match state_call(backend, procedure, &fh, clientid, &raw).await {
    Ok(body) => Ok(body),
    Err((Nfsstat4::Denied, denied)) => {
      frame.denied = Some(denied);
      Err(Nfsstat4::Denied)
    }
    Err((status, _)) => Err(status),
  }
}

/// TEST_STATEID (§18.48): each state id tested at the owner its `other` names (D-14).
async fn test_stateid<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  /// Format: the most state ids one TEST_STATEID reads (a client tests a handful).
  const MAX_TESTED: u32 = 1024;
  let count = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  if count > MAX_TESTED {
    return Err(Nfsstat4::Badxdr);
  }
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  let mut body = XdrWriter::new();
  body.u32(count);
  for _ in 0..count {
    let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
    let owner = super::files::owner_of(&stateid.other);
    let status = match owner_call(
      backend,
      owner,
      extension::STATE_TEST,
      clientid,
      Some(&stateid),
    )
    .await
    {
      Ok(_) => Nfsstat4::Ok,
      Err(status) => status,
    };
    body.u32(status.wire());
  }
  Ok(body.into_bytes())
}

/// FREE_STATEID (§18.38) at the owner its `other` names (D-14).
async fn free_stateid<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  let owner = super::files::owner_of(&stateid.other);
  owner_call(
    backend,
    owner,
    extension::STATE_FREE,
    clientid,
    Some(&stateid),
  )
  .await?;
  Ok(Vec::new())
}

/// An id-only state procedure at the owner partition `owner`: the body after its status, or the
/// status.
async fn owner_call<B: Backend>(
  backend: &mut B,
  owner: u16,
  procedure: u32,
  clientid: u64,
  stateid: Option<&Stateid>,
) -> Result<Vec<u8>, Nfsstat4> {
  let mut args = XdrWriter::new();
  args.u64(clientid);
  if let Some(stateid) = stateid {
    stateid.encode(&mut args);
  }
  let result = backend
    .call_owner(owner, procedure, args.into_bytes())
    .await;
  let mut reader = XdrReader::new(&result);
  let status = reader.u32().map_err(|_| Nfsstat4::Serverfault)?;
  match Nfsstat4::from_wire(status) {
    Some(Nfsstat4::Ok) => Ok(reader.rest().to_vec()),
    Some(refused) => Err(refused),
    None => Err(Nfsstat4::Serverfault),
  }
}

/// Tells every owner to drop the file state of each client the session table dropped. An owner that
/// cannot be reached keeps its purge queued, retried on the next drop, so no dead client's state is
/// forgotten.
async fn purge_dropped<B: Backend>(backend: &mut B) -> Result<(), Nfsstat4> {
  let owners = backend.owners();
  let purges = backend.with_v4(|server| {
    let mut purges = std::mem::take(&mut server.pending_purges);
    for clientid in server.take_dropped() {
      purges.extend(owners.iter().map(|owner| (clientid, *owner)));
    }
    purges
  })?;
  let mut failed = Vec::new();
  for (clientid, owner) in purges {
    if owner_call(backend, owner, extension::STATE_PURGE, clientid, None)
      .await
      .is_err()
    {
      failed.push((clientid, owner));
    }
  }
  backend.with_v4(|server| server.pending_purges.extend(failed))
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
) -> Result<v3call::Looked, Nfsstat4> {
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
  requested: &Bitmap,
) -> Result<FsFigures, Nfsstat4> {
  /// Format: nanoseconds per second.
  const NS_PER_SECOND: u64 = 1_000_000_000;
  let limits = backend.with_v4(|server| server.limits)?;
  let mut figures = FsFigures {
    lease_seconds: u32::try_from(limits.lease_ns / NS_PER_SECOND).unwrap_or(u32::MAX),
    max_file_size: u64::MAX,
    max_link: u32::MAX,
    max_name: u32::try_from(slates_vfs::names::NAME_MAX).unwrap_or(u32::MAX),
    // The v3 layer's transfer ceiling: what a READ or WRITE carries (the session is sized to hold one
    // with its compound's overhead).
    max_io: u64::from(crate::procedures::MAX_TRANSFER),
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
async fn getattr<B: Backend>(
  backend: &mut B,
  fh: &Nfsfh3,
  (requested, minor): (&Bitmap, u32),
) -> Outcome {
  let attrs = attrs_of(backend, fh).await?;
  let figures = figures(backend, fh, requested).await?;
  let mut body = XdrWriter::new();
  attr::encode((requested, minor), &attrs, fh, &figures, &mut body)?;
  Ok(body.into_bytes())
}

/// `READ` (§18.22).
async fn read<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let stateid = Stateid::decode(reader).map_err(|_| Nfsstat4::Badxdr)?;
  let offset = reader.u64().map_err(|_| Nfsstat4::Badxdr)?;
  let count = reader.u32().map_err(|_| Nfsstat4::Badxdr)?;
  let fh = current(frame)?.clone();
  let result = state_io(
    backend,
    extension::READ_STATE,
    v3call::read_args(&fh, offset, count),
    frame.clientid,
    &stateid,
  )
  .await?;
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
  let result = state_io(
    backend,
    extension::WRITE_STATE,
    v3call::write_args(&fh, offset, stable, data),
    frame.clientid,
    &stateid,
  )
  .await?;
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

/// `READDIR` (§18.23): a directory in a volume is listed by its owner, which encodes the page from the bridge's rows
/// (`extension::READDIR4`, A-95, `super::listing`); the pseudo-root, whose entries are volumes on other shards, is
/// listed from the gathered v3 READDIRPLUS (`readdir_of_root`).
async fn readdir<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &Frame,
) -> Outcome {
  let dir = current(frame)?.clone();
  if crate::multi::is_root_handle(&dir) {
    return readdir_of_root(backend, reader, frame).await;
  }
  let bad = |_| Nfsstat4::Badxdr;
  let cookie = reader.u64().map_err(bad)?;
  let mut verf = [0u8; VERIFIER_SIZE];
  verf.copy_from_slice(reader.fixed(VERIFIER_SIZE).map_err(bad)?);
  let _dircount = reader.u32().map_err(bad)?;
  let maxcount = reader.u32().map_err(bad)?;
  let requested = Bitmap::decode(reader).map_err(bad)?;
  attr::check_readable(&requested)?;
  if cookie == 1 || cookie == 2 {
    return Err(Nfsstat4::BadCookie);
  }
  // Every entry of a volume's directory is on the volume's filesystem, so one set of figures serves the page.
  let figures = figures(backend, &dir, &requested).await?;
  let request = super::listing::PageRequest {
    dir,
    cookie: cookie.saturating_sub(COOKIE_SHIFT),
    verf,
    maxcount,
    requested,
    minor: frame.minor,
    figures,
  };
  let result = backend.call_v3(extension::READDIR4, request.encode()).await;
  super::listing::page_of(result)
}

/// `READDIR` of the pseudo-root: a v3 READDIRPLUS of the gathered volumes, the dot entries dropped, the cookies
/// shifted past the reserved values, each entry's attributes those asked for, the whole bounded by `maxcount`.
async fn readdir_of_root<B: Backend>(
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
  attr::check_readable(&requested)?;
  if cookie == 1 || cookie == 2 {
    return Err(Nfsstat4::BadCookie);
  }
  let dir = current(frame)?.clone();
  let v3_cookie = cookie.saturating_sub(COOKIE_SHIFT);
  // The page is built from a v3 READDIRPLUS, whose entries are larger than the v4 entries a client lists with
  // (a whole `fattr3` and a handle each). Its budget is the client's plus the most every entry the page could
  // hold exceeds its v4 form, so the v3 budget never ends the page before `maxcount` does (A-90: under the bare
  // `maxcount` a page of `ls` entries was a quarter full, and a listing took four times the round trips).
  // At most as many entries as the smallest this request can encode to fill `maxcount` with: value-follows, the
  // cookie, the shortest name, and the requested attributes of a minimal object (A-90: counted at `ENTRY_FIXED` alone,
  // the v3 page fetched, stated and encoded twice the entries an `ls` page carries).
  let entry_floor = super::listing::entry_floor(&requested, frame.minor);
  let fit = u32::try_from(usize::try_from(maxcount).unwrap_or(usize::MAX) / entry_floor.max(1))
    .unwrap_or(u32::MAX);
  let v3_maxcount = maxcount
    .saturating_add(fit.saturating_mul(V3_ENTRY_EXCESS))
    .saturating_add(V3_REPLY_OVERHEAD);
  let result = backend
    .call_v3(
      NFSPROC3_READDIRPLUS,
      v3call::readdirplus_args(
        &dir,
        v3_cookie,
        verf,
        dircount.max(v3_maxcount),
        v3_maxcount,
      ),
    )
    .await;
  let max_entries = usize::try_from(fit).unwrap_or(0).max(1);
  let listing = v3(v3call::readdirplus(&result, max_entries.saturating_mul(2)))?;
  // The filesystem-wide figures, once per filesystem the entries are on: a volume root listed in the
  // pseudo-root is on its own volume, not the root's.
  let mut figures_by_fsid: Vec<(u64, FsFigures)> = Vec::new();
  let budget = usize::try_from(maxcount).unwrap_or(usize::MAX);
  // The page is built in one buffer sized to the reply it may become, and each entry in one reused buffer (A-90: a
  // writer per entry and a growing page were a fifth of a page's time).
  let mut entries =
    XdrWriter::with_capacity(budget.min(listing.entries.len().saturating_mul(ENTRY_TYPICAL)));
  let mut one = XdrWriter::new();
  let mut returned = 0usize;
  let mut all = true;
  for entry in &listing.entries {
    if entry.name == "." || entry.name == ".." {
      continue;
    }
    let (attrs, fh) = entry_object(backend, &dir, entry).await?;
    let figures = match figures_by_fsid.iter().find(|(fsid, _)| *fsid == attrs.fsid) {
      Some((_, figures)) => *figures,
      None => {
        let figures = figures(backend, &fh, &requested).await?;
        figures_by_fsid.push((attrs.fsid, figures));
        figures
      }
    };
    one.clear();
    one.bool(true);
    one.u64(entry.cookie.saturating_add(COOKIE_SHIFT));
    one.opaque(entry.name.as_bytes());
    attr::encode((&requested, frame.minor), &attrs, &fh, &figures, &mut one)?;
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

/// A listed entry's attributes and handle: as the listing carried them, or — for an entry it carried
/// without (the pseudo-root lists a volume on another shard by name only) — from a LOOKUP of the name,
/// which routes to the volume's owner as any call does. An entry is never dropped from a listing.
async fn entry_object<B: Backend>(
  backend: &mut B,
  dir: &Nfsfh3,
  entry: &v3call::Entry,
) -> Result<(Fattr3, Nfsfh3), Nfsstat4> {
  if let (Some(attrs), Some(fh)) = (&entry.attrs, &entry.fh) {
    return Ok((*attrs, fh.clone()));
  }
  let looked = lookup(backend, dir, &entry.name).await?;
  let attrs = match looked.attrs {
    Some(attrs) => attrs,
    None => attrs_of(backend, &looked.fh).await?,
  };
  Ok((attrs, looked.fh))
}

/// `OPEN` (§18.16): a name in the current directory (`CLAIM_NULL`), created if asked, or the current
/// file itself (`CLAIM_FH`); an existing file's permission checked against the access asked; the share
/// checked against the file's other opens; a state id recorded; a read delegation granted when one is due (A-78).
async fn open<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let bad = |_| Nfsstat4::Badxdr;
  let _seqid = reader.u32().map_err(bad)?;
  // The low bits are the share; the high bits of the access word are delegation wishes (§18.16.3).
  let wishes = reader.u32().map_err(bad)?;
  let access = wishes & share::BOTH;
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
  let dir = current(frame)?.clone();
  let opened = open_claim(backend, reader, dir, create, access).await?;
  let clientid = frame.clientid.ok_or(Nfsstat4::OpNotInSession)?;
  let may_delegate = delegations_allowed(backend, frame, &opened.fh, wishes)?;
  // The open is recorded at the file's owner (§4.6 A-36), which grants a delegation when one is due (A-78).
  let mut extra = XdrWriter::new();
  extra.opaque(&owner);
  extra.u32(access);
  extra.u32(deny);
  extra.u32(may_delegate);
  extra.u64(backend.now_ns());
  let recorded = state_call(
    backend,
    extension::STATE_OPEN,
    &opened.fh,
    clientid,
    extra.as_slice(),
  )
  .await
  .map_err(|(status, _)| status)?;
  let mut recorded = XdrReader::new(&recorded);
  let stateid = stateid_of(&mut recorded)?;
  let delegation = match recorded.u32() {
    Ok(kind @ (OPEN_DELEGATE_READ | OPEN_DELEGATE_WRITE)) => {
      Some((kind, stateid_of(&mut recorded)?))
    }
    _ => None,
  };
  let mut body = XdrWriter::new();
  stateid.encode(&mut body);
  body.fixed(&change_info(&opened.dir));
  body.u32(OPEN4_RESULT_LOCKTYPE_POSIX);
  opened.attrset.encode(&mut body);
  match delegation {
    Some((OPEN_DELEGATE_WRITE, delegation)) => encode_write_delegation(&mut body, &delegation),
    Some((_, delegation)) => encode_read_delegation(&mut body, &delegation),
    None => body.u32(OPEN_DELEGATE_NONE),
  }
  frame.current = Some(opened.fh);
  Ok(body.into_bytes())
}

/// The file an `OPEN` names by its claim (§18.16.3), with `dir` the current file: a name in it (`CLAIM_NULL`, created
/// if asked), the current file itself (`CLAIM_FH`), or either under a delegation being returned
/// (`CLAIM_DELEGATE_CUR`, `CLAIM_DELEG_CUR_FH`); each checked against the access asked.
async fn open_claim<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  dir: Nfsfh3,
  create: Option<OpenCreate>,
  access: u32,
) -> Result<Opened, Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  let opened = match reader.u32().map_err(bad)? {
    claim::NULL => {
      let name = component(reader)?;
      open_by_name(backend, &dir, &name, create, access).await?
    }
    // A re-open under a delegation being returned (§10.4.4) is an open of the same name or file: the delegation's
    // state id rides along, and the open is checked as any other.
    claim::DELEGATE_CUR => {
      Stateid::decode(reader).map_err(bad)?;
      let name = component(reader)?;
      open_by_name(backend, &dir, &name, None, access).await?
    }
    claim::DELEG_CUR_FH => {
      Stateid::decode(reader).map_err(bad)?;
      check_open_kind(backend, &dir).await?;
      check_open_access(backend, &dir, access).await?;
      Opened {
        fh: dir,
        attrset: Bitmap::default(),
        dir: Wcc::default(),
      }
    }
    claim::FH => {
      check_open_kind(backend, &dir).await?;
      check_open_access(backend, &dir, access).await?;
      Opened {
        fh: dir,
        attrset: Bitmap::default(),
        // No directory is named: the change info says nothing.
        dir: Wcc::default(),
      }
    }
    _ => return Err(Nfsstat4::Notsupp),
  };
  Ok(opened)
}

/// The delegations an open may be granted (A-78, A-80), as [`MAY_DELEGATE_READ`] and [`MAY_DELEGATE_WRITE`] bits:
/// none unless the session's back channel has answered (RFC 8881 §10.2: never before it is known to exist), the
/// client did not ask for no delegation, and the file's owner is the shard serving the session, so its recall,
/// revocation and the revocation's notice all happen where the session lives; a read one only when the client asked
/// for that kind.
fn delegations_allowed<B: Backend>(
  backend: &mut B,
  frame: &Frame,
  fh: &Nfsfh3,
  wishes: u32,
) -> Result<u32, Nfsstat4> {
  let wish = wishes & OPEN4_SHARE_ACCESS_WANT_DELEG_MASK;
  if wish == OPEN4_SHARE_ACCESS_WANT_NO_DELEG || !backend.owns_file(fh) {
    return Ok(0);
  }
  let Some(sessionid) = frame.session else {
    return Ok(0);
  };
  let answering = backend.with_v4(|server| {
    server
      .sessions
      .back_channel(&sessionid)
      .is_some_and(|back| back.state == super::session::CallbackState::Up)
  })?;
  Ok(match (answering, wish) {
    (false, _) => 0,
    (true, OPEN4_SHARE_ACCESS_WANT_READ_DELEG) => MAY_DELEGATE_READ,
    (true, _) => MAY_DELEGATE_READ | MAY_DELEGATE_WRITE,
  })
}

/// Writes an `open_delegation4` granting a read delegation (§18.16.2): its state id, no recall pending, and an
/// `nfsace4` that grants nothing, so the client still asks ACCESS for each user (§10.4: "the server may return an
/// nfsace4 that is more restrictive than the actual ACL ... including ... denial of all access").
fn encode_read_delegation(body: &mut XdrWriter, delegation: &Stateid) {
  body.u32(OPEN_DELEGATE_READ);
  delegation.encode(body);
  body.bool(false); // recall
  body.u32(ACE4_ACCESS_ALLOWED_ACE_TYPE);
  body.u32(0); // flags
  body.u32(0); // access mask: nothing granted by the delegation itself
  body.opaque(b"EVERYONE@");
}

/// Writes an `open_delegation4` granting a write delegation (§18.16.2): its state id, no recall pending, a space limit
/// of zero bytes (`NFS_LIMIT_SIZE`, filesize 0: §10.4.1's limit "that will always force modified data to be flushed
/// to the server on close", as Linux nfsd encodes it; A-80), and the same `nfsace4` as a read delegation.
fn encode_write_delegation(body: &mut XdrWriter, delegation: &Stateid) {
  body.u32(OPEN_DELEGATE_WRITE);
  delegation.encode(body);
  body.bool(false); // recall
  body.u32(NFS_LIMIT_SIZE);
  body.u64(0); // filesize
  body.u32(ACE4_ACCESS_ALLOWED_ACE_TYPE);
  body.u32(0); // flags
  body.u32(0); // access mask: nothing granted by the delegation itself
  body.opaque(b"EVERYONE@");
}

/// Format: the v3 ACCESS bits an open's share needs: read data (`ACCESS3_READ`) and change it
/// (`ACCESS3_MODIFY`), RFC 1813 §3.3.4.
mod access3 {
  /// Format: `ACCESS3_READ`.
  pub(super) const READ: u32 = 0x1;
  /// Format: `ACCESS3_MODIFY`.
  pub(super) const MODIFY: u32 = 0x4;
}

/// What `OPEN` opened: the file, the attributes the open itself set (its `attrset`), and its
/// directory's `wcc_data` for the reply's `change_info4`.
struct Opened {
  fh: Nfsfh3,
  attrset: Bitmap,
  dir: Wcc,
}

/// Opens `name` in `dir`. A create that makes the file sets its attributes and needs no permission on
/// the new file (POSIX: `open(O_CREAT|O_WRONLY)` of mode 0444 succeeds). An `UNCHECKED` create of a name
/// that exists opens it, applying only the size (the truncate `O_TRUNC` asks for) after checking the
/// access asked — also when another client creates the name between this open's LOOKUP and its create.
/// A guarded create of a name that exists is `NFS4ERR_EXIST`. An exclusive create is decided by the v3
/// layer in one call: its own earlier create (the verifier's times) opens, any other name is
/// `NFS4ERR_EXIST`.
async fn open_by_name<B: Backend>(
  backend: &mut B,
  dir: &Nfsfh3,
  name: &str,
  create: Option<OpenCreate>,
  access: u32,
) -> Result<Opened, Nfsstat4> {
  let attrs = match create {
    Some(OpenCreate::Exclusive { verifier, attrs }) => {
      return exclusive_open(backend, dir, name, verifier, attrs).await;
    }
    Some(OpenCreate::Unchecked(attrs) | OpenCreate::Guarded(attrs)) => Some(attrs),
    None => None,
  };
  let guarded = matches!(create, Some(OpenCreate::Guarded(_)));
  match (lookup(backend, dir, name).await, attrs) {
    (Ok(_), Some(_)) if guarded => Err(Nfsstat4::Exist),
    (Ok(looked), attrs) => open_existing(backend, looked, access, attrs).await,
    (Err(Nfsstat4::Noent), Some(attrs)) => {
      create_or_open(backend, (dir, name), attrs, access, guarded).await
    }
    (Err(status), _) => Err(status),
  }
}

/// Creates `name` in `dir`, guarded; for an unchecked open, a name another client created since this
/// open's LOOKUP is opened instead, as UNCHECKED4 requires (RFC 8881 §18.16.3), once — the name exists
/// on that second look, or its LOOKUP's refusal is the answer.
async fn create_or_open<B: Backend>(
  backend: &mut B,
  (dir, name): (&Nfsfh3, &str),
  attrs: Sattr3,
  access: u32,
  guarded: bool,
) -> Result<Opened, Nfsstat4> {
  match create_new(backend, dir, name, attrs).await {
    Err(Nfsstat4::Exist) if !guarded => {
      let looked = lookup(backend, dir, name).await?;
      open_existing(backend, looked, access, Some(attrs)).await
    }
    made => made,
  }
}

/// Opens an existing file: its kind and the caller's access checked, then the size an unchecked create
/// carried applied (`O_TRUNC`). No directory changed, and the directory's attributes came from the
/// LOOKUP, a call before this one: another client may have changed it since, so the change info is
/// not atomic.
async fn open_existing<B: Backend>(
  backend: &mut B,
  looked: v3call::Looked,
  access: u32,
  create: Option<Sattr3>,
) -> Result<Opened, Nfsstat4> {
  let fh = looked.fh;
  check_open_kind(backend, &fh).await?;
  check_open_access(backend, &fh, access).await?;
  let truncate = Sattr3 {
    size: create.and_then(|attrs| attrs.size),
    ..Sattr3::default()
  };
  if truncate.size.is_some() {
    let result = backend
      .call_v3(NFSPROC3_SETATTR, v3call::setattr_args(&fh, &truncate))
      .await;
    let (status, _) = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
    if status != Nfsstat3::Ok {
      return Err(Nfsstat4::of_v3(status));
    }
  }
  Ok(Opened {
    fh,
    attrset: set_bits(&truncate),
    dir: Wcc {
      pre: None,
      post: looked.dir,
    },
  })
}

/// Creates `name` in `dir` with `attrs`, guarded: `NFS4ERR_EXIST` if the name exists.
async fn create_new<B: Backend>(
  backend: &mut B,
  dir: &Nfsfh3,
  name: &str,
  attrs: Sattr3,
) -> Result<Opened, Nfsstat4> {
  let result = backend
    .call_v3(
      NFSPROC3_CREATE,
      v3call::create_args(dir, name, &v3call::CreateHow::Guarded(attrs)),
    )
    .await;
  let (fh, dir_wcc) = match v3(v3call::created(&result))? {
    (Some(fh), dir_wcc) => (fh, dir_wcc),
    (None, dir_wcc) => (lookup(backend, dir, name).await?.fh, dir_wcc),
  };
  Ok(Opened {
    fh,
    attrset: set_bits(&attrs),
    dir: dir_wcc,
  })
}

/// An exclusive create (EXCLUSIVE4 or EXCLUSIVE4_1): the v3 EXCLUSIVE create, which keeps the verifier
/// in the new file's times and opens a retry's own file, then `attrs` applied (they exclude the times).
async fn exclusive_open<B: Backend>(
  backend: &mut B,
  dir: &Nfsfh3,
  name: &str,
  verifier: [u8; VERIFIER_SIZE],
  attrs: Sattr3,
) -> Result<Opened, Nfsstat4> {
  let result = backend
    .call_v3(
      NFSPROC3_CREATE,
      v3call::create_args(dir, name, &v3call::CreateHow::Exclusive(verifier)),
    )
    .await;
  let (fh, dir_wcc) = match v3(v3call::created(&result))? {
    (Some(fh), dir_wcc) => (fh, dir_wcc),
    (None, dir_wcc) => (lookup(backend, dir, name).await?.fh, dir_wcc),
  };
  if attrs != Sattr3::default() {
    let result = backend
      .call_v3(NFSPROC3_SETATTR, v3call::setattr_args(&fh, &attrs))
      .await;
    let (status, _) = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
    if status != Nfsstat3::Ok {
      return Err(Nfsstat4::of_v3(status));
    }
  }
  Ok(Opened {
    fh,
    // The times hold the verifier: naming them tells the client to set its own (§18.16.3).
    attrset: Bitmap::of(
      &set_bits(&attrs)
        .bits()
        .chain([attr::number::TIME_ACCESS_SET, attr::number::TIME_MODIFY_SET])
        .collect::<Vec<_>>(),
    ),
    dir: dir_wcc,
  })
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
fn open_createhow(reader: &mut XdrReader<'_>) -> Result<OpenCreate, Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  let verifier = |reader: &mut XdrReader<'_>| {
    let mut verifier = [0u8; VERIFIER_SIZE];
    verifier.copy_from_slice(reader.fixed(VERIFIER_SIZE).map_err(bad)?);
    Ok::<_, Nfsstat4>(verifier)
  };
  match reader.u32().map_err(bad)? {
    createmode4::UNCHECKED => Ok(OpenCreate::Unchecked(decode_fattr_set(reader)?)),
    createmode4::GUARDED => Ok(OpenCreate::Guarded(decode_fattr_set(reader)?)),
    createmode4::EXCLUSIVE => Ok(OpenCreate::Exclusive {
      verifier: verifier(reader)?,
      attrs: Sattr3::default(),
    }),
    createmode4::EXCLUSIVE_1 => {
      let verifier = verifier(reader)?;
      let attrs = decode_fattr_set(reader)?;
      // The times hold the verifier, so `suppattr_exclcreat` leaves them out; setting one here is
      // outside it (RFC 8881 §18.16.3).
      if attrs.atime.is_some() || attrs.mtime.is_some() {
        return Err(Nfsstat4::Inval);
      }
      Ok(OpenCreate::Exclusive { verifier, attrs })
    }
    _ => Err(Nfsstat4::Badxdr),
  }
}

/// How an OPEN creates (`createhow4`, RFC 8881 §18.16.1).
enum OpenCreate {
  /// Create, or open an existing file, applying only the size then (the `O_TRUNC`).
  Unchecked(Sattr3),
  /// Create; an existing name is `NFS4ERR_EXIST`.
  Guarded(Sattr3),
  /// Create keyed by the verifier (kept in the new file's times, `procedures::exclusive_times`), so a
  /// retry of the same create — even after a daemon restart — opens the file it made; then set `attrs`.
  Exclusive {
    verifier: [u8; VERIFIER_SIZE],
    attrs: Sattr3,
  },
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
      attr::number::SIZE => attrs.size = Some(values.u64().map_err(bad)?),
      attr::number::MODE => attrs.mode = Some(values.u32().map_err(bad)?),
      attr::number::OWNER => attrs.uid = Some(numeric_id(&mut values)?),
      attr::number::OWNER_GROUP => attrs.gid = Some(numeric_id(&mut values)?),
      attr::number::TIME_ACCESS_SET => attrs.atime = Some(settime(&mut values)?),
      attr::number::TIME_MODIFY_SET => attrs.mtime = Some(settime(&mut values)?),
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
fn settime(values: &mut XdrReader<'_>) -> Result<Option<Nfstime4>, Nfsstat4> {
  /// Format: nanoseconds per second, the bound of `nfstime4`'s `nseconds`.
  const NS_PER_SECOND: u32 = 1_000_000_000;
  let bad = |_| Nfsstat4::Badxdr;
  if values.u32().map_err(bad)? != SET_TO_CLIENT_TIME4 {
    return Ok(None);
  }
  let time = Nfstime4::decode(values).map_err(bad)?;
  if time.nseconds >= NS_PER_SECOND {
    return Err(Nfsstat4::Inval);
  }
  Ok(Some(time))
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
  let result = state_io(
    backend,
    extension::SETATTR_STATE,
    v3call::setattr_args(&fh, &attrs),
    frame.clientid,
    &stateid,
  )
  .await?;
  let (status, _) = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
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
    (attrs.size.is_some(), attr::number::SIZE),
    (attrs.mode.is_some(), attr::number::MODE),
    (attrs.uid.is_some(), attr::number::OWNER),
    (attrs.gid.is_some(), attr::number::OWNER_GROUP),
    (attrs.atime.is_some(), attr::number::TIME_ACCESS_SET),
    (attrs.mtime.is_some(), attr::number::TIME_MODIFY_SET),
  ] {
    if present {
      set.push(bit);
    }
  }
  Bitmap::of(&set)
}

/// What a `CREATE` makes (`createtype4`, RFC 8881 §18.4.1).
enum Creation {
  /// A directory, through the v3 MKDIR.
  Dir,
  /// A symbolic link to the target, through the v3 SYMLINK.
  Link(String),
  /// A FIFO, socket, block or character device name, through the v3 MKNOD, which decides what the
  /// volume keeps (§4.6 A-26: FIFO and socket names; no device nodes).
  Node(Ftype3, Specdata3),
}

/// Reads a `createtype4`. A regular file is made by OPEN, never CREATE, and the named-attribute
/// types are not served: both are `NFS4ERR_BADTYPE` (§18.4.3).
fn decode_creation(reader: &mut XdrReader<'_>) -> Result<Creation, Nfsstat4> {
  let bad = |_| Nfsstat4::Badxdr;
  let kind = reader.u32().map_err(bad)?;
  match attr::ftype3_of(kind) {
    Some(Ftype3::Dir) => Ok(Creation::Dir),
    Some(Ftype3::Lnk) => Ok(Creation::Link(
      reader
        .string(crate::procedures::NFS_MAXPATHLEN)
        .map_err(bad)?
        .to_owned(),
    )),
    Some(kind @ (Ftype3::Blk | Ftype3::Chr)) => {
      let specdata1 = reader.u32().map_err(bad)?;
      let specdata2 = reader.u32().map_err(bad)?;
      Ok(Creation::Node(
        kind,
        Specdata3 {
          specdata1,
          specdata2,
        },
      ))
    }
    Some(kind @ (Ftype3::Sock | Ftype3::Fifo)) => Ok(Creation::Node(kind, Specdata3::default())),
    Some(Ftype3::Reg) | None => Err(Nfsstat4::Badtype),
  }
}

/// `CREATE` (§18.4): a directory, a symbolic link, or a FIFO, socket or device name in the current
/// directory, which becomes the new object. The reply's `attrset` names the attributes applied: the
/// v3 layer applies every requested one or refuses the call.
async fn create<B: Backend>(
  backend: &mut B,
  reader: &mut XdrReader<'_>,
  frame: &mut Frame,
) -> Outcome {
  let creation = decode_creation(reader)?;
  let name = component(reader)?;
  let attrs = decode_fattr_set(reader)?;
  let dir = current(frame)?.clone();
  let result = match &creation {
    Creation::Dir => {
      backend
        .call_v3(NFSPROC3_MKDIR, v3call::mkdir_args(&dir, &name, &attrs))
        .await
    }
    Creation::Link(target) => {
      backend
        .call_v3(
          NFSPROC3_SYMLINK,
          v3call::symlink_args(&dir, &name, &attrs, target),
        )
        .await
    }
    Creation::Node(kind, device) => {
      backend
        .call_v3(
          NFSPROC3_MKNOD,
          v3call::mknod_args(&dir, &name, *kind, &attrs, *device),
        )
        .await
    }
  };
  let (fh, dir_wcc) = match v3(v3call::created(&result))? {
    (Some(fh), dir_wcc) => (fh, dir_wcc),
    (None, dir_wcc) => (lookup(backend, &dir, &name).await?.fh, dir_wcc),
  };
  frame.current = Some(fh);
  let mut body = change_info(&dir_wcc);
  let mut attrset = XdrWriter::new();
  set_bits(&attrs).encode(&mut attrset);
  body.extend(attrset.into_bytes());
  Ok(body)
}

/// `REMOVE` (§18.25): a file or an empty directory.
async fn remove<B: Backend>(backend: &mut B, reader: &mut XdrReader<'_>, frame: &Frame) -> Outcome {
  let name = component(reader)?;
  let dir = current(frame)?.clone();
  let result = backend
    .call_v3(NFSPROC3_REMOVE, v3call::dir_name(&dir, &name))
    .await;
  let mut removed = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
  if removed.0 == Nfsstat3::Isdir {
    let result = backend
      .call_v3(NFSPROC3_RMDIR, v3call::dir_name(&dir, &name))
      .await;
    removed = v3call::wcc_status(&result).map_err(|_| Nfsstat4::Serverfault)?;
  }
  let (status, dir_wcc) = removed;
  if status != Nfsstat3::Ok {
    return Err(Nfsstat4::of_v3(status));
  }
  Ok(change_info(&dir_wcc))
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
  let (status, from_wcc, to_wcc) = v3call::renamed(&result).map_err(|_| Nfsstat4::Serverfault)?;
  if status != Nfsstat3::Ok {
    return Err(Nfsstat4::of_v3(status));
  }
  let mut body = change_info(&from_wcc);
  body.extend(change_info(&to_wcc));
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
  let (status, dir_wcc) = v3call::linked(&result).map_err(|_| Nfsstat4::Serverfault)?;
  if status != Nfsstat3::Ok {
    return Err(Nfsstat4::of_v3(status));
  }
  Ok(change_info(&dir_wcc))
}

/// A `change_info4` (§3.3.6) from the `wcc_data` of the one v3 call that made the change. Atomic when
/// both halves carry the change counter: the v3 layer took them inside that call on the owner shard,
/// with nothing else able to change the object between them (A-38), so a client whose cached `change`
/// equals `before` knows the only change was its own and keeps its cache. Otherwise it is not atomic,
/// with what is known, and the client revalidates.
pub(super) fn change_info(wcc: &Wcc) -> Vec<u8> {
  let before = wcc.pre.and_then(|pre| pre.change);
  let after = wcc.post.and_then(|post| post.v4).map(|v4| v4.change);
  let mut body = XdrWriter::new();
  match (before, after) {
    (Some(before), Some(after)) => {
      body.bool(true);
      body.u64(before);
      body.u64(after);
    }
    _ => {
      let known = after.or(before).unwrap_or(0);
      body.bool(false);
      body.u64(known);
      body.u64(known);
    }
  }
  body.into_bytes()
}
