//! A live WinFsp mount over a real slates volume (§4.6, R5), the Windows analogue of the FSKit
//! handler's live mount and the NFS `mount_nfs` test: provision a scratch volume, mount it at a free
//! drive letter through the real WinFsp kernel FSD, then create/write/read/list/delete a file *through
//! the Windows filesystem* and unmount. It drives the whole `FSP_FILE_SYSTEM_INTERFACE` — the vtable,
//! the callback trampolines, the owner-thread volume, and the mount lifecycle — end to end.
//!
//! Windows-only, and gated behind `WINFSP_TEST_MOUNT=1` (like the CLI mount test's `SLATES_TEST_CLI`):
//! it needs WinFsp installed and the runner to itself, so the default run skips loudly. The Windows CI
//! job installs WinFsp and sets the variable.
#![cfg(windows)]
// Test harness code: an unwrap/expect here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_bridge_core::Rights;
use slates_bridge_winfsp::host::{VolumeHost, WinFspError, mount};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

/// Shape: the page and arena size of the scratch volume the mount serves.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;

/// Builds the scratch volume the mount serves — run on the mount's owner thread, so the `!Send` volume
/// is born there and never crosses a thread. A bounded 1 GiB RAM volume under a full read-write
/// attachment for the mounting user.
fn build_host() -> Result<VolumeHost, WinFspError> {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(
      Region::map(PAGE * REGION_PAGES, PAGE, false).map_err(|_| WinFspError::OwnerThread)?,
    )
    .map_err(|_| WinFspError::OwnerThread)?;
  let mut store = Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  );
  let volume_id = VolumeId { bytes: [7; 16] };
  let volume = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .map_err(|_| WinFspError::OwnerThread)?;
  VolumeHost::new(
    volume_id,
    volume,
    store,
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
}

/// The first free drive letter from `E:` to `Z:` (`GetLogicalDrives` bit clear), or `None` if the host
/// has none free (a machine with 22 drives, which no CI runner is).
fn free_drive_letter() -> Option<char> {
  // SAFETY: `GetLogicalDrives` takes no arguments and returns a bitmask (bit 0 = A:, 1 = B:, …).
  let used = unsafe { GetLogicalDrives() };
  ('E'..='Z').find(|letter| {
    let bit = u32::from(*letter) - u32::from('A');
    used & (1 << bit) == 0
  })
}

/// Waits until `path` is reachable (the mount registers asynchronously after `StartDispatcher`), up to
/// a short budget. `std::fs::metadata` is a read, outside the R1 write wall.
fn wait_ready(path: &str) {
  let started = Instant::now();
  while started.elapsed() < Duration::from_secs(10) {
    if std::fs::metadata(path).is_ok() {
      return;
    }
    // The test harness paces its polls; shipped code parks on its driver (D-9).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(50));
  }
}

/// Runs a shell command and asserts it succeeded — the R1-clean way to drive the mount's *writes*
/// (create/delete), the way the CLI mount test drives `printf`/`cat` rather than `std::fs`. Reads
/// (`std::fs::read`, `read_dir`, `metadata`) are outside the write wall and used directly.
fn run(program: &str, args: &[&str]) {
  let status = std::process::Command::new(program)
    .args(args)
    .status()
    .unwrap_or_else(|e| panic!("run {program} {args:?}: {e}"));
  assert!(status.success(), "{program} {args:?} failed: {status}");
}

/// AC (§4.6): a Windows program sees a slates volume as a normal drive — a file created, written, read
/// back byte-for-byte, listed, and deleted through the Windows filesystem, then the mount removed.
#[test]
fn a_real_winfsp_mount_serves_a_volume_through_the_windows_filesystem() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    println!(
      "skipping the live WinFsp mount: set WINFSP_TEST_MOUNT=1 to run it (it needs WinFsp installed \
       and the machine to itself)"
    );
    return;
  }
  let Some(letter) = free_drive_letter() else {
    panic!("no free drive letter to mount at");
  };
  let mount_point = format!("{letter}:");
  let root = format!("{letter}:\\");

  let mounted = mount(build_host, &mount_point).expect("mount a slates volume at a drive letter");
  wait_ready(&root);

  // Create and write a file through the Windows filesystem (PowerShell's `WriteAllText`, ASCII, so the
  // bytes on the volume are exactly the message). A write goes through a subprocess (R1), not std::fs.
  let message = "hello from a live slates WinFsp mount";
  let file = format!("{root}hello.txt");
  run(
    "powershell",
    &[
      "-NoProfile",
      "-Command",
      &format!("[IO.File]::WriteAllText('{file}', '{message}')"),
    ],
  );

  // Read it back byte-for-byte (a read, outside the write wall).
  let got = std::fs::read(&file).expect("read the file back through the mount");
  assert_eq!(
    got,
    message.as_bytes(),
    "the bytes travel host -> WinFsp -> the slates volume and back"
  );

  // Make a directory and confirm both entries list.
  let dir = format!("{root}sub");
  run("cmd", &["/c", "mkdir", &dir]);
  let names: Vec<String> = std::fs::read_dir(&root)
    .expect("list the mount root")
    .filter_map(|entry| entry.ok())
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

  // Delete the file and the directory (subprocess writes).
  run("cmd", &["/c", "del", "/q", &file]);
  run("cmd", &["/c", "rmdir", &dir]);
  assert!(
    std::fs::metadata(&file).is_err(),
    "the file is gone after delete"
  );

  // Unmount by dropping the mount (stops the dispatcher, removes the mount point, joins the owner).
  drop(mounted);
}
