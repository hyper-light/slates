//! How many receive workers a mount runs (A-219), measured from this machine once a process, never named, and never on
//! the mount's path.
//!
//! A second worker pays only when it is already waiting in the kernel as the first sends its response, so the next
//! request is handed to a waiting thread rather than to one still on its way back into the call. Whether that wins is
//! the machine's: the open-cost harness measured two waiting threads 8-10 us an IRP faster on CI's arm64 runner and
//! about 7 us an IRP slower on its x86_64 one (audit step 7, BENCHMARKS "What a successful open costs under each serving
//! pattern"). So the probe measures that hand-off itself. It makes a hidden WinFsp volume with no drive and no file
//! system behind it, which no other process knows the name of. It serves the volume with one waiting worker, then with
//! two, in interleaved rounds, and times one client opening and closing a file through the volume's device path: a
//! Create answered "opened" and the Cleanup and Close that follow it, the closest requests a client sends. The probe
//! serves through the receive worker's own calls ([`crate::dispatcher::receive_batch`],
//! [`crate::dispatcher::send_response`]) and its own journal. Each request is answered at once, so the rounds differ
//! only in how many workers wait. More workers are taken only where a one-sided sign test over the paired rounds rejects
//! "they gain nothing" at 1 in 100 ([`crate::inflight::capture::workers_by_sign_test`]). The rounds and the count are
//! logged.
//!
//! An open of an absent name, one request with nothing behind it, was the first probe. It showed no second-worker win
//! on either lane (diagnostics run 42130583401, 35 processes a lane), because the worker was back in its receive long
//! before the client's next open; the one x86_64 process that derived two met a shift between that runner's two regimes
//! of open time (near 42 and near 58 us), not a win.
//!
//! The probe runs on the mount's second worker before that worker serves (`crate::dispatcher`), so a mount is answered
//! by its first worker at once. The probe took 105-216 ms on CI's runners, ten times the whole mount (A-213: 10-19 ms).
//! A stopped worker ends the probe at its next open. The probe volume is deleted however the probe ends, and with the
//! process when it is killed (the volume lives on its control handle). The probe writes nothing to disk.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use windows_sys::Win32::Foundation::{
  CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
  CreateFileW, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
  OPEN_EXISTING,
};

use crate::ffi::{self, FileSystem, Interface};
use crate::host::WinFspError;
use crate::inflight::{self, RESPONSE_BASE, Refusal, Request, capture};
use crate::purge_intents::Intents;

/// Shape: the rounds of each worker count, interleaved and rotated, so a drifting load lands on every count alike and the
/// spread of a count is the range of nine medians. Five let one x86_64 probe of 35 meet a shift between that runner's
/// two regimes of open time and take two (run 42130583401); off the mount's path, more rounds cost a mount nothing.
const ROUNDS: usize = 9;
/// Shape: the timed open-and-closes of a round, enough that one scheduling delay cannot move its median.
const OPENS: usize = 200;
/// Shape: the untimed open-and-closes that begin each round, while its fresh threads first wait.
const WARM: usize = 20;
/// Format: the one name every probe open asks for, under the probe volume's root.
const PROBE_NAME: &str = "\\slates-receive-probe";
/// Format: the Win32 prefix that names a device by its NT path, as WinFsp's own tests open an unmounted volume.
const GLOBALROOT: &str = "\\\\?\\GLOBALROOT";
/// Format: offsets in a Create response (`FSP_FSCTL_TRANSACT_RSP`, WinFsp v2.1 `inc/winfsp/fsctl.h`):
/// `IoStatus.Information`, then in `Rsp.Create.Opened` the per-open `UserContext2`, `GrantedAccess` and `FileInfo`, whose
/// `FileAttributes` comes first and `IndexNumber` 56 bytes in.
const INFORMATION_AT: usize = 16;
/// Format: see INFORMATION_AT.
const CONTEXT_AT: usize = 32;
/// Format: see INFORMATION_AT.
const GRANTED_AT: usize = 40;
/// Format: see INFORMATION_AT.
const FILE_INFO_AT: usize = 48;
/// Format: see INFORMATION_AT.
const INDEX_AT: usize = FILE_INFO_AT + 56;
/// Format: `Req.Create.DesiredAccess` in a Create request (fsctl.h), granted as asked.
const DESIRED_ACCESS_AT: usize = 48;
/// Format: `FILE_OPENED`, a successful open's `IoStatus.Information`.
const FILE_OPENED: u32 = 1;
/// Format: `FILE_ATTRIBUTE_ARCHIVE`, a regular file's attributes as slates reports them.
const ARCHIVE: u32 = 0x20;

/// What a probe measured: the count it derived, and each round's median open-and-close, in nanoseconds, by waiting
/// workers (the first row one worker).
pub(crate) type Measured = (usize, Vec<Vec<u64>>);

/// The probe volume's interface: no callback, since the probe answers every request itself and the DLL dispatches none.
static PROBE_INTERFACE: Interface = Interface::EMPTY;

/// The receive workers this process's mounts run, measured once, on the first caller's thread, while `stopped` stays
/// false; `None` when it turned true first. Two first callers racing may each measure; the first to finish sets the
/// count. `intents` is the calling worker's own channel, which the probe keeps draining.
pub(crate) fn receive_workers(
  intents: &mut Intents,
  stopped: &dyn Fn() -> bool,
) -> Result<Option<usize>, WinFspError> {
  // DIAG ONLY, never for main: the A/B fixes each arm's worker count at build time, with no probe running.
  if let Some(fixed) = option_env!("SLATES_DIAG_RECEIVE_WORKERS") {
    let _ = (&intents, &stopped);
    return fixed.parse().map(Some).map_err(|_| WinFspError::MountThread);
  }
  static WORKERS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
  if let Some(count) = WORKERS.get() {
    return Ok(Some(decided(*count)));
  }
  let Some((count, _medians)) = measure(intents, stopped)? else {
    return Ok(None);
  };
  Ok(Some(decided(*WORKERS.get_or_init(|| count))))
}

/// The count a mount runs: the measured one.
#[cfg(not(test))]
fn decided(measured: usize) -> usize {
  measured
}

/// The library's own tests run every mount with the most workers, the later ones released only once the probe has
/// measured, so the live kill oracle proves the multi-worker recovery and the late release on every lane whatever its
/// probe derives (A-219). The probe itself is tested below; the CLI's mount tests run the measured count.
#[cfg(test)]
fn decided(_measured: usize) -> usize {
  capture::MAX_WORKERS
}

/// Measures on a probe volume of its own (see [`measure_on`]).
pub(crate) fn measure(
  intents: &mut Intents,
  stopped: &dyn Fn() -> bool,
) -> Result<Option<Measured>, WinFspError> {
  let api = ffi::api().map_err(|_| WinFspError::NotInstalled)?;
  crate::host::checked_dll(api)?;
  let volume = ProbeVolume::new(api)?;
  measure_on(&volume, &volume.probe_path(), intents, stopped)
}

/// Measures one waiting worker against each larger count up to [`capture::MAX_WORKERS`] on `volume`, timing opens of
/// `path`, and derives the count, logging both. Returns the count and each round's median open-and-close, in
/// nanoseconds, by waiting workers (the first row one worker); `None` when `stopped` turned true first.
pub(crate) fn measure_on(
  volume: &ProbeVolume,
  path: &[u16],
  intents: &mut Intents,
  stopped: &dyn Fn() -> bool,
) -> Result<Option<Measured>, WinFspError> {
  let started = Instant::now();
  // The other probe workers' channels; their sender stays alive for the probe, since a closed channel is a failure.
  let (_sender, mut others) =
    crate::purge_intents::channel(capture::MAX_WORKERS.saturating_sub(1))?;
  crate::dispatcher::announce_ready(volume.control(), intents)?;
  let mut channels: Vec<&mut Intents> = std::iter::once(intents).chain(others.iter_mut()).collect();
  let mut journals: Vec<Vec<u8>> = (0..capture::MAX_WORKERS)
    .map(|_| vec![0; capture::BYTES])
    .collect();
  let mut medians: Vec<Vec<u64>> = (0..capture::MAX_WORKERS)
    .map(|_| Vec::with_capacity(ROUNDS))
    .collect();
  for round in 0..ROUNDS {
    for offset in 0..capture::MAX_WORKERS {
      let arm = round
        .checked_add(offset)
        .and_then(|turn| turn.checked_rem(capture::MAX_WORKERS))
        .ok_or(WinFspError::Capture(Refusal::Length))?;
      let waiting = arm
        .checked_add(1)
        .ok_or(WinFspError::Capture(Refusal::Length))?;
      let Some(median) = serve_round(volume, path, waiting, &mut channels, &mut journals, stopped)?
      else {
        slates_machine::log_line!(
          "slates-winfsp: the receive-worker probe stopped after {} ms, its worker told to end",
          started.elapsed().as_millis()
        );
        return Ok(None);
      };
      medians
        .get_mut(arm)
        .ok_or(WinFspError::Capture(Refusal::Length))?
        .push(median);
    }
  }
  let count = capture::workers_by_sign_test(&medians);
  let rounds = u32::try_from(ROUNDS).unwrap_or(u32::MAX);
  let needed = capture::sign_test_wins_needed(rounds)
    .map_or_else(|| "none".to_owned(), |wins| wins.to_string());
  slates_machine::log_line!(
    "slates-winfsp: receive workers = {count}: median open-and-close of each round, in ns, by waiting workers from one: \
     {medians:?}; more workers only where the paired rounds' sign test rejects no gain at 1 in {}, which at {rounds} \
     rounds needs {needed} wins of {rounds}; probe took {} ms",
    capture::SIGN_TEST_ONE_IN,
    started.elapsed().as_millis()
  );
  Ok(Some((count, medians)))
}

/// A hidden WinFsp volume with no mount point, deleted on drop. Only the probe's own threads transact on it.
pub(crate) struct ProbeVolume {
  api: &'static ffi::Api,
  file_system: *mut FileSystem,
  control: HANDLE,
  /// The volume's NT device name, unterminated (`\Device\Volume{...}`).
  name: Vec<u16>,
}

impl ProbeVolume {
  pub(crate) fn new(api: &'static ffi::Api) -> Result<Self, WinFspError> {
    let file_system =
      crate::host::create_file_system(api, &crate::host::volume_params(true), &PROBE_INTERFACE)?;
    // SAFETY: FspFileSystemCreate returned this initialized public prefix: the volume's control handle, owned by the
    // object until it is deleted on drop, and its terminated volume name.
    let (control, name) = unsafe { ((*file_system).volume_handle, (*file_system).volume_name) };
    Ok(Self {
      api,
      file_system,
      control,
      name: name.iter().copied().take_while(|unit| *unit != 0).collect(),
    })
  }

  /// The volume's control handle, live until this value drops, after every probe thread has joined.
  fn control(&self) -> HANDLE {
    self.control
  }

  /// The probe's file through the volume's device: `\\?\GLOBALROOT\Device\Volume{...}\slates-receive-probe`.
  pub(crate) fn probe_path(&self) -> Vec<u16> {
    GLOBALROOT
      .encode_utf16()
      .chain(self.name.iter().copied())
      .chain(PROBE_NAME.encode_utf16())
      .chain(Some(0))
      .collect()
  }

  /// The volume's entry in the object manager's `\Device` directory (`Volume{...}`), for the tests that it is gone.
  #[cfg(test)]
  pub(crate) fn device_entry(&self) -> String {
    let name = String::from_utf16_lossy(&self.name);
    name.rsplit('\\').next().unwrap_or_default().to_owned()
  }
}

impl Drop for ProbeVolume {
  fn drop(&mut self) {
    // SAFETY: every probe thread has joined (each round is a thread scope), so no transaction uses the object.
    unsafe { (self.api.delete)(self.file_system) };
  }
}

/// How a round tells its workers to end. A transact waits inside the call (WinFsp v2.1 `src/sys/fsctl.c:95-99`), so a
/// worker is ended by a request, never by cancelling its wait.
struct Ending {
  /// Set once the round's timed opens are done: a worker answers any Create received after this "not found" and ends.
  stop: AtomicBool,
  /// The workers that have ended or are ending: each counts itself before it sends that Create's response, so when the
  /// client's open returns it knows whether another worker still waits for one.
  ended: AtomicUsize,
  /// Set when the round failed with no open outstanding: a worker then ends at any return, its idle timeout too.
  abandoned: AtomicBool,
}

/// One round: `waiting` workers serve while this thread times its open-and-closes; then each worker is ended by one
/// more open and joined. Returns the round's median, `None` when `stopped` turned true first.
fn serve_round(
  volume: &ProbeVolume,
  path: &[u16],
  waiting: usize,
  channels: &mut [&mut Intents],
  journals: &mut [Vec<u8>],
  stopped: &dyn Fn() -> bool,
) -> Result<Option<u64>, WinFspError> {
  // A handle is a value, carried to the workers as one.
  let control = volume.control().addr();
  let ending = Ending {
    stop: AtomicBool::new(false),
    ended: AtomicUsize::new(0),
    abandoned: AtomicBool::new(false),
  };
  std::thread::scope(|scope| {
    let mut threads = Vec::with_capacity(waiting);
    let mut spawned = Ok(());
    for (intents, journal) in channels.iter_mut().zip(journals.iter_mut()).take(waiting) {
      let ending = &ending;
      let intents = &mut **intents;
      match std::thread::Builder::new()
        .name("slates-winfsp-receive-probe".to_owned())
        .spawn_scoped(scope, move || {
          serve_waiting(control, journal, intents, ending)
        }) {
        Ok(thread) => threads.push(thread),
        Err(_) => {
          spawned = Err(WinFspError::MountThread);
          break;
        }
      }
    }
    let timed = spawned
      .and_then(|()| time_cycles(path, stopped))
      .and_then(|median| {
        // No open is outstanding: each returned once its response was taken (a Close may still be queued, which ends no
        // worker). Every Create from here ends the worker that answers it, and one is made only while a worker has not
        // counted itself ending, so some worker receives it.
        ending.stop.store(true, Ordering::Release);
        while ending.ended.load(Ordering::Acquire) < threads.len() {
          open_ending(path)?;
        }
        Ok(median)
      });
    if timed.is_err() {
      ending.abandoned.store(true, Ordering::Release);
    }
    let mut outcome = timed;
    for thread in threads {
      let joined = thread
        .join()
        .map_err(|_| WinFspError::MountThread)
        .and_then(|served| served);
      if let (Ok(_), Err(error)) = (&outcome, joined) {
        outcome = Err(error);
      }
    }
    outcome
  })
}

/// One waiting worker, serving as a mount's worker does (A-215): receive into its journal, answer each request at once (a
/// Create "opened", or "not found" once the round stops; a Cleanup and a Close done; anything else cancelled), every
/// response but a batch's last sent alone, the last riding the call that receives the next batch. It ends once it has
/// answered a Create after the round's stop, that response sent alone, since a hand-off would wait for another request.
fn serve_waiting(
  control: usize,
  journal: &mut [u8],
  intents: &mut Intents,
  ending: &Ending,
) -> Result<(), WinFspError> {
  let control = control as HANDLE;
  // What this worker's last batch in each buffer filled; its first receive into each clears all of it.
  let mut fill = capture::Fill::new();
  // Each open's context, as a mount's ledger tokens are: small, distinct, never zero.
  let mut context = 0u64;
  // The transfer of the batch the last combined call delivered, to be taken next.
  let mut handed = None;
  loop {
    let batch = match handed.take() {
      None => {
        let mut receiving = capture::Capture::new(&mut *journal)
          .and_then(|capture| capture.begin(&fill))
          .map_err(WinFspError::Capture)?;
        let reported = crate::dispatcher::receive_batch(control, receiving.buffer_mut(), intents)?;
        if ending.abandoned.load(Ordering::Acquire) {
          return Ok(());
        }
        receiving
          .received(
            reported,
            &mut fill,
            crate::dispatcher::performance_counter()?,
          )
          .map_err(WinFspError::Capture)?
      }
      Some(reported) => {
        if ending.abandoned.load(Ordering::Acquire) {
          return Ok(());
        }
        let stamp = crate::dispatcher::performance_counter()?;
        capture::Capture::new(&mut *journal)
          .and_then(|capture| capture.handed_off(reported, &mut fill, stamp))
          .map_err(WinFspError::Capture)?
      }
    };
    let stopping = ending.stop.load(Ordering::Acquire);
    match answer_batch(
      control,
      batch,
      intents,
      &mut fill,
      &mut context,
      stopping,
      ending,
    )? {
      Answered::HandedOff(reported) => handed = Some(reported),
      Answered::Sent => {}
      Answered::Ended => return Ok(()),
    }
  }
}

/// How a probe worker's batch was answered.
enum Answered {
  /// Its last response rode a call that delivered this many bytes, the next batch.
  HandedOff(usize),
  /// Every response was sent alone.
  Sent,
  /// It held a Create received after the round's stop: this worker has ended.
  Ended,
}

fn answer_batch(
  control: HANDLE,
  mut batch: capture::Batch<'_>,
  intents: &mut Intents,
  fill: &mut capture::Fill,
  context: &mut u64,
  stopping: bool,
  ending: &Ending,
) -> Result<Answered, WinFspError> {
  let not_found = crate::ntstatus(&slates_vfs::VfsError::NotFound).as_i32();
  let mut ends = false;
  while let Some(request) = batch.next_request().map_err(WinFspError::Capture)? {
    let mut response = request.cancelled_response().map_err(WinFspError::Capture)?;
    match request.kind() {
      inflight::CREATE_KIND if stopping => {
        inflight::finish_response(&mut response, not_found).map_err(WinFspError::Capture)?;
        if !ends {
          ending.ended.fetch_add(1, Ordering::Release);
          ends = true;
        }
      }
      inflight::CREATE_KIND => {
        *context = context.saturating_add(1);
        opened(&mut response, &request, *context)?;
      }
      inflight::CLEANUP_KIND | inflight::CLOSE_KIND => {
        inflight::finish_response(&mut response, 0).map_err(WinFspError::Capture)?;
      }
      _ => {}
    }
    batch
      .record_response(&request, &response)
      .map_err(WinFspError::Capture)?;
    if batch.is_last() && !ends {
      let mut hand_off = batch
        .hand_off(&request, fill)
        .map_err(WinFspError::Capture)?;
      let (response, buffer) = hand_off.transaction();
      let reported = crate::dispatcher::send_and_receive(control, response, buffer, intents)?;
      return Ok(Answered::HandedOff(reported));
    }
    crate::dispatcher::send_response(control, &response, intents)?;
    batch
      .acknowledge(request.hint())
      .map_err(WinFspError::Capture)?;
  }
  Ok(if ends {
    Answered::Ended
  } else {
    Answered::Sent
  })
}

/// A Create's success, in place: the file opened with what it asked for granted, `context` its per-open context and
/// index number, a regular file's attributes and an empty size.
fn opened(
  response: &mut [u8; RESPONSE_BASE],
  request: &Request<'_>,
  context: u64,
) -> Result<(), WinFspError> {
  let desired: [u8; size_of::<u32>()] = request
    .bytes()
    .get(DESIRED_ACCESS_AT..DESIRED_ACCESS_AT.saturating_add(size_of::<u32>()))
    .and_then(|bytes| bytes.try_into().ok())
    .ok_or(WinFspError::Capture(Refusal::Length))?;
  put(response, INFORMATION_AT, &FILE_OPENED.to_le_bytes())?;
  put(response, CONTEXT_AT, &context.to_le_bytes())?;
  put(response, GRANTED_AT, &desired)?;
  put(response, FILE_INFO_AT, &ARCHIVE.to_le_bytes())?;
  put(response, INDEX_AT, &context.to_le_bytes())?;
  inflight::finish_response(response, 0)
    .map(|_| ())
    .map_err(WinFspError::Capture)
}

fn put(response: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), WinFspError> {
  let end = at
    .checked_add(bytes.len())
    .ok_or(WinFspError::Capture(Refusal::Length))?;
  response
    .get_mut(at..end)
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .copy_from_slice(bytes);
  Ok(())
}

/// Opens and closes the probe's file WARM and then OPENS times, and returns the timed ones' median; `None` as soon as
/// `stopped` is true.
fn time_cycles(path: &[u16], stopped: &dyn Fn() -> bool) -> Result<Option<u64>, WinFspError> {
  let mut times = Vec::with_capacity(OPENS);
  for cycle in 0..WARM.saturating_add(OPENS) {
    if stopped() {
      return Ok(None);
    }
    let began = Instant::now();
    let handle = create(path).map_err(|code| WinFspError::Os {
      call: "open the receive probe's file",
      code,
    })?;
    close(handle)?;
    if cycle >= WARM {
      times.push(u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
  }
  times.sort_unstable();
  times
    .get(times.len() / 2)
    .copied()
    .map(Some)
    .ok_or(WinFspError::Capture(Refusal::Length))
}

/// One open made after the round's stop, which a worker must answer "not found" as it ends.
fn open_ending(path: &[u16]) -> Result<(), WinFspError> {
  match create(path) {
    Ok(handle) => {
      close(handle)?;
      Err(WinFspError::Capture(Refusal::State))
    }
    Err(code) if code == i32::try_from(ERROR_FILE_NOT_FOUND).ok() => Ok(()),
    Err(code) => Err(WinFspError::Os {
      call: "end a receive-probe worker",
      code,
    }),
  }
}

/// Opens `path` for its attributes alone; the system's error code when refused.
fn create(path: &[u16]) -> Result<HANDLE, Option<i32>> {
  // SAFETY: a NUL-terminated path; no security attributes or template; a returned handle is closed by the caller.
  let handle = unsafe {
    CreateFileW(
      path.as_ptr(),
      FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
      std::ptr::null(),
      OPEN_EXISTING,
      0,
      std::ptr::null_mut(),
    )
  };
  if handle == INVALID_HANDLE_VALUE {
    return Err(std::io::Error::last_os_error().raw_os_error());
  }
  Ok(handle)
}

fn close(handle: HANDLE) -> Result<(), WinFspError> {
  // SAFETY: a handle `create` just returned, closed once.
  if unsafe { CloseHandle(handle) } == 0 {
    return Err(WinFspError::Os {
      call: "close the receive probe's file",
      code: std::io::Error::last_os_error().raw_os_error(),
    });
  }
  Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
  #![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
  )]
  use super::*;
  use std::collections::BTreeSet;
  use std::time::Duration;

  /// `OBJECT_DIRECTORY_INFORMATION`: one entry of an object-manager directory, as `NtQueryDirectoryObject` returns it.
  #[repr(C)]
  struct DirectoryEntry {
    name: windows_sys::Win32::Foundation::UNICODE_STRING,
    type_name: windows_sys::Win32::Foundation::UNICODE_STRING,
  }

  /// Every entry of the object manager's `\Device` directory: a WinFsp volume is one (`Volume{...}`) while it lives.
  pub(crate) fn device_entries() -> BTreeSet<String> {
    use windows_sys::Wdk::Foundation::{NtClose, OBJECT_ATTRIBUTES};
    use windows_sys::Wdk::Storage::FileSystem::{NtOpenDirectoryObject, NtQueryDirectoryObject};
    /// Format: DIRECTORY_QUERY, the right to list a directory object.
    const DIRECTORY_QUERY: u32 = 1;
    let mut path: Vec<u16> = "\\Device".encode_utf16().collect();
    let bytes = u16::try_from(path.len() * 2).unwrap();
    let name = windows_sys::Win32::Foundation::UNICODE_STRING {
      Length: bytes,
      MaximumLength: bytes,
      Buffer: path.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
      Length: u32::try_from(std::mem::size_of::<OBJECT_ATTRIBUTES>()).unwrap(),
      ObjectName: &name,
      ..OBJECT_ATTRIBUTES::default()
    };
    let mut directory = std::ptr::null_mut();
    // SAFETY: the attributes and the name they point at live through the call; the handle is closed below.
    let status = unsafe { NtOpenDirectoryObject(&mut directory, DIRECTORY_QUERY, &attributes) };
    assert_eq!(status, 0, "open the \\Device directory");
    let mut entries = BTreeSet::new();
    let mut buffer = vec![0u64; 512];
    let mut context = 0u32;
    loop {
      let mut returned = 0u32;
      // SAFETY: the buffer is owned and its length given in bytes; one entry a call, its strings inside the buffer.
      let status = unsafe {
        NtQueryDirectoryObject(
          directory,
          buffer.as_mut_ptr().cast(),
          u32::try_from(buffer.len() * size_of::<u64>()).unwrap(),
          true,
          false,
          &mut context,
          &mut returned,
        )
      };
      if status != 0 {
        break;
      }
      // SAFETY: a successful query wrote one entry at the buffer's start, eight-byte aligned, its name's buffer within
      // ours and its length in bytes.
      let name = unsafe {
        let entry = &*buffer.as_ptr().cast::<DirectoryEntry>();
        std::slice::from_raw_parts(entry.name.Buffer, usize::from(entry.name.Length) / 2)
      };
      entries.insert(String::from_utf16_lossy(name));
    }
    // SAFETY: the directory handle opened above, closed once.
    unsafe { NtClose(directory) };
    entries
  }

  fn never() -> bool {
    false
  }

  /// A-219. Do: run the receive-worker probe on a WinFsp volume of its own. Expect: every worker count measured in
  /// every round, each round's median a real time, a derived count between one and the most, printed with the rounds,
  /// and the probe's volume gone once it returns.
  #[test]
  fn the_probe_measures_every_count_in_every_round_and_derives_one_of_them() {
    if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
      eprintln!("SKIP: the receive-worker probe needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
      return;
    }
    let (_sender, mut intents) = crate::purge_intents::channel(1).unwrap();
    let api = ffi::api().unwrap();
    let volume = ProbeVolume::new(api).unwrap();
    let entry = volume.device_entry();
    assert!(
      device_entries().contains(&entry),
      "the probe's volume {entry}"
    );
    let (count, medians) = measure_on(&volume, &volume.probe_path(), &mut intents[0], &never)
      .unwrap()
      .unwrap();
    drop(volume);
    eprintln!("WINFSP PROBE: count {count} from medians {medians:?} ns");
    assert_eq!(medians.len(), capture::MAX_WORKERS);
    for rounds in &medians {
      assert_eq!(rounds.len(), ROUNDS);
      assert!(rounds.iter().all(|median| *median > 0));
    }
    assert!((1..=capture::MAX_WORKERS).contains(&count));
    assert!(
      !device_entries().contains(&entry),
      "{entry} outlived its probe"
    );
  }

  /// A-219. Do: run the probe on its own volume with a client path no volume answers, so its first open fails. Expect:
  /// the probe refuses, its workers end at their idle return, and its volume is gone once it drops.
  #[test]
  fn a_probe_whose_open_fails_ends_its_workers_and_leaves_no_volume() {
    if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
      eprintln!("SKIP: the receive-worker probe needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
      return;
    }
    let (_sender, mut intents) = crate::purge_intents::channel(1).unwrap();
    let volume = ProbeVolume::new(ffi::api().unwrap()).unwrap();
    let entry = volume.device_entry();
    let nowhere: Vec<u16> = "\\\\?\\GLOBALROOT\\Device\\slates-no-such-device\\file"
      .encode_utf16()
      .chain(Some(0))
      .collect();
    let refused = measure_on(&volume, &nowhere, &mut intents[0], &never);
    assert!(
      matches!(refused, Err(WinFspError::Os { .. })),
      "the probe's open must be refused, got {:?}",
      refused.map(|measured| measured.map(|(count, _)| count))
    );
    drop(volume);
    assert!(
      !device_entries().contains(&entry),
      "{entry} outlived its failed probe"
    );
  }

  /// A-219. Do: start the probe with its worker already told to stop. Expect: it ends at its first open, well within one
  /// transaction timeout (no worker waits one out), and its volume is gone once it drops.
  #[test]
  fn a_stopped_probe_ends_at_its_first_open_and_leaves_no_volume() {
    if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
      eprintln!("SKIP: the receive-worker probe needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
      return;
    }
    let (_sender, mut intents) = crate::purge_intents::channel(1).unwrap();
    let volume = ProbeVolume::new(ffi::api().unwrap()).unwrap();
    let entry = volume.device_entry();
    let began = Instant::now();
    let stopped = measure_on(&volume, &volume.probe_path(), &mut intents[0], &|| true).unwrap();
    let took = began.elapsed();
    drop(volume);
    eprintln!("WINFSP PROBE: a stopped probe ended in {took:?}");
    assert!(stopped.is_none());
    assert!(
      took < Duration::from_millis(u64::from(crate::dispatcher::TRANSACT_TIMEOUT_MS)),
      "a stopped probe took {took:?}"
    );
    assert!(
      !device_entries().contains(&entry),
      "{entry} outlived its stopped probe"
    );
  }
}
