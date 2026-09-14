//! Mount establishment (§4.6 "Linux (own /dev/fuse driver)", Phase 3 task 2). A FUSE mount
//! needs the kernel told about the `/dev/fuse` connection at a mount point. slates never
//! requires a privilege the user may lack (R10, D-2), so the default path is `fusermount3`, the
//! OS-shipped setuid helper: the daemon spawns it with one end of a socket pair in its
//! environment, `fusermount3` opens `/dev/fuse`, performs the mount, and hands the device
//! descriptor back over the socket with `SCM_RIGHTS`. Where the daemon has `CAP_SYS_ADMIN` in
//! its user namespace the new mount API (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) avoids the
//! helper; that path is owed. On unmount the same helper (`fusermount3 -u`) tears it down.
//!
//! The descriptor handshake itself — spawn a helper with the socket in its environment, wait for
//! the descriptor under a deadline, and own the helper until it is reaped or cancelled — is
//! plain Unix and lives here as [`handshake`], so its error exits are proven on every Unix host
//! against real child processes (`tests/handshake.rs`): a helper that exits without a descriptor
//! is reaped and its exit reported, a helper that never answers is killed at the deadline and
//! reaped, and a helper that sends a descriptor hands it over (AC-3.12/T-3.15; audit BUG-14,
//! "early failure receiving the mount helper's descriptor returns before waiting for its
//! child"). Only [`mount`] and [`Mount::unmount`], which name `fusermount3` and the FUSE channel,
//! are Linux; the handshake runs against the real helper in the CI Linux lane. No `unsafe`: the
//! socket pair, the descriptor pass, the deadline and the spawn use rustix's I/O-safe wrappers
//! and `std::process`.

#![cfg(unix)]

use std::io::IoSliceMut;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use rustix::net::sockopt::{Timeout, set_socket_timeout};
use rustix::net::{
  AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketFlags, SocketType,
};

#[cfg(target_os = "linux")]
use crate::channel::{ChannelError, FuseChannel};

/// Format: the helper the OS ships to mount FUSE without a privilege.
#[cfg(target_os = "linux")]
const FUSERMOUNT: &str = "fusermount3";
/// Format: the environment variable `fusermount3` reads for the socket to send the device fd
/// back over (the `_FUSE_COMMFD` protocol of libfuse's `fuse_mount_fusermount`).
pub const COMM_FD_ENV: &str = "_FUSE_COMMFD";
/// Format: the descriptor number the socket has in the helper: its standard input (0), where
/// [`handshake`] places it.
pub const COMM_FD_IN_CHILD: i32 = 0;
/// Format: the fixed mount options slates always sets: the kernel checks permissions itself
/// (`default_permissions`), and the fs type and name identify the mount.
#[cfg(target_os = "linux")]
const BASE_OPTIONS: &str = "default_permissions,fsname=slates,subtype=slates";

/// How the helper process ended, as the handshake reaped it: its exit code, or the signal that
/// killed it. Only a reaped child yields either, so an error carrying one is proof the helper
/// was not left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperExit {
  /// The exit code, when the helper exited.
  pub code: Option<i32>,
  /// The signal, when the helper was killed (the handshake's own cancellation sends `SIGKILL`).
  pub signal: Option<i32>,
}

impl HelperExit {
  fn of(status: ExitStatus) -> HelperExit {
    HelperExit {
      code: status.code(),
      signal: status.signal(),
    }
  }
}

/// A refusal from the mount.
#[derive(Debug)]
pub enum MountError {
  /// The socket pair could not be made.
  Socketpair {
    /// The code.
    code: Option<i32>,
  },
  /// The helper could not be spawned (it is not installed, or not on the path).
  Spawn {
    /// The code.
    code: Option<i32>,
  },
  /// The helper exited without mounting, after handing a descriptor back (the message it printed
  /// is the operator's cue).
  Helper {
    /// How it ended.
    exit: HelperExit,
  },
  /// The helper ended without handing back a device descriptor; it was reaped.
  NoDevice {
    /// How it ended.
    exit: HelperExit,
  },
  /// The helper had not answered within the deadline; it was killed and reaped.
  Timeout {
    /// The deadline the caller derived.
    deadline: Duration,
    /// How the helper ended (killed, unless it exited in the moment before).
    exit: HelperExit,
  },
  /// Receiving the descriptor refused.
  Recv {
    /// The code.
    code: Option<i32>,
  },
  /// The device the helper returned could not be used.
  #[cfg(target_os = "linux")]
  Channel(ChannelError),
}

impl std::fmt::Display for MountError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Socketpair { code } => write!(f, "socketpair refused (code {code:?})"),
      Self::Spawn { code } => write!(
        f,
        "cannot spawn the mount helper (code {code:?}); is it installed?"
      ),
      Self::Helper { exit } => write!(f, "the mount helper exited without mounting ({exit:?})"),
      Self::NoDevice { exit } => write!(
        f,
        "the mount helper returned no device descriptor ({exit:?})"
      ),
      Self::Timeout { deadline, exit } => write!(
        f,
        "the mount helper did not answer within {deadline:?}; killed ({exit:?})"
      ),
      Self::Recv { code } => write!(f, "receiving the device descriptor refused (code {code:?})"),
      #[cfg(target_os = "linux")]
      Self::Channel(e) => write!(f, "the mounted device: {e}"),
    }
  }
}

impl std::error::Error for MountError {}

/// A mount: the channel over the mounted connection and the mount point, so `unmount` can tear
/// it down with the same helper.
#[cfg(target_os = "linux")]
pub struct Mount {
  channel: FuseChannel,
  mount_point: String,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for Mount {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Mount")
      .field("mount_point", &self.mount_point)
      .finish()
  }
}

#[cfg(target_os = "linux")]
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
        exit: HelperExit::of(status),
      })
    }
  }
}

/// Mounts a slates connection at `mount_point` through `fusermount3`, returning the mount whose
/// channel serves it. `extra_options` are appended to the base options (e.g. `allow_other`
/// only when the operator set `user_allow_other`, §4.6). `deadline` bounds the helper's answer
/// (see [`handshake`]).
#[cfg(target_os = "linux")]
pub fn mount(
  mount_point: &str,
  extra_options: &[&str],
  deadline: Duration,
) -> Result<Mount, MountError> {
  let mut options = BASE_OPTIONS.to_owned();
  for extra in extra_options {
    options.push(',');
    options.push_str(extra);
  }
  let mut helper = Command::new(FUSERMOUNT);
  helper.arg("-o").arg(&options).arg(mount_point);
  let device = handshake(helper, deadline)?;
  Ok(Mount {
    channel: FuseChannel::from_device(device),
    mount_point: mount_point.to_owned(),
  })
}

/// Runs the descriptor handshake with `helper`: spawns it with one end of a socket pair as its
/// standard input and `_FUSE_COMMFD=0` in its environment (libfuse's protocol names the socket by
/// number; the number is ours to choose), waits for the descriptor it sends back with
/// `SCM_RIGHTS`, and owns the process until it is reaped — on every exit. A helper that ends
/// without sending a descriptor is reaped and reported with its exit ([`MountError::NoDevice`]);
/// one that has not answered within `deadline` is killed and reaped ([`MountError::Timeout`]);
/// one that sends a descriptor is waited for and, if it then reports failure, reported with its
/// exit ([`MountError::Helper`]). No path returns before the child is reaped or cancelled (audit
/// BUG-14; Part 2 item 9). The caller derives `deadline`: the daemon from its failover SLO, a
/// test from a measured trivial spawn.
///
/// The socket reaches the helper through the child's descriptor table alone (`dup2` in the child,
/// which clears close-on-exec on the copy), never through an inheritable descriptor in this
/// process: both ends are close-on-exec here, so a spawn from another thread in the same moment
/// cannot inherit the helper's end and hold the socket open past the helper's exit — the race
/// libfuse avoids by clearing the flag between `fork` and `exec`. `helper` is consumed so this
/// process's copy of the socket end closes with it the moment the child is running.
pub fn handshake(mut helper: Command, deadline: Duration) -> Result<OwnedFd, MountError> {
  // The pair is made without close-on-exec, which not every Unix offers at creation (macOS has no
  // `SOCK_CLOEXEC`), and both ends get it right after.
  let (ours, theirs) = rustix::net::socketpair(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::empty(),
    None,
  )
  .map_err(|e| MountError::Socketpair {
    code: Some(e.raw_os_error()),
  })?;
  for end in [&ours, &theirs] {
    rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC).map_err(|e| MountError::Recv {
      code: Some(e.raw_os_error()),
    })?;
  }
  // The deadline is enforced by the kernel on our end of the socket: a receive that has not
  // completed within it returns `EAGAIN`, never blocks the shard forever on a helper that hangs.
  set_socket_timeout(&ours, Timeout::Recv, Some(deadline)).map_err(|e| MountError::Recv {
    code: Some(e.raw_os_error()),
  })?;
  let child = helper
    .stdin(Stdio::from(theirs))
    .env(COMM_FD_ENV, COMM_FD_IN_CHILD.to_string())
    .spawn()
    .map_err(|e| MountError::Spawn {
      code: e.raw_os_error(),
    })?;
  // This process now holds only its own end (the command, and the copy it kept for the spawn, go
  // here); the child holds the other, so its exit closes the socket and an unanswered receive
  // ends with no descriptor rather than waiting.
  drop(helper);
  // Own the helper so it is reaped on *every* exit, not only the success path (audit BUG-14): an
  // early return below drops the guard, which kills and reaps the helper rather than leaving it
  // a zombie or an orphan. The success path waits for it through `finish`.
  let mut guard = HelperGuard(Some(child));
  match receive_device(&ours) {
    Ok(device) => {
      let status = guard.finish()?;
      if status.success() {
        Ok(device)
      } else {
        Err(MountError::Helper {
          exit: HelperExit::of(status),
        })
      }
    }
    Err(Received::Nothing) => Err(MountError::NoDevice {
      exit: HelperExit::of(guard.finish()?),
    }),
    Err(Received::TimedOut) => Err(MountError::Timeout {
      deadline,
      exit: guard.abort(),
    }),
    Err(Received::Refused(code)) => Err(MountError::Recv { code: Some(code) }),
  }
}

/// Owns the spawned helper so it is always reaped (audit BUG-14, and Part 2 item 9: no spawned
/// child without an owner that joins or cancels it). [`HelperGuard::finish`] waits for the
/// helper's own exit; [`HelperGuard::abort`] kills and reaps it; a guard dropped any other way
/// (a `?` on a later step) does the same in [`Drop`], so no zombie lingers.
struct HelperGuard(Option<std::process::Child>);

impl HelperGuard {
  /// Waits for the helper's exit, taking the child so the guard's drop is then a no-op.
  fn finish(&mut self) -> Result<ExitStatus, MountError> {
    match self.0.take() {
      Some(mut child) => child.wait().map_err(|e| MountError::Spawn {
        code: e.raw_os_error(),
      }),
      // `finish` runs once, after a successful spawn, so the child is present; a missing one is a
      // caller error, reported rather than panicked (the no-panic law).
      None => Err(MountError::Spawn { code: None }),
    }
  }

  /// Kills the helper and reaps it, reporting how it ended (killed, unless it exited in the
  /// moment before the kill).
  fn abort(&mut self) -> HelperExit {
    match self.0.take() {
      Some(child) => Self::kill_and_reap(child),
      None => HelperExit {
        code: None,
        signal: None,
      },
    }
  }

  fn kill_and_reap(mut child: std::process::Child) -> HelperExit {
    // The kill may fail if the helper has already exited; the wait still reaps it.
    let _ = child.kill();
    match child.wait() {
      Ok(status) => HelperExit::of(status),
      Err(_) => HelperExit {
        code: None,
        signal: None,
      },
    }
  }
}

impl Drop for HelperGuard {
  fn drop(&mut self) {
    if let Some(child) = self.0.take() {
      // The handshake did not complete: stop the helper and reap it so no orphan or zombie is
      // left. Nothing here is actionable at drop; the typed outcome was already reported.
      let _ = Self::kill_and_reap(child);
    }
  }
}

/// Why no descriptor arrived.
enum Received {
  /// The helper's end closed (it exited) with no descriptor sent.
  Nothing,
  /// The deadline passed.
  TimedOut,
  /// The receive itself refused (the errno).
  Refused(i32),
}

/// Receives the device descriptor the helper sends over the socket with `SCM_RIGHTS`. The
/// received descriptor is marked close-on-exec as soon as it is owned (`MSG_CMSG_CLOEXEC` is
/// Linux's alone), so a later spawn never inherits the mount's device.
fn receive_device(socket: &OwnedFd) -> Result<OwnedFd, Received> {
  let mut byte = [0u8; 1];
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
  let mut control = RecvAncillaryBuffer::new(&mut space);
  rustix::net::recvmsg(
    socket,
    &mut [IoSliceMut::new(&mut byte)],
    &mut control,
    RecvFlags::empty(),
  )
  .map_err(|e| match e {
    // `EAGAIN` (the same value as `EWOULDBLOCK` on every Unix): the receive timeout elapsed.
    rustix::io::Errno::AGAIN => Received::TimedOut,
    other => Received::Refused(other.raw_os_error()),
  })?;
  for message in control.drain() {
    if let RecvAncillaryMessage::ScmRights(mut fds) = message
      && let Some(device) = fds.next()
    {
      rustix::io::fcntl_setfd(&device, rustix::io::FdFlags::CLOEXEC)
        .map_err(|e| Received::Refused(e.raw_os_error()))?;
      return Ok(device);
    }
  }
  Err(Received::Nothing)
}
