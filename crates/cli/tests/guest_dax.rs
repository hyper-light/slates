//! A real VM over a slates mount, with DAX (`docs/wip/virtiofs.md` §8 item 7; §4.6 "Cache posture"; A-139, A-140,
//! A-146). A slates volume reaches a VM as a host mount the VMM shares in, with its own DAX window over the host's
//! cached copy of each file. Here crosvm's own virtio-fs device shares a `slates mount` folder into a guest whose
//! `/init` is `slates-guest-probe`. The test drives the guest over its serial console ([`slates_guest_probe::protocol`])
//! while it writes the same file through a second mount of the volume, the guest's other path.
//!
//! Gated. It needs Linux with KVM, and three files named in the environment; without them it skips loudly:
//! - `SLATES_GUEST_CROSVM`: a crosvm binary. Its device sandbox needs unprivileged user namespaces, which Linux
//!   distributions allow by default; with `--disable-sandbox` crosvm's virtio-fs device needs root.
//! - `SLATES_GUEST_KERNEL`: a guest kernel with virtio-fs and FUSE DAX built in (`CONFIG_VIRTIO_FS=y`,
//!   `CONFIG_FUSE_DAX=y`), such as many distributions' own.
//! - `SLATES_GUEST_PROBE`: the guest's `/init`, static, built by
//!   `RUSTFLAGS="-C target-feature=+crt-static" cargo build --release -p slates-guest-probe --target x86_64-unknown-linux-musl`
//!   (the workspace's own musl rustflags leave the C runtime dynamic, for its cdylibs).
//!
//! The guest's initramfs is written into a second slates volume through its mount, so the test writes nothing to disk
//! but its mount points (in `CARGO_TARGET_TMPDIR`, named with the process id, removed at the end).
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use slates_guest_probe::protocol::{Command as Ask, Reply};

/// Shape: how long the anchor and its daemon get to answer verbs.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: the pause between polls of a starting daemon.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive verbs a starting daemon must answer before the test runs its own, so the anchor's one startup
/// restart has settled (as `tests/cli.rs` waits).
const STABLE_STREAK: u32 = 10;
/// Shape: how long the guest gets to boot, mount the tag and say it is ready.
const BOOT_WAIT: Duration = Duration::from_secs(60);
/// Shape: how long one command's reply may take. A sync writes the map back through two filesystems.
const REPLY_WAIT: Duration = Duration::from_secs(30);
/// Shape: how long the guest waits, touching memory only, for another path's write to reach its map. Delivery takes a
/// median of 82 µs (GAPS, 2026-10-21); this bounds liveness, not latency.
const DELIVERY_MILLIS: u64 = 10_000;
/// Shape: how long the VM gets to power off after it is told to quit.
const QUIT_WAIT: Duration = Duration::from_secs(20);
/// Shape: the console lines kept to explain a failure.
const TRANSCRIPT_LINES: usize = 80;
/// Shape: the pages of each shared file; the guest's memory and processors.
const PAGES: u64 = 8;
/// Shape: the guest's memory, MiB.
const GUEST_MEMORY_MIB: u32 = 512;
/// Shape: the guest's processors.
const GUEST_CPUS: u32 = 2;
/// Shape: the generated histories' seeds, fixed so a failure is a sequence that replays, three so one lucky sequence
/// cannot pass alone; and each history's steps.
const SEEDS: [u64; 3] = [
  0x9e37_79b9_7f4a_7c15,
  0xd1b5_4a32_d192_ed03,
  0x2545_f491_4f6c_dd1d,
];
/// Shape: the steps of each generated history.
const STEPS: usize = 80;
/// Shape: the longest run of bytes one write or store covers: enough to cross a page now and then.
const LONGEST_RUN: u64 = 6000;
/// Format: the byte each shared file starts with.
const SEED_BYTE: u8 = b'.';
/// Format: the guest's stores take values from `[GUEST_FIRST, HOST_FIRST)` and the other path's writes from
/// `[HOST_FIRST, 256)`, each a fresh value per write, so every byte names its writer, and a wait for a write's value
/// can only end once that write has arrived.
const GUEST_FIRST: u8 = 1;
/// Format: see [`GUEST_FIRST`].
const HOST_FIRST: u8 = 128;
/// Format: the cpio `newc` magic, and the modes of the initramfs's directories and of `/init`
/// (`Documentation/driver-api/early-userspace/buffer-format.rst`).
const CPIO_MAGIC: &[u8] = b"070701";
/// Format: a directory, `rwxr-xr-x`.
const DIRECTORY_MODE: u32 = 0o040_755;
/// Format: a regular file, `rwxr-xr-x`.
const EXECUTABLE_MODE: u32 = 0o100_755;
/// Format: the name of the cpio archive's last entry.
const CPIO_TRAILER: &str = "TRAILER!!!";
/// Format: the daemon's status counter of bytes kept back from cache write-backs (A-140).
const KEPT_BACK: &str = "fuse.writeback_bytes_kept_back: ";

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

fn run(instance: &str, args: &[&str]) -> (i32, Vec<u8>, String) {
  let output = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .output()
    .unwrap();
  (
    output.status.code().unwrap_or(-1),
    output.stdout,
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn pause() {
  // The harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

/// The paths the test needs from the environment, or `None` after saying loudly which are missing.
struct Gate {
  crosvm: String,
  kernel: String,
  probe: String,
}

impl Gate {
  fn from_env() -> Option<Gate> {
    let named = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    match (
      named("SLATES_GUEST_CROSVM"),
      named("SLATES_GUEST_KERNEL"),
      named("SLATES_GUEST_PROBE"),
    ) {
      (Some(crosvm), Some(kernel), Some(probe)) => Some(Gate {
        crosvm,
        kernel,
        probe,
      }),
      _ => {
        eprintln!(
          "SKIP: the guest-over-the-mount test needs SLATES_GUEST_CROSVM, SLATES_GUEST_KERNEL and SLATES_GUEST_PROBE \
           (tests/guest_dax.rs says how to build the probe)"
        );
        None
      }
    }
  }
}

/// The anchor: killed and reaped when dropped, so a failed assertion leaves no daemon behind.
struct Anchor {
  child: Child,
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// Starts `slates anchor` for `instance`, waits until its daemon answers verbs steadily, and creates its consensus
/// group.
fn start_anchor(instance: &str) -> Anchor {
  let child = slates()
    .args(["--instance", instance, "anchor", "--quick", "--shards", "2"])
    .stdout(Stdio::null())
    .spawn()
    .unwrap();
  let anchor = Anchor { child };
  await_steady(instance);
  let (code, _, err) = run(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "explicit first-time bootstrap: {err}");
  anchor
}

/// Waits until `instance`'s daemon answers [`STABLE_STREAK`] verbs in a row.
fn await_steady(instance: &str) {
  let started = Instant::now();
  let mut streak = 0u32;
  while streak < STABLE_STREAK {
    assert!(
      started.elapsed() < START_WAIT,
      "the daemon answered {STABLE_STREAK} verbs in a row within {START_WAIT:?}"
    );
    streak = if run(instance, &["volume", "list"]).0 == 0 {
      streak.saturating_add(1)
    } else {
      0
    };
    pause();
  }
}

/// A volume made for `instance`: its id.
fn created_volume(instance: &str, name: &str) -> String {
  let (code, out, err) = run(instance, &["volume", "create", name, "--bounded", "64MiB"]);
  assert_eq!(code, 0, "volume create {name}: {err}");
  String::from_utf8_lossy(&out)
    .lines()
    .find_map(|line| line.strip_prefix("id: "))
    .unwrap_or_else(|| panic!("no id for {name}"))
    .to_owned()
}

/// A directory in the build output, named with the process id; removed by its [`MountPoint`].
fn scratch_dir(prefix: &str) -> String {
  static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let raw = format!(
    "{}/{prefix}-{}-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id(),
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
  );
  let made = Command::new("mkdir").args(["-p", &raw]).output().unwrap();
  assert!(
    made.status.success(),
    "mkdir -p {raw}: {}",
    String::from_utf8_lossy(&made.stderr)
  );
  std::fs::canonicalize(&raw)
    .unwrap()
    .to_string_lossy()
    .into_owned()
}

/// A `slates mount` of a volume: unmounted (lazily if busy) and its directory removed when dropped.
struct MountPoint {
  path: String,
}

impl MountPoint {
  fn of(instance: &str, volume: &str) -> MountPoint {
    let path = scratch_dir("slates-guest-mount");
    let (code, _, err) = run(instance, &["mount", volume, &path]);
    assert_eq!(code, 0, "slates mount {volume} {path}: {err}");
    MountPoint { path }
  }

  fn file(&self, name: &str) -> String {
    format!("{}/{name}", self.path)
  }
}

impl Drop for MountPoint {
  fn drop(&mut self) {
    let unmounted = Command::new("fusermount3")
      .args(["-u", &self.path])
      .output()
      .is_ok_and(|output| output.status.success());
    if !unmounted {
      let _ = Command::new("fusermount3")
        .args(["-u", "-z", &self.path])
        .output();
    }
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// Writes `bytes` as the whole of `path`, through a slates mount.
#[allow(clippy::disallowed_methods)] // the test's own write through a slates mount: RAM, not disk
fn write_whole(path: &str, bytes: &[u8]) {
  std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

/// Writes `bytes` at `offset` of `path` with one `pwrite`, through a slates mount: the other path's write.
#[allow(clippy::disallowed_methods)] // the test's own write through a slates mount: RAM, not disk
fn write_at(path: &str, offset: u64, bytes: &[u8]) {
  use std::os::unix::fs::FileExt;
  let file = std::fs::OpenOptions::new()
    .write(true)
    .open(path)
    .unwrap_or_else(|e| panic!("open {path}: {e}"));
  file
    .write_all_at(bytes, offset)
    .unwrap_or_else(|e| panic!("pwrite {path} at {offset}: {e}"));
}

/// The guest's initramfs in the `newc` cpio format the kernel unpacks: the directories the probe mounts over, and the
/// probe itself as `/init`.
fn initramfs(init: &[u8]) -> Vec<u8> {
  let mut archive = Vec::new();
  for (inode, name) in ["dev", "proc", "sys", "mnt"].into_iter().enumerate() {
    cpio_entry(
      &mut archive,
      inode.saturating_add(1),
      name,
      DIRECTORY_MODE,
      &[],
    );
  }
  cpio_entry(&mut archive, 5, "init", EXECUTABLE_MODE, init);
  cpio_entry(&mut archive, 0, CPIO_TRAILER, 0, &[]);
  archive
}

/// One `newc` entry: the magic and thirteen eight-digit hex fields, the name and its NUL padded to four bytes, then
/// the data padded to four.
fn cpio_entry(archive: &mut Vec<u8>, inode: usize, name: &str, mode: u32, data: &[u8]) {
  let fields = [
    inode,
    usize::try_from(mode).unwrap(),
    0,
    0,
    1,
    0,
    data.len(),
    0,
    0,
    0,
    0,
    name.len().saturating_add(1),
    0,
  ];
  archive.extend_from_slice(CPIO_MAGIC);
  for field in fields {
    archive.extend_from_slice(format!("{field:08X}").as_bytes());
  }
  archive.extend_from_slice(name.as_bytes());
  archive.push(0);
  pad_to_four(archive);
  archive.extend_from_slice(data);
  pad_to_four(archive);
}

fn pad_to_four(archive: &mut Vec<u8>) {
  while !archive.len().is_multiple_of(4) {
    archive.push(0);
  }
}

/// The VM: crosvm in a process group of its own, its serial console the test's channel to the probe. Dropped, it is
/// told to quit and, if it has not gone within [`QUIT_WAIT`], killed with its whole process group.
struct Guest {
  vm: Child,
  console: ChildStdin,
  lines: Receiver<String>,
  readers: Vec<JoinHandle<()>>,
  transcript: std::collections::VecDeque<String>,
  /// The most bytes a read may ask for: the map's.
  most_bytes: usize,
}

/// Sends `source`'s lines, each tagged with `tag`, until it ends.
fn forward_lines(
  source: impl Read + Send + 'static,
  tag: &'static str,
  lines: Sender<String>,
) -> JoinHandle<()> {
  std::thread::spawn(move || {
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();
    loop {
      line.clear();
      match reader.read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {
          let text = String::from_utf8_lossy(&line);
          if lines
            .send(format!("{tag}{}", text.trim_end_matches(['\r', '\n'])))
            .is_err()
          {
            return;
          }
        }
      }
    }
  })
}

impl Guest {
  /// Boots the guest with `shared` (a slates mount) shared in as tag `slates` with DAX, and waits until it is ready.
  fn boot(gate: &Gate, initrd: &str, shared: &str, most_bytes: usize) -> Guest {
    let shared_dir = format!(
      "{shared}:slates:type=fs:cache=always:timeout=3600:dax=true:writeback=false:posix_acl=false:security_ctx=false"
    );
    let mut vm = Command::new(&gate.crosvm)
      .args([
        "run",
        "-m",
        &GUEST_MEMORY_MIB.to_string(),
        "--cpus",
        &GUEST_CPUS.to_string(),
        "--shared-dir",
        &shared_dir,
        "--initrd",
        initrd,
        "-p",
        "console=ttyS0 rdinit=/init quiet slates.dax=always",
        "--serial",
        "type=stdout,hardware=serial,num=1,console,stdin",
        &gate.kernel,
      ])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .process_group(0)
      .spawn()
      .unwrap_or_else(|e| panic!("start {}: {e}", gate.crosvm));
    let (sender, lines) = channel();
    let readers = vec![
      forward_lines(vm.stdout.take().unwrap(), "", sender.clone()),
      forward_lines(vm.stderr.take().unwrap(), "crosvm: ", sender),
    ];
    let mut guest = Guest {
      console: vm.stdin.take().unwrap(),
      vm,
      lines,
      readers,
      transcript: std::collections::VecDeque::new(),
      most_bytes,
    };
    let ready = guest.next_reply(BOOT_WAIT);
    assert_eq!(ready, Reply::Ready, "{}", guest.transcript());
    guest
  }

  fn keep(&mut self, line: String) {
    if self.transcript.len() >= TRANSCRIPT_LINES {
      self.transcript.pop_front();
    }
    self.transcript.push_back(line);
  }

  fn transcript(&self) -> String {
    let lines: Vec<&str> = self.transcript.iter().map(String::as_str).collect();
    format!("--- the console's last lines:\n{}", lines.join("\n"))
  }

  /// The next reply on the console within `within`, the kernel's and crosvm's own lines kept for a failure's report.
  fn next_reply(&mut self, within: Duration) -> Reply {
    let deadline = Instant::now().checked_add(within).unwrap();
    loop {
      match self
        .lines
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
      {
        Ok(line) => {
          let parsed = Reply::parse(&line, self.most_bytes);
          self.keep(line.clone());
          if let Some(reply) = parsed {
            return reply.unwrap_or_else(|e| {
              panic!(
                "the reply `{line}` does not parse: {e}\n{}",
                self.transcript()
              )
            });
          }
        }
        Err(RecvTimeoutError::Timeout) => {
          panic!("no reply within {within:?}\n{}", self.transcript())
        }
        Err(RecvTimeoutError::Disconnected) => {
          panic!("the VM's console closed\n{}", self.transcript())
        }
      }
    }
  }

  /// Sends one command and returns its reply.
  fn ask(&mut self, command: &Ask, within: Duration) -> Reply {
    let line = command.encode();
    writeln!(self.console, "{line}")
      .and_then(|()| self.console.flush())
      .unwrap_or_else(|e| panic!("the console: {e}\n{}", self.transcript()));
    self.keep(format!("> {line}"));
    self.next_reply(within)
  }

  /// Asks, and expects [`Reply::Done`].
  fn done(&mut self, command: &Ask) {
    let reply = self.ask(command, REPLY_WAIT);
    assert_eq!(
      reply,
      Reply::Done,
      "{} → {reply:?}\n{}",
      command.encode(),
      self.transcript()
    );
  }

  /// Tells the guest to quit, and waits for the VM to power off: its exit status.
  fn quit(&mut self) -> std::process::ExitStatus {
    self.done(&Ask::Quit);
    let started = Instant::now();
    loop {
      if let Some(status) = self.vm.try_wait().unwrap() {
        return status;
      }
      assert!(
        started.elapsed() < QUIT_WAIT,
        "the VM powered off within {QUIT_WAIT:?}\n{}",
        self.transcript()
      );
      pause();
    }
  }
}

impl Drop for Guest {
  fn drop(&mut self) {
    if matches!(self.vm.try_wait(), Ok(None)) {
      let _ = writeln!(self.console, "{}", Ask::Quit.encode());
      let started = Instant::now();
      while matches!(self.vm.try_wait(), Ok(None)) && started.elapsed() < QUIT_WAIT {
        pause();
      }
      // crosvm forks a process per device; the group holds them all.
      let _ = rustix::process::kill_process_group(
        rustix::process::Pid::from_child(&self.vm),
        rustix::process::Signal::KILL,
      );
      let _ = self.vm.wait();
    }
    for reader in self.readers.drain(..) {
      let _ = reader.join();
    }
  }
}

/// What each byte of a shared file must hold: the value of the latest write to it from either side. That is the one
/// page cache the mount route must behave as. The history makes it exact: after each of the other path's writes, the
/// guest waits until both ends of the write show in its map, so the guest's later stores land in pages already
/// re-fetched (the AC-3.3 residual, a store into a page whose change is still on its way, never arises).
struct Expected {
  bytes: Vec<u8>,
}

impl Expected {
  fn write(&mut self, at: u64, len: u64, value: u8) {
    let start = usize::try_from(at).unwrap();
    let end = usize::try_from(at.checked_add(len).unwrap()).unwrap();
    self.bytes[start..end].fill(value);
  }

  /// Panics naming the first bytes that differ from `actual`, and which side wrote each.
  fn assert_held_by(&self, actual: &[u8], what: &str, guest: &Guest) {
    if actual == self.bytes.as_slice() {
      return;
    }
    let writer = |value: u8| match value {
      SEED_BYTE => "the seed",
      value if value >= HOST_FIRST => "the other path",
      _ => "the guest",
    };
    let wrong: Vec<String> = self
      .bytes
      .iter()
      .zip(actual)
      .enumerate()
      .filter(|(_, (expected, held))| expected != held)
      .take(8)
      .map(|(at, (expected, held))| {
        format!(
          "byte {at}: holds {held} ({}), expected {expected} ({})",
          writer(*held),
          writer(*expected)
        )
      })
      .collect();
    panic!(
      "{what} differs from the latest write to each byte (lengths {} and {}):\n{}\n{}",
      actual.len(),
      self.bytes.len(),
      wrong.join("\n"),
      guest.transcript()
    );
  }
}

/// The next value of a fixed-seed xorshift64 generator (Marsaglia, "Xorshift RNGs", 2003), as the other chaos tests
/// use.
fn xorshift(state: &mut u64) -> u64 {
  let mut x = *state;
  x ^= x << 13;
  x ^= x >> 7;
  x ^= x << 17;
  *state = x;
  x
}

/// The next fresh value from `[first, end)`, cycling.
fn next_value(counter: &mut u8, first: u8, end: u16) -> u8 {
  let span = end.saturating_sub(u16::from(first));
  let value = u16::from(first).saturating_add(u16::from(*counter) % span);
  *counter = counter.wrapping_add(1);
  u8::try_from(value).unwrap()
}

/// A run within a file of `len` bytes, from `roll`'s bits: its start and length.
fn run_within(roll: u64, len: u64) -> (u64, u64) {
  let at = (roll >> 8) % len;
  let run = ((roll >> 32) % LONGEST_RUN).saturating_add(1).min(len - at);
  (at, run)
}

/// What one generated history did.
#[derive(Default)]
struct Tally {
  stores: u32,
  writes: u32,
  syncs: u32,
  checks: u32,
  slowest_arrival_micros: u64,
}

/// Waits until both ends of the other path's write of `value` over `[at, at + run)` show in the guest's map, memory
/// alone: the first and the last page of the write, since the kernel drops a written range's pages in order. The
/// slower of the two waits, in microseconds.
fn arrival_in_the_map((at, run): (u64, u64), value: u8, guest: &mut Guest) -> u64 {
  let mut slowest = 0;
  for end in [at, at.saturating_add(run).saturating_sub(1)] {
    match guest.ask(
      &Ask::Wait {
        at: end,
        byte: value,
        millis: DELIVERY_MILLIS,
      },
      REPLY_WAIT,
    ) {
      Reply::Waited { micros } => slowest = slowest.max(micros),
      other => panic!(
        "the other path's write of {value} at [{at}, {at} + {run}) did not reach the guest's map at {end}: \
         {other:?}\n{}",
        guest.transcript()
      ),
    }
  }
  slowest
}

/// One generated history over one shared file: the model of what every byte must hold, the generator, and the tally.
/// It can stop and go on around a daemon's death or a VM's.
struct History<'r> {
  rig: &'r Rig,
  file: String,
  guest_file: String,
  host_file: String,
  len: u64,
  expected: Expected,
  state: u64,
  guest_counter: u8,
  host_counter: u8,
  tally: Tally,
}

impl<'r> History<'r> {
  /// Writes `file` through the guest's mount, has `guest` map it with DAX, and seeds the generator.
  fn start(rig: &'r Rig, file: &str, seed: u64, guest: &mut Guest) -> History<'r> {
    let len = PAGES.saturating_mul(u64::try_from(rustix::param::page_size()).unwrap());
    let history = History {
      rig,
      file: file.to_owned(),
      guest_file: rig.guest_side.file(file),
      host_file: rig.host_side.file(file),
      len,
      expected: Expected {
        bytes: vec![SEED_BYTE; usize::try_from(len).unwrap()],
      },
      state: seed,
      guest_counter: 0,
      host_counter: 0,
      tally: Tally::default(),
    };
    write_whole(&history.guest_file, &history.expected.bytes);
    history.map(guest);
    history
  }

  /// Has `guest` map the file, which must be served through the DAX window.
  fn map(&self, guest: &mut Guest) {
    match guest.ask(
      &Ask::Map {
        path: self.file.clone(),
        pages: PAGES,
      },
      REPLY_WAIT,
    ) {
      Reply::Mapped { dax: true } => {}
      other => panic!(
        "map {} → {other:?}: DAX is what this test is for\n{}",
        self.file,
        guest.transcript()
      ),
    }
  }

  /// A span of the file for a step chosen outside [`History::steps`].
  fn span(&mut self) -> (u64, u64) {
    let roll = xorshift(&mut self.state);
    run_within(roll, self.len)
  }

  /// The guest stores a fresh value over `span`, unsynced.
  fn guest_stores(&mut self, (at, run): (u64, u64), guest: &mut Guest) {
    let value = next_value(&mut self.guest_counter, GUEST_FIRST, u16::from(HOST_FIRST));
    guest.done(&Ask::Store {
      at,
      len: run,
      byte: value,
    });
    self.expected.write(at, run, value);
    self.tally.stores = self.tally.stores.saturating_add(1);
  }

  /// The other path writes a fresh value over `span`. With a live guest, the guest waits until the write shows in its
  /// map ([`arrival_in_the_map`]).
  fn other_path_writes(&mut self, (at, run): (u64, u64), guest: Option<&mut Guest>) {
    let value = next_value(&mut self.host_counter, HOST_FIRST, 256);
    write_at(
      &self.host_file,
      at,
      &vec![value; usize::try_from(run).unwrap()],
    );
    if let Some(guest) = guest {
      let arrival = arrival_in_the_map((at, run), value, guest);
      self.tally.slowest_arrival_micros = self.tally.slowest_arrival_micros.max(arrival);
    }
    self.expected.write(at, run, value);
    self.tally.writes = self.tally.writes.saturating_add(1);
  }

  /// `count` generated steps: the guest's unsynced stores, its syncs, the other path's writes, and checks.
  fn steps(&mut self, count: usize, guest: &mut Guest) {
    for _ in 0..count {
      let roll = xorshift(&mut self.state);
      let span = run_within(roll, self.len);
      match roll % 10 {
        0..=3 => self.guest_stores(span, guest),
        4..=6 => self.other_path_writes(span, Some(guest)),
        7 | 8 => {
          guest.done(&Ask::Sync);
          self.tally.syncs = self.tally.syncs.saturating_add(1);
        }
        _ => self.check(guest),
      }
    }
  }

  /// Syncs the guest, then holds the volume, read through slates itself past both mounts, and the guest's map to the
  /// latest write to every byte.
  fn check(&mut self, guest: &mut Guest) {
    guest.done(&Ask::Sync);
    self.volume_holds_the_latest_writes(guest);
    match guest.ask(
      &Ask::Read {
        at: 0,
        len: self.len,
      },
      REPLY_WAIT,
    ) {
      Reply::Bytes(mapped) => {
        self
          .expected
          .assert_held_by(&mapped, &format!("the guest's map of {}", self.file), guest)
      }
      other => panic!("read → {other:?}\n{}", guest.transcript()),
    }
    self.tally.checks = self.tally.checks.saturating_add(1);
  }

  /// Holds the volume's file, read through slates itself past both mounts, to the latest write to every byte; `guest`
  /// only lends its console's transcript to a failure.
  fn volume_holds_the_latest_writes(&self, guest: &Guest) {
    let (code, held, err) = run(
      &self.rig.instance,
      &["read", &self.rig.shared, &format!("/{}", self.file)],
    );
    assert_eq!(
      code, 0,
      "slates read {} /{}: {err}",
      self.rig.shared, self.file
    );
    self.expected.assert_held_by(
      &held,
      &format!("the volume's {}, read through slates", self.file),
      guest,
    );
  }
}

/// The daemon's bytes kept back from cache write-backs so far, over every shard.
fn kept_back(instance: &str) -> u64 {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "slates status: {err}");
  String::from_utf8_lossy(&out)
    .lines()
    .filter_map(|line| line.split_once(KEPT_BACK))
    .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
    .sum()
}

/// What every guest test stands on: an anchor; the shared volume mounted twice, once to be shared into the VM and once
/// as the host's other path; and a second volume holding the guest's initramfs. Fields drop in order, so the mounts go
/// before the anchor.
struct Rig {
  instance: String,
  shared: String,
  guest_side: MountPoint,
  host_side: MountPoint,
  _boot_side: MountPoint,
  initrd: String,
  most_bytes: usize,
  anchor: Anchor,
}

impl Rig {
  /// Starts an anchor for an instance named after `tag`, makes and mounts the volumes, and writes the initramfs.
  fn new(gate: &Gate, tag: &str) -> Rig {
    let instance = format!("{tag}-{}", std::process::id());
    let anchor = start_anchor(&instance);
    let shared = created_volume(&instance, "shared");
    let boot = created_volume(&instance, "boot");
    let guest_side = MountPoint::of(&instance, &shared);
    let host_side = MountPoint::of(&instance, &shared);
    let boot_side = MountPoint::of(&instance, &boot);
    #[allow(clippy::disallowed_methods)]
    // reads the built probe binary: a test input, never written
    let probe = std::fs::read(&gate.probe).unwrap_or_else(|e| panic!("read {}: {e}", gate.probe));
    let initrd = boot_side.file("initramfs.cpio");
    write_whole(&initrd, &initramfs(&probe));
    let most_bytes = usize::try_from(PAGES)
      .unwrap()
      .saturating_mul(rustix::param::page_size());
    Rig {
      instance,
      shared,
      guest_side,
      host_side,
      _boot_side: boot_side,
      initrd,
      most_bytes,
      anchor,
    }
  }

  /// Boots a guest with the guest side of the shared volume shared in.
  fn boot(&self, gate: &Gate) -> Guest {
    Guest::boot(gate, &self.initrd, &self.guest_side.path, self.most_bytes)
  }
}

/// Shape: the guest's stores left unsynced across a daemon's or a VM's death.
const UNSYNCED_STORES: usize = 6;
/// Shape: the other path's writes after a VM's death, over spans its unsynced stores may share.
const WRITES_AFTER_THE_KILL: usize = 4;

/// The anchor's daemon: the process whose parent is the anchor (`/proc/PID/stat`'s fourth field).
fn daemon_of(anchor: &Anchor) -> Option<u32> {
  let anchor_pid = anchor.child.id();
  #[allow(clippy::disallowed_methods)] // reads the kernel's process table: a query, never a write
  let processes = std::fs::read_dir("/proc").ok()?;
  processes
    .flatten()
    .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    .find(|pid| {
      #[allow(clippy::disallowed_methods)] // the same query, one process at a time
      let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
      let parent = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(1))
        .and_then(|parent| parent.parse::<u32>().ok());
      parent == Some(anchor_pid)
    })
}

/// Kills the rig's daemon with `SIGKILL`, and waits until the anchor's successor answers verbs steadily: the two
/// process ids.
fn kill_the_daemon(rig: &Rig) -> (u32, u32) {
  let old = daemon_of(&rig.anchor).expect("the anchor runs a daemon");
  rustix::process::kill_process(
    rustix::process::Pid::from_raw(i32::try_from(old).unwrap()).unwrap(),
    rustix::process::Signal::KILL,
  )
  .unwrap();
  let started = Instant::now();
  let new = loop {
    assert!(
      started.elapsed() < START_WAIT,
      "the anchor restarted its daemon within {START_WAIT:?}"
    );
    match daemon_of(&rig.anchor) {
      Some(pid) if pid != old => break pid,
      _ => pause(),
    }
  };
  await_steady(&rig.instance);
  (old, new)
}

/// Syncs `path` through its slates mount from the host, so what the host's page cache holds dirty is written back.
#[allow(clippy::disallowed_methods)] // the test's own sync through a slates mount: RAM, not disk
fn sync_through(path: &str) {
  std::fs::File::open(path)
    .and_then(|file| file.sync_all())
    .unwrap_or_else(|e| panic!("fsync {path}: {e}"));
}

impl Guest {
  /// Kills the VM and its device processes without warning, as a crashed VMM dies.
  fn kill(&mut self) {
    rustix::process::kill_process_group(
      rustix::process::Pid::from_child(&self.vm),
      rustix::process::Signal::KILL,
    )
    .unwrap();
    let _ = self.vm.wait();
  }
}

/// §4.6 (the mount route, A-139/A-140/A-146) through a real guest with DAX. Do: share a slates mount into a crosvm
/// guest with DAX, and over each of three fixed-seed files run a generated history of the guest's unsynced stores into
/// its shared map, its syncs, and the other path's writes through a second mount of the volume, each waited for in the
/// guest's map as memory alone. At random points and at the end, sync the guest and read the file through slates
/// itself, past both mounts, and through the guest's map.
///
/// Expect: the guest's file is in DAX mode; every write of the other path reaches the guest's map as memory alone; and
/// at every check, every byte of the volume and of the map holds the latest write to it from either side. That is one
/// page cache's behaviour, which a dirty guest page written back over a later write would break (A-139/A-140). The
/// keep-back count must move, or no history put a guest store and a later write on one page, and the test proved
/// nothing.
#[test]
fn a_guest_maps_a_slates_mount_with_dax_and_holds_the_latest_write_to_every_byte() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let rig = Rig::new(&gate, "guest-dax");
  let mut guest = rig.boot(&gate);
  let mut tallies = Vec::new();
  for (index, seed) in SEEDS.into_iter().enumerate() {
    let mut history = History::start(&rig, &format!("history-{index}"), seed, &mut guest);
    history.steps(STEPS, &mut guest);
    history.check(&mut guest);
    let tally = std::mem::take(&mut history.tally);
    eprintln!(
      "history {index} (seed {seed:#x}): {} guest stores, {} other-path writes (slowest to reach the map: {} µs), \
       {} syncs, {} checks",
      tally.stores, tally.writes, tally.slowest_arrival_micros, tally.syncs, tally.checks
    );
    tallies.push(tally);
  }
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
  let kept = kept_back(&rig.instance);
  eprintln!("bytes kept back from the guest's write-backs: {kept}");
  assert!(
    kept > 0,
    "non-vacuous: some history wrote through the other path over a page the guest had stored into unsynced"
  );
  assert!(
    tallies
      .iter()
      .all(|tally| tally.writes > 0 && tally.stores > 0 && tally.checks > 1),
    "every history stored, wrote through the other path and checked more than once"
  );
}

/// §4.6 with a takeover (A-61) under a real guest with DAX. Do: run half a generated history; leave several of the
/// guest's stores unsynced in its map; kill the daemon with `SIGKILL` while the guest holds the live map; wait for the
/// anchor's successor to answer; run the rest of the history on the same map.
///
/// Expect: the successor is a new process, and every check before and after holds every byte of the volume and of the
/// map to its latest write, the stores left unsynced across the kill included. The restored mount judges the guest's
/// write-backs without the dead daemon's byte states, against every record since it started (GAPS), which this history
/// stays well inside.
#[test]
fn a_daemon_killed_under_a_guests_live_map_is_taken_over_and_the_guest_goes_on() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let rig = Rig::new(&gate, "guest-takeover");
  let mut guest = rig.boot(&gate);
  let mut history = History::start(&rig, "takeover", SEEDS[0], &mut guest);
  history.steps(STEPS / 2, &mut guest);
  history.check(&mut guest);
  for _ in 0..UNSYNCED_STORES {
    let span = history.span();
    history.guest_stores(span, &mut guest);
  }
  let (old, new) = kill_the_daemon(&rig);
  assert_ne!(old, new, "the anchor's successor is a new daemon");
  history.steps(STEPS / 2, &mut guest);
  history.check(&mut guest);
  let tally = &history.tally;
  eprintln!(
    "daemon {old} killed, successor {new}: {} guest stores, {} other-path writes (slowest to reach the map: {} µs), \
     {} checks",
    tally.stores, tally.writes, tally.slowest_arrival_micros, tally.checks
  );
  assert!(
    tally.writes > 0 && tally.checks > 2,
    "non-vacuous: the history wrote through the other path and checked after the takeover"
  );
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
}

/// §4.6 through a real guest with DAX when the VM dies. Do: run half a generated history; have the guest store over
/// several spans without syncing; kill the VM and its device processes without warning; write over some spans through
/// the other path; then fsync the file through the guest's side of the mount from the host. Then boot a second guest
/// on the same mount and map the file.
///
/// Expect: the volume holds the latest write to every byte, the dead guest's unsynced stores included wherever nothing
/// later wrote over them, and the second guest's map holds the same. The host's page cache outlives the VM that
/// dirtied it, so its stores are written back, judged by the write-back rule like any other (A-140).
#[test]
fn a_guest_killed_with_unsynced_stores_leaves_them_to_land_and_a_new_guest_reads_them() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let rig = Rig::new(&gate, "guest-crash");
  let mut first = rig.boot(&gate);
  let mut history = History::start(&rig, "crash", SEEDS[1], &mut first);
  history.steps(STEPS / 2, &mut first);
  history.check(&mut first);
  for _ in 0..UNSYNCED_STORES {
    let span = history.span();
    history.guest_stores(span, &mut first);
  }
  first.kill();
  for _ in 0..WRITES_AFTER_THE_KILL {
    let span = history.span();
    history.other_path_writes(span, None);
  }
  sync_through(&history.guest_file);
  history.volume_holds_the_latest_writes(&first);
  let mut second = rig.boot(&gate);
  history.map(&mut second);
  history.check(&mut second);
  eprintln!(
    "the first VM killed with {UNSYNCED_STORES} unsynced stores, {WRITES_AFTER_THE_KILL} writes after it: the volume \
     and a second guest's map hold the latest write to every byte"
  );
  let status = second.quit();
  assert!(
    status.success(),
    "the second VM powered off cleanly: {status}\n{}",
    second.transcript()
  );
}
