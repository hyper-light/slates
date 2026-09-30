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
  Degradation, LandingReport, LandingRequest, LandingState, LandingTarget, Observer, Outcome,
  SkipReason,
};
use slates_land::manifest::LandingEntry;
use slates_land::verdict::ConflictClass;
use slates_vfs::base::BaseConfig;
use slates_vfs::host::sim::{SimHost, SimVerb};
use slates_vfs::host::{HostError, HostFs};
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{LARGE, Session, Setup, config, mkdir, request, store, write_file};

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

/// AUD-29-05 (a full disk mid-write, then a cleanup that fails). Do: with named temporaries, the data sync
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
    .fail(SimVerb::SyncFile, "", HostError::Unavailable(ENOSPC), 1);
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
