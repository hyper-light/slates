//! The NFSv4 attributes (`fattr4`, RFC 8881 §5.8; `xattr_support`, RFC 8276 §8.1): which the server
//! supports, and the encoding of a requested set from an object's v3 attributes and the filesystem's
//! figures. The values are encoded in ascending attribute number, as the attribute list requires.
//!
//! The supported set is every attribute this server encodes plus the two write-only times SETATTR
//! and CREATE accept (`time_access_set`, `time_modify_set`): a client sets only what `supported_attrs`
//! names — the Linux client drops `utimensat`'s explicit times otherwise — and a read of a write-only
//! attribute is `NFS4ERR_INVAL` (§5.6), never a value.

use super::Nfsstat4;
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
  /// Format: `FATTR4_TIME_ACCESS_SET` (write-only).
  pub const TIME_ACCESS_SET: u32 = 48;
  /// Format: `FATTR4_TIME_DELTA`.
  pub const TIME_DELTA: u32 = 51;
  /// Format: `FATTR4_TIME_METADATA`.
  pub const TIME_METADATA: u32 = 52;
  /// Format: `FATTR4_TIME_MODIFY`.
  pub const TIME_MODIFY: u32 = 53;
  /// Format: `FATTR4_TIME_MODIFY_SET` (write-only).
  pub const TIME_MODIFY_SET: u32 = 54;
  /// Format: `FATTR4_MOUNTED_ON_FILEID`.
  pub const MOUNTED_ON_FILEID: u32 = 55;
  /// Format: `FATTR4_SUPPATTR_EXCLCREAT` (NFSv4.1, REQUIRED).
  pub const SUPPATTR_EXCLCREAT: u32 = 75;
  /// Format: `FATTR4_XATTR_SUPPORT` (RFC 8276, NFSv4.2).
  pub const XATTR_SUPPORT: u32 = 82;
  /// Format: the highest attribute number NFSv4.1 defines (`suppattr_exclcreat`, RFC 8881 §5.8); every
  /// higher one is NFSv4.2's.
  pub const LAST_OF_MINOR_ONE: u32 = SUPPATTR_EXCLCREAT;
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

impl FsFigures {
  /// The figures' wire form inside an extension's arguments (`super::listing::PageRequest`), field by field.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.u32(self.lease_seconds);
    writer.bool(self.case_insensitive);
    for figure in [self.files, self.space] {
      writer.u64(figure.0);
      writer.u64(figure.1);
      writer.u64(figure.2);
    }
    writer.u64(self.max_file_size);
    writer.u32(self.max_link);
    writer.u32(self.max_name);
    writer.u64(self.max_io);
  }

  /// The figures from their wire form.
  pub fn decode(reader: &mut crate::xdr::XdrReader<'_>) -> Result<FsFigures, crate::xdr::XdrError> {
    let lease_seconds = reader.u32()?;
    let case_insensitive = reader.bool()?;
    let files = (reader.u64()?, reader.u64()?, reader.u64()?);
    let space = (reader.u64()?, reader.u64()?, reader.u64()?);
    Ok(FsFigures {
      lease_seconds,
      case_insensitive,
      files,
      space,
      max_file_size: reader.u64()?,
      max_link: reader.u32()?,
      max_name: reader.u32()?,
      max_io: reader.u64()?,
    })
  }
}

/// The attributes this server supports in a compound of minor version `minor`: every number in
/// [`number`], less NFSv4.2's in an NFSv4.1 compound (an attribute its version does not define).
pub fn supported(minor: u32) -> &'static Bitmap {
  /// The set for minor versions through 1, and for the highest: built once per process, as every listed entry's
  /// encoding asks for it (A-90: rebuilt per call, it was 15% of a READDIR page's time).
  static THROUGH_ONE: std::sync::OnceLock<Bitmap> = std::sync::OnceLock::new();
  static HIGHEST: std::sync::OnceLock<Bitmap> = std::sync::OnceLock::new();
  if minor >= super::MINOR_HIGHEST {
    HIGHEST.get_or_init(|| supported_set(minor))
  } else {
    THROUGH_ONE.get_or_init(|| supported_set(minor))
  }
}

/// The attributes this server supports at `minor`, built (see [`supported`]).
fn supported_set(minor: u32) -> Bitmap {
  use number::*;
  let all = [
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
    TIME_ACCESS_SET,
    TIME_DELTA,
    TIME_METADATA,
    TIME_MODIFY,
    TIME_MODIFY_SET,
    MOUNTED_ON_FILEID,
    SUPPATTR_EXCLCREAT,
    XATTR_SUPPORT,
  ];
  let defined: Vec<u32> = all
    .into_iter()
    .filter(|bit| minor >= super::MINOR_HIGHEST || *bit <= LAST_OF_MINOR_ONE)
    .collect();
  Bitmap::of(&defined)
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

/// The attributes SETATTR and a create set: size, mode, owner, group and the two times.
pub fn settable() -> Bitmap {
  use number::*;
  Bitmap::of(&[
    SIZE,
    MODE,
    OWNER,
    OWNER_GROUP,
    TIME_ACCESS_SET,
    TIME_MODIFY_SET,
  ])
}

/// `suppattr_exclcreat` (§5.8.1.14): what an EXCLUSIVE4_1 create sets — every settable attribute but
/// the two times, which keep the create's verifier (§18.16.3 requires they be left out then). The Linux
/// client sends in an exclusive create only the attributes named here and sets the rest afterwards
/// (the times, but never the mode), so an empty set had left every `O_EXCL` file at the default mode.
pub fn exclusive_create_settable() -> Bitmap {
  use number::*;
  Bitmap::of(&[SIZE, MODE, OWNER, OWNER_GROUP])
}

/// Refuses a read of `requested` that names an attribute that can be set and never read:
/// `NFS4ERR_INVAL` (§5.6).
pub fn check_readable(requested: &Bitmap) -> Result<(), Nfsstat4> {
  /// Built once per process (see [`supported`]).
  static WRITE_ONLY: std::sync::OnceLock<Bitmap> = std::sync::OnceLock::new();
  let write_only =
    WRITE_ONLY.get_or_init(|| Bitmap::of(&[number::TIME_ACCESS_SET, number::TIME_MODIFY_SET]));
  if requested.intersect(write_only).is_empty() {
    Ok(())
  } else {
    Err(Nfsstat4::Inval)
  }
}

/// Encodes the attributes of `requested` this server supports, for an object with v3 attributes
/// `attrs` and handle `handle`: the bitmap of those returned, then their values (`fattr4`). A request
/// naming a write-only attribute is `NFS4ERR_INVAL` (§5.6) and writes nothing. `change` is the
/// object's change counter, which the v3 layer carries in the front end's dialect (A-38): it moves on
/// every change whatever the wall clock does, where the change time can repeat or step back; attributes
/// without it did not come from that dialect, a server fault.
pub fn encode(
  (requested, minor): (&Bitmap, u32),
  attrs: &Fattr3,
  handle: &Nfsfh3,
  fs: &FsFigures,
  writer: &mut XdrWriter,
) -> Result<(), Nfsstat4> {
  check_readable(requested)?;
  // The change counter and the full-range times ride after the `fattr3` fields in the front end's
  // dialect; attributes without them are a server fault, never a guess from the 32-bit `fattr3`.
  let v4 = attrs.v4.ok_or(Nfsstat4::Serverfault)?;
  let returned = requested.intersect(supported(minor));
  returned.encode(writer);
  writer.opaque_built(|values| encode_values(&returned, minor, (attrs, &v4), handle, fs, values));
  Ok(())
}

/// The values of the `returned` attributes, in bit order (the body of [`encode`]'s `fattr4` opaque).
fn encode_values(
  returned: &Bitmap,
  minor: u32,
  (attrs, v4): (&Fattr3, &crate::nfs::V4Attrs),
  handle: &Nfsfh3,
  fs: &FsFigures,
  values: &mut XdrWriter,
) {
  use number::*;
  for bit in returned.bits() {
    match bit {
      SUPPORTED_ATTRS => supported(minor).encode(values),
      TYPE => values.u32(ftype4_of(attrs.kind)),
      FH_EXPIRE_TYPE => values.u32(FH4_PERSISTENT),
      CHANGE => values.u64(v4.change),
      SIZE => values.u64(attrs.size),
      LINK_SUPPORT | SYMLINK_SUPPORT | UNIQUE_HANDLES | CANSETTIME | CASE_PRESERVING
      | CHOWN_RESTRICTED | HOMOGENEOUS | NO_TRUNC | XATTR_SUPPORT => values.bool(true),
      NAMED_ATTR => values.bool(false),
      FSID => {
        // The object's own filesystem: a volume root listed in the pseudo-root is its volume's.
        values.u64(attrs.fsid);
        values.u64(0);
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
      TIME_ACCESS => time(values, v4.atime_ns),
      TIME_DELTA => time(values, 1),
      TIME_METADATA => time(values, v4.ctime_ns),
      TIME_MODIFY => time(values, v4.mtime_ns),
      SUPPATTR_EXCLCREAT => exclusive_create_settable().encode(values),
      _ => {}
    }
  }
}

/// The fewest bytes the `requested` attributes encode to (the bitmap and the `fattr4` opaque) for any object at `minor`
/// (A-90): the attributes of a minimal object (an empty handle, owner and group 0 — the shortest owner strings — and
/// every other field fixed in size) encoded once. The encoder itself is the table of sizes, so the floor cannot drift
/// from what [`encode`] writes. Zero for a request [`encode`] refuses.
pub fn encoded_floor((requested, minor): (&Bitmap, u32)) -> usize {
  let minimal = Fattr3 {
    kind: Ftype3::Reg,
    mode: 0,
    nlink: 0,
    uid: 0,
    gid: 0,
    size: 0,
    used: 0,
    rdev: crate::nfs::Specdata3::default(),
    fsid: 0,
    fileid: 0,
    atime: crate::nfs::Nfstime3::default(),
    mtime: crate::nfs::Nfstime3::default(),
    ctime: crate::nfs::Nfstime3::default(),
    v4: Some(crate::nfs::V4Attrs {
      change: 0,
      atime_ns: 0,
      mtime_ns: 0,
      ctime_ns: 0,
    }),
  };
  let mut writer = XdrWriter::new();
  match encode(
    (requested, minor),
    &minimal,
    &Nfsfh3::default(),
    &FsFigures::default(),
    &mut writer,
  ) {
    Ok(()) => writer.len(),
    Err(_) => 0,
  }
}

/// An `nfstime4` (RFC 8881 §3.3.1) of `ns` nanoseconds since the epoch: signed seconds, and the
/// nanoseconds past them (never negative: a time before the epoch counts its seconds down).
fn time(writer: &mut XdrWriter, ns: i64) {
  /// Format: nanoseconds per second.
  const NS_PER_SECOND: i64 = 1_000_000_000;
  writer.i64(ns.div_euclid(NS_PER_SECOND));
  writer.u32(u32::try_from(ns.rem_euclid(NS_PER_SECOND)).unwrap_or(0));
}

/// The v3 file type a v4 one names; `None` for the v4-only named-attribute types (`NF4ATTRDIR`,
/// `NF4NAMEDATTR`) and for values outside `nfs_ftype4`.
pub(crate) fn ftype3_of(kind: u32) -> Option<Ftype3> {
  [
    Ftype3::Reg,
    Ftype3::Dir,
    Ftype3::Blk,
    Ftype3::Chr,
    Ftype3::Lnk,
    Ftype3::Sock,
    Ftype3::Fifo,
  ]
  .into_iter()
  .find(|candidate| ftype4_of(*candidate) == kind)
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
