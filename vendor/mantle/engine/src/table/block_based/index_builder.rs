//! The index builders of `table/block_based/index_builder.{h,cc}` [R index_builder.h:36-784;
//! index_builder.cc:21-421]: `ShortenedIndexBuilder` (binary search, with or without each block's
//! first key), `HashIndexBuilder` (binary search plus the prefix blocks a hash search reads) and
//! `PartitionedIndexBuilder` (index partitions under a top-level index).
//!
//! An index entry's key separates its block from the next: the block's last key, shortened when
//! the options allow to the shortest key between it and the next block's first key. While no two
//! adjacent blocks share a user key, the index stores user keys; from the first pair that does,
//! internal keys. RocksDB builds both blocks as it goes and keeps one at the end; so does the port,
//! which writes the same bytes.
//!
//! Not ported here: user-defined timestamps (P16), and the split of `AddIndexEntry` into
//! `PrepareIndexEntry` and `FinishIndexEntry` for parallel compression, which comes with the
//! table builder that drives it. Where RocksDB asserts that a delta-encoded handle follows the
//! previous one, or that a key is in the prefix extractor's domain, the port returns an error.

use std::collections::VecDeque;

use crate::db::dbformat::{
    InternalKeyComparator, MAX_SEQUENCE_NUMBER, VALUE_TYPE_FOR_SEEK, extract_user_key,
    pack_sequence_and_type,
};
use crate::error::Error;
use crate::table::block_based::block_builder::{BlockBuilder, BlockBuilderOptions};
use crate::table::block_based::flush_block_policy::FlushBlockBySizePolicy;
use crate::table::format::{BlockHandle, IndexValue};
use crate::util::coding::{put_fixed64, put_varint32, put_varsignedint64};
use crate::util::comparator::Comparator;
use crate::util::slice_transform::SliceTransform;

/// `kHashIndexPrefixesBlock` [R block_based_table_factory.cc:1153].
pub const HASH_INDEX_PREFIXES_BLOCK: &str = "rocksdb.hashindex.prefixes";
/// `kHashIndexPrefixesMetadataBlock` [R block_based_table_factory.cc:1154-1155].
pub const HASH_INDEX_PREFIXES_METADATA_BLOCK: &str = "rocksdb.hashindex.metadata";

/// `BlockBasedTableOptions::IndexType` [R include/rocksdb/table.h:297-324], its stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    BinarySearch = 0x00,
    HashSearch = 0x01,
    TwoLevelIndexSearch = 0x02,
    BinarySearchWithFirstKey = 0x03,
}

/// `IndexShorteningMode` [R include/rocksdb/table.h:820-831].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexShorteningMode {
    /// Full keys.
    NoShortening,
    /// Separators between blocks shortened; the last key in full.
    #[default]
    ShortenSeparators,
    /// The last key shortened to a successor as well.
    ShortenSeparatorsAndSuccessor,
}

/// The table options an index builder reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexBuilderOptions {
    pub comparator: Comparator,
    pub index_block_restart_interval: u32,
    pub format_version: u32,
    /// Index values delta encoded (format_version 4 and later).
    pub use_value_delta_encoding: bool,
    pub index_shortening: IndexShorteningMode,
    /// `uniform_cv_threshold`; `None` for RocksDB's default of −1, which marks no block.
    pub uniform_cv_threshold: Option<f64>,
    /// The size a partition is cut at (`metadata_block_size`).
    pub metadata_block_size: u64,
    /// `block_size_deviation`, 0 to 100.
    pub block_size_deviation: u32,
}

impl Default for IndexBuilderOptions {
    /// `BlockBasedTableOptions`' defaults [R include/rocksdb/table.h:407-831] at format_version 7.
    fn default() -> Self {
        Self {
            comparator: Comparator::Bytewise,
            index_block_restart_interval: 1,
            format_version: 7,
            use_value_delta_encoding: true,
            index_shortening: IndexShorteningMode::ShortenSeparators,
            uniform_cv_threshold: None,
            metadata_block_size: 4096,
            block_size_deviation: 10,
        }
    }
}

/// What a finished index is: its index block and the meta blocks it adds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexBlocks {
    pub index_block: Vec<u8>,
    /// Meta blocks by name, as RocksDB's `meta_blocks` map holds them.
    pub meta_blocks: Vec<(&'static str, Vec<u8>)>,
}

/// What a partitioned index's `Finish` returned: a partition, after which `Finish` is called
/// again with the partition's handle, or the top-level index, which ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finished {
    /// RocksDB's `Status::Incomplete`.
    Partition(IndexBlocks),
    Done(IndexBlocks),
}

fn index_block_builder(
    options: &IndexBuilderOptions,
    is_user_key: bool,
) -> Result<BlockBuilder, Error> {
    BlockBuilder::new(BlockBuilderOptions {
        restart_interval: options.index_block_restart_interval,
        use_delta_encoding: true,
        use_value_delta_encoding: options.use_value_delta_encoding,
        is_user_key,
        uniform_cv_threshold: options.uniform_cv_threshold,
        ..BlockBuilderOptions::default()
    })
}

/// The trailer a shortened key takes: the earliest entry of its user key
/// (`PackSequenceAndType(kMaxSequenceNumber, kValueTypeForSeek)`).
fn append_earliest(key: &mut Vec<u8>) -> Result<(), Error> {
    put_fixed64(
        key,
        pack_sequence_and_type(MAX_SEQUENCE_NUMBER, VALUE_TYPE_FOR_SEEK)?,
    );
    Ok(())
}

/// `FindShortestInternalKeySeparator` [R index_builder.cc:77-100]: whether `scratch` now holds a
/// key in `[start, limit)` shorter than `start`'s user key, with the earliest trailer.
pub fn find_shortest_internal_key_separator(
    comparator: Comparator,
    start: &[u8],
    limit: &[u8],
    scratch: &mut Vec<u8>,
) -> Result<bool, Error> {
    let user_start = extract_user_key(start)?;
    let user_limit = extract_user_key(limit)?;
    scratch.clear();
    scratch.extend_from_slice(user_start);
    comparator.find_shortest_separator(scratch, user_limit);
    if scratch.len() <= user_start.len() && comparator.compare(user_start, scratch).is_lt() {
        append_earliest(scratch)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// `FindShortInternalKeySuccessor` [R index_builder.cc:102-117]: whether `scratch` now holds a
/// short key above `key`'s user key, with the earliest trailer.
pub fn find_short_internal_key_successor(
    comparator: Comparator,
    key: &[u8],
    scratch: &mut Vec<u8>,
) -> Result<bool, Error> {
    let user_key = extract_user_key(key)?;
    scratch.clear();
    scratch.extend_from_slice(user_key);
    comparator.find_short_successor(scratch);
    if scratch.len() <= user_key.len() && comparator.compare(user_key, scratch).is_lt() {
        append_earliest(scratch)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// `ShortenedIndexBuilder` [R index_builder.h:226-524]: one index block, keyed by shortened
/// separators.
#[derive(Debug)]
pub struct ShortenedIndexBuilder {
    icmp: InternalKeyComparator,
    with_seq: BlockBuilder,
    without_seq: BlockBuilder,
    use_value_delta_encoding: bool,
    /// Whether two adjacent blocks shared a user key, so the index must hold internal keys;
    /// set from the start before format_version 3.
    must_use_separator_with_seq: bool,
    include_first_key: bool,
    shortening: IndexShorteningMode,
    last_encoded_handle: BlockHandle,
    is_uniform: bool,
    current_block_first_internal_key: Vec<u8>,
    num_index_entries: u64,
    estimated_index_size: u64,
    index_size: usize,
}

impl ShortenedIndexBuilder {
    pub fn new(options: &IndexBuilderOptions, include_first_key: bool) -> Result<Self, Error> {
        Ok(Self {
            icmp: InternalKeyComparator::new(options.comparator),
            with_seq: index_block_builder(options, false)?,
            without_seq: index_block_builder(options, true)?,
            use_value_delta_encoding: options.use_value_delta_encoding,
            must_use_separator_with_seq: options.format_version <= 2,
            include_first_key,
            shortening: options.index_shortening,
            last_encoded_handle: BlockHandle::NULL,
            is_uniform: false,
            current_block_first_internal_key: Vec::new(),
            num_index_entries: 0,
            estimated_index_size: 0,
            index_size: 0,
        })
    }

    /// `OnKeyAdded`: notes a block's first key, when entries carry it.
    pub fn on_key_added(&mut self, key: &[u8]) {
        if self.include_first_key && self.current_block_first_internal_key.is_empty() {
            self.current_block_first_internal_key.extend_from_slice(key);
        }
    }

    /// `GetSeparatorWithSeq` [R index_builder.h:266-299].
    fn separator_with_seq<'k>(
        &mut self,
        last_key: &'k [u8],
        first_key_in_next_block: Option<&[u8]>,
        scratch: &'k mut Vec<u8>,
    ) -> Result<&'k [u8], Error> {
        let user = self.icmp.user_comparator();
        let shortened = match first_key_in_next_block {
            Some(next) => {
                let shortened = self.shortening != IndexShorteningMode::NoShortening
                    && find_shortest_internal_key_separator(user, last_key, next, scratch)?;
                if !self.must_use_separator_with_seq
                    && user.equal(extract_user_key(last_key)?, extract_user_key(next)?)
                {
                    self.must_use_separator_with_seq = true;
                }
                shortened
            }
            None => {
                self.shortening == IndexShorteningMode::ShortenSeparatorsAndSuccessor
                    && find_short_internal_key_successor(user, last_key, scratch)?
            }
        };
        Ok(if shortened {
            scratch.as_slice()
        } else {
            last_key
        })
    }

    /// `AddIndexEntryImpl` [R index_builder.h:318-358].
    fn add_entry(
        &mut self,
        separator: &[u8],
        handle: BlockHandle,
        skip_delta_encoding: bool,
    ) -> Result<(), Error> {
        let first: &[u8] = if self.include_first_key {
            &self.current_block_first_internal_key
        } else {
            &[]
        };
        let entry = IndexValue {
            handle,
            first_internal_key: first,
        };
        let mut encoded = Vec::new();
        entry.encode_to(&mut encoded, self.include_first_key, None)?;
        let mut delta = Vec::new();
        if self.use_value_delta_encoding
            && !self.last_encoded_handle.is_null()
            && !skip_delta_encoding
        {
            entry.encode_to(
                &mut delta,
                self.include_first_key,
                Some(&self.last_encoded_handle),
            )?;
        }
        self.last_encoded_handle = handle;
        self.with_seq
            .add(separator, &encoded, Some(&delta), skip_delta_encoding)?;
        if !self.must_use_separator_with_seq {
            self.without_seq.add(
                extract_user_key(separator)?,
                &encoded,
                Some(&delta),
                skip_delta_encoding,
            )?;
        }
        self.num_index_entries = self.num_index_entries.saturating_add(1);
        self.update_index_size_estimate();
        Ok(())
    }

    /// `AddIndexEntry` [R index_builder.h:360-375]: the entry for the block `handle`, whose last
    /// key is `last_key`; `first_key_in_next_block` is `None` for the table's last block. Returns
    /// the key stored, `last_key` or a shorter separator held in `scratch`.
    pub fn add_index_entry<'k>(
        &mut self,
        last_key: &'k [u8],
        first_key_in_next_block: Option<&[u8]>,
        handle: BlockHandle,
        scratch: &'k mut Vec<u8>,
        skip_delta_encoding: bool,
    ) -> Result<&'k [u8], Error> {
        let separator = self.separator_with_seq(last_key, first_key_in_next_block, scratch)?;
        if self.include_first_key && self.current_block_first_internal_key.is_empty() {
            return Err(Error::InvalidArgument {
                what: "an index entry with its first key for a block that had no key",
            });
        }
        self.add_entry(separator, handle, skip_delta_encoding)?;
        self.current_block_first_internal_key.clear();
        Ok(separator)
    }

    /// `Finish` [R index_builder.h:437-449].
    pub fn finish(&mut self) -> Result<IndexBlocks, Error> {
        let builder = if self.must_use_separator_with_seq {
            &mut self.with_seq
        } else {
            &mut self.without_seq
        };
        let index_block = builder.finish()?.to_vec();
        self.is_uniform = builder.is_uniform();
        self.index_size = index_block.len();
        Ok(IndexBlocks {
            index_block,
            meta_blocks: Vec::new(),
        })
    }

    /// `IndexSize`: the finished index block's size.
    pub fn index_size(&self) -> usize {
        self.index_size
    }

    /// `NumUniformIndexBlocks`.
    pub fn num_uniform_index_blocks(&self) -> u64 {
        u64::from(self.is_uniform)
    }

    /// `CurrentIndexSizeEstimate`.
    pub fn current_index_size_estimate(&self) -> u64 {
        self.estimated_index_size
    }

    /// `separator_is_key_plus_seq`.
    pub fn separator_is_key_plus_seq(&self) -> bool {
        self.must_use_separator_with_seq
    }

    /// The block the index's keys go to as things stand.
    fn active(&self) -> &BlockBuilder {
        if self.must_use_separator_with_seq {
            &self.with_seq
        } else {
            &self.without_seq
        }
    }

    /// `UpdateIndexSizeEstimate` [R index_builder.cc:119-131]: the block's size and room for two
    /// more entries of the mean size so far, RocksDB's margin for the next entry.
    fn update_index_size_estimate(&mut self) {
        let current = u64::try_from(self.active().current_size_estimate()).unwrap_or(u64::MAX);
        let margin = current
            .checked_div(self.num_index_entries)
            .map_or(0, |mean| mean.saturating_mul(2));
        self.estimated_index_size = current.saturating_add(margin);
    }
}

/// `HashIndexBuilder` [R index_builder.h:527-663]: a binary-search index, and for each run of
/// blocks whose keys share a prefix, the prefix and where its blocks start in the index.
#[derive(Debug)]
pub struct HashIndexBuilder {
    primary: ShortenedIndexBuilder,
    prefix_extractor: SliceTransform,
    prefix_block: Vec<u8>,
    prefix_meta_block: Vec<u8>,
    pending_block_num: u32,
    pending_entry_index: u32,
    pending_entry_prefix: Vec<u8>,
    current_restart_index: u64,
    /// The two prefix blocks' bytes, once handed out by `finish`.
    prefix_blocks_size: usize,
}

impl HashIndexBuilder {
    /// RocksDB requires an index restart interval of one here, and asserts it
    /// [R index_builder.cc:36-38]; the port refuses another.
    pub fn new(
        options: &IndexBuilderOptions,
        prefix_extractor: SliceTransform,
    ) -> Result<Self, Error> {
        if options.index_block_restart_interval != 1 {
            return Err(Error::InvalidArgument {
                what: "a hash-search index with an index restart interval other than one",
            });
        }
        Ok(Self {
            primary: ShortenedIndexBuilder::new(options, false)?,
            prefix_extractor,
            prefix_block: Vec::new(),
            prefix_meta_block: Vec::new(),
            pending_block_num: 0,
            pending_entry_index: 0,
            pending_entry_prefix: Vec::new(),
            current_restart_index: 0,
            prefix_blocks_size: 0,
        })
    }

    pub fn add_index_entry<'k>(
        &mut self,
        last_key: &'k [u8],
        first_key_in_next_block: Option<&[u8]>,
        handle: BlockHandle,
        scratch: &'k mut Vec<u8>,
        skip_delta_encoding: bool,
    ) -> Result<&'k [u8], Error> {
        self.current_restart_index = self.current_restart_index.saturating_add(1);
        self.primary.add_index_entry(
            last_key,
            first_key_in_next_block,
            handle,
            scratch,
            skip_delta_encoding,
        )
    }

    /// `OnKeyAdded` [R index_builder.h:574-600]: the internal key `key`'s prefix, counted into
    /// the run of blocks it belongs to.
    pub fn on_key_added(&mut self, key: &[u8]) -> Result<(), Error> {
        let user_key = extract_user_key(key)?;
        let prefix = self
            .prefix_extractor
            .transform(user_key)
            .ok_or(Error::InvalidArgument {
                what: "a key outside the prefix extractor's domain in a hash-search index",
            })?;
        let first = self.pending_block_num == 0;
        if first || self.pending_entry_prefix != prefix {
            if !first {
                self.flush_pending_prefix()?;
            }
            self.pending_entry_prefix.clear();
            self.pending_entry_prefix.extend_from_slice(prefix);
            self.pending_block_num = 1;
            self.pending_entry_index =
                u32::try_from(self.current_restart_index).map_err(|_| Error::LimitExceeded {
                    what: "a hash-search index's blocks",
                    limit: u64::from(u32::MAX),
                })?;
        } else {
            let last = u64::from(self.pending_entry_index)
                .saturating_add(u64::from(self.pending_block_num))
                .saturating_sub(1);
            if last != self.current_restart_index {
                self.pending_block_num = self.pending_block_num.saturating_add(1);
            }
        }
        Ok(())
    }

    /// `FlushPendingPrefix` [R index_builder.h:645-650].
    fn flush_pending_prefix(&mut self) -> Result<(), Error> {
        self.prefix_block
            .extend_from_slice(&self.pending_entry_prefix);
        let len =
            u32::try_from(self.pending_entry_prefix.len()).map_err(|_| Error::LimitExceeded {
                what: "a hash-search index's prefix",
                limit: u64::from(u32::MAX),
            })?;
        put_varint32(&mut self.prefix_meta_block, len);
        put_varint32(&mut self.prefix_meta_block, self.pending_entry_index);
        put_varint32(&mut self.prefix_meta_block, self.pending_block_num);
        Ok(())
    }

    /// `Finish` [R index_builder.h:602-614].
    pub fn finish(&mut self) -> Result<IndexBlocks, Error> {
        if self.pending_block_num != 0 {
            self.flush_pending_prefix()?;
        }
        let mut blocks = self.primary.finish()?;
        let prefixes = std::mem::take(&mut self.prefix_block);
        let metadata = std::mem::take(&mut self.prefix_meta_block);
        self.prefix_blocks_size = prefixes.len().saturating_add(metadata.len());
        blocks
            .meta_blocks
            .push((HASH_INDEX_PREFIXES_BLOCK, prefixes));
        blocks
            .meta_blocks
            .push((HASH_INDEX_PREFIXES_METADATA_BLOCK, metadata));
        Ok(blocks)
    }

    /// `IndexSize`: the index block and the two prefix blocks, once finished.
    pub fn index_size(&self) -> usize {
        self.primary
            .index_size()
            .saturating_add(self.prefix_blocks_size)
    }

    pub fn num_uniform_index_blocks(&self) -> u64 {
        self.primary.num_uniform_index_blocks()
    }

    /// RocksDB does not estimate a hash index's size.
    pub fn current_index_size_estimate(&self) -> u64 {
        0
    }

    pub fn separator_is_key_plus_seq(&self) -> bool {
        self.primary.separator_is_key_plus_seq()
    }
}

/// A partition being built, and the separator of its last entry.
#[derive(Debug)]
struct Partition {
    key: Vec<u8>,
    index: ShortenedIndexBuilder,
}

/// RocksDB's guess of a top-level index entry's size in its size estimate
/// [R index_builder.cc:396-398], kept so the port's table files are cut where RocksDB's are.
const TOP_LEVEL_ENTRY_GUESS: u64 = 70;

/// `PartitionedIndexBuilder` [R index_builder.h:665-781; index_builder.cc:133-421]: index
/// partitions cut at the metadata block size, then a top-level index over them.
#[derive(Debug)]
pub struct PartitionedIndexBuilder {
    options: IndexBuilderOptions,
    partitions: VecDeque<Partition>,
    top_with_seq: BlockBuilder,
    top_without_seq: BlockBuilder,
    flush_policy: FlushBlockBySizePolicy,
    finishing_indexes: bool,
    must_use_separator_with_seq: bool,
    partition_cut_requested: bool,
    cut_filter_block: bool,
    /// The handle of the partition last indexed; RocksDB starts it at `BlockHandle()`, all ones.
    last_encoded_handle: BlockHandle,
    top_level_index_size: usize,
    partition_count: usize,
    num_uniform_index_blocks: u64,
    index_size: usize,
    estimated_index_size: u64,
    estimated_completed_partitions_size: u64,
}

impl PartitionedIndexBuilder {
    pub fn new(options: &IndexBuilderOptions) -> Result<Self, Error> {
        // RocksDB builds the top level without the uniformity threshold, so it is never marked
        // uniform [R index_builder.cc:146-165].
        let top = IndexBuilderOptions {
            uniform_cv_threshold: None,
            ..*options
        };
        let mut builder = Self {
            options: *options,
            partitions: VecDeque::new(),
            top_with_seq: index_block_builder(&top, false)?,
            top_without_seq: index_block_builder(&top, true)?,
            flush_policy: FlushBlockBySizePolicy::new(
                options.metadata_block_size,
                options.block_size_deviation,
                false,
            )?,
            finishing_indexes: false,
            must_use_separator_with_seq: false,
            partition_cut_requested: true,
            cut_filter_block: false,
            last_encoded_handle: BlockHandle::new(u64::MAX, u64::MAX),
            top_level_index_size: 0,
            partition_count: 0,
            num_uniform_index_blocks: 0,
            index_size: 0,
            estimated_index_size: 0,
            estimated_completed_partitions_size: 0,
        };
        builder.new_partition()?;
        Ok(builder)
    }

    /// `MakeNewSubIndexBuilder` [R index_builder.cc:166-197].
    fn new_partition(&mut self) -> Result<(), Error> {
        let mut index = ShortenedIndexBuilder::new(&self.options, false)?;
        if self.must_use_separator_with_seq {
            index.must_use_separator_with_seq = true;
        }
        self.partitions.push_back(Partition {
            key: Vec::new(),
            index,
        });
        self.partition_cut_requested = false;
        Ok(())
    }

    /// The partition being built.
    fn current(&mut self) -> Result<&mut Partition, Error> {
        self.partitions.back_mut().ok_or(Error::InvalidArgument {
            what: "an index entry added to a finished partitioned index",
        })
    }

    /// The block the flush policy watches: the partition's, with or without sequence numbers as
    /// the whole index stands (`Retarget`).
    fn watched(&self) -> Option<&BlockBuilder> {
        let index = &self.partitions.back()?.index;
        Some(if self.must_use_separator_with_seq {
            &index.with_seq
        } else {
            &index.without_seq
        })
    }

    /// `RequestPartitionCut`.
    pub fn request_partition_cut(&mut self) {
        self.partition_cut_requested = true;
    }

    /// `ShouldCutFilterBlock`: whether a partition was cut since last asked.
    pub fn should_cut_filter_block(&mut self) -> bool {
        std::mem::take(&mut self.cut_filter_block)
    }

    /// `GetPartitionKey`: the separator of the partition being built.
    pub fn partition_key(&self) -> &[u8] {
        self.partitions.back().map_or(&[], |p| p.key.as_slice())
    }

    /// `MaybeFlush` [R index_builder.cc:217-232].
    fn maybe_flush(&mut self, index_key: &[u8], handle: BlockHandle) -> Result<(), Error> {
        let Some(partition) = self.partitions.back() else {
            return Ok(());
        };
        if partition.index.with_seq.is_empty() {
            return Ok(());
        }
        let cut = self.partition_cut_requested
            || self.watched().is_some_and(|b| {
                self.flush_policy
                    .update(b, index_key, handle.encoded().as_slice())
            });
        if cut {
            self.estimated_completed_partitions_size = self
                .estimated_completed_partitions_size
                .saturating_add(partition.index.current_index_size_estimate());
            self.cut_filter_block = true;
            self.new_partition()?;
        }
        Ok(())
    }

    /// `AddIndexEntry` [R index_builder.cc:259-290].
    pub fn add_index_entry<'k>(
        &mut self,
        last_key: &'k [u8],
        first_key_in_next_block: Option<&[u8]>,
        handle: BlockHandle,
        scratch: &'k mut Vec<u8>,
        skip_delta_encoding: bool,
    ) -> Result<&'k [u8], Error> {
        if first_key_in_next_block.is_some() {
            self.maybe_flush(last_key, handle)?;
        }
        let partition = self.current()?;
        let separator = partition.index.add_index_entry(
            last_key,
            first_key_in_next_block,
            handle,
            scratch,
            skip_delta_encoding,
        )?;
        partition.key.clear();
        partition.key.extend_from_slice(separator);
        let sub_must_use = partition.index.must_use_separator_with_seq;
        self.update_index_size_estimate();
        if !self.must_use_separator_with_seq && sub_must_use {
            self.must_use_separator_with_seq = true;
        }
        if first_key_in_next_block.is_none() {
            self.cut_filter_block = true;
        }
        Ok(separator)
    }

    /// `Finish` [R index_builder.cc:292-355]: the next partition, or the top-level index once
    /// every partition is out. `last_partition` is the handle the previous partition was written
    /// at; it is ignored on the first call.
    pub fn finish(&mut self, last_partition: BlockHandle) -> Result<Finished, Error> {
        if self.partition_count == 0 {
            if self
                .partitions
                .back()
                .is_some_and(|p| p.index.with_seq.is_empty())
            {
                self.partitions.pop_back();
            }
            self.partition_count = self.partitions.len();
        }
        if self.finishing_indexes {
            let last = self.partitions.pop_front().ok_or(Error::InvalidArgument {
                what: "a partitioned index finished past its end",
            })?;
            let encoded = last_partition.encoded();
            // RocksDB subtracts unsigned sizes and stores the result as a signed delta; the
            // first is never read, as it starts a restart interval.
            let delta_size = last_partition
                .size
                .wrapping_sub(self.last_encoded_handle.size);
            let mut delta = Vec::new();
            put_varsignedint64(&mut delta, i64::from_ne_bytes(delta_size.to_ne_bytes()));
            self.last_encoded_handle = last_partition;
            self.top_with_seq
                .add(&last.key, encoded.as_slice(), Some(&delta), false)?;
            if !self.must_use_separator_with_seq {
                self.top_without_seq.add(
                    extract_user_key(&last.key)?,
                    encoded.as_slice(),
                    Some(&delta),
                    false,
                )?;
            }
        }
        let must_use = self.must_use_separator_with_seq;
        match self.partitions.front_mut() {
            None => {
                let top = if must_use {
                    &mut self.top_with_seq
                } else {
                    &mut self.top_without_seq
                };
                let index_block = top.finish()?.to_vec();
                self.num_uniform_index_blocks = self
                    .num_uniform_index_blocks
                    .saturating_add(u64::from(top.is_uniform()));
                self.top_level_index_size = index_block.len();
                self.index_size = self.index_size.saturating_add(index_block.len());
                Ok(Finished::Done(IndexBlocks {
                    index_block,
                    meta_blocks: Vec::new(),
                }))
            }
            Some(partition) => {
                partition.index.must_use_separator_with_seq = must_use;
                let blocks = partition.index.finish()?;
                self.num_uniform_index_blocks = self
                    .num_uniform_index_blocks
                    .saturating_add(partition.index.num_uniform_index_blocks());
                self.index_size = self.index_size.saturating_add(blocks.index_block.len());
                self.finishing_indexes = true;
                Ok(Finished::Partition(blocks))
            }
        }
    }

    /// `IndexSize`: every partition and the top level, once finished.
    pub fn index_size(&self) -> usize {
        self.index_size
    }

    /// `TopLevelIndexSize`.
    pub fn top_level_index_size(&self) -> usize {
        self.top_level_index_size
    }

    /// `NumPartitions`.
    pub fn num_partitions(&self) -> usize {
        self.partition_count
    }

    pub fn num_uniform_index_blocks(&self) -> u64 {
        self.num_uniform_index_blocks
    }

    pub fn current_index_size_estimate(&self) -> u64 {
        self.estimated_index_size
    }

    pub fn separator_is_key_plus_seq(&self) -> bool {
        self.must_use_separator_with_seq
    }

    /// `UpdateIndexSizeEstimate` [R index_builder.cc:376-419].
    fn update_index_size_estimate(&mut self) {
        let completed = u64::try_from(self.partitions.len().saturating_sub(1)).unwrap_or(u64::MAX);
        let completed_size = self.estimated_completed_partitions_size;
        let current = self
            .partitions
            .back()
            .map_or(0, |p| p.index.current_index_size_estimate());
        let mut total = completed_size.saturating_add(current);
        if completed > 0 {
            let top = completed.saturating_mul(TOP_LEVEL_ENTRY_GUESS);
            total = total.saturating_add(top);
            let mean_partition = completed_size.checked_div(completed).unwrap_or(0);
            let mean_top = top.checked_div(completed).unwrap_or(0);
            total = total.saturating_add(mean_partition.saturating_add(mean_top).saturating_mul(2));
        } else if !self.partitions.is_empty() {
            total = total.saturating_add(current.saturating_mul(2));
        }
        self.estimated_index_size = total;
    }
}

/// `IndexBuilder::CreateIndexBuilder` [R index_builder.cc:21-75]: the builder for an index type.
#[derive(Debug)]
pub enum IndexBuilder {
    Shortened(ShortenedIndexBuilder),
    Hash(HashIndexBuilder),
    Partitioned(PartitionedIndexBuilder),
}

impl IndexBuilder {
    /// A hash-search index needs `prefix_extractor`.
    pub fn new(
        index_type: IndexType,
        options: &IndexBuilderOptions,
        prefix_extractor: Option<SliceTransform>,
    ) -> Result<Self, Error> {
        Ok(match index_type {
            IndexType::BinarySearch => Self::Shortened(ShortenedIndexBuilder::new(options, false)?),
            IndexType::BinarySearchWithFirstKey => {
                Self::Shortened(ShortenedIndexBuilder::new(options, true)?)
            }
            IndexType::HashSearch => Self::Hash(HashIndexBuilder::new(
                options,
                prefix_extractor.ok_or(Error::InvalidArgument {
                    what: "a hash-search index without a prefix extractor",
                })?,
            )?),
            IndexType::TwoLevelIndexSearch => {
                Self::Partitioned(PartitionedIndexBuilder::new(options)?)
            }
        })
    }

    pub fn add_index_entry<'k>(
        &mut self,
        last_key: &'k [u8],
        first_key_in_next_block: Option<&[u8]>,
        handle: BlockHandle,
        scratch: &'k mut Vec<u8>,
        skip_delta_encoding: bool,
    ) -> Result<&'k [u8], Error> {
        match self {
            Self::Shortened(b) => b.add_index_entry(
                last_key,
                first_key_in_next_block,
                handle,
                scratch,
                skip_delta_encoding,
            ),
            Self::Hash(b) => b.add_index_entry(
                last_key,
                first_key_in_next_block,
                handle,
                scratch,
                skip_delta_encoding,
            ),
            Self::Partitioned(b) => b.add_index_entry(
                last_key,
                first_key_in_next_block,
                handle,
                scratch,
                skip_delta_encoding,
            ),
        }
    }

    /// `OnKeyAdded`.
    pub fn on_key_added(&mut self, key: &[u8]) -> Result<(), Error> {
        match self {
            Self::Shortened(b) => b.on_key_added(key),
            Self::Hash(b) => b.on_key_added(key)?,
            Self::Partitioned(_) => {}
        }
        Ok(())
    }

    /// `Finish`: see [`PartitionedIndexBuilder::finish`] for `last_partition`.
    pub fn finish(&mut self, last_partition: BlockHandle) -> Result<Finished, Error> {
        match self {
            Self::Shortened(b) => b.finish().map(Finished::Done),
            Self::Hash(b) => b.finish().map(Finished::Done),
            Self::Partitioned(b) => b.finish(last_partition),
        }
    }

    pub fn index_size(&self) -> usize {
        match self {
            Self::Shortened(b) => b.index_size(),
            Self::Hash(b) => b.index_size(),
            Self::Partitioned(b) => b.index_size(),
        }
    }

    pub fn num_uniform_index_blocks(&self) -> u64 {
        match self {
            Self::Shortened(b) => b.num_uniform_index_blocks(),
            Self::Hash(b) => b.num_uniform_index_blocks(),
            Self::Partitioned(b) => b.num_uniform_index_blocks(),
        }
    }

    pub fn current_index_size_estimate(&self) -> u64 {
        match self {
            Self::Shortened(b) => b.current_index_size_estimate(),
            Self::Hash(b) => b.current_index_size_estimate(),
            Self::Partitioned(b) => b.current_index_size_estimate(),
        }
    }

    pub fn separator_is_key_plus_seq(&self) -> bool {
        match self {
            Self::Shortened(b) => b.separator_is_key_plus_seq(),
            Self::Hash(b) => b.separator_is_key_plus_seq(),
            Self::Partitioned(b) => b.separator_is_key_plus_seq(),
        }
    }
}
