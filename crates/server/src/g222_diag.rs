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
/// The ring row's columns: queued, served, landed, written, deferred, then a forward's split (w-perf 84a, [`FWD_COLS`]).
const RING_COLS: usize = 5 + FWD_COLS;
static RING: [[AtomicU64; RING_COLS]; RING_CAP] =
  [const { [const { AtomicU64::new(0) }; RING_COLS] }; RING_CAP];

/// w-perf 84a: a forwarded request's stamps across its shards, by request id, host monotonic ns: sent (origin), started
/// (owner) with the owner's last park [began, kicked, returned], the reply sent home (owner), the reply's task run
/// (origin) with the origin's last park; a submit's wait for its version's acceptance (AUD-11) and its resolution with the
/// owner's last park; which stamps were made; and where `state::deliver` was called for it.
#[derive(Clone, Copy, Default)]
struct Fwd {
  sent: u64,
  started: u64,
  owner_woke: [u64; 3],
  replied: u64,
  home: u64,
  origin_woke: [u64; 3],
  waiting: u64,
  resolved: u64,
  resolve_woke: [u64; 3],
  /// Which stamps were made: sent 1, started 2, replied 4, home 8, waiting 16, resolved 32, delivered 64.
  mask: u64,
  /// The `state::deliver` call's file (a short hash, named in the ring's header) and line.
  deliver_file: u64,
  deliver_line: u64,
  /// A parked submit run again (`verbs::resume_all`): how long it was parked, who resumed it, and the refining job's
  /// turns when a refinement's end resumed it.
  park_wait: u64,
  resume_file: u64,
  resume_line: u64,
  job_turns: u64,
}

thread_local! {
  /// The turns of the refining job whose end is resuming its parked submits now (0 for any other resume).
  static JOB_TURNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A refining job ended after `turns` turns: its parked submits are resumed next.
pub fn job_turns(turns: u64) {
  JOB_TURNS.set(turns);
}

fn file_hash(at: &std::panic::Location<'_>) -> u64 {
  let file = at.file().bytes().fold(0u64, |hash, byte| {
    hash.wrapping_mul(31).wrapping_add(u64::from(byte))
  }) % 1_000_000;
  if let Ok(mut guard) = FILES.lock() {
    guard
      .get_or_insert_with(std::collections::HashMap::new)
      .entry(file)
      .or_insert_with(|| at.file().to_owned());
  }
  file
}

/// A parked submit of request `request` run again after `waited` ns parked, resumed from `at`.
pub fn resumed(request: u64, waited: u64, at: &std::panic::Location<'_>) {
  let (file, line, turns) = (file_hash(at), u64::from(at.line()), JOB_TURNS.get());
  fwd(request, |f| {
    f.mask |= 128;
    f.park_wait = waited;
    f.resume_file = file;
    f.resume_line = line;
    f.job_turns = turns;
  });
}

/// The resumes of one `resume_all` are done.
pub fn resumes_done() {
  JOB_TURNS.set(0);
}
static FWD: std::sync::Mutex<Option<std::collections::HashMap<u64, Fwd>>> =
  std::sync::Mutex::new(None);
/// The files `deliver` was called from, by short hash.
static FILES: std::sync::Mutex<Option<std::collections::HashMap<u64, String>>> =
  std::sync::Mutex::new(None);
/// Shape: forwards kept in flight at once.
const FWD_CAP: usize = 100_000;
/// A deferred reply's split: complete (1 a forward answered at once, 2 a submit answered at its acceptance, 0 a reply
/// some stamp missed), then for the owner how it met the forward ([`side`]: met, kick lateness after the send, kick ->
/// run, run -> stamp), the owner's work (start -> reply sent, or -> the acceptance wait's start), the same four for the
/// origin from the reply's sending, the origin's reply task to the reply written; for an accepted submit the wait for
/// placement and how the owner met it; and for every one the stamps made and the `deliver` call's file and line.
/// And for a parked submit run again: its park's wait, the resume's file and line, and the refining job's turns.
const FWD_COLS: usize = 23;

fn fwd(request: u64, change: impl FnOnce(&mut Fwd)) {
  if let Ok(mut guard) = FWD.lock() {
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    if map.len() < FWD_CAP || map.contains_key(&request) {
      change(map.entry(request).or_default());
    }
  }
}

fn last_here() -> [u64; 3] {
  slates_rt::registry::current_shard().map_or([0; 3], slates_rt::g222_parks::last)
}

/// On the origin, as a request is sent to its owner.
pub fn fwd_sent(request: u64) {
  let at = now_mono();
  fwd(request, |f| {
    *f = Fwd {
      sent: at,
      mask: 1,
      ..Fwd::default()
    }
  });
}

/// On the owner, as the forward's task starts.
pub fn fwd_started(request: u64) {
  let (at, woke) = (now_mono(), last_here());
  fwd(request, |f| {
    f.mask |= 2;
    f.started = at;
    f.owner_woke = woke;
  });
}

/// On the owner, as the reply is sent home.
pub fn fwd_replied(request: u64) {
  let at = now_mono();
  fwd(request, |f| {
    f.mask |= 4;
    f.replied = at;
  });
}

/// On the origin, as the reply's task runs.
pub fn fwd_home(request: u64) {
  let (at, woke) = (now_mono(), last_here());
  fwd(request, |f| {
    f.mask |= 8;
    f.home = at;
    f.origin_woke = woke;
  });
}

/// On the owner, as a submit's reply begins waiting for its version's acceptance.
pub fn acc_waiting(request: u64) {
  let at = now_mono();
  fwd(request, |f| {
    f.mask |= 16;
    f.waiting = at;
  });
}

/// On the owner, as a submit's version is placed and its reply resolved.
pub fn acc_resolved(request: u64) {
  let (at, woke) = (now_mono(), last_here());
  fwd(request, |f| {
    f.mask |= 32;
    f.resolved = at;
    f.resolve_woke = woke;
  });
}

/// On the origin, as a reply from elsewhere is queued for its client's ring (`state::deliver`): where it was called.
pub fn delivered(request: u64, at: &std::panic::Location<'_>) {
  let file = at.file().bytes().fold(0u64, |hash, byte| {
    hash.wrapping_mul(31).wrapping_add(u64::from(byte))
  }) % 1_000_000;
  let line = u64::from(at.line());
  fwd(request, |f| {
    f.mask |= 64;
    f.deliver_file = file;
    f.deliver_line = line;
  });
  if let Ok(mut guard) = FILES.lock() {
    guard
      .get_or_insert_with(std::collections::HashMap::new)
      .entry(file)
      .or_insert_with(|| at.file().to_owned());
  }
}

/// The files `deliver` was called from, as header lines.
fn files_header() -> String {
  FILES
    .lock()
    .ok()
    .and_then(|guard| {
      guard.as_ref().map(|files| {
        files
          .iter()
          .map(|(hash, name)| format!("# deliver file {hash} {name}\n"))
          .collect()
      })
    })
    .unwrap_or_default()
}

/// The split of request `request`'s reply, written at `written`; taken out of the table.
fn fwd_split(request: u64, written: u64) -> [u64; FWD_COLS] {
  let Some(f) = FWD
    .lock()
    .ok()
    .and_then(|mut guard| guard.as_mut().and_then(|map| map.remove(&request)))
  else {
    return [0; FWD_COLS];
  };
  let mut out = [0; FWD_COLS];
  out[16] = f.mask;
  out[17] = f.deliver_file;
  out[18] = f.deliver_line;
  out[19] = f.park_wait;
  out[20] = f.resume_file;
  out[21] = f.resume_line;
  out[22] = f.job_turns;
  let accepted = f.replied == 0 && f.waiting != 0 && f.resolved != 0;
  if f.sent == 0 || f.started == 0 || f.home == 0 || (f.replied == 0 && !accepted) {
    return out;
  }
  let sent_home = if accepted { f.resolved } else { f.replied };
  let worked_to = if accepted { f.waiting } else { f.replied };
  out[0] = if accepted { 2 } else { 1 };
  out[1..5].copy_from_slice(&side(f.sent, f.started, f.owner_woke));
  out[5] = worked_to.saturating_sub(f.started);
  out[6..10].copy_from_slice(&side(sent_home, f.home, f.origin_woke));
  out[10] = written.saturating_sub(f.home);
  if accepted {
    out[11] = f.resolved.saturating_sub(f.waiting);
    out[12..16].copy_from_slice(&side(f.waiting, f.resolved, f.resolve_woke));
  }
  out
}
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
    let written = now_mono();
    let split = fwd_split(request, written);
    ring_row_split(landed, taken, written, 1, split);
  }
}

fn now_mono() -> u64 {
  slates_machine::clock::monotonic_ns()
}

pub fn ring(landed: u64, taken: u64, written: u64) {
  ring_row(landed, taken, written, 0);
}

fn ring_row(landed: u64, taken: u64, written: u64, deferred: u64) {
  ring_row_split(landed, taken, written, deferred, [0; FWD_COLS]);
}

fn ring_row_split(landed: u64, taken: u64, written: u64, deferred: u64, split: [u64; FWD_COLS]) {
  let n = RING_TAKEN.fetch_add(1, Relaxed);
  if n < RING_CAP {
    for (slot, value) in RING[n][5..].iter().zip(split) {
      slot.store(value, Relaxed);
    }
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
  let mut out = files_header();
  out += &format!(
    "# client sends {} rings {}\nqueued\tserved\tlanded\twritten\tdeferred\tcomplete\towner_met\towner_kick_after\towner_kick_run\towner_run_start\towner_work\torigin_met\torigin_kick_after\torigin_kick_run\torigin_run_home\thome_written\tplacement\tresolve_met\tresolve_kick_after\tresolve_kick_run\tresolve_run_at\tmask\tdeliver_file\tdeliver_line\tpark_wait\tresume_file\tresume_line\tjob_turns\n",
    slates_ipc::endpoint::G222_SENDS.swap(0, Relaxed),
    slates_ipc::endpoint::G222_RINGS.swap(0, Relaxed)
  );
  for row in RING.iter().take(n) {
    let line: Vec<String> = row.iter().map(|v| v.load(Relaxed).to_string()).collect();
    out.push_str(&line.join("\t"));
    out.push('\n');
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
