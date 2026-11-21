//! A repository created through the daemon lands its creation into the node's version-control store, and the store
//! recovers it (A-767 item 7; main's ruling of 2026-11-20: "red first: a repository created, landed into and recovered,
//! read back through the store"). The store's directory is a real one beside the test binary in the build output, never
//! `/tmp` (R1).

use std::path::PathBuf;

use slates_db::catalog::Principal;
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};
use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::log::{ChangesetLog, SectionKind, Tool};
use slates_store::owner::StoreOwner;
use slates_store::store::{LOG_FILE, Sizing, Store};

use crate::{Daemon, SegmentSource};

/// Shape: the daemon's shards.
const SHARDS: u16 = 2;

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// A fresh directory beside the test binary, in the build output.
#[allow(clippy::disallowed_methods)] // test code: its own scratch directory
fn store_directory(name: &str) -> PathBuf {
  let binary = std::env::current_exe().unwrap();
  let dir = binary
    .parent()
    .unwrap()
    .join(format!("vcs-store-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

fn root(dir: &std::path::Path) -> StoreRoot {
  let grant = StoreGrant {
    repository: 0,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  StoreRoot::open(dir, &grant).unwrap()
}

/// Shape: a test's store: 8 MiB of memory, 1 GiB of disk, 128 KiB extents, fanout 8.
fn sizing() -> Sizing {
  Sizing {
    memory: 8 << 20,
    disk: 1 << 30,
    extent_bytes: 128 << 10,
    fanout: 8,
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
  let mut store = Store::open(root(dir), sizing()).unwrap();
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
  let dir = store_directory("create");
  let store_owner = StoreOwner::start(root(&dir), sizing(), 16).unwrap();
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-create-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store_owner.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let name = "stored".to_owned();
  let partition = usize::from(crate::verbs::owner_of_name(&name, usize::from(SHARDS)));
  let asked = ("stored", VcsTool::Git, ObjectFormat::Sha1, "main");
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    routed_create(state, 1, asked)
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
  store_owner.stop().unwrap();

  creation_recovered(&dir);
}

/// A-767 C6 (svn onto the changeset store; main's order of 2026-11-20 19:50: "a push's landing as a changeset append,
/// and its replay", red first on the real store). Do: start a daemon holding a store, create a Subversion repository,
/// land a client's accepted work adding `/trunk` through the commit's landing (`svn_commit::land_work`), wait for its
/// changeset, stop the daemon and the store's owner, and read the log back from its files. Expect: the repository's two
/// changesets, its creation (sequence 0) and revision 1 (sequence 1, judged over 0), whose Record section is the
/// revision's record as the register holds it: `/trunk` added. Before, a commit landed nothing into the store.
#[test]
fn a_subversion_commit_lands_as_a_changeset_in_the_store() {
  let dir = store_directory("svn-commit");
  let store_owner = StoreOwner::start(root(&dir), sizing(), 16).unwrap();
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-svn-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store_owner.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let name = "stored-svn".to_owned();
  let partition = usize::from(crate::verbs::owner_of_name(&name, usize::from(SHARDS)));
  let created_name = name.clone();
  let created = crate::daemon::observe_at(&daemon, partition, move |state| {
    routed_create(
      state,
      1,
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
  store_owner.stop().unwrap();
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

/// Dispatches `CreateRepository` as the verb runner leaves a client's request (its origin, id and reply route), so a
/// creation that waits for its landing can answer.
fn routed_create(
  state: &mut crate::state::ShardState,
  request: u64,
  (name, tool, format, default_branch): (&str, VcsTool, ObjectFormat, &str),
) -> ReplyBody {
  state.current_request = Some((1, slates_wire::request::RequestId::from_word(request)));
  state.reply_route = Some(crate::merge_service::ReplyRoute {
    shard: state.shard,
    client_index: 0,
    request,
    through_work: false,
  });
  let reply = crate::verbs::dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool,
      format,
      default_branch: default_branch.to_owned(),
      green: None,
    },
  );
  state.current_request = None;
  state.reply_route = None;
  reply
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
  let dir = store_directory("kill");
  let store_owner = StoreOwner::start(root(&dir), sizing(), 16).unwrap();
  let profile = crate::daemon::test_profile();
  let instance = format!("vcs-store-kill-{}", std::process::id());
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_disk = Some(store_owner.handle());
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance }).unwrap();
  daemon.bootstrap(true).unwrap();
  let name = "killed";
  let partition = usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS)));
  let asked = (name, VcsTool::Git, ObjectFormat::Sha1, "main");
  let first = crate::daemon::observe_at(&daemon, partition, move |state| {
    super::KILL_BETWEEN_LANDING_AND_RECORD.with(|kill| kill.set(true));
    routed_create(state, 1, asked)
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
    routed_create(state, 2, asked)
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
  store_owner.stop().unwrap();
  let mut store = Store::open(root(&dir), sizing()).unwrap();
  let rows = store
    .changesets(u128::from_be_bytes(created.bytes), 0, 10)
    .unwrap();
  assert_eq!(rows.len(), 1, "the repository's creation, once");
  assert_eq!(rows[0].sequence, 0);
}
