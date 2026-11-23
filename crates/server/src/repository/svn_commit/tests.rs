//! A-655 part 2b's tests (§4.17.4 items 1 and 3.5): a Subversion repository made with its creation's record, revisions
//! landed through the owner's step as its green's versions with their records, a moved work judged again, refusals
//! that change nothing, and the records made again from the partition's log.

use std::collections::BTreeMap;

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{ObjectFormat, Refusal, VcsTool};
use slates_svn::commit::drive::Work;
use slates_svn::commit::record::Record;
use slates_svn::commit::revision::{Revision, StepRefusal};
use slates_svn::commit::{CommitRefusal, ConflictKind};
use slates_svn::index::{
  Action, ChangedPath, ContentIdentity, CopySource, NodeKind, PropertyList, RevisionRecord, Text,
};
use slates_svn::lock::{AprTime, CommitLocks};

use super::{Committed, PROPS, UNSUPPORTED, Uncommitted};
use crate::repository::push::AppendRefused;
use crate::state::ShardState;

/// Shape: the date every test revision carries.
const DATE: &str = "2026-11-19T20:00:00.000000Z";
/// Shape: [`DATE`]'s instant: microseconds since the epoch.
const DATE_INSTANT: AprTime = AprTime(1_795_118_400_000_000);

/// The repository's owner.
pub(crate) fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates Subversion repository `name` from a history running as a task on the shard, through a client's request,
/// once its creation landed on the node's store and was recorded (A-767 item 7): its id and its one green.
pub(crate) async fn subversion(name: &str) -> (RepositoryId, DbVolumeId) {
  crate::test_store::created(
    &owner(),
    (name, VcsTool::Subversion, ObjectFormat::Revisions, "main"),
  )
  .await
}

/// `bytes`' content identity.
pub(crate) fn identity(bytes: &[u8]) -> ContentIdentity {
  *blake3::hash(bytes).as_bytes()
}

/// A text of `bytes` as a row names it; its digests are the drive's to check, and no part of the step.
fn text(bytes: &[u8]) -> Text {
  Text {
    identity: identity(bytes),
    md5: [0; 16],
    sha1: [0; 20],
    size: u64::try_from(bytes.len()).unwrap(),
  }
}

/// A property list of one property in FSFS's form (`hash.c` `svn_hash_write2`).
fn list(name: &str, value: &str) -> Vec<u8> {
  format!(
    "K {}\n{name}\nV {}\n{value}\nEND\n",
    name.len(),
    value.len()
  )
  .into_bytes()
}

/// The list `bytes` as a row names it.
fn held(bytes: &[u8]) -> PropertyList {
  PropertyList::Held {
    identity: identity(bytes),
    has_mergeinfo: false,
  }
}

/// A row of `action` on the node of `kind` at `path`, changing neither text nor properties.
fn row(path: &str, action: Action, kind: NodeKind) -> ChangedPath {
  ChangedPath {
    path: path.to_owned(),
    action,
    kind,
    copy_from: None,
    text: None,
    properties: None,
    mergeinfo_modified: false,
  }
}

/// A row giving the file at `path` the text `bytes`.
fn file(path: &str, action: Action, bytes: &[u8]) -> ChangedPath {
  ChangedPath {
    text: Some(text(bytes)),
    ..row(path, action, NodeKind::File)
  }
}

/// The revision after `head` of `rows`, bringing `contents`, by alice.
fn revision(head: u64, rows: Vec<ChangedPath>, contents: &[&[u8]]) -> Revision {
  Revision {
    head,
    record: RevisionRecord {
      transaction: format!("{head}-1"),
      changes: rows,
    },
    properties: BTreeMap::from([
      (b"svn:author".to_vec(), b"alice".to_vec()),
      (b"svn:date".to_vec(), DATE.as_bytes().to_vec()),
      (b"svn:log".to_vec(), b"a change".to_vec()),
    ]),
    contents: contents
      .iter()
      .map(|bytes| (identity(bytes), bytes.to_vec()))
      .collect(),
    stored: BTreeMap::new(),
    lock_entries: Vec::new(),
  }
}

/// `/trunk`'s property list.
fn ignores() -> Vec<u8> {
  list("svn:ignore", "*.o")
}

/// Revision 1: `/trunk` holding a property list, `/trunk/a.txt`, and `/trunk/sub` holding `b.txt`.
pub(crate) fn first() -> Revision {
  let trunk = ChangedPath {
    properties: Some(held(&ignores())),
    ..row("/trunk", Action::Add, NodeKind::Directory)
  };
  let rows = vec![
    trunk,
    file("/trunk/a.txt", Action::Add, b"alpha\n"),
    row("/trunk/sub", Action::Add, NodeKind::Directory),
    file("/trunk/sub/b.txt", Action::Add, b"beta\n"),
  ];
  revision(0, rows, &[&ignores(), b"alpha\n", b"beta\n"])
}

/// The answer to a revision of the tests' properties made as `number`.
fn answered(number: u64) -> Result<Committed, Uncommitted> {
  Ok(Committed {
    revision: number,
    date: Some(DATE.to_owned()),
    author: Some("alice".to_owned()),
  })
}

/// What the green holds at `path`: a file's bytes.
fn green_file(state: &ShardState, green: DbVolumeId, path: &str) -> Option<Vec<u8>> {
  let engine = state.greens.get(&green)?;
  engine.content_bytes(path).map(<[u8]>::to_vec)
}

/// Whether the green holds a directory at `path`.
fn green_dir(state: &ShardState, green: DbVolumeId, path: &str) -> bool {
  state
    .greens
    .get(&green)
    .is_some_and(|engine| engine.is_dir(path))
}

/// The property list's identity the green names at `path`.
fn green_props(state: &ShardState, green: DbVolumeId, path: &str) -> Option<Vec<u8>> {
  let engine = state.greens.get(&green)?;
  engine.xattr(path, PROPS).map(<[u8]>::to_vec)
}

/// The green's head.
fn green_head(state: &ShardState, green: DbVolumeId) -> Option<u64> {
  state.greens.get(&green).map(|engine| engine.head())
}

/// The records the repository's register holds, in its order.
fn logged(state: &ShardState, repository: RepositoryId) -> Vec<Vec<u8>> {
  state
    .db
    .partition()
    .repositories()
    .objects(repository)
    .map(<[u8]>::to_vec)
    .collect()
}

/// The records' youngest revision, the bytes they hold, and what the shard's budget is charged for them.
fn kept(state: &mut ShardState, repository: RepositoryId) -> (u64, u64, u64) {
  let (youngest, held) = state
    .vcs_stores
    .subversion_records(repository)
    .map_or((u64::MAX, 0), |records| {
      (records.youngest(), records.held())
    });
  let charged = state
    .vcs_stores
    .subversion(repository)
    .map_or(0, |svn| svn.charged);
  (youngest, held, charged)
}

/// Everything a refusal must leave as it was: the green's head, the records whole as they print, the register's
/// records and the charge.
fn whole(
  state: &mut ShardState,
  (repository, green): (RepositoryId, DbVolumeId),
) -> (Option<u64>, String, usize, (u64, u64, u64)) {
  let printed = format!("{:?}", state.vcs_stores.subversion_records(repository));
  (
    green_head(state, green),
    printed,
    logged(state, repository).len(),
    kept(state, repository),
  )
}

/// Shape: how long a test's history may run on its shard before the test fails.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// Runs `history` as a task on a fresh store-backed audit daemon's shard (C2: a Subversion record lands on the node's
/// changeset store before the register names it, so a commit is awaited), the daemon and its store given back after.
pub(crate) fn stored<T: Send + 'static, F: std::future::Future<Output = T> + 'static>(
  history: impl FnOnce() -> F + Clone + Send + 'static,
) -> T {
  stored_configured(history, |_| {})
}

/// [`stored`] on a daemon whose configuration `configure` changed first.
fn stored_configured<T: Send + 'static, F: std::future::Future<Output = T> + 'static>(
  history: impl FnOnce() -> F + Clone + Send + 'static,
  configure: impl FnOnce(&mut crate::DaemonConfig),
) -> T {
  let (_store, daemon) = crate::test_store::audit_daemon_stored_configured(configure);
  let out = crate::test_store::run_on_first(&daemon, history, PATIENCE);
  daemon.stop();
  out
}

/// The shard's state, borrowed for one step of a history.
pub(crate) fn on<R>(step: impl FnOnce(&mut ShardState) -> R) -> R {
  crate::state::with_state(step).expect("the shard's state")
}

/// `revision` landed on `svn` by `principal` through the step (`svn_commit::land_revision`): its answer.
pub(crate) async fn commit(
  (principal, svn): (&Principal, RepositoryId),
  revision: Revision,
  judged: Option<(&CommitLocks, AprTime)>,
) -> Result<Committed, Uncommitted> {
  super::land_revision((principal, svn), revision, judged)
    .await
    .expect("the shard's state")
}

/// `svn`'s store made again from its changesets on the node's store, its turns run to their end.
pub(crate) async fn rebuild(svn: RepositoryId) {
  on(|state| crate::vcs_store::rebuild(state, svn));
  crate::vcs_store::rebuilt(svn).await;
}

/// `svn`'s changeset `sequence`'s record as the node's store holds it, once landed (a creation lands as a task).
pub(crate) async fn stored_record(svn: RepositoryId, sequence: u64) -> Record {
  let disk = on(|state| state.vcs_disk.clone()).expect("the node's store");
  let deadline = slates_rt::futures::now_ns()
    .saturating_add(u64::try_from(PATIENCE.as_nanos()).unwrap_or(u64::MAX));
  loop {
    if let Ok(bytes) = super::landing::changeset_record(&disk, svn, sequence).await {
      return Record::decode(&bytes).expect("a Subversion record");
    }
    assert!(
      slates_rt::futures::now_ns() < deadline,
      "changeset {sequence} never landed"
    );
    slates_rt::futures::yield_now().await;
  }
}

/// Waits until `svn`'s records reach revision `youngest` (a submitted version's revision lands as a task, C2).
pub(crate) async fn youngest_reaches(svn: RepositoryId, youngest: u64) {
  let deadline = slates_rt::futures::now_ns()
    .saturating_add(u64::try_from(PATIENCE.as_nanos()).unwrap_or(u64::MAX));
  while on(|state| kept(state, svn).0) != youngest {
    assert!(
      slates_rt::futures::now_ns() < deadline,
      "revision {youngest} never landed"
    );
    slates_rt::futures::yield_now().await;
  }
}

/// A-655 part 2b (`CreateRepository` of a Subversion repository; `svnadmin create`'s UUID and revision 0's date). Do:
/// create a Subversion repository. Expect: its records at revision 0, its UUID its id's sixteen bytes in RFC 4122's
/// form, revision 0's `svn:date` the one its creation record holds; the register's one record a reference to changeset 0,
/// whose record on the node's store is the creation's (C2); the green at version 0; the shard charged what the records
/// hold.
#[test]
fn t_a655_a_subversion_repository_is_made_with_its_creation_record() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    let records = on(|state| logged(state, svn));
    let sequences: Vec<_> = records
      .iter()
      .map(|bytes| super::landing::sequence_of(bytes))
      .collect();
    assert_eq!(
      sequences,
      vec![Some(0)],
      "the register names the creation alone, by reference"
    );
    let created = stored_record(svn, 0).await;
    let Record::Created { uuid, date } = created else {
      panic!("not a creation: {created:?}")
    };
    let made = on(|state| {
      state.vcs_stores.subversion_records(svn).map(|records| {
        let dated = records
          .revision_properties(0)
          .and_then(|properties| properties.get(b"svn:date".as_slice()))
          .cloned();
        (records.youngest(), records.uuid(), dated)
      })
    });
    assert_eq!(made, Some((0, uuid, Some(date))));
    let digits: String = svn.bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let printed = String::from_utf8(uuid.as_bytes().to_vec()).unwrap();
    assert_eq!(printed.replace('-', ""), digits);
    let (_, held, charged) = on(|state| kept(state, svn));
    assert_eq!(
      (on(|state| green_head(state, green)), charged),
      (Some(0), held)
    );
  });
}

/// A-655 part 2b (§4.17.4 items 1 and 3.5). Do: commit revision 1 (a directory with a property list, a file, a
/// directory holding a file) through the step. Expect: answered revision 1 with its date and author; the green at
/// version 1 holding each file's bytes, the directories, and the directory's list named by its identity; the
/// register's second record a reference to changeset 1, whose record on the node's store is the revision with its texts
/// named by identity and length (C2; the texts are its changeset's payload, stage B part 2 (b)); the records at revision
/// 1 naming the texts stored, none of their bytes in RAM; the shard charged what they hold.
#[test]
fn t_a655_a_revision_lands_as_the_greens_next_version_with_its_record() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    let committed = commit((&owner(), svn), first(), None).await;
    assert_eq!(committed, answered(1));
    let seen = (
      on(|state| green_head(state, green)),
      on(|state| green_file(state, green, "trunk/a.txt")),
      on(|state| green_file(state, green, "trunk/sub/b.txt")),
      on(|state| green_dir(state, green, "trunk/sub")),
      on(|state| green_props(state, green, "trunk")),
    );
    let expected = (
      Some(1),
      Some(b"alpha\n".to_vec()),
      Some(b"beta\n".to_vec()),
      true,
      Some(identity(&ignores()).to_vec()),
    );
    assert_eq!(seen, expected);
    let records = on(|state| logged(state, svn));
    let second = records
      .get(1)
      .and_then(|bytes| super::landing::sequence_of(bytes));
    assert_eq!(second, Some(1));
    assert_eq!(
      stored_record(svn, 1).await,
      Record::Revision(texts_stored(first()))
    );
    let alpha = on(|state| {
      state.vcs_stores.subversion_records(svn).map(|records| {
        let alpha = identity(b"alpha\n");
        (records.stored_len(&alpha), records.text(&alpha).is_some())
      })
    });
    assert_eq!(alpha, Some((Some(6), false)));
    let (youngest, held, charged) = on(|state| kept(state, svn));
    assert_eq!((youngest, charged), (1, held));
  });
}

/// `revision` as its landing records it (C2 stage B part 2 (b)): each text its rows name moved out of its contents and
/// named among its stored texts by its length.
fn texts_stored(mut revision: Revision) -> Revision {
  for text in revision
    .record
    .changes
    .iter()
    .filter_map(|change| change.text)
  {
    if revision.contents.remove(&text.identity).is_some() {
      revision.stored.insert(text.identity, text.size);
    }
  }
  revision
}

/// Revision 2: `/branches/b` copied from `/trunk` at revision 1 (the head), its `a.txt` changed and its `sub` deleted.
fn branched() -> Revision {
  let copied = ChangedPath {
    copy_from: Some(CopySource {
      path: "/trunk".to_owned(),
      revision: 1,
    }),
    ..row("/branches/b", Action::Add, NodeKind::Directory)
  };
  let rows = vec![
    row("/branches", Action::Add, NodeKind::Directory),
    copied,
    file("/branches/b/a.txt", Action::Modify, b"alpha 2\n"),
    row("/branches/b/sub", Action::Delete, NodeKind::Directory),
  ];
  revision(1, rows, &[b"alpha 2\n"])
}

/// A-655 part 2b (the green's version: a copy by reference, a change beneath it, deletions, a replacement without
/// history). Do: commit revision 1; revision 2, `branched`; revision 3 deleting `/trunk`; revision 4 replacing
/// `/branches/b/a.txt` without history by a file of another text with a property list. Expect: after 2, the branch
/// holds the copy's list, its `a.txt` changed, no `sub`, and `/trunk` as it was; after 3, nothing of `/trunk`; after 4,
/// the replaced file's new text and list; the green at version 4.
#[test]
fn t_a655_copies_deletions_and_a_replacement_land_on_the_green() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    assert_eq!(commit((&owner(), svn), branched(), None).await, answered(2));
    let after_copy = (
      on(|state| green_props(state, green, "branches/b")),
      on(|state| green_file(state, green, "branches/b/a.txt")),
      on(|state| green_dir(state, green, "branches/b/sub")),
      on(|state| green_file(state, green, "branches/b/sub/b.txt")),
      on(|state| green_file(state, green, "trunk/a.txt")),
    );
    let expected = (
      Some(identity(&ignores()).to_vec()),
      Some(b"alpha 2\n".to_vec()),
      false,
      None,
      Some(b"alpha\n".to_vec()),
    );
    assert_eq!(after_copy, expected);
    let deleted = revision(
      2,
      vec![row("/trunk", Action::Delete, NodeKind::Directory)],
      &[],
    );
    assert_eq!(commit((&owner(), svn), deleted, None).await, answered(3));
    let gone = (
      on(|state| green_dir(state, green, "trunk")),
      on(|state| green_dir(state, green, "trunk/sub")),
      on(|state| green_file(state, green, "trunk/sub/b.txt")),
    );
    assert_eq!(gone, (false, false, None));
    let native = list("svn:eol-style", "native");
    let replaced = ChangedPath {
      properties: Some(held(&native)),
      ..file("/branches/b/a.txt", Action::Replace, b"gamma\n")
    };
    let replacing = revision(3, vec![replaced], &[b"gamma\n", &native]);
    assert_eq!(commit((&owner(), svn), replacing, None).await, answered(4));
    let after = (
      on(|state| green_head(state, green)),
      on(|state| green_file(state, green, "branches/b/a.txt")),
      on(|state| green_props(state, green, "branches/b/a.txt")),
    );
    let expected = (
      Some(4),
      Some(b"gamma\n".to_vec()),
      Some(identity(&native).to_vec()),
    );
    assert_eq!(after, expected);
  });
}

/// alice's commit's locks: no token presented.
fn alices() -> CommitLocks {
  CommitLocks {
    user: Some("alice".to_owned()),
    ..CommitLocks::default()
  }
}

/// A-655 part 2b (`tree.c` `svn_fs_fs__commit_txn` 2255-2310: FSFS merges a transaction whose base moved again; a
/// Moved is not "out of date"). Do: commit revision 1; then three works judged at revision 1, through the step as a
/// client's: one changing `/trunk/a.txt`, one changing `/trunk/sub/b.txt`, one changing `/trunk/a.txt` again. Expect:
/// the first is revision 2; the second, judged again at revision 2, lands as revision 3, the green holding both
/// changes; the third, judged again at revision 3, refused as the drive's verdict refuses it, "Conflict at
/// '/trunk/a.txt'" (`E160024`), the green, the records, the register and the charge as they were.
#[test]
fn t_a655_a_moved_work_is_judged_again_and_lands_unless_it_conflicts() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    let locks = alices();
    let judged = Some((&locks, AprTime(1)));
    let edit =
      |path: &str, bytes: &[u8]| revision(1, vec![file(path, Action::Modify, bytes)], &[bytes]);
    let one = edit("/trunk/a.txt", b"a 2\n");
    let two = edit("/trunk/sub/b.txt", b"b 2\n");
    let three = edit("/trunk/a.txt", b"a 3\n");
    assert_eq!(commit((&owner(), svn), one, judged).await, answered(2));
    assert_eq!(commit((&owner(), svn), two, judged).await, answered(3));
    let both = (
      on(|state| green_head(state, green)),
      on(|state| green_file(state, green, "trunk/a.txt")),
      on(|state| green_file(state, green, "trunk/sub/b.txt")),
    );
    assert_eq!(
      both,
      (Some(3), Some(b"a 2\n".to_vec()), Some(b"b 2\n".to_vec()))
    );
    let before = on(|state| whole(state, (svn, green)));
    let refused = commit((&owner(), svn), three, judged).await;
    let conflict = CommitRefusal::Conflict {
      path: "/trunk/a.txt".to_owned(),
      kind: ConflictKind::FileChanged,
    };
    let step = StepRefusal::Verdict {
      refusal: conflict,
      user: Some("alice".to_owned()),
    };
    let words = (step.subversion_code(), step.message());
    assert_eq!(words, (160_024, "Conflict at '/trunk/a.txt'".to_owned()));
    assert_eq!(refused, Err(Uncommitted::Step(step)));
    assert_eq!(on(|state| whole(state, (svn, green))), before);
  });
}

/// A-655 part 2b (a refusal leaves everything as it was). Do: commit revision 1; then, as an import's, a revision made
/// on revision 0; one naming a text it does not bring; one by a principal not the owner; and one deleting a directory
/// beneath a copy the same revision makes of revision 1, after revision 2 moved the head. Expect: refused "Transaction
/// out of date", "Filesystem is corrupt", `Forbidden` by the append with the growth's charge given back, and
/// `Unsupported` naming the directory (counted); after each, the green, the records, the register and the charge as
/// they were.
#[test]
fn t_a655_a_refused_revision_changes_nothing() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    let mut unbrought = revision(1, vec![file("/trunk/c.txt", Action::Add, b"c\n")], &[]);
    unbrought.contents.clear();
    let stranger = Principal::Uid {
      host: slates_db::HostId(0),
      uid: 4321,
    };
    let cases = [
      (
        owner(),
        revision(
          0,
          vec![file("/trunk/a.txt", Action::Modify, b"x\n")],
          &[b"x\n"],
        ),
        Uncommitted::Step(StepRefusal::OutOfDate {
          head: 0,
          youngest: 1,
        }),
      ),
      (owner(), unbrought, Uncommitted::Step(StepRefusal::Corrupt)),
      (
        stranger,
        revision(
          1,
          vec![file("/trunk/a.txt", Action::Modify, b"y\n")],
          &[b"y\n"],
        ),
        Uncommitted::Append(AppendRefused::Repository(Refusal::Forbidden {
          verb: "push".to_owned(),
        })),
      ),
    ];
    for (principal, refused, why) in cases {
      let before = on(|state| whole(state, (svn, green)));
      assert_eq!(commit((&principal, svn), refused, None).await, Err(why));
      assert_eq!(on(|state| whole(state, (svn, green))), before);
    }
    let edit = revision(
      1,
      vec![file("/trunk/a.txt", Action::Modify, b"a 2\n")],
      &[b"a 2\n"],
    );
    assert_eq!(commit((&owner(), svn), edit, None).await, answered(2));
    let mut older = branched();
    older.head = 2;
    let before = on(|state| whole(state, (svn, green)));
    let counted = on(|state| state.refusals.get(UNSUPPORTED).copied().unwrap_or(0));
    let unsupported = Uncommitted::Unsupported {
      path: "/branches/b/sub".to_owned(),
    };
    assert_eq!(commit((&owner(), svn), older, None).await, Err(unsupported));
    assert_eq!(on(|state| whole(state, (svn, green))), before);
    assert_eq!(
      on(|state| state.refusals.get(UNSUPPORTED).copied()),
      Some(counted + 1)
    );
  });
}

/// A-655 part 2b (`vcs_store`: a recovery's replay of the records through the step's check and apply). Do: commit
/// revisions 1 to 3 (a branch made by copy, a deletion), then make the store again from the changesets the register names
/// on the node's store (C2). Expect:
/// the records made again equal the live ones whole, as they print; every record replayed, the creation's among them;
/// none refused; the shard charged what they hold.
#[test]
fn t_a655_the_records_are_made_again_from_the_log() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    assert_eq!(commit((&owner(), svn), branched(), None).await, answered(2));
    let deleted = revision(
      2,
      vec![row("/trunk", Action::Delete, NodeKind::Directory)],
      &[],
    );
    assert_eq!(commit((&owner(), svn), deleted, None).await, answered(3));
    let live = on(|state| whole(state, (svn, green)));
    let replayed = on(|state| {
      state
        .refusals
        .get(crate::vcs_store::REPLAYED)
        .copied()
        .unwrap_or(0)
    });
    rebuild(svn).await;
    assert_eq!(on(|state| state.vcs_stores.unrecovered(svn).cloned()), None);
    assert_eq!(on(|state| whole(state, (svn, green))), live);
    let count = u64::try_from(on(|state| logged(state, svn)).len()).unwrap();
    assert_eq!(
      on(|state| state.refusals.get(crate::vcs_store::REPLAYED).copied()),
      Some(replayed + count)
    );
    let (_, held, charged) = on(|state| kept(state, svn));
    assert_eq!((count, charged), (4, held));
  });
}

/// A-698 (`land_work`: what both ra_svn transports call with the work their connection accepted). Do: land a client's
/// accepted work adding `/trunk`, by alice, presenting no lock, at [`DATE`]'s instant. Expect: revision 1, dated by that
/// instant and by alice, as `commit-info` carries them; the green at version 1 holding `/trunk`; changeset 1 on the
/// node's store the revision the step made.
#[test]
fn t_a698_a_clients_accepted_work_lands_through_the_step() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    let work = Work {
      head: 0,
      record: RevisionRecord {
        transaction: "0-1".to_owned(),
        changes: vec![row("/trunk", Action::Add, NodeKind::Directory)],
      },
      lock_entries: Vec::new(),
      revision_properties: vec![
        (b"svn:author".to_vec(), b"alice".to_vec()),
        (b"svn:log".to_vec(), b"a change".to_vec()),
      ],
      texts: std::collections::BTreeSet::new(),
      property_lists: BTreeMap::new(),
      locks: CommitLocks {
        user: Some("alice".to_owned()),
        ..CommitLocks::default()
      },
    };
    // The revision made of the work at the owner's instant and judged again with its locks then, as `land_work` makes
    // it, at [`DATE`]'s instant.
    let locks = work.locks.clone();
    let of_work = Revision::of_work(work, DATE_INSTANT).unwrap();
    let landed = commit((&owner(), svn), of_work, Some((&locks, DATE_INSTANT))).await;
    let made = revision(
      0,
      vec![row("/trunk", Action::Add, NodeKind::Directory)],
      &[],
    );
    let seen = (
      landed,
      on(|state| green_head(state, green)),
      on(|state| green_dir(state, green, "trunk")),
    );
    assert_eq!(seen, (answered(1), Some(1), true));
    assert_eq!(stored_record(svn, 1).await, Record::Revision(made));
  });
}

/// Shape: the most revisions the folding test lands before it expects a checkpoint: far more than the doubling rule
/// needs for increments of a few hundred bytes against revision 1's checkpoint, and few enough to run at once.
const FOLD_WITHIN: u64 = 256;

/// Found by use (w-perf, 2026-11-19: `slates repository import` of git/git's newest 5,000 commits as a dump refused
/// `TableAtCapacity { table: "green_chains" }` after 1,685 revisions). Do: land revision 1, then revisions changing
/// `/trunk/a.txt`, one at a time, no reader holding any version. Expect: the repository's green is checkpointed as
/// they land (its checkpoint floor rises past 0), as the merge service folds every other green's chain by its doubling
/// rule, so a Subversion repository's chain stays bounded however many revisions it takes. Before the fix no
/// Subversion landing checkpointed its green, so every revision's increment stayed in the partition's chain until the
/// cap refused the next.
#[test]
fn subversion_revisions_fold_their_greens_chain_as_they_land() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    let mut head = 1;
    while on(|state| state.db.partition().green_floor(green)) == 0 && head < FOLD_WITHIN {
      let bytes = format!("alpha {head}\n").into_bytes();
      let rows = vec![file("/trunk/a.txt", Action::Modify, &bytes)];
      let next = revision(head, rows, &[&bytes]);
      assert_eq!(
        commit((&owner(), svn), next, None).await,
        answered(head + 1)
      );
      head += 1;
    }
    assert!(
      on(|state| state.db.partition().green_floor(green)) > 0,
      "no checkpoint after {head} revisions"
    );
  });
}

/// Shape: the partition's merge budget in the content test: small, so a few revisions' records pass it.
const SMALL_MERGE_BUDGET: usize = 64 << 10;

/// Shape: each revision's text in the content test: a quarter of [`SMALL_MERGE_BUDGET`].
const QUARTER_TEXT: usize = SMALL_MERGE_BUDGET / 4;

/// Found by use (w-perf, 2026-11-19: `slates repository import` of git/git's newest 5,000 commits as a dump refused
/// `TableAtCapacity { table: "green_chains" }` at revision 1,685). Do: on a shard whose partition's merge budget is
/// [`SMALL_MERGE_BUDGET`], land revision 1, then eight revisions each bringing a text a quarter of that budget. Expect:
/// every revision lands, bringing more than the merge budget, while the register holds a reference a changeset alone,
/// far below it, charged to the shard's budget (A-662, `vcs_store::settle_objects`): a repository's content is
/// the node's changeset store's and its shard's (C2), never the merge budget the partition's chains share; and the
/// chains never ran short of room, so no revision checkpointed its green first (A-742's counter unmoved): since chain
/// entry format 6 (lane (A)) an entry names its revision's increment, which the owner shard's content store holds and
/// its room charges, so the chains hold a few hundred bytes a revision. Before A-662 each revision's record was charged
/// to the merge budget, so the repository's whole content had to fit in it; before format 6 the entry carried the
/// increment and the chains filled within a few revisions.
#[test]
fn a_repositorys_content_is_bounded_by_its_shard_not_the_partitions_merge_budget() {
  let landed = stored_configured(
    || async move {
      let (svn, _green) = subversion("svn").await;
      let mut answers = vec![commit((&owner(), svn), first(), None).await];
      for head in 1..9u64 {
        let bytes = vec![u8::try_from(head).unwrap_or(0); QUARTER_TEXT];
        let rows = vec![file("/trunk/a.txt", Action::Modify, &bytes)];
        answers.push(commit((&owner(), svn), revision(head, rows, &[&bytes]), None).await);
      }
      on(|state| {
        let referenced = state.db.partition().repositories().objects_bytes(svn);
        let charged = state.vcs_stores.objects_charged(svn);
        let checkpointed_first = state
          .refusals
          .get(super::CHAIN_ROOM_CHECKPOINTED)
          .copied()
          .unwrap_or(0);
        (
          answers,
          referenced < SMALL_MERGE_BUDGET,
          charged == u64::try_from(referenced).unwrap_or(0),
          checkpointed_first > 0,
        )
      })
    },
    |config| config.caps.green_chain_bytes = SMALL_MERGE_BUDGET,
  );
  let expected: Vec<_> = (1..10).map(answered).collect();
  assert_eq!(landed, (expected, true, true, false));
}

/// Found by use (w-perf, 2026-11-19, release b705a24c5: a 5,000-revision import's record 3,560 refused `Unpublished`,
/// "profile … exceeds its region", then every `svn commit` refused E160004 "The repository's green is not at its
/// youngest revision"). Do: land revision 1; with the next publication refused before its append, commit revision 2;
/// then commit revision 2 again. Expect: the first attempt refused, the green's engine still at revision 1, where the
/// durable chain and the records are; the second attempt lands as revision 2. Before the fix the engine had committed
/// the version before its record was refused, and kept it: the green moved past the records for good.
#[test]
fn a_revision_refused_at_publication_leaves_the_green_where_the_log_is_and_the_next_lands() {
  let seen = stored(|| async move {
    let (svn, green) = subversion("svn").await;
    let first_landed = commit((&owner(), svn), first(), None).await;
    let second = || {
      let rows = vec![file("/trunk/a.txt", Action::Modify, b"two\n")];
      revision(1, rows, &[b"two\n"])
    };
    on(|state| {
      state
        .db
        .inject_publication_fault(Some(slates_db::replay::PublicationFault::BeforeAppend));
    });
    let refused = commit((&owner(), svn), second(), None).await.is_err();
    let head_after = on(|state| green_head(state, green));
    let again = commit((&owner(), svn), second(), None).await;
    (first_landed, refused, head_after, again)
  });
  assert_eq!(seen, (answered(1), true, Some(1), answered(2)));
}

/// The head as the lock guard reads it, over the records' index.
struct IndexHead<'i>(&'i slates_svn::index::Index);

impl slates_svn::lock::HeadView for IndexHead<'_> {
  fn youngest(&self) -> u64 {
    self.0.youngest()
  }

  fn node(&self, path: &str) -> Option<(NodeKind, u64)> {
    let node = self.0.node(path, self.0.youngest()).ok()??;
    Some((node.kind(), node.created_revision()))
  }
}

/// A-693 part 2 (a lock verb's entries through the owner's step). Do: land revision 1; judge alice's lock of
/// `/trunk/a.txt` against the records' table, as the ra_svn connection judges `lock-many`, and land its entries as a
/// `Record::Locks`; then make the repository's store again from its changesets. Expect: the step lands the entries; the
/// records hold alice's lock, the last changeset the register names is the `locks` record on the node's store; the
/// store made again holds the same lock, so a lock is as durable as a revision.
#[test]
fn a_lock_verbs_entries_land_through_the_step_and_a_rebuild_keeps_them() {
  let seen = stored(|| async move {
    let (svn, _green) = subversion("svn").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    let request = slates_svn::lock::LockRequest {
      user: Some("alice".to_owned()),
      comment: Some(b"held".to_vec()),
      targets: BTreeMap::from([(
        "/trunk/a.txt".to_owned(),
        slates_svn::lock::LockTarget::default(),
      )]),
      ..slates_svn::lock::LockRequest::default()
    };
    let decision = on(|state| {
      let records = state.vcs_stores.subversion_records(svn).unwrap();
      records
        .locks()
        .lock(
          &request,
          &IndexHead(records.index()),
          AprTime(1),
          8,
          &mut || [7; 16],
        )
        .unwrap()
    });
    let landed = super::land_record((&owner(), svn), Record::Locks(decision.entries))
      .await
      .expect("the shard's state");
    let owner_of = |state: &ShardState| {
      state
        .vcs_stores
        .subversion_records(svn)
        .and_then(|records| {
          records
            .locks()
            .get_lock("/trunk/a.txt", AprTime(1))
            .map(|lock| lock.owner.clone())
        })
    };
    let held = on(|state| owner_of(state));
    let named = on(|state| logged(state, svn)).len();
    let last = stored_record(svn, u64::try_from(named).unwrap() - 1).await;
    rebuild(svn).await;
    (
      landed,
      held,
      Some(matches!(last, Record::Locks(_))),
      on(|state| owner_of(state)),
    )
  });
  let alice = Some("alice".to_owned());
  assert_eq!(seen, (Ok(()), alice.clone(), Some(true), alice));
}

/// A-664's Subversion objects (bug, found by reading: a version a work submits to a Subversion repository's green made
/// no revision, so the green passed its records' youngest revision and every later commit was refused). Do: make a
/// Subversion repository; submit a work making `/trunk/a.txt` to its green; then commit a client's revision on the
/// youngest revision adding `/trunk/b.txt`. Expect: the submit made revision 1, its rows adding `/trunk` and
/// `/trunk/a.txt` with the file's text, its author the session's Subversion name; the client's revision lands as
/// revision 2, the green at version 2 holding both files.
#[test]
fn t_a664_svn_a_submitted_version_is_the_revision_the_server_makes() {
  stored(|| async move {
    let (svn, green) = subversion("submitted").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let made = crate::vcs_store::paths::tests::create("/trunk/a.txt", b"alpha\n");
    let version =
      on(|state| crate::vcs_store::paths::tests::version(state, ipc_green, 1, vec![made]));
    assert_eq!(version, 1);
    youngest_reaches(svn, 1).await;
    let rows: Vec<(String, Action, NodeKind, bool)> = on(|state| {
      state
        .vcs_stores
        .subversion_records(svn)
        .and_then(|records| records.index().changes(1).ok().map(<[ChangedPath]>::to_vec))
    })
    .unwrap_or_default()
    .into_iter()
    .map(|row| {
      (
        row.path,
        row.action,
        row.kind,
        row.text == Some(real_text(b"alpha\n")),
      )
    })
    .collect();
    let made = vec![
      ("/trunk".to_owned(), Action::Add, NodeKind::Directory, false),
      ("/trunk/a.txt".to_owned(), Action::Add, NodeKind::File, true),
    ];
    assert_eq!(rows, made);
    let author = on(|state| {
      state
        .vcs_stores
        .subversion_records(svn)
        .and_then(|records| records.revision_properties(1))
        .and_then(|properties| properties.get(b"svn:author".as_slice()).cloned())
    });
    assert_eq!(
      author,
      Some(b"a-u-thor".to_vec()),
      "the principal's own name"
    );
    let rows = vec![file("/trunk/b.txt", Action::Add, b"beta\n")];
    let committed = commit((&owner(), svn), revision(1, rows, &[b"beta\n"]), None).await;
    assert_eq!(committed, answered(2));
    let seen = (
      on(|state| green_head(state, green)),
      on(|state| green_file(state, green, "trunk/a.txt")),
      on(|state| green_file(state, green, "trunk/b.txt")),
    );
    assert_eq!(
      seen,
      (Some(2), Some(b"alpha\n".to_vec()), Some(b"beta\n".to_vec()))
    );
  });
}

/// `bytes` as the server's own revision names its text, with its real digests.
fn real_text(bytes: &[u8]) -> Text {
  slates_svn::commit::drive::text_of(bytes).expect("the digests")
}

/// A-664's Subversion objects (symbolic links, main's ruling: as svnserve keeps them). Do: make a Subversion repository;
/// submit a work making `/trunk/a.txt` and a symbolic link `/trunk/l` to `a.txt`. Expect: revision 1 adds `/trunk/l`
/// as a file whose text is `link a.txt` and whose property list is `svn:special` set to `*` alone, which the records
/// hold.
#[test]
fn t_a664_svn_a_submitted_symbolic_link_is_a_special_file() {
  stored(|| async move {
    let (svn, green) = subversion("linked").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let link = crate::vcs_store::paths::tests::Op::Declare(slates_ipc::protocol::WorkOp::Symlink {
      path: "/trunk/l".to_owned(),
      target: "a.txt".to_owned(),
    });
    let made = crate::vcs_store::paths::tests::create("/trunk/a.txt", b"alpha\n");
    on(|state| crate::vcs_store::paths::tests::version(state, ipc_green, 1, vec![made, link]));
    youngest_reaches(svn, 1).await;
    let special = b"K 11\nsvn:special\nV 1\n*\nEND\n";
    let (row, kept_special) = on(|state| {
      let records = state
        .vcs_stores
        .subversion_records(svn)
        .expect("the records");
      let row = records
        .index()
        .changes(1)
        .ok()
        .and_then(|rows| rows.iter().find(|row| row.path == "/trunk/l").cloned());
      (
        row,
        records
          .text(&identity(special))
          .map(std::borrow::Cow::into_owned),
      )
    });
    let seen = row.map(|row| (row.action, row.kind, row.text, row.properties));
    let expected = (
      Action::Add,
      NodeKind::File,
      Some(real_text(b"link a.txt")),
      Some(held(special)),
    );
    assert_eq!(seen, Some(expected));
    assert_eq!(kept_special.as_deref(), Some(&special[..]));
  });
}

/// A-664's Subversion objects (a copy; main's ruling: a work's copy as Subversion's copy-from row). Do: make a Subversion
/// repository; submit a work making `/trunk/a.txt` and the directory `/branches`; submit a work copying `/trunk` to
/// `/branches/b` from its base; then commit a client's revision on the youngest adding `/branches/b/c.txt`. Expect:
/// revision 2's one row adds `/branches/b`, a directory, copied from `/trunk` at revision 1; the records hold
/// `/branches/b/a.txt` at revision 2; the client's revision lands as revision 3.
#[test]
fn t_a664_svn_a_submitted_copy_is_a_copy_from_row() {
  stored(|| async move {
    let (svn, green) = subversion("copied").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let first = vec![
      crate::vcs_store::paths::tests::create("/trunk/a.txt", b"alpha\n"),
      crate::vcs_store::paths::tests::Op::Declare(slates_ipc::protocol::WorkOp::Mkdir {
        path: "/branches".to_owned(),
      }),
    ];
    on(|state| crate::vcs_store::paths::tests::version(state, ipc_green, 1, first));
    youngest_reaches(svn, 1).await;
    let copy = crate::vcs_store::paths::tests::Op::Declare(slates_ipc::protocol::WorkOp::Copy {
      from: "/trunk".to_owned(),
      to: "/branches/b".to_owned(),
      version: None,
      replace: false,
    });
    assert_eq!(
      on(|state| crate::vcs_store::paths::tests::version(state, ipc_green, 2, vec![copy])),
      2
    );
    youngest_reaches(svn, 2).await;
    let held = on(|state| {
      let records = state
        .vcs_stores
        .subversion_records(svn)
        .expect("the records");
      let rows: Vec<(String, Action, NodeKind, Option<CopySource>)> = records
        .index()
        .changes(2)
        .map(|rows| {
          rows
            .iter()
            .map(|row| {
              (
                row.path.clone(),
                row.action,
                row.kind,
                row.copy_from.clone(),
              )
            })
            .collect()
        })
        .unwrap_or_default();
      let copied = CopySource {
        path: "/trunk".to_owned(),
        revision: 1,
      };
      assert_eq!(
        rows,
        vec![(
          "/branches/b".to_owned(),
          Action::Add,
          NodeKind::Directory,
          Some(copied)
        )]
      );
      records
        .index()
        .node("/branches/b/a.txt", 2)
        .ok()
        .flatten()
        .is_some()
    });
    assert!(held, "the copy brings /trunk/a.txt");
    let rows = vec![file("/branches/b/c.txt", Action::Add, b"gamma\n")];
    let committed = commit((&owner(), svn), revision(2, rows, &[b"gamma\n"]), None).await;
    assert_eq!(committed, answered(3));
  });
}

/// What a fetch's hold on `svn`'s records reads: their youngest revision and `/trunk/a.txt`'s text; none when they are
/// gone.
fn read_held(
  state: &mut ShardState,
  hold: &crate::vcs_store::svn_view::SvnHold,
) -> Option<(u64, Option<Vec<u8>>)> {
  crate::vcs_store::svn_view::lend_held_in(state, hold, |lent| {
    let text = slates_svn::reads::Repository::text(lent.repository, &identity(b"alpha\n"), 0)
      .map(std::borrow::Cow::into_owned);
    (lent.index.youngest(), text)
  })
}

/// A-749 rule 4 (§4.17.1 "A fetch's answer": fetches run over the index generation they began on), for Subversion's
/// records. Do: commit revision 1; hold the records as a fetch's answer does when it begins; make the store again from
/// its changesets (a rebuild: the store freed, made empty and lent to nothing until its records are read back from the
/// node's store and replayed, a turn each);
/// read through the hold while it is lent to nothing and again once it is whole; give the hold back. Expect: the hold
/// reads revision 1 and `/trunk/a.txt`'s text throughout; the records kept for it charged until it is given back, and
/// then the shard charged for the store made again alone, as before the rebuild; a hold taken after reads the store made
/// again. Red before the change: the hold read nothing once the store was freed.
#[test]
fn t_a745_a_fetchs_hold_reads_the_records_it_began_on_across_a_rebuild() {
  stored(|| async move {
    let (svn, _) = subversion("held").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    let (_, _, charged) = on(|state| kept(state, svn));
    let retained = on(|state| state.store.budget.retained());
    let hold =
      on(|state| crate::vcs_store::svn_view::hold_in(state, svn)).expect("the records held");
    let (lent_to_nothing, during) = on(|state| {
      crate::vcs_store::rebuild(state, svn);
      (
        state.vcs_stores.subversion_records(svn).is_none(),
        read_held(state, &hold),
      )
    });
    crate::vcs_store::rebuilt(svn).await;
    let made_again = on(|state| kept(state, svn));
    let both = on(|state| state.store.budget.retained());
    let after = on(|state| read_held(state, &hold));
    on(|state| hold.give_back(state));
    let given_back = on(|state| state.store.budget.retained());
    let again = on(|state| crate::vcs_store::svn_view::hold_in(state, svn))
      .expect("the store made again held");
    let fresh = on(|state| read_held(state, &again));
    on(|state| again.give_back(state));
    let alpha = Some((1, Some(b"alpha\n".to_vec())));
    assert_eq!(
      (lent_to_nothing, during, after, fresh),
      (true, alpha.clone(), alpha.clone(), alpha)
    );
    assert_eq!(made_again.0, 1);
    assert_eq!(
      (both, given_back),
      (
        retained.saturating_add(made_again.2),
        retained
          .saturating_sub(charged)
          .saturating_add(made_again.2)
      )
    );
  });
}

/// `svn`'s head texts read through its sources' text source: the replacing text borrowed from `green`, an earlier one
/// still read from the records.
fn assert_read_through_the_green(state: &ShardState, (svn, green): (RepositoryId, DbVolumeId)) {
  let engine = state.greens.get(&green).expect("the green");
  let records = state
    .vcs_stores
    .subversion_records(svn)
    .expect("the records");
  let texts = crate::vcs_store::svn_texts::HeadTexts {
    store: None,
    green: Some(&engine),
    records,
    pieces: None,
  };
  use slates_svn::migration::TextSource;
  assert!(matches!(
    texts.text(&identity(b"gamma\n")),
    Some(std::borrow::Cow::Borrowed(b"gamma\n"))
  ));
  // An earlier text is its changeset's on the node's store: the records name it, RAM holds none of it (C2 stage B
  // part 2 (b)), and a reader reads it through the store.
  let alpha = identity(b"alpha\n");
  assert_eq!(
    (texts.text(&alpha).is_none(), texts.fetchable(&alpha, 0)),
    (true, false),
    "a view without the store's pieces reads nothing of an earlier text"
  );
}

/// S4 slice 2 c1 (the head's texts held by the green, `vcs_store::svn_texts`). Do: commit revision 1 (`/trunk/a.txt`,
/// `/trunk/sub/b.txt`); 2, `branched` (a copy of `/trunk` with `a.txt` changed beneath it and `sub` deleted); 3,
/// deleting `/trunk`; 4, replacing `/branches/b/a.txt` by another text; reading each text through the sources' text
/// source after each. Expect: after 1 the green holds both texts; after 2 those of `trunk` and the branch's changed
/// `a.txt`, each once though two rows reach it; after 3 the branch's alone; after 4 only the replacing text; each held
/// text read from the green, borrowed, and an earlier text nowhere in RAM (its changeset's on the node's store).
#[test]
fn t_s4_c1_the_heads_texts_are_held_by_the_green_and_read_through_it() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    let held_by_green = |state: &mut ShardState| {
      let engine = state.greens.get(&green).expect("the green");
      let texts: [&[u8]; 5] = [b"alpha\n", b"beta\n", b"alpha 2\n", b"gamma\n", b"never\n"];
      let each: Vec<bool> = texts
        .iter()
        .map(|bytes| engine.held_bytes(&identity(bytes)) == Some(*bytes))
        .collect();
      (engine.held_count(), each)
    };
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    assert_eq!(
      on(|state| held_by_green(state)),
      (2, vec![true, true, false, false, false])
    );
    assert_eq!(commit((&owner(), svn), branched(), None).await, answered(2));
    assert_eq!(
      on(|state| held_by_green(state)),
      (3, vec![true, true, true, false, false])
    );
    let deleted = revision(
      2,
      vec![row("/trunk", Action::Delete, NodeKind::Directory)],
      &[],
    );
    assert_eq!(commit((&owner(), svn), deleted, None).await, answered(3));
    assert_eq!(
      on(|state| held_by_green(state)),
      (1, vec![false, false, true, false, false])
    );
    let replacing = revision(
      3,
      vec![file("/branches/b/a.txt", Action::Replace, b"gamma\n")],
      &[b"gamma\n"],
    );
    assert_eq!(commit((&owner(), svn), replacing, None).await, answered(4));
    assert_eq!(
      on(|state| held_by_green(state)),
      (1, vec![false, false, false, true, false])
    );
    on(|state| assert_read_through_the_green(state, (svn, green)));
  });
}

/// S4 slice 2 c1 (a recovery's green holds the head's texts again, `svn_texts::rehold_head`). Do: commit revision 1 and
/// `branched`; let go of every text the green holds, as a green a recovery made again holds none; hold the head again,
/// twice. Expect: the head's three texts held again, each once, as the commits had left them; the second call holding
/// nothing more, since the green holds texts already.
#[test]
fn t_s4_c1_a_green_made_again_holds_its_heads_texts_again_in_one_order() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    assert_eq!(commit((&owner(), svn), branched(), None).await, answered(2));
    let texts: [&[u8]; 3] = [b"alpha\n", b"beta\n", b"alpha 2\n"];
    let seen = on(|state| {
      let mut engine = state.greens.get_mut(&green).expect("the green");
      // Every holder of every text let go, as a green a recovery made again holds none.
      while engine.held_count() > 0 {
        for bytes in texts {
          let _ = engine.release_held(&identity(bytes));
        }
      }
      drop(engine);
      crate::vcs_store::svn_texts::rehold_head(state, svn, green);
      crate::vcs_store::svn_texts::rehold_head(state, svn, green);
      let engine = state.greens.get(&green).expect("the green");
      let held: Vec<bool> = texts
        .iter()
        .map(|bytes| engine.held_bytes(&identity(bytes)) == Some(*bytes))
        .collect();
      (engine.held_count(), held)
    });
    assert_eq!(seen, (3, vec![true, true, true]));
  });
}

/// R4 of Ada's close list (a recovery replayed a revision's record whole, its every text deltified in one step, so one
/// large revision held its shard), under C2 stage B part 2 (b). Do: commit a revision adding `/trunk` and twelve files of
/// 50 KiB distinct texts, then rebuild the repository from its changesets on the node's store, its turns driven to the
/// end. Expect: the same records and green; the twelve texts named stored by the records, none kept in RAM, so the
/// replay makes no text and takes at least one turn (`vcs_store.svn_replay_turns`). Before part 2 (b) the replay
/// deltified each text, a turn a file at least.
#[test]
fn a_revisions_texts_replay_as_stored_keeping_nothing_in_ram() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    let texts: Vec<Vec<u8>> = (0..12u32)
      .map(|at| {
        (0..50 * 1024u32)
          .map(|byte| ((byte * 31 + at * 7) % 251) as u8)
          .collect()
      })
      .collect();
    let mut rows = vec![row("/trunk", Action::Add, NodeKind::Directory)];
    for (at, bytes) in texts.iter().enumerate() {
      rows.push(file(&format!("/trunk/f{at:02}.bin"), Action::Add, bytes));
    }
    let contents: Vec<&[u8]> = texts.iter().map(Vec::as_slice).collect();
    assert_eq!(
      commit((&owner(), svn), revision(0, rows, &contents), None).await,
      answered(1)
    );
    let live = on(|state| whole(state, (svn, green)));
    let turns = |state: &ShardState| {
      state
        .refusals
        .get(crate::vcs_store::SVN_REPLAY_TURNS)
        .copied()
        .unwrap_or(0)
    };
    let before = on(|state| turns(state));
    rebuild(svn).await;
    assert_eq!(on(|state| state.vcs_stores.unrecovered(svn).cloned()), None);
    assert_eq!(on(|state| whole(state, (svn, green))), live);
    let taken = on(|state| turns(state)) - before;
    assert!(taken >= 1, "the revision replayed in {taken} turns");
    let stored = on(|state| {
      let records = state
        .vcs_stores
        .subversion_records(svn)
        .expect("the records");
      texts
        .iter()
        .filter(|bytes| {
          let named = identity(bytes);
          records.stored_len(&named).is_some() && records.text(&named).is_none()
        })
        .count()
    });
    assert_eq!(stored, 12, "a text kept in RAM");
  });
}

/// C2 stage B part 2 (b), P1 (a commit's green increment reads a stored text the content store let go through the
/// node's changeset store). Do: on a daemon whose revision 1 adds `/trunk/a.bin`, every hold RAM has on its text let go
/// (`crate::test_store::a_text_only_the_store_holds`), commit revision 2 adding `/trunk/b.bin` with the same text, as a
/// copy or a revert names one the records hold already. Expect revision 2 committed, the green's `trunk/b.bin` the text
/// whole, read through the store (`vcs_store.svn_texts.piece_fetched` at least once), and the content store holding
/// nothing of it after (the commit gave back what it brought). Before, the increment found no bytes and the commit was
/// refused `Corrupt`.
#[test]
fn a_commit_naming_a_stored_text_ram_let_go_brings_it_back_for_the_greens_increment() {
  let bytes = crate::test_store::noise(200 << 10);
  let (_store, daemon, id) =
    crate::test_store::a_text_only_the_store_holds(&owner(), "copied", &bytes);
  let named = text(&bytes);
  let landed = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let work = Work {
        head: 1,
        record: RevisionRecord {
          transaction: "1-2".to_owned(),
          changes: vec![ChangedPath {
            path: "/trunk/b.bin".to_owned(),
            action: Action::Add,
            kind: NodeKind::File,
            copy_from: None,
            text: Some(named),
            properties: None,
            mergeinfo_modified: false,
          }],
        },
        lock_entries: Vec::new(),
        revision_properties: vec![(b"svn:log".to_vec(), b"the same".to_vec())],
        texts: std::iter::once(named.identity).collect(),
        property_lists: BTreeMap::new(),
        locks: CommitLocks::default(),
      };
      let sink = slates_svn::commit::drive::HeldTexts::new();
      super::land_work((&owner(), id), work, &sink)
        .await
        .map(|landed| landed.map(|committed| committed.revision))
    },
    std::time::Duration::from_secs(60),
  );
  let (file, kept, fetched) = crate::daemon::observe_first(&daemon, move |state| {
    let green = super::green_of(state, id).ok();
    let file = green.and_then(|green| green_file(state, green, "trunk/b.bin"));
    let kept = state
      .vcs_stores
      .content()
      .is_some_and(|content| content.contains(&slates_vcs::ContentId(named.identity)));
    let fetched = state
      .refusals
      .get(crate::vcs_store::svn_texts::PIECE_FETCHED)
      .copied()
      .unwrap_or(0);
    (file, kept, fetched)
  });
  daemon.stop();
  assert!(matches!(landed, Some(Ok(2))), "{landed:?}");
  assert!(
    file.as_deref() == Some(bytes.as_slice()),
    "the green's file is not the text"
  );
  assert!(fetched >= 1, "no piece read through the store");
  let pieces = u64::try_from(bytes.len().div_ceil(slates_store::store::PIECE_BYTES)).unwrap();
  assert!(
    fetched <= pieces,
    "{fetched} pieces fetched through the store for a text of {pieces}"
  );
  assert!(!kept, "the content store kept what the commit brought");
}

/// A work over the Subversion repository's green `green` writing `bytes` at `path`, made by bare dispatches: its id.
fn made_work(
  state: &mut ShardState,
  green: slates_ipc::protocol::VolumeId,
  (name, path, bytes): (&str, &str, &[u8]),
) -> slates_ipc::protocol::VolumeId {
  let made = crate::vcs_store::paths::tests::ask(
    state,
    slates_ipc::protocol::RequestBody::CreateWork {
      green,
      name: name.to_owned(),
    },
  );
  let slates_ipc::protocol::ReplyBody::WorkCreated { id: work, .. } = made else {
    panic!("no work: {made:?}")
  };
  let edited = crate::vcs_store::paths::tests::ask(
    state,
    slates_ipc::protocol::RequestBody::Edit {
      work,
      path: path.to_owned(),
      at: 0,
      delete_len: 0,
      bytes: bytes.to_vec(),
    },
  );
  assert!(
    !matches!(edited, slates_ipc::protocol::ReplyBody::Refused { .. }),
    "{edited:?}"
  );
  work
}

/// Whether request `id` of `origin` is completed: answered, its answer recorded.
fn completed(state: &ShardState, origin: u64, id: slates_wire::request::RequestId) -> bool {
  matches!(
    state
      .db
      .partition()
      .completion(origin, id.client, id.sequence),
    slates_wire::request::Seen::Completed(_)
  )
}

/// Main's landing order for a server-made commit (inbox w-svn-from-main-c2-made: check, the store's land, the register's
/// commit, the answer). Do: submit a work to a Subversion repository's green as a client's recorded request; read its
/// completion, the records' youngest revision and the green's durable head in the verb's own step; then wait for the
/// revision. Expect: in the verb's step nothing is answered and nothing durable (the revision not landed); once it
/// landed, the request answered, the green's durable head 1, the register naming the creation and the revision, and the
/// made revision's text named stored by the records, none of it in RAM (its changeset carries it, C2 stage B part 2 (b)
/// P4b). Until the deferred-submit hook, the verb answered and made the version durable before the revision landed.
#[test]
fn a_server_made_commit_is_answered_only_once_its_revision_landed_and_committed() {
  stored(|| async move {
    let (svn, green) = subversion("deferred").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let work = on(|state| {
      made_work(
        state,
        ipc_green,
        ("deferred-work", "trunk/a.txt", b"alpha\n"),
      )
    });
    let id = slates_wire::request::RequestId {
      client: 7,
      sequence: 1,
    };
    let in_the_verb = on(|state| {
      crate::verbs::run_recorded_for_test(
        state,
        (7, id),
        &owner(),
        crate::identities::authored(work, "made"),
      );
      (
        completed(state, 7, id),
        kept(state, svn).0,
        state.db.partition().green_head(green),
      )
    });
    youngest_reaches(svn, 1).await;
    let after = on(|state| {
      (
        completed(state, 7, id),
        state.db.partition().green_head(green),
        logged(state, svn).len(),
      )
    });
    let alpha = identity(b"alpha\n");
    let kept_as = on(|state| {
      state
        .vcs_stores
        .subversion_records(svn)
        .map(|records| (records.stored_len(&alpha), records.text(&alpha).is_some()))
    });
    assert_eq!(
      kept_as,
      Some((Some(6), false)),
      "the made text is not stored alone"
    );
    assert_eq!(
      in_the_verb,
      (false, 0, 0),
      "nothing answered or durable before the revision landed"
    );
    assert_eq!(
      after,
      (true, 1, 2),
      "answered once the revision landed and both committed"
    );
  });
}

/// Main's landing order, its crash case. Do: submit a work to a Subversion repository's green as a client's recorded
/// request, the made-landing task killed before it commits (as a `kill -9` between the store's land and the register's
/// commit leaves it); make the green and the records again from what is durable, as a restart does; then run the same
/// request again. Expect: after the kill, the state before (the green's durable head 0, the records at revision 0, the
/// request not answered); the retry lands once (the records at revision 1, not 2), the request answered, the green's
/// durable head 1. Until the deferred-submit hook, the version was durable before its revision landed, so a kill left
/// version 1 standing with no revision.
#[test]
fn a_kill_between_a_made_commits_landing_and_its_commit_recovers_to_before_and_the_retry_lands_once()
 {
  stored(|| async move {
    let (svn, green) = subversion("killed").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let work = on(|state| made_work(state, ipc_green, ("killed-work", "trunk/a.txt", b"alpha\n")));
    let id = slates_wire::request::RequestId {
      client: 7,
      sequence: 1,
    };
    let submit = move |state: &mut ShardState| {
      crate::verbs::run_recorded_for_test(
        state,
        (7, id),
        &owner(),
        crate::identities::authored(work, "made"),
      )
    };
    crate::vcs_store::svn_made::KILL_BEFORE_MADE_COMMIT.with(|kill| kill.set(true));
    on(|state| submit(state));
    // The killed task: its changeset lands, then the kill fires (clearing itself) where the step would commit.
    let deadline = slates_rt::futures::now_ns()
      .saturating_add(u64::try_from(PATIENCE.as_nanos()).unwrap_or(u64::MAX));
    while crate::vcs_store::svn_made::KILL_BEFORE_MADE_COMMIT.with(std::cell::Cell::get) {
      assert!(
        slates_rt::futures::now_ns() < deadline,
        "the kill never fired"
      );
      slates_rt::futures::yield_now().await;
    }
    // A restart: what memory held past the durable state is made again from it.
    on(crate::verbs::reconcile_unpublished_effects);
    rebuild(svn).await;
    let after_kill = on(|state| {
      (
        state.db.partition().green_head(green),
        kept(state, svn).0,
        completed(state, 7, id),
      )
    });
    assert_eq!(after_kill, (0, 0, false), "the state before the commit");
    on(|state| submit(state));
    youngest_reaches(svn, 1).await;
    for _ in 0..64 {
      slates_rt::futures::yield_now().await;
    }
    let after_retry = on(|state| {
      (
        state.db.partition().green_head(green),
        kept(state, svn).0,
        completed(state, 7, id),
        logged(state, svn).len(),
      )
    });
    assert_eq!(after_retry, (1, 1, true, 2), "the retry landed once");
  });
}

/// The deferred-submit hook's turn first. Do: hold a Subversion repository's landing turn (as a client's commit landing
/// holds it), submit a work to its green as a client's recorded request, then give the turn back. Expect: while the
/// turn is held, nothing of the submit ran (the green's engine and durable head at 0, the request not answered); once
/// it is given back, the parked submit runs again holding it, its revision lands and the request is answered, the
/// register naming the creation and the revision.
#[test]
fn a_submit_meeting_its_repositorys_landing_turn_held_waits_and_lands_once_it_is_given_back() {
  stored(|| async move {
    let (svn, green) = subversion("turn-held").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let work = on(|state| made_work(state, ipc_green, ("turn-work", "trunk/a.txt", b"alpha\n")));
    let id = slates_wire::request::RequestId {
      client: 7,
      sequence: 1,
    };
    let held = on(|state| crate::repository::svn_commit::landing::Turn::take_now(state, svn));
    let while_held = on(|state| {
      crate::verbs::run_recorded_for_test(
        state,
        (7, id),
        &owner(),
        crate::identities::authored(work, "made"),
      );
      (
        green_head(state, green),
        state.db.partition().green_head(green),
        completed(state, 7, id),
      )
    });
    assert_eq!(
      while_held,
      (Some(0), 0, false),
      "nothing ran while the turn was held"
    );
    on(move |state| {
      if let Some(turn) = held {
        turn.give_back(state);
      }
    });
    youngest_reaches(svn, 1).await;
    for _ in 0..64 {
      slates_rt::futures::yield_now().await;
    }
    let after = on(|state| {
      (
        state.db.partition().green_head(green),
        completed(state, 7, id),
        logged(state, svn).len(),
      )
    });
    assert_eq!(
      after,
      (1, true, 2),
      "run again holding the turn, landed and answered"
    );
  });
}

/// C2 stage B part 2 (b), P3 (a committed text is its changeset's on the node's store, not the content store's). Do: on
/// a daemon holding a store, commit a revision adding `/trunk/a.bin`, its text put through the content store as a
/// client's commit puts it; then read the text as a session does through the repository's view, waiting for any piece
/// the view does not hold. Expect revision 1, the content store holding nothing of the text once the commit answered,
/// the green's file the text, and the session's read whole (the head's files are the green's; a text nothing in RAM
/// holds is read through the store, held by the stage's (a) tests). Before, the content store held every committed text
/// until the deltify job made it a delta in RAM.
#[test]
fn a_committed_text_is_let_go_by_the_content_store_and_read_through_the_store() {
  use slates_svn::commit::drive::TextSink;
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let asked = (
    "let-go",
    slates_ipc::protocol::VcsTool::Subversion,
    slates_ipc::protocol::ObjectFormat::Sha1,
    "trunk",
  );
  let (id, green) = crate::test_store::create_on(&daemon, 0, &owner(), asked);
  let bytes = crate::test_store::noise(200 << 10);
  let committed = bytes.clone();
  let named = text(&bytes);
  let landed = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let texts = slates_svn::commit::drive::HeldTexts::new();
      let staged = texts.stage(None).unwrap();
      texts.append(staged, &committed).unwrap();
      texts.seal(staged, named.identity).unwrap();
      let work = Work {
        head: 0,
        record: RevisionRecord {
          transaction: "0-1".to_owned(),
          changes: vec![
            row("/trunk", Action::Add, NodeKind::Directory),
            ChangedPath {
              path: "/trunk/a.bin".to_owned(),
              action: Action::Add,
              kind: NodeKind::File,
              copy_from: None,
              text: Some(named),
              properties: None,
              mergeinfo_modified: false,
            },
          ],
        },
        lock_entries: Vec::new(),
        revision_properties: vec![(b"svn:log".to_vec(), b"let go".to_vec())],
        texts: std::iter::once(named.identity).collect(),
        property_lists: BTreeMap::new(),
        locks: CommitLocks::default(),
      };
      super::land_work((&owner(), id), work, &texts)
        .await
        .map(|landed| landed.map(|committed| committed.revision))
    },
    std::time::Duration::from_secs(60),
  );
  let kept = crate::daemon::observe_first(&daemon, move |state| {
    state
      .vcs_stores
      .content()
      .is_some_and(|content| content.contains(&slates_vcs::ContentId(named.identity)))
  });
  let file = crate::daemon::observe_first(&daemon, move |state| {
    green_file(state, green, "trunk/a.bin")
  });
  // A session's read: a piece at a time, waiting for each piece the view does not hold (C2 stage B part 2 (a)).
  let read = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let mut out = Vec::new();
      let mut fetched = Vec::new();
      while u64::try_from(out.len()).unwrap() < named.size {
        let at = u64::try_from(out.len()).unwrap();
        let piece = crate::state::with_state(|state| {
          crate::vcs_store::svn_view::lend_in(state, id, |lent| {
            let repository = lent.repository;
            match slates_svn::reads::Repository::text(repository, &named.identity, at) {
              Some(piece) => Ok(piece.into_owned()),
              None => Err(slates_svn::reads::Repository::fetchable(
                repository,
                &named.identity,
                at,
              )),
            }
          })
        })
        .flatten();
        match piece {
          Some(Ok(piece)) if !piece.is_empty() => out.extend_from_slice(&piece),
          Some(Err(true)) => {
            let wanted = slates_svn::reads::Wanted {
              text: named.identity,
              offset: at,
            };
            assert!(crate::vcs_store::svn_texts::fetch_piece(id, wanted).await);
            fetched.push(wanted);
          }
          other => panic!("the read at {at} answered {other:?}"),
        }
      }
      crate::state::with_state(|state| {
        for wanted in fetched {
          crate::vcs_store::svn_texts::let_go(state, wanted);
        }
      })
      .map(|()| out)
    },
    std::time::Duration::from_secs(60),
  );
  daemon.stop();
  assert!(matches!(landed, Some(Ok(1))), "{landed:?}");
  assert!(!kept, "the content store kept the committed text");
  assert!(
    file.as_deref() == Some(bytes.as_slice()),
    "the green's file is not the text"
  );
  assert!(read == Some(bytes), "the read is not the text");
}

/// What a reader of `green`'s engine sees: its head and the bytes at `path` there.
fn read_green(state: &ShardState, green: DbVolumeId, path: &str) -> Option<(u64, Option<Vec<u8>>)> {
  let engine = state.greens.get(&green)?;
  Some((
    engine.head(),
    engine.content_bytes(path).map(<[u8]>::to_vec),
  ))
}

/// A server-made commit's verdict is held with its landing, never shown before it commits (A-805; main 02:37, piece 1;
/// the landing order's "never expose a revision that was never committed"). Do: submit a work writing `trunk/a.txt` to
/// a Subversion repository's green as a client's recorded request, and read the green's engine in the submit's own step,
/// while its landing is held. Expect: the old head (0) and no `trunk/a.txt`; once the revision landed and committed,
/// head 1 with the work's bytes. Under A-804 the verdict applied in memory at the submit, so a reader saw version 1
/// before it was durable.
#[test]
fn a_read_of_the_green_while_its_made_commit_lands_sees_the_old_head() {
  stored(|| async move {
    let (svn, green) = subversion("held-read").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let work = on(|state| {
      made_work(
        state,
        ipc_green,
        ("held-read-work", "trunk/a.txt", b"alpha\n"),
      )
    });
    let id = slates_wire::request::RequestId {
      client: 7,
      sequence: 1,
    };
    let while_held = on(|state| {
      crate::verbs::run_recorded_for_test(
        state,
        (7, id),
        &owner(),
        crate::identities::authored(work, "made"),
      );
      read_green(state, green, "trunk/a.txt")
    });
    youngest_reaches(svn, 1).await;
    let after = on(|state| read_green(state, green, "trunk/a.txt"));
    assert_eq!(
      while_held,
      Some((0, None)),
      "the old head while the landing is held"
    );
    assert_eq!(
      after,
      Some((1, Some(b"alpha\n".to_vec()))),
      "the version once committed"
    );
  });
}

/// A refused landing leaves nothing any reader saw (A-805; main 02:37, piece 1). Do: with the node's store refusing
/// every changeset, submit a work to a Subversion repository's green as a client's recorded request; read the green in
/// the submit's own step and after the landing was refused; then let the store take changesets and run the request
/// again. Expect: every read the old head with no `trunk/a.txt`, nothing durable, the request not answered; the retry
/// lands revision 1 once. Under A-804 a reader in the submit's step saw version 1, which the refusal then took back.
#[test]
fn a_refused_made_landing_leaves_nothing_any_reader_saw_and_its_retry_lands_once() {
  stored(|| async move {
    let (svn, green) = subversion("refused").await;
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let work = on(|state| {
      made_work(
        state,
        ipc_green,
        ("refused-work", "trunk/a.txt", b"alpha\n"),
      )
    });
    let id = slates_wire::request::RequestId {
      client: 7,
      sequence: 1,
    };
    let submit = move |state: &mut ShardState| {
      crate::verbs::run_recorded_for_test(
        state,
        (7, id),
        &owner(),
        crate::identities::authored(work, "made"),
      )
    };
    crate::repository::svn_commit::landing::REFUSE_LANDING.with(|refuse| refuse.set(true));
    let in_the_step = on(|state| {
      submit(state);
      read_green(state, green, "trunk/a.txt")
    });
    for _ in 0..64 {
      slates_rt::futures::yield_now().await;
    }
    let refused = on(|state| {
      (
        read_green(state, green, "trunk/a.txt"),
        state.db.partition().green_head(green),
        completed(state, 7, id),
        kept(state, svn).0,
      )
    });
    crate::repository::svn_commit::landing::REFUSE_LANDING.with(|refuse| refuse.set(false));
    assert_eq!(
      in_the_step,
      Some((0, None)),
      "no reader saw the version in the submit's step"
    );
    assert_eq!(
      refused,
      (Some((0, None)), 0, false, 0),
      "nothing of it after the refusal"
    );
    on(|state| submit(state));
    youngest_reaches(svn, 1).await;
    for _ in 0..64 {
      slates_rt::futures::yield_now().await;
    }
    let retried = on(|state| {
      (
        state.db.partition().green_head(green),
        completed(state, 7, id),
        logged(state, svn).len(),
      )
    });
    assert_eq!(retried, (1, true, 2), "the retry landed once");
  });
}

/// C2 stage B part 2 (b), the records' RAM texts retired (main's ruling of 2026-11-21 09:20). Do: in three repositories
/// commit revision 1 and set its properties (changeset 2); in the second land, as a hostile log, a deltified text's
/// record (the retired kind) at changeset 2; in the third, revision 1's record with its texts inline (the form logs held
/// before part 2 (b)) at changeset 1; rebuild each from the node's store. Expect: the first made again whole; the second
/// refused `SvnRecord(Retired(Deltified))` and the third `SvnTextsInline`, each typed, never a panic or a skip.
#[test]
fn a_log_holding_a_retired_record_or_inline_texts_is_refused_typed_at_rebuild() {
  stored(|| async move {
    let mut seen = Vec::new();
    for (name, hostile) in [
      ("whole", None),
      ("deltified", Some(2u64)),
      ("inline", Some(1u64)),
    ] {
      let (svn, _) = subversion(name).await;
      assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
      let properties = Record::RevisionProperties {
        revision: 1,
        properties: BTreeMap::from([(b"svn:log".to_vec(), b"again".to_vec())]),
      };
      assert_eq!(
        super::land_record((&owner(), svn), properties).await,
        Some(Ok(()))
      );
      if let Some(sequence) = hostile {
        let bytes = if sequence == 2 {
          // The retired kind's bytes as a log written before held them: the header, kind 5, a text, no base, a delta.
          let mut bytes = Record::Locks(Vec::new()).encode();
          bytes.truncate(9);
          bytes[8] = 5;
          bytes.extend_from_slice(&[7u8; 32]);
          bytes.push(0);
          bytes.extend_from_slice(&0u64.to_le_bytes());
          bytes
        } else {
          Record::Revision(first()).encode()
        };
        let disk = on(|state| state.vcs_disk.clone()).expect("the node's store");
        let landed = super::landing::land(
          &disk,
          (svn, sequence),
          vec![(slates_store::log::SectionKind::Record, bytes)],
        )
        .await;
        assert_eq!(landed, Ok(()));
      }
      rebuild(svn).await;
      seen.push(on(|state| {
        state
          .vcs_stores
          .unrecovered(svn)
          .map(|refused| format!("{refused:?}"))
      }));
    }
    assert_eq!(
      seen,
      vec![
        None,
        Some("SvnRecord(Retired(Deltified))".to_owned()),
        Some("SvnTextsInline".to_owned())
      ]
    );
  });
}

/// P7's finding (1), its robustness side (a green folds its history under a tight budget, `Green::fold_history_to_budget`;
/// FSFS copies from any committed revision). Do: commit revision 1 (`/trunk/a.txt` `alpha\n`, `/trunk/sub/b.txt`),
/// revision 2 changing `a.txt` to `alpha 2\n`; fold the green's history below its head, as the budget's fold does under
/// pressure; then commit revision 3 copying `/trunk` at revision 1 to `/branches/b`. Expect revision 3, and the green's
/// `branches/b/a.txt` revision 1's text (`alpha\n`), `branches/b/sub/b.txt` beside it and `trunk/a.txt` still revision 2's.
/// Before, the copy was refused once the green's history below its head was folded: its source's version was gone.
#[test]
fn a_copy_from_an_older_revision_commits_after_the_green_folded_its_history() {
  stored(|| async move {
    let (svn, green) = subversion("folded").await;
    assert_eq!(commit((&owner(), svn), first(), None).await, answered(1));
    let changed = revision(
      1,
      vec![file("/trunk/a.txt", Action::Modify, b"alpha 2\n")],
      &[b"alpha 2\n"],
    );
    assert_eq!(commit((&owner(), svn), changed, None).await, answered(2));
    on(|state| {
      let mut engine = state.greens.get_mut(&green).expect("the green");
      let head = engine.head();
      engine.fold_history_before(head);
    });
    let copied = ChangedPath {
      copy_from: Some(CopySource {
        path: "/trunk".to_owned(),
        revision: 1,
      }),
      ..row("/branches/b", Action::Add, NodeKind::Directory)
    };
    let branched = revision(
      2,
      vec![row("/branches", Action::Add, NodeKind::Directory), copied],
      &[],
    );
    let committed = commit((&owner(), svn), branched, None).await;
    let seen = (
      on(|state| green_file(state, green, "branches/b/a.txt")),
      on(|state| green_file(state, green, "branches/b/sub/b.txt")),
      on(|state| green_file(state, green, "trunk/a.txt")),
    );
    assert_eq!(committed, answered(3));
    assert_eq!(
      seen,
      (
        Some(b"alpha\n".to_vec()),
        Some(b"beta\n".to_vec()),
        Some(b"alpha 2\n".to_vec())
      )
    );
  });
}

/// A node of [`copies_oracle`]'s model: a directory, or a file's text, and its property list's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelNode {
  /// Whether it is a directory.
  directory: bool,
  /// A file's text.
  text: Vec<u8>,
  /// Its property list in FSFS's form, none for none.
  props: Option<Vec<u8>>,
}

/// A tree of the model, by green path.
type ModelTree = BTreeMap<String, ModelNode>;

/// The paths of `tree` at and beneath `path`.
fn subtree(tree: &ModelTree, path: &str) -> Vec<String> {
  tree
    .keys()
    .filter(|found| *found == path || found.starts_with(&format!("{path}/")))
    .cloned()
    .collect()
}

/// A green's tree as a test reads it: each path's directory flag, its bytes, and its property list.
type GreenTree = BTreeMap<String, (bool, Vec<u8>, Option<Vec<u8>>)>;

/// The green `green`'s tree as the model writes one: every node, its text and its property list's identity.
fn green_tree(state: &ShardState, green: DbVolumeId) -> GreenTree {
  let engine = state.greens.get(&green).expect("the green");
  engine
    .beneath("")
    .into_iter()
    .map(|(path, directory)| {
      let text = engine
        .content_bytes(&path)
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
      let props = engine.xattr(&path, PROPS).map(<[u8]>::to_vec);
      (
        path,
        (directory, if directory { Vec::new() } else { text }, props),
      )
    })
    .collect()
}

/// `tree` as [`green_tree`] reads a green: a property list by its identity.
fn as_green(tree: &ModelTree) -> GreenTree {
  tree
    .iter()
    .map(|(path, node)| {
      let props = node.props.as_deref().map(|bytes| identity(bytes).to_vec());
      (path.clone(), (node.directory, node.text.clone(), props))
    })
    .collect()
}

/// P7's finding (1), main's oracle of 2026-11-21 (10:55, condition 1): copies from older revisions made from the head
/// and set back. Do: for each of 24 seeds, commit 14 generated revisions to a Subversion repository (an addition of a
/// file or a directory, a text changed, a property list set, a deletion, a copy of any node from any revision before,
/// copies of copies among them, and a copy changed beneath in the revision that makes it); a model applies each to its
/// own tree. Expect every revision committed; after each, the green folded to its head (no version kept, so no copy can
/// read one) and its tree (every node, each file's text, each property list's identity) the model's: a copy from
/// revision R holds what its source held at R. Copies from older revisions walk their sources
/// (`repository.svn_commit.restore_slices` counted).
#[test]
fn copies_oracle_a_copy_from_any_revision_is_its_source_as_it_stood_then() {
  stored(|| async move {
    for seed in 1..=24u64 {
      let (svn, green) = subversion(&format!("oracle-{seed}")).await;
      let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
      let mut next = move |bound: usize| -> usize {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state % u64::try_from(bound.max(1)).unwrap()).unwrap()
      };
      let mut history: Vec<ModelTree> = vec![ModelTree::new()];
      let mut named = 0usize;
      for head in 0..14u64 {
        let mut tree = history.last().cloned().unwrap();
        let directories: Vec<String> = std::iter::once(String::new())
          .chain(
            tree
              .iter()
              .filter(|(_, node)| node.directory)
              .map(|(path, _)| path.clone()),
          )
          .collect();
        let files: Vec<String> = tree
          .iter()
          .filter(|(_, node)| !node.directory)
          .map(|(path, _)| path.clone())
          .collect();
        let under = |parent: &str, name: &str| {
          if parent.is_empty() {
            name.to_owned()
          } else {
            format!("{parent}/{name}")
          }
        };
        named += 1;
        let name = format!("n{named}");
        let bytes = format!("text {} of {seed}\n", next(4)).into_bytes();
        let (rows, contents): (Vec<ChangedPath>, Vec<Vec<u8>>) = match next(8) {
          0 | 1 => {
            let path = under(&directories[next(directories.len())], &name);
            tree.insert(
              path.clone(),
              ModelNode {
                directory: false,
                text: bytes.clone(),
                props: None,
              },
            );
            (
              vec![file(&format!("/{path}"), Action::Add, &bytes)],
              vec![bytes],
            )
          }
          2 => {
            let path = under(&directories[next(directories.len())], &name);
            tree.insert(
              path.clone(),
              ModelNode {
                directory: true,
                text: Vec::new(),
                props: None,
              },
            );
            (
              vec![row(&format!("/{path}"), Action::Add, NodeKind::Directory)],
              Vec::new(),
            )
          }
          3 if !files.is_empty() => {
            let path = files[next(files.len())].clone();
            let bytes = format!("changed {named} of {seed}\n").into_bytes();
            tree.get_mut(&path).unwrap().text = bytes.clone();
            (
              vec![file(&format!("/{path}"), Action::Modify, &bytes)],
              vec![bytes],
            )
          }
          4 if !tree.is_empty() => {
            let path = tree.keys().nth(next(tree.len())).unwrap().clone();
            let listed = list("p", &format!("v{}", next(3)));
            let node = tree.get_mut(&path).unwrap();
            node.props = Some(listed.clone());
            let kind = if node.directory {
              NodeKind::Directory
            } else {
              NodeKind::File
            };
            let changed = ChangedPath {
              properties: Some(held(&listed)),
              ..row(&format!("/{path}"), Action::Modify, kind)
            };
            (vec![changed], vec![listed])
          }
          5 if !tree.is_empty() => {
            let path = tree.keys().nth(next(tree.len())).unwrap().clone();
            let kind = if tree[&path].directory {
              NodeKind::Directory
            } else {
              NodeKind::File
            };
            for gone in subtree(&tree, &path) {
              tree.remove(&gone);
            }
            (
              vec![row(&format!("/{path}"), Action::Delete, kind)],
              Vec::new(),
            )
          }
          _ => {
            // A copy of a node of any revision before this one, from the oldest to the head.
            let from = 1 + next(usize::try_from(head).unwrap().max(1));
            let from = u64::try_from(from).unwrap().min(head);
            let then = &history[usize::try_from(from).unwrap()];
            if then.is_empty() || from == 0 {
              let path = under(&directories[next(directories.len())], &name);
              tree.insert(
                path.clone(),
                ModelNode {
                  directory: true,
                  text: Vec::new(),
                  props: None,
                },
              );
              (
                vec![row(&format!("/{path}"), Action::Add, NodeKind::Directory)],
                Vec::new(),
              )
            } else {
              let source = then.keys().nth(next(then.len())).unwrap().clone();
              let parents: Vec<&String> = directories
                .iter()
                .filter(|parent| **parent != source && !parent.starts_with(&format!("{source}/")))
                .collect();
              let dest = under(parents[next(parents.len())], &name);
              let kind = if then[&source].directory {
                NodeKind::Directory
              } else {
                NodeKind::File
              };
              for found in subtree(then, &source) {
                let moved = format!("{dest}{}", found.get(source.len()..).unwrap());
                tree.insert(moved, then[&found].clone());
              }
              let copied = ChangedPath {
                copy_from: Some(CopySource {
                  path: format!("/{source}"),
                  revision: from,
                }),
                ..row(&format!("/{dest}"), Action::Add, kind)
              };
              let mut rows = vec![copied];
              let mut contents = Vec::new();
              // Now and then, a file the copy brought changed in the same revision.
              let inside: Vec<String> = subtree(&tree, &dest)
                .into_iter()
                .filter(|found| *found != dest && !tree[found].directory)
                .collect();
              if !inside.is_empty() && next(2) == 0 {
                let path = inside[next(inside.len())].clone();
                let bytes = format!("changed in the copy {named} of {seed}\n").into_bytes();
                tree.get_mut(&path).unwrap().text = bytes.clone();
                rows.push(file(&format!("/{path}"), Action::Modify, &bytes));
                contents.push(bytes);
              }
              (rows, contents)
            }
          }
        };
        let rows = if rows.is_empty() {
          let path = under(&directories[next(directories.len())], &name);
          tree.insert(
            path.clone(),
            ModelNode {
              directory: true,
              text: Vec::new(),
              props: None,
            },
          );
          vec![row(&format!("/{path}"), Action::Add, NodeKind::Directory)]
        } else {
          rows
        };
        let brought: Vec<&[u8]> = contents.iter().map(Vec::as_slice).collect();
        let committed = commit(
          (&owner(), svn),
          revision(head, rows.clone(), &brought),
          None,
        )
        .await;
        assert_eq!(
          committed.map(|answer| answer.revision),
          Ok(head + 1),
          "seed {seed} revision {}: {rows:?}",
          head + 1
        );
        // The green keeps no version below its head: no reader names one, and no copy reads one.
        let folded = on(|state| {
          let engine = state.greens.get(&green).expect("the green");
          (engine.folded_below(), engine.head())
        });
        assert_eq!(
          folded,
          (head + 1, head + 1),
          "seed {seed}: the green kept a version"
        );
        let seen = on(|state| green_tree(state, green));
        assert!(
          seen == as_green(&tree),
          "seed {seed} revision {}: {rows:?}\ngreen {seen:?}\nmodel {:?}",
          head + 1,
          as_green(&tree)
        );
        history.push(tree);
      }
    }
    let slices = on(|state| {
      state
        .refusals
        .get(super::restore::SLICES)
        .copied()
        .unwrap_or(0)
    });
    assert!(
      slices > 0,
      "no copy from an older revision walked its source"
    );
  });
}

/// A-724 piece 3c (§4.17.5 "What a repository costs, counted" and "Learned demand": every tool's intakes counted).
/// Do: on a store-holding daemon, land a Subversion revision adding one file of 4,096 bytes through `land_work`, the
/// step both ra_svn transports and the HTTP edge call, then read the repository's usage row. Expect: the commit counted
/// as one intake begun and answered, and its text's bytes, exactly, learned as the repository's next intake.
#[test]
fn t_a724_a_subversion_commit_is_counted_and_learned_on_its_repositorys_usage_row() {
  let bytes = crate::test_store::noise(4096);
  let (_store, daemon, id) =
    crate::test_store::a_text_only_the_store_holds(&owner(), "counted", &bytes);
  let row = crate::daemon::observe_first(&daemon, move |state| {
    state.vcs_usage.usage(&id).map(|row| {
      (
        row.intake.begun,
        row.intake.answered,
        row.learned(slates_vcs::Work::Intake).bytes_per_work(),
      )
    })
  });
  daemon.stop();
  assert_eq!(
    row,
    Some((1, 1, Some(4096))),
    "the commit counted and its bytes learned"
  );
}

/// Shape: [`a_text_refused_part_way_through_its_put_leaves_the_store_as_it_was`]'s text, long enough that the windows
/// kept before its refusal are many whatever window the store derives.
const PART_PUT_BYTES: usize = 4 << 20;
/// Shape: the bytes of that text its reader answers before it refuses the next piece: half of it.
const PART_PUT_ANSWERED: usize = PART_PUT_BYTES / 2;
/// Shape: the most its reader answers a piece, so the put takes many steps whatever copy slice the machine derives.
const PART_PUT_PIECE: usize = 64 << 10;

/// Goal 93 (main 01a20182, 2026-11-22 03:55 PST; a sibling of P7 finding (2)'s `put_window` cut). Do: put a 4 MiB text
/// into the content store a piece of at most 64 KiB a step (`store_step_from`) until its reader refuses a piece half-way, then
/// release what the landing holds, as every refused landing does (`release_stored`). Expect: windows kept before the
/// refusal (the test's non-vacuity), the step refused `Corrupt`, and the store's entries and bytes used what they were
/// before the put. Failing first: the step took the text's writer out of the landing's `Storing` before the piece was
/// read and put, so a refusal dropped it with its kept windows still held, and the release found nothing to abandon.
#[test]
fn a_text_refused_part_way_through_its_put_leaves_the_store_as_it_was() {
  let seen = stored(|| async move {
    let (_svn, _green) = subversion("svn").await;
    let bytes: Vec<u8> = (0..PART_PUT_BYTES)
      .map(|at| u8::try_from(at % 251).unwrap_or(0))
      .collect();
    let text = identity(&bytes);
    let reader = |wanted: &ContentIdentity, offset: u64, len: usize| {
      let start = usize::try_from(offset)
        .ok()
        .filter(|start| *wanted == text && *start < PART_PUT_ANSWERED)?;
      bytes
        .get(
          start
            ..start
              .saturating_add(len.min(PART_PUT_PIECE))
              .min(PART_PUT_BYTES),
        )
        .map(std::borrow::Cow::Borrowed)
    };
    on(|state| {
      let content = |state: &ShardState| {
        state
          .vcs_stores
          .content()
          .map(|store| (store.len(), store.used()))
      };
      let before = content(state);
      let mut storing = super::Storing {
        queue: std::collections::VecDeque::from([(text, u64::try_from(PART_PUT_BYTES).unwrap())]),
        ..super::Storing::default()
      };
      let refused = loop {
        match super::store_step_from(state, &mut storing, &reader) {
          Ok(false) => continue,
          answer => break answer,
        }
      };
      let kept = content(state);
      super::release_stored(state, storing);
      (before, kept, refused, content(state))
    })
  });
  let (before, kept, refused, after) = seen;
  assert!(
    kept
      .zip(before)
      .is_some_and(|(kept, before)| kept.1 > before.1),
    "windows were kept before the refusal: {before:?} then {kept:?}"
  );
  assert_eq!(
    refused,
    Err(super::corrupt()),
    "the step refused at the missing piece"
  );
  assert_eq!(after, before, "the store after the refused put's release");
}

/// Shape: the texts [`an_increment_names_its_stored_texts_and_carries_none_of_their_bytes`] adds, in path order.
const POST_TEXTS: [(&str, usize); 3] = [
  ("/trunk/p", 3_000),
  ("/trunk/q", 1_000),
  ("/trunk/r", 1_000),
];

/// Goal 93, P7 finding (2), A-760 for the Subversion landing (main 01a20182's rulings of 2026-11-22 11:05 and ~11:35):
/// a text stored in the content store is named by its increment, never copied into its post-state. Do: land revision 1,
/// then stage revision 2's three texts as a landing does (`stage_texts`), check it, and build its green increment.
/// Expect: the post-state carries none of the texts' bytes; the increment names each text whole, in its rows' order, by
/// its identity and length; every run is read, the read counted once a byte. Failing first: each text was copied into
/// the post-state (5,000 bytes inline, no run named), the import's largest per-text copy after A-862.
#[test]
fn an_increment_names_its_stored_texts_and_carries_none_of_their_bytes() {
  let made = |at: u8, len: usize| {
    let mut bytes = vec![b'x'; len];
    bytes[0] = at;
    bytes
  };
  let built = stored(move || async move {
    let (svn, green) = subversion("svn").await;
    commit((&owner(), svn), first(), None).await.unwrap();
    let texts: Vec<Vec<u8>> = POST_TEXTS
      .iter()
      .zip(1u8..)
      .map(|((_, len), at)| made(at, *len))
      .collect();
    let rows = POST_TEXTS
      .iter()
      .zip(&texts)
      .map(|((path, _), bytes)| file(path, Action::Add, bytes))
      .collect();
    let contents: Vec<&[u8]> = texts.iter().map(Vec::as_slice).collect();
    let mut revision = revision(1, rows, &contents);
    let (storing, _) = super::stage_texts(svn, &mut revision, Vec::new())
      .await
      .unwrap()
      .unwrap();
    on(|state| {
      let read = |state: &ShardState| {
        state
          .refusals
          .get(super::NAMED_READ_BYTES)
          .copied()
          .unwrap_or(0)
      };
      // The records' room lent for the step, as a landing's check is lent it.
      assert!(crate::vcs_store::lend_svn(state, svn, 0));
      let (checked, record) = super::checked(state, svn, revision, None).unwrap();
      crate::vcs_store::settle_svn(state, svn);
      let read_before = read(state);
      let built = super::version_of(
        state,
        (svn, green),
        (&checked, Default::default()),
        super::identity(&record),
      )
      .unwrap();
      super::release_stored(state, storing);
      let named: Vec<([u8; 32], u64, u64)> = built
        .named
        .runs
        .iter()
        .map(|run| (run.identity.hash, run.identity.len, run.len))
        .collect();
      (
        built.post_state.len(),
        named,
        built.unread().is_none(),
        read(state).saturating_sub(read_before),
      )
    })
  });
  let expected: Vec<([u8; 32], u64, u64)> = POST_TEXTS
    .iter()
    .zip(1u8..)
    .map(|((_, len), at)| {
      let len_bytes = u64::try_from(*len).unwrap();
      (identity(&made(at, *len)), len_bytes, len_bytes)
    })
    .collect();
  let texts: u64 = POST_TEXTS.iter().map(|(_, len)| *len as u64).sum();
  assert_eq!(built, (0, expected, true, texts));
}

/// Shape: the lengths of the two texts [`an_increments_record_names_its_texts_not_their_bytes`] builds increments of.
const RECORDED_TEXTS: [usize; 2] = [1_000, 100_000];

/// A-760 for the Subversion landing, its log's side (main 01a20182's ruling of 2026-11-22 ~11:35, piece (b)). Do: land
/// revision 1, then build revision 2's green increment twice, each adding one file at the same path, once with a text of
/// 1,000 bytes and once with one of 100,000. Expect: the two increments' encodings (what the chain entry records, or
/// names, of its revision) the same length: a text is recorded by its name, never its bytes. Failing first (before
/// A-760's naming): each encoding carried its text inline, 99,000 bytes apart.
#[test]
fn an_increments_record_names_its_texts_not_their_bytes() {
  let encoded = stored(|| async move {
    let (svn, green) = subversion("svn").await;
    commit((&owner(), svn), first(), None).await.unwrap();
    let mut lengths = Vec::new();
    for len in RECORDED_TEXTS {
      let bytes = vec![b'r'; len];
      let rows = vec![file("/trunk/recorded", Action::Add, &bytes)];
      let mut revision = revision(1, rows, &[&bytes]);
      let (storing, _) = super::stage_texts(svn, &mut revision, Vec::new())
        .await
        .unwrap()
        .unwrap();
      let length = on(|state| {
        assert!(crate::vcs_store::lend_svn(state, svn, 0));
        let (checked, record) = super::checked(state, svn, revision, None).unwrap();
        crate::vcs_store::settle_svn(state, svn);
        let built = super::version_of(
          state,
          (svn, green),
          (&checked, Default::default()),
          super::identity(&record),
        )
        .unwrap();
        super::release_stored(state, storing);
        built.encode().len()
      });
      lengths.push(length);
    }
    lengths
  });
  assert_eq!(encoded.first(), encoded.get(1), "{encoded:?}");
}

/// Goal 93, A-760's append (main 01a20182's ruling of 2026-11-22 ~11:50: `judge_named` in the append every push shares,
/// its own landing). Do: land revision 1, then commit revision 2 adding a file of 50,000 bytes, its text stored and
/// named (A-865). Expect: its text read once (`NAMED_READ_BYTES`), and its verdict taking the read buffer rather than
/// cloning it (`push::READ_CLONED_BYTES` 0). Failing first: the append judged with `Green::judge`, which clones every
/// read buffer into the verdict: 50,000 bytes cloned.
#[test]
fn a_named_works_verdict_takes_its_read_texts_and_clones_none() {
  let counted = stored(|| async move {
    let (svn, _green) = subversion("svn").await;
    commit((&owner(), svn), first(), None).await.unwrap();
    let bytes = vec![b'n'; 50_000];
    let rows = vec![file("/trunk/named", Action::Add, &bytes)];
    let read = |state: &ShardState, name: &str| state.refusals.get(name).copied().unwrap_or(0);
    let before = on(|state| {
      (
        read(state, super::NAMED_READ_BYTES),
        read(state, crate::repository::push::READ_CLONED_BYTES),
      )
    });
    commit((&owner(), svn), revision(1, rows, &[&bytes]), None)
      .await
      .unwrap();
    on(|state| {
      (
        read(state, super::NAMED_READ_BYTES).saturating_sub(before.0),
        read(state, crate::repository::push::READ_CLONED_BYTES).saturating_sub(before.1),
      )
    })
  });
  assert_eq!(counted, (50_000, 0), "(read, cloned)");
}
