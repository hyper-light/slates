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
  CloseHandle, ERROR_IO_PENDING, GENERIC_READ, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, OPEN_EXISTING};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Memory::{
  CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
  MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc,
  VirtualFree,
};
use windows_sys::Win32::System::Threading::CreateEventW;

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
}

const MODES: [Mode; 5] = [
  Mode {
    name: "single-buffered-16320",
    code: TRANSACT_SINGLE,
    bytes: SINGLE_BYTES,
    section: false,
  },
  Mode {
    name: "batch-direct-64k-private",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN,
    section: false,
  },
  Mode {
    name: "batch-direct-64k-section",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN,
    section: true,
  },
  Mode {
    name: "batch-direct-128k-section",
    code: TRANSACT_BATCH,
    bytes: BATCH_MIN * 2,
    section: true,
  },
  Mode {
    name: "batch-direct-256k-section",
    code: TRANSACT_BATCH,
    bytes: LARGEST,
    section: true,
  },
];

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
    if error != ERROR_IO_PENDING as i32 {
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
  let (purge_intents, mut private_opens) = crate::purge_intents::channel().unwrap();
  let mounted = Mounted::new(
    &drive,
    true,
    requests,
    Waker::noop().clone(),
    None,
    purge_intents,
    &mut private_opens,
  )
  .unwrap();
  // SAFETY: the DLL object is live until `mounted` drops at the end, after the client has stopped.
  let control = unsafe { (*mounted.file_system).volume_handle };
  // SAFETY: an unnamed manual-reset event; no borrowed descriptor or name.
  let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  assert!(!event.is_null());
  // SAFETY: a fresh private allocation of LARGEST bytes, released below.
  let private = unsafe {
    VirtualAlloc(
      std::ptr::null(),
      LARGEST,
      MEM_COMMIT | MEM_RESERVE,
      PAGE_READWRITE,
    )
  };
  assert!(!private.is_null());
  // SAFETY: an unnamed pagefile-backed section of LARGEST bytes, viewed whole; both closed below.
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
  // SAFETY: as above.
  let view = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, LARGEST) };
  assert!(!view.Value.is_null());
  // SAFETY: both regions are LARGEST bytes, owned here, and only this thread touches them until they are freed.
  let (private_bytes, section_bytes) = unsafe {
    (
      std::slice::from_raw_parts_mut(private.cast::<u8>(), LARGEST),
      std::slice::from_raw_parts_mut(view.Value.cast::<u8>(), LARGEST),
    )
  };
  for refused in [16 * 1024, 4 * 1024] {
    let answer = transact(
      control,
      event,
      TRANSACT_BATCH,
      &[],
      &mut section_bytes[..refused],
    );
    eprintln!(
      "WINFSP TRANSACT batch output {refused} bytes: {answer:?} (Win32 122 is the driver's STATUS_BUFFER_TOO_SMALL)"
    );
    assert!(
      answer.is_err(),
      "the driver must refuse a batch output under its minimum"
    );
  }
  let mode = AtomicUsize::new(0);
  let stop = AtomicBool::new(false);
  let mut calls: Vec<Vec<u64>> = MODES.iter().map(|_| Vec::new()).collect();
  let client = std::thread::scope(|scope| {
    let client = scope.spawn(|| {
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
    });
    let mut responses = Vec::new();
    let mut warm = true;
    for round in 0..=ROUNDS {
      for offset in 0..MODES.len() {
        let index = (round + offset) % MODES.len();
        let chosen = &MODES[index];
        mode.store(index, Ordering::Relaxed);
        let mut served = 0;
        while served < PER_ROUND {
          let output = if chosen.section {
            &mut section_bytes[..chosen.bytes]
          } else {
            &mut private_bytes[..chosen.bytes]
          };
          let started = Instant::now();
          let delivered = transact(control, event, chosen.code, &responses, output).unwrap();
          let took = started.elapsed().as_nanos() as u64;
          let creates = answer(&output[..delivered], &mut responses);
          if creates > 0 && !warm {
            calls[index].push(took);
          }
          served += creates;
        }
      }
      warm = false;
    }
    stop.store(true, Ordering::Relaxed);
    // Answer whatever the client's last open sent, so it can see its refusal and stop.
    while !client.is_finished() {
      let output = &mut section_bytes[..BATCH_MIN];
      if let Ok(delivered) = transact(control, event, TRANSACT_BATCH, &responses, output) {
        answer(&output[..delivered], &mut responses);
      }
    }
    client.join().unwrap()
  });
  for (index, chosen) in MODES.iter().enumerate() {
    let mut opens = client[index].clone();
    eprintln!(
      "WINFSP TRANSACT mode={} call {} | client open {}",
      chosen.name,
      ranks(&mut calls[index]),
      ranks(&mut opens)
    );
  }
  // SAFETY: the regions and handles made above, each released once; no call is outstanding.
  unsafe {
    UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
    CloseHandle(section);
    VirtualFree(private, 0, MEM_RELEASE);
    CloseHandle(event);
  }
  drop(mounted);
}
