//! A landing's presentation as a client meets it (§4.15 steps 1–3, AUD-29-07, AUD-29-02, A-49): bounded per shard,
//! refused before it allocates anything past the bound, replaced by its client's re-presentation, and of a named
//! snapshot exactly as the snapshot froze. These ran inside the shard while a presentation was answered in the verb;
//! since 2026-11-08 it is planned in slices and its reply deferred (`presentation_fairness.rs`), so a client drives
//! them.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]
// A real landing target on the host's disk: macOS and Linux.
#![cfg(unix)]

use std::collections::BTreeMap;

use slates_client::{
  Client, ClientError, ConflictClass, CreateSpec, EntriesCursor, Filter, Landing, LandingSummary,
  PlannedAction, PlannedContent, PlannedEntry, Refusal, SnapshotId, VolumeId,
};
use slates_ipc::protocol::{AuditKind, GrantScope, Intent, LandingStage};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::landing::GRANT_TERM_NS;
use common::lease::{hold, lease_key_of, release};
use common::target::target_dir;

/// Shape: one shard, so every presentation meets the same shard's bound.
const TEST_SHARDS: u16 = 1;

fn daemon(tag: &str, awaiting_bound: Option<usize>) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-presented-{tag}-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  if let Some(bound) = awaiting_bound {
    config.landings_awaiting_per_shard = bound;
  }
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("presented-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// A presentation of `volume` (its `snapshot`, or the head) into `target` by `client`.
fn present(
  client: &mut Client,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  target: &str,
) -> Result<Landing, ClientError> {
  client.land(volume, snapshot, target, Filter::default(), None)
}

/// The landings the daemon's shards hold awaiting a grant.
fn awaiting(client: &mut Client) -> u64 {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .map(|shard| shard.landings_awaiting)
    .sum()
}

/// The `landing_planned` records the audit lists.
fn planned_records(client: &mut Client) -> usize {
  client
    .audit(0)
    .unwrap()
    .iter()
    .filter(|record| record.kind == "landing_planned")
    .count()
}

/// §4.15 step 3 (AUD-29-07): the landings a shard holds awaiting a grant are bounded, and a presentation past the
/// bound is refused before it allocates anything, while a client re-presenting the same volume and target replaces its
/// own and needs no room. Do: with the bound at one, present volume A, then B, then A again (into an empty target:
/// nothing is granted, so nothing is written). Expect: B is refused `LandingsAwaitingFull` and changes nothing — one
/// presentation awaiting, one `landing_planned` record, and A's re-presentation takes the very next id, so none was
/// spent on B; A's second presentation replaces its first (still one), and the first's id is no longer grantable.
/// Before 2026-09-29 nothing bounded the presentations.
#[test]
fn a_presentation_past_the_bound_is_refused_and_changes_nothing() {
  let (daemon, instance) = daemon("bound", Some(1));
  let target = target_dir();
  let mut client = connect(&instance);
  let a = client.create(&scratch("presented-a")).unwrap();
  let b = client.create(&scratch("presented-b")).unwrap();
  let Ok(Landing::GrantRequired {
    landing: first,
    manifest: first_manifest,
    ..
  }) = present(&mut client, a, None, &target.path)
  else {
    panic!("A was not presented");
  };
  assert_eq!(awaiting(&mut client), 1);
  let planned = planned_records(&mut client);
  let refusal = present(&mut client, b, None, &target.path);
  assert!(
    matches!(
      refusal,
      Err(ClientError::Refused(Refusal::LandingsAwaitingFull))
    ),
    "{refusal:?}"
  );
  assert_eq!(awaiting(&mut client), 1, "the refusal presented nothing");
  assert_eq!(
    planned_records(&mut client),
    planned,
    "nor recorded anything"
  );
  let Ok(Landing::GrantRequired { landing: again, .. }) =
    present(&mut client, a, None, &target.path)
  else {
    panic!("A was not presented again");
  };
  assert_eq!(again, first + 1, "no id was spent on the refusal");
  assert_eq!(
    awaiting(&mut client),
    1,
    "the re-presentation replaced the first"
  );
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::grant_proof(
    &secret,
    first,
    &first_manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let replaced = client.grant(
    first,
    first_manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
    proof,
  );
  assert!(
    matches!(replaced, Err(ClientError::Refused(Refusal::NotFound))),
    "the replaced presentation is no longer grantable: {replaced:?}"
  );
  drop(client);
  daemon.stop();
}

/// §4.15 (AUD-29-02, A-49): a landing of a named snapshot presents that snapshot's state, whatever the head has become
/// since, and a snapshot the volume does not hold is `NotFound` before any host access. Do: snapshot a volume holding
/// "v1" and present the snapshot's landing (into an empty target: nothing is granted, so nothing is written); write
/// "v2" at the head and present the snapshot again, then the head; present a snapshot the volume never had. Expect: the
/// snapshot's second presentation carries its first's manifest and the head's another; the unknown one is `NotFound`.
/// Before 2026-09-30 the changed head was refused `Unsupported`; before 2026-09-29 the head was landed under the
/// snapshot's name.
#[test]
fn a_landing_of_a_named_snapshot_presents_that_snapshot_whatever_the_head_became() {
  let (daemon, instance) = daemon("snapshot", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = client.create(&scratch("landed")).unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "f", b"v1", 0o644)
    .unwrap();
  let snapshot = client.snapshot(volume).unwrap();
  let manifest_of = |presented: Result<Landing, ClientError>| match presented {
    Ok(Landing::GrantRequired { manifest, .. }) => manifest,
    other => panic!("not presented: {other:?}"),
  };
  let before = manifest_of(present(&mut client, volume, Some(snapshot), &target.path));
  client
    .fs_write((volume, attachment), "f", b"v2", 0o644)
    .unwrap();
  assert_eq!(
    manifest_of(present(&mut client, volume, Some(snapshot), &target.path)),
    before,
    "the snapshot presents what it froze"
  );
  assert_ne!(
    manifest_of(present(&mut client, volume, None, &target.path)),
    before,
    "the head presents what it became"
  );
  let never = SnapshotId {
    value: snapshot.value.wrapping_add(1 << u32::BITS),
  };
  let unknown = present(&mut client, volume, Some(never), &target.path);
  assert!(
    matches!(unknown, Err(ClientError::Refused(Refusal::NotFound))),
    "a snapshot the volume does not hold: {unknown:?}"
  );
  drop(client);
  daemon.stop();
}

/// The name a presentation's summary counts `action` under (§4.15 step 2), so a list can be held to its summary.
fn action_name(action: &PlannedAction) -> &'static str {
  match action {
    PlannedAction::Create => "create",
    PlannedAction::Replace => "replace",
    PlannedAction::Delete => "delete",
    PlannedAction::Rename { .. } => "rename",
    PlannedAction::Mkdir => "mkdir",
    PlannedAction::Rmdir => "rmdir",
    PlannedAction::Symlink { .. } => "symlink",
    PlannedAction::Clear => "clear",
  }
}

/// A list's entries counted by action, and the bytes its files carry: what its summary must state.
fn counted(entries: &[PlannedEntry]) -> (BTreeMap<String, u64>, u64) {
  let mut by_action = BTreeMap::new();
  for entry in entries {
    *by_action
      .entry(action_name(&entry.action).to_owned())
      .or_insert(0) += 1;
  }
  let bytes = entries
    .iter()
    .filter_map(|entry| entry.content.map(|content| content.size))
    .sum();
  (by_action, bytes)
}

/// A summary's counts by action, and its bytes.
fn stated(summary: &LandingSummary) -> (BTreeMap<String, u64>, u64) {
  let by_action = summary
    .by_action
    .iter()
    .map(|count| (count.action.clone(), count.count))
    .collect();
  (by_action, summary.bytes)
}

/// The bytes a planned file must hold: their BLAKE3 and their length.
fn content(bytes: &[u8]) -> Option<PlannedContent> {
  Some(PlannedContent {
    hash: *blake3::hash(bytes).as_bytes(),
    size: u64::try_from(bytes.len()).unwrap(),
  })
}

/// §4.15 step 2 ("the full entry list on demand"), AC-2.8. Do: over a base holding `edit.txt`, `gone.txt`, `keep.txt`
/// and `olddir/inner.txt`, a volume creates `new.txt`, makes `made` with `made/f.txt` in it, replaces `edit.txt`,
/// removes `gone.txt` and renames `olddir` to `newdir`; an outsider then changes `edit.txt` on disk; the snapshot's
/// landing is presented (nothing is granted, so nothing is written) and its entry list read by its presenter, by a
/// consumer the presenter enrolled, against another manifest, and again after the presenter presents the snapshot
/// anew. Expect: the list is the manifest's six entries in landing order (directories made, files created, replaced,
/// the directory renamed, the file removed), each create and replace naming its bytes' BLAKE3 and length, the
/// replacement, the removal and the rename what the volume witnessed, the conflict at `edit.txt` alone, and its counts
/// and bytes the summary's; `keep.txt` and the renamed directory's contents are no entry. The consumer's read and the
/// replaced presentation's are `NotFound`, the other manifest's `UnexpectedReply`, and the new presentation lists the
/// same entries under the same manifest.
#[test]
fn a_presentations_entry_list_names_each_entry_its_grant_would_write() {
  let (daemon, instance) = daemon("entries", None);
  let target = target_dir();
  for (name, bytes) in [
    ("edit.txt", &b"base edit"[..]),
    ("gone.txt", b"base gone"),
    ("keep.txt", b"base keep"),
  ] {
    target.seed(name, bytes);
  }
  // The fixture's own directory in the build output, as its files are.
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir(format!("{}/olddir", target.path)).unwrap();
  target.seed("olddir/inner.txt", b"base inner");
  let mut client = connect(&instance);
  let volume = client
    .create(&CreateSpec {
      base: Some(target.path.clone()),
      ..scratch("entries")
    })
    .unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  let at = (volume, attachment);
  client.fs_write(at, "new.txt", b"fresh", 0o644).unwrap();
  client.fs_write(at, "edit.txt", b"edited", 0o644).unwrap();
  client.fs_remove(at, "gone.txt").unwrap();
  client.fs_mkdir(at, "made", 0o755).unwrap();
  client
    .fs_write(at, "made/f.txt", b"in made", 0o644)
    .unwrap();
  client.fs_rename(at, "olddir", "newdir").unwrap();
  // The outsider's change, past the base's timestamp granularity so the fingerprint moves.
  #[allow(clippy::disallowed_methods)] // a test may sleep (the lint's stated exception)
  std::thread::sleep(std::time::Duration::from_millis(1100));
  target.seed("edit.txt", b"outsider edit");
  let snapshot = client.snapshot(volume).unwrap();
  let Ok(Landing::GrantRequired {
    landing,
    manifest,
    summary,
    conflicts,
    ..
  }) = present(&mut client, volume, Some(snapshot), &target.path)
  else {
    panic!("the snapshot's landing was not presented");
  };
  assert_eq!(conflicts, vec!["/edit.txt (ModifyModify)".to_owned()]);
  let entries = client.landing_entries(landing, manifest).unwrap();
  expect_the_six_entries_in_landing_order(&entries);
  assert_eq!(
    counted(&entries),
    stated(&summary),
    "the list and the summary a person reads agree"
  );
  let workload =
    expect_only_its_presenter_reads_the_list(&daemon, &instance, &mut client, (landing, manifest));
  let Ok(Landing::GrantRequired {
    landing: again,
    manifest: again_manifest,
    ..
  }) = present(&mut client, volume, Some(snapshot), &target.path)
  else {
    panic!("the snapshot's landing was not presented again");
  };
  assert_eq!(again_manifest, manifest, "the snapshot plans what it froze");
  let replaced = client.landing_entries(landing, manifest);
  assert!(
    matches!(replaced, Err(ClientError::Refused(Refusal::NotFound))),
    "the replaced presentation's list went with it: {replaced:?}"
  );
  assert_eq!(client.landing_entries(again, manifest).unwrap(), entries);
  drop(workload);
  drop(client);
  daemon.stop();
}

/// The list of `a_presentations_entry_list_names_each_entry_its_grant_would_write`. Expect `entries` to be the
/// manifest's six in landing order (directories made, files created, replaced, the directory renamed, the file removed),
/// each create and replace naming its bytes' BLAKE3 and length, the replacement, the removal and the rename what the
/// volume witnessed, the conflict at `edit.txt` alone; a file and a directory landing with their modes and a removal
/// with none.
fn expect_the_six_entries_in_landing_order(entries: &[PlannedEntry]) {
  let seen: Vec<_> = entries
    .iter()
    .map(|entry| {
      (
        entry.path.as_str(),
        entry.action.clone(),
        entry.content,
        entry.witnessed,
        entry.conflict,
      )
    })
    .collect();
  assert_eq!(
    seen,
    vec![
      ("/made", PlannedAction::Mkdir, None, false, None),
      (
        "/made/f.txt",
        PlannedAction::Create,
        content(b"in made"),
        false,
        None
      ),
      (
        "/new.txt",
        PlannedAction::Create,
        content(b"fresh"),
        false,
        None
      ),
      (
        "/edit.txt",
        PlannedAction::Replace,
        content(b"edited"),
        true,
        Some(ConflictClass::ModifyModify)
      ),
      (
        "/newdir",
        PlannedAction::Rename {
          from: "/olddir".to_owned()
        },
        None,
        true,
        None
      ),
      ("/gone.txt", PlannedAction::Delete, None, true, None),
    ]
  );
  let permissions = |path: &str| {
    entries
      .iter()
      .find(|entry| entry.path == path)
      .and_then(|entry| entry.mode)
      .map(|mode| mode & 0o777)
  };
  assert_eq!(
    permissions("/new.txt"),
    Some(0o644),
    "a file lands with its mode"
  );
  assert_eq!(permissions("/made"), Some(0o755), "so does a directory");
  assert_eq!(permissions("/gone.txt"), None, "a removal lands none");
}

/// The readers of `a_presentations_entry_list_names_each_entry_its_grant_would_write`. Do: through `client`, the
/// presenter, enroll a consumer with `daemon`'s issuer secret and read the list of `landing` under `manifest` as that
/// consumer; then read it through `client` against another manifest. Expect the consumer's read `NotFound` and the
/// other manifest's `UnexpectedReply`. Returns the consumer's client, held as long as the test runs.
fn expect_only_its_presenter_reads_the_list(
  daemon: &Daemon,
  instance: &str,
  client: &mut Client,
  (landing, manifest): (u64, [u8; 32]),
) -> Client {
  // A consumer of another principal: the list is not its to read.
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = rustix::process::getuid().as_raw();
  let (consumer, capability) = client
    .enroll(
      account,
      slates_server::landing::enroll_proof(&secret, account),
    )
    .unwrap();
  let mut workload = connect(instance);
  workload.attest(consumer, &capability).unwrap();
  let other = workload.landing_entries(landing, manifest);
  assert!(
    matches!(other, Err(ClientError::Refused(Refusal::NotFound))),
    "another principal's read: {other:?}"
  );
  let mismatched = client.landing_entries(landing, [0; 32]);
  assert!(
    matches!(mismatched, Err(ClientError::UnexpectedReply { .. })),
    "pages of another manifest are never joined: {mismatched:?}"
  );
  workload
}

/// Shape: the directories the long list's deep file lies under, and the bytes of each one's name, so that the file's
/// path (3,986 bytes, under the OS path limit a request carries) takes, with its entry's fields, more than one reply
/// chunk's 4,023 bytes of room: an entry no single page holds.
const DEEP_LEVELS: usize = 15;
/// Shape: see [`DEEP_LEVELS`].
const DEEP_NAME_BYTES: usize = 250;
/// Shape: the files of the long list, and the bytes of each one's name (about 257 bytes an entry, so they fill
/// several pages).
const MANY_FILES: usize = 40;
/// Shape: see [`MANY_FILES`].
const MANY_NAME_BYTES: usize = 200;

/// §4.15 step 2. Do: a scratch volume holds 40 files of 200-byte names, and a file 3,986 bytes deep under 15
/// directories, so the file's entry alone outgrows a page; present its landing (nothing is granted, so nothing is
/// written) and read its list a page at a time, then whole. Expect: each page's cursor moves on, at least one page
/// ends inside an entry and the deep file's entry comes whole across pages (it is never refused), and the list is
/// every entry once in landing order (the directories made first, by path, then the files by path), each file naming
/// its bytes' BLAKE3.
#[test]
fn a_long_entry_list_comes_in_pages_and_an_entry_past_one_page_comes_whole() {
  let (daemon, instance) = daemon("entry-pages", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = client.create(&scratch("entry-pages")).unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  let at = (volume, attachment);
  let mut dirs = Vec::new();
  let mut deep = String::new();
  for level in 0..DEEP_LEVELS {
    if !deep.is_empty() {
      deep.push('/');
    }
    deep.push_str(&format!("{level:02}{}", "d".repeat(DEEP_NAME_BYTES - 2)));
    client.fs_mkdir(at, &deep, 0o755).unwrap();
    dirs.push(format!("/{deep}"));
  }
  let mut files: Vec<(String, Vec<u8>)> = (0..MANY_FILES)
    .map(|index| {
      let name = format!("f{index:03}{}", "x".repeat(MANY_NAME_BYTES - 4));
      (format!("/{name}"), name.into_bytes())
    })
    .collect();
  files.push((
    format!("/{deep}/{}", "z".repeat(220)),
    b"the deep file".to_vec(),
  ));
  for (path, bytes) in &files {
    client
      .fs_write(at, path.trim_start_matches('/'), bytes, 0o644)
      .unwrap();
  }
  let deep_file = files.last().unwrap().0.clone();
  assert_eq!(deep_file.len(), 3986, "the deep file's path");
  let Ok(Landing::GrantRequired {
    landing,
    manifest,
    summary,
    ..
  }) = present(&mut client, volume, None, &target.path)
  else {
    panic!("the landing was not presented");
  };
  let mut from = EntriesCursor::default();
  let mut sizes = Vec::new();
  let mut cut = 0usize;
  loop {
    let page = client.landing_entries_page(landing, from).unwrap();
    sizes.push(page.bytes.len());
    let Some(next) = page.next else {
      break;
    };
    assert!(next > from, "the cursor moves on: {from:?} then {next:?}");
    cut += usize::from(next.byte > 0);
    from = next;
  }
  // The record A-287 cites: how many pages, of how many bytes each.
  eprintln!(
    "the list came in {} pages of {sizes:?} bytes, {cut} of them ending inside an entry",
    sizes.len()
  );
  assert!(sizes.len() >= 3, "the list came in {} pages", sizes.len());
  assert!(cut >= 1, "no page ended inside an entry");
  let entries = client.landing_entries(landing, manifest).unwrap();
  files.sort();
  let mut expected: Vec<(String, PlannedAction, Option<PlannedContent>)> = dirs
    .into_iter()
    .map(|dir| (dir, PlannedAction::Mkdir, None))
    .collect();
  expected.extend(
    files
      .iter()
      .map(|(path, bytes)| (path.clone(), PlannedAction::Create, content(bytes))),
  );
  let seen: Vec<_> = entries
    .iter()
    .map(|entry| (entry.path.clone(), entry.action.clone(), entry.content))
    .collect();
  assert_eq!(seen, expected);
  assert_eq!(counted(&entries), stated(&summary));
  drop(client);
  daemon.stop();
}

/// §4.15 steps 2–3, AC-2.8 (a grant binds what will be written). Do: present a scratch volume's two files into an
/// empty target, read the list, grant the manifest and land it; then read the list again. Expect: each landed file's
/// bytes hash to what the list named for it, and the landed presentation's list is `NotFound`: the landing consumed it.
#[test]
fn a_landed_presentations_list_named_what_it_wrote_and_goes_with_it() {
  let (daemon, instance) = daemon("entries-landed", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = client.create(&scratch("entries-landed")).unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  for (name, bytes) in [("a.txt", &b"first"[..]), ("b.txt", b"second")] {
    client
      .fs_write((volume, attachment), name, bytes, 0o644)
      .unwrap();
  }
  let snapshot = client.snapshot(volume).unwrap();
  let Ok(Landing::GrantRequired {
    landing, manifest, ..
  }) = present(&mut client, volume, Some(snapshot), &target.path)
  else {
    panic!("the landing was not presented");
  };
  let entries = client.landing_entries(landing, manifest).unwrap();
  assert_eq!(entries.len(), 2, "{entries:?}");
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::grant_proof(
    &secret,
    landing,
    &manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let grant = client
    .grant(landing, manifest, GrantScope::Once, GRANT_TERM_NS, proof)
    .unwrap();
  let landed = client
    .land(
      volume,
      Some(snapshot),
      &target.path,
      Filter::default(),
      Some(grant),
    )
    .unwrap();
  assert!(matches!(landed, Landing::Landed(_)), "{landed:?}");
  for entry in &entries {
    // The fixture's own directory in the build output.
    #[allow(clippy::disallowed_methods)]
    let written = std::fs::read(format!("{}{}", target.path, entry.path)).unwrap();
    assert_eq!(content(&written), entry.content, "{} landed", entry.path);
  }
  let consumed = client.landing_entries(landing, manifest);
  assert!(
    matches!(consumed, Err(ClientError::Refused(Refusal::NotFound))),
    "the landed presentation's list went with it: {consumed:?}"
  );
  drop(client);
  daemon.stop();
}

/// §4.15 "Audit" (AUD-29-06): a landing's trail records each step once. Do: present a scratch volume's file, grant the
/// manifest, land it under the grant and read the audit. Expect one record of the presentation's plan (no grant), one of
/// the grant, one of the granted run's plan and one of its finish. Before 2026-11-18 the finish mirrored every record
/// the engine's ring held for the landing's id, so the presentation's plan, which the presentation records itself, was
/// recorded twice (measured: plan, grant, plan, plan under the grant, validation, entry, finish;
/// `docs/bugs/2026-11-18-a-presented-landings-plan-was-audited-twice.md`). One shard: `Audit` reads the partition of
/// the shard that serves it (GAPS).
#[test]
fn a_presented_then_granted_landing_records_each_step_once() {
  let (daemon, instance) = daemon("audit-once", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = client.create(&scratch("audit-once")).unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "once.txt", b"once", 0o644)
    .unwrap();
  let snapshot = client.snapshot(volume).unwrap();
  let Ok(Landing::GrantRequired {
    landing, manifest, ..
  }) = present(&mut client, volume, Some(snapshot), &target.path)
  else {
    panic!("the landing was not presented");
  };
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::grant_proof(
    &secret,
    landing,
    &manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let grant = client
    .grant(landing, manifest, GrantScope::Once, GRANT_TERM_NS, proof)
    .unwrap();
  let landed = client
    .land(
      volume,
      Some(snapshot),
      &target.path,
      Filter::default(),
      Some(grant),
    )
    .unwrap();
  assert!(matches!(landed, Landing::Landed(_)), "{landed:?}");
  let steps: Vec<(AuditKind, Option<u64>)> = client
    .audit(0)
    .unwrap()
    .iter()
    .filter(|record| record.landing == Some(landing))
    .map(|record| (record.audit_kind, record.grant))
    .collect();
  for step in [
    (AuditKind::LandingPlanned, None),
    (AuditKind::GrantIssued, Some(grant)),
    (AuditKind::LandingPlanned, Some(grant)),
    (AuditKind::LandingFinished, Some(grant)),
  ] {
    assert_eq!(
      steps.iter().filter(|recorded| **recorded == step).count(),
      1,
      "{step:?} once: {steps:?}"
    );
  }
  drop(client);
  daemon.stop();
}

/// A step of a landing's trail as the audit reads it back: the record's kind, its grant, the terminal state it names and
/// its manifest.
type Step = (
  AuditKind,
  Option<u64>,
  Option<LandingStage>,
  Option<[u8; 32]>,
);

/// Every record of `landing` the audit holds, in its order.
fn trail_of(client: &mut Client, landing: u64) -> Vec<Step> {
  client
    .audit(0)
    .unwrap()
    .iter()
    .filter(|record| record.landing == Some(landing))
    .map(|record| {
      (
        record.audit_kind,
        record.grant,
        record.stage,
        record.manifest,
      )
    })
    .collect()
}

/// A presentation of `volume`'s `snapshot` (the head when `None`) into `target`, granted once under `daemon`'s issuer
/// secret as the human's surface grants it: the landing, the manifest presented and the grant.
fn presented_and_granted(
  daemon: &Daemon,
  client: &mut Client,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  target: &str,
) -> (u64, [u8; 32], u64) {
  let presented = present(client, volume, snapshot, target);
  let Ok(Landing::GrantRequired {
    landing, manifest, ..
  }) = presented
  else {
    panic!("the landing was not presented: {presented:?}");
  };
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::landing::grant_proof(
    &secret,
    landing,
    &manifest,
    GrantScope::Once,
    GRANT_TERM_NS,
  );
  let grant = client
    .grant(landing, manifest, GrantScope::Once, GRANT_TERM_NS, proof)
    .unwrap();
  (landing, manifest, grant)
}

/// A scratch volume named `name` holding `path` with `bytes`, written under a write attachment.
fn volume_holding(client: &mut Client, name: &str, path: &str, bytes: &[u8]) -> VolumeId {
  let volume = client.create(&scratch(name)).unwrap();
  write_into(client, volume, path, bytes);
  volume
}

/// Writes `bytes` as the whole of `path` in `volume`, under a write attachment held for the one write.
fn write_into(client: &mut Client, volume: VolumeId, path: &str, bytes: &[u8]) {
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), path, bytes, 0o644)
    .unwrap();
  client.detach(attachment).unwrap();
}

/// §4.15 "Audit" and its state machine (`Validating → Refused`, a conflict): a granted landing a conflict refuses leaves
/// its trail. Do: present a scratch volume's file into an empty target and grant it; then write the file's name on the
/// disk with other bytes, as an outsider would; land under the grant and read the audit. Expect the landing refused
/// `LandingConflict` naming the file, the outsider's bytes untouched, and the landing's whole trail, in order: the
/// presentation's plan, the grant, the granted run's plan and its validation, and its finish `Refused`, each naming the
/// manifest. Before 2026-11-18 a refused granted landing wrote none of its records, so the trail ended at the grant
/// (found by g90 reading the code).
#[test]
fn a_granted_landing_a_conflict_refuses_records_its_steps_and_its_refusal() {
  let (daemon, instance) = daemon("refused-conflict", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = volume_holding(&mut client, "refused-conflict", "c.txt", b"the volume's");
  let snapshot = client.snapshot(volume).unwrap();
  let (landing, manifest, grant) =
    presented_and_granted(&daemon, &mut client, volume, Some(snapshot), &target.path);
  target.seed("c.txt", b"an outsider's");
  let landed = client.land(
    volume,
    Some(snapshot),
    &target.path,
    Filter::default(),
    Some(grant),
  );
  assert!(
    matches!(
      &landed,
      Err(ClientError::Refused(Refusal::LandingConflict { entries, .. })) if entries == &["/c.txt".to_owned()]
    ),
    "{landed:?}"
  );
  #[allow(clippy::disallowed_methods)] // reading the target back, read-only
  let on_disk = std::fs::read(format!("{}/c.txt", target.path)).unwrap();
  assert_eq!(on_disk, b"an outsider's", "nothing was written");
  assert_eq!(
    trail_of(&mut client, landing),
    vec![
      (AuditKind::LandingPlanned, None, None, Some(manifest)),
      (AuditKind::GrantIssued, Some(grant), None, Some(manifest)),
      (AuditKind::LandingPlanned, Some(grant), None, Some(manifest)),
      (
        AuditKind::LandingValidated,
        Some(grant),
        None,
        Some(manifest)
      ),
      (
        AuditKind::LandingFinished,
        Some(grant),
        Some(LandingStage::Refused),
        Some(manifest)
      ),
    ]
  );
  drop(client);
  daemon.stop();
}

/// §4.15 "Audit", step 3 (a single-use grant covers exactly the manifest its human saw): a granted landing its grant does
/// not cover leaves its trail. Do: present a scratch volume's head into an empty target and grant it; write a second
/// file into the volume; land the head under the grant and read the audit. Expect the landing refused `GrantMismatch`,
/// nothing written, and its trail after the grant: the granted run's plan, of a manifest other than the presented one,
/// and its finish `Refused` naming that manifest. Before 2026-11-18 the trail ended at the grant.
#[test]
fn a_granted_landing_its_grant_does_not_cover_records_its_plan_and_its_refusal() {
  let (daemon, instance) = daemon("refused-grant", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = volume_holding(&mut client, "refused-grant", "g.txt", b"granted");
  let (landing, manifest, grant) =
    presented_and_granted(&daemon, &mut client, volume, None, &target.path);
  write_into(&mut client, volume, "later.txt", b"never approved");
  let landed = client.land(volume, None, &target.path, Filter::default(), Some(grant));
  assert!(
    matches!(landed, Err(ClientError::Refused(Refusal::GrantMismatch))),
    "{landed:?}"
  );
  assert!(
    !std::path::Path::new(&format!("{}/g.txt", target.path)).exists(),
    "nothing was written"
  );
  let trail = trail_of(&mut client, landing);
  let steps: Vec<(AuditKind, Option<u64>, Option<LandingStage>)> = trail
    .iter()
    .map(|(kind, grant, stage, _)| (*kind, *grant, *stage))
    .collect();
  assert_eq!(
    steps,
    vec![
      (AuditKind::LandingPlanned, None, None),
      (AuditKind::GrantIssued, Some(grant), None),
      (AuditKind::LandingPlanned, Some(grant), None),
      (
        AuditKind::LandingFinished,
        Some(grant),
        Some(LandingStage::Refused)
      ),
    ],
    "{trail:?}"
  );
  let planned_under_the_grant = trail[2].3;
  assert!(
    planned_under_the_grant.is_some() && planned_under_the_grant != Some(manifest),
    "the run planned the volume as it is now: {trail:?}"
  );
  assert_eq!(trail[3].3, planned_under_the_grant, "{trail:?}");
  drop(client);
  daemon.stop();
}

/// Shape: the holder the lease test takes the target's lease for: no landing attempt of the daemon's is numbered this
/// high (its counters start at one under a sixteen-bit partition).
const OTHER_HOLDER: u64 = u64::MAX;
/// Shape: the term of the lease test's hold: a minute, far past the test.
const HOLD_TERM_NS: u64 = 60_000_000_000;

/// §4.15 step 4 and its state machine (`Validating → Refused`, a lease held): a granted landing refused its target's
/// lease leaves its refusal in the trail. Do: present a scratch volume's file into an empty target and grant it; hold
/// the target's lease for another attempt; land under the grant; release the lease and read the audit. Expect the
/// landing refused `LandingLeaseHeld` naming the other holder, nothing written, and its trail after the grant: its
/// finish `Refused` alone, naming the manifest the grant binds, since it planned nothing. Before 2026-11-18 the trail
/// ended at the grant.
#[test]
fn a_granted_landing_refused_its_targets_lease_records_its_refusal() {
  let (daemon, instance) = daemon("refused-lease", None);
  let target = target_dir();
  let mut client = connect(&instance);
  let volume = volume_holding(&mut client, "refused-lease", "l.txt", b"leased");
  let snapshot = client.snapshot(volume).unwrap();
  let (landing, manifest, grant) =
    presented_and_granted(&daemon, &mut client, volume, Some(snapshot), &target.path);
  let key = lease_key_of(&target.path);
  hold(&daemon, &key, OTHER_HOLDER, HOLD_TERM_NS).unwrap();
  let landed = client.land(
    volume,
    Some(snapshot),
    &target.path,
    Filter::default(),
    Some(grant),
  );
  release(&daemon, &key, OTHER_HOLDER);
  assert!(
    matches!(
      landed,
      Err(ClientError::Refused(Refusal::LandingLeaseHeld {
        holder: OTHER_HOLDER
      }))
    ),
    "{landed:?}"
  );
  assert!(
    !std::path::Path::new(&format!("{}/l.txt", target.path)).exists(),
    "nothing was written"
  );
  assert_eq!(
    trail_of(&mut client, landing),
    vec![
      (AuditKind::LandingPlanned, None, None, Some(manifest)),
      (AuditKind::GrantIssued, Some(grant), None, Some(manifest)),
      (
        AuditKind::LandingFinished,
        Some(grant),
        Some(LandingStage::Refused),
        Some(manifest)
      ),
    ]
  );
  drop(client);
  daemon.stop();
}
