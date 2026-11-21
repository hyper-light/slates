//! Real tools define filesystem correctness (w-realtools; EdenFS's own record, `eden/fs/docs/InodeStorage.md`: unstable
//! inode numbers broke C++ `#pragma once`, and a timestamp moved on an untouched file made `make` rebuild it). A test
//! that reads the right bytes after a restart can pass while a real incremental build does the wrong thing, so these
//! flows run the tools themselves over a real kernel mount of an overlay volume (the editor's workflow: a repository
//! checked out on the disk, mounted through slates), across a snapshot of the volume and a daemon's death:
//!
//! - a C compile that reads a `#pragma once` header, is held mid-compile by an include of a FIFO while the events
//!   happen, and then reads the same header by another path: it must see one header (one inode number), or the struct
//!   the header defines is redefined;
//! - `make -q` after each event: nothing untouched is out of date (modification times stable);
//! - `cargo build` after each event: a no-op (its fingerprints key on the sources' modification times);
//! - `git status --porcelain` after each event: no change shows (git's index keys on the inode number, both times and
//!   the size, `core.checkStat`);
//! - and, behind the tools, every tracked file's inode number, modification time and change time compared as `stat`
//!   reads them through the mount.
//!
//! The events: a snapshot of the volume and a daemon killed as a crash kills it, the anchor's next daemon taking the
//! volume over (the first flow); memory pressure, the daemon's admission hold and the host's memory stall injected
//! into an in-process daemon (the second).
//!
//! Every platform's mount: macOS's loopback NFS (`mount_nfs`), Linux's FUSE (`fusermount3`) and Windows' WinFsp drive.
//! On Windows the file's identity is its file index, last-write time and change time (`GetFileInformationByHandle`,
//! `FILE_BASIC_INFO`), the daemon is ended by `TerminateProcess`, and the tools are `cargo` and `git`: the compile held
//! by a FIFO and `make` are Unix's (Windows has no FIFO a compiler includes, and no `make` of its own).
//!
//! Gated: `SLATES_TEST_CLI=1` (a real anchor, its daemons and a kernel mount), and on Windows `WINFSP_TEST_MOUNT=1`
//! (WinFsp installed), as `winfsp.rs` is; the Linux and Windows runs are main's (run-requests.md). It skips loudly
//! where a tool it drives is missing.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  // The fixture is a git checkout the test writes in its own build-output directory (A-50), as git_interop's are.
  clippy::disallowed_methods
)]
#![cfg(any(target_os = "macos", target_os = "linux", windows))]

use std::collections::BTreeMap;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::process::{Child, Command, Output};
use std::time::Duration;

#[path = "support/process_wait.rs"]
mod process_wait;

/// Derived: how long a change in the kernel's name space (a drive arriving after its mount) may take: the observation
/// budget, from its one source (`slates_machine::budgets`), as `winfsp.rs` waits for it.
const KERNEL_WAIT: Duration = Duration::from_nanos(slates_machine::budgets::observe_budget_ns());
/// Shape: the poll interval of the waits.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answered verbs before the daemon counts as settled (the anchor may restart it once at
/// startup, cli.rs `start_anchor_with_environment`).
const STABLE_STREAK: u32 = 10;
/// Shape: the quick profile's probe budget (milliseconds), as `slates anchor --quick` measures it (cli.rs).
const QUICK_PROBE_MS: u64 = 5;
/// Shape: the watchdog's looks waited out under the pressure: its beat is 100 ms (the daemon's start line), so a
/// second of them.
const PRESSURE_LOOKS: u32 = 10;
/// See [`PRESSURE_LOOKS`].
const LOOK: Duration = Duration::from_millis(100);
/// Shape: the drives a restart's successor adopts: one on Windows (the drive held across the restart), none elsewhere.
const ADOPTED_A_RESTART: u64 = if cfg!(windows) { 1 } else { 0 };
/// Whether the flows build with `make` here: Unix's own tool (Windows ships none).
const BUILDS_WITH_MAKE: bool = cfg!(unix);
/// Format: the tools the flows drive on this platform.
#[cfg(unix)]
const TOOLS: [&str; 6] = [MOUNT_TOOL, "cc", "make", "cargo", "git", "mkfifo"];
/// Format: the tools the flows drive on this platform.
#[cfg(windows)]
const TOOLS: [&str; 2] = ["cargo", "git"];
/// A file's identity as the tools key on it: its number (the inode number; Windows' file index), then its modification
/// and change times (Unix: seconds and nanoseconds each; Windows: the FILETIME, and zero).
type Identity = (u64, i64, i64, i64, i64);
/// Format: the header both includes name, by two paths.
const HEADER: &str = "#pragma once\nstruct once { int x; };\n";

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
  pause_for(POLL);
}

fn pause_for(how_long: Duration) {
  #[allow(clippy::disallowed_methods)] // the test paces its polls
  std::thread::sleep(how_long);
}

/// The anchor, killed and reaped on drop, so a failed assertion leaves no daemon behind.
struct Anchor(Child);

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

/// The kernel mount, unmounted on drop (before the anchor goes, so no hard mount is left without its server: one such
/// mount stalls every NFS mount on a macOS client, w-realtools 2026-11-20): where it is mounted (a directory; a drive
/// on Windows), its root as the tools open it, and the instance serving it.
struct Mounted {
  at: String,
  root: PathBuf,
  #[cfg_attr(unix, allow(dead_code))]
  instance: String,
}

#[cfg(unix)]
impl Drop for Mounted {
  fn drop(&mut self) {
    for args in UNMOUNTS {
      let _ = Command::new(args[0])
        .args(&args[1..])
        .arg(&self.at)
        .output();
    }
  }
}

/// On Windows the drive is the daemon's own, so the command detaches it (`slates unmount`).
#[cfg(windows)]
impl Drop for Mounted {
  fn drop(&mut self) {
    let _ = run(&self.instance, &["unmount", &self.at]);
  }
}

/// Format: how the mount is taken down, then forced (macOS: `umount(8)`).
#[cfg(target_os = "macos")]
const UNMOUNTS: [&[&str]; 2] = [&["/sbin/umount"], &["/sbin/umount", "-f"]];
/// Format: how the mount is taken down, then lazily (Linux: the FUSE helper).
#[cfg(target_os = "linux")]
const UNMOUNTS: [&[&str]; 2] = [&["fusermount3", "-u"], &["fusermount3", "-uz"]];
/// Format: the tool `slates mount` drives on this platform (macOS: the loopback NFS mount).
#[cfg(target_os = "macos")]
const MOUNT_TOOL: &str = "mount_nfs";
/// Format: the tool `slates mount` drives on this platform (Linux: FUSE through the system's helper).
#[cfg(target_os = "linux")]
const MOUNT_TOOL: &str = "fusermount3";

fn start_anchor(instance: &str) -> Anchor {
  let child = slates()
    .args(["--instance", instance, "anchor", "--quick", "--shards", "1"])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::inherit())
    .spawn()
    .unwrap();
  let anchor = Anchor(child);
  let mut judged = process_wait::Judged::of(anchor.0.id());
  let mut streak = 0;
  while streak < STABLE_STREAK {
    streak = if run(instance, &["volume", "list"]).0 == 0 {
      streak + 1
    } else {
      0
    };
    judged.look("the daemon came up");
    pause();
  }
  let (code, _, error) = run(instance, &["bootstrap", "root"]);
  assert_eq!(code, 0, "bootstrap: {error}");
  anchor
}

/// The daemon's pid, from `status --json`, or `None` while no daemon answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, out, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  let after = out.split("\"pid\":").nth(1)?;
  after
    .trim_start()
    .split(|c: char| !c.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Kills the daemon as a crash does and waits for the anchor's next one to answer and, on Windows, to adopt the drive
/// (`winfsp.adopted`, as `winfsp.rs` waits for it).
fn kill_the_daemon(instance: &str, anchor: &Anchor) {
  let pid = daemon_pid(instance).expect("a daemon answers");
  let adopted = adoptions(instance);
  end_process(pid);
  let mut judged = process_wait::Judged::of(anchor.0.id());
  loop {
    if daemon_pid(instance).is_some_and(|now| now != pid)
      && adoptions(instance) >= adopted.saturating_add(ADOPTED_A_RESTART)
    {
      return;
    }
    judged.look("the anchor's next daemon answers");
    pause();
  }
}

/// Ends process `pid` as a crash does: `SIGKILL`.
#[cfg(unix)]
fn end_process(pid: u32) {
  let killed = Command::new("kill")
    .args(["-9", &pid.to_string()])
    .status()
    .unwrap();
  assert!(killed.success(), "kill -9 {pid}");
}

/// Ends process `pid` as a crash does: `TerminateProcess`, its exit observed (as `winfsp.rs`'s `kill_daemon`).
#[cfg(windows)]
fn end_process(pid: u32) {
  use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
  use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
  use windows_sys::Win32::System::Threading::{
    INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
  };
  // SAFETY: the pid came from the instance's status; only termination and exit observation are asked, no inheritance.
  let process = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
  assert!(
    !process.is_null(),
    "OpenProcess({pid}): {}",
    std::io::Error::last_os_error()
  );
  // SAFETY: this is the unique owned handle OpenProcess just returned.
  let process = unsafe { OwnedHandle::from_raw_handle(process) };
  // SAFETY: `process` stays owned for the call, and the exit code carries no pointer.
  assert_ne!(unsafe { TerminateProcess(process.as_raw_handle(), 1) }, 0);
  // SAFETY: `process` stays owned until the wait observes the termination TerminateProcess began.
  let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
  assert_eq!(waited, WAIT_OBJECT_0, "the killed daemon exits");
}

/// The drives the instance's daemons adopted across a restart (`winfsp.adopted`, summed over the shards); zero off
/// Windows, where a mount needs no adoption.
#[cfg(windows)]
fn adoptions(instance: &str) -> u64 {
  let (code, out, _) = run(instance, &["status"]);
  if code != 0 {
    return 0;
  }
  out
    .lines()
    .filter_map(|line| line.split_once("winfsp.adopted: "))
    .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
    .sum()
}

/// See the Windows [`adoptions`]: zero off Windows, so the restart's wait is the daemon's answer alone.
#[cfg(unix)]
fn adoptions(_instance: &str) -> u64 {
  0
}

fn value_of(out: &str, key: &str) -> String {
  out
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no {key} in {out}"))
    .trim()
    .to_owned()
}

#[cfg(unix)]
fn on_path(tool: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {tool}")])
    .output()
    .is_ok_and(|o| o.status.success())
}

#[cfg(windows)]
fn on_path(tool: &str) -> bool {
  Command::new("where")
    .arg(tool)
    .output()
    .is_ok_and(|o| o.status.success())
}

/// A fresh directory in the build output (A-50: never /tmp).
fn fresh_dir(name: &str) -> PathBuf {
  let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
    .join(format!("real-tools-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir
}

fn tool(dir: &Path, program: &str, args: &[&str]) -> Output {
  Command::new(program)
    .args(args)
    .current_dir(dir)
    .output()
    .unwrap()
}

fn text(output: &Output) -> String {
  format!(
    "{}{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  )
}

/// The base: a git repository holding a `#pragma once` header, a C file including it by two paths with a FIFO's
/// include between them, and a Makefile building that file's object into the volume (the object is the overlay's own).
fn make_base(base: &Path, gate: &Path) {
  std::fs::create_dir_all(base.join("inc")).unwrap();
  std::fs::create_dir_all(base.join("sub")).unwrap();
  std::fs::write(base.join("inc/once.h"), HEADER).unwrap();
  std::fs::write(base.join("sub/keep.h"), "/* a sibling */\n").unwrap();
  std::fs::write(
    base.join("main.c"),
    format!(
      concat!(
        "#include \"inc/once.h\"\n#include \"{}\"\n#include \"sub/../inc/once.h\"\n",
        "int main(void) {{ struct once o = {{0}}; return o.x; }}\n"
      ),
      gate.display()
    ),
  )
  .unwrap();
  std::fs::write(
    base.join("plain.c"),
    "#include \"inc/once.h\"\nint plain(void) { struct once o = {0}; return o.x; }\n",
  )
  .unwrap();
  std::fs::write(
    base.join("Makefile"),
    "plain.o: plain.c inc/once.h\n\tcc -c plain.c -o plain.o\n",
  )
  .unwrap();
  std::fs::create_dir_all(base.join("krate/src")).unwrap();
  std::fs::write(
    base.join("krate/Cargo.toml"),
    "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
  )
  .unwrap();
  std::fs::write(
    base.join("krate/src/lib.rs"),
    "pub fn answer() -> u32 {\n  42\n}\n",
  )
  .unwrap();
  std::fs::write(base.join(".gitignore"), "*.o\ntarget/\nCargo.lock\n").unwrap();
  for args in [
    &["init", "-q"][..],
    &["add", "-A"],
    &[
      "-c",
      "user.name=t",
      "-c",
      "user.email=t@t",
      "commit",
      "-qm",
      "base",
    ],
  ] {
    let done = tool(base, "git", args);
    assert!(done.status.success(), "git {args:?}: {}", text(&done));
  }
}

/// `cargo build` of the fixture's crate over the mount, as a developer runs it: the parent `cargo test`'s target
/// directory, build jobs and flags are not the crate's, so they are taken out of its environment (incremental on, the
/// dev profile's default).
fn cargo_build(mount: &Path) -> Output {
  Command::new("cargo")
    .args(["build", "--offline"])
    .current_dir(mount.join("krate"))
    .env_remove("CARGO_TARGET_DIR")
    .env_remove("CARGO_INCREMENTAL")
    .env_remove("CARGO_BUILD_JOBS")
    .env_remove("RUSTFLAGS")
    .env_remove("CARGO_ENCODED_RUSTFLAGS")
    .output()
    .unwrap()
}

/// The first build over the mount: make's object, cargo's crate, and git's index refreshed by a status.
fn first_build(mount: &Path) {
  if BUILDS_WITH_MAKE {
    let built = tool(mount, "make", &[]);
    assert!(built.status.success(), "make: {}", text(&built));
  }
  let crate_built = cargo_build(mount);
  assert!(
    crate_built.status.success(),
    "cargo build: {}",
    text(&crate_built)
  );
  let refreshed = tool(mount, "git", &["status", "--porcelain"]);
  assert!(
    refreshed.status.success(),
    "git status: {}",
    text(&refreshed)
  );
}

/// Every tracked file's [`Identity`], as the platform's own call reads it through the mount.
fn identities(mount: &Path) -> BTreeMap<String, Identity> {
  let listed = tool(mount, "git", &["ls-files"]);
  assert!(listed.status.success(), "git ls-files: {}", text(&listed));
  String::from_utf8_lossy(&listed.stdout)
    .lines()
    .map(|path| (path.to_owned(), identity_of(&mount.join(path))))
    .collect()
}

/// A file's [`Identity`] as `stat` reads it: the inode number, then the modification and change times.
#[cfg(unix)]
fn identity_of(path: &Path) -> Identity {
  let meta = std::fs::metadata(path).unwrap();
  (
    meta.ino(),
    meta.mtime(),
    meta.mtime_nsec(),
    meta.ctime(),
    meta.ctime_nsec(),
  )
}

/// A file's [`Identity`] as Windows reads it through a handle: the file index (`GetFileInformationByHandle`), then the
/// last-write and change times (`FILE_BASIC_INFO`).
#[cfg(windows)]
fn identity_of(path: &Path) -> Identity {
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx,
  };
  let file = std::fs::File::open(path).unwrap();
  // SAFETY: a plain-data struct of integers and FILETIMEs, for which all zero bits are a valid value.
  let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
  // SAFETY: the handle is the open file's and lives across the call; `info` is a valid, writable struct.
  let answered = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
  assert_ne!(answered, 0, "GetFileInformationByHandle({path:?}) failed");
  // SAFETY: a plain-data struct of integers, for which all zero bits are a valid value.
  let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
  let size = u32::try_from(std::mem::size_of::<FILE_BASIC_INFO>()).unwrap();
  // SAFETY: the handle is open for the call; `basic` is a writable record of the class named, of the size passed.
  let answered = unsafe {
    GetFileInformationByHandleEx(
      file.as_raw_handle(),
      FileBasicInfo,
      (&raw mut basic).cast(),
      size,
    )
  };
  assert_ne!(answered, 0, "GetFileInformationByHandleEx({path:?}) failed");
  (
    (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    basic.LastWriteTime,
    0,
    basic.ChangeTime,
    0,
  )
}

/// After `after`: every tracked file's identity as `before`, `make -q` up to date, `cargo build` a no-op and `git
/// status --porcelain` empty.
fn nothing_moved(mount: &Path, after: &str, before: &BTreeMap<String, Identity>) {
  assert_eq!(
    &identities(mount),
    before,
    "after {after}, an untouched file's inode number or times moved"
  );
  if BUILDS_WITH_MAKE {
    let made = tool(mount, "make", &["-q"]);
    assert!(
      made.status.success(),
      "after {after}, make would rebuild an untouched target: {}",
      text(&made)
    );
  }
  let crate_built = cargo_build(mount);
  assert!(
    crate_built.status.success() && !text(&crate_built).contains("Compiling"),
    "after {after}, cargo rebuilt an untouched crate: {}",
    text(&crate_built)
  );
  let status = tool(mount, "git", &["status", "--porcelain"]);
  assert!(status.status.success(), "git status: {}", text(&status));
  assert!(
    status.stdout.is_empty(),
    "after {after}, git status shows changes nothing made: {}",
    text(&status)
  );
}

/// Whether the real-tools flows can run here; says why not on standard error.
fn runnable() -> bool {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the real-tools flow: set SLATES_TEST_CLI=1 (a real kernel mount and daemon restarts)"
    );
    return false;
  }
  if cfg!(windows) && std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("skipping the real-tools flow: set WINFSP_TEST_MOUNT=1 (WinFsp installed)");
    return false;
  }
  for needed in TOOLS {
    if !on_path(needed) {
      eprintln!("skipping the real-tools flow: {needed} is not on this host");
      return false;
    }
  }
  true
}

/// Creates the overlay volume over `base` through the CLI and mounts it at `mount_point`; its id and the mount's guard.
fn mount_overlay(instance: &str, base: &Path, mount_point: &Path) -> (String, Mounted) {
  let (code, out, err) = run(
    instance,
    &[
      "volume",
      "create",
      "checkout",
      "--dynamic",
      "1GiB",
      "--base",
      base.to_str().unwrap(),
    ],
  );
  assert_eq!(code, 0, "create: {err}");
  let id = value_of(&out, "id");
  let (at, root) = mount_target(mount_point);
  let (code, _, err) = run(instance, &["mount", &id, &at]);
  assert_eq!(code, 0, "mount: {err}");
  let mounted = Mounted {
    at,
    root,
    instance: instance.to_owned(),
  };
  wait_ready(&mounted.root);
  (id, mounted)
}

/// Where the volume is mounted and the root the tools open: the directory itself on Unix.
#[cfg(unix)]
fn mount_target(mount_point: &Path) -> (String, PathBuf) {
  (
    mount_point.to_str().unwrap().to_owned(),
    mount_point.to_path_buf(),
  )
}

/// Where the volume is mounted and the root the tools open: a free drive letter on Windows (as `winfsp.rs` picks it),
/// and its root.
#[cfg(windows)]
fn mount_target(_mount_point: &Path) -> (String, PathBuf) {
  // SAFETY: `GetLogicalDrives` takes no arguments and returns a bitmask (bit 0 = A:, 1 = B:, ...).
  let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
  let letter = ('E'..='Z')
    .find(|letter| used & (1 << (u32::from(*letter) - u32::from('A'))) == 0)
    .expect("a free drive letter");
  (format!("{letter}:"), PathBuf::from(format!("{letter}:\\")))
}

/// Waits until the mount's root answers (a WinFsp drive registers after the mount's reply; a Unix mount at once),
/// within the observation budget.
fn wait_ready(root: &Path) {
  let started = std::time::Instant::now();
  while std::fs::metadata(root).is_err() {
    assert!(
      started.elapsed() < KERNEL_WAIT,
      "{root:?} never answered within {KERNEL_WAIT:?}"
    );
    pause();
  }
}

/// A compile, `make`, `cargo build` and `git status` over a real kernel mount across a snapshot of the volume and a
/// daemon's death (w-realtools; A-781). On Windows the compile and `make` are left out (the module's note). Do: mount
/// an overlay over a git checkout; build `plain.o` with make and the crate with cargo, and refresh git's index; start a
/// compile of `main.c`, which includes `inc/once.h`, then blocks on a FIFO's include; take a snapshot and kill the
/// daemon (the anchor starts the next); let the compile go on to `sub/../inc/once.h`. Expect the compile to succeed
/// (one header: one inode number before and after), and after each event every tracked file's number and times
/// unchanged, `make -q` up to date, `cargo build` a no-op and `git status` clean.
#[test]
fn a_pragma_once_build_make_cargo_and_git_status_hold_across_a_snapshot_and_a_daemon_restart() {
  if !runnable() {
    return;
  }
  let root = fresh_dir("pragma");
  let (base, mount_point, gate) = (root.join("base"), root.join("mnt"), root.join("gate.h"));
  std::fs::create_dir_all(&mount_point).unwrap();
  make_base(&base, &gate);

  let instance = format!("real-tools-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (id, mounted) = mount_overlay(&instance, &base, &mount_point);
  let tree = mounted.root.clone();
  first_build(&tree);
  let before = identities(&tree);
  nothing_moved(&tree, "the first build", &before);

  #[cfg(unix)]
  let compile = held_compile(&root, &gate, &tree);

  let (code, _, err) = run(&instance, &["volume", "snapshot", &id]);
  assert_eq!(code, 0, "snapshot: {err}");
  nothing_moved(&tree, "a snapshot", &before);
  kill_the_daemon(&instance, &anchor);
  nothing_moved(&tree, "a daemon restart", &before);

  #[cfg(unix)]
  compiled_once(compile, &gate);

  drop(mounted);
  drop(anchor);
}

/// Starts a compile of `main.c` in `tree`, which reads `inc/once.h` and then blocks on the FIFO made at `gate` (Unix:
/// the compiler's open of the FIFO waits for a writer).
#[cfg(unix)]
fn held_compile(root: &Path, gate: &Path, tree: &Path) -> std::process::Child {
  let made = tool(root, "mkfifo", &[gate.to_str().unwrap()]);
  assert!(made.status.success(), "mkfifo: {}", text(&made));
  Command::new("cc")
    .args(["-fsyntax-only", "main.c"])
    .current_dir(tree)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap()
}

/// Lets the held compile go on (opening the FIFO for writing releases its include; closing it ends it) and expects it
/// to compile: `sub/../inc/once.h`, read after the events, is the header it read before them.
#[cfg(unix)]
fn compiled_once(compile: std::process::Child, gate: &Path) {
  std::fs::OpenOptions::new()
    .write(true)
    .open(gate)
    .unwrap()
    .write_all(b"/* the gate */\n")
    .unwrap();
  let compiled = compile.wait_with_output().unwrap();
  assert!(
    compiled.status.success(),
    "a header read before the events and again after them is two files to the compiler: {}",
    text(&compiled)
  );
}

/// The same tools under memory pressure (w-realtools; §4.2 admission's pressure hold, A-336's memory stall). Do: start
/// a daemon in this process (so the test can put pressure on it), mount an overlay over a git checkout through the CLI,
/// build with make and cargo and refresh git's index; then hold the whole admittable memory and report a host memory
/// stall, as a host short of memory makes the daemon see, wait out a few of the daemon's own sampling looks, and look
/// again; release the pressure and look once more. Expect, under the pressure and after it, every tracked file's inode
/// number and times unchanged, `make -q` up to date, `cargo build` a no-op and `git status` clean.
#[test]
fn make_cargo_and_git_status_hold_under_memory_pressure() {
  if !runnable() {
    return;
  }
  let root = fresh_dir("pressure");
  let (base, mount_point, gate) = (root.join("base"), root.join("mnt"), root.join("gate.h"));
  std::fs::create_dir_all(&mount_point).unwrap();
  make_base(&base, &gate);

  let instance = format!("real-tools-pressure-{}", std::process::id());
  let profile =
    slates_machine::MachineProfile::measure_patiently(&slates_machine::ProfileOptions {
      budget_per_probe: Duration::from_millis(QUICK_PROBE_MS),
      codecs: false,
      core_matrix: false,
    })
    .unwrap();
  let config = slates_server::DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = slates_server::Daemon::start(
    &profile,
    config,
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-{instance}"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the test creates its local consensus group");
  let (_, mounted) = mount_overlay(&instance, &base, &mount_point);
  let tree = mounted.root.clone();
  first_build(&tree);
  let before = identities(&tree);
  nothing_moved(&tree, "the first build", &before);

  daemon
    .inject_pressure_hold(u64::MAX)
    .expect("the hold is installed on every shard");
  assert!(
    daemon.inject_memory_stall(Some(true)),
    "the daemon's watchdog takes the stall"
  );
  // A few of the watchdog's looks, so what the daemon does under the stall has happened before the tools look.
  for _ in 0..PRESSURE_LOOKS {
    pause_for(LOOK);
  }
  nothing_moved(&tree, "memory pressure", &before);

  daemon
    .inject_pressure_hold(0)
    .expect("the hold is released on every shard");
  let _ = daemon.inject_memory_stall(None);
  nothing_moved(&tree, "the pressure's release", &before);

  drop(mounted);
  daemon.stop();
}
