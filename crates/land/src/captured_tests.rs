//! A clone of a captured base landed against its capture (§4.15 `capture_base`, A-291's landing, stage 1; GAPS g267),
//! end to end over the simulated host: planned as the change since the capture, every entry judged by content, written
//! where the target still holds what the capture held, refused whole where an outsider changed it, and the clone left a
//! scratch volume. And a clone that moved to another member (A-503, stage 2): its head made again in a volume of its
//! own, as the new owner materializes it, and landed there against what its move carried of the capture and its mark,
//! with the same entries as the clone's own landing against the capture.

use slates_mem::Handle;
use slates_vfs::captured::{CaptureBase, CarryWalk, FrozenTree, Held};
use slates_vfs::dir::{Child, DirNode};
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::{SimHost, SimVerb};
use slates_vfs::ids::SnapshotId;
use slates_vfs::names::NameEquivalence;
use slates_vfs::volume::{Store, Volume};

use crate::engine::{
  Audit, Beginning, Degradation, HostSlot, LandingRefusal, LandingReport, LandingRequest,
  LandingState, LandingTarget, Outcome, Unobserved, begin_landing_from, settle_grant,
};
use crate::grant::{GrantScope, Grants, Leases, Surface, TargetIdentity, lease_key};
use crate::manifest::{Action, Filter, Judged};
use crate::source::Source;
use crate::test_store::{REFUSALS_OF_EVERY_KIND, config, store};
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
  let base = CaptureBase::Tree(origin.frozen_tree(captured).unwrap());
  land_against(host, target, store, (base, clone))
}

/// Presents `clone`'s landing against `base`, grants what was presented, and lands it.
fn land_against(
  host: &mut SimHost,
  target: &LandingTarget,
  store: &mut Store,
  (base, clone): (CaptureBase, &mut Volume),
) -> (
  Box<crate::engine::Presented>,
  Result<LandingReport, LandingRefusal>,
) {
  let mut grants = Grants::default();
  let mut audit = Audit::new(1 << 10);
  let presenting = Beginning::against_capture(clone, target, &request(None), base.clone()).unwrap();
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
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  assert_the_change_landed_and_the_outsiders_passed(&host);
  assert!(!clone.is_overlay(), "the clone took a base from the target");
}

/// The target after the captured clone's landing: the rewritten file holds the agent's body, the made file is there, the
/// removed file and directory are gone, and the outsider's file and the untouched one are as they were.
fn assert_the_change_landed_and_the_outsiders_passed(host: &SimHost) {
  assert_eq!(
    host.bytes("/target/src/edit.rs").as_deref(),
    Some(&b"the agent's body"[..])
  );
  assert_eq!(
    host.bytes("/target/src/made.rs").as_deref(),
    Some(&b"made by the agent"[..])
  );
  assert_eq!(host.bytes("/target/gone.rs"), None);
  assert!(
    !host
      .paths()
      .iter()
      .any(|(p, _)| p.starts_with("/target/olddir")),
    "{:?}",
    host.paths()
  );
  assert_eq!(
    host.bytes("/target/src/outsider.rs").as_deref(),
    Some(&b"an outsider's edit"[..])
  );
  assert_eq!(
    host.bytes("/target/src/keep.rs").as_deref(),
    Some(&b"kept"[..])
  );
}

/// A-539 (a refusal read by its kind, never its number). Do: land the captured clone, its `/olddir` removed with its
/// file, over a disk whose host refuses that directory's removal, once for each kind of refusal the seam names (among
/// them a name that exists, `EEXIST`, which POSIX lets `rmdir` answer for a directory that is not empty), each over a
/// fresh disk. Expect the directory the target in use every time and kept on the disk, the rest landed, and the landing
/// `Partial`, never crashed: a removal the host refuses is the target in use, whatever its kind. Before, the removal read
/// `Unavailable` alone so: a name that exists or a verb the filesystem lacks ended the whole landing, `Aborted`, and no
/// room failed the entry.
#[test]
fn a_directory_whose_removal_the_host_refuses_is_the_target_in_use_whatever_the_kind() {
  for refusal in REFUSALS_OF_EVERY_KIND {
    let mut store = store();
    let (origin, captured, mut clone) = captured_clone(&mut store);
    let (mut host, target) = disk(&[]);
    host.fail(SimVerb::Rmdir, "/target/olddir", refusal, 1);
    let (_, result) = land(
      &mut host,
      &target,
      &mut store,
      (&origin, captured, &mut clone),
    );
    let report = result.unwrap();
    let removal = report
      .entries
      .iter()
      .find(|entry| &*entry.path == "/olddir")
      .and_then(|entry| entry.outcome.clone());
    assert_eq!(
      removal,
      Some(Outcome::Conflict(ConflictClass::TargetInUse)),
      "{refusal:?}: {report:?}"
    );
    assert!(
      !report
        .degraded
        .iter()
        .any(|degraded| matches!(degraded, Degradation::Crashed { .. })),
      "{refusal:?}: {report:?}"
    );
    assert_eq!(report.state, LandingState::Partial, "{refusal:?}");
    assert!(
      host
        .paths()
        .iter()
        .any(|(path, _)| path.as_str() == "/target/olddir"),
      "{refusal:?}: the directory kept"
    );
    assert_eq!(
      host.bytes("/target/src/made.rs").as_deref(),
      Some(&b"made by the agent"[..]),
      "{refusal:?}: the rest landed"
    );
  }
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

/// Shape: the inode-number prefix of the volume a move makes on its new owner (the origin's is 1, the clone's 2).
const MOVED_PREFIX: u16 = 3;

/// `from`'s whole tree made again in `to`, every directory, file and link with its mode and bytes, no node shared with
/// it: the volume a move's new owner materializes from the head it was handed (A-291).
fn copy_tree(store: &mut Store, from: FrozenTree, to: &mut Volume) {
  let mut stack = vec![(String::from("/"), from.root)];
  while let Some((at, node)) = stack.pop() {
    let entries: Vec<(String, Child)> = store
      .dirs
      .get(node)
      .unwrap()
      .iter(&store.blocks)
      .map(|entry| (entry.name.to_owned(), entry.child))
      .collect();
    for (name, child) in entries {
      let path = if at == "/" {
        format!("/{name}")
      } else {
        format!("{at}/{name}")
      };
      let parent = dir(to, store, &at);
      match slates_vfs::captured::held_at(store, from, NameEquivalence::Exact, &path).unwrap() {
        Held::Dir { mode, .. } => {
          to.mkdir(store, parent, &name, mode).unwrap();
          if let Child::Dir(child) = child {
            stack.push((path, child));
          }
        }
        Held::File { no, size, mode, .. } => {
          let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
          let read = slates_vfs::captured::read_file(store, from, no, 0, &mut bytes).unwrap();
          bytes.truncate(read);
          let made = to.create_file(store, parent, &name, mode).unwrap();
          to.write(store, made, 0, &bytes).unwrap();
        }
        Held::Symlink { target, .. } => {
          to.symlink(store, parent, &name, &target).unwrap();
        }
        other => panic!("{path} holds {other:?}, which no test tree has"),
      }
    }
  }
}

/// The clone of [`captured_clone`] moved to another member (A-291's move, A-503's carry): the capture carried against
/// the head the move hands over, that head made again in a volume of the new owner's, and the snapshot of it the new
/// owner keeps as its mark. Returns the moved volume and its capture base.
fn move_clone(
  store: &mut Store,
  origin: &Volume,
  captured: SnapshotId,
  clone: &mut Volume,
) -> (Volume, CaptureBase) {
  let handed = clone.snapshot(store).unwrap();
  let handed_tree = clone.frozen_tree(handed).unwrap();
  let mut walk = CarryWalk::new(
    CaptureBase::Tree(origin.frozen_tree(captured).unwrap()),
    handed_tree,
  );
  let carried = loop {
    if let Some(carried) = walk.unit(store, NameEquivalence::Exact).unwrap() {
      break carried;
    }
  };
  let mut moved = Volume::create(store, config(MOVED_PREFIX)).unwrap();
  copy_tree(store, handed_tree, &mut moved);
  let mark = moved.snapshot(store).unwrap();
  let base = CaptureBase::Carried {
    mark: moved.frozen_tree(mark).unwrap(),
    carried,
  };
  (moved, base)
}

/// `bytes` written over `path` of `vol`, as an agent's tool rewrites a file.
fn rewrite(vol: &mut Volume, store: &mut Store, path: &str, bytes: &[u8]) {
  let no = vol.resolve(store, path).unwrap().inode;
  vol.truncate(store, no, 0).unwrap();
  vol.write(store, no, 0, bytes).unwrap();
}

/// A manifest's entries as a landing judges them, one line each: path, action, the identity each is judged against, and
/// what it writes (bytes, size and mode; a moved volume's own modification times are its own).
fn judged(manifest: &crate::manifest::Manifest) -> Vec<String> {
  manifest
    .entries
    .iter()
    .map(|e| {
      format!(
        "{} {:?} {:?} {:?}",
        e.path,
        e.action,
        e.witnessed.as_ref().map(|w| w.identity),
        e.overlay.as_ref().map(|o| (o.hash, o.size, o.mode))
      )
    })
    .collect()
}

/// `vol`'s head planned against `base` alone, as a presentation plans it.
fn planned(vol: &mut Volume, store: &mut Store, base: CaptureBase) -> crate::manifest::Manifest {
  let mut host = SimHost::new();
  let mut walk = crate::manifest::PlanWalk::against_capture(vol, Source::Head, base).unwrap();
  loop {
    if let Some(manifest) = walk
      .unit(vol, store, &mut host, &Filter::default())
      .unwrap()
    {
      return manifest;
    }
  }
}

/// The presentation of [`a_moved_captured_clone_lands_its_change_since_the_capture_and_passes_an_outsiders`]: judged by
/// content, the change since the capture before the move and after it, in landing order, every entry to apply.
fn assert_planned_since_the_capture_across_the_move(presented: &crate::engine::Presented) {
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
      ("/src/keep.rs", &Action::Replace),
      ("/olddir/f.rs", &Action::Delete),
      ("/gone.rs", &Action::Delete),
      ("/olddir", &Action::Rmdir),
    ],
    "the change since the capture, before the move and after it, in landing order"
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

/// The target [`a_moved_captured_clone_lands_its_change_since_the_capture_and_passes_an_outsiders`] leaves: the
/// rewritten, the later and the made file written, the removed file and directory gone, the outsider's file as it was.
fn assert_the_moved_change_landed_and_the_outsiders_passed(host: &SimHost) {
  let bytes = |path: &str| host.bytes(path);
  assert_eq!(
    bytes("/target/src/edit.rs").as_deref(),
    Some(&b"the agent's body"[..])
  );
  assert_eq!(
    bytes("/target/src/made.rs").as_deref(),
    Some(&b"made by the agent"[..])
  );
  assert_eq!(
    bytes("/target/src/keep.rs").as_deref(),
    Some(&b"changed on the new runner"[..])
  );
  assert_eq!(bytes("/target/gone.rs"), None);
  let paths = host.paths();
  assert!(
    !paths.iter().any(|(p, _)| p.starts_with("/target/olddir")),
    "{paths:?}"
  );
  assert_eq!(
    bytes("/target/src/outsider.rs").as_deref(),
    Some(&b"an outsider's edit"[..])
  );
}

/// A-503 (stage 2). Do: move the agent's clone to another member before landing it (its capture carried, its head made
/// again in a volume of the new owner's and kept there as the mark), change one more file there, then present and land
/// the moved volume into a target an outsider edited since the capture. Expect: the plan judged by content holds the
/// agent's changes since the capture alone, before the move and after it (the rewritten and the later file replaced,
/// the made one created, the removed file and directory removed), every entry applied; the landing writes them and
/// passes the outsider's file by; and the moved volume stays a scratch volume. Before A-503 the new owner held no
/// capture and planned the moved volume as a scratch volume: every file a create, no removal.
#[test]
fn a_moved_captured_clone_lands_its_change_since_the_capture_and_passes_an_outsiders() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let (mut moved, base) = move_clone(&mut store, &origin, captured, &mut clone);
  rewrite(
    &mut moved,
    &mut store,
    "/src/keep.rs",
    b"changed on the new runner",
  );
  let (mut host, target) = disk(&[("/src/outsider.rs", b"an outsider's edit".as_slice())]);
  let (presented, result) = land_against(&mut host, &target, &mut store, (base, &mut moved));
  assert_planned_since_the_capture_across_the_move(&presented);
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  assert_the_moved_change_landed_and_the_outsiders_passed(&host);
  assert!(
    !moved.is_overlay(),
    "the moved volume took a base from the target"
  );
}

/// A-503, the oracle. Do: make the same later change in the clone on its first runner and in the moved volume, then
/// plan each: the clone against its capture (stage 1), the moved volume against what its move carried and its mark.
/// Expect: the same entries, each judged against the same captured identity and writing the same bytes; and the moved
/// volume planned as the scratch volume it was before A-503 differs (every file a create, nothing removed), so the
/// equality is no accident of a plan that judges nothing.
#[test]
fn a_moved_clone_plans_what_its_clone_plans_on_its_first_runner() {
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let (mut moved, base) = move_clone(&mut store, &origin, captured, &mut clone);
  for vol in [&mut clone, &mut moved] {
    rewrite(vol, &mut store, "/src/keep.rs", b"changed after the move");
    rewrite(vol, &mut store, "/src/edit.rs", b"old body");
  }
  let first = planned(
    &mut clone,
    &mut store,
    CaptureBase::Tree(origin.frozen_tree(captured).unwrap()),
  );
  let there = planned(&mut moved, &mut store, base);
  assert_eq!(judged(&there), judged(&first));
  assert!(
    !first
      .entries
      .iter()
      .any(|e| e.path.as_ref() == "/src/edit.rs"),
    "a file changed and changed back is no change: {:?}",
    judged(&first)
  );
  let mut host = SimHost::new();
  let mut scratch = crate::manifest::PlanWalk::new(&moved, Source::Head).unwrap();
  let scratch = loop {
    if let Some(manifest) = scratch
      .unit(&mut moved, &mut store, &mut host, &Filter::default())
      .unwrap()
    {
      break manifest;
    }
  };
  assert_ne!(
    judged(&scratch),
    judged(&there),
    "a scratch plan of the moved volume equals the carried one"
  );
}

/// A-501 (a base directory's mode alone, in a captured clone). Do: in a captured clone, `chmod` the directory `/src` to
/// 0700, nothing else changed beyond the clone's own edits, and land it against its capture. Expect the change since the
/// capture to hold the directory, the landing `Done`, and the target's directory at 0700 under its own inode, synced.
/// Before A-501 a captured clone's plan made nothing of a directory that both held.
#[test]
fn a_captured_clones_directory_mode_change_alone_lands() {
  /// Shape: the directory made private to its owner.
  const PRIVATE_DIR_MODE: u32 = 0o040_700;
  let mut store = store();
  let (origin, captured, mut clone) = captured_clone(&mut store);
  let src = clone.resolve(&store, "/src").unwrap().inode;
  clone.chmod(&mut store, src, PRIVATE_DIR_MODE).unwrap();
  let (mut host, target) = disk(&[]);
  let before = host.fingerprint("/target/src").unwrap();
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
      .any(|e| e.path.as_ref() == "/src"),
    "{:?}",
    presented.manifest.entries
  );
  let report = result.unwrap();
  assert_eq!(report.state, LandingState::Done, "{:?}", report.entries);
  let after = host.fingerprint("/target/src").unwrap();
  assert_eq!(after.mode & PERMISSIONS, PRIVATE_DIR_MODE & PERMISSIONS);
  assert_eq!(after.ino, before.ino);
  assert!(!host.holds_unsynced_metadata("/target/src"));
}
