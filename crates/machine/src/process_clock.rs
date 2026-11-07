//! Another process's CPU clock, read by the process that supervises it (§2.6 boot step 1, the anchor's start judgment;
//! §4.14 `daemon.alive`). A daemon that has not yet beaten can say nothing for itself, so its anchor judges it from the
//! outside, in the daemon's own CPU time, as the daemon's watchdog judges each shard from inside it (A-228,
//! `slates_server::watchdog`): a start that keeps consuming CPU, however little, is starved, not stuck. Measured on
//! macOS at 144 spinners on 18 CPUs (2026-11-07): starts the anchor killed for missing a fixed wall second had used
//! 16-18 ms of CPU in over a second of wall time, every one still progressing.
//!
//! A reading carries the CPU time the process has run ([`ProcessCpu::spent_ns`]) and a count that grows whenever it
//! runs at all ([`ProcessCpu::progress`]):
//! - Linux: the process's CPU-time clock (`clock_getcpuclockid`, read with `clock_gettime`), the nanoseconds all its
//!   threads have run; the progress is the time itself. Until 2026-11-07 it was `/proc/<pid>/schedstat`, which is the
//!   main thread's alone, so a start whose main thread waited on its shards read as making no progress.
//! - macOS: `proc_pidinfo(PROC_PIDTASKINFO)`'s user plus system time, in Mach absolute time units, turned into
//!   nanoseconds by `mach_timebase_info`; the progress is the raw count.
//! - Windows: `GetProcessTimes`' kernel plus user time, which the scheduler charges a tick at a time (15.6 ms by
//!   default), and `QueryProcessCycleTime`, the cycles the process has run, exact, as the progress [B: Microsoft Learn,
//!   "QueryProcessCycleTime", "GetProcessTimes"].
//! - Elsewhere: `None`; the caller's wall window stands alone.

/// One reading of a process's CPU clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessCpu {
  /// The CPU time the process has run, nanoseconds.
  pub spent_ns: u64,
  /// A count that grows whenever the process runs (its CPU time on Linux, its raw time on macOS, its cycles on
  /// Windows).
  pub progress: u64,
}

/// The CPU clock of process `pid`, or `None` when the OS gives no reading (the process gone, or no query here).
pub fn process_cpu(pid: u32) -> Option<ProcessCpu> {
  platform::process_cpu(pid)
}

#[cfg(target_os = "linux")]
mod platform {
  use super::ProcessCpu;

  /// The process's CPU-time clock (`clock_getcpuclockid`), read with `clock_gettime`: the run time of all its threads,
  /// in nanoseconds. Not `/proc/<pid>/schedstat`, which is the main thread's alone.
  pub(super) fn process_cpu(pid: u32) -> Option<ProcessCpu> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut clock: libc::clockid_t = 0;
    // SAFETY: `clock` is this frame's own value for the call to write; the call returns an error number, checked
    // before it is read.
    if unsafe { libc::clock_getcpuclockid(pid, &raw mut clock) } != 0 {
      return None;
    }
    let mut time = libc::timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    // SAFETY: `time` is this frame's own record of the C layout the call fills; its status is checked before it is
    // read.
    if unsafe { libc::clock_gettime(clock, &raw mut time) } != 0 {
      return None;
    }
    let ran = u64::try_from(time.tv_sec)
      .ok()?
      .checked_mul(NS_PER_S)?
      .checked_add(u64::try_from(time.tv_nsec).ok()?)?;
    Some(ProcessCpu {
      spent_ns: ran,
      progress: ran,
    })
  }

  /// Format: nanoseconds a second.
  const NS_PER_S: u64 = 1_000_000_000;
}

#[cfg(target_os = "macos")]
mod platform {
  use super::ProcessCpu;

  /// `mach_timebase_info_data_t`: Mach absolute time is `numer / denom` nanoseconds a unit.
  #[repr(C)]
  struct Timebase {
    numer: u32,
    denom: u32,
  }

  // libc marks its binding deprecated in favour of the mach2 crate; one declaration here keeps the dependency set
  // small, as `facts.rs` does for `mach_host_self` (the symbol is in libSystem on every macOS we build for).
  unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
  }

  pub(super) fn process_cpu(pid: u32) -> Option<ProcessCpu> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: an all-zero `proc_taskinfo` is a valid value of the C struct (integers only).
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;
    // SAFETY: `info` is this frame's own record of `size` bytes, the flavor's own struct; the call writes at most that
    // many bytes and returns how many it wrote, checked before the record is read.
    let wrote =
      unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
    if wrote != size {
      return None;
    }
    let raw = info.pti_total_user.checked_add(info.pti_total_system)?;
    let mut base = Timebase { numer: 0, denom: 0 };
    // SAFETY: `base` is this frame's own record of the C layout the call fills; its status is checked before it is
    // read.
    let status = unsafe { mach_timebase_info(&raw mut base) };
    if status != 0 || base.denom == 0 {
      return None;
    }
    let spent_ns = u64::try_from(
      u128::from(raw)
        .checked_mul(u128::from(base.numer))?
        .checked_div(u128::from(base.denom))?,
    )
    .ok()?;
    Some(ProcessCpu {
      spent_ns,
      progress: raw,
    })
  }
}

#[cfg(windows)]
mod platform {
  use super::ProcessCpu;
  use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
  use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
  };
  use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;

  /// The opened process, closed when the read ends however it ends.
  struct Opened(HANDLE);

  impl Drop for Opened {
    fn drop(&mut self) {
      // SAFETY: the handle is this value's own, opened by `OpenProcess` and closed only here.
      unsafe { CloseHandle(self.0) };
    }
  }

  /// Format: nanoseconds per `FILETIME` unit (100 ns).
  const NS_PER_FILETIME_UNIT: u64 = 100;

  pub(super) fn process_cpu(pid: u32) -> Option<ProcessCpu> {
    // SAFETY: opens a process by id with the limited query right only; a refusal is a null handle, checked here.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
      return None;
    }
    let process = Opened(process);
    let unset = FILETIME {
      dwLowDateTime: 0,
      dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (unset, unset, unset, unset);
    // SAFETY: a live handle with the limited query right, which `GetProcessTimes` accepts, and four of this frame's own
    // values for the call to write; the status is checked before they are read.
    let timed =
      unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) };
    let mut cycles = 0u64;
    // SAFETY: the same live handle (`QueryProcessCycleTime` accepts the limited right) and this frame's own count; the
    // status is checked before it is read.
    let cycled = unsafe { QueryProcessCycleTime(process.0, &mut cycles) };
    if timed == 0 || cycled == 0 {
      return None;
    }
    let units =
      |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some(ProcessCpu {
      spent_ns: units(kernel)
        .checked_add(units(user))?
        .checked_mul(NS_PER_FILETIME_UNIT)?,
      progress: cycles,
    })
  }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
  use super::ProcessCpu;

  /// No process clock here.
  pub(super) fn process_cpu(_pid: u32) -> Option<ProcessCpu> {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::process_cpu;

  /// Shape: how long the test spins between its two readings: long enough that every platform's clock shows it (a
  /// Windows tick is about 15.6 ms), short beside the suite.
  const SPIN: std::time::Duration = std::time::Duration::from_millis(50);

  /// §2.6, the anchor's start judgment, by use: do read this process's CPU clock, spin, and read it again. Expect a
  /// reading on every platform that has one (Linux, macOS, Windows), whose progress rose and whose time did not go
  /// back.
  #[test]
  fn a_process_that_runs_shows_its_cpu_rise() {
    let pid = std::process::id();
    let before = process_cpu(pid);
    let has_clock = cfg!(any(target_os = "linux", target_os = "macos", windows));
    if !has_clock {
      eprintln!("skipped: no process clock on this platform");
      return;
    }
    let started = std::time::Instant::now();
    let mut spin = 0u64;
    while started.elapsed() < SPIN {
      spin = std::hint::black_box(spin.wrapping_add(1));
    }
    let after = process_cpu(pid);
    assert!(
      matches!((before, after), (Some(b), Some(a)) if a.progress > b.progress && a.spent_ns >= b.spent_ns),
      "{before:?} → {after:?}"
    );
  }

  /// Format: the variable that turns this test binary into the off-main-thread spinner
  /// ([`spinner_off_the_main_thread`]).
  const SPINNER: &str = "SLATES_PROCESS_CLOCK_SPINNER";

  /// Shape: how long the re-invoked spinner spins: twenty of [`SPIN`], one second, the anchor's liveness window; the
  /// parent reads it inside that.
  const SPINNER_SPANS: u32 = 20;

  /// The re-invoked child of [`a_process_whose_work_runs_off_its_main_thread_shows_its_cpu_rise`]: libtest runs it on a
  /// thread of its own while the binary's main thread waits for it, so all the process's work runs off its main
  /// thread. Ignored like the anchor crate's supervised child, and run by the parent with `--ignored`.
  #[test]
  #[ignore = "the off-main-thread spinner; run by its parent with --ignored"]
  fn spinner_off_the_main_thread() {
    if std::env::var_os(SPINNER).is_none() {
      return;
    }
    let started = std::time::Instant::now();
    let mut spin = 0u64;
    while started.elapsed() < SPIN * SPINNER_SPANS {
      spin = std::hint::black_box(spin.wrapping_add(1));
    }
  }

  /// §2.6, the anchor's start judgment: a daemon's work runs on its shard and watchdog threads while its main thread
  /// may wait on them, and its progress is the whole process's. Do: start a child of this binary whose main thread
  /// waits while another thread spins ([`spinner_off_the_main_thread`]), let it settle a span, and read its clock a span
  /// apart. Expect its progress to rise. On Linux the clock was `/proc/<pid>/schedstat`, which is the main thread's
  /// alone (`task->se.sum_exec_runtime`), so a start whose main thread waited a window on its shards' recovery read as
  /// making no progress.
  #[test]
  #[allow(clippy::disallowed_methods)] // the spans the child is watched across
  fn a_process_whose_work_runs_off_its_main_thread_shows_its_cpu_rise() {
    if !cfg!(any(target_os = "linux", target_os = "macos", windows)) {
      eprintln!("skipped: no process clock on this platform");
      return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
      .args([
        "--ignored",
        "--exact",
        "process_clock::tests::spinner_off_the_main_thread",
      ])
      .env(SPINNER, "1")
      .stdout(std::process::Stdio::null())
      .spawn()
      .unwrap();
    let pid = child.id();
    std::thread::sleep(SPIN * 2);
    let before = process_cpu(pid);
    std::thread::sleep(SPIN * 2);
    let after = process_cpu(pid);
    let _ = child.kill();
    let _ = child.wait();
    assert!(
      matches!((before, after), (Some(b), Some(a)) if a.progress > b.progress && a.spent_ns > b.spent_ns),
      "{before:?} → {after:?}"
    );
  }

  /// Shape: the spans a stopped child's clock gets to come to rest in: twenty of [`SPIN`], one second, the anchor's
  /// liveness window, inside which the start judgment needs it at rest.
  #[cfg(unix)]
  const SETTLE_SPANS: u32 = 20;

  /// By use on Unix: a stopped child's (`SIGSTOP`) clock comes to rest: within [`SETTLE_SPANS`], a whole span passes
  /// with no progress. Read only once the child is stopped (`waitpid` with `WUNTRACED`): the child takes the stop
  /// itself, running, after `kill` returns (a reading in between counted that run, ubuntu-24.04-arm, diag-liveness-10),
  /// and on macOS the stop's own accounting can land after `waitpid` reports it (0 then 13.75 µs, this Mac under load).
  /// Expect two equal readings a span apart.
  #[cfg(unix)]
  #[test]
  #[allow(clippy::disallowed_methods)] // the spans the stopped child is watched across
  fn a_stopped_process_comes_to_rest() {
    use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
    let mut child = std::process::Command::new("sleep")
      .arg("5")
      .spawn()
      .unwrap();
    let pid = child.id();
    let raw = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    kill_process(raw, Signal::STOP).unwrap();
    let stopped = waitpid(Some(raw), WaitOptions::UNTRACED).unwrap();
    assert!(
      matches!(stopped, Some((_, status)) if status.stopped()),
      "{stopped:?}"
    );
    let mut readings = vec![process_cpu(pid)];
    let mut at_rest = false;
    for _ in 0..SETTLE_SPANS {
      std::thread::sleep(SPIN);
      readings.push(process_cpu(pid));
      if let [.., before, after] = readings.as_slice()
        && before.is_some()
        && before == after
      {
        at_rest = true;
        break;
      }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(at_rest, "{readings:?}");
  }
}
