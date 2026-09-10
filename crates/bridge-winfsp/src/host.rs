//! The WinFsp mount host (§4.6, D-2): builds a live user-mode filesystem over the shared [`Bridge`]
//! and mounts it at a drive letter or directory, so a Windows program sees a slates volume as a normal
//! path — the Windows analogue of the NFS `Export`/daemon and the FSKit `MountSession`.
//!
//! **Threading.** A [`Volume`] is `!Send` (it holds a `Box<dyn Clock>`), so it can never cross a
//! thread. WinFsp, in turn, drives its `FSP_FILE_SYSTEM_INTERFACE` callbacks from a pool of dispatcher
//! threads. slates bridges the two with its own idiom (D-7, "sharing is a move over a bounded
//! channel"): a single **owner thread** *constructs* the volume locally (via a `Send` builder, so the
//! `!Send` value is born on that thread and never leaves it) and serves one operation at a time; each
//! WinFsp callback packages its request as an [`Op`], sends it over a bounded channel, and blocks for
//! the [`Reply`]. No `Mutex`, no `Arc` (R2) — one consumer of the volume, replies over per-call oneshot
//! channels. WinFsp's COARSE operation guard serializes the callbacks on top of this, so the handler
//! pointer stored in the file system's `UserContext` is never touched concurrently.
//!
//! **Verification.** Every struct and export is hand-transcribed from winfsp's headers ([`crate::ffi`])
//! and cross-lints on the Windows target from any host; the live mount — provision a volume, mount a
//! drive letter, create/write/read/list/delete through the Windows kernel, unmount — runs on the native
//! Windows CI runner (`WINFSP_TEST_MOUNT=1`), the way the FSKit handler's live mount runs on macOS.

use std::ffi::c_void;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

use slates_bridge_core::{
  Attachments, Bridge, NodeAttr, ObjectId, OpContext, RenameFlags, Rights, SetAttr, View,
  VolumeBridge, new_handle_store,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::Slab;
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;
use slates_vfs::volume::{Store, Volume};

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::GetSecurityDescriptorLength;

use crate::ffi::{
  self, Boolean, DirInfo, FileInfo, FileSystem, GUARD_STRATEGY_COARSE, Interface, Ntstatus,
  PSecurityDescriptor, Pvoid, Pwstr, VolumeInfo, VolumeParams,
};
use crate::{STATUS_SUCCESS, file_attributes, filetime_from_unix_ns, ntstatus};

/// A typed WinFsp host failure (R1: no panics; every refusal is a value).
#[derive(Debug)]
pub enum WinFspError {
  /// A `FspFileSystem*` call returned a failure `NTSTATUS`.
  Api {
    /// The call that failed.
    call: &'static str,
    /// Its `NTSTATUS`.
    status: i32,
  },
  /// The reference security descriptor could not be built (`ConvertStringSecurityDescriptor…`).
  SecurityDescriptor,
  /// The owner thread could not be started.
  OwnerThread,
}

/// Format: the sector size the mount reports — 4096 bytes, the volume core's chunk/page unit, so an
/// allocation unit is one page.
const SECTOR_SIZE: u16 = 4096;
/// Format: one sector per allocation unit — the allocation unit equals the sector (the page), matching
/// the volume core's page-granular accounting.
const SECTORS_PER_UNIT: u16 = 1;
/// Format: the file-info cache timeout WinFsp keeps attributes for (milliseconds). One second matches
/// the NFS mount's `actimeo=1`: an overlay changes under merges and outside edits, so the cache is
/// short but nonzero (the loopback path revalidates in well under a millisecond).
const FILE_INFO_TIMEOUT_MS: u32 = 1000;
/// Format: the dispatcher thread count. WinFsp uses a small pool regardless; the COARSE guard
/// serializes them onto the one owner thread, so the count only bounds concurrent kernel requests.
const DISPATCHER_THREADS: u32 = 0;
/// Shape: the bound on in-flight operations queued to the owner thread. At least the dispatcher's
/// concurrency, so no WinFsp thread blocks placing its one in-flight op; a small constant since each
/// thread has at most one outstanding request and the owner drains them one at a time.
const JOB_QUEUE_BOUND: usize = 64;
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
/// valid descriptor for its access checks; a RAM scratch volume grants freely (the harness owns
/// isolation, §4.13), so one fixed descriptor serves every object, exactly as WinFsp's `memfs` sample
/// starts from a single SDDL string.
const EVERYONE_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;WD)";

/// The per-open state WinFsp carries as an opaque `FileContext`: the object's identity and its bridge
/// open handle. Boxed and leaked to a raw pointer on open/create, reclaimed on close — created and
/// destroyed only inside COARSE-serialized callbacks, so never touched concurrently.
struct OpenFile {
  object: ObjectId,
  fh: u64,
  is_dir: bool,
}

/// One operation the owner thread runs on the volume, with the parameters marshalled from a WinFsp
/// callback. Paths arrive as Rust `String`s (converted from WinFsp's UTF-16), so the whole `Op` is
/// `Send` and crosses the channel to the owner thread that holds the `!Send` volume.
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
  /// List a directory's entries (name, attributes) — the owner reads them all; the trampoline handles
  /// the WinFsp marker and buffer fill.
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
  /// Delete the object named by a path (`Cleanup` with the delete flag): a file (`unlink`) or an empty
  /// directory (`rmdir`), decided by the resolved kind.
  Delete {
    path: String,
  },
  Release {
    object: ObjectId,
    fh: u64,
    is_dir: bool,
  },
}

/// The owner thread's reply to an [`Op`].
enum Reply {
  Volume {
    total: u64,
    free: u64,
  },
  /// An object and its attributes (resolve/open/create/info/setattr paths).
  Object {
    object: ObjectId,
    info: FileInfo,
    is_dir: bool,
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

/// A job on the owner-thread channel: the operation and the oneshot channel its reply returns on.
struct Job {
  op: Op,
  reply: SyncSender<Reply>,
}

/// The volume state the owner thread holds for a mount's whole life: the volume and its store, a
/// persistent handle table (so an open handle survives across the read/write calls that share a
/// `FileContext`), and the attachment the operations run under. Built on the owner thread (so the
/// `!Send` volume never crosses a thread) by [`VolumeHost::new`].
pub struct VolumeHost {
  volume: Volume,
  store: Store,
  handles: Slab<u64>,
  attachments: Attachments,
  attachment: slates_bridge_core::AttachmentId,
  volume_id: VolumeId,
}

impl VolumeHost {
  /// The host for `volume`/`store` (volume id `volume_id`), served under `subject` with `rights`. Panics
  /// only on an impossible attachment-registry overflow at construction (a fresh registry always admits
  /// one); everything after is a typed refusal. Call this inside the `build` closure of [`mount`], on
  /// the owner thread.
  pub fn new(
    volume_id: VolumeId,
    volume: Volume,
    store: Store,
    subject: Principal,
    rights: Rights,
  ) -> Result<VolumeHost, WinFspError> {
    let mut attachments = Attachments::new();
    let attachment = attachments
      .attach(volume_id, View::Current, subject, rights)
      .map_err(|_| WinFspError::OwnerThread)?;
    Ok(VolumeHost {
      volume,
      store,
      handles: new_handle_store(),
      attachments,
      attachment,
      volume_id,
    })
  }

  /// A transient bridge over the volume for one operation (the "marshal each operation into the owning
  /// shard's bridge queue" shape), with the persistent handle table lent in so open handles survive.
  fn bridge(&mut self) -> Result<(VolumeBridge<'_>, OpContext), VfsError> {
    let cx = self.attachments.context(self.attachment)?;
    let bridge = VolumeBridge::attached(
      self.volume_id,
      &mut self.volume,
      &mut self.store,
      &mut self.handles,
      None,
    );
    Ok((bridge, cx))
  }
}

/// Builds a [`FileInfo`] from the volume core's neutral attributes: the Windows attribute bits for the
/// kind, the size for both file and allocation size (page-granular), and the times as `FILETIME`.
fn file_info(attr: &NodeAttr) -> FileInfo {
  FileInfo {
    file_attributes: file_attributes(attr.kind),
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
  }
}

/// Splits a WinFsp path (`\` or `\dir\file`, backslash-separated) into its non-empty components. The
/// root is the empty slice.
fn components(path: &str) -> Vec<&str> {
  path.split('\\').filter(|c| !c.is_empty()).collect()
}

/// Resolves a WinFsp path to its object id and attributes by walking `lookup` from the root. The root
/// (`\`) is the volume root itself.
fn resolve(
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  path: &str,
) -> Result<NodeAttr, VfsError> {
  let root = bridge.root(cx)?;
  let mut current = bridge.getattr(ObjectId::new(root, 0), cx)?;
  for name in components(path) {
    current = bridge.lookup(ObjectId::new(current.ino, current.generation), cx, name)?;
  }
  Ok(current)
}

/// Runs one [`Op`] on the owner thread's volume and returns its [`Reply`]. A `VfsError` becomes an
/// `Err(NTSTATUS)` through the shared refusal taxonomy ([`ntstatus`]).
fn serve(host: &mut VolumeHost, op: Op) -> Reply {
  let (mut bridge, cx) = match host.bridge() {
    Ok(pair) => pair,
    Err(e) => return Reply::Err(ntstatus(&e).as_i32()),
  };
  let result = run(&mut bridge, &cx, op);
  result.unwrap_or_else(|e| Reply::Err(ntstatus(&e).as_i32()))
}

/// The operation body, returning `Result` so the `?` operator threads refusals; [`serve`] maps them.
fn run(bridge: &mut VolumeBridge<'_>, cx: &OpContext, op: Op) -> Result<Reply, VfsError> {
  Ok(match op {
    Op::VolumeInfo => {
      let root = bridge.root(cx)?;
      let stat = bridge.statfs(ObjectId::new(root, 0), cx)?;
      let total = stat.blocks.saturating_mul(u64::from(stat.bsize));
      let free = stat.bfree.saturating_mul(u64::from(stat.bsize));
      Reply::Volume { total, free }
    }
    Op::Resolve { path } => match resolve(bridge, cx, &path) {
      Ok(attr) => Reply::Object {
        object: ObjectId::new(attr.ino, attr.generation),
        info: file_info(&attr),
        is_dir: matches!(attr.kind, Kind::Dir),
        fh: 0,
      },
      Err(e) => Reply::Err(ntstatus(&e).as_i32()),
    },
    Op::Open { path } => {
      let attr = resolve(bridge, cx, &path)?;
      let object = ObjectId::new(attr.ino, attr.generation);
      let is_dir = matches!(attr.kind, Kind::Dir);
      let fh = if is_dir {
        bridge.opendir(object, cx)?
      } else {
        bridge.open(object, cx, 0)?
      };
      Reply::Object {
        object,
        info: file_info(&attr),
        is_dir,
        fh,
      }
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
        info: file_info(&attr),
      }
    }
    Op::Info { object } => {
      let attr = bridge.getattr(object, cx)?;
      Reply::Object {
        object,
        info: file_info(&attr),
        is_dir: matches!(attr.kind, Kind::Dir),
        fh: 0,
      }
    }
    Op::ReadDir { object, fh } => {
      let entries = read_all_entries(bridge, cx, object, fh)?;
      Reply::Entries { entries }
    }
    Op::SetSize { object, new_size } => {
      let attr = bridge.setattr(
        object,
        cx,
        SetAttr {
          size: Some(new_size),
          ..SetAttr::default()
        },
      )?;
      Reply::Object {
        object,
        info: file_info(&attr),
        is_dir: false,
        fh: 0,
      }
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
      Reply::Object {
        object,
        info: file_info(&attr),
        is_dir: false,
        fh: 0,
      }
    }
    Op::Rename { from, to } => op_rename(bridge, cx, &from, &to)?,
    Op::Delete { path } => op_delete(bridge, cx, &path)?,
    Op::Release { object, fh, is_dir } => {
      let _ = is_dir;
      let _ = bridge.release(object, cx, fh);
      Reply::Ok
    }
  })
}

/// Splits a WinFsp path into its final component and the resolved parent object (the directory the new
/// or renamed name lives in). A path with no component (`\`) has no parent, reported as
/// `STATUS_INVALID_PARAMETER`.
fn split_parent<'p>(
  bridge: &mut VolumeBridge<'_>,
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
  bridge: &mut VolumeBridge<'_>,
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
    let object = ObjectId::new(attr.ino, attr.generation);
    let fh = bridge.opendir(object, cx)?;
    Ok(Reply::Object {
      object,
      info: file_info(&attr),
      is_dir: true,
      fh,
    })
  } else {
    let (attr, fh) = bridge.create(parent_id, cx, name, DEFAULT_FILE_MODE, 0)?;
    Ok(Reply::Object {
      object: ObjectId::new(attr.ino, attr.generation),
      info: file_info(&attr),
      is_dir: false,
      fh,
    })
  }
}

/// Renames `from` to `to` by resolving each path's parent and final name.
fn op_rename(
  bridge: &mut VolumeBridge<'_>,
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
fn op_delete(bridge: &mut VolumeBridge<'_>, cx: &OpContext, path: &str) -> Result<Reply, VfsError> {
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
/// returns whole from offset 0), each resolved to its attributes by id — the owner-side of
/// `ReadDirectory`; the trampoline applies the WinFsp marker and buffer fill. An entry that vanished
/// between the listing and its attribute lookup is reported with the entry's kind and a zero size
/// rather than dropped, so the listing never desyncs.
fn read_all_entries(
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  object: ObjectId,
  fh: u64,
) -> Result<Vec<(String, FileInfo)>, VfsError> {
  let rows = bridge.readdir(object, cx, fh, 0)?;
  let mut out = Vec::with_capacity(rows.len());
  for entry in rows {
    let info = match bridge.getattr(ObjectId::new(entry.ino, 0), cx) {
      Ok(attr) => file_info(&attr),
      Err(_) => FileInfo {
        file_attributes: file_attributes(entry.kind),
        ..FileInfo::default()
      },
    };
    out.push((entry.name, info));
  }
  Ok(out)
}

// --------------------------------------------------------------------- the WinFsp callback handler

/// The handler stored in the file system's `UserContext`: the channel to the owner thread and the one
/// reference security descriptor every object reports. `Send + Sync` (a `SyncSender` and a byte vector),
/// so WinFsp's dispatcher threads reach it soundly; the COARSE guard serializes them regardless.
struct Handler {
  jobs: SyncSender<Job>,
  /// A self-relative security descriptor as raw bytes (built once from [`EVERYONE_SDDL`]).
  security_descriptor: Vec<u8>,
}

impl Handler {
  /// Sends `op` to the owner thread and blocks for its reply; a dead owner thread (its receiver gone)
  /// is `STATUS_UNSUCCESSFUL`, never a hang.
  fn call(&self, op: Op) -> Reply {
    let (tx, rx) = sync_channel(1);
    if self.jobs.send(Job { op, reply: tx }).is_err() {
      return Reply::Err(crate::status_unsuccessful());
    }
    rx.recv()
      .unwrap_or(Reply::Err(crate::status_unsuccessful()))
  }
}

/// The handler behind a file system pointer, from its `UserContext`.
///
/// # Safety
/// `fs` must be a live `FileSystem` whose `UserContext` holds a `Handler` pointer set by [`mount`] and
/// not yet reclaimed. The COARSE guard serializes callbacks, so no concurrent access occurs.
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
  // SAFETY: `wide` is a NUL-terminated UTF-16 string per the contract; we scan to the NUL.
  unsafe {
    while *wide.add(len) != 0 {
      len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(wide, len))
  }
}

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
  is_dir: bool,
  info: FileInfo,
  p_context: *mut Pvoid,
  p_info: *mut FileInfo,
) -> Ntstatus {
  let boxed = Box::into_raw(Box::new(OpenFile { object, fh, is_dir }));
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
    Reply::Object {
      object,
      info,
      is_dir,
      fh,
    } => finish_open(object, fh, is_dir, info, p_context, p_info),
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
    Reply::Object {
      object,
      info,
      is_dir,
      fh,
    } => finish_open(object, fh, is_dir, info, p_context, p_info),
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

extern "C" fn cleanup(fs: *mut FileSystem, context: Pvoid, file_name: Pwstr, flags: u32) {
  if flags & ffi::FSP_CLEANUP_DELETE == 0 {
    return;
  }
  // SAFETY: our file system; `file_name` the path to delete (sent only on a delete cleanup).
  let handler = unsafe { handler(fs) };
  let _ = context;
  // SAFETY: `file_name` is a NUL-terminated UTF-16 path WinFsp owns for this call.
  let path = unsafe { wide_to_string(file_name) };
  let _ = handler.call(Op::Delete { path });
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
  let _ = handler.call(Op::Release {
    object: open.object,
    fh: open.fh,
    is_dir: open.is_dir,
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
      let n = bytes.len().min(length as usize);
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
  // SAFETY: `buffer` is a WinFsp buffer of `length` bytes for this call.
  let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), length as usize) }.to_vec();
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

extern "C" fn flush(_fs: *mut FileSystem, _context: Pvoid, p_info: *mut FileInfo) -> Ntstatus {
  // A slates write lands synchronously (durable before its reply), so a flush is a no-op success. The
  // info out-parameter is left as WinFsp initialized it (a volume flush passes a null context).
  let _ = p_info;
  STATUS_SUCCESS.as_i32()
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
  _fs: *mut FileSystem,
  context: Pvoid,
  _file_attributes: u32,
  _creation_time: u64,
  _last_access_time: u64,
  _last_write_time: u64,
  _change_time: u64,
  p_info: *mut FileInfo,
) -> Ntstatus {
  // Times/attributes are accepted and reported from the current object (a RAM scratch mount does not
  // persist Windows attribute bits); the object's live info is returned so the caller sees consistency.
  // SAFETY: our file system and a live open-file context.
  let handler = unsafe { handler(_fs) };
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

extern "C" fn can_delete(_fs: *mut FileSystem, context: Pvoid, _file_name: Pwstr) -> Ntstatus {
  // The delete itself happens in Cleanup; here we only confirm deletability. An empty-directory check
  // is enforced by the volume core's `rmdir` at delete time (NotEmpty → the mapped NTSTATUS), so this
  // reports success and lets the real check run then.
  let _ = context;
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
  let mut past_marker = marker.is_empty();
  for (name, info) in entries {
    if !past_marker {
      if name == marker {
        past_marker = true;
      }
      continue;
    }
    let name_utf16: Vec<u16> = name.encode_utf16().collect();
    // FSP_FSCTL_DIR_INFO: the 104-byte head then the name bytes, `Size` covering both.
    let head = std::mem::size_of::<DirInfo>();
    let size = head + name_utf16.len() * std::mem::size_of::<u16>();
    let mut scratch = vec![0u8; size];
    // SAFETY: `scratch` is `size` bytes, at least the DirInfo head; we write the head then the name.
    unsafe {
      let dir_info = scratch.as_mut_ptr().cast::<DirInfo>();
      (*dir_info).size = u16::try_from(size).unwrap_or(u16::MAX);
      (*dir_info).file_info = *info;
      (*dir_info).padding = [0u8; 24];
      std::ptr::copy_nonoverlapping(
        name_utf16.as_ptr().cast::<u8>(),
        scratch.as_mut_ptr().add(head),
        name_utf16.len() * std::mem::size_of::<u16>(),
      );
      if ffi::FspFileSystemAddDirInfo(
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
    ffi::FspFileSystemAddDirInfo(std::ptr::null_mut(), buffer, length, p_bytes);
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

/// A live WinFsp mount: the file system object, the owner thread serving its volume, and the leaked
/// handler. Unmounts, stops the dispatcher, deletes the file system, and joins the owner thread on drop.
pub struct Mount {
  file_system: *mut FileSystem,
  handler: *mut Handler,
  owner: Option<JoinHandle<()>>,
}

/// Builds the volume (on the owner thread, via `build`, so the `!Send` volume never crosses a
/// thread), then mounts it at `mount_point` (a drive letter like `Z:` or a directory). The owner
/// thread serves callbacks until the [`Mount`] is dropped.
pub fn mount<F>(build: F, mount_point: &str) -> Result<Mount, WinFspError>
where
  F: FnOnce() -> Result<VolumeHost, WinFspError> + Send + 'static,
{
  let security_descriptor = everyone_security_descriptor()?;
  let (jobs, job_rx) = sync_channel::<Job>(JOB_QUEUE_BOUND);
  // The owner thread: build the volume locally, then serve one op at a time until the channel closes.
  let owner = std::thread::Builder::new()
    .name("slates-winfsp".to_owned())
    .spawn(move || owner_loop(build, job_rx))
    .map_err(|_| WinFspError::OwnerThread)?;
  let handler = Box::into_raw(Box::new(Handler {
    jobs,
    security_descriptor,
  }));

  // SAFETY: an all-zero VolumeParams is a valid, empty parameter block; the fields below fill it.
  let mut params: VolumeParams = unsafe { std::mem::zeroed() };
  params.version = 0;
  params.sector_size = SECTOR_SIZE;
  params.sectors_per_allocation_unit = SECTORS_PER_UNIT;
  params.max_component_length = MAX_COMPONENT_LENGTH;
  params.file_info_timeout = FILE_INFO_TIMEOUT_MS;
  params.flags = ffi::VOLUME_FLAG_CASE_SENSITIVE_SEARCH
    | ffi::VOLUME_FLAG_CASE_PRESERVED_NAMES
    | ffi::VOLUME_FLAG_UNICODE_ON_DISK
    | ffi::VOLUME_FLAG_PERSISTENT_ACLS;
  write_wide(&mut params.file_system_name, "slates");

  let mut file_system: *mut FileSystem = std::ptr::null_mut();
  let mut device = wide_nul("WinFsp.Disk");
  // SAFETY: `device`/`params`/`INTERFACE` are live for the call; `file_system` is a writable out-ptr.
  let status =
    unsafe { ffi::FspFileSystemCreate(device.as_mut_ptr(), &params, &INTERFACE, &mut file_system) };
  if status != 0 {
    // Nothing was mounted; reclaim the handler and let the owner thread end (its sender drops).
    // SAFETY: `handler` was just leaked and no callback has run.
    drop(unsafe { Box::from_raw(handler) });
    let _ = owner.join();
    return Err(WinFspError::Api {
      call: "FspFileSystemCreate",
      status,
    });
  }
  // SAFETY: `file_system` is the object just created; store the handler and serialize callbacks.
  unsafe {
    (*file_system).user_context = handler.cast::<c_void>();
    ffi::FspFileSystemSetOperationGuardStrategyF(file_system, GUARD_STRATEGY_COARSE);
  }
  let mut mount_wide = wide_nul(mount_point);
  // SAFETY: `file_system` is live; `mount_wide` a NUL-terminated mount point.
  let status = unsafe { ffi::FspFileSystemSetMountPoint(file_system, mount_wide.as_mut_ptr()) };
  if status != 0 {
    return Err(teardown_partial(
      file_system,
      handler,
      owner,
      "FspFileSystemSetMountPoint",
      status,
    ));
  }
  // SAFETY: `file_system` is live and mounted; start its dispatcher threads.
  let status = unsafe { ffi::FspFileSystemStartDispatcher(file_system, DISPATCHER_THREADS) };
  if status != 0 {
    // SAFETY: remove the mount point set above before tearing down.
    unsafe { ffi::FspFileSystemRemoveMountPoint(file_system) };
    return Err(teardown_partial(
      file_system,
      handler,
      owner,
      "FspFileSystemStartDispatcher",
      status,
    ));
  }
  Ok(Mount {
    file_system,
    handler,
    owner: Some(owner),
  })
}

/// Tears a partially-built mount down after a failed step: delete the file system, reclaim the handler
/// (closing the owner channel), join the owner thread, and return the typed error.
fn teardown_partial(
  file_system: *mut FileSystem,
  handler: *mut Handler,
  owner: JoinHandle<()>,
  call: &'static str,
  status: i32,
) -> WinFspError {
  // SAFETY: `file_system` is a live object never dispatched; `handler` a leaked box taken once.
  unsafe {
    ffi::FspFileSystemDelete(file_system);
    drop(Box::from_raw(handler));
  }
  let _ = owner.join();
  WinFspError::Api { call, status }
}

impl Drop for Mount {
  fn drop(&mut self) {
    // SAFETY: `file_system` is live; stop the dispatcher (no more callbacks), unmount, and delete it.
    unsafe {
      ffi::FspFileSystemStopDispatcher(self.file_system);
      ffi::FspFileSystemRemoveMountPoint(self.file_system);
      ffi::FspFileSystemDelete(self.file_system);
    }
    // No callback can run now, so reclaim the handler; dropping its sender closes the owner channel.
    // SAFETY: the handler was leaked in `mount` and the dispatcher is stopped, so this is the sole taker.
    drop(unsafe { Box::from_raw(self.handler) });
    if let Some(owner) = self.owner.take() {
      let _ = owner.join();
    }
  }
}

/// The owner thread: build the volume locally, then serve each job on it until the channel closes (the
/// handler dropped at unmount).
fn owner_loop<F>(build: F, jobs: Receiver<Job>)
where
  F: FnOnce() -> Result<VolumeHost, WinFspError>,
{
  let mut host = match build() {
    Ok(host) => host,
    Err(_) => {
      // The volume could not be built: drain jobs with a failure so no caller hangs.
      while let Ok(job) = jobs.recv() {
        let _ = job.reply.send(Reply::Err(crate::status_unsuccessful()));
      }
      return;
    }
  };
  while let Ok(job) = jobs.recv() {
    let reply = serve(&mut host, job.op);
    let _ = job.reply.send(reply);
  }
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
    let len = GetSecurityDescriptorLength(sd) as usize;
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

// The `Mount` owns the raw file-system and handler pointers exclusively for its lifetime and tears them
// down on drop; the owner thread's channel endpoints carry the shared state. It is safe to move a
// `Mount` across threads (the pointers are only used on drop), so it is `Send` — but the raw pointers
// make it `!Send` by default, and there is no need to share it, so no override is added (R2/D-8: no
// `unsafe impl Send`). The mount is used from the thread that created it.
