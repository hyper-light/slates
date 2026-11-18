//! An archive written as a stream (A-459; §4.11 "Archive", D-17): its bytes staged a bounded piece at a time and handed
//! to the caller in order, so neither the archive nor the content of the snapshot it holds is ever held whole. The
//! discipline is the streamed checkpoint's (`slates_vfs::recover::StreamedImage` with `encode_image_into`): a first
//! pass counts what the layout needs before any byte is written, and a second pass writes in order into a stage the
//! caller empties between items, a checksum folded over every byte as it is staged and written last.
//!
//! - **The first pass** ([`Measure`]) takes each new chunk as a walk cuts it ([`ChunkSink`]) and keeps its identity and
//!   lengths, never its payload. With the walk's header fields and manifest, it lays the archive out ([`Layout`]): the
//!   header (the chunk count, the raw and stored totals, the manifest's identity), every record's offset, the
//!   manifest's, the seek table's and the whole length. All of it is known before a byte is written, so a caller admits
//!   exactly the room the archive takes.
//! - **The second pass** ([`StreamWriter`]) takes the same walk's chunks again. Each is checked against its first-pass
//!   record (the walk of a frozen snapshot is deterministic, so a chunk that differs is a typed refusal, never a
//!   corrupt archive) and staged as its record behind the header. Then the manifest, the seek table and the trailer are
//!   staged a piece a call ([`StreamWriter::fill`]). The whole-archive BLAKE3 is folded over every byte as it is
//!   staged, and written last, in the trailer, where the format keeps it.
//!
//! The bytes equal [`Archive::encode`]'s, byte for byte. That whole-buffer encoder is the oracle these tests hold the
//! stream to, so it shares none of this module's code; the header is written here again, field by field, on purpose.
//! The cost of an exact admission and an in-order hash is the walk's second run: its identities and compression are
//! computed twice. A caller that holds an archive whole anyway (the fleet's replication) keeps [`Archive::encode`].

use crate::archive::{Archive, HEADER_LEN, TRAILER_LEN};
use crate::format::{
  ArchiveError, Chunk, FORMAT_MAJOR, FORMAT_MINOR, MAGIC, SEEK_TABLE_MAGIC, TRAILER_MAGIC, flag,
};
use crate::manifest;
use crate::wire::Writer;

/// Format: a chunk record's bytes before its payload, as [`Archive::encode`] writes them: the identity, the raw and
/// stored lengths, the encoding, the level and the dictionary's identity.
pub const RECORD_HEADER_BYTES: usize = size_of::<[u8; 32]>()
  + size_of::<u64>()
  + size_of::<u64>()
  + size_of::<u8>()
  + size_of::<u8>()
  + size_of::<[u8; 32]>();

/// Format: one seek table entry: a chunk's identity and its record's offset.
const SEEK_ENTRY_BYTES: usize = size_of::<[u8; 32]>() + size_of::<u64>();

/// Format: the seek table's head before its entries: the skippable frame's magic, the frame's length, and the entry
/// count that opens the frame.
const SEEK_HEAD_BYTES: usize = size_of::<u32>() + size_of::<u32>() + size_of::<u64>();

/// Where a walk hands each new chunk as it cuts it, in the order the whole archive holds them.
pub trait ChunkSink {
  /// Takes the walk's next new chunk; a refusal ends the walk.
  fn take(&mut self, chunk: Chunk) -> Result<(), ArchiveError>;
}

/// One chunk's record as the first pass measured it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Measured {
  identity: [u8; 32],
  raw_len: u64,
  stored_len: u64,
}

/// The first pass of a streamed archive: each new chunk's identity and lengths kept, its payload dropped.
#[derive(Debug, Default)]
pub struct Measure {
  records: Vec<Measured>,
}

impl ChunkSink for Measure {
  fn take(&mut self, chunk: Chunk) -> Result<(), ArchiveError> {
    self.records.push(Measured {
      identity: chunk.identity,
      raw_len: chunk.raw_len,
      stored_len: chunk.stored_len,
    });
    Ok(())
  }
}

/// An archive laid out by its first pass: the header, each record's offset, the manifest, the seek table's place and
/// the whole length, all known before a byte is written.
#[derive(Debug)]
pub struct Layout {
  header: Vec<u8>,
  records: Vec<Measured>,
  offsets: Vec<u64>,
  manifest: Vec<u8>,
  manifest_identity: [u8; 32],
  manifest_offset: u64,
  seek_offset: u64,
  len: u64,
}

impl Layout {
  /// Lays out the archive `shell` describes (its header fields, its root's metadata and its manifest) over the chunks
  /// the first pass measured, as [`Archive::encode`] would write it. The shell's own chunks must be none, since every
  /// chunk went to `measure`; a shell that holds any is refused `TotalsMismatch`, as is a layout past `u64`
  /// (`BadLength`).
  pub fn new(shell: &Archive, measure: Measure) -> Result<Layout, ArchiveError> {
    if !shell.chunks.is_empty() {
      return Err(ArchiveError::TotalsMismatch);
    }
    let records = measure.records;
    let mut manifest = manifest::encode_root_meta(&shell.root_meta);
    manifest.extend_from_slice(&shell.manifest.encode());
    let manifest_identity = shell.manifest_identity();
    let (mut raw_bytes, mut stored_bytes) = (0u64, 0u64);
    let mut offsets = Vec::with_capacity(records.len());
    let mut at = HEADER_LEN;
    let record_header = u64::try_from(RECORD_HEADER_BYTES).map_err(|_| ArchiveError::BadLength)?;
    for record in &records {
      offsets.push(at);
      raw_bytes = raw_bytes
        .checked_add(record.raw_len)
        .ok_or(ArchiveError::BadLength)?;
      stored_bytes = stored_bytes
        .checked_add(record.stored_len)
        .ok_or(ArchiveError::BadLength)?;
      at = at
        .checked_add(record_header)
        .and_then(|at| at.checked_add(record.stored_len))
        .ok_or(ArchiveError::BadLength)?;
    }
    let manifest_offset = at;
    let width = |bytes: usize| u64::try_from(bytes).map_err(|_| ArchiveError::BadLength);
    let seek_offset = manifest_offset
      .checked_add(width(size_of::<u64>())?)
      .and_then(|at| at.checked_add(u64::try_from(manifest.len()).ok()?))
      .ok_or(ArchiveError::BadLength)?;
    let entries = width(records.len())?
      .checked_mul(width(SEEK_ENTRY_BYTES)?)
      .ok_or(ArchiveError::BadLength)?;
    let len = seek_offset
      .checked_add(width(SEEK_HEAD_BYTES)?)
      .and_then(|at| at.checked_add(entries))
      .and_then(|at| at.checked_add(TRAILER_LEN))
      .ok_or(ArchiveError::BadLength)?;
    let header = header_of(
      shell,
      manifest_identity,
      width(records.len())?,
      (raw_bytes, stored_bytes),
    );
    Ok(Layout {
      header,
      records,
      offsets,
      manifest,
      manifest_identity,
      manifest_offset,
      seek_offset,
      len,
    })
  }

  /// The archive's whole length in bytes.
  pub fn len(&self) -> u64 {
    self.len
  }

  /// Whether the archive is empty: never, since its header and trailer always stand.
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// The manifest's identity the header names: the tree's (D-17).
  pub fn manifest_identity(&self) -> [u8; 32] {
    self.manifest_identity
  }
}

/// The header [`Archive::encode`] writes for `shell`, its chunks counted by the first pass: written here field by field
/// again, so the whole-buffer encoder stays an independent oracle.
fn header_of(
  shell: &Archive,
  manifest_identity: [u8; 32],
  chunk_count: u64,
  (raw_bytes, stored_bytes): (u64, u64),
) -> Vec<u8> {
  let mut writer = Writer::new();
  writer.u32(MAGIC);
  writer.u16(FORMAT_MAJOR);
  writer.u16(FORMAT_MINOR);
  writer.u32(u32::try_from(HEADER_LEN).unwrap_or(u32::MAX));
  writer.u32(flag::SEEK_TABLE);
  writer.u32(shell.base_page_size);
  writer.u32(shell.chunk_min);
  writer.u32(shell.chunk_max);
  writer.hash(&manifest_identity);
  writer.u64(chunk_count);
  writer.u64(raw_bytes);
  writer.u64(stored_bytes);
  writer.u64(shell.created_unix);
  writer.u64(shell.volume_id);
  writer.u64(shell.snapshot_id);
  writer.u32(shell.name_policy_id);
  writer.u32(shell.unicode_version);
  writer.finish()
}

/// What a [`StreamWriter`] asks of its caller next ([`StreamWriter::next`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
  /// Write the staged bytes where [`StreamWriter::pending`] says, then [`StreamWriter::consume`] what was written.
  Write,
  /// Let the walk cut its next unit, its chunks handed to the writer ([`ChunkSink::take`]).
  Walk,
  /// The archive is written whole.
  Done,
}

/// Where a [`StreamWriter`]'s tail stands once every record is staged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tail {
  /// The manifest's length.
  ManifestLength,
  /// The manifest's bytes, from `at`.
  Manifest { at: usize },
  /// The seek table's head.
  SeekHead,
  /// The seek table's entries, from the `at`th.
  Seek { at: usize },
  /// The trailer's section index, then the hash and the magic.
  Trailer,
  /// Every byte staged.
  Done,
}

/// The second pass of a streamed archive: written in order into a stage the caller empties
/// ([`StreamWriter::pending`], [`StreamWriter::consume`]), the whole-archive BLAKE3 folded over every byte as it is
/// staged. The stage takes the header first, then each record as its chunk is taken, then the tail a piece a call
/// ([`StreamWriter::fill`]). A caller that empties the stage whenever it holds `threshold` bytes or more keeps it
/// within [`StreamWriter::bound`]; the stage's largest length is kept ([`StreamWriter::high_water`]) and every emptying
/// counted ([`StreamWriter::drained`]), so a test sees both.
pub struct StreamWriter {
  layout: Layout,
  stage: Vec<u8>,
  /// Where the stage's first byte lies in the archive.
  staged_at: u64,
  hasher: Box<blake3::Hasher>,
  /// The next record the walk must hand.
  next: usize,
  tail: Tail,
  high_water: usize,
  drained: u64,
}

impl StreamWriter {
  /// The second pass of `layout`'s archive, its header staged.
  pub fn new(mut layout: Layout) -> StreamWriter {
    let header = std::mem::take(&mut layout.header);
    let mut writer = StreamWriter {
      layout,
      stage: Vec::new(),
      staged_at: 0,
      hasher: Box::new(blake3::Hasher::new()),
      next: 0,
      tail: Tail::ManifestLength,
      high_water: 0,
      drained: 0,
    };
    writer.stage_hashed(&header);
    writer
  }

  /// The most a stage emptied whenever it holds `threshold` bytes or more ever holds, for an archive whose chunks are
  /// at most `chunk_max` bytes: `threshold` less one byte, and the largest item staged whole after it — a record of the
  /// largest chunk, or the header, which the stage takes first.
  pub fn bound(threshold: usize, chunk_max: u32) -> usize {
    let record = usize::try_from(chunk_max)
      .unwrap_or(usize::MAX)
      .saturating_add(RECORD_HEADER_BYTES);
    let header = usize::try_from(HEADER_LEN).unwrap_or(usize::MAX);
    threshold.saturating_add(record.max(header))
  }

  /// The archive's whole length in bytes.
  pub fn len(&self) -> u64 {
    self.layout.len
  }

  /// Whether the archive is empty: never, since its header and trailer always stand.
  pub fn is_empty(&self) -> bool {
    self.layout.len == 0
  }

  /// The manifest's identity the header names.
  pub fn manifest_identity(&self) -> [u8; 32] {
    self.layout.manifest_identity
  }

  /// The staged bytes, and where the first of them lies in the archive.
  pub fn pending(&self) -> (u64, &[u8]) {
    (self.staged_at, &self.stage)
  }

  /// Drops the first `written` staged bytes, which the caller has written where [`StreamWriter::pending`] said; counted
  /// in [`StreamWriter::drained`] when any were.
  pub fn consume(&mut self, written: usize) {
    let written = written.min(self.stage.len());
    if written == 0 {
      return;
    }
    self.stage.drain(..written);
    self.staged_at = self
      .staged_at
      .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
    self.drained = self.drained.saturating_add(1);
  }

  /// Whether the whole archive is staged (every record, the manifest, the seek table and the trailer): once the stage
  /// is emptied too, it is written whole.
  pub fn staged_whole(&self) -> bool {
    self.tail == Tail::Done
  }

  /// The stage's largest length so far.
  pub fn high_water(&self) -> usize {
    self.high_water
  }

  /// How many times the stage was emptied, wholly or in part.
  pub fn drained(&self) -> u64 {
    self.drained
  }

  /// Stages `bytes`, folding them into the whole-archive hash.
  fn stage_hashed(&mut self, bytes: &[u8]) {
    self.hasher.update(bytes);
    self.stage_unhashed(bytes);
  }

  /// Stages `bytes` outside the hash: the hash itself and the magic after it.
  fn stage_unhashed(&mut self, bytes: &[u8]) {
    self.stage.extend_from_slice(bytes);
    self.high_water = self.high_water.max(self.stage.len());
  }

  /// Where the next staged byte lies in the archive.
  fn position(&self) -> u64 {
    self
      .staged_at
      .saturating_add(u64::try_from(self.stage.len()).unwrap_or(u64::MAX))
  }

  /// What to do next, for a caller that empties the stage at `threshold` bytes and whose walk is `walking` (still
  /// cutting chunks): write the stage once it holds `threshold` bytes or the archive's last; else let the walk cut its
  /// next unit; else stage the tail's next piece, within the threshold's room, until there is something to write. A
  /// caller that does what it asks, and nothing else, holds the stage within [`StreamWriter::bound`]: the decision the
  /// daemon's export and the tests follow alike.
  pub fn next(&mut self, threshold: usize, walking: bool) -> Result<Next, ArchiveError> {
    loop {
      let staged = self.stage.len();
      if staged > 0 && (staged >= threshold || self.tail == Tail::Done) {
        return Ok(Next::Write);
      }
      if self.tail == Tail::Done {
        return Ok(Next::Done);
      }
      if walking {
        return Ok(Next::Walk);
      }
      self.fill(threshold.saturating_sub(staged))?;
    }
  }

  /// Stages the tail once every record is staged — the manifest's length and bytes, the seek table, the trailer — a
  /// piece a call: at most `room` of the manifest's bytes or the seek table's entries, at least one, and a fixed field
  /// whole. `true` once the whole archive is staged. Refused `TotalsMismatch` while a record the first pass measured is
  /// still to come.
  pub fn fill(&mut self, room: usize) -> Result<bool, ArchiveError> {
    if self.next != self.layout.records.len() {
      return Err(ArchiveError::TotalsMismatch);
    }
    let room = room.max(1);
    match self.tail {
      Tail::ManifestLength => {
        let len = u64::try_from(self.layout.manifest.len()).map_err(|_| ArchiveError::BadLength)?;
        if self.position() != self.layout.manifest_offset {
          return Err(ArchiveError::BadLength);
        }
        self.stage_hashed(&len.to_le_bytes());
        self.tail = Tail::Manifest { at: 0 };
      }
      Tail::Manifest { at } => {
        let end = at.saturating_add(room).min(self.layout.manifest.len());
        let piece = self
          .layout
          .manifest
          .get(at..end)
          .unwrap_or_default()
          .to_vec();
        self.stage_hashed(&piece);
        self.tail = if end >= self.layout.manifest.len() {
          Tail::SeekHead
        } else {
          Tail::Manifest { at: end }
        };
      }
      Tail::SeekHead => {
        if self.position() != self.layout.seek_offset {
          return Err(ArchiveError::BadLength);
        }
        let count =
          u64::try_from(self.layout.records.len()).map_err(|_| ArchiveError::BadLength)?;
        let frame = size_of::<u64>()
          .saturating_add(self.layout.records.len().saturating_mul(SEEK_ENTRY_BYTES));
        let mut head = Writer::new();
        head.u32(SEEK_TABLE_MAGIC);
        head.u32(u32::try_from(frame).unwrap_or(u32::MAX));
        head.u64(count);
        self.stage_hashed(head.as_slice());
        self.tail = Tail::Seek { at: 0 };
      }
      Tail::Seek { at } => {
        let entries = (room / SEEK_ENTRY_BYTES).max(1);
        let end = at.saturating_add(entries).min(self.layout.records.len());
        let mut piece = Writer::new();
        for (record, offset) in self
          .layout
          .records
          .iter()
          .zip(&self.layout.offsets)
          .skip(at)
          .take(end.saturating_sub(at))
        {
          piece.hash(&record.identity);
          piece.u64(*offset);
        }
        self.stage_hashed(piece.as_slice());
        self.tail = if end >= self.layout.records.len() {
          Tail::Trailer
        } else {
          Tail::Seek { at: end }
        };
      }
      Tail::Trailer => {
        let mut index = Writer::new();
        index.u64(0);
        index.u64(HEADER_LEN);
        index.u64(self.layout.manifest_offset);
        index.u64(self.layout.seek_offset);
        self.stage_hashed(index.as_slice());
        let hash = *self.hasher.finalize().as_bytes();
        self.stage_unhashed(&hash);
        self.stage_unhashed(&TRAILER_MAGIC.to_le_bytes());
        if self.position() != self.layout.len {
          return Err(ArchiveError::BadLength);
        }
        self.tail = Tail::Done;
      }
      Tail::Done => {}
    }
    Ok(self.tail == Tail::Done)
  }
}

impl ChunkSink for StreamWriter {
  /// Stages `chunk`'s record, once it is checked against the first pass's record at its place: the same identity, the
  /// same lengths, at the same offset. A chunk the first pass did not measure there is refused (`ChunkIdentityMismatch`
  /// naming its place, `BadLength`, or `TotalsMismatch` past the last), so a walk that changed between the passes
  /// never stages a byte of an archive its layout does not describe.
  fn take(&mut self, chunk: Chunk) -> Result<(), ArchiveError> {
    let index = u64::try_from(self.next).unwrap_or(u64::MAX);
    let (Some(expected), Some(offset)) = (
      self.layout.records.get(self.next).copied(),
      self.layout.offsets.get(self.next).copied(),
    ) else {
      return Err(ArchiveError::TotalsMismatch);
    };
    if chunk.identity != expected.identity {
      return Err(ArchiveError::ChunkIdentityMismatch { index });
    }
    if chunk.raw_len != expected.raw_len
      || chunk.stored_len != expected.stored_len
      || u64::try_from(chunk.payload.len()).ok() != Some(chunk.stored_len)
      || self.position() != offset
    {
      return Err(ArchiveError::BadLength);
    }
    let mut record = Writer::new();
    record.hash(&chunk.identity);
    record.u64(chunk.raw_len);
    record.u64(chunk.stored_len);
    record.u8(chunk.encoding.to_wire());
    record.u8(chunk.level);
    record.hash(&chunk.dictionary);
    self.stage_hashed(record.as_slice());
    self.stage_hashed(&chunk.payload);
    self.next = self.next.saturating_add(1);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use proptest::prelude::*;

  use super::*;
  use crate::manifest::{Entry, Node, NodeMeta};

  /// Empties `writer`'s stage into `bytes` when it holds `at_least` bytes or more (and any), checking each piece comes
  /// where the bytes so far end.
  fn empty(writer: &mut StreamWriter, bytes: &mut Vec<u8>, at_least: usize) {
    let (at, staged) = writer.pending();
    if staged.len() >= at_least && !staged.is_empty() {
      assert_eq!(
        at,
        u64::try_from(bytes.len()).unwrap(),
        "pieces handed in order"
      );
      bytes.extend_from_slice(staged);
      let written = staged.len();
      writer.consume(written);
    }
  }

  /// `archive` written as a stream: its chunks measured, laid out, and taken again as [`StreamWriter::next`] asks, the
  /// stage emptied into the returned bytes whenever it asks to be written. The writer, for its counts.
  fn streamed(archive: &Archive, threshold: usize) -> (Vec<u8>, StreamWriter) {
    let mut measure = Measure::default();
    for chunk in &archive.chunks {
      measure.take(chunk.clone()).unwrap();
    }
    let shell = Archive {
      chunks: Vec::new(),
      ..archive.clone()
    };
    let mut writer = StreamWriter::new(Layout::new(&shell, measure).unwrap());
    let mut bytes = Vec::new();
    let mut chunks = archive.chunks.iter();
    let mut walking = true;
    loop {
      match writer.next(threshold, walking).unwrap() {
        Next::Write => empty(&mut writer, &mut bytes, 0),
        Next::Walk => match chunks.next() {
          Some(chunk) => writer.take(chunk.clone()).unwrap(),
          None => walking = false,
        },
        Next::Done => return (bytes, writer),
      }
    }
  }

  /// [`streamed`] with the tail filled by hand, `room` bytes a call, and the stage emptied whenever it holds
  /// `threshold`: the writer's pieces, whatever the caller's rhythm.
  fn streamed_by_hand(archive: &Archive, threshold: usize, room: usize) -> Vec<u8> {
    let mut measure = Measure::default();
    for chunk in &archive.chunks {
      measure.take(chunk.clone()).unwrap();
    }
    let shell = Archive {
      chunks: Vec::new(),
      ..archive.clone()
    };
    let mut writer = StreamWriter::new(Layout::new(&shell, measure).unwrap());
    let mut bytes = Vec::new();
    for chunk in &archive.chunks {
      empty(&mut writer, &mut bytes, threshold);
      writer.take(chunk.clone()).unwrap();
    }
    loop {
      empty(&mut writer, &mut bytes, threshold);
      if writer.fill(room).unwrap() {
        break;
      }
    }
    empty(&mut writer, &mut bytes, 0);
    bytes
  }

  /// `len` bytes no codec shrinks: a xorshift sequence from `seed`.
  fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
      .map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state.to_le_bytes().first().copied().unwrap_or_default()
      })
      .collect()
  }

  /// An archive of `contents` as chunks (compressed where LZ4 saves space, so every encoding the walk keeps appears),
  /// under a manifest of one file per chunk naming it whole.
  fn archive_of(contents: &[Vec<u8>]) -> Archive {
    let chunks: Vec<Chunk> = contents
      .iter()
      .map(|bytes| Archive::compressed_chunk(bytes.clone()))
      .collect();
    let entries = chunks
      .iter()
      .enumerate()
      .map(|(at, chunk)| Entry {
        name: format!("f{at:04}"),
        meta: NodeMeta {
          ino: u64::try_from(at).unwrap(),
          mode: 0o100_644,
          size: chunk.raw_len,
          nlink: 1,
          ..Default::default()
        },
        node: Node::File(vec![crate::manifest::Extent {
          offset: 0,
          len: chunk.raw_len,
          chunk: chunk.identity,
          chunk_offset: 0,
        }]),
      })
      .collect();
    Archive {
      base_page_size: 4096,
      chunk_min: 65_536,
      chunk_max: 65_536,
      created_unix: 1_700_000_000,
      volume_id: 7,
      snapshot_id: 42,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: NodeMeta {
        mode: 0o040_755,
        ..Default::default()
      },
      manifest: Node::Directory(entries),
      chunks,
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-459 (the oracle): do write generated archives — chunks of repeated and of varied bytes, one file per chunk —
    /// as streams emptied at a generated threshold, once as the writer asks and once with the tail filled by hand a
    /// generated room a call. Expect each stream's bytes to equal the whole-buffer encoder's byte for byte, the
    /// archive to decode, and its manifest's identity to be the one the layout named.
    #[test]
    fn a_streamed_archive_is_the_whole_buffer_encoders_byte_for_byte(
      contents in proptest::collection::vec(
        prop_oneof![
          (1usize..2048, any::<u8>()).prop_map(|(len, byte)| vec![byte; len]),
          proptest::collection::vec(any::<u8>(), 1..2048),
        ],
        0..12,
      ),
      threshold in 1usize..4096,
      room in 1usize..512,
    ) {
      let archive = archive_of(&contents);
      let (bytes, writer) = streamed(&archive, threshold);
      prop_assert_eq!(&bytes, &archive.encode());
      prop_assert_eq!(&streamed_by_hand(&archive, threshold, room), &bytes);
      prop_assert_eq!(writer.manifest_identity(), archive.manifest_identity());
      prop_assert_eq!(u64::try_from(bytes.len()).unwrap(), writer.len());
      prop_assert!(Archive::decode(&bytes).is_ok());
    }
  }

  /// Shape: the bound test's chunk: 4 KiB of bytes no codec shrinks, so each record is a chunk's length and more.
  const CHUNK: usize = 4096;

  /// A-459 (the stage's bound): do stream an archive many times larger than its threshold — sixteen chunks of 4 KiB at
  /// a threshold of two chunks, emptied whenever the stage holds the threshold. Expect the bytes to be the whole-buffer
  /// encoder's, the stage never past `StreamWriter::bound` for the archive's chunk maximum, and the stage emptied more
  /// than once (counted: a stream handed out whole at its end would be emptied once).
  #[test]
  fn a_stream_larger_than_its_threshold_never_stages_past_its_bound() {
    let contents: Vec<Vec<u8>> = (0u64..16).map(|seed| noise(seed, CHUNK)).collect();
    let archive = Archive {
      chunk_min: u32::try_from(CHUNK).unwrap(),
      chunk_max: u32::try_from(CHUNK).unwrap(),
      ..archive_of(&contents)
    };
    let threshold = 2 * CHUNK;
    let (bytes, writer) = streamed(&archive, threshold);
    assert_eq!(bytes, archive.encode());
    let bound = StreamWriter::bound(threshold, archive.chunk_max);
    assert!(
      writer.high_water() <= bound,
      "the stage reached {} of a {bound}-byte bound",
      writer.high_water()
    );
    assert!(
      bytes.len() > 4 * bound,
      "the archive ({} bytes) is larger than the bound many times",
      bytes.len()
    );
    assert!(
      writer.drained() > u64::try_from(bytes.len() / bound).unwrap(),
      "emptied {} times",
      writer.drained()
    );
  }

  /// A-459: do take, in the second pass, a chunk other than the one the first pass measured at its place, and one past
  /// the last. Expect each refused by its kind before a byte of it is staged: the identity named by its place, and the
  /// count's total.
  #[test]
  fn a_second_pass_chunk_that_differs_from_the_first_is_refused() {
    let archive = archive_of(&[b"first".to_vec(), b"second".to_vec()]);
    let mut measure = Measure::default();
    for chunk in &archive.chunks {
      measure.take(chunk.clone()).unwrap();
    }
    let shell = Archive {
      chunks: Vec::new(),
      ..archive.clone()
    };
    let mut writer = StreamWriter::new(Layout::new(&shell, measure).unwrap());
    let staged = writer.pending().1.len();
    assert_eq!(
      writer.take(Archive::raw_chunk(b"other".to_vec())),
      Err(ArchiveError::ChunkIdentityMismatch { index: 0 })
    );
    assert_eq!(writer.pending().1.len(), staged, "nothing of it staged");
    for chunk in &archive.chunks {
      writer.take(chunk.clone()).unwrap();
    }
    assert_eq!(
      writer.take(archive.chunks[0].clone()),
      Err(ArchiveError::TotalsMismatch)
    );
  }
}
