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
#[cfg(unix)]
const EXIT_STALE_TAKE_TOUCHED_OWN_PIPE: i32 = 15;
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
/// the handle list is what keeps it out). Returns its **identity** for the child — the number *and* what
/// proves the object at that number is this very decoy — and what keeps it open. The number alone was not
/// enough: numbers are process-local and reused, so a handle of the child's own at the same value read as
/// "the decoy came along" (Windows CI run 36500813478, exit 13 with nothing inherited).
#[cfg(unix)]
fn decoy() -> (String, (std::os::fd::OwnedFd, std::os::fd::OwnedFd)) {
  use std::os::fd::AsRawFd;
  let (read_end, write_end) = rustix::pipe::pipe().unwrap();
  rustix::io::fcntl_setfd(&read_end, rustix::io::FdFlags::CLOEXEC).unwrap();
  rustix::io::fcntl_setfd(&write_end, rustix::io::FdFlags::CLOEXEC).unwrap();
  let stat = rustix::fs::fstat(&read_end).unwrap();
  // The device and inode travel as their decimal text: their integer types differ across Unixes, and the
  // child compares its own fstat's fields formatted the same way.
  (
    format!("{}:{}:{}", read_end.as_raw_fd(), stat.st_dev, stat.st_ino),
    (read_end, write_end),
  )
}

/// Whether the decoy `identity` names is open in this process: a descriptor at its number that is the same
/// pipe (device and inode), not merely some descriptor that happens to have that number.
#[cfg(unix)]
fn decoy_is_here(identity: &str) -> bool {
  let (number, pipe) = identity.split_once(':').unwrap();
  let number: i32 = number.parse().unwrap();
  // SAFETY: a borrow for one fstat; a number that is not open fails EBADF and touches nothing.
  let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(number) };
  rustix::fs::fstat(borrowed).is_ok_and(|stat| format!("{}:{}", stat.st_dev, stat.st_ino) == pipe)
}

/// The Windows decoy: an inheritable handle to a uniquely **named** event, so the child can open the same
/// object by name and compare it with whatever sits at the decoy's handle value (`CompareObjectHandles`,
/// Windows 10 1607 / Server 2016 and later).
#[cfg(windows)]
fn decoy() -> (String, usize) {
  use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
  use windows_sys::Win32::System::Threading::CreateEventW;
  let attributes = SECURITY_ATTRIBUTES {
    nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap(),
    lpSecurityDescriptor: std::ptr::null_mut(),
    bInheritHandle: 1,
  };
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_nanos();
  let name = format!(
    "Local\\slates-delivery-decoy-{}-{nonce}",
    std::process::id()
  );
  let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: the attributes and the NUL-terminated name outlive the call; the attributes make the handle
  // inheritable, which is the point of the decoy (only the handle list keeps it from the child).
  let event = unsafe { CreateEventW(&attributes, 1, 0, wide.as_ptr()) };
  assert!(!event.is_null(), "CreateEventW");
  // The handle is leaked for the test's life; its value and its object's name are what matter.
  let value = event.expose_provenance();
  (format!("{value}:{name}"), value)
}

/// Whether the decoy `identity` names is open in this process: a handle at its value that refers to the
/// very event the parent created (opened here by name and compared), not merely some handle of the child's
/// own that happens to have that value.
#[cfg(windows)]
fn decoy_is_here(identity: &str) -> bool {
  use windows_sys::Win32::Foundation::{CloseHandle, CompareObjectHandles, GetHandleInformation};
  use windows_sys::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_SYNCHRONIZE};
  let (value, name) = identity.split_once(':').unwrap();
  let value: usize = value.parse().unwrap();
  let handle = std::ptr::with_exposed_provenance_mut(value);
  let mut flags = 0u32;
  // SAFETY: `GetHandleInformation` answers for any handle value, failing for one that is not open.
  if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
    return false;
  }
  let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: the NUL-terminated name outlives the call; a missing object returns null.
  let by_name = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide.as_ptr()) };
  if by_name.is_null() {
    return false;
  }
  // SAFETY: both handles are open in this process (checked above and just opened).
  let same = unsafe { CompareObjectHandles(handle, by_name) } != 0;
  // SAFETY: the handle this function opened, closed once.
  unsafe { CloseHandle(by_name) };
  same
}

/// The consumer child: takes the delivery, checks it is the one the parent made, that the decoy did
/// not come along, that a second look answers the same, that the delivery descriptor is closed after
/// the take, and (Unix) that the now stale name leaves a pipe of its own at that number alone. Exits
/// with the code of the first check that fails. Ignored so `cargo test` never runs it in-process; the
/// parent runs it with `--ignored --exact`.
#[test]
#[ignore = "the consumer child; run by a_consumer_child_takes_... with --ignored"]
fn delivery_consumer_child() {
  let Ok(role) = std::env::var(ROLE) else {
    return;
  };
  assert_eq!(role, "consumer");
  let delivery_name = std::env::var(ENV_CONSUMER_FD).unwrap();
  let verdict = the_take_is_the_parents_delivery()
    .and_then(|()| nothing_else_came_along())
    .and_then(|()| the_take_happened_once(&delivery_name));
  std::process::exit(verdict.err().unwrap_or(EXIT_OK));
}

/// The delivery is there and is the one the parent made: its consumer, and its capability by hash.
fn the_take_is_the_parents_delivery() -> Result<(), i32> {
  let expected_consumer: u64 = std::env::var(EXPECT_CONSUMER).unwrap().parse().unwrap();
  let expected_hash = std::env::var(EXPECT_HASH).unwrap();
  let taken = delivered().map_err(|e| {
    eprintln!("consumer child: {e}");
    EXIT_NO_DELIVERY
  })?;
  if taken.consumer != expected_consumer {
    return Err(EXIT_WRONG_CONSUMER);
  }
  if capability_hash(&taken.capability) != expected_hash {
    return Err(EXIT_WRONG_CAPABILITY);
  }
  Ok(())
}

/// The parent's decoy did not come along.
fn nothing_else_came_along() -> Result<(), i32> {
  let decoy = std::env::var(DECOY).unwrap();
  if decoy_is_here(&decoy) {
    eprintln!(
      "consumer child: the parent's decoy {decoy} is open here, the same object — inherited"
    );
    return Err(EXIT_DECOY_INHERITED);
  }
  Ok(())
}

/// Taken once: a second look is the same answer, the descriptor itself is closed, and (Unix) the
/// stale name leaves this process's own pipe at its number alone.
fn the_take_happened_once(delivery_name: &str) -> Result<(), i32> {
  let expected_consumer: u64 = std::env::var(EXPECT_CONSUMER).unwrap().parse().unwrap();
  assert!(matches!(delivered(), Ok(again) if again.consumer == expected_consumer));
  if take_named(delivery_name) != Err(DeliveryFault::NotInherited) {
    return Err(EXIT_NOT_CLOSED_AFTER_TAKE);
  }
  #[cfg(unix)]
  if !a_stale_take_leaves_this_processs_own_pipe_alone(delivery_name) {
    return Err(EXIT_STALE_TAKE_TOUCHED_OWN_PIPE);
  }
  Ok(())
}

/// Shape: the bytes a process's own pipe holds when a stale take looks at it — a length no delivery
/// record has, so a take that read them would say so.
const PROBE: &[u8] = b"probe";

/// After the take, the delivery's name is stale in this process, as it is in every process a consumer
/// starts (the variable is inherited, the descriptor is not). This process opens its own pipe at the
/// name's number — the number the take freed, which the kernel hands to the next open that asks for the
/// lowest free number at or above it — writes a probe into it, and asks for the delivery again. The
/// take must refuse and leave the pipe exactly as it was: open, the same pipe, its bytes unread, its
/// status flags unchanged. Checked through the raw number, so a take that closed it is seen rather than
/// closed a second time.
#[cfg(unix)]
fn a_stale_take_leaves_this_processs_own_pipe_alone(stale_name: &str) -> bool {
  use std::os::fd::AsRawFd;
  let number: i32 = stale_name.split(':').next().unwrap().parse().unwrap();
  let (read_end, write_end) = rustix::pipe::pipe().unwrap();
  let placed = rustix::io::fcntl_dupfd_cloexec(&read_end, number).unwrap();
  assert_eq!(
    placed.as_raw_fd(),
    number,
    "the number the take closed is the lowest free one at or above itself"
  );
  drop(read_end);
  assert_eq!(rustix::io::write(&write_end, PROBE).unwrap(), PROBE.len());
  let before = rustix::fs::fstat(&placed).unwrap();
  let status_before = rustix::fs::fcntl_getfl(&placed).unwrap();
  let refused = take_named(stale_name);
  // SAFETY: a borrow of this process's own number for the checks below; if the take closed it, the
  // calls fail with EBADF and touch nothing.
  let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(number) };
  let same_pipe = rustix::fs::fstat(borrowed)
    .is_ok_and(|now| now.st_dev == before.st_dev && now.st_ino == before.st_ino);
  let status_kept = rustix::fs::fcntl_getfl(borrowed).is_ok_and(|now| now == status_before);
  let bytes_kept = rustix::io::ioctl_fionread(borrowed)
    .is_ok_and(|available| usize::try_from(available).is_ok_and(|bytes| bytes == PROBE.len()));
  eprintln!(
    "consumer child: a stale take at {number} answered {refused:?}; the pipe there is the same: \
     {same_pipe}, its status flags kept: {status_kept}, its probe unread: {bytes_kept}"
  );
  let intact =
    refused == Err(DeliveryFault::NotInherited) && same_pipe && status_kept && bytes_kept;
  if !same_pipe {
    // The take closed the number: dropping `placed` would close it again (or close whatever reused
    // it), so its ownership is given up without a close.
    let _ = std::os::fd::IntoRawFd::into_raw_fd(placed);
  }
  intact
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
    "the consumer child took its delivery (10 none, 11 wrong consumer, 12 wrong capability, 13 the decoy came along, 14 not closed after the take, 15 a stale take touched the process's own pipe at the number)"
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

/// Windows: this process's own pipe holding the probe, a twin handle of its read end (the same
/// object), an unrelated event, and the pipe's write end — each a handle this test closes.
#[cfg(windows)]
struct OwnHandles {
  read: windows_sys::Win32::Foundation::HANDLE,
  twin: windows_sys::Win32::Foundation::HANDLE,
  unrelated: windows_sys::Win32::Foundation::HANDLE,
  write: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl OwnHandles {
  fn open() -> OwnHandles {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess};
    let (mut read, mut write): (HANDLE, HANDLE) = (null_mut(), null_mut());
    // SAFETY: two out-pointers to locals; no security attributes (not inheritable); the default size.
    assert_ne!(unsafe { CreatePipe(&mut read, &mut write, null(), 0) }, 0);
    let mut written = 0u32;
    let length = u32::try_from(PROBE.len()).unwrap();
    // SAFETY: the probe is live and of the length passed; the pipe's write end; a synchronous write.
    let ok = unsafe { WriteFile(write, PROBE.as_ptr(), length, &mut written, null_mut()) };
    assert_ne!(ok, 0);
    let mut twin: HANDLE = null_mut();
    // SAFETY: this process's pseudo-handle; a live handle; an out-pointer to a local.
    let ok = unsafe {
      DuplicateHandle(
        GetCurrentProcess(),
        read,
        GetCurrentProcess(),
        &mut twin,
        0,
        0,
        DUPLICATE_SAME_ACCESS,
      )
    };
    assert_ne!(ok, 0);
    // SAFETY: an unnamed, non-inheritable, auto-reset event.
    let unrelated = unsafe { CreateEventW(null(), 0, 0, null()) };
    assert!(!unrelated.is_null());
    OwnHandles {
      read,
      twin,
      unrelated,
      write,
    }
  }

  /// Each handle's flags, or `None` for one that is not open.
  fn flags(&self) -> [Option<u32>; 3] {
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE};
    let flags = |handle: HANDLE| {
      let mut flags = 0u32;
      // SAFETY: a handle value and an out-pointer to a local; fails for a handle that is not open.
      (unsafe { GetHandleInformation(handle, &mut flags) } != 0).then_some(flags)
    };
    [flags(self.read), flags(self.twin), flags(self.unrelated)]
  }

  /// The bytes waiting in the pipe, read without taking them.
  fn waiting(&self) -> usize {
    use std::ptr::null_mut;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available = 0u32;
    // SAFETY: no buffer (null, length zero); one out-pointer for the byte count; the pipe's read end.
    let ok = unsafe {
      PeekNamedPipe(
        self.read,
        null_mut(),
        0,
        null_mut(),
        &mut available,
        null_mut(),
      )
    };
    assert_ne!(ok, 0);
    usize::try_from(available).unwrap()
  }
}

#[cfg(windows)]
impl Drop for OwnHandles {
  fn drop(&mut self) {
    use windows_sys::Win32::Foundation::CloseHandle;
    for handle in [self.read, self.twin, self.write, self.unrelated] {
      // SAFETY: handles this struct created, each closed once, here.
      unsafe { CloseHandle(handle) };
    }
  }
}

/// Windows (§4.13; `docs/bugs/2026-09-28-a-stale-delivery-name-took-a-process-s-own-pipe.md`): a
/// delivery name whose two handle values hold this process's own pipe — the stale name a consumer's own
/// process inherits, its values since reused — is refused `NotInherited` by the pipe's name, and two
/// values holding different objects are refused before the pipe is queried at all. Every handle is
/// left as it was: open, its flags unchanged, the probe in the pipe unread.
#[cfg(windows)]
#[test]
fn a_stale_name_over_this_processs_own_handles_leaves_them_alone() {
  let delivery = Delivery::prepare(CONSUMER, &CAPABILITY).unwrap();
  let pipe_name = delivery
    .descriptor_name()
    .splitn(3, ':')
    .nth(2)
    .unwrap()
    .to_owned();
  let own = OwnHandles::open();
  let before = own.flags();
  let value = |handle: windows_sys::Win32::Foundation::HANDLE| handle.expose_provenance();
  let stale = format!("{}:{}:{pipe_name}", value(own.read), value(own.twin));
  assert_eq!(take_named(&stale), Err(DeliveryFault::NotInherited));
  let two_objects = format!("{}:{}:{pipe_name}", value(own.read), value(own.unrelated));
  assert_eq!(take_named(&two_objects), Err(DeliveryFault::NotInherited));
  assert_eq!(
    own.flags(),
    before,
    "every handle is open with its flags unchanged"
  );
  assert_eq!(own.waiting(), PROBE.len(), "the probe is unread");
  drop(own);
  drop(delivery);
}
