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
//! `MapViewOfFile`.
//!
//! **No reference into the mapping is ever handed out** (AUD-29-09). Another process writes the same
//! bytes, so a `&[u8]` over them would promise Rust an immutability the other process does not keep. The
//! layout's concurrent words are declared when the object is created or opened ([`Words`]) and reached
//! only through [`SharedObject::atomic_u64`] and [`SharedObject::atomic_u32`], for a declared word of
//! exactly that width. Every other byte crosses the boundary by copy — [`SharedObject::read`] and
//! [`SharedObject::write`], through the mapping's raw pointer — and a copy that touches a declared word is
//! refused. So a plain access and an atomic one, or two atomic widths, never meet on the same bytes, and
//! the protocol the words carry (a slot released by its sequence word) is what orders the copies. Until
//! 2026-09-30 the object handed out whole-map byte slices beside atomic views of words inside them.
//!
//! Hermeticity: tmpfs pages, `shm_open` objects and pagefile-backed sections are all pageable;
//! the caller locks what must never reach a disk with [`SharedObject::lock`], and the RAM-only
//! policy reports what it could not lock (D-12).
//!
//! A [`SparseObject`] is the same object for a layout far larger than what it will hold at once — the
//! anchor segment's rings and snapshot slots, the content object (§4.8) — backed only where it is
//! touched. A `memfd` or `shm_open` object is backed page by page on first touch already. A Windows
//! pagefile section is charged its whole size against the commit limit at creation, so a sparse one is
//! created `SEC_RESERVE` and its pages committed as they are reached: a 167 GB anchor layout on a
//! 16 GB runner was refused `ERROR_NO_SYSTEM_RESOURCES` (CI run 36202635768). A reserved page is not
//! readable until committed, so a sparse object is reached only by ranges, each committed before its
//! slice exists; it has no whole-object view.

use std::sync::atomic::{AtomicU32, AtomicU64};

use crate::error::{LayoutRefusal, MemError};
use crate::words::{Layout, RunId, SpanId, SpanRun, Width, WordRun, Words, refused};

/// How an object is handed to another process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handoff {
  /// A descriptor number the child inherits (Linux).
  Descriptor(i32),
  /// A name the other process opens (macOS, Windows).
  Name(String),
}

/// A shared memory object and its full view, reached by copy and through its declared words.
pub struct SharedObject {
  inner: platform::Inner,
  len: usize,
  words: Layout,
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
  /// where it is the handoff; on Linux it is the descriptor's label), whose concurrent words are `words`.
  pub fn create(name: &str, len: usize, words: Words) -> Result<SharedObject, MemError> {
    let words = words.layout(len)?;
    let inner = platform::create(name, len)?;
    Ok(SharedObject { inner, len, words })
  }

  /// Opens an object another process created, from its handoff, expecting `len` bytes laid out with
  /// `words` (the creator's layout: both sides declare the same).
  pub fn open(handoff: &Handoff, len: usize, words: Words) -> Result<SharedObject, MemError> {
    let words = words.layout(len)?;
    let inner = platform::open(handoff, len)?;
    Ok(SharedObject { inner, len, words })
  }

  /// This object with `words` declared: for an opener that learns the layout from a header it first read
  /// by copy (a client region's rings). By value, so no atomic view of the old layout can outlive the
  /// change; the new layout is checked like a created one, so a hostile header that would overlap or
  /// misalign words is refused.
  pub fn declare(self, words: Words) -> Result<SharedObject, MemError> {
    let words = words.layout(self.len)?;
    Ok(SharedObject { words, ..self })
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

  /// Copies the `into.len()` bytes at `offset` out, for the side that owns them under the layout's
  /// protocol; refused past the end or touching a declared word.
  pub fn read(&self, offset: usize, into: &mut [u8]) -> Result<(), MemError> {
    check_copy(&self.words, offset, into.len(), self.len)?;
    copy_out(&self.inner, offset, into);
    Ok(())
  }

  /// Copies `from` in at `offset`, for the side that owns those bytes under the layout's protocol;
  /// refused past the end or touching a declared word.
  pub fn write(&mut self, offset: usize, from: &[u8]) -> Result<(), MemError> {
    check_copy(&self.words, offset, from.len(), self.len)?;
    copy_in(&mut self.inner, offset, from);
    Ok(())
  }

  /// The declared 64-bit word at `offset`, as an atomic.
  pub fn atomic_u64(&self, offset: usize) -> Result<&AtomicU64, MemError> {
    check_word(&self.words, offset, Width::U64)?;
    word_view(&self.inner, offset)
  }

  /// The declared 32-bit word at `offset`, as an atomic.
  pub fn atomic_u32(&self, offset: usize) -> Result<&AtomicU32, MemError> {
    check_word(&self.words, offset, Width::U32)?;
    word_view(&self.inner, offset)
  }

  /// The id of the declared run `run`, resolved once so a hot path reaches its words in constant time.
  pub fn resolve(&self, run: &WordRun) -> Result<RunId, MemError> {
    resolve(&self.words, run)
  }

  /// Word `index` of the declared 64-bit run `id`, as an atomic, in constant time.
  pub fn run_u64(&self, id: RunId, index: usize) -> Result<&AtomicU64, MemError> {
    word_view(&self.inner, run_word(&self.words, id, index, Width::U64)?)
  }

  /// Word `index` of the declared 32-bit run `id`, as an atomic, in constant time.
  pub fn run_u32(&self, id: RunId, index: usize) -> Result<&AtomicU32, MemError> {
    word_view(&self.inner, run_word(&self.words, id, index, Width::U32)?)
  }

  /// The id of the declared span run `span`, resolved once so a hot path copies its spans in constant
  /// time.
  pub fn resolve_span(&self, span: &SpanRun) -> Result<SpanId, MemError> {
    self
      .words
      .resolve_span(span)
      .ok_or_else(|| refused(0, 0, LayoutRefusal::NotAWord))
  }

  /// Copies `into.len()` bytes out of span `index` of the declared span run `id`, in constant time: the
  /// layout proved every span of the run clear of its words when it was built.
  pub fn read_span(&self, id: SpanId, index: usize, into: &mut [u8]) -> Result<(), MemError> {
    copy_out(
      &self.inner,
      span_at(&self.words, id, index, into.len())?,
      into,
    );
    Ok(())
  }

  /// Copies `from` into span `index` of the declared span run `id`, in constant time.
  pub fn write_span(&mut self, id: SpanId, index: usize, from: &[u8]) -> Result<(), MemError> {
    let at = span_at(&self.words, id, index, from.len())?;
    copy_in(&mut self.inner, at, from);
    Ok(())
  }

  /// Locks the whole object into RAM (D-12); the refusal names the OS call.
  pub fn lock(&mut self) -> Result<(), MemError> {
    self.inner.lock()
  }
}

/// A shared memory object exactly one process reaches, through this value only: the storage a region
/// backs with anchor-owned RAM (`Region::shared`), whose arena hands out references to its bytes. Its one
/// constructor carries the promise; its views are then safe.
pub struct ExclusiveObject(SharedObject);

impl std::fmt::Debug for ExclusiveObject {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_tuple("ExclusiveObject").field(&self.0).finish()
  }
}

impl ExclusiveObject {
  /// `object`, as storage no one else reaches.
  ///
  /// # Safety
  ///
  /// While this value lives, no other process maps or reaches the object's bytes, and no other value
  /// in this one does: the anchor that keeps the object across a daemon restart holds it without touching
  /// it, and a restarted daemon wraps it only after the one before it has exited. The object must declare
  /// no words. A shared object two processes use at once is reached through its copies and words instead
  /// (AUD-29-09).
  pub unsafe fn new(object: SharedObject) -> ExclusiveObject {
    ExclusiveObject(object)
  }

  /// The bytes `[offset, offset + len)` of the object `handoff` names, mapped on their own, as storage no
  /// one else reaches (A-64: one shard's arena range of the anchor's content object). `offset` must be a
  /// multiple of [`mapping_granule`] (refused [`MemError::OutOfRange`] otherwise), and the system refuses a span
  /// past the object. On Windows
  /// the range is committed whole when it is mapped.
  ///
  /// # Safety
  ///
  /// As for [`ExclusiveObject::new`], for these bytes: while this value lives, no other mapping in any process
  /// reaches `[offset, offset + len)`. Other views of the object may exist, provided they never touch this
  /// range: the daemon's copies of the content object reach its write log and image slots only, and the
  /// anchor that keeps the object never touches its bytes.
  pub unsafe fn open_range(
    handoff: &Handoff,
    offset: usize,
    len: usize,
  ) -> Result<ExclusiveObject, MemError> {
    // Checked here, the same on every platform: Unix's map would round the offset down and Windows refuses it.
    let granule = platform::mapping_granule()?;
    if !offset.is_multiple_of(granule.max(1)) {
      return Err(MemError::OutOfRange { offset, len });
    }
    let words = Words::new().layout(len)?;
    let inner = platform::open_range(handoff, offset, len)?;
    Ok(ExclusiveObject(SharedObject { inner, len, words }))
  }

  /// The whole map.
  pub fn bytes(&self) -> &[u8] {
    self.0.inner.bytes()
  }

  /// The whole map, writable.
  pub fn bytes_mut(&mut self) -> &mut [u8] {
    self.0.inner.bytes_mut()
  }

  /// The object's handoff, for the process that re-wraps it after this one.
  pub fn handoff(&self) -> Result<Handoff, MemError> {
    self.0.handoff()
  }

  /// Locks the whole object into RAM (D-12).
  pub fn lock(&mut self) -> Result<(), MemError> {
    self.0.lock()
  }
}

/// The granule a mapping's offset must be a multiple of: the base page on Unix, the allocation granularity on
/// Windows. A range [`ExclusiveObject::open_range`] maps starts on one.
pub fn mapping_granule() -> Result<usize, MemError> {
  platform::mapping_granule()
}

/// A shared memory object backed only where it is touched (see the module doc): the anchor segment and
/// the content object. Reached by ranges; each range is committed (Windows) before its slice exists,
/// so no view ever spans an uncommitted page. `!Sync`, on every platform alike: the commit record is
/// this process's and this thread's, and a type that is `Sync` on one platform only would let a
/// cross-thread use compile where it is not tested.
pub struct SparseObject {
  inner: platform::Inner,
  len: usize,
  commits: platform::Commits,
  words: Layout,
}

impl std::fmt::Debug for SparseObject {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SparseObject")
      .field("len", &self.len)
      .finish()
  }
}

impl SparseObject {
  /// Creates a sparse object of `len` bytes named `name` (the name matters where it is the handoff),
  /// whose concurrent words are `words`.
  pub fn create(name: &str, len: usize, words: Words) -> Result<SparseObject, MemError> {
    let words = words.layout(len)?;
    let inner = platform::create_sparse(name, len)?;
    let commits = platform::Commits::new(len)?;
    Ok(SparseObject {
      inner,
      len,
      commits,
      words,
    })
  }

  /// Opens a sparse object another process created, from its handoff, expecting `len` bytes laid out
  /// with `words`.
  pub fn open(handoff: &Handoff, len: usize, words: Words) -> Result<SparseObject, MemError> {
    let words = words.layout(len)?;
    let inner = platform::open(handoff, len)?;
    let commits = platform::Commits::new(len)?;
    Ok(SparseObject {
      inner,
      len,
      commits,
      words,
    })
  }

  /// What to hand another process so it can [`SparseObject::open`] this object.
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

  /// Copies the `into.len()` bytes at `offset` out (committed first); refused past the end or touching a
  /// declared word.
  pub fn read(&self, offset: usize, into: &mut [u8]) -> Result<(), MemError> {
    check_copy(&self.words, offset, into.len(), self.len)?;
    self.commits.commit(&self.inner, offset, into.len())?;
    copy_out(&self.inner, offset, into);
    Ok(())
  }

  /// Copies `from` in at `offset` (committed first); refused past the end or touching a declared word.
  pub fn write(&mut self, offset: usize, from: &[u8]) -> Result<(), MemError> {
    check_copy(&self.words, offset, from.len(), self.len)?;
    self.commits.commit(&self.inner, offset, from.len())?;
    copy_in(&mut self.inner, offset, from);
    Ok(())
  }

  /// Copies the `into.len()` racy bytes at `offset` out, byte by byte through `AtomicU8` (committed first):
  /// for a seqlock reader, which validates the copy against the generation after. Refused unless the span
  /// lies wholly inside one declared racy span.
  pub fn read_racy(&self, offset: usize, into: &mut [u8]) -> Result<(), MemError> {
    check_racy(&self.words, offset, into.len(), self.len)?;
    self.commits.commit(&self.inner, offset, into.len())?;
    racy_out(&self.inner, offset, into);
    Ok(())
  }

  /// Copies `from` in at `offset` byte by byte through `AtomicU8` (committed first): for a seqlock writer.
  /// Refused unless the span lies wholly inside one declared racy span.
  pub fn write_racy(&mut self, offset: usize, from: &[u8]) -> Result<(), MemError> {
    check_racy(&self.words, offset, from.len(), self.len)?;
    self.commits.commit(&self.inner, offset, from.len())?;
    racy_in(&self.inner, offset, from);
    Ok(())
  }

  /// The declared 64-bit word at `offset`, committed first, as an atomic.
  pub fn atomic_u64(&self, offset: usize) -> Result<&AtomicU64, MemError> {
    check_word(&self.words, offset, Width::U64)?;
    self
      .commits
      .commit(&self.inner, offset, Width::U64.bytes())?;
    word_view(&self.inner, offset)
  }

  /// The declared 32-bit word at `offset`, committed first, as an atomic.
  pub fn atomic_u32(&self, offset: usize) -> Result<&AtomicU32, MemError> {
    check_word(&self.words, offset, Width::U32)?;
    self
      .commits
      .commit(&self.inner, offset, Width::U32.bytes())?;
    word_view(&self.inner, offset)
  }

  /// The id of the declared run `run`, resolved once so a hot path reaches its words in constant time.
  pub fn resolve(&self, run: &WordRun) -> Result<RunId, MemError> {
    resolve(&self.words, run)
  }

  /// Word `index` of the declared 64-bit run `id`, committed first, as an atomic, in constant time.
  pub fn run_u64(&self, id: RunId, index: usize) -> Result<&AtomicU64, MemError> {
    let offset = run_word(&self.words, id, index, Width::U64)?;
    self
      .commits
      .commit(&self.inner, offset, Width::U64.bytes())?;
    word_view(&self.inner, offset)
  }
}

/// A copy of `len` bytes at `offset` in an object of `object_len` bytes laid out with `words`: inside the
/// object, and touching no declared word.
fn check_copy(
  words: &Layout,
  offset: usize,
  len: usize,
  object_len: usize,
) -> Result<(), MemError> {
  match offset.checked_add(len) {
    Some(end) if end <= object_len => {}
    _ => return Err(refused(offset, len, LayoutRefusal::OutOfRange)),
  }
  if words.touches(offset, len) {
    return Err(refused(offset, len, LayoutRefusal::TouchesWord));
  }
  Ok(())
}

/// A racy copy of `len` bytes at `offset`: inside the object and wholly inside one declared racy span.
fn check_racy(
  words: &Layout,
  offset: usize,
  len: usize,
  object_len: usize,
) -> Result<(), MemError> {
  match offset.checked_add(len) {
    Some(end) if end <= object_len => {}
    _ => return Err(refused(offset, len, LayoutRefusal::OutOfRange)),
  }
  if !words.racy_covers(offset, len) {
    return Err(refused(offset, len, LayoutRefusal::NotAWord));
  }
  Ok(())
}

/// Copies the `into.len()` bytes at `offset` out (inside the map; the caller checked).
fn copy_out(inner: &platform::Inner, offset: usize, into: &mut [u8]) {
  // SAFETY: `[offset, offset + into.len())` lies inside the live map (the caller checked) and is
  // committed where the platform commits; the source is the mapping's raw pointer, never a reference, and
  // `into` is this process's own buffer, so the two cannot overlap. These are plain bytes, which the
  // layout's protocol gives the copying side for the copy (no declared word is touched: checked).
  unsafe {
    std::ptr::copy_nonoverlapping(inner.base().add(offset), into.as_mut_ptr(), into.len());
  }
}

/// Copies `from` in at `offset` (inside the map; the caller checked).
fn copy_in(inner: &mut platform::Inner, offset: usize, from: &[u8]) {
  // SAFETY: as for `copy_out`, in the other direction; `&mut` is this process's only writer through the
  // object.
  unsafe {
    std::ptr::copy_nonoverlapping(from.as_ptr(), inner.base().add(offset), from.len());
  }
}

/// Copies declared racy bytes out through `AtomicU8`, relaxed (the seqlock's generation orders them).
fn racy_out(inner: &platform::Inner, offset: usize, into: &mut [u8]) {
  let base = inner.base().wrapping_add(offset);
  for (index, byte) in into.iter_mut().enumerate() {
    // SAFETY: the byte lies inside the live map (the caller checked the span) and is a declared racy
    // byte, which every access on either side reaches only through `AtomicU8` (the layout refuses a plain
    // copy of it and any wider atomic over it); `AtomicU8` has no alignment requirement and every bit
    // pattern is valid.
    *byte = unsafe {
      &*base
        .wrapping_add(index)
        .cast::<std::sync::atomic::AtomicU8>()
    }
    .load(std::sync::atomic::Ordering::Relaxed);
  }
}

/// Copies `from` into declared racy bytes through `AtomicU8`, relaxed.
fn racy_in(inner: &platform::Inner, offset: usize, from: &[u8]) {
  let base = inner.base().wrapping_add(offset);
  for (index, byte) in from.iter().enumerate() {
    // SAFETY: as in `racy_out`.
    unsafe {
      &*base
        .wrapping_add(index)
        .cast::<std::sync::atomic::AtomicU8>()
    }
    .store(*byte, std::sync::atomic::Ordering::Relaxed);
  }
}

/// The offset of span `index` of span run `id` for a copy of `len` bytes: constant time, and only a span
/// the layout declared (validated inside the object and clear of every word when the layout was built).
fn span_at(layout: &Layout, id: SpanId, index: usize, len: usize) -> Result<usize, MemError> {
  layout
    .span(id)
    .and_then(|span| span.span(index, len))
    .ok_or_else(|| refused(index, len, LayoutRefusal::NotAWord))
}

/// `run`'s id in `layout`, or the refusal for a run it does not declare.
fn resolve(layout: &Layout, run: &WordRun) -> Result<RunId, MemError> {
  layout
    .resolve(run)
    .ok_or_else(|| refused(run.first_offset(), 0, LayoutRefusal::NotAWord))
}

/// The offset of word `index` of run `id` of `width` in `layout`: constant time, and only a word the
/// layout declared (validated in range and aligned when the layout was built).
fn run_word(layout: &Layout, id: RunId, index: usize, width: Width) -> Result<usize, MemError> {
  layout
    .run(id)
    .and_then(|run| run.word(index, width))
    .ok_or_else(|| refused(index, width.bytes(), LayoutRefusal::NotAWord))
}

/// An atomic view of `width` at `offset`: only a declared word of exactly that width.
fn check_word(words: &Layout, offset: usize, width: Width) -> Result<(), MemError> {
  if words.holds(offset, width) {
    Ok(())
  } else {
    Err(refused(offset, width.bytes(), LayoutRefusal::NotAWord))
  }
}

/// The atomic types a shared word is viewed as: each has every bit pattern valid, and its size equal
/// to its alignment.
trait Word {}
impl Word for AtomicU32 {}
impl Word for AtomicU64 {}

/// The declared word at `offset` (checked by the caller: inside the map, aligned to `W`, declared of `W`'s
/// width, touched by no copy) as the atomic `W`.
fn word_view<W: Word>(inner: &platform::Inner, offset: usize) -> Result<&W, MemError> {
  let word = inner.base().wrapping_add(offset);
  if !(word as usize).is_multiple_of(align_of::<W>()) {
    return Err(refused(offset, size_of::<W>(), LayoutRefusal::Misaligned));
  }
  // SAFETY: the word lies inside a map that lives as long as `inner` (the caller checked the range),
  // aligned to `W` (checked above), and is a declared word of `W`'s width, which the layout guarantees no
  // copy touches and no other width views; `W` is an atomic integer, valid for every bit pattern. No Rust
  // reference to the map's bytes exists to alias it: the object hands out none.
  Ok(unsafe { &*word.cast::<W>() })
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

  fn map(fd: &OwnedFd, offset: usize, len: usize) -> Result<MmapMut, MemError> {
    let offset = u64::try_from(offset).map_err(|_| MemError::OsRefused {
      call: "mmap",
      code: None,
    })?;
    // SAFETY: the object behind `fd` is a memory object this module created or opened by the
    // handoff its creator gave; by this module's rule no process truncates it while a view
    // lives, and every concurrent word is reached through the atomic views. The map borrows
    // nothing that outlives it.
    unsafe { MmapOptions::new().offset(offset).len(len).map_mut(fd) }.map_err(|e| {
      MemError::OsRefused {
        call: "mmap",
        code: e.raw_os_error(),
      }
    })
  }

  pub(super) fn create(name: &str, len: usize) -> Result<Inner, MemError> {
    let created = create_object(name)?;
    let size = u64::try_from(len).map_err(|_| MemError::OsRefused {
      call: "ftruncate",
      code: None,
    })?;
    #[allow(clippy::disallowed_methods)] // the memory object: RAM, not a host path (R1).
    // structural: allow — sizing the memory object just created (no filesystem entry; D-10).
    rustix::fs::ftruncate(&created.fd, size).map_err(|e| refused("ftruncate", e))?;
    let map = map(&created.fd, 0, len)?;
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
    open_range(handoff, 0, len)
  }

  /// `[offset, offset + len)` of the object `handoff` names, mapped on its own.
  pub(super) fn open_range(
    handoff: &Handoff,
    offset: usize,
    len: usize,
  ) -> Result<Inner, MemError> {
    let fd = open_object(handoff)?;
    let map = map(&fd, offset, len)?;
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
        use std::os::fd::BorrowedFd;
        // The handed-off number is inherited state of this process, never owned here: it is
        // **duplicated** (close-on-exec, so a child of this process does not inherit the duplicate)
        // and the duplicate is what this object owns and closes. A process attaches one handoff
        // more than once — the daemon reads the anchor's published profile before it starts, and
        // every shard maps the segment again — and two owners of one number closed it twice (the
        // second drop aborted the process under Rust's I/O-safety check; the daemon's shard mapping
        // found the number already closed: `mmap` EBADF).
        // Record: docs/bugs/2026-09-14-segment-handoff-descriptor-owned-twice.md.
        // SAFETY: the number names a descriptor the parent handed to this process by inheritance;
        // it stays open for the process's life (nothing here closes it), and the borrow lasts only
        // for the duplication.
        let inherited = unsafe { BorrowedFd::borrow_raw(*raw) };
        rustix::io::fcntl_dupfd_cloexec(inherited, 0).map_err(|e| refused("dup", e))
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

  /// The offset granule of a mapping: the base page.
  pub(super) fn mapping_granule() -> Result<usize, MemError> {
    Ok(rustix::param::page_size())
  }

  /// A sparse object is the same object here: a `memfd` or `shm_open` page is backed on first touch.
  pub(super) fn create_sparse(name: &str, len: usize) -> Result<Inner, MemError> {
    create(name, len)
  }

  /// Nothing to commit here: pages are backed as they are touched. Holds the same `!Sync` marker the
  /// Windows record does, so the sparse object's auto traits are the same on every platform.
  pub(super) struct Commits(std::marker::PhantomData<std::cell::Cell<()>>);

  impl Commits {
    pub(super) fn new(_len: usize) -> Result<Commits, MemError> {
      Ok(Commits(std::marker::PhantomData))
    }

    pub(super) fn commit(
      &self,
      _inner: &Inner,
      _offset: usize,
      _len: usize,
    ) -> Result<(), MemError> {
      Ok(())
    }
  }

  impl Inner {
    /// The map's first byte, as the raw pointer the mapping returned (no reference to its bytes is formed).
    pub(super) fn base(&self) -> *mut u8 {
      self.map.as_ptr().cast_mut()
    }

    /// The whole map, for [`super::ExclusiveObject`] only (its constructor's contract makes it sound).
    pub(super) fn bytes(&self) -> &[u8] {
      &self.map
    }

    /// The whole map, writable, for [`super::ExclusiveObject`] only.
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

  use std::cell::Cell;
  use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
  };

  use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEM_COMMIT, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_PROTECTION_FLAGS, PAGE_READWRITE, SEC_RESERVE, UnmapViewOfFile,
    VirtualAlloc, VirtualLock,
  };
  use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

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

  /// Closes a section handle this module created or opened and has not handed to an `Inner`.
  fn close(handle: HANDLE) {
    // SAFETY: the handle is ours, and nothing else holds or closes it.
    unsafe { CloseHandle(handle) };
  }

  fn view(handle: HANDLE, offset: usize, len: usize) -> Result<*mut c_void, MemError> {
    let offset = u64::try_from(offset).unwrap_or(u64::MAX);
    let high = u32::try_from(offset >> u32::BITS).unwrap_or(u32::MAX);
    let low = u32::try_from(offset & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    // SAFETY: a read/write view of `len` bytes at `offset` of a section handle this module owns; the
    // system refuses (null) an offset off its allocation granularity or a span past the section.
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, high, low, len) };
    if view.Value.is_null() {
      let err = os("MapViewOfFile");
      close(handle);
      return Err(err);
    }
    Ok(view.Value)
  }

  pub(super) fn handoff_for_name(name: &str) -> Option<Handoff> {
    Some(Handoff::Name(name.to_owned()))
  }

  pub(super) fn create(name: &str, len: usize) -> Result<Inner, MemError> {
    create_section(name, len, PAGE_READWRITE)
  }

  /// A section reserved, not committed: its pages are charged against the commit limit only as
  /// [`Commits::commit`] reaches them.
  pub(super) fn create_sparse(name: &str, len: usize) -> Result<Inner, MemError> {
    create_section(name, len, PAGE_READWRITE | SEC_RESERVE)
  }

  fn create_section(
    name: &str,
    len: usize,
    protection: PAGE_PROTECTION_FLAGS,
  ) -> Result<Inner, MemError> {
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
        protection,
        high,
        low,
        wide.as_ptr(),
      )
    };
    if handle.is_null() {
      return Err(os("CreateFileMappingW"));
    }
    // A name that already exists opens that section, at its size, with `ERROR_ALREADY_EXISTS` set:
    // two creators would share one object silently (two daemons writing one anchor segment). Refused
    // with the OS code instead; the caller names its objects uniquely.
    if std::io::Error::last_os_error().raw_os_error() == i32::try_from(ERROR_ALREADY_EXISTS).ok() {
      close(handle);
      return Err(MemError::OsRefused {
        call: "CreateFileMappingW (the name is already a live section)",
        code: i32::try_from(ERROR_ALREADY_EXISTS).ok(),
      });
    }
    let view = view(handle, 0, len)?;
    Ok(Inner {
      handle: handle.expose_provenance(),
      view: view.expose_provenance(),
      len,
      name: name.to_owned(),
    })
  }

  pub(super) fn open(handoff: &Handoff, len: usize) -> Result<Inner, MemError> {
    open_view(handoff, 0, len)
  }

  /// `[offset, offset + len)` of the section `handoff` names, mapped on its own and committed whole: a
  /// sparse section's pages are not readable until committed, and an exclusive range is reached by
  /// reference, so it cannot commit page by page as the copies do.
  pub(super) fn open_range(
    handoff: &Handoff,
    offset: usize,
    len: usize,
  ) -> Result<Inner, MemError> {
    let inner = open_view(handoff, offset, len)?;
    Commits::new(len)?.commit(&inner, 0, len)?;
    Ok(inner)
  }

  /// The offset granule of a mapping: the allocation granularity.
  pub(super) fn mapping_granule() -> Result<usize, MemError> {
    Commits::new(0).map(|commits| commits.granule)
  }

  fn open_view(handoff: &Handoff, offset: usize, len: usize) -> Result<Inner, MemError> {
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
    let view = view(handle, offset, len)?;
    Ok(Inner {
      handle: handle.expose_provenance(),
      view: view.expose_provenance(),
      len,
      name: name.clone(),
    })
  }

  /// Which granules of a sparse section this process has committed: one bit per allocation granule
  /// (the machine's, from `GetSystemInfo`), so a range already committed costs no system call. The
  /// commit itself is the section's, seen by every view in every process; a granule another process
  /// committed is committed again here once, harmlessly (`VirtualAlloc` of committed pages succeeds).
  pub(super) struct Commits {
    granule: usize,
    bits: Box<[Cell<u64>]>,
  }

  impl Commits {
    pub(super) fn new(len: usize) -> Result<Commits, MemError> {
      // SAFETY: `GetSystemInfo` fills the one `SYSTEM_INFO` it is given, a live local.
      let info = unsafe {
        let mut info: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut info);
        info
      };
      let granule = usize::try_from(info.dwAllocationGranularity)
        .unwrap_or(0)
        .max(usize::try_from(info.dwPageSize).unwrap_or(0));
      if granule == 0 {
        return Err(MemError::OsRefused {
          call: "GetSystemInfo (no allocation granularity)",
          code: None,
        });
      }
      let granules = len.div_ceil(granule);
      let words = granules.div_ceil(u64::BITS as usize);
      Ok(Commits {
        granule,
        bits: (0..words).map(|_| Cell::new(0)).collect(),
      })
    }

    fn is_committed(&self, granule: usize) -> bool {
      let bits = u64::BITS as usize;
      self
        .bits
        .get(granule / bits)
        .is_some_and(|word| word.get() & (1 << (granule % bits)) != 0)
    }

    fn mark(&self, granule: usize) {
      let bits = u64::BITS as usize;
      if let Some(word) = self.bits.get(granule / bits) {
        word.set(word.get() | (1 << (granule % bits)));
      }
    }

    /// Commits every granule `[offset, offset + len)` touches that this process has not, one
    /// `VirtualAlloc` per run of uncommitted granules.
    pub(super) fn commit(&self, inner: &Inner, offset: usize, len: usize) -> Result<(), MemError> {
      if len == 0 {
        return Ok(());
      }
      let first = offset / self.granule;
      let last = (offset + len - 1) / self.granule;
      let mut granule = first;
      while granule <= last {
        if self.is_committed(granule) {
          granule += 1;
          continue;
        }
        let run_start = granule;
        while granule <= last && !self.is_committed(granule) {
          granule += 1;
        }
        let start = run_start * self.granule;
        let end = (granule * self.granule).min(inner.len);
        // SAFETY: `[start, end)` lies inside this process's view of the section (`end` is clamped to
        // its length); committing pages of a view of a `SEC_RESERVE` section is the documented use,
        // and committing a page already committed is allowed.
        let committed = unsafe {
          VirtualAlloc(
            inner.view_ptr().cast::<u8>().add(start).cast(),
            end - start,
            MEM_COMMIT,
            PAGE_READWRITE,
          )
        };
        if committed.is_null() {
          return Err(os("VirtualAlloc(MEM_COMMIT)"));
        }
        for marked in run_start..granule {
          self.mark(marked);
        }
      }
      Ok(())
    }
  }

  impl Inner {
    /// The view's first byte, as the raw pointer the view returned (no reference to its bytes is formed).
    pub(super) fn base(&self) -> *mut u8 {
      self.view_ptr().cast::<u8>()
    }

    /// The whole view, for [`super::ExclusiveObject`] only.
    pub(super) fn bytes(&self) -> &[u8] {
      // SAFETY: the view is `len` readable bytes for as long as `self` lives, and `ExclusiveObject`'s
      // constructor keeps every other access away while the slice does.
      unsafe { std::slice::from_raw_parts(self.base(), self.len) }
    }

    /// The whole view, writable, for [`super::ExclusiveObject`] only.
    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
      // SAFETY: as for `bytes`, and `&mut self` is the only borrow of the view in this process.
      unsafe { std::slice::from_raw_parts_mut(self.base(), self.len) }
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
  use crate::words::WordRun;

  /// Shape: the test object's length.
  const LEN: usize = 4096;

  /// A small layout: a 32-bit word at 4, a 64-bit word at 8, a racy span at 64..128.
  fn test_words() -> Words {
    Words::new()
      .with(WordRun::one(4, Width::U32))
      .with(WordRun::one(8, Width::U64))
      .with(WordRun::racy(64, 64))
  }

  fn reason(result: Result<(), MemError>) -> Option<LayoutRefusal> {
    match result {
      Err(MemError::LayoutRefused { reason, .. }) => Some(reason),
      _ => None,
    }
  }

  /// AUD-29-09. Do: create an object with declared words, copy bytes in, store a word, and read both
  /// through a second mapping. Expect: the copies and the word cross; an atomic view of an undeclared
  /// offset, of the other width, or past the end is refused `NotAWord`; a copy that would touch a word is
  /// refused `TouchesWord` and leaves the word as it was.
  #[test]
  #[cfg_attr(miri, ignore)] // memfd_create / shm_open shared memory is not modelled by Miri
  fn a_shared_object_crosses_by_copies_and_declared_words_only() {
    let mut a = SharedObject::create(
      &format!("slates-mem-shared-test-a-{}", std::process::id()),
      LEN,
      test_words(),
    )
    .unwrap();
    a.write(200, &[1, 2, 3, 4]).unwrap();
    a.atomic_u64(8)
      .unwrap()
      .store(0xdead_beef, Ordering::Release);
    let b = SharedObject::open(&a.handoff().unwrap(), LEN, test_words()).unwrap();
    let mut back = [0u8; 4];
    b.read(200, &mut back).unwrap();
    assert_eq!(back, [1, 2, 3, 4]);
    assert_eq!(
      b.atomic_u64(8).unwrap().load(Ordering::Acquire),
      0xdead_beef
    );
    assert!(a.atomic_u32(4).is_ok());
    for refused in [
      a.atomic_u64(16).map(drop),
      a.atomic_u64(LEN).map(drop),
      a.atomic_u32(8).map(drop),
    ] {
      assert_eq!(reason(refused), Some(LayoutRefusal::NotAWord));
    }
    assert_eq!(
      reason(a.write(6, &[0xFF; 8])),
      Some(LayoutRefusal::TouchesWord)
    );
    assert_eq!(
      b.atomic_u64(8).unwrap().load(Ordering::Acquire),
      0xdead_beef,
      "the refused copy wrote nothing"
    );
    assert_eq!(
      reason(b.read(0, &mut [0u8; 16])),
      Some(LayoutRefusal::TouchesWord)
    );
  }

  /// AUD-29-09 (the hot path's constant-time copies). Do: declare a ring's slot bodies as a span run
  /// beside its sequence words; copy a body into span 3 and read it back through a second mapping; ask
  /// for span 4 of four, and a copy longer than a span; declare a span run that overlaps a word. Expect:
  /// the body crosses; the out-of-run span and the long copy refused `NotAWord`; the overlapping
  /// declaration refused `TouchesWord` at create, before any mapping exists.
  #[test]
  #[cfg_attr(miri, ignore)] // memfd_create / shm_open shared memory is not modelled by Miri
  fn a_declared_span_copies_in_constant_time_and_never_over_a_word() {
    let seq = WordRun::strided(0, 64, 4, Width::U64);
    let bodies = crate::words::SpanRun::strided(8, 64, 4, 56);
    let words = || Words::new().with(seq).with_span(bodies);
    let mut a = SharedObject::create(
      &format!("slates-mem-span-test-{}", std::process::id()),
      LEN,
      words(),
    )
    .unwrap();
    let span = a.resolve_span(&bodies).unwrap();
    a.write_span(span, 3, b"slot three body").unwrap();
    let b = SharedObject::open(&a.handoff().unwrap(), LEN, words()).unwrap();
    let span_b = b.resolve_span(&bodies).unwrap();
    let mut back = [0u8; 15];
    b.read_span(span_b, 3, &mut back).unwrap();
    assert_eq!(&back, b"slot three body");
    assert_eq!(
      reason(b.read_span(span_b, 4, &mut back)),
      Some(LayoutRefusal::NotAWord)
    );
    assert_eq!(
      reason(a.write_span(span, 0, &[0u8; 57])),
      Some(LayoutRefusal::NotAWord),
      "a copy longer than the span"
    );
    let over = Words::new()
      .with(seq)
      .with_span(crate::words::SpanRun::strided(4, 64, 4, 56));
    assert!(matches!(
      SharedObject::create(
        &format!("slates-mem-span-over-{}", std::process::id()),
        LEN,
        over
      ),
      Err(MemError::LayoutRefused {
        reason: LayoutRefusal::TouchesWord,
        ..
      })
    ));
  }

  /// AUD-29-09. Do: create an object whose layout overlaps two widths on one byte, and open one with a
  /// word past its end. Expect: both refused before any mapping is handed out.
  #[test]
  #[cfg_attr(miri, ignore)] // memfd_create / shm_open shared memory is not modelled by Miri
  fn a_layout_that_would_mix_accesses_is_refused_at_create() {
    let overlapping = test_words().with(WordRun::one(12, Width::U32));
    let created = SharedObject::create(
      &format!("slates-mem-shared-test-overlap-{}", std::process::id()),
      LEN,
      overlapping,
    );
    assert!(matches!(
      created,
      Err(MemError::LayoutRefused {
        reason: LayoutRefusal::Overlap,
        ..
      })
    ));
    let past = Words::new().with(WordRun::one(LEN, Width::U64));
    assert!(matches!(
      SharedObject::create(
        &format!("slates-mem-shared-test-past-{}", std::process::id()),
        LEN,
        past
      ),
      Err(MemError::LayoutRefused {
        reason: LayoutRefusal::OutOfRange,
        ..
      })
    ));
  }

  /// §4.8 and AUD-29-09: a sparse object is reached by copies far into a large layout, its racy span only
  /// by racy copies, and a second mapping sees all of it. Do: write plain bytes deep inside, a racy
  /// payload and a word; reopen by the handoff. Expect: each back through its own access; a plain copy of
  /// the racy span and a racy copy of plain bytes both refused; a copy ending past the object refused.
  #[test]
  #[cfg_attr(miri, ignore)] // memfd_create / shm_open shared memory is not modelled by Miri
  fn a_sparse_object_is_reached_by_its_declared_accesses_through_a_second_mapping() {
    /// Shape: a layout far larger than what the test touches, as the anchor segment is.
    const SPARSE_LEN: usize = 1 << 30;
    /// Shape: an offset deep inside it.
    const FAR: usize = SPARSE_LEN - (1 << 20);
    let mut a = SparseObject::create(
      &format!("slates-mem-sparse-test-{}", std::process::id()),
      SPARSE_LEN,
      test_words(),
    )
    .unwrap();
    a.write(FAR, &[4, 3, 2, 1]).unwrap();
    a.write_racy(64, b"published").unwrap();
    a.atomic_u64(8).unwrap().store(0x5eed, Ordering::Release);
    let b = SparseObject::open(&a.handoff().unwrap(), SPARSE_LEN, test_words()).unwrap();
    let mut far = [0u8; 4];
    b.read(FAR, &mut far).unwrap();
    assert_eq!(far, [4, 3, 2, 1]);
    let mut payload = [0u8; 9];
    b.read_racy(64, &mut payload).unwrap();
    assert_eq!(&payload, b"published");
    assert_eq!(b.atomic_u64(8).unwrap().load(Ordering::Acquire), 0x5eed);
    let mut untouched = [9u8; 8];
    b.read(1 << 20, &mut untouched).unwrap();
    assert_eq!(untouched, [0; 8], "an untouched range reads zeros");
    assert_eq!(
      reason(b.read(64, &mut [0u8; 4])),
      Some(LayoutRefusal::TouchesWord)
    );
    assert_eq!(
      reason(b.read_racy(FAR, &mut [0u8; 4])),
      Some(LayoutRefusal::NotAWord)
    );
    assert_eq!(
      reason(b.read(SPARSE_LEN - 2, &mut [0u8; 4])),
      Some(LayoutRefusal::OutOfRange)
    );
  }

  /// AC (docs/bugs/2026-09-14-segment-handoff-descriptor-owned-twice.md): one handoff is attached more
  /// than once by one process (the daemon reads the anchor's profile, then starts, then maps per shard),
  /// and each attach owns its own descriptor — dropping two of them closes two duplicates, never the
  /// inherited number twice (which aborted the process), and a third attach still finds the number open
  /// (which failed `EBADF` before). Linux only: the other platforms hand off a name and open it afresh.
  #[test]
  #[cfg(target_os = "linux")]
  #[cfg_attr(miri, ignore)] // memfd_create shared memory is not modelled by Miri
  fn two_attaches_from_one_handoff_each_own_their_descriptor() {
    let mut a = SharedObject::create("slates-mem-shared-test-twice", LEN, Words::new()).unwrap();
    a.write(0, &[9, 8, 7, 6]).unwrap();
    let handoff = a.handoff().unwrap();
    let read = |object: &SharedObject| {
      let mut back = [0u8; 4];
      object.read(0, &mut back).unwrap();
      back
    };
    let first = SharedObject::open(&handoff, LEN, Words::new()).unwrap();
    let second = SharedObject::open(&handoff, LEN, Words::new()).unwrap();
    assert_eq!(read(&first), [9, 8, 7, 6]);
    assert_eq!(read(&second), [9, 8, 7, 6]);
    drop(first);
    drop(second);
    let third =
      SharedObject::open(&handoff, LEN, Words::new()).expect("the inherited number is still open");
    assert_eq!(read(&third), [9, 8, 7, 6]);
  }
}
