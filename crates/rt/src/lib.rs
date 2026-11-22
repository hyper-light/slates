//! `slates-rt` — the runtime: a thread-per-core executor with arena task slots and `Copy` waker
//! encodings, an intrusive run queue, a hierarchical timing wheel, per-shard inbound rings with
//! cross-shard wake kicks, the OS drivers behind one completion seam, and a deterministic
//! simulation driver (design §4.3, D-7, D-9).
//!
//! The doctrine, in one paragraph. A task belongs to one shard for its whole life; nothing is
//! work-stolen and nothing is reference counted. A `Waker` is a `RawWaker` whose data pointer is
//! the packed `shard:16 | slot:24 | generation:24` word of the task's handle, so `clone` and
//! `drop` are no-ops and `wake` from the owning shard pushes the slot onto the local run queue,
//! `wake` from another shard pushes the word onto that pair's single-producer ring and kicks the
//! target's driver, and `wake` from a foreign thread goes through the target's multi-producer ring
//! (embassy-executor's model on std [C: embassy-executor src/raw; B: `RawWakerVTable` docs]). A
//! stale generation is ignored. Timers live in a hierarchical timing wheel [A: Varghese & Lauck,
//! SOSP'87] whose tick is derived from the measured wake cost. Every operation is cancel-safe by
//! construction: resources live in arenas keyed by handle, so dropping a future releases nothing
//! it did not own; a parent's completion cancels and joins its children (hecate's task-lifecycle
//! law). The drivers (io_uring or epoll, kqueue, IOCP) share one seam: block until a kick, a
//! completion or a deadline; the simulation driver replaces time and the kick with seeded,
//! single-threaded stand-ins so a whole cluster runs deterministically in one process (D-20).
//!
//! `LocalWaker` is still a nightly-only API on Rust 1.98 (`local_waker`, #118959), so the
//! executor uses `Waker` with a vtable that is thread-safe by construction; the `Copy` encoding
//! costs nothing to clone, which is what `LocalWaker` would have saved (Phase 0 task 6).
//!
//! Modules: [`error`], [`control`], [`waker`], [`registry`], [`parking`] (the kick-if-parked
//! protocol and its loom model), [`room`] (a sender told when a full control channel has room, and its loom model),
//! [`task`], [`queue`], [`timer`], [`driver`], [`sim`], [`shard`], [`held_turn`] (what a held shard's idle turn costs a
//! request, learned online, and whether a hold pays),
//! [`runtime`], [`futures`], `attribution` (who held a long poll: the task or the host), the async
//! sockets ([`udp`] everywhere and `tcp` on Unix — the NFS mount server's, over one shared readiness
//! future), `handle_wait` (Windows: a kernel object's signal, a client's process ending, waited on by the system thread
//! pool and posted to the shard's port), and the OS drivers.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds
// are exempt. Once a crate is clean this holds it there; out-of-bounds indexing and slicing are denied
// workspace-wide.
// The panic rule: a print macro panics on a refused write; lines go through `slates_machine::log_line!` (A-213).
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

mod attribution;
pub mod busy_harvest;
pub mod control;
pub mod driver;
pub mod error;
pub mod futures;
pub mod held_turn;
mod interest;
pub mod label;
mod netsys;
pub mod parking;
// A Windows pipe end on the runtime (A-179): its decisions are pure and tested on every host (`pipe_state`, public on
// every host so cfg-free callers can name its answers); its Win32 calls are Windows' alone (`pipe`, and the IOCP
// driver).
#[cfg(target_os = "windows")]
pub mod pipe;
pub mod pipe_state;
pub mod queue;
pub mod readiness;
pub mod registry;
pub mod retirement;
pub mod room;
pub mod runtime;
pub mod shard;
pub mod sim;
pub mod sync;
pub mod task;
mod thread_clock;
pub use thread_clock::{CpuReading, RunReading, RunState, process_run_reading};
// Async TCP on every platform, through the socket seam (`netsys`): the NFS servers, the MCP HTTP edge and the
// network export ride it (`docs/wip/platforms.md`, gap 1).
pub mod tcp;
pub mod timer;
pub mod udp;
pub mod waker;

#[cfg(target_os = "windows")]
mod afd;
#[cfg(target_os = "linux")]
pub mod epoll;
// A kernel object's signal awaited through the shard (A-475): a client's process handle, waited on by the system thread
// pool and posted to the shard's completion port.
#[cfg(target_os = "windows")]
pub mod handle_wait;
#[cfg(target_os = "windows")]
pub mod iocp;
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub mod kqueue;

pub use driver::{Driver, DriverKind};
pub use error::RtError;
pub use registry::SlotHolder;
pub use runtime::{Runtime, RuntimeConfig, ShardCapacity, WakeTracking};
pub use shard::{ShardId, TaskId};
pub use sim::SimRuntime;
pub use task::Outcome;
pub use task::{Admission, AdmissionReceipt};

/// Diagnostic only (g222, never main): raises this thread to macOS's user-interactive QoS class.
#[doc(hidden)]
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn g222_raise_qos() {
  // SAFETY: diagnostic only; sets the calling thread's own QoS class.
  unsafe {
    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
  }
}

/// Diagnostic only (g222, never main): every park's asked timeout, how long it waited, and how it ended
/// (0 timer or other, 1 kicked, 2 untimed, 3 found pending).
#[doc(hidden)]
#[allow(clippy::disallowed_types, missing_docs)]
pub mod g222_parks {
  use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
  pub const CAP: usize = 600_000;
  pub static ROWS: [[AtomicU64; 7]; CAP] = [const { [const { AtomicU64::new(0) }; 7] }; CAP];
  pub static N: AtomicUsize = AtomicUsize::new(0);
  /// Shape: shards a diagnostic run keeps a last park for.
  pub const SHARDS: usize = 64;
  /// w-perf 84a: each shard's last park, host monotonic ns: when it began, the kick's stamp (0: none), when it returned.
  pub static LAST: [[AtomicU64; 3]; SHARDS] = [const { [const { AtomicU64::new(0) }; 3] }; SHARDS];
  /// w-perf 84a: the calling thread's CPU time, ns (`CLOCK_THREAD_CPUTIME_ID`): beside a span's wall time, how much of
  /// it the thread ran.
  pub fn thread_cpu_ns() -> u64 {
    let reading = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    u64::try_from(reading.tv_sec)
      .unwrap_or(0)
      .saturating_mul(1_000_000_000)
      .saturating_add(u64::try_from(reading.tv_nsec).unwrap_or(0))
  }
  /// Shard `shard`'s last park.
  pub fn last(shard: u16) -> [u64; 3] {
    LAST.get(usize::from(shard)).map_or([0; 3], |slot| {
      [
        slot[0].load(Relaxed),
        slot[1].load(Relaxed),
        slot[2].load(Relaxed),
      ]
    })
  }
  /// A zero-timeout harvest's duration, binned by log2 of its microseconds (bin 0: under 1 us).
  pub static HARVEST: [AtomicU64; 24] = [const { AtomicU64::new(0) }; 24];
  pub fn harvested(ns: u64) {
    let us = ns / 1000;
    let bin = if us == 0 {
      0
    } else {
      (64 - us.leading_zeros()) as usize
    };
    HARVEST[bin.min(23)].fetch_add(1, Relaxed);
  }
  pub fn dump(path: &str) {
    use std::io::Write;
    let n = N.load(Relaxed).min(CAP);
    let bins: Vec<String> = HARVEST
      .iter()
      .map(|b| b.swap(0, Relaxed).to_string())
      .collect();
    let mut out = format!("# harvest_us_log2 {}\n", bins.join(" "));
    out.push_str("timeout\twaited\tended\tkick_after\twake_after\tshard\tbegan\n");
    for row in ROWS.iter().take(n) {
      let line: Vec<String> = row.iter().map(|v| v.load(Relaxed).to_string()).collect();
      out.push_str(&line.join("\t"));
      out.push('\n');
    }
    if let Ok(mut f) = std::fs::File::create(path) {
      let _ = f.write_all(out.as_bytes());
    }
    N.store(0, Relaxed);
  }
}

#[doc(hidden)]
pub(crate) fn g222_park(timeout: u64, waited: u64, ended: u64, kick: (u64, u64), at: (u16, u64)) {
  use std::sync::atomic::Ordering::Relaxed;
  let n = g222_parks::N.fetch_add(1, Relaxed);
  if let Some(row) = g222_parks::ROWS.get(n) {
    row[0].store(timeout, Relaxed);
    row[1].store(waited, Relaxed);
    row[2].store(ended, Relaxed);
    row[3].store(kick.0, Relaxed);
    row[4].store(kick.1, Relaxed);
    row[5].store(u64::from(at.0), Relaxed);
    row[6].store(at.1, Relaxed);
  }
}

/// Diagnostic only (w-perf 84a, never main): every poll past its allowed quantum, host monotonic ns: its shard, its
/// start, its length, the quantum it was allowed, how it was attributed (`attribution::Attribution` as a code) and its
/// task's label, to join a queued request's wait to the step that held its shard.
#[doc(hidden)]
#[allow(clippy::disallowed_types, missing_docs)]
pub mod g222_polls {
  use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
  pub const CAP: usize = 200_000;
  pub static ROWS: [[AtomicU64; 6]; CAP] = [const { [const { AtomicU64::new(0) }; 6] }; CAP];
  pub static N: AtomicUsize = AtomicUsize::new(0);
  pub fn record(row: [u64; 6]) {
    let n = N.fetch_add(1, Relaxed);
    if let Some(slot) = ROWS.get(n) {
      for (cell, value) in slot.iter().zip(row) {
        cell.store(value, Relaxed);
      }
    }
  }
  pub fn dump(path: &str) {
    use std::io::Write;
    let n = N.load(Relaxed).min(CAP);
    let mut out = String::from("shard\tstart\tlength\tallowed\tattribution\tlabel\n");
    let mut labels = std::collections::BTreeMap::new();
    for row in ROWS.iter().take(n) {
      let line: Vec<u64> = row.iter().map(|v| v.load(Relaxed)).collect();
      if let Some(label) = line.get(5).and_then(|l| u32::try_from(*l).ok()) {
        labels.entry(label).or_insert_with(|| {
          crate::label::label_of(label).map_or_else(|| "unlabelled".to_owned(), |l| l.to_string())
        });
      }
      let text: Vec<String> = line.iter().map(u64::to_string).collect();
      out.push_str(&text.join("\t"));
      out.push('\n');
    }
    for (index, name) in labels {
      out.push_str(&format!("# label {index} {name}\n"));
    }
    if let Ok(mut f) = std::fs::File::create(path) {
      let _ = f.write_all(out.as_bytes());
    }
    N.store(0, Relaxed);
  }
}

/// Diagnostic only (g222, never main): asks the calling thread for the EEVDF slice `G222_SLICE_NS` (A-224's
/// `sched_runtime` for a `SCHED_OTHER` thread, its policy and nice kept, no privilege), and counts the answer.
#[cfg(target_os = "linux")]
#[allow(
  unsafe_code,
  clippy::all,
  clippy::pedantic,
  clippy::restriction,
  clippy::disallowed_methods
)]
pub(crate) fn g222_slice() {
  #[repr(C)]
  #[derive(Clone, Copy, Default)]
  struct SchedAttr {
    size: u32,
    policy: u32,
    flags: u64,
    nice: i32,
    priority: u32,
    runtime: u64,
    deadline: u64,
    period: u64,
    util_min: u32,
    util_max: u32,
  }
  let Some(slice) = std::env::var("G222_SLICE_NS")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
  else {
    return;
  };
  let mut attr = SchedAttr::default();
  let size = std::mem::size_of::<SchedAttr>() as u32;
  // SAFETY: diagnostic; pid 0 is the calling thread, `attr` is a repr(C) sched_attr of `size` bytes.
  let got = unsafe { libc::syscall(libc::SYS_sched_getattr, 0, &raw mut attr, size, 0) };
  if got != 0 {
    eprintln!(
      "g222 slice: sched_getattr refused: {}",
      std::io::Error::last_os_error()
    );
    return;
  }
  attr.size = size;
  attr.policy = 0;
  attr.runtime = slice;
  // SAFETY: as above; the kernel reads `size` bytes.
  let set = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0) };
  if set != 0 {
    eprintln!(
      "g222 slice: sched_setattr({slice}) refused: {}",
      std::io::Error::last_os_error()
    );
  }
}
