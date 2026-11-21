//! Zstandard, the engine's own (docs/design/engine.md §5): the format of RFC 8878, which RocksDB
//! compresses WALs and table blocks with [R util/compression.h]. The reference C library is the
//! differential oracle in the tests, never a dependency.
//!
//! The decoder reads every frame the format defines, dictionaries raw and formatted included,
//! fed in pieces of any size, holding at most one block of input and the frame's window of
//! output. A frame that asks for a window above the decoder's bound is refused.

mod bits;
mod decoder;
mod encoder;
mod fse;
mod huffman;
mod sequences;

pub use decoder::{Decoder, Dictionary, Progress, SLACK};
pub use encoder::{Compressor, Encoder, Level, Written, compress};

use crate::error::{Error, Malformed};

/// How a Zstandard input fails to decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Corrupt {
    /// Not a Zstandard or skippable frame's magic number (§3.1.1, §3.1.2).
    Magic,
    /// A frame header with its reserved bit set, or naming a dictionary the decoder lacks
    /// (§3.1.1.1).
    Header,
    /// A reserved block type, or a block above the frame's maximum (§3.1.1.2).
    Block,
    /// A literals section that does not parse (§3.1.1.3.1).
    Literals,
    /// A Huffman tree or stream that does not decode (§4.2).
    Huffman,
    /// An FSE table description that does not parse (§4.1.1).
    Distribution,
    /// A sequences section that does not decode (§3.1.1.3.2).
    Sequences,
    /// A sequence that reads more literals than the block has, or matches before the data
    /// (§3.1.1.4).
    Execution,
    /// A bitstream without its end marker, or not consumed exactly.
    Bitstream,
    /// A frame whose decoded size is not its header's Frame_Content_Size (§3.1.1.1.4).
    ContentSize,
    /// A frame whose content checksum does not match (§3.1.1).
    Checksum,
    /// A dictionary that does not parse (§5).
    Dictionary,
    /// Input that ends inside a structure.
    Truncated,
}

impl Corrupt {
    fn what(self) -> &'static str {
        match self {
            Self::Magic => "ZSTD frame magic number",
            Self::Header => "ZSTD frame header",
            Self::Block => "ZSTD block",
            Self::Literals => "ZSTD literals section",
            Self::Huffman => "ZSTD Huffman literals",
            Self::Distribution => "ZSTD FSE table description",
            Self::Sequences => "ZSTD sequences section",
            Self::Execution => "ZSTD sequence execution",
            Self::Bitstream => "ZSTD bitstream",
            Self::ContentSize => "ZSTD frame content size",
            Self::Checksum => "ZSTD content checksum",
            Self::Dictionary => "ZSTD dictionary",
            Self::Truncated => "ZSTD input",
        }
    }
}

impl From<Corrupt> for Error {
    fn from(corrupt: Corrupt) -> Self {
        let why = match corrupt {
            Corrupt::Checksum => Malformed::ChecksumMismatch,
            Corrupt::Truncated => Malformed::Truncated,
            Corrupt::ContentSize => Malformed::CountMismatch,
            _ => Malformed::Undecodable,
        };
        Error::Corruption {
            what: corrupt.what(),
            why,
        }
    }
}
