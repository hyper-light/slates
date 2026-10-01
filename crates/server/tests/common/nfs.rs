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

/// Format: the NFS and MOUNT version every procedure here speaks (RFC 1813), unless it names another.
pub(crate) const VERSION_3: u32 = 3;

pub(crate) fn call(
  stream: &mut TcpStream,
  program: u32,
  procedure: u32,
  args: &[u8],
  xid: u32,
) -> Vec<u8> {
  call_version(stream, program, VERSION_3, procedure, args, xid)
}

/// One ONC RPC call of `program` at `version` (AUTH_NONE): the procedure's results, after the reply's
/// accept status.
pub(crate) fn call_version(
  stream: &mut TcpStream,
  program: u32,
  version: u32,
  procedure: u32,
  args: &[u8],
  xid: u32,
) -> Vec<u8> {
  let mut body = Vec::new();
  for field in [xid, 0, 2, program, version, procedure, 0, 0, 0, 0] {
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

/// MOUNT MNT `path` → its status, without asserting it succeeded (a capability that no longer authorizes
/// its volume is refused).
pub(crate) fn mount_status(stream: &mut TcpStream, path: &str, xid: u32) -> u32 {
  let mut args = Vec::new();
  opaque(path.as_bytes(), &mut args);
  status(&call(stream, MOUNT_PROGRAM, 1, &args, xid))
}

/// MOUNT UMNT `path` — what the kernel sends when a mount is removed (`umount`); a void reply (RFC 1813
/// §5.2.3), so nothing to assert on but its arrival.
pub(crate) fn umnt(stream: &mut TcpStream, path: &str, xid: u32) {
  let mut args = Vec::new();
  opaque(path.as_bytes(), &mut args);
  let reply = call(stream, MOUNT_PROGRAM, 3, &args, xid);
  assert!(reply.is_empty(), "UMNT {path} is void");
}

/// The NFS status of a READ of up to 400 bytes at offset zero from `file_fh` — without asserting it
/// succeeded, so a refusal (a capability that no longer authorizes the handle) is what the test reads.
pub(crate) fn read_status(stream: &mut TcpStream, file_fh: &[u8], xid: u32) -> u32 {
  let mut args = Vec::new();
  opaque(file_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // offset
  args.extend_from_slice(&400u32.to_be_bytes()); // count
  status(&call(stream, NFS_PROGRAM, 6, &args, xid))
}

/// NFS LOOKUP `name` in `dir_fh` → the status, and whether the reply carried the object's attributes
/// (`obj_attributes`, a `post_op_attr`: RFC 1813 §3.3.3) — without asserting it succeeded.
pub(crate) fn lookup_carries_attributes(
  stream: &mut TcpStream,
  dir_fh: &[u8],
  name: &str,
  xid: u32,
) -> (u32, bool) {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  opaque(name.as_bytes(), &mut args);
  let reply = call(stream, NFS_PROGRAM, 3, &args, xid);
  let looked = status(&reply);
  if looked != 0 {
    return (looked, false);
  }
  let (_, after_handle) = read_opaque(&reply, 4);
  let follows = u32::from_be_bytes(reply[after_handle..after_handle + 4].try_into().unwrap());
  (looked, follows == 1)
}

/// NFS LOOKUP `name` in `dir_fh` → the child's file handle.
pub(crate) fn lookup(stream: &mut TcpStream, dir_fh: &[u8], name: &str, xid: u32) -> Vec<u8> {
  lookup_status(stream, dir_fh, name, xid)
    .unwrap_or_else(|status| panic!("LOOKUP {name}: status {status}, expected 0"))
}

/// NFS LOOKUP `name` in `dir_fh` → the child's file handle, or the status the server answered instead — so a
/// test can report the server's state beside a retry-later (`NFS3ERR_JUKEBOX`) or stale answer.
pub(crate) fn lookup_status(
  stream: &mut TcpStream,
  dir_fh: &[u8],
  name: &str,
  xid: u32,
) -> Result<Vec<u8>, u32> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  opaque(name.as_bytes(), &mut args);
  let reply = call(stream, NFS_PROGRAM, 3, &args, xid);
  match status(&reply) {
    0 => Ok(read_opaque(&reply, 4).0),
    answered => Err(answered),
  }
}

/// NFS GETATTR of `fh` → its `(mode, uid, gid)` — what `stat` shows through a kernel mount, and what a
/// takeover successor must reproduce for the dead owner's tree (fattr3: type, mode, nlink, uid, gid, …).
pub(crate) fn owner_and_mode(stream: &mut TcpStream, fh: &[u8], xid: u32) -> (u32, u32, u32) {
  owner_and_mode_status(stream, fh, xid)
    .unwrap_or_else(|status| panic!("GETATTR: status {status}, expected 0"))
}

/// NFS GETATTR of `fh` → its `(mode, uid, gid)`, or the status the server answered instead.
pub(crate) fn owner_and_mode_status(
  stream: &mut TcpStream,
  fh: &[u8],
  xid: u32,
) -> Result<(u32, u32, u32), u32> {
  let mut args = Vec::new();
  opaque(fh, &mut args);
  let reply = call(stream, NFS_PROGRAM, 1, &args, xid);
  match status(&reply) {
    0 => {
      let field = |at: usize| u32::from_be_bytes(reply[at..at + 4].try_into().unwrap());
      Ok((field(8), field(16), field(20)))
    }
    answered => Err(answered),
  }
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

/// NFS MKDIR `name` in `dir_fh` with mode 0755 → the new directory's handle.
pub(crate) fn mkdir(stream: &mut TcpStream, dir_fh: &[u8], name: &str, xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  opaque(name.as_bytes(), &mut args);
  // sattr3: mode set to 0755, everything else unset, times unchanged.
  args.extend_from_slice(&1u32.to_be_bytes()); // set_mode: yes
  args.extend_from_slice(&0o755u32.to_be_bytes());
  for _unset in 0..5 {
    args.extend_from_slice(&0u32.to_be_bytes()); // uid, gid, size: no; atime, mtime: DONT_CHANGE
  }
  let reply = call(stream, NFS_PROGRAM, 9, &args, xid);
  assert_eq!(status(&reply), 0, "MKDIR {name}");
  // MKDIR3resok: obj (post_op_fh: follows bool, then the handle), then attributes and dir wcc.
  assert_eq!(
    u32::from_be_bytes(reply[4..8].try_into().unwrap()),
    1,
    "MKDIR returned a handle"
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

/// NFS WRITE `data` at offset zero to `file_fh` (FILE_SYNC): the status the server answered.
pub(crate) fn write_status(stream: &mut TcpStream, file_fh: &[u8], data: &[u8], xid: u32) -> u32 {
  let mut args = Vec::new();
  opaque(file_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // offset
  args.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes()); // count
  args.extend_from_slice(&2u32.to_be_bytes()); // stable: FILE_SYNC
  opaque(data, &mut args);
  status(&call(stream, NFS_PROGRAM, 7, &args, xid))
}

/// NFS READ up to 400 bytes at offset zero from `file_fh`.
pub(crate) fn read(stream: &mut TcpStream, file_fh: &[u8], xid: u32) -> Vec<u8> {
  read_bytes_status(stream, file_fh, xid)
    .unwrap_or_else(|status| panic!("READ: status {status}, expected 0"))
}

/// NFS READ of `file_fh` from offset 0 → the bytes, or the status the server answered instead.
pub(crate) fn read_bytes_status(
  stream: &mut TcpStream,
  file_fh: &[u8],
  xid: u32,
) -> Result<Vec<u8>, u32> {
  let mut args = Vec::new();
  opaque(file_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // offset
  args.extend_from_slice(&400u32.to_be_bytes()); // count
  let reply = call(stream, NFS_PROGRAM, 6, &args, xid);
  let answered = status(&reply);
  if answered != 0 {
    return Err(answered);
  }
  let mut off = 4;
  let follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
  off += 4;
  if follows == 1 {
    off += 84; // post_op_attr fattr3
  }
  off += 8; // count and eof
  Ok(read_opaque(&reply, off).0)
}

/// NFS FSSTAT `fh` → the volume's (total bytes, free bytes): the quota the export serves as the
/// filesystem's capacity (the truthful `statfs`, BUG-9), what a recovered volume's acknowledged size
/// policy is observed through.
pub(crate) fn fsstat(stream: &mut TcpStream, fh: &[u8], xid: u32) -> (u64, u64) {
  let mut args = Vec::new();
  opaque(fh, &mut args);
  let reply = call(stream, NFS_PROGRAM, 18, &args, xid);
  assert_eq!(status(&reply), 0, "FSSTAT succeeded");
  let mut off = 4;
  let follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
  off += 4;
  if follows == 1 {
    off += 84; // post_op_attr fattr3
  }
  let tbytes = u64::from_be_bytes(reply[off..off + 8].try_into().unwrap());
  let fbytes = u64::from_be_bytes(reply[off + 8..off + 16].try_into().unwrap());
  (tbytes, fbytes)
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

/// An `nfstime3` as (seconds, nanoseconds).
pub(crate) type Time3 = (u32, u32);

/// The three times an NFSv3 GETATTR of `fh` reports: (atime, mtime, ctime).
pub(crate) fn times(stream: &mut TcpStream, fh: &[u8], xid: u32) -> [Time3; 3] {
  let mut args = Vec::new();
  opaque(fh, &mut args);
  let reply = call(stream, NFS_PROGRAM, 1, &args, xid);
  assert_eq!(status(&reply), 0, "GETATTR");
  // fattr3 after the status: type, mode, nlink, uid, gid (4 bytes each), size, used, rdev, fsid,
  // fileid (8 each), then atime, mtime, ctime (each two words).
  let at = 4 + 5 * 4 + 5 * 8;
  let word = |offset: usize| u32::from_be_bytes(reply[offset..offset + 4].try_into().unwrap());
  [0, 1, 2].map(|index| (word(at + index * 8), word(at + index * 8 + 4)))
}

/// NFSv3 SETATTR of `fh`'s access and modification times to the client-supplied `atime` and `mtime`
/// (`SET_TO_CLIENT_TIME`), nothing else set and no guard.
pub(crate) fn set_times(stream: &mut TcpStream, fh: &[u8], atime: Time3, mtime: Time3, xid: u32) {
  /// Format: `time_how` SET_TO_CLIENT_TIME (RFC 1813 §2.6).
  const SET_TO_CLIENT_TIME: u32 = 2;
  let mut args = Vec::new();
  opaque(fh, &mut args);
  for _ in 0..4 {
    args.extend_from_slice(&0u32.to_be_bytes()); // mode, uid, gid, size: not set
  }
  for (seconds, nanoseconds) in [atime, mtime] {
    for field in [SET_TO_CLIENT_TIME, seconds, nanoseconds] {
      args.extend_from_slice(&field.to_be_bytes());
    }
  }
  args.extend_from_slice(&0u32.to_be_bytes()); // no guard
  let reply = call(stream, NFS_PROGRAM, 2, &args, xid);
  assert_eq!(status(&reply), 0, "SETATTR of the times");
}
