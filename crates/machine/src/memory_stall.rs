//! The host's memory stall, read at a look (§4.14 `daemon.alive`, A-336): whether the machine's tasks have been held
//! waiting on memory. A shard whose thread waits on a page-in, a reclaim or a refault is neither running nor runnable,
//! as a deadlocked one is, so its CPU clock and the scheduler's state alone call it wedged; on a host short of memory a
//! lone daemon then ended its own process (exit 4, fleet test binaries at 0.2 to 1 GB free on a 15-CPU Mac,
//! 2026-11-17), and an anchor killed a live daemon whose whole process paged. The liveness judgments (the watchdog's,
//! the anchor's start and lapse) read this at each look and call a still shard held for memory, starved and not stuck,
//! while the host reports a stall (`slates_server::watchdog`).
//!
//! One reading per platform, each the host's own classification, so no threshold is chosen here (R3):
//! - **Linux**: `/proc/pressure/memory`'s `some` line, its `total`, the microseconds in which at least one task was
//!   delayed on memory (reclaim, a refault of a page evicted from the working set, a swap-in of one), cumulative since
//!   boot [B: kernel `Documentation/accounting/psi.rst`]. A stall between two looks is the total risen; the kernel
//!   collects every CPU's time as the file is read (`psi_show` calls `collect_percpu_times`), so the total is current
//!   at the read [A: kernel `kernel/sched/psi.c` at 6f88f6cbb, 2026-10-16]. `some`, not `full`: FULL holds on a CPU only
//!   while no task there is productive (`tFULL[cpu] = time(nr_delayed_tasks[cpu] && !nr_productive_tasks[cpu])`), so a
//!   thread delayed on memory on a CPU that runs anything else never shows in it, and on a busy machine, the normal
//!   case, every CPU runs something else. A delayed thread is itself `some` on its CPU, so the total rises whenever it
//!   is delayed, by its CPU's share of the machine's non-idle time. Absent where the kernel was built without PSI or
//!   booted with `psi=0` (the read is refused, `EOPNOTSUPP`).
//! - **macOS**: `kern.memorystatus_vm_pressure_level`, XNU's dispatch level: `DISPATCH_MEMORYPRESSURE_NORMAL` (1),
//!   `WARN` (2) or `CRITICAL` (4), the system-wide condition libdispatch's memory-pressure sources report [B: XNU
//!   `bsd/kern/kern_memorystatus_notify.c` `sysctl_memorystatus_vm_pressure_level` (xnu-12377.121.6), the SDK's
//!   `dispatch/source.h`]. Anything but normal is a stall: XNU raises the level from its own measure of the pages it can
//!   still hand out. Readable without privilege on macOS (the handler checks `PRIV_VM_PRESSURE` only off macOS).
//! - **Windows**: a low-memory resource notification (`CreateMemoryResourceNotification(LowMemoryResourceNotification)`,
//!   made once a process and kept for its life, which the system closes at its end), queried at each look
//!   (`QueryMemoryResourceNotification`): "available physical memory is running low", a system-wide condition [B:
//!   Microsoft Learn, "CreateMemoryResourceNotification"].
//! - **Miri, another platform, or a source that cannot be read**: `None`, and the judgments stand as they were, a still
//!   shard never runnable judged wedged.
//!
//! The level is a state at the read, the total a count over time; [`stalled_between`] turns either into "the host
//! reported a memory stall since the last look", the one question the judgments ask.

/// One reading of the host's memory stall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallReading {
  /// Linux: the microseconds in which at least one task was delayed on memory, cumulative since boot (PSI's memory
  /// `some` total). A stall between two readings is the total risen.
  Cumulative {
    /// The `some` line's `total`, microseconds.
    stalled_us: u64,
  },
  /// macOS and Windows: whether the host classes itself under memory pressure at the read.
  Level {
    /// Above normal: XNU's warning or critical level, or Windows' low-memory notification signaled.
    pressed: bool,
  },
}

/// The host's memory stall now, or `None` where the platform reads none (the module's note names each source).
pub fn read() -> Option<StallReading> {
  platform::read()
}

/// Where this platform reads the host's memory stall, in a log line's words.
pub fn source() -> &'static str {
  platform::SOURCE
}

/// Whether the host reported a memory stall between the reading `before` and the reading `now` (A-336): a level pressed
/// at `now`, or a cumulative total risen past `before`'s. A cumulative reading with no earlier one of its kind says
/// nothing, since the stall it counts may lie anywhere before it; and no reading now says nothing at all.
pub fn stalled_between(before: Option<StallReading>, now: Option<StallReading>) -> bool {
  match (before, now) {
    (_, Some(StallReading::Level { pressed })) => pressed,
    (
      Some(StallReading::Cumulative { stalled_us: then }),
      Some(StallReading::Cumulative { stalled_us }),
    ) => stalled_us > then,
    (_, Some(StallReading::Cumulative { .. }) | None) => false,
  }
}

/// The `total` of the `some` line of a `/proc/pressure/*` file, microseconds: `None` for a text with no such line or a
/// total that is not a number.
#[cfg(any(all(target_os = "linux", not(miri)), test))]
fn some_total(text: &str) -> Option<u64> {
  text
    .lines()
    .find_map(|line| line.strip_prefix("some "))?
    .split_ascii_whitespace()
    .find_map(|field| field.strip_prefix("total="))?
    .parse()
    .ok()
}

#[cfg(all(target_os = "linux", not(miri)))]
mod platform {
  use super::StallReading;

  /// Where the stall is read.
  pub(super) const SOURCE: &str = "/proc/pressure/memory's some total, rising";

  /// PSI's memory `some` total, read from its pseudo-file (a query of the kernel, never a disk read).
  pub(super) fn read() -> Option<StallReading> {
    let text = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    super::some_total(&text).map(|stalled_us| StallReading::Cumulative { stalled_us })
  }
}

#[cfg(all(target_os = "macos", not(miri)))]
mod platform {
  use super::StallReading;

  /// Where the stall is read.
  pub(super) const SOURCE: &str = "kern.memorystatus_vm_pressure_level, above normal";

  /// Format: XNU's dispatch level for a system whose memory pressure is normal (`DISPATCH_MEMORYPRESSURE_NORMAL`,
  /// `NOTE_MEMORYSTATUS_PRESSURE_NORMAL`), the value `kern.memorystatus_vm_pressure_level` answers when it is.
  const NORMAL: u64 = 1;

  /// XNU's memory-pressure level: anything but normal is a stall.
  pub(super) fn read() -> Option<StallReading> {
    crate::facts::sysctl_u64(c"kern.memorystatus_vm_pressure_level").map(|level| {
      StallReading::Level {
        pressed: level != NORMAL,
      }
    })
  }
}

#[cfg(all(windows, not(miri)))]
mod platform {
  use std::sync::OnceLock;

  use windows_sys::Win32::Foundation::HANDLE;
  use windows_sys::Win32::System::Memory::{
    CreateMemoryResourceNotification, LowMemoryResourceNotification,
    QueryMemoryResourceNotification,
  };

  use super::StallReading;

  /// Where the stall is read.
  pub(super) const SOURCE: &str = "the low-memory resource notification, signaled";

  /// The process's low-memory notification, made at its first read and kept for the process's life (the system closes
  /// it when the process ends): its handle's address, `None` where the system refused one.
  static NOTIFICATION: OnceLock<Option<usize>> = OnceLock::new();

  /// Whether the system's low-memory notification is signaled now.
  pub(super) fn read() -> Option<StallReading> {
    let address = (*NOTIFICATION.get_or_init(|| {
      // SAFETY: takes the notification type by value and returns a handle the process owns, or null on a refusal,
      // checked before it is kept.
      let handle = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) };
      (!handle.is_null()).then_some(handle.expose_provenance())
    }))?;
    let handle: HANDLE = std::ptr::with_exposed_provenance_mut(address);
    let mut signaled: windows_sys::core::BOOL = 0;
    // SAFETY: `handle` is the process's own notification, open for the process's life; `signaled` is this frame's own
    // value for the call to write, read only when the call succeeds.
    let queried = unsafe { QueryMemoryResourceNotification(handle, &raw mut signaled) };
    (queried != 0).then_some(StallReading::Level {
      pressed: signaled != 0,
    })
  }
}

#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
mod platform {
  use super::StallReading;

  /// Where the stall is read: nowhere here.
  pub(super) const SOURCE: &str = "no source on this platform";

  /// No reading here.
  pub(super) fn read() -> Option<StallReading> {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::{StallReading, some_total, stalled_between};

  /// A-336, hostile input. Do: read the `some` total from `/proc/pressure/memory` texts: the kernel's own form, one with
  /// the lines swapped, totals at zero and at `u64::MAX`, and texts cut short, empty, holding only `full`, with a total
  /// that is not a number, negative or past `u64`. Expect: the `some` line's total, or `None`, and no panic.
  #[test]
  fn a_pressure_file_gives_its_some_lines_total() {
    let kernel = "some avg10=0.31 avg60=0.12 avg300=0.04 total=48213\n\
                  full avg10=0.00 avg60=0.00 avg300=0.00 total=1771\n";
    assert_eq!(some_total(kernel), Some(48_213));
    let swapped = "full avg10=0.00 avg60=0.00 avg300=0.00 total=7\nsome avg10=0.00 avg60=0.00 avg300=0.00 total=9\n";
    assert_eq!(
      some_total(swapped),
      Some(9),
      "the some line, wherever it is"
    );
    assert_eq!(
      some_total("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
      Some(0)
    );
    assert_eq!(
      some_total(&format!("some total={}\n", u64::MAX)),
      Some(u64::MAX)
    );
    for bad in [
      "",
      "some",
      "some avg10=0.00",
      "full avg10=0.00 avg60=0.00 avg300=0.00 total=5\n",
      "some total=\n",
      "some total=many\n",
      "some total=-1\n",
      "some total=18446744073709551616\n",
      "something total=5\n",
      "\u{0}",
    ] {
      assert_eq!(some_total(bad), None, "{bad:?}");
    }
  }

  /// A-336. Do: ask whether the host stalled between readings: two cumulative totals rising, equal and falling; a
  /// cumulative total with no earlier reading, or after a level; a level pressed and clear, with or without an earlier
  /// reading; and no reading now. Expect: a rise, or a pressed level, says stalled; nothing else does.
  #[test]
  fn a_stall_between_two_looks_is_a_risen_total_or_a_pressed_level() {
    let total = |stalled_us| Some(StallReading::Cumulative { stalled_us });
    let level = |pressed| Some(StallReading::Level { pressed });
    assert!(stalled_between(total(10), total(11)), "risen");
    assert!(!stalled_between(total(10), total(10)), "unchanged");
    assert!(
      !stalled_between(total(10), total(9)),
      "fallen (never, but no stall)"
    );
    assert!(
      !stalled_between(None, total(10)),
      "a total with nothing before it"
    );
    assert!(
      !stalled_between(level(true), total(10)),
      "a total after a level"
    );
    for before in [None, total(3), level(false), level(true)] {
      assert!(
        stalled_between(before, level(true)),
        "pressed after {before:?}"
      );
      assert!(
        !stalled_between(before, level(false)),
        "clear after {before:?}"
      );
      assert!(
        !stalled_between(before, None),
        "no reading after {before:?}"
      );
    }
  }

  /// A-336, by use. Do: read the host's memory stall twice, a little apart. Expect: a reading on every platform with a
  /// source (Linux with PSI, macOS, Windows), of the platform's kind, and a cumulative total that never falls; Linux
  /// built without PSI, or booted with `psi=0`, says so and passes.
  #[test]
  #[cfg(not(miri))]
  fn the_hosts_memory_stall_reads_on_every_platform_with_a_source() {
    let first = super::read();
    let second = super::read();
    println!(
      "memory stall ({}): {first:?} then {second:?}",
      super::source()
    );
    if cfg!(target_os = "linux") && first.is_none() {
      println!("skipped: this kernel keeps no memory pressure (no PSI, or psi=0)");
      return;
    }
    if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
      assert!(first.is_some() && second.is_some(), "a reading here");
    }
    match (first, second) {
      (
        Some(StallReading::Cumulative { stalled_us: before }),
        Some(StallReading::Cumulative { stalled_us: after }),
      ) => {
        assert!(cfg!(target_os = "linux"), "a total only on Linux");
        assert!(after >= before, "the total fell: {before} then {after}");
      }
      (Some(StallReading::Level { .. }), Some(StallReading::Level { .. })) => {
        assert!(
          cfg!(any(target_os = "macos", windows)),
          "a level only on macOS and Windows"
        );
      }
      (None, None) => assert!(!cfg!(any(target_os = "macos", windows))),
      mixed => panic!("two readings of different kinds: {mixed:?}"),
    }
  }
}
