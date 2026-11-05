//! Win32 file semantics through the WinFsp drive that winfstest found broken (A-214; §4.6 "Windows";
//! `docs/bugs/2026-11-05-winfstest-found-three-win32-semantics-the-drive-broke.md`). winfstest's base scripts, run over
//! `slates mount ID E:` on both Windows lanes (diagnostics run 41982130174), found three calls whose answer a Windows
//! program acts on and which the drive answered wrongly. Each test here makes the call a program makes and checks the
//! answer and what the drive holds afterwards:
//! - `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` onto an existing name must refuse (`ERROR_ALREADY_EXISTS`) and
//!   leave both files as they were (`t/base/04.t` cases 7 and 23); the drive replaced the target.
//! - `RemoveDirectoryW` of a directory that holds a file must refuse (`ERROR_DIR_NOT_EMPTY`) and leave it
//!   (`t/base/03.t` case 3); the drive answered success.
//! - An open with `TRUNCATE_EXISTING` must leave the file empty (`t/base/05.t` case 12); the drive kept its bytes.
//! - `SetFileTime`'s access and write times must be kept (`t/base/06.t` case 6); the drive answered success and kept
//!   neither. A creation time is not kept: the volume records no settable birth time (A-214's stated difference).
//!
//! Windows only, and gated on `SLATES_TEST_CLI=1` and `WINFSP_TEST_MOUNT=1`, as `tests/winfsp.rs` is; without them
//! each test skips loudly and passes. CI's Windows lanes install WinFsp and set both.
#![cfg(windows)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::BufRead;
use std::os::windows::ffi::OsStrExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: shards for the test daemon, as `tests/winfsp.rs`.
const SHARDS: &str = "2";
/// Shape: how long the drive gets to answer after the mount's reply: ten liveness budgets, as `tests/winfsp.rs`.
const WAIT_FOR: Duration = Duration::from_secs(10);
/// Shape: the pause between polls.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answers to `volume list` a start waits for, as `tests/winfsp.rs`.
const STABLE_STREAK: u32 = 10;
/// Shape: the most `volume list` attempts a start makes after the daemon's serving line, as `tests/winfsp.rs`.
const START_ATTEMPTS: u32 = 60;
/// Format: the start of the line the daemon prints once it serves (`crates/cli/src/daemon.rs`).
const SERVING_LINE: &str = "slates daemon: instance ";
/// Format: Win32's `ERROR_ALREADY_EXISTS` (winerror.h): what `MoveFileExW` answers for a name taken, unasked to replace.
const ERROR_ALREADY_EXISTS: i32 = 183;
/// Format: Win32's `ERROR_ACCESS_DENIED` (winerror.h): what `MoveFileExW` answers for a directory asked to be replaced.
const ERROR_ACCESS_DENIED: i32 = 5;
/// Format: Win32's `ERROR_DIR_NOT_EMPTY` (winerror.h).
const ERROR_DIR_NOT_EMPTY: i32 = 145;
/// Format: the bytes the truncation test writes first: winfstest's own length (`SetEndOfFile %s 42`).
const FILE_BYTES: usize = 42;
/// Format: the modification time the times test sets: 2100-01-01T00:00:00Z, in seconds since the Unix epoch, a time
/// no file the test makes could carry by itself.
const SET_MODIFIED_SECONDS: u64 = 4_102_444_800;
/// Format: the access time the times test sets: an hour after [`SET_MODIFIED_SECONDS`], so the two cannot be confused.
const SET_ACCESSED_SECONDS: u64 = SET_MODIFIED_SECONDS + 3_600;
/// Format: `TRUNCATE_EXISTING`, CreateFile's disposition (fileapi.h): open an existing file and empty it.
const TRUNCATE_EXISTING: u32 = 5;

fn pause() {
  // The test harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL);
}

/// Whether the live WinFsp tests run here; a skip is printed, never a failure.
fn live_mount_runs() -> bool {
  if std::env::var_os("SLATES_TEST_CLI").is_none()
    || std::env::var_os("WINFSP_TEST_MOUNT").is_none()
  {
    eprintln!(
      "SKIP: the live WinFsp mount needs SLATES_TEST_CLI=1 and WINFSP_TEST_MOUNT=1 (WinFsp installed)"
    );
    return false;
  }
  true
}

fn slates() -> Command {
  Command::new(env!("CARGO_BIN_EXE_slates"))
}

/// Runs a client verb; (exit code, stdout, stderr).
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

/// The value of `key: value` in a verb's text output.
fn value_of(text: &str, key: &str) -> String {
  text
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no `{key}` in {text:?}"))
    .to_owned()
}

/// The anchor, killed and reaped on drop with its log drain joined; killing it ends its daemon (its job).
struct Anchor {
  child: Child,
  drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Anchor {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    if let Some(drain) = self.drain.take() {
      let _ = drain.join();
    }
  }
}

/// Starts `slates anchor`, waits for its daemon to serve and answer a streak of verbs, and bootstraps it.
fn start_anchor(instance: &str) -> Anchor {
  let mut child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      SHARDS,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
  let serving = format!("{SERVING_LINE}{instance} serving on ");
  let mut line = String::new();
  loop {
    line.clear();
    assert!(
      stderr.read_line(&mut line).unwrap() > 0,
      "the anchor ended before its daemon served"
    );
    eprint!("{line}");
    if line.starts_with(&serving) {
      break;
    }
  }
  let drain = std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  });
  let anchor = Anchor {
    child,
    drain: Some(drain),
  };
  let mut streak = 0u32;
  for _ in 0..START_ATTEMPTS {
    let (code, _, _) = run(instance, &["volume", "list"]);
    streak = if code == 0 { streak + 1 } else { 0 };
    if streak >= STABLE_STREAK {
      let (code, _, error) = run(instance, &["bootstrap", "root"]);
      assert_eq!(code, 0, "explicit first-time bootstrap: {error}");
      return anchor;
    }
    pause();
  }
  panic!(
    "the serving daemon answered {STABLE_STREAK} verbs in a row within {START_ATTEMPTS} attempts"
  );
}

/// The first drive letter from `E:` to `Z:` whose root answers nothing.
fn free_drive_letter() -> String {
  ('E'..='Z')
    .map(|letter| format!("{letter}:"))
    .find(|drive| std::fs::metadata(format!("{drive}\\")).is_err())
    .expect("a free drive letter")
}

/// A volume mounted at a drive, unmounted on drop before its anchor goes.
struct Drive {
  instance: String,
  id: String,
  letter: String,
  _anchor: Anchor,
}

impl Drive {
  fn mount(tag: &str) -> Drive {
    let instance = format!("ws{tag}{}", std::process::id());
    let anchor = start_anchor(&instance);
    let (code, out, err) = run(&instance, &["volume", "create", tag, "--bounded", "64MiB"]);
    assert_eq!(code, 0, "{err}");
    let id = value_of(&out, "id");
    let letter = free_drive_letter();
    let (code, _, err) = run(&instance, &["mount", &id, &letter]);
    assert_eq!(code, 0, "mount: {err}");
    let started = Instant::now();
    while std::fs::metadata(format!("{letter}\\")).is_err() {
      assert!(
        started.elapsed() < WAIT_FOR,
        "{letter} never answered within {WAIT_FOR:?}"
      );
      pause();
    }
    Drive {
      instance,
      id,
      letter,
      _anchor: anchor,
    }
  }

  fn path(&self, name: &str) -> String {
    format!("{}\\{name}", self.letter)
  }

  /// A file's bytes as slates itself reads them (`slates read`), not through Windows' cache.
  fn read_through_slates(&self, name: &str) -> Option<String> {
    let (code, out, _) = run(&self.instance, &["read", &self.id, name]);
    (code == 0).then_some(out)
  }
}

impl Drop for Drive {
  fn drop(&mut self) {
    let _ = run(&self.instance, &["unmount", &self.letter]);
  }
}

fn wide(text: &str) -> Vec<u16> {
  std::ffi::OsStr::new(text)
    .encode_wide()
    .chain(std::iter::once(0))
    .collect()
}

/// `MoveFileExW(from, to, flags)`: Ok, or the Win32 error it set.
fn move_file(from: &str, to: &str, flags: u32) -> Result<(), i32> {
  let (from, to) = (wide(from), wide(to));
  // SAFETY: both are NUL-terminated UTF-16 strings that live across the call; the call reads them and nothing else.
  let moved = unsafe {
    windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), flags)
  };
  if moved != 0 {
    Ok(())
  } else {
    Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
  }
}

/// A-214, `t/base/04.t` cases 7 and 23. Do: make two files with different bytes on the drive, and move the first onto
/// the second with `MoveFileExW` and no `MOVEFILE_REPLACE_EXISTING`. Expect: `ERROR_ALREADY_EXISTS`, and both files
/// still there with their own bytes, through Windows and through slates; then with `MOVEFILE_REPLACE_EXISTING` the
/// move succeeds and the target holds the source's bytes; and asked to replace a directory, the move is refused
/// `ERROR_ACCESS_DENIED` and the directory stays (`t/base/04.t` case 26), as Windows never replaces one.
#[test]
fn a_move_not_asked_to_replace_refuses_a_taken_name_and_leaves_both_files() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("move");
  std::fs::write(drive.path("from.txt"), b"the source").unwrap();
  std::fs::write(drive.path("to.txt"), b"the target").unwrap();
  let refused = move_file(&drive.path("from.txt"), &drive.path("to.txt"), 0);
  assert_eq!(
    refused,
    Err(ERROR_ALREADY_EXISTS),
    "a move unasked to replace a taken name is refused"
  );
  assert_eq!(
    std::fs::read(drive.path("from.txt")).unwrap(),
    b"the source"
  );
  assert_eq!(std::fs::read(drive.path("to.txt")).unwrap(), b"the target");
  assert_eq!(
    drive.read_through_slates("to.txt").as_deref(),
    Some("the target")
  );
  let replaced = move_file(
    &drive.path("from.txt"),
    &drive.path("to.txt"),
    windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING,
  );
  assert_eq!(replaced, Ok(()), "asked to replace, the move succeeds");
  assert_eq!(std::fs::read(drive.path("to.txt")).unwrap(), b"the source");
  assert!(std::fs::metadata(drive.path("from.txt")).is_err());
  std::fs::create_dir(drive.path("dir")).unwrap();
  std::fs::create_dir(drive.path("other")).unwrap();
  let onto_a_directory = move_file(
    &drive.path("other"),
    &drive.path("dir"),
    windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING,
  );
  assert_eq!(
    onto_a_directory,
    Err(ERROR_ACCESS_DENIED),
    "a directory is never replaced"
  );
  assert!(std::fs::metadata(drive.path("dir")).unwrap().is_dir());
  assert!(std::fs::metadata(drive.path("other")).unwrap().is_dir());
}

/// A-214, `t/base/03.t` case 3. Do: make a directory holding a file on the drive and remove the directory
/// (`RemoveDirectoryW`, as `std::fs::remove_dir` does). Expect: `ERROR_DIR_NOT_EMPTY`, and the directory and its file
/// still there, through Windows and through slates; once the file is deleted, the directory removes.
#[test]
fn a_directory_holding_a_file_is_not_removed() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("rmdir");
  std::fs::create_dir(drive.path("dir")).unwrap();
  std::fs::write(drive.path("dir\\kept.txt"), b"kept").unwrap();
  let refused = std::fs::remove_dir(drive.path("dir"));
  assert_eq!(
    refused.as_ref().map_err(std::io::Error::raw_os_error),
    Err(Some(ERROR_DIR_NOT_EMPTY)),
    "a directory with a file in it is not removed: {refused:?}"
  );
  assert!(std::fs::metadata(drive.path("dir")).unwrap().is_dir());
  assert!(std::fs::metadata(drive.path("other")).unwrap().is_dir());
}

/// A-214, `t/base/03.t` case 3. Do: make a directory holding a file on the drive and remove the directory
/// (`RemoveDirectoryW`, as `std::fs::remove_dir` does). Expect: `ERROR_DIR_NOT_EMPTY`, and the directory and its file
/// still there, through Windows and through slates; once the file is deleted, the directory removes.
#[test]
fn a_directory_holding_a_file_is_not_removed() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("rmdir");
  std::fs::create_dir(drive.path("dir")).unwrap();
  std::fs::write(drive.path("dir\\kept.txt"), b"kept").unwrap();
  let refused = std::fs::remove_dir(drive.path("dir"));
  assert_eq!(
    refused.as_ref().map_err(|e| e.raw_os_error()),
    Err(&Some(ERROR_DIR_NOT_EMPTY)),
    "a directory with a file in it is not removed: {refused:?}"
  );
  assert!(std::fs::metadata(drive.path("dir")).unwrap().is_dir());
  assert_eq!(std::fs::read(drive.path("dir\\kept.txt")).unwrap(), b"kept");
  assert_eq!(
    drive.read_through_slates("dir/kept.txt").as_deref(),
    Some("kept")
  );
  std::fs::remove_file(drive.path("dir\\kept.txt")).unwrap();
  std::fs::remove_dir(drive.path("dir")).unwrap();
  assert!(std::fs::metadata(drive.path("dir")).is_err());
}

/// `CreateFileW(path, GENERIC_WRITE, 0, null, disposition, 0, null)` closed at once, as winfstest's `CreateFile`
/// makes it: Ok, or the Win32 error it set.
fn create_file(path: &str, disposition: u32) -> Result<(), i32> {
  use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
  use windows_sys::Win32::Storage::FileSystem::CreateFileW;
  let path = wide(path);
  // SAFETY: `path` is a NUL-terminated UTF-16 string that lives across the call; the null security attributes and
  // template are allowed by the API.
  let handle = unsafe {
    CreateFileW(
      path.as_ptr(),
      GENERIC_WRITE,
      0,
      std::ptr::null(),
      disposition,
      0,
      std::ptr::null_mut(),
    )
  };
  if handle == INVALID_HANDLE_VALUE {
    return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1));
  }
  // SAFETY: a handle this call opened and nothing else holds.
  unsafe { CloseHandle(handle) };
  Ok(())
}

/// A-214, `t/base/05.t` case 12. Do: as the script does, make a file on the drive, extend it to 42 bytes
/// (`SetEndOfFile`, `File::set_len`), then open it with `TRUNCATE_EXISTING` and close it. Expect: the file is empty,
/// through Windows and through slates.
#[test]
fn an_open_that_truncates_leaves_the_file_empty() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("trunc");
  let file = std::fs::File::create(drive.path("file.bin")).unwrap();
  file.set_len(FILE_BYTES as u64).unwrap();
  drop(file);
  assert_eq!(
    std::fs::metadata(drive.path("file.bin")).unwrap().len(),
    FILE_BYTES as u64
  );
  assert_eq!(
    create_file(&drive.path("file.bin"), TRUNCATE_EXISTING),
    Ok(())
  );
  assert_eq!(
    std::fs::metadata(drive.path("file.bin")).unwrap().len(),
    0,
    "an open that truncates empties the file"
  );
  assert_eq!(drive.read_through_slates("file.bin").as_deref(), Some(""));
}

/// A-214, `t/base/06.t` case 6. Do: make a file on the drive and set its write and access times through its handle
/// (`SetFileTime`, `File::set_times`), close it, and read its times back through a fresh look. Expect: the times set,
/// to the second.
#[test]
fn a_files_write_and_access_times_set_through_the_drive_are_kept() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("times");
  let modified = std::time::UNIX_EPOCH + Duration::from_secs(SET_MODIFIED_SECONDS);
  let accessed = std::time::UNIX_EPOCH + Duration::from_secs(SET_ACCESSED_SECONDS);
  let file = std::fs::File::create(drive.path("timed.txt")).unwrap();
  file
    .set_times(
      std::fs::FileTimes::new()
        .set_modified(modified)
        .set_accessed(accessed),
    )
    .unwrap();
  drop(file);
  let seen = std::fs::metadata(drive.path("timed.txt")).unwrap();
  assert_eq!(
    seen.modified().unwrap(),
    modified,
    "the write time set is kept"
  );
  assert_eq!(
    seen.accessed().unwrap(),
    accessed,
    "the access time set is kept"
  );
}
