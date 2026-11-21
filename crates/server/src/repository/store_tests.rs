//! A repository created through the daemon lands its creation into the node's version-control store, and the store
//! recovers it (A-767 item 7; main's ruling of 2026-11-20: "red first: a repository created, landed into and recovered,
//! read back through the store"). The store is the one shared test store (`crate::test_store`).

use slates_db::catalog::Principal;
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};
use slates_land::store_root::StoreRoot;
use slates_store::log::{ChangesetLog, SectionKind, Tool};
use slates_store::store::{LOG_FILE, Store};

use crate::test_store::{TestStore, root, sizing};
use crate::{Daemon, SegmentSource};

/// Shape: the daemon's shards.
const SHARDS: u16 = 2;

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
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
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateRepository {
        name: name.clone(),
        tool: VcsTool::Git,
        format: ObjectFormat::Sha1,
        default_branch: "main".to_owned(),
        green: None,
      },
    )
  });
  assert!(
    matches!(created, ReplyBody::GreenCreated { .. }),
    "{created:?}"
  );
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
  loop {
    let landed = crate::daemon::observe_at(&daemon, partition, |state| {
      (
        state
          .refusals
          .get(super::CREATION_LANDED)
          .copied()
          .unwrap_or(0),
        state
          .refusals
          .get(super::CREATION_UNLANDED)
          .copied()
          .unwrap_or(0),
      )
    });
    assert_eq!(landed.1, 0, "the store refused the creation");
    if landed.0 == 1 {
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "the creation never landed"
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
  }
  daemon.stop();
  store.stop();

  // Recovered: the log read whole names one changeset, and the store's rows find it by its repository.
  let log_file = hyper_block::file::DeviceFile::adopt(
    root(&dir).open_file(LOG_FILE).unwrap(),
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
  let target = StoreRoot::identify(&dir).unwrap();
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
  let mut reopened = Store::open(root(&dir), sizing()).unwrap();
  let rows = reopened.changesets(creation.header.repository, 0, 10).unwrap();
  assert_eq!(rows.len(), 1);
  assert_eq!(rows[0].identity, creation.identity);
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
  let repository = crate::daemon::observe_at(&daemon, partition, move |state| {
    let created = crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateRepository {
        name: created_name.clone(),
        tool: VcsTool::Subversion,
        format: ObjectFormat::Revisions,
        default_branch: "trunk".to_owned(),
        green: None,
      },
    );
    assert!(
      matches!(created, ReplyBody::GreenCreated { .. }),
      "{created:?}"
    );
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
  store.stop();
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
