//! The self-pipe `slates pmem` reads its signals from (Linux): one handler for each of the relayed signals writes the
//! signal's number into a pipe the launcher polls beside its VMM's pidfd, so it never polls on a timer.
//!
//! The stop flag the anchor and the daemon poll moved to the library with those roles on 2026-11-18
//! (`slates_api::role`, A-278); this module keeps what only this command uses.

use std::sync::atomic::Ordering;

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
