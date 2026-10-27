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

use std::io::{BufRead, Write};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};

/// Shape: shards for the test daemon.
const SHARDS: &str = "2";
/// Shape: how long an attachment gets to end after its end closes: ten liveness budgets (`LIVENESS_BUDGET_NS`, 1 s),
/// as the Unix CLI tests' `WAIT_FOR`.
const WAIT_FOR: Duration = Duration::from_secs(10);
/// Shape: the pause between polls of a change or a start.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answers to `volume list` a start waits for, so the anchor's one startup restart has settled.
const STABLE_STREAK: u32 = 10;
/// Shape: the most `volume list` attempts a start makes after the daemon's serving line (`crates/cli/tests/cli.rs`
/// `START_ATTEMPTS` says why attempts, not wall time).
const START_ATTEMPTS: u32 = 60;
/// Format: the start of the line the daemon prints once it serves (`crates/cli/src/daemon.rs`).
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

/// Polls `holds` at the test's pace until it holds or [`WAIT_FOR`] passes; whether it held.
fn wait_for(mut holds: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + WAIT_FOR;
  while Instant::now() < deadline {
    if holds() {
      return true;
    }
    pause();
  }
  holds()
}

/// The attachment `attachment` is gone: the volume's count back to `before` within the wait, and a `detach` of it
/// refused.
fn the_attachment_ends(instance: &str, id: &str, before: &str, attachment: &str) {
  assert!(
    wait_for(|| attachments_of(instance, id) == before),
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
/// closes its handle and then waits while `slates shards` waits for it, then is let go; a probe that holds its handle while the test
/// kills `slates shards`; then the refusals. Expect the bytes equal to the file and the attachment gone after the
/// probe's exit; the command's exit code 7; the closer's attachment standing while the command holds its copy, and gone
/// once the closer exits, so no other copy of the end exists (not another child's); the attachment gone once the command is
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
  the_attachment_ends_when_the_program_exits_not_when_it_closes_its_handle(&instance, &id, &before);
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

/// A program that closes its handle and then waits on its standard input: its attachment stands while it, and the
/// `slates shards` waiting for it, still run, since the command keeps its own copy until the program exits (the
/// daemon's claim check needs a live holder until a VM's first ask; `slates_ipc::stream::spawn_with`); once the
/// program exits, the command exits with it and the attachment is gone, so no other copy exists, not one another child
/// inherited.
fn the_attachment_ends_when_the_program_exits_not_when_it_closes_its_handle(
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
  assert_ne!(
    attachments_of(instance, id),
    before,
    "attachment {attachment} stands while the command holds its copy"
  );
  assert!(
    running.0.try_wait().unwrap().is_none(),
    "`slates shards` still waits for its program"
  );
  drop(running.0.stdin.take());
  assert_eq!(running.0.wait().unwrap().code(), Some(0));
  the_attachment_ends(instance, id, before, &attachment);
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
    wait_for(|| attachments_of(instance, id) == before),
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
    let mut init = Vec::new();
    for word in [FUSE_MAJOR, FUSE_MINOR, READAHEAD, 0] {
      init.extend_from_slice(&word.to_le_bytes());
    }
    assert_eq!(errno(&self.ask(Opcode::Init, 0, &init)), 0, "INIT");
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
