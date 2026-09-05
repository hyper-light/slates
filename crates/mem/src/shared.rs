//! A shared memory object: RAM-backed, created without a filesystem entry, mapped by more than
//! one process (§4.2, §4.7, D-10; `research/low-latency-ipc-and-runtime.md` §2.2). The anchor
//! segment, the profile cache and every client region are one of these.
//!
//! - Linux: `memfd_create`; shared by passing the descriptor (inheritance or `SCM_RIGHTS`).
//! - macOS: `shm_open` with a per-user name under the 31-character limit, mode 0600; shared by
//!   name; the creator unlinks the name when it drops the object.
//! - Windows: a pagefile-backed section `Local\<name>`; shared by name within the session.
//!
//! The mapping is a full read-write view. On Unix it is `memmap2`'s file-backed map (the one
//! `unsafe` call, whose invariant is that the object is ours to map: nothing truncates it while
//! a view lives, and every other mapper follows the same rule); on Windows the section view from
//! `MapViewOfFile`. Words that two processes read and write concurrently (a ring's head and
//! tail, a wake word, a heartbeat) are reached through [`SharedObject::atomic_u64`] and
//! [`SharedObject::atomic_u32`], which view the mapped bytes as atomics: aligned, inside the map,
//! living as long as the map. Everything else is read and written through byte slices by the
//! single owner of that part of the layout.
//!
//! Hermeticity: tmpfs pages, `shm_open` objects and pagefile-backed sections are all pageable;
//! the caller locks what must never reach a disk with [`SharedObject::lock`], and the RAM-only
//! policy reports what it could not lock (D-12).

use std::sync::atomic::{AtomicU32, AtomicU64};

use crate::error::MemError;

/// Format: the widest word the atomic views hand out; every atomic offset is a multiple of it.
const WORD_BYTES: usize = 8;

/// How an object is handed to another process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handoff {
  /// A descriptor number the child inherits (Linux).
  Descriptor(i32),
  /// A name the other process opens (macOS, Windows).
  Name(String),
}

/// A shared memory object and its full view.
pub struct SharedObject {
  inner: platform::Inner,
  len: usize,
}

impl std::fmt::Debug for SharedObject {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SharedObject")
      .field("len", &self.len)
      .finish()
  }
}

impl SharedObject {
  /// Creates an object of `len` bytes named `name` (the name matters on macOS and Windows,
  /// where it is the handoff; on Linux it is the descriptor's label).
  pub fn create(name: &str, len: usize) -> Result<SharedObject, MemError> {
    let inner = platform::create(name, len)?;
    Ok(SharedObject { inner, len })
  }

  /// Opens an object another process created, from its handoff, expecting `len` bytes.
  pub fn open(handoff: &Handoff, len: usize) -> Result<SharedObject, MemError> {
    let inner = platform::open(handoff, len)?;
    Ok(SharedObject { inner, len })
  }

  /// The handoff another process uses to open an object created under `name` on a platform
  /// that shares by name (macOS, Windows); on Linux objects are shared by descriptor only.
  pub fn handoff_for_name(name: &str) -> Option<Handoff> {
    platform::handoff_for_name(name)
  }

  /// What to hand a child process so it can [`SharedObject::open`] this object. On Linux the
  /// descriptor is duplicated without `CLOEXEC` so a spawned child inherits it; the caller
  /// closes nothing (the duplicate lives in the child).
  pub fn handoff(&self) -> Result<Handoff, MemError> {
    self.inner.handoff()
  }

  /// The mapped length.
  pub fn len(&self) -> usize {
    self.len
  }

  /// Whether the map is empty.
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// The bytes, for the single owner of the range it reads.
  pub fn bytes(&self) -> &[u8] {
    self.inner.bytes()
  }

  /// The bytes, for the single owner of the range it writes.
  pub fn bytes_mut(&mut self) -> &mut [u8] {
    self.inner.bytes_mut()
  }

  /// A 64-bit atomic view of the word at `offset` (8-byte aligned, inside the map).
  pub fn atomic_u64(&self, offset: usize) -> Result<&AtomicU64, MemError> {
    self.check_word(offset, WORD_BYTES)?;
    // SAFETY: the offset is a multiple of eight and the eight bytes lie inside the map (checked
    // above); the map lives as long as `self`, which the returned borrow is tied to; `AtomicU64`
    // has the same size and alignment as `u64` and no validity requirement beyond that; every
    // other access to these bytes across processes is atomic by this module's rule.
    let atomic = unsafe { &*self.inner.bytes().as_ptr().add(offset).cast::<AtomicU64>() };
    Ok(atomic)
  }

  /// A 32-bit atomic view of the word at `offset` (4-byte aligned, inside the map).
  pub fn atomic_u32(&self, offset: usize) -> Result<&AtomicU32, MemError> {
    self.check_word(offset, size_of::<u32>())?;
    // SAFETY: as for `atomic_u64`, with four-byte alignment and width.
    let atomic = unsafe { &*self.inner.bytes().as_ptr().add(offset).cast::<AtomicU32>() };
    Ok(atomic)
  }

  fn check_word(&self, offset: usize, width: usize) -> Result<(), MemError> {
    let end = offset.checked_add(width).ok_or(MemError::OutOfRange {
      offset,
      len: self.len,
    })?;
    if !offset.is_multiple_of(width)
      || end > self.len
      || !(self.inner.bytes().as_ptr() as usize).is_multiple_of(WORD_BYTES)
    {
      return Err(MemError::OutOfRange {
        offset,
        len: self.len,
      });
    }
    Ok(())
  }

  /// Locks the whole object into RAM (D-12); the refusal names the OS call.
  pub fn lock(&mut self) -> Result<(), MemError> {
    self.inner.lock()
  }
}

#[cfg(unix)]
mod platform {
  use std::os::fd::OwnedFd;

  use memmap2::{MmapMut, MmapOptions};

  use super::Handoff;
  use crate::error::MemError;

  pub(super) struct Inner {
    map: MmapMut,
    fd: OwnedFd,
    /// The object's name (macOS: what a handoff carries, known to the creator and to an
    /// opener alike, so an attached process can hand the object on again).
    #[cfg(target_os = "macos")]
    name: String,
    /// Whether this process created the object (macOS: the creator unlinks the name on drop).
    #[cfg(target_os = "macos")]
    creator: bool,
  }

  #[cfg(target_os = "macos")]
  impl Drop for Inner {
    fn drop(&mut self) {
      if self.creator {
        let _ = rustix::shm::unlink(self.name.as_str());
      }
    }
  }

  fn refused(call: &'static str, e: rustix::io::Errno) -> MemError {
    MemError::OsRefused {
      call,
      code: Some(e.raw_os_error()),
    }
  }

  fn map(fd: &OwnedFd, len: usize) -> Result<MmapMut, MemError> {
    // SAFETY: the object behind `fd` is a memory object this module created or opened by the
    // handoff its creator gave; by this module's rule no process truncates it while a view
    // lives, and every concurrent word is reached through the atomic views. The map borrows
    // nothing that outlives it.
    unsafe { MmapOptions::new().len(len).map_mut(fd) }.map_err(|e| MemError::OsRefused {
      call: "mmap",
      code: e.raw_os_error(),
    })
  }

  pub(super) fn create(name: &str, len: usize) -> Result<Inner, MemError> {
    let created = create_object(name)?;
    let size = u64::try_from(len).map_err(|_| MemError::OsRefused {
      call: "ftruncate",
      code: None,
    })?;
    // structural: allow — sizing the memory object just created (no filesystem entry; D-10).
    rustix::fs::ftruncate(&created.fd, size).map_err(|e| refused("ftruncate", e))?;
    let map = map(&created.fd, len)?;
    Ok(Inner {
      map,
      fd: created.fd,
      #[cfg(target_os = "macos")]
      name: created.name,
      #[cfg(target_os = "macos")]
      creator: true,
    })
  }

  pub(super) fn open(handoff: &Handoff, len: usize) -> Result<Inner, MemError> {
    let fd = open_object(handoff)?;
    let map = map(&fd, len)?;
    Ok(Inner {
      map,
      fd,
      #[cfg(target_os = "macos")]
      name: match handoff {
        Handoff::Name(name) => name.clone(),
        Handoff::Descriptor(_) => String::new(),
      },
      #[cfg(target_os = "macos")]
      creator: false,
    })
  }

  struct Created {
    fd: OwnedFd,
    #[cfg(target_os = "macos")]
    name: String,
  }

  #[cfg(target_os = "linux")]
  fn create_object(name: &str) -> Result<Created, MemError> {
    let fd = rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC)
      .map_err(|e| refused("memfd_create", e))?;
    Ok(Created { fd })
  }

  #[cfg(target_os = "linux")]
  fn open_object(handoff: &Handoff) -> Result<OwnedFd, MemError> {
    match handoff {
      Handoff::Descriptor(raw) if *raw >= 0 => {
        use std::os::fd::FromRawFd;
        // SAFETY: the number names a descriptor the parent handed to this process by
        // inheritance and nothing else in this process owns it (the handoff protocol).
        Ok(unsafe { OwnedFd::from_raw_fd(*raw) })
      }
      _ => Err(MemError::OsRefused {
        call: "handoff",
        code: None,
      }),
    }
  }

  /// The object's kernel name: the caller's name cleaned, with the uid as the per-user
  /// suffix; a name that would not fit the limit is replaced by a 64-bit hash of the whole
  /// name (truncation once made two clients' regions one object: GAPS §8d).
  #[cfg(target_os = "macos")]
  fn object_name(name: &str) -> String {
    /// Format: the POSIX shared-memory name limit on macOS (`PSHMNAMLEN`, 31).
    const NAME_LIMIT: usize = 31;
    /// Format: the FNV-1a 64-bit offset basis and prime (Fowler, Noll, Vo).
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    /// Format: the FNV-1a 64-bit prime.
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let uid = rustix::process::getuid().as_raw();
    let suffix = format!("-{uid}");
    let clean: String = name
      .chars()
      .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
      .collect();
    if 1 + clean.len() + suffix.len() <= NAME_LIMIT {
      return format!("/{clean}{suffix}");
    }
    let mut hash = FNV_OFFSET;
    for byte in name.bytes() {
      hash ^= u64::from(byte);
      hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("/{hash:016x}{suffix}")
  }

  #[cfg(target_os = "macos")]
  pub(super) fn handoff_for_name(name: &str) -> Option<Handoff> {
    Some(Handoff::Name(object_name(name)))
  }

  #[cfg(not(target_os = "macos"))]
  pub(super) fn handoff_for_name(_name: &str) -> Option<Handoff> {
    None
  }

  #[cfg(target_os = "macos")]
  fn create_object(name: &str) -> Result<Created, MemError> {
    use rustix::fs::Mode;
    use rustix::shm::OFlags;
    let name = object_name(name);
    // A stale object from a crashed earlier process is removed first; the name is per user.
    let _ = rustix::shm::unlink(name.as_str());
    // structural: allow — shm_open creates a kernel object in RAM, not a filesystem entry (D-10).
    let fd = rustix::shm::open(
      name.as_str(),
      // structural: allow — flags of the shared-memory object, not of a file.
      OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
      Mode::RUSR | Mode::WUSR,
    )
    .map_err(|e| refused("shm_open", e))?;
    Ok(Created { fd, name })
  }

  #[cfg(target_os = "macos")]
  fn open_object(handoff: &Handoff) -> Result<OwnedFd, MemError> {
    use rustix::fs::Mode;
    use rustix::shm::OFlags;
    match handoff {
      // structural: allow — opening the shared-memory object another process created (D-10).
      Handoff::Name(name) => rustix::shm::open(
        name.as_str(),
        // structural: allow — the object's access mode, not a file's.
        OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
      )
      .map_err(|e| refused("shm_open", e)),
      Handoff::Descriptor(_) => Err(MemError::OsRefused {
        call: "handoff",
        code: None,
      }),
    }
  }

  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  fn create_object(_name: &str) -> Result<Created, MemError> {
    Err(MemError::OsRefused {
      call: "memory object",
      code: None,
    })
  }

  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  fn open_object(_handoff: &Handoff) -> Result<OwnedFd, MemError> {
    Err(MemError::OsRefused {
      call: "memory object",
      code: None,
    })
  }

  impl Inner {
    pub(super) fn bytes(&self) -> &[u8] {
      &self.map
    }

    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
      &mut self.map
    }

    pub(super) fn lock(&mut self) -> Result<(), MemError> {
      self.map.lock().map_err(|e| MemError::OsRefused {
        call: "mlock",
        code: e.raw_os_error(),
      })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn handoff(&self) -> Result<Handoff, MemError> {
      use std::os::fd::{AsFd, IntoRawFd};
      // A duplicate without CLOEXEC, for a child to inherit; the number is what it receives.
      let dup = rustix::io::dup(self.fd.as_fd()).map_err(|e| refused("dup", e))?;
      rustix::io::fcntl_setfd(&dup, rustix::io::FdFlags::empty())
        .map_err(|e| refused("fcntl", e))?;
      Ok(Handoff::Descriptor(dup.into_raw_fd()))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn handoff(&self) -> Result<Handoff, MemError> {
      let _ = &self.fd;
      if self.name.is_empty() {
        return Err(MemError::OsRefused {
          call: "handoff of an object opened without a name",
          code: None,
        });
      }
      Ok(Handoff::Name(self.name.clone()))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn handoff(&self) -> Result<Handoff, MemError> {
      let _ = &self.fd;
      Err(MemError::OsRefused {
        call: "handoff",
        code: None,
      })
    }
  }
}

#[cfg(windows)]
mod platform {
  use std::ffi::c_void;

  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
  use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualLock,
  };

  use super::Handoff;
  use crate::error::MemError;

  /// The section and its view. The view is kept as its exposed address, not a pointer, so
  /// the object is `Send` (a mapping belongs to the process, not a thread) without an unsafe
  /// impl; the pointer is recovered with the provenance the exposure recorded.
  pub(super) struct Inner {
    handle: usize,
    view: usize,
    len: usize,
    name: String,
  }

  impl Inner {
    fn view_ptr(&self) -> *mut c_void {
      std::ptr::with_exposed_provenance_mut(self.view)
    }

    fn handle(&self) -> HANDLE {
      std::ptr::with_exposed_provenance_mut(self.handle)
    }
  }

  impl Drop for Inner {
    fn drop(&mut self) {
      // SAFETY: the view and handle were created or opened by this module and are ours.
      unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
          Value: self.view_ptr(),
        });
        CloseHandle(self.handle());
      }
    }
  }

  fn wide(name: &str) -> Vec<u16> {
    format!("Local\\{name}")
      .encode_utf16()
      .chain(std::iter::once(0))
      .collect()
  }

  fn os(call: &'static str) -> MemError {
    MemError::OsRefused {
      call,
      code: std::io::Error::last_os_error().raw_os_error(),
    }
  }

  fn view(handle: HANDLE, len: usize) -> Result<*mut c_void, MemError> {
    // SAFETY: a full read/write view of a section handle this module owns.
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
    if view.Value.is_null() {
      let err = os("MapViewOfFile");
      // SAFETY: the handle is ours.
      unsafe { CloseHandle(handle) };
      return Err(err);
    }
    Ok(view.Value)
  }

  pub(super) fn handoff_for_name(name: &str) -> Option<Handoff> {
    Some(Handoff::Name(name.to_owned()))
  }

  pub(super) fn create(name: &str, len: usize) -> Result<Inner, MemError> {
    let size = u64::try_from(len).map_err(|_| MemError::OsRefused {
      call: "CreateFileMappingW",
      code: None,
    })?;
    let high = u32::try_from(size >> u32::BITS).unwrap_or(u32::MAX);
    let low = u32::try_from(size & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    let wide = wide(name);
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
      return Err(os("CreateFileMappingW"));
    }
    let view = view(handle, len)?;
    Ok(Inner {
      handle: handle.expose_provenance(),
      view: view.expose_provenance(),
      len,
      name: name.to_owned(),
    })
  }

  pub(super) fn open(handoff: &Handoff, len: usize) -> Result<Inner, MemError> {
    let Handoff::Name(name) = handoff else {
      return Err(MemError::OsRefused {
        call: "handoff",
        code: None,
      });
    };
    let wide = wide(name);
    // SAFETY: a NUL-terminated wide name of a section in this session's namespace.
    let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr()) };
    if handle.is_null() {
      return Err(os("OpenFileMappingW"));
    }
    let view = view(handle, len)?;
    Ok(Inner {
      handle: handle.expose_provenance(),
      view: view.expose_provenance(),
      len,
      name: name.clone(),
    })
  }

  impl Inner {
    pub(super) fn bytes(&self) -> &[u8] {
      // SAFETY: the view is `len` readable bytes for as long as `self` lives.
      unsafe { std::slice::from_raw_parts(self.view_ptr().cast::<u8>(), self.len) }
    }

    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
      // SAFETY: the view is `len` writable bytes for as long as `self` lives, and `&mut self`
      // is the only borrow of them in this process.
      unsafe { std::slice::from_raw_parts_mut(self.view_ptr().cast::<u8>(), self.len) }
    }

    pub(super) fn lock(&mut self) -> Result<(), MemError> {
      // SAFETY: the view is ours and `len` bytes long.
      let ok = unsafe { VirtualLock(self.view_ptr(), self.len) };
      if ok == 0 {
        return Err(os("VirtualLock"));
      }
      Ok(())
    }

    pub(super) fn handoff(&self) -> Result<Handoff, MemError> {
      Ok(Handoff::Name(self.name.clone()))
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::Ordering;

  use super::*;

  /// The object is created without a path, written through its bytes, and read back through
  /// a second mapping of the same object within this process (the cross-process shape).
  #[test]
  fn a_shared_object_is_seen_through_a_second_mapping() {
    let mut a = SharedObject::create("slates-mem-shared-test-a", 4096).unwrap();
    a.bytes_mut()[100..104].copy_from_slice(&[1, 2, 3, 4]);
    let word = a.atomic_u64(8).unwrap();
    word.store(0xdead_beef, Ordering::Release);
    #[cfg(target_os = "linux")]
    let b = {
      let handoff = a.handoff().unwrap();
      SharedObject::open(&handoff, 4096).unwrap()
    };
    #[cfg(not(target_os = "linux"))]
    let b = {
      let handoff = a.handoff().unwrap();
      SharedObject::open(&handoff, 4096).unwrap()
    };
    assert_eq!(&b.bytes()[100..104], &[1, 2, 3, 4]);
    assert_eq!(
      b.atomic_u64(8).unwrap().load(Ordering::Acquire),
      0xdead_beef
    );
    assert!(a.atomic_u64(3).is_err(), "misaligned");
    assert!(a.atomic_u64(4096).is_err(), "past the end");
    assert!(a.atomic_u32(4).is_ok());
  }
}
