//! The Linux kernel's own NFSv4 client against the daemon (§4.6 A-35): a volume provisioned in an
//! in-process daemon is mounted with `mount -t nfs4 -o vers=4.1` and `vers=4.2` at a directory in
//! the build output (A-50: never `/tmp`, never a RAM directory), driven through ordinary file calls — create, write, read, append, mkdir,
//! rename, symlink, hard link, truncate, list, remove, and `flock` between two open files (the server's
//! LOCK, LOCKT and LOCKU), and on 4.2 `lseek(SEEK_HOLE/SEEK_DATA)` and `copy_file_range` (the server's
//! SEEK and COPY) and `user.` extended attributes (RFC 8276) — and read back over NFSv3 from the daemon, so the
//! kernel's compounds are proved to land in the volume, not only to succeed.
//!
//! Gated: it needs Linux, root (or passwordless `sudo`) for `mount`, and the `mount.nfs4` helper;
//! set `SLATES_TEST_NFS4_KERNEL=1` to run it. Without any of these it
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

/// `program` as root (through [`privileged`]) with its real and effective ids dropped to `uid`/`gid`
/// and no supplementary groups: a real non-root process, whatever this test runs as.
#[cfg(target_os = "linux")]
fn as_ids(uid: u32, gid: u32, program: &str) -> Command {
  let mut command = privileged("setpriv");
  command.args([
    "--reuid",
    &uid.to_string(),
    "--regid",
    &gid.to_string(),
    "--clear-groups",
    program,
  ]);
  command
}

/// A `chown uid:gid` of `path` through the kernel client by root: the caller POSIX lets give a file
/// away. Asserts it succeeded.
fn chown_as_root(path: &Path, uid: u32, gid: u32) {
  let output = privileged("chown")
    .arg(format!("{uid}:{gid}"))
    .arg(path)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "chown {uid}:{gid} as root: {}",
    String::from_utf8_lossy(&output.stderr)
  );
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
/// `minor`, at a fresh directory in the build output.
fn kernel_mount(source: &str, port: u16, minor: u32) -> KernelMount {
  kernel_mount_with(source, port, minor, "")
}

/// Format: the conformance lane's further NFSv4 options (`xtask/src/conformance/slates.rs` `LINUX_NFS4_OPTIONS`): a
/// high source port and a one-second attribute cache, under which pjdfstest runs.
#[cfg(target_os = "linux")]
const CONFORMANCE_OPTIONS: &str = ",noresvport,actimeo=1";

/// [`kernel_mount`] with `extra` appended to the options (each beginning with a comma), at a directory of its own.
fn kernel_mount_with(source: &str, port: u16, minor: u32, extra: &str) -> KernelMount {
  let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
  let path = base.join(format!(
    "nfs4-{minor}-{}{}",
    std::process::id(),
    if extra.is_empty() { "" } else { "-lane" }
  ));
  #[allow(clippy::disallowed_methods)] // the mount point, in the build output
  std::fs::create_dir_all(&path).unwrap();
  let mount = KernelMount { path };
  // Locks go to the server (the default `local_lock=none`): LOCK, LOCKT and LOCKU are served (A-35).
  let options = format!("vers=4.{minor},proto=tcp,port={port},soft,timeo=10,retrans=2{extra}");
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
  owners_change_as_numbers(&root.join("dir"));
  std::fs::remove_dir(root.join("dir")).unwrap();
  std::fs::remove_file(root.join("link")).unwrap();
  locks_conflict_across_open_files(&file);
  owners_change_as_numbers(&file);
  exclusive_create_keeps_its_mode(root);
  #[cfg(target_os = "linux")]
  truncating_open_needs_write_permission(root);
  #[cfg(target_os = "linux")]
  special_names(root);
  explicit_times(&file);
}

/// An `O_EXCL` create through the kernel client (an EXCLUSIVE4_1 OPEN) makes the file with the mode
/// asked: the client sends in the create only the attributes `suppattr_exclcreat` names, and sets no
/// mode afterwards, so a server naming none left every exclusive create at its default mode (git's
/// hook templates, 0755, arrived 0644 — the workload differential over NFSv4.2). The verifier the
/// server keeps in the times is replaced by the client's own times.
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn exclusive_create_keeps_its_mode(root: &Path) {
  use std::os::unix::fs::OpenOptionsExt;
  let path = root.join("exclusive");
  let umask = rustix::process::umask(rustix::fs::Mode::empty());
  rustix::process::umask(umask);
  let started = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  let created = std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .mode(0o755)
    .open(&path)
    .expect("an exclusive create");
  drop(created);
  // `Mode` and `st_mode` share the platform's mode width, so the comparison needs no conversion.
  let expected = (rustix::fs::Mode::from_raw_mode(0o755) & !umask).as_raw_mode();
  assert_eq!(
    rustix::fs::stat(&path).unwrap().st_mode & 0o7777,
    expected,
    "the exclusive create's mode (less the umask)"
  );
  // The server kept the create's verifier in the times (RFC 8881 §18.16.3); the client then set the
  // real ones, so the file's modification time is this create's, never the verifier's.
  let modified = rustix::fs::stat(&path).unwrap().st_mtime;
  assert!(
    u64::try_from(modified).unwrap() >= started,
    "the exclusive create's times are its own ({modified} before {started})"
  );
  std::fs::remove_file(&path).unwrap();
}

/// An `O_RDONLY|O_TRUNC` open by a caller whose class may read but not write is refused `EACCES` and
/// truncates nothing (POSIX `open`; pjdfstest `open/07.t`), through the kernel client as real non-root
/// processes: the owner of a file mode 0477 — whose truncate the server once let through, the kernel
/// sending it as a SETATTR under the read-only open's state id (RFC 8881 §9.1.2: `NFS4ERR_OPENMODE`) —
/// and a member of the file's group, mode 0747.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn truncating_open_needs_write_permission(root: &Path) {
  use std::os::unix::fs::PermissionsExt;
  /// Format: the owner, a group member, and `EACCES` as the child's exit status.
  const OWNER: u32 = 65534;
  const MEMBER: u32 = 65533;
  const EACCES: i32 = 13;
  for (uid, mode) in [(OWNER, 0o477), (MEMBER, 0o747)] {
    let path = root.join(format!("guarded-{uid}"));
    // Made by this test's caller, who sets the mode as its owner; then given to `OWNER` by root (a
    // gift of ownership is root's alone, and this test need not run as root).
    std::fs::write(&path, b"x").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    chown_as_root(&path, OWNER, OWNER);
    // A child as `uid` opens the file O_RDONLY|O_TRUNC and exits with the errno (0 on success).
    let status = as_ids(uid, OWNER, "perl")
      .args([
        "-MFcntl",
        "-e",
        "sysopen(my $f, $ARGV[0], O_RDONLY | O_TRUNC) ? exit(0) : exit($! + 0)",
      ])
      .arg(&path)
      .status()
      .unwrap();
    assert_eq!(
      status.code(),
      Some(EACCES),
      "uid {uid}, mode {mode:o}: a truncating open without write permission is refused"
    );
    assert_eq!(
      std::fs::metadata(&path).unwrap().len(),
      1,
      "uid {uid}, mode {mode:o}: nothing was truncated"
    );
    std::fs::remove_file(&path).unwrap();
  }
}

/// A child as `uid`/`gid` running `script` (perl, `Fcntl` loaded) on `path`; its exit status is the errno of the
/// first refused call, 0 when every call succeeded.
#[cfg(target_os = "linux")]
fn perl_as(uid: u32, gid: u32, script: &str, path: &Path) -> Option<i32> {
  as_ids(uid, gid, "perl")
    .args(["-MFcntl", "-e", script])
    .arg(path)
    .status()
    .unwrap()
    .code()
}

/// Format: pjdfstest's two users and `EACCES`.
#[cfg(target_os = "linux")]
const OPEN07_OWNER: u32 = 65534;
#[cfg(target_os = "linux")]
const OPEN07_OTHER: u32 = 65533;
#[cfg(target_os = "linux")]
const OPEN07_EACCES: i32 = 13;

/// One run of pjdfstest `open/07.t`'s sequence in a fresh directory `dir`: the owner creates its file and writes one
/// byte, then for each (mode, caller) the owner sets the mode and the caller opens `O_RDONLY|O_TRUNC`. Returns every
/// step that did not do what POSIX requires, empty when all did.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn open07_departures(dir: &Path) -> Vec<String> {
  std::fs::create_dir(dir).unwrap();
  chown_as_root(dir, OPEN07_OWNER, OPEN07_OWNER);
  let file = dir.join("n1");
  let made = perl_as(
    OPEN07_OWNER,
    OPEN07_OWNER,
    "sysopen(my $f, $ARGV[0], O_WRONLY | O_CREAT | O_EXCL, 0644) or exit($! + 0); syswrite($f, 'x') == 1 or exit($! + 0); close($f) or exit($! + 0); exit(0)",
    &file,
  );
  if made != Some(0) {
    return vec![format!("the owner's create and write: {made:?}")];
  }
  let mut departures = Vec::new();
  for (mode, uid, gid) in [
    (0o477, OPEN07_OWNER, OPEN07_OWNER),
    (0o747, OPEN07_OTHER, OPEN07_OWNER),
    (0o774, OPEN07_OTHER, OPEN07_OTHER),
  ] {
    let chmod = format!("chmod({mode:#o}, $ARGV[0]) ? exit(0) : exit($! + 0)");
    let set = perl_as(OPEN07_OWNER, OPEN07_OWNER, &chmod, &file);
    let opened = perl_as(
      uid,
      gid,
      "sysopen(my $f, $ARGV[0], O_RDONLY | O_TRUNC) ? exit(0) : exit($! + 0)",
      &file,
    );
    let kept = perl_as(
      OPEN07_OWNER,
      OPEN07_OWNER,
      "exit((stat($ARGV[0]))[7] == 1 ? 0 : 1)",
      &file,
    );
    if set != Some(0) || opened != Some(OPEN07_EACCES) || kept != Some(0) {
      departures.push(format!(
        "mode {mode:o}, uid {uid} gid {gid}: chmod {set:?}, O_RDONLY|O_TRUNC {opened:?} (EACCES due), one byte kept {}",
        kept == Some(0)
      ));
    }
  }
  departures
}

/// The kernel's NFSv4 client trace (its `nfs4` tracepoints, each operation with its state id and status) over
/// `run`, with the kernel's release: the evidence a departure on another kernel is diagnosed from (tracefs is the
/// kernel's own interface, nothing on disk; mounted first where it is not, as in a container). Turned off again
/// before it returns.
#[cfg(target_os = "linux")]
fn traced<T>(run: impl FnOnce() -> T) -> (T, String) {
  /// Format: the tracefs mount the kernel documents (`Documentation/trace/ftrace.rst`).
  const TRACING: &str = "/sys/kernel/tracing";
  let switch = |on: &str| {
    privileged("sh")
      .args([
        "-c",
        &format!(
          "{{ [ -d {TRACING}/events ] || mount -t tracefs nodev {TRACING}; }} && echo {on} > {TRACING}/events/nfs4/enable && echo {on} > {TRACING}/events/nfs/enable && echo {on} > {TRACING}/tracing_on && : > {TRACING}/trace"
        ),
      ])
      .status()
      .map(|status| status.success())
      .unwrap_or(false)
  };
  let switched = switch("1");
  let result = run();
  let trace = privileged("cat")
    .arg(format!("{TRACING}/trace"))
    .output()
    .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    .unwrap_or_default();
  switch("0");
  let release = Command::new("uname")
    .arg("-r")
    .output()
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .unwrap_or_default();
  (
    result,
    format!("kernel {release}; tracing switched on: {switched}\n{trace}"),
  )
}

/// pjdfstest `open/07.t`, step for step, through the kernel client as the users it names. Its owner creates the
/// file in a directory it owns, writes one byte through its own open, and changes the mode itself, so the client may
/// hold a delegation over the file as on CI's NFSv4.2 lane. Do: for each of 0477 (the owner), 0747 (a member of the
/// file's group) and 0774 (another user), set the mode as the owner, then open `O_RDONLY|O_TRUNC` as that caller.
/// Expect: `EACCES` each time and the file still one byte. CI (2026-10-05, 10-06) saw 0747 and 0774 open and
/// truncate on Ubuntu 24.04's client while Linux 6.12 refused them; a departure is re-run under the kernel's NFSv4
/// trace and reported with it.
#[cfg(target_os = "linux")]
fn the_owners_own_file_refuses_a_truncating_open_without_write_permission(root: &Path, tag: &str) {
  let departures = open07_departures(&root.join(format!("open07-{tag}")));
  if departures.is_empty() {
    return;
  }
  let (again, trace) = traced(|| open07_departures(&root.join(format!("open07-{tag}-traced"))));
  panic!("open/07 departures ({tag} mount): {departures:#?}\nunder the trace: {again:#?}\n{trace}");
}

/// `mkfifo` and a UNIX socket's `bind` create their names through the kernel client (NFSv4 CREATE of
/// `NF4FIFO` and `NF4SOCK`, §4.6 A-26) and read back as those kinds; a block or character device is
/// refused (A-26 keeps no device nodes) without creating a name.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn special_names(root: &Path) {
  use std::os::unix::fs::FileTypeExt;
  let fifo = root.join("fifo");
  rustix::fs::mkfifoat(
    rustix::fs::CWD,
    &fifo,
    rustix::fs::Mode::from_raw_mode(0o640),
  )
  .expect("mkfifo");
  let metadata = std::fs::symlink_metadata(&fifo).unwrap();
  assert!(metadata.file_type().is_fifo(), "a FIFO reads back as one");
  assert_eq!(
    std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o7777,
    0o640,
    "the FIFO's mode was set at creation"
  );
  std::fs::remove_file(&fifo).unwrap();

  let socket_path = root.join("socket");
  let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
  assert!(
    std::fs::symlink_metadata(&socket_path)
      .unwrap()
      .file_type()
      .is_socket(),
    "a socket name reads back as one"
  );
  drop(listener);
  std::fs::remove_file(&socket_path).unwrap();

  // As root, so the local kernel's own `CAP_MKNOD` check passes and the refusal is the server's.
  let device = root.join("device");
  let refused = privileged("mknod")
    .arg(&device)
    .args(["c", "1", "3"])
    .output()
    .unwrap();
  assert!(!refused.status.success(), "a device node is refused (A-26)");
  assert!(
    std::fs::symlink_metadata(&device).is_err(),
    "a refused device leaves no name"
  );
}

/// `utimensat` with explicit times through the kernel client, including times past 2038 and 2106:
/// the access and modification times set are the ones read back (the server advertises `time_access_set` and `time_modify_set`, without
/// which the client drops the times, RFC 8881 §5.8.2.37 and §5.8.2.43).
fn explicit_times(file: &Path) {
  use std::os::unix::fs::MetadataExt;
  let times = rustix::fs::Timestamps {
    last_access: rustix::fs::Timespec {
      tv_sec: 1_000_000_000,
      tv_nsec: 123_000_000,
    },
    last_modification: rustix::fs::Timespec {
      tv_sec: 1_100_000_000,
      tv_nsec: 456_000_000,
    },
  };
  #[allow(clippy::disallowed_methods)]
  // a slates volume through the kernel mount under test, not host disk
  rustix::fs::utimensat(rustix::fs::CWD, file, &times, rustix::fs::AtFlags::empty())
    .expect("utimensat");
  #[allow(clippy::disallowed_methods)] // the attribute read through the kernel mount under test
  let metadata = std::fs::metadata(file).unwrap();
  assert_eq!(
    (metadata.atime(), metadata.atime_nsec()),
    (1_000_000_000, 123_000_000),
    "the access time set"
  );
  assert_eq!(
    (metadata.mtime(), metadata.mtime_nsec()),
    (1_100_000_000, 456_000_000),
    "the modification time set"
  );
  // pjdfstest `utimensat/09.t`'s probe: 2^31 and 2^32 seconds, which NFSv4's 64-bit `nfstime4`
  // carries (NFSv3's 32-bit `nfstime3` cannot).
  let late = rustix::fs::Timestamps {
    last_access: rustix::fs::Timespec {
      tv_sec: 1 << 31,
      tv_nsec: 0,
    },
    last_modification: rustix::fs::Timespec {
      tv_sec: 1 << 32,
      tv_nsec: 0,
    },
  };
  #[allow(clippy::disallowed_methods)]
  // a slates volume through the kernel mount under test, not host disk
  rustix::fs::utimensat(rustix::fs::CWD, file, &late, rustix::fs::AtFlags::empty())
    .expect("utimensat past 2038 and 2106");
  let stat = rustix::fs::stat(file).unwrap();
  assert_eq!(
    (stat.st_atime, stat.st_mtime),
    (1 << 31, 1 << 32),
    "times past 2038 and 2106 read back"
  );
}

/// Shape: a sequential file of one-MiB writes: each past any one transfer, so the kernel client splits
/// it into WRITEs of its negotiated size (the NFS bench found such writes refused `EINVAL`).
const SEQUENTIAL_CHUNK: usize = 1 << 20;
/// Shape: the sequential file's writes: 8 MiB, half the test volume's 16 MiB bound.
const SEQUENTIAL_CHUNKS: usize = 8;

/// Large sequential writes through the kernel client: one-MiB writes, `fsync`ed, read back byte for
/// byte, as a build or a copy writes a file.
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn large_sequential_writes_round_trip(root: &Path) {
  use std::io::{Read, Write};
  let path = root.join("sequential");
  let mut file = std::fs::File::create(&path).unwrap();
  for index in 0..SEQUENTIAL_CHUNKS {
    let chunk = vec![u8::try_from(index % 251).unwrap(); SEQUENTIAL_CHUNK];
    file
      .write_all(&chunk)
      .unwrap_or_else(|error| panic!("write {index}: {error}"));
  }
  file.sync_all().expect("fsync of the sequential file");
  drop(file);
  let mut read = Vec::new();
  std::fs::File::open(&path)
    .unwrap()
    .read_to_end(&mut read)
    .unwrap();
  assert_eq!(
    read.len(),
    SEQUENTIAL_CHUNK * SEQUENTIAL_CHUNKS,
    "the whole file"
  );
  for (index, chunk) in read.chunks(SEQUENTIAL_CHUNK).enumerate() {
    assert!(
      chunk.iter().all(|byte| usize::from(*byte) == index % 251),
      "chunk {index} read back"
    );
  }
  std::fs::remove_file(&path).unwrap();
}

/// `chown` of a file or a directory through the kernel client by root: the owner and group set are the
/// ones read back, for ids with a passwd entry (root) and without, and the server never refuses an
/// owner (a refused owner turns the client's numeric ids into names for the rest of the mount, RFC 8881
/// §5.9). The object is handed back to its owner, so this test's caller keeps working with it.
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn owners_change_as_numbers(file: &Path) {
  use std::os::unix::fs::MetadataExt;
  let before = std::fs::metadata(file).unwrap();
  for (uid, gid) in [
    (65533, 65532),
    (0, 0),
    (65534, 65534),
    (123, 456),
    (before.uid(), before.gid()),
  ] {
    chown_as_root(file, uid, gid);
    let metadata = std::fs::metadata(file).unwrap();
    assert_eq!(
      (metadata.uid(), metadata.gid()),
      (uid, gid),
      "chown {uid}:{gid}"
    );
  }
}

/// Two open file descriptions of one file are two lock-owners to the NFSv4 client, which sends a
/// `flock` as a whole-file byte-range LOCK: the second's non-blocking exclusive lock is refused
/// (`EWOULDBLOCK`, the server's `NFS4ERR_DENIED`) until the first unlocks, then granted.
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn locks_conflict_across_open_files(file: &Path) {
  use rustix::fs::{FlockOperation, flock};
  let first = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(file)
    .unwrap();
  let second = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(file)
    .unwrap();
  flock(&first, FlockOperation::NonBlockingLockExclusive).expect("the first lock is granted");
  assert_eq!(
    flock(&second, FlockOperation::NonBlockingLockExclusive),
    Err(rustix::io::Errno::WOULDBLOCK),
    "the second owner is refused while the first holds the lock"
  );
  flock(&first, FlockOperation::Unlock).unwrap();
  flock(&second, FlockOperation::NonBlockingLockExclusive)
    .expect("granted once the first unlocked");
  flock(&second, FlockOperation::Unlock).unwrap();
}

/// RFC 8276 through the kernel's 4.2 client: `user.` attributes are set, read, listed and removed, and
/// `XATTR_CREATE` of a set name is refused `EEXIST` (the server's `NFS4ERR_EXIST`), a missing one
/// `ENODATA` (its `NFS4ERR_NOXATTR`).
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn user_attributes(root: &Path) {
  use rustix::fs::{XattrFlags, getxattr, listxattr, removexattr, setxattr};
  let file = root.join("attributed");
  std::fs::write(&file, b"x").unwrap();
  setxattr(&file, "user.origin", b"slates", XattrFlags::CREATE).unwrap();
  assert_eq!(
    setxattr(&file, "user.origin", b"again", XattrFlags::CREATE),
    Err(rustix::io::Errno::EXIST)
  );
  let mut value = [0u8; 64];
  let read = getxattr(&file, "user.origin", &mut value).unwrap();
  assert_eq!(&value[..read], b"slates");
  let mut names = [0u8; 256];
  let listed = listxattr(&file, &mut names).unwrap();
  assert!(
    names[..listed]
      .split(|byte| *byte == 0)
      .any(|name| name == b"user.origin"),
    "listed: {:?}",
    String::from_utf8_lossy(&names[..listed])
  );
  removexattr(&file, "user.origin").unwrap();
  assert_eq!(
    getxattr(&file, "user.origin", &mut value),
    Err(rustix::io::Errno::NODATA)
  );
  std::fs::remove_file(&file).unwrap();
}

/// Shape: the gap between a sparse file's two writes: several chunk windows.
#[cfg(target_os = "linux")]
const GAP: u64 = 1 << 20;

/// NFSv4.2 through the kernel (RFC 7862): `lseek(SEEK_HOLE)` on a file written at 0 and at [`GAP`] finds
/// the hole between the writes — the kernel's own fallback, without the server's SEEK, would answer the
/// end of the file, so a hole before the second write proves the server answered — and `SEEK_DATA`
/// from it finds the second write; `copy_file_range` (the kernel's COPY) makes a byte-identical copy.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // file calls through the kernel mount under test (RAM-backed)
fn sparse_seek_and_copy(root: &Path) {
  use rustix::fs::{SeekFrom, copy_file_range, seek};
  use std::os::unix::fs::FileExt;
  let sparse = root.join("sparse");
  // Read and write: `copy_file_range` reads the source through this descriptor.
  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .create_new(true)
    .open(&sparse)
    .unwrap();
  file.write_all_at(b"head", 0).unwrap();
  file.write_all_at(b"tail", GAP).unwrap();
  file.sync_all().unwrap();
  let hole = seek(&file, SeekFrom::Hole(0)).unwrap();
  assert!(
    (4..GAP).contains(&hole),
    "the server's SEEK found the hole between the writes: {hole}"
  );
  assert_eq!(seek(&file, SeekFrom::Data(hole)).unwrap(), GAP);
  let copy = std::fs::File::create(root.join("copy")).unwrap();
  let length = GAP + 4;
  // Explicit offsets: the seeks above moved the source descriptor's position.
  let (mut from, mut to) = (0u64, 0u64);
  while from < length {
    let remaining = usize::try_from(length - from).unwrap();
    let done = copy_file_range(&file, Some(&mut from), &copy, Some(&mut to), remaining).unwrap();
    assert!(done > 0, "copy_file_range makes progress");
  }
  drop(copy);
  assert!(
    std::fs::read(root.join("copy")).unwrap() == std::fs::read(&sparse).unwrap(),
    "the copy is byte-identical"
  );
  std::fs::remove_file(root.join("copy")).unwrap();
  std::fs::remove_file(&sparse).unwrap();
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
      large_sequential_writes_round_trip(&mounted.path);
      #[cfg(target_os = "linux")]
      if minor == 2 {
        sparse_seek_and_copy(&mounted.path);
        user_attributes(&mounted.path);
        the_owners_own_file_refuses_a_truncating_open_without_write_permission(
          &mounted.path,
          "suite",
        );
      }
    }
    #[cfg(target_os = "linux")]
    if minor == 2 {
      let lane = kernel_mount_with(&source, port, minor, CONFORMANCE_OPTIONS);
      the_owners_own_file_refuses_a_truncating_open_without_write_permission(&lane.path, "lane");
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
