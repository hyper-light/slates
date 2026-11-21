//! A chunk's identity: which block it belongs to, which layout generation of that block,
//! and which piece of the layout it is.

use std::fmt;

use mantle_codec::{Reader, Writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkKey {
    /// The block's random 128-bit id.
    pub block: u128,
    /// The block's layout generation: re-encoding a block writes chunks of a new epoch.
    pub epoch: u32,
    /// The piece of the layout: a replica number or an erasure-coded shard index.
    pub index: u16,
}

impl ChunkKey {
    pub const ENCODED_LEN: usize = 22;

    pub fn encode(&self, w: &mut Writer) {
        w.u128(self.block);
        w.u32(self.epoch);
        w.u16(self.index);
    }

    pub fn decode(r: &mut Reader<'_>) -> Option<Self> {
        Some(Self {
            block: r.u128()?,
            epoch: r.u32()?,
            index: r.u16()?,
        })
    }
}

impl fmt::Display for ChunkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}.{}.{}", self.block, self.epoch, self.index)
    }
}
