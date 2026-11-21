//! Snappy, the engine's own: RocksDB's `kSnappyCompression` block format (`util/compression.h`),
//! which is Snappy's raw format as Google's `format_description.txt` (snappy 1.2, in the
//! library's source) defines it.
//!
//! A block is the uncompressed length as a little-endian base-128 varint of at most 32 bits, then
//! elements, each led by a tag byte whose low two bits name it:
//! - `00`, a literal: its length less one in the tag's upper six bits when below 60; 60 to 63 name
//!   one to four bytes after the tag that hold it, little-endian; the literal's bytes follow;
//! - `01`, a copy of 4 to 11 bytes at an 11-bit offset: the length less four in bits 2 to 4, the
//!   offset's high three bits in bits 5 to 7 and its low eight in the next byte;
//! - `10`, a copy of 1 to 64 bytes at a 16-bit offset, little-endian after the tag;
//! - `11`, a copy of 1 to 64 bytes at a 32-bit offset, likewise.
//!
//! A copy's offset counts back from the end of what was produced; it is never zero and never
//! past the start, and a copy longer than its offset repeats what it copies. The elements produce
//! exactly the stated length.
//!
//! The decoder trusts nothing the bytes say: the stated length is checked against the caller's
//! bound before anything is reserved, every offset and length against what was produced and is
//! left, and the input must end where the last element does. The encoder finds matches as the
//! reference does, in fragments of 64 KiB so that every offset fits two bytes, with a table of
//! the positions of 4-byte sequences; its bytes need not be the reference's, only decode to its
//! input under any decoder of the format (`tests/snappy_test.rs`, and the reference's decoder of
//! them, engine.md §5).

use crate::error::{Error, Malformed};

/// What the errors name.
const WHAT: &str = "a snappy block";

/// The fragment the encoder matches within: the reference's `kBlockSize`, 64 KiB, so that an
/// offset is below 2^16 and a copy takes at most three bytes.
const FRAGMENT: usize = 1 << 16;

/// The largest table of positions the encoder keeps: the reference's `kMaxHashTableSize`, 2^14
/// entries of two bytes, a fragment's positions hashed into them.
const TABLE_MAX: usize = 1 << 14;

/// The smallest: the reference's `kMinHashTableSize`, 2^8.
const TABLE_MIN: usize = 1 << 8;

/// The multiplier of the encoder's hash of four bytes, the reference's (`snappy.cc` `HashBytes`).
const HASH_MUL: u32 = 0x1e35_a7bd;

/// Fails a decode with `why`.
fn bad(why: Malformed) -> Error {
    Error::corruption(WHAT, why)
}

/// The uncompressed length a block states, and the bytes after it.
pub fn decompressed_len(block: &[u8]) -> Result<(usize, &[u8]), Error> {
    let mut value: u64 = 0;
    for (i, (&byte, shift)) in block.iter().zip((0u32..35).step_by(7)).enumerate() {
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            let len = u32::try_from(value).map_err(|_| bad(Malformed::VarintOverflow))?;
            let after = i.checked_add(1).ok_or_else(|| bad(Malformed::Truncated))?;
            let rest = block
                .get(after..)
                .ok_or_else(|| bad(Malformed::Truncated))?;
            let len = usize::try_from(len).map_err(|_| bad(Malformed::TooLarge))?;
            return Ok((len, rest));
        }
    }
    Err(bad(if block.len() < 5 {
        Malformed::Truncated
    } else {
        Malformed::VarintTooLong
    }))
}

/// `block` decompressed, refused when it states more than `max` bytes.
pub fn decompress(block: &[u8], max: usize) -> Result<Vec<u8>, Error> {
    let (len, mut input) = decompressed_len(block)?;
    if len > max {
        return Err(Error::LimitExceeded {
            what: "a snappy block's stated length",
            limit: u64::try_from(max).unwrap_or(u64::MAX),
        });
    }
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| Error::LimitExceeded {
            what: "a snappy block's output",
            limit: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
    while let Some((&tag, rest)) = input.split_first() {
        input = rest;
        let (n, offset) = match tag & 0b11 {
            0 => {
                let n = literal_len(tag, &mut input)?;
                if out.len().checked_add(n).is_none_or(|end| end > len) {
                    return Err(bad(Malformed::CountMismatch));
                }
                let (literal, rest) = input
                    .split_at_checked(n)
                    .ok_or_else(|| bad(Malformed::Truncated))?;
                input = rest;
                out.extend_from_slice(literal);
                continue;
            }
            1 => {
                let (&low, rest) = input
                    .split_first()
                    .ok_or_else(|| bad(Malformed::Truncated))?;
                input = rest;
                let n = usize::from((tag >> 2) & 0b111).saturating_add(4);
                let offset = usize::from(u16::from_le_bytes([low, tag >> 5]));
                (n, offset)
            }
            2 => {
                let (bytes, rest) = input
                    .split_at_checked(2)
                    .ok_or_else(|| bad(Malformed::Truncated))?;
                input = rest;
                let mut le = [0u8; 2];
                le.copy_from_slice(bytes);
                (
                    usize::from(tag >> 2).saturating_add(1),
                    usize::from(u16::from_le_bytes(le)),
                )
            }
            _ => {
                let (bytes, rest) = input
                    .split_at_checked(4)
                    .ok_or_else(|| bad(Malformed::Truncated))?;
                input = rest;
                let mut le = [0u8; 4];
                le.copy_from_slice(bytes);
                let offset = usize::try_from(u32::from_le_bytes(le))
                    .map_err(|_| bad(Malformed::TooLarge))?;
                (usize::from(tag >> 2).saturating_add(1), offset)
            }
        };
        copy(&mut out, offset, n, len)?;
    }
    if out.len() != len {
        return Err(bad(Malformed::CountMismatch));
    }
    Ok(out)
}

/// A literal's length from its tag, and from the one to four bytes after it that tags 60 to 63
/// name, which are taken from `input`.
fn literal_len(tag: u8, input: &mut &[u8]) -> Result<usize, Error> {
    let head = tag >> 2;
    let less_one = if head < 60 {
        u32::from(head)
    } else {
        let bytes = usize::from(head.saturating_sub(59));
        let (len_bytes, rest) = input
            .split_at_checked(bytes)
            .ok_or_else(|| bad(Malformed::Truncated))?;
        *input = rest;
        let mut le = [0u8; 4];
        le.get_mut(..bytes)
            .ok_or_else(|| bad(Malformed::Undecodable))?
            .copy_from_slice(len_bytes);
        u32::from_le_bytes(le)
    };
    usize::try_from(less_one)
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| bad(Malformed::TooLarge))
}

/// Appends `len` bytes copied from `offset` back from the end of `out`, repeating them where the
/// copy is longer than its offset; refused past the start, at offset zero, or past the stated
/// length `total`.
fn copy(out: &mut Vec<u8>, offset: usize, len: usize, total: usize) -> Result<(), Error> {
    let start = out
        .len()
        .checked_sub(offset)
        .filter(|_| offset != 0)
        .ok_or_else(|| bad(Malformed::Undecodable))?;
    if out.len().checked_add(len).is_none_or(|end| end > total) {
        return Err(bad(Malformed::CountMismatch));
    }
    let end = start
        .checked_add(len)
        .ok_or_else(|| bad(Malformed::TooLarge))?;
    if offset >= len {
        out.extend_from_within(start..end);
    } else {
        // Longer than its offset: each byte copied was produced before it, the copy repeating.
        for at in start..end {
            let byte = *out.get(at).ok_or_else(|| bad(Malformed::Undecodable))?;
            out.push(byte);
        }
    }
    Ok(())
}

/// The most bytes [`compress`] writes for `len` input bytes: the reference's
/// `MaxCompressedLength`, `32 + len + len / 6`.
pub fn max_compressed_len(len: usize) -> Option<usize> {
    len.checked_add(len / 6)?.checked_add(32)
}

/// `input` compressed into one block.
pub fn compress(input: &[u8]) -> Result<Vec<u8>, Error> {
    let len = u32::try_from(input.len()).map_err(|_| Error::InvalidArgument {
        what: "a snappy block of 2^32 bytes or more",
    })?;
    let bound = max_compressed_len(input.len()).ok_or(Error::InvalidArgument {
        what: "a snappy block too large to bound",
    })?;
    let mut out = Vec::new();
    out.try_reserve_exact(bound)
        .map_err(|_| Error::LimitExceeded {
            what: "a snappy block's compressed bound",
            limit: u64::try_from(bound).unwrap_or(u64::MAX),
        })?;
    let mut value = len;
    loop {
        let byte = value.to_le_bytes()[0] & 0x7f;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    let mut table = vec![0u16; TABLE_MAX];
    for fragment in input.chunks(FRAGMENT) {
        compress_fragment(fragment, &mut out, &mut table)?;
    }
    Ok(out)
}

/// The table size for a fragment of `len` bytes: the least power of two at least `len`, within
/// the table's bounds (the reference's `CalculateTableSize`).
fn table_size(len: usize) -> usize {
    len.next_power_of_two().clamp(TABLE_MIN, TABLE_MAX)
}

/// The four bytes at `at`, little-endian, where there are four.
fn load32(src: &[u8], at: usize) -> Option<u32> {
    let bytes = src.get(at..at.checked_add(4)?)?;
    let mut le = [0u8; 4];
    le.copy_from_slice(bytes);
    Some(u32::from_le_bytes(le))
}

fn compress_fragment(src: &[u8], out: &mut Vec<u8>, table: &mut [u16]) -> Result<(), Error> {
    let internal = |what| Error::InvalidArgument { what };
    let size = table_size(src.len());
    let table = table
        .get_mut(..size)
        .ok_or_else(|| internal("a snappy table smaller than its fragment's"))?;
    table.fill(0);
    let shift = 32u32.saturating_sub(size.trailing_zeros());
    let hash = |word: u32| usize::try_from(word.wrapping_mul(HASH_MUL) >> shift).unwrap_or(0);
    let mut next_emit = 0usize;
    let mut ip = 0usize;
    // The last position a match may begin at: four bytes must follow it.
    let limit = src.len().saturating_sub(4);
    'search: while ip < limit {
        // The reference's skip: one byte at a time at first, then faster through bytes that do
        // not match, one more byte a step for every 32 misses.
        let mut skip = 32u32;
        let (candidate, at) = loop {
            let word = load32(src, ip).ok_or_else(|| internal("a snappy search past its end"))?;
            let slot = table
                .get_mut(hash(word))
                .ok_or_else(|| internal("a snappy hash past its table"))?;
            let candidate = usize::from(*slot);
            *slot = u16::try_from(ip).map_err(|_| internal("a position past a snappy fragment"))?;
            if candidate < ip && load32(src, candidate) == Some(word) {
                break (candidate, ip);
            }
            ip = ip.saturating_add(usize::try_from(skip >> 5).unwrap_or(usize::MAX));
            skip = skip.saturating_add(1);
            if ip >= limit {
                break 'search;
            }
        };
        emit_literal(out, src.get(next_emit..at).unwrap_or(&[]))?;
        // Four bytes match; extend while the next do. Both ends stay inside the fragment.
        let mut len = 4usize;
        while src.get(at.saturating_add(len)).is_some()
            && src.get(candidate.saturating_add(len)) == src.get(at.saturating_add(len))
        {
            len = len.saturating_add(1);
        }
        emit_copy(out, at.saturating_sub(candidate), len)?;
        ip = at.saturating_add(len);
        next_emit = ip;
    }
    emit_literal(out, src.get(next_emit..).unwrap_or(&[]))
}

/// A literal of `bytes`, which may be empty (then nothing is written). A fragment's literal is
/// below 2^16 bytes, so its length takes at most two bytes after the tag.
fn emit_literal(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    let Some(n) = bytes.len().checked_sub(1) else {
        return Ok(());
    };
    let unfit = || Error::InvalidArgument {
        what: "a snappy literal of 2^32 bytes or more",
    };
    let n = u32::try_from(n).map_err(|_| unfit())?;
    if n < 60 {
        out.push(n.to_le_bytes()[0] << 2);
    } else {
        // The bytes the length takes, one to four; the tag names them as 60 to 63.
        let count = 4u32.saturating_sub(n.leading_zeros() / 8);
        let tag = u8::try_from(count.saturating_add(59)).map_err(|_| unfit())?;
        out.push(tag << 2);
        let le = n.to_le_bytes();
        let count = usize::try_from(count).map_err(|_| unfit())?;
        out.extend_from_slice(le.get(..count).ok_or_else(unfit)?);
    }
    out.extend_from_slice(bytes);
    Ok(())
}

/// Copies of `len` bytes at `offset` (below 2^16), as the reference emits them: pieces of 64
/// while more than 67 remain, one of 60 if more than 64 remain, then the rest, which is at least
/// four.
fn emit_copy(out: &mut Vec<u8>, offset: usize, mut len: usize) -> Result<(), Error> {
    while len >= 68 {
        emit_copy_piece(out, offset, 64)?;
        len = len.saturating_sub(64);
    }
    if len > 64 {
        emit_copy_piece(out, offset, 60)?;
        len = len.saturating_sub(60);
    }
    emit_copy_piece(out, offset, len)
}

/// One copy element of 4 to 64 bytes at an offset below 2^16: one byte of offset where the
/// copy is under 12 bytes and the offset under 2^11, two otherwise.
fn emit_copy_piece(out: &mut Vec<u8>, offset: usize, len: usize) -> Result<(), Error> {
    let unfit = || Error::InvalidArgument {
        what: "a snappy copy outside 4 to 64 bytes or past its fragment",
    };
    let offset = u16::try_from(offset).map_err(|_| unfit())?;
    let len = u8::try_from(len).map_err(|_| unfit())?;
    if !(4..=64).contains(&len) {
        return Err(unfit());
    }
    let [low, high] = offset.to_le_bytes();
    if len < 12 && offset < 2048 {
        out.push(0b01 | (len.saturating_sub(4) << 2) | (high << 5));
        out.push(low);
    } else {
        out.push(0b10 | (len.saturating_sub(1) << 2));
        out.extend_from_slice(&[low, high]);
    }
    Ok(())
}
