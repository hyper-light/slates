//! A-510 (§2.6 "Boot order", the stop; `docs/bugs/2026-11-18-a-stop-asked-by-a-sender-that-exited.md`): a stop asked of
//! the anchor completes within its bound whether or not the process that asked stays. kiln's real-instance test (g267,
//! the Mac Studio, 2026-11-18) sent its anchor `SIGTERM` and exited, and its instance was seen still serving about two
//! minutes later; the same test, waiting for the stop, saw it end in about 0.2 s.
//!
//! The test is that shape across real processes. A sender (this binary re-invoked as [`a_sender_role`]) starts the anchor
//! as its child in a process group of its own, as a host's start does (`StartSpec::anchor_command`), connects a client
//! that creates a volume, attaches it for writing and writes it, sends the anchor `SIGTERM` and exits at once: its client
//! still connected, its write attachment held and the anchor not waited for. The test then watches the anchor and its
//! daemon by their process ids alone, with no client of its own, since a call is work that could wake a stop nothing
//! else woke. The anchor and the daemon are the liveness tests' roles (`super::liveness_tests`), the product's own
//! supervision and serve; their lines come to the test on the sender's standard streams, which both inherit.
//!
//! It did not reproduce kiln's delay (the Mac Studio, 2026-11-18): the anchor and its daemon were gone 283 ms after the
//! signal with a bare client, and 191 ms with the client mid-use. So the stop does not wait on the process that asked
//! for it: the daemon's stop asks no client anything, and the anchor holds it to its own bounds (`stop_step`). The test
//! stays as the guard of that rule, and the bug record says what kiln's run had that this one has not.
// Unix only, as the liveness tests (their module says why).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slates_client::{CreateSpec, Intent, NamePolicy, SizeClass};
use slates_server::daemon::OBSERVE_BUDGET_NS;

use super::liveness_tests::{
  ANCHOR_ROLE, DAEMON_READY, HOLD, INSTANCE, PID_WORD, ROLE, START_WAIT, STEP_PAUSE, await_tag,
  connect, lines_of, number_after, pause,
};

/// Format: the environment variable that runs the sender role, naming its instance.
const SENDER: &str = "SLATES_A510_SENDER";
/// Format: the sender's line once it has sent the anchor `SIGTERM`, looked for anywhere in a line, and the word before
/// the anchor's process id.
const SENT_TAG: &str = "A510-SENT";
const ANCHOR_WORD: &str = "anchor=";
/// Format: the liveness tests' anchor role, which the sender starts, and this module's sender role.
const ANCHOR_ROLE_PATH: &str = "role::liveness_tests::an_anchor_role";
const SENDER_ROLE_PATH: &str = "role::stop_tests::a_sender_role";
/// Format: what the daemon role holds: nothing, so it serves.
const SERVES: &str = "none";
/// Format: `setpgid`'s "the child's own process id" for a new group, as a host's start gives the anchor.
const OWN_GROUP: i32 = 0;
/// Format: the sender's volume, its file, the file's bytes and its mode (a plain file's).
const HELD_VOLUME: &str = "held";
const HELD_FILE: &str = "/held-at-exit";
const HELD_BYTES: &[u8] = b"written under a write attachment the sender holds as it exits";
const FILE_MODE: u32 = 0o644;
/// Shape: the sender's volume's bound: a mebibyte, room for its one file.
const VOLUME_BOUND: u64 = 1 << 20;
/// Shape: the bound the stop is held to here: the observation budget, the longest any one step of a stop waits (the
/// replicas' give-back, A-457), which a stop whose sender stays meets in about 0.2 s (g267's runs) and kiln's
/// fire-and-exit test missed by minutes.
const STOP_WITHIN: Duration = Duration::from_nanos(OBSERVE_BUDGET_NS);

/// The sender role: starts the anchor role as its child in a process group of its own, connects a client that creates a
/// volume, attaches it for writing and writes it, sends the anchor `SIGTERM`, says so, and exits at once: its client
/// still connected and its attachment held (no destructor runs), the anchor not waited for, as kiln's test did.
#[test]
#[ignore = "a role: run by this module's test in a child process"]
fn a_sender_role() {
  let Ok(instance) = std::env::var(SENDER) else {
    return;
  };
  let anchor = Command::new(std::env::current_exe().unwrap())
    .args(["--ignored", "--exact", ANCHOR_ROLE_PATH, "--nocapture"])
    .env_remove(SENDER)
    .env(ROLE, ANCHOR_ROLE)
    .env(INSTANCE, &instance)
    .env(HOLD, SERVES)
    .stdin(Stdio::null())
    .process_group(OWN_GROUP)
    .spawn()
    .unwrap();
  // A client mid-use, as kiln's process was: a volume of its own, attached for writing and written, the attachment
  // held when the process ends.
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      name: HELD_VOLUME.to_owned(),
      size: SizeClass::Bounded {
        limit: VOLUME_BOUND,
      },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let held = client.attach(volume, None, Intent::Write).unwrap();
  client
    .fs_write((volume, held.attachment), HELD_FILE, HELD_BYTES, FILE_MODE)
    .unwrap();
  let pid = rustix::process::Pid::from_raw(i32::try_from(anchor.id()).unwrap()).unwrap();
  rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
  println!("{SENT_TAG} {ANCHOR_WORD}{}", anchor.id());
  std::process::exit(0);
}

/// Whether the process `pid` still exists (a zombie included): `kill(pid, 0)`, refused `ESRCH` once it is gone.
fn exists(pid: u64) -> bool {
  let Some(pid) = i32::try_from(pid)
    .ok()
    .and_then(rustix::process::Pid::from_raw)
  else {
    return false;
  };
  !matches!(
    rustix::process::test_kill_process(pid),
    Err(rustix::io::Errno::SRCH)
  )
}

/// A-510 (§2.6, the stop). Do: a sender starts the anchor as its child in a process group of its own, connects a client
/// that holds a written write attachment, sends the anchor `SIGTERM` and exits at once; watch the anchor and its daemon
/// by their process ids alone. Expect: both gone within [`STOP_WITHIN`] of the signal. kiln's test of that shape saw its
/// instance still serving about two minutes after (g267, 2026-11-18), where waiting for the stop saw it end in about
/// 0.2 s; here both are gone in about 0.2 s either way.
#[test]
fn an_anchor_whose_stop_was_asked_by_a_sender_that_exited_stops_within_its_bound() {
  let instance = format!("a510-{}", std::process::id());
  let mut sender = Command::new(std::env::current_exe().unwrap())
    .args(["--ignored", "--exact", SENDER_ROLE_PATH, "--nocapture"])
    .env(SENDER, &instance)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let stdout = lines_of(sender.stdout.take().unwrap());
  let stderr = lines_of(sender.stderr.take().unwrap());
  let daemon = number_after(&await_tag(&stdout, DAEMON_READY, START_WAIT), PID_WORD);
  let anchor = number_after(&await_tag(&stdout, SENT_TAG, START_WAIT), ANCHOR_WORD);
  let sent = Instant::now();
  let _sender_gone = sender.wait();
  let stopped = loop {
    if !exists(anchor) && !exists(daemon) {
      break Some(sent.elapsed());
    }
    if sent.elapsed() > STOP_WITHIN {
      break None;
    }
    pause(STEP_PAUSE);
  };
  if stopped.is_none() {
    // Nothing is left behind by a failed run: both ended outright.
    for pid in [daemon, anchor] {
      if let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
      {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
      }
    }
  }
  let lines: Vec<String> = stdout
    .try_iter()
    .chain(stderr.try_iter())
    .map(|(at, line)| format!("{at} {line}"))
    .collect();
  println!("A510: stopped {stopped:?} after SIGTERM (anchor {anchor}, daemon {daemon})");
  for line in &lines {
    println!("A510 line: {line}");
  }
  assert!(
    stopped.is_some(),
    "the anchor (pid {anchor}) and its daemon (pid {daemon}) still ran {STOP_WITHIN:?} after SIGTERM from a sender \
     that exited; their lines: {lines:#?}"
  );
}
