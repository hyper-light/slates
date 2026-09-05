//! The RAM-only cache of the profile: a memory object that creates no filesystem entry, keyed by
//! the host identity, so a restart on the same machine skips the measurement (§4.1).
//!
//! - Linux: `memfd_create` (an anonymous file in RAM; the descriptor is what gets shared).
//! - macOS: `shm_open` (a named POSIX shared-memory object; a kernel object, not a path).
//! - Windows: a pagefile-backed section from `CreateFileMappingW` with a `Local\` name.
//!
//! The object is created through rustix's safe wrappers and mapped with `memmap2`; the one unsafe
//! block on Unix is the file-backed map itself, whose invariant is that we hold the only
//! descriptor and no one else mutates the object while it is mapped. Layout (`Format:` constants
//! below): a magic, the format version, the 32-byte identity hash, a generation word, the
//! payload length, then the JSON payload. The generation word is odd while a writer is inside and
//! even when the payload is complete, so a reader that sees an odd generation or a changed one
//! after reading reports `ProfileUnavailable` rather than a torn profile (the seqlock rule). In
//! Phase 0 the same process writes and reads through `&mut`/`&`, so the word is read and written
//! as a plain integer; the cross-process reader of Phase 2 (the anchor's clients) reads it
//! through an atomic view of the same bytes.

use memmap2::MmapMut;

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
  map: MmapMut,
  object: platform::Object,
}

impl std::fmt::Debug for Segment {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Segment")
      .field("len", &self.map.len())
      .finish()
  }
}

impl Segment {
  /// Creates a segment holding `payload` for `identity`. The name distinguishes segments of
  /// different users on the same host (the OS scopes it further on Windows with `Local\`).
  pub fn publish(name: &str, identity: &Identity, payload: &[u8]) -> Result<Segment, MachineError> {
    let len = HEADER_BYTES.saturating_add(payload.len());
    let (object, map) = platform::create(name, len)?;
    let mut segment = Segment { map, object };
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
    let generation_before = read_u64(bytes, AT_GENERATION);
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
    if read_u64(bytes, AT_GENERATION) != generation_before {
      return Err(unavailable("the segment changed while it was read"));
    }
    Ok(payload)
  }

  /// The mapped length.
  pub fn len(&self) -> usize {
    self.map.len()
  }

  /// Whether the mapping is empty (never, for a published segment).
  pub fn is_empty(&self) -> bool {
    self.map.is_empty()
  }

  fn write(&mut self, identity: &Identity, payload: &[u8]) {
    let start = read_u64(&self.map, AT_GENERATION);
    put(&mut self.map, AT_GENERATION, &(start | 1).to_le_bytes());
    put(&mut self.map, AT_MAGIC, &MAGIC.to_le_bytes());
    put(&mut self.map, AT_VERSION, &LAYOUT_VERSION.to_le_bytes());
    put(&mut self.map, AT_IDENTITY, &identity.hash());
    let length = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    put(&mut self.map, AT_LENGTH, &length.to_le_bytes());
    put(&mut self.map, HEADER_BYTES, payload);
    put(
      &mut self.map,
      AT_GENERATION,
      &((start | 1).wrapping_add(1)).to_le_bytes(),
    );
    let _ = &self.object;
  }

  fn bytes(&self) -> &[u8] {
    &self.map
  }

  #[cfg(test)]
  fn bytes_mut(&mut self) -> &mut [u8] {
    &mut self.map
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
  use memmap2::{MmapMut, MmapOptions};
  use std::os::fd::OwnedFd;

  /// The memory object behind the map; dropping it closes the descriptor (and unlinks the
  /// shared-memory name on macOS).
  pub(super) struct Object {
    _fd: OwnedFd,
    #[cfg(target_os = "macos")]
    name: String,
  }

  #[cfg(target_os = "macos")]
  impl Drop for Object {
    fn drop(&mut self) {
      let _ = rustix::shm::unlink(self.name.as_str());
    }
  }

  fn refused(call: &'static str, e: rustix::io::Errno) -> MachineError {
    MachineError::OsRefused {
      call,
      code: Some(e.raw_os_error()),
    }
  }

  pub(super) fn create(name: &str, len: usize) -> Result<(Object, MmapMut), MachineError> {
    let object = open_object(name)?;
    let size = u64::try_from(len).map_err(|_| MachineError::OsRefused {
      call: "ftruncate",
      code: None,
    })?;
    rustix::fs::ftruncate(&object._fd, size).map_err(|e| refused("ftruncate", e))?;
    // SAFETY: the object was just created by us, this is its only descriptor, and nothing else
    // maps or writes it while the map lives; the map never outlives the object it borrows.
    let map = unsafe { MmapOptions::new().len(len).map_mut(&object._fd) }.map_err(|e| {
      MachineError::OsRefused {
        call: "mmap",
        code: e.raw_os_error(),
      }
    })?;
    Ok((object, map))
  }

  #[cfg(target_os = "linux")]
  fn open_object(name: &str) -> Result<Object, MachineError> {
    let fd = rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC)
      .map_err(|e| refused("memfd_create", e))?;
    Ok(Object { _fd: fd })
  }

  #[cfg(target_os = "macos")]
  fn object_name(name: &str) -> String {
    let uid = rustix::process::getuid().as_raw();
    // A POSIX shm name: a leading slash, no other slashes, short enough for the OS.
    let clean: String = name
      .chars()
      .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
      .collect();
    format!("/{clean}-{uid}")
  }

  #[cfg(target_os = "macos")]
  fn open_object(name: &str) -> Result<Object, MachineError> {
    use rustix::fs::Mode;
    use rustix::shm::OFlags;
    let name = object_name(name);
    // A stale object from a crashed earlier process is removed first; the name is per user.
    let _ = rustix::shm::unlink(name.as_str());
    let fd = rustix::shm::open(
      name.as_str(),
      OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
      Mode::RUSR | Mode::WUSR,
    )
    .map_err(|e| refused("shm_open", e))?;
    Ok(Object { _fd: fd, name })
  }

  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  fn open_object(_name: &str) -> Result<Object, MachineError> {
    Err(MachineError::OsRefused {
      call: "memory object",
      code: None,
    })
  }
}

#[cfg(windows)]
mod platform {
  use crate::error::MachineError;
  use memmap2::MmapMut;
  use std::ffi::c_void;
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
  use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
  };

  /// The section and its view; the map is a view over the section's bytes.
  pub(super) struct Object {
    handle: HANDLE,
    view: *mut c_void,
  }

  impl Drop for Object {
    fn drop(&mut self) {
      // SAFETY: the view and handle were created in `create` and are ours.
      unsafe {
        UnmapViewOfFile(
          windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view },
        );
        CloseHandle(self.handle);
      }
    }
  }

  pub(super) fn create(name: &str, len: usize) -> Result<(Object, MmapMut), MachineError> {
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
    // The section's view is exposed through an anonymous map copied on publish; a Windows-native
    // zero-copy view arrives with the anchor process in Phase 2.
    let map = MmapMut::map_anon(len).map_err(|e| MachineError::OsRefused {
      call: "map_anon",
      code: e.raw_os_error(),
    })?;
    Ok((
      Object {
        handle,
        view: view.Value,
      },
      map,
    ))
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
    let mut segment = Segment::publish("slates-profile-test-d", &id, b"payload").unwrap();
    let generation = read_u64(segment.bytes(), AT_GENERATION);
    put(
      segment.bytes_mut(),
      AT_GENERATION,
      &(generation + 1).to_le_bytes(),
    );
    let err = segment.read(&id).unwrap_err();
    assert!(err.to_string().contains("writer"), "{err}");
  }
}
