//! RPC-with-TLS (RFC 9289) at the RPC layer: the `AUTH_TLS` probe a client sends before its TLS
//! handshake, and the two replies a server gives it (§4.6 "Kubernetes publication without privilege",
//! AUD-29-75). The network export accepts only this protocol: over cleartext its capability token would
//! be a bearer secret on the cluster network.
//!
//! The rules, from the RFC's own words:
//!
//! - **The probe** (§4.1) is a `NULL` call whose credential flavor is `AUTH_TLS` (7, §7.1) with an empty
//!   body, and whose verifier is an empty `AUTH_NONE`.
//! - **The answer** is `MSG_ACCEPTED` with an `AUTH_NONE` verifier whose eight-byte body is the ASCII
//!   `STARTTLS`; the client then sends its `ClientHello` on the same connection.
//! - **`AUTH_TLS` anywhere else** — on a procedure other than `NULL`, or inside an established session —
//!   "MUST" be rejected `MSG_DENIED`, `AUTH_ERROR`, `AUTH_BADCRED`.
//!
//! This module is the pure codec over [`crate::rpc`] and [`crate::xdr`]: no socket and no TLS state. The
//! session that runs the handshake lives with the listener (`slates-server`'s `nfs_tls`), so the
//! decisions here are unit-tested on every host and pinned by golden bytes.

use crate::rpc::RpcError;
use crate::xdr::{XdrReader, XdrWriter};

/// Format: the `AUTH_TLS` authentication flavor (RFC 9289 §7.1, "Value: 7").
pub const AUTH_TLS: u32 = 7;
/// Format: the verifier body that signals RPC-with-TLS support (RFC 9289 §4.1: "the ASCII characters
/// \"STARTTLS\" as a fixed-length opaque").
pub const STARTTLS: [u8; 8] = *b"STARTTLS";
/// Format: the ALPN protocol identifier for SunRPC (RFC 9289 §7.2: `0x73 0x75 0x6e 0x72 0x70 0x63`).
pub const ALPN_SUNRPC: &[u8] = b"sunrpc";
/// Format: the ONC RPC message type for a call (RFC 5531 `msg_type`).
const MSG_CALL: u32 = 0;
/// Format: the ONC RPC message type for a reply.
const MSG_REPLY: u32 = 1;
/// Format: the ONC RPC protocol version (RFC 5531 `rpcvers`).
const RPC_VERSION: u32 = 2;
/// Format: `reply_stat` `MSG_ACCEPTED`.
const MSG_ACCEPTED: u32 = 0;
/// Format: `reply_stat` `MSG_DENIED`.
const MSG_DENIED: u32 = 1;
/// Format: `reject_stat` `AUTH_ERROR`.
const AUTH_ERROR: u32 = 1;
/// Format: `auth_stat` `AUTH_BADCRED` ("bad credential (seal broken)", RFC 5531).
const AUTH_BADCRED: u32 = 1;
/// Format: `auth_stat` `AUTH_TOOWEAK` ("rejected for security reasons", RFC 5531).
const AUTH_TOOWEAK: u32 = 5;
/// Format: the `AUTH_NONE` flavor.
const AUTH_NONE: u32 = 0;
/// Format: `accept_stat` `SUCCESS`.
const ACCEPT_SUCCESS: u32 = 0;
/// Format: the `NULL` procedure every RPC program defines as procedure 0.
const NULL_PROCEDURE: u32 = 0;
/// Format: the largest `opaque_auth` body RFC 5531 allows (`opaque body<400>`).
const MAX_AUTH_BODY: usize = 400;

/// What a cleartext call is, judged before any TLS session exists on its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
  /// A well-formed `AUTH_TLS` probe: answer [`starttls_reply`], then expect the `ClientHello`.
  StartTls {
    /// The probe's transaction id, echoed in the reply.
    xid: u32,
  },
  /// `AUTH_TLS` on a call that is not a well-formed probe (another procedure, a credential with a body,
  /// a verifier that is not an empty `AUTH_NONE`): answer [`bad_credential_reply`].
  BadCredential {
    /// The call's transaction id.
    xid: u32,
  },
  /// A call that does not ask for TLS at all.
  Cleartext {
    /// The call's transaction id.
    xid: u32,
  },
}

/// Classifies one deframed call message (RFC 9289 §4.1). A message that is not a version-2 call is
/// refused as [`crate::rpc::parse_call`] refuses it.
pub fn classify(message: &[u8]) -> Result<Probe, RpcError> {
  let mut reader = XdrReader::new(message);
  let xid = reader.u32()?;
  if reader.u32()? != MSG_CALL || reader.u32()? != RPC_VERSION {
    return Err(RpcError::NotACall);
  }
  let _program = reader.u32()?;
  let _version = reader.u32()?;
  let procedure = reader.u32()?;
  let credential_flavor = reader.u32()?;
  let credential = reader.opaque(MAX_AUTH_BODY)?;
  let verifier_flavor = reader.u32()?;
  let verifier = reader.opaque(MAX_AUTH_BODY)?;
  if credential_flavor != AUTH_TLS {
    return Ok(Probe::Cleartext { xid });
  }
  let well_formed = procedure == NULL_PROCEDURE
    && credential.is_empty()
    && verifier_flavor == AUTH_NONE
    && verifier.is_empty();
  Ok(if well_formed {
    Probe::StartTls { xid }
  } else {
    Probe::BadCredential { xid }
  })
}

/// Whether a call inside an established session carries `AUTH_TLS`, which the server "MUST reject"
/// (RFC 9289 §4.1); `None` when the message is not a readable call.
pub fn carries_auth_tls(message: &[u8]) -> Option<bool> {
  match classify(message).ok()? {
    Probe::Cleartext { .. } => Some(false),
    Probe::StartTls { .. } | Probe::BadCredential { .. } => Some(true),
  }
}

/// The reply to a well-formed probe: accepted, an `AUTH_NONE` verifier carrying `STARTTLS`, and
/// `SUCCESS` (the `NULL` procedure has no results). Not record-marked.
pub fn starttls_reply(xid: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  writer.u32(xid);
  writer.u32(MSG_REPLY);
  writer.u32(MSG_ACCEPTED);
  writer.u32(AUTH_NONE);
  writer.opaque(&STARTTLS);
  writer.u32(ACCEPT_SUCCESS);
  writer.into_bytes()
}

/// The reply to `AUTH_TLS` where it may not be: denied, `AUTH_ERROR`, `AUTH_BADCRED`. Not record-marked.
pub fn bad_credential_reply(xid: u32) -> Vec<u8> {
  denied(xid, AUTH_BADCRED)
}

/// The reply to a cleartext call on a listener that serves only RPC-with-TLS: denied, `AUTH_ERROR`,
/// `AUTH_TOOWEAK`. RFC 9289 leaves a TLS-only server's answer to policy (§4.1, "RPC operation may
/// continue, depending on local policy"); this one names the reason, then the connection is closed.
pub fn too_weak_reply(xid: u32) -> Vec<u8> {
  denied(xid, AUTH_TOOWEAK)
}

/// A `MSG_DENIED` reply rejecting the call's authentication with `auth_stat`. Not record-marked.
fn denied(xid: u32, auth_stat: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  writer.u32(xid);
  writer.u32(MSG_REPLY);
  writer.u32(MSG_DENIED);
  writer.u32(AUTH_ERROR);
  writer.u32(auth_stat);
  writer.into_bytes()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A call message: xid, CALL, version 2, the program triple, then the credential and verifier.
  fn call(procedure: u32, credential: (u32, &[u8]), verifier: (u32, &[u8])) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    for field in [0x0102_0304, MSG_CALL, RPC_VERSION, 100_003, 4, procedure] {
      writer.u32(field);
    }
    writer.u32(credential.0);
    writer.opaque(credential.1);
    writer.u32(verifier.0);
    writer.opaque(verifier.1);
    writer.into_bytes()
  }

  /// RFC 9289 §4.1. Do: classify a well-formed probe, `AUTH_TLS` on `COMPOUND`, `AUTH_TLS` with a credential
  /// body, `AUTH_TLS` with a non-empty verifier, and an `AUTH_SYS` call. Expect: only the first is a probe;
  /// the next three are refused `BadCredential`; the last is cleartext.
  #[test]
  fn only_a_null_call_with_an_empty_auth_tls_credential_is_a_probe() {
    let xid = 0x0102_0304;
    assert_eq!(
      classify(&call(0, (AUTH_TLS, &[]), (AUTH_NONE, &[]))),
      Ok(Probe::StartTls { xid })
    );
    for (what, message) in [
      (
        "another procedure",
        call(1, (AUTH_TLS, &[]), (AUTH_NONE, &[])),
      ),
      (
        "a credential body",
        call(0, (AUTH_TLS, &[0, 0, 0, 1]), (AUTH_NONE, &[])),
      ),
      (
        "a verifier body",
        call(0, (AUTH_TLS, &[]), (AUTH_NONE, &[1, 2, 3, 4])),
      ),
      (
        "another verifier flavor",
        call(0, (AUTH_TLS, &[]), (1, &[])),
      ),
    ] {
      assert_eq!(
        classify(&message),
        Ok(Probe::BadCredential { xid }),
        "{what}"
      );
      assert_eq!(carries_auth_tls(&message), Some(true), "{what}");
    }
    let plain = call(1, (1, &[0, 0, 0, 0]), (AUTH_NONE, &[]));
    assert_eq!(classify(&plain), Ok(Probe::Cleartext { xid }));
    assert_eq!(carries_auth_tls(&plain), Some(false));
  }

  /// Golden vectors (RFC 9289 §4.1; RFC 5531 `reply_body`). Do: build both replies for xid
  /// `0x01020304`. Expect: exactly the bytes the RFCs' fields spell, in order.
  #[test]
  fn the_starttls_and_bad_credential_replies_are_the_rfcs_bytes() {
    let starttls: [u8; 32] = [
      1, 2, 3, 4, // xid
      0, 0, 0, 1, // REPLY
      0, 0, 0, 0, // MSG_ACCEPTED
      0, 0, 0, 0, // verifier flavor AUTH_NONE
      0, 0, 0, 8, // verifier body length
      b'S', b'T', b'A', b'R', b'T', b'T', b'L', b'S', // "STARTTLS"
      0, 0, 0, 0, // accept_stat SUCCESS
    ];
    assert_eq!(starttls_reply(0x0102_0304), starttls);
    let denied: [u8; 20] = [
      1, 2, 3, 4, // xid
      0, 0, 0, 1, // REPLY
      0, 0, 0, 1, // MSG_DENIED
      0, 0, 0, 1, // AUTH_ERROR
      0, 0, 0, 1, // AUTH_BADCRED
    ];
    assert_eq!(bad_credential_reply(0x0102_0304), denied);
    let mut too_weak = denied;
    too_weak[19] = 5; // AUTH_TOOWEAK
    assert_eq!(too_weak_reply(0x0102_0304), too_weak);
  }

  /// Hostile input. Do: classify a reply, version 3, a truncated header, a credential claiming
  /// `u32::MAX` bytes, and an empty message. Expect: each is a typed refusal, never a probe or a panic.
  #[test]
  fn malformed_messages_are_refused_typed() {
    let mut reply = call(0, (AUTH_TLS, &[]), (AUTH_NONE, &[]));
    reply[4..8].copy_from_slice(&MSG_REPLY.to_be_bytes());
    assert_eq!(classify(&reply), Err(RpcError::NotACall));
    let mut version = call(0, (AUTH_TLS, &[]), (AUTH_NONE, &[]));
    version[8..12].copy_from_slice(&3u32.to_be_bytes());
    assert_eq!(classify(&version), Err(RpcError::NotACall));
    let whole = call(0, (AUTH_TLS, &[]), (AUTH_NONE, &[]));
    assert_eq!(classify(&whole[..20]), Err(RpcError::Malformed));
    let mut huge = whole.clone();
    huge[28..32].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(classify(&huge), Err(RpcError::Malformed));
    assert_eq!(classify(&[]), Err(RpcError::Malformed));
    assert_eq!(carries_auth_tls(&[]), None);
  }
}
