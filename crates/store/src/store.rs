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
//! absent, the engine is recreated and rebuilt byte-identically from [`LOG_FILE`]'s self-contained preamble and
//! records. Otherwise the engine opens at its checkpoint offset and replays [`LOG_FILE`] from there, so the rows never
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

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fmt;

use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use hyper_block::block::BlockFile;
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use mantle_engine::rows::Rows;
use mantle_engine::shard_db::ShardDb;
use mantle_engine::store::Config;
use mantle_engine::trunk::TrunkConfig;
use slates_land::store_root::{StoreRoot, StoreRootRefusal};

use crate::log::{
  Appending, ChangesetLog, Header, Located, LogError, SEAL_BYTES, SEAL_PREAMBLE_BYTES,
  SealPreamble, SectionKind, decode_seal_preamble, encode_seal_preamble, identity_of_range,
  sections_in,
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
/// Format: the tag of the store's per-store changeset log seal row (`S`), holding the 32-byte
/// random secret authenticating every record header CRC and keyed BLAKE3 identity in [`LOG_FILE`].
const SEAL_ROW: u8 = b'S';
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

/// One 32-byte BLAKE3 digest of a single [`PIECE_BYTES`] payload block.
type BlockDigest = [u8; blake3::OUT_LEN];
/// Owned per-[`PIECE_BYTES`] block-digest slice for one authenticated landed payload.
type BlockDigests = Box<[BlockDigest]>;

/// Authenticated payload range key: its log offset, exact byte length, and expected BLAKE3 identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct VerifiedKey {
  at: u64,
  len: u64,
  identity: Identity,
}

/// Bounded FIFO cache of authenticated per-[`PIECE_BYTES`] BLAKE3 block digests for multi-piece
/// landed payloads.
///
/// An entry is inserted only after a full-payload streaming BLAKE3 pass verifies `identity`.
/// Cached reads still hash every disk block they touch against these digests and evict on mismatch,
/// giving O(N) sequential streaming while preserving per-piece integrity against mid-session disk
/// bit-flips (A5).
#[derive(Debug)]
struct PieceDigests {
  budget_bytes: usize,
  used_bytes: usize,
  entries: HashMap<VerifiedKey, BlockDigests>,
  order: VecDeque<VerifiedKey>,
  payload_verifications: u64,
}

impl PieceDigests {
  /// Derived: cache budget from `sizing.memory / sizing.fanout` floored at one [`PIECE_BYTES`]
  /// read unit, matching [`Sizing`]'s lent memory and engine trunk fanout inputs.
  fn from_sizing(sizing: Sizing) -> Self {
    let budget_bytes = sizing
      .memory
      .checked_div(sizing.fanout.max(1))
      .unwrap_or(PIECE_BYTES)
      .max(PIECE_BYTES);
    Self::with_budget(budget_bytes)
  }

  fn with_budget(budget_bytes: usize) -> Self {
    Self {
      budget_bytes,
      used_bytes: 0,
      entries: HashMap::new(),
      order: VecDeque::new(),
      payload_verifications: 0,
    }
  }

  fn budget_bytes(&self) -> usize {
    self.budget_bytes
  }

  fn get(&self, key: &VerifiedKey) -> Option<&[BlockDigest]> {
    self.entries.get(key).map(Box::as_ref)
  }

  fn insert(&mut self, key: VerifiedKey, digests: BlockDigests) {
    if self.entries.contains_key(&key) {
      return;
    }
    let Some(entry_bytes) = digests.len().checked_mul(blake3::OUT_LEN) else {
      return;
    };
    if entry_bytes > self.budget_bytes {
      return;
    }
    let limit = self.budget_bytes.saturating_sub(entry_bytes);
    self.evict_until(limit);
    self.used_bytes = self.used_bytes.saturating_add(entry_bytes);
    self.order.push_back(key);
    self.entries.insert(key, digests);
  }

  fn evict_until(&mut self, max_used_bytes: usize) {
    while self.used_bytes > max_used_bytes {
      let Some(oldest) = self.order.pop_front() else {
        break;
      };
      if let Some(evicted) = self.entries.remove(&oldest) {
        let evicted_bytes = evicted.len().saturating_mul(blake3::OUT_LEN);
        self.used_bytes = self.used_bytes.saturating_sub(evicted_bytes);
      }
    }
  }

  fn remove(&mut self, key: &VerifiedKey) {
    if let Some(digests) = self.entries.remove(key) {
      let removed_bytes = digests.len().saturating_mul(blake3::OUT_LEN);
      self.used_bytes = self.used_bytes.saturating_sub(removed_bytes);
      self.order.retain(|queued| queued != key);
    }
  }

  fn record_verification(&mut self) {
    self.payload_verifications = self.payload_verifications.saturating_add(1);
  }

  fn verifications(&self) -> u64 {
    self.payload_verifications
  }
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
  /// Bounded per-[`PIECE_BYTES`] BLAKE3 block-digest cache for landed payloads (A5).
  piece_digests: PieceDigests,
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
    let expected_grant = grant_id(root.grant().target.device, root.grant().target.inode);
    let mut replayed = Ok(());
    let mut value = Vec::new();
    let mut buffer = Vec::new();
    let log = ChangesetLog::recover_sealed(
      log_file,
      replay_from,
      seal,
      Some(expected_grant),
      |file, located| {
        if replayed.is_ok() {
          replayed = put_rows_from_log(
            &mut rows,
            file,
            &seal,
            located,
            (&mut value, &mut buffer),
            &mut corrupt_rows_skipped,
          );
        }
      },
    )?;
    replayed?;
    let mut store = Self {
      root,
      log,
      rows,
      value,
      piece_digests: PieceDigests::from_sizing(sizing),
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

  /// Verified disk piece reads completed by [`Store::piece`] so far (non-vacuity counter).
  pub fn payload_verifications(&self) -> u64 {
    self.piece_digests.verifications()
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
      &seal,
      located,
      (&mut self.value, &mut buffer),
      &mut self.corrupt_rows_skipped,
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
    let seal = self.log.seal();
    if !put_changeset_counted(
      &mut self.rows,
      self.log.file(),
      &seal,
      &located,
      &mut self.value,
      &mut self.corrupt_rows_skipped,
    )? {
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

  /// Returns up to [`PIECE_BYTES`] of the content `identity` (expected length `len`) starting at
  /// `offset`, or `None` when neither `staging` nor the log holds it or `offset >= len`.
  ///
  /// While `identity` is staged in flight, the remaining slice from `offset` is borrowed from
  /// `staging` without copying. For a landed payload in the log, every call verifies the disk
  /// bytes it reads against `identity`—the full payload on single-piece reads and cache misses,
  /// and each touched [`PIECE_BYTES`] block against its authenticated digest on cache hits
  /// (evicting and refusing [`LogError::Corrupt`] on any mismatch)—returning at most
  /// [`PIECE_BYTES`] from `offset`.
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
    let bytes = read_verified_piece(
      &self.log,
      &mut self.piece_digests,
      identity,
      (at, landed_len),
      offset,
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

/// Pairs an already-sealed `log_file` with `rows`, mirroring `seal` into `rows` when rebuilding
/// and refusing a mismatched `SEAL_ROW` without mutating either file.
fn prepare_sealed_log<F: BlockFile>(
  log_file: F,
  seal: [u8; SEAL_BYTES],
  (mut rows, applied, page_size): (ShardDb<F>, u64, usize),
) -> Result<OpenedStoreFiles<F>, StoreError> {
  let preamble_len = u64::try_from(SEAL_PREAMBLE_BYTES).map_err(|_| LogError::TooLarge)?;
  let seal_written_to_rows = match read_seal_row(&mut rows)? {
    Some(existing) if existing != seal => {
      return Err(StoreError::Log(LogError::Corrupt("a store seal mismatch")));
    }
    Some(_) => false,
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
    replay_from: applied.max(preamble_len),
    page_size,
    seal_written_to_rows,
  })
}

/// Recovers a log whose leading `SLSE` preamble is damaged only when an existing `rows` holds the
/// backup `SEAL_ROW`, refusing without mutating any file otherwise.
fn prepare_damaged_preamble_log<F: BlockFile>(
  log_file: F,
  existing_rows: Option<(ShardDb<F>, u64, usize)>,
) -> Result<OpenedStoreFiles<F>, StoreError> {
  let preamble_len = u64::try_from(SEAL_PREAMBLE_BYTES).map_err(|_| LogError::TooLarge)?;
  let Some((mut rows, applied, page_size)) = existing_rows else {
    return Err(StoreError::Log(LogError::Corrupt("a store seal preamble")));
  };
  let Some(seal) = read_seal_row(&mut rows)? else {
    return Err(StoreError::Log(LogError::Corrupt("a store seal preamble")));
  };
  Ok(OpenedStoreFiles {
    log_file,
    rows,
    seal,
    applied,
    replay_from: applied.max(preamble_len),
    page_size,
    seal_written_to_rows: false,
  })
}

/// Initializes an empty `log_file` with a durable `SLSE` preamble, reusing `SEAL_ROW` from `rows`
/// if present or generating fresh OS randomness.
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

/// Opens an existing [`ROWS_FILE`] without mutating it.
fn open_rows_existing(
  root: &StoreRoot,
  sizing: Sizing,
) -> Result<(ShardDb<DeviceFile>, u64, usize), StoreError> {
  let rows_file = open_existing(root, ROWS_FILE)?;
  let (config, mem_limit, trunk) = engine_sizing(&rows_file, sizing)?;
  let page_size = config.page_size;
  let (rows, applied) = ShardDb::open(rows_file, config, mem_limit, trunk)?;
  Ok((rows, applied, page_size))
}

/// Creates a new [`ROWS_FILE`] and empty [`ShardDb`] (`applied == 0`).
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

/// Opens existing [`ROWS_FILE`] or creates a fresh one when absent.
fn open_or_create_rows(
  root: &StoreRoot,
  rows_existed: bool,
  sizing: Sizing,
) -> Result<(ShardDb<DeviceFile>, u64, usize), StoreError> {
  if rows_existed {
    open_rows_existing(root, sizing)
  } else {
    create_rows_fresh(root, sizing)
  }
}

/// Initializes a store whose [`LOG_FILE`] is absent or empty, checking `rows` before creating the
/// log file and writing a durable `SLSE` preamble at offset 0.
fn open_empty_store_log(
  root: &StoreRoot,
  existing_log: Option<DeviceFile>,
  rows_existed: bool,
  sizing: Sizing,
) -> Result<OpenedStoreFiles<DeviceFile>, StoreError> {
  let rows_state = open_or_create_rows(root, rows_existed, sizing)?;
  if rows_state.1 > 0 {
    return Err(StoreError::Log(LogError::Corrupt(
      "a missing or truncated changeset log",
    )));
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
      let rows_state = open_or_create_rows(root, rows_existed, sizing)?;
      prepare_sealed_log(log_file, seal, rows_state)
    }
    LogHead::DamagedPreamble => {
      let existing = rows_existed
        .then(|| open_rows_existing(root, sizing))
        .transpose()?;
      prepare_damaged_preamble_log(log_file, existing)
    }
  }
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
  let seal = log.seal();
  for section in sections_of(log.file(), log.end(), &seal, &row)? {
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
  seal: &[u8; SEAL_BYTES],
  row: &Changeset,
) -> Result<Vec<crate::log::Section>, StoreError> {
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

/// Reads `take` raw bytes from `log` at `offset` within its committed prefix.
fn read_piece_raw<F: BlockFile>(
  log: &ChangesetLog<F>,
  offset: u64,
  take: usize,
) -> Result<Vec<u8>, StoreError> {
  let mut bytes = vec![0u8; take];
  log.read_at(&mut bytes, offset)?;
  Ok(bytes)
}

/// Copies the intersection of chunk `[done, chunk_end)` and requested window `[offset, req_end)`
/// from `slice` into `out`.
fn capture_overlap(
  slice: &[u8],
  (done, chunk_end): (u64, u64),
  (offset, req_end): (u64, u64),
  out: &mut Vec<u8>,
) -> Result<(), StoreError> {
  let copy_start = done.max(offset);
  let copy_end = chunk_end.min(req_end);
  if copy_start >= copy_end {
    return Ok(());
  }
  let local_start =
    usize::try_from(copy_start.saturating_sub(done)).map_err(|_| StoreError::BadRow)?;
  let local_end = usize::try_from(copy_end.saturating_sub(done)).map_err(|_| StoreError::BadRow)?;
  let part = slice
    .get(local_start..local_end)
    .ok_or(StoreError::BadRow)?;
  out.extend_from_slice(part);
  Ok(())
}

/// Returns the number of [`PIECE_BYTES`] block digests needed for `landed_len` when the full
/// digest slice fits within `budget_bytes`, or `None` when empty or over budget.
fn digest_capacity(landed_len: u64, piece_cap: u64, budget_bytes: usize) -> Option<usize> {
  if landed_len == 0 {
    return None;
  }
  let blocks = usize::try_from(landed_len.div_ceil(piece_cap.max(1))).ok()?;
  let bytes = blocks.checked_mul(blake3::OUT_LEN)?;
  (bytes <= budget_bytes).then_some(blocks)
}

/// Converts a payload byte offset into its zero-based [`PIECE_BYTES`] block index.
fn block_index(byte_offset: u64, piece_cap: u64) -> Result<usize, StoreError> {
  byte_offset
    .checked_div(piece_cap)
    .and_then(|idx| usize::try_from(idx).ok())
    .ok_or(StoreError::BadRow)
}

/// Returns `[block_start, block_end)` within `[0, landed_len)` for `block_idx`.
fn block_bounds(
  block_idx: usize,
  piece_cap: u64,
  landed_len: u64,
) -> Result<(u64, u64), StoreError> {
  let start = u64::try_from(block_idx)
    .ok()
    .and_then(|idx| idx.checked_mul(piece_cap))
    .ok_or(StoreError::BadRow)?;
  let end = start.saturating_add(piece_cap).min(landed_len);
  Ok((start, end))
}

/// Streams `[at, at + landed_len)` through BLAKE3 in [`PIECE_BYTES`] blocks (enforcing the
/// committed log boundary via [`ChangesetLog::read_at`]), collecting per-block digests when they
/// fit within `budget_bytes` and capturing `[offset, offset + take)` in a single pass. Returns
/// [`LogError::Corrupt`] before exposing any byte on whole-payload identity mismatch.
fn stream_verify_and_capture<F: BlockFile>(
  log: &ChangesetLog<F>,
  identity: &Identity,
  (at, landed_len): (u64, u64),
  (offset, take): (u64, usize),
  budget_bytes: usize,
) -> Result<(Vec<u8>, Option<BlockDigests>), StoreError> {
  let payload_end = at.checked_add(landed_len).ok_or(StoreError::BadRow)?;
  if payload_end > log.end() {
    return Err(StoreError::Log(LogError::Corrupt(
      "a read past the committed records",
    )));
  }
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  let req_end = offset
    .checked_add(u64::try_from(take).map_err(|_| StoreError::BadRow)?)
    .ok_or(StoreError::BadRow)?;
  let mut out = Vec::with_capacity(take);
  let mut digests = digest_capacity(landed_len, piece_cap, budget_bytes).map(Vec::with_capacity);
  let mut chunk_buf = vec![0u8; PIECE_BYTES];
  let mut hasher = blake3::Hasher::new();
  let mut done = 0u64;
  while done < landed_len {
    let left = landed_len.checked_sub(done).ok_or(StoreError::BadRow)?;
    let step = usize::try_from(left.min(piece_cap)).map_err(|_| StoreError::BadRow)?;
    let slice = chunk_buf.get_mut(..step).ok_or(StoreError::BadRow)?;
    log.read_at(slice, at.checked_add(done).ok_or(StoreError::BadRow)?)?;
    hasher.update(slice);
    if let Some(list) = &mut digests {
      list.push(*blake3::hash(slice).as_bytes());
    }
    let chunk_end = done
      .checked_add(u64::try_from(step).map_err(|_| StoreError::BadRow)?)
      .ok_or(StoreError::BadRow)?;
    capture_overlap(slice, (done, chunk_end), (offset, req_end), &mut out)?;
    done = chunk_end;
  }
  if *hasher.finalize().as_bytes() != *identity {
    return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
  }
  if out.len() != take {
    return Err(StoreError::BadRow);
  }
  Ok((out, digests.map(Vec::into_boxed_slice)))
}

/// Reads and verifies only the [`PIECE_BYTES`] blocks overlapping `[offset, offset + take)`
/// against their authenticated BLAKE3 digests in `block_digests`.
fn read_cached_blocks<F: BlockFile>(
  log: &ChangesetLog<F>,
  block_digests: &[BlockDigest],
  (at, landed_len): (u64, u64),
  (offset, take): (u64, usize),
) -> Result<Vec<u8>, StoreError> {
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  let take_u64 = u64::try_from(take).map_err(|_| StoreError::BadRow)?;
  let req_end = offset.checked_add(take_u64).ok_or(StoreError::BadRow)?;
  let last_byte = req_end.checked_sub(1).ok_or(StoreError::BadRow)?;
  let first_block = block_index(offset, piece_cap)?;
  let last_block = block_index(last_byte, piece_cap)?;
  let mut out = Vec::with_capacity(take);
  let mut block_buf = vec![0u8; PIECE_BYTES];
  for block_idx in first_block..=last_block {
    let expected = block_digests.get(block_idx).ok_or(StoreError::BadRow)?;
    let (block_start, block_end) = block_bounds(block_idx, piece_cap, landed_len)?;
    let block_len = usize::try_from(
      block_end
        .checked_sub(block_start)
        .ok_or(StoreError::BadRow)?,
    )
    .map_err(|_| StoreError::BadRow)?;
    let slice = block_buf.get_mut(..block_len).ok_or(StoreError::BadRow)?;
    log.read_at(
      slice,
      at.checked_add(block_start).ok_or(StoreError::BadRow)?,
    )?;
    if *blake3::hash(slice).as_bytes() != *expected {
      return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
    }
    capture_overlap(slice, (block_start, block_end), (offset, req_end), &mut out)?;
  }
  if out.len() != take {
    return Err(StoreError::BadRow);
  }
  Ok(out)
}

/// Reads a whole single-piece payload from `at` and authenticates it directly against `identity`.
fn read_single_piece<F: BlockFile>(
  log: &ChangesetLog<F>,
  digests: &mut PieceDigests,
  identity: &Identity,
  at: u64,
  take: usize,
) -> Result<Vec<u8>, StoreError> {
  let bytes = read_piece_raw(log, at, take)?;
  if *blake3::hash(&bytes).as_bytes() != *identity {
    return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
  }
  digests.record_verification();
  Ok(bytes)
}

/// Records a cached block-digest read on success, or evicts `key` before returning corruption.
fn finish_cached_read(
  digests: &mut PieceDigests,
  key: &VerifiedKey,
  result: Result<Vec<u8>, StoreError>,
) -> Result<Vec<u8>, StoreError> {
  match result {
    Ok(bytes) => {
      digests.record_verification();
      Ok(bytes)
    }
    Err(error @ StoreError::Log(LogError::Corrupt(_))) => {
      digests.remove(key);
      Err(error)
    }
    Err(error) => Err(error),
  }
}

/// Reads one piece (at most [`PIECE_BYTES`] from `offset`) of a landed payload at `(at, landed_len)`,
/// authenticating every disk block read against `identity` (whole payload on single-piece reads
/// and cache misses; cached per-[`PIECE_BYTES`] block digests on hits, evicting on any mismatch).
fn read_verified_piece<F: BlockFile>(
  log: &ChangesetLog<F>,
  digests: &mut PieceDigests,
  identity: &Identity,
  (at, landed_len): (u64, u64),
  offset: u64,
) -> Result<Vec<u8>, StoreError> {
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  let left = landed_len.checked_sub(offset).ok_or(StoreError::BadRow)?;
  let take = usize::try_from(left.min(piece_cap)).map_err(|_| StoreError::BadRow)?;
  if offset == 0 && u64::try_from(take).ok() == Some(landed_len) {
    return read_single_piece(log, digests, identity, at, take);
  }
  if take == 0 {
    return Err(StoreError::BadRow);
  }
  let key = VerifiedKey {
    at,
    len: landed_len,
    identity: *identity,
  };
  if let Some(cached) = digests.get(&key) {
    let result = read_cached_blocks(log, cached, (at, landed_len), (offset, take));
    return finish_cached_read(digests, &key, result);
  }
  let (bytes, block_digests) = stream_verify_and_capture(
    log,
    identity,
    (at, landed_len),
    (offset, take),
    digests.budget_bytes(),
  )?;
  if let Some(block_digests) = block_digests {
    digests.insert(key, block_digests);
  }
  digests.record_verification();
  Ok(bytes)
}

/// Puts every row of the committed record `located`, its sections read back from `file`: the `C` row, a `P` row for each
/// payload (its identity hashed from the log), the `T`/`H` rows of its `Texts`. One function for a recovery's replay and
/// for [`Store::applied`], so a record is indexed the same whichever way it landed.
fn put_rows_from_log<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  seal: &[u8; SEAL_BYTES],
  located: &Located,
  (value, buffer): (&mut Vec<u8>, &mut Vec<u8>),
  corrupt_rows_skipped: &mut u64,
) -> Result<(), StoreError> {
  if !put_changeset_counted(rows, file, seal, located, value, corrupt_rows_skipped)? {
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
    &crate::log::UNSEALED,
    located,
    value,
    &mut skipped,
  )
}

/// Names this record at its sequence and releases the payload places of any record it replaces,
/// logging and counting any replaced row whose prior log entry no longer verifies.
/// Returns false when a later landing already owns the sequence, so retrying the older apply writes no rows.
fn put_changeset_counted<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  seal: &[u8; SEAL_BYTES],
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
        // A retried creation can reuse a sequence. Only the newly named record can keep payloads there.
        match payloads::visit(
          file,
          (located.offset, seal),
          &previous,
          &mut Vec::new(),
          |identity, offset, _| {
            payloads::forget(rows, identity, offset, value)?;
            Ok(())
          },
        ) {
          Ok(()) => {}
          Err(StoreError::Log(LogError::Corrupt(what))) => {
            *corrupt_rows_skipped = corrupt_rows_skipped.saturating_add(1);
            slates_machine::log_line!(
              "slates-store: put_changeset skipped corrupt replaced changeset row repo={} seq={} offset={} ({what})",
              previous.repository,
              previous.sequence,
              previous.offset
            );
          }
          Err(error) => return Err(error),
        }
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
