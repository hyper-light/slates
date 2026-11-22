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

use slates_vcs::{ContentId, ContentStore, Index, NewRow, Writing};

use super::super::hook_view::Quantum;
use super::super::learn::Clock;
use super::super::naming::kind_of;
use super::super::push::{Push, Staged, StoredAt};
use super::super::rows::{self, ObjectRow};
use super::{Repository, StoreRefused};
use crate::object_name::{Kind, ObjectId};

/// A new object as staging meets it: its id, type, the content its row names (its own, or the pack it lies in), its
/// length, and for one read from a pack, its entry there ([`InPackFresh`]).
type Fresh = (ObjectId, Kind, ContentId, u64, Option<InPackFresh>);

/// A new object's entry in the pack it lies in, as staging meets it (A-864): where it begins, its raw length, the CRC32
/// of its raw bytes, and a delta's base by name, its position given once the rows are written.
type InPackFresh = (u64, u64, u32, Option<ObjectId>);

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
  /// The push's pack put in the content store as one content, a window a unit (A-722's record v4: each object's row
  /// names where it lies there, never a second copy inflated); none put for a push whose pack brought no object, or
  /// whose pack is put already (a stage made again).
  Packing {
    /// The put under way, boxed (its hasher is most of a step's size); none before its first window.
    writing: Option<Box<Writing>>,
    /// The pack's bytes put so far.
    at: usize,
  },
  /// The fork the rows are written into made, and the next position read: what the staging is current against. A
  /// landing after it moves the index's rows or its next position, so the commit stages the push again (A-722 part
  /// 4b's race, found by reading after the 100-push run: a fork made at the rows' step met rows another push landed
  /// meanwhile).
  Forking,
  /// Each held object checked against the index, an object a unit.
  Fresh {
    /// The fork, made as the staging began.
    fork: Index,
    /// The next position as the fork was made.
    base: u64,
    /// The last object checked.
    after: Option<ObjectId>,
    /// The new objects, in their ids' order.
    fresh: Vec<Fresh>,
    /// The new objects of each type.
    kinds: BTreeMap<Kind, u64>,
  },
  /// Each new object given its position, an object a unit.
  Positions {
    /// The fork.
    fork: Index,
    /// The next position as the fork was made.
    base: u64,
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
    /// The next position as the fork was made.
    base: u64,
    /// The runs of positions given each type (`Staged::kinds`).
    kinds: Vec<(u64, u64, Kind)>,
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
    /// The next position as the fork was made.
    base: u64,
    /// The runs of positions given each type.
    kinds: Vec<(u64, u64, Kind)>,
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
      Step::Packing {
        writing: Some(writing),
        ..
      } => {
        store.abandon_object(*writing);
        Ok(())
      }
      Step::Fresh { fork, .. }
      | Step::Positions { fork, .. }
      | Step::Rows { fork, .. }
      | Step::Sealing { fork, .. } => fork.release(store).map_err(StoreRefused::from),
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
      step: Step::Packing {
        writing: None,
        at: 0,
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

  /// The content the row of the push's object `id` names and its entry there: its own content, held whole; or the push's
  /// pack its staging put in the store ([`Step::Packing`]) and the entry, its place, raw length, CRC32 and base found
  /// from the pack the push holds (A-864, `Push::entry_of`).
  fn stored_for_row(
    push: &Push,
    id: &ObjectId,
  ) -> Result<(ContentId, Option<InPackFresh>), StoreRefused> {
    match push.stored(id) {
      Some(StoredAt::Whole(content)) => Ok((content, None)),
      Some(StoredAt::InPack(place)) => {
        let (len, crc, base) = push
          .entry_of(id)
          .ok_or(StoreRefused::Malformed { id: *id })?;
        Ok((place.pack, Some((place.offset, len, crc, base))))
      }
      None => Err(StoreRefused::Unreadable { id: *id }),
    }
  }

  /// One unit of the fresh objects' census: the next object the push holds after `after`, kept with its kind when the
  /// store has not indexed it; the positions begun once none is left. Refused, the fork is given back.
  fn fresh_unit(
    &mut self,
    store: &mut ContentStore,
    push: &mut Push,
    (fork, base, after): (Index, u64, Option<ObjectId>),
    (mut fresh, mut kinds): (Vec<Fresh>, BTreeMap<Kind, u64>),
  ) -> Result<Step, StoreRefused> {
    let Some((id, held)) = push.held_after(after.as_ref()) else {
      return self.positions_begun((fork, base), fresh, &kinds);
    };
    if self.indexed(store, &id).is_none() {
      let Some(kind) = kind_of(held.number) else {
        fork.release(store)?;
        return Err(StoreRefused::Malformed { id });
      };
      // An object of the push's pack is named by where it lies in the pack its staging put in the store (A-722's
      // record v4); one held whole, by its own content.
      let (content, offset) = match Self::stored_for_row(push, &id) {
        Ok(stored) => stored,
        Err(refused) => {
          fork.release(store)?;
          return Err(refused);
        }
      };
      fresh.push((id, kind, content, held.len, offset));
      let count = kinds.entry(kind).or_insert(0);
      *count = count.saturating_add(1);
    }
    Ok(Step::Fresh {
      fork,
      base,
      after: Some(id),
      fresh,
      kinds,
    })
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
          return Ok(Step::Packing {
            writing: None,
            at: 0,
          });
        };
        self.generations_from(store, push, id, &mut done)?;
        Ok(Step::Generations {
          ids,
          next: next.saturating_add(1),
          done,
        })
      }
      Step::Packing { writing, at } => packing_unit(store, push, (writing, at)),
      Step::Forking => Ok(Step::Fresh {
        fork: self.index.fork(store)?,
        base: self.next_position,
        after: None,
        fresh: Vec::new(),
        kinds: BTreeMap::new(),
      }),
      Step::Fresh {
        fork,
        base,
        after,
        fresh,
        kinds,
      } => self.fresh_unit(store, push, (fork, base, after), (fresh, kinds)),
      Step::Positions {
        fork,
        base,
        fresh,
        at,
        next_of,
        positions,
      } => positions_unit(store, (fork, base), (fresh, at), (next_of, positions)),
      Step::Rows {
        base,
        kinds,
        fresh,
        at,
        positions,
        mut fork,
      } => {
        let Some(object) = fresh.get(at).copied() else {
          return Ok(Step::Sealing {
            base,
            kinds,
            positions,
            fork,
          });
        };
        if let Err(refused) = self.rows_of(store, push, &positions, object, &mut fork) {
          fork.release(store)?;
          return Err(refused);
        }
        Ok(Step::Rows {
          base,
          kinds,
          fresh,
          at: at.saturating_add(1),
          positions,
          fork,
        })
      }
      Step::Sealing {
        base,
        kinds,
        positions,
        mut fork,
      } => match fork.seal_step(store) {
        Ok(false) => Ok(Step::Sealing {
          base,
          kinds,
          positions,
          fork,
        }),
        Ok(true) => {
          push.set_staged(Staged {
            fork,
            positions,
            base,
            kinds,
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
  /// from the next position as the fork was made (A-639: by type, then id).
  fn positions_begun(
    &self,
    (fork, base): (Index, u64),
    fresh: Vec<Fresh>,
    kinds: &BTreeMap<Kind, u64>,
  ) -> Result<Step, StoreRefused> {
    let mut next_of = BTreeMap::new();
    let mut start = base;
    for (kind, count) in kinds {
      next_of.insert(*kind, start);
      start = start
        .checked_add(*count)
        .filter(|end| *end < u64::MAX)
        .ok_or(StoreRefused::PositionsExhausted)?;
    }
    Ok(Step::Positions {
      fork,
      base,
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
    (id, kind, content, len, offset): Fresh,
    fork: &mut Index,
  ) -> Result<(), StoreRefused> {
    let position = self.position_of(store, positions, &id)?;
    let pack = match offset {
      Some((offset, raw, crc, base)) => Some(rows::PackedRow {
        offset,
        len: raw,
        crc,
        base: base
          .map(|base| self.position_of(store, positions, &base))
          .transpose()?,
      }),
      None => None,
    };
    let row = ObjectRow {
      kind,
      len,
      position,
      pack,
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

/// One unit of [`Step::Packing`]: the next window of the push's pack put in the content store, or, past the last, the
/// put finished and kept as the push's stored pack ([`Push::set_stored_pack`]) and the fork made next. A push whose
/// pack brought no object, or whose pack is put already, puts none. Refused, what the put kept is given back.
fn packing_unit(
  store: &mut ContentStore,
  push: &mut Push,
  (writing, at): (Option<Box<Writing>>, usize),
) -> Result<Step, StoreRefused> {
  if push.stored_pack().is_some() || !push.has_placed() {
    return Ok(Step::Forking);
  }
  let mut writing = writing.unwrap_or_else(|| Box::new(store.begin_object()));
  let end = at.saturating_add(store.window()).min(push.pack().len());
  if let Some(piece) = push.pack().get(at..end).filter(|piece| !piece.is_empty()) {
    if let Err(refused) = store.put_window(&mut writing, piece) {
      store.abandon_object(*writing);
      return Err(refused.into());
    }
    return Ok(Step::Packing {
      writing: Some(writing),
      at: end,
    });
  }
  let identity = match store.finish_object(&mut writing) {
    Ok(identity) => identity,
    Err(refused) => {
      store.abandon_object(*writing);
      return Err(refused.into());
    }
  };
  let len = u64::try_from(push.pack().len()).unwrap_or(u64::MAX);
  push.set_stored_pack(identity, len);
  Ok(Step::Forking)
}

/// One unit of [`Step::Positions`]: the fresh object at `at` given its kind's next position, or, past the last, the
/// rows begun. An object of a kind the push did not count is malformed, and the fork is given back.
fn positions_unit(
  store: &mut ContentStore,
  (fork, base): (Index, u64),
  (fresh, at): (Vec<Fresh>, usize),
  (mut next_of, mut positions): (BTreeMap<Kind, u64>, BTreeMap<ObjectId, u64>),
) -> Result<Step, StoreRefused> {
  let Some((id, kind, ..)) = fresh.get(at).copied() else {
    // Each type's positions run from where the types before it, in their order, end to its own next (A-639).
    let mut start = base;
    let kinds = next_of
      .iter()
      .filter_map(|(kind, end)| {
        let run = (start < *end).then_some((start, *end, *kind));
        start = *end;
        run
      })
      .collect();
    return Ok(Step::Rows {
      base,
      kinds,
      fresh,
      at: 0,
      positions,
      fork,
    });
  };
  let Some(next) = next_of.get_mut(&kind) else {
    fork.release(store)?;
    return Err(StoreRefused::Malformed { id });
  };
  positions.insert(id, *next);
  *next = next.saturating_add(1);
  Ok(Step::Positions {
    fork,
    base,
    fresh,
    at: at.saturating_add(1),
    next_of,
    positions,
  })
}
