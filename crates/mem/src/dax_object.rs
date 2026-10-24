//! A file's DAX object: the memory a VMM maps one file's DAX ranges from (§4.6, the Shards route; A-135;
//! `docs/wip/shards/dax-protocol.md`, agreed with the Shards owner 2026-10-24). One object per attachment and file is
//! handed to the VMM by descriptor. It holds the file's bytes at the file's own offsets and zeros everywhere else, so a
//! descriptor of it authorizes that one file's bytes and nothing of any other file or volume. A-135 is why it is a
//! copy: a descriptor authorizes its whole object, and the content arena holds every volume.
//!
//! **Sized once, to the largest file a guest can have** ([`DAX_OBJECT_BYTES`]), and never resized. Shards' DAX window
//! follows a mapped file's size: it maps the file's pages up to its end, and zeros past it. An object whose size never
//! changes leaves that machinery nothing to do, and no view of it, slates' or the VMM's, is ever cut short under its
//! holder.
//! - **Linux:** a `memfd` sized at creation and sealed against growing, shrinking and further seals (`F_SEAL_GROW`,
//!   `F_SEAL_SHRINK`, `F_SEAL_SEAL`). The resize seals still let a hole be punched; `mm/shmem.c` refuses a punch only
//!   under the write seals.
//! - **macOS:** a POSIX shared-memory object under a random name that is unlinked at once, so nothing reaches it by
//!   name, only by its descriptor. It is sized once, which is all macOS allows: a second `ftruncate` is refused
//!   `EINVAL`.
//! - **Elsewhere:** refused by kind. The route hands descriptors, which Windows does not have.
//!
//! Measured on this Mac (macOS 26.4.1, Apple silicon, 2026-10-24):
//! - every size up to `2^63 − 2 MiB` was accepted, and its last 2 MiB mapped, took a byte and read it back;
//! - a 64 GiB object's footprint stayed flat until touched;
//! - a page is charged to the process that touches it: 259 MB in a child that touched 256 MiB, 13 MB in the creator.
//!
//! A memfd takes `MAX_LFS_FILESIZE`, as a FUSE file does (`mm/shmem.c`, `fs/fuse/inode.c`).
//!
//! **Reached only by copy, through atomics.** The VMM's guest stores into the object at any moment, so every byte is
//! racy. slates loads and stores it only through relaxed `AtomicU64` words and `AtomicU8` bytes, never through a
//! reference to its bytes (the rule [`crate::shared`] keeps, AUD-29-09). It never uses `write(2)` either, which on
//! Linux would wake Shards' inotify watch once per call.
//!
//! **Viewed a range at a time.** An object of 2^63 bytes cannot be mapped whole, so slates maps the ranges it touches,
//! [`DAX_RANGE_BYTES`] each (the ranges a guest maps). It holds at most as many at once as its owner allows
//! ([`DaxObject::create`]), and unmaps each when its owner says ([`DaxObject::release_view`]).
//!
//! **Zeroing commits no fresh memory.**
//! - **Linux** punches the hole through slates' own view with `MADV_REMOVE`, which the kernel carries out as a
//!   `vfs_fallocate`. `fallocate` itself is a file write, allowed only in `slates-land` (R1).
//! - **macOS** has no hole punch for shared memory, so it zeroes only the pages that hold something: those `mincore`
//!   marks resident or paged out.

/// Format: the range a Linux guest maps a file in, and the unit of a DAX object's views: `FUSE_DAX_SZ`, 2 MiB
/// (`fs/fuse/dax.c`; Shards' `window::RANGE`).
pub const DAX_RANGE_BYTES: u64 = 2 * 1024 * 1024;

/// Format: a DAX object's size, fixed at creation. It is the largest whole number of ranges below `2^63`, the least
/// size no `off_t` reaches.
/// - It covers every offset of a Linux guest's file but the last range below `2^63`: `MAX_LFS_FILESIZE` is
///   `2^63 − 1` (`fs/fuse/inode.c`).
/// - Both platforms take it sparse: measured on macOS 26.4.1; Linux's shmem allows `MAX_LFS_FILESIZE` (`mm/shmem.c`).
pub const DAX_OBJECT_BYTES: u64 = match (1u64 << 63).checked_sub(DAX_RANGE_BYTES) {
  Some(bytes) => bytes,
  None => 0,
};

pub use imp::DaxObject;

#[cfg(unix)]
mod imp {
  use std::collections::BTreeMap;
  use std::collections::btree_map::Entry;
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
  use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

  use super::{DAX_OBJECT_BYTES, DAX_RANGE_BYTES};
  use crate::error::MemError;
  use crate::shared::RangeView;

  /// The span `offset .. offset + len` inside a DAX object, as its end; refused `OutOfRange` past the object.
  fn span_end(offset: u64, len: u64) -> Result<u64, MemError> {
    match offset.checked_add(len) {
      Some(end) if end <= DAX_OBJECT_BYTES => Ok(end),
      _ => Err(MemError::OutOfRange {
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        len: usize::try_from(DAX_OBJECT_BYTES).unwrap_or(usize::MAX),
      }),
    }
  }

  /// The range an offset falls in, and where in it.
  fn range_of(offset: u64) -> Option<(u64, u64)> {
    Some((
      offset.checked_div(DAX_RANGE_BYTES)?,
      offset.checked_rem(DAX_RANGE_BYTES)?,
    ))
  }

  /// Shape: the bytes of the word the racy copies move at once.
  const WORD_BYTES: usize = size_of::<u64>();

  fn refused(call: &'static str, e: rustix::io::Errno) -> MemError {
    MemError::OsRefused {
      call,
      code: Some(e.raw_os_error()),
    }
  }

  /// A file's DAX object and the views of it slates holds: one owner's (the shard serving its attachment), which
  /// reaches the object's bytes only with `&mut self`.
  pub struct DaxObject {
    fd: OwnedFd,
    views: BTreeMap<u64, RangeView>,
    most_views: usize,
  }

  impl std::fmt::Debug for DaxObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.debug_struct("DaxObject")
        .field("views", &self.views.len())
        .field("most_views", &self.most_views)
        .finish()
    }
  }

  impl DaxObject {
    /// A new object of [`DAX_OBJECT_BYTES`], all zeros, of which slates may map at most `most_views` ranges at once.
    pub fn create(most_views: usize) -> Result<DaxObject, MemError> {
      Ok(DaxObject {
        fd: platform::create_object()?,
        views: BTreeMap::new(),
        most_views,
      })
    }

    /// The object's descriptor, to hand the VMM a duplicate of (`SCM_RIGHTS` duplicates what it carries).
    pub fn descriptor(&self) -> BorrowedFd<'_> {
      self.fd.as_fd()
    }

    /// How many ranges slates maps now.
    pub fn views(&self) -> usize {
      self.views.len()
    }

    /// Unmaps slates' view of range `range`, if it holds one; the object keeps its bytes. Whether a view went.
    pub fn release_view(&mut self, range: u64) -> bool {
      self.views.remove(&range).is_some()
    }

    /// Copies `from` into the object at `offset`, through atomics. Refused `OutOfRange` past the object and
    /// `ViewsExhausted` where a range it needs cannot be viewed; a refusal part-way leaves the earlier ranges written.
    pub fn store(&mut self, offset: u64, from: &[u8]) -> Result<(), MemError> {
      let len = u64::try_from(from.len()).map_err(|_| MemError::OutOfRange {
        offset: usize::MAX,
        len: from.len(),
      })?;
      span_end(offset, len)?;
      let mut done = 0usize;
      while done < from.len() {
        let at = offset.saturating_add(u64::try_from(done).unwrap_or(u64::MAX));
        let (range, within, take) = piece(at, from.len().saturating_sub(done))?;
        let bytes = from
          .get(done..done.saturating_add(take))
          .ok_or(MemError::OutOfRange {
            offset: done,
            len: from.len(),
          })?;
        self.view(range)?.store(within, bytes)?;
        done = done.saturating_add(take);
      }
      Ok(())
    }

    /// Copies the object's bytes at `offset` into `into`, through atomics. Refused as [`DaxObject::store`] is.
    pub fn load(&mut self, offset: u64, into: &mut [u8]) -> Result<(), MemError> {
      let len = u64::try_from(into.len()).map_err(|_| MemError::OutOfRange {
        offset: usize::MAX,
        len: into.len(),
      })?;
      span_end(offset, len)?;
      let total = into.len();
      let mut done = 0usize;
      while done < total {
        let at = offset.saturating_add(u64::try_from(done).unwrap_or(u64::MAX));
        let (range, within, take) = piece(at, total.saturating_sub(done))?;
        let bytes = into
          .get_mut(done..done.saturating_add(take))
          .ok_or(MemError::OutOfRange {
            offset: done,
            len: total,
          })?;
        self.view(range)?.load(within, bytes)?;
        done = done.saturating_add(take);
      }
      Ok(())
    }

    /// Makes the object's bytes `offset .. offset + len` read zero without committing fresh memory: whole pages are
    /// given back (Linux) or zeroed only where they hold something (macOS), and the partial pages at the ends are
    /// zeroed through atomics. Refused as [`DaxObject::store`] is, and `OsRefused` where the OS refuses the punch.
    pub fn zero(&mut self, offset: u64, len: u64) -> Result<(), MemError> {
      let end = span_end(offset, len)?;
      let page = u64::try_from(rustix::param::page_size().max(1)).unwrap_or(u64::MAX);
      let mut at = offset;
      while at < end {
        let left = usize::try_from(end.saturating_sub(at)).unwrap_or(usize::MAX);
        let (range, within, take) = piece(at, left)?;
        let within_end = within.saturating_add(take);
        let view = self.view_mut(range)?;
        // The whole pages of this piece, and the partial ones at its ends.
        let first_page = u64::try_from(within)
          .unwrap_or(u64::MAX)
          .checked_next_multiple_of(page)
          .and_then(|p| usize::try_from(p).ok())
          .unwrap_or(within_end)
          .min(within_end);
        let page_bytes = usize::try_from(page).unwrap_or(usize::MAX);
        let last_page = within_end
          .checked_div(page_bytes)
          .and_then(|pages| pages.checked_mul(page_bytes))
          .unwrap_or(first_page)
          .max(first_page);
        view.zero_bytes(within, first_page)?;
        platform::zero_pages(view, first_page, last_page)?;
        view.zero_bytes(last_page, within_end)?;
        at = at.saturating_add(u64::try_from(take).unwrap_or(u64::MAX));
      }
      Ok(())
    }

    /// The view of range `range`, mapped now if slates holds none, within the owner's bound.
    fn view(&mut self, range: u64) -> Result<&RangeView, MemError> {
      self.view_mut(range).map(|view| &*view)
    }

    /// As [`DaxObject::view`], mutably (for the page-giving `madvise`).
    fn view_mut(&mut self, range: u64) -> Result<&mut RangeView, MemError> {
      let full = self.views.len() >= self.most_views;
      let most = self.most_views;
      match self.views.entry(range) {
        Entry::Occupied(held) => Ok(held.into_mut()),
        Entry::Vacant(_) if full => Err(MemError::ViewsExhausted { most }),
        Entry::Vacant(slot) => {
          let offset = range
            .checked_mul(DAX_RANGE_BYTES)
            .filter(|start| start.saturating_add(DAX_RANGE_BYTES) <= DAX_OBJECT_BYTES)
            .and_then(|start| usize::try_from(start).ok())
            .ok_or(MemError::OutOfRange {
              offset: usize::try_from(range).unwrap_or(usize::MAX),
              len: usize::try_from(DAX_OBJECT_BYTES).unwrap_or(usize::MAX),
            })?;
          let len = usize::try_from(DAX_RANGE_BYTES).map_err(|_| MemError::OsRefused {
            call: "mmap",
            code: None,
          })?;
          Ok(slot.insert(RangeView::map(&self.fd, offset, len)?))
        }
      }
    }
  }

  /// The piece of a copy at object offset `at` with `left` bytes to go: its range, where in the range, and how many
  /// bytes it takes (to the range's end at most).
  fn piece(at: u64, left: usize) -> Result<(u64, usize, usize), MemError> {
    let (range, within) = range_of(at).ok_or(MemError::OutOfRange {
      offset: usize::MAX,
      len: left,
    })?;
    let room = DAX_RANGE_BYTES.saturating_sub(within);
    let take = usize::try_from(room).unwrap_or(usize::MAX).min(left);
    let within = usize::try_from(within).map_err(|_| MemError::OutOfRange {
      offset: usize::MAX,
      len: left,
    })?;
    Ok((range, within, take))
  }

  /// The racy accesses to a view: every byte through an atomic, words where the offset is aligned.
  trait Racy {
    fn word(&self, at: usize) -> Option<&AtomicU64>;
    fn byte(&self, at: usize) -> Option<&AtomicU8>;
    fn store(&self, at: usize, from: &[u8]) -> Result<(), MemError>;
    fn load(&self, at: usize, into: &mut [u8]) -> Result<(), MemError>;
    fn zero_bytes(&self, from: usize, to: usize) -> Result<(), MemError>;
  }

  impl Racy for RangeView {
    /// The word at `at`, where `at` is word-aligned and the word lies inside the view.
    fn word(&self, at: usize) -> Option<&AtomicU64> {
      if !at.is_multiple_of(WORD_BYTES) || at.checked_add(WORD_BYTES)? > self.len() {
        return None;
      }
      // SAFETY: `at .. at + 8` lies inside this view's live mapping (checked above), which stays mapped while `self`
      // is borrowed; the view's base is page-aligned, so `at` word-aligned makes the address aligned for
      // `AtomicU64`, valid for every bit pattern. Every access slates makes to the object's bytes is atomic, and no
      // reference to them exists: the VMM's stores come from another process, outside this one's memory model.
      Some(unsafe { &*self.base().wrapping_add(at).cast::<AtomicU64>() })
    }

    /// The byte at `at`, where it lies inside the view.
    fn byte(&self, at: usize) -> Option<&AtomicU8> {
      if at >= self.len() {
        return None;
      }
      // SAFETY: as in `word`; `AtomicU8` has no alignment requirement.
      Some(unsafe { &*self.base().wrapping_add(at).cast::<AtomicU8>() })
    }

    fn store(&self, at: usize, from: &[u8]) -> Result<(), MemError> {
      let refused = MemError::OutOfRange {
        offset: at,
        len: self.len(),
      };
      let mut done = 0usize;
      while done < from.len() {
        let here = at.checked_add(done).ok_or(refused.clone())?;
        let word = (here.is_multiple_of(WORD_BYTES)
          && from.len().saturating_sub(done) >= WORD_BYTES)
          .then(|| from.get(done..done.saturating_add(WORD_BYTES)))
          .flatten()
          .and_then(|bytes| <[u8; WORD_BYTES]>::try_from(bytes).ok());
        if let Some(bytes) = word {
          self
            .word(here)
            .ok_or(refused.clone())?
            .store(u64::from_ne_bytes(bytes), Ordering::Relaxed);
          done = done.saturating_add(WORD_BYTES);
        } else {
          let byte = from.get(done).copied().ok_or(refused.clone())?;
          self
            .byte(here)
            .ok_or(refused.clone())?
            .store(byte, Ordering::Relaxed);
          done = done.saturating_add(1);
        }
      }
      Ok(())
    }

    fn load(&self, at: usize, into: &mut [u8]) -> Result<(), MemError> {
      let refused = MemError::OutOfRange {
        offset: at,
        len: self.len(),
      };
      let total = into.len();
      let mut done = 0usize;
      while done < total {
        let here = at.checked_add(done).ok_or(refused.clone())?;
        if here.is_multiple_of(WORD_BYTES) && total.saturating_sub(done) >= WORD_BYTES {
          let value = self
            .word(here)
            .ok_or(refused.clone())?
            .load(Ordering::Relaxed);
          into
            .get_mut(done..done.saturating_add(WORD_BYTES))
            .ok_or(refused.clone())?
            .copy_from_slice(&value.to_ne_bytes());
          done = done.saturating_add(WORD_BYTES);
        } else {
          let value = self
            .byte(here)
            .ok_or(refused.clone())?
            .load(Ordering::Relaxed);
          *into.get_mut(done).ok_or(refused.clone())? = value;
          done = done.saturating_add(1);
        }
      }
      Ok(())
    }

    fn zero_bytes(&self, from: usize, to: usize) -> Result<(), MemError> {
      /// Shape: the zeros one store takes from (a page on every target slates builds for).
      const ZEROS: [u8; 4096] = [0; 4096];
      let mut at = from;
      while at < to {
        let take = to.saturating_sub(at).min(ZEROS.len());
        self.store(at, ZEROS.get(..take).unwrap_or_default())?;
        at = at.saturating_add(take);
      }
      Ok(())
    }
  }

  #[cfg(target_os = "linux")]
  mod platform {
    use std::os::fd::OwnedFd;

    use super::{DAX_OBJECT_BYTES, Racy, refused};
    use crate::error::MemError;
    use crate::shared::RangeView;

    /// A memfd sized to [`DAX_OBJECT_BYTES`] and sealed against any change of size and any further seal.
    pub(super) fn create_object() -> Result<OwnedFd, MemError> {
      let fd = rustix::fs::memfd_create(
        "slates-dax",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
      )
      .map_err(|e| refused("memfd_create", e))?;
      #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
      // structural: allow — sizing the memory object just created (no filesystem entry; D-10).
      rustix::fs::ftruncate(&fd, DAX_OBJECT_BYTES).map_err(|e| refused("ftruncate", e))?;
      rustix::fs::fcntl_add_seals(
        &fd,
        rustix::fs::SealFlags::GROW | rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::SEAL,
      )
      .map_err(|e| refused("fcntl_add_seals", e))?;
      Ok(fd)
    }

    /// Gives the whole pages `from .. to` of `view` back to the object, which then reads zeros there and holds no
    /// memory for them (`MADV_REMOVE` over a shared mapping punches the object's hole).
    pub(super) fn zero_pages(view: &mut RangeView, from: usize, to: usize) -> Result<(), MemError> {
      if from >= to {
        return Ok(());
      }
      let refused = MemError::OsRefused {
        call: "madvise(MADV_REMOVE)",
        code: None,
      };
      // Under Miri no page goes back (it models no `madvise`): zeroed through atomics instead.
      if cfg!(miri) {
        return view.zero_bytes(from, to);
      }
      let pages = view.pages_mut().get_mut(from..to).ok_or(refused.clone())?;
      match crate::region::discard_slice(pages, true) {
        crate::region::Discarded::Hole => Ok(()),
        _ => Err(refused),
      }
    }
  }

  #[cfg(target_os = "macos")]
  mod platform {
    use std::os::fd::OwnedFd;

    use super::{DAX_OBJECT_BYTES, Racy, refused};
    use crate::error::MemError;
    use crate::shared::RangeView;

    /// Format: the prefix of a DAX object's name: short, so the random part fits `shm_open`'s limit beside it.
    const NAME_PREFIX: &str = "/sdax";
    /// Shape: the random bytes of a DAX object's name: 96 bits, so no other process guesses the name in the moment
    /// it exists. Written as 24 hex digits after the 5-character prefix: 29 characters, inside [`NAME_LIMIT`].
    const NAME_RANDOM_BYTES: usize = 12;
    /// Format: the POSIX shared-memory name limit on macOS (`PSHMNAMLEN`, 31).
    const NAME_LIMIT: usize = 31;
    /// Format: the hex digits a random byte is written as.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    /// Format: `mincore`'s mark of a page that was paged out (compressed or swapped), which still holds its bytes:
    /// `MINCORE_PAGED_OUT`, 0x20 in the macOS SDK's `sys/mman.h` (226-233), which the `libc` crate does not define.
    const MINCORE_PAGED_OUT: i32 = 0x20;

    /// A name no other process can guess: the prefix and random bytes from the OS.
    fn random_name() -> Result<String, MemError> {
      let mut random = [0u8; NAME_RANDOM_BYTES];
      // SAFETY: `getentropy` fills at most 256 bytes of the buffer it is given; this one is this function's own and
      // `NAME_RANDOM_BYTES` long.
      let failed = unsafe { libc::getentropy(random.as_mut_ptr().cast(), random.len()) };
      if failed != 0 {
        return Err(MemError::OsRefused {
          call: "getentropy",
          code: std::io::Error::last_os_error().raw_os_error(),
        });
      }
      let mut name = String::with_capacity(NAME_LIMIT);
      name.push_str(NAME_PREFIX);
      for byte in random {
        for nibble in [byte >> 4, byte & 0x0f] {
          let digit = HEX.get(usize::from(nibble)).copied().unwrap_or(b'0');
          name.push(char::from(digit));
        }
      }
      if name.len() > NAME_LIMIT {
        return Err(MemError::OsRefused {
          call: "shm_open",
          code: None,
        });
      }
      Ok(name)
    }

    /// A shared-memory object sized to [`DAX_OBJECT_BYTES`], reached from now on by its descriptor alone.
    pub(super) fn create_object() -> Result<OwnedFd, MemError> {
      use rustix::fs::Mode;
      use rustix::shm::OFlags;
      let name = random_name()?;
      // structural: allow — shm_open creates a kernel object in RAM, not a filesystem entry (D-10).
      let fd = rustix::shm::open(
        name.as_str(),
        // structural: allow — flags of the shared-memory object, not of a file.
        OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
      )
      .map_err(|e| refused("shm_open", e))?;
      // The name goes at once: the object is reached by its descriptor alone, so no other process of this user can
      // open it and read what slates fills in (AC-2.13 keeps consumers sharing a uid apart).
      rustix::shm::unlink(name.as_str()).map_err(|e| refused("shm_unlink", e))?;
      #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
      // structural: allow — sizing the memory object just created (no filesystem entry; D-10).
      rustix::fs::ftruncate(&fd, DAX_OBJECT_BYTES).map_err(|e| refused("ftruncate", e))?;
      Ok(fd)
    }

    /// Zeroes, among the whole pages `from .. to` of `view`, those that hold something: `mincore` marks them resident
    /// or paged out (a compressed or swapped page still holds its bytes). A page never touched stays untouched, so no
    /// memory is committed for it.
    pub(super) fn zero_pages(view: &mut RangeView, from: usize, to: usize) -> Result<(), MemError> {
      if from >= to {
        return Ok(());
      }
      let page = rustix::param::page_size().max(1);
      let pages = to.saturating_sub(from).div_ceil(page);
      let mut marks = vec![0 as libc::c_char; pages];
      // SAFETY: `from .. to` lies inside this view's live mapping (the caller passes whole pages of the view), and
      // `marks` holds one byte per page of it, which is what `mincore` writes; the call reads and writes no byte of
      // the mapping.
      let failed = unsafe {
        libc::mincore(
          view.base().wrapping_add(from).cast(),
          to.saturating_sub(from),
          marks.as_mut_ptr(),
        )
      };
      if failed != 0 {
        return Err(MemError::OsRefused {
          call: "mincore",
          code: std::io::Error::last_os_error().raw_os_error(),
        });
      }
      let holds = libc::MINCORE_INCORE | MINCORE_PAGED_OUT;
      for (index, mark) in marks.iter().enumerate() {
        if i32::from(*mark) & holds == 0 {
          continue;
        }
        let start = index.saturating_mul(page).saturating_add(from);
        view.zero_bytes(start, start.saturating_add(page).min(to))?;
      }
      Ok(())
    }
  }

  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  mod platform {
    use std::os::fd::OwnedFd;

    use crate::error::MemError;
    use crate::shared::RangeView;

    /// No DAX object on this platform: refused by kind.
    pub(super) fn create_object() -> Result<OwnedFd, MemError> {
      Err(MemError::OsRefused {
        call: "dax object",
        code: None,
      })
    }

    /// Never reached: no object is ever created here.
    pub(super) fn zero_pages(
      _view: &mut RangeView,
      _from: usize,
      _to: usize,
    ) -> Result<(), MemError> {
      Err(MemError::OsRefused {
        call: "dax object",
        code: None,
      })
    }
  }
}

#[cfg(not(unix))]
mod imp {
  use crate::error::MemError;

  /// No DAX object exists off Unix: the Shards route hands descriptors, which Windows does not have, so an object is
  /// never created and none of these methods can run.
  #[derive(Debug)]
  pub struct DaxObject {
    never: std::convert::Infallible,
  }

  impl DaxObject {
    /// Refused by kind: there is no DAX object here.
    pub fn create(_most_views: usize) -> Result<DaxObject, MemError> {
      Err(MemError::OsRefused {
        call: "dax object",
        code: None,
      })
    }

    /// Never reached: no object exists.
    pub fn views(&self) -> usize {
      match self.never {}
    }

    /// Never reached: no object exists.
    pub fn release_view(&mut self, _range: u64) -> bool {
      match self.never {}
    }

    /// Never reached: no object exists.
    pub fn store(&mut self, _offset: u64, _from: &[u8]) -> Result<(), MemError> {
      match self.never {}
    }

    /// Never reached: no object exists.
    pub fn load(&mut self, _offset: u64, _into: &mut [u8]) -> Result<(), MemError> {
      match self.never {}
    }

    /// Never reached: no object exists.
    pub fn zero(&mut self, _offset: u64, _len: u64) -> Result<(), MemError> {
      match self.never {}
    }
  }
}
