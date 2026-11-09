//! A shard thread's CPU clock, readable from any thread of the process (§4.14: an observation's budget is counted in
//! the observed shard's own time). A shard records a handle to its own clock as it starts
//! ([`crate::registry::record_cpu_clock`]); an observer reads it ([`crate::registry::shard_cpu`]) to tell a starved
//! shard, which keeps consuming CPU, from a wedged one, which consumes none. Measured 2026-10-03 in a Linux container
//! under 108 CPU burners on 18 cores: shards whose observations timed out were runnable (state R, preempted), not
//! blocked, and the 10 s wall budget read their starvation as a fault
//! (`docs/bugs/2026-10-03-an-observation-read-a-starved-shard-as-wedged.md`).
//!
//! A reading carries the CPU time the thread has run ([`CpuReading::spent_ns`], what the budget is charged) and a count
//! that grows whenever it runs at all ([`CpuReading::progress`], what tells it ran during a window):
//! - Linux: `pthread_getcpuclockid` of the calling thread, read with `clock_gettime` (any thread may read it); the
//!   progress is the time itself.
//! - macOS: the calling thread's Mach port (`pthread_mach_thread_np`), read with `thread_info(THREAD_BASIC_INFO)` (user
//!   plus system time); the progress is the time itself.
//! - Windows: the calling thread's id, opened for each read with query rights only. The time is `GetThreadTimes`'
//!   kernel plus user time, which the scheduler charges a tick at a time (15.6 ms by default), so a thread that ran less
//!   than a tick in a window may show none; the progress is `QueryThreadCycleTime`, the cycles the thread has run,
//!   exact [B: Microsoft Learn, "QueryThreadCycleTime", "GetThreadTimes"].
//! - Miri: no clock; the observation keeps its wall budget there.
//!
//! A CPU clock cannot tell a thread kept off the CPU from one blocked in a call: both stand still. The scheduler can
//! ([`RunReading`], from [`run_readings`]; A-228):
//! - Linux: the state in `/proc/self/task/<tid>/stat` (`R` is on a CPU or waiting for one) and the run-queue wait in
//!   `schedstat`'s second field [A: kernel docs, `Documentation/scheduler/sched-stats`]. The wait alone will not do: the
//!   kernel credits it only when the task reaches a CPU or leaves the queue (`kernel/sched/stats.h`), so a thread kept
//!   off a CPU for 5.7-7.9 s showed growth of 0 (measured 2026-11-08 on CachyOS 7.3 and Fedora 6.19).
//! - macOS: `thread_basic_info`'s `run_state`, whose `TH_STATE_RUNNING` is XNU's `TH_RUN`, "running or on runq". No
//!   per-thread runnable time exists; `ri_runnable_time` is the whole process's, so the wait is `None`.
//! - Windows: `ThreadState` from `NtQuerySystemInformation(SystemProcessInformation)`, linked at run time as Microsoft
//!   directs, one whole-system snapshot (3.1 ms at p50, 5.3 ms at most, measured 2026-11-08) for every thread asked
//!   about, which is why the read takes a batch. It holds no ready-wait time, so the wait is `None`.
//! - Miri, or a source that cannot be read: `None`.
//!
//! The liveness watchdog (`slates_server::watchdog`) calls a shard stuck only after a whole window with no CPU progress,
//! never runnable, and no wait credited; where no wait is kept, it goes by the state at each look. Measured on CI, ten
//! runs a platform (diag-a228-5, 2026-11-08): the starved shard read `Runnable` at all 21-29 looks on Linux, macOS and
//! Windows, and, held off its CPU for 3.0 s on Linux and Windows, its beat gapped at most 111 ms.
//!
//! The handle is a `u64`, nonzero when recorded. On Linux it also carries the thread's id, which `/proc` is named by.

/// One reading of a thread's CPU clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuReading {
  /// The CPU time the thread has run, nanoseconds.
  pub spent_ns: u64,
  /// A count that grows whenever the thread runs (its CPU time on Unix, its cycles on Windows).
  pub progress: u64,
}

/// A thread's scheduler state at one instant, which its CPU clock cannot tell: a thread whose clock barely moved over a
/// window either waited for a CPU (starved) or waited on something else (blocked in a call).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
  /// On a CPU, or ready to run and waiting for one.
  Runnable,
  /// Off the CPU and not ready: waiting in a call, or stopped.
  Blocked,
}

/// What the scheduler says of a thread at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunReading {
  /// Whether the thread is on a CPU or waiting for one, or off them both.
  pub state: RunState,
  /// The time it has waited for a CPU so far, nanoseconds, where the platform keeps one (Linux).
  pub waited_ns: Option<u64>,
}

impl CpuReading {
  /// A reading whose progress is its time (Unix).
  #[cfg(all(unix, not(miri)))]
  fn of_time(spent_ns: u64) -> CpuReading {
    CpuReading {
      spent_ns,
      progress: spent_ns,
    }
  }
}

/// The calling thread's CPU-clock handle, nonzero, or `None` where none can be taken.
#[cfg(all(target_os = "linux", not(miri)))]
pub(crate) fn current() -> Option<u64> {
  let mut clock: libc::clockid_t = 0;
  // SAFETY: `pthread_self` names the calling thread, alive for the call; `clock` is this frame's own value, written
  // by the call, and the status is checked before it is read.
  let status = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut clock) };
  if status != 0 {
    return None;
  }
  // A thread CPU clock id is negative on Linux; carried as its bits, tagged so zero never names one. The thread's id
  // goes above them: `/proc` names a thread by it, and no clock id gives it back without knowing the kernel's encoding.
  let tid = u64::from(
    rustix::thread::gettid()
      .as_raw_nonzero()
      .get()
      .cast_unsigned(),
  );
  Some(u64::from(clock.cast_unsigned()) | (tid << TID_SHIFT) | TAG)
}

/// The CPU clock of the thread `handle` names.
#[cfg(all(target_os = "linux", not(miri)))]
pub(crate) fn read(handle: u64) -> Option<CpuReading> {
  if handle & TAG == 0 {
    return None;
  }
  let clock = u32::try_from(handle & CLOCK_BITS).ok()?.cast_signed();
  let mut reading = libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
  };
  // SAFETY: `reading` is this frame's own value, written by the call; a clock id of a thread that has exited is
  // refused by the kernel (`EINVAL`), which the status check turns into `None`.
  let status = unsafe { libc::clock_gettime(clock, &mut reading) };
  if status != 0 {
    return None;
  }
  nanos(
    u64::try_from(reading.tv_sec).ok()?,
    u64::try_from(reading.tv_nsec).ok()?,
  )
  .map(CpuReading::of_time)
}

/// The calling thread's CPU-clock handle: its Mach port, tagged.
#[cfg(all(target_os = "macos", not(miri)))]
pub(crate) fn current() -> Option<u64> {
  // SAFETY: `pthread_self` names the calling thread, alive for the call; the answer is the thread's Mach port name,
  // valid in this task for the thread's life, and no right is taken.
  let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
  (port != 0).then(|| u64::from(port) | TAG)
}

/// The CPU clock (user and system time) of the thread `handle` names.
#[cfg(all(target_os = "macos", not(miri)))]
pub(crate) fn read(handle: u64) -> Option<CpuReading> {
  /// Format: nanoseconds per microsecond (`time_value_t` counts seconds and microseconds).
  const NS_PER_MICRO: u64 = 1_000;
  /// Format: microseconds per second.
  const MICROS_PER_SECOND: u64 = 1_000_000;
  if handle & TAG == 0 {
    return None;
  }
  let info = basic_info(handle)?;
  let micros = |time: libc::time_value_t| {
    u64::try_from(time.seconds)
      .ok()?
      .checked_mul(MICROS_PER_SECOND)?
      .checked_add(u64::try_from(time.microseconds).ok()?)
  };
  micros(info.user_time)?
    .checked_add(micros(info.system_time)?)?
    .checked_mul(NS_PER_MICRO)
    .map(CpuReading::of_time)
}

/// The Mach basic information of the thread `handle` names: its user and system time and its run state.
#[cfg(all(target_os = "macos", not(miri)))]
fn basic_info(handle: u64) -> Option<libc::thread_basic_info> {
  if handle & TAG == 0 {
    return None;
  }
  let port = libc::mach_port_t::try_from(handle & !TAG).ok()?;
  // SAFETY: an all-zero `thread_basic_info` is a valid value of the C struct (integers and `time_value_t`s).
  let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
  let mut count = libc::THREAD_BASIC_INFO_COUNT;
  // SAFETY: `info` is this frame's own value, `count` words long as `THREAD_BASIC_INFO` requires; a port of a thread
  // that has exited is refused (`KERN_*`), which the status check turns into `None`.
  let status = unsafe {
    libc::thread_info(
      port,
      libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
      (&raw mut info).cast(),
      &mut count,
    )
  };
  (status == libc::KERN_SUCCESS).then_some(info)
}

/// What the scheduler says of each thread `handles` name: `TH_STATE_RUNNING` is on a CPU or waiting in the run queue for
/// one [B: XNU `osfmk/kern/thread.h`, `TH_RUN`]; every other state (waiting, uninterruptible, stopped, halted) is off it.
#[cfg(all(target_os = "macos", not(miri)))]
pub(crate) fn run_readings(handles: &[u64]) -> Vec<Option<RunReading>> {
  handles
    .iter()
    .map(|handle| {
      basic_info(*handle).map(|info| RunReading {
        state: if info.run_state == libc::TH_STATE_RUNNING {
          RunState::Runnable
        } else {
          RunState::Blocked
        },
        waited_ns: None,
      })
    })
    .collect()
}

/// What the scheduler says of each thread `handles` name, from its `/proc` files (the module's note says why both).
#[cfg(all(target_os = "linux", not(miri)))]
pub(crate) fn run_readings(handles: &[u64]) -> Vec<Option<RunReading>> {
  /// A file of this process's thread `tid` under `/proc`.
  fn task_file(tid: u64, name: &str) -> Option<String> {
    // structural: allow: a query of the kernel's pseudo-file for this process's own thread; no host path is named.
    std::fs::read_to_string(format!("/proc/self/task/{tid}/{name}")).ok()
  }

  /// One thread's reading: `None` without its state; its wait is `None` where `schedstat` cannot be read.
  fn reading(handle: u64) -> Option<RunReading> {
    if handle & TAG == 0 {
      return None;
    }
    let tid = (handle & !TAG) >> TID_SHIFT;
    let stat = task_file(tid, "stat")?;
    let schedstat = task_file(tid, "schedstat");
    Some(RunReading {
      state: state_of_stat(&stat)?,
      waited_ns: schedstat.as_deref().and_then(wait_of_schedstat),
    })
  }
  handles.iter().map(|handle| reading(*handle)).collect()
}

/// What the scheduler says of each thread `handles` name, from one snapshot of the system's threads.
#[cfg(all(windows, not(miri)))]
pub(crate) fn run_readings(handles: &[u64]) -> Vec<Option<RunReading>> {
  if handles.is_empty() {
    return Vec::new();
  }
  let Some(snapshot) = system_snapshot() else {
    return vec![None; handles.len()];
  };
  let process = std::process::id();
  handles
    .iter()
    .map(|handle| {
      if handle & TAG == 0 {
        return None;
      }
      let thread = u32::try_from(handle & !TAG).ok()?;
      thread_state_in(&snapshot, process, thread).map(|state| RunReading {
        state: run_state_of_thread_state(state),
        waited_ns: None,
      })
    })
    .collect()
}

/// A snapshot of the system's processes and threads, or `None` where the call has gone or keeps failing.
#[cfg(all(windows, not(miri)))]
fn system_snapshot() -> Option<Vec<u8>> {
  use windows_sys::Win32::Foundation::FARPROC;
  use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

  /// `NtQuerySystemInformation`, whose only documented use is through run-time linking.
  type Query = unsafe extern "system" fn(u32, *mut core::ffi::c_void, u32, *mut u32) -> i32;
  /// Format: `SystemProcessInformation` [B: Microsoft Learn, "NtQuerySystemInformation"].
  const SYSTEM_PROCESS_INFORMATION: u32 = 5;
  /// Format: `STATUS_INFO_LENGTH_MISMATCH`: the buffer was too small, and the size wanted is returned.
  const LENGTH_MISMATCH: i32 = 0xC000_0004_u32.cast_signed();
  /// Shape: tries before giving up on a system whose thread list keeps outgrowing the buffer. The next look tries again.
  const TRIES: usize = 4;
  /// Shape: a quarter over what was asked for, room for threads started between two calls.
  const ROOM_DIVISOR: usize = 4;

  let module_name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
  // SAFETY: `module_name` is NUL-terminated and outlives the call; ntdll is mapped into every Windows process for the
  // process's life, so the module handle stays valid, and the name passed to `GetProcAddress` is NUL-terminated. The
  // returned pointer is `NtQuerySystemInformation`, whose signature `Query` states [B: Microsoft Learn].
  let query = unsafe {
    let module = GetModuleHandleW(module_name.as_ptr());
    if module.is_null() {
      return None;
    }
    let found = GetProcAddress(module, c"NtQuerySystemInformation".as_ptr().cast());
    std::mem::transmute::<FARPROC, Option<Query>>(found)?
  };
  // Byte storage: the global allocator's Windows heap aligns every block to 16 bytes, past what the call's records want.
  let mut buffer: Vec<u8> = Vec::new();
  for _ in 0..TRIES {
    let mut needed = 0u32;
    let length = u32::try_from(buffer.len()).ok()?;
    // SAFETY: `buffer` is `length` bytes of this frame's own for the call to write, and `needed` is this frame's own; a
    // zero length is the documented way to ask the size.
    let status = unsafe {
      query(
        SYSTEM_PROCESS_INFORMATION,
        buffer.as_mut_ptr().cast(),
        length,
        &mut needed,
      )
    };
    if status == 0 {
      return Some(buffer);
    }
    if status != LENGTH_MISMATCH {
      return None;
    }
    let needed = usize::try_from(needed).ok()?;
    buffer.resize(needed.checked_add(needed / ROOM_DIVISOR)?, 0);
  }
  None
}

/// No run state read here, as no clock is.
#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
pub(crate) fn run_readings(handles: &[u64]) -> Vec<Option<RunReading>> {
  vec![None; handles.len()]
}

/// Whether any thread of process `pid` is runnable, on a CPU or waiting for one: what tells a starting process that is
/// starved from one that is stuck, as [`run_readings`] tells a shard's thread (A-228's sibling: the anchor judges a
/// start in the whole process's CPU time, and a start held off its CPUs for a window was killed as stuck, "made no
/// progress for 1014 ms", on CI's Windows runner). `None` where the process is gone or nothing can be read.
/// - Linux: the state in every `/proc/<pid>/task/<tid>/stat`.
/// - macOS: `proc_pidinfo(PROC_PIDTASKALLINFO)`'s `pti_numrunning`, the threads in XNU's `TH_RUN`, running or on the run
///   queue [B: XNU `bsd/kern/proc_info.c`, `fill_taskprocinfo`].
/// - Windows: the process's threads in one system snapshot, their `ThreadState`.
pub fn process_runnable(pid: u32) -> Option<bool> {
  process_runnable_in(pid)
}

#[cfg(all(target_os = "linux", not(miri)))]
fn process_runnable_in(pid: u32) -> Option<bool> {
  // structural: allow: a listing of the kernel's pseudo-directory of another process's threads; no host path is named.
  let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
  let mut read_any = false;
  for task in tasks.flatten() {
    // structural: allow: a query of the kernel's pseudo-file for one thread of that process; no host path is named.
    let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else {
      continue;
    };
    let Some(state) = state_of_stat(&stat) else {
      continue;
    };
    read_any = true;
    if state == RunState::Runnable {
      return Some(true);
    }
  }
  read_any.then_some(false)
}

#[cfg(all(target_os = "macos", not(miri)))]
fn process_runnable_in(pid: u32) -> Option<bool> {
  let pid = libc::c_int::try_from(pid).ok()?;
  // SAFETY: an all-zero `proc_taskallinfo` is a valid value of the C struct (integers and byte arrays only).
  let mut info: libc::proc_taskallinfo = unsafe { std::mem::zeroed() };
  let size = libc::c_int::try_from(size_of::<libc::proc_taskallinfo>()).ok()?;
  // SAFETY: `info` is this frame's own record of `size` bytes, the flavor's own struct; the call writes at most that many
  // bytes and returns how many it wrote, checked before the record is read.
  let wrote = unsafe {
    libc::proc_pidinfo(
      pid,
      libc::PROC_PIDTASKALLINFO,
      0,
      (&raw mut info).cast(),
      size,
    )
  };
  (wrote == size).then_some(info.ptinfo.pti_numrunning > 0)
}

#[cfg(all(windows, not(miri)))]
fn process_runnable_in(pid: u32) -> Option<bool> {
  let snapshot = system_snapshot()?;
  process_thread_states_in(&snapshot, pid).map(|states| {
    states
      .into_iter()
      .any(|state| run_state_of_thread_state(state) == RunState::Runnable)
  })
}

#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
fn process_runnable_in(_pid: u32) -> Option<bool> {
  None
}

/// The state in a thread's `/proc/.../stat`: `R` is on a CPU or waiting for one, every other letter is off them both
/// (sleeping, in the kernel, stopped, dead). `None` for a line with no state. The state follows the last `)`, since the
/// thread's name, in parentheses before it, may hold anything.
#[cfg(any(all(target_os = "linux", not(miri)), test))]
fn state_of_stat(stat: &str) -> Option<RunState> {
  let (_, after_name) = stat.rsplit_once(')')?;
  let letter = after_name.split_ascii_whitespace().next()?;
  Some(if letter == "R" {
    RunState::Runnable
  } else {
    RunState::Blocked
  })
}

/// The run-queue wait in a thread's `schedstat`, its second field, nanoseconds; `None` for a line without one.
#[cfg(any(all(target_os = "linux", not(miri)), test))]
fn wait_of_schedstat(schedstat: &str) -> Option<u64> {
  schedstat.split_ascii_whitespace().nth(1)?.parse().ok()
}

/// Format: the `ThreadState` values on a CPU or bound for one: Ready (1), Running (2), Standby (3), Transition (6, ready
/// but for its stack) and DeferredReady (7) [B: Microsoft Learn, "Thread Object"; Geoff Chappell, "KTHREAD_STATE"].
#[cfg(any(all(windows, not(miri)), test))]
const RUNNABLE_THREAD_STATES: [u32; 5] = [1, 2, 3, 6, 7];

/// The `ThreadState` a Windows snapshot gives, as a run state: every value but the runnable ones, Waiting (5) above all,
/// is off the CPU.
#[cfg(any(all(windows, not(miri)), test))]
fn run_state_of_thread_state(state: u32) -> RunState {
  if RUNNABLE_THREAD_STATES.contains(&state) {
    RunState::Runnable
  } else {
    RunState::Blocked
  }
}

/// Format: the size of a 64-bit `SYSTEM_PROCESS_INFORMATION`, after which its threads follow.
#[cfg(any(all(windows, not(miri)), test))]
const PROCESS_RECORD: usize = 256;
/// Format: where a process record holds its thread count.
#[cfg(any(all(windows, not(miri)), test))]
const THREAD_COUNT_AT: usize = 4;
/// Format: where a process record holds its process id.
#[cfg(any(all(windows, not(miri)), test))]
const PROCESS_ID_AT: usize = 80;
/// Format: the size of a 64-bit `SYSTEM_THREAD_INFORMATION`.
#[cfg(any(all(windows, not(miri)), test))]
const THREAD_RECORD: usize = 80;
/// Format: where a thread record holds its thread id (`ClientId.UniqueThread`).
#[cfg(any(all(windows, not(miri)), test))]
const THREAD_ID_AT: usize = 48;
/// Format: where a thread record holds its `ThreadState`.
#[cfg(any(all(windows, not(miri)), test))]
const THREAD_STATE_AT: usize = 68;

/// The little-endian `u32` at `at` in `bytes`, if it is all there.
#[cfg(any(all(windows, not(miri)), test))]
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
  let bytes = bytes.get(at..at.checked_add(size_of::<u32>())?)?;
  Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Every thread's `ThreadState` of process `process` in a snapshot, walked as [`thread_state_in`] walks it; `None` for a
/// process the snapshot does not hold or a record cut short.
#[cfg(any(all(windows, not(miri)), test))]
fn process_thread_states_in(snapshot: &[u8], process: u32) -> Option<Vec<u32>> {
  let mut offset = 0usize;
  loop {
    let record = snapshot.get(offset..)?;
    let next = usize::try_from(u32_at(record, 0)?).ok()?;
    if u32_at(record, PROCESS_ID_AT)? == process {
      let threads = usize::try_from(u32_at(record, THREAD_COUNT_AT)?).ok()?;
      return (0..threads)
        .map(|index| {
          let at = PROCESS_RECORD.checked_add(index.checked_mul(THREAD_RECORD)?)?;
          u32_at(
            record.get(at..at.checked_add(THREAD_RECORD)?)?,
            THREAD_STATE_AT,
          )
        })
        .collect();
    }
    if next == 0 {
      return None;
    }
    offset = offset.checked_add(next)?;
  }
}

/// The `ThreadState` of thread `thread` of process `process` in a snapshot, walking its records with every offset and
/// count checked against the buffer: a record cut short, or an offset past the end, is `None`. The walk ends, since
/// each step moves forward by a nonzero offset.
#[cfg(any(all(windows, not(miri)), test))]
fn thread_state_in(snapshot: &[u8], process: u32, thread: u32) -> Option<u32> {
  let mut offset = 0usize;
  loop {
    let record = snapshot.get(offset..)?;
    let next = usize::try_from(u32_at(record, 0)?).ok()?;
    if u32_at(record, PROCESS_ID_AT)? == process {
      let threads = usize::try_from(u32_at(record, THREAD_COUNT_AT)?).ok()?;
      for index in 0..threads {
        let at = PROCESS_RECORD.checked_add(index.checked_mul(THREAD_RECORD)?)?;
        let held = record.get(at..at.checked_add(THREAD_RECORD)?)?;
        if u32_at(held, THREAD_ID_AT)? == thread {
          return u32_at(held, THREAD_STATE_AT);
        }
      }
      return None;
    }
    if next == 0 {
      return None;
    }
    offset = offset.checked_add(next)?;
  }
}

/// The calling thread's CPU-clock handle: its thread id, tagged.
#[cfg(all(windows, not(miri)))]
pub(crate) fn current() -> Option<u64> {
  // SAFETY: takes no argument and answers the calling thread's id; no OS thread has id zero.
  let id = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
  (id != 0).then(|| u64::from(id) | TAG)
}

/// The CPU clock of the thread `handle` names: opened by id with query rights for this read only, then closed.
#[cfg(all(windows, not(miri)))]
pub(crate) fn read(handle: u64) -> Option<CpuReading> {
  use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
  use windows_sys::Win32::System::Threading::{
    GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
  };
  use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

  /// The opened thread, closed when the read ends however it ends.
  struct Opened(HANDLE);
  impl Drop for Opened {
    fn drop(&mut self) {
      // SAFETY: the handle is this value's own, opened by `OpenThread` and closed only here.
      unsafe { CloseHandle(self.0) };
    }
  }

  /// Format: nanoseconds per `FILETIME` unit (100 ns).
  const NS_PER_FILETIME_UNIT: u64 = 100;
  if handle & TAG == 0 {
    return None;
  }
  let id = u32::try_from(handle & !TAG).ok()?;
  // SAFETY: opens a thread of this process by id with query rights only; a thread that has exited is refused with a
  // null handle, checked before use.
  let thread = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id) };
  if thread.is_null() {
    return None;
  }
  let thread = Opened(thread);
  let unset = FILETIME {
    dwLowDateTime: 0,
    dwHighDateTime: 0,
  };
  let (mut created, mut exited, mut kernel, mut user) = (unset, unset, unset, unset);
  // SAFETY: a live handle with query rights (`GetThreadTimes` accepts the limited right), and four of this frame's own
  // values for the call to write; the status is checked before they are read.
  let timed =
    unsafe { GetThreadTimes(thread.0, &mut created, &mut exited, &mut kernel, &mut user) };
  let mut cycles = 0u64;
  // SAFETY: the same live handle (`QueryThreadCycleTime` accepts the limited right) and this frame's own count; the
  // status is checked before it is read.
  let cycled = unsafe { QueryThreadCycleTime(thread.0, &mut cycles) };
  if timed == 0 || cycled == 0 {
    return None;
  }
  let units =
    |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
  Some(CpuReading {
    spent_ns: units(kernel)
      .checked_add(units(user))?
      .checked_mul(NS_PER_FILETIME_UNIT)?,
    progress: cycles,
  })
}

/// The calling thread's CPU time, nanoseconds: what it has run, never the time it waited for a core or slept (A-243's
/// snapshot trigger charges a commit's replay work by it). Linux and macOS: `CLOCK_THREAD_CPUTIME_ID`, user plus system
/// time, through rustix. Windows: `GetThreadTimes`' kernel plus user time through the calling thread's pseudo-handle;
/// the scheduler charges it a tick at a time (15.6 ms by default), so one short span reads zero or a whole tick, and a
/// sum over many spans is unbiased [B: Microsoft Learn, "GetThreadTimes"]. `None` where no clock is read (Miri).
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
pub fn calling_thread_cpu_ns() -> Option<u64> {
  let reading = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
  nanos(
    u64::try_from(reading.tv_sec).ok()?,
    u64::try_from(reading.tv_nsec).ok()?,
  )
}

/// The calling thread's CPU time, nanoseconds: see the Linux and macOS form.
#[cfg(all(windows, not(miri)))]
pub fn calling_thread_cpu_ns() -> Option<u64> {
  use windows_sys::Win32::Foundation::FILETIME;
  use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
  /// Format: nanoseconds per `FILETIME` unit (100 ns).
  const NS_PER_FILETIME_UNIT: u64 = 100;
  let unset = FILETIME {
    dwLowDateTime: 0,
    dwHighDateTime: 0,
  };
  let (mut created, mut exited, mut kernel, mut user) = (unset, unset, unset, unset);
  // SAFETY: `GetCurrentThread` is the calling thread's pseudo-handle, valid for the call with every access right and
  // never closed; the four values are this frame's own, written by the call, and the status is checked before they are
  // read.
  let timed = unsafe {
    GetThreadTimes(
      GetCurrentThread(),
      &mut created,
      &mut exited,
      &mut kernel,
      &mut user,
    )
  };
  if timed == 0 {
    return None;
  }
  let units =
    |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
  units(kernel)
    .checked_add(units(user))?
    .checked_mul(NS_PER_FILETIME_UNIT)
}

/// No clock here.
#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
pub fn calling_thread_cpu_ns() -> Option<u64> {
  None
}

/// No clock here.
#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
pub(crate) fn current() -> Option<u64> {
  None
}

/// No clock here.
#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
pub(crate) fn read(_handle: u64) -> Option<CpuReading> {
  None
}

/// Format: where a Linux handle holds the thread's id, above the 32 bits of its clock id.
#[cfg(all(target_os = "linux", not(miri)))]
const TID_SHIFT: u32 = 32;
/// Format: the bits of a Linux handle that hold its clock id.
#[cfg(all(target_os = "linux", not(miri)))]
const CLOCK_BITS: u64 = (1 << TID_SHIFT) - 1;

/// Format: the bit marking a recorded handle, so a zero word is never mistaken for one.
#[cfg(all(any(target_os = "linux", target_os = "macos", windows), not(miri)))]
const TAG: u64 = 1 << 63;

/// Seconds and nanoseconds as nanoseconds.
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
fn nanos(seconds: u64, nanos: u64) -> Option<u64> {
  /// Format: nanoseconds per second.
  const NS_PER_SECOND: u64 = 1_000_000_000;
  seconds.checked_mul(NS_PER_SECOND)?.checked_add(nanos)
}

#[cfg(test)]
mod tests {
  /// §4.14, A-228. Do: read the run state of a thread that spins and of one that waits on a channel, from this thread.
  /// Expect: the spinner `Runnable`, the waiter `Blocked`, on Linux, macOS and Windows; no state under Miri. Linux also
  /// gives the time waited, and macOS and Windows have none to give.
  #[test]
  fn a_threads_run_state_tells_a_spinning_thread_from_a_waiting_one() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    let reading = |handle: u64| super::run_readings(&[handle]).pop().flatten();
    let state = |handle: u64| reading(handle).map(|read| read.state);
    let spin = AtomicBool::new(true);
    let (handles, handed) = mpsc::channel();
    let (release, waited) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
      let spinner = handles.clone();
      let spin = &spin;
      scope.spawn(move || {
        let _ = spinner.send(super::current());
        while spin.load(Ordering::Relaxed) {
          std::hint::spin_loop();
        }
      });
      scope.spawn(move || {
        let _ = handles.send(super::current());
        let _ = waited.recv();
      });
      let first = handed.recv().ok().flatten();
      let second = handed.recv().ok().flatten();
      // Which thread sent first is a race; read both until each has settled into its state.
      let settled = std::time::Instant::now();
      let mut seen = (None, None);
      while settled.elapsed() < std::time::Duration::from_secs(2) {
        seen = (first.and_then(state), second.and_then(state));
        // Miri reads nothing, so there is no state to settle on.
        if cfg!(miri)
          || matches!(
            seen,
            (
              Some(super::RunState::Runnable),
              Some(super::RunState::Blocked)
            ) | (
              Some(super::RunState::Blocked),
              Some(super::RunState::Runnable)
            )
          )
        {
          break;
        }
        std::thread::yield_now();
      }
      // Read while both threads live: a thread on its way out has lost its `/proc` files one at a time.
      let live = first.and_then(reading);
      spin.store(false, Ordering::Relaxed);
      let _ = release.send(());
      if let Some(read) = live {
        assert_eq!(
          read.waited_ns.is_some(),
          cfg!(target_os = "linux"),
          "{read:?}"
        );
      }
      if cfg!(all(
        any(target_os = "linux", target_os = "macos", windows),
        not(miri)
      )) {
        assert!(
          matches!(
            seen,
            (
              Some(super::RunState::Runnable),
              Some(super::RunState::Blocked)
            ) | (
              Some(super::RunState::Blocked),
              Some(super::RunState::Runnable)
            )
          ),
          "{seen:?}"
        );
      } else {
        assert_eq!(seen, (None, None));
      }
    });
  }

  /// A-228, hostile input. Do: read the state from `/proc/.../stat` lines: plain ones, a name holding `)`, spaces and a
  /// decoy state, the states that are off the CPU, and lines cut short or empty. Expect: the state after the last `)`, or
  /// `None` where there is none, and no panic.
  #[test]
  fn a_stat_line_gives_the_state_after_the_last_parenthesis() {
    use super::{RunState, state_of_stat};
    assert_eq!(
      state_of_stat("12 (slates-shard-0) R 1 2 3\n"),
      Some(RunState::Runnable)
    );
    assert_eq!(
      state_of_stat("12 (a) R b) S 1 2"),
      Some(RunState::Blocked),
      "the name may hold `) R`"
    );
    assert_eq!(state_of_stat("12 (x (y)) R 1"), Some(RunState::Runnable));
    for off_cpu in ["S", "D", "T", "t", "Z", "X", "I"] {
      assert_eq!(
        state_of_stat(&format!("12 (x) {off_cpu} 1")),
        Some(RunState::Blocked),
        "{off_cpu}"
      );
    }
    for short in ["", "12", "12 (x", "12 (x)", "12 (x)   \n", "\u{0}"] {
      assert_eq!(state_of_stat(short), None, "{short:?}");
    }
  }

  /// A-228, hostile input. Do: read the wait from `schedstat` lines: a plain one, all zeros (scheduler statistics off),
  /// and lines cut short, empty, or not numbers. Expect: the second field, or `None`, and no panic.
  #[test]
  fn a_schedstat_line_gives_its_second_field() {
    use super::wait_of_schedstat;
    assert_eq!(wait_of_schedstat("100 200 3\n"), Some(200));
    assert_eq!(wait_of_schedstat("0 0 0\n"), Some(0));
    for bad in ["", "100", "a b c", "1 -2 3", "1 99999999999999999999 3"] {
      assert_eq!(wait_of_schedstat(bad), None, "{bad:?}");
    }
  }

  /// A-228. Do: map every Windows thread state. Expect: Ready, Running, Standby, Transition and DeferredReady runnable,
  /// everything else blocked, an unknown state included.
  #[test]
  fn a_windows_thread_state_maps_to_a_run_state() {
    use super::{RunState, run_state_of_thread_state};
    for runnable in [1, 2, 3, 6, 7] {
      assert_eq!(run_state_of_thread_state(runnable), RunState::Runnable);
    }
    for blocked in [0, 4, 5, 8, 9, 99] {
      assert_eq!(run_state_of_thread_state(blocked), RunState::Blocked);
    }
  }

  /// One process of a test snapshot: its id, the offset to the next, its threads as `(id, state)`.
  type Process<'a> = (u32, u32, &'a [(u32, u32)]);

  /// A snapshot of `processes`, in the call's layout.
  fn snapshot(processes: &[Process<'_>]) -> Vec<u8> {
    use super::{
      PROCESS_ID_AT, PROCESS_RECORD, THREAD_COUNT_AT, THREAD_ID_AT, THREAD_RECORD, THREAD_STATE_AT,
    };
    let mut bytes = Vec::new();
    for (id, next, threads) in processes {
      let start = bytes.len();
      bytes.resize(start + PROCESS_RECORD + threads.len() * THREAD_RECORD, 0);
      let put = |bytes: &mut Vec<u8>, at: usize, value: u32| {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
      };
      put(&mut bytes, start, *next);
      put(
        &mut bytes,
        start + THREAD_COUNT_AT,
        u32::try_from(threads.len()).unwrap(),
      );
      put(&mut bytes, start + PROCESS_ID_AT, *id);
      for (index, (thread, state)) in threads.iter().enumerate() {
        let at = start + PROCESS_RECORD + index * THREAD_RECORD;
        put(&mut bytes, at + THREAD_ID_AT, *thread);
        put(&mut bytes, at + THREAD_STATE_AT, *state);
      }
    }
    bytes
  }

  /// A-228. Do: find a thread's state in a snapshot of two processes. Expect: the state of that thread of that process,
  /// and `None` for a thread or process not in it.
  #[test]
  fn a_snapshot_gives_the_state_of_the_thread_asked_for() {
    use super::{PROCESS_RECORD, THREAD_RECORD, thread_state_in};
    let first_size = u32::try_from(PROCESS_RECORD + THREAD_RECORD).unwrap();
    let bytes = snapshot(&[(7, first_size, &[(70, 5)]), (8, 0, &[(80, 1), (81, 5)])]);
    assert_eq!(thread_state_in(&bytes, 7, 70), Some(5));
    assert_eq!(thread_state_in(&bytes, 8, 80), Some(1));
    assert_eq!(thread_state_in(&bytes, 8, 81), Some(5));
    assert_eq!(thread_state_in(&bytes, 8, 70), None, "not that process's");
    assert_eq!(thread_state_in(&bytes, 9, 90), None, "no such process");
  }

  /// A-228, hostile input. Do: read snapshots cut at every length, one whose thread count runs past the buffer, one whose
  /// next offset runs past it, and an empty one. Expect: `None` or the true state, and no panic or endless walk.
  #[test]
  fn a_damaged_snapshot_is_refused_without_a_panic() {
    use super::{PROCESS_RECORD, THREAD_COUNT_AT, thread_state_in};
    let whole = snapshot(&[(7, 0, &[(70, 5)])]);
    for length in 0..whole.len() {
      assert_eq!(
        thread_state_in(&whole[..length], 7, 70),
        None,
        "cut at {length}"
      );
    }
    assert_eq!(thread_state_in(&[], 7, 70), None);
    let mut many_threads = whole.clone();
    many_threads[THREAD_COUNT_AT..THREAD_COUNT_AT + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(thread_state_in(&many_threads, 7, 71), None);
    let far = snapshot(&[(7, u32::MAX, &[(70, 5)])]);
    assert_eq!(thread_state_in(&far, 9, 70), None);
    let just_past = snapshot(&[(7, u32::try_from(PROCESS_RECORD * 2).unwrap(), &[])]);
    assert_eq!(thread_state_in(&just_past, 9, 70), None);
  }

  /// A-228's sibling, the anchor's start judgment, by use. Do: ask whether this test's own process has a runnable thread,
  /// from the thread asking, which is on a CPU as it asks; and ask of a process id no process holds. Expect: runnable
  /// on every platform with a reader (Linux, macOS, Windows), and no answer for the missing process.
  #[test]
  #[cfg(not(miri))]
  fn a_running_process_reads_runnable() {
    let readable = cfg!(any(target_os = "linux", target_os = "macos", windows));
    let ours = super::process_runnable(std::process::id());
    assert_eq!(
      ours,
      readable.then_some(true),
      "this process, running as it asks"
    );
    assert_eq!(
      super::process_runnable(u32::MAX - 1),
      None,
      "no such process"
    );
  }

  /// A-228's sibling, the anchor's start judgment. Do: read every thread's state of each process in a snapshot of two,
  /// and of one not in it, and of snapshots cut at every length. Expect: that process's states in order, and `None` for a
  /// process not in it or a record cut short, without a panic.
  #[test]
  fn a_snapshot_gives_every_thread_state_of_the_process_asked_for() {
    use super::{PROCESS_RECORD, THREAD_RECORD, process_thread_states_in};
    let first_size = u32::try_from(PROCESS_RECORD + THREAD_RECORD).unwrap();
    let bytes = snapshot(&[(7, first_size, &[(70, 5)]), (8, 0, &[(80, 1), (81, 5)])]);
    assert_eq!(process_thread_states_in(&bytes, 7), Some(vec![5]));
    assert_eq!(process_thread_states_in(&bytes, 8), Some(vec![1, 5]));
    assert_eq!(process_thread_states_in(&bytes, 9), None, "no such process");
    let whole = snapshot(&[(7, 0, &[(70, 5), (71, 2)])]);
    for length in 0..whole.len() {
      assert_eq!(
        process_thread_states_in(&whole[..length], 7),
        None,
        "cut at {length}"
      );
    }
  }

  /// §4.14. Do: take the calling thread's clock handle, spin a while, and read it from another thread. Expect: a
  /// reading on every platform with a clock (Linux, macOS, Windows), whose progress grew with the spin and whose time
  /// did not go back.
  #[test]
  fn a_threads_cpu_clock_reads_from_another_thread_and_grows_as_it_runs() {
    if cfg!(miri) {
      eprintln!("skipped: no thread CPU clock under Miri");
      return;
    }
    let handle = super::current();
    assert!(handle.is_some(), "a thread CPU clock on this platform");
    let Some(handle) = handle else { return };
    let before = std::thread::scope(|scope| scope.spawn(|| super::read(handle)).join())
      .ok()
      .flatten();
    let mut spin = 0u64;
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_millis(20) {
      spin = std::hint::black_box(spin.wrapping_add(1));
    }
    let after = std::thread::scope(|scope| scope.spawn(|| super::read(handle)).join())
      .ok()
      .flatten();
    assert!(
      matches!((before, after), (Some(b), Some(a)) if a.progress > b.progress && a.spent_ns >= b.spent_ns),
      "{before:?} → {after:?}"
    );
  }
}
