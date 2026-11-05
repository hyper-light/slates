//! How many receive workers a mount runs (A-219), measured from this machine once a process, never named.
//!
//! A second worker pays only when it is already waiting in the kernel as the first sends its response, so the next
//! request is handed to a waiting thread rather than to one still on its way back into the call. Whether that wins is
//! the machine's: the open-cost harness measured two waiting threads 8-10 us an IRP faster on CI's arm64 runner and
//! about 7 us an IRP slower on its x86_64 one (audit step 7, BENCHMARKS "What a successful open costs under each serving
//! pattern"). So the probe measures that hand-off itself. It makes a hidden WinFsp volume with no drive and no file
//! system behind it, which no other process knows the name of. It serves the volume with one waiting worker, then with
//! two, in interleaved rounds, and times one client opening an absent name through the volume's device path. The probe
//! serves through the receive worker's own calls ([`crate::dispatcher::receive_batch`],
//! [`crate::dispatcher::send_response`]) and its own journal. Each request is answered at once, so the rounds differ
//! only in how many workers wait. More workers are taken only where every round with more beats every round with
//! fewer ([`crate::inflight::capture::workers_beyond_spread`]). The rounds and the count are logged. The probe
//! writes nothing to disk.

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
use crate::inflight::{self, Refusal, capture};
use crate::purge_intents::Intents;

/// Shape: the rounds of each worker count, interleaved and rotated, so a drifting load lands on every count alike and the
/// spread of a count is the range of five medians.
const ROUNDS: usize = 5;
/// Shape: the timed opens of a round, enough that one scheduling delay cannot move its median.
const OPENS: usize = 200;
/// Shape: the untimed opens that begin each round, while its fresh threads first wait.
const WARM: usize = 20;
/// Format: the absent name every probe open asks for, under the probe volume's root.
const ABSENT: &str = "\\slates-receive-probe";
/// Format: the Win32 prefix that names a device by its NT path, as WinFsp's own tests open an unmounted volume.
const GLOBALROOT: &str = "\\\\?\\GLOBALROOT";

/// The probe volume's interface: no callback, since the probe answers every request itself and the DLL dispatches none.
static PROBE_INTERFACE: Interface = Interface::EMPTY;

/// The receive workers each mount of this process runs, measured at the first mount. Two first mounts racing may each
/// measure; the first to finish sets the count for the process.
#[cfg(not(test))]
pub(crate) fn receive_workers() -> Result<usize, WinFspError> {
  static WORKERS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
  if let Some(count) = WORKERS.get() {
    return Ok(*count);
  }
  let (count, _medians) = measure()?;
  Ok(*WORKERS.get_or_init(|| count))
}

/// The library's own tests run every mount with the most workers, so the live kill oracles prove the multi-worker
/// recovery on every lane whatever its probe derives (A-219); the probe itself is tested on its own below, and the CLI's
/// mount tests run the derived count.
#[cfg(test)]
pub(crate) fn receive_workers() -> Result<usize, WinFspError> {
  Ok(capture::MAX_WORKERS)
}

/// Measures one waiting worker against each larger count up to [`capture::MAX_WORKERS`] and derives the count, logging
/// both. Returns the count and each round's median open, in nanoseconds, by waiting workers (the first row one worker).
pub(crate) fn measure() -> Result<(usize, Vec<Vec<u64>>), WinFspError> {
  let started = Instant::now();
  let api = ffi::api().map_err(|_| WinFspError::NotInstalled)?;
  crate::host::checked_dll(api)?;
  let volume = ProbeVolume::new(api)?;
  let path = volume.absent_path();
  // The sender stays alive for the probe: a worker's wait drains its intent channel, and a closed one is a failure.
  let (_sender, mut intents) = crate::purge_intents::channel(capture::MAX_WORKERS)?;
  crate::dispatcher::announce_ready(
    volume.control(),
    intents.first_mut().ok_or(WinFspError::MountThread)?,
  )?;
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
      let median = serve_round(&volume, &path, waiting, &mut intents, &mut journals)?;
      medians
        .get_mut(arm)
        .ok_or(WinFspError::Capture(Refusal::Length))?
        .push(median);
    }
  }
  let count = capture::workers_beyond_spread(&medians);
  slates_machine::log_line!(
    "slates-winfsp: receive workers = {count}: median open of each round, in ns, by waiting workers from one: {medians:?}; \
     more workers only where every round beats every round of fewer; probe took {} ms",
    started.elapsed().as_millis()
  );
  Ok((count, medians))
}

/// A hidden WinFsp volume with no mount point, deleted on drop. Only the probe's own threads transact on it.
struct ProbeVolume {
  api: &'static ffi::Api,
  file_system: *mut FileSystem,
  control: HANDLE,
  /// The volume's NT device name, unterminated (`\Device\Volume{...}`).
  name: Vec<u16>,
}

impl ProbeVolume {
  fn new(api: &'static ffi::Api) -> Result<Self, WinFspError> {
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

  /// The absent name's path through the volume's device: `\\?\GLOBALROOT\Device\Volume{...}\slates-receive-probe`.
  fn absent_path(&self) -> Vec<u16> {
    GLOBALROOT
      .encode_utf16()
      .chain(self.name.iter().copied())
      .chain(ABSENT.encode_utf16())
      .chain(Some(0))
      .collect()
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
  /// Set once the round's timed opens are done: a worker ends once it has answered a request received after this.
  stop: AtomicBool,
  /// The workers that have ended or are ending: each counts itself before it sends its last response, so the client
  /// knows, when that open returns, whether another worker still waits for one.
  ended: AtomicUsize,
  /// Set when the round failed with no open outstanding: a worker then ends at any return, its idle timeout too.
  abandoned: AtomicBool,
}

/// One round: `waiting` workers serve while this thread times its opens; then each worker is ended by one more open and
/// joined. Returns the round's median open.
fn serve_round(
  volume: &ProbeVolume,
  path: &[u16],
  waiting: usize,
  intents: &mut [Intents],
  journals: &mut [Vec<u8>],
) -> Result<u64, WinFspError> {
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
    for (intents, journal) in intents.iter_mut().zip(journals.iter_mut()).take(waiting) {
      let ending = &ending;
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
    let timed = spawned.and_then(|()| time_opens(path)).and_then(|median| {
      // No open is outstanding: each returned once its response was taken. Every open from here ends the worker that
      // answers it, and one is made only while a worker has not counted itself ending, so some worker receives it.
      ending.stop.store(true, Ordering::Release);
      while ending.ended.load(Ordering::Acquire) < threads.len() {
        open_absent(path)?;
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

/// One waiting worker: receive into its journal, answer each request at once (a Create "not found", anything else
/// cancelled), send each response alone, until it has answered a request after the round's stop.
fn serve_waiting(
  control: usize,
  journal: &mut [u8],
  intents: &mut Intents,
  ending: &Ending,
) -> Result<(), WinFspError> {
  let control = control as HANDLE;
  let not_found = crate::ntstatus(&slates_vfs::VfsError::NotFound).as_i32();
  // What this worker's last batch filled; its first receive clears the whole buffer.
  let mut used = None;
  loop {
    let mut receiving = capture::Capture::new(&mut *journal)
      .and_then(|capture| capture.begin(used))
      .map_err(WinFspError::Capture)?;
    let reported = crate::dispatcher::receive_batch(control, receiving.buffer_mut(), intents)?;
    if ending.abandoned.load(Ordering::Acquire) {
      return Ok(());
    }
    let stopping = ending.stop.load(Ordering::Acquire);
    used = Some(capture::filled(reported).map_err(WinFspError::Capture)?);
    let mut batch = receiving
      .received(reported, crate::dispatcher::performance_counter()?)
      .map_err(WinFspError::Capture)?;
    let mut answered = false;
    while let Some(request) = batch.next_request().map_err(WinFspError::Capture)? {
      let mut response = request.cancelled_response().map_err(WinFspError::Capture)?;
      if request.kind() == inflight::CREATE_KIND {
        inflight::finish_response(&mut response, not_found).map_err(WinFspError::Capture)?;
      }
      batch
        .record_response(&request, &response)
        .map_err(WinFspError::Capture)?;
      if stopping && !answered {
        ending.ended.fetch_add(1, Ordering::Release);
      }
      answered = true;
      crate::dispatcher::send_response(control, &response, intents)?;
      batch
        .acknowledge(request.hint())
        .map_err(WinFspError::Capture)?;
    }
    if stopping && answered {
      return Ok(());
    }
  }
}

/// Opens the absent name WARM and then OPENS times, and returns the timed opens' median.
fn time_opens(path: &[u16]) -> Result<u64, WinFspError> {
  let mut times = Vec::with_capacity(OPENS);
  for open in 0..WARM.saturating_add(OPENS) {
    let took = open_absent(path)?;
    if open >= WARM {
      times.push(took);
    }
  }
  times.sort_unstable();
  times
    .get(times.len() / 2)
    .copied()
    .ok_or(WinFspError::Capture(Refusal::Length))
}

/// One open of the absent name, which must be answered "not found"; returns its time in nanoseconds. Any other answer is
/// a refusal: the probe would be timing something else.
fn open_absent(path: &[u16]) -> Result<u64, WinFspError> {
  let began = Instant::now();
  // SAFETY: a NUL-terminated path; no security attributes or template; a returned handle is closed below.
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
  let took = began.elapsed();
  if handle != INVALID_HANDLE_VALUE {
    // SAFETY: the handle just opened, closed once.
    unsafe { CloseHandle(handle) };
    return Err(WinFspError::Capture(Refusal::State));
  }
  let code = std::io::Error::last_os_error().raw_os_error();
  if code != i32::try_from(ERROR_FILE_NOT_FOUND).ok() {
    return Err(WinFspError::Os {
      call: "open the receive probe's absent name",
      code,
    });
  }
  Ok(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  use super::*;

  /// A-219. Do: run the receive-worker probe on a WinFsp volume of its own. Expect: every worker count measured in
  /// every round, each round's median a real time, and a derived count between one and the most, printed with the rounds.
  #[test]
  fn the_probe_measures_every_count_in_every_round_and_derives_one_of_them() {
    if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
      eprintln!("SKIP: the receive-worker probe needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
      return;
    }
    let (count, medians) = measure().unwrap();
    eprintln!("WINFSP PROBE: count {count} from medians {medians:?} ns");
    assert_eq!(medians.len(), capture::MAX_WORKERS);
    for rounds in &medians {
      assert_eq!(rounds.len(), ROUNDS);
      assert!(rounds.iter().all(|median| *median > 0));
    }
    assert!((1..=capture::MAX_WORKERS).contains(&count));
  }
}
