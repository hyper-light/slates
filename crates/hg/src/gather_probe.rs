//! The boot probe of a gathered row's cost (goal 93; main 01a20182's ruling of 2026-11-28 00:46): a stream's gathering
//! timed over a small history at the daemon's start, beside its memcpy rate, so a shard's learned row cost
//! (`render::RowCost`) begins from a measured first value. A first value derived from the cache lines a row moves was
//! about ten times low: a shard's first stream ran past its slices (p50 11,792 ns against a 2,428 ns slice, release, the
//! MacBook), until its first step was measured.
//!
//! The history is made in memory, sealed into tables as a landed repository's index is, and gathered as a shard's
//! streams gather, a step at a time within the step's copy budget, each step timed ([`GatherProbe::learn`]). A walk of
//! the whole history in one go was the first probe: about 1.2 µs a row in debug, while a stream's step of a row cost
//! about 27 µs, its restart of the walks (the reads that begin it) the most of it. So the probe learns as a shard
//! does, the same estimator ([`RowCost`]) over a few rounds, each round's rows charged what the rounds before measured
//! (the first at their held form), so the steps it times are the size a shard's are.

use slates_vcs::ContentStore;

use crate::index::{HgIndex, Log, NewChangeset, NewRevision};
use crate::render::{RowCost, StepCopy};
use crate::{NULL_NODE, Node, node};

/// Shape: the rounds [`GatherProbe::learn`] gathers its history in: the first at a row's held form, each after it at
/// what the rounds before measured, so its steps reach a shard's size within a few (the cumulative mean damps the
/// first round's long steps).
const LEARN_ROUNDS: usize = 4;

/// Shape: the probe history's changesets: enough rows (a changeset, its manifest and [`PROBE_FILES`] file revisions
/// each, about 400) that a gathering's per-row cost outweighs its start, few enough to make at every start in a few
/// milliseconds.
const PROBE_CHANGESETS: usize = 64;

/// Shape: the files each probe changeset changes, so the gathering walks file logs, the manifest log and the changelog
/// as a stream's does.
const PROBE_FILES: usize = 4;

/// Shape: the bytes of each probe text: a short file's, as most of a history's revisions are.
const PROBE_TEXT_BYTES: usize = 48;

/// A small history's index and its store, to time a gathering over ([`GatherProbe::learn`]).
#[derive(Debug)]
pub struct GatherProbe {
  index: HgIndex,
  store: ContentStore,
}

impl GatherProbe {
  /// The probe history made in a store of `window`-byte windows, sized as a landed history is, and sealed into its
  /// tables; none where the store or the index refuses (no room), which leaves the daemon's first row cost to its first
  /// measured step.
  pub fn new(window: usize) -> Option<GatherProbe> {
    // Derived: room for every log's revisions a window each, more than their texts and rows take.
    let logs = PROBE_FILES.checked_add(2)?;
    let room = u64::try_from(window.checked_mul(PROBE_CHANGESETS)?.checked_mul(logs)?).ok()?;
    let mut store = ContentStore::new(room, window).ok()?;
    let mut index = HgIndex::new();
    let mut heads = Heads {
      files: vec![NULL_NODE; PROBE_FILES],
      changeset: NULL_NODE,
      manifest: NULL_NODE,
    };
    for at in 0..PROBE_CHANGESETS {
      append_changeset(&mut index, &mut store, (at, &mut heads))?;
    }
    size_logs(&mut index, &mut store)?;
    index.rows().seal_all(&mut store).ok()?;
    Some(GatherProbe { index, store })
  }

  /// A row's cost learned as a shard's streams learn it ([`RowCost`]): the history gathered [`LEARN_ROUNDS`] times a
  /// step at a time within `copy` (its bytes, its line, its clock), each round's rows charged what the rounds before
  /// measured at the copy rate `bytes_per_second` (the first at their held form). None where no clock is given or a row
  /// does not decode, which a probe history it made never holds.
  pub fn learn(&self, copy: StepCopy, bytes_per_second: u64) -> Option<RowCost> {
    let mut cost = RowCost::default();
    for _ in 0..LEARN_ROUNDS {
      let row = cost.row_bytes(0, (bytes_per_second, copy.bytes));
      let stepped =
        crate::render::gathered_stepped(&self.index, &self.store, StepCopy { row, ..copy })?;
      cost.observe(stepped);
    }
    Some(cost)
  }
}

/// The probe history's heads as it grows: each file's last revision, the changeset's and the manifest's.
struct Heads {
  files: Vec<Node>,
  changeset: Node,
  manifest: Node,
}

/// Appends probe changeset `at` to `index`, with its manifest and a revision of each file, `heads` moved on.
fn append_changeset(
  index: &mut HgIndex,
  store: &mut ContentStore,
  (at, heads): (usize, &mut Heads),
) -> Option<()> {
  let (changeset, manifest, manifest_parent) =
    changeset_of(store, (at, heads.changeset, heads.manifest))?;
  index.append_changesets(store, &[changeset]).ok()?;
  heads.changeset = changeset.node;
  heads.manifest = manifest;
  let manifest_row = revision_of(
    store,
    (manifest, manifest_parent, changeset.node),
    &text_of(at, PROBE_FILES),
  )?;
  index
    .append_revisions(store, Log::Manifest, &[manifest_row])
    .ok()?;
  for (file, head) in heads.files.iter_mut().enumerate() {
    let text = text_of(at, file);
    let node = node::compute(head, &NULL_NODE, &text).ok()?;
    let row = revision_of(store, (node, *head, changeset.node), &text)?;
    let path = file_path(file);
    index
      .append_revisions(store, Log::File(path.as_bytes()), &[row])
      .ok()?;
    *head = node;
  }
  Some(())
}

/// Sizes every probe log's revisions as a landed history's are, each its text's length, so a gathering walks each
/// log's sized rows beside its log rows as a stream's does.
fn size_logs(index: &mut HgIndex, store: &mut ContentStore) -> Option<()> {
  let lengths = vec![u32::try_from(PROBE_TEXT_BYTES).ok()?; PROBE_CHANGESETS];
  let paths: Vec<String> = (0..PROBE_FILES).map(file_path).collect();
  let logs = [Log::Changelog, Log::Manifest]
    .into_iter()
    .chain(paths.iter().map(|path| Log::File(path.as_bytes())));
  for log in logs {
    let sizes = crate::index::ToSize {
      from: 0,
      lengths: &lengths,
      streamable: true,
    };
    index.keep_sized(store, log, sizes).ok()?;
  }
  Some(())
}

/// The path of probe file `file`.
fn file_path(file: usize) -> String {
  format!("probe-{file}")
}

/// Probe changeset `at` after `parent`, its manifest after `manifest`: the changeset to append, its manifest's node and
/// that node's parent.
fn changeset_of(
  store: &mut ContentStore,
  (at, parent, manifest): (usize, Node, Node),
) -> Option<(NewChangeset<'static>, Node, Node)> {
  let text = text_of(at, usize::MAX);
  let manifest_node = node::compute(&manifest, &NULL_NODE, &text_of(at, PROBE_FILES)).ok()?;
  let node = node::compute(&parent, &NULL_NODE, &text).ok()?;
  let changeset = NewChangeset {
    node,
    p1: parent,
    p2: NULL_NODE,
    flags: 0,
    len: u64::try_from(text.len()).ok()?,
    text: store.put_object(&text).ok()?,
    manifest: manifest_node,
    closed: false,
    branch: b"default",
  };
  Some((changeset, manifest_node, manifest))
}

/// A revision of `node` after `p1`, linked to changeset `link`, its text `text` held in `store`.
fn revision_of(
  store: &mut ContentStore,
  (node, p1, link): (Node, Node, Node),
  text: &[u8],
) -> Option<NewRevision> {
  Some(NewRevision {
    node,
    p1,
    p2: NULL_NODE,
    link,
    flags: 0,
    len: u64::try_from(text.len()).ok()?,
    bytes: store.put_object(text).ok()?,
    block: None,
  })
}

/// The probe text of changeset `at`'s log `log` ([`PROBE_TEXT_BYTES`] bytes, each history's own).
fn text_of(at: usize, log: usize) -> Vec<u8> {
  let mut text = format!("probe changeset {at} log {log}\n").into_bytes();
  text.resize(PROBE_TEXT_BYTES.max(text.len()), b'.');
  text
}
