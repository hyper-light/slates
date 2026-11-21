//! CRC-32C and RocksDB's masking of stored CRCs: `util/crc32c.h` and `util/crc32c.cc`
//! (docs/research/24 §1.2).
//!
//! The CRC is Castagnoli's (reflected polynomial `0x82f63b78`) [R util/crc32c.cc:1197], which
//! mantle-crc computes with `crc-fast`; RocksDB's software, SSE 4.2, ARMv8 and POWER8 kernels
//! all compute the same function. RocksDB's `Crc32cCombine` [R util/crc32c.cc:1278-1293] is
//! zlib's combination in mantle-crc.

/// Added after rotating a CRC to mask it [R util/crc32c.h:37].
pub const MASK_DELTA: u32 = 0xa282_ead8;

/// The rotation that masks a CRC [R util/crc32c.h:44-47].
const MASK_ROTATION: u32 = 15;

/// `crc32c::Value` [R util/crc32c.h:35]: the CRC-32C of `data`.
pub fn value(data: &[u8]) -> u32 {
    mantle_crc::crc32c(data)
}

/// `crc32c::Extend` [R util/crc32c.h:26]: the CRC-32C of `A ‖ data`, given `init_crc`, the
/// CRC-32C of `A`.
pub fn extend(init_crc: u32, data: &[u8]) -> u32 {
    mantle_crc::crc32c_extend(init_crc, data)
}

/// `crc32c::Crc32cCombine` [R util/crc32c.h:28-33]: the CRC-32C of `A ‖ B`, given the
/// unmasked CRCs of `A` and `B` and `B`'s length.
pub fn crc32c_combine(crc1: u32, crc2: u32, crc2len: usize) -> u32 {
    // usize is at most 64 bits on every target mantle builds for.
    mantle_crc::crc32c_combine(crc1, crc2, crc2len as u64)
}

/// `crc32c::Mask` [R util/crc32c.h:44-47]: a stored CRC is masked so that the CRC of bytes
/// that contain CRCs is not degenerate.
pub const fn mask(crc: u32) -> u32 {
    crc.rotate_right(MASK_ROTATION).wrapping_add(MASK_DELTA)
}

/// `crc32c::Unmask` [R util/crc32c.h:50-53], the inverse of [`mask`].
pub const fn unmask(masked_crc: u32) -> u32 {
    masked_crc
        .wrapping_sub(MASK_DELTA)
        .rotate_left(MASK_ROTATION)
}
