//! The Windows hold channel across real processes (A-198, Step 9.2 of the VFS and DAX plan): the anchor duplicates a
//! handle out of a child it spawned, holds it past the child's death, and hands it to a successor it spawns, which
//! inherits the same live kernel object — and, without the hold, the object is gone. The handle stands in for a WinFsp
//! mount's (jupiter and CI's Windows runners have no WinFsp for a unit test; the live mount is proved on CI in 9.3): a
//! manual-reset event, signalled, whose signalled state any handle of the same object reads, so a successor can tell
//! the held object from a fresh one.
//!
//! The test binary re-invokes itself in roles chosen by environment variables, spawned through the same
//! `slates_ipc::win_spawn` the anchor uses, so the handle list and the inheritance are exactly the daemon's.
#![cfg(windows)]
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::ffi::{OsStr, OsString};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use slates_anchor::handles::{HandleMessage, WHAT_WINFSP_MOUNT, parse_env};
use slates_anchor::held_windows::{Channel, Held, channel, handle_from_value, handle_value};
use slates_ipc::win_spawn::{
  Process, Spawn, StandardOutput, duplicate_inheritable, environment_block,
};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
  CreateEventW, OpenEventW, SetEvent, WaitForSingleObject,
};

/// Format: the variable naming the role the re-invoked binary plays, and the value for each role.
const ROLE: &str = "SLATES_HANDLE_TEST_ROLE";
/// Format: the sender role — create a signalled event and ask the anchor to hold it.
const ROLE_SENDER: &str = "sender_role";
/// Format: the checker role — the successor; verify it inherited the held event live.
const ROLE_CHECKER: &str = "checker_role";
/// Format: the gone-checker role — the red-check successor; verify the event is gone.
const ROLE_GONE: &str = "gone_role";
/// Format: the variable naming the hold channel's daemon end the sender inherited (its handle value, hex).
const CHANNEL_VALUE: &str = "SLATES_HANDLE_TEST_CHANNEL";
/// Format: the variable naming the event the sender creates and the checkers open, a `Local\` name.
const EVENT_NAME: &str = "SLATES_HANDLE_TEST_EVENT";
/// Format: the entry id the test holds the event under.
const ENTRY_ID: u64 = 1;
/// Shape: how long a receive or a child wait may take (milliseconds): generous for a loaded runner.
const WAIT_MS: u32 = 10_000;

/// This test binary's path, to re-invoke as a child.
fn own_exe() -> OsString {
  std::env::current_exe().unwrap().into_os_string()
}

/// The args that re-invoke one ignored test by name, printing its output.
fn role_args(test: &str) -> Vec<OsString> {
  ["--ignored", "--exact", test, "--nocapture"]
    .iter()
    .map(OsString::from)
    .collect()
}

/// Spawns this binary as `test`, with `added` environment and `inherited` handles (already inheritable), inheriting
/// this process's standard output so its lines show.
fn spawn_role(test: &str, added: &[(&OsStr, &OsStr)], inherited: &[HANDLE]) -> Process {
  let program = own_exe();
  let args = role_args(test);
  let block = environment_block(added);
  slates_ipc::win_spawn::spawn(&Spawn {
    program: &program,
    args: &args,
    environment: Some(&block),
    inherited,
    output: StandardOutput::Inherited,
    job: None,
  })
  .unwrap()
}

/// A nameless manual-reset event, owned.
fn event(name: Option<&str>) -> OwnedHandle {
  let wide: Option<Vec<u16>> = name.map(|n| n.encode_utf16().chain(std::iter::once(0)).collect());
  let name_ptr = wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
  // SAFETY: a manual-reset (TRUE), unsignalled (FALSE) event; `name_ptr` is null or a live NUL-terminated string for
  // the call; the handle is owned below.
  let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, name_ptr) };
  assert!(!raw.is_null(), "CreateEventW");
  // SAFETY: a handle this process owns, closed on drop.
  unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// Whether `handle` names a signalled event (a zero-timeout wait returns at once).
fn signalled(handle: HANDLE) -> bool {
  // SAFETY: a live handle; a zero timeout only samples the object's state.
  unsafe { WaitForSingleObject(handle, 0) == WAIT_OBJECT_0 }
}

/// A `Local\` event name unique to this test run.
fn unique_event_name() -> String {
  format!("Local\\slates-hold-test-{}", std::process::id())
}

/// A-198, Step 9.2. Do: spawn a sender that creates a signalled named event and asks the anchor to hold it; duplicate
/// it out of the sender and answer `Held`; let the sender exit; hand the hold to a spawned checker. Expect: the checker
/// inherits the event at the named value, finds it signalled (the same live object), and opens it by name — the hold
/// kept it alive across the sender's death.
#[test]
fn a_held_handle_outlives_its_sender_and_is_inherited_by_the_successor() {
  let name = unique_event_name();
  let (anchor, daemon) = channel().unwrap();
  // Hand the sender the channel's daemon end: an inheritable duplicate, named to it by value.
  let daemon_dup = duplicate_inheritable(daemon.raw()).unwrap();
  let channel_hex = format!("{:x}", handle_value(daemon_dup.raw()));
  let mut sender = spawn_role(
    ROLE_SENDER,
    &[
      (OsStr::new(ROLE), OsStr::new(ROLE_SENDER)),
      (OsStr::new(CHANNEL_VALUE), OsStr::new(&channel_hex)),
      (OsStr::new(EVENT_NAME), OsStr::new(&name)),
    ],
    &[daemon_dup.raw()],
  );
  // The sender's Hold arrives; duplicate its event out of the sender and answer that we hold it.
  let message = match anchor.recv(WAIT_MS).unwrap() {
    Some(message) => message,
    None => panic!(
      "no hold from the sender; its exit so far: {:?}",
      sender.try_wait()
    ),
  };
  let mut held = Held::new(8);
  let answer = held.apply(sender.raw_handle(), message);
  assert_eq!(answer, Some(HandleMessage::Held { id: ENTRY_ID }));
  anchor.send(&answer.unwrap()).unwrap();
  // The sender exits once it has its answer; the anchor's hold is now the only thing keeping the event.
  assert_eq!(
    sender.wait().unwrap(),
    Some(0),
    "the sender exits cleanly after Held"
  );
  drop(daemon_dup);
  // Hand the held event to a checker, which inherits it and finds it live. The anchor keeps its own channel; the
  // successor gets a duplicate of the daemon end, as a real successor would.
  let handoff = held.handoff(&daemon).unwrap();
  let (_channel, entries) = parse_env(handoff.env(), 8).unwrap();
  assert_eq!(entries.len(), 1);
  assert_eq!(entries[0].id, ENTRY_ID);
  let env = handoff.env().to_owned();
  let mut checker = spawn_role(
    ROLE_CHECKER,
    &[
      (OsStr::new(ROLE), OsStr::new(ROLE_CHECKER)),
      (
        OsStr::new(slates_anchor::handles::ENV_HANDLES),
        OsStr::new(&env),
      ),
      (OsStr::new(EVENT_NAME), OsStr::new(&name)),
    ],
    &handoff.handles(),
  );
  assert_eq!(
    checker.wait().unwrap(),
    Some(0),
    "the checker inherited the held event live and opened it by name"
  );
}

/// A-198, Step 9.2, the red-check. Do: the same, but the anchor does not hold the event; let the sender exit; spawn a
/// gone-checker that only knows the event's name. Expect: the checker cannot open the name — with no hold, the event
/// died with its sender. (With the hold, `a_held_handle_outlives_its_sender_...` shows it lives.)
#[test]
fn without_the_hold_the_object_dies_with_its_sender() {
  let name = unique_event_name();
  let name = format!("{name}-red");
  let (anchor, daemon) = channel().unwrap();
  let daemon_dup = duplicate_inheritable(daemon.raw()).unwrap();
  let channel_hex = format!("{:x}", handle_value(daemon_dup.raw()));
  let mut sender = spawn_role(
    ROLE_SENDER,
    &[
      (OsStr::new(ROLE), OsStr::new(ROLE_SENDER)),
      (OsStr::new(CHANNEL_VALUE), OsStr::new(&channel_hex)),
      (OsStr::new(EVENT_NAME), OsStr::new(&name)),
    ],
    &[daemon_dup.raw()],
  );
  // Receive the hold but do NOT duplicate it (no `Held::apply`); answer so the sender can exit.
  let _message = anchor.recv(WAIT_MS).unwrap().expect("the sender's hold");
  anchor.send(&HandleMessage::Held { id: ENTRY_ID }).unwrap();
  assert_eq!(sender.wait().unwrap(), Some(0));
  drop(daemon_dup);
  drop(daemon);
  // The gone-checker knows only the name; with nothing holding the event, its open must fail.
  let mut checker = spawn_role(
    ROLE_GONE,
    &[
      (OsStr::new(ROLE), OsStr::new(ROLE_GONE)),
      (OsStr::new(EVENT_NAME), OsStr::new(&name)),
    ],
    &[],
  );
  assert_eq!(
    checker.wait().unwrap(),
    Some(0),
    "the red-check: the event was gone once nothing held it"
  );
}

/// The sender role: create the signalled named event, send its handle value to the anchor as a hold, wait for the
/// answer, and exit. Ignored so `cargo test` never runs it in-process; the parent re-invokes it.
#[test]
#[ignore = "the hold channel sender; run by the parent with --ignored"]
fn sender_role() {
  if std::env::var(ROLE).as_deref() != Ok(ROLE_SENDER) {
    return;
  }
  let name = std::env::var(EVENT_NAME).unwrap();
  let channel_value: u64 = u64::from_str_radix(&std::env::var(CHANNEL_VALUE).unwrap(), 16).unwrap();
  eprintln!("SENDER: role ok, channel value {channel_value:x}, event {name}");
  // SAFETY: the inherited channel end at the named value is this process's own handle now; owned and closed on drop.
  let daemon = Channel::adopt(unsafe {
    OwnedHandle::from_raw_handle(handle_from_value(channel_value).unwrap())
  });
  let event = event(Some(&name));
  // SAFETY: a live event handle this process owns.
  assert_ne!(unsafe { SetEvent(event.as_raw_handle()) }, 0, "SetEvent");
  eprintln!(
    "SENDER: sending hold for event value {:x}",
    handle_value(event.as_raw_handle())
  );
  daemon
    .send(&HandleMessage::Hold {
      id: ENTRY_ID,
      what: WHAT_WINFSP_MOUNT,
      values: vec![handle_value(event.as_raw_handle())],
      record: name.into_bytes(),
    })
    .expect("send the hold");
  eprintln!("SENDER: sent; waiting for the answer");
  // Wait for the anchor's answer before exiting: only then has it duplicated the event out of this process.
  let answer = daemon.recv(WAIT_MS).unwrap();
  assert_eq!(answer, Some(HandleMessage::Held { id: ENTRY_ID }));
  eprintln!("SENDER: got Held; exiting 0");
  std::process::exit(0);
}

/// The checker role (the successor): verify it inherited the held event at the value `ENV_HANDLES` names, that the
/// event is signalled (so it is the same live object the sender made), and that it opens by name. Exit 0 on success.
#[test]
#[ignore = "the hold channel checker; run by the parent with --ignored"]
fn checker_role() {
  if std::env::var(ROLE).as_deref() != Ok(ROLE_CHECKER) {
    return;
  }
  let env = std::env::var(slates_anchor::handles::ENV_HANDLES).unwrap();
  let name = std::env::var(EVENT_NAME).unwrap();
  let (_channel, entries) = parse_env(&env, 8).unwrap();
  let entry = entries
    .iter()
    .find(|e| e.id == ENTRY_ID)
    .expect("the entry");
  let value = *entry.values.first().expect("a handle value");
  let inherited = handle_from_value(value).expect("a handle");
  assert!(signalled(inherited), "the inherited event is signalled");
  let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: a NUL-terminated name for the call; EVENT_ALL_ACCESS (0x1F0003) is fine for a wait.
  let opened = unsafe { OpenEventW(0x001F_0003, 0, wide.as_ptr()) };
  assert!(!opened.is_null(), "the event opens by name");
  assert!(signalled(opened), "the named event is signalled");
  // SAFETY: a handle this process opened, closed once.
  unsafe { windows_sys::Win32::Foundation::CloseHandle(opened) };
  std::process::exit(0);
}

/// The gone-checker role (the red-check successor): with nothing holding the event, opening it by name must fail. Exit
/// 0 when it cannot open (the object is gone), 1 when it can (the hold was not needed — the red-check did not hold).
#[test]
#[ignore = "the hold channel gone-checker; run by the parent with --ignored"]
fn gone_role() {
  if std::env::var(ROLE).as_deref() != Ok(ROLE_GONE) {
    return;
  }
  let name = std::env::var(EVENT_NAME).unwrap();
  let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: a NUL-terminated name for the call; a failed open returns null.
  let opened = unsafe { OpenEventW(0x001F_0003, 0, wide.as_ptr()) };
  if opened.is_null() {
    std::process::exit(0);
  }
  // SAFETY: a handle this process opened, closed once.
  unsafe { windows_sys::Win32::Foundation::CloseHandle(opened) };
  std::process::exit(1);
}
