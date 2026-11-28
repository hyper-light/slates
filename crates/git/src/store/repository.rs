//! A git repository's index, versions and walks (A-614; §4.17.2 "The repository in git's terms", items 2 and 3; piece
//! 5, part 1): the store's module docs say what each holds and why.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::mem::size_of;

use slates_vcs::{ContentId, ContentRefused, ContentStore, Index, IndexRefused};

use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tag::Tag;
use crate::objects::tree::Tree;
use crate::{Admission, to_u64};

use super::encoding::Encoding;
use super::fragment::{self, FragmentRefused, Learning, Placed, Stored};
use super::hook_view::Quantum;
use super::learn::{Clock, Depth, Exchange, Levels, Rebuild, Window};
use super::naming::{GitNaming, kind_of};
use super::packed::{self, Base, Bases, InPack, Scratch};
use super::positions::Positions;
use super::push::{Push, Staged, StoredAt, stored_bytes};
use super::rows::{self, CommitRow, ObjectRow};
use reach::Sets;

// Objects by an abbreviated name (A-640 part 4), a child of this module so it reads the index kept here.
#[path = "abbrev.rs"]
pub(super) mod abbrev;

// The store read as upload-pack's commit graph (A-640), a child of this module so it reads the rows and chains kept
// here.
#[path = "graph.rs"]
pub(super) mod graph;

// A protected branch's questions about a move (A-644): how it changes its ref and what it brings, a child of this
// module so it reads the walks and chains kept here.
#[path = "protect.rs"]
pub(super) mod protect;

// A-614's D(k) and reached sets as a type of derived data (A-647 part 2), a child of this module so its derivation
// walks the store as the admission does.
#[path = "reach.rs"]
pub(super) mod reach;
// A push sealed and staged a unit at a time (A-722 part 4), a child of this module so it writes the rows kept here.
#[path = "staging.rs"]
pub(super) mod staging;

// A move admitted a unit at a time (A-722 part 4c), a child of this module so it walks the rows and chains kept here.
#[path = "admitting.rs"]
pub(super) mod admitting;
// A version's sets derived on first read (A-614 amended, main's ruling (A) of 2026-11-21), a child of this module so it
// walks the rows and chains kept here.
#[path = "deriving.rs"]
pub(super) mod deriving;

/// Shape: what one version's record costs the budget beside its set's runs. Derived from the type.
const VERSION_BYTES: usize = size_of::<Version>();

/// Shape: what one run of a set costs the budget. Derived from the type.
const RUN_BYTES: usize = size_of::<(u64, u64)>();

/// Shape: what one entry of the head map costs the budget, a position and a green. Derived from the type.
const HEAD_BYTES: usize = size_of::<(u64, u64)>();

/// Why the store refused. Each changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreRefused {
  /// An object a walk or a commit needs is in neither the push nor the repository.
  Missing {
    /// The object.
    id: ObjectId,
  },
  /// An object's bytes or a row it names are not in the store as its row says: the store and the index disagree.
  Unreadable {
    /// The object.
    id: ObjectId,
  },
  /// A tree or a tag git's parser refuses where the store must read what it names.
  Malformed {
    /// The object.
    id: ObjectId,
  },
  /// A commit's parent is no commit.
  NotACommit {
    /// The object.
    id: ObjectId,
  },
  /// A move was admitted before its push was sealed.
  Unsealed,
  /// A move's green is no longer where it was when the move was admitted, or two moves name it.
  Moved {
    /// The green.
    green: u64,
  },
  /// A commit moves no ref.
  NoMove,
  /// Every position is given.
  PositionsExhausted,
  /// A push made for a repository of another object format.
  ForeignPush,
  /// A green has no such version.
  NoVersion {
    /// The green.
    green: u64,
    /// The version asked for.
    version: u64,
  },
  /// A version's sets, its value of the reach sets (A-647, [`super::ReachSets`]), do not read as the type's.
  SetsUnreadable {
    /// The green.
    green: u64,
    /// The version.
    version: u64,
  },
  /// A version admitted with no walk (A-614 amended, main's ruling (A) of 2026-11-21) whose sets are not derived yet:
  /// its reader derives them first ([`Repository::deriving`]), a slice at a time, and asks again.
  Deferred {
    /// The green.
    green: u64,
    /// The version.
    version: u64,
  },
  /// A pack asked for a version whose fragments are not encoded yet.
  NotEncoded {
    /// The version.
    version: u64,
  },
  /// A fragment or a base pack could not be encoded.
  Encoding(FragmentRefused),
  /// A pack of more entries than its header can count.
  PackTooLarge,
  /// A kept fragment's bytes are not in the store.
  FragmentUnreadable,
  /// The pack's trailer could not be made (SHA-1's collision detection refused it).
  TrailerRefused,
  /// A replayed move's recorded base (A-850, [`Repository::based`]) is not a version this repository could have made
  /// it at: no such version of another green, one whose head is neither the move's head nor the commit it peels to, or
  /// a base named for a move admitted otherwise than remade with no walk.
  Base {
    /// The base's green.
    green: u64,
    /// The base's version.
    version: u64,
  },
  /// An answer named a position the repository never gave (A-639).
  NoPosition {
    /// The position.
    position: u64,
  },
  /// A green's base pack was made again while a stream read it entry by entry (A-639): the owner makes base packs
  /// between streams.
  BaseRebuilt {
    /// The green.
    green: u64,
  },
  /// A stream sent other than the objects its header counted, its repository changed under it (A-639).
  CountChanged {
    /// The objects the header counted.
    counted: u64,
    /// The entries sent.
    sent: u64,
  },
  /// The index refused the rows.
  Index(IndexRefused),
  /// The store refused (the budget).
  Content(ContentRefused),
}

impl From<IndexRefused> for StoreRefused {
  fn from(refused: IndexRefused) -> StoreRefused {
    StoreRefused::Index(refused)
  }
}

impl From<ContentRefused> for StoreRefused {
  fn from(refused: ContentRefused) -> StoreRefused {
    StoreRefused::Content(refused)
  }
}

/// What the store has done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreCounters {
  /// Positions given.
  pub positions: u64,
  /// Objects the walks visited that their green had not reached.
  pub walked: u64,
  /// Objects the walks met already reached, where each stopped.
  pub reached_met: u64,
  /// Of those, trees: each skipped whole with everything it names.
  pub trees_skipped: u64,
  /// Moves found to be fast-forwards.
  pub fast_forwards: u64,
  /// Fast-forward questions decided with no walk (A-644): the two heads one commit, or the new head's generation number
  /// no greater than the old head's, which no descendant's is.
  pub decided_without_walk: u64,
  /// Fast-forward questions decided by a walk back from the new head (A-644).
  pub descent_walks: u64,
  /// Commits a protected branch's checks judged: each commit a move brings, once (A-644).
  pub brought_judged: u64,
  /// Reached sets made again (a create, a rewrite).
  pub remade: u64,
  /// Of those, the ones that started from another green's set.
  pub bases_found: u64,
  /// Commits a create's or a rewrite's search back for its base met at its admission and passed (A-850): the search's
  /// non-vacuity counter, which grows with a push's history, not with its tags times its history.
  pub base_searched: u64,
  /// Moves admitted with no walk, their versions' sets the closure of their heads, derived on first read (A-614
  /// amended, main's ruling (A)).
  pub deferred: u64,
  /// Versions whose sets a reader derived ([`Repository::derive_turn`]), each once while its memo is kept.
  pub derived: u64,
  /// Versions [`Repository::derivations_due`] looked at to answer: what a derivation's every turn asks before it begins
  /// one (main 01a20182's sample of 2026-11-25: the question took a third of a git/git encoding's time behind a push).
  pub due_looked: u64,
  /// Bytes the versions' values added to the content store as their commits put them: what the store held new, never
  /// what it held already and was held once more by (A-647 part 4).
  pub value_bytes: u64,
  /// Fragments encoded.
  pub fragments: u64,
  /// Entries written whole.
  pub whole: u64,
  /// Entries written as a delta against an earlier entry.
  pub offset_deltas: u64,
  /// Entries written as a delta against a named object.
  pub reference_deltas: u64,
  /// Base packs made.
  pub rebuilds: u64,
  /// Pushes a commit staged again whole, their stage overtaken by a landing since (A-722 part 4e: a push's judge
  /// stages it again a turn at a time before its append, [`Repository::needs_stage`], so a commit meets none).
  pub restaged: u64,
  /// Chain records and head-map entries the commits read to make and charge their versions (P2, CLAUDE.md §3 bounded
  /// work): each commit's own moves' worth, whatever the repository's other greens and their histories hold.
  pub commit_walked: u64,
  /// Positions mapped to the move that brought them (the landing map's runs, [`Repository::bringer_of`]): by a commit's
  /// step only for one move's landing, whose one run maps them all at once; a landing of several moves is mapped a
  /// position a unit by [`Repository::settle_landings_turn`]'s turns, never in its step (P2 piece 2).
  pub landing_mapped: u64,
}

/// The entry a path names in a version's tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
  /// Its mode as git wrote it.
  pub mode: u16,
  /// Its object.
  pub id: ObjectId,
}

/// One version of a ref: its head's position (`None` for a deleted ref), its value of the reach sets, and how its
/// reached set followed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
  head: Option<u64>,
  /// Its value of [`super::ReachSets`] (A-647 part 3), held once by the record: the set its head reaches and D(k), what
  /// it brought, which the store reads from it. None for a version admitted with no walk (A-614 amended, main's ruling
  /// (A) of 2026-11-21): its sets are the closure of its head, derived on first read and kept in the memo
  /// ([`Repository::deriving`]).
  value: Option<ContentId>,
  /// Its D(k) encoded, once the owner asked (A-614 part 2).
  fragments: Option<Fragments>,
  /// How its reached set followed, kept so its derivation (A-647 part 2, [`reach`]) reads only its parents' values.
  followed: Followed,
}

/// How a version's reached set followed (A-647 part 2): the move's [`Reach`], with the version a remade set was taken
/// from named, since which green's head a rewrite or a create descends from is found among the greens' heads at its
/// admission, and a derivation reads only recorded history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Followed {
  /// A fast-forward: the green's previous set grown by what the version brought.
  Grown,
  /// A create or a rewrite: made again from the set of `base` (a green and its version when the move was admitted), or
  /// from nothing.
  Remade { base: Option<(u64, usize)> },
  /// A delete: nothing reached.
  Emptied,
}

/// Shape: what one fragment entry's record costs the budget. Derived from the type.
const PLACED_BYTES: usize = size_of::<Placed>();

/// Shape: what one entry's place in its part's position order costs the budget (A-653). Derived from the type.
const ORDER_BYTES: usize = size_of::<u32>();

/// One fragment, or a part of a base pack, kept: its bytes as content, its length, its entries; and
/// read from them once (A-639), the positions of its objects, the positions its reference deltas'
/// bases have, and how many of its entries are offset deltas; and its entries' indexes in their
/// objects' position order (A-653), so a stream finds the entries of the objects it wants by
/// halving, whatever the part's size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Kept {
  pub(super) content: ContentId,
  pub(super) len: u64,
  pub(super) placed: Vec<Placed>,
  pub(super) positions: Positions,
  pub(super) references: Positions,
  pub(super) offsets: u64,
  pub(super) order: Vec<u32>,
  /// The runs `positions` and `references` hold together, counted once as the part is kept: what its sets cost the
  /// budget, read for every kept part each time a repository's holding is weighed (main 01a20182's r100: git/git's work
  /// clone spent 85% of its shard counting them again, a set's runs walked whole each turn of its encoding ahead).
  pub(super) runs: usize,
  /// The bytes the store holds under its content, its whole tree of windows and lists (`ContentStore::tree_bytes`), read
  /// once as it is kept, since a kept part's content never changes: what weighing a repository's kept fragments reads
  /// ([`Repository::kept_fragments`]). It read `len_of` from the store's map for every kept part on every weighing (gate
  /// r935: 2,270 of a clone's 24,971 samples), and that was the root node's length alone, so a fragment kept as a tree
  /// was weighed at a few dozen bytes (main 01a20182's ruling of 2026-11-24 10:56).
  pub(super) held: u64,
}

/// A version's two fragments, or a base pack's two parts: commits, trees and tags, then blobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fragments {
  pub(super) objects: Kept,
  pub(super) blobs: Kept,
}

/// A green's base pack: every object its head reached at `version`, self-contained, as two parts
/// (A-639), so a fetch that wants no blobs takes the first whole.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BasePack {
  version: usize,
  parts: Fragments,
}

/// A ref's chain of versions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Chain {
  versions: Vec<Version>,
  base: Option<BasePack>,
  rebuild: Rebuild,
}

impl Chain {
  fn head(&self) -> Option<u64> {
    self.versions.last().and_then(|version| version.head)
  }

  /// The index of its newest version.
  fn newest(&self) -> Option<usize> {
    self.versions.len().checked_sub(1)
  }

  /// What the chain charges the budget: its records and its kept parts' runs. Its versions' sets are their values'
  /// bytes, which the content store charges as it keeps them (A-647 part 3).
  fn footprint(&self) -> u64 {
    let runs = self
      .kept()
      .fold(0_usize, |runs, kept| runs.saturating_add(kept.runs));
    let records = bytes(self.versions.len(), VERSION_BYTES).saturating_add(bytes(runs, RUN_BYTES));
    records.saturating_add(bytes(
      self.placed(),
      PLACED_BYTES.saturating_add(ORDER_BYTES),
    ))
  }

  /// Every kept fragment's and the base pack's entries.
  fn placed(&self) -> usize {
    self.kept().fold(0_usize, |total, kept| {
      total.saturating_add(kept.placed.len())
    })
  }

  /// Every fragment kept and the base pack.
  fn kept(&self) -> impl Iterator<Item = &Kept> + '_ {
    self
      .versions
      .iter()
      .filter_map(|version| version.fragments.as_ref())
      .chain(self.base.as_ref().map(|base| &base.parts))
      .flat_map(|fragments| [&fragments.objects, &fragments.blobs])
  }
}

/// A point after a commit's plan where it may be refused ([`Repository::commit`]): its versions' values put, its
/// records charged, its landing map charged, its stage adopted (refused as stale, `Repository::adopted`). Named so the
/// store's oracle refuses a commit at each and holds the store to what it was (P2, main 01a20182's condition of
/// 2026-11-27 19:22).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CommitPoint {
  /// The versions' values put ([`put_values`]).
  Values,
  /// The records' difference charged: the versions, the head map's entries and the maps' bound.
  Records,
  /// The landing map's runs charged.
  Landings,
  /// The stage's fork adopted into the index: refused only by the test's hook, since a current fork's adoption charges
  /// nothing a budget could refuse ([`Repository::adopted`]).
  #[cfg(test)]
  Adopt,
}

impl CommitPoint {
  /// Every point, in the commit's order.
  #[cfg(test)]
  pub(crate) const ALL: [CommitPoint; 4] = [
    CommitPoint::Values,
    CommitPoint::Records,
    CommitPoint::Landings,
    CommitPoint::Adopt,
  ];
}

// The tests' switch, kept on the thread that runs the commit.
#[cfg(test)]
thread_local! {
  /// The point a test asks the next commits to be refused at ([`at_point`]); none: no commit is.
  pub(crate) static REFUSE_COMMIT_AT: Cell<Option<CommitPoint>> = const { Cell::new(None) };
  /// The points a commit was refused at since the test last took them, so a test that asks knows each refusal happened.
  pub(crate) static COMMIT_REFUSED_AT: RefCell<BTreeSet<CommitPoint>> = const { RefCell::new(BTreeSet::new()) };
}

/// A commit's `step` at `point`, as it is; in a test that asks to refuse there ([`REFUSE_COMMIT_AT`]), with the
/// store's budget held to what it holds for the step, so whatever the step charges is refused as a full store refuses
/// it.
#[cfg(test)]
fn at_point<T, E>(
  store: &mut ContentStore,
  point: CommitPoint,
  step: impl FnOnce(&mut ContentStore) -> Result<T, E>,
) -> Result<T, E> {
  if REFUSE_COMMIT_AT.with(Cell::get) != Some(point) {
    return step(store);
  }
  let budget = store.budget();
  store.set_budget(store.used());
  let answer = step(store);
  store.set_budget(budget);
  if answer.is_err() {
    COMMIT_REFUSED_AT.with(|refused| refused.borrow_mut().insert(point));
  }
  answer
}

/// A commit's `step` at `point`, as it is (the tests' twin refuses there on asking).
#[cfg(not(test))]
fn at_point<T, E>(
  store: &mut ContentStore,
  _point: CommitPoint,
  step: impl FnOnce(&mut ContentStore) -> Result<T, E>,
) -> Result<T, E> {
  step(store)
}

/// `count` records of `each` bytes.
fn bytes(count: usize, each: usize) -> u64 {
  u64::try_from(count.saturating_mul(each)).unwrap_or(u64::MAX)
}

/// The objects a walk met that its green had not reached: those indexed by position, those new to the repository by
/// id; and, where the walk met every new one while the push's stage was current, the positions that stage gives them
/// (`staged`), which a commit adopting that stage takes rather than looking each id up in its step.
#[derive(Clone, Debug, Default)]
struct Members {
  indexed: Positions,
  new: Vec<ObjectId>,
  staged: Option<StagedAt>,
}

/// What a walk is: what it met. The positions it found staged are how it was met, not what (`Members::staged`).
impl PartialEq for Members {
  fn eq(&self, other: &Members) -> bool {
    self.indexed == other.indexed && self.new == other.new
  }
}

impl Eq for Members {}

/// The positions a stage gives a walk's new objects, and that stage: its base and how many positions it gives. A
/// stage made again at the same base gives the same positions, since no landing since gave any (so none of the push's
/// objects was indexed meanwhile and its fresh set is the same).
#[derive(Clone, Debug)]
struct StagedAt {
  stage: (u64, usize),
  positions: Positions,
}

/// How a move's reached set follows from its green's.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reach {
  /// A fast-forward over a version whose sets are known: the set grows by what the walk found the version brought.
  Grown,
  /// A fast-forward over a version admitted with no walk: none now either, its sets derived on first read (A-614
  /// amended).
  GrownUnwalked,
  /// A create or a rewrite, admitted with no walk (A-614 amended, main's ruling (A) of 2026-11-21: a create walked
  /// everything its head reached that no green's head did, so git/git's whole history, its 1,027 refs mostly tags at old
  /// commits, was decided in time about refs times history): its set the closure of its head, derived on first read,
  /// from `from` (a green and its version whose head is the move's own head, found with no walk) when one is.
  Remade { from: Option<(u64, usize)> },
  /// A delete: nothing reached.
  Emptied,
}

/// One ref's move, admitted before its verdict ([`Repository::admit`]): what its version brings and how its reached set
/// follows, read from the push and the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
  green: u64,
  from: Option<u64>,
  to: Option<ObjectId>,
  /// The commit `to` peels to, where it peels to one: what the commit notes the version under, a base a later create's
  /// search back may meet (A-850, the repository's peeled map).
  peeled: Option<ObjectId>,
  brought: Members,
  reach: Reach,
}

impl Move {
  /// The green it moves.
  pub fn green(&self) -> u64 {
    self.green
  }

  /// Whether it is a fast-forward of its green.
  pub fn is_fast_forward(&self) -> bool {
    matches!(self.reach, Reach::Grown | Reach::GrownUnwalked)
  }

  /// The position of its green's head when it was admitted; none for a green with no head.
  pub fn from_head(&self) -> Option<u64> {
    self.from
  }

  /// The object it moves its green to; none for a delete.
  pub fn to(&self) -> Option<ObjectId> {
    self.to
  }

  /// The version it was made at (A-850): for a create or a rewrite admitted with no walk, the green and its version
  /// whose head is the move's own head or the commit its tags peel to, found at its admission, which its sets are
  /// relative to and which a push's record names so that a replay takes the same ([`Repository::based`]); none for any
  /// other move.
  pub fn base(&self) -> Option<(u64, u64)> {
    match self.reach {
      Reach::Remade {
        from: Some((green, version)),
      } => Some((green, to_u64(version))),
      _ => None,
    }
  }

  /// The same move of `green`: a move admitted for a ref before its append named the green that holds it (A-722 part
  /// 4c: a create's green is made by its append), taken by that green while its head is still this move's `from`.
  pub fn of_green(mut self, green: u64) -> Move {
    self.green = green;
    self
  }
}

/// Where an object is: indexed, with its row, or held by the push.
enum Place<'p> {
  Indexed {
    row: ObjectRow,
    content: ContentId,
  },
  /// Held by `push`, read through it: from the content store or where it lies in the push's pack.
  Pushed {
    kind: Kind,
    push: &'p Push,
  },
}

impl Place<'_> {
  fn kind(&self) -> Kind {
    match self {
      Place::Indexed { row, .. } => row.kind,
      Place::Pushed { kind, .. } => *kind,
    }
  }
}

/// The repository's index as the bases a pack's REF_DELTA names (`packed::Bases`): each object written whole from the
/// content store.
pub struct IndexBases<'a> {
  /// The repository.
  pub repository: &'a Repository,
  /// The store its objects are in.
  pub store: &'a ContentStore,
}

impl Bases for IndexBases<'_> {
  fn locate(&self, id: &ObjectId, whole: &mut Vec<u8>, room: u64) -> Option<Base> {
    let (row, content) = self.repository.indexed(self.store, id)?;
    if row.len > room {
      return None;
    }
    // An object read from a pack is followed into its pack, its chain read there; one held whole is written out.
    match self.repository.stored_of(&row, content) {
      StoredAt::InPack(place) => Some(Base::InPack(place)),
      StoredAt::Whole(content) => {
        let bytes = stored_bytes(self.store, &content, row.len)?;
        whole.clear();
        whole.extend_from_slice(&bytes);
        Some(Base::Whole(row.kind))
      }
    }
  }
}

/// What [`Repository::widen_read_cache`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Widening {
  /// The cache already kept enough: nothing charged.
  Enough,
  /// It was widened, the growth charged.
  Grown,
  /// Its admission had no room for the growth: it stays as it is.
  Refused,
}

/// What a repository holds outside the content store ([`Repository::held`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepositoryHeld {
  /// Its read scratch's buffers' room.
  pub scratch: u64,
  /// The bytes of the objects it read lately.
  pub recent: u64,
  /// The entries of its in-memory maps.
  pub map_entries: u64,
  /// What it charged the content store.
  pub charged: u64,
}

/// A repository's records a commit changes, and the bytes its store holds ([`Repository::commit_state`]).
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct CommitState {
  next_position: u64,
  chains: BTreeMap<u64, Chain>,
  unvalued: u64,
  heads: HeadMap,
  peeled: BTreeMap<u64, (u64, usize)>,
  holders: BTreeMap<u64, BTreeMap<u64, usize>>,
  holder_entries: usize,
  charged: u64,
  chains_charged: u64,
  head_entries: usize,
  largest_pack: u64,
  landings: BTreeMap<u64, Landed>,
  landings_charged: u64,
  unsettled: VecDeque<u64>,
  kinds: BTreeMap<u64, (u64, Kind)>,
  packs: BTreeMap<ContentId, u64>,
  used: u64,
}

/// A git repository's store (module docs).
#[derive(Debug)]
pub struct Repository {
  format: ObjectFormat,
  index: Index,
  next_position: u64,
  chains: BTreeMap<u64, Chain>,
  /// The versions its chains hold with no value of their own (each left for its first reader, A-614 amended), counted
  /// as [`Repository::commit`] adds them: a version's value is never set after its commit, and the memo holds sets for
  /// such versions alone, so the versions due are this count less the memo's ([`Repository::derivations_due`]).
  unvalued: u64,
  /// For each commit some green's current head names, by its position, those greens.
  heads: BTreeMap<u64, BTreeSet<u64>>,
  /// For each commit a committed version's head peels to, by its position, the first such version (A-850): what a
  /// create's or a rewrite's search back at its admission meets as its base. Made by the commits alone, in the log's
  /// order, so a store made again from the log holds the same.
  peeled: BTreeMap<u64, (u64, usize)>,
  /// For each commit some version's head names, by its position, each green with such a version and its newest one
  /// (goal 93): what a push's search for the green holding a commit reads ([`Repository::holders`]), so what it costs
  /// follows the greens holding that commit, never the repository's greens. Added to as each version commits; versions
  /// are never taken out, so neither are its entries.
  holders: BTreeMap<u64, BTreeMap<u64, usize>>,
  /// The entries `holders` holds, green by green: what it charges the store.
  holder_entries: usize,
  /// What the chains, the head map, the peeled map and the holders' map charge the store.
  charged: u64,
  /// The chains' footprints summed ([`Chain::footprint`]), kept as each changes, so a commit or a recharge charges by
  /// the changed chain's difference and never sums every chain again (P2).
  chains_charged: u64,
  /// The entries the head map holds, green by green, kept as each head moves.
  head_entries: usize,
  /// The version that brought each position, as runs: each run's first position to its end (exclusive), the green and
  /// that green's version (A-722: kept beside the rows, which no longer carry it, so a push can write its rows before it
  /// lands).
  /// The packs its rows name (A-722's record v4: an object a push brought is read where it lies in the push's pack,
  /// held once as content), each one's length by its identity: what a row's offset is read against. One entry a
  /// landing that brought a pack.
  packs: BTreeMap<ContentId, u64>,
  /// The buffers its objects are read from their packs with, and the objects read lately (`packed::Scratch`), held to
  /// its largest pack's length; in a cell because a read is a look, not a change.
  reading: RefCell<Scratch>,
  /// The deltas its last read of a stored object applied (`Scratch::deltas_applied`; none for one held whole): what a
  /// tree's open costs in the store, read by the fetch's walk ([`Repository::last_read_deltas`]).
  last_deltas: std::cell::Cell<u64>,
  /// Whether its last read of a stored object found an entry of its chain kept ([`Repository::last_read_found_kept`]).
  last_found_kept: std::cell::Cell<bool>,
  /// Whether its last read of a stored object inflated its chain's root whole ([`Repository::last_read_inflated_base`]).
  last_inflated_base: std::cell::Cell<bool>,
  /// The first let-go piece a read met since its owner last asked ([`Repository::waiting`]): a read that meets one answers
  /// none, and its owner brings the piece back and takes the step again.
  waiting: Cell<Option<packed::Waited>>,
  /// While a reader with a slot of its own reads ([`Repository::reading_quietly`]): the first let-go its reads met,
  /// kept here and never on [`Repository::waiting`]'s shared slot. None outside such a read.
  quiet: Cell<Option<Option<packed::Waited>>>,
  /// The longest of its packs (`packs`' greatest length), kept as each lands: what its read-lately budget follows, read
  /// with no walk over the packs, one a landing (P2).
  largest_pack: u64,
  /// The longest object its packs hold: what a read from them is held to (§4.9: every size a stored entry's header
  /// names, and its chain's, checked against it before a buffer grows to it; a read allocates nothing in the store, so
  /// the store's lent room is no bound for it).
  largest: u64,
  /// Each landing's runs of positions by type, by first position: `start -> (end, kind)` (A-639: a landing gives its
  /// objects positions by type, then id, so each type's are one run). A position's type is read here, with no row read
  /// (`kind_at`; A-722 part 3's partial clone read two rows an object for it). A run a type each landing brought.
  kinds: BTreeMap<u64, (u64, Kind)>,
  landings: BTreeMap<u64, Landed>,
  /// What the landing map charges the store.
  landings_charged: u64,
  /// The first positions of the landings of several moves still to be mapped by position, oldest first
  /// ([`Repository::settle_landings_turn`]).
  unsettled: VecDeque<u64>,
  /// The oldest of them's mapping, under way.
  settling: Option<Settling>,
  counters: Cell<StoreCounters>,
  window: Window,
  levels: Levels,
  depth: Depth,
  /// The sets derived for versions admitted with no walk (A-614 amended), each held once by the memo, and for each
  /// commit a version whose sets are known peels to, that version: what a derivation searches back for its base. In a
  /// cell because a derivation's reader holds the repository to read (a fork a fetch serves from); a cache of values
  /// any tier derives alike, so it is never recorded and a restart starts it empty.
  memo: RefCell<Memo>,
}

/// The derived sets a repository keeps ([`Repository::memo`]).
#[derive(Clone, Debug, Default)]
pub(super) struct Memo {
  /// Each derived version's value, held once by the memo.
  pub(super) values: BTreeMap<(u64, usize), ContentId>,
  /// For each commit's position, a version whose head peels to it and whose sets are known (its value or the memo's).
  pub(super) at: BTreeMap<u64, (u64, usize)>,
}

impl Repository {
  /// An empty repository of `format`.
  pub fn new(format: ObjectFormat) -> Repository {
    Repository {
      format,
      index: Index::new(),
      next_position: 0,
      chains: BTreeMap::new(),
      unvalued: 0,
      heads: BTreeMap::new(),
      peeled: BTreeMap::new(),
      holders: BTreeMap::new(),
      holder_entries: 0,
      charged: 0,
      chains_charged: 0,
      head_entries: 0,
      largest_pack: 0,
      landings: BTreeMap::new(),
      kinds: BTreeMap::new(),
      packs: BTreeMap::new(),
      reading: RefCell::new(Scratch::default()),
      last_deltas: std::cell::Cell::new(0),
      last_found_kept: std::cell::Cell::new(false),
      last_inflated_base: std::cell::Cell::new(false),
      waiting: Cell::new(None),
      quiet: Cell::new(None),
      largest: 0,
      landings_charged: 0,
      unsettled: VecDeque::new(),
      settling: None,
      counters: Cell::new(StoreCounters::default()),
      window: Window::default(),
      levels: Levels::default(),
      depth: Depth::default(),
      memo: RefCell::new(Memo::default()),
    }
  }

  /// The repository's object format.
  pub fn format(&self) -> ObjectFormat {
    self.format
  }

  /// The green and that green's version that brought the object at `position` (A-722: the landing map's, the rows no
  /// longer carry it); `None` for a position no landing gave.
  pub fn bringer_of(&self, position: u64) -> Option<(u64, u64)> {
    let (base, landed) = self.landings.range(..=position).next_back()?;
    landed.bringer((*base, position))
  }

  /// git's naming in the repository's format.
  pub fn naming(&self) -> GitNaming {
    GitNaming {
      format: self.format,
    }
  }

  /// Refuses a push made for a repository of another format.
  fn same_format(&self, push: &Push) -> Result<(), StoreRefused> {
    if push.format() != self.format {
      return Err(StoreRefused::ForeignPush);
    }
    Ok(())
  }

  /// What the store has done.
  pub fn counters(&self) -> StoreCounters {
    self.counters.get()
  }

  /// The deepest delta chain the store's encoder makes: the deepest any commit's push carried, at most git's limit
  /// (A-614 part 2's learned depth). What a store made again from its pushes' records must learn alike (A-662).
  pub fn learned_depth(&self) -> u64 {
    self.depth.limit()
  }

  /// Widens what its reads keep of the objects read lately (`packed::Recent`, git's delta base cache) for a walk that
  /// opens up to `breadth` bytes of trees for one commit (the most one sent commit's trees came to so far, which may
  /// grow past the first commit's, since a later commit the walk reads can open more): to the entries of a chain as
  /// deep as its learned depth ([`Repository::learned_depth`], its deltas and its base) times `breadth`, so every
  /// tree's chain the walk reads between two revisions of one tree stays whole, each revision no longer than the tree
  /// it leads to. The growth alone is charged to `admission`, a fetch's own (main 01a20182's ruling of 2026-11-25: a
  /// fork a fetch reads through kept no cache, so every tree read rebuilt its chain from its base, 78 deltas for a
  /// 13-tree chain). Sized for one tree's chain alone, git/git's clone hit it 6,702 times and applied 2,004,133 deltas
  /// (main's leg at 47de9566d): the directories read between two of one directory's revisions put its chain out.
  /// Refused room, it stays as it is.
  pub fn widen_read_cache(&self, breadth: u64, admission: &mut Admission) -> Widening {
    let wanted = super::packed::Recent::budget_keeping(
      self
        .learned_depth()
        .saturating_add(1)
        .saturating_mul(breadth),
    );
    let mut reading = self.reading.borrow_mut();
    let have = to_u64(reading.recent().budget());
    if wanted <= have {
      return Widening::Enough;
    }
    if admission.charge(wanted.saturating_sub(have)).is_err() {
      return Widening::Refused;
    }
    reading
      .recent()
      .set_budget(usize::try_from(wanted).unwrap_or(usize::MAX));
    Widening::Grown
  }

  /// The bytes its reads keep of the objects read lately (`packed::Recent`, git's delta base cache).
  pub fn recent_bytes(&self) -> u64 {
    to_u64(self.reading.borrow_mut().recent().bytes())
  }

  /// Gives back the objects read lately and its read buffers' room (A-860: what a repository gone idle holds of its
  /// reads is given back, its cache's budget kept for the reads after), and what `store` was charged for them (A-861): the
  /// bytes given back.
  pub fn let_go_recent(&self, store: &mut ContentStore) -> u64 {
    let mut reading = self.reading.borrow_mut();
    let kept = to_u64(reading.recent().bytes());
    reading.recent().release(store);
    reading.shrink(0);
    kept
  }

  /// Charges `store` for the objects read lately as they stand ([`packed::Recent::true_up`], A-861): what a turn that
  /// reads the repository's objects runs once it holds the store mutably.
  pub fn true_up_recent(&self, store: &mut ContentStore) {
    self.reading.borrow_mut().recent().true_up(store);
  }

  /// The longest object its packs hold (A-859): what reading a stored entry's data is held to.
  pub(super) fn largest_object(&self) -> u64 {
    self.largest
  }

  fn count(&self, change: impl FnOnce(&mut StoreCounters)) {
    let mut counters = self.counters.get();
    change(&mut counters);
    self.counters.set(counters);
  }

  /// The row of the object `id` names, and its root content.
  pub(crate) fn indexed(
    &self,
    store: &ContentStore,
    id: &ObjectId,
  ) -> Option<(ObjectRow, ContentId)> {
    let found = self.index.get(store, &rows::object_key(id))?;
    let content = found.contents().next()?;
    Some((rows::decode(found.payload())?, content))
  }

  /// The row of the object `id` names.
  pub fn object(&self, store: &ContentStore, id: &ObjectId) -> Option<ObjectRow> {
    self.indexed(store, id).map(|(row, _)| row)
  }

  /// The type, place and length of the object `id` the index holds: where its bytes are read from (A-753: a pack
  /// resending an object the repository holds is read back from the index, its quarantine holding none).
  pub fn stored(&self, store: &ContentStore, id: &ObjectId) -> Option<(Kind, StoredAt, u64)> {
    let (row, content) = self.indexed(store, id)?;
    Some((row.kind, self.stored_of(&row, content), row.len))
  }

  /// Where the object whose row is `row`, naming `content`, is: at its offset in the pack the row names, for an object
  /// read from a pack; its own content otherwise.
  pub fn stored_of(&self, row: &ObjectRow, content: ContentId) -> StoredAt {
    match row.pack {
      Some(packed) => StoredAt::InPack(InPack {
        pack: content,
        len: self.packs.get(&content).copied().unwrap_or(0),
        offset: packed.offset,
      }),
      None => StoredAt::Whole(content),
    }
  }

  /// The bytes of the object at `stored`, `len` long: lent where the store holds it whole as one window, else
  /// assembled; read where it lies in its pack otherwise (`packed::read`, its chain from the objects read lately, a
  /// REF_DELTA's base followed through the index), the reader's buffer handed over.
  pub fn read_stored<'s>(
    &self,
    store: &'s ContentStore,
    stored: &StoredAt,
    len: u64,
  ) -> Option<Cow<'s, [u8]>> {
    self
      .read_stored_waited(store, stored, len)
      .map_err(|waited| {
        if let Some(waited) = waited {
          self.wait_for(waited);
        }
      })
      .ok()
  }

  /// [`Repository::read_stored`] noting nothing: the bytes, or the let-go window the read met (none for any other
  /// refusal), for a reader with a slot of its own to note it there alone (`crate::away::Waits`).
  pub fn read_stored_waited<'s>(
    &self,
    store: &'s ContentStore,
    stored: &StoredAt,
    len: u64,
  ) -> Result<Cow<'s, [u8]>, Option<packed::Waited>> {
    self.last_deltas.set(0);
    self.last_found_kept.set(false);
    self.last_inflated_base.set(false);
    match stored {
      StoredAt::Whole(content) => stored_bytes(store, content, len).ok_or(None),
      StoredAt::InPack(place) => {
        let bases = IndexBases {
          repository: self,
          store,
        };
        let mut out = Vec::new();
        let mut reading = self.reading.borrow_mut();
        let read = packed::read(
          store,
          (*place, self.format),
          &bases,
          (&mut *reading, self.largest.max(len)),
          &mut out,
        );
        self
          .last_deltas
          .set(u64::try_from(reading.deltas_applied()).unwrap_or(u64::MAX));
        self.last_found_kept.set(reading.found_kept());
        self.last_inflated_base.set(reading.base_inflated());
        read.map_err(|refused| refused.waited())?;
        (u64::try_from(out.len()).ok() == Some(len))
          .then_some(Cow::Owned(out))
          .ok_or(None)
      }
    }
  }

  /// Holds what the repository charged its store to the whole recomputation over every chain, the head map, the peeled
  /// map and the holders' map (P2's oracle: a commit charges by difference).
  #[cfg(test)]
  pub(crate) fn assert_charged_whole(&self) {
    let (chains, entries) = charged_whole(&self.chains, &self.heads);
    assert_eq!(
      (self.chains_charged, self.head_entries),
      (chains, entries),
      "the chains' charge and the head map's entries kept against the whole recomputation"
    );
    let whole = charge_of(chains, (entries, self.peeled.len(), self.holder_entries));
    assert_eq!(
      self.charged, whole,
      "the charge kept against the whole recomputation"
    );
  }

  /// What a refused commit leaves as it was (P2's oracle): every record a commit changes, and what the store holds.
  #[cfg(test)]
  pub(crate) fn commit_state(&self, store: &ContentStore) -> CommitState {
    CommitState {
      next_position: self.next_position,
      chains: self.chains.clone(),
      unvalued: self.unvalued,
      heads: self.heads.clone(),
      peeled: self.peeled.clone(),
      holders: self.holders.clone(),
      holder_entries: self.holder_entries,
      charged: self.charged,
      chains_charged: self.chains_charged,
      head_entries: self.head_entries,
      largest_pack: self.largest_pack,
      landings: self.landings.clone(),
      landings_charged: self.landings_charged,
      unsettled: self.unsettled.clone(),
      kinds: self.kinds.clone(),
      packs: self.packs.clone(),
      used: store.used(),
    }
  }

  /// What the repository holds outside the content store (the memory status's gauges): its read scratch's buffers' room
  /// and objects read lately ([`packed::Scratch::held`]), and the entries of its in-memory maps (chains, heads, peeled
  /// tags, kinds and landings).
  pub fn held(&self) -> RepositoryHeld {
    let (scratch, recent) = self.reading.borrow().held();
    let entries = [
      self.chains.len(),
      self.heads.len(),
      self.peeled.len(),
      self.holder_entries,
      self.kinds.len(),
      self.landings.len(),
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add);
    RepositoryHeld {
      scratch,
      recent,
      map_entries: u64::try_from(entries).unwrap_or(u64::MAX),
      charged: self.charged,
    }
  }

  /// The object the entry at `place` of a held pack resolves to, whatever its length (A-859: a stored delta's base, read
  /// to name it and to check the delta against it): its type and bytes; none where it is not read.
  pub(super) fn read_entry(&self, store: &ContentStore, place: InPack) -> Option<(Kind, Vec<u8>)> {
    let bases = IndexBases {
      repository: self,
      store,
    };
    let mut out = Vec::new();
    let mut reading = self.reading.borrow_mut();
    let read = packed::read(
      store,
      (place, self.format),
      &bases,
      (&mut *reading, self.largest),
      &mut out,
    );
    self.note_waited(&read);
    Some((read.ok()?, out))
  }

  /// Notes the let-go piece `read` met, the first since its owner last asked ([`Repository::waiting`]).
  fn note_waited<T>(&self, read: &Result<T, packed::PackedRefused>) {
    if let Err(refused) = read
      && let Some(waited) = refused.waited()
    {
      self.wait_for(waited);
    }
  }

  /// The let-go piece a read met since the last ask, taken (goal 93, main 01a20182's ruling of 2026-11-22 05:45): a step
  /// whose read answered none asks this, and where it names one, its owner brings that piece back from the node's store
  /// and takes the step again, never counting the read missing.
  pub fn waiting(&self) -> Option<packed::Waited> {
    self.waiting.take()
  }

  /// The let-go window a read has met and not yet taken ([`Repository::waiting`]), left as it is.
  pub fn peek_waiting(&self) -> Option<packed::Waited> {
    let waiting = self.waiting.take();
    self.waiting.set(waiting);
    waiting
  }

  /// Notes `waited` as what a step waits for, where none is noted yet ([`Repository::waiting`]): what a caller that took
  /// a step's miss to act on it gives back when another step of its turn is the one its driver brings it back for.
  pub fn wait_for(&self, waited: packed::Waited) {
    match self.quiet.get() {
      // A quiet read's own: kept for its reader alone (B3; one slot a reader).
      Some(None) => self.quiet.set(Some(Some(waited))),
      Some(Some(_)) => {}
      None if self.waiting.get().is_none() => self.waiting.set(Some(waited)),
      None => {}
    }
  }

  /// `read` run with every let-go window its reads meet kept for its caller alone, never noted on the repository's
  /// shared slot ([`Repository::waiting`]), which a derivation's turn clears and other owners take: what it answered,
  /// and the first let-go it met (B3 of the landed let-go's lift, main's ruling of 2026-11-26: a push's admission notes
  /// its let-go in the push's own slot). A read already quiet keeps its own scope: the inner one answers what it met
  /// and leaves the outer's as it was.
  pub fn reading_quietly<T>(&self, read: impl FnOnce() -> T) -> (T, Option<packed::Waited>) {
    let outer = self.quiet.replace(Some(None));
    let answered = read();
    let met = self.quiet.replace(outer).flatten();
    (answered, met)
  }

  /// The packs the repository's objects lie in, each with its length (goal 93 item 2a: what a fetch or an encoding
  /// reads, made resident before it begins).
  pub fn packs(&self) -> impl Iterator<Item = (ContentId, u64)> + '_ {
    self.packs.iter().map(|(pack, len)| (*pack, *len))
  }

  /// The bytes of the indexed object whose row is `row`, naming `content` ([`Repository::read_stored`]).
  pub fn row_bytes<'s>(
    &self,
    store: &'s ContentStore,
    row: &ObjectRow,
    content: ContentId,
  ) -> Option<Cow<'s, [u8]>> {
    self.read_stored(store, &self.stored_of(row, content), row.len)
  }

  /// The deltas its last read of a stored object applied going back up the object's chain: none for one held whole or
  /// kept lately (`packed::Scratch::deltas_applied`).
  pub fn last_read_deltas(&self) -> u64 {
    self.last_deltas.get()
  }

  /// Whether its last read of a stored object found the object, or an entry of its chain, among the objects read lately
  /// (`packed::Scratch::found_kept`): a hit of its read cache.
  pub fn last_read_found_kept(&self) -> bool {
    self.last_found_kept.get()
  }

  /// Whether its last read of a stored object inflated its chain's root, an entry whole in its pack
  /// (`packed::Scratch::base_inflated`): none for one held whole, kept lately, or a delta whose base was kept.
  pub fn last_read_inflated_base(&self) -> bool {
    self.last_inflated_base.get()
  }

  /// The most bytes its reads keep of the objects read lately now (`packed::Recent`'s budget).
  pub fn read_cache_budget(&self) -> u64 {
    to_u64(self.reading.borrow_mut().recent().budget())
  }

  /// The tail and runs of its index its lookups have visited since the index's merge rule last took them.
  pub fn index_visits(&self) -> u64 {
    self.index.visits()
  }

  /// The type of the object at `position`, read from its landing's runs ([`Repository`]'s `kinds`): none for a position
  /// no landing gave.
  pub fn kind_at(&self, position: u64) -> Option<Kind> {
    let (_, (end, kind)) = self.kinds.range(..=position).next_back()?;
    (position < *end).then_some(*kind)
  }

  /// The id of the object at `position`.
  pub fn id_at(&self, store: &ContentStore, position: u64) -> Option<ObjectId> {
    let found = self.index.get(store, &rows::position_key(position))?;
    rows::position_id(self.format, found.payload())
  }

  /// Where `id` is: indexed first, then held by the push.
  fn locate<'p>(&self, store: &ContentStore, push: &'p Push, id: &ObjectId) -> Option<Place<'p>> {
    if let Some((row, content)) = self.indexed(store, id) {
      return Some(Place::Indexed { row, content });
    }
    let held = push.object(id)?;
    Some(Place::Pushed {
      kind: kind_of(held.number)?,
      push,
    })
  }

  /// The bytes of the object at `place`: lent where the store holds it as one window, read where it lies in a push's
  /// pack otherwise (`Push::bytes`).
  fn bytes_of<'s>(
    &self,
    store: &'s ContentStore,
    id: &ObjectId,
    place: &Place<'s>,
  ) -> Result<Cow<'s, [u8]>, StoreRefused> {
    let unreadable = StoreRefused::Unreadable { id: *id };
    match place {
      Place::Indexed { row, content } => self.row_bytes(store, row, *content).ok_or(unreadable),
      Place::Pushed { push, .. } => {
        let outside = IndexBases {
          repository: self,
          store,
        };
        match push.bytes(store, &outside, id) {
          Some(Ok(bytes)) => Ok(bytes),
          // A REF_DELTA's base outside the push met let go: named for the reader's owner to bring back (B3).
          Some(Err(refused)) => {
            if let Some(waited) = refused.waited() {
              self.wait_for(waited);
            }
            Err(unreadable)
          }
          None => Err(unreadable),
        }
      }
    }
  }

  /// The objects `id` names: a commit's tree and parents, a tree's entries (a submodule's commit is no object of this
  /// repository), a tag's object.
  fn references(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
    place: &Place,
  ) -> Result<Vec<ObjectId>, StoreRefused> {
    match place.kind() {
      Kind::Blob => Ok(Vec::new()),
      Kind::Commit => {
        let mut named = vec![self.commit_tree(store, push, id, place)?];
        named.extend(self.parents(store, push, id)?);
        Ok(named)
      }
      Kind::Tree => self.tree_references(store, id, place),
      Kind::Tag => self
        .tag_reference(store, id, place)
        .map(|target| vec![target]),
    }
  }

  /// A commit's root tree.
  fn commit_tree(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
    place: &Place,
  ) -> Result<ObjectId, StoreRefused> {
    match place {
      Place::Indexed { row, .. } => {
        let commit = row
          .commit
          .as_ref()
          .ok_or(StoreRefused::Unreadable { id: *id })?;
        self
          .id_at(store, commit.tree)
          .ok_or(StoreRefused::Unreadable { id: *id })
      }
      Place::Pushed { .. } => push
        .commits()
        .get(id)
        .map(|arrived| arrived.tree)
        .ok_or(StoreRefused::Unreadable { id: *id }),
    }
  }

  /// A commit's parents, from the push or from its row.
  fn parents(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
  ) -> Result<Vec<ObjectId>, StoreRefused> {
    if let Some(arrived) = push.commits().get(id) {
      return Ok(arrived.parents.clone());
    }
    let (row, _) = self
      .indexed(store, id)
      .ok_or(StoreRefused::Missing { id: *id })?;
    let commit = row.commit.ok_or(StoreRefused::NotACommit { id: *id })?;
    commit
      .parents
      .iter()
      .map(|parent| {
        self
          .id_at(store, *parent)
          .ok_or(StoreRefused::Unreadable { id: *id })
      })
      .collect()
  }

  /// A commit's generation number, from the push (sealed) or from its row.
  fn generation(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
  ) -> Result<u64, StoreRefused> {
    if let Some(arrived) = push.commits().get(id) {
      return Ok(arrived.generation);
    }
    let (row, _) = self
      .indexed(store, id)
      .ok_or(StoreRefused::Missing { id: *id })?;
    row
      .commit
      .map(|commit| commit.generation)
      .ok_or(StoreRefused::NotACommit { id: *id })
  }

  /// What git's tree walk finds for `path` in the tree `id` ([`Tree::find`]; A-640 part 4: a `sparse:oid` filter's
  /// `<rev>:<path>`), a submodule's commit an entry like any other: the entry's object and the path left below it.
  fn tree_entry<'p>(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
    path: &'p [u8],
  ) -> Result<Option<(ObjectId, &'p [u8])>, StoreRefused> {
    let bytes = self.bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tree =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tree
      .find(path)
      .map(|(entry, below)| {
        entry
          .object_id(self.format)
          .map(|object| (object, below))
          .ok_or(malformed)
      })
      .transpose()
  }

  /// A tree's entries' objects with their names, in the tree's order, a submodule's commit left out (A-640 part 4: what
  /// a sparse filter asks its patterns of).
  fn tree_named_references(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
  ) -> Result<Vec<(ObjectId, Vec<u8>)>, StoreRefused> {
    let bytes = self.bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tree =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tree
      .entries()
      .iter()
      .filter(|entry| entry.kind() != Kind::Commit)
      .map(|entry| {
        entry
          .object_id(self.format)
          .map(|object| (object, entry.name.to_vec()))
          .ok_or(malformed)
      })
      .collect()
  }

  /// A tree's entries' objects, a submodule's commit left out.
  fn tree_references(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
  ) -> Result<Vec<ObjectId>, StoreRefused> {
    let bytes = self.bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tree =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tree
      .entries()
      .iter()
      .filter(|entry| entry.kind() != Kind::Commit)
      .map(|entry| entry.object_id(self.format).ok_or(malformed))
      .collect()
  }

  /// The object a tag names.
  fn tag_reference(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
  ) -> Result<ObjectId, StoreRefused> {
    let bytes = self.bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tag = Tag::parse(&bytes, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tag.object(self.format).ok_or(malformed)
  }

  /// Seals `push`: each of its commits' generation numbers, one more than its parents' greatest, its parents read from
  /// the push or the repository's commit rows. Refused at a parent neither holds, or one that is no commit.
  pub fn seal(&self, store: &ContentStore, push: &mut Push) -> Result<(), StoreRefused> {
    self.same_format(push)?;
    let mut generations: BTreeMap<ObjectId, u64> = BTreeMap::new();
    let ids: Vec<ObjectId> = push.commits().keys().copied().collect();
    for id in ids {
      self.generations_from(store, push, id, &mut generations)?;
    }
    push.seal_with(&generations);
    Ok(())
  }

  /// The generation numbers of `start` and of every commit of the push below it, parents first, without recursion.
  fn generations_from(
    &self,
    store: &ContentStore,
    push: &Push,
    start: ObjectId,
    done: &mut BTreeMap<ObjectId, u64>,
  ) -> Result<(), StoreRefused> {
    let mut stack = vec![(start, false)];
    while let Some((id, parents_done)) = stack.pop() {
      let Some(arrived) = push.commits().get(&id).filter(|_| !done.contains_key(&id)) else {
        continue;
      };
      if parents_done {
        let greatest = self.parents_generation(store, &arrived.parents, done)?;
        done.insert(id, greatest.saturating_add(1));
        continue;
      }
      stack.push((id, true));
      for parent in &arrived.parents {
        if push.commits().contains_key(parent) && !done.contains_key(parent) {
          stack.push((*parent, false));
        }
      }
    }
    Ok(())
  }

  /// The greatest generation number among `parents`, each from `done` or the repository's commit rows; zero for none.
  fn parents_generation(
    &self,
    store: &ContentStore,
    parents: &[ObjectId],
    done: &BTreeMap<ObjectId, u64>,
  ) -> Result<u64, StoreRefused> {
    let mut greatest: u64 = 0;
    for parent in parents {
      let generation = match done.get(parent) {
        Some(generation) => *generation,
        None => {
          let (row, _) = self
            .indexed(store, parent)
            .ok_or(StoreRefused::Missing { id: *parent })?;
          let commit = row.commit.ok_or(StoreRefused::NotACommit { id: *parent })?;
          commit.generation
        }
      };
      greatest = greatest.max(generation);
    }
    Ok(greatest)
  }

  /// The objects reachable from `from` that `stop` does not hold, read from the push and the index; a walk stops at each
  /// object `stop` holds, so a reached tree is skipped whole.
  fn walk(
    &self,
    store: &ContentStore,
    push: &Push,
    from: ObjectId,
    stop: &Positions,
  ) -> Result<Members, StoreRefused> {
    let (mut indexed, mut new) = (Vec::new(), Vec::new());
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut pending = vec![from];
    while let Some(id) = pending.pop() {
      if !seen.insert(id) {
        continue;
      }
      let place = self
        .locate(store, push, &id)
        .ok_or(StoreRefused::Missing { id })?;
      match &place {
        Place::Indexed { row, .. } if stop.contains(row.position) => {
          self.met_reached(row.kind);
          continue;
        }
        Place::Indexed { row, .. } => indexed.push(row.position),
        Place::Pushed { .. } => new.push(id),
      }
      self.count(|counters| counters.walked = counters.walked.saturating_add(1));
      pending.extend(self.references(store, push, &id, &place)?);
    }
    Ok(Members {
      staged: None,
      indexed: Positions::of(indexed),
      new,
    })
  }

  /// A walk met an object already reached.
  fn met_reached(&self, kind: Kind) {
    self.count(|counters| {
      counters.reached_met = counters.reached_met.saturating_add(1);
      if kind == Kind::Tree {
        counters.trees_skipped = counters.trees_skipped.saturating_add(1);
      }
    });
  }

  /// The commit `id` peels to: itself, or the commit a chain of tags names; `None` for a tree or a blob.
  pub(super) fn peel(
    &self,
    store: &ContentStore,
    push: &Push,
    id: ObjectId,
  ) -> Result<Option<ObjectId>, StoreRefused> {
    let mut at = id;
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    while seen.insert(at) {
      let place = self
        .locate(store, push, &at)
        .ok_or(StoreRefused::Missing { id: at })?;
      match place.kind() {
        Kind::Commit => return Ok(Some(at)),
        Kind::Tag => at = self.tag_reference(store, &at, &place)?,
        Kind::Tree | Kind::Blob => return Ok(None),
      }
    }
    Ok(None)
  }

  /// Whether the commit `ancestor` is `head` or one of its ancestors. Decided with no walk where the ids or the
  /// generation numbers decide it (A-644): `head` is `ancestor`; or `head`'s generation number is no greater than
  /// `ancestor`'s, and since a commit's is one more than its parents' greatest, no descendant of `ancestor` has one
  /// that small. Otherwise a walk back by generation number from `head`, which stops below the ancestor's generation,
  /// so it costs the commits between. Either not a commit: not a fast-forward.
  fn fast_forward(
    &self,
    store: &ContentStore,
    push: &Push,
    head: ObjectId,
    ancestor: ObjectId,
  ) -> Result<bool, StoreRefused> {
    let commits = (
      self.locate(store, push, &head).map(|place| place.kind()),
      self
        .locate(store, push, &ancestor)
        .map(|place| place.kind()),
    );
    if commits != (Some(Kind::Commit), Some(Kind::Commit)) {
      return Ok(false);
    }
    let floor = self.generation(store, push, &ancestor)?;
    let top = self.generation(store, push, &head)?;
    if head == ancestor || top <= floor {
      self.count(|counters| {
        counters.decided_without_walk = counters.decided_without_walk.saturating_add(1);
      });
      return Ok(head == ancestor);
    }
    self.count(|counters| counters.descent_walks = counters.descent_walks.saturating_add(1));
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut heap = BinaryHeap::from([(top, head)]);
    while let Some((generation, id)) = heap.pop() {
      if id == ancestor {
        return Ok(true);
      }
      if generation <= floor || !seen.insert(id) {
        continue;
      }
      for parent in self.parents(store, push, &id)? {
        heap.push((self.generation(store, push, &parent)?, parent));
      }
    }
    Ok(false)
  }

  /// Admits `green`'s move to `to` (`None` deletes the ref) before its verdict: what its version brings and how its
  /// reached set follows, read from the sealed `push` and the index (module docs).
  pub fn admit(
    &self,
    store: &ContentStore,
    push: &Push,
    green: u64,
    to: Option<ObjectId>,
  ) -> Result<Move, StoreRefused> {
    self.same_format(push)?;
    if !push.is_sealed() {
      return Err(StoreRefused::Unsealed);
    }
    let chain = self.chains.get(&green);
    let from = chain.and_then(Chain::head);
    let Some(to) = to else {
      return Ok(Move {
        green,
        from,
        to: None,
        peeled: None,
        brought: Members::default(),
        reach: Reach::Emptied,
      });
    };
    let mut admitting = self.admitting(store, push, green, to)?;
    let mut clock = staging::Unbounded;
    while !self.admit_turn(
      store,
      push,
      &mut admitting,
      Quantum {
        clock: &mut clock,
        ns: u64::MAX,
      },
    )? {}
    admitting.admitted().ok_or(StoreRefused::Unsealed)
  }

  /// The generation number of the commit `id` peels to, read through `push` and the index (A-850): what a push's
  /// command is decided by ([`crate::receive::Judge::rank`]), so its moves are admitted oldest first and each one's
  /// search back meets the one below. None for an object neither holds, or one that peels to no commit.
  pub fn peeled_generation(&self, store: &ContentStore, push: &Push, id: ObjectId) -> Option<u64> {
    let commit = self.peel(store, push, id).ok()??;
    self.generation(store, push, &commit).ok()
  }

  /// `movement`, admitted against the store as a replay finds it, with the base its push's record names in place of the
  /// one its admission found (A-850): the record, not the heads the replay meets, decides what the version's sets are
  /// relative to, so a store made again from the log derives the same sets and fragments. Refused
  /// [`StoreRefused::Base`] when `base` names no version with a head, one whose head peels to no commit,
  /// or to a commit neither the one the move's head peels to nor of a lower generation number (no ancestor of it; an
  /// ancestor is not proved, which would walk), or names one for a move not remade with no walk; a remade move whose
  /// record names none takes none.
  pub fn based(
    &self,
    store: &ContentStore,
    push: &Push,
    mut movement: Move,
    base: Option<(u64, u64)>,
  ) -> Result<Move, StoreRefused> {
    let to = movement.to;
    let Reach::Remade { from } = &mut movement.reach else {
      return match base {
        None => Ok(movement),
        Some((green, version)) => Err(StoreRefused::Base { green, version }),
      };
    };
    let Some((green, version)) = base else {
      *from = None;
      return Ok(movement);
    };
    let refused = StoreRefused::Base { green, version };
    let at = usize::try_from(version).map_err(|_| refused)?;
    // The moving green's own versions are all made before the move: any is a base its search back may meet.
    let head = self
      .chains
      .get(&green)
      .and_then(|chain| chain.versions.get(at))
      .and_then(|made_at| made_at.head)
      .ok_or(refused)?;
    let to = to.ok_or(refused)?;
    let at_head = matches!(self.locate(store, push, &to), Some(Place::Indexed { row, .. }) if row.position == head);
    if !at_head {
      let base_commit = self
        .id_at(store, head)
        .map(|id| self.peel(store, push, id))
        .transpose()?
        .flatten()
        .ok_or(refused)?;
      let commit = self.peel(store, push, to)?.ok_or(refused)?;
      if base_commit != commit
        && self.generation(store, push, &base_commit)? >= self.generation(store, push, &commit)?
      {
        return Err(refused);
      }
    }
    *from = Some((green, at));
    Ok(movement)
  }

  /// Commits `push` and `moves` once the verdict accepts them: a position and rows for each object new to the
  /// repository, and a version for each move with its value of the reach sets (what it brought and its reached set,
  /// A-647 part 3) and the head map moved. The values, the rows and the versions' bytes are admitted first: refused,
  /// nothing changed, and the push still holds its objects. Committed, the push holds nothing. The versions made, one
  /// for each move, in order.
  pub fn commit(
    &mut self,
    store: &mut ContentStore,
    push: &mut Push,
    moves: &[Move],
  ) -> Result<Vec<u64>, StoreRefused> {
    self.same_format(push)?;
    let first = moves.first().ok_or(StoreRefused::NoMove)?;
    self.check_moves(moves)?;
    let staged = match self.adoptable_stage(store, push)? {
      Some(staged) => staged,
      None => {
        if !push.is_empty() {
          self.count(|counters| counters.restaged = counters.restaged.saturating_add(1));
        }
        self.stage(store, push)?;
        push.take_staged().ok_or(StoreRefused::Unsealed)?
      }
    };
    let Staged {
      fork,
      positions,
      base,
      kinds,
    } = staged;
    let given = u64::try_from(positions.len()).unwrap_or(u64::MAX);
    // A stage that gives no position leaves the next position where it is, however long ago it was made.
    let next = if positions.is_empty() {
      self.next_position
    } else {
      base.saturating_add(given)
    };
    // Refused before its adoption, the stage's fork is given back with whatever else the commit took, so a refused
    // commit leaves the store as it was (P2's oracle: it was dropped holding its tables, 598 bytes a refusal).
    let prepared = match self.prepared(store, moves, (base, &positions, kinds.len())) {
      Ok(prepared) => prepared,
      Err(refused) => {
        let _ = fork.release(store);
        return Err(refused);
      }
    };
    let Prepared {
      landed,
      planned,
      values,
      entries,
      charge,
    } = prepared;
    // A stage that gives no position writes no row: its fork is given back, never adopted.
    let inserted = if positions.is_empty() {
      fork.release(store).map_err(StoreRefused::from)
    } else {
      self.adopted(store, fork).map_err(|refused| match refused {
        slates_vcs::AdoptRefused::Refused(refused, fork) => {
          let _ = fork.release(store);
          StoreRefused::from(refused)
        }
        slates_vcs::AdoptRefused::Stale(fork) | slates_vcs::AdoptRefused::NotAFork(fork) => {
          let _ = fork.release(store);
          StoreRefused::Moved { green: first.green }
        }
      })
    };
    if let Err(refused) = inserted {
      store.discharge(charge.records.saturating_add(charge.landings))?;
      give_back(store, &values.iter().flatten().copied().collect::<Vec<_>>())?;
      return Err(refused);
    }
    // Nothing of the repository changed before this point; nothing refuses after it but a discharge or a release past
    // what was held, which no commit's own charge and holds can be.
    if let Some(landed) = landed {
      if matches!(landed, Landed::Moves(_)) {
        self.unsettled.push_back(base);
      }
      self.landings.insert(base, landed);
    }
    if !positions.is_empty()
      && let Some((pack, len)) = push.stored_pack()
    {
      self.packs.insert(pack, len);
      self.largest = self.largest.max(push.largest());
      self.largest_pack = self.largest_pack.max(len);
      self
        .reading
        .get_mut()
        .recent()
        .set_budget(usize::try_from(self.largest_pack).unwrap_or(usize::MAX));
    }
    self.kinds.extend(
      kinds
        .into_iter()
        .map(|(start, end, kind)| (start, (end, kind))),
    );
    self.landings_charged = self.landings_charged.saturating_add(charge.landings);
    self
      .count(|counters| counters.value_bytes = counters.value_bytes.saturating_add(charge.values));
    let versions = self.install_versions(store, moves, (planned, &values), (entries, next));
    // Charged at the peeled and holders' maps' bound, now trued up to what they hold, so the charge is always the whole
    // recomputation's.
    let exact = self.charge_now();
    store.discharge(
      self
        .charged
        .saturating_add(charge.records)
        .saturating_sub(exact),
    )?;
    self.charged = exact;
    self.count(|counters| counters.positions = counters.positions.saturating_add(given));
    self.depth.seen(push.deepest());
    // The positions the stage gave, one entry an object, are given up with the push's maps, freed after the step (P2
    // piece 3, `Shedding`).
    push.shed_positions(positions);
    push.release_holds(store)?;
    // The push's objects read lately go with it, their charge given back (A-861).
    push.release_recent(store);
    Ok(versions)
  }

  /// Adopts the stage's `fork` into the index ([`Index::adopt`]). In a test that asks to refuse a commit at its
  /// adoption (the test-only `REFUSE_COMMIT_AT`) it is refused as a stale fork is, since a current fork's adoption
  /// seals no tail and charges nothing a budget could refuse, and the commit's give-back after it must still be
  /// exercised.
  fn adopted(
    &mut self,
    store: &mut ContentStore,
    fork: Index,
  ) -> Result<(), slates_vcs::AdoptRefused> {
    #[cfg(test)]
    if REFUSE_COMMIT_AT.with(Cell::get) == Some(CommitPoint::Adopt) {
      COMMIT_REFUSED_AT.with(|refused| refused.borrow_mut().insert(CommitPoint::Adopt));
      return Err(slates_vcs::AdoptRefused::Stale(Box::new(fork)));
    }
    self.index.adopt(store, fork)
  }

  /// What `commit` makes before its stage is adopted, changing nothing of the repository: each move's planned version
  /// ([`Repository::plan`]), their values put, the landing map's entry for the stage's `positions` from `base`
  /// ([`Repository::landed`]), the head map's entries once moved, and the records' and the landing
  /// map's charges taken. Each move adds one version record to its chain (a new version keeps no fragment) and notes at
  /// most one commit more in the peeled map and one green more in the holders' map, so the records are charged by that
  /// difference at that bound, never by summing every chain again (P2: 22,021 chain records and head entries read for
  /// one move over 1,000 greens of 20 versions each); the commit trues the bound up once they are noted. Refused at any
  /// step, everything it took is given back.
  fn prepared(
    &self,
    store: &mut ContentStore,
    moves: &[Move],
    (base, positions, kinds): (u64, &BTreeMap<ObjectId, u64>, usize),
  ) -> Result<Prepared, StoreRefused> {
    let (mut planned, entries) = self.plan(store, moves, (base, positions))?;
    let before = store.used();
    let values = at_point(store, CommitPoint::Values, |store| {
      put_values(store, &planned)
    })?;
    let landed = match self.landed(store, moves, &mut planned, (base, positions)) {
      Ok(landed) => landed,
      Err(refused) => {
        give_back(store, &values.iter().flatten().copied().collect::<Vec<_>>())?;
        return Err(refused);
      }
    };
    let put: Vec<ContentId> = values.iter().flatten().copied().collect();
    let added = store.used().saturating_sub(before);
    let chains = self
      .chains_charged
      .saturating_add(bytes(moves.len(), VERSION_BYTES));
    let peeled = self.peeled.len().saturating_add(moves.len());
    let holders = self.holder_entries.saturating_add(moves.len());
    let total = charge_of(chains, (entries, peeled, holders));
    let records = total.saturating_sub(self.charged);
    if let Err(refused) = at_point(store, CommitPoint::Records, |store| store.charge(records)) {
      give_back(store, &put)?;
      return Err(refused.into());
    }
    let landings = landed
      .as_ref()
      .map_or(0, Landed::cost)
      .saturating_add(bytes(kinds, LANDING_BYTES));
    if let Err(refused) = at_point(store, CommitPoint::Landings, |store| store.charge(landings)) {
      store.discharge(records)?;
      give_back(store, &put)?;
      return Err(refused.into());
    }
    Ok(Prepared {
      landed,
      planned,
      values,
      entries,
      charge: Charge {
        records,
        landings,
        values: added,
      },
    })
  }

  /// What the chains, the head map, the peeled map and the holders' map charge as they stand: the chains' kept sum
  /// (`chains_charged`) and the maps' entries.
  fn charge_now(&self) -> u64 {
    charge_of(
      self.chains_charged,
      (self.head_entries, self.peeled.len(), self.holder_entries),
    )
  }

  /// The versions `commit` makes, put in place once everything is charged and the stage adopted: each move's planned
  /// version pushed onto its green's chain in place with its value (`values`, in order), the versions with no value
  /// counted (`unvalued`), each green's head moved in the head map in place, the chains' charge and the map's
  /// `entries` (`plan`'s count) taken, the next position taken, each version's commit noted for a later create's search
  /// back (A-850) and its holders, and each version committed with its value noted known. The versions made, one for
  /// each move, in order. It reads each moved green's chain and head entries alone (P2: it cloned each moved chain and
  /// took a copy of the whole head map before).
  fn install_versions(
    &mut self,
    store: &ContentStore,
    moves: &[Move],
    (planned, values): (Vec<Planned>, &[Option<ContentId>]),
    (entries, next): (usize, u64),
  ) -> Vec<u64> {
    let mut versions = Vec::with_capacity(planned.len());
    for (planned, value) in planned.into_iter().zip(values) {
      let chain = self.chains.entry(planned.green).or_default();
      chain.versions.push(Version {
        head: planned.head,
        value: *value,
        fragments: None,
        followed: planned.followed,
      });
      versions.push(to_u64(chain.versions.len()).saturating_sub(1));
      if value.is_none() {
        self.unvalued = self.unvalued.saturating_add(1);
      }
      move_head(&mut self.heads, planned.green, planned.from, planned.head);
    }
    self.chains_charged = self
      .chains_charged
      .saturating_add(bytes(versions.len(), VERSION_BYTES));
    self.head_entries = entries;
    self.next_position = next;
    // A-850: each version's commit, the first version noted under it, for a later create's search back.
    for (movement, version) in moves.iter().zip(&versions) {
      let position = movement
        .peeled
        .and_then(|commit| self.indexed(store, &commit))
        .map(|(row, _)| row.position);
      if let Some(position) = position {
        let version = usize::try_from(*version).unwrap_or(usize::MAX);
        self
          .peeled
          .entry(position)
          .or_insert((movement.green, version));
      }
    }
    self.note_holders(moves, &versions);
    // Each version committed with its value is a base a later derivation may search back to (`deriving`).
    for (movement, version) in moves.iter().zip(&versions) {
      let version = usize::try_from(*version).unwrap_or(usize::MAX);
      let valued = self
        .chains
        .get(&movement.green())
        .and_then(|chain| chain.versions.get(version))
        .is_some_and(|record| record.value.is_some());
      if valued {
        self.note_known(store, (movement.green(), version));
      }
    }
    versions
  }

  /// Goal 93: each version `moves` committed (`versions`, in the same order) noted under its head in the holders' map,
  /// the green with this version, its newest; a green new under a head counted in `holder_entries`, what the map
  /// charges the store.
  fn note_holders(&mut self, moves: &[Move], versions: &[u64]) {
    for (movement, version) in moves.iter().zip(versions) {
      let version = usize::try_from(*version).unwrap_or(usize::MAX);
      let head = self
        .chains
        .get(&movement.green)
        .and_then(|chain| chain.versions.get(version))
        .and_then(|record| record.head);
      if let Some(head) = head
        && self
          .holders
          .entry(head)
          .or_default()
          .insert(movement.green, version)
          .is_none()
      {
        self.holder_entries = self.holder_entries.saturating_add(1);
      }
    }
  }

  /// Every move's green is where it was when the move was admitted, and no green is moved twice.
  fn check_moves(&self, moves: &[Move]) -> Result<(), StoreRefused> {
    let mut moved: BTreeSet<u64> = BTreeSet::new();
    for movement in moves {
      let now = self.chains.get(&movement.green).and_then(Chain::head);
      if now != movement.from || !moved.insert(movement.green) {
        return Err(StoreRefused::Moved {
          green: movement.green,
        });
      }
    }
    Ok(())
  }

  /// Stages `push`, sealed, whole (A-722 part 3): each object it holds that the repository does not index given a
  /// position after the repository's next, in order by type and then id as A-639 orders them, and its rows (its object
  /// row with a commit's part, and its position row) written into a fork of the index made now and sealed into
  /// tables, so the push's landing adopts them and writes none. Its positions are tentative: the landing takes them
  /// when nothing landed since, and otherwise stages the push again. A push staged before gives its stage back first.
  /// Every landing adopts a fork, so the repository's own index keeps no tail of rows, and a fork made at any time (a
  /// push abandoned, another staging at once) changes nothing a store made again from the log would not make alike.
  pub fn stage(&mut self, store: &mut ContentStore, push: &mut Push) -> Result<(), StoreRefused> {
    let staging = self.staging(store, push)?;
    self.stage_whole(store, push, staging)
  }

  /// Derived: the most staging `push` could charge the store ([`Self::stage`]): each object's object row (a commit's
  /// part and parents included) and position row with their tail slots, doubled for the tables a seal writes beside the
  /// tail it replaces; and the fork staging makes.
  pub fn stage_room(&self, push: &Push) -> u64 {
    let raw = self.format.raw_len();
    let object_row = raw
      .saturating_add(1)
      .saturating_add(rows::OBJECT_PAYLOAD_BYTES)
      .saturating_add(slates_vcs::TAIL_SLOT_BYTES);
    let position_row = raw
      .saturating_add(1)
      .saturating_add(size_of::<u64>())
      .saturating_add(slates_vcs::TAIL_SLOT_BYTES);
    let objects = bytes(
      push.len(),
      object_row.saturating_add(position_row).saturating_mul(2),
    );
    // A commit's part and its parents, each doubled, summed from the push's counts: no pass over its commits.
    let (commits, parents) = push.commit_parts();
    let commits = bytes(commits, rows::COMMIT_PART_BYTES.saturating_mul(2))
      .saturating_add(bytes(parents, size_of::<u64>().saturating_mul(2)));
    objects
      .saturating_add(commits)
      .saturating_add(self.index.fork_room())
  }

  /// Whether `push` holds objects its commit would stage again (A-722 part 4e): none staged, or a stage a landing
  /// overtook since ([`Self::adoptable_stage`]'s rule). A push's judge stages it again a turn at a time while it does.
  pub fn needs_stage(&self, push: &Push) -> bool {
    if push.is_empty() {
      return false;
    }
    !push
      .staged()
      .is_some_and(|staged| self.stage_current(staged))
  }

  /// Whether `staged` is a stage this index takes as it is: one that gives no position (every object the push holds
  /// indexed already, as they stay), which no landing can overtake (A-722 part 4e); or one no landing overtook, no row
  /// taken in nor position given since its fork was made.
  fn stage_current(&self, staged: &super::push::Staged) -> bool {
    staged.positions.is_empty()
      || (staged.base == self.next_position && self.index.adoptable(&staged.fork))
  }

  /// The push's staged rows when this index would adopt them as they are (A-722 part 3): no row taken in nor position
  /// given by the repository since its fork was made. A stage this repository outran is given back, for the push to be
  /// staged again ([`Self::stage`]).
  fn adoptable_stage(
    &self,
    store: &mut ContentStore,
    push: &mut Push,
  ) -> Result<Option<super::push::Staged>, StoreRefused> {
    let current = push
      .staged()
      .is_some_and(|staged| self.stage_current(staged));
    let Some(staged) = push.take_staged() else {
      return Ok(None);
    };
    if current {
      return Ok(Some(staged));
    }
    staged.fork.release(store)?;
    Ok(None)
  }

  /// The landing map's entry for a commit's fresh objects, the stage's `positions` from `base` ([`Landed`]): none where
  /// the stage gives none; for one move, one run of them all; for several, each move's brought set taken from its plan
  /// (its value put, nothing reads the set after) in the moves' order, a position none of them holds going to the
  /// first move, as A-722's map gives it, to be mapped by position later, a slice a turn
  /// ([`Repository::settle_landings_turn`]). The step walks no position: it walked every position the stage gave,
  /// and each move's brought set (P2 piece 2: 599 for a push of 599 commits moving two greens). A move with no sets
  /// planned (a create, a rewrite, one whose sets are unknown) has its walk's members taken as positions.
  fn landed(
    &self,
    store: &ContentStore,
    moves: &[Move],
    planned: &mut [Planned],
    (base, positions): (u64, &BTreeMap<ObjectId, u64>),
  ) -> Result<Option<Landed>, StoreRefused> {
    let given = to_u64(positions.len());
    let Some(first) = moves.first().filter(|_| given > 0) else {
      return Ok(None);
    };
    let end = base.saturating_add(given);
    let pair = |green: u64| {
      let version = self
        .chains
        .get(&green)
        .map_or(0, |chain| to_u64(chain.versions.len()));
      (green, version)
    };
    if moves.len() == 1 {
      let (green, version) = pair(first.green);
      return Ok(Some(Landed::Runs(vec![(base, end, green, version)])));
    }
    let mut brought = Vec::with_capacity(moves.len());
    for (movement, plan) in moves.iter().zip(planned.iter_mut()) {
      let set = match plan.sets.take() {
        Some(sets) => sets.brought,
        None => self.members_positions(store, &movement.brought, (base, positions))?,
      };
      brought.push((pair(movement.green), set));
    }
    Ok(Some(Landed::Moves(Box::new(LandedMoves {
      end,
      brought,
      otherwise: pair(first.green),
    }))))
  }

  /// Whether a landing of several moves still awaits its map by position ([`Repository::settle_landings_turn`]).
  pub fn landings_unsettled(&self) -> bool {
    !self.unsettled.is_empty()
  }

  /// Derived: the most the oldest unsettled landing's runs could charge the store once mapped
  /// ([`Repository::settle_landings_turn`]): a run's record a position it gave, at most. None with none unsettled.
  pub fn settle_room(&self) -> u64 {
    self
      .unsettled
      .front()
      .and_then(|base| match self.landings.get(base) {
        Some(Landed::Moves(moves)) => {
          let given = usize::try_from(moves.end.saturating_sub(*base)).unwrap_or(usize::MAX);
          Some(bytes(given, LANDING_BYTES))
        }
        _ => None,
      })
      .unwrap_or(0)
  }

  /// One turn of mapping the landings of several moves by position, oldest first (P2 piece 2): a position a unit, its
  /// bringer the first of its landing's moves whose brought set holds it, joined into runs, until the quantum is spent,
  /// a unit at least; a landing mapped whole has its runs put in place of its moves' sets and the store's charge moved
  /// by the difference. Whether none is left. Refused only by the store's charge for a landing's runs, which leaves it
  /// unsettled, answered by its moves' sets meanwhile, for a later turn.
  pub fn settle_landings_turn(
    &mut self,
    store: &mut ContentStore,
    quantum: Quantum<'_>,
  ) -> Result<bool, StoreRefused> {
    let deadline = quantum.clock.now_ns().saturating_add(quantum.ns);
    let mut mapped = 0_u64;
    let settled = loop {
      match self.settle_unit(store) {
        Ok(SettleUnit::Done) => break Ok(true),
        Ok(SettleUnit::Mapped) => mapped = mapped.saturating_add(1),
        Ok(SettleUnit::Settled) => {}
        Err(refused) => break Err(refused),
      }
      if quantum.clock.now_ns() >= deadline {
        break Ok(false);
      }
    };
    self.count(|counters| counters.landing_mapped = counters.landing_mapped.saturating_add(mapped));
    settled
  }

  /// One unit of [`Repository::settle_landings_turn`]: the oldest unsettled landing's next position mapped, or, mapped
  /// whole, its runs put in its moves' place ([`Repository::settled_runs`]).
  fn settle_unit(&mut self, store: &mut ContentStore) -> Result<SettleUnit, StoreRefused> {
    let Some(base) = self.unsettled.front().copied() else {
      return Ok(SettleUnit::Done);
    };
    let Some(Landed::Moves(moves)) = self.landings.get(&base) else {
      self.unsettled.pop_front();
      self.settling = None;
      return Ok(SettleUnit::Settled);
    };
    let settling = match &mut self.settling {
      Some(settling) if settling.base == base => settling,
      other => other.insert(Settling {
        base,
        next: base,
        runs: Vec::new(),
      }),
    };
    if settling.next < moves.end {
      let position = settling.next;
      settling.map(position, moves.bringer(position));
      return Ok(SettleUnit::Mapped);
    }
    self.settled_runs(store, base)?;
    Ok(SettleUnit::Settled)
  }

  /// Puts the runs mapped for the landing at `base` in place of its moves' sets, the store's charge moved from the
  /// sets' bytes to the runs'. Refused that charge, the runs are kept for a later turn and nothing changed.
  fn settled_runs(&mut self, store: &mut ContentStore, base: u64) -> Result<(), StoreRefused> {
    let was = match self.landings.get(&base) {
      Some(Landed::Moves(moves)) => moves.cost(),
      _ => 0,
    };
    let runs = self
      .settling
      .as_mut()
      .map(|settling| core::mem::take(&mut settling.runs))
      .unwrap_or_default();
    let now = bytes(runs.len(), LANDING_BYTES);
    let moved = if now > was {
      store.charge(now.saturating_sub(was))
    } else {
      store.discharge(was.saturating_sub(now))
    };
    if let Err(refused) = moved {
      if let Some(settling) = self.settling.as_mut() {
        settling.runs = runs;
      }
      return Err(refused.into());
    }
    self.landings_charged = self
      .landings_charged
      .saturating_sub(was)
      .saturating_add(now);
    self.landings.insert(base, Landed::Runs(runs));
    self.unsettled.pop_front();
    self.settling = None;
    Ok(())
  }

  /// The position of `id`: given now, or its row's.
  fn position_of(
    &self,
    store: &ContentStore,
    positions: &BTreeMap<ObjectId, u64>,
    id: &ObjectId,
  ) -> Result<u64, StoreRefused> {
    if let Some(position) = positions.get(id) {
      return Ok(*position);
    }
    self
      .indexed(store, id)
      .map(|(row, _)| row.position)
      .ok_or(StoreRefused::Missing { id: *id })
  }

  /// A fresh commit's part of its row: its tree and parents by position, its date and generation number.
  fn commit_row(
    &self,
    store: &ContentStore,
    push: &Push,
    positions: &BTreeMap<ObjectId, u64>,
    id: &ObjectId,
    kind: Kind,
  ) -> Result<Option<CommitRow>, StoreRefused> {
    if kind != Kind::Commit {
      return Ok(None);
    }
    let arrived = push
      .commits()
      .get(id)
      .ok_or(StoreRefused::Unreadable { id: *id })?;
    let parents = arrived
      .parents
      .iter()
      .map(|parent| self.position_of(store, positions, parent))
      .collect::<Result<Vec<u64>, StoreRefused>>()?;
    Ok(Some(CommitRow {
      tree: self.position_of(store, positions, &arrived.tree)?,
      date: arrived.date,
      generation: arrived.generation,
      parents,
    }))
  }

  /// Each move's version as `commit` will make it, read from the repository as it stands and changing nothing: its
  /// reached set grown by what it brought where its green's sets are known (A-647 part 3), and its head's position;
  /// and the head map's entries once every move is made ([`Repository::entries_moved`]). It reads each moved green's
  /// newest version and its two head entries (counted in `commit_walked`), never the whole map: it took a copy of the
  /// map before (P2).
  fn plan(
    &self,
    store: &ContentStore,
    moves: &[Move],
    (base, positions): (u64, &BTreeMap<ObjectId, u64>),
  ) -> Result<(Vec<Planned>, usize), StoreRefused> {
    let mut planned = Vec::with_capacity(moves.len());
    let mut entries = self.head_entries;
    for movement in moves {
      // A fast-forward over sets known when it was admitted grows them now; one whose green's sets went unknown since (a
      // memo let go) is left for its first reader, as one admitted so; a create, a rewrite and a fast-forward over sets
      // never walked are left for theirs (A-614 amended).
      let sets = match &movement.reach {
        Reach::Grown => match self.current_reached(store, movement.green) {
          Ok(reached) => {
            let brought = self.members_positions(store, &movement.brought, (base, positions))?;
            let reached = reached.unwrap_or_default().union(&brought);
            Some(Sets { reached, brought })
          }
          Err(StoreRefused::Deferred { .. }) => None,
          Err(refused) => return Err(refused),
        },
        Reach::GrownUnwalked | Reach::Remade { .. } => None,
        Reach::Emptied => Some(Sets::default()),
      };
      let head = movement
        .to
        .map(|to| self.position_of(store, positions, &to))
        .transpose()?;
      entries = self.entries_moved(entries, movement.green, (movement.from, head));
      planned.push(Planned {
        green: movement.green,
        from: movement.from,
        head,
        followed: followed(&movement.reach),
        sets,
      });
    }
    self.count(|counters| {
      counters.commit_walked = counters
        .commit_walked
        .saturating_add(to_u64(moves.len().saturating_mul(RECORDS_A_MOVE)));
    });
    Ok((planned, entries))
  }

  /// The head map's entries once `green` moves from the head at `from` to the one at `to` ([`move_head`]), from
  /// `entries`: its entry under its old head taken out, and one under its new head put in unless the map holds it
  /// there after the move out. A commit moves each green once (`check_moves`), so each move's change is its own.
  fn entries_moved(
    &self,
    entries: usize,
    green: u64,
    (from, to): (Option<u64>, Option<u64>),
  ) -> usize {
    let held = |head: u64| {
      self
        .heads
        .get(&head)
        .is_some_and(|greens| greens.contains(&green))
    };
    let out = from.is_some_and(held);
    let kept = to.is_some_and(|to| held(to) && !(out && from == Some(to)));
    let put = to.is_some() && !kept;
    entries
      .saturating_sub(usize::from(out))
      .saturating_add(usize::from(put))
  }

  /// A walk's members as positions, the new ones by the positions given them now: as the walk found them staged where
  /// the stage given from `base` is the one it met (`Members::staged`), else each looked up by id.
  fn members_positions(
    &self,
    store: &ContentStore,
    members: &Members,
    (base, positions): (u64, &BTreeMap<ObjectId, u64>),
  ) -> Result<Positions, StoreRefused> {
    if let Some(staged) = &members.staged
      && staged.stage == (base, positions.len())
    {
      return Ok(members.indexed.union(&staged.positions));
    }
    let new = members
      .new
      .iter()
      .map(|id| self.position_of(store, positions, id))
      .collect::<Result<Vec<u64>, StoreRefused>>()?;
    Ok(members.indexed.union(&Positions::of(new)))
  }

  /// The versions `green` has.
  pub fn versions(&self, green: u64) -> usize {
    self
      .chains
      .get(&green)
      .map_or(0, |chain| chain.versions.len())
  }

  /// D(k): what `green`'s version `version` brought, as positions, read from the version's value (A-647 part 3).
  pub fn brought(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Positions, StoreRefused> {
    Ok(self.sets(store, green, version)?.brought)
  }

  /// The set `green`'s version `version`'s head reaches, read from the version's value (A-647 part 3): any version's,
  /// not only the newest.
  pub fn reached(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Positions, StoreRefused> {
    Ok(self.sets(store, green, version)?.reached)
  }

  /// `green`'s version `version`'s two sets, read from its value or, for one admitted with no walk, from the memo;
  /// refused [`StoreRefused::Deferred`] while they are not derived ([`Self::deriving`]).
  pub(super) fn sets(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Sets, StoreRefused> {
    let value = self
      .value_of(green, version)?
      .ok_or(StoreRefused::Deferred {
        green,
        version: to_u64(version),
      })?;
    Sets::read(store, &value).ok_or(StoreRefused::SetsUnreadable {
      green,
      version: to_u64(version),
    })
  }

  /// `green`'s version `version`'s value: its record's, else the memo's; none while it is not derived.
  pub(super) fn value_of(
    &self,
    green: u64,
    version: usize,
  ) -> Result<Option<ContentId>, StoreRefused> {
    let record = self.version_record(green, version)?;
    Ok(
      record
        .value
        .or_else(|| self.memo.borrow().values.get(&(green, version)).copied()),
    )
  }

  /// The set `green`'s newest version's head reaches, read from its value; `None` for a green with no version.
  fn current_reached(
    &self,
    store: &ContentStore,
    green: u64,
  ) -> Result<Option<Positions>, StoreRefused> {
    let Some(newest) = self.chains.get(&green).and_then(Chain::newest) else {
      return Ok(None);
    };
    Ok(Some(self.sets(store, green, newest)?.reached))
  }

  /// The position of `green`'s current head; none while it has none (A-722 part 4c: what a move admitted before its
  /// append must still be from).
  pub fn current_head(&self, green: u64) -> Option<u64> {
    self.chains.get(&green).and_then(Chain::head)
  }

  /// The greens with a version whose head is the object `head`, each with its newest such version, in key order
  /// (goal 93): one look-up of the index and one of the holders' map, whatever the number of greens. None where the
  /// index holds no such object or no version names it.
  pub fn holders(&self, store: &ContentStore, head: &ObjectId) -> Vec<(u64, usize)> {
    self
      .indexed(store, head)
      .and_then(|(row, _)| self.holders.get(&row.position))
      .map(|holders| {
        holders
          .iter()
          .map(|(green, version)| (*green, *version))
          .collect()
      })
      .unwrap_or_default()
  }

  /// The head of `green`'s version `version`; `None` for a deleted ref.
  pub fn head(&self, store: &ContentStore, green: u64, version: usize) -> Option<ObjectId> {
    let position = self.chains.get(&green)?.versions.get(version)?.head?;
    self.id_at(store, position)
  }

  /// The root tree of `green`'s version `version`, read from its head's commit row.
  pub fn tree(&self, store: &ContentStore, green: u64, version: usize) -> Option<ObjectId> {
    let head = self.head(store, green, version)?;
    let commit = self.object(store, &head)?.commit?;
    self.id_at(store, commit.tree)
  }

  /// The entry `path` (`/`-separated) names in `green`'s version `version`, resolved one tree a component down: no walk
  /// of history and none of the tree. `None` when the version has no tree or the path names nothing.
  pub fn entry(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
    path: &[u8],
  ) -> Result<Option<Found>, StoreRefused> {
    let Some(mut tree) = self.tree(store, green, version) else {
      return Ok(None);
    };
    let mut found = None;
    let mut names = path
      .split(|byte| *byte == b'/')
      .filter(|name| !name.is_empty())
      .peekable();
    while let Some(name) = names.next() {
      let Some((entry, is_dir)) = self.entry_in(store, &tree, name)? else {
        return Ok(None);
      };
      if names.peek().is_some() && !is_dir {
        return Ok(None);
      }
      tree = entry.id;
      found = Some(entry);
    }
    Ok(found)
  }

  /// The entry named `name` in the indexed tree `tree`, and whether it is a subtree.
  fn entry_in(
    &self,
    store: &ContentStore,
    tree: &ObjectId,
    name: &[u8],
  ) -> Result<Option<(Found, bool)>, StoreRefused> {
    let (row, content) = self
      .indexed(store, tree)
      .ok_or(StoreRefused::Missing { id: *tree })?;
    let bytes = self
      .row_bytes(store, &row, content)
      .ok_or(StoreRefused::Unreadable { id: *tree })?;
    let malformed = StoreRefused::Malformed { id: *tree };
    let parsed =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    let Some(entry) = parsed.entries().iter().find(|entry| entry.name == name) else {
      return Ok(None);
    };
    let id = entry.object_id(self.format).ok_or(malformed)?;
    Ok(Some((
      Found {
        mode: entry.mode,
        id,
      },
      entry.is_dir(),
    )))
  }

  /// A fork of the repository as it stands now (A-745, §4.17.1 "A fetch's answer" rule 4: a fetch reads the index as it
  /// stood when it began): the index forked (its rows as they stand, their tables shared), its versions, base packs,
  /// heads and landings copied, and every content they name held once more, so whatever lands, is re-based or rebuilt
  /// in the repository meanwhile, the fork reads as this generation did, until [`Repository::release`] gives back what
  /// it holds. Its records charge the store as the repository's do. Refused, everything it took is given back.
  pub fn fork(&mut self, store: &mut ContentStore) -> Result<Repository, StoreRefused> {
    let charge = self.charged.saturating_add(self.landings_charged);
    store.charge(charge).map_err(StoreRefused::from)?;
    let index = match self.index.fork(store) {
      Ok(index) => index,
      Err(refused) => {
        let _ = store.discharge(charge);
        return Err(StoreRefused::from(refused));
      }
    };
    let named: Vec<ContentId> = self
      .chains
      .values()
      .flat_map(|chain| {
        chain
          .kept()
          .map(|kept| kept.content)
          .chain(chain.versions.iter().filter_map(|version| version.value))
      })
      .chain(self.memo.borrow().values.values().copied())
      .collect();
    if let Err(refused) = hold_all(store, &named) {
      let _ = index.release(store);
      let _ = store.discharge(charge);
      return Err(refused);
    }
    Ok(Repository {
      format: self.format,
      index,
      next_position: self.next_position,
      chains: self.chains.clone(),
      unvalued: self.unvalued,
      heads: self.heads.clone(),
      peeled: self.peeled.clone(),
      holders: self.holders.clone(),
      holder_entries: self.holder_entries,
      charged: self.charged,
      chains_charged: self.chains_charged,
      head_entries: self.head_entries,
      largest_pack: self.largest_pack,
      landings: self.landings.clone(),
      kinds: self.kinds.clone(),
      packs: self.packs.clone(),
      reading: RefCell::new(Scratch::default()),
      last_deltas: std::cell::Cell::new(0),
      last_found_kept: std::cell::Cell::new(false),
      last_inflated_base: std::cell::Cell::new(false),
      waiting: Cell::new(None),
      quiet: Cell::new(None),
      largest: self.largest,
      landings_charged: self.landings_charged,
      // The fork reads its landings as they stand; the repository maps the unsettled ones, never the fork.
      unsettled: VecDeque::new(),
      settling: None,
      counters: Cell::new(StoreCounters::default()),
      window: self.window.clone(),
      levels: self.levels.clone(),
      depth: self.depth,
      memo: RefCell::new(self.memo.borrow().clone()),
    })
  }

  /// The repository's shared index (its object and position rows), for its owner shard's upkeep: the merge of its runs,
  /// timed against its lookups (goal 93 perf; the server's `vcs_store::index_upkeep`).
  pub fn rows(&mut self) -> &mut Index {
    &mut self.index
  }

  /// Where the repository stands (A-745 rule 4): the positions it has given and the versions its greens have, both only
  /// ever growing, so every landing moves it and two moments with the same stamp read the same objects, refs' heads and
  /// versions. A fork taken at a stamp serves every fetch that begins at it.
  pub fn stamp(&self) -> (u64, u64) {
    let versions = self.chains.values().fold(0_u64, |count, chain| {
      count.saturating_add(u64::try_from(chain.versions.len()).unwrap_or(u64::MAX))
    });
    (self.next_position, versions)
  }

  /// Gives back everything the repository holds: its rows' holds, their tables, its versions' values and their charge.
  /// It goes on past a refusal and answers the first.
  pub fn release(self, store: &mut ContentStore) -> Result<(), StoreRefused> {
    // The objects its reads kept (a fetch's fork widens its cache as its walk opens trees): given back with it.
    self.reading.borrow_mut().recent().release(store);
    let mut first = self.index.release(store).map_err(StoreRefused::from);
    for kept in self.chains.values().flat_map(Chain::kept) {
      first = first.and(
        store
          .release(&kept.content)
          .map(drop)
          .map_err(StoreRefused::from),
      );
    }
    let values: Vec<ContentId> = self
      .chains
      .values()
      .flat_map(|chain| chain.versions.iter().filter_map(|version| version.value))
      .chain(self.memo.borrow().values.values().copied())
      .collect();
    first = first.and(give_back(store, &values));
    first.and(
      store
        .discharge(self.charged.saturating_add(self.landings_charged))
        .map_err(StoreRefused::from),
    )
  }
}

/// One landing's entry in the landing map (A-722: the version that brought each position, kept beside the rows, which
/// no longer carry it), keyed by its first position.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Landed {
  /// Mapped by position: runs `[start, end)`, ascending and apart, each with the green and the version that brought it.
  Runs(Vec<LandedRun>),
  /// As a commit of several moves leaves it, before its turns map it ([`Repository::settle_landings_turn`]).
  Moves(Box<LandedMoves>),
}

/// A landing's run: its first position, its end (exclusive), and the green and version that brought it.
type LandedRun = (u64, u64, u64, u64);

/// A landing of several moves as its commit leaves it ([`Landed::Moves`]): its end (exclusive), each move's green and
/// version with the set its walk brought, in the commit's order, and the first move's pair, for a position none holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LandedMoves {
  end: u64,
  brought: Vec<((u64, u64), Positions)>,
  otherwise: (u64, u64),
}

impl LandedMoves {
  /// The green and version that brought `position`, within the landing: the first move whose set holds it, else the
  /// first move's (A-722's rule, as the runs map it).
  fn bringer(&self, position: u64) -> (u64, u64) {
    self
      .brought
      .iter()
      .find(|(_, set)| set.contains(position))
      .map_or(self.otherwise, |(pair, _)| *pair)
  }

  /// What it charges the store: each move's set by its form's bytes and its pair a run's record.
  fn cost(&self) -> u64 {
    self.brought.iter().fold(
      bytes(self.brought.len(), LANDING_BYTES),
      |total, (_, set)| total.saturating_add(set.bytes()),
    )
  }
}

impl Landed {
  /// The green and version that brought `position`, if this landing gave it.
  fn bringer(&self, (base, position): (u64, u64)) -> Option<(u64, u64)> {
    match self {
      Landed::Runs(runs) => {
        let after = runs.partition_point(|run| run.0 <= position);
        let (_, end, green, version) = runs.get(after.checked_sub(1)?)?;
        (position < *end).then_some((*green, *version))
      }
      Landed::Moves(moves) => {
        (base <= position && position < moves.end).then(|| moves.bringer(position))
      }
    }
  }

  /// What it charges the store: a run's record a run, or its moves' sets ([`LandedMoves::cost`]).
  fn cost(&self) -> u64 {
    match self {
      Landed::Runs(runs) => bytes(runs.len(), LANDING_BYTES),
      Landed::Moves(moves) => moves.cost(),
    }
  }
}

/// A landing's mapping under way ([`Repository::settle_landings_turn`]): its first position, the next to map, and the
/// runs mapped so far.
#[derive(Debug)]
struct Settling {
  base: u64,
  next: u64,
  runs: Vec<LandedRun>,
}

impl Settling {
  /// Maps `position`, the next, to the green and `version` that brought it: the last run grown where it ends there with
  /// the same pair, else a run of its own.
  fn map(&mut self, position: u64, (green, version): (u64, u64)) {
    let after = position.saturating_add(1);
    match self.runs.last_mut() {
      Some((_, end, run_green, run_version))
        if *end == position && (*run_green, *run_version) == (green, version) =>
      {
        *end = after;
      }
      _ => self.runs.push((position, after, green, version)),
    }
    self.next = after;
  }
}

/// What one unit of a landing's mapping did ([`Repository::settle_landings_turn`]).
enum SettleUnit {
  /// None is left to map.
  Done,
  /// A position mapped.
  Mapped,
  /// A landing's runs put in place, or an entry no longer of moves passed over.
  Settled,
}

/// What a commit makes before its stage is adopted ([`Repository::prepared`]): the landing map's entry, each move's
/// planned version and its value, the head map's entries once moved, and what was charged.
struct Prepared {
  landed: Option<Landed>,
  planned: Vec<Planned>,
  values: Vec<Option<ContentId>>,
  entries: usize,
  charge: Charge,
}

/// What a commit charged the store before its stage was adopted: its records' difference at the bound, its landing
/// map's runs, and the bytes its values added.
struct Charge {
  records: u64,
  landings: u64,
  values: u64,
}

/// Shape: what a commit reads of the repository's records for each move it plans: its green's newest version, and the
/// head entries it moves out and in.
const RECORDS_A_MOVE: usize = 3;

/// A move's version as its commit plans it: its green, its head's position, how its reached set followed, and its sets,
/// which become its value once the commit puts them; none for a version left for its first reader (A-614 amended).
struct Planned {
  green: u64,
  /// Its green's head before the move ([`Move::from_head`]).
  from: Option<u64>,
  head: Option<u64>,
  followed: Followed,
  sets: Option<Sets>,
}

/// Puts each planned version's sets as its value (A-647 part 3), held once for its record, in order, none for a version
/// left for its first reader; refused, every value put is given back.
fn put_values(
  store: &mut ContentStore,
  planned: &[Planned],
) -> Result<Vec<Option<ContentId>>, StoreRefused> {
  let shape = reach::shape_of(store.window())?;
  let mut values = Vec::with_capacity(planned.len());
  for version in planned {
    let Some(sets) = &version.sets else {
      values.push(None);
      continue;
    };
    match sets.put(store, shape) {
      Ok(value) => values.push(Some(value)),
      Err(refused) => {
        give_back(store, &values.iter().flatten().copied().collect::<Vec<_>>())?;
        return Err(refused.into());
      }
    }
  }
  Ok(values)
}

/// Holds every one of `ids` in `store`; refused one, gives back those it held and answers the refusal.
fn hold_all(store: &mut ContentStore, ids: &[ContentId]) -> Result<(), StoreRefused> {
  for (at, id) in ids.iter().enumerate() {
    if let Err(refused) = store.hold(id) {
      let _ = give_back(store, ids.get(..at).unwrap_or_default());
      return Err(StoreRefused::from(refused));
    }
  }
  Ok(())
}

/// Gives back one hold of each of `values`; past a refusal the rest still, and the first refusal is the answer.
fn give_back(store: &mut ContentStore, values: &[ContentId]) -> Result<(), StoreRefused> {
  let mut first = Ok(());
  for value in values {
    if let Err(refused) = store.release(value) {
      first = first.and(Err(StoreRefused::from(refused)));
    }
  }
  first
}

/// What a version records of its move's [`Reach`].
fn followed(reach: &Reach) -> Followed {
  match reach {
    Reach::Grown | Reach::GrownUnwalked => Followed::Grown,
    Reach::Remade { from } => Followed::Remade { base: *from },
    Reach::Emptied => Followed::Emptied,
  }
}

/// Moves `green` in the head map from `from` to `to`.
fn move_head(
  heads: &mut BTreeMap<u64, BTreeSet<u64>>,
  green: u64,
  from: Option<u64>,
  to: Option<u64>,
) {
  if let Some(from) = from
    && let Some(greens) = heads.get_mut(&from)
  {
    greens.remove(&green);
    if greens.is_empty() {
      heads.remove(&from);
    }
  }
  if let Some(to) = to {
    heads.entry(to).or_default().insert(green);
  }
}

/// What `chains` (the chains' footprints summed) and the head map's, the peeled map's and the holders' map's entries
/// charge the store.
fn charge_of(chains: u64, (heads, peeled, holders): (usize, usize, usize)) -> u64 {
  chains
    .saturating_add(bytes(heads, HEAD_BYTES))
    .saturating_add(bytes(peeled, PEELED_BYTES))
    .saturating_add(bytes(holders, HOLDER_BYTES))
}

/// The chains' footprints summed and the head map's entries, recomputed whole: the oracle of the commit's and the
/// recharge's kept sums (P2).
#[cfg(test)]
fn charged_whole(chains: &BTreeMap<u64, Chain>, heads: &HeadMap) -> (u64, usize) {
  let footprints = chains.values().fold(0_u64, |total, chain| {
    total.saturating_add(chain.footprint())
  });
  let entries = heads
    .values()
    .fold(0_usize, |total, greens| total.saturating_add(greens.len()));
  (footprints, entries)
}

/// Derived: what one green of the holders' map charges the store: its key and its version, two words, the commit's
/// position each map is under counted with them as the peeled map counts it.
const HOLDER_BYTES: usize = size_of::<(u64, (u64, usize))>();

/// Derived: what one entry of the peeled map charges the store: a commit's position, a green and a version, three
/// words.
const PEELED_BYTES: usize = size_of::<(u64, (u64, usize))>();

/// Derived: what one run of the landing map charges the store: its key and its value, four words.
const LANDING_BYTES: usize = size_of::<(u64, (u64, u64, u64))>();

/// For each commit some green's head names, by its position, those greens: the shape the store's oracle compares
/// ([`CommitState`]).
#[cfg(test)]
type HeadMap = BTreeMap<u64, BTreeSet<u64>>;

/// A tree's entries that are objects of this repository: name, whether a subtree, id.
pub(super) type TreeEntries = BTreeMap<Vec<u8>, (bool, ObjectId)>;

/// The slot of `green`'s version `version`'s fragments.
fn fragments_slot(
  chains: &mut BTreeMap<u64, Chain>,
  green: u64,
  version: usize,
) -> Option<&mut Option<Fragments>> {
  chains
    .get_mut(&green)
    .and_then(|chain| chain.versions.get_mut(version))
    .map(|record| &mut record.fragments)
}

/// A fragment whose bytes are kept as `content` already (A-718: put a window at a time), and what
/// its entries hold read once (`Kept`); the caller's hold on `content` becomes the fragment's.
fn keep(((bytes, placed), content): (fragment::Encoded, ContentId)) -> Result<Kept, StoreRefused> {
  let order = position_order(&placed)?;
  let positions = Positions::of(placed.iter().map(|entry| entry.position).collect());
  let references = Positions::of(
    placed
      .iter()
      .filter(|entry| entry.stored == Stored::Reference)
      .filter_map(|entry| entry.base)
      .collect(),
  );
  let offsets = placed
    .iter()
    .filter(|entry| entry.stored == Stored::Offset)
    .count();
  let runs = positions.runs().saturating_add(references.runs());
  Ok(Kept {
    content,
    len: to_u64(bytes.len()),
    placed,
    positions,
    references,
    offsets: to_u64(offsets),
    order,
    runs,
    held: 0,
  })
}

/// The indexes of `placed` in their objects' position order; refused for a part of more entries
/// than a pack's header counts.
fn position_order(placed: &[Placed]) -> Result<Vec<u32>, StoreRefused> {
  let mut order = (0..placed.len())
    .map(|index| u32::try_from(index).map_err(|_| StoreRefused::PackTooLarge))
    .collect::<Result<Vec<u32>, StoreRefused>>()?;
  order.sort_by_key(|index| {
    usize::try_from(*index)
      .ok()
      .and_then(|index| placed.get(index))
      .map_or(u64::MAX, |entry| entry.position)
  });
  Ok(order)
}

/// A version's two fragments, or a base pack's two parts, each kept as content already (its root
/// held once by the caller), recorded; refused, both roots are given back.
fn keep_both(
  store: &mut ContentStore,
  objects: (fragment::Encoded, ContentId),
  blobs: (fragment::Encoded, ContentId),
) -> Result<Fragments, StoreRefused> {
  let roots = [objects.1, blobs.1];
  match (keep(objects), keep(blobs)) {
    (Ok(mut objects), Ok(mut blobs)) => {
      for kept in [&mut objects, &mut blobs] {
        kept.held = store.tree_bytes(&kept.content);
      }
      Ok(Fragments { objects, blobs })
    }
    (Err(refused), _) | (_, Err(refused)) => {
      for root in roots {
        store.release(&root)?;
      }
      Err(refused)
    }
  }
}

/// The fragments' part (A-614 part 2; module docs and `fragment`'s).
impl Repository {
  /// A version's record, or why there is none.
  fn version_record(&self, green: u64, version: usize) -> Result<&Version, StoreRefused> {
    self
      .chains
      .get(&green)
      .and_then(|chain| chain.versions.get(version))
      .ok_or(StoreRefused::NoVersion {
        green,
        version: to_u64(version),
      })
  }

  /// An indexed object's type and bytes.
  pub(super) fn read(
    &self,
    store: &ContentStore,
    id: &ObjectId,
  ) -> Result<(Kind, Vec<u8>), StoreRefused> {
    let (row, content) = self
      .indexed(store, id)
      .ok_or(StoreRefused::Missing { id: *id })?;
    let bytes = self
      .row_bytes(store, &row, content)
      .ok_or(StoreRefused::Unreadable { id: *id })?
      .into_owned();
    Ok((row.kind, bytes))
  }

  /// A tree's entries that are objects of this repository.
  pub(super) fn tree_entries(
    &self,
    store: &ContentStore,
    tree: &ObjectId,
  ) -> Result<TreeEntries, StoreRefused> {
    let (_, bytes) = self.read(store, tree)?;
    let malformed = StoreRefused::Malformed { id: *tree };
    let parsed =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    let mut entries = TreeEntries::new();
    for entry in parsed
      .entries()
      .iter()
      .filter(|entry| entry.kind() != Kind::Commit)
    {
      let id = entry.object_id(self.format).ok_or(malformed)?;
      entries.insert(entry.name.to_vec(), (entry.is_dir(), id));
    }
    Ok(entries)
  }

  /// Counts a kept fragment's entries by how each holds its object.
  fn count_entries(&self, placed: &[Placed]) {
    for entry in placed {
      self.count(|counters| match entry.stored {
        Stored::Whole => counters.whole = counters.whole.saturating_add(1),
        Stored::Offset => counters.offset_deltas = counters.offset_deltas.saturating_add(1),
        Stored::Reference => {
          counters.reference_deltas = counters.reference_deltas.saturating_add(1);
        }
      });
    }
  }

  /// What `green`'s chain charges the store now ([`Chain::footprint`]); none for a green with no chain.
  fn chain_footprint(&self, green: u64) -> u64 {
    self.chains.get(&green).map_or(0, Chain::footprint)
  }

  /// Charges what the chains and the maps take now against what was charged, `green`'s chain changed alone since it
  /// charged `before`: the chains' kept sum moved by that chain's difference, never every chain summed again (P2).
  /// Refused, nothing of the charge changed.
  fn recharge(
    &mut self,
    store: &mut ContentStore,
    (green, before): (u64, u64),
  ) -> Result<(), StoreRefused> {
    let chains = self
      .chains_charged
      .saturating_sub(before)
      .saturating_add(self.chain_footprint(green));
    let total = charge_of(
      chains,
      (self.head_entries, self.peeled.len(), self.holder_entries),
    );
    if total > self.charged {
      store.charge(total.saturating_sub(self.charged))?;
    } else {
      store.discharge(self.charged.saturating_sub(total))?;
    }
    self.charged = total;
    self.chains_charged = chains;
    Ok(())
  }

  /// Encodes `green`'s version `version` as its two fragments (`fragment`'s docs), kept as
  /// content, in one call: an [`Encoding`] driven to its end in one turn, for tests and tools. The
  /// owner encodes a version a turn at a time under its quantum ([`Encoding::advance`], A-653
  /// continued (2)), off the commit's answer's path. A version is encoded once. Refused, nothing is
  /// kept.
  pub fn encode_fragments(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    clock: &mut dyn Clock,
    exchange: Exchange,
  ) -> Result<(), StoreRefused> {
    let encoding = Encoding::fragments(self, store, green, version)?;
    encoding.finish(self, store, clock, exchange)
  }

  /// The first version whose fragments are not kept yet, by green and then version (A-718; §4.17.2
  /// "A push" item 8): what the owner encodes next, so each version a push made is encoded once and
  /// every fetch after copies its fragments rather than searching its objects for deltas again.
  /// None once every version's fragments are kept.
  pub fn first_unencoded(&self) -> Option<(u64, usize)> {
    self.chains.iter().find_map(|(green, chain)| {
      chain
        .versions
        .iter()
        .position(|version| version.fragments.is_none())
        .map(|version| (*green, version))
    })
  }

  /// The bytes `green`'s version `version`'s two fragments hold, their parts' lengths as encoded (the pack bytes a clone
  /// copies of them); `None` while they are not kept.
  pub fn fragments_len(&self, green: u64, version: usize) -> Option<u64> {
    let fragments = self
      .version_record(green, version)
      .ok()?
      .fragments
      .as_ref()?;
    Some(fragments.objects.len.saturating_add(fragments.blobs.len))
  }

  /// Every version whose fragments are kept, by green and then version, with the bytes dropping
  /// them gives back: their content and what their entries are charged (A-724 piece 4): what a repository over its memory share may drop, since a version
  /// whose fragments are gone is encoded again by the next fetch (A-718), as after a recovery.
  pub fn kept_fragments(&self) -> Vec<((u64, usize), u64)> {
    self
      .chains
      .iter()
      .flat_map(|(green, chain)| {
        chain
          .versions
          .iter()
          .enumerate()
          .filter_map(move |(version, record)| {
            let fragments = record.fragments.as_ref()?;
            let bytes = [&fragments.objects, &fragments.blobs]
              .into_iter()
              .map(|kept| {
                let content = kept.held;
                let runs = kept.runs;
                content
                  .saturating_add(bytes(
                    kept.placed.len(),
                    PLACED_BYTES.saturating_add(ORDER_BYTES),
                  ))
                  .saturating_add(bytes(runs, RUN_BYTES))
              })
              .fold(0u64, u64::saturating_add);
            Some(((*green, version), bytes))
          })
      })
      .collect()
  }

  /// Drops `green`'s version `version`'s kept fragments (A-724 piece 4): their content let go and
  /// the store's charge trued up, so the next fetch encodes the version again (A-718). Whether
  /// fragments were kept to drop.
  pub fn drop_fragments(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
  ) -> Result<bool, StoreRefused> {
    let before = self.chain_footprint(green);
    let Some(fragments) = fragments_slot(&mut self.chains, green, version).and_then(Option::take)
    else {
      return Ok(false);
    };
    store.release(&fragments.objects.content)?;
    store.release(&fragments.blobs.content)?;
    self.recharge(store, (green, before))?;
    Ok(true)
  }

  /// What `green`'s version `version` brought, still to encode as its fragments; `None` once they
  /// are kept (A-653 continued (2)).
  pub(super) fn unencoded(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Option<Positions>, StoreRefused> {
    let record = self.version_record(green, version)?;
    if record.fragments.is_some() {
      return Ok(None);
    }
    Ok(Some(self.sets(store, green, version)?.brought))
  }

  /// `green`'s head version and every object it reaches, read from the version's value (A-647 part 3): what its base
  /// pack is made from.
  pub(super) fn head_reach(
    &self,
    store: &ContentStore,
    green: u64,
  ) -> Result<(usize, Positions), StoreRefused> {
    let none = StoreRefused::NoVersion { green, version: 0 };
    let chain = self.chains.get(&green).ok_or(none)?;
    let version = chain.versions.len().checked_sub(1).ok_or(none)?;
    Ok((version, self.sets(store, green, version)?.reached))
  }

  /// The encoder's learned rules, this repository's own, with the owner's exchange (its measured rates among it).
  pub(super) fn learning(&mut self, exchange: Exchange) -> Learning<'_> {
    Learning {
      window: &mut self.window,
      levels: &mut self.levels,
      depth: self.depth.limit(),
      exchange,
    }
  }

  /// Records a version's two encoded fragments, each kept as content already under its root (A-718:
  /// put a window a unit); a version whose fragments were kept meanwhile keeps those, the roots given
  /// back. Refused, nothing is kept.
  pub(super) fn keep_version(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    (objects, blobs): (
      (fragment::Encoded, ContentId),
      (fragment::Encoded, ContentId),
    ),
  ) -> Result<(), StoreRefused> {
    if self.version_record(green, version)?.fragments.is_some() {
      store.release(&objects.1)?;
      store.release(&blobs.1)?;
      return Ok(());
    }
    let fragments = keep_both(store, objects, blobs)?;
    self.install(store, (green, version), fragments)
  }

  /// Records a base pack's two encoded parts, each kept as content already under its root, in place
  /// of the old ones, made from `green`'s version `version` in `ns` of the owner's time. Refused,
  /// the old base pack stays.
  pub(super) fn keep_base(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    (objects, blobs): (
      (fragment::Encoded, ContentId),
      (fragment::Encoded, ContentId),
    ),
    ns: u64,
  ) -> Result<(), StoreRefused> {
    let parts = keep_both(store, objects, blobs)?;
    self.install_base(store, green, BasePack { version, parts }, ns)
  }

  /// Records a version's fragments and charges their entries; refused, the fragments' content is
  /// let go and nothing changed.
  fn install(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    fragments: Fragments,
  ) -> Result<(), StoreRefused> {
    let contents = [fragments.objects.content, fragments.blobs.content];
    self.count_entries(&fragments.objects.placed);
    self.count_entries(&fragments.blobs.placed);
    let before = self.chain_footprint(green);
    if let Some(kept) = fragments_slot(&mut self.chains, green, version) {
      *kept = Some(fragments);
    }
    if let Err(refused) = self.recharge(store, (green, before)) {
      if let Some(kept) = fragments_slot(&mut self.chains, green, version) {
        *kept = None;
      }
      for content in contents {
        store.release(&content)?;
      }
      return Err(refused);
    }
    self.count(|counters| counters.fragments = counters.fragments.saturating_add(2));
    Ok(())
  }

  /// Makes `green`'s base pack again from every object its current head reaches, self-contained
  /// (offset deltas only), as two parts (commits, trees and tags; then blobs), and teaches the
  /// rebuild rule its measured time, in one call: an [`Encoding`] driven to its end in one turn,
  /// for tests and tools. The owner makes it a turn at a time under its quantum
  /// ([`Encoding::advance`], A-653 continued (2)). Refused, the old base pack stays.
  pub fn rebuild_base(
    &mut self,
    store: &mut ContentStore,
    green: u64,
    clock: &mut dyn Clock,
    exchange: Exchange,
  ) -> Result<(), StoreRefused> {
    let encoding = Encoding::base(self, store, green)?;
    encoding.finish(self, store, clock, exchange)
  }

  /// Records a base pack in place of the old one, charges it, and teaches the rule its time.
  fn install_base(
    &mut self,
    store: &mut ContentStore,
    green: u64,
    base: BasePack,
    ns: u64,
  ) -> Result<(), StoreRefused> {
    let contents = [base.parts.objects.content, base.parts.blobs.content];
    let placed: Vec<Placed> = [&base.parts.objects, &base.parts.blobs]
      .into_iter()
      .flat_map(|kept| kept.placed.iter().copied())
      .collect();
    let before = self.chain_footprint(green);
    let old = self
      .chains
      .get_mut(&green)
      .and_then(|chain| chain.base.replace(base));
    if let Err(refused) = self.recharge(store, (green, before)) {
      if let Some(chain) = self.chains.get_mut(&green) {
        chain.base = old;
      }
      for content in contents {
        store.release(&content)?;
      }
      return Err(refused);
    }
    if let Some(old) = old {
      store.release(&old.parts.objects.content)?;
      store.release(&old.parts.blobs.content)?;
    }
    if let Some(chain) = self.chains.get_mut(&green) {
      chain.rebuild.rebuilt(ns);
    }
    self.count_entries(&placed);
    self.count(|counters| counters.rebuilds = counters.rebuilds.saturating_add(1));
    Ok(())
  }

  /// A clone of `green` spent `ns` serving the fragments after its base pack: the rebuild rule's
  /// rent.
  pub fn clone_took(&mut self, green: u64, ns: u64) {
    if let Some(chain) = self.chains.get_mut(&green) {
      chain.rebuild.cloned(ns);
    }
  }

  /// Whether `green`'s base pack is due to be made again (`learn`'s rebuild rule).
  pub fn rebuild_due(&self, green: u64) -> bool {
    self
      .chains
      .get(&green)
      .is_some_and(|chain| chain.rebuild.due())
  }

  /// The entries of `green`'s version `version`'s fragments: commits, trees and tags, then blobs.
  pub fn fragments(&self, green: u64, version: usize) -> Option<(&[Placed], &[Placed])> {
    let fragments = self
      .version_record(green, version)
      .ok()?
      .fragments
      .as_ref()?;
    Some((&fragments.objects.placed, &fragments.blobs.placed))
  }
}

/// What upload-pack's stream reads (A-639, `upload`): every part by green, version and class, never
/// a borrow it keeps.
impl Repository {
  /// A copy of the learned rules a stream encodes with: the window, the levels and the depth.
  pub(super) fn rules(&self) -> (Window, Levels, u64) {
    (self.window.clone(), self.levels.clone(), self.depth.limit())
  }

  /// `green`'s version `version`'s fragment of blobs, or of the other kinds, once encoded.
  pub(super) fn fragment_part(&self, green: u64, version: usize, blobs: bool) -> Option<&Kept> {
    let fragments = self
      .chains
      .get(&green)?
      .versions
      .get(version)?
      .fragments
      .as_ref()?;
    Some(if blobs {
      &fragments.blobs
    } else {
      &fragments.objects
    })
  }

  /// `green`'s base pack's part of blobs, or of the other kinds, with the version it was made at.
  pub(super) fn base_part(&self, green: u64, blobs: bool) -> Option<(usize, &Kept)> {
    let base = self.chains.get(&green)?.base.as_ref()?;
    let part = if blobs {
      &base.parts.blobs
    } else {
      &base.parts.objects
    };
    Some((base.version, part))
  }

  /// The position the next object new to the repository gets: every position below it is given
  /// (A-665: the hook's view shows every object the repository holds).
  pub(super) fn next_position(&self) -> u64 {
    self.next_position
  }

  /// Every green with a chain, in order (A-665: the hook's view weighs each one's base pack; a reader deriving every
  /// version's sets, A-614 amended).
  pub fn greens(&self) -> impl Iterator<Item = u64> + '_ {
    self.chains.keys().copied()
  }

  /// The position of `green`'s version `version`'s head; `None` for a deleted ref.
  pub(super) fn head_position(&self, green: u64, version: usize) -> Option<u64> {
    self.chains.get(&green)?.versions.get(version)?.head
  }

  /// Each green, in order, with the position of each head its versions had.
  pub(super) fn heads(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
    self.chains.iter().flat_map(|(green, chain)| {
      chain
        .versions
        .iter()
        .filter_map(move |version| Some((*green, version.head?)))
    })
  }

  /// The object at `position`: its id, its row and its root content.
  pub(super) fn stored_at(
    &self,
    store: &ContentStore,
    position: u64,
  ) -> Result<(ObjectId, ObjectRow, StoredAt), StoreRefused> {
    let id = self
      .id_at(store, position)
      .ok_or(StoreRefused::NoPosition { position })?;
    let (row, content) = self
      .indexed(store, &id)
      .ok_or(StoreRefused::Missing { id })?;
    let stored = self.stored_of(&row, content);
    Ok((id, row, stored))
  }
}
