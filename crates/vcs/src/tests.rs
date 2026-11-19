//! The object store held to plain maps (A-581, §4.17.1 shared piece 3): arrivals, commits, folds, compactions, forks
//! and releases over several repositories sharing one store.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use crate::arrival::{
  Admitted, ArrivalRefused, Arriving, Naming, Quarantine, object_payload, object_row,
};
use crate::content::{ContentId, ContentRefused, ContentStore};
use crate::index::{Batch, Index, IndexRefused, Inserted, NewRow, TAIL_SLOT_BYTES};
use crate::table::{RowRef, encode_body};

/// Format: the ids the test tools give, as long as git's and Mercurial's SHA-1 names.
const ID_BYTES: usize = 20;

/// A stand-in for git's naming: the kind and the length framed before the bytes, as git frames `<type> <length>\0`.
/// The store never computes a tool's hash itself, so a test tool's hash stands for the real one.
struct GitShaped;

impl Naming for GitShaped {
  fn name(&self, kind: u8, _context: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[kind]);
    hasher.update(format!(" {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher.finalize().as_bytes()[..ID_BYTES].to_vec()
  }
}

/// A stand-in for Mercurial's naming: the parents (the context) hashed before the text.
struct HgShaped;

impl Naming for HgShaped {
  fn name(&self, kind: u8, context: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hg");
    hasher.update(&[kind]);
    hasher.update(context);
    hasher.update(bytes);
    hasher.finalize().as_bytes()[..ID_BYTES].to_vec()
  }
}

/// A tool whose hash collides by design: the first byte of the bytes, so the tests can bring one id twice with other
/// bytes.
struct FirstByte;

impl Naming for FirstByte {
  fn name(&self, _kind: u8, _context: &[u8], bytes: &[u8]) -> Vec<u8> {
    bytes.iter().take(1).copied().collect()
  }
}

/// The parents a Mercurial-shaped object is named with.
const PARENTS: &[u8] = &[7; 40];

fn git_arriving<'a>(id: &'a [u8], bytes: &'a [u8]) -> Arriving<'a> {
  Arriving {
    id,
    kind: 3,
    context: b"",
    bytes,
  }
}

/// A row naming the content of `bytes`, which the caller holds, under `key`.
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

fn read(index: &Index, store: &ContentStore) -> Vec<(Vec<u8>, Vec<u8>)> {
  index
    .iter(store)
    .map(|row| (row.key().to_vec(), row.body().to_vec()))
    .collect()
}

/// A-581, §4.17.1 "Objects" item 1. Do: bring the same bytes as a git blob to one repository and as a Mercurial file
/// revision to another, commit both, fold both, then release one repository and the other. Expect: one content for
/// both tools from the first arrival on (the second put deduplicated), each repository's row naming it with its own
/// kind and length, two holders through tails and then tables, the bytes kept while one repository names them, and an
/// empty store at the end.
#[test]
fn the_same_bytes_from_git_and_mercurial_are_one_content_held_by_both_repositories() {
  let mut store = ContentStore::new(1 << 20);
  let (mut git_repository, mut hg_repository) = (Index::new(), Index::new());
  let bytes: &[u8] = b"int main(void) { return 0; }\n";
  let git_id = GitShaped.name(3, b"", bytes);
  let hg_id = HgShaped.name(1, PARENTS, bytes);
  assert_ne!(git_id, hg_id);
  let (mut git_push, mut hg_push) = (Quarantine::new(), Quarantine::new());
  assert_eq!(
    git_push.admit(&mut store, &GitShaped, git_arriving(&git_id, bytes)),
    Ok(Admitted::New)
  );
  let hg_arriving = Arriving {
    id: &hg_id,
    kind: 1,
    context: PARENTS,
    bytes,
  };
  assert_eq!(
    hg_push.admit(&mut store, &HgShaped, hg_arriving),
    Ok(Admitted::New)
  );
  assert_eq!(store.len(), 1);
  assert_eq!(store.counters().deduplicated, 1);
  git_push.commit(&mut git_repository, &mut store).unwrap();
  hg_push.commit(&mut hg_repository, &mut store).unwrap();
  let content = ContentId::of(bytes);
  let len = bytes.len() as u64;
  let git_row = git_repository.get(&store, &git_id).unwrap();
  assert_eq!(object_row(&git_row), Some((3, len, content)));
  let hg_row = hg_repository.get(&store, &hg_id).unwrap();
  assert_eq!(object_row(&hg_row), Some((1, len, content)));
  assert_eq!(store.holders(&content), 2);
  git_repository.fold(&mut store).unwrap();
  hg_repository.fold(&mut store).unwrap();
  assert_eq!(store.holders(&content), 2);
  git_repository.release(&mut store).unwrap();
  assert_eq!(store.get(&content), Some(bytes));
  hg_repository.release(&mut store).unwrap();
  assert!(store.is_empty());
  assert_eq!((store.used(), store.charged()), (0, 0));
}

/// A-581, §4.17.1 "Objects" item 5 and "Failure matrix". Do: admit two objects of a push, then one whose bytes do not
/// hash to the id claimed, then one that brings an id the push holds with other bytes; abandon the push. Expect: the
/// lie refused `IdMismatch` naming the id and the collision `Collision`, neither storing a byte; the same object
/// brought again answered `Again`; the abandoned push leaving the store empty and the repository without a row.
#[test]
fn an_object_whose_bytes_do_not_hash_to_its_id_is_refused_and_its_push_leaves_nothing() {
  let mut store = ContentStore::new(1 << 20);
  let repository = Index::new();
  let mut push = Quarantine::new();
  for bytes in [&b"one"[..], b"two"] {
    let id = GitShaped.name(3, b"", bytes);
    assert_eq!(
      push.admit(&mut store, &GitShaped, git_arriving(&id, bytes)),
      Ok(Admitted::New)
    );
  }
  let claimed = GitShaped.name(3, b"", b"what the client said");
  assert_eq!(
    push.admit(&mut store, &GitShaped, git_arriving(&claimed, b"what it sent")),
    Err(ArrivalRefused::IdMismatch {
      id: claimed.clone().into_boxed_slice()
    })
  );
  assert_eq!((store.len(), push.len()), (2, 2));
  let again = GitShaped.name(3, b"", b"one");
  assert_eq!(
    push.admit(&mut store, &GitShaped, git_arriving(&again, b"one")),
    Ok(Admitted::Again)
  );
  assert_eq!(store.holders(&ContentId::of(b"one")), 1);
  let mut weak = Quarantine::new();
  weak
    .admit(&mut store, &FirstByte, git_arriving(b"o", b"one"))
    .unwrap();
  assert_eq!(
    weak.admit(&mut store, &FirstByte, git_arriving(b"o", b"other")),
    Err(ArrivalRefused::Collision {
      id: Box::from(&b"o"[..])
    })
  );
  assert!(!store.contains(&ContentId::of(b"other")));
  weak.abandon(&mut store).unwrap();
  push.abandon(&mut store).unwrap();
  assert!(push.is_empty());
  assert!(store.is_empty());
  assert_eq!(repository.rows(), 0);
}

/// A-581, §4.17.1 "Objects" item 4. Do: commit a push holding `o` → "one" (by a tool that names by the first byte);
/// then commit a second push holding `o` → "other" and `t` → "two". Expect: the second commit refused `RowDiffers`
/// with nothing of it committed, its objects still held for the caller to abandon, and once abandoned only the first
/// push's content left.
#[test]
fn a_commit_that_meets_a_differing_row_commits_nothing() {
  let mut store = ContentStore::new(1 << 20);
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
  second
    .admit(&mut store, &FirstByte, git_arriving(b"o", b"other"))
    .unwrap();
  second
    .admit(&mut store, &FirstByte, git_arriving(b"t", b"two"))
    .unwrap();
  let before = read(&repository, &store);
  assert_eq!(
    second.commit(&mut repository, &mut store),
    Err(ArrivalRefused::Index(IndexRefused::RowDiffers))
  );
  assert_eq!(read(&repository, &store), before);
  assert_eq!(second.len(), 2);
  second.abandon(&mut store).unwrap();
  assert_eq!(
    store.identities().copied().collect::<Vec<_>>(),
    vec![ContentId::of(b"one")]
  );
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A-581, §4.17.1 "Objects" item 3 ("a fork shares its parent's tables by identity"). Do: build a repository of three
/// tables, fork it, then change each side and release the parent, then the fork. Expect: the fork writes no table,
/// copies no row and stores no byte; every table held twice; both read alike; a row inserted on one side never shows
/// on the other; the fork reads whole after the parent is gone; and the store is empty at the end.
#[test]
fn a_fork_holds_every_table_once_more_and_copies_no_row() {
  let mut store = ContentStore::new(1 << 20);
  let mut parent = Index::new();
  for round in 0..3u8 {
    for item in 0..4u8 {
      insert_named(&mut parent, &mut store, &[round, item], &[b'x', round, item]);
    }
    parent.fold(&mut store).unwrap();
  }
  let (counters, used, contents) = (parent.counters(), store.used(), store.len());
  let mut fork = parent.fork(&mut store).unwrap();
  assert_eq!(parent.counters(), counters);
  assert_eq!(fork.counters(), Default::default());
  assert_eq!((store.used(), store.len()), (used, contents));
  for table in parent.tables() {
    assert_eq!(store.holders(&table), 2);
  }
  assert_eq!(read(&fork, &store), read(&parent, &store));
  insert_named(&mut fork, &mut store, b"fork only", b"fork bytes");
  insert_named(&mut parent, &mut store, b"parent only", b"parent bytes");
  assert!(parent.get(&store, b"fork only").is_none());
  assert!(fork.get(&store, b"parent only").is_none());
  let fork_rows = read(&fork, &store);
  parent.release(&mut store).unwrap();
  assert_eq!(read(&fork, &store), fork_rows);
  assert!(!store.contains(&ContentId::of(b"parent bytes")));
  for table in fork.tables() {
    assert_eq!(store.holders(&table), 1);
  }
  fork.release(&mut store).unwrap();
  assert!(store.is_empty());
  assert_eq!((store.used(), store.charged()), (0, 0));
}

/// A-581. Do: give a repository two tables, fork it, add a table and a tail to the parent and compact it; add two tables
/// to the fork and compact it; release the fork and compact the parent again. Expect: each compaction merges only the
/// layers its index alone holds (the two shared tables untouched while the fork lives, merged once it is gone), reads
/// unchanged by every compaction, and the store empty after both are released.
#[test]
fn a_compaction_merges_only_the_tables_this_index_holds() {
  let mut store = ContentStore::new(1 << 20);
  let mut parent = Index::new();
  for round in 0..2u8 {
    insert_named(&mut parent, &mut store, &[b'a', round], &[round]);
    parent.fold(&mut store).unwrap();
  }
  let shared: Vec<ContentId> = parent.tables().collect();
  let mut fork = parent.fork(&mut store).unwrap();
  insert_named(&mut parent, &mut store, b"p1", b"p1");
  parent.fold(&mut store).unwrap();
  insert_named(&mut parent, &mut store, b"p2", b"p2");
  let before = read(&parent, &store);
  assert_eq!(parent.compact(&mut store), Ok(2));
  assert_eq!(read(&parent, &store), before);
  let tables: Vec<ContentId> = parent.tables().collect();
  assert_eq!(tables.len(), 3);
  assert_eq!(&tables[..2], &shared[..]);
  for table in &shared {
    assert_eq!(store.holders(table), 2);
  }
  assert_eq!(parent.counters().compactions, 1);
  for name in [&b"f1"[..], b"f2"] {
    insert_named(&mut fork, &mut store, name, name);
    fork.fold(&mut store).unwrap();
  }
  let fork_before = read(&fork, &store);
  assert_eq!(fork.compact(&mut store), Ok(2));
  assert_eq!(read(&fork, &store), fork_before);
  assert_eq!(fork.tables().count(), 3);
  fork.release(&mut store).unwrap();
  assert_eq!(parent.compact(&mut store), Ok(4));
  assert_eq!(parent.tables().count(), 1);
  assert_eq!(read(&parent, &store), before);
  assert_eq!(parent.compact(&mut store), Ok(0), "one table, nothing to merge");
  parent.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A-581, §4.17.1 "Every tunable is learned online". Do: write three tables of ten rows, timing the inserts at
/// 100 ns a row, then time batches of lookups that miss (each probes three compactable tables, two beyond its first),
/// compact, time the compaction, and miss again. Expect: due exactly when the rent reaches what building the 30 rows
/// cost (3,000 ns), not before; after the compaction the price is its own measured rate and lookups that probe one
/// table pay no rent; a fork's shared tables pay none either.
#[test]
fn compaction_is_due_once_the_probes_it_would_save_have_paid_for_it() {
  let mut store = ContentStore::new(1 << 20);
  let mut index = Index::new();
  for round in 0..3u8 {
    for item in 0..10u8 {
      insert_named(&mut index, &mut store, &[round, item], &[round, item]);
    }
    index.fold(&mut store).unwrap();
  }
  index.inserts_took(3_000);
  index.lookups_took(0);
  assert!(!index.compaction_due());
  for miss in 0..10u8 {
    assert!(index.get(&store, &[9, miss]).is_none());
  }
  index.lookups_took(1_500);
  assert_eq!(index.rule().rent_ns(), 1_000);
  assert!(!index.compaction_due());
  for miss in 0..20u8 {
    assert!(index.get(&store, &[8, miss]).is_none());
  }
  index.lookups_took(3_000);
  assert_eq!(index.rule().rent_ns(), 3_000);
  assert!(index.compaction_due());
  assert_eq!(index.compact(&mut store), Ok(30));
  index.compaction_took(600);
  assert_eq!(index.rule().predicted_ns(30), Some(600));
  assert!(!index.compaction_due());
  for miss in 0..50u8 {
    assert!(index.get(&store, &[7, miss]).is_none());
  }
  index.lookups_took(1_000_000);
  assert_eq!(index.rule().rent_ns(), 0);
  insert_named(&mut index, &mut store, b"new", b"new");
  index.fold(&mut store).unwrap();
  let mut fork = index.fork(&mut store).unwrap();
  index.lookups_took(0);
  for miss in 0..50u8 {
    assert!(index.get(&store, &[6, miss]).is_none());
    assert!(fork.get(&store, &[6, miss]).is_none());
  }
  index.lookups_took(1_000_000);
  fork.lookups_took(1_000_000);
  assert_eq!((index.rule().rent_ns(), fork.rule().rent_ns()), (0, 0));
  fork.release(&mut store).unwrap();
  index.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A-581, §4.2. Do: refuse an admission past the budget, an insert past it, a batch whose rows fit one by one and not
/// together, and a fold whose table does not fit in what the tail frees. Expect: each refused `OverBudget` with nothing
/// changed: no content, no row, no charge, and the tail still read whole after the refused fold.
#[test]
fn an_arrival_an_insert_or_a_fold_past_the_budget_changes_nothing() {
  let mut store = ContentStore::new(4);
  let mut push = Quarantine::new();
  let id = GitShaped.name(3, b"", b"too large");
  assert!(matches!(
    push.admit(&mut store, &GitShaped, git_arriving(&id, b"too large")),
    Err(ArrivalRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  assert!(push.is_empty() && store.is_empty());
  let mut index = Index::new();
  let one_row = (1 + 1 + TAIL_SLOT_BYTES) as u64;
  store.set_budget(one_row);
  assert!(matches!(
    index.insert(&mut store, row(b"ab", &[], b"")),
    Err(IndexRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  assert!(matches!(
    index.insert_all(&mut store, &[row(b"a", &[], b""), row(b"b", &[], b"")]),
    Err(IndexRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  assert_eq!((index.rows(), store.charged()), (0, 0));
  assert_eq!(index.insert(&mut store, row(b"a", &[], b"")), Ok(Inserted::New));
  assert_eq!(store.room(), 0);
  assert!(matches!(
    index.fold(&mut store),
    Err(IndexRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  assert_eq!(index.tail_len(), 1);
  assert_eq!(store.charged(), one_row);
  assert!(index.get(&store, b"a").is_some());
  index.release(&mut store).unwrap();
  assert_eq!((store.used(), store.charged()), (0, 0));
}

/// A-581. Do: insert a batch out of order, a row naming a content the store does not hold, the same row twice, and a
/// second row of a key with another payload. Expect: `Unsorted` and `NotHeld` with nothing changed, `Identical` for the
/// repeat with no second hold, and `RowDiffers` for the other payload.
#[test]
fn an_insert_is_checked_against_the_index_and_the_store() {
  let mut store = ContentStore::new(1 << 20);
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
  assert_eq!(index.rows(), 0);
  let content = [store.put(b"held").unwrap()];
  assert_eq!(index.insert(&mut store, row(b"k", &content, b"1")), Ok(Inserted::New));
  index.fold(&mut store).unwrap();
  assert_eq!(
    index.insert(&mut store, row(b"k", &content, b"1")),
    Ok(Inserted::Identical)
  );
  assert_eq!(store.holders(&content[0]), 2);
  assert_eq!(
    index.insert(&mut store, row(b"k", &content, b"2")),
    Err(IndexRefused::RowDiffers)
  );
  assert_eq!(
    index.insert_all(&mut store, &[row(b"j", &[], b""), row(b"k", &content, b"1")]),
    Ok(Batch {
      new: 1,
      identical: 1
    })
  );
  store.release(&content[0]).unwrap();
  index.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// The bytes the oracle's objects are drawn from: two equal ones under other names, so tools and pushes share them.
const BLOBS: [&[u8]; 5] = [b"", b"a", b"fn main() {}\n", b"shared", b"shared"];

/// Shape: the repositories the oracle drives at once, enough for a parent, a fork and a fork of either.
const INDEXES: usize = 4;
/// Shape: the pushes in flight at once.
const PUSHES: usize = 3;

#[derive(Clone, Copy, Debug)]
enum Tool {
  Git,
  Hg,
}

#[derive(Clone, Debug)]
enum Step {
  Admit {
    push: usize,
    tool: Tool,
    blob: usize,
    kind: u8,
    lie: bool,
  },
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
  Fold {
    index: usize,
  },
  Compact {
    index: usize,
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

fn step() -> impl Strategy<Value = Step> {
  let index = 0..INDEXES;
  let push = 0..PUSHES;
  let blob = 0..BLOBS.len();
  let tool = prop_oneof![Just(Tool::Git), Just(Tool::Hg)];
  prop_oneof![
    4 => (push.clone(), tool, blob.clone(), 1u8..3, proptest::bool::weighted(0.1))
      .prop_map(|(push, tool, blob, kind, lie)| Step::Admit { push, tool, blob, kind, lie }),
    2 => (push.clone(), index.clone()).prop_map(|(push, index)| Step::Commit { push, index }),
    1 => push.prop_map(|push| Step::Abandon { push }),
    3 => (index.clone(), 0u8..6, blob, 0u8..2)
      .prop_map(|(index, key, blob, payload)| Step::Insert { index, key, blob, payload }),
    2 => (index.clone(), 0u8..6).prop_map(|(index, key)| Step::Get { index, key }),
    2 => index.clone().prop_map(|index| Step::Fold { index }),
    2 => index.clone().prop_map(|index| Step::Compact { index }),
    1 => index.clone().prop_map(|index| Step::Fork { index }),
    1 => index.clone().prop_map(|index| Step::Release { index }),
    1 => index.prop_map(|index| Step::Recreate { index }),
  ]
}

/// The oracle's model: each index a map of key to body, each push a map of id to kind and bytes.
#[derive(Default)]
struct Model {
  indexes: Vec<Option<BTreeMap<Vec<u8>, Vec<u8>>>>,
  pushes: Vec<BTreeMap<Vec<u8>, (u8, Vec<u8>)>>,
}

impl Model {
  /// Every content the model names, with its bytes.
  fn named(&self) -> BTreeMap<ContentId, Vec<u8>> {
    let mut named = BTreeMap::new();
    for push in &self.pushes {
      for (_, bytes) in push.values() {
        named.insert(ContentId::of(bytes), bytes.clone());
      }
    }
    for index in self.indexes.iter().flatten() {
      for body in index.values() {
        for content in RowRef::new(b"", body).unwrap().contents() {
          let bytes = BLOBS.iter().find(|blob| ContentId::of(blob) == content).unwrap();
          named.insert(content, bytes.to_vec());
        }
      }
    }
    named
  }
}

/// The real side: the store, the indexes and the pushes.
struct Real {
  store: ContentStore,
  indexes: Vec<Option<Index>>,
  pushes: Vec<Quarantine>,
}

fn object_body(kind: u8, bytes: &[u8]) -> Vec<u8> {
  encode_body(
    &[ContentId::of(bytes)],
    &object_payload(kind, bytes.len() as u64),
  )
  .unwrap()
}

fn admit(real: &mut Real, model: &mut Model, push: usize, tool: Tool, blob: usize, kind: u8, lie: bool) {
  let bytes = BLOBS[blob];
  let named = |bytes: &[u8]| match tool {
    Tool::Git => GitShaped.name(kind, b"", bytes),
    Tool::Hg => HgShaped.name(kind, PARENTS, bytes),
  };
  let id = if lie { named(&[bytes, b"!"].concat()) } else { named(bytes) };
  let arriving = Arriving {
    id: &id,
    kind,
    context: match tool {
      Tool::Git => b"",
      Tool::Hg => PARENTS,
    },
    bytes,
  };
  let result = match tool {
    Tool::Git => real.pushes[push].admit(&mut real.store, &GitShaped, arriving),
    Tool::Hg => real.pushes[push].admit(&mut real.store, &HgShaped, arriving),
  };
  if lie {
    assert!(matches!(result, Err(ArrivalRefused::IdMismatch { .. })));
    return;
  }
  let expected = match model.pushes[push].insert(id, (kind, bytes.to_vec())) {
    Some(_) => Admitted::Again,
    None => Admitted::New,
  };
  assert_eq!(result, Ok(expected));
}

fn commit(real: &mut Real, model: &mut Model, push: usize, index: usize) {
  let (Some(real_index), Some(model_index)) = (real.indexes[index].as_mut(), model.indexes[index].as_mut()) else {
    return;
  };
  let rows: Vec<(Vec<u8>, Vec<u8>)> = model.pushes[push]
    .iter()
    .map(|(id, (kind, bytes))| (id.clone(), object_body(*kind, bytes)))
    .collect();
  let differs = rows
    .iter()
    .any(|(id, body)| model_index.get(id).is_some_and(|held| held != body));
  let result = real.pushes[push].commit(real_index, &mut real.store);
  if differs {
    assert_eq!(result, Err(ArrivalRefused::Index(IndexRefused::RowDiffers)));
    return;
  }
  let identical = rows.iter().filter(|(id, _)| model_index.contains_key(id)).count() as u64;
  assert_eq!(
    result,
    Ok(Batch {
      new: rows.len() as u64 - identical,
      identical
    })
  );
  model_index.extend(rows);
  model.pushes[push].clear();
}

fn insert(real: &mut Real, model: &mut Model, index: usize, key: u8, blob: usize, payload: u8) {
  let held = model.named().contains_key(&ContentId::of(BLOBS[blob]));
  let (Some(real_index), Some(model_index)) = (real.indexes[index].as_mut(), model.indexes[index].as_mut()) else {
    return;
  };
  let contents = [ContentId::of(BLOBS[blob])];
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

fn apply(real: &mut Real, model: &mut Model, step: &Step) {
  match *step {
    Step::Admit { push, tool, blob, kind, lie } => admit(real, model, push, tool, blob, kind, lie),
    Step::Commit { push, index } => commit(real, model, push, index),
    Step::Abandon { push } => {
      real.pushes[push].abandon(&mut real.store).unwrap();
      model.pushes[push].clear();
    }
    Step::Insert { index, key, blob, payload } => insert(real, model, index, key, blob, payload),
    Step::Get { index, key } => {
      if let (Some(real_index), Some(model_index)) = (&real.indexes[index], &model.indexes[index]) {
        let found = real_index.get(&real.store, &[key]).map(|row| row.body().to_vec());
        assert_eq!(found.as_ref(), model_index.get([key].as_slice()));
      }
    }
    Step::Fold { index } => {
      if let Some(real_index) = real.indexes[index].as_mut() {
        real_index.fold(&mut real.store).unwrap();
      }
    }
    Step::Compact { index } => {
      if let Some(real_index) = real.indexes[index].as_mut() {
        real_index.compact(&mut real.store).unwrap();
      }
    }
    Step::Fork { index } => fork(real, model, index),
    Step::Release { index } => {
      if let Some(released) = real.indexes[index].take() {
        released.release(&mut real.store).unwrap();
        model.indexes[index] = None;
      }
    }
    Step::Recreate { index } => {
      if real.indexes[index].is_none() {
        real.indexes[index] = Some(Index::new());
        model.indexes[index] = Some(BTreeMap::new());
      }
    }
  }
}

/// Every index reads as its model; every named content is held with its bytes; every content held is named or is a
/// live index's table.
fn check(real: &Real, model: &Model) {
  let mut tables = BTreeSet::new();
  for (real_index, model_index) in real.indexes.iter().zip(&model.indexes) {
    assert_eq!(real_index.is_some(), model_index.is_some());
    if let (Some(real_index), Some(model_index)) = (real_index, model_index) {
      let rows: BTreeMap<Vec<u8>, Vec<u8>> = read(real_index, &real.store).into_iter().collect();
      assert_eq!(&rows, model_index);
      assert_eq!(real_index.rows(), model_index.len() as u64);
      for (key, body) in model_index {
        assert_eq!(real_index.get(&real.store, key).map(|row| row.body().to_vec()).as_ref(), Some(body));
      }
      tables.extend(real_index.tables());
    }
  }
  let named = model.named();
  for (content, bytes) in &named {
    assert_eq!(real.store.get(content), Some(bytes.as_slice()));
  }
  for content in real.store.identities() {
    assert!(named.contains_key(content) || tables.contains(content), "an orphan content");
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(300), ..ProptestConfig::default() }))]

  /// A-581, §4.17.1 "Objects" items 1, 3, 4 and 5. Do: run generated histories of arrivals (lies among them, through
  /// two tools over shared bytes), commits, abandons, raw inserts, lookups, folds, compactions, forks and releases over
  /// four repositories and three pushes sharing one store. Expect: after every step each repository reads as its
  /// model map, every refusal is the model's, every content named is held with its bytes and no content is held that
  /// nothing names; once everything is released and abandoned, the store is empty and charges nothing.
  #[test]
  fn the_object_store_reads_as_plain_maps_over_every_history(steps in proptest::collection::vec(step(), 0..120)) {
    let mut real = Real {
      store: ContentStore::new(1 << 24),
      indexes: (0..INDEXES).map(|slot| (slot == 0).then(Index::new)).collect(),
      pushes: (0..PUSHES).map(|_| Quarantine::new()).collect(),
    };
    let mut model = Model {
      indexes: (0..INDEXES).map(|slot| (slot == 0).then(BTreeMap::new)).collect(),
      pushes: vec![BTreeMap::new(); PUSHES],
    };
    for step in &steps {
      apply(&mut real, &mut model, step);
      check(&real, &model);
    }
    for index in real.indexes.iter_mut() {
      if let Some(index) = index.take() {
        index.release(&mut real.store).unwrap();
      }
    }
    for push in &mut real.pushes {
      push.abandon(&mut real.store).unwrap();
    }
    prop_assert!(real.store.is_empty());
    prop_assert_eq!((real.store.used(), real.store.charged()), (0, 0));
  }
}
