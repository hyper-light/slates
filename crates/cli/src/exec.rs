//! `slates exec` — the chosen-path launcher (§4.6 "The launcher", §4.12; Phase 3 task 4). It
//! makes a volume visible at a path the caller names, for one command, without changing what any
//! other process sees and without writing to disk: it enters a new user and mount namespace,
//! makes the mount tree private so nothing propagates back, bind-mounts the volume's directory
//! (under the daemon's root mount) onto the chosen path, and execs the command. The parent
//! shell's view of the chosen path is unchanged, and the launcher creates no filesystem entry —
//! an unsatisfiable path is refused with the exact missing directory, never created (AC-3.5).
//!
//! Linux only (user and mount namespaces are Linux's); it runs against a real daemon mount in
//! the CI Linux lane. This file compiles and cross-lints everywhere. No privilege is required:
//! an unprivileged user namespace provides the mount capability (D-2, R10); a host that forbids
//! them (a sysctl, or an AppArmor profile) is refused with the exact setting to change.

#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::fs::{Mode, OFlags};
use rustix::mount::{MountPropagationFlags, mount_bind, mount_change};
use rustix::thread::UnshareFlags;

use crate::Failure;
use crate::args::ExecRequest;

/// Format: the environment variable naming the daemon's root mount (where volumes appear as
/// `<root>/<volume>`). The daemon sets it when it mounts; until the mount publishes it (owed),
/// the caller may set it. No default: without a known root the launcher cannot find the volume.
const ROOT_ENV: &str = "SLATES_ROOT";

fn failed(what: &str, e: impl std::fmt::Display) -> Failure {
  Failure::Failed(format!("{what}: {e}"))
}

/// Runs the launcher. On success it never returns (it execs the command); any error before the
/// exec is a `Failure`, and the exec's own failure (the command not found) is one too.
pub(crate) fn run(request: &ExecRequest) -> Result<(), Failure> {
  let root = std::env::var(ROOT_ENV).map_err(|_| {
    Failure::Failed(format!(
      "{ROOT_ENV} is not set; it names the daemon's root mount (where the volume appears)"
    ))
  })?;
  let source = format!("{}/{}", root.trim_end_matches('/'), request.volume);
  // The source must exist (the daemon mounted the volume); the chosen path must exist and be a
  // directory (the launcher never creates it, AC-3.5).
  ensure_directory(&source, "the volume's mount")?;
  ensure_directory(&request.at, "the chosen path")?;
  let (program, args) = request
    .command
    .split_first()
    .ok_or_else(|| Failure::Failed("exec needs a command".to_owned()))?;

  enter_namespace()?;
  make_root_private()?;
  mount_bind(&source, &request.at).map_err(|e| bind_error(&source, &request.at, e))?;

  // Exec replaces this process; the bind mount lives only in this namespace, so the parent
  // shell's view is untouched and nothing is left mounted when the command exits.
  let error = Command::new(program).args(args).exec();
  Err(failed(&format!("exec {program}"), error))
}

/// Refuses when `path` is not an existing directory, naming it exactly; never creates it.
fn ensure_directory(path: &str, what: &str) -> Result<(), Failure> {
  // structural: allow — a read-only stat of the path to refuse a missing one; creates nothing.
  match rustix::fs::stat(path) {
    Ok(st)
      if rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Directory =>
    {
      Ok(())
    }
    Ok(_) => Err(Failure::Failed(format!("{what} {path} is not a directory"))),
    Err(_) => Err(Failure::Failed(format!(
      "{what} {path} does not exist (the launcher does not create it)"
    ))),
  }
}

/// Enters a new user and mount namespace, mapping the caller's own uid and gid so the mount
/// capability is available without privilege. A host that forbids unprivileged user namespaces
/// is refused with the exact setting.
fn enter_namespace() -> Result<(), Failure> {
  let uid = rustix::process::getuid().as_raw();
  let gid = rustix::process::getgid().as_raw();
  // SAFETY: `unshare` changes this process's namespace membership, which rustix marks unsafe
  // because it affects every thread; the launcher is single-threaded here and execs
  // immediately after, so no other thread observes the change. It creates the private
  // namespace the bind mount and exec then use.
  let unshared =
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWUSER | UnshareFlags::NEWNS) };
  unshared.map_err(|e| {
    Failure::Failed(format!(
      "cannot create a user+mount namespace (code {:?}); enable unprivileged user namespaces \
       (sysctl kernel.unprivileged_userns_clone=1, or an AppArmor userns profile)",
      e.raw_os_error()
    ))
  })?;
  // The mapping: deny setgroups, then map this uid and gid to themselves inside the namespace.
  write_proc("/proc/self/setgroups", "deny")?;
  write_proc("/proc/self/gid_map", &format!("{gid} {gid} 1"))?;
  write_proc("/proc/self/uid_map", &format!("{uid} {uid} 1"))?;
  Ok(())
}

/// Writes `contents` to a `/proc/self` control file (a kernel pseudo-file, not disk, so this
/// creates nothing on any filesystem, R1).
fn write_proc(path: &str, contents: &str) -> Result<(), Failure> {
  // /proc/self is a kernel pseudo-filesystem; the file already exists and writing it configures
  // the namespace, it never touches disk (R1).
  // structural: allow — a write to a /proc control file, not a disk file.
  let fd = rustix::fs::open(path, OFlags::WRONLY, Mode::empty())
    .map_err(|e| failed(&format!("open {path}"), errno(e)))?;
  rustix::io::write(&fd, contents.as_bytes())
    .map_err(|e| failed(&format!("write {path}"), errno(e)))?;
  Ok(())
}

/// Makes the mount tree recursively private so the bind mount does not propagate to the parent
/// namespace (the parent shell's view stays unchanged).
fn make_root_private() -> Result<(), Failure> {
  mount_change(
    "/",
    MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
  )
  .map_err(|e| failed("making the mount tree private", errno(e)))
}

fn bind_error(source: &str, at: &str, e: rustix::io::Errno) -> Failure {
  Failure::Failed(format!(
    "cannot bind {source} onto {at} (code {:?})",
    e.raw_os_error()
  ))
}

fn errno(e: rustix::io::Errno) -> String {
  format!("code {:?}", e.raw_os_error())
}
