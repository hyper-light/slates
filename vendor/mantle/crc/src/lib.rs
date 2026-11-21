//! The checksums mantle stores and verifies.
//!
//! CRC-32C (Castagnoli, iSCSI; RFC 3720 §12.1) protects every on-disk record and network
//! payload (docs/research/03 §14). CRC-64/NVME and CRC-32 (ISO-HDLC) are the full-object
//! checksums S3 clients send besides CRC-32C (docs/research/05 §3). All three come from
//! `crc-fast`, which computes them with carry-less multiplication (PCLMULQDQ on x86_64,
//! PMULL on aarch64) selected at run time; on the development machine it computes CRC-32C
//! at ~100 GB/s against ~32 GB/s for the `crc32c` crate's CRC instructions (1 MiB buffers,
//! Apple M5 Max). Only the three predefined algorithms are used: `crc-fast` panics only for
//! custom CRC parameters.
//!
//! Combining CRCs is done here, with zlib's method (crc32.c, `crc32_combine64`):
//! the CRC of `a ‖ b` is `crc(a) · x^(8·|b|) ^ crc(b)` modulo the polynomial, and the power of
//! x comes from a table of x^(2^k). `crc-fast`'s combination rebuilt that operator on every
//! call, which made combining a record's 64 KiB block CRCs cost nine times as much as
//! computing them (profile of the chunk writer, 8 MiB records); [`Crc32cShift`] builds it once
//! for a fixed length, so each combination is a single multiplication.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use crc_fast::{CrcAlgorithm, Digest};

/// CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    crc_fast::crc32_iscsi(data)
}

/// zlib's CRC combination (crc32.c: `multmodp`, `x2nmodp`, `crc32_combine64`) for one
/// bit-reflected CRC whose register starts and ends inverted, as CRC-32, CRC-32C and
/// CRC-64/NVME all do: then `crc(a ‖ b) = crc(a) · x^(8·|b|) ⊕ crc(b)` modulo the polynomial.
macro_rules! reflected_crc {
    ($module:ident, $word:ty, $bits:literal, $poly:literal) => {
        mod $module {
            /// The polynomial, bit-reflected, with its top term implied.
            const POLY: $word = $poly;
            const TOP: $word = 1 << ($bits - 1);

            /// a(x) · b(x) modulo the polynomial; `a` is never zero here, since powers of x are
            /// never zero modulo a polynomial with a constant term.
            pub(crate) const fn multmodp(a: $word, mut b: $word) -> $word {
                let mut m: $word = TOP;
                let mut p: $word = 0;
                while m != 0 {
                    if a & m != 0 {
                        p ^= b;
                        if a & (m.wrapping_sub(1)) == 0 {
                            break;
                        }
                    }
                    m >>= 1;
                    b = if b & 1 != 0 { (b >> 1) ^ POLY } else { b >> 1 };
                }
                p
            }

            /// x^(2^k) modulo the polynomial, for k in 0..bits.
            const X2N: [$word; $bits] = {
                let mut table = [0; $bits];
                let mut p: $word = TOP >> 1; // x^1
                let mut rest: &mut [$word] = &mut table;
                while let Some((slot, tail)) = rest.split_first_mut() {
                    *slot = p;
                    p = multmodp(p, p);
                    rest = tail;
                }
                table
            };

            /// x^(8·len) modulo the polynomial: shifts a CRC past `len` bytes.
            pub(crate) fn shift_operator(len: u64) -> $word {
                let mut p: $word = TOP; // x^0
                let mut n = len;
                let mut k: usize = 3; // 8 = 2^3
                while n != 0 {
                    if n & 1 != 0 {
                        p = multmodp(X2N.get(k % $bits).copied().unwrap_or(TOP), p);
                    }
                    n >>= 1;
                    k = k.wrapping_add(1);
                }
                p
            }

            pub(crate) fn combine(crc_a: $word, crc_b: $word, len_b: u64) -> $word {
                multmodp(shift_operator(len_b), crc_a) ^ crc_b
            }
        }
    };
}

// CRC-32C: 0x1EDC6F41, reflected. CRC-32 (ISO-HDLC, zlib's): 0x04C11DB7, reflected.
// CRC-64/NVME: 0xAD93D23594C93659, reflected.
reflected_crc!(c32c, u32, 32, 0x82F6_3B78);
reflected_crc!(c32, u32, 32, 0xEDB8_8320);
reflected_crc!(c64nvme, u64, 64, 0x9A6C_9329_AC4B_C9B5);

/// The CRC-32C of `a ‖ data`, given `crc32c(a)`: the register resumes from `a`'s state, the
/// complement of its CRC (CRC-32C's final XOR is all ones), so extending costs one pass over
/// `data` where combining would add a shift across it.
pub fn crc32c_extend(crc_a: u32, data: &[u8]) -> u32 {
    let mut digest = Digest::new_with_init_state(CrcAlgorithm::Crc32Iscsi, u64::from(!crc_a));
    digest.update(data);
    u32::try_from(digest.finalize()).unwrap_or(0)
}

/// The CRC-32C of `a ‖ b`, given `crc32c(a)`, `crc32c(b)` and `b.len()`.
pub fn crc32c_combine(crc_a: u32, crc_b: u32, len_b: u64) -> u32 {
    c32c::combine(crc_a, crc_b, len_b)
}

/// CRC-32 (ISO-HDLC, as zlib and S3's `CRC32` compute it) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut digest = Digest::new(CrcAlgorithm::Crc32IsoHdlc);
    digest.update(data);
    u32::try_from(digest.finalize()).unwrap_or(0)
}

/// The CRC-32 of `a ‖ b`, given `crc32(a)`, `crc32(b)` and `b.len()`.
pub fn crc32_combine(crc_a: u32, crc_b: u32, len_b: u64) -> u32 {
    c32::combine(crc_a, crc_b, len_b)
}

/// The CRC-64/NVME of `a ‖ b`, given `crc64nvme(a)`, `crc64nvme(b)` and `b.len()`.
pub fn crc64nvme_combine(crc_a: u64, crc_b: u64, len_b: u64) -> u64 {
    c64nvme::combine(crc_a, crc_b, len_b)
}

/// An incremental CRC-32 (ISO-HDLC).
#[derive(Clone)]
pub struct Crc32(Digest);

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    pub fn new() -> Self {
        Self(Digest::new(CrcAlgorithm::Crc32IsoHdlc))
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(&self) -> u32 {
        u32::try_from(self.0.finalize()).unwrap_or(0)
    }
}

/// Combines CRC-32Cs across a fixed length: built once, each combination is one
/// multiplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32cShift(u32);

impl Crc32cShift {
    /// For appending `len` bytes.
    pub fn new(len: u64) -> Self {
        Self(c32c::shift_operator(len))
    }

    /// The CRC-32C of `a ‖ b`, given `crc32c(a)` and `crc32c(b)`, where `b` is `len` bytes.
    pub fn combine(self, crc_a: u32, crc_b: u32) -> u32 {
        c32c::multmodp(self.0, crc_a) ^ crc_b
    }
}

/// An incremental CRC-32C.
#[derive(Clone)]
pub struct Crc32c(Digest);

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32c {
    pub fn new() -> Self {
        Self(Digest::new(CrcAlgorithm::Crc32Iscsi))
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(&self) -> u32 {
        u32::try_from(self.0.finalize()).unwrap_or(0)
    }
}

/// CRC-64/NVME of `data`.
pub fn crc64nvme(data: &[u8]) -> u64 {
    crc_fast::checksum(CrcAlgorithm::Crc64Nvme, data)
}

/// An incremental CRC-64/NVME.
#[derive(Clone)]
pub struct Crc64Nvme(Digest);

impl Default for Crc64Nvme {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc64Nvme {
    pub fn new() -> Self {
        Self(Digest::new(CrcAlgorithm::Crc64Nvme))
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(&self) -> u64 {
        self.0.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The catalogue check values: the CRC of the ASCII string "123456789" (Williams, "A
    /// Painless Guide to CRC Error Detection Algorithms"; the Rocksoft model parameters
    /// crc-fast implements). CRC-32C also matches RFC 3720 Appendix B.4's vectors below.
    #[test]
    fn check_values() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc64nvme(b"123456789"), 0xAE8B_1486_0A79_9888);
    }

    /// RFC 3720, Appendix B.4: 32 bytes of zeros, of 0xFF, and incrementing.
    #[test]
    fn rfc3720_vectors() {
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let inc: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&inc), 0x46DD_794E);
        let dec: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&dec), 0x113F_DB5C);
    }

    #[test]
    fn combining_with_nothing_changes_nothing() {
        let c = crc32c(b"123456789");
        assert_eq!(crc32c_combine(c, crc32c(&[]), 0), c);
        assert_eq!(crc32c_combine(crc32c(&[]), c, 9), c);
        assert_eq!(
            crc32c_combine(crc32c(b"1234"), crc32c(b"56789"), 5),
            0xE306_9283
        );
        assert_eq!(crc32c_extend(crc32c(b"1234"), b"56789"), 0xE306_9283);
        assert_eq!(crc32c_extend(c, &[]), c);
        assert_eq!(crc32c_extend(0, b"123456789"), c);
    }

    proptest! {
        #[test]
        fn combine_equals_whole(a in proptest::collection::vec(any::<u8>(), 0..5000),
                                b in proptest::collection::vec(any::<u8>(), 0..5000)) {
            let whole: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
            prop_assert_eq!(crc32c_combine(crc32c(&a), crc32c(&b), b.len() as u64), crc32c(&whole));
        }

        #[test]
        fn every_crc_combines(a in proptest::collection::vec(any::<u8>(), 0..3000),
                              b in proptest::collection::vec(any::<u8>(), 0..3000)) {
            let whole: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
            let n = b.len() as u64;
            prop_assert_eq!(crc32_combine(crc32(&a), crc32(&b), n), crc32(&whole));
            prop_assert_eq!(crc64nvme_combine(crc64nvme(&a), crc64nvme(&b), n), crc64nvme(&whole));
            let mut incremental = Crc32::new();
            incremental.update(&a);
            incremental.update(&b);
            prop_assert_eq!(incremental.finish(), crc32(&whole));
        }

        #[test]
        fn a_fixed_shift_combines_like_the_general_one(blocks in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 1000), 1..20)) {
            let shift = Crc32cShift::new(1000);
            let mut acc = crc32c(&blocks[0]);
            for b in &blocks[1..] {
                acc = shift.combine(acc, crc32c(b));
            }
            let whole: Vec<u8> = blocks.concat();
            prop_assert_eq!(acc, crc32c(&whole));
        }

        #[test]
        fn extending_equals_whole(a in proptest::collection::vec(any::<u8>(), 0..5000),
                                  b in proptest::collection::vec(any::<u8>(), 0..5000)) {
            let whole: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
            prop_assert_eq!(crc32c_extend(crc32c(&a), &b), crc32c(&whole));
        }

        #[test]
        fn incremental_equals_one_shot(data in proptest::collection::vec(any::<u8>(), 0..20000),
                                       cut in 0usize..20000) {
            let cut = cut.min(data.len());
            let mut c = Crc32c::new();
            c.update(&data[..cut]);
            c.update(&data[cut..]);
            prop_assert_eq!(c.finish(), crc32c(&data));
            let mut n = Crc64Nvme::new();
            n.update(&data[..cut]);
            n.update(&data[cut..]);
            prop_assert_eq!(n.finish(), crc64nvme(&data));
        }
    }
}
