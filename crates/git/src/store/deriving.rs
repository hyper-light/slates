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

use super::admitting::{Descent, Walk};
use super::reach::{self, Sets};
use super::{Followed, Repository, StoreRefused};
use crate::object_name::{Kind, ObjectId};
use crate::store::hook_view::Quantum;
use crate::store::{Positions, Push};

/// A derivation under way of one version's sets and those its own follow from (module docs).
#[derive(Debug)]
pub struct Deriving {
  /// The versions to derive, the asked one first, each before those it follows from: the last is derived next.
  waiting: Vec<(u64, usize)>,
  /// The version being derived and where its derivation stands.
  now: Option<Now>,
  /// An empty push, through which the walks read the committed index alone.
  push: Push,
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
  /// Its head.
  head: ObjectId,
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
    walk: Walk,
    made_at: bool,
  },
  /// A fast-forward's walk from its head beyond the version before's set: what it brought, which grows that set.
  Grown { previous: Positions, walk: Walk },
  /// The walk from its head beyond the version before's set, its reached set known: what it brought.
  Brought { reached: Positions, walk: Walk },
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
      if quantum.clock.now_ns() >= deadline {
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
      Step::Base(descent) => {
        match self.base_unit(store, &deriving.push, (now.key, now.head), descent) {
          Ok(Some(base)) => {
            now.step = Step::Reached {
              walk: Walk::from(now.head, base.clone()),
              base,
              made_at: false,
            };
            Ok(None)
          }
          Ok(None) => Ok(None),
          Err(refused) => Err(refused),
        }
      }
      Step::Reached { walk, .. } => match self.walk_unit(store, &deriving.push, walk) {
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
              walk: Walk::from(now.head, Positions::new()),
              made_at: false,
            },
          )
          else {
            return Ok(Unit::Went);
          };
          let found = indexed_alone(walk)?;
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
            now.step = Step::Brought {
              reached,
              walk: Walk::from(now.head, previous),
            };
            Ok(None)
          }
        }
        Err(refused) => Err(refused),
      },
      Step::Grown { walk, .. } => match self.walk_unit(store, &deriving.push, walk) {
        Ok(true) => Ok(None),
        Ok(false) => {
          let Step::Grown { previous, walk } = core::mem::replace(
            &mut now.step,
            Step::Reached {
              base: Positions::new(),
              walk: Walk::from(now.head, Positions::new()),
              made_at: false,
            },
          ) else {
            return Ok(Unit::Went);
          };
          let brought = indexed_alone(walk)?;
          Ok(Some(Sets {
            reached: previous.union(&brought),
            brought,
          }))
        }
        Err(refused) => Err(refused),
      },
      Step::Brought { walk, .. } => match self.walk_unit(store, &deriving.push, walk) {
        Ok(true) => Ok(None),
        Ok(false) => {
          let Step::Brought { reached, walk } = core::mem::replace(
            &mut now.step,
            Step::Reached {
              base: Positions::new(),
              walk: Walk::from(now.head, Positions::new()),
              made_at: false,
            },
          ) else {
            return Ok(Unit::Went);
          };
          let brought = indexed_alone(walk)?;
          Ok(Some(Sets { reached, brought }))
        }
        Err(refused) => Err(refused),
      },
    };
    match step_result {
      Ok(Some(sets)) => {
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
    let head = match (record.followed, record.head) {
      (Followed::Emptied, _) | (_, None) => return Ok(Begun::Sets(Sets::default())),
      (_, Some(position)) => self.id_at(store, position).ok_or(StoreRefused::NoVersion {
        green,
        version: crate::to_u64(version),
      })?,
    };
    let unknown = |key: (u64, usize)| -> Result<bool, StoreRefused> {
      Ok(self.value_of(key.0, key.1)?.is_none())
    };
    let now = |step: Step| {
      Begun::Now(Box::new(Now {
        key: (green, version),
        head,
        before,
        step,
      }))
    };
    match record.followed {
      Followed::Grown => {
        if !before {
          return Ok(now(Step::Reached {
            base: Positions::new(),
            walk: Walk::from(head, Positions::new()),
            made_at: false,
          }));
        }
        if unknown(previous)? {
          return Ok(Begun::Needs(previous));
        }
        let previous = self.sets(store, green, version.saturating_sub(1))?.reached;
        Ok(now(Step::Grown {
          walk: Walk::from(head, previous.clone()),
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
          let set = self.base_set(store, (base, record.head), reached);
          return Ok(now(Step::Reached {
            walk: Walk::from(head, set.clone()),
            base: set,
            made_at: true,
          }));
        }
        let push = Push::new(self.format);
        match self.peel(store, &push, head)? {
          Some(commit) => {
            let generation = self.generation(store, &push, &commit)?;
            Ok(now(Step::Base(Descent::back_from(commit, generation))))
          }
          None => Ok(now(Step::Reached {
            base: Positions::new(),
            walk: Walk::from(head, Positions::new()),
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
    (key, _head): ((u64, usize), ObjectId),
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
}

/// What a walk over the committed index alone found: every object it met indexed, since nothing is pushed.
fn indexed_alone(walk: Walk) -> Result<Positions, StoreRefused> {
  let members = walk.members();
  if let Some(id) = members.new.first() {
    return Err(StoreRefused::Missing { id: *id });
  }
  Ok(members.indexed)
}
