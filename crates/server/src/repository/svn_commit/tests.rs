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
pub(crate) fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates Subversion repository `name` on the shard: its id and its one green.
pub(crate) fn subversion(state: &mut ShardState, name: &str) -> (RepositoryId, DbVolumeId) {
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
    crate::vcs_store::settle_rebuild(state, svn);
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
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    let mut head = 1;
    while state.db.partition().green_floor(green) == 0 && head < FOLD_WITHIN {
      let bytes = format!("alpha {head}\n").into_bytes();
      let rows = vec![file("/trunk/a.txt", Action::Modify, &bytes)];
      let next = revision(head, rows, &[&bytes]);
      assert_eq!(
        commit(state, (&owner(), svn), next, None),
        answered(head + 1)
      );
      head += 1;
    }
    assert!(
      state.db.partition().green_floor(green) > 0,
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
/// every revision lands, the register's objects hold more than the merge budget, and the shard's budget charges them:
/// a repository's content is bounded by what its owner shard can hold (A-662, `vcs_store::settle_objects`), never by
/// the merge budget the partition's chains share; and a revision whose entry the chains had no room for checkpointed
/// its green first and was appended once (A-742's counter moved). Before the fix each revision's record was charged to the merge
/// budget, so the repository's whole content had to fit in it.
#[test]
fn a_repositorys_content_is_bounded_by_its_shard_not_the_partitions_merge_budget() {
  let landed = crate::daemon::audit_on_shard_configured(
    |state| {
      let (svn, _green) = subversion(state, "svn");
      let mut answers = vec![commit(state, (&owner(), svn), first(), None)];
      for head in 1..9u64 {
        let bytes = vec![u8::try_from(head).unwrap_or(0); QUARTER_TEXT];
        let rows = vec![file("/trunk/a.txt", Action::Modify, &bytes)];
        answers.push(commit(
          state,
          (&owner(), svn),
          revision(head, rows, &[&bytes]),
          None,
        ));
      }
      let held = state.db.partition().repositories().objects_bytes(svn);
      let charged = state.vcs_stores.objects_charged(svn);
      let checkpointed_first = state
        .refusals
        .get(super::CHAIN_ROOM_CHECKPOINTED)
        .copied()
        .unwrap_or(0);
      (
        answers,
        held > SMALL_MERGE_BUDGET,
        charged == u64::try_from(held).unwrap_or(0),
        checkpointed_first > 0,
      )
    },
    |config| config.caps.green_chain_bytes = SMALL_MERGE_BUDGET,
  );
  let expected: Vec<_> = (1..10).map(answered).collect();
  assert_eq!(landed, (expected, true, true, true));
}

/// Found by use (w-perf, 2026-11-19, release b705a24c5: a 5,000-revision import's record 3,560 refused `Unpublished`,
/// "profile … exceeds its region", then every `svn commit` refused E160004 "The repository's green is not at its
/// youngest revision"). Do: land revision 1; with the next publication refused before its append, commit revision 2;
/// then commit revision 2 again. Expect: the first attempt refused, the green's engine still at revision 1, where the
/// durable chain and the records are; the second attempt lands as revision 2. Before the fix the engine had committed
/// the version before its record was refused, and kept it: the green moved past the records for good.
#[test]
fn a_revision_refused_at_publication_leaves_the_green_where_the_log_is_and_the_next_lands() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    let first_landed = commit(state, (&owner(), svn), first(), None);
    let second = || {
      let rows = vec![file("/trunk/a.txt", Action::Modify, b"two\n")];
      revision(1, rows, &[b"two\n"])
    };
    state
      .db
      .inject_publication_fault(Some(slates_db::replay::PublicationFault::BeforeAppend));
    let refused = commit(state, (&owner(), svn), second(), None).is_err();
    let head_after = green_head(state, green);
    let again = commit(state, (&owner(), svn), second(), None);
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
/// `Record::Locks`; then make the repository's store again from its log. Expect: the step lands the entries; the
/// records hold alice's lock, the register holds the record as `locks`; the store made again from the log holds the
/// same lock, so a lock is as durable as a revision.
#[test]
fn a_lock_verbs_entries_land_through_the_step_and_a_rebuild_keeps_them() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let (svn, _green) = subversion(state, "svn");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    let records = state.vcs_stores.subversion_records(svn).unwrap();
    let request = slates_svn::lock::LockRequest {
      user: Some("alice".to_owned()),
      comment: Some(b"held".to_vec()),
      targets: BTreeMap::from([(
        "/trunk/a.txt".to_owned(),
        slates_svn::lock::LockTarget::default(),
      )]),
      ..slates_svn::lock::LockRequest::default()
    };
    let decision = records
      .locks()
      .lock(
        &request,
        &IndexHead(records.index()),
        AprTime(1),
        8,
        &mut || [7; 16],
      )
      .unwrap();
    let landed = super::record(state, (&owner(), svn), Record::Locks(decision.entries));
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
    let held = owner_of(state);
    let kinds: Vec<bool> = logged(state, svn)
      .iter()
      .map(|bytes| matches!(Record::decode(bytes), Ok(Record::Locks(_))))
      .collect();
    crate::vcs_store::rebuild(state, svn);
    crate::vcs_store::settle_rebuild(state, svn);
    (landed, held, kinds.last().copied(), owner_of(state))
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
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "submitted");
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let made = crate::vcs_store::paths::tests::create("/trunk/a.txt", b"alpha\n");
    let version = crate::vcs_store::paths::tests::version(state, ipc_green, 1, vec![made]);
    assert_eq!(version, 1);
    assert_eq!(kept(state, svn).0, 1, "the submit made no revision");
    let rows: Vec<(String, Action, NodeKind, bool)> = state
      .vcs_stores
      .subversion_records(svn)
      .and_then(|records| records.index().changes(1).ok().map(<[ChangedPath]>::to_vec))
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
    let author = state
      .vcs_stores
      .subversion_records(svn)
      .and_then(|records| records.revision_properties(1))
      .and_then(|properties| properties.get(b"svn:author".as_slice()).cloned());
    assert_eq!(
      author,
      Some(b"a-u-thor".to_vec()),
      "the principal's own name"
    );
    let rows = vec![file("/trunk/b.txt", Action::Add, b"beta\n")];
    let committed = commit(
      state,
      (&owner(), svn),
      revision(1, rows, &[b"beta\n"]),
      None,
    );
    assert_eq!(committed, answered(2));
    let seen = (
      green_head(state, green),
      green_file(state, green, "trunk/a.txt"),
      green_file(state, green, "trunk/b.txt"),
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
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "linked");
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let link = crate::vcs_store::paths::tests::Op::Declare(slates_ipc::protocol::WorkOp::Symlink {
      path: "/trunk/l".to_owned(),
      target: "a.txt".to_owned(),
    });
    let made = crate::vcs_store::paths::tests::create("/trunk/a.txt", b"alpha\n");
    crate::vcs_store::paths::tests::version(state, ipc_green, 1, vec![made, link]);
    let special = b"K 11\nsvn:special\nV 1\n*\nEND\n";
    let records = state
      .vcs_stores
      .subversion_records(svn)
      .expect("the records");
    let row = records
      .index()
      .changes(1)
      .ok()
      .and_then(|rows| rows.iter().find(|row| row.path == "/trunk/l").cloned());
    let seen = row.map(|row| (row.action, row.kind, row.text, row.properties));
    let expected = (
      Action::Add,
      NodeKind::File,
      Some(real_text(b"link a.txt")),
      Some(held(special)),
    );
    assert_eq!(seen, Some(expected));
    assert_eq!(records.content(&identity(special)), Some(&special[..]));
  });
}

/// A-664's Subversion objects (a copy; main's ruling: a work's copy as Subversion's copy-from row). Do: make a Subversion
/// repository; submit a work making `/trunk/a.txt` and the directory `/branches`; submit a work copying `/trunk` to
/// `/branches/b` from its base; then commit a client's revision on the youngest adding `/branches/b/c.txt`. Expect:
/// revision 2's one row adds `/branches/b`, a directory, copied from `/trunk` at revision 1; the records hold
/// `/branches/b/a.txt` at revision 2; the client's revision lands as revision 3.
#[test]
fn t_a664_svn_a_submitted_copy_is_a_copy_from_row() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "copied");
    let ipc_green = slates_ipc::protocol::VolumeId { bytes: green.bytes };
    let first = vec![
      crate::vcs_store::paths::tests::create("/trunk/a.txt", b"alpha\n"),
      crate::vcs_store::paths::tests::Op::Declare(slates_ipc::protocol::WorkOp::Mkdir {
        path: "/branches".to_owned(),
      }),
    ];
    crate::vcs_store::paths::tests::version(state, ipc_green, 1, first);
    let copy = crate::vcs_store::paths::tests::Op::Declare(slates_ipc::protocol::WorkOp::Copy {
      from: "/trunk".to_owned(),
      to: "/branches/b".to_owned(),
      version: None,
      replace: false,
    });
    assert_eq!(
      crate::vcs_store::paths::tests::version(state, ipc_green, 2, vec![copy]),
      2
    );
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
    let held = records
      .index()
      .node("/branches/b/a.txt", 2)
      .ok()
      .flatten()
      .is_some();
    assert!(held, "the copy brings /trunk/a.txt");
    let rows = vec![file("/branches/b/c.txt", Action::Add, b"gamma\n")];
    let committed = commit(
      state,
      (&owner(), svn),
      revision(2, rows, &[b"gamma\n"]),
      None,
    );
    assert_eq!(committed, answered(3));
  });
}

/// What a fetch's hold on `svn`'s records reads: their youngest revision and `/trunk/a.txt`'s text; none when they are
/// gone.
fn read_held(
  state: &mut ShardState,
  hold: &crate::vcs_store::svn_view::SvnHold,
) -> Option<(u64, Option<Vec<u8>>)> {
  use slates_svn::reads::Repository as _;
  crate::vcs_store::svn_view::lend_held_in(state, hold, |lent| {
    let text = lent
      .repository
      .text(&identity(b"alpha\n"), 0)
      .map(std::borrow::Cow::into_owned);
    (lent.index.youngest(), text)
  })
}

/// A-749 rule 4 (§4.17.1 "A fetch's answer": fetches run over the index generation they began on), for Subversion's
/// records. Do: commit revision 1; hold the records as a fetch's answer does when it begins; make the store again from
/// the log (a rebuild: the store freed, made empty and lent to nothing until its records are replayed, a turn each);
/// read through the hold while it is lent to nothing and again once it is whole; give the hold back. Expect: the hold
/// reads revision 1 and `/trunk/a.txt`'s text throughout; the records kept for it charged until it is given back, and
/// then the shard charged for the store made again alone, as before the rebuild; a hold taken after reads the store made
/// again. Red before the change: the hold read nothing once the store was freed.
#[test]
fn t_a745_a_fetchs_hold_reads_the_records_it_began_on_across_a_rebuild() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, _) = subversion(state, "held");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    let (_, _, charged) = kept(state, svn);
    let retained = state.store.budget.retained();
    let hold = crate::vcs_store::svn_view::hold_in(state, svn).expect("the records held");
    crate::vcs_store::rebuild(state, svn);
    let lent_to_nothing = state.vcs_stores.subversion_records(svn).is_none();
    let during = read_held(state, &hold);
    while crate::vcs_store::rebuild_turn_once(state, svn) {}
    let made_again = kept(state, svn);
    let both = state.store.budget.retained();
    let after = read_held(state, &hold);
    hold.give_back(state);
    let given_back = state.store.budget.retained();
    let again = crate::vcs_store::svn_view::hold_in(state, svn).expect("the store made again held");
    let fresh = read_held(state, &again);
    again.give_back(state);
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

/// S4 slice 2 c1 (the head's texts held by the green, `vcs_store::svn_texts`). Do: commit revision 1 (`/trunk/a.txt`,
/// `/trunk/sub/b.txt`); 2, `branched` (a copy of `/trunk` with `a.txt` changed beneath it and `sub` deleted); 3,
/// deleting `/trunk`; 4, replacing `/branches/b/a.txt` by another text; reading each text through the sources' text
/// source after each. Expect: after 1 the green holds both texts; after 2 those of `trunk` and the branch's changed
/// `a.txt`, each once though two rows reach it; after 3 the branch's alone; after 4 only the replacing text; each held
/// text read from the green, borrowed, and an earlier text still read from the records.
#[test]
fn t_s4_c1_the_heads_texts_are_held_by_the_green_and_read_through_it() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    let held = |state: &ShardState| {
      let engine = state.greens.get(&green).expect("the green");
      let texts: [&[u8]; 5] = [b"alpha\n", b"beta\n", b"alpha 2\n", b"gamma\n", b"never\n"];
      let each: Vec<bool> = texts
        .iter()
        .map(|bytes| engine.held(&identity(bytes)) == Some(*bytes))
        .collect();
      (engine.held_count(), each)
    };
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    assert_eq!(held(state), (2, vec![true, true, false, false, false]));
    assert_eq!(
      commit(state, (&owner(), svn), branched(), None),
      answered(2)
    );
    assert_eq!(held(state), (3, vec![true, true, true, false, false]));
    let deleted = revision(
      2,
      vec![row("/trunk", Action::Delete, NodeKind::Directory)],
      &[],
    );
    assert_eq!(commit(state, (&owner(), svn), deleted, None), answered(3));
    assert_eq!(held(state), (1, vec![false, false, true, false, false]));
    let replacing = revision(
      3,
      vec![file("/branches/b/a.txt", Action::Replace, b"gamma\n")],
      &[b"gamma\n"],
    );
    assert_eq!(commit(state, (&owner(), svn), replacing, None), answered(4));
    assert_eq!(held(state), (1, vec![false, false, false, true, false]));
    let engine = state.greens.get(&green).expect("the green");
    let records = state
      .vcs_stores
      .subversion_records(svn)
      .expect("the records");
    let texts = crate::vcs_store::svn_texts::HeadTexts {
      green: Some(&engine),
      records,
    };
    use slates_svn::migration::TextSource;
    assert!(matches!(
      texts.text(&identity(b"gamma\n")),
      Some(std::borrow::Cow::Borrowed(b"gamma\n"))
    ));
    assert_eq!(
      texts.text(&identity(b"alpha\n")).as_deref(),
      Some(&b"alpha\n"[..]),
      "an earlier text is read from the records"
    );
  });
}

/// S4 slice 2 c1 (a recovery's green holds the head's texts again, `svn_texts::rehold_head`). Do: commit revision 1 and
/// `branched`; let go of every text the green holds, as a green a recovery made again holds none; hold the head again,
/// twice. Expect: the head's three texts held again, each once, as the commits had left them; the second call holding
/// nothing more, since the green holds texts already.
#[test]
fn t_s4_c1_a_green_made_again_holds_its_heads_texts_again_in_one_order() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    assert_eq!(commit(state, (&owner(), svn), first(), None), answered(1));
    assert_eq!(
      commit(state, (&owner(), svn), branched(), None),
      answered(2)
    );
    let texts: [&[u8]; 3] = [b"alpha\n", b"beta\n", b"alpha 2\n"];
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
      .map(|bytes| engine.held(&identity(bytes)) == Some(*bytes))
      .collect();
    assert_eq!((engine.held_count(), held), (3, vec![true, true, true]));
  });
}
