//! The NFSv3 MOUNT protocol (RFC 1813 Appendix I): the separate RPC program a client calls before
//! any NFS procedure, to turn an export path into the export's root file handle. This is the pure
//! wire layer — the status codes, the `MNT` request path and the `mountres3` reply — over the XDR
//! codec, golden and hostile-input tested with no socket. Resolving a path to a volume's root
//! handle needs the catalog and is the procedures layer (owed); the reply here carries whatever
//! root handle and authentication flavors the procedures choose.

use crate::nfs::Nfsfh3;
use crate::xdr::{XdrError, XdrReader, XdrWriter};

/// Format: the MOUNT program number (RFC 1813 Appendix I).
pub const MOUNT_PROGRAM: u32 = 100_005;
/// Format: the MOUNT protocol version this bridge speaks.
pub const MOUNT_VERSION: u32 = 3;
/// Format: MOUNTPROC3_NULL — a ping, no arguments and no results.
pub const MOUNTPROC3_NULL: u32 = 0;
/// Format: MOUNTPROC3_MNT — mount a path, returning its root file handle.
pub const MOUNTPROC3_MNT: u32 = 1;
/// Format: MOUNTPROC3_DUMP — list the server's current mount entries.
pub const MOUNTPROC3_DUMP: u32 = 2;
/// Format: MOUNTPROC3_UMNT — remove one mount entry.
pub const MOUNTPROC3_UMNT: u32 = 3;
/// Format: MOUNTPROC3_UMNTALL — remove all of a client's mount entries.
pub const MOUNTPROC3_UMNTALL: u32 = 4;
/// Format: MOUNTPROC3_EXPORT — list the exported filesystems.
pub const MOUNTPROC3_EXPORT: u32 = 5;
/// Format: MNTPATHLEN — the maximum bytes in a mount path (RFC 1813 Appendix I). A longer path is
/// refused before allocating.
pub const MNTPATHLEN: usize = 1024;

/// The status of a MOUNT reply (RFC 1813 `mountstat3`). Only the values a userspace loopback
/// server returns are modeled; the wire value is the enum discriminant.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mountstat3 {
  /// Format: MNT3_OK — the mount succeeded; the root handle and auth flavors follow.
  Ok = 0,
  /// Format: MNT3ERR_PERM — not owner.
  Perm = 1,
  /// Format: MNT3ERR_NOENT — no such export.
  Noent = 2,
  /// Format: MNT3ERR_IO — an I/O error.
  Io = 5,
  /// Format: MNT3ERR_ACCES — permission denied.
  Acces = 13,
  /// Format: MNT3ERR_NOTDIR — the path is not a directory.
  Notdir = 20,
  /// Format: MNT3ERR_INVAL — an invalid argument.
  Inval = 22,
  /// Format: MNT3ERR_NAMETOOLONG — the path is too long.
  Nametoolong = 63,
  /// Format: MNT3ERR_NOTSUPP — the operation is not supported.
  Notsupp = 10004,
  /// Format: MNT3ERR_SERVERFAULT — an error not covered by the protocol.
  ServerFault = 10006,
}

impl Mountstat3 {
  /// The wire value.
  pub fn wire(self) -> u32 {
    self as u32
  }

  /// Writes the status.
  pub fn encode(self, writer: &mut XdrWriter) {
    writer.u32(self.wire());
  }
}

/// A MOUNT `MNT` reply (`mountres3`): on success the export's root file handle and the
/// authentication flavors the server accepts, otherwise a failure status. The variants make the
/// encoding correct by construction — a success always carries its payload, a failure only a
/// status (which the caller sets to a non-`Ok` value).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountReply {
  /// The mount succeeded.
  Ok {
    /// The export's root file handle.
    handle: Nfsfh3,
    /// The authentication flavors the server accepts (e.g. `AUTH_NONE`, `AUTH_SYS`).
    auth_flavors: Vec<u32>,
  },
  /// The mount failed with this (non-`Ok`) status.
  Err(Mountstat3),
}

impl MountReply {
  /// Writes the reply: the status, then on success the root handle and the auth-flavor list.
  pub fn encode(&self, writer: &mut XdrWriter) {
    match self {
      MountReply::Ok {
        handle,
        auth_flavors,
      } => {
        Mountstat3::Ok.encode(writer);
        handle.encode(writer);
        writer.u32(u32::try_from(auth_flavors.len()).unwrap_or(u32::MAX));
        for flavor in auth_flavors {
          writer.u32(*flavor);
        }
      }
      MountReply::Err(status) => status.encode(writer),
    }
  }
}

/// Parses a `MNT` request: a single mount path, capped at [`MNTPATHLEN`].
pub fn parse_mount_path<'a>(reader: &mut XdrReader<'a>) -> Result<&'a str, XdrError> {
  reader.string(MNTPATHLEN)
}
