//! Durability and cleanup under host failures (§4.15 steps 6–9; AUD-29-05): a landing reports every
//! directory it could not open or sync, a media barrier the grant asked for that failed, and every entry of
//! its own it could not remove; it advances only entries that reached the durability boundary, so the rest
//! stay in the volume's overlay, a resume syncs and advances them, and no private work is lost.
//!
//! Until 2026-09-29 a directory that failed to open was skipped with the directories reported synced; a
//! sync failure that was not crash-like (a full disk) still let every entry advance out of the overlay; the
//! sweep's failures counted zero; and a temporary a failed write could not remove was dropped unreported
//! (docs/bugs/2026-09-29-a-landing-advanced-entries-it-had-not-made-durable.md).
//!
//! Faults are armed on the simulated host ([`SimHost::fail`]): a verb, a path prefix, the host's answer
//! and how many calls it answers so.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_land::engine::{
  Degradation, HostSlot, LandingReport, LandingRequest, LandingState, LandingTarget, Observer,
  Outcome, SkipReason, Unobserved, begin_landing,
};
use slates_land::grant::{GrantScope, Surface};
use slates_land::manifest::LandingEntry;
use slates_land::verdict::ConflictClass;
use slates_vfs::base::BaseConfig;
use slates_vfs::host::sim::{SimHost, SimVerb};
use slates_vfs::host::{HostError, HostFs};
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{
  LARGE, LEASE_HOLDER, Session, Setup, TERM_NS, config, mkdir, request, store, target_lease_key,
  write_file,
};

/// Format: `ENOSPC` on Linux and macOS — a full disk, which a landing survives (not crash-like).
const ENOSPC: i32 = 28;
/// Format: `EACCES` on Linux and macOS — a removal the host refuses.
const EACCES: i32 = 13;

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  // The base tree predates the mount (`common::SETTLED_NS`).
  host.advance_ns(common::SETTLED_NS);
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
  }
}

/// A disk with a file in each of `/a` and `/b`, and an overlay that replaces both.
fn two_directories() -> (SimHost, Store, Volume) {
  let mut host = SimHost::new();
  host.mkdir("/a");
  host.replace_file("/a/x", b"old x");
  host.mkdir("/b");
  host.replace_file("/b/y", b"old y");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/a/x", b"new x");
  write_file(&mut vol, &mut host, &mut store, "/b/y", b"new y");
  (host, store, vol)
}

/// One landing session over a history: the host, the volume and its store, the target and the grants.
struct History {
  host: SimHost,
  store: Store,
  vol: Volume,
  target: LandingTarget,
  session: Session,
}

impl History {
  fn new((mut host, store, vol): (SimHost, Store, Volume)) -> History {
    let target = root_target(&mut host);
    History {
      host,
      store,
      vol,
      target,
      session: Session::new(),
    }
  }

  fn setup(&mut self) -> Setup<'_, SimHost> {
    Setup {
      host: &mut self.host,
      target: &self.target,
      vol: &mut self.vol,
      store: &mut self.store,
      session: &mut self.session,
    }
  }

  /// Presents, grants and lands `req`.
  fn land(&mut self, req: LandingRequest) -> LandingReport {
    self.setup().land(req).unwrap()
  }

  /// [`History::land`] with `observer` before each write.
  fn land_with<O: Observer<SimHost>>(
    &mut self,
    req: LandingRequest,
    observer: &mut O,
  ) -> LandingReport {
    self.setup().land_with(req, observer).unwrap()
  }

  /// The paths the volume still diverges at: what a plan of it would land now.
  fn diverged(&mut self) -> Vec<String> {
    let presented = self.setup().present(&request(1));
    presented
      .manifest
      .entries
      .iter()
      .map(|e| e.path.to_string())
      .collect()
  }

  /// The landing's own names left on the disk.
  fn leftovers(&self) -> Vec<String> {
    self
      .host
      .paths()
      .into_iter()
      .map(|(path, _)| path)
      .filter(|path| path.split('/').any(|part| part.starts_with(".slates-")))
      .collect()
  }
}

fn outcome_of<'r>(report: &'r LandingReport, path: &str) -> Option<&'r Outcome> {
  report
    .entries
    .iter()
    .find(|e| e.path.as_ref() == path)
    .and_then(|e| e.outcome.as_ref())
}

/// A resume without faults: `Done` with nothing held and its directories synced, a further plan empty, and
/// both files holding their new bytes. The resume's report.
fn assert_resume_reaches_the_reference(
  history: &mut History,
  req: LandingRequest,
) -> LandingReport {
  let resumed = history.land(req);
  assert_eq!(resumed.state, LandingState::Done, "{resumed:?}");
  assert_eq!(resumed.held, 0, "{resumed:?}");
  assert!(resumed.durability.dirs_synced, "{resumed:?}");
  assert!(history.diverged().is_empty(), "{:?}", history.diverged());
  assert_eq!(history.host.bytes("/a/x").unwrap(), b"new x");
  assert_eq!(history.host.bytes("/b/y").unwrap(), b"new y");
  resumed
}

/// AUD-29-05 (a sync failure). Do: land replacements in `/a` and `/b` while the sync of `/a` answers
/// `ENOSPC` once. Expect: the landing is `Partial` with its directories not synced, `/a` reported `Unsynced`
/// with that answer and one entry held; both files hold their new bytes, but only `/b/y` has left the
/// overlay. Then a resume without the fault reaches the reference.
#[test]
fn an_unsynced_directory_holds_its_entries_until_a_resume_syncs_it() {
  let mut history = History::new(two_directories());
  history
    .host
    .fail(SimVerb::SyncDir, "/a", HostError::Unavailable(ENOSPC), 1);
  let report = history.land(request(1));
  assert_eq!(report.state, LandingState::Partial, "{report:?}");
  assert!(!report.durability.dirs_synced, "{report:?}");
  assert!(
    report.degraded.contains(&Degradation::Unsynced {
      dir: "/a".into(),
      error: HostError::Unavailable(ENOSPC),
    }),
    "{:?}",
    report.degraded
  );
  assert_eq!(report.held, 1, "{report:?}");
  assert_eq!(
    history.diverged(),
    vec!["/a/x".to_owned()],
    "the unsynced entry stays private"
  );
  assert_resume_reaches_the_reference(&mut history, request(1));
}

/// An outsider that puts a file at `path` just before the entry at `path` is written.
struct FileBefore(&'static str);

impl Observer<SimHost> for FileBefore {
  fn before_write(&mut self, host: &mut SimHost, entry: &LandingEntry) {
    if entry.path.as_ref() == self.0 {
      host.replace_file(self.0, b"an outsider's file");
    }
  }
}

/// AUD-29-05 (a directory that cannot be opened for its sync) and its sibling (a mkdir that met a file).
/// Do: the overlay makes `/new` with `/new/x` in it, and an outsider puts a file at `/new` just before the
/// mkdir. Expect: the mkdir is a type conflict, not "already there"; `/new/x` is skipped, its parent not a
/// directory; `/new` is reported `Unsynced` (it cannot be opened for its sync) and the directories are not
/// reported synced; the landing is `Partial`; and both entries stay in the overlay — nothing of the private
/// work is advanced over the outsider's file.
#[test]
fn a_directory_that_cannot_be_opened_for_its_sync_is_reported_and_the_private_work_stays() {
  let mut host = SimHost::new();
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  mkdir(&mut vol, &mut host, &mut store, "/new");
  write_file(&mut vol, &mut host, &mut store, "/new/x", b"private work");
  let mut history = History::new((host, store, vol));
  let report = history.land_with(request(1), &mut FileBefore("/new"));
  assert_eq!(
    outcome_of(&report, "/new"),
    Some(&Outcome::Conflict(ConflictClass::TypeChanged)),
    "{report:?}"
  );
  assert_eq!(
    outcome_of(&report, "/new/x"),
    Some(&Outcome::Skipped(SkipReason::ParentMissing)),
    "{report:?}"
  );
  assert!(
    report.degraded.contains(&Degradation::Unsynced {
      dir: "/new".into(),
      error: HostError::NotDirectory,
    }),
    "{:?}",
    report.degraded
  );
  assert!(!report.durability.dirs_synced);
  assert_eq!(report.state, LandingState::Partial);
  assert_eq!(
    history.diverged(),
    vec!["/new".to_owned(), "/new/x".to_owned()],
    "the private work stays in the overlay"
  );
  assert_eq!(history.host.bytes("/new").unwrap(), b"an outsider's file");
}

/// The one `Leftover` a report names: its path and the host's answer.
fn the_leftover(report: &LandingReport) -> (Box<str>, HostError) {
  let leftovers: Vec<(Box<str>, HostError)> = report
    .degraded
    .iter()
    .filter_map(|d| match d {
      Degradation::Leftover { path, error } => Some((path.clone(), *error)),
      _ => None,
    })
    .collect();
  assert_eq!(leftovers.len(), 1, "{:?}", report.degraded);
  leftovers.into_iter().next().unwrap()
}

/// AUD-29-05 (a full disk mid-write, then a cleanup that fails). Do: with named temporaries, the write-out
/// of the first replacement answers `ENOSPC` and the removal of its temporary is refused. Expect: that entry
/// fails `ENOSPC` and stays in the overlay, the other lands; the temporary is reported `Leftover` with the
/// refusal; the landing is `Partial`. Then a resume without faults sweeps the temporary, reaches the
/// reference, and leaves none of its names.
#[test]
fn a_full_disk_mid_write_and_a_temporary_left_behind_are_reported_and_the_resume_recovers() {
  let mut history = History::new(two_directories());
  history.host.set_unnamed_temporaries(false);
  history
    .host
    .fail(SimVerb::WriteOut, "", HostError::Unavailable(ENOSPC), 1);
  history.host.fail(
    SimVerb::Unlink,
    "/a/.slates-",
    HostError::Unavailable(EACCES),
    1,
  );
  let report = history.land(request(1));
  assert_eq!(
    outcome_of(&report, "/a/x"),
    Some(&Outcome::Failed { errno: ENOSPC }),
    "{report:?}"
  );
  assert_eq!(outcome_of(&report, "/b/y"), Some(&Outcome::Written));
  let (path, error) = the_leftover(&report);
  assert!(path.starts_with("/a/.slates-"), "{path}");
  assert_eq!(error, HostError::Unavailable(EACCES));
  assert_eq!(history.leftovers(), vec![path.to_string()]);
  assert_eq!(report.state, LandingState::Partial);
  assert_eq!(history.diverged(), vec!["/a/x".to_owned()]);
  let resumed = assert_resume_reaches_the_reference(&mut history, request(1));
  assert_eq!(resumed.swept, 1, "the resume swept the temporary");
  assert!(history.leftovers().is_empty(), "{:?}", history.leftovers());
}

/// AUD-29-05 (a sibling the sweep cannot remove). Do: an earlier attempt of landing 1 left a temporary in
/// `/a`; the resume's removal of it is refused, and the sync of `/a` answers `ENOSPC` so the landing has
/// more to do. Expect: the sibling is reported `Leftover` with the refusal and nothing swept (not a silent
/// zero), and `/a/x` is held. A further resume without faults sweeps the sibling, reaches the reference,
/// and leaves none of its names.
#[test]
fn a_sibling_the_sweep_cannot_remove_is_reported_and_a_later_resume_removes_it() {
  let mut history = History::new(two_directories());
  let sibling = format!("/a/.slates-{:016x}-0", 1);
  history
    .host
    .replace_file(&sibling, b"an earlier attempt's temporary");
  history.host.fail(
    SimVerb::Unlink,
    "/a/.slates-",
    HostError::Unavailable(EACCES),
    1,
  );
  history
    .host
    .fail(SimVerb::SyncDir, "/a", HostError::Unavailable(ENOSPC), 1);
  let report = history.land(request(1));
  assert_eq!(
    the_leftover(&report),
    (sibling.clone().into(), HostError::Unavailable(EACCES))
  );
  assert_eq!(report.swept, 0);
  assert_eq!(report.held, 1);
  assert_eq!(history.leftovers(), vec![sibling]);
  let resumed = assert_resume_reaches_the_reference(&mut history, request(1));
  assert_eq!(resumed.swept, 1);
  assert!(history.leftovers().is_empty(), "{:?}", history.leftovers());
}

/// AUD-29-05 (a failed requested media barrier, as against barriers only). Do: land both replacements with
/// media durability asked for while the media barrier answers `ENOSPC` once. Expect: `MediaUnsynced` with
/// that answer, the media not reached though asked for, both entries held and still private, `Partial`, and
/// no `BarriersOnly` (that cell is the supported level when media durability is not asked for). Then a
/// resume reaches the media and the reference.
#[test]
fn a_failed_media_barrier_holds_every_entry_until_a_resume_reaches_the_media() {
  let mut history = History::new(two_directories());
  history
    .host
    .fail(SimVerb::SyncMedia, "", HostError::Unavailable(ENOSPC), 1);
  let media = LandingRequest {
    media_durability: true,
    ..request(1)
  };
  let report = history.land(media.clone());
  assert_eq!(
    report
      .degraded
      .iter()
      .filter(|d| matches!(
        d,
        Degradation::MediaUnsynced { .. } | Degradation::BarriersOnly
      ))
      .collect::<Vec<_>>(),
    vec![&Degradation::MediaUnsynced {
      error: HostError::Unavailable(ENOSPC),
    }]
  );
  assert!(!report.durability.media && report.durability.media_requested);
  assert_eq!(report.held, 2, "{report:?}");
  assert_eq!(report.state, LandingState::Partial);
  assert_eq!(
    history.diverged(),
    vec!["/a/x".to_owned(), "/b/y".to_owned()]
  );
  let resumed = assert_resume_reaches_the_reference(&mut history, media);
  assert!(resumed.durability.media);
}

/// A-130 (a refused batch barrier). Do: land replacements in `/a` and `/b` — one batch, one barrier — while that
/// barrier answers `ENOSPC` once. Expect: both entries fail `ENOSPC` and neither is visible (both names keep their old
/// bytes); both temporaries are removed; the landing is `Partial` with both entries in the overlay and no barrier
/// counted. Then a resume without the fault reaches the reference with one barrier.
#[test]
fn a_refused_barrier_fails_its_whole_batch_and_shows_none_of_it() {
  let mut history = History::new(two_directories());
  history
    .host
    .fail(SimVerb::Barrier, "", HostError::Unavailable(ENOSPC), 1);
  let report = history.land(request(1));
  assert_the_batch_failed_unseen(&mut history, &report);
  let resumed = assert_resume_reaches_the_reference(&mut history, request(1));
  assert_eq!(resumed.barriers, 1, "{resumed:?}");
}

/// A-132 (a barrier refused while it runs away from the run). Do: land the two-directory batch sliced, its barrier
/// held away for three slices and its run answering `ENOSPC` once (an `fdatasync` the device fails on its own thread).
/// Expect: exactly as for a barrier refused when taken — both entries fail `ENOSPC`, neither is visible, both
/// temporaries are removed, the landing is `Partial` with no barrier counted — and a resume without the fault reaches
/// the reference with one barrier.
#[test]
fn a_barrier_refused_while_it_runs_away_fails_its_whole_batch_and_shows_none_of_it() {
  let mut history = History::new(two_directories());
  history.session = Session::sliced_holding_barriers(3);
  history
    .host
    .fail(SimVerb::BarrierRun, "", HostError::Unavailable(ENOSPC), 1);
  let report = history.land(request(1));
  assert_eq!(history.session.barriers_away, 1);
  assert_the_batch_failed_unseen(&mut history, &report);
  let resumed = assert_resume_reaches_the_reference(&mut history, request(1));
  assert_eq!(resumed.barriers, 1, "{resumed:?}");
}

/// Both entries of the two-directory batch failed `ENOSPC` at its barrier: neither visible, no temporary left, the
/// landing `Partial` with both entries private and no barrier counted.
fn assert_the_batch_failed_unseen(history: &mut History, report: &LandingReport) {
  for path in ["/a/x", "/b/y"] {
    assert_eq!(
      outcome_of(report, path),
      Some(&Outcome::Failed { errno: ENOSPC }),
      "{report:?}"
    );
  }
  assert_eq!(history.host.bytes("/a/x").unwrap(), b"old x");
  assert_eq!(history.host.bytes("/b/y").unwrap(), b"old y");
  assert!(history.leftovers().is_empty(), "{:?}", history.leftovers());
  assert!(history.host.visible_before_durable().is_empty());
  assert_eq!(report.state, LandingState::Partial, "{report:?}");
  assert_eq!(report.barriers, 0, "{report:?}");
  assert_eq!(
    history.diverged(),
    vec!["/a/x".to_owned(), "/b/y".to_owned()]
  );
}

/// The most host handles open at any entry's write.
struct OpenHandles {
  most: usize,
}

impl Observer<SimHost> for OpenHandles {
  fn before_write(&mut self, host: &mut SimHost, _entry: &LandingEntry) {
    self.most = self.most.max(host.open_handles());
  }
}

/// A-130 (the batch and its bound). Do: land, in one directory, more small new files than two batches hold and one
/// file larger than a batch, in one call and then in slices of one unit. Expect: every file written and none visible
/// before it was durable; one barrier per batch of at most a copy window of pages (each file at least one page) and
/// one for the large file alone — fewer barriers than files, the batching's non-vacuity — the same count sliced as in
/// one call; a full batch of temporaries open at its last write and never more; and every handle closed after.
#[test]
fn files_land_in_batches_of_at_most_a_window_of_pages_behind_one_barrier_each() {
  for session in [Session::new(), Session::sliced()] {
    land_small_files_and_one_large(session);
  }
}

/// [`files_land_in_batches_of_at_most_a_window_of_pages_behind_one_barrier_each`] under `session`.
fn land_small_files_and_one_large(mut session: Session) {
  let mut host = SimHost::new();
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let batch = store.content.chunk_bytes() / store.content.page();
  let small = 2 * batch + 3;
  for n in 0..small {
    let path = format!("/f{n:03}.txt");
    write_file(&mut vol, &mut host, &mut store, &path, path.as_bytes());
  }
  let large = vec![7u8; store.content.chunk_bytes() + 1];
  write_file(&mut vol, &mut host, &mut store, "/z.bin", &large);
  let target = root_target(&mut host);
  let before = host.open_handles();
  let mut observed = OpenHandles { most: 0 };
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land_with(request(1), &mut observed)
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(report.written, small + 1, "{report:?}");
  assert!(host.visible_before_durable().is_empty());
  assert_eq!(
    report.barriers,
    small.div_ceil(batch) + 1,
    "a barrier per batch of {batch} small files, one for the large file: {report:?}"
  );
  assert_eq!(
    observed.most + 1,
    before + batch,
    "a full batch's temporaries but its last open at that last one's write, beyond the {before} handles before"
  );
  assert_eq!(host.open_handles(), before, "every handle closed");
  assert_eq!(host.bytes("/z.bin").unwrap(), large);
}

/// What an abandoned run holds open when it is abandoned.
#[derive(Clone, Copy, Debug)]
enum Holding {
  /// A staged batch of small files.
  Batch,
  /// A file past one window, mid-copy.
  Copy,
}

/// AUD-29-25 with A-130 (an abandoned run). Do: step a granted landing one unit per slice and abandon it once it holds
/// temporaries open — a staged batch of small files, or a file past one window mid-copy — on a filesystem with unnamed
/// temporaries and on one without. Expect: every handle the run opened is closed; a later landing sweeps whatever the
/// run left at hidden names and reaches the overlay's bytes. Red before 2026-10-10 for the copy (its temporary's
/// handle stayed open in the host for as long as the host lived); a staged batch would have done the same.
#[test]
fn an_abandoned_landing_leaves_no_handle_open() {
  for holding in [Holding::Batch, Holding::Copy] {
    for unnamed in [true, false] {
      abandon_while_holding(holding, unnamed);
    }
  }
}

/// One case of [`an_abandoned_landing_leaves_no_handle_open`].
fn abandon_while_holding(holding: Holding, unnamed: bool) {
  let mut host = SimHost::new();
  host.set_unnamed_temporaries(unnamed);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let window = store.content.chunk_bytes();
  let (files, held): (Vec<(String, Vec<u8>)>, usize) = match holding {
    Holding::Batch => (
      (0..4)
        .map(|n| (format!("/f{n}.txt"), format!("file {n}").into_bytes()))
        .collect(),
      2,
    ),
    Holding::Copy => (vec![("/big.bin".to_owned(), vec![3u8; 3 * window + 1])], 1),
  };
  for (path, bytes) in &files {
    write_file(&mut vol, &mut host, &mut store, path, bytes);
  }
  let target = root_target(&mut host);
  let mut session = Session::sliced();
  let (req, lease) = granted_and_leased(&mut host, &target, &mut vol, &mut store, &mut session);
  let before = host.open_handles();
  let Ok(mut run) = begin_landing(
    HostSlot::Owned(host),
    &target,
    &mut vol,
    &mut store,
    &mut session.grants,
    Some(&lease),
    &mut session.audit,
    &req,
  ) else {
    panic!("{holding:?}: the landing did not begin");
  };
  while run.take_host().is_some_and(|lent| {
    let holds = lent.open_handles() >= before + held;
    run.put_host(lent);
    !holds
  }) {
    let ended = run.step(&mut vol, &mut store, &mut session.audit, &mut Unobserved, 0);
    assert!(
      ended.is_none() && !run.ready_to_finish(),
      "{holding:?}: ended before holding: {ended:?}"
    );
  }
  run.abandon();
  let mut host = run.into_host().unwrap();
  assert_eq!(
    host.open_handles(),
    before,
    "{holding:?} unnamed {unnamed}: every handle closed"
  );
  session.leases.release(&lease);
  let report = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut Session::new(),
  }
  .land(request(1))
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{holding:?}: {report:?}");
  for (path, bytes) in &files {
    assert_eq!(host.bytes(path).as_ref(), Some(bytes), "{holding:?} {path}");
  }
  let hidden: Vec<String> = host
    .paths()
    .into_iter()
    .map(|(path, _)| path)
    .filter(|path| path.contains("/.slates-"))
    .collect();
  assert!(hidden.is_empty(), "{holding:?}: {hidden:?}");
}

/// Landing 1's request, presented and granted once, and the target's lease taken for it.
fn granted_and_leased(
  host: &mut SimHost,
  target: &LandingTarget,
  vol: &mut Volume,
  store: &mut Store,
  session: &mut Session,
) -> (LandingRequest, slates_land::grant::LandingLease) {
  let mut req = request(1);
  let presented = Setup {
    host: &mut *host,
    target,
    vol,
    store,
    session: &mut *session,
  }
  .present(&req);
  req.grant = session.grants.issue(
    Surface::Cli,
    presented.manifest.hash,
    presented.binding.clone(),
    GrantScope::Once,
    req.now_ns,
    TERM_NS,
  );
  let key = target_lease_key(host, target);
  let lease = session
    .leases
    .take(&key, LEASE_HOLDER, req.now_ns, TERM_NS)
    .unwrap();
  (req, lease)
}
