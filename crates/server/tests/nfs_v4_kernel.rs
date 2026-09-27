//! The Linux kernel's own NFSv4 client against the daemon (§4.6 A-35): a volume provisioned in an
//! in-process daemon is mounted with `mount -t nfs4 -o vers=4.1` and `vers=4.2` at a directory under
//! the RAM test directory, driven through ordinary file calls — create, write, read, append, mkdir,
//! rename, symlink, hard link, truncate, list, remove — and read back over NFSv3 from the daemon, so the
//! kernel's compounds are proved to land in the volume, not only to succeed.
//!
//! Gated: it needs Linux, root (or passwordless `sudo`) for `mount`, the `mount.nfs4` helper, and a
//! RAM-backed `SLATES_TEST_RAMDIR`; set `SLATES_TEST_NFS4_KERNEL=1` to run it. Without any of these it
//! skips loudly, printing why, and passes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use slates_ipc::protocol::{
  Direction, NamePolicy, ReplyBody, RequestBody, SizeClass, pack, unpack,
};
use slates_ipc::{ClientEnd, connect};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;
use common::nfs::{lookup, mount, read};

/// Format: the variable that opts into this test.
const ENV_OPT_IN: &str = "SLATES_TEST_NFS4_KERNEL";
/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long the client waits for the daemon's rendezvous.
const CREDIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why the test cannot run here, or `None` when it can.
fn skip_reason() -> Option<String> {
  if !cfg!(target_os = "linux") {
    return Some("the kernel NFSv4 client under test is Linux's".to_owned());
  }
  if std::env::var_os(ENV_OPT_IN).is_none() {
    return Some(format!("{ENV_OPT_IN} is not set"));
  }
  if std::env::var_os("SLATES_TEST_RAMDIR").is_none() {
    return Some("SLATES_TEST_RAMDIR is not set (name a RAM-backed directory)".to_owned());
  }
  if !Path::new("/sbin/mount.nfs4").exists() && !Path::new("/usr/sbin/mount.nfs4").exists() {
    return Some("the mount.nfs4 helper is not installed".to_owned());
  }
  None
}

/// `mount`/`umount` as root: directly when this process is root, else through `sudo -n`.
fn privileged(program: &str) -> Command {
  if rustix::process::geteuid().is_root() {
    Command::new(program)
  } else {
    let mut command = Command::new("sudo");
    command.args(["-n", program]);
    command
  }
}

/// A kernel mount: unmounted and its directory removed on drop, so a failed assertion leaves nothing.
struct KernelMount {
  path: PathBuf,
}

impl Drop for KernelMount {
  fn drop(&mut self) {
    let _ = privileged("umount").arg("-l").arg(&self.path).output();
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// A one-shard daemon with the volume `name` provisioned, and its instance.
fn daemon_with_volume(tag: &str, name: &str) -> Daemon {
  let profile = common::machine_profile();
  let instance = format!("srv-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{tag}"),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let started = std::time::Instant::now();
  let connected = loop {
    match connect(&instance) {
      Ok(connected) => break connected,
      Err(slates_ipc::IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
        std::hint::spin_loop();
      }
      Err(error) => panic!("{error}"),
    }
  };
  let client = connected.region.client_id();
  let mut end = ClientEnd::connected(connected);
  let body = RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 24 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  };
  let id = RequestId {
    client,
    sequence: 1,
  };
  let index = end.next_request_index();
  let slot = pack(
    end.region_mut(),
    Direction::Request,
    index,
    id.word(),
    &body,
  )
  .unwrap();
  end.send(&slot).unwrap();
  let reply = end.wait(Some(DEADLINE_NS)).unwrap();
  let created = unpack(end.region(), reply.kind, &reply.payload).unwrap();
  assert!(matches!(created, ReplyBody::Created { .. }), "{created:?}");
  daemon
}

/// Mounts `source` (`127.0.0.1:/<name>@<capability>`) with the kernel's NFSv4 client at minor version
/// `minor`, at a fresh directory under the RAM test directory.
fn kernel_mount(source: &str, port: u16, minor: u32) -> KernelMount {
  let base = PathBuf::from(std::env::var_os("SLATES_TEST_RAMDIR").unwrap());
  let path = base.join(format!("nfs4-{minor}-{}", std::process::id()));
  #[allow(clippy::disallowed_methods)] // the mount point, inside the RAM test directory
  std::fs::create_dir_all(&path).unwrap();
  let mount = KernelMount { path };
  // Byte-range locks are kept by the client (`local_lock=all`) until the server offers them (A-35).
  let options =
    format!("vers=4.{minor},proto=tcp,port={port},soft,timeo=10,retrans=2,local_lock=all");
  let output = privileged("mount")
    .args(["-t", "nfs4", "-o", &options, source])
    .arg(&mount.path)
    .output()
    .unwrap();
  let (_, capability) = source.rsplit_once('@').unwrap();
  assert!(
    output.status.success(),
    "mount -t nfs4 -o {options} failed: {}{}",
    String::from_utf8_lossy(&output.stdout).replace(capability, "<capability>"),
    String::from_utf8_lossy(&output.stderr).replace(capability, "<capability>")
  );
  // Non-vacuity: the kernel's own table names the version it negotiated, so a mount that fell back to
  // another version cannot pass as this one.
  #[allow(clippy::disallowed_methods)] // the kernel's mount table, read-only
  let table = std::fs::read_to_string("/proc/self/mounts").unwrap();
  let line = table
    .lines()
    .find(|line| line.split_whitespace().nth(1) == mount.path.to_str())
    .expect("the mount is in the kernel's table");
  assert!(
    line.split_whitespace().nth(2) == Some("nfs4") && line.contains(&format!("vers=4.{minor}")),
    "the kernel negotiated NFSv4.{minor}: {}",
    line.replace(capability, "<capability>")
  );
  mount
}

/// Drives the ordinary file calls a workload makes through the mount at `root`, checking each result
/// through the same mount.
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn exercise(root: &Path, payload: &[u8]) {
  use std::io::Write;
  let file = root.join("kernel.txt");
  std::fs::write(&file, payload).unwrap();
  assert_eq!(
    std::fs::read(&file).unwrap(),
    payload,
    "read back what was written"
  );
  let mut appended = std::fs::OpenOptions::new()
    .append(true)
    .open(&file)
    .unwrap();
  appended.write_all(b"tail\n").unwrap();
  drop(appended);
  let mut expected = payload.to_vec();
  expected.extend_from_slice(b"tail\n");
  assert_eq!(std::fs::read(&file).unwrap(), expected, "the append landed");

  std::fs::create_dir(root.join("dir")).unwrap();
  std::fs::rename(root.join("kernel.txt"), root.join("dir/moved.txt")).unwrap();
  std::fs::rename(root.join("dir/moved.txt"), root.join("kernel.txt")).unwrap();
  std::os::unix::fs::symlink("kernel.txt", root.join("link")).unwrap();
  assert_eq!(
    std::fs::read_link(root.join("link")).unwrap(),
    Path::new("kernel.txt")
  );
  std::fs::hard_link(root.join("kernel.txt"), root.join("hard")).unwrap();
  assert_eq!(std::fs::read(root.join("hard")).unwrap(), expected);
  std::fs::remove_file(root.join("hard")).unwrap();

  let truncated = std::fs::OpenOptions::new()
    .write(true)
    .open(root.join("link"))
    .unwrap();
  truncated.set_len(3).unwrap();
  drop(truncated);
  assert_eq!(
    std::fs::metadata(&file).unwrap().len(),
    3,
    "truncated through the link"
  );
  std::fs::write(&file, payload).unwrap();

  let mut names: Vec<String> = std::fs::read_dir(root)
    .unwrap()
    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
    .collect();
  names.sort();
  assert_eq!(names, ["dir", "kernel.txt", "link"], "the listing");
  std::fs::remove_dir(root.join("dir")).unwrap();
  std::fs::remove_file(root.join("link")).unwrap();
}

/// §4.6 A-35: the Linux kernel's NFSv4.1 and NFSv4.2 clients mount a daemon volume through its
/// capability and run ordinary file calls through it; NFSv3 reads what the kernel wrote.
#[test]
fn the_linux_kernel_nfsv4_client_mounts_and_works_a_volume() {
  if let Some(reason) = skip_reason() {
    println!("nfs_v4_kernel: skipped — {reason}");
    return;
  }
  for minor in [1u32, 2] {
    let name = format!("k4v{minor}");
    let daemon = daemon_with_volume(&format!("nfs4k{minor}"), &name);
    let port = daemon.nfs_port().expect("the daemon is serving NFS");
    let path = daemon.mount_capability(&name).unwrap().unwrap();
    let source = format!("127.0.0.1:{path}");
    let payload = format!("written by the Linux NFSv4.{minor} client\n").into_bytes();
    {
      let mounted = kernel_mount(&source, port, minor);
      exercise(&mounted.path, &payload);
    }
    let mut v3 = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let root = mount(&mut v3, &path, 1);
    let file = lookup(&mut v3, &root, "kernel.txt", 2);
    assert_eq!(
      read(&mut v3, &file, 3),
      payload,
      "NFSv3 reads what the v4.{minor} kernel client wrote"
    );
    drop(v3);
    daemon.stop();
  }
}
