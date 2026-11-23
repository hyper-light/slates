//! A repository created through the daemon lands its creation into the node's version-control store, and the store
//! recovers it (A-767 item 7; main's ruling of 2026-11-20: "red first: a repository created, landed into and recovered,
//! read back through the store"). The store's directory is a real one beside the test binary in the build output, never
//! `/tmp` (R1).

use slates_db::catalog::{Principal, RepositoryId};
use slates_ipc::protocol::{ObjectFormat, ReplyBody, VcsTool};
use slates_land::store_root::StoreRoot;
use slates_store::log::{ChangesetLog, SectionKind, Tool};
use slates_store::store::{LOG_FILE, Store};

use crate::test_store::{
  SIZING, TestStore, created_subversion, file_work, landed_work, root, routed_create,
};

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

/// Goal 93, jupiter's red (w-k90-win, 2026-11-21: the store's owner ended with no word and every `repository create`
/// answered `Overloaded { shard: 0 }`, hiding the store's own reason). Do: start a daemon holding a store whose owner
/// cannot open it (a sizing the engine refuses), create a git repository through the verb, and read the answer its
/// request was completed with. Expect: the store's own refusal, saying it could not be opened, never `Overloaded`; no
/// repository recorded.
#[test]
fn a_creation_the_store_refuses_answers_the_stores_reason() {
  let store = TestStore::start_sized(
    "refused",
    slates_store::store::Sizing {
      memory: 0,
      disk: 0,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  );
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-refused-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let name = "refused-git";
  let partition = usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS)));
  let asked = (name, VcsTool::Git, ObjectFormat::Sha1, "main");
  let request = 1u64;
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    routed_create(state, (&owner(), request), asked)
  });
  assert!(matches!(created, ReplyBody::Acknowledged), "{created:?}");
  wait_for(&daemon, partition, super::CREATION_UNLANDED, 1);
  let id = slates_wire::request::RequestId::from_word(request);
  let answered = crate::daemon::observe_at(&daemon, partition, move |state| {
    match state.db.partition().completion(1, id.client, id.sequence) {
      slates_wire::request::Seen::Completed(bytes) => {
        <ReplyBody as slates_wire::Wire>::from_bytes(&bytes).ok()
      }
      _ => None,
    }
  });
  let Some(ReplyBody::Refused { refusal }) = answered else {
    panic!("the creation's request answered a refusal: {answered:?}");
  };
  match &refusal {
    slates_ipc::protocol::Refusal::StoreRefused { reason } => assert!(
      reason.contains("could not be opened"),
      "the store's own reason: {reason}"
    ),
    other => panic!("the store's own refusal, not {other:?}"),
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

/// The folded history of `repository`'s green: the version below which it keeps none, and its head.
fn green_folded(daemon: &Daemon, repository: RepositoryId) -> Option<(u64, u64)> {
  crate::daemon::observe_first(daemon, move |state| {
    let green = super::svn_commit::green_of(state, repository).ok()?;
    let engine = state.greens.get(&green)?;
    Some((engine.folded_below(), engine.head()))
  })
}

/// P7's finding (2) from main's attribution of 2026-11-21 (the restarted daemon's green kept every version its chain
/// replay made: 434 MB under `GreenReplay` for git-2000). Do: on a daemon holding a store, land revision 1 adding
/// `/trunk/a.txt` and revisions 2 and 3 changing it; stop the daemon and the store's owner; start both again over the
/// same segment and store. Expect: the green folded to its head after each landing (A-820), and folded to its head again
/// once the recovery replayed its chain: it keeps no version no reader names. (A chain checkpointed at each landing
/// restores folded already; a replay past the checkpoint, the batch a kill cut short, is held by the sliced commit's
/// crash test.)
#[test]
fn a_recovered_subversion_green_keeps_no_version_its_readers_do_not_name() {
  let instance = format!("vcs-store-svn-folded-{}", std::process::id());
  let mut store = TestStore::start("svn-folded");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let repository = created_subversion(&daemon, "folded");
  let texts = [&b"one\n"[..], b"one two\n", b"one two three\n"];
  for (head, bytes) in texts.iter().enumerate() {
    let head = u64::try_from(head).unwrap();
    let action = if head == 0 {
      slates_svn::index::Action::Add
    } else {
      slates_svn::index::Action::Modify
    };
    assert_eq!(
      landed_work(&daemon, repository, head, (action, bytes.to_vec())),
      Ok(head + 1)
    );
  }
  let before = green_folded(&daemon, repository);
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let after = green_folded(&daemon, repository);
  daemon.stop();
  assert_eq!(before, Some((3, 3)), "folded after each landing");
  assert_eq!(
    after,
    Some((3, 3)),
    "folded once the recovery replayed the chain"
  );
}

/// C2 stage C's sliced commit, main's ruling of 2026-11-21 12:43 (a batch's versions applied a slice a turn after its
/// durable record; a kill between slices recovers from the record). Do: for each slice boundary of `full.dump`'s
/// import (13 revisions in one batch, its apply three versions a slice: five slices), on a fresh daemon holding a store,
/// kill the batch's landing there (`staging::KILL_AT_COMMIT_SLICE`); stop the daemon and the store's owner, start both
/// again, import `full.dump` again. Expect, at every boundary: the recovered green at revision 13 and folded to it (the
/// replay past its checkpoint keeps no version, P7's finding (2)); the import resuming with nothing to land; the history
/// `full.dump` byte for byte. Before the fold at recovery, the recovered green kept every version the replay made.
/// Since A-760 the batch's entries name their texts, which the killed daemon's content store took with it: the
/// successor brings them back from the node's store before the green's replay (`svn_texts::bring_back_named`, counted,
/// the non-vacuity of this case). Red before it: the green fenced, `ContentMissing { version: 1 }`.
#[test]
fn a_batch_killed_between_its_commits_slices_recovers_and_resumes_to_the_same_history() {
  use crate::vcs_store::svn_import::tests::{FULL, FULL_LAST, history, imported};
  /// Shape: the versions a slice of the recorded apply takes here, so `full.dump`'s 13 make five slices.
  const SLICE_VERSIONS: usize = 3;
  let slices = FULL_LAST.div_ceil(u64::try_from(SLICE_VERSIONS).unwrap());
  for kill_at in 0..usize::try_from(slices).unwrap() {
    let instance = format!("vcs-store-svn-killed-{}-{kill_at}", std::process::id());
    let mut store = TestStore::start(&format!("svn-killed-{kill_at}"));
    let anchor = anchor_of(&instance);
    let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
    daemon.bootstrap(true).unwrap();
    let repository = created_subversion(&daemon, "killed");
    crate::daemon::observe_first(&daemon, move |_state| {
      crate::repository::push::SLICE_VERSIONS.with(|most| most.set(Some(SLICE_VERSIONS)));
      super::svn_commit::staging::KILL_AT_COMMIT_SLICE.with(|kill| kill.set(Some(kill_at)));
      let task = slates_rt::futures::spawn(async move {
        let _ = imported(repository, FULL).await;
      })
      .expect("the import's task");
      slates_rt::futures::detach(task).expect("detached");
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while crate::daemon::observe_first(&daemon, |_state| {
      super::svn_commit::staging::KILL_AT_COMMIT_SLICE
        .with(std::cell::Cell::get)
        .is_some()
    }) {
      assert!(
        std::time::Instant::now() < deadline,
        "the kill at {kill_at} never fired"
      );
      std::thread::yield_now();
    }
    daemon.stop();
    store.restart_owner();
    let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
    let folded = green_folded(&daemon, repository);
    let brought = brought_back(&daemon);
    let resumed = crate::test_store::run_on_first(
      &daemon,
      move || async move {
        imported(repository, FULL)
          .await
          .0
          .map(|loaded| (loaded.revisions, loaded.youngest))
      },
      std::time::Duration::from_secs(60),
    );
    let exported = crate::test_store::run_on_first(
      &daemon,
      move || history(repository),
      std::time::Duration::from_secs(60),
    );
    daemon.stop();
    assert_eq!(
      folded,
      Some((FULL_LAST, FULL_LAST)),
      "killed at {kill_at}: recovered and folded"
    );
    assert_eq!(resumed, Ok((0, FULL_LAST)), "killed at {kill_at}: resumed");
    assert!(
      exported == Some((FULL_LAST, FULL.to_vec())),
      "killed at {kill_at}: full.dump"
    );
    assert!(
      brought > 0,
      "killed at {kill_at}: no named text brought back"
    );
  }
}

/// The texts the daemon's recovery brought back for its Subversion greens' named runs (A-760's pre-pass).
fn brought_back(daemon: &Daemon) -> u64 {
  crate::daemon::observe_first(daemon, |state| {
    state
      .refusals
      .get(crate::vcs_store::svn_texts::NAMED_BROUGHT_BACK)
      .copied()
      .unwrap_or(0)
  })
}

/// A-760's crash oracle, its other side (main 01a20182's ruling of 2026-11-22 ~11:35): an entry naming a text commits
/// only once the changeset holding the text is durable. Do: import `full.dump` (13 revisions in one batch) on a daemon
/// holding a store, killing its landing once the changeset is durable and before the entry is appended
/// (`staging::KILL_BEFORE_ENTRY`); stop the daemon and the store's owner, start both again, import `full.dump` again.
/// Expect: no entry names a text the successor lacks, so nothing is brought back; the import lands every revision
/// again, its changeset at the same sequence replacing the one no entry named; the history `full.dump` byte for byte.
#[test]
fn a_batch_killed_between_its_changeset_and_its_entry_lands_again_whole() {
  use crate::vcs_store::svn_import::tests::{FULL, FULL_LAST, history, imported};
  let instance = format!("vcs-store-svn-before-entry-{}", std::process::id());
  let mut store = TestStore::start("svn-before-entry");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let repository = created_subversion(&daemon, "before-entry");
  crate::daemon::observe_first(&daemon, move |_state| {
    super::svn_commit::staging::KILL_BEFORE_ENTRY.with(|kill| kill.set(true));
    let task = slates_rt::futures::spawn(async move {
      let _ = imported(repository, FULL).await;
    })
    .expect("the import's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
  while crate::daemon::observe_first(&daemon, |_state| {
    super::svn_commit::staging::KILL_BEFORE_ENTRY.with(std::cell::Cell::get)
  }) {
    assert!(std::time::Instant::now() < deadline, "the kill never fired");
    std::thread::yield_now();
  }
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let brought = brought_back(&daemon);
  let resumed = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      imported(repository, FULL)
        .await
        .0
        .map(|loaded| (loaded.revisions, loaded.youngest))
    },
    std::time::Duration::from_secs(60),
  );
  let exported = crate::test_store::run_on_first(
    &daemon,
    move || history(repository),
    std::time::Duration::from_secs(60),
  );
  daemon.stop();
  assert_eq!(brought, 0, "a text brought back that no entry named");
  assert_eq!(resumed, Ok((FULL_LAST, FULL_LAST)), "landed again");
  assert!(exported == Some((FULL_LAST, FULL.to_vec())), "full.dump");
}

/// How many distinct objects repository `repository`'s green's checkpoint names its files by (A-869), once the
/// checkpoint covers the green's head `head`, waited for within a minute: the import answers before its batch's
/// checkpoint commits, so a test that stops the daemon at once races it (w-hg's and w-shared's finding of 2026-11-22: 1
/// run in 6 on the big Studio recovered from the entries, all 13 texts brought back). Zero for no such checkpoint.
fn checkpoint_named(daemon: &Daemon, repository: RepositoryId, head: u64) -> u64 {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
  loop {
    let named = checkpoint_named_now(daemon, repository, head);
    if named.is_some() || std::time::Instant::now() >= deadline {
      return named.unwrap_or(0);
    }
    // The harness paces its looks; the daemon under test waits on its wheel.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(std::time::Duration::from_millis(10));
  }
}

/// [`checkpoint_named`] as the shard holds it now: none until a checkpoint at `head` stands.
fn checkpoint_named_now(daemon: &Daemon, repository: RepositoryId, head: u64) -> Option<u64> {
  crate::daemon::observe_first(daemon, move |state| {
    let green = super::svn_commit::green_of(state, repository).ok()?;
    let (held, named) = state.db.partition().green_checkpoint(green)?;
    if held < head {
      return None;
    }
    let bytes = crate::merge_service::checkpoint_bytes(state, green, &named)?;
    let checkpoint = slates_merge::checkpoint::Checkpoint::decode(&bytes).ok()?;
    let distinct: std::collections::BTreeSet<[u8; 32]> = checkpoint
      .named
      .iter()
      .map(|file| file.identity.hash)
      .collect();
    u64::try_from(distinct.len()).ok()
  })
}

/// A-760's pre-pass on the cold path alone (main 01a20182's ruling of 2026-11-22 ~11:35), with A-869's checkpoint naming
/// its files. Do: import `full.dump` whole on a daemon holding a store; wait for the batch's checkpoint at its head and
/// count the objects it names; stop the daemon and the store's owner, start both again, read the history. Expect: the batch's checkpoint covers
/// every entry, so what is brought back is the texts the checkpoint names and nothing for an entry (before A-869 the
/// checkpoint carried their bytes, and nothing was brought); some named; the history `full.dump` byte for byte.
#[test]
fn a_cleanly_restarted_import_brings_back_only_its_checkpoints_named_texts() {
  use crate::vcs_store::svn_import::tests::{FULL, FULL_LAST, history, imported};
  let instance = format!("vcs-store-svn-clean-{}", std::process::id());
  let mut store = TestStore::start("svn-clean-restart");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let repository = created_subversion(&daemon, "clean");
  let loaded = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      imported(repository, FULL)
        .await
        .0
        .map(|loaded| loaded.youngest)
    },
    std::time::Duration::from_secs(60),
  );
  assert_eq!(loaded, Ok(FULL_LAST));
  let named = checkpoint_named(&daemon, repository, FULL_LAST);
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let brought = brought_back(&daemon);
  let exported = crate::test_store::run_on_first(
    &daemon,
    move || history(repository),
    std::time::Duration::from_secs(60),
  );
  daemon.stop();
  assert!(named > 0, "the checkpoint names no file");
  assert_eq!(
    brought, named,
    "what was brought back is what the checkpoint names"
  );
  assert!(exported == Some((FULL_LAST, FULL.to_vec())), "full.dump");
}

/// A-760's pre-pass beside the image's own restores (main 01a20182's ruling of 2026-11-22 ~11:55): the pre-pass makes
/// the content store before the repositories' stores are recovered (`vcs_store::recover`), and making it is what takes
/// in the image's windows and region holds (`VcsStores::content_made`, `homes::restore_region_holds`). Do: on a daemon
/// holding a store, keep a window for the region (`homes::keep_for_region`), then import `full.dump`, killing its batch
/// between its entry and its first apply slice; stop the daemon and the store's owner, start both again. Expect: texts
/// brought back (the pre-pass ran, and made the store); the region's window still held for the region and in the content
/// store; the green recovered at revision 13 and folded to it.
#[test]
fn a_region_held_window_is_restored_beside_the_named_texts_brought_back() {
  use crate::vcs_store::svn_import::tests::{FULL, FULL_LAST, imported};
  let instance = format!("vcs-store-svn-region-{}", std::process::id());
  let mut store = TestStore::start("svn-region-beside");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let repository = created_subversion(&daemon, "region");
  let window = crate::daemon::observe_first(&daemon, |state| {
    crate::vcs_store::homes::keep_for_region(state, b"a window the region homes on this shard")
      .expect("kept for the region")
      .0
  });
  crate::daemon::observe_first(&daemon, move |_state| {
    super::svn_commit::staging::KILL_AT_COMMIT_SLICE.with(|kill| kill.set(Some(0)));
    let task = slates_rt::futures::spawn(async move {
      let _ = imported(repository, FULL).await;
    })
    .expect("the import's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
  while crate::daemon::observe_first(&daemon, |_state| {
    super::svn_commit::staging::KILL_AT_COMMIT_SLICE
      .with(std::cell::Cell::get)
      .is_some()
  }) {
    assert!(std::time::Instant::now() < deadline, "the kill never fired");
    std::thread::yield_now();
  }
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let brought = brought_back(&daemon);
  let region = crate::daemon::observe_first(&daemon, move |state| {
    (
      crate::vcs_store::homes::region_holds_here(&window),
      state
        .vcs_stores
        .content()
        .is_some_and(|content| content.contains(&window)),
    )
  });
  let folded = green_folded(&daemon, repository);
  daemon.stop();
  assert!(brought > 0, "the pre-pass brought nothing back");
  assert_eq!(region, (1, true), "the region's window after the restart");
  assert_eq!(folded, Some((FULL_LAST, FULL_LAST)), "recovered and folded");
}

/// A-865. Copy a directory from an older revision after the shared plane has released its source
/// file's landed window. The durable text is still readable and the copy must land with those bytes.
#[test]
fn a_subversion_copy_from_a_stored_revision_reads_a_window_that_was_let_go() {
  use slates_svn::index::{Action, CopySource, NodeKind};
  let (_store, daemon) = crate::test_store::audit_daemon_stored_configured(|_| {});
  let repository = created_subversion(&daemon, "cold-copy");
  let first = b"source revision one";
  let identity = slates_vcs::ContentId(*blake3::hash(first).as_bytes());
  assert_eq!(
    landed_work(&daemon, repository, 0, (Action::Add, first.to_vec())),
    Ok(1)
  );
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::within_budget(state, first.len() as u64, |stores| {
      assert_eq!(
        stores.content_mut().unwrap().put_object(first).unwrap(),
        identity
      );
    });
  });
  assert_eq!(
    landed_work(
      &daemon,
      repository,
      1,
      (Action::Modify, b"source revision two".to_vec())
    ),
    Ok(2)
  );
  let (released, durable, committed, copied) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let (released, disk) = crate::state::with_state(|state| {
        let released = crate::vcs_store::within_budget(state, 0, |stores| {
          stores
            .content_mut()
            .unwrap()
            .let_go_object_landed(&identity, &|_| None)
        });
        (released, state.vcs_disk.clone().unwrap())
      })
      .unwrap();
      let durable = disk
        .text(u128::from_be_bytes(repository.bytes), identity.0, 0)
        .await
        .unwrap();
      let (mut work, _) = file_work(2, Action::Add, b"");
      let copy = work.record.changes.last_mut().unwrap();
      copy.path = "/copy".to_owned();
      copy.kind = NodeKind::Directory;
      copy.text = None;
      copy.copy_from = Some(CopySource {
        path: "/trunk".to_owned(),
        revision: 1,
      });
      work.texts.clear();
      let texts = slates_svn::commit::drive::HeldTexts::new();
      let committed = super::svn_commit::land_work((&owner(), repository), work, &texts)
        .await
        .unwrap()
        .map(|landed| landed.revision);
      let copied = crate::state::with_state(|state| {
        let green = super::svn_commit::green_of(state, repository).ok()?;
        state
          .greens
          .get(&green)?
          .content("copy/a.txt")
          .map(<[u8]>::to_vec)
      })
      .flatten();
      (released, durable, committed, copied)
    },
    std::time::Duration::from_secs(60),
  );
  daemon.stop();
  assert_eq!(released, (first.len() as u64, 0));
  assert_eq!(durable.as_deref(), Some(first.as_slice()));
  assert_eq!(committed, Ok(3));
  assert_eq!(copied.as_deref(), Some(first.as_slice()));
}

/// A-865, §4.17.4. Stop each import after r1's entry and before its first apply slice.
/// The shard's first green opens the shared content store and the next green reuses it.
/// Even a named window recorded as away in the same image has durable bytes for replay.
fn restart_with_two_named_texts(release_window: bool) {
  use crate::vcs_store::svn_import::tests::{PART1, imported};
  use std::time::{Duration, Instant};

  let synced: &[u8] = include_bytes!("../../../svn/fixtures/replay/svnsync.dump");
  let fixtures = [
    (PART1, b"alpha\nbeta\ngamma\n".as_slice()),
    (synced, b"alpha\n".as_slice()),
  ];
  let instance = format!("svn-named-replay-{}-{release_window}", std::process::id());
  let mut store = TestStore::start("svn-named-replay");
  let anchor = anchor_of(&instance);
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  daemon.bootstrap(true).unwrap();
  let repositories = [
    created_subversion(&daemon, "first"),
    created_subversion(&daemon, "second"),
  ];
  // Both root greens have the same lineage rank. Recovery visits them in volume-id order.
  let last = crate::daemon::observe_first(&daemon, move |state| {
    repositories
      .into_iter()
      .max_by_key(|repository| {
        super::svn_commit::green_of(state, *repository)
          .unwrap()
          .bytes
      })
      .unwrap()
  });
  let patience = Duration::from_secs(60);
  for (repository, (dump, expected)) in repositories.into_iter().zip(fixtures) {
    let next = b"Revision-number: 2\n";
    let end = dump
      .windows(next.len())
      .position(|bytes| bytes == next)
      .unwrap();
    let first_revision = &dump[..end];
    let release = release_window && repository == last;
    let (completed, outcome) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |_state| {
      super::svn_commit::staging::KILL_AT_COMMIT_SLICE.with(|kill| kill.set(Some(0)));
      let completed = completed.clone();
      let task = slates_rt::futures::spawn(async move {
        let result = imported(repository, first_revision).await.0;
        let _sent = completed.send(result);
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let deadline = Instant::now() + patience;
    while crate::daemon::observe_first(&daemon, |_state| {
      super::svn_commit::staging::KILL_AT_COMMIT_SLICE
        .with(std::cell::Cell::get)
        .is_some()
    }) {
      if let Ok(early) = outcome.try_recv() {
        panic!("import ended before the cut: {early:?}");
      }
      assert!(
        Instant::now() < deadline,
        "import never reached the durable entry"
      );
      std::thread::yield_now();
    }
    crate::test_store::run_on_first(
      &daemon,
      move || async move {
        let identity = slates_vcs::ContentId::of(expected);
        let disk = crate::state::with_state(|state| state.vcs_disk.clone())
          .flatten()
          .unwrap();
        let from = u128::from_be_bytes(repository.bytes);
        assert_eq!(
          disk.text(from, identity.0, 0).await.unwrap().as_deref(),
          Some(expected)
        );
        crate::state::with_state(|state| {
          let green = super::svn_commit::green_of(state, repository).unwrap();
          assert_eq!(state.db.partition().green_head(green), 1);
          assert_eq!(state.greens.get(&green).unwrap().head(), 0);
          let record = state.db.partition().green_chain(green).last().unwrap();
          let increment = crate::merge_service::entry_increment(state, green, record).unwrap();
          let entry = slates_merge::chain_entry::decode(record, &increment).unwrap();
          assert!(
            entry
              .increment
              .named
              .runs
              .iter()
              .any(|run| run.identity.hash == identity.0),
            "the durable entry names this file's text"
          );
          crate::vcs_store::within_budget(state, 0, |stores| {
            let content = stores.content_mut().unwrap();
            assert!(content.holds_whole(&identity));
            if release {
              assert!(content.mark_landed(&identity));
              assert_eq!(
                content.let_go(&identity),
                Ok(u64::try_from(expected.len()).unwrap())
              );
            }
            assert!(content.contains(&identity));
            assert_eq!(content.holds_whole(&identity), !release);
          });
        })
        .unwrap();
      },
      patience,
    );
  }
  daemon.publish_every_shard().unwrap();
  daemon.stop();
  store.restart_owner();
  let daemon = daemon_over(&instance, (&anchor.0, &anchor.1), &store);
  let mut reads = Vec::new();
  for (repository, (_, expected)) in repositories.into_iter().zip(fixtures) {
    let identity = slates_vcs::ContentId::of(expected);
    let disk = store.handle();
    let durable = crate::test_store::run_on_first(
      &daemon,
      move || {
        let disk = disk.clone();
        async move {
          disk
            .text(u128::from_be_bytes(repository.bytes), identity.0, 0)
            .await
            .unwrap()
        }
      },
      patience,
    );
    assert_eq!(
      durable.as_deref(),
      Some(expected),
      "the reopened store's own text"
    );
    let fence = crate::daemon::observe_first(&daemon, move |state| {
      let green = state
        .db
        .partition()
        .repositories()
        .branch(repository, "trunk")
        .unwrap();
      state.fenced_greens.get(&green).cloned()
    });
    let (session, file) = read_back(&daemon, repository, expected);
    reads.push((session, file, fence));
  }
  let brought = brought_back(&daemon);
  daemon.stop();
  let expected = fixtures.map(|(_, bytes)| (Some(bytes.to_vec()), Some(bytes.to_vec()), None));
  assert_eq!(
    reads.as_slice(),
    expected.as_slice(),
    "first reads after reboot, {brought} named texts fetched"
  );
}

/// A-865. With both referenced texts resident in the published image, read both after restarting.
#[test]
fn a_subversion_restart_replays_two_named_texts_whose_windows_are_resident() {
  restart_with_two_named_texts(false);
}

/// A-865. Only the later green's named window was released. Its first session and green reads must return r1's file.
#[test]
fn a_subversion_restart_replays_a_named_text_with_one_released_window() {
  restart_with_two_named_texts(true);
}

/// The re-adoption's bring-back (main 01a20182's ruling of 2026-11-23, the sibling of the remake fix cf1529f1d): a
/// takeover's re-adoption replays the green's own checkpoint and chain onto the engine it is handed, and since A-869 the
/// checkpoint names its texts. Do: on a daemon holding a store, land revision 1 adding `/trunk/a.txt`, whose checkpoint
/// names its text; leave the text the node's store's alone (the content store holds none of it) and the green as a
/// restart leaves an adopted one (its engine gone, awaiting its re-adoption through version 0); re-adopt it over a
/// seeded engine (`green_adopt::readopt_with`, as the existing readoption test hands `Green::new()`); then commit
/// revision 2. Expect: re-adopted, the file the text's bytes, the next commit revision 2. Failing first: the replay
/// fenced the green `ContentMissing` (no pre-pass), so it was not re-adopted. End to end this stays unreachable until
/// A-727 (d): an svn takeover fails today at the holder's replica build (`merge.replica.checkpoint_unread`).
#[test]
fn a_readopted_green_brings_back_the_texts_its_checkpoint_names_first() {
  use slates_svn::index::Action;
  let (_store, daemon) = crate::test_store::audit_daemon_stored_configured(|_| {});
  let repository = created_subversion(&daemon, "readopted");
  let text = b"alpha, read again\n";
  assert_eq!(
    landed_work(&daemon, repository, 0, (Action::Add, text.to_vec())),
    Ok(1)
  );
  assert!(
    checkpoint_named(&daemon, repository, 1) > 0,
    "the checkpoint names its text"
  );
  let identity = slates_vcs::ContentId(*blake3::hash(text).as_bytes());
  let green = crate::daemon::observe_first(&daemon, move |state| {
    let green = super::svn_commit::green_of(state, repository).unwrap();
    crate::vcs_store::within_budget(state, 0, |stores| {
      if let Some(content) = stores.content_mut() {
        while content.contains(&identity) {
          if content.release(&identity).is_err() {
            break;
          }
        }
      }
    });
    if let Some((lineage, Ok(mut release))) = state.greens.retire(&green) {
      while state
        .greens
        .release_step(lineage, &mut release)
        .is_ok_and(|more| more)
      {}
    }
    state.greens.remove(&green);
    state.merge.awaiting_adoption.insert(
      green,
      crate::green_adopt::AwaitingAdoption {
        through: 0,
        identity: None,
        replay: None,
      },
    );
    green
  });
  let (readopted, served) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let shard = crate::state::with_state(|state| state.shard).unwrap();
      let readopted =
        crate::green_adopt::readopt_with(shard, shard, green, slates_merge::engine::Green::new())
          .await;
      let served = crate::state::with_state(|state| {
        state
          .greens
          .get(&green)
          .and_then(|engine| engine.content("trunk/a.txt").map(<[u8]>::to_vec))
      })
      .flatten();
      (readopted, served)
    },
    std::time::Duration::from_secs(60),
  );
  let next = landed_work(&daemon, repository, 1, (Action::Modify, b"two\n".to_vec()));
  daemon.stop();
  assert_eq!(readopted, Some(true), "re-adopted");
  assert_eq!(served.as_deref(), Some(text.as_slice()), "its file");
  assert_eq!(next, Ok(2), "the next commit");
}
