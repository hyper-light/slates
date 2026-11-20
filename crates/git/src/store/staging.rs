//! A push sealed and staged a unit at a time (A-722 part 4; main's ruling of 2026-11-20: what a push's landing still ran
//! whole in one step held its shard past the watchdog's window at git/git's size).
//!
//! **What.** [`Repository::stage`]'s work (A-722 part 3), and the seal before it ([`Repository::seal`]), cut into units
//! a turn takes as many of as its quantum allows, one at least: a commit's generation number; a held object checked
//! against the index (new, or held already); a new object given its position; a new object's two rows, its object row
//! and its position row, inserted into the fork; one table of the fork's tail sealed ([`Index::seal_step`]). The
//! positions are A-639's, by type and then id, given without sorting the push: the new objects are met in their ids'
//! order and counted by type first, so each type's run starts where the types before it end and each object takes its
//! type's next position.
//!
//! **One layout.** Rows inserted one at a time make the tail an insert of them all at once makes, and a tail sealed a
//! table a step makes the run a whole seal makes, so a push staged in many turns or in one ([`Repository::stage`] runs
//! the same units to their end) leaves the same tables: a store made again from the log is the live one.
//!
//! **Given back.** A staging refused gives back its fork; one abandoned before its end is given back by
//! [`Staging::abandon`].

use std::collections::BTreeMap;

use slates_vcs::{ContentId, ContentStore, Index, NewRow};

use super::super::hook_view::Quantum;
use super::super::learn::Clock;
use super::super::naming::kind_of;
use super::super::push::{Push, Staged};
use super::super::rows::{self, ObjectRow};
use super::{Repository, StoreRefused};
use crate::object_name::{Kind, ObjectId};

/// A new object as staging meets it: its id, type, root content and length.
type Fresh = (ObjectId, Kind, ContentId, u64);

/// A push's seal and stage under way (module docs).
#[derive(Debug)]
pub struct Staging {
  /// What comes next.
  step: Step,
}

/// A staging's step.
#[derive(Debug)]
enum Step {
  /// Each commit's generation number, a commit a unit.
  Generations {
    /// The push's commits.
    ids: Vec<ObjectId>,
    /// The next to number.
    next: usize,
    /// The numbers so far.
    done: BTreeMap<ObjectId, u64>,
  },
  /// Each held object checked against the index, an object a unit.
  Fresh {
    /// The last object checked.
    after: Option<ObjectId>,
    /// The new objects, in their ids' order.
    fresh: Vec<Fresh>,
    /// The new objects of each type.
    kinds: BTreeMap<Kind, u64>,
  },
  /// Each new object given its position, an object a unit.
  Positions {
    /// The new objects.
    fresh: Vec<Fresh>,
    /// The next one to place.
    at: usize,
    /// Each type's next position.
    next_of: BTreeMap<Kind, u64>,
    /// The positions given.
    positions: BTreeMap<ObjectId, u64>,
  },
  /// Each new object's rows inserted into the fork, an object a unit.
  Rows {
    /// The new objects.
    fresh: Vec<Fresh>,
    /// The next one to write.
    at: usize,
    /// The positions given.
    positions: BTreeMap<ObjectId, u64>,
    /// The fork written into.
    fork: Index,
  },
  /// The fork's tail sealed, a table a unit.
  Sealing {
    /// The positions given.
    positions: BTreeMap<ObjectId, u64>,
    /// The fork sealed.
    fork: Index,
  },
  /// Staged, or given back.
  Ended,
}

/// A clock that never moves: a staging run to its end in one call ([`Repository::stage`]).
pub(super) struct Unbounded;

impl Clock for Unbounded {
  fn now_ns(&mut self) -> u64 {
    0
  }
}

impl Staging {
  /// Gives back what a staging ended before its end holds: its fork.
  pub fn abandon(self, store: &mut ContentStore) -> Result<(), StoreRefused> {
    match self.step {
      Step::Rows { fork, .. } | Step::Sealing { fork, .. } => {
        fork.release(store).map_err(StoreRefused::from)
      }
      _ => Ok(()),
    }
  }

  /// Whether the staging has ended.
  pub fn is_ended(&self) -> bool {
    matches!(self.step, Step::Ended)
  }
}

impl Repository {
  /// A seal and stage of `push` begun (module docs): its commits numbered, then its new objects staged. A push staged
  /// before gives its stage back first.
  pub fn sealing(
    &self,
    store: &mut ContentStore,
    push: &mut Push,
  ) -> Result<Staging, StoreRefused> {
    self.same_format(push)?;
    if let Some(old) = push.take_staged() {
      old.fork.release(store)?;
    }
    Ok(Staging {
      step: Step::Generations {
        ids: push.commits().keys().copied().collect(),
        next: 0,
        done: BTreeMap::new(),
      },
    })
  }

  /// A stage of `push`, sealed, begun (module docs): its new objects staged. A push staged before gives its stage back
  /// first.
  pub fn staging(
    &self,
    store: &mut ContentStore,
    push: &mut Push,
  ) -> Result<Staging, StoreRefused> {
    self.same_format(push)?;
    if !push.is_sealed() {
      return Err(StoreRefused::Unsealed);
    }
    if let Some(old) = push.take_staged() {
      old.fork.release(store)?;
    }
    Ok(Staging {
      step: Step::Fresh {
        after: None,
        fresh: Vec::new(),
        kinds: BTreeMap::new(),
      },
    })
  }

  /// One turn of `staging` (module docs): a unit at least, then units until the quantum is spent. Whether `push` is
  /// staged. Refused, the staging is given back and ended.
  pub fn stage_turn(
    &mut self,
    store: &mut ContentStore,
    push: &mut Push,
    staging: &mut Staging,
    quantum: Quantum<'_>,
  ) -> Result<bool, StoreRefused> {
    let deadline = quantum.clock.now_ns().saturating_add(quantum.ns);
    loop {
      let step = core::mem::replace(&mut staging.step, Step::Ended);
      match self.unit(store, push, step) {
        Ok(Step::Ended) => return Ok(true),
        Ok(next) => staging.step = next,
        Err(refused) => return Err(refused),
      }
      if quantum.clock.now_ns() >= deadline {
        return Ok(false);
      }
    }
  }

  /// [`Self::stage_turn`]s to the end in one call: `staging` whole.
  pub(super) fn stage_whole(
    &mut self,
    store: &mut ContentStore,
    push: &mut Push,
    mut staging: Staging,
  ) -> Result<(), StoreRefused> {
    let mut clock = Unbounded;
    while !self.stage_turn(
      store,
      push,
      &mut staging,
      Quantum {
        clock: &mut clock,
        ns: u64::MAX,
      },
    )? {}
    Ok(())
  }

  /// One unit of `step`: the step after it. Refused, what the step held is given back.
  fn unit(
    &mut self,
    store: &mut ContentStore,
    push: &mut Push,
    step: Step,
  ) -> Result<Step, StoreRefused> {
    match step {
      Step::Generations {
        ids,
        next,
        mut done,
      } => {
        let Some(id) = ids.get(next).copied() else {
          push.seal_with(&done);
          return Ok(Step::Fresh {
            after: None,
            fresh: Vec::new(),
            kinds: BTreeMap::new(),
          });
        };
        self.generations_from(store, push, id, &mut done)?;
        Ok(Step::Generations {
          ids,
          next: next.saturating_add(1),
          done,
        })
      }
      Step::Fresh {
        after,
        mut fresh,
        mut kinds,
      } => {
        let held = push.held_after(after.as_ref().map(ObjectId::as_bytes));
        let Some((bytes, number, content, len)) = held else {
          return self.positions_begun(fresh, &kinds);
        };
        let id = ObjectId::from_bytes(self.format, bytes).ok_or(StoreRefused::ForeignPush)?;
        if self.indexed(store, &id).is_none() {
          let kind = kind_of(number).ok_or(StoreRefused::Malformed { id })?;
          fresh.push((id, kind, content, len));
          let count = kinds.entry(kind).or_insert(0);
          *count = count.saturating_add(1);
        }
        Ok(Step::Fresh {
          after: Some(id),
          fresh,
          kinds,
        })
      }
      Step::Positions {
        fresh,
        at,
        mut next_of,
        mut positions,
      } => {
        let Some((id, kind, ..)) = fresh.get(at).copied() else {
          let fork = self.index.fork(store)?;
          return Ok(Step::Rows {
            fresh,
            at: 0,
            positions,
            fork,
          });
        };
        let next = next_of
          .get_mut(&kind)
          .ok_or(StoreRefused::Malformed { id })?;
        positions.insert(id, *next);
        *next = next.saturating_add(1);
        Ok(Step::Positions {
          fresh,
          at: at.saturating_add(1),
          next_of,
          positions,
        })
      }
      Step::Rows {
        fresh,
        at,
        positions,
        mut fork,
      } => {
        let Some(object) = fresh.get(at).copied() else {
          return Ok(Step::Sealing { positions, fork });
        };
        if let Err(refused) = self.rows_of(store, push, &positions, object, &mut fork) {
          fork.release(store)?;
          return Err(refused);
        }
        Ok(Step::Rows {
          fresh,
          at: at.saturating_add(1),
          positions,
          fork,
        })
      }
      Step::Sealing {
        positions,
        mut fork,
      } => match fork.seal_step(store) {
        Ok(false) => Ok(Step::Sealing { positions, fork }),
        Ok(true) => {
          push.set_staged(Staged {
            fork,
            positions,
            base: self.next_position,
          });
          Ok(Step::Ended)
        }
        Err(refused) => {
          fork.release(store)?;
          Err(refused.into())
        }
      },
      Step::Ended => Ok(Step::Ended),
    }
  }

  /// The positions step begun over `fresh`: each type's run starting where the types before it, in their order, end,
  /// from the repository's next position (A-639: by type, then id).
  fn positions_begun(
    &self,
    fresh: Vec<Fresh>,
    kinds: &BTreeMap<Kind, u64>,
  ) -> Result<Step, StoreRefused> {
    let mut next_of = BTreeMap::new();
    let mut start = self.next_position;
    for (kind, count) in kinds {
      next_of.insert(*kind, start);
      start = start
        .checked_add(*count)
        .filter(|end| *end < u64::MAX)
        .ok_or(StoreRefused::PositionsExhausted)?;
    }
    Ok(Step::Positions {
      fresh,
      at: 0,
      next_of,
      positions: BTreeMap::new(),
    })
  }

  /// Inserts `object`'s rows into `fork`: its object row, with a commit's part, and its position row.
  fn rows_of(
    &self,
    store: &mut ContentStore,
    push: &Push,
    positions: &BTreeMap<ObjectId, u64>,
    (id, kind, content, len): Fresh,
    fork: &mut Index,
  ) -> Result<(), StoreRefused> {
    let position = self.position_of(store, positions, &id)?;
    let row = ObjectRow {
      kind,
      len,
      position,
      commit: self.commit_row(store, push, positions, &id, kind)?,
    };
    let key = rows::object_key(&id);
    let payload = rows::encode(&row);
    let contents = [content];
    fork.insert(
      store,
      NewRow {
        key: &key,
        contents: &contents,
        payload: &payload,
      },
    )?;
    let key = rows::position_key(position);
    fork.insert(
      store,
      NewRow {
        key: &key,
        contents: &[],
        payload: id.as_bytes(),
      },
    )?;
    Ok(())
  }
}
