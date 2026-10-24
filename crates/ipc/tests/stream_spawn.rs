//! `slates_ipc::stream::spawn_with` by use (§4.6, A-161): a child handed a stream end at a descriptor number writes
//! through that number. A test binary of its own: a fork copies every descriptor the process holds until the child's
//! `exec`, and on macOS a socket pair is made close-on-exec only after it is made, so a child forked beside the stream
//! module's tests would hold their ends and hide each peer's close from them (the CLAUDE.md gotcha on children of a
//! shared test process).
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#![cfg(unix)]

use std::ffi::OsStr;
use std::os::fd::{AsFd, AsRawFd};

use slates_ipc::stream::{FIRST_HANDED_NUMBER, pair, recv, spawn_with};

/// Shape: how many numbers from the lowest free one a spawn is tried at: the standard library's exec-error pipe takes
/// the two lowest free numbers at the spawn, so its write end is the second, and a few more cover a descriptor
/// another thread holds meanwhile.
const NUMBERS_TRIED: i32 = 4;

/// Do: hand a pair's end to `sh` at descriptor 5 and have it write through that number; then ask for number 2.
/// Expect the bytes on the daemon's end, the daemon's end to read the end once the child exits (the parent kept no
/// copy), and the low number refused before anything is spawned.
#[test]
fn a_spawned_child_writes_through_the_number_it_was_handed() {
  let (daemon, handed) = pair().unwrap();
  let mut child = spawn_with(
    OsStr::new("/bin/sh"),
    &["-c".into(), "printf handed >&5".into()],
    handed,
    5,
  )
  .unwrap();
  assert!(child.wait().unwrap().success());
  rustix::io::ioctl_fionbio(&daemon, false).unwrap();
  let mut buf = [0u8; 16];
  let read = recv(daemon.as_fd(), &mut buf).unwrap();
  assert_eq!(buf.get(..read), Some(&b"handed"[..]));
  assert_eq!(
    recv(daemon.as_fd(), &mut buf),
    Ok(0),
    "no copy outlived the child"
  );
  let (_daemon, low) = pair().unwrap();
  assert!(
    spawn_with(OsStr::new("/bin/sh"), &[], low, 2).is_err(),
    "standard error is not a stream's number"
  );
}

/// Do: ask to spawn a program that does not exist, handing it the end at each of the lowest free numbers in turn,
/// among them the number the standard library's own exec-error pipe takes for its write end. Expect every spawn
/// refused and nothing written into the stream. A child's `dup2` onto that pipe's number would close the pipe, so the
/// parent would read its end at once and report a program that never ran as spawned, and the failed exec would write
/// its error into the stream.
#[test]
fn a_program_that_cannot_run_is_refused_whatever_number_it_is_handed_at() {
  for offset in 0..NUMBERS_TRIED {
    let (daemon, handed) = pair().unwrap();
    let lowest_free = rustix::io::fcntl_dupfd_cloexec(&daemon, FIRST_HANDED_NUMBER)
      .unwrap()
      .as_raw_fd();
    let at = lowest_free + offset;
    let spawned = spawn_with(
      OsStr::new("/nonexistent/slates-spawn-with-test"),
      &[],
      handed,
      at,
    );
    if let Ok(mut child) = spawned {
      let _ = child.wait();
      panic!("at {at}: a program that does not exist was reported spawned");
    }
    rustix::io::ioctl_fionbio(&daemon, false).unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(
      recv(daemon.as_fd(), &mut buf),
      Ok(0),
      "at {at}: nothing reached the stream and no copy outlived the spawn"
    );
  }
}
