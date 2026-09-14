//! The mount helper's descriptor handshake against real child processes (AC-3.12/T-3.15 "fail
//! the mount descriptor handshake ... expect no orphan child"; audit BUG-14). On every Unix host,
//! no mount and no privilege: a helper that exits without sending a descriptor is reaped and its
//! exit status reported (only a reaped child yields one); a helper that never answers is killed
//! at the derived deadline and reaped (its signal reported); and a helper that sends a
//! descriptor — this test binary re-invoked in the helper role, the anchor tests' idiom — hands
//! it over. Nothing here writes disk: the descriptor the helper sends is `/dev/null`.
//!
//! The deadline is derived, not chosen: a trivial helper's whole handshake (spawn, exit, the
//! socket closing) is timed first, and a helper that has not answered within a hundred of those
//! is not answering.
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
use slates_bridge_fuse::mount::{COMM_FD_ENV, COMM_FD_IN_CHILD, HelperExit, MountError, handshake};

/// Format: the environment variable that turns this binary into the descriptor-sending helper.
const HELPER_ROLE: &str = "SLATES_FUSE_TEST_HELPER_ROLE";
/// Derived: the deadline is this many trivial-helper handshakes — a helper that has not
/// answered within a hundred spawn-and-exit round trips is not answering, on a loaded box as on
/// an idle one, since both sides scale with the same scheduler.
const DEADLINE_IN_TRIVIAL_HANDSHAKES: u32 = 100;
/// Format: `SIGKILL`, the signal the handshake's cancellation sends.
const SIGKILL: i32 = 9;
/// Shape: a helper that sleeps far past any derived deadline (seconds), so a handshake that did
/// not enforce its deadline would visibly hang the test rather than pass by luck.
const HANG_SECONDS: &str = "30";

/// Shape: how many trivial handshakes are timed for the deadline's unit; the slowest is taken, so
/// one unloaded sample never sets a deadline the loaded runs cannot meet (the tests here spawn
/// processes in parallel, and this box is shared).
const TRIVIAL_SAMPLES: u32 = 5;

/// Times one whole handshake with a helper of the real helper's own class — a shell that exits
/// at once — the unit the deadline is derived from.
fn trivial_handshake() -> Duration {
  let start = Instant::now();
  let mut helper = Command::new("sh");
  helper.args(["-c", "true"]);
  // A helper that sends no descriptor ends as NoDevice, which is what a trivial run produces.
  let outcome = handshake(helper, Duration::from_secs(60));
  assert!(matches!(outcome, Err(MountError::NoDevice { .. })));
  start.elapsed()
}

/// The derived deadline: a hundred times the slowest of several trivial handshakes (floored at
/// one nanosecond, so an all-zero measurement on a coarse clock still yields a deadline the
/// kernel can enforce).
fn derived_deadline() -> Duration {
  let slowest = (0..TRIVIAL_SAMPLES)
    .map(|_| trivial_handshake())
    .max()
    .unwrap_or_default()
    .max(Duration::from_nanos(1));
  slowest * DEADLINE_IN_TRIVIAL_HANDSHAKES
}

/// A helper that exits without sending a descriptor is reaped, and the error carries its exit
/// status — evidence the child was waited for, not left an orphan or a zombie (audit BUG-14: the
/// early return before `wait`).
#[test]
fn a_helper_that_exits_without_a_descriptor_is_reaped_and_its_exit_reported() {
  let mut helper = Command::new("sh");
  helper.args(["-c", "exit 7"]);
  let outcome = handshake(helper, derived_deadline());
  match outcome {
    Err(MountError::NoDevice { exit }) => assert_eq!(
      exit,
      HelperExit {
        code: Some(7),
        signal: None
      },
      "the reaped helper's own exit code"
    ),
    other => panic!("expected NoDevice with the helper's exit, got {other:?}"),
  }
}

/// A helper that never answers is killed at the derived deadline and reaped: the error names the
/// deadline and the signal, and the whole handshake takes far less than the helper's own life.
#[test]
fn a_helper_that_never_answers_is_killed_at_the_derived_deadline_and_reaped() {
  let deadline = derived_deadline();
  // `exec`, so the process the handshake kills is the sleeper itself, not a shell whose child
  // would outlive the test.
  let mut helper = Command::new("sh");
  helper.args(["-c", &format!("exec sleep {HANG_SECONDS}")]);
  let start = Instant::now();
  let outcome = handshake(helper, deadline);
  let took = start.elapsed();
  match outcome {
    Err(MountError::Timeout {
      deadline: reported,
      exit,
    }) => {
      assert_eq!(reported, deadline);
      assert_eq!(
        exit,
        HelperExit {
          code: None,
          signal: Some(SIGKILL)
        },
        "the helper was killed and reaped"
      );
    }
    other => panic!("expected Timeout, got {other:?}"),
  }
  assert!(
    took < Duration::from_secs(HANG_SECONDS.parse::<u64>().unwrap()),
    "the handshake ended at the deadline, not the helper's sleep ({took:?})"
  );
}

/// A helper that sends a descriptor hands it over: this binary re-invoked in the helper role
/// sends `/dev/null` over `_FUSE_COMMFD`, and the handshake returns a usable descriptor once the
/// helper has exited successfully.
#[test]
fn a_helper_that_sends_a_descriptor_hands_it_over() {
  let exe = std::env::current_exe().unwrap();
  let mut helper = Command::new(exe);
  helper
    .args([
      "--ignored",
      "--exact",
      "helper_role_sends_a_descriptor",
      "--nocapture",
    ])
    .env(HELPER_ROLE, "1");
  let device = handshake(helper, derived_deadline()).unwrap();
  let stat = rustix::fs::fstat(device.as_fd()).unwrap();
  assert_eq!(
    rustix::fs::FileType::from_raw_mode(stat.st_mode),
    rustix::fs::FileType::CharacterDevice,
    "the descriptor the helper sent (/dev/null) arrived intact"
  );
}

/// The helper role: reads the socket number from the environment (its standard input, as the
/// protocol names it), sends `/dev/null` over it with `SCM_RIGHTS`, and exits successfully.
/// Ignored like the anchor's supervised child; run only by
/// `a_helper_that_sends_a_descriptor_hands_it_over` with `--ignored --exact`.
#[test]
#[ignore = "the descriptor-sending helper; run by a_helper_that_sends_a_descriptor_hands_it_over"]
fn helper_role_sends_a_descriptor() {
  if std::env::var(HELPER_ROLE).is_err() {
    return;
  }
  let comm: i32 = std::env::var(COMM_FD_ENV).unwrap().parse().unwrap();
  assert_eq!(
    comm, COMM_FD_IN_CHILD,
    "the handshake places the socket at the child's stdin"
  );
  // SAFETY: the number came from the handshake's environment and names the socket end this
  // process received as its standard input; nothing else in this process reads it (libtest does
  // not touch stdin), so taking ownership here for the send is sound, and it is closed at exit.
  let socket = unsafe { OwnedFd::from_raw_fd(comm) };
  let device = rustix::fs::open(
    "/dev/null",
    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
    rustix::fs::Mode::empty(),
  )
  .unwrap();
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
  let mut control = SendAncillaryBuffer::new(&mut space);
  let fds = [device.as_fd()];
  assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
  rustix::net::sendmsg(
    &socket,
    &[std::io::IoSlice::new(&[0u8])],
    &mut control,
    SendFlags::empty(),
  )
  .unwrap();
  std::process::exit(0);
}
