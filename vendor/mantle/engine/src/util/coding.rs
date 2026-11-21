//! Fixed-width and varint integer coding and length-prefixed byte strings: RocksDB's
//! `util/coding_lean.h`, `util/coding.h` and `util/coding.cc` (docs/research/24 §1.1).
//!
//! Fixed-width integers are little-endian. A varint is LEB128: seven bits per byte, low bits
//! first, bit 0x80 set on every byte but the last; a varint32 takes at most 5 bytes and a
//! varint64 at most 10. A signed varint64 is the zigzag of the value as a varint64.
//!
//! RocksDB's `Put*` append to a `std::string`; here they append to a `Vec<u8>`. Its `Get*` read
//! from the front of a `Slice` and advance it; here they take `&mut &[u8]`, advance it only on
//! success, and return a typed error where RocksDB returns `false` or null. The `*Ptr` forms,
//! which RocksDB bounds by a `limit` pointer, take the bytes up to that limit and return the
//! value with the number of bytes it took.
//!
//! Two decodes are stricter than RocksDB's (docs/research/24 §1.1, DECISION): a varint32 whose
//! fifth byte carries bits above 2^32, and a varint64 whose tenth byte is above 1, are
//! [`Malformed::VarintOverflow`]. RocksDB drops those bits in its shift and never writes them,
//! so no file it wrote is refused. RocksDB's `GetLengthPrefixedSlice(const char*)`, which
//! trusts its input, checks it here.

use crate::error::{Error, Malformed};

/// The longest varint32: 32 bits at 7 per byte [R util/coding.h:156, `kMaxBytesPerVarint32`].
pub const MAX_VARINT32_LENGTH: usize = 5;

/// The longest varint64: 64 bits at 7 per byte [R util/coding.h:36, `kMaxVarint64Length`].
pub const MAX_VARINT64_LENGTH: usize = 10;

/// The continuation bit of a varint byte, and the 7 payload bits below it.
const CONTINUATION: u8 = 0x80;
const PAYLOAD: u8 = 0x7F;

/// The shift of each varint byte's payload.
const SHIFTS: [u32; MAX_VARINT64_LENGTH] = [0, 7, 14, 21, 28, 35, 42, 49, 56, 63];

/// The largest last byte of a full-length varint: the bits left of the type's width after the
/// earlier bytes' 28 or 63.
const VARINT32_LAST_BYTE_MAX: u8 = 0x0F;
const VARINT64_LAST_BYTE_MAX: u8 = 0x01;

/// An encoded varint or prefix varint, held without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Varint {
    bytes: [u8; MAX_VARINT64_LENGTH],
    len: usize,
}

impl Varint {
    /// An encoding of the first `len` bytes of `bytes`; `len` never exceeds the array.
    pub(crate) fn new(bytes: [u8; MAX_VARINT64_LENGTH], len: usize) -> Self {
        Self {
            bytes,
            len: len.min(MAX_VARINT64_LENGTH),
        }
    }

    /// The encoded bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&self.bytes)
    }

    /// The number of encoded bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Never true: every encoding takes at least one byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The low byte of `v`.
pub(crate) fn low_byte(v: u64) -> u8 {
    let [b, ..] = v.to_le_bytes();
    b
}

/// `EncodeFixed16` [R util/coding_lean.h:24-31].
pub const fn encode_fixed16(value: u16) -> [u8; 2] {
    value.to_le_bytes()
}

/// `EncodeFixed32` [R util/coding_lean.h:33-42].
pub const fn encode_fixed32(value: u32) -> [u8; 4] {
    value.to_le_bytes()
}

/// `EncodeFixed64` [R util/coding_lean.h:44-57].
pub const fn encode_fixed64(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

/// The first `N` bytes of `src`.
fn first<const N: usize>(src: &[u8], what: &'static str) -> Result<[u8; N], Error> {
    src.first_chunk::<N>()
        .copied()
        .ok_or(Error::truncated(what))
}

/// `DecodeFixed16` [R util/coding_lean.h:62-72], which reads without a bound; this one fails
/// on fewer than 2 bytes.
pub fn decode_fixed16(src: &[u8]) -> Result<u16, Error> {
    first::<2>(src, "fixed16").map(u16::from_le_bytes)
}

/// `DecodeFixed32` [R util/coding_lean.h:74-86], bounded as [`decode_fixed16`].
pub fn decode_fixed32(src: &[u8]) -> Result<u32, Error> {
    first::<4>(src, "fixed32").map(u32::from_le_bytes)
}

/// `DecodeFixed64` [R util/coding_lean.h:88-97], bounded as [`decode_fixed16`].
pub fn decode_fixed64(src: &[u8]) -> Result<u64, Error> {
    first::<8>(src, "fixed64").map(u64::from_le_bytes)
}

/// `PutFixed16` [R util/coding.h:119-128].
pub fn put_fixed16(dst: &mut Vec<u8>, value: u16) {
    dst.extend_from_slice(&encode_fixed16(value));
}

/// `PutFixed32` [R util/coding.h:130-139].
pub fn put_fixed32(dst: &mut Vec<u8>, value: u32) {
    dst.extend_from_slice(&encode_fixed32(value));
}

/// `PutFixed64` [R util/coding.h:141-150].
pub fn put_fixed64(dst: &mut Vec<u8>, value: u64) {
    dst.extend_from_slice(&encode_fixed64(value));
}

/// `EncodeVarint64` [R util/coding.h:163-172]; `EncodeVarint32` [R util/coding.cc:24-50] writes
/// the same bytes for every 32-bit value.
pub fn encode_varint64(value: u64) -> Varint {
    let mut bytes = [0u8; MAX_VARINT64_LENGTH];
    let mut len = MAX_VARINT64_LENGTH;
    let mut v = value;
    for (slot, n) in bytes.iter_mut().zip(1usize..) {
        if v <= u64::from(PAYLOAD) {
            *slot = low_byte(v);
            len = n;
            break;
        }
        *slot = (low_byte(v) & PAYLOAD) | CONTINUATION;
        v >>= 7;
    }
    Varint::new(bytes, len)
}

/// `EncodeVarint32` [R util/coding.cc:24-50].
pub fn encode_varint32(value: u32) -> Varint {
    encode_varint64(u64::from(value))
}

/// `PutVarint32` [R util/coding.h:152-161], one value per call.
pub fn put_varint32(dst: &mut Vec<u8>, value: u32) {
    dst.extend_from_slice(encode_varint32(value).as_bytes());
}

/// `PutVarint32Varint32Varint32` [R util/coding.h:163-172], for up to four values: encoded on the
/// stack and appended at once, one byte each when every value is below 128.
#[inline]
pub fn put_varint32s(dst: &mut Vec<u8>, values: &[u32]) {
    if values.iter().all(|&v| v <= u32::from(PAYLOAD)) {
        let mut small = [0u8; 4];
        for (slot, &v) in small.iter_mut().zip(values) {
            *slot = low_byte(u64::from(v));
        }
        dst.extend_from_slice(small.get(..values.len()).unwrap_or_default());
        return;
    }
    let mut bytes = [0u8; 4 * MAX_VARINT32_LENGTH];
    let mut len = 0usize;
    for &v in values {
        let encoded = encode_varint32(v);
        let e = encoded.as_bytes();
        if let Some(slot) = bytes.get_mut(len..len.saturating_add(e.len())) {
            slot.copy_from_slice(e);
            len = len.saturating_add(e.len());
        }
    }
    dst.extend_from_slice(bytes.get(..len).unwrap_or_default());
}

/// `PutVarint64` [R util/coding.h:174-178].
pub fn put_varint64(dst: &mut Vec<u8>, value: u64) {
    dst.extend_from_slice(encode_varint64(value).as_bytes());
}

/// `PutVarsignedint64` [R util/coding.h:180-185].
pub fn put_varsignedint64(dst: &mut Vec<u8>, value: i64) {
    put_varint64(dst, i64_to_zigzag(value));
}

/// `PutVarint64Varint64` [R util/coding.h:187-192].
pub fn put_varint64_varint64(dst: &mut Vec<u8>, v1: u64, v2: u64) {
    put_varint64(dst, v1);
    put_varint64(dst, v2);
}

/// `PutVarint32Varint64` [R util/coding.h:194-199].
pub fn put_varint32_varint64(dst: &mut Vec<u8>, v1: u32, v2: u64) {
    put_varint32(dst, v1);
    put_varint64(dst, v2);
}

/// `PutVarint32Varint32Varint64` [R util/coding.h:201-208].
pub fn put_varint32_varint32_varint64(dst: &mut Vec<u8>, v1: u32, v2: u32, v3: u64) {
    put_varint32(dst, v1);
    put_varint32(dst, v2);
    put_varint64(dst, v3);
}

/// A length the varint32 prefix can state. RocksDB casts a longer one to 32 bits and writes a
/// prefix that disagrees with the bytes after it.
fn prefix_length(len: usize) -> Result<u32, Error> {
    u32::try_from(len).map_err(|_| Error::InvalidArgument {
        what: "length-prefixed slice longer than a varint32 prefix can state",
    })
}

/// `PutLengthPrefixedSlice` [R util/coding.h:210-213].
pub fn put_length_prefixed_slice(dst: &mut Vec<u8>, value: &[u8]) -> Result<(), Error> {
    put_varint32(dst, prefix_length(value.len())?);
    dst.extend_from_slice(value);
    Ok(())
}

/// `PutLengthPrefixedSliceParts` [R util/coding.h:215-229]: the parts' summed length, then
/// each part.
pub fn put_length_prefixed_slice_parts(dst: &mut Vec<u8>, parts: &[&[u8]]) -> Result<(), Error> {
    put_parts(dst, parts, 0)
}

/// `PutLengthPrefixedSlicePartsWithPadding` [R util/coding.h:231-235]: the prefix counts
/// `pad_sz` zero bytes appended after the parts.
pub fn put_length_prefixed_slice_parts_with_padding(
    dst: &mut Vec<u8>,
    parts: &[&[u8]],
    pad_sz: usize,
) -> Result<(), Error> {
    put_parts(dst, parts, pad_sz)?;
    let padded = dst
        .len()
        .checked_add(pad_sz)
        .ok_or(Error::InvalidArgument {
            what: "length-prefixed slice parts longer than memory",
        })?;
    dst.resize(padded, 0);
    Ok(())
}

fn put_parts(dst: &mut Vec<u8>, parts: &[&[u8]], extra: usize) -> Result<(), Error> {
    let total = parts
        .iter()
        .try_fold(extra, |sum, part| sum.checked_add(part.len()))
        .ok_or(Error::InvalidArgument {
            what: "length-prefixed slice parts longer than memory",
        })?;
    put_varint32(dst, prefix_length(total)?);
    for part in parts {
        dst.extend_from_slice(part);
    }
    Ok(())
}

/// `VarintLength` [R util/coding.h:237-244]: the bytes of `v`'s varint encoding.
pub const fn varint_length(v: u64) -> usize {
    match v {
        0..=0x7F => 1,
        0x80..=0x3FFF => 2,
        0x4000..=0x1F_FFFF => 3,
        0x20_0000..=0xFFF_FFFF => 4,
        0x1000_0000..=0x7_FFFF_FFFF => 5,
        0x8_0000_0000..=0x3FF_FFFF_FFFF => 6,
        0x400_0000_0000..=0x1_FFFF_FFFF_FFFF => 7,
        0x2_0000_0000_0000..=0xFF_FFFF_FFFF_FFFF => 8,
        0x100_0000_0000_0000..=0x7FFF_FFFF_FFFF_FFFF => 9,
        _ => 10,
    }
}

/// Decodes a varint of at most `SHIFTS.len()` bytes whose last byte is at most `last_max`.
fn decode_varint(
    src: &[u8],
    max_len: usize,
    last_max: u8,
    what: &'static str,
) -> Result<(u64, usize), Error> {
    let mut result = 0u64;
    for ((&byte, shift), n) in src.iter().zip(SHIFTS).zip(1usize..).take(max_len) {
        let last = n == max_len;
        if last && byte & CONTINUATION != 0 {
            return Err(Error::Corruption {
                what,
                why: Malformed::VarintTooLong,
            });
        }
        if last && byte > last_max {
            return Err(Error::Corruption {
                what,
                why: Malformed::VarintOverflow,
            });
        }
        result |= u64::from(byte & PAYLOAD).wrapping_shl(shift);
        if byte & CONTINUATION == 0 {
            return Ok((result, n));
        }
    }
    Err(Error::truncated(what))
}

/// `GetVarint32Ptr` [R util/coding.h:106-116; util/coding.cc:55-71] over `src` = `[p, limit)`:
/// the value and the bytes it took.
pub fn get_varint32_ptr(src: &[u8]) -> Result<(u32, usize), Error> {
    if let Some(&b) = src.first()
        && b & CONTINUATION == 0
    {
        return Ok((u32::from(b), 1));
    }
    let (v, n) = decode_varint(src, MAX_VARINT32_LENGTH, VARINT32_LAST_BYTE_MAX, "varint32")?;
    // Four 7-bit groups and a last byte of at most 0x0F hold at most 32 bits.
    let v = u32::try_from(v).map_err(|_| Error::Corruption {
        what: "varint32",
        why: Malformed::VarintOverflow,
    })?;
    Ok((v, n))
}

/// `GetVarint64Ptr` [R util/coding.cc:73-88] over `src` = `[p, limit)`.
pub fn get_varint64_ptr(src: &[u8]) -> Result<(u64, usize), Error> {
    decode_varint(src, MAX_VARINT64_LENGTH, VARINT64_LAST_BYTE_MAX, "varint64")
}

/// `GetVarsignedint64Ptr` [R util/coding.h:86-93].
pub fn get_varsignedint64_ptr(src: &[u8]) -> Result<(i64, usize), Error> {
    get_varint64_ptr(src).map(|(u, n)| (zigzag_to_i64(u), n))
}

/// Takes `n` bytes off the front of `input`; `n` never exceeds its length here.
fn advance(input: &mut &[u8], n: usize) {
    *input = input.get(n..).unwrap_or(&[]);
}

/// Reads with `f` from the front of `input` and advances past what it took, on success only.
fn get<T>(
    input: &mut &[u8],
    f: impl FnOnce(&[u8]) -> Result<(T, usize), Error>,
) -> Result<T, Error> {
    let (v, n) = f(input)?;
    advance(input, n);
    Ok(v)
}

/// `GetFixed16` [R util/coding.h:264-271].
pub fn get_fixed16(input: &mut &[u8]) -> Result<u16, Error> {
    get(input, |s| decode_fixed16(s).map(|v| (v, 2)))
}

/// `GetFixed32` [R util/coding.h:255-262].
pub fn get_fixed32(input: &mut &[u8]) -> Result<u32, Error> {
    get(input, |s| decode_fixed32(s).map(|v| (v, 4)))
}

/// `GetFixed64` [R util/coding.h:246-253].
pub fn get_fixed64(input: &mut &[u8]) -> Result<u64, Error> {
    get(input, |s| decode_fixed64(s).map(|v| (v, 8)))
}

/// `GetVarint32` [R util/coding.h:273-283].
pub fn get_varint32(input: &mut &[u8]) -> Result<u32, Error> {
    get(input, get_varint32_ptr)
}

/// `GetVarint64` [R util/coding.h:285-295].
pub fn get_varint64(input: &mut &[u8]) -> Result<u64, Error> {
    get(input, get_varint64_ptr)
}

/// `GetVarsignedint64` [R util/coding.h:297-307].
pub fn get_varsignedint64(input: &mut &[u8]) -> Result<i64, Error> {
    get(input, get_varsignedint64_ptr)
}

/// A varint32 length and that many bytes after it, from the front of `src`: the slice and the
/// bytes taken in all.
fn length_prefixed(src: &[u8]) -> Result<(&[u8], usize), Error> {
    let (len, n) = get_varint32_ptr(src)?;
    let len = usize::try_from(len).map_err(|_| Error::truncated("length-prefixed slice"))?;
    let end = n
        .checked_add(len)
        .ok_or(Error::truncated("length-prefixed slice"))?;
    let value = src
        .get(n..end)
        .ok_or(Error::truncated("length-prefixed slice"))?;
    Ok((value, end))
}

/// `GetLengthPrefixedSlice(Slice*, Slice*)` [R util/coding.h:309-318]. RocksDB leaves the
/// input past the length when the bytes fall short; this leaves it unmoved.
pub fn get_length_prefixed_slice<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let src: &'a [u8] = input;
    let (value, n) = length_prefixed(src)?;
    *input = src.get(n..).unwrap_or(&[]);
    Ok(value)
}

/// `GetLengthPrefixedSlice(const char*)` [R util/coding.h:320-326], which assumes well-formed
/// input and reads the length's bytes without a bound; this one checks both.
pub fn decode_length_prefixed_slice(data: &[u8]) -> Result<&[u8], Error> {
    length_prefixed(data).map(|(value, _)| value)
}

/// `GetSliceUntil` [R util/coding.h:328-337]: the bytes before the first `delimiter` (or all of
/// them), and `slice` advanced past the delimiter.
pub fn get_slice_until<'a>(slice: &mut &'a [u8], delimiter: u8) -> &'a [u8] {
    let src: &'a [u8] = slice;
    let found = src
        .iter()
        .position(|&b| b == delimiter)
        .and_then(|len| src.split_at_checked(len));
    match found {
        Some((ret, rest)) => {
            *slice = rest.get(1..).unwrap_or(&[]);
            ret
        }
        None => {
            *slice = &[];
            src
        }
    }
}

/// `i64ToZigzag` [R util/coding.h:73-75].
pub const fn i64_to_zigzag(l: i64) -> u64 {
    (l.cast_unsigned() << 1) ^ (l >> 63).cast_unsigned()
}

/// `zigzagToI64` [R util/coding.h:76-78].
pub const fn zigzag_to_i64(n: u64) -> i64 {
    (n >> 1).cast_signed() ^ (n & 1).cast_signed().wrapping_neg()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_length_matches_the_encoder_at_every_boundary() {
        for bits in 0..64 {
            for v in [(1u64 << bits) - 1, 1u64 << bits, (1u64 << bits) + 1] {
                assert_eq!(varint_length(v), encode_varint64(v).len(), "{v:#x}");
            }
        }
        assert_eq!(varint_length(u64::MAX), 10);
    }

    #[test]
    fn overlong_last_bytes_are_refused() {
        // The fifth varint32 byte may hold 4 bits; RocksDB drops a fifth bit, this refuses it.
        let over32 = [0xFF, 0xFF, 0xFF, 0xFF, 0x10];
        assert_eq!(
            get_varint32_ptr(&over32),
            Err(Error::Corruption {
                what: "varint32",
                why: Malformed::VarintOverflow
            })
        );
        assert_eq!(
            get_varint32_ptr(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]),
            Ok((u32::MAX, 5))
        );
        let mut over64 = [0xFFu8; 10];
        over64[9] = 0x02;
        assert_eq!(
            get_varint64_ptr(&over64),
            Err(Error::Corruption {
                what: "varint64",
                why: Malformed::VarintOverflow
            })
        );
        over64[9] = 0x01;
        assert_eq!(get_varint64_ptr(&over64), Ok((u64::MAX, 10)));
    }

    #[test]
    fn a_failed_get_leaves_the_input_where_it_was() {
        let bytes = [0x05, b'a', b'b'];
        let mut input: &[u8] = &bytes;
        assert_eq!(
            get_length_prefixed_slice(&mut input),
            Err(Error::truncated("length-prefixed slice"))
        );
        assert_eq!(input, &bytes[..]);
        let mut short: &[u8] = &[1, 2, 3];
        assert!(get_fixed32(&mut short).is_err());
        assert_eq!(short, &[1, 2, 3]);
    }

    #[test]
    fn slice_until_splits_at_the_first_delimiter() {
        let mut s: &[u8] = b"ab,cd,";
        assert_eq!(get_slice_until(&mut s, b','), b"ab");
        assert_eq!(get_slice_until(&mut s, b','), b"cd");
        assert_eq!(s, b"");
        let mut t: &[u8] = b"xyz";
        assert_eq!(get_slice_until(&mut t, b','), b"xyz");
        assert!(t.is_empty());
    }

    #[test]
    fn padding_is_counted_in_the_prefix() {
        let mut dst = Vec::new();
        put_length_prefixed_slice_parts_with_padding(&mut dst, &[b"ab", b"c"], 2).unwrap();
        assert_eq!(dst, [5, b'a', b'b', b'c', 0, 0]);
    }

    #[test]
    fn zigzag_round_trips_the_extremes() {
        for v in [0, 1, -1, i64::MAX, i64::MIN, 63, -64] {
            assert_eq!(zigzag_to_i64(i64_to_zigzag(v)), v);
        }
        assert_eq!(i64_to_zigzag(-1), 1);
        assert_eq!(i64_to_zigzag(1), 2);
    }
}
