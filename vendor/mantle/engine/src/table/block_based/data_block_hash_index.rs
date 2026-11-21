//! `DataBlockHashIndexBuilder` and `DataBlockHashIndex` of
//! `table/block_based/data_block_hash_index.{h,cc}` [R data_block_hash_index.h:15-140,
//! data_block_hash_index.cc:14-104]: a data block's map from a user key's hash to the restart
//! interval holding it, for point lookups. Each bucket is one byte, a restart index or one of
//! two marks; the bucket count is a 16-bit word after them.

use crate::error::Error;
use crate::util::coding::{decode_fixed16, put_fixed16};
use crate::util::hash::get_slice_hash;

/// `kNoEntry` [R data_block_hash_index.h:57]: a bucket no key hashed to.
pub const NO_ENTRY: u8 = 255;
/// `kCollision` [R data_block_hash_index.h:58]: a bucket keys of two restart intervals hashed to.
pub const COLLISION: u8 = 254;
/// `kMaxRestartSupportedByHashIndex` [R data_block_hash_index.h:59]: the restart indexes a byte
/// holds besides the two marks.
pub const MAX_RESTART_SUPPORTED_BY_HASH_INDEX: usize = 253;
/// `kMaxBlockSizeSupportedByHashIndex` [R data_block_hash_index.h:62]: offsets are 16-bit.
pub const MAX_BLOCK_SIZE_SUPPORTED_BY_HASH_INDEX: usize = 1 << 16;
/// `kDefaultUtilRatio` [R data_block_hash_index.h:64], taken for a ratio of zero or less.
pub const DEFAULT_UTIL_RATIO: f64 = 0.75;

/// `DataBlockHashIndexBuilder` [R data_block_hash_index.h:66-104].
#[derive(Debug, Clone, Default)]
pub struct DataBlockHashIndexBuilder {
    /// The inverse of the utilisation ratio; 0 while the builder is not in use.
    bucket_per_key: f64,
    estimated_num_buckets: f64,
    /// Whether every restart index added fits a bucket; RocksDB's `valid_`.
    valid: bool,
    hash_and_restart_pairs: Vec<(u32, u8)>,
    /// The whole part of `estimated_num_buckets`, at most 65,535: kept as the estimate grows,
    /// so the size estimate a flush policy asks for on every entry reads it, not a conversion.
    whole: u16,
}

impl DataBlockHashIndexBuilder {
    /// `Initialize` [R data_block_hash_index.h:73-79].
    pub fn initialize(&mut self, util_ratio: f64) {
        let util_ratio = if util_ratio <= 0.0 {
            DEFAULT_UTIL_RATIO
        } else {
            util_ratio
        };
        self.bucket_per_key = 1.0 / util_ratio;
        self.valid = true;
    }

    /// `Valid` [R data_block_hash_index.h:81].
    pub fn valid(&self) -> bool {
        self.valid && self.bucket_per_key > 0.0
    }

    /// The bucket count `Finish` writes: the estimate cast to 16 bits, made odd. RocksDB's cast
    /// of a `double` past 65,535 to `uint16_t` is undefined behaviour in C++; here the estimate
    /// saturates, so the block's size estimate passes the 64 KiB a hash index supports and no
    /// index is written.
    fn num_buckets(&self) -> u16 {
        self.whole | 1
    }

    /// Advances `whole` to the largest 16-bit value at most the estimate's whole part, as C++'s
    /// conversion rounds toward zero, compared in `f64` so no rounding enters; at 2^16 it
    /// saturates. A step of the usual size (a ratio of 0.3 to 1 adds 1 to 3.4 buckets a key) is
    /// a few increments; a jump of more than 64 is found bit by bit, in 16 compares.
    fn catch_up(&mut self) {
        let whole = self.estimated_num_buckets.trunc();
        if f64::from(self.whole) + 64.0 < whole {
            let mut n = 0u16;
            for bit in (0..16).rev() {
                let candidate = n | (1 << bit);
                if f64::from(candidate) <= whole {
                    n = candidate;
                }
            }
            self.whole = n;
            return;
        }
        while self.whole < u16::MAX && f64::from(self.whole.saturating_add(1)) <= whole {
            self.whole = self.whole.saturating_add(1);
        }
    }

    /// `EstimateSize` [R data_block_hash_index.h:86-92]: the buckets and their count.
    pub fn estimate_size(&self) -> usize {
        2usize.saturating_add(usize::from(self.num_buckets()))
    }

    /// `Add` [R data_block_hash_index.cc:14-25]: a key of the restart interval `restart_index`.
    /// An index past what a bucket holds makes the builder invalid for the block.
    pub fn add(&mut self, user_key: &[u8], restart_index: usize) {
        let Ok(index) = u8::try_from(restart_index) else {
            self.valid = false;
            return;
        };
        if restart_index > MAX_RESTART_SUPPORTED_BY_HASH_INDEX {
            self.valid = false;
            return;
        }
        self.hash_and_restart_pairs
            .push((get_slice_hash(user_key), index));
        self.estimated_num_buckets += self.bucket_per_key;
        self.catch_up();
    }

    /// `Finish` [R data_block_hash_index.cc:27-62]: appends the buckets and their count.
    pub fn finish(&self, buffer: &mut Vec<u8>) {
        let num_buckets = self.num_buckets();
        // The buckets are written in place at the buffer's end: no array of their own.
        let start = buffer.len();
        buffer.resize(start.saturating_add(usize::from(num_buckets)), NO_ENTRY);
        let buckets = buffer.get_mut(start..).unwrap_or_default();
        for &(hash, restart_index) in &self.hash_and_restart_pairs {
            // `num_buckets` is odd, never zero.
            let at = hash
                .checked_rem(u32::from(num_buckets))
                .and_then(|b| usize::try_from(b).ok())
                .unwrap_or_default();
            if let Some(bucket) = buckets.get_mut(at) {
                if *bucket == NO_ENTRY {
                    *bucket = restart_index;
                } else if *bucket != restart_index {
                    *bucket = COLLISION;
                }
            }
        }
        put_fixed16(buffer, num_buckets);
    }

    /// `Reset` [R data_block_hash_index.cc:64-68].
    pub fn reset(&mut self) {
        self.estimated_num_buckets = 0.0;
        self.whole = 0;
        self.valid = true;
        self.hash_and_restart_pairs.clear();
    }
}

/// `DataBlockHashIndex` [R data_block_hash_index.h:106-137]: a block's hash index as read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataBlockHashIndex {
    num_buckets: u16,
    /// Where the buckets begin in the block.
    map_offset: u16,
}

impl DataBlockHashIndex {
    /// `Initialize` [R data_block_hash_index.cc:70-92]: the index at the end of `data`, which
    /// holds the block up to and including the index. `None` where the size or the stored
    /// bucket count cannot be an index; the caller treats it as a corrupt block.
    pub fn initialize(data: &[u8]) -> Option<Self> {
        let size = data.len();
        if !(2..MAX_BLOCK_SIZE_SUPPORTED_BY_HASH_INDEX).contains(&size) {
            return None;
        }
        let count_at = size.checked_sub(2)?;
        let num_buckets = decode_fixed16(data.get(count_at..)?).ok()?;
        if num_buckets == 0 || usize::from(num_buckets) > count_at {
            return None;
        }
        let map_offset = u16::try_from(count_at.checked_sub(usize::from(num_buckets))?).ok()?;
        Some(Self {
            num_buckets,
            map_offset,
        })
    }

    /// Where the buckets begin in the block: RocksDB's `map_offset`.
    pub fn map_offset(&self) -> u16 {
        self.map_offset
    }

    /// `Valid` [R data_block_hash_index.h:126].
    pub fn valid(&self) -> bool {
        self.num_buckets != 0
    }

    /// `Lookup` [R data_block_hash_index.cc:94-102]: the bucket `user_key` hashes to.
    pub fn lookup(&self, data: &[u8], user_key: &[u8]) -> Result<u8, Error> {
        let bucket = get_slice_hash(user_key)
            .checked_rem(u32::from(self.num_buckets))
            .and_then(|b| usize::try_from(b).ok())
            .and_then(|b| usize::from(self.map_offset).checked_add(b))
            .ok_or(Error::truncated("data block hash index"))?;
        data.get(bucket)
            .copied()
            .ok_or(Error::truncated("data block hash index"))
    }
}
