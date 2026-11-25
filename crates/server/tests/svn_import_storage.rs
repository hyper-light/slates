//! A Subversion import's bytes on the node's store, against FSFS's for the same dump (main 01a20182's order of
//! 2026-11-24, item 4 of the note of 11-21 21:50: "an order of magnitude faster", after C and R; a measurement only,
//! bytes, no seconds). An in-process daemon over a changeset store imports the dump `SLATES_SVN_IMPORT_DUMP` names into
//! a Subversion repository; the store's directory is measured before the import and after the daemon and the store's
//! owner stop, and the process's resident set and the daemon's memory gauges after the import. FSFS's side is stock
//! `svnadmin load` of the same dump, run beside it. `#[ignore]`d: a measurement, run by name.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::print_stderr,
  clippy::disallowed_methods
)]

mod common;

use std::collections::BTreeMap;

/// Shape: the store's sizing: the tests' memory, extent and fanout, its disk a cap past any dump measured here (the
/// engine's file grows to it, never made at it).
const SIZING: slates_store::store::Sizing = slates_store::store::Sizing {
  memory: 8 << 20,
  disk: 64 << 30,
  extent_bytes: 128 << 10,
  fanout: 8,
};

/// Shape: the gauges printed after the import, the largest first.
const LISTED: usize = 16;

/// This process's resident set, KiB (`ps -o rss=`): the daemon is in-process.
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

/// `dir`'s bytes as `du -sk` (allocated) and `du -sk -A` (apparent, macOS) give them, KiB.
fn du_kib(dir: &str) -> (u64, u64) {
  let kib = |args: &[&str]| {
    let out = std::process::Command::new("du")
      .args(args)
      .arg(dir)
      .output()
      .unwrap();
    String::from_utf8_lossy(&out.stdout)
      .split_whitespace()
      .next()
      .and_then(|kib| kib.parse().ok())
      .unwrap_or(0)
  };
  (kib(&["-sk"]), kib(&["-sk", "-A"]))
}

/// This process's physical footprint, KiB, on macOS (`phys_footprint`, `Facts::footprint_now`); `None` elsewhere, where
/// the resident set is the honest reading. On macOS a purge's pages leave the footprint at once and stay in the resident
/// set until the kernel takes them (goal 93 P7, main's ruling of 2026-11-25: judge the arms on this there).
fn footprint_kib() -> Option<u64> {
  slates_machine::facts::Facts::footprint_now().map(|bytes| bytes >> 10)
}

/// The daemon's memory gauges, each shard's by its own name (`s<partition>:<row>`).
fn gauges(client: &mut slates_client::Client) -> BTreeMap<String, u64> {
  let report = client.daemon_memory_status().unwrap();
  let mut rows = BTreeMap::new();
  for shard in &report.shards {
    for row in &shard.refusals {
      rows.insert(format!("s{}:{}", shard.partition, row.kind), row.count);
    }
  }
  rows
}

/// Main's order of 2026-11-24 (the svn storage A/B against FSFS). Do: start a two-shard daemon over a changeset store
/// ([`SIZING`]); make a Subversion repository; measure the store's directory; import the dump `SLATES_SVN_IMPORT_DUMP`
/// names; read the process's resident set and the daemon's gauges; stop the daemon and the store's owner and measure the
/// store's directory again. Expect: the import answered. Prints the store's bytes before and after (allocated and
/// apparent), their difference (the repository's, the store holding nothing else of size), and the resident figures.
#[test]
#[ignore = "a measurement (the svn storage A/B): run by name, with SLATES_SVN_IMPORT_DUMP naming the dump"]
fn a_subversion_imports_bytes_on_the_store_are_measured_against_fsfs() {
  let Ok(dump) = std::env::var("SLATES_SVN_IMPORT_DUMP") else {
    eprintln!(
      "SKIPPED a_subversion_imports_bytes_on_the_store_are_measured_against_fsfs: no SLATES_SVN_IMPORT_DUMP"
    );
    return;
  };
  let served = common::hg::serve_sized("svn-storage", "", SIZING);
  let mut client = served.client();
  client.create_repository(&dumped_repository()).unwrap();
  let store = format!("{}/store", served.base());
  let before = du_kib(&store);
  let resident_before = resident_kib();
  let footprint_before = footprint_kib();
  let imported = client.import_repository("dumped", &dump);
  let measured = Measured {
    resident: (resident_before, resident_kib()),
    footprint: (footprint_before, footprint_kib()),
    rows: gauges(&mut client),
    store: (before, du_kib(&store)),
  };
  let settled = settled_reading(&mut client);
  let after = served.stop_measuring(|base| du_kib(&format!("{base}/store")));
  println!("svn_import_storage: dump {dump}: {imported:?}");
  print_measured(&measured, after);
  print_settled(&settled);
  assert!(imported.is_ok(), "the import answered: {imported:?}");
}

/// Shape: the longest the second reading waits for the import's deferred frees to be released by a publication.
const SETTLE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);
/// Shape: the second reading's look at the daemon's gauges, between its waits.
const SETTLE_LOOK: std::time::Duration = std::time::Duration::from_millis(100);

/// The gauges' sum over the shards for `kind` (`mem.gauge.<kind>`), in `rows`.
fn summed(rows: &BTreeMap<String, u64>, kind: &str) -> u64 {
  rows
    .iter()
    .filter(|(name, _)| name.ends_with(&format!(":mem.gauge.{kind}")))
    .map(|(_, value)| *value)
    .sum()
}

/// The daemon's gauges again once a publication has released the import's deferred frees (main's ruling of 2026-11-25,
/// goal 93 P7: deferred frees a published image names are A-64's rule, so the claim is read one publication after the
/// import ends): read until `content.arena_deferred_bytes` is zero or [`SETTLE_PATIENCE`] passes, with the time waited.
fn settled_reading(
  client: &mut slates_client::Client,
) -> (std::time::Duration, BTreeMap<String, u64>) {
  let began = std::time::Instant::now();
  loop {
    let rows = gauges(client);
    if summed(&rows, "content.arena_deferred_bytes") == 0 || began.elapsed() >= SETTLE_PATIENCE {
      return (began.elapsed(), rows);
    }
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(SETTLE_LOOK);
  }
}

/// Prints the second reading's arena gauges, the process's resident size and (macOS) footprint, and the time it waited.
fn print_settled((waited, rows): &(std::time::Duration, BTreeMap<String, u64>)) {
  println!(
    "svn_import_storage: settled after {} ms",
    waited.as_millis()
  );
  for kind in [
    "content.arena_deferred_bytes",
    "content.arena_free_bytes",
    "content.arena_resident_free_bytes",
    "content.arena_mapped_bytes",
    "content.object_resident_bytes",
    "process.resident_bytes",
    "process.footprint_bytes",
  ] {
    println!(
      "svn_import_storage: settled {kind} = {}",
      summed(rows, kind)
    );
  }
}

/// The Subversion repository the dump is imported into.
fn dumped_repository() -> slates_client::RepositorySpec {
  slates_client::RepositorySpec {
    name: "dumped".to_owned(),
    tool: slates_ipc::protocol::VcsTool::Subversion,
    format: slates_ipc::protocol::ObjectFormat::Revisions,
    default_branch: "trunk".to_owned(),
    green: None,
  }
}

/// What the run read while the daemon served: the process's resident set and (macOS) footprint before and after the
/// import (KiB), the
/// gauges after it, and the store's directory before it and right after it (KiB, allocated and apparent).
struct Measured {
  /// Resident before and after the import.
  resident: (u64, u64),
  /// The physical footprint before and after the import, on macOS ([`footprint_kib`]).
  footprint: (Option<u64>, Option<u64>),
  /// The gauges after the import.
  rows: BTreeMap<String, u64>,
  /// The store before the import and live after it.
  store: ((u64, u64), (u64, u64)),
}

/// Prints `measured` and the store's directory `after` the daemon and its owner stopped: the repository's bytes on the
/// store are what it grew from before the import, and the [`LISTED`] largest gauges.
fn print_measured(measured: &Measured, after: (u64, u64)) {
  let (before, live) = measured.store;
  println!(
    "svn_import_storage: store KiB allocated/apparent: before {}/{}, after the import (live) {}/{}, after the stop {}/{}",
    before.0, before.1, live.0, live.1, after.0, after.1
  );
  println!(
    "svn_import_storage: the repository's on the store, KiB allocated/apparent: {}/{}",
    after.0.saturating_sub(before.0),
    after.1.saturating_sub(before.1)
  );
  println!(
    "svn_import_storage: resident KiB before {}, after the import {}",
    measured.resident.0, measured.resident.1
  );
  if let (Some(before), Some(after)) = measured.footprint {
    println!("svn_import_storage: footprint KiB before {before}, after the import {after}");
  }
  let mut listed: Vec<(&String, &u64)> = measured
    .rows
    .iter()
    .filter(|(name, _)| name.contains(":mem.gauge."))
    .collect();
  listed.sort_by(|a, b| b.1.cmp(a.1));
  for (name, value) in listed.into_iter().take(LISTED) {
    println!("svn_import_storage: after the import {name} = {value}");
  }
  // The import's own memory counters, whatever their size (goal 93 P7: what its batches gave back, main's ask).
  for (name, value) in &measured.rows {
    if name.contains(":repository.svn_import.purged") || name.contains(":content.purge") {
      println!("svn_import_storage: after the import {name} = {value}");
    }
  }
}
