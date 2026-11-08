//! The WinFsp mount through the real command (§4.6 "Windows"; G-mount step 2): `slates anchor` supervising a real
//! `slates daemon`, `slates mount VOLUME S:` mounting the volume through WinFsp, and Windows programs reading and
//! writing it as a drive — then a change made through another path (the SDK, another attachment) seen through the
//! mount: by a cached read through a handle opened before the change, and by a mapped view
//! (`CreateFileMappingW`/`MapViewOfFile`), whose outcome the DAX matrix's Windows mount-route cell records
//! (`docs/wip/virtiofs.md` §8).
//!
//! Windows only, and gated on `SLATES_TEST_CLI=1` and `WINFSP_TEST_MOUNT=1` (the runner to itself, and WinFsp
//! installed); without them each test skips loudly and passes. CI's Windows lanes install WinFsp and set both.
#![cfg(windows)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

#[path = "../../anchor/tests/windows_access/mod.rs"]
mod windows_access;

use std::io::BufRead;
use std::os::windows::fs::FileExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shape: shards for the test daemon.
const SHARDS: &str = "2";
/// Shape: how long a change through another path gets to reach the mount: ten liveness budgets
/// (`LIVENESS_BUDGET_NS`, 1 s), as the Unix CLI tests' `WAIT_FOR`.
const WAIT_FOR: Duration = Duration::from_secs(10);
/// Shape: the pause between polls of a change or a start.
const POLL: Duration = Duration::from_millis(20);
/// Shape: consecutive answers to `volume list` a start waits for, so the anchor's one startup restart has settled.
const STABLE_STREAK: u32 = 10;
/// Shape: the most `volume list` attempts a start makes after the daemon's serving line (`crates/cli/tests/cli.rs`
/// `START_ATTEMPTS` says why attempts, not wall time).
const START_ATTEMPTS: u32 = 60;
/// Format: the start of the line the daemon prints once it serves (`crates/cli/src/daemon.rs`).
const SERVING_LINE: &str = "slates daemon: instance ";
/// Format: the file's bytes in the change tests: one page, so a cached read and a view each hold it whole.
const FILE_BYTES: usize = 4096;
/// Format: the permission bits the SDK gives the file (`rw-r--r--`, `slates write`'s default).
const FILE_MODE: u32 = 0o644;
/// Format: the CLI's EXIT_FAILED (main.rs): startup failed before the process could serve the existing instance.
const EXIT_FAILED: i32 = 4;
/// Shape: the fixture admits one startup trace of at most 64 KiB. Reaching the cap is a failed test, never a pass
/// caused by a discarded error or a child blocked writing its output.
const STARTUP_TRACE_BYTES: u64 = 64 * 1024;

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

/// A test-owned process, killed and reaped on drop. An anchor also owns a log-drain thread; a short-lived probe
/// has no drain. Killing the anchor ends its daemon too, so an assertion leaves neither process behind.
struct OwnedProcess {
  child: Child,
  drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for OwnedProcess {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    if let Some(drain) = self.drain.take() {
      let _ = drain.join();
    }
  }
}

/// Starts `slates --instance INSTANCE anchor --quick --shards SHARDS`, waits for its daemon to serve and answer a
/// streak of verbs, and bootstraps its consensus group.
fn start_anchor(instance: &str) -> OwnedProcess {
  start_anchor_of(instance, SHARDS)
}

/// [`start_anchor`] with `shards` shards.
fn start_anchor_of(instance: &str, shards: &str) -> OwnedProcess {
  let mut child = slates()
    .args([
      "--instance",
      instance,
      "anchor",
      "--quick",
      "--shards",
      shards,
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
    let read = stderr.read_line(&mut line).unwrap();
    assert!(read > 0, "the anchor ended before its daemon served");
    eprint!("{line}");
    if line.starts_with(&serving) {
      break;
    }
  }
  let drain = std::thread::spawn(move || {
    let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
  });
  let anchor = OwnedProcess {
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

/// An instance name unique to this process and `tag`, so parallel runs never share a daemon.
fn instance(tag: &str) -> String {
  format!("wf{tag}{}", std::process::id())
}

/// The first free drive letter from `E:` to `Z:` (`GetLogicalDrives` bit clear).
fn free_drive_letter() -> String {
  // SAFETY: `GetLogicalDrives` takes no arguments and returns a bitmask (bit 0 = A:, 1 = B:, …).
  let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
  let letter = ('E'..='Z')
    .find(|letter| used & (1 << (u32::from(*letter) - u32::from('A'))) == 0)
    .expect("a free drive letter");
  format!("{letter}:")
}

/// Waits until `path` answers a read (the drive registers after the mount's reply), within [`WAIT_FOR`].
fn wait_ready(path: &str) {
  let started = Instant::now();
  while started.elapsed() < WAIT_FOR {
    if std::fs::metadata(path).is_ok() {
      return;
    }
    pause();
  }
  panic!("{path} never answered within {WAIT_FOR:?}");
}

/// Runs a Windows program and asserts it succeeded: the mount's writes are made by programs (PowerShell's file
/// APIs, `cmd`), as a user's are.
fn program(name: &str, args: &[&str]) {
  let status = Command::new(name).args(args).status().unwrap();
  assert!(status.success(), "{name} {args:?} failed: {status}");
}

/// The volume id a verb printed — 32 hexadecimal characters — as the SDK takes it.
fn volume_id_of(hex: &str) -> slates_client::VolumeId {
  let mut bytes = [0u8; 16];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2).unwrap(), 16).unwrap();
  }
  slates_client::VolumeId { bytes }
}

/// Through Windows programs: create, write and read back a file, make a directory, list the root, delete both — each
/// effect checked through the drive and, for the file, through slates itself.
fn windows_programs_read_and_write(instance: &str, id: &str, root: &str) {
  let message = "hello from a slates volume served by its daemon through WinFsp";
  let file = format!("{root}hello.txt");
  program(
    "powershell",
    &[
      "-NoProfile",
      "-Command",
      &format!("[IO.File]::WriteAllText('{file}', '{message}')"),
    ],
  );
  assert_eq!(
    std::fs::read(&file).unwrap(),
    message.as_bytes(),
    "the bytes travel Windows -> WinFsp -> the owner shard and back"
  );
  let (code, read, err) = run(instance, &["read", id, "hello.txt"]);
  assert_eq!(code, 0, "{err}");
  assert_eq!(read, message, "slates itself reads what Windows wrote");

  let dir = format!("{root}sub");
  program("cmd", &["/c", "mkdir", &dir]);
  let names: Vec<String> = std::fs::read_dir(root)
    .unwrap()
    .filter_map(Result::ok)
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .collect();
  assert!(
    names.iter().any(|n| n == "hello.txt"),
    "the file lists: {names:?}"
  );
  assert!(
    names.iter().any(|n| n == "sub"),
    "the directory lists: {names:?}"
  );

  program("cmd", &["/c", "del", "/q", &file]);
  program("cmd", &["/c", "rmdir", &dir]);
  assert!(std::fs::metadata(&file).is_err(), "the file is gone");
  let (code, _, _) = run(instance, &["read", id, "hello.txt"]);
  assert_ne!(code, 0, "and gone from the volume");
}

/// §4.6 "Windows", G-mount step 2. Do: start a daemon, make a volume, `slates mount` it at a free drive letter, then
/// through Windows programs create a file, write it, make a directory, list the root, read the file back, delete both,
/// and `slates unmount` the drive. Expect: the bytes read back through the drive and through slates itself are the
/// bytes written, both names list, the deletes take, and after the unmount the drive is gone.
#[test]
fn a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("rw");
  let _anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "winfsp", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let drive = free_drive_letter();
  let (code, out, err) = run(&instance, &["mount", &id, &drive]);
  assert_eq!(code, 0, "mount: {err}");
  assert_eq!(value_of(&out, "mounted"), drive);
  let root = format!("{drive}\\");
  wait_ready(&root);

  windows_programs_read_and_write(&instance, &id, &root);

  let (code, _, err) = run(&instance, &["unmount", &drive]);
  assert_eq!(code, 0, "unmount: {err}");
  let started = Instant::now();
  while std::fs::metadata(&root).is_ok() || drive_target(&drive).is_some() {
    assert!(
      started.elapsed() < WAIT_FOR,
      "the drive is gone after the unmount"
    );
    pause();
  }
}

/// A volume mounted read-only at a drive, with a page-long `file.bin` written through an SDK client whose write
/// attachment is the other path.
struct ChangedElsewhere {
  instance: String,
  writer: slates_client::Client,
  volume: slates_client::VolumeId,
  attachment: u64,
  file: String,
  _anchor: OwnedProcess,
}

impl ChangedElsewhere {
  /// Starts a daemon, writes `file.bin` as `seed` bytes through the SDK, and mounts the volume read-only.
  fn new(tag: &str, seed: u8) -> ChangedElsewhere {
    let instance = instance(tag);
    let anchor = start_anchor(&instance);
    let (code, out, err) = run(&instance, &["volume", "create", tag, "--bounded", "64MiB"]);
    assert_eq!(code, 0, "{err}");
    let hex = value_of(&out, "id");
    let volume = volume_id_of(&hex);
    let deadlines = slates_client::Deadlines::derive(
      slates_server::daemon::LIVENESS_BUDGET_NS,
      slates_db::replay::RECOVERY_BUDGET_NS,
    )
    .get();
    let mut writer = slates_client::Client::connect(&instance, deadlines).unwrap();
    let attachment = writer
      .attach(volume, None, slates_client::Intent::Write)
      .unwrap()
      .attachment;
    writer
      .fs_write(
        (volume, attachment),
        "file.bin",
        &[seed; FILE_BYTES],
        FILE_MODE,
      )
      .unwrap();
    let drive = free_drive_letter();
    let (code, _, err) = run(&instance, &["mount", &hex, &drive, "--read-only"]);
    assert_eq!(code, 0, "mount: {err}");
    let root = format!("{drive}\\");
    wait_ready(&root);
    ChangedElsewhere {
      instance,
      writer,
      volume,
      attachment,
      file: format!("{root}file.bin"),
      _anchor: anchor,
    }
  }

  /// Rewrites `file.bin` in place as `fill` bytes through the SDK — the same file, its length unchanged.
  fn change_through_slates(&mut self, fill: u8) {
    self
      .writer
      .fs_write(
        (self.volume, self.attachment),
        "file.bin",
        &[fill; FILE_BYTES],
        FILE_MODE,
      )
      .unwrap();
  }
}

/// Reads the whole page through `handle` at offset zero.
fn read_page(handle: &std::fs::File) -> Vec<u8> {
  let mut page = vec![0u8; FILE_BYTES];
  let read = handle.seek_read(&mut page, 0).unwrap();
  page.truncate(read);
  page
}

/// Format: the mount reports 4096-byte sectors. Both the address and transfer length of an unbuffered read are
/// sector-aligned, so it must reach the successor rather than be answered by Windows' cached page.
#[repr(align(4096))]
struct SectorPage([u8; FILE_BYTES]);

fn read_uncached_page(handle: &std::fs::File) -> std::io::Result<Vec<u8>> {
  let mut page = SectorPage([0; FILE_BYTES]);
  let read = handle.seek_read(&mut page.0, 0)?;
  Ok(page.0[..read].to_vec())
}

/// Polls `look` until every byte it returns is `fill`, within [`WAIT_FOR`]; the time it took, or `None`.
fn follows(fill: u8, mut look: impl FnMut() -> Vec<u8>) -> Option<Duration> {
  let started = Instant::now();
  loop {
    let bytes = look();
    if bytes.len() == FILE_BYTES && bytes.iter().all(|byte| *byte == fill) {
      return Some(started.elapsed());
    }
    if started.elapsed() >= WAIT_FOR {
      return None;
    }
    pause();
  }
}

/// §4.6 "Windows" cache posture (`FileInfoTimeout` infinite with `FspFileSystemNotify`), G-mount step 2. Do: open
/// `file.bin` through the drive and read it twice (the second read served from Windows' cache), then rewrite it in
/// place through the SDK, and read again through the same handle. Expect: the new bytes within [`WAIT_FOR`]: the mount
/// told Windows of the change, which purged the file's cached pages, so the next read reached the shard.
#[test]
fn a_cached_read_through_a_winfsp_mount_shows_another_paths_change() {
  if !live_mount_runs() {
    return;
  }
  let mut fixture = ChangedElsewhere::new("cached", b'A');
  let handle = std::fs::File::open(&fixture.file).unwrap();
  assert_eq!(read_page(&handle), vec![b'A'; FILE_BYTES]);
  assert_eq!(
    read_page(&handle),
    vec![b'A'; FILE_BYTES],
    "read again, from the cache"
  );
  fixture.change_through_slates(b'B');
  let took = follows(b'B', || read_page(&handle));
  eprintln!(
    "WINFSP CACHED READ: {}",
    took.map_or_else(
      || format!("stale after {WAIT_FOR:?}"),
      |took| format!("followed the SDK's change in {took:?}")
    )
  );
  assert!(
    took.is_some(),
    "a cached read through the same handle shows the SDK's change within {WAIT_FOR:?}"
  );
  let _ = &fixture.instance;
}

/// A read-only view of the first page of an open file (`CreateFileMappingW` `PAGE_READONLY`, `MapViewOfFile`
/// `FILE_MAP_READ`), unmapped and closed on drop.
struct MappedView {
  mapping: windows_sys::Win32::Foundation::HANDLE,
  view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
}

impl MappedView {
  fn of(file: &std::fs::File) -> MappedView {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Memory::{
      CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY,
    };
    // SAFETY: `file` is an open handle for the call; no name, default security, the file's own size.
    let mapping = unsafe {
      CreateFileMappingW(
        file.as_raw_handle(),
        std::ptr::null(),
        PAGE_READONLY,
        0,
        0,
        std::ptr::null(),
      )
    };
    assert!(
      !mapping.is_null(),
      "CreateFileMappingW: {}",
      std::io::Error::last_os_error()
    );
    // SAFETY: `mapping` is the section just made; map its first page for reading.
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, FILE_BYTES) };
    assert!(
      !view.Value.is_null(),
      "MapViewOfFile: {}",
      std::io::Error::last_os_error()
    );
    MappedView { mapping, view }
  }

  /// The view's page, read as memory alone (volatile, so every look touches the mapped page).
  fn page(&self) -> Vec<u8> {
    let base = self.view.Value.cast::<u8>();
    (0..FILE_BYTES)
      // SAFETY: the view maps `FILE_BYTES` readable bytes from `base` for its life.
      .map(|offset| unsafe { std::ptr::read_volatile(base.add(offset)) })
      .collect()
  }
}

impl Drop for MappedView {
  fn drop(&mut self) {
    // SAFETY: the view and the mapping were made in `of` and are released once, here.
    unsafe {
      windows_sys::Win32::System::Memory::UnmapViewOfFile(self.view);
      windows_sys::Win32::Foundation::CloseHandle(self.mapping);
    }
  }
}

/// Shape: Step 9's requested kill budget: twenty deaths of the daemon under the same drive, open handle and view.
const TAKEOVER_KILLS: u32 = 20;
/// Format: WinFsp's maximum volume name, `FSP_FSCTL_VOLUME_NAME_SIZEMAX`: 64 volume-name units plus 192 prefix units.
const VOLUME_NAME_UNITS: usize = 64 + 192;

/// The kernel target of this drive letter, so a newly mounted replacement cannot stand in for the surviving volume.
fn drive_target(drive: &str) -> Option<Vec<u16>> {
  let name: Vec<_> = drive.encode_utf16().chain(Some(0)).collect();
  let mut target = vec![0u16; VOLUME_NAME_UNITS];
  // SAFETY: `name` is terminated and `target` has the reported writable capacity in UTF-16 units.
  let length = unsafe {
    windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW(
      name.as_ptr(),
      target.as_mut_ptr(),
      u32::try_from(target.len()).unwrap(),
    )
  };
  if length == 0 {
    return None;
  }
  target.truncate(usize::try_from(length).unwrap());
  Some(target)
}

/// The serving process, through the command's public status response; absent while no daemon answers.
fn daemon_pid(instance: &str) -> Option<u32> {
  let (code, text, _) = run(instance, &["status", "--json"]);
  if code != 0 {
    return None;
  }
  let after = text.split("\"pid\":").nth(1)?;
  after
    .trim_start()
    .split(|character: char| !character.is_ascii_digit())
    .next()?
    .parse()
    .ok()
}

/// Terminates the daemon and observes its exit before checking the drive: no graceful shutdown may keep it alive.
fn kill_daemon(pid: u32) {
  use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
  use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
  use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
  };
  // SAFETY: the pid came from this fixture's status; request only termination and exit observation, no inheritance.
  let process = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
  assert!(
    !process.is_null(),
    "OpenProcess({pid}): {}",
    std::io::Error::last_os_error()
  );
  // SAFETY: this is the unique owned handle just returned by OpenProcess.
  let process = unsafe { OwnedHandle::from_raw_handle(process) };
  // SAFETY: `process` stays owned for the call, and the exit code carries no pointer.
  assert_ne!(unsafe { TerminateProcess(process.as_raw_handle(), 1) }, 0);
  // SAFETY: `process` stays owned until the bounded wait observes termination.
  let waited = unsafe {
    WaitForSingleObject(
      process.as_raw_handle(),
      u32::try_from(WAIT_FOR.as_millis()).unwrap(),
    )
  };
  assert_eq!(
    waited, WAIT_OBJECT_0,
    "the killed daemon must exit before the drive is checked"
  );
}

/// Waits for the held volume to be adopted, checking its drive target throughout the gap between daemons.
fn wait_for_successor(instance: &str, drive: &str, original: &[u16], killed: u32, iteration: u32) {
  let started = Instant::now();
  loop {
    let target = drive_target(drive);
    eprintln!(
      "WINFSP TAKEOVER: kill {iteration}/{TAKEOVER_KILLS}, pid {killed}, drive held {}",
      target.as_deref() == Some(original)
    );
    assert_eq!(
      target.as_deref(),
      Some(original),
      "the original drive must survive kill {iteration}"
    );
    if daemon_pid(instance).is_some_and(|pid| pid != killed)
      && counter(instance, "winfsp.adopted") >= 1
    {
      break;
    }
    assert!(
      started.elapsed() < WAIT_FOR,
      "no successor served after kill {iteration}"
    );
    pause();
  }
}

/// Checks the first old-handle read, then a new SDK write through the old view and a counted uncached read.
fn successor_bytes(
  fixture: &mut ChangedElsewhere,
  handle: &std::fs::File,
  view: &MappedView,
  iteration: u32,
) -> u64 {
  let next_read = read_uncached_page(handle);
  assert!(
    next_read.is_ok(),
    "the original drive must serve its old handle after kill {iteration}: {next_read:?}"
  );
  assert_eq!(
    next_read.unwrap(),
    vec![b'A' + u8::try_from(iteration - 1).unwrap(); FILE_BYTES]
  );
  // A previous SDK session's attachment ends at recovery. A fresh attachment changes the recovered volume.
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  fixture.writer = slates_client::Client::connect(&fixture.instance, deadlines).unwrap();
  fixture.attachment = fixture
    .writer
    .attach(fixture.volume, None, slates_client::Intent::Write)
    .unwrap()
    .attachment;
  let fill = b'A' + u8::try_from(iteration).unwrap();
  fixture.change_through_slates(fill);
  assert!(
    follows(fill, || view.page()).is_some(),
    "the original view must follow the successor after kill {iteration}"
  );
  let before = counter(&fixture.instance, "winfsp.restored_reads");
  assert_eq!(
    read_uncached_page(handle).unwrap(),
    vec![fill; FILE_BYTES],
    "the original handle's next read must return the successor's bytes after kill {iteration}"
  );
  let served = counter_reaching(&fixture.instance, "winfsp.restored_reads", before + 1);
  assert!(
    served > before,
    "the successor must serve the original open after kill {iteration}"
  );
  served
}

/// A-198, Step 9.3. Do: keep one handle and one mapped view open through twenty killed daemons. Expect: the same
/// kernel volume target throughout; each successor serves the original handle, and its SDK write reaches the old
/// mapped view. Dropping the volume's held control handle is the diagnostic red-check of this unchanged oracle.
#[test]
fn a_winfsp_drive_handle_and_view_survive_twenty_killed_daemons() {
  use std::os::windows::fs::OpenOptionsExt;
  use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
  if !live_mount_runs() {
    return;
  }
  let mut fixture = ChangedElsewhere::new("takeover", b'A');
  let drive = fixture.file.get(..2).unwrap().to_owned();
  let original = drive_target(&drive).expect("the mounted drive names a kernel volume");
  let handle = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(FILE_FLAG_NO_BUFFERING)
    .open(&fixture.file)
    .unwrap();
  let mapped_file = std::fs::File::open(&fixture.file).unwrap();
  let view = MappedView::of(&mapped_file);
  assert_eq!(read_uncached_page(&handle).unwrap(), vec![b'A'; FILE_BYTES]);
  assert_eq!(view.page(), vec![b'A'; FILE_BYTES]);
  for iteration in 1..=TAKEOVER_KILLS {
    let killed = daemon_pid(&fixture.instance).expect("the daemon answers before the kill");
    kill_daemon(killed);
    wait_for_successor(&fixture.instance, &drive, &original, killed, iteration);
    let served = successor_bytes(&mut fixture, &handle, &view, iteration);
    assert_eq!(drive_target(&drive).as_ref(), Some(&original));
    eprintln!(
      "WINFSP TAKEOVER: completed {iteration}/{TAKEOVER_KILLS}, drives lost 0, restored reads {served}"
    );
  }
}

/// A-198/A-158. Do: keep a view of a file open, rename it through the SDK, kill the daemon, then write the renamed
/// file through the successor. Expect: the original view follows its file even though its old pathname is gone.
#[test]
fn a_winfsp_view_of_a_renamed_file_follows_its_successor() {
  if !live_mount_runs() {
    return;
  }
  let mut fixture = ChangedElsewhere::new("renamed-takeover", b'A');
  let mapped_file = std::fs::File::open(&fixture.file).unwrap();
  let view = MappedView::of(&mapped_file);
  assert_eq!(view.page(), vec![b'A'; FILE_BYTES]);
  fixture
    .writer
    .fs_rename(
      (fixture.volume, fixture.attachment),
      "file.bin",
      "renamed.bin",
    )
    .unwrap();
  let drive = fixture.file.get(..2).unwrap().to_owned();
  let original = drive_target(&drive).unwrap();
  let killed = daemon_pid(&fixture.instance).unwrap();
  kill_daemon(killed);
  wait_for_successor(&fixture.instance, &drive, &original, killed, 1);
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  fixture.writer = slates_client::Client::connect(&fixture.instance, deadlines).unwrap();
  fixture.attachment = fixture
    .writer
    .attach(fixture.volume, None, slates_client::Intent::Write)
    .unwrap()
    .attachment;
  fixture
    .writer
    .fs_write(
      (fixture.volume, fixture.attachment),
      "renamed.bin",
      &[b'B'; FILE_BYTES],
      FILE_MODE,
    )
    .unwrap();
  let followed = follows(b'B', || view.page()).is_some();
  renamed_purge_counters(&fixture.instance);
  assert!(
    std::fs::File::open(&fixture.file).is_err(),
    "a private purge must not revive the removed public name"
  );
  assert!(
    followed,
    "the pre-takeover view kept bytes from its old pathname after the file was renamed"
  );
}

/// A-190/A-198. Do: rename a viewed file through the SDK and write it at its new name, without killing its daemon.
/// Expect: the old view follows its file; the cache purge must reach the kernel node under its old name.
#[test]
fn a_winfsp_view_follows_its_file_after_an_sdk_rename() {
  if !live_mount_runs() {
    return;
  }
  let mut fixture = ChangedElsewhere::new("renamed-live", b'A');
  let mapped_file = std::fs::File::open(&fixture.file).unwrap();
  let view = MappedView::of(&mapped_file);
  assert_eq!(view.page(), vec![b'A'; FILE_BYTES]);
  fixture
    .writer
    .fs_rename(
      (fixture.volume, fixture.attachment),
      "file.bin",
      "renamed.bin",
    )
    .unwrap();
  fixture
    .writer
    .fs_write(
      (fixture.volume, fixture.attachment),
      "renamed.bin",
      &[b'B'; FILE_BYTES],
      FILE_MODE,
    )
    .unwrap();
  let followed = follows(b'B', || view.page()).is_some();
  renamed_purge_counters(&fixture.instance);
  assert!(
    std::fs::File::open(&fixture.file).is_err(),
    "a private purge must not revive the removed public name"
  );
  assert!(
    followed,
    "the live view kept bytes from its old pathname after the file was renamed"
  );
}

#[link(name = "ntdll")]
unsafe extern "system" {
  fn NtSuspendProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
  fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
}

/// The anchor's process stopped dead, as an anchor that does not answer is, and resumed on drop so that a failed
/// assertion leaves it reapable. Its daemon runs on: nothing asks the anchor but the hold channel.
struct AnchorSuspended<'a> {
  anchor: &'a OwnedProcess,
}

impl<'a> AnchorSuspended<'a> {
  fn of(anchor: &'a OwnedProcess) -> AnchorSuspended<'a> {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: a live process handle this test owns, for the call.
    let status = unsafe { NtSuspendProcess(anchor.child.as_raw_handle()) };
    assert_eq!(status, 0, "NtSuspendProcess");
    AnchorSuspended { anchor }
  }
}

impl Drop for AnchorSuspended<'_> {
  fn drop(&mut self) {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: a live process handle this test owns, for the call.
    let status = unsafe { NtResumeProcess(self.anchor.child.as_raw_handle()) };
    if status != 0 {
      eprintln!("NtResumeProcess answered {status:#x}");
    }
  }
}

/// Waits for the drive to be gone from the kernel's name space, within [`WAIT_FOR`]: whether it went.
fn drive_goes(drive: &str) -> bool {
  let started = Instant::now();
  while started.elapsed() < WAIT_FOR {
    if drive_target(drive).is_none() {
      return true;
    }
    pause();
  }
  false
}

/// Shape: unmounts whose release the anchor does not answer: the goal's 20.
const REFUSED_UNMOUNTS: u32 = 20;
/// Shape: refused holds in one daemon's life: enough to show the channel outlives each.
const REFUSED_MOUNTS: u32 = 3;
/// Shape: how long the first unmount keeps the anchor stopped: past any deadline the release's bound could be, on a
/// daemon that has just started. Before 2026-11-04 the deadline was the daemon's uptime plus a second.
const FIRST_SILENCE: Duration = Duration::from_secs(20);

/// A-206 (2026-11-04), audit step 4. Do: mount a drive beside the one under test; stop the anchor's process, unmount, and
/// read through the drive; resume the anchor; repeat with a fresh mount twenty times, the first with the anchor silent
/// past any deadline, and unmount the drive beside it after that first. Expect: every unmount is answered a typed
/// refusal while the drive it asked about still serves; once the anchor answers, the drive goes; every mount after a
/// silence succeeds; so no drive stays held by an anchor with nobody serving it. Before: the unmount answered success and
/// deleted the file system under the anchor's hold, and the deadline's pass closed the channel for the daemon's life.
#[test]
#[allow(clippy::cognitive_complexity)]
fn a_release_the_anchor_does_not_answer_leaves_no_drive_held() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("unheld");
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "unheld", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let beside = free_drive_letter();
  let (code, _, err) = run(&instance, &["mount", &id, &beside]);
  assert_eq!(code, 0, "mount beside: {err}");
  wait_ready(&format!("{beside}\\"));
  write_through_drive(&format!("{beside}\\file.bin"), &[b'A'; FILE_BYTES]).unwrap();
  let (mut answered_while_held, mut held_after, mut mounts_refused) = (0u32, 0u32, 0u32);
  for iteration in 1..=REFUSED_UNMOUNTS {
    let drive = free_drive_letter();
    let (code, _, err) = run(&instance, &["mount", &id, &drive]);
    if code != 0 {
      eprintln!("WINFSP UNHELD: iteration {iteration}: mount refused: {err}");
      mounts_refused += 1;
      continue;
    }
    let root = format!("{drive}\\");
    wait_ready(&root);
    let suspended = AnchorSuspended::of(&anchor);
    let (code, _, err) = run(&instance, &["unmount", &drive]);
    if code == 0 {
      answered_while_held += u32::from(drive_target(&drive).is_some());
    } else {
      assert!(err.contains("release"), "a typed refusal: {err}");
      assert_eq!(
        std::fs::read(format!("{root}file.bin")).unwrap(),
        vec![b'A'; FILE_BYTES],
        "the refused drive still serves"
      );
    }
    if iteration == 1 {
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(FIRST_SILENCE);
    }
    drop(suspended);
    let gone = drive_goes(&drive);
    held_after += u32::from(!gone);
    eprintln!("WINFSP UNHELD: iteration {iteration}: unmount exit {code}, drive gone {gone}");
    if iteration == 1 {
      let (code, _, err) = run(&instance, &["unmount", &beside]);
      assert_eq!(code, 0, "unmount beside: {err}");
      let gone = drive_goes(&beside);
      held_after += u32::from(!gone);
      eprintln!("WINFSP UNHELD: the drive beside gone {gone}");
    }
  }
  let refused = counter(&instance, "winfsp.hold_release_refused");
  eprintln!(
    "WINFSP UNHELD: {held_after} drives held after their unmount, {answered_while_held} answered success while held, \
     {mounts_refused} mounts refused, {refused} release attempts refused, of {REFUSED_UNMOUNTS}"
  );
  assert_eq!(held_after, 0, "no drive stays held after its unmount");
  assert_eq!(answered_while_held, 0, "no success is answered while held");
  assert_eq!(mounts_refused, 0, "the channel outlives a silence");
  assert!(
    refused >= u64::from(REFUSED_UNMOUNTS),
    "non-vacuous: each release met the silence ({refused} attempts refused)"
  );
}

/// A-206 (2026-11-04), audit step 4. Do: stop the anchor's process and mount, three times in one daemon's life, resuming
/// the anchor after each. Expect: each mount is a typed refusal naming the hold; once the anchor answers, nothing is left
/// mounted at the drive; and a mount and unmount after that succeed, so the channel outlived the refusals.
#[test]
fn a_refused_mount_time_hold_leaves_nothing_mounted() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("unmounted");
  let anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "unmounted", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  for iteration in 1..=REFUSED_MOUNTS {
    let drive = free_drive_letter();
    let suspended = AnchorSuspended::of(&anchor);
    let (code, _, err) = run(&instance, &["mount", &id, &drive]);
    eprintln!("WINFSP REFUSED HOLD: iteration {iteration}: exit {code}: {err}");
    assert_ne!(code, 0, "the mount is refused while the anchor is silent");
    assert!(
      err.contains("TargetUnavailable") && err.contains("hold"),
      "a typed refusal naming the hold: {err}"
    );
    drop(suspended);
    assert!(
      drive_goes(&drive),
      "nothing stays mounted once the anchor answers"
    );
    let (code, _, err) = run(&instance, &["mount", &id, &drive]);
    assert_eq!(code, 0, "the next mount after the refusal: {err}");
    wait_ready(&format!("{drive}\\"));
    let (code, _, err) = run(&instance, &["unmount", &drive]);
    assert_eq!(code, 0, "unmount: {err}");
    assert!(drive_goes(&drive), "the unmounted drive goes");
  }
}

/// Shape: the longest component a long-path test makes; WinFsp admits 255 (`MaxComponentLength`).
const LONG_PATH_COMPONENT: usize = 200;
/// Format: `\\?\X:`, what a verbatim path costs before the kernel's name, out of `UNICODE_STRING`'s 32,767 units with
/// the `\??\X:` it becomes: the longest name reachable through a drive letter is 32,761 units.
const VERBATIM_DRIVE_UNITS: usize = 6;

/// Writes a file through the mounted drive: what the test is about, and a volume in RAM, never the disk (R1).
#[allow(clippy::disallowed_methods)]
fn write_through_drive(path: &str, bytes: &[u8]) -> std::io::Result<()> {
  std::fs::write(path, bytes)
}

/// Makes a directory through the mounted drive, as [`write_through_drive`] writes.
#[allow(clippy::disallowed_methods)]
fn make_dir_through_drive(path: &str) -> std::io::Result<()> {
  std::fs::create_dir(path)
}

/// The components of a kernel name of exactly `units` UTF-16 units (each with its leading `\`), each within WinFsp's
/// component limit; a directory chain shared by every shorter name, so a ladder of lengths reuses it.
fn long_components(units: usize) -> Vec<String> {
  let each = LONG_PATH_COMPONENT + 1;
  let mut parts = vec!["d".repeat(LONG_PATH_COMPONENT); units / each];
  let rest = units % each;
  match (rest, parts.last_mut()) {
    (0, _) => {}
    (1, Some(last)) => {
      // A lone `\` is no component: move a unit from the last directory to the file.
      last.pop();
      parts.push("f".to_owned());
    }
    (rest, _) => parts.push("f".repeat(rest - 1)),
  }
  parts
}

/// Makes every directory of `parts` but the last under `root`, and answers the file's verbatim path.
fn long_parents(root: &str, parts: &[String]) -> std::io::Result<String> {
  let mut path = format!("\\\\?\\{root}");
  let (file, directories) = parts.split_last().unwrap();
  for directory in directories {
    path.push('\\');
    path.push_str(directory);
    match make_dir_through_drive(&path) {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
      Err(error) => return Err(error),
    }
  }
  path.push('\\');
  path.push_str(file);
  Ok(path)
}

/// Format: `ERROR_INVALID_PARAMETER`, what `STATUS_INVALID_PARAMETER` becomes: the WinFsp driver's answer to a request
/// too long to build (`FspIopCreateRequestFunnel`, `src/sys/iop.c`).
const DRIVER_REFUSED: i32 = 87;
/// Format: `ERROR_FILE_NOT_FOUND` and `ERROR_PATH_NOT_FOUND`: the file system's answer to a name it does not have, which
/// shows the request reached it.
const REACHED_THE_VOLUME: [i32; 2] = [2, 3];

/// Whether an open of a name of `units` units, which the volume does not have, reached the file system. The two answers
/// tell the driver from slates with nothing else to read: slates could only answer that the name is not there.
fn open_reaches_the_volume(root: &str, units: usize) -> bool {
  let path = format!("\\\\?\\{root}\\{}", long_components(units).join("\\"));
  let error = std::fs::File::open(&path).expect_err("a name the volume does not have");
  let code = error.raw_os_error().unwrap_or(-1);
  assert!(
    code == DRIVER_REFUSED || REACHED_THE_VOLUME.contains(&code),
    "an open of {units} units answered neither the driver's refusal nor the volume's: {error}"
  );
  code != DRIVER_REFUSED
}

/// The smallest length the driver refuses, found by halving between a length that reaches the volume and one that does
/// not, on the live driver.
fn driver_limit(root: &str) -> usize {
  let mut reaches = 1025;
  // No request can hold as many units as it has bytes, so the driver refuses this whatever its exact limit.
  let mut refused = slates_bridge_winfsp::inflight::REQUEST_MAX;
  assert!(
    open_reaches_the_volume(root, reaches),
    "1,025 units reach the volume"
  );
  assert!(
    !open_reaches_the_volume(root, refused),
    "a name as many units long as a request has bytes is refused by the driver"
  );
  while refused - reaches > 1 {
    let middle = reaches + (refused - reaches) / 2;
    if open_reaches_the_volume(root, middle) {
      reaches = middle;
    } else {
      refused = middle;
    }
  }
  refused
}

/// Creates, writes and opens again a file of `units` units through the drive, whose bytes name its length: what it
/// reads back.
fn long_path_round_trip(root: &str, units: usize) -> std::io::Result<String> {
  let parts = long_components(units);
  let path = long_parents(root, &parts)?;
  write_through_drive(&path, format!("{units}").as_bytes())?;
  drop(std::fs::File::open(&path)?);
  std::fs::read_to_string(&path)
}

/// A-206 (2026-11-04), the goal's criterion as the supervisor restated it: slates refuses no path the WinFsp driver
/// passes. Do: measure the driver's limit on this driver by halving. Expect: it is the limit the name reserve is derived
/// from (`MAX_PATH_UNITS`); a file one unit under it, and at every length on a ladder from 1,025, is created, written,
/// read back and opened again through slates; at the limit, an open with every parent in place is answered the driver's
/// status, and not the not-found that slates would answer. Before this every open past 1,024 units was refused by the
/// daemon, for the ledger's name record.
#[test]
fn opens_past_a_thousand_units_are_refused_only_by_the_winfsp_driver() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("longladder");
  let _anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "ladder", "--bounded", "256MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let drive = free_drive_letter();
  let (code, _, err) = run(&instance, &["mount", &id, &drive]);
  assert_eq!(code, 0, "mount: {err}");
  wait_ready(&format!("{drive}\\"));
  let limit = driver_limit(&drive);
  eprintln!("WINFSP LONG PATH: first refusal: the driver refuses from {limit} units");
  let longest = (u16::MAX as usize) / 2 - VERBATIM_DRIVE_UNITS;
  let at_the_longest = std::fs::File::open(format!(
    "\\\\?\\{drive}\\{}",
    long_components(longest).join("\\")
  ));
  eprintln!(
    "WINFSP LONG PATH: {longest} units, the longest a drive letter can name, answered {:?}",
    at_the_longest
      .map(drop)
      .map_err(|error| error.raw_os_error())
  );
  for units in [1025, 1026, 2000, 4000, 8000, limit - 1] {
    let read = long_path_round_trip(&drive, units);
    eprintln!("WINFSP LONG PATH: {units} units read back {read:?}");
    assert_eq!(
      read.unwrap_or_else(|error| panic!("{units} units, under the driver's limit: {error}")),
      format!("{units}"),
      "{units} units read back whole"
    );
  }
  let parts = long_components(limit);
  let at_the_limit = long_parents(&drive, &parts).unwrap();
  let refused = std::fs::File::open(&at_the_limit).unwrap_err();
  eprintln!("WINFSP LONG PATH: {limit} units with its parents in place: {refused}");
  assert_eq!(
    refused.raw_os_error(),
    Some(DRIVER_REFUSED),
    "at the limit the refusal is the driver's"
  );
  assert_eq!(
    limit,
    slates_bridge_winfsp::notify::MAX_PATH_UNITS + 1,
    "the name reserve is derived from what the driver passes"
  );
}

/// A-206 (2026-11-04). Do: through the drive, create a file whose kernel name is 2,000 UTF-16 units, write and read it,
/// keep a handle and a view of it; kill the daemon; then through the successor read the old handle, change the file
/// through the SDK and watch the old view and a fresh open. Expect: the bytes each time, the same drive throughout, and
/// a read counted through the restored context, so the 2,000-unit name was retained and told across the takeover.
#[test]
#[allow(clippy::cognitive_complexity)]
fn a_file_at_a_two_thousand_unit_path_is_served_across_a_killed_daemon() {
  use std::os::windows::fs::OpenOptionsExt;
  use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
  if !live_mount_runs() {
    return;
  }
  let instance = instance("longpath");
  let _anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "longpath", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let hex = value_of(&out, "id");
  let drive = free_drive_letter();
  let (code, _, err) = run(&instance, &["mount", &hex, &drive]);
  assert_eq!(code, 0, "mount: {err}");
  wait_ready(&format!("{drive}\\"));
  let parts = long_components(2000);
  assert_eq!(
    parts.iter().map(|part| part.len() + 1).sum::<usize>(),
    2000,
    "the kernel's name is exactly 2,000 units"
  );
  let path = long_parents(&drive, &parts).expect("the directories of a 2,000-unit path");
  write_through_drive(&path, &[b'A'; FILE_BYTES]).expect("create a 2,000-unit path");
  assert_eq!(std::fs::read(&path).unwrap(), vec![b'A'; FILE_BYTES]);
  let handle = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(FILE_FLAG_NO_BUFFERING)
    .open(&path)
    .unwrap();
  let mapped_file = std::fs::File::open(&path).unwrap();
  let view = MappedView::of(&mapped_file);
  assert_eq!(view.page(), vec![b'A'; FILE_BYTES]);
  let original = drive_target(&drive).unwrap();
  let killed = daemon_pid(&instance).unwrap();
  kill_daemon(killed);
  wait_for_successor(&instance, &drive, &original, killed, 1);
  assert_eq!(read_uncached_page(&handle).unwrap(), vec![b'A'; FILE_BYTES]);
  let volume = volume_id_of(&hex);
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let mut writer = slates_client::Client::connect(&instance, deadlines).unwrap();
  let attachment = writer
    .attach(volume, None, slates_client::Intent::Write)
    .unwrap()
    .attachment;
  writer
    .fs_write(
      (volume, attachment),
      &parts.join("/"),
      &[b'B'; FILE_BYTES],
      FILE_MODE,
    )
    .unwrap();
  assert!(
    follows(b'B', || view.page()).is_some(),
    "the old view follows the successor's change at a 2,000-unit path"
  );
  let before = counter(&instance, "winfsp.restored_reads");
  assert_eq!(read_uncached_page(&handle).unwrap(), vec![b'B'; FILE_BYTES]);
  assert!(
    counter_reaching(&instance, "winfsp.restored_reads", before + 1) > before,
    "the successor served the restored open at a 2,000-unit path"
  );
  assert_eq!(std::fs::read(&path).unwrap(), vec![b'B'; FILE_BYTES]);
  assert_eq!(
    counter(&instance, "winfsp.names_too_long"),
    0,
    "no name was left out of a round"
  );
  drop(view);
  drop(mapped_file);
  drop(handle);
  let (code, _, err) = run(&instance, &["unmount", &drive]);
  assert_eq!(code, 0, "unmount: {err}");
}

fn renamed_purge_counters(instance: &str) {
  for name in [
    PURGES_DONE,
    PURGES_REFUSED,
    PURGES_UNAVAILABLE,
    PURGES_UNTAKEN,
    PURGES_KEPT,
    PURGE_ECHOES,
  ] {
    eprintln!("WINFSP RENAMED: {name} = {}", counter(instance, name));
  }
}

/// Format: target process ids passed to the independent test role, separated by a comma.
const FOREIGN_TARGETS: &str = "SLATES_WINFSP_FOREIGN_TARGETS";
/// Shape: a probe's successful exit differs from libtest's zero when an exact filter selected no test.
const FOREIGN_EXIT: i32 = 3;

/// Waits for a short-lived owned process, bounded by the same fixture deadline as a daemon takeover.
fn waited_process(process: &mut OwnedProcess) -> std::process::ExitStatus {
  use std::os::windows::io::AsRawHandle;
  // SAFETY: the child owns a live process handle throughout the bounded wait.
  let waited = unsafe {
    windows_sys::Win32::System::Threading::WaitForSingleObject(
      process.child.as_raw_handle(),
      u32::try_from(WAIT_FOR.as_millis()).unwrap(),
    )
  };
  assert_eq!(
    waited,
    windows_sys::Win32::Foundation::WAIT_OBJECT_0,
    "the probe must exit within {WAIT_FOR:?}"
  );
  process.child.wait().unwrap()
}

/// Captures one bounded startup trace while the test-owned process runs, with the same bounded wait and reaping
/// guard as the independent access probe. The one-result channel cannot block its owned drain thread.
fn startup_failure(command: &mut Command) -> (std::process::ExitStatus, String) {
  use std::io::Read;
  let (send, receive) = std::sync::mpsc::sync_channel(1);
  let mut process = OwnedProcess {
    child: command.stderr(Stdio::piped()).spawn().unwrap(),
    drain: None,
  };
  let stderr = process.child.stderr.take().unwrap();
  process.drain = Some(std::thread::spawn(move || {
    let mut bytes = Vec::new();
    let result = stderr.take(STARTUP_TRACE_BYTES).read_to_end(&mut bytes);
    send.send((result, bytes)).unwrap();
  }));
  let exit = waited_process(&mut process);
  let (result, bytes) = receive.recv_timeout(WAIT_FOR).unwrap();
  result.unwrap();
  assert!(
    u64::try_from(bytes.len()).unwrap() < STARTUP_TRACE_BYTES,
    "startup trace reached its bound"
  );
  let text = String::from_utf8(bytes).unwrap();
  eprint!("{text}");
  (exit, text)
}

/// A-198, Step 9.4. Do: ask a process outside the anchor's descendants to open new duplication and ACL-changing
/// handles to both mount holders. Expect: Windows refuses each access with ERROR_ACCESS_DENIED, while the anchor
/// continues to serve its original drive. The probe has no inherited process or mount handle.
#[test]
fn a_process_outside_the_anchor_cannot_take_either_holders_mount_handles() {
  if !live_mount_runs() {
    return;
  }
  let fixture = ChangedElsewhere::new("foreign", b'A');
  let daemon = daemon_pid(&fixture.instance).unwrap();
  let targets = format!("{},{}", fixture._anchor.child.id(), daemon);
  let child = Command::new(std::env::current_exe().unwrap())
    .args([
      "--ignored",
      "--exact",
      "foreign_process_role",
      "--nocapture",
    ])
    .env(FOREIGN_TARGETS, targets)
    .spawn()
    .unwrap();
  let mut probe = OwnedProcess { child, drain: None };
  assert_eq!(
    waited_process(&mut probe).code(),
    Some(FOREIGN_EXIT),
    "the independent probe must execute and be refused"
  );
  assert_eq!(
    std::fs::read(&fixture.file).unwrap(),
    vec![b'A'; FILE_BYTES]
  );
}

/// A-198, Step 9.4. Do: start an independent daemon for the mounted instance without an anchor handoff. Expect:
/// the CLI's typed failure exit, the original daemon still serving, and the original drive still returning its file.
#[test]
fn a_second_daemon_cannot_take_an_instance_with_a_live_winfsp_drive() {
  if !live_mount_runs() {
    return;
  }
  let fixture = ChangedElsewhere::new("second", b'A');
  let owner = daemon_pid(&fixture.instance).unwrap();
  let mut command = slates();
  command.args([
    "--instance",
    &fixture.instance,
    "daemon",
    "--quick",
    "--shards",
    SHARDS,
  ]);
  // This process is an independent claimant, never a successor handed the live anchor's authority.
  for variable in [
    slates_anchor::segment::ENV_HANDOFF,
    slates_anchor::segment::ENV_LEN,
    slates_anchor::segment::ENV_CONTENT,
    slates_anchor::segment::ENV_CONTENT_LEN,
  ] {
    command.env_remove(variable);
  }
  let (exit, error) = startup_failure(&mut command);
  assert_eq!(
    exit.code(),
    Some(EXIT_FAILED),
    "the foreign daemon must return the CLI's startup failure exit"
  );
  assert!(
    error.contains("the name is already a live section") && error.contains("Some(183)"),
    "the second daemon must reach and fail the existing-instance ownership check: {error}"
  );
  assert_eq!(daemon_pid(&fixture.instance), Some(owner));
  assert_eq!(
    std::fs::read(&fixture.file).unwrap(),
    vec![b'A'; FILE_BYTES]
  );
  eprintln!("WINFSP FOREIGN: second daemon refused; original owner {owner} still serves");
}

/// A-198. A sibling process of the anchor, selected by its parent through libtest's exact role name.
#[test]
#[ignore = "the independent Windows access probe, re-invoked by its parent"]
fn foreign_process_role() {
  use std::os::windows::io::{FromRawHandle, OwnedHandle};
  use windows_sys::Win32::Storage::FileSystem::{WRITE_DAC, WRITE_OWNER};
  use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_DUP_HANDLE};
  let targets = std::env::var(FOREIGN_TARGETS).expect("the independent probe's targets");
  windows_access::as_unprivileged(|| {
    for target in targets.split(',') {
      let pid: u32 = target.parse().unwrap();
      for rights in [PROCESS_DUP_HANDLE, WRITE_DAC, WRITE_OWNER] {
        // SAFETY: a pid from the public fixture; no inherited handle. The kernel checks access before opening it.
        let raw = unsafe { OpenProcess(rights, 0, pid) };
        if !raw.is_null() {
          // SAFETY: OpenProcess returned a new handle owned by this role, closed before the failed assertion.
          drop(unsafe { OwnedHandle::from_raw_handle(raw) });
          panic!("WINFSP FOREIGN: process {pid} granted forbidden rights {rights:#x}");
        }
        let error = std::io::Error::last_os_error();
        assert_eq!(
          error.raw_os_error(),
          Some(i32::try_from(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED).unwrap()),
          "OpenProcess({pid}, {rights:#x}): {error}"
        );
        eprintln!("WINFSP FOREIGN: process {pid} refused rights {rights:#x}, access denied");
      }
    }
  });
  std::process::exit(FOREIGN_EXIT);
}

/// What a mapped view through the WinFsp mount does after another path's change, as the DAX matrix's Windows
/// mount-route cell records it (`docs/wip/virtiofs.md` §8): it follows (A-190). WinFsp's notification purges with
/// `CcPurgeCacheSection` (`src/sys/file.c`), which "will not purge mapped files" (Microsoft Learn,
/// `CcPurgeCacheSection`), and the view had kept its first byte for 10 s on CI's Windows runner (run 40249524066,
/// Windows Server 2025, WinFsp 2.1.25156, 2026-10-26; A-177). Since A-190 the mount writes each changed range through
/// the drive unbuffered after the round, which makes WinFsp purge with `CcCoherencyFlushAndPurgeCache` (`src/sys/write.c`),
/// documented to invalidate user mapped views, and drops the write itself (`slates_bridge_winfsp::purge`).
const MAPPED_VIEW_FOLLOWS: bool = true;

/// Format: the daemon's counter of files the mount purged from Windows' cache and its views (A-190).
const PURGES_DONE: &str = "winfsp.purges_done";
/// Format: the daemon's counter of purge writes the mount answered without applying them (A-190).
const PURGE_ECHOES: &str = "winfsp.purge_echoes";
/// Format: the daemon's counter of purges whose open or write Windows refused (A-190).
const PURGES_REFUSED: &str = "winfsp.purges_refused";
/// Format: the daemon's counter of purges not made because WinFsp cannot name an open's process (A-190).
const PURGES_UNAVAILABLE: &str = "winfsp.purges_unavailable";
/// Format: the daemon's counter of purge attempts Windows did not take, each owed again (Step 8 of the VFS and DAX plan).
const PURGES_UNTAKEN: &str = "winfsp.purges_untaken";
/// Format: the daemon's counter of purges given up after Windows kept their page until the volume's request timeout.
const PURGES_KEPT: &str = "winfsp.purges_kept";

/// The daemon's counter `name` once it reaches `floor`, asked again until [`WAIT_FOR`] passes, else its last reading. A
/// purge is counted after the view it purged has followed (its read back comes after its write), so a reading taken
/// the moment the view follows can be early.
fn counter_reaching(instance: &str, name: &str, floor: u64) -> u64 {
  let started = Instant::now();
  loop {
    let count = counter(instance, name);
    if count >= floor || started.elapsed() >= WAIT_FOR {
      return count;
    }
    pause();
  }
}

/// The daemon's counter `name`, summed over the shards `slates status` prints.
fn counter(instance: &str, name: &str) -> u64 {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "slates status: {err}");
  let named = format!("{name}: ");
  out
    .lines()
    .filter_map(|line| line.split_once(named.as_str()))
    .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
    .sum()
}

/// §4.6 "Windows", the DAX matrix's Windows mount-route cell, G-mount step 2, A-190. Do: map `file.bin` read-only through
/// the drive and read the view, then rewrite the file in place through the SDK, and watch the view as memory alone for
/// [`WAIT_FOR`]; beside it, read the file through a second handle opened after the view; then close the view and every
/// handle and open the file again. Expect: the view does what [`MAPPED_VIEW_FOLLOWS`] records (it follows), and so does
/// the read through the second handle, since the purge drops the range from Windows' cache too; and the daemon counts a
/// purge made and its write answered without being applied, so the view cannot follow by any other path. Before A-190
/// both stayed stale for 10 s on CI's runner (runs 40249524066 and 40250856246). A purge Windows did not take (a page it
/// kept, read back stale through its cache) is counted untaken and made again on its own after a wait (Step 8 of the
/// VFS and DAX plan): before that, 9 of 200 iterations under a CPU and I/O load stayed stale on CI's x86_64 runner.
#[test]
fn a_mapped_view_through_a_winfsp_mount_after_another_paths_change() {
  if !live_mount_runs() {
    return;
  }
  let mut fixture = ChangedElsewhere::new("mapped", b'A');
  let handle = std::fs::File::open(&fixture.file).unwrap();
  let view = MappedView::of(&handle);
  assert_eq!(
    view.page(),
    vec![b'A'; FILE_BYTES],
    "the view reads the volume's bytes"
  );
  fixture.change_through_slates(b'C');
  let view_took = follows(b'C', || view.page());
  let reader = std::fs::File::open(&fixture.file).unwrap();
  let read_took = follows(b'C', || read_page(&reader));
  let first = view.page().first().copied();
  eprintln!(
    "WINFSP MAPPED VIEW: {}; a read through a handle {}",
    view_took.map_or_else(
      || format!("stale after {WAIT_FOR:?} (first byte {first:?})"),
      |took| format!("followed the SDK's change in {took:?}")
    ),
    read_took.map_or_else(
      || format!("stale after {WAIT_FOR:?}"),
      |took| format!("followed it in {took:?}")
    ),
  );
  // The purge's counters are read and printed before any assertion, so a failure carries what the mount did: no purge
  // planned, one refused, or one made that did not take (main's Windows job saw the view stale once, run 41148561681).
  // A purge is counted once its read back ends, after the view has followed, so the count of purges made is awaited.
  let purges = counter_reaching(&fixture.instance, PURGES_DONE, 1);
  let (echoes, refused, unavailable, untaken, kept) = (
    counter(&fixture.instance, PURGE_ECHOES),
    counter(&fixture.instance, PURGES_REFUSED),
    counter(&fixture.instance, PURGES_UNAVAILABLE),
    counter(&fixture.instance, PURGES_UNTAKEN),
    counter(&fixture.instance, PURGES_KEPT),
  );
  eprintln!(
    "WINFSP PURGES: {purges} made, {echoes} writes answered without being applied, {refused} refused, {unavailable} \
     unavailable, {untaken} untaken and made again, {kept} given up"
  );
  assert_eq!(
    view_took.is_some(),
    MAPPED_VIEW_FOLLOWS,
    "the mapped view does what the matrix records ({purges} purges, {refused} refused, {unavailable} unavailable)"
  );
  assert!(
    read_took.is_some(),
    "a read through a second handle follows the change once the range is purged"
  );
  assert!(
    purges >= 1 && echoes >= 1,
    "non-vacuous: the mount purged the file and dropped the purge's write ({purges} purges, {echoes} echoes)"
  );
  drop(view);
  drop(reader);
  drop(handle);
  let fresh_took = follows(b'C', || {
    std::fs::File::open(&fixture.file)
      .map(|fresh| read_page(&fresh))
      .unwrap_or_default()
  });
  eprintln!(
    "WINFSP AFTER THE VIEW: {}",
    fresh_took.map_or_else(
      || format!("a fresh open still stale after {WAIT_FOR:?}"),
      |took| format!("a fresh open read the change in {took:?}")
    )
  );
}

/// Shape: how long one container command may take, its image's pull included: the bound the Linux container leg of
/// `crates/cli/tests/cli.rs` keeps (`CONTAINER_WAIT`, its first emulated run measured past 90 s at the memory wall).
const CONTAINER_WAIT: Duration = Duration::from_secs(300);

/// Format: the image a process-isolated container runs on each host build: a process-isolated container runs only on
/// a host of its own build (Microsoft Learn, "Windows container version compatibility"), and Nano Server is the
/// smallest image that has `cmd`. Windows Server 2025 is build 26100, Windows Server 2022 build 20348.
const NANOSERVER_BY_BUILD: [(&str, &str); 2] = [
  ("26100", "mcr.microsoft.com/windows/nanoserver:ltsc2025"),
  ("20348", "mcr.microsoft.com/windows/nanoserver:ltsc2022"),
];

/// Reads `pipe` to its end on a thread of its own, so a child that fills it never stops; what it held, with a note if
/// it could not be read whole.
fn gather<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
  std::thread::spawn(move || {
    let mut text = String::new();
    if let Some(mut pipe) = pipe
      && let Err(why) = pipe.read_to_string(&mut text)
    {
      text.push_str(&format!("\n(the output could not be read whole: {why})"));
    }
    text
  })
}

/// Runs `command` to its end within `wait`, its two outputs read as they come; its exit code and outputs, or why it gave
/// none (it never started, or it was killed at the bound, with what it had printed).
fn bounded_output(command: &mut Command, wait: Duration) -> Result<(i32, String, String), String> {
  let mut child = command
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("spawn: {e}"))?;
  let out = gather(child.stdout.take());
  let err = gather(child.stderr.take());
  let started = Instant::now();
  let ended = loop {
    match child.try_wait() {
      Ok(Some(status)) => break Ok(status.code().unwrap_or(-1)),
      Ok(None) if started.elapsed() > wait => {
        let killed = child
          .kill()
          .err()
          .map(|e| format!("; the kill refused: {e}"));
        let reaped = child
          .wait()
          .err()
          .map(|e| format!("; the reap refused: {e}"));
        break Err(format!(
          "killed after {wait:?}{}{}",
          killed.unwrap_or_default(),
          reaped.unwrap_or_default()
        ));
      }
      Ok(None) => pause(),
      Err(why) => break Err(format!("wait: {why}")),
    }
  };
  let out = out
    .join()
    .map_err(|_| "the output's reader ended in a panic".to_owned())?;
  let err = err
    .join()
    .map_err(|_| "the error output's reader ended in a panic".to_owned())?;
  match ended {
    Ok(code) => Ok((code, out, err)),
    Err(why) => Err(format!("{why}; it had printed: {out} / {err}")),
  }
}

/// The Nano Server image a process-isolated container runs here: Docker's engine answering in Windows mode, and an
/// image built for this host's build; else `None`, the loud skip printed.
fn process_isolated_image() -> Option<&'static str> {
  match bounded_output(
    Command::new("docker").args(["version", "--format", "{{.Server.Os}}"]),
    CONTAINER_WAIT,
  ) {
    Ok((0, os, _)) if os.trim() == "windows" => {}
    other => {
      eprintln!(
        "SKIP: the container leg needs Docker's engine in Windows-container mode: {other:?}"
      );
      return None;
    }
  }
  // `ver` prints "Microsoft Windows [Version 10.0.BUILD.REVISION]".
  let (_, ver, _) = bounded_output(Command::new("cmd").args(["/c", "ver"]), WAIT_FOR).ok()?;
  let build = ver
    .split("Version ")
    .nth(1)
    .and_then(|version| version.split('.').nth(2))
    .unwrap_or("unknown")
    .to_owned();
  let image = NANOSERVER_BY_BUILD
    .iter()
    .find(|(known, _)| *known == build)
    .map(|(_, image)| *image);
  if image.is_none() {
    eprintln!("SKIP: no Nano Server image is listed for this host's build {build} ({ver:?})");
  }
  image
}

/// `docker run --rm --isolation=process` of `image` with `source` bound read-only at `C:\data`, running `cmd /c`
/// `command` there; its exit code and outputs.
fn run_bound(image: &str, source: &str, command: &str) -> (i32, String, String) {
  let mount = format!("type=bind,source={source},target=C:\\data,readonly");
  bounded_output(
    Command::new("docker").args([
      "run",
      "--rm",
      "--isolation=process",
      "--mount",
      &mount,
      image,
      "cmd",
      "/c",
      command,
    ]),
    CONTAINER_WAIT,
  )
  .unwrap_or_else(|why| panic!("docker run of {image} with {source} bound: {why}"))
}

/// What Windows answers when a process-isolated container binds a folder of the WinFsp mount, as the DAX matrix's
/// Windows process-isolated container mount-route cell records it (`docs/wip/virtiofs.md` §8): refused. Decided by CI's
/// Windows runner, run 40257213499 (Windows Server 2025, Docker 29.7.2, `nanoserver:ltsc2025`, 2026-10-26): Docker's
/// engine answered "invalid mount config for type \"bind\": bind source path does not exist: E:\\share". The engine is a
/// service in a logon session of its own, and each logon session has its own drive letters (Microsoft Learn, "Services
/// and Redirected Drives"), so it never sees the drive an unprivileged mount defines. Past that, Microsoft's container
/// team says the bind filter cannot attach to a custom file system's volume ("Do not attach the filter to the volume
/// at this time", microsoft/Windows-Containers#335, closed by design).
const CONTAINER_BIND_REFUSED: bool = true;

/// §4.6 "Windows", the DAX matrix's Windows process-isolated container cell, G-mount step 4. Do: pull the Nano Server
/// image of this host's build; as a control, bind the test binary's own folder (NTFS) into a process-isolated container
/// and list it; then mount a volume read-only through WinFsp, its `share/file.bin` written through the SDK, and bind the
/// mount's `share` folder into the same kind of container, which types the file. Expect: the control lists its folder,
/// so this runner binds a folder into a process-isolated container; and the WinFsp bind does what
/// [`CONTAINER_BIND_REFUSED`] records, its answer printed either way.
#[test]
fn a_winfsp_folder_bound_into_a_process_isolated_container() {
  if !live_mount_runs() {
    return;
  }
  let Some(image) = process_isolated_image() else {
    return;
  };
  let (code, out, err) = bounded_output(
    Command::new("docker").args(["pull", "--quiet", image]),
    CONTAINER_WAIT,
  )
  .unwrap_or_else(|why| panic!("docker pull {image}: {why}"));
  assert_eq!(code, 0, "docker pull {image}: {out} {err}");

  let exe = std::env::current_exe().unwrap();
  let own_folder = exe.parent().unwrap().display().to_string();
  let own_name = exe.file_name().unwrap().to_string_lossy().into_owned();
  let (code, listed, err) = run_bound(image, &own_folder, "dir /b C:\\data");
  assert!(
    code == 0 && listed.contains(&own_name),
    "the control: a process-isolated container lists an NTFS folder bound into it (exit {code}): {listed} {err}"
  );

  let instance = instance("container");
  let _anchor = start_anchor(&instance);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "container", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let hex = value_of(&out, "id");
  let volume = volume_id_of(&hex);
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let mut writer = slates_client::Client::connect(&instance, deadlines).unwrap();
  let attachment = writer
    .attach(volume, None, slates_client::Intent::Write)
    .unwrap()
    .attachment;
  writer
    .fs_mkdir((volume, attachment), "share", 0o755)
    .unwrap();
  writer
    .fs_write(
      (volume, attachment),
      "share/file.bin",
      &[b'A'; FILE_BYTES],
      FILE_MODE,
    )
    .unwrap();
  let drive = free_drive_letter();
  let (code, _, err) = run(&instance, &["mount", &hex, &drive, "--read-only"]);
  assert_eq!(code, 0, "mount: {err}");
  let share = format!("{drive}\\share");
  wait_ready(&format!("{share}\\file.bin"));

  let (code, typed, err) = run_bound(image, &share, "type C:\\data\\file.bin");
  let bound = code == 0 && typed.contains(&"A".repeat(FILE_BYTES));
  eprintln!(
    "WINFSP CONTAINER BIND: {} (exit {code}; image {image}): {} {}",
    if bound {
      "bound, the file typed whole"
    } else {
      "refused"
    },
    typed.trim(),
    err.trim()
  );
  assert_eq!(
    !bound, CONTAINER_BIND_REFUSED,
    "the WinFsp folder's bind does what the matrix records; if it binds, step 4 owes the map through it"
  );
  let (code, _, err) = run(&instance, &["unmount", &drive]);
  assert_eq!(code, 0, "unmount: {err}");
}

/// Format: one shard, where a request the daemon sends its own drive waits on the shard that must answer it (A-227).
const ONE_SHARD: &str = "1";

/// Shape: how long a refusal of a base on the daemon's own drive may take to come back (A-227): the verb's own work and
/// one round trip, far under a client's stall deadline.
const OWN_MOUNT_ANSWER: Duration = Duration::from_secs(10);

/// Runs a client verb as [`run`] does, but gives up after `limit`: `None` (the client killed and reaped) when it had not
/// answered by then, for a verb that would wait on a daemon blocked on itself.
fn run_within(instance: &str, args: &[&str], limit: Duration) -> Option<(i32, String, String)> {
  let mut child = slates()
    .arg("--instance")
    .arg(instance)
    .args(args)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let started = Instant::now();
  while child.try_wait().unwrap().is_none() {
    if started.elapsed() > limit {
      child.kill().unwrap();
      child.wait().unwrap();
      return None;
    }
    pause();
  }
  let output = child.wait_with_output().unwrap();
  Some((
    output.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  ))
}

/// A-227, the base plane on Windows (§4.6: no daemon thread opens a path on a volume the daemon serves). Do: on a
/// one-shard daemon, mount a volume at a free drive, make a directory on it, and create a second volume with that
/// directory as its base. Expect: refused at once (exit 1, `OwnMount`), nothing created, and the daemon still answering.
/// Before A-227 the shard opened the base through its own drive and waited on the request only it could answer; a kill
/// then could not end it (A-190's amendment).
#[test]
fn a_base_on_the_daemons_own_drive_is_refused_before_any_open() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("ownbase");
  let _anchor = start_anchor_of(&instance, ONE_SHARD);
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "outer", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let id = value_of(&out, "id");
  let drive = free_drive_letter();
  let (code, _, err) = run(&instance, &["mount", &id, &drive]);
  assert_eq!(code, 0, "mount: {err}");
  let root = format!("{drive}\\");
  wait_ready(&root);
  let base = format!("{drive}\\project");
  let made = Command::new("cmd")
    .args(["/C", "mkdir", &base])
    .output()
    .unwrap();
  assert!(made.status.success(), "mkdir {base}");
  let created = [
    "volume",
    "create",
    "inner",
    "--bounded",
    "8MiB",
    "--base",
    &base,
  ];
  let Some((code, _, err)) = run_within(&instance, &created, OWN_MOUNT_ANSWER) else {
    panic!(
      "the daemon did not answer a base on its own drive within {OWN_MOUNT_ANSWER:?}: it opened the base through \
       its own drive and waits on itself"
    );
  };
  assert_eq!(
    code, 1,
    "a base on the daemon's own drive is refused: {err}"
  );
  assert!(
    err.contains("OwnMount"),
    "refused as the daemon's own mount: {err}"
  );
  let (code, out, err) = run(&instance, &["volume", "list"]);
  assert_eq!(code, 0, "the daemon still answers: {err}");
  assert!(!out.contains("inner"), "nothing was created: {out}");
  let (code, _, err) = run(&instance, &["unmount", &drive]);
  assert_eq!(code, 0, "unmount: {err}");
}

/// Runs `cmd /C` with `args`, asserting it succeeded: the Windows shell's own `mkdir`, `mklink` and `rmdir`, so the test
/// makes its host directories and junctions without a file write of its own.
fn cmd(args: &[&str]) {
  let made = Command::new("cmd").arg("/C").args(args).output().unwrap();
  assert!(
    made.status.success(),
    "cmd /C {}: {}{}",
    args.join(" "),
    String::from_utf8_lossy(&made.stdout),
    String::from_utf8_lossy(&made.stderr)
  );
}

/// A-227, recovery's reacquire on Windows (§4.6, §4.8): a base that a drive of the daemon's own came to cover is never
/// entered when a successor reacquires it. Do: on a one-shard daemon, create a volume over `link\project`, where `link`
/// is a junction to a directory holding `project`; mount a second volume at a drive, make `project` on it, and make
/// `link` a directory symbolic link to the drive's root, so the base is now the drive's; kill the daemon. Expect: the successor serves the
/// held drive again within the takeover bound (it learned the held volume as its own before any shard started, so its
/// shard's reacquire refused the base rather than open it through the drive nobody served yet), the covered volume is
/// not rebuilt, and the drive still lists `project`.
#[test]
fn a_base_a_held_drive_covers_is_not_reacquired_through_it() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("ownreacq");
  let _anchor = start_anchor_of(&instance, ONE_SHARD);
  let scratch = format!(
    "{}\\ownreacq-{}",
    env!("CARGO_TARGET_TMPDIR").replace('/', "\\"),
    std::process::id()
  );
  let real = format!("{scratch}\\real");
  let link = format!("{scratch}\\link");
  cmd(&["mkdir", &format!("{real}\\project")]);
  cmd(&["mklink", "/J", &link, &real]);
  let base = format!("{link}\\project");
  let (code, out, err) = run(
    &instance,
    &[
      "volume",
      "create",
      "covered",
      "--bounded",
      "8MiB",
      "--base",
      &base,
    ],
  );
  assert_eq!(code, 0, "{err}");
  let covered = value_of(&out, "id");
  let (code, out, err) = run(
    &instance,
    &["volume", "create", "cover", "--bounded", "64MiB"],
  );
  assert_eq!(code, 0, "{err}");
  let cover = value_of(&out, "id");
  let drive = free_drive_letter();
  let (code, _, err) = run(&instance, &["mount", &cover, &drive]);
  assert_eq!(code, 0, "mount: {err}");
  let root = format!("{drive}\\");
  wait_ready(&root);
  cmd(&["mkdir", &format!("{drive}\\project")]);
  // A junction names local volumes only, which a WinFsp drive is not to `mklink`; a directory symbolic link (a name
  // surrogate too, followed by the same walk) names it.
  cmd(&["rmdir", &link]);
  cmd(&["mklink", "/D", &link, &root]);
  let original = drive_target(&drive).unwrap();
  let killed = daemon_pid(&instance).unwrap();
  kill_daemon(killed);
  wait_for_successor(&instance, &drive, &original, killed, 1);
  let (code, out, err) = run(&instance, &["status", &covered]);
  assert_ne!(
    code, 0,
    "the covered volume is not rebuilt through the drive: {out}"
  );
  eprintln!("WINFSP OWN REACQUIRE: the covered volume's status: exit {code}: {err}");
  let listed = Command::new("cmd")
    .args(["/C", "dir", "/B", &root])
    .output()
    .unwrap();
  assert_eq!(
    String::from_utf8_lossy(&listed.stdout).trim(),
    "project",
    "the held drive still serves"
  );
  cmd(&["rmdir", &link]);
  // The drive's own attachment, detached by its id: `slates unmount` scans every volume's status for the drive and
  // stops at the first refusal, which the covered volume, not rebuilt, now is (GAPS).
  let (code, out, err) = run(&instance, &["status", &cover]);
  assert_eq!(code, 0, "status: {err}");
  let attachment = out
    .lines()
    .filter_map(|line| line.strip_prefix("mount: "))
    .find_map(|line| line.split_once(" (attachment "))
    .and_then(|(_, rest)| rest.strip_suffix(')'))
    .expect("the cover's status names its drive's attachment")
    .to_owned();
  let (code, _, err) = run(&instance, &["detach", &attachment]);
  assert_eq!(code, 0, "detach: {err}");
}

/// Changes the drive at `path`'s file in each of the ways a read-only file system refuses: an overwrite, a create
/// beside it, a directory, a deletion. The Win32 errors, in that order.
#[allow(clippy::disallowed_methods)]
fn changes_through_drive(path: &str) -> [Option<i32>; 4] {
  let beside = format!("{path}.new");
  let dir = format!("{path}.dir");
  [
    std::fs::write(path, b"no")
      .err()
      .and_then(|e| e.raw_os_error()),
    std::fs::write(&beside, b"no")
      .err()
      .and_then(|e| e.raw_os_error()),
    std::fs::create_dir(&dir)
      .err()
      .and_then(|e| e.raw_os_error()),
    std::fs::remove_file(path)
      .err()
      .and_then(|e| e.raw_os_error()),
  ]
}

/// GAPS (2026-11-08): a read-only attachment is a read-only file system on every transport. WinFsp reports the volume
/// read-only (`ReadOnlyVolume`) and a change that reaches the mount is answered `STATUS_MEDIA_WRITE_PROTECTED`, which
/// Win32 names `ERROR_WRITE_PROTECT`. Do: mount a volume read-only; through the drive overwrite `file.bin`, create a file
/// beside it, make a directory, delete `file.bin`. Expect each refused `ERROR_WRITE_PROTECT`, and `file.bin` reading its
/// first bytes still.
#[test]
fn a_read_only_drive_refuses_every_change_as_write_protected() {
  if !live_mount_runs() {
    return;
  }
  let fixture = ChangedElsewhere::new("rodrive", b'A');
  let write_protect = i32::try_from(windows_sys::Win32::Foundation::ERROR_WRITE_PROTECT).unwrap();
  assert_eq!(
    changes_through_drive(&fixture.file),
    [Some(write_protect); 4],
    "every change through the read-only drive is refused as write-protected"
  );
  assert_eq!(
    std::fs::read(&fixture.file).unwrap(),
    vec![b'A'; FILE_BYTES]
  );
}

/// Shape: the WinFsp drives the unmount test mounts beside the volume recovery cannot rebuild, so the scan meets the
/// refused volume whichever place the catalog's order gives it.
const HEALTHY_DRIVES: usize = 2;

/// Waits until a successor of `killed` has adopted `drives` held drives.
fn wait_for_adoptions(instance: &str, killed: u32, drives: u64) {
  let started = Instant::now();
  while !(daemon_pid(instance).is_some_and(|pid| pid != killed)
    && counter(instance, "winfsp.adopted") >= drives)
  {
    assert!(
      started.elapsed() < WAIT_FOR,
      "no successor adopted the {drives} held drives"
    );
    pause();
  }
}

/// Bug (GAPS, A-227's sweep, 2026-11-06): Windows `slates unmount` asked each volume's status in turn and stopped at the
/// first refusal, so one volume recovery could not rebuild kept every drive from being unmounted by its letter. Do:
/// create a volume over a host directory and mount two more at drives; remove the directory and kill the daemon, so its
/// successor serves both held drives and cannot rebuild the first volume. Expect: `slates unmount` of each drive
/// succeeds and the drive goes, and each names the volume it could not ask on standard error with its refusal's kind,
/// `NotRecovered`, the one its own status gives (naming the missing base).
#[test]
fn unmount_releases_every_healthy_drive_beside_a_volume_recovery_could_not_rebuild() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("unmountbad");
  let _anchor = start_anchor(&instance);
  let base = format!(
    "{}\\unmountbad-{}\\base",
    env!("CARGO_TARGET_TMPDIR").replace('/', "\\"),
    std::process::id()
  );
  cmd(&["mkdir", &base]);
  let (code, out, err) = run(
    &instance,
    &[
      "volume",
      "create",
      "unrebuilt",
      "--bounded",
      "8MiB",
      "--base",
      &base,
    ],
  );
  assert_eq!(code, 0, "{err}");
  let unrebuilt = value_of(&out, "id");
  let drives = mount_healthy_drives(&instance);
  cmd(&["rmdir", &base]);
  let killed = daemon_pid(&instance).unwrap();
  kill_daemon(killed);
  wait_for_adoptions(&instance, killed, u64::try_from(HEALTHY_DRIVES).unwrap());
  let (code, _, err) = run(&instance, &["status", &unrebuilt]);
  assert_ne!(
    code, 0,
    "recovery could not rebuild the volume whose base is gone"
  );
  assert!(
    err.contains("NotRecovered") && err.contains("BaseMissing"),
    "the listed volume's status names why recovery could not rebuild it, by kind: {err}"
  );
  let kind = "NotRecovered";
  for drive in &drives {
    unmount_naming_the_unasked(&instance, drive, kind);
  }
}

/// Creates [`HEALTHY_DRIVES`] volumes and mounts each at a free drive, ready, returning the drives.
fn mount_healthy_drives(instance: &str) -> Vec<String> {
  let mut drives = Vec::new();
  for index in 0..HEALTHY_DRIVES {
    let name = format!("healthy{index}");
    let (code, out, err) = run(instance, &["volume", "create", &name, "--bounded", "64MiB"]);
    assert_eq!(code, 0, "{err}");
    let drive = free_drive_letter();
    let (code, _, err) = run(instance, &["mount", &value_of(&out, "id"), &drive]);
    assert_eq!(code, 0, "mount: {err}");
    wait_ready(&format!("{drive}\\"));
    drives.push(drive);
  }
  drives
}

/// `slates unmount` of the healthy `drive` succeeds, the drive goes, and standard error names the volume the scan could
/// not ask (`unrebuilt`) by its refusal's `kind`.
fn unmount_naming_the_unasked(instance: &str, drive: &str, kind: &str) {
  let (code, _, err) = run(instance, &["unmount", drive]);
  assert_eq!(
    code, 0,
    "the healthy drive {drive} is unmounted beside the unrebuilt volume: {err}"
  );
  assert!(
    err.contains("volume unrebuilt") && err.contains(kind),
    "the unmount of {drive} names the volume it could not ask, by its refusal ({kind}): {err}"
  );
  assert!(drive_goes(drive), "the drive {drive} goes");
}

/// Makes `base` on the runner's disk with an ordinary file, a read-only one and a hidden one, each through the Windows
/// shell (`echo`, then `attrib`), so the test writes no file of its own.
fn make_attribute_base(base: &str) {
  cmd(&["mkdir", base]);
  for name in ["plain.txt", "locked.txt", "hidden.txt"] {
    cmd(&["echo", "from the disk", ">", &format!("{base}\\{name}")]);
  }
  cmd(&["attrib", "+R", &format!("{base}\\locked.txt")]);
  cmd(&["attrib", "+H", &format!("{base}\\hidden.txt")]);
}

/// Through the drive at `drive`, over [`make_attribute_base`]'s files: each one's attributes, the ordinary file's append
/// and delete, and the read-only file's refusals of both.
#[allow(clippy::disallowed_methods)] // file calls through the drive under test
fn assert_attributes_through(drive: &str) {
  use std::io::Write;
  use std::os::windows::fs::MetadataExt;
  /// Format: `FILE_ATTRIBUTE_READONLY` and `FILE_ATTRIBUTE_HIDDEN`.
  const READONLY: u32 = 0x1;
  const HIDDEN: u32 = 0x2;
  let path = |name: &str| format!("{drive}\\{name}");
  let attributes = |name: &str| std::fs::metadata(path(name)).unwrap().file_attributes();
  assert_eq!(
    attributes("plain.txt") & (READONLY | HIDDEN),
    0,
    "an ordinary file is neither read-only nor hidden"
  );
  assert_eq!(
    attributes("locked.txt") & READONLY,
    READONLY,
    "the read-only file reads read-only"
  );
  assert_eq!(
    attributes("hidden.txt") & HIDDEN,
    HIDDEN,
    "the hidden file reads hidden"
  );
  std::fs::OpenOptions::new()
    .append(true)
    .open(path("plain.txt"))
    .unwrap()
    .write_all(b"appended through the drive\n")
    .unwrap();
  std::fs::remove_file(path("plain.txt")).unwrap();
  let denied = Some(i32::try_from(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED).unwrap());
  let opened = std::fs::OpenOptions::new()
    .append(true)
    .open(path("locked.txt"));
  assert_eq!(
    opened.unwrap_err().raw_os_error(),
    denied,
    "the read-only file refuses a write"
  );
  assert_eq!(
    std::fs::remove_file(path("locked.txt"))
      .unwrap_err()
      .raw_os_error(),
    denied,
    "the read-only file refuses its delete"
  );
}

/// A-262, the Windows base plane. Do: in a real directory on the runner's disk make an ordinary file, a read-only one
/// (`attrib +R`) and a hidden one (`attrib +H`); create an overlay volume over the directory and mount it at a drive;
/// through the drive read each file's attributes, append to the ordinary file and delete it, then try to append to and
/// delete the read-only one. Expect: the ordinary file reports neither `READONLY` nor `HIDDEN`, takes the append and
/// deletes; the read-only one reports `READONLY` and refuses both (`ERROR_ACCESS_DENIED`); the hidden one reports
/// `HIDDEN`. Before, the Windows base reported each file's attribute word as its mode (`ARCHIVE`, `0x20`, as `0o040`),
/// so the drive took an ordinary file for one its owner may not write.
#[test]
fn an_overlay_of_a_windows_directory_keeps_its_files_read_only_state_and_attributes() {
  if !live_mount_runs() {
    return;
  }
  let instance = instance("attrbase");
  let _anchor = start_anchor_of(&instance, ONE_SHARD);
  let base = format!(
    "{}\\attrbase-{}",
    env!("CARGO_TARGET_TMPDIR").replace('/', "\\"),
    std::process::id()
  );
  make_attribute_base(&base);
  let (code, out, err) = run(
    &instance,
    &[
      "volume",
      "create",
      "over",
      "--bounded",
      "8MiB",
      "--base",
      &base,
    ],
  );
  assert_eq!(code, 0, "volume create --base: {err}");
  let id = value_of(&out, "id");
  let drive = free_drive_letter();
  let (code, _, err) = run(&instance, &["mount", &id, &drive]);
  assert_eq!(code, 0, "mount: {err}");
  wait_ready(&format!("{drive}\\"));
  assert_attributes_through(&drive);
  let (code, _, err) = run(&instance, &["unmount", &drive]);
  assert_eq!(code, 0, "unmount: {err}");
  cmd(&["attrib", "-R", &format!("{base}\\locked.txt")]);
  cmd(&["rmdir", "/S", "/Q", &base]);
}
