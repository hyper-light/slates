//! A-655 part 2b's tests (§4.17.4 items 1 and 3.5): a Subversion repository made with its creation's record, revisions
//! landed through the owner's step as its green's versions with their records, a moved work judged again, refusals
//! that change nothing, and the records made again from the partition's log.

use std::collections::BTreeMap;

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{ObjectFormat, Refusal, ReplyBody, RequestBody, VcsTool};
use slates_svn::commit::drive::Work;
use slates_svn::commit::record::Record;
use slates_svn::commit::revision::{Revision, StepRefusal};
use slates_svn::commit::{CommitRefusal, ConflictKind};
use slates_svn::index::{
  Action, ChangedPath, ContentIdentity, CopySource, NodeKind, PropertyList, RevisionRecord, Text,
};
use slates_svn::lock::{AprTime, CommitLocks};

use super::{Committed, PROPS, UNSUPPORTED, Uncommitted, commit, commit_work};
use crate::repository::push::AppendRefused;
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

/// Shape: the date every test revision carries.
const DATE: &str = "2026-11-19T20:00:00.000000Z";
/// Shape: [`DATE`]'s instant: microseconds since the epoch.
const DATE_INSTANT: AprTime = AprTime(1_795_118_400_000_000);

/// The repository's owner.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates Subversion repository `name` on the shard: its id and its one green.
fn subversion(state: &mut ShardState, name: &str) -> (RepositoryId, DbVolumeId) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: VcsTool::Subversion,
      format: ObjectFormat::Revisions,
      default_branch: "main".to_owned(),
      green: None,
    },
  );
  let ReplyBody::GreenCreated { id } = reply else {
    panic!("no repository: {reply:?}")
  };
  let repositories = state.db.partition().repositories();
  let record = repositories.repository_named(name).expect("the record");
  (record.id, to_db_volume(id))
}

/// `bytes`' content identity.
fn identity(bytes: &[u8]) -> ContentIdentity {
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
    lock_entries: Vec::new(),
  }
}

/// `/trunk`'s property list.
fn ignores() -> Vec<u8> {
  list("svn:ignore", "*.o")
}

/// Revision 1: `/trunk` holding a property list, `/trunk/a.txt`, and `/trunk/sub` holding `b.txt`.
fn first() -> Revision {
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
  engine.content(path).map(<[u8]>::to_vec)
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

/// A-655 part 2b (`CreateRepository` of a Subversion repository; `svnadmin create`'s UUID and revision 0's date). Do:
/// create a Subversion repository. Expect: its records at revision 0, its UUID its id's sixteen bytes in RFC 4122's
/// form, revision 0's `svn:date` the one its register's one record holds; the register's record its creation's; the
/// green at version 0; the shard charged what the records hold.
#[test]
fn t_a655_a_subversion_repository_is_made_with_its_creation_record() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    let records = logged(state, svn);
    let [created] = records.as_slice() else {
      panic!("not one record: {records:?}")
    };
    let Ok(Record::Created { uuid, date }) = Record::decode(created) else {
      panic!("not a creation: {created:?}")
    };
    let made = state.vcs_stores.subversion_records(svn).map(|records| {
      let dated = records
        .revision_properties(0)
        .and_then(|properties| properties.get(b"svn:date".as_slice()))
        .cloned();
      (records.youngest(), records.uuid(), dated)
    });
    assert_eq!(made, Some((0, uuid, Some(date))));
    let digits: String = svn.bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let printed = String::from_utf8(uuid.as_bytes().to_vec()).unwrap();
    assert_eq!(printed.replace('-', ""), digits);
    let (_, held, charged) = kept(state, svn);
    assert_eq!((green_head(state, green), charged), (Some(0), held));
  });
}

/// A-655 part 2b (§4.17.4 items 1 and 3.5). Do: commit revision 1 (a directory with a property list, a file, a
/// directory holding a file) through the step. Expect: answered revision 1 with its date and author; the green at
/// version 1 holding each file's bytes, the directories, and the directory's list named by its identity; the
/// register's second record the revision whole; the records at revision 1 holding the texts; the shard charged what
/// they hold.
#[test]
fn t_a655_a_revision_lands_as_the_greens_next_version_with_its_record() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    let committed = commit(state, (&owner(), svn), first(), None);
    assert_eq!(committed, answered(1));
    let seen = (
      green_head(state, green),
      green_file(state, green, "trunk/a.txt"),
      green_file(state, green, "trunk/sub/b.txt"),
      green_dir(state, green, "trunk/sub"),
      green_props(state, green, "trunk"),
    );
    let expected = (
      Some(1),
      Some(b"alpha\n".to_vec()),
      Some(b"beta\n".to_vec()),
      true,
      Some(identity(&ignores()).to_vec()),
    );
    assert_eq!(seen, expected);
    let records = logged(state, svn);
    let second = records.get(1).map(|bytes| Record::decode(bytes));
    assert_eq!(second, Some(Ok(Record::Revision(first()))));
    let alpha = state
      .vcs_stores
      .subversion_records(svn)
      .and_then(|records| records.content(&identity(b"alpha\n")).map(<[u8]>::to_vec));
    assert_eq!(alpha, Some(b"alpha\n".to_vec()));
    let (youngest, held, charged) = kept(state, svn);
    assert_eq!((youngest, charged), (1, held));
  });
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
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    assert_eq!(
      commit(state, (&owner(), svn), branched(), None),
      answered(2)
    );
    let after_copy = (
      green_props(state, green, "branches/b"),
      green_file(state, green, "branches/b/a.txt"),
      green_dir(state, green, "branches/b/sub"),
      green_file(state, green, "branches/b/sub/b.txt"),
      green_file(state, green, "trunk/a.txt"),
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
    assert_eq!(commit(state, (&owner(), svn), deleted, None), answered(3));
    let gone = (
      green_dir(state, green, "trunk"),
      green_dir(state, green, "trunk/sub"),
      green_file(state, green, "trunk/sub/b.txt"),
    );
    assert_eq!(gone, (false, false, None));
    let native = list("svn:eol-style", "native");
    let replaced = ChangedPath {
      properties: Some(held(&native)),
      ..file("/branches/b/a.txt", Action::Replace, b"gamma\n")
    };
    let replacing = revision(3, vec![replaced], &[b"gamma\n", &native]);
    assert_eq!(commit(state, (&owner(), svn), replacing, None), answered(4));
    let after = (
      green_head(state, green),
      green_file(state, green, "branches/b/a.txt"),
      green_props(state, green, "branches/b/a.txt"),
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
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    let locks = alices();
    let judged = Some((&locks, AprTime(1)));
    let edit =
      |path: &str, bytes: &[u8]| revision(1, vec![file(path, Action::Modify, bytes)], &[bytes]);
    let one = edit("/trunk/a.txt", b"a 2\n");
    let two = edit("/trunk/sub/b.txt", b"b 2\n");
    let three = edit("/trunk/a.txt", b"a 3\n");
    assert_eq!(commit(state, (&owner(), svn), one, judged), answered(2));
    assert_eq!(commit(state, (&owner(), svn), two, judged), answered(3));
    let both = (
      green_head(state, green),
      green_file(state, green, "trunk/a.txt"),
      green_file(state, green, "trunk/sub/b.txt"),
    );
    assert_eq!(
      both,
      (Some(3), Some(b"a 2\n".to_vec()), Some(b"b 2\n".to_vec()))
    );
    let before = whole(state, (svn, green));
    let refused = commit(state, (&owner(), svn), three, judged);
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
    assert_eq!(whole(state, (svn, green)), before);
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
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
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
      let before = whole(state, (svn, green));
      assert_eq!(commit(state, (&principal, svn), refused, None), Err(why));
      assert_eq!(whole(state, (svn, green)), before);
    }
    let edit = revision(
      1,
      vec![file("/trunk/a.txt", Action::Modify, b"a 2\n")],
      &[b"a 2\n"],
    );
    assert_eq!(commit(state, (&owner(), svn), edit, None), answered(2));
    let mut older = branched();
    older.head = 2;
    let before = whole(state, (svn, green));
    let counted = state.refusals.get(UNSUPPORTED).copied().unwrap_or(0);
    let unsupported = Uncommitted::Unsupported {
      path: "/branches/b/sub".to_owned(),
    };
    assert_eq!(
      commit(state, (&owner(), svn), older, None),
      Err(unsupported)
    );
    assert_eq!(whole(state, (svn, green)), before);
    assert_eq!(state.refusals.get(UNSUPPORTED).copied(), Some(counted + 1));
  });
}

/// A-655 part 2b (`vcs_store`: a recovery's replay of the records through the step's check and apply). Do: commit
/// revisions 1 to 3 (a branch made by copy, a deletion), then make the store again from the partition's log. Expect:
/// the records made again equal the live ones whole, as they print; every record replayed, the creation's among them;
/// none refused; the shard charged what they hold.
#[test]
fn t_a655_the_records_are_made_again_from_the_log() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    assert_eq!(
      commit(state, (&owner(), svn), branched(), None),
      answered(2)
    );
    let deleted = revision(
      2,
      vec![row("/trunk", Action::Delete, NodeKind::Directory)],
      &[],
    );
    assert_eq!(commit(state, (&owner(), svn), deleted, None), answered(3));
    let live = whole(state, (svn, green));
    let replayed = state
      .refusals
      .get(crate::vcs_store::REPLAYED)
      .copied()
      .unwrap_or(0);
    crate::vcs_store::rebuild(state, svn);
    assert_eq!(state.vcs_stores.unrecovered(svn), None);
    assert_eq!(whole(state, (svn, green)), live);
    let count = u64::try_from(logged(state, svn).len()).unwrap();
    assert_eq!(
      state.refusals.get(crate::vcs_store::REPLAYED).copied(),
      Some(replayed + count)
    );
    let (_, held, charged) = kept(state, svn);
    assert_eq!((count, charged), (4, held));
  });
}

/// A-698 (`commit_work`: what both ra_svn transports call with the work their connection accepted). Do: land a client's
/// accepted work adding `/trunk`, by alice, presenting no lock, at [`DATE`]'s instant. Expect: revision 1, dated by that
/// instant and by alice, as `commit-info` carries them; the green at version 1 holding `/trunk`; the register's second
/// record the revision the step made.
#[test]
fn t_a698_a_clients_accepted_work_lands_through_the_step() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
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
      texts: BTreeMap::new(),
      property_lists: BTreeMap::new(),
      locks: CommitLocks {
        user: Some("alice".to_owned()),
        ..CommitLocks::default()
      },
    };
    let landed = commit_work(state, (&owner(), svn), work, DATE_INSTANT);
    let made = revision(
      0,
      vec![row("/trunk", Action::Add, NodeKind::Directory)],
      &[],
    );
    let records = logged(state, svn);
    let second = records.get(1).map(|bytes| Record::decode(bytes));
    let seen = (
      landed,
      green_head(state, green),
      green_dir(state, green, "trunk"),
    );
    assert_eq!(seen, (answered(1), Some(1), true));
    assert_eq!(second, Some(Ok(Record::Revision(made))));
  });
}
