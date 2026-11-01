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

/// The write end of the self-pipe [`relay`] made, `-1` until it is made. Read only by the handler.
#[cfg(target_os = "linux")]
static RELAY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// The signals [`relay`] carries: the sync request `slates pmem` answers (`SIGUSR1`), and the stop requests it passes
/// to its VMM.
#[cfg(target_os = "linux")]
pub(crate) const RELAYED: [libc::c_int; 3] = [libc::SIGUSR1, libc::SIGINT, libc::SIGTERM];

/// Makes a self-pipe and installs one handler for each of [`RELAYED`] that writes the signal's number into it, one
/// byte each: what a process that waits on descriptors (`poll`) reads its signals from, never polling on a timer. The
/// read end comes back, non-blocking and close-on-exec, as is the write end the handler keeps. A signal that arrives
/// while the pipe is full is lost, which the reader tolerates by acting on each kind once per read. Once per process.
#[cfg(target_os = "linux")]
pub(crate) fn relay() -> Result<std::os::fd::OwnedFd, String> {
  use std::os::fd::IntoRawFd;
  extern "C" fn on_signal(signal: libc::c_int) {
    let byte = u8::try_from(signal).unwrap_or(u8::MAX);
    let to = RELAY.load(Ordering::Acquire);
    // SAFETY: `write(2)` is async-signal-safe; `to` is the pipe's write end, which [`relay`] stored before installing
    // this handler and never closes, and the one byte read lives on this frame. A full pipe answers `EAGAIN` (the end
    // is non-blocking), and the byte is dropped.
    let _written = unsafe { libc::write(to, std::ptr::addr_of!(byte).cast(), 1) };
  }
  let (read, write) =
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK)
      .map_err(|e| format!("the signal pipe: {e}"))?;
  // The write end lives as long as the process: the handler may run at any moment until it ends.
  RELAY.store(write.into_raw_fd(), Ordering::Release);
  let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
  for signal in RELAYED {
    // SAFETY: the handler is async-signal-safe (an atomic load and one `write`), and `signal` installs it for this
    // process only.
    let previous = unsafe { libc::signal(signal, handler) };
    if previous == libc::SIG_ERR {
      return Err(format!("signal({signal}) refused"));
    }
  }
  Ok(read)
}
