//! The NFSv3 calls the v4 front end makes on the semantic layer (A-35): each procedure's arguments
//! encoded and its result decoded, exactly as RFC 1813 lays them out, so a v4 operation is served by
//! the same procedure a v3 client would reach. Every decoder refuses a malformed result rather than
//! guessing (the result comes from this server, so a malformed one is a server fault).

use crate::nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, Nfstime4, PostOpAttr, Specdata3, Wcc};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: `NFS3_FHSIZE`, the largest v3 handle a result may carry.
const FHSIZE3: usize = 64;
/// Format: the longest name or symlink target a v3 result may carry (`NFS3_MAXPATHLEN` class).
const MAXPATH: usize = 4096;
/// Format: the largest READ a v3 result may carry (the server's own transfer ceiling, which is far
/// below this bound).
const MAX_DATA: usize = 1 << 24;
/// Format: the size of a write or cookie verifier (`NFS3_WRITEVERFSIZE`, `NFS3_COOKIEVERFSIZE`).
pub const VERF_SIZE: usize = 8;

/// A v3 result that did not decode: a server fault, never guessed around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

/// The optional fields of a `sattr3` (RFC 1813 §2.5.3); `None` leaves a field as it is. A client time
/// is an `nfstime4`, as the front end's dialect carries it (A-38).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sattr3 {
  /// A new mode.
  pub mode: Option<u32>,
  /// A new owner uid.
  pub uid: Option<u32>,
  /// A new group gid.
  pub gid: Option<u32>,
  /// A new size.
  pub size: Option<u64>,
  /// A new access time: `Some(None)` is "the server's time", `Some(Some(t))` a client time.
  pub atime: Option<Option<Nfstime4>>,
  /// A new modification time, as `atime`.
  pub mtime: Option<Option<Nfstime4>>,
}

/// Format: `time_how` values of a `sattr3` (RFC 1813 §2.5.3).
mod time_how {
  /// Format: `DONT_CHANGE`.
  pub(super) const DONT_CHANGE: u32 = 0;
  /// Format: `SET_TO_SERVER_TIME`.
  pub(super) const SERVER: u32 = 1;
  /// Format: `SET_TO_CLIENT_TIME`.
  pub(super) const CLIENT: u32 = 2;
}

impl Sattr3 {
  /// Writes the `sattr3`.
  pub fn encode(&self, writer: &mut XdrWriter) {
    optional_u32(writer, self.mode);
    optional_u32(writer, self.uid);
    optional_u32(writer, self.gid);
    match self.size {
      Some(size) => {
        writer.bool(true);
        writer.u64(size);
      }
      None => writer.bool(false),
    }
    for time in [self.atime, self.mtime] {
      match time {
        None => writer.u32(time_how::DONT_CHANGE),
        Some(None) => writer.u32(time_how::SERVER),
        Some(Some(at)) => {
          writer.u32(time_how::CLIENT);
          at.encode(writer);
        }
      }
    }
  }
}

fn optional_u32(writer: &mut XdrWriter, value: Option<u32>) {
  match value {
    Some(value) => {
      writer.bool(true);
      writer.u32(value);
    }
    None => writer.bool(false),
  }
}

/// Arguments that are a handle and nothing else (GETATTR, READLINK, FSSTAT, PATHCONF).
pub fn handle_only(fh: &Nfsfh3) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  fh.encode(&mut writer);
  writer.into_bytes()
}

/// Arguments that are a directory handle and a name (LOOKUP, REMOVE, RMDIR).
pub fn dir_name(dir: &Nfsfh3, name: &str) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  dir.encode(&mut writer);
  writer.opaque(name.as_bytes());
  writer.into_bytes()
}

/// The result status, then the rest of the reader.
fn status(reader: &mut XdrReader<'_>) -> Result<Nfsstat3, Malformed> {
  Nfsstat3::decode(reader).map_err(|_| Malformed)
}

/// Reads a `wcc_data` of the front end's dialect, change counters included (A-38).
fn wcc(reader: &mut XdrReader<'_>) -> Result<Wcc, Malformed> {
  Wcc::decode_with_change(reader).map_err(|_| Malformed)
}

/// Reads a `post_op_attr` of the front end's dialect, change counter included.
fn post_op(reader: &mut XdrReader<'_>) -> Result<Option<Fattr3>, Malformed> {
  Ok(
    PostOpAttr::decode_with_change(reader)
      .map_err(|_| Malformed)?
      .0,
  )
}

/// A GETATTR result: the attributes, or the status.
pub fn getattr(result: &[u8]) -> Result<Result<Fattr3, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  match status(&mut reader)? {
    Nfsstat3::Ok => Ok(Ok(
      Fattr3::decode_with_change(&mut reader).map_err(|_| Malformed)?,
    )),
    other => Ok(Err(other)),
  }
}

/// A LOOKUP result: the object's handle and attributes and the directory's (each when the server
/// returned them), or the status.
pub fn lookup(result: &[u8]) -> Reply<Looked> {
  let mut reader = XdrReader::new(result);
  match status(&mut reader)? {
    Nfsstat3::Ok => {
      let fh = Nfsfh3::decode(&mut reader).map_err(|_| Malformed)?;
      let attrs = post_op(&mut reader)?;
      let dir = post_op(&mut reader)?;
      Ok(Ok(Looked { fh, attrs, dir }))
    }
    other => Ok(Err(other)),
  }
}

/// What a LOOKUP found.
pub struct Looked {
  /// The object's handle.
  pub fh: Nfsfh3,
  /// The object's attributes, when returned.
  pub attrs: Option<Fattr3>,
  /// The directory's attributes, when returned.
  pub dir: Option<Fattr3>,
}

/// ACCESS arguments.
pub fn access_args(fh: &Nfsfh3, access: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  fh.encode(&mut writer);
  writer.u32(access);
  writer.into_bytes()
}

/// An ACCESS result: the granted bits, or the status.
pub fn access(result: &[u8]) -> Result<Result<u32, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  post_op(&mut reader)?;
  match status {
    Nfsstat3::Ok => Ok(Ok(reader.u32().map_err(|_| Malformed)?)),
    other => Ok(Err(other)),
  }
}

/// A READLINK result: the target, or the status.
pub fn readlink(result: &[u8]) -> Result<Result<String, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  post_op(&mut reader)?;
  match status {
    Nfsstat3::Ok => Ok(Ok(
      reader.string(MAXPATH).map_err(|_| Malformed)?.to_owned(),
    )),
    other => Ok(Err(other)),
  }
}

/// READ arguments.
pub fn read_args(fh: &Nfsfh3, offset: u64, count: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  fh.encode(&mut writer);
  writer.u64(offset);
  writer.u32(count);
  writer.into_bytes()
}

/// A READ result: whether the end was reached and the bytes, or the status.
pub fn read(result: &[u8]) -> Result<Result<(bool, Vec<u8>), Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  post_op(&mut reader)?;
  match status {
    Nfsstat3::Ok => {
      let _count = reader.u32().map_err(|_| Malformed)?;
      let eof = reader.bool().map_err(|_| Malformed)?;
      let data = reader.opaque(MAX_DATA).map_err(|_| Malformed)?.to_vec();
      Ok(Ok((eof, data)))
    }
    other => Ok(Err(other)),
  }
}

/// SEEK extension arguments (A-35, `crate::procedures::extension::SEEK`).
pub fn seek_args(fh: &Nfsfh3, offset: u64, what: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  fh.encode(&mut writer);
  writer.u64(offset);
  writer.u32(what);
  writer.into_bytes()
}

/// A SEEK extension result: whether a match was found, its offset, and the file's size.
pub fn seek(result: &[u8]) -> Result<Result<(bool, u64, u64), Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  match status(&mut reader)? {
    Nfsstat3::Ok => {
      let found = reader.bool().map_err(|_| Malformed)?;
      let offset = reader.u64().map_err(|_| Malformed)?;
      let size = reader.u64().map_err(|_| Malformed)?;
      Ok(Ok((found, offset, size)))
    }
    other => Ok(Err(other)),
  }
}

/// A v3 result: the procedure's value or its status, or `Malformed` if the reply did not decode.
pub type Reply<T> = Result<Result<T, Nfsstat3>, Malformed>;

/// WRITE arguments.
pub fn write_args(fh: &Nfsfh3, offset: u64, stable: u32, data: &[u8]) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  fh.encode(&mut writer);
  writer.u64(offset);
  writer.u32(u32::try_from(data.len()).unwrap_or(u32::MAX));
  writer.u32(stable);
  writer.opaque(data);
  writer.into_bytes()
}

/// A WRITE result: the bytes written, the stability committed and the write verifier, or the status.
pub fn write(result: &[u8]) -> Reply<(u32, u32, [u8; VERF_SIZE])> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  wcc(&mut reader)?;
  match status {
    Nfsstat3::Ok => {
      let count = reader.u32().map_err(|_| Malformed)?;
      let committed = reader.u32().map_err(|_| Malformed)?;
      let mut verf = [0u8; VERF_SIZE];
      verf.copy_from_slice(reader.fixed(VERF_SIZE).map_err(|_| Malformed)?);
      Ok(Ok((count, committed, verf)))
    }
    other => Ok(Err(other)),
  }
}

/// COMMIT arguments.
pub fn commit_args(fh: &Nfsfh3, offset: u64, count: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  fh.encode(&mut writer);
  writer.u64(offset);
  writer.u32(count);
  writer.into_bytes()
}

/// A COMMIT result: the write verifier, or the status.
pub fn commit(result: &[u8]) -> Result<Result<[u8; VERF_SIZE], Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  wcc(&mut reader)?;
  match status {
    Nfsstat3::Ok => {
      let mut verf = [0u8; VERF_SIZE];
      verf.copy_from_slice(reader.fixed(VERF_SIZE).map_err(|_| Malformed)?);
      Ok(Ok(verf))
    }
    other => Ok(Err(other)),
  }
}

/// How a CREATE makes its file (`createhow3`, RFC 1813 §3.3.8).
pub enum CreateHow {
  /// Create, or open an existing file (`UNCHECKED`).
  Unchecked(Sattr3),
  /// Create; an existing name is `NFS3ERR_EXIST` (`GUARDED`).
  Guarded(Sattr3),
  /// Create keyed by a verifier (`EXCLUSIVE`).
  Exclusive([u8; crate::procedures::CREATEVERF_SIZE]),
}

/// CREATE arguments: a regular file `name` in `dir`, made as `how` says.
pub fn create_args(dir: &Nfsfh3, name: &str, how: &CreateHow) -> Vec<u8> {
  /// Format: the `createmode3` values (RFC 1813 §3.3.8).
  const UNCHECKED: u32 = 0;
  const GUARDED: u32 = 1;
  const EXCLUSIVE: u32 = 2;
  let mut writer = XdrWriter::new();
  dir.encode(&mut writer);
  writer.opaque(name.as_bytes());
  match how {
    CreateHow::Unchecked(attrs) => {
      writer.u32(UNCHECKED);
      attrs.encode(&mut writer);
    }
    CreateHow::Guarded(attrs) => {
      writer.u32(GUARDED);
      attrs.encode(&mut writer);
    }
    CreateHow::Exclusive(verifier) => {
      writer.u32(EXCLUSIVE);
      writer.fixed(verifier);
    }
  }
  writer.into_bytes()
}

/// MKDIR arguments.
pub fn mkdir_args(dir: &Nfsfh3, name: &str, attrs: &Sattr3) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  dir.encode(&mut writer);
  writer.opaque(name.as_bytes());
  attrs.encode(&mut writer);
  writer.into_bytes()
}

/// SYMLINK arguments.
pub fn symlink_args(dir: &Nfsfh3, name: &str, attrs: &Sattr3, target: &str) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  dir.encode(&mut writer);
  writer.opaque(name.as_bytes());
  attrs.encode(&mut writer);
  writer.opaque(target.as_bytes());
  writer.into_bytes()
}

/// MKNOD arguments (`MKNOD3args`, RFC 1813 §3.3.11): a device carries its attributes and its
/// `specdata3`, a FIFO or socket its attributes only.
pub fn mknod_args(
  dir: &Nfsfh3,
  name: &str,
  kind: Ftype3,
  attrs: &Sattr3,
  device: Specdata3,
) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  dir.encode(&mut writer);
  writer.opaque(name.as_bytes());
  writer.u32(kind as u32);
  match kind {
    Ftype3::Blk | Ftype3::Chr => {
      attrs.encode(&mut writer);
      writer.u32(device.specdata1);
      writer.u32(device.specdata2);
    }
    Ftype3::Sock | Ftype3::Fifo => attrs.encode(&mut writer),
    Ftype3::Reg | Ftype3::Dir | Ftype3::Lnk => {}
  }
  writer.into_bytes()
}

/// A CREATE, MKDIR, SYMLINK or MKNOD result: the new object's handle (when returned) and the
/// directory's `wcc_data`, or the status.
pub fn created(result: &[u8]) -> Reply<(Option<Nfsfh3>, Wcc)> {
  let mut reader = XdrReader::new(result);
  match status(&mut reader)? {
    Nfsstat3::Ok => {
      let fh = if reader.bool().map_err(|_| Malformed)? {
        let bytes = reader.opaque(FHSIZE3).map_err(|_| Malformed)?;
        Some(Nfsfh3(bytes.to_vec()))
      } else {
        None
      };
      let _attrs = post_op(&mut reader)?;
      Ok(Ok((fh, wcc(&mut reader)?)))
    }
    other => Ok(Err(other)),
  }
}

/// A result that is a status then one `wcc_data` (REMOVE, RMDIR, SETATTR): the status and the wcc.
pub fn wcc_status(result: &[u8]) -> Result<(Nfsstat3, Wcc), Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  Ok((status, wcc(&mut reader)?))
}

/// A RENAME result: the status and the source and destination directories' `wcc_data`.
pub fn renamed(result: &[u8]) -> Result<(Nfsstat3, Wcc, Wcc), Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  let from = wcc(&mut reader)?;
  Ok((status, from, wcc(&mut reader)?))
}

/// A LINK result: the status and the directory's `wcc_data`.
pub fn linked(result: &[u8]) -> Result<(Nfsstat3, Wcc), Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  let _file = post_op(&mut reader)?;
  Ok((status, wcc(&mut reader)?))
}

/// RENAME arguments.
pub fn rename_args(from_dir: &Nfsfh3, from: &str, to_dir: &Nfsfh3, to: &str) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  from_dir.encode(&mut writer);
  writer.opaque(from.as_bytes());
  to_dir.encode(&mut writer);
  writer.opaque(to.as_bytes());
  writer.into_bytes()
}

/// LINK arguments.
pub fn link_args(file: &Nfsfh3, dir: &Nfsfh3, name: &str) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  file.encode(&mut writer);
  dir.encode(&mut writer);
  writer.opaque(name.as_bytes());
  writer.into_bytes()
}

/// SETATTR arguments, unguarded.
pub fn setattr_args(fh: &Nfsfh3, attrs: &Sattr3) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  fh.encode(&mut writer);
  attrs.encode(&mut writer);
  writer.bool(false); // no ctime guard
  writer.into_bytes()
}

/// READDIRPLUS arguments.
pub fn readdirplus_args(
  dir: &Nfsfh3,
  cookie: u64,
  verf: [u8; VERF_SIZE],
  dircount: u32,
  maxcount: u32,
) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  dir.encode(&mut writer);
  writer.u64(cookie);
  writer.fixed(&verf);
  writer.u32(dircount);
  writer.u32(maxcount);
  writer.into_bytes()
}

/// One READDIRPLUS entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  /// The entry's name.
  pub name: String,
  /// The v3 cookie that resumes after it.
  pub cookie: u64,
  /// Its attributes, when returned.
  pub attrs: Option<Fattr3>,
  /// Its handle, when returned.
  pub fh: Option<Nfsfh3>,
}

/// A READDIRPLUS result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
  /// The cookie verifier.
  pub verf: [u8; VERF_SIZE],
  /// The entries, in order.
  pub entries: Vec<Entry>,
  /// Whether the listing reached the end.
  pub eof: bool,
}

/// A READDIRPLUS result, or the status. `max_entries` bounds how many entries are read (the result is
/// this server's own, sized by the `maxcount` asked).
pub fn readdirplus(
  result: &[u8],
  max_entries: usize,
) -> Result<Result<Listing, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  post_op(&mut reader)?;
  if status != Nfsstat3::Ok {
    return Ok(Err(status));
  }
  let mut verf = [0u8; VERF_SIZE];
  verf.copy_from_slice(reader.fixed(VERF_SIZE).map_err(|_| Malformed)?);
  let mut entries = Vec::new();
  while reader.bool().map_err(|_| Malformed)? {
    if entries.len() >= max_entries {
      return Err(Malformed);
    }
    let _fileid = reader.u64().map_err(|_| Malformed)?;
    let name = reader.string(MAXPATH).map_err(|_| Malformed)?.to_owned();
    let cookie = reader.u64().map_err(|_| Malformed)?;
    let attrs = post_op(&mut reader)?;
    let fh = if reader.bool().map_err(|_| Malformed)? {
      Some(Nfsfh3(
        reader.opaque(FHSIZE3).map_err(|_| Malformed)?.to_vec(),
      ))
    } else {
      None
    };
    entries.push(Entry {
      name,
      cookie,
      attrs,
      fh,
    });
  }
  let eof = reader.bool().map_err(|_| Malformed)?;
  Ok(Ok(Listing { verf, entries, eof }))
}

/// The filesystem figures an FSSTAT result carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsStat {
  /// Total, free and available bytes.
  pub bytes: (u64, u64, u64),
  /// Total, free and available files.
  pub files: (u64, u64, u64),
}

/// An FSSTAT result, or the status.
pub fn fsstat(result: &[u8]) -> Result<Result<FsStat, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  post_op(&mut reader)?;
  if status != Nfsstat3::Ok {
    return Ok(Err(status));
  }
  let mut word = || reader.u64().map_err(|_| Malformed);
  Ok(Ok(FsStat {
    bytes: (word()?, word()?, word()?),
    files: (word()?, word()?, word()?),
  }))
}

/// The figures a PATHCONF result carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathConf {
  /// The most hard links.
  pub link_max: u32,
  /// The longest name.
  pub name_max: u32,
  /// Whether names fold case.
  pub case_insensitive: bool,
}

/// A PATHCONF result, or the status.
pub fn pathconf(result: &[u8]) -> Result<Result<PathConf, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  post_op(&mut reader)?;
  if status != Nfsstat3::Ok {
    return Ok(Err(status));
  }
  let link_max = reader.u32().map_err(|_| Malformed)?;
  let name_max = reader.u32().map_err(|_| Malformed)?;
  let _no_trunc = reader.bool().map_err(|_| Malformed)?;
  let _chown_restricted = reader.bool().map_err(|_| Malformed)?;
  let case_insensitive = reader.bool().map_err(|_| Malformed)?;
  Ok(Ok(PathConf {
    link_max,
    name_max,
    case_insensitive,
  }))
}
