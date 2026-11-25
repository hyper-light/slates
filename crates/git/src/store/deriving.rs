//! A version's sets derived on its first read, a unit at a time (A-614 amended, main's ruling (A) of 2026-11-21; A-647's
//! derived data; A-719's bound on a step).
//!
//! **Why.** A create or a rewrite once walked, at its admission, everything its head reached that no green's head did,
//! and a fast-forward everything new to its green. git/git's whole history, 1,027 refs mostly tags at old commits, was
//! decided at about 0.5 ms a step and still unanswered after 1,200 s (w-git, Studio-2, 2026-11-21): a walk of the
//! history for each ref. Stock git walks a push once and asks reachability when it is read. So a version admitted with
//! no walk ([`super::Reach::Remade`], [`super::Reach::GrownUnwalked`]) records no value, and its sets are the closure
//! of its head, derived here when a reader first asks for them, and kept in the repository's memo.
//!
//! **How.** A derivation of a version first derives the versions its own follows from (the version before it, where
//! its record says it grew from it or brought what that one had not reached; the green its set was remade from, where
//! its admission found one with no walk). Then:
//! - a fast-forward walks from its head, stopping at the version before's reached set: that walk is what it brought,
//!   and its reached set that set grown by it;
//! - a create or a rewrite takes a base: the version its record names, or else the nearest commit back from its head,
//!   by generation number, that a version whose sets are known peels to, that version's reached set less its own tags
//!   (which the new head need not reach; the walk adds any it does). It walks from its head, stopping at the base: the
//!   base and the walk are its reached set. What it brought, where no version came before it with a head, is the walk
//!   alone when its base is the version it was made at, found at its admission (A-850: a branch made at another's head,
//!   a tag of a commit another green heads, brings what the base does not reach), else that whole set; where a version
//!   came before it with a head, the walk from its head stopping at the version before's set.
//!
//! Every unit is one object of a walk or one commit of the search back, so a turn holds its shard no longer than its
//! quantum, however long the history; and a run of versions read in their heads' generation order (a fetch of many
//! tags) walks each segment of the history once, each derivation taking the one before as its base.
//!
//! **Kept.** A derived version's sets are put in the content store as its value would have been, the same bytes by
//! identity (A-647: a type's value is a function of the version's history), and held once by the memo, which also maps
//! the commit each known version's head peels to, to that version: the search's targets. Values are charged to the
//! store's budget as they are put, and refused past it, typed; the memo is let go whole when its owner needs the room
//! ([`Repository::forget_derived`]), and a version is derived again by its next reader. It is a cache: never recorded,
//! never replayed, empty after a restart.

use slates_vcs::ContentStore;

use super::admitting::Descent;
use super::reach::{self, Sets};
use super::{Followed, Place, Repository, StoreRefused};
use crate::object_name::{Kind, ObjectId};
use crate::store::hook_view::Quantum;
use crate::store::{Positions, Push};

/// Shape: bit shift for indexing a dense `u64`-word position bitset (`log2(u64::BITS)`).
const SEEN_WORD_SHIFT: u32 = u64::BITS.trailing_zeros();

/// Shape: bit mask within a `u64` word of [`SeenPositions`].
const SEEN_WORD_MASK: u64 = (u64::BITS as u64).saturating_sub(1);

/// Reusable O(1) dense position bitset cleared in O(touched words) between derived versions so
/// multi-ref derivations (such as git/git's 1,027 tags) allocate once and avoid per-object
/// `BTreeSet<ObjectId>` lookups.
#[derive(Debug, Default)]
struct SeenPositions {
  words: Vec<u64>,
  touched: Vec<usize>,
}

impl SeenPositions {
  /// Whether `position` has been marked visited in the current walk.
  fn contains(&self, position: u64) -> bool {
    let Ok(word) = usize::try_from(position >> SEEN_WORD_SHIFT) else {
      return false;
    };
    let bit = 1_u64 << (position & SEEN_WORD_MASK);
    self.words.get(word).is_some_and(|slot| *slot & bit != 0)
  }

  /// Marks `position` visited; returns `true` when it was not previously set.
  fn insert(&mut self, position: u64) -> bool {
    let Ok(word) = usize::try_from(position >> SEEN_WORD_SHIFT) else {
      return false;
    };
    let bit = 1_u64 << (position & SEEN_WORD_MASK);
    if word >= self.words.len() {
      self.words.resize(word.saturating_add(1), 0);
    }
    let Some(slot) = self.words.get_mut(word) else {
      return false;
    };
    if *slot & bit != 0 {
      return false;
    }
    if *slot == 0 {
      self.touched.push(word);
    }
    *slot |= bit;
    true
  }

  /// Resets every bit set during the current walk in O(touched words), retaining capacity.
  fn clear(&mut self) {
    for word in self.touched.drain(..) {
      if let Some(slot) = self.words.get_mut(word) {
        *slot = 0;
      }
    }
  }
}

/// Position-native cooperative reachability walk over the committed index alone.
///
/// Walks commits and tags first (`pending_commits`), then walks commit trees in oldest-commit-first
/// (ascending generation) order so each commit's first parent tree is already fully closed in
/// `seen ∪ stop`. Diffing each commit's tree against its closed first-parent tree via
/// [`Repository::tree_diff_references`] skips unchanged subtrees and blobs in word-sized chunks
/// without index lookups while preserving exact reachability (including across `git revert` cycles,
/// because `base_tree` is only used once it is already closed in `seen ∪ stop`).
#[derive(Debug)]
struct IndexedWalk {
  stop: Positions,
  pending_commits: Vec<u64>,
  /// `(generation, commit_position, tree_position, first_parent_position)` collected in Phase 1.
  commits: Vec<(u64, u64, u64, Option<u64>)>,
  /// `(commit_position, tree_position)` sorted by `commit_position` for O(log C) first-parent tree
  /// lookup without hitting the repository index during Phase 2.
  commit_trees: Vec<(u64, u64)>,
  commits_sorted: bool,
  /// Pending `(tree_position, optional_closed_base_tree_position)` pairs for the current commit or
  /// tag-targeted trees. Drained completely before advancing to the next commit.
  tree_stack: Vec<(u64, Option<u64>)>,
  found: Vec<u64>,
}

impl IndexedWalk {
  /// Starts a position-native walk from `head` stopping at `stop`.
  fn from_position(head: u64, stop: Positions) -> IndexedWalk {
    IndexedWalk {
      stop,
      pending_commits: vec![head],
      commits: Vec::new(),
      commit_trees: Vec::new(),
      commits_sorted: false,
      tree_stack: Vec::new(),
      found: Vec::new(),
    }
  }

  /// Empty placeholder used when moving a completed walk out of [`Step`].
  fn empty() -> IndexedWalk {
    IndexedWalk {
      stop: Positions::new(),
      pending_commits: Vec::new(),
      commits: Vec::new(),
      commit_trees: Vec::new(),
      commits_sorted: true,
      tree_stack: Vec::new(),
      found: Vec::new(),
    }
  }

  /// Returns the root tree position of a commit collected during Phase 1, if present.
  fn walked_commit_tree(&self, commit_position: u64) -> Option<u64> {
    let idx = self
      .commit_trees
      .binary_search_by_key(&commit_position, |&(pos, _)| pos)
      .ok()?;
    self.commit_trees.get(idx).map(|&(_, tree)| tree)
  }

  /// Compresses all discovered positions into [`Positions`].
  fn into_positions(self) -> Positions {
    Positions::of(self.found)
  }
}

/// A derivation under way of one version's sets and those its own follow from (module docs).
#[derive(Debug)]
pub struct Deriving {
  /// The versions to derive, the asked one first, each before those it follows from: the last is derived next.
  waiting: Vec<(u64, usize)>,
  /// The version being derived and where its derivation stands.
  now: Option<Now>,
  /// An empty push, through which base searches read the committed index alone.
  push: Push,
  /// Reusable visited-position bitset cleared between walks.
  seen: SeenPositions,
}

impl Deriving {
  /// Whether nothing is left of it: every version it queued is known.
  pub fn is_done(&self) -> bool {
    self.now.is_none() && self.waiting.is_empty()
  }
}

/// One version's derivation under way.
#[derive(Debug)]
struct Now {
  /// The green and version.
  key: (u64, usize),
  /// Its head's position in the committed index.
  head: u64,
  /// Whether a version with a head came before it, whose set what it brought stops at.
  before: bool,
  /// Where it stands.
  step: Step,
}

/// Where one version's derivation stands.
#[derive(Debug)]
enum Step {
  /// A create's or a rewrite's search back from its head for the nearest commit a version whose sets are known peels
  /// to.
  Base(Descent),
  /// The walk from its head beyond its base: the base and the walk are its reached set. `made_at`: the base is the
  /// version the green was made at, found at its admission (A-850), so a first version brings the walk alone; a base
  /// found by a search back, or none, leaves a first version bringing its whole reached set.
  Reached {
    base: Positions,
    walk: IndexedWalk,
    made_at: bool,
  },
  /// A fast-forward's walk from its head beyond the version before's set: what it brought, which grows that set.
  Grown {
    previous: Positions,
    walk: IndexedWalk,
  },
  /// The walk from its head beyond the version before's set, its reached set known: what it brought.
  Brought {
    reached: Positions,
    walk: IndexedWalk,
  },
}

/// What a turn of a derivation leaves.
#[derive(Debug)]
pub enum Derived {
  /// More units are due: the next turn goes on.
  More,
  /// A version's sets, for the caller to keep ([`Repository::keep_derived`]) before the next turn.
  Sets {
    /// The green.
    green: u64,
    /// The version.
    version: usize,
    /// Its sets.
    sets: Sets,
  },
  /// The asked version's sets are known.
  Done,
}

/// What one unit of a derivation did.
enum Unit {
  /// Nothing to report: a unit of a walk or a search, or a version found known.
  Went,
  /// A version's sets.
  Sets((u64, usize), Sets),
}

/// How a version's derivation begins.
enum Begun {
  /// A version it follows from is not known: that one first.
  Needs((u64, usize)),
  /// Its derivation, under way.
  Now(Box<Now>),
  /// Its sets at once (a delete, a deleted ref).
  Sets(Sets),
}

impl Repository {
  /// A derivation of `green`'s version `version`'s sets begun (module docs); refused for a version the green does not
  /// have.
  pub fn deriving(&self, green: u64, version: usize) -> Result<Deriving, StoreRefused> {
    self.version_record(green, version)?;
    Ok(Deriving {
      waiting: vec![(green, version)],
      now: None,
      push: Push::new(self.format),
      seen: SeenPositions::default(),
    })
  }

  /// A derivation of every version whose sets are not known (module docs): what a repository's owner runs, a turn at a
  /// time, before it serves the repository or encodes its fragments. The versions whose heads peel to the oldest commits
  /// come first, by generation number, so each one's search back meets the one before as its base and git/git's tags
  /// walk their history about once between them, where each from nothing walked it a tag. Building the queue reads each
  /// unknown version's head once, a read the versions' own count bounds.
  pub fn deriving_due(&self, store: &ContentStore) -> Deriving {
    let mut due: Vec<(u64, (u64, usize))> = Vec::new();
    let memo = self.memo.borrow();
    for (green, chain) in &self.chains {
      for (version, record) in chain.versions.iter().enumerate() {
        if record.value.is_some() || memo.values.contains_key(&(*green, version)) {
          continue;
        }
        let generation = record
          .head
          .and_then(|head| self.head_generation(store, head))
          .unwrap_or(0);
        due.push((generation, (*green, version)));
      }
    }
    drop(memo);
    // The last is derived first: the newest generation at the front.
    due.sort_unstable_by(|one, other| other.cmp(one));
    Deriving {
      waiting: due.into_iter().map(|(_, key)| key).collect(),
      now: None,
      push: Push::new(self.format),
      seen: SeenPositions::default(),
    }
  }

  /// Whether any version's sets are not known: whether [`Self::deriving_due`] has anything to do.
  pub fn derivations_due(&self) -> bool {
    self.undecided_derivations.get() > 0
  }

  /// The generation number of the commit the object at `position` peels to; none for one that peels to no commit.
  fn head_generation(&self, store: &ContentStore, position: u64) -> Option<u64> {
    let push = Push::new(self.format);
    let id = self.id_at(store, position)?;
    let commit = self.peel(store, &push, id).ok()??;
    self.generation(store, &push, &commit).ok()
  }

  /// One turn of `deriving`: units until `quantum` is spent, one at least, or until a version's sets are derived, which
  /// the caller keeps before the next turn; done once the asked version's sets are known.
  pub fn derive_turn(
    &self,
    store: &ContentStore,
    deriving: &mut Deriving,
    quantum: Quantum<'_>,
  ) -> Result<Derived, StoreRefused> {
    let deadline = quantum.clock.now_ns().saturating_add(quantum.ns);
    let mut units = 0_u32;
    loop {
      if deriving.now.is_none() && deriving.waiting.is_empty() {
        return Ok(Derived::Done);
      }
      if let Unit::Sets((green, version), sets) = self.derive_unit(store, deriving)? {
        return Ok(Derived::Sets {
          green,
          version,
          sets,
        });
      }
      units = units.wrapping_add(1);
      // Sample the monotonic clock once per machine-word batch of units (`u64::BITS` = 64) when
      // a non-zero quantum is given, avoiding per-object clock calls on fast indexed walk steps.
      if (quantum.ns == 0 || units.is_multiple_of(u64::BITS)) && quantum.clock.now_ns() >= deadline
      {
        return Ok(Derived::More);
      }
    }
  }

  /// Keeps `sets`, derived for `green`'s version `version`, in the memo: put in the store as the version's value would
  /// have been, held once by the memo (module docs, "Kept"); a version known already keeps what it had.
  pub fn keep_derived(
    &self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    sets: &Sets,
  ) -> Result<(), StoreRefused> {
    if self.value_of(green, version)?.is_some() {
      return Ok(());
    }
    let shape = reach::shape_of(store.window())?;
    let value = sets.put(store, shape)?;
    if self
      .memo
      .borrow_mut()
      .values
      .insert((green, version), value)
      .is_none()
    {
      self
        .undecided_derivations
        .set(self.undecided_derivations.get().saturating_sub(1));
    }
    self.count(|counters| counters.derived = counters.derived.saturating_add(1));
    self.note_known(store, (green, version));
    Ok(())
  }

  /// `green`'s version `version`'s sets, derived where they are not known, the units of [`Self::derive_turn`] run to
  /// their end and each version kept on the way: the derivation whole, as [`Repository::admit`] is the admission whole,
  /// for a reader off the shard's step.
  pub fn derive(
    &self,
    store: &mut ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Sets, StoreRefused> {
    let mut deriving = self.deriving(green, version)?;
    let mut clock = super::staging::Unbounded;
    loop {
      let turned = self.derive_turn(
        store,
        &mut deriving,
        Quantum {
          clock: &mut clock,
          ns: u64::MAX,
        },
      )?;
      match turned {
        Derived::More => {}
        Derived::Sets {
          green,
          version,
          sets,
        } => self.keep_derived(store, (green, version), &sets)?,
        Derived::Done => return self.sets(store, green, version),
      }
    }
  }

  /// Lets go of every derived set the memo holds (module docs, "Kept"): each is derived again by its next reader.
  pub fn forget_derived(&self, store: &mut ContentStore) -> Result<(), StoreRefused> {
    let mut memo = self.memo.borrow_mut();
    let values = core::mem::take(&mut memo.values);
    let chains = &self.chains;
    memo.at.retain(|_, (green, version)| {
      chains
        .get(green)
        .and_then(|chain| chain.versions.get(*version))
        .is_some_and(|record| record.value.is_some())
    });
    drop(memo);
    self.undecided_derivations.set(
      self
        .undecided_derivations
        .get()
        .saturating_add(values.len()),
    );
    let mut first = Ok(());
    for value in values.values() {
      if let Err(refused) = store.release(value) {
        first = first.and(Err(StoreRefused::from(refused)));
      }
    }
    first
  }

  /// Notes that `key`'s sets are known, under the commit its head peels to: a derivation's search back stops there.
  pub(super) fn note_known(&self, store: &ContentStore, (green, version): (u64, usize)) {
    let Some(position) = self
      .chains
      .get(&green)
      .and_then(|chain| chain.versions.get(version))
      .and_then(|record| record.head)
    else {
      return;
    };
    let Some(commit) = self.peeled_position(store, position) else {
      return;
    };
    self
      .memo
      .borrow_mut()
      .at
      .entry(commit)
      .or_insert((green, version));
  }

  /// The position of the commit the object at `position` peels to; none for a tree or a blob, or one not indexed.
  fn peeled_position(&self, store: &ContentStore, position: u64) -> Option<u64> {
    let id = self.id_at(store, position)?;
    let commit = self.peel(store, &Push::new(self.format), id).ok()??;
    Some(self.indexed(store, &commit)?.0.position)
  }

  /// What a version whose head is at `head` and made at `base` takes from it (A-850): `reached`, the base's reached
  /// set, less the base's own tags, which the version made at it need not reach (its walk adds any it does); whole where
  /// the base's head is the version's own.
  pub(super) fn base_set(
    &self,
    store: &ContentStore,
    ((green, version), head): ((u64, usize), Option<u64>),
    reached: Positions,
  ) -> Positions {
    let base_head = self
      .chains
      .get(&green)
      .and_then(|chain| chain.versions.get(version))
      .and_then(|record| record.head);
    match base_head {
      Some(base_head) if Some(base_head) != head => {
        reached.difference(&self.tag_chain(store, base_head))
      }
      _ => reached,
    }
  }

  /// The positions of the tags from the object at `position` to the commit it peels to: what a version reaches that a
  /// version whose head peels to that commit, or descends from it, need not. Reads `ObjectRow::tag_target` directly
  /// when present, avoiding pack decompression.
  fn tag_chain(&self, store: &ContentStore, position: u64) -> Positions {
    let mut chain = Positions::new();
    let mut at = position;
    let push = Push::new(self.format);
    while self.kind_at(at) == Some(Kind::Tag) && chain.insert_new(at) {
      let Some(id) = self.id_at(store, at) else {
        break;
      };
      let next = self
        .indexed(store, &id)
        .and_then(|(row, _)| row.tag_target)
        .or_else(|| {
          self
            .locate(store, &push, &id)
            .and_then(|place| self.tag_reference(store, &id, &place).ok())
            .and_then(|target| Some(self.indexed(store, &target)?.0.position))
        });
      match next {
        Some(next) => at = next,
        None => break,
      }
    }
    chain
  }

  /// One unit of `deriving` (module docs).
  fn derive_unit(
    &self,
    store: &ContentStore,
    deriving: &mut Deriving,
  ) -> Result<Unit, StoreRefused> {
    let Some(now) = deriving.now.take() else {
      let Some(&key) = deriving.waiting.last() else {
        return Ok(Unit::Went);
      };
      if self.value_of(key.0, key.1)?.is_some() {
        deriving.waiting.pop();
        return Ok(Unit::Went);
      }
      return match self.begin(store, key)? {
        Begun::Needs(first) => {
          // A version's sets never follow from its own or a later one's, so a version asked twice is a history that
          // is its own ancestor: refused rather than looped on.
          if deriving.waiting.contains(&first) {
            return Err(StoreRefused::NoVersion {
              green: first.0,
              version: crate::to_u64(first.1),
            });
          }
          deriving.waiting.push(first);
          Ok(Unit::Went)
        }
        Begun::Now(now) => {
          deriving.seen.clear();
          deriving.now = Some(*now);
          Ok(Unit::Went)
        }
        Begun::Sets(sets) => {
          deriving.waiting.pop();
          Ok(Unit::Sets(key, sets))
        }
      };
    };
    let mut now = now;
    let step_result: Result<Option<Sets>, StoreRefused> = match &mut now.step {
      Step::Base(descent) => match self.base_unit(store, &deriving.push, now.key, descent) {
        Ok(Some(base)) => {
          deriving.seen.clear();
          now.step = Step::Reached {
            walk: IndexedWalk::from_position(now.head, base.clone()),
            base,
            made_at: false,
          };
          Ok(None)
        }
        Ok(None) => Ok(None),
        Err(refused) => Err(refused),
      },
      Step::Reached { walk, .. } => {
        match self.indexed_walk_unit(store, walk, &mut deriving.seen) {
          Ok(true) => Ok(None),
          Ok(false) => {
            let Step::Reached {
              base,
              walk,
              made_at,
            } = core::mem::replace(
              &mut now.step,
              Step::Reached {
                base: Positions::new(),
                walk: IndexedWalk::empty(),
                made_at: false,
              },
            )
            else {
              return Ok(Unit::Went);
            };
            let found = walk.into_positions();
            let reached = base.union(&found);
            if !now.before {
              // A green's first version brings what it reaches beyond the version it was made at (A-850: a tag of a held
              // commit brings its tag objects alone), the whole of what it reaches where it was made at none.
              let brought = if made_at { found } else { reached.clone() };
              Ok(Some(Sets { brought, reached }))
            } else {
              let previous = self
                .sets(store, now.key.0, now.key.1.saturating_sub(1))?
                .reached;
              deriving.seen.clear();
              now.step = Step::Brought {
                reached,
                walk: IndexedWalk::from_position(now.head, previous),
              };
              Ok(None)
            }
          }
          Err(refused) => Err(refused),
        }
      }
      Step::Grown { walk, .. } => match self.indexed_walk_unit(store, walk, &mut deriving.seen) {
        Ok(true) => Ok(None),
        Ok(false) => {
          let Step::Grown { previous, walk } = core::mem::replace(
            &mut now.step,
            Step::Reached {
              base: Positions::new(),
              walk: IndexedWalk::empty(),
              made_at: false,
            },
          ) else {
            return Ok(Unit::Went);
          };
          let brought = walk.into_positions();
          Ok(Some(Sets {
            reached: previous.union(&brought),
            brought,
          }))
        }
        Err(refused) => Err(refused),
      },
      Step::Brought { walk, .. } => match self.indexed_walk_unit(store, walk, &mut deriving.seen) {
        Ok(true) => Ok(None),
        Ok(false) => {
          let Step::Brought { reached, walk } = core::mem::replace(
            &mut now.step,
            Step::Reached {
              base: Positions::new(),
              walk: IndexedWalk::empty(),
              made_at: false,
            },
          ) else {
            return Ok(Unit::Went);
          };
          let brought = walk.into_positions();
          Ok(Some(Sets { reached, brought }))
        }
        Err(refused) => Err(refused),
      },
    };
    match step_result {
      Ok(Some(sets)) => {
        deriving.seen.clear();
        deriving.waiting.pop();
        Ok(Unit::Sets(now.key, sets))
      }
      Ok(None) => {
        deriving.now = Some(now);
        Ok(Unit::Went)
      }
      Err(refused) => {
        // Restore `now` so the caller can bring back a let-go pack window and resume the exact
        // walk unit in place rather than restarting the version's derivation from scratch.
        deriving.now = Some(now);
        Err(refused)
      }
    }
  }

  /// How `key`'s derivation begins (module docs): a version it follows from first, where one is not known.
  fn begin(
    &self,
    store: &ContentStore,
    (green, version): (u64, usize),
  ) -> Result<Begun, StoreRefused> {
    let (record, before) =
      self
        .reach_record(green, crate::to_u64(version))
        .ok_or(StoreRefused::NoVersion {
          green,
          version: crate::to_u64(version),
        })?;
    let previous = (green, version.saturating_sub(1));
    let (head_pos, head_id) = match (record.followed, record.head) {
      (Followed::Emptied, _) | (_, None) => return Ok(Begun::Sets(Sets::default())),
      (_, Some(position)) => {
        let id = self.id_at(store, position).ok_or(StoreRefused::NoVersion {
          green,
          version: crate::to_u64(version),
        })?;
        (position, id)
      }
    };
    let unknown = |key: (u64, usize)| -> Result<bool, StoreRefused> {
      Ok(self.value_of(key.0, key.1)?.is_none())
    };
    let now = |step: Step| {
      Begun::Now(Box::new(Now {
        key: (green, version),
        head: head_pos,
        before,
        step,
      }))
    };
    match record.followed {
      Followed::Grown => {
        if !before {
          return Ok(now(Step::Reached {
            base: Positions::new(),
            walk: IndexedWalk::from_position(head_pos, Positions::new()),
            made_at: false,
          }));
        }
        if unknown(previous)? {
          return Ok(Begun::Needs(previous));
        }
        let previous = self.sets(store, green, version.saturating_sub(1))?.reached;
        Ok(now(Step::Grown {
          walk: IndexedWalk::from_position(head_pos, previous.clone()),
          previous,
        }))
      }
      Followed::Remade { base } => {
        if before && unknown(previous)? {
          return Ok(Begun::Needs(previous));
        }
        if let Some(base) = base {
          if unknown(base)? {
            return Ok(Begun::Needs(base));
          }
          let reached = self.sets(store, base.0, base.1)?.reached;
          let base_head = self
            .chains
            .get(&base.0)
            .and_then(|chain| chain.versions.get(base.1))
            .and_then(|record| record.head);
          if !before && base_head.is_some() && base_head == record.head {
            return Ok(Begun::Sets(Sets {
              brought: Positions::new(),
              reached,
            }));
          }
          let set = self.base_set(store, (base, record.head), reached);
          return Ok(now(Step::Reached {
            walk: IndexedWalk::from_position(head_pos, set.clone()),
            base: set,
            made_at: true,
          }));
        }
        let push = Push::new(self.format);
        match self.peel(store, &push, head_id)? {
          Some(commit) => {
            let generation = self.generation(store, &push, &commit)?;
            Ok(now(Step::Base(Descent::back_from(commit, generation))))
          }
          None => Ok(now(Step::Reached {
            base: Positions::new(),
            walk: IndexedWalk::from_position(head_pos, Positions::new()),
            made_at: false,
          })),
        }
      }
      Followed::Emptied => Ok(Begun::Sets(Sets::default())),
    }
  }

  /// One commit of a create's search back for its base: the base, once found (the reached set of the version a known
  /// commit's version is, less that version's own tags), or none to go on; the empty set once the history is walked
  /// to its roots with none found.
  fn base_unit(
    &self,
    store: &ContentStore,
    push: &Push,
    key: (u64, usize),
    descent: &mut Descent,
  ) -> Result<Option<Positions>, StoreRefused> {
    let Some(id) = descent.next() else {
      return Ok(Some(Positions::new()));
    };
    let position = self.indexed(store, &id).map(|(row, _)| row.position);
    let known = position.and_then(|position| self.memo.borrow().at.get(&position).copied());
    if let Some(found) = known.filter(|found| *found != key) {
      self.count(|counters| counters.bases_found = counters.bases_found.saturating_add(1));
      let reached = self.sets(store, found.0, found.1)?.reached;
      return Ok(Some(self.base_set(store, (found, None), reached)));
    }
    for parent in self.parents(store, push, &id)? {
      descent.push(parent, self.generation(store, push, &parent)?);
    }
    Ok(None)
  }

  /// One cooperative unit of [`IndexedWalk`]: returns `Ok(true)` while more work remains,
  /// `Ok(false)` once every reachable object beyond `walk.stop` has been collected. Leaves `walk`
  /// and `seen` unchanged on `Err` so a paused derivation resumes cleanly once a let-go pack window
  /// is restored.
  fn indexed_walk_unit(
    &self,
    store: &ContentStore,
    walk: &mut IndexedWalk,
    seen: &mut SeenPositions,
  ) -> Result<bool, StoreRefused> {
    if let Some(position) = walk.pending_commits.pop() {
      return self.indexed_walk_commit_or_tag(store, walk, seen, position);
    }
    if !walk.commits_sorted {
      walk.commit_trees = walk
        .commits
        .iter()
        .map(|&(_, commit_pos, tree_pos, _)| (commit_pos, tree_pos))
        .collect();
      walk.commit_trees.sort_unstable_by_key(|&(pos, _)| pos);
      // Descending by `(generation, commit_pos)` so `.pop()` processes oldest commits first
      // (parents strictly before children).
      walk
        .commits
        .sort_unstable_by_key(|a| std::cmp::Reverse((a.0, a.1)));
      walk.commits_sorted = true;
    }
    if let Some((tree_pos, base_pos)) = walk.tree_stack.pop() {
      return self.indexed_walk_tree(store, walk, seen, tree_pos, base_pos);
    }
    let Some((_, _, tree_pos, first_parent)) = walk.commits.pop() else {
      return Ok(false);
    };
    if seen.contains(tree_pos) {
      return Ok(true);
    }
    if walk.stop.contains(tree_pos) {
      seen.insert(tree_pos);
      self.met_reached(Kind::Tree);
      return Ok(true);
    }
    let base_tree = first_parent
      .and_then(|parent| {
        walk.walked_commit_tree(parent).or_else(|| {
          self
            .commit_header_at(store, parent)
            .map(|(tree, _, _)| tree)
        })
      })
      .filter(|&bt| bt != tree_pos && (walk.stop.contains(bt) || seen.contains(bt)));
    walk.tree_stack.push((tree_pos, base_tree));
    Ok(true)
  }

  /// Phase 1 unit of [`IndexedWalk`]: expands a commit, tag, blob, or tag-targeted tree.
  fn indexed_walk_commit_or_tag(
    &self,
    store: &ContentStore,
    walk: &mut IndexedWalk,
    seen: &mut SeenPositions,
    position: u64,
  ) -> Result<bool, StoreRefused> {
    if seen.contains(position) {
      return Ok(true);
    }
    if walk.stop.contains(position) {
      seen.insert(position);
      if let Some(kind) = self.kind_at(position) {
        self.met_reached(kind);
      }
      return Ok(true);
    }
    match self.kind_at(position) {
      Some(Kind::Blob) => {
        seen.insert(position);
        walk.found.push(position);
        self.count(|counters| counters.walked = counters.walked.saturating_add(1));
        Ok(true)
      }
      Some(Kind::Tree) => {
        walk.tree_stack.push((position, None));
        Ok(true)
      }
      Some(Kind::Tag) => {
        let target = match self.tag_target_at(store, position) {
          Some(Some(target)) => target,
          _ => {
            let Some((id, row, content)) = self.indexed_at(store, position) else {
              walk.pending_commits.push(position);
              return Err(StoreRefused::Unreadable {
                id: self
                  .id_at(store, position)
                  .unwrap_or_else(|| ObjectId::null(self.format)),
              });
            };
            let place = Place::Indexed { row, content };
            let target_id = match self.tag_reference(store, &id, &place) {
              Ok(target_id) => target_id,
              Err(refused) => {
                walk.pending_commits.push(position);
                return Err(refused);
              }
            };
            match self.indexed_position(store, &target_id) {
              Some(target_pos) => target_pos,
              None => {
                walk.pending_commits.push(position);
                return Err(StoreRefused::Missing { id: target_id });
              }
            }
          }
        };
        seen.insert(position);
        walk.found.push(position);
        self.count(|counters| counters.walked = counters.walked.saturating_add(1));
        walk.pending_commits.push(target);
        Ok(true)
      }
      Some(Kind::Commit) => {
        let Some((tree, generation, parents)) = self.commit_tree_gen_parents_at(store, position)
        else {
          walk.pending_commits.push(position);
          return Err(StoreRefused::Unreadable {
            id: self
              .id_at(store, position)
              .unwrap_or_else(|| ObjectId::null(self.format)),
          });
        };
        seen.insert(position);
        walk.found.push(position);
        self.count(|counters| counters.walked = counters.walked.saturating_add(1));
        let first_parent = parents.first();
        for parent in parents {
          walk.pending_commits.push(parent);
        }
        walk
          .commits
          .push((generation, position, tree, first_parent));
        Ok(true)
      }
      None => {
        walk.pending_commits.push(position);
        Err(StoreRefused::Unreadable {
          id: self
            .id_at(store, position)
            .unwrap_or_else(|| ObjectId::null(self.format)),
        })
      }
    }
  }

  /// Phase 2 unit of [`IndexedWalk`]: expands one tree, diffing against `base_pos` when a closed
  /// counterpart tree is available, and resolving all child positions before mutating `walk`/`seen`.
  fn indexed_walk_tree(
    &self,
    store: &ContentStore,
    walk: &mut IndexedWalk,
    seen: &mut SeenPositions,
    tree_pos: u64,
    base_pos: Option<u64>,
  ) -> Result<bool, StoreRefused> {
    if seen.contains(tree_pos) {
      return Ok(true);
    }
    if walk.stop.contains(tree_pos) {
      seen.insert(tree_pos);
      self.met_reached(Kind::Tree);
      return Ok(true);
    }
    match self.kind_at(tree_pos) {
      Some(Kind::Blob) => {
        seen.insert(tree_pos);
        walk.found.push(tree_pos);
        self.count(|counters| counters.walked = counters.walked.saturating_add(1));
        return Ok(true);
      }
      Some(Kind::Tree) => {}
      _ => {
        walk.pending_commits.push(tree_pos);
        return Ok(true);
      }
    }
    let Some((tree_id, tree_row, tree_content)) = self.indexed_at(store, tree_pos) else {
      walk.tree_stack.push((tree_pos, base_pos));
      return Err(StoreRefused::Unreadable {
        id: self
          .id_at(store, tree_pos)
          .unwrap_or_else(|| ObjectId::null(self.format)),
      });
    };
    let resolved = self.resolve_tree_children(
      store,
      (&tree_id, &tree_row, tree_content),
      tree_pos,
      base_pos,
    );
    let children = match resolved {
      Ok(children) => children,
      Err(refused) => {
        walk.tree_stack.push((tree_pos, base_pos));
        return Err(refused);
      }
    };
    seen.insert(tree_pos);
    walk.found.push(tree_pos);
    self.count(|counters| counters.walked = counters.walked.saturating_add(1));
    self.indexed_walk_enqueue_children(walk, seen, children);
    Ok(true)
  }

  /// Reads and resolves all non-identical child positions of `tree_pos`, diffing against `base_pos`
  /// when available, without mutating walk state so errors resume cleanly.
  fn resolve_tree_children(
    &self,
    store: &ContentStore,
    (tree_id, tree_row, tree_content): (&ObjectId, &super::ObjectRow, slates_vcs::ContentId),
    tree_pos: u64,
    base_pos: Option<u64>,
  ) -> Result<Vec<(u64, Option<u64>)>, StoreRefused> {
    if let Some(base) = base_pos.filter(|&b| b != tree_pos)
      && let Some((_, base_row, base_content)) = self.indexed_at(store, base)
      && base_row.kind == Kind::Tree
    {
      let pairs = self.tree_diff_references(
        store,
        (tree_id, tree_row, tree_content),
        (&base_row, base_content),
      )?;
      let mut resolved = Vec::with_capacity(pairs.len());
      for (child_oid, counterpart_oid) in pairs {
        let child_pos = self
          .indexed_position(store, &child_oid)
          .ok_or(StoreRefused::Missing { id: child_oid })?;
        let counter_pos = counterpart_oid
          .as_ref()
          .and_then(|oid| self.indexed_position(store, oid))
          .filter(|&cp| cp != child_pos && self.kind_at(cp) == Some(Kind::Tree));
        resolved.push((child_pos, counter_pos));
      }
      return Ok(resolved);
    }
    let place = Place::Indexed {
      row: tree_row.clone(),
      content: tree_content,
    };
    let ids = self.tree_references(store, tree_id, &place)?;
    let mut resolved = Vec::with_capacity(ids.len());
    for child_oid in ids {
      let child_pos = self
        .indexed_position(store, &child_oid)
        .ok_or(StoreRefused::Missing { id: child_oid })?;
      resolved.push((child_pos, None));
    }
    Ok(resolved)
  }

  /// Records resolved blob children directly in O(1) and queues unseen child trees for expansion.
  fn indexed_walk_enqueue_children(
    &self,
    walk: &mut IndexedWalk,
    seen: &mut SeenPositions,
    children: Vec<(u64, Option<u64>)>,
  ) {
    for (child_pos, counter_pos) in children {
      if seen.contains(child_pos) {
        continue;
      }
      if walk.stop.contains(child_pos) {
        seen.insert(child_pos);
        if let Some(kind) = self.kind_at(child_pos) {
          self.met_reached(kind);
        }
        continue;
      }
      match self.kind_at(child_pos) {
        Some(Kind::Blob) => {
          seen.insert(child_pos);
          walk.found.push(child_pos);
          self.count(|counters| counters.walked = counters.walked.saturating_add(1));
        }
        Some(Kind::Tree) => walk.tree_stack.push((child_pos, counter_pos)),
        _ => walk.pending_commits.push(child_pos),
      }
    }
  }
}
