//! A Mercurial import's memory, measured (goal 93 hg, R; main 01a20182's order of 2026-11-24 15:21): the import's peak
//! resident set, with what holds it at the peak named by the daemon's own memory gauges (`mem.gauge.*`, the memory
//! status's rows), never guessed. An in-process daemon (a changeset store, its edge on loopback) imports a repository's
//! directory through `import_repository` while this test samples the process's resident set and the gauges.
//!
//! By default it imports a [`CHANGESETS`]-changeset history `hg debugbuilddag` makes (skipping, saying so, where `hg` is
//! not installed). With `SLATES_HG_IMPORT_SOURCE` naming a Mercurial repository (Mercurial's own history, 57,036
//! changesets, is the one the bar is read on), it imports that one. `#[ignore]`d: a measurement, run by name.
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
use std::time::{Duration, Instant};

/// Shape: the default history's changesets, each a new file and the overwritten file rewritten.
const CHANGESETS: u64 = 2_000;
/// Shape: the time between samples.
const SAMPLE: Duration = Duration::from_millis(500);
/// Shape: the gauges printed at the peak, the largest first.
const LISTED: usize = 24;
/// Shape: the longest an import may take before the run gives up on it.
const PATIENCE: Duration = Duration::from_secs(1_800);

/// This process's resident set, KiB (`ps -o rss=`): the daemon is in-process, so it is the daemon's, its anchor's
/// content object mapped in as it touches it.
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
/// process's (the content object's residency) is read once a shard, never summed.
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

/// Format: the series' columns, each a row of [`gauges`] summed over the shards, in MiB where a byte count.
const SERIES: [&str; 7] = [
  "mem.gauge.content.object_resident_bytes",
  "mem.gauge.content.arena_mapped_bytes",
  "mem.gauge.content.arena_resident_free_bytes",
  "mem.gauge.content.arena_deferred_bytes",
  "mem.gauge.vcs_content.used_bytes",
  "vcs_store.hg_import.segments_landed",
  "vcs_store.hg_import.file_text_bytes_let_go",
];

/// `rows`' `name` summed over the shards.
fn summed(rows: &BTreeMap<String, u64>, name: &str) -> u64 {
  rows
    .iter()
    .filter(|(key, _)| key.split_once(':').is_some_and(|(_, kind)| kind == name))
    .map(|(_, value)| *value)
    .sum()
}

/// Goal 93 hg, R (main's order of 2026-11-24 15:21). Do: start a two-shard daemon holding a changeset store; import a
/// Mercurial repository's directory into its empty repository (Mercurial's own where `SLATES_HG_IMPORT_SOURCE` names it,
/// else a [`CHANGESETS`]-changeset `debugbuilddag` history), sampling the process's resident set and the daemon's memory
/// gauges every [`SAMPLE`] while it runs. Expect: the import answered. Prints its wall time, the peak resident set, the
/// gauges at the peak sample (the [`LISTED`] largest) and after the import, and the import's counters.
#[test]
#[ignore = "a measurement (goal 93 hg, R): run by name, with SLATES_HG_IMPORT_SOURCE for Mercurial's own history"]
fn a_mercurial_imports_peak_resident_set_is_named_by_the_daemons_gauges() {
  if !common::hg::has_hg() && std::env::var("SLATES_HG_IMPORT_SOURCE").is_err() {
    eprintln!(
      "SKIPPED a_mercurial_imports_peak_resident_set_is_named_by_the_daemons_gauges: no `hg` on PATH"
    );
    return;
  }
  let served = common::hg::serve("hg-import-memory", "");
  let source = match std::env::var("SLATES_HG_IMPORT_SOURCE") {
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
  let mut sampler = served.client();
  let before_kib = resident_kib();
  let before = gauges(&mut sampler);
  let began = Instant::now();
  let importing = {
    let mut client = served.client();
    let source = source.clone();
    std::thread::spawn(move || client.import_repository("project", &source))
  };
  let (mut peak_kib, mut peak_at, mut at_peak) = (before_kib, Duration::ZERO, before.clone());
  let mut samples = 0_u64;
  println!(
    "hg_import_memory: series: s, rss MiB, {}",
    SERIES.join(", ")
  );
  while !importing.is_finished() {
    assert!(
      began.elapsed() < PATIENCE,
      "the import ran past {PATIENCE:?}"
    );
    std::thread::sleep(SAMPLE);
    let kib = resident_kib();
    samples += 1;
    let rows = gauges(&mut sampler);
    let columns: Vec<String> = SERIES
      .iter()
      .map(|name| {
        let value = summed(&rows, name);
        if name.ends_with("_bytes") {
          (value >> 20).to_string()
        } else {
          value.to_string()
        }
      })
      .collect();
    println!(
      "hg_import_memory: series: {:.1}, {}, {}",
      began.elapsed().as_secs_f64(),
      kib >> 10,
      columns.join(", ")
    );
    if kib > peak_kib {
      peak_kib = kib;
      peak_at = began.elapsed();
      at_peak = rows;
    }
  }
  let took = began.elapsed();
  let imported = importing.join().unwrap();
  let after_kib = resident_kib();
  let after = gauges(&mut sampler);
  served.stop();
  println!("hg_import_memory: source {source}: {imported:?} in {took:?}, {samples} samples");
  println!(
    "hg_import_memory: resident before {before_kib} KiB, peak {peak_kib} KiB at {peak_at:?}, after {after_kib} KiB"
  );
  let mut listed: Vec<(&String, &u64)> = at_peak
    .iter()
    .filter(|(name, _)| name.contains(":mem.gauge."))
    .collect();
  listed.sort_by(|a, b| b.1.cmp(a.1));
  for (name, value) in listed.into_iter().take(LISTED) {
    let then = after.get(name).copied().unwrap_or(0);
    println!("hg_import_memory: at peak {name} = {value}; after {then}");
  }
  for (name, value) in &after {
    if name.contains(":vcs_store.hg_import") || name.contains(":vcs.hg.landed_let_go") {
      println!("hg_import_memory: counter {name} = {value}");
    }
  }
  assert!(imported.is_ok(), "the import answered: {imported:?}");
}
