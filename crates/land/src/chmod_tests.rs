//! A file whose only change is its mode or its Windows bits, landed (A-458; §4.15 step 1's "mode changes", the data
//! model's `Chmod`): an overlay over the simulated host, planned, presented, granted and landed through the engine,
//! against a disk that kept the file as the volume witnessed it, already holds the volume's change, or changed it
//! another way. Its bytes are never rewritten: the same file keeps its inode.

use slates_vfs::base::BaseConfig;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::inode::{Kind, WindowsAttributes};
use slates_vfs::volume::{Store, Volume};

use crate::engine::{
  Audit, LandingRefusal, LandingReport, LandingRequest, LandingState, LandingTarget, Outcome,
  Unobserved, land,
};
use crate::grant::{GrantScope, Grants, Leases, Surface, TargetIdentity, lease_key};
use crate::manifest::Filter;
use crate::source::Source;
use crate::test_store::{config, store};
use crate::verdict::{ConflictClass, Verdict};

/// Shape: the inode-number prefix of these tests' volumes.
const PREFIX: u16 = 3;
/// Shape: the lease and grant terms (monotonic ns); no test crosses them.
const TERM_NS: u64 = 1_000_000_000_000;
/// Shape: the large-file class boundary, one chunk window.
const LARGE: u64 = 65_536;
/// Shape: how far the simulated clock moves between making the disk and mounting it, so the base predates its mount
/// (as `tests/common`'s `SETTLED_NS`).
const SETTLED_NS: i64 = 1_000_000_000;
/// Shape: the audit records a test keeps.
const AUDIT_RECORDS: usize = 1 << 10;
/// Format: a mode's permission bits.
const PERMISSIONS: u32 = 0o7777;
/// Shape: a script as the disk holds it, read-write for its owner (`S_IFREG | 0644`).
const SCRIPT_MODE: u32 = 0o100_644;
/// Shape: the same script made executable (`chmod +x`).
const EXECUTABLE_MODE: u32 = 0o100_755;
/// Shape: an outsider's mode for the script, private to its owner.
const PRIVATE_MODE: u32 = 0o100_600;
/// Shape: the script's bytes.
const SCRIPT: &[u8] = b"#!/bin/sh\necho landed\n";
/// Shape: a base directory as the disk holds it (`S_IFDIR | 0755`).
const DIR_MODE: u32 = 0o040_755;
/// Shape: the same directory made private to its owner.
const PRIVATE_DIR_MODE: u32 = 0o040_700;
/// Shape: a modification time a `touch` gives the script, past every time the fixture's disk holds.
const TOUCHED_NS: i64 = 5_000_000_000;
/// Shape: the base directory opened to its group again, the head's later mode past a snapshot.
const GROUP_DIR_MODE: u32 = 0o040_750;
/// Shape: the landing ids of a head's landing and of a snapshot's, distinct so the two landings of one test never share
/// a hidden name.
const HEAD_LANDING: u64 = 1;
/// Shape: see `HEAD_LANDING`.
const SNAPSHOT_LANDING: u64 = 2;

/// A disk holding `/run.sh` (the script, at `SCRIPT_MODE`) and the directory `/lib` (at `DIR_MODE`) with a file in it, and
/// an overlay of it, the disk settled before the mount.
fn overlay_of_a_script(store: &mut Store) -> (SimHost, Volume) {
  let mut host = SimHost::new();
  host.replace_file("/run.sh", SCRIPT);
  host.chmod("/run.sh", SCRIPT_MODE);
  host.mkdir("/lib");
  host.chmod("/lib", DIR_MODE);
  host.replace_file("/lib/mod.rs", SCRIPT);
  host.close_files();
  host.advance_ns(SETTLED_NS);
  let root = host.root();
  let facts = host.facts(root).unwrap();
  let vol = Volume::create_overlay(
    store,
    config(PREFIX),
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap();
  (host, vol)
}

/// The script's inode in the overlay.
fn script(vol: &mut Volume, host: &mut SimHost, store: &mut Store) -> slates_vfs::ids::InodeNo {
  vol.with_host(host).resolve(store, "/run.sh").unwrap().inode
}

fn request(grant: Option<crate::grant::GrantId>, source: Source, landing_id: u64) -> LandingRequest {
  LandingRequest {
    landing_id,
    consumer: b"consumer".to_vec().into_boxed_slice(),
    volume: [3; 16],
    snapshot: 1,
    source,
    grant,
    filter: Filter::default(),
    now_ns: 1,
    media_durability: false,
    large_class_bytes: LARGE,
  }
}

/// A landing's presentation and its granted run: the presentation's preliminary verdicts by path, and the granted
/// landing's result.
type Landed = (
  Vec<(Box<str>, Option<Verdict>)>,
  Result<LandingReport, LandingRefusal>,
);

/// Presents the overlay's landing into the disk's root, grants what was presented, and lands it.
fn land_granted(host: &mut SimHost, vol: &mut Volume, store: &mut Store) -> Landed {
  land_granted_from(host, vol, store, Source::Head, HEAD_LANDING)
}

/// [`land_granted`] of `source` (the head, or a snapshot of the volume) as landing `landing_id`.
fn land_granted_from(
  host: &mut SimHost,
  vol: &mut Volume,
  store: &mut Store,
  source: Source,
  landing_id: u64,
) -> Landed {
  let target = LandingTarget {
    dir: host.root(),
    key: "/".into(),
  };
  let mut grants = Grants::default();
  let mut audit = Audit::new(AUDIT_RECORDS);
  let presented = match land(
    host,
    &target,
    vol,
    store,
    &mut grants,
    None,
    &mut audit,
    &request(None, source, landing_id),
    &mut Unobserved,
  ) {
    Err(LandingRefusal::GrantRequired(presented)) => presented,
    other => panic!("expected GrantRequired, got {other:?}"),
  };
  let preliminary = presented
    .preliminary
    .iter()
    .map(|report| (report.path.clone(), report.verdict))
    .collect();
  let grant = grants.issue(
    Surface::Cli,
    presented.manifest.hash,
    presented.binding.clone(),
    GrantScope::Once,
    1,
    TERM_NS,
  );
  let identity = host.fingerprint_dir(target.dir).unwrap();
  let key = lease_key(&TargetIdentity {
    key: target.key.clone(),
    device: identity.dev,
    inode: identity.ino,
  });
  let lease = Leases::default().take(&key, 1, 1, TERM_NS).unwrap();
  let landed = land(
    host,
    &target,
    vol,
    store,
    &mut grants,
    Some(&lease),
    &mut audit,
    &request(grant, source, landing_id),
    &mut Unobserved,
  );
  (preliminary, landed)
}

/// Each entry's outcome by path.
fn outcomes(report: &LandingReport) -> Vec<(&str, Option<Outcome>)> {
  report
    .entries
    .iter()
    .map(|entry| (entry.path.as_ref(), entry.outcome.clone()))
    .collect()
}

/// A-458 (§4.15 step 1's mode changes). Do: over a disk holding `/run.sh` at 0644, `chmod` it to 0755 through the
/// overlay with its bytes untouched, and land the overlay. Expect the landing `Done` with the entry written, the disk's
/// file at 0755 holding the same bytes under the same inode (no byte rewritten: no temporary, no exchange) and no byte
/// counted written. Before A-458 the verdict passed the entry `Skip`, its bytes being unchanged, and the file stayed 0644.
#[test]
fn a_base_files_mode_changed_alone_lands_and_no_byte_is_rewritten() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let no = script(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .chmod(&mut store, no, EXECUTABLE_MODE)
    .unwrap();
  let before = host.fingerprint("/run.sh").unwrap();
  let (_, landed) = land_granted(&mut host, &mut vol, &mut store);
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(outcomes(&report), vec![("/run.sh", Some(Outcome::Written))]);
  let after = host.fingerprint("/run.sh").unwrap();
  assert_eq!(after.mode & PERMISSIONS, EXECUTABLE_MODE & PERMISSIONS);
  assert_eq!(
    after.ino, before.ino,
    "the same file: its bytes never rewritten"
  );
  assert_eq!(host.bytes("/run.sh").as_deref(), Some(SCRIPT));
  assert_eq!(report.bytes_written, 0, "{report:?}");
}

/// A-472 (a landed mode is durable before it is reported). Do: `chmod` `/run.sh` through the overlay and land it over the
/// simulated host, whose sync model holds a mode set in place unsynced until a barrier names its file. Expect the entry
/// written in a batch whose one barrier synced the file: no mode a power loss could take back, the file still the one it
/// was. Before A-472 the mode was set and reported with no sync of the file, its directory's sync the only one, which
/// fsync(2) does not say covers another inode's metadata.
#[test]
fn a_metadata_entry_is_synced_by_its_batchs_barrier_before_it_is_reported() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let no = script(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .chmod(&mut store, no, EXECUTABLE_MODE)
    .unwrap();
  let (_, landed) = land_granted(&mut host, &mut vol, &mut store);
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(outcomes(&report), vec![("/run.sh", Some(Outcome::Written))]);
  assert_eq!(report.barriers, 1, "the batch's barrier: {report:?}");
  assert!(
    !host.holds_unsynced_metadata("/run.sh"),
    "the mode a power loss could take back"
  );
}

/// A-501 (a `touch` alone). Do: give `/run.sh` a new modification time through the overlay, its bytes and mode untouched,
/// and land. Expect the entry written in a batch whose barrier synced it, the disk's file holding the new time under the
/// same inode. Before A-501 the entry was a replacement its verdict passed by (its bytes unchanged), and the time never
/// landed.
#[test]
fn a_touch_alone_lands_and_is_synced() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let no = script(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .set_times(&mut store, no, None, Some(TOUCHED_NS), None, None)
    .unwrap();
  let before = host.fingerprint("/run.sh").unwrap();
  let (_, landed) = land_granted(&mut host, &mut vol, &mut store);
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(outcomes(&report), vec![("/run.sh", Some(Outcome::Written))]);
  let after = host.fingerprint("/run.sh").unwrap();
  assert_eq!(after.mtime_ns, TOUCHED_NS);
  assert_eq!(after.ino, before.ino, "the same file: its bytes never rewritten");
  assert_eq!(after.mode & PERMISSIONS, SCRIPT_MODE & PERMISSIONS);
  assert!(!host.holds_unsynced_metadata("/run.sh"));
}

/// A-501 (a base directory's mode alone). Do: `chmod` the base directory `/lib` to 0700 through the overlay, nothing else
/// changed, and land. Expect its entry written in a batch whose barrier synced it, the disk's directory at 0700 under the
/// same inode, its file untouched. Before A-501 a base directory whose mode alone changed was in no diverged set, and the
/// mode never landed.
#[test]
fn a_base_directorys_mode_alone_lands_and_is_synced() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let lib = vol
    .with_host(&mut host)
    .resolve(&mut store, "/lib")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .chmod(&mut store, lib, PRIVATE_DIR_MODE)
    .unwrap();
  let before = host.fingerprint("/lib").unwrap();
  let (_, landed) = land_granted(&mut host, &mut vol, &mut store);
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(outcomes(&report), vec![("/lib", Some(Outcome::Written))]);
  let after = host.fingerprint("/lib").unwrap();
  assert_eq!(after.mode & PERMISSIONS, PRIVATE_DIR_MODE & PERMISSIONS);
  assert_eq!(after.ino, before.ino, "the same directory");
  assert!(!host.holds_unsynced_metadata("/lib"));
  assert_eq!(host.bytes("/lib/mod.rs").as_deref(), Some(SCRIPT));
}

/// A-501 (a directory's later mode survives a snapshot's landing). Do: `chmod` the base directory `/lib` to 0700 through
/// the overlay, snapshot the volume, `chmod` `/lib` to 0750 at the head; land the snapshot, then the head. Expect the
/// snapshot's landing to leave `/lib` at 0700 with the head still diverged there, and the head's landing to leave it at
/// 0750: the head's later mode rebased onto what landed. Forgotten with the landed entry, it would never land.
#[test]
fn a_directorys_mode_the_head_changed_past_a_landed_snapshot_lands_next() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let lib = vol
    .with_host(&mut host)
    .resolve(&mut store, "/lib")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .chmod(&mut store, lib, PRIVATE_DIR_MODE)
    .unwrap();
  let snapshot = vol.snapshot(&mut store).unwrap();
  vol
    .with_host(&mut host)
    .chmod(&mut store, lib, GROUP_DIR_MODE)
    .unwrap();
  let (_, landed) = land_granted_from(
    &mut host,
    &mut vol,
    &mut store,
    Source::Snapshot(snapshot),
    SNAPSHOT_LANDING,
  );
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(outcomes(&report), vec![("/lib", Some(Outcome::Written))]);
  let mode = |host: &SimHost| host.fingerprint("/lib").unwrap().mode & PERMISSIONS;
  assert_eq!(mode(&host), PRIVATE_DIR_MODE & PERMISSIONS);
  let diverged: Vec<String> = vol.diverged(&store).into_iter().map(|d| d.path).collect();
  assert_eq!(diverged, vec!["/lib"], "the head's later mode, still to land");
  let (_, landed) = land_granted(&mut host, &mut vol, &mut store);
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(outcomes(&report), vec![("/lib", Some(Outcome::Written))]);
  assert_eq!(mode(&host), GROUP_DIR_MODE & PERMISSIONS);
}

/// A-458 (a field the volume kept is the disk's, as bytes are). Do: mark `/run.sh` hidden through the overlay (its Windows
/// bits) and leave its mode, while an outsider makes the disk's file 0600; land. Expect the landing `Done` with the entry
/// written: the disk's file holds the hidden bit the volume set and keeps the 0600 the outsider set, its bytes and inode
/// as they were. Before A-458 the bits never landed, the entry passed by as unchanged.
#[test]
fn a_mode_the_disk_changed_while_the_volume_kept_it_is_passed_by_and_the_volumes_bits_land() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let no = script(&mut vol, &mut host, &mut store);
  let hidden = WindowsAttributes::kept(u32::from(WindowsAttributes::HIDDEN), Kind::File);
  vol
    .with_host(&mut host)
    .set_windows_attributes(&mut store, no, hidden)
    .unwrap();
  host.advance_ns(1);
  host.chmod("/run.sh", PRIVATE_MODE);
  let before = host.fingerprint("/run.sh").unwrap();
  let (_, landed) = land_granted(&mut host, &mut vol, &mut store);
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(outcomes(&report), vec![("/run.sh", Some(Outcome::Written))]);
  let after = host.fingerprint("/run.sh").unwrap();
  assert_eq!(
    after.windows,
    WindowsAttributes::HIDDEN,
    "the volume's bits"
  );
  assert_eq!(
    after.mode & PERMISSIONS,
    PRIVATE_MODE & PERMISSIONS,
    "the outsider's mode, which the volume never changed"
  );
  assert_eq!(after.ino, before.ino);
  assert_eq!(host.bytes("/run.sh").as_deref(), Some(SCRIPT));
}

/// A-458 (the verdict's identity, for a mode). Do: `chmod` `/run.sh` to 0755 through the overlay, and have an outsider
/// make the disk's file 0755 too; land. Expect the entry accepted as identical (nothing set) and the landing `Done`.
#[test]
fn a_mode_the_disk_already_holds_is_accepted_as_identical() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let no = script(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .chmod(&mut store, no, EXECUTABLE_MODE)
    .unwrap();
  host.advance_ns(1);
  host.chmod("/run.sh", EXECUTABLE_MODE);
  let (_, landed) = land_granted(&mut host, &mut vol, &mut store);
  let report = landed.unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(
    outcomes(&report),
    vec![("/run.sh", Some(Outcome::AcceptedIdentical))]
  );
  assert_eq!(
    host.fingerprint("/run.sh").unwrap().mode & PERMISSIONS,
    EXECUTABLE_MODE & PERMISSIONS
  );
}

/// A-458 (the verdict's conflict, for a mode; D-27: never inferred). Do: `chmod` `/run.sh` to 0755 through the overlay
/// while an outsider makes the disk's file 0600; present and land. Expect the presentation to find `ModifyModify` at the
/// file, and the landing to write nothing: the disk's file stays 0600.
#[test]
fn a_mode_both_changed_differently_is_a_conflict_and_nothing_is_written() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let no = script(&mut vol, &mut host, &mut store);
  vol
    .with_host(&mut host)
    .chmod(&mut store, no, EXECUTABLE_MODE)
    .unwrap();
  host.advance_ns(1);
  host.chmod("/run.sh", PRIVATE_MODE);
  let (preliminary, landed) = land_granted(&mut host, &mut vol, &mut store);
  assert_eq!(
    preliminary,
    vec![(
      "/run.sh".into(),
      Some(Verdict::Conflict(ConflictClass::ModifyModify))
    )]
  );
  assert!(
    landed.is_err() || landed.as_ref().is_ok_and(|report| report.written == 0),
    "{landed:?}"
  );
  assert_eq!(
    host.fingerprint("/run.sh").unwrap().mode & PERMISSIONS,
    PRIVATE_MODE & PERMISSIONS
  );
}

/// A-466's sibling for an overlay (T-1.14; §4.15 step 6). Do: rewrite `/run.sh` through the overlay and leave its mode,
/// while an outsider makes the disk's file 0600; present and land. Expect `ModifyModify` at the file and nothing written:
/// an overlay's replacement needs its witnessed file whole before its exchange, so the outsider's mode is never written
/// over and the landing is refused instead, the disk keeping its bytes and the outsider's 0600.
#[test]
fn an_overlays_replacement_over_an_outsiders_mode_is_refused_and_the_mode_kept() {
  let mut store = store();
  let (mut host, mut vol) = overlay_of_a_script(&mut store);
  let no = script(&mut vol, &mut host, &mut store);
  {
    let mut overlay = vol.with_host(&mut host);
    overlay.truncate(&mut store, no, 0).unwrap();
    overlay
      .write(&mut store, no, 0, b"#!/bin/sh\necho rewritten\n")
      .unwrap();
  }
  host.advance_ns(1);
  host.chmod("/run.sh", PRIVATE_MODE);
  let (preliminary, landed) = land_granted(&mut host, &mut vol, &mut store);
  assert_eq!(
    preliminary,
    vec![(
      "/run.sh".into(),
      Some(Verdict::Conflict(ConflictClass::ModifyModify))
    )]
  );
  assert!(
    landed.is_err() || landed.as_ref().is_ok_and(|report| report.written == 0),
    "{landed:?}"
  );
  assert_eq!(host.bytes("/run.sh").as_deref(), Some(SCRIPT));
  assert_eq!(
    host.fingerprint("/run.sh").unwrap().mode & PERMISSIONS,
    PRIVATE_MODE & PERMISSIONS
  );
}
