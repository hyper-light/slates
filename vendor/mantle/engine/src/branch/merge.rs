//! Merging branches (docs/design/engine-structure.md §4, step E4): their entries in key order,
//! the newest branch's entry for each key and no other, as SplinterDB's merge iterator gives them
//! to a compaction (research/34 §1).

use super::filter::Keys;
use super::{Branch, Builder, Initial, Op, RunCursor};
use crate::error::Error;
use crate::store::Store;
use crate::util::keys;
use hyper_block::block::BlockFile;

/// A merge over cursors given newest first, from a start key up to an end key (exclusive, none
/// for the end of the branches).
#[derive(Debug)]
pub struct Merge {
    cursors: Vec<RunCursor>,
    end: Option<Vec<u8>>,
    /// The cursor whose entry is current, if any.
    current: Option<usize>,
    /// The cursor at the least key of the others, as [`Self::pick`] left it: while the current
    /// key stays below its key, no other cursor holds the current key, so a step moves the
    /// current cursor alone and compares its next key with this one only.
    second: Option<usize>,
    /// The key `next` moves past, its buffer reused so a step allocates nothing.
    past: Vec<u8>,
    /// While opening, `current` is the next input and `past` holds its from-key.
    opening: bool,
}

impl Merge {
    /// A merge of `branches`, newest first, over `[from, end)`.
    pub fn new<'a, F: BlockFile>(
        store: &mut Store<F>,
        branches: impl IntoIterator<Item = &'a Branch>,
        from: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Self, Error> {
        Self::with(store, branches, from, end, false)
    }

    /// [`Self::new`] for a compaction, which reads its inputs to their end: an extent a read
    /// from the first.
    pub fn sequential<'a, F: BlockFile>(
        store: &mut Store<F>,
        branches: impl IntoIterator<Item = &'a Branch>,
        from: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Self, Error> {
        Self::with(store, branches, from, end, true)
    }

    fn with<'a, F: BlockFile>(
        store: &mut Store<F>,
        branches: impl IntoIterator<Item = &'a Branch>,
        from: &[u8],
        end: Option<&[u8]>,
        sequential: bool,
    ) -> Result<Self, Error> {
        let mut cursors = Vec::new();
        for b in branches {
            let cursor = if sequential {
                b.seek_sequential(store, from)
            } else {
                b.seek(store, from)
            };
            match cursor {
                Ok(cursor) => cursors.push(cursor),
                Err(error) => {
                    for cursor in cursors {
                        cursor.give_back(store);
                    }
                    return Err(error);
                }
            }
        }
        let mut merge = Self {
            cursors,
            end: end.map(<[u8]>::to_vec),
            current: None,
            second: None,
            past: Vec::new(),
            opening: false,
        };
        merge.pick();
        Ok(merge)
    }

    fn prepared(from: Vec<u8>, end: Option<Vec<u8>>) -> Self {
        Self {
            cursors: Vec::new(),
            end,
            current: Some(0),
            second: None,
            past: from,
            opening: true,
        }
    }

    /// Opens one input leaf. Partial cursors retain only their existing span and page owners.
    fn open<'a, F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        branches: impl IntoIterator<Item = &'a Branch>,
        count: usize,
        yield_io: bool,
    ) -> Result<bool, Error> {
        if !self.opening {
            return Ok(true);
        }
        let at = self.current.ok_or(Error::InvalidArgument {
            what: "a compaction opened over inconsistent inputs",
        })?;
        if count == 0 {
            self.opening = false;
            self.past.clear();
            self.pick();
            return Ok(true);
        }
        let branch = branches.into_iter().nth(at).ok_or(Error::InvalidArgument {
            what: "a compaction opened over inconsistent inputs",
        })?;
        if self.cursors.len() == at {
            self.cursors
                .push(RunCursor::prepare(branch, store, &self.past)?);
        }
        let cursor = self.cursors.get_mut(at).ok_or(Error::InvalidArgument {
            what: "a compaction opened over inconsistent inputs",
        })?;
        match cursor.begin(branch, store, &self.past, yield_io)? {
            Initial::Waiting => return Ok(false),
            Initial::More => return Ok(true),
            Initial::Done => self.current = Some(at.saturating_add(1)),
        }
        if self.current == Some(count) {
            self.opening = false;
            self.past.clear();
            self.pick();
        }
        Ok(true)
    }

    /// An opening failure returns every partial cursor, including unclaimed read buffers.
    fn cancel_opening<F: BlockFile>(&mut self, store: &mut Store<F>) {
        for cursor in self.cursors.drain(..) {
            cursor.give_back(store);
        }
        self.current = Some(0);
        self.opening = true;
    }

    /// Gives every cursor's span back to `store`'s pool: the merge is done with them.
    pub fn give_back<F: BlockFile>(self, store: &mut Store<F>) {
        for c in self.cursors {
            c.give_back(store);
        }
    }

    /// The cursor at the smallest key, the newest of those tied; none past the end. Notes the
    /// least of the others too ([`Self::second`]).
    fn pick(&mut self) {
        // The least and second least cursors held as they are found, each with its place.
        let mut best: Option<(usize, &RunCursor)> = None;
        let mut second: Option<(usize, &RunCursor)> = None;
        for (i, c) in self.cursors.iter().enumerate() {
            if !c.valid() {
                continue;
            }
            match best {
                None => best = Some((i, c)),
                // Strictly smaller only: an equal key keeps the earlier, newer cursor.
                Some((_, b)) if keys::less(c.key(), b.key()) => {
                    second = best;
                    best = Some((i, c));
                }
                Some(_) => {
                    if second.is_none_or(|(_, s)| keys::less(c.key(), s.key())) {
                        second = Some((i, c));
                    }
                }
            }
        }
        self.second = second.map(|(i, _)| i);
        self.current = best
            .filter(|(_, c)| {
                self.end
                    .as_deref()
                    .is_none_or(|end| keys::less(c.key(), end))
            })
            .map(|(i, _)| i);
    }

    /// Whether the current key is below every other cursor's: no other cursor holds it.
    fn alone(&self, at: &RunCursor) -> bool {
        self.second
            .and_then(|s| self.cursors.get(s))
            .is_none_or(|s| !s.valid() || keys::less(at.key(), s.key()))
    }

    /// Whether moving past the current key reads no page that has not landed: every cursor
    /// holding it either stays in its leaf or has its next leaf ready ([`RunCursor::next_ready`]).
    pub fn ready<'a, F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        branches: impl IntoIterator<Item = &'a Branch>,
    ) -> Result<bool, Error> {
        if self.opening {
            return Ok(true);
        }
        let Some(i) = self.current else {
            return Ok(true);
        };
        let Some(at) = self.cursors.get(i) else {
            return Ok(true);
        };
        if self.alone(at) {
            let b = branches.into_iter().nth(i).ok_or(Error::InvalidArgument {
                what: "a merge stepped over inputs it was not opened on",
            })?;
            return match self.cursors.get_mut(i) {
                Some(c) => c.next_ready(b, store),
                None => Ok(true),
            };
        }
        self.past.clear();
        self.past.extend_from_slice(at.key());
        for (c, b) in self.cursors.iter_mut().zip(branches) {
            if c.valid() && keys::equal(c.key(), &self.past) && !c.next_ready(b, store)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The current entry: key, operation, value.
    pub fn entry(&self) -> Option<(&[u8], Op, &[u8])> {
        if self.opening {
            return None;
        }
        self.current
            .and_then(|i| self.cursors.get(i))
            .map(|c| (c.key(), c.op(), c.value()))
    }

    /// Moves past the current key in every cursor that holds it; the cursors moved, each an
    /// input entry consumed. `branches` are the merge's, in the order it was opened on: a
    /// cursor moving to its next leaf reads it by its branch's page counts.
    pub fn next<'a, F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        branches: impl IntoIterator<Item = &'a Branch>,
    ) -> Result<u64, Error> {
        if self.opening {
            return Ok(0);
        }
        let Some(i) = self.current else {
            return Ok(0);
        };
        let Some(at) = self.cursors.get(i) else {
            return Ok(0);
        };
        if self.alone(at) {
            // Only this cursor holds the key: it moves alone, and stays current while its next
            // key is below the others' and the end.
            let b = branches.into_iter().nth(i).ok_or(Error::InvalidArgument {
                what: "a merge stepped over inputs it was not opened on",
            })?;
            let c = self.cursors.get_mut(i).ok_or(Error::InvalidArgument {
                what: "a merge stepped over inputs it was not opened on",
            })?;
            c.next(b, store)?;
            let stays = self.cursors.get(i).is_some_and(|c| {
                c.valid()
                    && self.alone(c)
                    && self
                        .end
                        .as_deref()
                        .is_none_or(|end| keys::less(c.key(), end))
            });
            if !stays {
                self.pick();
            }
            return Ok(1);
        }
        self.past.clear();
        self.past.extend_from_slice(at.key());
        let mut moved = 0u64;
        for (c, b) in self.cursors.iter_mut().zip(branches) {
            if c.valid() && keys::equal(c.key(), &self.past) {
                c.next(b, store)?;
                moved = moved.saturating_add(1);
            }
        }
        self.pick();
        Ok(moved)
    }
}

/// A compaction run a slice at a time ([`Compaction::step`]): `branches` (newest first) merged
/// over `[from, end)` into branches of at most `per` entries each, the newest entry of each key,
/// tombstones dropped when `drop_tombstones` (nothing older lies below). Its inputs are not
/// changed while it runs, so it is resumed between any other reads of the store.
#[derive(Debug)]
pub struct Compaction {
    merge: Merge,
    /// The inputs' roots, newest first: each step is given the inputs again and checks them.
    roots: Vec<u64>,
    drop_tombstones: bool,
    per: u64,
    /// The branch being built: its first key, its builder, its entries.
    building: Option<(Vec<u8>, Builder, u64)>,
    /// Branches sealed whose filter pages are still to write, oldest first: at most the parts
    /// the compaction makes.
    closing: std::collections::VecDeque<(Vec<u8>, Builder)>,
    out: Vec<(Vec<u8>, Branch)>,
    /// Input entries not yet merged, at most: the inputs' counts less those consumed (a range
    /// narrower than a branch leaves some never reached).
    remaining: u64,
    /// Whether a step stops when an input's next page has not landed ([`Self::set_yield`]),
    /// and whether the last step did.
    yield_io: bool,
    waiting: bool,
    /// With no end to the range, what the inputs' greatest range filter entries share: each is a
    /// prefix of its input's last key, so every key from the first an output takes up to any
    /// input's last shares with that first at least what it shares with this.
    ceiling: Vec<u8>,
}

/// Folds `branch` into `ceiling`, what the greatest range filter entries of the inputs share,
/// `scratch` holding each in turn.
fn fold_ceiling(ceiling: &mut Option<Vec<u8>>, branch: &Branch, scratch: &mut Vec<u8>) {
    if !branch.range.greatest(scratch) {
        return;
    }
    match ceiling.as_mut() {
        Some(c) => c.truncate(crate::util::keys::common_prefix(c, scratch)),
        None => *ceiling = Some(scratch.clone()),
    }
}

impl Compaction {
    /// A compaction of `branches`, newest first, over `[from, end)`. The branches are taken by
    /// reference: a branch's descriptor carries its filter, which a compaction never reads, and
    /// planning cloned the descriptors, megabytes of filters a plan.
    pub fn new<'a, F: BlockFile>(
        store: &mut Store<F>,
        branches: impl IntoIterator<Item = &'a Branch>,
        from: &[u8],
        end: Option<&[u8]>,
        drop_tombstones: bool,
        per: u64,
    ) -> Result<Self, Error> {
        let mut remaining = 0u64;
        let mut roots = Vec::new();
        let (mut ceiling, mut scratch) = (None, Vec::new());
        let counted = branches.into_iter().inspect(|b| {
            remaining = remaining.saturating_add(b.count);
            roots.push(b.root);
            if end.is_none() {
                fold_ceiling(&mut ceiling, b, &mut scratch);
            }
        });
        let merge = Merge::sequential(store, counted, from, end)?;
        let ceiling = ceiling.unwrap_or_default();
        Ok(Self {
            merge,
            roots,
            drop_tombstones,
            per: per.max(1),
            building: None,
            closing: std::collections::VecDeque::new(),
            out: Vec::new(),
            remaining,
            yield_io: false,
            waiting: false,
            ceiling,
        })
    }

    /// The trunk's private plan: retain the existing from-key buffer and open input leaves in
    /// budgeted steps, after yielding has been selected. Public constructors remain immediate.
    pub(crate) fn prepare<'a>(
        branches: impl IntoIterator<Item = &'a Branch>,
        from: Vec<u8>,
        end: Option<Vec<u8>>,
        drop_tombstones: bool,
        per: u64,
    ) -> Self {
        let mut remaining = 0u64;
        let mut roots = Vec::new();
        let (mut ceiling, mut scratch) = (None, Vec::new());
        for branch in branches {
            remaining = remaining.saturating_add(branch.count);
            roots.push(branch.root);
            if end.is_none() {
                fold_ceiling(&mut ceiling, branch, &mut scratch);
            }
        }
        let ceiling = ceiling.unwrap_or_default();
        let mut merge = Merge::prepared(from, end);
        if roots.is_empty() {
            merge.opening = false;
            merge.current = None;
            merge.past.clear();
        }
        Self {
            merge,
            roots,
            drop_tombstones,
            per: per.max(1),
            building: None,
            closing: std::collections::VecDeque::new(),
            out: Vec::new(),
            remaining,
            yield_io: false,
            waiting: false,
            ceiling,
        }
    }

    /// Whether a step stops, rather than wait, when an initial or next input page has not landed from
    /// the device ([`Store::ready`]): for a step a put paces, which the device's latency must
    /// not hold. Steps of idle time and of a stall wait, and always finish their budget.
    pub fn set_yield(&mut self, on: bool) {
        self.yield_io = on;
    }

    /// Only an unfinished private opening can restart after its owners have been returned.
    pub(crate) fn opening(&self) -> bool {
        self.merge.opening
    }

    /// Whether it holds any input open: a cursor's span and pages, kept until it finishes.
    pub(crate) fn holds_inputs(&self) -> bool {
        !self.merge.cursors.is_empty()
    }

    /// Whether the last step stopped for a page still being read.
    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// The work estimate: input entries left, one unit per input still opening, and filter
    /// pages at their worth in keys. A gap may take another leaf move; its opening stays owed
    /// until the input is positioned.
    pub fn remaining(&self) -> u64 {
        let opening = if self.merge.opening {
            self.merge
                .current
                .map_or(self.roots.len(), |at| self.roots.len().saturating_sub(at))
        } else {
            0
        };
        let remaining = self
            .remaining
            .saturating_add(u64::try_from(opening).unwrap_or(u64::MAX));
        // The filter pages left to write, each at its worth in keys.
        let filters = |b: &Builder| b.filter_pages_left().saturating_mul(b.page_keys());
        self.closing
            .iter()
            .map(|(_, b)| filters(b))
            .chain(self.building.iter().map(|(_, b, _)| filters(b)))
            .fold(remaining, u64::saturating_add)
    }

    /// Works for up to `budget`: a private plan opens one input leaf per unit first, yielding
    /// before an unlanded read; then the filter pages of branches already sealed, each at its
    /// worth in keys ([`Builder::page_keys`]), then keys merged; a part that reaches its size, or
    /// the last once the merge is done, is sealed and its filter's pages wait their turn. Returns
    /// the budget used, less than `budget` only once the compaction is done or, when it yields
    /// ([`Self::set_yield`]), stopped for a page still being read ([`Self::waiting`]).
    pub fn step<'a, F: BlockFile, I>(
        &mut self,
        store: &mut Store<F>,
        inputs: I,
        budget: u64,
    ) -> Result<u64, Error>
    where
        I: IntoIterator<Item = &'a Branch>,
    {
        // The inputs are the ones the compaction was opened on, in its order: walked once here
        // into a list of them, which the merge then indexes for each entry rather than walking
        // the caller's iterator (in-flight bundles flattened) to its cursor's branch.
        let mut list: Vec<&'a Branch> = Vec::with_capacity(self.roots.len());
        for b in inputs {
            if self.roots.get(list.len()) != Some(&b.root) {
                if self.merge.opening {
                    self.merge.cancel_opening(store);
                }
                return Err(Error::InvalidArgument {
                    what: "a compaction stepped over inputs it was not opened on",
                });
            }
            list.push(b);
        }
        let inputs = list.iter().copied();
        if list.len() != self.roots.len() {
            if self.merge.opening {
                self.merge.cancel_opening(store);
            }
            return Err(Error::InvalidArgument {
                what: "a compaction stepped over inputs it was not opened on",
            });
        }
        let mut done = 0u64;
        self.waiting = false;
        while done < budget {
            if self.merge.opening {
                match self
                    .merge
                    .open(store, inputs.clone(), self.roots.len(), self.yield_io)
                {
                    Ok(true) => {
                        done = done.saturating_add(1);
                        continue;
                    }
                    Ok(false) => {
                        self.waiting = true;
                        break;
                    }
                    Err(error) => {
                        self.merge.cancel_opening(store);
                        return Err(error);
                    }
                }
            }
            if let Some((_, b)) = self.closing.front_mut() {
                let keys = b.page_keys();
                let pages = budget
                    .saturating_sub(done)
                    .checked_div(keys)
                    .unwrap_or(1)
                    .max(1);
                let before = b.filter_pages_left();
                let finished = b.write_filter(store, pages)?;
                let written = before.saturating_sub(b.filter_pages_left());
                done = done.saturating_add(written.saturating_mul(keys));
                if finished && let Some((first, b)) = self.closing.pop_front() {
                    self.out.push((first, b.into_branch(store)?));
                }
                continue;
            }
            if self.yield_io && !self.merge.ready(store, inputs.clone())? {
                self.waiting = true;
                break;
            }
            let Self {
                merge,
                building,
                closing,
                per,
                remaining,
                drop_tombstones,
                ceiling,
                ..
            } = self;
            let Some((key, op, value)) = merge.entry() else {
                // The merge is done: the last part is sealed, or the compaction is.
                match building.take() {
                    Some((first, mut b, _)) => {
                        b.seal(store)?;
                        closing.push_back((first, b));
                        continue;
                    }
                    None => break,
                }
            };
            if !(*drop_tombstones && op == Op::Delete) {
                if building.as_ref().is_some_and(|(_, _, n)| *n >= *per)
                    && let Some((first, mut b, _)) = building.take()
                {
                    b.seal(store)?;
                    closing.push_back((first, b));
                }
                let (_, b, n) = match building.as_mut() {
                    Some(b) => b,
                    None => {
                        let mut b = Builder::new(store, Keys::AtMost((*remaining).min(*per)))?;
                        // Every key from this first to the range's end, or to the inputs' last
                        // keys, shares with it what it shares with that bound.
                        let bound = merge.end.as_deref().unwrap_or(ceiling.as_slice());
                        b.set_shared(crate::util::keys::common_prefix(key, bound))?;
                        building.insert((key.to_vec(), b, 0))
                    }
                };
                b.add(store, key, op, value)?;
                *n = n.saturating_add(1);
            }
            let consumed = self.merge.next(store, inputs.clone())?;
            self.remaining = self.remaining.saturating_sub(consumed);
            done = done.saturating_add(1);
        }
        Ok(done.min(budget))
    }

    /// Whether every key is merged and every branch made is whole, its filter written.
    pub fn is_done(&self) -> bool {
        !self.merge.opening
            && self.merge.entry().is_none()
            && self.building.is_none()
            && self.closing.is_empty()
    }

    /// The branches made, each with its first key, in key order; the merge must be done.
    pub fn finish<F: BlockFile>(
        self,
        store: &mut Store<F>,
    ) -> Result<Vec<(Vec<u8>, Branch)>, Error> {
        if !self.is_done() {
            return Err(Error::InvalidArgument {
                what: "a compaction finished before its merge",
            });
        }
        self.merge.give_back(store);
        Ok(self.out)
    }
}

/// Merges `branches` (newest first) over `[from, end)` into a new branch: the newest entry of
/// each key, tombstones dropped when `drop_tombstones` (a compaction with nothing older below
/// it). None when nothing is left.
pub fn compact<F: BlockFile>(
    store: &mut Store<F>,
    branches: &[Branch],
    from: &[u8],
    end: Option<&[u8]>,
    drop_tombstones: bool,
) -> Result<Option<Branch>, Error> {
    let mut c = Compaction::new(store, branches, from, end, drop_tombstones, u64::MAX)?;
    c.step(store, branches, u64::MAX)?;
    Ok(c.finish(store)?.into_iter().next().map(|(_, b)| b))
}

/// [`compact`] into branches of at most `per` entries each: each with its first key, in key
/// order, for a leaf's split. Empty when nothing is left.
pub fn compact_split<F: BlockFile>(
    store: &mut Store<F>,
    branches: &[Branch],
    from: &[u8],
    end: Option<&[u8]>,
    drop_tombstones: bool,
    per: u64,
) -> Result<Vec<(Vec<u8>, Branch)>, Error> {
    let mut c = Compaction::new(store, branches, from, end, drop_tombstones, per)?;
    c.step(store, branches, u64::MAX)?;
    c.finish(store)
}
