//! The stop request: `SIGINT`/`SIGTERM` on Unix, the console control event on Windows, each
//! setting one flag the main loops poll. Paired `#[cfg]` functions with one signature; the
//! handler does nothing but store to an atomic (the only thing a handler may do).

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the handler; polled by the anchor's and the daemon's loops.
static STOP: AtomicBool = AtomicBool::new(false);

/// Whether a stop was requested.
pub(crate) fn stop_requested() -> bool {
  STOP.load(Ordering::Acquire)
}

#[cfg(unix)]
pub(crate) fn install() -> Result<(), String> {
  extern "C" fn on_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::Release);
  }
  let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
  for signal in [libc::SIGINT, libc::SIGTERM] {
    // SAFETY: the handler is async-signal-safe (one atomic store), and `signal` installs it
    // for this process only.
    let previous = unsafe { libc::signal(signal, handler) };
    if previous == libc::SIG_ERR {
      return Err(format!("signal({signal}) refused"));
    }
  }
  Ok(())
}

#[cfg(windows)]
pub(crate) fn install() -> Result<(), String> {
  use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
  use windows_sys::core::BOOL;

  unsafe extern "system" fn on_event(_event: u32) -> BOOL {
    STOP.store(true, Ordering::Release);
    1
  }
  // SAFETY: the handler is a plain function that stores to an atomic and reports handled.
  let ok = unsafe { SetConsoleCtrlHandler(Some(on_event), 1) };
  if ok == 0 {
    return Err("SetConsoleCtrlHandler refused".to_owned());
  }
  Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn install() -> Result<(), String> {
  Ok(())
}
