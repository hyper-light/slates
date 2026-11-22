//! The learned descriptor ceiling (A-837 part 2; Ada 2026-11-21, "a way to avoid descriptor exhaustion"): the number
//! of descriptors this process held when it first met exhaustion, from any open, and the ceiling every budget reads,
//! the lower of that and the rlimit.
//!
//! **Why learned.** The rlimit is not the only bound. kiln's daemon on adacachy met `EMFILE` from `accept`,
//! `memfd_create` and `eventfd` at about 508,000 descriptors, half its `RLIMIT_NOFILE` of 1,048,576: something past the
//! rlimit (a kernel or container bound slates cannot read) refused first. So the first exhaustion met is a measurement,
//! kept for the process's life: a budget derived from the rlimit alone would plan for descriptors the host will not give.
//!
//! **Counted without a descriptor.** At exhaustion there is none to open, so the count is read where it costs none:
//! macOS's `proc_pidinfo(PROC_PIDLISTFDS)` sized with no buffer, Windows' `GetProcessHandleCount`. Linux has no such
//! call (`/proc/self/fd` must be opened to be read), so a spare descriptor is held from the daemon's start
//! ([`reserve_spare`]), given up to read `/proc/self/fd`, and taken again: the classic answer to `EMFILE` at `accept`.
//!
//! **Cold.** Only the first exhaustion counts, so the path runs once in a process's life; every later caller reads the
//! one word.

use core::sync::atomic::{AtomicU64, Ordering};

/// The count at the first exhaustion met; zero until then (a process exhausted at zero descriptors holds none to
/// count, so zero means "not learned").
static LEARNED: AtomicU64 = AtomicU64::new(0);

/// The descriptor count at which this process first met exhaustion, once it has.
#[must_use]
pub fn learned() -> Option<u64> {
  let count = LEARNED.load(Ordering::Acquire);
  (count > 0).then_some(count)
}

/// The ceiling a descriptor budget reads (A-837): the lower of the learned count and the rlimit
/// ([`crate::facts::descriptor_limit`]); either alone where the other is unknown.
#[must_use]
pub fn ceiling() -> Option<u64> {
  lower(learned(), crate::facts::descriptor_limit())
}

/// The lower of two bounds, `None` meaning none binds.
#[must_use]
pub fn lower(first: Option<u64>, second: Option<u64>) -> Option<u64> {
  match (first, second) {
    (Some(first), Some(second)) => Some(first.min(second)),
    (bound, None) | (None, bound) => bound,
  }
}

/// Records that an open met exhaustion: the first time, counts the descriptors held now and keeps the count; later,
/// keeps the first. The count learned, `None` where it could not be read.
pub fn note_exhaustion() -> Option<u64> {
  if let Some(count) = learned() {
    return Some(count);
  }
  let count = open_count()?.max(1);
  let _first = LEARNED.compare_exchange(0, count, Ordering::AcqRel, Ordering::Acquire);
  learned()
}

/// Whether an `errno` names descriptor exhaustion: the process's (`EMFILE`) or the system's (`ENFILE`).
#[cfg(unix)]
#[must_use]
pub fn is_exhaustion_errno(errno: i32) -> bool {
  errno == libc::EMFILE || errno == libc::ENFILE
}

/// Format: Windows' `ERROR_TOO_MANY_OPEN_FILES` (winerror.h, 4), "The system cannot open the file", the handle table's
/// exhaustion as Win32 names it; and `ERROR_NO_SYSTEM_RESOURCES` (1450), the paged pool a handle lives in spent.
const WINDOWS_EXHAUSTION: [u32; 2] = [4, 1450];

/// Whether a Win32 error code names handle exhaustion ([`WINDOWS_EXHAUSTION`]); cfg-free, so every host tests it.
#[must_use]
pub fn is_exhaustion_win32(code: u32) -> bool {
  WINDOWS_EXHAUSTION.contains(&code)
}

/// The descriptors this process holds, counted without opening one (macOS).
#[cfg(target_os = "macos")]
fn open_count() -> Option<u64> {
  let pid = i32::try_from(std::process::id()).ok()?;
  // SAFETY: `proc_pidinfo` with a null buffer and a zero size writes nothing and answers the bytes the descriptor list
  // would take (libproc's documented sizing call); `pid` is this process.
  let bytes =
    unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, core::ptr::null_mut(), 0) };
  let bytes = u64::try_from(bytes).ok()?;
  let each = u64::try_from(core::mem::size_of::<libc::proc_fdinfo>()).ok()?;
  bytes.checked_div(each)
}

/// The descriptors this process holds (Linux): the spare given up so `/proc/self/fd` can be opened, its entries
/// counted less the directory's own, the spare taken again.
#[cfg(target_os = "linux")]
fn open_count() -> Option<u64> {
  give_up_spare();
  let count = std::fs::read_dir("/proc/self/fd").ok().map(|entries| {
    u64::try_from(entries.count())
      .unwrap_or(u64::MAX)
      .saturating_sub(1)
  });
  reserve_spare();
  count
}

/// The paired arm for another Unix: no count without a descriptor.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn open_count() -> Option<u64> {
  None
}

/// The handles this process holds (Windows).
#[cfg(windows)]
fn open_count() -> Option<u64> {
  let mut count: u32 = 0;
  // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid in this process and closed by nothing;
  // `GetProcessHandleCount` writes one `u32` through the valid pointer to `count`.
  let done = unsafe {
    windows_sys::Win32::System::Threading::GetProcessHandleCount(
      windows_sys::Win32::System::Threading::GetCurrentProcess(),
      &raw mut count,
    )
  };
  (done != 0).then_some(u64::from(count))
}

/// The spare descriptor Linux's count gives up (`-1` while none is held): a raw descriptor this module alone opened
/// and alone closes, so the swap that takes it hands its one owner.
#[cfg(target_os = "linux")]
static SPARE: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);

/// Holds the spare descriptor Linux's count needs ([`open_count`]), once a daemon starts; again after each count. A
/// process that already holds one keeps it. Nothing on other platforms, which count without one.
pub fn reserve_spare() {
  #[cfg(target_os = "linux")]
  {
    use std::os::fd::IntoRawFd;
    if SPARE.load(Ordering::Acquire) >= 0 {
      return;
    }
    let Ok(spare) = rustix::fs::open(
      c"/dev/null",
      rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
      rustix::fs::Mode::empty(),
    ) else {
      return;
    };
    let raw = spare.into_raw_fd();
    if SPARE
      .compare_exchange(-1, raw, Ordering::AcqRel, Ordering::Acquire)
      .is_err()
    {
      close_raw(raw);
    }
  }
}

/// Gives the spare up (Linux), so the count's own open finds a free descriptor.
#[cfg(target_os = "linux")]
fn give_up_spare() {
  let raw = SPARE.swap(-1, Ordering::AcqRel);
  if raw >= 0 {
    close_raw(raw);
  }
}

/// Closes a raw descriptor this module opened and now alone holds.
#[cfg(target_os = "linux")]
fn close_raw(raw: i32) {
  use std::os::fd::FromRawFd;
  // SAFETY: `raw` came from `into_raw_fd` of a descriptor this module opened, and the atomic swap or the failed
  // exchange that produced it leaves this call its only holder, so it is closed once and never used again.
  drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) });
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-837 part 2. Do: take the lower of two bounds in every shape, and judge Windows' codes. Expect: the smaller where
  /// both bind, either where one does, none where neither; handle exhaustion's two codes and no other.
  #[test]
  fn a837_the_ceiling_is_the_lower_bound_and_windows_names_exhaustion_by_two_codes() {
    assert_eq!(lower(Some(508_000), Some(1_048_576)), Some(508_000));
    assert_eq!(lower(Some(2_048), Some(1_024)), Some(1_024));
    assert_eq!(lower(None, Some(1_024)), Some(1_024));
    assert_eq!(lower(Some(508_000), None), Some(508_000));
    assert_eq!(lower(None, None), None);
    assert!(is_exhaustion_win32(4) && is_exhaustion_win32(1450));
    assert!(!is_exhaustion_win32(2) && !is_exhaustion_win32(5));
  }

  /// A-837 part 2. Do: note exhaustion twice. Expect: the first note counts the descriptors held (at least the three
  /// standard ones a test process holds) where the platform can count, and the second keeps the first count.
  #[cfg(any(target_os = "linux", target_os = "macos", windows))]
  #[test]
  fn a837_the_first_exhaustion_noted_is_counted_and_kept() {
    reserve_spare();
    let first = note_exhaustion();
    assert!(first.is_some_and(|count| count >= 3), "{first:?}");
    assert_eq!(note_exhaustion(), first);
    assert_eq!(learned(), first);
  }
}
