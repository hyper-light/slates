//! R2 on Windows (A-789's Windows shape; main's ruling of 2026-11-20 18:40): a content pool extent is a section of its
//! own, held by the anchor across a daemon restart and closed once wholly free, so its commit returns. These tests run
//! a real anchor and daemon through the `slates` binary, as `cli.rs` does on Unix. They live in their own target
//! because `cli.rs` is `#![cfg(unix)]` (its NFS bridge is a Unix dependency), where they had compiled to nothing on
//! Windows (w-lint, 2026-11-21). Like the rest of the CLI flows they run only with `SLATES_TEST_CLI=1` (the machine to
//! itself) and skip loudly otherwise.
#![cfg(windows)]
// Test harness code: an unwrap or a panic here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: shards for the daemon under test.
const SHARDS: &str = "2";
/// Shape: verbs in a row the daemon must answer before a test starts: the anchor may restart a daemon once at its start
/// (its first heartbeat lapsing), and a verb in that window answers exit 3.
const STABLE_STREAK: u32 = 5;
/// Shape: the bound on the anchor's start and the daemon's settling, past any healthy start.
const START_BOUND: Duration = Duration::from_secs(120);
/// Format: the line the daemon prints once it serves (`crates/api/src/role/daemon.rs`), the instance name after it.
const SERVING_LINE: &str = "slates daemon: instance ";

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a verb against `instance`: the exit code and both outputs.
fn run(instance: &str, args: &[&str]) -> (i32, String, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// Runs a verb with `input` on its stdin: the exit code, its stdout and its stderr.
fn run_with_stdin(instance: &str, args: &[&str], input: &[u8]) -> (i32, Vec<u8>, String) {
  use std::io::Write;
  let mut child = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child.stdin.take().unwrap().write_all(input).unwrap();
  let output = child.wait_with_output().unwrap();
  (
    output.status.code().unwrap_or(-1),
    output.stdout,
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// The value of the `key: value` line in `text`.
fn value_of(text: &str, key: &str) -> String {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .to_owned()
}

/// A short wait between polls (the harness paces its polls; shipped code parks on its driver, D-9).
fn pause() {
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(100));
}

/// The anchor process, killed and reaped when dropped, so a failed assertion leaves no daemon behind (on Windows the
/// anchor's job object ends the daemon with it).
struct AnchorProcess {
  child: Child,
  drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AnchorProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    if let Some(drain) = self.drain.take() {
      let _ = drain.join();
    }
  }
}

/// Starts `slates anchor` for `instance`, waits for its daemon to serve and answer verbs steadily, and bootstraps the
/// node; the anchor's stderr is echoed throughout, so it never blocks on a full pipe.
fn start_anchor(instance: &str) -> AnchorProcess {
  let mut child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let mut line = String::new();
  loop {
    line.clear();
    let read = stderr.read_line(&mut line).unwrap();
    assert!(read > 0, "the anchor ended before its daemon served");
    eprint!("{line}");
    if line.starts_with(&serving) {
      break;
    }
  }
  let drain = std::thread::spawn(move || {
    let mut line = String::new();
    while stderr.read_line(&mut line).is_ok_and(|read| read > 0) {
      eprint!("{line}");
      line.clear();
    }
  });
  let anchor = AnchorProcess {
    child,
    drain: Some(drain),
  };
  let started = Instant::now();
  let mut streak = 0u32;
  while streak < STABLE_STREAK {
    assert!(
      started.elapsed() < START_BOUND,
      "the daemon answers verbs steadily"
    );
    streak = if run(instance, &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    pause();
  }
  let (code, _, err) = run(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap: {err}");
  anchor
}

/// The system's commit charge, bytes: what Windows has promised backing for (`GlobalMemoryStatusEx`: the page file's
/// total less what is still available).
fn commit_charge() -> u64 {
  use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
  // SAFETY: an all-zero `MEMORYSTATUSEX` is a valid value for the call to fill; its length is set next.
  let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
  status.dwLength = u32::try_from(size_of::<MEMORYSTATUSEX>()).unwrap();
  // SAFETY: `status` is a live, sized `MEMORYSTATUSEX` the call fills.
  assert_ne!(unsafe { GlobalMemoryStatusEx(&raw mut status) }, 0);
  status
    .ullTotalPageFile
    .saturating_sub(status.ullAvailPageFile)
}

/// The pid the instance's `status` names; absent while no daemon answers.
fn status_pid(instance: &str) -> Option<u32> {
  let (code, text, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  text
    .split("\"pid\":")
    .nth(1)?
    .trim_start()
    .split(|character: char| !character.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Terminates process `pid` and waits for it to have exited.
fn terminate(pid: u32) {
  use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
  use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
  use windows_sys::Win32::System::Threading::{
    INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
  };
  // SAFETY: the pid came from the instance's status; only termination and its exit are asked for, nothing inherited.
  let process = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
  assert!(!process.is_null(), "OpenProcess({pid})");
  // SAFETY: the unique owned handle OpenProcess just returned.
  let process = unsafe { OwnedHandle::from_raw_handle(process) };
  // SAFETY: `process` stays owned for the call.
  assert_ne!(unsafe { TerminateProcess(process.as_raw_handle(), 1) }, 0);
  // SAFETY: `process` stays owned for the wait.
  assert_eq!(
    unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) },
    WAIT_OBJECT_0
  );
}

/// A volume of `bounded` bytes made through the CLI and attached for writing: its id and the attachment.
fn writable_volume(instance: &str, name: &str, bounded: &str) -> (String, String) {
  let (code, out, err) = run(instance, &["volume", "create", name, "--bounded", bounded]);
  assert_eq!(code, 0, "create {name}: {err}");
  let id = value_of(&out, "id");
  let (code, out, err) = run(instance, &["attach", &id, "--write"]);
  assert_eq!(code, 0, "attach {name}: {err}");
  (id, value_of(&out, "attachment"))
}

/// MEMORY SIZING, R2 on Windows (main's ruling of 2026-11-20 18:40: every platform gives a burst's memory back; A-789's
/// Windows shape). Do: under a real anchor, read the system's commit charge, write a burst of N bytes into a volume of
/// its own, destroy the volume, and wait. Expect the commit charge to come back to within a quarter of N of where it
/// began. Before, each pool extent was a range of the anchor's one content section, committed whole when the daemon
/// mapped it, and a section's commit comes back only when it closes (g226 on jupiter: decommitting a view is refused,
/// error 87), so a freed extent kept its commit for the anchor's whole life. Now each extent is a section of its own,
/// closed once wholly free.
#[test]
fn a_freed_content_extent_gives_its_commit_back_on_windows() {
  /// Shape: the burst, far above the commit noise of a test's own processes.
  const BURST: usize = 256 << 20;
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!("skipping the Windows extent commit flow: set SLATES_TEST_CLI=1 to run it");
    return;
  }
  let instance = format!("cli-extent-commit-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let before = commit_charge();
  let (id, attachment) = writable_volume(&instance, "burst", "512MiB");
  let burst: Vec<u8> = (0..BURST)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let (code, _, err) = run_with_stdin(&instance, &["write", &id, &attachment, "burst.bin"], &burst);
  assert_eq!(code, 0, "write: {err}");
  drop(burst);
  let with_burst = commit_charge();
  let (code, _, err) = run(&instance, &["detach", &attachment]);
  assert_eq!(code, 0, "detach: {err}");
  let (code, _, err) = run(&instance, &["volume", "destroy", &id]);
  assert_eq!(code, 0, "destroy: {err}");
  let file = u64::try_from(BURST).unwrap();
  let started = std::time::Instant::now();
  let mut after = commit_charge();
  while after.saturating_sub(before) * 4 >= file
    && started.elapsed() < std::time::Duration::from_secs(60)
  {
    pause();
    after = commit_charge();
  }
  drop(anchor);
  eprintln!(
    "a {} MiB burst: commit {} MiB before, {} MiB with it, {} MiB after its volume was destroyed",
    BURST >> 20,
    before >> 20,
    with_burst >> 20,
    after >> 20
  );
  assert!(
    with_burst.saturating_sub(before) >= file / 2,
    "the burst was committed: {} MiB before, {} MiB with it",
    before >> 20,
    with_burst >> 20
  );
  assert!(
    after.saturating_sub(before) * 4 < file,
    "the freed extents gave their commit back: {} MiB before, {} MiB after, for a {} MiB burst",
    before >> 20,
    after >> 20,
    BURST >> 20
  );
}

/// R2 on Windows, condition (d) of main's ruling: recovery adopts every extent after a daemon restart, across
/// processes. Do: under a real anchor, write a file of several chunk windows into a volume, terminate the daemon, and
/// wait for the anchor's successor to answer. Expect the file to read back byte for byte: its extents' sections were
/// held by the anchor across the death and handed to the successor (A-789's hold kind `WHAT_CONTENT_EXTENT`).
#[test]
fn a_content_extent_outlives_a_killed_daemon_on_windows() {
  /// Shape: the file, several chunk windows.
  const FILE: usize = 16 << 20;
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!("skipping the Windows extent restart flow: set SLATES_TEST_CLI=1 to run it");
    return;
  }
  let instance = format!("cli-extent-restart-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (id, attachment) = writable_volume(&instance, "kept", "64MiB");
  let bytes: Vec<u8> = (0..FILE)
    .map(|at| u8::try_from((at * 7 + at / 4096) % 251).unwrap())
    .collect();
  let (code, _, err) = run_with_stdin(&instance, &["write", &id, &attachment, "kept.bin"], &bytes);
  assert_eq!(code, 0, "write: {err}");
  let killed = status_pid(&instance).expect("the instance's daemon answers");
  terminate(killed);
  let started = std::time::Instant::now();
  while status_pid(&instance).is_none_or(|pid| pid == killed) {
    assert!(
      started.elapsed() < std::time::Duration::from_secs(120),
      "the anchor's successor answers"
    );
    pause();
  }
  let (code, read, err) = run_with_stdin(&instance, &["read", &id, "kept.bin"], b"");
  drop(anchor);
  assert_eq!(code, 0, "read after the restart: {err}");
  assert!(
    read == bytes,
    "the file reads back byte for byte after the daemon's death"
  );
}
