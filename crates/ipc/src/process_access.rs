//! Process authority for Windows mount holders (A-198, §4.6 and §4.13). A protected OWNER RIGHTS DACL refuses
//! unprivileged OpenProcess calls for duplication or ACL-changing authority, including from the same user. The supervisor retains
//! the all-access handle CreateProcessW issued, and GetCurrentProcess retains all access on supported Windows, so
//! legitimate held-object transfers still work. The child receives this DACL at creation, before it runs with any
//! inherited handle. Dedicated CLI holder roles also protect themselves before acquiring their first held object.
//!
//! Evidence: Microsoft's Process Security and Access Rights and GetCurrentProcess contracts; the descriptor's owner
//! restriction follows A-175's measured peer-section policy. The real-process tests in anchor's handles_windows and
//! CLI's winfsp both admitted foreign duplication before this change (bug record 2026-11-02).

use std::ptr::null_mut;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{
  DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
  SECURITY_ATTRIBUTES, SetKernelObjectSecurity,
};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

use crate::IpcError;

/// Format: the rights a new process handle may carry: public liveness queries, exit observation and termination,
/// plus descriptor reading. None permits duplicating handles, changing the DACL/owner, or writing process memory.
const HOLDER_PROCESS_RIGHTS: u32 =
  READ_CONTROL | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE;
/// Format: revision one of SDDL, as required by ConvertStringSecurityDescriptorToSecurityDescriptorW.
const SDDL_REVISION: u32 = 1;

/// Owns the self-relative descriptor until every CreateProcessW or current-process security call using it returned.
pub struct ProcessSecurity {
  descriptor: PSECURITY_DESCRIPTOR,
}

impl ProcessSecurity {
  /// The mount-holder process policy. OWNER RIGHTS also suppresses the owner's implicit WRITE_DAC, preventing a
  /// refused peer from first rewriting the DACL to grant itself duplication authority (A-175).
  pub fn handle_owner() -> Result<Self, IpcError> {
    let text = format!("D:P(A;;{HOLDER_PROCESS_RIGHTS:#x};;;OW)");
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = null_mut();
    // SAFETY: wide is terminated, the revision is defined, and the output belongs to this call. The API allocates
    // with LocalAlloc; ProcessSecurity frees it after its last use.
    let converted = unsafe {
      ConvertStringSecurityDescriptorToSecurityDescriptorW(
        wide.as_ptr(),
        SDDL_REVISION,
        &mut descriptor,
        null_mut(),
      )
    };
    if converted == 0 {
      return Err(crate::win_spawn::refused(
        "ConvertStringSecurityDescriptorToSecurityDescriptorW (process)",
      ));
    }
    if descriptor.is_null() {
      return Err(IpcError::OsRefused {
        call: "process descriptor conversion returned no descriptor",
        code: None,
      });
    }
    Ok(Self { descriptor })
  }

  /// The borrowed attributes CreateProcessW consumes synchronously. A returned process handle is not inheritable;
  /// the explicit spawn list, rather than this descriptor, chooses what the child inherits.
  pub(crate) fn attributes(&self) -> Result<SECURITY_ATTRIBUTES, IpcError> {
    let length =
      u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(|_| IpcError::OsRefused {
        call: "SECURITY_ATTRIBUTES length does not fit the Win32 field",
        code: None,
      })?;
    Ok(SECURITY_ATTRIBUTES {
      nLength: length,
      lpSecurityDescriptor: self.descriptor,
      bInheritHandle: 0,
    })
  }

  /// Applies the holder policy before a dedicated anchor or daemon role owns protected handles. The pseudo handle
  /// retains this process's own authority; newly opened handles are checked against the new descriptor.
  pub fn apply_to_current(&self) -> Result<(), IpcError> {
    // SAFETY: the current-process pseudo handle is valid, the descriptor stays owned throughout the call, and only
    // its protected DACL is applied; neither a privilege nor a filesystem object is involved.
    let applied = unsafe {
      SetKernelObjectSecurity(
        GetCurrentProcess(),
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        self.descriptor,
      )
    };
    if applied == 0 {
      return Err(crate::win_spawn::refused(
        "SetKernelObjectSecurity (process holder)",
      ));
    }
    Ok(())
  }
}

impl Drop for ProcessSecurity {
  fn drop(&mut self) {
    // SAFETY: the descriptor is exactly the LocalAlloc allocation conversion returned, owned once here; all
    // synchronous borrowers have returned before this owner can drop.
    let remaining = unsafe { LocalFree(self.descriptor) };
    if !remaining.is_null() {
      slates_machine::log_line!(
        "slates-ipc: {}",
        crate::win_spawn::refused("LocalFree (process descriptor)")
      );
    }
  }
}

/// Protects a dedicated holder role. Embedding a library does not silently change its host process's security.
pub fn confine_current_holder() -> Result<(), IpcError> {
  ProcessSecurity::handle_owner()?.apply_to_current()
}
