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
/// Derived: the bound on the anchor's start and the daemon's settling, past any healthy start: [`START_BUDGETS`]
/// observation budgets (`slates_machine::budgets::observe_budget_ns`, the longest any party goes unobserved before its
/// observer's question ends with a deadline), for the anchor's start, the daemon's recovery and its one startup restart.
const START_BOUND: Duration =
  Duration::from_nanos(START_BUDGETS * slates_machine::budgets::observe_budget_ns());
/// Shape: the observation budgets in [`START_BOUND`]: twelve, the 120 s it was at today's 10 s budget.
const START_BUDGETS: u64 = 12;
/// Derived: how long a test waits for `slates write` to say its hold is taken ([`write_over_held`]): two observation
/// budgets (`slates_machine::budgets::observe_budget_ns`), one for the write's start and connect and one for its hold's
/// first renewal, each a question its asker ends within one budget.
const HOLD_WAIT: Duration = Duration::from_nanos(2 * slates_machine::budgets::observe_budget_ns());
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
  // Fed beside the wait, and a feed the command cut short by ending first is part of its outcome, not the test's panic:
  // a held write that fails ends before it reads (A-879), and its stderr says why.
  let mut stdin = child.stdin.take().unwrap();
  let (fed, output) = std::thread::scope(|scope| {
    let feeder = scope.spawn(move || stdin.write_all(input));
    let output = child.wait_with_output().unwrap();
    (feeder.join().unwrap(), output)
  });
  let mut err = String::from_utf8_lossy(&output.stderr).into_owned();
  if let Err(error) = fed {
    err.push_str(&format!("\n(the input was not all read: {error})"));
  }
  (output.status.code().unwrap_or(-1), output.stdout, err)
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
  start_anchor_of(instance, SHARDS)
}

/// [`start_anchor`] with `shards` shards.
fn start_anchor_of(instance: &str, shards: &str) -> AnchorProcess {
  let anchor = spawn_anchor(instance, shards);
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

/// Starts `slates anchor` for `instance` with `shards` shards and returns the moment its daemon's serving line is read,
/// within [`START_BOUND`]; the anchor's stderr is echoed throughout, so it never blocks on a full pipe, and a daemon
/// that never serves fails the test at the bound with its stderr above it.
fn spawn_anchor(instance: &str, shards: &str) -> AnchorProcess {
  let mut child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      shards,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let (served, serves) = std::sync::mpsc::channel::<()>();
  let drain = std::thread::spawn(move || {
    let mut line = String::new();
    while stderr.read_line(&mut line).is_ok_and(|read| read > 0) {
      eprint!("{line}");
      if line.starts_with(&serving) {
        let _ = served.send(());
      }
      line.clear();
    }
  });
  let anchor = AnchorProcess {
    child,
    drain: Some(drain),
  };
  assert!(
    serves.recv_timeout(START_BOUND).is_ok(),
    "{instance}'s daemon served within {START_BOUND:?} (the anchor's and daemon's stderr is above)"
  );
  anchor
}

/// Derived: the bound on one test's whole run under its anchor, past any healthy run: a start ([`START_BOUND`]) and as
/// long again for its verbs.
const TEST_BOUND: Duration = Duration::from_secs(2 * START_BOUND.as_secs());

/// A test's bound (main 01a20182's ask of 2026-11-23): past [`TEST_BOUND`] the anchor is killed, which ends its daemon
/// through its job, so a step blocked on the daemon (a claim waits while its holder lives) returns and the test fails
/// at that step, named, rather than holding the run; the anchor's and daemon's stderr is echoed above. Dropped at the
/// test's end, it lets the anchor be.
struct Bounded {
  done: Option<std::sync::mpsc::Sender<()>>,
  watcher: Option<std::thread::JoinHandle<()>>,
}

impl Bounded {
  /// Watches `anchor` for the test named `test`.
  fn over(anchor: &AnchorProcess, test: &'static str) -> Bounded {
    let pid = anchor.child.id();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watcher = std::thread::spawn(move || {
      if matches!(
        finished.recv_timeout(TEST_BOUND),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
      ) {
        eprintln!(
          "{test} passed its bound of {TEST_BOUND:?}: its anchor (pid {pid}) is killed, and its daemon with it"
        );
        end_quietly(pid);
      }
    });
    Bounded {
      done: Some(done),
      watcher: Some(watcher),
    }
  }
}

impl Drop for Bounded {
  fn drop(&mut self) {
    drop(self.done.take());
    if let Some(watcher) = self.watcher.take() {
      let _ = watcher.join();
    }
  }
}

/// Terminates process `pid` if it still runs: nothing when it has gone already.
fn end_quietly(pid: u32) {
  use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
  use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
  // SAFETY: only termination is asked for, nothing inherited; a pid already gone opens nothing.
  let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
  if process.is_null() {
    return;
  }
  // SAFETY: the unique owned handle OpenProcess just returned.
  let process = unsafe { OwnedHandle::from_raw_handle(process) };
  // SAFETY: `process` stays owned for the call; a refusal (the process ending meanwhile) leaves nothing to do.
  let _ = unsafe { TerminateProcess(process.as_raw_handle(), 1) };
}

/// A client of `instance`'s daemon, connected under the deadlines a client derives.
fn client_of(instance: &str) -> slates_client::Client {
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  slates_client::Client::connect(instance, deadlines).unwrap()
}

/// Bootstraps the node `client` reaches as its root, as `slates bootstrap root` does.
fn bootstrap_root(client: &mut slates_client::Client) {
  let member = client.daemon_status().unwrap().fleet.host;
  let reply = client
    .call(&slates_ipc::protocol::RequestBody::Bootstrap { root: true, member })
    .unwrap();
  assert_eq!(
    reply,
    slates_ipc::protocol::ReplyBody::Acknowledged,
    "bootstrap"
  );
}

/// A bounded scratch volume named `name` of `limit` bytes.
fn bounded(name: &str, limit: u64) -> slates_client::CreateSpec {
  slates_client::CreateSpec {
    name: name.to_owned(),
    size: slates_client::SizeClass::Bounded { limit },
    names: slates_client::NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

/// The count of `kind` over every shard of the daemon `client` reaches.
fn refusal_count(client: &mut slates_client::Client, kind: &str) -> u64 {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .flat_map(|shard| shard.refusals.iter())
    .filter(|counted| counted.kind == kind)
    .map(|counted| counted.count)
    .sum()
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
  let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
  assert_eq!(waited, WAIT_OBJECT_0);
}

/// A volume of `bounded` bytes made through the CLI and attached for writing: its id and the attachment.
/// A volume's write attachment held from the moment it is made by a `slates attach --hold` process (A-879), until the
/// write that names it has taken its own hold ([`write_over_held`]): no moment passes with nothing renewing the
/// attachment, however slowly the runner starts the write. A CLI attachment lapses about a second after its last use
/// here, and a write that began later met it lapsed ("could not be held ... AttachmentExpired", diag run 45289962818).
struct HeldAttachment {
  holder: Child,
  id: String,
  attachment: String,
}

/// Creates volume `name` of `bounded` bytes on `instance` and attaches it for writing with `slates attach --hold`,
/// whose stdout names the attachment; both of its outputs are drained as they come, so it never blocks or meets a
/// closed pipe, its stderr echoed.
fn held_writable_volume(instance: &str, name: &str, bounded: &str) -> HeldAttachment {
  let (code, out, err) = run(instance, &["volume", "create", name, "--bounded", bounded]);
  assert_eq!(code, 0, "create {name}: {err}");
  let id = value_of(&out, "id");
  let mut holder = slates()
    .args(["--instance", instance, "attach", &id, "--write", "--hold"])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(holder.stderr.take().unwrap());
  std::thread::spawn(move || {
    let mut line = String::new();
    while stderr.read_line(&mut line).is_ok_and(|read| read > 0) {
      eprint!("{line}");
      line.clear();
    }
  });
  let (lines, said) = std::sync::mpsc::channel::<String>();
  let mut stdout = std::io::BufReader::new(holder.stdout.take().unwrap());
  std::thread::spawn(move || {
    let mut line = String::new();
    while stdout.read_line(&mut line).is_ok_and(|read| read > 0) {
      let _ = lines.send(std::mem::take(&mut line));
    }
  });
  let attachment = loop {
    let line = said.recv_timeout(START_BOUND).unwrap_or_else(|_| {
      panic!("attach --hold for {name} named no attachment within {START_BOUND:?}")
    });
    if let Some(attachment) = line.trim_end().strip_prefix("attachment: ") {
      break attachment.to_owned();
    }
  };
  HeldAttachment {
    holder,
    id,
    attachment,
  }
}

/// The line `slates write` gives once its own hold of attachment `attachment` is taken, before it reads any input.
fn write_hold_line(attachment: &str) -> String {
  format!("slates write: holding attachment {attachment} ")
}

/// Writes `input` to `path` under `held`'s attachment: the write starts, `held`'s holder is ended once the write's
/// stderr says its own hold is taken (or the write has ended without one, or [`HOLD_WAIT`] passed), and only then is
/// the input fed, so the
/// staging rests on the write's hold alone. The exit code, the stdout and the whole stderr, a feed the write cut short
/// part of it.
fn write_over_held(
  instance: &str,
  held: HeldAttachment,
  path: &str,
  input: &[u8],
) -> (i32, Vec<u8>, String) {
  use std::io::{Read, Write};
  let HeldAttachment {
    mut holder,
    id,
    attachment,
  } = held;
  let mut child = slates()
    .args(["--instance", instance, "write", &id, &attachment, path])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let (lines, seen) = std::sync::mpsc::channel::<String>();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let reader = std::thread::spawn(move || {
    let mut line = String::new();
    while stderr.read_line(&mut line).is_ok_and(|read| read > 0) {
      let _ = lines.send(std::mem::take(&mut line));
    }
  });
  let hold_line = write_hold_line(&attachment);
  let mut err = String::new();
  while let Ok(line) = seen.recv_timeout(HOLD_WAIT) {
    // Anywhere in the line: a log line may carry a prefix of its own.
    let held_now = line.contains(&hold_line);
    err.push_str(&line);
    if held_now {
      break;
    }
  }
  // The write holds the attachment now, or has ended without a hold, or never said: either way the holder goes.
  let _ = holder.kill();
  let _ = holder.wait();
  let mut stdin = child.stdin.take().unwrap();
  let mut stdout = child.stdout.take().unwrap();
  let (fed, out) = std::thread::scope(|scope| {
    let feeder = scope.spawn(move || stdin.write_all(input));
    let mut out = Vec::new();
    let _ = stdout.read_to_end(&mut out);
    (feeder.join().unwrap(), out)
  });
  let status = child.wait().unwrap();
  let _ = reader.join();
  for line in seen.try_iter() {
    err.push_str(&line);
  }
  if let Err(error) = fed {
    err.push_str(&format!("\n(the input was not all read: {error})"));
  }
  (status.code().unwrap_or(-1), out, err)
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
  // Made before the attachment is, and the attachment held from its making until the write holds it
  // ([`held_writable_volume`]): the burst takes seconds to make in a debug build, and an attachment made before it,
  // with nothing renewing it, lapsed before its write began (A-879's named refusal, diag run 45289962818).
  let burst: Vec<u8> = (0..BURST)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let before = commit_charge();
  let held = held_writable_volume(&instance, "burst", "512MiB");
  let (id, attachment) = (held.id.clone(), held.attachment.clone());
  let (code, _, err) = write_over_held(&instance, held, "burst.bin", &burst);
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
  // Made before the attachment is, and the attachment held until the write holds it (the commit test's lesson).
  let bytes: Vec<u8> = (0..FILE)
    .map(|at| u8::try_from((at * 7 + at / 4096) % 251).unwrap())
    .collect();
  let held = held_writable_volume(&instance, "kept", "64MiB");
  let id = held.id.clone();
  let (code, _, err) = write_over_held(&instance, held, "kept.bin", &bytes);
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

/// Shape: the bounded volume the first-create test makes, the size the Node SDK suites' first create asks for.
const FIRST_CREATE: u64 = 8 << 20;

/// A-789 on Windows. Do: start a real anchor of one shard, as the Node SDK suites do, and the moment its daemon's
/// serving line is read connect a client, bootstrap the node and create a bounded volume. Expect: created. Until
/// 2026-11-23 the daemon served while the anchor was still taking its extents' holds, one exchange each, and a claim
/// takes only a held extent, so the create was refused `BudgetExceeded`, its `available` naming the whole pool
/// (5,722,144,768 bytes on CI's windows-latest, with 2 of 25 extents held; diag run 45238179939).
#[test]
fn a_create_the_moment_the_daemon_serves_is_admitted_on_windows() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!("skipping the Windows first-create flow: set SLATES_TEST_CLI=1 to run it");
    return;
  }
  let instance = format!("cli-extent-first-{}", std::process::id());
  let anchor = spawn_anchor(&instance, "1");
  let bounded_run = Bounded::over(&anchor, "the first-create test");
  let mut client = client_of(&instance);
  bootstrap_root(&mut client);
  let created = client.create(&bounded("first", FIRST_CREATE));
  drop(client);
  drop(bounded_run);
  drop(anchor);
  assert!(created.is_ok(), "the first create: {created:?}");
}

/// Shape: the share of the daemon's room the released volume takes, in quarters: three, so its extents hold more room
/// than the pool keeps ready while they are made again.
const RELEASED_QUARTERS: u64 = 3;

/// A-789 on Windows, the remake window (main 01a20182's ruling of 2026-11-23). Do: under a real anchor of one shard,
/// steady, learn the daemon's room from a create past it; create a volume of three quarters of that room and destroy
/// it; once its teardown has ended (the volume gone from the list), create a small volume, whose publication gives the
/// destroyed volume's extents back to the pool, each then closed and made again through two anchor exchanges; and at
/// once create the large size again. Expect: the large create admitted, having waited for the remakes
/// (`room.wait.parked`). Until 2026-11-23 the pool counted an extent being made again as free though no claim could
/// take it, and the create was refused `BudgetExceeded`. (A create sent at once after the destroy meets its teardown,
/// which holds the volume's reservation past the destroy's answer: item 3b's case, on every platform.)
#[test]
fn a_create_during_the_released_extents_remakes_waits_for_them_on_windows() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!("skipping the Windows remake-window flow: set SLATES_TEST_CLI=1 to run it");
    return;
  }
  let instance = format!("cli-extent-remake-{}", std::process::id());
  let anchor = start_anchor_of(&instance, "1");
  let bounded_run = Bounded::over(&anchor, "the remake-window test");
  let mut client = client_of(&instance);
  let room = match client.create(&bounded("past-the-room", u64::MAX >> 1)) {
    Err(slates_client::ClientError::Refused(slates_client::Refusal::BudgetExceeded {
      available,
    })) => available,
    other => panic!("a create past the daemon's room is refused BudgetExceeded: {other:?}"),
  };
  let size = room / 4 * RELEASED_QUARTERS;
  let released = client.create(&bounded("released", size)).unwrap();
  client.destroy(released).unwrap();
  let destroyed = Instant::now();
  while client
    .list()
    .unwrap()
    .iter()
    .any(|volume| volume.name == "released")
  {
    assert!(
      destroyed.elapsed() < START_BOUND,
      "the destroyed volume's teardown ended within {START_BOUND:?}"
    );
    pause();
  }
  client.create(&bounded("publishes", FIRST_CREATE)).unwrap();
  let again = client.create(&bounded("again", size));
  let parked = refusal_count(&mut client, "room.wait.parked");
  drop(client);
  drop(bounded_run);
  drop(anchor);
  assert!(
    again.is_ok(),
    "a create of {size} bytes while the destroyed volume's extents are made again ({room} bytes of room): {again:?}"
  );
  assert!(
    parked > 0,
    "the create waited for the released extents' remakes (room.wait.parked {parked})"
  );
}
