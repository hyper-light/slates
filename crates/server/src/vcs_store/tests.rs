//! A-662's tests (§4.17.1 "Durability and residency"): the record's encoding, its replay held to the live store over
//! generated histories recovered at generated points, each tool's arm on the owner shard, and a push's objects riding
//! its entry through A-635's append and made again from the partition's log.

use std::collections::BTreeMap;

use proptest::prelude::*;
use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_git::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use slates_git::objects::tree::{Entry, Mode, compare, write_entries};
use slates_git::store::{Incoming, Positions, Push, PushRefused, StoreRefused};
use slates_git::store::rows::ObjectRow;
use slates_ipc::protocol::{ObjectFormat as WireFormat, Refusal, ReplyBody, RequestBody, VcsTool};
use slates_merge::push as work;
use slates_vcs::{ContentId, ContentRefused, ContentStore};

use super::record::{Record, RecordRefused, RecordWriter, RecordedMove, RecordedObject};
use super::{
  GitPush, GitRefused, GitStore, ReplayRefused, RepositoryStore, Target, admit_object, commit_moves,
  green_key, replay_git, replay_into, seal_moves,
};
use crate::repository::push::{BranchAppended, BranchChange, EntryParts, StoreMove};
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

/// Shape: the window the store-level tests' content store holds content in: small, so a blob of the generated sizes is
/// held as several windows under a list, as a large file is.
const WINDOW: usize = 1 << 6;

/// Shape: the store-level tests' content budget, room to spare.
const BUDGET: u64 = 1 << 26;

/// Shape: the refs the generated histories move.
const GREENS: u64 = 3;

/// Shape: the room a wiring test's work is built within, far past its trees.
const ROOM: u64 = 1 << 20;

/// Format: the format every test repository is in.
const FORMAT: ObjectFormat = ObjectFormat::Sha1;

/// Format: the first committer date the histories use, seconds since the epoch.
const FIRST_DATE: u64 = 1_700_000_000;

// ---------------------------------------------------------------------------------------------------------------------
// Objects as git writes them.

/// A blob of `bytes`: its id and content.
fn blob(bytes: &[u8]) -> (ObjectId, Vec<u8>) {
  (name_of(FORMAT, Kind::Blob, bytes).expect("named"), bytes.to_vec())
}

/// A tree of plain files, its entries in git's order: its id and content.
fn tree(files: &[(Vec<u8>, ObjectId)]) -> (ObjectId, Vec<u8>) {
  let mut entries: Vec<Entry<'_>> = files
    .iter()
    .map(|(name, id)| Entry::new(Mode::File, name, id.as_bytes()))
    .collect();
  entries.sort_by(compare);
  let mut out = Vec::new();
  write_entries(&entries, &mut out);
  (name_of(FORMAT, Kind::Tree, &out).expect("named"), out)
}

/// An id's lowercase hex, as a commit names its tree and parents.
fn hex(id: ObjectId) -> String {
  id.as_bytes()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// A commit of `tree` with `parents`, committed at `date`: its id and content.
fn commit(tree: ObjectId, parents: &[ObjectId], date: u64) -> (ObjectId, Vec<u8>) {
  let mut text = format!("tree {}\n", hex(tree));
  for parent in parents {
    text.push_str(&format!("parent {}\n", hex(*parent)));
  }
  text.push_str(&format!(
    "author A U Thor <author@example.com> {date} +0000\ncommitter C O Mitter <committer@example.com> {date} \
     +0000\n\nA-662\n"
  ));
  (name_of(FORMAT, Kind::Commit, text.as_bytes()).expect("named"), text.into_bytes())
}

/// One object a push brings: its type, id and content.
type Brought = (Kind, ObjectId, Vec<u8>);

/// `brought` as a source hands it in, claimed by its id, at `depth`.
fn incoming(brought: &Brought, depth: u64) -> Incoming<'_> {
  Incoming {
    kind: brought.0,
    claimed: Some(brought.1),
    content: &brought.2,
    depth,
  }
}

/// A commit of a tree of `files` over `parents` at `date`: its blobs, its tree and itself, in that order, and its id.
fn commit_of(files: &[(Vec<u8>, Vec<u8>)], parents: &[ObjectId], date: u64) -> (Vec<Brought>, ObjectId) {
  let mut brought = Vec::new();
  let mut named = Vec::new();
  for (name, bytes) in files {
    let (id, content) = blob(bytes);
    if !brought.iter().any(|(_, held, _)| *held == id) {
      brought.push((Kind::Blob, id, content));
    }
    named.push((name.clone(), id));
  }
  named.sort();
  named.dedup_by(|a, b| a.0 == b.0);
  let (tree_id, tree_bytes) = tree(&named);
  brought.push((Kind::Tree, tree_id, tree_bytes));
  let (commit_id, commit_bytes) = commit(tree_id, parents, date);
  brought.push((Kind::Commit, commit_id, commit_bytes));
  (brought, commit_id)
}

// ---------------------------------------------------------------------------------------------------------------------
// The record.

/// A-662 (the record's format, pinned). Do: write a record of one delete and one move, and one object, then read it
/// back. Expect: the bytes the module's encoding states, byte for byte, and the same moves and object read back.
#[test]
fn t_a662_a_record_reads_back_what_it_wrote_and_its_bytes_are_pinned() {
  let head = ObjectId::from_bytes(FORMAT, &[0xab; 20]).expect("an id");
  let identity = ContentId([0x11; 32]);
  let mut writer = RecordWriter::default();
  writer.object(Kind::Blob, identity, b"hi");
  let moves: Vec<RecordedMove> = vec![(7, 4, None), (9, 5, Some(head))];
  let bytes = writer.finish(3, &moves);
  let mut expected = Vec::new();
  expected.extend_from_slice(b"SGPR");
  expected.extend_from_slice(&1u32.to_le_bytes());
  expected.extend_from_slice(&3u64.to_le_bytes());
  expected.extend_from_slice(&2u64.to_le_bytes());
  expected.extend_from_slice(&7u64.to_le_bytes());
  expected.extend_from_slice(&4u64.to_le_bytes());
  expected.push(0);
  expected.extend_from_slice(&9u64.to_le_bytes());
  expected.extend_from_slice(&5u64.to_le_bytes());
  expected.push(1);
  expected.extend_from_slice(&[0xab; 20]);
  expected.extend_from_slice(&1u64.to_le_bytes());
  expected.push(3);
  expected.extend_from_slice(&[0x11; 32]);
  expected.extend_from_slice(&2u64.to_le_bytes());
  expected.extend_from_slice(b"hi");
  assert_eq!(bytes, expected);
  let read = Record::decode(&bytes, FORMAT).expect("read back");
  let object = RecordedObject {
    kind: Kind::Blob,
    identity,
    content: b"hi",
  };
  assert_eq!(
    read,
    Record {
      deepest: 3,
      moves,
      objects: vec![object],
    }
  );
}

/// A-662 (hostile input on a parser of bytes the log holds). Do: read a record cut at every length, one whose counts are
/// `u64::MAX`, one of another magic and another version, one with a marker of neither kind, one read in another format,
/// one naming a delta's type number, and one with a byte after its last object. Expect: each refused by its kind, never
/// a panic, and no count allocated past what the bytes could hold.
#[test]
fn t_a662_hostile_records_are_refused_by_kind() {
  let head = ObjectId::from_bytes(FORMAT, &[0xcd; 20]).expect("an id");
  let mut writer = RecordWriter::default();
  writer.object(Kind::Commit, ContentId([2; 32]), b"commit");
  let bytes = writer.finish(0, &[(1, 1, Some(head))]);
  for cut in 0..bytes.len() {
    let read = Record::decode(bytes.get(..cut).expect("a prefix"), FORMAT);
    assert!(read.is_err(), "a record cut at {cut} was read");
  }
  assert_hostile_counts_and_words(&bytes);
  assert_hostile_marks(&bytes);
}

/// The counts and the leading words of `bytes` made hostile, each refused by its kind.
fn assert_hostile_counts_and_words(bytes: &[u8]) {
  let with = |at: std::ops::Range<usize>, put: &[u8]| {
    let mut changed = bytes.to_vec();
    changed.get_mut(at).expect("in the record").copy_from_slice(put);
    changed
  };
  let moves_huge = with(16..24, &u64::MAX.to_le_bytes());
  assert_eq!(Record::decode(&moves_huge, FORMAT), Err(RecordRefused::Count));
  let magic = with(0..4, b"GCHN");
  assert_eq!(Record::decode(&magic, FORMAT), Err(RecordRefused::Magic));
  let version = with(4..8, &2u32.to_le_bytes());
  assert_eq!(
    Record::decode(&version, FORMAT),
    Err(RecordRefused::Version { version: 2 })
  );
  // Format: the objects' count follows the header (24 bytes), the move's key, version and marker (17) and its head (20).
  let objects_at = 24 + 17 + 20;
  let objects_huge = with(objects_at..objects_at + 8, &u64::MAX.to_le_bytes());
  assert_eq!(Record::decode(&objects_huge, FORMAT), Err(RecordRefused::Count));
}

/// The markers, the format, the type and the end of `bytes` made hostile, each refused by its kind.
fn assert_hostile_marks(bytes: &[u8]) {
  let mut marker = bytes.to_vec();
  // Format: the move's marker follows the header (24 bytes), its key and its version (16).
  if let Some(byte) = marker.get_mut(24 + 16) {
    *byte = 2;
  }
  assert_eq!(Record::decode(&marker, FORMAT), Err(RecordRefused::Marker));
  assert!(Record::decode(bytes, ObjectFormat::Sha256).is_err());
  let mut delta = bytes.to_vec();
  // Format: the object's type follows the move (24 + 17 + 20) and the objects' count (8).
  if let Some(byte) = delta.get_mut(24 + 17 + 20 + 8) {
    *byte = 6;
  }
  assert_eq!(Record::decode(&delta, FORMAT), Err(RecordRefused::Malformed));
  let mut trailing = bytes.to_vec();
  trailing.push(0);
  assert_eq!(Record::decode(&trailing, FORMAT), Err(RecordRefused::Trailing));
}

// ---------------------------------------------------------------------------------------------------------------------
// The store, live and replayed.

/// A store as a shard holds it, with the records its pushes made, in order, and what the test knows of it.
struct Live {
  content: ContentStore,
  store: GitStore,
  records: Vec<Vec<u8>>,
  /// Each ref's head, by its key.
  heads: BTreeMap<u64, ObjectId>,
  /// Each ref's green's version, by its key: what its moves name.
  greens: BTreeMap<u64, u64>,
  /// Every blob a push brought, to send one again.
  blobs: Vec<Brought>,
}

impl Live {
  fn new() -> Live {
    Live {
      content: ContentStore::new(BUDGET, WINDOW).expect("a content store"),
      store: GitStore::new(FORMAT),
      records: Vec::new(),
      heads: BTreeMap::new(),
      greens: BTreeMap::new(),
      blobs: Vec::new(),
    }
  }

  /// The live path a push takes (`super::GitPush`, `super::commit`): its objects admitted in order, the push sealed,
  /// its moves admitted, its record made, and its commit. The record.
  fn push(&mut self, objects: &[Brought], moves: &[RecordedMove], depth: u64) -> Result<(), GitRefused> {
    let mut push = Push::new(FORMAT);
    let mut record = RecordWriter::default();
    let pushed = self.push_held(&mut push, &mut record, (objects, moves), depth);
    let deepest = push.deepest();
    let given = push.abandon(&mut self.content);
    pushed?;
    given.map_err(|refused| GitRefused::Object(PushRefused::Content(refused)))?;
    self.records.push(record.finish(deepest, moves));
    for (key, version, head) in moves {
      self.greens.insert(*key, *version);
      match head {
        Some(id) => self.heads.insert(*key, *id),
        None => self.heads.remove(key),
      };
    }
    for brought in objects.iter().filter(|(kind, ..)| *kind == Kind::Blob) {
      self.blobs.push(brought.clone());
    }
    Ok(())
  }

  fn push_held(
    &mut self,
    push: &mut Push,
    record: &mut RecordWriter,
    (objects, moves): (&[Brought], &[RecordedMove]),
    depth: u64,
  ) -> Result<(), GitRefused> {
    for brought in objects {
      admit_object(&mut self.content, &self.store, (&mut *push, &mut *record), incoming(brought, depth))?;
    }
    let admitted = seal_moves(&self.content, &self.store, push, moves)?;
    commit_moves(&mut self.content, &mut self.store, push, (&admitted, moves)).map(drop)
  }

  /// The store made again from the records alone, as a recovery makes it.
  fn recovered(&self) -> Result<Live, ReplayRefused> {
    let mut content = ContentStore::new(BUDGET, WINDOW).expect("a content store");
    let mut store = GitStore::new(FORMAT);
    for record in &self.records {
      replay_git(&mut content, &mut store, record)?;
    }
    Ok(Live {
      content,
      store,
      records: self.records.clone(),
      heads: self.heads.clone(),
      greens: self.greens.clone(),
      blobs: self.blobs.clone(),
    })
  }
}

/// What a test compares of a version: D(k), its head, its root tree and its derived value.
type VersionFacts = (Option<Positions>, Option<ObjectId>, Option<ObjectId>, Option<ContentId>);

/// What a test compares of a ref: each version's facts, its reached set, and its green's version at each.
type RefFacts = (Vec<VersionFacts>, Option<Positions>, Vec<u64>);

/// Everything a store holds that a reader can ask (the module docs' "the store after a recovery equals the store
/// before"): every position's id and row, every ref's versions and reached set, every derived value, and every content
/// the content store holds with its holders.
#[derive(Debug, PartialEq, Eq)]
struct Facts {
  rows: Vec<(ObjectId, Option<ObjectRow>)>,
  refs: Vec<RefFacts>,
  contents: Vec<(ContentId, u64)>,
  used: u64,
}

/// [`Facts`] of `store` over `content`, for the refs keyed `greens`.
fn facts(content: &ContentStore, store: &GitStore, greens: &[u64]) -> Facts {
  let mut rows = Vec::new();
  let mut position = 0u64;
  while let Some(id) = store.repository.id_at(content, position) {
    rows.push((id, store.repository.object(content, &id)));
    position += 1;
  }
  let refs = greens
    .iter()
    .map(|green| {
      let versions = (0..store.repository.versions(*green))
        .map(|at| {
          let version = u64::try_from(at).expect("a version");
          let value = store
            .derivations
            .value(content, &store.reach, &store.repository, (*green, version))
            .ok()
            .flatten();
          (
            store.repository.brought(*green, at).cloned(),
            store.repository.head(content, *green, at),
            store.repository.tree(content, *green, at),
            value,
          )
        })
        .collect();
      (
        versions,
        store.repository.reached(*green).cloned(),
        store.versions(*green).to_vec(),
      )
    })
    .collect();
  let mut contents: Vec<(ContentId, u64)> = content
    .identities()
    .map(|identity| (*identity, content.holders(identity)))
    .collect();
  contents.sort();
  Facts {
    rows,
    refs,
    contents,
    used: content.used(),
  }
}

impl Live {
  fn facts(&self) -> Facts {
    let greens: Vec<u64> = (0..GREENS).collect();
    facts(&self.content, &self.store, &greens)
  }
}

/// One generated push (the oracle's docs): which ref it moves, what it does (`act`), the ref it reads, the files of its
/// new tree, whether it sends a blob the repository holds again, and one byte more ([`Extra`]).
type Step = (u8, u8, u8, Vec<(u8, Vec<u8>)>, bool, u8);

/// A step's last byte read as three choices: its objects reversed (bit 0), the delta depth its pack carried (bits 1 and
/// 2), and the store recovered after it (the bits above, one value in three).
struct Extra {
  reversed: bool,
  depth: u64,
  recover: bool,
}

impl Extra {
  fn of(byte: u8) -> Extra {
    Extra {
      reversed: byte & 1 == 1,
      depth: u64::from((byte >> 1) & 3),
      recover: (byte >> 3) % 3 == 1,
    }
  }
}

/// What a generated history did, for the census.
#[derive(Debug, Default)]
struct Census {
  fast_forwards: u64,
  roots: u64,
  branched: u64,
  moved_to_another: u64,
  deletes: u64,
  atomic: u64,
  resent: u64,
  recoveries: u64,
}

/// The push one step makes against `live` at `date`: its objects and its moves; counted in `census`.
fn planned(live: &Live, step: &Step, date: u64, census: &mut Census) -> (Vec<Brought>, Vec<RecordedMove>) {
  let (target, act, other, files, resend, ..) = step;
  let target = u64::from(*target) % GREENS;
  let other = (target + 1 + u64::from(*other) % (GREENS - 1)) % GREENS;
  let files: Vec<(Vec<u8>, Vec<u8>)> = files
    .iter()
    .map(|(name, bytes)| (vec![b'a' + name % 6], bytes.clone()))
    .collect();
  let mine = live.heads.get(&target).copied();
  let theirs = live.heads.get(&other).copied();
  let next = |key: u64| live.greens.get(&key).copied().unwrap_or(0) + 1;
  let kept = |key: u64| live.greens.get(&key).copied().unwrap_or(0);
  let (mut objects, moves) = match (act % 6, mine, theirs) {
    (3, _, Some(theirs)) => {
      census.moved_to_another += 1;
      (Vec::new(), vec![(target, next(target), Some(theirs))])
    }
    (4, Some(_), _) => {
      census.deletes += 1;
      (Vec::new(), vec![(target, kept(target), None)])
    }
    (2, _, Some(theirs)) => {
      census.branched += 1;
      let (objects, id) = commit_of(&files, &[theirs], date);
      (objects, vec![(target, next(target), Some(id))])
    }
    (1, Some(mine), _) => {
      census.atomic += 1;
      let (objects, id) = commit_of(&files, &[mine], date);
      (
        objects,
        vec![
          (target, next(target), Some(id)),
          (other, next(other), Some(id)),
        ],
      )
    }
    (_, Some(mine), _) => {
      census.fast_forwards += 1;
      let (objects, id) = commit_of(&files, &[mine], date);
      (objects, vec![(target, next(target), Some(id))])
    }
    _ => {
      census.roots += 1;
      let (objects, id) = commit_of(&files, &[], date);
      (objects, vec![(target, next(target), Some(id))])
    }
  };
  if *resend && let Some(held) = live.blobs.first() {
    census.resent += 1;
    objects.insert(0, held.clone());
  }
  (objects, moves)
}

/// Runs `steps` against a reference store that is never recovered and a live one recovered after the steps that ask,
/// comparing them whole after every recovery and at the end, and their records after every push.
fn run_history(steps: &[Step]) -> Result<Census, TestCaseError> {
  let mut census = Census::default();
  let mut reference = Live::new();
  let mut live = Live::new();
  for (index, step) in steps.iter().enumerate() {
    let date = FIRST_DATE + u64::try_from(index).unwrap_or(0);
    let (objects, moves) = planned(&reference, step, date, &mut census);
    let extra = Extra::of(step.5);
    let order: Vec<Brought> = if extra.reversed {
      objects.iter().rev().cloned().collect()
    } else {
      objects
    };
    prop_assert_eq!(reference.push(&order, &moves, extra.depth), Ok(()));
    prop_assert_eq!(live.push(&order, &moves, extra.depth), Ok(()));
    prop_assert_eq!(&live.records, &reference.records, "the recovered store records alike");
    if extra.recover {
      census.recoveries += 1;
      live = live
        .recovered()
        .map_err(|why| TestCaseError::fail(format!("not recovered: {why:?}")))?;
      prop_assert_eq!(live.facts(), reference.facts());
    }
  }
  let recovered = live
    .recovered()
    .map_err(|why| TestCaseError::fail(format!("not recovered: {why:?}")))?;
  prop_assert_eq!(recovered.facts(), reference.facts());
  prop_assert_eq!(live.facts(), reference.facts());
  Ok(census)
}

/// A generated step.
fn step() -> impl Strategy<Value = Step> {
  (
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    proptest::collection::vec((any::<u8>(), proptest::collection::vec(any::<u8>(), 0..200)), 1..4),
    any::<bool>(),
    any::<u8>(),
  )
}

proptest! {
  // Shape: generated histories a run: 64 (4 under Miri), each of up to 24 pushes.
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(if cfg!(miri) { 4 } else { 64 }),
    ..ProptestConfig::default()
  }))]

  /// A-662 (§4.17.1 "Durability and residency": the store after a recovery equals the store before; guard then apply,
  /// as `crates/db/tests/model.rs` drops and recovers the database at random points). Do: run generated histories of
  /// pushes against three refs (commits on a head, roots, commits on another ref's head, a ref moved to another's head,
  /// deletes, one commit moving two refs at once, blobs the repository holds sent again, objects in pack order and
  /// reversed, packs of several depths), against a store never recovered and one made again from its records alone after
  /// the steps that ask. Expect: every push accepted on both; both make the same records; after every recovery and at
  /// the end the two stores are equal whole: every position's id and row, every ref's D(k), heads, trees, derived values
  /// and reached set, and every content held with its holders.
  #[test]
  fn t_a662_the_store_after_a_recovery_equals_the_store_before(
    steps in proptest::collection::vec(step(), 1..24),
  ) {
    run_history(&steps)?;
  }
}

/// A-662 (the oracle's non-vacuity census, over one fixed history). Do: run a history that makes every kind of push
/// and recovers three times. Expect: the stores equal as the oracle checks, and every kind of push counted, so a dead
/// branch of the generator can never pass the oracle unseen.
#[test]
fn t_a662_a_fixed_history_makes_every_kind_of_push_and_recovers() {
  let files = |byte: u8| vec![(byte, vec![byte; 150]), (byte.wrapping_add(1), vec![byte; 7])];
  // Each last byte (`Extra`): 9 reversed and recovered, 2 at depth 1, 8 recovered, 5 reversed at depth 2, 14 recovered
  // at depth 3, 1 reversed.
  let steps: Vec<Step> = vec![
    (0, 0, 0, files(1), false, 0),
    (0, 0, 0, files(2), true, 9),
    (1, 2, 1, files(3), false, 2),
    (2, 3, 0, files(4), true, 8),
    (0, 1, 0, files(5), false, 5),
    (1, 4, 0, files(6), true, 14),
    (1, 0, 0, files(7), false, 0),
    (2, 2, 1, files(8), true, 1),
  ];
  let census = run_history(&steps).expect("the history holds");
  assert!(census.fast_forwards > 0, "{census:?}");
  assert!(census.roots > 0, "{census:?}");
  assert!(census.branched > 0, "{census:?}");
  assert!(census.moved_to_another > 0, "{census:?}");
  assert!(census.deletes > 0, "{census:?}");
  assert!(census.atomic > 0, "{census:?}");
  assert!(census.resent > 0, "{census:?}");
  assert_eq!(census.recoveries, 3, "{census:?}");
  assert!(census.roots > 0 && census.fast_forwards > 0, "{census:?}");
}

/// A-662 (guard then apply: a replay refuses what is not what its record names, and leaves nothing). Do: replay a
/// record whose object's bytes were changed, one whose first object the store holds already (a record the live
/// admission would not have made), and one whose head names a commit nobody sent. Expect: refused `Identity`,
/// `Diverged` and the store's `Missing` in turn, and each refused replay leaves the store as it was.
#[test]
fn t_a662_a_record_not_what_it_names_is_refused_and_leaves_nothing() {
  let mut live = Live::new();
  let (objects, head) = commit_of(&[(b"a".to_vec(), b"bytes".to_vec())], &[], FIRST_DATE);
  live.push(&objects, &[(0, 1, Some(head))], 0).expect("pushed");
  let before = live.facts();
  let record = live.records.first().expect("a record").clone();
  let mut changed = record.clone();
  if let Some(byte) = changed.last_mut() {
    *byte ^= 1;
  }
  let mut store = live.recovered().expect("recovered");
  assert_eq!(
    replay_git(&mut store.content, &mut store.store, &changed),
    Err(ReplayRefused::Identity)
  );
  assert_eq!(store.facts(), before);
  assert!(matches!(
    replay_git(&mut store.content, &mut store.store, &record),
    Err(ReplayRefused::Diverged)
  ));
  assert_eq!(store.facts(), before);
  let nobody = ObjectId::from_bytes(FORMAT, &[0x5a; 20]).expect("an id");
  let missing = RecordWriter::default().finish(0, &[(1, 1, Some(nobody))]);
  assert!(matches!(
    replay_git(&mut store.content, &mut store.store, &missing),
    Err(ReplayRefused::Git(GitRefused::Store(_)))
  ));
  assert_eq!(store.facts(), before);
}

// ---------------------------------------------------------------------------------------------------------------------
// The owner shard.

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates repository `name` of `tool` on the shard: its id and its root green.
fn repository(state: &mut ShardState, name: &str, tool: VcsTool) -> (RepositoryId, DbVolumeId) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool,
      format: WireFormat::Sha1,
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

/// A-662 (one map, each tool's arm, made with the record and freed with it). Do: create a git, a Mercurial and a
/// Subversion repository; replay a record into the Mercurial and Subversion stores; take the Mercurial one's refusal as
/// a recovery takes it; free all three. Expect: each store made on the owner shard as its tool's arm, git's in its
/// record's format; the two replays refused `Owed`, their replay being their own push pieces'; the refused store given
/// back and named unrecovered with why; after the frees no store is held, none unrecovered, and nothing is charged.
#[test]
fn t_a662_each_tool_has_its_arm_made_with_its_record_and_freed_with_it() {
  crate::daemon::audit_on_shard(|state| {
    let (git, _) = repository(state, "git", VcsTool::Git);
    let (hg, _) = repository(state, "hg", VcsTool::Mercurial);
    let (svn, _) = repository(state, "svn", VcsTool::Subversion);
    let stores = &state.vcs_stores;
    assert!(matches!(
      stores.store(git),
      Some(RepositoryStore::Git(store)) if store.repository.format() == FORMAT
    ));
    assert!(matches!(stores.store(hg), Some(RepositoryStore::Mercurial(_))));
    assert!(matches!(stores.store(svn), Some(RepositoryStore::Subversion(_))));
    assert!(state.vcs_stores.mercurial(hg).is_some());
    assert!(state.vcs_stores.subversion(svn).is_some());
    for repository in [hg, svn] {
      assert_eq!(
        replay_into(&mut state.vcs_stores, repository, b"a record"),
        Err(ReplayRefused::Owed)
      );
    }
    assert!(!super::replayed(state, hg, Err(ReplayRefused::Owed)));
    assert!(!state.vcs_stores.holds(hg));
    assert_eq!(state.vcs_stores.unrecovered(hg), Some(&ReplayRefused::Owed));
    for repository in [git, hg, svn] {
      super::free(state, repository);
      assert!(!state.vcs_stores.holds(repository));
      assert_eq!(state.vcs_stores.unrecovered(repository), None);
    }
    assert_eq!(state.vcs_stores.charged(), 0);
  });
}

/// A-662 (the wire's words for a store's refusal). Do: map a store's budget refusal and a moved green. Expect:
/// `BudgetExceeded` with the room the budget had, then `ContentUnavailable`, counted.
#[test]
fn t_a662_a_store_refusal_reads_as_the_budget_or_unavailable() {
  crate::daemon::audit_on_shard(|state| {
    let budget = GitRefused::Store(StoreRefused::Content(ContentRefused::OverBudget {
      need: 10,
      room: 3,
    }));
    assert_eq!(
      super::refusal_of(state, &budget),
      Refusal::BudgetExceeded { available: 3 }
    );
    let moved = GitRefused::Store(StoreRefused::Moved { green: 1 });
    assert_eq!(super::refusal_of(state, &moved), Refusal::ContentUnavailable);
    assert_eq!(state.refusals.get(super::COMMIT_REFUSED).copied(), Some(1));
  });
}

/// A push begun on the shard with `objects` admitted.
fn begun(state: &mut ShardState, repository: RepositoryId, objects: &[Brought]) -> GitPush {
  let mut push = GitPush::begin(&state.vcs_stores, repository).expect("a git store");
  let arriving: Vec<Incoming<'_>> = objects.iter().map(|brought| incoming(brought, 0)).collect();
  push.admit_all(state, &arriving).expect("admitted");
  push
}

/// A-618's work for `push` on `green`, from `base` (a version and its tree) to `head`'s tree, read through A-638's view.
fn work_of(
  state: &ShardState,
  (push, green): (&GitPush, DbVolumeId),
  (version, base): (u64, Option<ObjectId>),
  (head, tree_id): (ObjectId, ObjectId),
) -> slates_merge::engine::Increment {
  let key = |id: ObjectId| work::ObjectKey::new(id.as_bytes()).expect("a key");
  push
    .read(&state.vcs_stores, |view| {
      let engine = state.greens.get(&green).expect("the green's engine");
      let asked = work::Push {
        name: *b"A-662 a push through its entry..",
        base_version: version,
        base_tree: base.map(key),
        tree: key(tree_id),
        head: Some(head.as_bytes()),
      };
      work::build(&asked, &engine, view, ROOM).map(|built| built.increment)
    })
    .expect("a view")
    .expect("the work")
}

/// `changes` appended with `sealed`'s objects and `moves` through A-635's append.
fn appended(
  state: &mut ShardState,
  repository: RepositoryId,
  changes: Vec<BranchChange>,
  sealed: (&mut super::SealedPush, &[StoreMove]),
) -> Result<crate::repository::push::Appended, crate::repository::push::AppendRefused> {
  crate::repository::push::push_sealed(state, (&owner(), repository), changes, EntryParts::default(), sealed)
}

/// One push through the shard: `objects` admitted, the work from `base` to the commit's tree, the push sealed with its
/// move and appended. The version it made.
fn push_through(
  state: &mut ShardState,
  (repository, green): (RepositoryId, DbVolumeId),
  objects: &[Brought],
  (base, head, tree_id): (Option<ObjectId>, ObjectId, ObjectId),
) -> u64 {
  let push = begun(state, repository, objects);
  let version = state.greens.get(&green).expect("the green's engine").head();
  let built = work_of(state, (&push, green), (version, base), (head, tree_id));
  let mut sealed = push.seal(state).expect("sealed");
  let changes = vec![BranchChange::Update { green, work: built }];
  let moves = [(Target::Green(green), Some(head))];
  let made = appended(state, repository, changes, (&mut sealed, &moves)).expect("appended");
  // Committed, the push holds nothing: giving it back gives back nothing.
  sealed.abandon(state);
  match made.changes.first() {
    Some(BranchAppended::Moved { version, .. }) => *version,
    other => panic!("not moved: {other:?}"),
  }
}

/// A-662 (a push without `atomic`: each ref its own append, the push kept whole by its caller). Do: push a first commit;
/// then admit a second push's objects, seal it and append it with a work made against the stale version 0, which the
/// verdict refuses; append it again with the work made against version 1. Expect: the stale append
/// refused with nothing applied, the store unmoved and the push still holding its objects; the second append accepted
/// as version 2, its record holding every object the push brought, and the store's ref at the new head.
#[test]
fn t_a662_a_refused_ref_leaves_the_push_whole_for_the_next() {
  crate::daemon::audit_on_shard(|state| {
    let (repository, root) = repository(state, "tree", VcsTool::Git);
    let files = [(b"README".to_vec(), b"one\n".to_vec())];
    let (first, one) = commit_of(&files, &[], FIRST_DATE);
    let first_tree = tree_of(&first);
    push_through(state, (repository, root), &first, (None, one, first_tree));
    let files = [(b"README".to_vec(), b"two\n".to_vec())];
    let (second, two) = commit_of(&files, &[one], FIRST_DATE + 1);
    let second_tree = tree_of(&second);
    let push = begun(state, repository, &second);
    let stale = work_of(state, (&push, root), (0, None), (two, second_tree));
    let fresh = work_of(state, (&push, root), (1, Some(first_tree)), (two, second_tree));
    let mut sealed = push.seal(state).expect("sealed");
    let moves = [(Target::Green(root), Some(two))];
    let held = state.vcs_stores.content().map(ContentStore::used);
    let changes = vec![BranchChange::Update { green: root, work: stale }];
    assert!(appended(state, repository, changes, (&mut sealed, &moves)).is_err());
    assert_eq!(state.vcs_stores.content().map(ContentStore::used), held);
    assert_eq!(state.db.partition().repositories().objects(repository).count(), 1);
    let changes = vec![BranchChange::Update { green: root, work: fresh }];
    let made = appended(state, repository, changes, (&mut sealed, &moves)).expect("appended");
    assert!(matches!(made.changes.first(), Some(BranchAppended::Moved { version: 2, .. })));
    sealed.abandon(state);
    let records: Vec<Vec<u8>> = state
      .db
      .partition()
      .repositories()
      .objects(repository)
      .map(<[u8]>::to_vec)
      .collect();
    let last = records.last().expect("a record");
    let read = Record::decode(last, FORMAT).expect("a record");
    assert_eq!(read.objects.len(), second.len());
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    let key = green_key(root);
    let newest = store.repository.versions(key).checked_sub(1).expect("a version");
    assert_eq!(store.repository.head(content, key, newest), Some(two));
  });
}

/// The root tree a commit's content names.
fn tree_of(objects: &[Brought]) -> ObjectId {
  objects
    .iter()
    .find(|(kind, ..)| *kind == Kind::Tree)
    .map(|(_, id, _)| *id)
    .expect("a tree")
}

/// A-662 (§4.17.1 "Durability and residency" over the owner shard; A-635's append). Do: create a git repository; admit
/// a push's objects and abandon it; push a first commit and a fast-forward through the append, each sealed into the
/// store; read the partition's register; make the store again from the log (`rebuild`, the recovery's own replay); free
/// the repository's store. Expect: the abandoned push leaves nothing held or charged; each push moves the root's green
/// and the store's ref; the register holds one record a push, in order, the second holding only the objects the first
/// did not bring; the store's charge is what its content store holds; the store made again equals the live one whole
/// and replayed both records; freeing gives every byte back.
#[test]
fn t_a662_a_pushs_objects_ride_its_entry_and_the_log_makes_the_same_store() {
  crate::daemon::audit_on_shard(|state| {
    let (repository, root) = repository(state, "kernel", VcsTool::Git);
    let files = [(b"README".to_vec(), b"one\n".to_vec())];
    let (first, one) = commit_of(&files, &[], FIRST_DATE);
    let mut abandoned = GitPush::begin(&state.vcs_stores, repository).expect("a git store");
    let arriving: Vec<Incoming<'_>> = first.iter().map(|brought| incoming(brought, 0)).collect();
    abandoned.admit_all(state, &arriving).expect("admitted");
    assert!(state.vcs_stores.charged() > 0);
    abandoned.abandon(state);
    assert_eq!(state.vcs_stores.content().map(ContentStore::used), Some(0));
    assert_eq!(state.vcs_stores.charged(), 0);
    let first_tree = tree_of(&first);
    assert_eq!(
      push_through(state, (repository, root), &first, (None, one, first_tree)),
      1
    );
    let files = [
      (b"README".to_vec(), b"one\n".to_vec()),
      (b"src".to_vec(), b"two\n".to_vec()),
    ];
    let (second, two) = commit_of(&files, &[one], FIRST_DATE + 1);
    let second_tree = tree_of(&second);
    assert_eq!(
      push_through(state, (repository, root), &second, (Some(first_tree), two, second_tree)),
      2
    );
    assert_recorded_and_charged(state, repository, root, (first.len(), second.len()));
    let greens = [green_key(root)];
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    assert_eq!(store.version_of(content, greens[0], &one), Some((1, first_tree)));
    assert_eq!(store.version_of(content, greens[0], &two), Some((2, second_tree)));
    assert_eq!(store.versions(greens[0]), &[1, 2]);
    let live = {
      let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
      facts(content, store, &greens)
    };
    let replayed = state.refusals.get(super::REPLAYED).copied().unwrap_or(0);
    super::rebuild(state, repository);
    let made = {
      let (content, store) = state.vcs_stores.git_ref(repository).expect("the store made again");
      facts(content, store, &greens)
    };
    assert_eq!(made, live);
    assert_eq!(state.refusals.get(super::REPLAYED).copied(), Some(replayed + 2));
    super::free(state, repository);
    assert_eq!(state.vcs_stores.charged(), 0);
    assert_eq!(state.vcs_stores.content().map(ContentStore::used), Some(0));
  });
}

/// The register holds one record a push, the second holding only what the first did not bring, and the store's
/// charge is what its content store holds.
fn assert_recorded_and_charged(
  state: &ShardState,
  repository: RepositoryId,
  root: DbVolumeId,
  (first, second): (usize, usize),
) {
  let records: Vec<Vec<u8>> = state
    .db
    .partition()
    .repositories()
    .objects(repository)
    .map(<[u8]>::to_vec)
    .collect();
  let read: Vec<Record<'_>> = records
    .iter()
    .map(|bytes| Record::decode(bytes, FORMAT).expect("a record"))
    .collect();
  let counts: Vec<usize> = read.iter().map(|record| record.objects.len()).collect();
  // The second push's README blob is the first's, already held, so only its new blob, tree and commit are recorded.
  assert_eq!(counts, vec![first, second - 1]);
  assert!(read.iter().all(|record| record.moves.len() == 1
    && record.moves.first().map(|movement| movement.0) == Some(green_key(root))));
  let used = state.vcs_stores.content().map_or(0, ContentStore::used);
  assert!(used > 0);
  assert_eq!(state.vcs_stores.charged(), used);
}

/// A-662 (a new branch's move keyed by the green its append makes). Do: push a first commit to the default branch; then
/// create `topic` at a commit on it through the append, made at the default branch's version 1, its store move naming
/// the branch by name. Expect: the branch made as a new green at its version 1; its store keyed by that green, at
/// version 1 with the new head; the record naming that key.
#[test]
fn t_a662_a_created_branch_is_keyed_by_the_green_its_append_makes() {
  crate::daemon::audit_on_shard(|state| {
    let (repository, root) = repository(state, "made", VcsTool::Git);
    let files = [(b"README".to_vec(), b"one\n".to_vec())];
    let (first, one) = commit_of(&files, &[], FIRST_DATE);
    let first_tree = tree_of(&first);
    push_through(state, (repository, root), &first, (None, one, first_tree));
    let files = [(b"README".to_vec(), b"topic\n".to_vec())];
    let (topic, head) = commit_of(&files, &[one], FIRST_DATE + 1);
    let push = begun(state, repository, &topic);
    let work = work_of(state, (&push, root), (1, Some(first_tree)), (head, tree_of(&topic)));
    let mut sealed = push.seal(state).expect("sealed");
    let changes = vec![BranchChange::Create {
      name: "topic".to_owned(),
      from: root,
      version: 1,
      work: Some(work),
    }];
    let moves = [(Target::Created("topic".to_owned()), Some(head))];
    let made = appended(state, repository, changes, (&mut sealed, &moves)).expect("appended");
    sealed.abandon(state);
    let Some(BranchAppended::Created { green, version: 1 }) = made.changes.first().cloned() else {
      panic!("not created: {made:?}");
    };
    let key = green_key(green);
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    assert_eq!(store.versions(key), &[1]);
    assert_eq!(store.repository.head(content, key, 0), Some(head));
    let last = state
      .db
      .partition()
      .repositories()
      .objects(repository)
      .last()
      .map(<[u8]>::to_vec)
      .expect("a record");
    let read = Record::decode(&last, FORMAT).expect("a record");
    assert_eq!(read.moves, vec![(key, 1, Some(head))]);
  });
}
