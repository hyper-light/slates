//! Tests for the MOUNT and portmap wire codecs (§4.6, Phase 4). A successful MNT reply carries the
//! root handle and the accepted auth flavors; a failure carries only its status; a mount path
//! round-trips and one past the length cap is refused; a portmap mapping round-trips and a GETPORT
//! reply is the bare port. Pure, every host — no socket, no mount.

use slates_bridge_nfs::mount::{MNTPATHLEN, MountReply, Mountstat3, parse_mount_path};
use slates_bridge_nfs::nfs::Nfsfh3;
use slates_bridge_nfs::portmap::{
  IPPROTO_TCP, Mapping, PORT_NOT_REGISTERED, PORTMAP_PROGRAM, getport_reply,
};
use slates_bridge_nfs::xdr::{XdrError, XdrReader, XdrWriter};

/// A successful MNT reply encodes MNT3_OK, then the root handle, then the auth-flavor list.
#[test]
fn a_successful_mount_reply_carries_the_handle_and_auth_flavors() {
  let handle = Nfsfh3(vec![0x07; 33]);
  let reply = MountReply::Ok {
    handle: handle.clone(),
    auth_flavors: vec![1, 0], // AUTH_SYS, AUTH_NONE
  };
  let mut w = XdrWriter::new();
  reply.encode(&mut w);
  let mut r = XdrReader::new(w.as_slice());
  assert_eq!(r.u32().unwrap(), Mountstat3::Ok.wire());
  assert_eq!(Nfsfh3::decode(&mut r).unwrap(), handle);
  assert_eq!(r.u32().unwrap(), 2); // two flavors
  assert_eq!(r.u32().unwrap(), 1);
  assert_eq!(r.u32().unwrap(), 0);
  assert_eq!(r.remaining(), 0);
}

/// A failed MNT reply is only its status (no handle, no flavors).
#[test]
fn a_failed_mount_reply_is_only_the_status() {
  let mut w = XdrWriter::new();
  MountReply::Err(Mountstat3::Noent).encode(&mut w);
  let bytes = w.into_bytes();
  assert_eq!(bytes.len(), size_of::<u32>());
  assert_eq!(
    u32::from_be_bytes(bytes.try_into().unwrap()),
    Mountstat3::Noent.wire()
  );
}

/// A mount path round-trips, and one past the length cap is refused before allocating.
#[test]
fn a_mount_path_round_trips_and_caps_length() {
  let mut w = XdrWriter::new();
  w.opaque("/scratch/build".as_bytes());
  assert_eq!(
    parse_mount_path(&mut XdrReader::new(w.as_slice())).unwrap(),
    "/scratch/build"
  );

  let mut over = XdrWriter::new();
  over.opaque(&vec![b'a'; MNTPATHLEN + 1]);
  assert_eq!(
    parse_mount_path(&mut XdrReader::new(over.as_slice())),
    Err(XdrError::BadLength)
  );
}

/// A portmap mapping round-trips, and a GETPORT reply is the bare port.
#[test]
fn a_portmap_mapping_round_trips_and_getport_is_the_bare_port() {
  let mapping = Mapping {
    program: PORTMAP_PROGRAM,
    version: 2,
    protocol: IPPROTO_TCP,
    port: 0,
  };
  let mut w = XdrWriter::new();
  mapping.encode(&mut w);
  assert_eq!(
    Mapping::decode(&mut XdrReader::new(w.as_slice())).unwrap(),
    mapping
  );

  assert_eq!(getport_reply(2049), 2049u32.to_be_bytes());
  assert_eq!(getport_reply(PORT_NOT_REGISTERED), [0, 0, 0, 0]);
}
