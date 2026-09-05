//! The landing oracle (Phase 1 task 12; §4.15, D-26) over the simulated host: the worked
//! example of §4.15 with both of its failures, T-1.12's plan, T-1.14's outsider edits injected
//! between validation and write, T-1.15's crash at every write instruction with the sibling
//! sweep and the idempotent re-run, T-1.16's exchange fallback, AC-1.13's atomicity, AC-1.14's
//! proportionality, and the stage-and-exchange path for an empty target.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use slates_land::engine::{
  AuditKind, Degradation, LandingRefusal, LandingReport, LandingRequest, LandingState,
  LandingTarget, Observer, Outcome, Presented, Unobserved,
};
use slates_land::grant::{GrantRefusal, GrantScope, GrantState, Surface};
use slates_land::manifest::{Action, Filter, LandingEntry};
use slates_land::verdict::{ConflictClass, Verdict};
use slates_vfs::base::BaseConfig;
use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{HostFs, HostKind};
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{
  LARGE, Session, Setup, TERM_NS, config, mkdir, read_through, rename, request, rm_r, scratch,
  store, symlink, unlink, write_file,
};

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    config(),
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

fn root_target(host: &mut SimHost) -> LandingTarget {
  LandingTarget {
    dir: host.root(),
    key: "/".into(),
    parent: None,
  }
}

// ---------------------------------------------------------------- the disk

type Disk = BTreeMap<String, (HostKind, Vec<u8>)>;

fn is_hidden(path: &str) -> bool {
  path.split('/').any(|c| c.starts_with(".slates-"))
}

/// The disk as (path → kind and bytes), hidden siblings excluded.
fn disk(host: &SimHost) -> Disk {
  host
    .paths()
    .into_iter()
    .filter(|(p, _)| !is_hidden(p))
    .map(|(p, k)| {
      let bytes = host.bytes(&p).unwrap_or_default();
      (p, (k, bytes))
    })
    .collect()
}

fn hidden_names(host: &SimHost) -> Vec<String> {
  host
    .paths()
    .into_iter()
    .filter(|(p, _)| is_hidden(p))
    .map(|(p, _)| p)
    .collect()
}

fn outcome_of<'r>(report: &'r LandingReport, path: &str) -> &'r Outcome {
  report
    .entries
    .iter()
    .find(|e| e.path.as_ref() == path)
    .unwrap_or_else(|| panic!("no entry for {path}"))
    .outcome
    .as_ref()
    .unwrap()
}

// ---------------------------------------------------------------- the worked example

/// The base of §4.15's worked example, scaled: `files` under `src/` and `tests/`.
fn example_base(host: &mut SimHost, files: usize) {
  host.mkdir("/src");
  host.mkdir("/tests");
  for f in 0..files {
    let dir = if f % 2 == 0 { "src" } else { "tests" };
    host.replace_file(&format!("/{dir}/f{f}.rs"), format!("base {f}").as_bytes());
  }
  host.replace_file("/src/lib.rs", b"base lib");
  host.replace_file("/tests/a.rs", b"base a");
}

/// The edits: twelve replacements, one delete, three creates, and 190 files under `target/`.
fn example_edits(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  for f in 0..10 {
    let dir = if f % 2 == 0 { "src" } else { "tests" };
    write_file(
      vol,
      host,
      store,
      &format!("/{dir}/f{f}.rs"),
      format!("edited {f}").as_bytes(),
    );
  }
  write_file(vol, host, store, "/src/lib.rs", b"edited lib");
  write_file(vol, host, store, "/tests/a.rs", b"edited a");
  unlink(vol, host, store, "/src/f10.rs");
  write_file(vol, host, store, "/src/new1.rs", b"new 1");
  write_file(vol, host, store, "/src/new2.rs", b"new 2");
  write_file(vol, host, store, "/tests/new3.rs", b"new 3");
  mkdir(vol, host, store, "/target");
  for f in 0..190 {
    write_file(vol, host, store, &format!("/target/o{f}"), b"obj");
  }
}

fn example_request(id: u64) -> LandingRequest {
  LandingRequest {
    filter: Filter {
      include: Vec::new(),
      exclude: vec!["/target".into()],
    },
    ..request(id)
  }
}

/// The presented manifest of the worked example: 16 entries, 191 filtered out, all `Apply`.
fn assert_presented(presented: &Presented) {
  assert_eq!(presented.manifest.entries.len(), 16);
  assert_eq!(
    presented.manifest.summary.filtered_out, 191,
    "190 objects and target/ itself"
  );
  let by_action = &presented.manifest.summary.by_action;
  assert_eq!(by_action.get("replace").copied(), Some(12));
  assert_eq!(by_action.get("delete").copied(), Some(1));
  assert_eq!(by_action.get("create").copied(), Some(3));
  assert!(
    presented
      .preliminary
      .iter()
      .all(|e| e.verdict == Some(Verdict::Apply))
  );
}

/// The disk and the overlay after the worked example landed.
fn assert_example_landed(report: &LandingReport, host: &SimHost, vol: &Volume, store: &Store) {
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(report.written, 16);
  assert!(report.durability.data_synced && report.durability.dirs_synced);
  assert_eq!(host.bytes("/src/lib.rs").unwrap(), b"edited lib");
  assert_eq!(host.bytes("/src/new1.rs").unwrap(), b"new 1");
  assert!(host.bytes("/src/f10.rs").is_none());
  assert!(host.bytes("/target/o0").is_none(), "excluded by the filter");
  assert!(hidden_names(host).is_empty());
  assert_only_excluded_remain(vol, store);
}

/// Advance: the landed entries left the overlay; the excluded ones stayed.
fn assert_only_excluded_remain(vol: &Volume, store: &Store) {
  let diverged = vol.diverged(store);
  assert!(
    diverged.iter().all(|d| d.path.starts_with("/target")),
    "{diverged:?}"
  );
  assert_eq!(diverged.len(), 191);
}

/// One run of the worked example over a base of `base_files`; returns the host calls it took.
fn worked_example(base_files: usize) -> u64 {
  let mut host = SimHost::new();
  example_base(&mut host, base_files);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  example_edits(&mut vol, &mut host, &mut store);
  let target = root_target(&mut host);
  let mut session = Session::new();
  let calls_before = host.calls();
  let writes_before = host.write_steps();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let presented = setup.present(&example_request(1));
  assert_presented(&presented);
  assert_eq!(
    setup.host.write_steps(),
    writes_before,
    "presenting writes nothing"
  );
  let report = setup.land(example_request(1)).unwrap();
  assert_example_landed(&report, &host, &vol, &store);
  host.calls() - calls_before
}

/// §4.15's worked example: 16 entries planned, 190 excluded, 0 conflicts; granted once; 16
/// `Written`; the disk holds the overlay's bytes; the overlay is empty after; the host work
/// is proportional to the 16 entries, not the base (AC-1.14 at 10^3 and 10^5 base entries).
#[test]
fn the_worked_example_lands_sixteen_entries_and_the_work_does_not_grow_with_the_base() {
  let small = worked_example(1_000);
  let large = worked_example(100_000);
  assert_eq!(small, large, "host calls at 10^3 and 10^5 base entries");
}

/// Failure one of the worked example: the user edited `src/lib.rs` before the grant; the
/// preliminary verdict shows `ModifyModify`; a landing refuses with nothing written; the agent
/// reads the disk version, rewrites, rewitnesses and lands with a new manifest.
#[test]
fn failure_one_a_conflict_refuses_before_any_write_and_rewitness_clears_it() {
  let mut host = SimHost::new();
  example_base(&mut host, 20);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  example_edits(&mut vol, &mut host, &mut store);
  host.replace_file("/src/lib.rs", b"user's lib");
  let target = root_target(&mut host);
  let mut session = Session::new();
  let writes_before = host.write_steps();
  let refused = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(example_request(2));
  let Err(LandingRefusal::Conflict(entries)) = refused else {
    panic!("{refused:?}");
  };
  let lib = entries
    .iter()
    .find(|e| e.path.as_ref() == "/src/lib.rs")
    .unwrap();
  assert_eq!(
    lib.verdict,
    Some(Verdict::Conflict(ConflictClass::ModifyModify))
  );
  assert_eq!(
    host.write_steps(),
    writes_before,
    "AC-1.12: nothing written while any verdict is a conflict"
  );
  assert_eq!(host.bytes("/src/lib.rs").unwrap(), b"user's lib");
  // Resolve: read the disk version, merge, rewitness, land again.
  let disk_version = vol.with_host(&mut host).read_base("/src/lib.rs").unwrap();
  assert_eq!(disk_version, b"user's lib");
  write_file(
    &mut vol,
    &mut host,
    &mut store,
    "/src/lib.rs",
    b"user's lib + edited lib",
  );
  vol
    .with_host(&mut host)
    .rewitness(&mut store, Some(&["/src/lib.rs".to_owned()]))
    .unwrap();
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(example_request(3))
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(
    host.bytes("/src/lib.rs").unwrap(),
    b"user's lib + edited lib"
  );
}

/// An observer that replaces named files on the disk right before their write (an editor or
/// `git checkout` racing the landing).
struct Outsider {
  victims: Vec<&'static str>,
}

impl Observer<SimHost> for Outsider {
  fn before_write(&mut self, host: &mut SimHost, entry: &LandingEntry) {
    if self.victims.contains(&entry.path.as_ref()) {
      host.replace_file(&entry.path, format!("outsider {}", entry.path).as_bytes());
    }
  }
}

/// Failure two of the worked example: `git checkout` replaced `tests/a.rs` between validation
/// and the exchange; the displaced file is not the witness; the landing exchanges back and
/// records `Undone(TargetInUse)`; the report is `Partial` with 15 `Written`; the outsider's
/// bytes stay; the entry stays in the overlay.
#[test]
fn failure_two_a_lost_compare_and_swap_is_undone_and_reported() {
  let mut host = SimHost::new();
  example_base(&mut host, 20);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  example_edits(&mut vol, &mut host, &mut store);
  let target = root_target(&mut host);
  let mut session = Session::new();
  let mut outsider = Outsider {
    victims: vec!["/tests/a.rs"],
  };
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land_with(example_request(4), &mut outsider)
  .unwrap();
  assert_eq!(report.state, LandingState::Partial, "{report:?}");
  assert_eq!(report.written, 15);
  assert_eq!(report.conflicts, 1);
  assert_eq!(
    *outcome_of(&report, "/tests/a.rs"),
    Outcome::Undone(ConflictClass::TargetInUse)
  );
  assert_eq!(
    host.bytes("/tests/a.rs").unwrap(),
    b"outsider /tests/a.rs",
    "no outsider write lost"
  );
  assert!(hidden_names(&host).is_empty(), "the temporary went away");
  let diverged = vol.diverged(&store);
  assert!(
    diverged.iter().any(|d| d.path == "/tests/a.rs"),
    "the entry stays in the overlay"
  );
}

/// An observer that rewrites a pseudo-random subset of the replacements, alternating a new
/// inode (save-by-rename) and an in-place write.
struct RandomOutsider {
  seed: u64,
  hit: BTreeMap<String, Vec<u8>>,
}

impl Observer<SimHost> for RandomOutsider {
  fn before_write(&mut self, host: &mut SimHost, entry: &LandingEntry) {
    /// Shape: a linear congruential step (Knuth's MMIX constants).
    const MUL: u64 = 6_364_136_223_846_793_005;
    const INC: u64 = 1_442_695_040_888_963_407;
    self.seed = self.seed.wrapping_mul(MUL).wrapping_add(INC);
    if !matches!(entry.action, Action::Replace) || self.seed >> 62 == 0 {
      return;
    }
    let bytes = format!("outsider {}", entry.path).into_bytes();
    if (self.seed >> 61) & 1 == 0 {
      host.replace_file(&entry.path, &bytes);
    } else {
      host.write_in_place(&entry.path, &bytes, 1_000);
    }
    self.hit.insert(entry.path.to_string(), bytes);
  }
}

/// Every entry the outsider hit is `Undone(TargetInUse)` with the outsider's bytes on the
/// disk; every other one is `Written` with the agent's.
fn assert_losses_detected(report: &LandingReport, host: &SimHost, hit: &BTreeMap<String, Vec<u8>>) {
  for entry in &report.entries {
    let path = entry.path.to_string();
    match hit.get(&path) {
      Some(bytes) => {
        assert_eq!(
          entry.outcome,
          Some(Outcome::Undone(ConflictClass::TargetInUse)),
          "{path}"
        );
        assert_eq!(
          host.bytes(&path).unwrap(),
          *bytes,
          "{path}: the outsider's write stays"
        );
      }
      None => {
        assert_eq!(entry.outcome, Some(Outcome::Written), "{path}");
        assert!(host.bytes(&path).unwrap().starts_with(b"agent"), "{path}");
      }
    }
  }
}

/// T-1.14: an outsider rewriting target files at random while a landing runs; every loss is
/// detected at the swap, undone and reported; no outsider write is lost; no agent write is
/// applied over one. Both outsider forms: a new inode (save-by-rename) and an in-place write.
#[test]
fn t_1_14_random_outsider_rewrites_are_all_detected() {
  for seed in 1..=8u64 {
    let mut host = SimHost::new();
    example_base(&mut host, 40);
    let mut store = store();
    let mut vol = overlay(&mut host, &mut store);
    for f in 0..40 {
      let dir = if f % 2 == 0 { "src" } else { "tests" };
      write_file(
        &mut vol,
        &mut host,
        &mut store,
        &format!("/{dir}/f{f}.rs"),
        format!("agent {f}").as_bytes(),
      );
    }
    let target = root_target(&mut host);
    let mut session = Session::new();
    let mut outsider = RandomOutsider {
      seed,
      hit: BTreeMap::new(),
    };
    let report = Setup {
      host: &mut host,
      target: &target,
      vol: &mut vol,
      store: &mut store,
      session: &mut session,
    }
    .land_with(request(seed), &mut outsider)
    .unwrap();
    assert_losses_detected(&report, &host, &outsider.hit);
    let expected = if outsider.hit.is_empty() {
      LandingState::Done
    } else {
      LandingState::Partial
    };
    assert_eq!(report.state, expected);
    assert_eq!(report.conflicts, outsider.hit.len());
    assert!(hidden_names(&host).is_empty());
  }
}

// ---------------------------------------------------------------- crashes

/// The crash scenario's base: every action class at once.
fn crash_base(host: &mut SimHost) {
  host.replace_file("/keep.txt", b"keep");
  for n in 1..=3 {
    host.replace_file(&format!("/old{n}.txt"), format!("old {n}").as_bytes());
  }
  host.replace_file("/gone.txt", b"gone");
  host.mkdir("/tree");
  host.replace_file("/tree/x.txt", b"x");
  host.mkdir("/tree/y");
  host.replace_file("/tree/y/z.txt", b"z");
  host.mkdir("/moved");
  host.replace_file("/moved/m.txt", b"m");
  host.mkdir("/cleared");
  host.replace_file("/cleared/a.txt", b"a");
  host.replace_file("/cleared/b.txt", b"b");
}

fn crash_edits(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  write_file(vol, host, store, "/new1.txt", b"new 1");
  write_file(vol, host, store, "/new2.txt", b"new 2");
  mkdir(vol, host, store, "/dir");
  write_file(vol, host, store, "/dir/inner.txt", b"inner");
  for n in 1..=3 {
    write_file(
      vol,
      host,
      store,
      &format!("/old{n}.txt"),
      format!("replaced {n}").as_bytes(),
    );
  }
  unlink(vol, host, store, "/gone.txt");
  rm_r(vol, host, store, "/tree");
  rename(vol, host, store, "/moved", "/moved2");
  symlink(vol, host, store, "/link", "keep.txt");
  rm_r(vol, host, store, "/cleared");
  mkdir(vol, host, store, "/cleared");
  write_file(vol, host, store, "/cleared/c.txt", b"c");
}

/// The crash scenario, ready to land: the host, the volume and its store.
fn crash_scenario() -> (SimHost, Store, Volume) {
  let mut host = SimHost::new();
  crash_base(&mut host);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  crash_edits(&mut vol, &mut host, &mut store);
  (host, store, vol)
}

/// The reference: the disk before, the disk after a clean landing, and the write count.
fn crash_reference() -> (Disk, Disk, u64) {
  let (mut host, mut store, mut vol) = crash_scenario();
  let before = {
    let mut fresh = SimHost::new();
    crash_base(&mut fresh);
    disk(&fresh)
  };
  let target = root_target(&mut host);
  let mut session = Session::new();
  let writes = host.write_steps();
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(request(1))
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert!(hidden_names(&host).is_empty());
  (before, disk(&host), host.write_steps() - writes)
}

/// Every path on `now` is as on `before` or as on `after`: old or new, never torn.
fn assert_old_or_new(crash_at: u64, now: &Disk, before: &Disk, after: &Disk) {
  for path in before.keys().chain(after.keys()) {
    let old_or_new = now.get(path) == before.get(path) || now.get(path) == after.get(path);
    assert!(
      old_or_new,
      "crash {crash_at}: {path} is torn: {:?}",
      now.get(path)
    );
  }
}

/// One crash at write instruction `crash_at`: every path old or new, then the resume reaches
/// the reference, sweeps the siblings, and a further run plans nothing.
fn crash_then_resume(crash_at: u64, before: &Disk, after: &Disk) {
  let (mut host, mut store, mut vol) = crash_scenario();
  let target = root_target(&mut host);
  let mut session = Session::new();
  host.crash_at_write(host.write_steps() + crash_at);
  let result = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(request(1));
  assert!(host.crashed(), "crash {crash_at}");
  if let Ok(report) = &result {
    assert_eq!(
      report.state,
      LandingState::Aborted,
      "crash {crash_at}: {report:?}"
    );
    assert!(
      report
        .degraded
        .iter()
        .any(|d| matches!(d, Degradation::Crashed { .. }))
    );
  }
  assert_old_or_new(crash_at, &disk(&host), before, after);
  host.recover();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let report = setup.land(request(1)).unwrap();
  assert_eq!(
    report.state,
    LandingState::Done,
    "crash {crash_at}: resume {report:?}"
  );
  let presented = setup.present(&request(1));
  assert!(
    presented.manifest.entries.is_empty(),
    "crash {crash_at}: idempotent: {:?}",
    presented.manifest.entries
  );
  assert_eq!(
    &disk(&host),
    after,
    "crash {crash_at}: the resumed landing reaches the reference"
  );
  assert!(
    hidden_names(&host).is_empty(),
    "crash {crash_at}: siblings swept: {:?}",
    hidden_names(&host)
  );
}

/// T-1.15 and AC-1.13: crash at every write instruction; every entry is old or new; the
/// resume sweeps the hidden siblings and lands the rest; a further run plans nothing.
#[test]
fn t_1_15_crash_at_every_write_instruction_then_resume() {
  let (before, after, steps) = crash_reference();
  assert!(steps > 20, "the scenario writes in many steps: {steps}");
  for crash_at in 0..steps {
    crash_then_resume(crash_at, &before, &after);
  }
}

// ---------------------------------------------------------------- exchange fallback

/// T-1.16: a filesystem without exchange; replacements take verify-then-rename; the window is
/// measured into the outcome; the Degraded cell is reported; an outsider edit before the write
/// is still refused (at the verify).
#[test]
fn t_1_16_without_exchange_the_fallback_reports_its_window() {
  let mut host = SimHost::new();
  host.set_exchange_supported(false);
  example_base(&mut host, 20);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  example_edits(&mut vol, &mut host, &mut store);
  let target = root_target(&mut host);
  let mut session = Session::new();
  let mut outsider = Outsider {
    victims: vec!["/tests/a.rs"],
  };
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land_with(example_request(5), &mut outsider)
  .unwrap();
  assert_eq!(report.state, LandingState::Partial, "{report:?}");
  assert!(
    report
      .degraded
      .iter()
      .any(|d| matches!(d, Degradation::NoExchange { .. })),
    "{:?}",
    report.degraded
  );
  let lib = report
    .entries
    .iter()
    .find(|e| e.path.as_ref() == "/src/lib.rs")
    .unwrap();
  assert_eq!(lib.outcome, Some(Outcome::Written));
  assert!(
    lib.window_ns.is_some(),
    "the window is written into the outcome"
  );
  assert_eq!(
    *outcome_of(&report, "/tests/a.rs"),
    Outcome::Conflict(ConflictClass::TargetInUse)
  );
  assert_eq!(host.bytes("/tests/a.rs").unwrap(), b"outsider /tests/a.rs");
  assert_eq!(host.bytes("/src/lib.rs").unwrap(), b"edited lib");
  assert!(hidden_names(&host).is_empty());
}

/// Named temporaries (no `O_TMPFILE`): the same landing, the hidden names gone after.
#[test]
fn named_temporaries_leave_nothing_behind() {
  let mut host = SimHost::new();
  host.set_unnamed_temporaries(false);
  example_base(&mut host, 20);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  example_edits(&mut vol, &mut host, &mut store);
  let target = root_target(&mut host);
  let mut session = Session::new();
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(example_request(6))
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert!(hidden_names(&host).is_empty());
  assert_eq!(host.bytes("/src/new1.rs").unwrap(), b"new 1");
}

// ---------------------------------------------------------------- T-1.12

/// T-1.12: `rm -r` of a 40k-entry base directory, then two files recreated inside; the plan
/// is one recursive removal (a clear) and two creates; the landing leaves exactly two entries.
#[test]
fn t_1_12_a_removed_and_recreated_directory_plans_one_clear_and_two_creates() {
  let mut host = SimHost::new();
  host.mkdir("/big");
  for f in 0..40_000 {
    host.replace_file(&format!("/big/f{f}"), b"x");
  }
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  rm_r(&mut vol, &mut host, &mut store, "/big");
  mkdir(&mut vol, &mut host, &mut store, "/big");
  write_file(&mut vol, &mut host, &mut store, "/big/one", b"1");
  write_file(&mut vol, &mut host, &mut store, "/big/two", b"2");
  let target = root_target(&mut host);
  let mut session = Session::new();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let presented = setup.present(&request(7));
  let actions: Vec<&Action> = presented
    .manifest
    .entries
    .iter()
    .map(|e| &e.action)
    .collect();
  assert_eq!(
    actions,
    vec![&Action::Clear, &Action::Create, &Action::Create],
    "{actions:?}"
  );
  let report = setup.land(request(7)).unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  let inside: Vec<String> = host
    .paths()
    .into_iter()
    .filter(|(p, _)| p.starts_with("/big/"))
    .map(|(p, _)| p)
    .collect();
  assert_eq!(inside, vec!["/big/one".to_owned(), "/big/two".to_owned()]);
  assert!(
    vol.diverged(&store).is_empty(),
    "{:?}",
    vol.diverged(&store)
  );
}

// ---------------------------------------------------------------- staging

/// Ten directories of a hundred files each in a scratch volume.
fn scratch_tree(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  for d in 0..10 {
    mkdir(vol, host, store, &format!("/d{d}"));
    for f in 0..100 {
      write_file(
        vol,
        host,
        store,
        &format!("/d{d}/f{f}"),
        format!("{d}/{f}").as_bytes(),
      );
    }
  }
}

/// A scratch volume into an empty target: built in a hidden sibling and exchanged in one
/// step; the volume becomes an overlay over the target; a second landing plans nothing; reads
/// then come from the disk and follow it.
#[test]
fn a_scratch_volume_into_an_empty_target_is_staged_and_exchanged() {
  let mut host = SimHost::new();
  host.mkdir("/out");
  let root = host.root();
  let dir = host.open_dir(root, "out").unwrap();
  let target = LandingTarget {
    dir,
    key: "/out".into(),
    parent: Some((root, "out".into())),
  };
  let mut store = store();
  let mut vol = scratch(&mut store);
  scratch_tree(&mut vol, &mut host, &mut store);
  let mut session = Session::new();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let report = setup.land(request(8)).unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert!(report.staged, "an empty target is staged and exchanged");
  assert_eq!(report.written, 1_010);
  let presented = setup.present(&request(8));
  assert!(presented.manifest.entries.is_empty());
  assert_eq!(host.bytes("/out/d3/f7").unwrap(), b"3/7");
  assert!(hidden_names(&host).is_empty());
  assert!(
    vol.is_overlay(),
    "a scratch volume gains a base on the target"
  );
  assert!(vol.diverged(&store).is_empty());
  assert_reads_follow_the_disk(&mut vol, &mut host, &mut store);
}

/// After the landing the volume reads the file from the disk and follows an outsider's edit.
fn assert_reads_follow_the_disk(vol: &mut Volume, host: &mut SimHost, store: &mut Store) {
  assert_eq!(read_through(vol, host, store, "/d3/f7"), b"3/7");
  host.advance_ns(1_000_000);
  host.replace_file("/out/d3/f7", b"disk");
  assert_eq!(
    read_through(vol, host, store, "/d3/f7"),
    b"disk",
    "untouched entries follow the live disk"
  );
}

/// A scratch volume into a populated target lands in place, creates only, with `CreateCreate`
/// when a name already holds different bytes.
#[test]
fn a_scratch_volume_into_a_populated_target_lands_in_place() {
  let mut host = SimHost::new();
  host.replace_file("/existing", b"theirs");
  host.replace_file("/same", b"same");
  let mut store = store();
  let mut vol = scratch(&mut store);
  write_file(&mut vol, &mut host, &mut store, "/fresh", b"fresh");
  write_file(&mut vol, &mut host, &mut store, "/same", b"same");
  write_file(&mut vol, &mut host, &mut store, "/existing", b"mine");
  let target = root_target(&mut host);
  let mut session = Session::new();
  let refused = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(request(9));
  let Err(LandingRefusal::Conflict(entries)) = refused else {
    panic!("{refused:?}");
  };
  let existing = entries
    .iter()
    .find(|e| e.path.as_ref() == "/existing")
    .unwrap();
  assert_eq!(
    existing.verdict,
    Some(Verdict::Conflict(ConflictClass::CreateCreate))
  );
  let same = entries.iter().find(|e| e.path.as_ref() == "/same").unwrap();
  assert_eq!(
    same.verdict,
    Some(Verdict::Skip),
    "same bytes: nothing to do"
  );
  assert_eq!(host.bytes("/existing").unwrap(), b"theirs");
  unlink(&mut vol, &mut host, &mut store, "/existing");
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(request(10))
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert!(!report.staged);
  assert_eq!(host.bytes("/fresh").unwrap(), b"fresh");
}

// ---------------------------------------------------------------- grants and leases

/// The failure matrix: a single-use grant bound to another manifest refuses with nothing
/// written; a held lease refuses before anything else.
#[test]
fn a_mismatched_grant_and_a_held_lease_refuse() {
  let mut host = SimHost::new();
  example_base(&mut host, 4);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/src/lib.rs", b"v1");
  let target = root_target(&mut host);
  let mut session = Session::new();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let presented = setup.present(&request(11));
  let once = setup.session.grants.issue(
    Surface::Cli,
    presented.manifest.hash,
    GrantScope::Once,
    1,
    TERM_NS,
  );
  // The plan changed after the human saw it.
  write_file(setup.vol, setup.host, setup.store, "/src/lib.rs", b"v2");
  let mut req = request(11);
  req.grant = Some(once);
  let refused = setup.try_land(&req, &mut Unobserved);
  assert!(
    matches!(
      refused,
      Err(LandingRefusal::Grant(GrantRefusal::GrantMismatch { .. }))
    ),
    "{refused:?}"
  );
  assert_eq!(setup.host.bytes("/src/lib.rs").unwrap(), b"base lib");
  assert!(
    setup
      .session
      .grants
      .get(once)
      .is_some_and(|g| g.state == GrantState::Issued),
    "the mismatched grant was never used"
  );
}

/// A held lease refuses before anything else; released, the landing runs and consumes its
/// single-use grant.
#[test]
fn a_held_lease_refuses_and_a_single_use_grant_is_consumed() {
  let mut host = SimHost::new();
  example_base(&mut host, 4);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/src/lib.rs", b"v2");
  let target = root_target(&mut host);
  let mut session = Session::new();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let mut req = request(11);
  let presented = setup.present(&request(11));
  let fresh = setup.session.grants.issue(
    Surface::Cli,
    presented.manifest.hash,
    GrantScope::Once,
    1,
    TERM_NS,
  );
  req.grant = Some(fresh);
  let held = setup.session.leases.take("/", 99, 1, TERM_NS).unwrap();
  let refused = setup.try_land(&req, &mut Unobserved);
  assert!(
    matches!(refused, Err(LandingRefusal::LeaseHeld(_))),
    "{refused:?}"
  );
  assert_eq!(setup.host.bytes("/src/lib.rs").unwrap(), b"base lib");
  setup.session.leases.release(&held);
  let report = setup.try_land(&req, &mut Unobserved).unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(setup.host.bytes("/src/lib.rs").unwrap(), b"v2");
  assert!(
    setup
      .session
      .grants
      .get(fresh)
      .is_some_and(|g| g.state == GrantState::Consumed),
    "a single-use grant is consumed by its landing"
  );
}

/// A session grant covers later landings of the same volume into the same target; each still
/// presents and still lands; the grant is not consumed; the audit log has every finish.
#[test]
fn a_session_grant_covers_the_next_landing() {
  let mut host = SimHost::new();
  example_base(&mut host, 4);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/src/lib.rs", b"v1");
  let target = root_target(&mut host);
  let mut session = Session::new();
  let mut setup = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  };
  let presented = setup.present(&request(12));
  let grant = setup.session.grants.issue(
    Surface::Confirmation,
    presented.manifest.hash,
    GrantScope::Session,
    1,
    TERM_NS,
  );
  let mut req = request(12);
  req.grant = Some(grant);
  let report = setup.try_land(&req, &mut Unobserved).unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(setup.host.bytes("/src/lib.rs").unwrap(), b"v1");
  write_file(setup.vol, setup.host, setup.store, "/src/lib.rs", b"v3");
  let report = setup.try_land(&req, &mut Unobserved).unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(setup.host.bytes("/src/lib.rs").unwrap(), b"v3");
  assert!(
    setup
      .session
      .grants
      .get(grant)
      .is_some_and(|g| g.state == GrantState::Issued),
    "a session grant is not consumed"
  );
  let finished = setup
    .session
    .audit
    .records()
    .iter()
    .filter(|r| r.kind == AuditKind::LandingFinished)
    .count();
  assert_eq!(finished, 2);
}
