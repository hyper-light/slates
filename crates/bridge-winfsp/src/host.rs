//! The WinFsp mount host (§4.6 "Windows"): a live user-mode file system at a drive letter whose every request is
//! served by the daemon on the volume's owner shard, so a Windows program sees a slates volume as a normal drive — the
//! Windows analogue of the FUSE mount (`slates-server`'s `fuse`).
//!
//! **Threading (D-7, D-9).** WinFsp answers the kernel from a pool of dispatcher threads in this process, which are not
//! the runtime's; the volume lives on its owner shard and never leaves it. The design's rule is that "WinFsp requests
//! arrive on WinFsp's threads and are routed to the owning shard by handle" (§4.3 "Drivers"): each callback packages
//! its request as a [`Request`], sends it over a bounded channel to the mount's task on that shard, wakes the task, and
//! blocks its own thread for the reply. The task serves it whole, one request at a time, through the shared operation
//! layer ([`Request::serve`] over any [`Bridge`]) and answers. No lock, no shared ownership (R2): one consumer of the
//! channel, one reply channel per call. A dispatcher thread that finds the channel closed (the mount is being torn
//! down) answers `STATUS_UNSUCCESSFUL` at once, never a hang.
//!
//! **The mount thread.** `FspFileSystemSetMountPoint` defines a drive letter and broadcasts the device change, and
//! `FspFileSystemStopDispatcher` joins the dispatcher threads; neither belongs on a shard. So each mount has one owned
//! thread ([`MountThread`]) that creates, mounts and starts the file system, sends it each notification round, and
//! stops and deletes it, reporting each outcome to the task with a wake. The thread is the file system's only owner,
//! so its raw pointers never cross a thread. Its owner joins it ([`MountThread::stop`], or its drop).
//!
//! **Cache posture (§4.6 "Windows").** `FileInfoTimeout` is infinite: Windows keeps a file's attributes, listings and
//! pages until told, and the mount tells it of every change made through any other path by `FspFileSystemNotify`
//! (`crate::notify`), the analogue of the FUSE mount's notifications (A-141). The volume's free space has no
//! notification, so its timeout is zero: every query asks the shard, since any write anywhere changes it. A
//! notification round is begun with a zero timeout: while a rename holds WinFsp's rename lock it answers
//! `STATUS_CANT_WAIT`, and the round is owed again rather than waited for, since the rename itself may be waiting on
//! the shard.
//!
//! **Verification.** Every struct and export is hand-transcribed from winfsp's headers (`crate::ffi`) and cross-lints
//! on the Windows target from any host; the live mount runs on the native Windows CI runner through the CLI
//! (`crates/cli/tests/winfsp.rs`).

use std::ffi::c_void;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::task::Waker;
use std::thread::JoinHandle;

use slates_bridge_core::{Bridge, NodeAttr, ObjectId, OpContext, RenameFlags, SetAttr};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::GetSecurityDescriptorLength;

use crate::ffi::{
  self, Api, Boolean, DirInfo, FileInfo, FileSystem, GUARD_STRATEGY_COARSE, Interface, Ntstatus,
  PSecurityDescriptor, Pvoid, Pwstr, VolumeInfo, VolumeParams,
};
use crate::notify::OpenNames;
use crate::{STATUS_SUCCESS, file_attributes, filetime_from_unix_ns, ntstatus};

/// A typed WinFsp host failure (no panics; every refusal is a value).
#[derive(Debug)]
pub enum WinFspError {
  /// WinFsp's library could not be loaded or lacks an export this host calls: WinFsp is not installed (or predates
  /// the notification exports).
  NotInstalled,
  /// A `FspFileSystem*` call returned a failure `NTSTATUS`.
  Api {
    /// The call that failed.
    call: &'static str,
    /// Its `NTSTATUS`.
    status: i32,
  },
  /// The reference security descriptor could not be built (`ConvertStringSecurityDescriptor…`).
  SecurityDescriptor,
  /// The mount thread could not be started, or ended without reporting.
  MountThread,
}

impl std::fmt::Display for WinFspError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotInstalled => write!(
        f,
        "WinFsp is not installed (winfsp-x64.dll could not be loaded)"
      ),
      Self::Api { call, status } => write!(f, "{call} answered NTSTATUS {status:#010x}"),
      Self::SecurityDescriptor => write!(f, "the mount's security descriptor could not be built"),
      Self::MountThread => write!(f, "the mount thread could not run"),
    }
  }
}

/// Whether WinFsp's library loads on this host: the fact the daemon's transport report states (`WinFspNotInstalled`
/// where it does not). Loads it once for the process.
pub fn installed() -> bool {
  ffi::api().is_ok()
}

/// Format: the sector size the mount reports — 4096 bytes, the volume core's chunk/page unit, so an
/// allocation unit is one page.
const SECTOR_SIZE: u16 = 4096;
/// Format: one sector per allocation unit — the allocation unit equals the sector (the page), matching
/// the volume core's page-granular accounting.
const SECTORS_PER_UNIT: u16 = 1;
/// Format: `INFINITE` (`0xFFFF_FFFF`) as WinFsp's `FileInfoTimeout`: Windows keeps what it fetched until the mount
/// tells it otherwise (§4.6 "Windows": `FileInfoTimeout = -1` with `FspFileSystemNotify` invalidations). Under version-1
/// parameters the listing, security, stream and extended-attribute caches inherit it (WinFsp `src/sys/volume.c`).
const FILE_INFO_TIMEOUT_INFINITE: u32 = u32::MAX;
/// Derived: the volume-information cache timeout, zero. Free space changes with every write through any path and no
/// notification names it, so any positive timeout would report a stale number; a query costs one request to the shard.
const VOLUME_INFO_TIMEOUT_MS: u32 = 0;
/// Format: `VolumeInfoTimeoutValid`, bit 0 of the version-1 flag word (`fsctl.h`): the volume-information timeout is
/// set apart from `FileInfoTimeout`.
const VOLUME_INFO_TIMEOUT_VALID: u32 = 1 << 0;
/// Derived: the dispatcher threads WinFsp runs for the mount: two. The COARSE guard admits one callback at a time and
/// the owner shard serves one request at a time, so a second thread only takes the next request from the kernel while
/// the first waits for its reply; a third would wait at the guard.
const DISPATCHER_THREADS: u32 = 2;
/// Derived: the requests that can wait in the channel to the shard: one per dispatcher thread, since each blocks for
/// its reply before taking another.
pub const REQUEST_BOUND: usize = DISPATCHER_THREADS as usize;
/// Shape: the outcomes the mount thread can have unread at once: a notification round's (the task sends at most one
/// round before reading its outcome) or the mount's, and the stop's after either.
const OUTCOME_BOUND: usize = 2;
/// Shape: commands queued to the mount thread: one round out at a time.
const COMMAND_BOUND: usize = 1;
/// Format: the default permission bits a created file takes (`rw-r--r--`); the mount presents Windows
/// ACLs through the fixed descriptor, so the POSIX mode is bookkeeping the volume core carries.
const DEFAULT_FILE_MODE: u32 = 0o644;
/// Format: the default permission bits a created directory takes (`rwxr-xr-x`).
const DEFAULT_DIR_MODE: u32 = 0o755;
/// Format: the maximum file-name component length the mount reports (`MaxComponentLength`) — 255, the
/// NTFS/`MAX_PATH` component limit, matching the volume core's own 255-byte name cap (§4.5).
const MAX_COMPONENT_LENGTH: u16 = 255;
/// Format: an "allow Everyone full access" security descriptor in SDDL — owner and group the built-in
/// Administrators, a protected DACL granting `FA` (full access) to `WD` (Everyone). WinFsp requires a
/// valid descriptor for its access checks; the volume's own rights are the attachment's, enforced by the
/// shard on every request, so one fixed descriptor serves every object, as WinFsp's `memfs` sample starts
/// from a single SDDL string.
const EVERYONE_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;WD)";
/// Format: the cleanup flags that say the handle changed the file or asked for its deletion (`winfsp.h`
/// `FspCleanupDelete`, `SetAllocationSize`, `SetArchiveBit`, `SetLastWriteTime`, `SetChangeTime`): such a close
/// publishes before its reply, as a FUSE close's `flush` does. A close that only read (`SetLastAccessTime` alone, or
/// nothing) publishes nothing.
const CLEANUP_CHANGED: u32 = 0x01 | 0x02 | 0x10 | 0x40 | 0x80;

/// The per-open state WinFsp carries as an opaque `FileContext`: the object's identity and its bridge open handle.
/// Boxed and leaked to a raw pointer on open/create, reclaimed on close — created and destroyed only inside
/// COARSE-serialized callbacks, so never touched concurrently.
struct OpenFile {
  object: ObjectId,
  fh: u64,
}

/// One operation the owner shard runs on the volume, with the parameters marshalled from a WinFsp callback. Paths
/// arrive as Rust `String`s (converted from WinFsp's UTF-16), so the whole `Op` crosses the channel.
enum Op {
  VolumeInfo,
  /// Resolve a path to an object and its attributes (`GetSecurityByName`), or report it missing.
  Resolve {
    path: String,
  },
  /// Open an existing path (`Open`): resolve, then take a file or directory handle.
  Open {
    path: String,
  },
  /// Create a file or directory at a path (`Create`).
  Create {
    path: String,
    is_dir: bool,
  },
  Read {
    object: ObjectId,
    offset: u64,
    length: u32,
  },
  Write {
    object: ObjectId,
    offset: u64,
    data: Vec<u8>,
    append: bool,
  },
  Info {
    object: ObjectId,
  },
  /// List a directory's entries (name, attributes) — the shard reads them all; the trampoline handles the WinFsp
  /// marker and buffer fill.
  ReadDir {
    object: ObjectId,
    fh: u64,
  },
  SetSize {
    object: ObjectId,
    new_size: u64,
  },
  Truncate {
    object: ObjectId,
  },
  Rename {
    from: String,
    to: String,
  },
  /// `FlushFileBuffers` on a handle: the bridge's flush, then the publication that makes it durable.
  Flush {
    object: ObjectId,
    fh: u64,
  },
  /// The last handle of a file object closed (`Cleanup`) after it changed the file: deletes the path when asked
  /// (a file's `unlink`, an empty directory's `rmdir`, decided by the resolved kind), and publishes.
  Cleanup {
    delete: Option<String>,
  },
  Release {
    object: ObjectId,
    fh: u64,
  },
}

impl Op {
  /// Whether the request changed what survives a restart, so the shard publishes before it answers (§4.8, D-18): a
  /// namespace or size change, an explicit flush, or the close of a handle that changed its file. A write alone does
  /// not: Windows' cache writes back lazily, and its handle's close or flush carries the barrier, as a FUSE close's
  /// `flush` does.
  fn needs_barrier(&self) -> bool {
    matches!(
      self,
      Op::Create { .. }
        | Op::SetSize { .. }
        | Op::Truncate { .. }
        | Op::Rename { .. }
        | Op::Flush { .. }
        | Op::Cleanup { .. }
    )
  }
}

/// The shard's reply to an [`Op`].
enum Reply {
  Volume {
    total: u64,
    free: u64,
  },
  /// An object and its attributes (resolve/open/create/info/setattr paths).
  Object {
    object: ObjectId,
    info: FileInfo,
    fh: u64,
  },
  Data {
    bytes: Vec<u8>,
  },
  Wrote {
    written: u32,
    info: FileInfo,
  },
  Entries {
    entries: Vec<(String, FileInfo)>,
  },
  Ok,
  /// A typed refusal, already mapped to its `NTSTATUS`.
  Err(Ntstatus),
}

/// One request from a dispatcher thread: the operation and the channel its reply returns on. The mount's task takes it
/// from the channel [`MountThread::start`] was given and serves it on the owner shard ([`Request::serve`]).
pub struct Request {
  op: Op,
  reply: SyncSender<Reply>,
}

impl Request {
  /// Serves the request whole through `bridge` under `cx`, keeping `names` — what Windows holds open, which the
  /// mount's notifications name ([`crate::notify`]) — in step with its opens, closes and renames. The reply is not sent
  /// yet: the caller publishes first when [`Served::needs_barrier`] says so.
  pub fn serve(self, bridge: &mut dyn Bridge, cx: &OpContext, names: &mut OpenNames) -> Served {
    let barrier = self.op.needs_barrier();
    let opened = match &self.op {
      Op::Open { path } | Op::Create { path, .. } => Some(path.clone()),
      _ => None,
    };
    let closed = match &self.op {
      Op::Release { fh, .. } => Some(*fh),
      _ => None,
    };
    let renamed = match &self.op {
      Op::Rename { from, to } => Some((from.clone(), to.clone())),
      _ => None,
    };
    let reply = run(bridge, cx, self.op).unwrap_or_else(|e| Reply::Err(ntstatus(&e).as_i32()));
    match (&reply, opened, closed, renamed) {
      (Reply::Object { object, fh, .. }, Some(path), _, _) => {
        names.opened(*fh, object.inode, &path);
      }
      (_, _, Some(fh), _) => names.closed(fh),
      (Reply::Ok, _, _, Some((from, to))) => names.renamed(&from, &to),
      _ => {}
    }
    Served {
      reply,
      sender: self.reply,
      barrier,
    }
  }
}

/// A served request whose reply is still to be sent.
pub struct Served {
  reply: Reply,
  sender: SyncSender<Reply>,
  barrier: bool,
}

impl Served {
  /// Whether the request succeeded and changed what survives a restart: its reply waits for the shard's publication.
  pub fn needs_barrier(&self) -> bool {
    self.barrier && !matches!(self.reply, Reply::Err(_))
  }

  /// The publication was refused: the reply becomes `STATUS_UNEXPECTED_IO_ERROR`, the effect being in the volume but not
  /// durable, so the caller is told rather than promised survival.
  pub fn refuse_unpublished(&mut self) {
    self.reply = Reply::Err(crate::status_unexpected_io_error());
  }

  /// Sends the reply to the waiting dispatcher thread. `false` when that thread is gone (the mount is being torn down).
  pub fn send(self) -> bool {
    self.sender.send(self.reply).is_ok()
  }
}

/// Builds a [`FileInfo`] from the volume core's neutral attributes: the Windows attribute bits for the
/// kind, the size for both file and allocation size (page-granular), and the times as `FILETIME`.
fn file_info(attr: &NodeAttr) -> Result<FileInfo, VfsError> {
  Ok(FileInfo {
    file_attributes: file_attributes(attr.kind)?,
    reparse_tag: 0,
    allocation_size: attr.size,
    file_size: attr.size,
    creation_time: filetime_from_unix_ns(attr.ctime),
    last_access_time: filetime_from_unix_ns(attr.atime),
    last_write_time: filetime_from_unix_ns(attr.mtime),
    change_time: filetime_from_unix_ns(attr.ctime),
    index_number: attr.ino,
    hard_links: 0,
    ea_size: 0,
  })
}

/// Splits a WinFsp path (`\` or `\dir\file`, backslash-separated) into its non-empty components. The
/// root is the empty slice.
fn components(path: &str) -> Vec<&str> {
  path.split('\\').filter(|c| !c.is_empty()).collect()
}

/// Resolves a WinFsp path to its object id and attributes by walking `lookup` from the root. The root
/// (`\`) is the volume root itself.
fn resolve(bridge: &mut dyn Bridge, cx: &OpContext, path: &str) -> Result<NodeAttr, VfsError> {
  let root = bridge.root(cx)?;
  let mut current = bridge.getattr(ObjectId::new(root, 0), cx)?;
  for name in components(path) {
    current = bridge.lookup(ObjectId::new(current.ino, current.generation), cx, name)?;
  }
  Ok(current)
}

/// The attributes of `attr` as a reply naming its object.
fn object_reply(attr: &NodeAttr, fh: u64) -> Result<Reply, VfsError> {
  Ok(Reply::Object {
    object: ObjectId::new(attr.ino, attr.generation),
    info: file_info(attr)?,
    fh,
  })
}

/// The operation body, returning `Result` so the `?` operator threads refusals; [`Request::serve`] maps them.
fn run(bridge: &mut dyn Bridge, cx: &OpContext, op: Op) -> Result<Reply, VfsError> {
  Ok(match op {
    Op::VolumeInfo => {
      let root = bridge.root(cx)?;
      let stat = bridge.statfs(ObjectId::new(root, 0), cx)?;
      let total = stat.blocks.saturating_mul(u64::from(stat.bsize));
      let free = stat.bfree.saturating_mul(u64::from(stat.bsize));
      Reply::Volume { total, free }
    }
    Op::Resolve { path } => match resolve(bridge, cx, &path) {
      Ok(attr) => object_reply(&attr, 0)?,
      Err(e) => Reply::Err(ntstatus(&e).as_i32()),
    },
    Op::Open { path } => {
      let attr = resolve(bridge, cx, &path)?;
      let object = ObjectId::new(attr.ino, attr.generation);
      let fh = if matches!(attr.kind, Kind::Dir) {
        bridge.opendir(object, cx)?
      } else {
        bridge.open(object, cx, 0)?
      };
      object_reply(&attr, fh)?
    }
    Op::Create { path, is_dir } => op_create(bridge, cx, &path, is_dir)?,
    Op::Read {
      object,
      offset,
      length,
    } => {
      let mut out = Vec::new();
      bridge.read(object, cx, offset, length, &mut out)?;
      Reply::Data { bytes: out }
    }
    Op::Write {
      object,
      offset,
      data,
      append,
    } => {
      let at = if append {
        bridge.getattr(object, cx)?.size
      } else {
        offset
      };
      let written = bridge.write(object, cx, at, &data)?;
      let attr = bridge.getattr(object, cx)?;
      Reply::Wrote {
        written,
        info: file_info(&attr)?,
      }
    }
    Op::Info { object } => object_reply(&bridge.getattr(object, cx)?, 0)?,
    Op::ReadDir { object, fh } => Reply::Entries {
      entries: read_all_entries(bridge, cx, object, fh)?,
    },
    Op::SetSize { object, new_size } => {
      let attr = bridge.setattr(
        object,
        cx,
        SetAttr {
          size: Some(new_size),
          ..SetAttr::default()
        },
      )?;
      object_reply(&attr, 0)?
    }
    Op::Truncate { object } => {
      let attr = bridge.setattr(
        object,
        cx,
        SetAttr {
          size: Some(0),
          ..SetAttr::default()
        },
      )?;
      object_reply(&attr, 0)?
    }
    Op::Rename { from, to } => op_rename(bridge, cx, &from, &to)?,
    Op::Flush { object, fh } => {
      bridge.flush(object, cx, fh)?;
      Reply::Ok
    }
    Op::Cleanup { delete } => match delete {
      Some(path) => op_delete(bridge, cx, &path)?,
      None => Reply::Ok,
    },
    Op::Release { object, fh } => {
      bridge.release(object, cx, fh)?;
      Reply::Ok
    }
  })
}

/// Splits a WinFsp path into its final component and the resolved parent object (the directory the new
/// or renamed name lives in). A path with no component (`\`) has no parent, reported as
/// `STATUS_INVALID_PARAMETER`.
fn split_parent<'p>(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  path: &'p str,
) -> Result<Result<(&'p str, ObjectId), Ntstatus>, VfsError> {
  let parts = components(path);
  let Some((name, parent_parts)) = parts.split_last() else {
    return Ok(Err(crate::status_invalid_parameter()));
  };
  let parent = resolve(bridge, cx, &format!("\\{}", parent_parts.join("\\")))?;
  Ok(Ok((name, ObjectId::new(parent.ino, parent.generation))))
}

/// Creates a file or directory at `path`: split into parent and name, then `create`/`mkdir` and open a
/// handle so the returned context is ready for the writes that follow.
fn op_create(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  path: &str,
  is_dir: bool,
) -> Result<Reply, VfsError> {
  let (name, parent_id) = match split_parent(bridge, cx, path)? {
    Ok(pair) => pair,
    Err(status) => return Ok(Reply::Err(status)),
  };
  if is_dir {
    let attr = bridge.mkdir(parent_id, cx, name, DEFAULT_DIR_MODE)?;
    let fh = bridge.opendir(ObjectId::new(attr.ino, attr.generation), cx)?;
    object_reply(&attr, fh)
  } else {
    let (attr, fh) = bridge.create(parent_id, cx, name, DEFAULT_FILE_MODE, 0)?;
    object_reply(&attr, fh)
  }
}

/// Renames `from` to `to` by resolving each path's parent and final name.
fn op_rename(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  from: &str,
  to: &str,
) -> Result<Reply, VfsError> {
  let (from_name, old_parent) = match split_parent(bridge, cx, from)? {
    Ok(pair) => pair,
    Err(status) => return Ok(Reply::Err(status)),
  };
  let (to_name, new_parent) = match split_parent(bridge, cx, to)? {
    Ok(pair) => pair,
    Err(status) => return Ok(Reply::Err(status)),
  };
  bridge.rename(
    old_parent,
    new_parent,
    cx,
    from_name,
    to_name,
    RenameFlags::default(),
  )?;
  Ok(Reply::Ok)
}

/// Deletes `path`: an empty directory (`rmdir`) or a file (`unlink`), decided by the resolved kind.
fn op_delete(bridge: &mut dyn Bridge, cx: &OpContext, path: &str) -> Result<Reply, VfsError> {
  let (name, parent_id) = match split_parent(bridge, cx, path)? {
    Ok(pair) => pair,
    Err(status) => return Ok(Reply::Err(status)),
  };
  let target = bridge.lookup(parent_id, cx, name)?;
  if matches!(target.kind, Kind::Dir) {
    bridge.rmdir(parent_id, cx, name)?;
  } else {
    bridge.unlink(parent_id, cx, name)?;
  }
  Ok(Reply::Ok)
}

/// Reads a directory's full entry list (`.`/`..` plus its children, which the bridge's `readdir`
/// returns whole from offset 0), each resolved to its attributes by id — the shard side of
/// `ReadDirectory`; the trampoline applies the WinFsp marker and buffer fill. An entry that vanished
/// between the listing and its attribute lookup is reported with the entry's kind and a zero size
/// rather than dropped, so the listing never desyncs.
fn read_all_entries(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  fh: u64,
) -> Result<Vec<(String, FileInfo)>, VfsError> {
  // WinFsp asks for the whole listing at once and applies its own marker.
  let rows = bridge.readdir(object, cx, fh, 0, usize::MAX)?;
  let mut out = Vec::with_capacity(rows.len());
  for entry in rows {
    let info = match bridge.getattr(ObjectId::new(entry.ino, 0), cx) {
      Ok(attr) => file_info(&attr)?,
      Err(_) => FileInfo {
        file_attributes: file_attributes(entry.kind)?,
        ..FileInfo::default()
      },
    };
    out.push((entry.name, info));
  }
  Ok(out)
}

// --------------------------------------------------------------------- the WinFsp callback handler

/// The handler stored in the file system's `UserContext`: the channel to the mount's task, the waker of that task, and
/// the one reference security descriptor every object reports. `Send + Sync` (a `SyncSender`, a `Waker` and a byte
/// vector), so WinFsp's dispatcher threads reach it soundly; the COARSE guard serializes them regardless.
struct Handler {
  requests: SyncSender<Request>,
  wake: Waker,
  /// A self-relative security descriptor as raw bytes (built once from [`EVERYONE_SDDL`]).
  security_descriptor: Vec<u8>,
}

impl Handler {
  /// Sends `op` to the mount's task, wakes it, and blocks this dispatcher thread for its reply; a closed channel (the
  /// mount is being torn down) is `STATUS_UNSUCCESSFUL`, never a hang.
  fn call(&self, op: Op) -> Reply {
    let (reply, answer) = sync_channel(1);
    if self.requests.send(Request { op, reply }).is_err() {
      return Reply::Err(crate::status_unsuccessful());
    }
    self.wake.wake_by_ref();
    answer
      .recv()
      .unwrap_or(Reply::Err(crate::status_unsuccessful()))
  }
}

/// The handler behind a file system pointer, from its `UserContext`.
///
/// # Safety
/// `fs` must be a live `FileSystem` whose `UserContext` holds a `Handler` pointer set by the mount thread and not yet
/// reclaimed (the thread reclaims it only after the dispatcher has stopped).
unsafe fn handler<'a>(fs: *mut FileSystem) -> &'a Handler {
  // SAFETY: the contract above — a live file system whose UserContext is our leaked Handler.
  unsafe { &*((*fs).user_context as *const Handler) }
}

/// The `OpenFile` behind a WinFsp `FileContext`.
///
/// # Safety
/// `context` must be a pointer [`open_trampoline`]/`create` leaked and not yet reclaimed by `close`.
unsafe fn open_file<'a>(context: Pvoid) -> &'a OpenFile {
  // SAFETY: the contract above — a leaked OpenFile, accessed under the serializing guard.
  unsafe { &*(context as *const OpenFile) }
}

/// Copies a `FileInfo` into a WinFsp out-parameter when the pointer is non-null.
///
/// # Safety
/// `out` is either null or a writable `FileInfo` from WinFsp.
unsafe fn put_info(out: *mut FileInfo, info: FileInfo) {
  if !out.is_null() {
    // SAFETY: `out` is non-null and a valid FileInfo per the contract.
    unsafe { *out = info };
  }
}

/// Reads a WinFsp `PWSTR` into a Rust `String` (lossy on unpaired surrogates, which a real path never
/// carries). A null pointer is the empty string.
///
/// # Safety
/// `wide` is either null or a NUL-terminated UTF-16 string owned by WinFsp for the call's duration.
unsafe fn wide_to_string(wide: Pwstr) -> String {
  if wide.is_null() {
    return String::new();
  }
  let mut len = 0usize;
  // SAFETY: `wide` is a NUL-terminated UTF-16 string per the contract; we scan to the NUL, and never past
  // the longest string Windows can hand a file system (`MAX_WIDE_UNITS`).
  unsafe {
    while len < MAX_WIDE_UNITS && *wide.add(len) != 0 {
      len = len.saturating_add(1);
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(wide, len))
  }
}

/// Format: the most UTF-16 units a Windows path or name can hold — a `UNICODE_STRING` counts its length
/// in bytes in a `u16`, so 32,767 units — where a scan of a WinFsp string stops, NUL or not.
const MAX_WIDE_UNITS: usize = 32_767;

extern "C" fn get_volume_info(fs: *mut FileSystem, out: *mut VolumeInfo) -> Ntstatus {
  // SAFETY: WinFsp passes our live file system and a writable VolumeInfo.
  let handler = unsafe { handler(fs) };
  match handler.call(Op::VolumeInfo) {
    Reply::Volume { total, free } => {
      // SAFETY: `out` is a writable VolumeInfo for this call.
      unsafe {
        (*out).total_size = total;
        (*out).free_size = free;
        (*out).volume_label_length = 0;
      }
      STATUS_SUCCESS.as_i32()
    }
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn get_security_by_name(
  fs: *mut FileSystem,
  file_name: Pwstr,
  p_file_attributes: *mut u32,
  security_descriptor: PSecurityDescriptor,
  p_sd_size: *mut usize,
) -> Ntstatus {
  // SAFETY: our file system; `file_name` a NUL-terminated path.
  let handler = unsafe { handler(fs) };
  // SAFETY: `file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let path = unsafe { wide_to_string(file_name) };
  match handler.call(Op::Resolve { path }) {
    Reply::Object { info, .. } => {
      if !p_file_attributes.is_null() {
        // SAFETY: non-null attributes out-parameter.
        unsafe { *p_file_attributes = info.file_attributes };
      }
      // SAFETY: `p_sd_size`/`security_descriptor` follow WinFsp's in/out size protocol.
      unsafe { copy_security_descriptor(handler, security_descriptor, p_sd_size) }
    }
    Reply::Err(status) => status,
    _ => crate::status_object_name_not_found(),
  }
}

/// Copies the handler's reference security descriptor into WinFsp's buffer following the in/out size
/// protocol: `*p_sd_size` is the buffer capacity on entry and the descriptor's real size on exit; a too
/// small buffer is `STATUS_BUFFER_OVERFLOW` with the needed size reported.
///
/// # Safety
/// `p_sd_size` is null or a writable `SIZE_T`; `sd` is a buffer of the incoming `*p_sd_size` bytes.
unsafe fn copy_security_descriptor(
  handler: &Handler,
  sd: PSecurityDescriptor,
  p_sd_size: *mut usize,
) -> Ntstatus {
  if p_sd_size.is_null() {
    return STATUS_SUCCESS.as_i32();
  }
  let needed = handler.security_descriptor.len();
  // SAFETY: `p_sd_size` is a writable SIZE_T per the contract.
  let capacity = unsafe { *p_sd_size };
  // SAFETY: `p_sd_size` is the same writable SIZE_T; report the descriptor's real size.
  unsafe { *p_sd_size = needed };
  if sd.is_null() {
    return STATUS_SUCCESS.as_i32();
  }
  if capacity < needed {
    return crate::status_buffer_overflow();
  }
  // SAFETY: `sd` has at least `needed` bytes (checked), and the source is a live byte vector.
  unsafe {
    std::ptr::copy_nonoverlapping(
      handler.security_descriptor.as_ptr(),
      sd.cast::<u8>(),
      needed,
    );
  }
  STATUS_SUCCESS.as_i32()
}

/// Boxes an [`OpenFile`] for a WinFsp `FileContext` out-parameter and reports the file info.
fn finish_open(
  object: ObjectId,
  fh: u64,
  info: FileInfo,
  p_context: *mut Pvoid,
  p_info: *mut FileInfo,
) -> Ntstatus {
  let boxed = Box::into_raw(Box::new(OpenFile { object, fh }));
  // SAFETY: `p_context` is a writable PVOID out-parameter; `p_info` is handled by `put_info`.
  unsafe {
    *p_context = boxed.cast::<c_void>();
    put_info(p_info, info);
  }
  STATUS_SUCCESS.as_i32()
}

extern "C" fn create(
  fs: *mut FileSystem,
  file_name: Pwstr,
  create_options: u32,
  _granted_access: u32,
  _file_attributes: u32,
  _security_descriptor: PSecurityDescriptor,
  _allocation_size: u64,
  p_context: *mut Pvoid,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // SAFETY: our file system; `file_name` a path.
  let handler = unsafe { handler(fs) };
  // SAFETY: `file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let path = unsafe { wide_to_string(file_name) };
  let is_dir = create_options & ffi::FILE_DIRECTORY_FILE != 0;
  match handler.call(Op::Create { path, is_dir }) {
    Reply::Object { object, info, fh } => finish_open(object, fh, info, p_context, p_info),
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn open_trampoline(
  fs: *mut FileSystem,
  file_name: Pwstr,
  _create_options: u32,
  _granted_access: u32,
  p_context: *mut Pvoid,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // SAFETY: our file system; `file_name` a path.
  let handler = unsafe { handler(fs) };
  // SAFETY: `file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let path = unsafe { wide_to_string(file_name) };
  match handler.call(Op::Open { path }) {
    Reply::Object { object, info, fh } => finish_open(object, fh, info, p_context, p_info),
    Reply::Err(status) => status,
    _ => crate::status_object_name_not_found(),
  }
}

extern "C" fn overwrite(
  fs: *mut FileSystem,
  context: Pvoid,
  _file_attributes: u32,
  _replace: Boolean,
  _allocation_size: u64,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile leaked by open/create, taken under the COARSE guard.
  let object = unsafe { open_file(context) }.object;
  match handler.call(Op::Truncate { object }) {
    Reply::Object { info, .. } => {
      // SAFETY: `p_info` handled by put_info.
      unsafe { put_info(p_info, info) };
      STATUS_SUCCESS.as_i32()
    }
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn cleanup(fs: *mut FileSystem, _context: Pvoid, file_name: Pwstr, flags: u32) {
  if flags & CLEANUP_CHANGED == 0 {
    return;
  }
  // SAFETY: our file system; `file_name` the path, sent when a delete is asked for.
  let handler = unsafe { handler(fs) };
  let delete = (flags & ffi::FSP_CLEANUP_DELETE != 0).then(|| {
    // SAFETY: `file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
    unsafe { wide_to_string(file_name) }
  });
  // Cleanup returns nothing to the kernel: a refused delete or publication leaves the file as it was, which the
  // caller sees at its next look; the shard counts what it refused.
  let _ = handler.call(Op::Cleanup { delete });
}

extern "C" fn close(fs: *mut FileSystem, context: Pvoid) {
  // SAFETY: our file system; `context` a leaked OpenFile taken exactly once here.
  let handler = unsafe { handler(fs) };
  if context.is_null() {
    return;
  }
  // SAFETY: `context` is the OpenFile leaked by open/create, reclaimed exactly once here (WinFsp
  // calls Close once per context, after the dispatcher has stopped delivering other calls for it).
  let open = unsafe { Box::from_raw(context as *mut OpenFile) };
  // Close returns nothing to the kernel; the shard counts a release it refused.
  let _ = handler.call(Op::Release {
    object: open.object,
    fh: open.fh,
  });
}

extern "C" fn read(
  fs: *mut FileSystem,
  context: Pvoid,
  buffer: Pvoid,
  offset: u64,
  length: u32,
  p_bytes: *mut u32,
) -> Ntstatus {
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile leaked by open/create, taken under the COARSE guard.
  let object = unsafe { open_file(context) }.object;
  match handler.call(Op::Read {
    object,
    offset,
    length,
  }) {
    Reply::Data { bytes } => {
      let n = bytes
        .len()
        .min(usize::try_from(length).unwrap_or(usize::MAX));
      // SAFETY: `buffer` is a WinFsp buffer of at least `length` bytes; we copy `n <= length`.
      unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), n);
        *p_bytes = u32::try_from(n).unwrap_or(0);
      }
      STATUS_SUCCESS.as_i32()
    }
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn write(
  fs: *mut FileSystem,
  context: Pvoid,
  buffer: Pvoid,
  offset: u64,
  length: u32,
  write_to_end: Boolean,
  _constrained: Boolean,
  p_bytes: *mut u32,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile leaked by open/create, taken under the COARSE guard.
  let object = unsafe { open_file(context) }.object;
  let Ok(length) = usize::try_from(length) else {
    return crate::status_invalid_parameter();
  };
  // SAFETY: `buffer` is a WinFsp buffer of `length` bytes for this call.
  let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), length) }.to_vec();
  match handler.call(Op::Write {
    object,
    offset,
    data,
    append: write_to_end != 0,
  }) {
    Reply::Wrote { written, info } => {
      // SAFETY: `p_bytes` writable; `p_info` handled by put_info.
      unsafe {
        *p_bytes = written;
        put_info(p_info, info);
      }
      STATUS_SUCCESS.as_i32()
    }
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn flush(fs: *mut FileSystem, context: Pvoid, p_info: *mut FileInfo) -> Ntstatus {
  // A volume flush passes a null context: every publication already covers the whole shard, so there is nothing
  // further to make durable than each handle's own flush does.
  if context.is_null() {
    return STATUS_SUCCESS.as_i32();
  }
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile leaked by open/create, taken under the COARSE guard.
  let open = unsafe { open_file(context) };
  let (object, fh) = (open.object, open.fh);
  match handler.call(Op::Flush { object, fh }) {
    Reply::Ok => match handler.call(Op::Info { object }) {
      Reply::Object { info, .. } => {
        // SAFETY: `p_info` handled by put_info.
        unsafe { put_info(p_info, info) };
        STATUS_SUCCESS.as_i32()
      }
      Reply::Err(status) => status,
      _ => crate::status_unsuccessful(),
    },
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn get_file_info(
  fs: *mut FileSystem,
  context: Pvoid,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile leaked by open/create, taken under the COARSE guard.
  let object = unsafe { open_file(context) }.object;
  match handler.call(Op::Info { object }) {
    Reply::Object { info, .. } => {
      // SAFETY: `p_info` a writable FileInfo.
      unsafe { *p_info = info };
      STATUS_SUCCESS.as_i32()
    }
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn set_basic_info(
  fs: *mut FileSystem,
  context: Pvoid,
  _file_attributes: u32,
  _creation_time: u64,
  _last_access_time: u64,
  _last_write_time: u64,
  _change_time: u64,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // Times and attribute bits are accepted and reported from the current object (the volume keeps POSIX times, set
  // through its own writes); the object's live info is returned so the caller sees what the volume holds.
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile leaked by open/create, taken under the COARSE guard.
  let object = unsafe { open_file(context) }.object;
  match handler.call(Op::Info { object }) {
    Reply::Object { info, .. } => {
      // SAFETY: `p_info` writable.
      unsafe { put_info(p_info, info) };
      STATUS_SUCCESS.as_i32()
    }
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn set_file_size(
  fs: *mut FileSystem,
  context: Pvoid,
  new_size: u64,
  set_allocation_size: Boolean,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile leaked by open/create, taken under the COARSE guard.
  let object = unsafe { open_file(context) }.object;
  // Setting only the allocation size does not change the file size; report the current info unchanged.
  let op = if set_allocation_size != 0 {
    Op::Info { object }
  } else {
    Op::SetSize { object, new_size }
  };
  match handler.call(op) {
    Reply::Object { info, .. } => {
      // SAFETY: `p_info` writable.
      unsafe { put_info(p_info, info) };
      STATUS_SUCCESS.as_i32()
    }
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn can_delete(_fs: *mut FileSystem, _context: Pvoid, _file_name: Pwstr) -> Ntstatus {
  // The delete itself happens in Cleanup; here we only confirm deletability. An empty-directory check
  // is enforced by the volume core's `rmdir` at delete time (NotEmpty → the mapped NTSTATUS), so this
  // reports success and lets the real check run then.
  STATUS_SUCCESS.as_i32()
}

extern "C" fn rename(
  fs: *mut FileSystem,
  _context: Pvoid,
  file_name: Pwstr,
  new_file_name: Pwstr,
  _replace: Boolean,
) -> Ntstatus {
  // SAFETY: our file system; both names are paths.
  let handler = unsafe { handler(fs) };
  // SAFETY: `file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let from = unsafe { wide_to_string(file_name) };
  // SAFETY: `new_file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let to = unsafe { wide_to_string(new_file_name) };
  match handler.call(Op::Rename { from, to }) {
    Reply::Ok => STATUS_SUCCESS.as_i32(),
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn read_directory(
  fs: *mut FileSystem,
  context: Pvoid,
  _pattern: Pwstr,
  marker: Pwstr,
  buffer: Pvoid,
  length: u32,
  p_bytes: *mut u32,
) -> Ntstatus {
  // SAFETY: our file system.
  let handler = unsafe { handler(fs) };
  // SAFETY: `context` is a live OpenFile (the open directory) leaked by open, under the guard.
  let open = unsafe { open_file(context) };
  // SAFETY: `marker` is null or a NUL-terminated UTF-16 name WinFsp owns for this call.
  let marker = unsafe { wide_to_string(marker) };
  let entries = match handler.call(Op::ReadDir {
    object: open.object,
    fh: open.fh,
  }) {
    Reply::Entries { entries } => entries,
    Reply::Err(status) => return status,
    _ => return crate::status_unsuccessful(),
  };
  // SAFETY: `buffer`/`p_bytes` are WinFsp's directory buffer and its transferred-count out-parameter;
  // `FspFileSystemAddDirInfo` fills them, and a final NULL DirInfo marks the end.
  unsafe { fill_directory(&entries, &marker, buffer, length, p_bytes) }
}

/// Fills WinFsp's directory buffer from the gathered entries, skipping past the marker, until the
/// buffer is full or the entries are exhausted, then writes the end marker.
///
/// # Safety
/// `buffer` is a WinFsp directory buffer of `length` bytes and `p_bytes` a writable count; both live
/// for the call.
unsafe fn fill_directory(
  entries: &[(String, FileInfo)],
  marker: &str,
  buffer: Pvoid,
  length: u32,
  p_bytes: *mut u32,
) -> Ntstatus {
  let Ok(api) = ffi::api() else {
    return crate::status_unsuccessful();
  };
  let mut past_marker = marker.is_empty();
  for (name, info) in entries {
    if !past_marker {
      if name == marker {
        past_marker = true;
      }
      continue;
    }
    let name_utf16: Vec<u16> = name.encode_utf16().collect();
    // FSP_FSCTL_DIR_INFO: the 104-byte head then the name bytes, `Size` covering both in a `u16`. A name
    // whose record would not fit that field cannot be listed truthfully, so the listing is refused.
    let head = std::mem::size_of::<DirInfo>();
    let Some((name_bytes, size, record_size)) = name_utf16
      .len()
      .checked_mul(std::mem::size_of::<u16>())
      .and_then(|name_bytes| {
        let size = name_bytes.checked_add(head)?;
        Some((name_bytes, size, u16::try_from(size).ok()?))
      })
    else {
      return crate::status_name_too_long();
    };
    let mut scratch = vec![0u8; size];
    // SAFETY: `scratch` is `size` bytes, the DirInfo head plus the name's bytes; we write the head then
    // the name.
    unsafe {
      let dir_info = scratch.as_mut_ptr().cast::<DirInfo>();
      (*dir_info).size = record_size;
      (*dir_info).file_info = *info;
      (*dir_info).padding = [0u8; 24];
      std::ptr::copy_nonoverlapping(
        name_utf16.as_ptr().cast::<u8>(),
        scratch.as_mut_ptr().add(head),
        name_bytes,
      );
      if (api.add_dir_info)(
        scratch.as_mut_ptr().cast::<DirInfo>(),
        buffer,
        length,
        p_bytes,
      ) == 0
      {
        // The buffer is full; stop here (a later call resumes past the marker).
        return STATUS_SUCCESS.as_i32();
      }
    }
  }
  // A NULL DirInfo marks the end of the listing.
  // SAFETY: AddDirInfo accepts a null DirInfo to write the terminating marker.
  unsafe {
    (api.add_dir_info)(std::ptr::null_mut(), buffer, length, p_bytes);
  }
  STATUS_SUCCESS.as_i32()
}

/// The interface vtable — one process-lifetime static, its pointer stable for every mount. The slots
/// slates implements point at the trampolines; the rest are `None` (WinFsp: "not supported").
static INTERFACE: Interface = Interface {
  get_volume_info: Some(get_volume_info),
  get_security_by_name: Some(get_security_by_name),
  create: Some(create),
  open: Some(open_trampoline),
  overwrite: Some(overwrite),
  cleanup: Some(cleanup),
  close: Some(close),
  read: Some(read),
  write: Some(write),
  flush: Some(flush),
  get_file_info: Some(get_file_info),
  set_basic_info: Some(set_basic_info),
  set_file_size: Some(set_file_size),
  can_delete: Some(can_delete),
  rename: Some(rename),
  read_directory: Some(read_directory),
  ..Interface::EMPTY
};

// ------------------------------------------------------------------------------- the mount lifecycle

/// A live WinFsp file system: the object, the leaked handler, and the library's exports. Owned by the mount thread
/// for its whole life; unmounts, stops the dispatcher, deletes the file system and reclaims the handler on drop.
struct Mounted {
  api: &'static Api,
  file_system: *mut FileSystem,
  handler: *mut Handler,
}

/// What a notification round came to (§4.6 "Windows").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notified {
  /// Windows took the round: its work item invalidates and notifies each name (WinFsp processes a round
  /// asynchronously, `FspVolumeNotify`).
  Taken,
  /// A rename held WinFsp's rename lock (`STATUS_CANT_WAIT` from a zero-timeout begin): nothing was told, and the
  /// round is owed again.
  RenameInProgress,
}

impl Mounted {
  /// Creates the file system, mounts it at `mount_point`, and starts its dispatcher; every partial step is undone on a
  /// refusal.
  fn new(
    mount_point: &str,
    read_only: bool,
    requests: SyncSender<Request>,
    wake: Waker,
  ) -> Result<Mounted, WinFspError> {
    let api = ffi::api().map_err(|_| WinFspError::NotInstalled)?;
    let security_descriptor = everyone_security_descriptor()?;
    let handler = Box::into_raw(Box::new(Handler {
      requests,
      wake,
      security_descriptor,
    }));
    // SAFETY: an all-zero VolumeParams is a valid, empty parameter block; the fields below fill it.
    let mut params: VolumeParams = unsafe { std::mem::zeroed() };
    // Version-1 parameters (the version is the struct's size), so the volume-information timeout is set apart.
    params.version = u16::try_from(std::mem::size_of::<VolumeParams>()).unwrap_or(0);
    params.sector_size = SECTOR_SIZE;
    params.sectors_per_allocation_unit = SECTORS_PER_UNIT;
    params.max_component_length = MAX_COMPONENT_LENGTH;
    params.file_info_timeout = FILE_INFO_TIMEOUT_INFINITE;
    params.flags = ffi::VOLUME_FLAG_CASE_SENSITIVE_SEARCH
      | ffi::VOLUME_FLAG_CASE_PRESERVED_NAMES
      | ffi::VOLUME_FLAG_UNICODE_ON_DISK
      | ffi::VOLUME_FLAG_PERSISTENT_ACLS;
    if read_only {
      params.flags |= ffi::VOLUME_FLAG_READ_ONLY_VOLUME;
    }
    params.flags_v1 = VOLUME_INFO_TIMEOUT_VALID;
    params.volume_info_timeout = VOLUME_INFO_TIMEOUT_MS;
    write_wide(&mut params.file_system_name, "slates");

    let mut file_system: *mut FileSystem = std::ptr::null_mut();
    let mut device = wide_nul("WinFsp.Disk");
    // SAFETY: `device`/`params`/`INTERFACE` are live for the call; `file_system` is a writable out-ptr.
    let status =
      unsafe { (api.create)(device.as_mut_ptr(), &params, &INTERFACE, &mut file_system) };
    if status != 0 {
      // Nothing was created; no callback can reach the handler.
      // SAFETY: `handler` was just leaked and nothing else holds it.
      drop(unsafe { Box::from_raw(handler) });
      return Err(WinFspError::Api {
        call: "FspFileSystemCreate",
        status,
      });
    }
    let mut mounted = Mounted {
      api,
      file_system,
      handler,
    };
    // SAFETY: `file_system` is the object just created; store the handler and serialize callbacks.
    unsafe {
      (*file_system).user_context = handler.cast::<c_void>();
      (api.set_guard_strategy)(file_system, GUARD_STRATEGY_COARSE);
    }
    let mut mount_wide = wide_nul(mount_point);
    // SAFETY: `file_system` is live; `mount_wide` a NUL-terminated mount point.
    let status = unsafe { (api.set_mount_point)(file_system, mount_wide.as_mut_ptr()) };
    if status != 0 {
      mounted.file_system = std::ptr::null_mut();
      // SAFETY: the object was created and never mounted or dispatched; deleting it is its whole undo, and then the
      // handler has no holder.
      unsafe {
        (api.delete)(file_system);
        drop(Box::from_raw(handler));
      }
      return Err(WinFspError::Api {
        call: "FspFileSystemSetMountPoint",
        status,
      });
    }
    // SAFETY: `file_system` is live and mounted; start its dispatcher threads.
    let status = unsafe { (api.start_dispatcher)(file_system, DISPATCHER_THREADS) };
    if status != 0 {
      // The drop below removes the mount point and deletes the object; stopping a dispatcher that never started is a
      // no-op in WinFsp.
      return Err(WinFspError::Api {
        call: "FspFileSystemStartDispatcher",
        status,
      });
    }
    Ok(mounted)
  }

  /// Hands Windows one round of notification records (`crate::notify::NotifyRound::bytes`).
  fn notify(&self, round: &[u8]) -> Result<Notified, WinFspError> {
    // SAFETY: `file_system` is live and its dispatcher started; a zero timeout never sleeps.
    let begun = unsafe { (self.api.notify_begin)(self.file_system, 0) };
    if begun == ffi::STATUS_CANT_WAIT {
      return Ok(Notified::RenameInProgress);
    }
    if begun != 0 {
      return Err(WinFspError::Api {
        call: "FspFileSystemNotifyBegin",
        status: begun,
      });
    }
    // SAFETY: `round` is a live buffer of its length, read by the call (the kernel copies it before it returns).
    let told = unsafe {
      (self.api.notify)(
        self.file_system,
        round.as_ptr().cast_mut().cast::<c_void>(),
        round.len(),
      )
    };
    // SAFETY: as above; ends the session the begin opened, whatever the notify answered.
    let ended = unsafe { (self.api.notify_end)(self.file_system) };
    match (told, ended) {
      (0, 0) => Ok(Notified::Taken),
      (0, status) => Err(WinFspError::Api {
        call: "FspFileSystemNotifyEnd",
        status,
      }),
      (status, _) => Err(WinFspError::Api {
        call: "FspFileSystemNotify",
        status,
      }),
    }
  }
}

impl Drop for Mounted {
  fn drop(&mut self) {
    if self.file_system.is_null() {
      return;
    }
    // SAFETY: `file_system` is live; stop the dispatcher (its threads joined, so no callback runs after), unmount, and
    // delete it. A dispatcher thread waiting for the shard's reply returns once the channel's receiver is dropped,
    // which the mount's owner does before it stops the thread.
    unsafe {
      (self.api.stop_dispatcher)(self.file_system);
      (self.api.remove_mount_point)(self.file_system);
      (self.api.delete)(self.file_system);
    }
    // No callback can run now, so reclaim the handler.
    // SAFETY: the handler was leaked in `new` and the dispatcher is stopped, so this is the sole taker.
    drop(unsafe { Box::from_raw(self.handler) });
  }
}

/// A command to the mount thread.
enum Command {
  /// Hand Windows this round of notification records.
  Notify(Vec<u8>),
}

/// What the mount thread reports, each followed by a wake of the mount's task.
pub enum Outcome {
  /// The mount is up, or why it is not.
  Mounted(Result<(), WinFspError>),
  /// A notification round's outcome.
  Notified(Result<Notified, WinFspError>),
  /// The file system is unmounted and deleted; the thread is ending.
  Stopped,
}

/// The owned thread that holds a WinFsp mount for its whole life (see the module docs): it mounts, hands Windows each
/// notification round, and unmounts, reporting each outcome to the mount's task and waking it.
pub struct MountThread {
  commands: Option<SyncSender<Command>>,
  outcomes: Receiver<Outcome>,
  thread: Option<JoinHandle<()>>,
}

impl MountThread {
  /// Starts the thread, which mounts the volume at `mount_point` (`S:`) — read-only when `read_only` — with its
  /// requests sent on `requests` and `wake` woken after each, and reports [`Outcome::Mounted`]. Refused only when no
  /// thread can start.
  pub fn start(
    mount_point: &str,
    read_only: bool,
    requests: SyncSender<Request>,
    wake: Waker,
  ) -> Result<MountThread, WinFspError> {
    let (commands, command_rx) = sync_channel::<Command>(COMMAND_BOUND);
    let (outcome_tx, outcomes) = sync_channel::<Outcome>(OUTCOME_BOUND);
    let mount_point = mount_point.to_owned();
    let thread = std::thread::Builder::new()
      .name("slates-winfsp".to_owned())
      .spawn(move || {
        run_mount(
          &mount_point,
          read_only,
          (requests, wake),
          &command_rx,
          &outcome_tx,
        );
      })
      .map_err(|_| WinFspError::MountThread)?;
    Ok(MountThread {
      commands: Some(commands),
      outcomes,
      thread: Some(thread),
    })
  }

  /// The next outcome the thread reported, if any; [`Outcome::Stopped`] also when the thread ended without saying so.
  pub fn outcome(&mut self) -> Option<Outcome> {
    match self.outcomes.try_recv() {
      Ok(outcome) => Some(outcome),
      Err(TryRecvError::Empty) => None,
      Err(TryRecvError::Disconnected) => Some(Outcome::Stopped),
    }
  }

  /// Sends the thread a round of notification records; its [`Outcome::Notified`] follows. Refused when the thread is
  /// gone or a round is already out (the caller sends one at a time).
  pub fn notify(&self, round: Vec<u8>) -> Result<(), WinFspError> {
    let commands = self.commands.as_ref().ok_or(WinFspError::MountThread)?;
    commands
      .try_send(Command::Notify(round))
      .map_err(|_| WinFspError::MountThread)
  }

  /// Asks the thread to unmount and end: its [`Outcome::Stopped`] follows. The caller has dropped the request channel's
  /// receiver first, so no dispatcher thread waits on it while the dispatcher stops.
  pub fn stop(&mut self) {
    self.commands = None;
  }

  /// Joins the thread once it has reported [`Outcome::Stopped`] (it has only to return).
  pub fn join(&mut self) {
    self.commands = None;
    if let Some(thread) = self.thread.take() {
      // A thread of shipped code cannot panic (and a panic aborts the process); nothing is left to report.
      let _ = thread.join();
    }
  }
}

impl Drop for MountThread {
  fn drop(&mut self) {
    self.join();
  }
}

/// The mount thread's body: mount, report, serve commands until the owner drops the command channel, unmount, report.
fn run_mount(
  mount_point: &str,
  read_only: bool,
  (requests, wake): (SyncSender<Request>, Waker),
  commands: &Receiver<Command>,
  outcomes: &SyncSender<Outcome>,
) {
  let report = |outcome: Outcome| {
    // The receiver lives in the `MountThread`, which joins this thread before it drops; the bound holds every
    // outcome the protocol leaves unread at once (`OUTCOME_BOUND`).
    let _ = outcomes.send(outcome);
    wake.wake_by_ref();
  };
  let mounted = match Mounted::new(mount_point, read_only, requests, wake.clone()) {
    Ok(mounted) => {
      report(Outcome::Mounted(Ok(())));
      mounted
    }
    Err(error) => {
      report(Outcome::Mounted(Err(error)));
      report(Outcome::Stopped);
      return;
    }
  };
  while let Ok(Command::Notify(round)) = commands.recv() {
    report(Outcome::Notified(mounted.notify(&round)));
  }
  drop(mounted);
  report(Outcome::Stopped);
}

/// Builds the "allow Everyone" reference security descriptor as self-relative bytes, from [`EVERYONE_SDDL`].
fn everyone_security_descriptor() -> Result<Vec<u8>, WinFspError> {
  let sddl = wide_nul(EVERYONE_SDDL);
  let mut sd: PSecurityDescriptor = std::ptr::null_mut();
  let mut size: u32 = 0;
  // SAFETY: `sddl` is a NUL-terminated SDDL string; the call allocates a self-relative SD into `sd`
  // (with `LocalFree` ownership) and writes its size. SDDL_REVISION_1 = 1.
  let ok = unsafe {
    ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, &mut size)
  };
  if ok == 0 || sd.is_null() {
    return Err(WinFspError::SecurityDescriptor);
  }
  // SAFETY: `sd` is a valid self-relative descriptor; copy its bytes out, then free the original.
  let bytes = unsafe {
    let len = usize::try_from(GetSecurityDescriptorLength(sd)).unwrap_or(0);
    let bytes = std::slice::from_raw_parts(sd.cast::<u8>(), len).to_vec();
    LocalFree(sd.cast());
    bytes
  };
  Ok(bytes)
}

/// A NUL-terminated UTF-16 buffer from `s` (for a WinFsp `PWSTR` argument).
fn wide_nul(s: &str) -> Vec<u16> {
  s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Writes `s` as UTF-16 into a fixed WinFsp `WCHAR` field, NUL-terminated and truncated to fit.
fn write_wide(field: &mut [u16], s: &str) {
  let cap = field.len().saturating_sub(1);
  for (slot, unit) in field.iter_mut().zip(s.encode_utf16().take(cap)) {
    *slot = unit;
  }
}
