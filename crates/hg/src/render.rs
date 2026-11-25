//! A repository's index rendered as the files of a stock Mercurial 7.2 repository (stream clones part 1; §4.17.3
//! "Pull and clone"; `internals.revlogs` \[B\]; `revlog.py`, `store.py`, `phases.py`, `bookmarks.py` \[C\]): what
//! `stream_out` and a `stream2` part send (parts 2 and 3), and what stock `hg verify` reads back as a whole repository.
//!
//! **The shape.** `.hg/requires` names a store 7.2 makes by default without `share-safe`: `dotencode`, `fncache`,
//! `generaldelta`, `revlog-compression-zstd`, `revlogv1`, `sparserevlog` and `store`. Every log is a revlog version 1
//! with generaldelta, inline (its index file alone, each entry followed by its chunk) or split (an index file of 64-byte
//! entries and a data file) exactly where 7.2 keeps it so ([`crate::revlog_chunk::inline`]: any log but the changelog
//! whose chunks hold under 131,072 bytes is inline). A client lists in its fncache the files it was sent, so a split
//! log's data file is always sent, and an inline log's never exists (a split log missing its data file there fails `hg
//! verify`, hg-src's `mercurial/__init__.py`). Each revision is stored whole (its delta base itself, a full snapshot,
//! which `sparserevlog` allows), its chunk as 7.2's `revlog.compress` stores it with its default zstd engine
//! ([`crate::revlog_chunk::chunk`]: one zstd frame where shorter, the text itself where it begins with a NUL, else `u`
//! and the text; none for an empty text). Its entry holds the chunk's offset among the chunks, the revision's flags,
//! the chunk's and the text's lengths, the base, the link (a changeset's own number), the parents' numbers (-1 for
//! none) and the node, all big-endian, the first four bytes the header (version 1 with the generaldelta flag, and the
//! inline flag where inline; the changelog's with neither, as 7.2 writes a new one, each delta in it against the revision
//! before and its entry's base its chain's start). The file logs are named by the dotencode encoding ([`crate::store::encode_path`]) and
//! listed in `store/fncache`, each file it has, as 7.2's opener lists every data file it writes. `store/phaseroots` holds each phase root (`<phase> <node>`), `.hg/bookmarks` each bookmark (`<node>
//! <name>`), and `store/obsstore` the markers after their format's version byte, when there are any.
//!
//! **Bounded.** Every byte rendered is charged to the caller's admission first; a revision the index cannot give back
//! whole is refused by its log and number.
//!
//! **Compressed as stock's** (goal 93 perf, main's ruling of 2026-11-25): a stream clone sends the files as they are,
//! and at r400 of Mercurial's history slates' uncompressed, always split logs streamed 1.94x stock's bytes and 1.97x
//! its files (`crates/cli/bench/hg_stream_compare.sh`). Each chunk is compressed as the stream writes it, a count pass
//! and a send pass ([`Streaming`]), until the changeset store keeps each revision's chunk as 7.2 stores it from its
//! arrival (w-hg's piece (b)), which [`crate::revlog_chunk`] serves alike.

use std::collections::{BTreeMap, VecDeque};

use slates_vcs::ContentStore;

use crate::import::record::{kept, write_text};
use crate::index::{HgIndex, Log, NONE};
use crate::node::CollisionAttack;
use crate::phases::PhaseHead;
use crate::read::to_u64;
use crate::revlog_chunk::{self, ChunkRefused, Pass};
use crate::store::{encode_dir, encode_path};
use crate::{Admission, NODE_LEN, Node, PastAdmission};

/// Format: the store's directory from the repository's root.
const STORE: &[u8] = b".hg/store/";
/// Format: a file log's store path's prefix.
const DATA: &[u8] = b"data/";
/// Format: the requirements a rendered store needs of a stream clone's client, as `streamreqs` lists them (7.2's
/// `streamed_requirements`: the store's formats among `.hg/requires`, sorted and joined by commas).
pub const STREAM_REQUIREMENTS: &[u8] =
  b"generaldelta,revlog-compression-zstd,revlogv1,sparserevlog";
/// Format: `.hg/requires`, sorted as 7.2 writes it.
const REQUIRES: &[u8] =
  b"dotencode\nfncache\ngeneraldelta\nrevlog-compression-zstd\nrevlogv1\nsparserevlog\nstore\n";
/// Format: the most bytes a count takes as decimal text (`u64::MAX`'s 20 digits).
const DECIMAL_BYTES: u64 = 20;
/// Format: the index header: revlog version 1 with generaldelta (`FLAG_GENERALDELTA`, bit 17), big-endian.
const HEADER: [u8; 4] = [0, 2, 0, 1];
/// Format: the index header of an inline log: [`HEADER`] with the inline flag (`FLAG_INLINE_DATA`, bit 16), its chunks
/// inside the index file. Module docs, "The shape".
const INLINE_HEADER: [u8; 4] = [0, 3, 0, 1];
/// Format: the changelog's index header: revlog version 1 with no flag, as 7.2 writes a new changelog (`changelog.py`
/// clears `FLAG_GENERALDELTA`, "changelogs don't benefit from generaldelta"; never inline, `may_inline=False`). Without
/// generaldelta a delta is against the revision before, and an entry's base names its delta chain's start.
const CHANGELOG_HEADER: [u8; 4] = [0, 0, 0, 1];
/// Format: a version 1 index entry's bytes: the node's field is 32 bytes, its last 12 zero.
const ENTRY_BYTES: usize = 64;
/// Format: the node field's padding after the node.
const NODE_PADDING: [u8; 12] = [0; 12];
/// Format: the bits of an index entry's first field the flags take, below the offset (`offset << 16 | flags`).
const FLAGS_BITS: u32 = 16;
/// Format: the obsstore's name in a refusal of a marker the index cannot give back.
const OBSSTORE: &[u8] = b"obsstore";
/// Format: a `stream2` file's source byte for the store (7.2's `_srcstore`).
const STORE_SOURCE: u8 = b's';
/// Format: the most bytes an unsigned varint of a `u64` takes (64 bits at seven a byte, rounded up).
const UVARINT_BYTES: u64 = 10;
/// Format: the bits of a varint's byte that carry the value.
const UVARINT_LOW: u64 = 0x7f;
/// Format: the bits a varint's byte carries.
const UVARINT_SHIFT: u32 = 7;
/// Format: the bit of a varint's byte that says another follows.
const UVARINT_MORE: u8 = 0x80;

/// One rendered file: its path from the repository's root, and its bytes.
pub type File = (Vec<u8>, Vec<u8>);

/// Why a rendering was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// A revision the index could not give back whole: its log's path (empty for the changelog, `\0` for the manifest
  /// log) and its number.
  Unreadable {
    /// The log.
    log: Vec<u8>,
    /// The revision's number.
    rev: u32,
  },
  /// A file log's encoded name met a known SHA-1 collision attack.
  Path(CollisionAttack),
  /// A length past what a revlog's entry holds.
  TooLong,
  /// Past the caller's admission.
  Past(PastAdmission),
  /// A chunk could not be compressed ([`crate::revlog_chunk`]).
  Chunk(ChunkRefused),
}

impl From<PastAdmission> for Refused {
  fn from(past: PastAdmission) -> Self {
    Refused::Past(past)
  }
}

impl From<ChunkRefused> for Refused {
  fn from(refused: ChunkRefused) -> Self {
    Refused::Chunk(refused)
  }
}

/// What the repository holds beside its logs: its bookmarks, each name and node, and its phase roots.
pub type Beside<'b> = (&'b [(&'b [u8], Node)], &'b [PhaseHead]);

/// Renders the repository `index` holds (module docs), every byte charged to `admission`.
pub fn render(
  index: &HgIndex,
  store: &ContentStore,
  (bookmarks, roots): Beside<'_>,
  admission: &mut Admission,
) -> Result<Vec<File>, Refused> {
  let mut files = vec![file(b".hg/requires", REQUIRES.to_vec(), admission)?];
  let mut fncache = Vec::new();
  for (store_path, bytes) in revlogs(index, store, admission)? {
    let named = if store_path.starts_with(DATA) {
      admission.charge(to_u64(store_path.len()).saturating_add(1))?;
      fncache.extend_from_slice(&encode_dir(&store_path));
      fncache.push(b'\n');
      encode_path(&store_path, true).map_err(Refused::Path)?
    } else {
      store_path
    };
    files.push(([STORE, named.as_slice()].concat(), bytes));
  }
  files.push((b".hg/store/fncache".to_vec(), fncache));
  let phases: Vec<(Vec<u8>, Node)> = roots
    .iter()
    .map(|root| (format!("{} ", root.phase.number()).into_bytes(), root.node))
    .collect();
  let phases = phases
    .iter()
    .map(|(prefix, node)| (prefix.as_slice(), node, &b""[..]));
  files.push(lines(b".hg/store/phaseroots", phases, admission)?);
  files.push(lines(
    b".hg/bookmarks",
    bookmarks.iter().map(|(name, node)| (&b""[..], node, *name)),
    admission,
  )?);
  if let Some(obsstore) = obsstore(index, store, admission)? {
    files.push((b".hg/store/obsstore".to_vec(), obsstore));
  }
  Ok(files)
}

/// Every log as a revlog, each file by its store path before encoding (`data/<path>.i`, `00manifest.d`), in the order
/// 7.2's `store.walk` lists them for a stream: the file logs by path, then the manifest log, then the changelog, each
/// index file before its data file, which only a split log has (stream clones part 2's `stream_out` sends them so).
pub fn revlogs(
  index: &HgIndex,
  store: &ContentStore,
  admission: &mut Admission,
) -> Result<Vec<File>, Refused> {
  let mut paths: Vec<&[u8]> = index.file_logs(store).collect();
  paths.sort_unstable();
  let mut logs: Vec<(Log<'_>, Vec<u8>)> = paths
    .into_iter()
    .map(|path| (Log::File(path), [DATA, path].concat()))
    .collect();
  logs.push((Log::Manifest, b"00manifest".to_vec()));
  logs.push((Log::Changelog, b"00changelog".to_vec()));
  let mut files = Vec::with_capacity(logs.len().saturating_mul(2));
  for (log, radix) in logs {
    let (index_file, data) = revlog(index, store, log, admission)?;
    files.push(([radix.as_slice(), b".i"].concat(), index_file));
    if let Some(data) = data {
      files.push(([radix.as_slice(), b".d"].concat(), data));
    }
  }
  Ok(files)
}

/// A file of `bytes` at `path`, charged.
fn file(path: &[u8], bytes: Vec<u8>, admission: &mut Admission) -> Result<File, Refused> {
  admission.charge(to_u64(bytes.len()))?;
  Ok((path.to_vec(), bytes))
}

/// A file of lines, each its prefix, a node in hexadecimal, then (when it has one) a space and its name.
fn lines<'l>(
  path: &[u8],
  each: impl Iterator<Item = (&'l [u8], &'l Node, &'l [u8])>,
  admission: &mut Admission,
) -> Result<File, Refused> {
  let mut out = Vec::new();
  for (prefix, node, name) in each {
    let line = [prefix.len(), NODE_LEN, NODE_LEN, name.len(), 2]
      .into_iter()
      .fold(0_usize, usize::saturating_add);
    admission.charge(to_u64(line))?;
    out.extend_from_slice(prefix);
    crate::pushkey::encode_hex(node, &mut out);
    if !name.is_empty() {
      out.push(b' ');
      out.extend_from_slice(name);
    }
    out.push(b'\n');
  }
  Ok((path.to_vec(), out))
}

/// Whether `log` may be inline (any but the changelog: 7.2's `changelog.py` opens it `may_inline=False`).
fn may_inline(log: Log<'_>) -> bool {
  log != Log::Changelog
}

/// One log rendered as a revlog (module docs): its index file, and its data file where it is split.
fn revlog(
  index: &HgIndex,
  store: &ContentStore,
  log: Log<'_>,
  admission: &mut Admission,
) -> Result<(Vec<u8>, Option<Vec<u8>>), Refused> {
  let count = index.len(store, log);
  let (mut entries, mut data, mut ends) = (Vec::new(), Vec::new(), Vec::new());
  for rev in 0..count {
    let unreadable = || Refused::Unreadable {
      log: match log {
        Log::Changelog => Vec::new(),
        Log::Manifest => vec![0],
        Log::File(path) => path.to_vec(),
      },
      rev,
    };
    let (parents, link) = numbers(index, store, log, rev).ok_or_else(unreadable)?;
    let kept = kept(index, store, log, rev).ok_or_else(unreadable)?;
    let mut text = Vec::new();
    // A chunk is at most its text and one header byte ([`revlog_chunk::chunk`]).
    admission.charge(kept.len.saturating_add(to_u64(ENTRY_BYTES + 1)))?;
    if write_text(&mut text, store, &kept) != kept.len {
      return Err(unreadable());
    }
    let offset = to_u64(data.len());
    let chunk = revlog_chunk::chunk(&text, Pass::Send, &mut data)?;
    ends.push(data.len());
    let entry = Entry {
      offset,
      flags: kept.flags,
      chunk: usize::try_from(chunk).map_err(|_| Refused::TooLong)?,
      text: text.len(),
      base: rev,
      link,
      parents,
      node: kept.node,
    };
    entry.write(&mut entries)?;
  }
  let inline = revlog_chunk::inline(may_inline(log), to_u64(data.len()));
  put_header(&mut entries, 0, header_of(log, inline));
  if !inline {
    return Ok((entries, Some(data)));
  }
  // Inline: each entry, then its chunk.
  let mut joined = Vec::with_capacity(entries.len().saturating_add(data.len()));
  let mut begun = 0;
  for (entry, end) in entries.chunks(ENTRY_BYTES).zip(ends) {
    joined.extend_from_slice(entry);
    joined.extend_from_slice(data.get(begun..end).ok_or(Refused::TooLong)?);
    begun = end;
  }
  Ok((joined, None))
}

/// One log of the walk a stream makes (A-745): a file log by its path, the manifest log, then the changelog.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Walked {
  /// A file log.
  File(Vec<u8>),
  /// The manifest log.
  Manifest,
  /// The changelog.
  Changelog,
}

impl Walked {
  /// The index's name of the log.
  fn log(&self) -> Log<'_> {
    match self {
      Walked::File(path) => Log::File(path),
      Walked::Manifest => Log::Manifest,
      Walked::Changelog => Log::Changelog,
    }
  }

  /// Its name in the changeset store's `Texts` section: a file's path, `\0manifest`, `\0changelog` (w-hg's intake).
  fn stored_name(&self) -> Vec<u8> {
    match self {
      Walked::File(path) => path.clone(),
      Walked::Manifest => b"\0manifest".to_vec(),
      Walked::Changelog => b"\0changelog".to_vec(),
    }
  }

  /// Its store path's radix before encoding (`data/<path>`, `00manifest`, `00changelog`).
  fn radix(&self) -> Vec<u8> {
    match self {
      Walked::File(path) => [DATA, path.as_slice()].concat(),
      Walked::Manifest => b"00manifest".to_vec(),
      Walked::Changelog => b"00changelog".to_vec(),
    }
  }
}

/// Where a streamed `stream_out` stands (A-745).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stage {
  /// Not begun: the first log the walk writes is found.
  Start,
  /// Counting the files and their bytes, which the answer's head names first: the log being sized and its next
  /// revision.
  Counting(Walked, u32),
  /// The head: `0`, then the files' count and bytes; or `1`, no stream served.
  Head,
  /// A log's index file: its next revision, and its data file's bytes before it.
  Index(Walked, u32, u64),
  /// A log's data file: its next revision.
  Data(Walked, u32),
  /// Ended.
  Done,
}

/// A place in the changeset store (its `texts::Place`): a payload section by its identity, an offset into it and a
/// length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredPlace {
  /// The payload section's identity.
  pub payload: [u8; 32],
  /// The first byte in it.
  pub offset: u64,
  /// The bytes.
  pub len: u64,
  /// Whether they are a revlog chunk as stock stores it (`slates_store::texts::Form::Chunk`; goal 93, main's ruling
  /// (b)): a stream copies it as it lies, a fetch decodes it; else its delta or whole as it is.
  pub chunk: bool,
}

/// A revision as the changeset store keeps it (its `I` row: the version of a log by its node, as it landed): its full
/// text's identity (the revlog's text, its metadata block with it), the full text its edit is against (none for a
/// whole), and where that edit or whole lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredRevision {
  /// The full text's identity.
  pub text: [u8; 32],
  /// The full text the edit is against, a revision of the same log; none for a whole.
  pub base: Option<[u8; 32]>,
  /// The edit's or the whole's place.
  pub place: StoredPlace,
}

/// What a stream asks of the changeset store before its next step ([`Streaming::wants`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
  /// Logs' revisions by node, each log in its order and the logs in the walk's: a log as the store's `Texts` section
  /// names it (a file's path, `\0manifest`, `\0changelog`) and its revisions' nodes. As many logs as the room the last
  /// step was given holds, one at least, so a stream asks by its bytes and never once a log (r20 on hg-src: about 12,000
  /// asks, the serving shard idle half the stream).
  Rows {
    /// Each log's name and its revisions' nodes, in order.
    logs: Vec<(Vec<u8>, Vec<Node>)>,
  },
  /// These places' bytes, in order.
  Chunks {
    /// The places.
    places: Vec<StoredPlace>,
  },
  /// These full texts, each by its identity, made by the store from its chain (A-767 C1 step 1b: a pull's file revision
  /// whose stored base the client may lack, sent whole against the null base; a lookup's `.hgtags` at the served heads).
  /// The texts together hold at most `most` bytes: the store reads them a piece at a time and stops past it, answering
  /// [`StoreWants::texts_past`], so a hostile or huge text is never read whole.
  Texts {
    /// Each text's identity.
    texts: Vec<[u8; 32]>,
    /// The most bytes the texts together may hold: the asking step's room.
    most: u64,
  },
}

/// What answers a stream's wants at once (a whole answer, [`crate::serve::stream::stream_out`]; a test).
pub trait StoredSource {
  /// Each log's revisions named by its nodes, in order, the logs in order: none for one the store does not keep; none
  /// whole where it cannot answer.
  fn rows(&mut self, logs: &[(Vec<u8>, Vec<Node>)]) -> Option<Vec<Vec<Option<StoredRevision>>>>;
  /// Each place's bytes, in order; none where it cannot answer.
  fn chunks(&mut self, places: &[StoredPlace]) -> Option<Vec<Vec<u8>>>;
}

/// An answer that may ask the changeset store before its next step (a stream's, [`Streaming`]): what an edge answers
/// between steps, over its own channel to the store, so one helper serves every edge.
pub trait StoreWants {
  /// What the next step needs, if anything.
  fn wants(&self) -> Option<&Want>;
  /// The rows a [`Want::Rows`] asked for, a log's in turn.
  fn give_rows(&mut self, rows: Vec<Vec<Option<StoredRevision>>>);
  /// The chunks a [`Want::Chunks`] asked for.
  fn give_chunks(&mut self, chunks: Vec<Vec<u8>>);
  /// The texts a [`Want::Texts`] asked for, in order; an answer that never asks for texts (a stream's) takes none.
  fn give_texts(&mut self, _texts: Vec<Vec<u8>>) {}
  /// The texts a [`Want::Texts`] asked for hold more than its `most` bytes: the store stopped reading them there, and
  /// the asker refuses its answer as past its room. An answer that never asks for texts (a stream's) is told none.
  fn texts_past(&mut self) {}
  /// Whether the asker is reading `.hgtags` for a name's lookup (`crate::serve::lookup::TagsRead`): what the daemon
  /// counts its asks by.
  fn reads_tags(&self) -> bool {
    false
  }
  /// Whether the asker is a push's intake reading a file delta's base (`crate::push::intake::Stepped::Store`): what the
  /// daemon counts its asks by.
  fn reads_bases(&self) -> bool {
    false
  }
  /// Whether the asker's next want reads a changeset's or a manifest's text whole, its window let go once landed (goal
  /// 93, C1 (1) P5; a fetch's `crate::serve::getbundle` `LogTexts`): a text it reads to send or diff, never a revision it
  /// sends whole, so the daemon counts it apart. Read before the want is answered.
  fn reads_logs(&self) -> bool {
    false
  }
}

impl StoreWants for Streaming {
  fn wants(&self) -> Option<&Want> {
    Streaming::wants(self)
  }

  fn give_rows(&mut self, rows: Vec<Vec<Option<StoredRevision>>>) {
    Streaming::give_rows(self, rows);
  }

  fn give_chunks(&mut self, chunks: Vec<Vec<u8>>) {
    Streaming::give_chunks(self, chunks);
  }
}

/// A source that keeps no revision: a stream asked of it is not served (`1\n`). What a whole answer, which cannot wait
/// for the changeset store, is given (`crate::serve::session`): the edges stream a step at a time and ask the store.
#[derive(Clone, Copy, Debug, Default)]
pub struct NotKept;

impl StoredSource for NotKept {
  fn rows(&mut self, logs: &[(Vec<u8>, Vec<Node>)]) -> Option<Vec<Vec<Option<StoredRevision>>>> {
    Some(
      logs
        .iter()
        .map(|(_, nodes)| vec![None; nodes.len()])
        .collect(),
    )
  }

  fn chunks(&mut self, _places: &[StoredPlace]) -> Option<Vec<Vec<u8>>> {
    None
  }
}

/// Answers `streaming`'s want from `source`, at once; whether it had one. A source that cannot answer is refused.
pub fn answer_want(
  streaming: &mut Streaming,
  source: &mut impl StoredSource,
) -> Result<bool, Refused> {
  let Some(want) = streaming.wants().cloned() else {
    return Ok(false);
  };
  match want {
    Want::Rows { logs } => {
      let rows = source.rows(&logs).ok_or(Refused::TooLong)?;
      streaming.give_rows(rows);
    }
    Want::Chunks { places } => {
      let chunks = source.chunks(&places).ok_or(Refused::TooLong)?;
      streaming.give_chunks(chunks);
    }
    // A stream sends each revision as the store keeps it, never a text made whole.
    Want::Texts { .. } => return Err(Refused::TooLong),
  }
  Ok(true)
}

/// The store's rows of a log the walk holds: its number among the logs given (which tags its chunks), each revision's
/// stored form, and the first revision of each text (a base names its text, the entry its revision).
#[derive(Debug)]
struct StoredLog {
  seq: u64,
  log: Walked,
  revisions: Vec<StoredRevision>,
  first: BTreeMap<[u8; 32], u32>,
}

/// `stream_out`'s answer made a step at a time (A-745; §4.17.1 "A fetch's answer"), each revision sent as the changeset
/// store keeps it (goal 93 perf; main's ruling of 2026-11-21 00:30): its stored edit against its stored base, read in
/// place, or its whole where the store keeps one, never a text made whole (a stream of Mercurial's history was 4.3 GiB of
/// whole texts where `hg serve` copies an 84 MB store), each chunk stored as 7.2 stores it ([`crate::revlog_chunk`]). The
/// answer names its files' count and bytes first, so a first walk, the count pass, reads each log's chunks a batch at a
/// time and sizes each one compressed (an entry 64 bytes, a chunk its stored length), which decides whether the log is
/// inline ([`revlog_chunk::inline`]); then each log in [`revlogs`]' order is written, the send pass, its index file's
/// entries a row a step (each entry's base the revision of the log that first holds its base's text), each followed by
/// its chunk where the log is inline, then a split log's data file's chunks, a batch a step, each compressed again and
/// held to the length the count gave it (one compressor, the same bytes: a chunk that differs is refused as unreadable
/// rather than sent past what the head named). A log whose revision the store does not keep (an imported repository's,
/// before its import lands as changesets) serves no stream: the answer is `1\n`, 7.2's "operation forbidden by
/// server", and the client clones. What the walk holds: its place, two counts, each log's chunk lengths from the count
/// to the send (4 bytes a revision, less than the index's own row of it), a batch of logs' rows and a batch of
/// chunks, each as much as the room the last step was given holds (one log, one chunk at least); a chunk batch runs on
/// into the logs held after the one being written, so the store is asked by the stream's bytes, never once a log.
/// The store's rows and chunks are asked for by [`Streaming::wants`] and given back before the next step
/// ([`Streaming::give_rows`], [`Streaming::give_chunks`]).
#[derive(Debug)]
pub struct Streaming {
  /// Where it stands.
  stage: Stage,
  /// The files counted, and their bytes.
  counted: (u64, u64),
  /// What it asks of the store before its next step.
  want: Option<Want>,
  /// The logs the asked rows are for, in order.
  wanted_logs: Vec<Walked>,
  /// The rows of the logs held, the one being walked first.
  held: VecDeque<StoredLog>,
  /// The number the next log given takes.
  next_seq: u64,
  /// The log number and revision each asked chunk is for, in order.
  wanted_chunks: Vec<(u64, u32)>,
  /// A batch of chunks given, each with its log's number and its revision.
  chunks: VecDeque<(u64, u32, Vec<u8>)>,
  /// Whether a revision the store does not keep was met: no stream is served.
  not_served: bool,
  /// The room the last step was given, which sizes the next batch of chunks.
  room_hint: u64,
  /// The rows' ask being gathered, a window of nodes a step ([`Streaming::rows_held`]).
  gathering: Option<Gathering>,
  /// How each file it sends is framed: `stream_out`'s, or a `stream2` part's.
  framing: Framing,
  /// The logs the count sized and the send has not ended, in the walk's order.
  sized: VecDeque<SizedLog>,
  /// The chunk lengths of the log the count is sizing, so far.
  sizing: Vec<u32>,
  /// The base the last entry written names: for the changelog, its delta chain's start ([`CHANGELOG_HEADER`]).
  chain_base: u32,
}

/// A log the count pass sized, for the send pass: each revision's stored chunk length, their sum, and whether the log is
/// inline.
#[derive(Debug)]
struct SizedLog {
  /// The log.
  log: Walked,
  /// Each revision's stored chunk length, in order.
  chunks: Vec<u32>,
  /// Their sum: the data file's size, or the chunks' share of an inline index file.
  data: u64,
  /// Whether its chunks follow its entries in its index file.
  inline: bool,
}

/// How a stream frames the files it sends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Framing {
  /// `stream_out`'s (7.2's `generatev1`): a head of `0` and the counts, then each file's line, its store path through
  /// `encodedir`, a NUL, its size and a line end, before its bytes.
  #[default]
  V1,
  /// A `stream2` part's payload (7.2's `generatev2` and `_emit2`): no head, since the part's parameters carry the counts;
  /// each file its source (`s`, the store), its name's length and its size as unsigned varints, and its store path
  /// before encoding (the client's store opener encodes it and lists it in its fncache), before its bytes.
  V2,
}

/// Where a stream's count stands ([`Streaming::counted`]): what a `stream2` part's parameters name before its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counted {
  /// Still counting.
  Counting,
  /// A revision the store does not keep was met: no stream is served.
  Unserved,
  /// Counted: the files it sends and their bytes.
  Served {
    /// The files.
    files: u64,
    /// Their bytes.
    bytes: u64,
  },
}

/// A [`Want::Rows`] being gathered (main 01a20182's order of 2026-11-22 10:40: a stream of a 57,054-changeset history
/// spun its shard past a second gathering the changelog's nodes in one step): the logs gathered whole, their walk, the
/// bytes their rows will take, and the log being gathered, its next revision and its nodes so far.
#[derive(Debug)]
struct Gathering {
  /// Each log gathered: its name and its revisions' nodes, in order.
  logs: Vec<(Vec<u8>, Vec<Node>)>,
  /// Those logs as the walk names them.
  wanted: Vec<Walked>,
  /// What their rows will take.
  bytes: u64,
  /// The log being gathered, its next revision, and its nodes so far.
  at: (Walked, u32, Vec<Node>),
}

impl Default for Streaming {
  fn default() -> Self {
    Streaming {
      stage: Stage::Start,
      counted: (0, 0),
      want: None,
      wanted_logs: Vec::new(),
      held: VecDeque::new(),
      next_seq: 0,
      wanted_chunks: Vec::new(),
      chunks: VecDeque::new(),
      not_served: false,
      room_hint: 0,
      gathering: None,
      framing: Framing::V1,
      sized: VecDeque::new(),
      sizing: Vec::new(),
      chain_base: 0,
    }
  }
}

impl Streaming {
  /// A stream framed as a `stream2` part's payload ([`Framing::V2`]): its steps count first and write nothing until
  /// [`Streaming::counted`] answers, then send each file.
  pub fn v2() -> Self {
    Streaming {
      framing: Framing::V2,
      ..Streaming::default()
    }
  }

  /// Where its count stands: counting until every log is counted; then the files and bytes it sends, or unserved.
  pub fn counted(&self) -> Counted {
    match self.stage {
      Stage::Start | Stage::Counting(..) => Counted::Counting,
      _ if self.not_served => Counted::Unserved,
      _ => Counted::Served {
        files: self.counted.0,
        bytes: self.counted.1,
      },
    }
  }

  /// What the next step needs of the changeset store, if anything: given back by [`Streaming::give_rows`] or
  /// [`Streaming::give_chunks`] before the next step.
  pub fn wants(&self) -> Option<&Want> {
    self.want.as_ref()
  }

  /// The rows a [`Want::Rows`] asked for, a log's in turn, each in order: a revision the store does not keep ends the
  /// stream unserved, and so does a log the answer leaves out.
  pub fn give_rows(&mut self, rows: Vec<Vec<Option<StoredRevision>>>) {
    self.want = None;
    let wanted = core::mem::take(&mut self.wanted_logs);
    if rows.len() != wanted.len() {
      self.not_served = true;
    }
    for (log, rows) in wanted.into_iter().zip(rows) {
      let mut revisions = Vec::with_capacity(rows.len());
      let mut first = BTreeMap::new();
      for (rev, row) in (0_u32..).zip(rows) {
        match row {
          Some(row) => {
            first.entry(row.text).or_insert(rev);
            revisions.push(row);
          }
          None => self.not_served = true,
        }
      }
      self.held.push_back(StoredLog {
        seq: self.next_seq,
        log,
        revisions,
        first,
      });
      self.next_seq = self.next_seq.saturating_add(1);
    }
  }

  /// The chunks a [`Want::Chunks`] asked for, in order.
  pub fn give_chunks(&mut self, chunks: Vec<Vec<u8>>) {
    if let Some(Want::Chunks { .. }) = self.want.take() {
      let tags = core::mem::take(&mut self.wanted_chunks);
      self.chunks = tags
        .into_iter()
        .zip(chunks)
        .map(|((seq, rev), chunk)| (seq, rev, chunk))
        .collect();
    }
  }

  /// Appends the answer's next part to `out`, what a step holds charged to `admission`: whether more follow. A step
  /// that needs the store's rows or chunks writes nothing and says what it wants ([`Streaming::wants`]). A refused step
  /// leaves the walk where it was.
  pub fn step(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    admission: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refused> {
    if self.want.is_some() {
      return Ok(true);
    }
    self.room_hint = admission.left();
    let stage = self.stage.clone();
    self.stage = match stage {
      Stage::Start => first_log(index, store).map_or(Stage::Head, |log| Stage::Counting(log, 0)),
      Stage::Counting(log, rev) => self.count(index, store, (log, rev))?,
      Stage::Head => self.head(index, store, admission, out)?,
      Stage::Index(log, rev, offset) => {
        self.index_entry(index, store, (log, rev, offset), admission, out)?
      }
      Stage::Data(log, rev) => self.data_chunk(index, store, (log, rev), admission, out)?,
      Stage::Done => return Ok(false),
    };
    Ok(self.stage != Stage::Done)
  }

  /// Whether the rows of `log` are held, the logs walked before it let go; asks for them when not, with the logs after
  /// it, as many as the room the last step was given holds (one at least). The ask's nodes are gathered at most a
  /// content window's worth of rows a step ([`rows_a_step`]), so no step reads a whole log's rows, however long.
  fn rows_held(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    log: &Walked,
  ) -> Result<bool, Refused> {
    if self.gathering.is_none() {
      while let Some(front) = self.held.front() {
        if front.log == *log {
          return Ok(true);
        }
        self.held.pop_front();
      }
      self.gathering = Some(Gathering {
        logs: Vec::new(),
        wanted: Vec::new(),
        bytes: 0,
        at: (log.clone(), 0, Vec::new()),
      });
    }
    let row_bytes = to_u64(core::mem::size_of::<StoredRevision>());
    let mut budget = rows_a_step(store);
    let Some(gathering) = self.gathering.as_mut() else {
      return Ok(false);
    };
    loop {
      let (at, rev, nodes) = &mut gathering.at;
      let count = index.len(store, at.log());
      if *rev == 0 {
        let cost = u64::from(count).saturating_mul(row_bytes);
        if !gathering.logs.is_empty() && gathering.bytes.saturating_add(cost) > self.room_hint {
          break;
        }
        gathering.bytes = gathering.bytes.saturating_add(cost);
        nodes
          .try_reserve_exact(usize::try_from(count).map_err(|_| Refused::TooLong)?)
          .map_err(|_| Refused::TooLong)?;
      }
      while *rev < count {
        if budget == 0 {
          return Ok(false);
        }
        nodes.push(node_of(index, store, at, *rev)?);
        *rev = rev.saturating_add(1);
        budget = budget.saturating_sub(1);
      }
      let next = next_log(at, index, store);
      let done = core::mem::take(nodes);
      gathering.logs.push((at.stored_name(), done));
      gathering.wanted.push(at.clone());
      match next {
        Some(next) => gathering.at = (next, 0, Vec::new()),
        None => break,
      }
    }
    if let Some(gathering) = self.gathering.take() {
      self.want = Some(Want::Rows {
        logs: gathering.logs,
      });
      self.wanted_logs = gathering.wanted;
    }
    Ok(false)
  }

  /// One step of the count, once the log's rows are held: its next revision's chunk sized compressed (its stored bytes
  /// asked for a batch at a time); past its last, the log counted, inline as one file of its entries and chunks or split
  /// as its index file and (where it holds any) its data file, then the next log, or the head.
  fn count(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    (log, rev): (Walked, u32),
  ) -> Result<Stage, Refused> {
    if !self.rows_held(index, store, &log)? {
      return Ok(Stage::Counting(log, rev));
    }
    if self.not_served {
      return Ok(Stage::Head);
    }
    let (seq, count) = self.front_log()?;
    if rev < count {
      let Some(plain) = self.chunk_at(seq, rev)? else {
        return Ok(Stage::Counting(log, rev));
      };
      let length = if self.chunk_place(seq, rev) {
        to_u64(plain.len())
      } else {
        revlog_chunk::chunk_len(&plain, Pass::Count)?
      };
      if rev == 0 {
        self.sizing.clear();
      }
      self
        .sizing
        .push(u32::try_from(length).map_err(|_| Refused::TooLong)?);
      return Ok(Stage::Counting(log, rev.saturating_add(1)));
    }
    if log == Walked::Changelog && !self.held.front().is_some_and(changelog_servable) {
      self.not_served = true;
      return Ok(Stage::Head);
    }
    let chunks = core::mem::take(&mut self.sizing);
    let data = chunks
      .iter()
      .map(|length| u64::from(*length))
      .fold(0_u64, u64::saturating_add);
    let inline = revlog_chunk::inline(may_inline(log.log()), data);
    let entries = u64::from(count).saturating_mul(to_u64(ENTRY_BYTES));
    let files = if inline {
      [entries.saturating_add(data), 0]
    } else {
      [entries, data]
    };
    for bytes in files {
      if bytes > 0 {
        self.counted.0 = self.counted.0.saturating_add(1);
        self.counted.1 = self.counted.1.saturating_add(bytes);
      }
    }
    let next = next_log(&log, index, store);
    self.sized.push_back(SizedLog {
      log,
      chunks,
      data,
      inline,
    });
    Ok(next.map_or(Stage::Head, |next| Stage::Counting(next, 0)))
  }

  /// Whether revision `rev` of the log numbered `seq` has its place held as a revlog chunk (goal 93, main's ruling (b)):
  /// sent as it lies, sized by its length, never compressed again.
  fn chunk_place(&self, seq: u64, rev: u32) -> bool {
    self
      .held
      .iter()
      .find(|held| held.seq == seq)
      .and_then(|held| held.revisions.get(usize::try_from(rev).ok()?))
      .is_some_and(|row| row.place.chunk)
  }

  /// Revision `rev`'s chunk of the log numbered `seq` written to `out` from `plain`, its place's bytes: as they lie where
  /// they are a chunk already ([`Streaming::chunk_place`]); else made into one by 7.2's rule (a place kept before the
  /// ruling, or one a store keeps raw). Its length.
  fn stored_into(
    &self,
    (seq, rev): (u64, u32),
    plain: &[u8],
    out: &mut Vec<u8>,
  ) -> Result<u64, Refused> {
    if self.chunk_place(seq, rev) {
      out.extend_from_slice(plain);
      return Ok(to_u64(plain.len()));
    }
    Ok(revlog_chunk::chunk(plain, Pass::Send, out)?)
  }

  /// The number and revision count of the log whose rows are held first.
  fn front_log(&self) -> Result<(u64, u32), Refused> {
    let stored = self.held.front().ok_or(Refused::TooLong)?;
    let count = u32::try_from(stored.revisions.len()).map_err(|_| Refused::TooLong)?;
    Ok((stored.seq, count))
  }

  /// Revision `rev`'s stored bytes in the log numbered `seq` (its delta or its whole, as the store keeps it), when the
  /// batch given holds it next; else none, a batch from it on asked for, running into the logs held after this one: as
  /// many places as fit the room the last step was given, at least one.
  fn chunk_at(&mut self, seq: u64, rev: u32) -> Result<Option<Vec<u8>>, Refused> {
    if self
      .chunks
      .front()
      .is_some_and(|(at_seq, at_rev, _)| (*at_seq, *at_rev) == (seq, rev))
    {
      return Ok(self.chunks.pop_front().map(|(_, _, chunk)| chunk));
    }
    self.chunks.clear();
    let (mut places, mut tags, mut bytes) = (Vec::new(), Vec::new(), 0_u64);
    'logs: for held in &self.held {
      let from = if held.seq == seq { rev } else { 0 };
      for (at, row) in (0_u32..)
        .zip(&held.revisions)
        .skip(usize::try_from(from).map_err(|_| Refused::TooLong)?)
      {
        if !places.is_empty() && bytes.saturating_add(row.place.len) > self.room_hint {
          break 'logs;
        }
        bytes = bytes.saturating_add(row.place.len);
        places.push(row.place);
        tags.push((held.seq, at));
      }
    }
    self.want = Some(Want::Chunks { places });
    self.wanted_chunks = tags;
    Ok(None)
  }

  /// The answer's head: `0` and the counts, or `1` where a revision the store does not keep was met.
  fn head(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    admission: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Stage, Refused> {
    if self.framing == Framing::V2 {
      // A `stream2` part's counts are its parameters, written by its caller once [`Streaming::counted`] answers.
      if self.not_served {
        return Ok(Stage::Done);
      }
      return Ok(first_log(index, store).map_or(Stage::Done, |log| Stage::Index(log, 0, 0)));
    }
    if self.not_served {
      // Format: 7.2's "operation forbidden by server", two bytes.
      admission.charge(2)?;
      out.extend_from_slice(b"1\n");
      return Ok(Stage::Done);
    }
    // Format: `0\n`, then the two counts, a space between and a newline after: two decimals and four bytes.
    admission.charge(DECIMAL_BYTES.saturating_mul(2).saturating_add(4))?;
    out.extend_from_slice(b"0\n");
    out.extend_from_slice(crate::decimal::Decimal::unsigned(self.counted.0).as_bytes());
    out.push(b' ');
    out.extend_from_slice(crate::decimal::Decimal::unsigned(self.counted.1).as_bytes());
    out.push(b'\n');
    Ok(first_log(index, store).map_or(Stage::Done, |log| Stage::Index(log, 0, 0)))
  }

  /// The index file's next entry (its line first, at its first), followed by its chunk where the log is inline; or the
  /// index file's end ([`Streaming::index_end`]).
  fn index_entry(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    (log, rev, offset): (Walked, u32, u64),
    admission: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Stage, Refused> {
    if !self.rows_held(index, store, &log)? {
      return Ok(Stage::Index(log, rev, offset));
    }
    let unreadable = || Refused::Unreadable {
      log: unreadable_name(&log),
      rev,
    };
    // The log's length is its held rows' count: asked of the index, each entry took a search of the log's keys.
    let (seq, count) = self.front_log()?;
    let (inline, data, length) = self.sized_at(&log, rev)?;
    if rev >= count {
      return self.index_end(index, store, (log, inline, data), admission, out);
    }
    let length = u64::from(length.ok_or_else(unreadable)?);
    // An inline log's chunk is had before anything is written, so a step that must ask the store writes nothing.
    let plain = if inline {
      self.chunk_at(seq, rev)?
    } else {
      None
    };
    if inline && plain.is_none() {
      return Ok(Stage::Index(log, rev, offset));
    }
    if rev == 0 {
      let name = [log.radix().as_slice(), b".i"].concat();
      let entries = u64::from(count).saturating_mul(to_u64(ENTRY_BYTES));
      let chunks = plain.as_ref().map_or(0, |_| data);
      file_head(
        self.framing,
        (&name, entries.saturating_add(chunks)),
        admission,
        out,
      )?;
    }
    admission.charge(to_u64(ENTRY_BYTES).saturating_add(plain.as_ref().map_or(0, |_| length)))?;
    let entry = self.entry_of(index, store, (&log, rev, offset), length)?;
    let at = out.len();
    entry.write(out)?;
    if rev == 0 {
      put_header(out, at, header_of(log.log(), inline));
    }
    self.chain_base = entry.base;
    if let Some(plain) = plain
      && self.stored_into((seq, rev), &plain, out)? != length
    {
      return Err(unreadable());
    }
    Ok(Stage::Index(
      log,
      rev.saturating_add(1),
      offset.saturating_add(length),
    ))
  }

  /// What the count gave `log`: whether it is inline, its chunks' bytes, and revision `rev`'s chunk length (none past
  /// its last); refused unreadable where the count sized another log.
  fn sized_at(&self, log: &Walked, rev: u32) -> Result<(bool, u64, Option<u32>), Refused> {
    let sized = self
      .sized
      .front()
      .filter(|sized| sized.log == *log)
      .ok_or_else(|| Refused::Unreadable {
        log: unreadable_name(log),
        rev,
      })?;
    let at = usize::try_from(rev).map_err(|_| Refused::TooLong)?;
    Ok((sized.inline, sized.data, sized.chunks.get(at).copied()))
  }

  /// The index file's end: an inline log (or one with no chunk to send) ends there and the next log begins; a split log's
  /// data file is next, its line first.
  fn index_end(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    (log, inline, data): (Walked, bool, u64),
    admission: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Stage, Refused> {
    if inline || data == 0 {
      self.sized.pop_front();
      return Ok(next_log(&log, index, store).map_or(Stage::Done, |next| Stage::Index(next, 0, 0)));
    }
    let name = [log.radix().as_slice(), b".d"].concat();
    file_head(self.framing, (&name, data), admission, out)?;
    Ok(Stage::Data(log, 0))
  }

  /// Revision `rev`'s entry in `log`, its chunk `length` bytes at `offset` among the chunks: its row's base as the
  /// revision of the log that first holds its base's text, its parents, link, node and flags from the index. A
  /// revision against the null base is refused unreadable unless its place holds its text whole.
  fn entry_of(
    &self,
    index: &HgIndex,
    store: &ContentStore,
    (log, rev, offset): (&Walked, u32, u64),
    length: u64,
  ) -> Result<Entry, Refused> {
    let unreadable = || Refused::Unreadable {
      log: unreadable_name(log),
      rev,
    };
    let stored = self.held.front().ok_or_else(unreadable)?;
    let row = stored
      .revisions
      .get(usize::try_from(rev).map_err(|_| Refused::TooLong)?)
      .ok_or_else(unreadable)?;
    let based = row.base.is_some();
    let base = match row.base {
      None => rev,
      Some(base) => {
        let base = stored
          .first
          .get(&base)
          .copied()
          .filter(|base| *base < rev)
          .ok_or_else(unreadable)?;
        match log.log() {
          // No generaldelta ([`CHANGELOG_HEADER`]): against the revision before, its entry naming the chain's start.
          Log::Changelog if Some(base) == rev.checked_sub(1) => self.chain_base,
          Log::Changelog => return Err(unreadable()),
          _ => base,
        }
      }
    };
    let (parents, link) = numbers(index, store, log.log(), rev).ok_or_else(unreadable)?;
    let (node, flags, text) = match log.log() {
      Log::Changelog => {
        let found = index.changeset(store, rev).ok_or_else(unreadable)?;
        (found.node, found.flags, found.len)
      }
      other => {
        let found = index.revision(store, other, rev).ok_or_else(unreadable)?;
        (found.node, found.flags, found.len)
      }
    };
    // A revision against the null base is sent as its text alone, which a raw place holds whole (A-767 C1); a chunk's
    // place holds its stored chunk, whatever its length (ruling (b)).
    if !based && !row.place.chunk && row.place.len != text {
      return Err(unreadable());
    }
    Ok(Entry {
      offset,
      flags,
      chunk: usize::try_from(length).map_err(|_| Refused::TooLong)?,
      text: usize::try_from(text).map_err(|_| Refused::TooLong)?,
      base,
      link,
      parents,
      node,
    })
  }

  /// A split log's data file's next chunk, its stored bytes (asked for a batch at a time) compressed to the length the
  /// count gave it, or the log's end and the next log's index file.
  fn data_chunk(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    (log, rev): (Walked, u32),
    admission: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Stage, Refused> {
    let unreadable = || Refused::Unreadable {
      log: unreadable_name(&log),
      rev,
    };
    let (seq, count) = self.front_log()?;
    if rev >= count {
      // The next log's rows let go of this one's ([`Streaming::rows_held`]); chunks asked for after it are kept.
      self.sized.pop_front();
      return Ok(next_log(&log, index, store).map_or(Stage::Done, |next| Stage::Index(next, 0, 0)));
    }
    let length = self
      .sized
      .front()
      .filter(|sized| sized.log == log)
      .and_then(|sized| sized.chunks.get(usize::try_from(rev).ok()?).copied())
      .ok_or_else(unreadable)?;
    let Some(plain) = self.chunk_at(seq, rev)? else {
      return Ok(Stage::Data(log, rev));
    };
    admission.charge(u64::from(length))?;
    if self.stored_into((seq, rev), &plain, out)? != u64::from(length) {
      return Err(unreadable());
    }
    Ok(Stage::Data(log, rev.saturating_add(1)))
  }
}

/// The header 7.2 writes for `log`: the changelog's without generaldelta, an inline log's, or a split one's.
fn header_of(log: Log<'_>, inline: bool) -> &'static [u8; 4] {
  if log == Log::Changelog {
    &CHANGELOG_HEADER
  } else if inline {
    &INLINE_HEADER
  } else {
    &HEADER
  }
}

/// Writes `header` over the first four bytes of a log's first entry, written at `at`.
fn put_header(out: &mut [u8], at: usize, header: &[u8; 4]) {
  if let Some(over) = out.get_mut(at..at.saturating_add(header.len())) {
    over.copy_from_slice(header);
  }
}

/// Whether the changelog's stored rows can be sent without generaldelta ([`CHANGELOG_HEADER`]): each delta against
/// the revision before it. Stock 7.2 sends its changelog whole, so a push's rows are; a store holding one delta
/// against another revision serves no stream, and the client clones.
fn changelog_servable(stored: &StoredLog) -> bool {
  (0_u32..).zip(&stored.revisions).all(|(rev, row)| {
    row
      .base
      .is_none_or(|base| stored.first.get(&base).copied() == rev.checked_sub(1))
  })
}

/// A revision's node in `log`.
fn node_of(index: &HgIndex, store: &ContentStore, log: &Walked, rev: u32) -> Result<Node, Refused> {
  let unreadable = || Refused::Unreadable {
    log: unreadable_name(log),
    rev,
  };
  match log.log() {
    Log::Changelog => index
      .changeset(store, rev)
      .map(|row| row.node)
      .ok_or_else(unreadable),
    other => index
      .revision(store, other, rev)
      .map(|row| row.node)
      .ok_or_else(unreadable),
  }
}

/// The first log the walk writes: the first file log, else the manifest log, else the changelog; none for an empty
/// repository.
fn first_log(index: &HgIndex, store: &ContentStore) -> Option<Walked> {
  match index.file_revision_from(store, (&[], 0)) {
    Some((path, _, _)) => Some(Walked::File(path)),
    None => next_log(&Walked::File(Vec::new()), index, store),
  }
}

/// Derived: the rows a stream's step reads to gather an ask ([`Streaming::rows_held`]): a content window's worth of
/// their stored form, the store's own unit of a step (as a long revision is taken a window a step), one at least.
pub(crate) fn rows_a_step(store: &ContentStore) -> u64 {
  to_u64(store.window())
    .checked_div(to_u64(core::mem::size_of::<StoredRevision>()).max(1))
    .unwrap_or(0)
    .max(1)
}

/// The log after `log` in the walk's order that holds a revision.
fn next_log(log: &Walked, index: &HgIndex, store: &ContentStore) -> Option<Walked> {
  let after = match log {
    Walked::File(path) => match index.file_revision_from(store, (path, u32::MAX)) {
      Some((found, _, _)) if found != *path => return Some(Walked::File(found)),
      _ => Walked::Manifest,
    },
    Walked::Manifest => Walked::Changelog,
    Walked::Changelog => return None,
  };
  if index.len(store, after.log()) > 0 {
    Some(after)
  } else {
    next_log(&after, index, store)
  }
}

/// The log's name in a refusal: a file log's path, the manifest log's one byte, the changelog's none.
fn unreadable_name(log: &Walked) -> Vec<u8> {
  match log {
    Walked::File(path) => path.clone(),
    Walked::Manifest => vec![0],
    Walked::Changelog => Vec::new(),
  }
}

/// A file's head in the answer, framed as `framing` frames it, before its bytes.
fn file_head(
  framing: Framing,
  (name, size): (&[u8], u64),
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  match framing {
    Framing::V1 => file_line(name, size, admission, out),
    Framing::V2 => file_entry(name, size, admission, out),
  }
}

/// A file's head in a `stream2` part's payload (7.2's `_emit2`): its source, the store (`s`); its name's length and its
/// size, each an unsigned varint; then its store path before encoding.
pub(crate) fn file_entry(
  name: &[u8],
  size: u64,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  // Format: the source's byte, two varints of at most `UVARINT_BYTES` each, and the name.
  admission.charge(
    to_u64(name.len())
      .saturating_add(UVARINT_BYTES.saturating_mul(2))
      .saturating_add(1),
  )?;
  out.push(STORE_SOURCE);
  put_uvarint(to_u64(name.len()), out);
  put_uvarint(size, out);
  out.extend_from_slice(name);
  Ok(())
}

/// Appends `value` as an unsigned varint (7.2's `util.uvarintencode`): seven bits a byte, least significant first, the
/// high bit set on every byte but the last.
pub(crate) fn put_uvarint(mut value: u64, out: &mut Vec<u8>) {
  loop {
    let low = u8::try_from(value & UVARINT_LOW).unwrap_or(0);
    value >>= UVARINT_SHIFT;
    if value == 0 {
      out.push(low);
      return;
    }
    out.push(low | UVARINT_MORE);
  }
}

/// A file's line in the answer: its store path through `encodedir`, a NUL, its size and a line end.
fn file_line(
  name: &[u8],
  size: u64,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let line = encode_dir(name);
  admission.charge(
    to_u64(line.len())
      .saturating_add(DECIMAL_BYTES)
      .saturating_add(2),
  )?;
  out.extend_from_slice(&line);
  out.push(0);
  out.extend_from_slice(crate::decimal::Decimal::unsigned(size).as_bytes());
  out.push(b'\n');
  Ok(())
}

/// A revision's parents' numbers in its log and its link (a changeset's own number), `None` where the index lacks it.
fn numbers(
  index: &HgIndex,
  store: &ContentStore,
  log: Log<'_>,
  rev: u32,
) -> Option<([u32; 2], u32)> {
  if log == Log::Changelog {
    let row = index.changeset(store, rev)?;
    return Some(([row.p1, row.p2], rev));
  }
  let row = index.revision(store, log, rev)?;
  Some(([row.p1, row.p2], row.link))
}

/// One version 1 index entry, before its big-endian bytes.
struct Entry {
  /// The chunk's offset in the data file.
  offset: u64,
  /// The revision's flags.
  flags: u16,
  /// The chunk's length.
  chunk: usize,
  /// The text's length.
  text: usize,
  /// The revision its chunk is a delta against, the revision itself for a whole.
  base: u32,
  /// The link's number.
  link: u32,
  /// The parents' numbers, [`NONE`] for none.
  parents: [u32; 2],
  /// The node.
  node: Node,
}

impl Entry {
  /// Appends the entry's 64 bytes.
  fn write(&self, out: &mut Vec<u8>) -> Result<(), Refused> {
    let offset_flags = self
      .offset
      .checked_shl(FLAGS_BITS)
      .filter(|shifted| shifted >> FLAGS_BITS == self.offset);
    let offset_flags = offset_flags.ok_or(Refused::TooLong)? | u64::from(self.flags);
    out.extend_from_slice(&offset_flags.to_be_bytes());
    for length in [self.chunk, self.text] {
      let length = i32::try_from(length).map_err(|_| Refused::TooLong)?;
      out.extend_from_slice(&length.to_be_bytes());
    }
    for number in [self.base, self.link, self.parents[0], self.parents[1]] {
      let number = if number == NONE {
        -1
      } else {
        i32::try_from(number).map_err(|_| Refused::TooLong)?
      };
      out.extend_from_slice(&number.to_be_bytes());
    }
    out.extend_from_slice(&self.node);
    out.extend_from_slice(&NODE_PADDING);
    Ok(())
  }
}

/// The obsstore's bytes: the format's version byte, then each marker's body. The index keeps each marker alone in its
/// obsstore's format, its own version byte first (`import::keep_markers`), so the first marker's byte is the store's;
/// `None` when the repository holds no marker.
fn obsstore(
  index: &HgIndex,
  store: &ContentStore,
  admission: &mut Admission,
) -> Result<Option<Vec<u8>>, Refused> {
  let mut out = Vec::new();
  for number in 0..index.markers_len(store) {
    let unreadable = || Refused::Unreadable {
      log: OBSSTORE.to_vec(),
      rev: u32::try_from(number).unwrap_or(u32::MAX),
    };
    let marker = index.marker(store, number).ok_or_else(unreadable)?;
    let mut alone = Vec::new();
    for window in store.windows(&marker) {
      admission.charge(to_u64(window.len()))?;
      alone.extend_from_slice(&window);
    }
    let (version, body) = alone.split_first().ok_or_else(unreadable)?;
    if out.is_empty() {
      out.push(*version);
    }
    out.extend_from_slice(body);
  }
  Ok((!out.is_empty()).then_some(out))
}
