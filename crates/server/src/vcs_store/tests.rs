//! A-662's tests (§4.17.1 "Durability and residency"): the record's encoding, its replay held to the live store over
//! generated histories recovered at generated points, each tool's arm on the owner shard, and a push's objects riding
//! its entry through A-635's append and made again from the partition's log.

use std::collections::BTreeMap;

use proptest::prelude::*;
use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_git::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use slates_git::objects::tree::{Entry, Mode, compare, write_entries};
use slates_git::store::rows::ObjectRow;
use slates_git::store::{Incoming, Positions, Push, PushRefused, StoreRefused};
use slates_ipc::protocol::{ObjectFormat as WireFormat, Refusal, ReplyBody, RequestBody, VcsTool};
use slates_merge::push as work;
use slates_vcs::{ContentId, ContentRefused, ContentStore};

use super::record::{Noted, Record, RecordRefused, RecordWriter, RecordedBase, RecordedMove};
use super::{
  GitPush, GitRefused, GitStore, ReplayRefused, RepositoryStore, Target, admit_object,
  commit_moves, green_key, replay_git, replay_into, seal_moves,
};
use crate::repository::push::{Appended, BranchAppended, BranchChange, EntryParts, StoreMove};
use crate::state::ShardState;
use crate::verbs::to_db_volume;

/// Shape: the window the store-level tests' content store holds content in, as A-614's view tests hold theirs: past the
/// longest row the store writes here with its table's frame (`slates_vcs::index` refuses one past it), and short, so a
/// generated blob is held as several windows under a list, as a large file is.
const WINDOW: usize = 1 << 8;

/// Shape: the longest generated blob: three windows, so blobs are held as one window and as several.
const BLOB_BYTES: usize = WINDOW * 3;

/// Shape: the store-level tests' content budget, room to spare.
const BUDGET: u64 = 1 << 26;

/// Shape: the refs the generated histories move.
const GREENS: u64 = 3;

/// Shape: the room a wiring test's work is built within, far past its trees.
const ROOM: u64 = 1 << 20;

/// Shape: the deepest delta chain a wiring test's pack carried: past zero, so its records carry a chain to replay.
const DEEP: u64 = 2;

/// Format: the format every test repository is in.
const FORMAT: ObjectFormat = ObjectFormat::Sha1;

/// Format: the first committer date the histories use, seconds since the epoch.
pub(super) const FIRST_DATE: u64 = 1_700_000_000;

// ---------------------------------------------------------------------------------------------------------------------
// Objects as git writes them.

/// A blob of `bytes`: its id and content.
fn blob(bytes: &[u8]) -> (ObjectId, Vec<u8>) {
  (
    name_of(FORMAT, Kind::Blob, bytes).expect("named"),
    bytes.to_vec(),
  )
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
pub(crate) fn hex(id: ObjectId) -> String {
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
  (
    name_of(FORMAT, Kind::Commit, text.as_bytes()).expect("named"),
    text.into_bytes(),
  )
}

/// An annotated tag named `name` of the commit `target`, as a push brings it.
fn tag_of(target: ObjectId, name: &str) -> Brought {
  let text = format!(
    "object {}\ntype commit\ntag {name}\ntagger T A Gger <tagger@example.com> 0 +0000\n\nA-850\n",
    hex(target)
  );
  (
    Kind::Tag,
    name_of(FORMAT, Kind::Tag, text.as_bytes()).expect("named"),
    text.into_bytes(),
  )
}

/// One object a push brings: its type, id and content.
pub(crate) type Brought = (Kind, ObjectId, Vec<u8>);

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
pub(crate) fn commit_of(
  files: &[(Vec<u8>, Vec<u8>)],
  parents: &[ObjectId],
  date: u64,
) -> (Vec<Brought>, ObjectId) {
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

/// A content store holding `bytes` as an object: the store, and the object as a writer notes it.
fn holding(kind: Kind, bytes: &[u8]) -> (ContentStore, Noted) {
  let mut content = ContentStore::new(BUDGET, WINDOW).expect("a content store");
  let identity = content.put_object(bytes).expect("held");
  let id = name_of(FORMAT, kind, bytes).expect("named");
  let len = u64::try_from(bytes.len()).expect("a length");
  (
    content,
    Noted {
      kind,
      id,
      identity,
      len,
    },
  )
}

/// A-850's record v6 (it replaces A-767 C5's version 5, which replaced A-722's version 4 and A-724 piece 8c's version
/// 3). Do: write the record of one move made at a base and one object as version 6, its names put as one content first,
/// its pack named by an identity and a length, and the changeset the store landed them as; then read it back, and read
/// it with its version word made 3 and 5. Expect: the bytes the module's encoding states: the head at version 6, the
/// move with its base, the names' identity (the BLAKE3 of the names' rows) and their count, the pack's identity and its
/// length, the changeset's sequence, and nothing after; read back with the same move and base, the names, the pack and
/// the sequence named, none inline and no pack bytes; versions 3 and 5 refused typed, no shim reading either.
#[test]
fn t_a850_a_version_6_record_names_its_moves_bases_its_names_its_pack_and_its_changeset() {
  let head = ObjectId::from_bytes(FORMAT, &[0xab; 20]).expect("an id");
  let (mut content, noted) = holding(Kind::Blob, b"hi");
  let mut writer = RecordWriter::default();
  writer.object(noted);
  let moves: Vec<RecordedMove> = vec![(9, 5, Some(head))];
  let bases: Vec<RecordedBase> = vec![Some((4, 2))];
  let names = writer.names_of(|_| true);
  let (identity, count) =
    super::record_names::names_to_content(&mut content, &names).expect("names put");
  assert_eq!(count, 1);
  let mut rows = vec![Kind::Blob.pack_number()];
  rows.extend_from_slice(&noted.identity.0);
  rows.extend_from_slice(&2u64.to_le_bytes());
  assert_eq!(
    identity,
    slates_vcs::ContentId::of(&rows),
    "the names' identity is the BLAKE3 of their rows"
  );
  let pack = slates_vcs::ContentId::of(b"PACK-bytes");
  let bytes = RecordWriter::finish_packed(
    (3, &moves, &bases),
    Some((identity, count)),
    Some((pack, 10)),
    7,
  )
  .expect("written");
  let mut expected = Vec::new();
  expected.extend_from_slice(b"SGPR");
  expected.extend_from_slice(&6u32.to_le_bytes());
  expected.extend_from_slice(&3u64.to_le_bytes());
  expected.extend_from_slice(&1u64.to_le_bytes());
  expected.extend_from_slice(&9u64.to_le_bytes());
  expected.extend_from_slice(&5u64.to_le_bytes());
  expected.push(1);
  expected.extend_from_slice(&[0xab; 20]);
  expected.push(1);
  expected.extend_from_slice(&4u64.to_le_bytes());
  expected.extend_from_slice(&2u64.to_le_bytes());
  expected.extend_from_slice(&identity.0);
  expected.extend_from_slice(&1u64.to_le_bytes());
  expected.extend_from_slice(&pack.0);
  expected.extend_from_slice(&10u64.to_le_bytes());
  expected.extend_from_slice(&7u64.to_le_bytes());
  assert_eq!(bytes, expected);
  let read = Record {
    named: Some((identity, 1)),
    packed: Some((pack, 10)),
    sequence: 7,
    deepest: 3,
    moves,
    bases,
  };
  assert_eq!(Record::decode(&bytes, FORMAT), Ok(read));
  for older in [3u32, 5] {
    let mut old = bytes.clone();
    old.splice(4..8, older.to_le_bytes());
    assert_eq!(
      Record::decode(&old, FORMAT),
      Err(super::record::RecordRefused::Version { version: older }),
      "version {older} is no longer read (A-850: a version-5 record names no move's base)"
    );
  }
}

/// A-662 (hostile input on a parser of bytes the log holds), over a version-6 record. Do: read a record cut at every
/// length, one whose moves' count is `u64::MAX`, one of another magic, one of an unknown version and one of each retired
/// version (1 and 2, which carried a push's objects), one with a head marker and one with a base marker (A-850) of
/// neither kind, one read in another format, and one with a byte after its sequence. Expect: each refused by its kind,
/// never a panic, and no count allocated past what the bytes could hold.
#[test]
fn t_a662_hostile_records_are_refused_by_kind() {
  let head = ObjectId::from_bytes(FORMAT, &[0xcd; 20]).expect("an id");
  let names = (ContentId([3; 32]), 1);
  let pack = (ContentId([4; 32]), 9);
  let bytes = RecordWriter::finish_packed(
    (0, &[(1, 1, Some(head))], &[Some((2, 0))]),
    Some(names),
    Some(pack),
    2,
  )
  .expect("written");
  for cut in 0..bytes.len() {
    let read = Record::decode(bytes.get(..cut).expect("a prefix"), FORMAT);
    assert!(read.is_err(), "a record cut at {cut} was read");
  }
  assert_hostile_counts_and_words(&bytes);
  assert_hostile_marks(&bytes);
}

/// Main's ruling of 2026-11-21 on part 2 (w-svn's A-819 rule: a retired format is refused typed, never skipped or a
/// panic; A-824). Do: build a log of two records, a version-5 one a live push wrote and, after it, a version-2 one as a
/// segment written before C5 part 2b piece 3 holds it (its moves, its objects' names and an inline pack); replay the
/// version-5 record alone into an empty store, then the log in order into another. Expect: the version-5-only log
/// replayed whole, the same store as the live one; the log meeting the version-2 record refused `Retired { version: 2 }`
/// (and a version-1 record `Retired { version: 1 }`), the store as the version-5 record left it.
#[test]
fn t_a824_a_log_meeting_a_retired_record_version_is_refused_typed_and_a_version_5_log_replays_whole()
 {
  let mut live = Live::new();
  let (objects, head) = commit_of(&[(b"a".to_vec(), b"bytes".to_vec())], &[], FIRST_DATE);
  live
    .push(&objects, vec![(0, Some(head))], 0)
    .expect("pushed");
  let before = live.facts();
  assert_eq!(
    live.recovered().map(|store| store.facts()),
    Ok(before.clone()),
    "a version-5 log replays whole"
  );
  let mut old = Vec::new();
  old.extend_from_slice(b"SGPR");
  old.extend_from_slice(&2u32.to_le_bytes());
  old.extend_from_slice(&0u64.to_le_bytes());
  old.extend_from_slice(&1u64.to_le_bytes());
  old.extend_from_slice(&1u64.to_le_bytes());
  old.extend_from_slice(&2u64.to_le_bytes());
  old.push(1);
  old.extend_from_slice(head.as_bytes());
  old.extend_from_slice(&0u64.to_le_bytes());
  old.extend_from_slice(&4u64.to_le_bytes());
  old.extend_from_slice(b"PACK");
  let mut store = live.recovered().expect("recovered");
  assert_eq!(
    replay_git(&mut store.content, &mut store.store, &old),
    Err(ReplayRefused::Record(RecordRefused::Retired { version: 2 })),
    "a version-2 record refused typed, naming its version"
  );
  assert_eq!(
    store.facts(),
    before,
    "the store as the version-5 record left it"
  );
  let mut first = old.clone();
  first.splice(4..8, 1u32.to_le_bytes());
  assert_eq!(
    Record::decode(&first, FORMAT),
    Err(RecordRefused::Retired { version: 1 })
  );
}

/// Lets go of the region's holds this shard keeps that `kept` did not hold (the names and packs a test's pushes homed
/// here, a one-node daemon being each one's region home), as the home's own collection lets go of content no record
/// names: what a test comparing the repository's own bytes leaves out (C5 part 2b).
fn let_go_region_holds_since(state: &mut ShardState, kept: &[ContentId]) {
  for identity in super::homes::region_held_here() {
    if !kept.contains(&identity) {
      let _ = super::homes::let_go_for_region(state, &identity, None);
    }
  }
  super::settle(state);
}

/// The count of names `record` names (version 5 keeps them as a content, with their count).
fn named_count(record: &Record) -> usize {
  record
    .named
    .map_or(0, |(_, count)| usize::try_from(count).unwrap_or(usize::MAX))
}

/// The counts and the leading words of `bytes` made hostile, each refused by its kind.
fn assert_hostile_counts_and_words(bytes: &[u8]) {
  let with = |at: std::ops::Range<usize>, put: &[u8]| {
    let mut changed = bytes.to_vec();
    changed
      .get_mut(at)
      .expect("in the record")
      .copy_from_slice(put);
    changed
  };
  let moves_huge = with(16..24, &u64::MAX.to_le_bytes());
  assert_eq!(
    Record::decode(&moves_huge, FORMAT),
    Err(RecordRefused::Count)
  );
  let magic = with(0..4, b"GCHN");
  assert_eq!(Record::decode(&magic, FORMAT), Err(RecordRefused::Magic));
  let version = with(4..8, &7u32.to_le_bytes());
  assert_eq!(
    Record::decode(&version, FORMAT),
    Err(RecordRefused::Version { version: 7 })
  );
  for retired in [1u32, 2] {
    let old = with(4..8, &retired.to_le_bytes());
    assert_eq!(
      Record::decode(&old, FORMAT),
      Err(RecordRefused::Retired { version: retired }),
      "version {retired} is no longer read (C5 part 2b piece 3: no record carries a push's objects)"
    );
  }
}

/// The markers, the format, the type and the end of `bytes` made hostile, each refused by its kind.
fn assert_hostile_marks(bytes: &[u8]) {
  // Format: the move's head marker follows the header (24 bytes), its key and its version (16); its base marker follows
  // the head's id (20).
  for at in [24 + 16, 24 + 16 + 1 + 20] {
    let mut marker = bytes.to_vec();
    if let Some(byte) = marker.get_mut(at) {
      *byte = 2;
    }
    assert_eq!(Record::decode(&marker, FORMAT), Err(RecordRefused::Marker));
  }
  assert!(Record::decode(bytes, ObjectFormat::Sha256).is_err());
  let mut trailing = bytes.to_vec();
  trailing.push(0);
  assert_eq!(
    Record::decode(&trailing, FORMAT),
    Err(RecordRefused::Trailing)
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// The store, live and replayed.

/// One ref's planned move: its key and its new head, none to delete it.
type Planned = (u64, Option<ObjectId>);

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

/// The record a commit of `moves` writes now as a client's push's append writes it once its edge homed and landed it
/// (version 6, C5 part 2b and A-850, each move with its base): the names of the objects `writer` noted that the store lacks now, by the predicate the
/// commit gives positions by, put as one content, and the push's pack held by its identity (a pack of whole objects
/// where the push kept none), as their homing leaves them in the content store; no changeset (the model holds no node
/// store). An append that brings no object names none.
fn landed_record(
  content: &mut ContentStore,
  store: &GitStore,
  (writer, deepest, pack): (&RecordWriter, u64, &[u8]),
  (moves, bases): (&[RecordedMove], &[RecordedBase]),
) -> Result<Vec<u8>, GitRefused> {
  let names = writer.names_of(|id| store.repository.object(content, id).is_none());
  if names.is_empty() {
    return RecordWriter::finish_packed((deepest, moves, bases), None, None, 0)
      .map_err(super::unwritten);
  }
  let named =
    super::record_names::names_to_content(content, &names).map_err(|_| GitRefused::Allocation)?;
  let whole;
  let pack = if pack.is_empty() {
    let mut objects = Vec::new();
    for (kind, identity, len) in &names {
      let bytes = content
        .object_bytes(identity, *len)
        .ok_or(GitRefused::Allocation)?;
      objects.push((*kind, bytes));
    }
    whole = slates_git::pack::of_whole_placed(
      store.repository.format(),
      objects
        .iter()
        .map(|(kind, bytes)| (*kind, bytes.as_slice())),
    )
    .map_err(|_| GitRefused::Allocation)?
    .0;
    whole.as_slice()
  } else {
    pack
  };
  let identity = content
    .put_object(pack)
    .map_err(|refused| GitRefused::Object(PushRefused::Content(refused)))?;
  let len = u64::try_from(pack.len()).unwrap_or(u64::MAX);
  RecordWriter::finish_packed(
    (deepest, moves, bases),
    Some(named),
    Some((identity, len)),
    0,
  )
  .map_err(super::unwritten)
}

/// A push admitted and not yet committed, as a sealed push waits for its appends: its quarantine and its record's
/// writer, the moves it plans, whether it commits them a ref at a time from the one push (a push without `atomic`, each
/// ref its own append), and every object it brought.
struct Pending {
  push: Push,
  writer: RecordWriter,
  moves: Vec<Planned>,
  split: bool,
  brought: Vec<Brought>,
}

/// An empty content store of the tests' budget and window.
fn empty_content() -> ContentStore {
  ContentStore::new(BUDGET, WINDOW).expect("a content store")
}

impl Live {
  fn new() -> Live {
    Live {
      content: empty_content(),
      store: GitStore::new(FORMAT, WINDOW),
      records: Vec::new(),
      heads: BTreeMap::new(),
      greens: BTreeMap::new(),
      blobs: Vec::new(),
    }
  }

  /// Admits `objects` at `depth` as the live path does (`super::GitPush::admit_all`), for `moves` to commit later.
  fn begin(
    &mut self,
    objects: &[Brought],
    depth: u64,
    (moves, split): (Vec<Planned>, bool),
  ) -> Result<Pending, GitRefused> {
    let mut push = Push::new(FORMAT);
    let mut writer = RecordWriter::default();
    let arriving: Vec<Incoming<'_>> = objects
      .iter()
      .map(|brought| incoming(brought, depth))
      .collect();
    let (pack, offsets) = slates_git::pack::of_whole_placed(
      FORMAT,
      arriving
        .iter()
        .map(|incoming| (incoming.kind, incoming.content)),
    )
    .map_err(|_| GitRefused::Allocation)?;
    push.extend_pack(&pack);
    for (arriving, offset) in arriving.into_iter().zip(offsets) {
      admit_object(
        &mut self.content,
        &self.store,
        (&mut push, &mut writer),
        (arriving, Some(offset)),
      )?;
    }
    Ok(Pending {
      push,
      writer,
      moves,
      split,
      brought: objects.to_vec(),
    })
  }

  /// Commits `pending` as the live path appends it (`super::SealedPush`, `super::commit`): its moves at once, or a ref
  /// at a time from the one push, each commit's record written as it commits and each move's version read off the refs
  /// then.
  fn commit(&mut self, mut pending: Pending) -> Result<(), GitRefused> {
    let parts: Vec<Vec<Planned>> = if pending.split {
      pending
        .moves
        .iter()
        .map(|movement| vec![*movement])
        .collect()
    } else {
      vec![pending.moves.clone()]
    };
    for part in &parts {
      let moves = self.versioned(part);
      self.commit_part(&mut pending, &moves)?;
      self.moved(&moves);
    }
    pending
      .push
      .abandon(&mut self.content)
      .map_err(|refused| GitRefused::Object(PushRefused::Content(refused)))?;
    let blobs = pending
      .brought
      .into_iter()
      .filter(|(kind, ..)| *kind == Kind::Blob);
    self.blobs.extend(blobs);
    Ok(())
  }

  /// One append's commit of `moves`: admitted against the store as it is now, its record written, committed.
  fn commit_part(
    &mut self,
    pending: &mut Pending,
    moves: &[RecordedMove],
  ) -> Result<(), GitRefused> {
    let admitted = seal_moves(&mut self.content, &mut self.store, &mut pending.push, moves)?;
    let deepest = pending.push.deepest();
    let bases: Vec<RecordedBase> = admitted.iter().map(slates_git::store::Move::base).collect();
    let record = landed_record(
      &mut self.content,
      &self.store,
      (&pending.writer, deepest, pending.push.pack()),
      (moves, &bases),
    )?;
    commit_moves(
      &mut self.content,
      &mut self.store,
      &mut pending.push,
      (&admitted, moves),
    )?;
    pending.writer = RecordWriter::default();
    self.records.push(record);
    Ok(())
  }

  /// `moves` with each ref's green's version once moved: the next for a new head, its own for a delete.
  fn versioned(&self, moves: &[Planned]) -> Vec<RecordedMove> {
    moves
      .iter()
      .map(|(key, head)| {
        let now = self.greens.get(key).copied().unwrap_or(0);
        (*key, if head.is_some() { now + 1 } else { now }, *head)
      })
      .collect()
  }

  /// The refs and their greens' versions once `moves` committed.
  fn moved(&mut self, moves: &[RecordedMove]) {
    for (key, version, head) in moves {
      self.greens.insert(*key, *version);
      match head {
        Some(id) => self.heads.insert(*key, *id),
        None => self.heads.remove(key),
      };
    }
  }

  /// A push admitted and committed at once.
  fn push(
    &mut self,
    objects: &[Brought],
    moves: Vec<Planned>,
    depth: u64,
  ) -> Result<(), GitRefused> {
    let pending = self.begin(objects, depth, (moves, false))?;
    self.commit(pending)
  }

  /// The store made again from the records alone, as a recovery makes it.
  fn recovered(&self) -> Result<Live, ReplayRefused> {
    let mut content = empty_content();
    let mut store = GitStore::new(FORMAT, WINDOW);
    for record in &self.records {
      self.fetched_into(&mut content, record);
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

impl Live {
  /// The names and the pack `record` names, put into `content` as a recovery's fetch from their region homes puts them
  /// (C5: a record holds neither; its fetch's hold is the record's), from this store's content where it holds them.
  fn fetched_into(&self, content: &mut ContentStore, record: &[u8]) {
    let Ok(read) = Record::decode(record, FORMAT) else {
      return;
    };
    if let Some((identity, count)) = read.named
      && let Ok(names) = super::record_names::names_from_content(&self.content, &identity, count)
    {
      let _ = super::record_names::names_to_content(content, &names);
    }
    if let Some((identity, len)) = read.packed
      && let Some(pack) = self.content.object_bytes(&identity, len)
    {
      let _ = content.put_object(&pack);
    }
  }
}

/// What a test compares of a version: D(k), its head, its root tree and its derived value.
type VersionFacts = (
  Option<Positions>,
  Option<ObjectId>,
  Option<ObjectId>,
  Option<ContentId>,
);

/// What a test compares of a ref: each version's facts, its reached set, and its green's version at each.
type RefFacts = (Vec<VersionFacts>, Option<Positions>, Vec<u64>);

/// Everything a store holds that a reader can ask (the module docs' "the store after a recovery equals the store
/// before"): every position's id and row, every ref's versions and reached set, every derived value, every content the
/// content store holds with its holders, and the deepest chain the store learned from its pushes' packs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Facts {
  rows: Vec<(ObjectId, Option<ObjectRow>)>,
  refs: Vec<RefFacts>,
  contents: Vec<(ContentId, u64)>,
  used: u64,
  depth: u64,
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
    .map(|green| ref_facts(content, store, *green))
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
    depth: store.repository.learned_depth(),
  }
}

/// [`RefFacts`] of the ref keyed `green`.
fn ref_facts(content: &ContentStore, store: &GitStore, green: u64) -> RefFacts {
  let versions = (0..store.repository.versions(green))
    .map(|at| {
      let version = u64::try_from(at).expect("a version");
      let value = store
        .derivations
        .value(content, &store.reach, &store.repository, (green, version))
        .ok()
        .flatten();
      (
        store.repository.brought(content, green, at).ok(),
        store.repository.head(content, green, at),
        store.repository.tree(content, green, at),
        value,
      )
    })
    .collect();
  (
    versions,
    store
      .repository
      .versions(green)
      .checked_sub(1)
      .and_then(|newest| store.repository.reached(content, green, newest).ok()),
    store.versions(green).to_vec(),
  )
}

impl Live {
  fn facts(&self) -> Facts {
    let greens: Vec<u64> = (0..GREENS).collect();
    facts(&self.content, &self.store, &greens)
  }
}

/// One generated push (the oracle's docs): which ref it moves, what it does (`act`), the ref it reads, the files of its
/// new tree, whether it sends a blob again, one byte more ([`Extra`]) and its order ([`Order`]).
type Step = (u8, u8, u8, Vec<(u8, Vec<u8>)>, bool, u8, u8);

/// A step's sixth byte read as three choices: its objects reversed (bit 0), the delta depth its pack carried (bits 1 and
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

/// A step's last byte read as its order: a push moving two refs committed a ref at a time from its one push (bit 0, a
/// push without `atomic`); its commit held back until the next step's push has committed (bit 1, a hook's wait); and,
/// when it sends again what a push held back holds, every such object rather than one blob (bit 2: two clients pushing
/// one commit at once, the held one's record left with no object and its own chain).
struct Order {
  split: bool,
  defer: bool,
  whole: bool,
}

impl Order {
  fn of(byte: u8) -> Order {
    Order {
      split: byte & 1 == 1,
      defer: byte & 2 == 2,
      whole: byte & 4 == 4,
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
  split: u64,
  deferred: u64,
  shared: u64,
  shared_whole: u64,
  recoveries: u64,
}

/// The push one step makes against `live` at `date`: its objects and its moves; counted in `census`. A step that sends
/// objects again sends what the push `held` back holds and the repository lacks when there is any (one blob, or all of
/// it when the step's order says whole), so this push commits it first; else a blob the repository holds.
fn planned(
  live: &Live,
  (step, held): (&Step, Option<&Pending>),
  date: u64,
  census: &mut Census,
) -> (Vec<Brought>, Vec<Planned>) {
  let (target, act, other, files, resend, ..) = step;
  let target = u64::from(*target) % GREENS;
  let other = (target + 1 + u64::from(*other) % (GREENS - 1)) % GREENS;
  let files: Vec<(Vec<u8>, Vec<u8>)> = files
    .iter()
    .map(|(name, bytes)| (vec![b'a' + name % 6], bytes.clone()))
    .collect();
  let mine = live.heads.get(&target).copied();
  let theirs = live.heads.get(&other).copied();
  let (mut objects, moves) = match (act % 6, mine, theirs) {
    (3, _, Some(theirs)) => {
      census.moved_to_another += 1;
      (Vec::new(), vec![(target, Some(theirs))])
    }
    (4, Some(_), _) => {
      census.deletes += 1;
      (Vec::new(), vec![(target, None)])
    }
    (2, _, Some(theirs)) => {
      census.branched += 1;
      let (objects, id) = commit_of(&files, &[theirs], date);
      (objects, vec![(target, Some(id))])
    }
    (1, Some(mine), _) => {
      census.atomic += 1;
      let (objects, id) = commit_of(&files, &[mine], date);
      (objects, vec![(target, Some(id)), (other, Some(id))])
    }
    (_, Some(mine), _) => {
      census.fast_forwards += 1;
      let (objects, id) = commit_of(&files, &[mine], date);
      (objects, vec![(target, Some(id))])
    }
    _ => {
      census.roots += 1;
      let (objects, id) = commit_of(&files, &[], date);
      (objects, vec![(target, Some(id))])
    }
  };
  if *resend {
    let mut sent = sent_again(live, (held, Order::of(step.6).whole), census);
    sent.append(&mut objects);
    objects = sent;
  }
  (objects, moves)
}

/// What a step sends again: what the push `held` back holds and the repository lacks, every such object when `whole`
/// (counted shared whole) and else its first blob (counted shared); else the first blob the repository holds (counted
/// resent); nothing when none of these is.
fn sent_again(
  live: &Live,
  (held, whole): (Option<&Pending>, bool),
  census: &mut Census,
) -> Vec<Brought> {
  let lacks = |(kind, id, _): &&Brought| {
    (whole || *kind == Kind::Blob) && live.store.repository.object(&live.content, id).is_none()
  };
  let lacked: Vec<Brought> = held
    .map(|pending| pending.brought.iter().filter(lacks).cloned().collect())
    .unwrap_or_default();
  if whole && !lacked.is_empty() {
    census.shared_whole += 1;
    return lacked;
  }
  if let Some(blob) = lacked.into_iter().next() {
    census.shared += 1;
    return vec![blob];
  }
  let Some(blob) = live.blobs.first() else {
    return Vec::new();
  };
  census.resent += 1;
  vec![blob.clone()]
}

/// A failure of the oracle's own step, named.
fn fail(why: impl std::fmt::Debug) -> TestCaseError {
  TestCaseError::fail(format!("{why:?}"))
}

/// A generated history's two stores, taking the same pushes in the same order: a reference never recovered and a live
/// one recovered after the steps that ask; the push a step held back, in both; and the census.
struct History {
  reference: Live,
  live: Live,
  held: Option<(Pending, Pending)>,
  census: Census,
}

impl History {
  /// Runs `steps`, comparing both stores' records after every step, and both whole after every recovery and at the end.
  fn run(steps: &[Step]) -> Result<Census, TestCaseError> {
    let mut history = History {
      reference: Live::new(),
      live: Live::new(),
      held: None,
      census: Census::default(),
    };
    for (index, step) in steps.iter().enumerate() {
      history.step(step, FIRST_DATE + u64::try_from(index).unwrap_or(0))?;
    }
    history.end()
  }

  /// One step: its push planned against the reference and admitted to both stores, then committed now, or held back
  /// until the next step's push has committed; then the live store recovered when the step asks.
  fn step(&mut self, step: &Step, date: u64) -> Result<(), TestCaseError> {
    let (extra, order) = (Extra::of(step.5), Order::of(step.6));
    let begun = self.begin(step, date, &extra, &order)?;
    if order.defer && self.held.is_none() {
      self.census.deferred += 1;
      self.held = Some(begun);
      return Ok(());
    }
    self.commit(begun)?;
    if let Some(held) = self.held.take() {
      self.commit(held)?;
    }
    prop_assert_eq!(
      &self.live.records,
      &self.reference.records,
      "both stores record alike"
    );
    if extra.recover {
      self.recover()?;
    }
    Ok(())
  }

  /// A step's push admitted to both stores.
  fn begin(
    &mut self,
    step: &Step,
    date: u64,
    extra: &Extra,
    order: &Order,
  ) -> Result<(Pending, Pending), TestCaseError> {
    let held = self.held.as_ref().map(|(held, _)| held);
    let (objects, moves) = planned(&self.reference, (step, held), date, &mut self.census);
    let objects: Vec<Brought> = if extra.reversed {
      objects.into_iter().rev().collect()
    } else {
      objects
    };
    let split = order.split && moves.len() > 1;
    self.census.split += u64::from(split);
    let theirs = self
      .reference
      .begin(&objects, extra.depth, (moves.clone(), split))
      .map_err(fail)?;
    let ours = self
      .live
      .begin(&objects, extra.depth, (moves, split))
      .map_err(fail)?;
    Ok((theirs, ours))
  }

  /// A push committed to both stores.
  fn commit(&mut self, (theirs, ours): (Pending, Pending)) -> Result<(), TestCaseError> {
    self.reference.commit(theirs).map_err(fail)?;
    self.live.commit(ours).map_err(fail)
  }

  /// The live store made again from its records alone, equal whole to the reference.
  fn recover(&mut self) -> Result<(), TestCaseError> {
    self.census.recoveries += 1;
    self.live = self.live.recovered().map_err(fail)?;
    prop_assert_eq!(self.live.facts(), self.reference.facts());
    Ok(())
  }

  /// The push still held back committed; both stores' records alike, and the live store, and one made again from its
  /// records, equal whole to the reference.
  fn end(mut self) -> Result<Census, TestCaseError> {
    if let Some(held) = self.held.take() {
      self.commit(held)?;
    }
    prop_assert_eq!(
      &self.live.records,
      &self.reference.records,
      "both stores record alike"
    );
    let recovered = self.live.recovered().map_err(fail)?;
    prop_assert_eq!(recovered.facts(), self.reference.facts());
    prop_assert_eq!(self.live.facts(), self.reference.facts());
    Ok(self.census)
  }
}

/// A generated step.
fn step() -> impl Strategy<Value = Step> {
  (
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    proptest::collection::vec(
      (
        any::<u8>(),
        proptest::collection::vec(any::<u8>(), 0..BLOB_BYTES),
      ),
      1..4,
    ),
    any::<bool>(),
    any::<u8>(),
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
  /// deletes, one commit moving two refs at once or a ref at a time from one push, blobs the repository holds sent
  /// again, pushes held back while the next commits, sharing a blob with it or everything it holds, objects in pack
  /// order and reversed, packs of several depths), against a store never recovered and one made again from its records
  /// alone after the steps that ask. Expect: every push accepted on both; both make the same records; after every
  /// recovery and at the end the two stores are equal whole: every position's id and row, every ref's D(k), heads,
  /// trees, derived values, reached set and its greens' versions, every content held with its holders, and the learned
  /// depth.
  #[test]
  fn t_a662_the_store_after_a_recovery_equals_the_store_before(
    steps in proptest::collection::vec(step(), 1..24),
  ) {
    History::run(&steps)?;
  }
}

/// A-662 (the oracle's non-vacuity census, over one fixed history). Do: run a history that makes every kind of push,
/// commits a push moving two refs a ref at a time from a pack two deep, holds a push back while the next commits a blob
/// it holds, holds one from a pack three deep (deeper than any before) back while the next, from a pack whole, sends
/// every object it holds, and recovers three times. Expect: the stores equal as the oracle checks, the learned depth
/// too, and every kind counted, so a dead branch of the generator can never pass the oracle unseen.
#[test]
fn t_a662_a_fixed_history_makes_every_kind_of_push_and_recovers() {
  // Shape: each commit's files: a blob over three windows and one within one.
  let files = |byte: u8| {
    vec![
      (byte, vec![byte; WINDOW * 2 + WINDOW / 2]),
      (byte.wrapping_add(1), vec![byte; 7]),
    ]
  };
  // Each sixth byte (`Extra`): 9 reversed and recovered, 2 at depth 1, 8 recovered, 5 reversed at depth 2, 14
  // recovered at depth 3, 1 reversed, 6 at depth 3. Each last byte (`Order`): 1 a ref at a time, 2 held back, 4 what
  // the push held back holds sent whole.
  let steps: Vec<Step> = vec![
    (0, 0, 0, files(1), false, 0, 0),
    (0, 0, 0, files(2), true, 9, 0),
    (1, 2, 1, files(3), false, 2, 0),
    (2, 3, 0, files(4), true, 8, 0),
    (0, 1, 0, files(5), false, 5, 1),
    (1, 4, 0, files(6), true, 14, 0),
    (1, 0, 0, files(7), false, 0, 2),
    (2, 2, 1, files(8), true, 1, 0),
    (0, 0, 0, files(9), false, 6, 2),
    (1, 0, 0, files(10), true, 0, 4),
  ];
  let census = History::run(&steps).expect("the history holds");
  let kinds = [
    ("fast-forwards", census.fast_forwards),
    ("roots", census.roots),
    ("branched", census.branched),
    ("moved to another", census.moved_to_another),
    ("deletes", census.deletes),
    ("atomic", census.atomic),
    ("resent", census.resent),
    ("a ref at a time", census.split),
    ("held back", census.deferred),
    ("shared", census.shared),
    ("shared whole", census.shared_whole),
  ];
  let never: Vec<&str> = kinds
    .iter()
    .filter(|(_, count)| *count == 0)
    .map(|(kind, _)| *kind)
    .collect();
  assert!(never.is_empty(), "never made: {never:?} in {census:?}");
  assert_eq!(census.recoveries, 3, "{census:?}");
}

/// A-662 (guard then apply: a replay refuses what is not what its record names, and leaves nothing). Do: replay a
/// record whose names' identity was changed, one whose objects the store holds already (a record the live admission
/// would not have made), and one whose head names a commit nobody sent. Expect: refused `Names` (no content of that
/// identity), `Diverged` and the store's `Missing` in turn, and each refused replay leaves the store as it was.
#[test]
fn t_a662_a_record_not_what_it_names_is_refused_and_leaves_nothing() {
  let mut live = Live::new();
  let (objects, head) = commit_of(&[(b"a".to_vec(), b"bytes".to_vec())], &[], FIRST_DATE);
  live
    .push(&objects, vec![(0, Some(head))], 0)
    .expect("pushed");
  let before = live.facts();
  let record = live.records.first().expect("a record").clone();
  let mut changed = record.clone();
  // Format: the names' identity follows the head (24 bytes) and the one move (18 and its head's 20; it has no base).
  if let Some(byte) = changed.get_mut(24 + 18 + 20) {
    *byte ^= 1;
  }
  let mut store = live.recovered().expect("recovered");
  assert!(matches!(
    replay_git(&mut store.content, &mut store.store, &changed),
    Err(ReplayRefused::Names(_))
  ));
  assert_eq!(store.facts(), before);
  assert!(matches!(
    replay_git(&mut store.content, &mut store.store, &record),
    Err(ReplayRefused::Diverged)
  ));
  assert_eq!(store.facts(), before);
  assert_a_head_nobody_sent_is_refused_and_leaves_nothing(&mut store, &before);
}

/// Main 01a20182's ruling of 2026-11-24 01:36 (a git green's version commits with its made commit or not at all, so no
/// commit is owed). Do: replay, over a store made again from a push's log, an owed commit's record of green key 0's
/// version 2 as a server before the ruling staged one. Expect: refused `OwedCommit` naming the key and the version,
/// and the store left as the push's record left it.
#[test]
fn t_a664_an_owed_commits_record_is_refused_typed_naming_its_green_and_version() {
  let mut live = Live::new();
  let (objects, head) = commit_of(&[(b"a".to_vec(), b"bytes".to_vec())], &[], FIRST_DATE);
  live
    .push(&objects, vec![(0, Some(head))], 0)
    .expect("pushed");
  let before = live.facts();
  let owed = super::owed::Owed {
    key: 0,
    version: 2,
    seconds: FIRST_DATE,
    message: "owed".to_owned(),
    name: "A U Thor".to_owned(),
    email: "author@example.com".to_owned(),
  };
  let mut store = live.recovered().expect("recovered");
  assert_eq!(
    replay_git(&mut store.content, &mut store.store, &owed.encode()),
    Err(ReplayRefused::OwedCommit { key: 0, version: 2 })
  );
  assert_eq!(store.facts(), before);
}

/// What the tag ref keyed 1 brought at its first version, its sets derived first: the count of objects.
fn tag_brought(live: &mut Live) -> Result<u64, StoreRefused> {
  live.store.repository.derive(&mut live.content, 1, 0)?;
  Ok(live.store.repository.brought(&live.content, 1, 0)?.len())
}

/// A-850 (main's ruling of 2026-11-21 21:35: a created ref's first version records its base, and a recovery replays it
/// from the record). Do: push a commit as the ref keyed 0, then an annotated tag of it as the ref keyed 1; read the tag
/// push's record; make the store again from the records; then make it again with the tag's record rewritten to name no
/// base, to name a version its base green never had, and to name the tag's own green. Expect: the record names the tag
/// made at ref 0's version 0; live and recovered, the tag's first version brings its tag object alone and the stores are
/// alike; with no base recorded the replay takes none, though the heads it meets would give one, and the tag brings its
/// commit's whole history (the record decides, not the replay); each impossible base is refused typed, `Base`.
#[test]
fn t_a850_a_recovery_replays_each_moves_base_from_its_record() {
  let mut live = Live::new();
  let (objects, head) = commit_of(&[(b"a".to_vec(), b"bytes".to_vec())], &[], FIRST_DATE);
  live
    .push(&objects, vec![(0, Some(head))], 0)
    .expect("pushed");
  let tag = tag_of(head, "v0");
  live
    .push(std::slice::from_ref(&tag), vec![(1, Some(tag.1))], 0)
    .expect("tagged");
  let read =
    Record::decode(live.records.get(1).expect("the tag's record"), FORMAT).expect("a record");
  assert_eq!(read.bases, vec![Some((0, 0))], "made at ref 0's version 0");
  let mut recovered = live.recovered().expect("recovered");
  assert_eq!(recovered.facts(), live.facts());
  assert_eq!(tag_brought(&mut live), Ok(1), "live: its tag object alone");
  assert_eq!(
    tag_brought(&mut recovered),
    Ok(1),
    "recovered: its tag object alone"
  );
  let rewritten = |bases: &[RecordedBase]| {
    RecordWriter::finish_packed(
      (read.deepest, &read.moves, bases),
      read.named,
      read.packed,
      read.sequence,
    )
    .expect("written")
  };
  *live.records.get_mut(1).expect("the tag's record") = rewritten(&[None]);
  let mut unbased = live.recovered().expect("recovered with no base");
  let whole = u64::try_from(objects.len() + 1).expect("a count");
  assert_eq!(
    tag_brought(&mut unbased),
    Ok(whole),
    "no base recorded: none taken"
  );
  for (green, version) in [(0, 5), (1, 0)] {
    *live.records.get_mut(1).expect("the tag's record") = rewritten(&[Some((green, version))]);
    assert!(
      matches!(
        live.recovered(),
        Err(ReplayRefused::Git(GitRefused::Store(StoreRefused::Base { green: g, version: v })))
          if (g, v) == (green, version)
      ),
      "a base at ({green}, {version}) is refused"
    );
  }
}

/// A-767 C5's record v5 (A-722's pack homing; A-724 piece 8c's names): a replay reads a version-5 record's names and
/// pack by their identities, locally, else from their region homes, and holds them for the record. Do: push a commit,
/// write its record as version 5, and replay it into an empty store twice: once holding its names and pack (as a fetch
/// from the region leaves them, or a restart's restored windows held for no one), once without them. Expect: the first
/// made again equal to the live store, which holds the names' content and the pack as its homing left them; the second
/// refused `Names` (missing, to be fetched first), the store left as it was.
#[test]
fn t_a767_c5_a_version_5_record_replays_from_its_names_and_pack_by_identity_and_refuses_without_them()
 {
  let mut live = Live::new();
  let (objects, head) = commit_of(&[(b"a".to_vec(), b"bytes".to_vec())], &[], FIRST_DATE);
  live
    .push(&objects, vec![(0, Some(head))], 0)
    .expect("pushed");
  let before = live.facts();
  let v5 = live.records.first().expect("a record").clone();
  let read = Record::decode(&v5, FORMAT).expect("a version-5 record");
  let ((identity, _), (pack, _)) = (
    read.named.expect("names named"),
    read.packed.expect("a pack named"),
  );
  let mut content = empty_content();
  live.fetched_into(&mut content, &v5);
  let mut store = GitStore::new(FORMAT, WINDOW);
  replay_git(&mut content, &mut store, &v5).expect("replayed from its names and pack");
  assert!(
    content.holders(&identity) >= 1,
    "the names are held for the record"
  );
  assert!(
    content.holders(&pack) >= 2,
    "the pack is held for the record and by its rows"
  );
  let made = Live {
    content,
    store,
    records: vec![v5.clone()],
    heads: live.heads.clone(),
    greens: live.greens.clone(),
    blobs: live.blobs.clone(),
  };
  assert_eq!(made.facts(), before, "the same store as the live one");
  let mut bare = empty_content();
  let mut store = GitStore::new(FORMAT, WINDOW);
  assert!(matches!(
    replay_git(&mut bare, &mut store, &v5),
    Err(ReplayRefused::Names(
      super::record_names::NamesRefused::Rows(slates_vcs::rows::RowsRefused::Missing)
    ))
  ));
  assert!(
    store.versions(0).is_empty(),
    "a refused replay made nothing"
  );
}

/// The last case of [`t_a662_a_record_not_what_it_names_is_refused_and_leaves_nothing`]: a record whose head names a
/// commit nobody sent is refused by the store as `Missing`, and leaves `store`'s facts `before`.
fn assert_a_head_nobody_sent_is_refused_and_leaves_nothing(store: &mut Live, before: &Facts) {
  let nobody = ObjectId::from_bytes(FORMAT, &[0x5a; 20]).expect("an id");
  let missing = RecordWriter::finish_packed((0, &[(1, 1, Some(nobody))], &[None]), None, None, 0)
    .expect("written");
  assert!(matches!(
    replay_git(&mut store.content, &mut store.store, &missing),
    Err(ReplayRefused::Git(GitRefused::Store(_)))
  ));
  assert_eq!(&store.facts(), before);
}

// ---------------------------------------------------------------------------------------------------------------------
// The owner shard.

/// The principal every test repository is created by, and so its owner.
pub(crate) fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates repository `name` of `tool` on the shard: its id and its root green.
pub(crate) fn repository(
  state: &mut ShardState,
  name: &str,
  tool: VcsTool,
) -> (RepositoryId, DbVolumeId) {
  let reply = crate::test_store::create(
    state,
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

/// A-662 (one map, each tool's arm, made with the record and freed with it), with A-655 part 2b's Subversion records.
/// Do: create a git, a Mercurial and a Subversion repository; replay bytes that are no record into the Mercurial and
/// Subversion stores; take the Mercurial one's refusal as a recovery takes it; free all three. Expect: each store made
/// on the owner shard as its tool's arm, git's in its record's format, Subversion's records at revision 0 from its
/// creation's record; Mercurial's replay refused by its record's magic (A-687: an import's record replays there),
/// and Subversion's refused by the record's kind; the refused store given back and named unrecovered with why; after the frees no store
/// is held, none unrecovered, and nothing is charged.
#[test]
fn t_a662_each_tool_has_its_arm_made_with_its_record_and_freed_with_it() {
  // Each made through a client's request on a store-backed daemon (A-767 item 7: a creation lands on the store before
  // it is recorded, and a Subversion repository needs the store, C2).
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let [git, hg, svn] = [
    ("git", VcsTool::Git),
    ("hg", VcsTool::Mercurial),
    ("svn", VcsTool::Subversion),
  ]
  .map(|(name, tool)| {
    crate::test_store::create_on(&daemon, 0, &owner(), (name, tool, WireFormat::Sha1, "main")).0
  });
  crate::daemon::observe_first(&daemon, move |state| {
    assert_each_tool_has_its_arm(state, [git, hg, svn]);
    assert_bytes_that_are_no_record_are_refused(state, hg, svn);
    assert!(!super::replayed(state, hg, Err(ReplayRefused::Diverged)));
    assert!(!state.vcs_stores.holds(hg));
    assert_eq!(
      state.vcs_stores.unrecovered(hg),
      Some(&ReplayRefused::Diverged)
    );
    for repository in [git, hg, svn] {
      super::free(state, repository);
      assert!(!state.vcs_stores.holds(repository));
      assert_eq!(state.vcs_stores.unrecovered(repository), None);
    }
    assert_eq!(state.vcs_stores.charged(), 0);
  });
  daemon.stop();
}

/// The stores of [`t_a662_each_tool_has_its_arm_made_with_its_record_and_freed_with_it`]: each its tool's arm, git's
/// in its record's format, Subversion's records at revision 0.
fn assert_each_tool_has_its_arm(state: &mut ShardState, [git, hg, svn]: [RepositoryId; 3]) {
  let stores = &state.vcs_stores;
  assert!(matches!(
    stores.store(git),
    Some(RepositoryStore::Git(store)) if store.repository.format() == FORMAT
  ));
  assert!(matches!(
    stores.store(hg),
    Some(RepositoryStore::Mercurial(_))
  ));
  assert!(matches!(
    stores.store(svn),
    Some(RepositoryStore::Subversion(_))
  ));
  assert!(state.vcs_stores.mercurial(hg).is_some());
  // Made with its record: Subversion's revision 0, an empty root.
  let youngest = state
    .vcs_stores
    .subversion_records(svn)
    .map(|records| records.youngest());
  assert_eq!(youngest, Some(0));
}

/// The replays of [`t_a662_each_tool_has_its_arm_made_with_its_record_and_freed_with_it`]: bytes that are no record,
/// refused by the Mercurial record's magic and by the Subversion record's kind.
fn assert_bytes_that_are_no_record_are_refused(
  state: &mut ShardState,
  hg: RepositoryId,
  svn: RepositoryId,
) {
  let refused = [
    (
      hg,
      ReplayRefused::Mercurial(slates_hg::import::record::Refused::Magic),
    ),
    (
      svn,
      ReplayRefused::SvnRecord(slates_svn::commit::record::RecordRefused::Magic),
    ),
  ];
  for (repository, why) in refused {
    let room = slates_svn::migration::Room {
      entries: 0,
      bytes: 0,
    };
    assert_eq!(
      replay_into(&mut state.vcs_stores, repository, b"a record", Some(room)),
      Err(why)
    );
  }
}

/// A-662 (the wire's words for a store's refusal). Do: map a store's budget refusal, a record whose room the allocator
/// refused, and a moved green. Expect: `BudgetExceeded` with the room the budget had, then with what the shard could
/// admit now, then `ContentUnavailable`, counted.
#[test]
fn t_a662_a_store_refusal_reads_as_the_budget_or_unavailable() {
  crate::test_store::audit_on_stored_shard("vcs", |state| {
    let budget = GitRefused::Store(StoreRefused::Content(ContentRefused::OverBudget {
      need: 10,
      room: 3,
    }));
    assert_eq!(
      super::refusal_of(state, &budget),
      Refusal::BudgetExceeded { available: 3 }
    );
    let available = state.store.admittable();
    assert_eq!(
      super::refusal_of(state, &GitRefused::Allocation),
      Refusal::BudgetExceeded { available }
    );
    let moved = GitRefused::Store(StoreRefused::Moved { green: 1 });
    assert_eq!(
      super::refusal_of(state, &moved),
      Refusal::ContentUnavailable
    );
    assert_eq!(state.refusals.get(super::COMMIT_REFUSED).copied(), Some(1));
  });
}

/// A push begun on the shard with `objects` admitted at `depth`.
fn begun_at(
  state: &mut ShardState,
  repository: RepositoryId,
  objects: &[Brought],
  depth: u64,
) -> GitPush {
  let mut push = GitPush::begin(&state.vcs_stores, repository).expect("a git store");
  let arriving: Vec<Incoming<'_>> = objects
    .iter()
    .map(|brought| incoming(brought, depth))
    .collect();
  push.admit_all(state, &arriving).expect("admitted");
  push
}

/// A push begun on the shard with `objects` admitted whole.
pub(crate) fn begun(
  state: &mut ShardState,
  repository: RepositoryId,
  objects: &[Brought],
) -> GitPush {
  begun_at(state, repository, objects, 0)
}

/// A-618's work for `push` on `green`, from `base` (a version and its tree) to `head`'s tree, read through A-638's view
/// with the room the shard lends the read.
pub(crate) fn work_of(
  state: &mut ShardState,
  (push, green): (&GitPush, DbVolumeId),
  (version, base): (u64, Option<ObjectId>),
  (head, tree_id): (ObjectId, ObjectId),
) -> slates_merge::engine::Increment {
  let key = |id: ObjectId| work::ObjectKey::new(id.as_bytes()).expect("a key");
  let ShardState {
    store,
    vcs_stores,
    greens,
    ..
  } = state;
  push
    .read((store, vcs_stores), |view| {
      let engine = greens.get(&green).expect("the green's engine");
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
pub(crate) fn appended(
  state: &mut ShardState,
  repository: RepositoryId,
  changes: Vec<BranchChange>,
  sealed: (&mut super::SealedPush, &[StoreMove]),
) -> Result<Appended, crate::repository::push::AppendRefused> {
  // Its names and pack homed and landed first, as a client's edge carries them (C5 part 2b: no record carries a push's
  // objects); nothing to carry where it brings none.
  let _ = crate::vcs_store::commits::home_and_land_blocking(state, sealed.0);
  let (pushing, moves) = sealed;
  let appended = crate::repository::push::push_sealed(
    state,
    (&owner(), repository),
    changes,
    EntryParts::default(),
    (&mut *pushing, moves),
  );
  // Refused, its homing's holds are given back, as a refused push gives them back.
  if appended.is_err() {
    pushing.release_homed(state);
  }
  // A push's whole map is built by a task of the shard (A-664's live whole builds), which a test's step never runs:
  // driven to its end here, so the test reads the store as the build leaves it.
  crate::vcs_store::paths::whole_build::settle(state);
  appended
}

/// [`push_through`] with the push's whole map left to its build's task (A-664's live whole builds), as a push leaves it
/// when it is answered: the build is under way, not driven.
pub(crate) fn push_unsettled(
  state: &mut ShardState,
  (repository, green): (RepositoryId, DbVolumeId),
  objects: &[Brought],
  (base, head, tree_id): (Option<ObjectId>, ObjectId, ObjectId),
) {
  let push = begun(state, repository, objects);
  let version = state.greens.get(&green).expect("the green's engine").head();
  let built = work_of(state, (&push, green), (version, base), (head, tree_id));
  let mut sealed = push.seal(state).expect("sealed");
  let changes = vec![BranchChange::Update { green, work: built }];
  let moves = [(Target::Green(green), Some(head))];
  // Homed and landed first, as a client's edge carries a push (C5 part 2b: no record carries a push's objects).
  let _ = crate::vcs_store::commits::home_and_land_blocking(state, &mut sealed);
  crate::repository::push::push_sealed(
    state,
    (&owner(), repository),
    changes,
    EntryParts::default(),
    (&mut sealed, &moves),
  )
  .expect("appended");
  sealed.abandon(state);
}

/// One push through the shard: `objects` admitted, the work from `base` to the commit's tree, the push sealed with its
/// move and appended. The version it made.
pub(crate) fn push_through(
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

/// The root tree a commit's content names.
pub(crate) fn tree_of(objects: &[Brought]) -> ObjectId {
  objects
    .iter()
    .find(|(kind, ..)| *kind == Kind::Tree)
    .map(|(_, id, _)| *id)
    .expect("a tree")
}

/// Branch `topic` made at `version` of `from`, with no work of its own.
fn topic_at(from: DbVolumeId, version: u64) -> Vec<BranchChange> {
  vec![BranchChange::Create {
    name: "topic".to_owned(),
    from,
    version,
    work: None,
  }]
}

/// `topic`'s store move to `head`, named by its branch until its append makes its green.
fn topic_move(head: ObjectId) -> [StoreMove; 1] {
  [(Target::Created("topic".to_owned()), Some(head))]
}

/// The green an append made for its new branch.
fn created_green(made: &Appended) -> DbVolumeId {
  match made.changes.first() {
    Some(BranchAppended::Created { green, .. }) => *green,
    other => panic!("not created: {other:?}"),
  }
}

/// `repository`'s records, in the order its register received them.
pub(crate) fn records(state: &ShardState, repository: RepositoryId) -> Vec<Vec<u8>> {
  state
    .db
    .partition()
    .repositories()
    .objects(repository)
    .map(<[u8]>::to_vec)
    .collect()
}

/// The store made again from the partition's log (`super::rebuild`, the recovery's own replay) equals the live one,
/// its whole builds settled, whole over the refs keyed `greens`, every record replayed and none refused.
pub(crate) fn assert_made_again_whole(
  state: &mut ShardState,
  repository: RepositoryId,
  greens: &[u64],
) {
  // The live store's whole builds run in slices after a push's answer, and a rebuild settles its own before it is
  // compared (`settle_rebuild`), so the live store is compared settled too: otherwise a build under way holds what it
  // has put so far and the two differ in holders and contents while their rows and refs agree.
  super::paths::whole_build::settle(state);
  let live = {
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    facts(content, store, greens)
  };
  let count = u64::try_from(records(state, repository).len()).expect("a count");
  let replayed = state.refusals.get(super::REPLAYED).copied().unwrap_or(0);
  super::rebuild(state, repository);
  super::settle_rebuild(state, repository);
  assert_eq!(state.vcs_stores.unrecovered(repository), None);
  let made = {
    let (content, store) = state
      .vcs_stores
      .git_ref(repository)
      .expect("the store made again");
    facts(content, store, greens)
  };
  assert_eq!(made, live);
  assert_eq!(
    state.refusals.get(super::REPLAYED).copied(),
    Some(replayed + count)
  );
}

/// A-662 (a push without `atomic`, a ref at a time from one sealed push, as `repository::push` appends each). Do: admit
/// a commit's objects from a pack whose chains were two deep and seal the push; append its move of the default branch;
/// then, from the same push, append `topic` made at that commit; make the store again from the log. Expect: the second
/// record holds the push's deepest chain and its move alone, the first append having committed every object; the store
/// made again equal whole, both records replayed and none refused.
#[test]
fn t_a662_a_push_appended_a_ref_at_a_time_is_made_again_from_its_records() {
  crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "split", VcsTool::Git);
    let files = [(b"README".to_vec(), b"one\n".to_vec())];
    let (objects, one) = commit_of(&files, &[], FIRST_DATE);
    let push = begun_at(state, repository, &objects, DEEP);
    let work = work_of(state, (&push, root), (0, None), (one, tree_of(&objects)));
    let mut sealed = push.seal(state).expect("sealed");
    let main = vec![BranchChange::Update { green: root, work }];
    let moves = [(Target::Green(root), Some(one))];
    appended(state, repository, main, (&mut sealed, &moves)).expect("the default branch");
    let made = appended(
      state,
      repository,
      topic_at(root, 1),
      (&mut sealed, &topic_move(one)),
    )
    .expect("topic");
    sealed.abandon(state);
    let last = records(state, repository).pop().expect("a record");
    let read = Record::decode(&last, FORMAT).map(|record| (record.deepest, named_count(&record)));
    assert_eq!(read, Ok((DEEP, 0)));
    let greens = [green_key(root), green_key(created_green(&made))];
    assert_made_again_whole(state, repository, &greens);
  });
}

/// A-662 (two pushes in flight at once, as a hook's wait allows: one commits what the other holds). Do: admit one
/// commit's objects into two pushes, the second from a pack whose chains were two deep, and seal both; append the
/// first, moving the default branch; then the second, making `topic` at that commit; make the store again from the
/// log. Expect: the second record holds no object, the first's commit having given every one its position, and the
/// second's chain; the store made again equal whole, its learned depth the second's chain, both records replayed.
#[test]
fn t_a662_a_push_whose_objects_another_committed_first_records_none_of_them() {
  crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "twice", VcsTool::Git);
    let files = [(b"README".to_vec(), b"one\n".to_vec())];
    let (objects, one) = commit_of(&files, &[], FIRST_DATE);
    let first = begun(state, repository, &objects);
    let second = begun_at(state, repository, &objects, DEEP);
    let work = work_of(state, (&first, root), (0, None), (one, tree_of(&objects)));
    let mut first = first.seal(state).expect("sealed");
    let mut second = second.seal(state).expect("sealed");
    let main = vec![BranchChange::Update { green: root, work }];
    let moves = [(Target::Green(root), Some(one))];
    appended(state, repository, main, (&mut first, &moves)).expect("the first");
    first.abandon(state);
    let made = appended(
      state,
      repository,
      topic_at(root, 1),
      (&mut second, &topic_move(one)),
    )
    .expect("the second");
    second.abandon(state);
    let last = records(state, repository).pop().expect("a record");
    let read = Record::decode(&last, FORMAT).map(|record| (record.deepest, named_count(&record)));
    assert_eq!(read, Ok((DEEP, 0)));
    let learned = state
      .vcs_stores
      .git_ref(repository)
      .map(|(_, store)| store.repository.learned_depth());
    assert_eq!(learned, Some(DEEP));
    let greens = [green_key(root), green_key(created_green(&made))];
    assert_made_again_whole(state, repository, &greens);
  });
}

/// A-662 (a push without `atomic`: each ref its own append, the push kept whole by its caller). Do: push a first commit;
/// then admit a second push's objects, seal it and append it with a work made against the stale version 0, which the
/// verdict refuses; append it again with the work made against version 1. Expect: the stale append
/// refused with nothing applied, the store unmoved and the push still holding its objects; the second append accepted
/// as version 2, its record holding every object the push brought, and the store's ref at the new head.
#[test]
fn t_a662_a_refused_ref_leaves_the_push_whole_for_the_next() {
  crate::test_store::audit_on_stored_shard("vcs", |state| {
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
    let fresh = work_of(
      state,
      (&push, root),
      (1, Some(first_tree)),
      (two, second_tree),
    );
    let mut sealed = push.seal(state).expect("sealed");
    let moves = [(Target::Green(root), Some(two))];
    let held = state.vcs_stores.content().map(ContentStore::used);
    let kept = super::homes::region_held_here();
    let changes = vec![BranchChange::Update {
      green: root,
      work: stale,
    }];
    assert!(appended(state, repository, changes, (&mut sealed, &moves)).is_err());
    // The refused push's homing left its names and pack with their region home (this shard): the home's, let go here
    // as the home lets go of content no record names; the repository holds nothing more.
    let_go_region_holds_since(state, &kept);
    assert_eq!(state.vcs_stores.content().map(ContentStore::used), held);
    assert_eq!(records(state, repository).len(), 1);
    let changes = vec![BranchChange::Update {
      green: root,
      work: fresh,
    }];
    let made = appended(state, repository, changes, (&mut sealed, &moves)).expect("appended");
    assert!(matches!(
      made.changes.first(),
      Some(BranchAppended::Moved { version: 2, .. })
    ));
    sealed.abandon(state);
    let last = records(state, repository).pop().expect("a record");
    let read = Record::decode(&last, FORMAT).expect("a record");
    assert_eq!(named_count(&read), second.len());
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    let key = green_key(root);
    let newest = store
      .repository
      .versions(key)
      .checked_sub(1)
      .expect("a version");
    assert_eq!(store.repository.head(content, key, newest), Some(two));
  });
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
  crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "kernel", VcsTool::Git);
    let files = [(b"README".to_vec(), b"one\n".to_vec())];
    let (first, one) = commit_of(&files, &[], FIRST_DATE);
    assert_an_abandoned_push_leaves_nothing(state, repository, &first);
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
      push_through(
        state,
        (repository, root),
        &second,
        (Some(first_tree), two, second_tree)
      ),
      2
    );
    assert_recorded_and_charged(state, repository, root, (first.len(), second.len()));
    let key = green_key(root);
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    assert_eq!(store.version_of(content, key, &one), Some((1, first_tree)));
    assert_eq!(store.version_of(content, key, &two), Some((2, second_tree)));
    assert_eq!(store.versions(key), &[1, 2]);
    assert_made_again_whole(state, repository, &[key]);
    // The records' own holds of their names and packs, which a free leaves (a rebuild replays the same records), let go
    // as a destroy of the repository lets them go with its records.
    super::release_record_holds(state, repository);
    super::free(state, repository);
    // The region's own holds of the pushes' names and packs (this shard their home) are the home's, not the
    // repository's: let go as the home lets go of content no record names, then nothing is held or charged.
    let_go_region_holds_since(state, &[]);
    assert_eq!(state.vcs_stores.charged(), 0);
    assert_eq!(state.vcs_stores.content().map(ContentStore::used), Some(0));
  });
}

/// A push of `objects` to `repository` admitted and abandoned leaves nothing held or charged.
fn assert_an_abandoned_push_leaves_nothing(
  state: &mut ShardState,
  repository: RepositoryId,
  objects: &[Brought],
) {
  let mut abandoned = GitPush::begin(&state.vcs_stores, repository).expect("a git store");
  let arriving: Vec<Incoming<'_>> = objects.iter().map(|brought| incoming(brought, 0)).collect();
  abandoned.admit_all(state, &arriving).expect("admitted");
  assert!(!abandoned.quarantine().is_empty());
  abandoned.abandon(state);
  assert_eq!(state.vcs_stores.content().map(ContentStore::used), Some(0));
  assert_eq!(state.vcs_stores.charged(), 0);
}

/// The register holds one record a push, the second holding only what the first did not bring, and the store's
/// charge is what its content store holds.
fn assert_recorded_and_charged(
  state: &ShardState,
  repository: RepositoryId,
  root: DbVolumeId,
  (first, second): (usize, usize),
) {
  let records = records(state, repository);
  let read: Vec<Record> = records
    .iter()
    .map(|bytes| Record::decode(bytes, FORMAT).expect("a record"))
    .collect();
  let counts: Vec<usize> = read.iter().map(named_count).collect();
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
  crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "made", VcsTool::Git);
    let files = [(b"README".to_vec(), b"one\n".to_vec())];
    let (first, one) = commit_of(&files, &[], FIRST_DATE);
    let first_tree = tree_of(&first);
    push_through(state, (repository, root), &first, (None, one, first_tree));
    let files = [(b"README".to_vec(), b"topic\n".to_vec())];
    let (topic, head) = commit_of(&files, &[one], FIRST_DATE + 1);
    let push = begun(state, repository, &topic);
    let work = work_of(
      state,
      (&push, root),
      (1, Some(first_tree)),
      (head, tree_of(&topic)),
    );
    let mut sealed = push.seal(state).expect("sealed");
    let changes = vec![BranchChange::Create {
      name: "topic".to_owned(),
      from: root,
      version: 1,
      work: Some(work),
    }];
    let made =
      appended(state, repository, changes, (&mut sealed, &topic_move(head))).expect("appended");
    sealed.abandon(state);
    let Some(BranchAppended::Created { green, version: 1 }) = made.changes.first().cloned() else {
      panic!("not created: {made:?}");
    };
    let key = green_key(green);
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    assert_eq!(store.versions(key), &[1]);
    assert_eq!(store.repository.head(content, key, 0), Some(head));
    let last = records(state, repository).pop().expect("a record");
    let read = Record::decode(&last, FORMAT).expect("a record");
    assert_eq!(read.moves, vec![(key, 1, Some(head))]);
  });
}

/// The files of the map the store holds for `green`, a green of `repository`, when it is of `version` (A-664 part 3).
fn kept_files(
  state: &ShardState,
  repository: RepositoryId,
  green: DbVolumeId,
  version: u64,
) -> Option<BTreeMap<Vec<u8>, slates_vcs::derived::paths::Entry>> {
  let (content, git) = state.vcs_stores.git_ref(repository)?;
  let paths = git.paths.as_ref()?;
  let head = git
    .heads
    .get(&green_key(green))
    .filter(|head| head.version == version)?;
  paths.files(content, head.root).ok()
}

/// A-664 part 3 (its first piece: a push's version keeps its map of paths to git's files, as a submit's does). Do: push
/// a commit of two files to a git repository's root green through A-635's append. Expect: the green's new version keeps
/// a map holding both, each a regular file whose content is its blob's bytes as an object, built whole (the green's
/// first version) and none refused.
#[test]
fn t_a664_p3_a_pushs_version_keeps_its_map_of_gits_files() {
  crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, green) = repository(state, "pushed", VcsTool::Git);
    let (a, a_bytes) = blob(b"alpha\n");
    let (b, b_bytes) = blob(b"beta\n");
    let (tree_id, tree_bytes) = tree(&[(b"a".to_vec(), a), (b"b".to_vec(), b)]);
    let (head, commit_bytes) = commit(tree_id, &[], FIRST_DATE);
    let objects: Vec<Brought> = vec![
      (Kind::Blob, a, a_bytes.clone()),
      (Kind::Blob, b, b_bytes.clone()),
      (Kind::Tree, tree_id, tree_bytes),
      (Kind::Commit, head, commit_bytes),
    ];
    let version = push_through(state, (repository, green), &objects, (None, head, tree_id));
    let files = kept_files(state, repository, green, version).expect("the version's map");
    let content = state.vcs_stores.content().expect("the content store");
    let file = |bytes: &[u8]| slates_vcs::derived::paths::Entry {
      class: slates_git::derived::FileClass::File.class(),
      content: content.object_id(bytes),
    };
    let expected = BTreeMap::from([
      (b"a".to_vec(), file(&a_bytes)),
      (b"b".to_vec(), file(&b_bytes)),
    ]);
    assert_eq!(files, expected);
    let counted = |name: &str| state.refusals.get(name).copied();
    assert_eq!(
      (counted(super::paths::BUILT), counted(super::paths::REFUSED)),
      (Some(1), None)
    );
  });
}

/// A-681 (§4.17.2's reaping, applied to an HTTP commit's transaction). Do: keep a transaction begun at instant 1,000,
/// release idle ones at 1,500 against a bound of 1,000, take it and put it back then, release at 2,400, then at 2,600.
/// Expect it kept through the first two releases (its last step at 1,500 is within the bound at 2,400) and released by
/// the third, after which no take finds it.
#[test]
fn an_http_commit_idle_past_the_edges_idle_bound_is_released() {
  use slates_svn::commit::drive::Setup;
  use slates_svn::dav::commit::{DavCommit, World};
  use slates_svn::dump::Uuid;
  use slates_svn::lock::{AprTime, CommitLocks};
  use slates_svn::migration::{Repository, Room};

  let room = Room {
    entries: 1 << 20,
    bytes: 1 << 30,
  };
  let uuid = Uuid::parse(b"00000000-0000-4000-8000-000000000681").unwrap();
  let records = Repository::new(uuid, b"2026-11-20T00:00:00.000000Z", room).unwrap();
  let world = World {
    index: records.index(),
    green: &records,
    locks: records.locks(),
  };
  let setup = Setup {
    base_path: String::from("/"),
    repository_url: String::from("/project"),
    filesystem: String::from("project"),
    transaction: String::from("0-1"),
    locks: CommitLocks {
      user: Some(String::from("alice")),
      presented: BTreeMap::new(),
      keep: false,
    },
    now: AprTime(1),
    room: 1 << 20,
    rules: None,
    revision_properties: Vec::new(),
  };
  let texts = Box::new(slates_svn::commit::drive::HeldTexts::new());
  let commit = DavCommit::begin(setup, &world, &|_| Ok(()), texts).unwrap();
  let id = RepositoryId { bytes: [6; 16] };
  let mut stores = super::VcsStores::default();
  stores.begin_dav_commit(id, b"alice", String::from("0-1"), (commit, 1_000));
  stores.release_idle_dav_commits(1_500, Some(1_000));
  let taken = stores.take_dav_commit(id, b"alice", ("0-1", 1_500));
  let found = taken.is_some();
  if let Some(commit) = taken {
    stores.keep_dav_commit(id, b"alice", "0-1", (commit, 1_500));
  }
  stores.release_idle_dav_commits(2_400, Some(1_000));
  let kept = stores.dav_commits.len();
  stores.release_idle_dav_commits(2_600, Some(1_000));
  let after = stores
    .take_dav_commit(id, b"alice", ("0-1", 2_600))
    .is_some();
  assert_eq!((found, kept, after), (true, 1, false));
}

/// A git repository of `pushes` pushes to its root green, each a commit changing `README` over the one before: its id
/// and root.
fn pushed_many(state: &mut ShardState, name: &str, pushes: u64) -> (RepositoryId, DbVolumeId) {
  let (repository, root) = repository(state, name, VcsTool::Git);
  let mut parent: Option<(ObjectId, ObjectId)> = None;
  for push in 0..pushes {
    let files = [(b"README".to_vec(), format!("push {push}\n").into_bytes())];
    let parents: Vec<ObjectId> = parent.iter().map(|(commit, _)| *commit).collect();
    let (objects, commit) = commit_of(&files, &parents, FIRST_DATE.saturating_add(push));
    let tree = tree_of(&objects);
    let base = parent.map(|(_, tree)| tree);
    push_through(state, (repository, root), &objects, (base, commit, tree));
    parent = Some((commit, tree));
  }
  (repository, root)
}

/// A-732's sibling for git (main's ruling of 2026-11-20: a large repository's rebuild in one step spun past the
/// watchdog and crash-looped the daemon). Do: push five commits to a git repository; make its store again from the log.
/// Expect: the rebuild under way and the store lent to nothing; its five records replayed a turn each at least (a pack
/// record over as many as its units take, A-722 part 4); then the store
/// lent again and its green's map made again by its build. Red before the change: the store was made whole at once.
#[test]
fn t_a732_a_git_rebuild_lends_nothing_and_replays_a_record_a_turn() {
  let seen = crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = pushed_many(state, "sliced-rebuild", 5);
    let records = records(state, repository).len();
    super::rebuild(state, repository);
    let during = (
      state.vcs_stores.rebuilding(repository),
      state.vcs_stores.git_ref(repository).is_none(),
    );
    let mut turns = 0usize;
    while super::rebuild_turn_once(state, repository) {
      turns = turns.saturating_add(1);
    }
    let lent =
      state.vcs_stores.git_ref(repository).is_some() && !state.vcs_stores.rebuilding(repository);
    crate::vcs_store::paths::whole_build::settle(state);
    let mapped = state
      .vcs_stores
      .git_ref(repository)
      .is_some_and(|(_, git)| git.heads.contains_key(&green_key(root)));
    (during, turns >= records && records == 5, lent, mapped)
  });
  assert_eq!(seen, ((true, true), true, true, true));
}

/// Shape: the files of the one commit whose push's record holds many objects: enough that a replay a unit a turn takes
/// many turns, few enough for a test's step.
const MANY_FILES: usize = 64;

/// A-722 part 4 (main's ruling 2026-11-20: a large pack record's replay ran in one turn and crash-looped the daemon). Do:
/// push one commit of [`MANY_FILES`] files to a git repository, one record whose pack holds every blob, the tree and the
/// commit; make its store again from the log, each turn held to no time at all. Expect: the one record replayed over
/// at least as many turns as its pack has objects, and the store made again holding the commit at the green's head.
/// Red before the change: the record was replayed whole in one turn.
#[test]
fn t_a722_p4_a_many_object_pack_record_is_replayed_over_many_turns() {
  let seen = crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "many-objects", VcsTool::Git);
    let files: Vec<(Vec<u8>, Vec<u8>)> = (0..MANY_FILES)
      .map(|n| {
        (
          format!("f{n}").into_bytes(),
          format!("file {n}\n").into_bytes(),
        )
      })
      .collect();
    let (objects, commit) = commit_of(&files, &[], FIRST_DATE);
    let tree = tree_of(&objects);
    push_through(state, (repository, root), &objects, (None, commit, tree));
    let held = records(state, repository).len();
    super::rebuild(state, repository);
    let mut turns = 0usize;
    while super::rebuild_turn_once_within(state, repository, 0) {
      turns = turns.saturating_add(1);
    }
    crate::vcs_store::paths::whole_build::settle(state);
    let key = green_key(root);
    let head = state
      .vcs_stores
      .git_ref(repository)
      .and_then(|(content, git)| {
        let newest = git.repository.versions(key).checked_sub(1)?;
        git.repository.head(content, key, newest)
      });
    (held, turns >= objects.len(), head == Some(commit))
  });
  assert_eq!(seen, (1, true, true));
}

/// A-722 part 4 (a push's seal and stage a unit at a time on the owner, `GitPush::sealing`). Do: begin a push of one
/// commit of [`MANY_FILES`] files; seal it a turn at a time, each turn held to no time at all, counting the turns; land
/// it. Expect: more turns than the push holds objects; the push landed, its green's head the commit.
#[test]
fn t_a722_p4_a_pushs_seal_takes_many_turns_and_lands() {
  let seen = crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "many-sealed", VcsTool::Git);
    let files: Vec<(Vec<u8>, Vec<u8>)> = (0..MANY_FILES)
      .map(|n| {
        (
          format!("f{n}").into_bytes(),
          format!("file {n}\n").into_bytes(),
        )
      })
      .collect();
    let (objects, commit) = commit_of(&files, &[], FIRST_DATE);
    let tree = tree_of(&objects);
    let push = begun(state, repository, &objects);
    let version = state.greens.get(&root).expect("the green's engine").head();
    let built = work_of(state, (&push, root), (version, None), (commit, tree));
    let mut sealing = push.sealing(state).expect("sealing");
    let mut turns = 0usize;
    let mut sealed = loop {
      turns += 1;
      match sealing.turn(state, 0).expect("a turn") {
        super::SealTurn::More(more) => sealing = *more,
        super::SealTurn::Sealed(sealed) => break *sealed,
      }
    };
    let changes = vec![BranchChange::Update {
      green: root,
      work: built,
    }];
    let moves = [(Target::Green(root), Some(commit))];
    appended(state, repository, changes, (&mut sealed, &moves)).expect("appended");
    sealed.abandon(state);
    let key = green_key(root);
    let head = state
      .vcs_stores
      .git_ref(repository)
      .and_then(|(content, git)| {
        let newest = git.repository.versions(key).checked_sub(1)?;
        git.repository.head(content, key, newest)
      });
    (turns > objects.len(), head == Some(commit))
  });
  assert_eq!(seen, (true, true));
}

/// A-722 part 4c (a move admitted a turn at a time before its append, `SealedPush::admission_turn`). Do: seal a push of
/// one commit of [`MANY_FILES`] files; admit its green's move to the commit a turn at a time, each turn held to no time
/// at all, counting the turns; append it. Expect: more turns than the push holds objects; the append took the move
/// admitted before it (none left kept) and landed it, the green's head the commit.
#[test]
fn t_a722_p4_a_move_admitted_over_many_turns_is_the_one_its_append_lands() {
  let seen = crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "many-admitted", VcsTool::Git);
    let files: Vec<(Vec<u8>, Vec<u8>)> = (0..MANY_FILES)
      .map(|n| {
        (
          format!("f{n}").into_bytes(),
          format!("file {n}\n").into_bytes(),
        )
      })
      .collect();
    let (objects, commit) = commit_of(&files, &[], FIRST_DATE);
    let tree = tree_of(&objects);
    let push = begun(state, repository, &objects);
    let version = state.greens.get(&root).expect("the green's engine").head();
    let built = work_of(state, (&push, root), (version, None), (commit, tree));
    let mut sealed = push.seal(state).expect("sealed");
    let (mut admitting, mut turns) = (None, 0usize);
    let key = green_key(root);
    loop {
      turns += 1;
      if sealed.admission_turn(state, (key, commit), (&mut admitting, 0)) {
        break;
      }
    }
    let kept = sealed.prepared.len();
    let changes = vec![BranchChange::Update {
      green: root,
      work: built,
    }];
    let moves = [(Target::Green(root), Some(commit))];
    appended(state, repository, changes, (&mut sealed, &moves)).expect("appended");
    let left = sealed.prepared.len();
    sealed.abandon(state);
    let head = state
      .vcs_stores
      .git_ref(repository)
      .and_then(|(content, git)| {
        let newest = git.repository.versions(key).checked_sub(1)?;
        git.repository.head(content, key, newest)
      });
    (turns, kept, left, head == Some(commit))
  });
  // A create is admitted with no walk (A-614 amended), in one turn; its sets are its first reader's to derive.
  assert_eq!(seen, (1, 1, 0, true));
}

/// A-752 part 3 (main's order of 2026-11-20: a push changing every file of a 1,500-file head stepped its map in its
/// answer's step, 52 ms debug). Do: push a commit of [`MANY_FILES`] files and build its map; push a second changing every
/// file but one and removing one; step the map's change build a slice at a time, each slice given no budget (one path),
/// counting the slices; then build the head's map whole. Expect: the change build begun once, its first path put in the
/// push's own step and each other in a slice of its own (62 of the change's 63 paths: 62 changed, one removed), the map
/// at the first version until they ran; its map and trees derived (not built whole), and equal, roots and all, to the
/// ones built from the whole head.
#[test]
fn t_a752_p3_a_pushs_map_step_runs_a_path_a_slice_and_equals_the_whole_build() {
  let seen = crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "change-build", VcsTool::Git);
    let files = |word: &str, count: usize| -> Vec<(Vec<u8>, Vec<u8>)> {
      (0..count)
        .map(|n| {
          let bytes = if n == 0 {
            "kept\n".to_owned()
          } else {
            format!("{word} {n}\n")
          };
          (format!("f{n:03}").into_bytes(), bytes.into_bytes())
        })
        .collect()
    };
    let (objects, first) = commit_of(&files("first", MANY_FILES), &[], FIRST_DATE);
    let first_tree = tree_of(&objects);
    push_through(
      state,
      (repository, root),
      &objects,
      (None, first, first_tree),
    );
    crate::vcs_store::paths::whole_build::settle(state);
    let (objects, second) = commit_of(&files("second", MANY_FILES - 1), &[first], FIRST_DATE + 1);
    let begun_before = state
      .refusals
      .get(super::paths::whole_build::CHANGE_BEGUN)
      .copied();
    let derived_before = state
      .refusals
      .get(super::paths::DERIVED)
      .copied()
      .unwrap_or(0);
    push_unsettled(
      state,
      (repository, root),
      &objects,
      (Some(first_tree), second, tree_of(&objects)),
    );
    let version = state.greens.get(&root).expect("the green's engine").head();
    let key = green_key(root);
    let head_version = |state: &ShardState| {
      state
        .vcs_stores
        .git_ref(repository)
        .and_then(|(_, git)| git.heads.get(&key).map(|head| head.version))
    };
    let pending = head_version(state) == Some(version - 1);
    let mut slices = 0usize;
    while super::paths::whole_build::building(state, root) {
      super::paths::whole_build::step_once(state);
      slices = slices.saturating_add(1);
    }
    let derived = state
      .refusals
      .get(super::paths::DERIVED)
      .copied()
      .unwrap_or(0)
      - derived_before;
    let begun = state
      .refusals
      .get(super::paths::whole_build::CHANGE_BEGUN)
      .copied()
      .unwrap_or(0)
      - begun_before.unwrap_or(0);
    let stepped = state
      .vcs_stores
      .git_ref(repository)
      .and_then(|(_, git)| git.heads.get(&key).copied())
      .expect("the change build's map");
    super::paths::remake_green(state, root);
    let whole = state
      .vcs_stores
      .git_ref(repository)
      .and_then(|(_, git)| git.heads.get(&key).copied())
      .expect("the whole build's map");
    (
      pending,
      begun,
      slices >= MANY_FILES - 2,
      derived,
      (stepped.version, stepped.root, stepped.trees) == (whole.version, whole.root, whole.trees),
    )
  });
  assert_eq!(seen, (true, 1, true, 1, true));
}

/// The objects repository `id`'s store's walks have visited so far, read while a rebuild lends the store to nothing.
fn walked(state: &ShardState, id: RepositoryId) -> u64 {
  match state.vcs_stores.entries.get(&id) {
    Some(super::RepositoryStore::Git(git)) => git.repository.counters().walked,
    _ => 0,
  }
}

/// A-722 part 4g (main's report of 2026-11-20: after a restart, a daemon recovering a repository pushed git/git's v2.0.0
/// spun past the watchdog's window and was killed again, a crash loop); A-614 amended (main's ruling (A) of 2026-11-21,
/// its condition 3: a replay walks nothing, so a restart's cost stays flat in history). Do: push one commit of
/// [`MANY_FILES`] files; make the store again from the log, each turn held to no time at all, reading after each turn
/// how many objects the store's walks visited. Expect: the record replayed, and no object walked at all, by any turn:
/// the create is admitted with no walk, its sets its first reader's to derive. Red before A-614's amendment: the move's
/// admission walked every object the push brought, an object a turn (and before part 4g, the whole walk in one turn).
#[test]
fn t_a722_p4g_a_replayed_records_move_is_admitted_an_object_a_turn() {
  let (most, total, objects) = crate::test_store::audit_on_stored_shard("vcs", |state| {
    let (repository, root) = repository(state, "admitted-in-turns", VcsTool::Git);
    let files: Vec<(Vec<u8>, Vec<u8>)> = (0..MANY_FILES)
      .map(|n| {
        (
          format!("f{n}").into_bytes(),
          format!("file {n}\n").into_bytes(),
        )
      })
      .collect();
    let (objects, commit) = commit_of(&files, &[], FIRST_DATE);
    let tree = tree_of(&objects);
    push_through(state, (repository, root), &objects, (None, commit, tree));
    super::rebuild(state, repository);
    let (mut most, mut before, begun) =
      (0u64, walked(state, repository), walked(state, repository));
    while super::rebuild_turn_once_within(state, repository, 0) {
      let now = walked(state, repository);
      most = most.max(now.saturating_sub(before));
      before = now;
    }
    (most, before.saturating_sub(begun), objects.len())
  });
  assert!(objects > 0);
  assert_eq!((total, most), (0, 0), "a replay walks nothing");
}

/// main's ruling (b) on the identity rule's compatibility shim (banned item 7): a git record that names an object past
/// one window by this node's root of it, as records did before the rule, is refused, never read as the object. Do:
/// push a create of `main` whose one file is 512 KiB (its record version 5, naming its objects by its names' content);
/// take its record and name the file in that content by its root in place of its BLAKE3, the content put again; replay
/// the record so altered into the repository's store made again empty. Expect: the record as written
/// replays; the altered one refused, typed (`ReplayRefused::Diverged`: the objects the replay admits are named by
/// their BLAKE3, which the record does not name), never accepted.
#[test]
fn t_identity_rule_a_record_naming_an_object_by_its_old_root_is_refused_typed() {
  crate::test_store::audit_on_stored_shard("vcs", |state| {
    // Shape: a file past one window of the content store (its `assert_ne!` below holds it so), and within the room a
    // wiring test's work is built in ([`ROOM`]).
    const LARGE: usize = 1 << 19;
    let (repository, green) = repository(state, "old-form", VcsTool::Git);
    let blob: Vec<u8> = (0..LARGE)
      .map(|at| u8::try_from(at % 251).unwrap_or(u8::MAX))
      .collect();
    let (objects, head) = commit_of(&[(b"large".to_vec(), blob.clone())], &[], FIRST_DATE);
    push_through(
      state,
      (repository, green),
      &objects,
      (None, head, tree_of(&objects)),
    );
    let identity = ContentId::of(&blob);
    // The root a record before the rule named the file by: the one this store would cut its bytes into (the file is
    // held where it lies in its push's pack now, A-722's record v4, so the store keeps no root of it).
    let window = state
      .vcs_stores
      .content()
      .map(ContentStore::window)
      .unwrap();
    let mut scratch = ContentStore::new(u64::try_from(4 * LARGE).unwrap(), window).unwrap();
    let held = scratch.put_object(&blob).unwrap();
    let root = scratch.root_of(&held);
    assert_ne!(root, identity, "the file is past one window");
    // A record names its objects by its names' content (version 5): the file's name in it rewritten to its old root,
    // the content put again, and the record written again naming that content.
    let names_of = |state: &mut ShardState, record: &[u8]| {
      let (named, count) = Record::decode(record, FORMAT).ok()?.named?;
      // Read with the shard's chunk store lent, where the names' windows are kept.
      let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
      super::record_names::names_from_content(lent.stores.content()?, &named, count).ok()
    };
    let written = records(state, repository)
      .into_iter()
      .find(|record| {
        names_of(state, record).is_some_and(|names| names.iter().any(|name| name.1 == identity))
      })
      .expect("a record naming the file");
    let read = Record::decode(&written, FORMAT).expect("a record");
    let mut names = names_of(state, &written).expect("its names");
    for name in &mut names {
      if name.1 == identity {
        name.1 = root;
      }
    }
    let renamed = {
      // The room the names' content needs, lent as a push's homing lends it (`vcs_store::put_names`).
      super::lend(
        &mut state.store,
        &mut state.vcs_stores,
        u64::try_from(written.len()).unwrap(),
      );
      let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
      let content = lent.stores.content.as_mut().expect("the content store");
      super::record_names::names_to_content(content, &names).expect("names put")
    };
    super::settle(state);
    let altered = RecordWriter::finish_packed(
      (read.deepest, &read.moves, &read.bases),
      Some(renamed),
      read.packed,
      read.sequence,
    )
    .expect("written");
    let record = state
      .db
      .partition()
      .repositories()
      .repository(repository)
      .cloned()
      .unwrap();
    let mut replayed = |bytes: &[u8]| {
      state.vcs_stores.entries.remove(&repository);
      super::make(state, &record);
      // The room a replay is lent, as `replay_record` lends it: the record's bytes.
      super::lend(
        &mut state.store,
        &mut state.vcs_stores,
        u64::try_from(bytes.len()).unwrap(),
      );
      let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
      super::replay_into(lent.stores, repository, bytes, None).map(|_| ())
    };
    assert_eq!(replayed(&written), Ok(()), "the record as written replays");
    assert_eq!(replayed(&altered), Err(super::ReplayRefused::Diverged));
  });
}

/// Goal 93 item 2b (main 01a20182's orders of 2026-11-23: the let-go a push's end owed ran as one walk over its whole
/// pack inside `ReceivePack`'s drop, git/git's ~600 MB at once, a bounded-work breach; the shard's step quantum alone
/// ends a slice; each slice judged by the shard's own measures, never a test's wall time, so load cannot fail it). Do: on
/// an audit shard, put a landed pack of 131 windows in a git repository's content store and owe its let-go, then go idle
/// (`git_encode::give_back_idle`, as a push's end and an idle repository run it); then turn the shard's let-go queue a
/// slice at a time (`let_go_turn`) to its end, each slice's own record kept (`SliceTrace`: the quantum it read, each
/// window's span by its clock, its own past-the-quantum judgement). Expect: going idle lets nothing go itself, only
/// queues it; the let-go takes a slice to list the pack's windows and then more, every slice counted (`LET_GO_SLICES`)
/// and recorded; within a slice, each window after the first was taken only where the slice's elapsed and its longest
/// window so far still fit its quantum; the slices judged past the quantum each counted (`LET_GO_PAST_QUANTUM`); and at
/// its end the whole pack let go. Red before: the pack's every window let go within the idle give-back's one step; and,
/// with a test's wall time judging the slices, a spinning machine failed it (123 of 128 slices past by the shard's
/// counter, 128 by the test's clock).
#[test]
fn t_goal93_a_large_landed_packs_let_go_is_sliced_within_the_step_quantum_never_one_walk() {
  use super::{
    LANDED_LET_GO_BYTES, LANDED_PINNED, LET_GO_PAST_QUANTUM, LET_GO_SLICES, LET_GO_TRACE,
  };
  let seen = crate::test_store::audit_on_stored_shard("let-go-sliced", |state| {
    let id = crate::git_push::tests::git_repository(state, "let-go-sliced");
    let window = state
      .vcs_stores
      .content()
      .map_or(1 << 18, ContentStore::window);
    let windows = 131;
    let bytes: Vec<u8> = (0..windows * window)
      .map(|at| u8::try_from((at / 7) % 251).unwrap())
      .collect();
    let len = u64::try_from(bytes.len()).unwrap();
    let pack = super::within_budget(state, len, |stores| {
      stores.content_mut().unwrap().put_object(&bytes).unwrap()
    });
    state
      .vcs_stores
      .landed_owed
      .entry(id)
      .or_default()
      .push(pack);
    let count =
      |state: &crate::state::ShardState, name: &str| state.refusals.get(name).copied().unwrap_or(0);
    let freed = |state: &crate::state::ShardState| count(state, LANDED_LET_GO_BYTES);
    let before = (
      freed(state),
      count(state, LET_GO_SLICES),
      count(state, LET_GO_PAST_QUANTUM),
    );
    LET_GO_TRACE.with(|trace| trace.borrow_mut().clear());
    super::git_encode::give_back_idle(state, id);
    let in_the_step = freed(state) - before.0;
    while super::let_go_turn(state) {}
    let trace = LET_GO_TRACE.with(|trace| trace.borrow().clone());
    let whole_after = state
      .vcs_stores
      .content()
      .is_some_and(|content| content.holds_whole(&pack));
    (
      in_the_step,
      trace,
      (
        count(state, LET_GO_SLICES) - before.1,
        count(state, LET_GO_PAST_QUANTUM) - before.2,
      ),
      (freed(state) - before.0, count(state, LANDED_PINNED), len),
      whole_after,
    )
  });
  let (in_the_step, trace, (counted, past), (freed, pinned, len), whole_after) = seen;
  let windows: usize = trace.iter().map(|slice| slice.windows.len()).sum();
  eprintln!(
    "let-go: {counted} slices, {past} past the quantum, {windows} windows, freed {freed} of {len} bytes"
  );
  assert_eq!(
    in_the_step, 0,
    "nothing let go within the step that owes it"
  );
  assert!(counted >= 4, "the listing and many slices: {counted}");
  assert_eq!(
    counted,
    u64::try_from(trace.len()).unwrap(),
    "every slice recorded"
  );
  for slice in &trace {
    let mut longest = 0;
    for pair in slice.windows.windows(2) {
      let (begun, ended) = pair[0];
      longest = longest.max(ended - begun);
      assert!(
        ended + longest <= slice.quantum_ns,
        "a window taken past the quantum by the slice's own measure: {slice:?}"
      );
    }
  }
  let judged_past = trace.iter().filter(|slice| slice.past).count();
  assert_eq!(
    past,
    u64::try_from(judged_past).unwrap(),
    "each slice judged past the quantum counted"
  );
  assert_eq!((freed, pinned), (len, 0), "the whole pack let go");
  assert!(!whole_after, "the pack held whole no more");
}

/// The guard on room asked inside a lend (main 01a20182's ruling of 2026-11-23 21:30; w-tsan found six steps asking for
/// room inside `lent_state`, where the pool is out of reach, each refused `OverBudget` only once the arena's slack ran
/// out). Do: on a store-backed shard holding a repository's content store, grow the arena by a window's room outside
/// any lend, so it holds slack; put a window within room asked inside `lent_state`, then within room asked outside one.
/// Expect: inside, the slack able to take the put, yet the put refused `ContentRefused::OutOfReach` with its need,
/// nothing kept, and the ask counted once (`vcs_store.room_out_of_reach`); outside, the put taken and nothing more
/// counted. Red without the guard: the put inside taken from the slack, the misuse unseen (2026-11-23).
#[test]
fn room_asked_inside_a_lend_is_refused_out_of_reach_at_its_first_put() {
  let bytes = b"a window put within the room asked for it".to_vec();
  let need = u64::try_from(bytes.len()).unwrap();
  let (slack_inside, inside, held_after, outside, counted) =
    crate::test_store::audit_on_stored_shard("room-out-of-reach", move |state| {
      crate::git_push::tests::git_repository(state, "room-out-of-reach");
      let window = u64::try_from(state.store.content.chunk_bytes()).unwrap();
      super::within_budget(state, window, |_| ());
      let put =
        |stores: &mut super::VcsStores| stores.content_mut().map(|content| content.put(&bytes));
      let (slack_inside, inside) = super::lent_state(state, |state| {
        (
          state.store.admittable(),
          super::within_budget(state, need, put),
        )
      });
      let held_after = state
        .vcs_stores
        .content()
        .is_some_and(|content| content.contains(&ContentId::of(&bytes)));
      let outside = super::within_budget(state, need, put);
      let counted = state
        .refusals
        .get(super::ROOM_OUT_OF_REACH)
        .copied()
        .unwrap_or(0);
      (slack_inside, inside, held_after, outside, counted)
    });
  assert!(
    slack_inside >= need,
    "the slack inside the lend could take the put ({slack_inside} bytes), so the kind alone refuses"
  );
  assert_eq!(
    inside,
    Some(Err(ContentRefused::OutOfReach { need })),
    "room asked inside a lend refused by its own kind at the first put"
  );
  assert!(!held_after, "nothing kept by the refused put");
  assert!(
    matches!(outside, Some(Ok(_))),
    "the same put within room asked outside the lend taken: {outside:?}"
  );
  assert_eq!(counted, 1, "the ask inside the lend counted once");
}
