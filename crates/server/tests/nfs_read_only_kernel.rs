//! A read-only attachment through the Linux kernel's own NFS clients (§4.6 "A read-only attachment is a read-only file
//! system"; GAPS, 2026-11-08): a volume's read attachment is mounted with `mount -t nfs -o vers=3` and `mount -t nfs4
//! -o vers=4.2`, with the conformance lane's options (`xtask/src/conformance/slates.rs`), and an existing file is opened
//! for writing through it, and a file is created beside it. The kernel decides a v3 open from the ACCESS reply alone (v3
//! has no OPEN), and a v4 open from the server's OPEN, so only a kernel mount shows what an application sees: the
//! bridge's own tests drive the procedures and never meet the kernel's open. Over v4 every refusal is `EROFS`. Over v3 an
//! open for writing is `EACCES`, the most a v3 server can say (A-260): Linux turns an ACCESS bit the server did not grant
//! into `-EACCES` (`fs/nfs/dir.c` v6.8, `nfs_do_access`, lines 3219–3221); a change that reaches the server, the create,
//! is `EROFS`.
//!
//! Found by conformance's read-only pjdfstest run (CI run 42498995934): `open(O_WRONLY)` and `open(O_RDWR)` of an
//! existing file succeeded over both versions, and v4's `open(O_RDONLY|O_TRUNC)` answered `EACCES`.
//!
//! Gated: Linux, root (or passwordless `sudo`) for `mount`, the `mount.nfs` helper, and `SLATES_TEST_NFS4_KERNEL=1`,
//! the kernel NFS lane's variable. Without any of these it skips loudly, printing why, and passes.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use slates_client::Intent;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Format: the variable that opts into the kernel NFS lane.
const ENV_OPT_IN: &str = "SLATES_TEST_NFS4_KERNEL";
/// Format: the conformance lane's NFSv3 options (`LINUX_NFS_OPTIONS`), its port and mount port appended.
const NFS3_OPTIONS: &str = "vers=3,tcp,nolock,noresvport,soft,timeo=10,retrans=2,actimeo=1";
/// Format: the conformance lane's NFSv4 options at minor version 2, its port appended.
const NFS4_OPTIONS: &str = "vers=4.2,proto=tcp,soft,timeo=10,retrans=2,noresvport,actimeo=1";
/// Format: POSIX `EROFS`, a read-only file system, on Linux.
const EROFS: i32 = 30;
/// Format: POSIX `EACCES`, permission denied, on Linux: an NFSv3 open for writing on a read-only attachment (A-260).
const EACCES: i32 = 13;
/// Format: the bytes the file holds, written through the volume's writable mount.
const FIRST: &[u8] = b"the file's first bytes";

/// Why the test cannot run here, or `None` when it can.
fn skip_reason() -> Option<String> {
  if !cfg!(target_os = "linux") {
    return Some("the kernel NFS clients under test are Linux's".to_owned());
  }
  if std::env::var_os(ENV_OPT_IN).is_none() {
    return Some(format!("{ENV_OPT_IN} is not set"));
  }
  if !Path::new("/sbin/mount.nfs").exists() && !Path::new("/usr/sbin/mount.nfs").exists() {
    return Some("the mount.nfs helper is not installed".to_owned());
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

/// A kernel mount, unmounted and its directory removed on drop.
struct KernelMount {
  path: PathBuf,
}

impl Drop for KernelMount {
  fn drop(&mut self) {
    let _ = privileged("umount").arg("-l").arg(&self.path).output();
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// Mounts the export `path` of the daemon serving NFS on `port` as file system `kind` with `options`, at a fresh
/// directory in the build output named `tag`.
fn kernel_mount(kind: &str, options: &str, port: u16, path: &str, tag: &str) -> KernelMount {
  let point =
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("nfsro-{tag}-{}", std::process::id()));
  #[allow(clippy::disallowed_methods)] // the mount point, in the build output
  std::fs::create_dir_all(&point).unwrap();
  let mount = KernelMount { path: point };
  let options = match kind {
    "nfs" => format!("{options},port={port},mountport={port}"),
    _ => format!("{options},port={port}"),
  };
  let output = privileged("mount")
    .args(["-t", kind, "-o", &options, &format!("127.0.0.1:{path}")])
    .arg(&mount.path)
    .output()
    .unwrap();
  let (_, capability) = path.rsplit_once('@').unwrap();
  assert!(
    output.status.success(),
    "mount -t {kind} -o {options} failed: {}",
    String::from_utf8_lossy(&output.stderr).replace(capability, "<capability>")
  );
  mount
}

/// The export path of a read-only host-mount attachment of the volume `name` in `instance`'s daemon.
fn read_only_export(instance: &str, name: &str) -> String {
  let mut client = common::client::connect(instance);
  let volume = client
    .list()
    .unwrap()
    .into_iter()
    .find(|summary| summary.name == name)
    .expect("the volume is listed")
    .id;
  let attachment = client.attach_mount(volume, Intent::Read).unwrap();
  let token = attachment
    .token
    .expect("a host mount's attachment carries its capability");
  let token_hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
  format!("/{name}@{:x}.{token_hex}", attachment.attachment)
}

/// The errno of each way of opening the existing `file` for writing (`O_WRONLY`, `O_RDWR`, `O_RDONLY|O_TRUNC`), then of
/// creating a file beside it. `None` for one that succeeded.
#[allow(clippy::disallowed_methods)] // opens through the kernel mount under test
fn opens_for_writing(file: &Path) -> [Option<i32>; 4] {
  let open =
    |options: &mut std::fs::OpenOptions| options.open(file).err().and_then(|e| e.raw_os_error());
  let beside = file.with_extension("new");
  [
    open(std::fs::OpenOptions::new().write(true)),
    open(std::fs::OpenOptions::new().read(true).write(true)),
    open(
      std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::TRUNC.bits().try_into().unwrap()),
    ),
    std::fs::write(&beside, FIRST)
      .err()
      .and_then(|e| e.raw_os_error()),
  ]
}

/// GAPS (2026-11-08), found by conformance's read-only run: a read-only attachment refuses an open for writing through
/// the kernel's own NFS clients. Do: write `f` through the volume's writable mount; mount a read attachment of the volume
/// over NFSv3 and over NFSv4.2; through each, open `f` with `O_WRONLY`, `O_RDWR` and `O_RDONLY|O_TRUNC`, and create a
/// file beside it. Expect over v4 each refused `EROFS`; over v3 each open refused `EACCES` (A-260, the v3 limit) and the
/// create `EROFS`; and a read-only open reading the first bytes. Before the fix both clients granted `O_WRONLY` and
/// `O_RDWR`, and v4 answered the truncation `EACCES` (diag-slabs-42, run 42517407649).
#[test]
fn a_read_only_attachment_refuses_an_open_for_writing_through_the_kernels_nfs_clients() {
  if let Some(reason) = skip_reason() {
    println!("nfs_read_only_kernel: skipped — {reason}");
    return;
  }
  let (daemon, instance) = daemon_with_volume("rofs", "rofs");
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let writable = daemon.mount_capability("rofs").unwrap().unwrap();
  {
    let mounted = kernel_mount("nfs4", NFS4_OPTIONS, port, &writable, "writable");
    #[allow(clippy::disallowed_methods)] // a write through the kernel mount under test
    std::fs::write(mounted.path.join("f"), FIRST).unwrap();
  }
  let read_only = read_only_export(&instance, "rofs");
  let mut found = Vec::new();
  for (kind, options) in [("nfs", NFS3_OPTIONS), ("nfs4", NFS4_OPTIONS)] {
    let mounted = kernel_mount(kind, options, port, &read_only, kind);
    let file = mounted.path.join("f");
    found.push((kind, opens_for_writing(&file)));
    #[allow(clippy::disallowed_methods)] // a read through the kernel mount under test
    let read = std::fs::read(&file);
    assert_eq!(
      read.ok().as_deref(),
      Some(FIRST),
      "{kind}: a read-only open reads the file"
    );
  }
  assert_eq!(
    found,
    vec![
      (
        "nfs",
        [Some(EACCES), Some(EACCES), Some(EACCES), Some(EROFS)]
      ),
      ("nfs4", [Some(EROFS); 4])
    ],
    "O_WRONLY, O_RDWR, O_RDONLY|O_TRUNC, then a create: v4 refuses each EROFS; v3 refuses each open EACCES, its limit, \
     and the create EROFS"
  );
  daemon.stop();
}

/// A one-shard daemon serving NFS with a 4 MiB volume `name`, and its instance.
fn daemon_with_volume(tag: &str, name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", tag),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let mut client = common::client::connect(&instance);
  client.create(&common::client::scratch(name)).unwrap();
  (daemon, instance)
}
