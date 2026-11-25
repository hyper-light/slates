//! The changeset log (A-767 item 2): a repository's history, an append-only file of landed changesets, each one record.
//!
//! **A record** (all integers little-endian):
//! - a header of [`HEADER_BYTES`]: magic `SLCS`, the format, the tool, a flags byte, then the repository, the
//!   changeset's sequence in it, the sequence it was judged over, the grant, the work and the time (the repository a
//!   `u128`, the rest each a `u64`), the
//!   section count (`u16`), two zero bytes, and the CRC-32C of everything before it;
//! - its sections, each a head of [`SECTION_HEAD_BYTES`] (the kind, three zero bytes, the length as a `u64`, the CRC-32C
//!   of the section's bytes) followed by the bytes;
//! - a trailer of [`TRAILER_BYTES`]: the changeset's identity, the commit magic `SLCC`, the record's whole length
//!   (`u64`), and the CRC-32C of the trailer before it. The identity is BLAKE3 of the header and, for each section in
//!   order, its kind, its length (`u64`) and its bytes: the content, not the checksums, so it is hashed as the bytes are
//!   written and nothing is read back to name a record.
//!
//! **Appending.** A record is written where the log ends, a section at a time and a section's bytes in pieces of any
//! size, so an import of any size holds one piece in memory; a section's CRC is filled into its head once its bytes are
//! written. The trailer is written last, then one flush of the file (`sync_data`: `F_FULLFSYNC` on macOS,
//! `fdatasync` on Linux, `FlushFileBuffers` on Windows), and only then does the log's end move past the record: a
//! reader of the log sees a record only once it is committed and durable. A record abandoned before its commit (its
//! writer dropped, an error) leaves bytes past the end that the next append overwrites.
//!
//! **Recovery.** The log is read from a committed offset the caller trusts (the engine's checkpoint, A-767 item 3, or
//! the file's start), record by record, each verified whole: the header's CRC, every section's CRC, the trailer's CRC
//! and length, and the identity recomputed over the record. The first record that does not verify is where the log
//! ends: a write torn by a crash, or the bytes of a record never committed. A record whose trailer reached the device
//! before a crash is whole and is kept, as any write that completed.

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

/// A committed record: where it is, its header, its identity and its length; and, as its commit returns it, how long
/// that commit's flush took. Two are equal where they name the same record, whatever its commit's flush took (a record
/// read back carries none).
#[derive(Clone, Copy, Debug)]
pub struct Located {
  /// The record's first byte in the log.
  pub offset: u64,
  /// Its header.
  pub header: Header,
  /// Its identity (BLAKE3 of the header and each section's kind, length and bytes).
  pub identity: [u8; IDENTITY],
  /// Its whole length, trailer included.
  pub len: u64,
  /// The nanoseconds its commit's flush took (`sync_data`, on the host's monotonic clock): the device's own durable
  /// write, no queue and no write before it; zero for a record read back from the log.
  pub flush_ns: u64,
}

impl PartialEq for Located {
  fn eq(&self, other: &Self) -> bool {
    (self.offset, self.header, self.identity, self.len)
      == (other.offset, other.header, other.identity, other.len)
  }
}

impl Eq for Located {}

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

/// The changeset log over the block file `F`.
#[derive(Debug)]
pub struct ChangesetLog<F: BlockFile> {
  file: F,
  /// Where the committed records end: the next append's offset.
  end: u64,
  /// The record being appended, if one is: its state lives here, not in a borrow, so a record appended a slice at a
  /// time over many requests leaves the log readable between them (the owner's sliced landing).
  open: Option<OpenRecord>,
  /// The nanoseconds its reads of committed records have taken, all told ([`ChangesetLog::read_ns`]): the disk's share
  /// of a store request's service (goal 93, main's order of 2026-11-25: a clone's store wait split by part).
  read_ns: core::cell::Cell<u64>,
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
  /// Opens the log over `file`, whose committed records run from `from` (a committed offset the caller trusts: the
  /// file's start, or where the engine's checkpoint says it applied to), calling `each` with every committed record
  /// found after it, in order, with the file to read its sections from ([`sections_in`]). The log ends at the first
  /// record that does not verify.
  pub fn recover(file: F, from: u64, mut each: impl FnMut(&F, &Located)) -> Result<Self, LogError> {
    let len = file.len()?;
    let mut end = from;
    let mut buffer = Vec::new();
    while let Some(located) = verify_at(&file, end, len, &mut buffer)? {
      each(&file, &located);
      end = located.end();
    }
    Ok(Self {
      file,
      end,
      open: None,
      read_ns: core::cell::Cell::new(0),
    })
  }

  /// Where the committed records end.
  pub fn end(&self) -> u64 {
    self.end
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
    let mut bytes = [0u8; HEADER_BYTES];
    encode_header(&header, &mut bytes);
    let start = self.end;
    self.file.write_all_at(&bytes, start)?;
    let mut identity = blake3::Hasher::new();
    identity.update(&bytes);
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
    let flushing = slates_machine::clock::monotonic_ns();
    self.file.sync_data()?;
    let flush_ns = slates_machine::clock::monotonic_ns().saturating_sub(flushing);
    self.end = past;
    Ok(Located {
      offset: record.start,
      header: record.header,
      identity,
      len,
      flush_ns,
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
    header_in(&self.file, offset, self.end)
  }

  /// Reads `buf.len()` bytes of the log from `offset` (inside a committed record's section, by its [`Section`]).
  pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<(), LogError> {
    let past = offset_after(offset, buf.len())?;
    if past > self.end {
      return Err(LogError::Corrupt("a read past the committed records"));
    }
    let began = slates_machine::clock::monotonic_ns();
    let read = self.file.read_exact_at(buf, offset);
    let took = slates_machine::clock::monotonic_ns().saturating_sub(began);
    self.read_ns.set(self.read_ns.get().saturating_add(took));
    read?;
    Ok(())
  }

  /// The nanoseconds its reads of committed records have taken so far ([`ChangesetLog::read_at`]).
  pub fn read_ns(&self) -> u64 {
    self.read_ns.get()
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
pub(crate) fn header_in<F: BlockFile>(file: &F, offset: u64, end: u64) -> Result<Header, LogError> {
  if offset_after(offset, HEADER_BYTES)? > end {
    return Err(LogError::Corrupt("a header past the committed records"));
  }
  let mut head = [0u8; HEADER_BYTES];
  file.read_exact_at(&mut head, offset)?;
  decode_header(&head).ok_or(LogError::Corrupt("a header"))
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

/// Verifies the record at `offset` in a file of `len` bytes: `None` where none verifies (the log ends there).
fn verify_at<F: BlockFile>(
  file: &F,
  offset: u64,
  len: u64,
  buffer: &mut Vec<u8>,
) -> Result<Option<Located>, LogError> {
  let fits = |at: u64, count: usize| offset_after(at, count).is_ok_and(|past| past <= len);
  if !fits(offset, HEADER_BYTES) {
    return Ok(None);
  }
  let mut head = [0u8; HEADER_BYTES];
  file.read_exact_at(&mut head, offset)?;
  let Some(header) = decode_header(&head) else {
    return Ok(None);
  };
  let mut identity = blake3::Hasher::new();
  identity.update(&head);
  let start = offset_after(offset, HEADER_BYTES)?;
  let Some(at) = sections_verified(file, (start, len), header.sections, &mut identity, buffer)?
  else {
    return Ok(None);
  };
  if !fits(at, TRAILER_BYTES) {
    return Ok(None);
  }
  let mut trailer = [0u8; TRAILER_BYTES];
  file.read_exact_at(&mut trailer, at)?;
  let Some((recorded, record_len)) = decode_trailer(&trailer) else {
    return Ok(None);
  };
  let past = offset_after(at, TRAILER_BYTES)?;
  if past.checked_sub(offset) != Some(record_len) || *identity.finalize().as_bytes() != recorded {
    return Ok(None);
  }
  Ok(Some(Located {
    offset,
    header,
    identity: recorded,
    len: record_len,
    flush_ns: 0,
  }))
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

fn encode_header(header: &Header, out: &mut [u8; HEADER_BYTES]) {
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
  let crc = out.get(..HEADER_COVERED).map_or(0, mantle_crc::crc32c);
  put_u32(out, HEADER_COVERED, crc);
}

fn decode_header(bytes: &[u8; HEADER_BYTES]) -> Option<Header> {
  if get_array::<{ MAGIC.len() }>(bytes, 0)? != MAGIC
    || get_u16(bytes, H_FORMAT)? != FORMAT
    || *bytes.get(H_FLAGS)? != 0
  {
    return None;
  }
  if get_u32(bytes, HEADER_COVERED)? != mantle_crc::crc32c(bytes.get(..HEADER_COVERED)?) {
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
