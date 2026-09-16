#![allow(clippy::unwrap_used)]
//! The `AUTH_SYS` credential parser (§4.13, R5): a loopback NFS server reads the mounting user's
//! identity from a call's credential so a request runs as that user, not always as root — its uid, and
//! its primary and supplementary groups, which the permission checks judge the group class by. Drives
//! `auth_sys_uid` and `auth_sys_identity` with a crafted call carrying an `AUTH_SYS` credential (the
//! shape `mount_nfs` sends) and with one carrying `AUTH_NONE`, and refuses a credential claiming more
//! groups than the protocol allows.

use slates_bridge_nfs::{AUTH_SYS_MAX_GIDS, UnixGroups, auth_sys_identity, auth_sys_uid};

fn opaque(bytes: &[u8], out: &mut Vec<u8>) {
  out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
  out.extend_from_slice(bytes);
  out.extend(std::iter::repeat_n(0u8, (4 - bytes.len() % 4) % 4));
}

/// An ONC RPC call body (RFC 5531) whose credential is `AUTH_SYS` naming `uid` with primary group
/// 1000 and the supplementary `gids`, or `AUTH_NONE`.
fn call_with(uid: Option<u32>, gids: &[u32]) -> Vec<u8> {
  let mut body = Vec::new();
  // The call header: xid, mtype (CALL = 0), rpcvers (2), program, version, procedure.
  for field in [1u32, 0, 2, 100_003, 3, 1] {
    body.extend_from_slice(&field.to_be_bytes());
  }
  match uid {
    Some(uid) => {
      body.extend_from_slice(&1u32.to_be_bytes()); // credential flavor AUTH_SYS
      let mut cred = Vec::new();
      cred.extend_from_slice(&0u32.to_be_bytes()); // stamp
      opaque(b"host", &mut cred); // machinename
      cred.extend_from_slice(&uid.to_be_bytes()); // uid
      cred.extend_from_slice(&1000u32.to_be_bytes()); // gid
      cred.extend_from_slice(&u32::try_from(gids.len()).unwrap().to_be_bytes()); // gids count
      for gid in gids {
        cred.extend_from_slice(&gid.to_be_bytes());
      }
      opaque(&cred, &mut body); // the credential body, length-prefixed
    }
    None => {
      body.extend_from_slice(&0u32.to_be_bytes()); // credential flavor AUTH_NONE
      body.extend_from_slice(&0u32.to_be_bytes()); // and an empty body
    }
  }
  body.extend_from_slice(&0u32.to_be_bytes()); // verifier flavor AUTH_NONE
  body.extend_from_slice(&0u32.to_be_bytes()); // verifier empty body
  body
}

/// An `AUTH_SYS` credential names the mounting user; the server reads its uid.
#[test]
fn an_auth_sys_credential_names_the_mounting_user() {
  let call = call_with(Some(1000), &[]);
  assert_eq!(
    auth_sys_uid(&call),
    Some(1000),
    "the AUTH_SYS credential's uid is read"
  );
}

/// An `AUTH_NONE` credential names no user; the server falls back to root.
#[test]
fn an_auth_none_credential_names_no_user() {
  let call = call_with(None, &[]);
  assert_eq!(auth_sys_uid(&call), None, "AUTH_NONE carries no uid");
  assert_eq!(auth_sys_identity(&call), None);
}

/// The whole identity is read: the uid, the primary group, and every supplementary group in order —
/// what the permission checks judge the group class by.
#[test]
fn an_auth_sys_credential_carries_the_groups() {
  let call = call_with(Some(501), &[20, 12, 80]);
  assert_eq!(
    auth_sys_identity(&call).unwrap().groups,
    UnixGroups {
      gid: 1000,
      supplementary: vec![20, 12, 80],
    }
  );
}

/// A credential claiming more supplementary groups than `authsys_parms.gids<16>` allows is refused as
/// malformed before any group is read — a hostile length never sizes an allocation (banned item 8).
#[test]
fn a_credential_with_too_many_groups_is_refused() {
  let at_the_bound: Vec<u32> = (0..u32::try_from(AUTH_SYS_MAX_GIDS).unwrap()).collect();
  assert_eq!(
    auth_sys_identity(&call_with(Some(1), &at_the_bound))
      .unwrap()
      .groups
      .supplementary
      .len(),
    AUTH_SYS_MAX_GIDS,
    "sixteen groups are the most the protocol carries"
  );
  let over: Vec<u32> = (0..=u32::try_from(AUTH_SYS_MAX_GIDS).unwrap()).collect();
  assert_eq!(
    auth_sys_identity(&call_with(Some(1), &over)),
    None,
    "a seventeenth group is a malformed credential"
  );
}
