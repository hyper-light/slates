//! Diagnostic only (g222, never main): timestamps along a lone client's submit off its green's shard. With one submit in
//! flight, one set of stamps serves; the reply's step turns them into one sample of each hop.
#![allow(
  clippy::all,
  clippy::pedantic,
  clippy::nursery,
  clippy::restriction,
  missing_docs,
  clippy::disallowed_types,
  clippy::disallowed_methods
)]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

pub const PARK: usize = 0;
pub const STAGED: usize = 1;
pub const ASKED: usize = 2;
pub const SENDING: usize = 3;
pub const OWNER_IN: usize = 4;
pub const OWNER_OUT: usize = 5;
pub const BACK: usize = 6;
pub const RESUMING: usize = 7;
pub const REPLIED: usize = 8;
const STAMPS: usize = 9;
const CAP: usize = 20_000;

static NOW: [AtomicU64; STAMPS] = [const { AtomicU64::new(0) }; STAMPS];
static SAMPLES: [[AtomicU64; STAMPS - 1]; CAP] =
  [const { [const { AtomicU64::new(0) }; STAMPS - 1] }; CAP];
/// w-perf 84a: each stamp on the host's monotonic clock, as the shards' parks are stamped.
static MONO: [AtomicU64; STAMPS] = [const { AtomicU64::new(0) }; STAMPS];
/// w-perf 84a: the last park of the shard that made the owner's and the origin's stamps, read as each ran: [began,
/// kicked, returned] for OWNER_IN (the owner) and BACK (the origin).
static WOKE: [[AtomicU64; 3]; 2] = [const { [const { AtomicU64::new(0) }; 3] }; 2];
/// w-perf 84a: per sample, for the owner then the origin: how it met its message (0 running, it never parked after the
/// message was sent; 1 parked and kicked; 2 parked and woken otherwise), the kick's lateness after the send, the
/// kick to running, and running to the stamp.
const SIDES: usize = 8;
static SPLIT: [[AtomicU64; SIDES]; CAP] = [const { [const { AtomicU64::new(0) }; SIDES] }; CAP];
static TAKEN: AtomicUsize = AtomicUsize::new(0);
static COUNTS: [AtomicU64; STAMPS] = [const { AtomicU64::new(0) }; STAMPS];

fn now() -> u64 {
  slates_rt::futures::now_ns()
}

pub fn stamp(at: usize) {
  if at == PARK {
    for slot in &NOW {
      slot.store(0, Relaxed);
    }
  }
  NOW[at].store(now(), Relaxed);
  MONO[at].store(slates_machine::clock::monotonic_ns(), Relaxed);
  COUNTS[at].fetch_add(1, Relaxed);
  let side = match at {
    OWNER_IN => Some(0),
    BACK => Some(1),
    _ => None,
  };
  if let (Some(side), Some(shard)) = (side, slates_rt::registry::current_shard()) {
    let last = slates_rt::g222_parks::last(shard);
    for (slot, value) in WOKE[side].iter().zip(last) {
      slot.store(value, Relaxed);
    }
  }
}

/// How one side met the message sent to it at `sent`, its stamp at `at`, from its last park `woke`.
fn side(sent: u64, at: u64, woke: [u64; 3]) -> [u64; 4] {
  let [_, kicked, returned] = woke;
  if returned <= sent || returned > at {
    return [0, 0, 0, at.saturating_sub(sent)];
  }
  if kicked == 0 {
    return [2, 0, 0, at.saturating_sub(returned)];
  }
  [
    1,
    kicked.saturating_sub(sent),
    returned.saturating_sub(kicked),
    at.saturating_sub(returned),
  ]
}

/// At the reply: one sample of every hop, if every stamp of this submit was made.
pub fn replied() {
  NOW[REPLIED].store(now(), Relaxed);
  COUNTS[REPLIED].fetch_add(1, Relaxed);
  let stamps: Vec<u64> = NOW.iter().map(|s| s.load(Relaxed)).collect();
  if stamps.iter().any(|s| *s == 0) {
    return;
  }
  let n = TAKEN.fetch_add(1, Relaxed);
  if n >= CAP {
    return;
  }
  for hop in 0..STAMPS - 1 {
    SAMPLES[n][hop].store(stamps[hop + 1].saturating_sub(stamps[hop]), Relaxed);
  }
  let mono: Vec<u64> = MONO.iter().map(|s| s.load(Relaxed)).collect();
  let woke = |side: usize| -> [u64; 3] { [0, 1, 2].map(|i| WOKE[side][i].load(Relaxed)) };
  let owner = side(mono[SENDING], mono[OWNER_IN], woke(0));
  let origin = side(mono[OWNER_OUT], mono[BACK], woke(1));
  for (slot, value) in SPLIT[n].iter().zip(owner.into_iter().chain(origin)) {
    slot.store(value, Relaxed);
  }
  for slot in &NOW {
    slot.store(0, Relaxed);
  }
}

/// The samples as TSV to `path`, one line a submit: the eight hops in ns.
pub fn dump(path: &str) {
  use std::io::Write;
  let n = TAKEN.load(Relaxed).min(CAP);
  let counts: Vec<String> = COUNTS.iter().map(|c| c.load(Relaxed).to_string()).collect();
  let mut out = format!("# stamp counts park..replied: {}\n", counts.join(" "));
  out.push_str("park_staged\tstaged_asked\tasked_sending\tsending_owner_in\towner_judge\towner_back\tback_resuming\tresuming_replied\towner_met\towner_kick_after\towner_kick_run\towner_run_in\torigin_met\torigin_kick_after\torigin_kick_run\torigin_run_back\n");
  for (row, split) in SAMPLES.iter().zip(SPLIT.iter()).take(n) {
    let line: Vec<String> = row
      .iter()
      .chain(split.iter())
      .map(|v| v.load(Relaxed).to_string())
      .collect();
    out.push_str(&line.join("\t"));
    out.push('\n');
  }
  if let Ok(mut f) = std::fs::File::create(path) {
    let _ = f.write_all(out.as_bytes());
  }
  TAKEN.store(0, Relaxed);
}

// Ring split: for every request a shard takes, (landed → taken, taken → reply written), ns.
const RING_CAP: usize = 300_000;
static RING: [[AtomicU64; 5]; RING_CAP] = [const { [const { AtomicU64::new(0) }; 5] }; RING_CAP];
static RING_TAKEN: AtomicUsize = AtomicUsize::new(0);

thread_local! {
  static TAKES: std::cell::RefCell<std::collections::HashMap<u64, (u64, u64)>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A request a shard took: kept until its reply is written, so a reply written in a later step is split too.
pub fn took(request: u64, landed: u64, taken: u64) {
  TAKES.with(|t| {
    let mut t = t.borrow_mut();
    if t.len() < 100_000 {
      t.insert(request, (landed, taken));
    }
  });
}

pub fn forget(request: u64) {
  TAKES.with(|t| t.borrow_mut().remove(&request));
}

/// A reply written for a request taken in an earlier step (resumed or forwarded): a ring row flagged deferred.
pub fn deferred_written(request: u64) {
  if let Some((landed, taken)) = TAKES.with(|t| t.borrow_mut().remove(&request)) {
    ring_row(landed, taken, now_mono(), 1);
  }
}

fn now_mono() -> u64 {
  slates_machine::clock::monotonic_ns()
}

pub fn ring(landed: u64, taken: u64, written: u64) {
  ring_row(landed, taken, written, 0);
}

fn ring_row(landed: u64, taken: u64, written: u64, deferred: u64) {
  let n = RING_TAKEN.fetch_add(1, Relaxed);
  if n < RING_CAP {
    RING[n][0].store(taken.saturating_sub(landed), Relaxed);
    RING[n][1].store(written.saturating_sub(taken), Relaxed);
    RING[n][2].store(landed, Relaxed);
    RING[n][3].store(written, Relaxed);
    RING[n][4].store(deferred, Relaxed);
  }
}

pub fn dump_ring(path: &str) {
  use std::io::Write;
  let n = RING_TAKEN.load(Relaxed).min(RING_CAP);
  let mut out = format!(
    "# client sends {} rings {}\nqueued\tserved\tlanded\twritten\tdeferred\n",
    slates_ipc::endpoint::G222_SENDS.swap(0, Relaxed),
    slates_ipc::endpoint::G222_RINGS.swap(0, Relaxed)
  );
  for row in RING.iter().take(n) {
    out.push_str(&format!(
      "{}\t{}\t{}\t{}\t{}\n",
      row[0].load(Relaxed),
      row[1].load(Relaxed),
      row[2].load(Relaxed),
      row[3].load(Relaxed),
      row[4].load(Relaxed)
    ));
  }
  if let Ok(mut f) = std::fs::File::create(path) {
    let _ = f.write_all(out.as_bytes());
  }
  RING_TAKEN.store(0, Relaxed);
}

pub fn dump_parks(path: &str) {
  slates_rt::g222_parks::dump(path);
}

#[cfg(target_os = "macos")]
pub fn raise_qos() {
  slates_rt::g222_raise_qos();
}
