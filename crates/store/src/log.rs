//! The changeset log (A-767 item 2): a repository's history, an append-only file of landed changesets, each one record.
//!
//! **A record** (all integers little-endian):
//! - a header of [`HEADER_BYTES`]: magic `SLCS`, the format, the tool, a flags byte, then the repository, the
//!   changeset's sequence in it, the sequence it was judged over, the grant, the work and the time (the repository a
//!   `u128`, the rest each a `u64`), the section count (`u16`), two zero bytes, and the CRC-32C of the log's
//!   32-byte seal, the record's file offset (`u64` little-endian), and the 68 header bytes before the CRC;
//! - its sections, each a head of [`SECTION_HEAD_BYTES`] (the kind, three zero bytes, the length as a `u64`, the CRC-32C
//!   of the section's bytes) followed by the bytes;
//! - a trailer of [`TRAILER_BYTES`]: the changeset's identity, the commit magic `SLCC`, the record's whole length
//!   (`u64`), and the CRC-32C of the trailer before it. The identity is keyed BLAKE3 (keyed by the log's 32-byte seal)
//!   over the record's file offset (`u64` little-endian), the header bytes, and, for each section in order, its kind,
//!   its length (`u64`) and its bytes: bound to both position and store secret while keeping the exact `FORMAT = 1`
//!   on-disk byte layout, hashed as bytes are written so nothing is read back to name a record.
//!
//! **Appending.** A record is written where the log ends, a section at a time and a section's bytes in pieces of any
//! size, so an import of any size holds one piece in memory; a section's CRC is filled into its head once its bytes are
//! written. The trailer is written last, then one flush of the file (`sync_data`: `F_FULLFSYNC` on macOS,
//! `fdatasync` on Linux, `FlushFileBuffers` on Windows), and only then does the log's end move past the record: a
//! reader of the log sees a record only once it is committed and durable. A record abandoned before its commit (its
//! writer dropped, an error) leaves bytes past the end that the next append overwrites.
//!
//! **Recovery.** The log is read from a committed offset the caller trusts (the engine's checkpoint, A-767 item 3, or
//! the file's start), record by record, each verified whole against `(offset, seal)`: the header's CRC, every section's
//! CRC, the trailer's CRC and length, and the keyed BLAKE3 identity recomputed over the record. A record whose header,
//! section heads, and commit trailer all verify at `cursor` (`Verify::Damaged`) but whose payload CRC or identity fails
//! has an authenticated boundary `[cursor, next)`: it is never indexed and recovery skips directly to `next`. An
//! authenticated `(offset, seal)` header whose next section head was never written (`[0u8; SECTION_HEAD_BYTES]`), with
//! no valid commit trailer (for the last section) or next section head (for earlier sections) immediately following it
//! and with all prior sections' CRCs intact, is an interrupted uncommitted write at EOF (`Verify::UnwrittenTail`):
//! recovery stops cleanly in O(1) reads without scanning the uncommitted tail payload and without incrementing
//! `skipped`. Whenever header decoding, section-head framing, or commit-trailer verification fails at `cursor` (including
//! a single-bit flip zeroing a 0-byte section head of a mid-file committed record, or flipping a prior section's length
//! into zero payload bytes), recovery never trusts an unverified `section.len` to stop early or jump past siblings; it
//! scans linearly from `cursor + 1` in [`VERIFY_PIECE`] chunks (overlapping by [`HEADER_OVERLAP`]) with a linear
//! section-head read budget and optional `expected_grant` filter. False `SLCS` occurrences inside payloads or torn EOF
//! tails fail `(offset, seal)` header CRC verification immediately in memory without touching disk or section heads, so
//! mid-file faults (including every single-bit media fault in a header, section descriptor, payload, or trailer) recover
//! healthy trailing siblings while uncommitted torn EOF tails stop cleanly at the last genuine committed record. A record
//! whose trailer reached the device before a crash is whole and is kept, as any write that completed.

use std::fmt;

use hyper_block::DiskError;
use hyper_block::block::BlockFile;

/// Format: the record header's magic.
const MAGIC: [u8; 4] = *b"SLCS";
/// Format: the trailer's commit magic.
const COMMIT: [u8; 4] = *b"SLCC";
/// Format: this layout's number.
const FORMAT: u16 = 1;
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

/// Format: a section head's zero bytes, after its kind byte.
const S_ZERO: usize = 1;
/// Format: a section head's zero bytes' count (the head's length field is `u64`-aligned).
const S_ZERO_LEN: usize = U32 - 1;
/// Format: a section head's length, after its zero bytes.
const S_LEN: usize = S_ZERO + S_ZERO_LEN;
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
  /// Its identity (BLAKE3 of the header and each section's kind, length and bytes).
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

/// One section of a committed record: its kind and where its bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
  /// The kind.
  pub kind: SectionKind,
  /// Its first byte in the log.
  pub offset: u64,
  /// Its length.
  pub len: u64,
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
}

impl fmt::Display for LogError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Disk(error) => write!(f, "the changeset log's file: {error}"),
      Self::OutOfOrder(what) => write!(f, "a changeset appended out of order: {what}"),
      Self::TooLarge => f.write_str("a changeset past what the log can hold"),
      Self::Corrupt(what) => write!(f, "a changeset that does not verify: {what}"),
    }
  }
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

  /// Begins the open record's next section, `len` bytes of `kind`; its bytes follow through [`ChangesetLog::open_piece`].
  pub fn open_section(&mut self, kind: SectionKind, len: u64) -> Result<(), LogError> {
    self.close_section()?;
    let record = self
      .open
      .as_mut()
      .ok_or(LogError::OutOfOrder("a section with no record open"))?;
    if record.written >= record.header.sections {
      return Err(LogError::OutOfOrder("more sections than the header names"));
    }
    let head = record.at;
    record.at = offset_after(head, SECTION_HEAD_BYTES)?;
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
    let Some(record) = self.open.as_mut() else {
      return Ok(());
    };
    let Some(section) = record.section.take() else {
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

/// The sections of the committed record `located` in `file`, in order: each head read for its kind and length.
pub fn sections_in<F: BlockFile>(file: &F, located: &Located) -> Result<Vec<Section>, LogError> {
  let mut sections = Vec::new();
  sections
    .try_reserve_exact(usize::from(located.header.sections))
    .map_err(|_| LogError::TooLarge)?;
  let mut at = offset_after(located.offset, HEADER_BYTES)?;
  for _ in 0..located.header.sections {
    let mut head = [0u8; SECTION_HEAD_BYTES];
    file.read_exact_at(&mut head, at)?;
    let (kind, len, _) = decode_section_head(&head).ok_or(LogError::Corrupt("a section's head"))?;
    let offset = offset_after(at, SECTION_HEAD_BYTES)?;
    sections.push(Section { kind, offset, len });
    at = offset.checked_add(len).ok_or(LogError::TooLarge)?;
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
  /// An all-zero section head after `prior_sections` closed sections with no committed
  /// continuation immediately following it (an interrupted uncommitted write at EOF).
  UnwrittenTail { prior_sections: u16 },
  /// A section head failed to decode or a section ran past `len`.
  Broken,
  /// The resync section-head read budget ran out before all sections were framed.
  BudgetExhausted,
}

/// Structural framing check of a record at `offset` before hashing any section payload.
enum Frame {
  /// Valid `(offset, seal)` header, valid section heads fitting in the file, and a valid `SLCC`
  /// commit trailer whose `record_len` matches `past - offset`.
  Committed(Framed),
  /// Valid `(offset, seal)` header followed by an unwritten all-zero section head after
  /// `prior_sections` framed sections and no committed continuation immediately after it.
  UnwrittenTail { prior_sections: u16 },
  /// Header, section heads, or commit trailer failed verification or ran past `len`.
  Broken,
  /// The resync section-head read budget ran out while framing this candidate.
  BudgetExhausted,
}

/// Outcome of verifying a record at `offset`.
enum Verify {
  /// A verified committed record.
  Valid(Located),
  /// A structurally committed record whose header, section heads, and `SLCC` trailer all verified
  /// at `offset`, but whose payload CRC or keyed BLAKE3 identity failed: never indexed, skip
  /// directly to `next`.
  Damaged { next: u64 },
  /// Valid `(offset, seal)` header whose next section head was never closed and has no committed
  /// continuation after it: stop cleanly without scanning the uncommitted tail payload.
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

/// Peeks `TRAILER_BYTES` at `bytes_at` after an all-zero final section head: a decodable `SLCC`
/// trailer indicates a single-bit-zeroed 0-byte section head (`Broken`), otherwise `UnwrittenTail`.
fn peek_trailer<F: BlockFile>(
  file: &F,
  (bytes_at, len): (u64, u64),
  prior_sections: u16,
  context: &mut FrameContext<'_>,
) -> Result<SectionsFrame, LogError> {
  if fits_after(bytes_at, TRAILER_BYTES, len)?.is_none() {
    return Ok(SectionsFrame::UnwrittenTail { prior_sections });
  }
  if !spend_read(context) {
    return Ok(SectionsFrame::BudgetExhausted);
  }
  let mut trailer = [0u8; TRAILER_BYTES];
  file.read_exact_at(&mut trailer, bytes_at)?;
  Ok(if decode_trailer(&trailer).is_some() {
    SectionsFrame::Broken
  } else {
    SectionsFrame::UnwrittenTail { prior_sections }
  })
}

/// Peeks `SECTION_HEAD_BYTES` at `bytes_at` after an all-zero non-final section head: a decodable
/// next section head indicates a single-bit-zeroed 0-byte section head (`Broken`), otherwise
/// `UnwrittenTail`.
fn peek_next_section_head<F: BlockFile>(
  file: &F,
  (bytes_at, len): (u64, u64),
  prior_sections: u16,
  context: &mut FrameContext<'_>,
) -> Result<SectionsFrame, LogError> {
  if fits_after(bytes_at, SECTION_HEAD_BYTES, len)?.is_none() {
    return Ok(SectionsFrame::UnwrittenTail { prior_sections });
  }
  if !spend_read(context) {
    return Ok(SectionsFrame::BudgetExhausted);
  }
  let mut next_head = [0u8; SECTION_HEAD_BYTES];
  file.read_exact_at(&mut next_head, bytes_at)?;
  Ok(if decode_section_head(&next_head).is_some() {
    SectionsFrame::Broken
  } else {
    SectionsFrame::UnwrittenTail { prior_sections }
  })
}

/// Distinguishes an ordinary unwritten section head (`UnwrittenTail`) from a single-bit-zeroed
/// 0-byte section head of a mid-file committed record (`Broken`) with one constant-size peek.
fn classify_zero_section_head<F: BlockFile>(
  file: &F,
  (bytes_at, len): (u64, u64),
  (prior_sections, remaining_sections): (u16, u16),
  context: &mut FrameContext<'_>,
) -> Result<SectionsFrame, LogError> {
  if remaining_sections == 0 {
    peek_trailer(file, (bytes_at, len), prior_sections, context)
  } else {
    peek_next_section_head(file, (bytes_at, len), prior_sections, context)
  }
}

/// Frames one section head at `at` within a file of `len` bytes under `context`.
fn step_section<F: BlockFile>(
  file: &F,
  (at, len): (u64, u64),
  (prior_sections, remaining_sections): (u16, u16),
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
  if section == [0u8; SECTION_HEAD_BYTES] {
    return classify_zero_section_head(
      file,
      (bytes_at, len),
      (prior_sections, remaining_sections),
      context,
    );
  }
  let Some((_, section_len, _)) = decode_section_head(&section) else {
    return Ok(SectionsFrame::Broken);
  };
  match bytes_at
    .checked_add(section_len)
    .filter(|past| *past <= len)
  {
    Some(past) => Ok(SectionsFrame::Complete(past)),
    None => Ok(SectionsFrame::Broken),
  }
}

/// Walks `sections` section heads from `at` without reading payload bytes, charging each head read
/// to `context` when scanning.
fn frame_sections_budgeted<F: BlockFile>(
  file: &F,
  (mut at, len): (u64, u64),
  sections: u16,
  context: &mut FrameContext<'_>,
) -> Result<SectionsFrame, LogError> {
  for prior_sections in 0..sections {
    let remaining_sections = sections.saturating_sub(prior_sections.saturating_add(1));
    match step_section(
      file,
      (at, len),
      (prior_sections, remaining_sections),
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
  header: Header,
  head_bytes: [u8; HEADER_BYTES],
  context: &mut FrameContext<'_>,
) -> Result<Frame, LogError> {
  let Some(start) = fits_after(offset, HEADER_BYTES, len)? else {
    return Ok(Frame::Broken);
  };
  let at = match frame_sections_budgeted(file, (start, len), header.sections, context)? {
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

/// Confirms an `UnwrittenTail` found after `prior_sections` sections: when `prior_sections > 0`,
/// verifies each prior section's CRC so a single-bit flip in an earlier section's length that
/// jumped into zero payload bytes falls back to linear resync (`Verify::Broken`).
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
  match frame_decoded_header(file, (offset, len), header, head_bytes, &mut context)? {
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
      FrameContext::Resync(_) => Ok(Verify::UnwrittenTail),
    },
    Frame::Committed(framed) => verify_committed(file, (offset, len), seal, &framed, buffer),
  }
}

/// Reads and verifies the record at `offset` in a file of `len` bytes against `(offset, seal)`.
fn verify_at<F: BlockFile>(
  file: &F,
  (offset, len): (u64, u64),
  seal: &[u8; SEAL_BYTES],
  buffer: &mut Vec<u8>,
) -> Result<Verify, LogError> {
  if fits_after(offset, HEADER_BYTES, len)?.is_none() {
    return Ok(Verify::Broken);
  }
  let mut head_bytes = [0u8; HEADER_BYTES];
  file.read_exact_at(&mut head_bytes, offset)?;
  let Some(header) = decode_header(&head_bytes, offset, seal) else {
    return Ok(Verify::Broken);
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
    Verify::Damaged { .. } | Verify::UnwrittenTail | Verify::Broken => Ok(None),
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
      progress.cursor = next;
      Ok(true)
    }
    Verify::UnwrittenTail | Verify::BudgetExhausted => Ok(false),
    Verify::Broken => apply_resync_scan(
      file,
      (cursor.saturating_add(1), len),
      progress,
      buffers,
      each,
    ),
  }
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

/// Computes the header CRC-32C bound to `(seal, offset)` over `covered` (`head[..HEADER_COVERED]`).
fn header_crc(seal: &[u8; SEAL_BYTES], offset: u64, covered: &[u8]) -> u32 {
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
    .map_or(0, |covered| header_crc(seal, offset, covered));
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
  if get_u32(bytes, HEADER_COVERED)? != header_crc(seal, offset, bytes.get(..HEADER_COVERED)?) {
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

fn encode_section_head(kind: SectionKind, len: u64, crc: u32, out: &mut [u8; SECTION_HEAD_BYTES]) {
  put_bytes(out, 0, &[kind.byte()]);
  put_u64(out, S_LEN, len);
  put_u32(out, S_CRC, crc);
}

fn decode_section_head(bytes: &[u8; SECTION_HEAD_BYTES]) -> Option<(SectionKind, u64, u32)> {
  if bytes.get(S_ZERO..S_LEN)?.iter().any(|byte| *byte != 0) {
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
