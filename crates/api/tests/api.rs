//! The library and its consumer contract by use (§4.12 "Embedding", A-278; "The consumer contract", A-299): an
//! embedding host's whole flow over a daemon in this process — attach, the consensus group, volumes provisioned from
//! each source, snapshotted, cloned and destroyed, an overlay written, its drift read typed, a landing presented for
//! approval and approved exactly as shown, a conflict typed per path, a merge accepted and a merge conflicted, a signed
//! merge judged and its record read back (a signed landing refused until the daemon verifies a landing's signature), a
//! Shards stream that serves the volume, and the daemon observed — each
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

#[cfg(unix)]
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_api::{
  AttachTransport, Denied, Error, Exhausted, Existing, Instance, Intent, Invalid, IssuerFault,
  KindMismatch, MergeApproval, MergeOutcome, MergeRecord, Missing, MountOptions, PathFault,
  PolicyReason, PolicyRefusal, PolicySubject, Provisioned, RebaseOutcome, SimVolumes, SnapshotId,
  Source, TrustedSigner, Unusable, Verification, VolumeId, VolumeSpec, Volumes, Work,
  WorkSignature, WriteAct, WriteMode, WritePolicy,
};
// What only the Unix-gated tests (the overlay and landing tests) name.
#[cfg(unix)]
use slates_api::{
  AuditKind, BaseState, ConflictClass, Drift, DriftKind, Filter, GrantState, LandOutcome,
  LandingApproval, LandingConflict, LandingStage, LandingState, Unsupported,
};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// A software SSH signer (A-321), shared with the daemon's own signed-work tests; not every helper is used here.
#[path = "../../server/tests/common/signer.rs"]
#[allow(dead_code)]
mod signer;

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds); measured once per process.
const PROBE_MS: u64 = 5;
/// Shape: one shard per test daemon: the library's verbs are the subject, not placement.
const TEST_SHARDS: u16 = 1;
/// Shape: the shards of the daemon kiln starts (`crates/kiln/src/workspace/slates.rs`, `SHARDS`): two on every
/// machine, so each shard's reserve is a sixth of the machine's memory (three memory classes) while kiln's volume asks
/// a quarter.
const KILN_SHARDS: u16 = 2;
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
    let largest = DaemonConfig::derive(&profile, "api-reserve", Some(TEST_SHARDS.max(KILN_SHARDS)));
    slates_server::seal_keys::reserve(largest.seal_key_slots)
  });
  profile
}

/// A daemon of this process named after `tag` (its consensus group not made: [`Instance::ensure_group`] makes it), and
/// an instance attached to it, retried while the rendezvous comes up.
fn daemon(tag: &str) -> (Daemon, Instance) {
  daemon_on(tag, TEST_SHARDS)
}

/// [`daemon`] on `shards` shards.
fn daemon_on(tag: &str, shards: u16) -> (Daemon, Instance) {
  daemon_with(tag, shards, |_| {})
}

/// [`daemon_on`] with its derived configuration adjusted first (a capped slab, say).
fn daemon_with(
  tag: &str,
  shards: u16,
  adjust: impl FnOnce(&mut DaemonConfig),
) -> (Daemon, Instance) {
  static NEXT: AtomicU64 = AtomicU64::new(0);
  let profile = profile();
  let serial = NEXT.fetch_add(1, Ordering::Relaxed);
  let instance = format!("api-{tag}-{}-{serial}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(shards));
  adjust(&mut config);
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

/// [`grouped`] on the shards of the daemon kiln starts ([`KILN_SHARDS`]).
#[cfg(unix)]
fn kiln_grouped(tag: &str) -> (Daemon, Instance) {
  let (daemon, mut instance) = daemon_on(tag, KILN_SHARDS);
  instance.ensure_group().unwrap();
  (daemon, instance)
}

/// A real directory for an overlay's base and a landing's target (A-50: never `/tmp`, never a RAM directory): in the
/// build output, named with the process id and a counter, canonical, removed on drop.
#[cfg(unix)]
struct Folder {
  path: String,
}

#[cfg(unix)]
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

#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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

  let approval = expect_an_approval_asked(&mut instance, &landing_into(volume, &folder));
  expect_the_entries_listed_as_they_read(&mut instance, volume, &approval);

  let issuer = issuer_of(&daemon);
  let stale = LandingApproval {
    manifest: [0u8; 32],
    ..approval.clone()
  };
  let changed = instance.approve_landing(&stale, &issuer).unwrap();
  assert!(
    matches!(&changed, LandOutcome::ApprovalRequired(fresh) if fresh.manifest == approval.manifest),
    "an approval of another manifest is no approval of this one: {changed:?}"
  );
  assert_eq!(folder.read("a.txt").unwrap(), b"a\n", "nothing was written");

  expect_the_approved_manifest_written(&mut instance, &approval, &issuer, &folder);
  expect_the_landing_audited_and_its_grant_consumed(&mut instance, &approval);
  daemon.stop();
}

/// The ask of `an_overlay_lands_exactly_what_was_approved_and_nothing_else`. Do: land `request` signed, then unsigned.
/// Expect the signed landing refused `Unsupported(SignedWork)` before anything is sent, and the unsigned one answered
/// `ApprovalRequired`, unsigned, for `request`, with a typed summary of one create and one replace, `.git` filtered out.
/// Returns the approval.
#[cfg(unix)]
fn expect_an_approval_asked(
  instance: &mut Instance,
  request: &slates_api::LandingRequest,
) -> LandingApproval {
  assert_eq!(
    instance.land(request, Some(&a_signature())),
    Err(Error::Unsupported(Unsupported::SignedWork))
  );
  let LandOutcome::ApprovalRequired(approval) = instance.land(request, None).unwrap() else {
    panic!("a landing with changes asks under today's mode");
  };
  assert_eq!(approval.request, *request);
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
  approval
}

/// The entry list of `an_overlay_lands_exactly_what_was_approved_and_nothing_else` (A-287). Expect `approval`'s list to
/// be what it writes, path by path, `/.git` left out, each file as it reads in `volume`.
#[cfg(unix)]
fn expect_the_entries_listed_as_they_read(
  instance: &mut Instance,
  volume: VolumeId,
  approval: &LandingApproval,
) {
  let entries = instance.approval_entries(approval).unwrap();
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
    let bytes = read_file(instance, volume, &entry.path);
    assert_eq!(
      entry.content.map(|content| content.hash),
      Some(*blake3::hash(&bytes).as_bytes()),
      "{} is listed as it reads",
      entry.path
    );
  }
}

/// The approved landing of `an_overlay_lands_exactly_what_was_approved_and_nothing_else`. Do: approve `approval` with
/// `issuer`. Expect it `Written` and `Done`, unsigned, two entries written, and `folder` holding exactly the volume's
/// two files with `.git/HEAD` as it was.
#[cfg(unix)]
fn expect_the_approved_manifest_written(
  instance: &mut Instance,
  approval: &LandingApproval,
  issuer: &slates_api::Issuer,
  folder: &Folder,
) {
  let landed = instance.approve_landing(approval, issuer).unwrap();
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
}

/// The record of `an_overlay_lands_exactly_what_was_approved_and_nothing_else` (A-299). Expect the approval and the
/// landing audited and listed, typed: the presentation, the grant the launcher issued, the landing finished `Done`, and
/// the single-use grant of `approval` consumed by it.
#[cfg(unix)]
fn expect_the_landing_audited_and_its_grant_consumed(
  instance: &mut Instance,
  approval: &LandingApproval,
) {
  let audit = instance.audit(0).unwrap();
  let kinds: Vec<AuditKind> = audit.iter().map(|record| record.kind).collect();
  assert!(
    kinds.contains(&AuditKind::LandingPlanned) && kinds.contains(&AuditKind::GrantIssued),
    "{kinds:?}"
  );
  assert!(
    audit
      .iter()
      .any(|record| record.kind == AuditKind::LandingFinished
        && record.outcome == Some(LandingStage::Done)),
    "{audit:?}"
  );
  let grants = instance.grants().unwrap();
  assert!(
    grants
      .iter()
      .any(|grant| grant.manifest == approval.manifest && grant.state == GrantState::Consumed),
    "{grants:?}"
  );
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
/// call, `Written` and `Done`, its file on the disk; its plan presented and nothing written (the daemon's present-only
/// verb); the forbid volume's landing answered `Refused` by its own policy and nothing written; and the signed-merge
/// requirement refused `Unsupported(SignedWork)` before anything is sent, since the daemon does not keep it yet.
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
  assert!(
    matches!(
      instance.plan_landing(&request).unwrap(),
      LandOutcome::ApprovalRequired(_)
    ),
    "a plan presents, whatever the mode"
  );
  assert_eq!(bypassed.read("w.txt"), None, "a plan never writes");
  let LandOutcome::Written(report) = instance.land(&request, None).unwrap() else {
    panic!("a bypass volume lands in the one call");
  };
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(bypassed.read("w.txt").unwrap(), b"written at once\n");
  expect_a_forbidding_volume_never_lands(&mut instance, shut, &forbidden, &launcher);
  daemon.stop();
}

/// The forbidding half of `a_landing_follows_its_volumes_write_mode`. Do: forbid `shut`'s merges and landings with the
/// `launcher`'s authority, write a file into it and land it over `forbidden`; then set a policy that also requires
/// signed merges on it, and one on a consumer. Expect the landing answered `Refused` by the volume's own policy, nothing
/// written; the volume's requirement set and read back (A-321); and the consumer's refused
/// `Invalid::SignedMergesNotAVolume` before anything is sent, since the requirement is a volume's rule.
#[cfg(unix)]
fn expect_a_forbidding_volume_never_lands(
  instance: &mut Instance,
  shut: VolumeId,
  forbidden: &Folder,
  launcher: &slates_api::Issuer,
) {
  let forbid = WritePolicy {
    merge: WriteMode::Forbid,
    land: WriteMode::Forbid,
    require_signed_merges: false,
  };
  instance
    .set_write_policy(PolicySubject::Volume(shut), &forbid, launcher)
    .unwrap();
  write_file(instance, shut, "/f.txt", b"never landed\n");
  assert_eq!(
    instance.land(&landing_into(shut, forbidden), None).unwrap(),
    LandOutcome::Refused(PolicyRefusal {
      act: WriteAct::Land,
      by: PolicySubject::Volume(shut),
      reason: PolicyReason::Forbidden,
    })
  );
  assert_eq!(forbidden.read("f.txt"), None, "nothing was written");
  let requiring = WritePolicy {
    require_signed_merges: true,
    ..forbid
  };
  instance
    .set_write_policy(PolicySubject::Volume(shut), &requiring, launcher)
    .unwrap();
  assert_eq!(
    instance.write_policy(PolicySubject::Volume(shut)).unwrap(),
    Some(requiring)
  );
  assert_eq!(
    instance.set_write_policy(PolicySubject::Consumer(1), &requiring, launcher),
    Err(Error::Invalid(Invalid::SignedMergesNotAVolume))
  );
}

/// AC-5.12 / T-5.14 (A-299; §4.16, D-27): do provision a green and work volumes over it; merge a seed, then two works
/// based on the same version that rewrite the same bytes, one after the other; rebase and merge a third that changed
/// another file. Expect a green's and a work's status read with no base; the seed and the first `Merged` at versions 1
/// and 2, unsigned; the second `Conflicted` with a
/// window on the rewritten bytes and the green's head 2, nothing merged; and the third rebased onto 2 and merged at 3.
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
  // A green and a work volume have no base folder: their status reads, with no base (the merge service reports their
  // watcher as no overlay's, which a reading of the status text once took for a protocol fault).
  assert_eq!(instance.status(green).unwrap().base, None);
  assert_eq!(instance.drift(seed.volume).unwrap(), None);
  instance
    .client()
    .edit(seed.volume, "f", 0, 0, b"aaaaaaaa")
    .unwrap();
  assert_eq!(
    instance.merge(&seed, &[], None).unwrap(),
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
    instance.merge(&first, &[], None).unwrap(),
    MergeOutcome::Merged { version: 2, .. }
  ));
  expect_the_same_bytes_rewritten_conflict(&mut instance, &second);
  expect_a_rebased_work_merged(&mut instance, &third);
  daemon.stop();
}

/// The conflict of `a_merge_is_accepted_or_conflicted_typed`. Do: in `second`, based on the version before the first
/// work's merge, rewrite the bytes that merge rewrote, and merge it. Expect it `Conflicted` with a window on the
/// rewritten bytes and the green's head 2, nothing merged.
fn expect_the_same_bytes_rewritten_conflict(instance: &mut Instance, second: &Work) {
  instance
    .client()
    .edit(second.volume, "f", 0, 4, b"YYYY")
    .unwrap();
  let MergeOutcome::Conflicted(conflicts) = instance.merge(second, &[], None).unwrap() else {
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
}

/// The rebase of `a_merge_is_accepted_or_conflicted_typed`. Do: change another file in `third`, rebase it and merge it.
/// Expect it rebased onto version 2 and merged at 3.
fn expect_a_rebased_work_merged(instance: &mut Instance, third: &Work) {
  instance
    .client()
    .edit(third.volume, "g", 0, 0, b"other")
    .unwrap();
  assert_eq!(
    instance.rebase(third).unwrap(),
    RebaseOutcome::Rebased { version: 2 }
  );
  assert!(matches!(
    instance.merge(third, &[], None).unwrap(),
    MergeOutcome::Merged { version: 3, .. }
  ));
}

/// AC-5.12 (A-321; §4.16, §4.12 "The consumer contract"): do trust a key on a green and require signed merges there,
/// with the launcher's authority; merge a work unsigned; ask for its statement, sign it with the trusted key and merge
/// it; read the version's merge record back, and a version never committed. Expect the signers read back as set; the
/// unsigned merge `Refused` by the green's requirement, unsigned, nothing merged; the statement naming the work, a
/// person's text; the signed merge `Merged` at 1, verified naming the trusted key; its record verified, read back after
/// the checkpoint the merge let fold its chain entry (the work moved to 1, so nothing below it is live); the origin's
/// record unsigned; and the uncommitted version `NotFound` naming it.
#[test]
fn a_signed_merge_is_judged_and_read_back_through_the_contract() {
  let (daemon, mut instance) = grouped("signed-merge");
  let launcher = issuer_of(&daemon);
  let green = provision(
    &mut instance,
    &VolumeSpec {
      name: "signed-green".to_owned(),
      source: slates_api::Source::Green {
        base: None,
        require_evidence: false,
      },
    },
  );
  let key = signer::TestSigner::fresh();
  let trusted = TrustedSigner::from_wire(key.public()).unwrap();
  instance
    .set_signers(green, std::slice::from_ref(&trusted), &launcher)
    .unwrap();
  assert_eq!(instance.signers(green).unwrap(), vec![trusted.clone()]);
  let requiring = WritePolicy {
    merge: WriteMode::Bypass,
    land: WriteMode::Ask,
    require_signed_merges: true,
  };
  instance
    .set_write_policy(PolicySubject::Volume(green), &requiring, &launcher)
    .unwrap();
  let Provisioned::Work { work, .. } = instance
    .provision(&VolumeSpec::work("signed", green))
    .unwrap()
  else {
    panic!("a work volume provisions as one");
  };
  instance
    .client()
    .edit(work.volume, "f", 0, 0, b"signed work")
    .unwrap();
  assert_eq!(
    instance.merge(&work, &[], None).unwrap(),
    MergeOutcome::Refused(PolicyRefusal {
      act: WriteAct::Merge,
      by: PolicySubject::Volume(green),
      reason: PolicyReason::SignatureRequired {
        verification: Verification::Unsigned
      },
    })
  );
  assert_eq!(instance.client().versions(green).unwrap(), 0);
  let statement = instance.merge_statement(&work, &[]).unwrap();
  assert_eq!(statement.work, work);
  assert!(
    String::from_utf8(statement.statement.clone())
      .unwrap()
      .starts_with("slates merge v1\n")
  );
  let signature = WorkSignature::from_blob(key.sign(&statement.statement));
  let verified = Verification::Verified {
    signer: trusted.signer(),
  };
  assert_eq!(
    instance.merge(&work, &[], Some(&signature)).unwrap(),
    MergeOutcome::Merged {
      version: 1,
      verification: verified
    }
  );
  assert_eq!(
    instance.merge_record(green, 1).unwrap(),
    MergeRecord {
      version: 1,
      verification: verified,
    }
  );
  assert_eq!(
    instance.merge_record(green, 0).unwrap().verification,
    Verification::Unsigned,
    "the origin, which no merge made"
  );
  assert_eq!(
    instance.merge_record(green, 9),
    Err(Error::NotFound(Missing::Version { green, version: 9 }))
  );
  daemon.stop();
}

/// AC-5.12 (A-299, A-285; §4.16): do set a green's merge mode to ask with the launcher's authority, merge a work into
/// it, approve an increment the approver never saw, then approve the one shown; then forbid the green's merges and merge
/// another work. Expect: the merge held `ApprovalRequired` with the increment's identity and its one path, nothing
/// merged (the green's head stays 0); the stranger's approval merging nothing and asking again with the same identity;
/// the shown one `Merged` at version 1; and the forbidden merge answered `Refused` by the green's own policy.
#[test]
fn a_merge_follows_its_greens_write_mode() {
  let (daemon, mut instance) = grouped("merge-modes");
  let launcher = issuer_of(&daemon);
  let green = provision(
    &mut instance,
    &VolumeSpec {
      name: "held-green".to_owned(),
      source: slates_api::Source::Green {
        base: None,
        require_evidence: false,
      },
    },
  );
  let ask = WritePolicy {
    merge: WriteMode::Ask,
    land: WriteMode::Ask,
    require_signed_merges: false,
  };
  instance
    .set_write_policy(PolicySubject::Volume(green), &ask, &launcher)
    .unwrap();
  let Provisioned::Work { work, .. } = instance
    .provision(&VolumeSpec::work("held", green))
    .unwrap()
  else {
    panic!("a work volume provisions as one");
  };
  instance
    .client()
    .edit(work.volume, "f", 0, 0, b"held")
    .unwrap();
  let MergeOutcome::ApprovalRequired(approval) = instance.merge(&work, &[], None).unwrap() else {
    panic!("an ask green holds the merge");
  };
  assert_eq!((approval.work, approval.paths), (work, 1), "{approval:?}");
  assert_eq!(
    instance.client().versions(green).unwrap(),
    0,
    "nothing merged"
  );
  let stranger = MergeApproval {
    identity: [0u8; 32],
    ..approval.clone()
  };
  assert!(
    matches!(
      instance.approve_merge(&stranger, &launcher).unwrap(),
      MergeOutcome::ApprovalRequired(again) if again.identity == approval.identity
    ),
    "an approval of another increment merges nothing and asks again"
  );
  assert_eq!(instance.client().versions(green).unwrap(), 0);
  assert!(matches!(
    instance.approve_merge(&approval, &launcher).unwrap(),
    MergeOutcome::Merged { version: 1, .. }
  ));
  let forbid = WritePolicy {
    merge: WriteMode::Forbid,
    ..ask
  };
  instance
    .set_write_policy(PolicySubject::Volume(green), &forbid, &launcher)
    .unwrap();
  let Provisioned::Work { work: late, .. } = instance
    .provision(&VolumeSpec::work("late", green))
    .unwrap()
  else {
    panic!("a work volume provisions as one");
  };
  instance
    .client()
    .edit(late.volume, "g", 0, 0, b"no")
    .unwrap();
  assert_eq!(
    instance.merge(&late, &[], None).unwrap(),
    MergeOutcome::Refused(PolicyRefusal {
      act: WriteAct::Merge,
      by: PolicySubject::Volume(green),
      reason: PolicyReason::Forbidden,
    })
  );
  daemon.stop();
}

/// kiln's volume bound (`crates/kiln/src/workspace/slates.rs`, `volume_max`): a quarter of the machine's memory in
/// whole GiB, at least one and at most 64.
#[cfg(unix)]
fn kiln_volume_max() -> u64 {
  /// Format: kiln's `MAX_VOLUME_GIB`.
  const MAX_VOLUME_GIB: u64 = 64;
  ((profile().facts.memory.total / 4) >> 30).clamp(1, MAX_VOLUME_GIB) << 30
}

/// The daemon's own count of each shard's inode-version slab (§4.2 inode dimension), printed beside a test's verdict
/// as its evidence: the slots, those committed to volumes' allowances, and those snapshots retain.
#[cfg(unix)]
fn print_version_slabs(instance: &mut Instance, when: &str) {
  for shard in instance.daemon_status().unwrap().shards {
    eprintln!(
      "{when}: shard {} version_slots {} committed_versions {} retained_versions {} volumes {}",
      shard.partition,
      shard.version_slots,
      shard.committed_versions,
      shard.retained_versions,
      shard.volumes
    );
  }
}

/// P1 (2026-11-18, kiln's review over a microVM's volume, which failed `agents.change` with "reading .gitignore: a
/// bound is reached: Quota"; A-342, `docs/bugs/2026-11-18-a-dynamic-volume-reserved-the-whole-version-slab.md`). Do:
/// what kiln does, on the daemon kiln starts ([`KILN_SHARDS`]) with kiln's own volume spec (a quarter of the machine's
/// memory):
/// the agent's three edits and its lookup of the project's `.gitignore` (git reads it); then review: snapshot, present
/// that snapshot's landing without writing and list its entries, read `.gitignore` at the head as kiln's ignore rules
/// do, and a base file nothing looked up before the snapshot. Expect: each file's bytes as the disk holds them — a read
/// of a file nobody changed is never refused for space.
#[cfg(unix)]
#[test]
fn an_unchanged_base_file_reads_at_the_head_while_a_review_holds_its_snapshot() {
  let (daemon, mut instance) = kiln_grouped("review-read");
  let folder = Folder::new();
  let ignores = b"target/\n*.log\n";
  let readme = b"the project\n";
  folder.write(".gitignore", ignores);
  folder.write("README.md", readme);
  folder.write("src/lib.rs", b"pub fn a() {}\n");
  folder.write(".git/HEAD", b"ref: refs/heads/main\n");
  let volume = provision(
    &mut instance,
    &VolumeSpec::overlay("kiln-review", &folder.path, kiln_volume_max()),
  );
  write_file(
    &mut instance,
    volume,
    "/src/lib.rs",
    b"pub fn a() { b() }\n",
  );
  write_file(&mut instance, volume, "/src/new.rs", b"pub fn b() {}\n");
  write_file(&mut instance, volume, "/notes.txt", b"done\n");
  assert_eq!(read_file(&mut instance, volume, "/.gitignore"), ignores);
  let snapshot = instance.snapshot(volume).unwrap();
  let request = slates_api::LandingRequest {
    snapshot: Some(snapshot),
    ..landing_into(volume, &folder)
  };
  let LandOutcome::ApprovalRequired(approval) = instance.plan_landing(&request).unwrap() else {
    panic!("the agent's edits are presented");
  };
  assert_eq!(instance.approval_entries(&approval).unwrap().len(), 3);
  print_version_slabs(&mut instance, "review");
  assert_eq!(
    instance.read(volume, "/.gitignore", slates_api::ReadAt::Head),
    Ok(ignores.to_vec()),
    "an unchanged base file looked up before the snapshot reads at the head"
  );
  assert_eq!(
    instance.read(volume, "/README.md", slates_api::ReadAt::Head),
    Ok(readme.to_vec()),
    "an unchanged base file first looked up after the snapshot reads at the head"
  );
  instance.destroy_snapshot(volume, snapshot).unwrap();
  daemon.stop();
}

/// P1's sibling (2026-11-18, the same bound, A-342): do provision one more of kiln's workspaces (kiln's own volume
/// spec, a quarter of the machine's memory each) than the daemon kiln starts has shards, through one instance, as kiln
/// does for its agents' sessions. Expect every one made: a workspace that may grow to a quarter of the machine holds
/// none of its shard's room before it grows (§4.2: dynamic growth consumes only unpromised capacity).
#[cfg(unix)]
#[test]
fn kilns_workspaces_are_provisioned_side_by_side() {
  let (daemon, mut instance) = kiln_grouped("side-by-side");
  let folders: Vec<Folder> = (0..=KILN_SHARDS)
    .map(|_| {
      let folder = Folder::new();
      folder.write("README.md", b"the project\n");
      folder
    })
    .collect();
  let mut made = Vec::new();
  for (serial, folder) in folders.iter().enumerate() {
    let spec = VolumeSpec::overlay(&format!("kiln-{serial}"), &folder.path, kiln_volume_max());
    let provisioned = instance.provision(&spec);
    print_version_slabs(&mut instance, &format!("after workspace {serial}"));
    made.push(provisioned.map(|made| made.volume()));
  }
  for (serial, volume) in made.iter().enumerate() {
    assert!(volume.is_ok(), "workspace {serial}: {volume:?}");
  }
  for volume in made.into_iter().flatten() {
    instance.destroy(volume).unwrap();
  }
  daemon.stop();
}

/// Shape: a capped daemon's version slab: room for a handful of files, so a test can spend every unpromised slot.
#[cfg(unix)]
const CAPPED_SLAB: usize = 64;
/// Shape: the capped daemon's arena, sized with its slab so a slot is about 1 MiB of quota (A-109).
#[cfg(unix)]
const CAPPED_RESERVE: u64 = 64 << 20;

/// The retention sibling of P1 (2026-11-18; A-342's record): a snapshot's retention refused reached kiln as
/// `Exhausted(Quota)` when the bound was the shard's version slab, never the volume's quota. Do: on a daemon whose
/// slab is capped small, grow an overlay (dynamic, twice the arena at most) with empty files until the daemon refuses
/// the growth, snapshot it, and change a file the snapshot holds. Expect: the growth refused `NoSpace`, the volume's
/// own bound, and the change refused `Volume { RetentionExceeded }` on the wire, typed apart from it (the contract
/// classes it `Exhausted::Volume`, `error.rs`'s unit test), the file as it was.
#[cfg(unix)]
#[test]
fn a_snapshots_retention_refused_is_not_the_quota() {
  let (daemon, mut instance) = daemon_with("retention", TEST_SHARDS, |config| {
    config.store.max_inodes = CAPPED_SLAB;
    config.reserve_per_shard = CAPPED_RESERVE;
  });
  instance.ensure_group().unwrap();
  let folder = Folder::new();
  let volume = provision(
    &mut instance,
    &VolumeSpec::overlay("retention", &folder.path, 2 * CAPPED_RESERVE),
  );
  let attached = instance.attach_volume(volume, Intent::Write).unwrap();
  let at = (volume, attached.attachment);
  let grown = (0..=CAPPED_SLAB).find_map(|serial| {
    instance
      .client()
      .fs_write(at, &format!("/f{serial}"), b"", MODE)
      .err()
  });
  assert!(
    matches!(
      grown,
      Some(slates_client::ClientError::Refused(
        slates_ipc::protocol::Refusal::NoSpace
      ))
    ),
    "the dynamic volume's growth past the slab is its own bound: {grown:?}"
  );
  let snapshot = instance.snapshot(volume).unwrap();
  let changed = instance.client().fs_write(at, "/f0", b"changed", MODE);
  assert!(
    matches!(
      changed,
      Err(slates_client::ClientError::Refused(
        slates_ipc::protocol::Refusal::Volume {
          refusal: slates_api::VolumeRefusal::RetentionExceeded
        }
      ))
    ),
    "a snapshot's retention refused is typed apart from the quota: {changed:?}"
  );
  assert_eq!(read_file(&mut instance, volume, "/f0"), b"");
  instance.detach(attached.attachment).unwrap();
  instance.destroy_snapshot(volume, snapshot).unwrap();
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

/// The async form by use (§4.12 "Embedding", R6; A-299): an instance a host awaits on its own loop.
#[cfg(unix)]
mod asynchronous_form {
  use std::cell::Cell;
  use std::os::fd::BorrowedFd;
  use std::pin::pin;
  use std::task::{Context, Poll, Waker};

  use slates_api::{AsyncInstance, Completion, Reactor, Waiting, Woke};

  use super::*;

  /// Shape: the most polls [`block_on`] gives a future whose waits all answer at once: one is enough, a few more
  /// guard against a call that loops without waiting.
  const POLLS: usize = 4;

  /// Shape: how many times the rounds the cancellation case wants dropped it may run: a round whose call is answered on
  /// its first poll (the daemon replied before the call reached its wait) drops nothing and runs again.
  const ROUNDS_PER_DROP: usize = 4;

  /// A host's reactor in miniature: its wait blocks this thread in `poll(2)` until the completion signal is readable
  /// or the timeout passes, then answers at once, so an awaited call ends in the one poll [`block_on`] gives it. A wait
  /// with no timeout is bounded by the suite's wait and fails past it, so a call that would wait for good fails the
  /// test instead of hanging it. It counts how its waits ended, the evidence a test prints: a wait that timed out is
  /// the driver's deadline, not a reply's signal.
  #[derive(Default)]
  struct Blocking {
    /// Waits the completion signal ended.
    readable: Cell<u64>,
    /// Waits the timeout ended.
    timed_out: Cell<u64>,
  }

  impl Reactor for Blocking {
    fn wait(&self, completion: Completion, timeout_ns: Option<u64>) -> Waiting<'_> {
      // SAFETY: `completion` is the waiting instance's completion descriptor, which its client keeps open for as long
      // as the instance lives, and the call that waits here borrows the instance.
      let descriptor = unsafe { BorrowedFd::borrow_raw(completion) };
      let bound = timeout_ns.map_or(START_WAIT, Duration::from_nanos);
      let timeout = rustix::event::Timespec::try_from(bound).ok();
      let mut interest = [rustix::event::PollFd::new(
        &descriptor,
        rustix::event::PollFlags::IN,
      )];
      let woke = match rustix::event::poll(&mut interest, timeout.as_ref()) {
        Ok(0) if timeout_ns.is_none() => Err(std::io::Error::new(
          std::io::ErrorKind::TimedOut,
          "nothing landed within the suite's wait",
        )),
        Ok(0) => Ok(Woke::TimedOut),
        Ok(_) => Ok(Woke::Readable),
        Err(errno) => Err(std::io::Error::from(errno)),
      };
      let counted = match woke {
        Ok(Woke::Readable) => &self.readable,
        Ok(Woke::TimedOut) => &self.timed_out,
        Err(_) => return Box::pin(std::future::ready(woke)),
      };
      counted.set(counted.get().saturating_add(1));
      Box::pin(std::future::ready(woke))
    }
  }

  /// A reactor whose wait never answers: a host's loop while its call waits, for the cancellation case.
  struct Never;

  impl Reactor for Never {
    fn wait(&self, _completion: Completion, _timeout_ns: Option<u64>) -> Waiting<'_> {
      Box::pin(std::future::pending())
    }
  }

  /// Polls `future` to its end with a waker that wakes nothing: its waits answer at once, so it ends within
  /// [`POLLS`].
  fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..POLLS {
      if let Poll::Ready(out) = future.as_mut().poll(&mut context) {
        return out;
      }
    }
    panic!("an awaited call did not end within {POLLS} polls");
  }

  /// A-299, R6: do drive the async form over a daemon: provision an empty volume, snapshot it, read its status,
  /// provision a clone of the snapshot, ask the status of a volume no one made; then drop a status call while its wait
  /// has not answered, and ask again, until more calls have been dropped than twice the calls the client admits
  /// outstanding (a call the daemon answered before it reached its wait is answered on its first poll, drops nothing,
  /// and the round runs again); destroy the clone, and give the instance back to the synchronous form. Expect each
  /// answer as the synchronous verb answers it, the stranger refused `NotFound` naming the volume, every call after a
  /// dropped one answered (a dropped call is cancelled and its reply dropped when it comes; calls left in the driver
  /// would fill it and have one refused `TooManyOutstanding`), every drop made within the rounds' bound, and the
  /// synchronous form still serving afterwards. The reactor's wakes and the slowest answered round are printed: a
  /// round's wait that timed out was ended by the driver's deadline, not by its reply's signal.
  #[test]
  fn an_async_instance_provisions_snapshots_reports_and_destroys() {
    let (daemon, mut instance) = grouped("async");
    let abandoned = instance
      .client()
      .outstanding_limit()
      .saturating_mul(2)
      .saturating_add(1);
    let reactor = Blocking::default();
    let mut instance = AsyncInstance::new(instance).unwrap();
    let volume = block_on(instance.provision(&reactor, &VolumeSpec::empty("async", ROOM)))
      .unwrap()
      .volume();
    let snapshot = block_on(instance.snapshot(&reactor, volume)).unwrap();
    let status = block_on(instance.status(&reactor, volume)).unwrap();
    assert_eq!((status.name.as_str(), status.snapshots), ("async", 1));
    let copy = block_on(instance.provision(
      &reactor,
      &VolumeSpec::clone_of("async-copy", volume, snapshot),
    ))
    .unwrap()
    .volume();
    assert_eq!(
      block_on(instance.status(&reactor, NOWHERE)),
      Err(Error::NotFound(Missing::Volume(NOWHERE)))
    );
    let (dropped, slowest) = drop_and_ask_again(&mut instance, &reactor, volume, abandoned);
    eprintln!(
      "dropped {dropped} of {abandoned} calls; the slowest answered round took {slowest:?}; the reactor's waits ended \
       {} by the signal, {} by the driver's deadline",
      reactor.readable.get(),
      reactor.timed_out.get()
    );
    assert_eq!(
      dropped, abandoned,
      "every drop made within the rounds' bound"
    );
    assert_eq!(
      block_on(instance.status(&reactor, copy)).map(|status| status.name),
      Ok("async-copy".to_owned()),
      "a dropped call leaves the instance serving"
    );
    block_on(instance.destroy(&reactor, copy)).unwrap();
    let mut instance = instance.into_instance();
    assert_eq!(
      instance.status(volume).map(|status| status.snapshots),
      Ok(1)
    );
    daemon.stop();
  }

  /// The cancellation case of `an_async_instance_provisions_snapshots_reports_and_destroys`: rounds of a status call of
  /// `volume` polled once on a loop that never answers and dropped, then one asked on `reactor`, until `wanted` calls
  /// were dropped or [`ROUNDS_PER_DROP`] times that many rounds ran. A call the daemon answered on its first poll drops
  /// nothing and must have been answered. Each asked call must be answered. The calls dropped, and the slowest round's
  /// asked call.
  fn drop_and_ask_again(
    instance: &mut AsyncInstance,
    reactor: &Blocking,
    volume: VolumeId,
    wanted: usize,
  ) -> (usize, Duration) {
    let mut dropped = 0;
    let mut slowest = Duration::ZERO;
    for round in 0..wanted.saturating_mul(ROUNDS_PER_DROP) {
      if dropped == wanted {
        break;
      }
      {
        let waiting = instance.status(&Never, volume);
        let mut waiting = pin!(waiting);
        let mut context = Context::from_waker(Waker::noop());
        match waiting.as_mut().poll(&mut context) {
          Poll::Pending => dropped += 1,
          Poll::Ready(answered) => assert!(
            answered.is_ok(),
            "round {round}: a call answered on its first poll: {answered:?}"
          ),
        }
      }
      let asked = Instant::now();
      let answered = block_on(instance.status(reactor, volume));
      slowest = slowest.max(asked.elapsed());
      assert!(
        answered.is_ok(),
        "round {round}: a dropped call is cancelled, so the next is answered: {answered:?}"
      );
    }
    (dropped, slowest)
  }
}

/// A volume id no instance issued (A-310's conformance cases).
const NOWHERE: VolumeId = VolumeId { bytes: [0xAB; 16] };
/// Shape: the simulator's bound in the conformance suite: room for every volume and attachment the suite makes.
const SIM_MOST: usize = 16;

/// The volume `seam` provisions from `spec`.
fn made<V: Volumes>(seam: &mut V, spec: &VolumeSpec) -> VolumeId {
  seam.provision(spec).unwrap().volume()
}

/// The names `seam` lists, sorted.
fn names_of<V: Volumes>(seam: &mut V) -> Vec<String> {
  let mut names: Vec<String> = seam.list().unwrap().into_iter().map(|v| v.name).collect();
  names.sort();
  names
}

/// A stream end made ready for [`reads_its_end`] while its attachment still lives: on Unix a socket whose read waits at
/// most the suite's wait. The wait is set before the detach, since macOS refuses `SO_RCVTIMEO` (`EINVAL`) on a socket
/// whose peer has gone.
#[cfg(unix)]
fn awaiting(end: slates_api::StreamEnd) -> std::os::unix::net::UnixStream {
  let socket = std::os::unix::net::UnixStream::from(end);
  socket.set_read_timeout(Some(START_WAIT)).unwrap();
  socket
}

/// Whether the peer of a stream end reads its end within the suite's wait: the attachment it reached has ended.
#[cfg(unix)]
fn reads_its_end(socket: &std::os::unix::net::UnixStream) -> bool {
  let mut byte = [0u8; 1];
  matches!(std::io::Read::read(&mut &*socket, &mut byte), Ok(0))
}

/// A stream end as it is where it is a pipe handle (Windows): [`reads_its_end`] does not judge it.
#[cfg(not(unix))]
fn awaiting(end: slates_api::StreamEnd) -> slates_api::StreamEnd {
  end
}

/// Whether the peer of a stream end reads its end: not judged where the end is a pipe handle (Windows).
#[cfg(not(unix))]
fn reads_its_end(_end: &slates_api::StreamEnd) -> bool {
  true
}

/// A-310's conformance suite: one flow of the seam's verbs, each outcome and each refusal asserted as its typed value,
/// run over an instance and over the simulator. Do: provision an empty volume and again by its name; snapshot it and
/// clone the snapshot; list; snapshot, clone from, destroy and serve a volume no one made; detach an attachment no one
/// made; provision an overlay of a relative folder; make a green, a work over it, a work over a volume that is no green
/// and one over no volume; serve a volume to a Shards VMM and detach it; destroy a snapshot a clone pins, then an
/// unpinned one; destroy the clone and wait for it to leave the list; snapshot it. Expect: the volume made once and
/// refused `AlreadyExists` naming it; both volumes listed; the strangers refused `NotFound` naming the volume, the
/// snapshot or the attachment; the relative folder refused `NotAbsolute`; the work at the green's first version, the
/// others refused `NotGreen` and `NotFound`; a stream of the Shards transport whose peer reads its end once detached;
/// the pinned snapshot's destroy refused `Busy` by the volume core's `Pinned` (a holder to end first, typed since
/// A-299) and the unpinned one's done; the clone gone from the list, and refused `NotFound` after.
fn the_seam_conforms<V: Volumes>(seam: &mut V) {
  let origin = made(seam, &VolumeSpec::empty("seam-origin", ROOM));
  assert_eq!(
    seam.provision(&VolumeSpec::empty("seam-origin", ROOM)),
    Err(Error::AlreadyExists(Existing::Volume { existing: origin }))
  );
  let snapshot = seam.snapshot(origin).unwrap();
  let clone = made(seam, &VolumeSpec::clone_of("seam-clone", origin, snapshot));
  assert_eq!(names_of(seam), ["seam-clone", "seam-origin"]);
  expect_strangers_refused_typed(seam, snapshot);
  expect_a_work_over_a_green_alone(seam, origin);
  expect_a_served_streams_peer_reads_its_end_at_the_detach(seam, origin);
  assert!(
    matches!(
      seam.destroy_snapshot(origin, snapshot),
      Err(Error::Busy(slates_api::Busy::Volume(
        slates_api::VolumeRefusal::Pinned
      )))
    ),
    "a snapshot a clone pins is not destroyed"
  );
  let unpinned = seam.snapshot(origin).unwrap();
  assert_eq!(seam.destroy_snapshot(origin, unpinned), Ok(()));
  seam.destroy(clone).unwrap();
  let started = Instant::now();
  while names_of(seam).contains(&"seam-clone".to_owned()) {
    assert!(started.elapsed() < START_WAIT, "the clone leaves the list");
    std::thread::yield_now();
  }
  assert_eq!(
    seam.snapshot(clone),
    Err(Error::NotFound(Missing::Volume(clone)))
  );
}

/// The strangers of `the_seam_conforms`. Do: snapshot, clone from (at `snapshot`), destroy and serve a volume no one
/// made; detach an attachment no one made; provision an overlay of a relative folder. Expect each refused typed:
/// `NotFound` naming the volume, the snapshot or the attachment, and the folder `NotAbsolute`.
fn expect_strangers_refused_typed<V: Volumes>(seam: &mut V, snapshot: SnapshotId) {
  assert_eq!(
    seam.snapshot(NOWHERE),
    Err(Error::NotFound(Missing::Volume(NOWHERE)))
  );
  assert_eq!(
    seam.provision(&VolumeSpec::clone_of("seam-stranger", NOWHERE, snapshot)),
    Err(Error::NotFound(Missing::Snapshot {
      volume: NOWHERE,
      snapshot
    }))
  );
  assert_eq!(
    seam.destroy(NOWHERE),
    Err(Error::NotFound(Missing::Volume(NOWHERE)))
  );
  assert_eq!(
    seam
      .serve_shards(NOWHERE, Intent::Read, None)
      .map(|served| served.attachment.attachment),
    Err(Error::NotFound(Missing::Volume(NOWHERE)))
  );
  assert_eq!(
    seam.detach(u64::MAX),
    Err(Error::NotFound(Missing::Attachment {
      attachment: u64::MAX
    }))
  );
  assert_eq!(
    seam.provision(&VolumeSpec::overlay("seam-overlay", "relative/dir", ROOM)),
    Err(Error::Invalid(Invalid::Path {
      path: "relative/dir".to_owned(),
      fault: PathFault::NotAbsolute
    }))
  );
}

/// The works of `the_seam_conforms`. Do: make a green, a work over it, a work over `origin` (no green) and one over no
/// volume. Expect the work at the green's first version, and the others refused `NotGreen` and `NotFound`.
fn expect_a_work_over_a_green_alone<V: Volumes>(seam: &mut V, origin: VolumeId) {
  let green = made(
    seam,
    &VolumeSpec {
      name: "seam-green".to_owned(),
      source: Source::Green {
        base: None,
        require_evidence: false,
      },
    },
  );
  match seam.provision(&VolumeSpec::work("seam-work", green)) {
    Ok(Provisioned::Work { work, base }) => assert_eq!((work.green, base), (green, 0)),
    other => panic!("a work over a green: {other:?}"),
  }
  assert_eq!(
    seam.provision(&VolumeSpec::work("seam-not-over-a-green", origin)),
    Err(Error::Invalid(Invalid::Kind(KindMismatch::NotGreen)))
  );
  assert_eq!(
    seam.provision(&VolumeSpec::work("seam-over-nothing", NOWHERE)),
    Err(Error::NotFound(Missing::Volume(NOWHERE)))
  );
}

/// The Shards stream of `the_seam_conforms`. Do: serve `origin` to a Shards VMM, then detach it. Expect a stream of the
/// Shards transport whose peer reads its end once detached.
fn expect_a_served_streams_peer_reads_its_end_at_the_detach<V: Volumes>(
  seam: &mut V,
  origin: VolumeId,
) {
  let served = seam.serve_shards(origin, Intent::Write, None).unwrap();
  assert_eq!(
    served.attachment.capability.transport,
    AttachTransport::ShardsFs
  );
  let peer = awaiting(served.stream);
  seam.detach(served.attachment.attachment).unwrap();
  assert!(
    reads_its_end(&peer),
    "a detached stream's peer reads its end"
  );
}

/// A-310: the conformance suite over a real instance, in this process.
#[test]
fn an_instance_answers_the_seams_conformance_suite() {
  let (daemon, mut instance) = grouped("seam");
  the_seam_conforms(&mut instance);
  daemon.stop();
}

/// A-310: the conformance suite over the simulator, with no daemon anywhere.
#[test]
fn the_simulator_answers_the_seams_conformance_suite() {
  the_seam_conforms(&mut SimVolumes::new(SIM_MOST));
}

/// A-310: the simulator's own bound and what a mount is to it. Do: give it room for one volume and one attachment;
/// provision a second volume; mount the first, then mount it again, then a volume no one made; detach the mount. Expect
/// the second volume refused `Exhausted::Table`, the first mount answered with the path as given, the second refused
/// at the bound, the stranger `NotFound`, and the detach done.
#[test]
fn the_simulator_holds_what_it_was_given_room_for() {
  let mut sim = SimVolumes::new(1);
  let only = made(&mut sim, &VolumeSpec::empty("only", ROOM));
  assert!(matches!(
    sim.provision(&VolumeSpec::empty("second", ROOM)),
    Err(Error::Exhausted(Exhausted::Table { .. }))
  ));
  let options = MountOptions {
    read_only: false,
    subtree: None,
    shared: false,
  };
  let mounted = sim.mount(only, "/simulated/mount", &options).unwrap();
  assert_eq!(mounted.path, "/simulated/mount");
  assert!(matches!(
    sim.mount(only, "/simulated/another", &options),
    Err(Error::Exhausted(Exhausted::Table { .. }))
  ));
  assert_eq!(
    sim.mount(NOWHERE, "/simulated/stranger", &options),
    Err(Error::NotFound(Missing::Volume(NOWHERE)))
  );
  assert_eq!(sim.detach(mounted.attachment), Ok(()));
}
