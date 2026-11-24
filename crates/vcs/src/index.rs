//! A repository's index (§4.17.1 "Objects" item 3): from its tool's id to the row it keeps for each object, held as
//! immutable sorted tables that are content plus a mutable tail, so a fork shares its parent's tables by identity and
//! adds only rows for its own objects.
//!
//! **The tail and its seal.** A row goes into the tail, an ordered map: the open table, as a volume's file bytes stay in
//! an open extent until they are sealed into chunks (D-6). A table is at most one window of the content plane, the
//! store's ([`crate::content`]): the table's size is that derived window, never a number of rows. Once the tail's rows
//! no longer fit one table it is due to seal ([`Index::seal_due`]), and a seal cuts its rows, in key order, into tables
//! of at most a window each, keeping the last, partly filled one's rows in the tail: so the tail's length is one window
//! of rows, and every table a seal writes is full.
//!
//! **Runs.** What one seal or one merge writes is a run: tables in key order whose ranges do not meet. Beside each run
//! the index keeps each table's first key, its fence, so a lookup binary-searches the fences and reads one table a run.
//! A lookup reads the tail, then the runs from the newest, and stops at the first that has the key. An id names one
//! object for ever, so a key is in one layer at most and a row is never rewritten: an insert of a key the index holds
//! is answered `Identical` when its row is the same and refused `RowDiffers` when it is not.
//!
//! **Merging.** A merge ([`Index::compact`]) reads the rows of the newest `j` runs only this index holds, in key order,
//! and cuts them into one run, a step at a time ([`Index::merge_step`]): each step cuts and puts one table, the step
//! after the last swaps the merged run in for the runs it merged, and each step after that gives back one of their
//! tables (goal 93: a last step that put every table and gave back every run took 89.7 ms in r499, past the slice;
//! main's ruling of 2026-11-23 18:41). Which `j`, and when, is the learned rule of [`crate::rule`]: the index counts, for each
//! lookup, how many runs it visited and how many of those it alone holds, and the caller reports the times it
//! measured. Runs a fork shares are never merged, since merging them would copy rows the fork still shares; a run is
//! read again as this index's own once no other index holds its tables (a fork released). Report the lookups' time
//! before a seal or a merge: lookups not yet timed when the runs change are dropped, since they were counted against
//! runs that are gone.
//!
//! **Key filters.** Each run keeps a filter of its keys ([`crate::filter`]), written with its tables by the seal or
//! merge that writes them and charged with its fences, so a lookup passes over a run whose filter says no without
//! resolving its table through the store (goal 93, r536: under load a push's lookups, its duplicate checks nearly all
//! misses, spent their time resolving a table a run). A key's hash is taken once a lookup.
//!
//! **A fork** seals its parent's whole tail, holds each of the parent's tables once more, copies the runs' fences (not
//! their key filters, so a fork costs its fences and no copy of rows' worth; its lookups resolve the runs it shares as
//! before) and starts with an empty tail: it writes no table and copies no row ([`IndexCounters`] are the non-vacuity
//! counters the tests read).
//!
//! **Holds** (the crate's docs, "What holds what"). Each tail row holds the contents it names once. A table stored new
//! takes over its rows' holds, and a table whose bytes were stored already (another index wrote the same rows) gives
//! them back, since the stored copy holds them; a table freed with its last holder gives back its rows'.
//!
//! **Bounds.** Every table is at most a window, and a row too long for a table of its own is refused
//! ([`IndexRefused::RowPastWindow`]). A tail row is charged to the store's budget for its key, its body and its slot
//! ([`TAIL_SLOT_BYTES`]), a run's fences for their keys and their slots ([`FENCE_SLOT_BYTES`]) and its key filter for
//! its words, and a table's bytes are
//! content in the same budget. An insert past the budget is refused with nothing changed; a batch
//! ([`Index::insert_all`]) is checked whole, its order, its rows and its room, before any of it is applied; a seal's
//! tables are admitted as one step ([`ContentStore::put_tables`]), and a merge's a table a step, a refusal giving back
//! what the merge had put.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::iter::Peekable;
use std::mem::{size_of, take};
use std::ops::Bound;

use crate::content::{ContentId, ContentRefused, ContentStore, Stored};
use crate::filter::{KeyFilter, KeyHash};
use crate::rule::Compaction;
use crate::table::{
  RowRef, TABLE_FRAME_BYTES, TableBuilder, TableRefused, TableView, compare_keys, encode_body,
  key_prefix_at, row_bytes,
};

/// Shape: what one tail row costs the budget beyond its key's and its body's bytes: its boxed key and boxed body, the
/// slot each takes in the tail's ordered map. Derived from the types; a lower bound, since a map's nodes keep spare
/// slots.
pub const TAIL_SLOT_BYTES: usize = size_of::<(Box<[u8]>, Box<[u8]>)>();

/// Derived: the size-tiered rule's one decision ([`Index::tiered_due`]): the newest runs are merged through the deepest
/// run whose bytes are at most this many times all the newer runs' together. So each run holds more than this many
/// times the bytes of all the runs newer than it together, the bytes to a run grow more than [`TIER_BASE`] = this + 1 fold a run, and
/// an index's own runs number at most ⌊log_{TIER_BASE}(own bytes ÷ the fewest bytes a seal writes)⌋ + 1. Change it and
/// the base, the bound and the key filters' size ([`crate::filter`]) follow.
pub const TIER_RATIO: u64 = 2;

/// Derived: the base of the size-tiered bound's logarithm, [`TIER_RATIO`] + 1 (three).
pub const TIER_BASE: u64 = TIER_RATIO.saturating_add(1);

/// Shape: what one table of a run costs the budget beside the store beyond its first key's bytes:
/// its fence record plus its 8-byte L1-resident search prefix. Derived from the types.
pub const FENCE_SLOT_BYTES: usize = size_of::<Fence>() + size_of::<u64>();

/// Why the index refused a request. Each leaves the index and the store as they were, unless it says otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexRefused {
  /// The index holds a row of that key with another body: an id names one object for ever.
  RowDiffers,
  /// A batch's keys do not strictly increase.
  Unsorted,
  /// The row is too long for a table of one window by itself, so no table could hold it.
  RowPastWindow,
  /// The store refused: a content the row names is not held, or the budget has no room.
  Content(ContentRefused),
  /// A row broke the table format.
  Table(TableRefused),
  /// A run's table is not in the store or does not open: the index and its store disagree. A release that meets it
  /// goes on releasing the rest and answers it at the end.
  LayerMissing,
}

impl From<ContentRefused> for IndexRefused {
  fn from(refused: ContentRefused) -> IndexRefused {
    IndexRefused::Content(refused)
  }
}

impl From<TableRefused> for IndexRefused {
  fn from(refused: TableRefused) -> IndexRefused {
    IndexRefused::Table(refused)
  }
}

/// What an insert did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inserted {
  /// The row is new.
  New,
  /// The index held the same row already.
  Identical,
}

/// A row to insert: its key, the contents it names, and its payload.
#[derive(Clone, Copy, Debug)]
pub struct NewRow<'a> {
  /// The key: the tool's id, or any key the tool's own index sorts by.
  pub key: &'a [u8],
  /// The contents the row names, each held by it while it is in the index.
  pub contents: &'a [ContentId],
  /// The rest of the row, in the tool's layout.
  pub payload: &'a [u8],
}

/// What a batch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Batch {
  /// Rows inserted new.
  pub new: u64,
  /// Rows the index held already, the same.
  pub identical: u64,
}

/// What the index has done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexCounters {
  /// Tables written by a seal or a merge.
  pub tables_built: u64,
  /// Rows written into tables.
  pub rows_written: u64,
  /// Tables whose bytes the store held already.
  pub tables_deduplicated: u64,
  /// Seals that wrote a run.
  pub seals: u64,
  /// Merges of two runs or more.
  pub compactions: u64,
  /// The runs those merges read.
  pub runs_merged: u64,
  /// Lookups made ([`Index::get`]), for the index's life; a fork counts its own from none.
  pub lookups: u64,
  /// The runs those lookups visited, a miss every run (goal 93: a push's fork seals as it appends, so its lookups walk
  /// runs where they met one tail; arena-19 put the reland at 24-35% of push-all outside the intake).
  pub runs_visited: u64,
  /// The tables those lookups resolved through the store, a run each at most (goal 93, r536: under load a push's
  /// lookups spent their time in `ContentStore::get` and the shared object's atomics behind it, a resolve a run).
  pub resolves: u64,
  /// The runs those lookups passed over on their key filter's word, resolving nothing: the filter's non-vacuity
  /// counter.
  pub filter_rejections: u64,
}

/// One table of a run: its identity, the byte range of its first key inside its [`Run`]'s
/// contiguous `fence_keys` buffer (zero per-table heap allocations), and its row count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fence {
  table: ContentId,
  first_start: u32,
  first_len: u32,
  rows: u64,
}

/// A run (module docs): its tables in key order, their L1-compact big-endian 8-byte key prefixes
/// (`prefixes`), their contiguous first-key bytes (`fence_keys`), their rows and their bytes (what
/// the size-tiered rule weighs, [`Index::tiered_due`]), what their fences and its key filter
/// charged the store, whether another index may hold its tables too (set at a fork or when a
/// table's bytes were stored already; read again from the store's counts before each merge), and
/// its key filter ([`crate::filter`]; none in a fork's copy).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
  fences: Vec<Fence>,
  prefixes: Vec<u64>,
  fence_keys: Vec<u8>,
  rows: u64,
  bytes: u64,
  charged: u64,
  shared: bool,
  filter: KeyFilter,
}

impl Run {
  /// Appends a table's fence to `fences`, `prefixes`, and `fence_keys`.
  fn push_fence(
    fences: &mut Vec<Fence>,
    prefixes: &mut Vec<u64>,
    fence_keys: &mut Vec<u8>,
    table: ContentId,
    first: &[u8],
    rows: u64,
  ) {
    let start = u32::try_from(fence_keys.len()).unwrap_or(u32::MAX);
    let len = u32::try_from(first.len()).unwrap_or(u32::MAX);
    fence_keys.extend_from_slice(first);
    prefixes.push(key_prefix_at(first, 0));
    fences.push(Fence {
      table,
      first_start: start,
      first_len: len,
      rows,
    });
  }

  /// First key slice of fence `at`.
  #[inline]
  fn fence_key(&self, at: usize) -> Option<&[u8]> {
    let fence = self.fences.get(at)?;
    let start = usize::try_from(fence.first_start).ok()?;
    let end = start.checked_add(usize::try_from(fence.first_len).ok()?)?;
    self.fence_keys.get(start..end)
  }

  /// A fork's copy of this run: its fences, charged for them alone, and no key filter (module docs, "A fork").
  fn fork_copy(&self) -> Run {
    Run {
      fences: self.fences.clone(),
      prefixes: self.prefixes.clone(),
      fence_keys: self.fence_keys.clone(),
      rows: self.rows,
      bytes: self.bytes,
      charged: self.charged.saturating_sub(self.filter.bytes()),
      shared: true,
      filter: KeyFilter::default(),
    }
  }

  /// Index of the table that may hold `key`: the last whose first key is not after it. Binary
  /// searches the L1-compact `prefixes` slice first so 99.9% of lookups resolve with zero fence-key
  /// dereferences.
  #[inline]
  fn fence_index(&self, key: &[u8]) -> Option<usize> {
    let key_prefix = key_prefix_at(key, 0);
    let after = self.prefixes.partition_point(|&prefix| prefix < key_prefix);
    if self.prefixes.get(after) != Some(&key_prefix) {
      return after.checked_sub(1);
    }
    // At least one fence shares the same 8-byte prefix: narrow only across the equal-prefix span.
    let eq_end = if self.prefixes.get(after.saturating_add(1)) == Some(&key_prefix) {
      let eq_len = self.prefixes.get(after..).map_or(0, |tail| {
        tail.partition_point(|&prefix| prefix == key_prefix)
      });
      let extra = self
        .fences
        .get(after..after.saturating_add(eq_len))
        .map_or(0, |slice| {
          slice.partition_point(|fence| {
            let start = usize::try_from(fence.first_start).unwrap_or(0);
            let end = start.saturating_add(usize::try_from(fence.first_len).unwrap_or(0));
            let first = self.fence_keys.get(start..end).unwrap_or_default();
            compare_keys(first, key) != std::cmp::Ordering::Greater
          })
        });
      after.saturating_add(extra)
    } else if self
      .fence_key(after)
      .is_some_and(|first| compare_keys(first, key) != std::cmp::Ordering::Greater)
    {
      after.saturating_add(1)
    } else {
      after
    };
    eq_end.checked_sub(1)
  }

  /// Every row, in key order.
  fn rows<'s>(&'s self, store: &'s ContentStore) -> impl Iterator<Item = RowRef<'s>> + 's {
    self
      .fences
      .iter()
      .filter_map(move |fence| view(store, &fence.table))
      .flat_map(|table| table.iter())
  }

  /// Every row whose key is not before `start`, in key order: the table that may hold `start` read from `start` on,
  /// then each table after it whole, since a run's tables do not meet.
  fn rows_from<'s>(
    &'s self,
    store: &'s ContentStore,
    start: &[u8],
  ) -> impl Iterator<Item = RowRef<'s>> + use<'s> {
    let first = self.fence_index(start).unwrap_or(0);
    let first_rows = self
      .fences
      .get(first)
      .and_then(|fence| view(store, &fence.table))
      .map(|table| table.iter_from(start))
      .into_iter()
      .flatten();
    let later = self
      .fences
      .get(first.saturating_add(1)..)
      .unwrap_or_default()
      .iter()
      .filter_map(move |fence| view(store, &fence.table))
      .flat_map(|table| table.iter());
    first_rows.chain(later)
  }
}

/// What one lookup visited: the tail and the runs, and of those runs the ones only this index holds.
#[derive(Debug, Default)]
struct Visit {
  visits: u64,
  compactable: u64,
  runs: u64,
  resolves: u64,
  rejected: u64,
}

impl Visit {
  fn visit(&mut self, compactable: bool) {
    self.visits = self.visits.saturating_add(1);
    if compactable {
      self.compactable = self.compactable.saturating_add(1);
    }
  }
}

/// The tail's first table ([`Index::seal_step`]): its cut, what its rows charged as tail rows and take in tables, and
/// the first key the tail keeps after it.
struct FirstTable {
  cut: Cut,
  discharged: u64,
  bytes: usize,
  kept_from: Option<Box<[u8]>>,
}

/// Rows cut into one table: its bytes, its first key and its rows.
#[derive(Debug)]
struct Cut {
  bytes: Vec<u8>,
  first: Box<[u8]>,
  rows: u64,
}

/// A merge under way ([`Index::merge_begin`]), made a table at a time ([`Index::merge_step`]): which of the index's
/// merges it is, the runs it merges, each by its tables' identities (so a fork that comes to share one, or a run gone,
/// is seen), the last key it cut, the tables cut so far and the store's window when it began. What it has put the index
/// holds ([`MergeWork`]), so a merge dropped part-way leaks nothing.
#[derive(Debug)]
pub struct Merge {
  begun: u64,
  runs: Vec<Vec<ContentId>>,
  after: Option<Box<[u8]>>,
  tables_cut: usize,
  window: usize,
  filter_bytes: u64,
}

impl Merge {
  /// The tables cut so far: the merge's progress.
  pub fn tables_cut(&self) -> usize {
    self.tables_cut
  }

  /// Derived: the most one step puts into the content store ([`Index::merge_step`]), so the room a step is lent: one
  /// table of at most the window and its fence's charge (its first key, a row's at most a window, and the fence's slot),
  /// and the merged run's key filter, charged with its first table.
  pub fn step_room(&self) -> u64 {
    let table = u64::try_from(self.window).unwrap_or(u64::MAX);
    let slot = u64::try_from(FENCE_SLOT_BYTES).unwrap_or(u64::MAX);
    table
      .saturating_mul(2)
      .saturating_add(slot)
      .saturating_add(self.filter_bytes)
  }
}

/// What a merge under way holds between its steps (goal 93, main's ruling of 2026-11-23 18:41: no step puts more than
/// one table): the merged run's tables put so far, each held, its rows' holds taken when it was stored new and its fence
/// charged; and once that run is admitted in place of the runs it merged, their tables left to give back a step each
/// and the merged rows to answer. Given back whole when the merge is given up or refused, when another is begun, and
/// with the index ([`Index::release`]).
#[derive(Debug, Default)]
struct MergeWork {
  /// Which merge this is ([`Merge::begun`]).
  begun: u64,
  /// The merged run's tables put so far.
  fences: Vec<Fence>,
  /// Their L1-compact big-endian 8-byte key prefixes.
  prefixes: Vec<u64>,
  /// Their contiguous first-key bytes.
  fence_keys: Vec<u8>,
  /// Their rows.
  rows: u64,
  /// Their bytes.
  bytes: u64,
  /// Their fences' charge.
  charged: u64,
  /// Of them, the tables found held already.
  deduplicated: u64,
  /// The merged run's key filter, sized for the rows of the runs it merges ([`crate::filter`]), charged with its first
  /// table and noting each table's keys as it is put.
  filter: KeyFilter,
  /// Once the merged run is admitted: its rows, and the merged runs' tables left to give back.
  admitted: Option<(u64, Vec<ContentId>)>,
}

/// What one step of a merge did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStep {
  /// A table cut; more follow.
  More,
  /// The merged run admitted in place of the runs it merged and their tables given back: its rows.
  Done(u64),
  /// Given up, its runs unchanged and what it had put given back: its runs are no longer all this index's own, or
  /// another merge of the index was begun since.
  GivenUp,
}

/// A repository's index (module docs).
#[derive(Debug, Default)]
pub struct Index {
  /// The runs, oldest first.
  runs: Vec<Run>,
  tail: BTreeMap<Box<[u8]>, Box<[u8]>>,
  /// The bytes the tail charged to the store.
  tail_charged: u64,
  /// The bytes the tail's rows would take in tables (each row's [`row_bytes`]), held against the window.
  tail_bytes: usize,
  /// Visits since the caller last reported its lookups' time.
  visits: Cell<u64>,
  /// Lookups made, for the index's life ([`IndexCounters::lookups`]).
  lookups: Cell<u64>,
  /// The runs those lookups visited ([`IndexCounters::runs_visited`]).
  runs_visited: Cell<u64>,
  /// The tables those lookups resolved ([`IndexCounters::resolves`]).
  resolves: Cell<u64>,
  /// The runs those lookups passed over on their filters ([`IndexCounters::filter_rejections`]).
  filter_rejections: Cell<u64>,
  /// Lookups since the last report that visited zero runs only this index holds (`depths[0]` fast path
  /// avoiding a `RefCell` borrow on read-only forks and single-shared-run lookups).
  depth_zero: Cell<u64>,
  /// `depths[p]`: the lookups since then that visited `p` runs only this index holds.
  depths: RefCell<Vec<u64>>,
  /// Rows inserted since the caller last reported its inserts' time.
  inserted: u64,
  /// The rows of the last merge, until the caller reports its time.
  compacted_rows: u64,
  rule: Compaction,
  counters: IndexCounters,
  /// The rows this index has taken in that it did not hold, by insert or adoption: what tells a fork whether its parent
  /// changed since it was made ([`Index::adopt`]).
  keys_added: u64,
  /// For a fork, the runs it took from its parent (the first of its runs) and its parent's `keys_added` then.
  forked: Option<(usize, u64)>,
  /// A seal of the whole tail under way a table at a time ([`Index::seal_step`], A-722 part 4): the runs only this index
  /// held before its run, the last of `runs`, was begun.
  sealing: Option<usize>,
  /// What the merge under way holds between its steps ([`Index::merge_step`]).
  merge_work: MergeWork,
  /// Merges begun, for the index's life: what tells a [`Merge`] whether it is still the index's merge under way.
  merges_begun: u64,
}

/// Why a parent did not adopt a fork ([`Index::adopt`]), the fork handed back whole.
#[derive(Debug)]
pub enum AdoptRefused {
  /// The parent took in rows since the fork was made, so a key of the fork's may be the parent's too: the fork's rows
  /// are to be checked against the parent first (A-722's stale fork).
  Stale(Box<Index>),
  /// The index is no fork of this parent: it was made by no fork, or by one of another index.
  NotAFork(Box<Index>),
  /// Sealing the fork's tail was refused.
  Refused(IndexRefused, Box<Index>),
}

/// A table, opened.
fn view<'s>(store: &'s ContentStore, table: &ContentId) -> Option<TableView<'s>> {
  TableView::open_known(store.get_table(table)?).ok()
}

/// What a tail row of `key` and `body` is charged.
fn tail_cost(key: &[u8], body: &[u8]) -> Result<u64, IndexRefused> {
  let bytes = key
    .len()
    .checked_add(body.len())
    .and_then(|bytes| bytes.checked_add(TAIL_SLOT_BYTES))
    .ok_or(TableRefused::TooLarge)?;
  Ok(u64::try_from(bytes).map_err(|_| TableRefused::TooLarge)?)
}

/// Takes one hold of each content, or none: a refusal gives back the holds taken before it.
fn hold_all(store: &mut ContentStore, contents: &[ContentId]) -> Result<(), IndexRefused> {
  for (taken, content) in contents.iter().enumerate() {
    if let Err(refused) = store.hold(content) {
      give_back(store, contents.get(..taken).unwrap_or_default())?;
      return Err(refused.into());
    }
  }
  Ok(())
}

/// Gives back one hold of each content, going on past a refusal and answering the first.
fn give_back(store: &mut ContentStore, contents: &[ContentId]) -> Result<(), IndexRefused> {
  let mut first = Ok(());
  for content in contents {
    first = first.and(store.release(content).map(drop));
  }
  first.map_err(IndexRefused::from)
}

/// Gives back the holds `rows` take of their contents, going on past a refusal and answering the first.
fn release_rows<'a>(
  store: &mut ContentStore,
  rows: impl Iterator<Item = RowRef<'a>>,
) -> Result<(), IndexRefused> {
  let mut first = Ok(());
  for row in rows {
    for content in row.contents() {
      first = first.and(store.release(&content).map(drop));
    }
  }
  first.map_err(IndexRefused::from)
}

/// Takes the holds `rows` take of their contents, all or none ([`hold_all`]).
fn hold_rows<'a>(
  store: &mut ContentStore,
  rows: impl Iterator<Item = RowRef<'a>>,
) -> Result<(), IndexRefused> {
  let contents: Vec<ContentId> = rows.flat_map(|row| row.contents()).collect();
  hold_all(store, &contents)
}

/// The holds a table's rows take, read from its bytes: taken (`hold`) or given back.
fn table_rows_held(store: &mut ContentStore, bytes: &[u8], hold: bool) -> Result<(), IndexRefused> {
  let view = TableView::open_known(bytes)?;
  if hold {
    hold_rows(store, view.iter())
  } else {
    release_rows(store, view.iter())
  }
}

/// Gives back this index's hold of a table; a table freed with it gives back its rows'.
fn release_table(store: &mut ContentStore, table: &ContentId) -> Result<(), IndexRefused> {
  let crate::content::Freed::Table(freed) = store.release(table)? else {
    return Ok(());
  };
  let view = TableView::open_known(&freed).map_err(|_| IndexRefused::LayerMissing)?;
  release_rows(store, view.iter())
}

/// Gives back a run's tables and its fences' charge, going on past a refusal and answering the first.
fn release_run(store: &mut ContentStore, run: &Run) -> Result<(), IndexRefused> {
  let mut first = Ok(());
  for fence in &run.fences {
    first = first.and(release_table(store, &fence.table));
  }
  first.and(store.discharge(run.charged).map_err(IndexRefused::from))
}

/// The tail's rows, in key order.
fn tail_rows(tail: &BTreeMap<Box<[u8]>, Box<[u8]>>) -> impl Iterator<Item = RowRef<'_>> {
  tail.iter().filter_map(|(key, body)| RowRef::new(key, body))
}

/// Rows in key order cut into tables of at most `window` bytes, greedily: a table takes rows until the next would pass
/// the window. A row too long for a table alone gets one of its own, which the store then refuses.
fn cut<'a>(
  rows: impl Iterator<Item = RowRef<'a>>,
  window: usize,
) -> Result<Vec<Cut>, IndexRefused> {
  let mut cuts = Vec::new();
  let mut builder = TableBuilder::new();
  let mut first: Option<Box<[u8]>> = None;
  for row in rows {
    let bytes = row_bytes(row.key(), row.body()).ok_or(TableRefused::TooLarge)?;
    if !builder.is_empty() && builder.len().saturating_add(bytes) > window {
      cuts.push(finish(take(&mut builder), first.take())?);
    }
    if builder.is_empty() {
      first = Some(Box::from(row.key()));
    }
    builder.push(row.key(), row.body())?;
  }
  if !builder.is_empty() {
    cuts.push(finish(builder, first)?);
  }
  Ok(cuts)
}

/// A table cut from rows: its cut, and its first key (none: no row was left).
type OneTable = (Option<Cut>, Option<Box<[u8]>>);

/// The first table of `rows` (in key order) cut as [`cut`] cuts each, at most `window` bytes, and its last key; none when
/// no row is left.
fn one_table<'a>(
  rows: impl Iterator<Item = RowRef<'a>>,
  window: usize,
) -> Result<OneTable, IndexRefused> {
  let mut builder = TableBuilder::new();
  let mut first: Option<Box<[u8]>> = None;
  let mut last: Option<Box<[u8]>> = None;
  for row in rows {
    let bytes = row_bytes(row.key(), row.body()).ok_or(TableRefused::TooLarge)?;
    if !builder.is_empty() && builder.len().saturating_add(bytes) > window {
      break;
    }
    if builder.is_empty() {
      first = Some(Box::from(row.key()));
    }
    builder.push(row.key(), row.body())?;
    last = Some(Box::from(row.key()));
  }
  if builder.is_empty() {
    return Ok((None, None));
  }
  Ok((Some(finish(builder, first)?), last))
}

/// A finished table's cut.
fn finish(builder: TableBuilder, first: Option<Box<[u8]>>) -> Result<Cut, IndexRefused> {
  let rows = builder.rows();
  Ok(Cut {
    bytes: builder.finish()?,
    first: first.unwrap_or_default(),
    rows,
  })
}

/// What the fences of `cuts` charge: each first key and its slot.
fn fences_charge(cuts: &[Cut]) -> Result<u64, IndexRefused> {
  let mut charge: u64 = 0;
  for cut in cuts {
    let bytes = cut
      .first
      .len()
      .checked_add(FENCE_SLOT_BYTES)
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or(TableRefused::TooLarge)?;
    charge = charge.checked_add(bytes).ok_or(TableRefused::TooLarge)?;
  }
  Ok(charge)
}

/// The cuts' rows.
fn cut_rows(cuts: &[Cut]) -> u64 {
  cuts
    .iter()
    .fold(0_u64, |rows, cut| rows.saturating_add(cut.rows))
}

/// The run `cuts` make once `stored` put them.
fn run_of(cuts: Vec<Cut>, stored: &[Stored], charged: u64, filter: KeyFilter) -> Run {
  let rows = cut_rows(&cuts);
  let bytes = cuts.iter().fold(0_u64, |bytes, cut| {
    bytes.saturating_add(u64::try_from(cut.bytes.len()).unwrap_or(u64::MAX))
  });
  let shared = stored.iter().any(|put| !put.fresh);
  let key_bytes: usize = cuts.iter().map(|cut| cut.first.len()).sum();
  let mut fences = Vec::with_capacity(cuts.len());
  let mut prefixes = Vec::with_capacity(cuts.len());
  let mut fence_keys = Vec::with_capacity(key_bytes);
  for (cut, put) in cuts.into_iter().zip(stored) {
    Run::push_fence(
      &mut fences,
      &mut prefixes,
      &mut fence_keys,
      put.identity,
      &cut.first,
      cut.rows,
    );
  }
  Run {
    fences,
    prefixes,
    fence_keys,
    rows,
    bytes,
    charged,
    shared,
    filter,
  }
}

/// The hashes of the keys of the table of `bytes`, taken before the table changes anything, so its run's key filter
/// ([`KeyFilter::add`]) notes every key or the step is refused with nothing changed: a filter missing a key would say no
/// to a row its run holds.
fn table_hashes(bytes: &[u8]) -> Result<Vec<KeyHash>, IndexRefused> {
  Ok(
    TableView::open_known(bytes)?
      .iter()
      .map(|row| KeyHash::of(row.key()))
      .collect(),
  )
}

/// Notes `hashes` in `filter`.
fn note_all(filter: &mut KeyFilter, hashes: &[KeyHash]) {
  for hash in hashes {
    filter.add(hash);
  }
}

/// A row of a batch as planned: its body and its charge when it is new, `None` when the index holds it.
type Planned = Option<(Vec<u8>, u64)>;

/// One source of rows in key order, boxed so that tails and runs merge alike.
type Source<'a> = Peekable<Box<dyn Iterator<Item = RowRef<'a>> + 'a>>;

/// Rows of several sources in key order: each step takes the least key the sources' heads hold.
struct Merged<'a> {
  sources: Vec<Source<'a>>,
}

impl<'a> Iterator for Merged<'a> {
  type Item = RowRef<'a>;

  fn next(&mut self) -> Option<RowRef<'a>> {
    let mut least: Option<(usize, &'a [u8])> = None;
    for (at, source) in self.sources.iter_mut().enumerate() {
      if let Some(row) = source.peek()
        && least.is_none_or(|(_, key)| row.key() < key)
      {
        least = Some((at, row.key()));
      }
    }
    let (at, _) = least?;
    self.sources.get_mut(at)?.next()
  }
}

impl Index {
  /// An empty index.
  pub fn new() -> Index {
    Index::default()
  }

  /// The row of `key`, with what the lookup visited counted for the merge rule.
  pub fn get<'s>(&'s self, store: &'s ContentStore, key: &[u8]) -> Option<RowRef<'s>> {
    let mut visit = Visit::default();
    let found = self.find(store, key, &mut visit);
    self
      .visits
      .set(self.visits.get().saturating_add(visit.visits));
    self.lookups.set(self.lookups.get().saturating_add(1));
    self
      .runs_visited
      .set(self.runs_visited.get().saturating_add(visit.runs));
    self
      .resolves
      .set(self.resolves.get().saturating_add(visit.resolves));
    self
      .filter_rejections
      .set(self.filter_rejections.get().saturating_add(visit.rejected));
    if visit.compactable == 0 {
      self.depth_zero.set(self.depth_zero.get().saturating_add(1));
    } else if let Ok(mut depths) = self.depths.try_borrow_mut() {
      let at = usize::try_from(visit.compactable).unwrap_or(usize::MAX);
      if depths.len() <= at {
        depths.resize(at.saturating_add(1), 0);
      }
      if let Some(count) = depths.get_mut(at) {
        *count = count.saturating_add(1);
      }
    }
    found
  }

  /// The tail, then the runs from the newest, to the first that has `key`. A table missing from the store is skipped
  /// (the release that would meet it says so).
  fn find<'s>(
    &'s self,
    store: &'s ContentStore,
    key: &[u8],
    visit: &mut Visit,
  ) -> Option<RowRef<'s>> {
    if !self.tail.is_empty() {
      visit.visit(false);
      if let Some((stored, body)) = self.tail.get_key_value(key) {
        return RowRef::new(stored, body);
      }
    }
    if self.runs.is_empty() {
      return None;
    }
    let mut hash: Option<KeyHash> = None;
    self.runs.iter().rev().find_map(|run| {
      visit.visit(!run.shared);
      visit.runs = visit.runs.saturating_add(1);
      if !run.filter.is_empty() {
        let key_hash = *hash.get_or_insert_with(|| KeyHash::of(key));
        if !run.filter.may_hold(&key_hash) {
          visit.rejected = visit.rejected.saturating_add(1);
          return None;
        }
      }
      let idx = run.fence_index(key)?;
      let fence = run.fences.get(idx)?;
      let lo_key = run.fence_key(idx)?;
      let hi_key = run.fence_key(idx.saturating_add(1));
      visit.resolves = visit.resolves.saturating_add(1);
      view(store, &fence.table)?.get_hinted(key, lo_key, hi_key)
    })
  }

  /// Inserts one row; `Identical` when the index holds the same row, `RowDiffers` when it holds another of the key.
  pub fn insert(
    &mut self,
    store: &mut ContentStore,
    row: NewRow<'_>,
  ) -> Result<Inserted, IndexRefused> {
    match self.plan_row(store, row)? {
      Some(body) => {
        let charge = tail_cost(row.key, &body)?;
        let room = store.room();
        if charge > room {
          return Err(ContentRefused::OverBudget { need: charge, room }.into());
        }
        self.admit(store, row, body, charge)?;
        Ok(Inserted::New)
      }
      None => Ok(Inserted::Identical),
    }
  }

  /// Inserts `rows`, whose keys strictly increase, as one step: the order, every row against the index, every content
  /// held, and the room for all of them are checked before any is applied, so a refusal changes nothing.
  pub fn insert_all(
    &mut self,
    store: &mut ContentStore,
    rows: &[NewRow<'_>],
  ) -> Result<Batch, IndexRefused> {
    let plan = self.plan(store, rows)?;
    let mut batch = Batch::default();
    for (row, planned) in rows.iter().zip(plan) {
      match planned {
        Some((body, charge)) => {
          self.admit(store, *row, body, charge)?;
          batch.new = batch.new.saturating_add(1);
        }
        None => batch.identical = batch.identical.saturating_add(1),
      }
    }
    Ok(batch)
  }

  /// The checks of [`Index::insert_all`]: for each row, its body and charge when it is new, `None` when it is held.
  fn plan(&self, store: &ContentStore, rows: &[NewRow<'_>]) -> Result<Vec<Planned>, IndexRefused> {
    let mut previous: Option<&[u8]> = None;
    let mut need: u64 = 0;
    let mut plan = Vec::with_capacity(rows.len());
    for row in rows {
      if previous.is_some_and(|previous| previous >= row.key) {
        return Err(IndexRefused::Unsorted);
      }
      previous = Some(row.key);
      let planned = match self.plan_row(store, *row)? {
        Some(body) => {
          let charge = tail_cost(row.key, &body)?;
          need = need.checked_add(charge).ok_or(TableRefused::TooLarge)?;
          Some((body, charge))
        }
        None => None,
      };
      plan.push(planned);
    }
    let room = store.room();
    if need > room {
      return Err(ContentRefused::OverBudget { need, room }.into());
    }
    Ok(plan)
  }

  /// One row against the index: `None` when it holds the same row; the new row's body when it holds none of the key, the
  /// row fits a table of its own, and the store holds every content the row names.
  fn plan_row(
    &self,
    store: &ContentStore,
    row: NewRow<'_>,
  ) -> Result<Option<Vec<u8>>, IndexRefused> {
    let body = encode_body(row.contents, row.payload)?;
    if let Some(found) = self.get(store, row.key) {
      if found.body() == body.as_slice() {
        return Ok(None);
      }
      return Err(IndexRefused::RowDiffers);
    }
    let alone = row_bytes(row.key, &body)
      .and_then(|bytes| bytes.checked_add(TABLE_FRAME_BYTES))
      .ok_or(TableRefused::TooLarge)?;
    if alone > store.window() {
      return Err(IndexRefused::RowPastWindow);
    }
    if !row.contents.iter().all(|content| store.contains(content)) {
      return Err(ContentRefused::NotHeld.into());
    }
    Ok(Some(body))
  }

  /// Puts a checked new row into the tail: its contents' holds, then its charge.
  fn admit(
    &mut self,
    store: &mut ContentStore,
    row: NewRow<'_>,
    body: Vec<u8>,
    charge: u64,
  ) -> Result<(), IndexRefused> {
    let bytes = row_bytes(row.key, &body).ok_or(TableRefused::TooLarge)?;
    hold_all(store, row.contents)?;
    if let Err(refused) = store.charge(charge) {
      give_back(store, row.contents)?;
      return Err(refused.into());
    }
    self
      .tail
      .insert(Box::from(row.key), body.into_boxed_slice());
    self.tail_charged = self.tail_charged.saturating_add(charge);
    self.tail_bytes = self.tail_bytes.saturating_add(bytes);
    self.inserted = self.inserted.saturating_add(1);
    self.keys_added = self.keys_added.saturating_add(1);
    Ok(())
  }

  /// Whether the tail's rows no longer fit one table of the store's window: the seal rule (module docs).
  pub fn seal_due(&self, store: &ContentStore) -> bool {
    TABLE_FRAME_BYTES.saturating_add(self.tail_bytes) > store.window()
  }

  /// Seals the tail's full tables (module docs): its rows cut into tables of at most a window, every table but the
  /// last written as one new run, the last one's rows kept in the tail; the rows sealed. Nothing is sealed until the
  /// tail is due. Refused, nothing changed.
  pub fn seal(&mut self, store: &mut ContentStore) -> Result<u64, IndexRefused> {
    self.seal_rows(store, true)
  }

  /// Seals the whole tail now, its last table too, so the index is wholly content, named by its tables alone (what a
  /// fork does first, and what an owner does before it names the index by its tables, to a snapshot or a replica); the
  /// rows sealed. Refused, nothing changed.
  pub fn seal_all(&mut self, store: &mut ContentStore) -> Result<u64, IndexRefused> {
    if self.sealing.is_some() {
      let before = self.rows();
      while !self.seal_step(store)? {}
      return Ok(self.rows().saturating_sub(before));
    }
    self.seal_rows(store, false)
  }

  /// One table of [`Index::seal_all`] (A-722 part 4: a large tail sealed over many turns): the tail's first rows that fit
  /// a window, cut by the rule `seal_all` cuts by, written as the next table of one run kept as the index's newest, so
  /// every read finds its rows meanwhile; the run's bookkeeping done once the tail is empty. A tail sealed so, step by
  /// step, leaves the index and the store as `seal_all` leaves them. Whether the tail is sealed whole. Refused, the step
  /// changed nothing.
  pub fn seal_step(&mut self, store: &mut ContentStore) -> Result<bool, IndexRefused> {
    let Some(table) = self.first_table(store.window())? else {
      self.sealed_whole();
      return Ok(true);
    };
    let fence = table
      .cut
      .first
      .len()
      .checked_add(FENCE_SLOT_BYTES)
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or(TableRefused::TooLarge)?;
    // The run's key filter, sized for the whole tail's rows, is charged with its first table (rows the tail takes in
    // meanwhile only raise its rate of a wrong yes, never make it say no to a key the run holds).
    let mut filter = match self.sealing {
      Some(_) => None,
      None => Some(KeyFilter::sized(
        u64::try_from(self.tail.len()).unwrap_or(u64::MAX),
        store.budget(),
      )),
    };
    let filter_bytes = filter.as_ref().map_or(0, KeyFilter::bytes);
    let hashes = table_hashes(&table.cut.bytes)?;
    let stored = store.put_tables(
      &[table.cut.bytes.as_slice()],
      table.discharged,
      fence.saturating_add(filter_bytes),
    )?;
    let put = stored.first().ok_or(IndexRefused::LayerMissing)?;
    let given_back = if put.fresh {
      Ok(())
    } else {
      table_rows_held(store, &table.cut.bytes, false)
    };
    self.tail = match table.kept_from {
      Some(first_kept) => self.tail.split_off(first_kept.as_ref()),
      None => BTreeMap::new(),
    };
    self.tail_charged = self.tail_charged.saturating_sub(table.discharged);
    self.tail_bytes = self.tail_bytes.saturating_sub(table.bytes);
    if let Some(filter) = filter.take() {
      self.sealing = Some(self.compactable());
      self.runs.push(Run {
        fences: Vec::new(),
        prefixes: Vec::new(),
        fence_keys: Vec::new(),
        rows: 0,
        bytes: 0,
        charged: filter_bytes,
        shared: false,
        filter,
      });
    }
    let rows = table.cut.rows;
    if let Some(run) = self.runs.last_mut() {
      note_all(&mut run.filter, &hashes);
      Run::push_fence(
        &mut run.fences,
        &mut run.prefixes,
        &mut run.fence_keys,
        put.identity,
        &table.cut.first,
        rows,
      );
      run.rows = run.rows.saturating_add(rows);
      run.bytes = run
        .bytes
        .saturating_add(u64::try_from(table.cut.bytes.len()).unwrap_or(u64::MAX));
      run.charged = run.charged.saturating_add(fence);
      run.shared |= !put.fresh;
    }
    self.counters.tables_built = self.counters.tables_built.saturating_add(1);
    self.counters.rows_written = self.counters.rows_written.saturating_add(rows);
    if !put.fresh {
      self.counters.tables_deduplicated = self.counters.tables_deduplicated.saturating_add(1);
    }
    let whole = self.tail.is_empty();
    if whole {
      self.sealed_whole();
    }
    given_back.map(|()| whole)
  }

  /// The tail's first table as [`cut`] cuts it: its rows that fit a window, what they charged as tail rows and take
  /// in tables, and the first key past them; none for an empty tail.
  fn first_table(&self, window: usize) -> Result<Option<FirstTable>, IndexRefused> {
    let mut builder = TableBuilder::new();
    let mut first: Option<Box<[u8]>> = None;
    let (mut discharged, mut bytes) = (0_u64, 0_usize);
    let mut kept_from = None;
    for row in tail_rows(&self.tail) {
      let row_len = row_bytes(row.key(), row.body()).ok_or(TableRefused::TooLarge)?;
      if !builder.is_empty() && builder.len().saturating_add(row_len) > window {
        kept_from = Some(Box::from(row.key()));
        break;
      }
      if builder.is_empty() {
        first = Some(Box::from(row.key()));
      }
      builder.push(row.key(), row.body())?;
      discharged = discharged.saturating_add(tail_cost(row.key(), row.body())?);
      bytes = bytes.saturating_add(row_len);
    }
    if builder.is_empty() {
      return Ok(None);
    }
    Ok(Some(FirstTable {
      cut: finish(builder, first)?,
      discharged,
      bytes,
      kept_from,
    }))
  }

  /// A step-by-step seal's end ([`Index::seal_step`]): its run counted as [`Index::seal_all`]'s is, once.
  fn sealed_whole(&mut self) {
    let Some(compactable) = self.sealing.take() else {
      return;
    };
    let own = self.runs.last().is_some_and(|run| !run.shared);
    if own {
      self.rule.run_added(compactable);
      self.drop_untimed();
    }
    self.counters.seals = self.counters.seals.saturating_add(1);
  }

  /// Cuts the tail and writes its tables as one run, all of them or all but the last (`keep_last`).
  fn seal_rows(&mut self, store: &mut ContentStore, keep_last: bool) -> Result<u64, IndexRefused> {
    let mut cuts = cut(tail_rows(&self.tail), store.window())?;
    if keep_last {
      cuts.pop();
    }
    if cuts.is_empty() {
      return Ok(0);
    }
    let sealed = usize::try_from(cut_rows(&cuts)).map_err(|_| TableRefused::TooLarge)?;
    let (mut discharged, mut bytes) = (0_u64, 0_usize);
    for (key, body) in self.tail.iter().take(sealed) {
      discharged = discharged.saturating_add(tail_cost(key, body)?);
      bytes = bytes.saturating_add(row_bytes(key, body).ok_or(TableRefused::TooLarge)?);
    }
    let kept_from: Option<Box<[u8]>> = self.tail.keys().nth(sealed).cloned();
    let mut filter = KeyFilter::sized(cut_rows(&cuts), store.budget());
    for cut in &cuts {
      note_all(&mut filter, &table_hashes(&cut.bytes)?);
    }
    let fences = fences_charge(&cuts)?.saturating_add(filter.bytes());
    let tables: Vec<&[u8]> = cuts.iter().map(|cut| cut.bytes.as_slice()).collect();
    let stored = store.put_tables(&tables, discharged, fences)?;
    let mut given_back = Ok(());
    for (cut, put) in cuts.iter().zip(&stored) {
      if !put.fresh {
        given_back = given_back.and(table_rows_held(store, &cut.bytes, false));
      }
    }
    self.tail = match kept_from {
      Some(first_kept) => self.tail.split_off(first_kept.as_ref()),
      None => BTreeMap::new(),
    };
    self.tail_charged = self.tail_charged.saturating_sub(discharged);
    self.tail_bytes = self.tail_bytes.saturating_sub(bytes);
    let rows = self.add_run(cuts, &stored, fences, filter);
    self.counters.seals = self.counters.seals.saturating_add(1);
    given_back.map(|()| rows)
  }

  /// Records a run a seal wrote, its key filter `filter`, on top; its rows.
  fn add_run(&mut self, cuts: Vec<Cut>, stored: &[Stored], charged: u64, filter: KeyFilter) -> u64 {
    let tables = u64::try_from(cuts.len()).unwrap_or(u64::MAX);
    let deduplicated = stored.iter().filter(|put| !put.fresh).count();
    let run = run_of(cuts, stored, charged, filter);
    self.admit_run(run, tables, u64::try_from(deduplicated).unwrap_or(u64::MAX))
  }

  /// Records `run`, of `tables` tables of which `deduplicated` were found held already, on top: a seal's or a merge's;
  /// its rows.
  fn admit_run(&mut self, run: Run, tables: u64, deduplicated: u64) -> u64 {
    let rows = run.rows;
    let compactable = self.compactable();
    let own = !run.shared;
    self.runs.push(run);
    if own {
      self.rule.run_added(compactable);
      self.drop_untimed();
    }
    self.counters.tables_built = self.counters.tables_built.saturating_add(tables);
    self.counters.rows_written = self.counters.rows_written.saturating_add(rows);
    self.counters.tables_deduplicated = self
      .counters
      .tables_deduplicated
      .saturating_add(deduplicated);
    rows
  }

  /// The runs only this index holds.
  fn compactable(&self) -> usize {
    self.runs.iter().filter(|run| !run.shared).count()
  }

  /// Drops the lookups not yet timed: they were counted against runs that changed (module docs).
  fn drop_untimed(&mut self) {
    self.visits.set(0);
    self.depth_zero.set(0);
    self.depths.get_mut().clear();
  }

  /// The runs changed under the rule: every rent starts over (`crate::rule`), and the untimed lookups are dropped.
  fn restart(&mut self) {
    let compactable = self.compactable();
    self.rule.restart(compactable);
    self.drop_untimed();
  }

  /// Reads again, from the store's counts, which shared runs no other index holds now (a fork released them); a change
  /// restarts the rule.
  fn relearn(&mut self, store: &ContentStore) {
    let mut changed = false;
    for run in self.runs.iter_mut().filter(|run| run.shared) {
      if !run
        .fences
        .iter()
        .any(|fence| store.holders(&fence.table) > 1)
      {
        run.shared = false;
        changed = true;
      }
    }
    if changed {
      self.restart();
    }
  }

  /// The merge the size-tiered rule says is due now (goal 93: a push's fork seals as it appends and never had its runs
  /// merged, so each new row's lookup walked every run it sealed; arena-14's segment steps grew past the watchdog): the
  /// number of the newest runs only this index holds to merge, through the deepest own run whose tables' bytes are at
  /// most [`TIER_RATIO`] times the bytes of all the own runs newer than it together; `None` where there is none. The
  /// rule's invariant is that every own run holds more than that many times the bytes of all newer own runs together,
  /// and a seal adds bytes to the newer runs of every run below it, so it can break the invariant at any depth (r618:
  /// the rule once stopped at the first older run past its share, and held 8 own runs where the bound is 6; runs of
  /// 100, 40 and 10 rows and a seal of 4 left the oldest at most twice the 54 newer, nothing due). Merging the newest
  /// through the deepest such run changes no deeper run's newer bytes, so one merge restores the invariant for the whole
  /// stack. Asked after every seal and merged to its end each time, the bytes to a run grow more than [`TIER_BASE`] fold
  /// a run. Weighed in bytes, not rows (goal 93 (B); r499 held 8 own runs where a bound in rows computed 6): a seal is
  /// due by bytes, a window of them, so a seal of large rows writes few rows, and a rule in rows let each phase of larger
  /// rows keep a stack of its own beside the runs of smaller rows before it.
  ///
  /// Derived ([`TIER_RATIO`], [`TIER_BASE`]): where no merge is due, an index's own runs number at most ⌊log₃(their
  /// tables' bytes ÷ the fewest bytes a seal writes)⌋ + 1 ([`Index::own_bytes`]); between a seal and the end of the
  /// merge it makes due, one more (the run it sealed). A seal writes a window's bytes of rows at most, and about a window
  /// when its rows are small beside the window; a table cut before a row near a window long is short, so the fewest is
  /// the data's, not fixed. A lookup walks that many of them at most beside the runs it shares (its parent's, merged by
  /// the parent's own rule). Read from the runs as they are marked (a shared run is passed over), with no look at the
  /// store: cheap after any seal. Beside [`Index::compaction_due`]'s rent-or-buy, which weighs measured lookups an index
  /// answers for long.
  pub fn tiered_due(&self) -> Option<usize> {
    let mut newer = 0_u64;
    let mut due = None;
    for (depth, run) in self.runs.iter().rev().filter(|run| !run.shared).enumerate() {
      if depth > 0 && run.bytes <= newer.saturating_mul(TIER_RATIO) {
        due = Some(depth.saturating_add(1));
      }
      newer = newer.saturating_add(run.bytes);
    }
    due
  }

  /// The bytes of the tables of the runs only this index holds: what the size-tiered rule's bound is stated over
  /// ([`Index::tiered_due`]).
  pub fn own_bytes(&self) -> u64 {
    self
      .runs
      .iter()
      .filter(|run| !run.shared)
      .fold(0_u64, |bytes, run| bytes.saturating_add(run.bytes))
  }

  /// How many runs only this index holds: the runs [`Index::tiered_due`] and [`Index::compaction_due`] may merge.
  pub fn own_runs(&self) -> usize {
    self.runs.iter().filter(|run| !run.shared).count()
  }

  /// The merge the rule says is due now, as the number of newest runs only this index holds to merge, or `None`.
  pub fn compaction_due(&mut self, store: &ContentStore) -> Option<usize> {
    self.relearn(store);
    let mut rows = Vec::new();
    let mut total: u64 = 0;
    for run in self.runs.iter().rev().filter(|run| !run.shared) {
      total = total.saturating_add(run.rows);
      rows.push(total);
    }
    self.rule.due(&rows)
  }

  /// Merges the newest `runs` runs only this index holds into one run (module docs), at once: [`Index::merge_begin`] and
  /// every [`Index::merge_step`] to its end; the rows written. Fewer than two such runs merge nothing. Each merged table
  /// takes its rows' holds before the merged runs give theirs back: refused, what the merge put is given back and the
  /// runs are as they were.
  pub fn compact(&mut self, store: &mut ContentStore, runs: usize) -> Result<u64, IndexRefused> {
    let Some(mut merge) = self.merge_begin(store, runs)? else {
      return Ok(0);
    };
    loop {
      match self.merge_step(store, &mut merge)? {
        MergeStep::More => {}
        MergeStep::Done(rows) => return Ok(rows),
        MergeStep::GivenUp => return Ok(0),
      }
    }
  }

  /// Plans a merge of the newest `runs` runs only this index holds (module docs, "Merging"), made a table at a time by
  /// [`Index::merge_step`], so its work fits a step's budget however large the runs are; `None` for fewer than two such
  /// runs. Lookups read the runs it merges until the step that swaps its run in. A merge left part-way before (its
  /// caller dropped it) is given back first; refused, the give-back's refusal.
  pub fn merge_begin(
    &mut self,
    store: &mut ContentStore,
    runs: usize,
  ) -> Result<Option<Merge>, IndexRefused> {
    self.merge_given_back(store)?;
    self.relearn(store);
    let merging: Vec<Vec<ContentId>> = self
      .runs
      .iter()
      .rev()
      .filter(|run| !run.shared)
      .take(runs)
      .map(|run| run.fences.iter().map(|fence| fence.table).collect())
      .collect();
    if merging.len() < 2 {
      return Ok(None);
    }
    let rows = self
      .runs
      .iter()
      .rev()
      .filter(|run| !run.shared)
      .take(merging.len())
      .fold(0_u64, |rows, run| rows.saturating_add(run.rows));
    let filter = KeyFilter::sized(rows, store.budget());
    self.merges_begun = self.merges_begun.saturating_add(1);
    let filter_bytes = filter.bytes();
    self.merge_work = MergeWork {
      begun: self.merges_begun,
      filter,
      ..MergeWork::default()
    };
    Ok(Some(Merge {
      begun: self.merges_begun,
      runs: merging,
      after: None,
      tables_cut: 0,
      window: store.window(),
      filter_bytes,
    }))
  }

  /// One step of `merge` (module docs, "Merging"): the next table of its merged run cut from the rows of the runs it
  /// merges past the last key it cut and put into the store; once no row is left, the merged run swapped in for the runs
  /// it merged; then one of their tables given back a step, the last answering `Done`. A merge whose runs are no longer
  /// all this index's own (a fork shares one, or one is gone), or that is no longer the index's merge under way, is given
  /// up, its runs unchanged and what it put given back. Refused, the merge is given up likewise and the refusal
  /// answered (a refusal giving a table back answered first).
  pub fn merge_step(
    &mut self,
    store: &mut ContentStore,
    merge: &mut Merge,
  ) -> Result<MergeStep, IndexRefused> {
    if merge.begun != self.merge_work.begun {
      return Ok(MergeStep::GivenUp);
    }
    if self.merge_work.admitted.is_some() {
      return self.merge_give_back_step(store);
    }
    self.relearn(store);
    let Some(merging) = self.merging_positions(merge) else {
      self.merge_given_back(store)?;
      return Ok(MergeStep::GivenUp);
    };
    match self.merge_cut_step(store, merge, &merging) {
      Ok(true) => Ok(MergeStep::More),
      Ok(false) => {
        self.merge_swap(store, &merging)?;
        Ok(MergeStep::More)
      }
      Err(refused) => {
        self.merge_given_back(store)?;
        Err(refused)
      }
    }
  }

  /// The next table of `merge`'s run, cut from the rows of the runs at `merging` past the last key it cut and put: held,
  /// its rows' holds taken when stored new, its fence charged. Whether one was cut (none: no row is left).
  fn merge_cut_step(
    &mut self,
    store: &mut ContentStore,
    merge: &mut Merge,
    merging: &[usize],
  ) -> Result<bool, IndexRefused> {
    let (cut, last) = {
      let mut sources: Vec<Source<'_>> = Vec::with_capacity(merging.len());
      for run in merging.iter().filter_map(|at| self.runs.get(*at)) {
        let rows: Box<dyn Iterator<Item = RowRef<'_>> + '_> = match &merge.after {
          Some(after) => {
            let after = after.clone();
            Box::new(
              run
                .rows_from(store, &after)
                .skip_while(move |row| row.key() <= after.as_ref()),
            )
          }
          None => Box::new(run.rows(store)),
        };
        sources.push(rows.peekable());
      }
      one_table(Merged { sources }, store.window())?
    };
    let Some(cut) = cut else {
      return Ok(false);
    };
    let hashes = table_hashes(&cut.bytes)?;
    // The merged run's key filter is charged with its first table.
    let filter_bytes = if self.merge_work.fences.is_empty() {
      self.merge_work.filter.bytes()
    } else {
      0
    };
    let fence = fences_charge(std::slice::from_ref(&cut))?.saturating_add(filter_bytes);
    let stored = store.put_tables(&[cut.bytes.as_slice()], 0, fence)?;
    let put = stored.first().ok_or(IndexRefused::LayerMissing)?;
    if put.fresh
      && let Err(refused) = table_rows_held(store, &cut.bytes, true)
    {
      // Its rows' holds were not taken, so only the table's own hold and its fence's charge are given back.
      give_back(store, &[put.identity])?;
      store.discharge(fence)?;
      return Err(refused);
    }
    let work = &mut self.merge_work;
    note_all(&mut work.filter, &hashes);
    Run::push_fence(
      &mut work.fences,
      &mut work.prefixes,
      &mut work.fence_keys,
      put.identity,
      &cut.first,
      cut.rows,
    );
    work.rows = work.rows.saturating_add(cut.rows);
    work.bytes = work
      .bytes
      .saturating_add(u64::try_from(cut.bytes.len()).unwrap_or(u64::MAX));
    work.charged = work.charged.saturating_add(fence);
    work.deduplicated = work.deduplicated.saturating_add(u64::from(!put.fresh));
    merge.after = last;
    merge.tables_cut = merge.tables_cut.saturating_add(1);
    Ok(true)
  }

  /// Swaps the merged run, its tables all put, in for the runs at `merging`: their fences' charge given back now and
  /// their tables kept to give back a step each ([`Index::merge_give_back_step`]).
  fn merge_swap(
    &mut self,
    store: &mut ContentStore,
    merging: &[usize],
  ) -> Result<(), IndexRefused> {
    let work = take(&mut self.merge_work);
    let taken = self.take_runs(merging);
    let charged = taken
      .iter()
      .fold(0_u64, |charged, run| charged.saturating_add(run.charged));
    let tables: Vec<ContentId> = taken
      .iter()
      .flat_map(|run| run.fences.iter().map(|fence| fence.table))
      .collect();
    let built = u64::try_from(work.fences.len()).unwrap_or(u64::MAX);
    let run = Run {
      fences: work.fences,
      prefixes: work.prefixes,
      fence_keys: work.fence_keys,
      rows: work.rows,
      bytes: work.bytes,
      charged: work.charged,
      shared: work.deduplicated > 0,
      filter: work.filter,
    };
    let rows = self.admit_run(run, built, work.deduplicated);
    self.merge_work = MergeWork {
      begun: work.begun,
      admitted: Some((rows, tables)),
      ..MergeWork::default()
    };
    self.counters.compactions = self.counters.compactions.saturating_add(1);
    self.counters.runs_merged = self
      .counters
      .runs_merged
      .saturating_add(u64::try_from(merging.len()).unwrap_or(u64::MAX));
    self.compacted_rows = rows;
    self.restart();
    store.discharge(charged).map_err(IndexRefused::from)
  }

  /// Gives back one table of the runs a merge swapped out, the last answering `Done` with the merged rows.
  fn merge_give_back_step(&mut self, store: &mut ContentStore) -> Result<MergeStep, IndexRefused> {
    let Some((rows, tables)) = self.merge_work.admitted.as_mut() else {
      return Ok(MergeStep::GivenUp);
    };
    let rows = *rows;
    let Some(table) = tables.pop() else {
      self.merge_work = MergeWork::default();
      return Ok(MergeStep::Done(rows));
    };
    let given_back = release_table(store, &table);
    if tables.is_empty() {
      self.merge_work = MergeWork::default();
      return given_back.map(|()| MergeStep::Done(rows));
    }
    given_back.map(|()| MergeStep::More)
  }

  /// Gives back whatever the merge under way holds ([`MergeWork`]): the tables it put, their rows' holds and their
  /// fences' charge; or, its run swapped in, the merged runs' tables left. It goes on past a refusal and answers the
  /// first.
  fn merge_given_back(&mut self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    let work = take(&mut self.merge_work);
    let mut first = store.discharge(work.charged).map_err(IndexRefused::from);
    for fence in &work.fences {
      first = first.and(release_table(store, &fence.table));
    }
    if let Some((_, tables)) = &work.admitted {
      for table in tables {
        first = first.and(release_table(store, table));
      }
    }
    first
  }

  /// Where `merge`'s runs are among this index's, each still its own: none when one is gone or shared.
  fn merging_positions(&self, merge: &Merge) -> Option<Vec<usize>> {
    merge
      .runs
      .iter()
      .map(|tables| {
        self.runs.iter().position(|run| {
          !run.shared
            && run.fences.len() == tables.len()
            && run
              .fences
              .iter()
              .zip(tables)
              .all(|(fence, table)| fence.table == *table)
        })
      })
      .collect()
  }

  /// Takes the runs at `positions` out of the index, keeping the rest in order.
  fn take_runs(&mut self, positions: &[usize]) -> Vec<Run> {
    let mut taken = Vec::with_capacity(positions.len());
    for (at, run) in take(&mut self.runs).into_iter().enumerate() {
      if positions.contains(&at) {
        taken.push(run);
      } else {
        self.runs.push(run);
      }
    }
    taken
  }

  /// A fork: the whole tail sealed, every table held once more and every run marked shared on both sides, the fences
  /// copied and charged, an empty tail of its own. Refused after its seal, it leaves the tail sealed, which no read can
  /// tell, and nothing else changed.
  pub fn fork(&mut self, store: &mut ContentStore) -> Result<Index, IndexRefused> {
    self.seal_rows(store, false)?;
    let copies: Vec<Run> = self.runs.iter().map(Run::fork_copy).collect();
    let fences = copies
      .iter()
      .try_fold(0_u64, |charged, run| charged.checked_add(run.charged))
      .ok_or(TableRefused::TooLarge)?;
    store.charge(fences)?;
    let tables: Vec<ContentId> = self.tables().collect();
    if let Err(refused) = hold_all(store, &tables) {
      store.discharge(fences)?;
      return Err(refused);
    }
    for run in &mut self.runs {
      run.shared = true;
    }
    self.restart();
    Ok(Index {
      runs: copies,
      rule: self.rule.clone(),
      forked: Some((self.runs.len(), self.keys_added)),
      ..Index::default()
    })
  }

  /// Derived: the most a fork of this index could charge the store as it is made ([`Index::fork`]): its tail sealed into
  /// tables (the rows' bytes, a frame and a fence a table, held beside the tail they replace until it is given back) and
  /// every run's fences copied.
  pub fn fork_room(&self) -> u64 {
    let fences = self.runs.iter().fold(0_u64, |charged, run| {
      charged.saturating_add(run.charged.saturating_sub(run.filter.bytes()))
    });
    let tail = self
      .tail_bytes
      .saturating_add(TABLE_FRAME_BYTES)
      .saturating_add(FENCE_SLOT_BYTES)
      .saturating_mul(2);
    fences.saturating_add(u64::try_from(tail).unwrap_or(u64::MAX))
  }

  /// Whether this index would adopt `fork` now ([`Index::adopt`]): it is a fork, and this index took in no row since it
  /// was made.
  pub fn adoptable(&self, fork: &Index) -> bool {
    fork.forked.is_some_and(|(inherited, keys_added)| {
      keys_added == self.keys_added && inherited <= fork.runs.len()
    })
  }

  /// Takes in `fork`, a fork of this index, whole (A-722: a push's index rows staged in its own fork, then made the
  /// repository's as the push lands): the fork's tail sealed into tables of its own, its holds on the runs it took from
  /// this index given back, and its own runs added as this index's newest, holding what they held. It copies no row and
  /// writes no table but its tail's, so it costs the fork's runs and one window of rows at most. Refused, with the
  /// fork handed back, when this index took in any row since the fork was made (the fork's rows are to be checked
  /// against it first) or the fork was made of no index; a refusal to seal leaves the tail sealed as far as it went.
  pub fn adopt(&mut self, store: &mut ContentStore, mut fork: Index) -> Result<(), AdoptRefused> {
    let Some((inherited, keys_added)) = fork.forked else {
      return Err(AdoptRefused::NotAFork(Box::new(fork)));
    };
    if keys_added != self.keys_added || inherited > fork.runs.len() {
      return Err(AdoptRefused::Stale(Box::new(fork)));
    }
    // A merge of the fork's own left part-way is given back: the fork's runs are what is adopted.
    if let Err(refused) = fork.merge_given_back(store) {
      return Err(AdoptRefused::Refused(refused, Box::new(fork)));
    }
    if let Err(refused) = fork.seal_rows(store, false) {
      return Err(AdoptRefused::Refused(refused, Box::new(fork)));
    }
    let own = fork.runs.split_off(inherited);
    for run in &fork.runs {
      // The parent holds each of these tables still: this gives back the fork's own hold and its fences' charge.
      let _ = release_run(store, run);
    }
    let added: u64 = own.iter().map(|run| run.rows).sum();
    self.runs.extend(own);
    self.keys_added = self.keys_added.saturating_add(added.max(1));
    self.relearn(store);
    self.restart();
    Ok(())
  }

  /// Gives back everything the index holds: its tail rows' holds and charge, and its runs, each table freed giving back
  /// its rows'. It goes on past a refusal and answers the first.
  pub fn release(mut self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    let tail = take(&mut self.tail);
    let mut first = release_rows(store, tail_rows(&tail));
    first = first.and(
      store
        .discharge(self.tail_charged)
        .map_err(IndexRefused::from),
    );
    for run in &self.runs {
      first = first.and(release_run(store, run));
    }
    first.and(self.merge_given_back(store))
  }

  /// Every row, in key order.
  pub fn iter<'s>(&'s self, store: &'s ContentStore) -> impl Iterator<Item = RowRef<'s>> + 's {
    let tail: Box<dyn Iterator<Item = RowRef<'s>> + 's> = Box::new(tail_rows(&self.tail));
    let mut sources: Vec<Source<'s>> = vec![tail.peekable()];
    for run in &self.runs {
      let rows: Box<dyn Iterator<Item = RowRef<'s>> + 's> = Box::new(run.rows(store));
      sources.push(rows.peekable());
    }
    Merged { sources }
  }

  /// Every row whose key is not before `start`, in key order, so the rows whose keys begin with a prefix are the run
  /// from it (A-640 part 4: an abbreviated object name's objects): the tail's from `start` on and each run's from the
  /// table that may hold `start`, merged as [`Index::iter`] merges them. Reading a run costs one search of its fences
  /// and one of its first table's rows, then a row a step.
  pub fn iter_from<'s>(
    &'s self,
    store: &'s ContentStore,
    start: &[u8],
  ) -> impl Iterator<Item = RowRef<'s>> + use<'s> {
    let tail: Box<dyn Iterator<Item = RowRef<'s>> + 's> = Box::new(
      self
        .tail
        .range::<[u8], _>((Bound::Included(start), Bound::Unbounded))
        .filter_map(|(key, body)| RowRef::new(key, body)),
    );
    let mut sources: Vec<Source<'s>> = vec![tail.peekable()];
    for run in &self.runs {
      let rows: Box<dyn Iterator<Item = RowRef<'s>> + 's> = Box::new(run.rows_from(store, start));
      sources.push(rows.peekable());
    }
    Merged { sources }
  }

  /// The rows the index holds.
  pub fn rows(&self) -> u64 {
    let tail = u64::try_from(self.tail.len()).unwrap_or(u64::MAX);
    self
      .runs
      .iter()
      .fold(tail, |rows, run| rows.saturating_add(run.rows))
  }

  /// The bytes the index holds in `store`: each table's stored length and what its tail charged, shared tables counted
  /// whole (a fork's are its parent's too): what a gauge of an index's size reads (goal 93's `vcs.hg.push_rows_bytes`).
  pub fn held_bytes(&self, store: &ContentStore) -> u64 {
    self
      .tables()
      .filter_map(|table| store.len_of(&table))
      .fold(self.tail_charged, u64::saturating_add)
  }

  /// The rows in the tail.
  pub fn tail_len(&self) -> usize {
    self.tail.len()
  }

  /// Every table's identity, the oldest run's first, each run's in key order.
  pub fn tables(&self) -> impl Iterator<Item = ContentId> + '_ {
    self
      .runs
      .iter()
      .flat_map(|run| run.fences.iter().map(|fence| fence.table))
  }

  /// Each run's tables, the oldest run first.
  pub fn run_tables(&self) -> Vec<Vec<ContentId>> {
    self
      .runs
      .iter()
      .map(|run| run.fences.iter().map(|fence| fence.table).collect())
      .collect()
  }

  /// The tail and runs the lookups since the merge rule's last report visited ([`Index::lookups_took`] takes them).
  pub fn visits(&self) -> u64 {
    self.visits.get()
  }

  /// What the index has done.
  pub fn counters(&self) -> IndexCounters {
    IndexCounters {
      lookups: self.lookups.get(),
      runs_visited: self.runs_visited.get(),
      resolves: self.resolves.get(),
      filter_rejections: self.filter_rejections.get(),
      ..self.counters
    }
  }

  /// The fences of every run: what a fork copies ([`Index::fork`]).
  pub fn fences(&self) -> u64 {
    self.runs.iter().fold(0_u64, |fences, run| {
      fences.saturating_add(u64::try_from(run.fences.len()).unwrap_or(u64::MAX))
    })
  }

  /// The caller's lookups since its last report took `elapsed_ns`: the merge rule's rent.
  pub fn lookups_took(&mut self, elapsed_ns: u64) {
    let visits = self.visits.replace(0);
    let zero = self.depth_zero.replace(0);
    let mut depths = take(self.depths.get_mut());
    if zero > 0 {
      if depths.is_empty() {
        depths.push(zero);
      } else if let Some(slot) = depths.first_mut() {
        *slot = slot.saturating_add(zero);
      }
    }
    self.rule.lookups(elapsed_ns, visits, &depths);
  }

  /// The caller's inserts since its last report took `elapsed_ns`: the rule's first price.
  pub fn inserts_took(&mut self, elapsed_ns: u64) {
    let rows = take(&mut self.inserted);
    self.rule.built(elapsed_ns, rows);
  }

  /// The last merge took `elapsed_ns`: the rule's price from now on.
  pub fn compaction_took(&mut self, elapsed_ns: u64) {
    let rows = take(&mut self.compacted_rows);
    self.rule.compacted(elapsed_ns, rows);
  }

  /// The merge rule's state.
  pub fn rule(&self) -> &Compaction {
    &self.rule
  }
}
