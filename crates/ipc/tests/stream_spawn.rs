//! `slates_ipc::stream::spawn_with` by use (§4.6, A-179): on Windows a process hands a program a Shards stream's end by
//! starting it, the end's value named where `{handle}` stands in its arguments, since no process replaces itself there
//! as `exec_with` does on Unix. This test binary is re-invoked as the program. It learns its role and the value from its
//! arguments, which libtest takes as extra name filters that match no test, so no environment of this process changes.
//! A sibling started afterwards through the standard library, which inherits every inheritable handle of this process,
//! is the bystander that must find nothing of the end.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#![cfg(windows)]

use std::os::windows::io::{AsHandle, BorrowedHandle, OwnedHandle};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use slates_ipc::IpcError;
use slates_ipc::stream::{name_of, pair, read_waiting, spawn_with, write_waiting};

/// Format: this test's name, which the re-invoked binary runs.
const TEST: &str =
  "a_spawned_program_and_its_launcher_alone_hold_the_end_and_the_launcher_takes_it_along";
/// Format: where the program's arguments name its end (the CLI's `{handle}`).
const HANDLE_PLACEHOLDER: &str = "{handle}";
/// Format: the argument naming the program's role: write, then exit.
const WRITER: &str = "role=writer";
/// Format: the argument naming the program's role: write, then wait to be ended.
const LINGERER: &str = "role=lingerer";
/// Format: the variables that make this binary the bystander, and tell it the handle value and the pipe's name.
const BYSTANDER_VALUE: &str = "SLATES_IPC_TEST_BYSTANDER_VALUE";
/// Format: see [`BYSTANDER_VALUE`].
const BYSTANDER_PIPE: &str = "SLATES_IPC_TEST_BYSTANDER_PIPE";
/// Format: what the program writes through its end.
const HANDED: &[u8] = b"handed";
/// Shape: the writer's exit code, not zero, so the launcher is seen to report the program's own.
const WRITER_EXIT: i32 = 7;
/// Shape: the bystander's exit code when it found the end: a handle at the value, naming the pipe.
const EXIT_END_INHERITED: i32 = 13;
/// Shape: each direction's buffer, more than any message here.
const FRAME_BYTES: usize = 64;
/// Shape: how long the kept end waits for a byte or its end of stream before the test calls a copy leaked.
const WAIT: Duration = Duration::from_secs(10);

/// The program: adopts the end at the value it was given (its last argument), as Shards' spawner adopts
/// `--slates-fs-handle V`, and writes [`HANDED`] through it. The writer then exits [`WRITER_EXIT`]; the lingerer waits
/// to be ended.
fn program(lingers: bool) -> ! {
  let value: u64 = std::env::args().next_back().unwrap().parse().unwrap();
  let end = slates_ipc::handle_route::adopt_stream(value).unwrap();
  assert_eq!(
    write_waiting(end.as_handle(), HANDED).unwrap(),
    HANDED.len()
  );
  if lingers {
    loop {
      std::thread::park();
    }
  }
  drop(end);
  std::process::exit(WRITER_EXIT);
}

/// The bystander: exits [`EXIT_END_INHERITED`] if a handle at the value it was told names the end's pipe, else 0.
fn bystander(value: &str) -> ! {
  let value: usize = value.parse().unwrap();
  let pipe = std::env::var(BYSTANDER_PIPE).unwrap();
  let raw = std::ptr::with_exposed_provenance_mut::<core::ffi::c_void>(value);
  let mut flags = 0u32;
  // SAFETY: `GetHandleInformation` answers for any value, refusing one that names no open handle.
  let open = unsafe { windows_sys::Win32::Foundation::GetHandleInformation(raw, &mut flags) } != 0;
  // SAFETY: the value was just found open in this process, and nothing closes it while the borrow lasts.
  let named =
    open && name_of(unsafe { BorrowedHandle::borrow_raw(raw) }).is_ok_and(|name| name == pipe);
  std::process::exit(if named { EXIT_END_INHERITED } else { 0 });
}

/// What the kept end reads next, on a thread of its own: the bytes (empty at end of stream), or `None` when [`WAIT`]
/// passed first.
fn next_read(kept: &OwnedHandle) -> Option<Vec<u8>> {
  let kept = kept.try_clone().unwrap();
  let (sender, receiver) = mpsc::channel();
  std::thread::spawn(move || {
    let mut buffer = [0u8; FRAME_BYTES];
    let read = read_waiting(kept.as_handle(), &mut buffer).unwrap();
    let _ = sender.send(buffer[..read].to_vec());
  });
  receiver.recv_timeout(WAIT).ok()
}

/// The program's arguments: this test, its role, and the placeholder.
fn arguments(role: &str) -> Vec<String> {
  ["--exact", TEST, "--nocapture", role, HANDLE_PLACEHOLDER]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Do: (1) hand a pair's end to a program that writes through it and exits 7; (2) hand an end to a program that writes
/// and then waits, and while it holds the end start a bystander through the standard library, told the value the
/// program was handed and the pipe's name; then drop the launcher's hold on the program; (3) hand an end with no
/// placeholder in the arguments. Expect (1) the bytes on the kept end; the program's own exit code; the end still open
/// once the program has exited, as the launcher keeps its copy while it holds the child (the daemon's claim check needs
/// a live holder until the VM's first ask); then end of stream once the launcher lets the child go, so no other copy
/// exists; (2) the bytes; the bystander finding no handle at that value naming the pipe,
/// so a child started after the hand-off inherits nothing of it; then end of stream within the wait, as the job ends the
/// program with its launcher's hold, as exec's single process does on Unix; (3) refused `Layout`, nothing started, and
/// the end closed.
#[test]
fn a_spawned_program_and_its_launcher_alone_hold_the_end_and_the_launcher_takes_it_along() {
  if let Ok(value) = std::env::var(BYSTANDER_VALUE) {
    bystander(&value);
  }
  let args: Vec<String> = std::env::args().collect();
  if args.iter().any(|argument| argument == WRITER) {
    program(false);
  }
  if args.iter().any(|argument| argument == LINGERER) {
    program(true);
  }
  let exe = std::env::current_exe().unwrap();
  the_end_lasts_until_the_launcher_lets_its_program_go(&exe);
  a_later_child_inherits_nothing_and_the_launcher_takes_the_program_along(&exe);
  no_placeholder_is_refused_and_the_end_closed(&exe);
}

/// Case (1).
fn the_end_lasts_until_the_launcher_lets_its_program_go(exe: &std::path::Path) {
  let (kept, handed) = pair(FRAME_BYTES).unwrap();
  let mut writer = spawn_with(
    exe.as_os_str(),
    &arguments(WRITER),
    handed,
    HANDLE_PLACEHOLDER,
  )
  .unwrap();
  assert_eq!(
    next_read(&kept).as_deref(),
    Some(HANDED),
    "the program wrote through the end it was handed"
  );
  assert_eq!(
    writer.wait().unwrap(),
    WRITER_EXIT,
    "the program's own exit code"
  );
  assert!(
    peer_holds(kept.as_handle()),
    "the launcher's copy keeps the end open after its program exited"
  );
  drop(writer);
  assert_eq!(
    next_read(&kept).as_deref(),
    Some(&[][..]),
    "the end closed once the launcher let its program go: no other copy exists"
  );
}

/// Whether some process still holds the other end of `kept`'s pipe: `PeekNamedPipe` answers at once, refusing with
/// `ERROR_BROKEN_PIPE` once every holder of the other end has closed it.
fn peer_holds(kept: BorrowedHandle<'_>) -> bool {
  use std::os::windows::io::AsRawHandle;
  let mut available = 0u32;
  // SAFETY: a live handle; no buffer is asked for (null, zero); the out-pointer is a local.
  let ok = unsafe {
    windows_sys::Win32::System::Pipes::PeekNamedPipe(
      kept.as_raw_handle(),
      std::ptr::null_mut(),
      0,
      std::ptr::null_mut(),
      &mut available,
      std::ptr::null_mut(),
    )
  };
  ok != 0
}

/// Case (2).
fn a_later_child_inherits_nothing_and_the_launcher_takes_the_program_along(exe: &std::path::Path) {
  let (kept, handed) = pair(FRAME_BYTES).unwrap();
  let pipe = name_of(kept.as_handle()).unwrap();
  let lingerer = spawn_with(
    exe.as_os_str(),
    &arguments(LINGERER),
    handed,
    HANDLE_PLACEHOLDER,
  )
  .unwrap();
  assert_eq!(
    next_read(&kept).as_deref(),
    Some(HANDED),
    "the lingerer wrote through its end"
  );
  assert_eq!(
    bystander_status(exe, lingerer.handed(), &pipe),
    Some(0),
    "a child started after the hand-off holds no handle naming {pipe} at {}",
    lingerer.handed()
  );
  let contrast = inheritable_copy(kept.as_handle());
  assert_eq!(
    bystander_status(exe, contrast.0, &pipe),
    Some(EXIT_END_INHERITED),
    "the non-vacuity contrast: an inheritable handle to the pipe is found by the same bystander"
  );
  drop(contrast);
  drop(lingerer);
  assert_eq!(
    next_read(&kept).as_deref(),
    Some(&[][..]),
    "dropping the launcher's hold ended the program, and its end with it"
  );
}

/// Case (3).
fn no_placeholder_is_refused_and_the_end_closed(exe: &std::path::Path) {
  let (kept, handed) = pair(FRAME_BYTES).unwrap();
  let refused = spawn_with(
    exe.as_os_str(),
    &["--exact".to_owned(), TEST.to_owned()],
    handed,
    HANDLE_PLACEHOLDER,
  );
  assert!(
    matches!(refused, Err(IpcError::Layout { .. })),
    "{refused:?}"
  );
  assert_eq!(
    next_read(&kept).as_deref(),
    Some(&[][..]),
    "the refused end was closed"
  );
}

/// The bystander's exit code, told `value` and `pipe`.
fn bystander_status(exe: &std::path::Path, value: u64, pipe: &str) -> Option<i32> {
  Command::new(exe)
    .args(["--exact", TEST, "--nocapture"])
    .env(BYSTANDER_VALUE, value.to_string())
    .env(BYSTANDER_PIPE, pipe)
    .status()
    .unwrap()
    .code()
}

/// An inheritable duplicate of `end`, its value and the handle closed on drop: what a standard-library child inherits.
struct InheritableCopy(u64, #[allow(dead_code)] OwnedHandle);

fn inheritable_copy(end: BorrowedHandle<'_>) -> InheritableCopy {
  use std::os::windows::io::{AsRawHandle, FromRawHandle};
  use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
  use windows_sys::Win32::System::Threading::GetCurrentProcess;
  let mut copy = std::ptr::null_mut();
  // SAFETY: the pseudo-handle of this process needs no closing; `end` is live; the out-pointer is a local.
  let ok = unsafe {
    DuplicateHandle(
      GetCurrentProcess(),
      end.as_raw_handle(),
      GetCurrentProcess(),
      &mut copy,
      0,
      1,
      DUPLICATE_SAME_ACCESS,
    )
  };
  assert_ne!(ok, 0, "DuplicateHandle");
  let value = u64::try_from(copy.expose_provenance()).unwrap();
  // SAFETY: the duplicate just made, owned here alone.
  InheritableCopy(value, unsafe { OwnedHandle::from_raw_handle(copy) })
}
