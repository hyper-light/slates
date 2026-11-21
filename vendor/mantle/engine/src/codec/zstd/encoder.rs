//! The Zstandard encoder (RFC 8878 §3): frames a [`super::Decoder`] and any compliant decoder
//! read, built from the decoder's own tables run backwards.
//!
//! A frame is compressed whole from its input: its content size is known, so it is written as
//! one segment with the size and, when asked, the content checksum. Each block of at most 128 KB
//! is the smallest of raw, RLE and compressed; a compressed block's matches come from hash
//! chains over every byte before it in the frame, its literals are raw, RLE or Huffman-coded,
//! and its sequences' three codes each use the predefined, RLE or a block's own FSE table,
//! whichever the counts make cheapest.

use super::Corrupt;
use super::bits::Writer;
use super::decoder::{BLOCK_MAX, FRAME_MAGIC};
use super::fse::{
    CELLS_MAX, EncodeState, EncodeTable, LITERALS_LENGTH_DEFAULT, LITERALS_LENGTH_DEFAULT_LOG,
    MATCH_LENGTH_DEFAULT, MATCH_LENGTH_DEFAULT_LOG, OFFSET_DEFAULT, OFFSET_DEFAULT_LOG,
    SYMBOLS_MAX, normalize, optimal_log, table_log, write_distribution,
};
use super::huffman::{HuffmanEncoder, LITERALS, MAX_BITS, code_lengths};
use super::sequences::{
    LITERALS_LENGTH_CODES, LITERALS_LENGTH_MAX_LOG, MATCH_LENGTH_CODES, MATCH_LENGTH_MAX_LOG,
    OFFSET_MAX_LOG,
};
use crate::error::Error;
use crate::util::xxhash::xxh64;

/// The bytes the match finder hashes: a match it finds is at least this long, whatever shorter
/// one a level allows (the format codes matches from 3, §3.1.1.3.2.1.1).
const HASHED: usize = 4;
/// `ZSTD_HASHLOG_MIN` (zstd 1.5.7, lib/zstd.h:1268): the smallest window and hash logs the
/// reference fits parameters to.
const HASH_LOG_MIN: u32 = 6;
/// The highest offset code the predefined offset table holds (§3.1.1.3.2.2.3: N = 28).
const OFFSET_DEFAULT_MAX_CODE: u8 = 28;

/// The reference's strategies (zstd 1.5.7, lib/zstd.h, `ZSTD_strategy`), in its order: each
/// searches harder than the one before.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Strategy {
    Fast,
    DFast,
    Greedy,
    Lazy,
    Lazy2,
    BtLazy2,
    BtOpt,
    BtUltra,
    BtUltra2,
}

/// The reference's parameters by input size and by level 0 (the base of the negative levels) to
/// 22 (zstd 1.5.7, lib/compress/clevels.h, `ZSTD_defaultCParameters`, generated from it): for
/// each, the window's log `W`, the chain table's log `C`, the hash table's log `H`, the search
/// log `S`, the shortest match `L`, the target length `T`, and the strategy. Tables 0 to 3 are
/// for inputs above 256 KB, of at most 256 KB, 128 KB and 16 KB, as `ZSTD_getCParams` chooses
/// them.
type Params = (u32, u32, u32, u32, usize, u32, Strategy);

const LEVELS: [[Params; 23]; 4] = [
    // "default" - for any srcSize > 256 KB
    [
        (19, 12, 13, 1, 6, 1, Strategy::Fast),
        (19, 13, 14, 1, 7, 0, Strategy::Fast),
        (20, 15, 16, 1, 6, 0, Strategy::Fast),
        (21, 16, 17, 1, 5, 0, Strategy::DFast),
        (21, 18, 18, 1, 5, 0, Strategy::DFast),
        (21, 18, 19, 3, 5, 2, Strategy::Greedy),
        (21, 18, 19, 3, 5, 4, Strategy::Lazy),
        (21, 19, 20, 4, 5, 8, Strategy::Lazy),
        (21, 19, 20, 4, 5, 16, Strategy::Lazy2),
        (22, 20, 21, 4, 5, 16, Strategy::Lazy2),
        (22, 21, 22, 5, 5, 16, Strategy::Lazy2),
        (22, 21, 22, 6, 5, 16, Strategy::Lazy2),
        (22, 22, 23, 6, 5, 32, Strategy::Lazy2),
        (22, 22, 22, 4, 5, 32, Strategy::BtLazy2),
        (22, 22, 23, 5, 5, 32, Strategy::BtLazy2),
        (22, 23, 23, 6, 5, 32, Strategy::BtLazy2),
        (22, 22, 22, 5, 5, 48, Strategy::BtOpt),
        (23, 23, 22, 5, 4, 64, Strategy::BtOpt),
        (23, 23, 22, 6, 3, 64, Strategy::BtUltra),
        (23, 24, 22, 7, 3, 256, Strategy::BtUltra2),
        (25, 25, 23, 7, 3, 256, Strategy::BtUltra2),
        (26, 26, 24, 7, 3, 512, Strategy::BtUltra2),
        (27, 27, 25, 9, 3, 999, Strategy::BtUltra2),
    ],
    // for srcSize <= 256 KB
    [
        (18, 12, 13, 1, 5, 1, Strategy::Fast),
        (18, 13, 14, 1, 6, 0, Strategy::Fast),
        (18, 14, 14, 1, 5, 0, Strategy::DFast),
        (18, 16, 16, 1, 4, 0, Strategy::DFast),
        (18, 16, 17, 3, 5, 2, Strategy::Greedy),
        (18, 17, 18, 5, 5, 2, Strategy::Greedy),
        (18, 18, 19, 3, 5, 4, Strategy::Lazy),
        (18, 18, 19, 4, 4, 4, Strategy::Lazy),
        (18, 18, 19, 4, 4, 8, Strategy::Lazy2),
        (18, 18, 19, 5, 4, 8, Strategy::Lazy2),
        (18, 18, 19, 6, 4, 8, Strategy::Lazy2),
        (18, 18, 19, 5, 4, 12, Strategy::BtLazy2),
        (18, 19, 19, 7, 4, 12, Strategy::BtLazy2),
        (18, 18, 19, 4, 4, 16, Strategy::BtOpt),
        (18, 18, 19, 4, 3, 32, Strategy::BtOpt),
        (18, 18, 19, 6, 3, 128, Strategy::BtOpt),
        (18, 19, 19, 6, 3, 128, Strategy::BtUltra),
        (18, 19, 19, 8, 3, 256, Strategy::BtUltra),
        (18, 19, 19, 6, 3, 128, Strategy::BtUltra2),
        (18, 19, 19, 8, 3, 256, Strategy::BtUltra2),
        (18, 19, 19, 10, 3, 512, Strategy::BtUltra2),
        (18, 19, 19, 12, 3, 512, Strategy::BtUltra2),
        (18, 19, 19, 13, 3, 999, Strategy::BtUltra2),
    ],
    // for srcSize <= 128 KB
    [
        (17, 12, 12, 1, 5, 1, Strategy::Fast),
        (17, 12, 13, 1, 6, 0, Strategy::Fast),
        (17, 13, 15, 1, 5, 0, Strategy::Fast),
        (17, 15, 16, 2, 5, 0, Strategy::DFast),
        (17, 17, 17, 2, 4, 0, Strategy::DFast),
        (17, 16, 17, 3, 4, 2, Strategy::Greedy),
        (17, 16, 17, 3, 4, 4, Strategy::Lazy),
        (17, 16, 17, 3, 4, 8, Strategy::Lazy2),
        (17, 16, 17, 4, 4, 8, Strategy::Lazy2),
        (17, 16, 17, 5, 4, 8, Strategy::Lazy2),
        (17, 16, 17, 6, 4, 8, Strategy::Lazy2),
        (17, 17, 17, 5, 4, 8, Strategy::BtLazy2),
        (17, 18, 17, 7, 4, 12, Strategy::BtLazy2),
        (17, 18, 17, 3, 4, 12, Strategy::BtOpt),
        (17, 18, 17, 4, 3, 32, Strategy::BtOpt),
        (17, 18, 17, 6, 3, 256, Strategy::BtOpt),
        (17, 18, 17, 6, 3, 128, Strategy::BtUltra),
        (17, 18, 17, 8, 3, 256, Strategy::BtUltra),
        (17, 18, 17, 10, 3, 512, Strategy::BtUltra),
        (17, 18, 17, 5, 3, 256, Strategy::BtUltra2),
        (17, 18, 17, 7, 3, 512, Strategy::BtUltra2),
        (17, 18, 17, 9, 3, 512, Strategy::BtUltra2),
        (17, 18, 17, 11, 3, 999, Strategy::BtUltra2),
    ],
    // for srcSize <= 16 KB
    [
        (14, 12, 13, 1, 5, 1, Strategy::Fast),
        (14, 14, 15, 1, 5, 0, Strategy::Fast),
        (14, 14, 15, 1, 4, 0, Strategy::Fast),
        (14, 14, 15, 2, 4, 0, Strategy::DFast),
        (14, 14, 14, 4, 4, 2, Strategy::Greedy),
        (14, 14, 14, 3, 4, 4, Strategy::Lazy),
        (14, 14, 14, 4, 4, 8, Strategy::Lazy2),
        (14, 14, 14, 6, 4, 8, Strategy::Lazy2),
        (14, 14, 14, 8, 4, 8, Strategy::Lazy2),
        (14, 15, 14, 5, 4, 8, Strategy::BtLazy2),
        (14, 15, 14, 9, 4, 8, Strategy::BtLazy2),
        (14, 15, 14, 3, 4, 12, Strategy::BtOpt),
        (14, 15, 14, 4, 3, 24, Strategy::BtOpt),
        (14, 15, 14, 5, 3, 32, Strategy::BtUltra),
        (14, 15, 15, 6, 3, 64, Strategy::BtUltra),
        (14, 15, 15, 7, 3, 256, Strategy::BtUltra),
        (14, 15, 15, 5, 3, 48, Strategy::BtUltra2),
        (14, 15, 15, 6, 3, 128, Strategy::BtUltra2),
        (14, 15, 15, 7, 3, 256, Strategy::BtUltra2),
        (14, 15, 15, 8, 3, 256, Strategy::BtUltra2),
        (14, 15, 15, 8, 3, 512, Strategy::BtUltra2),
        (14, 15, 15, 9, 3, 512, Strategy::BtUltra2),
        (14, 15, 15, 10, 3, 999, Strategy::BtUltra2),
    ],
];

/// How hard the encoder looks for matches, a level's row of [`LEVELS`] fitted to the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    level: i32,
    hash_log: u32,
    chain_log: u32,
    /// Positions a chain search tries: `2^S`.
    chain: u32,
    min_match: usize,
    strategy: Strategy,
}

impl Level {
    /// A level by number, as RocksDB passes one: 1 to 22, the negative levels taking the
    /// reference's base row (0), above 22 the 22nd. Its parameters are those for inputs above
    /// 256 KB until [`Level::fit`] fits them to an input.
    pub fn new(level: i32) -> Self {
        Self::from_table(level, 0, None)
    }

    fn from_table(level: i32, table: usize, src_len: Option<usize>) -> Self {
        let row = usize::try_from(level.clamp(0, 22)).unwrap_or(0);
        let (mut window_log, mut chain_log, mut hash_log, search_log, min_match, _, strategy) =
            LEVELS
                .get(table)
                .and_then(|t| t.get(row))
                .copied()
                .unwrap_or((19, 12, 13, 1, 6, 1, Strategy::Fast));
        // `ZSTD_adjustCParams_internal` (zstd 1.5.7, lib/compress/zstd_compress.c): a known
        // input shrinks the window to its size, at least 2^6 (`ZSTD_HASHLOG_MIN`); the hash table
        // to twice the window; and the chain table's cycle (one log less for the binary-tree
        // strategies, `ZSTD_cycleLog`) to the window.
        if let Some(len) = src_len {
            let src_log = if len < 1 << HASH_LOG_MIN {
                HASH_LOG_MIN
            } else {
                usize::BITS.saturating_sub(len.saturating_sub(1).leading_zeros())
            };
            window_log = window_log.min(src_log);
            hash_log = hash_log.min(window_log.saturating_add(1));
            let cycle_log = chain_log.saturating_sub(u32::from(strategy >= Strategy::BtLazy2));
            if cycle_log > window_log {
                chain_log = chain_log.saturating_sub(cycle_log.saturating_sub(window_log));
            }
        }
        Self {
            level,
            hash_log,
            chain_log,
            chain: 1u32 << search_log,
            min_match: min_match.max(HASHED),
            strategy,
        }
    }

    /// The parameters the reference uses for an input of `len` bytes: `ZSTD_getCParams`'s table
    /// by size (lib/compress/zstd_compress.c:7759-7762), then its adjustment to the size.
    pub fn fit(self, len: usize) -> Self {
        let table = [256usize << 10, 128 << 10, 16 << 10]
            .iter()
            .filter(|&&bound| len <= bound)
            .count();
        Self::from_table(self.level, table, Some(len))
    }

    /// Whether the chain search looks one position ahead before taking a match.
    fn lazy(self) -> bool {
        self.strategy >= Strategy::Lazy
    }
}

/// Compresses `input` as one frame, with the content checksum when `checksum`.
pub fn compress(input: &[u8], level: Level, checksum: bool) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    Compressor::new()?.compress_into(input, level, checksum, &mut out)?;
    Ok(out)
}

/// The predefined distributions' encoding tables (§3.1.1.3.2.2), built once a compressor.
#[derive(Debug)]
struct Predefined {
    literals_length: EncodeTable,
    offset: EncodeTable,
    match_length: EncodeTable,
}

/// What compressing keeps from one frame to the next: the reference's `ZSTD_CCtx` working area,
/// which RocksDB keeps a thread's. The match finder's tables, the block's sequences and
/// literals, the predefined tables and the cost table are allocated once and reused.
#[derive(Debug)]
pub struct Compressor {
    head: Vec<u32>,
    chain: Vec<u32>,
    /// What the next frame's first position is stored as in `head` and `chain`: entries below it
    /// are earlier frames', so the tables are never cleared between frames, as the reference
    /// continues its window's indices (`ZSTD_window_clear`, zstd 1.5.7
    /// lib/compress/zstd_compress_internal.h). 0 is an empty entry.
    base: u32,
    found: Vec<Found>,
    literals: Vec<u8>,
    codes: Vec<Codes>,
    /// The block's own encoding tables, rebuilt in place: literals length, offset, match length.
    own: [EncodeTable; 3],
    predefined: Predefined,
    /// `256·log2(n)` of a symbol's cells `n`, 0 to the largest table's size.
    log2: Vec<u16>,
    lengths: LengthCodes,
}

impl Compressor {
    /// A compressor with its fixed tables built.
    pub fn new() -> Result<Self, Error> {
        let log2 = (0..=CELLS_MAX)
            .map(|n| u16::try_from(log2_256(u64::try_from(n).unwrap_or(0))).unwrap_or(u16::MAX))
            .collect();
        Ok(Self {
            head: Vec::new(),
            chain: Vec::new(),
            base: 1,
            found: Vec::new(),
            literals: Vec::new(),
            codes: Vec::new(),
            own: Default::default(),
            predefined: Predefined {
                literals_length: EncodeTable::new(
                    &LITERALS_LENGTH_DEFAULT,
                    LITERALS_LENGTH_DEFAULT_LOG,
                )?,
                offset: EncodeTable::new(&OFFSET_DEFAULT, OFFSET_DEFAULT_LOG)?,
                match_length: EncodeTable::new(&MATCH_LENGTH_DEFAULT, MATCH_LENGTH_DEFAULT_LOG)?,
            },
            log2,
            lengths: LengthCodes::new(),
        })
    }

    /// Compresses `input` as one frame onto the end of `out`, with the content checksum when
    /// `checksum`.
    pub fn compress_into(
        &mut self,
        input: &[u8],
        level: Level,
        checksum: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        Ok(self.encode_frame(input, level, checksum, out)?)
    }

    fn encode_frame(
        &mut self,
        input: &[u8],
        level: Level,
        checksum: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), Corrupt> {
        let level = level.fit(input.len());
        out.reserve((input.len() / 2).saturating_add(32));
        out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        frame_header(out, input.len(), checksum);
        // A frame whose positions would pass u32 from the base starts the tables over.
        let len = u32::try_from(input.len()).unwrap_or(u32::MAX);
        let next_base = match self.base.checked_add(len) {
            Some(next) => next,
            None => {
                self.head.fill(0);
                self.chain.fill(0);
                self.base = 1;
                1u32.saturating_add(len)
            }
        };
        let mut matcher = Matcher::new(input, level, &mut self.head, &mut self.chain, self.base);
        self.base = next_base;
        let mut offsets = [1u32, 4, 8];
        let mut start = 0usize;
        loop {
            let end = start.saturating_add(BLOCK_MAX).min(input.len());
            let last = end == input.len();
            let mut work = Work {
                found: &mut self.found,
                literals: &mut self.literals,
                block: &mut *out,
                codes: &mut self.codes,
                own: &mut self.own,
                predefined: &self.predefined,
                log2: &self.log2,
                lengths: &self.lengths,
            };
            encode_block(
                input,
                start..end,
                last,
                &mut matcher,
                &mut offsets,
                &mut work,
            )?;
            start = end;
            if last {
                break;
            }
        }
        if checksum {
            let digest = xxh64(input, 0);
            out.extend_from_slice(
                &u32::try_from(digest & 0xFFFF_FFFF)
                    .unwrap_or(0)
                    .to_le_bytes(),
            );
        }
        Ok(())
    }
}

/// A block's working buffers and the fixed tables, lent by the [`Compressor`].
struct Work<'a> {
    found: &'a mut Vec<Found>,
    literals: &'a mut Vec<u8>,
    /// The frame being written: each block's header and content go straight onto its end.
    block: &'a mut Vec<u8>,
    codes: &'a mut Vec<Codes>,
    own: &'a mut [EncodeTable; 3],
    predefined: &'a Predefined,
    log2: &'a [u16],
    lengths: &'a LengthCodes,
}

/// A sequence's three codes, each with its extra bits' value and count: literals length, match
/// length, offset.
type Codes = [(u8, u32, u8); 3];

/// A single-segment frame header with the content size (§3.1.1.1).
fn frame_header(out: &mut Vec<u8>, size: usize, checksum: bool) {
    let size = u64::try_from(size).unwrap_or(u64::MAX);
    // FCS_Field_Size 1, 2, 4 or 8 bytes; 2 stores the size less 256 (§3.1.1.1.4).
    let (flag, bytes): (u8, Vec<u8>) = if size < 256 {
        (0, vec![u8::try_from(size).unwrap_or(0)])
    } else if size < 65_536 + 256 {
        (
            1,
            u16::try_from(size.saturating_sub(256))
                .unwrap_or(0)
                .to_le_bytes()
                .to_vec(),
        )
    } else if size <= u64::from(u32::MAX) {
        (2, u32::try_from(size).unwrap_or(0).to_le_bytes().to_vec())
    } else {
        (3, size.to_le_bytes().to_vec())
    };
    let descriptor = (flag << 6) | 0b0010_0000 | if checksum { 0b0000_0100 } else { 0 };
    out.push(descriptor);
    out.extend_from_slice(&bytes);
}

/// One block, `range` of `input`: compressed if that is smaller than raw, RLE when every byte is
/// one.
fn encode_block(
    input: &[u8],
    range: std::ops::Range<usize>,
    last: bool,
    matcher: &mut Matcher<'_>,
    offsets: &mut [u32; 3],
    work: &mut Work<'_>,
) -> Result<(), Corrupt> {
    let (start, end) = (range.start, range.end);
    let block = input.get(start..end).ok_or(Corrupt::Block)?;
    let header = |kind: u32, size: usize| -> Result<[u8; 3], Corrupt> {
        let size = u32::try_from(size).map_err(|_| Corrupt::Block)?;
        let raw = u32::from(last) | (kind << 1) | (size << 3);
        let b = raw.to_le_bytes();
        Ok([b[0], b[1], b[2]])
    };
    let out = &mut *work.block;
    if let Some(&first) = block.first()
        && block.len() > 1
        && block.iter().all(|&b| b == first)
    {
        out.extend_from_slice(&header(1, block.len())?);
        out.push(first);
        // The match finder still learns the block's positions for the blocks after it.
        matcher.skip(start, end);
        return Ok(());
    }
    let saved = *offsets;
    matcher.sequences(start, end, offsets, work.found);
    // The content is written after a header's room, straight into the frame, and the header
    // filled in once its size is known; content no smaller than the block is cut back for the
    // raw block, so a block's bytes are never copied from a buffer of their own.
    let at = work.block.len();
    work.block.extend_from_slice(&[0; 3]);
    compress_block(input, start, end, matcher.level.strategy, work)?;
    let out = &mut *work.block;
    let content = out.len().saturating_sub(at).saturating_sub(3);
    if content < block.len() {
        out.get_mut(at..at.saturating_add(3))
            .ok_or(Corrupt::Block)?
            .copy_from_slice(&header(2, content)?);
    } else {
        // Raw: a raw block does not move the repeat offsets (§3.1.1.5).
        *offsets = saved;
        out.truncate(at);
        out.extend_from_slice(&header(0, block.len())?);
        out.extend_from_slice(block);
    }
    Ok(())
}

/// The bytes one literal copy moves (`ZSTD_safecopyLiterals`'s `WILDCOPY_OVERLENGTH`-bounded
/// pieces are 16 bytes, zstd 1.5.7 lib/compress/zstd_compress_internal.h): a run this long or
/// shorter is one fixed-size copy.
const LITERAL_PIECE: usize = 16;

/// A match the finder chose: `literals` bytes before it, then `length` bytes from `offset_value`
/// (the coded value, 1 to 3 a repeat offset, §3.1.1.5).
#[derive(Clone, Copy, Debug)]
struct Found {
    literals: u32,
    offset_value: u32,
    length: u32,
}

/// A hash table's entry for a search position: where it points in the frame (the frame's first
/// byte when it is not this frame's), and whether it is this frame's.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    at: usize,
    valid: bool,
}

/// Hash chains over the frame's input: `head` the latest position of each 4-byte hash, `chain`
/// each position's previous one of the same hash.
struct Matcher<'a> {
    input: &'a [u8],
    level: Level,
    head: &'a mut [u32],
    chain: &'a mut [u32],
    /// What the frame's first position is stored as; lower entries are earlier frames'.
    base: u32,
    /// Positions inserted so far.
    next: usize,
}

/// The bytes the double-fast search reads at a position (`HASH_READ_SIZE`, zstd 1.5.7
/// lib/compress/zstd_compress_internal.h).
const HASH_READ: usize = 8;
/// Positions searched without a match before the step between them grows by one:
/// `1 << kSearchStrength`, `kSearchStrength` 8 (zstd 1.5.7 lib/compress/zstd_compress_internal.h).
const STEP_AFTER: usize = 1 << 8;
/// The reference's multiplicative hash constants (zstd 1.5.7
/// lib/compress/zstd_compress_internal.h, `prime4bytes` to `prime8bytes`).
const PRIME_4: u32 = 2_654_435_761;
const PRIME_5: u64 = 889_523_592_379;
const PRIME_6: u64 = 227_718_039_650_203;
const PRIME_7: u64 = 58_295_818_150_454_627;
const PRIME_8: u64 = 0xCF1B_BCDC_B7A5_6463;
/// A word's low 4 bytes.
const LOW_4: u64 = 0xFFFF_FFFF;

impl<'a> Matcher<'a> {
    /// A matcher over `input` in `head` and `chain`, sized for it, the frame's positions stored
    /// from `base`: for the double-fast search, the long and short hash tables; for the chain
    /// search, the hash table and a link a position. Entries of earlier frames stay, below
    /// `base`, so nothing is cleared.
    fn new(
        input: &'a [u8],
        level: Level,
        head: &'a mut Vec<u32>,
        chain: &'a mut Vec<u32>,
        base: u32,
    ) -> Self {
        let grow = |table: &mut Vec<u32>, len: usize| {
            if table.len() < len {
                table.resize(len, 0);
            }
        };
        grow(head, 1usize << level.hash_log);
        if level.strategy == Strategy::DFast {
            grow(chain, 1usize << level.chain_log);
        } else {
            grow(chain, input.len());
        }
        Self {
            input,
            level,
            head,
            chain,
            base,
            next: 0,
        }
    }

    /// A frame position as the tables store it.
    #[inline(always)]
    fn stored(&self, at: usize) -> u32 {
        u32::try_from(at)
            .ok()
            .and_then(|at| self.base.checked_add(at))
            .unwrap_or(0)
    }

    /// The frame position a stored entry names, if it is this frame's.
    #[inline(always)]
    fn position(&self, entry: u32) -> Option<usize> {
        entry
            .checked_sub(self.base)
            .and_then(|at| usize::try_from(at).ok())
    }

    fn hash(&self, at: usize) -> Option<usize> {
        let b = self.input.get(at..at.checked_add(4)?)?;
        let v = u32::from_le_bytes([*b.first()?, *b.get(1)?, *b.get(2)?, *b.get(3)?]);
        // Knuth's multiplicative hash (TAOCP 3, §6.4): the golden-ratio multiplier's top bits.
        let shift = 32u32.saturating_sub(self.level.hash_log);
        Some(
            usize::try_from(v.wrapping_mul(0x9E37_79B1).checked_shr(shift).unwrap_or(0))
                .unwrap_or(0),
        )
    }

    /// Inserts every position before `to`.
    fn insert_to(&mut self, to: usize) {
        while self.next < to {
            let stored = self.stored(self.next);
            if let Some(h) = self.hash(self.next)
                && let (Some(slot), Some(link)) =
                    (self.head.get_mut(h), self.chain.get_mut(self.next))
            {
                *link = *slot;
                *slot = stored;
            }
            self.next = self.next.saturating_add(1);
        }
    }

    fn skip(&mut self, _start: usize, end: usize) {
        self.insert_to(end);
    }

    /// The length the input matches itself at `a` and `b` (`b` later), up to `limit`.
    #[inline]
    fn common(&self, a: usize, b: usize, limit: usize) -> usize {
        let (Some(x), Some(y)) = (self.input.get(a..), self.input.get(b..limit)) else {
            return 0;
        };
        // Eight bytes at a time, as `ZSTD_count` (zstd 1.5.7, lib/compress/zstd_compress_internal.h)
        // counts: the lowest set bit of two little-endian words' difference lies in their first
        // differing byte.
        let (mut x, mut y, mut n) = (x, y, 0usize);
        while let (Some((xw, xr)), Some((yw, yr))) =
            (x.split_first_chunk::<8>(), y.split_first_chunk::<8>())
        {
            let d = u64::from_le_bytes(*xw) ^ u64::from_le_bytes(*yw);
            if d != 0 {
                return n.saturating_add((d.trailing_zeros() / 8) as usize);
            }
            n = n.saturating_add(8);
            x = xr;
            y = yr;
        }
        n.saturating_add(x.iter().zip(y).take_while(|(p, q)| p == q).count())
    }

    /// The longest match at `at` within the block's end `end`, trying the last offset and then the
    /// chain: its length and offset.
    fn best(&mut self, at: usize, end: usize, rep: u32) -> (usize, usize) {
        self.insert_to(at);
        let mut best = (0usize, 0usize);
        let rep = usize::try_from(rep).unwrap_or(0);
        if rep > 0 && rep <= at {
            let len = self.common(at.saturating_sub(rep), at, end);
            if len >= self.level.min_match {
                best = (len, rep);
            }
        }
        let Some(h) = self.hash(at) else {
            return best;
        };
        let mut candidate = self.head.get(h).and_then(|&e| self.position(e));
        let mut tries = self.level.chain;
        while let Some(c) = candidate
            && tries > 0
        {
            let len = self.common(c, at, end);
            if len > best.0 {
                best = (len, at.saturating_sub(c));
                if at.saturating_add(len) >= end {
                    break;
                }
            }
            candidate = self.chain.get(c).and_then(|&e| self.position(e));
            tries = tries.saturating_sub(1);
        }
        if best.0 < self.level.min_match {
            (0, 0)
        } else {
            best
        }
    }

    /// The little-endian word of the 8 bytes at `at`; 0 where the input does not hold them,
    /// which the search's limit keeps from happening at any position it reads.
    #[inline(always)]
    fn read64(&self, at: usize) -> u64 {
        self.input
            .get(at..)
            .and_then(<[u8]>::first_chunk::<8>)
            .map_or(0, |b| u64::from_le_bytes(*b))
    }

    /// The long hash table's slot for a position's word: its 8 bytes' hash (`ZSTD_hash8`).
    #[inline(always)]
    fn long_slot(&self, v: u64) -> usize {
        let shift = 64u32.saturating_sub(self.level.hash_log);
        usize::try_from(v.wrapping_mul(PRIME_8).checked_shr(shift).unwrap_or(0)).unwrap_or(0)
    }

    /// The short hash table's slot for a position's word: the hash of its shortest match's bytes,
    /// 4 to 7 (`ZSTD_hashPtr`; a minimum of 8 hashes 4, as the double-fast search's template
    /// does).
    #[inline(always)]
    fn short_slot(&self, v: u64) -> usize {
        let h = self.level.chain_log;
        let hashed = match self.level.min_match {
            5 => (v << 24)
                .wrapping_mul(PRIME_5)
                .checked_shr(64u32.saturating_sub(h)),
            6 => (v << 16)
                .wrapping_mul(PRIME_6)
                .checked_shr(64u32.saturating_sub(h)),
            7 => (v << 8)
                .wrapping_mul(PRIME_7)
                .checked_shr(64u32.saturating_sub(h)),
            _ => u64::from(u32::try_from(v & LOW_4).unwrap_or(0).wrapping_mul(PRIME_4))
                .checked_shr(32u32.saturating_sub(h)),
        };
        usize::try_from(hashed.unwrap_or(0)).unwrap_or(0)
    }

    /// The long hash table's slot for `at`.
    #[inline(always)]
    fn hash_long(&self, at: usize) -> usize {
        self.long_slot(self.read64(at))
    }

    /// The short hash table's slot for `at`.
    #[inline(always)]
    fn hash_short(&self, at: usize) -> usize {
        self.short_slot(self.read64(at))
    }

    /// The long table's candidate at `slot`: its position and whether it is this frame's.
    #[inline(always)]
    fn long_at(&self, slot: usize) -> Candidate {
        self.candidate(self.head.get(slot).copied().unwrap_or(0))
    }

    /// The short table's candidate at `slot`.
    #[inline(always)]
    fn short_at(&self, slot: usize) -> Candidate {
        self.candidate(self.chain.get(slot).copied().unwrap_or(0))
    }

    /// Zero exactly when `c` is this frame's and its bytes under `mask` equal `v`'s: the
    /// difference of the words with the candidate's invalidity or-ed in, so the compiler has no
    /// separate validity test to branch on (see [`Self::candidate`]).
    #[inline(always)]
    fn differs(&self, c: Candidate, v: u64, mask: u64) -> u64 {
        ((self.read64(c.at) ^ v) & mask) | u64::from(!c.valid)
    }

    /// An entry as a candidate: an earlier frame's (below `base`) is not this frame's, and its
    /// position is the frame's first byte, so it is read like any other and only `valid` keeps
    /// it from matching. Validity then joins the data comparison in one branch ([`Self::differs`]), as the tables
    /// are never cleared and early in a frame its entries and older ones alternate past
    /// prediction: tested apart, that branch was the search's most mispredicted.
    #[inline(always)]
    fn candidate(&self, entry: u32) -> Candidate {
        let valid = entry >= self.base;
        let at = usize::try_from(entry.wrapping_sub(self.base)).unwrap_or(0);
        Candidate {
            at: if valid { at } else { 0 },
            valid,
        }
    }

    /// Sets the long table's `slot` to `at`.
    #[inline(always)]
    fn put_long(&mut self, slot: usize, at: usize) {
        let stored = self.stored(at);
        if let Some(e) = self.head.get_mut(slot) {
            *e = stored;
        }
    }

    /// Sets the short table's `slot` to `at`.
    #[inline(always)]
    fn put_short(&mut self, slot: usize, at: usize) {
        let stored = self.stored(at);
        if let Some(e) = self.chain.get_mut(slot) {
            *e = stored;
        }
    }

    /// The block's matches by the reference's double-fast search
    /// (`ZSTD_compressBlock_doubleFast_noDict_generic`, zstd 1.5.7
    /// lib/compress/zstd_double_fast.c): at each position a repeat of the last offset one ahead,
    /// then the long table's 8-byte match, then the short table's, extended by the long match
    /// one ahead when that is longer; the step between positions grows by one every 2^8
    /// positions without a match, and after a match the repeats of the second offset are taken
    /// at once. Only the positions searched and a few around each match enter the tables.
    fn double_fast(
        &mut self,
        start: usize,
        end: usize,
        offsets: &mut [u32; 3],
        out: &mut Vec<Found>,
    ) {
        // The last position whose 8 bytes the block holds: every read is within the block.
        let Some(limit) = end.checked_sub(HASH_READ) else {
            return;
        };
        let mut anchor = start;
        // The frame's first byte starts no search, as the reference's window begins after it.
        let mut ip = start.saturating_add(usize::from(start == 0));
        'outer: loop {
            let mut step = 1usize;
            let mut next_step = ip.saturating_add(STEP_AFTER);
            let mut ip1 = ip.saturating_add(step);
            if ip1 > limit {
                break;
            }
            // Each position's 8 bytes are read once: both hashes and every comparison at it use
            // them, and the next position's carry over when the search steps.
            let mut v0 = self.read64(ip);
            let mut hl0 = self.long_slot(v0);
            let mut idxl0 = self.long_at(hl0);
            // One search position at a time until a match: its start, offset and length, the
            // position searched, and the long slot and position one ahead with the step.
            let (at, offset, length, curr, (hl1, ip1, step)) = loop {
                let hs0 = self.short_slot(v0);
                let idxs0 = self.short_at(hs0);
                let curr = ip;
                self.put_long(hl0, curr);
                self.put_short(hs0, curr);
                // The last offset, one position ahead: bytes 1 to 4 of the word.
                let rep1 = usize::try_from(offsets[0]).unwrap_or(0);
                if rep1 > 0 && rep1 <= ip {
                    let (from, to) = (
                        ip.saturating_add(1).saturating_sub(rep1),
                        ip.saturating_add(1),
                    );
                    if self.read64(from) & LOW_4 == (v0 >> 8) & LOW_4 {
                        let length = self
                            .common(from.saturating_add(4), to.saturating_add(4), end)
                            .saturating_add(4);
                        self.store(out, offsets, anchor, to, rep1, length);
                        (ip, anchor) =
                            self.after(out, offsets, to.saturating_add(length), curr, end);
                        continue 'outer;
                    }
                }
                let v1 = self.read64(ip1);
                let hl1 = self.long_slot(v1);
                if self.differs(idxl0, v0, u64::MAX) == 0 {
                    let m = idxl0.at;
                    let length = self
                        .common(m.saturating_add(8), ip.saturating_add(8), end)
                        .saturating_add(8);
                    let (at, m, length) = self.catch_up(ip, m, length, anchor);
                    break (at, at.saturating_sub(m), length, curr, (hl1, ip1, step));
                }
                let idxl1 = self.long_at(hl1);
                if self.differs(idxs0, v0, LOW_4) == 0 {
                    let m = idxs0.at;
                    // A short match: a long one one position ahead replaces it if longer.
                    let mut found = (
                        ip,
                        m,
                        self.common(m.saturating_add(4), ip.saturating_add(4), end)
                            .saturating_add(4),
                    );
                    if self.differs(idxl1, v1, u64::MAX) == 0 {
                        let m1 = idxl1.at;
                        let l1 = self
                            .common(m1.saturating_add(8), ip1.saturating_add(8), end)
                            .saturating_add(8);
                        if l1 > found.2 {
                            found = (ip1, m1, l1);
                        }
                    }
                    let (at, m, length) = self.catch_up(found.0, found.1, found.2, anchor);
                    break (at, at.saturating_sub(m), length, curr, (hl1, ip1, step));
                }
                if ip1 >= next_step {
                    step = step.saturating_add(1);
                    next_step = next_step.saturating_add(STEP_AFTER);
                }
                ip = ip1;
                ip1 = ip1.saturating_add(step);
                v0 = v1;
                hl0 = hl1;
                idxl0 = idxl1;
                if ip1 > limit {
                    break 'outer;
                }
            };
            // The long slot one ahead takes that position while it is surely before the match's
            // end: a match is at least 4 bytes, so with a step below 4 it is.
            if step < 4 {
                self.put_long(hl1, ip1);
            }
            self.store(out, offsets, anchor, at, offset, length);
            (ip, anchor) = self.after(out, offsets, at.saturating_add(length), curr, end);
        }
    }

    /// Extends a match at `at` from `m` backward over the bytes before both that agree, down to
    /// the anchor and the frame's second byte.
    #[inline(always)]
    fn catch_up(
        &self,
        mut at: usize,
        mut m: usize,
        mut length: usize,
        anchor: usize,
    ) -> (usize, usize, usize) {
        while at > anchor
            && m > 0
            && self.input.get(at.wrapping_sub(1)) == self.input.get(m.wrapping_sub(1))
        {
            at = at.wrapping_sub(1);
            m = m.wrapping_sub(1);
            length = length.saturating_add(1);
        }
        (at, m, length)
    }

    /// Records a match of `length` at `at` from `offset` back, after the literals from
    /// `anchor`.
    #[inline(always)]
    fn store(
        &self,
        out: &mut Vec<Found>,
        offsets: &mut [u32; 3],
        anchor: usize,
        at: usize,
        offset: usize,
        length: usize,
    ) {
        let literals = u32::try_from(at.saturating_sub(anchor)).unwrap_or(0);
        let offset_value = code_offset(offsets, u32::try_from(offset).unwrap_or(0), literals);
        out.push(Found {
            literals,
            offset_value,
            length: u32::try_from(length).unwrap_or(0),
        });
    }

    /// After a match ending at `ip`, found searching `curr`, in a block ending at `end`: the
    /// positions two past `curr` and just before the match's end enter both tables, then every
    /// immediate repeat of the second offset is taken. Returns the next position and the anchor.
    fn after(
        &mut self,
        out: &mut Vec<Found>,
        offsets: &mut [u32; 3],
        mut ip: usize,
        curr: usize,
        end: usize,
    ) -> (usize, usize) {
        let limit = end.saturating_sub(HASH_READ);
        if ip > limit {
            return (ip, ip);
        }
        let two = curr.saturating_add(2);
        let (h, s) = (self.hash_long(two), self.hash_short(two));
        self.put_long(h, two);
        self.put_short(s, two);
        let (before_end, last) = (ip.saturating_sub(2), ip.saturating_sub(1));
        let h = self.hash_long(before_end);
        self.put_long(h, before_end);
        let s = self.hash_short(last);
        self.put_short(s, last);
        while ip <= limit {
            let rep2 = usize::try_from(offsets[1]).unwrap_or(0);
            if rep2 == 0
                || rep2 > ip
                || self.read64(ip.saturating_sub(rep2)) & LOW_4 != self.read64(ip) & LOW_4
            {
                break;
            }
            let length = self
                .common(
                    ip.saturating_sub(rep2).saturating_add(4),
                    ip.saturating_add(4),
                    end,
                )
                .saturating_add(4);
            let (h, s) = (self.hash_long(ip), self.hash_short(ip));
            self.put_short(s, ip);
            self.put_long(h, ip);
            self.store(out, offsets, ip, ip, rep2, length);
            ip = ip.saturating_add(length);
        }
        (ip, ip)
    }

    /// The block's matches, greedy or one position lazy, the repeat offsets updated as the
    /// decoder will update them.
    fn sequences(
        &mut self,
        start: usize,
        end: usize,
        offsets: &mut [u32; 3],
        out: &mut Vec<Found>,
    ) {
        out.clear();
        if self.level.strategy == Strategy::DFast {
            self.double_fast(start, end, offsets, out);
            return;
        }
        let mut at = start;
        let mut anchor = start;
        while at.saturating_add(self.level.min_match) <= end {
            let (mut len, mut off) = self.best(at, end, offsets[0]);
            if len == 0 {
                at = at.saturating_add(1);
                continue;
            }
            if self.level.lazy() && at.saturating_add(1).saturating_add(self.level.min_match) <= end
            {
                let (len2, off2) = self.best(at.saturating_add(1), end, offsets[0]);
                if len2 > len.saturating_add(1) {
                    at = at.saturating_add(1);
                    len = len2;
                    off = off2;
                }
            }
            let literals = u32::try_from(at.saturating_sub(anchor)).unwrap_or(0);
            let offset = u32::try_from(off).unwrap_or(0);
            let offset_value = code_offset(offsets, offset, literals);
            out.push(Found {
                literals,
                offset_value,
                length: u32::try_from(len).unwrap_or(0),
            });
            at = at.saturating_add(len);
            anchor = at;
        }
        self.insert_to(end);
        // The literals after the last match are the block's tail; the count is implied.
        let _ = anchor;
    }
}

/// The value that codes `offset` after `literals` literals, updating the repeat offsets as the
/// decoder does (§3.1.1.5): a repeat code where the offset is one, a new offset otherwise.
fn code_offset(offsets: &mut [u32; 3], offset: u32, literals: u32) -> u32 {
    let [r1, r2, r3] = *offsets;
    if literals > 0 {
        if offset == r1 {
            return 1;
        }
        if offset == r2 {
            *offsets = [r2, r1, r3];
            return 2;
        }
        if offset == r3 {
            *offsets = [r3, r1, r2];
            return 3;
        }
    } else {
        // With no literals the codes shift: 1 names the second, 2 the third, 3 the first less one.
        if offset == r2 {
            *offsets = [r2, r1, r3];
            return 1;
        }
        if offset == r3 {
            *offsets = [r3, r1, r2];
            return 2;
        }
        if r1 > 1 && offset == r1.saturating_sub(1) {
            *offsets = [offset, r1, r2];
            return 3;
        }
    }
    *offsets = [offset, r1, r2];
    offset.saturating_add(3)
}

/// The code of a literals length, and its extra bits' value (§3.1.1.3.2.1.1, Table 16).
fn literals_length_code(len: u32) -> (u8, u32, u8) {
    code_for(len, &LITERALS_LENGTH_CODES)
}

fn match_length_code(len: u32) -> (u8, u32, u8) {
    code_for(len, &MATCH_LENGTH_CODES)
}

/// The largest code whose baseline is at most `value`: the code, the extra bits' value, their
/// count. The baselines ascend, so a binary search finds it.
#[inline]
fn code_for(value: u32, table: &[(u32, u8)]) -> (u8, u32, u8) {
    let code = table
        .partition_point(|&(base, _)| base <= value)
        .saturating_sub(1);
    let (base, bits) = table.get(code).copied().unwrap_or((0, 0));
    (
        u8::try_from(code).unwrap_or(0),
        value.saturating_sub(base),
        bits,
    )
}

/// Literals below which, and match lengths less 3 below which, a length's code is read from a
/// table; above, each code covers a power of two (§3.1.1.3.2.1.1, Tables 16 and 17), as the
/// reference's `ZSTD_LLcode` and `ZSTD_MLcode` find them.
const LITERALS_DIRECT: usize = 64;
const MATCHES_DIRECT: usize = 128;
/// The last literals length and match length codes, as the search's tables end.
const LITERALS_LAST: u8 = 35;
const MATCHES_LAST: u8 = 52;

/// The lengths' codes found without a search: tables for the short lengths, and above them the
/// highest bit plus a delta, each taken from the code tables when built.
#[derive(Debug)]
struct LengthCodes {
    literals: [u8; LITERALS_DIRECT],
    matches: [u8; MATCHES_DIRECT],
    literals_delta: u8,
    matches_delta: u8,
}

impl LengthCodes {
    fn new() -> Self {
        let mut literals = [0u8; LITERALS_DIRECT];
        for (v, code) in literals.iter_mut().enumerate() {
            *code = literals_length_code(u32::try_from(v).unwrap_or(0)).0;
        }
        let mut matches = [0u8; MATCHES_DIRECT];
        for (v, code) in matches.iter_mut().enumerate() {
            *code = match_length_code(u32::try_from(v).unwrap_or(0).saturating_add(3)).0;
        }
        // The code of the first length past each table, less its highest bit.
        let delta = |code: u8, value: usize| {
            code.saturating_sub(u8::try_from(value.trailing_zeros()).unwrap_or(0))
        };
        Self {
            literals,
            matches,
            literals_delta: delta(
                literals_length_code(u32::try_from(LITERALS_DIRECT).unwrap_or(0)).0,
                LITERALS_DIRECT,
            ),
            matches_delta: delta(
                match_length_code(u32::try_from(MATCHES_DIRECT).unwrap_or(0).saturating_add(3)).0,
                MATCHES_DIRECT,
            ),
        }
    }

    /// A literals length's code, its extra bits' value and their count.
    #[inline(always)]
    fn literals(&self, len: u32) -> (u8, u32, u8) {
        let code = usize::try_from(len)
            .ok()
            .and_then(|l| self.literals.get(l).copied())
            .unwrap_or_else(|| high_bit(len).saturating_add(self.literals_delta))
            .min(LITERALS_LAST);
        let (base, bits) = LITERALS_LENGTH_CODES
            .get(usize::from(code))
            .copied()
            .unwrap_or((0, 0));
        (code, len.saturating_sub(base), bits)
    }

    /// A match length's code, its extra bits' value and their count.
    #[inline(always)]
    fn matches(&self, len: u32) -> (u8, u32, u8) {
        let v = len.saturating_sub(3);
        let code = usize::try_from(v)
            .ok()
            .and_then(|l| self.matches.get(l).copied())
            .unwrap_or_else(|| high_bit(v).saturating_add(self.matches_delta))
            .min(MATCHES_LAST);
        let (base, bits) = MATCH_LENGTH_CODES
            .get(usize::from(code))
            .copied()
            .unwrap_or((0, 0));
        (code, len.saturating_sub(base), bits)
    }
}

/// The index of `v`'s highest set bit (0 for 0 and 1).
#[inline(always)]
fn high_bit(v: u32) -> u8 {
    u8::try_from(31u32.saturating_sub(v.leading_zeros())).unwrap_or(0)
}

/// The offset code of an offset value: its highest bit, the rest its extra bits
/// (§3.1.1.3.2.1.1).
fn offset_code(value: u32) -> (u8, u32, u8) {
    let code = u32::BITS
        .saturating_sub(1)
        .saturating_sub(value.leading_zeros());
    let base = 1u32 << code;
    let c = u8::try_from(code).unwrap_or(0);
    (c, value.saturating_sub(base), c)
}

/// A compressed block's content onto the end of `work.block`: the literals section, then the
/// sequences section (§3.1.1.3).
fn compress_block(
    input: &[u8],
    start: usize,
    end: usize,
    strategy: Strategy,
    work: &mut Work<'_>,
) -> Result<(), Corrupt> {
    // The literals gathered into a buffer kept at the longest length a block needed plus a
    // piece (at most `BLOCK_MAX + LITERAL_PIECE`): a run of up to a piece is one fixed-size copy
    // that may run past it into bytes the next run overwrites (`ZSTD_safecopyLiterals`'s wild
    // copy), where a call per run cost the gathering most of its time.
    let literals = &mut *work.literals;
    let room = end.saturating_sub(start).saturating_add(LITERAL_PIECE);
    if literals.len() < room {
        literals.resize(room, 0);
    }
    let mut n = 0usize;
    let mut at = start;
    let mut take = |at: usize, len: usize, n: usize| -> Result<(), Corrupt> {
        let piece = input
            .get(at..)
            .and_then(<[u8]>::first_chunk::<LITERAL_PIECE>);
        match piece {
            Some(piece) if len <= LITERAL_PIECE => {
                *literals
                    .get_mut(n..)
                    .and_then(<[u8]>::first_chunk_mut::<LITERAL_PIECE>)
                    .ok_or(Corrupt::Block)? = *piece;
            }
            _ => literals
                .get_mut(n..n.saturating_add(len))
                .ok_or(Corrupt::Block)?
                .copy_from_slice(
                    input
                        .get(at..at.saturating_add(len))
                        .ok_or(Corrupt::Block)?,
                ),
        }
        Ok(())
    };
    for f in work.found.iter() {
        let len = usize::try_from(f.literals).map_err(|_| Corrupt::Block)?;
        take(at, len, n)?;
        n = n.saturating_add(len);
        at = at
            .saturating_add(len)
            .saturating_add(usize::try_from(f.length).map_err(|_| Corrupt::Block)?);
    }
    let rest = end.checked_sub(at).ok_or(Corrupt::Block)?;
    take(at, rest, n)?;
    n = n.saturating_add(rest);
    let literals = work.literals.get(..n).ok_or(Corrupt::Block)?;
    literals_section(literals, strategy, work.block)?;
    sequences_section(work)?;
    Ok(())
}

/// The literals section (§3.1.1.3.1) onto `out`: RLE when every literal is one, Huffman-coded
/// when that saves bytes, raw otherwise.
fn literals_section(literals: &[u8], strategy: Strategy, out: &mut Vec<u8>) -> Result<(), Corrupt> {
    let n = literals.len();
    if let Some(&first) = literals.first()
        && n > 1
        && literals.iter().all(|&b| b == first)
    {
        raw_or_rle_header(1, n, out)?;
        out.push(first);
        return Ok(());
    }
    let start = out.len();
    if n >= min_literals(strategy) && matches!(huffman_literals(literals, strategy, out), Ok(true))
    {
        return Ok(());
    }
    out.truncate(start);
    raw_or_rle_header(0, n, out)?;
    out.extend_from_slice(literals);
    Ok(())
}

/// The reference's number of a strategy, `ZSTD_fast` 1 to `ZSTD_btultra2` 9.
fn strategy_number(strategy: Strategy) -> usize {
    match strategy {
        Strategy::Fast => 1,
        Strategy::DFast => 2,
        Strategy::Greedy => 3,
        Strategy::Lazy => 4,
        Strategy::Lazy2 => 5,
        Strategy::BtLazy2 => 6,
        Strategy::BtOpt => 7,
        Strategy::BtUltra => 8,
        Strategy::BtUltra2 => 9,
    }
}

/// Fewer literals than this are stored raw without a Huffman attempt
/// (`ZSTD_minLiteralsToCompress`, zstd 1.5.7 lib/compress/zstd_compress_literals.c, without a
/// previous table to repeat, which a block of RocksDB's never has): 8 bytes at `btultra2`,
/// twice as many for each strategy below it, at most 64.
fn min_literals(strategy: Strategy) -> usize {
    8usize << 9usize.saturating_sub(strategy_number(strategy)).min(3)
}

/// The least a coded literals section must save over raw to be kept (`ZSTD_minGain`, zstd
/// 1.5.7 lib/compress/zstd_compress_internal.h): `n >> 6` plus 2, the shift the strategy's
/// number less one from `btultra` (8) up.
fn min_gain(n: usize, strategy: Strategy) -> usize {
    let number = strategy_number(strategy);
    let shift = if number >= 8 {
        number.saturating_sub(1)
    } else {
        6
    };
    (n >> shift).saturating_add(2)
}

/// The length of a raw or RLE literals header for `size` literals (§3.1.1.3.1.1).
fn raw_header_len(size: usize) -> usize {
    if size < 32 {
        1
    } else if size < 4096 {
        2
    } else {
        3
    }
}

/// A raw or RLE literals header of the shortest size format onto `out` (§3.1.1.3.1.1).
fn raw_or_rle_header(kind: u8, size: usize, out: &mut Vec<u8>) -> Result<(), Corrupt> {
    let s = u32::try_from(size).map_err(|_| Corrupt::Literals)?;
    let kind = u32::from(kind);
    let (v, len) = match raw_header_len(size) {
        1 => (kind | (s << 3), 1),
        2 => (kind | (1 << 2) | (s << 4), 2),
        _ => (kind | (3 << 2) | (s << 4), 3),
    };
    out.extend_from_slice(v.to_le_bytes().get(..len).ok_or(Corrupt::Literals)?);
    Ok(())
}

/// Huffman-coded literals with their tree (§3.1.1.3.1.4) onto `out`: one stream below 256
/// literals, four above, as the reference chooses. The header's size format follows from the
/// literals' count alone, as the reference's `ZSTD_compressLiterals` sets it: a coded section
/// no shorter than the literals is not kept, so its size fits wherever theirs does. On an error
/// `out` may hold a partial section.
fn huffman_literals(
    literals: &[u8],
    strategy: Strategy,
    out: &mut Vec<u8>,
) -> Result<bool, Corrupt> {
    let n = literals.len();
    let mut counts = [0u32; LITERALS];
    for &b in literals {
        let c = counts.get_mut(usize::from(b)).ok_or(Corrupt::Huffman)?;
        *c = c.saturating_add(1);
    }
    // `HUF_compress_internal`'s heuristic: no byte value more frequent than `n / 128 + 4`
    // means the literals are near uniform, and a table would cost about what it saves.
    let largest = counts.iter().copied().max().unwrap_or(0);
    let largest = usize::try_from(largest).map_err(|_| Corrupt::Huffman)?;
    if largest <= (n >> 7).saturating_add(4) {
        return Ok(false);
    }
    let max_symbol = counts.iter().rposition(|&c| c > 0).unwrap_or(0);
    let mut lengths = [0u8; LITERALS];
    code_lengths(
        &counts,
        optimal_log(n, max_symbol, MAX_BITS, 1),
        &mut lengths,
    );
    let table = HuffmanEncoder::new(&lengths)?;
    let r = u32::try_from(literals.len()).map_err(|_| Corrupt::Literals)?;
    let four = literals.len() >= 256;
    // Size_Format (§3.1.1.3.1.1): one stream only with 10-bit sizes.
    let (format, header): (u64, usize) = if !four {
        (0, 3)
    } else if r < 1024 {
        (1, 3)
    } else if r < 16_384 {
        (2, 4)
    } else if r < 262_144 {
        (3, 5)
    } else {
        return Err(Corrupt::Literals);
    };
    let start = out.len();
    out.extend_from_slice([0; 5].get(..header).ok_or(Corrupt::Literals)?);
    let described = out.len();
    table.description(out)?;
    // A table whose description leaves under 12 bytes to save is not used (`HUF_compress_internal`).
    if out.len().saturating_sub(described).saturating_add(12) >= n {
        return Ok(false);
    }
    table.streams(literals, four, out)?;
    let c = u64::try_from(out.len().saturating_sub(start).saturating_sub(header))
        .map_err(|_| Corrupt::Literals)?;
    // Each size's bits, and where the compressed size starts after the 4 header bits.
    let (width, at) = match format {
        0 | 1 => (10, 14),
        2 => (14, 18),
        _ => (18, 22),
    };
    if c >= 1 << width {
        return Err(Corrupt::Literals);
    }
    // Kept only if it saves the strategy's least gain (`ZSTD_compressLiterals`).
    let kept_below = n.saturating_sub(min_gain(n, strategy));
    if usize::try_from(c).map_err(|_| Corrupt::Literals)? >= kept_below {
        return Ok(false);
    }
    let v = 2u64 | (format << 2) | (u64::from(r) << 4) | (c << at);
    out.get_mut(start..start.saturating_add(header))
        .ok_or(Corrupt::Literals)?
        .copy_from_slice(v.to_le_bytes().get(..header).ok_or(Corrupt::Literals)?);
    Ok(true)
}

/// One symbol type's table for the block: its mode (§3.1.1.3.2.1, Table 15) and its encoding
/// table, none in RLE mode, where its one state takes no bits to start, step or flush.
struct Chosen<'t> {
    mode: u8,
    table: Option<&'t EncodeTable>,
}

/// A state over a chosen table; none in RLE mode, where nothing is written.
struct Coder<'t> {
    table: Option<&'t EncodeTable>,
    state: Option<EncodeState>,
}

impl<'t> Coder<'t> {
    fn init(chosen: &Chosen<'t>, symbol: u8) -> Result<Self, Corrupt> {
        let table = chosen.table;
        let state = table.map(|t| EncodeState::init(t, symbol)).transpose()?;
        Ok(Self { table, state })
    }

    #[inline]
    fn encode(&mut self, symbol: u8, w: &mut Writer<'_>) -> Result<(), Corrupt> {
        if let (Some(t), Some(s)) = (self.table, self.state.as_mut()) {
            s.encode(t, symbol, w)?;
        }
        Ok(())
    }

    fn flush(self, w: &mut Writer<'_>) {
        if let (Some(t), Some(s)) = (self.table, self.state) {
            s.flush(t, w);
        }
    }
}

/// The cheapest of predefined, RLE and the block's own FSE table for a code's `counts` (one per
/// code, `total` in all), by the bits each spends; its description, if any, goes onto `out`.
/// `default` is the predefined distribution and `predefined` its encoding table, `own` the table
/// rebuilt for the block's own, and `log2` the cost table.
fn choose<'t>(
    counts: &[u32],
    total: usize,
    max_log: u32,
    (default, default_log, predefined): (&[i16], u32, &'t EncodeTable),
    own: &'t mut EncodeTable,
    log2: &[u16],
    out: &mut Vec<u8>,
) -> Result<Chosen<'t>, Corrupt> {
    let max_symbol = counts
        .iter()
        .rposition(|&c| c > 0)
        .ok_or(Corrupt::Sequences)?;
    let present = counts.iter().filter(|&&c| c > 0).count();
    if present == 1 {
        // One symbol: RLE (FSE_Compressed_Mode needs two, §3.1.1.3.2.1).
        out.push(u8::try_from(max_symbol).map_err(|_| Corrupt::Sequences)?);
        return Ok(Chosen {
            mode: 1,
            table: None,
        });
    }
    let total32 = u32::try_from(total).map_err(|_| Corrupt::Sequences)?;
    let log = table_log(total, max_symbol, max_log);
    let counts = counts.get(..=max_symbol).ok_or(Corrupt::Sequences)?;
    let mut norm = [0i16; SYMBOLS_MAX];
    let norm = normalize(counts, total32, log, &mut norm)?;
    let start = out.len();
    write_distribution(norm, log, out)?;
    let description = out.len().saturating_sub(start);
    // The table's own bits and its description's, both in 256ths of a bit.
    let own_bits = cost(counts, norm, log, log2).saturating_add(
        u64::try_from(description)
            .unwrap_or(0)
            .saturating_mul(8 * 256),
    );
    // The predefined table covers the codes only if every code present has a cell in it.
    let default_fits = counts
        .iter()
        .enumerate()
        .all(|(s, &c)| c == 0 || default.get(s).is_some_and(|&p| p != 0));
    if default_fits && cost(counts, default, default_log, log2) <= own_bits {
        out.truncate(start);
        return Ok(Chosen {
            mode: 0,
            table: Some(predefined),
        });
    }
    own.set(norm, log)?;
    Ok(Chosen {
        mode: 2,
        table: Some(own),
    })
}

/// `256·log2(x)` for `x ≥ 1`, rounded down: the integer part from the bit length, eight fraction
/// bits by repeated squaring of the mantissa (each square doubles the exponent; a square of 2 or
/// more is a one bit). Integer arithmetic, so every host chooses alike.
fn log2_256(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    let whole = u64::from(63u32.saturating_sub(x.leading_zeros()));
    // The mantissa in [1, 2) as a 1.32 fixed-point number.
    let shift = u32::try_from(whole).unwrap_or(0);
    let mut m: u128 = (u128::from(x) << 32) >> shift;
    let mut frac = 0u64;
    for _ in 0..8 {
        m = (m.saturating_mul(m)) >> 32;
        frac <<= 1;
        if m >= 2u128 << 32 {
            m >>= 1;
            frac |= 1;
        }
    }
    (whole << 8) | frac
}

/// The bits `counts` take under `norm` at `log`, in 256ths of a bit: each symbol
/// `log − log2(p)` bits a time, `p` its cells (a "less than 1" symbol one), absent from `norm` the
/// most a cost can be. `log2` holds `256·log2(p)` for every count of cells a table has.
fn cost(counts: &[u32], norm: &[i16], log: u32, log2: &[u16]) -> u64 {
    let mut total = 0u64;
    for (s, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let cells = match norm.get(s) {
            Some(&-1) => 1u64,
            Some(&n) if n > 0 => u64::from(n.unsigned_abs()),
            _ => return u64::MAX,
        };
        let cells = usize::try_from(cells).unwrap_or(usize::MAX);
        let each = (u64::from(log) << 8)
            .saturating_sub(u64::from(log2.get(cells).copied().unwrap_or(u16::MAX)));
        total = total.saturating_add(u64::from(c).saturating_mul(each));
    }
    total
}

/// The sequences section (§3.1.1.3.2) onto the block: the count, the modes, the
/// descriptions, and the interleaved bitstream, written last sequence first.
fn sequences_section(work: &mut Work<'_>) -> Result<(), Corrupt> {
    let found = &*work.found;
    let out = &mut *work.block;
    let n = found.len();
    match n {
        0 => {
            out.push(0);
            return Ok(());
        }
        1..=127 => out.push(u8::try_from(n).map_err(|_| Corrupt::Sequences)?),
        128..=0x7EFF => {
            out.push(u8::try_from((n >> 8) | 0x80).map_err(|_| Corrupt::Sequences)?);
            out.push(u8::try_from(n & 0xFF).map_err(|_| Corrupt::Sequences)?);
        }
        _ => {
            let rest = n.checked_sub(0x7F00).ok_or(Corrupt::Sequences)?;
            out.push(255);
            out.push(u8::try_from(rest & 0xFF).map_err(|_| Corrupt::Sequences)?);
            out.push(u8::try_from(rest >> 8).map_err(|_| Corrupt::Sequences)?);
        }
    }
    // Each sequence's codes, and each code's count, in one pass.
    let codes = &mut *work.codes;
    codes.clear();
    let mut ll_counts = [0u32; LITERALS_LENGTH_CODES.len()];
    let mut ml_counts = [0u32; MATCH_LENGTH_CODES.len()];
    let mut of_counts = [0u32; 32];
    for f in found {
        let c = [
            work.lengths.literals(f.literals),
            work.lengths.matches(f.length),
            offset_code(f.offset_value),
        ];
        for (counts, &(code, _, _)) in [&mut ll_counts[..], &mut ml_counts[..], &mut of_counts[..]]
            .into_iter()
            .zip(&c)
        {
            let slot = counts
                .get_mut(usize::from(code))
                .ok_or(Corrupt::Sequences)?;
            *slot = slot.saturating_add(1);
        }
        codes.push(c);
    }
    let modes_at = out.len();
    out.push(0);
    let [ll_own, of_own, ml_own] = &mut *work.own;
    let predefined = work.predefined;
    let ll_t = choose(
        &ll_counts,
        n,
        LITERALS_LENGTH_MAX_LOG,
        (
            &LITERALS_LENGTH_DEFAULT,
            LITERALS_LENGTH_DEFAULT_LOG,
            &predefined.literals_length,
        ),
        ll_own,
        work.log2,
        out,
    )?;
    // Offset codes above the predefined table's N cannot use it; `choose` sees that as no cell.
    let of_max = of_counts.iter().rposition(|&c| c > 0).unwrap_or(0);
    let of_default: &[i16] = if of_max > usize::from(OFFSET_DEFAULT_MAX_CODE) {
        &[]
    } else {
        &OFFSET_DEFAULT
    };
    let of_t = choose(
        &of_counts,
        n,
        OFFSET_MAX_LOG,
        (of_default, OFFSET_DEFAULT_LOG, &predefined.offset),
        of_own,
        work.log2,
        out,
    )?;
    let ml_t = choose(
        &ml_counts,
        n,
        MATCH_LENGTH_MAX_LOG,
        (
            &MATCH_LENGTH_DEFAULT,
            MATCH_LENGTH_DEFAULT_LOG,
            &predefined.match_length,
        ),
        ml_own,
        work.log2,
        out,
    )?;
    *out.get_mut(modes_at).ok_or(Corrupt::Sequences)? =
        (ll_t.mode << 6) | (of_t.mode << 4) | (ml_t.mode << 2);
    // The bitstream, the reference's ZSTD_encodeSequences: the last sequence's states and bits
    // first, then each earlier one, then the states flushed, read back in the decoder's order.
    // Each sequence's bits at most: three states of at most 9, 9 and 8 bits, then extra bits of
    // at most 16, 16 and 31.
    let mut w = Writer::new(out, n.saturating_mul(26 + 16 + 16 + 31));
    let [l_ll, l_ml, l_of] = *codes.last().ok_or(Corrupt::Sequences)?;
    let mut s_ml = Coder::init(&ml_t, l_ml.0)?;
    let mut s_of = Coder::init(&of_t, l_of.0)?;
    let mut s_ll = Coder::init(&ll_t, l_ll.0)?;
    w.add(u64::from(l_ll.1), u32::from(l_ll.2));
    w.add(u64::from(l_ml.1), u32::from(l_ml.2));
    w.add(u64::from(l_of.1), u32::from(l_of.2));
    w.flush();
    // `ZSTD_encodeSequences`' flushes for a 64-bit word: at most 7 bits held after a flush, 26
    // more for the states, a flush before the extra bits unless all three fit with them, another
    // before the offset's unless the lengths' and offset's fit after the states.
    for &[c_ll, c_ml, c_of] in codes.iter().rev().skip(1) {
        s_of.encode(c_of.0, &mut w)?;
        s_ml.encode(c_ml.0, &mut w)?;
        s_ll.encode(c_ll.0, &mut w)?;
        // Each at most 31, so the sum is far below u32's range.
        let extra = u32::from(c_ll.2)
            .wrapping_add(u32::from(c_ml.2))
            .wrapping_add(u32::from(c_of.2));
        if extra >= 64 - 7 - 26 {
            w.flush();
        }
        w.add_held(u64::from(c_ll.1), u32::from(c_ll.2));
        w.add_held(u64::from(c_ml.1), u32::from(c_ml.2));
        if extra > 56 {
            w.flush();
        }
        w.add_held(u64::from(c_of.1), u32::from(c_of.2));
        w.flush();
    }
    s_ml.flush(&mut w);
    s_of.flush(&mut w);
    s_ll.flush(&mut w);
    w.close();
    Ok(())
}

/// What one [`Encoder::compress`] call wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// Bytes written to the output.
    pub output_len: usize,
    /// Bytes of the frame still to hand out; 0 once the frame is out.
    pub remaining: usize,
}

/// A frame at a time, handed out in pieces of the caller's size: RocksDB's
/// `ZSTDStreamingCompress`, which compresses one record per frame (`ZSTD_e_end`) and calls again
/// with the same record until nothing remains [R util/compression.cc:190-231].
#[derive(Debug)]
pub struct Encoder {
    level: Level,
    checksum: bool,
    compressor: Option<Compressor>,
    frame: Vec<u8>,
    /// Bytes of `frame` handed out.
    at: usize,
    /// Whether `frame` is the current record's, still being handed out.
    active: bool,
}

impl Encoder {
    /// An encoder at `level`, each frame with the content checksum when `checksum`.
    pub fn new(level: Level, checksum: bool) -> Self {
        Self {
            level,
            checksum,
            compressor: None,
            frame: Vec::new(),
            at: 0,
            active: false,
        }
    }

    /// Continues `input`'s frame into `output`: the first call for an input compresses it, each
    /// call hands out what `output` holds. An empty input writes nothing.
    pub fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<Written, Error> {
        if input.is_empty() {
            return Ok(Written {
                output_len: 0,
                remaining: 0,
            });
        }
        if !self.active {
            let compressor = match &mut self.compressor {
                Some(c) => c,
                None => self.compressor.insert(Compressor::new()?),
            };
            self.frame.clear();
            compressor.compress_into(input, self.level, self.checksum, &mut self.frame)?;
            self.at = 0;
            self.active = true;
        }
        let rest = self.frame.get(self.at..).unwrap_or_default();
        let n = rest.len().min(output.len());
        if let (Some(dst), Some(src)) = (output.get_mut(..n), rest.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.at = self.at.saturating_add(n);
        let remaining = self.frame.len().saturating_sub(self.at);
        if remaining == 0 {
            self.active = false;
        }
        Ok(Written {
            output_len: n,
            remaining,
        })
    }

    /// Ends the frame being handed out, so the next input starts a new one.
    pub fn reset(&mut self) {
        self.active = false;
        self.at = 0;
        self.frame.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::sequences::{self, Sequence};
    use super::*;

    /// A frame whose positions would pass u32 from the base starts the tables over, and still
    /// matches only within itself.
    #[test]
    fn the_tables_start_over_before_positions_pass_u32() {
        let mut c = Compressor::new().unwrap();
        let input: Vec<u8> = b"abcdefgh-abcdefgh-abcdefgh-0123456789-0123456789".repeat(40);
        let mut first = Vec::new();
        c.compress_into(&input, Level::new(3), false, &mut first)
            .unwrap();
        c.base = u32::MAX - 100;
        let mut second = Vec::new();
        c.compress_into(&input, Level::new(3), false, &mut second)
            .unwrap();
        assert_eq!(c.base, 1 + u32::try_from(input.len()).unwrap());
        assert_eq!(first, second);
    }

    proptest::proptest! {
        /// Sequences written by the encoder's section read back as written, the extra bits of
        /// one sequence reaching 31 + 16 + 16: offsets of every code and lengths past 2^16, so
        /// the decoder's reads past one refill are exercised (§3.1.1.3.2.1).
        #[test]
        fn sequences_read_back_as_written(
            sequences in proptest::collection::vec(
                (0u32..=131_071, 3u32..=131_074, 0u32..=31, proptest::prelude::any::<u32>()),
                1..300,
            ),
        ) {
            let mut c = Compressor::new().unwrap();
            c.found.clear();
            c.found.extend(sequences.iter().map(|&(literals, length, bits, raw)| {
                // An offset value of code `bits`: its top bit and `bits` extra bits below it;
                // code 0 is the value 1, a repeat offset.
                let offset_value = (1u32 << bits) | (raw & ((1u32 << bits) - 1));
                Found { literals, offset_value, length }
            }));
            let expected: Vec<Sequence> = c
                .found
                .iter()
                .map(|f| Sequence {
                    literals: f.literals,
                    offset_value: f.offset_value,
                    match_length: f.length,
                })
                .collect();
            let mut section = Vec::new();
            let mut work = Work {
                found: &mut c.found,
                literals: &mut c.literals,
                block: &mut section,
                codes: &mut c.codes,
                own: &mut c.own,
                predefined: &c.predefined,
                log2: &c.log2,
                lengths: &c.lengths,
            };
            sequences_section(&mut work).unwrap();
            let mut tables = sequences::Tables::default();
            let predefined = sequences::Predefined::new().unwrap();
            let mut got = Vec::new();
            sequences::decode(&section, &mut tables, &predefined, |seq| {
                got.push(seq);
                Ok(())
            })
            .unwrap();
            proptest::prop_assert_eq!(got, expected);
        }
    }

    #[test]
    fn length_codes_without_a_search_equal_the_search() {
        let codes = LengthCodes::new();
        for len in 0..=(1u32 << 17) {
            assert_eq!(
                codes.literals(len),
                literals_length_code(len),
                "literals {len}"
            );
        }
        for len in 3..=(1u32 << 17) {
            assert_eq!(codes.matches(len), match_length_code(len), "match {len}");
        }
    }
}
