//! The changeset log (A-767 item 2): a repository's history, an append-only file of landed changesets, each one record.
//!
//! **A record** (all integers little-endian):
//! - a header of [`HEADER_BYTES`]: magic `SLCS`, the format, the tool, a flags byte, then the repository, the
//!   changeset's sequence in it, the sequence it was judged over, the grant, the work and the time (each a `u64`), the
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
/// Format: the header's bytes: magic 4, format 2, tool 1, flags 1, six `u64`s 48, section count 2, zero 2, CRC 4.
pub const HEADER_BYTES: usize = 64;
/// Format: a section head's bytes: kind 1, zero 3, length 8, CRC 4.
pub const SECTION_HEAD_BYTES: usize = 16;
/// Format: the trailer's bytes: identity 32, commit magic 4, record length 8, CRC 4.
pub const TRAILER_BYTES: usize = 48;
/// Format: the bytes of a header that its CRC covers.
const HEADER_COVERED: usize = HEADER_BYTES - 4;
/// Format: the bytes of a trailer that its CRC covers.
const TRAILER_COVERED: usize = TRAILER_BYTES - 4;
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
  fn byte(self) -> u8 {
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
  /// The repository.
  pub repository: u64,
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
  pub identity: [u8; 32],
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

/// The changeset log over the block file `F`.
#[derive(Debug)]
pub struct ChangesetLog<F: BlockFile> {
  file: F,
  /// Where the committed records end: the next append's offset.
  end: u64,
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
    Ok(Self { file, end })
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

  /// Begins a record with `header` where the log ends.
  pub fn begin(&mut self, header: Header) -> Result<Appending<'_, F>, LogError> {
    let mut bytes = [0u8; HEADER_BYTES];
    encode_header(&header, &mut bytes);
    let start = self.end;
    self.file.write_all_at(&bytes, start)?;
    let mut identity = blake3::Hasher::new();
    identity.update(&bytes);
    Ok(Appending {
      log: self,
      header,
      start,
      at: offset_after(start, HEADER_BYTES)?,
      identity,
      written: 0,
      open: None,
    })
  }

  /// The sections of the committed record `located`, in order.
  pub fn sections(&self, located: &Located) -> Result<Vec<Section>, LogError> {
    sections_in(&self.file, located)
  }

  /// The header of the committed record at `offset` (a row's offset): read and its CRC checked.
  pub fn header_at(&self, offset: u64) -> Result<Header, LogError> {
    if offset_after(offset, HEADER_BYTES)? > self.end {
      return Err(LogError::Corrupt("a header past the committed records"));
    }
    let mut head = [0u8; HEADER_BYTES];
    self.file.read_exact_at(&mut head, offset)?;
    decode_header(&head).ok_or(LogError::Corrupt("a header"))
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

/// A record being appended: its sections written in order, its commit the last step ([`Appending::commit`]). Dropped
/// before its commit, it leaves the log's end where it was.
#[derive(Debug)]
pub struct Appending<'a, F: BlockFile> {
  log: &'a mut ChangesetLog<F>,
  header: Header,
  start: u64,
  /// Where the next byte goes.
  at: u64,
  identity: blake3::Hasher,
  /// Sections begun.
  written: u16,
  /// The open section: its head's offset, its kind and length, its bytes written so far, and their CRC so far.
  open: Option<OpenSection>,
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
    self.close_section()?;
    if self.written >= self.header.sections {
      return Err(LogError::OutOfOrder("more sections than the header names"));
    }
    let head = self.at;
    self.at = offset_after(head, SECTION_HEAD_BYTES)?;
    self.written = self.written.saturating_add(1);
    self.identity.update(&section_label(kind, len));
    self.open = Some(OpenSection {
      head,
      kind,
      len,
      done: 0,
      crc: 0,
    });
    Ok(())
  }

  /// Writes the next `bytes` of the open section.
  pub fn piece(&mut self, bytes: &[u8]) -> Result<(), LogError> {
    let Some(mut open) = self.open else {
      return Err(LogError::OutOfOrder("bytes with no section begun"));
    };
    let count = u64::try_from(bytes.len()).map_err(|_| LogError::TooLarge)?;
    let done = open.done.checked_add(count).ok_or(LogError::TooLarge)?;
    if done > open.len {
      return Err(LogError::OutOfOrder("a section's bytes past its length"));
    }
    self.log.file.write_all_at(bytes, self.at)?;
    self.at = self.at.checked_add(count).ok_or(LogError::TooLarge)?;
    self.identity.update(bytes);
    open.crc = mantle_crc::crc32c_extend(open.crc, bytes);
    open.done = done;
    self.open = Some(open);
    Ok(())
  }

  /// Writes the open section's head now that its bytes are whole and their CRC known.
  fn close_section(&mut self) -> Result<(), LogError> {
    let Some(open) = self.open.take() else {
      return Ok(());
    };
    if open.done != open.len {
      return Err(LogError::OutOfOrder(
        "a section closed before its bytes were whole",
      ));
    }
    let mut head = [0u8; SECTION_HEAD_BYTES];
    encode_section_head(open.kind, open.len, open.crc, &mut head);
    self.log.file.write_all_at(&head, open.head)?;
    Ok(())
  }

  /// Commits the record: the trailer written, the log flushed, and only then the log's end moved past it. Refused, with
  /// nothing committed, when a section is not whole or fewer sections were written than the header names.
  pub fn commit(mut self) -> Result<Located, LogError> {
    self.close_section()?;
    if self.written != self.header.sections {
      return Err(LogError::OutOfOrder("fewer sections than the header names"));
    }
    let identity: [u8; 32] = *self.identity.finalize().as_bytes();
    let past = offset_after(self.at, TRAILER_BYTES)?;
    let len = past.checked_sub(self.start).ok_or(LogError::TooLarge)?;
    let mut trailer = [0u8; TRAILER_BYTES];
    encode_trailer(&identity, len, &mut trailer);
    self.log.file.write_all_at(&trailer, self.at)?;
    self.log.file.sync_data()?;
    self.log.end = past;
    Ok(Located {
      offset: self.start,
      header: self.header,
      identity,
      len,
    })
  }
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
) -> Result<[u8; 32], LogError> {
  let mut hasher = blake3::Hasher::new();
  hash_range(file, &mut hasher, offset, len, buffer)?;
  Ok(*hasher.finalize().as_bytes())
}

/// Format: what a section adds to its record's identity before its bytes: its kind and its length.
fn section_label(kind: SectionKind, len: u64) -> [u8; 9] {
  let mut label = [0u8; 9];
  if let Some(byte) = label.get_mut(0) {
    *byte = kind.byte();
  }
  if let Some(slot) = label.get_mut(1..) {
    slot.copy_from_slice(&len.to_le_bytes());
  }
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
  }))
}

fn put_u16(out: &mut [u8], at: usize, value: u16) {
  if let Some(slot) = out.get_mut(at..at.saturating_add(2)) {
    slot.copy_from_slice(&value.to_le_bytes());
  }
}

fn put_u32(out: &mut [u8], at: usize, value: u32) {
  if let Some(slot) = out.get_mut(at..at.saturating_add(4)) {
    slot.copy_from_slice(&value.to_le_bytes());
  }
}

fn put_u64(out: &mut [u8], at: usize, value: u64) {
  if let Some(slot) = out.get_mut(at..at.saturating_add(8)) {
    slot.copy_from_slice(&value.to_le_bytes());
  }
}

fn get_u16(bytes: &[u8], at: usize) -> Option<u16> {
  bytes
    .get(at..at.checked_add(2)?)?
    .try_into()
    .ok()
    .map(u16::from_le_bytes)
}

fn get_u32(bytes: &[u8], at: usize) -> Option<u32> {
  bytes
    .get(at..at.checked_add(4)?)?
    .try_into()
    .ok()
    .map(u32::from_le_bytes)
}

fn get_u64(bytes: &[u8], at: usize) -> Option<u64> {
  bytes
    .get(at..at.checked_add(8)?)?
    .try_into()
    .ok()
    .map(u64::from_le_bytes)
}

fn encode_header(header: &Header, out: &mut [u8; HEADER_BYTES]) {
  if let Some(magic) = out.get_mut(0..4) {
    magic.copy_from_slice(&MAGIC);
  }
  put_u16(out, 4, FORMAT);
  if let Some(tool) = out.get_mut(6) {
    *tool = header.tool.byte();
  }
  put_u64(out, 8, header.repository);
  put_u64(out, 16, header.sequence);
  put_u64(out, 24, header.judged_over);
  put_u64(out, 32, header.grant);
  put_u64(out, 40, header.work);
  put_u64(out, 48, header.time_ns);
  put_u16(out, 56, header.sections);
  let crc = out.get(..HEADER_COVERED).map_or(0, mantle_crc::crc32c);
  put_u32(out, HEADER_COVERED, crc);
}

fn decode_header(bytes: &[u8; HEADER_BYTES]) -> Option<Header> {
  if bytes.get(0..4)? != MAGIC || get_u16(bytes, 4)? != FORMAT || *bytes.get(7)? != 0 {
    return None;
  }
  if get_u32(bytes, HEADER_COVERED)? != mantle_crc::crc32c(bytes.get(..HEADER_COVERED)?) {
    return None;
  }
  Some(Header {
    tool: Tool::from_byte(*bytes.get(6)?)?,
    repository: get_u64(bytes, 8)?,
    sequence: get_u64(bytes, 16)?,
    judged_over: get_u64(bytes, 24)?,
    grant: get_u64(bytes, 32)?,
    work: get_u64(bytes, 40)?,
    time_ns: get_u64(bytes, 48)?,
    sections: get_u16(bytes, 56)?,
  })
}

fn encode_section_head(kind: SectionKind, len: u64, crc: u32, out: &mut [u8; SECTION_HEAD_BYTES]) {
  if let Some(byte) = out.get_mut(0) {
    *byte = kind.byte();
  }
  put_u64(out, 4, len);
  put_u32(out, 12, crc);
}

fn decode_section_head(bytes: &[u8; SECTION_HEAD_BYTES]) -> Option<(SectionKind, u64, u32)> {
  if bytes.get(1..4)? != [0, 0, 0] {
    return None;
  }
  Some((
    SectionKind::from_byte(*bytes.first()?)?,
    get_u64(bytes, 4)?,
    get_u32(bytes, 12)?,
  ))
}

fn encode_trailer(identity: &[u8; 32], len: u64, out: &mut [u8; TRAILER_BYTES]) {
  if let Some(slot) = out.get_mut(0..32) {
    slot.copy_from_slice(identity);
  }
  if let Some(slot) = out.get_mut(32..36) {
    slot.copy_from_slice(&COMMIT);
  }
  put_u64(out, 36, len);
  let crc = out.get(..TRAILER_COVERED).map_or(0, mantle_crc::crc32c);
  put_u32(out, TRAILER_COVERED, crc);
}

fn decode_trailer(bytes: &[u8; TRAILER_BYTES]) -> Option<([u8; 32], u64)> {
  if bytes.get(32..36)? != COMMIT {
    return None;
  }
  if get_u32(bytes, TRAILER_COVERED)? != mantle_crc::crc32c(bytes.get(..TRAILER_COVERED)?) {
    return None;
  }
  Some((bytes.get(0..32)?.try_into().ok()?, get_u64(bytes, 36)?))
}

#[cfg(test)]
mod tests;
