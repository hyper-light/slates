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
//! changeset's sequence (big-endian, a `u128` and a `u64`, so the order of the bytes is the order of the numbers), whose value
//! is the record's offset and length in the log, its identity, its tool byte and its work; and `P`, then a payload
//! section's identity (BLAKE3 of its bytes), whose value is where those bytes are in the log and their length, so a
//! landed content is read by identity as a staged one is ([`Store::piece`]). Further copies have the log offset
//! appended to that key so forgetting any copy preserves reads from the others (A-767 C3). The other tables of item 3
//! (names, versions, object places, derived values) are each tool's, derived from its sections the same way (C4 to C6).

mod payloads;

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
/// Format: the tag of a standing grant's row (A-767's `G`: the grant the store's directory was opened under, R10).
const GRANTS: u8 = b'G';
/// Format: a grant row's key: the tag and the grant's id.
const GRANT_KEY_BYTES: usize = AFTER_TAG + U64;
/// Format: a grant row's value: when it was first issued, the directory's device and inode.
const GRANT_VALUE_BYTES: usize = U64 + U64 + U64;
/// Format: the BLAKE3 key-derivation context of a standing grant's id, from its directory's device and inode.
const GRANT_ID_CONTEXT: &str = "slates 2026-11-20 store standing grant id";
/// Format: a `u64` field's width.
const U64: usize = size_of::<u64>();
/// Format: a repository id's width (`u128`, A-515).
const U128: usize = size_of::<u128>();
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
pub const PIECE_BYTES: usize = 1 << 16;
/// Format: a changeset row key's sequence, after the tag and the repository.
const K_SEQUENCE: usize = AFTER_TAG + U128;
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
/// Bytes of a changeset row in a memtable, including the engine's entry framing.
const CHANGESET_ENTRY_BYTES: usize = KEY_BYTES + VALUE_BYTES + ROW_OVERHEAD;

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
  /// A sliced landing's step named a landing that is not open: committed, abandoned (its client let it go), or
  /// another's.
  NoLanding,
  /// The owner's queue of landings waiting for an open sliced landing is full (the asker's `OwnerError::Busy`).
  Held,
  /// The store could not be opened when its owner started (goal 93; jupiter's red, 2026-11-21): the owner stays and
  /// answers every request with the open's reason, in its words, rather than ending with no word.
  Unopened(Box<str>),
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
      Self::Held => f.write_str("the store's owner holds as many landings as it may wait"),
      Self::NoLanding => f.write_str("no such landing is open (committed, abandoned or another's)"),
      Self::Unopened(reason) => {
        write!(f, "the version-control store could not be opened: {reason}")
      }
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
  pub repository: u128,
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
  /// The standing grant its directory is written under, as first recorded.
  grant: GrantRow,
  /// The log reads that answered places ([`Store::places_bytes`]): the counter its coalescing is held to.
  place_reads: u64,
  /// The engine reads (a lookup, or a page of a scan) that answered versions by their tool ids
  /// ([`Store::texts_by_tools`]): the counter its scan is held to.
  tool_reads: u64,
}

/// A standing grant as the store recorded it (its `G` row, A-767; R10's record of the human's grant): what a landing's
/// header names as its `grant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantRow {
  /// The grant's id: its directory's device and inode, hashed.
  pub id: u64,
  /// When it was first issued (monotonic ns of the issuing process): kept as first recorded.
  pub issued_ns: u64,
  /// The directory's device.
  pub device: u64,
  /// The directory's inode.
  pub inode: u64,
}

impl Store {
  /// Opens the store in the granted directory `root`, creating its files on first use, and recovers: the engine at its checkpoint, the log replayed from there, and the standing grant
  /// `root` was opened under recorded durably as its `G` row the first time (checkpointed before the store serves), or
  /// found as first recorded every time after.
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
    payloads::index_checkpoint(&mut rows, &log_file, applied, config.page_size)?;
    let mut replayed = Ok(());
    let mut value = Vec::new();
    let mut buffer = Vec::new();
    let log = ChangesetLog::recover(log_file, applied, |file, located| {
      if replayed.is_ok() {
        replayed = put_rows_from_log(&mut rows, file, located, (&mut value, &mut buffer));
      }
    })?;
    replayed?;
    let mut store = Self {
      root,
      log,
      rows,
      value,
      place_reads: 0,
      tool_reads: 0,
      grant: GrantRow {
        id: 0,
        issued_ns: 0,
        device: 0,
        inode: 0,
      },
    };
    store.grant = store.record_grant()?;
    Ok(store)
  }

  /// The standing grant the store's directory is written under, as first recorded (its `G` row).
  pub fn grant(&self) -> &GrantRow {
    &self.grant
  }

  /// The log reads that answered places so far ([`Store::places_bytes`]).
  pub fn place_reads(&self) -> u64 {
    self.place_reads
  }

  /// The engine reads that answered versions by their tool ids so far ([`Store::texts_by_tools`]).
  pub fn tool_reads(&self) -> u64 {
    self.tool_reads
  }

  /// Counts one engine read that answered versions by their tool ids.
  pub(crate) fn tool_read(&mut self) {
    self.tool_reads = self.tool_reads.saturating_add(1);
  }

  /// Counts one log read that answered places.
  pub(crate) fn place_read(&mut self) {
    self.place_reads = self.place_reads.saturating_add(1);
  }

  /// Finds the `G` row of the grant the directory was opened under, or puts it and checkpoints the engine so it is
  /// durable before anything lands.
  fn record_grant(&mut self) -> Result<GrantRow, StoreError> {
    let grant = self.root.grant();
    let id = grant_id(grant.target.device, grant.target.inode);
    let mut key = [0u8; GRANT_KEY_BYTES];
    put(&mut key, 0, &[GRANTS]);
    put(&mut key, AFTER_TAG, &id.to_be_bytes());
    if self.rows.get(&key, &mut self.value)? {
      let word = |at: usize| -> Result<u64, StoreError> {
        let bytes = self
          .value
          .get(at..at.saturating_add(U64))
          .ok_or(StoreError::BadRow)?;
        Ok(u64::from_le_bytes(
          bytes.try_into().map_err(|_| StoreError::BadRow)?,
        ))
      };
      return Ok(GrantRow {
        id,
        issued_ns: word(0)?,
        device: word(U64)?,
        inode: word(U64 + U64)?,
      });
    }
    let row = GrantRow {
      id,
      issued_ns: grant.issued_ns,
      device: grant.target.device,
      inode: grant.target.inode,
    };
    let mut value = [0u8; GRANT_VALUE_BYTES];
    put(&mut value, 0, &row.issued_ns.to_le_bytes());
    put(&mut value, U64, &row.device.to_le_bytes());
    put(&mut value, U64 + U64, &row.inode.to_le_bytes());
    self.rows.put(&key, &value)?;
    self.checkpoint()?;
    Ok(row)
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

  /// Opens a landing whose steps come over many requests (the owner's sliced landing): the record's header written
  /// where the log ends, nothing visible until [`Store::commit_landing`]. Refused while one is open.
  pub fn open_landing(&mut self, header: Header) -> Result<(), StoreError> {
    Ok(self.log.open_record(header)?)
  }

  /// Begins the open landing's next section, `len` bytes of `kind`.
  pub fn landing_section(&mut self, kind: SectionKind, len: u64) -> Result<(), StoreError> {
    Ok(self.log.open_section(kind, len)?)
  }

  /// Writes the next `bytes` of the open landing's section.
  pub fn landing_part(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
    Ok(self.log.open_piece(bytes)?)
  }

  /// Commits the open landing (one flush) and applies its rows ([`Store::applied`]).
  pub fn commit_landing(&mut self) -> Result<Located, StoreError> {
    let located = self.log.commit_open()?;
    self.applied(&located)?;
    Ok(located)
  }

  /// Lets the open landing go: the log's end stays where it was, and the next record is written over its bytes.
  pub fn abandon_landing(&mut self) {
    self.log.abandon_open();
  }

  /// Applies a committed record's rows: the step after [`Appending::commit`], which made the record durable. Its sections
  /// are read back from the log, as a recovery's replay reads them, so a changeset landed a slice at a time gets every row
  /// `land` puts (`C`, `P` for each payload, `T`/`H` from its `Texts`; w-perf's finding).
  pub fn applied(&mut self, located: &Located) -> Result<(), StoreError> {
    let mut buffer = Vec::new();
    put_rows_from_log(
      &mut self.rows,
      self.log.file(),
      located,
      (&mut self.value, &mut buffer),
    )
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
    if !put_changeset(&mut self.rows, self.log.file(), &located, &mut self.value)? {
      return Ok(located);
    }
    for (kind, contents) in sections {
      if *kind == SectionKind::Texts {
        let bytes = match contents {
          Contents::Bytes(bytes) => *bytes,
          Contents::Staged(identity) => staging.get(identity).ok_or(StoreError::NotStaged)?,
        };
        crate::texts::put_rows(&mut self.rows, &located.header, bytes)?;
      }
    }
    let mut payload = payloads.iter();
    for section in self.log.sections(&located)? {
      if section.kind == SectionKind::Payload
        && let Some(identity) = payload.next()
      {
        payloads::keep(
          &mut self.rows,
          identity,
          section.offset,
          section.len,
          &mut self.value,
        )?;
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
    let Some((at, landed_len)) = self.content_place(identity)? else {
      return Ok(None);
    };
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

  /// Where the landed content `identity` lies in the log: its offset and length; none where no `P` row names it.
  pub(crate) fn content_place(
    &mut self,
    identity: &Identity,
  ) -> Result<Option<(u64, u64)>, StoreError> {
    if !self.rows.get(&content_key(identity), &mut self.value)? {
      return Ok(None);
    }
    decode_content(&self.value)
      .map(Some)
      .ok_or(StoreError::BadRow)
  }

  /// The engine's rows, for the derived tables beside the store's own (`crate::texts`).
  pub(crate) fn rows_mut(&mut self) -> &mut ShardDb<DeviceFile> {
    &mut self.rows
  }

  /// The changeset log, for reading committed records' sections.
  pub fn log(&self) -> &ChangesetLog<DeviceFile> {
    &self.log
  }

  /// The sections of the landed changeset `row`, in order (its header read from the log).
  pub fn sections(&self, row: &Changeset) -> Result<Vec<crate::log::Section>, StoreError> {
    sections_of(self.log.file(), self.log.end(), row)
  }

  /// At most [`PIECE_BYTES`] of the log from `offset` (a section's bytes, by the place [`Store::sections`] gave), as
  /// far as `len` reaches.
  pub fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>, StoreError> {
    let take =
      usize::try_from(len.min(u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?))
        .map_err(|_| StoreError::BadRow)?;
    let mut bytes = vec![0u8; take];
    self.log.read_at(&mut bytes, offset)?;
    Ok(bytes)
  }

  /// The repository's landed changesets from sequence `from` on, at most `limit`, in order.
  pub fn changesets(
    &mut self,
    repository: u128,
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

  /// Forgets a changeset no register named (A-767 C1, the abandoned push ruling of 2026-11-22).
  /// It removes the changeset row and its payload places, moving the lookup to a surviving copy when needed.
  /// Each text or version this changeset supplied moves to another keeper through `texts::forget_rows`.
  /// The checkpoint at the log's end makes these changes durable. The cleaner can reclaim the abandoned
  /// record's log space. Returns the number of rows changed, or zero if the changeset was already absent.
  ///
  /// # Errors
  /// [`StoreError::BadRow`] for a row or section that does not decode; the log's and the engine's refusals.
  pub fn forget(&mut self, repository: u128, sequence: u64) -> Result<u64, StoreError> {
    let key = changeset_key(repository, sequence);
    let Some(row) = changeset_row(&mut self.rows, &key)? else {
      return Ok(0);
    };
    let mut forgotten = 0_u64;
    let mut buffer = Vec::new();
    for section in self.sections(&row)? {
      match section.kind {
        SectionKind::Payload => {
          let identity =
            identity_of_range(self.log.file(), section.offset, section.len, &mut buffer)?;
          forgotten = forgotten.saturating_add(payloads::forget(
            &mut self.rows,
            &identity,
            section.offset,
            &mut self.value,
          )?);
        }
        SectionKind::Texts => {
          let entries = section_texts(self.log.file(), &section, &mut buffer)?;
          let Store { rows, log, .. } = self;
          let mut entries_of = |rows: &mut ShardDb<DeviceFile>, keeper: u64| {
            texts_of_changeset(rows, log, repository, keeper)
          };
          forgotten = forgotten.saturating_add(crate::texts::forget_rows(
            rows,
            (repository, sequence),
            &entries,
            &mut entries_of,
          )?);
        }
        _ => {}
      }
    }
    self.rows.delete(&key)?;
    self.rows.checkpoint(self.log.end())?;
    Ok(forgotten.saturating_add(1))
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
  let leaf_entries = sizing
    .memory
    .checked_div(CHANGESET_ENTRY_BYTES)
    .and_then(|entries| u64::try_from(entries).ok())
    .filter(|entries| *entries > 0)
    .ok_or(StoreError::Sizing("no memory for a row"))?;
  Ok((
    Config {
      page_size,
      extent_pages,
      max_extents,
    },
    memtable_bytes(sizing.memory),
    TrunkConfig {
      fanout: sizing.fanout,
      leaf_entries,
    },
  ))
}

/// A standing grant's id (its `G` row's key, and what each landing's header names as its `grant`): its directory's
/// device and inode, hashed (so a directory replaced at the same path is another
/// grant's).
pub fn grant_id(device: u64, inode: u64) -> u64 {
  let mut hasher = blake3::Hasher::new_derive_key(GRANT_ID_CONTEXT);
  hasher.update(&device.to_le_bytes());
  hasher.update(&inode.to_le_bytes());
  let hash = hasher.finalize();
  let mut id = [0u8; U64];
  id.copy_from_slice(hash.as_bytes().get(..U64).unwrap_or(&[0u8; U64]));
  u64::from_le_bytes(id)
}

/// The memtable's bytes from the memory lent, `memory`: at most what its arena's 32-bit offsets address (mantle's
/// `BTreeMem::new` refuses a larger arena, "a memtable past 4 GiB"). Until 2026-11-20 the whole loan was the memtable,
/// so a shard's reserve of tens of GiB (a Mac Studio's) refused every landing (A-645 (a)'s run found it).
fn memtable_bytes(memory: usize) -> usize {
  memory.min(usize::try_from(u32::MAX).unwrap_or(usize::MAX))
}

/// The changeset row at `key`, decoded; none where no changeset landed there.
fn changeset_row<F: BlockFile>(
  rows: &mut ShardDb<F>,
  key: &[u8],
) -> Result<Option<Changeset>, StoreError> {
  let mut value = Vec::new();
  if !rows.get(key, &mut value)? {
    return Ok(None);
  }
  decode_changeset(key, &value).map(Some)
}

/// The `Texts` entries of a committed record's section `section`, read whole from `file` into `buffer`.
fn section_texts<F: BlockFile>(
  file: &F,
  section: &crate::log::Section,
  buffer: &mut Vec<u8>,
) -> Result<Vec<crate::texts::TextEntry>, StoreError> {
  buffer.clear();
  buffer.resize(
    usize::try_from(section.len).map_err(|_| StoreError::Sizing("a texts section past memory"))?,
    0,
  );
  file
    .read_exact_at(buffer, section.offset)
    .map_err(LogError::from)?;
  crate::texts::decode(buffer).map_err(|_| StoreError::BadRow)
}

/// The `Texts` entries of repository `repository`'s changeset `sequence`, read back from `log` (none where it landed
/// none, or no such changeset is held).
fn texts_of_changeset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log: &ChangesetLog<F>,
  repository: u128,
  sequence: u64,
) -> Result<Vec<crate::texts::TextEntry>, StoreError> {
  let Some(row) = changeset_row(rows, &changeset_key(repository, sequence))? else {
    return Ok(Vec::new());
  };
  let mut buffer = Vec::new();
  for section in sections_of(log.file(), log.end(), &row)? {
    if section.kind == SectionKind::Texts {
      return section_texts(log.file(), &section, &mut buffer);
    }
  }
  Ok(Vec::new())
}

/// The sections named by a changeset row within a committed prefix of the log.
fn sections_of<F: BlockFile>(
  file: &F,
  end: u64,
  row: &Changeset,
) -> Result<Vec<crate::log::Section>, StoreError> {
  let located = Located {
    offset: row.offset,
    header: crate::log::header_in(file, row.offset, end)?,
    identity: row.identity,
    len: row.len,
    flush_ns: 0,
  };
  Ok(sections_in(file, &located)?)
}

/// The changeset row's key.
fn changeset_key(repository: u128, sequence: u64) -> [u8; KEY_BYTES] {
  let mut key = [0u8; KEY_BYTES];
  put(&mut key, 0, &[CHANGESETS]);
  put(&mut key, AFTER_TAG, &repository.to_be_bytes());
  put(&mut key, K_SEQUENCE, &sequence.to_be_bytes());
  key
}

/// Puts every row of the committed record `located`, its sections read back from `file`: the `C` row, a `P` row for each
/// payload (its identity hashed from the log), the `T`/`H` rows of its `Texts`. One function for a recovery's replay and
/// for [`Store::applied`], so a record is indexed the same whichever way it landed.
fn put_rows_from_log<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  located: &Located,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(), StoreError> {
  if !put_changeset(rows, file, located, value)? {
    return Ok(());
  }
  for section in sections_in(file, located)? {
    if section.kind == SectionKind::Payload {
      let identity = identity_of_range(file, section.offset, section.len, buffer)?;
      payloads::keep(rows, &identity, section.offset, section.len, value)?;
    }
    if section.kind == SectionKind::Texts {
      buffer.clear();
      buffer.resize(
        usize::try_from(section.len)
          .map_err(|_| StoreError::Sizing("a texts section past memory"))?,
        0,
      );
      file
        .read_exact_at(buffer, section.offset)
        .map_err(LogError::from)?;
      crate::texts::put_rows(rows, &located.header, buffer)?;
    }
  }
  Ok(())
}

/// Names this record at its sequence and releases the payload places of any record it replaces.
/// Returns false when a later landing already owns the sequence, so retrying the older apply writes no rows.
fn put_changeset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  located: &Located,
  value: &mut Vec<u8>,
) -> Result<bool, StoreError> {
  let header = &located.header;
  let key = changeset_key(header.repository, header.sequence);
  if rows.get(&key, value)? {
    let previous = decode_changeset(&key, value)?;
    match previous.offset.cmp(&located.offset) {
      std::cmp::Ordering::Greater => return Ok(false),
      std::cmp::Ordering::Equal => return Ok(true),
      std::cmp::Ordering::Less => {
        // A retried creation can reuse a sequence. Only the newly named record can keep payloads there.
        payloads::visit(
          file,
          located.offset,
          &previous,
          &mut Vec::new(),
          |identity, offset, _| {
            payloads::forget(rows, identity, offset, value)?;
            Ok(())
          },
        )?;
      }
    }
  }
  value.clear();
  value.resize(VALUE_BYTES, 0);
  put(value, 0, &located.offset.to_le_bytes());
  put(value, V_LEN, &located.len.to_le_bytes());
  put(value, V_IDENTITY, &located.identity);
  put(value, V_TOOL, &[header.tool.byte()]);
  put(value, V_WORK, &header.work.to_le_bytes());
  rows.put(&key, value)?;
  Ok(true)
}

/// The content row's key of `identity`.
fn content_key(identity: &Identity) -> [u8; CONTENT_KEY_BYTES] {
  let mut key = [0u8; CONTENT_KEY_BYTES];
  put(&mut key, 0, &[CONTENTS]);
  put(&mut key, AFTER_TAG, identity);
  key
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
    repository: field(key, AFTER_TAG)
      .map(u128::from_be_bytes)
      .ok_or(StoreError::BadRow)?,
    sequence: be(K_SEQUENCE)?,
    offset: le(0)?,
    len: le(V_LEN)?,
    identity: field(value, V_IDENTITY).ok_or(StoreError::BadRow)?,
    work: le(V_WORK)?,
  })
}
