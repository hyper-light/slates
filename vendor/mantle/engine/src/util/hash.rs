//! RocksDB's hash functions: `util/hash.h`, `util/hash.cc` and `util/hash128.h`
//! (docs/research/24 §1.2).
//!
//! - [`hash`] (`Hash`) is the 32-bit MurmurHash1 variant that keys the legacy Bloom filter
//!   ([`bloom_hash`]) and the data-block hash index ([`get_slice_hash`]). Its tail bytes are
//!   added sign-extended through `int8_t` [R util/hash.cc:45-59], so a byte of 0x80 or more
//!   adds `0xFFFFFF80` and up, not `0x80`: part of the format (docs/research/24 §5 R2).
//! - [`hash64`] (`Hash64`) is XXPH3, the XXH3 preview (`util::xxph3`), which keys the
//!   FastLocalBloom and Ribbon filters.
//! - [`hash2x64`]/[`hash128`] are the released XXH3-128 (`util::xxhash`), and
//!   [`bijective_hash2x64`] a bijection of 128 bits adapted from it; the SST unique ID is built
//!   from these [R table/unique_id.cc:15-161].
//!
//! Hash arithmetic wraps: that is the stated meaning of a hash.

use crate::util::math128::{lower64of128, multiply64to128, upper64of128};
use crate::util::xxhash::xxh3_128bits_with_seed;
use crate::util::xxph3::{xxph3_64bits, xxph3_64bits_with_seed};

/// `BloomHash`'s seed [R util/hash.h:93-95].
pub const BLOOM_HASH_SEED: u32 = 0xbc9f_1d34;

/// `GetSliceHash`'s seed [R util/hash.h:121-123].
pub const SLICE_HASH_SEED: u32 = 397;

/// `Hash`'s multiplier and final shift [R util/hash.cc:29-30].
const MURMUR_M: u32 = 0xc6a4_a793;
const MURMUR_R: u32 = 24;

/// A byte sign-extended to 32 bits, as `static_cast<uint32_t>(static_cast<int8_t>(b))`
/// [R util/hash.cc:45-59].
const fn sign_extended(b: u8) -> u32 {
    (b.cast_signed() as i32).cast_unsigned()
}

/// `Hash` [R util/hash.cc:25-65]: stable 32-bit hash of `data`.
pub fn hash(data: &[u8], seed: u32) -> u32 {
    // `seed ^ (n * m)` in size_t, then narrowed: only n's low 32 bits reach the result.
    let n = lower32of64(data.len() as u64);
    let mut h = seed ^ n.wrapping_mul(MURMUR_M);
    let (words, tail) = data.as_chunks::<4>();
    for w in words {
        h = h.wrapping_add(u32::from_le_bytes(*w));
        h = h.wrapping_mul(MURMUR_M);
        h ^= h >> 16;
    }
    let tail_sum = match *tail {
        [a, b, c] => Some(
            sign_extended(c)
                .wrapping_shl(16)
                .wrapping_add(sign_extended(b).wrapping_shl(8))
                .wrapping_add(sign_extended(a)),
        ),
        [a, b] => Some(
            sign_extended(b)
                .wrapping_shl(8)
                .wrapping_add(sign_extended(a)),
        ),
        [a] => Some(sign_extended(a)),
        _ => None,
    };
    if let Some(sum) = tail_sum {
        h = h.wrapping_add(sum);
        h = h.wrapping_mul(MURMUR_M);
        h ^= h >> MURMUR_R;
    }
    h
}

/// `BloomHash` [R util/hash.h:93-95]: the legacy Bloom filter's hash.
pub fn bloom_hash(key: &[u8]) -> u32 {
    hash(key, BLOOM_HASH_SEED)
}

/// `GetSliceHash` [R util/hash.h:121-123]: the data-block hash index's hash.
pub fn get_slice_hash(key: &[u8]) -> u32 {
    hash(key, SLICE_HASH_SEED)
}

/// `Hash64(data, n)` [R util/hash.cc:85-88]: stable 64-bit hash, the same as seed 0.
pub fn hash64(data: &[u8]) -> u64 {
    xxph3_64bits(data)
}

/// `Hash64(data, n, seed)` [R util/hash.cc:81-83].
pub fn hash64_with_seed(data: &[u8], seed: u64) -> u64 {
    xxph3_64bits_with_seed(data, seed)
}

/// `GetSliceHash64` [R util/hash.h:97-99]: the FastLocalBloom and Ribbon filters' hash.
pub fn get_slice_hash64(key: &[u8]) -> u64 {
    hash64(key)
}

/// `NPHash64(data, n)` [R util/hash.h:56-64]: a hash only in-memory structures may use; today
/// the same as [`hash64`].
pub fn np_hash64(data: &[u8]) -> u64 {
    hash64(data)
}

/// `NPHash64(data, n, seed)` [R util/hash.h:45-53].
pub fn np_hash64_with_seed(data: &[u8], seed: u64) -> u64 {
    hash64_with_seed(data, seed)
}

/// `GetSliceNPHash64(s, seed)` [R util/hash.h:108-110]: the caches' shard hash.
pub fn get_slice_np_hash64(key: &[u8], seed: u64) -> u64 {
    np_hash64_with_seed(key, seed)
}

/// `Hash128(data, n, seed)` [R util/hash.cc:105-108]: XXH3-128.
pub fn hash128_with_seed(data: &[u8], seed: u64) -> u128 {
    xxh3_128bits_with_seed(data, seed)
}

/// `Hash128(data, n)` [R util/hash.cc:110-114], the same as seed 0.
pub fn hash128(data: &[u8]) -> u128 {
    hash128_with_seed(data, 0)
}

/// `GetSliceHash128` [R util/hash128.h:22-24].
pub fn get_slice_hash128(key: &[u8]) -> u128 {
    hash128(key)
}

/// `Hash2x64(data, n, seed, &high64, &low64)` [R util/hash.cc:123-129]: `(high64, low64)`.
pub fn hash2x64_with_seed(data: &[u8], seed: u64) -> (u64, u64) {
    let h = hash128_with_seed(data, seed);
    (upper64of128(h), lower64of128(h))
}

/// `Hash2x64(data, n, &high64, &low64)` [R util/hash.cc:116-121].
pub fn hash2x64(data: &[u8]) -> (u64, u64) {
    hash2x64_with_seed(data, 0)
}

/// The parts of XXH3's secret `BijectiveHash2x64` folds in [R util/hash.cc:151-152].
const BITFLIP_LOW: u64 = 0x5997_3f00_3336_2349;
const BITFLIP_HIGH: u64 = 0xc202_7976_92d6_3d58;
/// XXH3's primes [R util/hash.cc:154, :159-163] and the avalanche multiplier [R :134].
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME32_2_MINUS_1: u64 = 0x85EB_CA76;
const AVALANCHE: u64 = 0x1656_6791_9E37_79F9;
/// `(len - 1) << 54` for XXH3's 16-byte input [R util/hash.cc:157].
const LEN16_MARK: u64 = 0x03c0_0000_0000_0000;
/// Multiplicative inverses modulo 2^64 (2^32 for the last) of the constants above
/// [R util/hash.cc:141, :175, :180, :182].
const AVALANCHE_INVERSE: u64 = 0x08da_8ee4_1d6d_f849;
const PRIME64_2_INVERSE: u64 = 0x0ba7_9078_168d_4baf;
const PRIME64_1_INVERSE: u64 = 0x0887_4934_32ba_db37;
const PRIME32_2_INVERSE: u32 = 0xb6c9_2f47;
/// The high word of a 64-bit value.
const HIGH_WORD: u64 = 0xFFFF_FFFF_0000_0000;

/// `XXH3_avalanche` [R util/hash.cc:132-137].
fn xxh3_avalanche(mut h64: u64) -> u64 {
    h64 ^= h64 >> 37;
    h64 = h64.wrapping_mul(AVALANCHE);
    h64 ^ (h64 >> 32)
}

/// `XXH3_unavalanche` [R util/hash.cc:139-144].
fn xxh3_unavalanche(mut h64: u64) -> u64 {
    h64 ^= h64 >> 32;
    h64 = h64.wrapping_mul(AVALANCHE_INVERSE);
    h64 ^ (h64 >> 37)
}

/// `BijectiveHash2x64(in_high64, in_low64, seed, ...)` [R util/hash.cc:148-166]: equal to
/// [`hash2x64_with_seed`] of the 16 bytes `in_low64 ‖ in_high64` (little-endian), and
/// invertible; returns `(high64, low64)`.
pub fn bijective_hash2x64_with_seed(in_high64: u64, in_low64: u64, seed: u64) -> (u64, u64) {
    let bitflipl = BITFLIP_LOW.wrapping_sub(seed);
    let bitfliph = BITFLIP_HIGH.wrapping_add(seed);
    let tmp = multiply64to128(in_low64 ^ in_high64 ^ bitflipl, PRIME64_1);
    let mut lo = lower64of128(tmp);
    let mut hi = upper64of128(tmp);
    lo = lo.wrapping_add(LEN16_MARK);
    let in_high = in_high64 ^ bitfliph;
    hi = hi
        .wrapping_add(in_high)
        .wrapping_add(u64::from(lower32of64(in_high)).wrapping_mul(PRIME32_2_MINUS_1));
    lo ^= hi.swap_bytes();
    let tmp = multiply64to128(lo, PRIME64_2);
    lo = lower64of128(tmp);
    hi = upper64of128(tmp).wrapping_add(hi.wrapping_mul(PRIME64_2));
    (xxh3_avalanche(hi), xxh3_avalanche(lo))
}

/// `BijectiveHash2x64(in_high64, in_low64, ...)` [R util/hash.cc:192-195], seed 0.
pub fn bijective_hash2x64(in_high64: u64, in_low64: u64) -> (u64, u64) {
    bijective_hash2x64_with_seed(in_high64, in_low64, 0)
}

/// `BijectiveUnhash2x64(in_high64, in_low64, seed, ...)` [R util/hash.cc:168-190]: the inverse
/// of [`bijective_hash2x64_with_seed`]; returns `(high64, low64)`.
pub fn bijective_unhash2x64_with_seed(in_high64: u64, in_low64: u64, seed: u64) -> (u64, u64) {
    let bitflipl = BITFLIP_LOW.wrapping_sub(seed);
    let bitfliph = BITFLIP_HIGH.wrapping_add(seed);
    let mut lo = xxh3_unavalanche(in_low64);
    let mut hi = xxh3_unavalanche(in_high64);
    lo = lo.wrapping_mul(PRIME64_2_INVERSE);
    hi = hi.wrapping_sub(upper64of128(multiply64to128(lo, PRIME64_2)));
    hi = hi.wrapping_mul(PRIME64_2_INVERSE);
    lo ^= hi.swap_bytes();
    lo = lo.wrapping_sub(LEN16_MARK);
    lo = lo.wrapping_mul(PRIME64_1_INVERSE);
    hi = hi.wrapping_sub(upper64of128(multiply64to128(lo, PRIME64_1)));
    let tmp32 = lower32of64(hi).wrapping_mul(PRIME32_2_INVERSE);
    hi = hi.wrapping_sub(u64::from(tmp32));
    hi = (hi & HIGH_WORD)
        .wrapping_sub(u64::from(tmp32).wrapping_mul(PRIME32_2_MINUS_1) & HIGH_WORD)
        .wrapping_add(u64::from(tmp32));
    hi ^= bitfliph;
    lo ^= hi ^ bitflipl;
    (hi, lo)
}

/// `BijectiveUnhash2x64(in_high64, in_low64, ...)` [R util/hash.cc:197-200], seed 0.
pub fn bijective_unhash2x64(in_high64: u64, in_low64: u64) -> (u64, u64) {
    bijective_unhash2x64_with_seed(in_high64, in_low64, 0)
}

/// `Upper32of64` [R util/hash.h:126-128].
pub const fn upper32of64(v: u64) -> u32 {
    lower32of64(v >> 32)
}

/// `Lower32of64` [R util/hash.h:129].
pub const fn lower32of64(v: u64) -> u32 {
    let [a, b, c, d, ..] = v.to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}
