//! What a daemon's push allocates on the system allocator's path, by site, size and how long each block lives (goal 93,
//! the 410 MiB bar; main 01a20182's order of 2026-11-23): its own binary, so the slates binary's allocator
//! ([`LargeMapped`]) is the global one, wrapped in a sampler that runs only while the push runs.
//!
//! CI's hg matrix (arena-11, 193757bdf, x86) left glibc's arena at 1,009 MB after push-all, 588 MB of it free blocks
//! glibc kept (`mallinfo2`'s `fordblks`) beside about 420 MB in use. A block under glibc's mapping threshold comes from
//! its arena, and one that outlives the work around it pins the arena's top, so the free blocks around it stay. This
//! test names which sites those blocks are: each sampled block is classed by its size (under glibc's mapping threshold,
//! from it to slates-mem's mapped floor on x86_64, or mapped by slates-mem whole there) and by its life (freed within a millisecond,
//! within a second, later, or alive when the push ends), so a site's blocks that live past a turn are told from its
//! temporaries.
//!
//! By default it pushes a [`CHANGESETS`]-changeset history `hg debugbuilddag` makes, which proves the sampler. With
//! `SLATES_HG_PUSH_SOURCE` naming a Mercurial repository (CI's push-all pushes Mercurial's own history, 57,054
//! changesets), it pushes that one. `#[ignore]`d: a measurement, run by name.
//!
//! First read (2026-11-23, MacBook arm64, `CARGO_PROFILE_DEV_OPT_LEVEL=2`, Mercurial's history at 57,036 changesets,
//! sizes classed at x86_64's): the push allocated 17.5 GB in 152 s. Under the 256 KiB floor, 4.18 GB of sampled bytes
//! lived a second or more or past the push, 83% of it the store owner's text cache (`slates_store::texts::apply_into`
//! from `Store::make` and `Store::text_at`, each text its own buffer, kept seconds). In [128 KiB, 256 KiB), 3.52 GB
//! churned, mostly whole texts the hg crate rebuilds (`delta::apply_into` from `HgIndex::revision_text` and the
//! intake's `rebuilt`) and the same cache. Alive at the push's end under the floor: 366 MB, the cache's 195 MB first.
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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use slates_mem::large::LargeMapped;

/// Shape: the default history's changesets, each a new file and the overwritten file rewritten.
const CHANGESETS: u64 = 400;
/// Shape: the bytes between samples, on each thread.
const SAMPLE: u64 = 64 << 10;
/// Shape: the sites each table prints.
const LISTED: usize = 12;
/// Format: glibc's default mapping threshold (`M_MMAP_THRESHOLD`'s `DEFAULT_MMAP_THRESHOLD_MIN`, 128 KiB, malloc(3)),
/// under which a block comes from an arena. glibc raises it to the size of a mapped block freed, up to 32 MiB on 64-bit
/// hosts, so blocks past it may come from the arena too once such a block was freed.
const GLIBC_MMAP_THRESHOLD: usize = 128 << 10;
/// Format: x86_64's page, 4 KiB: CI's push-all runs there, where slates-mem maps a block of
/// [`slates_mem::large::FLOOR_PAGES`] of them (256 KiB) or more itself and glibc's arena serves only what is smaller. The
/// size classes are x86_64's on every host, so a run on Apple silicon (whose floor is 1 MiB) reads what CI's glibc sees.
const X86_64_PAGE: usize = 4 << 10;
/// Format: the life classes' upper bounds, nanoseconds: a millisecond (within a step), a second (within a turn of a
/// push's admission), and later.
const LIVES_NS: [u64; 2] = [1_000_000, 1_000_000_000];
/// Format: the life classes' names, the last for blocks alive when the push ended.
const LIFE_NAMES: [&str; 4] = ["<1ms", "<1s", ">=1s", "live"];
/// Format: the size classes' names.
const SIZE_NAMES: [&str; 3] = ["<128K", "<floor", "mapped"];

/// Whether the sampler counts: only while the push runs.
static ON: AtomicBool = AtomicBool::new(false);
/// The bytes allocated while on.
static TOTAL: AtomicU64 = AtomicU64::new(0);
/// The sampled blocks alive, by address: each one's site, size class, the bytes it stands for and when it was made.
static LIVE: Mutex<Option<HashMap<usize, Block>>> = Mutex::new(None);
/// The sampled bytes by site, size class and life class.
static SITES: Mutex<Option<HashMap<String, Classes>>> = Mutex::new(None);
/// The clock's origin.
static ORIGIN: Mutex<Option<Instant>> = Mutex::new(None);

/// A sampled block.
#[derive(Clone)]
struct Block {
  site: String,
  size_class: usize,
  bytes: u64,
  made_ns: u64,
}

/// Bytes by size class and life class.
type Classes = [[u64; 4]; 3];

std::thread_local! {
  /// Set while this thread samples: its own allocations are not sampled.
  static GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// This thread's bytes since its last sample.
  static SINCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Nanoseconds since the clock's origin.
fn now_ns() -> u64 {
  let origin = *ORIGIN.lock().unwrap().get_or_insert_with(Instant::now);
  u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A backtrace's site: its innermost three frames outside the allocator, the standard library and this test.
fn site(trace: &str) -> String {
  let skip = |name: &str| {
    [
      "std::", "core::", "alloc::", "<alloc::", "<core::", "<std::", "__rust", "__rdl",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
      || [
        "slates_mem::large",
        "hg_push_lifetimes::",
        "backtrace",
        "GlobalAlloc",
      ]
      .iter()
      .any(|part| name.contains(part))
  };
  let short = |name: &str| {
    let name = name.rsplit_once("::h").map_or(name, |(head, _)| head);
    name.chars().take(160).collect::<String>()
  };
  let frames: Vec<String> = trace
    .lines()
    .map(str::trim)
    .filter_map(|line| {
      let (number, name) = line.split_once(": ")?;
      number.chars().all(|c| c.is_ascii_digit()).then_some(name)
    })
    .filter(|name| !skip(name))
    .take(3)
    .map(short)
    .collect();
  if frames.is_empty() {
    "(no frame)".to_owned()
  } else {
    frames.join(" <- ")
  }
}

/// The size class of a block of `size` bytes.
fn size_class(size: usize) -> usize {
  if size >= slates_mem::large::FLOOR_PAGES * X86_64_PAGE {
    2
  } else if size >= GLIBC_MMAP_THRESHOLD {
    1
  } else {
    0
  }
}

/// The life class of a block that lived `lived_ns`.
fn life_class(lived_ns: u64) -> usize {
  LIVES_NS
    .iter()
    .position(|bound| lived_ns < *bound)
    .unwrap_or(LIVES_NS.len())
}

/// Adds `bytes` to `site`'s classes.
fn charge(site: &str, size_class: usize, life: usize, bytes: u64) {
  if let Ok(mut sites) = SITES.lock() {
    sites
      .get_or_insert_with(HashMap::new)
      .entry(site.to_owned())
      .or_insert([[0; 4]; 3])[size_class][life] += bytes;
  }
}

/// Notes a block at `ptr` of `size` bytes allocated: sampled every [`SAMPLE`] bytes a thread while on.
fn made(ptr: *mut u8, size: usize) {
  if ptr.is_null() || !ON.load(Ordering::Relaxed) || GUARD.with(std::cell::Cell::get) {
    return;
  }
  let size64 = size as u64;
  TOTAL.fetch_add(size64, Ordering::Relaxed);
  let since = SINCE.with(|since| {
    let now = since.get().saturating_add(size64);
    since.set(now);
    now
  });
  if since < SAMPLE || GUARD.with(|guard| guard.replace(true)) {
    return;
  }
  SINCE.with(|since| since.set(0));
  let block = Block {
    site: site(&std::backtrace::Backtrace::force_capture().to_string()),
    size_class: size_class(size),
    bytes: since,
    made_ns: now_ns(),
  };
  if let Ok(mut live) = LIVE.lock() {
    live
      .get_or_insert_with(HashMap::new)
      .insert(ptr as usize, block);
  }
  GUARD.with(|guard| guard.set(false));
}

/// Notes the block at `ptr` freed: a sampled one's bytes go to its life class.
fn freed(ptr: *mut u8) {
  if GUARD.with(std::cell::Cell::get) {
    return;
  }
  GUARD.with(|guard| guard.set(true));
  let block = LIVE
    .lock()
    .ok()
    .and_then(|mut live| live.as_mut()?.remove(&(ptr as usize)));
  if let Some(block) = block {
    let lived = now_ns().saturating_sub(block.made_ns);
    charge(
      &block.site,
      block.size_class,
      life_class(lived),
      block.bytes,
    );
  }
  GUARD.with(|guard| guard.set(false));
}

/// `LargeMapped`, sampled.
struct Sampled;

// SAFETY: every call is `LargeMapped`'s, unchanged; the sampler only reads the layout's size and the block's address.
unsafe impl GlobalAlloc for Sampled {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    // SAFETY: as the caller's contract with `alloc`.
    let ptr = unsafe { LargeMapped.alloc(layout) };
    made(ptr, layout.size());
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
    made(ptr, layout.size());
    ptr
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // A reallocation ends the old block's life and makes a new one of its new size.
    freed(ptr);
    // SAFETY: as the caller's contract with `realloc`.
    let moved = unsafe { LargeMapped.realloc(ptr, layout, new_size) };
    made(moved, new_size);
    moved
  }
}

#[global_allocator]
static ALLOCATOR: Sampled = Sampled;

/// Prints the sites with the most bytes in `size_classes` and `lives`, under `title`, with the total of every site's.
fn table(title: &str, sites: &[(String, Classes)], size_classes: &[usize], lives: &[usize]) -> u64 {
  let sum = |classes: &Classes| -> u64 {
    size_classes
      .iter()
      .flat_map(|size| lives.iter().map(move |life| classes[*size][*life]))
      .sum()
  };
  let mut ranked: Vec<(&String, u64, &Classes)> = sites
    .iter()
    .map(|(site, classes)| (site, sum(classes), classes))
    .filter(|(_, bytes, _)| *bytes > 0)
    .collect();
  ranked.sort_by(|left, right| right.1.cmp(&left.1));
  let total: u64 = ranked.iter().map(|(_, bytes, _)| bytes).sum();
  println!("hg_push_lifetimes: == {title}: {total} B sampled");
  for (site, bytes, classes) in ranked.iter().take(LISTED) {
    let lives: Vec<String> = (0..4)
      .map(|life| {
        let bytes: u64 = size_classes.iter().map(|size| classes[*size][life]).sum();
        format!("{} {}", LIFE_NAMES[life], bytes)
      })
      .collect();
    println!(
      "hg_push_lifetimes:   {:>5.1}% {bytes:>12} B [{}]  {site}",
      *bytes as f64 * 100.0 / total.max(1) as f64,
      lives.join(", ")
    );
  }
  total
}

/// Goal 93, the 410 MiB bar (main's order of 2026-11-23: attribute glibc's kept bytes before cutting). Do: start a
/// two-shard daemon holding a changeset store with its HTTP edge on loopback; push a Mercurial history to it with stock
/// `hg` (Mercurial's own where `SLATES_HG_PUSH_SOURCE` names it, else a [`CHANGESETS`]-changeset `debugbuilddag`
/// history), the daemon's allocation sampled by site, size and life. Expect the push to succeed and the sampler to have
/// counted; print every size class's bytes by life, and the sites whose blocks under slates-mem's mapped floor
/// outlived a second, or were alive at the push's end, beside the sites of the temporaries. Skips, saying so, where
/// `hg` is not installed.
#[test]
#[ignore = "a measurement (goal 93's memory term): run by name, with SLATES_HG_PUSH_SOURCE for Mercurial's own history"]
fn a_pushs_allocation_by_site_size_and_life_under_stock_mercurial() {
  if !common::hg::has_hg() {
    eprintln!(
      "SKIPPED a_pushs_allocation_by_site_size_and_life_under_stock_mercurial: no `hg` on PATH"
    );
    return;
  }
  let served = common::hg::serve("hg-push-lives", "");
  let url = served.url.clone();
  let source = match std::env::var("SLATES_HG_PUSH_SOURCE") {
    Ok(source) => source,
    Err(_) => {
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
  };
  let _ = now_ns();
  TOTAL.store(0, Ordering::Relaxed);
  let began = Instant::now();
  ON.store(true, Ordering::Relaxed);
  let output = served
    .hg(".")
    .args(["-R", &source, "push", "-q", "--new-branch", "-f", &url])
    .output()
    .expect("hg ran");
  ON.store(false, Ordering::Relaxed);
  let took = began.elapsed();
  let ended_ns = now_ns();
  assert!(
    output.status.code().is_some_and(|code| code <= 1),
    "push: {}\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  // The blocks still alive at the push's end, charged to the last life class: taken out of the table, so a later free
  // of one by the daemon's threads charges nothing twice.
  let alive: Vec<Block> = LIVE
    .lock()
    .unwrap()
    .as_mut()
    .map(|live| live.drain().map(|(_, block)| block).collect())
    .unwrap_or_default();
  for block in &alive {
    if block.made_ns <= ended_ns {
      charge(
        &block.site,
        block.size_class,
        LIFE_NAMES.len() - 1,
        block.bytes,
      );
    }
  }
  let sites: Vec<(String, Classes)> = SITES
    .lock()
    .unwrap()
    .as_ref()
    .map(|sites| {
      sites
        .iter()
        .map(|(site, classes)| (site.clone(), *classes))
        .collect()
    })
    .unwrap_or_default();
  served.stop();
  let total = TOTAL.load(Ordering::Relaxed);
  println!(
    "hg_push_lifetimes: source {source}: {total} B allocated in {took:?}; x86_64's mapped floor {} B (this host's {} B), glibc threshold {} B",
    slates_mem::large::FLOOR_PAGES * X86_64_PAGE,
    slates_mem::large::floor(),
    GLIBC_MMAP_THRESHOLD
  );
  for (size, name) in SIZE_NAMES.iter().enumerate() {
    let lives: Vec<String> = (0..4)
      .map(|life| {
        let bytes: u64 = sites.iter().map(|(_, classes)| classes[size][life]).sum();
        format!("{} {}", LIFE_NAMES[life], bytes)
      })
      .collect();
    println!("hg_push_lifetimes: size {name}: [{}]", lives.join(", "));
  }
  let kept = table(
    "under the mapped floor, alive a second or more or at the end",
    &sites,
    &[0, 1],
    &[2, 3],
  );
  // Main's read of arena-12: glibc's kept bytes go once blocks of 128 KiB and up are mapped, so the sites churning
  // blocks in [128 KiB, the floor) are named apart, every life.
  table(
    "in [128 KiB, x86_64's floor), every life",
    &sites,
    &[1],
    &[0, 1, 2, 3],
  );
  // What is still held when the push ends: the live bytes the 410 MiB bar meets next.
  table(
    "under the mapped floor, alive at the push's end",
    &sites,
    &[0, 1],
    &[3],
  );
  table(
    "under the mapped floor, freed within a second",
    &sites,
    &[0, 1],
    &[0, 1],
  );
  table(
    "x86_64's mapped size, every life",
    &sites,
    &[2],
    &[0, 1, 2, 3],
  );
  assert!(total > 0 && !sites.is_empty(), "the sampler counted");
  println!("hg_push_lifetimes: kept-class bytes sampled {kept}");
}
