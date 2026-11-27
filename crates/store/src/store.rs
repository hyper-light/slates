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
//! **Recovery.** [`LOG_FILE`] opens with a [`SEAL_PREAMBLE_BYTES`]-byte `SLSE` preamble naming its format version and
//! 32-byte random secret seal, mirrored as a backup `S` row in [`ROWS_FILE`]. Opening the store inspects [`LOG_FILE`]
//! before touching [`ROWS_FILE`], refusing unsupported formats without mutating any file; when [`ROWS_FILE`] is
//! absent, the engine is recreated and rebuilt with every committed record intact and the log byte-identical from
//! [`LOG_FILE`]'s self-contained preamble and records. Otherwise the engine opens at its checkpoint offset and replays
//! [`LOG_FILE`] from there, so the rows never name a changeset the log lacks and lack none it holds.
//!
//! **Rows** (item 3), keyed so a repository's rows are one contiguous range: `C`, then the repository and the
//! changeset's sequence (big-endian, a `u128` and a `u64`, so the order of the bytes is the order of the numbers), whose value
//! is the record's offset and length in the log, its identity, its tool byte and its work; `P`, then a payload
//! section's identity (BLAKE3 of its bytes), whose value is where those bytes are in the log and their length, so a
//! landed content is read by identity as a staged one is ([`Store::piece`]); and `B`, then a multi-piece (`> PIECE_BYTES`)
//! payload section's log offset and block index (each big-endian `u64`), whose value is its 32-byte keyed BLAKE3 MAC
//! bound to `(seal, section_offset, section_len, block_idx)`, giving O(1)-time and O(1)-RAM per-block authentication on
//! every disk read. Further copies have the log offset appended to the `P` key so forgetting any copy preserves reads
//! from the others (A-767 C3). The other tables of item 3 (names, versions, object places, derived values) are each
//! tool's, derived from its sections the same way (C4 to C6).

mod payloads;

use std::borrow::Cow;
use std::fmt;

use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use hyper_block::block::BlockFile;
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use mantle_engine::rows::Rows;
use mantle_engine::shard_db::ShardDb;
use mantle_engine::store::{Config, MIN_PAGE};
use mantle_engine::trunk::TrunkConfig;
use slates_land::store_root::{StoreRoot, StoreRootRefusal};

use crate::log::{
  Appending, ChangesetLog, Header, Located, LogError, SEAL_BYTES, SEAL_PREAMBLE_BYTES,
  SealPreamble, Section, SectionKind, decode_seal_preamble, encode_seal_preamble,
  has_valid_record_at, sections_in, verified_identity_of_section, verify_seal_matches_log,
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
/// Format: the block-digest table's tag: per-[`PIECE_BYTES`] keyed BLAKE3 MACs for multi-piece payloads.
const BLOCK_DIGESTS: u8 = b'B';
/// Format: the tag of a standing grant's row (A-767's `G`: the grant the store's directory was opened under, R10).
const GRANTS: u8 = b'G';
/// Format: the tag of the store's per-store changeset log seal row (`S`), holding the 32-byte
/// random secret authenticating every record header CRC and keyed BLAKE3 identity in [`LOG_FILE`].
const SEAL_ROW: u8 = b'S';
/// Format: a grant row's key: the tag and the grant's id.
const GRANT_KEY_BYTES: usize = AFTER_TAG + U64;
/// Format: a grant row's value: when it was first issued, the directory's device and inode.
const GRANT_VALUE_BYTES: usize = U64 + U64 + U64;
/// Format: the BLAKE3 key-derivation context of a standing grant's id, from its directory's device and inode.
const GRANT_ID_CONTEXT: &str = "slates 2026-11-20 store standing grant id";
/// Format: domain separator mixed into per-block keyed BLAKE3 MAC rows (`B` rows).
const BLOCK_MAC_DOMAIN: &[u8] = b"slates-store-block-v2\0";
/// Format: a `u64` field's width.
const U64: usize = size_of::<u64>();
/// Format: a repository id's width (`u128`, A-515).
const U128: usize = size_of::<u128>();
/// Format: an identity's width (BLAKE3).
const IDENTITY: usize = blake3::OUT_LEN;
/// Format: a row key's first field, after its one-byte tag.
const AFTER_TAG: usize = 1;
/// Format: a block-digest row key's block index, after the tag and section log offset.
const BK_INDEX: usize = AFTER_TAG + U64;
/// Format: a block-digest row's key: the tag, the section's log offset, and the block index.
const BLOCK_KEY_BYTES: usize = BK_INDEX + U64;
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
/// Format: mantle's minimum trunk fanout and leaf entry count (`mantle_engine/src/trunk/mod.rs`).
const MIN_TRUNK_DEGREE: usize = 2;
/// Fewest engine extents [`Store::open`] requires for a fresh store to complete `ShardDb::create`
/// and its initial grant/seal checkpoint without `LimitExceeded` (`vendor/mantle/engine/src/`):
/// extent 0 (permanent superblock, `store/alloc.rs`), generation-1 allocator map held live until
/// generation 2 commits (`store/mod.rs`), initial flushed branch for `S`/`P`/`B`/`G` rows
/// (`branch/mod.rs`: 1 leaf + 1 filter page <= `extent_pages`), initial trunk image
/// (`trunk/mod.rs`: 1 header + 1 blob page <= `extent_pages`), and generation-2 allocator map
/// written copy-on-write before freeing generation 1's map (`store/mod.rs` `checkpoint`).
///
/// Derived: `1 superblock + 1 gen-1 map + 1 branch + 1 trunk + 1 gen-2 map = 5` extents.
const MIN_STORE_EXTENTS: u64 = 5;

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

/// Non-vacuity and O(1)-work counters updated by verified disk payload reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PieceMetrics {
  /// Verified disk payload reads completed (single-piece or multi-piece).
  verifications: u64,
  /// Total payload bytes fed to BLAKE3 during verified disk reads.
  bytes_hashed: u64,
}

/// The store, open.
#[derive(Debug)]
pub struct Store {
  root: StoreRoot,
  log: ChangesetLog<DeviceFile>,
  rows: ShardDb<DeviceFile>,
  /// Reused for every row's value.
  value: Vec<u8>,
  /// Engine page size derived from the store device's layout block, used for bounded table scans.
  page_size: usize,
  /// The standing grant its directory is written under, as first recorded.
  grant: GrantRow,
  /// Non-vacuity and bytes-hashed counters for verified disk payload reads (A5).
  piece_metrics: PieceMetrics,
  /// Corrupt changeset rows encountered and skipped during checkpoint indexing or row replacement.
  corrupt_rows_skipped: u64,
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
  /// Opens the store in the granted directory `root`, inspecting [`LOG_FILE`]'s `SLSE` preamble
  /// before creating or mutating [`ROWS_FILE`] so unsupported format versions are refused without
  /// touching any file and a deleted `rows` index rebuilds cleanly from the self-contained log.
  /// Records the standing grant `root` was opened under durably as its `G` row on first open.
  pub fn open(root: StoreRoot, sizing: Sizing) -> Result<Self, StoreError> {
    let OpenedStoreFiles {
      log_file,
      mut rows,
      seal,
      applied,
      replay_from,
      page_size,
      seal_written_to_rows,
    } = open_store_files(&root, sizing)?;
    let mut corrupt_rows_skipped =
      payloads::index_checkpoint(&mut rows, &log_file, (applied, &seal), page_size)?;
    let mut replayed = Ok(());
    let mut value = Vec::new();
    let mut buffer = Vec::new();
    let log = ChangesetLog::recover_sealed(log_file, replay_from, seal, None, |file, located| {
      if replayed.is_ok() {
        replayed = put_rows_from_log(
          &mut rows,
          file,
          (&seal, page_size),
          located,
          (&mut value, &mut buffer),
          &mut corrupt_rows_skipped,
        );
      }
    })?;
    replayed?;
    let mut store = Self {
      root,
      log,
      rows,
      value,
      page_size,
      piece_metrics: PieceMetrics::default(),
      corrupt_rows_skipped,
      place_reads: 0,
      tool_reads: 0,
      grant: GrantRow {
        id: 0,
        issued_ns: 0,
        device: 0,
        inode: 0,
      },
    };
    let (grant, checkpointed) = store.record_grant()?;
    store.grant = grant;
    if seal_written_to_rows && !checkpointed {
      store.checkpoint()?;
    }
    Ok(store)
  }

  /// The per-store 32-byte changeset log seal persisted in [`LOG_FILE`]'s `SLSE` preamble and
  /// mirrored in [`ROWS_FILE`]'s `S` row.
  pub fn seal(&self) -> [u8; SEAL_BYTES] {
    self.log.seal()
  }

  /// The standing grant the store's directory is written under, as first recorded (its `G` row).
  pub fn grant(&self) -> &GrantRow {
    &self.grant
  }

  /// Verified disk payload reads completed by [`Store::piece`] / [`Store::places_bytes`] /
  /// [`Store::text_at`] so far (non-vacuity counter).
  pub fn payload_verifications(&self) -> u64 {
    self.piece_metrics.verifications
  }

  /// Verified disk payload reads completed so far (non-vacuity counter).
  pub fn piece_verifications(&self) -> u64 {
    self.piece_metrics.verifications
  }

  /// Total payload bytes hashed through BLAKE3 during verified disk reads (O(1)-per-piece work counter).
  pub fn piece_bytes_hashed(&self) -> u64 {
    self.piece_metrics.bytes_hashed
  }

  /// Corrupt changeset rows logged and skipped during checkpoint indexing or changeset replacement.
  pub fn corrupt_rows_skipped(&self) -> u64 {
    self.corrupt_rows_skipped
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

  /// Finds the `G` row of the grant the directory was opened under, or puts it and checkpoints the
  /// engine so it is durable before anything lands. Returns `(grant, checkpointed)`.
  fn record_grant(&mut self) -> Result<(GrantRow, bool), StoreError> {
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
      return Ok((
        GrantRow {
          id,
          issued_ns: word(0)?,
          device: word(U64)?,
          inode: word(U64 + U64)?,
        },
        false,
      ));
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
    Ok((row, true))
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
    let seal = self.log.seal();
    put_rows_from_log(
      &mut self.rows,
      self.log.file(),
      (&seal, self.page_size),
      located,
      (&mut self.value, &mut buffer),
      &mut self.corrupt_rows_skipped,
    )
  }

  /// Lands a changeset whose sections are `sections`, each bytes the caller holds or a content staged in `staging`
  /// (written from the staged bytes, never copied first): appended, committed (one flush), then its rows applied, a
  /// `P` row (and per-block keyed MAC `B` rows when `> PIECE_BYTES`) for each payload section. The header's section
  /// count is the list's.
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
        let bytes = resolve_contents(contents, staging)?;
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
    self.index_landed_sections(&located, sections, &payloads, staging)?;
    Ok(located)
  }

  /// Indexes the `C`, `T`/`H`/`I`/`K`, `B`, and `P` rows of a record just committed by [`Store::land`].
  fn index_landed_sections(
    &mut self,
    located: &Located,
    sections: &[(SectionKind, Contents<'_>)],
    payloads: &[Identity],
    staging: &Staging,
  ) -> Result<(), StoreError> {
    let seal = self.log.seal();
    if !put_changeset_counted(
      &mut self.rows,
      self.log.file(),
      (&seal, self.page_size),
      located,
      &mut self.value,
      &mut self.corrupt_rows_skipped,
    )? {
      return Ok(());
    }
    for (kind, contents) in sections {
      if *kind == SectionKind::Texts {
        let bytes = resolve_contents(contents, staging)?;
        crate::texts::put_rows(&mut self.rows, &located.header, bytes)?;
      }
    }
    self.index_landed_payloads(located, &seal, sections, payloads, staging)
  }

  /// Writes multi-piece `B` rows and `P` rows for each payload section of a newly landed record.
  fn index_landed_payloads(
    &mut self,
    located: &Located,
    seal: &[u8; SEAL_BYTES],
    sections: &[(SectionKind, Contents<'_>)],
    payloads: &[Identity],
    staging: &Staging,
  ) -> Result<(), StoreError> {
    let mut payload_ids = payloads.iter();
    let mut payload_inputs = sections
      .iter()
      .filter(|(kind, _)| *kind == SectionKind::Payload);
    for section in self.log.sections(located)? {
      if section.kind == SectionKind::Payload
        && let (Some(identity), Some((_, contents))) = (payload_ids.next(), payload_inputs.next())
      {
        let bytes = resolve_contents(contents, staging)?;
        put_block_digests_from_slice(&mut self.rows, seal, section.offset, bytes)?;
        payloads::keep(
          &mut self.rows,
          identity,
          section.offset,
          section.len,
          &mut self.value,
        )?;
      }
    }
    Ok(())
  }

  /// Returns up to [`PIECE_BYTES`] of the content `identity` (expected length `len`) starting at
  /// `offset`, or `None` when neither `staging` nor the log holds it or `offset >= len`.
  ///
  /// While `identity` is staged in flight, the remaining slice from `offset` is borrowed from
  /// `staging` without copying. For a landed payload in the log, every call authenticates every
  /// touched [`PIECE_BYTES`] disk block in O(1) time and O(1) RAM—against `identity` directly when
  /// `len <= PIECE_BYTES` and against the section's durable keyed BLAKE3 `B` rows when
  /// `len > PIECE_BYTES`—returning [`LogError::Corrupt`] on any mismatch or missing digest.
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
    let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
    let take = landed_len
      .checked_sub(offset)
      .ok_or(StoreError::BadRow)?
      .min(piece_cap);
    let bytes = self.read_verified_span(identity, (at, landed_len), (offset, take))?;
    Ok(Some(Cow::Owned(bytes)))
  }

  /// Reads `[span_start, span_start + span_len)` from a landed payload at `(at, landed_len)`,
  /// authenticating every touched [`PIECE_BYTES`] block against `identity` (for single-piece
  /// payloads) or durable keyed `B` rows (for multi-piece payloads) in one log read.
  pub(crate) fn read_verified_span(
    &mut self,
    identity: &Identity,
    place: (u64, u64),
    span: (u64, u64),
  ) -> Result<Vec<u8>, StoreError> {
    read_verified_span_with(
      &self.log,
      &mut self.rows,
      &mut self.piece_metrics,
      identity,
      place,
      span,
    )
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
    let seal = self.log.seal();
    sections_of(self.log.file(), self.log.end(), &seal, row)
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

  /// Reads an entire [`Section`] piece by piece via [`Store::read`] and verifies its accumulated
  /// CRC-32C against `section.crc`, refusing [`LogError::Corrupt`] on any byte mismatch.
  pub fn read_section(&self, section: &Section) -> Result<Vec<u8>, StoreError> {
    let len =
      usize::try_from(section.len).map_err(|_| StoreError::Sizing("a section past memory"))?;
    let mut bytes = Vec::new();
    bytes
      .try_reserve_exact(len)
      .map_err(|_| StoreError::Sizing("a section past memory"))?;
    let mut crc = section.crc_verifier();
    while bytes.len() < len {
      let done = u64::try_from(bytes.len()).map_err(|_| StoreError::BadRow)?;
      let at = section.offset.checked_add(done).ok_or(StoreError::BadRow)?;
      let left = section.len.checked_sub(done).ok_or(StoreError::BadRow)?;
      let piece = self.read(at, left)?;
      if piece.is_empty() {
        return Err(StoreError::Log(LogError::Corrupt("a section's bytes")));
      }
      crc.update(&piece);
      bytes.extend_from_slice(&piece);
    }
    if bytes.len() != len {
      return Err(StoreError::Log(LogError::Corrupt("a section's bytes")));
    }
    crc.finish()?;
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
    let mut buffer = Vec::new();
    forget_changeset(
      &mut self.rows,
      &self.log,
      (repository, sequence),
      self.page_size,
      (&mut self.value, &mut buffer),
    )
  }

  /// Checkpoints the engine at the log's end, so a recovery replays only what lands after.
  pub fn checkpoint(&mut self) -> Result<(), StoreError> {
    self.rows.checkpoint(self.log.end())?;
    Ok(())
  }
}

/// Result of inspecting the leading bytes of an existing [`BlockFile`] log without mutating it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogHead {
  /// The log file is empty (0 bytes).
  Empty,
  /// The log file begins with a valid `FORMAT` `SLSE` preamble carrying this 32-byte seal.
  Sealed([u8; SEAL_BYTES]),
  /// The log file is non-empty and its preamble is damaged or unrecognized (not a foreign format).
  DamagedPreamble,
}

/// Prepared store files and recovery start offset ready for log replay.
struct OpenedStoreFiles<F: BlockFile> {
  log_file: F,
  rows: ShardDb<F>,
  seal: [u8; SEAL_BYTES],
  applied: u64,
  replay_from: u64,
  page_size: usize,
  seal_written_to_rows: bool,
}

/// Inspects the leading bytes of `file` without modifying it. Refuses recognized foreign formats
/// (`SLCS` v1 or `SLSE` != [`crate::log::FORMAT`]) with [`LogError::UnsupportedFormat`].
fn inspect_log_file<F: BlockFile>(file: &F) -> Result<LogHead, StoreError> {
  let len = file.len()?;
  if len == 0 {
    return Ok(LogHead::Empty);
  }
  let cap = u64::try_from(SEAL_PREAMBLE_BYTES).map_err(|_| LogError::TooLarge)?;
  let take = usize::try_from(len.min(cap)).map_err(|_| LogError::TooLarge)?;
  let mut prefix = [0u8; SEAL_PREAMBLE_BYTES];
  let head = prefix.get_mut(..take).ok_or(LogError::TooLarge)?;
  file.read_exact_at(head, 0)?;
  match decode_seal_preamble(head) {
    SealPreamble::Valid(seal) => Ok(LogHead::Sealed(seal)),
    SealPreamble::UnsupportedFormat(found) => {
      Err(StoreError::Log(LogError::UnsupportedFormat(found)))
    }
    SealPreamble::Damaged => Ok(LogHead::DamagedPreamble),
  }
}

/// Reads the backup per-store 32-byte log seal (`S` row) from `rows`, if present.
fn read_seal_row<F: BlockFile>(
  rows: &mut ShardDb<F>,
) -> Result<Option<[u8; SEAL_BYTES]>, StoreError> {
  let mut value = Vec::new();
  if !rows.get(&[SEAL_ROW], &mut value)? {
    return Ok(None);
  }
  value
    .as_slice()
    .try_into()
    .map(Some)
    .map_err(|_| StoreError::BadRow)
}

/// Generates a fresh 32-byte per-store log seal from OS randomness via AWS-LC.
fn generate_seal() -> Result<[u8; SEAL_BYTES], StoreError> {
  let mut seal = [0u8; SEAL_BYTES];
  SystemRandom::new()
    .fill(&mut seal)
    .map_err(|_| StoreError::Sizing("the OS random source refused the store seal"))?;
  Ok(seal)
}

/// Writes the 48-byte `SLSE` preamble at offset 0 of `file` and flushes it durably.
fn write_seal_preamble<F: BlockFile>(file: &F, seal: &[u8; SEAL_BYTES]) -> Result<(), StoreError> {
  let preamble = encode_seal_preamble(seal);
  file.write_all_at(&preamble, 0)?;
  file.sync_data()?;
  Ok(())
}

/// Returns `true` when `rows` holds at least one entry in any table (`B`, `C`, `G`, `H`, `I`,
/// `K`, `P`, `S`, `T`), distinguishing a freshly created empty `rows` index from an unsealed or
/// tampered `rows` file carrying pre-existing rows at `applied == 0`.
fn has_any_rows<F: BlockFile>(rows: &mut ShardDb<F>) -> Result<bool, StoreError> {
  let mut found = Rows::new();
  rows.scan(b"", None, 1, &mut found, &mut Vec::new())?;
  Ok(found.get(0).is_some())
}

/// Pairs an already-sealed `log_file` with `rows`, refusing a truncated log, a mismatched
/// `SEAL_ROW`, or a seal-less `rows` with `applied > 0` or any pre-existing rows without mutating
/// either file, and mirroring `seal` into a fresh empty `rows` when rebuilding.
fn prepare_sealed_log<F: BlockFile>(
  log_file: F,
  seal: [u8; SEAL_BYTES],
  (mut rows, applied, page_size): (ShardDb<F>, u64, usize),
) -> Result<OpenedStoreFiles<F>, StoreError> {
  let preamble_len = u64::try_from(SEAL_PREAMBLE_BYTES).map_err(|_| LogError::TooLarge)?;
  let replay_from = applied.max(preamble_len);
  if log_file.len()? < replay_from {
    return Err(StoreError::Log(LogError::Corrupt(
      "a missing or truncated changeset log",
    )));
  }
  let seal_written_to_rows = match read_seal_row(&mut rows)? {
    Some(existing) if existing != seal => {
      return Err(StoreError::Log(LogError::Corrupt("a store seal mismatch")));
    }
    Some(_) => false,
    None if applied > 0 || has_any_rows(&mut rows)? => {
      return Err(StoreError::Log(LogError::Corrupt("a store seal mismatch")));
    }
    None => {
      rows.put(&[SEAL_ROW], &seal)?;
      true
    }
  };
  Ok(OpenedStoreFiles {
    log_file,
    rows,
    seal,
    applied,
    replay_from,
    page_size,
    seal_written_to_rows,
  })
}

/// Returns `true` when `rows` holds at least one `CHANGESETS` (`C`) row.
fn has_changeset_rows<F: BlockFile>(rows: &mut ShardDb<F>) -> Result<bool, StoreError> {
  let start = [CHANGESETS];
  let end = [CHANGESETS.saturating_add(1)];
  let mut found = Rows::new();
  rows.scan(&start, Some(&end), 1, &mut found, &mut Vec::new())?;
  Ok(found.get(0).is_some())
}

/// Updates `newest` with the largest changeset row offset in `[preamble_len, file_len)` present in
/// one scanned `page`, skipping any malformed `C` row instead of aborting preamble repair.
fn max_row_offset_in_page(page: &Rows, preamble_len: u64, file_len: u64, newest: &mut Option<u64>) {
  for (key, entry) in page.iter() {
    let Ok(row) = decode_changeset(key, entry) else {
      continue;
    };
    if (preamble_len..file_len).contains(&row.offset) {
      *newest = Some(newest.map_or(row.offset, |prev| prev.max(row.offset)));
    }
  }
}

/// Finds the largest changeset row offset in `[preamble_len, file_len)` stored in `rows` to use as
/// an O(1) verification hint when record 0 at `preamble_len` is itself damaged.
fn newest_changeset_offset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  preamble_len: u64,
  file_len: u64,
  page_size: usize,
) -> Result<Option<u64>, StoreError> {
  let per_page = page_size
    .checked_div(CHANGESET_ENTRY_BYTES)
    .unwrap_or(1)
    .max(1);
  let mut start = vec![CHANGESETS];
  let mut next = Vec::new();
  let mut page = Rows::new();
  let end = [CHANGESETS.saturating_add(1)];
  let mut newest: Option<u64> = None;
  loop {
    page.clear();
    let more = rows.scan(&start, Some(&end), per_page, &mut page, &mut next)?;
    max_row_offset_in_page(&page, preamble_len, file_len, &mut newest);
    if !more {
      return Ok(newest);
    }
    std::mem::swap(&mut start, &mut next);
  }
}

/// Verifies that `log_file` genuinely belongs to `rows`'s backup `seal` before repairing a damaged
/// `SLSE` preamble: empty-payload logs require `applied == SEAL_PREAMBLE_BYTES` with zero `C`
/// rows, and non-empty logs check record 0 at `preamble_len` first (zero `C`-row scan on the
/// common path) before falling back to a resilient `C`-row hint and un-filtered linear resync
/// authenticated by the 256-bit keyed BLAKE3 `seal` MAC.
fn verify_damaged_preamble_match<F: BlockFile>(
  log_file: &F,
  rows: &mut ShardDb<F>,
  (file_len, preamble_len, applied, page_size): (u64, u64, u64, usize),
  seal: &[u8; SEAL_BYTES],
) -> Result<(), StoreError> {
  if file_len == preamble_len {
    if applied != preamble_len || has_changeset_rows(rows)? {
      return Err(StoreError::Log(LogError::Corrupt("a store seal preamble")));
    }
    return Ok(());
  }
  if has_valid_record_at(log_file, (preamble_len, file_len), seal)? {
    return Ok(());
  }
  let hint = newest_changeset_offset(rows, preamble_len, file_len, page_size)?;
  if !verify_seal_matches_log(log_file, file_len, seal, hint)? {
    return Err(StoreError::Log(LogError::Corrupt("a store seal preamble")));
  }
  Ok(())
}

/// Recovers a log whose leading `SLSE` preamble is damaged only when an existing `rows` holds the
/// backup `SEAL_ROW`, `log_file` reaches `applied.max(SEAL_PREAMBLE_BYTES)`, and the log itself
/// verifies under that `SEAL_ROW` seal; only then durably repairs the 48-byte preamble on disk so
/// a subsequent loss of `rows` still rebuilds cleanly.
fn prepare_damaged_preamble_log<F: BlockFile>(
  log_file: F,
  existing_rows: Option<(ShardDb<F>, u64, usize)>,
) -> Result<OpenedStoreFiles<F>, StoreError> {
  let preamble_len = u64::try_from(SEAL_PREAMBLE_BYTES).map_err(|_| LogError::TooLarge)?;
  let Some((mut rows, applied, page_size)) = existing_rows else {
    return Err(StoreError::Log(LogError::Corrupt("a store seal preamble")));
  };
  let replay_from = applied.max(preamble_len);
  let file_len = log_file.len()?;
  if file_len < replay_from {
    return Err(StoreError::Log(LogError::Corrupt(
      "a missing or truncated changeset log",
    )));
  }
  let Some(seal) = read_seal_row(&mut rows)? else {
    return Err(StoreError::Log(LogError::Corrupt("a store seal preamble")));
  };
  verify_damaged_preamble_match(
    &log_file,
    &mut rows,
    (file_len, preamble_len, applied, page_size),
    &seal,
  )?;
  write_seal_preamble(&log_file, &seal)?;
  slates_machine::log_line!("slates-store: repaired damaged seal preamble from rows backup");
  Ok(OpenedStoreFiles {
    log_file,
    rows,
    seal,
    applied,
    replay_from,
    page_size,
    seal_written_to_rows: false,
  })
}

/// Initializes an empty `log_file` with a durable `SLSE` preamble, reusing `SEAL_ROW` from `rows`
/// if present or generating fresh OS randomness (refusing a seal-less `rows` with pre-existing
/// rows without mutating either file).
fn prepare_empty_log<F: BlockFile>(
  log_file: F,
  (mut rows, applied, page_size): (ShardDb<F>, u64, usize),
) -> Result<OpenedStoreFiles<F>, StoreError> {
  if applied > 0 {
    return Err(StoreError::Log(LogError::Corrupt(
      "a missing or truncated changeset log",
    )));
  }
  let preamble_len = u64::try_from(SEAL_PREAMBLE_BYTES).map_err(|_| LogError::TooLarge)?;
  let (seal, seal_written_to_rows) = match read_seal_row(&mut rows)? {
    Some(existing) => (existing, false),
    None if has_any_rows(&mut rows)? => {
      return Err(StoreError::Log(LogError::Corrupt("a store seal mismatch")));
    }
    None => {
      let fresh = generate_seal()?;
      rows.put(&[SEAL_ROW], &fresh)?;
      (fresh, true)
    }
  };
  write_seal_preamble(&log_file, &seal)?;
  Ok(OpenedStoreFiles {
    log_file,
    rows,
    seal,
    applied,
    replay_from: preamble_len,
    page_size,
    seal_written_to_rows,
  })
}

/// Opens an existing store file `name` without creating it.
fn open_existing(root: &StoreRoot, name: &str) -> Result<DeviceFile, StoreError> {
  Ok(DeviceFile::adopt(
    root.open_file(name)?,
    &root.path_of(name),
    CachingRequest::Buffered,
    Alignment::BYTE,
  )?)
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

/// Rejects invalid `Sizing` parameters against `min_page` (at least [`MIN_PAGE`]) before creating
/// any store file on disk.
fn validate_sizing_bounds(sizing: Sizing, min_page: usize) -> Result<(), StoreError> {
  engine_sizing_for_page(min_page.max(MIN_PAGE), sizing).map(|_| ())
}

/// Opens an existing non-empty [`ROWS_FILE`] without mutating it, returning `None` when the file
/// is 0 bytes (uninitialized).
fn open_rows_existing(
  root: &StoreRoot,
  sizing: Sizing,
) -> Result<Option<(ShardDb<DeviceFile>, u64, usize)>, StoreError> {
  let rows_file = open_existing(root, ROWS_FILE)?;
  if rows_file.len()? == 0 {
    return Ok(None);
  }
  let (config, mem_limit, trunk) = engine_sizing(&rows_file, sizing)?;
  let page_size = config.page_size;
  let (rows, applied) = ShardDb::open(rows_file, config, mem_limit, trunk)?;
  Ok(Some((rows, applied, page_size)))
}

/// Creates (or initializes an existing 0-byte) [`ROWS_FILE`] and empty [`ShardDb`] (`applied == 0`).
fn create_rows_fresh(
  root: &StoreRoot,
  sizing: Sizing,
) -> Result<(ShardDb<DeviceFile>, u64, usize), StoreError> {
  let rows_file = adopt(root, ROWS_FILE)?;
  let (config, mem_limit, trunk) = engine_sizing(&rows_file, sizing)?;
  let page_size = config.page_size;
  let rows = ShardDb::create(rows_file, config, mem_limit, trunk)?;
  Ok((rows, 0, page_size))
}

/// Opens an existing non-empty [`ROWS_FILE`], or creates/initializes a fresh one when absent or
/// 0 bytes, validating sizing bounds against `known_page` (or [`MIN_PAGE`]) before creating any
/// file.
fn open_or_create_rows(
  root: &StoreRoot,
  rows_existed: bool,
  known_page: Option<usize>,
  sizing: Sizing,
) -> Result<(ShardDb<DeviceFile>, u64, usize), StoreError> {
  validate_sizing_bounds(sizing, known_page.unwrap_or(MIN_PAGE))?;
  if rows_existed && let Some(opened) = open_rows_existing(root, sizing)? {
    return Ok(opened);
  }
  create_rows_fresh(root, sizing)
}

/// Initializes a store whose [`LOG_FILE`] is absent or empty, checking `rows` before creating the
/// log file and writing a durable `SLSE` preamble at offset 0.
fn open_empty_store_log(
  root: &StoreRoot,
  existing_log: Option<DeviceFile>,
  rows_existed: bool,
  sizing: Sizing,
) -> Result<OpenedStoreFiles<DeviceFile>, StoreError> {
  let known_page = existing_log.as_ref().map(|file| file.layout_block().get());
  let mut rows_state = open_or_create_rows(root, rows_existed, known_page, sizing)?;
  if rows_state.1 > 0 {
    return Err(StoreError::Log(LogError::Corrupt(
      "a missing or truncated changeset log",
    )));
  }
  if read_seal_row(&mut rows_state.0)?.is_none() && has_any_rows(&mut rows_state.0)? {
    return Err(StoreError::Log(LogError::Corrupt("a store seal mismatch")));
  }
  let log_file = match existing_log {
    Some(file) => file,
    None => adopt(root, LOG_FILE)?,
  };
  prepare_empty_log(log_file, rows_state)
}

/// Inspects [`LOG_FILE`] before touching [`ROWS_FILE`], refusing unsupported formats or corrupt
/// preambles without mutating any file, and prepares both files with a durable seal.
fn open_store_files(
  root: &StoreRoot,
  sizing: Sizing,
) -> Result<OpenedStoreFiles<DeviceFile>, StoreError> {
  let rows_existed = root.exists(ROWS_FILE)?;
  if !root.exists(LOG_FILE)? {
    return open_empty_store_log(root, None, rows_existed, sizing);
  }
  let log_file = open_existing(root, LOG_FILE)?;
  match inspect_log_file(&log_file)? {
    LogHead::Empty => open_empty_store_log(root, Some(log_file), rows_existed, sizing),
    LogHead::Sealed(seal) => {
      let known_page = Some(log_file.layout_block().get());
      let rows_state = open_or_create_rows(root, rows_existed, known_page, sizing)?;
      prepare_sealed_log(log_file, seal, rows_state)
    }
    LogHead::DamagedPreamble => {
      let existing = if rows_existed {
        open_rows_existing(root, sizing)?
      } else {
        None
      };
      prepare_damaged_preamble_log(log_file, existing)
    }
  }
}

/// Pure sizing derivation for `page_size`: checks extent pages (`>= 2`), disk allowance
/// (`>= MIN_STORE_EXTENTS`), memory (`>= 2` leaf entries), and trunk fanout (`>= 2`).
fn engine_sizing_for_page(
  page_size: usize,
  sizing: Sizing,
) -> Result<(Config, usize, TrunkConfig), StoreError> {
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
    .filter(|extents| *extents >= MIN_STORE_EXTENTS)
    .ok_or(StoreError::Sizing(
      "a disk allowance under five extents (superblocks, initial and next allocator maps, initial branch, and initial trunk)",
    ))?;
  let min_entries =
    u64::try_from(MIN_TRUNK_DEGREE).map_err(|_| StoreError::Sizing("no memory for a row"))?;
  let leaf_entries = sizing
    .memory
    .checked_div(CHANGESET_ENTRY_BYTES)
    .and_then(|entries| u64::try_from(entries).ok())
    .filter(|entries| *entries >= min_entries)
    .ok_or(StoreError::Sizing("no memory for a row"))?;
  if sizing.fanout < MIN_TRUNK_DEGREE {
    return Err(StoreError::Sizing("a trunk fanout under two"));
  }
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

/// The engine's shape from the caller's sizing: its page the file's layout block, its extents bounded by the disk
/// allowed, its leaves by the memory lent over a row's bytes.
fn engine_sizing(
  file: &DeviceFile,
  sizing: Sizing,
) -> Result<(Config, usize, TrunkConfig), StoreError> {
  engine_sizing_for_page(file.layout_block().get(), sizing)
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

/// Forgets one payload section during [`Store::forget`] or changeset replacement: uses the O(1)
/// identity lookup when the section's CRC-32C matches on disk, and falls back to
/// [`payloads::forget_by_offset`] when damaged so its `P` and `B` rows never leak. Returns
/// `(rows_changed, saw_corrupt)`.
fn forget_payload_section<F: BlockFile>(
  rows: &mut ShardDb<F>,
  (file, end, seal): (&F, u64, &[u8; SEAL_BYTES]),
  section: &Section,
  page_size: usize,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(u64, bool), StoreError> {
  match verified_identity_of_section(file, section, buffer) {
    Ok(identity) => Ok((
      payloads::forget_in_log(rows, (file, end, seal), &identity, section.offset, value)?,
      false,
    )),
    Err(LogError::Corrupt(what)) => {
      slates_machine::log_line!(
        "slates-store: forget cleaned damaged payload at offset={} len={} by offset ({what})",
        section.offset,
        section.len
      );
      Ok((
        payloads::forget_by_offset(rows, (file, end, seal), section.offset, page_size)?,
        true,
      ))
    }
    Err(error) => Err(error.into()),
  }
}

/// Forgets one `Texts` section during [`Store::forget`] or changeset replacement: uses the fast
/// [`crate::texts::forget_rows`] path when the section's CRC-32C verifies on disk, and falls back
/// to bounded repository-scoped cleanup via [`crate::texts::forget_rows_by_sequence`] when
/// damaged. Returns `(rows_changed, saw_corrupt)`.
fn forget_texts_section<F: BlockFile>(
  rows: &mut ShardDb<F>,
  (file, end, seal): (&F, u64, &[u8; SEAL_BYTES]),
  (repository, sequence): (u128, u64),
  (section, page_size): (&Section, usize),
  buffer: &mut Vec<u8>,
) -> Result<(u64, bool), StoreError> {
  let mut entries_of = |rows: &mut ShardDb<F>, keeper: u64| {
    texts_of_changeset_in(rows, file, (end, seal), repository, keeper)
  };
  match section_texts(file, section, buffer) {
    Ok(entries) => Ok((
      crate::texts::forget_rows(rows, (repository, sequence), &entries, &mut entries_of)?,
      false,
    )),
    Err(StoreError::Log(LogError::Corrupt(what))) => {
      slates_machine::log_line!(
        "slates-store: forget cleaned damaged texts section repo={repository} seq={sequence} offset={} ({what})",
        section.offset
      );
      Ok((
        crate::texts::forget_rows_by_sequence(
          rows,
          (repository, sequence),
          page_size,
          &mut entries_of,
        )?,
        true,
      ))
    }
    Err(error) => Err(error),
  }
}

/// Fallback cleanup when a forgotten or replaced changeset's record header or section heads fail
/// verification on disk: removes all `P`/`B` rows in `[row.offset, row.offset + row.len)` and all
/// `K`/`H`/`T`/`I` rows of `(row.repository, row.sequence)`.
fn forget_corrupt_record<F: BlockFile>(
  rows: &mut ShardDb<F>,
  (file, end, seal): (&F, u64, &[u8; SEAL_BYTES]),
  row: &Changeset,
  page_size: usize,
) -> Result<u64, StoreError> {
  let range = (row.offset, row.offset.saturating_add(row.len));
  let payload_rows = payloads::forget_by_offset_range(rows, (file, end, seal), range, page_size)?;
  let mut entries_of = |rows: &mut ShardDb<F>, keeper: u64| {
    texts_of_changeset_in(rows, file, (end, seal), row.repository, keeper)
  };
  let text_rows = crate::texts::forget_rows_by_sequence(
    rows,
    (row.repository, row.sequence),
    page_size,
    &mut entries_of,
  )?;
  Ok(payload_rows.saturating_add(text_rows))
}

/// Forgets every `Payload` and `Texts` section of a record whose section heads decoded cleanly,
/// returning `(rows_changed, saw_corrupt)`.
fn forget_record_sections<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
  (repository, sequence): (u128, u64),
  (sections, page_size): (&[Section], usize),
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(u64, bool), StoreError> {
  let mut forgotten = 0_u64;
  let mut saw_corrupt = false;
  for section in sections {
    let (changed, corrupt) = match section.kind {
      SectionKind::Payload => {
        forget_payload_section(rows, log_ctx, section, page_size, (value, buffer))?
      }
      SectionKind::Texts => forget_texts_section(
        rows,
        log_ctx,
        (repository, sequence),
        (section, page_size),
        buffer,
      )?,
      _ => (0, false),
    };
    forgotten = forgotten.saturating_add(changed);
    saw_corrupt |= corrupt;
  }
  Ok((forgotten, saw_corrupt))
}

/// Releases every indexed payload (`P`, `B`) and text (`K`, `H`, `T`, `I`) row belonging to `row`,
/// falling back cleanly to offset-range and sequence scans whenever the record, payload, or
/// `Texts` section fails verification on disk. Returns `(rows_changed, saw_corrupt)`.
fn forget_record_rows<F: BlockFile>(
  rows: &mut ShardDb<F>,
  (file, end, seal): (&F, u64, &[u8; SEAL_BYTES]),
  row: &Changeset,
  page_size: usize,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(u64, bool), StoreError> {
  match sections_of(file, end, seal, row) {
    Ok(sections) => forget_record_sections(
      rows,
      (file, end, seal),
      (row.repository, row.sequence),
      (&sections, page_size),
      (value, buffer),
    ),
    Err(StoreError::Log(LogError::Corrupt(what))) => {
      slates_machine::log_line!(
        "slates-store: forget cleaned damaged changeset record repo={} seq={} offset={} ({what})",
        row.repository,
        row.sequence,
        row.offset
      );
      Ok((
        forget_corrupt_record(rows, (file, end, seal), row, page_size)?,
        true,
      ))
    }
    Err(error) => Err(error),
  }
}

/// Generic implementation of [`Store::forget`] over any [`BlockFile`]: cleans every indexed row
/// (`P`, `B`, `K`, `H`, `T`, `I`, `C`) and checkpoints `rows` at `log.end()`, falling back cleanly
/// to offset-range and sequence scans whenever the record, payload, or `Texts` section is corrupt.
fn forget_changeset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log: &ChangesetLog<F>,
  (repository, sequence): (u128, u64),
  page_size: usize,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<u64, StoreError> {
  let key = changeset_key(repository, sequence);
  let Some(row) = changeset_row(rows, &key)? else {
    return Ok(0);
  };
  let seal = log.seal();
  let (forgotten, _) = forget_record_rows(
    rows,
    (log.file(), log.end(), &seal),
    &row,
    page_size,
    (value, buffer),
  )?;
  rows.delete(&key)?;
  rows.checkpoint(log.end())?;
  Ok(forgotten.saturating_add(1))
}

/// The `Texts` entries of a committed record's section `section`, read whole from `file` into
/// `buffer` and verified against `section.crc`.
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
  section.verify_bytes(buffer)?;
  crate::texts::decode(buffer).map_err(|_| StoreError::BadRow)
}

/// The `Texts` entries of repository `repository`'s changeset `sequence`, read back from `file`
/// within the committed prefix `[0, end)` (none where it landed none, or no such changeset is
/// held).
fn texts_of_changeset_in<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  (end, seal): (u64, &[u8; SEAL_BYTES]),
  repository: u128,
  sequence: u64,
) -> Result<Vec<crate::texts::TextEntry>, StoreError> {
  let Some(row) = changeset_row(rows, &changeset_key(repository, sequence))? else {
    return Ok(Vec::new());
  };
  let mut buffer = Vec::new();
  for section in sections_of(file, end, seal, &row)? {
    if section.kind == SectionKind::Texts {
      return section_texts(file, &section, &mut buffer);
    }
  }
  Ok(Vec::new())
}

/// The sections named by a changeset row within a committed prefix of the log.
fn sections_of<F: BlockFile>(
  file: &F,
  end: u64,
  seal: &[u8; SEAL_BYTES],
  row: &Changeset,
) -> Result<Vec<crate::log::Section>, StoreError> {
  let record_end = row
    .offset
    .checked_add(row.len)
    .ok_or(LogError::Corrupt("a record's length"))?;
  if record_end > end {
    return Err(StoreError::Log(LogError::Corrupt(
      "a record past the committed records",
    )));
  }
  let located = Located {
    offset: row.offset,
    header: crate::log::header_in(file, row.offset, end, seal)?,
    identity: row.identity,
    len: row.len,
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

/// Resolves one section's raw bytes from either caller bytes or `staging`.
fn resolve_contents<'a>(
  contents: &Contents<'a>,
  staging: &'a Staging,
) -> Result<&'a [u8], StoreError> {
  match contents {
    Contents::Bytes(bytes) => Ok(*bytes),
    Contents::Staged(identity) => staging.get(identity).ok_or(StoreError::NotStaged),
  }
}

/// Key of block `block_idx` of the multi-piece payload section at `section_offset` in the log.
fn block_key(section_offset: u64, block_idx: u64) -> [u8; BLOCK_KEY_BYTES] {
  let mut key = [0u8; BLOCK_KEY_BYTES];
  put(&mut key, 0, &[BLOCK_DIGESTS]);
  put(&mut key, AFTER_TAG, &section_offset.to_be_bytes());
  put(&mut key, BK_INDEX, &block_idx.to_be_bytes());
  key
}

/// Computes the 32-byte keyed BLAKE3 MAC for block `block_idx` of the payload section at
/// `(section_offset, section_len)`.
fn keyed_block_tag(
  seal: &[u8; SEAL_BYTES],
  section_offset: u64,
  section_len: u64,
  block_idx: u64,
  block_bytes: &[u8],
) -> Identity {
  let mut hasher = blake3::Hasher::new_keyed(seal);
  hasher.update(BLOCK_MAC_DOMAIN);
  hasher.update(&section_offset.to_le_bytes());
  hasher.update(&section_len.to_le_bytes());
  hasher.update(&block_idx.to_le_bytes());
  hasher.update(block_bytes);
  *hasher.finalize().as_bytes()
}

/// Writes per-[`PIECE_BYTES`] keyed MAC `B` rows for an in-memory multi-piece payload slice.
/// Writes zero rows when `bytes.len() <= PIECE_BYTES`.
fn put_block_digests_from_slice<F: BlockFile>(
  rows: &mut ShardDb<F>,
  seal: &[u8; SEAL_BYTES],
  section_offset: u64,
  bytes: &[u8],
) -> Result<(), StoreError> {
  if bytes.len() <= PIECE_BYTES {
    return Ok(());
  }
  let section_len = u64::try_from(bytes.len()).map_err(|_| StoreError::BadRow)?;
  for (idx, chunk) in bytes.chunks(PIECE_BYTES).enumerate() {
    let block_idx = u64::try_from(idx).map_err(|_| StoreError::BadRow)?;
    let tag = keyed_block_tag(seal, section_offset, section_len, block_idx, chunk);
    rows.put(&block_key(section_offset, block_idx), &tag)?;
  }
  Ok(())
}

/// Deletes every per-block MAC `B` row belonging to a multi-piece payload section at
/// `(section_offset, section_len)`. No-op when `section_len <= PIECE_BYTES`.
fn delete_block_digests<F: BlockFile>(
  rows: &mut ShardDb<F>,
  section_offset: u64,
  section_len: u64,
) -> Result<(), StoreError> {
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  if section_len <= piece_cap {
    return Ok(());
  }
  let blocks = section_len.div_ceil(piece_cap);
  for block_idx in 0..blocks {
    rows.delete(&block_key(section_offset, block_idx))?;
  }
  Ok(())
}

/// Streams a multi-piece payload `section` in [`PIECE_BYTES`] blocks (peak RAM = one
/// [`PIECE_BYTES`] buffer), persisting each block's keyed MAC `B` row in `rows` and re-checking
/// `section.crc` (cleaning up any written `B` rows if a transient read mismatch occurs).
fn put_block_digests_from_log<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  seal: &[u8; SEAL_BYTES],
  section: &Section,
  buffer: &mut Vec<u8>,
) -> Result<(), StoreError> {
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  buffer.resize(PIECE_BYTES, 0);
  let mut crc = section.crc_verifier();
  let mut done = 0u64;
  let mut block_idx = 0u64;
  while done < section.len {
    let left = section.len.checked_sub(done).ok_or(StoreError::BadRow)?;
    let step = usize::try_from(left.min(piece_cap)).map_err(|_| StoreError::BadRow)?;
    let slice = buffer.get_mut(..step).ok_or(StoreError::BadRow)?;
    let at = section.offset.checked_add(done).ok_or(StoreError::BadRow)?;
    file.read_exact_at(slice, at).map_err(LogError::from)?;
    crc.update(slice);
    let tag = keyed_block_tag(seal, section.offset, section.len, block_idx, slice);
    rows.put(&block_key(section.offset, block_idx), &tag)?;
    done = done
      .checked_add(u64::try_from(step).map_err(|_| StoreError::BadRow)?)
      .ok_or(StoreError::BadRow)?;
    block_idx = block_idx.checked_add(1).ok_or(StoreError::BadRow)?;
  }
  if let Err(error) = crc.finish() {
    delete_block_digests(rows, section.offset, section.len)?;
    return Err(error.into());
  }
  Ok(())
}

/// Persists the `B` rows (when `section.len > PIECE_BYTES`) and `P` row for a payload section
/// whose CRC-32C and `identity` have already been verified against `file`.
fn index_verified_payload_from_log<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  seal: &[u8; SEAL_BYTES],
  section: &Section,
  identity: &Identity,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(), StoreError> {
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  if section.len > piece_cap {
    put_block_digests_from_log(rows, file, seal, section, buffer)?;
  }
  payloads::keep(rows, identity, section.offset, section.len, value)
}

/// Indexes one `SectionKind::Payload` from `file`, verifying `section.crc` BEFORE writing any `B`
/// or `P` rows to `rows`.
fn index_payload_from_log<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  seal: &[u8; SEAL_BYTES],
  section: &Section,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(), StoreError> {
  let identity = verified_identity_of_section(file, section, buffer)?;
  index_verified_payload_from_log(rows, file, seal, section, &identity, (value, buffer))
}

/// Verifies a zero-length span request (checking the empty-bytes BLAKE3 identity when the whole
/// payload is empty).
fn verify_empty_span(identity: &Identity, landed_len: u64) -> Result<Vec<u8>, StoreError> {
  if landed_len == 0 && *blake3::hash(&[]).as_bytes() != *identity {
    return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
  }
  Ok(Vec::new())
}

/// Reads a single-piece (`<= PIECE_BYTES`) payload in one log read, verifies its whole-payload
/// BLAKE3 against `identity`, and returns `[span_start..span_end)`.
fn read_verified_single_piece<F: BlockFile>(
  log: &ChangesetLog<F>,
  metrics: &mut PieceMetrics,
  identity: &Identity,
  (at, landed_len): (u64, u64),
  (span_start, span_end): (u64, u64),
) -> Result<Vec<u8>, StoreError> {
  let total = usize::try_from(landed_len).map_err(|_| StoreError::BadRow)?;
  let mut bytes = vec![0u8; total];
  log.read_at(&mut bytes, at)?;
  metrics.bytes_hashed = metrics.bytes_hashed.saturating_add(landed_len);
  if *blake3::hash(&bytes).as_bytes() != *identity {
    return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
  }
  metrics.verifications = metrics.verifications.saturating_add(1);
  if span_start == 0 && span_end == landed_len {
    return Ok(bytes);
  }
  let from = usize::try_from(span_start).map_err(|_| StoreError::BadRow)?;
  let to = usize::try_from(span_end).map_err(|_| StoreError::BadRow)?;
  bytes
    .get(from..to)
    .map(<[u8]>::to_vec)
    .ok_or(StoreError::BadRow)
}

/// Authenticates every [`PIECE_BYTES`] block in `aligned` (starting at `first_block`) against its
/// durable keyed BLAKE3 `B` row in `rows`.
fn verify_aligned_blocks<F: BlockFile>(
  rows: &mut ShardDb<F>,
  metrics: &mut PieceMetrics,
  seal: &[u8; SEAL_BYTES],
  (at, landed_len): (u64, u64),
  first_block: u64,
  aligned: &[u8],
) -> Result<(), StoreError> {
  let mut expected = Vec::with_capacity(IDENTITY);
  for (rel_idx, block_bytes) in aligned.chunks(PIECE_BYTES).enumerate() {
    let rel_u64 = u64::try_from(rel_idx).map_err(|_| StoreError::BadRow)?;
    let block_idx = first_block.checked_add(rel_u64).ok_or(StoreError::BadRow)?;
    expected.clear();
    if !rows.get(&block_key(at, block_idx), &mut expected)? || expected.len() != IDENTITY {
      return Err(StoreError::Log(LogError::Corrupt(
        "a payload's block digest",
      )));
    }
    let hashed = u64::try_from(block_bytes.len()).map_err(|_| StoreError::BadRow)?;
    metrics.bytes_hashed = metrics.bytes_hashed.saturating_add(hashed);
    let tag = keyed_block_tag(seal, at, landed_len, block_idx, block_bytes);
    if tag.as_slice() != expected.as_slice() {
      return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
    }
  }
  Ok(())
}

/// Reads the block-aligned span covering `[span_start, span_end)` of a multi-piece payload in one
/// [`ChangesetLog::read_at`] call, verifies every touched block against its `B` row, and returns
/// the exact requested slice.
fn read_verified_multi_piece<F: BlockFile>(
  log: &ChangesetLog<F>,
  rows: &mut ShardDb<F>,
  metrics: &mut PieceMetrics,
  (at, landed_len): (u64, u64),
  (span_start, span_end): (u64, u64),
) -> Result<Vec<u8>, StoreError> {
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  let last_byte = span_end.checked_sub(1).ok_or(StoreError::BadRow)?;
  let first_block = span_start
    .checked_div(piece_cap)
    .ok_or(StoreError::BadRow)?;
  let last_block = last_byte.checked_div(piece_cap).ok_or(StoreError::BadRow)?;
  let aligned_start = first_block
    .checked_mul(piece_cap)
    .ok_or(StoreError::BadRow)?;
  let aligned_end = last_block
    .checked_add(1)
    .and_then(|next| next.checked_mul(piece_cap))
    .map_or(landed_len, |end| end.min(landed_len));
  let aligned_len = usize::try_from(
    aligned_end
      .checked_sub(aligned_start)
      .ok_or(StoreError::BadRow)?,
  )
  .map_err(|_| StoreError::BadRow)?;
  let mut aligned = vec![0u8; aligned_len];
  log.read_at(
    &mut aligned,
    at.checked_add(aligned_start).ok_or(StoreError::BadRow)?,
  )?;
  let seal = log.seal();
  verify_aligned_blocks(
    rows,
    metrics,
    &seal,
    (at, landed_len),
    first_block,
    &aligned,
  )?;
  metrics.verifications = metrics.verifications.saturating_add(1);
  if span_start == aligned_start && span_end == aligned_end {
    return Ok(aligned);
  }
  let from = usize::try_from(
    span_start
      .checked_sub(aligned_start)
      .ok_or(StoreError::BadRow)?,
  )
  .map_err(|_| StoreError::BadRow)?;
  let to = usize::try_from(
    span_end
      .checked_sub(aligned_start)
      .ok_or(StoreError::BadRow)?,
  )
  .map_err(|_| StoreError::BadRow)?;
  aligned
    .get(from..to)
    .map(<[u8]>::to_vec)
    .ok_or(StoreError::BadRow)
}

/// Shared verified span reader for [`Store::piece`], [`Store::places_bytes`], and `read_place`:
/// checks the committed log boundary, authenticates every touched [`PIECE_BYTES`] block against
/// `identity` (`<= PIECE_BYTES`) or `B` rows (`> PIECE_BYTES`) in one log read, and updates
/// `metrics`.
fn read_verified_span_with<F: BlockFile>(
  log: &ChangesetLog<F>,
  rows: &mut ShardDb<F>,
  metrics: &mut PieceMetrics,
  identity: &Identity,
  (at, landed_len): (u64, u64),
  (span_start, span_len): (u64, u64),
) -> Result<Vec<u8>, StoreError> {
  let payload_end = at.checked_add(landed_len).ok_or(StoreError::BadRow)?;
  if payload_end > log.end() {
    return Err(StoreError::Log(LogError::Corrupt(
      "a read past the committed records",
    )));
  }
  let span_end = span_start.checked_add(span_len).ok_or(StoreError::BadRow)?;
  if span_end > landed_len {
    return Err(StoreError::BadRow);
  }
  if span_len == 0 {
    return verify_empty_span(identity, landed_len);
  }
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  if landed_len <= piece_cap {
    return read_verified_single_piece(
      log,
      metrics,
      identity,
      (at, landed_len),
      (span_start, span_end),
    );
  }
  read_verified_multi_piece(log, rows, metrics, (at, landed_len), (span_start, span_end))
}

/// Puts every row of the committed record `located`, its sections read back from `file`: the `C` row, a `P` row (and
/// per-block keyed MAC `B` rows when `> PIECE_BYTES`) for each payload, and the `T`/`H`/`I`/`K` rows of its `Texts`.
/// One function for a recovery's replay and for [`Store::applied`], so a record is indexed the same whichever way it
/// landed.
fn put_rows_from_log<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  (seal, page_size): (&[u8; SEAL_BYTES], usize),
  located: &Located,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
  corrupt_rows_skipped: &mut u64,
) -> Result<(), StoreError> {
  if !put_changeset_counted(
    rows,
    file,
    (seal, page_size),
    located,
    value,
    corrupt_rows_skipped,
  )? {
    return Ok(());
  }
  for section in sections_in(file, located)? {
    if section.kind == SectionKind::Payload {
      index_payload_from_log(rows, file, seal, &section, (value, buffer))?;
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
      section.verify_bytes(buffer)?;
      crate::texts::put_rows(rows, &located.header, buffer)?;
    }
  }
  Ok(())
}

/// Test-compatible wrapper for [`put_changeset_counted`] used by `payloads::tests`.
#[cfg(test)]
fn put_changeset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  located: &Located,
  value: &mut Vec<u8>,
) -> Result<bool, StoreError> {
  let mut skipped = 0u64;
  put_changeset_counted(
    rows,
    file,
    (&crate::log::UNSEALED, MIN_PAGE),
    located,
    value,
    &mut skipped,
  )
}

/// Releases all payload (`P`, `B`) and text (`K`, `H`, `T`, `I`) rows owned by a replaced
/// changeset `previous`, counting any corruption encountered during cleanup.
fn forget_replaced_changeset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  (end, seal, page_size): (u64, &[u8; SEAL_BYTES], usize),
  previous: &Changeset,
  value: &mut Vec<u8>,
  corrupt_rows_skipped: &mut u64,
) -> Result<(), StoreError> {
  let mut buffer = Vec::new();
  let (_, saw_corrupt) = forget_record_rows(
    rows,
    (file, end, seal),
    previous,
    page_size,
    (value, &mut buffer),
  )?;
  if saw_corrupt {
    *corrupt_rows_skipped = corrupt_rows_skipped.saturating_add(1);
  }
  Ok(())
}

/// Names this record at its sequence and releases the payload (`P`, `B`) and text (`K`, `H`, `T`,
/// `I`) rows of any record it replaces, logging, counting, and cleaning by offset range and
/// sequence any replaced row whose prior log entry no longer verifies. Returns false when a later
/// landing already owns the sequence, so retrying the older apply writes no rows.
fn put_changeset_counted<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  (seal, page_size): (&[u8; SEAL_BYTES], usize),
  located: &Located,
  value: &mut Vec<u8>,
  corrupt_rows_skipped: &mut u64,
) -> Result<bool, StoreError> {
  let header = &located.header;
  let key = changeset_key(header.repository, header.sequence);
  if rows.get(&key, value)? {
    let previous = decode_changeset(&key, value)?;
    match previous.offset.cmp(&located.offset) {
      std::cmp::Ordering::Greater => return Ok(false),
      std::cmp::Ordering::Equal => return Ok(true),
      std::cmp::Ordering::Less => {
        // A retried creation can reuse a sequence. Only the newly named record can keep rows there.
        forget_replaced_changeset(
          rows,
          file,
          (located.offset, seal, page_size),
          &previous,
          value,
          corrupt_rows_skipped,
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

#[cfg(test)]
mod tests;
