//! The harness delivery channel across real processes (§4.13 "a consumer channel is bound at
//! rendezvous using a capability delivered and retained outside other agents' reach"; GAP-A9-9 "the
//! harness delivery channel"; decided 2026-09-14 as the inherited descriptor): the test binary
//! re-invoked as the consumer child takes the capability from the one descriptor it inherited, finds
//! the parent's decoy descriptor — inheritable on Windows, close-on-exec on Unix — closed, so nothing
//! else came along, and finds its own delivery descriptor closed after the take; a sibling spawned
//! without a delivery finds none (`Absent`), and one told the number of a descriptor it did not
//! inherit is refused `NotInherited` — the non-vacuity contrast for the one that did inherit.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::{OsStr, OsString};
use std::process::Command;

use slates_ipc::delivery::{
  Capability, Delivery, DeliveryFault, ENV_CONSUMER_FD, Output, delivered, take_named,
};

/// Format: the environment variable selecting the child role, and the ones carrying what the consumer
/// child expects: its consumer id, the BLAKE3 hash of its capability (never the capability), and the
/// decoy descriptor's name.
const ROLE: &str = "SLATES_DELIVERY_TEST_ROLE";
const EXPECT_CONSUMER: &str = "SLATES_DELIVERY_TEST_CONSUMER";
const EXPECT_HASH: &str = "SLATES_DELIVERY_TEST_HASH";
const DECOY: &str = "SLATES_DELIVERY_TEST_DECOY";
/// Shape: the consumer child's exit codes, one per way it can fail, so a failure names its cause.
const EXIT_OK: i32 = 0;
const EXIT_NO_DELIVERY: i32 = 10;
const EXIT_WRONG_CONSUMER: i32 = 11;
const EXIT_WRONG_CAPABILITY: i32 = 12;
const EXIT_DECOY_INHERITED: i32 = 13;
const EXIT_NOT_CLOSED_AFTER_TAKE: i32 = 14;
/// Shape: the consumer the parent enrolls (an owner-tagged id shape) and its capability.
const CONSUMER: u64 = 0x0002_0000_0000_002a;
const CAPABILITY: Capability = [0x5a; 32];

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn capability_hash(capability: &Capability) -> String {
  hex(blake3::hash(capability).as_bytes())
}

/// The child's invocation: this binary running one ignored role function.
fn role_args(role: &str) -> Vec<OsString> {
  ["--ignored", "--exact", role, "--nocapture"]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn current_exe() -> OsString {
  std::env::current_exe().unwrap().into_os_string()
}

/// A descriptor the parent holds that the child must **not** inherit: a pipe end that is
/// close-on-exec, as every descriptor of the parent is (Unix), or an inheritable one (Windows, where
/// the handle list is what keeps it out). Returns its name for the child and what keeps it open.
#[cfg(unix)]
fn decoy() -> (String, (std::os::fd::OwnedFd, std::os::fd::OwnedFd)) {
  use std::os::fd::AsRawFd;
  let (read_end, write_end) = rustix::pipe::pipe().unwrap();
  rustix::io::fcntl_setfd(&read_end, rustix::io::FdFlags::CLOEXEC).unwrap();
  rustix::io::fcntl_setfd(&write_end, rustix::io::FdFlags::CLOEXEC).unwrap();
  (read_end.as_raw_fd().to_string(), (read_end, write_end))
}

/// Whether the descriptor `name` names is open in this process.
#[cfg(unix)]
fn is_open(name: &str) -> bool {
  let number: i32 = name.parse().unwrap();
  // SAFETY: a borrow for one fstat; a number that is not open fails EBADF and touches nothing.
  let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(number) };
  rustix::fs::fstat(borrowed).is_ok()
}

#[cfg(windows)]
fn decoy() -> (String, (usize, usize)) {
  use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
  use windows_sys::Win32::System::Pipes::CreatePipe;
  let attributes = SECURITY_ATTRIBUTES {
    nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap(),
    lpSecurityDescriptor: std::ptr::null_mut(),
    bInheritHandle: 1,
  };
  let mut read = std::ptr::null_mut();
  let mut write = std::ptr::null_mut();
  // SAFETY: two out-pointers to locals the call fills; the attributes make both ends inheritable,
  // which is the point of the decoy (only the handle list keeps them from the child).
  let ok = unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) };
  assert_ne!(ok, 0, "CreatePipe");
  // The handles are leaked for the test's life; their values are what matter.
  (
    read.expose_provenance().to_string(),
    (read.expose_provenance(), write.expose_provenance()),
  )
}

#[cfg(windows)]
fn is_open(name: &str) -> bool {
  use windows_sys::Win32::Foundation::{ERROR_INVALID_HANDLE, GetLastError};
  use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType};
  let number: usize = name.parse().unwrap();
  let handle = std::ptr::with_exposed_provenance_mut(number);
  // SAFETY: `GetFileType` answers for any handle value or fails with ERROR_INVALID_HANDLE.
  let kind = unsafe { GetFileType(handle) };
  // SAFETY: a thread-local read with no preconditions.
  !(kind == FILE_TYPE_UNKNOWN && unsafe { GetLastError() } == ERROR_INVALID_HANDLE)
}

/// The consumer child: takes the delivery, checks it is the one the parent made, that the decoy did
/// not come along, that a second look answers the same, and that the delivery descriptor is closed
/// after the take. Ignored so `cargo test` never runs it in-process; the parent runs it with
/// `--ignored --exact`.
#[test]
#[ignore = "the consumer child; run by a_consumer_child_takes_... with --ignored"]
fn delivery_consumer_child() {
  let Ok(role) = std::env::var(ROLE) else {
    return;
  };
  assert_eq!(role, "consumer");
  let expected_consumer: u64 = std::env::var(EXPECT_CONSUMER).unwrap().parse().unwrap();
  let expected_hash = std::env::var(EXPECT_HASH).unwrap();
  let delivery_name = std::env::var(ENV_CONSUMER_FD).unwrap();
  let taken = match delivered() {
    Ok(taken) => taken,
    Err(e) => {
      eprintln!("consumer child: {e}");
      std::process::exit(EXIT_NO_DELIVERY);
    }
  };
  if taken.consumer != expected_consumer {
    std::process::exit(EXIT_WRONG_CONSUMER);
  }
  if capability_hash(&taken.capability) != expected_hash {
    std::process::exit(EXIT_WRONG_CAPABILITY);
  }
  if is_open(&std::env::var(DECOY).unwrap()) {
    std::process::exit(EXIT_DECOY_INHERITED);
  }
  // Taken once: a second look is the same answer, and the descriptor itself is closed.
  assert!(matches!(delivered(), Ok(again) if again.consumer == expected_consumer));
  if take_named(&delivery_name) != Err(DeliveryFault::NotInherited) {
    std::process::exit(EXIT_NOT_CLOSED_AFTER_TAKE);
  }
  std::process::exit(EXIT_OK);
}

/// A child spawned without a delivery: prints the fault it finds, tagged.
#[test]
#[ignore = "the sibling child; run by the parent with --ignored"]
fn delivery_sibling_child() {
  let Ok(role) = std::env::var(ROLE) else {
    return;
  };
  assert!(role == "sibling" || role == "stale", "{role}");
  match delivered() {
    Ok(_) => println!("fault: none"),
    Err(slates_ipc::IpcError::CapabilityNotDelivered { fault }) => println!("fault: {fault:?}"),
    Err(e) => println!("fault: other {e}"),
  }
  std::process::exit(EXIT_OK);
}

/// AC (§4.13, GAP-A9-9): the consumer child inherits the one delivery descriptor and nothing else —
/// the parent's decoy, which its spawn would leak on Windows without the handle list and which is
/// close-on-exec on Unix, is closed in the child — takes the consumer id and the exact capability from
/// it (compared by hash, so the capability is in neither the child's arguments nor its environment),
/// and holds the delivery closed after the take. The child's exit code names any failure.
#[test]
fn a_consumer_child_takes_the_capability_from_the_one_inherited_descriptor_and_nothing_else() {
  let delivery = Delivery::prepare(CONSUMER, &CAPABILITY).unwrap();
  let (decoy_name, _keep_decoy_open) = decoy();
  let consumer_text = CONSUMER.to_string();
  let hash = capability_hash(&CAPABILITY);
  let environment: [(&OsStr, &OsStr); 4] = [
    (OsStr::new(ROLE), OsStr::new("consumer")),
    (OsStr::new(EXPECT_CONSUMER), OsStr::new(&consumer_text)),
    (OsStr::new(EXPECT_HASH), OsStr::new(&hash)),
    (OsStr::new(DECOY), OsStr::new(&decoy_name)),
  ];
  let mut child = delivery
    .spawn(
      &current_exe(),
      &role_args("delivery_consumer_child"),
      &environment,
      Output::Inherit,
    )
    .unwrap();
  assert!(child.id() > 0);
  let code = child.wait().unwrap();
  assert_eq!(
    code,
    Some(EXIT_OK),
    "the consumer child took its delivery (10 none, 11 wrong consumer, 12 wrong capability, 13 the decoy came along, 14 not closed after the take)"
  );
}

/// Runs a role child through a plain `Command` (no delivery) and returns the tagged fault line.
fn fault_of(role: &str, delivery_name: Option<&str>) -> String {
  let mut command = Command::new(current_exe());
  command
    .args(role_args("delivery_sibling_child"))
    .env(ROLE, role);
  if let Some(name) = delivery_name {
    command.env(ENV_CONSUMER_FD, name);
  }
  let output = command.output().unwrap();
  assert!(output.status.success(), "{output:?}");
  String::from_utf8_lossy(&output.stdout)
    .lines()
    .find_map(|line| line.strip_prefix("fault: ").map(str::to_owned))
    .unwrap_or_else(|| panic!("no fault line in {output:?}"))
}

/// The contrast: a sibling spawned without a delivery finds the variable absent (it is not a
/// consumer, and nothing else about it changes), and a process told the number of a delivery
/// descriptor it did not inherit — the parent's own read end, close-on-exec like every descriptor it
/// holds — is refused `NotInherited`, never bound and never handed the account's authority.
#[test]
fn a_sibling_without_a_delivery_is_absent_and_a_stale_number_is_not_inherited() {
  assert_eq!(fault_of("sibling", None), "Absent");
  let delivery = Delivery::prepare(CONSUMER, &CAPABILITY).unwrap();
  assert_eq!(
    fault_of("stale", Some(&delivery.descriptor_name())),
    "NotInherited"
  );
  drop(delivery);
}
