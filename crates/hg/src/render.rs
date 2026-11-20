//! A repository's index rendered as the files of a stock Mercurial 7.2 repository (stream clones part 1; §4.17.3
//! "Pull and clone"; `internals.revlogs` \[B\]; `revlog.py`, `store.py`, `phases.py`, `bookmarks.py` \[C\]): what
//! `stream_out` and a `stream2` part send (parts 2 and 3), and what stock `hg verify` reads back as a whole repository.
//!
//! **The shape.** `.hg/requires` names a store 7.2 makes by default without `share-safe`: `dotencode`, `fncache`,
//! `generaldelta`, `revlogv1`, `sparserevlog` and `store`. Every log is a revlog version 1 with generaldelta, split (an
//! index file of 64-byte entries and a data file), never inline: 7.2 reads either, and one shape keeps the renderer one
//! path. Each revision is stored whole (its delta base itself, a full snapshot, which `sparserevlog` allows), its chunk
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
/// Format: a version 1 index entry's bytes: the node's field is 32 bytes, its last 12 zero.
const ENTRY_BYTES: usize = 64;
/// Format: the node field's padding after the node.
const NODE_PADDING: [u8; 12] = [0; 12];
/// Format: the bits of an index entry's first field the flags take, below the offset (`offset << 16 | flags`).
const FLAGS_BITS: u32 = 16;
/// Format: the chunk header of a text stored as it is (`revlog.compress`'s `u`).
const UNCOMPRESSED: u8 = b'u';
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
    let stored = match text.first() {
      None | Some(0) => 0,
      Some(_) => {
        data.push(UNCOMPRESSED);
        1
      }
    };
    data.extend_from_slice(&text);
    let entry = Entry {
      offset,
      flags: kept.flags,
      chunk: text.len().saturating_add(stored),
      text: text.len(),
      rev,
      link,
      parents,
      node: kept.node,
    };
    entry.write(&mut entries)?;
  }
  if let Some(header) = entries.get_mut(..HEADER.len()) {
    header.copy_from_slice(&HEADER);
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
  /// Counting the files and their bytes, which the answer's head names first: the log and its revision next.
  Counting(Walked, u32),
  /// The head: `0`, then the files' count and bytes.
  Head,
  /// A log's index file: its next revision, and its data file's bytes before it.
  Index(Walked, u32, u64),
  /// A log's data file: its next revision.
  Data(Walked, u32),
  /// Ended.
  Done,
}

/// `stream_out`'s answer made a row at a time (A-745; §4.17.1 "A fetch's answer"): what [`revlogs`] renders whole,
/// written as it is made. The answer names its files' count and bytes first, so a first walk counts them from the
/// index's rows alone (a revision's chunk is its text's length and, unless the text is empty or begins with a NUL, the
/// one byte of `u`), a row a step; then each log in [`revlogs`]' order is written, its index file's entries a row a
/// step (each chunk's offset kept as the walk goes, so the data file's size is known when its line is due), then its
/// data file's chunks a row a step, a data file with no bytes left out as 7.2 leaves an empty file out. What the walk
/// holds is its place and two counts; a step holds one revision's text.
#[derive(Debug)]
pub struct Streaming {
  /// Where it stands.
  stage: Stage,
  /// The files counted, and their bytes.
  counted: (u64, u64),
  /// A log's index file bytes and data file bytes, as the count walks it.
  walking: (u64, u64),
}

impl Default for Streaming {
  fn default() -> Self {
    Streaming {
      stage: Stage::Counting(Walked::File(Vec::new()), 0),
      counted: (0, 0),
      walking: (0, 0),
    }
  }
}

impl Streaming {
  /// Appends the answer's next part to `out`, what a step holds charged to `admission`: whether more follow. A refused
  /// step leaves the walk where it was.
  pub fn step(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    admission: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refused> {
    let stage = self.stage.clone();
    self.stage = match stage {
      Stage::Counting(log, rev) => self.count(index, store, (log, rev))?,
      Stage::Head => {
        // Format: `0\n`, then the two counts, a space between and a newline after: two decimals and four bytes.
        admission.charge(DECIMAL_BYTES.saturating_mul(2).saturating_add(4))?;
        out.extend_from_slice(b"0\n");
        out.extend_from_slice(crate::decimal::Decimal::unsigned(self.counted.0).as_bytes());
        out.push(b' ');
        out.extend_from_slice(crate::decimal::Decimal::unsigned(self.counted.1).as_bytes());
        out.push(b'\n');
        first_log(index, store).map_or(Stage::Done, |log| Stage::Index(log, 0, 0))
      }
      Stage::Index(log, rev, offset) => {
        index_entry(index, store, (log, rev, offset), admission, out)?
      }
      Stage::Data(log, rev) => data_chunk(index, store, (log, rev), admission, out)?,
      Stage::Done => return Ok(false),
    };
    Ok(self.stage != Stage::Done)
  }

  /// One row of the count, or a log's end: its files and bytes counted.
  fn count(
    &mut self,
    index: &HgIndex,
    store: &ContentStore,
    (log, rev): (Walked, u32),
  ) -> Result<Stage, Refused> {
    let row = match &log {
      Walked::File(path) => index
        .file_revision_from(store, (path, rev))
        .map(|(found, number, _)| (Walked::File(found), number)),
      Walked::Manifest | Walked::Changelog => {
        (rev < index.len(store, log.log())).then(|| (log.clone(), rev))
      }
    };
    let (found, number) = match row {
      Some((found, number)) if found == log || rev == 0 => (found, number),
      _ => {
        self.close_count();
        return Ok(
          next_log(&log, index, store).map_or(Stage::Head, |next| Stage::Counting(next, 0)),
        );
      }
    };
    if found != log {
      // A file log after the one counted: the one counted ends here.
      self.close_count();
      return Ok(Stage::Counting(found, 0));
    }
    let chunk = chunk_len(index, store, (found.log(), number)).ok_or(Refused::Unreadable {
      log: unreadable_name(&found),
      rev: number,
    })?;
    self.walking.0 = self.walking.0.saturating_add(to_u64(ENTRY_BYTES));
    self.walking.1 = self.walking.1.saturating_add(chunk);
    Ok(Stage::Counting(found, number.saturating_add(1)))
  }

  /// A log's count ended: its index file, and its data file where it has bytes.
  fn close_count(&mut self) {
    let (entries, data) = core::mem::take(&mut self.walking);
    for bytes in [entries, data] {
      if bytes > 0 {
        self.counted.0 = self.counted.0.saturating_add(1);
        self.counted.1 = self.counted.1.saturating_add(bytes);
      }
    }
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

/// A revision's chunk length in its data file, from its row alone: its text's length, and one byte more for the `u`
/// that marks a text stored as it is, unless the text is empty or begins with a NUL. A changeset's text begins with its
/// manifest's hexadecimal node, a manifest's with a path, and a file revision's with a metadata block's marker where it
/// has one, none of them a NUL; a file revision's bytes otherwise begin as the store's first window of them does.
fn chunk_len(index: &HgIndex, store: &ContentStore, (log, rev): (Log<'_>, u32)) -> Option<u64> {
  let (len, leading) = match log {
    Log::Changelog => (index.changeset(store, rev)?.len, None),
    Log::Manifest => (index.revision(store, log, rev)?.len, None),
    Log::File(_) => {
      let row = index.revision(store, log, rev)?;
      let leading = match row.block {
        Some(_) => None,
        None => store
          .windows(&row.bytes)
          .next()
          .and_then(|window| window.first().copied()),
      };
      (row.len, leading)
    }
  };
  let marked = len > 0 && leading != Some(0);
  Some(len.saturating_add(u64::from(marked)))
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

/// The index file's next entry (its line first, at its first), or the index file's end and the data file next.
fn index_entry(
  index: &HgIndex,
  store: &ContentStore,
  (log, rev, offset): (Walked, u32, u64),
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<Stage, Refused> {
  let count = index.len(store, log.log());
  if rev >= count {
    if offset == 0 {
      return Ok(next_log(&log, index, store).map_or(Stage::Done, |next| Stage::Index(next, 0, 0)));
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
  let (parents, link) = numbers(index, store, log.log(), rev).ok_or_else(unreadable)?;
  let chunk = chunk_len(index, store, (log.log(), rev)).ok_or_else(unreadable)?;
  let (node, flags, text) = match log.log() {
    Log::Changelog => {
      let row = index.changeset(store, rev).ok_or_else(unreadable)?;
      (row.node, row.flags, row.len)
    }
    other => {
      let row = index.revision(store, other, rev).ok_or_else(unreadable)?;
      (row.node, row.flags, row.len)
    }
  };
  let entry = Entry {
    offset,
    flags,
    chunk: usize::try_from(chunk).map_err(|_| Refused::TooLong)?,
    text: usize::try_from(text).map_err(|_| Refused::TooLong)?,
    rev,
    link,
    parents,
    node,
  };
  let at = out.len();
  entry.write(out)?;
  if rev == 0
    && let Some(header) = out.get_mut(at..at.saturating_add(HEADER.len()))
  {
    header.copy_from_slice(&HEADER);
  }
  Ok(Stage::Index(
    log,
    rev.saturating_add(1),
    offset.saturating_add(chunk),
  ))
}

/// The data file's next chunk, or the log's end and the next log's index file.
fn data_chunk(
  index: &HgIndex,
  store: &ContentStore,
  (log, rev): (Walked, u32),
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<Stage, Refused> {
  if rev >= index.len(store, log.log()) {
    return Ok(next_log(&log, index, store).map_or(Stage::Done, |next| Stage::Index(next, 0, 0)));
  }
  let unreadable = || Refused::Unreadable {
    log: unreadable_name(&log),
    rev,
  };
  let kept = kept(index, store, log.log(), rev).ok_or_else(unreadable)?;
  admission.charge(kept.len.saturating_add(1))?;
  let mut text = Vec::new();
  if write_text(&mut text, store, &kept) != kept.len {
    return Err(unreadable());
  }
  if text.first().is_some_and(|first| *first != 0) {
    out.push(UNCOMPRESSED);
  }
  out.extend_from_slice(&text);
  Ok(Stage::Data(log, rev.saturating_add(1)))
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
  /// The revision's number, its own delta base.
  rev: u32,
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
    for number in [self.rev, self.link, self.parents[0], self.parents[1]] {
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
