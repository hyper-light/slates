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
//!
//!   The three calls are found in kernel32 at run time, never imported (A-357). Nano Server's kernel32 exports none of
//!   them, and Windows refuses to load a binary that imports a function its DLL lacks: once A-223 imported them, the
//!   x86_64 binary exited 0xC0000139 (`STATUS_ENTRYPOINT_NOT_FOUND`) in `nanoserver:ltsc2025` before any of its code
//!   ran, where the binary without them had answered `slates version` that morning (release runs 44326428333 and
//!   44266895044, 2026-11-18). Where kernel32 lacks them the count is `None`, as when the OS refuses the profiling.
//! - **Elsewhere:** `None`. Linux's waits are judged by `getrusage(RUSAGE_THREAD)`'s voluntary switches in `slates-rt`
//!   and the probe's by `/proc` state; macOS's probe by `thread_info`.

/// The calling thread's context switches since it first asked, where the OS counts them for the thread itself
/// (Windows); `None` elsewhere, where kernel32 lacks the profiling calls, or when the OS refuses the profiling.
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
  use windows_sys::Win32::Foundation::{FARPROC, HANDLE};
  use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
  use windows_sys::Win32::System::Performance::HardwareCounterProfiling::PERFORMANCE_DATA;
  use windows_sys::Win32::System::Threading::GetCurrentThread;

  /// `EnableThreadProfiling` [B: Microsoft Learn]: the thread, the flags, the hardware counters, and where the
  /// profiling handle is written; it answers a Win32 error code.
  type Enable = unsafe extern "system" fn(HANDLE, u32, u64, *mut HANDLE) -> u32;
  /// `ReadThreadProfilingData` [B: Microsoft Learn]: the profiling handle, the flags and the record it fills; it
  /// answers a Win32 error code.
  type Read = unsafe extern "system" fn(HANDLE, u32, *mut PERFORMANCE_DATA) -> u32;
  /// `DisableThreadProfiling` [B: Microsoft Learn]: the profiling handle; it answers a Win32 error code.
  type Disable = unsafe extern "system" fn(HANDLE) -> u32;

  /// Format: `THREAD_PROFILING_FLAG_DISPATCH` (winnt.h): collect the thread's dispatch data, its switch count.
  const THREAD_PROFILING_FLAG_DISPATCH: u32 = 1;
  /// Format: `READ_THREAD_PROFILING_FLAG_DISPATCHING` (winnt.h): read the dispatch data.
  const READ_THREAD_PROFILING_FLAG_DISPATCHING: u32 = 1;
  /// Format: `PERFORMANCE_DATA_VERSION` (winnt.h), the record's version the caller states.
  const PERFORMANCE_DATA_VERSION: u8 = 1;
  /// Format: `ERROR_SUCCESS`, what both calls answer when they succeed (they return a Win32 error code).
  const ERROR_SUCCESS: u32 = 0;

  /// The three profiling calls as kernel32 exports them, found at run time (see the module doc).
  struct Calls {
    /// `EnableThreadProfiling`.
    enable: Enable,
    /// `ReadThreadProfilingData`.
    read: Read,
    /// `DisableThreadProfiling`.
    disable: Disable,
  }

  impl Calls {
    /// The three calls in kernel32, or `None` where it lacks any of them (Nano Server's has none of the three).
    fn find() -> Option<Calls> {
      let module_name: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
      // SAFETY: `module_name` is NUL-terminated and outlives the call; kernel32 is mapped into every Windows process
      // for the process's life, so the module handle stays valid, and each name passed to `GetProcAddress` is
      // NUL-terminated. Each pointer found is kernel32's function of that name, whose signature its type states
      // [B: Microsoft Learn, "EnableThreadProfiling", "ReadThreadProfilingData", "DisableThreadProfiling"]; a missing
      // one is a null pointer, which becomes `None`.
      unsafe {
        let module = GetModuleHandleW(module_name.as_ptr());
        if module.is_null() {
          return None;
        }
        let enable = GetProcAddress(module, c"EnableThreadProfiling".as_ptr().cast());
        let read = GetProcAddress(module, c"ReadThreadProfilingData".as_ptr().cast());
        let disable = GetProcAddress(module, c"DisableThreadProfiling".as_ptr().cast());
        Some(Calls {
          enable: std::mem::transmute::<FARPROC, Option<Enable>>(enable)?,
          read: std::mem::transmute::<FARPROC, Option<Read>>(read)?,
          disable: std::mem::transmute::<FARPROC, Option<Disable>>(disable)?,
        })
      }
    }
  }

  /// The calling thread's profiling handle, disabled when the thread ends, with the calls that read and disable it.
  pub(super) struct Profiling {
    /// The handle `EnableThreadProfiling` wrote.
    handle: HANDLE,
    /// `ReadThreadProfilingData`.
    read: Read,
    /// `DisableThreadProfiling`.
    disable: Disable,
  }

  impl Profiling {
    /// Enables dispatch profiling on the calling thread; `None` where kernel32 lacks the calls or the OS refuses.
    fn enable() -> Option<Profiling> {
      let calls = Calls::find()?;
      let mut handle: HANDLE = std::ptr::null_mut();
      // SAFETY: `calls.enable` is kernel32's `EnableThreadProfiling`; it is passed the calling thread's pseudo-handle,
      // as it requires (it profiles only the calling thread); no hardware counters asked; the handle is this frame's
      // own, written by the call, and read only when the call answers success.
      let answer = unsafe {
        (calls.enable)(
          GetCurrentThread(),
          THREAD_PROFILING_FLAG_DISPATCH,
          0,
          &mut handle,
        )
      };
      (answer == ERROR_SUCCESS && !handle.is_null()).then_some(Profiling {
        handle,
        read: calls.read,
        disable: calls.disable,
      })
    }

    /// The thread's context switches since profiling was enabled.
    pub(super) fn switches(&self) -> Option<u64> {
      let mut data = PERFORMANCE_DATA {
        Size: u16::try_from(std::mem::size_of::<PERFORMANCE_DATA>()).ok()?,
        Version: PERFORMANCE_DATA_VERSION,
        ..PERFORMANCE_DATA::default()
      };
      // SAFETY: `self.read` is kernel32's `ReadThreadProfilingData`; this value's own live handle, read on the thread
      // that enabled it (a thread-local); `data` is this frame's own record, its size and version stated as the call
      // requires; read only when the call succeeds.
      let answer = unsafe {
        (self.read)(
          self.handle,
          READ_THREAD_PROFILING_FLAG_DISPATCHING,
          &mut data,
        )
      };
      (answer == ERROR_SUCCESS).then_some(u64::from(data.ContextSwitchCount))
    }
  }

  impl Drop for Profiling {
    fn drop(&mut self) {
      // SAFETY: `self.disable` is kernel32's `DisableThreadProfiling`; the handle is this value's own, made by
      // `EnableThreadProfiling` on this thread and disabled only here, as the thread's locals are destroyed.
      unsafe { (self.disable)(self.handle) };
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
    assert!(
      after.is_some_and(|after| after > before),
      "{before} → {after:?}"
    );
    let agreed = (0..QUICK_PAIRS).any(|_| {
      let first = switches_now();
      first.is_some() && first == switches_now()
    });
    assert!(
      agreed,
      "every one of {QUICK_PAIRS} quick read pairs switched"
    );
  }
}
