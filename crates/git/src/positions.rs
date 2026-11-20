//! A set of object positions (A-614; §4.17.2 "The repository in git's terms", item 3: "kept per green as a compressed
//! bitmap over the repository's object positions"). Positions are given in arrival order, so the objects one push brings
//! new to the repository are one run, and a green's reached set is a few runs however many objects it holds. A set is
//! kept in whichever of three forms costs least, as Roaring keeps each container [A: Lemire, Ssi-Yan-Kai and Kaser,
//! "Consistently faster and smaller compressed bitmaps with Roaring", Software: Practice and Experience 46, 2016]: runs
//! `[start, end)`, 16 bytes a run; a sorted list, 8 bytes a position; or a bit for each position across the set's span,
//! 8 bytes a 64 of them. So a set never costs more than 8 bytes a position, the per-object handle it replaced (A-745 item
//! 4: a shallow clone's plan of a tip's tree, its objects brought by many pushes, had cost a run, 16 bytes, an object),
//! and one push's objects stay one run. A position is below `u64::MAX`, which the repository never gives.
//!
//! The form is chosen when a set is made whole (`of`, `from_runs`, a union, an intersection, a difference): each costs
//! one pass over the runs it holds. An insert keeps the set's form (a walk inserts an object at a time and is never made
//! to convert), but for a bit table it would grow past what the list form costs, where it becomes the list. Two sets are
//! equal when they hold the same positions, whatever their forms; a set is encoded as its runs (`run_pairs`), so what
//! is kept as content is the same whatever form a set was in.
//!
//! It is the crate's, not the store's: the negotiation's answer and the pack's plan carry the objects they send as
//! positions too (A-745), and the negotiation names no store.

use std::borrow::Cow;

/// A set of positions in its cheapest form (module docs).
#[derive(Clone, Debug, Default)]
pub struct Positions {
  form: Form,
}

/// The three forms a set is kept in (module docs).
#[derive(Clone, Debug)]
enum Form {
  /// Runs `[start, end)`, ascending, never touching or overlapping.
  Runs(Vec<(u64, u64)>),
  /// Positions, ascending, each once.
  List(Vec<u64>),
  /// A bit for each position from `start` (a word's first position), a word for each 64.
  Bits { start: u64, words: Vec<u64> },
}

impl Default for Form {
  fn default() -> Form {
    Form::Runs(Vec::new())
  }
}

/// Format: the positions one word of a bit table holds.
const WORD_BITS: u64 = u64::BITS as u64;
/// Format: the bytes a run costs: its two positions.
const RUN_BYTES: u64 = 16;
/// Format: the bytes a listed position, or a word of a bit table, costs.
const WORD_BYTES: u64 = 8;

impl PartialEq for Positions {
  fn eq(&self, other: &Positions) -> bool {
    match (&self.form, &other.form) {
      (Form::Runs(mine), Form::Runs(theirs)) => mine == theirs,
      (Form::List(mine), Form::List(theirs)) => mine == theirs,
      _ => self.iter().eq(other.iter()),
    }
  }
}

impl Eq for Positions {}

impl Positions {
  /// The empty set.
  pub fn new() -> Positions {
    Positions::default()
  }

  /// The set of `positions`, given in any order, repeats allowed.
  pub fn of(mut positions: Vec<u64>) -> Positions {
    positions.sort_unstable();
    positions.dedup();
    let mut set = Positions {
      form: Form::List(positions),
    };
    set.settle();
    set
  }

  /// The positions from `start` up to `end`, one run, made without listing them (A-665: the view
  /// starts from every position the repository gave); empty when `end` is not past `start`.
  pub fn span(start: u64, end: u64) -> Positions {
    if end <= start {
      return Positions::new();
    }
    Positions {
      form: Form::Runs(vec![(start, end)]),
    }
  }

  /// The set of runs `runs`, ascending and apart, in its cheapest form.
  fn of_runs(runs: Vec<(u64, u64)>) -> Positions {
    let mut set = Positions {
      form: Form::Runs(runs),
    };
    set.settle();
    set
  }

  /// A bit table's set (the negotiation's walk keeps its own, A-745): `words` from `start`, a word's first position, in
  /// its cheapest form.
  pub(crate) fn of_bits(start: u64, words: Vec<u64>) -> Positions {
    let mut set = Positions {
      form: Form::Bits {
        start: start.saturating_sub(start % WORD_BITS),
        words,
      },
    };
    set.settle();
    set
  }

  /// The bytes the set costs in its form.
  pub fn bytes(&self) -> u64 {
    match &self.form {
      Form::Runs(runs) => RUN_BYTES.saturating_mul(crate::to_u64(runs.len())),
      Form::List(list) => WORD_BYTES.saturating_mul(crate::to_u64(list.len())),
      Form::Bits { words, .. } => WORD_BYTES.saturating_mul(crate::to_u64(words.len())),
    }
  }

  /// Puts the set in its cheapest form (module docs): the runs, the list or the bit table, whichever costs fewest bytes,
  /// runs before the list and the list before the table where two tie.
  fn settle(&mut self) {
    let (mut runs, mut count) = (0_u64, 0_u64);
    let (mut first, mut last) = (None, None);
    for (start, end) in self.runs_iter() {
      runs = runs.saturating_add(1);
      count = count.saturating_add(end.saturating_sub(start));
      first.get_or_insert(start);
      last = Some(end.saturating_sub(1));
    }
    let (Some(first), Some(last)) = (first, last) else {
      self.form = Form::Runs(Vec::new());
      return;
    };
    let span_words = (last / WORD_BITS)
      .saturating_sub(first / WORD_BITS)
      .saturating_add(1);
    let costs = [
      RUN_BYTES.saturating_mul(runs),
      WORD_BYTES.saturating_mul(count),
      WORD_BYTES.saturating_mul(span_words),
    ];
    let cheapest = (0..costs.len())
      .min_by_key(|at| costs.get(*at).copied().unwrap_or(u64::MAX))
      .unwrap_or(0);
    let form = match (cheapest, &self.form) {
      (0, Form::Runs(_)) | (1, Form::List(_)) | (2, Form::Bits { .. }) => return,
      (0, _) => Form::Runs(self.runs_iter().collect()),
      (1, _) => Form::List(self.iter().collect()),
      _ => {
        let start = first.saturating_sub(first % WORD_BITS);
        let mut words = vec![0_u64; usize::try_from(span_words).unwrap_or(0)];
        for position in self.iter() {
          let offset = position.saturating_sub(start);
          if let Some(word) = usize::try_from(offset / WORD_BITS)
            .ok()
            .and_then(|at| words.get_mut(at))
          {
            *word |= 1u64 << (offset % WORD_BITS);
          }
        }
        Form::Bits { start, words }
      }
    };
    self.form = form;
  }

  /// Whether the set holds `position`.
  pub fn contains(&self, position: u64) -> bool {
    match &self.form {
      Form::Runs(runs) => {
        let after = runs.partition_point(|run| run.0 <= position);
        after
          .checked_sub(1)
          .and_then(|at| runs.get(at))
          .is_some_and(|run| position < run.1)
      }
      Form::List(list) => list.binary_search(&position).is_ok(),
      Form::Bits { start, words } => bit(*start, words, position),
    }
  }

  /// The union of this set and `other`: the two sets' runs merged in one pass over both.
  pub fn union(&self, other: &Positions) -> Positions {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let (mut mine, mut theirs) = (self.runs_iter().peekable(), other.runs_iter().peekable());
    loop {
      let next = match (mine.peek(), theirs.peek()) {
        (Some(left), Some(right)) if left.0 <= right.0 => mine.next(),
        (Some(_), Some(_)) | (None, Some(_)) => theirs.next(),
        (Some(_), None) => mine.next(),
        (None, None) => break,
      };
      let Some((start, end)) = next else { break };
      match runs.last_mut() {
        Some(last) if start <= last.1 => last.1 = last.1.max(end),
        _ => runs.push((start, end)),
      }
    }
    Positions::of_runs(runs)
  }

  /// The positions held.
  pub fn len(&self) -> u64 {
    match &self.form {
      Form::Runs(runs) => runs.iter().fold(0_u64, |count, run| {
        count.saturating_add(run.1.saturating_sub(run.0))
      }),
      Form::List(list) => crate::to_u64(list.len()),
      Form::Bits { words, .. } => words.iter().fold(0_u64, |count, word| {
        count.saturating_add(u64::from(word.count_ones()))
      }),
    }
  }

  /// Whether the set holds no position.
  pub fn is_empty(&self) -> bool {
    match &self.form {
      Form::Runs(runs) => runs.is_empty(),
      Form::List(list) => list.is_empty(),
      Form::Bits { words, .. } => words.iter().all(|word| *word == 0),
    }
  }

  /// The runs the set's positions make.
  pub fn runs(&self) -> usize {
    match &self.form {
      Form::Runs(runs) => runs.len(),
      _ => self.runs_iter().count(),
    }
  }

  /// The runs `[start, end)`, ascending: what a derived value encodes (A-647 part 2), the same whatever the set's form.
  pub(crate) fn run_pairs(&self) -> Cow<'_, [(u64, u64)]> {
    match &self.form {
      Form::Runs(runs) => Cow::Borrowed(runs),
      _ => Cow::Owned(self.runs_iter().collect()),
    }
  }

  /// Copies into this set the window of `source`'s kept units from unit `at`, `count` at most: its runs, listed
  /// positions or table words, whichever its form keeps, read from `at` on and none before (A-745 item 4: a reader takes
  /// a plan a window a turn, so a turn's work is its window, never the plan). This set is taken as empty when `at` is 0
  /// and as the windows before otherwise, so once the last window is copied it is equal to `source`, in its form. The
  /// unit after the window, or `None` once the last is copied; and the units read.
  pub(crate) fn copy_window(
    &mut self,
    source: &Positions,
    at: usize,
    count: usize,
  ) -> (Option<usize>, usize) {
    /// The units of `units` from `at`, `count` at most, and the unit after them unless they end the slice.
    fn window<T>(units: &[T], at: usize, count: usize) -> (&[T], Option<usize>) {
      let end = at.saturating_add(count).min(units.len());
      let taken = units.get(at..end).unwrap_or_default();
      (taken, (end < units.len()).then_some(end))
    }
    if at == 0 {
      self.form = match &source.form {
        Form::Runs(_) => Form::Runs(Vec::new()),
        Form::List(_) => Form::List(Vec::new()),
        Form::Bits { start, .. } => Form::Bits {
          start: *start,
          words: Vec::new(),
        },
      };
    }
    match (&source.form, &mut self.form) {
      (Form::Runs(theirs), Form::Runs(mine)) => {
        let (taken, next) = window(theirs, at, count);
        mine.extend_from_slice(taken);
        (next, taken.len())
      }
      (Form::List(theirs), Form::List(mine)) => {
        let (taken, next) = window(theirs, at, count);
        mine.extend_from_slice(taken);
        (next, taken.len())
      }
      (Form::Bits { words: theirs, .. }, Form::Bits { words: mine, .. }) => {
        let (taken, next) = window(theirs, at, count);
        mine.extend_from_slice(taken);
        (next, taken.len())
      }
      _ => (None, 0),
    }
  }

  /// The set kept as `runs`, or `None` unless each is non-empty and they ascend with a gap between each two: the one
  /// way a set is encoded, so a decoded set is equal to the set encoded and to no other.
  pub(crate) fn from_runs(runs: Vec<(u64, u64)>) -> Option<Positions> {
    let each = runs.iter().all(|run| run.0 < run.1);
    let apart = runs
      .windows(2)
      .all(|pair| matches!(pair, [left, right] if left.1 < right.0));
    (each && apart).then(|| Positions::of_runs(runs))
  }

  /// Every position, ascending.
  pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
    let runs = match &self.form {
      Form::Runs(runs) => Some(runs.iter().flat_map(|&(start, end)| start..end)),
      _ => None,
    };
    let list = match &self.form {
      Form::List(list) => Some(list.iter().copied()),
      _ => None,
    };
    let bits = match &self.form {
      Form::Bits { start, words } => Some(bits_of(*start, words)),
      _ => None,
    };
    runs
      .into_iter()
      .flatten()
      .chain(list.into_iter().flatten())
      .chain(bits.into_iter().flatten())
  }

  /// The set's runs, ascending, whatever its form.
  fn runs_iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
    let kept = match &self.form {
      Form::Runs(runs) => Some(runs.iter().copied()),
      _ => None,
    };
    let positions = match &self.form {
      Form::Runs(_) => None,
      _ => Some(self.iter()),
    };
    let mut positions = positions.into_iter().flatten().peekable();
    let made = core::iter::from_fn(move || {
      let start = positions.next()?;
      let mut end = start.saturating_add(1);
      while positions.next_if_eq(&end).is_some() {
        end = end.saturating_add(1);
      }
      Some((start, end))
    });
    kept.into_iter().flatten().chain(made)
  }

  /// The positions both this set and `other` hold (A-639): each run of one met with the runs of the other it overlaps,
  /// in one pass over both. Two runs out never touch, since a gap of either set lies between them.
  pub fn intersection(&self, other: &Positions) -> Positions {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let (mut mine, mut theirs) = (self.runs_iter().peekable(), other.runs_iter().peekable());
    while let (Some(&(start, end)), Some(&(from, to))) = (mine.peek(), theirs.peek()) {
      let (low, high) = (start.max(from), end.min(to));
      if low < high {
        runs.push((low, high));
      }
      if end <= to {
        mine.next();
      } else {
        theirs.next();
      }
    }
    Positions::of_runs(runs)
  }

  /// The positions this set holds and `other` does not (A-639): each run cut by the runs of `other` it overlaps, in one
  /// pass over both.
  pub fn difference(&self, other: &Positions) -> Positions {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut cuts = other.runs_iter().peekable();
    for (start, end) in self.runs_iter() {
      while cuts.next_if(|cut| cut.1 <= start).is_some() {}
      let mut from = start;
      while let Some(&(cut_start, cut_end)) = cuts.peek() {
        if cut_start >= end {
          break;
        }
        if cut_start > from {
          runs.push((from, cut_start));
        }
        from = from.max(cut_end);
        if cut_end > end {
          break;
        }
        cuts.next();
      }
      if from < end {
        runs.push((from, end));
      }
    }
    Positions::of_runs(runs)
  }

  /// Whether `other` holds every position this set holds (A-639).
  pub fn is_subset(&self, other: &Positions) -> bool {
    self.difference(other).is_empty()
  }

  /// Adds `position` (A-639): in constant time at the end, as positions given in order arrive; anywhere else in place
  /// ([`Positions::insert_new`]).
  pub fn insert(&mut self, position: u64) {
    self.insert_new(position);
  }

  /// Adds `position` in place, wherever it falls, in the set's form (A-745: a walk meets positions in any order): whether
  /// the set did not hold it. A bit table that would grow past what the set's list costs becomes the list first.
  pub fn insert_new(&mut self, position: u64) -> bool {
    match &mut self.form {
      Form::Runs(runs) => insert_run(runs, position),
      Form::List(list) => match list.binary_search(&position) {
        Ok(_) => false,
        Err(at) => {
          list.insert(at, position);
          true
        }
      },
      Form::Bits { start, words } => {
        if bit(*start, words, position) {
          return false;
        }
        let low = (*start).min(position.saturating_sub(position % WORD_BITS));
        let high = start
          .saturating_add(crate::to_u64(words.len()).saturating_mul(WORD_BITS))
          .max(position.saturating_add(1));
        let grown = high.saturating_sub(low).div_ceil(WORD_BITS);
        let count = self.len();
        if WORD_BYTES.saturating_mul(grown) > WORD_BYTES.saturating_mul(count.saturating_add(1)) {
          let mut list: Vec<u64> = self.iter().collect();
          let at = list.partition_point(|held| *held < position);
          list.insert(at, position);
          self.form = Form::List(list);
          return true;
        }
        if let Form::Bits { start, words } = &mut self.form {
          set_bit(start, words, position);
        }
        true
      }
    }
  }

  /// The least position the set holds at `from` or after it (A-639).
  pub fn next_from(&self, from: u64) -> Option<u64> {
    match &self.form {
      Form::Runs(runs) => {
        let at = runs.partition_point(|run| run.1 <= from);
        runs.get(at).map(|run| run.0.max(from))
      }
      Form::List(list) => list.get(list.partition_point(|held| *held < from)).copied(),
      Form::Bits { start, words } => bits_of(*start, words).find(|position| *position >= from),
    }
  }
}

/// Adds `position` to the runs `runs` in place: whether they did not hold it. Halving finds the run it falls in or
/// beside; it extends that run or the next, joins the two, or begins a run of its own, moving no more than the runs
/// after it.
fn insert_run(runs: &mut Vec<(u64, u64)>, position: u64) -> bool {
  let end = position.saturating_add(1);
  let after = runs.partition_point(|run| run.0 <= position);
  let before = after.checked_sub(1);
  let previous = before.and_then(|at| runs.get(at)).copied();
  if previous.is_some_and(|run| position < run.1) {
    return false;
  }
  let joins_previous = previous.is_some_and(|run| run.1 == position);
  let next = runs.get(after).copied();
  let joins_next = next.is_some_and(|run| run.0 == end);
  match (joins_previous, joins_next, before) {
    (true, true, Some(at)) => {
      let next_end = next.map_or(end, |run| run.1);
      if let Some(run) = runs.get_mut(at) {
        run.1 = next_end;
      }
      runs.remove(after);
    }
    (true, false, Some(at)) => {
      if let Some(run) = runs.get_mut(at) {
        run.1 = end;
      }
    }
    (false, true, _) => {
      if let Some(run) = runs.get_mut(after) {
        run.0 = position;
      }
    }
    _ => runs.insert(after, (position, end)),
  }
  true
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
/// first position).
fn set_bit(start: &mut u64, words: &mut Vec<u64>, position: u64) {
  let word_start = position.saturating_sub(position % WORD_BITS);
  if word_start < *start {
    let more = usize::try_from(start.saturating_sub(word_start) / WORD_BITS).unwrap_or(0);
    words.splice(0..0, core::iter::repeat_n(0, more));
    *start = word_start;
  }
  let offset = position.saturating_sub(*start);
  let Ok(at) = usize::try_from(offset / WORD_BITS) else {
    return;
  };
  if words.len() <= at {
    words.resize(at.saturating_add(1), 0);
  }
  if let Some(word) = words.get_mut(at) {
    *word |= 1u64 << (offset % WORD_BITS);
  }
}

/// The positions the bit table of `words` from `start` holds, ascending.
fn bits_of(start: u64, words: &[u64]) -> impl Iterator<Item = u64> + '_ {
  words.iter().enumerate().flat_map(move |(at, &word)| {
    let base = start.saturating_add(crate::to_u64(at).saturating_mul(WORD_BITS));
    let mut left = word;
    core::iter::from_fn(move || {
      if left == 0 {
        return None;
      }
      let low = u64::from(left.trailing_zeros());
      left &= left.wrapping_sub(1);
      Some(base.saturating_add(low))
    })
  })
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;

  use proptest::prelude::*;

  use super::Positions;

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-745 (a walk inserts positions in any order). Do: insert random positions, repeats among them, in the order
    /// drawn, into a set and into an ordered set beside it. Expect: after each insert, the same answer to "was it new",
    /// the same members, and runs kept in their one form (each non-empty, ascending, a gap between each two), so the
    /// set is equal to the set its members make.
    #[test]
    fn a745_positions_inserted_in_any_order_are_the_set_they_make(drawn in proptest::collection::vec(0u64..200, 0..300)) {
      let mut set = Positions::new();
      let mut model = BTreeSet::new();
      for position in drawn {
        prop_assert_eq!(set.insert_new(position), model.insert(position));
        prop_assert!(Positions::from_runs(set.run_pairs().to_vec()).is_some(), "{:?}", set);
      }
      prop_assert_eq!(set.iter().collect::<Vec<_>>(), model.iter().copied().collect::<Vec<_>>());
      prop_assert_eq!(set, Positions::of(model.into_iter().collect()));
    }
  }

  /// The set `drawn` makes, built by inserts in the order drawn (a walk's way) or whole (`of`).
  fn built(drawn: &[u64], by_insert: bool) -> Positions {
    if !by_insert {
      return Positions::of(drawn.to_vec());
    }
    let mut set = Positions::new();
    for position in drawn {
      set.insert(*position);
    }
    set
  }

  /// A set's cost against the bound it is kept to: a set made whole costs no more than 8 bytes a position (the
  /// per-object handle it replaced) and no more than 16 a run.
  fn within_bound(set: &Positions) -> bool {
    set.bytes() <= 8 * set.len().max(1) && set.bytes() <= 16 * crate::to_u64(set.runs()).max(1)
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-745 item 4 (a set in its cheapest form). Do: draw two sets of positions, dense and scattered alike (gaps of
    /// one to a few hundred, so all three forms are met), each made by inserts in the order drawn or whole; take their
    /// union, intersection and difference, their encoded runs read back, and each position's membership and next
    /// position. Expect: everything the ordered sets beside them answer; equality whatever the forms; and every set made
    /// whole within its bound: no more than 8 bytes a position, nor 16 a run.
    #[test]
    fn a745_positions_in_any_form_are_the_set_they_make_within_8_bytes_a_position(
      left in proptest::collection::vec((0u64..4, 1u64..400), 0..200),
      right in proptest::collection::vec((0u64..4, 1u64..400), 0..200),
      left_inserted in any::<bool>(),
      right_inserted in any::<bool>(),
    ) {
      let walk = |steps: &[(u64, u64)]| {
        let mut at = 0u64;
        steps.iter().map(|(dense, gap)| { at += if *dense == 0 { 1 } else { *gap }; at }).collect::<Vec<u64>>()
      };
      let (left, right) = (walk(&left), walk(&right));
      let (mine, theirs) = (built(&left, left_inserted), built(&right, right_inserted));
      let (model_mine, model_theirs): (BTreeSet<u64>, BTreeSet<u64>) =
        (left.iter().copied().collect(), right.iter().copied().collect());
      let listed = |set: &Positions| set.iter().collect::<Vec<u64>>();
      prop_assert_eq!(listed(&mine), model_mine.iter().copied().collect::<Vec<_>>());
      prop_assert_eq!(&mine, &Positions::of(left.clone()));
      let union = mine.union(&theirs);
      let intersection = mine.intersection(&theirs);
      let difference = mine.difference(&theirs);
      prop_assert_eq!(listed(&union), model_mine.union(&model_theirs).copied().collect::<Vec<_>>());
      prop_assert_eq!(
        listed(&intersection),
        model_mine.intersection(&model_theirs).copied().collect::<Vec<_>>()
      );
      prop_assert_eq!(
        listed(&difference),
        model_mine.difference(&model_theirs).copied().collect::<Vec<_>>()
      );
      prop_assert_eq!(mine.is_subset(&theirs), model_mine.is_subset(&model_theirs));
      prop_assert_eq!(mine.len(), crate::to_u64(model_mine.len()));
      for set in [&union, &intersection, &difference, &Positions::of(left.clone())] {
        prop_assert!(within_bound(set), "{:?}: {} bytes", set, set.bytes());
        let read_back = Positions::from_runs(set.run_pairs().into_owned());
        prop_assert_eq!(read_back.as_ref(), Some(set));
      }
      for probe in 0..left.last().copied().unwrap_or(0).saturating_add(2) {
        prop_assert_eq!(mine.contains(probe), model_mine.contains(&probe));
        prop_assert_eq!(mine.next_from(probe), model_mine.range(probe..).next().copied());
      }
    }

    /// A-745 item 4 (a reader takes a plan a window a turn). Do: draw a set, dense or scattered (all three forms met),
    /// made whole or by inserts, and copy it a window of a drawn size at a time from unit 0, each window from where the
    /// last ended. Expect: each window reads at most its size; the copy, once the last window is read, equal to the set.
    #[test]
    fn a745_a_set_copied_a_window_at_a_time_is_the_set(
      steps in proptest::collection::vec((0u64..4, 1u64..400), 0..200),
      inserted in any::<bool>(),
      count in 1usize..40,
    ) {
      let mut at = 0u64;
      let positions: Vec<u64> = steps
        .iter()
        .flat_map(|&(run, gap)| {
          at += gap;
          let from = at;
          at += run;
          from..=from + run
        })
        .collect();
      let set = built(&positions, inserted);
      let mut copy = Positions::new();
      let mut next = Some(0);
      while let Some(from) = next {
        let (after, read) = copy.copy_window(&set, from, count);
        prop_assert!(read <= count);
        next = after;
      }
      prop_assert_eq!(copy, set);
    }
  }

  /// A-745 item 4 (found by use: a shallow clone's plan of a tip's tree, objects many pushes brought, had cost a run an
  /// object). Do: make the set of 3,000 positions every third one apart, as a push's commits, trees and blobs interleave,
  /// whole and by inserts. Expect: each costs at most 8 bytes a position, here its bit table's 1,128 bytes, where its
  /// 3,000 runs cost 48,000.
  #[test]
  fn a745_a_scattered_plan_costs_a_bit_table_not_a_run_a_position() {
    let scattered: Vec<u64> = (0..3_000u64).map(|at| at * 3).collect();
    let whole = Positions::of(scattered.clone());
    assert_eq!(whole.runs(), 3_000);
    assert_eq!(whole.bytes(), 1_128);
    assert!(within_bound(&whole));
    assert_eq!(whole, built(&scattered, true));
  }
}
