//! `DataBlockFooter` of `table/block_based/data_block_footer.{h,cc}`
//! [R table/block_based/data_block_footer.h:18-89, data_block_footer.cc:12-100]: the last four
//! bytes of a block, the number of restart points in the low 28 bits and feature flags above
//! them, and before them, when the block keeps its values apart, the offset of its values.

use crate::error::{Error, Malformed};
use crate::util::coding::{decode_fixed32, put_fixed32};

/// How a data block is searched [R include/rocksdb/table.h, `DataBlockIndexType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataBlockIndexType {
    /// Binary search over the restart points.
    #[default]
    BinarySearch,
    /// Binary search, and a hash index of user keys to restart points for point lookups.
    BinaryAndHash,
}

/// Bit 31: the block carries a hash index [R data_block_footer.cc:12].
const HASH_INDEX_BIT: u32 = 1 << 31;
/// Bit 29: the restart keys are uniformly spread [R data_block_footer.cc:14].
const UNIFORM_KEYS_BIT: u32 = 1 << 29;
/// Bit 28: keys and values are kept in separate sections [R data_block_footer.cc:16].
const SEPARATED_KV_BIT: u32 = 1 << 28;

/// `DataBlockFooter` [R data_block_footer.h:43-87].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataBlockFooter {
    pub index_type: DataBlockIndexType,
    /// Whether the block keeps its values in a section after its keys.
    pub separated_kv: bool,
    /// Where the values section begins, when `separated_kv`.
    pub values_section_offset: u32,
    pub num_restarts: u32,
    pub is_uniform: bool,
}

impl DataBlockFooter {
    /// `kMaxNumRestarts` [R data_block_footer.h:52]: the low 28 bits; the top four are flags.
    pub const MAX_NUM_RESTARTS: u32 = (1 << 28) - 1;

    /// `kMaxEncodedLength` [R data_block_footer.h:57]: the values offset and the packed word.
    pub const MAX_ENCODED_LENGTH: usize = 8;

    /// `kMinEncodedLength` [R data_block_footer.h:60].
    pub const MIN_ENCODED_LENGTH: usize = 4;

    /// `EncodeTo` [R data_block_footer.cc:18-40]. RocksDB asserts the restart count fits its 28
    /// bits; here a count past it is refused.
    pub fn encode_to(&self, dst: &mut Vec<u8>) -> Result<(), Error> {
        if self.num_restarts > Self::MAX_NUM_RESTARTS {
            return Err(Error::LimitExceeded {
                what: "restart points in a block",
                limit: u64::from(Self::MAX_NUM_RESTARTS),
            });
        }
        if self.separated_kv {
            put_fixed32(dst, self.values_section_offset);
        }
        let mut packed = self.num_restarts;
        if self.index_type == DataBlockIndexType::BinaryAndHash {
            packed |= HASH_INDEX_BIT;
        }
        if self.separated_kv {
            packed |= SEPARATED_KV_BIT;
        }
        if self.is_uniform {
            packed |= UNIFORM_KEYS_BIT;
        }
        put_fixed32(dst, packed);
        Ok(())
    }

    /// `DecodeFrom` [R data_block_footer.cc:42-98]: the footer at the end of `input`, which is
    /// shortened to exclude it.
    pub fn decode_from(input: &mut &[u8]) -> Result<Self, Error> {
        let packed_at = input
            .len()
            .checked_sub(4)
            .ok_or(Error::truncated("data block footer"))?;
        let mut packed = decode_fixed32(input.get(packed_at..).unwrap_or_default())?;
        let mut footer = Self::default();
        if packed & HASH_INDEX_BIT != 0 {
            footer.index_type = DataBlockIndexType::BinaryAndHash;
            packed &= !HASH_INDEX_BIT;
        }
        if packed & SEPARATED_KV_BIT != 0 {
            footer.separated_kv = true;
            packed &= !SEPARATED_KV_BIT;
        }
        if packed & UNIFORM_KEYS_BIT != 0 {
            footer.is_uniform = true;
            packed &= !UNIFORM_KEYS_BIT;
        }
        // Bit 30, or any other feature this version does not know.
        if packed > Self::MAX_NUM_RESTARTS {
            return Err(Error::corruption(
                "data block footer (reserved bits set)",
                Malformed::Forbidden,
            ));
        }
        footer.num_restarts = packed;
        *input = input.get(..packed_at).unwrap_or_default();
        if footer.separated_kv {
            let at = input
                .len()
                .checked_sub(4)
                .ok_or(Error::truncated("separated values section offset"))?;
            footer.values_section_offset = decode_fixed32(input.get(at..).unwrap_or_default())?;
            *input = input.get(..at).unwrap_or_default();
        }
        Ok(footer)
    }
}
