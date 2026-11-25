//! What holds a daemon's memory once stock Mercurial has fetched a whole history from it (goal 93, a bounded working
//! set per operation; main 01a20182's order of 2026-11-25: the 57k clone left the daemon at 5,659 MiB against a 337 MB
//! store, `MALLOC_SMALL` 6.9 GB virtual). Its own binary, so the slates binary's allocator ([`LargeMapped`]) is the
//! global one, wrapped in a sampler of live bytes: about one allocation in every [`SAMPLE`] bytes is recorded with its
//! site, followed through `realloc` and forgotten at its `dealloc`, so what is still recorded at a moment is the live
//! heap by site, each sample standing for the bytes allocated since the last. An in-process daemon (a changeset store,
//! its HTTP edge on loopback) is pushed a history by stock `hg`, then a whole clone is fetched from it with the sampler
//! on; the live sites are read as the clone ends, beside the daemon's own memory gauges and the process's resident set.
//!
//! By default it pushes a [`CHANGESETS`]-changeset history `hg debugbuilddag` makes; with `SLATES_HG_PUSH_SOURCE`
//! naming a Mercurial repository (Mercurial's own history, 57,036 changesets), it pushes that one. `#[ignore]`d: a
//! measurement, run by name.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::print_stderr,
  clippy::disallowed_methods,
  clippy::disallowed_types,
  clippy::cast_precision_loss
)]

mod common;

use std::alloc::{GlobalAlloc, Layout};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use slates_mem::large::LargeMapped;

/// Shape: the default history's changesets, each a new file and the overwritten file rewritten.
const CHANGESETS: u64 = 2_000;
/// Shape: the bytes allocated between samples, a thread's own: at the 57k clone's 5.6 GB about 1,400 live samples.
const SAMPLE: u64 = 4 << 20;
/// Shape: the bytes allocated on the mapped path (blocks of at least `slates_mem::large::floor`, each a fresh mapping
/// whose pages are faulted in) between that path's samples, a thread's own: fewer, larger blocks than the system path's.
const MAPPED_SAMPLE: u64 = 64 << 20;
/// Shape: the frames in slates' crates a site is named by, innermost first.
const SITE_FRAMES: usize = 3;
/// Shape: the live sites printed, the largest first.
const LISTED: usize = 20;

/// Whether the sampler records: only while the fetch runs, and until its live sites are read.
static ON: AtomicBool = AtomicBool::new(false);
/// The sampled heap: the allocations still live and the largest moment since [`Live::peak`] was last reset.
static LIVE: Mutex<Option<Live>> = Mutex::new(None);
/// The churn since it was last taken ([`take_churn`]), by site: the system path's bytes allocated, and the mapped path's,
/// each sample standing for the bytes its path allocated since the last, kept whether freed or not.
static CHURN: Mutex<Option<Churn>> = Mutex::new(None);

/// What [`CHURN`] tallies: the system path's and the mapped path's bytes allocated, by site.
#[derive(Default)]
struct Churn {
  /// Blocks under the mapped floor (the system allocator's), by site.
  system: BTreeMap<String, u64>,
  /// Blocks at or past it (each a fresh mapping), by site.
  mapped: BTreeMap<String, u64>,
}

/// The sampled allocations still live, their sum, and the live heap by site at its peak.
#[derive(Default)]
struct Live {
  /// By address, each its site and the bytes it stands for.
  samples: HashMap<usize, (String, u64)>,
  /// The bytes the live samples stand for.
  total: u64,
  /// The largest `total` since the peak was reset.
  peak: u64,
  /// The live heap by site when `total` last passed `peak` by a [`PEAK_STEP`]th.
  at_peak: BTreeMap<String, u64>,
}

/// Shape: a peak is taken again once the live total passes the last by this fraction's inverse (a sixteenth).
const PEAK_STEP: u64 = 16;

impl Live {
  /// The live heap by site.
  fn by_site(&self) -> BTreeMap<String, u64> {
    let mut by_site: BTreeMap<String, u64> = BTreeMap::new();
    for (site, bytes) in self.samples.values() {
      *by_site.entry(site.clone()).or_insert(0) += bytes;
    }
    by_site
  }

  /// Records a sample, and the live heap by site if it is a new peak.
  fn insert(&mut self, ptr: usize, sample: (String, u64)) {
    self.total = self.total.saturating_add(sample.1);
    if let Some((_, old)) = self.samples.insert(ptr, sample) {
      self.total = self.total.saturating_sub(old);
    }
    if self.total > self.peak.saturating_add(self.peak / PEAK_STEP) {
      self.peak = self.total;
      self.at_peak = self.by_site();
    }
  }

  /// Forgets the sample at `ptr`, if any, and answers it.
  fn remove(&mut self, ptr: usize) -> Option<(String, u64)> {
    let sample = self.samples.remove(&ptr)?;
    self.total = self.total.saturating_sub(sample.1);
    Some(sample)
  }
}

std::thread_local! {
  /// Set while this thread samples or forgets: its own allocations are not sampled.
  static GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// This thread's bytes allocated since its last sample.
  static SINCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
  /// This thread's bytes allocated on the mapped path since its last sample of that path.
  static MAPPED_SINCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A backtrace's site: its innermost [`SITE_FRAMES`] frames outside the standard library (slates' crates and its
/// dependencies), each its function and its file and line, the sampler's own file and the allocator's passed over. By the
/// frames' files, not their names: built with line tables alone, a frame names its function bare (`allocated`,
/// `{closure#1}`), so a name says nothing of where it lies (main's 57k run at d3057763e: every site the sampler itself).
fn site(trace: &str) -> String {
  let mut frames: Vec<(String, Option<String>)> = Vec::new();
  for line in trace.lines().map(str::trim) {
    if let Some(at) = line.strip_prefix("at ") {
      if let Some(frame) = frames.last_mut() {
        frame.1 = Some(at.to_owned());
      }
    } else if let Some((number, name)) = line.split_once(": ")
      && number.chars().all(|c| c.is_ascii_digit())
    {
      let name = name.rsplit_once("::h").map_or(name, |(head, _)| head);
      frames.push((name.to_owned(), None));
    }
  }
  let ours: Vec<String> = frames
    .iter()
    .filter_map(|(name, at)| {
      let at = at.as_deref()?;
      // The standard library's frames (std, core, alloc) are passed over; a dependency's (vendor/, a registry's) are
      // kept, since a future's own allocations may lie wholly in one (main's 57k run: a site the runtime's poll alone).
      if at.contains("/rustc/") || at.contains("/library/") {
        return None;
      }
      let within = at
        .split_once("/crates/")
        .map(|(_, within)| format!("crates/{within}"))
        .or_else(|| {
          at.split_once("/vendor/")
            .map(|(_, within)| format!("vendor/{within}"))
        })
        .or_else(|| {
          at.split_once("/registry/src/")
            .map(|(_, within)| format!("registry/{within}"))
        })
        .unwrap_or_else(|| at.to_owned());
      let passed = within.contains("tests/hg_fetch_heap.rs") || within.contains("mem/src/large.rs");
      let line = within
        .rsplit_once(':')
        .map_or(within.as_str(), |(head, _)| head);
      (!passed).then(|| format!("{name} ({line})"))
    })
    .take(SITE_FRAMES)
    .collect();
  if ours.is_empty() {
    "(no frame outside the standard library)".to_owned()
  } else {
    ours.join(" <- ")
  }
}

/// Runs `step` with this thread's sampler guarded, so nothing it allocates is sampled; none where it already is.
fn guarded<T>(step: impl FnOnce() -> T) -> Option<T> {
  if GUARD.with(|guard| guard.replace(true)) {
    return None;
  }
  let answer = step();
  GUARD.with(|guard| guard.set(false));
  Some(answer)
}

/// Notes `size` bytes allocated at `ptr`: recorded with its site once this thread has allocated [`SAMPLE`] bytes since
/// its last sample.
fn allocated(ptr: *mut u8, size: usize) {
  if ptr.is_null() || !ON.load(Ordering::Relaxed) || GUARD.with(std::cell::Cell::get) {
    return;
  }
  if size >= slates_mem::large::floor() {
    mapped_allocated(size);
  }
  let since = SINCE.with(|since| {
    let now = since.get().saturating_add(size as u64);
    since.set(now);
    now
  });
  if since < SAMPLE {
    return;
  }
  let _ = guarded(|| {
    SINCE.with(|since| since.set(0));
    let key = site(&std::backtrace::Backtrace::force_capture().to_string());
    if size < slates_mem::large::floor()
      && let Ok(mut churn) = CHURN.lock()
    {
      *churn
        .get_or_insert_with(Churn::default)
        .system
        .entry(key.clone())
        .or_insert(0) += since;
    }
    if let Ok(mut live) = LIVE.lock() {
      live
        .get_or_insert_with(Live::default)
        .insert(ptr as usize, (key, since));
    }
  });
}

/// Notes `size` bytes allocated on the mapped path: tallied by site ([`CHURN`]) once this thread has allocated
/// [`MAPPED_SAMPLE`] bytes there since its last sample of it.
fn mapped_allocated(size: usize) {
  let since = MAPPED_SINCE.with(|since| {
    let now = since.get().saturating_add(size as u64);
    since.set(now);
    now
  });
  if since < MAPPED_SAMPLE {
    return;
  }
  let _ = guarded(|| {
    MAPPED_SINCE.with(|since| since.set(0));
    let key = site(&std::backtrace::Backtrace::force_capture().to_string());
    if let Ok(mut churn) = CHURN.lock() {
      *churn
        .get_or_insert_with(Churn::default)
        .mapped
        .entry(key)
        .or_insert(0) += since;
    }
  });
}

/// The churn since it was last taken, the system path's and the mapped path's, by site; taken, it starts again.
fn take_churn() -> (Listed, Listed) {
  let churn = guarded(|| {
    CHURN
      .lock()
      .ok()
      .and_then(|mut churn| churn.take())
      .unwrap_or_default()
  })
  .unwrap_or_default();
  (listed(churn.system), listed(churn.mapped))
}

/// Forgets the allocation at `ptr`, if it was sampled.
fn freed(ptr: *mut u8) {
  if !ON.load(Ordering::Relaxed) {
    return;
  }
  let _ = guarded(|| {
    if let Ok(mut live) = LIVE.lock()
      && let Some(live) = live.as_mut()
    {
      live.remove(ptr as usize);
    }
  });
}

/// Moves the sample at `from`, if any, to `to`, its allocation moved by a `realloc`.
fn moved(from: *mut u8, to: *mut u8) {
  if !ON.load(Ordering::Relaxed) || from == to || to.is_null() {
    return;
  }
  let _ = guarded(|| {
    if let Ok(mut live) = LIVE.lock()
      && let Some(live) = live.as_mut()
      && let Some(sample) = live.remove(from as usize)
    {
      live.insert(to as usize, sample);
    }
  });
}

/// `LargeMapped`, its live allocations sampled.
struct Sampled;

// SAFETY: every call is `LargeMapped`'s, unchanged; the sampler only reads the layout's size and the pointers around it.
unsafe impl GlobalAlloc for Sampled {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    // SAFETY: as the caller's contract with `alloc`.
    let ptr = unsafe { LargeMapped.alloc(layout) };
    allocated(ptr, layout.size());
    ptr
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    freed(ptr);
    // SAFETY: as the caller's contract with `dealloc`.
    unsafe { LargeMapped.dealloc(ptr, layout) }
  }
  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    // SAFETY: as the caller's contract with `alloc_zeroed`.
    let ptr = unsafe { LargeMapped.alloc_zeroed(layout) };
    allocated(ptr, layout.size());
    ptr
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // SAFETY: as the caller's contract with `realloc`.
    let moved_to = unsafe { LargeMapped.realloc(ptr, layout, new_size) };
    moved(ptr, moved_to);
    allocated(moved_to, new_size.saturating_sub(layout.size()));
    moved_to
  }
}

#[global_allocator]
static ALLOCATOR: Sampled = Sampled;

/// Sampled sites with their live bytes, the largest first, and their sum.
type Listed = (Vec<(String, u64)>, u64);

/// `by_site`'s sites, the largest first, and their sum.
fn listed(by_site: BTreeMap<String, u64>) -> Listed {
  let total = by_site.values().sum();
  let mut listed: Vec<(String, u64)> = by_site.into_iter().collect();
  listed.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
  (listed, total)
}

/// The live sampled heap by site now, and at its peak since the last [`reset_peak`].
fn live_now_and_at_peak() -> (Listed, Listed) {
  let (now, at_peak) = guarded(|| {
    LIVE
      .lock()
      .ok()
      .and_then(|live| {
        live
          .as_ref()
          .map(|live| (live.by_site(), live.at_peak.clone()))
      })
      .unwrap_or_default()
  })
  .unwrap_or_default();
  (listed(now), listed(at_peak))
}

/// Starts a new peak from the live heap now.
fn reset_peak() {
  let _ = guarded(|| {
    if let Ok(mut live) = LIVE.lock()
      && let Some(live) = live.as_mut()
    {
      live.peak = live.total;
      live.at_peak = live.by_site();
    }
  });
}

/// This process's resident set, KiB.
fn resident_kib() -> u64 {
  let out = std::process::Command::new("ps")
    .args(["-o", "rss=", "-p", &std::process::id().to_string()])
    .output()
    .unwrap();
  String::from_utf8_lossy(&out.stdout)
    .trim()
    .parse()
    .unwrap_or(0)
}

/// The daemon's memory gauges and counters, each shard's by its own name (`s<partition>:<row>`): a gauge of the
/// process's is read once a shard, never summed.
fn gauges(served: &common::hg::Served) -> BTreeMap<String, u64> {
  let report = served.client().daemon_memory_status().unwrap();
  let mut rows = BTreeMap::new();
  for shard in &report.shards {
    for row in &shard.refusals {
      rows.insert(format!("s{}:{}", shard.partition, row.kind), row.count);
    }
  }
  rows
}

/// The source pushed: `SLATES_HG_PUSH_SOURCE`'s repository, else a [`CHANGESETS`]-changeset `debugbuilddag` history
/// made in the test's directory.
fn source(served: &common::hg::Served) -> String {
  if let Ok(source) = std::env::var("SLATES_HG_PUSH_SOURCE") {
    return source;
  }
  let made = served.hg(".").args(["init", "source"]).output().unwrap();
  assert!(made.status.success(), "hg init");
  let built = served
    .hg("source")
    .args([
      "debugbuilddag",
      "--new-file",
      "--overwritten-file",
      &format!("+{CHANGESETS}"),
    ])
    .output()
    .unwrap();
  assert!(
    built.status.success(),
    "{}",
    String::from_utf8_lossy(&built.stderr)
  );
  format!("{}/source", served.base())
}

/// Prints the churn's sites, `when`: the system path's and the mapped path's bytes allocated, the largest first.
fn print_churn(when: &str, (system, mapped): &(Listed, Listed)) {
  for (path, (listed, total)) in [("system", system), ("mapped", mapped)] {
    println!("hg_fetch_heap: {when}: {path} churn sampled {total} B");
    for (site, bytes) in listed.iter().take(LISTED) {
      println!(
        "hg_fetch_heap: {when}: {path}   {:>5.1}% {:>14} B  {site}",
        *bytes as f64 * 100.0 / (*total).max(1) as f64,
        bytes
      );
    }
  }
}

/// Prints the live sites and the daemon's nonzero memory gauges, `when`.
fn print_held(when: &str, (listed, total): &Listed, rows: &BTreeMap<String, u64>) {
  println!("hg_fetch_heap: {when}: live sampled {total} B");
  for (site, bytes) in listed.iter().take(LISTED) {
    println!(
      "hg_fetch_heap: {when}:   {:>5.1}% {:>12} B  {site}",
      *bytes as f64 * 100.0 / (*total).max(1) as f64,
      bytes
    );
  }
  for (name, value) in rows {
    if name.contains(":mem.gauge.") && *value > 0 {
      println!("hg_fetch_heap: {when}: {name} = {value}");
    }
  }
}

/// Goal 93 (main's order of 2026-11-25: name what holds the 57k clone's 5.6 GB). Do: start a two-shard daemon holding a
/// changeset store, its HTTP edge on loopback; with the live-heap sampler on, push a history to it with stock `hg`
/// (Mercurial's own where `SLATES_HG_PUSH_SOURCE` names it, else a [`CHANGESETS`]-changeset `debugbuilddag` history),
/// then clone it whole. Expect: the push and the clone succeed. Prints the process's resident set after the push and
/// after the clone; the live sampled heap by site (the [`LISTED`] largest) after the push, at the clone's peak and as
/// the clone ends, with the daemon's memory gauges then; and each one's churn by site, the system path's and the mapped
/// path's apart (main's order of 2026-11-25: 64 GB of system allocations and 418 GB of fresh mapped pages at 57k).
#[test]
#[ignore = "a measurement (the hg fetch's memory): run by name, with SLATES_HG_PUSH_SOURCE for Mercurial's own history"]
fn what_holds_a_daemons_memory_once_a_whole_mercurial_history_is_fetched() {
  if !common::hg::has_hg() {
    eprintln!("SKIPPED: no `hg` on PATH");
    return;
  }
  let served = common::hg::serve("hg-fetch-heap", "");
  let url = served.url.clone();
  let source = source(&served);
  ON.store(true, Ordering::Relaxed);
  let pushed = served
    .hg(".")
    .args(["-R", &source, "push", "-q", "--new-branch", "-f", &url])
    .output()
    .unwrap();
  assert!(
    pushed.status.code().is_some_and(|code| code <= 1),
    "push: {}",
    String::from_utf8_lossy(&pushed.stderr)
  );
  let (after_push, _) = live_now_and_at_peak();
  let push_churn = take_churn();
  let before = resident_kib();
  reset_peak();
  let cloned = served
    .hg(".")
    .args(["clone", "-q", "-U", &url, "cloned"])
    .output()
    .unwrap();
  let (after_clone, clone_peak) = live_now_and_at_peak();
  let clone_churn = take_churn();
  ON.store(false, Ordering::Relaxed);
  let after = resident_kib();
  let rows = gauges(&served);
  served.stop();
  println!(
    "hg_fetch_heap: source {source}: resident KiB after the push {before}, after the clone {after}"
  );
  print_held("after the push", &after_push, &BTreeMap::new());
  print_held("at the clone's peak", &clone_peak, &BTreeMap::new());
  print_held("as the clone ends", &after_clone, &rows);
  print_churn("the push", &push_churn);
  print_churn("the clone", &clone_churn);
  assert!(
    cloned.status.success(),
    "clone: {}",
    String::from_utf8_lossy(&cloned.stderr)
  );
}
