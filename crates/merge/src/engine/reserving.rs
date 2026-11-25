//! A work's retention reservation taken a slice at a time (A-920 piece 3b): [`Green::history_reservation`] plus
//! [`Green::changes_reservation`], the sum a batch's room check and the verdict's admission charge, walked a unit at a
//! time. Its units are the work's ops, each path they name collected once with what the two reservations ask of it;
//! then those paths, each one's history and change-index charge ([`Green::reserving_step`]); then the shares of the
//! work's copies and replaces, whole ([`Green::reservation_shares`]: nothing for a work with none, which an import's
//! revision of 9,000 files is; the walks were 30 ms of its judge step whole, on a debug build). The walks read the
//! work's paths and kinds alone; the shares read its copies' versions, in the green's numbering.
//!
//! **Contract.** Between its begin and its end neither the green nor the work changes: an import batch's scratch member
//! and its pending work, under the landing turn. The sum is the two reservations' whole: each charge is a saturating
//! sum of charges that are never negative, so the order it is taken in does not change it.

use std::collections::BTreeMap;

use super::{Green, Increment, SPECIAL_NODE_BYTES, change_bytes, is_content};
use crate::ops_doc::OpKind;

/// What the two reservations ask of one path the work's ops name.
#[derive(Clone, Copy, Debug, Default)]
struct Asked {
  /// The content ops on it ([`Green::history_reservation`]'s edits).
  edits: usize,
  /// Whether an op replaces what stands there (a create, an unlink, a rename onto or away from it, a node made).
  replaced: bool,
  /// Whether the change index records it ([`Green::changes_reservation`]'s paths).
  indexed: bool,
}

/// Where a reservation stands between its steps.
#[derive(Debug)]
enum Phase {
  /// Walking the work's ops: the next.
  Ops(usize),
  /// Charging the paths they named.
  Paths,
  /// Walked: the copies' and replaces' shares are its caller's to add ([`Green::reservation_shares`]).
  Walked,
}

/// A work's reservation being taken a slice at a time ([`Green::begin_reserving`], [`Green::reserving_step`]).
#[derive(Debug)]
pub struct Reserving {
  /// Where it stands.
  phase: Phase,
  /// Each path the ops named, with what is asked of it; taken off as it is charged.
  asked: BTreeMap<String, Asked>,
  /// The sum so far.
  total: usize,
}

impl Green {
  /// Begins `inc`'s reservation ([`Green::history_reservation`] plus [`Green::changes_reservation`]) a slice at a time
  /// ([`Green::reserving_step`]): nothing to walk for an increment already accepted, which reserves nothing.
  pub fn begin_reserving(&self, inc: &Increment) -> Reserving {
    let phase = if self.accepted.contains_key(&inc.id) {
      Phase::Walked
    } else {
      Phase::Ops(0)
    };
    Reserving {
      phase,
      asked: BTreeMap::new(),
      total: 0,
    }
  }

  /// Takes `reserving` on, a unit at least, until `spent`: once its walks end, what they reserve, to which
  /// [`Green::reservation_shares`] adds the copies' and replaces'. `inc` is the increment it began on, unchanged.
  pub fn reserving_step(
    &self,
    inc: &Increment,
    reserving: &mut Reserving,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<usize> {
    loop {
      match reserving.phase {
        Phase::Ops(at) => reserving.phase = Self::ask_ops(inc, reserving, (at, spent)),
        Phase::Paths => {
          if !self.charge_paths(reserving, spent) {
            return None;
          }
          reserving.phase = Phase::Walked;
        }
        Phase::Walked => return Some(reserving.total),
      }
      if matches!(reserving.phase, Phase::Ops(_)) {
        return None;
      }
    }
  }

  /// The work's ops from `at` noted in `reserving`, until `spent`: where the walk stands after.
  fn ask_ops(
    inc: &Increment,
    reserving: &mut Reserving,
    (mut at, spent): (usize, &mut dyn FnMut() -> bool),
  ) -> Phase {
    while let Some(op) = inc.doc.ops.get(at) {
      let path = inc.doc.paths.path(op.path);
      if let Some(path) = path {
        let asked = reserving.asked.entry(path.to_owned()).or_default();
        asked.indexed = true;
        asked.edits = asked.edits.saturating_add(usize::from(is_content(op.kind)));
        asked.replaced |= matches!(
          op.kind,
          OpKind::Create | OpKind::Unlink | OpKind::Rename | OpKind::Mknod
        );
      }
      if op.kind == OpKind::Rename
        && let Some(source) = inc.doc.paths.path(op.src)
      {
        // The change index names a rename's source whatever its destination; the history reserves for it only
        // where the destination names a path.
        let asked = reserving.asked.entry(source.to_owned()).or_default();
        asked.indexed = true;
        asked.replaced |= path.is_some();
      }
      at = at.saturating_add(1);
      if at < inc.doc.ops.len() && spent() {
        return Phase::Ops(at);
      }
    }
    Phase::Paths
  }

  /// The paths `reserving` collected charged, a path a unit, until `spent`: whether every one is.
  fn charge_paths(&self, reserving: &mut Reserving, spent: &mut dyn FnMut() -> bool) -> bool {
    while let Some((path, asked)) = reserving.asked.pop_first() {
      reserving.total = reserving
        .total
        .saturating_add(self.path_charge(&path, asked));
      if !reserving.asked.is_empty() && spent() {
        return false;
      }
    }
    true
  }

  /// What the two reservations charge for `path`, asked `asked` of: its history's, where an op edits or replaces it
  /// ([`Green::superseded_bytes`], a special's node where it is replaced); its change index's, where an op names it,
  /// and the hard links naming it where it is a special.
  fn path_charge(&self, path: &str, asked: Asked) -> usize {
    let mut charge = 0usize;
    if asked.edits > 0 || asked.replaced {
      charge = self.superseded_bytes(path, asked.edits);
    }
    if asked.replaced && self.specials.contains_key(path) {
      charge = charge.saturating_add(SPECIAL_NODE_BYTES);
    }
    if !asked.indexed {
      return charge;
    }
    let links = self
      .was_special(path)
      .then(|| self.links_to.get(path))
      .flatten()
      .into_iter()
      .flatten();
    links
      .map(|link| change_bytes(link).saturating_mul(2))
      .fold(charge, usize::saturating_add)
      .saturating_add(change_bytes(path).saturating_mul(2))
  }

  /// The copies' and replaces' shares of both reservations of `inc`, in this green's numbering, added to `walked`, what
  /// [`Green::reserving_step`] reserved of it: the whole reservation. Whole: nothing for a work with neither, nothing
  /// for an increment already accepted.
  pub fn reservation_shares(&self, inc: &Increment, walked: usize) -> usize {
    if self.accepted.contains_key(&inc.id) {
      return 0;
    }
    walked
      .saturating_add(self.replace_history_reservation(inc))
      .saturating_add(Green::copy_records_reservation(inc))
      .saturating_add(self.replace_changes_reservation(inc))
      .saturating_add(self.copy_entry_room(inc))
  }
}
