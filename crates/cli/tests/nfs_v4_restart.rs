//! NFSv4 state survives a daemon restart (§4.6 A-37), end to end: the real `slates anchor` supervises
//! the daemon and holds its NFS listener and segment; the Linux kernel's NFSv4.2 client mounts a
//! volume; one process holds a file open and locked; the daemon is killed with `SIGKILL` and the
//! anchor starts a new one. The open descriptor must go on reading and writing (no `EIO`), the lock
//! must still bind (another open file's `flock` is refused), and the file must open afresh — the
//! client's id and state ids were kept in the partitions, so no grace period and no reclaim.
//!
//! Gated: Linux, root (or passwordless `sudo`) for `mount`, the `mount.nfs4` helper, a RAM-backed
//! `SLATES_TEST_RAMDIR`, and `SLATES_TEST_NFS4_KERNEL=1`; otherwise it skips loudly and passes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(target_os = "linux")]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: how long the daemon may take to come up, and to be replaced after a kill.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: the poll interval of the waits.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answered verbs before the daemon counts as settled (the anchor may restart it
/// once at startup, cli.rs `start_anchor_with_environment`).
const STABLE_STREAK: u32 = 10;

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

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

fn pause() {
  #[allow(clippy::disallowed_methods)] // the test paces its polls
  std::thread::sleep(POLL);
}

fn skip_reason() -> Option<String> {
  if std::env::var_os("SLATES_TEST_NFS4_KERNEL").is_none() {
    return Some("SLATES_TEST_NFS4_KERNEL is not set".to_owned());
  }
  if std::env::var_os("SLATES_TEST_RAMDIR").is_none() {
    return Some("SLATES_TEST_RAMDIR is not set (name a RAM-backed directory)".to_owned());
  }
  if !Path::new("/sbin/mount.nfs4").exists() && !Path::new("/usr/sbin/mount.nfs4").exists() {
    return Some("the mount.nfs4 helper is not installed".to_owned());
  }
  None
}

fn privileged(program: &str) -> Command {
  if rustix::process::geteuid().is_root() {
    Command::new(program)
  } else {
    let mut command = Command::new("sudo");
    command.args(["-n", program]);
    command
  }
}

/// The anchor, killed and reaped on drop, so a failed assertion leaves no daemon behind.
struct Anchor(Child);

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

/// A kernel mount, lazily unmounted and its directory removed on drop.
struct KernelMount(PathBuf);

impl Drop for KernelMount {
  fn drop(&mut self) {
    let _ = privileged("umount").arg("-l").arg(&self.0).output();
    let _ = Command::new("rmdir").arg(&self.0).output();
  }
}

fn start_anchor(instance: &str) -> Anchor {
  let child = slates()
    .args(["--instance", instance, "anchor", "--quick", "--shards", "2"])
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
  let anchor = Anchor(child);
  let started = Instant::now();
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if run(instance, &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    assert!(started.elapsed() < START_WAIT, "the daemon came up");
    pause();
  }
  let (code, _, error) = run(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap: {error}");
  anchor
}

/// §4.6 A-37: a file held open and locked through the kernel's NFSv4.2 client keeps working across a
/// `SIGKILL` of the daemon under its anchor.
#[test]
fn nfsv4_opens_and_locks_survive_a_daemon_restart() {
  if let Some(reason) = skip_reason() {
    println!("nfs_v4_restart: skipped — {reason}");
    return;
  }
  let instance = format!("nfs4-restart-{}", std::process::id());
  let _anchor = start_anchor(&instance);
  let (code, out, error) = run(
    &instance,
    &["volume", "create", "held", "--bounded", "16MiB"],
  );
  assert_eq!(code, 0, "create: {error}");
  let id = out
    .lines()
    .find_map(|line| line.strip_prefix("id: "))
    .unwrap()
    .to_owned();
  let volume = slates_client::VolumeId {
    bytes: u128::from_str_radix(&id, 16).unwrap().to_be_bytes(),
  };
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let mut client = slates_client::Client::connect(&instance, deadlines).unwrap();
  let attached = client
    .attach_mount(volume, slates_client::Intent::Write)
    .unwrap();
  let token: String = attached
    .token
    .unwrap()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let capability = format!("{:x}.{token}", attached.attachment);
  let port = client.status(volume).unwrap().nfs_port.unwrap();

  let point = PathBuf::from(std::env::var_os("SLATES_TEST_RAMDIR").unwrap())
    .join(format!("nfs4-restart-{}", std::process::id()));
  #[allow(clippy::disallowed_methods)] // the mount point, in the RAM test directory
  std::fs::create_dir_all(&point).unwrap();
  let mount = KernelMount(point);
  let options = format!("vers=4.2,proto=tcp,port={port},hard,timeo=10");
  let source = format!("127.0.0.1:/held@{capability}");
  let output = privileged("mount")
    .args(["-t", "nfs4", "-o", &options, &source])
    .arg(&mount.0)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "mount failed: {}",
    String::from_utf8_lossy(&output.stderr).replace(&capability, "<capability>")
  );

  held_file_survives_restart(&mut client, &mount.0);
}

/// Opens and locks a file, restarts the daemon, and checks the open and the lock carried over.
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn held_file_survives_restart(client: &mut slates_client::Client, root: &Path) {
  use rustix::fs::{FlockOperation, flock};
  let path = root.join("journal");
  let mut held = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .create_new(true)
    .open(&path)
    .unwrap();
  held.write_all(b"before;").unwrap();
  held.sync_all().unwrap();
  flock(&held, FlockOperation::NonBlockingLockExclusive).expect("the lock is granted");

  let previous = client.daemon_status().unwrap().pid;
  let killed = Command::new("kill")
    .args(["-KILL", &previous.to_string()])
    .status()
    .unwrap();
  assert!(killed.success(), "kill the daemon");
  let started = Instant::now();
  while client.daemon_status().map(|status| status.pid).ok() == Some(previous)
    || client.daemon_status().is_err()
  {
    assert!(
      started.elapsed() < START_WAIT,
      "the anchor replaced the daemon"
    );
    pause();
  }

  // The descriptor opened before the restart goes on writing and reading: its state id was kept.
  held.write_all(b"after;").unwrap();
  held.sync_all().unwrap();
  held.seek(SeekFrom::Start(0)).unwrap();
  let mut contents = String::new();
  held.read_to_string(&mut contents).unwrap();
  assert_eq!(
    contents, "before;after;",
    "the held file read back across the restart"
  );

  // The lock was kept: another open file (another lock-owner) is refused until it is released.
  let other = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&path)
    .unwrap();
  assert_eq!(
    flock(&other, FlockOperation::NonBlockingLockExclusive),
    Err(rustix::io::Errno::WOULDBLOCK),
    "the lock held before the restart still binds"
  );
  flock(&held, FlockOperation::Unlock).unwrap();
  flock(&other, FlockOperation::NonBlockingLockExclusive).expect("granted after the unlock");
  flock(&other, FlockOperation::Unlock).unwrap();
  drop((held, other));
  std::fs::remove_file(&path).unwrap();
}
