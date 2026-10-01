//! What a FUSE creation makes (§4.6; AUD-29-80, AUD-29-81): the real dispatch over a real volume, as the kernel
//! sends it — the creating process's uid and gid in the request header, and its umask beside the mode (the
//! kernel negotiates `FUSE_DONT_MASK`, so the mode arrives unmasked). Every host: the dispatch is pure.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, Opcode};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};

mod common;
use common::{store, volume_for_owner};

/// Shape: the volume, the enrolled account the context is admitted for, and the account's group.
const VOLUME: VolumeId = VolumeId { bytes: [5; 16] };
const ENROLLED: u32 = 501;
const ENROLLED_GROUP: u32 = 20;
/// Shape: the creating process — another user and group than the enrolled account (§7.6's 1000:100).
const CREATOR_UID: u32 = 1000;
const CREATOR_GID: u32 = 100;
/// Format: the kernel's root node id.
const ROOT: u64 = 1;
/// Format: file type bits as the kernel sends a mode (`S_IFREG`, `S_IFIFO`) and the set-group-ID bit.
const S_IFREG: u32 = 0o100_000;
const S_IFIFO: u32 = 0o010_000;
const S_ISGID: u32 = 0o2000;
/// Format: the reply buffer — far above any entry reply.
const REPLY_BYTES: usize = 4096;

/// A request as the kernel sends it: the header with the creating process's uid and gid, then `body`.
fn message(opcode: Opcode, unique: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&(opcode as u32).to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&ROOT.to_le_bytes());
  m[24..28].copy_from_slice(&CREATOR_UID.to_le_bytes());
  m[28..32].copy_from_slice(&CREATOR_GID.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

fn with_name(mut head: Vec<u8>, name: &str) -> Vec<u8> {
  head.extend_from_slice(name.as_bytes());
  head.push(0);
  head
}

/// `fuse_create_in`: flags, mode, umask, open_flags, then the name.
fn create(mode: u32, umask: u32, name: &str) -> Vec<u8> {
  let mut head = vec![0u8; 16];
  head[4..8].copy_from_slice(&mode.to_le_bytes());
  head[8..12].copy_from_slice(&umask.to_le_bytes());
  with_name(head, name)
}

/// `fuse_mkdir_in`: mode, umask, then the name.
fn mkdir(mode: u32, umask: u32, name: &str) -> Vec<u8> {
  let mut head = vec![0u8; 8];
  head[0..4].copy_from_slice(&mode.to_le_bytes());
  head[4..8].copy_from_slice(&umask.to_le_bytes());
  with_name(head, name)
}

/// `fuse_mknod_in`: mode, rdev, umask, padding, then the name.
fn mknod(mode: u32, umask: u32, name: &str) -> Vec<u8> {
  let mut head = vec![0u8; 16];
  head[0..4].copy_from_slice(&mode.to_le_bytes());
  head[8..12].copy_from_slice(&umask.to_le_bytes());
  with_name(head, name)
}

/// What `name` under the root is: its permission bits (with set-group-ID), owner and group.
fn made(bridge: &mut VolumeBridge<'_>, cx: &OpContext, name: &str) -> (u32, u32, u32) {
  let root = ObjectId::new(bridge.root(cx).unwrap(), 0);
  let node = bridge.lookup(root, cx, name).unwrap();
  (node.mode, node.uid, node.gid)
}

/// AUD-29-80 and AUD-29-81. Do: under a context enrolled as uid 501, dispatch creations from a 1000:100
/// process — a file (0666, umask 077), a directory (0777, umask 022) and a FIFO (0666, umask 027) — then mark
/// the root set-group-ID and create a file and a directory in it. Expect: each mode is the request's less its
/// umask (0600, 0755, 0640; before, 0666 and 0777 survived); each object belongs to the creating process,
/// 1000:100 (before, 501 and the parent's group); and under the set-group-ID parent both take the parent's
/// group, the directory taking the bit too.
#[test]
fn a_creation_takes_the_creators_umask_and_ids_and_the_setgid_parents_group() {
  let mut store = store();
  let mut volume = volume_for_owner(&mut store, ENROLLED, ENROLLED_GROUP);
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VOLUME,
      View::Current,
      Principal::Uid { uid: ENROLLED },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  let cx = attachments.context(id).unwrap();
  let mut bridge = VolumeBridge::new(VOLUME, &mut volume, &mut store);
  let mut out = vec![0u8; REPLY_BYTES];
  let mut send = |bridge: &mut VolumeBridge<'_>, opcode, unique, body: Vec<u8>| {
    let n =
      slates_bridge_fuse::bridge::dispatch(&message(opcode, unique, &body), bridge, &cx, &mut out);
    let error = i32::from_le_bytes(out[4..8].try_into().unwrap());
    assert!(n > 0 && error == 0, "{opcode:?} answered {error}");
  };
  send(
    &mut bridge,
    Opcode::Create,
    1,
    create(S_IFREG | 0o666, 0o077, "private"),
  );
  send(&mut bridge, Opcode::MkDir, 2, mkdir(0o777, 0o022, "dir"));
  send(
    &mut bridge,
    Opcode::MkNod,
    3,
    mknod(S_IFIFO | 0o666, 0o027, "pipe"),
  );
  assert_eq!(
    made(&mut bridge, &cx, "private"),
    (0o600, CREATOR_UID, CREATOR_GID)
  );
  assert_eq!(
    made(&mut bridge, &cx, "dir"),
    (0o755, CREATOR_UID, CREATOR_GID)
  );
  assert_eq!(
    made(&mut bridge, &cx, "pipe"),
    (0o640, CREATOR_UID, CREATOR_GID)
  );

  let root = ObjectId::new(bridge.root(&cx).unwrap(), 0);
  bridge
    .setattr(
      root,
      &cx,
      slates_bridge_core::SetAttr {
        mode: Some(0o775 | S_ISGID),
        ..slates_bridge_core::SetAttr::default()
      },
    )
    .unwrap();
  send(
    &mut bridge,
    Opcode::Create,
    4,
    create(S_IFREG | 0o644, 0o022, "shared"),
  );
  send(&mut bridge, Opcode::MkDir, 5, mkdir(0o775, 0o002, "team"));
  assert_eq!(
    made(&mut bridge, &cx, "shared"),
    (0o644, CREATOR_UID, ENROLLED_GROUP)
  );
  assert_eq!(
    made(&mut bridge, &cx, "team"),
    (0o775 | S_ISGID, CREATOR_UID, ENROLLED_GROUP)
  );
}
