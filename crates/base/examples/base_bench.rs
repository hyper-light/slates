//! The Phase 1 base-plane baseline (Phase 1 task 13; BENCHMARKS.md): listing cost per
//! directory size, the stat cost per entry inside it, a directory fingerprint, an open and
//! `fstat` (the drift check), a 4 KiB `pread`, and the two copy-up classes through an overlay
//! volume, all over the workspace's own `target/debug/deps` directory (read-only; the largest
//! directory a build leaves on this machine).
//!
//! `cargo run --release -p slates-base --example base_bench`

use std::path::PathBuf;
use std::time::Duration;

use slates_base::OsHost;
use slates_machine::bench::{Measurement, measure};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::HostClock;
use slates_vfs::host::{HostFs, HostKind};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the measurement budget per row (the same as the other benches).
const BUDGET: Duration = Duration::from_millis(500);
/// Format: the page size the bench builds its store with.
const PAGE: usize = 4096;
/// Shape: content region pages (64 MiB), room for the large-class pins of the copy-up rows.
const REGION_PAGES: usize = 16_384;
/// Derived: the large-file class boundary, one chunk window (the same as the volume tests).
const LARGE: u64 = 65_536;

fn report(name: &str, m: &Measurement) {
  report_tagged("ratchet", name, m);
}

/// A row whose cost follows what happens to be in the build directory (its size, its file
/// sizes), printed and never gated.
fn report_info(name: &str, m: &Measurement) {
  report_tagged("ratchet-info", name, m);
}

fn report_tagged(tag: &str, name: &str, m: &Measurement) {
  println!(
    "{tag}\t{}\t{}\t{}\t{}",
    key(name),
    m.interval.lower,
    m.median_ns(),
    m.interval.upper
  );
  println!(
    "{name}: median {} ns [{}, {}] p99 {} ns, {} samples × batch {}{}",
    m.median_ns(),
    m.interval.lower,
    m.interval.upper,
    m.p99_ns,
    m.samples,
    m.batch,
    if m.quick { " (quick)" } else { "" }
  );
}

fn key(name: &str) -> String {
  let words: Vec<&str> = name
    .split(|c: char| !c.is_ascii_alphanumeric())
    .filter(|w| !w.is_empty())
    .collect();
  format!("base.{}", words.join("_").to_ascii_lowercase())
}

fn store() -> Result<Store, Box<dyn std::error::Error>> {
  let mut arena = ChunkArena::new(PAGE);
  arena.add_region(Region::map(PAGE * REGION_PAGES, PAGE, false)?)?;
  Ok(Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: 1 << 16,
      max_inodes: 1 << 20,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 1 << 16,
      dir_cutover: 2,
    },
    arena,
  ))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let deps = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/deps");
  let Ok((mut host, root)) = OsHost::open_root(&deps) else {
    println!(
      "base bench: skipped — {} is not there (build the workspace first)",
      deps.display()
    );
    return Ok(());
  };
  let facts = host.facts(root)?;
  let listing = host.list(root)?;
  let entries = listing.len().max(1);
  println!(
    "target/debug/deps: {entries} entries, timestamp granularity {} ns",
    facts.timestamp_granularity_ns
  );
  report_info(
    &format!("list a {entries} entry directory per entry"),
    &scale(
      measure(
        || {
          std::hint::black_box(host.list(root).ok());
        },
        BUDGET,
      ),
      entries,
    ),
  );
  report(
    "fingerprint a directory",
    &measure(
      || {
        std::hint::black_box(host.fingerprint_dir(root).ok());
      },
      BUDGET,
    ),
  );
  let small = listing
    .iter()
    .filter(|e| e.kind == HostKind::File && e.fingerprint.size > 0 && e.fingerprint.size <= 4096)
    .min_by_key(|e| e.fingerprint.size)
    .cloned();
  if let Some(e) = &small {
    let name = e.name.clone();
    report(
      "open and fstat a file (the drift check)",
      &measure(
        || {
          if let Ok(f) = host.open_file(root, &name) {
            std::hint::black_box(host.fstat(f).ok());
            host.close_file(f);
          }
        },
        BUDGET,
      ),
    );
    let f = host.open_file(root, &name)?;
    let mut buf = vec![0u8; 4096];
    report(
      "pread up to 4 kib",
      &measure(
        || {
          std::hint::black_box(host.read_at(f, 0, &mut buf).ok());
        },
        BUDGET,
      ),
    );
    host.close_file(f);
  }
  // Copy-up rows over the small `target/debug` directory itself (a dozen entries), so a
  // sample pays the copy-up and not a 43k-entry listing.
  let debug = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
  let (mut small_host, debug_root) = OsHost::open_root(&debug)?;
  let debug_facts = small_host.facts(debug_root)?;
  let debug_listing = small_host.list(debug_root)?;
  let small = debug_listing
    .iter()
    .filter(|e| e.kind == HostKind::File && e.fingerprint.size > 0 && e.fingerprint.size <= 4096)
    .min_by_key(|e| e.fingerprint.size)
    .map(|e| e.name.clone());
  let large = debug_listing
    .iter()
    .filter(|e| e.kind == HostKind::File && e.fingerprint.size > 4 * LARGE)
    .min_by_key(|e| e.fingerprint.size)
    .map(|e| {
      println!(
        "large-class candidate: {} ({} bytes; the witness hashes it whole)",
        e.name, e.fingerprint.size
      );
      e.name.clone()
    });
  copy_up_rows(&mut small_host, debug_root, debug_facts, small, large)?;
  Ok(())
}

/// A measurement of one call over `n` entries, per entry.
fn scale(mut m: Measurement, n: usize) -> Measurement {
  let n = u64::try_from(n).unwrap_or(1).max(1);
  m.interval.lower /= n;
  m.interval.median /= n;
  m.interval.upper /= n;
  m.p99_ns /= n;
  m.min_ns /= n;
  m
}

/// Copy-up costs through an overlay volume over the directory: the small class reads the file
/// whole; the large class pins one window; each row rebuilds the volume so every sample copies
/// up afresh.
fn copy_up_rows(
  host: &mut OsHost,
  root: slates_vfs::host::HostDir,
  facts: slates_vfs::host::HostFacts,
  small: Option<Box<str>>,
  large: Option<Box<str>>,
) -> Result<(), Box<dyn std::error::Error>> {
  let mut store = store()?;
  for (label, name, gated) in [
    ("small class 4 kib file", small, true),
    ("large class one window", large, false),
  ] {
    let Some(name) = name else {
      println!("{label}: no candidate file in target/debug/deps");
      continue;
    };
    let path = format!("/{name}");
    let fresh = |store: &mut Store| -> Result<Volume, Box<dyn std::error::Error>> {
      Ok(Volume::create_overlay(
        store,
        VolumeConfig {
          prefix: 7,
          names: NameEquivalence::Exact,
          quota: Quota::Bounded { limit: 1 << 40 },
          journal_bytes: 1 << 16,
          clock: Box::new(HostClock::default()),
        },
        BaseConfig {
          root,
          facts,
          large_class_bytes: LARGE,
        },
      )?)
    };
    let mut vol = fresh(&mut store)?;
    let row = if gated { report } else { report_info };
    row(
      &format!("copy up {label} on first write"),
      &measure(
        || {
          let mut o = vol.with_host(host);
          if let Ok(l) = o.resolve(&mut store, &path) {
            std::hint::black_box(o.write(&mut store, l.inode, 0, b"x").ok());
          }
          // Rebuild for the next sample so the next write copies up again.
          if let Ok(v) = fresh(&mut store) {
            vol = v;
          }
        },
        BUDGET,
      ),
    );
  }
  Ok(())
}
