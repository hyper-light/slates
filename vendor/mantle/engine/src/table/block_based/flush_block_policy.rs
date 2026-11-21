//! `FlushBlockBySizePolicy` of `table/block_based/flush_block_policy.cc`
//! [R flush_block_policy.cc:22-90]: when a block being built is cut, by its estimated size.
//!
//! RocksDB's policy holds a pointer to the builder it watches and is retargeted to another; here
//! the caller hands [`FlushBlockBySizePolicy::update`] the builder to judge, so nothing is held.

use crate::error::Error;
use crate::table::block_based::block_builder::BlockBuilder;
use crate::table::format::BLOCK_TRAILER_SIZE;

/// Cuts a block once it reaches its size, or once the next entry would take it past its size
/// when it is already within the deviation of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushBlockBySizePolicy {
    block_size: u64,
    /// `block_size_deviation_limit_`: the size past which a block counts as almost full.
    deviation_limit: u64,
    /// `block_align`: cut so a block and its trailer fit the block size.
    align: bool,
}

impl FlushBlockBySizePolicy {
    /// A policy for blocks of `block_size` bytes; `deviation` is the percentage of it a block may
    /// fall short by (`block_size_deviation`, 0 to 100).
    pub fn new(block_size: u64, deviation: u32, align: bool) -> Result<Self, Error> {
        let short = 100u64
            .checked_sub(u64::from(deviation))
            .ok_or(Error::InvalidArgument {
                what: "a block size deviation above 100 percent",
            })?;
        let deviation_limit = block_size
            .checked_mul(short)
            .and_then(|n| n.checked_add(99))
            .map(|n| n / 100)
            .ok_or(Error::InvalidArgument {
                what: "a block size too large for its deviation",
            })?;
        Ok(Self {
            block_size,
            deviation_limit,
            align,
        })
    }

    /// `Update` [R flush_block_policy.cc:37-51]: whether `builder` should be cut before the
    /// entry `key`, `value` is added.
    pub fn update(&self, builder: &BlockBuilder, key: &[u8], value: &[u8]) -> bool {
        if builder.is_empty() {
            return false;
        }
        let size = u64::try_from(builder.current_size_estimate()).unwrap_or(u64::MAX);
        size >= self.block_size || self.block_almost_full(builder, size, key, value)
    }

    /// `BlockAlmostFull` [R flush_block_policy.cc:54-70].
    fn block_almost_full(
        &self,
        builder: &BlockBuilder,
        size: u64,
        key: &[u8],
        value: &[u8],
    ) -> bool {
        if self.deviation_limit == 0 {
            return false;
        }
        let after = u64::try_from(builder.estimate_size_after_kv(key, value)).unwrap_or(u64::MAX);
        if self.align {
            return after.saturating_add(BLOCK_TRAILER_SIZE) > self.block_size;
        }
        after > self.block_size && size > self.deviation_limit
    }
}
