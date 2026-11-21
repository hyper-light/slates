//! Table blocks' compression: the built-in `Compressor` and `Decompressor` of
//! `util/compression.cc` [R util/compression.cc:386-404, :476-1270, :1273-1556] and the table
//! builder's choice to keep a compressed block [R table/block_based/block_based_table_builder.cc:
//! 2036-2140], over the engine's own codecs (docs/design/engine.md §5).
//!
//! A compressed block is its codec's output after the uncompressed size as a varint32
//! (`compress_format_version` 2), except Snappy's and XPRESS's, which state their own size; the
//! block's trailer names the codec. The builder keeps a compressed block only when it is at most
//! `max_compressed_bytes_per_kb` of every 1024 bytes of the block, at most 1023.
//!
//! The port reads every built-in codec RocksDB can write except BZip2 and XPRESS (Windows-only in
//! RocksDB), which it refuses as unsupported, as RocksDB does in a build without them. Where
//! RocksDB allocates whatever size a block's prefix states (up to `SIZE_MAX`), the reader here
//! refuses a size above the caller's bound before allocating anything (CLAUDE.md §2).

use crate::codec::zstd::{Compressor, Decoder, Dictionary, Level};
use crate::codec::{deflate, lz4, snappy, zstd};
use crate::error::{Error, Malformed};
use crate::util::coding::{get_varint64, put_varint32};

/// A block's compression type, the byte in its trailer [R include/rocksdb/compression_type.h:
/// 18-31].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    NoCompression,
    Snappy,
    Zlib,
    BZip2,
    Lz4,
    Lz4hc,
    Xpress,
    Zstd,
    /// `kCustomCompression80` to `kCustomCompressionFE`: a custom `CompressionManager`'s.
    Custom(u8),
}

impl CompressionType {
    /// The type `byte` names. 0x08 to 0x7F are reserved and 0xFF is `kDisableCompressionOption`,
    /// which no block carries.
    pub fn from_u8(byte: u8) -> Result<Self, Error> {
        Ok(match byte {
            0x00 => Self::NoCompression,
            0x01 => Self::Snappy,
            0x02 => Self::Zlib,
            0x03 => Self::BZip2,
            0x04 => Self::Lz4,
            0x05 => Self::Lz4hc,
            0x06 => Self::Xpress,
            0x07 => Self::Zstd,
            0x80..=0xFE => Self::Custom(byte),
            other => {
                return Err(Error::corruption(
                    "block compression type",
                    Malformed::UnknownTag(other),
                ));
            }
        })
    }

    /// The trailer byte.
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::NoCompression => 0x00,
            Self::Snappy => 0x01,
            Self::Zlib => 0x02,
            Self::BZip2 => 0x03,
            Self::Lz4 => 0x04,
            Self::Lz4hc => 0x05,
            Self::Xpress => 0x06,
            Self::Zstd => 0x07,
            Self::Custom(byte) => byte,
        }
    }
}

/// `CompressionOptions` as the built-in compressors read them [R include/rocksdb/
/// compression_type.h:176-330].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionOptions {
    /// The codec's level; `None` is `kDefaultCompressionLevel`, each codec's own default.
    pub level: Option<i32>,
    /// zlib's `windowBits`: negative for raw deflate, as RocksDB writes; its magnitude is the
    /// window's base-2 logarithm.
    pub window_bits: i32,
    /// ZSTD's frame content checksum (`ZSTD_c_checksumFlag`).
    pub checksum: bool,
    /// The most compressed bytes kept for each 1024 of a block; above 1023 counts as 1023, so a
    /// block is kept compressed only when that saves space.
    pub max_compressed_bytes_per_kb: u32,
}

impl Default for CompressionOptions {
    /// RocksDB's defaults: the codec's default level, a raw window of 2^14, no ZSTD checksum, and
    /// 7/8 of the block (`1024 * 7 / 8`).
    fn default() -> Self {
        Self {
            level: None,
            window_bits: -14,
            checksum: false,
            max_compressed_bytes_per_kb: 1024 * 7 / 8,
        }
    }
}

/// What [`compress_block`] did with a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compressed {
    /// Compressed, and small enough to keep: the bytes to write and the type to name.
    Kept(CompressionType, Vec<u8>),
    /// Compressed, but not small enough: the block is written uncompressed (RocksDB's
    /// `NUMBER_BLOCK_COMPRESSION_REJECTED`).
    Rejected,
    /// Not compressed at all: no compression asked, a block of 2^31 − 1 bytes or more, or a
    /// bound too small for the size prefix (`NUMBER_BLOCK_COMPRESSION_BYPASSED`).
    Bypassed,
}

/// The table builder's `kCompressionSizeLimit` [R block_based_table_builder.h:224]: blocks this
/// large or larger are not compressed.
const COMPRESSION_SIZE_LIMIT: usize = i32::MAX as usize;

/// The largest window a block's ZSTD frame may ask of the decoder: the reference decoder's
/// default bound, 2^27 (`ZSTD_WINDOWLOG_LIMIT_DEFAULT`, zstd 1.5.7 lib/zstd.h:1287), under which
/// RocksDB's `ZSTD_decompressDCtx` reads.
const ZSTD_WINDOW_MAX: usize = 1 << 27;

/// ZSTD's `ZSTD_CLEVEL_DEFAULT`, 3 (zstd 1.5.7 lib/zstd.h:134), which RocksDB's default level
/// means for ZSTD; its level 0 means −1 [R util/compression.h:65-75].
const ZSTD_DEFAULT_LEVEL: i32 = 3;

/// `StartCompressBlockV2` [R util/compression.cc:544-562]: the varint32 size prefix, or `None`
/// where the block bypasses compression (4 GiB or more, or no room past a prefix's 5 bytes).
fn size_prefix(raw: &[u8], bound: usize) -> Option<Vec<u8>> {
    let len = u32::try_from(raw.len()).ok()?;
    if bound <= 5 {
        return None;
    }
    // Room for every block the caller keeps: a longer one is rejected.
    let mut out = Vec::with_capacity(bound);
    put_varint32(&mut out, len);
    Some(out)
}

/// `CompressAndVerifyBlock`'s compression [R block_based_table_builder.cc:2036-2070] with the
/// built-in compressor of `kind` [R util/compression.cc:476-1270]: the block compressed after
/// `dict` (empty for none), kept only within `opts.max_compressed_bytes_per_kb`.
///
/// Snappy ignores a dictionary, as RocksDB's does. The port writes no LZ4HC (its LZ4 decoder
/// reads LZ4HC blocks, which are LZ4 blocks), and no zlib or ZSTD block with a dictionary yet:
/// those are refused as unsupported rather than written without one. Its LZ4 compresses at
/// acceleration one and its deflate with fixed codes whatever the level asks, both valid input
/// to RocksDB's decoders (docs/design/engine.md §5).
pub fn compress_block(
    kind: CompressionType,
    raw: &[u8],
    dict: &[u8],
    opts: &CompressionOptions,
) -> Result<Compressed, Error> {
    compress_block_with(kind, raw, dict, opts, &mut Workspace::default())
}

/// [`compress_block`] with `workspace` reused: a table builder that keeps one compresses each
/// block with the working areas the last one left, as RocksDB's `CompressionContext` does.
pub fn compress_block_with(
    kind: CompressionType,
    raw: &[u8],
    dict: &[u8],
    opts: &CompressionOptions,
    workspace: &mut Workspace,
) -> Result<Compressed, Error> {
    if kind == CompressionType::NoCompression || raw.len() >= COMPRESSION_SIZE_LIMIT {
        return Ok(Compressed::Bypassed);
    }
    let per_kb = u64::from(opts.max_compressed_bytes_per_kb.min(1023));
    let bound = u64::try_from(raw.len())
        .ok()
        .and_then(|n| n.checked_mul(per_kb))
        .and_then(|n| usize::try_from(n >> 10).ok())
        .unwrap_or(0);
    let unsupported = |what: &'static str| Error::Unsupported {
        feature: what,
        value: u64::from(kind.as_u8()),
    };
    let out = match kind {
        CompressionType::Snappy => snappy::compress(raw)?,
        CompressionType::Zlib | CompressionType::Lz4 | CompressionType::Zstd => {
            let Some(mut out) = size_prefix(raw, bound) else {
                return Ok(Compressed::Bypassed);
            };
            let body = match kind {
                CompressionType::Zlib => {
                    if !dict.is_empty() {
                        return Err(unsupported("writing zlib blocks with a dictionary"));
                    }
                    let window_log = opts.window_bits.unsigned_abs();
                    if opts.window_bits >= 0 {
                        return Err(unsupported(
                            "writing zlib blocks with a zlib or gzip header",
                        ));
                    }
                    deflate::compress(raw, window_log)?
                }
                CompressionType::Lz4 => lz4::compress(raw, dict)?,
                _ => {
                    if !dict.is_empty() {
                        return Err(unsupported("writing ZSTD blocks with a dictionary"));
                    }
                    let level = match opts.level {
                        None => ZSTD_DEFAULT_LEVEL,
                        Some(0) => -1,
                        Some(level) => level,
                    };
                    let compressor = match &mut workspace.zstd_compressor {
                        Some(c) => c,
                        None => workspace.zstd_compressor.insert(Compressor::new()?),
                    };
                    // The frame goes straight after the size prefix.
                    compressor.compress_into(raw, Level::new(level), opts.checksum, &mut out)?;
                    return Ok(keep(kind, out, bound));
                }
            };
            out.extend_from_slice(&body);
            out
        }
        CompressionType::NoCompression => return Ok(Compressed::Bypassed),
        CompressionType::Lz4hc
        | CompressionType::BZip2
        | CompressionType::Xpress
        | CompressionType::Custom(_) => {
            return Err(unsupported("writing blocks with compression type"));
        }
    };
    Ok(keep(kind, out, bound))
}

/// The block kept if it is within `bound`, rejected otherwise.
fn keep(kind: CompressionType, out: Vec<u8>, bound: usize) -> Compressed {
    if out.is_empty() || out.len() > bound {
        return Compressed::Rejected;
    }
    Compressed::Kept(kind, out)
}

/// `ExtractUncompressedSize` [R util/compression.cc:386-404, :1490-1524]: the size a compressed
/// block states, and the codec's bytes after its prefix.
pub fn uncompressed_size(kind: CompressionType, data: &[u8]) -> Result<(u64, &[u8]), Error> {
    match kind {
        CompressionType::NoCompression => Err(Error::InvalidArgument {
            what: "the uncompressed size of a block that is not compressed",
        }),
        CompressionType::Snappy => {
            let (len, _) = snappy::decompressed_len(data)?;
            Ok((u64::try_from(len).unwrap_or(u64::MAX), data))
        }
        CompressionType::Xpress | CompressionType::BZip2 | CompressionType::Custom(_) => {
            Err(Error::Unsupported {
                feature: "reading blocks with compression type",
                value: u64::from(kind.as_u8()),
            })
        }
        _ => {
            let mut rest = data;
            let len = get_varint64(&mut rest).map_err(|_| {
                Error::corruption("compressed block's uncompressed size", Malformed::Truncated)
            })?;
            Ok((len, rest))
        }
    }
}

/// What compressing or decompressing blocks keeps from one block to the next: RocksDB's
/// `CompressionContext` and `Decompressor::ManagedWorkingArea`, ZSTD contexts reused across
/// blocks. A table builder or reader keeps one per thread; it holds no block's bytes.
#[derive(Debug, Default)]
pub struct Workspace {
    zstd: Option<Decoder>,
    zstd_compressor: Option<Compressor>,
}

/// `DecompressBlockData` [R table/format.cc:684-744] with the built-in decompressor
/// [R util/compression.cc:1273-1556]: `data`, a block compressed with `kind` after `dict` (empty
/// for none), decompressed to exactly the size it states. A stated size above `max_len` is
/// refused before any allocation.
pub fn decompress_block(
    kind: CompressionType,
    data: &[u8],
    dict: &[u8],
    max_len: usize,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    decompress_block_into(
        kind,
        data,
        dict,
        max_len,
        &mut Workspace::default(),
        &mut out,
    )?;
    Ok(out)
}

/// [`decompress_block`] into `out`, whose capacity is kept, with `workspace` reused: a block
/// read by a table reader that keeps both allocates nothing once they have grown.
pub fn decompress_block_into(
    kind: CompressionType,
    data: &[u8],
    dict: &[u8],
    max_len: usize,
    workspace: &mut Workspace,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let (stated, body) = uncompressed_size(kind, data)?;
    let len = usize::try_from(stated)
        .ok()
        .filter(|&len| len <= max_len)
        .ok_or(Error::LimitExceeded {
            what: "a compressed block's stated size",
            limit: u64::try_from(max_len).unwrap_or(u64::MAX),
        })?;
    match kind {
        CompressionType::Zstd => {
            zstd_decompress(body, dict, len, workspace, out)?;
        }
        CompressionType::Snappy => *out = snappy::decompress(body, len)?,
        CompressionType::Zlib => {
            if !dict.is_empty() {
                return Err(Error::Unsupported {
                    feature: "reading zlib blocks with a dictionary",
                    value: u64::from(kind.as_u8()),
                });
            }
            // RocksDB inflates every block with windowBits −14 [R util/compression.cc:1293].
            *out = deflate::decompress(body, 14, len)?;
        }
        CompressionType::Lz4 | CompressionType::Lz4hc => *out = lz4::decompress(body, dict, len)?,
        _ => {
            return Err(Error::Unsupported {
                feature: "reading blocks with compression type",
                value: u64::from(kind.as_u8()),
            });
        }
    }
    if out.len() != len {
        return Err(Error::corruption(
            "decompressed block's size",
            Malformed::CountMismatch,
        ));
    }
    Ok(())
}

/// `ZSTD_decompressDCtx` or `ZSTD_decompress_usingDict` into exactly `len` bytes of `out`: every
/// frame of `body` decoded, and its output exactly `len`. Without a dictionary the workspace's
/// decoder is reset and reused; a dictionary's decoder is made for the block.
fn zstd_decompress(
    body: &[u8],
    dict: &[u8],
    len: usize,
    workspace: &mut Workspace,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let mut own;
    let decoder = if dict.is_empty() {
        workspace
            .zstd
            .get_or_insert_with(|| Decoder::new(ZSTD_WINDOW_MAX, None))
    } else {
        own = Decoder::new(ZSTD_WINDOW_MAX, Some(Dictionary::new(dict)?));
        &mut own
    };
    out.clear();
    // The decoder's copies may run past the block's end by its slack, then let it go.
    out.try_reserve_exact(len.saturating_add(zstd::SLACK))
        .map_err(|_| Error::LimitExceeded {
            what: "a ZSTD block's output",
            limit: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
    let bad = |why| Error::corruption("ZSTD block", why);
    // The block is whole in memory: every frame is decoded straight into `out`. Frames that hold
    // more than the block states are corrupt, not a limit reached.
    decoder
        .decompress_all(body, out, len)
        .map_err(|e| match e {
            Error::LimitExceeded { .. } => bad(Malformed::TooLarge),
            e => e,
        })?;
    if out.len() != len {
        return Err(bad(Malformed::CountMismatch));
    }
    Ok(())
}
