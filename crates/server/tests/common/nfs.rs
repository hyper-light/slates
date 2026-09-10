//! A hand-rolled ONC RPC client for the daemon's NFS loopback port (§4.6, R5): MOUNT, LOOKUP, CREATE,
//! WRITE, READ and READDIRPLUS over TCP, needing no kernel mount and no privilege, so a test drives a
//! provisioned volume through the daemon's real NFS transport — the bytes travel client → NFS → the
//! shard's real volume and back. A real `mount_nfs localhost:PORT` does the same over the kernel.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::TcpStream;

/// Format: the MOUNT program number (RFC 1813).
pub(crate) const MOUNT_PROGRAM: u32 = 100_005;
/// Format: the NFS program number (RFC 1813).
pub(crate) const NFS_PROGRAM: u32 = 100_003;

pub(crate) fn opaque(bytes: &[u8], out: &mut Vec<u8>) {
  out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
  out.extend_from_slice(bytes);
  out.extend(std::iter::repeat_n(0u8, (4 - bytes.len() % 4) % 4));
}

pub(crate) fn read_opaque(buf: &[u8], off: usize) -> (Vec<u8>, usize) {
  let len = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
  let start = off + 4;
  (
    buf[start..start + len].to_vec(),
    start + len + (4 - len % 4) % 4,
  )
}

pub(crate) fn call(
  stream: &mut TcpStream,
  program: u32,
  procedure: u32,
  args: &[u8],
  xid: u32,
) -> Vec<u8> {
  let mut body = Vec::new();
  for field in [xid, 0, 2, program, 3, procedure, 0, 0, 0, 0] {
    body.extend_from_slice(&field.to_be_bytes());
  }
  body.extend_from_slice(args);
  let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
  stream.write_all(&marker.to_be_bytes()).unwrap();
  stream.write_all(&body).unwrap();

  let mut marker_buf = [0u8; 4];
  stream.read_exact(&mut marker_buf).unwrap();
  let len = (u32::from_be_bytes(marker_buf) & 0x7fff_ffff) as usize;
  let mut reply = vec![0u8; len];
  stream.read_exact(&mut reply).unwrap();
  let verf_len = u32::from_be_bytes(reply[16..20].try_into().unwrap()) as usize;
  let accept_off = 20 + verf_len + (4 - verf_len % 4) % 4;
  assert_eq!(
    u32::from_be_bytes(reply[accept_off..accept_off + 4].try_into().unwrap()),
    0,
    "RPC accepted"
  );
  reply[accept_off + 4..].to_vec()
}

pub(crate) fn status(results: &[u8]) -> u32 {
  u32::from_be_bytes(results[0..4].try_into().unwrap())
}

/// MOUNT MNT `path` → the root file handle.
pub(crate) fn mount(stream: &mut TcpStream, path: &str, xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(path.as_bytes(), &mut args);
  let reply = call(stream, MOUNT_PROGRAM, 1, &args, xid);
  assert_eq!(status(&reply), 0, "MNT {path}");
  read_opaque(&reply, 4).0
}

/// NFS LOOKUP `name` in `dir_fh` → the child's file handle.
pub(crate) fn lookup(stream: &mut TcpStream, dir_fh: &[u8], name: &str, xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  opaque(name.as_bytes(), &mut args);
  let reply = call(stream, NFS_PROGRAM, 3, &args, xid);
  assert_eq!(status(&reply), 0, "LOOKUP {name}");
  read_opaque(&reply, 4).0
}

/// NFS CREATE (UNCHECKED) `name` in `dir_fh` with mode 0644 → the new file's handle.
pub(crate) fn create(stream: &mut TcpStream, dir_fh: &[u8], name: &str, xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  opaque(name.as_bytes(), &mut args);
  args.extend_from_slice(&0u32.to_be_bytes()); // createmode3 UNCHECKED
  // sattr3: mode set to 0644, everything else unset, times unchanged.
  args.extend_from_slice(&1u32.to_be_bytes()); // set_mode: yes
  args.extend_from_slice(&0o644u32.to_be_bytes());
  args.extend_from_slice(&0u32.to_be_bytes()); // set_uid: no
  args.extend_from_slice(&0u32.to_be_bytes()); // set_gid: no
  args.extend_from_slice(&0u32.to_be_bytes()); // set_size: no
  args.extend_from_slice(&0u32.to_be_bytes()); // atime: DONT_CHANGE
  args.extend_from_slice(&0u32.to_be_bytes()); // mtime: DONT_CHANGE
  let reply = call(stream, NFS_PROGRAM, 8, &args, xid);
  assert_eq!(status(&reply), 0, "CREATE {name}");
  // CREATE3resok: obj (post_op_fh: follows bool, then the handle), then attributes and dir wcc.
  assert_eq!(
    u32::from_be_bytes(reply[4..8].try_into().unwrap()),
    1,
    "CREATE returned a handle"
  );
  read_opaque(&reply, 8).0
}

/// NFS WRITE `data` at offset zero to `file_fh` (FILE_SYNC).
pub(crate) fn write(stream: &mut TcpStream, file_fh: &[u8], data: &[u8], xid: u32) {
  let mut args = Vec::new();
  opaque(file_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // offset
  args.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes()); // count
  args.extend_from_slice(&2u32.to_be_bytes()); // stable: FILE_SYNC
  opaque(data, &mut args);
  let reply = call(stream, NFS_PROGRAM, 7, &args, xid);
  assert_eq!(status(&reply), 0, "WRITE succeeded");
}

/// NFS READ up to 400 bytes at offset zero from `file_fh`.
pub(crate) fn read(stream: &mut TcpStream, file_fh: &[u8], xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(file_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // offset
  args.extend_from_slice(&400u32.to_be_bytes()); // count
  let reply = call(stream, NFS_PROGRAM, 6, &args, xid);
  assert_eq!(status(&reply), 0, "READ succeeded");
  let mut off = 4;
  let follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
  off += 4;
  if follows == 1 {
    off += 84; // post_op_attr fattr3
  }
  off += 8; // count and eof
  read_opaque(&reply, off).0
}

/// READDIRPLUS `dir_fh` and return the entry names (skipping each entry's fileid, cookie, optional
/// attributes and optional handle).
pub(crate) fn readdirplus(stream: &mut TcpStream, dir_fh: &[u8], xid: u32) -> Vec<String> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // cookie
  args.extend_from_slice(&[0u8; 8]); // cookieverf
  args.extend_from_slice(&512u32.to_be_bytes()); // dircount (advisory)
  args.extend_from_slice(&8192u32.to_be_bytes()); // maxcount
  let reply = call(stream, NFS_PROGRAM, 17, &args, xid);
  assert_eq!(status(&reply), 0, "READDIRPLUS");
  let mut off = 4;
  let dir_follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
  off += 4;
  if dir_follows == 1 {
    off += 84; // dir post_op_attr
  }
  off += 8; // cookieverf
  let mut names = Vec::new();
  loop {
    let follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
    off += 4;
    if follows == 0 {
      break;
    }
    off += 8; // fileid
    let (name, next) = read_opaque(&reply, off);
    off = next;
    off += 8; // cookie
    let name_attr = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
    off += 4;
    if name_attr == 1 {
      off += 84; // name_attributes
    }
    let name_handle = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
    off += 4;
    if name_handle == 1 {
      let (_handle, next) = read_opaque(&reply, off);
      off = next;
    }
    names.push(String::from_utf8_lossy(&name).into_owned());
  }
  names
}
