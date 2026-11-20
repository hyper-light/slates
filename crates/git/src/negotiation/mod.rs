//! upload-pack v2's negotiation (A-615 part 2; §4.17.2 "A fetch" and "Pieces (git)" piece 7): what a `fetch` request
//! asks of a repository's commit graph, answered as pure functions over a trait the object store implements ([`Graph`];
//! A-614's store, kept behind the trait so neither module names the other).
//!
//! **What it decides.** Given the wants, the haves, `done`, `wait-for-done`, the client's shallow commits, a deepening
//! and a filter, [`negotiate`] answers what upload-pack answers: the acknowledgments (an `ACK` for every have the store
//! holds, or `NAK`) and whether it is `ready`; and, once the client said `done` or the server is ready, the shallow-info
//! section and what the pack carries: whole runs of a green's versions ([`Run`]) and single objects.
//!
//! **One round on a version head** (§4.17.2: "When one is the head of a version M on the chain of the green whose head
//! is wanted, the objects to send are D(M+1) to D(N), and the answer is `ready` in the first round"). A want that is a
//! green's head at version N, with a have that headed the same green at a version M, is answered without a walk: the run
//! of versions M+1 to N, the latest such M. It is taken only where the union rule is exact, so where versions M+1 to N
//! all fast-forwarded (§4.17.2 item 3: "The union rule holds within each run of fast-forwards"), the client is not
//! shallow and asks no deepening, and the filter decides each object by its own row (none, `blob:none`, `blob:limit`,
//! `object:type`, and `combine:` of those), so the pack can apply it to the run's fragments.
//!
//! **The walk otherwise** ("the server walks from the wants back through commit rows, by generation number, until it
//! meets commits the haves reach"). The commits to send are those reachable from the wants and from no have, found by
//! one walk in decreasing generation order that stops once every commit it holds is reached from a have
//! (the `paint` module). The trees and blobs to send are those of the sent commits' trees, less those of the
//! edge commits' trees: each have's commit the walk met beside a sent one, as git's `pack-objects` leaves out the trees of
//! `mark_edges_uninteresting` [C: `list-objects.c`, `builtin/pack-objects.c` v2.50.1, without bitmaps]. `ready` asks of
//! each want, peeled through tags, whether it reaches a have the store holds or one of its parents, as git's
//! `ok_to_give_up` asks [C: `upload-pack.c`, `commit-reach.c` `can_all_from_reach_with_flag`]; git prunes that search by
//! commit date, which clock skew can fool, where generation numbers prune it exactly.
//!
//! **Shallow** [C: `shallow.c` `get_shallow_commits` and `get_shallow_commits_by_rev_list`, `upload-pack.c` `deepen`,
//! `send_shallow` and `send_unshallow`]. `deepen <n>` cuts at the commits whose least depth from the wants is `n`;
//! `deepen-relative` measures from the client's shallow commits the refs reach; `deepen-since` and `deepen-not` cut where
//! a commit `rev-list` selects has a parent it does not. The client's shallow commits the new cut passes are
//! `unshallow`, and their parents are wanted. Every shallow commit, the client's and the new, is a graft for the walk:
//! its parents are not followed.
//!
//! **Filters** [B: `rev-list-options` "--filter"; C: `list-objects-filter.c`, `list-objects-filter-options.c`]: see
//! [`filter`]. The wants, and what their tags name, are sent whatever the filter, as git sends the objects a client
//! named. A `sparse:oid` filter is served with the patterns the server read from its blob ([`sparse`]), alone: every
//! tree the walk reaches is sent, and a blob when a path it is met at is matched (the `objects` module).
//!
//! **Bounds (§4.2, §4.9).** Every walk is iterative, never recursive, and every entry it keeps (a commit's flags, a
//! queued commit, a tree seen, an object chosen) is charged to the caller's [`Admission`] before it is kept, at its own
//! size, so a negotiation past the request's admission is refused, typed, with what it held so far dropped. No count is
//! fixed here: the request and the admission bound every walk.

pub mod filter;
mod objects;
mod one_round;
mod paint;
mod ready;
mod shallow;
pub mod sparse;

use crate::object_name::{Kind, ObjectId};
use crate::positions::Positions;
use crate::{Admission, PastAdmission};

pub use filter::Filter;
pub use sparse::Patterns;

/// Format: the depth git reads as no depth at all, its `INFINITE_DEPTH`, which `git fetch --unshallow` sends: every
/// shallow commit of the client is made whole (`upload-pack.c` `deepen`).
pub const INFINITE_DEPTH: u32 = i32::MAX.unsigned_abs();

/// What the negotiation reads of a repository's object store. A-614's store implements it over its index rows, commit
/// rows and version chains; the tests implement it over generated histories.
pub trait Graph {
  /// One object the store holds, named by the store's own handle (its position in the repository's index), so a walk
  /// keeps handles rather than names.
  /// Its position in the index is its number (A-745): the answer carries the objects it sends as runs of positions
  /// ([`Positions`]), never one at a time.
  type Node: Copy + Ord + core::fmt::Debug + Into<u64> + TryFrom<u64>;
  /// A green, one branch of the repository, whose versions each have a head.
  type Green: Copy + Ord + core::fmt::Debug;

  /// The object `id` names, when the store holds it.
  fn node(&self, id: &ObjectId) -> Option<Self::Node>;
  /// The name of `node`.
  fn id(&self, node: Self::Node) -> ObjectId;
  /// What kind of object `node` is.
  fn kind(&self, node: Self::Node) -> Kind;
  /// A commit's row; `None` for an object that is not a commit.
  fn commit(&self, node: Self::Node) -> Option<CommitRow<Self::Node>>;
  /// A commit's parents, in order; none for an object that is not a commit.
  fn parents(&self, commit: Self::Node) -> impl Iterator<Item = Self::Node>;
  /// A tree's entries the store holds: its subtrees and blobs, never a submodule's commit (a gitlink, which no
  /// repository but the submodule's holds).
  fn entries(&self, tree: Self::Node) -> impl Iterator<Item = Self::Node>;
  /// The same entries, each with its kind: by default each entry's own ([`Graph::kind`]); a store reads them from the
  /// tree's modes, as git's walk does (`040000` a tree, else a blob), so a walk asks no row of an entry for its kind.
  fn typed_entries(&self, tree: Self::Node) -> impl Iterator<Item = (Self::Node, Kind)> {
    self.entries(tree).map(|entry| (entry, self.kind(entry)))
  }
  /// The same entries, in the tree's order, each with its name: what a sparse filter asks its patterns of (A-640
  /// part 4).
  fn named_entries(&self, tree: Self::Node) -> impl Iterator<Item = (Self::Node, Vec<u8>)>;
  /// A blob's size in bytes.
  fn size(&self, blob: Self::Node) -> u64;
  /// What an annotated tag names, when the store holds it.
  fn target(&self, tag: Self::Node) -> Option<Self::Node>;
  /// What every ref names (`deepen-relative` measures from the client's shallow commits these reach).
  fn tips(&self) -> impl Iterator<Item = Self::Node>;
  /// The annotated tags the refs under `refs/tags/` name (`include-tag` sends those whose object the pack carries).
  fn tags(&self) -> impl Iterator<Item = Self::Node>;
  /// The greens whose current head is `head`, each with its current version.
  fn greens_at(&self, head: Self::Node) -> impl Iterator<Item = (Self::Green, u64)>;
  /// The versions of `green` whose head was `head`.
  fn versions_at(&self, green: Self::Green, head: Self::Node) -> impl Iterator<Item = u64>;
  /// Whether version `version` of `green` fast-forwarded the version before it: its head descends from that one's.
  fn fast_forward(&self, green: Self::Green, version: u64) -> bool;
  /// D(k): the objects version `version` of `green` brought, reachable from its head and not from the head before.
  fn brought(&self, green: Self::Green, version: u64) -> impl Iterator<Item = Self::Node>;
  /// The same, as runs of positions (A-745: `include-tag` asks a run's objects whether they hold a tag's end, a version
  /// a unit, never an object a unit). A store that keeps D(k) as runs gives them as kept.
  fn brought_set(&self, green: Self::Green, version: u64) -> Positions {
    Positions::of(self.brought(green, version).map(Into::into).collect())
  }
  /// What every ref names, from the ref at index `from` on, each with its ref's index (A-745: a walk over the refs
  /// keeps its place by index across steps). A store that keeps its refs in a list starts at `from` at once.
  fn tips_from(&self, from: usize) -> impl Iterator<Item = (usize, Self::Node)> {
    self.tips().enumerate().skip(from)
  }
  /// The annotated tags under `refs/tags/`, from the ref at index `from` on, each with its ref's index, as
  /// [`Graph::tips_from`] walks the refs.
  fn tags_from(&self, from: usize) -> impl Iterator<Item = (usize, Self::Node)> {
    self.tags().enumerate().skip(from)
  }
}

/// What a commit's row holds of what the negotiation reads (§4.17.2 "The repository in git's terms", item 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitRow<N> {
  /// Its generation number: greater than every parent's (git's topological level or corrected commit date).
  pub generation: u64,
  /// Its committer date, in seconds since the epoch, as `deepen-since` compares it.
  pub date: u64,
  /// Its root tree.
  pub tree: N,
}

/// How a request deepens a shallow history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deepen<'r> {
  /// `deepen <depth>`: a history this many commits deep; measured from the client's shallow commits under
  /// `deepen-relative`. [`INFINITE_DEPTH`] makes every shallow commit whole.
  Depth {
    /// The depth.
    depth: u32,
    /// `deepen-relative`.
    relative: bool,
  },
  /// `deepen-since` and `deepen-not`: a history cut at commits older than a time, at what other commits reach, or both.
  Cut {
    /// `deepen-since <time>`.
    since: Option<u64>,
    /// `deepen-not <rev>`, each resolved to the object its ref names.
    not: &'r [ObjectId],
  },
}

/// A `fetch` request, as the negotiation reads it: the server has resolved each `want-ref` to its id and each
/// `deepen-not` to the object its ref names.
#[derive(Clone, Copy, Debug)]
pub struct Request<'r> {
  /// The objects wanted.
  pub wants: &'r [ObjectId],
  /// The objects the client says it has.
  pub haves: &'r [ObjectId],
  /// `done`: no more negotiation; send the pack.
  pub done: bool,
  /// `wait-for-done`: never answer `ready`.
  pub wait_for_done: bool,
  /// The client's shallow commits.
  pub shallow: &'r [ObjectId],
  /// The deepening asked, if any.
  pub deepen: Option<Deepen<'r>>,
  /// The filter asked, if any.
  pub filter: Option<&'r Filter>,
  /// The patterns of the filter's `sparse:oid`, read from its blob by the server (A-640 part 4); a `sparse:oid` filter
  /// without them is refused.
  pub sparse: Option<&'r Patterns>,
  /// `include-tag`: send the annotated tags of objects the pack carries.
  pub include_tag: bool,
  /// `thin-pack`: the client takes a pack whose deltas name bases it holds. In a shallow exchange git's server then
  /// leaves out every had commit's tree, not the edges' alone (`pack-objects --objects-edge-aggressive`), and so does
  /// this.
  pub thin: bool,
}

/// The acknowledgments section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acknowledgments {
  /// The haves the store holds, each once, in the order sent: an `ACK` each, or `NAK` when there are none.
  pub common: Vec<ObjectId>,
  /// Whether the server is ready: a pack follows in this response.
  pub ready: bool,
}

/// The shallow-info section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShallowInfo {
  /// `shallow <id>`: commits whose parents the pack leaves out, in id order.
  pub shallow: Vec<ObjectId>,
  /// `unshallow <id>`: commits the client held shallow whose parents the pack now brings, in id order.
  pub unshallow: Vec<ObjectId>,
}

/// A run of a green's versions the pack carries whole, from their fragments: versions `after + 1` to `through`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Run<G> {
  /// The green.
  pub green: G,
  /// The version the client holds the head of.
  pub after: u64,
  /// The version wanted, the green's current one.
  pub through: u64,
}

/// What a pack carries, and the sections before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pack<G, N> {
  /// The acknowledgments, `ready` among them; `None` after `done`, which omits the section.
  pub acknowledgments: Option<Acknowledgments>,
  /// The shallow-info section, when the client is shallow or asked a deepening.
  pub shallow_info: Option<ShallowInfo>,
  /// Runs of versions sent whole; the request's filter applies to their objects.
  pub runs: Vec<Run<G>>,
  /// Objects sent one by one, by position, as runs (A-745: a fetch holds what it sends as runs, which one push's objects
  /// make one of, never an object at a time); `N` names the graph's objects elsewhere in the answer.
  pub objects: Positions,
  /// The graph's objects, named in the answer's other sections.
  pub(crate) _node: core::marker::PhantomData<N>,
}

/// What a `fetch` request is answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer<G, N> {
  /// Nothing: a request with no wants and no `wait-for-done`, which git answers with nothing at all.
  Nothing,
  /// The acknowledgments section alone: the client sends another round.
  Acknowledgments(Acknowledgments),
  /// A pack, and the sections before it.
  Pack(Pack<G, N>),
}

/// How a negotiation went, for a test to see which path each want took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
  /// Wants answered by a run of versions, in one round.
  pub one_round: u64,
  /// Commits the walk took from its queue.
  pub walked: u64,
  /// Commits the search for `ready` visited.
  pub searched: u64,
  /// Trees the object enumeration opened.
  pub trees: u64,
}

/// What a negotiation answered, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome<G, N> {
  /// The answer.
  pub answer: Answer<G, N>,
  /// How it was found.
  pub counters: Counters,
}

/// Why a request cannot be answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A want the store does not hold (git's "not our ref").
  NotOurs {
    /// The want.
    id: ObjectId,
  },
  /// A client's shallow line naming an object that is not a commit (git's "invalid shallow object").
  ShallowNotCommit {
    /// The object.
    id: ObjectId,
  },
  /// A `deepen-not` naming an object the store does not hold.
  DeepenNotHeld {
    /// The object.
    id: ObjectId,
  },
  /// `deepen-since` or `deepen-not` selected no commit (git's "no commits selected for shallow requests").
  NothingSelected,
  /// A deepening of depth zero, which git refuses ("Invalid deepen").
  ZeroDepth,
  /// The filter is one this negotiation does not serve.
  Filter(filter::Refusal),
  /// What the negotiation would keep is past the caller's admission.
  PastAdmission(PastAdmission),
}

/// Charges one kept entry of type `T` to `admission`.
fn charge<T>(admission: &mut Admission) -> Result<(), Refusal> {
  admission
    .charge_items::<T>(1)
    .map_err(Refusal::PastAdmission)
}

/// A set of the graph's objects a walk keeps, by position (A-745, §4.17.1 "A fetch's answer" rule 1: a fetch's working
/// set is a bit or a word an object at most, never an entry an object), in whichever of two forms is smaller, as
/// Roaring keeps each container [A: Lemire, Ssi-Yan-Kai and Kaser, "Consistently faster and smaller compressed bitmaps
/// with Roaring", 2016]: runs of positions ([`Positions`]), a few words for one push's objects whatever their number;
/// or a bit for each position across the set's span, where the runs would cost more (a push's commits interleaved with
/// its trees and blobs, every third position). Charged as it grows past what was charged before.
#[derive(Debug, Default)]
pub(super) struct Held {
  /// The set, in its form.
  form: Form,
  /// The most bytes it has cost, each charged once.
  charged: u64,
}

/// The two forms a [`Held`] keeps its positions in.
#[derive(Debug)]
enum Form {
  /// Runs of positions.
  Runs(Positions),
  /// A bit for each position from `start`, a word for each 64 of them.
  Bits {
    /// The position of the first word's lowest bit.
    start: u64,
    /// The words.
    words: Vec<u64>,
  },
}

impl Default for Form {
  fn default() -> Form {
    Form::Runs(Positions::new())
  }
}

/// Format: the positions one word of a bit table holds.
const WORD_BITS: u64 = u64::BITS as u64;

/// Format: the bytes one run of positions costs.
const RUN_BYTES: u64 = (2 * core::mem::size_of::<u64>()) as u64;

impl Held {
  /// Adds `node`: whether it was not held, what the set costs past what was charged before charged now.
  pub(super) fn insert<N: Into<u64>>(
    &mut self,
    node: N,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let position = node.into();
    let new = match &mut self.form {
      Form::Runs(runs) => runs.insert_new(position),
      Form::Bits { start, words } => set_bit(start, words, position),
    };
    if new {
      self.choose_form();
      let bytes = self.bytes();
      if bytes > self.charged {
        admission
          .charge(bytes.saturating_sub(self.charged))
          .map_err(Refusal::PastAdmission)?;
        self.charged = bytes;
      }
    }
    Ok(new)
  }

  /// Whether `node` is held.
  pub(super) fn contains<N: Into<u64>>(&self, node: N) -> bool {
    let position = node.into();
    match &self.form {
      Form::Runs(runs) => runs.contains(position),
      Form::Bits { start, words } => bit(*start, words, position),
    }
  }

  /// The least position held at `from` or after it.
  pub(super) fn next_from(&self, from: u64) -> Option<u64> {
    match &self.form {
      Form::Runs(runs) => runs.next_from(from),
      Form::Bits { start, words } => {
        let mut at = from.max(*start);
        loop {
          let offset = at.saturating_sub(*start);
          let word_at = usize::try_from(offset / WORD_BITS).ok()?;
          let word = *words.get(word_at)?;
          let shifted = word
            .checked_shr(u32::try_from(offset % WORD_BITS).unwrap_or(0))
            .unwrap_or(0);
          if shifted != 0 {
            return Some(at.saturating_add(u64::from(shifted.trailing_zeros())));
          }
          at = start.saturating_add(
            crate::to_u64(word_at)
              .saturating_add(1)
              .saturating_mul(WORD_BITS),
          );
        }
      }
    }
  }

  /// The positions held, ascending.
  pub(super) fn iter(&self) -> impl Iterator<Item = u64> + '_ {
    let mut next = self.next_from(0);
    core::iter::from_fn(move || {
      let at = next?;
      next = self.next_from(at.saturating_add(1));
      Some(at)
    })
  }

  /// The positions held, the set given up: its bit table handed over as it is, never turned back into a run a position
  /// (A-745 item 4: a plan of scattered positions costs at most what each object's handle cost).
  pub(super) fn into_positions(self) -> Positions {
    match self.form {
      Form::Runs(runs) => runs,
      Form::Bits { start, words } => Positions::of_bits(start, words),
    }
  }

  /// What the set costs in its form.
  fn bytes(&self) -> u64 {
    match &self.form {
      Form::Runs(runs) => RUN_BYTES.saturating_mul(crate::to_u64(runs.runs())),
      Form::Bits { words, .. } => {
        crate::to_u64(core::mem::size_of::<u64>()).saturating_mul(crate::to_u64(words.len()))
      }
    }
  }

  /// Keeps the set in runs while they cost no more than a bit table over its span would, in bits once they cost more.
  fn choose_form(&mut self) {
    let Form::Runs(runs) = &self.form else {
      return;
    };
    let (Some(&(first, _)), Some(&(_, end))) = (runs.run_pairs().first(), runs.run_pairs().last())
    else {
      return;
    };
    let span_words = end.saturating_sub(first).div_ceil(WORD_BITS);
    let bits_bytes = span_words.saturating_mul(crate::to_u64(core::mem::size_of::<u64>()));
    if RUN_BYTES.saturating_mul(crate::to_u64(runs.runs())) <= bits_bytes {
      return;
    }
    let start = first.saturating_sub(first % WORD_BITS);
    let mut words = Vec::new();
    let mut start_at = start;
    for position in runs.iter() {
      set_bit(&mut start_at, &mut words, position);
    }
    self.form = Form::Bits {
      start: start_at,
      words,
    };
  }
}

/// Whether the bit table of `words` from `start` holds `position`.
fn bit(start: u64, words: &[u64], position: u64) -> bool {
  let Some(offset) = position.checked_sub(start) else {
    return false;
  };
  let Ok(word_at) = usize::try_from(offset / WORD_BITS) else {
    return false;
  };
  words
    .get(word_at)
    .is_some_and(|word| word & (1u64 << (offset % WORD_BITS)) != 0)
}

/// Sets `position`'s bit in the table of `words` from `start`, growing it either way to hold it (`start` stays a word's
/// first position): whether it was not set.
fn set_bit(start: &mut u64, words: &mut Vec<u64>, position: u64) -> bool {
  let aligned = position.saturating_sub(position % WORD_BITS);
  if words.is_empty() {
    *start = aligned;
  }
  if aligned < *start {
    let more = usize::try_from(start.saturating_sub(aligned) / WORD_BITS).unwrap_or(0);
    words.splice(0..0, core::iter::repeat_n(0, more));
    *start = aligned;
  }
  let offset = position.saturating_sub(*start);
  let Ok(word_at) = usize::try_from(offset / WORD_BITS) else {
    return false;
  };
  if word_at >= words.len() {
    words.resize(word_at.saturating_add(1), 0);
  }
  let mask = 1u64 << (offset % WORD_BITS);
  match words.get_mut(word_at) {
    Some(word) if *word & mask == 0 => {
      *word |= mask;
      true
    }
    _ => false,
  }
}

/// What a walk's transient list (a stack, a queue) has cost: its most entries, each charged once as the list first grows
/// past it (A-745: a walk's stack is charged at its deepest, never once an entry pushed over the walk).
#[derive(Debug, Default)]
pub(super) struct Peak {
  /// The most entries charged.
  charged: usize,
}

impl Peak {
  /// Charges `len` entries of `T` past the most charged before.
  pub(super) fn reach<T>(&mut self, len: usize, admission: &mut Admission) -> Result<(), Refusal> {
    if len > self.charged {
      admission
        .charge_items::<T>(len.saturating_sub(self.charged))
        .map_err(Refusal::PastAdmission)?;
      self.charged = len;
    }
    Ok(())
  }
}

/// Answers `request` from `graph`, keeping what it must within `admission`.
pub fn negotiate<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  admission: &mut Admission,
) -> Result<Outcome<S::Green, S::Node>, Refusal> {
  match begin(graph, request, admission)? {
    Begun::Answered(outcome) => Ok(outcome),
    Begun::Selecting(mut negotiating) => loop {
      if let Some(outcome) = negotiating.step(graph, admission, &mut || false)? {
        return Ok(outcome);
      }
    },
  }
}

/// A negotiation begun (A-745): answered already (nothing, or the acknowledgments alone where no search could make it
/// ready), or under way a unit at a time: `ready`'s search, the cut, and the choice of the pack's objects.
pub enum Begun<G, N> {
  /// Answered.
  Answered(Outcome<G, N>),
  /// Under way ([`Negotiating::step`]); boxed, the walk being most of its size.
  Selecting(Box<Negotiating<G, N>>),
}

/// A negotiation answered a unit at a time (A-745, §4.17.1 "A fetch's answer" rule 3): whether the server is `ready`
/// (the search below each want), the shallow cut (its walk), then the choice of the objects its pack sends one by one;
/// what its answer says besides them (the acknowledgments, the shallow-info section and the runs) as each is decided,
/// how it went so far, and the walk under way, which keeps its place across steps and is lent the graph for each.
pub struct Negotiating<G, N> {
  /// The acknowledgments, `None` after `done` (and before `ready` is decided).
  acknowledgments: Option<Acknowledgments>,
  /// The shallow-info section, when one is due, once the cut is made.
  shallow_info: Option<ShallowInfo>,
  /// The runs the pack carries whole.
  runs: Vec<Run<G>>,
  /// How it went so far.
  counters: Counters,
  /// The walk under way.
  phase: Phase<G, N>,
}

/// What a negotiation does next.
enum Phase<G, N> {
  /// The search for `ready`.
  Ready(Box<ready::Readying<N>>, Box<Decided<N>>),
  /// The shallow cut.
  Cut(Box<shallow::Cutting<N>>, Box<Decided<N>>),
  /// The choice of the objects sent one by one.
  Selecting(Box<objects::Selecting<N, G>>),
  /// Answered: nothing left.
  Done,
}

/// What the request decided before the walks, owned so it outlives every lending of the graph: the wants, those a run
/// answers, the common haves, the client's shallow commits, the deepening, and what the choice reads of the request.
struct Decided<N> {
  /// The wants.
  wants: Vec<N>,
  /// The wants a run answers.
  one_round: Vec<N>,
  /// The common haves.
  common: Vec<N>,
  /// The client's shallow commits.
  client_shallow: Vec<N>,
  /// The deepening, taken when the cut begins.
  deepening: Option<shallow::Deepening<N>>,
  /// What the choice reads of the request.
  asked: objects::Asked,
  /// `done`: the acknowledgments are left out.
  done: bool,
}

impl<G, N> Negotiating<G, N> {
  /// How it went so far.
  pub fn counters(&self) -> Counters {
    self.counters
  }

  /// The sections the answer gives before its pack, once they are decided (`ready` found and the cut made, the choice
  /// of the objects under way): the acknowledgments (`None` after `done`) and the shallow-info section; `None` before.
  pub fn sections(&self) -> Option<(Option<&Acknowledgments>, Option<&ShallowInfo>)> {
    matches!(self.phase, Phase::Selecting(_))
      .then(|| (self.acknowledgments.as_ref(), self.shallow_info.as_ref()))
  }
}

impl<G: Copy, N: Copy + Ord + Into<u64> + TryFrom<u64>> Negotiating<G, N> {
  /// Does units of the negotiation until `spent` says the owner's quantum is spent, at least one (a step of `ready`'s
  /// search, of the cut's walk, or of the choice: a commit or a tree entry): the outcome once it is answered (the
  /// acknowledgments alone when the server is not ready, else the pack once its last object is chosen), `None` while
  /// more is left.
  pub fn step<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    admission: &mut Admission,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<Option<Outcome<G, N>>, Refusal> {
    loop {
      if let Some(outcome) = self.unit(graph, admission, spent)? {
        return Ok(Some(outcome));
      }
      if spent() {
        return Ok(None);
      }
    }
  }

  /// One unit (the choice does as many as `spent` allows, its own step): the outcome once answered.
  fn unit<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    admission: &mut Admission,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<Option<Outcome<G, N>>, Refusal> {
    match core::mem::replace(&mut self.phase, Phase::Done) {
      Phase::Ready(mut readying, decided) => {
        let found = readying.unit(
          graph,
          &decided.wants,
          &decided.one_round,
          admission,
          &mut self.counters,
        );
        match found {
          Err(refusal) => Err(refusal),
          Ok(None) => {
            self.phase = Phase::Ready(readying, decided);
            Ok(None)
          }
          Ok(Some(ready)) => {
            let acknowledgments = Acknowledgments {
              common: decided.common.iter().map(|node| graph.id(*node)).collect(),
              ready,
            };
            if !ready {
              return Ok(Some(Outcome {
                answer: Answer::Acknowledgments(acknowledgments),
                counters: self.counters,
              }));
            }
            self.acknowledgments = Some(acknowledgments);
            self.cut(graph, decided, admission).map(|()| None)
          }
        }
      }
      Phase::Cut(mut cutting, decided) => {
        match cutting.unit(
          graph,
          &decided.client_shallow,
          admission,
          &mut self.counters,
        )? {
          None => {
            self.phase = Phase::Cut(cutting, decided);
            Ok(None)
          }
          Some(cut) => self.select(graph, cut, *decided, admission).map(|()| None),
        }
      }
      Phase::Selecting(mut selecting) => {
        let chosen = selecting.step(graph, admission, &mut self.counters, spent)?;
        let Some(objects) = chosen else {
          self.phase = Phase::Selecting(selecting);
          return Ok(None);
        };
        Ok(Some(Outcome {
          answer: Answer::Pack(Pack {
            acknowledgments: self.acknowledgments.take(),
            shallow_info: self.shallow_info.take(),
            runs: core::mem::take(&mut self.runs),
            objects,
            _node: core::marker::PhantomData,
          }),
          counters: self.counters,
        }))
      }
      Phase::Done => Ok(None),
    }
  }

  /// Begins the cut, `ready` decided (or `done` said).
  fn cut<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    mut decided: Box<Decided<N>>,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let cutting = shallow::Cutting::begin(
      graph,
      decided.deepening.take(),
      &decided.wants,
      &decided.client_shallow,
      admission,
    )?;
    self.phase = Phase::Cut(Box::new(cutting), decided);
    Ok(())
  }

  /// Begins the choice of the objects, the cut made.
  fn select<S: Graph<Node = N, Green = G>>(
    &mut self,
    graph: &S,
    cut: shallow::Cut<N>,
    decided: Decided<N>,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    self.shallow_info = cut.info.clone();
    admission
      .charge_items::<Run<G>>(self.runs.len())
      .map_err(Refusal::PastAdmission)?;
    let selecting = objects::Selecting::begin(
      graph,
      decided.asked,
      objects::Sides {
        wants: decided.wants,
        one_round: decided.one_round,
        common: decided.common,
        cut,
      },
      self.runs.clone(),
      admission,
    )?;
    self.phase = Phase::Selecting(Box::new(selecting));
    Ok(())
  }
}

/// Begins answering `request` from `graph` (A-745): what the request bounds (its wants, haves and shallow lines, the
/// one-round runs, the deepening's `deepen-not` names), answered at once where no search could make the server ready
/// (`wait-for-done`, or no common have); else `ready`'s search begun (or, after `done`, the cut), each walk then left to
/// [`Negotiating::step`]; within `admission`.
pub fn begin<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  admission: &mut Admission,
) -> Result<Begun<S::Green, S::Node>, Refusal> {
  let mut counters = Counters::default();
  let wants = ready::wants(graph, request.wants, admission)?;
  if wants.is_empty() && !request.wait_for_done {
    return Ok(Begun::Answered(Outcome {
      answer: Answer::Nothing,
      counters,
    }));
  }
  let common = ready::common(graph, request.haves, admission)?;
  let client_shallow = shallow::client(graph, request.shallow, admission)?;
  if let Some(found) = request.filter {
    filter::check_served(found, request.sparse.is_some()).map_err(Refusal::Filter)?;
  }
  let runs = one_round::runs(graph, request, &wants, &common, &client_shallow, admission)?;
  counters.one_round = runs.answered;
  // Never ready under `wait-for-done`, nor without a common have: answered without a search.
  if !request.done && (request.wait_for_done || common.is_empty()) {
    let acknowledgments = Acknowledgments {
      common: common.iter().map(|node| graph.id(*node)).collect(),
      ready: false,
    };
    return Ok(Begun::Answered(Outcome {
      answer: Answer::Acknowledgments(acknowledgments),
      counters,
    }));
  }
  let deepening = shallow::Deepening::of(graph, request, admission)?;
  let asked = objects::Asked::of(request, admission)?;
  let decided = Box::new(Decided {
    wants,
    one_round: runs.wants,
    common,
    client_shallow,
    deepening,
    asked,
    done: request.done,
  });
  let mut negotiating = Negotiating {
    acknowledgments: None,
    shallow_info: None,
    runs: runs.runs,
    counters,
    phase: Phase::Done,
  };
  if decided.done {
    negotiating.cut(graph, decided, admission)?;
  } else {
    let readying = ready::Readying::begin(graph, &decided.common, admission)?;
    negotiating.phase = Phase::Ready(Box::new(readying), decided);
  }
  Ok(Begun::Selecting(Box::new(negotiating)))
}

/// Whether a server whose client holds `common` is ready to send the pack (A-640 part 3: protocol v0's
/// `ok_to_give_up`, which `get_common_commits` asks again as each have arrives): there is a common object, and each
/// want, peeled through its tags, reaches a common commit or one of its parents, as `ready` above is decided.
pub fn ready<S: Graph>(
  graph: &S,
  wants: &[ObjectId],
  common: &[ObjectId],
  admission: &mut Admission,
) -> Result<bool, Refusal> {
  let mut asking = Ready::begin(graph, wants, common, admission)?;
  loop {
    if let Some(ready) = asking.step(graph, admission, &mut || false)? {
      return Ok(ready);
    }
  }
}

/// [`ready`] asked a unit at a time (A-745 item 3: protocol v0's acknowledgments in turns): the wants and the common
/// haves looked up as it begins (what the request names), then the search below each want a parent a unit.
pub struct Ready<N> {
  /// The wants.
  wants: Vec<N>,
  /// The search.
  readying: ready::Readying<N>,
  /// What it searched.
  counters: Counters,
}

impl<N: Copy + Ord> Ready<N> {
  /// The question begun for `wants` over the client's `common` objects.
  pub fn begin<S: Graph<Node = N>>(
    graph: &S,
    wants: &[ObjectId],
    common: &[ObjectId],
    admission: &mut Admission,
  ) -> Result<Ready<N>, Refusal> {
    let wants = ready::wants(graph, wants, admission)?;
    let common = ready::common(graph, common, admission)?;
    let readying = ready::Readying::begin(graph, &common, admission)?;
    Ok(Ready {
      wants,
      readying,
      counters: Counters::default(),
    })
  }

  /// Units of the search until `spent` says the owner's quantum is spent, at least one: whether the server is ready
  /// once decided, `None` while it is not.
  pub fn step<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    admission: &mut Admission,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<Option<bool>, Refusal> {
    loop {
      if let Some(ready) =
        self
          .readying
          .unit(graph, &self.wants, &[], admission, &mut self.counters)?
      {
        return Ok(Some(ready));
      }
      if spent() {
        return Ok(None);
      }
    }
  }
}

/// The shallow-update `request`'s deepening makes, the shallow-info section's lines (A-640 part 3: protocol v0 sends
/// them before any acknowledgment, `receive_needs`, so ahead of the pack's walk): `None` when it asks for no deepening.
pub fn shallow_update<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  admission: &mut Admission,
) -> Result<Option<ShallowInfo>, Refusal> {
  if request.deepen.is_none() {
    return Ok(None);
  }
  let wants = ready::wants(graph, request.wants, admission)?;
  let client = shallow::client(graph, request.shallow, admission)?;
  let cut = shallow::cut(
    graph,
    request,
    &wants,
    &client,
    admission,
    &mut Counters::default(),
  )?;
  Ok(cut.info)
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
