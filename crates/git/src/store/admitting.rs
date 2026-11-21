//! A move admitted a unit at a time (A-722 part 4c; main's ruling of 2026-11-20: what a push's decision ran whole held
//! its shard past the watchdog's window at git/git's size).
//!
//! **What.** [`Repository::admit`]'s work (A-614's D(k) and reached set) cut into units a turn takes as many of as its
//! quantum allows, one at least: an object the walk of what a fast-forward brings meets, where its green's sets are
//! known; a commit the fast-forward's descent by generation number meets. A create and a rewrite are admitted with no
//! walk (A-614 amended, main's ruling (A) of 2026-11-21): their sets are the closure of their heads, derived on first
//! read (`super::deriving`), as is a fast-forward's over sets never walked.
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
  /// A move onto a head whose sets are known: what it brings walked against the green's reached set.
  Brought {
    /// The walk.
    walk: Walk,
    /// The commit at the green's head.
    old: ObjectId,
  },
  /// Whether the move is a fast-forward, by descent from its head back by generation number.
  Descent {
    /// What the move brings, walked; none where its green's sets were never walked.
    brought: Option<Members>,
    /// The descent.
    descent: Descent,
  },
  /// Admitted.
  Done(Box<Move>),
  /// Taken.
  Taken,
}

/// A walk of the objects reachable from a head that `stop` does not hold (`Repository::walk`), a unit an object.
#[derive(Debug)]
pub(super) struct Walk {
  stop: Positions,
  pending: Vec<ObjectId>,
  seen: BTreeSet<ObjectId>,
  indexed: Vec<u64>,
  new: Vec<ObjectId>,
  /// The positions the push's stage gives the new objects met, and that stage (its base and how many positions it
  /// gives), while every one met had one under the same stage; none once one had none or the stage was made again.
  /// Kept so the commit that adopts that stage takes them rather than looking each up in its step (one word an object).
  staged: Option<StagedRun>,
}

/// The stage a walk's new objects met (its base and how many positions it gives, once known) and their positions in it.
type StagedRun = (Option<(u64, usize)>, Vec<u64>);

/// A walk back from a commit by generation number, newest first, a unit a commit.
#[derive(Debug)]
pub(super) struct Descent {
  heap: BinaryHeap<(u64, ObjectId)>,
  seen: BTreeSet<ObjectId>,
  /// For a fast-forward's descent: the ancestor sought and its generation number.
  ancestor: Option<(ObjectId, u64)>,
}

impl Walk {
  /// A walk from `from` stopping at what `stop` holds.
  pub(super) fn from(from: ObjectId, stop: Positions) -> Walk {
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
  pub(super) fn members(self) -> Members {
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
    let mut admitting = Admitting {
      green,
      from,
      to,
      phase: Phase::Taken,
    };
    admitting.phase = match (from, chain) {
      (Some(from), Some(_)) => {
        let old = self
          .id_at(store, from)
          .ok_or(StoreRefused::Unreadable { id: to })?;
        match self.current_reached(store, green) {
          Ok(reached) => Phase::Brought {
            walk: Walk::from(to, reached.unwrap_or_default()),
            old,
          },
          // Sets never walked: whether it is a fast-forward is still asked, what it brings is not.
          Err(StoreRefused::Deferred { .. }) => {
            self.descent_begun(store, push, (None, old), &admitting)?
          }
          Err(refused) => return Err(refused),
        }
      }
      _ => self.unwalked(store, push, &admitting)?,
    };
    Ok(admitting)
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
        self.descent_begun(store, push, (Some(brought), old), admitting)
      }
      Phase::Descent {
        brought,
        mut descent,
      } => match self.descent_unit(store, push, &mut descent)? {
        None => Ok(Phase::Descent { brought, descent }),
        Some(true) => {
          self.count(|counters| counters.fast_forwards = counters.fast_forwards.saturating_add(1));
          Ok(self.grown(admitting, brought))
        }
        Some(false) => self.unwalked(store, push, admitting),
      },
      done @ (Phase::Done(_) | Phase::Taken) => Ok(done),
    }
  }

  /// The admitted fast-forward: grown by what it brought where that was walked, else left for its first reader.
  fn grown(&self, admitting: &Admitting, brought: Option<Members>) -> Phase {
    match brought {
      Some(brought) => self.done(admitting, brought, Reach::Grown),
      None => {
        self.count(|counters| counters.deferred = counters.deferred.saturating_add(1));
        self.done(admitting, Members::default(), Reach::GrownUnwalked)
      }
    }
  }

  /// A create or a rewrite admitted with no walk (A-614 amended): its sets the closure of its head, derived on first
  /// read, from the green and version whose head is the move's own head where one is (found by its row, with no walk:
  /// a branch made at another's head), from the nearest derived ancestor otherwise.
  fn unwalked(
    &self,
    store: &ContentStore,
    push: &Push,
    admitting: &Admitting,
  ) -> Result<Phase, StoreRefused> {
    // A head neither pushed nor indexed is refused here, as the walk refused it, before the commit takes the push's
    // stage (a replayed record naming a commit nobody sent, `vcs_store`'s t_a662).
    let from = match self.locate(store, push, &admitting.to) {
      Some(Place::Indexed { row, .. }) => self.heads.get(&row.position).and_then(|greens| {
        let green = greens
          .iter()
          .copied()
          .find(|green| *green != admitting.green)?;
        Some((green, self.chains.get(&green).and_then(Chain::newest)?))
      }),
      Some(Place::Pushed { .. }) => None,
      None => return Err(StoreRefused::Missing { id: admitting.to }),
    };
    self.count(|counters| {
      counters.remade = counters.remade.saturating_add(1);
      counters.deferred = counters.deferred.saturating_add(1);
      if from.is_some() {
        counters.bases_found = counters.bases_found.saturating_add(1);
      }
    });
    Ok(self.done(admitting, Members::default(), Reach::Remade { from }))
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
  pub(super) fn walk_unit(
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
    (brought, old): (Option<Members>, ObjectId),
    admitting: &Admitting,
  ) -> Result<Phase, StoreRefused> {
    let head = admitting.to;
    let commits = (
      self.locate(store, push, &head).map(|place| place.kind()),
      self.locate(store, push, &old).map(|place| place.kind()),
    );
    if commits != (Some(Kind::Commit), Some(Kind::Commit)) {
      return self.unwalked(store, push, admitting);
    }
    let floor = self.generation(store, push, &old)?;
    let top = self.generation(store, push, &head)?;
    if head == old || top <= floor {
      self.count(|counters| {
        counters.decided_without_walk = counters.decided_without_walk.saturating_add(1);
      });
      if head == old {
        self.count(|counters| counters.fast_forwards = counters.fast_forwards.saturating_add(1));
        return Ok(self.grown(admitting, brought));
      }
      return self.unwalked(store, push, admitting);
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
}

impl Descent {
  /// A walk back from `start`, of generation number `generation`, seeking no ancestor: a derivation's search for its
  /// base (`super::deriving`).
  pub(super) fn back_from(start: ObjectId, generation: u64) -> Descent {
    Descent {
      heap: BinaryHeap::from([(generation, start)]),
      seen: BTreeSet::new(),
      ancestor: None,
    }
  }

  /// The next commit of the walk back, newest first, once each; none when none is left.
  pub(super) fn next(&mut self) -> Option<ObjectId> {
    while let Some((_, id)) = self.heap.pop() {
      if self.seen.insert(id) {
        return Some(id);
      }
    }
    None
  }

  /// `id`, a parent of generation number `generation`, to walk back to.
  pub(super) fn push(&mut self, id: ObjectId, generation: u64) {
    self.heap.push((generation, id));
  }
}
