//! Whether anything but a process group's leader is still in the group (main 01a20182's ruling of 2026-11-22 ~17:30,
//! the hook drain): what tells a hook's run that nothing of the hook's can still write its words stream, once the hook
//! itself has exited, and whatever still holds the stream open is something the hook never started.
//!
//! **Why it is asked.** A hook's words stream is a pipe the daemon makes. Where a pipe is made in two calls, a pipe and
//! then its close-on-exec flag, a process spawned in between by anyone in the same process (a test binary's stock
//! client, an embedder's own spawn) inherits it and holds it open while it lives. The daemon's own spawns are confined
//! (`crate::confined_spawn`), so only a foreign spawner can. Such a holder is outside the hook's group, so once the group
//! holds nothing but its exited leader, the stream's writers that are the hook's are gone, and its run is judged by its
//! own exit.
//!
//! **The leader stays a zombie.** The hook's run reads its exit without reaping it, so its pid, and with it the group's
//! id, stays reserved: a later kill of the group can never reach a group that took the id. So `kill(-pgid, 0)` answers
//! success while the leader stands, and "empty" means no member but the leader.
//!
//! **Per platform**, three arms of one signature:
//! - macOS lists the group (`proc_listpgrppids`): std makes a pipe there with `pipe()` and then `set_cloexec()` on each
//!   end (`library/std/src/sys/pipe/unix.rs`, the `_` arm, Rust 1.94.1's source), so the window is real.
//! - Linux answers that others may hold it: std makes a pipe with `pipe2(O_CLOEXEC)` there (the same file's Linux arm),
//!   atomic, so no foreign spawn inherits it and whatever holds it past the leader's exit is something the hook started,
//!   waited for until the stream ends or the run's deadline.
//! - Windows answers the same: std makes a pipe with `CreatePipe(&read, &write, null, 0)`
//!   (`library/std/src/sys/pipe/windows.rs`, Rust 1.94.1), and "If *lpPipeAttributes* is **NULL**, the handle cannot be
//!   inherited" (Microsoft, `CreatePipe`, learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-createpipe).

/// Shape: the pids a group listing is read into: its leader and one more are all the answer needs.
#[cfg(target_os = "macos")]
const LISTED: usize = 2;

/// Whether a process other than `leader`, its group's leader (the group's id), is in the group (module docs): asked of
/// the kernel; a listing refused answers that others may be, which keeps a run waiting as before.
#[cfg(target_os = "macos")]
pub fn group_has_others(leader: u32) -> bool {
  let Ok(group) = libc::pid_t::try_from(leader) else {
    return true;
  };
  let mut listed = [0 as libc::pid_t; LISTED];
  let Ok(room) = libc::c_int::try_from(std::mem::size_of_val(&listed)) else {
    return true;
  };
  // SAFETY: `listed` is a live, writable array of `room` bytes for the whole call, which writes at most `room` bytes of
  // pids into it and keeps no pointer to it after it returns.
  let answered = unsafe { libc::proc_listpgrppids(group, listed.as_mut_ptr().cast(), room) };
  if answered < 0 {
    return true;
  }
  // The pids written, read from the array itself: an unwritten slot stays 0, which is no process.
  listed.iter().any(|pid| *pid != 0 && *pid != group)
}

/// [`group_has_others`] on Linux (module docs): others may hold it, since only the hook's own can.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn group_has_others(_leader: u32) -> bool {
  true
}

/// [`group_has_others`] on Windows (module docs): others may hold it, since only the hook's own can.
#[cfg(windows)]
pub fn group_has_others(_leader: u32) -> bool {
  true
}
