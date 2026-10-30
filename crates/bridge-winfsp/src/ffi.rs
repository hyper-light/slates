//! The WinFsp user-mode FFI (§4.6), hand-transcribed from winfsp's `winfsp.h`/`fsctl.h` (v2.0), the
//! exact discipline the AFD reactor uses over the WDK: the real `#[repr(C)]` struct layouts and the
//! `FspFileSystem*` exports, found in the DLL at run time ([`api`]), so the bridge cross-lints on the Windows
//! target and runs against the real DLL on Windows. Not a guess — the structs carry the header's own `static_assert`
//! sizes as comments, and the calling conventions match the header (`extern "system"` = the `FSP_API`
//! stdcall/WINAPI exports; `extern "C"` = the interface's default-convention callbacks).

#![allow(non_snake_case)]

use std::ffi::c_void;

/// An `NTSTATUS` at the FFI boundary — the `LONG` (`i32`) WinFsp's API takes. The host-buildable core
/// holds status codes as `u32` ([`crate::Ntstatus`]); this is their signed form at the ABI.
pub(crate) type Ntstatus = i32;
/// A wide (UTF-16) C string pointer, WinFsp's `PWSTR`.
pub(crate) type Pwstr = *mut u16;
/// A `PVOID` — an opaque pointer (a file context, a buffer).
pub(crate) type Pvoid = *mut c_void;
/// `PSECURITY_DESCRIPTOR` — an opaque self-relative security descriptor.
pub(crate) type PSecurityDescriptor = *mut c_void;
/// WinFsp's `BOOLEAN` — a single byte, 0 or 1.
pub(crate) type Boolean = u8;

/// The opaque `FSP_FILE_SYSTEM`, accessed only through its stable leading fields (`Version`, then the
/// `UserContext` a host stores its handler pointer in). A prefix struct is a sound view of the real
/// object's head under `#[repr(C)]` — the two fields sit at the same offsets (0 and, `PVOID`-aligned,
/// 8 on 64-bit) as in the full 792-byte struct, and the host never reads past them.
#[repr(C)]
pub(crate) struct FileSystem {
  pub(crate) version: u16,
  pub(crate) user_context: Pvoid,
}

/// `FSP_FSCTL_VOLUME_PARAMS` (504 bytes; header `static_assert`) — the mount parameters. The two
/// bitfield words are held as raw `u32`s (`flags`/`flags_v1`) whose bits the host sets by constant, so
/// the layout is exact without a bitfield representation.
#[repr(C)]
pub(crate) struct VolumeParams {
  pub(crate) version: u16,
  pub(crate) sector_size: u16,
  pub(crate) sectors_per_allocation_unit: u16,
  pub(crate) max_component_length: u16,
  pub(crate) volume_creation_time: u64,
  pub(crate) volume_serial_number: u32,
  pub(crate) transact_timeout: u32,
  pub(crate) irp_timeout: u32,
  pub(crate) irp_capacity: u32,
  pub(crate) file_info_timeout: u32,
  /// The V0 flag bits (`CaseSensitiveSearch` = bit 0, `CasePreservedNames` = 1, `UnicodeOnDisk` = 2,
  /// `PersistentAcls` = 3, `ReadOnlyVolume` = 9; the 32 V0 bitfields pack into this one word).
  pub(crate) flags: u32,
  /// Format: the UNC-prefix field — `FSP_FSCTL_VOLUME_PREFIX_SIZE / sizeof(WCHAR)` = 192 wide chars.
  pub(crate) prefix: [u16; 192],
  /// Format: the file-system-name field — `FSP_FSCTL_VOLUME_FSNAME_SIZE / sizeof(WCHAR)` = 16 wide chars.
  pub(crate) file_system_name: [u16; 16],
  pub(crate) flags_v1: u32,
  pub(crate) volume_info_timeout: u32,
  pub(crate) dir_info_timeout: u32,
  pub(crate) security_timeout: u32,
  pub(crate) stream_info_timeout: u32,
  pub(crate) ea_timeout: u32,
  pub(crate) fsext_control_code: u32,
  pub(crate) reserved32: [u32; 1],
  pub(crate) reserved64: [u64; 2],
}

/// Format: `CaseSensitiveSearch` — bit 0 of [`VolumeParams::flags`].
pub(crate) const VOLUME_FLAG_CASE_SENSITIVE_SEARCH: u32 = 1 << 0;
/// Format: `CasePreservedNames` — bit 1.
pub(crate) const VOLUME_FLAG_CASE_PRESERVED_NAMES: u32 = 1 << 1;
/// Format: `UnicodeOnDisk` — bit 2.
pub(crate) const VOLUME_FLAG_UNICODE_ON_DISK: u32 = 1 << 2;
/// Format: `PersistentAcls` — bit 3 (the file system enforces access-control lists).
pub(crate) const VOLUME_FLAG_PERSISTENT_ACLS: u32 = 1 << 3;
/// Format: `ReadOnlyVolume` — bit 9: Windows refuses every write to the volume before it reaches the shard.
pub(crate) const VOLUME_FLAG_READ_ONLY_VOLUME: u32 = 1 << 9;
/// Format: `FlushAndPurgeOnCleanup` — bit 14: at a file's last handle's cleanup the kernel writes its cached pages back
/// and then purges them, before it posts the `Cleanup` request (`src/sys/cleanup.c`, `FspFileNodeCleanupFlush`).
pub(crate) const VOLUME_FLAG_FLUSH_AND_PURGE_ON_CLEANUP: u32 = 1 << 14;
/// Format: `UmFileContextIsUserContext2` — bit 16: a callback's `FileContext` is the one its own open returned (the
/// kernel's per-handle `UserContext2`), not the file node's `UserContext`, which every open of the file shares
/// (`src/dll/fsop.c`, `ValOfFileContext`).
pub(crate) const VOLUME_FLAG_FILE_CONTEXT_PER_OPEN: u32 = 1 << 16;

/// `FSP_FSCTL_VOLUME_INFO` (88 bytes) — free/total size and the volume label.
#[repr(C)]
pub(crate) struct VolumeInfo {
  pub(crate) total_size: u64,
  pub(crate) free_size: u64,
  pub(crate) volume_label_length: u16,
  pub(crate) volume_label: [u16; 32],
}

/// `FSP_FSCTL_FILE_INFO` (72 bytes) — a file's attributes and times, the neutral shape every WinFsp
/// reply carries. Times are Windows `FILETIME` (100-ns ticks since 1601), from [`crate::filetime_from_unix_ns`].
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct FileInfo {
  pub(crate) file_attributes: u32,
  pub(crate) reparse_tag: u32,
  pub(crate) allocation_size: u64,
  pub(crate) file_size: u64,
  pub(crate) creation_time: u64,
  pub(crate) last_access_time: u64,
  pub(crate) last_write_time: u64,
  pub(crate) change_time: u64,
  pub(crate) index_number: u64,
  pub(crate) hard_links: u32,
  pub(crate) ea_size: u32,
}

/// `FSP_FSCTL_DIR_INFO`'s fixed head (104 bytes; the variable `FileNameBuf` follows). The host fills
/// one per directory entry and hands it to `FspFileSystemAddDirInfo` ([`Api::add_dir_info`]), which copies it plus the name.
#[repr(C)]
pub(crate) struct DirInfo {
  pub(crate) size: u16,
  pub(crate) file_info: FileInfo,
  /// The union `{ UINT64 NextOffset; UINT8 Padding[24]; }` — held as its 24-byte extent so the head is
  /// the full 104 bytes the FSD expects (it copies in place into a `FILE_ID_BOTH_DIR_INFORMATION`).
  pub(crate) padding: [u8; 24],
}

/// A callback slot of unknown-to-us signature (an interface entry the host leaves unimplemented). All
/// function pointers share one layout, so a generic pointer type is a layout-correct `None` filler.
pub(crate) type OpaqueCallback = Option<unsafe extern "C" fn()>;

/// `FSP_FILE_SYSTEM_INTERFACE` — the 64-slot vtable (header `static_assert`: exactly 64 entries). The
/// slots the host implements carry their precise `extern "C"` signatures; the rest are `OpaqueCallback`
/// (`None`), which WinFsp treats as "operation not supported". Field order is the header's, exactly.
#[repr(C)]
pub(crate) struct Interface {
  pub(crate) get_volume_info:
    Option<unsafe extern "C" fn(*mut FileSystem, *mut VolumeInfo) -> Ntstatus>,
  pub(crate) set_volume_label: OpaqueCallback,
  pub(crate) get_security_by_name: Option<
    unsafe extern "C" fn(
      *mut FileSystem,
      Pwstr,
      *mut u32,
      PSecurityDescriptor,
      *mut usize,
    ) -> Ntstatus,
  >,
  pub(crate) create: Option<
    unsafe extern "C" fn(
      *mut FileSystem,
      Pwstr,
      u32,
      u32,
      u32,
      PSecurityDescriptor,
      u64,
      *mut Pvoid,
      *mut FileInfo,
    ) -> Ntstatus,
  >,
  pub(crate) open: Option<
    unsafe extern "C" fn(*mut FileSystem, Pwstr, u32, u32, *mut Pvoid, *mut FileInfo) -> Ntstatus,
  >,
  pub(crate) overwrite: Option<
    unsafe extern "C" fn(*mut FileSystem, Pvoid, u32, Boolean, u64, *mut FileInfo) -> Ntstatus,
  >,
  pub(crate) cleanup: Option<unsafe extern "C" fn(*mut FileSystem, Pvoid, Pwstr, u32)>,
  pub(crate) close: Option<unsafe extern "C" fn(*mut FileSystem, Pvoid)>,
  pub(crate) read:
    Option<unsafe extern "C" fn(*mut FileSystem, Pvoid, Pvoid, u64, u32, *mut u32) -> Ntstatus>,
  pub(crate) write: Option<
    unsafe extern "C" fn(
      *mut FileSystem,
      Pvoid,
      Pvoid,
      u64,
      u32,
      Boolean,
      Boolean,
      *mut u32,
      *mut FileInfo,
    ) -> Ntstatus,
  >,
  pub(crate) flush: Option<unsafe extern "C" fn(*mut FileSystem, Pvoid, *mut FileInfo) -> Ntstatus>,
  pub(crate) get_file_info:
    Option<unsafe extern "C" fn(*mut FileSystem, Pvoid, *mut FileInfo) -> Ntstatus>,
  pub(crate) set_basic_info: Option<
    unsafe extern "C" fn(
      *mut FileSystem,
      Pvoid,
      u32,
      u64,
      u64,
      u64,
      u64,
      *mut FileInfo,
    ) -> Ntstatus,
  >,
  pub(crate) set_file_size:
    Option<unsafe extern "C" fn(*mut FileSystem, Pvoid, u64, Boolean, *mut FileInfo) -> Ntstatus>,
  pub(crate) can_delete: Option<unsafe extern "C" fn(*mut FileSystem, Pvoid, Pwstr) -> Ntstatus>,
  pub(crate) rename:
    Option<unsafe extern "C" fn(*mut FileSystem, Pvoid, Pwstr, Pwstr, Boolean) -> Ntstatus>,
  pub(crate) get_security: OpaqueCallback,
  pub(crate) set_security: OpaqueCallback,
  pub(crate) read_directory: Option<
    unsafe extern "C" fn(*mut FileSystem, Pvoid, Pwstr, Pwstr, Pvoid, u32, *mut u32) -> Ntstatus,
  >,
  pub(crate) resolve_reparse_points: OpaqueCallback,
  pub(crate) get_reparse_point: OpaqueCallback,
  pub(crate) set_reparse_point: OpaqueCallback,
  pub(crate) delete_reparse_point: OpaqueCallback,
  pub(crate) get_stream_info: OpaqueCallback,
  pub(crate) get_dir_info_by_name: OpaqueCallback,
  pub(crate) control: OpaqueCallback,
  pub(crate) set_delete: OpaqueCallback,
  pub(crate) create_ex: OpaqueCallback,
  pub(crate) overwrite_ex: OpaqueCallback,
  pub(crate) get_ea: OpaqueCallback,
  pub(crate) set_ea: OpaqueCallback,
  pub(crate) obsolete0: OpaqueCallback,
  pub(crate) dispatcher_stopped: OpaqueCallback,
  /// Slots 34–64 (`NTSTATUS (*Reserved[31])()`), keeping the vtable exactly 64 pointers wide.
  pub(crate) reserved: [OpaqueCallback; 31],
}

impl Interface {
  /// An all-`None` interface, to be filled with the implemented slots. `const` so the host's vtable is
  /// a single static, its pointer stable for the file system's whole life.
  pub(crate) const EMPTY: Interface = Interface {
    get_volume_info: None,
    set_volume_label: None,
    get_security_by_name: None,
    create: None,
    open: None,
    overwrite: None,
    cleanup: None,
    close: None,
    read: None,
    write: None,
    flush: None,
    get_file_info: None,
    set_basic_info: None,
    set_file_size: None,
    can_delete: None,
    rename: None,
    get_security: None,
    set_security: None,
    read_directory: None,
    resolve_reparse_points: None,
    get_reparse_point: None,
    set_reparse_point: None,
    delete_reparse_point: None,
    get_stream_info: None,
    get_dir_info_by_name: None,
    control: None,
    set_delete: None,
    create_ex: None,
    overwrite_ex: None,
    get_ea: None,
    set_ea: None,
    obsolete0: None,
    dispatcher_stopped: None,
    reserved: [None; 31],
  };
}

/// Format: `FILE_DIRECTORY_FILE` (NT `CreateOptions`) — the one create option a user-mode FS must
/// read: the object being created/opened is a directory, not a file.
pub(crate) const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
/// Format: `FILE_NO_INTERMEDIATE_BUFFERING` (`0x0000_0008`, wdm.h), the create option an unbuffered open
/// (`FILE_FLAG_NO_BUFFERING`) carries; WinFsp passes the request's options to `Open` whole (`src/sys/create.c`).
pub(crate) const FILE_NO_INTERMEDIATE_BUFFERING: u32 = 0x0000_0008;
/// Format: `FspCleanupDelete` — the Cleanup flag that means "delete this object now" (`winfsp.h`).
pub(crate) const FSP_CLEANUP_DELETE: u32 = 0x01;
/// Format: `FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY_COARSE` — one mutually-exclusive lock around every
/// callback, so the host's single-owner volume is never touched concurrently.
pub(crate) const GUARD_STRATEGY_COARSE: i32 = 1;

/// Format: `STATUS_CANT_WAIT` (`0xC000_00D8`, ntstatus.h) as WinFsp's `NTSTATUS`: `FspFileSystemNotifyBegin` with a zero
/// timeout answers it while a rename is in progress, and the round is owed again.
pub(crate) const STATUS_CANT_WAIT: Ntstatus = i32::from_ne_bytes(0xC000_00D8_u32.to_ne_bytes());
/// Format: `STATUS_DLL_NOT_FOUND` (`0xC000_0135`, ntstatus.h): WinFsp's library is not on this host.
pub(crate) const STATUS_DLL_NOT_FOUND: Ntstatus = i32::from_ne_bytes(0xC000_0135_u32.to_ne_bytes());

/// `FspFileSystemCreate` (`winfsp.h`).
pub(crate) type CreateFn = unsafe extern "system" fn(
  device_name: Pwstr,
  volume_params: *const VolumeParams,
  interface: *const Interface,
  file_system: *mut *mut FileSystem,
) -> Ntstatus;
/// `FspFileSystemDelete`, `FspFileSystemRemoveMountPoint` and `FspFileSystemStopDispatcher`: each takes the file
/// system and returns nothing.
pub(crate) type FileSystemFn = unsafe extern "system" fn(file_system: *mut FileSystem);
/// `FspFileSystemSetMountPoint`.
pub(crate) type SetMountPointFn =
  unsafe extern "system" fn(file_system: *mut FileSystem, mount_point: Pwstr) -> Ntstatus;
/// `FspFileSystemStartDispatcher`.
pub(crate) type StartDispatcherFn =
  unsafe extern "system" fn(file_system: *mut FileSystem, thread_count: u32) -> Ntstatus;
/// `FspFileSystemSetOperationGuardStrategyF`.
pub(crate) type SetGuardStrategyFn =
  unsafe extern "system" fn(file_system: *mut FileSystem, strategy: i32);
/// `FspFileSystemAddDirInfo`.
pub(crate) type AddDirInfoFn = unsafe extern "system" fn(
  dir_info: *mut DirInfo,
  buffer: Pvoid,
  length: u32,
  bytes_transferred: *mut u32,
) -> Boolean;
/// `FspFileSystemNotifyBegin`.
pub(crate) type NotifyBeginFn =
  unsafe extern "system" fn(file_system: *mut FileSystem, timeout: u32) -> Ntstatus;
/// `FspFileSystemNotifyEnd`.
pub(crate) type NotifyEndFn = unsafe extern "system" fn(file_system: *mut FileSystem) -> Ntstatus;
/// `FspFileSystemOperationProcessIdF`: the process that made the create or open being served (its id from the request's
/// access token, `winfsp.h` `FspFileSystemOperationProcessId`), zero for any other request.
pub(crate) type OperationProcessIdFn = unsafe extern "system" fn() -> u32;
/// `FspFileSystemNotify`: the records of a round (`FSP_FSCTL_NOTIFY_INFO`, built by `crate::notify`) and their size.
pub(crate) type NotifyFn = unsafe extern "system" fn(
  file_system: *mut FileSystem,
  notify_info: Pvoid,
  size: usize,
) -> Ntstatus;

/// The `FSP_API` exports the host calls, found in `winfsp-x64.dll` at run time rather than linked through its import
/// library. Linked, a daemon on a Windows machine without WinFsp could not start at all (`STATUS_DLL_NOT_FOUND` at
/// load), though WinFsp is only its mount transport; loaded, the daemon starts everywhere and reports the WinFsp
/// transport refused `WinFspNotInstalled` where the library is absent. The search is WinFsp's own `FspLoad`
/// (`winfsp.h`): the library by name, then `<InstallDir>\bin\` under the registry key WinFsp's installer writes.
#[derive(Clone, Copy)]
pub(crate) struct Api {
  pub(crate) create: CreateFn,
  pub(crate) delete: FileSystemFn,
  pub(crate) set_mount_point: SetMountPointFn,
  pub(crate) remove_mount_point: FileSystemFn,
  pub(crate) start_dispatcher: StartDispatcherFn,
  pub(crate) stop_dispatcher: FileSystemFn,
  pub(crate) set_guard_strategy: SetGuardStrategyFn,
  pub(crate) add_dir_info: AddDirInfoFn,
  pub(crate) notify_begin: NotifyBeginFn,
  pub(crate) notify_end: NotifyEndFn,
  pub(crate) notify: NotifyFn,
  /// Absent from a WinFsp that predates it: the mount then makes no purge (`crate::purge`), and serves as before.
  pub(crate) operation_process_id: Option<OperationProcessIdFn>,
}

/// Format: the library's name for this architecture (`FSP_DLLNAME`: `winfsp-` and `FSP_FSCTL_PRODUCT_FILE_ARCH`).
#[cfg(target_arch = "x86_64")]
const LIBRARY: &str = "winfsp-x64.dll";
/// Format: see the x86_64 form.
#[cfg(target_arch = "aarch64")]
const LIBRARY: &str = "winfsp-a64.dll";
/// Format: the registry key WinFsp's installer records its directory under on a 64-bit Windows
/// (`FSP_FSCTL_PRODUCT_FULL_REGKEY`: the 32-bit installer writes the 32-bit view).
const REGISTRY_KEY: &str = "Software\\WOW6432Node\\WinFsp";
/// Format: the value naming the install directory (`FspLoad`'s `InstallDir`).
const REGISTRY_VALUE: &str = "InstallDir";
/// Format: where the library sits under the install directory (`FSP_DLLPATH`).
const LIBRARY_DIRECTORY: &str = "bin\\";
/// Format: the most UTF-16 units of the install directory read from the registry: `MAX_PATH`, the bound `FspLoad`'s own
/// buffer has.
const INSTALL_DIR_UNITS: usize = 260;

/// The exports, loaded once for the process and kept for its life (the library is never unloaded: every mount's
/// dispatcher threads run inside it). The load's refusal is kept as well, so a host without WinFsp asks once.
pub(crate) fn api() -> Result<&'static Api, Ntstatus> {
  static API: std::sync::OnceLock<Result<Api, Ntstatus>> = std::sync::OnceLock::new();
  API.get_or_init(load).as_ref().map_err(|status| *status)
}

/// Loads the library and finds every export, or refuses `STATUS_DLL_NOT_FOUND` (no library) or the procedure's absence
/// (`STATUS_DLL_NOT_FOUND` too: a WinFsp older than the notify exports, 2021's 1.10, cannot serve this mount).
fn load() -> Result<Api, Ntstatus> {
  let module = load_library().ok_or(STATUS_DLL_NOT_FOUND)?;
  Ok(Api {
    create: symbol(module, c"FspFileSystemCreate")?,
    delete: symbol(module, c"FspFileSystemDelete")?,
    set_mount_point: symbol(module, c"FspFileSystemSetMountPoint")?,
    remove_mount_point: symbol(module, c"FspFileSystemRemoveMountPoint")?,
    start_dispatcher: symbol(module, c"FspFileSystemStartDispatcher")?,
    stop_dispatcher: symbol(module, c"FspFileSystemStopDispatcher")?,
    set_guard_strategy: symbol(module, c"FspFileSystemSetOperationGuardStrategyF")?,
    add_dir_info: symbol(module, c"FspFileSystemAddDirInfo")?,
    notify_begin: symbol(module, c"FspFileSystemNotifyBegin")?,
    notify_end: symbol(module, c"FspFileSystemNotifyEnd")?,
    notify: symbol(module, c"FspFileSystemNotify")?,
    operation_process_id: symbol(module, c"FspFileSystemOperationProcessIdF").ok(),
  })
}

/// `winfsp-x64.dll` by name (a library on the loader's path), else by the directory WinFsp's installer recorded.
fn load_library() -> Option<windows_sys::Win32::Foundation::HMODULE> {
  use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
  let by_name = wide(LIBRARY);
  // SAFETY: `by_name` is a NUL-terminated UTF-16 string that outlives the call.
  let module = unsafe { LoadLibraryW(by_name.as_ptr()) };
  if !module.is_null() {
    return Some(module);
  }
  let mut path = install_dir()?;
  path.push_str(LIBRARY_DIRECTORY);
  path.push_str(LIBRARY);
  let by_path = wide(&path);
  // SAFETY: as above, for the full path.
  let module = unsafe { LoadLibraryW(by_path.as_ptr()) };
  (!module.is_null()).then_some(module)
}

/// The install directory WinFsp's installer recorded (with its trailing separator), or `None`.
fn install_dir() -> Option<String> {
  use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
  let key = wide(REGISTRY_KEY);
  let value = wide(REGISTRY_VALUE);
  let mut buffer = [0u16; INSTALL_DIR_UNITS];
  let mut bytes = u32::try_from(std::mem::size_of_val(&buffer)).ok()?;
  // SAFETY: `key` and `value` are NUL-terminated; `buffer` holds `bytes` bytes, which the call reads as its capacity and
  // overwrites with the length it wrote, never more.
  let status = unsafe {
    RegGetValueW(
      HKEY_LOCAL_MACHINE,
      key.as_ptr(),
      value.as_ptr(),
      RRF_RT_REG_SZ,
      std::ptr::null_mut(),
      buffer.as_mut_ptr().cast(),
      &mut bytes,
    )
  };
  if status != 0 {
    return None;
  }
  let units = usize::try_from(bytes)
    .ok()?
    .checked_div(std::mem::size_of::<u16>())?;
  let written = buffer.get(..units)?;
  let text: Vec<u16> = written
    .iter()
    .copied()
    .take_while(|unit| *unit != 0)
    .collect();
  let mut dir = String::from_utf16(&text).ok()?;
  if !dir.ends_with('\\') {
    dir.push('\\');
  }
  Some(dir)
}

/// A NUL-terminated UTF-16 copy of `text`.
fn wide(text: &str) -> Vec<u16> {
  text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The function-pointer types [`symbol`] may produce: exactly the export signatures above. Sealed by being private, so
/// [`symbol`]'s one reinterpretation is only ever to the signature the header gives the export it names.
trait Export: Copy {}
impl Export for CreateFn {}
impl Export for FileSystemFn {}
impl Export for SetMountPointFn {}
impl Export for StartDispatcherFn {}
impl Export for SetGuardStrategyFn {}
impl Export for AddDirInfoFn {}
// `NotifyBeginFn` has `StartDispatcherFn`'s shape (the file system and a `u32`), so the impl above covers it.
impl Export for NotifyEndFn {}
impl Export for NotifyFn {}
impl Export for OperationProcessIdFn {}

/// The export `name` of `module`, as the function-pointer type `T` the header gives it; refused when the library does not
/// export it.
fn symbol<T: Export>(
  module: windows_sys::Win32::Foundation::HMODULE,
  name: &std::ffi::CStr,
) -> Result<T, Ntstatus> {
  use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
  // SAFETY: `module` is a library this process loaded and never unloads; `name` is NUL-terminated.
  let found =
    unsafe { GetProcAddress(module, name.as_ptr().cast()) }.ok_or(STATUS_DLL_NOT_FOUND)?;
  if std::mem::size_of::<T>() != std::mem::size_of_val(&found) {
    return Err(STATUS_DLL_NOT_FOUND);
  }
  // SAFETY: `found` is the address of the export `name`, and `T` is a function-pointer type of the same size (checked)
  // whose signature and convention are the header's for that export (the sealed `Export` set; each caller names the
  // export and the type together in `load`). A function pointer is reinterpreted as another of identical layout.
  Ok(unsafe { std::mem::transmute_copy::<_, T>(&found) })
}
