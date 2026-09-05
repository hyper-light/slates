//! The `Bridge` implementation over the volume core (§4.6 "one implementation in the core").
//! A [`VolumeBridge`] borrows a `Volume` and its `Store` and turns the kernel's requests, by
//! FUSE node id, into volume operations. The FUSE node id is the volume's inode number (the
//! design's `(no, gen)`; generation reuse after `forget` is owed and noted), with node id 1 the
//! root. File handles are a small counter into a table naming the inode they were opened on, so
//! a read or write finds its inode in O(1). Nothing here writes a host path; a scratch volume
//! lives entirely in RAM, so this is exercised on every host without a mount or a bridge
//! transport.

use slates_vfs::error::VfsError;
use slates_vfs::inode::{Attrs, Kind};
use slates_vfs::volume::{Store, Volume};

use crate::bridge::{Bridge, DirEntry};

use crate::reply::{Attr, EntryOut};

/// Format: the FUSE node id of the root directory.
const ROOT_NODE: u64 = 1;
/// Format: how long the kernel may cache an entry or attributes: forever (slates invalidates
/// explicitly on every mutation, §4.6).
const CACHE_FOREVER: u64 = u64::MAX;
/// Format: the Unix `d_type` values a `readdir` entry carries.
const DT_DIR: u32 = 4;
const DT_REG: u32 = 8;
const DT_LNK: u32 = 10;
// The Linux errno values the volume core's refusals map to (the FUSE ABI is Linux, so the
// numbers are the kernel's regardless of the host the codec is tested on; the dispatch negates
// them). Each is a Format constant.
/// Format: ENOENT, no such file or directory.
const ENOENT: i32 = 2;
/// Format: EPERM, operation not permitted.
const EPERM: i32 = 1;
/// Format: EIO, input/output error (an uncategorised refusal).
const EIO: i32 = 5;
/// Format: EEXIST, the name already exists.
const EEXIST: i32 = 17;
/// Format: ENOTDIR, not a directory.
const ENOTDIR: i32 = 20;
/// Format: EISDIR, is a directory.
const EISDIR: i32 = 21;
/// Format: EINVAL, invalid argument.
const EINVAL: i32 = 22;
/// Format: EFBIG, file too large.
const EFBIG: i32 = 27;
/// Format: ENOSPC, no space left.
const ENOSPC: i32 = 28;
/// Format: EMLINK, too many links.
const EMLINK: i32 = 31;
/// Format: ENOTEMPTY, directory not empty.
const ENOTEMPTY: i32 = 39;
/// Format: the block unit `fuse_attr.blocks` counts in (512-byte blocks, the stat convention).
const BYTES_PER_BLOCK: u64 = 512;
/// Shape: the block size reported to the kernel: one page, the volume core's chunk unit.
const BLKSIZE: u32 = 4096;
/// Format: the maximum name length the volume core allows (§4.5's name cap).
const NAME_MAX: u32 = 255;

/// The `Bridge` over one volume.
pub struct VolumeBridge<'v> {
  volume: &'v mut Volume,
  store: &'v mut Store,
  /// Open handles: the index is the handle, the value the inode it names (files and dirs share
  /// one space; the kernel never confuses them, and the table is bounded by open files).
  handles: Vec<Option<u64>>,
  /// Shape: the size a read is capped at when the request asks for more than one arena chunk
  /// (the daemon negotiated this at INIT; here it bounds a single reply buffer).
  max_read: usize,
}

impl std::fmt::Debug for VolumeBridge<'_> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("VolumeBridge")
      .field(
        "open_handles",
        &self.handles.iter().filter(|h| h.is_some()).count(),
      )
      .finish()
  }
}

/// Shape: the read cap, one arena chunk (256 KiB), matched to the INIT negotiation.
const MAX_READ: usize = 256 * 1024;

impl<'v> VolumeBridge<'v> {
  /// A bridge over `volume` and its `store`.
  pub fn new(volume: &'v mut Volume, store: &'v mut Store) -> VolumeBridge<'v> {
    VolumeBridge {
      volume,
      store,
      handles: Vec::new(),
      max_read: MAX_READ,
    }
  }

  /// The inode number a FUSE node id names (node id 1 is the root's inode).
  fn inode_of(&self, nodeid: u64) -> Result<u64, i32> {
    if nodeid == ROOT_NODE {
      self
        .volume
        .root_inode(self.store)
        .map(|no| no.0)
        .map_err(errno)
    } else {
      Ok(nodeid)
    }
  }

  /// Assigns a handle naming `inode`.
  fn open_handle(&mut self, inode: u64) -> u64 {
    let index = self.handles.len();
    self.handles.push(Some(inode));
    u64::try_from(index).unwrap_or(u64::MAX)
  }

  /// The `fuse_entry_out` for an inode number (its attributes, cached forever).
  fn entry_of(&self, no: u64) -> Result<EntryOut, i32> {
    let attrs = self
      .volume
      .stat(self.store, slates_vfs::ids::InodeNo(no))
      .map_err(errno)?;
    Ok(EntryOut {
      nodeid: no,
      generation: 0,
      entry_valid: CACHE_FOREVER,
      attr_valid: CACHE_FOREVER,
      attr: attr_of(no, &attrs),
    })
  }

  /// The inode a handle names, or an error when it is stale.
  fn handle_inode(&self, fh: u64) -> Result<u64, i32> {
    usize::try_from(fh)
      .ok()
      .and_then(|i| self.handles.get(i).copied().flatten())
      .ok_or(EINVAL)
  }
}

/// Maps a volume refusal to its POSIX errno.
fn errno(e: VfsError) -> i32 {
  match e {
    VfsError::NotFound => ENOENT,
    VfsError::AlreadyExists => EEXIST,
    VfsError::NotDirectory => ENOTDIR,
    VfsError::IsDirectory => EISDIR,
    VfsError::NotEmpty => ENOTEMPTY,
    VfsError::NoSpace => ENOSPC,
    VfsError::FileTooLarge => EFBIG,
    VfsError::TooManyLinks => EMLINK,
    VfsError::NotPermitted => EPERM,
    VfsError::Invalid | VfsError::InvalidName => EINVAL,
    VfsError::BaseUnavailable(code) => code,
    _ => EIO,
  }
}

/// The FUSE `d_type` for a volume entry kind.
fn dtype(kind: Kind) -> u32 {
  match kind {
    Kind::Dir => DT_DIR,
    Kind::File => DT_REG,
    Kind::Symlink => DT_LNK,
  }
}

/// A FUSE `fuse_attr` from the volume's attributes for inode `no`.
fn attr_of(no: u64, attrs: &Attrs) -> Attr {
  Attr {
    ino: no,
    size: attrs.size,
    blocks: attrs.size.div_ceil(BYTES_PER_BLOCK),
    mtime: split_ns(attrs.mtime),
    ctime: split_ns(attrs.mtime),
    atime: split_ns(attrs.atime),
    mode: attrs.mode,
    nlink: attrs.nlink,
    uid: attrs.uid,
    gid: attrs.gid,
    blksize: BLKSIZE,
  }
}

/// Splits a signed nanosecond time into (seconds, nanoseconds), clamping a negative time to
/// zero (the kernel takes unsigned seconds).
fn split_ns(ns: i64) -> (u64, u32) {
  /// Format: nanoseconds per second, splitting a time into (seconds, nanoseconds).
  const NS_PER_SEC: u64 = 1_000_000_000;
  let ns = u64::try_from(ns).unwrap_or(0);
  (ns / NS_PER_SEC, u32::try_from(ns % NS_PER_SEC).unwrap_or(0))
}

impl Bridge for VolumeBridge<'_> {
  fn lookup(&mut self, parent: u64, name: &str) -> Result<EntryOut, i32> {
    let dir = self.inode_of(parent)?;
    let located = self
      .volume
      .lookup_no(self.store, slates_vfs::ids::InodeNo(dir), name)
      .map_err(errno)?;
    let attrs = self.volume.stat(self.store, located.inode).map_err(errno)?;
    Ok(EntryOut {
      nodeid: located.inode.0,
      generation: 0,
      entry_valid: CACHE_FOREVER,
      attr_valid: CACHE_FOREVER,
      attr: attr_of(located.inode.0, &attrs),
    })
  }

  fn getattr(&mut self, nodeid: u64) -> Result<Attr, i32> {
    let no = self.inode_of(nodeid)?;
    let attrs = self
      .volume
      .stat(self.store, slates_vfs::ids::InodeNo(no))
      .map_err(errno)?;
    Ok(attr_of(no, &attrs))
  }

  fn open(&mut self, nodeid: u64, _flags: u32) -> Result<u64, i32> {
    let no = self.inode_of(nodeid)?;
    // A directory is opened through opendir; open refuses it.
    if self
      .volume
      .kind(self.store, slates_vfs::ids::InodeNo(no))
      .map_err(errno)?
      == Kind::Dir
    {
      return Err(EISDIR);
    }
    Ok(self.open_handle(no))
  }

  fn read(
    &mut self,
    _nodeid: u64,
    fh: u64,
    offset: u64,
    size: u32,
    out: &mut Vec<u8>,
  ) -> Result<(), i32> {
    let no = self.handle_inode(fh)?;
    let want = usize::try_from(size).unwrap_or(0).min(self.max_read);
    let mut buf = vec![0u8; want];
    let read = self
      .volume
      .read(self.store, slates_vfs::ids::InodeNo(no), offset, &mut buf)
      .map_err(errno)?;
    out.extend_from_slice(&buf[..read]);
    Ok(())
  }

  fn write(&mut self, _nodeid: u64, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
    let no = self.handle_inode(fh)?;
    let written = self
      .volume
      .write(self.store, slates_vfs::ids::InodeNo(no), offset, data)
      .map_err(errno)?;
    u32::try_from(written).map_err(|_| EIO)
  }

  fn opendir(&mut self, nodeid: u64) -> Result<u64, i32> {
    let no = self.inode_of(nodeid)?;
    if self
      .volume
      .kind(self.store, slates_vfs::ids::InodeNo(no))
      .map_err(errno)?
      != Kind::Dir
    {
      return Err(ENOTDIR);
    }
    Ok(self.open_handle(no))
  }

  fn readdir(&mut self, nodeid: u64, _fh: u64, offset: u64) -> Result<Vec<DirEntry>, i32> {
    let no = self.inode_of(nodeid)?;
    let rows = self
      .volume
      .readdir_no(self.store, slates_vfs::ids::InodeNo(no))
      .map_err(errno)?;
    let start = usize::try_from(offset).unwrap_or(0);
    Ok(
      rows
        .into_iter()
        .skip(start)
        .map(|row| DirEntry {
          ino: row.inode.0,
          kind: dtype(row.kind),
          name: row.name.to_owned(),
        })
        .collect(),
    )
  }

  fn create(
    &mut self,
    parent: u64,
    name: &str,
    mode: u32,
    _flags: u32,
  ) -> Result<(EntryOut, u64), i32> {
    let dir = self.inode_of(parent)?;
    let no = self
      .volume
      .create_file_no(self.store, slates_vfs::ids::InodeNo(dir), name, mode)
      .map_err(errno)?;
    let attrs = self.volume.stat(self.store, no).map_err(errno)?;
    let entry = EntryOut {
      nodeid: no.0,
      generation: 0,
      entry_valid: CACHE_FOREVER,
      attr_valid: CACHE_FOREVER,
      attr: attr_of(no.0, &attrs),
    };
    let fh = self.open_handle(no.0);
    Ok((entry, fh))
  }

  fn release(&mut self, _nodeid: u64, fh: u64) -> Result<(), i32> {
    if let Some(slot) = usize::try_from(fh)
      .ok()
      .and_then(|i| self.handles.get_mut(i))
    {
      *slot = None;
    }
    Ok(())
  }

  fn forget(&mut self, _nodeid: u64, _nlookup: u64) {
    // Node ids are inode numbers, never reclaimed while the volume lives; a forget is a hint
    // the kernel dropped its cache. Generation-tracked reuse is owed (§4.6 `(no, gen)`).
  }

  fn flush(&mut self, _nodeid: u64, _fh: u64) -> Result<(), i32> {
    // No disk write: the data is already in the anchor segment (§4.6). Success.
    Ok(())
  }

  fn mkdir(&mut self, parent: u64, name: &str, mode: u32) -> Result<EntryOut, i32> {
    let dir = self.inode_of(parent)?;
    let no = self
      .volume
      .mkdir_no(self.store, slates_vfs::ids::InodeNo(dir), name, mode)
      .map_err(errno)?;
    self.entry_of(no.0)
  }

  fn unlink(&mut self, parent: u64, name: &str) -> Result<(), i32> {
    let dir = self.inode_of(parent)?;
    self
      .volume
      .unlink_no(self.store, slates_vfs::ids::InodeNo(dir), name)
      .map_err(errno)
  }

  fn rmdir(&mut self, parent: u64, name: &str) -> Result<(), i32> {
    let dir = self.inode_of(parent)?;
    self
      .volume
      .rmdir_no(self.store, slates_vfs::ids::InodeNo(dir), name)
      .map_err(errno)
  }

  fn symlink(&mut self, parent: u64, name: &str, target: &str) -> Result<EntryOut, i32> {
    let dir = self.inode_of(parent)?;
    let no = self
      .volume
      .symlink_no(self.store, slates_vfs::ids::InodeNo(dir), name, target)
      .map_err(errno)?;
    self.entry_of(no.0)
  }

  fn readlink(&mut self, nodeid: u64) -> Result<String, i32> {
    let no = self.inode_of(nodeid)?;
    self
      .volume
      .readlink(self.store, slates_vfs::ids::InodeNo(no))
      .map(|t| t.into_string())
      .map_err(errno)
  }

  fn rename(
    &mut self,
    old_parent: u64,
    old_name: &str,
    new_parent: u64,
    new_name: &str,
  ) -> Result<(), i32> {
    let from = self.inode_of(old_parent)?;
    let to = self.inode_of(new_parent)?;
    self
      .volume
      .rename_no(
        self.store,
        slates_vfs::ids::InodeNo(from),
        old_name,
        slates_vfs::ids::InodeNo(to),
        new_name,
      )
      .map_err(errno)
  }

  fn setattr(&mut self, nodeid: u64, valid: u32, size: u64, mode: u32) -> Result<Attr, i32> {
    let no = self.inode_of(nodeid)?;
    let inode = slates_vfs::ids::InodeNo(no);
    if valid & crate::request::SetAttrIn::FATTR_SIZE != 0 {
      self
        .volume
        .truncate(self.store, inode, size)
        .map_err(errno)?;
    }
    if valid & crate::request::SetAttrIn::FATTR_MODE != 0 {
      self.volume.chmod(self.store, inode, mode).map_err(errno)?;
    }
    let attrs = self.volume.stat(self.store, inode).map_err(errno)?;
    Ok(attr_of(no, &attrs))
  }

  fn statfs(&mut self, _nodeid: u64) -> Result<crate::reply::StatfsOut, i32> {
    let accounting = self.volume.accounting();
    // Blocks are the volume's referenced bytes over the block size; the volume core does not
    // expose a hard cap here (a dynamic volume grows), so free is reported generously and the
    // quota is enforced on write, not by statfs. The kernel uses this only for `df`.
    let block = u64::from(BLKSIZE);
    let used = accounting.referenced_bytes.div_ceil(block);
    Ok(crate::reply::StatfsOut {
      blocks: used.saturating_mul(2).max(1),
      bfree: used,
      bavail: used,
      files: 0,
      ffree: 0,
      bsize: BLKSIZE,
      namelen: NAME_MAX,
      frsize: BLKSIZE,
    })
  }
}
