//! The NFSv4 attributes (`fattr4`, RFC 8881 §5.8; `xattr_support`, RFC 8276 §8.1): which the server
//! supports, and the encoding of a requested set from an object's v3 attributes and the filesystem's
//! figures. The values are encoded in ascending attribute number, as the attribute list requires.

use super::types::Bitmap;
use crate::nfs::{Fattr3, Ftype3, Nfsfh3};
use crate::xdr::XdrWriter;

/// Format: the attribute numbers this server encodes (RFC 8881 §5.8; RFC 8276 §8.1).
pub mod number {
  /// Format: `FATTR4_SUPPORTED_ATTRS`.
  pub const SUPPORTED_ATTRS: u32 = 0;
  /// Format: `FATTR4_TYPE`.
  pub const TYPE: u32 = 1;
  /// Format: `FATTR4_FH_EXPIRE_TYPE`.
  pub const FH_EXPIRE_TYPE: u32 = 2;
  /// Format: `FATTR4_CHANGE`.
  pub const CHANGE: u32 = 3;
  /// Format: `FATTR4_SIZE`.
  pub const SIZE: u32 = 4;
  /// Format: `FATTR4_LINK_SUPPORT`.
  pub const LINK_SUPPORT: u32 = 5;
  /// Format: `FATTR4_SYMLINK_SUPPORT`.
  pub const SYMLINK_SUPPORT: u32 = 6;
  /// Format: `FATTR4_NAMED_ATTR`.
  pub const NAMED_ATTR: u32 = 7;
  /// Format: `FATTR4_FSID`.
  pub const FSID: u32 = 8;
  /// Format: `FATTR4_UNIQUE_HANDLES`.
  pub const UNIQUE_HANDLES: u32 = 9;
  /// Format: `FATTR4_LEASE_TIME`.
  pub const LEASE_TIME: u32 = 10;
  /// Format: `FATTR4_RDATTR_ERROR`.
  pub const RDATTR_ERROR: u32 = 11;
  /// Format: `FATTR4_CANSETTIME`.
  pub const CANSETTIME: u32 = 15;
  /// Format: `FATTR4_CASE_INSENSITIVE`.
  pub const CASE_INSENSITIVE: u32 = 16;
  /// Format: `FATTR4_CASE_PRESERVING`.
  pub const CASE_PRESERVING: u32 = 17;
  /// Format: `FATTR4_CHOWN_RESTRICTED`.
  pub const CHOWN_RESTRICTED: u32 = 18;
  /// Format: `FATTR4_FILEHANDLE`.
  pub const FILEHANDLE: u32 = 19;
  /// Format: `FATTR4_FILEID`.
  pub const FILEID: u32 = 20;
  /// Format: `FATTR4_FILES_AVAIL`.
  pub const FILES_AVAIL: u32 = 21;
  /// Format: `FATTR4_FILES_FREE`.
  pub const FILES_FREE: u32 = 22;
  /// Format: `FATTR4_FILES_TOTAL`.
  pub const FILES_TOTAL: u32 = 23;
  /// Format: `FATTR4_HOMOGENEOUS`.
  pub const HOMOGENEOUS: u32 = 26;
  /// Format: `FATTR4_MAXFILESIZE`.
  pub const MAXFILESIZE: u32 = 27;
  /// Format: `FATTR4_MAXLINK`.
  pub const MAXLINK: u32 = 28;
  /// Format: `FATTR4_MAXNAME`.
  pub const MAXNAME: u32 = 29;
  /// Format: `FATTR4_MAXREAD`.
  pub const MAXREAD: u32 = 30;
  /// Format: `FATTR4_MAXWRITE`.
  pub const MAXWRITE: u32 = 31;
  /// Format: `FATTR4_MODE`.
  pub const MODE: u32 = 33;
  /// Format: `FATTR4_NO_TRUNC`.
  pub const NO_TRUNC: u32 = 34;
  /// Format: `FATTR4_NUMLINKS`.
  pub const NUMLINKS: u32 = 35;
  /// Format: `FATTR4_OWNER`.
  pub const OWNER: u32 = 36;
  /// Format: `FATTR4_OWNER_GROUP`.
  pub const OWNER_GROUP: u32 = 37;
  /// Format: `FATTR4_RAWDEV`.
  pub const RAWDEV: u32 = 41;
  /// Format: `FATTR4_SPACE_AVAIL`.
  pub const SPACE_AVAIL: u32 = 42;
  /// Format: `FATTR4_SPACE_FREE`.
  pub const SPACE_FREE: u32 = 43;
  /// Format: `FATTR4_SPACE_TOTAL`.
  pub const SPACE_TOTAL: u32 = 44;
  /// Format: `FATTR4_SPACE_USED`.
  pub const SPACE_USED: u32 = 45;
  /// Format: `FATTR4_TIME_ACCESS`.
  pub const TIME_ACCESS: u32 = 47;
  /// Format: `FATTR4_TIME_DELTA`.
  pub const TIME_DELTA: u32 = 51;
  /// Format: `FATTR4_TIME_METADATA`.
  pub const TIME_METADATA: u32 = 52;
  /// Format: `FATTR4_TIME_MODIFY`.
  pub const TIME_MODIFY: u32 = 53;
  /// Format: `FATTR4_MOUNTED_ON_FILEID`.
  pub const MOUNTED_ON_FILEID: u32 = 55;
  /// Format: `FATTR4_SUPPATTR_EXCLCREAT` (NFSv4.1, REQUIRED).
  pub const SUPPATTR_EXCLCREAT: u32 = 75;
  /// Format: `FATTR4_XATTR_SUPPORT` (RFC 8276).
  pub const XATTR_SUPPORT: u32 = 82;
}

/// Format: `nfs_ftype4` values (RFC 8881 §3.3.4).
mod ftype4 {
  /// Format: `NF4REG`.
  pub(super) const REG: u32 = 1;
  /// Format: `NF4DIR`.
  pub(super) const DIR: u32 = 2;
  /// Format: `NF4BLK`.
  pub(super) const BLK: u32 = 3;
  /// Format: `NF4CHR`.
  pub(super) const CHR: u32 = 4;
  /// Format: `NF4LNK`.
  pub(super) const LNK: u32 = 5;
  /// Format: `NF4SOCK`.
  pub(super) const SOCK: u32 = 6;
  /// Format: `NF4FIFO`.
  pub(super) const FIFO: u32 = 7;
}

/// Format: `FH4_PERSISTENT`: slates handles never expire (`(volume, inode, generation)`, D-4).
const FH4_PERSISTENT: u32 = 0;

/// The filesystem-wide figures an attribute set may ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsFigures {
  /// The filesystem id (the export's `fsid`), as `(major, minor)`.
  pub fsid: (u64, u64),
  /// The lease, in seconds.
  pub lease_seconds: u32,
  /// Whether names fold case (a folding volume, EQUIVALENCE §4).
  pub case_insensitive: bool,
  /// Files available, free and total.
  pub files: (u64, u64, u64),
  /// Bytes available, free and total.
  pub space: (u64, u64, u64),
  /// The largest file, in bytes.
  pub max_file_size: u64,
  /// The most hard links to one file.
  pub max_link: u32,
  /// The longest name, in bytes.
  pub max_name: u32,
  /// The largest READ and WRITE, in bytes.
  pub max_io: u64,
}

/// The attributes this server supports: every number in [`number`].
pub fn supported() -> Bitmap {
  use number::*;
  Bitmap::of(&[
    SUPPORTED_ATTRS,
    TYPE,
    FH_EXPIRE_TYPE,
    CHANGE,
    SIZE,
    LINK_SUPPORT,
    SYMLINK_SUPPORT,
    NAMED_ATTR,
    FSID,
    UNIQUE_HANDLES,
    LEASE_TIME,
    RDATTR_ERROR,
    CANSETTIME,
    CASE_INSENSITIVE,
    CASE_PRESERVING,
    CHOWN_RESTRICTED,
    FILEHANDLE,
    FILEID,
    FILES_AVAIL,
    FILES_FREE,
    FILES_TOTAL,
    HOMOGENEOUS,
    MAXFILESIZE,
    MAXLINK,
    MAXNAME,
    MAXREAD,
    MAXWRITE,
    MODE,
    NO_TRUNC,
    NUMLINKS,
    OWNER,
    OWNER_GROUP,
    RAWDEV,
    SPACE_AVAIL,
    SPACE_FREE,
    SPACE_TOTAL,
    SPACE_USED,
    TIME_ACCESS,
    TIME_DELTA,
    TIME_METADATA,
    TIME_MODIFY,
    MOUNTED_ON_FILEID,
    SUPPATTR_EXCLCREAT,
    XATTR_SUPPORT,
  ])
}

/// Whether any of the attributes in `requested` needs the filesystem's figures (a v3 FSSTAT).
pub fn needs_fs_figures(requested: &Bitmap) -> bool {
  use number::*;
  [
    FILES_AVAIL,
    FILES_FREE,
    FILES_TOTAL,
    SPACE_AVAIL,
    SPACE_FREE,
    SPACE_TOTAL,
  ]
  .iter()
  .any(|bit| requested.has(*bit))
}

/// Encodes the attributes of `requested` this server supports, for an object with v3 attributes
/// `attrs` and handle `handle`: the bitmap of those returned, then their values (`fattr4`).
pub fn encode(
  requested: &Bitmap,
  attrs: &Fattr3,
  handle: &Nfsfh3,
  fs: &FsFigures,
  writer: &mut XdrWriter,
) {
  use number::*;
  let returned = requested.intersect(&supported());
  let mut values = XdrWriter::new();
  for bit in returned.bits() {
    match bit {
      SUPPORTED_ATTRS => supported().encode(&mut values),
      TYPE => values.u32(ftype4_of(attrs.kind)),
      FH_EXPIRE_TYPE => values.u32(FH4_PERSISTENT),
      CHANGE => values.u64(change_of(attrs)),
      SIZE => values.u64(attrs.size),
      LINK_SUPPORT | SYMLINK_SUPPORT | UNIQUE_HANDLES | CANSETTIME | CASE_PRESERVING
      | CHOWN_RESTRICTED | HOMOGENEOUS | NO_TRUNC | XATTR_SUPPORT => values.bool(true),
      NAMED_ATTR => values.bool(false),
      FSID => {
        values.u64(fs.fsid.0);
        values.u64(fs.fsid.1);
      }
      LEASE_TIME => values.u32(fs.lease_seconds),
      RDATTR_ERROR => values.u32(0),
      CASE_INSENSITIVE => values.bool(fs.case_insensitive),
      FILEHANDLE => values.opaque(&handle.0),
      FILEID | MOUNTED_ON_FILEID => values.u64(attrs.fileid),
      FILES_AVAIL => values.u64(fs.files.0),
      FILES_FREE => values.u64(fs.files.1),
      FILES_TOTAL => values.u64(fs.files.2),
      MAXFILESIZE => values.u64(fs.max_file_size),
      MAXLINK => values.u32(fs.max_link),
      MAXNAME => values.u32(fs.max_name),
      MAXREAD | MAXWRITE => values.u64(fs.max_io),
      MODE => values.u32(attrs.mode),
      NUMLINKS => values.u32(attrs.nlink),
      OWNER => values.opaque(attrs.uid.to_string().as_bytes()),
      OWNER_GROUP => values.opaque(attrs.gid.to_string().as_bytes()),
      RAWDEV => {
        values.u32(attrs.rdev.specdata1);
        values.u32(attrs.rdev.specdata2);
      }
      SPACE_AVAIL => values.u64(fs.space.0),
      SPACE_FREE => values.u64(fs.space.1),
      SPACE_TOTAL => values.u64(fs.space.2),
      SPACE_USED => values.u64(attrs.used),
      TIME_ACCESS => time(&mut values, attrs.atime.seconds, attrs.atime.nseconds),
      TIME_DELTA => time(&mut values, 0, 1),
      TIME_METADATA => time(&mut values, attrs.ctime.seconds, attrs.ctime.nseconds),
      TIME_MODIFY => time(&mut values, attrs.mtime.seconds, attrs.mtime.nseconds),
      SUPPATTR_EXCLCREAT => Bitmap::default().encode(&mut values),
      _ => {}
    }
  }
  returned.encode(writer);
  writer.opaque(&values.into_bytes());
}

/// An `nfstime4`: signed seconds and nanoseconds.
fn time(writer: &mut XdrWriter, seconds: u32, nseconds: u32) {
  writer.u64(u64::from(seconds));
  writer.u32(nseconds);
}

/// The `change` attribute: the change time in nanoseconds, which moves with every change to the
/// object's data or attributes (the volume stamps `ctime` on each).
fn change_of(attrs: &Fattr3) -> u64 {
  /// Format: nanoseconds per second.
  const NS_PER_SECOND: u64 = 1_000_000_000;
  u64::from(attrs.ctime.seconds) * NS_PER_SECOND + u64::from(attrs.ctime.nseconds)
}

/// The v4 file type of a v3 one.
fn ftype4_of(kind: Ftype3) -> u32 {
  match kind {
    Ftype3::Reg => ftype4::REG,
    Ftype3::Dir => ftype4::DIR,
    Ftype3::Blk => ftype4::BLK,
    Ftype3::Chr => ftype4::CHR,
    Ftype3::Lnk => ftype4::LNK,
    Ftype3::Sock => ftype4::SOCK,
    Ftype3::Fifo => ftype4::FIFO,
  }
}
