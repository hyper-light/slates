//! The store: one granted directory holding the changeset log and the engine that indexes it (A-767 items 3 and 4).
//!
//! **Files.** Both live in the store's directory under its standing grant and are opened only through
//! `slates_land::store_root` (one plain name relative to the granted directory, a new name made durable with the
//! directory): [`LOG_FILE`], the changeset log ([`crate::log`]), and [`ROWS_FILE`], mantle's engine (`ShardDb`,
//! vendored). The store opens no path itself.
//!
//! **A landing** appends the changeset to the log and commits it (one flush), then applies its rows to the engine's
//! memtable: the log is the engine's write-ahead log, so the rows need no flush of their own. A landing is acknowledged
//! once its commit is durable.
//!
//! **Recovery.** The engine's checkpoint records the log offset it applied to; opening the store opens the engine at
//! its checkpoint and replays the log from that offset, applying each committed record's rows again, so the rows never
//! name a changeset the log lacks and lack none it holds.
//!
//! **Rows** (item 3), keyed so a repository's rows are one contiguous range: `C`, then the repository and the
//! changeset's sequence (each a big-endian `u64`, so the order of the bytes is the order of the numbers), whose value
//! is the record's offset and length in the log, its identity, its tool byte and its work; and `P`, then a payload
//! section's identity (BLAKE3 of its bytes), whose value is where those bytes are in the log and their length, so a
//! landed content is read by identity as a staged one is ([`Store::piece`]). The other tables of item 3
//! (names, versions, object places, derived values) are each tool's, derived from its sections the same way (C4 to C6).

use std::fmt;

use hyper_block::block::BlockFile;
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use mantle_engine::rows::Rows;
use mantle_engine::shard_db::ShardDb;
use mantle_engine::store::Config;
use mantle_engine::trunk::TrunkConfig;
use slates_land::store_root::{StoreRoot, StoreRootRefusal};

use std::borrow::Cow;

use crate::log::{
  Appending, ChangesetLog, Header, Located, LogError, SectionKind, identity_of_range, sections_in,
};
use crate::staging::{Identity, Staging};

/// Format: the changeset log's file name in the store's directory.
pub const LOG_FILE: &str = "changesets";
/// Format: the engine's file name in the store's directory.
pub const ROWS_FILE: &str = "rows";
/// Format: the changeset table's tag, the first byte of its keys.
const CHANGESETS: u8 = b'C';
/// Format: the content table's tag: a payload section's identity to its place in the log.
const CONTENTS: u8 = b'P';
/// Format: a `u64` field's width.
const U64: usize = size_of::<u64>();
/// Format: an identity's width (BLAKE3).
const IDENTITY: usize = blake3::OUT_LEN;
/// Format: a row key's first field, after its one-byte tag.
const AFTER_TAG: usize = 1;
/// Format: a content row's key: the tag and the identity.
const CONTENT_KEY_BYTES: usize = AFTER_TAG + IDENTITY;
/// Format: a content row's value: the bytes' offset in the log, then their length.
const CONTENT_VALUE_BYTES: usize = U64 + U64;
/// Format: the most bytes one [`Store::piece`] reads from the log: the read's unit (a caller walks pieces of any length
/// until what it needs is whole), not a tuning of the reader.
const PIECE_BYTES: usize = 1 << 16;
/// Format: a changeset row key's sequence, after the tag and the repository.
const K_SEQUENCE: usize = AFTER_TAG + U64;
/// Format: a changeset row's key: the tag, the repository, the sequence.
const KEY_BYTES: usize = K_SEQUENCE + U64;
/// Format: a changeset row value's length, after the offset.
const V_LEN: usize = U64;
/// Format: a changeset row value's identity, after the length.
const V_IDENTITY: usize = V_LEN + U64;
/// Format: a changeset row value's tool byte, after the identity.
const V_TOOL: usize = V_IDENTITY + IDENTITY;
/// Format: a changeset row value's work, after the tool byte.
const V_WORK: usize = V_TOOL + 1;
/// Format: a changeset row's value.
const VALUE_BYTES: usize = V_WORK + U64;
/// Format: the engine's framing of one entry beyond its key and value (mantle's memtable entry: a type byte and a
/// one-byte length varint for each of a key and a value shorter than 128 bytes, as these rows' are).
const ROW_OVERHEAD: usize = 1 + 1 + 1;

/// What the store may hold, from its caller: the memory its shard lends it and the disk its grant allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizing {
  /// Bytes of memory the engine's memtable and cache may hold (the shard's learned share, never a fraction of the
  /// machine).
  pub memory: usize,
  /// Bytes the engine's file may grow to (the grant's allowance).
  pub disk: u64,
  /// Bytes of one engine extent, the unit its file grows and its compactions write in: the device's transfer at full
  /// bandwidth, which the caller's machine profile measures (mantle derives it so, node.md §2.7).
  pub extent_bytes: u64,
  /// The engine trunk's fanout, measured on the running hardware by the caller (mantle engine-structure.md §4:
  /// "the fanout F and the memtable size m are measured on the running hardware").
  pub fanout: usize,
}

/// Why the store refused.
#[derive(Debug)]
pub enum StoreError {
  /// The store's directory or one of its files.
  Root(StoreRootRefusal),
  /// The block layer.
  Disk(hyper_block::DiskError),
  /// The changeset log.
  Log(LogError),
  /// The engine.
  Engine(mantle_engine::error::Error),
  /// A sizing the engine cannot take (no memory, a disk too small for one extent).
  Sizing(&'static str),
  /// A row that does not decode.
  BadRow,
  /// A section named a staged content that is not staged.
  NotStaged,
}

impl fmt::Display for StoreError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Root(refusal) => write!(f, "{refusal}"),
      Self::Disk(error) => write!(f, "{error}"),
      Self::Log(error) => write!(f, "{error}"),
      Self::Engine(error) => write!(f, "the store's engine: {error}"),
      Self::Sizing(what) => write!(f, "the store's sizing: {what}"),
      Self::BadRow => f.write_str("a store row that does not decode"),
      Self::NotStaged => f.write_str("a section named a content that is not staged"),
    }
  }
}

impl std::error::Error for StoreError {}

impl From<StoreRootRefusal> for StoreError {
  fn from(refusal: StoreRootRefusal) -> Self {
    Self::Root(refusal)
  }
}

impl From<hyper_block::DiskError> for StoreError {
  fn from(error: hyper_block::DiskError) -> Self {
    Self::Disk(error)
  }
}

impl From<LogError> for StoreError {
  fn from(error: LogError) -> Self {
    Self::Log(error)
  }
}

impl From<mantle_engine::error::Error> for StoreError {
  fn from(error: mantle_engine::error::Error) -> Self {
    Self::Engine(error)
  }
}

/// A section's bytes for [`Store::land`]: held by the caller, or a content staged in flight.
#[derive(Clone, Copy, Debug)]
pub enum Contents<'a> {
  /// Bytes the caller holds.
  Bytes(&'a [u8]),
  /// A content staged by identity ([`Staging::stage`]).
  Staged(Identity),
}

/// A landed changeset as its row names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Changeset {
  /// The repository.
  pub repository: u64,
  /// Its sequence.
  pub sequence: u64,
  /// The record's offset in the log.
  pub offset: u64,
  /// The record's length.
  pub len: u64,
  /// Its identity.
  pub identity: [u8; 32],
  /// Its work.
  pub work: u64,
}

/// The store, open.
#[derive(Debug)]
pub struct Store {
  root: StoreRoot,
  log: ChangesetLog<DeviceFile>,
  rows: ShardDb<DeviceFile>,
  /// Reused for every row's value.
  value: Vec<u8>,
}

impl Store {
  /// Opens the store in the granted directory `root`, creating its files on first use, and recovers: the engine at its
  /// checkpoint, then the log's records past it applied again.
  pub fn open(root: StoreRoot, sizing: Sizing) -> Result<Self, StoreError> {
    let log_file = adopt(&root, LOG_FILE)?;
    let rows_existed = root.exists(ROWS_FILE)?;
    let rows_file = adopt(&root, ROWS_FILE)?;
    let (config, mem_limit, trunk) = engine_sizing(&rows_file, sizing)?;
    let (mut rows, applied) = if rows_existed {
      ShardDb::open(rows_file, config, mem_limit, trunk)?
    } else {
      (ShardDb::create(rows_file, config, mem_limit, trunk)?, 0)
    };
    let mut replayed = Ok(());
    let mut value = Vec::new();
    let mut buffer = Vec::new();
    let log = ChangesetLog::recover(log_file, applied, |file, located| {
      if replayed.is_ok() {
        replayed = put_changeset(&mut rows, located, &mut value).and_then(|()| {
          for section in sections_in(file, located)? {
            if section.kind == SectionKind::Payload {
              let identity = identity_of_range(file, section.offset, section.len, &mut buffer)?;
              put_content(&mut rows, &identity, section.offset, section.len)?;
            }
          }
          Ok(())
        });
      }
    })?;
    replayed?;
    Ok(Self {
      root,
      log,
      rows,
      value,
    })
  }

  /// The granted directory.
  pub fn root(&self) -> &StoreRoot {
    &self.root
  }

  /// Begins a landing: the record is appended a section and a piece at a time ([`Appending`]), then committed, then its
  /// rows applied ([`Store::applied`]).
  pub fn begin(&mut self, header: Header) -> Result<Appending<'_, DeviceFile>, StoreError> {
    Ok(self.log.begin(header)?)
  }

  /// Applies a committed record's rows: the step after [`Appending::commit`], which made the record durable.
  pub fn applied(&mut self, located: &Located) -> Result<(), StoreError> {
    put_changeset(&mut self.rows, located, &mut self.value)
  }

  /// Lands a changeset whose sections are `sections`, each bytes the caller holds or a content staged in `staging`
  /// (written from the staged bytes, never copied first): appended, committed (one flush), then its rows applied, a
  /// `P` row for each payload section, named by its identity. The header's section count is the list's.
  pub fn land(
    &mut self,
    mut header: Header,
    sections: &[(SectionKind, Contents<'_>)],
    staging: &Staging,
  ) -> Result<Located, StoreError> {
    header.sections = u16::try_from(sections.len())
      .map_err(|_| StoreError::Sizing("more sections than a record holds"))?;
    let mut payloads: Vec<Identity> = Vec::new();
    let located = {
      let mut record = self.log.begin(header)?;
      for (kind, contents) in sections {
        let bytes = match contents {
          Contents::Bytes(bytes) => *bytes,
          Contents::Staged(identity) => staging.get(identity).ok_or(StoreError::NotStaged)?,
        };
        record.section(
          *kind,
          u64::try_from(bytes.len()).map_err(|_| StoreError::Sizing("a section past u64"))?,
        )?;
        record.piece(bytes)?;
        if *kind == SectionKind::Payload {
          payloads.push(match contents {
            Contents::Staged(identity) => *identity,
            Contents::Bytes(bytes) => *blake3::hash(bytes).as_bytes(),
          });
        }
      }
      record.commit()?
    };
    put_changeset(&mut self.rows, &located, &mut self.value)?;
    let mut payload = payloads.iter();
    for section in self.log.sections(&located)? {
      if section.kind == SectionKind::Payload
        && let Some(identity) = payload.next()
      {
        put_content(&mut self.rows, identity, section.offset, section.len)?;
      }
    }
    Ok(located)
  }

  /// The bytes of the content `identity` (of `len` bytes) from `offset` on, as far as one piece reaches: from `staging`
  /// while it is in flight (borrowed, no copy), else from the log where it landed (a read of at most [`PIECE_BYTES`]).
  /// `None` where neither holds it, or `offset` is at or past its end.
  pub fn piece<'a>(
    &mut self,
    staging: &'a Staging,
    identity: &Identity,
    len: u64,
    offset: u64,
  ) -> Result<Option<Cow<'a, [u8]>>, StoreError> {
    if let Some(bytes) = staging.get(identity) {
      let start = usize::try_from(offset).map_err(|_| StoreError::BadRow)?;
      return Ok(
        bytes
          .get(start..)
          .filter(|rest| !rest.is_empty())
          .map(Cow::Borrowed),
      );
    }
    let mut key = [0u8; CONTENT_KEY_BYTES];
    content_key(identity, &mut key);
    if !self.rows.get(&key, &mut self.value)? {
      return Ok(None);
    }
    let (at, landed_len) = decode_content(&self.value).ok_or(StoreError::BadRow)?;
    if landed_len != len || offset >= landed_len {
      return Ok(None);
    }
    let left = landed_len.checked_sub(offset).ok_or(StoreError::BadRow)?;
    let take =
      usize::try_from(left.min(u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?))
        .map_err(|_| StoreError::BadRow)?;
    let mut bytes = vec![0u8; take];
    self.log.read_at(
      &mut bytes,
      at.checked_add(offset).ok_or(StoreError::BadRow)?,
    )?;
    Ok(Some(Cow::Owned(bytes)))
  }

  /// The changeset log, for reading committed records' sections.
  pub fn log(&self) -> &ChangesetLog<DeviceFile> {
    &self.log
  }

  /// The sections of the landed changeset `row`, in order (its header read from the log).
  pub fn sections(&self, row: &Changeset) -> Result<Vec<crate::log::Section>, StoreError> {
    let header = self.log.header_at(row.offset)?;
    let located = Located {
      offset: row.offset,
      header,
      identity: row.identity,
      len: row.len,
    };
    Ok(self.log.sections(&located)?)
  }

  /// The repository's landed changesets from sequence `from` on, at most `limit`, in order.
  pub fn changesets(
    &mut self,
    repository: u64,
    from: u64,
    limit: usize,
  ) -> Result<Vec<Changeset>, StoreError> {
    let start = changeset_key(repository, from);
    let end = match repository.checked_add(1) {
      Some(next) => changeset_key(next, 0).to_vec(),
      None => vec![CHANGESETS.saturating_add(1)],
    };
    let mut rows = Rows::new();
    let mut next = Vec::new();
    self
      .rows
      .scan(&start, Some(&end), limit, &mut rows, &mut next)?;
    rows
      .iter()
      .map(|(key, value)| decode_changeset(key, value))
      .collect()
  }

  /// Checkpoints the engine at the log's end, so a recovery replays only what lands after.
  pub fn checkpoint(&mut self) -> Result<(), StoreError> {
    self.rows.checkpoint(self.log.end())?;
    Ok(())
  }
}

/// Opens (creating on first use) the store file `name` as a buffered device file.
fn adopt(root: &StoreRoot, name: &str) -> Result<DeviceFile, StoreError> {
  let file = if root.exists(name)? {
    root.open_file(name)?
  } else {
    root.create(name)?
  };
  Ok(DeviceFile::adopt(
    file,
    &root.path_of(name),
    CachingRequest::Buffered,
    Alignment::BYTE,
  )?)
}

/// The engine's shape from the caller's sizing: its page the file's layout block, its extents bounded by the disk
/// allowed, its leaves by the memory lent over a row's bytes.
fn engine_sizing(
  file: &DeviceFile,
  sizing: Sizing,
) -> Result<(Config, usize, TrunkConfig), StoreError> {
  let page_size = file.layout_block().get();
  let page = u64::try_from(page_size).map_err(|_| StoreError::Sizing("a page past u64"))?;
  let extent_pages = sizing
    .extent_bytes
    .checked_div(page)
    .and_then(|pages| u32::try_from(pages).ok())
    .filter(|pages| *pages > 1)
    .ok_or(StoreError::Sizing("an extent under two pages"))?;
  let extent_bytes = page
    .checked_mul(u64::from(extent_pages))
    .ok_or(StoreError::Sizing("an extent past u64"))?;
  let max_extents = sizing
    .disk
    .checked_div(extent_bytes)
    .filter(|extents| *extents > 1)
    .ok_or(StoreError::Sizing(
      "a disk allowance under two extents (the superblocks' and one of rows)",
    ))?;
  let row_bytes = KEY_BYTES
    .saturating_add(VALUE_BYTES)
    .saturating_add(ROW_OVERHEAD);
  let leaf_entries = sizing
    .memory
    .checked_div(row_bytes)
    .and_then(|entries| u64::try_from(entries).ok())
    .filter(|entries| *entries > 0)
    .ok_or(StoreError::Sizing("no memory for a row"))?;
  Ok((
    Config {
      page_size,
      extent_pages,
      max_extents,
    },
    sizing.memory,
    TrunkConfig {
      fanout: sizing.fanout,
      leaf_entries,
    },
  ))
}

/// The changeset row's key.
fn changeset_key(repository: u64, sequence: u64) -> [u8; KEY_BYTES] {
  let mut key = [0u8; KEY_BYTES];
  put(&mut key, 0, &[CHANGESETS]);
  put(&mut key, AFTER_TAG, &repository.to_be_bytes());
  put(&mut key, K_SEQUENCE, &sequence.to_be_bytes());
  key
}

/// Puts the changeset row of the committed record `located`.
fn put_changeset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  located: &Located,
  value: &mut Vec<u8>,
) -> Result<(), StoreError> {
  let header = &located.header;
  value.clear();
  value.resize(VALUE_BYTES, 0);
  put(value, 0, &located.offset.to_le_bytes());
  put(value, V_LEN, &located.len.to_le_bytes());
  put(value, V_IDENTITY, &located.identity);
  put(value, V_TOOL, &[header.tool.byte()]);
  put(value, V_WORK, &header.work.to_le_bytes());
  rows.put(&changeset_key(header.repository, header.sequence), value)?;
  Ok(())
}

/// The content row's key of `identity`.
fn content_key(identity: &Identity, key: &mut [u8; CONTENT_KEY_BYTES]) {
  put(key, 0, &[CONTENTS]);
  put(key, AFTER_TAG, identity);
}

/// Puts the content row: `identity`'s bytes are `len` bytes at `offset` in the log.
fn put_content<F: BlockFile>(
  rows: &mut ShardDb<F>,
  identity: &Identity,
  offset: u64,
  len: u64,
) -> Result<(), StoreError> {
  let mut key = [0u8; CONTENT_KEY_BYTES];
  content_key(identity, &mut key);
  let mut value = [0u8; CONTENT_VALUE_BYTES];
  put(&mut value, 0, &offset.to_le_bytes());
  put(&mut value, U64, &len.to_le_bytes());
  rows.put(&key, &value)?;
  Ok(())
}

/// A content row's value: the offset and length.
fn decode_content(value: &[u8]) -> Option<(u64, u64)> {
  if value.len() != CONTENT_VALUE_BYTES {
    return None;
  }
  Some((u64_at(value, 0)?, u64_at(value, U64)?))
}

/// Writes `bytes` into `out` at `at` (a layout's field; a field past `out` is a layout fault no row reaches).
fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
  if let Some(slot) = at
    .checked_add(bytes.len())
    .and_then(|end| out.get_mut(at..end))
  {
    slot.copy_from_slice(bytes);
  }
}

fn field<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
  bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
  field(bytes, at).map(u64::from_le_bytes)
}

fn decode_changeset(key: &[u8], value: &[u8]) -> Result<Changeset, StoreError> {
  if key.len() != KEY_BYTES || key.first() != Some(&CHANGESETS) || value.len() != VALUE_BYTES {
    return Err(StoreError::BadRow);
  }
  let be = |at: usize| {
    field(key, at)
      .map(u64::from_be_bytes)
      .ok_or(StoreError::BadRow)
  };
  let le = |at: usize| u64_at(value, at).ok_or(StoreError::BadRow);
  Ok(Changeset {
    repository: be(AFTER_TAG)?,
    sequence: be(K_SEQUENCE)?,
    offset: le(0)?,
    len: le(V_LEN)?,
    identity: field(value, V_IDENTITY).ok_or(StoreError::BadRow)?,
    work: le(V_WORK)?,
  })
}
