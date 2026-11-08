//! DIAG ONLY, never for main: where a WinFsp request's time goes, per phase (the supervisor's per-IRP profile,
//! 2026-11-06). Each phase of a request's service is timed with [`std::time::Instant`] (the performance counter on
//! Windows) and counted into a fixed histogram per slot and phase: slots below [`OP_BASE`] are the receiver's, keyed by
//! the IRP's transaction kind, and slots from it are the shard's operations, keyed by [`crate::host`]'s operation.
//! Nothing allocates and nothing locks: every counter is a relaxed atomic. [`dump`] prints every non-empty cell to
//! stderr at the mount's end and clears them.
#![allow(dead_code)]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// The performance counter call a receive stamps its batch with.
pub(crate) const STAMP: usize = 0;
/// The journal's record of a delivered batch (`received`, `handed_off`), its stamp included.
pub(crate) const CAPTURE: usize = 1;
/// Taking the next request from the batch.
pub(crate) const DECODE: usize = 2;
/// The whole DLL operation: the copy into the working packet, the private-open check, the callback, the finish.
pub(crate) const OPERATION: usize = 3;
/// The DLL's operation callback alone, our interface callback and the shard's round trip inside it.
pub(crate) const CALLBACK: usize = 4;
/// The journal's save of the response.
pub(crate) const RECORD: usize = 5;
/// The journal's hand-off of the last response to the call that sends it.
pub(crate) const HAND_OFF: usize = 6;
/// Posting an open's confirmation to the shard.
pub(crate) const CONFIRM_POST: usize = 7;
/// Taking the confirmation's answer after the send.
pub(crate) const CONFIRM_FINISH: usize = 8;
/// A response sent alone (every response of a batch but its last).
pub(crate) const SEND_ALONE: usize = 9;
/// The receive worker's whole service: from a transaction's return to the next transaction's call.
pub(crate) const SERVICE: usize = 10;
/// The interface callback's round trip to the shard: post, wake, serve, answer.
pub(crate) const ROUND: usize = 11;
/// From the post to the shard's start of serving it.
pub(crate) const HOP_IN: usize = 12;
/// The shard's serving of the request.
pub(crate) const SERVE: usize = 13;
/// From the shard's send of the reply to the callback's having it.
pub(crate) const TO_ANSWER: usize = 14;
/// Our interface callback alone, from its entry to its return (the DLL's op function calls it).
pub(crate) const INTERFACE: usize = 15;
/// From this thread's last interface callback's return to this one's entry: inside one request, the DLL's op function
/// between our two callbacks (the access check between the security lookup and the open).
pub(crate) const BETWEEN: usize = 16;
/// Main's form: the receive buffer's clearing before a receive (`Capture::begin`).
pub(crate) const BEGIN: usize = 17;
/// Main's form: the journal's acknowledgement of a request once its response was sent.
pub(crate) const ACKNOWLEDGE: usize = 18;
/// The reply's way back when the waiter was still spinning: from the shard's send to the waiter having it.
pub(crate) const TO_ANSWER_SPUN: usize = 19;
/// The reply's way back when the waiter had parked: from the shard's send to the woken waiter having it, the wake's cost.
pub(crate) const TO_ANSWER_PARKED: usize = 20;
/// The spin a waiter spent before it parked: the window, paid in full by every wait that parks.
pub(crate) const SPIN_BEFORE_PARK: usize = 21;
/// Shape: the phases above.
const PHASES: usize = 22;
/// Format: each phase's printed name, in order.
const NAMES: [&str; PHASES] = [
  "stamp",
  "capture",
  "decode",
  "operation",
  "callback",
  "record",
  "hand_off",
  "confirm_post",
  "confirm_finish",
  "send_alone",
  "service",
  "round",
  "hop_in",
  "serve",
  "to_answer",
  "interface",
  "between",
  "begin",
  "acknowledge",
  "to_answer_spun",
  "to_answer_parked",
  "spin_before_park",
];
/// Format: the first operation slot; the receiver's slot for a request of no kind is one below it.
pub(crate) const OP_BASE: usize = 32;
/// Format: the receiver's slot for work not tied to one request (a receive's stamp and capture).
pub(crate) const RECEIVER: usize = OP_BASE - 1;
/// Format: the first interface-callback slot.
pub(crate) const CALLBACK_BASE: usize = OP_BASE * 2;
/// Format: each interface callback's printed name, from CALLBACK_BASE, in the order both arms define them.
const CALLBACK_NAMES: [&str; 16] = [
  "cb_get_volume_info",
  "cb_get_security_by_name",
  "cb_create",
  "cb_open",
  "cb_overwrite",
  "cb_cleanup",
  "cb_close",
  "cb_read",
  "cb_write",
  "cb_flush",
  "cb_get_file_info",
  "cb_set_basic_info",
  "cb_set_file_size",
  "cb_can_delete",
  "cb_rename",
  "cb_read_directory",
];
/// Shape: receiver slots, operation slots and interface-callback slots.
const SLOTS: usize = OP_BASE * 3;

/// Times one interface callback from its creation to its drop.
pub(crate) struct Timer {
  slot: usize,
  start: u64,
}

thread_local! {
  /// When this thread's last interface callback returned.
  static LAST_EXIT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl Timer {
  pub(crate) fn start(callback: usize) -> Self {
    let slot = CALLBACK_BASE + callback;
    let start = now();
    let last = LAST_EXIT.replace(0);
    if last != 0 {
      record(slot, BETWEEN, last, start);
    }
    Self { slot, start }
  }
}

impl Drop for Timer {
  fn drop(&mut self) {
    let end = now();
    record(self.slot, INTERFACE, self.start, end);
    LAST_EXIT.set(end);
  }
}
/// Shape: 100 ns buckets below 20 µs, 1 µs buckets below 200 µs, 25 µs buckets below 5 ms, and one above.
const FINE: u64 = 20_000;
/// Shape: see FINE.
const MEDIUM: u64 = 200_000;
/// Shape: see FINE.
const COARSE: u64 = 5_000_000;
/// Shape: see FINE.
const BUCKETS: usize = 200 + 180 + 192 + 1;

static CELLS: [[[AtomicU64; BUCKETS]; PHASES]; SLOTS] =
  [const { [const { [const { AtomicU64::new(0) }; BUCKETS] }; PHASES] }; SLOTS];
static SUMS: [[AtomicU64; PHASES]; SLOTS] =
  [const { [const { AtomicU64::new(0) }; PHASES] }; SLOTS];
static BASE: OnceLock<Instant> = OnceLock::new();

/// Nanoseconds since the process's first stamp.
pub(crate) fn now() -> u64 {
  u64::try_from(BASE.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn bucket(ns: u64) -> usize {
  let index = if ns < FINE {
    ns / 100
  } else if ns < MEDIUM {
    200 + (ns - FINE) / 1_000
  } else if ns < COARSE {
    380 + (ns - MEDIUM) / 25_000
  } else {
    572
  };
  usize::try_from(index)
    .unwrap_or(BUCKETS - 1)
    .min(BUCKETS - 1)
}

/// A bucket's upper edge, ns.
fn edge(index: usize) -> u64 {
  let index = u64::try_from(index).unwrap_or(u64::MAX);
  if index < 200 {
    (index + 1) * 100
  } else if index < 380 {
    FINE + (index - 199) * 1_000
  } else if index < 572 {
    MEDIUM + (index - 379) * 25_000
  } else {
    u64::MAX
  }
}

/// Counts one phase of slot `slot` that ran from `start` to `end`.
pub(crate) fn record(slot: usize, phase: usize, start: u64, end: u64) {
  let ns = end.saturating_sub(start);
  if let Some(cells) = CELLS.get(slot).and_then(|slot| slot.get(phase)) {
    if let Some(cell) = cells.get(bucket(ns)) {
      cell.fetch_add(1, Relaxed);
    }
  }
  if let Some(sum) = SUMS.get(slot).and_then(|slot| slot.get(phase)) {
    sum.fetch_add(ns, Relaxed);
  }
}

/// The receiver's slot for a transaction kind.
pub(crate) fn kind_slot(kind: u32) -> usize {
  usize::try_from(kind).unwrap_or(RECEIVER).min(RECEIVER)
}

/// Prints every non-empty cell, `phases <slot> <phase> <count> <p50> <p90> <p99> <mean>` in ns, and clears them.
/// `ops` names the operation slots from [`OP_BASE`].
pub(crate) fn dump(ops: &[&str]) {
  for (slot, phases) in CELLS.iter().enumerate() {
    let name = if slot < OP_BASE {
      format!("irp{slot}")
    } else if slot >= CALLBACK_BASE {
      CALLBACK_NAMES
        .get(slot - CALLBACK_BASE)
        .map_or_else(|| format!("cb{slot}"), |name| (*name).to_owned())
    } else {
      ops
        .get(slot - OP_BASE)
        .map_or_else(|| format!("op{slot}"), |name| (*name).to_owned())
    };
    for (phase, cells) in phases.iter().enumerate() {
      let counts: Vec<u64> = cells.iter().map(|cell| cell.swap(0, Relaxed)).collect();
      let sum = SUMS
        .get(slot)
        .and_then(|slot| slot.get(phase))
        .map_or(0, |sum| sum.swap(0, Relaxed));
      let total: u64 = counts.iter().sum();
      if total == 0 {
        continue;
      }
      let rank = |share: u64| {
        let wanted = total.saturating_mul(share).div_ceil(100).max(1);
        let mut seen = 0u64;
        for (index, count) in counts.iter().enumerate() {
          seen = seen.saturating_add(*count);
          if seen >= wanted {
            return edge(index);
          }
        }
        u64::MAX
      };
      eprintln!(
        "phases\t{name}\t{}\t{total}\t{}\t{}\t{}\t{}",
        NAMES.get(phase).unwrap_or(&"?"),
        rank(50),
        rank(90),
        rank(99),
        sum / total
      );
      // The reply's way back is bimodal (a waiter still spinning or one parked), so its whole histogram is printed, and
      // the round trip's with it: `phases_hist <slot> <phase> <upper edge ns>:<count> ...`, non-empty buckets only.
      if matches!(phase, TO_ANSWER | ROUND | TO_ANSWER_SPUN | TO_ANSWER_PARKED) {
        let buckets: Vec<String> = counts
          .iter()
          .enumerate()
          .filter(|(_, count)| **count > 0)
          .map(|(index, count)| format!("{}:{count}", edge(index)))
          .collect();
        eprintln!(
          "phases_hist\t{name}\t{}\t{}",
          NAMES.get(phase).unwrap_or(&"?"),
          buckets.join(" ")
        );
      }
    }
  }
}
