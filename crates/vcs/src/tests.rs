//! The object store held to plain maps (A-581, §4.17.1 shared piece 3): arrivals, commits, seals, merges, forks and
//! releases over several repositories sharing one store, and each derived or learned bound by a test of its own.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;

use proptest::prelude::*;

use crate::arrival::{
  AdmitRefused, Admitted, Arriving, CommitRefused, Naming, Quarantine, object_payload, object_row,
};
use crate::content::{ContentId, ContentRefused, ContentStore, Form, IDENTITY_BYTES};
use crate::index::{
  Batch, FENCE_SLOT_BYTES, Index, IndexCounters, IndexRefused, Inserted, NewRow, TAIL_SLOT_BYTES,
};
use crate::table::{RowRef, encode_body};

/// Shape: the window the tests' store holds content in: eight identities a list, three object rows a table, and six
/// rows of one byte's key and twenty bytes' payload.
const WINDOW: usize = 256;

/// Format: the ids the test tools give, as long as git's and Mercurial's SHA-1 names.
const ID_BYTES: usize = 20;

/// A stand-in for git's naming: the kind and the length framed before the bytes, as git frames `<type> <length>\0`.
/// The store never computes a tool's hash itself, so a test tool's hash stands for the real one.
struct GitShaped;

impl Naming for GitShaped {
  type Id = Vec<u8>;
  type Refused = Infallible;

  fn name(&self, kind: u8, _context: &[u8], bytes: &[u8]) -> Result<Vec<u8>, Infallible> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[kind]);
    hasher.update(format!(" {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    Ok(hasher.finalize().as_bytes()[..ID_BYTES].to_vec())
  }
}

/// A stand-in for Mercurial's naming: the parents (the context) hashed before the text.
struct HgShaped;

impl Naming for HgShaped {
  type Id = Vec<u8>;
  type Refused = Infallible;

  fn name(&self, kind: u8, context: &[u8], bytes: &[u8]) -> Result<Vec<u8>, Infallible> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hg");
    hasher.update(&[kind]);
    hasher.update(context);
    hasher.update(bytes);
    Ok(hasher.finalize().as_bytes()[..ID_BYTES].to_vec())
  }
}

/// A tool whose hash collides by design: the first byte of the bytes, so the tests can bring one id twice with other
/// bytes.
struct FirstByte;

impl Naming for FirstByte {
  type Id = Vec<u8>;
  type Refused = Infallible;

  fn name(&self, _kind: u8, _context: &[u8], bytes: &[u8]) -> Result<Vec<u8>, Infallible> {
    Ok(bytes.iter().take(1).copied().collect())
  }
}

/// A refusing tool's reason: the plain hash it names a refused input by, as git names an attacked input by its plain
/// SHA-1.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Attack {
  plain: Vec<u8>,
}

/// A stand-in for git's SHA-1 with collision detection (`slates_git::object_name::name_of`, which refuses an attacked
/// input as `CollisionAttack`): it refuses bytes that begin as a PDF does, as git's own SHAttered vector does, and names
/// the rest as `GitShaped` does.
struct Detecting;

impl Naming for Detecting {
  type Id = Vec<u8>;
  type Refused = Attack;

  fn name(&self, kind: u8, context: &[u8], bytes: &[u8]) -> Result<Vec<u8>, Attack> {
    if bytes.starts_with(b"%PDF") {
      return Err(Attack {
        plain: ContentId::of(bytes).0[..ID_BYTES].to_vec(),
      });
    }
    GitShaped
      .name(kind, context, bytes)
      .map_err(|never| match never {})
  }
}

/// The parents a Mercurial-shaped object is named with.
const PARENTS: &[u8] = &[7; 40];

/// The id a tool that never refuses gives an object.
fn named<N: Naming<Id = Vec<u8>, Refused = Infallible>>(
  naming: &N,
  kind: u8,
  context: &[u8],
  bytes: &[u8],
) -> Vec<u8> {
  let Ok(id) = naming.name(kind, context, bytes);
  id
}

fn git_arriving<'a>(id: &'a [u8], bytes: &'a [u8]) -> Arriving<'a> {
  Arriving {
    id,
    kind: 3,
    context: b"",
    bytes,
  }
}

fn hg_arriving<'a>(id: &'a [u8], bytes: &'a [u8]) -> Arriving<'a> {
  Arriving {
    id,
    kind: 1,
    context: PARENTS,
    bytes,
  }
}

/// A store of the tests' window with room to spare.
fn store() -> ContentStore {
  ContentStore::new(1 << 20, WINDOW).unwrap()
}

/// A row of `key` naming `contents` with `payload`.
fn row<'a>(key: &'a [u8], contents: &'a [ContentId], payload: &'a [u8]) -> NewRow<'a> {
  NewRow {
    key,
    contents,
    payload,
  }
}

/// Puts `bytes` into the store, inserts a row naming them under `key`, and gives back the test's own hold.
fn insert_named(index: &mut Index, store: &mut ContentStore, key: &[u8], bytes: &[u8]) {
  let content = store.put(bytes).unwrap();
  let contents = [content];
  assert_eq!(
    index.insert(store, row(key, &contents, b"p")).unwrap(),
    Inserted::New
  );
  store.release(&content).unwrap();
}

/// Inserts a row naming nothing under `key`, with a payload of twenty bytes: 34 bytes in a table.
fn insert_plain(index: &mut Index, store: &mut ContentStore, key: u8) {
  assert_eq!(
    index.insert(store, row(&[key], &[], &[key; 20])),
    Ok(Inserted::New)
  );
}

fn read(index: &Index, store: &ContentStore) -> Vec<(Vec<u8>, Vec<u8>)> {
  index
    .iter(store)
    .map(|row| (row.key().to_vec(), row.body().to_vec()))
    .collect()
}

fn assert_empty(store: &ContentStore) {
  assert!(store.is_empty());
  assert_eq!((store.used(), store.charged()), (0, 0));
}

fn assert_object_row(
  index: &Index,
  store: &ContentStore,
  id: &[u8],
  expected: (u8, u64, ContentId),
) {
  let found = index.get(store, id).unwrap();
  assert_eq!(object_row(&found), Some(expected));
}

/// Every table `index` holds is at most a window.
fn assert_tables_within_the_window(index: &Index, store: &ContentStore) {
  for table in index.tables() {
    assert!(store.get(&table).is_some_and(|bytes| bytes.len() <= WINDOW));
  }
}

/// Ten windows of the tests' window, the first, fourth, seventh and a short tenth alike in their letter: an object two
/// levels of lists tall, whose full `a` window is repeated three times.
fn long_object() -> Vec<u8> {
  (0..9 * WINDOW + 96)
    .map(|at| b"abc"[(at / WINDOW) % 3])
    .collect()
}

/// A-581, §4.17.1 "Objects" item 1. Do: bring the same bytes as a git blob to one repository and as a Mercurial file
/// revision to another, commit both, seal both, then release one repository and the other. Expect: one content for
/// both tools from the first arrival on (the second put deduplicated), each repository's row naming it with its own
/// kind and length, two holders through tails and then tables, the bytes kept while one repository names them, and an
/// empty store at the end.
#[test]
fn the_same_bytes_from_git_and_mercurial_are_one_content_held_by_both_repositories() {
  let mut store = store();
  let (mut git_repository, mut hg_repository) = (Index::new(), Index::new());
  let bytes: &[u8] = b"int main(void) { return 0; }\n";
  let git_id = named(&GitShaped, 3, b"", bytes);
  let hg_id = named(&HgShaped, 1, PARENTS, bytes);
  assert_ne!(git_id, hg_id);
  let (mut git_push, mut hg_push) = (Quarantine::new(), Quarantine::new());
  assert_eq!(
    git_push.admit(&mut store, &GitShaped, git_arriving(&git_id, bytes)),
    Ok(Admitted::New)
  );
  assert_eq!(
    hg_push.admit(&mut store, &HgShaped, hg_arriving(&hg_id, bytes)),
    Ok(Admitted::New)
  );
  assert_eq!((store.len(), store.counters().deduplicated), (1, 1));
  git_push.commit(&mut git_repository, &mut store).unwrap();
  hg_push.commit(&mut hg_repository, &mut store).unwrap();
  let (len, content) = (u64::try_from(bytes.len()).unwrap(), ContentId::of(bytes));
  assert_object_row(&git_repository, &store, &git_id, (3, len, content));
  assert_object_row(&hg_repository, &store, &hg_id, (1, len, content));
  git_repository.seal_all(&mut store).unwrap();
  hg_repository.seal_all(&mut store).unwrap();
  assert_eq!(store.holders(&content), 2);
  git_repository.release(&mut store).unwrap();
  assert_eq!(store.get(&content), Some(bytes));
  hg_repository.release(&mut store).unwrap();
  assert_empty(&store);
}

/// Brings `o` twice through a tool that names by the first byte, the second time with other bytes.
fn a_collision_stores_nothing(store: &mut ContentStore) {
  let mut weak = Quarantine::new();
  weak
    .admit(store, &FirstByte, git_arriving(b"o", b"one"))
    .unwrap();
  assert_eq!(
    weak.admit(store, &FirstByte, git_arriving(b"o", b"other")),
    Err(AdmitRefused::Collision {
      id: Box::from(&b"o"[..])
    })
  );
  assert!(!store.contains(&ContentId::of(b"other")));
  weak.abandon(store).unwrap();
}

/// A-581, §4.17.1 "Objects" item 5 and "Failure matrix". Do: admit two objects of a push, then one whose bytes do not
/// hash to the id claimed, then the first again, then one that brings an id a push holds with other bytes; abandon the
/// push. Expect: the lie refused `IdMismatch` naming the id and the collision `Collision`, neither storing a byte; the
/// same object brought again answered `Again` and held once; the abandoned push leaving the store empty.
#[test]
fn an_object_whose_bytes_do_not_hash_to_its_id_is_refused_and_its_push_leaves_nothing() {
  let mut store = store();
  let mut push = Quarantine::new();
  for bytes in [&b"one"[..], b"two"] {
    let id = named(&GitShaped, 3, b"", bytes);
    assert_eq!(
      push.admit(&mut store, &GitShaped, git_arriving(&id, bytes)),
      Ok(Admitted::New)
    );
  }
  let claimed = named(&GitShaped, 3, b"", b"what the client said");
  assert_eq!(
    push.admit(
      &mut store,
      &GitShaped,
      git_arriving(&claimed, b"what it sent")
    ),
    Err(AdmitRefused::IdMismatch {
      id: claimed.clone().into_boxed_slice()
    })
  );
  assert_eq!((store.len(), push.len()), (2, 2));
  let again = named(&GitShaped, 3, b"", b"one");
  assert_eq!(
    push.admit(&mut store, &GitShaped, git_arriving(&again, b"one")),
    Ok(Admitted::Again)
  );
  assert_eq!(store.holders(&ContentId::of(b"one")), 1);
  a_collision_stores_nothing(&mut store);
  push.abandon(&mut store).unwrap();
  assert!(push.is_empty());
  assert_empty(&store);
}

/// A-581, §4.17.1 "Objects" item 5; the tool's naming is a parameter that may refuse (git's sha1dc: "SHA-1 appears to
/// be part of a collision attack"). Do: bring an object its tool refuses to name, under the very id the tool's plain
/// hash would give it, then an object it names. Expect: the first refused `NotNamed`, the claimed id and the tool's own
/// reason (its plain hash) carried intact, nothing stored and nothing held by the push; the second admitted.
#[test]
fn an_object_its_tool_will_not_name_is_refused_with_the_tools_own_reason() {
  let mut store = store();
  let mut push = Quarantine::new();
  let attacked: &[u8] = b"%PDF-1.3 a colliding block";
  let claimed = named(&GitShaped, 3, b"", attacked);
  assert_eq!(
    push.admit(&mut store, &Detecting, git_arriving(&claimed, attacked)),
    Err(AdmitRefused::NotNamed {
      id: claimed.clone().into_boxed_slice(),
      reason: Attack {
        plain: ContentId::of(attacked).0[..ID_BYTES].to_vec()
      }
    })
  );
  assert!(store.is_empty() && push.is_empty());
  let plain: &[u8] = b"a plain text";
  let id = named(&GitShaped, 3, b"", plain);
  assert_eq!(
    push.admit(&mut store, &Detecting, git_arriving(&id, plain)),
    Ok(Admitted::New)
  );
  push.abandon(&mut store).unwrap();
  assert_empty(&store);
}

/// A-614 (§4.17.2 "A push", item 3: a store that names each object itself holds it under that name). Do: hold bytes
/// under the name their tool gives them, then the same bytes again, then other bytes under that name; list what the
/// push holds. Expect: `New`, then `Again` with one hold, then `Collision` with nothing stored; the push listing its one
/// object with its kind, root and length.
#[test]
fn bytes_named_by_their_caller_are_held_under_that_name() {
  let mut store = store();
  let mut push = Quarantine::new();
  let bytes: &[u8] = b"a blob its store named";
  let id = named(&GitShaped, 3, b"", bytes);
  assert_eq!(
    push.hold_named(&mut store, &id, 3, bytes),
    Ok(Admitted::New)
  );
  assert_eq!(
    push.hold_named(&mut store, &id, 3, bytes),
    Ok(Admitted::Again)
  );
  assert_eq!(store.holders(&ContentId::of(bytes)), 1);
  assert!(matches!(
    push.hold_named(&mut store, &id, 3, b"other bytes"),
    Err(AdmitRefused::Collision { .. })
  ));
  assert!(!store.contains(&ContentId::of(b"other bytes")));
  let held: Vec<(Vec<u8>, u8, ContentId, u64)> = push
    .objects()
    .map(|(id, kind, content, len)| (id.to_vec(), kind, content, len))
    .collect();
  let len = u64::try_from(bytes.len()).unwrap();
  assert_eq!(held, vec![(id, 3, ContentId::of(bytes), len)]);
  push.abandon(&mut store).unwrap();
  assert_empty(&store);
}

/// A-581, §4.17.1 "Objects" item 1, D-6. Do: push a git-shaped object of ten windows (a full `a` window three times)
/// and an object equal to that window, commit both, and read the long one back through its row. Expect: the long
/// object's row names its root, a list; its bytes read back whole by its row's length; the short object is the long
/// one's window, one content held by the long one's list three times and by its own row once; no content longer than
/// a window; the store empty once the repository is released.
#[test]
fn an_object_longer_than_a_window_arrives_as_its_windows_and_reads_back_through_its_row() {
  let mut store = store();
  let mut repository = Index::new();
  let (long, short) = (long_object(), vec![b'a'; WINDOW]);
  let mut push = Quarantine::new();
  for bytes in [&long, &short] {
    let id = named(&GitShaped, 3, b"", bytes);
    assert_eq!(
      push.admit(&mut store, &GitShaped, git_arriving(&id, bytes)),
      Ok(Admitted::New)
    );
  }
  push.commit(&mut repository, &mut store).unwrap();
  let long_id = named(&GitShaped, 3, b"", &long);
  let (kind, len, root) = object_row(&repository.get(&store, &long_id).unwrap()).unwrap();
  assert_eq!((kind, store.form(&root)), (3, Some(Form::List)));
  assert_eq!(store.object_bytes(&root, len), Some(long.clone()));
  assert_eq!(store.holders(&ContentId::of(&short)), 4);
  assert!(
    store
      .identities()
      .all(|id| store.get(id).unwrap().len() <= WINDOW)
  );
  repository.release(&mut store).unwrap();
  assert_empty(&store);
}

/// A-581, §4.17.1 "Objects" item 4. Do: commit a push holding `o` → "one" (by a tool that names by the first byte);
/// then commit a second push holding `o` → "other" and `t` → "two". Expect: the second commit refused `RowDiffers`
/// with nothing of it committed, its objects still held for the caller to abandon, and once abandoned only the first
/// push's content left.
#[test]
fn a_commit_that_meets_a_differing_row_commits_nothing() {
  let mut store = store();
  let mut repository = Index::new();
  let mut first = Quarantine::new();
  first
    .admit(&mut store, &FirstByte, git_arriving(b"o", b"one"))
    .unwrap();
  assert_eq!(
    first.commit(&mut repository, &mut store),
    Ok(Batch {
      new: 1,
      identical: 0
    })
  );
  let mut second = Quarantine::new();
  for bytes in [&b"other"[..], b"two"] {
    second
      .admit(&mut store, &FirstByte, git_arriving(&bytes[..1], bytes))
      .unwrap();
  }
  let before = read(&repository, &store);
  assert_eq!(
    second.commit(&mut repository, &mut store),
    Err(CommitRefused::Index(IndexRefused::RowDiffers))
  );
  assert_eq!(read(&repository, &store), before);
  assert_eq!(second.len(), 2);
  second.abandon(&mut store).unwrap();
  assert_eq!(
    store.identities().copied().collect::<Vec<_>>(),
    vec![ContentId::of(b"one")]
  );
  repository.release(&mut store).unwrap();
  assert_empty(&store);
}

/// A parent of three runs of four rows each, one table a run, and a fork of it; the fences' charge the fork took.
fn parent_and_fork(store: &mut ContentStore) -> (Index, Index, u64) {
  let mut parent = Index::new();
  for round in 0..3u8 {
    for item in 0..4u8 {
      insert_named(&mut parent, store, &[round, item], &[b'x', round, item]);
    }
    parent.seal_all(store).unwrap();
  }
  let (counters, used, contents) = (parent.counters(), store.used(), store.len());
  let fork = parent.fork(store).unwrap();
  assert_eq!(
    (parent.counters(), fork.counters()),
    (counters, IndexCounters::default())
  );
  assert_eq!(store.len(), contents);
  let fences = u64::try_from(3 * (2 + FENCE_SLOT_BYTES)).unwrap();
  assert_eq!(store.used(), used + fences);
  (parent, fork, fences)
}

/// A-581, §4.17.1 "Objects" item 3 ("a fork shares its parent's tables by identity"). Do: build a repository of three
/// runs, fork it, then change each side and release the parent, then the fork. Expect: the fork writes no table, copies
/// no row and stores no byte, charging only its copy of the fences; every table held twice; both read alike; a row
/// inserted on one side never shows on the other; the fork reads whole after the parent is gone; the store empty at the
/// end.
#[test]
fn a_fork_holds_every_table_once_more_and_copies_no_row() {
  let mut store = store();
  let (mut parent, mut fork, _) = parent_and_fork(&mut store);
  assert!(parent.tables().all(|table| store.holders(&table) == 2));
  assert_eq!(read(&fork, &store), read(&parent, &store));
  insert_named(&mut fork, &mut store, b"fork only", b"fork bytes");
  insert_named(&mut parent, &mut store, b"parent only", b"parent bytes");
  assert!(parent.get(&store, b"fork only").is_none());
  assert!(fork.get(&store, b"parent only").is_none());
  let fork_rows = read(&fork, &store);
  parent.release(&mut store).unwrap();
  assert_eq!(read(&fork, &store), fork_rows);
  assert!(!store.contains(&ContentId::of(b"parent bytes")));
  assert!(fork.tables().all(|table| store.holders(&table) == 1));
  fork.release(&mut store).unwrap();
  assert_empty(&store);
}

/// A-581, §4.17.1 "Objects" item 3 (tables are content). Do: write the same rows into two repositories, seal both, then
/// release one and the other. Expect: the second seal's table is the first's, counted deduplicated and held twice; each
/// row's content held once by that one stored table, not once per repository; both read alike; the store empty at the
/// end.
#[test]
fn two_repositories_that_seal_the_same_rows_share_one_table() {
  let mut store = store();
  let (mut first, mut second) = (Index::new(), Index::new());
  for index in [&mut first, &mut second] {
    for name in [&b"a"[..], b"b"] {
      insert_named(index, &mut store, name, name);
    }
  }
  assert_eq!(store.holders(&ContentId::of(b"a")), 2);
  first.seal_all(&mut store).unwrap();
  second.seal_all(&mut store).unwrap();
  assert_eq!(second.counters().tables_deduplicated, 1);
  let tables: Vec<ContentId> = first.tables().collect();
  assert_eq!(second.tables().collect::<Vec<_>>(), tables);
  assert_eq!(
    (
      store.holders(&tables[0]),
      store.holders(&ContentId::of(b"a"))
    ),
    (2, 1)
  );
  assert_eq!(read(&first, &store), read(&second, &store));
  first.release(&mut store).unwrap();
  assert_eq!(read(&second, &store).len(), 2);
  second.release(&mut store).unwrap();
  assert_empty(&store);
}

/// Adds one run of one row a name to `index`.
fn add_runs(index: &mut Index, store: &mut ContentStore, names: &[&[u8]]) {
  for name in names {
    insert_named(index, store, name, name);
    index.seal_all(store).unwrap();
  }
}

/// A-581. Do: give a repository two runs, fork it, add two runs to the parent and merge every run it may; add two to
/// the fork and merge it; release the fork and merge the parent again. Expect: each merge reads only the runs its index
/// alone holds (the two shared runs untouched while the fork lives, merged once it is gone), reads unchanged by every
/// merge, and the store empty after both are released.
#[test]
fn a_merge_reads_only_the_runs_this_index_holds() {
  let mut store = store();
  let mut parent = Index::new();
  add_runs(&mut parent, &mut store, &[b"a0", b"a1"]);
  let shared = parent.run_tables();
  let mut fork = parent.fork(&mut store).unwrap();
  add_runs(&mut parent, &mut store, &[b"p1", b"p2"]);
  let before = read(&parent, &store);
  assert_eq!(parent.compact(&mut store, usize::MAX), Ok(2));
  assert_eq!(read(&parent, &store), before);
  let runs = parent.run_tables();
  assert_eq!((runs.len(), &runs[..2]), (3, &shared[..]));
  add_runs(&mut fork, &mut store, &[b"f1", b"f2"]);
  assert_eq!(fork.compact(&mut store, usize::MAX), Ok(2));
  fork.release(&mut store).unwrap();
  assert_eq!(parent.compact(&mut store, usize::MAX), Ok(4));
  assert_eq!(parent.run_tables().len(), 1);
  assert_eq!(read(&parent, &store), before);
  parent.release(&mut store).unwrap();
  assert_empty(&store);
}

/// Asks whether the tail is due to seal after an insert, and seals it when it is: due exactly when its rows no longer
/// fit one table (six rows of 34 bytes and the frame's 24 fit 256 bytes, seven do not), and a seal writes the six and
/// keeps the seventh.
fn seal_when_due(index: &mut Index, store: &mut ContentStore) {
  let due = index.seal_due(store);
  assert_eq!(due, index.tail_len() == 7);
  if due {
    assert_eq!(index.seal(store), Ok(6));
    assert_eq!(index.tail_len(), 1);
    let newest = index.run_tables().pop().unwrap();
    assert_eq!(newest.len(), 1);
    assert_eq!(store.get(&newest[0]).map(<[u8]>::len), Some(24 + 6 * 34));
  }
}

/// A-581, §4.17.1 "Every tunable is learned online" (the table's size and the tail's length are the derived window).
/// Do: insert forty rows one at a time under a window of 256 bytes, asking after each whether the tail is due to seal
/// and sealing when it is; then seal everything and merge every run. Expect: due exactly when the tail's rows no longer
/// fit one table; each seal writes one full table and keeps one row; six seals and four rows left in the tail; the
/// merge's seven tables at most a window each; all forty rows read back throughout.
#[test]
fn no_table_passes_the_window_and_the_tail_seals_once_its_rows_no_longer_fit_one() {
  let mut store = store();
  let mut index = Index::new();
  for key in 0..40u8 {
    insert_plain(&mut index, &mut store, key);
    seal_when_due(&mut index, &mut store);
  }
  assert_eq!((index.counters().seals, index.tail_len()), (6, 4));
  assert_eq!(index.rows(), 40);
  index.seal_all(&mut store).unwrap();
  assert_eq!(index.compact(&mut store, usize::MAX), Ok(40));
  assert_eq!(
    index.run_tables().iter().map(Vec::len).collect::<Vec<_>>(),
    vec![7]
  );
  assert_tables_within_the_window(&index, &store);
  assert_eq!(read(&index, &store).len(), 40);
  index.release(&mut store).unwrap();
  assert_empty(&store);
}

/// Looks up `count` keys no run holds.
fn miss(index: &Index, store: &ContentStore, count: u8) {
  for at in 0..count {
    assert!(index.get(store, &[200, at]).is_none());
  }
}

/// A run of thirty rows under two runs of six, the inserts timed at 10 ns a row.
fn a_large_run_under_two_small_ones(store: &mut ContentStore) -> Index {
  let mut index = Index::new();
  for keys in [0..30u8, 30..36, 36..42] {
    for key in keys {
      insert_plain(&mut index, store, key);
    }
    index.seal_all(store).unwrap();
  }
  index.inserts_took(420);
  index
}

/// A-581, §4.17.1 "Every tunable is learned online" (the merge point is learned). Do: a run of thirty rows under two
/// runs of six, the inserts timed at 10 ns a row; misses that visit all three runs timed at 100 ns a visit, asking
/// after each which merge is due and merging it; then fork, and miss on both sides. Expect: nothing due after one miss
/// (100 ns paid toward the small pair's 120, 200 toward all three's 420); the small pair due after two, and merging it
/// leaves the large run's tables as they were; nothing due once the merge is timed; and nothing due on either side of
/// the fork, whose shared runs pay no rent.
#[test]
fn two_small_runs_are_merged_and_the_large_run_below_them_is_left_as_it_is() {
  let mut store = store();
  let mut index = a_large_run_under_two_small_ones(&mut store);
  let large = index.run_tables()[0].clone();
  miss(&index, &store, 1);
  index.lookups_took(300);
  assert_eq!(index.compaction_due(&store), None);
  miss(&index, &store, 1);
  index.lookups_took(300);
  assert_eq!(index.compaction_due(&store), Some(2));
  assert_eq!(index.compact(&mut store, 2), Ok(12));
  assert_eq!(index.run_tables()[0], large);
  index.compaction_took(120);
  assert_eq!(index.compaction_due(&store), None);
  let mut fork = index.fork(&mut store).unwrap();
  for side in [&mut index, &mut fork] {
    miss(side, &store, 50);
    side.lookups_took(1_000_000);
    assert_eq!(side.compaction_due(&store), None);
  }
  fork.release(&mut store).unwrap();
  index.release(&mut store).unwrap();
  assert_empty(&store);
}

/// The index's own refusals at the budget: an insert, a batch whose rows fit one by one and not together, and a seal
/// whose table and fence do not fit in what the tail frees.
fn the_index_refuses_past_the_budget(store: &mut ContentStore) {
  let mut index = Index::new();
  let one_row = u64::try_from(1 + 1 + TAIL_SLOT_BYTES).unwrap();
  store.set_budget(one_row);
  let refused = Err(IndexRefused::Content(ContentRefused::OverBudget {
    need: one_row + 1,
    room: one_row,
  }));
  assert_eq!(index.insert(store, row(b"ab", &[], b"")), refused);
  let both = index.insert_all(store, &[row(b"a", &[], b""), row(b"b", &[], b"")]);
  assert!(matches!(
    both,
    Err(IndexRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  assert_eq!(index.insert(store, row(b"a", &[], b"")), Ok(Inserted::New));
  let sealed = index.seal_all(store);
  assert!(matches!(
    sealed,
    Err(IndexRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  assert_eq!((index.tail_len(), store.charged()), (1, one_row));
  assert!(index.get(store, b"a").is_some());
  index.release(store).unwrap();
}

/// A-581, §4.2. Do: refuse an admission past the budget, then the index's insert, batch and seal past it. Expect: each
/// refused `OverBudget` with nothing changed: no content, no row, no charge, and the tail still read whole after the
/// refused seal.
#[test]
fn an_arrival_an_insert_or_a_seal_past_the_budget_changes_nothing() {
  let mut store = ContentStore::new(4, WINDOW).unwrap();
  let mut push = Quarantine::new();
  let id = named(&GitShaped, 3, b"", b"too large");
  let refused = push.admit(&mut store, &GitShaped, git_arriving(&id, b"too large"));
  assert!(matches!(
    refused,
    Err(AdmitRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  assert!(push.is_empty() && store.is_empty());
  the_index_refuses_past_the_budget(&mut store);
  assert_empty(&store);
}

/// A held row inserted again: the same answered `Identical` with no second hold, another payload refused, and a batch of
/// a new row and the held one answered by count.
fn a_held_row_is_identical_or_differs(
  index: &mut Index,
  store: &mut ContentStore,
  content: &[ContentId],
) {
  assert_eq!(
    index.insert(store, row(b"k", content, b"1")),
    Ok(Inserted::Identical)
  );
  assert_eq!(store.holders(&content[0]), 2);
  assert_eq!(
    index.insert(store, row(b"k", content, b"2")),
    Err(IndexRefused::RowDiffers)
  );
  assert_eq!(
    index.insert_all(store, &[row(b"j", &[], b""), row(b"k", content, b"1")]),
    Ok(Batch {
      new: 1,
      identical: 1
    })
  );
}

/// A-581. Do: insert a batch out of order, a row naming a content the store does not hold, a row too long for a table
/// of one window, the same row twice, and a second row of a key with another payload. Expect: `Unsorted`, `NotHeld`
/// and `RowPastWindow` with nothing changed; `Identical` for the repeat with no second hold; `RowDiffers` for the
/// other payload.
#[test]
fn an_insert_is_checked_against_the_index_the_store_and_the_window() {
  let mut store = store();
  let mut index = Index::new();
  assert_eq!(
    index.insert_all(&mut store, &[row(b"b", &[], b""), row(b"a", &[], b"")]),
    Err(IndexRefused::Unsorted)
  );
  let absent = [ContentId::of(b"absent")];
  assert_eq!(
    index.insert(&mut store, row(b"k", &absent, b"")),
    Err(IndexRefused::Content(ContentRefused::NotHeld))
  );
  assert_eq!(
    index.insert(&mut store, row(b"long", &[], &[0; WINDOW])),
    Err(IndexRefused::RowPastWindow)
  );
  assert_eq!((index.rows(), store.charged()), (0, 0));
  let content = [store.put(b"held").unwrap()];
  assert_eq!(
    index.insert(&mut store, row(b"k", &content, b"1")),
    Ok(Inserted::New)
  );
  index.seal_all(&mut store).unwrap();
  a_held_row_is_identical_or_differs(&mut index, &mut store, &content);
  store.release(&content[0]).unwrap();
  index.release(&mut store).unwrap();
  assert_empty(&store);
}

/// Shape: the repositories the oracle drives at once, enough for a parent, a fork and a fork of either.
const INDEXES: usize = 4;
/// Shape: the pushes in flight at once.
const PUSHES: usize = 3;

/// The oracle's statement of an object's layout (§4.17.1 "Objects" item 1, D-6), written apart from the store's: one
/// window of at most a window's bytes; else its windows cut at `0, w, 2w, …` and named by plain BLAKE3, under lists of
/// at most `w / 32` identities named in the list form, a level at a time, up to one root. The root, and every node
/// with its bytes.
fn layout(bytes: &[u8]) -> (ContentId, BTreeMap<ContentId, Vec<u8>>) {
  let mut nodes = BTreeMap::new();
  if bytes.len() <= WINDOW {
    nodes.insert(ContentId::of(bytes), bytes.to_vec());
    return (ContentId::of(bytes), nodes);
  }
  let mut level: Vec<ContentId> = bytes
    .chunks(WINDOW)
    .map(|window| {
      nodes.insert(ContentId::of(window), window.to_vec());
      ContentId::of(window)
    })
    .collect();
  while level.len() > 1 {
    level = level
      .chunks(WINDOW / IDENTITY_BYTES)
      .map(|group| {
        let list: Vec<u8> = group.iter().flat_map(|id| id.0).collect();
        let id = ContentId::named(Form::List, &list);
        nodes.insert(id, list);
        id
      })
      .collect();
  }
  (level[0], nodes)
}

#[derive(Clone, Copy, Debug)]
enum Tool {
  Git,
  Hg,
}

/// An arrival the oracle makes: which push, by which tool, which blob as which kind, and whether the id is a lie.
#[derive(Clone, Copy, Debug)]
struct Arrival {
  push: usize,
  tool: Tool,
  blob: usize,
  kind: u8,
  lie: bool,
}

#[derive(Clone, Debug)]
enum Step {
  Admit(Arrival),
  Commit {
    push: usize,
    index: usize,
  },
  Abandon {
    push: usize,
  },
  Insert {
    index: usize,
    key: u8,
    blob: usize,
    payload: u8,
  },
  Get {
    index: usize,
    key: u8,
  },
  Seal {
    index: usize,
    all: bool,
  },
  Compact {
    index: usize,
    runs: usize,
  },
  Due {
    index: usize,
    lookups_ns: u64,
    inserts_ns: u64,
  },
  Fork {
    index: usize,
  },
  Release {
    index: usize,
  },
  Recreate {
    index: usize,
  },
}

/// The bytes the oracle's objects are drawn from: two equal ones under other names, so tools and pushes share them; a
/// long one of ten windows, three of them alike; and one equal to a window of the long one, so an object and another
/// object's window are one content.
fn blobs() -> Vec<Vec<u8>> {
  vec![
    b"".to_vec(),
    b"a".to_vec(),
    b"fn main() {}\n".to_vec(),
    b"shared".to_vec(),
    b"shared".to_vec(),
    long_object(),
    vec![b'a'; WINDOW],
  ]
}

fn step() -> impl Strategy<Value = Step> {
  let index = 0..INDEXES;
  let push = 0..PUSHES;
  let blob = 0..blobs().len();
  let tool = prop_oneof![Just(Tool::Git), Just(Tool::Hg)];
  prop_oneof![
    4 => (push.clone(), tool, blob.clone(), 1u8..3, proptest::bool::weighted(0.1))
      .prop_map(|(push, tool, blob, kind, lie)| Step::Admit(Arrival { push, tool, blob, kind, lie })),
    2 => (push.clone(), index.clone()).prop_map(|(push, index)| Step::Commit { push, index }),
    1 => push.prop_map(|push| Step::Abandon { push }),
    3 => (index.clone(), 0u8..12, blob, 0u8..2)
      .prop_map(|(index, key, blob, payload)| Step::Insert { index, key, blob, payload }),
    2 => (index.clone(), 0u8..12).prop_map(|(index, key)| Step::Get { index, key }),
    2 => (index.clone(), any::<bool>()).prop_map(|(index, all)| Step::Seal { index, all }),
    2 => (index.clone(), 0usize..5).prop_map(|(index, runs)| Step::Compact { index, runs }),
    1 => (index.clone(), 0u64..10_000, 0u64..10_000)
      .prop_map(|(index, lookups_ns, inserts_ns)| Step::Due { index, lookups_ns, inserts_ns }),
    1 => index.clone().prop_map(|index| Step::Fork { index }),
    1 => index.clone().prop_map(|index| Step::Release { index }),
    1 => index.prop_map(|index| Step::Recreate { index }),
  ]
}

/// The oracle's model: each index a map of key to body, each push a map of id to kind and bytes, and every blob's
/// layout.
struct Model {
  indexes: Vec<Option<BTreeMap<Vec<u8>, Vec<u8>>>>,
  pushes: Vec<BTreeMap<Vec<u8>, (u8, Vec<u8>)>>,
  blobs: Vec<Vec<u8>>,
  layouts: BTreeMap<ContentId, BTreeMap<ContentId, Vec<u8>>>,
}

impl Model {
  /// Every content the model names, with its bytes: each named object's whole tree.
  fn named(&self) -> BTreeMap<ContentId, Vec<u8>> {
    let mut roots = BTreeSet::new();
    for push in &self.pushes {
      roots.extend(push.values().map(|(_, bytes)| layout(bytes).0));
    }
    for index in self.indexes.iter().flatten() {
      for body in index.values() {
        roots.extend(RowRef::new(b"", body).unwrap().contents());
      }
    }
    roots
      .iter()
      .flat_map(|root| self.layouts[root].clone())
      .collect()
  }
}

/// The real side: the store, the indexes and the pushes.
struct Real {
  store: ContentStore,
  indexes: Vec<Option<Index>>,
  pushes: Vec<Quarantine>,
}

fn start() -> (Real, Model) {
  let real = Real {
    store: ContentStore::new(1 << 24, WINDOW).unwrap(),
    indexes: (0..INDEXES)
      .map(|slot| (slot == 0).then(Index::new))
      .collect(),
    pushes: (0..PUSHES).map(|_| Quarantine::new()).collect(),
  };
  let blobs = blobs();
  let layouts = blobs.iter().map(|bytes| layout(bytes.as_slice())).collect();
  let model = Model {
    indexes: (0..INDEXES)
      .map(|slot| (slot == 0).then(BTreeMap::new))
      .collect(),
    pushes: vec![BTreeMap::new(); PUSHES],
    blobs,
    layouts,
  };
  (real, model)
}

fn object_body(kind: u8, bytes: &[u8]) -> Vec<u8> {
  encode_body(
    &[layout(bytes).0],
    &object_payload(kind, u64::try_from(bytes.len()).unwrap()),
  )
  .unwrap()
}

fn admit(real: &mut Real, model: &mut Model, arrival: Arrival) {
  let bytes = model.blobs[arrival.blob].clone();
  let (kind, context) = (
    arrival.kind,
    match arrival.tool {
      Tool::Git => &b""[..],
      Tool::Hg => PARENTS,
    },
  );
  let id_of = |bytes: &[u8]| match arrival.tool {
    Tool::Git => named(&GitShaped, kind, context, bytes),
    Tool::Hg => named(&HgShaped, kind, context, bytes),
  };
  let id = if arrival.lie {
    id_of(&[bytes.as_slice(), b"!"].concat())
  } else {
    id_of(&bytes)
  };
  let arriving = Arriving {
    id: &id,
    kind,
    context,
    bytes: &bytes,
  };
  let push = &mut real.pushes[arrival.push];
  let result = match arrival.tool {
    Tool::Git => push.admit(&mut real.store, &GitShaped, arriving),
    Tool::Hg => push.admit(&mut real.store, &HgShaped, arriving),
  };
  if arrival.lie {
    assert!(matches!(result, Err(AdmitRefused::IdMismatch { .. })));
    return;
  }
  let expected = match model.pushes[arrival.push].insert(id, (kind, bytes)) {
    Some(_) => Admitted::Again,
    None => Admitted::New,
  };
  assert_eq!(result, Ok(expected));
}

fn commit(real: &mut Real, model: &mut Model, push: usize, index: usize) {
  let rows: Vec<(Vec<u8>, Vec<u8>)> = model.pushes[push]
    .iter()
    .map(|(id, (kind, bytes))| (id.clone(), object_body(*kind, bytes)))
    .collect();
  let (Some(real_index), Some(model_index)) =
    (real.indexes[index].as_mut(), model.indexes[index].as_mut())
  else {
    return;
  };
  let differs = rows
    .iter()
    .any(|(id, body)| model_index.get(id).is_some_and(|held| held != body));
  let result = real.pushes[push].commit(real_index, &mut real.store);
  if differs {
    assert_eq!(result, Err(CommitRefused::Index(IndexRefused::RowDiffers)));
    return;
  }
  let identical = rows
    .iter()
    .filter(|(id, _)| model_index.contains_key(id))
    .count();
  let new = u64::try_from(rows.len() - identical).unwrap();
  let identical = u64::try_from(identical).unwrap();
  assert_eq!(result, Ok(Batch { new, identical }));
  model_index.extend(rows);
  model.pushes[push].clear();
}

fn insert(real: &mut Real, model: &mut Model, index: usize, key: u8, blob: usize, payload: u8) {
  let root = layout(&model.blobs[blob]).0;
  let held = model.named().contains_key(&root);
  let (Some(real_index), Some(model_index)) =
    (real.indexes[index].as_mut(), model.indexes[index].as_mut())
  else {
    return;
  };
  let contents = [root];
  let body = encode_body(&contents, &[payload]).unwrap();
  let result = real_index.insert(&mut real.store, row(&[key], &contents, &[payload]));
  match model_index.get([key].as_slice()) {
    Some(held_body) if *held_body == body => assert_eq!(result, Ok(Inserted::Identical)),
    Some(_) => assert_eq!(result, Err(IndexRefused::RowDiffers)),
    None if !held => assert_eq!(result, Err(IndexRefused::Content(ContentRefused::NotHeld))),
    None => {
      assert_eq!(result, Ok(Inserted::New));
      model_index.insert(vec![key], body);
    }
  }
}

fn get(real: &Real, model: &Model, index: usize, key: u8) {
  if let (Some(real_index), Some(model_index)) = (&real.indexes[index], &model.indexes[index]) {
    let found = real_index
      .get(&real.store, &[key])
      .map(|row| row.body().to_vec());
    assert_eq!(found.as_ref(), model_index.get([key].as_slice()));
  }
}

fn seal(real: &mut Real, index: usize, all: bool) {
  let Some(real_index) = real.indexes[index].as_mut() else {
    return;
  };
  if all {
    real_index.seal_all(&mut real.store).unwrap();
    assert_eq!(real_index.tail_len(), 0);
  } else {
    real_index.seal(&mut real.store).unwrap();
    assert!(!real_index.seal_due(&real.store));
  }
}

fn compact(real: &mut Real, index: usize, runs: usize) {
  if let Some(real_index) = real.indexes[index].as_mut() {
    real_index.compact(&mut real.store, runs).unwrap();
  }
}

/// Reports times, and merges what the rule says is due.
fn due(real: &mut Real, index: usize, lookups_ns: u64, inserts_ns: u64) {
  let Some(real_index) = real.indexes[index].as_mut() else {
    return;
  };
  real_index.lookups_took(lookups_ns);
  real_index.inserts_took(inserts_ns);
  if let Some(runs) = real_index.compaction_due(&real.store) {
    real_index.compact(&mut real.store, runs).unwrap();
    real_index.compaction_took(lookups_ns);
  }
}

fn fork(real: &mut Real, model: &mut Model, index: usize) {
  let Some(parent) = real.indexes[index].as_mut() else {
    return;
  };
  let forked = parent.fork(&mut real.store).unwrap();
  match real.indexes.iter().position(Option::is_none) {
    Some(slot) => {
      real.indexes[slot] = Some(forked);
      model.indexes[slot] = model.indexes[index].clone();
    }
    None => forked.release(&mut real.store).unwrap(),
  }
}

fn release(real: &mut Real, model: &mut Model, index: usize) {
  if let Some(released) = real.indexes[index].take() {
    released.release(&mut real.store).unwrap();
    model.indexes[index] = None;
  }
}

fn recreate(real: &mut Real, model: &mut Model, index: usize) {
  if real.indexes[index].is_none() {
    real.indexes[index] = Some(Index::new());
    model.indexes[index] = Some(BTreeMap::new());
  }
}

fn abandon(real: &mut Real, model: &mut Model, push: usize) {
  real.pushes[push].abandon(&mut real.store).unwrap();
  model.pushes[push].clear();
}

fn apply(real: &mut Real, model: &mut Model, step: &Step) {
  match *step {
    Step::Admit(arrival) => admit(real, model, arrival),
    Step::Commit { push, index } => commit(real, model, push, index),
    Step::Abandon { push } => abandon(real, model, push),
    Step::Insert {
      index,
      key,
      blob,
      payload,
    } => insert(real, model, index, key, blob, payload),
    Step::Get { index, key } => get(real, model, index, key),
    Step::Seal { index, all } => seal(real, index, all),
    Step::Compact { index, runs } => compact(real, index, runs),
    Step::Due {
      index,
      lookups_ns,
      inserts_ns,
    } => due(real, index, lookups_ns, inserts_ns),
    Step::Fork { index } => fork(real, model, index),
    Step::Release { index } => release(real, model, index),
    Step::Recreate { index } => recreate(real, model, index),
  }
}

/// One index reads as its model map, row by row and key by key, and holds no table longer than the window.
fn check_index(index: &Index, store: &ContentStore, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
  let rows: BTreeMap<Vec<u8>, Vec<u8>> = read(index, store).into_iter().collect();
  assert_eq!(&rows, model);
  assert_eq!(index.rows(), u64::try_from(model.len()).unwrap());
  for (key, body) in model {
    let found = index.get(store, key).map(|row| row.body().to_vec());
    assert_eq!(found.as_ref(), Some(body));
  }
  assert_tables_within_the_window(index, store);
}

/// Every index reads as its model; the tables the live indexes hold.
fn check_indexes(real: &Real, model: &Model) -> BTreeSet<ContentId> {
  let mut tables = BTreeSet::new();
  for (real_index, model_index) in real.indexes.iter().zip(&model.indexes) {
    assert_eq!(real_index.is_some(), model_index.is_some());
    if let (Some(real_index), Some(model_index)) = (real_index, model_index) {
      check_index(real_index, &real.store, model_index);
      tables.extend(real_index.tables());
    }
  }
  tables
}

/// Every index reads as its model; every named content is held with its bytes; every content held is named or is a
/// live index's table.
fn check(real: &Real, model: &Model) {
  let tables = check_indexes(real, model);
  let named = model.named();
  for (content, bytes) in &named {
    assert_eq!(real.store.get(content), Some(bytes.as_slice()));
  }
  for content in real.store.identities() {
    assert!(
      named.contains_key(content) || tables.contains(content),
      "an orphan content"
    );
  }
}

/// Releases every index and abandons every push.
fn finish(real: &mut Real) {
  for index in real.indexes.iter_mut() {
    if let Some(index) = index.take() {
      index.release(&mut real.store).unwrap();
    }
  }
  for push in &mut real.pushes {
    push.abandon(&mut real.store).unwrap();
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(300), ..ProptestConfig::default() }))]

  /// A-581, §4.17.1 "Objects" items 1, 3, 4 and 5. Do: run generated histories of arrivals (lies among them, through
  /// two tools over shared bytes, a long object among them whose window is another object), commits, abandons, raw
  /// inserts, lookups, seals due and whole, merges of any number of runs, timed merges the rule calls due, forks and
  /// releases over four repositories and three pushes sharing one store. Expect: after every step each repository
  /// reads as its model map, every refusal is the model's, no table is longer than the window, every content named is
  /// held with its bytes as the oracle's own layout of each object says, and no content is held that nothing names;
  /// once everything is released and abandoned, the store is empty and charges nothing.
  #[test]
  fn the_object_store_reads_as_plain_maps_over_every_history(steps in proptest::collection::vec(step(), 0..120)) {
    let (mut real, mut model) = start();
    for step in &steps {
      apply(&mut real, &mut model, step);
      check(&real, &model);
    }
    finish(&mut real);
    prop_assert!(real.store.is_empty());
    prop_assert_eq!((real.store.used(), real.store.charged()), (0, 0));
  }
}
