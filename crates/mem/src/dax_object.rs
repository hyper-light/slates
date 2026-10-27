//! A file's DAX object: the memory a VMM maps one file's DAX ranges from (§4.6, the Shards route; A-135;
//! `docs/wip/shards/dax-protocol.md`, agreed with the Shards owner 2026-10-24). One object per attachment and file is
//! handed to the VMM by descriptor. It holds the file's bytes at the file's own offsets and zeros everywhere else, so a
//! descriptor of it authorizes that one file's bytes and nothing of any other file or volume. A-135 is why it is a
//! copy: a descriptor authorizes its whole object, and the content arena holds every volume. The same reason makes the
//! same-kernel direct map hand a copy too, its own shape below ([`PeerObject`], A-171).
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
//! - **Windows** (plan G-shards-windows step 5): an unnamed pagefile-backed section under the peer section's confining
//!   descriptor, made reserved at [`dax_object_bytes`] (the machine's memory: a section's making costs in proportion
//!   to its size, measured) and committed a range at a time as slates views or prepares it, since a reserved page
//!   faults until committed. The VMM is handed a duplicate of no more access than its ask needs, which the
//!   descriptor lets no holder widen (A-175).
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
//! **A same-kernel peer's object** ([`PeerObject`], §4.6, A-171). The same-kernel direct map hands a client process the
//! descriptor of one file's copy, which the client maps read alone ([`PeerView`]). A descriptor carries its
//! whole object (A-135), so the object holds that file's bytes alone, at the file's own offsets, over the file's page
//! span when it was made (at least a page), with zeros past the file's end. It is sized once, at its creation, and the
//! client maps all of it.
//! - **Linux:** a sealable memfd. slates maps its one writable view of the whole object first, then seals it with
//!   `F_SEAL_FUTURE_WRITE`, `F_SEAL_SHRINK`, `F_SEAL_GROW` and `F_SEAL_SEAL`. The write seal refuses every writable map
//!   made after it, every `write` and every hole punch, whoever asks, slates included, while the view made before it
//!   keeps writing (`memfd_create(2)`: writes "via shared writable mappings that were created prior to the seal being
//!   set"). A read-only duplicate alone would be no defence: measured 2026-10-18 on 7.2.9, a peer reopens one `O_RDWR`
//!   through `/proc/self/fd` and maps it writable. Measured 2026-10-25 on fedora (`crates/client/tests/client.rs`): the
//!   reopen is granted, and its writable map and its `write` are refused `EPERM`, as are the handed descriptor's own; a
//!   read-only map turned writable is refused `EACCES`.
//! - **macOS:** an unlinked POSIX shared-memory object whose handed descriptor is a second one, opened read alone
//!   before the unlink. No name opens the object again, a writable shared map of a read-only descriptor is refused, and
//!   its size was set once, all macOS allows.
//! - **Windows:** an unnamed pagefile-backed section under a security descriptor that grants no one any right to it
//!   (`PEER_SECTION_SDDL`: `OWNER RIGHTS` may read the descriptor alone, which also cuts the owner's implicit
//!   `WRITE_DAC`). slates keeps the creator's handle, with every access, and its writable view; a peer is handed a
//!   read-only duplicate. Windows checks a duplicate against the descriptor only when it asks for more than its source
//!   holds, so the read-only duplicates work and a holder's duplicate that writes or that may rewrite the descriptor is
//!   refused `ERROR_ACCESS_DENIED`; measured 2026-10-25 on Windows 10.0.26200 (`tests/peer_section.rs`), where the
//!   default descriptor grants both (A-175). Zeroing is stores over the whole span: Windows tells no section's holes.
//!
//! Because the write seal refuses slates' own hole punch too, a peer object's zeroing is stores of zeros over what
//! holds bytes ([`PeerObject::zero`]): its cost is the bytes zeroed, where a DAX object's punch gives pages back.
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

/// The size of every DAX object this process makes, fixed for its life ([`DaxObject::create`]): nothing is written
/// through past it, and a change that would take a backed file past it is refused `FileTooLarge`.
/// - **Unix:** [`DAX_OBJECT_BYTES`], every offset a guest's file can have; both kernels take it sparse at no cost.
/// - **Windows** (plan G-shards-windows step 5): derived, the machine's physical memory in whole ranges, at least one.
///   A pagefile-backed section's making costs in proportion to its size, reserved or not: measured on jupiter
///   (Windows 10.0.26200, 2026-10-26) a reserved section of 2^48 bytes took 3.3 ms and 12,001 pages of commit to make,
///   2^53 took 129 ms and 358,807 pages, and 2^62 or more was refused `ERROR_NO_SYSTEM_RESOURCES`. A RAM-only volume's
///   file holds no more bytes than the machine has, so its memory covers every file but a hand-made sparse one, at
///   about 2^-22 of the size in commit (a few pages on a machine of 128 GiB).
#[must_use]
pub fn dax_object_bytes() -> u64 {
  #[cfg(windows)]
  {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
      slates_machine::facts::Facts::memory_total()
        .max(DAX_RANGE_BYTES)
        .checked_next_multiple_of(DAX_RANGE_BYTES)
        .unwrap_or(DAX_RANGE_BYTES)
        .min(DAX_OBJECT_BYTES)
    })
  }
  #[cfg(not(windows))]
  {
    DAX_OBJECT_BYTES
  }
}

/// The security descriptor every Windows peer section is made under (A-175), for the test that measures it.
#[cfg(windows)]
pub use imp::PEER_SECTION_SDDL;
pub use imp::{DaxObject, PeerObject, PeerView};

/// A live mapping's bytes reached only through atomics, the one discipline every view of a peer object or a DAX object
/// keeps on every platform (AUD-29-09): another process may store into the bytes at any moment, so no reference to them
/// is ever formed.
#[cfg(any(unix, windows))]
mod racy {
  use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

  use crate::error::MemError;

  /// Shape: the bytes of the word the racy copies move at once.
  const WORD_BYTES: usize = size_of::<u64>();

  /// A live mapping's bytes as racy memory, loaded through atomics: every byte through an atomic, words where the
  /// offset is aligned. Another process may store into the bytes at any moment, so no reference to them is ever formed.
  pub(super) trait Racy {
    /// The mapping's first byte, as the raw address it was mapped at.
    fn address(&self) -> *mut u8;

    /// The mapping's length in bytes.
    fn extent(&self) -> usize;

    /// The word at `at`, where `at` is word-aligned and the word lies inside the view.
    fn word(&self, at: usize) -> Option<&AtomicU64> {
      if !at.is_multiple_of(WORD_BYTES) || at.checked_add(WORD_BYTES)? > self.extent() {
        return None;
      }
      // SAFETY: `at .. at + 8` lies inside this view's live mapping (checked above), which stays mapped while `self`
      // is borrowed; the view's base is page-aligned, so `at` word-aligned makes the address aligned for
      // `AtomicU64`, valid for every bit pattern. Every access slates makes to the object's bytes is atomic, and no
      // reference to them exists: the other process's stores come from outside this one's memory model. A read-only
      // view is only loaded (it has no `RacyStore`), and a relaxed load is a plain load on the 64-bit targets slates
      // builds for (x86_64 and aarch64), which never writes the line.
      Some(unsafe { &*self.address().wrapping_add(at).cast::<AtomicU64>() })
    }

    /// The byte at `at`, where it lies inside the view.
    fn byte(&self, at: usize) -> Option<&AtomicU8> {
      if at >= self.extent() {
        return None;
      }
      // SAFETY: as in `word`; `AtomicU8` has no alignment requirement.
      Some(unsafe { &*self.address().wrapping_add(at).cast::<AtomicU8>() })
    }

    fn load(&self, at: usize, into: &mut [u8]) -> Result<(), MemError> {
      let refused = MemError::OutOfRange {
        offset: at,
        len: self.extent(),
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
  }

  /// A writable mapping's racy stores, through the same atomics as its loads.
  pub(super) trait RacyStore: Racy {
    fn store(&self, at: usize, from: &[u8]) -> Result<(), MemError> {
      let refused = MemError::OutOfRange {
        offset: at,
        len: self.extent(),
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
}

#[cfg(unix)]
mod imp {
  use std::collections::BTreeMap;
  use std::collections::btree_map::Entry;
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

  use super::racy::{Racy, RacyStore};
  use super::{DAX_OBJECT_BYTES, DAX_RANGE_BYTES};
  use crate::error::MemError;
  use crate::shared::{RangeView, ReadView};

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

    /// The object's size in bytes, which never changes: [`DAX_OBJECT_BYTES`].
    #[must_use]
    pub fn len(&self) -> u64 {
      DAX_OBJECT_BYTES
    }

    /// Never: an object is at least a range. Here for the convention beside [`DaxObject::len`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
      false
    }

    /// Readies range `range` for a VMM's map: nothing here, since a shared-memory page comes into being when it is
    /// first touched, by anyone (Windows commits the range, its arm's doc says why). Refused `OutOfRange` past the
    /// object.
    pub fn prepare_range(&mut self, range: u64) -> Result<(), MemError> {
      range
        .checked_mul(DAX_RANGE_BYTES)
        .and_then(|start| span_end(start, DAX_RANGE_BYTES).ok())
        .map(drop)
        .ok_or(MemError::OutOfRange {
          offset: usize::try_from(range).unwrap_or(usize::MAX),
          len: usize::try_from(DAX_OBJECT_BYTES).unwrap_or(usize::MAX),
        })
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

  /// One file's copy for a same-kernel peer, and slates' one writable view of it (§4.6, A-171): the object a client
  /// maps read alone. It is sized once, at its creation, to the file's page span ([`PeerObject::bytes_for`]), and
  /// slates reaches its bytes only with `&mut self`, through atomics, since the client loads them at any moment.
  pub struct PeerObject {
    fd: OwnedFd,
    /// The descriptor a peer is handed: a second one, opened read alone before the object's name went (macOS).
    #[cfg(target_os = "macos")]
    handed: OwnedFd,
    view: RangeView,
    len: u64,
  }

  impl std::fmt::Debug for PeerObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.debug_struct("PeerObject")
        .field("len", &self.len)
        .finish()
    }
  }

  impl PeerObject {
    /// The size of the object a file of `file_bytes` is copied into: its page span, and at least one page, so the
    /// object is never empty and a peer maps all of it. Refused `OutOfRange` where the span overflows.
    pub fn bytes_for(file_bytes: u64) -> Result<u64, MemError> {
      let page = u64::try_from(rustix::param::page_size().max(1)).unwrap_or(u64::MAX);
      file_bytes
        .max(1)
        .checked_next_multiple_of(page)
        .ok_or(MemError::OutOfRange {
          offset: usize::try_from(file_bytes).unwrap_or(usize::MAX),
          len: usize::MAX,
        })
    }

    /// A new object of zeros, [`PeerObject::bytes_for`] `file_bytes` long, named `name` where the OS shows names
    /// (`memfd:<name>` in `/proc` on Linux), with slates' writable view of all of it taken and every other writer
    /// refused: sealed against writes and resizing on Linux, handed read alone on macOS. Refused `OutOfRange` where the
    /// span does not fit an address, `OsRefused` where the OS refuses a step, and by kind where there is no route.
    pub fn create(name: &str, file_bytes: u64) -> Result<PeerObject, MemError> {
      let len = PeerObject::bytes_for(file_bytes)?;
      let view_len = usize::try_from(len).map_err(|_| MemError::OutOfRange {
        offset: 0,
        len: usize::MAX,
      })?;
      let made = platform::create_peer_object(name, len)?;
      // The view first, then the seals: the write seal refuses every writable map made after it, slates' own included,
      // and keeps working only the ones made before (`memfd_create(2)`, F_SEAL_FUTURE_WRITE).
      let view = RangeView::map(&made.fd, 0, view_len)?;
      platform::seal_peer_object(&made.fd)?;
      Ok(PeerObject {
        fd: made.fd,
        #[cfg(target_os = "macos")]
        handed: made.handed,
        view,
        len,
      })
    }

    /// The object's size in bytes, which never changes.
    #[must_use]
    pub fn len(&self) -> u64 {
      self.len
    }

    /// Never: an object is at least a page. Here for the convention beside [`PeerObject::len`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
      self.len == 0
    }

    /// The descriptor to hand a peer: the sealed object itself on Linux, the read-only second descriptor on macOS.
    #[must_use]
    pub fn descriptor(&self) -> BorrowedFd<'_> {
      #[cfg(target_os = "macos")]
      {
        self.handed.as_fd()
      }
      #[cfg(not(target_os = "macos"))]
      {
        self.fd.as_fd()
      }
    }

    /// Copies `from` into the object at `offset`, through atomics. Refused `OutOfRange` past the object.
    pub fn store(&mut self, offset: u64, from: &[u8]) -> Result<(), MemError> {
      let at = self.within(offset, from.len())?;
      self.view.store(at, from)
    }

    /// Copies the object's bytes at `offset` into `into`, through atomics. Refused `OutOfRange` past the object.
    pub fn load(&mut self, offset: u64, into: &mut [u8]) -> Result<(), MemError> {
      let at = self.within(offset, into.len())?;
      self.view.load(at, into)
    }

    /// Makes the object's bytes `offset .. offset + len` read zero. Refused `OutOfRange` past the object.
    ///
    /// By stores of zeros over what holds bytes: the write seal refuses a hole punch, slates' own included
    /// (`mm/shmem.c` refuses `FALLOC_FL_PUNCH_HOLE` under `F_SEAL_FUTURE_WRITE`), so the pages zeroed stay held. A page
    /// that holds nothing reads zero already and is passed over: Linux finds the object's data with `SEEK_DATA` and
    /// `SEEK_HOLE`, which count a swapped page as data; macOS asks `mincore`, as a DAX object's zeroing does.
    pub fn zero(&mut self, offset: u64, len: u64) -> Result<(), MemError> {
      let count = usize::try_from(len).map_err(|_| MemError::OutOfRange {
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        len: usize::MAX,
      })?;
      let from = self.within(offset, count)?;
      platform::zero_peer(&self.fd, &mut self.view, from, from.saturating_add(count))
    }

    /// `offset .. offset + count` as an offset into slates' view, refused `OutOfRange` past the object.
    fn within(&self, offset: u64, count: usize) -> Result<usize, MemError> {
      let refused = MemError::OutOfRange {
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        len: usize::try_from(self.len).unwrap_or(usize::MAX),
      };
      let end = u64::try_from(count)
        .ok()
        .and_then(|count| offset.checked_add(count))
        .ok_or(refused.clone())?;
      if end > self.len {
        return Err(refused);
      }
      usize::try_from(offset).map_err(|_| refused)
    }
  }

  /// A client's view of the peer object a daemon handed it (§4.6, A-171): the whole object, mapped read alone. The
  /// daemon writes it at any moment, since it follows the file, so its bytes are loaded through atomics and never
  /// borrowed.
  pub struct PeerView {
    fd: OwnedFd,
    view: ReadView,
    len: u64,
  }

  impl std::fmt::Debug for PeerView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.debug_struct("PeerView").field("len", &self.len).finish()
    }
  }

  impl PeerView {
    /// Maps the first `len` bytes of the object behind `fd`, read alone. Refused `OutOfRange` for an empty view or one
    /// longer than the object, which would fault past its end, and `OsRefused` where the OS refuses the map.
    pub fn map(fd: OwnedFd, len: u64) -> Result<PeerView, MemError> {
      let object = rustix::fs::fstat(&fd).map_err(|e| refused("fstat", e))?;
      let object_bytes = u64::try_from(object.st_size).unwrap_or(0);
      let view_len = usize::try_from(len)
        .ok()
        .filter(|bytes| *bytes > 0 && len <= object_bytes)
        .ok_or(MemError::OutOfRange {
          offset: usize::try_from(len).unwrap_or(usize::MAX),
          len: usize::try_from(object_bytes).unwrap_or(usize::MAX),
        })?;
      let view = ReadView::map(&fd, view_len)?;
      Ok(PeerView { fd, view, len })
    }

    /// The view's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
      self.len
    }

    /// Never: an empty view is refused. Here for the convention beside [`PeerView::len`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
      self.len == 0
    }

    /// The descriptor the view maps, as the daemon handed it.
    #[must_use]
    pub fn descriptor(&self) -> BorrowedFd<'_> {
      self.fd.as_fd()
    }

    /// Copies the view's bytes at `offset` into `into`, through atomics. Refused `OutOfRange` past the view.
    pub fn load(&self, offset: u64, into: &mut [u8]) -> Result<(), MemError> {
      let refused = MemError::OutOfRange {
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        len: usize::try_from(self.len).unwrap_or(usize::MAX),
      };
      let end = u64::try_from(into.len())
        .ok()
        .and_then(|count| offset.checked_add(count))
        .ok_or(refused.clone())?;
      if end > self.len {
        return Err(refused);
      }
      let at = usize::try_from(offset).map_err(|_| refused)?;
      self.view.load(at, into)
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

  impl Racy for RangeView {
    fn address(&self) -> *mut u8 {
      self.base()
    }

    fn extent(&self) -> usize {
      self.len()
    }
  }

  impl RacyStore for RangeView {}

  impl Racy for ReadView {
    fn address(&self) -> *mut u8 {
      self.base()
    }

    fn extent(&self) -> usize {
      self.len()
    }
  }

  /// A new peer object's descriptors: slates' own, and on macOS the read-only one a peer is handed.
  struct MadePeer {
    fd: OwnedFd,
    #[cfg(target_os = "macos")]
    handed: OwnedFd,
  }

  #[cfg(target_os = "linux")]
  mod platform {
    use std::os::fd::OwnedFd;

    use super::{DAX_OBJECT_BYTES, MadePeer, RacyStore, refused};
    use crate::error::MemError;
    use crate::shared::RangeView;

    /// A sealable memfd of `len` bytes named `name`, not yet sealed: slates maps its view before the write seal.
    pub(super) fn create_peer_object(name: &str, len: u64) -> Result<MadePeer, MemError> {
      let fd = rustix::fs::memfd_create(
        name,
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
      )
      .map_err(|e| refused("memfd_create", e))?;
      #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
      // structural: allow — sizing the memory object just created (no filesystem entry; D-10).
      rustix::fs::ftruncate(&fd, len).map_err(|e| refused("ftruncate", e))?;
      Ok(MadePeer { fd })
    }

    /// Seals a peer object against every later writer and any change of size or seals. The write seal is the
    /// "future" one, so the view slates took before it keeps writing (Linux 5.1 and later; an older kernel refuses
    /// the seal, and the object with it).
    pub(super) fn seal_peer_object(fd: &OwnedFd) -> Result<(), MemError> {
      rustix::fs::fcntl_add_seals(
        fd,
        rustix::fs::SealFlags::FUTURE_WRITE
          | rustix::fs::SealFlags::SHRINK
          | rustix::fs::SealFlags::GROW
          | rustix::fs::SealFlags::SEAL,
      )
      .map_err(|e| refused("fcntl_add_seals", e))
    }

    /// Zeroes `from .. to` of a peer object's view by stores over the object's data alone. `SEEK_DATA` and `SEEK_HOLE`
    /// find the data, a swapped page counting as data (`mm/shmem.c`); a hole reads zero already.
    pub(super) fn zero_peer(
      fd: &OwnedFd,
      view: &mut RangeView,
      from: usize,
      to: usize,
    ) -> Result<(), MemError> {
      // Under Miri no object answers `lseek`: the whole span is zeroed through atomics instead.
      if cfg!(miri) {
        return view.zero_bytes(from, to);
      }
      let end = u64::try_from(to).unwrap_or(u64::MAX);
      let mut at = u64::try_from(from).unwrap_or(u64::MAX);
      while at < end {
        let data = match rustix::fs::seek(fd, rustix::fs::SeekFrom::Data(at)) {
          Ok(data) => data,
          // No data from `at` to the object's end.
          Err(rustix::io::Errno::NXIO) => return Ok(()),
          Err(e) => return Err(refused("lseek(SEEK_DATA)", e)),
        };
        if data >= end {
          return Ok(());
        }
        let hole = rustix::fs::seek(fd, rustix::fs::SeekFrom::Hole(data))
          .map_err(|e| refused("lseek(SEEK_HOLE)", e))?
          .min(end);
        if hole <= data {
          // The kernel answered a hole at or before the data it found: nothing can be trusted past here, so the rest
          // is zeroed whole rather than left.
          return view.zero_bytes(usize::try_from(data).unwrap_or(to).min(to), to);
        }
        view.zero_bytes(
          usize::try_from(data).unwrap_or(to),
          usize::try_from(hole).unwrap_or(to),
        )?;
        at = hole;
      }
      Ok(())
    }

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

    use super::{DAX_OBJECT_BYTES, MadePeer, RacyStore, refused};
    use crate::error::MemError;
    use crate::shared::RangeView;

    /// A shared-memory object of `len` bytes reached from now on by its two descriptors alone: slates' own, read and
    /// write, and the one a peer is handed, opened read alone while the name still reached the object. A read-only
    /// descriptor of an unlinked object is all a peer can have: no name opens it again, a writable shared map of it is
    /// refused, and its size was set once, which is all macOS allows. `_name` is Linux's: here the name is random and
    /// goes at once.
    pub(super) fn create_peer_object(_name: &str, len: u64) -> Result<MadePeer, MemError> {
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
      // structural: allow — shm_open of the kernel object just created, read alone (no filesystem entry; D-10).
      let handed = rustix::shm::open(name.as_str(), OFlags::RDONLY, Mode::empty());
      // The name goes at once, whether or not the second open was granted, so no other process of this user can open
      // the object and nothing is left behind under its name.
      let unlinked = rustix::shm::unlink(name.as_str()).map_err(|e| refused("shm_unlink", e));
      let handed = handed.map_err(|e| refused("shm_open", e))?;
      unlinked?;
      #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
      // structural: allow — sizing the memory object just created (no filesystem entry; D-10).
      rustix::fs::ftruncate(&fd, len).map_err(|e| refused("ftruncate", e))?;
      Ok(MadePeer { fd, handed })
    }

    /// Nothing to seal on macOS: the peer's descriptor is read alone and the size was set once.
    pub(super) fn seal_peer_object(_fd: &OwnedFd) -> Result<(), MemError> {
      Ok(())
    }

    /// Zeroes `from .. to` of a peer object's view: the partial pages at the ends by stores, and among the whole pages
    /// those that hold something, as [`zero_pages`] finds them.
    pub(super) fn zero_peer(
      _fd: &OwnedFd,
      view: &mut RangeView,
      from: usize,
      to: usize,
    ) -> Result<(), MemError> {
      let page = rustix::param::page_size().max(1);
      let first_page = from.checked_next_multiple_of(page).unwrap_or(to).min(to);
      let last_page = to
        .checked_div(page)
        .and_then(|pages| pages.checked_mul(page))
        .unwrap_or(first_page)
        .max(first_page);
      view.zero_bytes(from, first_page)?;
      zero_pages(view, first_page, last_page)?;
      view.zero_bytes(last_page, to)
    }

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

    use super::MadePeer;
    use crate::error::MemError;
    use crate::shared::RangeView;

    /// No peer object on this platform: refused by kind.
    pub(super) fn create_peer_object(_name: &str, _len: u64) -> Result<MadePeer, MemError> {
      Err(MemError::OsRefused {
        call: "peer object",
        code: None,
      })
    }

    /// Never reached: no peer object is ever created here.
    pub(super) fn seal_peer_object(_fd: &OwnedFd) -> Result<(), MemError> {
      Err(MemError::OsRefused {
        call: "peer object",
        code: None,
      })
    }

    /// Never reached: no peer object is ever created here.
    pub(super) fn zero_peer(
      _fd: &OwnedFd,
      _view: &mut RangeView,
      _from: usize,
      _to: usize,
    ) -> Result<(), MemError> {
      Err(MemError::OsRefused {
        call: "peer object",
        code: None,
      })
    }

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

#[cfg(windows)]
mod imp {
  use std::collections::BTreeMap;
  use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

  use windows_sys::Win32::Foundation::{
    DuplicateHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
  };
  use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
  };
  use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
  use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEM_COMMIT, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, SEC_COMMIT, SEC_RESERVE,
    SECTION_QUERY, UnmapViewOfFile, VirtualAlloc, VirtualQuery,
  };
  use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
  use windows_sys::Win32::System::Threading::GetCurrentProcess;

  use super::racy::{Racy, RacyStore};
  use crate::error::MemError;

  /// A file's DAX object and the views of it slates holds (Windows, plan G-shards-windows step 5): an unnamed
  /// pagefile-backed section under [`PEER_SECTION_SDDL`], made reserved (`SEC_RESERVE`) at [`super::dax_object_bytes`]
  /// and committed a range at a time as slates views it. Reserved, a section charges no commit for its size; a page of
  /// it must be committed before any view of it is touched, so a range is committed whole when slates first views it
  /// ([`DaxObject::prepare_range`] for one the fill copies nothing into), and the VMM maps only ranges an ask has had
  /// filled. A commit is the section's, not the view's, so it outlives the view that made it. slates keeps the
  /// creator's handle, every access, and hands duplicates of no more access than an ask needs ([`DaxObject::handed`]);
  /// the descriptor refuses any duplicate of more (A-175's discipline).
  pub struct DaxObject {
    section: OwnedHandle,
    len: u64,
    views: BTreeMap<u64, WriteView>,
    most_views: usize,
  }

  impl std::fmt::Debug for DaxObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.debug_struct("DaxObject")
        .field("len", &self.len)
        .field("views", &self.views.len())
        .field("most_views", &self.most_views)
        .finish()
    }
  }

  impl DaxObject {
    /// A new object of [`super::dax_object_bytes`], all zeros and nothing committed, of which slates may map at most
    /// `most_views` ranges at once. Refused `OsRefused` where the OS refuses the section.
    pub fn create(most_views: usize) -> Result<DaxObject, MemError> {
      let len = super::dax_object_bytes();
      Ok(DaxObject {
        section: confined_section(len, SEC_RESERVE)?,
        len,
        views: BTreeMap::new(),
        most_views,
      })
    }

    /// The object's size in bytes, which never changes.
    #[must_use]
    pub fn len(&self) -> u64 {
      self.len
    }

    /// Never: an object is at least a range. Here for the convention beside [`DaxObject::len`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
      self.len == 0
    }

    /// A duplicate of the section's handle in this process that maps it read alone, or read and write where
    /// `writable`, and may ask its size (`SECTION_QUERY`): what a VMM is handed, duplicated again into its process.
    /// The section's own handle never leaves the object.
    pub fn handed(&self, writable: bool) -> Result<OwnedHandle, MemError> {
      let access = if writable {
        FILE_MAP_READ | FILE_MAP_WRITE | SECTION_QUERY
      } else {
        FILE_MAP_READ | SECTION_QUERY
      };
      duplicate_here(self.section.as_handle(), access)
    }

    /// How many ranges slates maps now.
    pub fn views(&self) -> usize {
      self.views.len()
    }

    /// Unmaps slates' view of range `range`, if it holds one; the object keeps its bytes and its commit. Whether a view
    /// went.
    pub fn release_view(&mut self, range: u64) -> bool {
      self.views.remove(&range).is_some()
    }

    /// Commits range `range` whole, so a view of it anywhere may touch any byte: a range an ask covers is prepared
    /// before the VMM is answered, though the fill copies nothing into it (a hole, or past the file's end). A view
    /// slates holds was committed when it was made; otherwise a view is made for the commit and let go. Refused
    /// `OutOfRange` past the object and `OsRefused` where the OS refuses the view or the commit (no commit left).
    pub fn prepare_range(&mut self, range: u64) -> Result<(), MemError> {
      if self.views.contains_key(&range) {
        return Ok(());
      }
      self.committed_view(range).map(drop)
    }

    /// Copies `from` into the object at `offset`, through atomics. Refused `OutOfRange` past the object and
    /// `ViewsExhausted` where a range it needs cannot be viewed; a refusal part-way leaves the earlier ranges written.
    pub fn store(&mut self, offset: u64, from: &[u8]) -> Result<(), MemError> {
      let len = u64::try_from(from.len()).map_err(|_| MemError::OutOfRange {
        offset: usize::MAX,
        len: from.len(),
      })?;
      self.span_end(offset, len)?;
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
      self.span_end(offset, len)?;
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

    /// Makes the object's bytes `offset .. offset + len` read zero, by stores of zeros: Windows gives no committed page
    /// of a section back, so the pages stay charged, within the ranges the fill budget already counts. Refused as
    /// [`DaxObject::store`] is.
    pub fn zero(&mut self, offset: u64, len: u64) -> Result<(), MemError> {
      let end = self.span_end(offset, len)?;
      let mut at = offset;
      while at < end {
        let left = usize::try_from(end.saturating_sub(at)).unwrap_or(usize::MAX);
        let (range, within, take) = piece(at, left)?;
        self
          .view(range)?
          .zero_bytes(within, within.saturating_add(take))?;
        at = at.saturating_add(u64::try_from(take).unwrap_or(u64::MAX));
      }
      Ok(())
    }

    /// The span `offset .. offset + len` inside the object, as its end; refused `OutOfRange` past it.
    fn span_end(&self, offset: u64, len: u64) -> Result<u64, MemError> {
      match offset.checked_add(len) {
        Some(end) if end <= self.len => Ok(end),
        _ => Err(MemError::OutOfRange {
          offset: usize::try_from(offset).unwrap_or(usize::MAX),
          len: usize::try_from(self.len).unwrap_or(usize::MAX),
        }),
      }
    }

    /// The view of range `range`, mapped and committed now if slates holds none, within the owner's bound.
    fn view(&mut self, range: u64) -> Result<&WriteView, MemError> {
      if self.views.contains_key(&range) {
        return self.views.get(&range).ok_or(MemError::ViewsExhausted {
          most: self.most_views,
        });
      }
      if self.views.len() >= self.most_views {
        return Err(MemError::ViewsExhausted {
          most: self.most_views,
        });
      }
      let view = self.committed_view(range)?;
      Ok(self.views.entry(range).or_insert(view))
    }

    /// A new writable view of range `range`, its pages committed. Refused `OutOfRange` past the object and `OsRefused`
    /// where the OS refuses the view or the commit.
    fn committed_view(&self, range: u64) -> Result<WriteView, MemError> {
      let refused = MemError::OutOfRange {
        offset: usize::try_from(range).unwrap_or(usize::MAX),
        len: usize::try_from(self.len).unwrap_or(usize::MAX),
      };
      let start = range
        .checked_mul(super::DAX_RANGE_BYTES)
        .filter(|start| start.saturating_add(super::DAX_RANGE_BYTES) <= self.len)
        .ok_or(refused.clone())?;
      let len = usize::try_from(super::DAX_RANGE_BYTES).map_err(|_| refused)?;
      let view = SectionView::map_at(
        self.section.as_handle(),
        FILE_MAP_WRITE | FILE_MAP_READ,
        start,
        len,
      )?;
      // SAFETY: commits the pages of the view just made, whose span this call names exactly; a reserved section's
      // pages are committed through any view of them, and the commit is the section's (`VirtualAlloc`, `SEC_RESERVE`).
      let committed = unsafe {
        VirtualAlloc(
          std::ptr::with_exposed_provenance(view.address),
          len,
          MEM_COMMIT,
          PAGE_READWRITE,
        )
      };
      if committed.is_null() {
        return Err(os("VirtualAlloc(MEM_COMMIT) of a DAX range"));
      }
      Ok(WriteView(view))
    }
  }

  /// The range an offset falls in, where in it, and how many of `left` bytes lie in it from there.
  fn piece(at: u64, left: usize) -> Result<(u64, usize, usize), MemError> {
    let refused = MemError::OutOfRange {
      offset: usize::try_from(at).unwrap_or(usize::MAX),
      len: left,
    };
    let range = at
      .checked_div(super::DAX_RANGE_BYTES)
      .ok_or(refused.clone())?;
    let within = at
      .checked_rem(super::DAX_RANGE_BYTES)
      .ok_or(refused.clone())?;
    let room = usize::try_from(super::DAX_RANGE_BYTES.saturating_sub(within))
      .map_err(|_| refused.clone())?;
    let within = usize::try_from(within).map_err(|_| refused)?;
    Ok((range, within, left.min(room)))
  }

  /// A duplicate of `handle` in this process with `access`, which a section's descriptor grants only where the source
  /// holds as much.
  fn duplicate_here(handle: BorrowedHandle<'_>, access: u32) -> Result<OwnedHandle, MemError> {
    let mut duplicate: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's pseudo-handle on both sides, a live handle, and an out-pointer to a local; the duplicate
    // is owned below.
    let done = unsafe {
      DuplicateHandle(
        GetCurrentProcess(),
        handle.as_raw_handle(),
        GetCurrentProcess(),
        &mut duplicate,
        access,
        0,
        0,
      )
    };
    if done == 0 {
      return Err(os("DuplicateHandle"));
    }
    // SAFETY: the duplicate just made, owned from here and closed on drop.
    Ok(unsafe { OwnedHandle::from_raw_handle(duplicate) })
  }

  /// Format: a peer section's security descriptor, in SDDL: a protected DACL (`P`, nothing inherited) whose one entry
  /// grants `OWNER RIGHTS` (`OW`) reading the descriptor (`RC`) alone. So the descriptor grants no one any right to the
  /// section, and the owner's implicit `WRITE_DAC` is cut, so no one can rewrite it to grant one: a handle conveys only
  /// the access it was made with. A handle duplicated with no more than its source's access is never checked against
  /// the descriptor, so slates' own handle (the creator's, every access) and the read-only duplicates it hands keep
  /// working, while a holder's duplicate that writes or that may rewrite the descriptor is refused (measured on Windows
  /// 10.0.26200, `tests/peer_section.rs`; with the default descriptor both are granted).
  pub const PEER_SECTION_SDDL: &str = "D:P(A;;RC;;;OW)";

  /// The OS refused `call`: its error code, read before any other call.
  fn os(call: &'static str) -> MemError {
    // SAFETY: reads this thread's last error, which the failed call just set.
    let code = unsafe { GetLastError() };
    MemError::OsRefused {
      call,
      code: i32::try_from(code).ok(),
    }
  }

  /// The system's page size, which an object's size is a whole number of.
  fn page_bytes() -> u64 {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: fills the struct this function owns.
    unsafe { GetSystemInfo(&mut info) };
    u64::from(info.dwPageSize.max(1))
  }

  /// A view of a section, unmapped when dropped. Its address is kept exposed rather than as a pointer, so the views'
  /// owners are `Send` (a mapping belongs to the process, not a thread) with no unsafe impl, as `crate::shared` keeps
  /// its views.
  struct SectionView {
    address: usize,
    len: usize,
  }

  impl SectionView {
    /// Maps `len` bytes of `section` from its start with `access`; `len` zero maps the whole section, whose length is
    /// then read back from the view (`VirtualQuery`: the section's size, a whole number of pages).
    fn map(section: BorrowedHandle<'_>, access: u32, len: usize) -> Result<SectionView, MemError> {
      SectionView::map_at(section, access, 0, len)
    }

    /// Maps `len` bytes of `section` from `offset` (a whole number of the system's allocation granules) with `access`;
    /// `len` zero maps from there to the section's end, as [`SectionView::map`] does.
    fn map_at(
      section: BorrowedHandle<'_>,
      access: u32,
      offset: u64,
      len: usize,
    ) -> Result<SectionView, MemError> {
      let high = u32::try_from(offset >> u32::BITS).unwrap_or(u32::MAX);
      let low = u32::try_from(offset & u64::from(u32::MAX)).unwrap_or(u32::MAX);
      // SAFETY: a view at an address the system chooses of a section this process holds a handle to; the system
      // refuses (null) a span past the section, an offset off its granule or an access the handle lacks. Unmapped by
      // `drop`.
      let view = unsafe { MapViewOfFile(section.as_raw_handle(), access, high, low, len) };
      if view.Value.is_null() {
        return Err(os("MapViewOfFile"));
      }
      let address = view.Value.expose_provenance();
      if len > 0 {
        return Ok(SectionView { address, len });
      }
      let mut region = MEMORY_BASIC_INFORMATION::default();
      // SAFETY: asks about the view just made; the answer is written to a local of the size passed.
      let answered = unsafe {
        VirtualQuery(
          view.Value,
          &mut region,
          std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
      };
      let mapped = SectionView {
        address,
        len: region.RegionSize,
      };
      if answered == 0 {
        return Err(os("VirtualQuery"));
      }
      Ok(mapped)
    }
  }

  impl Drop for SectionView {
    fn drop(&mut self) {
      // SAFETY: the view this value mapped and alone holds; nothing reaches it after the drop.
      unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
          Value: std::ptr::with_exposed_provenance_mut(self.address),
        })
      };
    }
  }

  impl Racy for SectionView {
    fn address(&self) -> *mut u8 {
      std::ptr::with_exposed_provenance_mut(self.address)
    }

    fn extent(&self) -> usize {
      self.len
    }
  }

  /// slates' own view of a peer object, read and write: the only view that stores.
  struct WriteView(SectionView);

  impl Racy for WriteView {
    fn address(&self) -> *mut u8 {
      self.0.address()
    }

    fn extent(&self) -> usize {
      self.0.extent()
    }
  }

  impl RacyStore for WriteView {}

  /// A new unnamed pagefile-backed section of `len` bytes under [`PEER_SECTION_SDDL`], which confines every handle but
  /// the creator's to the access it was made with. The creator's handle keeps every access and is never handed.
  /// `commit` is `SEC_COMMIT` (every page charged now, a peer's copy) or `SEC_RESERVE` (none until committed, a DAX
  /// object).
  fn confined_section(len: u64, commit: u32) -> Result<OwnedHandle, MemError> {
    let text: Vec<u16> = PEER_SECTION_SDDL
      .encode_utf16()
      .chain(std::iter::once(0))
      .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated SDDL string and an out-pointer to a local; the descriptor is freed below.
    let parsed = unsafe {
      ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text.as_ptr(),
        SDDL_REVISION_1,
        &mut descriptor,
        std::ptr::null_mut(),
      )
    };
    if parsed == 0 {
      return Err(os("ConvertStringSecurityDescriptorToSecurityDescriptorW"));
    }
    let attributes = SECURITY_ATTRIBUTES {
      nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
      lpSecurityDescriptor: descriptor,
      bInheritHandle: 0,
    };
    let high = u32::try_from(len >> u32::BITS).unwrap_or(u32::MAX);
    let low = u32::try_from(len & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    // SAFETY: a pagefile-backed section with no name, so nothing opens it by one; the attributes and the descriptor
    // they point at outlive the call.
    let section = unsafe {
      CreateFileMappingW(
        INVALID_HANDLE_VALUE,
        &attributes,
        PAGE_READWRITE | commit,
        high,
        low,
        std::ptr::null(),
      )
    };
    let made = if section.is_null() {
      Err(os("CreateFileMappingW"))
    } else {
      // SAFETY: the section handle just made, owned from here and closed on drop.
      Ok(unsafe { OwnedHandle::from_raw_handle(section) })
    };
    // SAFETY: the descriptor `ConvertStringSecurityDescriptorToSecurityDescriptorW` allocated, freed once.
    unsafe { LocalFree(descriptor) };
    made
  }

  /// One file's copy for a same-kernel peer, and slates' one writable view of it (§4.6, A-175 on Windows): an unnamed
  /// pagefile-backed section ([`confined_section`]) sized once to the file's page span. slates holds the creator's
  /// handle, with every access, and never hands it; a peer gets a read-only duplicate ([`PeerObject::read_only_handle`])
  /// that the section's descriptor lets no one turn into more.
  pub struct PeerObject {
    section: OwnedHandle,
    view: WriteView,
    len: u64,
  }

  impl std::fmt::Debug for PeerObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.debug_struct("PeerObject")
        .field("len", &self.len)
        .finish()
    }
  }

  impl PeerObject {
    /// The size of the object a file of `file_bytes` is copied into: its page span, and at least one page, so the
    /// object is never empty and a peer maps all of it. Refused `OutOfRange` where the span overflows.
    pub fn bytes_for(file_bytes: u64) -> Result<u64, MemError> {
      file_bytes
        .max(1)
        .checked_next_multiple_of(page_bytes())
        .ok_or(MemError::OutOfRange {
          offset: usize::try_from(file_bytes).unwrap_or(usize::MAX),
          len: usize::MAX,
        })
    }

    /// A new object of zeros, [`PeerObject::bytes_for`] `file_bytes` long, with slates' writable view of all of it.
    /// `_name` is Linux's (a memfd's name); a section here has none, so nothing opens it by one. Refused `OutOfRange`
    /// where the span does not fit an address, `OsRefused` where the OS refuses a step.
    pub fn create(_name: &str, file_bytes: u64) -> Result<PeerObject, MemError> {
      let len = PeerObject::bytes_for(file_bytes)?;
      let view_len = usize::try_from(len).map_err(|_| MemError::OutOfRange {
        offset: 0,
        len: usize::MAX,
      })?;
      let section = confined_section(len, SEC_COMMIT)?;
      let view = WriteView(SectionView::map(
        section.as_handle(),
        FILE_MAP_WRITE | FILE_MAP_READ,
        view_len,
      )?);
      Ok(PeerObject { section, view, len })
    }

    /// The object's size in bytes, which never changes.
    #[must_use]
    pub fn len(&self) -> u64 {
      self.len
    }

    /// Never: an object is at least a page. Here for the convention beside [`PeerObject::len`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
      self.len == 0
    }

    /// A duplicate of the section's handle in this process that maps it read alone (`FILE_MAP_READ`): what a peer is
    /// handed, duplicated again into its process. The section's own handle, which writes, never leaves the object.
    pub fn read_only_handle(&self) -> Result<OwnedHandle, MemError> {
      duplicate_here(self.section.as_handle(), FILE_MAP_READ)
    }

    /// Copies `from` into the object at `offset`, through atomics. Refused `OutOfRange` past the object.
    pub fn store(&mut self, offset: u64, from: &[u8]) -> Result<(), MemError> {
      let at = self.within(offset, from.len())?;
      self.view.store(at, from)
    }

    /// Copies the object's bytes at `offset` into `into`, through atomics. Refused `OutOfRange` past the object.
    pub fn load(&mut self, offset: u64, into: &mut [u8]) -> Result<(), MemError> {
      let at = self.within(offset, into.len())?;
      self.view.load(at, into)
    }

    /// Makes the object's bytes `offset .. offset + len` read zero, by stores of zeros over all of them: Windows tells
    /// no section's holes, so the pages zeroed are charged, within the object's size that its making charged already.
    /// Refused `OutOfRange` past the object.
    pub fn zero(&mut self, offset: u64, len: u64) -> Result<(), MemError> {
      let count = usize::try_from(len).map_err(|_| MemError::OutOfRange {
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        len: usize::MAX,
      })?;
      let from = self.within(offset, count)?;
      self.view.zero_bytes(from, from.saturating_add(count))
    }

    /// `offset .. offset + count` as an offset into slates' view, refused `OutOfRange` past the object.
    fn within(&self, offset: u64, count: usize) -> Result<usize, MemError> {
      let refused = MemError::OutOfRange {
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        len: usize::try_from(self.len).unwrap_or(usize::MAX),
      };
      let end = u64::try_from(count)
        .ok()
        .and_then(|count| offset.checked_add(count))
        .ok_or(refused.clone())?;
      if end > self.len {
        return Err(refused);
      }
      usize::try_from(offset).map_err(|_| refused)
    }
  }

  /// A client's view of the peer object a daemon handed it (§4.6, A-175 on Windows): the whole section, mapped read
  /// alone through the handle the daemon duplicated into this process. The daemon writes it at any moment, since it
  /// follows the file, so its bytes are loaded through atomics and never borrowed.
  pub struct PeerView {
    handle: OwnedHandle,
    view: SectionView,
    len: u64,
  }

  impl std::fmt::Debug for PeerView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.debug_struct("PeerView").field("len", &self.len).finish()
    }
  }

  impl PeerView {
    /// Maps the whole section behind `handle`, read alone, and serves its first `len` bytes. Refused `OutOfRange` for an
    /// empty view or one longer than the section, and `OsRefused` where the OS refuses the map.
    pub fn map(handle: OwnedHandle, len: u64) -> Result<PeerView, MemError> {
      let view = SectionView::map(handle.as_handle(), FILE_MAP_READ, 0)?;
      let object_bytes = u64::try_from(view.len).unwrap_or(0);
      if len == 0 || len > object_bytes {
        return Err(MemError::OutOfRange {
          offset: usize::try_from(len).unwrap_or(usize::MAX),
          len: view.len,
        });
      }
      Ok(PeerView { handle, view, len })
    }

    /// The view's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
      self.len
    }

    /// Never: an empty view is refused. Here for the convention beside [`PeerView::len`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
      self.len == 0
    }

    /// The handle the view maps, as the daemon handed it.
    #[must_use]
    pub fn handle(&self) -> BorrowedHandle<'_> {
      self.handle.as_handle()
    }

    /// Copies the view's bytes at `offset` into `into`, through atomics. Refused `OutOfRange` past the view.
    pub fn load(&self, offset: u64, into: &mut [u8]) -> Result<(), MemError> {
      let refused = MemError::OutOfRange {
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        len: usize::try_from(self.len).unwrap_or(usize::MAX),
      };
      let end = u64::try_from(into.len())
        .ok()
        .and_then(|count| offset.checked_add(count))
        .ok_or(refused.clone())?;
      if end > self.len {
        return Err(refused);
      }
      let at = usize::try_from(offset).map_err(|_| refused)?;
      self.view.load(at, into)
    }
  }
}
