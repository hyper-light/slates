//! Crash-dump exclusion of the memory that holds private bytes, region by region (A-239; GAPS `8a-win-dumps`; §4.2
//! "RAM-only guarantees", R1).
//!
//! **Why.** On Unix the anchor and the daemon exclude their whole process from core dumps before they map a private
//! byte (`slates-cli`'s `dumps`: the core limit at 0, and on Linux an empty core filter). Windows has no unprivileged
//! equivalent: a crash dump is written by Windows Error Reporting, and turning it off for a process is a registry
//! setting an administrator makes, which R10 refuses to require. What Windows does offer an ordinary process is
//! `WerRegisterExcludedMemoryBlock`: a block registered by the process is left out of the dump WER writes when the
//! process crashes. So each mapping that holds tenant content or keys is registered when it is mapped and
//! unregistered before it is unmapped, by the value of this module's [`DumpExclusion`], which the mapping owns:
//! - every content region of the chunk arena ([`crate::region::Region`]), private or a range of the anchor's content
//!   object;
//! - every view of a shared memory object (the anchor segment with its seal root, write logs and image slots, the
//!   content object, the copies a client or a VMM maps), in [`crate::shared`];
//! - hyper-seal's key region, registered once for the process's life by the daemon (`slates-server`'s `seal_keys`).
//!
//! **What is left (the residual).** Stacks and the heap are not excluded and may hold fragments of content or keys in
//! flight: a request's bytes in a buffer, an inline file's bytes in a tree node, a key schedule on the stack. A dump
//! that another tool takes on purpose (a debugger, `procdump`, Task Manager's "Create dump file") does not consult the
//! registration; such a tool runs as the same user or an administrator, who can read the process's memory directly
//! anyway, so the dump adds no reach. WER keeps at most [`WER_MAX_REGISTERED_ENTRIES`] blocks a process, and a block is
//! at most `u32::MAX` bytes, so a mapping past the limit, or a Windows without the call, stays in the dump: each such
//! mapping is counted ([`refused`]) and the daemon's status reports the count, never hidden (D-12's honest degradation).
//!
//! **The call is found at run time.** The two functions are resolved from `kernel32.dll` with `GetProcAddress` the
//! first time a block is registered, not linked by import: an import missing from an older Windows would stop the
//! binary loading at all, a bigger hole than the one this closes. A Windows without them counts every mapping refused.
//!
//! On Unix a [`DumpExclusion`] is empty and does nothing: the whole process is excluded already.

#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Format: WER's limit on the blocks one process may register (`WER_MAX_REGISTERED_ENTRIES`, `werapi.h`; Microsoft
/// Learn, "WerRegisterExcludedMemoryBlock"). Stated here so the module doc and the counting share one source.
pub const WER_MAX_REGISTERED_ENTRIES: u32 = 512;

/// Mappings of this process left in WER's dumps because a block could not be registered: the limit was reached, or
/// the call is missing or refused. Zero is the healthy reading; always zero on Unix.
#[cfg(windows)]
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// The mappings of this process that a crash dump may carry because their exclusion was refused (the module doc).
#[cfg(windows)]
pub fn refused() -> u64 {
  REFUSED.load(Ordering::Relaxed)
}

/// The mappings of this process that a crash dump may carry because their exclusion was refused: none on Unix,
/// where the process is excluded whole.
#[cfg(not(windows))]
pub fn refused() -> u64 {
  0
}

/// A range of this process's memory kept out of WER's crash dumps while this value lives (the module doc). Owned by
/// the mapping it covers and dropped before the mapping is unmapped.
#[derive(Debug, Default)]
pub struct DumpExclusion {
  /// The range's first byte, as an exposed address.
  #[cfg(windows)]
  base: usize,
  /// The blocks registered from `base`, each [`windows::BLOCK_BYTES`] long but the last.
  #[cfg(windows)]
  blocks: usize,
}

impl DumpExclusion {
  /// Keeps `[base, base + len)` out of crash dumps until the value is dropped; a range WER refuses is counted in
  /// [`refused`] and kept in. The caller must drop the value before the range is unmapped.
  #[cfg(windows)]
  pub fn exclude(base: *const u8, len: usize) -> DumpExclusion {
    let (blocks, whole) = (0usize, true); // DIAG mutation: nothing registered
    if !whole {
      REFUSED.fetch_add(1, Ordering::Relaxed);
    }
    DumpExclusion {
      base: base.expose_provenance(),
      blocks,
    }
  }

  /// Nothing to register on Unix: the process is excluded whole (`slates-cli`'s `dumps`).
  #[cfg(not(windows))]
  pub fn exclude(_base: *const u8, _len: usize) -> DumpExclusion {
    DumpExclusion {}
  }

  /// Unregisters the range now, before its mapping goes; the value is empty afterwards.
  pub fn release(&mut self) {
    #[cfg(windows)]
    {
      windows::unregister(self.base, self.blocks);
      self.blocks = 0;
    }
  }
}

impl Drop for DumpExclusion {
  fn drop(&mut self) {
    self.release();
  }
}

#[cfg(windows)]
mod windows {
  use std::ffi::c_void;
  use std::sync::OnceLock;

  use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
  use windows_sys::core::HRESULT;

  /// Format: the most bytes one WER block covers, its size being a `u32` (`WerRegisterExcludedMemoryBlock`'s `dwSize`).
  pub(super) const BLOCK_BYTES: usize = u32::MAX as usize;

  /// `WerRegisterExcludedMemoryBlock(address, size)`, as `werapi.h` declares it.
  type Register = unsafe extern "system" fn(*const c_void, u32) -> HRESULT;
  /// `WerUnregisterExcludedMemoryBlock(address)`, as `werapi.h` declares it.
  type Unregister = unsafe extern "system" fn(*const c_void) -> HRESULT;

  /// What `GetProcAddress` answers for an export: an untyped function pointer (`FARPROC`'s inner type).
  type Found = unsafe extern "system" fn() -> isize;

  /// The two functions, found once a process; `None` on a Windows that lacks them.
  static CALLS: OnceLock<Option<(Register, Unregister)>> = OnceLock::new();

  fn calls() -> Option<(Register, Unregister)> {
    *CALLS.get_or_init(|| {
      let kernel32: Vec<u16> = "kernel32.dll"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
      // SAFETY: a NUL-terminated wide module name; kernel32 is loaded in every process and never unloaded.
      let module = unsafe { GetModuleHandleW(kernel32.as_ptr()) };
      if module.is_null() {
        return None;
      }
      // SAFETY: `module` is loaded for the process's life and the name is NUL-terminated.
      let register =
        unsafe { GetProcAddress(module, c"WerRegisterExcludedMemoryBlock".as_ptr().cast()) }?;
      // SAFETY: as above.
      let unregister =
        unsafe { GetProcAddress(module, c"WerUnregisterExcludedMemoryBlock".as_ptr().cast()) }?;
      // SAFETY: each address is the export of that name, and each type is the function-pointer type `werapi.h`
      // declares for it, with the `system` convention; a function pointer is reinterpreted as one of the same layout.
      Some(unsafe {
        (
          std::mem::transmute::<Found, Register>(register),
          std::mem::transmute::<Found, Unregister>(unregister),
        )
      })
    })
  }

  /// Registers `[base, base + len)` a block at a time: the blocks registered, and whether they cover the range. A
  /// refusal part-way unregisters the blocks already made, so a range is either excluded whole or not at all.
  pub(super) fn register(base: *const u8, len: usize) -> (usize, bool) {
    let Some((register, unregister)) = calls() else {
      return (0, false);
    };
    let mut registered = 0usize;
    let mut offset = 0usize;
    while offset < len {
      let size = len.saturating_sub(offset).min(BLOCK_BYTES);
      let at = base.wrapping_add(offset);
      // SAFETY: `at .. at + size` lies in a mapping the caller owns for as long as the registration lives; the call
      // records the address and size and reads no byte.
      let answer = unsafe { register(at.cast(), u32::try_from(size).unwrap_or(u32::MAX)) };
      if answer != 0 {
        unregister_from(unregister, base, registered);
        return (0, false);
      }
      registered = registered.saturating_add(1);
      offset = offset.saturating_add(size);
    }
    (registered, true)
  }

  /// Unregisters the `blocks` blocks registered from `base`.
  pub(super) fn unregister(base: usize, blocks: usize) {
    if blocks == 0 {
      return;
    }
    if let Some((_, unregister)) = calls() {
      unregister_from(unregister, std::ptr::with_exposed_provenance(base), blocks);
    }
  }

  fn unregister_from(unregister: Unregister, base: *const u8, blocks: usize) {
    for block in 0..blocks {
      let at = base.wrapping_add(block.saturating_mul(BLOCK_BYTES));
      // SAFETY: `at` is the address of a block this process registered and has not unregistered; the call reads no
      // byte. Its answer is not kept: a refusal can only mean the block is not in WER's list, and nothing is undone by
      // knowing it, since the mapping it named is about to go either way.
      let _not_listed = unsafe { unregister(at.cast()) };
    }
  }
}
