//! A-198's single WinFsp receiver. The old public batch FSCTL uses direct output: the kernel writes each request
//! into anchor-held pages before putting it in processing. Responses never receive another batch. After a death,
//! captured requests settle before any new receive, preventing a reused IRP address from naming another processing
//! request. WinFsp v2.1 sys/volume.c, sys/ioq.c and dll/fs.c define this order; the live taken-read oracle checks it.

use std::cell::Cell;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, HANDLE};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Memory::{
  FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::CreateEventW;

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
/// Format: the public FileSystem.Version is reserved zero in v2.1; dll/fs.c zeroes the object and never sets it.
/// The header's separate static assertion gives the full object's 792-byte size, not this field's value.
pub(crate) const FILE_SYSTEM_VERSION: u16 = 0;
/// Format: status and size offsets in the checked response header (fsctl.h).
const STATUS_AT: usize = 20;
/// Format: the status occupies the final word of the common response header.
const STATUS_END: usize = STATUS_AT + size_of::<i32>();
/// Format: see STATUS_AT.
const SIZE_AT: usize = 2;
/// Format: the response size is a two-byte field after its version.
const SIZE_END: usize = SIZE_AT + size_of::<u16>();
/// Format: FSP_FSCTL_DEFAULT_ALIGNMENT.
const ALIGNMENT: usize = 8;

thread_local! {
  /// The current Create's requester, decoded before entering the DLL operation. No DLL dispatcher TLS is required.
  static REQUESTER: Cell<u32> = const { Cell::new(0) };
  /// The opaque kernel hint of the current callback, copied into a newly recorded open before its response exists.
  static CURRENT_HINT: Cell<u64> = const { Cell::new(0) };
  /// Void Cleanup/Close callbacks still carry their typed refusal into this operation's kernel response.
  static TERMINAL_REFUSAL: Cell<i32> = const { Cell::new(0) };
}

pub(crate) fn opened_by_this_process() -> bool {
  REQUESTER.get() == std::process::id()
}

pub(crate) fn current_hint() -> u64 {
  CURRENT_HINT.get()
}

pub(crate) fn record_terminal_refusal(status: i32) {
  TERMINAL_REFUSAL.set(status);
}

pub(crate) fn announce_ready(control: HANDLE) -> Result<(), WinFspError> {
  transaction(control, TRANSACT_READY, &[], &mut []).map(|_| ())
}

/// The only receive thread. Its owner closes the stop channel and joins before freeing the DLL object.
pub(crate) struct Worker {
  stop: Option<SyncSender<()>>,
  thread: Option<JoinHandle<Result<(), WinFspError>>>,
}

impl Worker {
  pub(crate) fn start(
    file_system: usize,
    section: OwnedHandle,
    bound: usize,
    outcomes: std::sync::mpsc::SyncSender<crate::host::Outcome>,
    wake: std::task::Waker,
    confirmations: SyncSender<crate::host::Request>,
  ) -> Result<Self, WinFspError> {
    let (stop, stopped) = sync_channel(STOP_BOUND);
    let thread = std::thread::Builder::new()
      .name("slates-winfsp-receive".to_owned())
      .spawn(move || {
        if let Err(error) =
          receive_loop(file_system, section, bound, stopped, &confirmations, &wake)
        {
          eprintln!("slates-winfsp: receive worker ended: {error}");
          if outcomes.send(crate::host::Outcome::Failed(error)).is_err() {
            eprintln!(
              "slates-winfsp: receive worker's owner ended before its failure was reported"
            );
          }
          wake.wake_by_ref();
        }
        Ok(())
      })
      .map_err(|_| WinFspError::MountThread)?;
    Ok(Self {
      stop: Some(stop),
      thread: Some(thread),
    })
  }

  pub(crate) fn join(&mut self) -> Result<(), WinFspError> {
    self.stop = None;
    match self.thread.take() {
      Some(thread) => thread.join().map_err(|_| WinFspError::MountThread)?,
      None => Ok(()),
    }
  }
}

/// A view of the held section. Only the receive suffix is borrowed: the shard independently owns the disjoint open
/// frames before it. The old process has exited before this view is adopted; no live worker shares this region.
struct CaptureView {
  address: usize,
  offset: usize,
  _section: OwnedHandle,
}

impl CaptureView {
  fn new(section: OwnedHandle, bound: usize) -> Result<Self, WinFspError> {
    let length = crate::opens::bytes(bound).map_err(WinFspError::Ledger)?;
    let offset = crate::opens::capture_offset(bound).map_err(WinFspError::Ledger)?;
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
      offset,
      _section: section,
    })
  }

  fn capture(&mut self) -> Result<Capture<'_>, WinFspError> {
    let address = self
      .address
      .checked_add(self.offset)
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
      eprintln!(
        "slates-winfsp: {}",
        os_error("unmap the held receive buffer")
      );
    }
  }
}

/// One Win32 transaction, including completion if the direct request pends. All borrowed buffers and the event remain
/// owned until GetOverlappedResult settles it. The finite transaction wait lets an idle receiver return and observe
/// cancellation without stopping the held volume. The mount owner joins before deleting its own file system.
fn transaction(
  control: HANDLE,
  code: u32,
  input: &[u8],
  output: &mut [u8],
) -> Result<usize, WinFspError> {
  // SAFETY: unnamed manual-reset event; no security descriptor or initial owner is borrowed.
  let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  if event.is_null() {
    return Err(os_error("create transaction completion event"));
  }
  // SAFETY: CreateEventW returned a fresh handle; this value takes its ownership exactly once.
  let event = unsafe { OwnedHandle::from_raw_handle(event) };
  let mut overlapped = OVERLAPPED {
    hEvent: event.as_raw_handle(),
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
    // SAFETY: this is the same outstanding overlapped I/O, with all its buffers and event still owned. Waiting settles
    // the request before any return can free them; an idle receive has the volume's finite transaction wait.
    if unsafe { GetOverlappedResult(control, &overlapped, &mut done, 1) } == 0 {
      return Err(os_error("complete WinFsp control transaction"));
    }
  }
  usize::try_from(done).map_err(|_| WinFspError::Capture(Refusal::Length))
}

/// Aligned working copies, as required by WinFsp's native request/response structs. The immutable captured original
/// is never passed to a callback that may adjust request fields.
#[repr(align(8))]
struct Packet([u8; RESPONSE_MAX]);

fn operation(
  file_system: *mut FileSystem,
  request: &Request<'_>,
  packet: &mut Packet,
) -> Result<usize, WinFspError> {
  let mut working = Packet([0; RESPONSE_MAX]);
  working
    .0
    .get_mut(..request.bytes().len())
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .copy_from_slice(request.bytes());
  packet.0.fill(0);
  let header = request.cancelled_response().map_err(WinFspError::Capture)?;
  packet
    .0
    .get_mut(..RESPONSE_BASE)
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .copy_from_slice(&header);
  let index = usize::try_from(request.kind()).map_err(|_| WinFspError::Capture(Refusal::Kind))?;
  // SAFETY: the owner created and ABI-checked this DLL object and keeps it alive until this worker joins. There is
  // exactly one operation caller, so the operation table needs no DLL guard or lock.
  let callback = unsafe { (*file_system).operations.get(index).copied().flatten() };
  eprintln!("WINFSP CAPTURE: entering kind={} has_callback={}", request.kind(), callback.is_some());
  let Some(callback) = callback else {
    return Ok(RESPONSE_BASE);
  };
  REQUESTER.set(request.process_id().map_err(WinFspError::Capture)?);
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
  eprintln!("WINFSP CAPTURE: kind={} hint={} status={:#x}", request.kind(), request.hint(), status);
  REQUESTER.set(0);
  CURRENT_HINT.set(0);
  packet
    .0
    .get_mut(STATUS_AT..STATUS_END)
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .copy_from_slice(&status.to_le_bytes());
  let size: [u8; 2] = packet
    .0
    .get(SIZE_AT..SIZE_END)
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .try_into()
    .map_err(|_| WinFspError::Capture(Refusal::Length))?;
  let size = usize::from(u16::from_le_bytes(size))
    .checked_next_multiple_of(ALIGNMENT)
    .ok_or(WinFspError::Capture(Refusal::Length))?;
  if !(RESPONSE_BASE..=RESPONSE_MAX).contains(&size) {
    return Err(WinFspError::Capture(Refusal::Length));
  }
  packet
    .0
    .get_mut(SIZE_AT..SIZE_END)
    .ok_or(WinFspError::Capture(Refusal::Length))?
    .copy_from_slice(
      &u16::try_from(size)
        .map_err(|_| WinFspError::Capture(Refusal::Length))?
        .to_le_bytes(),
    );
  Ok(size)
}

fn settle(
  file_system: *mut FileSystem,
  batch: &mut inflight::capture::Batch<'_>,
  recovered: bool,
  confirmations: &SyncSender<crate::host::Request>,
  wake: &std::task::Waker,
) -> Result<u64, WinFspError> {
  // SAFETY: same owner/worker lifetime as operation; volume control remains owned by the DLL object.
  let control = unsafe { (*file_system).volume_handle };
  let mut packet = Packet([0; RESPONSE_MAX]);
  let mut count = 0u64;
  while let Some(request) = batch.next_request().map_err(WinFspError::Capture)? {
    if recovered {
      let saved = batch.response_for(&request).map_err(WinFspError::Capture)?;
      // Only Create's saved result is needed to preserve a possibly issued context. Read completion can reference
      // an old process mapping (sys/read.c), so all other buffer-bearing operations cancel without dereferencing it.
      if request.kind() == inflight::CREATE_KIND
        && let Some(saved) = saved
      {
        transaction(control, TRANSACT_BATCH, saved, &mut [])?;
      } else if matches!(
        request.kind(),
        inflight::CLEANUP_KIND | inflight::CLOSE_KIND
      ) {
        let length = operation(file_system, &request, &mut packet)?;
        let response = packet
          .0
          .get(..length)
          .ok_or(WinFspError::Capture(Refusal::Length))?;
        if saved.is_none() {
          batch
            .record_response(&request, response)
            .map_err(WinFspError::Capture)?;
        }
        transaction(control, TRANSACT_BATCH, response, &mut [])?;
      } else {
        let response = request.cancelled_response().map_err(WinFspError::Capture)?;
        if saved.is_none() {
          batch
            .record_response(&request, &response)
            .map_err(WinFspError::Capture)?;
        }
        transaction(control, TRANSACT_BATCH, &response, &mut [])?;
      }
    } else {
      let length = operation(file_system, &request, &mut packet)?;
      let response = packet
        .0
        .get(..length)
        .ok_or(WinFspError::Capture(Refusal::Length))?;
      batch
        .record_response(&request, response)
        .map_err(WinFspError::Capture)?;
      transaction(control, TRANSACT_BATCH, response, &mut [])?;
      if let Some((_, context)) =
        inflight::opened_context(response).map_err(WinFspError::Capture)?
      {
        crate::host::confirm_open(confirmations, wake, context)?;
      }
    }
    batch
      .acknowledge(request.hint())
      .map_err(WinFspError::Capture)?;
    count = count.saturating_add(1);
  }
  Ok(count)
}

fn receive_loop(
  file_system: usize,
  section: OwnedHandle,
  bound: usize,
  stopped: Receiver<()>,
  confirmations: &SyncSender<crate::host::Request>,
  wake: &std::task::Waker,
) -> Result<(), WinFspError> {
  eprintln!("WINFSP CAPTURE: receiver started");
  let file_system = file_system as *mut FileSystem;
  let mut view = CaptureView::new(section, bound)?;
  if let Some(mut batch) = view.capture()?.resume().map_err(WinFspError::Capture)? {
    let count = settle(file_system, &mut batch, true, confirmations, wake)?;
    eprintln!("slates-winfsp: settled {count} captured requests before receiving");
  }
  // SAFETY: the mount owner cannot delete this DLL object until the receive thread is joined.
  let control = unsafe { (*file_system).volume_handle };
  loop {
    if is_stopped(&stopped) {
      return Ok(());
    }
    let mut receiving = view.capture()?.begin().map_err(WinFspError::Capture)?;
    transaction(control, TRANSACT_BATCH, &[], receiving.buffer_mut())?;
    // Keep a batch taken while stopping intact for the successor, without invoking another operation on this owner.
    if is_stopped(&stopped) {
      return Ok(());
    }
    let mut batch = receiving.received().map_err(WinFspError::Capture)?;
    settle(file_system, &mut batch, false, confirmations, wake)?;
  }
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
