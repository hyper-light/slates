//! The calling thread's own context-switch count, where the OS keeps one the thread can read cheaply (§4.1 the wake
//! probe, §4.3 the shard's online wake estimate; A-223). A wait that slept switched the thread out; a wait that found
//! its wake already pending returned without switching. That is the one question both consumers ask of a wait, and
//! the answer must come per wait, since a wake timed across a wait that never slept is an on-CPU handoff (0.1–0.8 µs)
//! taken for a sleeper's wake (6–10 µs on CI's Windows runners, 2026-11-06).
//!
//! - **Windows:** `ReadThreadProfilingData` after `EnableThreadProfiling(THREAD_PROFILING_FLAG_DISPATCH)` on the
//!   calling thread [B: Microsoft Learn, "EnableThreadProfiling", "ReadThreadProfilingData", "PERFORMANCE_DATA"]:
//!   `ContextSwitchCount`, the switches since profiling was enabled. It counts every switch, a preemption too, so a wait
//!   preempted before it blocked reads as slept; that needs the thread preempted inside the few hundred nanoseconds
//!   between a wait's entry and its block. Windows has no other per-thread switch count short of a system-wide
//!   snapshot, and its per-thread times tick at about 15.6 ms. Profiling is enabled once per thread, on its first
//!   read, and disabled when the thread ends.
//! - **Elsewhere:** `None`. Linux's waits are judged by `getrusage(RUSAGE_THREAD)`'s voluntary switches in `slates-rt`
//!   and the probe's by `/proc` state; macOS's probe by `thread_info`.

/// The calling thread's context switches since it first asked, where the OS counts them for the thread itself
/// (Windows); `None` elsewhere, or when the OS refuses the profiling.
#[cfg(windows)]
pub fn switches_now() -> Option<u64> {
  windows::PROFILING.with(|profiling| profiling.as_ref().and_then(windows::Profiling::switches))
}

/// No per-thread switch count read here (see the module doc).
#[cfg(not(windows))]
pub fn switches_now() -> Option<u64> {
  None
}

#[cfg(windows)]
mod windows {
  use windows_sys::Win32::Foundation::HANDLE;
  use windows_sys::Win32::System::Performance::HardwareCounterProfiling::{
    DisableThreadProfiling, EnableThreadProfiling, PERFORMANCE_DATA, ReadThreadProfilingData,
  };
  use windows_sys::Win32::System::Threading::GetCurrentThread;

  /// Format: `THREAD_PROFILING_FLAG_DISPATCH` (winnt.h): collect the thread's dispatch data, its switch count.
  const THREAD_PROFILING_FLAG_DISPATCH: u32 = 1;
  /// Format: `READ_THREAD_PROFILING_FLAG_DISPATCHING` (winnt.h): read the dispatch data.
  const READ_THREAD_PROFILING_FLAG_DISPATCHING: u32 = 1;
  /// Format: `PERFORMANCE_DATA_VERSION` (winnt.h), the record's version the caller states.
  const PERFORMANCE_DATA_VERSION: u8 = 1;
  /// Format: `ERROR_SUCCESS`, what both calls answer when they succeed (they return a Win32 error code).
  const ERROR_SUCCESS: u32 = 0;

  /// The calling thread's profiling handle, disabled when the thread ends.
  pub(super) struct Profiling(HANDLE);

  impl Profiling {
    /// Enables dispatch profiling on the calling thread; `None` when the OS refuses.
    fn enable() -> Option<Profiling> {
      let mut handle: HANDLE = std::ptr::null_mut();
      // SAFETY: the calling thread's pseudo-handle, as `EnableThreadProfiling` requires (it profiles only the calling
      // thread); no hardware counters asked; the handle is this frame's own, written by the call, and read only when
      // the call answers success.
      let answer = unsafe {
        EnableThreadProfiling(GetCurrentThread(), THREAD_PROFILING_FLAG_DISPATCH, 0, &mut handle)
      };
      (answer == ERROR_SUCCESS && !handle.is_null()).then_some(Profiling(handle))
    }

    /// The thread's context switches since profiling was enabled.
    pub(super) fn switches(&self) -> Option<u64> {
      let mut data = PERFORMANCE_DATA {
        Size: u16::try_from(std::mem::size_of::<PERFORMANCE_DATA>()).ok()?,
        Version: PERFORMANCE_DATA_VERSION,
        ..PERFORMANCE_DATA::default()
      };
      // SAFETY: this value's own live handle, read on the thread that enabled it (a thread-local); `data` is this
      // frame's own record, its size and version stated as the call requires; read only when the call succeeds.
      let answer =
        unsafe { ReadThreadProfilingData(self.0, READ_THREAD_PROFILING_FLAG_DISPATCHING, &mut data) };
      (answer == ERROR_SUCCESS).then_some(u64::from(data.ContextSwitchCount))
    }
  }

  impl Drop for Profiling {
    fn drop(&mut self) {
      // SAFETY: the handle is this value's own, made by `EnableThreadProfiling` on this thread and disabled only here,
      // as the thread's locals are destroyed.
      unsafe { DisableThreadProfiling(self.0) };
    }
  }

  thread_local! {
    /// The calling thread's profiling, enabled on its first read.
    pub(super) static PROFILING: Option<Profiling> = Profiling::enable();
  }
}

#[cfg(test)]
mod tests {
  use super::switches_now;

  /// Shape: read pairs the test tries for one with no switch between: a preemption between two reads a few hundred
  /// nanoseconds apart is rare even on a busy runner, so a hundred tries all switching would be a count that moves
  /// without a switch.
  const QUICK_PAIRS: usize = 100;

  /// A-223, by use: do read the calling thread's switch count, block it in a sleep, and read again; then read pairs
  /// with nothing between. Expect the sleep to have switched the thread out, and a quick pair to agree, where the OS
  /// counts (Windows); elsewhere the test skips loudly.
  #[test]
  #[allow(clippy::disallowed_methods)] // the test blocks the thread on purpose: the block is what is counted.
  fn a_blocked_thread_switched_and_a_running_one_did_not() {
    let Some(before) = switches_now() else {
      eprintln!("skipped: no per-thread switch count read on this platform");
      return;
    };
    std::thread::sleep(std::time::Duration::from_millis(1));
    let after = switches_now();
    assert!(after.is_some_and(|after| after > before), "{before} → {after:?}");
    let agreed = (0..QUICK_PAIRS).any(|_| {
      let first = switches_now();
      first.is_some() && first == switches_now()
    });
    assert!(agreed, "every one of {QUICK_PAIRS} quick read pairs switched");
  }
}
