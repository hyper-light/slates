//! Tests of the kept rendered length (goal 93, cut 2; main 01a20182's rulings of 2026-11-27 18:56 and 18:58): each
//! revision's chunk span and the repository's [`Rendered`] kept where revisions land
//! ([`crate::index::HgIndex::keep_sized`]), so a stream names its head and writes its index entries with no count walk.
//! The oracle is the count walk, kept as the recount ([`crate::render::Recount`]): the same generated history landed
//! twice, once sized where each landing lands and once sized by a recount after it, must keep the same values after
//! every landing; a replay from a generated landing on, sizing nothing before it (an import's record names no length),
//! must reach them by its recount; and the landed history's stream is served, every text it sends read back through
//! the entries the kept values wrote, which holds each kept length to the chunk actually sent.

use std::collections::BTreeSet;

use super::*;
use crate::index::{ChunkSpan, KeptSized, Placed, ToSize};
use crate::render::{StoredPlace, StoredRevision, StoredSource};

/// Shape: the paths a generated history's files take: plain lowercase names, which a store path keeps as they are, one
/// in a directory.
const PATHS: [&[u8]; 3] = [b"a", b"b/c", b"d"];

/// One file a generated changeset changes: its path's place in [`PATHS`], its text's length, and whether the store
/// keeps its place as a revlog chunk.
type FileChange = (usize, usize, bool);

/// A generated changeset: the files it changes, and whether its changeset's and its manifest's places are chunks.
#[derive(Clone, Debug)]
struct Change {
  files: Vec<FileChange>,
  changeset_chunk: bool,
  manifest_chunk: bool,
}

/// A generated landing: the changesets one push or import segment brings, in order.
type Landing = Vec<Change>;

/// Generated histories: one to four landings of one to three changesets, each changing up to two files. Most texts are
/// short; some are tens of KiB of bytes that do not compress, so a file log crosses 7.2's inline bound
/// (`revlog_chunk::MAX_INLINE_DATA`) as the history grows.
fn landings() -> impl Strategy<Value = Vec<Landing>> {
  let length = prop_oneof![3 => 1_usize..400, 1 => 20_000_usize..70_000];
  let file = (0..PATHS.len(), length, any::<bool>());
  let change = (
    prop::collection::vec(file, 0..3),
    any::<bool>(),
    any::<bool>(),
  )
    .prop_map(|(files, changeset_chunk, manifest_chunk)| Change {
      files,
      changeset_chunk,
      manifest_chunk,
    });
  prop::collection::vec(prop::collection::vec(change, 1..4), 1..5)
}

/// `len` bytes that do not compress, drawn from `seed`.
fn noise(len: usize, seed: u64) -> Vec<u8> {
  let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
  (0..len)
    .map(|_| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state.to_le_bytes()[0]
    })
    .collect()
}

/// A changeset store answering a stream over a generated history: every revision whole, its place a chunk of its whole
/// where the history chose so, else its raw text ([`KeptAsChunks`] and [`WholeTexts`] mixed by text).
struct Mixed<'s> {
  whole: WholeTexts<'s>,
  chunked: BTreeSet<[u8; 32]>,
}

impl StoredSource for Mixed<'_> {
  fn rows(&mut self, logs: &[(Vec<u8>, Vec<Node>)]) -> Option<Vec<Vec<Option<StoredRevision>>>> {
    let mut answered = self.whole.rows(logs)?;
    for row in answered.iter_mut().flatten().flatten() {
      if self.chunked.contains(&row.text) {
        row.place = self.chunk_of(&row.place)?;
      }
    }
    Some(answered)
  }

  fn chunks(&mut self, places: &[StoredPlace]) -> Option<Vec<Vec<u8>>> {
    self.whole.chunks(places)
  }
}

impl Mixed<'_> {
  /// The chunk place of the whole at `place`: the whole made into stock's chunk, kept under its own identity.
  fn chunk_of(&mut self, place: &StoredPlace) -> Option<StoredPlace> {
    let whole = self.whole.texts.get(&place.payload)?.clone();
    let mut chunk = Vec::new();
    crate::revlog_chunk::chunk(&whole, crate::revlog_chunk::Pass::Send, &mut chunk).ok()?;
    let payload = *blake3::hash(&chunk).as_bytes();
    let len = u64::try_from(chunk.len()).ok()?;
    self.whole.texts.insert(payload, chunk);
    Some(StoredPlace {
      payload,
      offset: 0,
      len,
      chunk: true,
    })
  }
}

/// A generated history as it lands: the index, its store, each log's nodes in order, and the texts kept as chunks.
struct History {
  built: Built,
  changesets: Vec<Node>,
  manifests: Vec<Node>,
  files: Vec<Vec<Node>>,
  chunked: BTreeSet<[u8; 32]>,
  seed: u64,
}

impl History {
  fn new() -> History {
    History {
      built: Built {
        index: HgIndex::new(),
        store: store(),
        nodes: Vec::new(),
        roots: Vec::new(),
        bookmarks: Vec::new(),
      },
      changesets: Vec::new(),
      manifests: Vec::new(),
      files: vec![Vec::new(); PATHS.len()],
      chunked: BTreeSet::new(),
      seed: 0,
    }
  }

  /// The next node and text seed.
  fn next_seed(&mut self) -> u64 {
    self.seed = self.seed.wrapping_add(1);
    self.seed
  }

  /// `text` held as content, its identity marked a chunk where `chunk` says.
  fn put(&mut self, text: &[u8], chunk: bool) -> ContentId {
    if chunk {
      self.chunked.insert(*blake3::hash(text).as_bytes());
    }
    self.built.store.put_object(text).unwrap()
  }

  /// Appends one generated changeset, its manifest and its files.
  fn append(&mut self, change: &Change) {
    let text = noise(64, self.next_seed());
    let manifest_text = noise(48, self.next_seed());
    let manifest = self.node_after(Log::Manifest, &manifest_text);
    let p1 = self.changesets.last().copied().unwrap_or(NULL_NODE);
    let node = node::compute(&p1, &NULL_NODE, &text).unwrap();
    let new = NewChangeset {
      node,
      p1,
      p2: NULL_NODE,
      flags: 0,
      len: u64::try_from(text.len()).unwrap(),
      text: self.put(&text, change.changeset_chunk),
      manifest,
      closed: false,
      branch: b"default",
    };
    let built = &mut self.built;
    built
      .index
      .append_changesets(&mut built.store, &[new])
      .unwrap();
    self.changesets.push(node);
    self.built.nodes.push(node);
    self.append_revision(Log::Manifest, node, (&manifest_text, change.manifest_chunk));
    for (path, len, chunk) in &change.files {
      let text = noise(*len, self.next_seed());
      self.append_revision(Log::File(PATHS[*path]), node, (&text, *chunk));
    }
  }

  /// The node `text` takes after `log`'s last revision, its first parent (7.2's hash of the sorted parents and text).
  fn node_after(&self, log: Log<'_>, text: &[u8]) -> Node {
    let p1 = self.nodes_of(log).last().copied().unwrap_or(NULL_NODE);
    node::compute(&p1, &NULL_NODE, text).unwrap()
  }

  /// Appends `text` to `log` after its last revision, linked to changeset `link`, its node its hash.
  fn append_revision(&mut self, log: Log<'_>, link: Node, (text, chunk): (&[u8], bool)) {
    let p1 = self.nodes_of(log).last().copied().unwrap_or(NULL_NODE);
    let node = self.node_after(log, text);
    let revision = NewRevision {
      node,
      p1,
      p2: NULL_NODE,
      link,
      flags: 0,
      len: u64::try_from(text.len()).unwrap(),
      bytes: self.put(text, chunk),
      block: None,
    };
    let built = &mut self.built;
    built
      .index
      .append_revisions(&mut built.store, log, &[revision])
      .unwrap();
    match log {
      Log::Changelog => self.changesets.push(node),
      Log::Manifest => self.manifests.push(node),
      Log::File(path) => {
        let at = PATHS.iter().position(|known| *known == path).unwrap();
        self.files[at].push(node);
      }
    }
  }

  /// `log`'s nodes so far, in order.
  fn nodes_of(&self, log: Log<'_>) -> &[Node] {
    match log {
      Log::Changelog => &self.changesets,
      Log::Manifest => &self.manifests,
      Log::File(path) => PATHS
        .iter()
        .position(|known| *known == path)
        .map_or(&[], |at| &self.files[at]),
    }
  }

  /// Every log the history holds, as its stream names it: the changelog, the manifest log, each file's.
  fn logs(&self) -> Vec<Log<'static>> {
    let files = PATHS
      .iter()
      .zip(&self.files)
      .filter(|(_, nodes)| !nodes.is_empty())
      .map(|(path, _)| Log::File(path));
    [Log::Changelog, Log::Manifest]
      .into_iter()
      .chain(files)
      .collect()
  }

  /// The changeset store over this history.
  fn source(&self) -> Mixed<'_> {
    Mixed {
      whole: whole_texts(&self.built.index, &self.built.store),
      chunked: self.chunked.clone(),
    }
  }
}

/// A log's name as the changeset store's `Texts` section names it.
fn stored_name(log: Log<'_>) -> Vec<u8> {
  match log {
    Log::Changelog => b"\0changelog".to_vec(),
    Log::Manifest => b"\0manifest".to_vec(),
    Log::File(path) => path.to_vec(),
  }
}

/// The places of `log`'s revisions from `from` on, as the changeset store keeps them: what a landing sizes.
fn places_from(history: &History, log: Log<'_>, from: usize) -> Vec<(StoredPlace, Vec<u8>)> {
  let mut source = history.source();
  let nodes = history.nodes_of(log)[from..].to_vec();
  let rows = source.rows(&[(stored_name(log), nodes)]).unwrap().remove(0);
  rows
    .into_iter()
    .map(|row| {
      let place = row.unwrap().place;
      let bytes = source.chunks(&[place]).unwrap().remove(0);
      (place, bytes)
    })
    .collect()
}

/// How a landing sizes what it brings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sizing {
  /// Each log's new revisions sized where they land, every one of them (a push, a first import).
  Landed,
  /// Sized where they land unless an earlier revision of the log is not (a push replayed over an import's record).
  Replayed,
  /// Not sized (an import's record replayed: it names no chunk length).
  None,
}

/// Lands `landing` on `history`: its changesets appended, then each log's new revisions sized as `sizing` says
/// ([`HgIndex::keep_sized`]), as the changeset store keeps their places.
fn land(history: &mut History, landing: &Landing, sizing: Sizing) {
  let before: Vec<(Log<'static>, usize)> = history
    .logs()
    .into_iter()
    .map(|log| (log, history.nodes_of(log).len()))
    .collect();
  for change in landing {
    history.append(change);
  }
  if sizing == Sizing::None {
    return;
  }
  for log in history.logs() {
    let from = before
      .iter()
      .find(|(known, _)| *known == log)
      .map_or(0, |(_, count)| *count);
    let kept = keep_log(history, log, from);
    assert!(
      sizing == Sizing::Replayed || matches!(kept, KeptSized::Sized { .. }),
      "{log:?} sized where it landed: {kept:?}"
    );
  }
}

/// Sizes `log`'s revisions from `from` on where they land: each place's chunk length, raw ones by the stream's own
/// compressor ([`crate::index::chunk_len_of`]). What the index did.
fn keep_log(history: &mut History, log: Log<'_>, from: usize) -> KeptSized {
  let places = places_from(history, log, from);
  let from = u32::try_from(from).unwrap();
  let lengths: Vec<u32> = (from..)
    .zip(&places)
    .map(|(rev, (place, bytes))| {
      let placed = if place.chunk {
        Placed::Chunk(place.len)
      } else {
        Placed::Raw(bytes)
      };
      crate::index::chunk_len_of(placed, rev).unwrap()
    })
    .collect();
  let sizes = ToSize {
    from,
    lengths: &lengths,
    streamable: true,
  };
  let built = &mut history.built;
  built
    .index
    .keep_sized(&mut built.store, log, sizes)
    .unwrap()
}

/// `history` recounted from its changeset store ([`super::sized_by`], the count walk), what it sized before kept as it is.
fn recount(history: &mut History) {
  let chunked = history.chunked.clone();
  let built = &mut history.built;
  super::sized_by(&mut built.index, &mut built.store, &|index, store| {
    Box::new(Mixed {
      whole: whole_texts(index, store),
      chunked: chunked.clone(),
    })
  });
}

/// The stream the count walk makes of `history` now: its status and its files.
fn walked(history: &History) -> Streamed {
  use crate::serve::stream::stream_out;
  let view = view_of(&history.built, true);
  let mut answer = Vec::new();
  stream_out(&view, &mut history.source(), &mut room(), &mut answer).unwrap();
  streamed(&answer)
}

/// The file a stream names `log`'s index by.
fn index_name(log: Log<'_>) -> Vec<u8> {
  match log {
    Log::Changelog => b"00changelog.i".to_vec(),
    Log::Manifest => b"00manifest.i".to_vec(),
    Log::File(path) => [b"data/", path, b".i"].concat(),
  }
}

/// `log`'s index and data files as `sent` carries them (no data file for an inline log).
fn files_of(sent: &[(Vec<u8>, Vec<u8>)], log: Log<'_>) -> (Vec<u8>, Vec<u8>) {
  let index = index_name(log);
  let mut data = index.clone();
  data.pop();
  data.push(b'd');
  let named = |name: &[u8]| {
    sent
      .iter()
      .find(|(sent_name, _)| sent_name == name)
      .map(|(_, bytes)| bytes.clone())
  };
  (named(&index).unwrap(), named(&data).unwrap_or_default())
}

/// `landed` and `counted` keep the same values: the head's files and bytes, and every revision's chunk span; the head
/// kept at all.
fn check_same(landed: &History, counted: &History) {
  let rendered = landed.built.index.rendered();
  assert!(rendered.is_some(), "the landed history serves a stream");
  assert_eq!(rendered, counted.built.index.rendered(), "the head");
  for log in landed.logs() {
    assert_eq!(
      spans_of(landed, log),
      spans_of(counted, log),
      "{log:?}'s chunk spans"
    );
    let (index, store) = (&landed.built.index, &landed.built.store);
    let counted_data = counted.built.index.log_data(&counted.built.store, log);
    assert_eq!(
      index.log_data(store, log),
      counted_data,
      "{log:?}'s chunks' bytes"
    );
  }
}

/// Every revision of `log` in `history`, its kept chunk span.
fn spans_of(history: &History, log: Log<'_>) -> Vec<Option<ChunkSpan>> {
  let (index, store) = (&history.built.index, &history.built.store);
  (0..index.len(store, log))
    .map(|rev| index.chunk_span(store, log, rev))
    .collect()
}

/// `history`'s stream is served, and each revlog it sends reads back: every text through its entry's offset and length
/// (the kept values), held to its node by the reader. What `sent` carried.
fn check_served(history: &History) -> Vec<(Vec<u8>, Vec<u8>)> {
  let (status, sent) = walked(history);
  assert_eq!(status, b"0", "the stream served");
  for log in history.logs() {
    let (index, data) = files_of(&sent, log);
    let revlog = crate::revlog::Revlog::open(&index, &data, &mut room()).unwrap();
    for rev in 0..revlog.len() {
      revlog.text(rev, &mut room()).unwrap();
    }
  }
  sent
}

/// The generated `landings` landed on three histories: sized where each lands, recounted after each, and replayed from
/// landing `replayed_from` on (none sized before it), then recounted at the end; the checks after every landing and at
/// the end.
fn landed_thrice(landings: &[Landing], replayed_from: usize) {
  let (mut landed, mut counted, mut replayed) = (History::new(), History::new(), History::new());
  for (at, landing) in landings.iter().enumerate() {
    land(&mut landed, landing, Sizing::Landed);
    land(&mut counted, landing, Sizing::None);
    recount(&mut counted);
    let sizing = if at < replayed_from {
      Sizing::None
    } else {
      Sizing::Replayed
    };
    land(&mut replayed, landing, sizing);
    check_same(&landed, &counted);
    check_served(&landed);
  }
  recount(&mut replayed);
  check_same(&landed, &replayed);
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(24)))]

  /// Goal 93, cut 2 (main 01a20182's rulings of 2026-11-27 18:56 and 18:58), the oracle. Do: land a generated history
  /// ([`landings`]) a landing at a time, its texts kept as raw places or as stock's chunks as generated, three ways
  /// ([`landed_thrice`]): each log's new revisions sized where they land ([`HgIndex::keep_sized`]); sized by the count
  /// walk after each landing instead ([`crate::render::Recount`]); and replayed from a generated landing on, nothing
  /// sized before it, then recounted. Expect, after every landing: the landed and the counted histories keep the same
  /// head (files and bytes), the same chunks' bytes a log and the same chunk span a revision; the landed one's stream
  /// served and every text it sends read back; and the replayed history, recounted, keeps what the landed one does.
  /// Red before: nothing kept (`rendered` none).
  #[test]
  fn a_kept_rendered_length_is_what_the_count_walks(
    (landings, replayed_from) in landings().prop_flat_map(|landings| {
      let count = landings.len();
      (Just(landings), 0..=count)
    })
  ) {
    landed_thrice(&landings, replayed_from);
  }
}

/// Goal 93, cut 2, the oracle's reach (non-vacuity): do land a fixed history whose file `a` takes three texts of
/// 60,000 bytes that do not compress (past 7.2's inline bound together), kept raw, as a chunk and raw, beside short
/// texts, three ways as the oracle does ([`landed_thrice`]). Expect the oracle's checks to hold, the stream to send
/// `a` split (its data file `data/a.d`), and the history to keep raw places and chunk places both.
#[test]
fn a_kept_rendered_length_reaches_a_split_log_and_both_forms() {
  let change = |files: Vec<FileChange>, chunk: bool| Change {
    files,
    changeset_chunk: chunk,
    manifest_chunk: !chunk,
  };
  let landings = vec![
    vec![change(vec![(0, 60_000, false), (1, 40, true)], false)],
    vec![
      change(vec![(0, 60_000, true)], true),
      change(vec![(2, 12, false)], false),
    ],
    vec![change(vec![(0, 60_000, false)], true)],
  ];
  landed_thrice(&landings, 1);
  let mut history = History::new();
  for landing in &landings {
    land(&mut history, landing, Sizing::Landed);
  }
  let sent = check_served(&history);
  assert!(
    sent.iter().any(|(name, _)| name == b"data/a.d"),
    "file a sent split"
  );
  let texts = history.built.nodes.len() * 2 + 4;
  assert!(
    !history.chunked.is_empty() && history.chunked.len() < texts,
    "raw and chunk places both: {} chunks of {texts} texts",
    history.chunked.len()
  );
}

/// Goal 93, cut 2, a stale length (main 01a20182's ruling of 2026-11-27 18:56: refused typed, never sent). Do: land a
/// history sized where it lands, then append a changeset whose revisions are not sized. Expect no stream served (`1\n`,
/// before the head: the index's kept totals are not its rows'). Then size a fresh history's changelog one byte past its
/// chunk. Expect its stream refused [`crate::render::Refused::StaleLength`] at that chunk, its head written and no byte
/// past what the head named.
#[test]
fn a_stale_kept_length_is_refused_and_never_sent() {
  let short = Change {
    files: vec![(0, 30, false)],
    changeset_chunk: false,
    manifest_chunk: false,
  };
  let mut history = History::new();
  land(&mut history, &vec![short.clone()], Sizing::Landed);
  history.append(&short);
  assert_eq!(history.built.index.rendered(), None, "a revision not sized");
  let mut answer = Vec::new();
  crate::serve::stream::stream_out(
    &view_of(&history.built, true),
    &mut history.source(),
    &mut room(),
    &mut answer,
  )
  .unwrap();
  assert_eq!(answer, b"1\n", "no stream before its head");
  let refused = streamed_past_its_length(short);
  assert!(
    matches!(
      refused,
      Err(crate::render::Refused::StaleLength { rev: 0, .. })
    ),
    "{refused:?}"
  );
}

/// A history of `change` alone, its changelog's one chunk kept a byte longer than it is and every other revision as it
/// is, streamed a step at a time: what the stream answered once it stopped, its bytes held to what its head named.
fn streamed_past_its_length(change: Change) -> Result<(), crate::render::Refused> {
  let mut history = History::new();
  land(&mut history, &vec![change], Sizing::None);
  for log in history.logs() {
    let places = places_from(&history, log, 0);
    let mut lengths: Vec<u32> = places
      .iter()
      .map(|(_, bytes)| crate::index::chunk_len_of(Placed::Raw(bytes), 0).unwrap())
      .collect();
    if log == Log::Changelog {
      lengths[0] += 1;
    }
    let sizes = ToSize {
      from: 0,
      lengths: &lengths,
      streamable: true,
    };
    let built = &mut history.built;
    built
      .index
      .keep_sized(&mut built.store, log, sizes)
      .unwrap();
  }
  let named = history.built.index.rendered().unwrap();
  let mut streaming = crate::render::Streaming::default();
  let mut source = history.source();
  let mut out = Vec::new();
  let answered = loop {
    crate::render::answer_want(&mut streaming, &mut source).unwrap();
    match streaming.step(
      &history.built.index,
      &history.built.store,
      &mut room(),
      &mut out,
    ) {
      Ok(true) => {}
      Ok(false) => break Ok(()),
      Err(refused) => break Err(refused),
    }
  };
  let head = format!("0\n{} {}\n", named.files, named.bytes);
  assert!(out.starts_with(head.as_bytes()), "the head written first");
  assert!(
    u64::try_from(out.len()).unwrap()
      <= u64::try_from(head.len()).unwrap() + named.bytes + 64 * named.files,
    "no byte past what the head named"
  );
  answered
}
