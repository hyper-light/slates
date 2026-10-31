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

/// The anchor process, killed and reaped on drop, so a failed assertion leaves no daemon behind (the daemon leaves
/// when its anchor dies).
struct AnchorProcess {
  child: Child,
  drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AnchorProcess {
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
fn start_anchor(instance: &str) -> AnchorProcess {
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
  let anchor = AnchorProcess {
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
  while std::fs::metadata(&root).is_ok() {
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
  _anchor: AnchorProcess,
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
/// both stayed stale for 10 s on CI's runner (runs 40249524066 and 40250856246).
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
  let (purges, echoes, refused, unavailable) = (
    counter(&fixture.instance, PURGES_DONE),
    counter(&fixture.instance, PURGE_ECHOES),
    counter(&fixture.instance, PURGES_REFUSED),
    counter(&fixture.instance, PURGES_UNAVAILABLE),
  );
  eprintln!(
    "WINFSP PURGES: {purges} made, {echoes} writes answered without being applied, {refused} refused, {unavailable} \
     unavailable"
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
