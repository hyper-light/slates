//! The WAL's compression: `CompressionTypeRecord`, `StreamingCompress` and
//! `StreamingUncompress` of `util/compression.{h,cc}` [R util/compression.h:585-759;
//! util/compression.cc:158-266] (docs/research/24 §1.5).
//!
//! RocksDB compresses a WAL only with ZSTD, as one frame per logical record written with
//! `ZSTD_compressStream2(.., ZSTD_e_end)` and a frame checksum (`ZSTD_c_checksumFlag = 1`) at the
//! context's default level; the log writer cuts each call's output into physical records and the
//! reader decompresses them fragment by fragment [R db/log_writer.cc:139-156;
//! db/log_reader.cc:646-693]. The ZSTD is the engine's own (`codec::zstd`,
//! docs/design/engine.md §5): a frame a record at the reference's default level, with the content
//! checksum, and a decoder fed each fragment as the reader meets it.

use crate::codec::zstd::{Decoder, Encoder, Level};

use crate::error::Error;
use crate::util::coding::{get_fixed32, put_fixed32};

/// The compression types a WAL can name [R include/rocksdb/compression_type.h:18-31]:
/// `StreamingCompressionTypeSupported` accepts only these two [R util/compression.h:470-480].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    /// `kNoCompression`, 0x0.
    NoCompression,
    /// `kZSTD`, 0x7.
    Zstd,
}

impl CompressionType {
    /// The stored value.
    pub const fn to_u32(self) -> u32 {
        match self {
            Self::NoCompression => 0x0,
            Self::Zstd => 0x7,
        }
    }

    /// The type a stored value names, if a WAL may use it. RocksDB refuses the others as
    /// "WAL compression type not supported" [R util/compression.h:604-609].
    pub const fn from_u32(value: u32) -> Result<Self, Error> {
        match value {
            0x0 => Ok(Self::NoCompression),
            0x7 => Ok(Self::Zstd),
            other => Err(Error::Unsupported {
                feature: "WAL compression type",
                value: other as u64,
            }),
        }
    }
}

/// `CompressionTypeRecord::EncodeTo` [R util/compression.h:592-595]: the payload of a
/// `kSetCompressionType` record.
pub fn encode_compression_type_record(dst: &mut Vec<u8>, compression_type: CompressionType) {
    put_fixed32(dst, compression_type.to_u32());
}

/// `CompressionTypeRecord::DecodeFrom` [R util/compression.h:597-612].
pub fn decode_compression_type_record(src: &mut &[u8]) -> Result<CompressionType, Error> {
    CompressionType::from_u32(get_fixed32(src)?)
}

/// The level a WAL is compressed at: the reference's default, `ZSTD_CLEVEL_DEFAULT` 3 (zstd 1.5.7
/// lib/zstd.h:134), the level of the context RocksDB's streaming compressor creates.
const WAL_LEVEL: i32 = 3;

/// The largest window a WAL's frame may ask of the decoder: the reference decoder's default
/// bound, 2^27 bytes (`ZSTD_WINDOWLOG_LIMIT_DEFAULT`, zstd 1.5.7 lib/zstd.h:1287), which RocksDB's
/// decoder keeps.
const WAL_WINDOW_MAX: usize = 1 << 27;

/// What one [`StreamingCompress::compress`] call produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compressed {
    /// Bytes written to the output buffer.
    pub output_len: usize,
    /// The codec's lower bound on the bytes still to flush before the frame ends; 0 once it has.
    pub remaining: usize,
}

/// `ZSTDStreamingCompress` [R util/compression.h:703-730; util/compression.cc:190-231].
#[derive(Debug)]
pub struct StreamingCompress {
    encoder: Encoder,
    max_output_len: usize,
}

impl StreamingCompress {
    /// `StreamingCompress::Create(kZSTD, ..)` [R util/compression.cc:158-172]: each call's output
    /// is at most `max_output_len` bytes, each frame checksummed (`ZSTD_c_checksumFlag = 1`).
    pub fn zstd(max_output_len: usize) -> Result<Self, Error> {
        Ok(Self {
            encoder: Encoder::new(Level::new(WAL_LEVEL), true),
            max_output_len,
        })
    }

    /// `ZSTDStreamingCompress::Compress` [R util/compression.cc:190-224]: continues the frame of
    /// `input`, the same input on every call until it is out. An empty input writes nothing.
    pub fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<Compressed, Error> {
        let out = output
            .get_mut(..self.max_output_len)
            .ok_or(Error::InvalidArgument {
                what: "compression output buffer shorter than its stated length",
            })?;
        let written = self.encoder.compress(input, out)?;
        Ok(Compressed {
            output_len: written.output_len,
            remaining: written.remaining,
        })
    }

    /// `ZSTDStreamingCompress::Reset` [R util/compression.cc:226-231].
    pub fn reset(&mut self) -> Result<(), Error> {
        self.encoder.reset();
        Ok(())
    }
}

/// What one [`StreamingUncompress::uncompress`] call produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uncompressed {
    /// Bytes written to the output buffer.
    pub output_len: usize,
    /// Input bytes not yet consumed.
    pub remaining: usize,
}

/// `ZSTDStreamingUncompress` [R util/compression.h:732-757; util/compression.cc:233-266].
#[derive(Debug)]
pub struct StreamingUncompress {
    decoder: Decoder,
    max_output_len: usize,
}

impl StreamingUncompress {
    /// `StreamingUncompress::Create(kZSTD, ..)` [R util/compression.cc:174-188]: each call's
    /// output is at most `max_output_len` bytes.
    pub fn zstd(max_output_len: usize) -> Result<Self, Error> {
        Ok(Self {
            decoder: Decoder::new(WAL_WINDOW_MAX, None),
            max_output_len,
        })
    }

    /// `ZSTDStreamingUncompress::Uncompress` [R util/compression.cc:233-260]: continues the
    /// frame from `input[*pos..]`, advancing `*pos`. An empty input produces only what the frame
    /// still holds.
    pub fn uncompress(
        &mut self,
        input: &[u8],
        pos: &mut usize,
        output: &mut [u8],
    ) -> Result<Uncompressed, Error> {
        let out = output
            .get_mut(..self.max_output_len)
            .ok_or(Error::InvalidArgument {
                what: "decompression output buffer shorter than its stated length",
            })?;
        let rest = input.get(*pos..).ok_or(Error::InvalidArgument {
            what: "decompression input position past its end",
        })?;
        let progress = self.decoder.decompress(rest, out)?;
        *pos = pos.saturating_add(progress.consumed);
        Ok(Uncompressed {
            output_len: progress.produced,
            remaining: input.len().saturating_sub(*pos),
        })
    }

    /// `ZSTDStreamingUncompress::Reset` [R util/compression.cc:262-266].
    pub fn reset(&mut self) -> Result<(), Error> {
        self.decoder.reset();
        Ok(())
    }
}
