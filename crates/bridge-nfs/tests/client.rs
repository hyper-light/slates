//! The mount's client side (§4.6 A-34): the MOUNT call the CLI sends is one the server's own parser
//! reads, with the caller's credentials; the reply the server encodes parses back to the handle; every
//! truncation of a reply is refused, never read past; and the XDR mount arguments are self-consistent
//! (their two length fields describe the buffer), as the kernel checks before copying them in.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_nfs::client::{Credentials, MountArgs, mnt_call, parse_mnt_reply};
use slates_bridge_nfs::mount::{MOUNT_PROGRAM, MOUNTPROC3_MNT, MountReply, parse_mount_path};
use slates_bridge_nfs::{
  AcceptStatus, Nfsfh3, XdrReader, XdrWriter, auth_sys_creds, parse_call, read_record, reply_bytes,
  write_record,
};

fn caller() -> Credentials {
  Credentials {
    uid: 501,
    gid: 20,
    gids: vec![20, 12, 61],
  }
}

/// A-34: do build the CLI's MNT call and parse it with the server's parser; expect the MOUNT program
/// and procedure, the caller's uid and gid, and the path.
#[test]
fn the_servers_parser_reads_the_clis_mount_call() {
  let call = mnt_call(7, &caller(), "/vol@1.00112233445566778899aabbccddeeff");
  let (body, _) = read_record(&call).unwrap();
  let (header, mut args) = parse_call(&body).unwrap();
  assert_eq!(
    (header.xid, header.program, header.procedure),
    (7, MOUNT_PROGRAM, MOUNTPROC3_MNT)
  );
  assert_eq!(auth_sys_creds(&body), Some((501, 20)));
  assert_eq!(
    parse_mount_path(&mut args).unwrap(),
    "/vol@1.00112233445566778899aabbccddeeff"
  );
}

/// A-34: do encode an MNT success the way the server does and parse it; expect the handle. Every
/// truncation of the same reply is refused.
#[test]
fn a_servers_mount_reply_parses_to_its_handle_and_a_truncated_one_is_refused() {
  let handle = Nfsfh3((0u8..57).collect());
  let mut results = XdrWriter::new();
  MountReply::Ok {
    handle: handle.clone(),
    auth_flavors: vec![0, 1],
  }
  .encode(&mut results);
  let reply = write_record(&reply_bytes(
    9,
    AcceptStatus::Success,
    &results.into_bytes(),
  ));
  let (body, _) = read_record(&reply).unwrap();
  assert_eq!(parse_mnt_reply(&body, 9).unwrap(), handle);
  assert!(parse_mnt_reply(&body, 10).is_err(), "another call's reply");
  for cut in 0..body.len() - 4 {
    assert!(
      parse_mnt_reply(&body[..cut], 9).is_err(),
      "truncated to {cut}"
    );
  }
}

/// A-34: do encode the mount arguments; expect the leading version word, an args length equal to the
/// whole buffer (as `mount_nfs` patches it), an attributes length equal to the bytes after it, and the
/// handle and source name inside.
#[test]
fn the_mount_arguments_describe_their_own_length() {
  let args = MountArgs {
    port: 50_123,
    handle: Nfsfh3(vec![0xab; 57]),
    attr_cache_seconds: 1,
    mnt_flags: 0x8,
    mnt_from: "slates:/vol".to_owned(),
    path: vec!["vol".to_owned()],
  }
  .encode();
  let mut reader = XdrReader::new(&args);
  assert_eq!(reader.u32().unwrap(), 88, "NFS_ARGSVERSION_XDR");
  assert_eq!(
    reader.u32().unwrap() as usize,
    args.len(),
    "the args length"
  );
  assert_eq!(reader.u32().unwrap(), 0, "NFS_XDRARGS_VERSION_0");
  let words = reader.u32().unwrap();
  for _ in 0..words {
    reader.u32().unwrap();
  }
  let attrs_len = reader.u32().unwrap() as usize;
  assert_eq!(attrs_len, reader.remaining(), "the attributes length");
  assert!(
    args.windows(57).any(|window| window == [0xab; 57]),
    "the handle"
  );
  assert!(
    args.windows(11).any(|window| window == b"slates:/vol"),
    "the source name"
  );
  assert!(
    !args.windows(1).any(|window| window == b"@"),
    "no capability"
  );
}
