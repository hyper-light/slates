//! Prefix varints: RocksDB's `util/prefix_varint.h`, the little-endian, low-bit-prefix
//! alternative to LEB128 that LLVM/MLIR bytecode also uses.
//!
//! The count of low zero bits in the first byte gives the encoding's length, so a reader knows
//! from one byte how many more to fetch:
//!
//! ```text
//! xxxxxxx1 -> 1 byte   (7 payload bits)     xxx10000 -> 5 bytes (35 bits)
//! xxxxxx10 -> 2 bytes  (14 bits)            xx100000 -> 6 bytes (42 bits)
//! xxxxx100 -> 3 bytes  (21 bits)            x1000000 -> 7 bytes (49 bits)
//! xxxx1000 -> 4 bytes  (28 bits)            10000000 -> 8 bytes (56 bits)
//!                                           00000000 -> 9 bytes (a fixed64 follows)
//! ```
//!
//! A prefix varint32 uses the first five rows, and so the same lengths as a LEB128 varint32; a
//! prefix varint64 uses all nine [R util/prefix_varint.h:20-56]. RocksDB 11.8.1 defines the
//! format and tests it (util/coding_test.cc) but no file format uses it yet.

use crate::error::{Error, Malformed};
use crate::util::coding::{MAX_VARINT64_LENGTH, Varint, decode_fixed64, varint_length};

/// The longest prefix varint32 [R util/prefix_varint.h:58].
pub const MAX_PREFIX_VARINT32_LENGTH: usize = 5;

/// The longest prefix varint64: a zero byte and a fixed64 [R util/prefix_varint.h:65].
pub const MAX_PREFIX_VARINT64_LENGTH: usize = 9;

/// `PrefixVarint32Length` [R util/prefix_varint.h:107-109]: the lengths of LEB128.
pub const fn prefix_varint32_length(value: u32) -> usize {
    varint_length(value as u64)
}

/// `PrefixVarint64Length` [R util/prefix_varint.h:111-114]: LEB128's lengths, with the
/// 10-byte case taking 9.
pub const fn prefix_varint64_length(value: u64) -> usize {
    let len = varint_length(value);
    if len < MAX_PREFIX_VARINT64_LENGTH {
        len
    } else {
        MAX_PREFIX_VARINT64_LENGTH
    }
}

/// The low `len` bytes of `(value << len) | (1 << (len - 1))`, for `1 <= len <= 8` and `value`
/// below `2^(7·len)`, so nothing is shifted out [R util/prefix_varint.h:116-125, :127-143].
fn store(value: u64, len: usize) -> Varint {
    let shift = u32::try_from(len).unwrap_or(0);
    let marker = 1u64.wrapping_shl(shift.wrapping_sub(1));
    let word = value.wrapping_shl(shift) | marker;
    let mut bytes = [0u8; MAX_VARINT64_LENGTH];
    for (slot, b) in bytes.iter_mut().zip(word.to_le_bytes()).take(len) {
        *slot = b;
    }
    Varint::new(bytes, len)
}

/// `EncodePrefixVarint32` [R util/prefix_varint.h:116-125].
pub fn encode_prefix_varint32(value: u32) -> Varint {
    store(u64::from(value), prefix_varint32_length(value))
}

/// The 9-byte form: a zero byte, then the value as a fixed64.
fn store_full(value: u64) -> Varint {
    let mut bytes = [0u8; MAX_VARINT64_LENGTH];
    for (slot, b) in bytes.iter_mut().skip(1).zip(value.to_le_bytes()) {
        *slot = b;
    }
    Varint::new(bytes, MAX_PREFIX_VARINT64_LENGTH)
}

/// `EncodePrefixVarint64` [R util/prefix_varint.h:127-143] with no minimum length.
pub fn encode_prefix_varint64(value: u64) -> Varint {
    let len = prefix_varint64_length(value);
    if len == MAX_PREFIX_VARINT64_LENGTH {
        store_full(value)
    } else {
        store(value, len)
    }
}

/// `EncodePrefixVarint64<kMinimumBytes>` [R util/prefix_varint.h:127-143]: at least
/// `min_bytes` bytes, a non-minimal encoding every decoder accepts. RocksDB requires
/// `kMinimumBytes < 9` at compile time; here a larger one is an [`Error::InvalidArgument`].
pub fn encode_prefix_varint64_min_bytes(value: u64, min_bytes: usize) -> Result<Varint, Error> {
    if min_bytes >= MAX_PREFIX_VARINT64_LENGTH {
        return Err(Error::InvalidArgument {
            what: "prefix varint64 minimum length of 9 or more",
        });
    }
    let len = prefix_varint64_length(value).max(min_bytes);
    Ok(if len == MAX_PREFIX_VARINT64_LENGTH {
        store_full(value)
    } else {
        store(value, len)
    })
}

/// `PutPrefixVarint32` [R util/prefix_varint.h:145-149].
pub fn put_prefix_varint32(dst: &mut Vec<u8>, value: u32) {
    dst.extend_from_slice(encode_prefix_varint32(value).as_bytes());
}

/// `PutPrefixVarint64` [R util/prefix_varint.h:151-155].
pub fn put_prefix_varint64(dst: &mut Vec<u8>, value: u64) {
    dst.extend_from_slice(encode_prefix_varint64(value).as_bytes());
}

/// `PrefixVarint32AddlByteCount` [R util/prefix_varint.h:161-173]: the bytes after `first`.
/// RocksDB returns the sentinel `kInvalidPrefixVarint32AddlByteCount` for a first byte that
/// names no prefix varint32 length; here that is an error.
pub fn prefix_varint32_addl_byte_count(first_byte: u8) -> Result<usize, Error> {
    let count = usize::try_from(first_byte.trailing_zeros()).unwrap_or(usize::MAX);
    if count < MAX_PREFIX_VARINT32_LENGTH {
        Ok(count)
    } else {
        Err(Error::Corruption {
            what: "prefix varint32",
            why: Malformed::PrefixVarintFirstByte,
        })
    }
}

/// `PrefixVarint64AddlByteCount` [R util/prefix_varint.h:176-180]: 8 for the zero byte of the
/// 9-byte form.
pub fn prefix_varint64_addl_byte_count(first_byte: u8) -> usize {
    // At most 8: a zero byte has 8 trailing zeros.
    usize::try_from(first_byte.trailing_zeros()).unwrap_or(MAX_PREFIX_VARINT64_LENGTH - 1)
}

/// `LoadPrefixVarintEncodedWord` [R util/prefix_varint.h:77-90] shifted past the prefix bits:
/// `first` and the `count <= 7` bytes after it as a little-endian word.
fn load(first_byte: u8, addl: &[u8], count: usize) -> u64 {
    let mut word = [0u8; 8];
    let mut slots = word.iter_mut();
    if let Some(slot) = slots.next() {
        *slot = first_byte;
    }
    for (slot, &b) in slots.zip(addl).take(count) {
        *slot = b;
    }
    let shift = u32::try_from(count).unwrap_or(0).wrapping_add(1);
    u64::from_le_bytes(word).wrapping_shr(shift)
}

/// `DecodePrefixVarint32` [R util/prefix_varint.h:186-213] for a reader that holds the first
/// byte and the additional bytes [`prefix_varint32_addl_byte_count`] asked for, in `addl`.
pub fn decode_prefix_varint32(first_byte: u8, addl: &[u8]) -> Result<u32, Error> {
    if first_byte & 1 != 0 {
        return Ok(u32::from(first_byte >> 1));
    }
    let count = prefix_varint32_addl_byte_count(first_byte)?;
    if addl.len() < count {
        return Err(Error::truncated("prefix varint32"));
    }
    u32::try_from(load(first_byte, addl, count)).map_err(|_| Error::Corruption {
        what: "prefix varint32",
        why: Malformed::VarintOverflow,
    })
}

/// `DecodePrefixVarint64` [R util/prefix_varint.h:216-240].
pub fn decode_prefix_varint64(first_byte: u8, addl: &[u8]) -> Result<u64, Error> {
    if first_byte & 1 != 0 {
        return Ok(u64::from(first_byte >> 1));
    }
    let count = prefix_varint64_addl_byte_count(first_byte);
    if addl.len() < count {
        return Err(Error::truncated("prefix varint64"));
    }
    if count == MAX_PREFIX_VARINT64_LENGTH - 1 {
        return decode_fixed64(addl);
    }
    Ok(load(first_byte, addl, count))
}

/// Splits `src` into its first byte and the rest.
fn split<'a>(src: &'a [u8], what: &'static str) -> Result<(u8, &'a [u8]), Error> {
    src.split_first()
        .map(|(&b, rest)| (b, rest))
        .ok_or(Error::truncated(what))
}

/// `GetPrefixVarint32Ptr` [R util/prefix_varint.h:244-268] over `src` = `[p, limit)`: the
/// value and the bytes it took.
pub fn get_prefix_varint32_ptr(src: &[u8]) -> Result<(u32, usize), Error> {
    let (first, rest) = split(src, "prefix varint32")?;
    if first & 1 != 0 {
        return Ok((u32::from(first >> 1), 1));
    }
    let count = prefix_varint32_addl_byte_count(first)?;
    let value = decode_prefix_varint32(first, rest)?;
    Ok((value, count.wrapping_add(1)))
}

/// `GetPrefixVarint64Ptr` [R util/prefix_varint.h:271-294].
pub fn get_prefix_varint64_ptr(src: &[u8]) -> Result<(u64, usize), Error> {
    let (first, rest) = split(src, "prefix varint64")?;
    if first & 1 != 0 {
        return Ok((u64::from(first >> 1), 1));
    }
    let count = prefix_varint64_addl_byte_count(first);
    let value = decode_prefix_varint64(first, rest)?;
    Ok((value, count.wrapping_add(1)))
}

/// `GetPrefixVarint32` [R util/prefix_varint.h:296-306]: advances `input` on success only.
pub fn get_prefix_varint32(input: &mut &[u8]) -> Result<u32, Error> {
    let (v, n) = get_prefix_varint32_ptr(input)?;
    *input = input.get(n..).unwrap_or(&[]);
    Ok(v)
}

/// `GetPrefixVarint64` [R util/prefix_varint.h:309-319].
pub fn get_prefix_varint64(input: &mut &[u8]) -> Result<u64, Error> {
    let (v, n) = get_prefix_varint64_ptr(input)?;
    *input = input.get(n..).unwrap_or(&[]);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_byte_names_the_length() {
        for len in 1..=8usize {
            let v = if len == 1 { 0 } else { 1u64 << (7 * (len - 1)) };
            let enc = encode_prefix_varint64(v);
            assert_eq!(enc.len(), len);
            assert_eq!(prefix_varint64_addl_byte_count(enc.as_bytes()[0]), len - 1);
        }
    }
}
