//! The I/O completion port driver (Windows): `PostQueuedCompletionStatus` for kicks and no-ops,
//! `GetQueuedCompletionStatusEx` for the wait [B: Microsoft Learn].
//!
//! **A timed wait ends at its deadline, not the next millisecond (84a).** `GetQueuedCompletionStatusEx` takes whole
//! milliseconds, so a wait for 200 µs lasted a millisecond or more (a 10 ms wait, a 15.6 ms tick). A timed wait instead
//! arms a high-resolution waitable timer (`CreateWaitableTimerExW`, `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) whose wait
//! completion packet (`NtAssociateWaitCompletionPacket`) posts to the port when it fires, and waits on the port untimed,
//! as the epoll driver's timerfd does; Go's runtime does the same since 1.23 (`runtime/netpoll_windows.go`). Where the
//! calls are missing (before Windows 8) or a packet can't be re-armed this time, the wait falls back to whole
//! milliseconds ([`Deadline`]). No privilege is asked.
//!
//! **A blocked wait sits one level above normal (A-361).** Windows raises a thread's priority a level when an event, a
//! semaphore or an address it waits on wakes it, and none when it is dequeued from an I/O completion port, which is how a
//! parked shard waits. Measured on CI's Windows runners (2026-11-18, diag-g234-wake-1, two spinning threads a logical CPU
//! at the job's own BelowNormal level): a thread woken from a completion port stayed at its base priority and ran 29 ms
//! (windows-latest) and 14 ms (windows-11-arm) after its kick at the median, 95–112 ms and 32–48 ms at the p99, as the
//! runtime's own parked shard did, waiting out the spinners' turns of 31.6 ms; a waiter on an event or on
//! WaitOnAddress ran one level up and took 15 µs and 6–7 µs; an event waiter with its boost disabled waited the 29 and
//! 14 ms again; and a completion-port waiter raised one level for its wait alone took 15–17 µs and 6–7 µs. So a wait
//! that may block raises a thread at the normal level to the next for as long as it blocks, and puts it back before it
//! runs anything ([`RaisedForWait`]): the shard is woken as Windows wakes an event's waiter and runs, as before, at its
//! own level. A thread at another level (a test that starves a shard at IDLE) is left as it is, and a zero-timeout
//! harvest, which never blocks, pays nothing. A raise and its restore cost 0.77–0.88 µs a pair quiet and 1.3–5.5 µs
//! loaded, against a parked wake of 6–15 µs. No privilege is asked: a thread may set its own level within its process's
//! class (R10).

use std::ffi::c_void;
use std::time::Instant;

use windows_sys::Win32::Foundation::{
  CloseHandle, FARPROC, GENERIC_ALL, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::SOCKET;
use windows_sys::Win32::System::IO::{
  CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{
  CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread,
  GetThreadPriority, SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_ABOVE_NORMAL,
  THREAD_PRIORITY_NORMAL, TIMER_ALL_ACCESS,
};

use crate::afd::{Afd, Block, READABLE_EVENTS, WRITABLE_EVENTS, base_socket};
use crate::driver::{Completion, Driver, DriverKind, Kick, KickPort, nanos_since};
use crate::error::RtError;
use crate::pipe::{PIPE_KEY, Pipes};
use crate::pipe_state::{PipeRead, PipeWrite};

/// Format: the completion key that marks a kick.
const KICK_KEY: usize = usize::MAX;
/// Format: the completion key that marks a no-op; its user word rides in the overlapped pointer.
const NOP_KEY: usize = usize::MAX - 1;
/// Format: the completion key the AFD readiness device is associated under. Its completions carry a
/// poll block in the overlapped pointer (its head is the block), not a user word in the key.
const AFD_KEY: usize = usize::MAX - 2;

/// Format: the completion key the deadline timer's wait completion packet posts under ([`Deadline`]).
const DEADLINE_KEY: usize = usize::MAX - 4;

/// Shape: entries drained per wait (see the kqueue driver for the reasoning).
const EVENTS_PER_WAIT: usize = 64;

/// A completion port the registry slot owns (§4.3, D-8), closed when the slot retires its
/// registration — after the shard's thread has ended and every foreign kick borrowing it has returned —
/// so a late kick can never post to a handle value the process has since reused. Kept as its exposed
/// address so it is `Send` without an unsafe impl.
#[derive(Debug)]
pub struct Port {
  handle: usize,
}

impl Port {
  /// The port's raw handle value.
  pub(crate) fn raw(&self) -> usize {
    self.handle
  }
}

impl Drop for Port {
  fn drop(&mut self) {
    let handle: HANDLE = std::ptr::with_exposed_provenance_mut(self.handle);
    // SAFETY: the port was created by `prepare` and is owned by this value alone; closed once here.
    unsafe { CloseHandle(handle) };
  }
}

/// The driver. It borrows its port from the registry slot (see [`Port`]) and never closes it.
pub struct IocpDriver {
  port: HANDLE,
  /// The registry's kick over this driver's port.
  kick: KickPort,
  epoch: Instant,
  entries: Vec<OVERLAPPED_ENTRY>,
  /// The AFD readiness device, opened and associated with `port` on the first socket registration
  /// (a shard that only kicks and times out never opens it). Socket readiness is polled through it.
  afd: Option<Afd>,
  /// The pipe ends this driver serves (A-179): their completions come back under `PIPE_KEY`, and their readiness is a
  /// finished operation rather than an AFD poll.
  pipes: Pipes,
  /// The timed wait's deadline (84a, the module doc); none where Windows lacks the calls, so waits take whole
  /// milliseconds there.
  deadline: Option<Deadline>,
}

impl std::fmt::Debug for IocpDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IocpDriver")
      .field("port", &self.port)
      .field("entries", &self.entries.len())
      .finish()
  }
}

/// Creates the port with one concurrent thread (the shard), for the registry slot to own.
pub fn prepare() -> Result<Port, RtError> {
  // SAFETY: creating a fresh port; the result is checked.
  let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
  if port.is_null() {
    return Err(RtError::os("CreateIoCompletionPort"));
  }
  Ok(Port {
    handle: port.expose_provenance(),
  })
}

impl IocpDriver {
  /// Builds the driver over the port the registry slot owns, on the shard's thread. Refused when the
  /// registration no longer holds a port (it was retired).
  pub fn from_prepared(kick: KickPort) -> Result<IocpDriver, RtError> {
    let port = kick.raw().ok_or(RtError::DriverRefused {
      call: "IOCP driver over a retired registration",
      code: None,
    })?;
    // SAFETY: an all-zero OVERLAPPED_ENTRY is a valid, empty record.
    let entries = (0..EVENTS_PER_WAIT)
      .map(|_| unsafe { std::mem::zeroed() })
      .collect();
    Ok(IocpDriver {
      port: std::ptr::with_exposed_provenance_mut(port),
      kick,
      epoch: Instant::now(),
      entries,
      afd: None,
      pipes: Pipes::default(),
      deadline: Deadline::new(),
    })
  }

  /// The AFD readiness device, opened and associated with this port on first use (§4.6). Lazily,
  /// because a driver that never awaits a socket needs no AFD handle.
  fn afd(&mut self) -> Result<&Afd, RtError> {
    if let Some(ref afd) = self.afd {
      return Ok(afd);
    }
    let afd = Afd::open()?;
    // Associate the AFD device with this port so its polls complete here under `AFD_KEY`.
    // SAFETY: both handles are live and ours; `CreateIoCompletionPort` with an existing `port`
    // associates `afd.handle()` with it and returns the port (null on failure).
    let associated = unsafe { CreateIoCompletionPort(afd.handle(), self.port, AFD_KEY, 0) };
    if associated.is_null() {
      return Err(RtError::os("CreateIoCompletionPort(AFD)"));
    }
    // `insert` stores the device and hands back a reference to it — no `expect` on a re-read.
    Ok(self.afd.insert(afd))
  }

  /// Arms a one-shot AFD poll for `events` on the socket `raw` names, waking `user_data` when it
  /// fires. A Windows `SOCKET` fits in a positive `i32` in practice (kernel handle-table values), so
  /// the readiness seam carries it as the same `i32` a Unix fd uses; it is reconstructed here as the
  /// low 32 bits, unsigned. The leaked poll block is owned by the kernel until `wait` reclaims it.
  fn arm(&mut self, raw: i32, events: u32, user_data: u64) -> Result<(), RtError> {
    // The inverse of `netsys::Socket::raw_id`: reinterpret the `i32` as its 32 bits, then widen to the
    // pointer-width `SOCKET` — bit-for-bit the handle the seam narrowed. No sign-losing `as` cast.
    let socket = u32::from_ne_bytes(raw.to_ne_bytes()) as SOCKET;
    let base = base_socket(socket)?;
    let afd = self.afd()?;
    // SAFETY: `afd()` associated the device with this port before returning it, so the poll's
    // completion is delivered here and `wait` reclaims the block exactly once.
    let _block = unsafe { afd.poll(base, events, user_data)? };
    Ok(())
  }
}

impl IocpDriver {
  /// One `GetQueuedCompletionStatusEx` for at most `remaining_ns` (none: without bound): through the deadline timer
  /// where it is armed, so the port's own wait is untimed, else rounded up to whole milliseconds. How many entries it
  /// filled (the deadline's packet among them, [`DEADLINE_KEY`]), or `None` when it timed out.
  fn dequeue(&mut self, remaining_ns: Option<u64>) -> Result<Option<usize>, RtError> {
    /// Format: nanoseconds per millisecond.
    const NANOS_PER_MILLI: u64 = 1_000_000;
    /// Format: INFINITE.
    const INFINITE: u32 = u32::MAX;
    let whole_ms =
      |ns: u64| u32::try_from(ns.div_ceil(NANOS_PER_MILLI)).unwrap_or(INFINITE - 1);
    let timeout_ms = match remaining_ns {
      None => INFINITE,
      Some(0) => 0,
      Some(ns) => match self.deadline.as_ref().map(|d| d.arm(self.port, ns)) {
        Some(Armed::Armed) => INFINITE,
        Some(Armed::Fired) => 0,
        Some(Armed::Busy) | None => whole_ms(ns),
      },
    };
    let mut count: u32 = 0;
    let capacity = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
    // SAFETY: the entry buffer holds `capacity` records and `count` is writable.
    let ok = unsafe {
      GetQueuedCompletionStatusEx(
        self.port,
        self.entries.as_mut_ptr(),
        capacity,
        &raw mut count,
        timeout_ms,
        0,
      )
    };
    if ok == 0 {
      let code = std::io::Error::last_os_error().raw_os_error();
      return if code == Some(i32::try_from(WAIT_TIMEOUT).unwrap_or(0)) {
        Ok(None)
      } else {
        Err(RtError::DriverRefused {
          call: "GetQueuedCompletionStatusEx",
          code,
        })
      };
    }
    Ok(Some(usize::try_from(count).unwrap_or(0)))
  }
}

impl IocpDriver {
  /// Whether the first `count` entries are the deadline's packet alone: a wait that timed out, not one with work.
  fn deadline_alone(&self, count: usize) -> bool {
    self
      .entries
      .iter()
      .take(count)
      .all(|entry| entry.lpCompletionKey == DEADLINE_KEY)
  }
}

/// `NtCreateWaitCompletionPacket`: a wait completion packet, which posts to a port when its target is signaled.
type CreatePacket = unsafe extern "system" fn(*mut HANDLE, u32, *const c_void) -> i32;
/// `NtAssociateWaitCompletionPacket`: the packet bound to a port and a target, its key and status as the completion's.
type AssociatePacket = unsafe extern "system" fn(
  HANDLE,
  HANDLE,
  HANDLE,
  *const c_void,
  *const c_void,
  i32,
  usize,
  *mut u8,
) -> i32;
/// `NtCancelWaitCompletionPacket`: the packet's binding cancelled, and with `TRUE` a packet already posted taken back.
type CancelPacket = unsafe extern "system" fn(HANDLE, u8) -> i32;

/// Format: `STATUS_SUCCESS`.
const STATUS_SUCCESS: i32 = 0;
/// Format: `STATUS_PENDING`: a cancel that can't take effect yet (the target was being signaled).
const STATUS_PENDING: i32 = 0x0000_0103;
/// Format: `STATUS_CANCELLED`: a cancel of a binding whose target had already fired (nothing left to cancel).
const STATUS_CANCELLED: i32 = 0xC000_0120_u32.cast_signed();

/// The timed wait's deadline (84a, the module doc): a high-resolution waitable timer and a wait completion packet bound
/// to the port whenever a timed wait arms it, so the port's wait ends at the deadline's nanoseconds.
struct Deadline {
  timer: HANDLE,
  packet: HANDLE,
  associate: AssociatePacket,
  cancel: CancelPacket,
}

/// What arming the deadline did.
enum Armed {
  /// The timer runs and its packet posts to the port when it fires: wait untimed.
  Armed,
  /// The timer had fired by the time its packet was bound, so no packet comes: harvest without waiting.
  Fired,
  /// The packet couldn't be re-armed this time (its last binding was firing as it was cancelled): wait in whole
  /// milliseconds.
  Busy,
}

impl Deadline {
  /// The timer and its packet, or none where this Windows lacks either (the packet calls are Windows 8's).
  fn new() -> Option<Deadline> {
    let module_name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    // SAFETY: `module_name` is NUL-terminated and outlives the calls; ntdll is mapped into every Windows process for its
    // life; each name passed is NUL-terminated, and each function's signature is the one its type above states
    // (Windows 8+, as Go's runtime declares them in os_windows.go).
    let (create, associate, cancel) = unsafe {
      let module = GetModuleHandleW(module_name.as_ptr());
      if module.is_null() {
        return None;
      }
      let find = |name: &core::ffi::CStr| GetProcAddress(module, name.as_ptr().cast());
      (
        std::mem::transmute::<FARPROC, Option<CreatePacket>>(find(c"NtCreateWaitCompletionPacket"))?,
        std::mem::transmute::<FARPROC, Option<AssociatePacket>>(find(c"NtAssociateWaitCompletionPacket"))?,
        std::mem::transmute::<FARPROC, Option<CancelPacket>>(find(c"NtCancelWaitCompletionPacket"))?,
      )
    };
    // SAFETY: an unnamed timer with default security; the result is checked.
    let timer = unsafe {
      CreateWaitableTimerExW(
        std::ptr::null(),
        std::ptr::null(),
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
        TIMER_ALL_ACCESS,
      )
    };
    if timer.is_null() {
      return None;
    }
    let mut packet: HANDLE = std::ptr::null_mut();
    // SAFETY: `packet` is this frame's own for the call to write; no object attributes.
    if unsafe { create(&raw mut packet, GENERIC_ALL, std::ptr::null()) } != STATUS_SUCCESS
      || packet.is_null()
    {
      // SAFETY: the timer was created above and is owned here alone.
      unsafe { CloseHandle(timer) };
      return None;
    }
    Some(Deadline {
      timer,
      packet,
      associate,
      cancel,
    })
  }

  /// Arms the timer for `ns` from now, its packet bound to `port` under [`DEADLINE_KEY`]. A binding a past wait left
  /// (one a completion ended before its timer fired) is cancelled first, a packet it posted taken back with it.
  fn arm(&self, port: HANDLE, ns: u64) -> Armed {
    /// Format: nanoseconds per 100 ns tick, a waitable timer's unit.
    const NANOS_PER_TICK: u64 = 100;
    // SAFETY: the packet is this deadline's own and open.
    match unsafe { (self.cancel)(self.packet, 1) } {
      STATUS_SUCCESS | STATUS_CANCELLED => {}
      _ => return Armed::Busy,
    }
    // A relative due time is negative; rounded up, so the timer never fires before the deadline.
    let due = i64::try_from(ns.div_ceil(NANOS_PER_TICK))
      .map_or(i64::MIN, i64::wrapping_neg)
      .min(-1);
    // SAFETY: the timer is this deadline's own and open; `due` outlives the call; no completion routine.
    if unsafe { SetWaitableTimer(self.timer, &raw const due, 0, None, std::ptr::null(), 0) } == 0 {
      return Armed::Busy;
    }
    let mut fired: u8 = 0;
    // SAFETY: the packet, the port and the timer are open; the key is a tag the wait reads back, never dereferenced;
    // `fired` is this frame's own.
    let bound = unsafe {
      (self.associate)(
        self.packet,
        port,
        self.timer,
        std::ptr::without_provenance(DEADLINE_KEY),
        std::ptr::null(),
        STATUS_SUCCESS,
        0,
        &raw mut fired,
      )
    };
    match (bound, fired) {
      (STATUS_SUCCESS, 0) => Armed::Armed,
      (STATUS_SUCCESS, _) => Armed::Fired,
      _ => Armed::Busy,
    }
  }
}

impl Drop for Deadline {
  fn drop(&mut self) {
    // SAFETY: both handles are this deadline's own, closed once here, the packet's binding cancelled first.
    unsafe {
      let _ = (self.cancel)(self.packet, 1);
      CloseHandle(self.packet);
      CloseHandle(self.timer);
    }
  }
}

/// Moves the calling thread from priority level `from` to `to` when it stands at `from`: whether it moved.
fn move_level(from: i32, to: i32) -> bool {
  // SAFETY: the calling thread's pseudo-handle, valid for both calls; a thread reads and sets its own level within its
  // process's priority class without any privilege.
  unsafe {
    GetThreadPriority(GetCurrentThread()) == from && SetThreadPriority(GetCurrentThread(), to) != 0
  }
}

/// The calling thread raised one level above normal for a wait that may block, and put back as this drops (A-361, the
/// module doc). Holds nothing when the wait cannot block or the thread was not at the normal level.
struct RaisedForWait(bool);

impl RaisedForWait {
  /// Raises the calling thread for a wait of `timeout_ns` (`None`: without bound) when that wait may block.
  fn raise(timeout_ns: Option<u64>) -> RaisedForWait {
    RaisedForWait(
      timeout_ns != Some(0) && move_level(THREAD_PRIORITY_NORMAL, THREAD_PRIORITY_ABOVE_NORMAL),
    )
  }
}

impl Drop for RaisedForWait {
  fn drop(&mut self) {
    if self.0 {
      // A level something else set during the wait is left as it is.
      let _ = move_level(THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_NORMAL);
    }
  }
}

/// Posts a kick packet to `port`, a live port the caller has pinned ([`KickPort::with`]); safe from
/// any thread. A failed post means the port is being torn down with its shard, the documented outcome
/// of a kick to an exiting shard (`Kick::kick`).
pub fn post_kick(port: usize) {
  let _posted = post(port, KICK_KEY, 0);
}

/// Posts `word` to `port` as a no-op's completion, which the shard's wait delivers as it delivers any word (A-475: a
/// handle wait's callback, on a thread-pool thread, `crate::handle_wait`). Safe from any thread for a live port the
/// caller has pinned ([`KickPort::with`]); whether the packet was posted.
pub(crate) fn post_word(port: usize, word: u64) -> bool {
  post(port, NOP_KEY, usize::try_from(word).unwrap_or(0))
}

/// Posts a packet with completion key `key` and the overlapped tag `tag` (a word the wait reads back as an address, never
/// dereferenced) to `port`, a live port the caller has pinned; whether it was posted.
fn post(port: usize, key: usize, tag: usize) -> bool {
  let handle: HANDLE = std::ptr::with_exposed_provenance_mut(port);
  // SAFETY: the caller pinned the registration that owns this port, so the handle is open; the overlapped pointer is a
  // tag, never dereferenced.
  unsafe { PostQueuedCompletionStatus(handle, 0, key, std::ptr::without_provenance_mut(tag)) != 0 }
}

impl Driver for IocpDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::Iocp
  }

  fn kick_handle(&self) -> Kick {
    Kick::Iocp(self.kick)
  }

  fn now_ns(&self) -> u64 {
    nanos_since(self.epoch)
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    // Windows expires a timed wait on its timer's tick, which can fall before the deadline as the shard's clock (QPC)
    // reads it. Measured on jupiter 2026-10-26: a park for a 5 ms timer woke 2.32 ms and 0.18 ms early, 2 waits in 60
    // (`docs/bugs/2026-10-26-a-windows-park-woke-before-its-deadline.md`). A wait that times out before its deadline
    // waits out the rest, so a park never returns early, as the seam promises. Each further wait asks for at least a
    // millisecond, so the rest shrinks every time; a kick or a completion still ends the wait at once.
    let deadline = timeout_ns.map(|ns| self.now_ns().saturating_add(ns));
    // One level up while the wait may block, back to normal before anything runs (A-361, the module doc); every return
    // below drops it.
    let raised = RaisedForWait::raise(timeout_ns);
    let count = loop {
      let remaining = deadline.map(|due| due.saturating_sub(self.now_ns()));
      match self.dequeue(remaining)? {
        Some(count) if !self.deadline_alone(count) => break count,
        // The deadline's packet alone (or a timeout): the wait is over once the deadline has passed. Before it (a
        // timer that fired early, or a packet a past wait left behind), and on an untimed wait, it waits again.
        _ if deadline.is_some_and(|due| self.now_ns() >= due) => return Ok(()),
        _ => {}
      }
    };
    drop(raised);
    for entry in self.entries.iter().take(count) {
      match entry.lpCompletionKey {
        KICK_KEY | DEADLINE_KEY => {}
        NOP_KEY => out.push(Completion {
          user_data: u64::try_from(entry.lpOverlapped.addr()).unwrap_or(0),
          result: 0,
        }),
        AFD_KEY => {
          // A socket readiness poll fired: the overlapped pointer is the leaked poll block (its head
          // is the `OVERLAPPED`), so reclaim it and wake the word it carried. The fired events are not
          // needed — the readiness future just retries its non-blocking syscall (a spurious wake, e.g.
          // for a since-dropped task, harmlessly wakes nothing).
          if !entry.lpOverlapped.is_null() {
            let block = entry.lpOverlapped.cast::<Block>();
            // SAFETY: `block` is a `Block` leaked by `Afd::poll` (its `OVERLAPPED` head is what was
            // armed) and delivered here exactly once by the port; `reclaim` takes ownership back.
            let user_data = unsafe { Block::reclaim(block) };
            out.push(Completion {
              user_data,
              result: 0,
            });
          }
        }
        PIPE_KEY => {
          // A pipe operation finished: reclaim its block, and wake the waiter its side registered, if its end is still
          // served (a completion for a released end frees its block and wakes no one).
          // SAFETY: completions under `PIPE_KEY` carry a block `pipe::start` leaked, delivered once by the port.
          if let Some(word) = unsafe { self.pipes.complete(entry.lpOverlapped) } {
            out.push(Completion {
              user_data: word,
              result: 0,
            });
          }
        }
        key => out.push(Completion {
          user_data: u64::try_from(key).unwrap_or(0),
          result: i32::try_from(entry.dwNumberOfBytesTransferred).unwrap_or(i32::MAX),
        }),
      }
    }
    Ok(())
  }

  fn has_pending(&self) -> bool {
    false
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    let overlapped = std::ptr::without_provenance_mut(usize::try_from(user_data).unwrap_or(0));
    // SAFETY: the overlapped pointer is a tag the wait reads back as an address, never dereferenced.
    if unsafe { PostQueuedCompletionStatus(self.port, 0, NOP_KEY, overlapped) } == 0 {
      return Err(RtError::os("PostQueuedCompletionStatus"));
    }
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A pipe end's readability is a finished read (A-179): told at once when one is held, else by its completion.
    if self.pipes.holds(raw) {
      if self.pipes.want(raw, false, user_data)? {
        self.submit_nop(user_data)?;
      }
      return Ok(());
    }
    // One-shot AFD poll for the read edges (data, an incoming connection, EOF, an error): it completes
    // on this port when any fires, and `wait` wakes `user_data`. The readiness-native drivers (kqueue,
    // epoll) do this with `EVFILT_READ`/`EPOLLIN`; AFD is the Windows equivalent.
    self.arm(raw, READABLE_EVENTS, user_data)
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A pipe end's writability is its write in the kernel finishing (A-179), or at once when none is.
    if self.pipes.holds(raw) {
      if self.pipes.want(raw, true, user_data)? {
        self.submit_nop(user_data)?;
      }
      return Ok(());
    }
    // One-shot AFD poll for the write edges (send-buffer space, a connect result, an error).
    self.arm(raw, WRITABLE_EVENTS, user_data)
  }

  fn adopt_pipe(&mut self, raw: i32, handle: usize, sizes: (usize, usize)) -> Result<(), RtError> {
    self.pipes.adopt(self.port, (raw, handle), sizes)
  }

  fn release_pipe(&mut self, raw: i32) {
    self.pipes.release(raw);
  }

  fn pipe_read(&mut self, raw: i32, out: &mut [u8]) -> Result<PipeRead, RtError> {
    self.pipes.read(raw, out)
  }

  fn pipe_write(&mut self, raw: i32, bytes: &[u8]) -> Result<PipeWrite, RtError> {
    self.pipes.write(raw, bytes)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 84a: the deadline is made on this Windows, and a sub-millisecond arm posts its packet to the port, ending an
  /// untimed wait before a millisecond. Each step's status is said, so a refusal names its call.
  #[test]
  #[cfg_attr(miri, ignore)]
  fn a_deadline_armed_for_200_us_posts_its_packet_before_a_millisecond() {
    /// Shape: a wait well under a millisecond, as the driver test's.
    const WAIT_NS: u64 = 200_000;
    let port = prepare().unwrap();
    let handle: HANDLE = std::ptr::with_exposed_provenance_mut(port.raw());
    let deadline = Deadline::new();
    eprintln!("deadline made: {}", deadline.is_some());
    let deadline = deadline.unwrap();
    // SAFETY: the deadline's packet is open.
    let first_cancel = unsafe { (deadline.cancel)(deadline.packet, 1) };
    eprintln!("first cancel: {first_cancel:#x}");
    let started = Instant::now();
    let armed = deadline.arm(handle, WAIT_NS);
    eprintln!(
      "armed: {}",
      match armed {
        Armed::Armed => "armed",
        Armed::Fired => "fired",
        Armed::Busy => "busy",
      }
    );
    // SAFETY: an all-zero OVERLAPPED_ENTRY is a valid, empty record.
    let mut entries: [OVERLAPPED_ENTRY; 4] = unsafe { std::mem::zeroed() };
    let mut count = 0u32;
    // SAFETY: `entries` holds 4 records and `count` is writable; the port is open.
    let ok = unsafe {
      GetQueuedCompletionStatusEx(handle, entries.as_mut_ptr(), 4, &raw mut count, 1_000, 0)
    };
    let took = started.elapsed();
    let keys: Vec<usize> = entries.iter().take(count as usize).map(|e| e.lpCompletionKey).collect();
    eprintln!("dequeued ok={ok} count={count} keys={keys:?} took={took:?}");
    assert!(matches!(armed, Armed::Armed), "the deadline didn't arm");
    assert!(keys.contains(&DEADLINE_KEY), "no deadline packet came: {keys:?}");
    assert!(took.as_nanos() < 1_000_000, "the deadline's packet took {took:?}");
  }
}
