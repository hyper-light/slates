//! The RAM-only cache of the profile: a memory object that creates no filesystem entry, keyed by
//! the host identity, so a restart on the same machine skips the measurement (§4.1).
//!
//! - Linux: `memfd_create` (an anonymous file in RAM; the descriptor is what gets shared).
//! - macOS: `shm_open` (a named POSIX shared-memory object; a kernel object, not a path).
//! - Windows: a pagefile-backed section from `CreateFileMappingW` with a `Local\` name.
//!
//! Layout (`Format:` constants below): a magic, the format version, the 32-byte identity hash,
//! a generation word, the payload length, then the JSON payload. The generation word is odd
//! while a writer is inside and even when the payload is complete, so a reader that sees an odd
//! generation or a changed one after reading reports `ProfileUnavailable` rather than a torn
//! profile (the seqlock rule). In Phase 2 the anchor process owns this object; in Phase 0 the
//! same process creates and reads it, which is what the tests exercise.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::MachineError;
use crate::facts::Identity;

/// Format: the segment's magic, `SLPF` in little-endian ASCII.
const MAGIC: u32 = 0x4650_4C53;

/// Format: the layout version.
const LAYOUT_VERSION: u32 = 1;

/// Format: the header's size in bytes: magic (4), version (4), identity hash (32),
/// generation (8), payload length (8), padding to a 64-byte boundary.
const HEADER_BYTES: usize = 64;

/// Format: the magic's offset.
const AT_MAGIC: usize = 0;
/// Format: the layout version's offset.
const AT_VERSION: usize = 4;
/// Format: the identity hash's offset.
const AT_IDENTITY: usize = 8;
/// Format: the identity hash's width (BLAKE3).
const IDENTITY_BYTES: usize = 32;
/// Format: the generation word's offset (8-byte aligned).
const AT_GENERATION: usize = 40;
/// Format: the payload length's offset.
const AT_LENGTH: usize = 48;

/// A mapped segment. Dropping it unmaps and releases the object.
pub struct Segment {
  mapping: platform::Mapping,
  len: usize,
}

impl std::fmt::Debug for Segment {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Segment").field("len", &self.len).finish()
  }
}

impl Segment {
  /// Creates a segment holding `payload` for `identity`. The name distinguishes segments of
  /// different users on the same host (the OS scopes it further on Windows with `Local\`).
  pub fn publish(name: &str, identity: &Identity, payload: &[u8]) -> Result<Segment, MachineError> {
    let len = HEADER_BYTES.saturating_add(payload.len());
    let mapping = platform::Mapping::create(name, len)?;
    let mut segment = Segment { mapping, len };
    segment.write(identity, payload);
    Ok(segment)
  }

  /// Reads the payload back, checking the magic, version, identity and generation.
  pub fn read(&self, identity: &Identity) -> Result<Vec<u8>, MachineError> {
    let bytes = self.bytes();
    if bytes.len() < HEADER_BYTES {
      return Err(unavailable("segment shorter than its header"));
    }
    if read_u32(bytes, AT_MAGIC) != MAGIC {
      return Err(unavailable("wrong magic"));
    }
    if read_u32(bytes, AT_VERSION) != LAYOUT_VERSION {
      return Err(unavailable("wrong layout version"));
    }
    let cached = &bytes[AT_IDENTITY..AT_IDENTITY + IDENTITY_BYTES];
    if cached != identity.hash() {
      return Err(MachineError::ProfileStale {
        cached: hex(cached),
        current: hex(&identity.hash()),
      });
    }
    let generation_before = self.generation().load(Ordering::Acquire);
    if generation_before % 2 == 1 {
      return Err(unavailable("a writer is inside the segment"));
    }
    let length = usize::try_from(read_u64(bytes, AT_LENGTH))
      .map_err(|_| unavailable("payload length overflows"))?;
    let end = HEADER_BYTES
      .checked_add(length)
      .ok_or_else(|| unavailable("payload length overflows"))?;
    if end > bytes.len() {
      return Err(unavailable("payload length exceeds the segment"));
    }
    let payload = bytes[HEADER_BYTES..end].to_vec();
    if self.generation().load(Ordering::Acquire) != generation_before {
      return Err(unavailable("the segment changed while it was read"));
    }
    Ok(payload)
  }

  /// The mapped length.
  pub fn len(&self) -> usize {
    self.len
  }

  /// Whether the mapping is empty (never, for a published segment).
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  fn write(&mut self, identity: &Identity, payload: &[u8]) {
    let generation = self.generation();
    let start = generation.load(Ordering::Acquire);
    generation.store(start | 1, Ordering::Release);
    let bytes = self.bytes_mut();
    put(bytes, AT_MAGIC, &MAGIC.to_le_bytes());
    put(bytes, AT_VERSION, &LAYOUT_VERSION.to_le_bytes());
    put(bytes, AT_IDENTITY, &identity.hash());
    let length = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    put(bytes, AT_LENGTH, &length.to_le_bytes());
    put(bytes, HEADER_BYTES, payload);
    self
      .generation()
      .store((start | 1).wrapping_add(1), Ordering::Release);
  }

  fn generation(&self) -> &AtomicU64 {
    // SAFETY: the mapping is at least HEADER_BYTES long, the generation word sits at an
    // 8-byte-aligned offset of a page-aligned mapping, and AtomicU64 has the layout of u64.
    unsafe { &*self.mapping.ptr().add(AT_GENERATION).cast::<AtomicU64>() }
  }

  fn bytes(&self) -> &[u8] {
    // SAFETY: the mapping is `len` readable bytes for the life of `self`.
    unsafe { std::slice::from_raw_parts(self.mapping.ptr(), self.len) }
  }

  fn bytes_mut(&mut self) -> &mut [u8] {
    // SAFETY: the mapping is `len` writable bytes, and `&mut self` makes this the only access.
    unsafe { std::slice::from_raw_parts_mut(self.mapping.ptr(), self.len) }
  }
}

fn unavailable(reason: &str) -> MachineError {
  MachineError::ProfileUnavailable {
    reason: reason.to_owned(),
  }
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
  bytes[at..at + value.len()].copy_from_slice(value);
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
  let mut word = [0u8; size_of::<u32>()];
  let n = word.len();
  word.copy_from_slice(&bytes[at..at + n]);
  u32::from_le_bytes(word)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
  let mut word = [0u8; size_of::<u64>()];
  let n = word.len();
  word.copy_from_slice(&bytes[at..at + n]);
  u64::from_le_bytes(word)
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
mod platform {
  use crate::error::MachineError;
  use std::ffi::{CString, c_void};

  /// A mapped memory object.
  pub(super) struct Mapping {
    ptr: *mut u8,
    len: usize,
    fd: libc::c_int,
    #[cfg(target_os = "macos")]
    name: CString,
  }

  impl Mapping {
    pub(super) fn create(name: &str, len: usize) -> Result<Mapping, MachineError> {
      let fd = open_object(name)?;
      let size = libc::off_t::try_from(len).map_err(|_| MachineError::OsRefused {
        call: "ftruncate",
        code: None,
      })?;
      // SAFETY: `fd` is an open memory object we own.
      // structural: allow — the object is in RAM (memfd or shm); sizing it is not a disk write.
      if unsafe { libc::ftruncate(fd, size) } != 0 {
        let err = MachineError::os("ftruncate");
        close(fd, name);
        return Err(err);
      }
      // SAFETY: a shared read/write mapping of the whole object.
      let ptr = unsafe {
        libc::mmap(
          std::ptr::null_mut(),
          len,
          libc::PROT_READ | libc::PROT_WRITE,
          libc::MAP_SHARED,
          fd,
          0,
        )
      };
      if ptr == libc::MAP_FAILED {
        let err = MachineError::os("mmap");
        close(fd, name);
        return Err(err);
      }
      Ok(Mapping {
        ptr: ptr.cast::<u8>(),
        len,
        fd,
        #[cfg(target_os = "macos")]
        name: object_name(name),
      })
    }

    pub(super) fn ptr(&self) -> *mut u8 {
      self.ptr
    }
  }

  impl Drop for Mapping {
    fn drop(&mut self) {
      // SAFETY: `ptr`/`len` are the mapping created above; `fd` is ours to close.
      unsafe {
        libc::munmap(self.ptr.cast::<c_void>(), self.len);
        libc::close(self.fd);
      }
      #[cfg(target_os = "macos")]
      // SAFETY: a NUL-terminated name of an object we created.
      unsafe {
        libc::shm_unlink(self.name.as_ptr());
      }
    }
  }

  #[cfg(target_os = "linux")]
  fn open_object(name: &str) -> Result<libc::c_int, MachineError> {
    let cname = CString::new(name).map_err(|_| MachineError::OsRefused {
      call: "memfd_create",
      code: None,
    })?;
    // SAFETY: a NUL-terminated name; the flag closes the descriptor on exec.
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
      Err(MachineError::os("memfd_create"))
    } else {
      Ok(fd)
    }
  }

  #[cfg(target_os = "macos")]
  fn object_name(name: &str) -> CString {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    // A POSIX shm name: a leading slash, no other slashes, short enough for the OS.
    let clean: String = name
      .chars()
      .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
      .collect();
    CString::new(format!("/{clean}-{uid}")).unwrap_or_default()
  }

  /// Format: owner read/write only.
  #[cfg(target_os = "macos")]
  const SHM_MODE: libc::c_uint = 0o600;

  #[cfg(target_os = "macos")]
  fn open_object(name: &str) -> Result<libc::c_int, MachineError> {
    let cname = object_name(name);
    // A stale object from a crashed earlier process is removed first; the name is per user.
    // SAFETY: a NUL-terminated name.
    unsafe { libc::shm_unlink(cname.as_ptr()) };
    // SAFETY: a NUL-terminated name; the mode is owner read/write only.
    let fd = unsafe {
      libc::shm_open(
        cname.as_ptr(),
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        SHM_MODE,
      )
    };
    if fd < 0 {
      Err(MachineError::os("shm_open"))
    } else {
      Ok(fd)
    }
  }

  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  fn open_object(_name: &str) -> Result<libc::c_int, MachineError> {
    Err(MachineError::OsRefused {
      call: "memory object",
      code: None,
    })
  }

  fn close(fd: libc::c_int, _name: &str) {
    // SAFETY: `fd` is ours.
    unsafe { libc::close(fd) };
    #[cfg(target_os = "macos")]
    // SAFETY: a NUL-terminated name of an object we created.
    unsafe {
      libc::shm_unlink(object_name(_name).as_ptr());
    }
  }
}

#[cfg(windows)]
mod platform {
  use crate::error::MachineError;
  use std::ffi::c_void;
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
  use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
  };

  pub(super) struct Mapping {
    ptr: *mut u8,
    handle: HANDLE,
  }

  impl Mapping {
    pub(super) fn create(name: &str, len: usize) -> Result<Mapping, MachineError> {
      let wide: Vec<u16> = format!("Local\\{name}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
      let size = u64::try_from(len).map_err(|_| MachineError::OsRefused {
        call: "CreateFileMappingW",
        code: None,
      })?;
      let high = u32::try_from(size >> u32::BITS).unwrap_or(u32::MAX);
      let low = u32::try_from(size & u64::from(u32::MAX)).unwrap_or(u32::MAX);
      // SAFETY: a pagefile-backed section with a NUL-terminated wide name.
      let handle = unsafe {
        CreateFileMappingW(
          INVALID_HANDLE_VALUE,
          std::ptr::null(),
          PAGE_READWRITE,
          high,
          low,
          wide.as_ptr(),
        )
      };
      if handle.is_null() {
        return Err(MachineError::os("CreateFileMappingW"));
      }
      // SAFETY: a full read/write view of the section.
      let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
      if view.Value.is_null() {
        let err = MachineError::os("MapViewOfFile");
        // SAFETY: the handle is ours.
        unsafe { CloseHandle(handle) };
        return Err(err);
      }
      Ok(Mapping {
        ptr: view.Value.cast::<u8>(),
        handle,
      })
    }

    pub(super) fn ptr(&self) -> *mut u8 {
      self.ptr
    }
  }

  impl Drop for Mapping {
    fn drop(&mut self) {
      // SAFETY: the view and handle were created above and are ours.
      unsafe {
        UnmapViewOfFile(
          windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.ptr.cast::<c_void>(),
          },
        );
        CloseHandle(self.handle);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn identity(cores: u32) -> Identity {
    Identity {
      cpu: "cpu".into(),
      os: "os".into(),
      arch: "arch".into(),
      cores,
      memory: 1,
      page: 4096,
    }
  }

  #[test]
  fn a_published_profile_reads_back_for_the_same_identity() {
    let id = identity(8);
    let payload = b"{\"profile\":true}";
    let segment = Segment::publish("slates-profile-test-a", &id, payload).unwrap();
    assert_eq!(segment.len(), HEADER_BYTES + payload.len());
    assert!(!segment.is_empty());
    assert_eq!(segment.read(&id).unwrap(), payload);
  }

  #[test]
  fn a_different_identity_is_reported_stale_with_both_hashes() {
    let segment = Segment::publish("slates-profile-test-b", &identity(8), b"x").unwrap();
    match segment.read(&identity(9)) {
      Err(MachineError::ProfileStale { cached, current }) => {
        assert_ne!(cached, current);
        assert_eq!(cached.len(), 64);
      }
      other => panic!("expected ProfileStale, got {other:?}"),
    }
  }

  #[test]
  fn a_corrupted_header_is_unavailable_not_a_panic() {
    let id = identity(8);
    let mut segment = Segment::publish("slates-profile-test-c", &id, b"payload").unwrap();
    segment.bytes_mut()[AT_MAGIC] ^= 0xFF;
    assert!(matches!(
      segment.read(&id),
      Err(MachineError::ProfileUnavailable { .. })
    ));
    segment.bytes_mut()[AT_MAGIC] ^= 0xFF;
    put(segment.bytes_mut(), AT_LENGTH, &u64::MAX.to_le_bytes());
    assert!(matches!(
      segment.read(&id),
      Err(MachineError::ProfileUnavailable { .. })
    ));
  }

  #[test]
  fn an_odd_generation_means_a_writer_is_inside() {
    let id = identity(8);
    let segment = Segment::publish("slates-profile-test-d", &id, b"payload").unwrap();
    segment.generation().fetch_add(1, Ordering::Release);
    let err = segment.read(&id).unwrap_err();
    assert!(err.to_string().contains("writer"), "{err}");
  }
}
