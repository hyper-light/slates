//! Diagnostic only (g222, never main): timestamps along a lone client's submit off its green's shard. With one submit in
//! flight, one set of stamps serves; the reply's step turns them into one sample of each hop.
#![allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction, missing_docs, clippy::disallowed_types, clippy::disallowed_methods)]
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
static SAMPLES: [[AtomicU64; STAMPS - 1]; CAP] = [const { [const { AtomicU64::new(0) }; STAMPS - 1] }; CAP];
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
  COUNTS[at].fetch_add(1, Relaxed);
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
  out.push_str("park_staged\tstaged_asked\tasked_sending\tsending_owner_in\towner_judge\towner_back\tback_resuming\tresuming_replied\n");
  for row in SAMPLES.iter().take(n) {
    let line: Vec<String> = row.iter().map(|v| v.load(Relaxed).to_string()).collect();
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
static RING: [[AtomicU64; 2]; RING_CAP] = [const { [const { AtomicU64::new(0) }; 2] }; RING_CAP];
static RING_TAKEN: AtomicUsize = AtomicUsize::new(0);

pub fn ring(landed: u64, taken: u64, written: u64) {
  let n = RING_TAKEN.fetch_add(1, Relaxed);
  if n < RING_CAP {
    RING[n][0].store(taken.saturating_sub(landed), Relaxed);
    RING[n][1].store(written.saturating_sub(taken), Relaxed);
  }
}

pub fn dump_ring(path: &str) {
  use std::io::Write;
  let n = RING_TAKEN.load(Relaxed).min(RING_CAP);
  let mut out = String::from("queued\tserved\n");
  for row in RING.iter().take(n) {
    out.push_str(&format!("{}\t{}\n", row[0].load(Relaxed), row[1].load(Relaxed)));
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
