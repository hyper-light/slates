//! The WinFsp receive workers (A-198, A-215, A-219). The public batch FSCTL uses direct output: the kernel writes each
//! request into anchor-held pages before putting it in processing. Each worker owns a journal in those pages. A batch's
//! last response rides the call that receives the worker's next batch into the journal's other buffer, as the DLL's own
//! dispatcher sends it (A-215); every other response is sent alone. After a death, every journal settles before any
//! worker receives, and each address's newest delivery alone is answered, so a reused IRP address never names another
//! processing request (A-219). WinFsp v2.1 sys/volume.c, sys/ioq.c and dll/fs.c define this order; the model oracles in
//! `inflight::capture` and the live kill oracles check it.

use std::cell::{Cell, RefCell};
use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, HANDLE};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Memory::{
  FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};

use crate::ffi::FileSystem;
use crate::host::WinFspError;
use crate::inflight::{self, RESPONSE_BASE, RESPONSE_MAX, Refusal, Request, capture::Capture};

/// Format: CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 0x800 + 't', METHOD_OUT_DIRECT, FILE_ANY_ACCESS), fsctl.h.
const TRANSACT_BATCH: u32 = (9 << 16) | ((0x800 + b't' as u32) << 2) | 2;
/// Format: the public single transaction code, sent with both lengths zero to announce the ready receiver. WinFsp's
/// own StartDispatcher sends this Transact0 before exposing service (FSP_CFG_REJECT_EARLY_IRP, dll/fs.c).
const TRANSACT_READY: u32 = (9 << 16) | ((0x800 + b'T' as u32) << 2);
/// Derived: the shortest transaction wait the public API accepts (FspFsctlTransactTimeoutMinimum, fsctl.h).
/// An idle receive returns at this interval, allowing the owner to join without stopping the held volume.
pub(crate) const TRANSACT_TIMEOUT_MS: u32 = 1000;
/// Shape: the stop channel carries no messages; dropping its sender is the worker's cancellation signal.
const STOP_BOUND: usize = 0;
/// Shape: worker 0 releases each other worker once, after recovery; a release waits for nobody.
const RELEASE_BOUND: usize = 1;
/// Format: the status offset in the checked response header (fsctl.h).
const STATUS_AT: usize = 20;
/// Format: the status occupies the final word of the common response header.
const STATUS_END: usize = STATUS_AT + size_of::<i32>();

thread_local! {
  /// Only a marker-authenticated Create may resolve the pending coherence file by identity.
  static PRIVATE_PURGE: RefCell<Option<crate::purge::Purge>> = const { RefCell::new(None) };
  /// The opaque kernel hint of the current callback, copied into a newly recorded open before its response exists.
  static CURRENT_HINT: Cell<u64> = const { Cell::new(0) };
  /// Void Cleanup/Close callbacks still carry their typed refusal into this operation's kernel response.
  static TERMINAL_REFUSAL: Cell<i32> = const { Cell::new(0) };
}

pub(crate) fn private_path(path: &str) -> Result<Option<crate::purge::PrivatePath>, i32> {
  PRIVATE_PURGE.with_borrow(|purge| match purge {
    None => Ok(None),
    Some(purge) => crate::purge::private_path(purge, path)
      .map(Some)
      .ok_or_else(|| crate::ntstatus(&slates_vfs::VfsError::NotPermitted).as_i32()),
  })
}

/// The command thread brackets one purge at a time. Its bounded channel carries the old End and next Begin; the
/// receiver drains it before each callback and while control I/O waits: a refused drive open may produce no callback.
pub(crate) struct PrivateOpens {
  marker: crate::purge_token::Marker,
  intents: crate::purge_intents::Intents,
}

impl PrivateOpens {
  pub(crate) fn new(
    marker: crate::purge_token::Marker,
    intents: crate::purge_intents::Intents,
  ) -> Self {
    Self { marker, intents }
  }

  fn authenticate(
    &mut self,
    request: &Request<'_>,
  ) -> Result<Option<crate::purge::Purge>, WinFspError> {
    self.intents.progress()?;
    if request.kind() != inflight::CREATE_KIND
      || request.process_id().map_err(WinFspError::Capture)? != std::process::id()
    {
      return Ok(None);
    }
    let value = usize::try_from(request.access_token().map_err(WinFspError::Capture)?)
      .map_err(|_| WinFspError::Capture(Refusal::Length))?;
    if value == 0 {
      return Err(WinFspError::Capture(Refusal::State));
    }
    // SAFETY: this fresh Create's token handle was opened into this receiver's process by WinFsp. The driver keeps
    // it live until this original request is answered; recovery never queries a predecessor's token.
    let token = unsafe { BorrowedHandle::borrow_raw(value as *mut _) };
    if !self.marker.matches(token)? {
      return Ok(None);
    }
    self
      .intents
      .current()
      .cloned()
      .map(Some)
      .ok_or(WinFspError::Capture(Refusal::State))
  }
}

pub(crate) fn current_hint() -> u64 {
  CURRENT_HINT.get()
}

pub(crate) fn record_terminal_refusal(status: i32) {
  TERMINAL_REFUSAL.set(status);
}

pub(crate) fn announce_ready(
  control: HANDLE,
  intents: &mut crate::purge_intents::Intents,
) -> Result<(), WinFspError> {
  transaction(control, TRANSACT_READY, &[], &mut [], intents).map(|_| ())
}

/// The receive workers (A-219), each on its own thread with its own journal. Their owner closes every stop channel and joins
/// every thread before freeing the DLL object. Worker 0 recovers every journal before any worker receives and serves at
/// once, so the mount is answered without waiting for the probe. Each further worker first runs the receive-worker probe
/// (`crate::probe`) and serves, after worker 0's recovery, only where the count reaches it; otherwise it ends.
pub(crate) struct Worker {
  stops: Vec<SyncSender<()>>,
  threads: Vec<JoinHandle<Result<(), WinFspError>>>,
}

impl Worker {
  /// Starts one worker for each of `privates`, at most [`inflight::capture::MAX_WORKERS`].
  pub(crate) fn start(
    file_system: usize,
    section: OwnedHandle,
    bound: usize,
    outcomes: std::sync::mpsc::SyncSender<crate::host::Outcome>,
    wake: std::task::Waker,
    confirmations: SyncSender<crate::host::Request>,
    privates: Vec<PrivateOpens>,
  ) -> Result<Self, WinFspError> {
    if privates.is_empty() || privates.len() > inflight::capture::MAX_WORKERS {
      return Err(WinFspError::Capture(Refusal::Length));
    }
    let mut worker = Self {
      stops: Vec::with_capacity(privates.len()),
      threads: Vec::with_capacity(privates.len()),
    };
    let mut releases = Vec::new();
    let mut waits = Vec::new();
    for _ in 1..privates.len() {
      let (release, wait) = sync_channel(RELEASE_BOUND);
      releases.push(release);
      waits.push(wait);
    }
    let mut waits = waits.into_iter();
    let mut releases = Some(releases);
    for (index, private) in privates.into_iter().enumerate() {
      let (stop, stopped) = sync_channel(STOP_BOUND);
      let section = section.try_clone().map_err(|error| WinFspError::Os {
        call: "duplicate the held receive section",
        code: error.raw_os_error(),
      })?;
      let start = if index == 0 {
        Start::Recover(releases.take().unwrap_or_default())
      } else {
        Start::Probe(waits.next().ok_or(WinFspError::MountThread)?)
      };
      let (outcomes, wake, confirmations) = (outcomes.clone(), wake.clone(), confirmations.clone());
      let thread = std::thread::Builder::new()
        .name(format!("slates-winfsp-receive-{index}"))
        .spawn(move || {
          if let Err(error) = crate::reply::measure().and_then(|()| {
            receive_loop(
              file_system,
              section,
              bound,
              Receive {
                worker: index,
                start,
                stopped,
                confirmations: &confirmations,
                wake: &wake,
                outcomes: &outcomes,
                private,
              },
            )
          }) {
            slates_machine::log_line!("slates-winfsp: receive worker {index} ended: {error}");
            if outcomes.send(crate::host::Outcome::Failed(error)).is_err() {
              slates_machine::log_line!(
                "slates-winfsp: receive worker's owner ended before its failure was reported"
              );
            }
            wake.wake_by_ref();
          }
          Ok(())
        })
        .map_err(|_| WinFspError::MountThread)?;
      worker.stops.push(stop);
      worker.threads.push(thread);
    }
    Ok(worker)
  }

  pub(crate) fn join(&mut self) -> Result<(), WinFspError> {
    self.stops.clear();
    let mut outcome = Ok(());
    for thread in self.threads.drain(..) {
      if let Err(error) = thread
        .join()
        .map_err(|_| WinFspError::MountThread)
        .and_then(|joined| joined)
      {
        outcome = Err(error);
      }
    }
    outcome
  }
}

/// How a worker begins: worker 0 recovers every journal and then releases the others; each other worker probes, and if
/// the count reaches it waits for that release (or for worker 0's end, which drops it) before its first receive (A-219).
enum Start {
  Recover(Vec<SyncSender<()>>),
  Probe(Receiver<()>),
}

/// A view of the held section. Only the receive suffix is borrowed: the shard independently owns the disjoint open
/// frames before it. The old process has exited before this view is adopted; no live worker shares this region.
struct CaptureView {
  address: usize,
  bound: usize,
  _section: OwnedHandle,
}

impl CaptureView {
  fn new(section: OwnedHandle, bound: usize) -> Result<Self, WinFspError> {
    let length = crate::opens::bytes(bound).map_err(WinFspError::Ledger)?;
    // SAFETY: this owned section was created/adopted with exactly the checked ledger length. The view is released
    // only after the synchronous/owned overlapped transactions using its suffix have completed.
    let view = unsafe {
      MapViewOfFile(
        section.as_raw_handle(),
        FILE_MAP_READ | FILE_MAP_WRITE,
        0,
        0,
        length,
      )
    };
    if view.Value.is_null() {
      return Err(os_error("map the held receive buffer"));
    }
    Ok(Self {
      address: view.Value.addr(),
      bound,
      _section: section,
    })
  }

  /// Worker `worker`'s journal (A-219). Each worker lends only its own region; recovery, on one thread before any worker
  /// receives, lends each region in turn.
  fn capture(&mut self, worker: usize) -> Result<Capture<'_>, WinFspError> {
    let offset = crate::opens::journal_offset(self.bound, worker).map_err(WinFspError::Ledger)?;
    let address = self
      .address
      .checked_add(offset)
      .ok_or(WinFspError::Capture(Refusal::Length))?;
    // SAFETY: new checked the whole section and suffix offset. Only this worker touches the suffix, and the mutable
    // borrow ends before any other capture borrow. No Rust slice covers the shard-owned prefix.
    let bytes =
      unsafe { std::slice::from_raw_parts_mut(address as *mut u8, inflight::capture::BYTES) };
    Capture::new(bytes).map_err(WinFspError::Capture)
  }
}

impl Drop for CaptureView {
  fn drop(&mut self) {
    // SAFETY: address is this object's sole live MapViewOfFile view, and all transactions using it have ended.
    if unsafe {
      UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
        Value: self.address as *mut _,
      })
    } == 0
    {
      slates_machine::log_line!(
        "slates-winfsp: {}",
        os_error("unmap the held receive buffer")
      );
    }
  }
}

/// One Win32 transaction, including completion if the direct request pends. All borrowed buffers and the worker's completion event remain
/// owned until GetOverlappedResult settles it. The finite transaction wait lets an idle receiver return and observe
/// cancellation without stopping the held volume. The mount owner joins before deleting its own file system.
fn transaction(
  control: HANDLE,
  code: u32,
  input: &[u8],
  output: &mut [u8],
  intents: &mut crate::purge_intents::Intents,
) -> Result<usize, WinFspError> {
  let mut overlapped = OVERLAPPED {
    hEvent: intents.completion_event(),
    ..OVERLAPPED::default()
  };
  let mut done = 0;
  let input_len = u32::try_from(input.len()).map_err(|_| WinFspError::Capture(Refusal::Length))?;
  let output_len =
    u32::try_from(output.len()).map_err(|_| WinFspError::Capture(Refusal::Length))?;
  // SAFETY: all buffers and overlapped storage remain alive and unmoved until the transaction has completed below;
  // the control belongs to the live DLL object, which cannot be deleted before this worker is joined.
  let accepted = unsafe {
    DeviceIoControl(
      control,
      code,
      if input.is_empty() {
        std::ptr::null()
      } else {
        input.as_ptr().cast()
      },
      input_len,
      if output.is_empty() {
        std::ptr::null_mut()
      } else {
        output.as_mut_ptr().cast()
      },
      output_len,
      &mut done,
      &mut overlapped,
    )
  };
  if accepted == 0 {
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != i32::try_from(ERROR_IO_PENDING).ok() {
      return Err(WinFspError::Os {
        call: "WinFsp control transaction",
        code: error.raw_os_error(),
      });
    }
    let progress = intents.wait();
    // SAFETY: this is the same outstanding overlapped I/O, with all its buffers and event still owned. Waiting settles
    // the request before any return can free them; an idle receive has the volume's finite transaction wait.
    if unsafe { GetOverlappedResult(control, &overlapped, &mut done, 1) } == 0 {
      return Err(os_error("complete WinFsp control transaction"));
    }
    // Even a failed event wait or channel drain cannot return before the control I/O has settled its borrowed pages.
    progress?;
  }
  intents.progress()?;
  usize::try_from(done).map_err(|_| WinFspError::Capture(Refusal::Length))
}

/// Receives a batch of requests into `output`, a worker's direct-output journal buffer. The call waits for a request
/// up to the volume's transaction timeout and returns zero bytes at it.
pub(crate) fn receive_batch(
  control: HANDLE,
  output: &mut [u8],
  intents: &mut crate::purge_intents::Intents,
) -> Result<usize, WinFspError> {
  transaction(control, TRANSACT_BATCH, &[], output, intents)
}

/// Sends a batch's last response and receives the next batch into `output`, in one call, as the DLL's dispatcher does
/// (A-215, WinFsp v2.1 `src/dll/fs.c:288`): the driver finishes the response before it waits for a request.
pub(crate) fn send_and_receive(
  control: HANDLE,
  response: &[u8],
  output: &mut [u8],
  intents: &mut crate::purge_intents::Intents,
) -> Result<usize, WinFspError> {
  transaction(control, TRANSACT_BATCH, response, output, intents)
}

/// Sends one response alone, as every response of a batch but its last is sent.
pub(crate) fn send_response(
  control: HANDLE,
  response: &[u8],
  intents: &mut crate::purge_intents::Intents,
) -> Result<(), WinFspError> {
  transaction(control, TRANSACT_BATCH, response, &mut [], intents).map(|_| ())
}

/// Aligned working copies, as required by WinFsp's native request/response structs. The immutable captured original
/// is never passed to a callback that may adjust request fields.
#[repr(align(8))]
struct Packet([u8; RESPONSE_MAX]);

/// What the receive worker keeps across requests: its private-open state and the two packets every operation uses,
/// made once for the worker's life. Zeroing two fresh packets a request cost 2.6-4.4 us of each IRP on arm64 and
/// 1.7-2.0 us on x86_64 (diagnostics run 42091919319); WinFsp's own dispatcher clears nothing (dll/fs.c:303-305).
struct Owned {
  private: PrivateOpens,
  working: Box<Packet>,
  response: Box<Packet>,
}

impl Owned {
  fn new(private: PrivateOpens) -> Self {
    Self {
      private,
      working: Box::new(Packet([0; RESPONSE_MAX])),
      response: Box::new(Packet([0; RESPONSE_MAX])),
    }
  }
}

/// One DLL operation on the worker's reused packets. The request is copied whole into the working packet, and the
/// callbacks read nothing past its own size. Only the response's fixed part is cleared, as the header the callback
/// fills: a callback writes whatever it returns past that part and sets the size over it, and
/// [`inflight::finish_response`] clears the padding up to the aligned length, so no byte of an earlier response is sent.
fn operation(
  file_system: *mut FileSystem,
  request: &Request<'_>,
  owned: &mut Owned,
) -> Result<usize, WinFspError> {
  let Owned {
    private,
    working,
    response: packet,
  } = owned;
  working
    .0
    .get_mut(..request.bytes().len())
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .copy_from_slice(request.bytes());
  let header = request.cancelled_response().map_err(WinFspError::Capture)?;
  packet
    .0
    .get_mut(..RESPONSE_BASE)
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .copy_from_slice(&header);
  let index = usize::try_from(request.kind()).map_err(|_| WinFspError::Capture(Refusal::Kind))?;
  // SAFETY: the owner created and ABI-checked this DLL object and keeps it alive until this worker joins. Each worker
  // calls the table on its own packets, as the DLL's own dispatcher threads do. WinFsp's guard (dll/fsop.c,
  // FspFileSystemOpEnter) orders only a file system's namespace work; each of these callbacks is one message to the
  // shard, which serves one at a time (A-219).
  let callback = unsafe { (*file_system).operations.get(index).copied().flatten() };
  let Some(callback) = callback else {
    return Ok(RESPONSE_BASE);
  };
  let purge = match private.authenticate(request) {
    Ok(purge) => purge,
    Err(error) => {
      slates_machine::log_line!("slates-winfsp: private open refused: {error}");
      packet
        .0
        .get_mut(STATUS_AT..STATUS_END)
        .ok_or(WinFspError::Capture(Refusal::Length))?
        .copy_from_slice(
          &crate::ntstatus(&slates_vfs::VfsError::NotPermitted)
            .as_i32()
            .to_le_bytes(),
        );
      return Ok(RESPONSE_BASE);
    }
  };
  PRIVATE_PURGE.set(purge);
  CURRENT_HINT.set(request.hint());
  // SAFETY: callback is the DLL's initialized operation for this checked kind; both packet addresses have native
  // eight-byte alignment and protocol maximum capacity, and file_system stays live throughout the synchronous call.
  let callback_status = unsafe {
    callback(
      file_system,
      working.0.as_mut_ptr().cast(),
      packet.0.as_mut_ptr().cast(),
    )
  };
  let terminal_status = TERMINAL_REFUSAL.replace(0);
  let status = if callback_status == 0 {
    terminal_status
  } else {
    callback_status
  };
  PRIVATE_PURGE.set(None);
  CURRENT_HINT.set(0);
  inflight::finish_response(&mut packet.0, status).map_err(WinFspError::Capture)
}

/// What a worker's operations reach the shard through: the request channel, its waker, and the ledger's open bound.
struct Shard<'a> {
  confirmations: &'a SyncSender<crate::host::Request>,
  wake: &'a std::task::Waker,
  bound: usize,
}

/// Settles every request of a dead worker's batch before any worker receives: each address a newer delivery in another
/// journal holds (`passed`) is passed over, and every other request answered as A-198 settles one (A-219).
fn settle(
  file_system: *mut FileSystem,
  batch: &mut inflight::capture::Batch<'_>,
  passed: &std::collections::BTreeSet<u64>,
  shard: &Shard<'_>,
  owned: &mut Owned,
) -> Result<u64, WinFspError> {
  // SAFETY: same owner/worker lifetime as operation; volume control remains owned by the DLL object.
  let control = unsafe { (*file_system).volume_handle };
  let mut count = 0u64;
  while let Some(request) = batch.next_request().map_err(WinFspError::Capture)? {
    // A newer delivery at this address sits in another worker's journal, so this request is completed (A-219).
    if passed.contains(&request.hint()) {
      batch
        .pass_over(request.hint())
        .map_err(WinFspError::Capture)?;
      continue;
    }
    settle_recovered(file_system, control, batch, &request, shard, owned)?;
    batch
      .acknowledge(request.hint())
      .map_err(WinFspError::Capture)?;
    count = count.saturating_add(1);
  }
  Ok(count)
}

/// Answers one request a dead worker's journal held: its saved response where that is safe to replay, a Cleanup or Close
/// served again, anything else cancelled.
fn settle_recovered(
  file_system: *mut FileSystem,
  control: HANDLE,
  batch: &mut inflight::capture::Batch<'_>,
  request: &Request<'_>,
  shard: &Shard<'_>,
  owned: &mut Owned,
) -> Result<(), WinFspError> {
  let saved = batch.response_for(request).map_err(WinFspError::Capture)?;
  // Only Create's saved result is needed to preserve a possibly issued context. Read completion can reference
  // an old process mapping (sys/read.c), so all other buffer-bearing operations cancel without dereferencing it.
  if let Some(saved) =
    replayable_response(request, saved, shard.confirmations, shard.wake, shard.bound)?
  {
    return send_response(control, saved, &mut owned.private.intents);
  }
  if matches!(
    request.kind(),
    inflight::CLEANUP_KIND | inflight::CLOSE_KIND
  ) {
    let length = operation(file_system, request, owned)?;
    let response = owned
      .response
      .0
      .get(..length)
      .ok_or(WinFspError::Capture(Refusal::Length))?;
    if saved.is_none() {
      batch
        .record_response(request, response)
        .map_err(WinFspError::Capture)?;
    }
    return send_response(control, response, &mut owned.private.intents);
  }
  let response = request.cancelled_response().map_err(WinFspError::Capture)?;
  if saved.is_none() {
    batch
      .record_response(request, &response)
      .map_err(WinFspError::Capture)?;
  }
  send_response(control, &response, &mut owned.private.intents)
}

/// Saved Create and successful Rename responses refer only to kernel-owned state. Other buffer-bearing responses
/// can refer into the dead process. Durable name updates finish before the saved rename success reaches the kernel.
fn replayable_response<'a>(
  request: &Request<'_>,
  saved: Option<&'a [u8]>,
  confirmations: &SyncSender<crate::host::Request>,
  wake: &std::task::Waker,
  bound: usize,
) -> Result<Option<&'a [u8]>, WinFspError> {
  let Some(saved) = saved else {
    return Ok(None);
  };
  if request.kind() == inflight::CREATE_KIND {
    return Ok(Some(saved));
  }
  if let Some((from, to)) = request
    .successful_rename(saved)
    .map_err(WinFspError::Capture)?
  {
    crate::host::rename_open_names(confirmations, wake, &from, &to, bound)?;
    return Ok(Some(saved));
  }
  Ok(None)
}

/// The receive worker's owned control channels and immutable reporting endpoints.
struct Receive<'a> {
  worker: usize,
  start: Start,
  stopped: Receiver<()>,
  confirmations: &'a SyncSender<crate::host::Request>,
  wake: &'a std::task::Waker,
  outcomes: &'a SyncSender<crate::host::Outcome>,
  private: PrivateOpens,
}

fn receive_loop(
  file_system: usize,
  section: OwnedHandle,
  bound: usize,
  channels: Receive<'_>,
) -> Result<(), WinFspError> {
  let Receive {
    worker,
    start,
    stopped,
    confirmations,
    wake,
    outcomes,
    private,
  } = channels;
  let mut owned = Owned::new(private);
  let file_system = file_system as *mut FileSystem;
  let mut view = CaptureView::new(section, bound)?;
  let shard = Shard {
    confirmations,
    wake,
    bound,
  };
  match start {
    Start::Recover(releases) => {
      if let Some(count) = recover(file_system, &mut view, &shard, &mut owned)? {
        slates_machine::log_line!(
          "slates-winfsp: settled {count} captured requests before receiving"
        );
        outcomes
          .send(crate::host::Outcome::Recovered(count))
          .map_err(|_| WinFspError::MountThread)?;
        wake.wake_by_ref();
      }
      for release in releases {
        release.send(()).map_err(|_| WinFspError::MountThread)?;
      }
    }
    Start::Probe(wait) => {
      // The probe keeps this worker's intent channel drained; a stop ends it at its next open.
      let Some(workers) =
        crate::probe::receive_workers(&mut owned.private.intents, &|| is_stopped(&stopped))?
      else {
        return Ok(());
      };
      // The count stops short of this worker: it ends, and its intent channel closes with it.
      if worker >= workers {
        return Ok(());
      }
      // Worker 0 dropping the release without sending it ended the mount first: nothing to serve.
      if wait.recv().is_err() {
        return Ok(());
      }
      slates_machine::log_line!("slates-winfsp: receive worker {worker} serves, after the probe");
      outcomes
        .send(crate::host::Outcome::Released(worker.saturating_add(1)))
        .map_err(|_| WinFspError::MountThread)?;
      wake.wake_by_ref();
    }
  }
  serve_loop(file_system, &mut view, worker, &stopped, &shard, &mut owned)
}

/// Recovers every worker's journal before any worker receives (A-219). For an address in more than one unsettled batch,
/// only the newest delivery is answered; each batch then settles as A-198 settles one. `None` when no journal held a batch.
fn recover(
  file_system: *mut FileSystem,
  view: &mut CaptureView,
  shard: &Shard<'_>,
  owned: &mut Owned,
) -> Result<Option<u64>, WinFspError> {
  let mut journals = Vec::with_capacity(inflight::capture::MAX_WORKERS);
  let mut any = false;
  for worker in 0..inflight::capture::MAX_WORKERS {
    match view
      .capture(worker)?
      .resume()
      .map_err(WinFspError::Capture)?
    {
      Some(batch) => {
        any = true;
        journals.push((batch.stamp(), batch.hints()));
      }
      None => journals.push((None, Vec::new())),
    }
  }
  if !any {
    return Ok(None);
  }
  let passed = inflight::capture::superseded(&journals).map_err(WinFspError::Capture)?;
  let mut count = 0u64;
  for (worker, passed) in passed.iter().enumerate() {
    if let Some(mut batch) = view
      .capture(worker)?
      .resume()
      .map_err(WinFspError::Capture)?
    {
      let settled = settle(file_system, &mut batch, passed, shard, owned)?;
      count = count.saturating_add(settled);
    }
  }
  Ok(Some(count))
}

/// What a worker does next: receive with no response, or take the batch its last combined call delivered (A-215).
enum Next {
  Receive,
  Take(usize),
}

/// One worker's live serving on its own journal, until told to stop: receive, stamp, then serve the batch, its last
/// response riding the call that receives the next batch into the journal's other buffer (A-215), stamped as it returns.
fn serve_loop(
  file_system: *mut FileSystem,
  view: &mut CaptureView,
  worker: usize,
  stopped: &Receiver<()>,
  shard: &Shard<'_>,
  owned: &mut Owned,
) -> Result<(), WinFspError> {
  // SAFETY: the mount owner cannot delete this DLL object until every receive thread is joined.
  let control = unsafe { (*file_system).volume_handle };
  // What this worker's last batch in each buffer filled; its first receive into each clears all of it.
  let mut fill = inflight::capture::Fill::new();
  let mut next = Next::Receive;
  loop {
    let batch = match next {
      Next::Receive => {
        if is_stopped(stopped) {
          return Ok(());
        }
        let mut receiving = view
          .capture(worker)?
          .begin(&fill)
          .map_err(WinFspError::Capture)?;
        let reported = receive_batch(control, receiving.buffer_mut(), &mut owned.private.intents)?;
        // Keep a batch taken while stopping intact for the successor, without invoking another operation on this owner.
        if is_stopped(stopped) {
          return Ok(());
        }
        receiving
          .received(reported, &mut fill, performance_counter()?)
          .map_err(WinFspError::Capture)?
      }
      Next::Take(reported) => view
        .capture(worker)?
        .handed_off(reported, &mut fill, performance_counter()?)
        .map_err(WinFspError::Capture)?,
    };
    let Some(following) = serve_batch(file_system, control, batch, shard, owned, &mut fill)? else {
      return Ok(());
    };
    // A hand-off returned while stopping is the successor's: it settles whatever the call delivered.
    if matches!(following, Next::Take(_)) && is_stopped(stopped) {
      return Ok(());
    }
    next = following;
  }
}

/// Serves a fresh batch. Every response is saved first; each but the last is sent alone and acknowledged, and the last is
/// handed to the call that receives the next batch, its open confirmed while that call waits (A-215).
fn serve_batch(
  file_system: *mut FileSystem,
  control: HANDLE,
  mut batch: inflight::capture::Batch<'_>,
  shard: &Shard<'_>,
  owned: &mut Owned,
  fill: &mut inflight::capture::Fill,
) -> Result<Option<Next>, WinFspError> {
  while let Some(request) = batch.next_request().map_err(WinFspError::Capture)? {
    let length = operation(file_system, &request, owned)?;
    let response = owned
      .response
      .0
      .get(..length)
      .ok_or(WinFspError::Capture(Refusal::Length))?;
    batch
      .record_response(&request, response)
      .map_err(WinFspError::Capture)?;
    if let Some((from, to)) = request
      .successful_rename(response)
      .map_err(WinFspError::Capture)?
    {
      crate::host::rename_open_names(shard.confirmations, shard.wake, &from, &to, shard.bound)?;
    }
    if batch.is_last() {
      let mut hand_off = batch
        .hand_off(&request, fill)
        .map_err(WinFspError::Capture)?;
      // Once the hand-off is recorded, every death still hands the kernel this context (the call consumed the response,
      // or recovery sends the saved Create again), so its confirmation is posted before the call (A-215).
      let confirming = inflight::opened_context(hand_off.response())
        .map_err(WinFspError::Capture)?
        .map(|(_, context)| crate::host::post_confirm(shard.confirmations, shard.wake, context));
      let (response, buffer) = hand_off.transaction();
      let reported = send_and_receive(control, response, buffer, &mut owned.private.intents)?;
      if let Some(confirming) = confirming {
        confirming.finish()?;
      }
      return Ok(Some(Next::Take(reported)));
    }
    send_response(control, response, &mut owned.private.intents)?;
    if let Some((_, context)) = inflight::opened_context(response).map_err(WinFspError::Capture)? {
      crate::host::confirm_open(shard.confirmations, shard.wake, context)?;
    }
    batch
      .acknowledge(request.hint())
      .map_err(WinFspError::Capture)?;
  }
  Ok(Some(Next::Receive))
}

/// The system-wide performance counter, a receive's delivery stamp (A-219). It is consistent across processors and
/// processes, so a successor orders a dead daemon's journals by it; Windows never reports zero for it.
pub(crate) fn performance_counter() -> Result<u64, WinFspError> {
  let mut count = 0i64;
  // SAFETY: the out-parameter is a live local i64, the counter's own type.
  if unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) } == 0 {
    return Err(os_error("read the performance counter"));
  }
  u64::try_from(count)
    .ok()
    .filter(|stamp| *stamp != 0)
    .ok_or(WinFspError::Capture(Refusal::State))
}

fn is_stopped(stopped: &Receiver<()>) -> bool {
  !matches!(stopped.try_recv(), Err(TryRecvError::Empty))
}

fn os_error(call: &'static str) -> WinFspError {
  WinFspError::Os {
    call,
    code: std::io::Error::last_os_error().raw_os_error(),
  }
}
