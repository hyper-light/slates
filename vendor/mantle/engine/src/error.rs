//! The engine's typed errors, which replace RocksDB's `Status` codes (docs/research/24 §2.3).
//!
//! RocksDB reports a bad decode as `Status::Corruption`, a `false` return or a null pointer,
//! and a refused format as `Status::NotSupported`; its decoders also trust some inputs and
//! read past them (docs/research/24 §5 R6). Here each of those is a variant a caller can match.

use std::fmt;

/// An engine failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Stored bytes that do not decode: RocksDB's `Status::Corruption`. It feeds repair
    /// (CLAUDE.md §6).
    #[error("corruption in {what}: {why}")]
    Corruption { what: &'static str, why: Malformed },
    /// A value the format defines, or does not, that this engine does not read or write:
    /// RocksDB's `Status::NotSupported` (docs/research/24 §1.18).
    #[error("unsupported {feature}: {value}")]
    Unsupported { feature: &'static str, value: u64 },
    /// A caller's argument the format cannot represent: RocksDB's `Status::InvalidArgument`, or
    /// a C++ precondition RocksDB leaves unchecked.
    #[error("invalid argument: {what}")]
    InvalidArgument { what: &'static str },
    /// A bounded resource would pass its bound (CLAUDE.md §2): RocksDB's `Status::MemoryLimit`,
    /// or a bound the port adds where RocksDB has none. Nothing was changed.
    #[error("{what} would exceed its bound of {limit}")]
    LimitExceeded { what: &'static str, limit: u64 },
    /// A file operation failed: RocksDB's `IOStatus::IOError`. After a failed write or flush
    /// the durability of what was written is unknown (hyper-block's `BlockFile::sync_data`), so
    /// the file that returned it takes no more writes (docs/research/24 §2.3).
    #[error("I/O error in {op}: {detail}")]
    Io { op: &'static str, detail: String },
    /// An entry with the same internal key (user key, sequence and type) is already in the
    /// memtable: RocksDB's `Status::TryAgain("key+seq exists")`, which the write path handles
    /// and never returns to a caller (docs/research/24 §2.3, §4.1).
    #[error("an entry with this key and sequence number exists")]
    Duplicate,
    /// The shard that owns a range is gone (stopped, or its task ended on an error), so the
    /// request was not applied, or its answer cannot arrive.
    #[error("{what} is gone")]
    Gone { what: &'static str },
}

/// How stored bytes fail to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// The input ends before the value does.
    Truncated,
    /// A varint whose last permitted byte still has its continuation bit set.
    VarintTooLong,
    /// A varint whose last byte carries bits above its type's width, which RocksDB drops and
    /// never writes (docs/research/24 §1.1).
    VarintOverflow,
    /// A prefix varint whose first byte names a length its type does not have.
    PrefixVarintFirstByte,
    /// A tag, type or kind byte the format does not define: a WriteBatch record tag, a value
    /// type, a wide-column type, a blob-index type.
    UnknownTag(u8),
    /// A format version the format does not define.
    UnknownVersion(u64),
    /// A count or size stored in the bytes that disagrees with what they hold.
    CountMismatch,
    /// Bytes after the end of a value that must end where its input does.
    TrailingBytes,
    /// Entries a format stores in strictly ascending order that are not.
    OutOfOrder,
    /// A length larger than the format allows.
    TooLarge,
    /// A stored checksum that does not match the bytes it covers.
    ChecksumMismatch,
    /// A reference to data the reader has no way to fetch: a blob-backed value read where no
    /// blob reader is given (RocksDB's null `BlobFetcher`).
    Unresolvable,
    /// Compressed bytes that do not decode under their format; the structure is named beside it
    /// (`codec::zstd`).
    Undecodable,
    /// A magic number that is not the one the structure must carry: a table's, or a footer's
    /// extended magic.
    BadMagic,
    /// A field holding a value its format forbids, such as a footer's base context checksum
    /// that disables the context it must enable.
    Forbidden,
    /// Two stored values that must agree arithmetically and cannot: an offset that would lie
    /// before the file's start, or a delta-encoded size below zero.
    OutOfRange,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "input ends inside a value",
            Self::VarintTooLong => "varint longer than its type allows",
            Self::VarintOverflow => "varint carries bits above its type's width",
            Self::PrefixVarintFirstByte => "prefix varint's first byte names no valid length",
            Self::UnknownTag(tag) => return write!(f, "unknown tag {tag:#04x}"),
            Self::UnknownVersion(v) => return write!(f, "unknown version {v}"),
            Self::CountMismatch => "stored count disagrees with the entries present",
            Self::TrailingBytes => "bytes follow the end of the value",
            Self::OutOfOrder => "entries not in strictly ascending order",
            Self::TooLarge => "length larger than the format allows",
            Self::ChecksumMismatch => "checksum mismatch",
            Self::Unresolvable => "refers to data this reader cannot fetch",
            Self::Undecodable => "compressed bytes that do not decode under their format",
            Self::BadMagic => "magic number not the one expected",
            Self::Forbidden => "field holds a value the format forbids",
            Self::OutOfRange => "stored values that cannot both hold",
        })
    }
}

impl Error {
    pub(crate) const fn truncated(what: &'static str) -> Self {
        Self::Corruption {
            what,
            why: Malformed::Truncated,
        }
    }

    pub(crate) const fn corruption(what: &'static str, why: Malformed) -> Self {
        Self::Corruption { what, why }
    }
}
