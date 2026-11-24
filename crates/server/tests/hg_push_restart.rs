//! A daemon's restart after a Mercurial push, measured (A-887; main 01a20182's run request of 2026-11-24 04:21): what a
//! restart's recovery brings back of the push's texts, and in what order it rebuilds the repository's greens. A push
//! names each file whose bytes the content store holds whole, and lets the texts go once its segment is durable; a
//! replay reads no run its apply names whole, so a recovery over a push alone should bring nothing back and rebuild
//! no green after the stores.
//!
//! By default it pushes a [`CHANGESETS`]-changeset history `hg debugbuilddag` makes. With `SLATES_HG_PUSH_SOURCE`
//! naming a Mercurial repository (CI's push-all pushes Mercurial's own history, 57,054 changesets), it pushes that one.
//! `#[ignore]`d: a measurement, run by name.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::print_stderr
)]

mod common;

use std::time::Instant;

/// Shape: the default history's changesets, each a new file and the overwritten file rewritten.
const CHANGESETS: u64 = 400;

/// Format: the counters the run prints after the restart: a recovery's Mercurial pre-pass's texts brought back, the
/// greens rebuilt after the stores, the reads that brought a named text back, and the content store refused at the
/// order's decision.
const COUNTERS: [&str; 4] = [
  "vcs.hg.named_recovery_brought_back",
  "merge.recovery.greens_after_stores",
  "merge.name_read.brought_back",
  "merge.recovery.content_unmade_at_order",
];

/// A-887 (main's run request of 2026-11-24 04:21). Do: start a two-shard daemon holding a changeset store with its HTTP
/// edge on loopback; push a Mercurial history to it with stock `hg` (Mercurial's own where `SLATES_HG_PUSH_SOURCE`
/// names it, else a [`CHANGESETS`]-changeset `debugbuilddag` history); restart the daemon and the store's owner over
/// the same anchor and store; then ask the restarted edge for the pushed tip. Expect: the push and the restart succeed;
/// the restarted daemon serves the source's tip; nothing brought back by the recovery's Mercurial pre-pass and no green
/// rebuilt after the stores (a push alone names its files whole, which a replay never reads). Prints the push's and the
/// restart's wall times and every counter of [`COUNTERS`]. Skips, saying so, where `hg` is not installed.
#[test]
#[ignore = "a measurement (A-887's restart): run by name, with SLATES_HG_PUSH_SOURCE for Mercurial's own history"]
fn a_restart_after_a_push_brings_back_none_of_its_texts() {
  if !common::hg::has_hg() {
    eprintln!("SKIPPED a_restart_after_a_push_brings_back_none_of_its_texts: no `hg` on PATH");
    return;
  }
  let mut served = common::hg::serve("hg-push-restart", "");
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
  let began = Instant::now();
  let pushed = served
    .hg(".")
    .args([
      "-R",
      &source,
      "push",
      "-q",
      "--new-branch",
      "-f",
      &served.url,
    ])
    .output()
    .expect("hg ran");
  let push_took = began.elapsed();
  // Printed before the restart, so a run the restart ends still says where the push got (main's r854).
  println!(
    "hg_push_restart: source {source}: pushed in {push_took:?}, exit {:?}",
    pushed.status.code()
  );
  assert!(
    pushed.status.code().is_some_and(|code| code <= 1),
    "push: {}\n{}",
    String::from_utf8_lossy(&pushed.stdout),
    String::from_utf8_lossy(&pushed.stderr)
  );
  let began = Instant::now();
  served.restart();
  let restart_took = began.elapsed();
  let counted = served.daemon().refusals_on_every_shard().unwrap();
  let tip = |url: &str| {
    let identified = served
      .hg(".")
      .args(["identify", "--id", "-r", "tip", url])
      .output()
      .expect("hg ran");
    assert!(
      identified.status.success(),
      "identify {url}: {}",
      String::from_utf8_lossy(&identified.stderr)
    );
    String::from_utf8_lossy(&identified.stdout)
      .trim()
      .to_owned()
  };
  let served_tip = tip(&served.url.clone());
  let source_tip = tip(&source);
  served.stop();
  println!(
    "hg_push_restart: restarted in {restart_took:?}; tip served {served_tip}, source {source_tip}"
  );
  for name in COUNTERS {
    println!(
      "hg_push_restart: {name} = {}",
      counted.get(name).copied().unwrap_or(0)
    );
  }
  assert_eq!(
    served_tip, source_tip,
    "the restarted daemon serves the pushed tip"
  );
  assert_eq!(
    counted.get(COUNTERS[0]).copied().unwrap_or(0),
    0,
    "nothing brought back for the push's runs"
  );
  assert_eq!(
    counted.get(COUNTERS[1]).copied().unwrap_or(0),
    0,
    "no green rebuilt after the stores"
  );
}
