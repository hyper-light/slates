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
  store_owner.stop().unwrap();

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
  let sections = log.sections(&creation).unwrap();
  assert_eq!(sections.len(), 1);
  assert_eq!(sections[0].kind, SectionKind::Record);
  let mut record = vec![0u8; usize::try_from(sections[0].len).unwrap()];
  log.read_at(&mut record, sections[0].offset).unwrap();
  let record = String::from_utf8(record).unwrap();
  assert!(record.contains("\nstored\nmain\nGit\n"), "{record}");
  drop(log);
  let mut store = Store::open(root(&dir), sizing()).unwrap();
  let rows = store.changesets(creation.header.repository, 0, 10).unwrap();
  assert_eq!(rows.len(), 1);
  assert_eq!(rows[0].identity, creation.identity);
}
