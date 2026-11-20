//! A move admitted a unit at a time (A-722 part 4c; main's ruling of 2026-11-20: what a push's decision ran whole held
//! its shard past the watchdog's window at git/git's size).
//!
//! **What.** [`Repository::admit`]'s work (A-614's D(k) and reached set) cut into units a turn takes as many of as its
//! quantum allows, one at least: an object the walk of what the move brings meets; a commit the fast-forward's descent
//! by generation number meets; a commit the search for a base green meets; an object the walk beyond the base meets.
//! [`Repository::admit`] runs the same units to their end, so a move admitted in one turn or many is the same move, and
//! the store's counters count it alike.
//!
//! **Still true at its append.** A move holds the green's head it was admitted from; the commit refuses one whose green
//! moved since (`check_moves`). Objects the walk found new that another landing indexed meanwhile are positioned by
//! their rows, so a move admitted turns before its append lands as one admitted at it.

use std::collections::{BTreeSet, BinaryHeap};

use slates_vcs::ContentStore;

use super::super::hook_view::Quantum;
use super::super::positions::Positions;
use super::super::push::Push;
use super::{Chain, Members, Move, Place, Reach, Repository, StagedAt, StoreRefused};
use crate::object_name::{Kind, ObjectId};

/// A move's admission under way (module docs).
#[derive(Debug)]
pub struct Admitting {
  /// The green moved.
  green: u64,
  /// Its head when the admission began.
  from: Option<u64>,
  /// Where it moves to.
  to: ObjectId,
  /// What comes next.
  phase: Phase,
}

/// An admission's phase.
#[derive(Debug)]
enum Phase {
  /// A move onto a head: what it brings walked against the green's reached set.
  Brought {
    /// The walk.
    walk: Walk,
    /// The commit at the green's head.
    old: ObjectId,
  },
  /// Whether the move is a fast-forward, by descent from its head back by generation number.
  Descent {
    /// What the move brings.
    brought: Members,
    /// The descent.
    descent: Descent,
  },
  /// A create or a rewrite: the green whose head is the newest commit the move's head descends from.
  Base {
    /// What a rewrite brings; none for a create, which brings all its head reaches.
    brought: Option<Members>,
    /// The search, none when the head peels to no commit.
    search: Option<Descent>,
  },
  /// The walk beyond the base green's reached set.
  Beyond {
    /// What a rewrite brings.
    brought: Option<Members>,
    /// The base set.
    base: Positions,
    /// The base green and version.
    from: Option<(u64, usize)>,
    /// The walk.
    walk: Walk,
  },
  /// Admitted.
  Done(Box<Move>),
  /// Taken.
  Taken,
}

/// A walk of the objects reachable from a head that `stop` does not hold (`Repository::walk`), a unit an object.
#[derive(Debug)]
struct Walk {
  stop: Positions,
  pending: Vec<ObjectId>,
  seen: BTreeSet<ObjectId>,
  indexed: Vec<u64>,
  new: Vec<ObjectId>,
  /// The positions the push's stage gives the new objects met, and that stage (its base and how many positions it
  /// gives), while every one met had one under the same stage; none once one had none or the stage was made again.
  /// Kept so the commit that adopts that stage takes them rather than looking each up in its step (one word an object).
  staged: Option<(Option<(u64, usize)>, Vec<u64>)>,
}

/// A walk back from a commit by generation number, newest first, a unit a commit.
#[derive(Debug)]
struct Descent {
  heap: BinaryHeap<(u64, ObjectId)>,
  seen: BTreeSet<ObjectId>,
  /// For a fast-forward's descent: the ancestor sought and its generation number.
  ancestor: Option<(ObjectId, u64)>,
}

impl Walk {
  /// A walk from `from` stopping at what `stop` holds.
  fn from(from: ObjectId, stop: Positions) -> Walk {
    Walk {
      stop,
      pending: vec![from],
      seen: BTreeSet::new(),
      indexed: Vec::new(),
      new: Vec::new(),
      staged: Some((None, Vec::new())),
    }
  }

  /// Notes the position `push`'s stage gives `id`, a new object met: none kept from now on where it has none, or the
  /// stage is another than the one the walk met before.
  fn note_staged(&mut self, push: &Push, id: &ObjectId) {
    let Some((met, positions)) = self.staged.as_mut() else {
      return;
    };
    let given = push.staged().and_then(|staged| {
      let stage = (staged.base, staged.positions.len());
      let position = staged.positions.get(id).copied()?;
      met
        .is_none_or(|met| met == stage)
        .then_some((stage, position))
    });
    match given {
      Some((stage, position)) => {
        *met = Some(stage);
        positions.push(position);
      }
      None => self.staged = None,
    }
  }

  /// What the walk found, once its pending objects are none.
  fn members(self) -> Members {
    let staged = match self.staged {
      Some((Some(stage), positions)) => Some(StagedAt {
        stage,
        positions: Positions::of(positions),
      }),
      _ => None,
    };
    Members {
      indexed: Positions::of(self.indexed),
      new: self.new,
      staged,
    }
  }
}

impl Admitting {
  /// The move admitted, once its admission is done.
  pub fn admitted(&mut self) -> Option<Move> {
    match core::mem::replace(&mut self.phase, Phase::Taken) {
      Phase::Done(admitted) => Some(*admitted),
      other => {
        self.phase = other;
        None
      }
    }
  }

  /// The green the move moves and the head it was admitted from: what an append's move must still be to take it.
  pub fn moves(&self) -> (u64, Option<u64>, ObjectId) {
    (self.green, self.from, self.to)
  }
}

impl Repository {
  /// The admission of `green`'s move to `to` begun (module docs), read from the sealed `push` and the index: a delete
  /// is admitted at once ([`Repository::admit`]).
  pub fn admitting(
    &self,
    store: &ContentStore,
    push: &Push,
    green: u64,
    to: ObjectId,
  ) -> Result<Admitting, StoreRefused> {
    self.same_format(push)?;
    if !push.is_sealed() {
      return Err(StoreRefused::Unsealed);
    }
    let chain = self.chains.get(&green);
    let from = chain.and_then(Chain::head);
    let phase = match (from, chain) {
      (Some(from), Some(_)) => {
        let reached = self.current_reached(store, green)?.unwrap_or_default();
        let old = self
          .id_at(store, from)
          .ok_or(StoreRefused::Unreadable { id: to })?;
        Phase::Brought {
          walk: Walk::from(to, reached),
          old,
        }
      }
      _ => self.base_begun(store, push, None, to)?,
    };
    Ok(Admitting {
      green,
      from,
      to,
      phase,
    })
  }

  /// One turn of `admitting` (module docs): a unit at least, then units until the quantum is spent. Whether the move
  /// is admitted ([`Admitting::admitted`] takes it).
  pub fn admit_turn(
    &self,
    store: &ContentStore,
    push: &Push,
    admitting: &mut Admitting,
    quantum: Quantum<'_>,
  ) -> Result<bool, StoreRefused> {
    let deadline = quantum.clock.now_ns().saturating_add(quantum.ns);
    loop {
      let phase = core::mem::replace(&mut admitting.phase, Phase::Taken);
      admitting.phase = self.admission_unit(store, push, admitting, phase)?;
      if matches!(admitting.phase, Phase::Done(_)) {
        return Ok(true);
      }
      if quantum.clock.now_ns() >= deadline {
        return Ok(false);
      }
    }
  }

  /// One unit of `phase`: the phase after it.
  fn admission_unit(
    &self,
    store: &ContentStore,
    push: &Push,
    admitting: &Admitting,
    phase: Phase,
  ) -> Result<Phase, StoreRefused> {
    match phase {
      Phase::Brought { mut walk, old } => {
        if self.walk_unit(store, push, &mut walk)? {
          return Ok(Phase::Brought { walk, old });
        }
        let brought = walk.members();
        self.descent_begun(store, push, (brought, old), admitting)
      }
      Phase::Descent {
        brought,
        mut descent,
      } => match self.descent_unit(store, push, &mut descent)? {
        None => Ok(Phase::Descent { brought, descent }),
        Some(true) => {
          self.count(|counters| counters.fast_forwards = counters.fast_forwards.saturating_add(1));
          Ok(self.done(admitting, brought, Reach::Grown))
        }
        Some(false) => self.base_begun(store, push, Some(brought), admitting.to),
      },
      Phase::Base {
        brought,
        mut search,
      } => {
        let found = match search.as_mut() {
          Some(search) => match self.base_unit(store, push, search)? {
            None => {
              return Ok(Phase::Base {
                brought,
                search: Some(core::mem::replace(search, Descent::empty())),
              });
            }
            Some(found) => found,
          },
          None => None,
        };
        self.beyond_begun(store, brought, found, admitting.to)
      }
      Phase::Beyond {
        brought,
        base,
        from,
        mut walk,
      } => {
        if self.walk_unit(store, push, &mut walk)? {
          return Ok(Phase::Beyond {
            brought,
            base,
            from,
            walk,
          });
        }
        let beyond = walk.members();
        self.count(|counters| counters.remade = counters.remade.saturating_add(1));
        let brought = match brought {
          Some(brought) => brought,
          None => Members {
            indexed: base.union(&beyond.indexed),
            new: beyond.new.clone(),
            staged: beyond.staged.clone(),
          },
        };
        Ok(self.done(admitting, brought, Reach::Remade { base, beyond, from }))
      }
      done @ (Phase::Done(_) | Phase::Taken) => Ok(done),
    }
  }

  /// The admitted move.
  fn done(&self, admitting: &Admitting, brought: Members, reach: Reach) -> Phase {
    Phase::Done(Box::new(Move {
      green: admitting.green,
      from: admitting.from,
      to: Some(admitting.to),
      brought,
      reach,
    }))
  }

  /// One object of `walk` (`Repository::walk`'s loop body): whether more remain.
  fn walk_unit(
    &self,
    store: &ContentStore,
    push: &Push,
    walk: &mut Walk,
  ) -> Result<bool, StoreRefused> {
    let Some(id) = walk.pending.pop() else {
      return Ok(false);
    };
    if !walk.seen.insert(id) {
      return Ok(true);
    }
    let place = self
      .locate(store, push, &id)
      .ok_or(StoreRefused::Missing { id })?;
    match &place {
      Place::Indexed { row, .. } if walk.stop.contains(row.position) => {
        self.met_reached(row.kind);
        return Ok(true);
      }
      Place::Indexed { row, .. } => walk.indexed.push(row.position),
      Place::Pushed { .. } => {
        walk.new.push(id);
        walk.note_staged(push, &id);
      }
    }
    self.count(|counters| counters.walked = counters.walked.saturating_add(1));
    walk
      .pending
      .extend(self.references(store, push, &id, &place)?);
    Ok(true)
  }

  /// The fast-forward's question begun (`Repository::fast_forward`): decided at once where the ids or the generation
  /// numbers decide it, a descent otherwise.
  fn descent_begun(
    &self,
    store: &ContentStore,
    push: &Push,
    (brought, old): (Members, ObjectId),
    admitting: &Admitting,
  ) -> Result<Phase, StoreRefused> {
    let head = admitting.to;
    let commits = (
      self.locate(store, push, &head).map(|place| place.kind()),
      self.locate(store, push, &old).map(|place| place.kind()),
    );
    if commits != (Some(Kind::Commit), Some(Kind::Commit)) {
      return self.base_begun(store, push, Some(brought), head);
    }
    let floor = self.generation(store, push, &old)?;
    let top = self.generation(store, push, &head)?;
    if head == old || top <= floor {
      self.count(|counters| {
        counters.decided_without_walk = counters.decided_without_walk.saturating_add(1);
      });
      if head == old {
        self.count(|counters| counters.fast_forwards = counters.fast_forwards.saturating_add(1));
        return Ok(self.done(admitting, brought, Reach::Grown));
      }
      return self.base_begun(store, push, Some(brought), head);
    }
    self.count(|counters| counters.descent_walks = counters.descent_walks.saturating_add(1));
    Ok(Phase::Descent {
      brought,
      descent: Descent {
        heap: BinaryHeap::from([(top, head)]),
        seen: BTreeSet::new(),
        ancestor: Some((old, floor)),
      },
    })
  }

  /// One commit of a fast-forward's descent: whether the ancestor was met, once decided.
  fn descent_unit(
    &self,
    store: &ContentStore,
    push: &Push,
    descent: &mut Descent,
  ) -> Result<Option<bool>, StoreRefused> {
    let Some((ancestor, floor)) = descent.ancestor else {
      return Ok(Some(false));
    };
    let Some((generation, id)) = descent.heap.pop() else {
      return Ok(Some(false));
    };
    if id == ancestor {
      return Ok(Some(true));
    }
    if generation <= floor || !descent.seen.insert(id) {
      return Ok(None);
    }
    for parent in self.parents(store, push, &id)? {
      descent
        .heap
        .push((self.generation(store, push, &parent)?, parent));
    }
    Ok(None)
  }

  /// The search for a base green begun (`Repository::base`): none where the head peels to no commit.
  fn base_begun(
    &self,
    store: &ContentStore,
    push: &Push,
    brought: Option<Members>,
    head: ObjectId,
  ) -> Result<Phase, StoreRefused> {
    let search = match self.peel(store, push, head)? {
      Some(start) => Some(Descent {
        heap: BinaryHeap::from([(self.generation(store, push, &start)?, start)]),
        seen: BTreeSet::new(),
        ancestor: None,
      }),
      None => None,
    };
    Ok(Phase::Base { brought, search })
  }

  /// One commit of the base search: the green found (or none), once decided.
  fn base_unit(
    &self,
    store: &ContentStore,
    push: &Push,
    search: &mut Descent,
  ) -> Result<Option<Option<u64>>, StoreRefused> {
    let Some((_, id)) = search.heap.pop() else {
      return Ok(Some(None));
    };
    if !search.seen.insert(id) {
      return Ok(None);
    }
    if let Some(green) = self.green_at(store, &id) {
      return Ok(Some(Some(green)));
    }
    for parent in self.parents(store, push, &id)? {
      search
        .heap
        .push((self.generation(store, push, &parent)?, parent));
    }
    Ok(None)
  }

  /// The walk beyond the base begun (`Repository::remade`): the base green's newest reached set, or none.
  fn beyond_begun(
    &self,
    store: &ContentStore,
    brought: Option<Members>,
    found: Option<u64>,
    to: ObjectId,
  ) -> Result<Phase, StoreRefused> {
    let (base, from) = match found {
      Some(green) => {
        self.count(|counters| counters.bases_found = counters.bases_found.saturating_add(1));
        let version = self.chains.get(&green).and_then(Chain::newest);
        let set = match version {
          Some(version) => self.sets(store, green, version)?.reached,
          None => Positions::new(),
        };
        (set, version.map(|version| (green, version)))
      }
      None => (Positions::new(), None),
    };
    Ok(Phase::Beyond {
      brought,
      walk: Walk::from(to, base.clone()),
      base,
      from,
    })
  }
}

impl Descent {
  /// A descent with nothing left: what a phase holds while its own is borrowed out.
  fn empty() -> Descent {
    Descent {
      heap: BinaryHeap::new(),
      seen: BTreeSet::new(),
      ancestor: None,
    }
  }
}
