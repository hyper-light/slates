//! A repository's index rendered as the files of a stock Mercurial 7.2 repository (stream clones part 1; §4.17.3
//! "Pull and clone"; `internals.revlogs` \[B\]; `revlog.py`, `store.py`, `phases.py`, `bookmarks.py` \[C\]): what
//! `stream_out` and a `stream2` part send (parts 2 and 3), and what stock `hg verify` reads back as a whole repository.
//!
//! **The shape.** `.hg/requires` names a store 7.2 makes by default without `share-safe`: `dotencode`, `fncache`,
//! `generaldelta`, `revlogv1`, `sparserevlog` and `store`. Every log is a revlog version 1 with generaldelta, split (an
//! index file of 64-byte entries and a data file), never inline but where the data file would be empty (every text
//! empty): that log is inline, its index file alone, as 7.2 keeps it, since a client lists in its fncache the files it
//! was sent and a split log missing its data file there fails `hg verify` (hg-src's `mercurial/__init__.py`). Each revision is stored whole (its delta base itself, a full snapshot, which `sparserevlog` allows), its chunk
//! as 7.2's `revlog.compress` stores what it does not compress: `u` and the text, or the text itself when it is empty or
//! begins with a NUL. Its entry holds the chunk's offset, the revision's flags, the chunk's and the text's lengths, the
//! base, the link (a changeset's own number), the parents' numbers (-1 for none) and the node, all big-endian, the
//! first four bytes the header (version 1 with the generaldelta flag). The file logs are named by the dotencode
//! encoding ([`crate::store::encode_path`]) and listed in `store/fncache`, both files each, as 7.2's opener lists every
//! data file it writes. `store/phaseroots` holds each phase root (`<phase> <node>`), `.hg/bookmarks` each bookmark (`<node>
//! <name>`), and `store/obsstore` the markers after their format's version byte, when there are any.
//!
//! **Bounded.** Every byte rendered is charged to the caller's admission first; a revision the index cannot give back
//! whole is refused by its log and number.
//!
//! **Not compressed, on purpose for now.** A stream clone sends the files as they are, so a rendered store is as large
//! as its texts; compressing each chunk (7.2's zlib or zstd) and storing deltas are a later part's, with the learned
//! choice the wire's compression makes (§4.17.3).

use std::collections::{BTreeMap, VecDeque};

use slates_vcs::ContentStore;

use crate::import::record::{kept, write_text};
use crate::index::{HgIndex, Log, NONE};
use crate::node::CollisionAttack;
use crate::phases::PhaseHead;
use crate::read::to_u64;
use crate::store::{encode_dir, encode_path};
use crate::{Admission, NODE_LEN, Node, PastAdmission};

/// Format: the store's directory from the repository's root.
const STORE: &[u8] = b".hg/store/";
/// Format: a file log's store path's prefix.
const DATA: &[u8] = b"data/";
/// Format: the requirements a rendered store needs of a stream clone's client, as `streamreqs` lists them (7.2's
/// `streamed_requirements`: the store's formats among `.hg/requires`, sorted and joined by commas).
pub const STREAM_REQUIREMENTS: &[u8] = b"generaldelta,revlogv1,sparserevlog";
/// Format: `.hg/requires`, sorted as 7.2 writes it.
const REQUIRES: &[u8] = b"dotencode\nfncache\ngeneraldelta\nrevlogv1\nsparserevlog\nstore\n";
/// Format: the most bytes a count takes as decimal text (`u64::MAX`'s 20 digits).
const DECIMAL_BYTES: u64 = 20;
/// Format: the index header: revlog version 1 with generaldelta (`FLAG_GENERALDELTA`, bit 17), big-endian.
const HEADER: [u8; 4] = [0, 2, 0, 1];
/// Format: the index header of a log whose every chunk is empty: [`HEADER`] with the inline flag (`FLAG_INLINE_DATA`,
/// bit 16), as 7.2 keeps such a log (its data, none, inside the index file). Module docs, "The shape".
const INLINE_HEADER: [u8; 4] = [0, 3, 0, 1];
/// Format: a version 1 index entry's bytes: the node's field is 32 bytes, its last 12 zero.
const ENTRY_BYTES: usize = 64;
/// Format: the node field's padding after the node.
const NODE_PADDING: [u8; 12] = [0; 12];
/// Format: the bits of an index entry's first field the flags take, below the offset (`offset << 16 | flags`).
const FLAGS_BITS: u32 = 16;
/// Format: the chunk header of a text stored as it is (`revlog.compress`'s `u`).
const UNCOMPRESSED: u8 = b'u';
/// Format: the hunk a changegroup puts before a text it deltas against the null base (7.2's `mdiff.trivialdiffheader`:
/// start 0, end 0 and the text's length, three big-endian 32-bit words). The store keeps such a revision as it came,
/// so a stream sends what follows the hunk, the text, as the revlog's full snapshot: a revlog chunk whose base is its
/// own revision is the text itself, and the hunk sent with it failed stock 7.2's clone ("integrity check failed on
/// 00changelog:0", found by `stock_hg_clone_stream_streams_a_pushed_repository_and_an_imported_one`).
/// An empty text comes with no hunk at all, an empty delta (7.2's `hg debugbundle --all`: an empty file's delta is of
/// length 0), which is sent as it is: refused, it left hg-src's stream empty (`mercurial/__init__.py`'s first revision).
/// Format: three big-endian 32-bit words.
const TRIVIAL_HUNK_BYTES: u64 = 12;
/// Format: the obsstore's name in a refusal of a marker the index cannot give back.
const OBSSTORE: &[u8] = b"obsstore";

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
}

impl From<PastAdmission> for Refused {
  fn from(past: PastAdmission) -> Self {
    Refused::Past(past)
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

/// Every log as a split revlog, each file by its store path before encoding (`data/<path>.i`, `00manifest.d`), in the
/// order 7.2's `store.walk` lists them for a stream: the file logs by path, then the manifest log, then the changelog,
/// each index file before its data file (stream clones part 2's `stream_out` sends them so).
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
    files.push(([radix.as_slice(), b".d"].concat(), data));
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

/// One log rendered as a split revlog: its index file and its data file (module docs).
fn revlog(
  index: &HgIndex,
  store: &ContentStore,
  log: Log<'_>,
  admission: &mut Admission,
) -> Result<(Vec<u8>, Vec<u8>), Refused> {
  let count = index.len(store, log);
  let (mut entries, mut data) = (Vec::new(), Vec::new());
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
    admission.charge(kept.len.saturating_add(to_u64(ENTRY_BYTES + 1)))?;
    if write_text(&mut text, store, &kept) != kept.len {
      return Err(unreadable());
    }
    let offset = to_u64(data.len());
    // The `u` that marks a chunk stored as it is, on every chunk but an empty one: 7.2's reader strips it whatever
    // follows, and the stream marks its chunks alike ([`Streaming`]).
    let stored = if text.is_empty() {
      0
    } else {
      data.push(UNCOMPRESSED);
      1
    };
    data.extend_from_slice(&text);
    let entry = Entry {
      offset,
      flags: kept.flags,
      chunk: text.len().saturating_add(stored),
      text: text.len(),
      base: rev,
      link,
      parents,
      node: kept.node,
    };
    entry.write(&mut entries)?;
  }
  if let Some(header) = entries.get_mut(..HEADER.len()) {
    header.copy_from_slice(if data.is_empty() {
      &INLINE_HEADER
    } else {
      &HEADER
    });
  }
  Ok((entries, data))
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
  /// Counting the files and their bytes, which the answer's head names first: the log next.
  Counting(Walked),
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
  /// whose stored base the client may lack, sent whole against the null base).
  Texts {
    /// Each text's identity.
    texts: Vec<[u8; 32]>,
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
/// whole texts where `hg serve` copies an 84 MB store). The answer names its files' count and bytes first, so a first
/// walk counts them from each log's rows (an entry 64 bytes, a chunk its stored bytes and the `u` that marks a chunk
/// stored as it is, which 7.2's reader strips whatever follows); then each log in [`revlogs`]' order is written, its
/// index file's entries a row a step (each entry's base the revision of the log that first holds its base's text), then
/// its data file's chunks, a batch a step. A log whose revision the store does not keep (an imported repository's,
/// before its import lands as changesets) serves no stream: the answer is `1\n`, 7.2's "operation forbidden by
/// server", and the client clones. What the walk holds: its place, two counts, a batch of logs' rows and a batch of
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
    }
  }
}

impl Streaming {
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
      Stage::Start => first_log(index, store).map_or(Stage::Head, Stage::Counting),
      Stage::Counting(log) => self.count(index, store, log)?,
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
  /// it, as many as the room the last step was given holds (one at least).
  fn rows_held(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    log: &Walked,
  ) -> Result<bool, Refused> {
    while let Some(front) = self.held.front() {
      if front.log == *log {
        return Ok(true);
      }
      self.held.pop_front();
    }
    let row_bytes = to_u64(core::mem::size_of::<StoredRevision>());
    let (mut logs, mut wanted, mut bytes) = (Vec::new(), Vec::new(), 0_u64);
    let mut next = Some(log.clone());
    while let Some(at) = next {
      let count = index.len(store, at.log());
      let cost = u64::from(count).saturating_mul(row_bytes);
      if !logs.is_empty() && bytes.saturating_add(cost) > self.room_hint {
        break;
      }
      bytes = bytes.saturating_add(cost);
      let mut nodes = Vec::with_capacity(usize::try_from(count).map_err(|_| Refused::TooLong)?);
      for rev in 0..count {
        nodes.push(node_of(index, store, &at, rev)?);
      }
      logs.push((at.stored_name(), nodes));
      next = next_log(&at, index, store);
      wanted.push(at);
    }
    self.want = Some(Want::Rows { logs });
    self.wanted_logs = wanted;
    Ok(false)
  }

  /// One log of the count, once its rows are held: its index file and data file counted; then the next log, or the head.
  fn count(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    log: Walked,
  ) -> Result<Stage, Refused> {
    if !self.rows_held(index, store, &log)? {
      return Ok(Stage::Counting(log));
    }
    if self.not_served {
      return Ok(Stage::Head);
    }
    let stored = self.held.front().ok_or(Refused::TooLong)?;
    let entries = to_u64(stored.revisions.len()).saturating_mul(to_u64(ENTRY_BYTES));
    let data = stored.revisions.iter().try_fold(0_u64, |data, row| {
      sent_len(row).map(|sent| data.saturating_add(sent))
    });
    // A place too short for the trivial hunk it must hold: the store's row is not one a stream can send.
    let data = data.ok_or(Refused::Unreadable {
      log: unreadable_name(&log),
      rev: 0,
    })?;
    for bytes in [entries, data] {
      if bytes > 0 {
        self.counted.0 = self.counted.0.saturating_add(1);
        self.counted.1 = self.counted.1.saturating_add(bytes);
      }
    }
    Ok(next_log(&log, index, store).map_or(Stage::Head, Stage::Counting))
  }

  /// The answer's head: `0` and the counts, or `1` where a revision the store does not keep was met.
  fn head(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    admission: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Stage, Refused> {
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

  /// The index file's next entry (its line first, at its first), or the index file's end and the data file next.
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
    // The log's length is its held rows' count: asked of the index, each entry took a search of the log's keys.
    let count = self.held.front().map_or(Ok(0), |stored| {
      u32::try_from(stored.revisions.len()).map_err(|_| Refused::TooLong)
    })?;
    if rev >= count {
      if offset == 0 {
        return Ok(
          next_log(&log, index, store).map_or(Stage::Done, |next| Stage::Index(next, 0, 0)),
        );
      }
      let name = [log.radix().as_slice(), b".d"].concat();
      file_line(&name, offset, admission, out)?;
      return Ok(Stage::Data(log, 0));
    }
    let unreadable = || Refused::Unreadable {
      log: unreadable_name(&log),
      rev,
    };
    if rev == 0 {
      let name = [log.radix().as_slice(), b".i"].concat();
      let size = u64::from(count).saturating_mul(to_u64(ENTRY_BYTES));
      file_line(&name, size, admission, out)?;
    }
    admission.charge(to_u64(ENTRY_BYTES))?;
    let stored = self.held.front().ok_or_else(unreadable)?;
    let row = stored
      .revisions
      .get(usize::try_from(rev).map_err(|_| Refused::TooLong)?)
      .ok_or_else(unreadable)?;
    let based = row.base.is_some();
    let base = match row.base {
      None => rev,
      Some(base) => stored
        .first
        .get(&base)
        .copied()
        .filter(|base| *base < rev)
        .ok_or_else(unreadable)?,
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
    // A revision against the null base is sent as its text alone: its place must be the trivial hunk and the text.
    let whole_text =
      row.place.len == 0 && text == 0 || row.place.len == text.saturating_add(TRIVIAL_HUNK_BYTES);
    let chunk = sent_len(row)
      .filter(|_| based || whole_text)
      .ok_or_else(unreadable)?;
    let entry = Entry {
      offset,
      flags,
      chunk: usize::try_from(chunk).map_err(|_| Refused::TooLong)?,
      text: usize::try_from(text).map_err(|_| Refused::TooLong)?,
      base,
      link,
      parents,
      node,
    };
    let at = out.len();
    entry.write(out)?;
    if rev == 0
      && let Some(header) = out.get_mut(at..at.saturating_add(HEADER.len()))
    {
      // Once a log, at its first entry: inline where no revision has a chunk to send.
      let no_data = stored.revisions.iter().all(|row| sent_len(row) == Some(0));
      header.copy_from_slice(if no_data { &INLINE_HEADER } else { &HEADER });
    }
    Ok(Stage::Index(
      log,
      rev.saturating_add(1),
      offset.saturating_add(chunk),
    ))
  }

  /// The data file's next chunk, its stored bytes as they lie (asked for a batch at a time), or the log's end and the
  /// next log's index file.
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
    let stored = self.held.front().ok_or_else(unreadable)?;
    let count = u32::try_from(stored.revisions.len()).map_err(|_| Refused::TooLong)?;
    if rev >= count {
      // The next log's rows let go of this one's ([`Streaming::rows_held`]); chunks asked for after it are kept.
      return Ok(next_log(&log, index, store).map_or(Stage::Done, |next| Stage::Index(next, 0, 0)));
    }
    let seq = stored.seq;
    if self
      .chunks
      .front()
      .is_none_or(|(at_seq, at_rev, _)| (*at_seq, *at_rev) != (seq, rev))
    {
      // A batch from this revision on, running into the logs held after this one: as many places as fit the room the
      // last step was given, at least one.
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
      return Ok(Stage::Data(log, rev));
    }
    let row = stored
      .revisions
      .get(usize::try_from(rev).map_err(|_| Refused::TooLong)?)
      .ok_or_else(unreadable)?;
    let (_, _, chunk) = self.chunks.pop_front().ok_or_else(unreadable)?;
    let sent = if row.base.is_some() {
      chunk.as_slice()
    } else {
      text_after_trivial_hunk(&chunk).ok_or_else(unreadable)?
    };
    admission.charge(chunk_len(to_u64(sent.len())))?;
    if !sent.is_empty() {
      out.push(UNCOMPRESSED);
      out.extend_from_slice(sent);
    }
    Ok(Stage::Data(log, rev.saturating_add(1)))
  }
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

/// A stored revision's chunk length as a stream sends it ([`chunk_len`]): its stored delta's, or for a delta against
/// the null base its text's, the trivial hunk not sent ([`TRIVIAL_HUNK_BYTES`]; an empty delta, an empty text's, as it
/// is); none for a place too short to hold that hunk.
fn sent_len(row: &StoredRevision) -> Option<u64> {
  let sent = if row.base.is_some() || row.place.len == 0 {
    row.place.len
  } else {
    row.place.len.checked_sub(TRIVIAL_HUNK_BYTES)?
  };
  Some(chunk_len(sent))
}

/// The text a delta against the null base carries ([`TRIVIAL_HUNK_BYTES`]): what follows its one hunk, which must
/// replace bytes 0 to 0 with exactly the rest; an empty delta, an empty text's, as it is; none for anything else.
fn text_after_trivial_hunk(delta: &[u8]) -> Option<&[u8]> {
  if delta.is_empty() {
    return Some(delta);
  }
  let header = usize::try_from(TRIVIAL_HUNK_BYTES).ok()?;
  let (hunk, text) = (delta.get(..header)?, delta.get(header..)?);
  let words: Vec<u32> = hunk
    .as_chunks::<{ size_of::<u32>() }>()
    .0
    .iter()
    .map(|word| u32::from_be_bytes(*word))
    .collect();
  let length = u32::try_from(text.len()).ok()?;
  (words == [0, 0, length]).then_some(text)
}

/// A chunk's length in its data file: its stored bytes and the `u` that marks a chunk stored as it is (7.2's reader
/// strips it whatever follows), none for an empty chunk.
fn chunk_len(stored: u64) -> u64 {
  if stored == 0 {
    0
  } else {
    stored.saturating_add(1)
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
