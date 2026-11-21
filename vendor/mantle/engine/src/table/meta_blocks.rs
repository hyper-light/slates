//! `MetaIndexBuilder`, `PropertyBlockBuilder`, `ParsePropertiesBlock` and `FindMetaBlock` of
//! `table/meta_blocks.{h,cc}` [R meta_blocks.cc:25-213, :286-440, :580-606]: the block that
//! names a table's meta blocks, and the properties block.
//!
//! Both builders keep their entries sorted bytewise and write them at `Finish`, as RocksDB's
//! `std::map`s do. RocksDB keeps the first of two entries of one name and asserts against the
//! second; the port refuses the second.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::error::{Error, Malformed};
use crate::table::block_based::block::Block;
use crate::table::block_based::block_builder::{BlockBuilder, BlockBuilderOptions};
use crate::table::format::BlockHandle;
use crate::table::table_properties::{TableProperties, names};
use crate::util::coding::{get_varint64, put_varint64};

/// `kPropertiesBlockName` [R meta_blocks.cc:28].
pub const PROPERTIES_BLOCK: &[u8] = b"rocksdb.properties";
/// `kIndexBlockName` [R meta_blocks.cc:29].
pub const INDEX_BLOCK: &[u8] = b"rocksdb.index";
/// `kCompressionDictBlockName` [R meta_blocks.cc:30].
pub const COMPRESSION_DICT_BLOCK: &[u8] = b"rocksdb.compression_dict";
/// `kRangeDelBlockName` [R meta_blocks.cc:31].
pub const RANGE_DEL_BLOCK: &[u8] = b"rocksdb.range_del";

fn insert_once(
    map: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    name: &[u8],
    value: Vec<u8>,
    what: &'static str,
) -> Result<(), Error> {
    match map.entry(name.to_vec()) {
        Entry::Vacant(v) => {
            v.insert(value);
            Ok(())
        }
        Entry::Occupied(_) => Err(Error::InvalidArgument { what }),
    }
}

/// `MetaIndexBuilder` [R meta_blocks.cc:33-50]: the meta blocks' handles by name, in a block of
/// restart interval one.
#[derive(Debug, Default)]
pub struct MetaIndexBuilder {
    handles: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl MetaIndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Add`.
    pub fn add(&mut self, name: &[u8], handle: BlockHandle) -> Result<(), Error> {
        let mut encoded = Vec::new();
        handle.encode_to(&mut encoded);
        insert_once(&mut self.handles, name, encoded, "a meta block named twice")
    }

    /// `Finish`.
    pub fn finish(self) -> Result<Vec<u8>, Error> {
        let mut block = BlockBuilder::new(BlockBuilderOptions {
            restart_interval: 1,
            ..BlockBuilderOptions::default()
        })?;
        for (name, handle) in &self.handles {
            block.add(name, handle, None, false)?;
        }
        Ok(block.finish()?.to_vec())
    }
}

/// `PropertyBlockBuilder` [R meta_blocks.cc:52-213]: properties by name, in one restart
/// interval (`INT32_MAX` entries).
#[derive(Debug, Default)]
pub struct PropertyBlockBuilder {
    props: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl PropertyBlockBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Add(name, string)`.
    pub fn add(&mut self, name: &[u8], value: &[u8]) -> Result<(), Error> {
        insert_once(
            &mut self.props,
            name,
            value.to_vec(),
            "a table property named twice",
        )
    }

    /// `Add(name, uint64_t)`: the number as a varint64.
    pub fn add_u64(&mut self, name: &[u8], value: u64) -> Result<(), Error> {
        let mut v = Vec::new();
        put_varint64(&mut v, value);
        self.add(name, &v)
    }

    /// `Add(UserCollectedProperties)`.
    pub fn add_user_collected(
        &mut self,
        properties: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<(), Error> {
        for (name, value) in properties {
            self.add(name, value)?;
        }
        Ok(())
    }

    /// `AddTableProperty` [R meta_blocks.cc:79-198]: the table's own properties, each optional one
    /// only when set.
    pub fn add_table_property(&mut self, p: &TableProperties) -> Result<(), Error> {
        use names::*;
        self.add_u64(ORIGINAL_FILE_NUMBER, p.orig_file_number)?;
        self.add_u64(RAW_KEY_SIZE, p.raw_key_size)?;
        self.add_u64(RAW_VALUE_SIZE, p.raw_value_size)?;
        self.add_u64(DATA_SIZE, p.data_size)?;
        self.add_u64(INDEX_SIZE, p.index_size)?;
        if p.index_partitions != 0 {
            self.add_u64(INDEX_PARTITIONS, p.index_partitions)?;
            self.add_u64(TOP_LEVEL_INDEX_SIZE, p.top_level_index_size)?;
        }
        self.add_u64(INDEX_KEY_IS_USER_KEY, p.index_key_is_user_key)?;
        self.add_u64(INDEX_VALUE_IS_DELTA_ENCODED, p.index_value_is_delta_encoded)?;
        if p.udi_is_primary_index != 0 {
            self.add_u64(UDI_IS_PRIMARY_INDEX, p.udi_is_primary_index)?;
        }
        self.add_u64(NUM_ENTRIES, p.num_entries)?;
        self.add_u64(NUM_FILTER_ENTRIES, p.num_filter_entries)?;
        self.add_u64(DELETED_KEYS, p.num_deletions)?;
        self.add_u64(MERGE_OPERANDS, p.num_merge_operands)?;
        self.add_u64(NUM_RANGE_DELETIONS, p.num_range_deletions)?;
        self.add_u64(NUM_DATA_BLOCKS, p.num_data_blocks)?;
        if p.num_data_blocks_compression_rejected > 0 {
            self.add_u64(
                NUM_DATA_BLOCKS_COMPRESSION_REJECTED,
                p.num_data_blocks_compression_rejected,
            )?;
        }
        if p.num_data_blocks_compression_bypassed > 0 {
            self.add_u64(
                NUM_DATA_BLOCKS_COMPRESSION_BYPASSED,
                p.num_data_blocks_compression_bypassed,
            )?;
        }
        self.add_u64(NUM_UNIFORM_BLOCKS, p.num_uniform_blocks)?;
        self.add_u64(FILTER_SIZE, p.filter_size)?;
        self.add_u64(FORMAT_VERSION, p.format_version)?;
        self.add_u64(FIXED_KEY_LEN, p.fixed_key_len)?;
        self.add_u64(COLUMN_FAMILY_ID, p.column_family_id)?;
        self.add_u64(CREATION_TIME, p.creation_time)?;
        self.add_u64(OLDEST_KEY_TIME, p.oldest_key_time)?;
        self.add_u64(NEWEST_KEY_TIME, p.newest_key_time)?;
        if p.file_creation_time > 0 {
            self.add_u64(FILE_CREATION_TIME, p.file_creation_time)?;
        }
        if p.slow_compression_estimated_data_size > 0 {
            self.add_u64(
                SLOW_COMPRESSION_ESTIMATED_DATA_SIZE,
                p.slow_compression_estimated_data_size,
            )?;
        }
        if p.fast_compression_estimated_data_size > 0 {
            self.add_u64(
                FAST_COMPRESSION_ESTIMATED_DATA_SIZE,
                p.fast_compression_estimated_data_size,
            )?;
        }
        self.add_u64(TAIL_START_OFFSET, p.tail_start_offset)?;
        if p.user_defined_timestamps_persisted == 0 {
            self.add_u64(
                USER_DEFINED_TIMESTAMPS_PERSISTED,
                p.user_defined_timestamps_persisted,
            )?;
        }
        for (name, value) in [
            (DB_ID, &p.db_id),
            (DB_SESSION_ID, &p.db_session_id),
            (DB_HOST_ID, &p.db_host_id),
            (FILTER_POLICY, &p.filter_policy_name),
            (COMPARATOR, &p.comparator_name),
            (MERGE_OPERATOR, &p.merge_operator_name),
            (PREFIX_EXTRACTOR_NAME, &p.prefix_extractor_name),
            (PROPERTY_COLLECTORS, &p.property_collectors_names),
            (COLUMN_FAMILY_NAME, &p.column_family_name),
            (COMPRESSION, &p.compression_name),
            (COMPRESSION_OPTIONS, &p.compression_options),
            (SEQUENCE_NUMBER_TIME_MAPPING, &p.seqno_to_time_mapping),
        ] {
            if !value.is_empty() {
                self.add(name, value)?;
            }
        }
        if p.key_largest_seqno != u64::MAX {
            self.add_u64(KEY_LARGEST_SEQNO, p.key_largest_seqno)?;
        }
        if p.key_smallest_seqno != u64::MAX {
            self.add_u64(KEY_SMALLEST_SEQNO, p.key_smallest_seqno)?;
        }
        if p.data_block_restart_interval > 0 {
            self.add_u64(DATA_BLOCK_RESTART_INTERVAL, p.data_block_restart_interval)?;
        }
        if p.index_block_restart_interval > 0 {
            self.add_u64(INDEX_BLOCK_RESTART_INTERVAL, p.index_block_restart_interval)?;
        }
        if p.separate_key_value_in_data_block > 0 {
            self.add_u64(
                SEPARATE_KEY_VALUE_IN_DATA_BLOCK,
                p.separate_key_value_in_data_block,
            )?;
        }
        Ok(())
    }

    /// `Finish`.
    pub fn finish(self) -> Result<Vec<u8>, Error> {
        let mut block = BlockBuilder::new(BlockBuilderOptions {
            restart_interval: u32::try_from(i32::MAX).unwrap_or(u32::MAX),
            ..BlockBuilderOptions::default()
        })?;
        for (name, value) in &self.props {
            block.add(name, value, None, false)?;
        }
        Ok(block.finish()?.to_vec())
    }
}

/// The number field a property name stores, among `ParsePropertiesBlock`'s
/// `predefined_uint64_properties` [R meta_blocks.cc:292-361].
fn number_field<'p>(p: &'p mut TableProperties, name: &[u8]) -> Option<&'p mut u64> {
    use names::*;
    Some(match name {
        ORIGINAL_FILE_NUMBER => &mut p.orig_file_number,
        DATA_SIZE => &mut p.data_size,
        INDEX_SIZE => &mut p.index_size,
        INDEX_PARTITIONS => &mut p.index_partitions,
        TOP_LEVEL_INDEX_SIZE => &mut p.top_level_index_size,
        INDEX_KEY_IS_USER_KEY => &mut p.index_key_is_user_key,
        INDEX_VALUE_IS_DELTA_ENCODED => &mut p.index_value_is_delta_encoded,
        UDI_IS_PRIMARY_INDEX => &mut p.udi_is_primary_index,
        FILTER_SIZE => &mut p.filter_size,
        RAW_KEY_SIZE => &mut p.raw_key_size,
        RAW_VALUE_SIZE => &mut p.raw_value_size,
        NUM_DATA_BLOCKS => &mut p.num_data_blocks,
        NUM_DATA_BLOCKS_COMPRESSION_REJECTED => &mut p.num_data_blocks_compression_rejected,
        NUM_DATA_BLOCKS_COMPRESSION_BYPASSED => &mut p.num_data_blocks_compression_bypassed,
        NUM_UNIFORM_BLOCKS => &mut p.num_uniform_blocks,
        NUM_ENTRIES => &mut p.num_entries,
        NUM_FILTER_ENTRIES => &mut p.num_filter_entries,
        DELETED_KEYS => &mut p.num_deletions,
        MERGE_OPERANDS => &mut p.num_merge_operands,
        NUM_RANGE_DELETIONS => &mut p.num_range_deletions,
        FORMAT_VERSION => &mut p.format_version,
        FIXED_KEY_LEN => &mut p.fixed_key_len,
        COLUMN_FAMILY_ID => &mut p.column_family_id,
        CREATION_TIME => &mut p.creation_time,
        OLDEST_KEY_TIME => &mut p.oldest_key_time,
        NEWEST_KEY_TIME => &mut p.newest_key_time,
        FILE_CREATION_TIME => &mut p.file_creation_time,
        SLOW_COMPRESSION_ESTIMATED_DATA_SIZE => &mut p.slow_compression_estimated_data_size,
        FAST_COMPRESSION_ESTIMATED_DATA_SIZE => &mut p.fast_compression_estimated_data_size,
        TAIL_START_OFFSET => &mut p.tail_start_offset,
        USER_DEFINED_TIMESTAMPS_PERSISTED => &mut p.user_defined_timestamps_persisted,
        KEY_LARGEST_SEQNO => &mut p.key_largest_seqno,
        KEY_SMALLEST_SEQNO => &mut p.key_smallest_seqno,
        DATA_BLOCK_RESTART_INTERVAL => &mut p.data_block_restart_interval,
        INDEX_BLOCK_RESTART_INTERVAL => &mut p.index_block_restart_interval,
        SEPARATE_KEY_VALUE_IN_DATA_BLOCK => &mut p.separate_key_value_in_data_block,
        _ => return None,
    })
}

/// The string field a property name stores [R meta_blocks.cc:404-430].
fn string_field<'p>(p: &'p mut TableProperties, name: &[u8]) -> Option<&'p mut Vec<u8>> {
    use names::*;
    Some(match name {
        DB_ID => &mut p.db_id,
        DB_SESSION_ID => &mut p.db_session_id,
        DB_HOST_ID => &mut p.db_host_id,
        FILTER_POLICY => &mut p.filter_policy_name,
        COLUMN_FAMILY_NAME => &mut p.column_family_name,
        COMPARATOR => &mut p.comparator_name,
        MERGE_OPERATOR => &mut p.merge_operator_name,
        PREFIX_EXTRACTOR_NAME => &mut p.prefix_extractor_name,
        PROPERTY_COLLECTORS => &mut p.property_collectors_names,
        COMPRESSION => &mut p.compression_name,
        COMPRESSION_OPTIONS => &mut p.compression_options,
        SEQUENCE_NUMBER_TIME_MAPPING => &mut p.seqno_to_time_mapping,
        _ => return None,
    })
}

/// `ParsePropertiesBlock` [R meta_blocks.cc:286-440]: the properties of `block`, the
/// uncompressed properties block, which lies at `offset` in its file. Names must ascend; a
/// number that does not decode is skipped and named in `malformed`, as RocksDB logs it.
pub fn parse_properties_block(block: &Block, offset: u64) -> Result<TableProperties, Error> {
    let mut props = TableProperties::default();
    let mut iter = block.new_meta_iterator();
    let mut last: Option<Vec<u8>> = None;
    iter.seek_to_first();
    while iter.valid() {
        let key = iter.key();
        if last.as_deref().is_some_and(|l| key <= l) {
            return Err(Error::corruption(
                "properties unsorted",
                Malformed::OutOfOrder,
            ));
        }
        last = Some(key.to_vec());
        let value = iter.value();
        if key == names::EXTERNAL_SST_FILE_GLOBAL_SEQNO {
            props.external_sst_file_global_seqno_offset = offset
                .checked_add(u64::from(iter.value_offset()))
                .ok_or_else(|| Error::corruption("properties block offset", Malformed::TooLarge))?;
        }
        if key == names::DELETED_KEYS || key == names::MERGE_OPERANDS {
            props
                .user_collected_properties
                .insert(key.to_vec(), value.to_vec());
        }
        if let Some(field) = number_field(&mut props, key) {
            let mut v = value;
            match get_varint64(&mut v) {
                Ok(n) => *field = n,
                Err(_) => props.malformed.push(key.to_vec()),
            }
        } else if let Some(field) = string_field(&mut props, key) {
            *field = value.to_vec();
        } else {
            props
                .user_collected_properties
                .insert(key.to_vec(), value.to_vec());
        }
        iter.next();
    }
    iter.status()?;
    Ok(props)
}

/// `FindOptionalMetaBlock` [R meta_blocks.cc:580-594]: the handle the meta-index block `block`
/// gives for `name`, if it names it.
pub fn find_meta_block(block: &Block, name: &[u8]) -> Result<Option<BlockHandle>, Error> {
    let mut iter = block.new_meta_iterator();
    iter.seek(name);
    iter.status()?;
    if iter.valid() && iter.key() == name {
        let mut v = iter.value();
        return BlockHandle::decode_from(&mut v).map(Some);
    }
    Ok(None)
}
