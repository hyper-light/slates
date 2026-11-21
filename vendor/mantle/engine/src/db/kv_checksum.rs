//! `ProtectionInfo::ProtectKV` and its `Encode`/`Verify` of `db/kv_checksum.h`
//! [R db/kv_checksum.h:59-140, :324-332]: a 64-bit checksum of a key and its value, of which a
//! block keeps the low 1, 2, 4 or 8 bytes per entry (`block_protection_bytes_per_key`).

use crate::util::hash::get_slice_np_hash64;

/// `kSeedK` [R db/kv_checksum.h:84].
const SEED_K: u64 = 0;
/// `kSeedV` [R db/kv_checksum.h:85].
const SEED_V: u64 = 0xD28A_AD72_F49B_D50B;

/// `ProtectionInfo<uint64_t>().ProtectKV(key, value)`: the key's and the value's seeded hashes,
/// exclusive-or'd.
pub fn protect_kv(key: &[u8], value: &[u8]) -> u64 {
    get_slice_np_hash64(key, SEED_K) ^ get_slice_np_hash64(value, SEED_V)
}

/// Whether `len` is a width `Encode` writes: 1, 2, 4 or 8 bytes.
pub const fn is_supported_len(len: u8) -> bool {
    matches!(len, 1 | 2 | 4 | 8)
}

/// `Encode` [R db/kv_checksum.h:97-114]: appends the low `len` bytes of `value`, little-endian,
/// to `dst`; false for a width RocksDB does not write.
pub fn encode_to(value: u64, len: u8, dst: &mut Vec<u8>) -> bool {
    if !is_supported_len(len) {
        return false;
    }
    dst.extend(value.to_le_bytes().iter().take(usize::from(len)));
    true
}

/// `Verify` [R db/kv_checksum.h:117-135]: whether `stored`, `len` bytes, are the low bytes of
/// `value`.
pub fn verify(value: u64, len: u8, stored: &[u8]) -> bool {
    is_supported_len(len)
        && stored.len() == usize::from(len)
        && value.to_le_bytes().iter().zip(stored).all(|(a, b)| a == b)
}
