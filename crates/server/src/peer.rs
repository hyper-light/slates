//! Whether a client's process is gone (§4.7 "Failure matrix": "the daemon detects the dead
//! peer (credential socket closes / slot heartbeat lapse)"). Paired `#[cfg]` functions with
//! one signature, asked on a cold path only: a client silent past the liveness budget.
//!
//! - Linux: the control socket the rendezvous kept; the kernel closes its peer end when the
//!   client dies, and a non-blocking peek sees end of stream.
//! - macOS: the process id from the claim slot, probed with signal 0; `ESRCH` is a dead
//!   process and `EPERM` a reused id owned by another user, so both are "gone".
//! - Windows: the process id, opened for synchronization and waited on with a zero timeout.

use slates_ipc::rendezvous::platform::Control;

/// Whether the peer is gone: dead, or its id reused by someone else.
pub fn peer_gone(control: Option<&Control>, pid: u32) -> bool {
  platform_gone(control, pid)
}

#[cfg(target_os = "linux")]
fn platform_gone(control: Option<&Control>, pid: u32) -> bool {
  use rustix::net::RecvFlags;
  let Some(control) = control else {
    return pid_gone(pid);
  };
  let mut probe = [0u8; 1];
  match rustix::net::recv(
    &control.socket,
    &mut probe,
    RecvFlags::PEEK | RecvFlags::DONTWAIT,
  ) {
    Ok((0, _)) => true,
    Ok(_) | Err(rustix::io::Errno::AGAIN) => false,
    Err(_) => true,
  }
}

#[cfg(target_os = "macos")]
fn platform_gone(_control: Option<&Control>, pid: u32) -> bool {
  pid_gone(pid)
}

#[cfg(unix)]
fn pid_gone(pid: u32) -> bool {
  let Some(pid) = i32::try_from(pid)
    .ok()
    .and_then(rustix::process::Pid::from_raw)
  else {
    return true;
  };
  rustix::process::test_kill_process(pid).is_err()
}

#[cfg(windows)]
fn platform_gone(_control: Option<&Control>, pid: u32) -> bool {
  use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
  use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
  use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
  // SAFETY: a query by id; a null handle means no such process (or no right to it), and the
  // handle is closed below.
  let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
  if handle.is_null() {
    return true;
  }
  // SAFETY: a live handle this function opened; a zero timeout never blocks.
  let waited = unsafe { WaitForSingleObject(handle, 0) };
  // SAFETY: the handle this function opened, closed once.
  unsafe { CloseHandle(handle) };
  waited != WAIT_TIMEOUT
}

#[cfg(not(any(unix, windows)))]
fn platform_gone(_control: Option<&Control>, _pid: u32) -> bool {
  true
}
