//! A minimal NFSv4.2 client for the daemon's NFS loopback port (A-35; RFC 8881 sessions, RFC 8276 extended
//! attributes): EXCHANGE_ID and CREATE_SESSION once, then one `COMPOUND` per call on slot 0 — SEQUENCE,
//! PUTROOTFH, a LOOKUP of the mount capability and of each path component, and the operation under test.
//! It speaks the same TCP port and record marking as the NFSv3 client in `nfs.rs`, so a test drives the
//! daemon's real NFSv4.2 transport with no kernel mount and no privilege.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn,
  clippy::unwrap_in_result
)]

use std::net::TcpStream;

use slates_bridge_nfs::v4::compound::op;
use slates_bridge_nfs::v4::types::ChannelAttrs;
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};

use super::nfs::{NFS_PROGRAM, call_version};

/// Format: the NFS version these calls speak (RFC 8881).
const VERSION_4: u32 = 4;
/// Format: the minor version of every compound (RFC 7862: the extended attribute operations are 4.2).
const MINOR_2: u32 = 2;
/// Format: the COMPOUND procedure.
const COMPOUND: u32 = 1;
/// Format: the bytes of a SEQUENCE result after its status — the session id and five words.
const SEQUENCE_RESULT_BYTES: usize = 16 + 5 * 4;
/// Shape: the largest request and reply this client asks its session for — one page of ops and values
/// well past any value the tests set.
const CHANNEL_BYTES: u32 = 1 << 20;
/// Shape: the most operations one compound of this client carries (SEQUENCE, PUTROOTFH, a few LOOKUPs and
/// the operation), with room.
const CHANNEL_OPERATIONS: u32 = 16;
/// Format: the callback program CREATE_SESSION names (unused: this client takes no callbacks).
const CALLBACK_PROGRAM: u32 = 0x4000_0000;
/// Format: `ca_maxrequests`, one slot: this client sends one compound at a time.
const ONE_SLOT: u32 = 1;

/// One NFSv4.2 session over `stream`: its id, slot 0's last sequence id and the next RPC xid.
pub(crate) struct Session {
  stream: TcpStream,
  sessionid: [u8; 16],
  sequence: u32,
  xid: u32,
}

impl Session {
  /// Connects to `port` and opens a session for the client owner `owner` (unique per test).
  pub(crate) fn open(port: u16, owner: &[u8]) -> Session {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
    let mut session = Session {
      stream,
      sessionid: [0; 16],
      sequence: 0,
      xid: 0,
    };
    let mut args = frame(1);
    args.u32(op::EXCHANGE_ID);
    args.fixed(&[1; 8]);
    args.opaque(owner);
    args.u32(0); // flags
    args.u32(0); // SP4_NONE
    args.u32(0); // no implementation id
    let reply = session.compound(args.as_slice());
    let mut body = results(&reply, op::EXCHANGE_ID).expect("EXCHANGE_ID");
    let clientid = body.u64().unwrap();
    let sequenceid = body.u32().unwrap();
    let mut args = frame(1);
    args.u32(op::CREATE_SESSION);
    args.u64(clientid);
    args.u32(sequenceid);
    args.u32(0); // flags
    let asked = ChannelAttrs {
      header_pad: 0,
      max_request: CHANNEL_BYTES,
      max_response: CHANNEL_BYTES,
      max_response_cached: CHANNEL_BYTES,
      max_operations: CHANNEL_OPERATIONS,
      max_requests: ONE_SLOT,
    };
    asked.encode(&mut args);
    asked.encode(&mut args);
    args.u32(CALLBACK_PROGRAM);
    args.u32(1); // one security parameter
    args.u32(0); // AUTH_NONE
    let reply = session.compound(args.as_slice());
    let mut body = results(&reply, op::CREATE_SESSION).expect("CREATE_SESSION");
    session.sessionid.copy_from_slice(body.fixed(16).unwrap());
    session
  }

  /// Runs `operation` on the node at `path` (components below the mount `capability`, the
  /// `<name>@<attachment>.<token>` an NFSv3 mount presents with its leading slash): the operation's result
  /// body, or the first failing status.
  pub(crate) fn at(
    &mut self,
    capability: &str,
    path: &[&str],
    operation: impl FnOnce(&mut XdrWriter),
  ) -> Result<Vec<u8>, u32> {
    self.sequence += 1;
    let lookups = u32::try_from(path.len()).unwrap() + 1;
    // SEQUENCE, PUTROOTFH, the LOOKUPs and the operation.
    let mut args = frame(lookups + 3);
    args.u32(op::SEQUENCE);
    args.fixed(&self.sessionid);
    args.u32(self.sequence);
    args.u32(0); // slot
    args.u32(0); // highest slot
    args.bool(false);
    args.u32(op::PUTROOTFH);
    for component in std::iter::once(capability.trim_start_matches('/')).chain(path.iter().copied())
    {
      args.u32(op::LOOKUP);
      args.opaque(component.as_bytes());
    }
    operation(&mut args);
    let reply = self.compound(args.as_slice());
    let mut body = XdrReader::new(&reply);
    let status = body.u32().unwrap();
    body.opaque(1024).unwrap();
    let count = body.u32().unwrap();
    for index in 0..count {
      let _opnum = body.u32().unwrap();
      let result = body.u32().unwrap();
      if result != 0 {
        return Err(result);
      }
      if index == 0 {
        body.fixed(SEQUENCE_RESULT_BYTES).unwrap();
      }
      if index + 1 == count {
        assert_eq!(status, 0, "the compound succeeded with its last operation");
        return Ok(body.rest().to_vec());
      }
    }
    Err(status)
  }

  /// SETXATTR of `name` to `value` on the node at `path` (`SETXATTR4_EITHER`).
  pub(crate) fn set_xattr(&mut self, capability: &str, path: &[&str], name: &[u8], value: &[u8]) {
    self
      .at(capability, path, |args| {
        args.u32(op::SETXATTR);
        args.u32(0); // SETXATTR4_EITHER
        args.opaque(name);
        args.opaque(value);
      })
      .unwrap_or_else(|status| panic!("SETXATTR answered {status}"));
  }

  /// Every extended attribute of the node at `path`, by name, each value read with GETXATTR: LISTXATTRS
  /// pages through the names from cookie zero until it reports the end.
  pub(crate) fn xattrs(&mut self, capability: &str, path: &[&str]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut cookie = 0u64;
    loop {
      let body = self
        .at(capability, path, |args| {
          args.u32(op::LISTXATTRS);
          args.u64(cookie);
          args.u32(CHANNEL_BYTES);
        })
        .unwrap_or_else(|status| panic!("LISTXATTRS answered {status}"));
      let mut body = XdrReader::new(&body);
      cookie = body.u64().unwrap();
      for _ in 0..body.u32().unwrap() {
        names.push(body.opaque(usize::MAX).unwrap().to_vec());
      }
      if body.bool().unwrap() {
        break;
      }
    }
    names
      .into_iter()
      .map(|name| {
        let body = self
          .at(capability, path, |args| {
            args.u32(op::GETXATTR);
            args.opaque(&name);
          })
          .unwrap_or_else(|status| panic!("GETXATTR answered {status}"));
        let value = XdrReader::new(&body).opaque(usize::MAX).unwrap().to_vec();
        (name, value)
      })
      .collect()
  }

  /// One COMPOUND call: its results (the COMPOUND4res).
  fn compound(&mut self, args: &[u8]) -> Vec<u8> {
    self.xid += 1;
    call_version(
      &mut self.stream,
      NFS_PROGRAM,
      VERSION_4,
      COMPOUND,
      args,
      self.xid,
    )
  }
}

/// A COMPOUND's frame: an empty tag, minor version 2 and the operation count.
fn frame(count: u32) -> XdrWriter {
  let mut args = XdrWriter::new();
  args.opaque(b"");
  args.u32(MINOR_2);
  args.u32(count);
  args
}

/// The body of a one-operation compound's result, after checking it is `opnum` succeeding.
fn results(reply: &[u8], opnum: u32) -> Result<XdrReader<'_>, u32> {
  let mut body = XdrReader::new(reply);
  let status = body.u32().unwrap();
  body.opaque(1024).unwrap();
  let _count = body.u32().unwrap();
  if status != 0 {
    return Err(status);
  }
  assert_eq!(body.u32().unwrap(), opnum, "result order");
  assert_eq!(body.u32().unwrap(), 0, "op {opnum} status");
  Ok(body)
}
