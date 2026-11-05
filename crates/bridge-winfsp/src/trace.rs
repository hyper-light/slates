//! DIAG, not for main (audit step 7, winfsp-overlap): which IRPs each mount_bench phase makes, how long each one waits
//! and is served, and whether another arrived while one was served. Both arms build this one file: the DLL arm through
//! WinFsp's operation guard (around the lock its coarse strategy takes), the receiver arm around its own operation
//! call. mount_bench opens a missing `__phase-<pass>-r<round>-<phase>` name before each phase, which names the phase
//! here, and `__phase-<pass>-dump` after its last round, which prints that pass's table on stderr.
#![allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]

use std::cell::Cell;
use std::sync::Mutex;
use std::time::Instant;

const KINDS: usize = 22;
const PHASES: [&str; 5] = ["create", "stat", "open", "rename", "unlink"];
const PASSES: [&str; 2] = ["quiet", "contended"];
/// Phase slots: each pass's five, and one for anything before the first marker.
const SLOTS: usize = 11;
const SUB: usize = 16;
const OCTAVES: usize = 32;
const KIND_NAMES: [&str; KINDS] = [
  "Reserved", "Create", "Overwrite", "Cleanup", "Close", "Read", "Write", "QueryInformation", "SetInformation",
  "QueryEa", "SetEa", "FlushBuffers", "QueryVolumeInformation", "SetVolumeInformation", "QueryDirectory",
  "FileSystemControl", "DeviceControl", "Shutdown", "QuerySecurity", "SetSecurity", "QueryStreamInformation", "Kind21",
];

#[derive(Clone)]
struct Hist {
  buckets: Vec<u64>,
  count: u64,
  sum: u128,
}

impl Hist {
  fn new() -> Self {
    Self { buckets: vec![0; SUB * OCTAVES], count: 0, sum: 0 }
  }
  fn add(&mut self, ns: u64) {
    let index = if ns < SUB as u64 {
      ns as usize
    } else {
      let octave = 63 - ns.leading_zeros() as usize;
      let shift = octave - 4;
      let sub = ((ns >> shift) as usize) & (SUB - 1);
      ((octave - 3) * SUB + sub).min(SUB * OCTAVES - 1)
    };
    self.buckets[index] += 1;
    self.count += 1;
    self.sum += ns as u128;
  }
  fn low(index: usize) -> u64 {
    if index < SUB {
      return index as u64;
    }
    let octave = index / SUB + 3;
    let sub = (index % SUB) as u64;
    (SUB as u64 + sub) << (octave - 4)
  }
  fn rank(&self, permille: u64) -> f64 {
    if self.count == 0 {
      return 0.0;
    }
    let target = (self.count * permille).div_ceil(1000).max(1);
    let mut seen = 0;
    for (index, n) in self.buckets.iter().enumerate() {
      seen += n;
      if seen >= target {
        return Self::low(index) as f64 / 1000.0;
      }
    }
    0.0
  }
  fn mean(&self) -> f64 {
    if self.count == 0 { 0.0 } else { self.sum as f64 / self.count as f64 / 1000.0 }
  }
}

struct KindCell {
  wait: Hist,
  service: Hist,
  /// Arrived while another request was being served.
  arrived_busy: u64,
}

struct Slot {
  kinds: Vec<KindCell>,
  /// The receiver arm: each receive call's duration, and how many requests it brought.
  receive: Hist,
  batches: [u64; 5],
  /// Nanoseconds with one or more requests in service, and with one or more waiting besides.
  busy_ns: u128,
  queued_ns: u128,
  first: Option<Instant>,
  last: Option<Instant>,
}

struct State {
  slot: usize,
  in_service: usize,
  waiting: usize,
  changed: Instant,
  slots: Vec<Slot>,
  pass_rounds: [u32; 2],
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

thread_local! {
  static STARTED: Cell<Option<(Instant, usize)>> = const { Cell::new(None) };
}

fn state<R>(with: impl FnOnce(&mut State) -> R) -> R {
  let mut guard = STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = guard.get_or_insert_with(|| State {
    slot: SLOTS - 1,
    in_service: 0,
    waiting: 0,
    changed: Instant::now(),
    slots: (0..SLOTS)
      .map(|_| Slot {
        kinds: (0..KINDS)
          .map(|_| KindCell { wait: Hist::new(), service: Hist::new(), arrived_busy: 0 })
          .collect(),
        receive: Hist::new(),
        batches: [0; 5],
        busy_ns: 0,
        queued_ns: 0,
        first: None,
        last: None,
      })
      .collect(),
    pass_rounds: [0; 2],
  });
  with(state)
}

fn account(state: &mut State, now: Instant) {
  let span = now.saturating_duration_since(state.changed).as_nanos();
  let slot = &mut state.slots[state.slot];
  if state.in_service > 0 {
    slot.busy_ns += span;
  }
  if state.in_service > 0 && state.waiting > 0 {
    slot.queued_ns += span;
  }
  state.changed = now;
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
  Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

/// The Create's file name, if it is a phase marker: `(pass, phase)` where phase `5` is a dump.
fn marker(request: &[u8]) -> Option<(usize, usize, bool)> {
  let offset = usize::from(read_u16(request, 80)?);
  let length = usize::from(read_u16(request, 82)?);
  let start = 88 + offset;
  let bytes = request.get(start..start + length)?;
  let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
  let name = String::from_utf16_lossy(&units);
  let tail = &name[name.find("__phase-")? + "__phase-".len()..];
  let tail = tail.trim_end_matches('\0');
  let pass = PASSES.iter().position(|pass| tail.starts_with(pass))?;
  if tail.ends_with("-dump") {
    return Some((pass, 0, true));
  }
  let phase = PHASES.iter().position(|phase| tail.ends_with(&format!("-{phase}")))?;
  Some((pass, phase, false))
}

/// A request reaches the server: before the DLL arm's guard lock, or at the receiver's batch receipt.
pub(crate) fn arrive(request: &[u8], arrived: Instant) {
  let kind = request.get(4..8).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])) as usize);
  let mut dump = None;
  state(|state| {
    if kind == 1 {
      if let Some((pass, phase, is_dump)) = marker(request) {
        if is_dump {
          dump = Some(pass);
        } else {
          account(state, arrived);
          state.slot = pass * PHASES.len() + phase;
          if phase == 0 {
            state.pass_rounds[pass] += 1;
          }
        }
      }
    }
    account(state, arrived);
    if state.in_service > 0 {
      if let Some(cell) = state.slots[state.slot].kinds.get_mut(kind) {
        cell.arrived_busy += 1;
      }
    }
    state.waiting += 1;
  });
  if let Some(pass) = dump {
    print(pass);
  }
}

/// The request's service begins: the guard lock taken, or the receiver's operation called.
pub(crate) fn begin(request: &[u8], arrived: Instant) {
  let kind = request.get(4..8).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])) as usize);
  let now = Instant::now();
  state(|state| {
    account(state, now);
    state.waiting = state.waiting.saturating_sub(1);
    state.in_service += 1;
    let slot = &mut state.slots[state.slot];
    slot.first.get_or_insert(now);
    if let Some(cell) = slot.kinds.get_mut(kind) {
      cell.wait.add(now.saturating_duration_since(arrived).as_nanos() as u64);
    }
  });
  STARTED.set(Some((now, kind)));
}

/// The request's service ends.
pub(crate) fn end() {
  let now = Instant::now();
  let Some((started, kind)) = STARTED.take() else { return };
  state(|state| {
    account(state, now);
    state.in_service = state.in_service.saturating_sub(1);
    let slot = &mut state.slots[state.slot];
    slot.last = Some(now);
    if let Some(cell) = slot.kinds.get_mut(kind) {
      cell.service.add(now.saturating_duration_since(started).as_nanos() as u64);
    }
  });
}

/// The receiver arm: one receive call returned `count` requests after `waited`.
pub(crate) fn received(count: usize, waited: std::time::Duration) {
  state(|state| {
    let slot = &mut state.slots[state.slot];
    slot.receive.add(waited.as_nanos() as u64);
    slot.batches[count.min(4)] += 1;
  });
}

fn print(pass: usize) {
  let lines = state(|state| {
    let mut lines = Vec::new();
    let rounds = state.pass_rounds[pass];
    for (phase_index, phase) in PHASES.iter().enumerate() {
      let slot = &state.slots[pass * PHASES.len() + phase_index];
      let wall = match (slot.first, slot.last) {
        (Some(first), Some(last)) => last.saturating_duration_since(first).as_secs_f64() * 1e6,
        _ => 0.0,
      };
      lines.push(format!(
        "WINFSP TRACE pass={} phase={phase} rounds={rounds} busy_us={:.0} queued_us={:.0} wall_us={wall:.0} \
         receives={} receive_p50={:.1} receive_p90={:.1} batches0..4+={:?}",
        PASSES[pass],
        slot.busy_ns as f64 / 1000.0,
        slot.queued_ns as f64 / 1000.0,
        slot.receive.count,
        slot.receive.rank(500),
        slot.receive.rank(900),
        slot.batches,
      ));
      for (kind, cell) in slot.kinds.iter().enumerate() {
        if cell.service.count == 0 {
          continue;
        }
        lines.push(format!(
          "WINFSP TRACE pass={} phase={phase} kind={} n={} arrived_busy={} wait_mean={:.1} wait_p50={:.1} \
           wait_p90={:.1} service_mean={:.1} service_p50={:.1} service_p90={:.1}",
          PASSES[pass],
          KIND_NAMES[kind],
          cell.service.count,
          cell.arrived_busy,
          cell.wait.mean(),
          cell.wait.rank(500),
          cell.wait.rank(900),
          cell.service.mean(),
          cell.service.rank(500),
          cell.service.rank(900),
        ));
      }
    }
    lines
  });
  for line in lines {
    eprintln!("{line}");
  }
}
