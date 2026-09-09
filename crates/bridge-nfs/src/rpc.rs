//! ONC RPC (RFC 1057/5531): the remote-procedure-call layer NFSv3, MOUNT and portmap ride on. Over
//! TCP a message is framed by *record marking* — a four-byte header per fragment whose top bit is
//! the last-fragment flag and whose low 31 bits are the fragment length — so a reader can tell a
//! complete message from a partial one on the stream. A call names a program, version and procedure
//! and carries two credentials; the loopback server accepts and sends `AUTH_NONE` (it trusts the
//! peer of a socket only it created and the kernel connects, checked out of band). A reply is
//! accepted or denied, and an accepted reply carries a status then the procedure's results.
//!
//! This module is the pure codec: it parses a call header and builds a reply message, and frames and
//! deframes records, all bounds-checked over [`crate::xdr`] with no socket. A hostile length is a
//! typed refusal, and a partial stream is distinguished from a malformed one so the reader can wait
//! for more bytes rather than fail.

use crate::xdr::{XdrError, XdrReader, XdrWriter};

/// Format: the ONC RPC message type for a call.
const MSG_CALL: u32 = 0;
/// Format: the ONC RPC message type for a reply.
const MSG_REPLY: u32 = 1;
/// Format: the ONC RPC version this codec speaks (RFC 5531).
const RPC_VERSION: u32 = 2;
/// Format: the reply status for an accepted message (`MSG_ACCEPTED`).
const REPLY_ACCEPTED: u32 = 0;
/// Format: the `AUTH_NONE` authentication flavor (RFC 5531).
const AUTH_NONE: u32 = 0;
/// Format: the largest credential or verifier body ONC RPC allows (RFC 5531, 400 bytes).
const MAX_AUTH_BODY: usize = 400;
/// Format: the largest ONC RPC message the loopback server accepts, bounding a record-marking
/// fragment so a hostile length is refused before any accumulation. Sized to a NFSv3 header plus the
/// largest write payload the server offers; the negotiated write size derives the exact bound once
/// the server wires it (owed). Two mebibytes is comfortably above a 1 MiB write plus overhead.
const MAX_MESSAGE: usize = 2 * 1024 * 1024;
/// Format: the record-marking last-fragment flag — the top bit of the four-byte marker (RFC 5531).
const LAST_FRAGMENT: u32 = 0x8000_0000;
/// Format: the record-marking fragment-length mask — the low 31 bits of the marker.
const FRAGMENT_LEN_MASK: u32 = 0x7fff_ffff;
/// Format: `accept_stat` SUCCESS (RFC 5531 §9).
const ACCEPT_SUCCESS: u32 = 0;
/// Format: `accept_stat` PROG_UNAVAIL.
const ACCEPT_PROG_UNAVAIL: u32 = 1;
/// Format: `accept_stat` PROG_MISMATCH.
const ACCEPT_PROG_MISMATCH: u32 = 2;
/// Format: `accept_stat` PROC_UNAVAIL.
const ACCEPT_PROC_UNAVAIL: u32 = 3;
/// Format: `accept_stat` GARBAGE_ARGS.
const ACCEPT_GARBAGE_ARGS: u32 = 4;
/// Format: `accept_stat` SYSTEM_ERR.
const ACCEPT_SYSTEM_ERR: u32 = 5;

/// A refusal from the RPC codec, or a signal that the stream does not yet hold a whole record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcError {
  /// The stream does not yet contain a complete record; read more bytes and retry.
  Incomplete,
  /// A record's declared length exceeds the message cap (a hostile or corrupt marker).
  RecordTooLarge,
  /// The message is not a call, or its RPC version is not 2.
  NotACall,
  /// A field is malformed (a bad length or a short body).
  Malformed,
}

impl From<XdrError> for RpcError {
  fn from(_: XdrError) -> RpcError {
    RpcError::Malformed
  }
}

/// A parsed RPC call header: the transaction id and the program, version and procedure it targets.
/// The credentials are validated for shape and skipped (the loopback server uses `AUTH_NONE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcCall {
  /// The transaction id, echoed in the reply.
  pub xid: u32,
  /// The RPC program number (NFS, MOUNT or portmap).
  pub program: u32,
  /// The program version.
  pub version: u32,
  /// The procedure number.
  pub procedure: u32,
}

/// The status of an accepted reply (RFC 5531 `accept_stat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptStatus {
  /// The procedure ran; its results follow.
  Success,
  /// The program is not served here.
  ProgUnavail,
  /// The program version is out of range; the supported `[low, high]` follows.
  ProgMismatch {
    /// The lowest supported version.
    low: u32,
    /// The highest supported version.
    high: u32,
  },
  /// The procedure number is not served.
  ProcUnavail,
  /// The arguments would not decode.
  GarbageArgs,
  /// The server hit an internal error.
  SystemErr,
}

impl AcceptStatus {
  /// The RFC 5531 wire value.
  fn wire(self) -> u32 {
    match self {
      AcceptStatus::Success => ACCEPT_SUCCESS,
      AcceptStatus::ProgUnavail => ACCEPT_PROG_UNAVAIL,
      AcceptStatus::ProgMismatch { .. } => ACCEPT_PROG_MISMATCH,
      AcceptStatus::ProcUnavail => ACCEPT_PROC_UNAVAIL,
      AcceptStatus::GarbageArgs => ACCEPT_GARBAGE_ARGS,
      AcceptStatus::SystemErr => ACCEPT_SYSTEM_ERR,
    }
  }
}

/// Skips one `opaque_auth` (flavor then a bounded opaque body), validating its shape.
fn skip_auth(reader: &mut XdrReader<'_>) -> Result<(), RpcError> {
  let _flavor = reader.u32()?;
  let _body = reader.opaque(MAX_AUTH_BODY)?;
  Ok(())
}

/// Deframes one record-marked message from the front of `bytes`, returning the assembled message and
/// the number of stream bytes it consumed. `Incomplete` means the stream does not yet hold the whole
/// record (wait for more bytes); a fragment claiming more than the message cap is refused.
pub fn read_record(bytes: &[u8]) -> Result<(Vec<u8>, usize), RpcError> {
  let mut message = Vec::new();
  let mut pos = 0usize;
  loop {
    let header = bytes
      .get(pos..pos + size_of::<u32>())
      .ok_or(RpcError::Incomplete)?;
    let marker = u32::from_be_bytes(header.try_into().map_err(|_| RpcError::Malformed)?);
    let last = marker & LAST_FRAGMENT != 0;
    let len = usize::try_from(marker & FRAGMENT_LEN_MASK).unwrap_or(usize::MAX);
    if len > MAX_MESSAGE || message.len().saturating_add(len) > MAX_MESSAGE {
      return Err(RpcError::RecordTooLarge);
    }
    let start = pos + size_of::<u32>();
    let fragment = bytes.get(start..start + len).ok_or(RpcError::Incomplete)?;
    message.extend_from_slice(fragment);
    pos = start + len;
    if last {
      return Ok((message, pos));
    }
  }
}

/// Frames `body` as one last-fragment record (prepends the four-byte record marker).
pub fn write_record(body: &[u8]) -> Vec<u8> {
  let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
  let marker = len | LAST_FRAGMENT;
  let mut out = Vec::with_capacity(body.len().saturating_add(size_of::<u32>()));
  out.extend_from_slice(&marker.to_be_bytes());
  out.extend_from_slice(body);
  out
}

/// Parses an RPC call from a message body, returning the header and a reader positioned at the
/// procedure arguments. Refuses a message that is not a version-2 call.
pub fn parse_call(body: &[u8]) -> Result<(RpcCall, XdrReader<'_>), RpcError> {
  let mut reader = XdrReader::new(body);
  let xid = reader.u32()?;
  if reader.u32()? != MSG_CALL || reader.u32()? != RPC_VERSION {
    return Err(RpcError::NotACall);
  }
  let program = reader.u32()?;
  let version = reader.u32()?;
  let procedure = reader.u32()?;
  skip_auth(&mut reader)?; // credential
  skip_auth(&mut reader)?; // verifier
  Ok((
    RpcCall {
      xid,
      program,
      version,
      procedure,
    },
    reader,
  ))
}

/// Format: the `AUTH_SYS` authentication flavor (RFC 5531): a credential carrying the caller's uid/gid.
const AUTH_SYS: u32 = 1;

/// The Unix user id an `AUTH_SYS` credential on a call names — the uid the client mounted as — or
/// `None` for `AUTH_NONE` or a credential this cannot read. On a loopback mount the kernel fills it from
/// the mounting process, so it is the mounting user's identity (§4.13), which a server may map to a
/// principal rather than trusting every request as root. Re-reads the call header (cheap) to reach the
/// credential `parse_call` skips.
pub fn auth_sys_uid(body: &[u8]) -> Option<u32> {
  let mut reader = XdrReader::new(body);
  // The call header before the credential: xid, mtype, rpcvers, program, version, procedure.
  let _xid = reader.u32().ok()?;
  let _mtype = reader.u32().ok()?;
  let _rpcvers = reader.u32().ok()?;
  let _program = reader.u32().ok()?;
  let _version = reader.u32().ok()?;
  let _procedure = reader.u32().ok()?;
  let flavor = reader.u32().ok()?;
  let credential = reader.opaque(MAX_AUTH_BODY).ok()?;
  if flavor != AUTH_SYS {
    return None;
  }
  // authsys_parms (RFC 5531 Appendix A): stamp, machinename, uid, gid, gids.
  let mut credential = XdrReader::new(credential);
  let _stamp = credential.u32().ok()?;
  let _machinename = credential.opaque(MAX_AUTH_BODY).ok()?;
  credential.u32().ok()
}

/// Builds an accepted RPC reply message body (not record-marked): the xid, the accepted status, an
/// `AUTH_NONE` verifier, the accept status, and — for success — the already-XDR-encoded `results`.
pub fn reply_bytes(xid: u32, status: AcceptStatus, results: &[u8]) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  writer.u32(xid);
  writer.u32(MSG_REPLY);
  writer.u32(REPLY_ACCEPTED);
  writer.u32(AUTH_NONE); // verifier flavor
  writer.opaque(&[]); // empty verifier body
  writer.u32(status.wire());
  if let AcceptStatus::ProgMismatch { low, high } = status {
    writer.u32(low);
    writer.u32(high);
  }
  let mut out = writer.into_bytes();
  if matches!(status, AcceptStatus::Success) {
    out.extend_from_slice(results);
  }
  out
}
