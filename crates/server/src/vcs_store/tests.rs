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

use super::record::{
  Noted, Record, RecordRefused, RecordWriter, RecordedMove, RecordedObject, WriteRefused,
};
use super::{
  GitPush, GitRefused, GitStore, ReplayRefused, RepositoryStore, Target, admit_object,
  commit_moves, green_key, record_of, replay_git, replay_into, seal_moves,
};
use crate::repository::push::{Appended, BranchAppended, BranchChange, EntryParts, StoreMove};
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

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
const FIRST_DATE: u64 = 1_700_000_000;

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

/// A-662 (the record's format, pinned). Do: write a record of one delete, one move and one object held by a content
/// store, then read it back. Expect: the bytes the module's encoding states, byte for byte, the object's bytes read back
/// from its root; and the same moves and object read back.
#[test]
fn t_a662_a_record_reads_back_what_it_wrote_and_its_bytes_are_pinned() {
  let head = ObjectId::from_bytes(FORMAT, &[0xab; 20]).expect("an id");
  let (content, noted) = holding(Kind::Blob, b"hi");
  let mut writer = RecordWriter::default();
  writer.object(noted);
  let moves: Vec<RecordedMove> = vec![(7, 4, None), (9, 5, Some(head))];
  let bytes = writer
    .finish(&content, |_| true, (3, &moves))
    .expect("written");
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
  expected.extend_from_slice(&noted.identity.0);
  expected.extend_from_slice(&2u64.to_le_bytes());
  expected.extend_from_slice(b"hi");
  assert_eq!(bytes, expected);
  let object = RecordedObject {
    kind: Kind::Blob,
    identity: noted.identity,
    content: b"hi",
  };
  let read = Record {
    deepest: 3,
    moves,
    objects: vec![object],
  };
  assert_eq!(Record::decode(&bytes, FORMAT), Ok(read));
}

/// A-662 (the record written when an append commits, its bytes read back). Do: note three objects, the third longer
/// than three windows; write the record with the second one held by the repository by then; then write one naming an
/// object the content store does not hold. Expect: the record holds the first and the third in the order noted, the
/// long one's bytes whole from its windows, and not the second; the object not held refused `Unreadable`, never
/// written short.
#[test]
fn t_a662_a_record_holds_what_the_repository_lacks_when_written_and_refuses_bytes_not_held() {
  let mut content = ContentStore::new(BUDGET, WINDOW).expect("a content store");
  let long: Vec<u8> = (0..=u8::MAX).cycle().take(WINDOW * 3 + 5).collect();
  let mut writer = RecordWriter::default();
  for bytes in [&b"one"[..], b"two", &long] {
    let identity = content.put_object(bytes).expect("held");
    let (id, _) = blob(bytes);
    let len = u64::try_from(bytes.len()).expect("a length");
    writer.object(Noted {
      kind: Kind::Blob,
      id,
      identity,
      len,
    });
  }
  let (held, _) = blob(b"two");
  let bytes = writer
    .finish(&content, |id| *id != held, (0, &[]))
    .expect("written");
  let read = Record::decode(&bytes, FORMAT).expect("read back");
  let contents: Vec<&[u8]> = read.objects.iter().map(|object| object.content).collect();
  assert_eq!(contents, vec![&b"one"[..], &long[..]]);
  let (ghost, _) = blob(b"never held");
  let mut unheld = RecordWriter::default();
  unheld.object(Noted {
    kind: Kind::Blob,
    id: ghost,
    identity: ContentId([9; 32]),
    len: 10,
  });
  assert_eq!(
    unheld.finish(&content, |_| true, (0, &[])),
    Err(WriteRefused::Unreadable { id: ghost })
  );
}

/// A-662 (hostile input on a parser of bytes the log holds). Do: read a record cut at every length, one whose counts are
/// `u64::MAX`, one of another magic and another version, one with a marker of neither kind, one read in another format,
/// one naming a delta's type number, and one with a byte after its last object. Expect: each refused by its kind, never
/// a panic, and no count allocated past what the bytes could hold.
#[test]
fn t_a662_hostile_records_are_refused_by_kind() {
  let head = ObjectId::from_bytes(FORMAT, &[0xcd; 20]).expect("an id");
  let (content, noted) = holding(Kind::Commit, b"commit");
  let mut writer = RecordWriter::default();
  writer.object(noted);
  let bytes = writer
    .finish(&content, |_| true, (0, &[(1, 1, Some(head))]))
    .expect("written");
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
  let version = with(4..8, &2u32.to_le_bytes());
  assert_eq!(
    Record::decode(&version, FORMAT),
    Err(RecordRefused::Version { version: 2 })
  );
  // Format: the objects' count follows the header (24 bytes), the move's key, version and marker (17) and its head (20).
  let objects_at = 24 + 17 + 20;
  let objects_huge = with(objects_at..objects_at + 8, &u64::MAX.to_le_bytes());
  assert_eq!(
    Record::decode(&objects_huge, FORMAT),
    Err(RecordRefused::Count)
  );
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
  assert_eq!(
    Record::decode(&delta, FORMAT),
    Err(RecordRefused::Malformed)
  );
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

  /// Admits `objects` at `depth` as the live path does (`super::GitPush::admit_all`), for `moves` to commit later.
  fn begin(
    &mut self,
    objects: &[Brought],
    depth: u64,
    (moves, split): (Vec<Planned>, bool),
  ) -> Result<Pending, GitRefused> {
    let mut push = Push::new(FORMAT);
    let mut writer = RecordWriter::default();
    for brought in objects {
      let arriving = incoming(brought, depth);
      admit_object(
        &mut self.content,
        &self.store,
        (&mut push, &mut writer),
        arriving,
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
    let admitted = seal_moves(&self.content, &self.store, &mut pending.push, moves)?;
    let deepest = pending.push.deepest();
    let record = record_of(
      &self.content,
      &self.store,
      (&pending.writer, deepest),
      moves,
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
#[derive(Debug, PartialEq, Eq)]
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
/// record whose object's bytes were changed, one whose objects the store holds already (a record the live admission
/// would not have made), and one whose head names a commit nobody sent. Expect: refused `Identity`, `Diverged` and the
/// store's `Missing` in turn, and each refused replay leaves the store as it was.
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
  let missing = RecordWriter::default()
    .finish(&store.content, |_| true, (0, &[(1, 1, Some(nobody))]))
    .expect("written");
  assert!(matches!(
    replay_git(&mut store.content, &mut store.store, &missing),
    Err(ReplayRefused::Git(GitRefused::Store(_)))
  ));
  assert_eq!(store.facts(), before);
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

/// A-662 (one map, each tool's arm, made with the record and freed with it), with A-655 part 2b's Subversion records.
/// Do: create a git, a Mercurial and a Subversion repository; replay bytes that are no record into the Mercurial and
/// Subversion stores; take the Mercurial one's refusal as a recovery takes it; free all three. Expect: each store made
/// on the owner shard as its tool's arm, git's in its record's format, Subversion's records at revision 0 from its
/// creation's record; Mercurial's replay refused `Owed`, its replay being its own push piece's, and Subversion's
/// refused by the record's kind; the refused store given back and named unrecovered with why; after the frees no store
/// is held, none unrecovered, and nothing is charged.
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
    let refused = [
      (hg, ReplayRefused::Owed),
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

/// A-662 (the wire's words for a store's refusal). Do: map a store's budget refusal, a record whose room the allocator
/// refused, and a moved green. Expect: `BudgetExceeded` with the room the budget had, then with what the shard could
/// admit now, then `ContentUnavailable`, counted.
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
  crate::repository::push::push_sealed(
    state,
    (&owner(), repository),
    changes,
    EntryParts::default(),
    sealed,
  )
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
fn records(state: &ShardState, repository: RepositoryId) -> Vec<Vec<u8>> {
  state
    .db
    .partition()
    .repositories()
    .objects(repository)
    .map(<[u8]>::to_vec)
    .collect()
}

/// The store made again from the partition's log (`super::rebuild`, the recovery's own replay) equals the live one
/// whole over the refs keyed `greens`, every record replayed and none refused.
fn assert_made_again_whole(state: &mut ShardState, repository: RepositoryId, greens: &[u64]) {
  let live = {
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the store");
    facts(content, store, greens)
  };
  let count = u64::try_from(records(state, repository).len()).expect("a count");
  let replayed = state.refusals.get(super::REPLAYED).copied().unwrap_or(0);
  super::rebuild(state, repository);
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
  crate::daemon::audit_on_shard(|state| {
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
    let read = Record::decode(&last, FORMAT).map(|record| (record.deepest, record.objects.len()));
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
  crate::daemon::audit_on_shard(|state| {
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
    let read = Record::decode(&last, FORMAT).map(|record| (record.deepest, record.objects.len()));
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
    let fresh = work_of(
      state,
      (&push, root),
      (1, Some(first_tree)),
      (two, second_tree),
    );
    let mut sealed = push.seal(state).expect("sealed");
    let moves = [(Target::Green(root), Some(two))];
    let held = state.vcs_stores.content().map(ContentStore::used);
    let changes = vec![BranchChange::Update {
      green: root,
      work: stale,
    }];
    assert!(appended(state, repository, changes, (&mut sealed, &moves)).is_err());
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
    assert_eq!(read.objects.len(), second.len());
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
  let records = records(state, repository);
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
  crate::daemon::audit_on_shard(|state| {
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
