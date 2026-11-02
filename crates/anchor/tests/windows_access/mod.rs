//! A-198's access probe as an ordinary caller, even when a runner inherited debugger authority. SeDebugPrivilege
//! bypasses a process DACL (Microsoft OpenProcess). A restricted copy of this process's token removes privileges
//! only while the synchronous probe runs on this thread; the primary token and every other thread are unchanged.
//! The helper checks its starting and terminal context, and independently samples the effective debug privilege.

use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{ERROR_NO_TOKEN, LUID};
use windows_sys::Win32::Security::{
  CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateToken, ImpersonateLoggedOnUser,
  LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, PRIVILEGE_SET, PrivilegeCheck, RevertToSelf,
  SE_DEBUG_NAME, SecurityIdentification, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

fn thread_token() -> Option<OwnedHandle> {
  let mut token = null_mut();
  // SAFETY: samples this thread's token only; OpenAsSelf checks the query against its primary identity.
  let opened = unsafe {
    OpenThreadToken(
      GetCurrentThread(),
      TOKEN_QUERY | TOKEN_DUPLICATE,
      1,
      &mut token,
    )
  };
  if opened == 0 {
    assert_eq!(
      std::io::Error::last_os_error().raw_os_error(),
      Some(i32::try_from(ERROR_NO_TOKEN).unwrap())
    );
    return None;
  }
  // SAFETY: the successful call returned this newly owned handle.
  Some(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn primary_token() -> OwnedHandle {
  let mut token = null_mut();
  // SAFETY: samples this process's own token, with exactly the rights required for a restricted copy.
  let opened = unsafe {
    OpenProcessToken(
      GetCurrentProcess(),
      TOKEN_QUERY | TOKEN_DUPLICATE,
      &mut token,
    )
  };
  assert_ne!(
    opened,
    0,
    "OpenProcessToken: {}",
    std::io::Error::last_os_error()
  );
  // SAFETY: the successful call returned this newly owned handle.
  unsafe { OwnedHandle::from_raw_handle(token) }
}

/// Whether the effective token, including this thread's impersonation, has the DACL-bypassing debugger privilege.
pub(crate) fn debug_privilege_enabled() -> bool {
  let effective = thread_token().unwrap_or_else(primary_token);
  let mut identified = null_mut();
  // SAFETY: duplicates our own effective token for identification only, without installing it on a thread.
  let copied = unsafe {
    DuplicateToken(
      effective.as_raw_handle(),
      SecurityIdentification,
      &mut identified,
    )
  };
  assert_ne!(
    copied,
    0,
    "DuplicateToken: {}",
    std::io::Error::last_os_error()
  );
  // SAFETY: the successful call returned this newly owned handle.
  let identified = unsafe { OwnedHandle::from_raw_handle(identified) };
  let mut debug = LUID {
    LowPart: 0,
    HighPart: 0,
  };
  // SAFETY: the local system's defined privilege name and a writable local identifier output.
  let named = unsafe { LookupPrivilegeValueW(null(), SE_DEBUG_NAME, &mut debug) };
  assert_ne!(
    named,
    0,
    "LookupPrivilegeValueW: {}",
    std::io::Error::last_os_error()
  );
  let mut required = PRIVILEGE_SET {
    PrivilegeCount: 1,
    Control: 0,
    Privilege: [LUID_AND_ATTRIBUTES {
      Luid: debug,
      Attributes: 0,
    }],
  };
  let mut enabled = 0;
  // SAFETY: an identification token opened for query, one initialized privilege and a writable result.
  let checked = unsafe { PrivilegeCheck(identified.as_raw_handle(), &mut required, &mut enabled) };
  assert_ne!(
    checked,
    0,
    "PrivilegeCheck: {}",
    std::io::Error::last_os_error()
  );
  enabled != 0
}

struct Unprivileged {
  _token: OwnedHandle,
  _thread: PhantomData<*mut ()>,
}

impl Drop for Unprivileged {
  fn drop(&mut self) {
    // SAFETY: this guard cannot move to another thread, and its caller established that the thread had no token.
    let restored = unsafe { RevertToSelf() };
    if restored == 0 {
      eprintln!(
        "the access probe could not restore its thread: {}",
        std::io::Error::last_os_error()
      );
    }
  }
}

/// Runs only the probe without elevated privileges. Checks restore on normal completion; unwinding still restores
/// through the same guard. No user, account, primary token or other thread is changed.
pub(crate) fn as_unprivileged(probe: impl FnOnce()) {
  assert!(
    thread_token().is_none(),
    "the probe must start without impersonation"
  );
  let before = debug_privilege_enabled();
  let primary = primary_token();
  let mut token = null_mut();
  // SAFETY: creates a less privileged copy of our own token, disabling privileges and changing no SIDs. It requires
  // TOKEN_DUPLICATE, not a privilege, and returns a handle with the source handle's query/duplicate rights.
  let restricted = unsafe {
    CreateRestrictedToken(
      primary.as_raw_handle(),
      DISABLE_MAX_PRIVILEGE,
      0,
      null(),
      0,
      null(),
      0,
      null(),
      &mut token,
    )
  };
  assert_ne!(
    restricted,
    0,
    "CreateRestrictedToken: {}",
    std::io::Error::last_os_error()
  );
  // SAFETY: the successful call returned this newly owned handle.
  let token = unsafe { OwnedHandle::from_raw_handle(token) };
  // SAFETY: a restricted copy of our own primary identity, with QUERY and DUPLICATE. The API permits same-identity
  // impersonation without a privilege; the thread-local guard reverts it on every exit.
  let impersonated = unsafe { ImpersonateLoggedOnUser(token.as_raw_handle()) };
  assert_ne!(
    impersonated,
    0,
    "ImpersonateLoggedOnUser: {}",
    std::io::Error::last_os_error()
  );
  let guard = Unprivileged {
    _token: token,
    _thread: PhantomData,
  };
  let during = debug_privilege_enabled();
  eprintln!("WINDOWS ACCESS: debug privilege before {before}, during {during}");
  assert!(!during, "debugger authority must not bypass the DACL probe");
  probe();
  drop(guard);
  assert!(
    thread_token().is_none(),
    "the probe must restore its original thread context"
  );
}
