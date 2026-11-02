//! A private coherence open's caller (A-190/A-198, §4.6 Windows). PID alone also matches ordinary I/O made by an
//! embedding application. The owned mount thread instead uses a restricted copy of its own token with a fresh SID
//! marker. WinFsp copies this effective token into Create.AccessToken (sys/create.c); its restricting SIDs survive
//! that copy, whereas TokenId does not. The receiver requires both its own PID and this mount's marker before using
//! a pending identity-addressed purge. No privilege is enabled: CreateRestrictedToken removes privileges and
//! ImpersonateLoggedOnUser permits the caller's own authenticated identity (Microsoft's API contracts).

use crate::host::WinFspError;
use std::marker::PhantomData;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::Security::Cryptography::{
  BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
  CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, EqualSid, GetTokenInformation,
  ImpersonateLoggedOnUser, RevertToSelf, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES,
  TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_QUERY, TokenRestrictedSids,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Shape: Everyone permits the normal restricted access check; one private SID identifies this mount's requests.
const RESTRICTING_SIDS: usize = 2;
/// Format: a domain SID's 21 followed by four random 32-bit subauthorities, a 128-bit marker.
const MARKER_WORDS: usize = 5;
/// Format: the four random subauthorities' bytes.
const RANDOM_BYTES: usize = (MARKER_WORDS - 1) * size_of::<u32>();
/// Format: TOKEN_GROUPS with two SID_AND_ATTRIBUTES entries and two maximum-size SID bodies, aligned for pointers.
const GROUP_WORDS: usize = (size_of::<TOKEN_GROUPS>()
  + size_of::<SID_AND_ATTRIBUTES>() * RESTRICTING_SIDS
  + SECURITY_MAX_SID_SIZE as usize * RESTRICTING_SIDS)
  .div_ceil(size_of::<usize>());

/// An inline SID with its platform header and aligned subauthorities; Win32 only borrows it synchronously.
#[repr(C)]
#[derive(Clone, Copy)]
struct Sid<const WORDS: usize> {
  revision: u8,
  count: u8,
  authority: [u8; 6],
  subauthorities: [u32; WORDS],
}

/// The immutable mount-specific marker, copied to the receiver before it starts.
#[derive(Clone, Copy)]
pub(crate) struct Marker(Sid<MARKER_WORDS>);

impl Marker {
  /// Checks the kernel-owned Create token while its request remains in processing. A query refusal is an error,
  /// never permission to reinterpret an uncertain purge as an ordinary zero write.
  pub(crate) fn matches(self, token: BorrowedHandle<'_>) -> Result<bool, WinFspError> {
    let mut words = [0usize; GROUP_WORDS];
    let mut returned = 0;
    let length =
      u32::try_from(size_of_val(&words)).map_err(|_| error("restricted token buffer length"))?;
    // SAFETY: pointer-aligned initialized storage of exactly length bytes; token is borrowed through this call.
    if unsafe {
      GetTokenInformation(
        token.as_raw_handle(),
        TokenRestrictedSids,
        words.as_mut_ptr().cast(),
        length,
        &mut returned,
      )
    } == 0
    {
      return Err(error("GetTokenInformation (purge marker)"));
    }
    if usize::try_from(returned)
      .ok()
      .is_none_or(|length| length > size_of_val(&words) || length < size_of::<u32>())
    {
      return Err(error("restricted token result length"));
    }
    // SAFETY: GetTokenInformation initialized the native TOKEN_GROUPS layout in pointer-aligned live storage.
    let groups = unsafe { &*words.as_ptr().cast::<TOKEN_GROUPS>() };
    let count =
      usize::try_from(groups.GroupCount).map_err(|_| error("restricted token group count"))?;
    if count > RESTRICTING_SIDS {
      return Err(error("unexpected restricting SIDs in an own-process open"));
    }
    // SAFETY: the kernel returned count initialized entries and their SID pointers into this live result buffer.
    // The cap above is the exact number in our private token; a larger foreign thread token is refused.
    let entries = unsafe {
      std::slice::from_raw_parts(
        words
          .as_ptr()
          .cast::<u8>()
          .add(std::mem::offset_of!(TOKEN_GROUPS, Groups))
          .cast::<SID_AND_ATTRIBUTES>(),
        count,
      )
    };
    for entry in entries {
      // SAFETY: both arguments are valid SID layouts, one returned by the kernel and one owned inline above.
      if unsafe { EqualSid(entry.Sid, std::ptr::from_ref(&self.0).cast_mut().cast()) } != 0 {
        return Ok(true);
      }
    }
    Ok(false)
  }
}

/// Owned only on the mount's command thread, separate from the receiver's immutable marker.
pub(crate) struct PurgeToken {
  token: OwnedHandle,
  marker: Marker,
}

impl PurgeToken {
  pub(crate) fn new() -> Result<Self, WinFspError> {
    let mut random = [0u8; RANDOM_BYTES];
    // SAFETY: the system-preferred generator fills exactly this live output array; no algorithm handle is borrowed.
    let status = unsafe {
      BCryptGenRandom(
        std::ptr::null_mut(),
        random.as_mut_ptr(),
        u32::try_from(random.len()).map_err(|_| error("purge marker length"))?,
        BCRYPT_USE_SYSTEM_PREFERRED_RNG,
      )
    };
    if status != 0 {
      return Err(WinFspError::Api {
        call: "BCryptGenRandom (purge marker)",
        status,
      });
    }
    let mut subauthorities = [0; MARKER_WORDS];
    *subauthorities
      .first_mut()
      .ok_or_else(|| error("purge SID layout"))? = 21;
    for (word, bytes) in subauthorities
      .iter_mut()
      .skip(1)
      .zip(random.as_chunks::<{ size_of::<u32>() }>().0.iter())
    {
      *word = u32::from_le_bytes(*bytes);
    }
    let marker = Marker(Sid {
      revision: 1,
      count: u8::try_from(MARKER_WORDS).map_err(|_| error("purge SID count"))?,
      authority: [0, 0, 0, 0, 0, 5],
      subauthorities,
    });
    let everyone = Sid {
      revision: 1,
      count: 1,
      authority: [0, 0, 0, 0, 0, 1],
      subauthorities: [0],
    };
    let mut primary = std::ptr::null_mut();
    // SAFETY: queries only this process's primary token, with the rights required for a less privileged copy.
    if unsafe {
      OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_QUERY | TOKEN_DUPLICATE,
        &mut primary,
      )
    } == 0
    {
      return Err(error("OpenProcessToken (purge)"));
    }
    // SAFETY: the successful open returned one fresh owned token handle.
    let primary = unsafe { OwnedHandle::from_raw_handle(primary) };
    let restricting = [
      SID_AND_ATTRIBUTES {
        Sid: std::ptr::from_ref(&everyone).cast_mut().cast(),
        Attributes: 0,
      },
      SID_AND_ATTRIBUTES {
        Sid: std::ptr::from_ref(&marker.0).cast_mut().cast(),
        Attributes: 0,
      },
    ];
    let mut token = std::ptr::null_mut();
    // SAFETY: both valid inline SIDs and the owned source token live until this synchronous copy completes.
    if unsafe {
      CreateRestrictedToken(
        primary.as_raw_handle(),
        DISABLE_MAX_PRIVILEGE,
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
        u32::try_from(restricting.len()).map_err(|_| error("purge SID count"))?,
        restricting.as_ptr(),
        &mut token,
      )
    } == 0
    {
      return Err(error("CreateRestrictedToken (purge)"));
    }
    // SAFETY: the successful restricted-token creation returned one newly owned handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    if !marker.matches(token.as_handle())? {
      return Err(error("restricted purge token lost its marker"));
    }
    Ok(Self { token, marker })
  }

  pub(crate) fn marker(&self) -> Marker {
    self.marker
  }

  /// Calls only private drive I/O under the less privileged token and restores before the command thread continues.
  /// A restoration failure terminates that owned thread through the caller's error path; dropping also retries it.
  pub(crate) fn during<T>(
    &self,
    operation: impl FnOnce() -> Result<T, WinFspError>,
  ) -> Result<T, WinFspError> {
    // SAFETY: this dedicated mount thread uses its own same-identity restricted token, no added privilege.
    if unsafe { ImpersonateLoggedOnUser(self.token.as_raw_handle()) } == 0 {
      return Err(error("ImpersonateLoggedOnUser (purge)"));
    }
    let mut scope = ScopedToken {
      active: true,
      _thread: PhantomData,
    };
    let result = operation();
    scope.restore()?;
    result
  }
}

struct ScopedToken {
  active: bool,
  _thread: PhantomData<*mut ()>,
}
impl ScopedToken {
  fn restore(&mut self) -> Result<(), WinFspError> {
    if self.active {
      // SAFETY: this non-Send guard stays on the dedicated thread that installed its own token.
      if unsafe { RevertToSelf() } == 0 {
        return Err(error("RevertToSelf (purge)"));
      }
      self.active = false;
    }
    Ok(())
  }
}
impl Drop for ScopedToken {
  fn drop(&mut self) {
    if let Err(error) = self.restore() {
      eprintln!("slates-winfsp: {error}");
    }
  }
}

fn error(call: &'static str) -> WinFspError {
  WinFspError::Os {
    call,
    code: std::io::Error::last_os_error().raw_os_error(),
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]
  use super::*;
  use windows_sys::Win32::Security::{DuplicateToken, SecurityIdentification};

  /// A-198. Do: copy a private token as WinFsp does for identification and compare it with another mount's marker.
  /// Expect: the copy retains its own marker, never the other's, without relying on the token object's identity.
  #[test]
  fn a_private_marker_survives_a_security_copy_and_is_mount_specific() {
    let own = PurgeToken::new().unwrap();
    let other = PurgeToken::new().unwrap();
    let mut copied = std::ptr::null_mut();
    // SAFETY: this owned token has duplication rights, the output is writable, and no thread impersonation changes.
    let duplicated = unsafe { DuplicateToken(own.token.as_raw_handle(), SecurityIdentification, &mut copied) };
    assert_ne!(duplicated, 0);
    // SAFETY: the successful duplication returned one fresh owned handle.
    let copied = unsafe { OwnedHandle::from_raw_handle(copied) };
    assert!(own.marker.matches(copied.as_handle()).unwrap());
    assert!(!other.marker.matches(copied.as_handle()).unwrap());
    own.during(|| Ok(())).unwrap();
  }
}
