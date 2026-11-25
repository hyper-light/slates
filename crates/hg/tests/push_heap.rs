//! A push's allocation churn on the system allocator's path (goal 93, the 410 MiB bar; main's order of 2026-11-23 on CI
//! 45127535742: 57.3 GB allocated over the hg matrix's run, which breeds the 1.26 GB the allocator keeps after push-all):
//! its own binary, so the slates binary's allocator ([`LargeMapped`]) is the global one and its figures read what a
//! push's intake allocates per changeset, as `crates/svn/tests/import_heap.rs` reads an import's.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::indexing_slicing,
  clippy::arithmetic_side_effects,
  clippy::cast_precision_loss
)]

use slates_hg::changegroup::{Changegroup, Group, Revision, Version};
use slates_hg::index::{HgIndex, Log};
use slates_hg::manifest::{self, Entry, Flag};
use slates_hg::push::intake::{Intake, PushSegment, Stepped};
use slates_hg::serve::getbundle::Store;
use slates_hg::serve::indexed::{Indexed, State};
use slates_hg::{Admission, NULL_NODE, Node, node};
use slates_mem::large::{LargeMapped, heap};
use std::borrow::Cow;

use slates_vcs::{ContentRefused, ContentStore, StoredWindow, WindowStorage};

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Held by each test while it runs: the heap's figures are the process's, so a test that allocates while another reads
/// them is counted as the reader's (the long push's test, run without it, failed the cached base's in 4 of 6 runs).
// D-8 test harness: a lock held in turn by its owners, every test of this binary
// (`a_pushs_churn_per_changeset_is_its_file_alone`, `a_views_manifest_read_is_one_text`,
// `a_cached_file_base_is_read_where_it_lies` and `a_long_pushs_fork_keeps_its_runs_within_a_logarithm_of_its_rows`),
// poison recovered.
#[allow(clippy::disallowed_types)]
static ALONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Shape: the changesets after the first, each rewriting one file, in the shorter history; the longer has twice as many,
/// so their difference is what [`EDITS`] edit changesets allocate alone.
const EDITS: usize = 40;
/// Shape: the content store's window, a daemon's chunk.
const WINDOW: usize = 256 << 10;
/// Shape: the segment bound the intake cuts at, a test's: several segments over the history.
const SEGMENT: u64 = 64 << 10;

/// Shape: what the test's arena reserves up front, past every history's windows: one reservation, on the mapped path.
const ARENA_BYTES: usize = 16 << 20;
/// Shape: the windows its table reserves room for up front.
const ARENA_WINDOWS: usize = 1 << 16;

/// Windows kept as a daemon's store keeps them, in an arena reserved once (the shard's repository arena,
/// `ContentStore::with_storage`), never on the system heap a window at a time: so what the test reads is the push's own
/// churn, not a tool's window storage (`HeapWindows`). Released windows are not reused; nothing here outlives the test.
#[derive(Debug)]
struct Arena {
  /// The windows' bytes, end to end.
  bytes: Vec<u8>,
  /// Each window's place in them, by handle.
  places: Vec<(usize, usize)>,
}

impl Arena {
  /// An arena reserved up front.
  fn new() -> Arena {
    Arena {
      bytes: Vec::with_capacity(ARENA_BYTES),
      places: Vec::with_capacity(ARENA_WINDOWS),
    }
  }
}

impl WindowStorage for Arena {
  fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
    // The test's arena is reserved once to hold every window, so a put past it is a refusal the test then fails on,
    // never a reallocation charged to the push.
    let room = self.bytes.capacity().saturating_sub(self.bytes.len());
    if bytes.len() > room || self.places.len() >= self.places.capacity() {
      return Err(ContentRefused::OverBudget {
        need: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        room: u64::try_from(room).unwrap_or(u64::MAX),
      });
    }
    let at = self.bytes.len();
    self.bytes.extend_from_slice(bytes);
    self.places.push((at, bytes.len()));
    let window = u64::try_from(self.places.len().saturating_sub(1))
      .map_err(|_| ContentRefused::CountExhausted)?;
    Ok(StoredWindow(window))
  }

  fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
    let (at, len) = *self.places.get(usize::try_from(window.0).ok()?)?;
    self.bytes.get(at..at + len).map(Cow::Borrowed)
  }

  fn release(&mut self, _window: StoredWindow) {}
}

/// A revision's text as a changegroup carries it whole: one hunk replacing nothing.
fn whole(text: &[u8]) -> Vec<u8> {
  let mut data = Vec::with_capacity(12 + text.len());
  data.extend_from_slice(&0u32.to_be_bytes());
  data.extend_from_slice(&0u32.to_be_bytes());
  data.extend_from_slice(&u32::try_from(text.len()).unwrap().to_be_bytes());
  data.extend_from_slice(text);
  data
}

/// `text` as a client sends it against `base`: the one hunk replacing the bytes between their common prefix and
/// suffix, as a revision that rewrote one line arrives.
fn delta(base: &[u8], text: &[u8]) -> Vec<u8> {
  let prefix = base.iter().zip(text).take_while(|(a, b)| a == b).count();
  let suffix = base[prefix..]
    .iter()
    .rev()
    .zip(text[prefix..].iter().rev())
    .take_while(|(a, b)| a == b)
    .count();
  let inserted = &text[prefix..text.len() - suffix];
  let mut data = Vec::with_capacity(12 + inserted.len());
  data.extend_from_slice(&u32::try_from(prefix).unwrap().to_be_bytes());
  data.extend_from_slice(&u32::try_from(base.len() - suffix).unwrap().to_be_bytes());
  data.extend_from_slice(&u32::try_from(inserted.len()).unwrap().to_be_bytes());
  data.extend_from_slice(inserted);
  data
}

/// Each of `texts` as a changegroup sends it: the first whole, each later one its delta against the one before.
fn sent(texts: &[&[u8]]) -> Vec<Vec<u8>> {
  texts
    .iter()
    .enumerate()
    .map(
      |(at, text)| match at.checked_sub(1).and_then(|before| texts.get(before)) {
        Some(base) => delta(base, text),
        None => whole(text),
      },
    )
    .collect()
}

/// The forty hexadecimal digits of `node`.
fn hex(node: &Node) -> String {
  node.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A file's revision as the history builds it: its node, its first parent, its text and its link changeset.
type FileRevision = (Node, Node, Vec<u8>, usize);

/// A history of a first changeset adding `files` files of `size` bytes, then `edits` changesets each rewriting one,
/// its changegroup (version 03, each log's first revision whole and each later one its delta against the one before, as
/// a client sends them) encoded, and its manifest text's length.
fn history(files: usize, size: usize, edits: usize) -> (Vec<u8>, usize) {
  let paths: Vec<Vec<u8>> = (0..files)
    .map(|at| format!("dir/file-{at:06}").into_bytes())
    .collect();
  let text_of = |file: usize, generation: usize| -> Vec<u8> {
    let line = format!("file {file} generation {generation} ");
    line.bytes().cycle().take(size).collect()
  };
  // Each file's revisions.
  let mut file_revisions: Vec<Vec<FileRevision>> = vec![Vec::new(); files];
  let mut current: Vec<Node> = vec![NULL_NODE; files];
  let mut changesets: Vec<(Node, Node, Vec<u8>)> = Vec::new();
  let mut manifests: Vec<(Node, Node, Vec<u8>)> = Vec::new();
  for rev in 0..=edits {
    let touched: Vec<usize> = if rev == 0 {
      (0..files).collect()
    } else {
      vec![rev % files]
    };
    for &file in &touched {
      let text = text_of(file, rev);
      let p1 = current[file];
      let file_node = node::compute(&p1, &NULL_NODE, &text).unwrap();
      file_revisions[file].push((file_node, p1, text, rev));
      current[file] = file_node;
    }
    let entries: Vec<Entry<'_>> = paths
      .iter()
      .zip(&current)
      .map(|(path, node)| Entry {
        path,
        node: *node,
        flag: Flag::Regular,
      })
      .collect();
    let mut manifest_text = Vec::new();
    manifest::encode(&entries, &mut manifest_text).unwrap();
    let manifest_p1 = manifests.last().map_or(NULL_NODE, |(node, ..)| *node);
    let manifest_node = node::compute(&manifest_p1, &NULL_NODE, &manifest_text).unwrap();
    manifests.push((manifest_node, manifest_p1, manifest_text));
    let touched_paths: Vec<String> = touched
      .iter()
      .map(|file| String::from_utf8(paths[*file].clone()).unwrap())
      .collect();
    let changeset_text = format!(
      "{}\ntest\n0 0\n{}\n\nchangeset {rev}",
      hex(&manifest_node),
      touched_paths.join("\n")
    )
    .into_bytes();
    let changeset_p1 = changesets.last().map_or(NULL_NODE, |(node, ..)| *node);
    let changeset_node = node::compute(&changeset_p1, &NULL_NODE, &changeset_text).unwrap();
    changesets.push((changeset_node, changeset_p1, changeset_text));
  }
  let changeset_texts: Vec<&[u8]> = changesets
    .iter()
    .map(|(.., text)| text.as_slice())
    .collect();
  let manifest_texts: Vec<&[u8]> = manifests.iter().map(|(.., text)| text.as_slice()).collect();
  let datas: Vec<Vec<u8>> = sent(&changeset_texts)
    .into_iter()
    .chain(sent(&manifest_texts))
    .chain(file_revisions.iter().flat_map(|revisions| {
      let texts: Vec<&[u8]> = revisions
        .iter()
        .map(|(_, _, text, _)| text.as_slice())
        .collect();
      sent(&texts)
    }))
    .collect();
  let mut revisions = Vec::new();
  let mut data = datas.iter();
  for (rev, (node, p1, _)) in changesets.iter().enumerate() {
    revisions.push(revision(
      *node,
      *p1,
      changesets[rev].0,
      data.next().unwrap(),
    ));
  }
  for (rev, (node, p1, _)) in manifests.iter().enumerate() {
    revisions.push(revision(
      *node,
      *p1,
      changesets[rev].0,
      data.next().unwrap(),
    ));
  }
  let mut groups = Vec::new();
  for (file, revisions_of) in file_revisions.iter().enumerate() {
    let start = revisions.len();
    for (node, p1, _, link) in revisions_of {
      revisions.push(revision(
        *node,
        *p1,
        changesets[*link].0,
        data.next().unwrap(),
      ));
    }
    groups.push((file, start..revisions.len()));
  }
  let changegroup = Changegroup {
    version: Version::V03,
    revisions,
    changelog: 0..changesets.len(),
    manifests: changesets.len()..changesets.len() + manifests.len(),
    trees: Vec::new(),
    files: groups
      .into_iter()
      .map(|(file, range)| Group {
        name: &paths[file],
        revisions: range,
      })
      .collect(),
  };
  let mut out = Vec::new();
  changegroup.encode(&mut out).unwrap();
  (out, manifests.last().map_or(0, |(.., text)| text.len()))
}

/// A revision as sent: its node, first parent and link, its `data` against its first parent (the revision before it in
/// its log, or none).
fn revision(node: Node, p1: Node, link: Node, data: &[u8]) -> Revision<'_> {
  Revision {
    node,
    p1,
    p2: NULL_NODE,
    base: p1,
    link,
    flags: 0,
    info: None,
    protocol_flags: 0,
    data,
    sidedata: None,
  }
}

/// What one reading of the heap's figures allocates itself ([`heap`] reads every mapped block's residency into a
/// buffer), taken off the figure it reads.
fn reading_cost() -> u64 {
  let before = heap().system_allocated;
  heap().system_allocated.saturating_sub(before)
}

/// What taking `payload` into an empty index allocated on the system path, the readings' own allocations taken off: its
/// steps', each segment's record written as it is cut.
fn churned(payload: &[u8]) -> u64 {
  let mut store = ContentStore::with_storage(u64::MAX, WINDOW, Box::new(Arena::new())).unwrap();
  // Read once the arena is made: a reading reads every mapped block's residency, the arena's among them.
  let reading = reading_cost();
  let mut index = HgIndex::new();
  let mut admission = Admission::new(u64::MAX);
  let before = heap().system_allocated;
  let mut intake = Intake::begin(
    (Version::V03, payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap()
  .cutting(SEGMENT);
  loop {
    let stepped = intake.step(payload, &mut store, &mut admission).unwrap();
    if matches!(stepped, Stepped::Done) {
      intake.cut_rest();
    }
    while let Some(segment) = intake.take_segment() {
      let _record = whole_record(&intake, &segment, u64::MAX, &store);
    }
    match stepped {
      Stepped::More => {}
      Stepped::Done => break,
      other => panic!("a whole changegroup in memory stepped {other:?}"),
    }
  }
  heap()
    .system_allocated
    .saturating_sub(before)
    .saturating_sub(reading)
}

/// Shape: what an edit changeset may allocate: its rows, its bookkeeping and its share of its segment's record, no copy
/// of its file's text nor of its manifest's (measured at 10,284 B to 13,674 B over the five histories, debug, once the
/// file's rebuild reused a buffer the push let go).
const SLACK_BYTES: u64 = 16 << 10;

/// Goal 93 (main's orders on CI 45127535742 and of 2026-11-23: name the top allocators per changeset, then cut by reuse;
/// no allocation by design; w-perf's attribution of Mercurial's own push, 3.76 GB of `intake::rebuilt` freed within a
/// second, 1.07 GB of it in the [128 KiB, 256 KiB) band glibc keeps). Do: take a history of a first changeset adding a
/// number of files, then [`EDITS`] changesets each rewriting one, sent as a client sends them (each revision a delta
/// against the one before), into an empty index through the intake, its windows in an arena as a daemon's store keeps
/// them, cut at [`SEGMENT`], each segment's record written; again with twice the edits; at three manifest sizes and two
/// file sizes, one of them in that band. Read the system path's churn per edit changeset (the two histories'
/// difference over [`EDITS`]). Expect, at the largest manifest and at each large file, at most [`SLACK_BYTES`] and less
/// than one copy of the file's text: neither the manifest's rebuild nor the file's allocates, each reusing a buffer the
/// push let go. Red before (2026-11-23, MacBook arm64, debug): 291,519 B an edit at a 91,200 B manifest, 3.20 copies;
/// then 102,015 B, 1.12, a manifest rebuilt into a new buffer each edit; then a file revision rebuilt into its own text:
/// 28,855 B an edit at a 16 KiB file (1.76 copies), and 177,497 B at a 160 KiB one (1.08) with its rebuild's buffer
/// reused but every text of its group kept in the push's cache; now 10,284 B at the largest manifest, 12,047 B at the
/// 16 KiB file (0.74) and 13,674 B at the 160 KiB one (0.08): the cache keeps only a text a later delta rests on.
#[test]
fn a_pushs_churn_per_changeset_is_its_file_alone() {
  let _alone = ALONE
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  for (files, size) in [
    (100, 64),
    (400, 64),
    (1_600, 64),
    (100, 16 << 10),
    (4, 160 << 10),
  ] {
    let (short_payload, manifest) = history(files, size, EDITS);
    let short = churned(&short_payload);
    let long = churned(&history(files, size, 2 * EDITS).0);
    let per_edit = long.saturating_sub(short) / u64::try_from(EDITS).unwrap();
    let (manifest, file) = (
      u64::try_from(manifest).unwrap(),
      u64::try_from(size).unwrap(),
    );
    println!(
      "push_heap: {files} files of {size} B, a {manifest} B manifest: an edit changeset allocates {per_edit} B \
       ({:.2} manifests, {:.2} files)",
      per_edit as f64 / manifest as f64,
      per_edit as f64 / file as f64
    );
    if size >= 16 << 10 {
      assert!(
        per_edit < file,
        "an edit changeset allocated {per_edit} B, a copy of its {file} B file's text"
      );
    }
    if files == 1_600 || size >= 16 << 10 {
      assert!(
        per_edit <= SLACK_BYTES,
        "an edit changeset allocated {per_edit} B, past {SLACK_BYTES} B (its file {file} B, its manifest {manifest} B)"
      );
    }
  }
}

/// `index` with `payload` taken into it whole: the fork the intake made, `index` given back.
fn taken(mut index: HgIndex, payload: &[u8], store: &mut ContentStore) -> HgIndex {
  let mut admission = Admission::new(u64::MAX);
  let mut intake =
    Intake::begin((Version::V03, payload), &mut index, store, &mut admission).unwrap();
  loop {
    match intake.step(payload, store, &mut admission).unwrap() {
      Stepped::More => {}
      Stepped::Done => break,
      other => panic!("a whole changegroup in memory stepped {other:?}"),
    }
  }
  let Ok((fork, _)) = intake.finish() else {
    panic!("a whole changegroup taken");
  };
  index.release(store).unwrap();
  fork
}

/// Goal 93 (main's order: C1's cuts, `HgIndex::revision_text` second by bytes; w-perf's attribution of Mercurial's own
/// push, 2.57 GB of `revision_text`'s rebuilds freed within a second, 1.24 GB of them in the [128 KiB, 256 KiB) band):
/// the served view reads a manifest kept as a delta (A-729) by rebuilding it from its chain, as the server reads each
/// pushed changeset's manifest to build the push's works. Do: take a history of 1,600 files and twice [`EDITS`] edits
/// into an index, view it, and read the first [`EDITS`] manifests, then all of them. Read the churn per manifest read
/// (the two readings' difference over [`EDITS`]). Expect at most one copy of a manifest's text and [`SLACK_BYTES`]: the
/// text read lent out, its chain rebuilt through buffers the view holds. Red before: each read rebuilt into a text, a
/// next text and a delta of its own, the whole it began from grown from empty: 194,487 B a read of a 91,200 B manifest
/// (2.13 copies, 2026-11-23, MacBook arm64, debug); now 105,500 B (1.16), the text the read hands out.
#[test]
fn a_views_manifest_read_is_one_text() {
  let _alone = ALONE
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  let (payload, manifest) = history(1_600, 64, 2 * EDITS);
  let mut store = ContentStore::with_storage(u64::MAX, WINDOW, Box::new(Arena::new())).unwrap();
  let index = taken(HgIndex::new(), &payload, &mut store);
  let reading = reading_cost();
  let state = State {
    roots: &[],
    bookmarks: &[],
    publishing: true,
    markers: &[],
  };
  let view = Indexed::new(
    &index,
    &store,
    state,
    (u64::MAX, &mut Admission::new(u64::MAX)),
  )
  .unwrap();
  let nodes: Vec<Node> = (0..index.len(&store, Log::Manifest))
    .filter_map(|rev| {
      index
        .revision(&store, Log::Manifest, rev)
        .map(|row| row.node)
    })
    .collect();
  let churn = |count: usize| {
    let mut room = Admission::new(u64::MAX);
    let before = heap().system_allocated;
    for node in &nodes[..count] {
      let read = view.manifest(node, &mut room).unwrap();
      assert!(read.is_some(), "the manifest read");
    }
    heap()
      .system_allocated
      .saturating_sub(before)
      .saturating_sub(reading)
  };
  let per_read = churn(2 * EDITS).saturating_sub(churn(EDITS)) / u64::try_from(EDITS).unwrap();
  let manifest = u64::try_from(manifest).unwrap();
  println!(
    "push_heap: a view's manifest read allocates {per_read} B ({:.2} manifests of {manifest} B)",
    per_read as f64 / manifest as f64
  );
  assert!(
    per_read <= manifest + SLACK_BYTES,
    "a manifest read allocated {per_read} B, past one {manifest} B manifest and {SLACK_BYTES} B"
  );
}

/// A history of `revisions` changesets on two interleaved lines, each rewriting the one file `f` of `size` bytes: the
/// first a root, the second on it, every later one on the changeset two before it (its first parent `rev - 2`), each
/// log's revision sent as its delta against its first parent's text (as a client sends a branchy file history), so a
/// file delta rests on the revision two back, never the one kept last. Its changegroup (version 03) encoded.
fn interleaved(size: usize, revisions: usize) -> Vec<u8> {
  let path = b"f".to_vec();
  let parent = |rev: usize| match rev {
    0 => None,
    1 => Some(0),
    rev => Some(rev - 2),
  };
  let mut file_texts: Vec<Vec<u8>> = Vec::new();
  let mut file_nodes: Vec<Node> = Vec::new();
  let mut manifest_texts: Vec<Vec<u8>> = Vec::new();
  let mut manifest_nodes: Vec<Node> = Vec::new();
  let mut changeset_texts: Vec<Vec<u8>> = Vec::new();
  let mut changeset_nodes: Vec<Node> = Vec::new();
  for rev in 0..revisions {
    let p1 = parent(rev);
    let text: Vec<u8> = format!("f generation {rev} ")
      .bytes()
      .cycle()
      .take(size)
      .collect();
    let file_node = node::compute(
      &p1.map_or(NULL_NODE, |at| file_nodes[at]),
      &NULL_NODE,
      &text,
    )
    .unwrap();
    let mut manifest_text = Vec::new();
    manifest::encode(
      &[Entry {
        path: &path,
        node: file_node,
        flag: Flag::Regular,
      }],
      &mut manifest_text,
    )
    .unwrap();
    let manifest_node = node::compute(
      &p1.map_or(NULL_NODE, |at| manifest_nodes[at]),
      &NULL_NODE,
      &manifest_text,
    )
    .unwrap();
    let changeset_text =
      format!("{}\ntest\n0 0\nf\n\nchangeset {rev}", hex(&manifest_node)).into_bytes();
    let changeset_node = node::compute(
      &p1.map_or(NULL_NODE, |at| changeset_nodes[at]),
      &NULL_NODE,
      &changeset_text,
    )
    .unwrap();
    file_texts.push(text);
    file_nodes.push(file_node);
    manifest_texts.push(manifest_text);
    manifest_nodes.push(manifest_node);
    changeset_texts.push(changeset_text);
    changeset_nodes.push(changeset_node);
  }
  let data_of = |texts: &[Vec<u8>]| -> Vec<Vec<u8>> {
    (0..revisions)
      .map(|rev| match parent(rev) {
        Some(at) => delta(&texts[at], &texts[rev]),
        None => whole(&texts[rev]),
      })
      .collect()
  };
  let (changeset_data, manifest_data, file_data) = (
    data_of(&changeset_texts),
    data_of(&manifest_texts),
    data_of(&file_texts),
  );
  let p1_of = |nodes: &[Node], rev: usize| parent(rev).map_or(NULL_NODE, |at| nodes[at]);
  let mut revisions_sent = Vec::new();
  for rev in 0..revisions {
    revisions_sent.push(revision(
      changeset_nodes[rev],
      p1_of(&changeset_nodes, rev),
      changeset_nodes[rev],
      &changeset_data[rev],
    ));
  }
  for rev in 0..revisions {
    revisions_sent.push(revision(
      manifest_nodes[rev],
      p1_of(&manifest_nodes, rev),
      changeset_nodes[rev],
      &manifest_data[rev],
    ));
  }
  for rev in 0..revisions {
    revisions_sent.push(revision(
      file_nodes[rev],
      p1_of(&file_nodes, rev),
      changeset_nodes[rev],
      &file_data[rev],
    ));
  }
  let changegroup = Changegroup {
    version: Version::V03,
    revisions: revisions_sent,
    changelog: 0..revisions,
    manifests: revisions..2 * revisions,
    trees: Vec::new(),
    files: vec![Group {
      name: &path,
      revisions: 2 * revisions..3 * revisions,
    }],
  };
  let mut out = Vec::new();
  changegroup.encode(&mut out).unwrap();
  out
}

/// Goal 93 (main's order: C1's cuts, `Taker::file_base` third; w-perf's attribution of Mercurial's own push, its copy of
/// a cached base in the [128 KiB, 256 KiB) band). Do: take a history of one 160 KiB file on two interleaved lines, each
/// file delta resting on the revision two back (a base the push's cache holds, never the text kept last), then one of
/// twice as many revisions. Read the churn per revision (the two histories' difference over [`EDITS`]). Expect less
/// than one copy of the file's text a revision: a base the cache holds is read where it lies. Red before: each base
/// found in the cache was copied out of it whole for the step that read it.
#[test]
fn a_cached_file_base_is_read_where_it_lies() {
  let _alone = ALONE
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  let size = 160 << 10;
  let short = churned(&interleaved(size, EDITS));
  let long = churned(&interleaved(size, 2 * EDITS));
  let per_revision = long.saturating_sub(short) / u64::try_from(EDITS).unwrap();
  let file = u64::try_from(size).unwrap();
  println!(
    "push_heap: two lines of a {file} B file, each delta on the revision two back: a revision allocates {per_revision} B \
     ({:.2} files)",
    per_revision as f64 / file as f64
  );
  assert!(
    per_revision < file,
    "a revision allocated {per_revision} B, a copy of its {file} B file's text"
  );
}

/// Shape: the edits of the runs test's history: enough appends under its window for hundreds of seals.
const LONG_EDITS: usize = 1_500;

/// Goal 93 (the index seal's reland, main's ruling of 2026-11-23): every append seals the fork's tail when due, each
/// seal a run, and a lookup of a new row walks every run; arena-14's push of Mercurial's history spun past the watchdog
/// in a segment step as the fork's unmerged runs grew. Do: take a history of [`LONG_EDITS`] edits through the intake
/// under a 4 KiB window, so the fork seals hundreds of times. Expect the most runs only the fork held after any step
/// within ⌊log₃(its rows)⌋ + 1 (the size-tiered rule's bound, a seal writing a row at least), and merges made (the
/// counter, so the bound is not met by sealing nothing). Red before: no merge, the fork's own runs one a seal.
#[test]
fn a_long_pushs_fork_keeps_its_runs_within_a_logarithm_of_its_rows() {
  let _alone = ALONE
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  let (payload, _) = history(50, 64, LONG_EDITS);
  let mut store = ContentStore::new(u64::MAX, 4 << 10).unwrap();
  let mut index = HgIndex::new();
  let mut admission = Admission::new(u64::MAX);
  let mut intake = Intake::begin(
    (Version::V03, &payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap();
  loop {
    match intake.step(&payload, &mut store, &mut admission).unwrap() {
      Stepped::More => {}
      Stepped::Done => break,
      other => panic!("a whole changegroup in memory stepped {other:?}"),
    }
  }
  let Ok((mut fork, intaken)) = intake.finish() else {
    panic!("a whole changegroup taken");
  };
  let rows = fork.rows().rows();
  // In integers: f64's log₃ of a power of three can fall just short of it (243 gives 4.999999999999999).
  // The rule weighs bytes (⌊log₃(own bytes ÷ the fewest bytes a seal writes)⌋ + 1), but the fewest bytes a seal
  // writes is the data's: a small row cut before a row near a window long makes a short table. A seal writes a row at
  // least, so the bound in rows is the one that holds for any data, looser than the bytes bound.
  let bound = u64::from(rows.checked_ilog(3).unwrap_or(0)) + 1;
  println!(
    "push_heap: {rows} rows, {} seals, {} tiered merges, the most own runs {} (bound {bound})",
    fork.rows().counters().seals,
    intaken.tiered_merges,
    intaken.most_own_runs
  );
  assert!(
    fork.rows().counters().seals > 100,
    "the fork sealed as it appended"
  );
  assert!(intaken.tiered_merges > 0, "the tiered rule merged");
  assert!(
    intaken.most_own_runs <= bound,
    "the fork held {} runs of its own, past the bound {bound} at {rows} rows",
    intaken.most_own_runs
  );
}

/// `segment`'s record, written by `intake` a step at a time to its end (as the daemon writes it, a turn's steps at a time).
fn whole_record(
  intake: &Intake,
  segment: &PushSegment,
  room: u64,
  store: &ContentStore,
) -> Vec<u8> {
  let mut writer = intake.record_writer(segment, room).unwrap();
  while intake.record_step(&mut writer, store).unwrap() {}
  writer.finish().unwrap()
}
