//! The archive format's constants and types (D-17; research/compression-archive-dedup.md §2.6).
//! The archive is one streamable byte sequence — header, chunk records, manifest, seek table,
//! trailer — that is also the replication and clone-from-archive container. Every value here is
//! fixed in the format (not chosen at boot): the magic, the versions, the section tags, the
//! encodings, and the BLAKE3 hash that content-addresses every chunk.

/// Format: the archive magic, `SLAR` in little-endian ASCII, the first four bytes of every
/// archive.
pub const MAGIC: u32 = u32::from_le_bytes(*b"SLAR");

/// Format: the trailer magic, `SLAT`, the last four bytes so a tail-first reader can find the
/// trailer and a truncated stream is detected.
pub const TRAILER_MAGIC: u32 = u32::from_le_bytes(*b"SLAT");

/// Format: the seek table is a zstd seekable-format skippable frame; this is its frame magic
/// (`0x184D2A5E`), so a zstd reader skips it and a slates reader recognizes it.
pub const SEEK_TABLE_MAGIC: u32 = 0x184d_2a5e;

/// Format: the format major version. A reader rejects an archive whose major it does not know.
pub const FORMAT_MAJOR: u16 = 1;

/// Format: the format minor version. A reader accepts a newer minor of a known major, ignoring
/// optional sections it does not know. Minor 1 added per-entry node metadata to the manifest
/// (`manifest::NodeMeta`), which changes a tree's Merkle identity. Minor 2 (2026-09-15) appended
/// the owner (`uid`, `gid`) to that metadata and put the root directory's own metadata at the head
/// of the manifest section, both covered by the header's manifest identity — so a clone or a
/// takeover successor rebuilds ownership, not only modes and times. As with minor 1, the manifest
/// of an older minor is not decoded: archives live in RAM within one fleet release. Minor 3
/// (2026-09-30, AUD-29-56) made the metadata's times signed, added the access and birth times, and
/// replaced the "has extended attributes" flag with the attributes themselves (each name and its value's
/// extents over the archive's chunks), all covered by the manifest identity.
/// Format: minor 3 is the version this writer emits.
pub const FORMAT_MINOR: u16 = 3;

/// Format: header flags. Bit 0 the manifest is compressed; bit 1 dictionaries are present; bit 2
/// a seek table is present.
pub mod flag {
  /// The manifest section is compressed as a whole (not set by the raw-encoding writer).
  pub const COMPRESSED_MANIFEST: u32 = 1 << 0;
  /// The dictionary section is present (not set by the raw-encoding writer).
  pub const DICTIONARIES: u32 = 1 << 1;
  /// A seek table is present.
  pub const SEEK_TABLE: u32 = 1 << 2;
  /// Format: the flags a major-1 reader understands; any other required flag is refused.
  pub const KNOWN: u32 = COMPRESSED_MANIFEST | DICTIONARIES | SEEK_TABLE;
}

/// Format: how many base pages one chunk spans at most — the chunk rule (§4.5 derived constants: sixteen
/// base pages until the p90 sealed size is measured). The content store derives its chunk size from it
/// (`slates_vfs::content::chunk_bytes`), so a reader's cap and a writer's chunks cannot drift apart.
pub const CHUNK_PAGES: u64 = 16;

/// Format: the largest base page any supported target uses — 64 KiB (Linux arm64 and ppc64 64K-page
/// kernels; x86-64 uses 4 KiB and Apple arm64 16 KiB) [B: the kernels' page-size configurations].
pub const MAX_BASE_PAGE_BYTES: u64 = 64 * 1024;

/// The most raw bytes one chunk may hold, whatever an archive's header declares (AUD-29-13): a reader
/// decodes no chunk past it, so a small stream cannot demand a large decompression.
/// Derived: [`CHUNK_PAGES`] × [`MAX_BASE_PAGE_BYTES`] = 1 MiB — the largest chunk any supported writer
/// produces.
pub const MAX_CHUNK_BYTES: u64 = CHUNK_PAGES * MAX_BASE_PAGE_BYTES;

/// How a chunk's payload is stored. Only [`Encoding::Raw`] is produced today; `Lz4` and `Zstd`
/// are the codec pass (Phase 7 task 3, owed), and the field reserves their wire values so an
/// archive written later stays readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Encoding {
  /// The payload is the raw chunk bytes (`stored_len == raw_len`).
  Raw = 0,
  /// The payload is an LZ4 block (owed).
  Lz4 = 1,
  /// The payload is a zstd frame (owed).
  Zstd = 2,
}

impl Encoding {
  /// The wire value.
  pub fn to_wire(self) -> u8 {
    self as u8
  }

  /// The encoding for a wire value, or `None` for an unknown one.
  pub fn from_wire(value: u8) -> Option<Encoding> {
    ALL_ENCODINGS.iter().copied().find(|e| e.to_wire() == value)
  }
}

/// Every encoding, so `from_wire` needs no number of its own.
const ALL_ENCODINGS: &[Encoding] = &[Encoding::Raw, Encoding::Lz4, Encoding::Zstd];

/// One chunk in the archive: its content identity and payload (§2.6 item 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
  /// The BLAKE3 of the raw chunk bytes — the content address a reader verifies before use.
  pub identity: [u8; 32],
  /// The raw (decoded) length.
  pub raw_len: u64,
  /// The stored (encoded) length (equal to `raw_len` for [`Encoding::Raw`]).
  pub stored_len: u64,
  /// How the payload is stored.
  pub encoding: Encoding,
  /// The compression level (zero for raw).
  pub level: u8,
  /// The dictionary identity a zstd frame used, or all zeros for none.
  pub dictionary: [u8; 32],
  /// The stored payload bytes.
  pub payload: Vec<u8>,
}

/// A refusal from the archive reader: every way a stream can be malformed or corrupt, each
/// distinct so a caller can tell truncation from a bad hash from an unknown version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveError {
  /// The header magic is not [`MAGIC`].
  BadMagic,
  /// The format major is not one this reader knows.
  UnsupportedMajor {
    /// The major found.
    found: u16,
  },
  /// A required flag this reader does not understand is set.
  UnknownRequiredFlag {
    /// The flag bits not in [`flag::KNOWN`].
    unknown: u32,
  },
  /// The stream ends before a field or section it declares.
  Truncated,
  /// The trailer magic is missing or wrong (the stream is not a whole archive).
  BadTrailer,
  /// The whole-archive BLAKE3 in the trailer does not match the bytes.
  ArchiveHashMismatch,
  /// A chunk's payload does not hash to its declared identity.
  ChunkIdentityMismatch {
    /// The chunk's position in the archive.
    index: u64,
  },
  /// The manifest bytes do not hash to the identity the header declares.
  ManifestHashMismatch,
  /// The seek table frame is malformed (bad magic or size).
  BadSeekTable,
  /// A declared count or length is larger than the stream can hold.
  BadLength,
  /// A chunk's encoded payload could not be decoded (a corrupt LZ4/zstd frame).
  BadPayload {
    /// The chunk's position in the archive.
    index: u64,
  },
  /// A manifest extent names a chunk the archive does not hold (a malformed archive).
  MissingChunk,
  /// The header declares a maximum chunk size past the format's cap ([`MAX_CHUNK_BYTES`]).
  ChunkMaxTooLarge {
    /// The declared maximum.
    found: u32,
  },
  /// A chunk declares more raw bytes than the archive's maximum chunk size or the format's cap.
  ChunkTooLarge {
    /// The chunk's position in the archive.
    index: u64,
  },
  /// A chunk's record is not in the canonical form: an empty chunk, a raw chunk whose declared raw
  /// length is not its payload's, or an encoded chunk no smaller than its raw bytes.
  NonCanonicalChunk {
    /// The chunk's position in the archive.
    index: u64,
  },
  /// The header's raw or stored byte totals are not the sums of its chunks'.
  TotalsMismatch,
  /// A file's extents do not tile it exactly, or its recorded size is not the length they tile.
  BadExtents,
  /// A directory entry's name is not one valid path component.
  BadName,
  /// Two entries restore to one path.
  DuplicatePath,
  /// Restoring needs more bytes than the caller admitted, refused before any allocation.
  OverBudget {
    /// The bytes the restore needs: every file's length and one chunk's decode space.
    needed: u64,
    /// The bytes the caller admitted.
    budget: u64,
  },
}

impl core::fmt::Display for ArchiveError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Self::BadMagic => f.write_str("not a slates archive (bad magic)"),
      Self::UnsupportedMajor { found } => write!(f, "unsupported archive major {found}"),
      Self::UnknownRequiredFlag { unknown } => write!(f, "unknown required flag {unknown:#x}"),
      Self::Truncated => f.write_str("archive is truncated"),
      Self::BadTrailer => f.write_str("archive trailer is missing or wrong"),
      Self::ArchiveHashMismatch => f.write_str("archive hash does not match its bytes"),
      Self::ChunkIdentityMismatch { index } => write!(f, "chunk {index} fails its identity"),
      Self::ManifestHashMismatch => f.write_str("manifest fails its identity"),
      Self::BadSeekTable => f.write_str("archive seek table is malformed"),
      Self::BadLength => f.write_str("archive declares a length past its end"),
      Self::BadPayload { index } => write!(f, "chunk {index} has an undecodable payload"),
      Self::MissingChunk => f.write_str("a manifest extent names a chunk not in the archive"),
      Self::ChunkMaxTooLarge { found } => {
        write!(
          f,
          "archive declares a {found}-byte chunk maximum, past the format's cap"
        )
      }
      Self::ChunkTooLarge { index } => write!(f, "chunk {index} is larger than the chunk maximum"),
      Self::NonCanonicalChunk { index } => write!(f, "chunk {index} is not in canonical form"),
      Self::TotalsMismatch => f.write_str("archive byte totals do not match its chunks"),
      Self::BadExtents => f.write_str("a file's extents do not tile it exactly"),
      Self::BadName => f.write_str("a directory entry's name is not one path component"),
      Self::DuplicatePath => f.write_str("two entries restore to one path"),
      Self::OverBudget { needed, budget } => {
        write!(
          f,
          "restoring needs {needed} bytes, past the {budget} admitted"
        )
      }
    }
  }
}

impl std::error::Error for ArchiveError {}
