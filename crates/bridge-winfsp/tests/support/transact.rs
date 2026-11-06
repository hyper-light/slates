//! What one WinFsp control transaction costs against its output buffer (audit step 7, after A-215). A real WinFsp
//! volume is mounted with no file system behind it: this test's own loop answers every request at once (a Create "not
//! found", anything else "invalid request") and sends each response in the call that receives the next request, as the
//! DLL's dispatcher does. So a call's time is the driver's, the I/O manager's and one client's turnaround alone. The
//! modes differ only in the control code, the output buffer's size and its memory. The direct-output batch code makes
//! the I/O manager probe and lock the whole output buffer for each call (`METHOD_OUT_DIRECT`), and the driver refuses one
//! under 64 KiB (WinFsp v2.1 `src/sys/volume.c:1028-1032`, `inc/winfsp/fsctl.h:142`). The DLL's single-request code is
//! buffered (`fsctl.h:100`). Modes rotate each round, so a drifting load lands on every mode alike. Run by name, in
//! release, with `WINFSP_TEST_MOUNT=1`; it prints each mode's call and client latencies.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation
)]

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use windows_sys::Win32::Foundation::{
  CloseHandle, ERROR_IO_PENDING, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, OPEN_EXISTING};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Memory::{
  CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
  MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc,
  VirtualFree,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForMultipleObjects};

/// Format: CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 0x800 + 't', METHOD_OUT_DIRECT, FILE_ANY_ACCESS), fsctl.h:104.
const TRANSACT_BATCH: u32 = (9 << 16) | ((0x800 + b't' as u32) << 2) | 2;
/// Format: CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 0x800 + 'T', METHOD_BUFFERED, FILE_ANY_ACCESS), fsctl.h:100: the DLL's.
const TRANSACT_SINGLE: u32 = (9 << 16) | ((0x800 + b'T' as u32) << 2);
/// Format: FSP_FSCTL_TRANSACT_BUFFER_SIZEMIN, the single-request output the DLL passes (fsctl.h:143).
const SINGLE_BYTES: usize = 16 * 1024 - 64;
/// Format: FSP_FSCTL_TRANSACT_BATCH_BUFFER_SIZEMIN (fsctl.h:142).
const BATCH_MIN: usize = 64 * 1024;
/// Shape: the largest output measured, four times the minimum, for the cost each locked page adds.
const LARGEST: usize = BATCH_MIN * 4;
/// Format: sizeof(FSP_FSCTL_TRANSACT_RSP), the fixed response.
const RESPONSE: usize = 128;
/// Format: STATUS_OBJECT_NAME_NOT_FOUND and STATUS_INVALID_DEVICE_REQUEST.
const NOT_FOUND: u32 = 0xC000_0034;
/// Format: see NOT_FOUND.
const INVALID: u32 = 0xC000_0010;
/// Format: FspFsctlTransactCreateKind.
const CREATE: u32 = 1;
/// Shape: rounds, and requests each mode serves a round, as mount_bench's 10 rounds of 1,000 files.
const ROUNDS: usize = 10;
/// Shape: see ROUNDS.
const PER_ROUND: usize = 2000;

/// One way to make the call: its code, its output's length, and whether the output is a pagefile section's view (as
/// the anchor-held one is) or private memory.
struct Mode {
  name: &'static str,
  code: u32,
  bytes: usize,
  section: bool,
  wait: Wait,
}

/// How a call that pends is waited for (DIAG, diag-winfsp-overlap-39).
#[derive(Clone, Copy)]
enum Wait {
  /// The OVERLAPPED's own event, through `GetOverlappedResult` (this harness's form until now).
  Event,
  /// `WaitForMultipleObjects` over the completion event and a second, idle one, then `GetOverlappedResult`: the
  /// receiver's form (`crate::purge_intents::Intents::wait`, the second event its purge intents').
  Multiple,
  /// No OVERLAPPED at all, so the call itself waits on the volume handle: WinFsp's own `FspFsctlTransact`
  /// (`src/dll/fsctl.c`), the DLL dispatcher's form.
  Handle,
}

const MODES: [Mode; 8] = [
  Mode {
    name: "single-buffered-16320",
    code: TRANSACT_SINGLE,
    bytes: SINGLE_BYTES,
    section: false,
    wait: Wait::Event,
  },
  Mode {
    name: "batch-direct-64k-private",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN,
    section: false,
    wait: Wait::Event,
  },
  Mode {
    name: "batch-direct-64k-section",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN,
    section: true,
    wait: Wait::Event,
  },
  Mode {
    name: "batch-direct-128k-section",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN * 2,
    section: true,
    wait: Wait::Event,
  },
  Mode {
    name: "batch-direct-256k-section",
    code: TRANSACT_BATCH,
    bytes: LARGEST,
    section: true,
    wait: Wait::Event,
  },
  Mode {
    name: "single-buffered-16320-handle-wait",
    code: TRANSACT_SINGLE,
    bytes: SINGLE_BYTES,
    section: false,
    wait: Wait::Handle,
  },
  Mode {
    name: "batch-direct-64k-section-handle-wait",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN,
    section: true,
    wait: Wait::Handle,
  },
  Mode {
    name: "batch-direct-64k-section-two-event-wait",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN,
    section: true,
    wait: Wait::Multiple,
  },
];

/// One call in `wait`'s form; `idle` is the second, never-signalled event `Wait::Multiple` waits on as well.
fn transact_waiting(
  control: HANDLE,
  event: HANDLE,
  idle: HANDLE,
  wait: Wait,
  code: u32,
  input: &[u8],
  output: &mut [u8],
) -> Result<usize, i32> {
  match wait {
    Wait::Event => transact(control, event, code, input, output),
    Wait::Handle => {
      let mut done = 0u32;
      // SAFETY: both buffers stay owned and unmoved for the synchronous call; with no OVERLAPPED the call returns only
      // once the I/O has completed (it waits on the handle itself, as FspFsctlTransact does).
      let accepted = unsafe {
        DeviceIoControl(
          control,
          code,
          if input.is_empty() {
            std::ptr::null()
          } else {
            input.as_ptr().cast()
          },
          input.len() as u32,
          output.as_mut_ptr().cast(),
          output.len() as u32,
          &mut done,
          std::ptr::null_mut(),
        )
      };
      if accepted == 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
      }
      Ok(done as usize)
    }
    Wait::Multiple => {
      let mut overlapped = OVERLAPPED {
        hEvent: event,
        ..OVERLAPPED::default()
      };
      let mut done = 0u32;
      // SAFETY: both buffers and the overlapped block stay owned and unmoved until the call has settled below.
      let accepted = unsafe {
        DeviceIoControl(
          control,
          code,
          if input.is_empty() {
            std::ptr::null()
          } else {
            input.as_ptr().cast()
          },
          input.len() as u32,
          output.as_mut_ptr().cast(),
          output.len() as u32,
          &mut done,
          &mut overlapped,
        )
      };
      if accepted == 0 {
        let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if u32::try_from(error).ok() != Some(ERROR_IO_PENDING) {
          return Err(error);
        }
        let events = [event, idle];
        // SAFETY: both events are owned for the wait; the outstanding call's buffers stay owned until it settles.
        let waited = unsafe { WaitForMultipleObjects(2, events.as_ptr(), 0, INFINITE) };
        if waited != windows_sys::Win32::Foundation::WAIT_OBJECT_0 {
          // The idle event is never signalled, so any other answer is the wait's own failure.
          return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        // SAFETY: the same outstanding call, its event already signalled.
        if unsafe { GetOverlappedResult(control, &overlapped, &mut done, 1) } == 0 {
          return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
      }
      Ok(done as usize)
    }
  }
}

fn transact(
  control: HANDLE,
  event: HANDLE,
  code: u32,
  input: &[u8],
  output: &mut [u8],
) -> Result<usize, i32> {
  let mut overlapped = OVERLAPPED {
    hEvent: event,
    ..OVERLAPPED::default()
  };
  let mut done = 0u32;
  // SAFETY: both buffers and the overlapped block stay owned and unmoved until the call has settled below.
  let accepted = unsafe {
    DeviceIoControl(
      control,
      code,
      if input.is_empty() {
        std::ptr::null()
      } else {
        input.as_ptr().cast()
      },
      input.len() as u32,
      output.as_mut_ptr().cast(),
      output.len() as u32,
      &mut done,
      &mut overlapped,
    )
  };
  if accepted == 0 {
    let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    if u32::try_from(error).ok() != Some(ERROR_IO_PENDING) {
      return Err(error);
    }
    // SAFETY: the same outstanding call, its buffers still owned.
    if unsafe { GetOverlappedResult(control, &overlapped, &mut done, 1) } == 0 {
      return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
  }
  Ok(done as usize)
}

/// The responses to every request in `delivered`: a Create "not found", anything else "invalid request".
fn answer(delivered: &[u8], into: &mut Vec<u8>) -> usize {
  into.clear();
  let mut at = 0;
  let mut creates = 0;
  while at + 16 <= delivered.len() {
    let size = usize::from(u16::from_le_bytes([delivered[at + 2], delivered[at + 3]]));
    if size == 0 {
      break;
    }
    let kind = u32::from_le_bytes(delivered[at + 4..at + 8].try_into().unwrap());
    let mut response = [0u8; RESPONSE];
    response[2..4].copy_from_slice(&(RESPONSE as u16).to_le_bytes());
    response[4..8].copy_from_slice(&kind.to_le_bytes());
    response[8..16].copy_from_slice(&delivered[at + 8..at + 16]);
    let status = if kind == CREATE { NOT_FOUND } else { INVALID };
    response[20..24].copy_from_slice(&status.to_le_bytes());
    into.extend_from_slice(&response);
    if kind == CREATE {
      creates += 1;
    }
    at += size.next_multiple_of(8);
  }
  creates
}

fn free_drive() -> String {
  // SAFETY: this argument-free query reads the current DOS drive mask.
  let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
  let letter = ('E'..='Z')
    .find(|letter| used & (1 << (u32::from(*letter) - u32::from('A'))) == 0)
    .unwrap();
  format!("{letter}:")
}

fn ranks(samples: &mut [u64]) -> String {
  samples.sort_unstable();
  let at = |permille: usize| -> f64 {
    if samples.is_empty() {
      return 0.0;
    }
    let index = (samples.len() * permille).div_ceil(1000).max(1) - 1;
    samples[index.min(samples.len() - 1)] as f64 / 1000.0
  };
  format!(
    "n={} p50={:.1} p99={:.1} p999={:.1} max={:.1}",
    samples.len(),
    at(500),
    at(990),
    at(999),
    samples.last().copied().unwrap_or(0) as f64 / 1000.0
  )
}

/// The memory each mode's output lies in: private pages, and a pagefile section's view as the held one is.
struct Buffers {
  private: *mut core::ffi::c_void,
  section: HANDLE,
  view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Buffers {
  fn new() -> Self {
    // SAFETY: a fresh private allocation of LARGEST bytes, released in drop.
    let private = unsafe {
      VirtualAlloc(
        std::ptr::null(),
        LARGEST,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
      )
    };
    assert!(!private.is_null());
    // SAFETY: an unnamed pagefile-backed section of LARGEST bytes; closed in drop.
    let section = unsafe {
      CreateFileMappingW(
        INVALID_HANDLE_VALUE,
        std::ptr::null(),
        PAGE_READWRITE,
        0,
        LARGEST as u32,
        std::ptr::null(),
      )
    };
    assert!(!section.is_null());
    // SAFETY: the section just made, viewed whole; unmapped in drop.
    let view = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, LARGEST) };
    assert!(!view.Value.is_null());
    Self {
      private,
      section,
      view,
    }
  }

  /// The output for `mode`, lent until the next call.
  fn output(&mut self, mode: &Mode) -> &mut [u8] {
    let base = if mode.section {
      self.view.Value.cast::<u8>()
    } else {
      self.private.cast::<u8>()
    };
    // SAFETY: both regions are LARGEST bytes, owned here, touched by this thread alone; a mode's length is at most that.
    unsafe { std::slice::from_raw_parts_mut(base, mode.bytes.min(LARGEST)) }
  }
}

impl Drop for Buffers {
  fn drop(&mut self) {
    // SAFETY: the regions and handle made in new, each released once; no call is outstanding.
    unsafe {
      UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
        Value: self.view.Value,
      });
      CloseHandle(self.section);
      VirtualFree(self.private, 0, MEM_RELEASE);
    }
  }
}

/// The driver must refuse a batch output under its minimum, before any wait.
fn refusals(control: HANDLE, event: HANDLE, buffers: &mut Buffers) {
  for refused in [16 * 1024, 4 * 1024] {
    let mode = Mode {
      name: "refused",
      code: TRANSACT_BATCH,
      bytes: refused,
      section: true,
      wait: Wait::Event,
    };
    let answer = transact(control, event, TRANSACT_BATCH, &[], buffers.output(&mode));
    eprintln!(
      "WINFSP TRANSACT batch output {refused} bytes: {answer:?} (Win32 122 is the driver's STATUS_BUFFER_TOO_SMALL)"
    );
    assert!(
      answer.is_err(),
      "the driver must refuse a batch output under its minimum"
    );
  }
}

/// One client opening absent names until `stop`, each open's time kept under the mode it began and ended in.
fn client(drive: &str, mode: &AtomicUsize, stop: &AtomicBool) -> Vec<Vec<u64>> {
  let mut opens: Vec<Vec<u64>> = MODES.iter().map(|_| Vec::new()).collect();
  let mut number = 0u64;
  while !stop.load(Ordering::Relaxed) {
    let path: Vec<u16> = format!("{drive}\\absent-{number}")
      .encode_utf16()
      .chain(Some(0))
      .collect();
    number += 1;
    let before = mode.load(Ordering::Relaxed);
    let started = Instant::now();
    // SAFETY: a NUL-terminated path; the handle, if any, is closed at once.
    let handle = unsafe {
      CreateFileW(
        path.as_ptr(),
        GENERIC_READ,
        FILE_SHARE_READ,
        std::ptr::null(),
        OPEN_EXISTING,
        0,
        std::ptr::null_mut(),
      )
    };
    let took = started.elapsed().as_nanos() as u64;
    if handle != INVALID_HANDLE_VALUE {
      // SAFETY: the handle just opened.
      unsafe { CloseHandle(handle) };
    }
    if mode.load(Ordering::Relaxed) == before {
      opens[before].push(took);
    }
  }
  opens
}

/// Serves PER_ROUND Creates in `index`'s mode, keeping each call's time unless warming up.
fn serve(
  control: HANDLE,
  event: HANDLE,
  buffers: &mut Buffers,
  index: usize,
  responses: &mut Vec<u8>,
  calls: Option<&mut Vec<u64>>,
) {
  let chosen = &MODES[index];
  let mut kept = calls;
  let mut served = 0;
  // SAFETY: an unnamed manual-reset event, never signalled; closed below.
  let idle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  assert!(!idle.is_null());
  while served < PER_ROUND {
    let output = buffers.output(chosen);
    let started = Instant::now();
    let delivered = transact_waiting(
      control,
      event,
      idle,
      chosen.wait,
      chosen.code,
      responses,
      output,
    )
    .unwrap();
    let took = started.elapsed().as_nanos() as u64;
    let creates = answer(&output[..delivered], responses);
    if creates > 0
      && let Some(calls) = kept.as_mut()
    {
      calls.push(took);
    }
    served += creates;
  }
  // SAFETY: the event made above, closed once.
  unsafe { CloseHandle(idle) };
}

/// Answers whatever the client sent until it has stopped, so its last open is refused and it sees `stop`.
fn drain(
  control: HANDLE,
  event: HANDLE,
  buffers: &mut Buffers,
  responses: &mut Vec<u8>,
  client: &std::thread::ScopedJoinHandle<'_, Vec<Vec<u64>>>,
) {
  while !client.is_finished() {
    let output = buffers.output(&MODES[2]);
    if let Ok(delivered) = transact(control, event, TRANSACT_BATCH, responses, output) {
      answer(&output[..delivered], responses);
    }
  }
}

/// Audit step 7. Do: mount a WinFsp volume answered by this test's own loop, open absent names from one client, and
/// make the transaction each round in every mode in turn. Expect: each mode's call and client latencies printed with
/// every rank; a batch output under the driver's minimum is refused, before any wait.
#[test]
#[ignore = "a timing harness, run by name in release on a machine with WinFsp"]
fn transact_cost() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!(
      "SKIP: the transaction timing harness needs WINFSP_TEST_MOUNT=1 and WinFsp installed"
    );
    return;
  }
  let drive = free_drive();
  let (requests, _unused) = sync_channel(REQUEST_BOUND);
  let (purge_intents, mut readers) = crate::purge_intents::channel(1).unwrap();
  let private_opens = readers.first_mut().unwrap();
  let mounted = Mounted::new(
    &drive,
    true,
    requests,
    Waker::noop().clone(),
    None,
    purge_intents,
    private_opens,
  )
  .unwrap();
  // SAFETY: the DLL object is live until `mounted` drops at the end, after the client has stopped.
  let control = unsafe { (*mounted.file_system).volume_handle };
  // SAFETY: an unnamed manual-reset event; no borrowed descriptor or name.
  let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  assert!(!event.is_null());
  let mut buffers = Buffers::new();
  refusals(control, event, &mut buffers);
  let mode = AtomicUsize::new(0);
  let stop = AtomicBool::new(false);
  let mut calls: Vec<Vec<u64>> = MODES.iter().map(|_| Vec::new()).collect();
  let opens = std::thread::scope(|scope| {
    let running = scope.spawn(|| client(&drive, &mode, &stop));
    let mut responses = Vec::new();
    for round in 0..=ROUNDS {
      for offset in 0..MODES.len() {
        let index = (round + offset) % MODES.len();
        mode.store(index, Ordering::Relaxed);
        let kept = (round > 0).then_some(&mut calls[index]);
        serve(control, event, &mut buffers, index, &mut responses, kept);
      }
    }
    stop.store(true, Ordering::Relaxed);
    drain(control, event, &mut buffers, &mut responses, &running);
    running.join().unwrap()
  });
  for (index, chosen) in MODES.iter().enumerate() {
    let mut client_opens = opens[index].clone();
    eprintln!(
      "WINFSP TRANSACT mode={} call {} | client open {}",
      chosen.name,
      ranks(&mut calls[index]),
      ranks(&mut client_opens)
    );
  }
  drop(buffers);
  // SAFETY: the event made above, closed once; no call is outstanding.
  unsafe { CloseHandle(event) };
  drop(mounted);
}

/// Format: FspFsctlTransactCleanupKind and FspFsctlTransactCloseKind.
const CLEANUP: u32 = 3;
/// Format: see CLEANUP.
const CLOSE: u32 = 4;
/// Format: FILE_OPENED, a successful open's IoStatus.Information.
const FILE_OPENED: u32 = 1;
/// Format: MAXIMUM_ALLOWED, and FILE_ALL_ACCESS which it is answered with, as the DLL's access check grants.
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
/// Format: see MAXIMUM_ALLOWED.
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
/// Format: offsets in a Create request (Req.Create.DesiredAccess) and in its response (Rsp.Create.Opened), fsctl.h.
const DESIRED_ACCESS_AT: usize = 48;
/// Format: see DESIRED_ACCESS_AT.
const CONTEXT2_AT: usize = 32;
/// Format: see DESIRED_ACCESS_AT.
const GRANTED_AT: usize = 40;
/// Format: see DESIRED_ACCESS_AT.
const FILE_INFO_AT: usize = 48;
/// Format: FILE_ATTRIBUTE_ARCHIVE, a regular file's attributes as slates reports them.
const ARCHIVE: u32 = 0x20;
/// Shape: the opened file's size, mount_bench's 4,096-byte files.
const OPENED_BYTES: u64 = 4096;

/// How the harness's server serves: how many threads transact, whether a response rides the next receive, and what a
/// successful open's per-open context looks like.
struct Serving {
  name: &'static str,
  threads: usize,
  combined: bool,
  /// The UserContext2 of an open: a heap address, as the DLL arm's boxed open, or a small ledger token, as ours.
  context_pointer: bool,
}

const SERVINGS: [Serving; 4] = [
  Serving {
    name: "one-thread-combined-pointer",
    threads: 1,
    combined: true,
    context_pointer: true,
  },
  Serving {
    name: "one-thread-combined-token",
    threads: 1,
    combined: true,
    context_pointer: false,
  },
  Serving {
    name: "one-thread-separate-token",
    threads: 1,
    combined: false,
    context_pointer: false,
  },
  Serving {
    name: "two-threads-combined-pointer",
    threads: 2,
    combined: true,
    context_pointer: true,
  },
];

/// A successful open's response, in place: a 4,096-byte file, granted what the request asked (MAXIMUM_ALLOWED as all
/// access, as the DLL's check grants), its per-open context a heap-like address or a small token.
fn opened(response: &mut [u8; RESPONSE], request: &[u8], pointer: bool, next: &mut u64) {
  *next += 1;
  let context = if pointer {
    0x0000_01F0_0000_0000 + *next * 64
  } else {
    *next
  };
  let desired = u32::from_le_bytes(
    request[DESIRED_ACCESS_AT..DESIRED_ACCESS_AT + 4]
      .try_into()
      .unwrap(),
  );
  let granted = if desired & MAXIMUM_ALLOWED != 0 {
    FILE_ALL_ACCESS
  } else {
    desired
  };
  response[16..20].copy_from_slice(&FILE_OPENED.to_le_bytes());
  response[CONTEXT2_AT..CONTEXT2_AT + 8].copy_from_slice(&context.to_le_bytes());
  response[GRANTED_AT..GRANTED_AT + 4].copy_from_slice(&granted.to_le_bytes());
  let info = &mut response[FILE_INFO_AT..FILE_INFO_AT + 72];
  info[0..4].copy_from_slice(&ARCHIVE.to_le_bytes());
  info[8..16].copy_from_slice(&OPENED_BYTES.to_le_bytes());
  info[16..24].copy_from_slice(&OPENED_BYTES.to_le_bytes());
  info[56..64].copy_from_slice(&(*next).to_le_bytes());
}

/// Successful answers to every request in `delivered`: a Create opened as a 4,096-byte file, a Cleanup and a Close
/// done, anything else "invalid request". Returns the kinds answered, in order.
fn answer_open(delivered: &[u8], into: &mut Vec<u8>, pointer: bool, next: &mut u64) -> Vec<u32> {
  into.clear();
  let mut kinds = Vec::new();
  let mut at = 0;
  while at + 16 <= delivered.len() {
    let size = usize::from(u16::from_le_bytes([delivered[at + 2], delivered[at + 3]]));
    if size == 0 {
      break;
    }
    let kind = u32::from_le_bytes(delivered[at + 4..at + 8].try_into().unwrap());
    let mut response = [0u8; RESPONSE];
    response[2..4].copy_from_slice(&(RESPONSE as u16).to_le_bytes());
    response[4..8].copy_from_slice(&kind.to_le_bytes());
    response[8..16].copy_from_slice(&delivered[at + 8..at + 16]);
    match kind {
      CREATE => opened(&mut response, &delivered[at..at + size], pointer, next),
      CLEANUP | CLOSE => {}
      _ => response[20..24].copy_from_slice(&INVALID.to_le_bytes()),
    }
    into.extend_from_slice(&response);
    kinds.push(kind);
    at += size.next_multiple_of(8);
  }
  kinds
}

/// Keeps one call's time under the kind of the first request it delivered: Create, Cleanup or Close.
fn keep(calls: &mut [Vec<u64>; 3], first: Option<u32>, took: u64) {
  let slot = match first {
    Some(CREATE) => 0,
    Some(CLEANUP) => 1,
    Some(CLOSE) => 2,
    _ => return,
  };
  calls[slot].push(took);
}

/// One serving thread: until `stop`, transact on `control` and answer every request, keeping each call that delivered a
/// request under the kind of its first one (Create, Cleanup, Close). Separate serving sends a batch's responses alone,
/// then receives.
fn serve_opens(
  control: usize,
  serving: &Serving,
  stop: &AtomicBool,
  served: &AtomicUsize,
) -> [Vec<u64>; 3] {
  // A handle is a value, carried between threads as one.
  let control = control as HANDLE;
  // SAFETY: an unnamed manual-reset event for this thread's calls; closed at the end.
  let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  assert!(!event.is_null());
  let mut output = vec![0u8; BATCH_MIN];
  let mut responses = Vec::new();
  let mut next = 0u64;
  let mut calls: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
  while !stop.load(Ordering::Relaxed) {
    if !serving.combined && !responses.is_empty() {
      transact(control, event, TRANSACT_BATCH, &responses, &mut []).unwrap();
      responses.clear();
    }
    let started = Instant::now();
    let Ok(delivered) = transact(control, event, TRANSACT_BATCH, &responses, &mut output) else {
      break;
    };
    let took = started.elapsed().as_nanos() as u64;
    let kinds = answer_open(
      &output[..delivered],
      &mut responses,
      serving.context_pointer,
      &mut next,
    );
    keep(&mut calls, kinds.first().copied(), took);
    served.fetch_add(kinds.len(), Ordering::Relaxed);
  }
  // Send what was answered last, so no request is left processing when the volume is deleted.
  if !responses.is_empty() {
    let _ = transact(control, event, TRANSACT_BATCH, &responses, &mut []);
  }
  // SAFETY: the event made above, closed once; this thread's calls have settled.
  unsafe { CloseHandle(event) };
  calls
}

/// One client opening an existing name and closing it again until `stop`; each open-and-close timed.
fn open_and_close(drive: &str, stop: &AtomicBool) -> (Vec<u64>, u64) {
  let mut times = Vec::new();
  let mut refused = 0u64;
  let path: Vec<u16> = format!("{drive}\\file")
    .encode_utf16()
    .chain(Some(0))
    .collect();
  while !stop.load(Ordering::Relaxed) {
    let started = Instant::now();
    // SAFETY: a NUL-terminated path; the handle, if any, is closed at once.
    let handle = unsafe {
      CreateFileW(
        path.as_ptr(),
        GENERIC_READ,
        FILE_SHARE_READ,
        std::ptr::null(),
        OPEN_EXISTING,
        0,
        std::ptr::null_mut(),
      )
    };
    if handle == INVALID_HANDLE_VALUE {
      refused += 1;
    } else {
      // SAFETY: the handle just opened.
      unsafe { CloseHandle(handle) };
    }
    times.push(started.elapsed().as_nanos() as u64);
  }
  (times, refused)
}

/// One serving pattern on a fresh volume: one client for `opens` open-and-closes after a warm-up, every call kept.
fn measure_serving(serving: &Serving, opens: usize) {
  let drive = free_drive();
  let (requests, _unused) = sync_channel(REQUEST_BOUND);
  let (purge_intents, mut readers) = crate::purge_intents::channel(1).unwrap();
  let mounted = Mounted::new(
    &drive,
    true,
    requests,
    Waker::noop().clone(),
    None,
    purge_intents,
    readers.first_mut().unwrap(),
  )
  .unwrap();
  // SAFETY: the DLL object is live until `mounted` drops, after every thread here has joined.
  let control = unsafe { (*mounted.file_system).volume_handle } as usize;
  let stop_servers = AtomicBool::new(false);
  let stop_client = AtomicBool::new(false);
  let served = AtomicUsize::new(0);
  let (calls, times) = std::thread::scope(|scope| {
    let servers: Vec<_> = (0..serving.threads)
      .map(|_| scope.spawn(|| serve_opens(control, serving, &stop_servers, &served)))
      .collect();
    let client = scope.spawn(|| open_and_close(&drive, &stop_client));
    // Three IRPs an open-and-close; the first tenth is a warm-up, cut from the client's times below.
    while served.load(Ordering::Relaxed) < opens * 3 + opens / 10 * 3 {
      std::thread::yield_now();
    }
    stop_client.store(true, Ordering::Relaxed);
    let times = client.join().unwrap();
    stop_servers.store(true, Ordering::Relaxed);
    let calls: Vec<[Vec<u64>; 3]> = servers
      .into_iter()
      .map(|server| server.join().unwrap())
      .collect();
    (calls, times)
  });
  let mut by_kind: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
  for thread in calls {
    for (kind, mut kept) in thread.into_iter().enumerate() {
      by_kind[kind].append(&mut kept);
    }
  }
  let (times, refused) = times;
  let mut client_times: Vec<u64> = times.into_iter().skip(opens / 10).collect();
  eprintln!(
    "WINFSP OPENCOST serving={} refused={refused} client open+close {} | call before Create {} | before Cleanup {} | before Close {}",
    serving.name,
    ranks(&mut client_times),
    ranks(&mut by_kind[0]),
    ranks(&mut by_kind[1]),
    ranks(&mut by_kind[2])
  );
  drop(mounted);
}

/// Audit step 7. Do: serve one client's open-and-close loop on a WinFsp volume this test answers itself, each serving
/// pattern on a fresh volume, rotated: one thread combining each response with the next receive and an open's context a
/// pointer (the DLL's form) or a token (ours), one thread sending responses apart (the receiver's form), and two threads
/// combining (the DLL's two dispatchers). Expect: each pattern's client latency and per-IRP call times printed, every rank.
#[test]
#[ignore = "a timing harness, run by name in release on a machine with WinFsp"]
fn open_cost() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the open-cost harness needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  for round in 0..2 {
    for offset in 0..SERVINGS.len() {
      let serving = &SERVINGS[(round + offset) % SERVINGS.len()];
      measure_serving(serving, PER_ROUND * 5);
    }
  }
}
