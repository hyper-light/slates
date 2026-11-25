//! What a whole Mercurial clone waits on when it asks the changeset store (goal 93, main's order of 2026-11-25: hg's 57k
//! clone took 165.8 s against stock's 25.8 s, and its store asks waited 121.35 s of it, 6,232 asks at about 19 ms each).
//! An in-process daemon (a changeset store, its HTTP edge on loopback) is pushed a history by stock `hg`, then cloned
//! whole; the clone's store asks are read from the daemon's counters before and after it, the owner's part of each
//! split as `slates_store::owner::AskWaits` splits it: queued behind the owner's other work, the owner thread's wake,
//! its service, its log reads, and the hop back to the asking shard.
//!
//! By default it pushes a [`CHANGESETS`]-changeset history `hg debugbuilddag` makes; with `SLATES_HG_PUSH_SOURCE`
//! naming a Mercurial repository, it pushes that one. `#[ignore]`d: a measurement, run by name.
#![allow(
  clippy::unwrap_used,
  clippy::print_stdout,
  clippy::print_stderr,
  clippy::disallowed_methods,
  clippy::cast_precision_loss
)]

mod common;

use std::collections::BTreeMap;

/// Shape: the default history's changesets, each a new file and the overwritten file rewritten.
const CHANGESETS: u64 = 2_000;

/// Format: the counters of a fetch's store asks, the owner's split of them after the first two.
const COUNTERS: [&str; 12] = [
  "vcs_store.hg_stream_store_asks",
  "vcs_store.hg_stream_store_wait_ns",
  "vcs_store.hg_owner_requests",
  "vcs_store.hg_owner_total_ns",
  "vcs_store.hg_owner_behind_ns",
  "vcs_store.hg_owner_wake_ns",
  "vcs_store.hg_owner_serve_ns",
  "vcs_store.hg_owner_disk_ns",
  "vcs_store.hg_owner_back_ns",
  "vcs_store.hg_owner_most_ns",
  "vcs_store.hg_text_requests",
  "vcs_store.hg_stream_turns",
];

/// Each of [`COUNTERS`] summed over the daemon's shards.
fn counters(served: &common::hg::Served) -> BTreeMap<&'static str, u64> {
  let report = served.client().daemon_memory_status().unwrap();
  let mut sums = BTreeMap::new();
  for name in COUNTERS {
    let sum = report
      .shards
      .iter()
      .flat_map(|shard| &shard.refusals)
      .filter(|row| row.kind == name)
      .map(|row| row.count)
      .sum::<u64>();
    sums.insert(name, sum);
  }
  sums
}

/// The source pushed: `SLATES_HG_PUSH_SOURCE`'s repository, else a [`CHANGESETS`]-changeset `debugbuilddag` history.
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

/// Goal 93 (main's order of 2026-11-25). Do: push a history to an in-process daemon with stock `hg`, then clone it
/// whole, reading the store-ask counters before and after the clone. Expect: the clone succeeds; its owner requests'
/// parts sum to their whole. Prints the clone's wall time and each counter's change, the owner's split as shares of its
/// whole.
#[test]
#[ignore = "a measurement (the hg clone's store wait): run by name, with SLATES_HG_PUSH_SOURCE for a real history"]
fn what_a_whole_mercurial_clone_waits_on_when_it_asks_the_store() {
  if !common::hg::has_hg() {
    eprintln!("SKIPPED: no `hg` on PATH");
    return;
  }
  let served = common::hg::serve("hg-clone-wait", "");
  let url = served.url.clone();
  let source = source(&served);
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
  let before = counters(&served);
  let began = std::time::Instant::now();
  let cloned = served
    .hg(".")
    .args(["clone", "-q", "-U", &url, "cloned"])
    .output()
    .unwrap();
  let took = began.elapsed();
  let after = counters(&served);
  served.stop();
  assert!(
    cloned.status.success(),
    "clone: {}",
    String::from_utf8_lossy(&cloned.stderr)
  );
  let moved = |name: &str| after[name].saturating_sub(before[name]);
  println!(
    "hg_clone_store_wait: source {source}: the clone took {:.3} s",
    took.as_secs_f64()
  );
  for name in COUNTERS {
    println!("hg_clone_store_wait: {name} +{}", moved(name));
  }
  let total = moved("vcs_store.hg_owner_total_ns").max(1);
  for part in ["behind", "wake", "serve", "disk", "back"] {
    let ns = moved(&format!("vcs_store.hg_owner_{part}_ns"));
    println!(
      "hg_clone_store_wait: owner {part}: {ns} ns, {:.1}%",
      ns as f64 * 100.0 / total as f64
    );
  }
  let parts: u64 = ["behind", "wake", "serve", "disk", "back"]
    .iter()
    .map(|part| moved(&format!("vcs_store.hg_owner_{part}_ns")))
    .sum();
  assert_eq!(
    parts,
    moved("vcs_store.hg_owner_total_ns"),
    "the parts sum to the whole"
  );
}
