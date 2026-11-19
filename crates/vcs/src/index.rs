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
//! and cuts them into one run; which `j`, and when, is the learned rule of [`crate::rule`]: the index counts, for each
//! lookup, how many runs it visited and how many of those it alone holds, and the caller reports the times it
//! measured. Runs a fork shares are never merged, since merging them would copy rows the fork still shares; a run is
//! read again as this index's own once no other index holds its tables (a fork released). Report the lookups' time
//! before a seal or a merge: lookups not yet timed when the runs change are dropped, since they were counted against
//! runs that are gone.
//!
//! **A fork** seals its parent's whole tail, holds each of the parent's tables once more, copies the runs' fences and
//! starts with an empty tail: it writes no table and copies no row ([`IndexCounters`] are the non-vacuity counters the
//! tests read).
//!
//! **Holds** (the crate's docs, "What holds what"). Each tail row holds the contents it names once. A table stored new
//! takes over its rows' holds, and a table whose bytes were stored already (another index wrote the same rows) gives
//! them back, since the stored copy holds them; a table freed with its last holder gives back its rows'.
//!
//! **Bounds.** Every table is at most a window, and a row too long for a table of its own is refused
//! ([`IndexRefused::RowPastWindow`]). A tail row is charged to the store's budget for its key, its body and its slot
//! ([`TAIL_SLOT_BYTES`]), a run's fences for their keys and their slots ([`FENCE_SLOT_BYTES`]), and a table's bytes are
//! content in the same budget. An insert past the budget is refused with nothing changed; a batch
//! ([`Index::insert_all`]) is checked whole, its order, its rows and its room, before any of it is applied; a seal's
//! and a merge's tables are admitted as one step ([`ContentStore::put_tables`]).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::iter::Peekable;
use std::mem::{size_of, take};

use crate::content::{ContentId, ContentRefused, ContentStore, Stored};
use crate::rule::Compaction;
use crate::table::{
  RowRef, TABLE_FRAME_BYTES, TableBuilder, TableRefused, TableView, encode_body, row_bytes,
};

/// Shape: what one tail row costs the budget beyond its key's and its body's bytes: its boxed key and boxed body, the
/// slot each takes in the tail's ordered map. Derived from the types; a lower bound, since a map's nodes keep spare
/// slots.
pub const TAIL_SLOT_BYTES: usize = size_of::<(Box<[u8]>, Box<[u8]>)>();

/// Shape: what one table of a run costs the budget beside the store beyond its first key's bytes: its fence's record.
/// Derived from the type.
pub const FENCE_SLOT_BYTES: usize = size_of::<Fence>();

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
}

/// One table of a run: its identity, its first key (the fence a lookup searches) and its rows.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fence {
  table: ContentId,
  first: Box<[u8]>,
  rows: u64,
}

/// A run (module docs): its tables in key order, their rows, what their fences charged the store, and whether another
/// index may hold its tables too (set at a fork or when a table's bytes were stored already; read again from the
/// store's counts before each merge).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
  fences: Vec<Fence>,
  rows: u64,
  charged: u64,
  shared: bool,
}

impl Run {
  /// The table that may hold `key`: the last whose first key is not after it.
  fn fence_for(&self, key: &[u8]) -> Option<&Fence> {
    let after = self
      .fences
      .partition_point(|fence| fence.first.as_ref() <= key);
    self.fences.get(after.checked_sub(1)?)
  }

  /// Every row, in key order.
  fn rows<'s>(&'s self, store: &'s ContentStore) -> impl Iterator<Item = RowRef<'s>> + 's {
    self
      .fences
      .iter()
      .filter_map(move |fence| view(store, &fence.table))
      .flat_map(|table| table.iter())
  }
}

/// What one lookup visited: the tail and the runs, and of those runs the ones only this index holds.
#[derive(Debug, Default)]
struct Visit {
  visits: u64,
  compactable: u64,
}

impl Visit {
  fn visit(&mut self, compactable: bool) {
    self.visits = self.visits.saturating_add(1);
    if compactable {
      self.compactable = self.compactable.saturating_add(1);
    }
  }
}

/// Rows cut into one table: its bytes, its first key and its rows.
#[derive(Debug)]
struct Cut {
  bytes: Vec<u8>,
  first: Box<[u8]>,
  rows: u64,
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
  /// `depths[p]`: the lookups since then that visited `p` runs only this index holds.
  depths: RefCell<Vec<u64>>,
  /// Rows inserted since the caller last reported its inserts' time.
  inserted: u64,
  /// The rows of the last merge, until the caller reports its time.
  compacted_rows: u64,
  rule: Compaction,
  counters: IndexCounters,
}

/// A table, opened.
fn view<'s>(store: &'s ContentStore, table: &ContentId) -> Option<TableView<'s>> {
  TableView::open_known(store.get(table)?).ok()
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

/// Takes the holds `rows` take of their contents.
fn hold_rows<'a>(
  store: &mut ContentStore,
  rows: impl Iterator<Item = RowRef<'a>>,
) -> Result<(), IndexRefused> {
  for row in rows {
    for content in row.contents() {
      store.hold(&content)?;
    }
  }
  Ok(())
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
  let Some(freed) = store.release(table)? else {
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
fn run_of(cuts: Vec<Cut>, stored: &[Stored], charged: u64) -> Run {
  let rows = cut_rows(&cuts);
  let shared = stored.iter().any(|put| !put.fresh);
  let fences = cuts
    .into_iter()
    .zip(stored)
    .map(|(cut, put)| Fence {
      table: put.identity,
      first: cut.first,
      rows: cut.rows,
    })
    .collect();
  Run {
    fences,
    rows,
    charged,
    shared,
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
    if let Ok(mut depths) = self.depths.try_borrow_mut() {
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
    self.runs.iter().rev().find_map(|run| {
      visit.visit(!run.shared);
      let fence = run.fence_for(key)?;
      view(store, &fence.table)?.get(key)
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
    self.seal_rows(store, false)
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
    let fences = fences_charge(&cuts)?;
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
    let rows = self.add_run(cuts, &stored, fences);
    self.counters.seals = self.counters.seals.saturating_add(1);
    given_back.map(|()| rows)
  }

  /// Records a run a seal or a merge wrote, on top; its rows.
  fn add_run(&mut self, cuts: Vec<Cut>, stored: &[Stored], charged: u64) -> u64 {
    let tables = u64::try_from(cuts.len()).unwrap_or(u64::MAX);
    let deduplicated = stored.iter().filter(|put| !put.fresh).count();
    let run = run_of(cuts, stored, charged);
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
      .saturating_add(u64::try_from(deduplicated).unwrap_or(u64::MAX));
    rows
  }

  /// The runs only this index holds.
  fn compactable(&self) -> usize {
    self.runs.iter().filter(|run| !run.shared).count()
  }

  /// Drops the lookups not yet timed: they were counted against runs that changed (module docs).
  fn drop_untimed(&mut self) {
    self.visits.set(0);
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

  /// Merges the newest `runs` runs only this index holds into one run (module docs); the rows written. Fewer than two
  /// such runs merge nothing. The merged run's tables are admitted as one step and take their rows' holds before the
  /// merged runs give theirs back: refused, nothing changed.
  pub fn compact(&mut self, store: &mut ContentStore, runs: usize) -> Result<u64, IndexRefused> {
    self.relearn(store);
    let merging: Vec<usize> = (0..self.runs.len())
      .rev()
      .filter(|at| self.runs.get(*at).is_some_and(|run| !run.shared))
      .take(runs)
      .collect();
    if merging.len() < 2 {
      return Ok(0);
    }
    self.merge_fits(store, &merging)?;
    let cuts = self.merged_cuts(store, &merging)?;
    let fences = fences_charge(&cuts)?;
    let tables: Vec<&[u8]> = cuts.iter().map(|cut| cut.bytes.as_slice()).collect();
    let stored = store.put_tables(&tables, 0, fences)?;
    let mut given_back = Ok(());
    for (cut, put) in cuts.iter().zip(&stored) {
      if put.fresh {
        given_back = given_back.and(table_rows_held(store, &cut.bytes, true));
      }
    }
    for run in self.take_runs(&merging) {
      given_back = given_back.and(release_run(store, &run));
    }
    let rows = self.add_run(cuts, &stored, fences);
    self.counters.compactions = self.counters.compactions.saturating_add(1);
    self.counters.runs_merged = self
      .counters
      .runs_merged
      .saturating_add(u64::try_from(merging.len()).unwrap_or(u64::MAX));
    self.compacted_rows = rows;
    self.restart();
    given_back.map(|()| rows)
  }

  /// Refuses a merge whose runs' tables alone could not fit the room, before any of their rows is read: the merged run
  /// holds the same rows, so this is what its tables come to, and the exact check is the store's.
  fn merge_fits(&self, store: &ContentStore, merging: &[usize]) -> Result<(), IndexRefused> {
    let mut need: u64 = 0;
    for run in merging.iter().filter_map(|at| self.runs.get(*at)) {
      for fence in &run.fences {
        let bytes = store
          .get(&fence.table)
          .ok_or(IndexRefused::LayerMissing)?;
        let bytes = u64::try_from(bytes.len()).map_err(|_| TableRefused::TooLarge)?;
        need = need.checked_add(bytes).ok_or(TableRefused::TooLarge)?;
      }
    }
    let room = store.room();
    if need > room {
      return Err(ContentRefused::OverBudget { need, room }.into());
    }
    Ok(())
  }

  /// The rows of the `merging` runs, in key order, cut into tables of at most a window.
  fn merged_cuts(&self, store: &ContentStore, merging: &[usize]) -> Result<Vec<Cut>, IndexRefused> {
    let mut sources: Vec<Source<'_>> = Vec::with_capacity(merging.len());
    for run in merging.iter().filter_map(|at| self.runs.get(*at)) {
      let rows: Box<dyn Iterator<Item = RowRef<'_>> + '_> = Box::new(run.rows(store));
      sources.push(rows.peekable());
    }
    cut(Merged { sources }, store.window())
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
    let fences = self
      .runs
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
      runs: self.runs.clone(),
      rule: self.rule.clone(),
      ..Index::default()
    })
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
    first
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

  /// The rows the index holds.
  pub fn rows(&self) -> u64 {
    let tail = u64::try_from(self.tail.len()).unwrap_or(u64::MAX);
    self
      .runs
      .iter()
      .fold(tail, |rows, run| rows.saturating_add(run.rows))
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

  /// What the index has done.
  pub fn counters(&self) -> IndexCounters {
    self.counters
  }

  /// The caller's lookups since its last report took `elapsed_ns`: the merge rule's rent.
  pub fn lookups_took(&mut self, elapsed_ns: u64) {
    let visits = self.visits.replace(0);
    let depths = take(self.depths.get_mut());
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
