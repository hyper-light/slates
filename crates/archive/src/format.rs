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
/// optional sections it does not know.
pub const FORMAT_MINOR: u16 = 0;

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
    }
  }
}

impl std::error::Error for ArchiveError {}
