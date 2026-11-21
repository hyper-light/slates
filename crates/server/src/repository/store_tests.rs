//! A repository created through the daemon lands its creation into the node's version-control store, and the store
//! recovers it (A-767 item 7; main's ruling of 2026-11-20: "red first: a repository created, landed into and recovered,
//! read back through the store"). The store's directory is a real one beside the test binary in the build output, never
//! `/tmp` (R1).

use slates_db::catalog::{Principal, RepositoryId};
use slates_ipc::protocol::{ObjectFormat, ReplyBody, VcsTool};
use slates_land::store_root::StoreRoot;
use slates_store::log::{ChangesetLog, SectionKind, Tool};
use slates_store::store::{LOG_FILE, Store};

use crate::test_store::{SIZING, TestStore, root, routed_create};

use crate::{Daemon, SegmentSource};

/// Shape: the daemon's shards.
const SHARDS: u16 = 2;

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// The store recovered from its files in `dir`: its log read whole names one changeset, a git creation of `stored`
/// on `main` at sequence 0 under the store's standing grant, and the store's rows find it by its repository.
fn creation_recovered(dir: &std::path::Path) {
  // Recovered: the log read whole names one changeset, and the store's rows find it by its repository.
  let log_file = hyper_block::file::DeviceFile::adopt(
    root(dir).open_file(LOG_FILE).unwrap(),
    &dir.join(LOG_FILE),
    hyper_block::file::CachingRequest::Buffered,
    hyper_block::buf::Alignment::BYTE,
  )
  .unwrap();
  let mut found = Vec::new();
  let log = ChangesetLog::recover(log_file, 0, |_, located| found.push(*located)).unwrap();
  assert_eq!(found.len(), 1, "one creation landed");
  let creation = found[0];
  assert_eq!(
    (creation.header.tool, creation.header.sequence),
    (Tool::Git, 0)
  );
  let target = StoreRoot::identify(dir).unwrap();
  assert_eq!(
    creation.header.grant,
    slates_store::store::grant_id(target.device, target.inode),
    "landed under the store's standing grant (its G row, R10)"
  );
  let sections = log.sections(&creation).unwrap();
  assert_eq!(sections.len(), 1);
  assert_eq!(sections[0].kind, SectionKind::Record);
  let mut record = vec![0u8; usize::try_from(sections[0].len).unwrap()];
  log.read_at(&mut record, sections[0].offset).unwrap();
  let record = String::from_utf8(record).unwrap();
  assert!(record.contains("\nstored\nmain\nGit\n"), "{record}");
  drop(log);
  let mut store = Store::open(root(dir), SIZING).unwrap();
  let rows = store.changesets(creation.header.repository, 0, 10).unwrap();
  assert_eq!(rows.len(), 1);
  assert_eq!(rows[0].identity, creation.identity);
}

/// A-767 item 7. Do: start a daemon holding a store, create a git repository through the verb, wait for its creation to
/// land, stop the daemon and the store's owner, then open the store again from its files. Expect: the creation is one
/// changeset of that repository, sequence 0, a git record naming the repository and its default branch, read back
/// through the recovered store's rows and log.
#[test]
fn a_created_repository_is_landed_into_the_store_and_recovered() {
  let mut store = TestStore::start("create");
  let dir = store.dir().to_path_buf();
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-create-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let name = "stored".to_owned();
  let partition = usize::from(crate::verbs::owner_of_name(&name, usize::from(SHARDS)));
  let asked = ("stored", VcsTool::Git, ObjectFormat::Sha1, "main");
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    routed_create(state, (&owner(), 1), asked)
  });
  assert!(
    matches!(created, ReplyBody::Acknowledged),
    "answered once landed and recorded: {created:?}"
  );
  wait_for(&daemon, partition, super::CREATION_LANDED, 1);
  let unlanded = crate::daemon::observe_at(&daemon, partition, |state| {
    state
      .refusals
      .get(super::CREATION_UNLANDED)
      .copied()
      .unwrap_or(0)
  });
  assert_eq!(unlanded, 0, "the store refused the creation");
  daemon.stop();
  store.stop_owner();

  creation_recovered(&dir);
}

/// The store's Windows lane (main's order of 2026-11-21: every git, hg and svn repository was unusable on Windows, its
/// daemon refusing `--vcs-store` at its start). Do: start a daemon holding a store, and create one repository of each
/// tool through the verb, each with request numbers of its own. Expect: each create answered once landed and recorded,
/// each creation landed on its name's partition, none refused by the store. On Unix and Windows alike.
#[test]
fn each_tool_creates_a_repository_into_the_store() {
  let store = TestStore::start("each-tool");
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-each-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let mut landed_on = std::collections::BTreeMap::<usize, u64>::new();
  for (request, (name, tool)) in [
    ("each-git", VcsTool::Git),
    ("each-hg", VcsTool::Mercurial),
    ("each-svn", VcsTool::Subversion),
  ]
  .into_iter()
  .enumerate()
  {
    let partition = usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS)));
    let asked = (name, tool, ObjectFormat::Sha1, "main");
    let request = u64::try_from(request).unwrap() + 1;
    let created = crate::daemon::observe_at(&daemon, partition, move |state| {
      routed_create(state, (&owner(), request), asked)
    });
    assert!(
      matches!(created, ReplyBody::Acknowledged),
      "{tool:?}: answered once landed and recorded: {created:?}"
    );
    let landed = landed_on.entry(partition).or_insert(0);
    *landed += 1;
    wait_for(&daemon, partition, super::CREATION_LANDED, *landed);
  }
  for &partition in landed_on.keys() {
    let unlanded = crate::daemon::observe_at(&daemon, partition, |state| {
      state
        .refusals
        .get(super::CREATION_UNLANDED)
        .copied()
        .unwrap_or(0)
    });
    assert_eq!(
      unlanded, 0,
      "the store refused a creation on partition {partition}"
    );
  }
  daemon.stop();
}

/// A-767 C6 (svn onto the changeset store; main's order of 2026-11-20 19:50: "a push's landing as a changeset append,
/// and its replay", red first on the real store). Do: start a daemon holding a store, create a Subversion repository,
/// land a client's accepted work adding `/trunk` through the commit's landing (`svn_commit::land_work`), wait for its
/// changeset, stop the daemon and the store's owner, and read the log back from its files. Expect: the repository's two
/// changesets, its creation (sequence 0) and revision 1 (sequence 1, judged over 0), whose Record section is the
/// revision's record as the register holds it: `/trunk` added. Before, a commit landed nothing into the store.
#[test]
fn a_subversion_commit_lands_as_a_changeset_in_the_store() {
  let mut store = TestStore::start("svn-commit");
  let dir = store.dir().to_path_buf();
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-svn-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let name = "stored-svn".to_owned();
  let partition = usize::from(crate::verbs::owner_of_name(&name, usize::from(SHARDS)));
  let created_name = name.clone();
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    routed_create(
      state,
      (&owner(), 1),
      (
        "stored-svn",
        VcsTool::Subversion,
        ObjectFormat::Revisions,
        "trunk",
      ),
    )
  });
  assert!(matches!(created, ReplyBody::Acknowledged), "{created:?}");
  wait_for(&daemon, partition, super::CREATION_LANDED, 1);
  let repository = crate::daemon::observe_at(&daemon, partition, move |state| {
    state
      .db
      .partition()
      .repositories()
      .repository_named(&created_name)
      .map(|record| record.id)
      .unwrap()
  });
  let (sent, landed) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_at(&daemon, partition, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let work = slates_svn::commit::drive::Work {
        head: 0,
        record: slates_svn::index::RevisionRecord {
          transaction: "0-1".to_owned(),
          changes: vec![slates_svn::index::ChangedPath {
            path: "/trunk".to_owned(),
            action: slates_svn::index::Action::Add,
            kind: slates_svn::index::NodeKind::Directory,
            copy_from: None,
            text: None,
            properties: None,
            mergeinfo_modified: false,
          }],
        },
        lock_entries: Vec::new(),
        revision_properties: vec![(b"svn:log".to_vec(), b"the trunk".to_vec())],
        texts: std::collections::BTreeSet::new(),
        property_lists: std::collections::BTreeMap::new(),
        locks: slates_svn::lock::CommitLocks::default(),
      };
      let texts = slates_svn::commit::drive::HeldTexts::new();
      let landed =
        crate::repository::svn_commit::land_work((&owner(), repository), work, &texts).await;
      let _ = sent.send(landed.map(|landed| landed.map(|committed| committed.revision)));
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let landed = landed
    .recv_timeout(std::time::Duration::from_secs(60))
    .expect("the landing answered");
  daemon.stop();
  store.stop_owner();
  assert_eq!(landed, Some(Ok(1)));

  let log_file = hyper_block::file::DeviceFile::adopt(
    root(&dir).open_file(LOG_FILE).unwrap(),
    &dir.join(LOG_FILE),
    hyper_block::file::CachingRequest::Buffered,
    hyper_block::buf::Alignment::BYTE,
  )
  .unwrap();
  let mut found = Vec::new();
  let log = ChangesetLog::recover(log_file, 0, |_, located| found.push(*located)).unwrap();
  let ours: Vec<_> = found
    .iter()
    .filter(|located| located.header.repository == u128::from_be_bytes(repository.bytes))
    .copied()
    .collect();
  assert_eq!(
    ours
      .iter()
      .map(|located| (
        located.header.tool,
        located.header.sequence,
        located.header.judged_over
      ))
      .collect::<Vec<_>>(),
    vec![(Tool::Subversion, 0, 0), (Tool::Subversion, 1, 0)]
  );
  let sections = log.sections(&ours[1]).unwrap();
  assert_eq!(sections.len(), 1);
  assert_eq!(sections[0].kind, SectionKind::Record);
  let mut record = vec![0u8; usize::try_from(sections[0].len).unwrap()];
  log.read_at(&mut record, sections[0].offset).unwrap();
  let decoded = slates_svn::commit::record::Record::decode(&record).unwrap();
  let slates_svn::commit::record::Record::Revision(revision) = decoded else {
    panic!("not a revision: {decoded:?}")
  };
  assert_eq!(
    revision
      .record
      .changes
      .iter()
      .map(|change| change.path.as_str())
      .collect::<Vec<_>>(),
    vec!["/trunk"]
  );
}

/// Waits until partition `partition`'s counter `counter` reaches `count`.
fn wait_for(daemon: &Daemon, partition: usize, counter: &'static str, count: u64) {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
  loop {
    let now = crate::daemon::observe_at(daemon, partition, move |state| {
      state.refusals.get(counter).copied().unwrap_or(0)
    });
    if now >= count {
      return;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "{counter} never reached {count}"
    );
    std::thread::yield_now();
  }
}

/// A-767 item 7, the one landing order (main's order of 2026-11-20, w-svn's finding). Do: create a repository through a
/// client's request with the daemon killed between its creation's two steps (the landing and the register's record);
/// look the repository up; create it again (the client's retry); stop and read the store back. Expect: the kill leaves
/// no repository (nothing the store cannot recover); the retry creates it once; the register's repository is the one
/// whose creation the store holds at sequence 0. Red before: the register recorded the repository first, so the kill
/// left one with no creation in the store.
#[test]
fn a_kill_between_a_creations_landing_and_its_record_leaves_no_repository_and_its_retry_creates_it_once()
 {
  let mut store = TestStore::start("kill");
  let dir = store.dir().to_path_buf();
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-kill-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let name = "killed";
  let partition = usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS)));
  let asked = (name, VcsTool::Git, ObjectFormat::Sha1, "main");
  let first = crate::daemon::observe_at(&daemon, partition, move |state| {
    super::KILL_BETWEEN_LANDING_AND_RECORD.with(|kill| kill.set(true));
    routed_create(state, (&owner(), 1), asked)
  });
  assert!(!matches!(first, ReplyBody::Refused { .. }), "{first:?}");
  wait_for(&daemon, partition, super::CREATION_KILLED, 1);
  let left = crate::daemon::observe_at(&daemon, partition, move |state| {
    super::KILL_BETWEEN_LANDING_AND_RECORD.with(|kill| kill.set(false));
    state
      .db
      .partition()
      .repositories()
      .repository_named(name)
      .is_some()
  });
  assert!(!left, "the kill left a repository the store cannot recover");
  let retried = crate::daemon::observe_at(&daemon, partition, move |state| {
    routed_create(state, (&owner(), 2), asked)
  });
  assert!(!matches!(retried, ReplyBody::Refused { .. }), "{retried:?}");
  wait_for(&daemon, partition, super::CREATION_LANDED, 1);
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    state
      .db
      .partition()
      .repositories()
      .repository_named(name)
      .map(|record| record.id)
  });
  let created = created.expect("the retry created the repository");
  daemon.stop();
  store.stop_owner();
  let mut store = Store::open(root(&dir), SIZING).unwrap();
  let rows = store
    .changesets(u128::from_be_bytes(created.bytes), 0, 10)
    .unwrap();
  assert_eq!(rows.len(), 1, "the repository's creation, once");
  assert_eq!(rows[0].sequence, 0);
}

/// A-767 item 7 across a restart (main's order of 2026-11-20). Do: create a repository with the daemon killed between
/// its creation's landing and its record; stop the daemon and the store's owner; start both again over the same anchor
/// segment and store; create another repository. Expect: the new repository is recorded, and the store's rows give it
/// exactly its own creation (sequence 0, its own name), never the killed one's orphaned changeset 0, whose id a
/// restarted node mints again from the register's counter (asserted: the log holds two changesets 0 of that id). Green
/// with no change: a `(repository, sequence)` row is the newest landing at it, and the register can only ever name the
/// newest, since a record follows its landing and the counter passes the id once it is recorded.
#[test]
fn a_creation_after_a_restart_never_meets_a_killed_creations_orphaned_changeset() {
  use slates_anchor::AnchorSegment;
  let mut store = TestStore::start("orphan");
  let dir = store.dir().to_path_buf();
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-orphan-{}", std::process::id());
  let config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
    .unwrap()
    .with_content(
      &crate::daemon::content_name_of(&instance),
      config.content_bytes(),
    )
    .unwrap();
  let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
  let start = |store: &TestStore| {
    let (handoff, len) = segment.handoff().unwrap();
    let mut config = config.clone();
    config.vcs_disk = Some(store.handle());
    Daemon::start(
      &profile,
      config,
      SegmentSource::Handoff {
        handoff,
        len,
        content: segment.content_handoff().unwrap(),
        rendezvous: rendezvous.adoption(),
      },
    )
    .unwrap()
  };
  let killed = "killed-first";
  let partition = usize::from(crate::verbs::owner_of_name(killed, usize::from(SHARDS)));
  let daemon = start(&store);
  daemon.bootstrap(true).unwrap();
  let first = crate::daemon::observe_at(&daemon, partition, move |state| {
    super::KILL_BETWEEN_LANDING_AND_RECORD.with(|kill| kill.set(true));
    routed_create(
      state,
      (&owner(), 1),
      (killed, VcsTool::Git, ObjectFormat::Sha1, "main"),
    )
  });
  assert!(!matches!(first, ReplyBody::Refused { .. }), "{first:?}");
  wait_for(&daemon, partition, super::CREATION_KILLED, 1);
  daemon.stop();
  store.stop_owner();

  store.restart_owner();
  let daemon = start(&store);
  // The same partition, so the new id is minted from the same counter the killed creation's was.
  let second = crate::daemon::observe_at(&daemon, partition, move |state| {
    super::KILL_BETWEEN_LANDING_AND_RECORD.with(|kill| kill.set(false));
    routed_create(
      state,
      (&owner(), 2),
      (killed, VcsTool::Git, ObjectFormat::Sha1, "trunk"),
    )
  });
  assert!(!matches!(second, ReplyBody::Refused { .. }), "{second:?}");
  wait_for(&daemon, partition, super::CREATION_LANDED, 1);
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    state
      .db
      .partition()
      .repositories()
      .repository_named(killed)
      .map(|record| record.id)
  })
  .expect("the second creation recorded");
  daemon.stop();
  store.stop_owner();

  // Non-vacuity: the restarted node minted the killed creation's id again, so the log holds two changesets 0 of it.
  let log_file = hyper_block::file::DeviceFile::adopt(
    root(&dir).open_file(LOG_FILE).unwrap(),
    &dir.join(LOG_FILE),
    hyper_block::file::CachingRequest::Buffered,
    hyper_block::buf::Alignment::BYTE,
  )
  .unwrap();
  let mut at_zero = 0;
  let log = ChangesetLog::recover(log_file, 0, |_, located| {
    if located.header.repository == u128::from_be_bytes(created.bytes)
      && located.header.sequence == 0
    {
      at_zero += 1;
    }
  })
  .unwrap();
  drop(log);
  assert_eq!(at_zero, 2, "the orphan and the new creation share the id");
  let mut store = Store::open(root(&dir), SIZING).unwrap();
  let rows = store
    .changesets(u128::from_be_bytes(created.bytes), 0, 10)
    .unwrap();
  assert_eq!(rows.len(), 1, "one creation for the repository: {rows:?}");
  let sections = store.sections(&rows[0]).unwrap();
  let record = store.read(sections[0].offset, sections[0].len).unwrap();
  let record = String::from_utf8(record).unwrap();
  assert!(
    record.contains("\ntrunk\n"),
    "the repository's own creation, not the orphan's: {record}"
  );
}

/// A daemon of one shard over the anchor's `segment` (and its content object) holding `store`'s handle: what a node's
/// restart starts again over the segment and store the dead daemon left.
fn daemon_over(
  instance: &str,
  (segment, rendezvous): (&slates_anchor::AnchorSegment, &slates_ipc::HeldRendezvous),
  store: &TestStore,
) -> Daemon {
  let profile = crate::daemon::test_profile();
  let mut config = crate::DaemonConfig::derive(&profile, instance, Some(1));
  config.vcs_disk = Some(store.handle());
  let (handoff, len) = segment.handoff().unwrap();
  let source = SegmentSource::Handoff {
    handoff,
    len,
    content: segment.content_handoff().unwrap(),
    rendezvous: rendezvous.adoption(),
  };
  Daemon::start(&profile, config, source).unwrap()
}

/// The anchor's segment and content object for `instance`, sized as the anchor sizes them, and its rendezvous.
fn anchor_of(instance: &str) -> (slates_anchor::AnchorSegment, slates_ipc::HeldRendezvous) {
  let profile = crate::daemon::test_profile();
  let config = crate::DaemonConfig::derive(&profile, instance, Some(1));
  let segment = slates_anchor::AnchorSegment::create(
    &format!("{instance}-seg"),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(&format!("{instance}-con"), config.content_bytes())
  .unwrap();
  (segment, slates_ipc::HeldRendezvous::hold(instance).unwrap())
}

/// Creates Subversion repository `name` on `daemon`'s one shard through a client's request, waiting for its creation to
/// land on the store and be recorded (A-767 item 7's one order).
fn created_subversion(daemon: &Daemon, name: &str) -> RepositoryId {
  let asked = (name, VcsTool::Subversion, ObjectFormat::Revisions, "trunk");
  crate::test_store::create_on(daemon, 0, &owner(), asked).0
}

/// A client's work on revision `head` setting `/trunk/a.txt` to `bytes` (`action`: add or modify), `/trunk` added with
/// it at revision 0, with its text held as a commit's sink holds it.
fn file_work(
  head: u64,
  action: slates_svn::index::Action,
  bytes: &[u8],
) -> (
  slates_svn::commit::drive::Work,
  slates_svn::commit::drive::HeldTexts,
) {
  use slates_svn::commit::drive::TextSink;
  use slates_svn::index::{Action, ChangedPath, NodeKind};
  let text = slates_svn::commit::drive::text_of(bytes).unwrap();
  let texts = slates_svn::commit::drive::HeldTexts::new();
  let staged = texts.stage().unwrap();
  texts.append(staged, bytes).unwrap();
  texts.seal(staged, text.identity).unwrap();
  let mut changes = Vec::new();
  if head == 0 {
    changes.push(ChangedPath {
      path: "/trunk".to_owned(),
      action: Action::Add,
      kind: NodeKind::Directory,
      copy_from: None,
      text: None,
      properties: None,
      mergeinfo_modified: false,
    });
  }
  changes.push(ChangedPath {
    path: "/trunk/a.txt".to_owned(),
    action,
    kind: NodeKind::File,
    copy_from: None,
    text: Some(text),
    properties: None,
    mergeinfo_modified: false,
  });
  let work = slates_svn::commit::drive::Work {
    head,
    record: slates_svn::index::RevisionRecord {
      transaction: format!("{head}-1"),
      changes,
    },
    lock_entries: Vec::new(),
    revision_properties: vec![(b"svn:log".to_vec(), format!("r{}", head + 1).into_bytes())],
    texts: std::iter::once(text.identity).collect(),
    property_lists: std::collections::BTreeMap::new(),
    locks: slates_svn::lock::CommitLocks::default(),
  };
  (work, texts)
}

/// Lands `work` on `repository` through the client's commit landing (`svn_commit::land_work`), as a task on `daemon`'s
/// shard: the revision it committed, or why not.
fn landed_work(
  daemon: &Daemon,
  repository: RepositoryId,
  head: u64,
  (action, bytes): (slates_svn::index::Action, Vec<u8>),
) -> Result<u64, super::svn_commit::Uncommitted> {
  crate::test_store::run_on_first(
    daemon,
    move || async move {
      let (work, texts) = file_work(head, action, &bytes);
      super::svn_commit::land_work((&owner(), repository), work, &texts)
        .await
        .unwrap()
        .map(|committed| committed.revision)
    },
    std::time::Duration::from_secs(60),
  )
}

/// Everything recovery must make again of `repository`: its records whole as they print, their youngest revision, its
/// green's head, and the sequence each of its register's records names (each a reference: the register holds no
/// record's bytes), with what the recovery refused.
fn svn_seen(
  state: &crate::state::ShardState,
  repository: RepositoryId,
) -> (String, Option<u64>, Option<u64>, Vec<Option<u64>>) {
  let records = state.vcs_stores.subversion_records(repository);
  let green = super::svn_commit::green_of(state, repository).ok();
  (
    format!(
      "{records:?} unrecovered {:?}",
      state.vcs_stores.unrecovered(repository)
    ),
    records.map(slates_svn::migration::Repository::youngest),
    green.and_then(|green| state.greens.get(&green).map(|engine| engine.head())),
    state
      .db
      .partition()
      .repositories()
      .objects(repository)
      .map(super::svn_commit::landing::sequence_of)
      .collect(),
  )
}

/// The text `bytes` as a session reads it from the recovered repository (by its identity), and `/trunk/a.txt` as the
/// green holds it.
fn read_back(
  daemon: &Daemon,
  repository: RepositoryId,
  bytes: &[u8],
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
  let identity = *blake3::hash(bytes).as_bytes();
  crate::daemon::observe_first(daemon, move |state| {
    // Read as a session reads it: through the green, then the content store, then the records (`svn_view::lend_in`).
    let held = crate::vcs_store::svn_view::lend_in(state, repository, |lent| {
      slates_svn::reads::Repository::text(lent.repository, &identity, 0)
        .map(std::borrow::Cow::into_owned)
    })
    .flatten();
    let green = super::svn_commit::green_of(state, repository).ok();
    let file = green
      .and_then(|green| state.greens.get(&green))
      .and_then(|engine| engine.content("trunk/a.txt").map(<[u8]>::to_vec));
    (held, file)
  })
}

/// Revision 1's changeset carries the text it put (C2 stage B): its payload `alpha\n`, the `Texts` entry naming it there,
/// then its record.
fn carries_its_text(
  log: &ChangesetLog<hyper_block::file::DeviceFile>,
  located: &slates_store::log::Located,
) {
  let sections = log.sections(located).unwrap();
  let kinds: Vec<SectionKind> = sections.iter().map(|section| section.kind).collect();
  assert_eq!(
    kinds,
    vec![
      SectionKind::Payload,
      SectionKind::Texts,
      SectionKind::Record
    ]
  );
  let read = |section: &slates_store::log::Section| {
    let mut bytes = vec![0u8; usize::try_from(section.len).unwrap()];
    log.read_at(&mut bytes, section.offset).unwrap();
    bytes
  };
  let payload = read(&sections[0]);
  let entries = slates_store::texts::decode(&read(&sections[1])).unwrap();
  assert_eq!(payload, b"alpha\n");
  let entry = &entries[0];
  assert_eq!(
    (
      entries.len(),
      entry.path.as_slice(),
      entry.text,
      entry.len,
      entry.base
    ),
    (
      1,
      b"/trunk/a.txt".as_slice(),
      *blake3::hash(b"alpha\n").as_bytes(),
      6,
      None
    )
  );
  assert_eq!(entry.place.payload, *blake3::hash(&payload).as_bytes());
}

/// C2's arbiter (main's ruling of 2026-11-20 20:30, item 4: "the state before the kill must equal the state recovered
/// from the store"). Do: on a daemon holding a store, create a Subversion repository, land revision 1 (`/trunk` and a
/// file), a revision's properties set whole, and revision 2 changing the file; stop the daemon (a stop publishes
/// nothing, so what survives is what was made durable) and the store's owner; start both again over the same segment
/// and store. Expect: the recovered repository equal to the one before the stop (records whole, youngest 2, the green
/// at version 2), with nothing refused; its register four references naming sequences 0 to 3 (the creation, the two
/// revisions and the properties), no record's bytes; the store's log holding those four changesets, revision 1's
/// carrying its text (C2 stage B: its payload, the `Texts` entry naming it there, then its record); and the file read
/// back at revision 2.
#[test]
fn a_restarted_daemon_recovers_subversion_from_the_store_as_it_stood() {
  let instance = format!("vcs-store-svn-arbiter-{}", std::process::id());
  let mut store = TestStore::start("svn-arbiter");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let repository = created_subversion(&daemon, "arbiter");
  let first = b"alpha\n".to_vec();
  let second = b"alpha beta\n".to_vec();
  assert_eq!(
    landed_work(
      &daemon,
      repository,
      0,
      (slates_svn::index::Action::Add, first)
    ),
    Ok(1)
  );
  let set = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let mut properties = std::collections::BTreeMap::new();
      properties.insert(b"svn:log".to_vec(), b"the first, again".to_vec());
      properties.insert(b"svn:author".to_vec(), b"alice".to_vec());
      super::svn_commit::land_record(
        (&owner(), repository),
        slates_svn::commit::record::Record::RevisionProperties {
          revision: 1,
          properties,
        },
      )
      .await
      .unwrap()
    },
    std::time::Duration::from_secs(60),
  );
  assert_eq!(set, Ok(()));
  assert_eq!(
    landed_work(
      &daemon,
      repository,
      1,
      (slates_svn::index::Action::Modify, second.clone())
    ),
    Ok(2)
  );
  let before = crate::daemon::observe_first(&daemon, move |state| svn_seen(state, repository));
  assert_eq!(before.1, Some(2));
  assert_eq!(before.2, Some(2));
  assert_eq!(before.3, vec![Some(0), Some(1), Some(2), Some(3)]);
  daemon.stop();
  store.restart_owner();

  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let after = crate::daemon::observe_first(&daemon, move |state| svn_seen(state, repository));
  assert_eq!(after, before);
  assert_eq!(
    read_back(&daemon, repository, &second),
    (Some(second.clone()), Some(second))
  );
  daemon.stop();
  store.stop_owner();

  let log_file = hyper_block::file::DeviceFile::adopt(
    root(store.dir()).open_file(LOG_FILE).unwrap(),
    &store.dir().join(LOG_FILE),
    hyper_block::file::CachingRequest::Buffered,
    hyper_block::buf::Alignment::BYTE,
  )
  .unwrap();
  let mut found = Vec::new();
  let log = ChangesetLog::recover(log_file, 0, |_, located| found.push(*located)).unwrap();
  let ours: Vec<_> = found
    .iter()
    .filter(|located| located.header.repository == u128::from_be_bytes(repository.bytes))
    .copied()
    .collect();
  let sequences: Vec<u64> = ours.iter().map(|located| located.header.sequence).collect();
  assert_eq!(sequences, vec![0, 1, 2, 3]);
  carries_its_text(&log, &ours[1]);
}

/// C2's crash between the store's landing and the register's commit (main's rulings of 2026-11-20 20:30 item 5 and
/// 21:05 item 2: "a crash between the land and the commit recovers to the state before the land, and the retried commit
/// lands once"). Do: on a daemon holding a store, create a Subversion repository and land revision 1; then land revision
/// 2's changeset on the store at the next sequence, as a landing does, and stop the daemon before any register entry
/// names it (the crash); start the daemon and the store again; then commit revision 2 again, as the unanswered client
/// retries it. Expect: the recovered repository as it stood before the landing (youngest 1, its register naming
/// sequences 0 and 1 alone); the retry answered revision 2, committed once (youngest 2, the register naming 0 to 2);
/// and a second restart recovering that, revision 2's text the retry's.
#[test]
fn a_changeset_landed_and_never_named_is_dropped_at_recovery_and_its_retry_lands_once() {
  let instance = format!("vcs-store-svn-crash-{}", std::process::id());
  let mut store = TestStore::start("svn-crash");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let repository = created_subversion(&daemon, "crash");
  assert_eq!(
    landed_work(
      &daemon,
      repository,
      0,
      (slates_svn::index::Action::Add, b"one\n".to_vec())
    ),
    Ok(1)
  );
  let before = crate::daemon::observe_first(&daemon, move |state| svn_seen(state, repository));
  assert_eq!(before.3, vec![Some(0), Some(1)]);
  // The landing's first half: revision 2's record landed on the store at the next sequence, durable, no entry naming it.
  let disk = store.handle();
  let dropped = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let (work, _texts) = file_work(1, slates_svn::index::Action::Modify, b"two, lost\n");
      let revision = slates_svn::commit::revision::Revision::of_work(
        work,
        slates_svn::lock::AprTime(1_800_000_000_000_000),
      )
      .unwrap();
      let record = slates_svn::commit::record::Record::revision_bytes(&revision);
      super::svn_commit::landing::land(&disk, (repository, 2), vec![(SectionKind::Record, record)])
        .await
    },
    std::time::Duration::from_secs(60),
  );
  assert_eq!(dropped, Ok(()));
  daemon.stop();
  store.restart_owner();

  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let recovered = crate::daemon::observe_first(&daemon, move |state| svn_seen(state, repository));
  assert_eq!(recovered, before);
  assert_eq!(
    landed_work(
      &daemon,
      repository,
      1,
      (slates_svn::index::Action::Modify, b"two\n".to_vec())
    ),
    Ok(2)
  );
  let retried = crate::daemon::observe_first(&daemon, move |state| svn_seen(state, repository));
  assert_eq!(retried.1, Some(2));
  assert_eq!(retried.3, vec![Some(0), Some(1), Some(2)]);
  daemon.stop();
  store.restart_owner();

  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let again = crate::daemon::observe_first(&daemon, move |state| svn_seen(state, repository));
  assert_eq!(again, retried);
  let two = b"two\n".to_vec();
  assert_eq!(
    read_back(&daemon, repository, &two),
    (Some(two.clone()), Some(two))
  );
  daemon.stop();
}
