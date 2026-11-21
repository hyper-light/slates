//! 128-bit helpers: RocksDB's `util/math128.h`. RocksDB's `Unsigned128` is `__uint128_t`
//! where the compiler has it and a struct of two words elsewhere; here it is `u128`.

use crate::error::Error;
use crate::util::coding::{decode_fixed16, decode_fixed32, decode_fixed64};

/// `Lower64of128` [R util/math128.h:152-158].
pub const fn lower64of128(v: u128) -> u64 {
    let [a, b, c, d, e, f, g, h, ..] = v.to_le_bytes();
    u64::from_le_bytes([a, b, c, d, e, f, g, h])
}

/// `Upper64of128` [R util/math128.h:160-166].
pub const fn upper64of128(v: u128) -> u64 {
    lower64of128(v >> 64)
}

/// `Multiply64to128` [R util/math128.h:171-191]: the full product, which cannot overflow.
pub const fn multiply64to128(a: u64, b: u64) -> u128 {
    (a as u128).wrapping_mul(b as u128)
}

/// `EncodeFixed128` [R util/math128.h:266-269]: the low word, then the high word, each
/// little-endian.
pub const fn encode_fixed128(value: u128) -> [u8; 16] {
    value.to_le_bytes()
}

/// `DecodeFixed128` [R util/math128.h:271-274], failing on fewer than 16 bytes.
pub fn decode_fixed128(src: &[u8]) -> Result<u128, Error> {
    src.first_chunk::<16>()
        .map(|b| u128::from_le_bytes(*b))
        .ok_or(Error::truncated("fixed128"))
}

/// The integers `EncodeFixedGeneric`/`DecodeFixedGeneric` specialize
/// [R util/math128.h:278-336]: u16, u32, u64 and 128 bits.
pub trait FixedGeneric: Sized + Copy {
    /// `EncodeFixedGeneric`, appending to `dst`.
    fn put_fixed_generic(self, dst: &mut Vec<u8>);
    /// `DecodeFixedGeneric`, from the front of `src`.
    fn decode_fixed_generic(src: &[u8]) -> Result<Self, Error>;
}

impl FixedGeneric for u16 {
    fn put_fixed_generic(self, dst: &mut Vec<u8>) {
        dst.extend_from_slice(&self.to_le_bytes());
    }
    fn decode_fixed_generic(src: &[u8]) -> Result<Self, Error> {
        decode_fixed16(src)
    }
}

impl FixedGeneric for u32 {
    fn put_fixed_generic(self, dst: &mut Vec<u8>) {
        dst.extend_from_slice(&self.to_le_bytes());
    }
    fn decode_fixed_generic(src: &[u8]) -> Result<Self, Error> {
        decode_fixed32(src)
    }
}

impl FixedGeneric for u64 {
    fn put_fixed_generic(self, dst: &mut Vec<u8>) {
        dst.extend_from_slice(&self.to_le_bytes());
    }
    fn decode_fixed_generic(src: &[u8]) -> Result<Self, Error> {
        decode_fixed64(src)
    }
}

impl FixedGeneric for u128 {
    fn put_fixed_generic(self, dst: &mut Vec<u8>) {
        dst.extend_from_slice(&encode_fixed128(self));
    }
    fn decode_fixed_generic(src: &[u8]) -> Result<Self, Error> {
        decode_fixed128(src)
    }
}
