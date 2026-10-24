//! The harness the real-VM tests share (`tests/guest_dax.rs`, `tests/guest_image.rs`): an anchor and its daemon,
//! volumes and their mounts, the guest's initramfs in the `newc` cpio format, and the VM, driven over its serial
//! console with `slates-guest-probe`'s line protocol. A test names its VMM's command line; everything else is here.
//! Every mount point is in the build output, named with the process id and removed at the end, so the tests write
//! nothing to disk but those.
// Each test binary uses part of the harness.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use slates_guest_probe::protocol::{Command as Ask, Reply};

/// Shape: how long the anchor and its daemon get to answer verbs.
pub(crate) const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: the pause between polls of a starting daemon.
pub(crate) const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive verbs a starting daemon must answer before the test runs its own, so the anchor's one startup
/// restart has settled (as `tests/cli.rs` waits).
pub(crate) const STABLE_STREAK: u32 = 10;
/// Shape: how long the guest gets to boot, mount the tag and say it is ready.
pub(crate) const BOOT_WAIT: Duration = Duration::from_secs(60);
/// Shape: how long one command's reply may take. A sync writes the map back through two filesystems.
pub(crate) const REPLY_WAIT: Duration = Duration::from_secs(30);
/// Shape: how long the VM gets to power off after it is told to quit.
pub(crate) const QUIT_WAIT: Duration = Duration::from_secs(20);
/// Shape: the console lines kept to explain a failure.
pub(crate) const TRANSCRIPT_LINES: usize = 80;
/// Format: the cpio `newc` magic, and the modes of the initramfs's directories and of `/init`
/// (`Documentation/driver-api/early-userspace/buffer-format.rst`).
const CPIO_MAGIC: &[u8] = b"070701";
/// Format: a directory, `rwxr-xr-x`.
const DIRECTORY_MODE: u32 = 0o040_755;
/// Format: a regular file, `rwxr-xr-x`.
const EXECUTABLE_MODE: u32 = 0o100_755;
/// Format: the name of the cpio archive's last entry.
const CPIO_TRAILER: &str = "TRAILER!!!";

pub(crate) fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

pub(crate) fn run(instance: &str, args: &[&str]) -> (i32, Vec<u8>, String) {
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

pub(crate) fn pause() {
  // The harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

/// The anchor: killed and reaped when dropped, so a failed assertion leaves no daemon behind.
pub(crate) struct Anchor {
  pub(crate) child: Child,
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// Starts `slates anchor` for `instance`, waits until its daemon answers verbs steadily, and creates its consensus
/// group.
pub(crate) fn start_anchor(instance: &str) -> Anchor {
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
pub(crate) fn await_steady(instance: &str) {
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
pub(crate) fn created_volume(instance: &str, name: &str) -> String {
  let (code, out, err) = run(instance, &["volume", "create", name, "--bounded", "64MiB"]);
  assert_eq!(code, 0, "volume create {name}: {err}");
  String::from_utf8_lossy(&out)
    .lines()
    .find_map(|line| line.strip_prefix("id: "))
    .unwrap_or_else(|| panic!("no id for {name}"))
    .to_owned()
}

/// A directory in the build output, named with the process id; removed by its [`MountPoint`].
pub(crate) fn scratch_dir(prefix: &str) -> String {
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
pub(crate) struct MountPoint {
  pub(crate) path: String,
}

impl MountPoint {
  pub(crate) fn of(instance: &str, volume: &str) -> MountPoint {
    let path = scratch_dir("slates-guest-mount");
    let (code, _, err) = run(instance, &["mount", volume, &path]);
    assert_eq!(code, 0, "slates mount {volume} {path}: {err}");
    MountPoint { path }
  }

  pub(crate) fn file(&self, name: &str) -> String {
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
pub(crate) fn write_whole(path: &str, bytes: &[u8]) {
  std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

/// Writes `bytes` at `offset` of `path` with one `pwrite`, through a slates mount: the other path's write.
#[allow(clippy::disallowed_methods)] // the test's own write through a slates mount: RAM, not disk
pub(crate) fn write_at(path: &str, offset: u64, bytes: &[u8]) {
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
pub(crate) fn initramfs(init: &[u8]) -> Vec<u8> {
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

/// The VM: its VMM in a process group of its own, its serial console the test's channel to the probe. Dropped, it is
/// told to quit and, if it has not gone within [`QUIT_WAIT`], killed with its whole process group.
pub(crate) struct Guest {
  pub(crate) vm: Child,
  console: ChildStdin,
  lines: Receiver<String>,
  readers: Vec<JoinHandle<()>>,
  transcript: std::collections::VecDeque<String>,
  /// The most bytes a read may ask for.
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
  /// Starts the VM `vmm` describes, in a process group of its own with its serial console on its standard streams, and
  /// waits until the guest says it is ready. `what` names the VMM in its stderr's lines; `most_bytes` bounds a read.
  pub(crate) fn spawn(vmm: &mut Command, what: &'static str, most_bytes: usize) -> Guest {
    let mut vm = vmm
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .process_group(0)
      .spawn()
      .unwrap_or_else(|e| panic!("start {what}: {e}"));
    let (sender, lines) = channel();
    let readers = vec![
      forward_lines(vm.stdout.take().unwrap(), "", sender.clone()),
      forward_lines(vm.stderr.take().unwrap(), what, sender),
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

  pub(crate) fn transcript(&self) -> String {
    let lines: Vec<&str> = self.transcript.iter().map(String::as_str).collect();
    format!("--- the console's last lines:\n{}", lines.join("\n"))
  }

  /// The next reply on the console within `within`, the kernel's and crosvm's own lines kept for a failure's report.
  pub(crate) fn next_reply(&mut self, within: Duration) -> Reply {
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
  pub(crate) fn ask(&mut self, command: &Ask, within: Duration) -> Reply {
    let line = command.encode();
    writeln!(self.console, "{line}")
      .and_then(|()| self.console.flush())
      .unwrap_or_else(|e| panic!("the console: {e}\n{}", self.transcript()));
    self.keep(format!("> {line}"));
    self.next_reply(within)
  }

  /// Asks, and expects [`Reply::Done`].
  pub(crate) fn done(&mut self, command: &Ask) {
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
  pub(crate) fn quit(&mut self) -> std::process::ExitStatus {
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
      // A VMM may fork a process per device (crosvm does); the group holds them all.
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
