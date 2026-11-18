//! A clone of a captured base landed against its capture (§4.15 `capture_base`, A-291's landing, stage 1; GAPS g267),
//! end to end over the simulated host: planned as the change since the capture, every entry judged by content, written
//! where the target still holds what the capture held, refused whole where an outsider changed it, and the clone left a
//! scratch volume.

use slates_mem::Handle;
use slates_vfs::dir::{Child, DirNode};
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::ids::SnapshotId;
use slates_vfs::volume::{Store, Volume};

use crate::engine::{
  Audit, Beginning, HostSlot, LandingRefusal, LandingReport, LandingRequest, LandingState,
  LandingTarget, Unobserved, begin_landing_from, settle_grant,
};
use crate::grant::{GrantScope, Grants, Leases, Surface, TargetIdentity, lease_key};
use crate::manifest::{Action, Filter, Judged};
use crate::source::Source;
use crate::test_store::{config, store};
use crate::verdict::{ConflictClass, Verdict};

/// Shape: a file's mode.
const FILE_MODE: u32 = 0o100_644;
/// Shape: a directory's mode.
const DIR_MODE: u32 = 0o040_755;
/// Shape: the lease and grant terms (monotonic ns); no test crosses them.
const TERM_NS: u64 = 1_000_000_000_000;
/// Shape: the large-file class boundary, one chunk window.
const LARGE: u64 = 65_536;
/// Shape: a file made executable (`chmod +x` of `FILE_MODE`).
const EXECUTABLE_MODE: u32 = 0o100_755;
/// Shape: an outsider's mode for a file, private to its owner.
const PRIVATE_MODE: u32 = 0o100_600;
/// Format: a mode's permission bits.
const PERMISSIONS: u32 = 0o7777;

fn dir(vol: &Volume, store: &Store, path: &str) -> Handle<DirNode> {
  match vol.resolve(store, path).unwrap().child {
    Child::Dir(handle) => handle,
    other => panic!("{path} is {other:?}, not a directory"),
  }
}

fn write(vol: &mut Volume, store: &mut Store, parent: &str, name: &str, bytes: &[u8]) {
  let at = dir(vol, store, parent);
  let no = vol.create_file(store, at, name, FILE_MODE).unwrap();
  vol.write(store, no, 0, bytes).unwrap();
}

/// The tree a capture read, on the volume and on the disk alike.
const CAPTURED: &[(&str, &[u8])] = &[
  ("/src/keep.rs", b"kept"),
  ("/src/edit.rs", b"old body"),
  ("/src/outsider.rs", b"as captured"),
  ("/gone.rs", b"removed by the agent"),
  ("/olddir/f.rs", b"removed with its directory"),
];

/// An origin holding [`CAPTURED`], its capture, and a clone of it the agent changed: `/src/edit.rs` rewritten,
/// `/src/made.rs` made, `/gone.rs` removed, `/olddir` removed with its file.
fn captured_clone(store: &mut Store) -> (Volume, SnapshotId, Volume) {
  let mut origin = Volume::create(store, config(1)).unwrap();
  for d in ["src", "olddir"] {
    let root = dir(&origin, store, "/");
    origin.mkdir(store, root, d, DIR_MODE).unwrap();
  }
  for (path, bytes) in CAPTURED {
    let (parent, name) = path.rsplit_once('/').unwrap();
    let parent = if parent.is_empty() { "/" } else { parent };
    write(&mut origin, store, parent, name, bytes);
  }
  let captured = origin.snapshot(store).unwrap();
  let mut clone = Volume::clone_without_base(store, &mut origin, captured, config(2)).unwrap();
  let edit = clone.resolve(store, "/src/edit.rs").unwrap().inode;
  clone.truncate(store, edit, 0).unwrap();
  clone.write(store, edit, 0, b"the agent's body").unwrap();
  write(&mut clone, store, "/src", "made.rs", b"made by the agent");
  let root = dir(&clone, store, "/");
  clone.unlink(store, root, "gone.rs").unwrap();
  let olddir = dir(&clone, store, "/olddir");
  clone.unlink(store, olddir, "f.rs").unwrap();
  let root = dir(&clone, store, "/");
  clone.rmdir(store, root, "olddir").unwrap();
  (origin, captured, clone)
}

/// A disk at `/target` holding what the capture read, with `edits` made since by an outsider.
fn disk(edits: &[(&str, &[u8])]) -> (SimHost, LandingTarget) {
  let mut host = SimHost::new();
  for d in ["/target", "/target/src", "/target/olddir"] {
    host.mkdir(d);
  }
  for (path, bytes) in CAPTURED.iter().chain(edits) {
    host.replace_file(&format!("/target{path}"), bytes);
  }
  host.close_files();
  let root = host.root();
  let target = LandingTarget {
    dir: host.open_dir(root, "target").unwrap(),
    key: "/target".into(),
  };
  (host, target)
}

fn request(grant: Option<crate::grant::GrantId>) -> LandingRequest {
  LandingRequest {
    landing_id: 1,
    consumer: b"consumer".to_vec().into_boxed_slice(),
    volume: [2; 16],
    snapshot: 1,
    source: Source::Head,
    grant,
    filter: Filter::default(),
    now_ns: 1,
    media_durability: false,
    large_class_bytes: LARGE,
  }
}

/// Presents the clone's landing against its capture, grants what was presented, and lands it.
fn land(
  host: &mut SimHost,
  target: &LandingTarget,
  store: &mut Store,
  (origin, captured, clone): (&Volume, SnapshotId, &mut Volume),
) -> (
  Box<crate::engine::Presented>,
  Result<LandingReport, LandingRefusal>,
) {
  let base = origin.frozen_tree(captured).unwrap();
  let mut grants = Grants::default();
  let mut audit = Audit::new(1 << 10);
  let presenting = Beginning::against_capture(clone, target, &request(None), base).unwrap();
  let presented = match begin_landing_from(
    presenting,
    HostSlot::Lent(&mut *host),
    clone,
    store,
    &mut grants,
    None,
    &mut audit,
  ) {
    Err((_, LandingRefusal::GrantRequired(presented))) => presented,
    Err((_, other)) => panic!("expected GrantRequired, got {other:?}"),
    Ok(_) => panic!("a landing with no grant ran"),
  };
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
  let granted = request(grant);
  let beginning = Beginning::against_capture(clone, target, &granted, base).unwrap();
  let result = match begin_landing_from(
    beginning,
    HostSlot::Lent(&mut *host),
    clone,
    store,
    &mut grants,
    Some(&lease),
    &mut audit,
  ) {
    Ok(mut run) => {
      let result = run.run_through(clone, store, &mut audit, &mut Unobserved);
      settle_grant(&mut grants, run.grant(), &result);
      result
    }
    Err((_, refusal)) => Err(refusal),
  };
  (presented, result)
}

/// A clone of a captured base lands what its agent changed since the capture, judged by content: the file it rewrote
/// is written over the untouched target, the file it made is created, the file and the directory it removed are
/// removed, and a file an outsider changed that the agent never touched is passed by, neither in the manifest nor
/// written. The clone stays a scratch volume (it takes no base from the target, so it still moves). Before A-291's
/// landing a captured clone planned every file as a create, so the rewritten file met the target's copy as
/// `CreateCreate`, the removals were never carried, and the outsider's file was a conflict too.
#[test]
fn a_captured_clone_lands_its_change_since_the_capture_and_passes_an_outsiders() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let (mut host, target) = disk(&[("/src/outsider.rs", b"an outsider's edit".as_slice())]);
  let (presented, result) = land(
    &mut host,
    &target,
    &mut store,
    (&origin, captured, &mut clone),
  );
  planned_the_change_since_the_capture(&presented);
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  landed_the_change_and_kept_the_rest(&host);
  assert!(!clone.is_overlay(), "the clone took a base from the target");
}

/// The judge of [`a_captured_clone_lands_its_change_since_the_capture_and_passes_an_outsiders`]'s presentation: judged
/// by content, the change since the capture in landing order (removals deepest first), every entry to apply.
fn planned_the_change_since_the_capture(presented: &crate::engine::Presented) {
  assert_eq!(presented.manifest.basis, Judged::Content);
  let planned: Vec<(&str, &Action)> = presented
    .manifest
    .entries
    .iter()
    .map(|e| (e.path.as_ref(), &e.action))
    .collect();
  assert_eq!(
    planned,
    vec![
      ("/src/made.rs", &Action::Create),
      ("/src/edit.rs", &Action::Replace),
      ("/olddir/f.rs", &Action::Delete),
      ("/gone.rs", &Action::Delete),
      ("/olddir", &Action::Rmdir),
    ],
    "the change since the capture, in landing order (removals deepest first)"
  );
  assert!(
    presented
      .preliminary
      .iter()
      .all(|r| r.verdict == Some(Verdict::Apply)),
    "{:?}",
    presented.preliminary
  );
}

/// The judge of [`a_captured_clone_lands_its_change_since_the_capture_and_passes_an_outsiders`]'s disk: the rewritten
/// and the made file hold the agent's bytes, the removed file and directory are gone, and the outsider's file and the
/// untouched one are as they were.
fn landed_the_change_and_kept_the_rest(host: &SimHost) {
  let bytes = |path: &str| host.bytes(path);
  assert_eq!(
    bytes("/target/src/edit.rs").as_deref(),
    Some(&b"the agent's body"[..])
  );
  assert_eq!(
    bytes("/target/src/made.rs").as_deref(),
    Some(&b"made by the agent"[..])
  );
  assert_eq!(bytes("/target/gone.rs"), None);
  assert!(
    !host
      .paths()
      .iter()
      .any(|(p, _)| p.starts_with("/target/olddir")),
    "{:?}",
    host.paths()
  );
  assert_eq!(
    bytes("/target/src/outsider.rs").as_deref(),
    Some(&b"an outsider's edit"[..])
  );
  assert_eq!(
    bytes("/target/src/keep.rs").as_deref(),
    Some(&b"kept"[..])
  );
}

/// An outsider's change to a file the agent also changed refuses the whole landing, `ModifyModify`, and writes
/// nothing; so does an outsider's file in a directory the agent removed (`DeleteModify`: only what the capture held is
/// removed with it).
#[test]
fn an_outsiders_change_under_the_agents_is_a_conflict_and_nothing_is_written() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let (mut host, target) = disk(&[
    ("/src/edit.rs", b"an outsider's body".as_slice()),
    ("/olddir/theirs.rs", b"an outsider's file".as_slice()),
  ]);
  let before = host.paths();
  let (presented, result) = land(
    &mut host,
    &target,
    &mut store,
    (&origin, captured, &mut clone),
  );
  let verdict_of = |path: &str| {
    presented
      .preliminary
      .iter()
      .find(|r| r.path.as_ref() == path)
      .and_then(|r| r.verdict)
  };
  assert_eq!(
    verdict_of("/src/edit.rs"),
    Some(Verdict::Conflict(ConflictClass::ModifyModify))
  );
  assert_eq!(
    verdict_of("/olddir"),
    Some(Verdict::Conflict(ConflictClass::DeleteModify))
  );
  assert!(result.is_err() || result.as_ref().is_ok_and(|r| r.written == 0));
  assert_eq!(host.paths(), before, "a conflicted landing wrote");
  assert_eq!(
    host.bytes("/target/src/edit.rs").as_deref(),
    Some(&b"an outsider's body"[..])
  );
}

/// A target that already holds what the agent wrote is accepted by identity and written over nothing, and a removal
/// already made is passed: the landing is idempotent by content.
#[test]
fn a_target_already_holding_the_change_is_accepted_and_written_over_nothing() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let (mut host, target) = disk(&[
    ("/src/edit.rs", b"the agent's body".as_slice()),
    ("/src/made.rs", b"made by the agent".as_slice()),
  ]);
  host.remove("/target/gone.rs");
  let (_, result) = land(
    &mut host,
    &target,
    &mut store,
    (&origin, captured, &mut clone),
  );
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  let outcome_of = |path: &str| {
    report
      .entries
      .iter()
      .find(|r| r.path.as_ref() == path)
      .and_then(|r| r.outcome.clone())
  };
  assert_eq!(
    outcome_of("/src/edit.rs"),
    Some(crate::engine::Outcome::AcceptedIdentical)
  );
  assert!(matches!(
    outcome_of("/gone.rs"),
    Some(crate::engine::Outcome::Skipped(_))
  ));
  assert_eq!(host.bytes("/target/olddir/f.rs"), None);
}

/// A-458. Do: in a captured clone, `chmod` `/src/keep.rs`, a file whose bytes the agent never changed, from 0644 to
/// 0755, and land the clone against its capture. Expect the change since the capture to hold the file, the landing
/// `Done`, and the target's file at 0755 with its bytes and its inode kept (no byte rewritten). Before A-458 a captured
/// clone's plan compared content alone, so it planned nothing for the file and its mode never landed.
#[test]
fn a_captured_clones_mode_change_alone_lands() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let keep = clone.resolve(&store, "/src/keep.rs").unwrap().inode;
  clone.chmod(&mut store, keep, EXECUTABLE_MODE).unwrap();
  let (mut host, target) = disk(&[]);
  let before = host.fingerprint("/target/src/keep.rs").unwrap();
  let (presented, result) = land(
    &mut host,
    &target,
    &mut store,
    (&origin, captured, &mut clone),
  );
  assert!(
    presented
      .manifest
      .entries
      .iter()
      .any(|e| e.path.as_ref() == "/src/keep.rs"),
    "{:?}",
    presented.manifest.entries
  );
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  let after = host.fingerprint("/target/src/keep.rs").unwrap();
  assert_eq!(after.mode & PERMISSIONS, EXECUTABLE_MODE & PERMISSIONS);
  assert_eq!(
    after.ino, before.ino,
    "the same file: its bytes never rewritten"
  );
  assert_eq!(
    host.bytes("/target/src/keep.rs").as_deref(),
    Some(&b"kept"[..])
  );
  assert!(
    !host.holds_unsynced_metadata("/target/src/keep.rs"),
    "synced by its batch's barrier (A-472)"
  );
}

/// The mode the target's file holds, its permission bits alone.
fn mode_on_disk(host: &SimHost, path: &str) -> u32 {
  host.fingerprint(path).unwrap().mode & PERMISSIONS
}

/// A-466. Do: in a captured clone whose agent rewrote `/src/edit.rs` and left its mode, have an outsider make the
/// target's file 0600, and land the clone against its capture. Expect the landing `Done` and the target's file holding
/// the agent's bytes with the outsider's 0600: the bytes keep their content verdict, and a mode the agent left alone is
/// the disk's. Before A-466 the replacement was staged with the clone's 0644, and the outsider's change was lost.
#[test]
fn a_captured_clones_replacement_keeps_an_outsiders_mode_it_left_alone() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let (mut host, target) = disk(&[]);
  host.chmod("/target/src/edit.rs", PRIVATE_MODE);
  let (_, result) = land(
    &mut host,
    &target,
    &mut store,
    (&origin, captured, &mut clone),
  );
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  assert_eq!(
    host.bytes("/target/src/edit.rs").as_deref(),
    Some(&b"the agent's body"[..])
  );
  assert_eq!(
    mode_on_disk(&host, "/target/src/edit.rs"),
    PRIVATE_MODE & PERMISSIONS,
    "the outsider's mode, which the agent never changed"
  );
}

/// A-466 (a mode both changed, differently; D-27: never inferred). Do: in a captured clone whose agent rewrote
/// `/src/edit.rs`, `chmod` it 0755, while an outsider makes the target's file 0600; present and land. Expect
/// `ModifyModify` at the file and nothing written there: the target's file keeps its captured bytes and the outsider's
/// 0600. Before A-466 the bytes' verdict alone judged it, and the clone's 0755 replaced the outsider's 0600.
#[test]
fn a_captured_clones_replacement_whose_mode_both_changed_differently_is_a_conflict() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let edit = clone.resolve(&store, "/src/edit.rs").unwrap().inode;
  clone.chmod(&mut store, edit, EXECUTABLE_MODE).unwrap();
  let (mut host, target) = disk(&[]);
  host.chmod("/target/src/edit.rs", PRIVATE_MODE);
  let (presented, result) = land(
    &mut host,
    &target,
    &mut store,
    (&origin, captured, &mut clone),
  );
  let verdict = presented
    .preliminary
    .iter()
    .find(|r| r.path.as_ref() == "/src/edit.rs")
    .and_then(|r| r.verdict);
  assert_eq!(
    verdict,
    Some(Verdict::Conflict(ConflictClass::ModifyModify))
  );
  assert!(result.is_err() || result.as_ref().is_ok_and(|r| r.written == 0));
  assert_eq!(
    host.bytes("/target/src/edit.rs").as_deref(),
    Some(&b"old body"[..])
  );
  assert_eq!(
    mode_on_disk(&host, "/target/src/edit.rs"),
    PRIVATE_MODE & PERMISSIONS
  );
}

/// A-466 (the bytes already there, the mode not). Do: in a captured clone whose agent rewrote `/src/edit.rs`, `chmod` it
/// 0755, while an outsider writes the agent's bytes into the target's file and leaves its 0644; land. Expect the landing
/// `Done` and the target's file at 0755 with the agent's bytes under its own inode: the bytes accepted as identical and
/// the mode set alone, no byte rewritten. Before A-466 the whole entry was accepted as identical, and the agent's mode
/// never landed.
#[test]
fn a_captured_clones_replacement_already_on_the_disk_still_lands_its_mode() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let edit = clone.resolve(&store, "/src/edit.rs").unwrap().inode;
  clone.chmod(&mut store, edit, EXECUTABLE_MODE).unwrap();
  let (mut host, target) = disk(&[]);
  host.write_in_place("/target/src/edit.rs", b"the agent's body", 1);
  let before = host.fingerprint("/target/src/edit.rs").unwrap();
  let (_, result) = land(
    &mut host,
    &target,
    &mut store,
    (&origin, captured, &mut clone),
  );
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  assert_eq!(
    mode_on_disk(&host, "/target/src/edit.rs"),
    EXECUTABLE_MODE & PERMISSIONS
  );
  assert_eq!(
    host.fingerprint("/target/src/edit.rs").unwrap().ino,
    before.ino,
    "the same file: its bytes never rewritten"
  );
  assert_eq!(
    host.bytes("/target/src/edit.rs").as_deref(),
    Some(&b"the agent's body"[..])
  );
  assert!(
    !host.holds_unsynced_metadata("/target/src/edit.rs"),
    "synced by its batch's barrier (A-472)"
  );
}
