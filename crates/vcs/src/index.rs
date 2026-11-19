//! A repository's index (§4.17.1 "Objects" item 3): from its tool's id to the row it keeps for each object, held as
//! immutable sorted tables that are content plus a mutable tail, so a fork shares its parent's tables by identity and
//! adds only rows for its own objects.
//!
//! **Layers.** A row goes into the tail, an ordered map. A fold writes the tail as one table ([`crate::table`]) and
//! stores its bytes as content; a compaction merges the tail and every table only this index holds into one. A lookup
//! reads the tail, then the tables from the newest, and stops at the first that has the key. An id names one object
//! for ever, so a key is in one layer at most and a row is never rewritten: an insert of a key the index holds is
//! answered `Identical` when its row is the same and refused `RowDiffers` when it is not.
//!
//! **A fork** folds its parent's tail, holds each of the parent's tables once more and starts with an empty tail: it
//! writes no table and copies no row ([`IndexCounters`] are the non-vacuity counters the tests read). The tables it
//! shares are never merged by either side, which would copy the shared rows; each side's own rows are merged among
//! themselves.
//!
//! **Holds** (the crate's docs, "What holds what"). Each tail row holds the contents it names once. A table stored new
//! takes over its rows' holds, and a table whose bytes were stored already (another index wrote the same rows) gives
//! them back, since the stored copy holds them; a table freed with its last holder gives back its rows'.
//!
//! **Bounds.** A tail row is charged to the store's budget for its key, its body and its slot ([`TAIL_SLOT_BYTES`]); a
//! table's bytes are content in the same budget. An insert past the budget is refused with nothing changed, and a
//! batch ([`Index::insert_all`]) is checked whole, its order, its rows and its room, before any of it is applied. When
//! to compact is the learned rule of [`crate::rule`]: the index counts each lookup's probes, and the caller reports
//! the times it measured.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::iter::Peekable;
use std::mem::{size_of, take};

use crate::content::{ContentId, ContentRefused, ContentStore, Stored};
use crate::rule::Compaction;
use crate::table::{RowRef, TableBuilder, TableRefused, TableView, encode_body};

/// Shape: what one tail row costs the budget beyond its key's and its body's bytes: its boxed key and boxed body, the
/// slot each takes in the tail's ordered map. Derived from the types; a lower bound, since a map's nodes keep spare
/// slots.
pub const TAIL_SLOT_BYTES: usize = size_of::<(Box<[u8]>, Box<[u8]>)>();

/// Why the index refused a request. Each leaves the index and the store as they were, unless it says otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexRefused {
  /// The index holds a row of that key with another body: an id names one object for ever.
  RowDiffers,
  /// A batch's keys do not strictly increase.
  Unsorted,
  /// The store refused: a content the row names is not held, or the budget has no room.
  Content(ContentRefused),
  /// A row broke the table format.
  Table(TableRefused),
  /// A layer's table is not in the store or does not open: the index and its store disagree. A release that meets it
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
  /// Tables written by a fold or a compaction.
  pub tables_built: u64,
  /// Rows written into tables.
  pub rows_written: u64,
  /// Tables whose bytes the store held already.
  pub tables_deduplicated: u64,
  /// Compactions that merged two layers or more.
  pub compactions: u64,
}

/// One table of the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layer {
  /// The table's identity in the store.
  table: ContentId,
  /// Its rows.
  rows: u64,
  /// Whether another index may hold it too (set at a fork or when its bytes were stored already; learned again from
  /// the store's count at each compaction).
  shared: bool,
}

/// What one lookup probed.
#[derive(Debug, Default)]
struct Tally {
  probes: u64,
  compactable: u64,
}

impl Tally {
  fn probe(&mut self, compactable: bool) {
    self.probes = self.probes.saturating_add(1);
    if compactable {
      self.compactable = self.compactable.saturating_add(1);
    }
  }
}

/// A repository's index (module docs).
#[derive(Debug, Default)]
pub struct Index {
  /// The tables, oldest first.
  layers: Vec<Layer>,
  tail: BTreeMap<Box<[u8]>, Box<[u8]>>,
  /// The bytes the tail charged to the store.
  tail_charged: u64,
  /// Probes since the caller last reported its lookups' time.
  probes: Cell<u64>,
  /// Of those, the probes into compactable layers beyond each lookup's first.
  excess: Cell<u64>,
  /// Rows inserted since the caller last reported its inserts' time.
  inserted: u64,
  /// The rows of the last compaction, until the caller reports its time.
  compacted_rows: u64,
  rule: Compaction,
  counters: IndexCounters,
}

/// A layer's table, opened.
fn view<'s>(store: &'s ContentStore, layer: &Layer) -> Option<TableView<'s>> {
  TableView::open_known(store.get(&layer.table)?).ok()
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

/// Gives back this index's hold of a layer's table; a table freed with it gives back its rows' holds.
fn release_table(store: &mut ContentStore, layer: &Layer) -> Result<(), IndexRefused> {
  let Some(freed) = store.release(&layer.table)? else {
    return Ok(());
  };
  let view = TableView::open_known(&freed).map_err(|_| IndexRefused::LayerMissing)?;
  release_rows(store, view.iter())
}

/// The tail's rows, in key order.
fn tail_rows(tail: &BTreeMap<Box<[u8]>, Box<[u8]>>) -> impl Iterator<Item = RowRef<'_>> {
  tail.iter().filter_map(|(key, body)| RowRef::new(key, body))
}

/// A row of a batch as planned: its body and its charge when it is new, `None` when the index holds it.
type Planned = Option<(Vec<u8>, u64)>;

/// One source of rows in key order, boxed so that tails and tables merge alike.
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

  /// The row of `key`, with the probes it took counted for the compaction rule.
  pub fn get<'s>(&'s self, store: &'s ContentStore, key: &[u8]) -> Option<RowRef<'s>> {
    let mut tally = Tally::default();
    let found = self.find(store, key, &mut tally);
    self
      .probes
      .set(self.probes.get().saturating_add(tally.probes));
    self.excess.set(
      self
        .excess
        .get()
        .saturating_add(tally.compactable.saturating_sub(1)),
    );
    found
  }

  /// The tail, then the tables from the newest, to the first that has `key`. A table missing from the store is
  /// skipped (the release that would meet it says so).
  fn find<'s>(
    &'s self,
    store: &'s ContentStore,
    key: &[u8],
    tally: &mut Tally,
  ) -> Option<RowRef<'s>> {
    if !self.tail.is_empty() {
      tally.probe(true);
      if let Some((stored, body)) = self.tail.get_key_value(key) {
        return RowRef::new(stored, body);
      }
    }
    self.layers.iter().rev().find_map(|layer| {
      tally.probe(!layer.shared);
      view(store, layer)?.get(key)
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

  /// One row against the index: `None` when it holds the same row; the new row's body when it holds none of the key
  /// and the store holds every content the row names.
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
    hold_all(store, row.contents)?;
    if let Err(refused) = store.charge(charge) {
      give_back(store, row.contents)?;
      return Err(refused.into());
    }
    self
      .tail
      .insert(Box::from(row.key), body.into_boxed_slice());
    self.tail_charged = self.tail_charged.saturating_add(charge);
    self.inserted = self.inserted.saturating_add(1);
    Ok(())
  }

  /// Writes the tail as one table; the rows written.
  pub fn fold(&mut self, store: &mut ContentStore) -> Result<u64, IndexRefused> {
    if self.tail.is_empty() {
      return Ok(0);
    }
    let mut builder = TableBuilder::new();
    for (key, body) in &self.tail {
      builder.push(key, body)?;
    }
    let rows = builder.rows();
    let bytes = builder.finish()?;
    let stored = store.store_in_place_of(&bytes, self.tail_charged)?;
    self.tail_charged = 0;
    let tail = take(&mut self.tail);
    let given_back = if stored.fresh {
      Ok(())
    } else {
      release_rows(store, tail_rows(&tail))
    };
    self.layer_made(stored, rows);
    given_back.map(|()| rows)
  }

  /// Records a table a fold or a compaction made.
  fn layer_made(&mut self, stored: Stored, rows: u64) {
    self.layers.push(Layer {
      table: stored.identity,
      rows,
      shared: !stored.fresh,
    });
    self.counters.tables_built = self.counters.tables_built.saturating_add(1);
    self.counters.rows_written = self.counters.rows_written.saturating_add(rows);
    if !stored.fresh {
      self.counters.tables_deduplicated = self.counters.tables_deduplicated.saturating_add(1);
    }
  }

  /// Merges the tail and every table only this index holds into one table, leaving the shared tables as they are; the
  /// rows written. With a single such layer there is nothing to merge: a tail alone is folded, a table alone is kept.
  pub fn compact(&mut self, store: &mut ContentStore) -> Result<u64, IndexRefused> {
    for layer in &mut self.layers {
      layer.shared = store.holders(&layer.table) > 1;
    }
    let merging: Vec<Layer> = self
      .layers
      .iter()
      .filter(|layer| !layer.shared)
      .copied()
      .collect();
    if merging.is_empty() {
      return self.fold(store);
    }
    if self.tail.is_empty() && merging.len() < 2 {
      return Ok(0);
    }
    let (stored, rows) = self.write_merged(store, &merging)?;
    let given_back = self.give_back_merged(store, &merging);
    self.layers.retain(|layer| layer.shared);
    self.layer_made(stored, rows);
    self.counters.compactions = self.counters.compactions.saturating_add(1);
    self.compacted_rows = rows;
    self.rule = self.rule.restarted();
    given_back.map(|()| rows)
  }

  /// Stores the merged table in place of the tail's charge; a table stored new takes its rows' holds, and one stored
  /// already holds them. Its holds are taken before the merged layers give theirs back, so no content is freed between.
  fn write_merged(
    &mut self,
    store: &mut ContentStore,
    merging: &[Layer],
  ) -> Result<(Stored, u64), IndexRefused> {
    let (bytes, rows) = self.merged_table(store, merging)?;
    let stored = store.store_in_place_of(&bytes, self.tail_charged)?;
    self.tail_charged = 0;
    if stored.fresh {
      hold_rows(store, TableView::open_known(&bytes)?.iter())?;
    }
    Ok((stored, rows))
  }

  /// Gives back what the merged layers held: the tail rows' holds, and this index's hold of each merged table.
  fn give_back_merged(
    &mut self,
    store: &mut ContentStore,
    merging: &[Layer],
  ) -> Result<(), IndexRefused> {
    let tail = take(&mut self.tail);
    let mut given_back = release_rows(store, tail_rows(&tail));
    for layer in merging {
      given_back = given_back.and(release_table(store, layer));
    }
    given_back
  }

  /// The tail and the `merging` tables as one table's bytes, and its rows.
  fn merged_table(
    &self,
    store: &ContentStore,
    merging: &[Layer],
  ) -> Result<(Vec<u8>, u64), IndexRefused> {
    let mut sources: Vec<Source<'_>> = Vec::with_capacity(merging.len().saturating_add(1));
    let tail: Box<dyn Iterator<Item = RowRef<'_>> + '_> = Box::new(tail_rows(&self.tail));
    sources.push(tail.peekable());
    for layer in merging {
      let rows: Box<dyn Iterator<Item = RowRef<'_>> + '_> =
        Box::new(view(store, layer).ok_or(IndexRefused::LayerMissing)?.iter());
      sources.push(rows.peekable());
    }
    let mut builder = TableBuilder::new();
    for row in (Merged { sources }) {
      builder.push(row.key(), row.body())?;
    }
    let rows = builder.rows();
    Ok((builder.finish()?, rows))
  }

  /// A fork: the tail folded, every table held once more and marked shared on both sides, an empty tail of its own.
  pub fn fork(&mut self, store: &mut ContentStore) -> Result<Index, IndexRefused> {
    self.fold(store)?;
    for (taken, layer) in self.layers.iter().enumerate() {
      if let Err(refused) = store.hold(&layer.table) {
        for held in self.layers.get(..taken).unwrap_or_default() {
          store.release(&held.table)?;
        }
        return Err(refused.into());
      }
    }
    for layer in &mut self.layers {
      layer.shared = true;
    }
    self.rule = self.rule.restarted();
    Ok(Index {
      layers: self.layers.clone(),
      rule: self.rule,
      ..Index::default()
    })
  }

  /// Gives back everything the index holds: its tail rows' holds and charge, and its tables, each freed one giving
  /// back its rows'. It goes on past a refusal and answers the first.
  pub fn release(mut self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    let tail = take(&mut self.tail);
    let mut first = release_rows(store, tail_rows(&tail));
    first = first.and(
      store
        .discharge(self.tail_charged)
        .map_err(IndexRefused::from),
    );
    for layer in &self.layers {
      first = first.and(release_table(store, layer));
    }
    first
  }

  /// Every row, in key order.
  pub fn iter<'s>(&'s self, store: &'s ContentStore) -> impl Iterator<Item = RowRef<'s>> + 's {
    let tail: Box<dyn Iterator<Item = RowRef<'s>> + 's> = Box::new(tail_rows(&self.tail));
    let mut sources: Vec<Source<'s>> = vec![tail.peekable()];
    for layer in &self.layers {
      if let Some(table) = view(store, layer) {
        let rows: Box<dyn Iterator<Item = RowRef<'s>> + 's> = Box::new(table.iter());
        sources.push(rows.peekable());
      }
    }
    Merged { sources }
  }

  /// The rows the index holds.
  pub fn rows(&self) -> u64 {
    let tail = u64::try_from(self.tail.len()).unwrap_or(u64::MAX);
    self
      .layers
      .iter()
      .fold(tail, |rows, layer| rows.saturating_add(layer.rows))
  }

  /// The rows in the tail.
  pub fn tail_len(&self) -> usize {
    self.tail.len()
  }

  /// The tables' identities, oldest first.
  pub fn tables(&self) -> impl Iterator<Item = ContentId> + '_ {
    self.layers.iter().map(|layer| layer.table)
  }

  /// What the index has done.
  pub fn counters(&self) -> IndexCounters {
    self.counters
  }

  /// The caller's lookups since its last report took `elapsed_ns`: the compaction rule's rent.
  pub fn lookups_took(&mut self, elapsed_ns: u64) {
    let probes = self.probes.replace(0);
    let excess = self.excess.replace(0);
    self.rule.lookups(elapsed_ns, probes, excess);
  }

  /// The caller's inserts since its last report took `elapsed_ns`: the rule's first price.
  pub fn inserts_took(&mut self, elapsed_ns: u64) {
    let rows = take(&mut self.inserted);
    self.rule.built(elapsed_ns, rows);
  }

  /// The last compaction took `elapsed_ns`: the rule's price from now on.
  pub fn compaction_took(&mut self, elapsed_ns: u64) {
    let rows = take(&mut self.compacted_rows);
    self.rule.compacted(elapsed_ns, rows);
  }

  /// Whether the rule says to compact now.
  pub fn compaction_due(&self) -> bool {
    let tail = u64::try_from(self.tail.len()).unwrap_or(u64::MAX);
    let rows = self
      .layers
      .iter()
      .filter(|layer| !layer.shared)
      .fold(tail, |rows, layer| rows.saturating_add(layer.rows));
    self.rule.due(rows)
  }

  /// The compaction rule's state.
  pub fn rule(&self) -> &Compaction {
    &self.rule
  }
}
