//! The WinFsp user-mode FFI (§4.6), hand-transcribed from winfsp's `winfsp.h`/`fsctl.h` (v2.0), the
//! exact discipline the AFD reactor uses over the WDK: the real `#[repr(C)]` struct layouts and the
//! `FspFileSystem*` exports, so the bridge cross-lints on the Windows target without linking and runs
//! against the real DLL on Windows. Not a guess — the structs carry the header's own `static_assert`
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
/// one per directory entry and hands it to [`FspFileSystemAddDirInfo`], which copies it plus the name.
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
/// Format: `FspCleanupDelete` — the Cleanup flag that means "delete this object now" (`winfsp.h`).
pub(crate) const FSP_CLEANUP_DELETE: u32 = 0x01;
/// Format: `FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY_COARSE` — one mutually-exclusive lock around every
/// callback, so the host's single-owner volume is never touched concurrently.
pub(crate) const GUARD_STRATEGY_COARSE: i32 = 1;

// SAFETY (whole block): these are the `FSP_API` exports of `winfsp-x64.dll` (the import library the
// build script links), each transcribed with its header signature and the WINAPI (`extern "system"`)
// convention. A cross-target `cargo check` type-checks them without linking.
unsafe extern "system" {
  pub(crate) fn FspFileSystemCreate(
    device_name: Pwstr,
    volume_params: *const VolumeParams,
    interface: *const Interface,
    file_system: *mut *mut FileSystem,
  ) -> Ntstatus;
  pub(crate) fn FspFileSystemDelete(file_system: *mut FileSystem);
  pub(crate) fn FspFileSystemSetMountPoint(
    file_system: *mut FileSystem,
    mount_point: Pwstr,
  ) -> Ntstatus;
  pub(crate) fn FspFileSystemRemoveMountPoint(file_system: *mut FileSystem);
  pub(crate) fn FspFileSystemStartDispatcher(
    file_system: *mut FileSystem,
    thread_count: u32,
  ) -> Ntstatus;
  pub(crate) fn FspFileSystemStopDispatcher(file_system: *mut FileSystem);
  pub(crate) fn FspFileSystemSetOperationGuardStrategyF(
    file_system: *mut FileSystem,
    strategy: i32,
  );
  pub(crate) fn FspFileSystemAddDirInfo(
    dir_info: *mut DirInfo,
    buffer: Pvoid,
    length: u32,
    bytes_transferred: *mut u32,
  ) -> Boolean;
}
