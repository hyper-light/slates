//! `slates shards` on Windows through the real command (§4.6, the Shards route; A-179, G-shards-windows step 2):
//! `slates anchor` supervising a real `slates daemon`, and `slates shards ID -- CMD ... {handle} ...` attaching a volume
//! in the Shards form and starting CMD with the stream end as a handle whose value replaces `{handle}`, as Shards'
//! `--slates-fs-handle {handle}` takes it. CMD is this test binary, re-invoked as a probe that plays Shards' fs device:
//! it adopts the handle it was named, mounts the attachment over it and reads a file. The probe learns its role from its
//! arguments, which libtest takes as extra name filters that match no test.
//!
//! The Unix form (`--fd N`, the command replacing `slates shards`) is `crates/cli/tests/cli.rs`'s, a file that is Unix
//! as a whole. Windows only, and gated on `SLATES_TEST_CLI=1` (the runner to itself); without it each test skips loudly
//! and passes.
#![cfg(windows)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

#[path = "support/process_wait.rs"]
mod process_wait;

use std::io::{BufRead, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};

/// Shape: shards for the test daemon.
const SHARDS: &str = "2";
/// Shape: the pause between polls of a change or a start.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answers to `volume list` a start waits for, so the anchor's one startup restart has settled.
const STABLE_STREAK: u32 = 10;
/// Shape: the most `volume list` attempts a start makes after the daemon's serving line (`crates/cli/tests/cli.rs`
/// `START_ATTEMPTS` says why attempts, not wall time).
const START_ATTEMPTS: u32 = 60;
/// Format: the start of the line the daemon prints once it serves (`crates/api/src/role/daemon.rs`).
const SERVING_LINE: &str = "slates daemon: instance ";
/// Format: this test's name, which the re-invoked binary runs as the probe.
const TEST: &str = "slates_shards_hands_a_program_a_working_handle_and_the_attachment_ends_with_it";
/// Format: the probe's roles, each an argument: mount and read the file; exit without a word; close the end, then
/// wait on standard input; print its pid, then hold the end until ended.
const READER: &str = "role=reader";
/// Format: see [`READER`].
const SILENT: &str = "role=silent";
/// Format: see [`READER`].
const CLOSER: &str = "role=closer";
/// Format: see [`READER`]: the holder parks for good, so only its command's job can end it.
const HOLDER: &str = "role=holder";
/// Format: the daemon's counter of processes `ShardsHolder` added as a stream's known holders
/// (`crates/server/src/shards.rs`).
const HOLDER_ADDED: &str = "shards.holder_added";
/// Format: the placeholder `slates shards` replaces with the handle's value.
const PLACEHOLDER: &str = "{handle}";
/// Format: the tag the reader puts before the bytes it read, in hex: the re-invoked test binary prints libtest's banner
/// on standard output first.
const READ_TAG: &str = "shards-read: ";
/// Format: the tag before the holder's pid.
const HOLDER_PID_TAG: &str = "shards-holder-pid: ";
/// Format: the line the closer prints once it has closed its end.
const CLOSED_LINE: &str = "end-closed";
/// Shape: the silent probe's exit code, not zero, so the command is seen to exit with the program's own.
const SILENT_EXIT: i32 = 7;
/// The file the reader reads through its end; long enough to be more than a header's worth.
const SHARDS_FILE: &[u8] = b"read through the handle slates shards handed this program\n";
/// Format: the FUSE protocol a guest kernel offers (7.31), the node id of the attachment's root, and `O_RDONLY`.
const FUSE_MAJOR: u32 = 7;
/// Format: see [`FUSE_MAJOR`].
const FUSE_MINOR: u32 = 31;
/// Format: see [`FUSE_MAJOR`].
const ROOT_NODE: u64 = 1;
/// Format: see [`FUSE_MAJOR`].
const READ_ONLY: u32 = 0;
/// Shape: the read-ahead the reader offers at INIT (128 KiB, a Linux guest's default), and the bytes one READ asks for
/// (a page, more than the file).
const READAHEAD: u32 = 128 << 10;
/// Shape: see [`READAHEAD`].
const READ_BYTES: u32 = 4096;

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a client verb; (exit code, stdout, stderr).
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

/// Runs a client verb with `input` on its standard input; (exit code, stderr).
fn run_with_stdin(instance: &str, args: &[&str], input: &[u8]) -> (i32, String) {
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
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// The value of `key: value` in a verb's text output.
fn value_of(text: &str, key: &str) -> String {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .to_owned()
}

/// The anchor process, killed and reaped on drop, so a failed assertion leaves no daemon behind (the daemon leaves
/// when its anchor dies).
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

/// Starts `slates --instance INSTANCE anchor --quick --shards SHARDS`, waits for its daemon to serve and answer a
/// streak of verbs, and bootstraps its consensus group.
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
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  });
  let anchor = AnchorProcess {
    child,
    drain: Some(drain),
  };
  let mut streak = 0u32;
  for _ in 0..START_ATTEMPTS {
    let (code, _, _) = run(instance, &["volume", "list"]);
    streak = if code == 0 { streak + 1 } else { 0 };
    if streak >= STABLE_STREAK {
      let (code, _, error) = run(instance, &["bootstrap", "root"]);
      assert_eq!(code, 0, "explicit first-time bootstrap: {error}");
      return anchor;
    }
    pause();
  }
  panic!(
    "the serving daemon answered {STABLE_STREAK} verbs in a row within {START_ATTEMPTS} attempts"
  );
}

/// The volume's attachment count, as `status ID` prints it.
fn attachments_of(instance: &str, id: &str) -> String {
  let (code, out, err) = run(instance, &["status", id]);
  assert_eq!(code, 0, "{err}");
  value_of(&out, "attachments")
}

/// Polls `holds` at the test's pace while `instance`'s daemon gains CPU (`process_wait::Judged` on the pid its `status`
/// names: Windows lists no instance's processes by command line, and a live daemon's watchdog keeps it moving, so it
/// is waited on while it lives, A-232); whether it held. Until 2026-11-20 within `WAIT_FOR`, ten liveness budgets.
fn wait_for(instance: &str, mut holds: impl FnMut() -> bool) -> bool {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "status: {err}");
  let mut judged = process_wait::Judged::of(value_of(&out, "pid").parse().unwrap());
  loop {
    if holds() {
      return true;
    }
    if !matches!(judged.judge(), process_wait::Verdict::Waiting { .. }) {
      return holds();
    }
    pause();
  }
}

/// The attachment `attachment` is gone: the volume's count back to `before` within the wait, and a `detach` of it
/// refused.
fn the_attachment_ends(instance: &str, id: &str, before: &str, attachment: &str) {
  assert!(
    wait_for(instance, || attachments_of(instance, id) == before),
    "attachment {attachment} ended with its program's end: {} attachments, {before} before",
    attachments_of(instance, id)
  );
  let (code, _, err) = run(instance, &["detach", attachment]);
  assert_eq!(
    code, 1,
    "no attachment {attachment} remains to detach: {err}"
  );
}

/// A child killed and reaped when dropped, so a failed assertion leaves no process behind.
struct Running(Child);

impl Drop for Running {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

/// `slates --instance INSTANCE shards ID -- <this binary> --exact TEST --nocapture ROLE {handle}`.
fn shards_probe(instance: &str, id: &str, role: &str) -> Command {
  let mut command = slates();
  command
    .args(["--instance", instance, "shards", id, "--"])
    .arg(std::env::current_exe().unwrap())
    .args(["--exact", TEST, "--nocapture", role, PLACEHOLDER]);
  command
}

/// G-shards-windows step 2 (A-179): "the child named after `--` receives a working stream handle, and the attachment
/// ends when the child exits; no other child the CLI starts inherits the handle". Do: write a file into a volume; run
/// `slates shards ID -- <this binary> ... {handle}`, the probe mounting the attachment over the handle it was named
/// (INIT, LOOKUP, OPEN, READ, RELEASE) and printing what it read; run a probe that exits 7 without a word; a probe that
/// closes its handle and then waits while `slates shards` waits for it; a probe that holds its handle while the test
/// kills `slates shards`; then the refusals. Expect the bytes equal to the file and the attachment gone after the
/// probe's exit, and the daemon's count of holders added moved once per run, so the command named its program as a holder
/// before letting go; the command's exit code 7; the attachment gone while the closer and the command still run, so no
/// other copy of the end exists (not the command's own, not another child's); the attachment gone once the command is
/// killed, its job taking the holder along; a missing volume refused (exit 1), a program that cannot run a failure
/// (exit 4) leaving no attachment, and `--fd` or a command without `{handle}` usage (exit 2).
#[test]
fn slates_shards_hands_a_program_a_working_handle_and_the_attachment_ends_with_it() {
  let roles: Vec<String> = std::env::args().collect();
  if let Some(role) = roles.iter().find(|argument| argument.starts_with("role=")) {
    probe(role);
  }
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "SKIP: the shards hand-off needs SLATES_TEST_CLI=1 (an anchor and its daemon on this runner)"
    );
    return;
  }
  let instance = format!("cs{}", std::process::id());
  let _anchor = start_anchor(&instance);
  let id = a_volume_with_the_file(&instance);
  let before = attachments_of(&instance, &id);
  a_reader_reads_the_file_through_its_handle(&instance, &id, &before);
  a_silent_program_exits_and_ends_its_attachment(&instance, &id, &before);
  the_attachment_ends_when_the_handle_closes_while_the_command_runs(&instance, &id, &before);
  a_killed_command_takes_its_program_and_the_attachment_along(&instance, &id, &before);
  the_refusals_leave_no_attachment(&instance, &id, &before);
}

/// A volume holding `hello.txt` with [`SHARDS_FILE`], written through an attachment that is then detached.
fn a_volume_with_the_file(instance: &str) -> String {
  let (code, out, err) = run(
    instance,
    &["volume", "create", "shards", "--bounded", "4MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let (code, attached, err) = run(instance, &["attach", &id, "--write"]);
  assert_eq!(code, 0, "{err}");
  let writer = value_of(&attached, "attachment");
  let (code, err) = run_with_stdin(instance, &["write", &id, &writer, "hello.txt"], SHARDS_FILE);
  assert_eq!(code, 0, "{err}");
  let (code, _, err) = run(instance, &["detach", &writer]);
  assert_eq!(code, 0, "{err}");
  id
}

/// The reader reads the file's bytes through its handle; then its attachment is gone.
fn a_reader_reads_the_file_through_its_handle(instance: &str, id: &str, before: &str) {
  let added = counter(instance, HOLDER_ADDED);
  let reader = shards_probe(instance, id, READER).output().unwrap();
  let stdout = String::from_utf8_lossy(&reader.stdout);
  assert_eq!(
    reader.status.code(),
    Some(0),
    "{stdout}{}",
    String::from_utf8_lossy(&reader.stderr)
  );
  let read = stdout
    .lines()
    .find_map(|line| line.split_once(READ_TAG).map(|(_, read)| read))
    .unwrap_or_else(|| panic!("the reader printed what it read: {stdout}"));
  assert_eq!(
    unhex(read),
    SHARDS_FILE,
    "the bytes read through the handle are the file's"
  );
  the_attachment_ends(instance, id, before, &value_of(&stdout, "attachment"));
  assert_eq!(
    counter(instance, HOLDER_ADDED),
    added + 1,
    "the command named its program as the stream's holder, judged against its own copy, before letting go"
  );
}

/// The daemon's status counter `name`, summed over its shards (`slates status` prints `name: count` per shard).
fn counter(instance: &str, name: &str) -> u64 {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "slates status: {err}");
  let named = format!("{name}: ");
  out
    .lines()
    .filter_map(|line| line.split_once(named.as_str()))
    .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
    .sum()
}

/// A program that never speaks on its end and exits 7: the command exits 7, and the attachment ends.
fn a_silent_program_exits_and_ends_its_attachment(instance: &str, id: &str, before: &str) {
  let silent = shards_probe(instance, id, SILENT).output().unwrap();
  assert_eq!(
    silent.status.code(),
    Some(SILENT_EXIT),
    "the command's exit code is the program's"
  );
  the_attachment_ends(
    instance,
    id,
    before,
    &value_of(&String::from_utf8_lossy(&silent.stdout), "attachment"),
  );
}

/// Reads `stdout` line by line until a line holding `tag`; what came before (the announcement), and that line.
fn read_until(
  lines: &mut impl Iterator<Item = std::io::Result<String>>,
  tag: &str,
) -> (String, String) {
  let mut before = String::new();
  for line in lines {
    let line = line.unwrap();
    if line.contains(tag) {
      return (before, line);
    }
    before.push_str(&line);
    before.push('\n');
  }
  panic!("the probe never printed {tag:?}: {before}");
}

/// A program that closes its handle and then waits on its standard input: its attachment is gone while it, and the
/// `slates shards` waiting for it, still run, so nothing but the program's handle held the attachment: not a copy the
/// command kept (it let go once the daemon took the program as a holder), not one another child inherited. The
/// command then exits with the program.
fn the_attachment_ends_when_the_handle_closes_while_the_command_runs(
  instance: &str,
  id: &str,
  before: &str,
) {
  let mut running = Running(
    shards_probe(instance, id, CLOSER)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  let mut lines = std::io::BufReader::new(running.0.stdout.take().unwrap()).lines();
  let (announced, _) = read_until(&mut lines, CLOSED_LINE);
  let attachment = value_of(&announced, "attachment");
  assert!(
    wait_for(instance, || attachments_of(instance, id) == before),
    "attachment {attachment} ended once the program closed its handle"
  );
  assert!(
    running.0.try_wait().unwrap().is_none(),
    "`slates shards` still waits for its program"
  );
  drop(running.0.stdin.take());
  assert_eq!(running.0.wait().unwrap().code(), Some(0));
  let (code, _, err) = run(instance, &["detach", &attachment]);
  assert_eq!(
    code, 1,
    "no attachment {attachment} remains to detach: {err}"
  );
}

/// Chaos, the way a supervisor ends a run: killing `slates shards` (`TerminateProcess`) while its program holds the
/// handle ends the program too, through the job only the command held, and so the attachment. On Unix the command is
/// the program; here they are two processes, and the job is what makes them end as one.
fn a_killed_command_takes_its_program_and_the_attachment_along(
  instance: &str,
  id: &str,
  before: &str,
) {
  let mut launch = Running(
    shards_probe(instance, id, HOLDER)
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  let mut lines = std::io::BufReader::new(launch.0.stdout.take().unwrap()).lines();
  let (announced, line) = read_until(&mut lines, HOLDER_PID_TAG);
  let attachment = value_of(&announced, "attachment");
  let holder: u32 = line
    .split_once(HOLDER_PID_TAG)
    .unwrap()
    .1
    .trim()
    .parse()
    .unwrap();
  assert_ne!(
    holder,
    launch.0.id(),
    "the program is a child of `slates shards`"
  );
  assert_ne!(
    attachments_of(instance, id),
    before,
    "the holder's attachment {attachment} is live while it holds the handle"
  );
  launch.0.kill().unwrap();
  launch.0.wait().unwrap();
  the_attachment_ends(instance, id, before, &attachment);
}

/// A volume that does not exist is refused (exit 1); a program that cannot run is a failure (exit 4) whose attachment
/// ends with the handle it never received; a descriptor number, or a command that never names `{handle}`, is usage
/// (exit 2).
fn the_refusals_leave_no_attachment(instance: &str, id: &str, before: &str) {
  let missing = "0".repeat(32);
  let (code, _, err) = run(
    instance,
    &["shards", &missing, "--", "cmd", "/c", "exit 0", PLACEHOLDER],
  );
  assert_eq!(code, 1, "a volume that does not exist is refused: {err}");
  let (code, _, err) = run(
    instance,
    &[
      "shards",
      id,
      "--",
      r"C:\nonexistent\slates-shards-program.exe",
      PLACEHOLDER,
    ],
  );
  assert_eq!(code, 4, "a program that cannot run is a failure: {err}");
  assert!(
    wait_for(instance, || attachments_of(instance, id) == before),
    "the handle of a program that never ran closed, ending its attachment"
  );
  let (code, _, err) = run(
    instance,
    &["shards", id, "--fd", "5", "--", "cmd", PLACEHOLDER],
  );
  assert_eq!(code, 2, "Windows takes no descriptor number: {err}");
  let (code, _, err) = run(instance, &["shards", id, "--", "cmd", "/c", "exit 0"]);
  assert_eq!(
    code, 2,
    "a command that never names its handle is usage: {err}"
  );
  assert_eq!(attachments_of(instance, id), before);
}

fn unhex(text: &str) -> Vec<u8> {
  (0..text.len())
    .step_by(2)
    .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
    .collect()
}

/// The program `slates shards` runs in the test above: this test binary, re-invoked in `role`, the handle's value its
/// last argument. Never returns.
fn probe(role: &str) -> ! {
  let value: u64 = std::env::args().next_back().unwrap().parse().unwrap();
  let end = slates_ipc::handle_route::adopt_stream(value).unwrap();
  match role {
    READER => {
      let mut device = ShardsDevice { end, unique: 0 };
      let bytes = device.mount_and_read("hello.txt");
      let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
      println!("{READ_TAG}{hex}");
      std::process::exit(0);
    }
    SILENT => std::process::exit(SILENT_EXIT),
    CLOSER => {
      drop(end);
      println!("{CLOSED_LINE}");
      std::io::stdout().flush().unwrap();
      let mut held = String::new();
      std::io::stdin().read_line(&mut held).unwrap();
      std::process::exit(0);
    }
    HOLDER => {
      println!("{HOLDER_PID_TAG}{}", std::process::id());
      std::io::stdout().flush().unwrap();
      // Holds the end until something ends this process: nothing it reads can end it, so only the command's job can.
      loop {
        std::thread::park();
        eprintln!("shards-holder: woken, still holding");
      }
    }
    other => panic!("no probe role {other}"),
  }
}

/// Shards' fs device as the reader plays it: each request a little-endian length, then the FUSE request; one reply
/// frame back.
struct ShardsDevice {
  end: OwnedHandle,
  unique: u64,
}

impl ShardsDevice {
  fn mount_and_read(&mut self, path: &str) -> Vec<u8> {
    self.init();
    self.read_file(path)
  }

  /// The mount's `INIT`.
  fn init(&mut self) {
    let mut init = Vec::new();
    for word in [FUSE_MAJOR, FUSE_MINOR, READAHEAD, 0] {
      init.extend_from_slice(&word.to_le_bytes());
    }
    assert_eq!(errno(&self.ask(Opcode::Init, 0, &init)), 0, "INIT");
  }

  /// `path`'s node under the root.
  fn lookup(&mut self, path: &str) -> u64 {
    let mut name = path.as_bytes().to_vec();
    name.push(0);
    let found = self.ask(Opcode::Lookup, ROOT_NODE, &name);
    assert_eq!(errno(&found), 0, "LOOKUP {path}");
    word_at(&found, OUT_HEADER_LEN)
  }

  /// A Windows DESCRIPTOR ask for `node`'s first range, writable, this process claiming the stream by its own handle of
  /// it (`docs/wip/shards/dax-protocol.md`, "Windows"): the section the daemon duplicates into this process.
  fn section(&mut self, node: u64) -> OwnedHandle {
    use slates_bridge_fuse::shards_fs::{ASK_DESCRIPTOR, CONTROL_FRAME};
    let mut ask = vec![ASK_DESCRIPTOR];
    ask.extend_from_slice(&node.to_le_bytes());
    ask.push(1);
    ask.extend_from_slice(&0u64.to_le_bytes());
    ask.extend_from_slice(&DAX_RANGE.to_le_bytes());
    ask.extend_from_slice(&std::process::id().to_le_bytes());
    let stream = u64::try_from(self.end.as_raw_handle().expose_provenance()).unwrap();
    ask.extend_from_slice(&stream.to_le_bytes());
    let word = u32::try_from(ask.len()).unwrap() | CONTROL_FRAME;
    self.write_all(&word.to_le_bytes());
    self.write_all(&ask);
    let mut header = [0u8; 5];
    self.read_exact(&mut header);
    let mut payload =
      vec![0u8; usize::try_from(u32::from_be_bytes(header[1..5].try_into().unwrap())).unwrap()];
    self.read_exact(&mut payload);
    assert_eq!(
      header[0],
      0,
      "the DAX ask answered with errno {}",
      i32::from_le_bytes(payload[..4].try_into().unwrap())
    );
    let value = u64::from_le_bytes(payload.try_into().unwrap());
    // SAFETY: the value the daemon duplicated into this process for this ask, taken as this probe's own.
    unsafe {
      OwnedHandle::from_raw_handle(std::ptr::with_exposed_provenance_mut(
        usize::try_from(value).unwrap(),
      ))
    }
  }

  /// LOOKUP, OPEN, READ and RELEASE of `path` under the root: its first bytes.
  fn read_file(&mut self, path: &str) -> Vec<u8> {
    let mut name = path.as_bytes().to_vec();
    name.push(0);
    let found = self.ask(Opcode::Lookup, ROOT_NODE, &name);
    assert_eq!(errno(&found), 0, "LOOKUP {path}");
    let node = word_at(&found, OUT_HEADER_LEN);
    let mut open = READ_ONLY.to_le_bytes().to_vec();
    open.extend_from_slice(&0u32.to_le_bytes());
    let opened = self.ask(Opcode::Open, node, &open);
    assert_eq!(errno(&opened), 0, "OPEN {path}");
    let handle = word_at(&opened, OUT_HEADER_LEN);
    let mut read = handle.to_le_bytes().to_vec();
    read.extend_from_slice(&0u64.to_le_bytes());
    read.extend_from_slice(&READ_BYTES.to_le_bytes());
    read.extend_from_slice(&[0u8; 20]);
    let reply = self.ask(Opcode::Read, node, &read);
    assert_eq!(errno(&reply), 0, "READ {path}");
    let mut release = handle.to_le_bytes().to_vec();
    release.extend_from_slice(&[0u8; 16]);
    assert_eq!(
      errno(&self.ask(Opcode::Release, node, &release)),
      0,
      "RELEASE"
    );
    reply[OUT_HEADER_LEN..].to_vec()
  }

  fn ask(&mut self, opcode: Opcode, node: u64, body: &[u8]) -> Vec<u8> {
    self.unique += 1;
    let total = IN_HEADER_LEN + body.len();
    let length = u32::try_from(total).unwrap().to_le_bytes();
    let mut request = vec![0u8; total];
    request[0..4].copy_from_slice(&length);
    request[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
    request[8..16].copy_from_slice(&self.unique.to_le_bytes());
    request[16..24].copy_from_slice(&node.to_le_bytes());
    request[IN_HEADER_LEN..].copy_from_slice(body);
    self.write_all(&length);
    self.write_all(&request);
    let mut word = [0u8; 4];
    self.read_exact(&mut word);
    let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(word)).unwrap()];
    self.read_exact(&mut reply);
    reply
  }

  fn write_all(&self, mut bytes: &[u8]) {
    while !bytes.is_empty() {
      let wrote = slates_ipc::stream::write_waiting(self.end.as_handle(), bytes).unwrap();
      assert!(wrote > 0, "the daemon's end took nothing");
      bytes = &bytes[wrote..];
    }
  }

  fn read_exact(&self, mut into: &mut [u8]) {
    while !into.is_empty() {
      let read = slates_ipc::stream::read_waiting(self.end.as_handle(), into).unwrap();
      assert!(read > 0, "the daemon's end closed mid-frame");
      into = &mut into[read..];
    }
  }
}

fn errno(reply: &[u8]) -> i32 {
  -i32::from_le_bytes(reply[4..8].try_into().unwrap())
}

fn word_at(bytes: &[u8], at: usize) -> u64 {
  u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Format: the role of the probe that is resynced across its daemon's kill (A-433).
const RESYNCER: &str = "role=resyncer";
/// Format: the test the resyncer probe is re-invoked as.
const RESYNC_TEST: &str = "slates_shards_resyncs_its_programs_stream_after_its_daemon_is_killed";
/// Format: where the command's arguments name the resync end's handle (`slates shards`' `{resync}`, A-433).
const RESYNC_PLACEHOLDER: &str = "{resync}";
/// Format: the line the resyncer prints once it has read the file through its first stream and its channel's
/// `Attached`.
const READY_LINE: &str = "shards-resync-ready";

/// A-433, the Windows stream's resync across a real daemon's death (§4.6, A-316's channel on Windows). Do: under
/// `slates anchor`, run `slates shards ID -- <this binary> ... {handle} {resync}`, the probe reading the file through
/// its stream after the channel's `Attached`; kill the daemon (`TerminateProcess`, as `kill -9`); let the anchor start
/// the next. Expect: the probe reads a Windows `Resync` naming its attachment and token, its own process as the holder
/// and a live handle there, and reads the file's bytes through that fresh stream with no `INIT`; the command exits 0;
/// the next daemon counts `shards.resynced`. Red before A-433: the next daemon released the held end and ended the
/// attachment, so the probe read the channel's end and failed.
#[test]
fn slates_shards_resyncs_its_programs_stream_after_its_daemon_is_killed() {
  if std::env::args().any(|argument| argument == RESYNCER) {
    resyncer();
  }
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "SKIP: the shards resync needs SLATES_TEST_CLI=1 (an anchor and its daemon on this runner)"
    );
    return;
  }
  let instance = format!("cr{}", std::process::id());
  let (read, code, _anchor) = read_across_a_kill(&instance, RESYNC_TEST, RESYNCER);
  assert_eq!(read, SHARDS_FILE);
  assert_eq!(code, Some(0), "the command's exit");
  assert!(
    counter(&instance, "shards.resynced") >= 1,
    "the next daemon resynced the stream"
  );
}

/// Runs `slates shards` under a fresh anchor of `instance` over a volume holding the file, the command this test binary
/// re-invoked as `test` in `role` with its stream and resync ends as its last two arguments; once the probe says it is
/// ready, kills the daemon (`TerminateProcess`, as `kill -9`) and waits, while the probe lives, for the bytes the
/// probe reads through its resynced stream. The bytes, the command's exit, and the anchor's guard, so the caller reads
/// the next daemon's counters before it goes.
fn read_across_a_kill(
  instance: &str,
  test: &str,
  role: &str,
) -> (Vec<u8>, Option<i32>, AnchorProcess) {
  let anchor = start_anchor(instance);
  let id = a_volume_with_the_file(instance);
  let mut command = slates();
  let mut child = command
    .args(["--instance", instance, "shards", &id, "--"])
    .arg(std::env::current_exe().unwrap())
    .args([
      "--exact",
      test,
      "--nocapture",
      role,
      PLACEHOLDER,
      RESYNC_PLACEHOLDER,
    ])
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
  let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
  let mut line = String::new();
  loop {
    line.clear();
    assert!(
      out.read_line(&mut line).unwrap() > 0,
      "the probe ended before it was ready"
    );
    if line.contains(READY_LINE) {
      break;
    }
  }
  let (code, status, error) = run(instance, &["status"]);
  assert_eq!(code, 0, "status: {error}");
  kill_hard(value_of(&status, "pid").parse().unwrap());
  // Each line is read without a timeout: the probe writes while it lives, and its end closes its output, which ends the
  // read. Until 2026-11-20 each line was also checked against a 120 s `RESYNC_WAIT`, only after it had come.
  let mut read = None;
  while read.is_none() {
    line.clear();
    assert!(
      out.read_line(&mut line).unwrap() > 0,
      "the probe ended without reading through a resynced stream"
    );
    read = line
      .find(READ_TAG)
      .map(|at| unhex(line[at + READ_TAG.len()..].trim()));
  }
  (
    read.unwrap_or_default(),
    child.wait().unwrap().code(),
    anchor,
  )
}

/// The resyncer probe (A-433): its stream and resync ends are its last two arguments. Reads `Attached`, the file through
/// the stream, says it is ready, then waits on the channel for a `Resync`, takes the handle it names in this process,
/// and reads the file again through it, with no `INIT`. Never returns.
fn resyncer() -> ! {
  use slates_bridge_fuse::shards_resync::{
    HandedResync, RESYNC_HANDLE_MESSAGE_BYTES, ResyncKind, ResyncMessage,
  };
  let values: Vec<u64> = std::env::args()
    .rev()
    .take(2)
    .map(|value| value.parse().unwrap())
    .collect();
  let (resync, stream) = (values[0], values[1]);
  let channel = slates_ipc::handle_route::adopt_stream(resync).unwrap();
  let end = slates_ipc::handle_route::adopt_stream(stream).unwrap();
  let mut message = [0u8; RESYNC_HANDLE_MESSAGE_BYTES];
  let read = slates_ipc::stream::read_waiting(channel.as_handle(), &mut message).unwrap();
  let attached = ResyncMessage::parse(&message[..read]).unwrap();
  assert_eq!(attached.kind, ResyncKind::Attached);
  let mut device = ShardsDevice { end, unique: 0 };
  assert_eq!(device.mount_and_read("hello.txt"), SHARDS_FILE);
  println!("{READY_LINE}");
  std::io::stdout().flush().unwrap();
  let read = slates_ipc::stream::read_waiting(channel.as_handle(), &mut message).unwrap();
  let resync = HandedResync::parse(&message[..read]).unwrap();
  assert!(
    resync.resyncs(&attached),
    "the resync names its attachment and token"
  );
  assert_eq!(
    resync.holder,
    std::process::id(),
    "this process holds the fresh end"
  );
  let fresh = slates_ipc::handle_route::adopt_stream(resync.handle).unwrap();
  let mut device = ShardsDevice {
    end: fresh,
    unique: device.unique,
  };
  let bytes = device.read_file("hello.txt");
  let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
  println!("{READ_TAG}{hex}");
  std::io::stdout().flush().unwrap();
  std::process::exit(0);
}

/// Format: the role of the probe whose DAX section outlives its daemon's kill (A-448).
const DAX_RESYNCER: &str = "role=dax-resyncer";
/// Format: the test the DAX probe is re-invoked as.
const DAX_RESYNC_TEST: &str =
  "slates_shards_keeps_its_programs_dax_section_across_its_daemons_kill";
/// Format: a DAX range, 2 MiB (`FUSE_DAX_SZ`, slates' `DAX_RANGE_BYTES`), the span the probe's ask maps.
const DAX_RANGE: u64 = 2 << 20;
/// Format: what the probe stores through its mapped section, over the file's first bytes, and never syncs.
const DAX_STORED: &[u8] = b"stored through the section";

/// A-448, a Windows stream's DAX section across a real daemon's death (§4.6, A-365's object on Windows). Do: under
/// `slates anchor`, run `slates shards ID -- <this binary> ... {handle} {resync}`, the probe mapping a writable section
/// of the file and storing over its first bytes without a sync; kill the daemon; let the anchor start the next. Expect:
/// the probe reads its `Resync`, asks for the file's section again through the fresh stream and is handed the very
/// section it maps (`CompareObjectHandles`), and reads the file through that stream with its store in it; the command
/// exits 0; the next daemon counts `shards.resynced` and `shards.dax_adopted`. Red before A-448: the first map gave the
/// stream's resync up, so the probe read its channel's end and failed; and a fresh ask after a `Resync` was refused
/// `EACCES`, the holder's fresh stream being no known holder's.
#[test]
fn slates_shards_keeps_its_programs_dax_section_across_its_daemons_kill() {
  if std::env::args().any(|argument| argument == DAX_RESYNCER) {
    dax_resyncer();
  }
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "SKIP: the shards DAX resync needs SLATES_TEST_CLI=1 (an anchor and its daemon on this runner)"
    );
    return;
  }
  let instance = format!("cd{}", std::process::id());
  let (read, code, _anchor) = read_across_a_kill(&instance, DAX_RESYNC_TEST, DAX_RESYNCER);
  let mut expected = DAX_STORED.to_vec();
  expected.extend_from_slice(&SHARDS_FILE[DAX_STORED.len()..]);
  assert_eq!(
    read, expected,
    "the store through the section reached the file"
  );
  assert_eq!(code, Some(0), "the command's exit");
  assert!(
    counter(&instance, "shards.resynced") >= 1,
    "the next daemon resynced the stream"
  );
  assert!(
    counter(&instance, "shards.dax_adopted") >= 1,
    "the next daemon took the section back"
  );
}

/// The DAX probe (A-448): its stream and resync ends are its last two arguments. Reads `Attached`, mounts, maps a
/// writable section of the file and stores over its first bytes through it with no sync, says it is ready, then waits on
/// the channel for a `Resync`, takes the fresh stream, asks for the file's section again through it, checks it is the
/// section it maps, and reads the file through the fresh stream. Never returns.
fn dax_resyncer() -> ! {
  use slates_bridge_fuse::shards_resync::{
    HandedResync, RESYNC_HANDLE_MESSAGE_BYTES, ResyncKind, ResyncMessage,
  };
  let values: Vec<u64> = std::env::args()
    .rev()
    .take(2)
    .map(|value| value.parse().unwrap())
    .collect();
  let (resync, stream) = (values[0], values[1]);
  let channel = slates_ipc::handle_route::adopt_stream(resync).unwrap();
  let end = slates_ipc::handle_route::adopt_stream(stream).unwrap();
  let mut message = [0u8; RESYNC_HANDLE_MESSAGE_BYTES];
  let read = slates_ipc::stream::read_waiting(channel.as_handle(), &mut message).unwrap();
  let attached = ResyncMessage::parse(&message[..read]).unwrap();
  assert_eq!(attached.kind, ResyncKind::Attached);
  let mut device = ShardsDevice { end, unique: 0 };
  device.init();
  let node = device.lookup("hello.txt");
  let section = device.section(node);
  let view = SectionView::map(&section);
  view.store(0, DAX_STORED);
  println!("{READY_LINE}");
  std::io::stdout().flush().unwrap();
  let read = slates_ipc::stream::read_waiting(channel.as_handle(), &mut message).unwrap();
  let resync = HandedResync::parse(&message[..read]).unwrap();
  assert!(
    resync.resyncs(&attached),
    "the resync names its attachment and token"
  );
  let fresh = slates_ipc::handle_route::adopt_stream(resync.handle).unwrap();
  let mut device = ShardsDevice {
    end: fresh,
    unique: device.unique,
  };
  let again = device.section(node);
  // SAFETY: two live section handles of this process; the call compares the objects they name and touches nothing.
  let same = unsafe {
    windows_sys::Win32::Foundation::CompareObjectHandles(
      section.as_raw_handle(),
      again.as_raw_handle(),
    )
  };
  assert_ne!(same, 0, "the successor hands the section the anchor kept");
  let bytes = device.read_file("hello.txt");
  let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
  println!("{READ_TAG}{hex}");
  std::io::stdout().flush().unwrap();
  drop(view);
  std::process::exit(0);
}

/// A writable view of a section's first DAX range, as a VMM's window maps one; unmapped on drop.
struct SectionView(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS);

impl SectionView {
  fn map(section: &OwnedHandle) -> SectionView {
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile};
    // SAFETY: a view at an address the system chooses of a live section handle this process holds; a refused view is
    // null, asserted below.
    let view = unsafe {
      MapViewOfFile(
        section.as_raw_handle(),
        FILE_MAP_READ | FILE_MAP_WRITE,
        0,
        0,
        usize::try_from(DAX_RANGE).unwrap(),
      )
    };
    assert!(!view.Value.is_null(), "the probe maps its section");
    SectionView(view)
  }

  /// Stores `bytes` at `at` through the view, a byte at a time through atomics: the daemon reads and writes the same
  /// pages from its own process.
  fn store(&self, at: usize, bytes: &[u8]) {
    for (offset, byte) in bytes.iter().enumerate() {
      assert!(at + offset < usize::try_from(DAX_RANGE).unwrap());
      // SAFETY: inside this live view, which outlives the borrow; an `AtomicU8` has no alignment need.
      let cell = unsafe {
        &*self
          .0
          .Value
          .cast::<u8>()
          .add(at + offset)
          .cast::<std::sync::atomic::AtomicU8>()
      };
      cell.store(*byte, std::sync::atomic::Ordering::Relaxed);
    }
  }
}

impl Drop for SectionView {
  fn drop(&mut self) {
    // SAFETY: the view this value mapped, unmapped once.
    unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.0) };
  }
}

/// Ends process `pid` at once, as `kill -9` does (`TerminateProcess`).
fn kill_hard(pid: u32) {
  use windows_sys::Win32::Foundation::CloseHandle;
  use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
  // SAFETY: the pid came from the daemon's own status; only the right to end it is asked, with no inheritance.
  let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
  assert!(!process.is_null(), "OpenProcess({pid})");
  // SAFETY: the live handle just opened, used once and closed here.
  let ended = unsafe { TerminateProcess(process, 1) };
  // SAFETY: the same handle, closed once.
  unsafe { CloseHandle(process) };
  assert_ne!(ended, 0, "kill the daemon");
}
