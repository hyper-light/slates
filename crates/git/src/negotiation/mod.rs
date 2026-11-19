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
//! named.
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
use crate::{Admission, PastAdmission};

pub use filter::Filter;

/// Format: the depth git reads as no depth at all, its `INFINITE_DEPTH`, which `git fetch --unshallow` sends: every
/// shallow commit of the client is made whole (`upload-pack.c` `deepen`).
pub const INFINITE_DEPTH: u32 = i32::MAX.unsigned_abs();

/// What the negotiation reads of a repository's object store. A-614's store implements it over its index rows, commit
/// rows and version chains; the tests implement it over generated histories.
pub trait Graph {
  /// One object the store holds, named by the store's own handle (its position in the repository's index), so a walk
  /// keeps handles rather than names.
  type Node: Copy + Ord + core::fmt::Debug;
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
  /// Objects sent one by one, each once, in handle order.
  pub objects: Vec<N>,
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

/// Answers `request` from `graph`, keeping what it must within `admission`.
pub fn negotiate<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  admission: &mut Admission,
) -> Result<Outcome<S::Green, S::Node>, Refusal> {
  let mut counters = Counters::default();
  let wants = ready::wants(graph, request.wants, admission)?;
  if wants.is_empty() && !request.wait_for_done {
    return Ok(Outcome {
      answer: Answer::Nothing,
      counters,
    });
  }
  let common = ready::common(graph, request.haves, admission)?;
  let client_shallow = shallow::client(graph, request.shallow, admission)?;
  if let Some(found) = request.filter {
    filter::check_served(found).map_err(Refusal::Filter)?;
  }
  let runs = one_round::runs(graph, request, &wants, &common, &client_shallow, admission)?;
  counters.one_round = runs.answered;
  let acknowledgments = if request.done {
    None
  } else {
    let ready = !request.wait_for_done
      && ready::is_ready(
        graph,
        &wants,
        &runs.wants,
        &common,
        admission,
        &mut counters,
      )?;
    let acknowledgments = Acknowledgments {
      common: common.iter().map(|node| graph.id(*node)).collect(),
      ready,
    };
    if !ready {
      return Ok(Outcome {
        answer: Answer::Acknowledgments(acknowledgments),
        counters,
      });
    }
    Some(acknowledgments)
  };
  let cut = shallow::cut(
    graph,
    request,
    &wants,
    &client_shallow,
    admission,
    &mut counters,
  )?;
  let objects = objects::select(
    graph,
    request,
    &objects::Sides {
      wants: &wants,
      one_round: &runs.wants,
      common: &common,
      cut: &cut,
    },
    &runs.runs,
    admission,
    &mut counters,
  )?;
  Ok(Outcome {
    answer: Answer::Pack(Pack {
      acknowledgments,
      shallow_info: cut.info,
      runs: runs.runs,
      objects,
    }),
    counters,
  })
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
  let wants = ready::wants(graph, wants, admission)?;
  let common = ready::common(graph, common, admission)?;
  ready::is_ready(
    graph,
    &wants,
    &[],
    &common,
    admission,
    &mut Counters::default(),
  )
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
