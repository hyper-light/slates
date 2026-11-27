//! The changeset log (A-767 item 2): a repository's history, an append-only file of landed changesets, each one record.
//!
//! **Store preamble.** A [`crate::store::Store`]'s changeset log begins at offset 0 with a [`SEAL_PREAMBLE_BYTES`]-byte
//! self-describing `SLSE` preamble (`SLSE`, [`FORMAT`] as little-endian `u16`, two reserved zero bytes, the store's
//! 32-byte random secret seal, the CRC-32C of those 40 bytes, and its bitwise complement). Later `SLSE` formats keep
//! this 48-byte preamble layout and trailing CRC-32C + complement placement so [`decode_seal_preamble`] validates the
//! preamble checksum before checking `P_FORMAT`: a single-bit media flip in `P_FORMAT` classifies as
//! [`SealPreamble::Damaged`] (recoverable from `rows`'s backup `S` row) while a genuine future format classifies as
//! [`SealPreamble::UnsupportedFormat`]. The preamble makes the log self-contained so the derived `rows` index can be
//! deleted and rebuilt with every committed record intact and the log byte-identical from the log alone, and lets
//! `Store::open` refuse legacy `FORMAT = 1` or future format versions with [`LogError::UnsupportedFormat`] before
//! creating or mutating any file. Direct [`ChangesetLog::recover`] callers over bare logs still start at offset 0.
//!
//! **A record** (all integers little-endian):
//! - a header of [`HEADER_BYTES`]: magic `SLCS`, [`FORMAT`] (`2`), the tool, a flags byte, then the repository, the
//!   changeset's sequence in it, the sequence it was judged over, the grant, the work and the time (the repository a
//!   `u128`, the rest each a `u64`), the section count (`u16`), two zero bytes, and the CRC-32C of the log's
//!   32-byte seal, the record's file offset (`u64` little-endian), and the 68 header bytes before the CRC;
//! - its sections, each a head of [`SECTION_HEAD_BYTES`] followed by the section's bytes. When a section opens before
//!   its pieces stream, its head holds the kind, [`SECTION_OPEN_FLAG`] (`0x80`), two zero bytes, its declared length
//!   (`u64`), and the `(seal, section_head_offset)`-keyed CRC-32C of those 12 bytes. When the section closes, those 16
//!   bytes are overwritten with the closed head (the kind, three zero bytes, the actual length as a `u64`, and the
//!   CRC-32C of the section's bytes);
//! - a trailer of [`TRAILER_BYTES`]: the changeset's identity, the commit magic `SLCC`, the record's whole length
//!   (`u64`), and the CRC-32C of the trailer before it. The identity is keyed BLAKE3 (keyed by the log's 32-byte seal)
//!   over the record's file offset (`u64` little-endian), the header bytes, and, for each section in order, its kind,
//!   its length (`u64`) and its bytes: bound to both position and store secret, hashed as bytes are written so nothing
//!   is read back to name a record.
//!
//! **Appending.** A record is written where the log ends, a section at a time and a section's bytes in pieces of any
//! size, so an import of any size holds one piece in memory; opening a section writes its authenticated open head, and
//! closing it overwrites those 16 bytes with the closed head and payload CRC. The trailer is written last, then one
//! flush of the file (`sync_data`: `F_FULLFSYNC` on macOS, `fdatasync` on Linux, `FlushFileBuffers` on Windows), and
//! only then does the log's end move past the record: a reader of the log sees a record only once it is committed and
//! durable. A record abandoned before its commit (its writer dropped, an error) leaves bytes past the end that the next
//! append overwrites.
//!
//! **Recovery.** The log is read from a committed offset the caller trusts (the engine's checkpoint, A-767 item 3, or
//! the file's start), record by record, each verified whole against `(offset, seal)`: the header's CRC, every section's
//! CRC, the trailer's CRC and length, and the keyed BLAKE3 identity recomputed over the record. A record whose header,
//! section heads, and commit trailer all verify at `cursor` (`Verify::Damaged`) but whose payload CRC or identity fails
//! has an authenticated boundary `[cursor, next)`: it is never indexed, recovery skips directly to `next`, and `end`
//! advances to `next` so a subsequent append never overwrites intact blocks of a damaged committed tail record. At
//! `FrameContext::Cursor` (never during resync), an authenticated `(offset, seal)` header whose next section head
//! decodes as a genuinely authenticated open-section head (`SECTION_OPEN_FLAG` with valid `(seal, head_offset)` CRC),
//! whose declared remaining record cannot finish before EOF, and whose prior closed sections all pass CRC verification
//! is an interrupted uncommitted write at EOF (`Verify::UnwrittenTail`): recovery logs the offset and trailing byte
//! count and stops cleanly in O(1) reads without scanning the uncommitted tail payload and without incrementing
//! `skipped`. An all-zero section head `[0u8; SECTION_HEAD_BYTES]` (from a lost 16 B, 512 B, or 4 KiB sector or a
//! flipped kind bit) is always `Verify::Broken`, never `Verify::UnwrittenTail`. Whenever header decoding, section-head
//! framing, or commit-trailer verification fails at `cursor`, recovery never trusts an unverified `section.len` to stop
//! early or jump past siblings; it scans linearly from `cursor + 1` in [`VERIFY_PIECE`] chunks (overlapping by
//! [`HEADER_OVERLAP`]) with a linear section-head read budget and optional `expected_grant` filter. False `SLCS`
//! occurrences inside payloads or torn EOF tails fail `(offset, seal)` header CRC verification immediately in memory
//! without touching disk or section heads, so mid-file faults (including zeroed sectors and every single-bit media
//! fault in a header, section descriptor, payload, or trailer) recover healthy trailing siblings while uncommitted torn
//! EOF tails stop cleanly at the last genuine committed record. A record whose trailer reached the device before a
//! crash is whole and is kept, as any write that completed.

use std::fmt;

use hyper_block::DiskError;
use hyper_block::block::BlockFile;

/// Format: the record header's magic.
const MAGIC: [u8; 4] = *b"SLCS";
/// Format: the trailer's commit magic.
const COMMIT: [u8; 4] = *b"SLCC";
/// Format: the store seal preamble's magic at offset 0 of a `Store`'s changeset log.
const SEAL_MAGIC: [u8; 4] = *b"SLSE";
/// Format: this layout's number (2 = sealed + offset-bound + authenticated open-section head).
pub const FORMAT: u16 = 2;
/// Format: a `u16` field's width.
const U16: usize = size_of::<u16>();
/// Format: a `u32` field's width.
const U32: usize = size_of::<u32>();
/// Format: a `u64` field's width.
const U64: usize = size_of::<u64>();
/// Format: a `u128` field's width (a repository's id: its creator host, owner partition and counter, A-515).
const U128: usize = size_of::<u128>();
/// Format: an identity's width (BLAKE3).
const IDENTITY: usize = blake3::OUT_LEN;
/// Format: a per-store log seal's width (keyed BLAKE3 key size).
pub const SEAL_BYTES: usize = blake3::KEY_LEN;
/// Format: the all-zero seal used by direct unsealed [`ChangesetLog::recover`] callers (records
/// still bind to their exact file offset).
pub const UNSEALED: [u8; SEAL_BYTES] = [0u8; SEAL_BYTES];

/// Format: the seal preamble's format field, after its magic.
const P_FORMAT: usize = SEAL_MAGIC.len();
/// Format: the seal preamble's two reserved zero bytes, after its format.
const P_RESERVED: usize = P_FORMAT + U16;
/// Format: the seal preamble's 32-byte secret seal, after its reserved bytes.
const P_SEAL: usize = P_RESERVED + U16;
/// Format: the seal preamble's CRC-32C, after its seal: the bytes its CRC covers.
const SEAL_PREAMBLE_COVERED: usize = P_SEAL + SEAL_BYTES;
/// Format: the seal preamble's bitwise-complement CRC guard, after its CRC.
const P_CRC_COMPLEMENT: usize = SEAL_PREAMBLE_COVERED + U32;
/// Format: the store seal preamble's width in bytes (48 bytes).
pub const SEAL_PREAMBLE_BYTES: usize = P_CRC_COMPLEMENT + U32;

/// Format: the header's format field, after its magic.
const H_FORMAT: usize = MAGIC.len();
/// Format: the header's tool byte, after the format.
const H_TOOL: usize = H_FORMAT + U16;
/// Format: the header's flags byte (zero), after the tool.
const H_FLAGS: usize = H_TOOL + 1;
/// Format: the header's repository, after the flags.
const H_REPOSITORY: usize = H_FLAGS + 1;
/// Format: the header's sequence, after the repository.
const H_SEQUENCE: usize = H_REPOSITORY + U128;
/// Format: the header's judged-over sequence, after the sequence.
const H_JUDGED: usize = H_SEQUENCE + U64;
/// Format: the header's grant, after the judged-over sequence.
const H_GRANT: usize = H_JUDGED + U64;
/// Format: the header's work, after the grant.
const H_WORK: usize = H_GRANT + U64;
/// Format: the header's time, after the work.
const H_TIME: usize = H_WORK + U64;
/// Format: the header's section count, after the time.
const H_SECTIONS: usize = H_TIME + U64;
/// Format: the header's CRC, after the section count and two zero bytes: the bytes its CRC covers.
const HEADER_COVERED: usize = H_SECTIONS + U16 + U16;
/// Format: the header's bytes, its CRC last.
pub const HEADER_BYTES: usize = HEADER_COVERED + U32;

/// Format: a section head's flags byte, after its kind byte (`0` for a closed section,
/// [`SECTION_OPEN_FLAG`] for an open section written before its bytes stream).
const S_FLAGS: usize = 1;
/// Format: flags byte value marking an open, uncommitted section head authenticated against
/// `(seal, section_head_offset)` rather than holding a payload CRC.
const SECTION_OPEN_FLAG: u8 = 0x80;
/// Format: a section head's two reserved zero bytes, after its flags byte (the length is `u64`-aligned).
const S_RESERVED: usize = S_FLAGS + 1;
/// Format: a section head's length, after its reserved bytes.
const S_LEN: usize = S_RESERVED + U16;
/// Format: a section head's CRC, after its length.
const S_CRC: usize = S_LEN + U64;
/// Format: a section head's bytes.
pub const SECTION_HEAD_BYTES: usize = S_CRC + U32;

/// Format: the trailer's commit magic, after the identity.
const T_COMMIT: usize = IDENTITY;
/// Format: the trailer's record length, after the commit magic.
const T_LEN: usize = T_COMMIT + COMMIT.len();
/// Format: the trailer's CRC, after the record length: the bytes its CRC covers.
const TRAILER_COVERED: usize = T_LEN + U64;
/// Format: the trailer's bytes, its CRC last.
pub const TRAILER_BYTES: usize = TRAILER_COVERED + U32;
/// Format: a section's label in its record's identity: its kind byte and its length.
const LABEL_BYTES: usize = 1 + U64;
/// Format: the bytes a recovery's verification reads at a time, a buffer it keeps across sections. The
/// identity's hash and the CRC take pieces of any length; this is the read's size, not a tuning of either.
const VERIFY_PIECE: usize = 1 << 16;
/// Format: the fewest bytes a committed record can occupy: a zero-section header and its commit trailer.
const MIN_RECORD_BYTES: usize = HEADER_BYTES + TRAILER_BYTES;
/// Format: bytes overlapped between adjacent [`VERIFY_PIECE`] scan chunks so every candidate record
/// header in `[scan_start, len - MIN_RECORD_BYTES]` lies wholly inside one chunk and rejects false
/// [`MAGIC`] bytes in memory without disk reads.
const HEADER_OVERLAP: usize = HEADER_BYTES.saturating_sub(1);

/// The tool whose changeset a record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
  /// git.
  Git,
  /// Mercurial.
  Mercurial,
  /// Subversion.
  Subversion,
}

impl Tool {
  /// The tool's byte in a header and a row.
  pub(crate) fn byte(self) -> u8 {
    // Format: the tool's byte in the log (the format agreed with w-hg, w-git, w-svn).
    match self {
      Tool::Git => 1,
      Tool::Mercurial => 2,
      Tool::Subversion => 3,
    }
  }

  fn from_byte(byte: u8) -> Option<Self> {
    // Format: the tool's byte in the log, as `byte` writes it.
    match byte {
      1 => Some(Tool::Git),
      2 => Some(Tool::Mercurial),
      3 => Some(Tool::Subversion),
      _ => None,
    }
  }
}

/// What a section holds (A-767 item 2; the format agreed with w-hg, w-git, w-svn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
  /// The tool's own record bytes (git's update commands and push entry, Mercurial's heads, bookmarks, phases and
  /// obsolescence parts, Subversion's revision record).
  Record,
  /// The bytes the client sent, or the normalized edits and wholes they land as.
  Payload,
  /// The work's path changes: path, change and content identity.
  Work,
  /// Each object's tool id and place in the payload, where the arrival computed them.
  Names,
  /// Each touched path's full-text identity and length, with its edit's base (w-hg's ask).
  Texts,
}

impl SectionKind {
  fn byte(self) -> u8 {
    // Format: a section's kind byte (A-767 item 2; the format agreed with w-hg, w-git, w-svn).
    match self {
      SectionKind::Record => 1,
      SectionKind::Payload => 2,
      SectionKind::Work => 3,
      // Format: as above, in the agreed order.
      SectionKind::Names => 4,
      SectionKind::Texts => 5,
    }
  }

  fn from_byte(byte: u8) -> Option<Self> {
    // Format: a section's kind byte, as `byte` writes it.
    match byte {
      1 => Some(SectionKind::Record),
      2 => Some(SectionKind::Payload),
      3 => Some(SectionKind::Work),
      // Format: as above, in the agreed order.
      4 => Some(SectionKind::Names),
      5 => Some(SectionKind::Texts),
      _ => None,
    }
  }
}

/// A record's header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
  /// The tool.
  pub tool: Tool,
  /// The repository (slates' 128-bit repository id, A-515).
  pub repository: u128,
  /// The changeset's sequence in its repository.
  pub sequence: u64,
  /// The sequence of the changeset it was judged over.
  pub judged_over: u64,
  /// The standing grant it landed under.
  pub grant: u64,
  /// The work that carried it.
  pub work: u64,
  /// When it landed, the caller's clock in ns.
  pub time_ns: u64,
  /// Its sections.
  pub sections: u16,
}

/// A committed record: where it is, its header, its identity and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located {
  /// The record's first byte in the log.
  pub offset: u64,
  /// Its header.
  pub header: Header,
  /// Its identity (keyed BLAKE3 bound to `(offset, seal)` over the header and each section's kind,
  /// length and bytes).
  pub identity: [u8; IDENTITY],
  /// Its whole length, trailer included.
  pub len: u64,
}

impl Located {
  /// Where the next record begins.
  pub fn end(&self) -> u64 {
    self.offset.saturating_add(self.len)
  }
}

/// One section of a committed record: its kind, where its bytes are, and its CRC-32C checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
  /// The kind.
  pub kind: SectionKind,
  /// Its first byte in the log.
  pub offset: u64,
  /// Its length.
  pub len: u64,
  /// CRC-32C of the section's bytes, as recorded in its closed section head.
  pub crc: u32,
}

/// Incremental CRC-32C verifier for a [`Section`]'s bytes streamed across pieces.
#[derive(Clone, Copy, Debug)]
pub struct SectionCrc {
  expected: u32,
  running: u32,
}

impl SectionCrc {
  /// Starts verifying a section whose closed head recorded `expected` CRC-32C.
  pub fn new(expected: u32) -> Self {
    Self {
      expected,
      running: 0,
    }
  }

  /// Feeds the next contiguous piece of the section's bytes into the running CRC-32C.
  pub fn update(&mut self, piece: &[u8]) {
    self.running = mantle_crc::crc32c_extend(self.running, piece);
  }

  /// Verifies that the accumulated CRC-32C matches the section head's checksum.
  pub fn finish(self) -> Result<(), LogError> {
    if self.running == self.expected {
      Ok(())
    } else {
      Err(LogError::Corrupt("a section's bytes"))
    }
  }
}

impl Section {
  /// Verifies that `bytes` matches this section's recorded length and CRC-32C.
  pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), LogError> {
    let len = u64::try_from(bytes.len()).map_err(|_| LogError::TooLarge)?;
    if len != self.len || mantle_crc::crc32c(bytes) != self.crc {
      return Err(LogError::Corrupt("a section's bytes"));
    }
    Ok(())
  }

  /// Starts an incremental CRC-32C verifier for streaming this section piece by piece.
  pub fn crc_verifier(&self) -> SectionCrc {
    SectionCrc::new(self.crc)
  }
}

/// Why the log refused.
#[derive(Debug)]
pub enum LogError {
  /// The file refused a read, a write or the flush. After a failed flush the log trusts nothing it wrote since its
  /// last commit (Rebello et al., ATC 2020): the record is not committed.
  Disk(DiskError),
  /// A record appended out of order: more sections than its header says, a section's bytes past its length, or a
  /// commit before every section was written whole.
  OutOfOrder(&'static str),
  /// An offset or length past what the format or the file can hold.
  TooLarge,
  /// A record read where a committed one was expected does not verify (the caller's offset is not a record's start, or
  /// the bytes changed under the log).
  Corrupt(&'static str),
  /// The changeset log's on-disk format version is not supported by this binary.
  UnsupportedFormat(u16),
}

impl fmt::Display for LogError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Disk(error) => write!(f, "the changeset log's file: {error}"),
      Self::OutOfOrder(what) => write!(f, "a changeset appended out of order: {what}"),
      Self::TooLarge => f.write_str("a changeset past what the log can hold"),
      Self::Corrupt(what) => write!(f, "a changeset that does not verify: {what}"),
      Self::UnsupportedFormat(found) => write!(
        f,
        "unsupported changeset log format version {found} (this binary expects format {FORMAT})"
      ),
    }
  }
}

/// Result of inspecting the leading bytes of a non-empty store changeset log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealPreamble {
  /// Valid `SLSE` preamble matching [`FORMAT`], carrying the store's 32-byte seal.
  Valid([u8; SEAL_BYTES]),
  /// Recognized `SLSE` or legacy unprefixed `SLCS` log whose format version differs from [`FORMAT`].
  UnsupportedFormat(u16),
  /// Damaged, truncated, or unrecognized preamble bytes.
  Damaged,
}

impl std::error::Error for LogError {}

impl From<DiskError> for LogError {
  fn from(error: DiskError) -> Self {
    Self::Disk(error)
  }
}

/// Per-store 32-byte secret key authenticating record headers and BLAKE3 identities; redacted in
/// `Debug` output so secret bytes are never logged.
#[derive(Clone, Copy)]
struct Seal([u8; SEAL_BYTES]);

impl fmt::Debug for Seal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("Seal([redacted])")
  }
}

/// The changeset log over the block file `F`.
#[derive(Debug)]
pub struct ChangesetLog<F: BlockFile> {
  file: F,
  /// Per-store 32-byte secret key mixed into every record header CRC and BLAKE3 identity.
  seal: Seal,
  /// Where the committed records end: the next append's offset.
  end: u64,
  /// Damaged committed records or broken mid-file spans skipped during recovery (non-vacuity counter).
  skipped: u64,
  /// The record being appended, if one is: its state lives here, not in a borrow, so a record appended a slice at a
  /// time over many requests leaves the log readable between them (the owner's sliced landing).
  open: Option<OpenRecord>,
}

/// A record being appended: its header, where it starts and where its next byte goes, its identity so far, the sections
/// begun and the open one.
#[derive(Debug)]
struct OpenRecord {
  header: Header,
  start: u64,
  at: u64,
  identity: blake3::Hasher,
  written: u16,
  section: Option<OpenSection>,
}

impl<F: BlockFile> ChangesetLog<F> {
  /// Opens the log over `file` with [`UNSEALED`] (records still bind to their exact file offset),
  /// recovering committed records from `from` on and calling `each` with every verified record.
  pub fn recover(file: F, from: u64, each: impl FnMut(&F, &Located)) -> Result<Self, LogError> {
    Self::recover_sealed(file, from, UNSEALED, None, each)
  }

  /// Opens the log as [`ChangesetLog::recover`] does, additionally filtering any byte-scan
  /// resynchronization candidate against `expected_grant` when `Some`.
  pub fn recover_for_grant(
    file: F,
    from: u64,
    expected_grant: Option<u64>,
    each: impl FnMut(&F, &Located),
  ) -> Result<Self, LogError> {
    Self::recover_sealed(file, from, UNSEALED, expected_grant, each)
  }

  /// Opens the log over `file` from committed offset `from`, authenticating every record's header
  /// CRC and keyed BLAKE3 identity against `(offset, seal)` (and filtering resynchronization
  /// candidates against `expected_grant` when `Some`) and storing `seal` for all future appends.
  pub fn recover_sealed(
    file: F,
    from: u64,
    seal: [u8; SEAL_BYTES],
    expected_grant: Option<u64>,
    mut each: impl FnMut(&F, &Located),
  ) -> Result<Self, LogError> {
    let len = file.len()?;
    if from > len {
      return Err(LogError::Corrupt("a recovery offset past the log's end"));
    }
    let mut progress = RecoverState {
      end: from,
      cursor: from,
      skipped: 0,
      seal,
      expected_grant,
    };
    let mut verify_buf = Vec::new();
    let mut scan_buf = Vec::new();
    while progress.cursor < len {
      let keep_going = step_recover(
        &file,
        len,
        &mut progress,
        (&mut scan_buf, &mut verify_buf),
        &mut each,
      )?;
      if !keep_going {
        break;
      }
    }
    Ok(Self {
      file,
      seal: Seal(seal),
      end: progress.end,
      skipped: progress.skipped,
      open: None,
    })
  }

  /// The 32-byte seal this log authenticates its records with.
  pub fn seal(&self) -> [u8; SEAL_BYTES] {
    self.seal.0
  }

  /// Where the committed records end.
  pub fn end(&self) -> u64 {
    self.end
  }

  /// Damaged committed records or broken mid-file spans skipped during recovery.
  pub fn skipped(&self) -> u64 {
    self.skipped
  }

  /// The block file under the log.
  pub fn file(&self) -> &F {
    &self.file
  }

  /// The block file, the log let go (a recovery opens a new log over it).
  pub fn into_file(self) -> F {
    self.file
  }

  /// Begins a record with `header` where the log ends ([`Appending`], which abandons the record when dropped uncommitted).
  pub fn begin(&mut self, header: Header) -> Result<Appending<'_, F>, LogError> {
    self.open_record(header)?;
    Ok(Appending { log: self })
  }

  /// Opens a record with `header` where the log ends, its header written. Refused while another record is open.
  pub fn open_record(&mut self, header: Header) -> Result<(), LogError> {
    if self.open.is_some() {
      return Err(LogError::OutOfOrder("a record is already open"));
    }
    let start = self.end;
    let mut bytes = [0u8; HEADER_BYTES];
    encode_header(&header, start, &self.seal.0, &mut bytes);
    self.file.write_all_at(&bytes, start)?;
    let identity = new_record_hasher(&self.seal.0, start, &bytes);
    self.open = Some(OpenRecord {
      header,
      start,
      at: offset_after(start, HEADER_BYTES)?,
      identity,
      written: 0,
      section: None,
    });
    Ok(())
  }

  /// Whether a record is open.
  pub fn is_open(&self) -> bool {
    self.open.is_some()
  }

  /// Begins the open record's next section, `len` bytes of `kind`; writes an authenticated open-section head before its
  /// bytes follow through [`ChangesetLog::open_piece`].
  pub fn open_section(&mut self, kind: SectionKind, len: u64) -> Result<(), LogError> {
    self.close_section()?;
    let seal = self.seal.0;
    let record = self
      .open
      .as_mut()
      .ok_or(LogError::OutOfOrder("a section with no record open"))?;
    if record.written >= record.header.sections {
      return Err(LogError::OutOfOrder("more sections than the header names"));
    }
    let head = record.at;
    let next_at = offset_after(head, SECTION_HEAD_BYTES)?;
    let mut open_head = [0u8; SECTION_HEAD_BYTES];
    encode_open_section_head(kind, len, head, &seal, &mut open_head);
    self.file.write_all_at(&open_head, head)?;
    record.at = next_at;
    record.written = record.written.saturating_add(1);
    record.identity.update(&section_label(kind, len));
    record.section = Some(OpenSection {
      head,
      kind,
      len,
      done: 0,
      crc: 0,
    });
    Ok(())
  }

  /// Writes the next `bytes` of the open record's open section.
  pub fn open_piece(&mut self, bytes: &[u8]) -> Result<(), LogError> {
    let record = self
      .open
      .as_mut()
      .ok_or(LogError::OutOfOrder("bytes with no record open"))?;
    let Some(mut section) = record.section else {
      return Err(LogError::OutOfOrder("bytes with no section begun"));
    };
    let count = u64::try_from(bytes.len()).map_err(|_| LogError::TooLarge)?;
    let done = section.done.checked_add(count).ok_or(LogError::TooLarge)?;
    if done > section.len {
      return Err(LogError::OutOfOrder("a section's bytes past its length"));
    }
    self.file.write_all_at(bytes, record.at)?;
    record.at = record.at.checked_add(count).ok_or(LogError::TooLarge)?;
    record.identity.update(bytes);
    section.crc = mantle_crc::crc32c_extend(section.crc, bytes);
    section.done = done;
    record.section = Some(section);
    Ok(())
  }

  /// Writes the open section's head now that its bytes are whole and their CRC known.
  fn close_section(&mut self) -> Result<(), LogError> {
    let Some(section) = self.open.as_ref().and_then(|record| record.section) else {
      return Ok(());
    };
    if section.done != section.len {
      return Err(LogError::OutOfOrder(
        "a section closed before its bytes were whole",
      ));
    }
    let mut head = [0u8; SECTION_HEAD_BYTES];
    encode_section_head(section.kind, section.len, section.crc, &mut head);
    self.file.write_all_at(&head, section.head)?;
    if let Some(record) = self.open.as_mut() {
      record.section = None;
    }
    Ok(())
  }

  /// Commits the open record: the trailer written, the log flushed, and only then the log's end moved past it. Refused,
  /// with nothing committed and the record let go, when a section is not whole or fewer sections were written than the
  /// header names.
  pub fn commit_open(&mut self) -> Result<Located, LogError> {
    let closed = self.close_section();
    let record = self
      .open
      .take()
      .ok_or(LogError::OutOfOrder("a commit with no record open"))?;
    closed?;
    if record.written != record.header.sections {
      return Err(LogError::OutOfOrder("fewer sections than the header names"));
    }
    let identity: [u8; IDENTITY] = *record.identity.finalize().as_bytes();
    let past = offset_after(record.at, TRAILER_BYTES)?;
    let len = past.checked_sub(record.start).ok_or(LogError::TooLarge)?;
    let mut trailer = [0u8; TRAILER_BYTES];
    encode_trailer(&identity, len, &mut trailer);
    self.file.write_all_at(&trailer, record.at)?;
    self.file.sync_data()?;
    self.end = past;
    Ok(Located {
      offset: record.start,
      header: record.header,
      identity,
      len,
    })
  }

  /// Lets the open record go uncommitted: the log's end stays where it was, and the next record overwrites its bytes.
  pub fn abandon_open(&mut self) {
    self.open = None;
  }

  /// The sections of the committed record `located`, in order.
  pub fn sections(&self, located: &Located) -> Result<Vec<Section>, LogError> {
    sections_in(&self.file, located)
  }

  /// The header of the committed record at `offset` (a row's offset): read and its CRC checked.
  pub fn header_at(&self, offset: u64) -> Result<Header, LogError> {
    header_in(&self.file, offset, self.end, &self.seal.0)
  }

  /// Reads `buf.len()` bytes of the log from `offset` (inside a committed record's section, by its [`Section`]).
  pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<(), LogError> {
    let past = offset_after(offset, buf.len())?;
    if past > self.end {
      return Err(LogError::Corrupt("a read past the committed records"));
    }
    self.file.read_exact_at(buf, offset)?;
    Ok(())
  }
}

/// A record being appended in one borrow of the log: its sections written in order, its commit the last step
/// ([`Appending::commit`]). Dropped before its commit, it lets the record go and leaves the log's end where it was.
#[derive(Debug)]
pub struct Appending<'a, F: BlockFile> {
  log: &'a mut ChangesetLog<F>,
}

#[derive(Debug, Clone, Copy)]
struct OpenSection {
  head: u64,
  kind: SectionKind,
  len: u64,
  done: u64,
  crc: u32,
}

impl<F: BlockFile> Appending<'_, F> {
  /// Begins the next section, `len` bytes of `kind`; its bytes follow through [`Appending::piece`].
  pub fn section(&mut self, kind: SectionKind, len: u64) -> Result<(), LogError> {
    self.log.open_section(kind, len)
  }

  /// Writes the next `bytes` of the open section.
  pub fn piece(&mut self, bytes: &[u8]) -> Result<(), LogError> {
    self.log.open_piece(bytes)
  }

  /// Commits the record ([`ChangesetLog::commit_open`]).
  pub fn commit(self) -> Result<Located, LogError> {
    self.log.commit_open()
  }
}

impl<F: BlockFile> Drop for Appending<'_, F> {
  fn drop(&mut self) {
    self.log.abandon_open();
  }
}

/// Reads the checked header at a row's offset within the committed prefix ending at `end`.
pub(crate) fn header_in<F: BlockFile>(
  file: &F,
  offset: u64,
  end: u64,
  seal: &[u8; SEAL_BYTES],
) -> Result<Header, LogError> {
  if offset_after(offset, HEADER_BYTES)? > end {
    return Err(LogError::Corrupt("a header past the committed records"));
  }
  let mut head = [0u8; HEADER_BYTES];
  file.read_exact_at(&mut head, offset)?;
  decode_header(&head, offset, seal).ok_or(LogError::Corrupt("a header"))
}

/// Reads one closed section head at `at` bounded within `sections_end`, returning the section and
/// the offset immediately after its body.
fn read_bounded_section<F: BlockFile>(
  file: &F,
  at: u64,
  sections_end: u64,
) -> Result<(Section, u64), LogError> {
  let offset =
    offset_after(at, SECTION_HEAD_BYTES).map_err(|_| LogError::Corrupt("a section's length"))?;
  if offset > sections_end {
    return Err(LogError::Corrupt("a section's length"));
  }
  let mut head = [0u8; SECTION_HEAD_BYTES];
  file.read_exact_at(&mut head, at)?;
  let (kind, len, crc) = decode_section_head(&head).ok_or(LogError::Corrupt("a section's head"))?;
  let next = offset
    .checked_add(len)
    .ok_or(LogError::Corrupt("a section's length"))?;
  if next > sections_end {
    return Err(LogError::Corrupt("a section's length"));
  }
  Ok((
    Section {
      kind,
      offset,
      len,
      crc,
    },
    next,
  ))
}

/// The sections of the committed record `located` in `file`, in order: each head read for its
/// kind, offset, length, and CRC-32C, bounded strictly by `located`'s record frame.
pub fn sections_in<F: BlockFile>(file: &F, located: &Located) -> Result<Vec<Section>, LogError> {
  let record_end = located
    .offset
    .checked_add(located.len)
    .ok_or(LogError::Corrupt("a record's length"))?;
  let trailer_bytes = u64::try_from(TRAILER_BYTES).map_err(|_| LogError::TooLarge)?;
  let sections_end = record_end
    .checked_sub(trailer_bytes)
    .ok_or(LogError::Corrupt("a record's length"))?;
  let mut at = offset_after(located.offset, HEADER_BYTES)
    .map_err(|_| LogError::Corrupt("a record's length"))?;
  if at > sections_end {
    return Err(LogError::Corrupt("a record's length"));
  }
  let mut sections = Vec::new();
  sections
    .try_reserve_exact(usize::from(located.header.sections))
    .map_err(|_| LogError::TooLarge)?;
  for _ in 0..located.header.sections {
    let (section, next) = read_bounded_section(file, at, sections_end)?;
    sections.push(section);
    at = next;
  }
  if at != sections_end {
    return Err(LogError::Corrupt("a record's section layout"));
  }
  Ok(sections)
}

/// BLAKE3 of `len` bytes of `file` from `offset`, read `VERIFY_PIECE` at a time into `buffer`.
pub fn identity_of_range<F: BlockFile>(
  file: &F,
  offset: u64,
  len: u64,
  buffer: &mut Vec<u8>,
) -> Result<[u8; IDENTITY], LogError> {
  let mut hasher = blake3::Hasher::new();
  hash_range(file, &mut hasher, offset, len, buffer)?;
  Ok(*hasher.finalize().as_bytes())
}

/// BLAKE3 of `section`'s bytes in `file`, refusing [`LogError::Corrupt`] if their CRC-32C does
/// not match `section.crc`.
pub fn verified_identity_of_section<F: BlockFile>(
  file: &F,
  section: &Section,
  buffer: &mut Vec<u8>,
) -> Result<[u8; IDENTITY], LogError> {
  let mut hasher = blake3::Hasher::new();
  let crc = hash_range(file, &mut hasher, section.offset, section.len, buffer)?;
  if crc != section.crc {
    return Err(LogError::Corrupt("a section's bytes"));
  }
  Ok(*hasher.finalize().as_bytes())
}

/// Format: what a section adds to its record's identity before its bytes: its kind and its length.
fn section_label(kind: SectionKind, len: u64) -> [u8; LABEL_BYTES] {
  let mut label = [0u8; LABEL_BYTES];
  if let Some(byte) = label.first_mut() {
    *byte = kind.byte();
  }
  put_u64(&mut label, 1, len);
  label
}

/// `offset` plus `count` bytes, refused past `u64`.
fn offset_after(offset: u64, count: usize) -> Result<u64, LogError> {
  let count = u64::try_from(count).map_err(|_| LogError::TooLarge)?;
  offset.checked_add(count).ok_or(LogError::TooLarge)
}

/// Feeds `len` bytes of `file` from `offset` to `hasher`, `VERIFY_PIECE` at a time, and gives back their CRC-32C.
fn hash_range<F: BlockFile>(
  file: &F,
  hasher: &mut blake3::Hasher,
  offset: u64,
  len: u64,
  buffer: &mut Vec<u8>,
) -> Result<u32, LogError> {
  buffer.resize(VERIFY_PIECE, 0);
  let mut crc = 0u32;
  let mut done = 0u64;
  while done < len {
    let left = len.checked_sub(done).ok_or(LogError::TooLarge)?;
    let take =
      usize::try_from(left.min(u64::try_from(VERIFY_PIECE).map_err(|_| LogError::TooLarge)?))
        .map_err(|_| LogError::TooLarge)?;
    let piece = buffer.get_mut(..take).ok_or(LogError::TooLarge)?;
    file.read_exact_at(piece, offset.checked_add(done).ok_or(LogError::TooLarge)?)?;
    hasher.update(piece);
    crc = mantle_crc::crc32c_extend(crc, piece);
    done = done
      .checked_add(u64::try_from(take).map_err(|_| LogError::TooLarge)?)
      .ok_or(LogError::TooLarge)?;
  }
  Ok(crc)
}

/// The record's `sections` sections from `at` in a file of `len` bytes, each read whole within the file, its label and
/// bytes hashed into `identity`, its bytes matching its CRC: where they end, or `None` where one does not verify.
fn sections_verified<F: BlockFile>(
  file: &F,
  (mut at, len): (u64, u64),
  sections: u16,
  identity: &mut blake3::Hasher,
  buffer: &mut Vec<u8>,
) -> Result<Option<u64>, LogError> {
  for _ in 0..sections {
    if !offset_after(at, SECTION_HEAD_BYTES).is_ok_and(|past| past <= len) {
      return Ok(None);
    }
    let mut section = [0u8; SECTION_HEAD_BYTES];
    file.read_exact_at(&mut section, at)?;
    let Some((kind, section_len, crc)) = decode_section_head(&section) else {
      return Ok(None);
    };
    let bytes_at = offset_after(at, SECTION_HEAD_BYTES)?;
    let Some(past) = bytes_at
      .checked_add(section_len)
      .filter(|past| *past <= len)
    else {
      return Ok(None);
    };
    identity.update(&section_label(kind, section_len));
    if hash_range(file, identity, bytes_at, section_len, buffer)? != crc {
      return Ok(None);
    }
    at = past;
  }
  Ok(Some(at))
}

/// A record whose header, section heads, and `SLCC` commit trailer all decoded and matched lengths.
struct Framed {
  header: Header,
  head_bytes: [u8; HEADER_BYTES],
  recorded: [u8; IDENTITY],
  past: u64,
}

/// Mutable cursor and accounting state carried through [`ChangesetLog::recover_sealed`].
struct RecoverState {
  end: u64,
  cursor: u64,
  skipped: u64,
  seal: [u8; SEAL_BYTES],
  expected_grant: Option<u64>,
}

/// Outcome of framing a record's section heads without reading payload bytes.
enum SectionsFrame {
  /// Every section head decoded cleanly and fit within `len`, ending at this trailer offset.
  Complete(u64),
  /// A genuinely authenticated open-section head after `prior_sections` closed sections whose
  /// declared remaining record cannot finish before EOF (an interrupted uncommitted write at EOF).
  UnwrittenTail { prior_sections: u16 },
  /// A section head failed to decode (including all-zeroes) or a section ran past `len`.
  Broken,
  /// The resync section-head read budget ran out before all sections were framed.
  BudgetExhausted,
}

/// Structural framing check of a record at `offset` before hashing any section payload.
enum Frame {
  /// Valid `(offset, seal)` header, valid section heads fitting in the file, and a valid `SLCC`
  /// commit trailer whose `record_len` matches `past - offset`.
  Committed(Framed),
  /// Valid `(offset, seal)` header followed by an authenticated open-section head after
  /// `prior_sections` framed sections whose declared remaining record cannot finish before EOF.
  UnwrittenTail { prior_sections: u16 },
  /// Header, section heads, or commit trailer failed verification or ran past `len`.
  Broken,
  /// The resync section-head read budget ran out while framing this candidate.
  BudgetExhausted,
}

/// Outcome of verifying a record at `offset`.
enum Verify {
  /// A valid store `SLSE` seal preamble at offset `0` matching this recovery's `seal`.
  Preamble { next: u64 },
  /// A verified committed record.
  Valid(Located),
  /// A structurally committed record whose header, section heads, and `SLCC` trailer all verified
  /// at `offset`, but whose payload CRC or keyed BLAKE3 identity failed: never indexed, skip
  /// directly to `next`.
  Damaged { next: u64 },
  /// Valid `(offset, seal)` header followed by a genuinely authenticated open-section head whose
  /// declared remaining record cannot finish before EOF and whose prior sections all verify: stop
  /// cleanly without scanning the uncommitted tail payload.
  UnwrittenTail,
  /// Header, section head, or commit trailer failed verification (or ran past `len`): never trust
  /// an unverified `section.len` to jump or stop early; resync linearly from `cursor + 1`.
  Broken,
  /// The resync section-head read budget ran out while framing this candidate.
  BudgetExhausted,
}

/// Whether framing runs at the recovery cursor (unbudgeted, may confirm an unwritten tail) or
/// inside a linear resync scan (strictly bounded by section-head reads).
enum FrameContext<'a> {
  /// Top-level verification at the recovery cursor.
  Cursor,
  /// Candidate probing inside linear resync, bounded by remaining section-head reads.
  Resync(&'a mut u64),
}

/// Outcome of scanning one [`VERIFY_PIECE`] chunk for the next committed record.
enum ChunkProbe {
  /// Found a verified committed record.
  Found(Located),
  /// No verified record starts in this chunk.
  Exhausted,
  /// The linear section-head read budget for this resync scan ran out.
  BudgetExhausted,
}

/// `at + count` when it fits within `len`, or `None` when it runs past `len`.
fn fits_after(at: u64, count: usize, len: u64) -> Result<Option<u64>, LogError> {
  let past = offset_after(at, count)?;
  Ok((past <= len).then_some(past))
}

/// Consumes one section-head read from `context` when scanning, returning `false` when exhausted.
fn spend_read(context: &mut FrameContext<'_>) -> bool {
  let FrameContext::Resync(remaining) = context else {
    return true;
  };
  if **remaining == 0 {
    return false;
  }
  **remaining = remaining.saturating_sub(1);
  true
}

/// Returns `true` when an open section starting at `bytes_at` with `declared_len`, followed by
/// `remaining_sections` minimum section heads and the commit trailer, cannot finish within `len`.
fn open_record_exceeds_file(
  bytes_at: u64,
  declared_len: u64,
  remaining_sections: u16,
  len: u64,
) -> bool {
  let remaining_heads = u64::try_from(SECTION_HEAD_BYTES)
    .ok()
    .and_then(|width| u64::from(remaining_sections).checked_mul(width));
  let trailer = u64::try_from(TRAILER_BYTES).ok();
  let min_end = bytes_at
    .checked_add(declared_len)
    .and_then(|at| at.checked_add(remaining_heads?))
    .and_then(|at| at.checked_add(trailer?));
  min_end.is_none_or(|need| need > len)
}

/// Classifies a non-closed section head at `(at, bytes_at)` under `(seal, context)`.
fn classify_non_closed_section(
  section: &[u8; SECTION_HEAD_BYTES],
  (at, bytes_at, len): (u64, u64, u64),
  (prior_sections, remaining_sections): (u16, u16),
  seal: &[u8; SEAL_BYTES],
  context: &FrameContext<'_>,
) -> SectionsFrame {
  let open_tail = matches!(context, FrameContext::Cursor)
    && decode_open_section_head(section, at, seal).is_some_and(|(_, declared_len)| {
      open_record_exceeds_file(bytes_at, declared_len, remaining_sections, len)
    });
  if open_tail {
    SectionsFrame::UnwrittenTail { prior_sections }
  } else {
    SectionsFrame::Broken
  }
}

/// Frames one section head at `at` within a file of `len` bytes under `(seal, context)`.
/// An all-zero head is always `Broken`; only a genuinely authenticated open-section head at
/// `FrameContext::Cursor` whose declared remaining record exceeds `len` returns `UnwrittenTail`.
fn step_section<F: BlockFile>(
  file: &F,
  (at, len): (u64, u64),
  (prior_sections, remaining_sections): (u16, u16),
  seal: &[u8; SEAL_BYTES],
  context: &mut FrameContext<'_>,
) -> Result<SectionsFrame, LogError> {
  let Some(bytes_at) = fits_after(at, SECTION_HEAD_BYTES, len)? else {
    return Ok(SectionsFrame::Broken);
  };
  if !spend_read(context) {
    return Ok(SectionsFrame::BudgetExhausted);
  }
  let mut section = [0u8; SECTION_HEAD_BYTES];
  file.read_exact_at(&mut section, at)?;
  if let Some((_, section_len, _)) = decode_section_head(&section) {
    let next = bytes_at
      .checked_add(section_len)
      .filter(|past| *past <= len);
    return Ok(next.map_or(SectionsFrame::Broken, SectionsFrame::Complete));
  }
  Ok(classify_non_closed_section(
    &section,
    (at, bytes_at, len),
    (prior_sections, remaining_sections),
    seal,
    context,
  ))
}

/// Walks `sections` section heads from `at` without reading payload bytes, charging each head read
/// to `context` when scanning.
fn frame_sections_budgeted<F: BlockFile>(
  file: &F,
  (mut at, len): (u64, u64),
  sections: u16,
  seal: &[u8; SEAL_BYTES],
  context: &mut FrameContext<'_>,
) -> Result<SectionsFrame, LogError> {
  for prior_sections in 0..sections {
    let remaining_sections = sections.saturating_sub(prior_sections.saturating_add(1));
    match step_section(
      file,
      (at, len),
      (prior_sections, remaining_sections),
      seal,
      context,
    )? {
      SectionsFrame::Complete(next) => at = next,
      other => return Ok(other),
    }
  }
  Ok(SectionsFrame::Complete(at))
}

/// Reads and checks the commit trailer at `at` for a record spanning `[offset, past)`.
fn frame_trailer<F: BlockFile>(
  file: &F,
  (offset, at, past): (u64, u64, u64),
  header: Header,
  head_bytes: [u8; HEADER_BYTES],
) -> Result<Frame, LogError> {
  let mut trailer = [0u8; TRAILER_BYTES];
  file.read_exact_at(&mut trailer, at)?;
  let Some((recorded, record_len)) = decode_trailer(&trailer) else {
    return Ok(Frame::Broken);
  };
  if past.checked_sub(offset) != Some(record_len) {
    return Ok(Frame::Broken);
  }
  Ok(Frame::Committed(Framed {
    header,
    head_bytes,
    recorded,
    past,
  }))
}

/// Frames the sections and commit trailer following an already-decoded header at `offset`.
fn frame_decoded_header<F: BlockFile>(
  file: &F,
  (offset, len): (u64, u64),
  (header, head_bytes): (Header, [u8; HEADER_BYTES]),
  seal: &[u8; SEAL_BYTES],
  context: &mut FrameContext<'_>,
) -> Result<Frame, LogError> {
  let Some(start) = fits_after(offset, HEADER_BYTES, len)? else {
    return Ok(Frame::Broken);
  };
  let at = match frame_sections_budgeted(file, (start, len), header.sections, seal, context)? {
    SectionsFrame::Complete(at) => at,
    SectionsFrame::UnwrittenTail { prior_sections } => {
      return Ok(Frame::UnwrittenTail { prior_sections });
    }
    SectionsFrame::Broken => return Ok(Frame::Broken),
    SectionsFrame::BudgetExhausted => return Ok(Frame::BudgetExhausted),
  };
  let Some(past) = fits_after(at, TRAILER_BYTES, len)? else {
    return Ok(Frame::Broken);
  };
  frame_trailer(file, (offset, at, past), header, head_bytes)
}

/// Confirms an `UnwrittenTail` found after `prior_sections` closed sections: verifies each prior
/// section's CRC when `prior_sections > 0` so damage in an earlier section falls back to resync.
fn verify_unwritten_tail<F: BlockFile>(
  file: &F,
  (offset, len): (u64, u64),
  (seal, head_bytes): (&[u8; SEAL_BYTES], &[u8; HEADER_BYTES]),
  prior_sections: u16,
  buffer: &mut Vec<u8>,
) -> Result<Verify, LogError> {
  if prior_sections == 0 {
    return Ok(Verify::UnwrittenTail);
  }
  let mut identity = new_record_hasher(seal, offset, head_bytes);
  let start = offset_after(offset, HEADER_BYTES)?;
  let valid = sections_verified(file, (start, len), prior_sections, &mut identity, buffer)?;
  Ok(if valid.is_some() {
    Verify::UnwrittenTail
  } else {
    Verify::Broken
  })
}

/// Verifies every section's CRC and the whole-record keyed BLAKE3 identity for a framed record.
fn verify_committed<F: BlockFile>(
  file: &F,
  (offset, len): (u64, u64),
  seal: &[u8; SEAL_BYTES],
  framed: &Framed,
  buffer: &mut Vec<u8>,
) -> Result<Verify, LogError> {
  let mut identity = new_record_hasher(seal, offset, &framed.head_bytes);
  let start = offset_after(offset, HEADER_BYTES)?;
  let verified = sections_verified(
    file,
    (start, len),
    framed.header.sections,
    &mut identity,
    buffer,
  )?;
  let Some(record_len) = framed.past.checked_sub(offset) else {
    return Ok(Verify::Broken);
  };
  let end_matches =
    verified.is_some_and(|at| offset_after(at, TRAILER_BYTES).ok() == Some(framed.past));
  if end_matches && *identity.finalize().as_bytes() == framed.recorded {
    Ok(Verify::Valid(Located {
      offset,
      header: framed.header,
      identity: framed.recorded,
      len: record_len,
    }))
  } else {
    Ok(Verify::Damaged { next: framed.past })
  }
}

/// Frames and verifies a record whose header has already been decoded at `offset`.
fn verify_decoded_header<F: BlockFile>(
  file: &F,
  (offset, len): (u64, u64),
  (header, head_bytes): (Header, [u8; HEADER_BYTES]),
  seal: &[u8; SEAL_BYTES],
  mut context: FrameContext<'_>,
  buffer: &mut Vec<u8>,
) -> Result<Verify, LogError> {
  match frame_decoded_header(
    file,
    (offset, len),
    (header, head_bytes),
    seal,
    &mut context,
  )? {
    Frame::Broken => Ok(Verify::Broken),
    Frame::BudgetExhausted => Ok(Verify::BudgetExhausted),
    Frame::UnwrittenTail { prior_sections } => match context {
      FrameContext::Cursor => verify_unwritten_tail(
        file,
        (offset, len),
        (seal, &head_bytes),
        prior_sections,
        buffer,
      ),
      FrameContext::Resync(_) => Ok(Verify::Broken),
    },
    Frame::Committed(framed) => verify_committed(file, (offset, len), seal, &framed, buffer),
  }
}

/// Checks whether `prefix` at offset `0` is a valid `SLSE` store preamble matching `seal`.
fn matching_preamble_at_zero(offset: u64, prefix: &[u8], seal: &[u8; SEAL_BYTES]) -> Option<u64> {
  if offset != 0 {
    return None;
  }
  let head = prefix.get(..SEAL_PREAMBLE_BYTES)?;
  (decode_seal_preamble(head) == SealPreamble::Valid(*seal))
    .then(|| offset_after(0, SEAL_PREAMBLE_BYTES).ok())
    .flatten()
}

/// Checks an offset-0 file shorter than [`HEADER_BYTES`] for an exact [`SEAL_PREAMBLE_BYTES`]
/// matching `SLSE` preamble.
fn verify_short_preamble<F: BlockFile>(
  file: &F,
  (offset, len): (u64, u64),
  seal: &[u8; SEAL_BYTES],
) -> Result<Verify, LogError> {
  if offset != 0 || fits_after(0, SEAL_PREAMBLE_BYTES, len)?.is_none() {
    return Ok(Verify::Broken);
  }
  let mut preamble = [0u8; SEAL_PREAMBLE_BYTES];
  file.read_exact_at(&mut preamble, 0)?;
  Ok(
    matching_preamble_at_zero(0, &preamble, seal)
      .map_or(Verify::Broken, |next| Verify::Preamble { next }),
  )
}

/// Reads and verifies the record at `offset` in a file of `len` bytes against `(offset, seal)`.
fn verify_at<F: BlockFile>(
  file: &F,
  (offset, len): (u64, u64),
  seal: &[u8; SEAL_BYTES],
  buffer: &mut Vec<u8>,
) -> Result<Verify, LogError> {
  if fits_after(offset, HEADER_BYTES, len)?.is_none() {
    return verify_short_preamble(file, (offset, len), seal);
  }
  let mut head_bytes = [0u8; HEADER_BYTES];
  file.read_exact_at(&mut head_bytes, offset)?;
  let Some(header) = decode_header(&head_bytes, offset, seal) else {
    return Ok(
      matching_preamble_at_zero(offset, &head_bytes, seal)
        .map_or(Verify::Broken, |next| Verify::Preamble { next }),
    );
  };
  verify_decoded_header(
    file,
    (offset, len),
    (header, head_bytes),
    seal,
    FrameContext::Cursor,
    buffer,
  )
}

/// First offset at or after `from` in `chunk` where [`MAGIC`] begins, if any.
fn find_magic(chunk: &[u8], from: usize) -> Option<usize> {
  let tail = chunk.get(from..)?;
  let relative = tail
    .windows(MAGIC.len())
    .position(|window| window == MAGIC)?;
  from.checked_add(relative)
}

/// Checks one in-memory [`MAGIC`] candidate at `chunk[at..at + HEADER_BYTES]`, rejecting false
/// occurrences against `(candidate, seal, expected_grant)` without touching disk.
fn probe_candidate<F: BlockFile>(
  file: &F,
  (candidate, len): (u64, u64),
  head_slice: &[u8],
  (seal, expected_grant): (&[u8; SEAL_BYTES], Option<u64>),
  section_budget: &mut u64,
  verify_buf: &mut Vec<u8>,
) -> Result<Option<ChunkProbe>, LogError> {
  let Ok(head_bytes): Result<[u8; HEADER_BYTES], _> = head_slice.try_into() else {
    return Ok(None);
  };
  let Some(header) = decode_header(&head_bytes, candidate, seal) else {
    return Ok(None);
  };
  if expected_grant.is_some_and(|grant| header.grant != grant) {
    return Ok(None);
  }
  match verify_decoded_header(
    file,
    (candidate, len),
    (header, head_bytes),
    seal,
    FrameContext::Resync(section_budget),
    verify_buf,
  )? {
    Verify::Valid(located) => Ok(Some(ChunkProbe::Found(located))),
    Verify::BudgetExhausted => Ok(Some(ChunkProbe::BudgetExhausted)),
    // Never jump past a `Damaged` or `UnwrittenTail` candidate during resync; advance one byte to
    // preserve linearity and prevent crafted headers from skipping healthy trailing siblings.
    Verify::Preamble { .. } | Verify::Damaged { .. } | Verify::UnwrittenTail | Verify::Broken => {
      Ok(None)
    }
  }
}

/// Probes every [`MAGIC`] candidate whose full header lies inside `chunk` (starting at `scan`),
/// rejecting false headers in memory and enforcing `section_budget` so total work stays linear.
fn probe_chunk<F: BlockFile>(
  file: &F,
  (scan, len): (u64, u64),
  chunk: &[u8],
  (seal, expected_grant): (&[u8; SEAL_BYTES], Option<u64>),
  section_budget: &mut u64,
  verify_buf: &mut Vec<u8>,
) -> Result<ChunkProbe, LogError> {
  let mut pos = 0usize;
  while let Some(at) = find_magic(chunk, pos) {
    let Some(head_slice) = chunk.get(at..at.saturating_add(HEADER_BYTES)) else {
      break;
    };
    let candidate = offset_after(scan, at)?;
    if let Some(found) = probe_candidate(
      file,
      (candidate, len),
      head_slice,
      (seal, expected_grant),
      section_budget,
      verify_buf,
    )? {
      return Ok(found);
    }
    pos = at.saturating_add(1);
  }
  Ok(ChunkProbe::Exhausted)
}

/// Derived linear budget for section-head reads across a resync scan from `scan_start` to `len`:
/// at most one section-head read per [`SECTION_HEAD_BYTES`] of scanned bytes, plus one.
fn resync_section_budget(scan_start: u64, len: u64) -> Result<u64, LogError> {
  let head_bytes = u64::try_from(SECTION_HEAD_BYTES).map_err(|_| LogError::TooLarge)?;
  Ok(
    len
      .saturating_sub(scan_start)
      .checked_div(head_bytes)
      .unwrap_or(0)
      .saturating_add(1),
  )
}

/// Scans linearly from `scan_start` in [`VERIFY_PIECE`] chunks (overlapping by [`HEADER_OVERLAP`]
/// bytes so every candidate header lies wholly in memory) for the next `(offset, seal)`-verified
/// committed record matching `expected_grant`, or `None` when none exists before `len` or the
/// linear section-head read budget runs out.
fn resync_from<F: BlockFile>(
  file: &F,
  (scan_start, len): (u64, u64),
  (seal, expected_grant): (&[u8; SEAL_BYTES], Option<u64>),
  (scan_buf, verify_buf): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<Option<Located>, LogError> {
  let min_record = u64::try_from(MIN_RECORD_BYTES).map_err(|_| LogError::TooLarge)?;
  let piece_len = u64::try_from(VERIFY_PIECE).map_err(|_| LogError::TooLarge)?;
  let overlap = u64::try_from(HEADER_OVERLAP).map_err(|_| LogError::TooLarge)?;
  let mut section_budget = resync_section_budget(scan_start, len)?;
  let mut scan = scan_start;
  scan_buf.resize(VERIFY_PIECE, 0);
  while scan.checked_add(min_record).is_some_and(|need| need <= len) {
    let left = len.checked_sub(scan).ok_or(LogError::TooLarge)?;
    let take = usize::try_from(left.min(piece_len)).map_err(|_| LogError::TooLarge)?;
    let chunk = scan_buf.get_mut(..take).ok_or(LogError::TooLarge)?;
    file.read_exact_at(chunk, scan)?;
    match probe_chunk(
      file,
      (scan, len),
      chunk,
      (seal, expected_grant),
      &mut section_budget,
      verify_buf,
    )? {
      ChunkProbe::Found(located) => return Ok(Some(located)),
      ChunkProbe::BudgetExhausted => {
        slates_machine::log_line!(
          "slates-store: changeset log stopped resync from offset {scan_start}: section-head read budget exhausted"
        );
        return Ok(None);
      }
      ChunkProbe::Exhausted => {
        let read = u64::try_from(take).map_err(|_| LogError::TooLarge)?;
        if scan.checked_add(read).is_none_or(|end| end >= len) {
          break;
        }
        let step = read.saturating_sub(overlap).max(1);
        scan = scan.checked_add(step).ok_or(LogError::TooLarge)?;
      }
    }
  }
  Ok(None)
}

/// Scans forward from `scan_start` past unframed or broken bytes starting at `cursor`; returns
/// `true` when a verified committed record was found and indexed, or `false` to stop cleanly.
fn apply_resync_scan<F: BlockFile>(
  file: &F,
  (scan_start, len): (u64, u64),
  progress: &mut RecoverState,
  buffers: (&mut Vec<u8>, &mut Vec<u8>),
  each: &mut impl FnMut(&F, &Located),
) -> Result<bool, LogError> {
  let cursor = progress.cursor;
  let Some(located) = resync_from(
    file,
    (scan_start, len),
    (&progress.seal, progress.expected_grant),
    buffers,
  )?
  else {
    return Ok(false);
  };
  progress.skipped = progress.skipped.saturating_add(1);
  slates_machine::log_line!(
    "slates-store: changeset log resynchronized past corrupt span {cursor}..{}",
    located.offset
  );
  each(file, &located);
  progress.end = located.end();
  progress.cursor = progress.end;
  Ok(true)
}

/// Returns `true` when `offset` starts a valid committed record in `file` under `seal`.
fn is_valid_record_at<F: BlockFile>(
  file: &F,
  (offset, file_len): (u64, u64),
  seal: &[u8; SEAL_BYTES],
  verify_buf: &mut Vec<u8>,
) -> Result<bool, LogError> {
  Ok(matches!(
    verify_at(file, (offset, file_len), seal, verify_buf)?,
    Verify::Valid(_)
  ))
}

/// Returns `true` when at least one committed record in `[SEAL_PREAMBLE_BYTES, file_len)` of
/// `file` verifies as [`Verify::Valid`] under `seal`: checks offset [`SEAL_PREAMBLE_BYTES`]
/// first, then optional `hint_offset` (from `rows`), and finally scans linearly with
/// [`resync_from`] in case the leading record was damaged alongside the preamble.
pub(crate) fn verify_seal_matches_log<F: BlockFile>(
  file: &F,
  file_len: u64,
  seal: &[u8; SEAL_BYTES],
  hint_offset: Option<u64>,
) -> Result<bool, LogError> {
  let preamble_len = u64::try_from(SEAL_PREAMBLE_BYTES).map_err(|_| LogError::TooLarge)?;
  if file_len <= preamble_len {
    return Ok(false);
  }
  let mut verify_buf = Vec::new();
  if is_valid_record_at(file, (preamble_len, file_len), seal, &mut verify_buf)? {
    return Ok(true);
  }
  if let Some(hint) = hint_offset.filter(|off| *off > preamble_len && *off < file_len)
    && is_valid_record_at(file, (hint, file_len), seal, &mut verify_buf)?
  {
    return Ok(true);
  }
  let mut scan_buf = Vec::new();
  Ok(
    resync_from(
      file,
      (preamble_len, file_len),
      (seal, None),
      (&mut scan_buf, &mut verify_buf),
    )?
    .is_some(),
  )
}

/// Runs one step of [`ChangesetLog::recover_sealed`]; returns `false` when recovery stops cleanly.
fn step_recover<F: BlockFile>(
  file: &F,
  len: u64,
  progress: &mut RecoverState,
  buffers: (&mut Vec<u8>, &mut Vec<u8>),
  each: &mut impl FnMut(&F, &Located),
) -> Result<bool, LogError> {
  let cursor = progress.cursor;
  match verify_at(file, (cursor, len), &progress.seal, buffers.1)? {
    Verify::Preamble { next } => {
      progress.end = next;
      progress.cursor = next;
      Ok(true)
    }
    Verify::Valid(located) => {
      each(file, &located);
      progress.end = located.end();
      progress.cursor = progress.end;
      Ok(true)
    }
    Verify::Damaged { next } => {
      progress.skipped = progress.skipped.saturating_add(1);
      slates_machine::log_line!(
        "slates-store: changeset log skipped damaged committed record at offset {cursor} (len {})",
        next.saturating_sub(cursor)
      );
      progress.end = next;
      progress.cursor = next;
      Ok(true)
    }
    Verify::UnwrittenTail => {
      slates_machine::log_line!(
        "slates-store: changeset log stopped at uncommitted open tail offset {cursor} ({} trailing bytes)",
        len.saturating_sub(cursor)
      );
      Ok(false)
    }
    Verify::BudgetExhausted => Ok(false),
    Verify::Broken => apply_resync_scan(
      file,
      (cursor.saturating_add(1), len),
      progress,
      buffers,
      each,
    ),
  }
}

/// Encodes the 48-byte `SLSE` store seal preamble carrying [`FORMAT`] and `seal`.
pub fn encode_seal_preamble(seal: &[u8; SEAL_BYTES]) -> [u8; SEAL_PREAMBLE_BYTES] {
  let mut out = [0u8; SEAL_PREAMBLE_BYTES];
  put_bytes(&mut out, 0, &SEAL_MAGIC);
  put_u16(&mut out, P_FORMAT, FORMAT);
  put_bytes(&mut out, P_SEAL, seal);
  let crc = out
    .get(..SEAL_PREAMBLE_COVERED)
    .map_or(0, mantle_crc::crc32c);
  put_u32(&mut out, SEAL_PREAMBLE_COVERED, crc);
  put_u32(&mut out, P_CRC_COMPLEMENT, !crc);
  out
}

/// Validates the 48-byte `SLSE` preamble's length and CRC-32C + bitwise complement first so a
/// single-bit flip in `P_FORMAT` classifies as [`SealPreamble::Damaged`], then checks `P_FORMAT`
/// and reserved zero bytes.
fn verify_seal_preamble(prefix: &[u8]) -> SealPreamble {
  if prefix.len() != SEAL_PREAMBLE_BYTES {
    return SealPreamble::Damaged;
  }
  let Some(covered) = prefix.get(..SEAL_PREAMBLE_COVERED) else {
    return SealPreamble::Damaged;
  };
  let expected = mantle_crc::crc32c(covered);
  let crc_ok = get_u32(prefix, SEAL_PREAMBLE_COVERED) == Some(expected)
    && get_u32(prefix, P_CRC_COMPLEMENT) == Some(!expected);
  if !crc_ok {
    return SealPreamble::Damaged;
  }
  let Some(found) = get_u16(prefix, P_FORMAT) else {
    return SealPreamble::Damaged;
  };
  if found != FORMAT {
    return SealPreamble::UnsupportedFormat(found);
  }
  let Some(reserved) = prefix.get(P_RESERVED..P_SEAL) else {
    return SealPreamble::Damaged;
  };
  if reserved.iter().any(|byte| *byte != 0) {
    return SealPreamble::Damaged;
  }
  get_array::<SEAL_BYTES>(prefix, P_SEAL).map_or(SealPreamble::Damaged, SealPreamble::Valid)
}

/// Decodes the leading bytes (`1..=SEAL_PREAMBLE_BYTES`) of a non-empty store changeset log.
pub fn decode_seal_preamble(prefix: &[u8]) -> SealPreamble {
  let Some(magic) = get_array::<{ SEAL_MAGIC.len() }>(prefix, 0) else {
    return SealPreamble::Damaged;
  };
  if magic == SEAL_MAGIC {
    return verify_seal_preamble(prefix);
  }
  if magic == MAGIC
    && let Some(found) = get_u16(prefix, H_FORMAT)
    && found != FORMAT
  {
    return SealPreamble::UnsupportedFormat(found);
  }
  SealPreamble::Damaged
}

fn put_bytes(out: &mut [u8], at: usize, value: &[u8]) {
  if let Some(slot) = at
    .checked_add(value.len())
    .and_then(|end| out.get_mut(at..end))
  {
    slot.copy_from_slice(value);
  }
}

fn put_u16(out: &mut [u8], at: usize, value: u16) {
  put_bytes(out, at, &value.to_le_bytes());
}

fn put_u32(out: &mut [u8], at: usize, value: u32) {
  put_bytes(out, at, &value.to_le_bytes());
}

fn put_u64(out: &mut [u8], at: usize, value: u64) {
  put_bytes(out, at, &value.to_le_bytes());
}

fn get_array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
  bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn get_u16(bytes: &[u8], at: usize) -> Option<u16> {
  get_array(bytes, at).map(u16::from_le_bytes)
}

fn get_u32(bytes: &[u8], at: usize) -> Option<u32> {
  get_array(bytes, at).map(u32::from_le_bytes)
}

fn get_u64(bytes: &[u8], at: usize) -> Option<u64> {
  get_array(bytes, at).map(u64::from_le_bytes)
}

/// Computes CRC-32C bound to `(seal, offset)` over `covered` (a record header or open-section head).
fn keyed_offset_crc(seal: &[u8; SEAL_BYTES], offset: u64, covered: &[u8]) -> u32 {
  let crc = mantle_crc::crc32c(seal);
  let crc = mantle_crc::crc32c_extend(crc, &offset.to_le_bytes());
  mantle_crc::crc32c_extend(crc, covered)
}

/// Initializes a record's keyed BLAKE3 identity hasher bound to `(seal, offset)` and `head_bytes`.
fn new_record_hasher(
  seal: &[u8; SEAL_BYTES],
  offset: u64,
  head_bytes: &[u8; HEADER_BYTES],
) -> blake3::Hasher {
  let mut identity = blake3::Hasher::new_keyed(seal);
  identity.update(&offset.to_le_bytes());
  identity.update(head_bytes);
  identity
}

fn encode_header(
  header: &Header,
  offset: u64,
  seal: &[u8; SEAL_BYTES],
  out: &mut [u8; HEADER_BYTES],
) {
  put_bytes(out, 0, &MAGIC);
  put_u16(out, H_FORMAT, FORMAT);
  put_bytes(out, H_TOOL, &[header.tool.byte()]);
  put_bytes(out, H_REPOSITORY, &header.repository.to_le_bytes());
  put_u64(out, H_SEQUENCE, header.sequence);
  put_u64(out, H_JUDGED, header.judged_over);
  put_u64(out, H_GRANT, header.grant);
  put_u64(out, H_WORK, header.work);
  put_u64(out, H_TIME, header.time_ns);
  put_u16(out, H_SECTIONS, header.sections);
  let crc = out
    .get(..HEADER_COVERED)
    .map_or(0, |covered| keyed_offset_crc(seal, offset, covered));
  put_u32(out, HEADER_COVERED, crc);
}

fn decode_header(
  bytes: &[u8; HEADER_BYTES],
  offset: u64,
  seal: &[u8; SEAL_BYTES],
) -> Option<Header> {
  if get_array::<{ MAGIC.len() }>(bytes, 0)? != MAGIC
    || get_u16(bytes, H_FORMAT)? != FORMAT
    || *bytes.get(H_FLAGS)? != 0
  {
    return None;
  }
  if get_u32(bytes, HEADER_COVERED)? != keyed_offset_crc(seal, offset, bytes.get(..HEADER_COVERED)?)
  {
    return None;
  }
  Some(Header {
    tool: Tool::from_byte(*bytes.get(H_TOOL)?)?,
    repository: get_array(bytes, H_REPOSITORY).map(u128::from_le_bytes)?,
    sequence: get_u64(bytes, H_SEQUENCE)?,
    judged_over: get_u64(bytes, H_JUDGED)?,
    grant: get_u64(bytes, H_GRANT)?,
    work: get_u64(bytes, H_WORK)?,
    time_ns: get_u64(bytes, H_TIME)?,
    sections: get_u16(bytes, H_SECTIONS)?,
  })
}

/// Encodes a 16-byte authenticated open-section head at `head_offset`, written before the
/// section's payload streams and overwritten by [`encode_section_head`] when the section closes.
fn encode_open_section_head(
  kind: SectionKind,
  declared_len: u64,
  head_offset: u64,
  seal: &[u8; SEAL_BYTES],
  out: &mut [u8; SECTION_HEAD_BYTES],
) {
  put_bytes(out, 0, &[kind.byte(), SECTION_OPEN_FLAG]);
  put_u64(out, S_LEN, declared_len);
  let crc = out
    .get(..S_CRC)
    .map_or(0, |covered| keyed_offset_crc(seal, head_offset, covered));
  put_u32(out, S_CRC, crc);
}

/// Decodes a 16-byte authenticated open-section head at `head_offset` against `(seal, head_offset)`.
fn decode_open_section_head(
  bytes: &[u8; SECTION_HEAD_BYTES],
  head_offset: u64,
  seal: &[u8; SEAL_BYTES],
) -> Option<(SectionKind, u64)> {
  if *bytes.get(S_FLAGS)? != SECTION_OPEN_FLAG
    || bytes.get(S_RESERVED..S_LEN)?.iter().any(|byte| *byte != 0)
  {
    return None;
  }
  let kind = SectionKind::from_byte(*bytes.first()?)?;
  let declared_len = get_u64(bytes, S_LEN)?;
  if get_u32(bytes, S_CRC)? != keyed_offset_crc(seal, head_offset, bytes.get(..S_CRC)?) {
    return None;
  }
  Some((kind, declared_len))
}

fn encode_section_head(kind: SectionKind, len: u64, crc: u32, out: &mut [u8; SECTION_HEAD_BYTES]) {
  put_bytes(out, 0, &[kind.byte()]);
  put_u64(out, S_LEN, len);
  put_u32(out, S_CRC, crc);
}

fn decode_section_head(bytes: &[u8; SECTION_HEAD_BYTES]) -> Option<(SectionKind, u64, u32)> {
  if bytes.get(S_FLAGS..S_LEN)?.iter().any(|byte| *byte != 0) {
    return None;
  }
  Some((
    SectionKind::from_byte(*bytes.first()?)?,
    get_u64(bytes, S_LEN)?,
    get_u32(bytes, S_CRC)?,
  ))
}

fn encode_trailer(identity: &[u8; IDENTITY], len: u64, out: &mut [u8; TRAILER_BYTES]) {
  put_bytes(out, 0, identity);
  put_bytes(out, T_COMMIT, &COMMIT);
  put_u64(out, T_LEN, len);
  let crc = out.get(..TRAILER_COVERED).map_or(0, mantle_crc::crc32c);
  put_u32(out, TRAILER_COVERED, crc);
}

fn decode_trailer(bytes: &[u8; TRAILER_BYTES]) -> Option<([u8; IDENTITY], u64)> {
  if get_array::<{ COMMIT.len() }>(bytes, T_COMMIT)? != COMMIT {
    return None;
  }
  if get_u32(bytes, TRAILER_COVERED)? != mantle_crc::crc32c(bytes.get(..TRAILER_COVERED)?) {
    return None;
  }
  Some((get_array(bytes, 0)?, get_u64(bytes, T_LEN)?))
}

#[cfg(test)]
mod tests;
