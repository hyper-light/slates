//! LZ4, the engine's own: the raw block RocksDB's `kLZ4Compression` and `kLZ4HCCompression` hold
//! (`util/compression.cc`: `LZ4_compress_fast_continue`, `LZ4_decompress_safe_continue`), as
//! LZ4's `lz4_Block_format.md` (lz4 1.10) defines it. The uncompressed size is stored beside the
//! block by the block layer, not in it.
//!
//! A block is sequences. Each begins with a token: the literals' length in its high four bits and
//! the match's length less four in its low four. Fifteen in either means more: bytes follow,
//! each added, until one below 255. The literals follow the token's literal bytes; then the
//! match's offset, two bytes little-endian, counting back from the end of what was produced;
//! then the match's further length bytes. The last sequence has literals only. A match is at
//! least four bytes, its offset never zero; the last five bytes of a block are literals, and no
//! match begins within its last twelve.
//!
//! A dictionary is history before the block: an offset may reach back into it
//! (`LZ4_setStreamDecode`, `LZ4_loadDict`). The decoder is given the size the block layer stored
//! and refuses anything that would pass it, read past the input, or reach before the dictionary.
//! The encoder is LZ4's fast compressor at acceleration one: a table of the positions of 4-byte
//! sequences, the skip that speeds through bytes that do not match.

use crate::error::{Error, Malformed};

/// What the errors name.
const WHAT: &str = "an LZ4 block";

/// A match's least length: lz4's `MINMATCH`.
const MIN_MATCH: usize = 4;

/// The literals every block ends with: lz4's `LASTLITERALS`.
const LAST_LITERALS: usize = 5;

/// How far before the end a match may still begin: lz4's `MFLIMIT`.
const MATCH_LIMIT: usize = 12;

/// The farthest an offset reaches: lz4's `LZ4_DISTANCE_MAX`, 65,535, two bytes.
const DISTANCE_MAX: usize = 65_535;

/// The encoder's table: 2^12 positions, lz4's default `LZ4_MEMORY_USAGE` of 14 (16 KiB of
/// four-byte entries).
const HASH_LOG: u32 = 12;

/// The shift that leaves a hash's top `HASH_LOG` bits.
const HASH_SHIFT: u32 = 32 - HASH_LOG;

/// The multiplier of lz4's hash of four bytes (`LZ4_hash4`: 2654435761, Knuth's).
const HASH_MUL: u32 = 2_654_435_761;

/// Misses before the encoder's step grows by one: lz4's `LZ4_skipTrigger`, 6.
const SKIP_TRIGGER: u32 = 6;

fn bad(why: Malformed) -> Error {
    Error::corruption(WHAT, why)
}

/// A length's further bytes, each added to `base`, until one below 255.
fn more_len(input: &mut &[u8], base: usize) -> Result<usize, Error> {
    let mut len = base;
    loop {
        let (&byte, rest) = input
            .split_first()
            .ok_or_else(|| bad(Malformed::Truncated))?;
        *input = rest;
        len = len
            .checked_add(usize::from(byte))
            .ok_or_else(|| bad(Malformed::TooLarge))?;
        if byte != 255 {
            return Ok(len);
        }
    }
}

/// `block` decompressed after `dict`, to exactly `len` bytes.
pub fn decompress(block: &[u8], dict: &[u8], len: usize) -> Result<Vec<u8>, Error> {
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| Error::LimitExceeded {
            what: "an LZ4 block's output",
            limit: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
    let mut input = block;
    loop {
        let (&token, rest) = input
            .split_first()
            .ok_or_else(|| bad(Malformed::Truncated))?;
        input = rest;
        let mut literals = usize::from(token >> 4);
        if literals == 15 {
            literals = more_len(&mut input, literals)?;
        }
        if out.len().checked_add(literals).is_none_or(|end| end > len) {
            return Err(bad(Malformed::CountMismatch));
        }
        let (bytes, rest) = input
            .split_at_checked(literals)
            .ok_or_else(|| bad(Malformed::Truncated))?;
        input = rest;
        out.extend_from_slice(bytes);
        if input.is_empty() {
            // The last sequence: literals only, and the block must end at the stated size.
            break;
        }
        let (offset, rest) = input
            .split_at_checked(2)
            .ok_or_else(|| bad(Malformed::Truncated))?;
        input = rest;
        let mut le = [0u8; 2];
        le.copy_from_slice(offset);
        let offset = usize::from(u16::from_le_bytes(le));
        let mut matched = usize::from(token & 0x0f);
        if matched == 15 {
            matched = more_len(&mut input, matched)?;
        }
        let matched = matched
            .checked_add(MIN_MATCH)
            .ok_or_else(|| bad(Malformed::TooLarge))?;
        copy(&mut out, dict, offset, matched, len)?;
    }
    if out.len() != len {
        return Err(bad(Malformed::CountMismatch));
    }
    Ok(out)
}

/// Appends `len` bytes copied from `offset` back from the end of the history (the dictionary,
/// then `out`), repeating them where the copy is longer than its offset.
fn copy(
    out: &mut Vec<u8>,
    dict: &[u8],
    offset: usize,
    len: usize,
    total: usize,
) -> Result<(), Error> {
    if offset == 0 {
        return Err(bad(Malformed::Undecodable));
    }
    if out.len().checked_add(len).is_none_or(|end| end > total) {
        return Err(bad(Malformed::CountMismatch));
    }
    // The match's first byte, as a position in the history: the dictionary, then the output.
    let history = dict
        .len()
        .checked_add(out.len())
        .ok_or_else(|| bad(Malformed::TooLarge))?;
    let start = history
        .checked_sub(offset)
        .ok_or_else(|| bad(Malformed::Undecodable))?;
    for at in start
        ..start
            .checked_add(len)
            .ok_or_else(|| bad(Malformed::TooLarge))?
    {
        let byte = match at.checked_sub(dict.len()) {
            Some(in_out) => *out.get(in_out).ok_or_else(|| bad(Malformed::Undecodable))?,
            None => *dict.get(at).ok_or_else(|| bad(Malformed::Undecodable))?,
        };
        out.push(byte);
    }
    Ok(())
}

/// The most bytes [`compress`] writes for `len` input bytes: lz4's `LZ4_compressBound`,
/// `len + len / 255 + 16`.
pub fn compress_bound(len: usize) -> Option<usize> {
    len.checked_add(len / 255)?.checked_add(16)
}

/// The four bytes at `at` of `buf`, little-endian.
fn load32(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at.checked_add(4)?)?;
    let mut le = [0u8; 4];
    le.copy_from_slice(bytes);
    Some(u32::from_le_bytes(le))
}

/// `input` compressed after `dict` (the history its matches may reach into), one block.
pub fn compress(input: &[u8], dict: &[u8]) -> Result<Vec<u8>, Error> {
    let internal = |what| Error::InvalidArgument { what };
    let bound =
        compress_bound(input.len()).ok_or_else(|| internal("an LZ4 block too large to bound"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(bound)
        .map_err(|_| Error::LimitExceeded {
            what: "an LZ4 block's compressed bound",
            limit: u64::try_from(bound).unwrap_or(u64::MAX),
        })?;
    // The dictionary's last 64 KiB and the input as one history; positions are into it.
    let dict = dict
        .get(dict.len().saturating_sub(DISTANCE_MAX)..)
        .unwrap_or(&[]);
    let mut buf = Vec::new();
    buf.try_reserve_exact(dict.len().saturating_add(input.len()))
        .map_err(|_| internal("an LZ4 history too large"))?;
    buf.extend_from_slice(dict);
    buf.extend_from_slice(input);
    let base = dict.len();
    let end = buf.len();
    let mut table = vec![0u32; 1 << HASH_LOG];
    let hash = |word: u32| usize::try_from(word.wrapping_mul(HASH_MUL) >> HASH_SHIFT).unwrap_or(0);
    let position =
        |at: usize| u32::try_from(at).map_err(|_| internal("an LZ4 history of 2^32 bytes or more"));
    // The dictionary's positions are known before the input's first.
    let mut at = 0usize;
    while at.saturating_add(MIN_MATCH) <= base {
        if let Some(word) = load32(&buf, at) {
            *table
                .get_mut(hash(word))
                .ok_or_else(|| internal("an LZ4 hash past its table"))? = position(at)?;
        }
        at = at.saturating_add(1);
    }
    let mut anchor = base;
    let mut ip = base;
    // No match begins within the last MATCH_LIMIT bytes; an input that short is literals.
    let limit = end.saturating_sub(MATCH_LIMIT);
    if input.len() >= MATCH_LIMIT {
        'search: while ip < limit {
            let mut misses = 1u32 << SKIP_TRIGGER;
            let candidate = loop {
                let word =
                    load32(&buf, ip).ok_or_else(|| internal("an LZ4 search past its end"))?;
                let slot = table
                    .get_mut(hash(word))
                    .ok_or_else(|| internal("an LZ4 hash past its table"))?;
                let candidate = usize::try_from(*slot).unwrap_or(usize::MAX);
                *slot = position(ip)?;
                let reaches = ip.saturating_sub(candidate) <= DISTANCE_MAX && candidate < ip;
                if reaches && load32(&buf, candidate) == Some(word) {
                    break candidate;
                }
                ip = ip.saturating_add(usize::try_from(misses >> SKIP_TRIGGER).unwrap_or(1));
                misses = misses.saturating_add(1);
                if ip >= limit {
                    break 'search;
                }
            };
            // Extend while the next bytes match, short of the last literals.
            let last = end.saturating_sub(LAST_LITERALS);
            let mut len = MIN_MATCH;
            while ip.saturating_add(len) < last
                && buf.get(candidate.saturating_add(len)) == buf.get(ip.saturating_add(len))
            {
                len = len.saturating_add(1);
            }
            emit_sequence(
                &mut out,
                buf.get(anchor..ip).unwrap_or(&[]),
                Some((ip.saturating_sub(candidate), len)),
            )?;
            ip = ip.saturating_add(len);
            anchor = ip;
        }
    }
    emit_sequence(&mut out, buf.get(anchor..end).unwrap_or(&[]), None)?;
    Ok(out)
}

/// A length's further bytes past fifteen: 255s, then the rest below 255.
fn put_more(out: &mut Vec<u8>, mut more: usize) {
    while more >= 255 {
        out.push(255);
        more = more.saturating_sub(255);
    }
    out.push(u8::try_from(more).unwrap_or(254));
}

/// One sequence: `literals`, then the match at `(offset, length)` where there is one.
fn emit_sequence(
    out: &mut Vec<u8>,
    literals: &[u8],
    matched: Option<(usize, usize)>,
) -> Result<(), Error> {
    let unfit = || Error::InvalidArgument {
        what: "an LZ4 match past two bytes of offset or below four bytes",
    };
    let lit_nibble = literals.len().min(15);
    let match_more = match matched {
        Some((_, len)) => len.checked_sub(MIN_MATCH).ok_or_else(unfit)?,
        None => 0,
    };
    let match_nibble = match_more.min(15);
    let token = u8::try_from((lit_nibble << 4) | match_nibble).map_err(|_| unfit())?;
    out.push(token);
    if lit_nibble == 15 {
        put_more(out, literals.len().saturating_sub(15));
    }
    out.extend_from_slice(literals);
    if let Some((offset, _)) = matched {
        let offset = u16::try_from(offset)
            .ok()
            .filter(|o| *o != 0)
            .ok_or_else(unfit)?;
        out.extend_from_slice(&offset.to_le_bytes());
        if match_nibble == 15 {
            put_more(out, match_more.saturating_sub(15));
        }
    }
    Ok(())
}
