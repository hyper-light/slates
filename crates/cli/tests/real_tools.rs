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
//! - `git status --porcelain` after each event: no change shows (git's index keys on the inode number, both times and
//!   the size, `core.checkStat`).
//!
//! Gated: `SLATES_TEST_CLI=1` (a real anchor, its daemons and a kernel mount); macOS only for now (`mount_nfs`), the
//! Linux FUSE and Windows WinFsp runs are main's (run-requests.md). It skips loudly where `mount_nfs`, `cc`, `make` or
//! `git` is missing.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  // The fixture is a git checkout the test writes in its own build-output directory (A-50), as git_interop's are.
  clippy::disallowed_methods
)]
#![cfg(target_os = "macos")]

use std::io::Write;
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
  #[allow(clippy::disallowed_methods)] // the test paces its polls
  std::thread::sleep(POLL);
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
  std::fs::write(base.join(".gitignore"), "*.o\n").unwrap();
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

/// `make -q` and `git status --porcelain` over the mount: nothing out of date, nothing changed.
fn nothing_moved(mount: &Path, after: &str) {
  let made = tool(mount, "make", &["-q"]);
  assert!(
    made.status.success(),
    "after {after}, make would rebuild an untouched target: {}",
    text(&made)
  );
  let status = tool(mount, "git", &["status", "--porcelain"]);
  assert!(status.status.success(), "git status: {}", text(&status));
  assert!(
    status.stdout.is_empty(),
    "after {after}, git status shows changes nothing made: {}",
    text(&status)
  );
}

/// A compile, `make` and `git status` over a real kernel mount across a snapshot of the volume and a daemon's death
/// (w-realtools; A-781). Do: mount an overlay over a git checkout; build `plain.o` with make and refresh git's index;
/// start a compile of `main.c`, which includes `inc/once.h`, then blocks on a FIFO's include; take a snapshot and kill
/// the daemon (the anchor starts the next); let the compile go on to `sub/../inc/once.h`. Expect the compile to succeed
/// (one header: one inode number before and after), and after each event `make -q` up to date and `git status`
/// clean (the inode number and both times of every untouched file unchanged).
#[test]
fn a_pragma_once_build_make_and_git_status_hold_across_a_snapshot_and_a_daemon_restart() {
  if std::env::var_os("SLATES_TEST_CLI").is_none() {
    eprintln!(
      "skipping the real-tools flow: set SLATES_TEST_CLI=1 (a real kernel mount and daemon restarts)"
    );
    return;
  }
  for needed in ["mount_nfs", "cc", "make", "git", "mkfifo"] {
    if !on_path(needed) {
      eprintln!("skipping the real-tools flow: {needed} is not on this host");
      return;
    }
  }
  let root = fresh_dir("pragma");
  let (base, mount_point, gate) = (root.join("base"), root.join("mnt"), root.join("gate.h"));
  std::fs::create_dir_all(&mount_point).unwrap();
  make_base(&base, &gate);
  let made = tool(&root, "mkfifo", &[gate.to_str().unwrap()]);
  assert!(made.status.success(), "mkfifo: {}", text(&made));

  let instance = format!("real-tools-{}", std::process::id());
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
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
  let (code, _, err) = run(&instance, &["mount", &id, mount_point.to_str().unwrap()]);
  assert_eq!(code, 0, "mount: {err}");
  let mounted = Mounted(mount_point.clone());

  let built = tool(&mount_point, "make", &[]);
  assert!(built.status.success(), "make: {}", text(&built));
  let refreshed = tool(&mount_point, "git", &["status", "--porcelain"]);
  assert!(
    refreshed.status.success(),
    "git status: {}",
    text(&refreshed)
  );
  nothing_moved(&mount_point, "the first build");

  let compile = Command::new("cc")
    .args(["-fsyntax-only", "main.c"])
    .current_dir(&mount_point)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

  let (code, _, err) = run(&instance, &["volume", "snapshot", &id]);
  assert_eq!(code, 0, "snapshot: {err}");
  nothing_moved(&mount_point, "a snapshot");
  kill_the_daemon(&instance, &anchor);
  nothing_moved(&mount_point, "a daemon restart");

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
