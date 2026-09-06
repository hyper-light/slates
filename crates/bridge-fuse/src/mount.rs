//! Mount establishment (§4.6 "Linux (own /dev/fuse driver)", Phase 3 task 2). A FUSE mount
//! needs the kernel told about the `/dev/fuse` connection at a mount point. slates never
//! requires a privilege the user may lack (R10, D-2), so the default path is `fusermount3`, the
//! OS-shipped setuid helper: the daemon spawns it with one end of a socket pair in its
//! environment, `fusermount3` opens `/dev/fuse`, performs the mount, and hands the device
//! descriptor back over the socket with `SCM_RIGHTS`. Where the daemon has `CAP_SYS_ADMIN` in
//! its user namespace the new mount API (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) avoids the
//! helper; that path is owed. On unmount the same helper (`fusermount3 -u`) tears it down.
//!
//! Linux only; the handshake runs against a real `fusermount3` in the CI Linux lane. This file
//! compiles and cross-lints everywhere. No `unsafe`: the socket pair, the descriptor pass and
//! the spawn use rustix's I/O-safe wrappers and `std::process`.

#![cfg(target_os = "linux")]

use std::io::IoSliceMut;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::process::Command;

use rustix::net::{
  AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketFlags, SocketType,
};

use crate::channel::{ChannelError, FuseChannel};

/// Format: the helper the OS ships to mount FUSE without a privilege.
const FUSERMOUNT: &str = "fusermount3";
/// Format: the environment variable `fusermount3` reads for the socket to send the device fd
/// back over.
const COMM_FD_ENV: &str = "_FUSE_COMMFD";
/// Format: the fixed mount options slates always sets: the kernel checks permissions itself
/// (`default_permissions`), and the fs type and name identify the mount.
const BASE_OPTIONS: &str = "default_permissions,fsname=slates,subtype=slates";

/// A refusal from the mount.
#[derive(Debug)]
pub enum MountError {
  /// The socket pair could not be made.
  Socketpair {
    /// The code.
    code: Option<i32>,
  },
  /// `fusermount3` could not be spawned (it is not installed, or not on the path).
  Spawn {
    /// The code.
    code: Option<i32>,
  },
  /// `fusermount3` exited without mounting (the message it printed is the operator's cue).
  Helper {
    /// The exit code.
    exit: Option<i32>,
  },
  /// The helper did not hand back a device descriptor.
  NoDevice,
  /// Receiving the descriptor refused.
  Recv {
    /// The code.
    code: Option<i32>,
  },
  /// The device the helper returned could not be used.
  Channel(ChannelError),
}

impl std::fmt::Display for MountError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Socketpair { code } => write!(f, "socketpair refused (code {code:?})"),
      Self::Spawn { code } => write!(
        f,
        "cannot spawn {FUSERMOUNT} (code {code:?}); is it installed?"
      ),
      Self::Helper { exit } => write!(f, "{FUSERMOUNT} exited without mounting (code {exit:?})"),
      Self::NoDevice => write!(f, "{FUSERMOUNT} returned no device descriptor"),
      Self::Recv { code } => write!(f, "receiving the device descriptor refused (code {code:?})"),
      Self::Channel(e) => write!(f, "the mounted device: {e}"),
    }
  }
}

impl std::error::Error for MountError {}

/// A mount: the channel over the mounted connection and the mount point, so `unmount` can tear
/// it down with the same helper.
pub struct Mount {
  channel: FuseChannel,
  mount_point: String,
}

impl std::fmt::Debug for Mount {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Mount")
      .field("mount_point", &self.mount_point)
      .finish()
  }
}

impl Mount {
  /// The channel over the connection.
  pub fn channel(&mut self) -> &mut FuseChannel {
    &mut self.channel
  }

  /// The mount point.
  pub fn mount_point(&self) -> &str {
    &self.mount_point
  }

  /// Unmounts, through `fusermount3 -u` (the same no-privilege path).
  pub fn unmount(self) -> Result<(), MountError> {
    let status = Command::new(FUSERMOUNT)
      .arg("-u")
      .arg("-z")
      .arg(&self.mount_point)
      .status()
      .map_err(|e| MountError::Spawn {
        code: e.raw_os_error(),
      })?;
    if status.success() {
      Ok(())
    } else {
      Err(MountError::Helper {
        exit: status.code(),
      })
    }
  }
}

/// Mounts a slates connection at `mount_point` through `fusermount3`, returning the mount whose
/// channel serves it. `extra_options` are appended to the base options (e.g. `allow_other`
/// only when the operator set `user_allow_other`, §4.6).
pub fn mount(mount_point: &str, extra_options: &[&str]) -> Result<Mount, MountError> {
  let (ours, theirs) = rustix::net::socketpair(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::CLOEXEC,
    None,
  )
  .map_err(|e| MountError::Socketpair {
    code: Some(e.raw_os_error()),
  })?;
  let mut options = BASE_OPTIONS.to_owned();
  for extra in extra_options {
    options.push(',');
    options.push_str(extra);
  }
  // The helper inherits `theirs` (without CLOEXEC) and reads its number from the environment.
  let comm = clear_cloexec(&theirs).map_err(|e| MountError::Recv {
    code: Some(e.raw_os_error()),
  })?;
  let child = Command::new(FUSERMOUNT)
    .arg("-o")
    .arg(&options)
    .arg(mount_point)
    .env(COMM_FD_ENV, comm.to_string())
    .spawn()
    .map_err(|e| MountError::Spawn {
      code: e.raw_os_error(),
    })?;
  // The daemon holds only its own end now; the child holds `theirs`.
  drop(theirs);
  // Own the helper so it is reaped on *every* exit, not only the success path (audit BUG-14):
  // if `receive_device` fails below, an early `?` return drops this guard, which kills and reaps
  // the helper rather than leaving it a zombie/orphan. The success path disarms it via `finish`.
  let mut helper = HelperGuard(Some(child));
  let device = receive_device(&ours)?;
  let status = helper.finish()?;
  if !status.success() {
    return Err(MountError::Helper {
      exit: status.code(),
    });
  }
  Ok(Mount {
    channel: FuseChannel::from_device(device),
    mount_point: mount_point.to_owned(),
  })
}

/// Owns the spawned `fusermount3` helper so it is always reaped (audit BUG-14, and Part 2 item 9:
/// no spawned child without an owner that joins or cancels it). A successful mount calls
/// [`HelperGuard::finish`] to wait for the helper's own exit and disarm the guard; any earlier
/// failure drops the guard instead, and its [`Drop`] kills and waits the child so no zombie lingers.
struct HelperGuard(Option<std::process::Child>);

impl HelperGuard {
  /// Waits for the helper's normal exit, taking the child so the guard's drop is then a no-op.
  fn finish(&mut self) -> Result<std::process::ExitStatus, MountError> {
    match self.0.take() {
      Some(mut child) => child.wait().map_err(|e| MountError::Spawn {
        code: e.raw_os_error(),
      }),
      // `finish` runs once, after a successful spawn, so the child is present; a missing one is a
      // caller error, reported rather than panicked (the no-panic law).
      None => Err(MountError::Spawn { code: None }),
    }
  }
}

impl Drop for HelperGuard {
  fn drop(&mut self) {
    if let Some(mut child) = self.0.take() {
      // The mount did not complete: stop the helper and reap it so no orphan or zombie is left.
      // Both may fail if it has already exited; the wait still reaps the zombie, and either error
      // is nothing the caller can act on at drop.
      let _ = child.kill();
      let _ = child.wait();
    }
  }
}

/// Clears close-on-exec on a descriptor so the spawned helper inherits it, returning its raw
/// number for the environment.
fn clear_cloexec(fd: &OwnedFd) -> Result<i32, rustix::io::Errno> {
  rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())?;
  Ok(fd.as_fd().as_raw_fd())
}

/// Receives the `/dev/fuse` descriptor `fusermount3` sends over the socket with `SCM_RIGHTS`.
fn receive_device(socket: &OwnedFd) -> Result<OwnedFd, MountError> {
  let mut byte = [0u8; 1];
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
  let mut control = RecvAncillaryBuffer::new(&mut space);
  rustix::net::recvmsg(
    socket,
    &mut [IoSliceMut::new(&mut byte)],
    &mut control,
    RecvFlags::CMSG_CLOEXEC,
  )
  .map_err(|e| MountError::Recv {
    code: Some(e.raw_os_error()),
  })?;
  for message in control.drain() {
    if let RecvAncillaryMessage::ScmRights(mut fds) = message
      && let Some(device) = fds.next()
    {
      return Ok(device);
    }
  }
  Err(MountError::NoDevice)
}
