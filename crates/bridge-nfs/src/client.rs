//! The mount's client side (§4.6 "macOS: mounting without exposing the capability"): what `slates
//! mount` needs to mount a volume **without the mount capability ever appearing where another user can
//! read it**.
//!
//! `mount_nfs` takes the export path (which carried the capability token) on its command line, where
//! `ps` shows it, and records it as the mount's `f_mntfromname`, which `mount` shows every local user.
//! Instead, the CLI:
//! 1. asks the daemon's MOUNT service for the export's root file handle itself, over a loopback
//!    socket only it holds ([`fetch_root_handle`]);
//! 2. passes that handle to the kernel in the XDR mount arguments ([`MountArgs::encode`],
//!    `NFS_MATTR_FH`), so the kernel makes no MOUNT call of its own;
//! 3. names the mount with a string that carries no secret (`NFS_MATTR_MNTFROM`).
//!
//! The kernel is then the only holder of the handle. This is how EdenFS's privhelper mounts.
//!
//! The argument layout is transcribed from Apple's `mount_nfs` (`assemble_mount_args`, NFS-343.100.5)
//! and the attribute numbers from the SDK's `<nfs/nfs.h>`: an XDR buffer of the args version, the
//! args length, the XDR-args version, the attribute bitmap, the attributes' length, then each present
//! attribute in `mount_nfs`'s order, the two lengths patched at the end.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::Duration;

use crate::mount::{
  MNTPATHLEN, MOUNT_PROGRAM, MOUNT_VERSION, MOUNTPROC3_MNT, MOUNTPROC3_UMNT, Mountstat3,
};
use crate::nfs::{MAX_FH, Nfsfh3};
use crate::rpc::{AUTH_SYS_MAX_GIDS, read_record, write_record};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: an RPC call message (RFC 5531 `msg_type` CALL).
const MSG_CALL: u32 = 0;
/// Format: an RPC reply message.
const MSG_REPLY: u32 = 1;
/// Format: the ONC RPC protocol version.
const RPC_VERSION: u32 = 2;
/// Format: a reply the server accepted.
const REPLY_ACCEPTED: u32 = 0;
/// Format: an accepted reply whose procedure ran.
const ACCEPT_SUCCESS: u32 = 0;
/// Format: the `AUTH_NONE` flavor (the verifier the call carries).
const AUTH_NONE: u32 = 0;
/// Format: the `AUTH_SYS` flavor (the credential the call carries: the mounting user).
const AUTH_SYS: u32 = 1;
/// Format: the longest `AUTH_SYS` machine name and verifier body a reply may carry (RFC 5531 caps an
/// `opaque_auth` body at 400 bytes).
const MAX_AUTH_BODY: usize = 400;
/// Format: the most authentication flavors a `mountres3` lists that this client reads past.
const MAX_AUTH_FLAVORS: usize = 16;

/// Why the root handle could not be fetched.
#[derive(Debug)]
pub enum FetchError {
  /// The loopback socket could not be connected, written or read.
  Io(std::io::Error),
  /// The reply was not a well-formed MOUNT reply to this call.
  Malformed,
  /// The daemon refused the mount with this status.
  Refused(u32),
}

impl std::fmt::Display for FetchError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      FetchError::Io(e) => write!(f, "the daemon's MOUNT service: {e}"),
      FetchError::Malformed => f.write_str("the daemon's MOUNT reply was malformed"),
      FetchError::Refused(status) => {
        write!(f, "the daemon refused the mount (mountstat3 {status})")
      }
    }
  }
}

/// The caller the MOUNT call names (`AUTH_SYS`): the mounting user's uid, primary gid and
/// supplementary gids (at most [`AUTH_SYS_MAX_GIDS`] are sent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
  /// The user id.
  pub uid: u32,
  /// The primary group id.
  pub gid: u32,
  /// The supplementary group ids.
  pub gids: Vec<u32>,
}

/// The record-marked MOUNT `MNT` call for `path` from `caller`, with transaction id `xid`.
pub fn mnt_call(xid: u32, caller: &Credentials, path: &str) -> Vec<u8> {
  mount_call(xid, caller, MOUNTPROC3_MNT, path)
}

/// The record-marked MOUNT `UMNT` call for `path` from `caller`, with transaction id `xid`: what the
/// kernel sends on unmount, and what any local process can send.
pub fn umnt_call(xid: u32, caller: &Credentials, path: &str) -> Vec<u8> {
  mount_call(xid, caller, MOUNTPROC3_UMNT, path)
}

/// A record-marked MOUNT call of `procedure` on `path`.
fn mount_call(xid: u32, caller: &Credentials, procedure: u32, path: &str) -> Vec<u8> {
  let mut body = XdrWriter::new();
  body.u32(xid);
  body.u32(MSG_CALL);
  body.u32(RPC_VERSION);
  body.u32(MOUNT_PROGRAM);
  body.u32(MOUNT_VERSION);
  body.u32(procedure);
  let mut credential = XdrWriter::new();
  credential.u32(0); // stamp
  credential.opaque(b"localhost");
  credential.u32(caller.uid);
  credential.u32(caller.gid);
  let gids = caller.gids.get(..AUTH_SYS_MAX_GIDS).unwrap_or(&caller.gids);
  credential.u32(u32::try_from(gids.len()).unwrap_or(0));
  for gid in gids {
    credential.u32(*gid);
  }
  body.u32(AUTH_SYS);
  body.opaque(&credential.into_bytes());
  body.u32(AUTH_NONE);
  body.opaque(&[]);
  body.opaque(path.as_bytes());
  write_record(&body.into_bytes())
}

/// The root file handle a MOUNT reply record (deframed) for transaction `xid` carries.
pub fn parse_mnt_reply(message: &[u8], xid: u32) -> Result<Nfsfh3, FetchError> {
  let mut reader = XdrReader::new(message);
  let malformed = |_| FetchError::Malformed;
  if reader.u32().map_err(malformed)? != xid
    || reader.u32().map_err(malformed)? != MSG_REPLY
    || reader.u32().map_err(malformed)? != REPLY_ACCEPTED
  {
    return Err(FetchError::Malformed);
  }
  let _verifier_flavor = reader.u32().map_err(malformed)?;
  let _verifier = reader.opaque(MAX_AUTH_BODY).map_err(malformed)?;
  if reader.u32().map_err(malformed)? != ACCEPT_SUCCESS {
    return Err(FetchError::Malformed);
  }
  let status = reader.u32().map_err(malformed)?;
  if status != Mountstat3::Ok.wire() {
    return Err(FetchError::Refused(status));
  }
  let handle = reader.opaque(MAX_FH).map_err(malformed)?.to_vec();
  let flavors = usize::try_from(reader.u32().map_err(malformed)?).unwrap_or(usize::MAX);
  if flavors > MAX_AUTH_FLAVORS {
    return Err(FetchError::Malformed);
  }
  // Every flavor the count promises must be present: a reply cut short is refused, never accepted.
  for _ in 0..flavors {
    reader.u32().map_err(malformed)?;
  }
  Ok(Nfsfh3(handle))
}

/// Asks the daemon's MOUNT service on loopback `port` for `path`'s root file handle, as `caller`, the
/// exchange bounded by `deadline` (each connect, write and read). The socket is this process's own,
/// so the capability `path` carries is seen by no other user.
pub fn fetch_root_handle(
  port: u16,
  caller: &Credentials,
  path: &str,
  deadline: Duration,
) -> Result<Nfsfh3, FetchError> {
  if path.len() > MNTPATHLEN {
    return Err(FetchError::Refused(Mountstat3::Nametoolong.wire()));
  }
  let xid = std::process::id();
  let reply = exchange(port, &mnt_call(xid, caller, path), deadline)?;
  parse_mnt_reply(&reply, xid)
}

/// Sends one record-marked MOUNT `call` to the daemon on loopback `port` and returns the reply message
/// (deframed), each of connect, send and receive bounded by `deadline`.
pub fn exchange(port: u16, call: &[u8], deadline: Duration) -> Result<Vec<u8>, FetchError> {
  let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
  let mut stream = TcpStream::connect_timeout(&address.into(), deadline).map_err(FetchError::Io)?;
  stream
    .set_read_timeout(Some(deadline))
    .map_err(FetchError::Io)?;
  stream
    .set_write_timeout(Some(deadline))
    .map_err(FetchError::Io)?;
  stream.write_all(call).map_err(FetchError::Io)?;
  let mut received = Vec::new();
  let mut chunk = [0u8; MAX_REPLY_READ];
  loop {
    match read_record(&received) {
      Ok((message, _)) => return Ok(message),
      Err(crate::rpc::RpcError::Incomplete) => {}
      Err(_) => return Err(FetchError::Malformed),
    }
    let read = stream.read(&mut chunk).map_err(FetchError::Io)?;
    if read == 0 || received.len().saturating_add(read) > MAX_REPLY_BYTES {
      return Err(FetchError::Malformed);
    }
    received.extend_from_slice(chunk.get(..read).ok_or(FetchError::Malformed)?);
  }
}

/// Shape: one read of the MOUNT reply; a `mountres3` is a few dozen bytes.
const MAX_REPLY_READ: usize = 512;
/// Format: the largest MOUNT reply this client accepts: the record marker, the reply header with a
/// verifier at its cap, the status, a handle at `NFS3_FHSIZE` and the flavor list at its cap, each an
/// XDR word or an opaque.
const MAX_REPLY_BYTES: usize = 4 * 8 + MAX_AUTH_BODY + 4 + MAX_FH + 4 * (1 + MAX_AUTH_FLAVORS);

/// Format: `NFS_ARGSVERSION_XDR` (`<nfs/nfs.h>`): the mount arguments are an XDR buffer.
const ARGS_VERSION_XDR: u32 = 88;
/// Format: `NFS_XDRARGS_VERSION_0`.
const XDR_ARGS_VERSION: u32 = 0;
/// Format: `NFS_MATTR_BITMAP_LEN` and `NFS_MFLAG_BITMAP_LEN`, in 32-bit words.
const MATTR_WORDS: usize = 2;
/// Format: `NFS_MFLAG_BITMAP_LEN`.
const MFLAG_WORDS: usize = 1;

/// Format: the `NFS_MATTR_*` attribute numbers the slates mount passes (`<nfs/nfs.h>`).
mod mattr {
  /// Format: `NFS_MATTR_FLAGS`: the mount flags' mask and values.
  pub(super) const FLAGS: u32 = 0;
  /// Format: `NFS_MATTR_NFS_VERSION`.
  pub(super) const NFS_VERSION: u32 = 1;
  /// Format: `NFS_MATTR_READ_SIZE`: the largest READ the client sends.
  pub(super) const READ_SIZE: u32 = 3;
  /// Format: `NFS_MATTR_WRITE_SIZE`: the largest WRITE the client sends.
  pub(super) const WRITE_SIZE: u32 = 4;
  /// Format: `NFS_MATTR_ATTRCACHE_REG_MIN` (the four cache times follow in order).
  pub(super) const ATTRCACHE_REG_MIN: u32 = 7;
  /// Format: `NFS_MATTR_ATTRCACHE_REG_MAX`.
  pub(super) const ATTRCACHE_REG_MAX: u32 = 8;
  /// Format: `NFS_MATTR_ATTRCACHE_DIR_MIN`.
  pub(super) const ATTRCACHE_DIR_MIN: u32 = 9;
  /// Format: `NFS_MATTR_ATTRCACHE_DIR_MAX`.
  pub(super) const ATTRCACHE_DIR_MAX: u32 = 10;
  /// Format: `NFS_MATTR_LOCK_MODE`.
  pub(super) const LOCK_MODE: u32 = 11;
  /// Format: `NFS_MATTR_SOCKET_TYPE`.
  pub(super) const SOCKET_TYPE: u32 = 14;
  /// Format: `NFS_MATTR_NFS_PORT`.
  pub(super) const NFS_PORT: u32 = 15;
  /// Format: `NFS_MATTR_MOUNT_PORT`: where the kernel sends its `UMNT` on unmount.
  pub(super) const MOUNT_PORT: u32 = 16;
  /// Format: `NFS_MATTR_FH`: the root file handle, so the kernel skips the MOUNT protocol.
  pub(super) const FH: u32 = 20;
  /// Format: `NFS_MATTR_FS_LOCATIONS` (always present).
  pub(super) const FS_LOCATIONS: u32 = 21;
  /// Format: `NFS_MATTR_MNTFLAGS` (always present).
  pub(super) const MNTFLAGS: u32 = 22;
  /// Format: `NFS_MATTR_MNTFROM`: the fixed `f_mntfromname`.
  pub(super) const MNTFROM: u32 = 23;
}

/// Format: the `NFS_MFLAG_*` flag numbers the slates mount sets or clears (`<nfs/nfs.h>`).
mod mflag {
  /// Format: `NFS_MFLAG_SOFT`: requests fail rather than hang on an unresponsive daemon.
  pub(super) const SOFT: u32 = 0;
  /// Format: `NFS_MFLAG_INTR`: a waiting call can be interrupted.
  pub(super) const INTR: u32 = 1;
  /// Format: `NFS_MFLAG_RESVPORT`: cleared, so no privileged source port is needed (R10).
  pub(super) const RESVPORT: u32 = 2;
  /// Format: `NFS_MFLAG_CALLUMNT`: the kernel sends MOUNT's `UMNT` of the mount's source path on unmount,
  /// which ends the mount's attachment once the daemon confirms the unmount (§4.6 A-34).
  pub(super) const CALLUMNT: u32 = 5;
  /// Format: `NFS_MFLAG_RDIRPLUS`: READDIRPLUS for listings.
  pub(super) const RDIRPLUS: u32 = 6;
}

/// Format: `NFS_LOCK_MODE_LOCAL`: advisory locks are the kernel's own (`locallocks`).
const LOCK_MODE_LOCAL: u32 = 2;
/// Format: the NFS version the mount speaks.
const NFS_V3: u32 = 3;
/// Format: the socket type string for TCP over IPv4 (`mount_nfs`'s `get_socket_type_mount_arg`).
const SOCKET_TCP4: &str = "tcp4";
/// Format: the loopback server's name and universal address in the location list.
const SERVER_NAME: &str = "localhost";
/// Format: the IPv4 loopback address in universal-address form.
const SERVER_ADDRESS: &str = "127.0.0.1";

/// The arguments of one slates NFSv3 loopback mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountArgs {
  /// The daemon's loopback NFS port.
  pub port: u16,
  /// The export's root file handle ([`fetch_root_handle`]).
  pub handle: Nfsfh3,
  /// The attribute cache time for files and directories, whole seconds.
  pub attr_cache_seconds: u32,
  /// The largest READ and WRITE the client sends: the server's own transfer maximum, so a file of up to that size
  /// moves in one call. The kernel takes the smaller of this and its own limit.
  pub transfer_bytes: u32,
  /// The VFS mount flags (`MNT_NOSUID`, `MNT_RDONLY`, …) as `mount(2)` takes them.
  pub mnt_flags: u32,
  /// What the mount table shows as the source: a name with no secret in it.
  pub mnt_from: String,
  /// The path components the location list names (`nfsstat -m` shows them every user, so never a
  /// capability). The handle, not this path, is what the mount serves; the kernel's `UMNT` names the
  /// mount by its source (`mnt_from`), `slates:/<name>` giving `/<name>`.
  pub path: Vec<String>,
}

impl MountArgs {
  /// The XDR buffer `mount("nfs", dir, flags, buffer)` takes.
  pub fn encode(&self) -> Vec<u8> {
    let mut bitmap = [0u32; MATTR_WORDS];
    for attribute in [
      mattr::FLAGS,
      mattr::NFS_VERSION,
      mattr::READ_SIZE,
      mattr::WRITE_SIZE,
      mattr::ATTRCACHE_REG_MIN,
      mattr::ATTRCACHE_REG_MAX,
      mattr::ATTRCACHE_DIR_MIN,
      mattr::ATTRCACHE_DIR_MAX,
      mattr::LOCK_MODE,
      mattr::SOCKET_TYPE,
      mattr::NFS_PORT,
      mattr::MOUNT_PORT,
      mattr::FH,
      mattr::FS_LOCATIONS,
      mattr::MNTFLAGS,
      mattr::MNTFROM,
    ] {
      set_bit(&mut bitmap, attribute);
    }
    let mut mask = [0u32; MFLAG_WORDS];
    let mut value = [0u32; MFLAG_WORDS];
    for flag in [
      mflag::SOFT,
      mflag::INTR,
      mflag::RESVPORT,
      mflag::CALLUMNT,
      mflag::RDIRPLUS,
    ] {
      set_bit(&mut mask, flag);
    }
    for flag in [mflag::SOFT, mflag::INTR, mflag::CALLUMNT, mflag::RDIRPLUS] {
      set_bit(&mut value, flag);
    }

    // The attributes, in `assemble_mount_args`'s order.
    let mut attrs = XdrWriter::new();
    push_words(&mut attrs, &mask);
    push_words(&mut attrs, &value);
    attrs.u32(NFS_V3);
    attrs.u32(self.transfer_bytes);
    attrs.u32(self.transfer_bytes);
    // The four cache times, in attribute order: regular min and max, directory min and max.
    for _ in [
      mattr::ATTRCACHE_REG_MIN,
      mattr::ATTRCACHE_REG_MAX,
      mattr::ATTRCACHE_DIR_MIN,
      mattr::ATTRCACHE_DIR_MAX,
    ] {
      attrs.u32(self.attr_cache_seconds);
      attrs.u32(0); // nanoseconds
    }
    attrs.u32(LOCK_MODE_LOCAL);
    attrs.opaque(SOCKET_TCP4.as_bytes());
    attrs.u32(u32::from(self.port));
    attrs.u32(u32::from(self.port)); // the MOUNT port: the same server
    attrs.opaque(&self.handle.0);
    // One location: one server with one address, then the path's components.
    attrs.u32(1);
    attrs.u32(1);
    attrs.opaque(SERVER_NAME.as_bytes());
    attrs.u32(1);
    attrs.opaque(SERVER_ADDRESS.as_bytes());
    attrs.u32(0); // empty server info
    attrs.u32(u32::try_from(self.path.len()).unwrap_or(0));
    for component in &self.path {
      attrs.opaque(component.as_bytes());
    }
    attrs.u32(0); // empty location info
    attrs.u32(self.mnt_flags);
    attrs.opaque(self.mnt_from.as_bytes());
    let attrs = attrs.into_bytes();

    let mut out = XdrWriter::new();
    out.u32(ARGS_VERSION_XDR);
    // The args length counts from itself to the end, plus the version word before it.
    let args_len = size_of::<u32>() // the args length itself
      + size_of::<u32>() // the XDR-args version
      + size_of::<u32>() * (1 + MATTR_WORDS) // the bitmap
      + size_of::<u32>() // the attrs length
      + attrs.len()
      + size_of::<u32>(); // the leading version word
    out.u32(u32::try_from(args_len).unwrap_or(u32::MAX));
    out.u32(XDR_ARGS_VERSION);
    push_words(&mut out, &bitmap);
    out.u32(u32::try_from(attrs.len()).unwrap_or(u32::MAX));
    out.fixed(&attrs);
    out.into_bytes()
  }
}

/// Sets bit `bit` of a word bitmap (bit 0 is the low bit of word 0, as `NFS_BITMAP_SET`).
fn set_bit(words: &mut [u32], bit: u32) {
  /// Format: the bits in one bitmap word.
  const WORD_BITS: u32 = u32::BITS;
  let word = usize::try_from(bit / WORD_BITS).unwrap_or(usize::MAX);
  if let Some(slot) = words.get_mut(word) {
    *slot |= 1 << (bit % WORD_BITS);
  }
}

/// Writes a word array: its length, then each word (`xb_add_word_array`).
fn push_words(writer: &mut XdrWriter, words: &[u32]) {
  writer.u32(u32::try_from(words.len()).unwrap_or(0));
  for word in words {
    writer.u32(*word);
  }
}
