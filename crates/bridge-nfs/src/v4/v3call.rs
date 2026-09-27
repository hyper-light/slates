//! The NFSv3 calls the v4 front end makes on the semantic layer (A-35): each procedure's arguments
//! encoded and its result decoded, exactly as RFC 1813 lays them out, so a v4 operation is served by
//! the same procedure a v3 client would reach. Every decoder refuses a malformed result rather than
//! guessing (the result comes from this server, so a malformed one is a server fault).

use crate::nfs::{Fattr3, Nfsfh3, Nfsstat3, Nfstime3, PostOpAttr};
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

/// The optional fields of a `sattr3` (RFC 1813 §2.5.3); `None` leaves a field as it is.
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
  pub atime: Option<Option<Nfstime3>>,
  /// A new modification time, as `atime`.
  pub mtime: Option<Option<Nfstime3>>,
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

/// Skips a `wcc_data` (a `pre_op_attr`, then a `post_op_attr`).
fn skip_wcc(reader: &mut XdrReader<'_>) -> Result<(), Malformed> {
  if reader.bool().map_err(|_| Malformed)? {
    reader.u64().map_err(|_| Malformed)?;
    Nfstime3::decode(reader).map_err(|_| Malformed)?;
    Nfstime3::decode(reader).map_err(|_| Malformed)?;
  }
  PostOpAttr::decode(reader).map_err(|_| Malformed)?;
  Ok(())
}

/// A GETATTR result: the attributes, or the status.
pub fn getattr(result: &[u8]) -> Result<Result<Fattr3, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  match status(&mut reader)? {
    Nfsstat3::Ok => Ok(Ok(Fattr3::decode(&mut reader).map_err(|_| Malformed)?)),
    other => Ok(Err(other)),
  }
}

/// A LOOKUP result: the object's handle and attributes (when the server returned them), or the status.
pub fn lookup(result: &[u8]) -> Result<Result<(Nfsfh3, Option<Fattr3>), Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  match status(&mut reader)? {
    Nfsstat3::Ok => {
      let fh = Nfsfh3::decode(&mut reader).map_err(|_| Malformed)?;
      let attrs = PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?.0;
      Ok(Ok((fh, attrs)))
    }
    other => Ok(Err(other)),
  }
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
  PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?;
  match status {
    Nfsstat3::Ok => Ok(Ok(reader.u32().map_err(|_| Malformed)?)),
    other => Ok(Err(other)),
  }
}

/// A READLINK result: the target, or the status.
pub fn readlink(result: &[u8]) -> Result<Result<String, Nfsstat3>, Malformed> {
  let mut reader = XdrReader::new(result);
  let status = status(&mut reader)?;
  PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?;
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
  PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?;
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
  skip_wcc(&mut reader)?;
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
  skip_wcc(&mut reader)?;
  match status {
    Nfsstat3::Ok => {
      let mut verf = [0u8; VERF_SIZE];
      verf.copy_from_slice(reader.fixed(VERF_SIZE).map_err(|_| Malformed)?);
      Ok(Ok(verf))
    }
    other => Ok(Err(other)),
  }
}

/// Format: CREATE's `createmode3` values (RFC 1813 §3.3.8).
pub mod createmode {
  /// Format: `UNCHECKED`.
  pub const UNCHECKED: u32 = 0;
  /// Format: `GUARDED`.
  pub const GUARDED: u32 = 1;
}

/// CREATE arguments: a regular file `name` in `dir`, `mode` one of [`createmode`].
pub fn create_args(dir: &Nfsfh3, name: &str, mode: u32, attrs: &Sattr3) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  dir.encode(&mut writer);
  writer.opaque(name.as_bytes());
  writer.u32(mode);
  attrs.encode(&mut writer);
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

/// A CREATE, MKDIR or SYMLINK result: the new object's handle and attributes (when returned), or the
/// status.
pub fn created(result: &[u8]) -> Reply<(Option<Nfsfh3>, Option<Fattr3>)> {
  let mut reader = XdrReader::new(result);
  match status(&mut reader)? {
    Nfsstat3::Ok => {
      let fh = if reader.bool().map_err(|_| Malformed)? {
        let bytes = reader.opaque(FHSIZE3).map_err(|_| Malformed)?;
        Some(Nfsfh3(bytes.to_vec()))
      } else {
        None
      };
      let attrs = PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?.0;
      Ok(Ok((fh, attrs)))
    }
    other => Ok(Err(other)),
  }
}

/// A result that is a status then `wcc_data` (REMOVE, RMDIR, SETATTR), reduced to its status.
pub fn wcc_status(result: &[u8]) -> Result<Nfsstat3, Malformed> {
  let mut reader = XdrReader::new(result);
  status(&mut reader)
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
  PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?;
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
    let attrs = PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?.0;
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
  PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?;
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
  PostOpAttr::decode(&mut reader).map_err(|_| Malformed)?;
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
