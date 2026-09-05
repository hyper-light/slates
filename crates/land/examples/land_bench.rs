//! The landing baselines (Phase 1 task 13; T-1.17): the engine's own cost per entry over the
//! simulated host (plan, validate, write, advance), and, when `SLATES_TEST_RAMDIR` names a
//! RAM-backed directory, a 10k-entry delta landed into a 10^6-entry tree by the OS writer
//! against `cp -r` of the same delta, with the ramp's settled depth recorded. Without the
//! directory the OS rows are skipped loudly; nothing is written outside it.
//!
//! Rows are `ratchet\t<key>\t<lower>\t<median>\t<upper>` in nanoseconds per entry (the
//! bootstrap interval of the best-of-N runs), as `cargo xtask ratchet` reads them.
// Bench harness code: an unwrap here is a failed run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use slates_land::engine::{
  Audit, LandingRefusal, LandingRequest, LandingState, LandingTarget, Unobserved, land,
};
use slates_land::grant::{GrantScope, Grants, Leases, Surface};
use slates_land::manifest::Filter;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{HostFs, LandFs};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: pages in the bench region (64 MiB of content).
const REGION_PAGES: usize = 16_384;
/// Shape: the large-file class boundary, one chunk window.
const LARGE: u64 = 65_536;
/// Shape: the lease and grant terms (monotonic ns).
const TERM_NS: u64 = 1_000_000_000_000;
/// Shape: runs per row; the interval is over these.
const RUNS: usize = 5;
/// Shape: the delta the simulated rows land (entries).
const SIM_DELTA: usize = 1_000;
/// Shape: the delta the OS rows land (entries), T-1.17's 10k.
const OS_DELTA: usize = 10_000;
/// Shape: the base tree the OS rows land into (entries), T-1.17's 10^6.
const OS_BASE: usize = 1_000_000;
/// Shape: files per directory of the generated trees.
const PER_DIR: usize = 100;
/// Format: the environment variable naming the RAM-backed directory.
const RAM_DIR: &str = "SLATES_TEST_RAMDIR";

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: 1 << 18,
      max_inodes: 1 << 18,
      max_chunks: 1 << 18,
      max_dir_blocks: 1 << 18,
      dir_cutover: 4,
    },
    arena,
  )
}

fn config() -> VolumeConfig {
  VolumeConfig {
    prefix: 7,
    names: NameEquivalence::Exact,
    quota: Quota::Bounded { limit: 1 << 32 },
    journal_bytes: 1 << 24,
    clock: Box::new(StepClock::new(1_000_000, 1_000)),
  }
}

fn request(id: u64) -> LandingRequest {
  LandingRequest {
    landing_id: id,
    holder: 1,
    grant: None,
    filter: Filter::default(),
    now_ns: 1,
    lease_term_ns: TERM_NS,
    media_durability: false,
    large_class_bytes: LARGE,
    cores: 2,
    max_depth: 8,
    variance_permille: 100,
    costs: None,
    target_entries: None,
  }
}

/// The delta's edits: `delta` files replaced across the first directories of the tree.
fn edit_delta<H: HostFs>(vol: &mut Volume, host: &mut H, store: &mut Store, delta: usize) {
  for f in 0..delta {
    let d = f / PER_DIR;
    let path = format!("/d{d}/f{}", f % PER_DIR);
    let located = vol.with_host(host).resolve(store, &path).unwrap();
    let mut o = vol.with_host(host);
    o.truncate(store, located.inode, 0).unwrap();
    o.write(store, located.inode, 0, format!("edited {f}").as_bytes())
      .unwrap();
  }
}

/// One granted landing; returns the report.
fn land_once<H: LandFs>(
  host: &mut H,
  target: &LandingTarget,
  vol: &mut Volume,
  store: &mut Store,
  id: u64,
) -> slates_land::engine::LandingReport {
  let mut grants = Grants::default();
  let mut leases = Leases::default();
  let mut audit = Audit::new(1 << 10);
  let mut req = request(id);
  let presented = match land(
    host,
    target,
    vol,
    store,
    &mut grants,
    &mut leases,
    &mut audit,
    &req,
    &mut Unobserved,
  ) {
    Err(LandingRefusal::GrantRequired(p)) => p,
    other => panic!("{other:?}"),
  };
  req.grant = Some(grants.issue(
    Surface::Cli,
    presented.manifest.hash,
    GrantScope::Once,
    1,
    TERM_NS,
  ));
  land(
    host,
    target,
    vol,
    store,
    &mut grants,
    &mut leases,
    &mut audit,
    &req,
    &mut Unobserved,
  )
  .unwrap()
}

/// The bootstrap-free interval of `samples`: min, median, max (N is small and shown).
fn row(key: &str, mut samples: Vec<u64>, gated: bool) {
  samples.sort_unstable();
  let lower = samples[0];
  let median = samples[samples.len() / 2];
  let upper = samples[samples.len() - 1];
  let tag = if gated { "ratchet" } else { "ratchet-info" };
  println!("{tag}\t{key}\t{lower}\t{median}\t{upper}");
  println!(
    "  {key}: {median} ns/entry [{lower}, {upper}] over {} runs: {samples:?}",
    samples.len()
  );
}

fn ns_per_entry(elapsed: std::time::Duration, entries: usize) -> u64 {
  u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX) / u64::try_from(entries.max(1)).unwrap_or(1)
}

/// The simulated rows: a 1,000-entry delta into a 100,000-entry base, engine cost only.
fn sim_rows() {
  let mut plan = Vec::new();
  let mut total = Vec::new();
  let mut second = Vec::new();
  for run in 0..RUNS {
    let mut host = SimHost::new();
    for d in 0..(100_000 / PER_DIR) {
      host.mkdir(&format!("/d{d}"));
      for f in 0..PER_DIR {
        host.replace_file(&format!("/d{d}/f{f}"), b"base");
      }
    }
    let mut store = store();
    let root = host.root();
    let facts = host.facts(root).unwrap();
    let mut vol = Volume::create_overlay(
      &mut store,
      config(),
      BaseConfig {
        root,
        facts,
        large_class_bytes: LARGE,
      },
    )
    .unwrap();
    edit_delta(&mut vol, &mut host, &mut store, SIM_DELTA);
    let target = LandingTarget {
      dir: host.root(),
      key: "/".into(),
      parent: None,
    };
    let started = Instant::now();
    let manifest =
      slates_land::manifest::plan(&mut vol, &mut store, &mut host, &Filter::default()).unwrap();
    plan.push(ns_per_entry(started.elapsed(), manifest.entries.len()));
    let started = Instant::now();
    let report = land_once(
      &mut host,
      &target,
      &mut vol,
      &mut store,
      u64::try_from(run).unwrap(),
    );
    total.push(ns_per_entry(started.elapsed(), SIM_DELTA));
    assert_eq!(report.state, LandingState::Done);
    assert_eq!(report.written, SIM_DELTA);
    // The idempotent second landing: plan of the empty delta.
    let started = Instant::now();
    let again =
      slates_land::manifest::plan(&mut vol, &mut store, &mut host, &Filter::default()).unwrap();
    second.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    assert!(again.entries.is_empty());
  }
  row("land.plan_per_entry_sim_1000_of_100000", plan, true);
  row(
    "land.land_per_entry_sim_1000_of_100000_engine_only",
    total,
    true,
  );
  row("land.replan_after_landing_nothing_diverged", second, true);
}

#[cfg(unix)]
mod os_rows {
  use super::*;
  use slates_land::os::OsLand;

  #[allow(clippy::disallowed_methods)]
  fn build_tree(root: &Path, entries: usize) {
    for d in 0..(entries / PER_DIR) {
      let dir = root.join(format!("d{d}"));
      std::fs::create_dir_all(&dir).unwrap();
      for f in 0..PER_DIR {
        std::fs::write(dir.join(format!("f{f}")), b"base").unwrap();
      }
    }
  }

  /// `cp -r` of the same delta (the edited files, written to a sibling tree) as the comparison.
  #[allow(clippy::disallowed_methods)]
  fn cp_r_delta(src: &Path, dst: &Path) -> std::time::Duration {
    let started = Instant::now();
    let status = Command::new("cp")
      .arg("-r")
      .arg(src)
      .arg(dst)
      .status()
      .unwrap();
    assert!(status.success());
    started.elapsed()
  }

  #[allow(clippy::disallowed_methods)]
  pub(super) fn run(ram: &Path) {
    let ws = ram.join(format!("slates-land-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();
    let base = ws.join("base");
    println!("building a {OS_BASE}-entry tree under {}", base.display());
    build_tree(&base, OS_BASE);
    // The delta as a plain tree for cp -r.
    let delta_src = ws.join("delta");
    for f in 0..OS_DELTA {
      let d = f / PER_DIR;
      std::fs::create_dir_all(delta_src.join(format!("d{d}"))).unwrap();
      std::fs::write(
        delta_src.join(format!("d{d}/f{}", f % PER_DIR)),
        format!("edited {f}"),
      )
      .unwrap();
    }
    let mut landed = Vec::new();
    let mut copied = Vec::new();
    let mut depth = 0u32;
    for run in 0..RUNS {
      // Reset the edited files.
      for f in 0..OS_DELTA {
        let d = f / PER_DIR;
        std::fs::write(base.join(format!("d{d}/f{}", f % PER_DIR)), b"base").unwrap();
      }
      let (mut host, target) = OsLand::open_target(&base).unwrap();
      let mut store = store();
      let facts = host.facts(target.dir).unwrap();
      let mut vol = Volume::create_overlay(
        &mut store,
        config(),
        BaseConfig {
          root: target.dir,
          facts,
          large_class_bytes: LARGE,
        },
      )
      .unwrap();
      edit_delta(&mut vol, &mut host, &mut store, OS_DELTA);
      let started = Instant::now();
      let report = land_once(
        &mut host,
        &target,
        &mut vol,
        &mut store,
        u64::try_from(run).unwrap(),
      );
      landed.push(ns_per_entry(started.elapsed(), OS_DELTA));
      assert_eq!(report.state, LandingState::Done, "{report:?}");
      assert_eq!(report.written, OS_DELTA);
      depth = report.ramp_depth;
      let copy_dst = ws.join(format!("copy{run}"));
      copied.push(ns_per_entry(cp_r_delta(&delta_src, &copy_dst), OS_DELTA));
    }
    row("land.land_per_entry_os_10000_into_1000000", landed, true);
    row("land.cp_r_per_entry_of_the_same_delta", copied, false);
    println!("  ramp settled depth (recorded, entries run one at a time in Phase 1): {depth}");
    let _ = std::fs::remove_dir_all(&ws);
  }
}

fn main() {
  sim_rows();
  match std::env::var_os(RAM_DIR).map(PathBuf::from) {
    #[cfg(unix)]
    Some(ram) => os_rows::run(&ram),
    #[cfg(not(unix))]
    Some(_) => println!("land bench: the OS rows run on Unix only"),
    None => println!(
      "land bench: OS rows skipped — set {RAM_DIR} to a RAM-backed directory (CI Linux: /dev/shm)"
    ),
  }
}
