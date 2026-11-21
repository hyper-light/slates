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
//! Gated: `SLATES_TEST_CLI=1` (a real anchor, its daemons and a kernel mount); macOS only for now (`mount_nfs`), the
//! Linux FUSE and Windows WinFsp runs are main's (run-requests.md). It skips loudly where `mount_nfs`, `cc`, `make`,
//! `cargo` or `git` is missing.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  // The fixture is a git checkout the test writes in its own build-output directory (A-50), as git_interop's are.
  clippy::disallowed_methods
)]
#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

#[path = "support/process_wait.rs"]
mod process_wait;

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
/// mount stalls every NFS mount on a macOS client, w-realtools 2026-11-20).
struct Mounted(PathBuf);

impl Drop for Mounted {
  fn drop(&mut self) {
    let _ = Command::new("/sbin/umount").arg(&self.0).output();
    let _ = Command::new("/sbin/umount").arg("-f").arg(&self.0).output();
  }
}

fn start_anchor(instance: &str) -> Anchor {
  let child = slates()
    .args(["--instance", instance, "anchor", "--quick", "--shards", "1"])
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
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

/// Kills the daemon as a crash does and waits for the anchor's next one to answer.
fn kill_the_daemon(instance: &str, anchor: &Anchor) {
  let pid = daemon_pid(instance).expect("a daemon answers");
  let killed = Command::new("kill")
    .args(["-9", &pid.to_string()])
    .status()
    .unwrap();
  assert!(killed.success(), "kill -9 {pid}");
  let mut judged = process_wait::Judged::of(anchor.0.id());
  loop {
    if daemon_pid(instance).is_some_and(|now| now != pid) {
      return;
    }
    judged.look("the anchor's next daemon answers");
    pause();
  }
}

fn value_of(out: &str, key: &str) -> String {
  out
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no {key} in {out}"))
    .trim()
    .to_owned()
}

fn on_path(tool: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {tool}")])
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
      "#include \"inc/once.h\"\n#include \"{}\"\n#include \"sub/../inc/once.h\"\nint main(void) {{ struct once o = {{0}}; return o.x; }}\n",
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
  let built = tool(mount, "make", &[]);
  assert!(built.status.success(), "make: {}", text(&built));
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

/// Every tracked file's inode number, modification time and change time (seconds and nanoseconds), as `stat` reads
/// them through the mount.
fn identities(mount: &Path) -> BTreeMap<String, (u64, i64, i64, i64, i64)> {
  let listed = tool(mount, "git", &["ls-files"]);
  assert!(listed.status.success(), "git ls-files: {}", text(&listed));
  String::from_utf8_lossy(&listed.stdout)
    .lines()
    .map(|path| {
      let meta = std::fs::metadata(mount.join(path)).unwrap();
      (
        path.to_owned(),
        (
          meta.ino(),
          meta.mtime(),
          meta.mtime_nsec(),
          meta.ctime(),
          meta.ctime_nsec(),
        ),
      )
    })
    .collect()
}

/// After `after`: every tracked file's identity as `before`, `make -q` up to date, `cargo build` a no-op and `git
/// status --porcelain` empty.
fn nothing_moved(mount: &Path, after: &str, before: &BTreeMap<String, (u64, i64, i64, i64, i64)>) {
  assert_eq!(
    &identities(mount),
    before,
    "after {after}, an untouched file's inode number or times moved"
  );
  let made = tool(mount, "make", &["-q"]);
  assert!(
    made.status.success(),
    "after {after}, make would rebuild an untouched target: {}",
    text(&made)
  );
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
  for needed in ["mount_nfs", "cc", "make", "cargo", "git", "mkfifo"] {
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
  let (code, _, err) = run(instance, &["mount", &id, mount_point.to_str().unwrap()]);
  assert_eq!(code, 0, "mount: {err}");
  (id, Mounted(mount_point.to_path_buf()))
}

/// A compile, `make`, `cargo build` and `git status` over a real kernel mount across a snapshot of the volume and a daemon's death
/// (w-realtools; A-781). Do: mount an overlay over a git checkout; build `plain.o` with make and refresh git's index;
/// start a compile of `main.c`, which includes `inc/once.h`, then blocks on a FIFO's include; take a snapshot and kill
/// the daemon (the anchor starts the next); let the compile go on to `sub/../inc/once.h`. Expect the compile to succeed
/// (one header: one inode number before and after), and after each event every tracked file's inode number and times
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
  let made = tool(&root, "mkfifo", &[gate.to_str().unwrap()]);
  assert!(made.status.success(), "mkfifo: {}", text(&made));

  let instance = format!("real-tools-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (id, mounted) = mount_overlay(&instance, &base, &mount_point);
  first_build(&mount_point);
  let before = identities(&mount_point);
  nothing_moved(&mount_point, "the first build", &before);

  let compile = Command::new("cc")
    .args(["-fsyntax-only", "main.c"])
    .current_dir(&mount_point)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

  let (code, _, err) = run(&instance, &["volume", "snapshot", &id]);
  assert_eq!(code, 0, "snapshot: {err}");
  nothing_moved(&mount_point, "a snapshot", &before);
  kill_the_daemon(&instance, &anchor);
  nothing_moved(&mount_point, "a daemon restart", &before);

  // Opening the FIFO for writing lets the compile's include of it go on; closing it ends that include.
  std::fs::OpenOptions::new()
    .write(true)
    .open(&gate)
    .unwrap()
    .write_all(b"/* the gate */\n")
    .unwrap();
  let compiled = compile.wait_with_output().unwrap();
  assert!(
    compiled.status.success(),
    "a header read before the events and again after them is two files to the compiler: {}",
    text(&compiled)
  );

  drop(mounted);
  drop(anchor);
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
  first_build(&mount_point);
  let before = identities(&mount_point);
  nothing_moved(&mount_point, "the first build", &before);

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
  nothing_moved(&mount_point, "memory pressure", &before);

  daemon
    .inject_pressure_hold(0)
    .expect("the hold is released on every shard");
  let _ = daemon.inject_memory_stall(None);
  nothing_moved(&mount_point, "the pressure's release", &before);

  drop(mounted);
  daemon.stop();
}
