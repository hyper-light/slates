//! The library and its consumer contract by use (§4.12 "Embedding", A-278; "The consumer contract", A-299): an
//! embedding host's whole flow over a daemon in this process — attach, the consensus group, volumes provisioned from
//! each source, snapshotted, cloned and destroyed, an overlay written, its drift read typed, a landing presented for
//! approval and approved exactly as shown, a conflict typed per path, a merge accepted and a merge conflicted, signed
//! work refused until the daemon verifies it, a Shards stream that serves the volume, and the daemon observed — each
//! outcome and each refusal asserted as its typed variant. Every test drives the crate's own verbs and judges only what
//! a host could observe: the daemon's answers and the bytes on the disk.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_methods
)]

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_api::{
  Approval, Awaiting, BaseState, ConflictClass, Denied, Drift, DriftKind, Error, Existing, Filter,
  Instance, Intent, Invalid, IssuerFault, LandOutcome, LandingConflict, LandingState, MergeOutcome,
  Missing, PathFault, PolicyReason, PolicyRefusal, PolicySubject, Provisioned, RebaseOutcome,
  SnapshotId, Unsupported, Unusable, Verification, VolumeId, VolumeSpec, Work, WorkSignature,
  WriteAct, WriteMode, WritePolicy,
};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds); measured once per process.
const PROBE_MS: u64 = 5;
/// Shape: one shard per test daemon: the library's verbs are the subject, not placement.
const TEST_SHARDS: u16 = 1;
/// Shape: how long a test waits for its daemon's rendezvous to answer after the start.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: a scratch volume's quota and an overlay's growth bound: room for the few small files a test writes.
const ROOM: u64 = 4 << 20;
/// Shape: the permission bits of a file a test writes.
const MODE: u32 = 0o644;

/// The machine profile this binary's daemons derive from, measured once per process (§4.1), with the process's one
/// key region reserved for the largest daemon a test starts before any starts
/// (`docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`).
fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  let profile = PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures");
  static RESERVED: OnceLock<bool> = OnceLock::new();
  let _ = RESERVED.get_or_init(|| {
    let largest = DaemonConfig::derive(&profile, "api-reserve", Some(TEST_SHARDS));
    slates_server::seal_keys::reserve(largest.seal_key_slots)
  });
  profile
}

/// A daemon of this process named after `tag` (its consensus group not made: [`Instance::ensure_group`] makes it), and
/// an instance attached to it, retried while the rendezvous comes up.
fn daemon(tag: &str) -> (Daemon, Instance) {
  static NEXT: AtomicU64 = AtomicU64::new(0);
  let profile = profile();
  let serial = NEXT.fetch_add(1, Ordering::Relaxed);
  let instance = format!("api-{tag}-{}-{serial}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-api-{tag}-{}-{serial}", std::process::id()),
    },
  )
  .unwrap();
  let started = Instant::now();
  let attached = loop {
    match Instance::attach(&instance) {
      Ok(attached) => break attached,
      Err(error) if error.is_unavailable() && started.elapsed() < START_WAIT => {
        std::thread::yield_now();
      }
      Err(error) => panic!("attaching {instance}: {error}"),
    }
  };
  (daemon, attached)
}

/// A daemon and an instance with its consensus group made, as a host makes it for an instance it runs alone.
fn grouped(tag: &str) -> (Daemon, Instance) {
  let (daemon, mut instance) = daemon(tag);
  instance.ensure_group().unwrap();
  (daemon, instance)
}

/// A real directory for an overlay's base and a landing's target (A-50: never `/tmp`, never a RAM directory): in the
/// build output, named with the process id and a counter, canonical, removed on drop.
struct Folder {
  path: String,
}

impl Folder {
  fn new() -> Folder {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let made = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
      "slates-api-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&made).unwrap();
    Folder {
      path: std::fs::canonicalize(&made)
        .unwrap()
        .to_string_lossy()
        .into_owned(),
    }
  }

  fn write(&self, name: &str, bytes: &[u8]) {
    let path = Path::new(&self.path).join(name);
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
  }

  fn read(&self, name: &str) -> Option<Vec<u8>> {
    std::fs::read(Path::new(&self.path).join(name)).ok()
  }
}

impl Drop for Folder {
  fn drop(&mut self) {
    let _removed = std::fs::remove_dir_all(&self.path);
  }
}

/// Writes `bytes` as the whole of `path` in `volume` through the typed file channel, under a write attachment held for
/// the one write.
fn write_file(instance: &mut Instance, volume: VolumeId, path: &str, bytes: &[u8]) {
  let attached = instance.attach_volume(volume, Intent::Write).unwrap();
  instance
    .client()
    .fs_write((volume, attached.attachment), path, bytes, MODE)
    .unwrap();
  instance.detach(attached.attachment).unwrap();
}

/// `path`'s bytes at `volume`'s head.
fn read_file(instance: &mut Instance, volume: VolumeId, path: &str) -> Vec<u8> {
  instance
    .read(volume, path, slates_api::ReadAt::Head)
    .unwrap()
}

/// The volume `spec` makes.
fn provision(instance: &mut Instance, spec: &VolumeSpec) -> VolumeId {
  instance.provision(spec).unwrap().volume()
}

/// The landing of `volume`'s head into `folder`, `.git` left out as kiln's landings leave it.
fn landing_into(volume: VolumeId, folder: &Folder) -> slates_api::LandingRequest {
  slates_api::LandingRequest {
    volume,
    snapshot: None,
    target: folder.path.clone(),
    filter: Filter {
      include: Vec::new(),
      exclude: vec!["/.git".to_owned()],
    },
  }
}

/// The launcher's authority over `daemon`'s instance: its anchor segment's handoff, as the anchor hands it to its
/// children.
fn issuer_of(daemon: &Daemon) -> slates_api::Issuer {
  slates_api::Issuer::from_handoff(&daemon.segment().handoff_env().unwrap()).unwrap()
}

/// A signature in `ssh-keygen -Y sign`'s armour, for the refusals that come before any verifier reads it.
fn a_signature() -> WorkSignature {
  WorkSignature::from_armoured(
    "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----",
  )
  .unwrap()
}

/// T-5.1's library form (A-278, §4.7; A-299): do attach to an instance no daemon serves; expect the typed
/// `Unavailable`, retryable once an instance runs, never a hang or a panic, naming the instance asked.
#[test]
fn attaching_where_no_daemon_serves_is_refused_unavailable() {
  let name = format!("api-nobody-{}", std::process::id());
  let refused = Instance::attach(&name).unwrap_err();
  assert!(
    refused.is_unavailable() && refused.is_retryable(),
    "{refused:?}"
  );
  assert!(
    matches!(&refused, Error::Unavailable { instance, .. } if *instance == name),
    "{refused:?}"
  );
}

/// AC-5.12 / T-5.14 (A-278, A-299; §4.8 `bootstrap root`): do make an instance's group twice; expect both to succeed (a
/// host's start path may run it again), and a volume provisioned only once the group exists, while before it the
/// provision is refused `Unusable(NoGroup)` — the refusal a host meets if it skips the step.
#[test]
fn a_group_is_made_once_and_made_again_harmlessly_before_any_volume() {
  let (daemon, mut instance) = daemon("group");
  assert_eq!(
    instance.provision(&VolumeSpec::empty("early", ROOM)),
    Err(Error::Unusable(Unusable::NoGroup))
  );
  instance.ensure_group().unwrap();
  instance.ensure_group().unwrap();
  provision(&mut instance, &VolumeSpec::empty("late", ROOM));
  daemon.stop();
}

/// AC-5.12 / T-5.14 (A-278, A-299; §4.4, D-5): do provision an empty volume, write a file, snapshot, provision a clone
/// of the snapshot, write the origin again, then destroy the clone; expect the clone to read the snapshot's bytes while
/// the origin reads its own, the names to be listed and then the clone's gone and a verb on it `NotFound` naming it, a
/// second volume under a held name refused `AlreadyExists` naming the holder, a scratch volume's status to have no base,
/// and an overlay over a relative folder refused `Invalid(Path)` before the daemon is asked.
#[test]
fn a_volume_is_provisioned_snapshotted_cloned_and_destroyed() {
  let (daemon, mut instance) = grouped("lifecycle");
  let origin = provision(&mut instance, &VolumeSpec::empty("origin", ROOM));
  assert_eq!(
    instance.provision(&VolumeSpec::empty("origin", ROOM)),
    Err(Error::AlreadyExists(Existing::Volume { existing: origin }))
  );
  assert_eq!(instance.status(origin).unwrap().base, None);
  write_file(&mut instance, origin, "/f", b"at the snapshot");
  let snapshot: SnapshotId = instance.snapshot(origin).unwrap();
  let clone = provision(
    &mut instance,
    &VolumeSpec::clone_of("clone", origin, snapshot),
  );
  write_file(&mut instance, origin, "/f", b"the origin's own");
  assert_eq!(read_file(&mut instance, clone, "/f"), b"at the snapshot");
  assert_eq!(read_file(&mut instance, origin, "/f"), b"the origin's own");
  let mut names: Vec<String> = instance
    .list()
    .unwrap()
    .into_iter()
    .map(|v| v.name)
    .collect();
  names.sort();
  assert_eq!(names, ["clone", "origin"]);
  instance.destroy(clone).unwrap();
  let started = Instant::now();
  loop {
    let names: Vec<String> = instance
      .list()
      .unwrap()
      .into_iter()
      .map(|v| v.name)
      .collect();
    if names == ["origin"] {
      break;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "the clone leaves the list: {names:?}"
    );
    std::thread::yield_now();
  }
  assert_eq!(
    instance.snapshot(clone),
    Err(Error::NotFound(Missing::Volume(clone)))
  );
  assert_eq!(
    instance.provision(&VolumeSpec::overlay("o", "relative/dir", ROOM)),
    Err(Error::Invalid(Invalid::Path {
      path: "relative/dir".to_owned(),
      fault: PathFault::NotAbsolute
    }))
  );
  daemon.stop();
}

/// AC-5.10 (A-278, A-299; §4.13 "Grants"): do make the launcher's authority from no handoff, and from a handoff with no
/// or a malformed length; expect it denied `NoAuthority` with `NoHandoff` and `Malformed` — a process with no anchor in
/// its environment holds no authority.
#[test]
fn the_launchers_authority_needs_the_anchors_handoff() {
  assert_eq!(
    slates_api::Issuer::from_handoff(&[]).unwrap_err(),
    Error::Denied(Denied::NoAuthority(IssuerFault::NoHandoff))
  );
  assert!(matches!(
    slates_api::Issuer::from_handoff(&[("SLATES_ANCHOR".to_owned(), "x".to_owned())]),
    Err(Error::Denied(Denied::NoAuthority(
      IssuerFault::Malformed { .. }
    )))
  ));
  assert!(matches!(
    slates_api::Issuer::from_handoff(&[
      ("SLATES_ANCHOR".to_owned(), "x".to_owned()),
      ("SLATES_ANCHOR_LEN".to_owned(), "many".to_owned())
    ]),
    Err(Error::Denied(Denied::NoAuthority(
      IssuerFault::Malformed { .. }
    )))
  ));
}

/// AC-5.10 / T-5.12 and AC-5.12 / T-5.14 (A-278, A-299; §4.15, D-25, D-26) on the landing a host drives (Unix: the
/// daemon lands on Unix). Do: lay an overlay over a folder holding a file and a `.git`, change the file, add one and
/// change `.git/HEAD` in the volume; land it with `/.git` left out under the ask mode every landing has today; approve
/// an approval of another manifest; approve the one shown. Expect: the folder untouched until the approval; the
/// landing answered `ApprovalRequired` with a typed summary (one create, one replace, `.git` filtered out), unsigned,
/// listing exactly the two files as they read; the stale approval writing nothing and answering the fresh
/// presentation; the shown one `Written`, `Done`, two entries, and the folder then holding exactly the volume's two
/// files with `.git/HEAD` as it was; a signed landing refused `Unsupported(SignedWork)` before anything is sent.
#[cfg(unix)]
#[test]
fn an_overlay_lands_exactly_what_was_approved_and_nothing_else() {
  let (daemon, mut instance) = grouped("landing");
  let folder = Folder::new();
  folder.write("a.txt", b"a\n");
  folder.write(".git/HEAD", b"ref: refs/heads/main\n");
  let volume = provision(
    &mut instance,
    &VolumeSpec::overlay("work", &folder.path, ROOM),
  );
  assert_eq!(read_file(&mut instance, volume, "/a.txt"), b"a\n");
  write_file(&mut instance, volume, "/a.txt", b"a2\n");
  write_file(&mut instance, volume, "/n.txt", b"n\n");
  write_file(
    &mut instance,
    volume,
    "/.git/HEAD",
    b"ref: refs/heads/agent\n",
  );
  assert_eq!(
    folder.read("a.txt").unwrap(),
    b"a\n",
    "the folder is as it was"
  );
  assert_eq!(folder.read("n.txt"), None);

  let request = landing_into(volume, &folder);
  assert_eq!(
    instance.land(&request, Some(&a_signature())),
    Err(Error::Unsupported(Unsupported::SignedWork))
  );
  let LandOutcome::ApprovalRequired(approval) = instance.land(&request, None).unwrap() else {
    panic!("a landing with changes asks under today's mode");
  };
  assert_eq!(approval.awaiting, Awaiting::Landing(request.clone()));
  assert_eq!(approval.verification, Verification::Unsigned);
  assert_eq!(
    (
      approval.summary.actions.create,
      approval.summary.actions.replace
    ),
    (1, 1),
    "{:?}",
    approval.summary
  );
  assert_eq!(approval.summary.actions.total(), 2);
  assert!(approval.summary.filtered_out >= 1, "{:?}", approval.summary);
  assert_eq!(approval.manifest_text().len(), 64);
  // A-287: the approval's entry list is what it writes, path by path, `/.git` left out, each file as it reads.
  let entries = instance.approval_entries(&approval).unwrap();
  let listed: Vec<_> = entries
    .iter()
    .map(|entry| {
      (
        entry.path.as_str(),
        entry.action.clone(),
        entry.content.map(|content| content.size),
      )
    })
    .collect();
  assert_eq!(
    listed,
    vec![
      ("/n.txt", slates_api::PlannedAction::Create, Some(2)),
      ("/a.txt", slates_api::PlannedAction::Replace, Some(3)),
    ]
  );
  for entry in &entries {
    let bytes = read_file(&mut instance, volume, &entry.path);
    assert_eq!(
      entry.content.map(|content| content.hash),
      Some(*blake3::hash(&bytes).as_bytes()),
      "{} is listed as it reads",
      entry.path
    );
  }

  let issuer = issuer_of(&daemon);
  let stale = Approval {
    manifest: [0u8; 32],
    ..approval.clone()
  };
  let changed = instance.approve(&stale, &issuer).unwrap();
  assert!(
    matches!(&changed, LandOutcome::ApprovalRequired(fresh) if fresh.manifest == approval.manifest),
    "an approval of another manifest is no approval of this one: {changed:?}"
  );
  assert_eq!(folder.read("a.txt").unwrap(), b"a\n", "nothing was written");

  let landed = instance.approve(&approval, &issuer).unwrap();
  let LandOutcome::Written(report) = landed else {
    panic!("the approved manifest lands: {landed:?}");
  };
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(report.written, 2, "{report:?}");
  assert_eq!(report.verification, Verification::Unsigned);
  assert_eq!(folder.read("a.txt").unwrap(), b"a2\n");
  assert_eq!(folder.read("n.txt").unwrap(), b"n\n");
  assert_eq!(
    folder.read(".git/HEAD").unwrap(),
    b"ref: refs/heads/main\n",
    "git's own files were left out"
  );
  assert!(
    !instance.audit(0).unwrap().is_empty(),
    "the grant and the landing are audited"
  );
  daemon.stop();
}

/// AC-5.12 / T-5.14 (A-278, A-299; §4.15 "Drift", step 4): do change a file in an overlay, then change the same file on
/// the disk; expect no drift before and, after, the drift typed — the file `Modified` at its whole path, the base read
/// with a watcher — and the landing answered `Conflicted` with the file's class `ModifyModify` and the bytes the volume
/// would land there, nothing written.
#[cfg(unix)]
#[test]
fn a_file_changed_on_disk_after_the_volume_changed_it_is_typed_drift_and_a_typed_conflict() {
  let (daemon, mut instance) = grouped("drift");
  let folder = Folder::new();
  folder.write("d.txt", b"base\n");
  let volume = provision(
    &mut instance,
    &VolumeSpec::overlay("drifting", &folder.path, ROOM),
  );
  write_file(&mut instance, volume, "/d.txt", b"the volume's\n");
  let before = instance.drift(volume).unwrap().unwrap();
  assert!(before.drifted.is_empty(), "{before:?}");
  folder.write("d.txt", b"the person's\n");
  let after = instance.drift(volume).unwrap().unwrap();
  assert_eq!(
    after.drifted,
    vec![Drift {
      path: "/d.txt".to_owned(),
      kind: DriftKind::Modified
    }]
  );
  assert!(after.watcher.is_some(), "{after:?}");
  assert!(matches!(
    instance.status(volume).unwrap().base,
    Some(BaseState::Read(_))
  ));
  let landed = instance.land(&landing_into(volume, &folder), None).unwrap();
  let volume_bytes = b"the volume's\n";
  assert_eq!(
    landed,
    LandOutcome::Conflicted(vec![LandingConflict {
      path: "/d.txt".to_owned(),
      class: ConflictClass::ModifyModify,
      planned: Some(slates_api::PlannedContent {
        hash: *blake3::hash(volume_bytes).as_bytes(),
        size: u64::try_from(volume_bytes.len()).unwrap(),
      }),
    }])
  );
  assert_eq!(folder.read("d.txt").unwrap(), b"the person's\n");
  daemon.stop();
}

/// AC-5.10 / AC-5.12 (A-299, A-285; Ada's rule of 2026-11-18): do set overlays' write policies with the launcher's
/// authority — one bypass, one forbid — and land each, plan the bypass one, and ask for a policy that requires signed
/// merges. Expect: an unset volume's policy reads back as none; the bypass volume read back as set and landed in the one
/// call, `Written` and `Done`, its file on the disk; its plan refused `Unsupported(WritePolicies)` before anything is
/// written, since no verb presents a bypass landing alone yet; the forbid volume's landing answered `Refused` by its own
/// policy and nothing written; and the signed-merge requirement refused `Unsupported(SignedWork)` before anything is
/// sent, since the daemon does not keep it yet.
#[cfg(unix)]
#[test]
fn a_landing_follows_its_volumes_write_mode() {
  let (daemon, mut instance) = grouped("modes");
  let launcher = issuer_of(&daemon);
  let bypassed = Folder::new();
  let forbidden = Folder::new();
  let open = provision(
    &mut instance,
    &VolumeSpec::overlay("open", &bypassed.path, ROOM),
  );
  let shut = provision(
    &mut instance,
    &VolumeSpec::overlay("shut", &forbidden.path, ROOM),
  );
  assert_eq!(
    instance.write_policy(PolicySubject::Volume(open)).unwrap(),
    None,
    "nobody set one: it asks"
  );
  let bypass = WritePolicy {
    merge: WriteMode::Ask,
    land: WriteMode::Bypass,
    require_signed_merges: false,
  };
  instance
    .set_write_policy(PolicySubject::Volume(open), &bypass, &launcher)
    .unwrap();
  assert_eq!(
    instance.write_policy(PolicySubject::Volume(open)).unwrap(),
    Some(bypass)
  );
  write_file(&mut instance, open, "/w.txt", b"written at once\n");
  let request = landing_into(open, &bypassed);
  assert_eq!(
    instance.plan_landing(&request),
    Err(Error::Unsupported(Unsupported::WritePolicies)),
    "a plan never writes"
  );
  assert_eq!(bypassed.read("w.txt"), None);
  let LandOutcome::Written(report) = instance.land(&request, None).unwrap() else {
    panic!("a bypass volume lands in the one call");
  };
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(bypassed.read("w.txt").unwrap(), b"written at once\n");

  let forbid = WritePolicy {
    merge: WriteMode::Forbid,
    land: WriteMode::Forbid,
    require_signed_merges: false,
  };
  instance
    .set_write_policy(PolicySubject::Volume(shut), &forbid, &launcher)
    .unwrap();
  write_file(&mut instance, shut, "/f.txt", b"never landed\n");
  assert_eq!(
    instance
      .land(&landing_into(shut, &forbidden), None)
      .unwrap(),
    LandOutcome::Refused(PolicyRefusal {
      act: WriteAct::Land,
      by: PolicySubject::Volume(shut),
      reason: PolicyReason::Forbidden,
    })
  );
  assert_eq!(forbidden.read("f.txt"), None, "nothing was written");
  assert_eq!(
    instance.set_write_policy(
      PolicySubject::Volume(shut),
      &WritePolicy {
        require_signed_merges: true,
        ..forbid
      },
      &launcher
    ),
    Err(Error::Unsupported(Unsupported::SignedWork))
  );
  daemon.stop();
}

/// AC-5.12 / T-5.14 (A-299; §4.16, D-27): do provision a green and work volumes over it; merge a seed, then two works
/// based on the same version that rewrite the same bytes, one after the other; rebase and merge a third that changed
/// another file. Expect the seed and the first `Merged` at versions 1 and 2, unsigned; the second `Conflicted` with a
/// window on the rewritten bytes and the green's head 2, nothing merged; the third rebased onto 2 and merged at 3; and a
/// signed merge refused `Unsupported(SignedWork)` before anything is sent.
#[test]
fn a_merge_is_accepted_or_conflicted_typed() {
  let (daemon, mut instance) = grouped("merge");
  let green = provision(
    &mut instance,
    &VolumeSpec {
      name: "green".to_owned(),
      source: slates_api::Source::Green {
        base: None,
        require_evidence: false,
      },
    },
  );
  let work = |instance: &mut Instance, name: &str| match instance
    .provision(&VolumeSpec::work(name, green))
    .unwrap()
  {
    Provisioned::Work { work, base } => (work, base),
    other => panic!("a work volume provisions as one: {other:?}"),
  };
  let (seed, base) = work(&mut instance, "seed");
  assert_eq!((seed.green, base), (green, 0));
  instance
    .client()
    .edit(seed.volume, "f", 0, 0, b"aaaaaaaa")
    .unwrap();
  assert_eq!(
    instance.merge(&seed, Some(&a_signature())),
    Err(Error::Unsupported(Unsupported::SignedWork))
  );
  assert_eq!(
    instance.merge(&seed, None).unwrap(),
    MergeOutcome::Merged {
      version: 1,
      verification: Verification::Unsigned
    }
  );
  let (first, _) = work(&mut instance, "first");
  let (second, _) = work(&mut instance, "second");
  let (third, _): (Work, u64) = work(&mut instance, "third");
  instance
    .client()
    .edit(first.volume, "f", 0, 4, b"XXXX")
    .unwrap();
  assert!(matches!(
    instance.merge(&first, None).unwrap(),
    MergeOutcome::Merged { version: 2, .. }
  ));
  instance
    .client()
    .edit(second.volume, "f", 0, 4, b"YYYY")
    .unwrap();
  let MergeOutcome::Conflicted(conflicts) = instance.merge(&second, None).unwrap() else {
    panic!("a rewrite of bytes the green changed since the base conflicts");
  };
  assert_eq!(conflicts.head, 2);
  assert!(
    conflicts
      .conflicts
      .iter()
      .any(|conflict| conflict.path == "f" && conflict.at < 4),
    "the conflict names the rewritten range: {conflicts:?}"
  );
  instance
    .client()
    .edit(third.volume, "g", 0, 0, b"other")
    .unwrap();
  assert_eq!(
    instance.rebase(&third).unwrap(),
    RebaseOutcome::Rebased { version: 2 }
  );
  assert!(matches!(
    instance.merge(&third, None).unwrap(),
    MergeOutcome::Merged { version: 3, .. }
  ));
  daemon.stop();
}

/// Shape: the volumes the listing test makes: three pages' worth and more of summaries at these names (a page holds
/// what one reply chunk holds, about seventy).
const LISTED: usize = 200;
/// Shape: the files the drift test changes on disk under its overlay: more drifted entries at these path lengths than a
/// status reply carries beside its other fields, and more than one paged read holds.
const DRIFTED: usize = 100;
/// Shape: the paths the changed-since test's work changes in its green: more than one reply's worth at these lengths.
const CHANGED: usize = 150;
/// Shape: the landings the audit test presents, each one audit record: more than one reply's worth of records.
const PRESENTED: usize = 120;

/// Walks `read` a page at a time from the start: every page's items, and the cursors the pages were asked from (the
/// first `None`). Each cursor must move past the last, so the walk ends.
fn walk<T, C: Copy + Ord + std::fmt::Debug>(
  mut read: impl FnMut(Option<C>) -> slates_api::Page<T, C>,
) -> (Vec<Vec<T>>, Vec<Option<C>>) {
  let mut pages = Vec::new();
  let mut asked = vec![None];
  loop {
    let page = read(*asked.last().unwrap());
    pages.push(page.items);
    match page.next {
      None => return (pages, asked),
      Some(next) => {
        assert!(
          asked.last().unwrap().is_none_or(|last| next > last),
          "a page moves its cursor forward: {next:?} after {asked:?}"
        );
        asked.push(Some(next));
      }
    }
  }
}

/// A-263 (§4.4 `list`, §4.3; the library's paged listing, A-299's owed list): do make 200 volumes, then walk the
/// listing a page at a time through the library and read it whole. Expect: more than one page; each page in volume-id
/// order past the volume it was asked after; every volume exactly once across the walk; and the whole listing the same
/// volumes, in name order. Before A-263 the listing was one reply, refused past about 80 volumes.
#[test]
fn a_listing_is_read_a_page_at_a_time_and_whole() {
  let (daemon, mut instance) = grouped("list-pages");
  let made: std::collections::BTreeSet<VolumeId> = (0..LISTED)
    .map(|n| {
      provision(
        &mut instance,
        &VolumeSpec::empty(&format!("listed-volume-{n:04}"), ROOM),
      )
    })
    .collect();
  let (pages, asked) = walk(|after| instance.list_page(after).unwrap());
  assert!(
    pages.len() > 1,
    "{} volumes take more than one page",
    LISTED
  );
  for (page, after) in pages.iter().zip(&asked) {
    assert!(
      page.windows(2).all(|pair| pair[0].id < pair[1].id)
        && after.is_none_or(|after| page.iter().all(|summary| summary.id > after)),
      "a page is in id order past its cursor"
    );
  }
  let walked: Vec<VolumeId> = pages.iter().flatten().map(|summary| summary.id).collect();
  assert_eq!(walked.len(), made.len(), "every volume once");
  assert_eq!(
    walked
      .iter()
      .copied()
      .collect::<std::collections::BTreeSet<_>>(),
    made
  );
  let whole = instance.list().unwrap();
  let names: Vec<&str> = whole.iter().map(|summary| summary.name.as_str()).collect();
  assert!(
    names.windows(2).all(|pair| pair[0] <= pair[1]),
    "the whole listing is in name order"
  );
  assert_eq!(
    whole
      .iter()
      .map(|summary| summary.id)
      .collect::<std::collections::BTreeSet<_>>(),
    made
  );
  daemon.stop();
}

/// A-264 (§4.16 "Attachments and versions"): do merge into a green one work that writes 150 files with long names,
/// then read what the green changed since its first version a page at a time and whole. Expect: more than one page;
/// every path exactly once across the walk; the whole answer the same paths, in path order; and a version past the
/// head refused `Stale`. Before A-264 the answer was one reply, refused at 800 changed files.
#[cfg(unix)]
#[test]
fn a_greens_changes_are_read_a_page_at_a_time_and_whole() {
  let (daemon, mut instance) = grouped("changed-pages");
  let green = provision(
    &mut instance,
    &VolumeSpec {
      name: "green".to_owned(),
      source: slates_api::Source::Green {
        base: None,
        require_evidence: false,
      },
    },
  );
  let Provisioned::Work { work, base } = instance
    .provision(&VolumeSpec::work("work", green))
    .unwrap()
  else {
    panic!("a work volume provisions as one");
  };
  let written: std::collections::BTreeSet<String> = (0..CHANGED)
    .map(|n| format!("a-path-long-enough-to-fill-a-page-sooner-{n:04}.txt"))
    .collect();
  for path in &written {
    instance
      .client()
      .edit(work.volume, path, 0, 0, b"x")
      .unwrap();
  }
  let MergeOutcome::Merged { version, .. } = instance.merge(&work, None).unwrap() else {
    panic!("a lone work merges");
  };
  let (pages, _) = walk(|from| instance.changed_since_page(green, base, from).unwrap());
  assert!(pages.len() > 1, "{} paths take more than one page", CHANGED);
  let walked: Vec<String> = pages.into_iter().flatten().collect();
  assert_eq!(walked.len(), written.len(), "every path once: {walked:?}");
  assert_eq!(
    walked
      .iter()
      .cloned()
      .collect::<std::collections::BTreeSet<_>>(),
    written
  );
  let whole = instance.changed_since(green, base).unwrap();
  assert_eq!(whole, written.into_iter().collect::<Vec<_>>());
  assert!(
    matches!(
      instance.changed_since(green, version + 1),
      Err(Error::Stale(_))
    ),
    "a version past the head is stale"
  );
  daemon.stop();
}

/// A-267 (§4.15 "Drift"): do write 100 files with long names through an overlay, change each on the disk, then read the
/// volume's status and its drift. Expect: the status carries what fits and counts the rest (`elided` above zero);
/// `drift` names all 100 `Modified` and elides none; and the paged read, walked from position 0, names each once.
/// Before A-267 a status with 600 drifted entries was refused, and the library read only what a status carried.
#[test]
fn an_overlays_drift_past_one_reply_is_read_whole() {
  let (daemon, mut instance) = grouped("drift-pages");
  let folder = Folder::new();
  let paths: Vec<String> = (0..DRIFTED)
    .map(|n| format!("a-file-whose-name-takes-room-in-every-drift-entry-{n:04}.txt"))
    .collect();
  for path in &paths {
    folder.write(path, b"base\n");
  }
  let volume = provision(
    &mut instance,
    &VolumeSpec::overlay("drifting", &folder.path, ROOM),
  );
  for path in &paths {
    write_file(
      &mut instance,
      volume,
      &format!("/{path}"),
      b"the volume's\n",
    );
    folder.write(path, b"the person's\n");
  }
  let Some(BaseState::Read(carried)) = instance.status(volume).unwrap().base else {
    panic!("the base reads");
  };
  assert!(
    carried.elided > 0 && carried.drifted.len() < DRIFTED,
    "a status carries what fits and counts the rest: {} carried, {} elided",
    carried.drifted.len(),
    carried.elided
  );
  let whole = instance.drift(volume).unwrap().unwrap();
  let expected: Vec<Drift> = paths
    .iter()
    .map(|path| Drift {
      path: format!("/{path}"),
      kind: DriftKind::Modified,
    })
    .collect();
  assert_eq!((whole.drifted, whole.elided), (expected.clone(), 0));
  let mut from = 0;
  let mut walked = Vec::new();
  loop {
    let page = instance.drift_page(volume, from).unwrap();
    walked.extend(page.drifted);
    match page.next {
      None => break,
      Some(next) => from = next,
    }
  }
  assert_eq!(
    walked, expected,
    "the pages name each entry once, in path order"
  );
  daemon.stop();
}

/// A-265 (§4.15, §4.14): do present 120 landings of an overlay, each one audit record, then read the audit a page at a
/// time and whole. Expect: more than one page; the records numbered in order, each once, every page's first record the
/// sequence it was asked from or later; and the whole audit the same records. Before A-265 every record was numbered 0
/// and the audit was one reply, refused between 50 and 100 records.
#[cfg(unix)]
#[test]
fn the_audit_is_read_a_page_at_a_time_and_whole() {
  let (daemon, mut instance) = grouped("audit-pages");
  let folder = Folder::new();
  let volume = provision(
    &mut instance,
    &VolumeSpec::overlay("audited", &folder.path, ROOM),
  );
  write_file(&mut instance, volume, "/n.txt", b"n\n");
  for _ in 0..PRESENTED {
    assert!(matches!(
      instance
        .plan_landing(&landing_into(volume, &folder))
        .unwrap(),
      LandOutcome::ApprovalRequired(_)
    ));
  }
  let mut pages = Vec::new();
  let mut since = 0;
  loop {
    let page = instance.audit_page(since).unwrap();
    assert!(page.items.first().is_none_or(|record| record.seq >= since));
    pages.push(page.items);
    match page.next {
      None => break,
      Some(next) => since = next,
    }
  }
  assert!(
    pages.len() > 1,
    "{} records take more than one page",
    PRESENTED
  );
  let walked: Vec<u64> = pages.iter().flatten().map(|record| record.seq).collect();
  assert!(walked.len() >= PRESENTED, "every presentation recorded");
  assert_eq!(
    walked,
    (0..u64::try_from(walked.len()).unwrap()).collect::<Vec<_>>(),
    "numbered in order, each once"
  );
  assert_eq!(
    instance.audit(0).unwrap(),
    pages.into_iter().flatten().collect::<Vec<_>>()
  );
  daemon.stop();
}

/// AC-5.12 / T-5.14 (A-278; §4.14): do read the daemon's status and a shard's telemetry through the library; expect the
/// daemon's shards reported and the drain answered for the shard it has.
#[test]
fn the_daemon_is_observed_through_the_library() {
  let (daemon, mut instance) = grouped("observe");
  let report = instance.daemon_status().unwrap();
  assert_eq!(report.shards.len(), usize::from(TEST_SHARDS));
  instance.telemetry(0).unwrap();
  assert!(instance.grants().unwrap().is_empty());
  daemon.stop();
}

/// The Shards route by use (§4.6, A-161): the stream end the library hands a VMM serves the volume.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shards {
  use std::io::{Read, Write};
  use std::os::unix::net::UnixStream;

  use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};

  use super::*;

  /// Format: the FUSE node id of the attachment's root.
  const ROOT: u64 = 1;
  /// Format: Linux `ENOENT`, the errno FUSE carries.
  const ENOENT: i32 = 2;
  /// Format: the FUSE protocol a guest kernel offers (7.31), and the largest read it asks.
  const MAJOR: u32 = 7;
  const MINOR: u32 = 31;
  const MAX_READAHEAD: u32 = 128 << 10;

  /// Shards' fs device as the end's holder speaks it: a little-endian length, then the FUSE request; one reply frame
  /// back.
  struct Device {
    stream: UnixStream,
    unique: u64,
  }

  impl Device {
    fn ask(&mut self, opcode: Opcode, nodeid: u64, body: &[u8]) -> Vec<u8> {
      self.unique += 1;
      let total = IN_HEADER_LEN + body.len();
      let mut request = vec![0u8; total];
      request[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
      request[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
      request[8..16].copy_from_slice(&self.unique.to_le_bytes());
      request[16..24].copy_from_slice(&nodeid.to_le_bytes());
      request[IN_HEADER_LEN..].copy_from_slice(body);
      self
        .stream
        .write_all(&u32::try_from(total).unwrap().to_le_bytes())
        .unwrap();
      self.stream.write_all(&request).unwrap();
      let mut word = [0u8; 4];
      self.stream.read_exact(&mut word).unwrap();
      let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(word)).unwrap()];
      self.stream.read_exact(&mut reply).unwrap();
      reply
    }
  }

  fn errno(reply: &[u8]) -> i32 {
    -i32::from_le_bytes(reply[4..8].try_into().unwrap())
  }

  /// AC-5.12 / T-5.14 (A-278; §4.6, the Shards route, A-161): do serve a volume holding a file over the library's Shards stream, then
  /// speak FUSE through the end as Shards' device does; expect INIT answered and the file found by LOOKUP, a name the
  /// volume lacks answered `ENOENT` — the end serves that volume and no host mount is involved.
  #[test]
  fn a_shards_stream_end_serves_the_volume_it_was_made_for() {
    let (daemon, mut instance) = grouped("shards");
    let volume = provision(&mut instance, &VolumeSpec::empty("guest", ROOM));
    write_file(&mut instance, volume, "/present", b"through the stream\n");
    let served = instance.serve_shards(volume, Intent::Write, None).unwrap();
    let stream = UnixStream::from(served.stream);
    stream.set_read_timeout(Some(START_WAIT)).unwrap();
    let mut device = Device { stream, unique: 0 };
    let mut init = Vec::new();
    for word in [MAJOR, MINOR, MAX_READAHEAD, 0] {
      init.extend_from_slice(&word.to_le_bytes());
    }
    assert_eq!(errno(&device.ask(Opcode::Init, 0, &init)), 0, "INIT");
    let found = device.ask(Opcode::Lookup, ROOT, b"present\0");
    assert_eq!(errno(&found), 0, "LOOKUP of the volume's file");
    assert!(found.len() > OUT_HEADER_LEN);
    let missing = device.ask(Opcode::Lookup, ROOT, b"absent\0");
    assert_eq!(errno(&missing), ENOENT, "a name the volume lacks is ENOENT");
    drop(device);
    daemon.stop();
  }
}
