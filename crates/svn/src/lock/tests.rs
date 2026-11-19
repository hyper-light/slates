//! The lock table's tests (A-602; §4.17.4 item 7, item 3.4 and piece 8): Subversion's lock rules case by case, each
//! citing the function it holds the table to (Subversion `1.14.x` at `d434dc14`: `libsvn_fs_fs/lock.c`,
//! `libsvn_fs/fs-loader.c`, `libsvn_repos/fs-wrap.c`, `libsvn_fs_fs/transaction.c`, `svnserve/serve.c`); and the
//! oracle: those functions transcribed into a model that changes its locks in place, as FSFS changes its digest files,
//! which the table, guarded then applied, must equal on generated histories.

use super::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt::Debug;

/// The owner's clock in the case tests: a moment of 2023, in microseconds.
const NOW: AprTime = AprTime(1_700_000_000_000_000);

/// All the room there is, for the tests that are not about the bound.
const ALL_ROOM: usize = usize::MAX;

/// A principal.
const ALICE: &str = "alice";

/// Another principal.
const BOB: &str = "bob";

/// A token no lock holds.
const GARBAGE: &str = "opaquelocktoken:garbage";

/// A head for the case tests at revision 3: directories `/a` and `/b`; files `/a/f` (created in 2), `/a/g` (3), `/b/f`
/// (1) and `/c` (1).
fn head() -> Head {
  let mut head = Head {
    youngest: 3,
    nodes: BTreeMap::new(),
  };
  for (path, kind, created) in [
    ("/", NodeKind::Directory, 3),
    ("/a", NodeKind::Directory, 3),
    ("/b", NodeKind::Directory, 1),
    ("/a/f", NodeKind::File, 2),
    ("/a/g", NodeKind::File, 3),
    ("/b/f", NodeKind::File, 1),
    ("/c", NodeKind::File, 1),
  ] {
    head.nodes.insert(path.to_owned(), (kind, created));
  }
  head
}

/// A head the tests keep: the youngest revision and each path's node.
#[derive(Clone, Debug, Default)]
struct Head {
  youngest: Revision,
  nodes: BTreeMap<String, (NodeKind, Revision)>,
}

impl HeadView for Head {
  fn youngest(&self) -> Revision {
    self.youngest
  }

  fn node(&self, path: &str) -> Option<(NodeKind, Revision)> {
    self.nodes.get(path).copied()
  }
}

/// Sixteen bytes from a count of draws: what the tests' random source gives.
fn draw_from(count: &mut u64) -> [u8; UUID_BYTES] {
  *count = count.wrapping_add(1);
  let half = count.to_le_bytes();
  let mut bytes = [0; UUID_BYTES];
  for (at, byte) in bytes.iter_mut().enumerate() {
    *byte = half[at % half.len()];
  }
  bytes
}

/// A table with its head and its random source, as an owner keeps them.
struct Owner {
  table: LockTable,
  head: Head,
  drawn: u64,
}

impl Owner {
  /// A new repository's owner over [`head`].
  fn new() -> Self {
    Self {
      table: LockTable::new(),
      head: head(),
      drawn: 0,
    }
  }

  /// Runs a lock request at `now` and applies its entries: each path's answer.
  fn lock(&mut self, request: &LockRequest, now: AprTime) -> Vec<(String, LockAnswer)> {
    self.lock_within(request, now, ALL_ROOM)
  }

  /// [`Owner::lock`] within `room`.
  fn lock_within(
    &mut self,
    request: &LockRequest,
    now: AprTime,
    room: usize,
  ) -> Vec<(String, LockAnswer)> {
    let drawn = &mut self.drawn;
    let decision = self
      .table
      .lock(request, &self.head, now, room, &mut || draw_from(drawn))
      .unwrap();
    self.apply(&decision.entries);
    decision.answers
  }

  /// Runs an unlock request at `now` and applies its entries: each path's answer.
  fn unlock(&mut self, request: &UnlockRequest, now: AprTime) -> Vec<(String, UnlockAnswer)> {
    let decision = self.table.unlock(request, now).unwrap();
    self.apply(&decision.entries);
    decision.answers
  }

  /// Runs the verdict's check at `now` and, accepted, applies its entries.
  fn commit(
    &mut self,
    changes: &[(&str, Action)],
    commit: &CommitLocks,
    now: AprTime,
  ) -> Result<Vec<LockEntry>, LockRefusal> {
    let entries = self.table.verdict(changes.iter().copied(), commit, now)?;
    self.apply(&entries);
    Ok(entries)
  }

  /// Applies `entries` in order.
  fn apply(&mut self, entries: &[LockEntry]) {
    for entry in entries {
      self.table.apply(entry).unwrap();
    }
  }

  /// The token of the lock at `path`, expired or not.
  fn token(&self, path: &str) -> String {
    let lock = self.table.locks().find(|lock| lock.path == path).unwrap();
    lock.token.as_str().to_owned()
  }
}

/// A request by `user` to lock `paths`, naming no revision, supplying no token, with no comment and no expiry.
fn locking(user: &str, paths: &[&str]) -> LockRequest {
  LockRequest {
    user: Some(user.to_owned()),
    targets: paths
      .iter()
      .map(|path| ((*path).to_owned(), LockTarget::default()))
      .collect(),
    ..LockRequest::default()
  }
}

/// A request by `user` to unlock each path with its token, or to break them.
fn unlocking(user: &str, targets: &[(&str, Option<&str>)], break_lock: bool) -> UnlockRequest {
  UnlockRequest {
    user: Some(user.to_owned()),
    break_lock,
    targets: targets
      .iter()
      .map(|(path, token)| ((*path).to_owned(), token.map(str::to_owned)))
      .collect(),
  }
}

/// A commit by `user` presenting `presented`, keeping its locks or not.
fn committing(user: &str, presented: &[(&str, &str)], keep: bool) -> CommitLocks {
  CommitLocks {
    user: Some(user.to_owned()),
    presented: presented
      .iter()
      .map(|(path, token)| ((*path).to_owned(), (*token).to_owned()))
      .collect(),
    keep,
  }
}

/// Each path with the code of its refusal, or `None` for a lock made.
fn codes<T>(answers: &[(String, Result<T, LockRefusal>)]) -> Vec<(&str, Option<u32>)> {
  answers
    .iter()
    .map(|(path, answer)| {
      let code = answer.as_ref().err().map(LockRefusal::subversion_code);
      (path.as_str(), code)
    })
    .collect()
}

/// A-602 (§4.17.4 item 7; `lock.c` `lock_body`): do lock a file; expect one lock with a token drawn from the owner's
/// random source, the principal as its owner, the comment, the owner's time as its creation and the expiry asked, the
/// same lock answered, set by one entry, and read back by `get-lock`.
#[test]
fn a_file_is_locked_with_a_drawn_token_by_its_principal_at_the_owners_time() {
  let mut owner = Owner::new();
  let mut request = locking(ALICE, &["/a/f"]);
  request.comment = Some(b"editing".to_vec());
  request.expires = Some(AprTime(NOW.0 + 60));
  let answers = owner.lock(&request, NOW);
  let expected = Lock {
    path: "/a/f".to_owned(),
    token: LockToken::drawn(draw_from(&mut 0)),
    owner: ALICE.to_owned(),
    comment: Some("editing".to_owned()),
    is_dav_comment: false,
    created: NOW,
    expires: Some(AprTime(NOW.0 + 60)),
  };
  assert_eq!(answers, vec![("/a/f".to_owned(), Ok(expected.clone()))]);
  assert_eq!(owner.table.get_lock("/a/f", NOW), Some(&expected));
}

/// A-602 (`svn_fs_fs__generate_lock_token`): do make a token from sixteen bytes; expect `opaquelocktoken:` and the
/// bytes as a version 4 UUID, its version and variant bits set, in lower-case hex grouped 8-4-4-4-12.
#[test]
fn a_drawn_token_is_an_opaquelocktoken_holding_a_random_uuid() {
  assert_eq!(
    LockToken::drawn([0x01; UUID_BYTES]).as_str(),
    "opaquelocktoken:01010101-0101-4101-8101-010101010101"
  );
  assert_eq!(
    LockToken::drawn([0xff; UUID_BYTES]).as_str(),
    "opaquelocktoken:ffffffff-ffff-4fff-bfff-ffffffffffff"
  );
}

/// A-602 (`lock.c` `check_lock`): do lock a directory, a path the head lacks without a revision and with one; expect
/// `E160017` for the directory ("only a file is locked"), `E160013` without a revision and `E160042` with one ("Path
/// '%s' doesn't exist in HEAD revision"), and no lock made.
#[test]
fn a_directory_is_not_a_file_and_a_path_the_head_lacks_is_not_found_or_out_of_date() {
  let mut owner = Owner::new();
  let mut request = locking(ALICE, &["/a", "/nothing"]);
  let answers = owner.lock(&request, NOW);
  assert_eq!(
    codes(&answers),
    vec![("/a", Some(160_017)), ("/nothing", Some(160_013))]
  );
  request.targets.insert(
    "/nothing".to_owned(),
    LockTarget {
      current: Some(3),
      token: None,
    },
  );
  let answers = owner.lock(&request, NOW);
  assert_eq!(
    codes(&answers),
    vec![("/a", Some(160_017)), ("/nothing", Some(160_042))]
  );
  assert_eq!(owner.table.locks().count(), 0);
}

/// A-602 (`check_lock`): do lock `/a/f` (created in 2, the head at 3) naming revision 4, then 1, then 2 and 3; expect
/// `E160006` past the head, `E160042` before the file's last change ("Lock failed: newer version of '%s' exists"), and
/// a lock at 2 or later.
#[test]
fn a_revision_past_the_head_is_no_such_revision_and_one_before_the_files_change_out_of_date() {
  let mut owner = Owner::new();
  let mut answers = Vec::new();
  for current in [4, 1, 2, 3] {
    let mut request = locking(ALICE, &["/a/f"]);
    request.steal = true;
    request.targets.insert(
      "/a/f".to_owned(),
      LockTarget {
        current: Some(current),
        token: None,
      },
    );
    answers.extend(owner.lock(&request, NOW));
  }
  assert_eq!(
    codes(&answers),
    vec![
      ("/a/f", Some(160_006)),
      ("/a/f", Some(160_042)),
      ("/a/f", None),
      ("/a/f", None)
    ]
  );
}

/// A-602 (`check_lock`, "Sorry, the path is already locked"): do lock a file Alice holds, as Bob and as Alice, then as
/// Bob stealing it; expect `E160035` both times, whoever asks, then Bob's lock with a new token in place of Alice's.
#[test]
fn a_locked_file_is_refused_unless_the_request_steals_it() {
  let mut owner = Owner::new();
  owner.lock(&locking(ALICE, &["/c"]), NOW);
  let alices = owner.token("/c");
  let refused = [
    owner.lock(&locking(BOB, &["/c"]), NOW),
    owner.lock(&locking(ALICE, &["/c"]), NOW),
  ];
  assert!(
    refused
      .iter()
      .all(|answers| codes(answers) == vec![("/c", Some(160_035))])
  );
  let mut stealing = locking(BOB, &["/c"]);
  stealing.steal = true;
  assert_eq!(codes(&owner.lock(&stealing, NOW)), vec![("/c", None)]);
  let stolen = owner.table.get_lock("/c", NOW).unwrap();
  assert_eq!(
    (stolen.owner.as_str(), stolen.token.as_str() == alices),
    (BOB, false)
  );
}

/// A-602 (`fs-wrap.c` `svn_repos_fs_lock_many`; `lock.c` `svn_fs_fs__unlock`, "We need to have a username attached to
/// the fs"): do lock, unlock and break with no principal; expect each refused whole `E160034`, a break too.
#[test]
fn a_request_without_a_principal_is_refused_whole_even_a_break() {
  let mut owner = Owner::new();
  owner.lock(&locking(ALICE, &["/c"]), NOW);
  let mut request = locking(ALICE, &["/a/f"]);
  request.user = None;
  let none = Some(LockRefusal::NoUser { path: None });
  assert_eq!(
    owner
      .table
      .lock(&request, &owner.head, NOW, ALL_ROOM, &mut || [0;
        UUID_BYTES])
      .err(),
    none
  );
  let mut breaking = unlocking(ALICE, &[("/c", None)], true);
  breaking.user = None;
  assert_eq!(owner.table.unlock(&breaking, NOW).err(), none);
}

/// A-602 (`fs-loader.c` `svn_fs_lock_many`, "Lock comment contains illegal characters"; `xml.c`
/// `svn_xml_is_xml_safe`): do lock with comments holding a bell, a NUL and bytes that are not UTF-8, and one holding a
/// tab, a line feed, a carriage return and a delete; expect the first three refused whole `E130004` and the last kept
/// as written.
#[test]
fn a_comment_must_be_one_xml_carries() {
  let mut owner = Owner::new();
  let mut request = locking(ALICE, &["/c"]);
  for refused in [&b"ring \x07"[..], b"cut \x00 here", b"\xff\xfe"] {
    request.comment = Some(refused.to_vec());
    let answer = owner
      .table
      .lock(&request, &owner.head, NOW, ALL_ROOM, &mut || {
        [0; UUID_BYTES]
      });
    assert_eq!(answer.err(), Some(LockRefusal::UnescapableComment));
  }
  request.comment = Some(b"tab\there\nline\rreturn\x7f".to_vec());
  let answers = owner.lock(&request, NOW);
  let comment = answers
    .first()
    .and_then(|(_, answer)| answer.as_ref().ok()?.comment.clone());
  assert_eq!(comment.as_deref(), Some("tab\there\nline\rreturn\x7f"));
}

/// A-602 (`svn_fs_lock_many`, "Negative expiration date passed to svn_fs_lock"; FSFS keeps 0 for none): do lock with
/// an expiry before the epoch, then with zero; expect `E200004` refused whole, then a lock that never expires.
#[test]
fn an_expiry_before_the_epoch_is_refused_and_zero_is_none() {
  let mut owner = Owner::new();
  let mut request = locking(ALICE, &["/c"]);
  request.expires = Some(AprTime(-1));
  let refused = owner
    .table
    .lock(&request, &owner.head, NOW, ALL_ROOM, &mut || {
      [0; UUID_BYTES]
    });
  assert_eq!(
    refused.err().map(|refusal| refusal.subversion_code()),
    Some(200_004)
  );
  request.expires = Some(AprTime(0));
  owner.lock(&request, NOW);
  let made = owner
    .table
    .get_lock("/c", AprTime(i64::MAX))
    .map(|lock| lock.expires);
  assert_eq!(made, Some(None));
}

/// A-602 (`svn_fs_lock_many`: "has bad scheme; expected 'opaquelocktoken'", "is not ASCII or is a control
/// character"): do lock with supplied tokens of another scheme, holding a control character and holding a byte past
/// ASCII, then a good one; expect `E160037` for each bad one before any other check, even on a directory, and the good
/// one kept as the lock's token.
#[test]
fn a_supplied_token_must_be_an_opaquelocktoken_of_printable_ascii() {
  let mut owner = Owner::new();
  for bad in [
    "urn:uuid:1234",
    "opaquelocktoken:bell\x07",
    "opaquelocktoken:caf\u{e9}",
  ] {
    let mut request = locking(ALICE, &["/a"]);
    request.targets.insert(
      "/a".to_owned(),
      LockTarget {
        current: None,
        token: Some(bad.to_owned()),
      },
    );
    assert_eq!(
      codes(&owner.lock(&request, NOW)),
      vec![("/a", Some(160_037))]
    );
  }
  let mut request = locking(ALICE, &["/c"]);
  let supplied = "opaquelocktoken:from-a-dav-refresh";
  request.targets.insert(
    "/c".to_owned(),
    LockTarget {
      current: None,
      token: Some(supplied.to_owned()),
    },
  );
  owner.lock(&request, NOW);
  assert_eq!(owner.token("/c"), supplied);
}

/// A-602 (`lock.c` `check_unlock`): do unlock Alice's lock as Alice with no token and a wrong one, as Bob with its
/// token, then as Alice with it; then break Bob's lock as Alice with no token; then unlock a path with no lock. Expect
/// `E160040` without the token, `E160039` for Bob, the lock removed for Alice, the break removing Bob's lock, and
/// `E160040` where there is none.
#[test]
fn an_unlock_needs_the_locks_token_and_its_owner_and_a_break_needs_neither() {
  let mut owner = Owner::new();
  owner.lock(&locking(ALICE, &["/a/f"]), NOW);
  owner.lock(&locking(BOB, &["/b/f"]), NOW);
  let token = owner.token("/a/f");
  let mut answers = Vec::new();
  for (user, sent) in [
    (ALICE, None),
    (ALICE, Some(GARBAGE)),
    (BOB, Some(token.as_str())),
    (ALICE, Some(token.as_str())),
  ] {
    answers.extend(owner.unlock(&unlocking(user, &[("/a/f", sent)], false), NOW));
  }
  answers.extend(owner.unlock(&unlocking(ALICE, &[("/b/f", None)], true), NOW));
  answers.extend(owner.unlock(&unlocking(ALICE, &[("/c", None)], true), NOW));
  let expected = vec![
    ("/a/f", Some(160_040)),
    ("/a/f", Some(160_040)),
    ("/a/f", Some(160_039)),
    ("/a/f", None),
    ("/b/f", None),
    ("/c", Some(160_040)),
  ];
  assert_eq!(codes(&answers), expected);
  assert_eq!(owner.table.locks().count(), 0);
}

/// A-602 (`lock.c` `lock_expired`, `get_lock`, `get_lock_helper`): do let Alice's lock expire, then read it, lock the
/// path as Bob without stealing, and on another expired lock unlock twice. Expect the read to show nothing (and the
/// table to keep it), Bob's lock made, the first unlock `E160041` removing the lock, and the second `E160040`.
#[test]
fn an_expired_lock_is_no_lock_and_an_unlock_that_meets_it_removes_it() {
  let mut owner = Owner::new();
  let mut request = locking(ALICE, &["/a/f", "/c"]);
  request.expires = Some(AprTime(NOW.0 + 10));
  owner.lock(&request, NOW);
  let later = AprTime(NOW.0 + 11);
  assert_eq!(
    (
      owner.table.get_lock("/a/f", later),
      owner.table.locks().count()
    ),
    (None, 2)
  );
  assert_eq!(
    codes(&owner.lock(&locking(BOB, &["/a/f"]), later)),
    vec![("/a/f", None)]
  );
  let token = owner.token("/c");
  let first = owner.unlock(
    &unlocking(ALICE, &[("/c", Some(token.as_str()))], false),
    later,
  );
  let second = owner.unlock(
    &unlocking(ALICE, &[("/c", Some(token.as_str()))], false),
    later,
  );
  assert_eq!(codes(&first), vec![("/c", Some(160_041))]);
  assert_eq!(codes(&second), vec![("/c", Some(160_040))]);
}

/// A-602 (§4.17.4 item 3.4; `lock.c` `verify_lock`): do commit a change to Alice's locked file as Bob presenting her
/// token, as Alice presenting none, as Alice presenting it under another path, and with no principal. Expect `E160039`
/// for Bob (the owner is checked before the token), `E160037` for no token, an accept with the token presented
/// anywhere, and `E160034` without a principal.
#[test]
fn the_verdict_refuses_another_principals_lock_before_a_missing_token() {
  let mut owner = Owner::new();
  owner.lock(&locking(ALICE, &["/a/f"]), NOW);
  let token = owner.token("/a/f");
  let modify = [("/a/f", Action::Modify)];
  let mut anonymous = committing(ALICE, &[], true);
  anonymous.user = None;
  let outcomes = [
    owner.commit(&modify, &committing(BOB, &[("/a/f", &token)], true), NOW),
    owner.commit(&modify, &committing(ALICE, &[], true), NOW),
    owner.commit(
      &modify,
      &committing(ALICE, &[("/elsewhere", &token)], true),
      NOW,
    ),
    owner.commit(&modify, &anonymous, NOW),
  ];
  let found: Vec<Option<u32>> = outcomes
    .iter()
    .map(|outcome| outcome.as_ref().err().map(LockRefusal::subversion_code))
    .collect();
  assert_eq!(
    found,
    vec![Some(160_039), Some(160_037), None, Some(160_034)]
  );
}

/// A-602 (`transaction.c` `verify_locks`: "For paths being added or removed, we need to hold the locks for that path
/// and any children of that path"): with Bob's lock on `/a/f`, do commit as Alice a delete, a replacement and an
/// addition of `/a`, a modification of `/a` and of `/a/g`, and a delete of `/a-b`, which sorts between `/a` and `/a/f`.
/// Expect the delete, the replacement and the addition refused `E160039`, and the modifications and the sibling
/// accepted.
#[test]
fn the_verdict_meets_every_lock_beneath_a_path_added_deleted_or_replaced_and_only_its_own_for_a_modify()
 {
  let mut owner = Owner::new();
  owner.lock(&locking(BOB, &["/a/f"]), NOW);
  let alice = committing(ALICE, &[], true);
  let changes = [
    ("/a", Action::Delete),
    ("/a", Action::Replace),
    ("/a", Action::Add),
    ("/a", Action::Modify),
    ("/a/g", Action::Modify),
    ("/a-b", Action::Delete),
  ];
  let found: Vec<Option<u32>> = changes
    .iter()
    .map(|change| {
      owner
        .commit(&[*change], &alice, NOW)
        .err()
        .map(|refusal| refusal.subversion_code())
    })
    .collect();
  assert_eq!(
    found,
    vec![
      Some(160_039),
      Some(160_039),
      Some(160_039),
      None,
      None,
      None
    ]
  );
}

/// A-602 (`serve.c` `commit`: "if (! keep_locks && lock_tokens && lock_tokens->nelts) unlock_paths"): with Alice
/// holding `/a/f` and `/c` and Bob `/b/f`, do commit as Alice a change to `/a/f` presenting all three tokens and a
/// garbage one, keeping her locks, then the same releasing them. Expect the keep to leave all three; the release to
/// remove Alice's two, `/c` too though the commit did not change it, and to leave Bob's, whose owner is not the
/// committer, though its token was presented.
#[test]
fn an_accepted_commit_releases_every_presented_lock_it_may_unless_it_keeps_them() {
  let mut owner = Owner::new();
  owner.lock(&locking(ALICE, &["/a/f", "/c"]), NOW);
  owner.lock(&locking(BOB, &["/b/f"]), NOW);
  let presented: Vec<(String, String)> = ["/a/f", "/b/f", "/c"]
    .into_iter()
    .map(|path| (path.to_owned(), owner.token(path)))
    .chain([("/z".to_owned(), GARBAGE.to_owned())])
    .collect();
  let presented: Vec<(&str, &str)> = presented
    .iter()
    .map(|(path, token)| (path.as_str(), token.as_str()))
    .collect();
  let change = [("/a/f", Action::Modify)];
  assert_eq!(
    owner.commit(&change, &committing(ALICE, &presented, true), NOW),
    Ok(Vec::new())
  );
  assert_eq!(owner.table.locks().count(), 3);
  owner
    .commit(&change, &committing(ALICE, &presented, false), NOW)
    .unwrap();
  let left: Vec<&str> = owner.table.locks().map(|lock| lock.path.as_str()).collect();
  assert_eq!(left, vec!["/b/f"]);
}

/// A-602 (§4.17.4 item 7: a refused commit records nothing; `walk_locks` removes an expired lock it meets under the
/// write lock): with Alice's expired lock on `/a/f` and Bob's live one on `/a/g`, do commit as Alice a delete of `/a`,
/// then of `/a/f` alone. Expect the first refused `E160039` with nothing removed, and the second accepted, its one
/// entry removing the expired lock.
#[test]
fn a_refused_commit_records_nothing_and_an_accepted_one_removes_the_expired_locks_it_met() {
  let mut owner = Owner::new();
  let mut expiring = locking(ALICE, &["/a/f"]);
  expiring.expires = Some(AprTime(NOW.0 + 1));
  owner.lock(&expiring, NOW);
  owner.lock(&locking(BOB, &["/a/g"]), NOW);
  let later = AprTime(NOW.0 + 2);
  let alice = committing(ALICE, &[], true);
  let refused = owner.commit(&[("/a", Action::Delete)], &alice, later);
  assert_eq!(
    refused.err().map(|refusal| refusal.subversion_code()),
    Some(160_039)
  );
  assert_eq!(owner.table.locks().count(), 2);
  let entries = owner
    .commit(&[("/a/f", Action::Delete)], &alice, later)
    .unwrap();
  let removed: Vec<&LockEntry> = entries
    .iter()
    .filter(|entry| matches!(entry, LockEntry::Removed { path, .. } if path == "/a/f"))
    .collect();
  assert_eq!(
    (entries.len(), removed.len(), owner.table.locks().count()),
    (1, 1, 1)
  );
}

/// A-602 (`lock.c` `get_locks_filter_func`): with locks on `/a/f`, `/a/g` (expired), `/a-b`, `/b/f` and a lock left on
/// `/a/d/h`, do ask for the locks of `/a` at each depth and of `/` at infinity. Expect nothing at empty (a directory
/// holds no lock), `/a/f` alone at files and immediates (`/a/g` expired, `/a/d/h` two levels down, `/a-b` not beneath
/// `/a`), `/a/d/h` and `/a/f` at infinity, and every live lock at the root.
#[test]
fn get_locks_reaches_as_deep_as_its_depth_and_shows_no_expired_lock() {
  let mut owner = Owner::new();
  owner
    .head
    .nodes
    .insert("/a/d/h".to_owned(), (NodeKind::File, 3));
  owner
    .head
    .nodes
    .insert("/a-b".to_owned(), (NodeKind::File, 3));
  owner.lock(&locking(ALICE, &["/a/f", "/a-b", "/b/f", "/a/d/h"]), NOW);
  let mut expiring = locking(BOB, &["/a/g"]);
  expiring.expires = Some(AprTime(NOW.0 + 1));
  owner.lock(&expiring, NOW);
  let later = AprTime(NOW.0 + 2);
  let paths = |path: &str, depth: Depth| -> Vec<String> {
    owner
      .table
      .get_locks(path, depth, later)
      .iter()
      .map(|lock| lock.path.clone())
      .collect()
  };
  assert_eq!(paths("/a", Depth::Empty), Vec::<String>::new());
  assert_eq!(
    (paths("/a", Depth::Files), paths("/a", Depth::Immediates)),
    (vec!["/a/f".to_owned()], vec!["/a/f".to_owned()])
  );
  assert_eq!(
    paths("/a", Depth::Infinity),
    vec!["/a/d/h".to_owned(), "/a/f".to_owned()]
  );
  assert_eq!(
    paths("/", Depth::Infinity),
    vec!["/a-b", "/a/d/h", "/a/f", "/b/f"]
  );
}

/// A-602 (§4.17.4 item 7, the room): with a room of two, Alice's expired lock on `/a/f` and Bob's live one on `/b/f`,
/// do lock `/a/g` and `/c` as Alice. Expect the expired lock let go first by its own entry, `/a/g` made, `/c` refused
/// `E160000` with no token drawn for it, and two locks held.
#[test]
fn past_its_room_the_table_lets_go_of_expired_locks_before_refusing_a_new_one() {
  let mut owner = Owner::new();
  let mut expiring = locking(ALICE, &["/a/f"]);
  expiring.expires = Some(AprTime(NOW.0 + 1));
  owner.lock(&expiring, NOW);
  owner.lock(&locking(BOB, &["/b/f"]), NOW);
  let later = AprTime(NOW.0 + 2);
  let drawn_before = owner.drawn;
  let answers = owner.lock_within(&locking(ALICE, &["/a/g", "/c"]), later, 2);
  assert_eq!(codes(&answers), vec![("/a/g", None), ("/c", Some(160_000))]);
  let held: Vec<&str> = owner.table.locks().map(|lock| lock.path.as_str()).collect();
  assert_eq!(
    (held, owner.drawn - drawn_before),
    (vec!["/a/g", "/b/f"], 1)
  );
}

/// A-602 (guarded, then applied): do apply an entry removing a lock the table does not hold, and one naming a lock by a
/// token it does not hold; expect each refused `E160004`, the table unchanged.
#[test]
fn apply_refuses_to_remove_a_lock_the_table_does_not_hold() {
  let mut owner = Owner::new();
  owner.lock(&locking(ALICE, &["/c"]), NOW);
  let before = owner.table.clone();
  for (path, token) in [("/a/f", owner.token("/c")), ("/c", GARBAGE.to_owned())] {
    let entry = LockEntry::Removed {
      path: path.to_owned(),
      token: LockToken(token),
    };
    assert_eq!(
      owner
        .table
        .apply(&entry)
        .map_err(|refusal| refusal.subversion_code()),
      Err(160_004)
    );
  }
  assert_eq!(owner.table, before);
}

/// A-602 (the clock seam): do convert a reading of `Clock::wall_ns`; expect its whole microseconds, as `apr_time_t`
/// keeps them.
#[test]
fn the_clock_seams_nanoseconds_are_apr_microseconds() {
  assert_eq!(
    AprTime::from_wall_ns(1_700_000_000_123_456_789),
    AprTime(1_700_000_000_123_456)
  );
  assert_eq!(AprTime::from_wall_ns(999), AprTime(0));
}

/// A-602 (`lock.c` `lock_body`: every path checked, then each that passed locked, its token generated, in path
/// order): do lock four paths, one a directory and one supplying a token; expect every path answered, a token drawn
/// only for the two locks it made without one, in path order.
#[test]
fn a_lock_many_draws_a_token_only_for_each_lock_it_makes_in_path_order() {
  let mut owner = Owner::new();
  let mut request = locking(ALICE, &["/c", "/a", "/b/f", "/a/f"]);
  request.targets.insert(
    "/b/f".to_owned(),
    LockTarget {
      current: None,
      token: Some("opaquelocktoken:supplied".to_owned()),
    },
  );
  let answers = owner.lock(&request, NOW);
  let tokens: Vec<(&str, Option<&str>)> = answers
    .iter()
    .map(|(path, answer)| {
      (
        path.as_str(),
        answer.as_ref().ok().map(|lock| lock.token.as_str()),
      )
    })
    .collect();
  let first = LockToken::drawn(draw_from(&mut 0));
  let second = LockToken::drawn(draw_from(&mut 1));
  let expected = vec![
    ("/a", None),
    ("/a/f", Some(first.as_str())),
    ("/b/f", Some("opaquelocktoken:supplied")),
    ("/c", Some(second.as_str())),
  ];
  assert_eq!(tokens, expected);
}

// ── The oracle: FSFS's lock rules, transcribed ───────────────────────────────────────────────────────────────────────

/// FSFS's lock rules, transcribed from Subversion `1.14.x` at `d434dc14`: `libsvn_fs_fs/lock.c`,
/// `libsvn_fs/fs-loader.c` `svn_fs_lock_many`, `libsvn_repos/fs-wrap.c`'s username checks, `libsvn_fs_fs/transaction.c`
/// `verify_locks` and `svnserve/serve.c` `commit`; with slates' rules from §4.17.4 item 7 (canonical paths, the room, a
/// refused commit recording nothing, a NUL refused). Its map is FSFS's digest files, one lock a path; each method is
/// one FSFS call under the repository's write lock, changing the map as it goes, as FSFS changes its files.
#[derive(Clone, Debug, Default)]
struct Fsfs {
  digests: BTreeMap<String, Lock>,
}

/// What a lock request gives every lock it makes (`lock_body`'s second loop).
struct Making<'r> {
  username: &'r str,
  comment: Option<String>,
  is_dav_comment: bool,
  expiration_date: i64,
  room: usize,
  now: AprTime,
}

/// What `verify_locks` found: the locks that could refuse the commit, each its path and code, and the expired locks it
/// met.
#[derive(Default)]
struct Verified {
  failures: BTreeSet<(String, u32)>,
  expired: BTreeSet<String>,
}

impl Fsfs {
  /// `svn_repos_fs_lock_many`, `svn_fs_lock_many`, `svn_fs_fs__lock` and `lock_body`.
  fn lock_many(
    &mut self,
    request: &LockRequest,
    head: &Head,
    (now, room): (AprTime, usize),
    draw: &mut impl FnMut() -> [u8; UUID_BYTES],
  ) -> Result<Vec<(String, LockAnswer)>, LockRefusal> {
    // fs-wrap.c 614: "Cannot lock path, no authenticated username available."
    let username = request
      .user
      .as_deref()
      .ok_or(LockRefusal::NoUser { path: None })?;
    // fs-loader.c 1832-1839: the comment must be XML-safe, the expiration not negative.
    let comment = match &request.comment {
      Some(bytes) if !is_xml_safe(bytes) => return Err(LockRefusal::UnescapableComment),
      Some(bytes) => Some(String::from_utf8(bytes.clone()).unwrap()),
      None => None,
    };
    let expiration_date = request.expires.map_or(0, |expires| expires.0);
    if expiration_date < 0 {
      return Err(LockRefusal::NegativeExpiry {
        expires: AprTime(expiration_date),
      });
    }
    // slates (§4.17.4 item 7): a request that could take the table past its room lets go of every expired lock first.
    if self.digests.len() + request.targets.len() > room {
      self.digests.retain(|_, lock| !lock_expired(lock, now));
    }
    // lock_body's first loop: check_lock for every path, in path order.
    let checks: Vec<(&String, &LockTarget, Result<(), LockRefusal>)> = request
      .targets
      .iter()
      .map(|(path, target)| {
        (
          path,
          target,
          self.check_lock(path, target, request.steal, head, now),
        )
      })
      .collect();
    // Its second loop: each path that passed is locked.
    let making = Making {
      username,
      comment,
      is_dav_comment: request.is_dav_comment,
      expiration_date,
      room,
      now,
    };
    Ok(
      checks
        .into_iter()
        .map(|(path, target, checked)| {
          (
            path.clone(),
            checked.and_then(|()| self.set_lock(path, target, &making, draw)),
          )
        })
        .collect(),
    )
  }

  /// `fs-loader.c`'s token check, then `lock.c` `check_lock`, after slates' canonical path.
  fn check_lock(
    &mut self,
    path: &str,
    target: &LockTarget,
    steal_lock: bool,
    head: &Head,
    now: AprTime,
  ) -> Result<(), LockRefusal> {
    if !canonical(path) {
      return Err(LockRefusal::PathSyntax {
        path: path.to_owned(),
      });
    }
    if let Some(token) = &target.token {
      check_token(token)?;
    }
    let created_rev = check_path(path, target, head)?;
    if let Some(current_rev) = target.current {
      if current_rev > head.youngest {
        return Err(LockRefusal::NoSuchRevision {
          revision: current_rev,
        });
      }
      if current_rev < created_rev {
        return Err(LockRefusal::OutOfDate {
          path: path.to_owned(),
        });
      }
    }
    // get_lock_helper under the write lock: an expired lock is removed, and none.
    match self.get_lock_helper(path, now) {
      Some(existing_lock) if !steal_lock => Err(LockRefusal::AlreadyLocked {
        path: path.to_owned(),
        owner: existing_lock.owner,
      }),
      _ => Ok(()),
    }
  }

  /// `lock_body`'s second loop for one path, after slates' room: the lock, its token supplied or generated.
  fn set_lock(
    &mut self,
    path: &str,
    target: &LockTarget,
    making: &Making<'_>,
    draw: &mut impl FnMut() -> [u8; UUID_BYTES],
  ) -> LockAnswer {
    if !self.digests.contains_key(path) && self.digests.len() >= making.room {
      return Err(LockRefusal::Room { room: making.room });
    }
    let token = match &target.token {
      Some(token) => token.clone(),
      None => generate_lock_token(draw()),
    };
    let lock = Lock {
      path: path.to_owned(),
      token: LockToken(token),
      owner: making.username.to_owned(),
      comment: making.comment.clone(),
      is_dav_comment: making.is_dav_comment,
      created: making.now,
      expires: (making.expiration_date != 0).then_some(AprTime(making.expiration_date)),
    };
    self.digests.insert(path.to_owned(), lock.clone());
    Ok(lock)
  }

  /// `svn_fs_fs__unlock` and `unlock_body`: every path checked in path order (`check_unlock`, which removes an expired
  /// lock it meets), then the locks that passed deleted.
  fn unlock_many(
    &mut self,
    request: &UnlockRequest,
    now: AprTime,
  ) -> Result<Vec<(String, UnlockAnswer)>, LockRefusal> {
    // lock.c svn_fs_fs__unlock: "We need to have a username attached to the fs."
    let username = request
      .user
      .as_deref()
      .ok_or(LockRefusal::NoUser { path: None })?;
    let checks: Vec<(String, UnlockAnswer)> = request
      .targets
      .iter()
      .map(|(path, token)| {
        // fs-loader.c svn_fs_unlock: "if (!token) token = "";"
        let token = token.clone().unwrap_or_default();
        (
          path.clone(),
          self.check_unlock(path, &token, username, request.break_lock, now),
        )
      })
      .collect();
    for (path, checked) in &checks {
      if checked.is_ok() {
        self.digests.remove(path);
      }
    }
    Ok(checks)
  }

  /// `lock.c` `check_unlock`, after slates' canonical path.
  fn check_unlock(
    &mut self,
    path: &str,
    token: &str,
    username: &str,
    break_lock: bool,
    now: AprTime,
  ) -> UnlockAnswer {
    if !canonical(path) {
      return Err(LockRefusal::PathSyntax {
        path: path.to_owned(),
      });
    }
    let lock = self.get_lock(path, now)?;
    if !break_lock && token != lock.token.as_str() {
      return Err(LockRefusal::NoSuchLock {
        path: path.to_owned(),
      });
    }
    if !break_lock && username != lock.owner {
      return Err(LockRefusal::OwnerMismatch {
        path: path.to_owned(),
        owner: lock.owner,
      });
    }
    Ok(())
  }

  /// `lock.c` `get_lock` under the write lock, the lock having to exist: an expired one is removed (`unlock_single`).
  fn get_lock(&mut self, path: &str, now: AprTime) -> Result<Lock, LockRefusal> {
    let lock = self
      .digests
      .get(path)
      .cloned()
      .ok_or_else(|| LockRefusal::NoSuchLock {
        path: path.to_owned(),
      })?;
    if lock_expired(&lock, now) {
      self.digests.remove(path);
      return Err(LockRefusal::Expired {
        path: path.to_owned(),
      });
    }
    Ok(lock)
  }

  /// `lock.c` `get_lock_helper` under the write lock: "doesn't tell the caller *why* the lock is unavailable".
  fn get_lock_helper(&mut self, path: &str, now: AprTime) -> Option<Lock> {
    self.get_lock(path, now).ok()
  }

  /// A commit's locks: `transaction.c` `verify_locks` under the write lock, then, accepted, `serve.c`'s `unlock_paths`
  /// unless the client keeps its locks. Refused: every lock that could have refused it (`walk_locks` meets a
  /// directory's locks in a hash table's order, so FSFS reports whichever it meets first), with nothing changed.
  fn commit(
    &mut self,
    changes: &[(String, Action)],
    commit: &CommitLocks,
    now: AprTime,
  ) -> Result<(), BTreeSet<(String, u32)>> {
    let not_canonical: BTreeSet<(String, u32)> = changes
      .iter()
      .filter(|(path, _)| !canonical(path))
      .map(|(path, _)| (path.clone(), SVN_ERR_FS_PATH_SYNTAX))
      .collect();
    if !not_canonical.is_empty() {
      return Err(not_canonical);
    }
    let verified = self.verify_locks(changes, commit, now);
    if !verified.failures.is_empty() {
      return Err(verified.failures);
    }
    for path in &verified.expired {
      self.digests.remove(path);
    }
    if !commit.keep && !commit.presented.is_empty() {
      self.unlock_paths(commit, now);
    }
    Ok(())
  }

  /// `transaction.c` `verify_locks`: the changed paths "sorted depth-first-ily", each verified, recursively unless it
  /// was only modified, skipping a path a recursive check of one of its parents covered.
  fn verify_locks(
    &self,
    changes: &[(String, Action)],
    commit: &CommitLocks,
    now: AprTime,
  ) -> Verified {
    let mut sorted = changes.to_vec();
    sorted.sort_by(|(one, _), (two, _)| compare_paths(one, two));
    let mut verified = Verified::default();
    let mut last_recursed: Option<String> = None;
    for (path, action) in &sorted {
      if last_recursed
        .as_deref()
        .is_some_and(|recursed| skip_ancestor(recursed, path).is_some())
      {
        continue;
      }
      let recurse = *action != Action::Modify;
      self.allow_locked_operation(path, recurse, commit, now, &mut verified);
      if recurse {
        last_recursed = Some(path.clone());
      }
    }
    verified
  }

  /// `lock.c` `svn_fs_fs__allow_locked_operation`: the lock at the path, or every lock at or beneath it
  /// (`walk_locks`), each expired one met and each other verified (`verify_lock`).
  fn allow_locked_operation(
    &self,
    path: &str,
    recurse: bool,
    commit: &CommitLocks,
    now: AprTime,
    verified: &mut Verified,
  ) {
    let locks: Vec<&Lock> = if recurse {
      self
        .digests
        .values()
        .filter(|lock| skip_ancestor(path, &lock.path).is_some())
        .collect()
    } else {
      self.digests.get(path).into_iter().collect()
    };
    for lock in locks {
      if lock_expired(lock, now) {
        verified.expired.insert(lock.path.clone());
      } else if let Err(refusal) = verify_lock(lock, commit) {
        verified
          .failures
          .insert((lock.path.clone(), refusal.subversion_code()));
      }
    }
  }

  /// `serve.c` `unlock_paths`: `svn_repos_fs_unlock_many` of every presented path and token, never breaking, its errors
  /// ignored ("The lock may have become defunct after the commit, so ignore such errors").
  fn unlock_paths(&mut self, commit: &CommitLocks, now: AprTime) {
    // fs-wrap.c 770: "if (! break_lock && ! username)", the whole release refused, which serve.c ignores.
    if commit.user.is_none() {
      return;
    }
    let request = UnlockRequest {
      user: commit.user.clone(),
      break_lock: false,
      targets: commit
        .presented
        .iter()
        .map(|(path, token)| (path.clone(), Some(token.clone())))
        .collect(),
    };
    let _ignored = self.unlock_many(&request, now);
  }

  /// `svn_fs_fs__get_lock`: `get_lock_helper` without the write lock, so an expired lock is none, and stays.
  fn read_lock(&self, path: &str, now: AprTime) -> Option<&Lock> {
    self
      .digests
      .get(path)
      .filter(|lock| !lock_expired(lock, now))
  }

  /// `svn_fs_fs__get_locks`: `walk_locks` without the write lock, through `get_locks_filter_func`.
  fn read_locks(&self, path: &str, depth: Depth, now: AprTime) -> Vec<&Lock> {
    self
      .digests
      .values()
      .filter(|lock| skip_ancestor(path, &lock.path).is_some() && !lock_expired(lock, now))
      .filter(|lock| get_locks_filter(path, depth, lock))
      .collect()
  }
}

/// `lock.c` `lock_expired`: "lock->expiration_date && (apr_time_now() > lock->expiration_date)".
fn lock_expired(lock: &Lock, now: AprTime) -> bool {
  lock
    .expires
    .is_some_and(|expiration_date| now.0 > expiration_date.0)
}

/// `lock.c` `verify_lock`, in its order: no username, then the owner, then the token among those presented.
fn verify_lock(lock: &Lock, commit: &CommitLocks) -> Result<(), LockRefusal> {
  let Some(username) = commit.user.as_deref() else {
    return Err(LockRefusal::NoUser {
      path: Some(lock.path.clone()),
    });
  };
  if username != lock.owner {
    return Err(LockRefusal::OwnerMismatch {
      path: lock.path.clone(),
      owner: lock.owner.clone(),
    });
  }
  // The access context's tokens are keyed by token (`svn_hash_gets(fs->access_ctx->lock_tokens, lock->token)`).
  if !commit
    .presented
    .values()
    .any(|token| *token == lock.token.as_str())
  {
    return Err(LockRefusal::TokenNotPresented {
      path: lock.path.clone(),
    });
  }
  Ok(())
}

/// `check_lock`'s node checks: a directory is not a file; nothing there is out of date when the client named its
/// revision, and not found otherwise. The node's created revision.
fn check_path(path: &str, target: &LockTarget, head: &Head) -> Result<Revision, LockRefusal> {
  match head.nodes.get(path) {
    Some((NodeKind::Directory, _)) => Err(LockRefusal::NotFile {
      path: path.to_owned(),
    }),
    Some((NodeKind::File, created_rev)) => Ok(*created_rev),
    None if target.current.is_some() => Err(LockRefusal::OutOfDate {
      path: path.to_owned(),
    }),
    None => Err(LockRefusal::NotFound {
      path: path.to_owned(),
    }),
  }
}

/// `fs-loader.c` `svn_fs_lock_many`'s token check: the scheme, then ASCII and no control character (slates reads the
/// whole token, so a NUL is a control character), then XML-safe, which ASCII without control characters always is.
fn check_token(token: &str) -> Result<(), LockRefusal> {
  let bad = !token.starts_with("opaquelocktoken:")
    || token
      .bytes()
      .any(|c| !svn_ctype_isascii(c) || svn_ctype_iscntrl(c))
    || !is_xml_safe(token.as_bytes());
  if bad {
    return Err(LockRefusal::MalformedToken {
      token: token.to_owned(),
    });
  }
  Ok(())
}

/// `xml.c` `svn_xml_is_xml_safe` over the whole data: valid UTF-8, and no control character (`svn_ctype_iscntrl`: 0 to
/// 31 and 127) but tab, line feed, carriage return and delete.
fn is_xml_safe(data: &[u8]) -> bool {
  std::str::from_utf8(data).is_ok()
    && data
      .iter()
      .all(|&c| !svn_ctype_iscntrl(c) || matches!(c, b'\t' | b'\n' | b'\r' | 0x7f))
}

/// `svn_ctype_iscntrl`: the bytes `libsvn_subr/ctype.c` marks `SVN_CTYPE_CNTRL`, 0 to 31 and 127.
fn svn_ctype_iscntrl(c: u8) -> bool {
  c < 0x20 || c == 0x7f
}

/// `svn_ctype_isascii`: the bytes `libsvn_subr/ctype.c` marks `SVN_CTYPE_ASCII`, 0 to 127.
fn svn_ctype_isascii(c: u8) -> bool {
  c < 0x80
}

/// `svn_fs_fs__generate_lock_token`: "opaquelocktoken:" and `svn_uuid_generate`, written by `apr_uuid_format` as
/// lower-case hex grouped 8-4-4-4-12; a random UUID's version (4) and variant (0b10) bits set (RFC 9562 §5.4).
fn generate_lock_token(random: [u8; UUID_BYTES]) -> String {
  let mut uuid = random;
  uuid[6] = (uuid[6] & 0x0f) | 0x40;
  uuid[8] = (uuid[8] & 0x3f) | 0x80;
  let hex: Vec<String> = uuid.iter().map(|byte| format!("{byte:02x}")).collect();
  format!(
    "opaquelocktoken:{}-{}-{}-{}-{}",
    hex[0..4].concat(),
    hex[4..6].concat(),
    hex[6..8].concat(),
    hex[8..10].concat(),
    hex[10..16].concat()
  )
}

/// Subversion's canonical repository path, the model's own reading of `svn_fspath__is_canonical` and
/// `svn_fs__path_valid`: `/`, or `/` and names joined by `/`, none empty, `.` or `..`, nor holding a line feed or a
/// NUL.
fn canonical(path: &str) -> bool {
  path == "/"
    || path.strip_prefix('/').is_some_and(|names| {
      names
        .split('/')
        .all(|name| !name.is_empty() && name != "." && name != ".." && !name.contains(['\n', '\0']))
    })
}

/// `svn_fspath__skip_ancestor`: `child` relative to `parent` when it is `parent` or beneath it.
fn skip_ancestor<'c>(parent: &str, child: &'c str) -> Option<&'c str> {
  if parent == "/" {
    return child.strip_prefix('/');
  }
  if child == parent {
    return Some("");
  }
  child.strip_prefix(parent)?.strip_prefix('/')
}

/// `svn_path_compare_paths`: "Children of paths are greater than their parents, but less than greater siblings of
/// their parents", `/` sorting before every other byte.
fn compare_paths(one: &str, two: &str) -> Ordering {
  let (one, two) = (one.as_bytes(), two.as_bytes());
  let common = one
    .iter()
    .zip(two)
    .take_while(|(left, right)| left == right)
    .count();
  if one.len() == two.len() && common >= one.len() {
    return Ordering::Equal;
  }
  let next_one = one.get(common).copied().unwrap_or(0);
  let next_two = two.get(common).copied().unwrap_or(0);
  match (next_one, next_two) {
    (b'/', 0) => Ordering::Greater,
    (0, b'/') => Ordering::Less,
    (b'/', _) => Ordering::Less,
    (_, b'/') => Ordering::Greater,
    _ => next_one.cmp(&next_two),
  }
}

/// `lock.c` `get_locks_filter_func`: the path itself, everything at infinity, and at files or immediates what is one
/// component beneath.
fn get_locks_filter(path: &str, depth: Depth, lock: &Lock) -> bool {
  if path == lock.path || depth == Depth::Infinity {
    return true;
  }
  let one_down = skip_ancestor(path, &lock.path)
    .is_some_and(|rel_uri| rel_uri.split('/').filter(|name| !name.is_empty()).count() == 1);
  matches!(depth, Depth::Files | Depth::Immediates) && one_down
}

// ── Generated histories ──────────────────────────────────────────────────────────────────────────────────────────────

/// The paths a history names, with their kinds: directories, files, `/a-b` (which sorts between `/a` and `/a/f` in byte
/// order, and after them in Subversion's), and a file two levels down.
const UNIVERSE: [(&str, NodeKind); 10] = [
  ("/", NodeKind::Directory),
  ("/a", NodeKind::Directory),
  ("/a/d", NodeKind::Directory),
  ("/b", NodeKind::Directory),
  ("/a/f", NodeKind::File),
  ("/a/g", NodeKind::File),
  ("/a/d/h", NodeKind::File),
  ("/a-b", NodeKind::File),
  ("/b/f", NodeKind::File),
  ("/c", NodeKind::File),
];

/// A path out of Subversion's canonical form, the one index past [`UNIVERSE`].
const NOT_CANONICAL: &str = "/a//f";

/// The principals of a history; a step's principal may also be none.
const USERS: [&str; 2] = [ALICE, BOB];

/// The revision a lock request names for a path.
#[derive(Clone, Copy, Debug)]
enum Current {
  None,
  Youngest,
  Created,
  Older,
  Future,
}

/// The token a lock request supplies for a path.
#[derive(Clone, Copy, Debug)]
enum Supply {
  Drawn,
  Valid,
  BadScheme,
  Control,
}

/// The token an unlock or a commit sends for a path.
#[derive(Clone, Copy, Debug)]
enum Present {
  Own,
  Another,
  Stale,
  Garbage,
  Absent,
}

/// A lock request's comment.
#[derive(Clone, Copy, Debug)]
enum Comment {
  None,
  Plain,
  Allowed,
  Control,
  Nul,
  NotUtf8,
}

/// A lock request's expiry, from the owner's clock.
#[derive(Clone, Copy, Debug)]
enum Expiry {
  Never,
  Zero,
  Soon,
  Later,
  Past,
  Negative,
}

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
  Lock {
    user: Option<usize>,
    targets: Vec<(usize, Current, Supply)>,
    comment: Comment,
    expires: Expiry,
    steal: bool,
  },
  Unlock {
    user: Option<usize>,
    targets: Vec<(usize, Present)>,
    break_lock: bool,
  },
  Commit {
    user: Option<usize>,
    changes: Vec<(usize, Action)>,
    presented: Vec<(usize, Present)>,
    keep: bool,
  },
  Tick {
    micros: i64,
  },
}

/// A step's principal: one of [`USERS`], now and then none.
fn user() -> impl Strategy<Value = Option<usize>> {
  prop_oneof![8 => (0..USERS.len()).prop_map(Some), 1 => Just(None)]
}

/// A path's index: [`UNIVERSE`]'s, and now and then [`NOT_CANONICAL`]'s.
fn path() -> impl Strategy<Value = usize> {
  prop_oneof![30 => 0..UNIVERSE.len(), 1 => Just(UNIVERSE.len())]
}

/// A lock request's step.
fn lock_step() -> impl Strategy<Value = Step> {
  let current = prop_oneof![
    6 => Just(Current::None),
    2 => Just(Current::Youngest),
    2 => Just(Current::Created),
    1 => Just(Current::Older),
    1 => Just(Current::Future),
  ];
  let supply = prop_oneof![
    12 => Just(Supply::Drawn),
    2 => Just(Supply::Valid),
    1 => Just(Supply::BadScheme),
    1 => Just(Supply::Control),
  ];
  let comment = prop_oneof![
    6 => Just(Comment::None),
    3 => Just(Comment::Plain),
    1 => Just(Comment::Allowed),
    1 => Just(Comment::Control),
    1 => Just(Comment::Nul),
    1 => Just(Comment::NotUtf8),
  ];
  let expires = prop_oneof![
    6 => Just(Expiry::Never),
    1 => Just(Expiry::Zero),
    3 => Just(Expiry::Soon),
    1 => Just(Expiry::Later),
    2 => Just(Expiry::Past),
    1 => Just(Expiry::Negative),
  ];
  let targets = proptest::collection::vec((path(), current, supply), 0..4);
  (
    user(),
    targets,
    comment,
    expires,
    proptest::bool::weighted(0.25),
  )
    .prop_map(|(user, targets, comment, expires, steal)| Step::Lock {
      user,
      targets,
      comment,
      expires,
      steal,
    })
}

/// The token a step sends for a path.
fn present() -> impl Strategy<Value = Present> {
  prop_oneof![
    6 => Just(Present::Own),
    2 => Just(Present::Another),
    1 => Just(Present::Stale),
    1 => Just(Present::Garbage),
    2 => Just(Present::Absent),
  ]
}

/// An unlock request's step.
fn unlock_step() -> impl Strategy<Value = Step> {
  let targets = proptest::collection::vec((path(), present()), 0..3);
  (user(), targets, proptest::bool::weighted(0.25)).prop_map(|(user, targets, break_lock)| {
    Step::Unlock {
      user,
      targets,
      break_lock,
    }
  })
}

/// A commit's step.
fn commit_step() -> impl Strategy<Value = Step> {
  let action = prop_oneof![
    4 => Just(Action::Modify),
    2 => Just(Action::Delete),
    1 => Just(Action::Add),
    1 => Just(Action::Replace),
  ];
  let changes = proptest::collection::vec((path(), action), 1..4);
  let presented = proptest::collection::vec((path(), present()), 0..4);
  (user(), changes, presented, proptest::bool::weighted(0.3)).prop_map(
    |(user, changes, presented, keep)| Step::Commit {
      user,
      changes,
      presented,
      keep,
    },
  )
}

/// One step of a history: more locks than anything, the clock moving a little or past every expiry now and then.
fn step() -> impl Strategy<Value = Step> {
  let tick =
    prop_oneof![3 => 0..4i64, 1 => 1_000..100_000i64].prop_map(|micros| Step::Tick { micros });
  prop_oneof![5 => lock_step(), 2 => unlock_step(), 3 => commit_step(), 2 => tick]
}

/// A history: its room, and its steps.
fn histories() -> impl Strategy<Value = (usize, Vec<Step>)> {
  let room = prop_oneof![Just(2usize), Just(3), Just(5), Just(ALL_ROOM)];
  (room, proptest::collection::vec(step(), 1..48))
}

/// What a history runs against: the table, guarded and applied; the model; the head; the owner's clock; the room; the
/// draws so far; and every token either made, which a step may present again.
struct World {
  table: LockTable,
  model: Fsfs,
  head: Head,
  now: AprTime,
  room: usize,
  drawn: u64,
  tokens: Vec<String>,
}

/// The rules a run of histories met, so a rule never met cannot pass unseen; every run prints it.
#[derive(Debug, Default)]
struct Census {
  refusals: BTreeSet<&'static str>,
  drawn: usize,
  supplied: usize,
  stolen: usize,
  broken: usize,
  released: usize,
  kept: usize,
  expired_met: usize,
  let_go: usize,
  refused_commits: usize,
}

/// The name of a refusal's kind, for the census.
fn kind(refusal: &LockRefusal) -> &'static str {
  match refusal {
    LockRefusal::PathSyntax { .. } => "PathSyntax",
    LockRefusal::NoUser { .. } => "NoUser",
    LockRefusal::UnescapableComment => "UnescapableComment",
    LockRefusal::NegativeExpiry { .. } => "NegativeExpiry",
    LockRefusal::MalformedToken { .. } => "MalformedToken",
    LockRefusal::NotFile { .. } => "NotFile",
    LockRefusal::NotFound { .. } => "NotFound",
    LockRefusal::OutOfDate { .. } => "OutOfDate",
    LockRefusal::NoSuchRevision { .. } => "NoSuchRevision",
    LockRefusal::AlreadyLocked { .. } => "AlreadyLocked",
    LockRefusal::NoSuchLock { .. } => "NoSuchLock",
    LockRefusal::Expired { .. } => "Expired",
    LockRefusal::OwnerMismatch { .. } => "OwnerMismatch",
    LockRefusal::TokenNotPresented { .. } => "TokenNotPresented",
    LockRefusal::Room { .. } => "Room",
    LockRefusal::Unrecorded { .. } => "Unrecorded",
  }
}

/// Every kind of refusal a history can meet: all but `Unrecorded`, which no guard writes (a case test holds it).
const EVERY_REFUSAL: [&str; 15] = [
  "PathSyntax",
  "NoUser",
  "UnescapableComment",
  "NegativeExpiry",
  "MalformedToken",
  "NotFile",
  "NotFound",
  "OutOfDate",
  "NoSuchRevision",
  "AlreadyLocked",
  "NoSuchLock",
  "Expired",
  "OwnerMismatch",
  "TokenNotPresented",
  "Room",
];

/// The path at an index of a step.
fn path_at(index: usize) -> &'static str {
  UNIVERSE.get(index).map_or(NOT_CANONICAL, |(path, _)| path)
}

/// `Ok` when the table's and the model's are equal, else what differed.
fn same<T: PartialEq + Debug>(what: &str, table: &T, model: &T) -> Result<(), String> {
  if table == model {
    Ok(())
  } else {
    Err(format!(
      "{what}: the table's {table:?}, the model's {model:?}"
    ))
  }
}

impl World {
  /// A new repository at revision 1 holding every path of [`UNIVERSE`], within `room`.
  fn new(room: usize) -> Self {
    let nodes = UNIVERSE
      .iter()
      .map(|(path, kind)| ((*path).to_owned(), (*kind, 1)))
      .collect();
    Self {
      table: LockTable::new(),
      model: Fsfs::default(),
      head: Head { youngest: 1, nodes },
      now: NOW,
      room,
      drawn: 0,
      tokens: Vec::new(),
    }
  }

  /// Runs one step on the table and the model and compares them.
  fn run(&mut self, step: &Step, census: &mut Census) -> Result<(), String> {
    match step {
      Step::Lock { .. } => self.lock(step, census),
      Step::Unlock {
        user,
        targets,
        break_lock,
      } => {
        let request = UnlockRequest {
          user: user.map(|user| USERS[user].to_owned()),
          break_lock: *break_lock,
          targets: targets
            .iter()
            .map(|(path, sent)| {
              (
                path_at(*path).to_owned(),
                self.token_for(path_at(*path), *sent),
              )
            })
            .collect(),
        };
        self.unlock(&request, census)
      }
      Step::Commit {
        user,
        changes,
        presented,
        keep,
      } => {
        let commit = CommitLocks {
          user: user.map(|user| USERS[user].to_owned()),
          presented: presented
            .iter()
            .filter_map(|(path, sent)| {
              Some((
                path_at(*path).to_owned(),
                self.token_for(path_at(*path), *sent)?,
              ))
            })
            .collect(),
          keep: *keep,
        };
        let changes: Vec<(String, Action)> = changes
          .iter()
          .map(|(path, action)| (path_at(*path).to_owned(), *action))
          .collect();
        self.commit(&changes, &commit, census)
      }
      Step::Tick { micros } => {
        self.now = AprTime(self.now.0 + micros);
        Ok(())
      }
    }
  }

  /// The token a step sends for `path`.
  fn token_for(&self, path: &str, sent: Present) -> Option<String> {
    let held = |wanted: &dyn Fn(&Lock) -> bool| {
      self
        .table
        .locks()
        .find(|lock| wanted(lock))
        .map(|lock| lock.token.as_str().to_owned())
    };
    match sent {
      Present::Own => Some(held(&|lock| lock.path == path).unwrap_or_else(|| GARBAGE.to_owned())),
      Present::Another => {
        Some(held(&|lock| lock.path != path).unwrap_or_else(|| GARBAGE.to_owned()))
      }
      Present::Stale => Some(
        self
          .tokens
          .iter()
          .find(|token| {
            self
              .table
              .locks()
              .all(|lock| lock.token.as_str() != token.as_str())
          })
          .cloned()
          .unwrap_or_else(|| GARBAGE.to_owned()),
      ),
      Present::Garbage => Some(GARBAGE.to_owned()),
      Present::Absent => None,
    }
  }

  /// The lock request a step makes, against the head and the clock as they stand.
  fn lock_request(&self, step: &Step) -> LockRequest {
    let Step::Lock {
      user,
      targets,
      comment,
      expires,
      steal,
    } = step
    else {
      return LockRequest::default();
    };
    let comment = match comment {
      Comment::None => None,
      Comment::Plain => Some(b"a reason".to_vec()),
      Comment::Allowed => Some(b"tab\tline\nreturn\rdelete\x7f".to_vec()),
      Comment::Control => Some(b"bell\x07".to_vec()),
      Comment::Nul => Some(b"cut\x00short".to_vec()),
      Comment::NotUtf8 => Some(vec![0xff, 0xfe]),
    };
    let expires = match expires {
      Expiry::Never => None,
      Expiry::Zero => Some(AprTime(0)),
      Expiry::Soon => Some(AprTime(self.now.0 + 3)),
      Expiry::Later => Some(AprTime(self.now.0 + 50_000)),
      Expiry::Past => Some(AprTime(self.now.0 - 1)),
      Expiry::Negative => Some(AprTime(-1)),
    };
    LockRequest {
      user: user.map(|user| USERS[user].to_owned()),
      comment,
      is_dav_comment: false,
      expires,
      steal: *steal,
      targets: targets
        .iter()
        .map(|(path, current, supply)| {
          (
            path_at(*path).to_owned(),
            self.lock_target(path_at(*path), *current, *supply),
          )
        })
        .collect(),
    }
  }

  /// One path of a lock request.
  fn lock_target(&self, path: &str, current: Current, supply: Supply) -> LockTarget {
    let youngest = self.head.youngest;
    let created = self
      .head
      .nodes
      .get(path)
      .map_or(youngest, |(_, created)| *created);
    let current = match current {
      Current::None => None,
      Current::Youngest => Some(youngest),
      Current::Created => Some(created),
      Current::Older => Some(created.saturating_sub(1)),
      Current::Future => Some(youngest + 1),
    };
    let token = match supply {
      Supply::Drawn => None,
      Supply::Valid => Some(format!("opaquelocktoken:supplied-{}", self.tokens.len())),
      Supply::BadScheme => Some("urn:uuid:supplied".to_owned()),
      Supply::Control => Some("opaquelocktoken:bell\x07".to_owned()),
    };
    LockTarget { current, token }
  }

  /// A lock request on both, each drawing from the same count: the same answers, the same draws, the same tables.
  fn lock(&mut self, step: &Step, census: &mut Census) -> Result<(), String> {
    let request = self.lock_request(step);
    let (mut table_drawn, mut model_drawn) = (self.drawn, self.drawn);
    let guarded = self
      .table
      .lock(&request, &self.head, self.now, self.room, &mut || {
        draw_from(&mut table_drawn)
      });
    let modelled = self
      .model
      .lock_many(&request, &self.head, (self.now, self.room), &mut || {
        draw_from(&mut model_drawn)
      });
    same("lock draws", &table_drawn, &model_drawn)?;
    self.drawn = table_drawn;
    let decision = match (guarded, modelled) {
      (Ok(decision), Ok(answers)) => {
        same("lock answers", &decision.answers, &answers)?;
        decision
      }
      (guarded, modelled) => return refused_whole(census, guarded.err(), modelled.err()),
    };
    census.lock(&decision, &self.table, &request, self.now);
    self.apply(&decision.entries)?;
    self.tokens.extend(
      decision
        .answers
        .iter()
        .filter_map(|(_, answer)| Some(answer.as_ref().ok()?.token.as_str().to_owned())),
    );
    self.compare()
  }

  /// An unlock request on both.
  fn unlock(&mut self, request: &UnlockRequest, census: &mut Census) -> Result<(), String> {
    let guarded = self.table.unlock(request, self.now);
    let modelled = self.model.unlock_many(request, self.now);
    let decision = match (guarded, modelled) {
      (Ok(decision), Ok(answers)) => {
        same("unlock answers", &decision.answers, &answers)?;
        decision
      }
      (guarded, modelled) => return refused_whole(census, guarded.err(), modelled.err()),
    };
    census.unlock(&decision, &self.table, request);
    self.apply(&decision.entries)?;
    self.compare()
  }

  /// A commit's locks on both: refused by a lock the model could have refused it for, or accepted by both; accepted,
  /// the head moves on.
  fn commit(
    &mut self,
    changes: &[(String, Action)],
    commit: &CommitLocks,
    census: &mut Census,
  ) -> Result<(), String> {
    let guarded = self.table.verdict(
      changes
        .iter()
        .map(|(path, action)| (path.as_str(), *action)),
      commit,
      self.now,
    );
    let modelled = self.model.commit(changes, commit, self.now);
    match (guarded, modelled) {
      (Ok(entries), Ok(())) => {
        census.commit(&entries, &self.table, commit, self.now);
        self.apply(&entries)?;
        commit_head(&mut self.head, changes);
      }
      (Err(refusal), Err(failures)) => {
        let site = (refusal_path(&refusal), refusal.subversion_code());
        if !failures.contains(&site) {
          return Err(format!(
            "commit refused {refusal:?}, which is none of the model's {failures:?}"
          ));
        }
        census.refused(&[&refusal]);
        census.refused_commits += 1;
      }
      (guarded, modelled) => {
        return Err(format!(
          "commit: the table's {guarded:?}, the model's {modelled:?}"
        ));
      }
    }
    self.compare()
  }

  /// Applies a decision's entries to the table.
  fn apply(&mut self, entries: &[LockEntry]) -> Result<(), String> {
    for entry in entries {
      self
        .table
        .apply(entry)
        .map_err(|refusal| format!("{entry:?} refused {refusal:?}"))?;
    }
    Ok(())
  }

  /// The table equals the model: every lock it holds, expired ones too, and every read.
  fn compare(&self) -> Result<(), String> {
    let held: Vec<&Lock> = self.table.locks().collect();
    let model: Vec<&Lock> = self.model.digests.values().collect();
    same("the locks held", &held, &model)?;
    for (path, _) in UNIVERSE {
      same(
        "get-lock",
        &self.table.get_lock(path, self.now),
        &self.model.read_lock(path, self.now),
      )?;
      for depth in [
        Depth::Empty,
        Depth::Files,
        Depth::Immediates,
        Depth::Infinity,
      ] {
        same(
          "get-locks",
          &self.table.get_locks(path, depth, self.now),
          &self.model.read_locks(path, depth, self.now),
        )?;
      }
    }
    Ok(())
  }
}

/// A request refused whole, which changes nothing: the same refusal from the table and the model, counted.
fn refused_whole(
  census: &mut Census,
  table: Option<LockRefusal>,
  model: Option<LockRefusal>,
) -> Result<(), String> {
  same("refused whole", &table, &model)?;
  census.refused(&table.iter().collect::<Vec<_>>());
  Ok(())
}

/// The path a commit's refusal names.
fn refusal_path(refusal: &LockRefusal) -> String {
  match refusal {
    LockRefusal::NoUser { path: Some(path) }
    | LockRefusal::OwnerMismatch { path, .. }
    | LockRefusal::TokenNotPresented { path }
    | LockRefusal::PathSyntax { path } => path.clone(),
    other => format!("{other:?}"),
  }
}

/// The head after an accepted commit: each change made at the next revision, deletions taking what was beneath.
fn commit_head(head: &mut Head, changes: &[(String, Action)]) {
  head.youngest += 1;
  let revision = head.youngest;
  let mut sorted = changes.to_vec();
  sorted.sort_by(|(one, _), (two, _)| one.cmp(two));
  for (path, action) in sorted {
    let Some((_, kind)) = UNIVERSE.iter().find(|(known, _)| *known == path) else {
      continue;
    };
    if matches!(action, Action::Delete | Action::Replace) {
      head
        .nodes
        .retain(|held, _| skip_ancestor(&path, held).is_none());
    }
    if matches!(action, Action::Add | Action::Replace) {
      head.nodes.insert(path.clone(), (*kind, revision));
    }
    if let (Action::Modify, Some(node)) = (action, head.nodes.get_mut(&path)) {
      node.1 = revision;
    }
  }
}

impl Census {
  /// Counts the refusals among `refusals`.
  fn refused(&mut self, refusals: &[&LockRefusal]) {
    self
      .refusals
      .extend(refusals.iter().map(|refusal| kind(refusal)));
  }

  /// Counts a lock request's rules: its refusals, its tokens drawn and supplied, its steals and what it let go.
  fn lock(
    &mut self,
    decision: &Decision<LockAnswer>,
    before: &LockTable,
    request: &LockRequest,
    now: AprTime,
  ) {
    for (path, answer) in &decision.answers {
      match answer {
        Err(refusal) => self.refused(&[refusal]),
        Ok(_)
          if request
            .targets
            .get(path)
            .is_some_and(|target| target.token.is_some()) =>
        {
          self.supplied += 1
        }
        Ok(_) => self.drawn += 1,
      }
      if answer.is_ok() && request.steal && before.get_lock(path, now).is_some() {
        self.stolen += 1;
      }
    }
    if decision
      .entries
      .iter()
      .any(|entry| matches!(entry, LockEntry::Removed { .. }))
    {
      self.let_go += 1;
    }
  }

  /// Counts an unlock request's rules: its refusals, and each break of another principal's lock.
  fn unlock(
    &mut self,
    decision: &Decision<UnlockAnswer>,
    before: &LockTable,
    request: &UnlockRequest,
  ) {
    for (path, answer) in &decision.answers {
      if let Err(refusal) = answer {
        self.refused(&[refusal]);
      }
      let others = before
        .locks()
        .any(|lock| lock.path == *path && Some(lock.owner.as_str()) != request.user.as_deref());
      if answer.is_ok() && request.break_lock && others {
        self.broken += 1;
      }
    }
  }

  /// Counts an accepted commit's rules: locks released, kept, and met expired.
  fn commit(
    &mut self,
    entries: &[LockEntry],
    before: &LockTable,
    commit: &CommitLocks,
    now: AprTime,
  ) {
    for entry in entries {
      let LockEntry::Removed { path, .. } = entry else {
        continue;
      };
      if before.get_lock(path, now).is_some() {
        self.released += 1;
      } else {
        self.expired_met += 1;
      }
    }
    let own = |path: &String| {
      before
        .get_lock(path, now)
        .is_some_and(|lock| Some(lock.owner.as_str()) == commit.user.as_deref())
    };
    if commit.keep && commit.presented.keys().any(own) {
      self.kept += 1;
    }
  }

  /// Whether every rule was met.
  fn met(&self) -> bool {
    let counts = [
      self.drawn,
      self.supplied,
      self.stolen,
      self.broken,
      self.released,
      self.kept,
      self.expired_met,
      self.let_go,
      self.refused_commits,
    ];
    counts.iter().all(|&count| count > 0)
      && EVERY_REFUSAL
        .iter()
        .all(|kind| self.refusals.contains(kind))
  }
}

/// Runs one history: every step on the table and the model, compared after each.
fn check_history(room: usize, steps: &[Step], census: &mut Census) -> Result<(), String> {
  let mut world = World::new(room);
  for (at, step) in steps.iter().enumerate() {
    world
      .run(step, census)
      .map_err(|why| format!("step {at} {step:?}: {why}"))?;
  }
  Ok(())
}

/// A-602 (§4.17.4 piece 8, the oracle): on generated histories (locks with and without revisions, supplied and drawn
/// tokens, comments XML can and cannot carry, every kind of expiry, steals; unlocks with the lock's token, another's, a
/// stale one, garbage or none, and breaks; commits modifying, adding, deleting and replacing paths beneath and beside
/// locked ones, presenting tokens or not, keeping locks or not, with no principal now and then; the clock moving a
/// little or past every expiry; rooms of two, three and five locks and none), the table, guarded then applied, equals
/// the transcription of FSFS's lock rules: the same answers, the same tokens drawn in the same order, every lock held
/// (expired ones too) and every `get-lock` and `get-locks` the same after every step, and a commit refused for a lock
/// the model could have refused it for, or accepted by both. The census shows every rule was met: every refusal a
/// guard makes, tokens drawn and supplied, steals, breaks, releases, kept locks, expired locks met by a commit, expired
/// locks let go for the room, and commits refused; every run prints it.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn the_lock_table_equals_fsfs_lock_rules_on_generated_histories() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(&histories(), |(room, steps)| {
    check_history(room, &steps, &mut census.borrow_mut()).map_err(TestCaseError::fail)
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("lock oracle census: {census:?}");
  assert!(census.met(), "a rule never met: {census:?}");
}
