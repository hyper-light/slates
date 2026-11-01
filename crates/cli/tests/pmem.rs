//! `slates pmem` by use (Step 6 of the VFS and DAX plan; A-186's writable copy as a pmem VMM's backing, with no host
//! mount). The launcher maps a file of a volume to write with a 2 MiB granule and runs a VMM as its child with the copy
//! at a descriptor number. Here the VMM is this test binary re-invoked (an environment variable selects the role,
//! `pmem_vmm_role`): it opens the copy by the `/proc/self/fd/N` name the launcher put in its arguments, maps it shared
//! as a pmem VMM maps its backing, and stores what the test tells it on its standard input. The test reads the file
//! through slates itself (`slates read`), never through a mount.
//!
//! What the launcher must do, each held here:
//! - hand the VMM a copy whose length is a 2 MiB multiple, the length it announces and the VMM sees;
//! - compare the copy back into the file at each SIGUSR1 and at the VMM's exit, and never write a store past the
//!   file's end;
//! - end the VMM when it is killed itself (Linux's parent-death signal), so no VM outlives its map's holder;
//! - pass SIGTERM to the VMM, and compare its stores back after it ends.
//!
//! Linux only: the launcher runs a VMM with a pmem copy there alone.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod vm;

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use slates_client::{Client, Intent, VolumeId};
use vm::{Anchor, created_volume, run, start_anchor};

/// Shape: the file the launcher maps: 100 KiB, short of a 2 MiB multiple, so the copy's length shows the granule.
const FILE_BYTES: usize = 100 << 10;
/// Format: the granule the launcher asks its copy in (`slates pmem`, `PMEM_GRANULE`).
const GRANULE: u64 = 2 << 20;
/// Format: the file, at the root of the volume.
const FILE: &str = "image.bin";
/// Format: the descriptor number the VMM finds its copy at.
const COPY_FD: &str = "3";
/// Shape: how long a line from the launcher or its VMM, a sync, or a process's end may take.
const WAIT: Duration = Duration::from_secs(20);
/// Format: the environment variables the role reads: the copy's name and length, as the launcher wrote them into the
/// VMM's arguments.
const ROLE_PATH: &str = "SLATES_TEST_PMEM_PATH";
/// See [`ROLE_PATH`].
const ROLE_LEN: &str = "SLATES_TEST_PMEM_LEN";
/// Format: the tag of every line the role prints. libtest prints its own banner first, and `test <name> ... ` with no
/// line end, so the role's first line comes glued to that: the tag is looked for anywhere in a line.
const ROLE_TAG: &str = "pmem-vmm:";
/// Format: the byte the file starts as, and the bytes the VMM stores.
const SEED: u8 = 0x2E;
/// See [`SEED`].
const FIRST: u8 = 0x41;
/// See [`SEED`].
const SECOND: u8 = 0x42;
/// See [`SEED`].
const BEYOND: u8 = 0x5A;
/// Format: the exit status a shell gives a process ended by SIGTERM, which the launcher exits with when its VMM is.
const ENDED_BY_SIGTERM: i32 = 128 + 15;
/// Format: the words of `slates read`'s refusal of a file another write changed during the read.
const READ_CHANGED: &str = "changed while it was read";

/// The stand-in VMM, when the launcher's command selects it (`ROLE_PATH` set); a plain run of the suite does nothing
/// here. It opens the copy by name, maps all of it shared, prints its pid, the length it mapped and the size it sees,
/// then follows its standard input: `store AT BYTE COUNT` stores COUNT copies of BYTE from AT and says so; `exit CODE`
/// exits with CODE; the input's end exits 0.
#[test]
#[allow(clippy::disallowed_methods)] // the role opens the copy it was handed: a memory object, never a file on disk
fn pmem_vmm_role() {
  let Ok(path) = std::env::var(ROLE_PATH) else {
    return;
  };
  let len: usize = std::env::var(ROLE_LEN).unwrap().parse().unwrap();
  let copy = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&path)
    .unwrap_or_else(|e| panic!("open {path}: {e}"));
  let sized = copy.metadata().unwrap().len();
  // SAFETY: a fresh shared map of `len` bytes of the copy this function holds open, at an address the kernel chooses;
  // the process ends without unmapping it, which the kernel does then.
  let map = unsafe {
    rustix::mm::mmap(
      std::ptr::null_mut(),
      len,
      rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
      rustix::mm::MapFlags::SHARED,
      copy.as_fd(),
      0,
    )
  }
  .unwrap()
  .cast::<u8>();
  println!(
    "{ROLE_TAG} pid {} mapped {len} sized {sized}",
    std::process::id()
  );
  std::io::stdout().flush().unwrap();
  for line in std::io::stdin().lock().lines() {
    let line = line.unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
      ["store", at, byte, count] => {
        let (at, byte, count): (usize, u8, usize) = (
          at.parse().unwrap(),
          byte.parse().unwrap(),
          count.parse().unwrap(),
        );
        assert!(at + count <= len, "a store within the map");
        for offset in at..at + count {
          // SAFETY: `offset` is below `len`, the length mapped above, and the map lives until the process ends; a
          // volatile store holds no reference across the daemon's own writes into the copy.
          unsafe { std::ptr::write_volatile(map.add(offset), byte) };
        }
        println!("{ROLE_TAG} stored {count} at {at}");
        std::io::stdout().flush().unwrap();
      }
      ["exit", code] => std::process::exit(code.parse().unwrap()),
      _ => panic!("the role does not speak `{line}`"),
    }
  }
}

/// A running `slates pmem`: the launcher, the standard input the VMM reads, and the lines both print.
struct Launch {
  launcher: Child,
  input: ChildStdin,
  lines: Receiver<String>,
  seen: Vec<String>,
}

impl Launch {
  /// Starts `slates pmem` on `volume`'s file with the copy at [`COPY_FD`], running this test binary as the VMM through
  /// `sh`, whose command line carries `{pmem}` and `{len}` for the launcher to fill in.
  fn start(instance: &str, volume: &str) -> Launch {
    let me = std::env::current_exe().unwrap();
    let script = format!(
      "{ROLE_PATH}={{pmem}} {ROLE_LEN}={{len}} exec \"$0\" --exact pmem_vmm_role --nocapture --test-threads=1"
    );
    let mut launcher = vm::slates()
      .args([
        "--instance",
        instance,
        "pmem",
        volume,
        &format!("/{FILE}"),
        "--fd",
        COPY_FD,
        "--",
      ])
      .args(["sh", "-c", &script])
      .arg(&me)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit())
      .spawn()
      .unwrap();
    let input = launcher.stdin.take().unwrap();
    let output = launcher.stdout.take().unwrap();
    let (sender, lines) = channel();
    std::thread::spawn(move || {
      for line in BufReader::new(output).lines() {
        let Ok(line) = line else { return };
        if sender.send(line).is_err() {
          return;
        }
      }
    });
    Launch {
      launcher,
      input,
      lines,
      seen: Vec::new(),
    }
  }

  /// The next line holding `words`, within [`WAIT`]; every line read is kept for a failure's report.
  fn expect(&mut self, words: &str) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
      match self
        .lines
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
      {
        Ok(line) => {
          self.seen.push(line.clone());
          if line.contains(words) {
            return line;
          }
        }
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => panic!(
          "no line holding `{words}` within {WAIT:?}; the lines so far:\n{}",
          self.seen.join("\n")
        ),
      }
    }
  }

  /// The number after `key: ` in the next line holding it.
  fn value(&mut self, key: &str) -> u64 {
    let line = self.expect(&format!("{key}: "));
    line
      .rsplit_once(": ")
      .and_then(|(_, value)| value.trim().parse().ok())
      .unwrap_or_else(|| panic!("no number in `{line}`"))
  }

  /// Tells the VMM one line, and waits for its answer to a store.
  fn store(&mut self, at: u64, byte: u8, count: u64) {
    writeln!(self.input, "store {at} {byte} {count}").unwrap();
    self.input.flush().unwrap();
    self.expect(&format!("{ROLE_TAG} stored {count} at {at}"));
  }

  /// Sends the launcher `signal`.
  fn signal(&self, signal: rustix::process::Signal) {
    rustix::process::kill_process(rustix::process::Pid::from_child(&self.launcher), signal)
      .unwrap();
  }

  /// Waits for the launcher to end, within [`WAIT`]: its exit code, or the signal that ended it.
  fn ended(&mut self) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
      if let Some(status) = self.launcher.try_wait().unwrap() {
        return status;
      }
      assert!(
        started.elapsed() < WAIT,
        "the launcher ended within {WAIT:?}; its lines:\n{}",
        self.seen.join("\n")
      );
      vm::pause();
    }
  }
}

impl Drop for Launch {
  fn drop(&mut self) {
    let _ = self.launcher.kill();
    let _ = self.launcher.wait();
  }
}

/// The anchor, the volume holding the file of [`SEED`] bytes, and the SDK client that wrote it. Fields drop in order,
/// so the client goes before the anchor.
struct Rig {
  instance: String,
  volume: String,
  _sdk: Client,
  _anchor: Anchor,
}

impl Rig {
  fn new(tag: &str) -> Rig {
    let instance = format!("{tag}-{}", std::process::id());
    let anchor = start_anchor(&instance);
    let volume = created_volume(&instance, "pmem");
    let deadlines = slates_client::Deadlines::derive(
      slates_server::daemon::LIVENESS_BUDGET_NS,
      slates_db::replay::RECOVERY_BUDGET_NS,
    )
    .get();
    let mut sdk = Client::connect(&instance, deadlines).unwrap();
    let id = VolumeId {
      bytes: u128::from_str_radix(&volume, 16).unwrap().to_be_bytes(),
    };
    let writer = sdk.attach(id, None, Intent::Write).unwrap().attachment;
    sdk
      .fs_write((id, writer), FILE, &vec![SEED; FILE_BYTES], 0o644)
      .unwrap();
    Rig {
      instance,
      volume,
      _sdk: sdk,
      _anchor: anchor,
    }
  }

  /// The file as slates itself holds it, or `slates read`'s refusal.
  fn read(&self) -> Result<Vec<u8>, String> {
    let (code, held, err) = run(&self.instance, &["read", &self.volume, &format!("/{FILE}")]);
    if code == 0 { Ok(held) } else { Err(err) }
  }

  /// The file as slates itself holds it, when nothing else writes it.
  fn file(&self) -> Vec<u8> {
    self
      .read()
      .unwrap_or_else(|err| panic!("slates read: {err}"))
  }

  /// Waits, within [`WAIT`], until the file holds `byte` over `[at, at + count)`. A write landing during a read is
  /// refused by `slates read` (`READ_CHANGED`, "changed while it was read"), which here is a read to make again.
  fn holds_eventually(&self, (at, byte, count): (usize, u8, usize), what: &str) {
    let started = Instant::now();
    loop {
      match self.read() {
        Ok(held) if held[at..at + count].iter().all(|seen| *seen == byte) => return,
        Ok(_) => {}
        Err(err) if err.contains(READ_CHANGED) => {}
        Err(err) => panic!("slates read: {err}"),
      }
      assert!(started.elapsed() < WAIT, "{what} within {WAIT:?}");
      vm::pause();
    }
  }
}

/// The file the rig holds, with `byte` over `[at, at + count)` in each of `stores`.
fn expected(stores: &[(usize, u8, usize)]) -> Vec<u8> {
  let mut file = vec![SEED; FILE_BYTES];
  for (at, byte, count) in stores {
    file[*at..*at + *count].fill(*byte);
  }
  file
}

/// Step 6.1 by use. Do: run `slates pmem` on a 100 KiB file with the stand-in VMM; read the copy's length the launcher
/// announces and the one the VMM mapped and saw; have the VMM store a page at the file's start and send the launcher
/// SIGUSR1; read the file through slates; have it store a page further in and 100 bytes past the file's end, then exit.
/// Expect: the copy is 2 MiB, which the VMM maps and sees; the sync writes the first page and the file then holds it;
/// at the VMM's exit the launcher releases the map, writing the second page and nothing past the end, and exits 0;
/// the file holds both pages and keeps its 100 KiB.
#[test]
fn a_pmem_vmm_is_handed_a_writable_copy_compared_back_at_each_sync_and_at_its_exit() {
  let rig = Rig::new("pmem-sync");
  let mut launch = Launch::start(&rig.instance, &rig.volume);
  let length = launch.value("length");
  assert_eq!(length, GRANULE, "the copy is the file rounded up to 2 MiB");
  let mapped = launch.expect(ROLE_TAG);
  assert!(
    mapped.contains(&format!("mapped {GRANULE} sized {GRANULE}")),
    "the VMM maps and sees the copy's 2 MiB: {mapped}"
  );
  let page = usize::try_from(slates_client::page_bytes().unwrap()).unwrap();
  let first = (0, FIRST, page);
  launch.store(0, FIRST, u64::try_from(page).unwrap());
  launch.signal(rustix::process::Signal::USR1);
  assert_eq!(
    launch.value("synced"),
    u64::try_from(page).unwrap(),
    "the sync wrote the page"
  );
  assert!(
    rig.file() == expected(&[first]),
    "after the sync the file holds the first page"
  );
  let second = (2 * page, SECOND, page);
  launch.store(
    u64::try_from(2 * page).unwrap(),
    SECOND,
    u64::try_from(page).unwrap(),
  );
  launch.store(u64::try_from(FILE_BYTES).unwrap() + 100, BEYOND, 100);
  writeln!(launch.input, "exit 0").unwrap();
  launch.input.flush().unwrap();
  assert_eq!(
    launch.value("released"),
    u64::try_from(page).unwrap(),
    "the release wrote the second page and nothing past the end"
  );
  let status = launch.ended();
  assert!(
    status.success(),
    "the launcher exits with the VMM's 0: {status}"
  );
  assert!(
    rig.file() == expected(&[first, second]),
    "the file holds both pages and keeps its 100 KiB"
  );
}

/// Step 6.1: the launcher's death ends its VMM. Do: run `slates pmem` with the stand-in VMM, have it store a page, and
/// kill the launcher with SIGKILL. Expect: the VMM ends (Linux's parent-death signal), its process gone within the
/// wait; and the daemon, reaping the launcher's client, compares the copy back, so the file comes to hold the page.
#[test]
fn a_pmem_launchers_death_ends_its_vmm() {
  let rig = Rig::new("pmem-kill");
  let mut launch = Launch::start(&rig.instance, &rig.volume);
  let mapped = launch.expect(ROLE_TAG);
  let vmm: i32 = mapped
    .split_whitespace()
    .skip_while(|word| *word != "pid")
    .nth(1)
    .and_then(|pid| pid.parse().ok())
    .unwrap_or_else(|| panic!("no pid in `{mapped}`"));
  let page = usize::try_from(slates_client::page_bytes().unwrap()).unwrap();
  launch.store(0, FIRST, u64::try_from(page).unwrap());
  launch.signal(rustix::process::Signal::KILL);
  let _ = launch.ended();
  let started = Instant::now();
  while std::path::Path::new(&format!("/proc/{vmm}")).exists() {
    assert!(
      started.elapsed() < WAIT,
      "the VMM (process {vmm}) ended with its launcher within {WAIT:?}"
    );
    vm::pause();
  }
  rig.holds_eventually(
    (0, FIRST, page),
    "the daemon's reap of the launcher's client compared the copy back",
  );
}

/// Step 6.1: SIGTERM to the launcher reaches its VMM, and the VMM's stores reach the file after it ends. Do: run
/// `slates pmem` with the stand-in VMM, have it store a page, and send the launcher SIGTERM. Expect: the VMM ends by the
/// signal, the launcher releases the map, writing the page, and exits 143, the shell's status for a SIGTERM; the file
/// holds the page.
#[test]
fn a_pmem_launcher_passes_sigterm_to_its_vmm_and_compares_its_stores_back() {
  let rig = Rig::new("pmem-term");
  let mut launch = Launch::start(&rig.instance, &rig.volume);
  let _ = launch.expect(ROLE_TAG);
  let page = usize::try_from(slates_client::page_bytes().unwrap()).unwrap();
  launch.store(0, FIRST, u64::try_from(page).unwrap());
  launch.signal(rustix::process::Signal::TERM);
  assert_eq!(
    launch.value("released"),
    u64::try_from(page).unwrap(),
    "the release wrote the page"
  );
  let status = launch.ended();
  assert_eq!(
    status.code(),
    Some(ENDED_BY_SIGTERM),
    "the launcher exits as its VMM was ended: {status}"
  );
  assert!(
    rig.file() == expected(&[(0, FIRST, page)]),
    "the file holds the page"
  );
}
