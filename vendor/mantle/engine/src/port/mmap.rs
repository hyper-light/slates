//! RocksDB's `port/mmap.{h,cc}`: memory mapped anonymously, zero-filled by the OS and made
//! resident only where it is written (`MemMapping::AllocateLazyZeroed`), here as the 8-byte words
//! the memtable's arena stores into (memory/arena.rs).
//!
//! The OS interface (CLAUDE.md §7): on Unix `mmap(MAP_PRIVATE | MAP_ANONYMOUS)` and `munmap`
//! (mmap(2): anonymous mappings are initialized to zero; the pages are allocated on first touch);
//! on Windows a page-file-backed section, `CreateFileMappingW(INVALID_HANDLE_VALUE, …,
//! PAGE_READWRITE | SEC_COMMIT)` and `MapViewOfFile`, as RocksDB's port does (the Win32 reference:
//! a section's pages are zero-initialized and backed by the page file, committed as charge, and
//! made resident when touched). RocksDB's `AllocateHuge` waits for the arena's huge-page option.
#![allow(unsafe_code)]

use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;

use crate::error::Error;

/// The size of one word, the unit a mapping is counted in.
const WORD: usize = std::mem::size_of::<AtomicU64>();

/// Has the C library's allocator give a freed large block's memory back to the OS when it is
/// freed, as it does until its first such block is freed. glibc's `malloc` maps each block of at
/// least its mmap threshold, 128 KiB to begin with, and unmaps it when freed; but each mapped block
/// up to 32 MiB that is freed raises the threshold to its size, so later blocks of that size come
/// from its arenas and stay there once freed (mallopt(3), `M_MMAP_THRESHOLD`): 1.3-2.8 GiB held
/// at 100M, outside any account (docs/research/65 §6). Setting the threshold, here to its initial
/// value, turns the adjustment off (mallopt(3)). For a process's owner to call at its start,
/// before its threads allocate; elsewhere than glibc it does nothing.
pub fn return_freed_blocks() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        /// glibc's `M_MMAP_THRESHOLD` parameter number (malloc.h).
        const M_MMAP_THRESHOLD: i32 = -3;
        /// glibc's initial mmap threshold, `DEFAULT_MMAP_THRESHOLD_MIN` (mallopt(3): 128*1024).
        const INITIAL_MMAP_THRESHOLD: i32 = 128 * 1024;
        // SAFETY: mallopt(3) takes a parameter number and a value and changes only the
        // allocator's settings; a refusal (0) leaves them as they were.
        unsafe {
            mallopt(M_MMAP_THRESHOLD, INITIAL_MMAP_THRESHOLD);
        }
    }
}

/// The size of the OS's memory pages, the least a mapping takes: below it a block is not mapped
/// (memory/arena.rs).
pub fn page_size() -> usize {
    #[cfg(unix)]
    {
        rustix::param::page_size()
    }
    #[cfg(windows)]
    {
        let mut info = windows_sys::Win32::System::SystemInformation::SYSTEM_INFO::default();
        // SAFETY: GetSystemInfo writes the SYSTEM_INFO it is given, which lives for the call.
        unsafe { windows_sys::Win32::System::SystemInformation::GetSystemInfo(&mut info) };
        usize::try_from(info.dwPageSize).unwrap_or(usize::MAX)
    }
}

/// The size of a transparent huge page: the kernel's own figure
/// (`/sys/kernel/mm/transparent_hugepage/hpage_pmd_size`, Documentation/admin-guide/mm/transhuge.rst),
/// read once; where it cannot be read, what one page-table page of 8-byte entries maps, which a PMD
/// entry covers on the 64-bit architectures Linux runs mantle on (x86_64 and aarch64: 2 MiB on 4 KiB
/// pages, 32 MiB on 16 KiB, 512 MiB on 64 KiB).
#[cfg(target_os = "linux")]
pub fn huge_page_size() -> usize {
    static SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| {
                let page = page_size();
                page.saturating_mul(page.checked_div(8).unwrap_or(0))
            })
    })
}

/// Whether the kernel gives transparent huge pages only to memory that asks for them
/// (`/sys/kernel/mm/transparent_hugepage/enabled` set to `madvise`,
/// Documentation/admin-guide/mm/transhuge.rst), read once; false where it cannot be read.
#[cfg(target_os = "linux")]
fn huge_pages_on_request() -> bool {
    static ON_REQUEST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON_REQUEST.get_or_init(|| {
        std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .is_ok_and(|s| s.contains("[madvise]"))
    })
}

/// The bytes before a page buffer's first in its mapping ([`PageBuf`]), so its pages begin 16 bytes
/// into a cache line, as a block from the C library's allocator begins (glibc's `MALLOC_ALIGNMENT`
/// and chunk header, 16 bytes on 64-bit targets) and as data-layer's cache held them. Measured on
/// the VM (fixed 32 GiB caches, against data-layer): seeks at 1M ran 0.963 of data-layer's with the
/// pages on a line's start, 0.986 16 bytes in and 0.961 64 bytes in (eight rounds, R0 7381-7388),
/// and at 10M 0.963 and 1.000 (six rounds, R0 7391-7396) (docs/research/65 §9).
#[cfg(target_os = "linux")]
const PAGE_OFFSET: usize = 16;

/// Memory for pages kept in memory: the store's cache's chunks and the room it lends a worker's
/// pages ([`crate::store::cache`]). A buffer of bytes grown within the capacity it was made with,
/// as a `Vec<u8>` whose capacity is reserved once, zero until written and resident only where
/// written. On Linux it begins on a transparent huge page's boundary ([`huge_page_size`]), so the
/// kernel can back each whole huge page of it with one (Documentation/admin-guide/mm/transhuge.rst),
/// asked for with `madvise(MADV_HUGEPAGE)` where the kernel gives them only to memory that asks. A
/// buffer of a huge page from the C library's allocator lies on none: its block begins past the
/// allocator's header, and the kernel aligns an anonymous mapping for huge pages only when its
/// length is a whole number of them (mm/mmap.c `__get_unmapped_area`, as the VM's 6.12 kernel
/// has it). The cache's buffers of a
/// huge page each held the engine's memory 51% on huge pages where one buffer a loan held 86%, and
/// seeks, each copying pages out of them, ran 5-10% slower (docs/research/65 §9). Elsewhere it is
/// a `Vec<u8>`: no OS mantle runs on gives other memory huge pages by itself.
pub struct PageBuf {
    #[cfg(target_os = "linux")]
    addr: Option<NonNull<u8>>,
    /// The bytes mapped, a whole number of the OS's pages; the bytes usable, the capacity asked
    /// for; and the bytes in use.
    #[cfg(target_os = "linux")]
    mapped: usize,
    #[cfg(target_os = "linux")]
    cap: usize,
    #[cfg(target_os = "linux")]
    len: usize,
    #[cfg(not(target_os = "linux"))]
    vec: Vec<u8>,
}

// SAFETY: the mapping is owned by this value alone, reached only through `&self` and `&mut self`
// borrows of it, and unmapped only in `drop`; moving it to another thread moves that ownership.
#[cfg(target_os = "linux")]
unsafe impl Send for PageBuf {}

impl PageBuf {
    /// A buffer of no bytes, which holds no memory.
    pub const fn new() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                addr: None,
                mapped: 0,
                cap: 0,
                len: 0,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self { vec: Vec::new() }
        }
    }

    /// A buffer of `cap` bytes' capacity, none in use; none when the OS refuses the memory.
    pub fn with_capacity(cap: usize) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            if cap == 0 {
                return Some(Self::new());
            }
            let (addr, mapped) = map_on_huge_pages(cap.checked_add(PAGE_OFFSET)?).ok()?;
            let bytes = NonNull::new(addr.as_ptr().wrapping_add(PAGE_OFFSET))?;
            Some(Self {
                addr: Some(bytes),
                mapped,
                cap,
                len: 0,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut vec = Vec::new();
            vec.try_reserve_exact(cap).ok()?;
            Some(Self { vec })
        }
    }

    /// The bytes it can hold.
    pub fn capacity(&self) -> usize {
        #[cfg(target_os = "linux")]
        {
            self.cap
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.vec.capacity()
        }
    }

    /// Takes every byte out of use, its memory kept.
    pub fn clear(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.len = 0;
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.vec.clear();
        }
    }

    /// Appends `data` within the capacity; past it, nothing, and false.
    pub fn extend_from_slice(&mut self, data: &[u8]) -> bool {
        #[cfg(target_os = "linux")]
        {
            let start = self.len;
            let Some(end) = start.checked_add(data.len()).filter(|&end| end <= self.cap) else {
                return false;
            };
            let Some(room) = self.whole().get_mut(start..end) else {
                return false;
            };
            room.copy_from_slice(data);
            self.len = end;
            true
        }
        #[cfg(not(target_os = "linux"))]
        {
            let fits = self
                .vec
                .len()
                .checked_add(data.len())
                .is_some_and(|end| end <= self.vec.capacity());
            if fits {
                self.vec.extend_from_slice(data);
            }
            fits
        }
    }

    /// Sets the bytes in use to `len` within the capacity, any new ones `value`; past it, nothing,
    /// and false.
    pub fn resize(&mut self, len: usize, value: u8) -> bool {
        #[cfg(target_os = "linux")]
        {
            if len > self.cap {
                return false;
            }
            let start = self.len;
            if let Some(new) = self.whole().get_mut(start..len) {
                new.fill(value);
            }
            self.len = len;
            true
        }
        #[cfg(not(target_os = "linux"))]
        {
            let fits = len <= self.vec.capacity();
            if fits {
                self.vec.resize(len, value);
            }
            fits
        }
    }

    /// Every byte of its capacity, those not in use too.
    #[cfg(target_os = "linux")]
    fn whole(&mut self) -> &mut [u8] {
        match self.addr {
            // SAFETY: `addr` is `PAGE_OFFSET` bytes into a live mapping of `mapped` bytes, at
            // least `cap` of them past it, zero-filled by the OS and written only through these
            // slices, so every byte is initialized; unmapped only in `drop`, which the borrow of
            // `self` keeps from running.
            Some(addr) => unsafe { std::slice::from_raw_parts_mut(addr.as_ptr(), self.cap) },
            None => &mut [],
        }
    }
}

impl Default for PageBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PageBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageBuf")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

impl std::ops::Deref for PageBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        #[cfg(target_os = "linux")]
        {
            match self.addr {
                // SAFETY: as `whole`: a live mapping of at least `len` initialized bytes, unmapped
                // only in `drop`, which the borrow of `self` keeps from running.
                Some(addr) => unsafe { std::slice::from_raw_parts(addr.as_ptr(), self.len) },
                None => &[],
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            &self.vec
        }
    }
}

impl std::ops::DerefMut for PageBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        #[cfg(target_os = "linux")]
        {
            let len = self.len;
            self.whole().get_mut(..len).unwrap_or_default()
        }
        #[cfg(not(target_os = "linux"))]
        {
            &mut self.vec
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PageBuf {
    fn drop(&mut self) {
        let Some(addr) = self.addr.take() else {
            return;
        };
        let start = addr.as_ptr().wrapping_sub(PAGE_OFFSET);
        // SAFETY: unmaps exactly the mapping `map_on_huge_pages` left, which begins `PAGE_OFFSET`
        // bytes before the buffer's first and which no borrow outlives. A failure leaves the pages
        // mapped: nothing can act on it in a drop.
        let _ = unsafe { rustix::mm::munmap(start.cast(), self.mapped) };
    }
}

/// A private anonymous mapping of `cap` bytes, rounded up to the OS's pages, beginning on a
/// transparent huge page's boundary: mapped a huge page longer than it needs, the parts before
/// the first boundary and past the end given back, as jemalloc aligns its chunks
/// (`pages_map` and `pages_trim`). Asked for huge pages where the kernel gives them only on
/// request. Returns the mapping and its length.
#[cfg(target_os = "linux")]
fn map_on_huge_pages(cap: usize) -> Result<(NonNull<u8>, usize), ()> {
    use rustix::mm::{Advice, MapFlags, ProtFlags, madvise, mmap_anonymous, munmap};
    let page = page_size().max(1);
    let huge = huge_page_size().max(page);
    let mapped = cap.checked_next_multiple_of(page).ok_or(())?;
    let span = mapped.checked_add(huge.saturating_sub(page)).ok_or(())?;
    // SAFETY: a new private anonymous mapping at an address the kernel chooses touches no memory
    // this process already uses; `span` is nonzero.
    let base = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            span,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE,
        )
    }
    .map_err(|_| ())?
    .cast::<u8>();
    let head = base
        .addr()
        .checked_next_multiple_of(huge)
        .and_then(|start| start.checked_sub(base.addr()))
        .ok_or(())?;
    let start = base.wrapping_add(head);
    let tail = span.saturating_sub(head).saturating_sub(mapped);
    if head > 0 {
        // SAFETY: gives back the mapping's first `head` bytes, before the boundary, which nothing
        // uses; a failure leaves them mapped and untouched.
        let _ = unsafe { munmap(base.cast(), head) };
    }
    if tail > 0 {
        // SAFETY: gives back the mapping's last `tail` bytes, past the buffer's, which nothing
        // uses; a failure leaves them mapped and untouched.
        let _ = unsafe { munmap(start.wrapping_add(mapped).cast(), tail) };
    }
    if huge_pages_on_request() {
        // SAFETY: advice on the buffer's own mapping, which changes how the kernel backs it and
        // not what it holds; a refusal leaves it on the base pages.
        let _ = unsafe { madvise(start.cast(), mapped, Advice::LinuxHugepage) };
    }
    NonNull::new(start).map(|start| (start, mapped)).ok_or(())
}

/// `words` 8-byte words of anonymous memory, zero until written and resident only where written,
/// released when dropped.
pub struct LazyZeroed {
    /// The mapping's first word; `None` for a mapping of no words, which maps nothing.
    addr: Option<NonNull<AtomicU64>>,
    words: usize,
    /// The section the view maps, closed after the view is unmapped.
    #[cfg(windows)]
    section: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the mapping is owned by this value alone and every access to it is through `&AtomicU64`
// (`words`), whose loads and stores are atomic, so sharing or moving it across threads cannot
// race; it is unmapped only in `drop`, when no borrow of it is left.
unsafe impl Send for LazyZeroed {}
// SAFETY: as for `Send`: shared access is only through atomics.
unsafe impl Sync for LazyZeroed {}

impl std::fmt::Debug for LazyZeroed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyZeroed")
            .field("words", &self.words)
            .finish()
    }
}

impl LazyZeroed {
    /// Maps `words` zeroed words, or refuses with the OS's error.
    pub fn allocate(words: usize) -> Result<Self, Error> {
        let refused = |_| Error::LimitExceeded {
            what: "memtable arena block (the OS refused the mapping)",
            limit: u64::try_from(words.saturating_mul(WORD)).unwrap_or(u64::MAX),
        };
        let bytes = words.checked_mul(WORD).ok_or_else(|| refused(()))?;
        if bytes == 0 {
            return Ok(Self::empty());
        }
        Self::map(bytes, words).map_err(refused)
    }

    fn empty() -> Self {
        Self {
            addr: None,
            words: 0,
            #[cfg(windows)]
            section: std::ptr::null_mut(),
        }
    }

    #[cfg(unix)]
    fn map(bytes: usize, words: usize) -> Result<Self, ()> {
        use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
        // SAFETY: a new private anonymous mapping at an address the kernel chooses touches no
        // memory this process already uses; `bytes` is nonzero.
        let addr = unsafe {
            mmap_anonymous(
                std::ptr::null_mut(),
                bytes,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE,
            )
        }
        .map_err(|_| ())?;
        let addr = NonNull::new(addr.cast::<AtomicU64>()).ok_or(())?;
        Ok(Self {
            addr: Some(addr),
            words,
        })
    }

    #[cfg(windows)]
    fn map(bytes: usize, words: usize) -> Result<Self, ()> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, SEC_COMMIT,
        };
        let size = u64::try_from(bytes).map_err(|_| ())?;
        let high = u32::try_from(size >> 32).map_err(|_| ())?;
        let low = u32::try_from(size & u64::from(u32::MAX)).map_err(|_| ())?;
        // SAFETY: a section backed by the page file (INVALID_HANDLE_VALUE), unnamed, with no
        // security attributes; the handle is closed in `drop` or below on failure.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_COMMIT,
                high,
                low,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(());
        }
        // SAFETY: maps the whole section just created, writable; unmapped in `drop`.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes) };
        let Some(addr) = NonNull::new(view.Value.cast::<AtomicU64>()) else {
            // SAFETY: closes the section this function opened, which nothing else holds.
            unsafe { CloseHandle(section) };
            return Err(());
        };
        Ok(Self {
            addr: Some(addr),
            words,
            section,
        })
    }

    /// The mapping's words.
    pub fn words(&self) -> &[AtomicU64] {
        match self.addr {
            // SAFETY: `addr` is the start of a live mapping of `words` words: page-aligned, so
            // aligned for `AtomicU64`, which has `u64`'s size and alignment; zero-filled by the
            // OS, and every bit pattern of a `u64` is a valid `AtomicU64`; unmapped only in
            // `drop`, which the borrow of `self` this slice holds keeps from running.
            Some(addr) => unsafe { std::slice::from_raw_parts(addr.as_ptr(), self.words) },
            None => &[],
        }
    }
}

impl Drop for LazyZeroed {
    fn drop(&mut self) {
        let Some(addr) = self.addr.take() else {
            return;
        };
        #[cfg(unix)]
        {
            let bytes = self.words.saturating_mul(WORD);
            // SAFETY: unmaps exactly the mapping `map` made, which no borrow outlives (`words`
            // borrows `self`). A failure leaves the pages mapped: nothing can act on it in a
            // drop, and RocksDB's port ignores it too [R port/mmap.cc:27-33].
            let _ = unsafe { rustix::mm::munmap(addr.as_ptr().cast(), bytes) };
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            // SAFETY: unmaps the view `map` made and closes its section, neither held elsewhere;
            // failures are ignored as on Unix [R port/mmap.cc:19-25].
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: addr.as_ptr().cast(),
                });
                CloseHandle(self.section);
            }
        }
    }
}

#[cfg(test)]
mod page_buf_tests {
    use super::*;

    /// A buffer grows within the capacity it was made with, as a `Vec<u8>` with its capacity
    /// reserved, and refuses, unchanged, past it.
    #[test]
    fn a_page_buffer_grows_within_its_capacity_and_refuses_past_it() {
        let mut b = PageBuf::with_capacity(3 * 4096).unwrap();
        assert_eq!((b.len(), b.capacity()), (0, 3 * 4096));
        assert!(b.extend_from_slice(&[7u8; 100]));
        assert!(b.resize(4096, 0));
        assert_eq!(b.len(), 4096);
        assert_eq!(&b[..100], &[7u8; 100][..]);
        assert!(b[100..].iter().all(|&x| x == 0));
        b[4095] = 9;
        assert!(b.extend_from_slice(&[5u8; 4096]));
        assert_eq!(b[4095], 9);
        assert_eq!(b.len(), 2 * 4096);
        assert!(!b.extend_from_slice(&[1u8; 2 * 4096]), "past the capacity");
        assert!(!b.resize(3 * 4096 + 1, 0), "past the capacity");
        assert_eq!(b.len(), 2 * 4096);
        // Cleared and filled again: the bytes a resize adds are its value, whatever they held.
        b.clear();
        assert!(b.is_empty());
        assert!(b.resize(4096, 3));
        assert!(b.iter().all(|&x| x == 3));
        let empty = PageBuf::with_capacity(0).unwrap();
        assert_eq!((empty.len(), empty.capacity()), (0, 0));
        assert!(PageBuf::new().is_empty());
    }

    /// On Linux a buffer's mapping begins on a transparent huge page's boundary, whatever the C
    /// library's allocator would have placed it at, and its bytes `PAGE_OFFSET` past it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_page_buffer_begins_on_a_huge_page_boundary() {
        let huge = huge_page_size();
        for pages in [1usize, 2, 5] {
            let b = PageBuf::with_capacity(pages * huge + 4096).unwrap();
            let addr = b.addr.unwrap().as_ptr().addr();
            assert_eq!(
                addr % huge,
                PAGE_OFFSET,
                "a buffer of {pages} huge pages at {addr:#x}"
            );
            assert_eq!(b.capacity(), pages * huge + 4096);
        }
    }
}
