//! A daemon's allocation by site while it serves stock Mercurial (goal 93, the 410 MiB bar; main 01a20182's order of
//! 2026-11-23, (A)): its own binary, so the slates binary's allocator ([`LargeMapped`]) is the global one, wrapped in a
//! byte sampler that runs only while a row runs. An in-process daemon (a changeset store, the cleartext HTTP edge on
//! loopback, an HTTP token its issuer enrolled) is driven by real `hg` subprocesses, so what is sampled is the daemon's
//! alone: a push of a `hg debugbuilddag` history, a clone, a stream clone, and the clone verified. Each served row's
//! top sites are printed with their bytes and bytes a changeset, the figures the churn's cuts are ordered by; the test
//! holds each row to its success and its sampler to having counted. Where `hg` is absent it skips, and says so.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::print_stderr,
  clippy::disallowed_methods,
  clippy::disallowed_types,
  clippy::indexing_slicing,
  clippy::arithmetic_side_effects,
  clippy::cast_precision_loss
)]

mod common;

use std::alloc::{GlobalAlloc, Layout};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slates_mem::large::LargeMapped;

/// Shape: the changesets the pushed history holds, each a new file and the overwritten file rewritten: enough for the
/// push's and the clones' per-changeset work to show above their fixed costs.
const CHANGESETS: u64 = 400;
/// Shape: the bytes between samples.
const SAMPLE: u64 = 256 << 10;
/// Shape: the sites each row prints.
const LISTED: usize = 5;

/// Whether the sampler counts: only while a row runs.
static ON: AtomicBool = AtomicBool::new(false);
/// The bytes allocated while on.
static TOTAL: AtomicU64 = AtomicU64::new(0);
/// The sampled bytes by site.
static SITES: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

std::thread_local! {
  /// Set while this thread samples: its own allocations are not sampled.
  static GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// This thread's bytes since its last sample: a thread's sample charges its own allocation alone, never bytes another
  /// thread allocated (a global count charged a one-entry `push::phase_heads` 31 MB of other threads' bytes).
  static SINCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A backtrace's site: its innermost two frames outside the allocator, the standard library and this test.
fn site(trace: &str) -> String {
  let skip = |name: &str| {
    [
      "std::", "core::", "alloc::", "<alloc::", "<core::", "<std::", "__rust", "__rdl",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
      || [
        "slates_mem::large",
        "hg_push_heap::",
        "backtrace",
        "GlobalAlloc",
      ]
      .iter()
      .any(|part| name.contains(part))
  };
  let frames: Vec<&str> = trace
    .lines()
    .map(str::trim)
    .filter_map(|line| {
      let (number, name) = line.split_once(": ")?;
      number.chars().all(|c| c.is_ascii_digit()).then_some(name)
    })
    .filter(|name| !skip(name))
    .take(2)
    .collect();
  let short = |name: &str| {
    name
      .rsplit_once("::h")
      .map_or(name, |(head, _)| head)
      .to_owned()
  };
  match frames.as_slice() {
    [inner, caller, ..] => format!("{} <- {}", short(inner), short(caller)),
    [inner] => short(inner),
    [] => "(no frame)".to_owned(),
  }
}

/// Notes `size` bytes allocated: sampled every [`SAMPLE`] while on.
fn note(size: usize) {
  // The sampler's own allocations (a backtrace, its symbols: the first on a thread loads the binary's debug info, tens
  // of MB) are neither counted nor charged: counted, they were charged to the thread's next allocation (31 MB to a
  // one-entry `push::phase_heads`).
  if !ON.load(Ordering::Relaxed) || GUARD.with(std::cell::Cell::get) {
    return;
  }
  let size = size as u64;
  TOTAL.fetch_add(size, Ordering::Relaxed);
  let since = SINCE.with(|since| {
    let now = since.get().saturating_add(size);
    since.set(now);
    now
  });
  if since < SAMPLE || GUARD.with(|guard| guard.replace(true)) {
    return;
  }
  SINCE.with(|since| since.set(0));
  let key = site(&std::backtrace::Backtrace::force_capture().to_string());
  if let Ok(mut sites) = SITES.lock() {
    *sites
      .get_or_insert_with(HashMap::new)
      .entry(key)
      .or_insert(0) += since;
  }
  GUARD.with(|guard| guard.set(false));
}

/// `LargeMapped`, sampled.
struct Sampled;

// SAFETY: every call is `LargeMapped`'s, unchanged; the sampler only reads the layout's size before it.
unsafe impl GlobalAlloc for Sampled {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    note(layout.size());
    // SAFETY: as the caller's contract with `alloc`.
    unsafe { LargeMapped.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: as the caller's contract with `dealloc`.
    unsafe { LargeMapped.dealloc(ptr, layout) }
  }
  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    note(layout.size());
    // SAFETY: as the caller's contract with `alloc_zeroed`.
    unsafe { LargeMapped.alloc_zeroed(layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    note(new_size.saturating_sub(layout.size()));
    // SAFETY: as the caller's contract with `realloc`.
    unsafe { LargeMapped.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static ALLOCATOR: Sampled = Sampled;

/// One row's figures: what the daemon allocated while it ran, and its top sites.
struct Row {
  /// The row's name.
  name: &'static str,
  /// The bytes allocated.
  total: u64,
  /// The top sites, most bytes first.
  sites: Vec<(String, u64)>,
  /// The bytes sampled at a green's lend (`Greens::lend`, or the default `Green` a lend's restore made): none once a lend
  /// allocates nothing.
  lends: u64,
}

/// Runs `command` as row `name`, the sampler on for its length: its figures; panics where it fails.
fn row(name: &'static str, command: &mut Command) -> Row {
  if let Ok(mut sites) = SITES.lock() {
    *sites = None;
  }
  TOTAL.store(0, Ordering::Relaxed);
  ON.store(true, Ordering::Relaxed);
  let output = command.output().expect("hg ran");
  ON.store(false, Ordering::Relaxed);
  assert!(
    output.status.success(),
    "{name}: {}\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  let mut sites: Vec<(String, u64)> = SITES
    .lock()
    .ok()
    .and_then(|sites| sites.clone())
    .unwrap_or_default()
    .into_iter()
    .collect();
  let lends = sites
    .iter()
    .filter(|(site, _)| {
      site.contains("Greens>::lend") || site.contains("Green as core::default::Default")
    })
    .map(|(_, bytes)| *bytes)
    .sum();
  sites.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
  sites.truncate(LISTED);
  Row {
    name,
    total: TOTAL.load(Ordering::Relaxed),
    sites,
    lends,
  }
}

/// Prints `row`'s figures, each a changeset of [`CHANGESETS`].
fn print(row: &Row) {
  println!(
    "hg_push_heap: {}: {} B allocated, {} B a changeset",
    row.name,
    row.total,
    row.total / CHANGESETS
  );
  for (name, bytes) in &row.sites {
    println!(
      "hg_push_heap: {}:   {:>5.1}% {:>9} B a changeset  {name}",
      row.name,
      *bytes as f64 * 100.0 / row.total.max(1) as f64,
      bytes / CHANGESETS
    );
  }
}

/// Goal 93 (main's order of 2026-11-23, (A): the lasting regression test of a daemon's churn under stock Mercurial).
/// Do: start a two-shard daemon holding a changeset store, its HTTP edge on loopback; create a Mercurial repository and
/// enroll an HTTP token for this process's principal; build a history of [`CHANGESETS`] changesets with
/// `hg debugbuilddag`; push it, clone it, stream-clone it and verify the clone with stock `hg`, the daemon's allocation
/// sampled by site for each row. Expect every row to succeed, and the push, the clone and the stream clone each to
/// have allocated (the sampler counts); each row's top sites printed with their bytes a changeset. Skips, saying so,
/// where `hg` is not installed.
#[test]
fn a_daemons_allocation_by_site_under_stock_mercurial() {
  if !common::hg::has_hg() {
    eprintln!("SKIPPED a_daemons_allocation_by_site_under_stock_mercurial: no `hg` on PATH");
    return;
  }
  let served = common::hg::serve("hg-push-heap", "");
  let url = served.url.clone();
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
  // The rows, each sampled.
  let push = row(
    "push",
    served
      .hg("source")
      .args(["push", "-q", "--new-branch", "-f", &url]),
  );
  let clone = row(
    "clone",
    served.hg(".").args(["clone", "-q", &url, "cloned"]),
  );
  let stream = row(
    "stream-clone",
    served
      .hg(".")
      .args(["clone", "-q", "--stream", &url, "streamed"]),
  );
  let verified = served.hg("cloned").args(["verify", "-q"]).output().unwrap();
  served.stop();
  for row in [&push, &clone, &stream] {
    print(row);
  }
  assert!(
    verified.status.success(),
    "the clone verifies: {}",
    String::from_utf8_lossy(&verified.stderr)
  );
  for row in [&push, &clone, &stream] {
    assert!(
      row.total > 0 && !row.sites.is_empty(),
      "{}: the sampler counted",
      row.name
    );
    // Main's order of 2026-11-23: a green's lend allocates nothing (no default `Green`, no box a lend). Red before:
    // the push's 86%, 313 KB a changeset of `Green::default`'s set hash and `Greens::lend`'s box.
    assert_eq!(
      row.lends / CHANGESETS,
      0,
      "{}: a green's lend allocated {} B a changeset",
      row.name,
      row.lends / CHANGESETS
    );
  }
}
