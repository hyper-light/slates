//! The released xxHash algorithms RocksDB vendors as `util/xxhash.h` (xxHash 0.8.1, symbols
//! prefixed `ROCKSDB_`) [R util/xxhash.h:11-13, :474-476]: XXH32, XXH64, XXH3-64 and XXH3-128,
//! converted from that header's scalar code (docs/research/24 §1.2), which every SIMD path in it
//! computes the same as. They key the block checksums (`table::format`) and `Hash128`/`Hash2x64`
//! (`util::hash`). `Hash64`, which keys filters, is not one of these: it is the XXH3 preview in
//! `util::xxph3`, whose secret and 64-bit primitives this module shares and whose mixing differs.
//!
//! The engine's own, not a dependency (docs/design/engine.md §5); `twox-hash` is the test oracle
//! (`tests/xxhash_test.rs`), with the golden vectors of `tests/golden.rs` from RocksDB's C++.
//!
//! Every read below is of 4 or 8 bytes at an offset inside the input or the 192-byte secret for
//! the input's length class; `read64` and `read32` return 0 for an offset outside, which no length
//! reaches.

use crate::util::math128::{lower64of128, multiply64to128, upper64of128};
use crate::util::xxph3::{SECRET, SECRET_DEFAULT_SIZE, mul128_fold64, read32, read64};

/// XXH32's primes [R util/xxhash.h:2219-2223].
const PRIME32_1: u32 = 0x9E37_79B1;
const PRIME32_2: u32 = 0x85EB_CA77;
const PRIME32_3: u32 = 0xC2B2_AE3D;
const PRIME32_4: u32 = 0x27D4_EB2F;
const PRIME32_5: u32 = 0x1656_67B1;
/// XXH64's primes [R util/xxhash.h:2749-2753].
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;
/// The multipliers of `XXH3_avalanche` and of `XXH3_rrmxmx` (also `XXH3_len_4to8_128b`'s), literals
/// in xxHash 0.8.1 [R util/xxhash.h:3870, :3884, :5786].
const PRIME_MX1: u64 = 0x1656_6791_9E37_79F9;
const PRIME_MX2: u64 = 0x9FB2_1C65_1E98_DF25;

/// XXH32's stripe, four lanes of 4 bytes [R util/xxhash.h:2415-2445].
const STRIPE32: usize = 16;
/// XXH64's stripe, four lanes of 8 bytes [R util/xxhash.h:2854-2885].
const STRIPE64: usize = 32;
/// XXH3's stripe: eight accumulator lanes of 8 bytes [R util/xxhash.h:4151].
const STRIPE_LEN: usize = 64;
/// Secret bytes XXH3 consumes per stripe [R util/xxhash.h:4152].
const SECRET_CONSUME_RATE: usize = 8;
/// The smallest secret XXH3 accepts; the mid-size tail reads relative to it
/// [R util/xxhash.h:968].
const SECRET_SIZE_MIN: usize = 136;
/// Inputs up to this length take the mid-size path [R util/xxhash.h:4087].
const MIDSIZE_MAX: usize = 240;
/// Secret offsets of the mid-size path's later rounds and its last 16 bytes
/// [R util/xxhash.h:4097-4098].
const MIDSIZE_STARTOFFSET: usize = 3;
const MIDSIZE_LASTOFFSET: usize = 17;
/// Secret offset of the long path's last stripe [R util/xxhash.h:5157].
const SECRET_LASTACC_START: usize = 7;
/// Secret offset of the accumulators' merge [R util/xxhash.h:5213].
const SECRET_MERGEACCS_START: usize = 11;
/// Stripes per block, and bytes per block, with the default secret
/// [R util/xxhash.h:5136-5137] (the default secret is 192 bytes [R util/xxhash.h:3644]).
const STRIPES_PER_BLOCK: usize = (SECRET_DEFAULT_SIZE - STRIPE_LEN) / SECRET_CONSUME_RATE;
const BLOCK_LEN: usize = STRIPE_LEN * STRIPES_PER_BLOCK;

/// A length as the hashes mix it; usize is at most 64 bits on every target.
fn len64(len: usize) -> u64 {
    len as u64
}

/// The low 32 bits, as the C++'s narrowing casts take them.
fn low32(x: u64) -> u32 {
    u32::try_from(x & 0xFFFF_FFFF).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// XXH32 [R util/xxhash.h:2219-2470]

/// `XXH32_round` [R util/xxhash.h:2244-2275].
fn round32(acc: u32, lane: u32) -> u32 {
    acc.wrapping_add(lane.wrapping_mul(PRIME32_2))
        .rotate_left(13)
        .wrapping_mul(PRIME32_1)
}

/// `XXH32_avalanche` [R util/xxhash.h:2298-2306].
fn avalanche32(mut hash: u32) -> u32 {
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(PRIME32_2);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(PRIME32_3);
    hash ^ (hash >> 16)
}

/// `XXH32_endian_align` and `XXH32_finalize` [R util/xxhash.h:2415-2445, :2326-2390]: the hash of `total`
/// bytes, the whole stripes `stripes` and then `rest`, fewer than a stripe.
fn xxh32_core<'a>(
    stripes: impl Iterator<Item = &'a [u8; STRIPE32]>,
    rest: &[u8],
    total: usize,
    seed: u32,
) -> u32 {
    // Whole stripes exist exactly when the length reaches one.
    let mut hash = if total >= STRIPE32 {
        let mut v = [
            seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2),
            seed.wrapping_add(PRIME32_2),
            seed,
            seed.wrapping_sub(PRIME32_1),
        ];
        for stripe in stripes {
            for (acc, lane) in v.iter_mut().zip(stripe.as_chunks::<4>().0) {
                *acc = round32(*acc, u32::from_le_bytes(*lane));
            }
        }
        let [v1, v2, v3, v4] = v;
        v1.rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18))
    } else {
        seed.wrapping_add(PRIME32_5)
    };
    // The length mod 2^32, as the C++ adds a size_t to a 32-bit hash.
    hash = hash.wrapping_add(low32(len64(total)));
    let (words, bytes) = rest.as_chunks::<4>();
    for word in words {
        hash = hash
            .wrapping_add(u32::from_le_bytes(*word).wrapping_mul(PRIME32_3))
            .rotate_left(17)
            .wrapping_mul(PRIME32_4);
    }
    for byte in bytes {
        hash = hash
            .wrapping_add(u32::from(*byte).wrapping_mul(PRIME32_5))
            .rotate_left(11)
            .wrapping_mul(PRIME32_1);
    }
    avalanche32(hash)
}

/// `XXH32(data, len, seed)`.
pub fn xxh32(data: &[u8], seed: u32) -> u32 {
    let (stripes, rest) = data.as_chunks::<STRIPE32>();
    xxh32_core(stripes.iter(), rest, data.len(), seed)
}

/// XXH32 over `data ‖ last_byte` without joining them: RocksDB's streamed
/// `XXH32_update(data); XXH32_update(&last_byte, 1)` [R table/format.cc:652-660].
pub fn xxh32_with_last_byte(data: &[u8], last_byte: u8, seed: u32) -> u32 {
    let (stripes, partial) = data.as_chunks::<STRIPE32>();
    let mut tail = [0u8; STRIPE32];
    let filled = with_byte(&mut tail, partial, last_byte);
    let total = data.len().saturating_add(1);
    if filled.len() == STRIPE32 {
        xxh32_core(stripes.iter().chain([&tail]), &[], total, seed)
    } else {
        xxh32_core(stripes.iter().chain([]), filled, total, seed)
    }
}

// ---------------------------------------------------------------------------------------------
// XXH64 [R util/xxhash.h:2749-2890]

/// `XXH64_round` [R util/xxhash.h:2764-2770].
fn round64(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

/// `XXH64_mergeRound` [R util/xxhash.h:2772-2778].
fn merge_round64(acc: u64, val: u64) -> u64 {
    (acc ^ round64(0, val))
        .wrapping_mul(PRIME64_1)
        .wrapping_add(PRIME64_4)
}

/// `XXH64_avalanche` [R util/xxhash.h:2781-2790].
fn avalanche64(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME64_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME64_3);
    hash ^ (hash >> 32)
}

/// `XXH64_endian_align` and `XXH64_finalize` [R util/xxhash.h:2854-2885, :2810-2850]: as
/// [`xxh32_core`].
fn xxh64_core<'a>(
    stripes: impl Iterator<Item = &'a [u8; STRIPE64]>,
    rest: &[u8],
    total: usize,
    seed: u64,
) -> u64 {
    let hash = if total >= STRIPE64 {
        let mut v = [
            seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2),
            seed.wrapping_add(PRIME64_2),
            seed,
            seed.wrapping_sub(PRIME64_1),
        ];
        for stripe in stripes {
            for (acc, lane) in v.iter_mut().zip(stripe.as_chunks::<8>().0) {
                *acc = round64(*acc, u64::from_le_bytes(*lane));
            }
        }
        let [v1, v2, v3, v4] = v;
        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        for lane in v {
            h = merge_round64(h, lane);
        }
        h
    } else {
        seed.wrapping_add(PRIME64_5)
    };
    finalize64(hash.wrapping_add(len64(total)), rest)
}

/// `XXH64_finalize` [R util/xxhash.h:2810-2850]: the bytes after the last whole stripe, then the
/// avalanche.
fn finalize64(mut hash: u64, rest: &[u8]) -> u64 {
    let (words, after) = rest.as_chunks::<8>();
    for word in words {
        hash ^= round64(0, u64::from_le_bytes(*word));
        hash = hash
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
    }
    let (halves, bytes) = after.as_chunks::<4>();
    for half in halves {
        hash ^= u64::from(u32::from_le_bytes(*half)).wrapping_mul(PRIME64_1);
        hash = hash
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
    }
    for byte in bytes {
        hash ^= u64::from(*byte).wrapping_mul(PRIME64_5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME64_1);
    }
    avalanche64(hash)
}

/// `XXH64(data, len, seed)`.
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let (stripes, rest) = data.as_chunks::<STRIPE64>();
    xxh64_core(stripes.iter(), rest, data.len(), seed)
}

/// XXH64 over `data ‖ last_byte`, streamed as [`xxh32_with_last_byte`]
/// [R table/format.cc:662-670].
pub fn xxh64_with_last_byte(data: &[u8], last_byte: u8, seed: u64) -> u64 {
    let (stripes, partial) = data.as_chunks::<STRIPE64>();
    let mut tail = [0u8; STRIPE64];
    let filled = with_byte(&mut tail, partial, last_byte);
    let total = data.len().saturating_add(1);
    if filled.len() == STRIPE64 {
        xxh64_core(stripes.iter().chain([&tail]), &[], total, seed)
    } else {
        xxh64_core(stripes.iter().chain([]), filled, total, seed)
    }
}

/// XXH64 over input given in pieces: `XXH64_reset`, `XXH64_update` and `XXH64_digest`
/// [R util/xxhash.h:2931-3012], the stripes taken as whole ones arrive and the last partial one
/// held, so the digest equals [`xxh64`] over the pieces joined.
#[derive(Clone, Debug)]
pub struct Xxh64 {
    seed: u64,
    acc: [u64; 4],
    /// Whole stripes taken so far.
    stripes: u64,
    partial: [u8; STRIPE64],
    held: usize,
    total: u64,
}

impl Xxh64 {
    /// A state for `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            acc: [
                seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2),
                seed.wrapping_add(PRIME64_2),
                seed,
                seed.wrapping_sub(PRIME64_1),
            ],
            stripes: 0,
            partial: [0; STRIPE64],
            held: 0,
            total: 0,
        }
    }

    fn stripe(&mut self, stripe: &[u8; STRIPE64]) {
        for (acc, lane) in self.acc.iter_mut().zip(stripe.as_chunks::<8>().0) {
            *acc = round64(*acc, u64::from_le_bytes(*lane));
        }
        self.stripes = self.stripes.wrapping_add(1);
    }

    /// Takes `data`.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(len64(data.len()));
        if self.held > 0 {
            let need = STRIPE64.saturating_sub(self.held);
            let take = need.min(data.len());
            let (head, rest) = data.split_at(take);
            if let Some(dst) = self
                .partial
                .get_mut(self.held..self.held.saturating_add(take))
            {
                dst.copy_from_slice(head);
            }
            self.held = self.held.saturating_add(take);
            data = rest;
            if self.held < STRIPE64 {
                return;
            }
            let full = self.partial;
            self.stripe(&full);
            self.held = 0;
        }
        let (stripes, rest) = data.as_chunks::<STRIPE64>();
        for stripe in stripes {
            self.stripe(stripe);
        }
        if let Some(dst) = self.partial.get_mut(..rest.len()) {
            dst.copy_from_slice(rest);
        }
        self.held = rest.len();
    }

    /// The hash of everything taken.
    pub fn digest(&self) -> u64 {
        let mut hash = if self.stripes > 0 {
            let [v1, v2, v3, v4] = self.acc;
            let mut h = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            for lane in self.acc {
                h = merge_round64(h, lane);
            }
            h
        } else {
            self.seed.wrapping_add(PRIME64_5)
        };
        hash = hash.wrapping_add(self.total);
        finalize64(hash, self.partial.get(..self.held).unwrap_or(&[]))
    }
}

/// `partial ‖ byte` in `buf`, `partial` being shorter than `buf`: the filled prefix.
fn with_byte<'b, const N: usize>(buf: &'b mut [u8; N], partial: &[u8], byte: u8) -> &'b [u8] {
    let n = partial.len();
    if let Some(head) = buf.get_mut(..n) {
        head.copy_from_slice(partial);
    }
    if let Some(slot) = buf.get_mut(n) {
        *slot = byte;
    }
    buf.get(..n.saturating_add(1)).unwrap_or(&[])
}

// ---------------------------------------------------------------------------------------------
// XXH3 [R util/xxhash.h:3860-6145]

/// `XXH3_avalanche` [R util/xxhash.h:3867-3873].
fn avalanche(mut h64: u64) -> u64 {
    h64 ^= h64 >> 37;
    h64 = h64.wrapping_mul(PRIME_MX1);
    h64 ^ (h64 >> 32)
}

/// `XXH3_rrmxmx` [R util/xxhash.h:3880-3887].
fn rrmxmx(mut h64: u64, len: u64) -> u64 {
    h64 ^= h64.rotate_left(49) ^ h64.rotate_left(24);
    h64 = h64.wrapping_mul(PRIME_MX2);
    h64 ^= (h64 >> 35).wrapping_add(len);
    h64 = h64.wrapping_mul(PRIME_MX2);
    h64 ^ (h64 >> 28)
}

/// A byte of the input at `at`, 0 outside it.
fn byte(input: &[u8], at: usize) -> u32 {
    input.get(at).map_or(0, |b| u32::from(*b))
}

/// `XXH3_len_1to3_64b` [R util/xxhash.h:3925-3944].
fn len_1to3_64(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let len = input.len();
    let combined = (byte(input, 0) << 16)
        | (byte(input, len >> 1) << 24)
        | byte(input, len.wrapping_sub(1))
        | (low32(len64(len)) << 8);
    let bitflip = u64::from(read32(secret, 0) ^ read32(secret, 4)).wrapping_add(seed);
    avalanche64(u64::from(combined) ^ bitflip)
}

/// `XXH3_len_4to8_64b` [R util/xxhash.h:3947-3960].
fn len_4to8_64(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let len = input.len();
    let seed = seed ^ (u64::from(low32(seed).swap_bytes()) << 32);
    let input1 = read32(input, 0);
    let input2 = read32(input, len.wrapping_sub(4));
    let bitflip = (read64(secret, 8) ^ read64(secret, 16)).wrapping_sub(seed);
    let input64 = u64::from(input2).wrapping_add(u64::from(input1) << 32);
    rrmxmx(input64 ^ bitflip, len64(len))
}

/// `XXH3_len_9to16_64b` [R util/xxhash.h:3963-3977].
fn len_9to16_64(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let len = input.len();
    let bitflip1 = (read64(secret, 24) ^ read64(secret, 32)).wrapping_add(seed);
    let bitflip2 = (read64(secret, 40) ^ read64(secret, 48)).wrapping_sub(seed);
    let input_lo = read64(input, 0) ^ bitflip1;
    let input_hi = read64(input, len.wrapping_sub(8)) ^ bitflip2;
    let acc = len64(len)
        .wrapping_add(input_lo.swap_bytes())
        .wrapping_add(input_hi)
        .wrapping_add(mul128_fold64(input_lo, input_hi));
    avalanche(acc)
}

/// `XXH3_len_0to16_64b` [R util/xxhash.h:3980-3989].
fn len_0to16_64(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    match input.len() {
        9.. => len_9to16_64(input, secret, seed),
        4.. => len_4to8_64(input, secret, seed),
        1.. => len_1to3_64(input, secret, seed),
        0 => avalanche64(seed ^ (read64(secret, 56) ^ read64(secret, 64))),
    }
}

/// `XXH3_mix16B` [R util/xxhash.h:4016-4032]: 16 input bytes at `at` against the secret at
/// `secret_at`.
fn mix16b(input: &[u8], at: usize, secret: &[u8], secret_at: usize, seed: u64) -> u64 {
    let input_lo = read64(input, at);
    let input_hi = read64(input, at.wrapping_add(8));
    mul128_fold64(
        input_lo ^ read64(secret, secret_at).wrapping_add(seed),
        input_hi ^ read64(secret, secret_at.wrapping_add(8)).wrapping_sub(seed),
    )
}

/// `XXH3_len_17to128_64b` [R util/xxhash.h:4050-4085].
fn len_17to128_64(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let len = input.len();
    let mut acc = len64(len).wrapping_mul(PRIME64_1);
    let mut acc_end = 0u64;
    // Rounds from the outside in, as many as the length reaches past 16, 32, 64 and 96.
    for (round, past) in [(0usize, 0usize), (1, 32), (2, 64), (3, 96)] {
        if len <= past {
            break;
        }
        acc = acc.wrapping_add(mix16b(
            input,
            round.wrapping_mul(16),
            secret,
            round.wrapping_mul(32),
            seed,
        ));
        acc_end = acc_end.wrapping_add(mix16b(
            input,
            len.wrapping_sub(round.wrapping_add(1).wrapping_mul(16)),
            secret,
            round.wrapping_mul(32).wrapping_add(16),
            seed,
        ));
    }
    avalanche(acc.wrapping_add(acc_end))
}

/// `XXH3_len_129to240_64b` [R util/xxhash.h:4090-4145].
fn len_129to240_64(input: &[u8], secret: &[u8], seed: u64) -> u64 {
    let len = input.len();
    let mut acc = len64(len).wrapping_mul(PRIME64_1);
    for i in 0..8usize {
        acc = acc.wrapping_add(mix16b(
            input,
            i.wrapping_mul(16),
            secret,
            i.wrapping_mul(16),
            seed,
        ));
    }
    let mut acc_end = mix16b(
        input,
        len.wrapping_sub(16),
        secret,
        SECRET_SIZE_MIN - MIDSIZE_LASTOFFSET,
        seed,
    );
    acc = avalanche(acc);
    for i in 8..len / 16 {
        acc_end = acc_end.wrapping_add(mix16b(
            input,
            i.wrapping_mul(16),
            secret,
            i.wrapping_sub(8)
                .wrapping_mul(16)
                .wrapping_add(MIDSIZE_STARTOFFSET),
            seed,
        ));
    }
    avalanche(acc.wrapping_add(acc_end))
}

/// `XXH3_accumulate_512`, scalar [R util/xxhash.h:4919-4952]: one stripe against the secret at
/// `secret`; each lane's input is also added to its neighbour.
fn accumulate_512(acc: &mut [u64; 8], stripe: &[u8], secret: &[u8]) {
    let mut lanes = [0u64; 8];
    let mut keyed = [0u64; 8];
    for (((lane, key), data), k) in lanes
        .iter_mut()
        .zip(keyed.iter_mut())
        .zip(stripe.as_chunks::<8>().0)
        .zip(secret.as_chunks::<8>().0)
    {
        *lane = u64::from_le_bytes(*data);
        *key = *lane ^ u64::from_le_bytes(*k);
    }
    for (i, slot) in acc.iter_mut().enumerate() {
        // The neighbour's input, `xacc[lane ^ 1] += data_val`.
        let swapped = lanes.get(i ^ 1).copied().unwrap_or(0);
        let key = keyed.get(i).copied().unwrap_or(0);
        *slot = slot
            .wrapping_add(swapped)
            .wrapping_add((key & 0xFFFF_FFFF).wrapping_mul(key >> 32));
    }
}

/// `XXH3_scrambleAcc`, scalar [R util/xxhash.h:4968-4998].
fn scramble(acc: &mut [u64; 8], secret: &[u8]) {
    for (lane, key) in acc.iter_mut().zip(secret.as_chunks::<8>().0) {
        let mut a = *lane;
        a ^= a >> 47;
        a ^= u64::from_le_bytes(*key);
        *lane = a.wrapping_mul(u64::from(PRIME32_1));
    }
}

/// `XXH3_accumulate` [R util/xxhash.h:4182-4198]: `stripes` stripes of `data` against the secret
/// advanced 8 bytes per stripe.
fn accumulate(acc: &mut [u64; 8], data: &[u8], secret: &[u8], stripes: usize) {
    for (n, stripe) in data
        .as_chunks::<STRIPE_LEN>()
        .0
        .iter()
        .take(stripes)
        .enumerate()
    {
        let key = secret
            .get(n.wrapping_mul(SECRET_CONSUME_RATE)..)
            .unwrap_or(&[]);
        accumulate_512(acc, stripe, key);
    }
}

/// `XXH3_hashLong_internal_loop` [R util/xxhash.h:5130-5158], for `len > 240` and a 192-byte
/// secret: every stripe but the last whole, then the last 64 bytes against the secret's end.
fn hash_long_loop(acc: &mut [u64; 8], input: &[u8], secret: &[u8; SECRET_DEFAULT_SIZE]) {
    let len = input.len();
    let blocks = len.wrapping_sub(1) / BLOCK_LEN;
    let scramble_key = secret
        .get(SECRET_DEFAULT_SIZE - STRIPE_LEN..)
        .unwrap_or(&[]);
    for block in input.as_chunks::<BLOCK_LEN>().0.iter().take(blocks) {
        accumulate(acc, block, secret, STRIPES_PER_BLOCK);
        scramble(acc, scramble_key);
    }
    let done = blocks.wrapping_mul(BLOCK_LEN);
    let stripes = len.wrapping_sub(1).wrapping_sub(done) / STRIPE_LEN;
    accumulate(acc, input.get(done..).unwrap_or(&[]), secret, stripes);
    let last = input.get(len.wrapping_sub(STRIPE_LEN)..).unwrap_or(&[]);
    let key = secret
        .get(SECRET_DEFAULT_SIZE - STRIPE_LEN - SECRET_LASTACC_START..)
        .unwrap_or(&[]);
    accumulate_512(acc, last, key);
}

/// `XXH3_mergeAccs` [R util/xxhash.h:5171-5195].
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

/// `XXH3_INIT_ACC` [R util/xxhash.h:5197-5198].
fn init_acc() -> [u64; 8] {
    [
        u64::from(PRIME32_3),
        PRIME64_1,
        PRIME64_2,
        PRIME64_3,
        PRIME64_4,
        u64::from(PRIME32_2),
        PRIME64_5,
        u64::from(PRIME32_1),
    ]
}

/// `XXH3_initCustomSecret` [R util/xxhash.h:5001-5066]: the default secret with `seed` added to
/// the first and subtracted from the second word of every 16 bytes.
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

/// The secret of a long input: the default for seed 0, else derived from the seed
/// (`XXH3_hashLong_64b_withSeed_internal` [R util/xxhash.h:5257-5276]).
fn long_secret(seed: u64) -> [u8; SECRET_DEFAULT_SIZE] {
    if seed == 0 {
        SECRET
    } else {
        custom_secret(seed)
    }
}

/// `XXH3_64bits_withSeed(data, len, seed)` [R util/xxhash.h:5293-5305, :5332-5335].
pub fn xxh3_64bits_with_seed(input: &[u8], seed: u64) -> u64 {
    match input.len() {
        0..=16 => len_0to16_64(input, &SECRET, seed),
        17..=128 => len_17to128_64(input, &SECRET, seed),
        129..=MIDSIZE_MAX => len_129to240_64(input, &SECRET, seed),
        len => {
            let secret = long_secret(seed);
            let mut acc = init_acc();
            hash_long_loop(&mut acc, input, &secret);
            let merge_key = secret.get(SECRET_MERGEACCS_START..).unwrap_or(&[]);
            merge_accs(&acc, merge_key, len64(len).wrapping_mul(PRIME64_1))
        }
    }
}

/// `XXH3_64bits(data, len)`: the same as seed 0.
pub fn xxh3_64bits(input: &[u8]) -> u64 {
    xxh3_64bits_with_seed(input, 0)
}

/// An XXH3-128 result as the C++ struct holds it.
#[derive(Clone, Copy)]
struct Hash128 {
    low64: u64,
    high64: u64,
}

impl Hash128 {
    /// `XXH_mult64to128`.
    fn product(lhs: u64, rhs: u64) -> Self {
        let p = multiply64to128(lhs, rhs);
        Self {
            low64: lower64of128(p),
            high64: upper64of128(p),
        }
    }
}

/// `XXH3_len_1to3_128b` [R util/xxhash.h:5738-5764].
fn len_1to3_128(input: &[u8], secret: &[u8], seed: u64) -> Hash128 {
    let len = input.len();
    let combinedl = (byte(input, 0) << 16)
        | (byte(input, len >> 1) << 24)
        | byte(input, len.wrapping_sub(1))
        | (low32(len64(len)) << 8);
    let combinedh = combinedl.swap_bytes().rotate_left(13);
    let bitflipl = u64::from(read32(secret, 0) ^ read32(secret, 4)).wrapping_add(seed);
    let bitfliph = u64::from(read32(secret, 8) ^ read32(secret, 12)).wrapping_sub(seed);
    Hash128 {
        low64: avalanche64(u64::from(combinedl) ^ bitflipl),
        high64: avalanche64(u64::from(combinedh) ^ bitfliph),
    }
}

/// `XXH3_len_4to8_128b` [R util/xxhash.h:5767-5791].
fn len_4to8_128(input: &[u8], secret: &[u8], seed: u64) -> Hash128 {
    let len = input.len();
    let seed = seed ^ (u64::from(low32(seed).swap_bytes()) << 32);
    let input_lo = read32(input, 0);
    let input_hi = read32(input, len.wrapping_sub(4));
    let input_64 = u64::from(input_lo).wrapping_add(u64::from(input_hi) << 32);
    let bitflip = (read64(secret, 16) ^ read64(secret, 24)).wrapping_add(seed);
    let keyed = input_64 ^ bitflip;
    let mut m = Hash128::product(keyed, PRIME64_1.wrapping_add(len64(len) << 2));
    m.high64 = m.high64.wrapping_add(m.low64 << 1);
    m.low64 ^= m.high64 >> 3;
    m.low64 ^= m.low64 >> 35;
    m.low64 = m.low64.wrapping_mul(PRIME_MX2);
    m.low64 ^= m.low64 >> 28;
    m.high64 = avalanche(m.high64);
    m
}

/// `XXH3_len_9to16_128b` [R util/xxhash.h:5794-5866], the 64-bit platforms' form (every target
/// of this repository).
fn len_9to16_128(input: &[u8], secret: &[u8], seed: u64) -> Hash128 {
    let len = input.len();
    let bitflipl = (read64(secret, 32) ^ read64(secret, 40)).wrapping_sub(seed);
    let bitfliph = (read64(secret, 48) ^ read64(secret, 56)).wrapping_add(seed);
    let input_lo = read64(input, 0);
    let mut input_hi = read64(input, len.wrapping_sub(8));
    let mut m = Hash128::product(input_lo ^ input_hi ^ bitflipl, PRIME64_1);
    m.low64 = m.low64.wrapping_add(len64(len).wrapping_sub(1) << 54);
    input_hi ^= bitfliph;
    m.high64 = m.high64.wrapping_add(input_hi).wrapping_add(
        u64::from(low32(input_hi)).wrapping_mul(u64::from(PRIME32_2.wrapping_sub(1))),
    );
    m.low64 ^= m.high64.swap_bytes();
    let mut h = Hash128::product(m.low64, PRIME64_2);
    h.high64 = h.high64.wrapping_add(m.high64.wrapping_mul(PRIME64_2));
    Hash128 {
        low64: avalanche(h.low64),
        high64: avalanche(h.high64),
    }
}

/// `XXH3_len_0to16_128b` [R util/xxhash.h:5869-5885].
fn len_0to16_128(input: &[u8], secret: &[u8], seed: u64) -> Hash128 {
    match input.len() {
        9.. => len_9to16_128(input, secret, seed),
        4.. => len_4to8_128(input, secret, seed),
        1.. => len_1to3_128(input, secret, seed),
        0 => Hash128 {
            low64: avalanche64(seed ^ read64(secret, 64) ^ read64(secret, 72)),
            high64: avalanche64(seed ^ read64(secret, 80) ^ read64(secret, 88)),
        },
    }
}

/// `XXH128_mix32B` [R util/xxhash.h:5888-5897]: 16 bytes at `at1` and 16 at `at2`.
fn mix32b(
    mut acc: Hash128,
    input: &[u8],
    at1: usize,
    at2: usize,
    secret: &[u8],
    secret_at: usize,
    seed: u64,
) -> Hash128 {
    acc.low64 = acc
        .low64
        .wrapping_add(mix16b(input, at1, secret, secret_at, seed));
    acc.low64 ^= read64(input, at2).wrapping_add(read64(input, at2.wrapping_add(8)));
    acc.high64 =
        acc.high64
            .wrapping_add(mix16b(input, at2, secret, secret_at.wrapping_add(16), seed));
    acc.high64 ^= read64(input, at1).wrapping_add(read64(input, at1.wrapping_add(8)));
    acc
}

/// The ending of the mid-size 128-bit paths [R util/xxhash.h:5930-5938, :5989-5997].
fn finish_mid_128(acc: Hash128, len: usize, seed: u64) -> Hash128 {
    let low = acc.low64.wrapping_add(acc.high64);
    let high = acc
        .low64
        .wrapping_mul(PRIME64_1)
        .wrapping_add(acc.high64.wrapping_mul(PRIME64_4))
        .wrapping_add(len64(len).wrapping_sub(seed).wrapping_mul(PRIME64_2));
    Hash128 {
        low64: avalanche(low),
        high64: 0u64.wrapping_sub(avalanche(high)),
    }
}

/// `XXH3_len_17to128_128b` [R util/xxhash.h:5900-5941].
fn len_17to128_128(input: &[u8], secret: &[u8], seed: u64) -> Hash128 {
    let len = input.len();
    let mut acc = Hash128 {
        low64: len64(len).wrapping_mul(PRIME64_1),
        high64: 0,
    };
    // From the inside out: the deepest round the length reaches first.
    for (round, past) in [(3usize, 96usize), (2, 64), (1, 32), (0, 0)] {
        if len > past {
            acc = mix32b(
                acc,
                input,
                round.wrapping_mul(16),
                len.wrapping_sub(round.wrapping_add(1).wrapping_mul(16)),
                secret,
                round.wrapping_mul(32),
                seed,
            );
        }
    }
    finish_mid_128(acc, len, seed)
}

/// `XXH3_len_129to240_128b` [R util/xxhash.h:5944-5999].
fn len_129to240_128(input: &[u8], secret: &[u8], seed: u64) -> Hash128 {
    let len = input.len();
    let mut acc = Hash128 {
        low64: len64(len).wrapping_mul(PRIME64_1),
        high64: 0,
    };
    for i in (32..160usize).step_by(32) {
        let at = i.wrapping_sub(32);
        acc = mix32b(acc, input, at, i.wrapping_sub(16), secret, at, seed);
    }
    acc.low64 = avalanche(acc.low64);
    acc.high64 = avalanche(acc.high64);
    for i in (160..=len).step_by(32) {
        acc = mix32b(
            acc,
            input,
            i.wrapping_sub(32),
            i.wrapping_sub(16),
            secret,
            i.wrapping_sub(160).wrapping_add(MIDSIZE_STARTOFFSET),
            seed,
        );
    }
    acc = mix32b(
        acc,
        input,
        len.wrapping_sub(16),
        len.wrapping_sub(32),
        secret,
        SECRET_SIZE_MIN - MIDSIZE_LASTOFFSET - 16,
        0u64.wrapping_sub(seed),
    );
    finish_mid_128(acc, len, seed)
}

/// `XXH3_128bits_withSeed(data, len, seed)` [R util/xxhash.h:6002-6027, :6087-6100, :6129-6135],
/// as
/// `high64 << 64 | low64`.
pub fn xxh3_128bits_with_seed(input: &[u8], seed: u64) -> u128 {
    let h = match input.len() {
        0..=16 => len_0to16_128(input, &SECRET, seed),
        17..=128 => len_17to128_128(input, &SECRET, seed),
        129..=MIDSIZE_MAX => len_129to240_128(input, &SECRET, seed),
        len => {
            let secret = long_secret(seed);
            let mut acc = init_acc();
            hash_long_loop(&mut acc, input, &secret);
            let low_key = secret.get(SECRET_MERGEACCS_START..).unwrap_or(&[]);
            let high_key = secret
                .get(SECRET_DEFAULT_SIZE - 64 - SECRET_MERGEACCS_START..)
                .unwrap_or(&[]);
            Hash128 {
                low64: merge_accs(&acc, low_key, len64(len).wrapping_mul(PRIME64_1)),
                high64: merge_accs(&acc, high_key, !len64(len).wrapping_mul(PRIME64_2)),
            }
        }
    };
    (u128::from(h.high64) << 64) | u128::from(h.low64)
}
