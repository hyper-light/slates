//! XXPH3, the preview of XXH3 in xxHash 0.7.2 that RocksDB vendors as `util/xxph3.h` and
//! calls `Hash64` [R util/hash.cc:81-88; util/xxph3.h:39-43, :133-135].
//!
//! It is not the released XXH3 (`util::xxhash`): its short-input mixing, its long-input
//! accumulator and its secret offsets differ, and RocksDB changed its empty-input result
//! [R util/xxph3.h:1126-1139]. It keys the FastLocalBloom and Ribbon filters, so a different
//! hash makes those filters answer "absent" for keys that exist (docs/research/24 §5 R1). This
//! port follows the header's scalar code path, which every SIMD path in it computes the same
//! as; the golden vectors of `tests/golden.rs` check it against the C++ over every length to
//! 4096 and several seeds.
//!
//! Only the 64-bit hash, seeded and not, is ported: RocksDB calls nothing else in the header,
//! and its streaming API was removed there [R util/xxph3.h:1742-1744].
//!
//! Every read below is of 4 or 8 bytes at an offset inside the input or the 192-byte secret
//! for the input's length class; [`read64`] and [`read32`] return 0 for an offset outside,
//! which no length reaches (each class is covered by the golden vectors).

use crate::util::math128::{lower64of128, multiply64to128, upper64of128};

/// The 192-byte default secret [R util/xxph3.h:920-935], the released XXH3's too
/// [R util/xxhash.h:3651-3664] (`util::xxhash`).
pub(crate) const SECRET: [u8; SECRET_DEFAULT_SIZE] = [
    0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c,
    0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb, 0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f,
    0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
    0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c,
    0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb, 0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3,
    0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
    0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d,
    0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31, 0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64,
    0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
    0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e,
    0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc, 0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce,
    0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
];

/// The default secret's length [R util/xxph3.h:914].
pub(crate) const SECRET_DEFAULT_SIZE: usize = 192;
/// The smallest secret the algorithm accepts; the mid-size tail reads relative to it
/// [R util/xxph3.h:283].
const SECRET_SIZE_MIN: usize = 136;

const PRIME32_1: u32 = 0x9E37_79B1; // [R util/xxph3.h:564]
const PRIME32_2: u32 = 0x85EB_CA77; // [R :565]
const PRIME32_3: u32 = 0xC2B2_AE3D; // [R :566]
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87; // [R :642]
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F; // [R :643]
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9; // [R :644]
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63; // [R :645]
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5; // [R :646]

/// Bytes hashed per accumulation round [R util/xxph3.h:1145].
const STRIPE_LEN: usize = 64;
/// Secret bytes consumed per stripe [R util/xxph3.h:1146].
const SECRET_CONSUME_RATE: usize = 8;
/// Inputs up to this length take the mid-size path [R util/xxph3.h:1679].
const MIDSIZE_MAX: usize = 240;
/// Secret offsets of the mid-size path's later rounds and its last 16 bytes
/// [R util/xxph3.h:1689-1690].
const MIDSIZE_STARTOFFSET: usize = 3;
const MIDSIZE_LASTOFFSET: usize = 17;
/// Secret offset of the long path's last stripe, and of the accumulators' merge
/// [R util/xxph3.h:1541, :1580].
const SECRET_LASTACC_START: usize = 7;
const SECRET_MERGEACCS_START: usize = 11;

/// Stripes per block, and bytes per block, with the default secret
/// [R util/xxph3.h:1519-1520].
const STRIPES_PER_BLOCK: usize = (SECRET_DEFAULT_SIZE - STRIPE_LEN) / SECRET_CONSUME_RATE;
const BLOCK_LEN: usize = STRIPE_LEN * STRIPES_PER_BLOCK;

/// `XXPH_readLE64` at `at`.
pub(crate) fn read64(bytes: &[u8], at: usize) -> u64 {
    bytes
        .get(at..)
        .and_then(<[u8]>::first_chunk::<8>)
        .map_or(0, |b| u64::from_le_bytes(*b))
}

/// `XXPH_readLE32` at `at`.
pub(crate) fn read32(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map_or(0, |b| u32::from_le_bytes(*b))
}

/// The input's length as the hash mixes it; usize is at most 64 bits on every target.
fn len64(input: &[u8]) -> u64 {
    input.len() as u64
}

/// `XXPH3_mul128_fold64` [R util/xxph3.h:1061-1066].
pub(crate) fn mul128_fold64(lhs: u64, rhs: u64) -> u64 {
    let product = multiply64to128(lhs, rhs);
    lower64of128(product) ^ upper64of128(product)
}

/// `XXPH3_avalanche` [R util/xxph3.h:1069-1075].
fn avalanche(mut h64: u64) -> u64 {
    h64 ^= h64 >> 37;
    h64 = h64.wrapping_mul(PRIME64_3);
    h64 ^ (h64 >> 32)
}

/// `XXPH3_len_1to3_64b` [R util/xxph3.h:1082-1096].
fn len_1to3(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    // c1 = input[0], c2 = input[len >> 1], c3 = input[len - 1].
    let (c1, c2, c3) = match *input {
        [a] => (a, a, a),
        [a, b] => (a, b, b),
        [a, b, c, ..] => (a, b, c),
        [] => (0, 0, 0),
    };
    let len = u32::try_from(input.len()).unwrap_or(0);
    let combined =
        u32::from(c1) | (u32::from(c2) << 8) | (u32::from(c3) << 16) | len.wrapping_shl(24);
    let keyed = u64::from(combined) ^ u64::from(read32(secret, 0)).wrapping_add(seed);
    avalanche(keyed.wrapping_mul(PRIME64_1))
}

/// `XXPH3_len_4to8_64b` [R util/xxph3.h:1098-1111].
fn len_4to8(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let len = input.len();
    let input_lo = read32(input, 0);
    let input_hi = read32(input, len.wrapping_sub(4));
    let input64 = u64::from(input_lo) | (u64::from(input_hi) << 32);
    let keyed = input64 ^ read64(secret, 0).wrapping_add(seed);
    let mix64 =
        len64(input).wrapping_add((keyed ^ (keyed >> 51)).wrapping_mul(u64::from(PRIME32_1)));
    avalanche((mix64 ^ (mix64 >> 47)).wrapping_mul(PRIME64_2))
}

/// `XXPH3_len_9to16_64b` [R util/xxph3.h:1113-1124].
fn len_9to16(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let input_lo = read64(input, 0) ^ read64(secret, 0).wrapping_add(seed);
    let input_hi =
        read64(input, input.len().wrapping_sub(8)) ^ read64(secret, 8).wrapping_sub(seed);
    let acc = len64(input)
        .wrapping_add(input_lo.wrapping_add(input_hi))
        .wrapping_add(mul128_fold64(input_lo, input_hi));
    avalanche(acc)
}

/// `XXPH3_len_0to16_64b` [R util/xxph3.h:1126-1139], with RocksDB's change for empty input:
/// a hash of the seed, not zero.
fn len_0to16(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    match input.len() {
        9.. => len_9to16(input, secret, seed),
        4.. => len_4to8(input, secret, seed),
        1.. => len_1to3(input, secret, seed),
        0 => mul128_fold64(seed.wrapping_add(read64(secret, 0)), PRIME64_2),
    }
}

/// `XXPH3_mix16B` [R util/xxph3.h:1640-1648] over 16 bytes of input at `at` and of secret at
/// `secret_at`.
fn mix16b(input: &[u8], at: usize, secret: &[u8], secret_at: usize, seed: u64) -> u64 {
    let input_lo = read64(input, at);
    let input_hi = read64(input, at.wrapping_add(8));
    mul128_fold64(
        input_lo ^ read64(secret, secret_at).wrapping_add(seed),
        input_hi ^ read64(secret, secret_at.wrapping_add(8)).wrapping_sub(seed),
    )
}

/// `XXPH3_len_17to128_64b` [R util/xxph3.h:1651-1676].
fn len_17to128(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let len = input.len();
    // The offset `back` bytes before the end; `back <= 64 < len` in every call.
    let end = |back: usize| len.wrapping_sub(back);
    let mut acc = len64(input).wrapping_mul(PRIME64_1);
    if len > 32 {
        if len > 64 {
            if len > 96 {
                acc = acc.wrapping_add(mix16b(input, 48, secret, 96, seed));
                acc = acc.wrapping_add(mix16b(input, end(64), secret, 112, seed));
            }
            acc = acc.wrapping_add(mix16b(input, 32, secret, 64, seed));
            acc = acc.wrapping_add(mix16b(input, end(48), secret, 80, seed));
        }
        acc = acc.wrapping_add(mix16b(input, 16, secret, 32, seed));
        acc = acc.wrapping_add(mix16b(input, end(32), secret, 48, seed));
    }
    acc = acc.wrapping_add(mix16b(input, 0, secret, 0, seed));
    acc = acc.wrapping_add(mix16b(input, end(16), secret, 16, seed));
    avalanche(acc)
}

/// `XXPH3_len_129to240_64b` [R util/xxph3.h:1681-1706].
fn len_129to240(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let mut acc = len64(input).wrapping_mul(PRIME64_1);
    let (rounds, _) = input.as_chunks::<16>();
    let mut rounds = rounds.iter();
    // The first 8 rounds read the secret from 0; `len > 128`, so all 8 exist.
    for (i, _) in rounds.by_ref().take(8).enumerate() {
        let at = i.wrapping_mul(16);
        acc = acc.wrapping_add(mix16b(input, at, secret, at, seed));
    }
    acc = avalanche(acc);
    // The later rounds read the secret from 3, restarting at round 8.
    for (i, _) in rounds.enumerate() {
        let at = i.wrapping_add(8).wrapping_mul(16);
        let secret_at = i.wrapping_mul(16).wrapping_add(MIDSIZE_STARTOFFSET);
        acc = acc.wrapping_add(mix16b(input, at, secret, secret_at, seed));
    }
    let last = input.len().wrapping_sub(16);
    acc = acc.wrapping_add(mix16b(
        input,
        last,
        secret,
        SECRET_SIZE_MIN - MIDSIZE_LASTOFFSET,
        seed,
    ));
    avalanche(acc)
}

/// `XXPH3_accumulate_512`, scalar, 64-bit accumulators [R util/xxph3.h:1322-1340]: one stripe
/// of input against the secret at `secret`.
fn accumulate_512(acc: &mut [u64; 8], stripe: &[u8], secret: &[u8]) {
    for ((lane, data), key) in acc
        .iter_mut()
        .zip(stripe.as_chunks::<8>().0)
        .zip(secret.as_chunks::<8>().0)
    {
        let data_val = u64::from_le_bytes(*data);
        let data_key = data_val ^ u64::from_le_bytes(*key);
        *lane = lane
            .wrapping_add(data_val)
            .wrapping_add((data_key & 0xFFFF_FFFF).wrapping_mul(data_key >> 32));
    }
}

/// `XXPH3_scrambleAcc`, scalar [R util/xxph3.h:1464-1477].
fn scramble(acc: &mut [u64; 8], secret: &[u8]) {
    for (lane, key) in acc.iter_mut().zip(secret.as_chunks::<8>().0) {
        let mut a = *lane;
        a ^= a >> 47;
        a ^= u64::from_le_bytes(*key);
        *lane = a.wrapping_mul(u64::from(PRIME32_1));
    }
}

/// `XXPH3_accumulate` [R util/xxph3.h:1485-1500]: each full stripe of `data` against the secret
/// advanced 8 bytes per stripe.
fn accumulate(acc: &mut [u64; 8], data: &[u8], secret: &[u8]) {
    let mut key = secret;
    for stripe in data.as_chunks::<STRIPE_LEN>().0 {
        accumulate_512(acc, stripe, key);
        key = key.get(SECRET_CONSUME_RATE..).unwrap_or(&[]);
    }
}

/// `XXPH3_mergeAccs` [R util/xxph3.h:1554-1565].
fn merge_accs(acc: &[u64; 8], secret: &[u8], start: u64) -> u64 {
    let mut result = start;
    for (pair, key) in acc
        .as_chunks::<2>()
        .0
        .iter()
        .zip(secret.as_chunks::<16>().0)
    {
        let [a0, a1] = *pair;
        result = result.wrapping_add(mul128_fold64(a0 ^ read64(key, 0), a1 ^ read64(key, 8)));
    }
    avalanche(result)
}

/// `XXPH3_hashLong_internal` with its loop [R util/xxph3.h:1510-1583], for `len > 240` and a
/// 192-byte secret.
fn hash_long(input: &[u8], secret: &[u8; SECRET_DEFAULT_SIZE]) -> u64 {
    let mut acc: [u64; 8] = [
        u64::from(PRIME32_3),
        PRIME64_1,
        PRIME64_2,
        PRIME64_3,
        PRIME64_4,
        u64::from(PRIME32_2),
        PRIME64_5,
        u64::from(PRIME32_1),
    ];
    let scramble_key = secret
        .get(SECRET_DEFAULT_SIZE - STRIPE_LEN..)
        .unwrap_or(&[]);
    let (blocks, partial) = input.as_chunks::<BLOCK_LEN>();
    for block in blocks {
        accumulate(&mut acc, block, secret);
        scramble(&mut acc, scramble_key);
    }
    accumulate(&mut acc, partial, secret);
    if !input.len().is_multiple_of(STRIPE_LEN) {
        let last_stripe = input
            .get(input.len().wrapping_sub(STRIPE_LEN)..)
            .unwrap_or(&[]);
        let key = secret
            .get(SECRET_DEFAULT_SIZE - STRIPE_LEN - SECRET_LASTACC_START..)
            .unwrap_or(&[]);
        accumulate_512(&mut acc, last_stripe, key);
    }
    let merge_key = secret.get(SECRET_MERGEACCS_START..).unwrap_or(&[]);
    merge_accs(&acc, merge_key, len64(input).wrapping_mul(PRIME64_1))
}

/// `XXPH3_initCustomSecret` [R util/xxph3.h:1609-1620]: the default secret with `seed` added
/// to the first and subtracted from the second word of every 16 bytes.
fn custom_secret(seed: u64) -> [u8; SECRET_DEFAULT_SIZE] {
    let mut out = SECRET;
    for (dst, src) in out
        .as_chunks_mut::<16>()
        .0
        .iter_mut()
        .zip(SECRET.as_chunks::<16>().0)
    {
        let lo = read64(src, 0).wrapping_add(seed).to_le_bytes();
        let hi = read64(src, 8).wrapping_sub(seed).to_le_bytes();
        for (d, s) in dst.iter_mut().zip(lo.iter().chain(hi.iter())) {
            *d = *s;
        }
    }
    out
}

/// `XXPH3_64bits_withSeed` [R util/xxph3.h:1733-1740]: RocksDB's `Hash64(data, n, seed)`.
pub fn xxph3_64bits_with_seed(input: &[u8], seed: u64) -> u64 {
    match input.len() {
        0..=16 => len_0to16(input, &SECRET, seed),
        17..=128 => len_17to128(input, &SECRET, seed),
        129..=MIDSIZE_MAX => len_129to240(input, &SECRET, seed),
        // XXPH3_hashLong_64b_withSeed [R util/xxph3.h:1630-1636]: seed 0 keeps the default.
        _ if seed == 0 => hash_long(input, &SECRET),
        _ => hash_long(input, &custom_secret(seed)),
    }
}

/// `XXPH3_64bits` [R util/xxph3.h:1711-1717]: RocksDB's `Hash64(data, n)`, the same as seed 0.
pub fn xxph3_64bits(input: &[u8]) -> u64 {
    xxph3_64bits_with_seed(input, 0)
}
