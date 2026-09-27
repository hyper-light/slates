//! NFSv3 core data types (RFC 1813): the status codes, file types, times, attributes and file
//! handles the procedures exchange. These are the message layer atop the XDR and RPC codec — pure
//! encode and decode over [`crate::xdr`], round-trip and golden-vector tested with no socket. The
//! procedures themselves (LOOKUP, GETATTR, READ, WRITE, READDIR, …) over the volume core, and the
//! file-handle-to-inode map, are the next layer (owed).

use crate::xdr::{XdrError, XdrReader, XdrWriter};

/// Format: the maximum NFSv3 file-handle length (`NFS3_FHSIZE`, RFC 1813).
pub const MAX_FH: usize = 64;

/// The NFSv3 status of a reply (RFC 1813 §2.6). Only the values a userspace loopback server returns
/// are modeled; the wire value is the enum discriminant.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nfsstat3 {
  /// Format: NFS3_OK — the call succeeded.
  Ok = 0,
  /// Format: NFS3ERR_PERM — not owner.
  Perm = 1,
  /// Format: NFS3ERR_NOENT — no such file or directory.
  Noent = 2,
  /// Format: NFS3ERR_IO — an I/O error.
  Io = 5,
  /// Format: NFS3ERR_ACCES — permission denied.
  Acces = 13,
  /// Format: NFS3ERR_EXIST — the file exists.
  Exist = 17,
  /// Format: NFS3ERR_NOTDIR — not a directory.
  Notdir = 20,
  /// Format: NFS3ERR_ISDIR — is a directory.
  Isdir = 21,
  /// Format: NFS3ERR_INVAL — an invalid argument.
  Inval = 22,
  /// Format: NFS3ERR_NOSPC — no space left on the device.
  Nospc = 28,
  /// Format: NFS3ERR_ROFS — a read-only filesystem.
  Rofs = 30,
  /// Format: NFS3ERR_NAMETOOLONG — the name is too long.
  Nametoolong = 63,
  /// Format: NFS3ERR_NOTEMPTY — the directory is not empty.
  Notempty = 66,
  /// Format: NFS3ERR_STALE — a stale file handle.
  Stale = 70,
  /// Format: NFS3ERR_BADHANDLE — an illegal file handle.
  Badhandle = 10001,
  /// Format: NFS3ERR_NOT_SYNC — a SETATTR guard's ctime did not match the object's, so the
  /// caller's cached state is out of date and the update is refused (RFC 1813 §3.3.2).
  NotSync = 10002,
  /// Format: NFS3ERR_BAD_COOKIE — a READDIR/READDIRPLUS continuation whose cookieverf no longer
  /// matches the directory's, so the directory changed since the listing began (RFC 1813 §3.3.16).
  BadCookie = 10003,
  /// Format: NFS3ERR_NOTSUPP — the operation is not supported.
  Notsupp = 10004,
  /// Format: NFS3ERR_TOOSMALL — a READDIR/READDIRPLUS `count` too small to hold even one entry.
  Toosmall = 10005,
  /// Format: NFS3ERR_SERVERFAULT — an error not covered by the protocol.
  ServerFault = 10006,
  /// Format: NFS3ERR_BADTYPE — an unsupported or illegal MKNOD type (RFC 1813 §3.3.11).
  Badtype = 10007,
  /// Format: NFS3ERR_JUKEBOX — the server cannot complete the request now; the client should retry
  /// later (RFC 1813 §2.6). Answered while the volume's owner lease is unconfirmed (§4.8 "Leases and
  /// reads"): the state may be served again within the bound, or its owner may have moved.
  Jukebox = 10008,
}

impl Nfsstat3 {
  /// The wire value.
  pub fn wire(self) -> u32 {
    self as u32
  }

  /// Writes the status.
  pub fn encode(self, writer: &mut XdrWriter) {
    writer.u32(self.wire());
  }

  /// The status a wire value names; `None` for a value no NFSv3 status has.
  pub fn from_wire(value: u32) -> Option<Nfsstat3> {
    use Nfsstat3::*;
    [
      Ok,
      Perm,
      Noent,
      Io,
      Acces,
      Exist,
      Notdir,
      Isdir,
      Inval,
      Nospc,
      Rofs,
      Nametoolong,
      Notempty,
      Stale,
      Badhandle,
      NotSync,
      BadCookie,
      Notsupp,
      Toosmall,
      ServerFault,
      Badtype,
      Jukebox,
    ]
    .into_iter()
    .find(|status| status.wire() == value)
  }

  /// Reads a status, refusing a value no NFSv3 status has.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Nfsstat3, XdrError> {
    Nfsstat3::from_wire(reader.u32()?).ok_or(XdrError::BadLength)
  }
}

/// The type of a filesystem object (RFC 1813 `ftype3`); the wire value is the discriminant.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ftype3 {
  /// Format: NF3REG — a regular file.
  Reg = 1,
  /// Format: NF3DIR — a directory.
  Dir = 2,
  /// Format: NF3BLK — a block-special device.
  Blk = 3,
  /// Format: NF3CHR — a character-special device.
  Chr = 4,
  /// Format: NF3LNK — a symbolic link.
  Lnk = 5,
  /// Format: NF3SOCK — a socket.
  Sock = 6,
  /// Format: NF3FIFO — a named pipe.
  Fifo = 7,
}

impl Ftype3 {
  /// The type for a wire value, or `None` for an unknown one.
  pub fn from_wire(value: u32) -> Option<Ftype3> {
    ALL_FTYPES.iter().copied().find(|t| *t as u32 == value)
  }
}

/// Every file type, so a wire lookup needs no number of its own.
const ALL_FTYPES: &[Ftype3] = &[
  Ftype3::Reg,
  Ftype3::Dir,
  Ftype3::Blk,
  Ftype3::Chr,
  Ftype3::Lnk,
  Ftype3::Sock,
  Ftype3::Fifo,
];

/// An NFSv3 timestamp (`nfstime3`): whole seconds and nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nfstime3 {
  /// Whole seconds.
  pub seconds: u32,
  /// Nanoseconds.
  pub nseconds: u32,
}

impl Nfstime3 {
  /// Writes the timestamp.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.u32(self.seconds);
    writer.u32(self.nseconds);
  }

  /// Reads a timestamp.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Nfstime3, XdrError> {
    Ok(Nfstime3 {
      seconds: reader.u32()?,
      nseconds: reader.u32()?,
    })
  }
}

/// An NFSv4 timestamp (`nfstime4`, RFC 8881 §3.3.1): signed seconds since the Unix epoch and the
/// nanoseconds past them. The front end's dialect carries client times this way inside a `sattr3`
/// (A-38), so NFSv3's unsigned 32-bit seconds never limit an NFSv4 client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nfstime4 {
  /// Signed seconds.
  pub seconds: i64,
  /// Nanoseconds, below one second.
  pub nseconds: u32,
}

impl Nfstime4 {
  /// Writes the timestamp.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.i64(self.seconds);
    writer.u32(self.nseconds);
  }

  /// Reads a timestamp.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Nfstime4, XdrError> {
    Ok(Nfstime4 {
      seconds: reader.i64()?,
      nseconds: reader.u32()?,
    })
  }
}

/// A device's major/minor pair (`specdata3`); zero for a non-device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Specdata3 {
  /// The first component (major).
  pub specdata1: u32,
  /// The second component (minor).
  pub specdata2: u32,
}

/// The attributes of a filesystem object (`fattr3`, RFC 1813 §2.3.6): a fixed structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fattr3 {
  /// The object type.
  pub kind: Ftype3,
  /// The mode bits.
  pub mode: u32,
  /// The hard-link count.
  pub nlink: u32,
  /// The owner's user id.
  pub uid: u32,
  /// The owner's group id.
  pub gid: u32,
  /// The size in bytes.
  pub size: u64,
  /// The bytes actually used.
  pub used: u64,
  /// The device numbers (zero for a non-device).
  pub rdev: Specdata3,
  /// The filesystem id.
  pub fsid: u64,
  /// The object's id within the filesystem (the inode number).
  pub fileid: u64,
  /// The last access time.
  pub atime: Nfstime3,
  /// The last modification time.
  pub mtime: Nfstime3,
  /// The last change time.
  pub ctime: Nfstime3,
  /// What a reply to the NFSv4 front end carries after the `fattr3` fields (the
  /// [`crate::procedures::Dialect::Nfs4`] dialect); `None` in an NFSv3 reply, which carries exactly RFC
  /// 1813's structure (A-38).
  pub v4: Option<V4Attrs>,
}

/// The attributes NFSv4 has and `fattr3` cannot hold, carried to the front end after the `fattr3`
/// fields (A-38): the change counter, and the three times at the volume's full range and precision —
/// nanoseconds in an `i64`, before 1970 and past 2106 included, where `nfstime3`'s seconds are an
/// unsigned 32-bit count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V4Attrs {
  /// The change counter.
  pub change: u64,
  /// The access time, nanoseconds since the Unix epoch.
  pub atime_ns: i64,
  /// The modification time.
  pub mtime_ns: i64,
  /// The change time.
  pub ctime_ns: i64,
}

impl Fattr3 {
  /// Writes the attributes in `fattr3` field order, then the change counter when it is carried.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.u32(self.kind as u32);
    writer.u32(self.mode);
    writer.u32(self.nlink);
    writer.u32(self.uid);
    writer.u32(self.gid);
    writer.u64(self.size);
    writer.u64(self.used);
    writer.u32(self.rdev.specdata1);
    writer.u32(self.rdev.specdata2);
    writer.u64(self.fsid);
    writer.u64(self.fileid);
    self.atime.encode(writer);
    self.mtime.encode(writer);
    self.ctime.encode(writer);
    if let Some(v4) = self.v4 {
      writer.u64(v4.change);
      writer.i64(v4.atime_ns);
      writer.i64(v4.mtime_ns);
      writer.i64(v4.ctime_ns);
    }
  }

  /// Reads the attributes and what follows them in a reply to the NFSv4 front end
  /// ([`crate::procedures::Dialect::Nfs4`], [`V4Attrs`]).
  pub fn decode_with_change(reader: &mut XdrReader<'_>) -> Result<Fattr3, XdrError> {
    let mut attrs = Fattr3::decode(reader)?;
    attrs.v4 = Some(V4Attrs {
      change: reader.u64()?,
      atime_ns: reader.i64()?,
      mtime_ns: reader.i64()?,
      ctime_ns: reader.i64()?,
    });
    Ok(attrs)
  }

  /// Reads the attributes, refusing an unknown file type.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Fattr3, XdrError> {
    let kind = Ftype3::from_wire(reader.u32()?).ok_or(XdrError::BadLength)?;
    let mode = reader.u32()?;
    let nlink = reader.u32()?;
    let uid = reader.u32()?;
    let gid = reader.u32()?;
    let size = reader.u64()?;
    let used = reader.u64()?;
    let rdev = Specdata3 {
      specdata1: reader.u32()?,
      specdata2: reader.u32()?,
    };
    let fsid = reader.u64()?;
    let fileid = reader.u64()?;
    let atime = Nfstime3::decode(reader)?;
    let mtime = Nfstime3::decode(reader)?;
    let ctime = Nfstime3::decode(reader)?;
    Ok(Fattr3 {
      kind,
      mode,
      nlink,
      uid,
      gid,
      size,
      used,
      rdev,
      fsid,
      fileid,
      atime,
      mtime,
      ctime,
      v4: None,
    })
  }
}

/// The attributes a `wcc_data` carries from before an operation (`wcc_attr`, RFC 1813 §2.6): the
/// size and the two times a client compares with its cache, then — in a reply to the NFSv4 front end
/// — the change counter, from which it builds an atomic `change_info4` (A-38).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WccAttr {
  /// The size in bytes.
  pub size: u64,
  /// The last modification time.
  pub mtime: Nfstime3,
  /// The last change time.
  pub ctime: Nfstime3,
  /// The change counter, carried only in the NFSv4 dialect (as [`Fattr3::change`]).
  pub change: Option<u64>,
}

/// A `wcc_data` (RFC 1813 §2.6): the object's attributes immediately before and after the
/// operation. slates takes both inside the one call on the owner shard, so nothing else changed the
/// object between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wcc {
  /// The attributes before (`pre_op_attr`), `None` when the operation refused before its effect.
  pub pre: Option<WccAttr>,
  /// The attributes after (`post_op_attr`).
  pub post: Option<Fattr3>,
}

impl Wcc {
  /// The `wcc_data` of a call that refused: the object's attributes as they are, and no pre-operation
  /// half (the refusal changed nothing).
  pub fn refused(post: Option<Fattr3>) -> Wcc {
    Wcc { pre: None, post }
  }

  /// Writes the `wcc_data`.
  pub fn encode(&self, writer: &mut XdrWriter) {
    match &self.pre {
      Some(pre) => {
        writer.bool(true);
        writer.u64(pre.size);
        pre.mtime.encode(writer);
        pre.ctime.encode(writer);
        if let Some(change) = pre.change {
          writer.u64(change);
        }
      }
      None => writer.bool(false),
    }
    PostOpAttr(self.post).encode(writer);
  }

  /// Reads a `wcc_data` of a reply to the NFSv4 front end, change counters included.
  pub fn decode_with_change(reader: &mut XdrReader<'_>) -> Result<Wcc, XdrError> {
    let pre = if reader.bool()? {
      let size = reader.u64()?;
      let mtime = Nfstime3::decode(reader)?;
      let ctime = Nfstime3::decode(reader)?;
      Some(WccAttr {
        size,
        mtime,
        ctime,
        change: Some(reader.u64()?),
      })
    } else {
      None
    };
    let post = PostOpAttr::decode_with_change(reader)?.0;
    Ok(Wcc { pre, post })
  }
}

/// A `post_op_attr`: attributes optionally returned after an operation. Encoded as a boolean
/// present-flag then, if present, the attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PostOpAttr(pub Option<Fattr3>);

impl PostOpAttr {
  /// Writes the optional attributes.
  pub fn encode(&self, writer: &mut XdrWriter) {
    match &self.0 {
      Some(attr) => {
        writer.bool(true);
        attr.encode(writer);
      }
      None => writer.bool(false),
    }
  }

  /// Reads the optional attributes.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<PostOpAttr, XdrError> {
    if reader.bool()? {
      Ok(PostOpAttr(Some(Fattr3::decode(reader)?)))
    } else {
      Ok(PostOpAttr(None))
    }
  }

  /// Reads the optional attributes of a reply to the NFSv4 front end, change counter included.
  pub fn decode_with_change(reader: &mut XdrReader<'_>) -> Result<PostOpAttr, XdrError> {
    if reader.bool()? {
      Ok(PostOpAttr(Some(Fattr3::decode_with_change(reader)?)))
    } else {
      Ok(PostOpAttr(None))
    }
  }
}

/// An NFSv3 file handle (`nfs_fh3`): opaque bytes, at most [`MAX_FH`], that name a volume object.
/// The bridge's map from a handle to a volume inode is the dispatch layer (owed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nfsfh3(pub Vec<u8>);

impl Nfsfh3 {
  /// Writes the handle as a variable opaque.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.opaque(&self.0);
  }

  /// Reads a handle, refusing one longer than [`MAX_FH`].
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Nfsfh3, XdrError> {
    Ok(Nfsfh3(reader.opaque(MAX_FH)?.to_vec()))
  }
}
