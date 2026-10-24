//! `slates_ipc::stream::exec_with` by use (§4.6, A-161): a process replacing itself with a program hands it a stream
//! end at a number, and the program writes through that number. An exec replaces the process that makes it, so this test
//! binary is re-invoked as that process (an environment variable selects the role and carries the number its end
//! arrives at). The refusals return and run in place, in this one test, one after another: the failed exec leaves the
//! end at its number in this process, and no other test may be holding that number meanwhile.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#![cfg(unix)]

use std::ffi::OsStr;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;

use slates_ipc::IpcError;
use slates_ipc::stream::{FIRST_HANDED_NUMBER, exec_with, pair, recv, send};

/// Format: the variable that makes this binary the process that execs; its value is the number its end arrives at.
const ROLE_FD: &str = "SLATES_IPC_TEST_EXEC_FD";
/// Format: the number the parent hands the role its end at, and the one the role's program is handed it at.
const INHERITED_AT: i32 = 9;
/// Format: see [`INHERITED_AT`].
const HANDED_AT: i32 = 5;
/// Format: this test's name, which the re-invoked binary runs.
const TEST: &str = "a_process_replaced_by_its_program_hands_it_the_end_and_refusals_return";
/// Shape: the bytes a read takes, more than any message here.
const READ_BYTES: usize = 16;
/// Shape: how long the kept end waits for a byte or its end of stream before the test calls a copy leaked.
const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The process that execs: it adopts the end at the number it was told and replaces itself with `sh`, which writes
/// through descriptor [`HANDED_AT`]. Returns only if the exec failed, which fails the run.
fn role(number: &str) -> ! {
  let number: i32 = number.parse().unwrap();
  // SAFETY: the parent placed the end at this number before this process's exec; nothing here opened it, so this is its
  // one owner.
  let end = unsafe { OwnedFd::from_raw_fd(number) };
  let error = exec_with(
    OsStr::new("/bin/sh"),
    &[
      "-c".into(),
      format!("printf handed >&{HANDED_AT}; exec {HANDED_AT}>&-; read wait; exit 0").into(),
    ],
    end,
    HANDED_AT,
  );
  panic!("the exec failed: {error}");
}

/// Do: (1) hand a pair's end to a process that replaces itself with `sh` at descriptor 5, `sh` writing through it, then
/// closing its 5 and waiting; (2) ask for number 2; (3) replace this process with a program that does not exist. Expect
/// (1) the bytes on the kept end, then its end of stream while `sh` still waits, so no other copy of the end went into
/// the program (the process's own copy, inherited here without close-on-exec, would have held it open); (2) refused
/// before anything changes; (3) refused as a failed exec, with the end then at the number asked, in this process,
/// carrying what is written there to the kept end.
#[test]
fn a_process_replaced_by_its_program_hands_it_the_end_and_refusals_return() {
  if let Some(number) = std::env::var_os(ROLE_FD) {
    role(&number.to_string_lossy());
  }
  a_replaced_process_hands_its_program_the_end_alone();
  let (_kept, low) = pair().unwrap();
  assert!(matches!(
    exec_with(OsStr::new("/bin/sh"), &[], low, FIRST_HANDED_NUMBER - 1),
    IpcError::Layout { .. }
  ));
  a_failed_exec_leaves_the_end_at_its_number();
}

/// Phase (1): the role, spawned with the end at [`INHERITED_AT`], replaces itself with `sh`, which writes through 5,
/// closes it and waits; the end closes while `sh` still runs.
fn a_replaced_process_hands_its_program_the_end_alone() {
  let (kept, handed) = pair().unwrap();
  rustix::io::ioctl_fionbio(&kept, false).unwrap();
  // The number the child takes the end at is held here until the spawn returns, so the standard library's own
  // exec-error pipe can never be given it (the hazard `exec_with` replaced `spawn_with` to escape).
  let holder = rustix::io::fcntl_dupfd_cloexec(&handed, INHERITED_AT).unwrap();
  let from = handed.as_raw_fd();
  let mut command = std::process::Command::new(std::env::current_exe().unwrap());
  command
    .args(["--exact", TEST, "--nocapture"])
    .env(ROLE_FD, INHERITED_AT.to_string())
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::null());
  // SAFETY: the closure runs in the forked child between `fork` and `exec` and makes one async-signal-safe call,
  // `dup2`, on numbers it copied; `from` is open until the spawn returns, as `handed` is dropped only after it.
  unsafe {
    command.pre_exec(move || {
      if libc::dup2(from, INHERITED_AT) < 0 {
        return Err(std::io::Error::last_os_error());
      }
      Ok(())
    });
  }
  let mut child = command.spawn().unwrap();
  drop(holder);
  drop(handed);
  rustix::net::sockopt::set_socket_timeout(&kept, rustix::net::sockopt::Timeout::Recv, Some(WAIT))
    .unwrap();
  let mut buf = [0u8; READ_BYTES];
  let read = recv(kept.as_fd(), &mut buf).unwrap();
  assert_eq!(buf.get(..read), Some(&b"handed"[..]));
  assert_eq!(
    recv(kept.as_fd(), &mut buf),
    Ok(0),
    "the end closed with the program's 5 while it still runs: no other copy went into it"
  );
  assert!(child.try_wait().unwrap().is_none(), "sh still waits");
  drop(child.stdin.take());
  assert!(child.wait().unwrap().success(), "the role ran sh");
}

/// Phase (3): a program that does not exist is refused as a failed exec, and the end is then at the number asked, in
/// this process, carrying what is written there to the kept end.
fn a_failed_exec_leaves_the_end_at_its_number() {
  let (kept, end) = pair().unwrap();
  rustix::io::ioctl_fionbio(&kept, false).unwrap();
  let at = rustix::io::fcntl_dupfd_cloexec(&kept, 100)
    .unwrap()
    .as_raw_fd();
  let refused = exec_with(OsStr::new("/nonexistent/slates-exec-test"), &[], end, at);
  assert!(
    matches!(refused, IpcError::OsRefused { call: "exec", .. }),
    "{refused}"
  );
  // SAFETY: the failed exec's `dup2` placed the end at `at`, which nothing else in this test holds.
  let placed = unsafe { OwnedFd::from_raw_fd(at) };
  send(placed.as_fd(), b"placed").unwrap();
  let mut buf = [0u8; READ_BYTES];
  let read = recv(kept.as_fd(), &mut buf).unwrap();
  assert_eq!(buf.get(..read), Some(&b"placed"[..]));
}
