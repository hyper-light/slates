//! Win32 file semantics through the WinFsp drive that winfstest found broken (A-209; §4.6 "Windows";
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
//!   neither. Its creation time must be kept too (`t/base/06.t` case 5, A-259): the volume keeps a birth time, but the
//!   drive reported the change time as the creation time and dropped the one set.
//! - A file shrunk and then extended must reach its new size: the WinFsp fsx port wedges at an extension that follows
//!   a cached write past a hole and a shrink below it (its operations 162 to 164 at seed 1 in diagnostics run
//!   41989365843, 41 to 43 in later runs). The port extends through the C runtime's `_chsize`, which writes zeros from
//!   the end until the size is reached. These two replays pass: alone, the operations do not wedge (an open bug).
//! - `CreateSymbolicLinkW` must make a link Windows follows and reads back (`t/reparse/02.t` and `03.t`, A-275), as
//!   git and npm make links on Windows; the drive answered `ERROR_INVALID_FUNCTION`, since it served no reparse point.
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
// Every `std::fs` call here is a Windows program's call on the slates drive (a RAM volume behind `E:`), which is what
// the tests are for; none touches the host's disk (R1).
#![allow(clippy::disallowed_methods)]

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
/// Format: the creation time the creation-time test sets: a day before [`SET_MODIFIED_SECONDS`], and earlier than any
/// time the drive could stamp on a new file, so a kept creation time cannot be mistaken for one the drive made.
const SET_CREATED_SECONDS: u64 = SET_MODIFIED_SECONDS - 86_400;
/// Format: the file sizes and the write of fsx's operations 157 to 164 at seed 1, which wedged the drive at 164: the
/// size before (157), the shrink (161), the write past the end that leaves a hole (162: 28,320 bytes at 205,924), the
/// second shrink (163) and the extension (164).
const FSX_SIZE_BEFORE: u64 = 196_692;
/// Format: see [`FSX_SIZE_BEFORE`].
const FSX_SHRUNK: u64 = 41_688;
/// Format: see [`FSX_SIZE_BEFORE`].
const FSX_WRITE_AT: u64 = 205_924;
/// Format: see [`FSX_SIZE_BEFORE`].
const FSX_WRITE_BYTES: usize = 28_320;
/// Format: see [`FSX_SIZE_BEFORE`].
const FSX_SHRUNK_AGAIN: u64 = 158_132;
/// Format: see [`FSX_SIZE_BEFORE`].
const FSX_EXTENDED: u64 = 246_956;
/// Format: the zero block the C runtime's `_chsize` extends a file with (its `_INTERNAL_BUFSIZ`, 4096 bytes).
const CHSIZE_BLOCK: usize = 4096;
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

/// A-209, `t/base/04.t` cases 7 and 23. Do: make two files with different bytes on the drive, and move the first onto
/// the second with `MoveFileExW` and no `MOVEFILE_REPLACE_EXISTING`. Expect: `ERROR_ALREADY_EXISTS`, and both files
/// still there with their own bytes, through Windows and through slates; then with `MOVEFILE_REPLACE_EXISTING` the
/// move succeeds and the target holds the source's bytes.
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
}

/// A-209, `t/base/04.t` case 26. Do: make two directories on the drive and move one onto the other with
/// `MOVEFILE_REPLACE_EXISTING`. Expect: `ERROR_ACCESS_DENIED`, as Windows never replaces a directory, and both stay.
#[test]
fn a_move_asked_to_replace_a_directory_is_refused() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("movedir");
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

/// A-209, `t/base/03.t` case 3. Do: make a directory holding a file on the drive and remove the directory
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

/// A-209, `t/base/05.t` case 12. Do: as the script does, make a file on the drive, extend it to 42 bytes
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

/// A-209, `t/base/06.t` case 6. Do: make a file on the drive and set its write and access times through its handle
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

/// A-259, `t/base/06.t` case 5. Do: make a file on the drive, set its creation time through its handle
/// (`SetFileTime`, `FileTimesExt::set_created`), close it, and read its times back through a fresh look. Expect: the
/// creation time set, to the second, and the write time left as it was. Before, the drive reported each object's change
/// time as its creation time, and `SetBasicInfo` dropped the creation time it was given, so the read back was the
/// moment of the set.
#[test]
fn a_files_creation_time_set_through_the_drive_is_kept() {
  use std::os::windows::fs::FileTimesExt;
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("created");
  let created = std::time::UNIX_EPOCH + Duration::from_secs(SET_CREATED_SECONDS);
  let file = std::fs::File::create(drive.path("born.txt")).unwrap();
  let written = file.metadata().unwrap().modified().unwrap();
  file
    .set_times(std::fs::FileTimes::new().set_created(created))
    .unwrap();
  drop(file);
  let seen = std::fs::metadata(drive.path("born.txt")).unwrap();
  assert_eq!(
    seen.created().unwrap(),
    created,
    "the creation time set is kept"
  );
  assert_eq!(
    seen.modified().unwrap(),
    written,
    "setting the creation time left the write time as it was"
  );
}

/// Runs `steps` on a thread and waits [`WAIT_FOR`] for its answer: a call that wedges the drive fails the test with
/// `what` instead of holding it forever. A wedged thread is left behind holding its handle; the test has failed.
fn within_wait<T: Send + 'static>(what: &str, steps: impl FnOnce() -> T + Send + 'static) -> T {
  let (done, answer) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    let _ = done.send(steps());
  });
  answer
    .recv_timeout(WAIT_FOR)
    .unwrap_or_else(|_| panic!("{what} did not answer within {WAIT_FOR:?}"))
}

/// fsx's operations 157 to 163 at seed 1 on `path`: the file at its size, shrunk, written past its end, shrunk again.
fn fsx_shrink_write_shrink(path: &str) -> std::fs::File {
  use std::os::windows::fs::FileExt;
  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(true)
    .open(path)
    .unwrap();
  file.set_len(FSX_SIZE_BEFORE).unwrap();
  file.set_len(FSX_SHRUNK).unwrap();
  assert_eq!(
    file
      .seek_write(&[b'w'; FSX_WRITE_BYTES], FSX_WRITE_AT)
      .unwrap(),
    FSX_WRITE_BYTES
  );
  file.set_len(FSX_SHRUNK_AGAIN).unwrap();
  file
}

/// A-209, fsx's operation 164 by `SetEndOfFile`. Do: fsx's operations 157 to 163 on a file on the drive, then extend it
/// to 246,956 bytes with `File::set_len`. Expect: the extension answers within the wait, and the file reports the size.
#[test]
fn a_file_shrunk_then_extended_by_its_end_reaches_the_size() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("extend");
  let path = drive.path("fsx.bin");
  let file = fsx_shrink_write_shrink(&path);
  let len = within_wait("the extension by SetEndOfFile", move || {
    file.set_len(FSX_EXTENDED).unwrap();
    file.metadata().unwrap().len()
  });
  assert_eq!(len, FSX_EXTENDED);
  assert_eq!(std::fs::metadata(&path).unwrap().len(), FSX_EXTENDED);
}

/// A-209, fsx's operation 164 as the port makes it. Do: fsx's operations 157 to 163 on a file on the drive, then extend
/// it to 246,956 bytes as the C runtime's `_chsize` does, writing 4096-byte blocks of zeros from the end. Expect: every
/// write answers the bytes it was given (a write answering none would loop `_chsize` forever), the extension answers
/// within the wait, the file reports the size, and the extended range reads as zeros.
#[test]
fn a_file_shrunk_then_extended_by_zero_writes_reaches_the_size() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("chsize");
  let path = drive.path("fsx.bin");
  let file = fsx_shrink_write_shrink(&path);
  let file = within_wait("the extension by zero writes", move || {
    use std::os::windows::fs::FileExt;
    let mut at = file.metadata().unwrap().len();
    while at < FSX_EXTENDED {
      let block = usize::try_from(FSX_EXTENDED - at)
        .unwrap()
        .min(CHSIZE_BLOCK);
      let wrote = file.seek_write(&[0u8; CHSIZE_BLOCK][..block], at).unwrap();
      assert_eq!(
        wrote, block,
        "a write at {at} answered {wrote} of {block} bytes"
      );
      at += wrote as u64;
    }
    file
  });
  assert_eq!(file.metadata().unwrap().len(), FSX_EXTENDED);
  drop(file);
  let bytes = std::fs::read(&path).unwrap();
  assert_eq!(bytes.len() as u64, FSX_EXTENDED);
  let extended = &bytes[usize::try_from(FSX_SHRUNK_AGAIN).unwrap()..];
  assert!(
    extended.iter().all(|byte| *byte == 0),
    "the extension reads as zeros"
  );
}

/// A-262, `t/base/01.t` and `07.t`. Do: create a file on the drive with `FILE_ATTRIBUTE_HIDDEN`, read its attributes,
/// mark it read-only (`SetFileAttributes` through `set_permissions`), try to delete it, clear its read-only state and
/// delete it. Expect: the file reports `HIDDEN | ARCHIVE` (kept, and archived as NTFS marks a new file), then
/// `READONLY` beside them; its delete is refused `ERROR_ACCESS_DENIED` while read-only; once cleared it deletes. Before,
/// every file reported `NORMAL` and a read-only file deleted.
#[test]
fn a_files_attributes_are_kept_and_its_read_only_state_refuses_a_delete() {
  use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
  /// Format: `FILE_ATTRIBUTE_READONLY`, `HIDDEN`, `ARCHIVE` and `NORMAL`.
  const READONLY: u32 = 0x1;
  const HIDDEN: u32 = 0x2;
  const ARCHIVE: u32 = 0x20;
  const NORMAL: u32 = 0x80;
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("attributes");
  let path = drive.path("kept.txt");
  drop(
    std::fs::OpenOptions::new()
      .write(true)
      .create_new(true)
      .attributes(HIDDEN)
      .open(&path)
      .unwrap(),
  );
  let attributes = || std::fs::metadata(&path).unwrap().file_attributes();
  assert_eq!(
    attributes() & (READONLY | HIDDEN | ARCHIVE | NORMAL),
    HIDDEN | ARCHIVE,
    "a file created hidden is kept hidden, and archived"
  );
  let mut permissions = std::fs::metadata(&path).unwrap().permissions();
  permissions.set_readonly(true);
  std::fs::set_permissions(&path, permissions.clone()).unwrap();
  assert_eq!(
    attributes() & (READONLY | HIDDEN | ARCHIVE),
    READONLY | HIDDEN | ARCHIVE,
    "read-only beside the kept bits"
  );
  assert_eq!(
    std::fs::remove_file(&path).unwrap_err().raw_os_error(),
    Some(ERROR_ACCESS_DENIED),
    "a read-only file refuses its delete"
  );
  // On Windows this clears `FILE_ATTRIBUTE_READONLY` alone; the lint's world-writable mode is Unix's.
  #[allow(clippy::permissions_set_readonly_false)]
  permissions.set_readonly(false);
  std::fs::set_permissions(&path, permissions).unwrap();
  std::fs::remove_file(&path).unwrap();
}

/// A-275, `t/reparse/02.t` and `03.t`. Do: on the drive, make a file and a directory holding a file, then link to them
/// as git and npm on Windows do, through `CreateSymbolicLinkW` (std's `symlink_file` and `symlink_dir`): a relative file
/// link, a relative directory link and an absolute file link; read through each, read each link back, then remove the
/// links. Expect: each link is made; reading through it gives its target's bytes; `read_link` gives the target it was
/// made with; the file links report `REPARSE_POINT` and the directory link `DIRECTORY | REPARSE_POINT`; a link made
/// before its target exists is followed once the target is made; removing a link leaves its target. Before, every
/// `CreateSymbolicLinkW` on the drive answered `ERROR_INVALID_FUNCTION`.
/// Format: `FILE_ATTRIBUTE_DIRECTORY` and `FILE_ATTRIBUTE_REPARSE_POINT`, the bits that tell a link's kind.
const LINK_KIND_BITS: (u32, u32) = (0x10, 0x400);

/// The directory and reparse-point bits of the drive's object `name`, the object itself rather than what it links to.
fn link_kind(drive: &Drive, name: &str) -> u32 {
  use std::os::windows::fs::MetadataExt;
  let (directory, reparse_point) = LINK_KIND_BITS;
  std::fs::symlink_metadata(drive.path(name))
    .unwrap()
    .file_attributes()
    & (directory | reparse_point)
}

/// The relative and absolute file links of the symbolic-link test: made, read through, read back.
fn file_links_are_followed(drive: &Drive) {
  use std::os::windows::fs::symlink_file;
  symlink_file("target.txt", drive.path("file-link")).expect("a relative file link is made");
  assert_eq!(
    std::fs::read(drive.path("file-link")).unwrap(),
    b"the target"
  );
  assert_eq!(
    std::fs::read_link(drive.path("file-link")).unwrap(),
    std::path::Path::new("target.txt")
  );
  assert_eq!(
    link_kind(drive, "file-link"),
    LINK_KIND_BITS.1,
    "a file link"
  );
  let absolute = drive.path("target.txt");
  symlink_file(&absolute, drive.path("absolute-link")).expect("an absolute file link is made");
  assert_eq!(
    std::fs::read(drive.path("absolute-link")).unwrap(),
    b"the target"
  );
  assert_eq!(
    std::fs::read_link(drive.path("absolute-link")).unwrap(),
    std::path::Path::new(&absolute)
  );
}

/// The directory link of the symbolic-link test, and a link made before its target.
fn directory_and_early_links_are_followed(drive: &Drive) {
  use std::os::windows::fs::{symlink_dir, symlink_file};
  symlink_dir("lib", drive.path("dir-link")).expect("a relative directory link is made");
  assert_eq!(
    std::fs::read(drive.path("dir-link\\inner.txt")).unwrap(),
    b"inside lib"
  );
  assert_eq!(
    std::fs::read_link(drive.path("dir-link")).unwrap(),
    std::path::Path::new("lib")
  );
  let (directory, reparse_point) = LINK_KIND_BITS;
  assert_eq!(
    link_kind(drive, "dir-link"),
    directory | reparse_point,
    "a directory link"
  );
  symlink_file("later.txt", drive.path("early-link")).expect("a link to a name not yet made");
  assert!(
    std::fs::read(drive.path("early-link")).is_err(),
    "a dangling link opens nothing"
  );
  std::fs::write(drive.path("later.txt"), b"made later").unwrap();
  assert_eq!(
    std::fs::read(drive.path("early-link")).unwrap(),
    b"made later"
  );
}

/// A-275, `t/reparse/02.t` and `03.t`. Do: on the drive, make a file and a directory holding a file, then link to them
/// as git and npm on Windows do, through `CreateSymbolicLinkW` (std's `symlink_file` and `symlink_dir`): a relative file
/// link, an absolute file link, a relative directory link and a link made before its target; read through each, read
/// each link back, then remove the links. Expect: each link is made; reading through it gives its target's bytes;
/// `read_link` gives the target it was made with; the file links report `REPARSE_POINT` and the directory link
/// `DIRECTORY | REPARSE_POINT`; the early link opens nothing until its target is made; removing a link leaves its
/// target. Before, every `CreateSymbolicLinkW` on the drive answered `ERROR_INVALID_FUNCTION`.
#[test]
fn symbolic_links_made_through_the_drive_are_followed_and_read_back() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("links");
  // A-275's SymlinkEvaluation class, measured where the links are followed: which evaluations the machine allows, and
  // what kind of drive Windows takes the slates drive for (a local disk's links are local, L2L and L2R).
  for args in [
    &["behavior", "query", "SymlinkEvaluation"][..],
    &["fsinfo", "drivetype", &drive.letter][..],
  ] {
    let said = Command::new("fsutil").args(args).output().unwrap();
    eprintln!(
      "A-275 fsutil {}: {}{}",
      args.join(" "),
      String::from_utf8_lossy(&said.stdout).trim(),
      String::from_utf8_lossy(&said.stderr).trim()
    );
  }
  // A-275: whether a drive link to a remote (UNC) target is followed, which the SymlinkEvaluation class L2R governs on
  // NTFS; informational, since the runner's admin share may be absent.
  let unc = std::os::windows::fs::symlink_file(UNC_PROBE, drive.path("unc-link"))
    .map(|()| std::fs::read(drive.path("unc-link")).map(|bytes| bytes.len()));
  eprintln!("A-275 a drive link to {UNC_PROBE}: {unc:?}");
  std::fs::write(drive.path("target.txt"), b"the target").unwrap();
  std::fs::create_dir(drive.path("lib")).unwrap();
  std::fs::write(drive.path("lib\\inner.txt"), b"inside lib").unwrap();
  file_links_are_followed(&drive);
  directory_and_early_links_are_followed(&drive);
  std::fs::remove_file(drive.path("file-link")).unwrap();
  std::fs::remove_file(drive.path("absolute-link")).unwrap();
  std::fs::remove_dir(drive.path("dir-link")).unwrap();
  assert_eq!(
    std::fs::read(drive.path("target.txt")).unwrap(),
    b"the target",
    "the target outlives its links"
  );
  assert_eq!(
    std::fs::read(drive.path("lib\\inner.txt")).unwrap(),
    b"inside lib"
  );
}

/// Format: a remote (UNC) file every Windows runner has, through its administrative share: the target of A-275's
/// SymlinkEvaluation probe, a local-to-remote (L2R) link on NTFS.
const UNC_PROBE: &str = "\\\\localhost\\C$\\Windows\\win.ini";

/// Runs `step` on this thread under a token holding no privilege (`CreateRestrictedToken` with `DISABLE_MAX_PRIVILEGE`),
/// as a standard user without `SeCreateSymbolicLinkPrivilege` would, then reverts to the process's token.
fn without_privileges<T>(step: impl FnOnce() -> T) -> T {
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
  use windows_sys::Win32::Security::{
    CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, DuplicateToken, RevertToSelf,
    SecurityImpersonation, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY,
  };
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, SetThreadToken,
  };
  let (mut process, mut restricted, mut impersonation): (HANDLE, HANDLE, HANDLE) = (
    std::ptr::null_mut(),
    std::ptr::null_mut(),
    std::ptr::null_mut(),
  );
  // SAFETY: each call writes only the handle out-parameter it is given, every pointer argument is null or a live local,
  // and each handle made is closed below after the thread reverts.
  unsafe {
    assert_ne!(
      OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE,
        &mut process
      ),
      0
    );
    assert_ne!(
      CreateRestrictedToken(
        process,
        DISABLE_MAX_PRIVILEGE,
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
        &mut restricted
      ),
      0
    );
    assert_ne!(
      DuplicateToken(restricted, SecurityImpersonation, &mut impersonation),
      0
    );
    assert_ne!(SetThreadToken(std::ptr::null(), impersonation), 0);
  }
  let made = step();
  // SAFETY: the thread reverts to the process's token, then closes the three handles it opened above.
  unsafe {
    assert_ne!(RevertToSelf(), 0);
    CloseHandle(impersonation);
    CloseHandle(restricted);
    CloseHandle(process);
  }
  made
}

/// `CreateSymbolicLinkW(link, target, flags)`: Ok, or the Win32 error it set.
fn create_symbolic_link(link: &str, target: &str, flags: u32) -> Result<(), i32> {
  let (link, target) = (wide(link), wide(target));
  // SAFETY: both are NUL-terminated UTF-16 strings that live across the call; the call reads them and nothing else.
  let made = unsafe {
    windows_sys::Win32::Storage::FileSystem::CreateSymbolicLinkW(
      link.as_ptr(),
      target.as_ptr(),
      flags,
    )
  };
  if made {
    Ok(())
  } else {
    Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
  }
}

/// Format: Win32's `ERROR_PRIVILEGE_NOT_HELD` (winerror.h): what `CreateSymbolicLinkW` answers on NTFS for a caller
/// without `SeCreateSymbolicLinkPrivilege` outside Developer Mode's unprivileged create.
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

/// Format: the registry key and value Developer Mode sets (`AllowDevelopmentWithoutDevLicense`, 1 when on), which
/// `CreateSymbolicLinkW`'s `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE` requires (Microsoft Learn, CreateSymbolicLinkW).
const DEVELOPER_MODE: (&str, &str) = (
  "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\AppModelUnlock",
  "AllowDevelopmentWithoutDevLicense",
);

/// Whether Developer Mode is on, as its registry value says.
fn developer_mode() -> bool {
  let output = Command::new("reg")
    .args([
      "query",
      &format!("HKLM\\{}", DEVELOPER_MODE.0),
      "/v",
      DEVELOPER_MODE.1,
    ])
    .output();
  output.is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("0x1"))
}

/// A-275 (who may make a link, NTFS's rule). Do: on the drive, as a caller whose token holds no privilege (a standard
/// user without `SeCreateSymbolicLinkPrivilege`), make a file link with no flag. Expect `ERROR_PRIVILEGE_NOT_HELD`, as
/// NTFS answers, and no link. Before A-275 set `ReparsePointsAccessCheck`, the drive made it.
#[test]
fn a_link_is_refused_to_a_caller_without_the_symlink_privilege() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("refused");
  std::fs::write(drive.path("target.txt"), b"the target").unwrap();
  let made =
    without_privileges(|| create_symbolic_link(&drive.path("refused-link"), "target.txt", 0));
  eprintln!("A-275 an unprivileged CreateSymbolicLinkW on the drive, no flag: {made:?}");
  assert_eq!(
    made,
    Err(ERROR_PRIVILEGE_NOT_HELD),
    "refused as NTFS refuses"
  );
  assert!(
    std::fs::symlink_metadata(drive.path("refused-link")).is_err(),
    "no link is left"
  );
}

/// A-275 (who may make a link, NTFS's rule). Do: on the drive, as the test's own caller (an administrator, whose token
/// holds `SeCreateSymbolicLinkPrivilege`), make a file link with no flag. Expect it made and followed.
#[test]
fn a_link_is_made_by_a_caller_holding_the_symlink_privilege() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("privileged");
  std::fs::write(drive.path("target.txt"), b"the target").unwrap();
  let made = create_symbolic_link(&drive.path("privileged-link"), "target.txt", 0);
  eprintln!("A-275 a privileged CreateSymbolicLinkW on the drive: {made:?}");
  assert_eq!(made, Ok(()));
  assert_eq!(
    std::fs::read(drive.path("privileged-link")).unwrap(),
    b"the target"
  );
}

/// A-275 (who may make a link, NTFS's rule). Do: with Developer Mode on, as a caller whose token holds no privilege, make
/// a file link with `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE`, as Git for Windows passes it
/// (`compat/mingw.c`, `adjust_symlink_flags`). Expect it made and followed, as on NTFS. Skips loudly where Developer
/// Mode is off (CI's diagnostics run turns it on for this test).
#[test]
fn a_link_is_made_without_the_privilege_under_developer_mode() {
  use windows_sys::Win32::Storage::FileSystem::SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE;
  if !live_mount_runs() {
    return;
  }
  if !developer_mode() {
    eprintln!("SKIP: Developer Mode is off, so an unprivileged create is refused on NTFS too");
    return;
  }
  let drive = Drive::mount("developer");
  std::fs::write(drive.path("target.txt"), b"the target").unwrap();
  let made = without_privileges(|| {
    create_symbolic_link(
      &drive.path("developer-link"),
      "target.txt",
      SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
    )
  });
  eprintln!("A-275 an unprivileged CreateSymbolicLinkW under Developer Mode: {made:?}");
  assert_eq!(
    made,
    Ok(()),
    "Developer Mode's unprivileged create is allowed, as on NTFS"
  );
  assert_eq!(
    std::fs::read(drive.path("developer-link")).unwrap(),
    b"the target"
  );
}

/// Format: `FSCTL_SET_REPARSE_POINT` (winioctl.h): `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 41, METHOD_BUFFERED,
/// FILE_SPECIAL_ACCESS)`.
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;

/// Makes `path` a symbolic link relative to the drive's root (`rooted`, as `\x\y`) through `FSCTL_SET_REPARSE_POINT`, as
/// a program that writes the reparse buffer itself does: `CreateSymbolicLinkW` turns a rooted target into an absolute
/// one, so it never makes this form. The buffer is MS-FSCC 2.1.2.4's, built field by field: the tag, the data length, the
/// substitute and print names (the same), and `SYMLINK_FLAG_RELATIVE`.
fn set_root_relative_link(path: &str, rooted: &str) {
  use std::os::windows::fs::OpenOptionsExt;
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
  let name: Vec<u8> = rooted.encode_utf16().flat_map(u16::to_le_bytes).collect();
  let name_len = u16::try_from(name.len()).unwrap();
  let mut buffer = Vec::new();
  buffer.extend_from_slice(&0xA000_000Cu32.to_le_bytes());
  buffer.extend_from_slice(&(12 + 2 * name_len).to_le_bytes());
  buffer.extend_from_slice(&[0, 0]);
  for field in [0, name_len, name_len, name_len] {
    buffer.extend_from_slice(&field.to_le_bytes());
  }
  buffer.extend_from_slice(&1u32.to_le_bytes());
  buffer.extend_from_slice(&name);
  buffer.extend_from_slice(&name);
  let file = std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
    .open(path)
    .unwrap();
  let mut returned = 0u32;
  // SAFETY: the handle is the open file's; the input is the live buffer of its stated length, and there is no output.
  let set = unsafe {
    windows_sys::Win32::System::IO::DeviceIoControl(
      file.as_raw_handle(),
      FSCTL_SET_REPARSE_POINT,
      buffer.as_ptr().cast(),
      u32::try_from(buffer.len()).unwrap(),
      std::ptr::null_mut(),
      0,
      &mut returned,
      std::ptr::null_mut(),
    )
  };
  assert_ne!(
    set,
    0,
    "FSCTL_SET_REPARSE_POINT: {}",
    std::io::Error::last_os_error()
  );
}

/// A-275's stated difference (a link relative to the drive's root). Do: under the drive, put `\x\y` ("at the root") and
/// `\a\x\y` ("under a"), make `\a\b\link` a link relative to the drive's root naming `\x\y`, read through it and read it
/// back; then move the link to `\a\b\c\link` and read through it again. Expect: first the root's file, and the link read
/// back as the path from its own directory (`..\..\x\y`), since POSIX has no form relative to a volume's root; after the
/// move, `\a\x\y`, the object that path reaches from the link's new directory. NTFS keeps a moved link of this form
/// naming `\x\y`.
#[test]
fn a_link_relative_to_the_drives_root_follows_its_directory_when_moved() {
  if !live_mount_runs() {
    return;
  }
  let drive = Drive::mount("rooted");
  for dir in ["x", "a", "a\\x", "a\\b", "a\\b\\c"] {
    std::fs::create_dir(drive.path(dir)).unwrap();
  }
  std::fs::write(drive.path("x\\y"), b"at the root").unwrap();
  std::fs::write(drive.path("a\\x\\y"), b"under a").unwrap();
  set_root_relative_link(&drive.path("a\\b\\link"), "\\x\\y");
  assert_eq!(
    std::fs::read(drive.path("a\\b\\link")).unwrap(),
    b"at the root"
  );
  assert_eq!(
    std::fs::read_link(drive.path("a\\b\\link")).unwrap(),
    std::path::Path::new("..\\..\\x\\y")
  );
  std::fs::rename(drive.path("a\\b\\link"), drive.path("a\\b\\c\\link")).unwrap();
  assert_eq!(
    std::fs::read(drive.path("a\\b\\c\\link")).unwrap(),
    b"under a",
    "moved a directory deeper, the link reaches what its path names from there, not the root's \\x\\y"
  );
}
