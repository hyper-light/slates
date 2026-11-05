//! The WinFsp mount host (§4.6 "Windows"): a live user-mode file system at a drive letter whose every request is
//! served by the daemon on the volume's owner shard, so a Windows program sees a slates volume as a normal drive — the
//! Windows analogue of the FUSE mount (`slates-server`'s `fuse`).
//!
//! **Threading (D-7, D-9, A-198).** One owned receiver answers the kernel through the DLL's operation table; the
//! DLL's receive pool is replaced by the retained direct-output receiver (`crate::dispatcher`). The volume lives on
//! its owner shard and never leaves it. Following §4.3's routing rule, each callback packages
//! its request as a [`Request`], sends it over a bounded channel to the mount's task on that shard, wakes the task, and
//! blocks its own thread for the reply. The task serves it whole, one request at a time, through the shared operation
//! layer ([`Request::serve`] over any [`Bridge`]) and answers. No lock, no shared ownership (R2): one consumer of the
//! channel, one reply channel per call. A dispatcher thread that finds the channel closed (the mount is being torn
//! down) answers `STATUS_UNSUCCESSFUL` at once, never a hang.
//!
//! **The mount thread (A-198).** Each mount has one owned command thread ([`MountThread`]) that creates the DLL's file system,
//! defines the temporary drive link through `crate::held`, starts the receiver and sends notification rounds. The
//! anchor holds duplicates of the volume control, drive-link and open-ledger objects. After a death the successor
//! creates a fresh DLL operation table and substitutes the inherited control handle and volume name; per-open contexts
//! and original kernel names resolve through `crate::opens` on the shard. Teardown joins the receiver before deleting
//! the DLL object and closing this process's volume/link handles. The anchor's copies keep a killed daemon's volume
//! and temporary link alive. Raw framework pointers remain owned until both threads have been joined.
//!
//! **Cache posture (§4.6 "Windows").** `FileInfoTimeout` is infinite: Windows keeps a file's attributes, listings and
//! pages until told, and the mount tells it of every change made through any other path by `FspFileSystemNotify`
//! (`crate::notify`), the analogue of the FUSE mount's notifications (A-141). The volume's free space has no
//! notification, so its timeout is zero: every query asks the shard, since any write anywhere changes it. A
//! notification round is begun with a zero timeout: while a rename holds WinFsp's rename lock it answers
//! `STATUS_CANT_WAIT`, and the round is owed again rather than waited for, since the rename itself may be waiting on
//! the shard.
//!
//! **Mapped views (A-190, an experiment).** A notification's purge leaves a mapped file's pages (`crate::purge` says
//! why), so after a round Windows took, the mount thread opens each changed file through the drive unbuffered and writes
//! the changed range, which makes WinFsp purge coherently. A private restricting-SID marker distinguishes these opens
//! from ordinary I/O by an embedding application. They address the held file by identity at its original cached name,
//! even after another attachment renamed it; ordinary callers still resolve the namespace. The purge's write is an
//! echo. A separate event carries Begin/End progress even when Windows refuses an open without calling the host.
//!
//! **Verification.** Every struct and export is hand-transcribed from winfsp's headers (`crate::ffi`) and cross-lints
//! on the Windows target from any host; the live mount runs on the native Windows CI runner through the CLI
//! (`crates/cli/tests/winfsp.rs`).

use std::ffi::c_void;
use std::os::windows::io::{
  AsRawHandle, BorrowedHandle, FromRawHandle, IntoRawHandle, OwnedHandle,
};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::task::Waker;
use std::thread::JoinHandle;

use slates_bridge_core::{Bridge, NodeAttr, ObjectId, OpContext, RenameFlags, SetAttr};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

use windows_sys::Win32::Foundation::{
  GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::GetSecurityDescriptorLength;
use windows_sys::Win32::Storage::FileSystem::{
  CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
  FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::ffi::{
  self, Api, Boolean, DirInfo, FileInfo, FileSystem, Interface, Ntstatus, PSecurityDescriptor,
  Pvoid, Pwstr, VolumeInfo, VolumeParams,
};
use crate::held::{
  Record,
  windows::{Drive, Inherited, Offer},
};
use crate::notify::OpenNames;
use crate::opens::{Ledger, Open, RESTORED_HANDLE};
use crate::purge::{Purge, PurgeOutcome, PurgeTarget, purge_through};
use crate::{STATUS_SUCCESS, file_attributes, filetime_from_unix_ns, ntstatus};
use slates_mem::SparseObject;

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
  /// The held volume, drive or access record was malformed or did not match the catalog.
  Record(crate::held::Refusal),
  /// A checked open-ledger allocation, read or write was refused.
  Ledger(crate::opens::Refusal),
  /// The retained direct-output receive journal was malformed or could not be settled.
  Capture(crate::inflight::Refusal),
  /// The loaded DLL is not the version this host's receiver was written from (`crate::dll_version`).
  UnsupportedDll(crate::dll_version::UnsupportedVersion),
  /// A Windows object operation refused, preserving its system error code.
  Os {
    /// The Windows object operation.
    call: &'static str,
    /// Its original system error code.
    code: Option<i32>,
  },
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
      Self::Record(reason) => write!(f, "the held mount record was refused: {reason:?}"),
      Self::Ledger(reason) => write!(f, "{reason}"),
      Self::Capture(reason) => write!(f, "{reason}"),
      Self::UnsupportedDll(refusal) => write!(f, "{refusal}"),
      Self::Os { call, code } => write!(f, "{call} refused: {code:?}"),
    }
  }
}

/// Refuses a DLL other than the version the receiver was written from, before anything touches its layout
/// (`crate::dll_version`). `FspVersion` itself takes no file system.
fn checked_dll(api: &ffi::Api) -> Result<(), WinFspError> {
  let mut found = 0u32;
  // SAFETY: the export was found by name in the loaded library, and `found` is a writable word for the call.
  let status = unsafe { (api.version)(&mut found) };
  if status != 0 {
    return Err(WinFspError::Api {
      call: "FspVersion",
      status,
    });
  }
  crate::dll_version::check(found).map_err(WinFspError::UnsupportedDll)
}

/// Whether WinFsp's library loads on this host: the fact the daemon's transport report states (`WinFspNotInstalled`
/// where it does not). Loads it once for the process.
pub fn installed() -> bool {
  ffi::api().is_ok()
}

/// Format: the sector size the mount reports — 4096 bytes, the volume core's chunk/page unit, so an
/// allocation unit is one page.
const SECTOR_SIZE: u16 = 4096;
/// The bytes of the mount's sector: what a purge's range is widened to (`crate::purge::purge_range`), since an
/// unbuffered transfer moves whole sectors.
pub fn sector_bytes() -> u64 {
  u64::from(SECTOR_SIZE)
}

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
/// Shape: the one receiver waits for its current callback's answer before sending another request to the shard.
pub const REQUEST_BOUND: usize = 1;
/// Shape: the outcomes the mount thread can have unread at once: a command's (the task sends at most one command, a
/// round or a purge, before reading its outcome) or the mount's, and the stop's after either.
const OUTCOME_BOUND: usize = 4;
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

/// One operation the owner shard runs on the volume, with the parameters marshalled from a WinFsp callback. Paths
/// arrive as Rust `String`s (converted from WinFsp's UTF-16), so the whole `Op` crosses the channel.
enum Op<T> {
  /// One durable kernel-name update after a successful rename response was saved, before it reaches the driver.
  RenameName {
    after: Option<u64>,
    from: String,
    to: String,
  },
  /// The receiver has handed this context to the driver; its single-byte confirmation precedes another callback.
  Confirm {
    context: u64,
  },
  VolumeInfo,
  /// Resolve a path to an object and its attributes (`GetSecurityByName`), or report it missing.
  Resolve {
    path: String,
  },
  /// Open an existing path (`Open`): resolve, then take a file or directory handle.
  Open {
    path: String,
  },
  /// Resolve a private coherence open by identity, after its kernel token marker was checked.
  PurgeResolve {
    object: ObjectId,
    ancestor: bool,
  },
  PurgeOpen {
    path: String,
    object: ObjectId,
  },
  /// Create a file or directory at a path (`Create`).
  Create {
    path: String,
    is_dir: bool,
  },
  Read {
    object: T,
    offset: u64,
    length: u32,
  },
  Write {
    object: T,
    offset: u64,
    data: Vec<u8>,
    append: bool,
    /// A paging write the cache manager makes (`ConstrainedIo`): it may not change the file's size, so it is clipped to
    /// the size the file has, as WinFsp's `memfs` clips it.
    constrained: bool,
  },
  Info {
    object: T,
  },
  /// List a directory's entries (name, attributes) — the shard reads them all; the trampoline handles the WinFsp
  /// marker and buffer fill.
  ReadDir {
    object: T,
  },
  SetSize {
    object: T,
    new_size: u64,
  },
  Truncate {
    object: T,
  },
  /// `MoveFileEx` and its kin: `replace` is WinFsp's `ReplaceIfExists`, the caller's `MOVEFILE_REPLACE_EXISTING`
  /// (A-214: a move not asked to replace must refuse a taken name, `t/base/04.t`).
  Rename {
    from: String,
    to: String,
    replace: bool,
  },
  /// `SetFileSize` with `SetAllocationSize`: an allocation below the file's size truncates the file to it (WinFsp's
  /// contract for `SetFileSize`; an open with `TRUNCATE_EXISTING` arrives so, A-214, `t/base/05.t`), and any other
  /// allocation changes nothing the volume keeps.
  SetAllocation {
    object: T,
    new_size: u64,
  },
  /// `SetBasicInfo`'s times (`SetFileTime`): each `Some` is applied to the volume's access, modification or change
  /// time (A-214, `t/base/06.t`); WinFsp passes a creation time too, which the volume does not keep settable.
  SetTimes {
    object: T,
    atime: Option<i64>,
    mtime: Option<i64>,
    ctime: Option<i64>,
  },
  /// `CanDelete`: whether a delete asked of an open object may be granted, decided before Windows marks it for
  /// deletion. A directory with any child refuses (A-214, `t/base/03.t`): the delete itself runs in Cleanup, which
  /// answers the kernel nothing, so a refusal there reached no program.
  CanDelete {
    object: T,
  },
  /// `FlushFileBuffers` on a handle: the bridge's flush, then the publication that makes it durable.
  Flush {
    object: T,
  },
  /// The last handle of a file object closed (`Cleanup`) after it changed the file: deletes the path when asked
  /// (a file's `unlink`, an empty directory's `rmdir`, decided by the resolved kind), and publishes.
  Cleanup {
    object: T,
    delete: Option<String>,
  },
  Release {
    object: T,
  },
}

impl<T> Op<T> {
  /// Whether the request changed what survives a restart, so the shard publishes before it answers (§4.8, D-18): a
  /// namespace or size change, an explicit flush, or the close of a handle that changed its file. A write alone does
  /// not: Windows' cache writes back lazily, and its handle's close or flush carries the barrier, as a FUSE close's
  /// `flush` does.
  fn needs_barrier(&self) -> bool {
    matches!(
      self,
      Op::Open { .. }
        | Op::PurgeOpen { .. }
        | Op::Release { .. }
        | Op::Create { .. }
        | Op::SetSize { .. }
        | Op::Truncate { .. }
        | Op::SetTimes { .. }
        | Op::SetAllocation { .. }
        | Op::Rename { .. }
        | Op::Flush { .. }
        | Op::Cleanup { .. }
    )
  }
}

/// The shard's reply to an [`Op`].
enum Reply {
  /// The last context visited by a cooperative name update, or None when all live contexts were visited.
  RenamedName(Option<u64>),
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
  Opened {
    context: u64,
    info: FileInfo,
  },
  Ok,
  /// A typed refusal, already mapped to its `NTSTATUS`.
  Err(Ntstatus),
}

/// One request from a dispatcher thread: the operation and the channel its reply returns on. The mount's task takes it
/// from the channel [`MountThread::start`] was given and serves it on the owner shard ([`Request::serve`]).
pub struct Request {
  op: Op<u64>,
  reply: SyncSender<Reply>,
  hint: u64,
}

impl Request {
  /// Serves on the owner shard. Kernel contexts are resolved here, never on a foreign dispatcher thread. Opens are
  /// recorded before their contexts are returned; only a successful close retires a token.
  pub fn serve(
    self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    names: &mut OpenNames,
    ledger: &mut Ledger<SparseObject>,
  ) -> Served {
    if let Op::RenameName { after, from, to } = &self.op {
      let reply = rename_one_name(names, ledger, *after, from, to);
      return Served {
        reply,
        sender: self.reply,
        barrier: false,
        echo: false,
        restored_read: false,
      };
    }
    if let Op::Confirm { context } = &self.op {
      let reply = confirm_reply(ledger, *context);
      return Served {
        reply,
        sender: self.reply,
        barrier: false,
        echo: false,
        restored_read: false,
      };
    }
    self.serve_operation(bridge, cx, names, ledger)
  }

  /// Runs a filesystem operation after the receiver's confirmation messages have been handled above.
  fn serve_operation(
    self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    names: &mut OpenNames,
    ledger: &mut Ledger<SparseObject>,
  ) -> Served {
    let opened = match &self.op {
      Op::Open { path } => Some((path.clone(), false)),
      Op::PurgeOpen { path, .. } => Some((path.clone(), true)),
      Op::Create { path, .. } => Some((path.clone(), false)),
      _ => None,
    };
    let closed = match &self.op {
      Op::Release { object } => Some(*object),
      _ => None,
    };
    let refused = admission_refusal(&self.op, names, ledger);
    let bound = bind(self.op, ledger);
    let barrier = bound.as_ref().is_ok_and(|op| {
      op.needs_barrier() && !matches!(op, Op::Cleanup { object, .. } if object.purge)
    });
    let echo = matches!(&bound, Ok(Op::Write { object, .. }) if object.purge);
    let restored_read =
      matches!(&bound, Ok(Op::Read { object, .. }) if object.fh == RESTORED_HANDLE);
    let mut reply = match (bound, refused) {
      (Ok(_op), Some(refusal)) => Reply::Err(ntstatus(&refusal).as_i32()),
      (Ok(op), None) => {
        run(bridge, cx, op).unwrap_or_else(|error| Reply::Err(ntstatus(&error).as_i32()))
      }
      (Err(error), _) => {
        eprintln!("slates-winfsp: context refused: {error}");
        Reply::Err(ntstatus(&VfsError::StaleHandle).as_i32())
      }
    };
    reply = record_open_reply(reply, opened, self.hint, bridge, cx, names, ledger);
    reply = finish_context_effects(reply, closed, names, ledger);
    Served {
      reply,
      sender: self.reply,
      barrier,
      echo,
      restored_read,
    }
  }
}

fn confirm_reply(ledger: &mut Ledger<SparseObject>, context: u64) -> Reply {
  match ledger.confirmed(context) {
    Ok(()) => Reply::Ok,
    Err(error) => {
      eprintln!("slates-winfsp: open confirmation refused: {error}");
      Reply::Err(ntstatus(&VfsError::RecoveryIncomplete).as_i32())
    }
  }
}

fn rename_one_name(
  names: &mut OpenNames,
  ledger: &mut Ledger<SparseObject>,
  after: Option<u64>,
  from: &str,
  to: &str,
) -> Reply {
  let Some((context, inode)) = names.next_open(after) else {
    return Reply::RenamedName(None);
  };
  let result = ledger
    .rename_name(context, from, to)
    .and_then(|()| ledger.name(context));
  match result {
    Ok(name) => {
      names.opened(context, inode, &name);
      Reply::RenamedName(Some(context))
    }
    Err(error) => {
      eprintln!("slates-winfsp: durable kernel rename refused: {error}");
      Reply::Err(ntstatus(&VfsError::RecoveryIncomplete).as_i32())
    }
  }
}

/// Why `op` must not run, decided before it has any effect: an open with no ledger slot left, or a rename that would
/// lengthen a name Windows holds past what the ledger records ([`OpenNames::rename_fits`]).
fn admission_refusal<C>(
  op: &Op<C>,
  names: &OpenNames,
  ledger: &Ledger<SparseObject>,
) -> Option<VfsError> {
  match op {
    Op::Open { .. } | Op::PurgeOpen { .. } | Op::Create { .. } if !ledger.has_room() => {
      Some(VfsError::NoSpace)
    }
    Op::Rename { from, to, .. } if !names.rename_fits(from, to) => Some(VfsError::NameTooLong),
    _ => None,
  }
}

/// Records a new kernel context before returning it; a refused record releases the newly allocated bridge handle.
fn record_open_reply(
  reply: Reply,
  opened: Option<(String, bool)>,
  hint: u64,
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  names: &mut OpenNames,
  ledger: &mut Ledger<SparseObject>,
) -> Reply {
  let (Reply::Object { object, fh, info }, Some((path, purge))) = (&reply, opened) else {
    return reply;
  };
  let (object, fh, info) = (*object, *fh, *info);
  match ledger.opened_for(Open { object, fh, purge }, &path, hint) {
    Ok(context) => {
      names.opened(context, object.inode, &path);
      Reply::Opened { context, info }
    }
    Err(error) => {
      eprintln!("slates-winfsp: open record refused: {error}");
      if let Err(release) = bridge.release(object, cx, fh) {
        eprintln!("slates-winfsp: refused open cleanup failed: {release}");
      }
      Reply::Err(ntstatus(&VfsError::RecoveryIncomplete).as_i32())
    }
  }
}

/// Retires a released context. Kernel-name changes wait until the receiver has saved the complete rename result.
fn finish_context_effects(
  reply: Reply,
  closed: Option<u64>,
  names: &mut OpenNames,
  ledger: &mut Ledger<SparseObject>,
) -> Reply {
  if let Some(context) = closed {
    match ledger.closed(context) {
      Ok(()) | Err(crate::opens::Refusal::Stale) => {}
      Err(error) => {
        eprintln!("slates-winfsp: close record refused: {error}");
        return Reply::Err(ntstatus(&VfsError::RecoveryIncomplete).as_i32());
      }
    }
    names.closed(context);
  }
  reply
}

/// Binds opaque context tokens to their durable per-open records. The resulting operation cannot contain a pointer
/// or a context that has not been checked against its generation.
fn bind(op: Op<u64>, ledger: &Ledger<SparseObject>) -> Result<Op<Open>, crate::opens::Refusal> {
  Ok(match op {
    Op::RenameName { after, from, to } => Op::RenameName { after, from, to },
    Op::Confirm { context } => Op::Confirm { context },
    Op::VolumeInfo => Op::VolumeInfo,
    Op::Resolve { path } => Op::Resolve { path },
    Op::Open { path } => Op::Open { path },
    Op::PurgeResolve { object, ancestor } => Op::PurgeResolve { object, ancestor },
    Op::PurgeOpen { path, object } => Op::PurgeOpen { path, object },
    Op::Create { path, is_dir } => Op::Create { path, is_dir },
    Op::Read {
      object,
      offset,
      length,
    } => Op::Read {
      object: ledger.get(object)?,
      offset,
      length,
    },
    Op::Write {
      object,
      offset,
      data,
      append,
      constrained,
    } => Op::Write {
      object: ledger.get(object)?,
      offset,
      data,
      append,
      constrained,
    },
    Op::Info { object } => Op::Info {
      object: ledger.get(object)?,
    },
    Op::ReadDir { object } => Op::ReadDir {
      object: ledger.get(object)?,
    },
    Op::SetSize { object, new_size } => Op::SetSize {
      object: ledger.get(object)?,
      new_size,
    },
    Op::Truncate { object } => Op::Truncate {
      object: ledger.get(object)?,
    },
    Op::Rename { from, to, replace } => Op::Rename { from, to, replace },
    Op::CanDelete { object } => Op::CanDelete {
      object: ledger.get(object)?,
    },
    Op::SetAllocation { object, new_size } => Op::SetAllocation {
      object: ledger.get(object)?,
      new_size,
    },
    Op::SetTimes {
      object,
      atime,
      mtime,
      ctime,
    } => Op::SetTimes {
      object: ledger.get(object)?,
      atime,
      mtime,
      ctime,
    },
    Op::Flush { object } => Op::Flush {
      object: ledger.get(object)?,
    },
    Op::Cleanup { object, delete } => Op::Cleanup {
      object: ledger.get(object)?,
      delete,
    },
    Op::Release { object } => Op::Release {
      object: ledger.get(object)?,
    },
  })
}

/// A served request whose reply is still to be sent.
pub struct Served {
  reply: Reply,
  sender: SyncSender<Reply>,
  barrier: bool,
  echo: bool,
  restored_read: bool,
}

impl Served {
  /// A successful read addressed an open restored from the predecessor's ledger. This counts actual server reads,
  /// so cached bytes alone cannot make the live takeover oracle pass.
  pub fn is_restored_read(&self) -> bool {
    self.restored_read && !matches!(self.reply, Reply::Err(_))
  }

  /// Whether the request was a purge's write, answered without being applied: the count that shows purges reach the
  /// host (`crate::purge`).
  pub fn is_purge_echo(&self) -> bool {
    self.echo
  }

  /// Whether the request succeeded and changed what survives a restart: its reply waits for the shard's publication.
  pub fn needs_barrier(&self) -> bool {
    self.barrier
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
fn run(bridge: &mut dyn Bridge, cx: &OpContext, op: Op<Open>) -> Result<Reply, VfsError> {
  Ok(match op {
    Op::RenameName { .. } => return Err(VfsError::RecoveryIncomplete),
    Op::Confirm { .. } => return Err(VfsError::RecoveryIncomplete),
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
    Op::PurgeResolve { object, ancestor } => {
      let object = if ancestor {
        ObjectId::new(bridge.root(cx)?, 0)
      } else {
        object
      };
      object_reply(&bridge.getattr(object, cx)?, 0)?
    }
    Op::PurgeOpen { object, .. } => {
      let attr = bridge.getattr(object, cx)?;
      let fh = bridge.open(object, cx, 0)?;
      object_reply(&attr, fh)?
    }
    Op::Create { path, is_dir } => op_create(bridge, cx, &path, is_dir)?,
    Op::Read {
      object,
      offset,
      length,
    } => {
      let mut out = Vec::new();
      bridge.read(object.object, cx, offset, length, &mut out)?;
      Reply::Data { bytes: out }
    }
    Op::Write {
      object,
      offset,
      data,
      append,
      constrained,
    } => {
      let open = object;
      let object = open.object;
      if open.purge {
        return Ok(Reply::Wrote {
          written: u32::try_from(data.len()).map_err(|_| VfsError::Invalid)?,
          info: file_info(&bridge.getattr(object, cx)?)?,
        });
      }
      let size = bridge.getattr(object, cx)?.size;
      let at = if append && !constrained { size } else { offset };
      // A paging write covers whole pages, past the file's end; only the bytes within its size are the file's. Before
      // this, a 62-byte file written by PowerShell grew to its 4096-byte page, the rest zeros (CI run 40251802164).
      let within = if constrained {
        usize::try_from(size.saturating_sub(at)).unwrap_or(usize::MAX)
      } else {
        data.len()
      };
      let bytes = data.get(..within.min(data.len())).unwrap_or_default();
      let written = if bytes.is_empty() {
        0
      } else {
        bridge.write(object, cx, at, bytes)?
      };
      let attr = bridge.getattr(object, cx)?;
      Reply::Wrote {
        written,
        info: file_info(&attr)?,
      }
    }
    Op::Info { object } => object_reply(&bridge.getattr(object.object, cx)?, 0)?,
    Op::ReadDir { object } => Reply::Entries {
      entries: read_all_entries(bridge, cx, object.object, object.fh)?,
    },
    Op::SetSize { object, new_size } => {
      let attr = bridge.setattr(
        object.object,
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
        object.object,
        cx,
        SetAttr {
          size: Some(0),
          ..SetAttr::default()
        },
      )?;
      object_reply(&attr, 0)?
    }
    Op::Rename { from, to, replace } => op_rename(bridge, cx, &from, &to, replace)?,
    Op::CanDelete { object } => op_can_delete(bridge, cx, object.object, object.fh)?,
    Op::SetAllocation { object, new_size } => {
      op_set_allocation(bridge, cx, object.object, new_size)?
    }
    Op::SetTimes {
      object,
      atime,
      mtime,
      ctime,
    } => op_set_times(
      bridge,
      cx,
      object.object,
      SetAttr {
        atime,
        mtime,
        ctime,
        ..SetAttr::default()
      },
    )?,
    Op::Flush { object } => {
      bridge.flush(object.object, cx, object.fh)?;
      object_reply(&bridge.getattr(object.object, cx)?, 0)?
    }
    Op::Cleanup { object, delete } => {
      if object.purge {
        Reply::Ok
      } else {
        match delete {
          Some(path) => op_delete(bridge, cx, &path, object.object)?,
          None => Reply::Ok,
        }
      }
    }
    Op::Release { object } => {
      bridge.release(object.object, cx, object.fh)?;
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
  replace: bool,
) -> Result<Reply, VfsError> {
  let (from_name, old_parent) = match split_parent(bridge, cx, from)? {
    Ok(pair) => pair,
    Err(status) => return Ok(Reply::Err(status)),
  };
  let (to_name, new_parent) = match split_parent(bridge, cx, to)? {
    Ok(pair) => pair,
    Err(status) => return Ok(Reply::Err(status)),
  };
  if replace && replaces_a_directory(bridge, cx, (old_parent, from_name), (new_parent, to_name))? {
    return Err(VfsError::NotPermitted);
  }
  bridge.rename(
    old_parent,
    new_parent,
    cx,
    from_name,
    to_name,
    RenameFlags {
      no_replace: !replace,
      ..RenameFlags::default()
    },
  )?;
  Ok(Reply::Ok)
}

/// Whether a rename asked to replace would replace a directory other than its own source: Windows never does
/// (`MoveFileEx` with `MOVEFILE_REPLACE_EXISTING` onto a directory answers `ERROR_ACCESS_DENIED`, WinFsp's memfs refuses
/// it so; `t/base/04.t` case 26), where POSIX `rename` replaces an empty one. A name that resolves to the source itself (a
/// rename of case alone) replaces nothing.
fn replaces_a_directory(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  (old_parent, from_name): (ObjectId, &str),
  (new_parent, to_name): (ObjectId, &str),
) -> Result<bool, VfsError> {
  let target = match bridge.lookup(new_parent, cx, to_name) {
    Ok(target) => target,
    Err(VfsError::NotFound) => return Ok(false),
    Err(error) => return Err(error),
  };
  if !matches!(target.kind, Kind::Dir) {
    return Ok(false);
  }
  let source = bridge.lookup(old_parent, cx, from_name)?;
  Ok((source.ino, source.generation) != (target.ino, target.generation))
}

/// `SetFileSize` with `SetAllocationSize`: an allocation below the file's size truncates the file to it; any other
/// leaves it as it is. The object's information either way.
fn op_set_allocation(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  new_size: u64,
) -> Result<Reply, VfsError> {
  let attr = bridge.getattr(object, cx)?;
  let attr = if new_size < attr.size {
    bridge.setattr(
      object,
      cx,
      SetAttr {
        size: Some(new_size),
        ..SetAttr::default()
      },
    )?
  } else {
    attr
  };
  object_reply(&attr, 0)
}

/// `SetBasicInfo`'s times applied (`times` carries only them); a call that sets none reads the object.
fn op_set_times(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  times: SetAttr,
) -> Result<Reply, VfsError> {
  let attr = if times == SetAttr::default() {
    bridge.getattr(object, cx)?
  } else {
    bridge.setattr(object, cx, times)?
  };
  object_reply(&attr, 0)
}

/// Format: the most entries `CanDelete` reads of a directory: its `.` and `..`, which the bridge's `readdir` lists
/// first, and one child, which is enough to refuse.
const DOTS_AND_ONE_CHILD: usize = 3;

/// Whether `object` may be deleted: a directory refuses `NotEmpty` (`STATUS_DIRECTORY_NOT_EMPTY`) while it has any child,
/// read from the first few entries of its listing; any other object may.
fn op_can_delete(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  fh: u64,
) -> Result<Reply, VfsError> {
  if !matches!(bridge.getattr(object, cx)?.kind, Kind::Dir) {
    return Ok(Reply::Ok);
  }
  let rows = bridge.readdir(object, cx, fh, 0, DOTS_AND_ONE_CHILD)?;
  if rows.iter().any(|row| row.name != "." && row.name != "..") {
    return Err(VfsError::NotEmpty);
  }
  Ok(Reply::Ok)
}

/// Deletes `path`: an empty directory (`rmdir`) or a file (`unlink`), decided by the resolved kind.
fn op_delete(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  path: &str,
  opened: ObjectId,
) -> Result<Reply, VfsError> {
  let (name, parent_id) = match split_parent(bridge, cx, path)? {
    Ok(pair) => pair,
    Err(status) => return Ok(Reply::Err(status)),
  };
  let target = match bridge.lookup(parent_id, cx, name) {
    Ok(target) => target,
    Err(VfsError::NotFound) => return Ok(Reply::Ok),
    Err(error) => return Err(error),
  };
  if ObjectId::new(target.ino, target.generation) != opened {
    return Err(VfsError::StaleHandle);
  }
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
/// vector), so the owned receiver reaches it soundly. That single caller needs no DLL operation guard.
struct Handler {
  requests: SyncSender<Request>,
  wake: Waker,
  /// A self-relative security descriptor as raw bytes (built once from [`EVERYONE_SDDL`]).
  security_descriptor: Vec<u8>,
}

impl Handler {
  /// Sends `op` to the mount's task, wakes it, and blocks this dispatcher thread for its reply; a closed channel (the
  /// mount is being torn down) is `STATUS_UNSUCCESSFUL`, never a hang.
  fn call(&self, op: Op<u64>) -> Reply {
    call_on(&self.requests, &self.wake, op)
  }
}

fn call_on(requests: &SyncSender<Request>, wake: &Waker, op: Op<u64>) -> Reply {
  let (reply, answer) = sync_channel(1);
  if requests
    .send(Request {
      op,
      reply,
      hint: crate::dispatcher::current_hint(),
    })
    .is_err()
  {
    return Reply::Err(crate::status_unsuccessful());
  }
  wake.wake_by_ref();
  answer
    .recv()
    .unwrap_or(Reply::Err(crate::status_unsuccessful()))
}

pub(crate) fn confirm_open(
  requests: &SyncSender<Request>,
  wake: &Waker,
  context: u64,
) -> Result<(), WinFspError> {
  match call_on(requests, wake, Op::Confirm { context }) {
    Reply::Ok => Ok(()),
    Reply::Err(status) => Err(WinFspError::Api {
      call: "confirm handed open",
      status,
    }),
    _ => Err(WinFspError::Ledger(crate::opens::Refusal::Corrupt)),
  }
}

/// Once the complete successful response is retained, update one context per owner turn. A second crash replays
/// that response and finishes these idempotent updates before the kernel can rename its node or receive fresh work.
pub(crate) fn rename_open_names(
  requests: &SyncSender<Request>,
  wake: &Waker,
  from: &str,
  to: &str,
  bound: usize,
) -> Result<(), WinFspError> {
  let mut after = None;
  for _ in 0..=bound {
    match call_on(
      requests,
      wake,
      Op::RenameName {
        after,
        from: from.to_owned(),
        to: to.to_owned(),
      },
    ) {
      Reply::RenamedName(None) => return Ok(()),
      Reply::RenamedName(Some(context)) if after.is_none_or(|previous| previous < context) => {
        after = Some(context)
      }
      Reply::Err(status) => {
        return Err(WinFspError::Api {
          call: "record kernel rename",
          status,
        });
      }
      _ => return Err(WinFspError::Ledger(crate::opens::Refusal::Corrupt)),
    }
  }
  Err(WinFspError::Ledger(crate::opens::Refusal::Bound))
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

/// WinFsp carries this integer opaquely as UserContext2; it is never a pointer into a daemon's heap.
fn context_key(context: Pvoid) -> u64 {
  u64::try_from(context.addr()).unwrap_or(0)
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
  let op = match crate::dispatcher::private_path(&path) {
    Ok(None) => Op::Resolve { path },
    Ok(Some(crate::purge::PrivatePath::File(object))) => Op::PurgeResolve {
      object,
      ancestor: false,
    },
    Ok(Some(crate::purge::PrivatePath::Ancestor)) => Op::PurgeResolve {
      object: ObjectId::new(0, 0),
      ancestor: true,
    },
    Err(status) => return status,
  };
  match handler.call(op) {
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

/// Returns a generation-checked ledger token in WinFsp's opaque context field, with no allocation or pointer borrow.
fn finish_open(
  context: u64,
  info: FileInfo,
  p_context: *mut Pvoid,
  p_info: *mut FileInfo,
) -> Ntstatus {
  let Ok(context) = usize::try_from(context) else {
    return crate::status_invalid_parameter();
  };
  // SAFETY: WinFsp owns these writable output parameters. The context is an opaque integer, never dereferenced.
  unsafe {
    *p_context = std::ptr::without_provenance_mut(context);
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
    Reply::Opened { context, info } => finish_open(context, info, p_context, p_info),
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
  let op = match crate::dispatcher::private_path(&path) {
    Ok(None) => Op::Open { path },
    Ok(Some(crate::purge::PrivatePath::File(object))) => Op::PurgeOpen { path, object },
    _ => return ntstatus(&VfsError::NotPermitted).as_i32(),
  };
  match handler.call(op) {
    Reply::Opened { context, info } => finish_open(context, info, p_context, p_info),
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
  let object = context_key(context);
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

extern "C" fn cleanup(fs: *mut FileSystem, context: Pvoid, file_name: Pwstr, flags: u32) {
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
  report_terminal_reply(
    "cleanup",
    handler.call(Op::Cleanup {
      object: context_key(context),
      delete,
    }),
  );
}

extern "C" fn close(fs: *mut FileSystem, context: Pvoid) {
  // SAFETY: the file system remains live for this callback; the context is only an integer token.
  let handler = unsafe { handler(fs) };
  if context.is_null() {
    return;
  }
  report_terminal_reply(
    "close",
    handler.call(Op::Release {
      object: context_key(context),
    }),
  );
}

fn report_terminal_reply(call: &'static str, reply: Reply) {
  if let Reply::Err(status) = reply {
    eprintln!("slates-winfsp: {call} refused with NTSTATUS {status:#010x}");
    crate::dispatcher::record_terminal_refusal(status);
  }
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
  let object = context_key(context);
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
  constrained: Boolean,
  p_bytes: *mut u32,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  let object = context_key(context);
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
    constrained: constrained != 0,
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
  match handler.call(Op::Flush {
    object: context_key(context),
  }) {
    Reply::Object { info, .. } => {
      // SAFETY: `p_info` is WinFsp's writable output parameter.
      unsafe { put_info(p_info, info) };
      STATUS_SUCCESS.as_i32()
    }
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
  let object = context_key(context);
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
  last_access_time: u64,
  last_write_time: u64,
  change_time: u64,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // The access, write and change times are applied (A-214); the attribute bits and the creation time are not kept by
  // the volume (A-214's stated differences), and the object's live info is returned so the caller sees what it holds.
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  let object = context_key(context);
  match handler.call(Op::SetTimes {
    object,
    atime: crate::unix_ns_from_filetime(last_access_time),
    mtime: crate::unix_ns_from_filetime(last_write_time),
    ctime: crate::unix_ns_from_filetime(change_time),
  }) {
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
  let object = context_key(context);
  // An allocation below the file's size truncates it, as WinFsp's `SetFileSize` contract says (an open with
  // `TRUNCATE_EXISTING` arrives as an allocation of zero); any other allocation leaves the file as it is (A-214).
  let op = if set_allocation_size != 0 {
    Op::SetAllocation { object, new_size }
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

extern "C" fn can_delete(fs: *mut FileSystem, context: Pvoid, _file_name: Pwstr) -> Ntstatus {
  // The delete itself happens in Cleanup, which answers the kernel nothing, so the refusal a program must see (a
  // directory with a child, `ERROR_DIR_NOT_EMPTY`) is decided here, before Windows marks the object for deletion.
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(fs) };
  match handler.call(Op::CanDelete {
    object: context_key(context),
  }) {
    Reply::Ok => STATUS_SUCCESS.as_i32(),
    Reply::Err(status) => status,
    _ => crate::status_unsuccessful(),
  }
}

extern "C" fn rename(
  fs: *mut FileSystem,
  _context: Pvoid,
  file_name: Pwstr,
  new_file_name: Pwstr,
  replace: Boolean,
) -> Ntstatus {
  // SAFETY: our file system; both names are paths.
  let handler = unsafe { handler(fs) };
  // SAFETY: `file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let from = unsafe { wide_to_string(file_name) };
  // SAFETY: `new_file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let to = unsafe { wide_to_string(new_file_name) };
  match handler.call(Op::Rename {
    from,
    to,
    replace: replace != 0,
  }) {
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
  // SAFETY: `marker` is null or a NUL-terminated UTF-16 name WinFsp owns for this call.
  let marker = unsafe { wide_to_string(marker) };
  let entries = match handler.call(Op::ReadDir {
    object: context_key(context),
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
/// for its whole life; joins the receiver, closes its own held-object copies and reclaims the DLL object/handler on drop.
struct Mounted {
  api: &'static Api,
  file_system: *mut FileSystem,
  handler: *mut Handler,
  /// The drive it is mounted at (`S:`), which a purge's path is opened under.
  mount_point: String,
  drive: Option<Drive>,
  record: Record,
  worker: Option<crate::dispatcher::Worker>,
  purge_token: crate::purge_token::PurgeToken,
  purge_intents: crate::purge_intents::Sender,
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
    inherited: Option<(Record, OwnedHandle, OwnedHandle)>,
    purge_intents: crate::purge_intents::Sender,
    private_opens: &mut crate::purge_intents::Intents,
  ) -> Result<Mounted, WinFspError> {
    let drive: [u16; 2] = mount_point
      .encode_utf16()
      .collect::<Vec<_>>()
      .try_into()
      .map_err(|_| WinFspError::Record(crate::held::Refusal::Drive))?;
    if !drive
      .first()
      .copied()
      .and_then(|letter| u8::try_from(letter).ok())
      .is_some_and(|letter| letter.is_ascii_alphabetic())
      || drive.get(1) != Some(&u16::from(b':'))
    {
      return Err(WinFspError::Record(crate::held::Refusal::Drive));
    }
    let api = ffi::api().map_err(|_| WinFspError::NotInstalled)?;
    checked_dll(api)?;
    let purge_token = crate::purge_token::PurgeToken::new()?;
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
    // WinFsp's default, named here because a purge Windows did not take is made again until this passes
    // (`crate::retry::REQUEST_TIMEOUT_MS`): the two are one number.
    params.irp_timeout = crate::retry::REQUEST_TIMEOUT_MS;
    params.transact_timeout = crate::dispatcher::TRANSACT_TIMEOUT_MS;
    // A closed file's bytes are written back before its close's request (which publishes them) and its pages dropped:
    // a Windows program's write reaches every other path by its close, not at the cache manager's lazy write-back
    // seconds later. Without it, CI's first run (40249524066) read a file PowerShell had written and closed as 62 zero
    // bytes through slates: its size had reached the shard, and its bytes were still in Windows' cache.
    params.flags = ffi::VOLUME_FLAG_CASE_SENSITIVE_SEARCH
      | ffi::VOLUME_FLAG_CASE_PRESERVED_NAMES
      | ffi::VOLUME_FLAG_UNICODE_ON_DISK
      | ffi::VOLUME_FLAG_PERSISTENT_ACLS
      | ffi::VOLUME_FLAG_FLUSH_AND_PURGE_ON_CLEANUP
      // Each open's `FileContext` is its own ledger token (A-198), retired by its own Close. Without this flag the
      // file node supplies its first open's context to every later open. The old boxed-context implementation freed
      // that first box twice (run 40251802164); a stable token would likewise name the wrong open.
      | ffi::VOLUME_FLAG_FILE_CONTEXT_PER_OPEN;
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
    // SAFETY: FspFileSystemCreate returned this initialized public prefix, including the terminated volume name.
    let volume = unsafe { (*file_system).volume_name };
    let mut mounted = Mounted {
      api,
      file_system,
      handler,
      mount_point: mount_point.to_owned(),
      drive: None,
      record: Record {
        volume,
        drive,
        read_only,
      },
      worker: None,
      purge_token,
      purge_intents,
    };
    // SAFETY: `file_system` is the DLL's live full object. Only its stable public prefix is accessed here.
    unsafe {
      (*file_system).user_context = handler.cast::<c_void>();
    }
    if let Some((record, volume, link)) = inherited {
      if record.drive_text().map_err(WinFspError::Record)? != mount_point
        || record.read_only != read_only
      {
        return Err(WinFspError::Record(crate::held::Refusal::Drive));
      }
      // SAFETY: the throwaway volume belongs only to this undispatched DLL object. Move its ownership out and the
      // held volume's ownership in; FspFileSystemDelete will close only the substituted handle. The create call was
      // necessary to initialize WinFsp's dispatcher TLS and operation table (v2.1 src/dll/fs.c).
      unsafe {
        let old = (*file_system).volume_handle;
        (*file_system).volume_handle = volume.into_raw_handle();
        (*file_system).volume_name = record.volume;
        drop(OwnedHandle::from_raw_handle(old));
      }
      mounted.drive = Some(Drive::inherited(&record, link));
      mounted.record = record;
    }
    // SAFETY: the DLL object now owns the final control handle (including substitution on adoption), and remains
    // live throughout the zero-length readiness transaction. This never takes a request from the kernel.
    let control = unsafe { (*file_system).volume_handle };
    crate::dispatcher::announce_ready(control, private_opens)?;
    if mounted.drive.is_none() {
      mounted.drive = Some(Drive::create(&mounted.record)?);
    }
    Ok(mounted)
  }

  /// Keeps duplicates alive while the shard waits for the anchor to acknowledge taking its own copies.
  fn offer(&self) -> Result<Offer, WinFspError> {
    // SAFETY: the DLL object owns this live control handle for the lifetime of `self`; the borrow cannot outlive it.
    let volume = unsafe { BorrowedHandle::borrow_raw((*self.file_system).volume_handle) }
      .try_clone_to_owned()
      .map_err(|error| WinFspError::Os {
        call: "duplicate volume control handle",
        code: error.raw_os_error(),
      })?;
    let link = self
      .drive
      .as_ref()
      .ok_or(WinFspError::MountThread)?
      .duplicate()?;
    Ok(Offer {
      volume,
      link,
      record: self.record.clone(),
    })
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

  /// Purges each file's changed range from Windows' cache and from every view mapped of it (`crate::purge`): opens the
  /// file through the drive unbuffered and writes the range, a piece at a time, from a zeroed buffer. WinFsp flushes and
  /// purges each piece's range with `CcCoherencyFlushAndPurgeCache` before the write reaches the host, which answers the
  /// write without applying it (its open carries the private restricting SID and expected file identity). Each piece is
  /// then read back through Windows' cache and around it, and a purge whose pages Windows kept comes back untaken, for the server to owe it again
  /// (`crate::retry`). A missing or foreign private token never authorizes a purge write.
  fn purge(&self, purges: &[Purge]) -> Result<Purged, WinFspError> {
    if purges.is_empty() {
      return Ok(Purged::default());
    }
    let piece = purge_piece();
    let piece_bytes = usize::try_from(piece).unwrap_or(0);
    let zeros = AlignedBytes::new(piece_bytes, usize::from(SECTOR_SIZE));
    let mut aligned = AlignedBytes::new(piece_bytes, usize::from(SECTOR_SIZE));
    let outcomes = purges
      .iter()
      .map(|purge| {
        self.purge_intents.publish(Some(purge.clone()))?;
        let outcome = self
          .purge_token
          .during(|| Ok(self.purge_one(purge, zeros.bytes(), &mut aligned, piece)));
        self.purge_intents.publish(None)?;
        outcome.map(|outcome| (purge.clone(), outcome))
      })
      .collect::<Result<_, _>>()?;
    Ok(Purged {
      outcomes,
      unavailable: 0,
    })
  }

  /// One file's purge (`crate::purge::purge_through`) through an unbuffered open of it, which writes the range and reads
  /// it around Windows' cache, and a cached open made for the read through the cache.
  fn purge_one(
    &self,
    purge: &Purge,
    zeros: &[u8],
    aligned: &mut AlignedBytes,
    piece: u32,
  ) -> PurgeOutcome {
    let path = wide_nul(&format!("\\\\?\\{}{}", self.mount_point, purge.path));
    let Some(unbuffered) = open_through_drive(
      &path,
      GENERIC_READ | GENERIC_WRITE,
      FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
    ) else {
      return PurgeOutcome::Refused;
    };
    let mut file = DriveFile {
      path: &path,
      unbuffered,
      cached: None,
      zeros,
      aligned,
    };
    purge_through(&mut file, purge, piece)
  }
}

/// Opens the file at `path` (a NUL-terminated UTF-16 path through the drive) with `access` and `flags`, sharing it
/// whole: its handle, or `None` when Windows refused the open.
fn open_through_drive(path: &[u16], access: u32, flags: u32) -> Option<OwnedHandle> {
  // SAFETY: `path` is a live NUL-terminated UTF-16 string for the call; no security attributes and no template.
  let raw = unsafe {
    CreateFileW(
      path.as_ptr(),
      access,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
      std::ptr::null(),
      OPEN_EXISTING,
      flags,
      std::ptr::null_mut(),
    )
  };
  if raw == INVALID_HANDLE_VALUE || raw.is_null() {
    return None;
  }
  // SAFETY: a handle the call above just opened and nothing else holds; owned from here and closed on drop.
  Some(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// A file a purge works on through the drive ([`PurgeTarget`]): its unbuffered open, which writes the purge and reads
/// around Windows' cache into a sector-aligned buffer, and a cached open for the read through the cache, made at the
/// first such read.
struct DriveFile<'p> {
  path: &'p [u16],
  unbuffered: OwnedHandle,
  cached: Option<OwnedHandle>,
  zeros: &'p [u8],
  aligned: &'p mut AlignedBytes,
}

impl PurgeTarget for DriveFile<'_> {
  fn write_zeros(&mut self, offset: u64, len: u32) -> bool {
    write_zeros_at(&self.unbuffered, self.zeros, offset, len)
  }

  fn read_cached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool {
    if self.cached.is_none() {
      self.cached = open_through_drive(self.path, GENERIC_READ, FILE_ATTRIBUTE_NORMAL);
    }
    let Some(handle) = self.cached.as_ref() else {
      return false;
    };
    out.clear();
    out.resize(usize::try_from(len).unwrap_or(0), 0);
    let Some(read) = read_at(handle, out, offset) else {
      return false;
    };
    out.truncate(read);
    true
  }

  fn read_uncached(&mut self, offset: u64, len: u32, out: &mut Vec<u8>) -> bool {
    let Some(buffer) = usize::try_from(len)
      .ok()
      .and_then(|len| self.aligned.bytes_mut().get_mut(..len))
    else {
      return false;
    };
    let Some(read) = read_at(&self.unbuffered, buffer, offset) else {
      return false;
    };
    out.clear();
    out.extend_from_slice(buffer.get(..read).unwrap_or_default());
    true
  }
}

/// Reads into `buffer` at `offset` through `handle`, synchronously: how many bytes were read, or `None` when Windows
/// refused the read.
fn read_at(handle: &OwnedHandle, buffer: &mut [u8], offset: u64) -> Option<usize> {
  let len = u32::try_from(buffer.len()).ok()?;
  let mut overlapped = OVERLAPPED::default();
  overlapped.Anonymous.Anonymous.Offset = u32::try_from(offset & u64::from(u32::MAX)).unwrap_or(0);
  overlapped.Anonymous.Anonymous.OffsetHigh = u32::try_from(offset >> u32::BITS).unwrap_or(0);
  let mut read = 0u32;
  // SAFETY: a live handle opened synchronously, a buffer of `len` writable bytes, and an OVERLAPPED on this frame that
  // only names the offset (a synchronous handle completes the read before the call returns).
  let done = unsafe {
    ReadFile(
      handle.as_raw_handle(),
      buffer.as_mut_ptr(),
      len,
      &mut read,
      &mut overlapped,
    )
  };
  if done == 0 {
    return None;
  }
  usize::try_from(read).ok()
}

/// Format: the bytes of a purge write's piece when the system names no allocation granularity: one sector.
const PURGE_PIECE_FLOOR: u32 = SECTOR_SIZE as u32;

/// The most bytes one purge write covers: the system's allocation granularity (64 KiB on every Windows machine; the unit
/// views are mapped in, `slates_machine::facts::Facts::allocation_granularity`) rounded down to whole sectors, at least
/// one sector. It bounds a purge's buffer whatever the change.
fn purge_piece() -> u32 {
  let sector = u32::from(SECTOR_SIZE);
  u32::try_from(slates_machine::facts::Facts::allocation_granularity())
    .ok()
    .and_then(|granularity| granularity.checked_div(sector))
    .and_then(|sectors| sectors.checked_mul(sector))
    .filter(|piece| *piece > 0)
    .unwrap_or(PURGE_PIECE_FLOOR)
}

/// A buffer, zeroed when made, whose first byte lies at a multiple of the alignment an unbuffered transfer asks (a
/// sector), found within a larger allocation, so no allocator call needs to be unsafe: a purge's zeros, and the bytes
/// an unbuffered read lands in.
struct AlignedBytes {
  storage: Vec<u8>,
  start: usize,
  len: usize,
}

impl AlignedBytes {
  /// `len` zeroed bytes aligned to `align`.
  fn new(len: usize, align: usize) -> AlignedBytes {
    let storage = vec![0u8; len.saturating_add(align)];
    let start = storage.as_ptr().align_offset(align).min(align);
    AlignedBytes {
      storage,
      start,
      len,
    }
  }

  /// The aligned bytes (empty if the allocation could not hold them).
  fn bytes(&self) -> &[u8] {
    self
      .storage
      .get(self.start..self.start.saturating_add(self.len))
      .unwrap_or(&[])
  }

  /// The aligned bytes, to write into (empty if the allocation could not hold them).
  fn bytes_mut(&mut self) -> &mut [u8] {
    let end = self.start.saturating_add(self.len);
    self.storage.get_mut(self.start..end).unwrap_or_default()
  }
}

/// Writes `len` bytes of `zeros` at `offset` through `handle`, synchronously: whether the whole length was written.
fn write_zeros_at(handle: &OwnedHandle, zeros: &[u8], offset: u64, len: u32) -> bool {
  let Some(buffer) = usize::try_from(len).ok().and_then(|len| zeros.get(..len)) else {
    return false;
  };
  let mut overlapped = OVERLAPPED::default();
  overlapped.Anonymous.Anonymous.Offset = u32::try_from(offset & u64::from(u32::MAX)).unwrap_or(0);
  overlapped.Anonymous.Anonymous.OffsetHigh = u32::try_from(offset >> u32::BITS).unwrap_or(0);
  let mut written = 0u32;
  // SAFETY: a live handle opened synchronously, a buffer of `len` bytes, and an OVERLAPPED on this frame that only
  // names the offset (a synchronous handle completes the write before the call returns).
  let done = unsafe {
    WriteFile(
      handle.as_raw_handle(),
      buffer.as_ptr(),
      len,
      &mut written,
      &mut overlapped,
    )
  };
  done != 0 && written == len
}

impl Drop for Mounted {
  fn drop(&mut self) {
    if self.file_system.is_null() {
      return;
    }
    // SAFETY: `file_system` is live; stop the dispatcher (its threads joined, so no callback runs after), unmount, and
    // delete it. A dispatcher thread waiting for the shard's reply returns once the channel's receiver is dropped,
    // which the mount's owner does before it stops the thread.
    if let Some(mut worker) = self.worker.take()
      && let Err(error) = worker.join()
    {
      eprintln!("slates-winfsp: join receive worker: {error}");
    }
    // SAFETY: the only receive/operation worker has been joined, so no callback can reach the DLL object again.
    unsafe { (self.api.delete)(self.file_system) };
    // No callback can run now, so reclaim the handler.
    // SAFETY: the handler was leaked in `new` and the dispatcher is stopped, so this is the sole taker.
    drop(unsafe { Box::from_raw(self.handler) });
  }
}

/// A command to the mount thread.
enum Command {
  /// Hand Windows this round of notification records, then purge these files' changed ranges (`crate::purge`).
  Notify { round: Vec<u8>, purges: Vec<Purge> },
  /// Purge these ranges again, with no round: purges Windows did not take, now due again (`crate::retry`).
  Purge { purges: Vec<Purge> },
}

/// What a command's purges came to (`Mounted::purge`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
  /// Each purge made, in order, with what it came to: taken (Windows dropped the range's pages), untaken (a read through
  /// Windows' cache served bytes the host no longer holds, so Windows kept a page and the purge is owed again,
  /// `crate::retry`), or refused (its open, a write or a read back, as for a file renamed or removed since).
  pub outcomes: Vec<(Purge, PurgeOutcome)>,
  /// Purges not made because WinFsp cannot name an open's process, so the host could not tell a purge's write from a
  /// program's (`FspFileSystemOperationProcessIdF` absent).
  pub unavailable: u64,
}

/// The mounted volume's durable open ledger and the temporary duplicates used for its acknowledged anchor hold.
pub struct Ready {
  /// Owned on the volume's shard after creation or recovery off the shard on the mount thread.
  pub ledger: Ledger<SparseObject>,
  /// Recovered contexts gathered off the shard, to seed names in cooperative turns.
  pub opens: std::collections::VecDeque<(u64, Open, String)>,
  /// The held contexts by file identity, computed off the shard. Recovery reconciles one file's reference count per turn.
  pub open_counts: std::collections::BTreeMap<u64, (u64, u32)>,
  /// The volume and link duplicates, kept until the anchor owns its copies.
  pub offer: Offer,
}

impl Ready {
  /// The three source values in this process, whose objects remain owned until the hold acknowledgement arrives.
  pub fn handle_values(&self) -> Result<Vec<u64>, WinFspError> {
    let volume = u64::try_from(self.offer.volume.as_raw_handle().addr())
      .map_err(|_| WinFspError::MountThread)?;
    let link = u64::try_from(self.offer.link.as_raw_handle().addr())
      .map_err(|_| WinFspError::MountThread)?;
    let ledger = self
      .ledger
      .memory()
      .handle_value()
      .map_err(|error| WinFspError::Ledger(crate::opens::Refusal::Memory(error)))?;
    Ok(vec![volume, link, ledger])
  }
}

/// What the mount thread reports, each followed by a wake of the mount's task.
pub enum Outcome {
  /// The mount is up, or why it is not.
  Mounted(Result<Box<Ready>, WinFspError>),
  /// A notification round's outcome, and what its purges came to (none when Windows did not take the round, which is
  /// gathered again).
  Notified(Result<Notified, WinFspError>, Purged),
  /// What a purge-only command's purges came to.
  Purged(Purged),
  /// The receive worker ended before the mount was intentionally stopped.
  Failed(WinFspError),
  /// The receive journal was settled before any fresh receive. The owner counts it; the taken-read oracle requires it.
  Recovered(u64),
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
    open_bound: usize,
    inherited: Option<Inherited>,
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
          open_bound,
          inherited,
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

  /// Sends the thread a round of notification records and the purges it owes; its [`Outcome::Notified`] follows.
  /// Refused when the thread is gone or a round is already out (the caller sends one at a time).
  pub fn notify(&self, round: Vec<u8>, purges: Vec<Purge>) -> Result<(), WinFspError> {
    let commands = self.commands.as_ref().ok_or(WinFspError::MountThread)?;
    commands
      .try_send(Command::Notify { round, purges })
      .map_err(|_| WinFspError::MountThread)
  }

  /// Sends the thread purges owed again, with no round; its [`Outcome::Purged`] follows. Refused when the thread is gone
  /// or a command is already out (the caller sends one, a round or a purge, at a time).
  pub fn purge(&self, purges: Vec<Purge>) -> Result<(), WinFspError> {
    let commands = self.commands.as_ref().ok_or(WinFspError::MountThread)?;
    commands
      .try_send(Command::Purge { purges })
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
  open_bound: usize,
  inherited: Option<Inherited>,
) {
  let report = |outcome: Outcome| {
    if outcomes.send(outcome).is_err() {
      eprintln!("slates-winfsp: the mount's outcome receiver closed before its owner joined");
    }
    wake.wake_by_ref();
  };
  let confirmations = requests.clone();
  // Shape: the preceding purge's End and the next purge's Begin can be queued before a fresh kernel Create arrives.
  let (purge_intents, mut private_opens) = match crate::purge_intents::channel() {
    Ok(channel) => channel,
    Err(error) => {
      report(Outcome::Mounted(Err(error)));
      report(Outcome::Stopped);
      return;
    }
  };
  let prepared = (|| {
    let len = crate::opens::bytes(open_bound).map_err(WinFspError::Ledger)?;
    let (mut ledger, inherited) = match inherited {
      Some(Inherited {
        volume,
        link,
        ledger,
        record,
      }) => {
        let memory = SparseObject::from_held_handle(ledger, len, slates_mem::Words::new())
          .map_err(|error| WinFspError::Ledger(crate::opens::Refusal::Memory(error)))?;
        (
          Ledger::recover(memory, open_bound).map_err(WinFspError::Ledger)?,
          Some((record, volume, link)),
        )
      }
      None => {
        let memory = SparseObject::unnamed(len, slates_mem::Words::new())
          .map_err(|error| WinFspError::Ledger(crate::opens::Refusal::Memory(error)))?;
        (
          Ledger::create(memory, open_bound).map_err(WinFspError::Ledger)?,
          None,
        )
      }
    };
    let mut captured = vec![0; crate::inflight::capture::BYTES];
    let at = crate::opens::capture_offset(open_bound).map_err(WinFspError::Ledger)?;
    ledger
      .memory()
      .read(at, &mut captured)
      .map_err(|error| WinFspError::Ledger(crate::opens::Refusal::Memory(error)))?;
    let saved_open = crate::inflight::capture::Capture::new(&mut captured)
      .map_err(WinFspError::Capture)?
      .resume()
      .map_err(WinFspError::Capture)?
      .map(|batch| batch.saved_open())
      .transpose()
      .map_err(WinFspError::Capture)?
      .flatten();
    ledger
      .settle_pending(saved_open)
      .map_err(WinFspError::Ledger)?;
    let mounted = Mounted::new(
      mount_point,
      read_only,
      requests,
      wake.clone(),
      inherited,
      purge_intents,
      &mut private_opens,
    )?;
    let value = ledger
      .memory()
      .handle_value()
      .map_err(|error| WinFspError::Ledger(crate::opens::Refusal::Memory(error)))?;
    let raw = usize::try_from(value).map_err(|_| WinFspError::MountThread)? as *mut c_void;
    // SAFETY: the ledger owns this live section, and the borrowed handle lasts only until duplication returns.
    let section = unsafe { BorrowedHandle::borrow_raw(raw) }
      .try_clone_to_owned()
      .map_err(|error| WinFspError::Os {
        call: "duplicate receive section",
        code: error.raw_os_error(),
      })?;
    let offer = mounted.offer()?;
    let opens = ledger
      .live()
      .map(|(context, open)| ledger.name(context).map(|name| (context, open, name)))
      .collect::<Result<_, _>>()
      .map_err(WinFspError::Ledger)?;
    let mut open_counts = std::collections::BTreeMap::<u64, (u64, u32)>::new();
    for (_, open) in ledger.live() {
      let entry = open_counts
        .entry(open.object.inode)
        .or_insert((open.object.generation, 0));
      if entry.0 != open.object.generation {
        return Err(WinFspError::Ledger(crate::opens::Refusal::Corrupt));
      }
      entry.1 = entry
        .1
        .checked_add(1)
        .ok_or(WinFspError::Ledger(crate::opens::Refusal::Bound))?;
    }
    Ok::<_, WinFspError>((
      mounted,
      Ready {
        ledger,
        offer,
        opens,
        open_counts,
      },
      section,
    ))
  })();
  let mounted = match prepared {
    Ok((mut mounted, ready, section)) => {
      report(Outcome::Mounted(Ok(Box::new(ready))));
      match crate::dispatcher::Worker::start(
        mounted.file_system.addr(),
        section,
        open_bound,
        outcomes.clone(),
        wake.clone(),
        confirmations,
        crate::dispatcher::PrivateOpens::new(mounted.purge_token.marker(), private_opens),
      ) {
        Ok(worker) => mounted.worker = Some(worker),
        Err(error) => {
          report(Outcome::Failed(error));
          drop(mounted);
          report(Outcome::Stopped);
          return;
        }
      }
      mounted
    }
    Err(error) => {
      report(Outcome::Mounted(Err(error)));
      report(Outcome::Stopped);
      return;
    }
  };
  while let Ok(command) = commands.recv() {
    match command {
      Command::Notify { round, purges } => {
        let told = mounted.notify(&round);
        let purged = if matches!(told, Ok(Notified::Taken)) {
          match mounted.purge(&purges) {
            Ok(purged) => purged,
            Err(error) => {
              report(Outcome::Failed(error));
              break;
            }
          }
        } else {
          Purged::default()
        };
        report(Outcome::Notified(told, purged));
      }
      Command::Purge { purges } => match mounted.purge(&purges) {
        Ok(purged) => report(Outcome::Purged(purged)),
        Err(error) => {
          report(Outcome::Failed(error));
          break;
        }
      },
    }
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

#[cfg(test)]
#[path = "../tests/support/host.rs"]
mod tests;
