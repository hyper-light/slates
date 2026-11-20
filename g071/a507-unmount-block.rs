/// Unmounts the slates mount at `path`, the lifecycle counterpart of a mount (sylk's cgofuse `Close`), with no daemon
/// and no privilege: a user unmounts a mount they made, so a stale mount is removable after its daemon has gone. macOS
/// and the BSDs call `unmount(2)` itself (A-507: until 2026-11-18 this ran `umount` found on the `PATH`, so a host
/// whose `PATH` lacked `/sbin` could not unmount at all, g264's kiln runner). Linux runs `fusermount3 -u`, the setuid
/// helper its FUSE mount was made through, since `umount2(2)` needs `CAP_SYS_ADMIN` (the daemon's serve task then sees
/// the kernel's disconnect and ends the attachment, AUD-29-64). Either answer is judged by the kernel's mount table:
/// nothing mounted at `path` is `NotMounted` before any call, and an unmount is done only once the table no longer
/// lists the mount. A WinFsp drive is the daemon's own and is unmounted by detaching it ([`Instance::unmount`]), so this
/// is the Unix hosts' alone.
#[cfg(not(windows))]
pub fn unmount(path: &str) -> Result<(), MountFault> {
  unmount_as(path, Unmounting::Plain)
}

/// How [`unmount_busy`] let go of a mount processes still used: the OS's way, by which a host knows what its leftover
/// processes meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LetGo {
  /// Taken out of the namespace now; its users read and write on until they close, and the unmount finishes then
  /// (Linux: `fusermount3 -u -z`, the kernel's `MNT_DETACH`).
  Detached,
  /// Unmounted now; its users' next calls on it fail (macOS and the BSDs: `unmount(2)` with `MNT_FORCE`). The loopback
  /// NFS mount is `hard`, so this is the only way off it for a holder whose daemon has gone.
  Forced,
}

/// Lets go of the mount at `path` even while processes still use it: what a host does for an ended agent whose
/// leftover process still holds a file there, where [`unmount`] is refused `Busy`. Linux detaches it (`fusermount3 -u
/// -z`); macOS and the BSDs have no detaching unmount and force it (`unmount(2)` with `MNT_FORCE`); the answer says
/// which ([`LetGo`]). Like [`unmount`] it needs no daemon and no privilege, and is judged by the kernel's mount table.
/// Until 2026-11-18 this was Linux's `unmount_lazy`, refused `Unsupported` elsewhere, so a macOS host had no library
/// route off a busy mount (found by g076a, whose conformance harness forces it).
#[cfg(not(windows))]
pub fn unmount_busy(path: &str) -> Result<LetGo, MountFault> {
  unmount_as(path, Unmounting::LetGo).map(|()| BUSY_UNMOUNT)
}

/// Which unmount is asked.
#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unmounting {
  /// Refused while a process holds a file on the mount.
  Plain,
  /// Lets go of the mount even then ([`LetGo`]).
  LetGo,
}

/// An unmount as the OS is asked it: the mount point as the call takes it, and whether it lets go of a busy mount.
#[cfg(not(windows))]
#[derive(Debug, PartialEq, Eq)]
struct UnmountRequest {
  /// The mount point, NUL-terminated.
  point: std::ffi::CString,
  /// Whether it lets go of a busy mount: `MNT_FORCE` on macOS and the BSDs; `-z`, the kernel's `MNT_DETACH`, through
  /// `fusermount3` on Linux.
  let_go: bool,
}

/// The request for `path`, or `MountPoint` when no OS call can name it (a path holding a NUL byte).
#[cfg(not(windows))]
fn unmount_request(path: &str, unmounting: Unmounting) -> Result<UnmountRequest, MountFault> {
  let point = std::ffi::CString::new(path).map_err(|_| MountFault::MountPoint {
    path: path.to_owned(),
    errno: None,
  })?;
  Ok(UnmountRequest {
    point,
    let_go: unmounting == Unmounting::LetGo,
  })
}

/// `unmount(2)`'s flags for a request: `MNT_FORCE` ("forcibly unmounted even if files are still active") to let go of
/// a busy mount, none otherwise.
#[cfg(any(all(unix, not(target_os = "linux")), test))]
fn unmount_flags(request: &UnmountRequest) -> libc::c_int {
  if request.let_go { libc::MNT_FORCE } else { 0 }
}

/// Unmounts `path` as `unmounting` asks, judged by the kernel's mount table (A-507): the mount the table lists at
/// `path` before the call must be gone from it after. With nothing mounted there the answer is `NotMounted` and no call
/// is made; where the platform has no table to ask (the BSDs other than macOS), the call's own answer stands.
#[cfg(not(windows))]
fn unmount_as(path: &str, unmounting: Unmounting) -> Result<(), MountFault> {
  let request = unmount_request(path, unmounting)?;
  let point = recorded_point(path)?;
  let before = mount_table_for(path)?;
  let mounted = match &before {
    Some(table) => match mount_at(table, &point) {
      Some(entry) => Some(entry.identity),
      None => {
        return Err(MountFault::NotMounted {
          path: path.to_owned(),
          unreported: Vec::new(),
        });
      }
    },
    None => None,
  };
  let answer = ask_unmount(path, &request);
  let after = mount_table_for(path)?;
  judged(path, mounted, after.as_deref(), answer)
}

/// The mount point as the kernel's table records it, worked out without touching the mount (a lookup on a `hard` NFS
/// mount whose daemon has gone waits forever): the real path of its parent directory with its own name, so macOS's
/// `/var` reads as the table's `/private/var`. `MountPoint` when `path` names no entry or its parent cannot be resolved.
#[cfg(not(windows))]
fn recorded_point(path: &str) -> Result<String, MountFault> {
  let refused = |errno: Option<i32>| MountFault::MountPoint {
    path: path.to_owned(),
    errno,
  };
  let given = std::path::Path::new(path);
  let name = given.file_name().ok_or_else(|| refused(None))?;
  let parent = given
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .unwrap_or_else(|| std::path::Path::new("."));
  let parent = std::fs::canonicalize(parent).map_err(|error| refused(error.raw_os_error()))?;
  Ok(parent.join(name).to_string_lossy().into_owned())
}

/// The kernel's mount table (`slates_bridge_oci::mount_table`: `getfsstat(MNT_NOWAIT)` on macOS, `/proc/self/mountinfo`
/// on Linux, neither touching a mount), `None` where the platform has no query, or `MountTable` when the kernel refused
/// it.
#[cfg(not(windows))]
fn mount_table_for(path: &str) -> Result<Option<Vec<MountEntry>>, MountFault> {
  match slates_bridge_oci::mount_table::mount_table() {
    Ok(table) => Ok(Some(table)),
    Err(MountTableError::Unsupported { .. }) => Ok(None),
    Err(MountTableError::Query { errno }) => Err(MountFault::MountTable {
      path: path.to_owned(),
      errno: Some(errno),
    }),
    Err(_) => Err(MountFault::MountTable {
      path: path.to_owned(),
      errno: None,
    }),
  }
}

/// The mount the table lists at `point` that an unmount takes: the last listed there, since a later mount at one path
/// shadows an earlier one.
#[cfg(not(windows))]
fn mount_at<'table>(table: &'table [MountEntry], point: &str) -> Option<&'table MountEntry> {
  table.iter().rev().find(|entry| entry.mount_point == point)
}

/// An unmount's answer judged by the table read after it (A-507; kiln judges its own so): the mount gone from the
/// table is done, whatever the call said; still listed, it is the call's refusal, or `StillMounted` when the call said
/// done. With no table on the platform (`mounted` or `after` is `None`) the call's own answer stands.
#[cfg(not(windows))]
fn judged(
  path: &str,
  mounted: Option<MountIdentity>,
  after: Option<&[MountEntry]>,
  answer: Result<(), MountFault>,
) -> Result<(), MountFault> {
  match (mounted, after) {
    (Some(identity), Some(table)) => {
      if table.iter().any(|entry| entry.identity == identity) {
        answer.and(Err(MountFault::StillMounted {
          path: path.to_owned(),
        }))
      } else {
        Ok(())
      }
    }
    _ => answer,
  }
}

/// The kernel's refusal of an unmount typed by its error number (`unmount(2)`): `EBUSY` ("a process is holding a
/// reference to a file located on the filesystem") is `Busy`, `EINVAL` ("the requested directory is not in the mount
/// table") is `NotMounted`, `ENOENT` and `ENOTDIR` (no such directory) are `MountPoint`, and every other is `Unmount`
/// with its number.
#[cfg(any(all(unix, not(target_os = "linux")), test))]
fn refusal_of_errno(path: &str, errno: Option<i32>) -> MountFault {
  let path = path.to_owned();
  match errno {
    Some(libc::EBUSY) => MountFault::Busy { path },
    Some(libc::EINVAL) => MountFault::NotMounted {
      path,
      unreported: Vec::new(),
    },
    Some(libc::ENOENT | libc::ENOTDIR) => MountFault::MountPoint { path, errno },
    errno => MountFault::Unmount {
      path,
      status: None,
      errno,
    },
  }
}

/// Asks the kernel to unmount (macOS and the BSDs, `unmount(2)`): no program and no `PATH` (A-507), and no privilege,
/// since only "the caller is not the super-user, and the mount() was not done by the user" is refused `EPERM`.
#[cfg(all(unix, not(target_os = "linux")))]
fn ask_unmount(path: &str, request: &UnmountRequest) -> Result<(), MountFault> {
  // SAFETY: `request.point` is a NUL-terminated string that outlives the call, the only pointer `unmount(2)` reads,
  // and the flags are `unmount(2)`'s own.
  let result = unsafe { libc::unmount(request.point.as_ptr(), unmount_flags(request)) };
  if result == 0 {
    Ok(())
  } else {
    Err(refusal_of_errno(
      path,
      std::io::Error::last_os_error().raw_os_error(),
    ))
  }
}

/// Asks `fusermount3 -u` (with `-z`, the kernel's `MNT_DETACH`, to let go of a busy mount), the setuid helper the FUSE
/// mount was made through: `umount2(2)` "requires appropriate privilege (Linux: the `CAP_SYS_ADMIN` capability)", so an
/// unprivileged caller's own call is refused `EPERM`. Its answer is its exit, or the error number of starting it.
#[cfg(target_os = "linux")]
fn ask_unmount(path: &str, request: &UnmountRequest) -> Result<(), MountFault> {
  use std::os::unix::ffi::OsStrExt;
  let args = if request.let_go {
    BUSY_UNMOUNT_ARGS
  } else {
    UNMOUNT_ARGS
  };
  let status = Command::new(UNMOUNT_PROGRAM)
    .args(args)
    .arg(std::ffi::OsStr::from_bytes(request.point.as_bytes()))
    .status()
    .map_err(|error| MountFault::Unmount {
      path: path.to_owned(),
      status: None,
      errno: error.raw_os_error(),
    })?;
  if status.success() {
    Ok(())
  } else {
    Err(MountFault::Unmount {
      path: path.to_owned(),
      status: status.code(),
      errno: None,
    })
  }
}

/// Format: the helper that removes a slates mount on Linux without a privilege (R10): every slates mount there is FUSE,
/// made and removed by the OS's `fusermount3`.
#[cfg(target_os = "linux")]
const UNMOUNT_PROGRAM: &str = "fusermount3";
/// Format: `fusermount3`'s unmount (`-u`).
#[cfg(target_os = "linux")]
const UNMOUNT_ARGS: &[&str] = &["-u"];
/// Format: `fusermount3`'s lazy unmount (`-z`: the kernel's `MNT_DETACH`), for [`unmount_busy`].
#[cfg(target_os = "linux")]
const BUSY_UNMOUNT_ARGS: &[&str] = &["-u", "-z"];
/// Format: how [`unmount_busy`] lets go on Linux.
#[cfg(target_os = "linux")]
const BUSY_UNMOUNT: LetGo = LetGo::Detached;
/// Format: how [`unmount_busy`] lets go on macOS and the BSDs (`unmount(2)` with `MNT_FORCE`).
#[cfg(all(unix, not(target_os = "linux")))]
const BUSY_UNMOUNT: LetGo = LetGo::Forced;

