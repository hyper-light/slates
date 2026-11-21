//! `BlockBuilder` of `table/block_based/block_builder.{h,cc}` [R block_builder.h:20-160,
//! block_builder.cc:10-441]: a block of prefix-compressed entries. Each entry stores the bytes it
//! shares with the previous key, the bytes it does not, and its value or the value's length;
//! every `restart_interval` entries the key is stored whole, a restart point, and the block ends
//! with the restart offsets, an optional hash index and a [`DataBlockFooter`].
//!
//! An entry is `shared: varint32, non_shared: varint32, [value_length: varint32],
//! key_delta[non_shared], value[value_length]`; index blocks of format_version 4 and later leave
//! out the value length, their value being self-describing varints, and with separated keys and
//! values a restart entry also carries its value's offset in the values section.
//!
//! Where RocksDB asserts (a restart interval of at least one, sizes below 4 GiB, a delta value
//! with value delta encoding, a key holding an internal key's eight trailing bytes for a hash
//! index) the builder returns a typed error; where RocksDB truncates silently (a block or values
//! section past 4 GiB, its own FIXMEs at block_builder.cc:287 and :309), it refuses. User-defined
//! timestamps, which a builder may strip from keys, are refused until P16.

use crate::db::dbformat::NUM_INTERNAL_BYTES;
use crate::error::Error;
use crate::table::block_based::block_util::{decode_entry, read_be64_from_key};
use crate::table::block_based::data_block_footer::{DataBlockFooter, DataBlockIndexType};
use crate::table::block_based::data_block_hash_index::{
    DataBlockHashIndexBuilder, MAX_BLOCK_SIZE_SUPPORTED_BY_HASH_INDEX,
};
use crate::util::coding::{put_fixed32, put_varint32s, varint_length};

/// How a block is built: RocksDB's constructor arguments [R block_builder.h:26-33].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockBuilderOptions {
    /// Entries between restart points; at least one.
    pub restart_interval: u32,
    /// Whether a key stores only the bytes it does not share with the previous one.
    pub use_delta_encoding: bool,
    /// Index blocks of format_version 4 and later: no value length, and a value given as a delta
    /// where the key shares bytes (`BlockIter::DecodeCurrentValue`).
    pub use_value_delta_encoding: bool,
    pub index_type: DataBlockIndexType,
    /// The hash index's buckets per key is this ratio's inverse.
    pub data_block_hash_table_util_ratio: f64,
    /// Whether the keys are user keys rather than internal keys.
    pub is_user_key: bool,
    /// Keys in one section and values in another.
    pub use_separated_kv_storage: bool,
    /// Below this coefficient of variation of the restart keys' gaps the block is marked
    /// uniform; `None` never marks it.
    pub uniform_cv_threshold: Option<f64>,
    /// Bytes to reserve for a block's buffer when the builder is made: the table's block size,
    /// at which blocks are cut. The buffer keeps its capacity across `reset`, so it grows only
    /// for a block larger than any before it. 0 reserves nothing.
    pub capacity: usize,
}

impl Default for BlockBuilderOptions {
    /// RocksDB's defaults [R block_builder.h:27-33].
    fn default() -> Self {
        Self {
            restart_interval: 16,
            use_delta_encoding: true,
            use_value_delta_encoding: false,
            index_type: DataBlockIndexType::BinarySearch,
            data_block_hash_table_util_ratio: 0.75,
            is_user_key: false,
            use_separated_kv_storage: false,
            uniform_cv_threshold: None,
            capacity: 0,
        }
    }
}

/// `UniformDataTracker` [R block_builder.cc:58-88]: Welford's online mean and variance of the gaps
/// between consecutive restart keys, in `double` as RocksDB computes them, so the decision is the
/// same bit for bit.
#[derive(Debug, Default)]
struct UniformDataTracker {
    prev_key_value: u64,
    num_keys: usize,
    mean: f64,
    m2: f64,
}

impl UniformDataTracker {
    fn add_key(&mut self, key_value: u64) {
        if self.num_keys > 0 {
            // RocksDB subtracts unsigned; restart keys ascend, and a wrap is kept as it does.
            let gap = key_value.wrapping_sub(self.prev_key_value) as f64;
            let delta = gap - self.mean;
            self.mean += delta / self.num_keys as f64;
            let delta2 = gap - self.mean;
            self.m2 += delta * delta2;
        }
        self.prev_key_value = key_value;
        self.num_keys = self.num_keys.saturating_add(1);
    }

    /// The gaps' coefficient of variation, or `None` with fewer than two gaps or a mean of none.
    fn cv(&self) -> Option<f64> {
        let gaps = self.num_keys.saturating_sub(1);
        if gaps < 2 || self.mean <= 0.0 {
            return None;
        }
        Some((self.m2 / gaps as f64).sqrt() / self.mean)
    }
}

/// `BlockBuilder` [R block_builder.h:20-160].
#[derive(Debug)]
pub struct BlockBuilder {
    options: BlockBuilderOptions,
    buffer: Vec<u8>,
    restarts: Vec<u32>,
    estimate: usize,
    counter: u32,
    finished: bool,
    is_uniform: bool,
    last_key: Vec<u8>,
    hash_index: DataBlockHashIndexBuilder,
    values_buffer: Vec<u8>,
}

/// A size RocksDB keeps in 32 bits.
fn size32(n: usize, what: &'static str) -> Result<u32, Error> {
    u32::try_from(n).map_err(|_| Error::LimitExceeded {
        what,
        limit: u64::from(u32::MAX),
    })
}

/// `Slice::difference_offset`: the bytes `a` and `b` share at their start.
#[inline]
fn difference_offset(a: &[u8], b: &[u8]) -> usize {
    // Eight bytes at a time: read little-endian, the lowest set bit of the words' difference
    // lies in the first byte that differs.
    let (mut x, mut y, mut at) = (a, b, 0usize);
    while let (Some((xw, xr)), Some((yw, yr))) =
        (x.split_first_chunk::<8>(), y.split_first_chunk::<8>())
    {
        let d = u64::from_le_bytes(*xw) ^ u64::from_le_bytes(*yw);
        if d != 0 {
            return at.saturating_add((d.trailing_zeros() / 8) as usize);
        }
        at = at.saturating_add(8);
        x = xr;
        y = yr;
    }
    at.saturating_add(x.iter().zip(y).take_while(|(p, q)| p == q).count())
}

impl BlockBuilder {
    /// The empty block's estimate: the restart count and the first restart, and the values
    /// offset with separated keys and values [R block_builder.cc:123-124].
    fn empty_estimate(options: &BlockBuilderOptions) -> usize {
        if options.use_separated_kv_storage {
            12
        } else {
            8
        }
    }

    /// `BlockBuilder::BlockBuilder` [R block_builder.cc:92-125].
    pub fn new(options: BlockBuilderOptions) -> Result<Self, Error> {
        if options.restart_interval == 0 {
            return Err(Error::InvalidArgument {
                what: "a block's restart interval must be at least one",
            });
        }
        let mut hash_index = DataBlockHashIndexBuilder::default();
        if options.index_type == DataBlockIndexType::BinaryAndHash {
            hash_index.initialize(options.data_block_hash_table_util_ratio);
        }
        Ok(Self {
            estimate: Self::empty_estimate(&options),
            options,
            buffer: Vec::with_capacity(options.capacity),
            restarts: vec![0],
            counter: 0,
            finished: false,
            is_uniform: false,
            last_key: Vec::new(),
            hash_index,
            values_buffer: Vec::new(),
        })
    }

    /// `Reset` [R block_builder.cc:127-145].
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.restarts.clear();
        self.restarts.push(0);
        self.estimate = Self::empty_estimate(&self.options);
        self.counter = 0;
        self.finished = false;
        self.is_uniform = false;
        self.last_key.clear();
        if self.hash_index.valid() {
            self.hash_index.reset();
        }
        self.values_buffer.clear();
    }

    /// `SwapAndReset` [R block_builder.cc:147-150]: the finished block, and the builder reset.
    pub fn take_and_reset(&mut self) -> Vec<u8> {
        let block = std::mem::take(&mut self.buffer);
        self.reset();
        block
    }

    /// `CurrentSizeEstimate` [R block_builder.h:82-85].
    pub fn current_size_estimate(&self) -> usize {
        let hash = if self.hash_index.valid() {
            self.hash_index.estimate_size()
        } else {
            0
        };
        self.estimate.saturating_add(hash)
    }

    /// `EstimateSizeAfterKV` [R block_builder.cc:152-187]: the estimate after adding `key` and
    /// `value`, counting the whole key as unshared.
    pub fn estimate_size_after_kv(&self, key: &[u8], value: &[u8]) -> usize {
        let restarting = self.counter >= self.options.restart_interval;
        let value_bytes = if !self.options.use_value_delta_encoding || restarting {
            value.len()
        } else {
            value.len() / 2
        };
        let mut estimate = self
            .current_size_estimate()
            .saturating_add(key.len())
            .saturating_add(value_bytes);
        if restarting {
            estimate = estimate.saturating_add(4);
        }
        if self.options.use_separated_kv_storage && (self.counter == 0 || restarting) {
            estimate = estimate.saturating_add(varint_length(self.values_buffer.len() as u64));
        }
        // The shared length's varint, counted as four bytes, and the key length's.
        estimate = estimate
            .saturating_add(4)
            .saturating_add(varint_length(key.len() as u64));
        if !self.options.use_value_delta_encoding || restarting {
            estimate = estimate.saturating_add(varint_length(value.len() as u64));
        }
        estimate
    }

    /// `empty` [R block_builder.h:91].
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// `IsUniform` [R block_builder.h:97]: whether the last finished block was marked uniform.
    pub fn is_uniform(&self) -> bool {
        self.is_uniform
    }

    /// `Add` [R block_builder.cc:222-236]: an entry after the previous one this builder took.
    /// `delta_value` is the value's delta form, needed with value delta encoding where the key
    /// shares bytes. Keys must ascend, as RocksDB requires and does not check.
    pub fn add(
        &mut self,
        key: &[u8],
        value: &[u8],
        delta_value: Option<&[u8]>,
        skip_delta_encoding: bool,
    ) -> Result<(), Error> {
        // The previous key's buffer is lent to `add_impl` and then holds this key, its capacity
        // kept, as RocksDB's `last_key_.assign` keeps it.
        let mut last_key = std::mem::take(&mut self.last_key);
        let added = self.add_impl(key, value, &last_key, delta_value, skip_delta_encoding);
        if self.options.use_delta_encoding {
            last_key.clear();
            last_key.extend_from_slice(key);
        }
        self.last_key = last_key;
        added
    }

    /// `AddWithLastKey` [R block_builder.cc:238-262]: an entry whose previous key the caller
    /// already holds; `last_key` is ignored on the block's first entry.
    pub fn add_with_last_key(
        &mut self,
        key: &[u8],
        value: &[u8],
        last_key: &[u8],
        delta_value: Option<&[u8]>,
        skip_delta_encoding: bool,
    ) -> Result<(), Error> {
        let last_key = if self.buffer.is_empty() {
            &[][..]
        } else {
            last_key
        };
        self.add_impl(key, value, last_key, delta_value, skip_delta_encoding)
    }

    /// `AddWithLastKeyImpl` [R block_builder.cc:264-367].
    fn add_impl(
        &mut self,
        key: &[u8],
        value: &[u8],
        last_key: &[u8],
        delta_value: Option<&[u8]>,
        skip_delta_encoding: bool,
    ) -> Result<(), Error> {
        if self.finished {
            return Err(Error::InvalidArgument {
                what: "an entry added to a finished block",
            });
        }
        let key_len = size32(key.len(), "a block entry's key bytes")?;
        let value_len = size32(value.len(), "a block entry's value bytes")?;
        let buffer_size = self.buffer.len();
        let buffer_size32 = size32(buffer_size, "a block's bytes")?;
        let prev_values_size = self.values_buffer.len();
        let prev_values_size32 = size32(prev_values_size, "a block's values section")?;
        let mut shared = 0u32;
        if self.counter >= self.options.restart_interval {
            self.restarts.push(buffer_size32);
            self.estimate = self.estimate.saturating_add(4);
            self.counter = 0;
        } else if self.options.use_delta_encoding && !skip_delta_encoding {
            // At most the key's length, which fits 32 bits.
            shared = size32(
                difference_offset(key, last_key),
                "a block entry's key bytes",
            )?;
        }
        let non_shared = key_len.saturating_sub(shared);
        let restart = self.counter == 0;
        let separated = self.options.use_separated_kv_storage;
        // The header in one append, as RocksDB's `PutVarint32Varint32Varint32` writes it.
        let mut header = [0u32; 4];
        let mut fields = 0usize;
        for (value, present) in [
            (shared, true),
            (non_shared, true),
            (value_len, !self.options.use_value_delta_encoding),
            (prev_values_size32, separated && restart),
        ] {
            if present && let Some(slot) = header.get_mut(fields) {
                *slot = value;
                fields = fields.saturating_add(1);
            }
        }
        put_varint32s(&mut self.buffer, header.get(..fields).unwrap_or_default());
        self.buffer
            .extend_from_slice(key.get(shared as usize..).unwrap_or_default());
        let stored_value = if shared != 0 && self.options.use_value_delta_encoding {
            delta_value.ok_or(Error::InvalidArgument {
                what: "value delta encoding needs the value's delta where the key shares bytes",
            })?
        } else {
            value
        };
        size32(stored_value.len(), "a block entry's value bytes")?;
        let values = if separated {
            &mut self.values_buffer
        } else {
            &mut self.buffer
        };
        values.extend_from_slice(stored_value);
        if self.hash_index.valid() {
            // Data blocks hold internal keys; their hash index is of the user key.
            let user = key
                .len()
                .checked_sub(NUM_INTERNAL_BYTES)
                .ok_or(Error::truncated("internal key"))?;
            let restart_index = self.restarts.len().saturating_sub(1);
            self.hash_index
                .add(key.get(..user).unwrap_or_default(), restart_index);
        }
        self.counter = self.counter.saturating_add(1);
        let grew = self
            .buffer
            .len()
            .saturating_sub(buffer_size)
            .saturating_add(self.values_buffer.len().saturating_sub(prev_values_size));
        self.estimate = self.estimate.saturating_add(grew);
        Ok(())
    }

    /// `GetRestartKey` [R block_builder.cc:385-404]: the whole key stored at restart `index`.
    fn restart_key(&self, index: usize) -> Result<&[u8], Error> {
        let at = self
            .restarts
            .get(index)
            .and_then(|&r| usize::try_from(r).ok())
            .ok_or(Error::truncated("block restart point"))?;
        let entry = self.buffer.get(at..).unwrap_or_default();
        let (header, n) = decode_entry(
            entry,
            !self.options.use_value_delta_encoding,
            self.options.use_separated_kv_storage,
        )?;
        let end = n
            .checked_add(header.non_shared as usize)
            .ok_or(Error::truncated("block restart key"))?;
        entry
            .get(n..end)
            .ok_or(Error::truncated("block restart key"))
    }

    /// `ScanForUniformity` [R block_builder.cc:406-441]: whether the restart keys, read as
    /// big-endian integers past their common prefix, are spread with a coefficient of variation
    /// below the threshold.
    fn scan_for_uniformity(&self) -> Result<bool, Error> {
        let Some(threshold) = self.options.uniform_cv_threshold else {
            return Ok(false);
        };
        if threshold < 0.0 || self.restarts.len() < 3 {
            return Ok(false);
        }
        let is_user_key = self.options.is_user_key;
        let first = self.restart_key(0)?;
        let last = self.restart_key(self.restarts.len().saturating_sub(1))?;
        if !is_user_key && (first.len() < NUM_INTERNAL_BYTES || last.len() < NUM_INTERNAL_BYTES) {
            return Ok(false);
        }
        let prefix_len = difference_offset(first, last);
        let mut tracker = UniformDataTracker::default();
        for i in 0..self.restarts.len() {
            let key = self.restart_key(i)?;
            if !is_user_key && key.len() < NUM_INTERNAL_BYTES {
                return Ok(false);
            }
            tracker.add_key(read_be64_from_key(key, is_user_key, prefix_len)?);
        }
        Ok(tracker.cv().is_some_and(|cv| cv < threshold))
    }

    /// `Finish` [R block_builder.cc:189-220]: the block, which stays this builder's until
    /// `reset`.
    pub fn finish(&mut self) -> Result<&[u8], Error> {
        self.is_uniform = self.scan_for_uniformity()?;
        let values_section_offset = size32(self.buffer.len(), "a block's bytes")?;
        if self.options.use_separated_kv_storage {
            let values = std::mem::take(&mut self.values_buffer);
            self.buffer.extend_from_slice(&values);
            self.values_buffer = values;
        }
        for &restart in &self.restarts {
            put_fixed32(&mut self.buffer, restart);
        }
        let mut footer = DataBlockFooter {
            num_restarts: size32(self.restarts.len(), "restart points in a block")?,
            is_uniform: self.is_uniform,
            ..DataBlockFooter::default()
        };
        if self.hash_index.valid()
            && self.current_size_estimate() <= MAX_BLOCK_SIZE_SUPPORTED_BY_HASH_INDEX
        {
            self.hash_index.finish(&mut self.buffer);
            footer.index_type = DataBlockIndexType::BinaryAndHash;
        }
        if self.options.use_separated_kv_storage {
            footer.separated_kv = true;
            footer.values_section_offset = values_section_offset;
        }
        footer.encode_to(&mut self.buffer)?;
        self.finished = true;
        Ok(&self.buffer)
    }
}
