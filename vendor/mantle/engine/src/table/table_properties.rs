//! `TableProperties` and the names its properties are stored under
//! [R include/rocksdb/table_properties.h:255-380; table/table_properties.cc:311-395;
//! table/sst_file_writer.cc:27-31].
//!
//! The properties are bytes as RocksDB keeps them (`std::string`), so a name or value that is not
//! UTF-8 survives a read and a write unchanged.

use std::collections::BTreeMap;

/// `TablePropertiesNames` [R table/table_properties.cc:311-395].
pub mod names {
    pub const DB_ID: &[u8] = b"rocksdb.creating.db.identity";
    pub const DB_SESSION_ID: &[u8] = b"rocksdb.creating.session.identity";
    pub const DB_HOST_ID: &[u8] = b"rocksdb.creating.host.identity";
    pub const ORIGINAL_FILE_NUMBER: &[u8] = b"rocksdb.original.file.number";
    pub const DATA_SIZE: &[u8] = b"rocksdb.data.size";
    pub const INDEX_SIZE: &[u8] = b"rocksdb.index.size";
    pub const INDEX_PARTITIONS: &[u8] = b"rocksdb.index.partitions";
    pub const TOP_LEVEL_INDEX_SIZE: &[u8] = b"rocksdb.top-level.index.size";
    pub const INDEX_KEY_IS_USER_KEY: &[u8] = b"rocksdb.index.key.is.user.key";
    pub const INDEX_VALUE_IS_DELTA_ENCODED: &[u8] = b"rocksdb.index.value.is.delta.encoded";
    pub const UDI_IS_PRIMARY_INDEX: &[u8] = b"rocksdb.udi.is.primary.index";
    pub const FILTER_SIZE: &[u8] = b"rocksdb.filter.size";
    pub const RAW_KEY_SIZE: &[u8] = b"rocksdb.raw.key.size";
    pub const RAW_VALUE_SIZE: &[u8] = b"rocksdb.raw.value.size";
    pub const NUM_DATA_BLOCKS: &[u8] = b"rocksdb.num.data.blocks";
    pub const NUM_DATA_BLOCKS_COMPRESSION_REJECTED: &[u8] =
        b"rocksdb.num.data.blocks.compression.rejected";
    pub const NUM_DATA_BLOCKS_COMPRESSION_BYPASSED: &[u8] =
        b"rocksdb.num.data.blocks.compression.bypassed";
    pub const NUM_UNIFORM_BLOCKS: &[u8] = b"rocksdb.num.uniform.blocks";
    pub const NUM_ENTRIES: &[u8] = b"rocksdb.num.entries";
    pub const NUM_FILTER_ENTRIES: &[u8] = b"rocksdb.num.filter_entries";
    pub const DELETED_KEYS: &[u8] = b"rocksdb.deleted.keys";
    pub const MERGE_OPERANDS: &[u8] = b"rocksdb.merge.operands";
    pub const NUM_RANGE_DELETIONS: &[u8] = b"rocksdb.num.range-deletions";
    pub const FILTER_POLICY: &[u8] = b"rocksdb.filter.policy";
    pub const FORMAT_VERSION: &[u8] = b"rocksdb.format.version";
    pub const FIXED_KEY_LEN: &[u8] = b"rocksdb.fixed.key.length";
    pub const COLUMN_FAMILY_ID: &[u8] = b"rocksdb.column.family.id";
    pub const COLUMN_FAMILY_NAME: &[u8] = b"rocksdb.column.family.name";
    pub const COMPARATOR: &[u8] = b"rocksdb.comparator";
    pub const MERGE_OPERATOR: &[u8] = b"rocksdb.merge.operator";
    pub const PREFIX_EXTRACTOR_NAME: &[u8] = b"rocksdb.prefix.extractor.name";
    pub const PROPERTY_COLLECTORS: &[u8] = b"rocksdb.property.collectors";
    pub const COMPRESSION: &[u8] = b"rocksdb.compression";
    pub const COMPRESSION_OPTIONS: &[u8] = b"rocksdb.compression_options";
    pub const CREATION_TIME: &[u8] = b"rocksdb.creation.time";
    pub const OLDEST_KEY_TIME: &[u8] = b"rocksdb.oldest.key.time";
    pub const NEWEST_KEY_TIME: &[u8] = b"rocksdb.newest.key.time";
    pub const FILE_CREATION_TIME: &[u8] = b"rocksdb.file.creation.time";
    pub const SLOW_COMPRESSION_ESTIMATED_DATA_SIZE: &[u8] =
        b"rocksdb.sample_for_compression.slow.data.size";
    pub const FAST_COMPRESSION_ESTIMATED_DATA_SIZE: &[u8] =
        b"rocksdb.sample_for_compression.fast.data.size";
    pub const SEQUENCE_NUMBER_TIME_MAPPING: &[u8] = b"rocksdb.seqno.time.map";
    pub const TAIL_START_OFFSET: &[u8] = b"rocksdb.tail.start.offset";
    pub const USER_DEFINED_TIMESTAMPS_PERSISTED: &[u8] =
        b"rocksdb.user.defined.timestamps.persisted";
    pub const KEY_LARGEST_SEQNO: &[u8] = b"rocksdb.key.largest.seqno";
    pub const KEY_SMALLEST_SEQNO: &[u8] = b"rocksdb.key.smallest.seqno";
    pub const DATA_BLOCK_RESTART_INTERVAL: &[u8] = b"rocksdb.data.block.restart.interval";
    pub const INDEX_BLOCK_RESTART_INTERVAL: &[u8] = b"rocksdb.index.block.restart.interval";
    pub const SEPARATE_KEY_VALUE_IN_DATA_BLOCK: &[u8] = b"rocksdb.separate.key.value.in.data.block";
    /// `ExternalSstFilePropertyNames::kVersion` [R table/sst_file_writer.cc:27-28].
    pub const EXTERNAL_SST_FILE_VERSION: &[u8] = b"rocksdb.external_sst_file.version";
    /// `ExternalSstFilePropertyNames::kGlobalSeqno` [R table/sst_file_writer.cc:29-30].
    pub const EXTERNAL_SST_FILE_GLOBAL_SEQNO: &[u8] = b"rocksdb.external_sst_file.global_seqno";
}

/// `TablePropertiesCollectorFactory::Context::kUnknownColumnFamily`, `INT32_MAX`
/// [R table/table_properties.cc:22-23].
pub const UNKNOWN_COLUMN_FAMILY: u64 = i32::MAX as u64;

/// `TableProperties` [R include/rocksdb/table_properties.h:255-380], less the derived
/// `uncompressed_data_size` and the collectors' `readable_properties`, which no file stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableProperties {
    pub orig_file_number: u64,
    pub data_size: u64,
    pub index_size: u64,
    pub index_partitions: u64,
    pub top_level_index_size: u64,
    pub index_key_is_user_key: u64,
    pub index_value_is_delta_encoded: u64,
    pub udi_is_primary_index: u64,
    pub filter_size: u64,
    pub raw_key_size: u64,
    pub raw_value_size: u64,
    pub num_data_blocks: u64,
    pub num_data_blocks_compression_rejected: u64,
    pub num_data_blocks_compression_bypassed: u64,
    pub num_uniform_blocks: u64,
    pub num_entries: u64,
    pub num_filter_entries: u64,
    pub num_deletions: u64,
    pub num_merge_operands: u64,
    pub num_range_deletions: u64,
    pub format_version: u64,
    pub fixed_key_len: u64,
    pub column_family_id: u64,
    pub creation_time: u64,
    pub oldest_key_time: u64,
    pub newest_key_time: u64,
    pub file_creation_time: u64,
    pub slow_compression_estimated_data_size: u64,
    pub fast_compression_estimated_data_size: u64,
    /// Where in the file the global sequence number property's value is, when the table has one.
    pub external_sst_file_global_seqno_offset: u64,
    pub tail_start_offset: u64,
    pub user_defined_timestamps_persisted: u64,
    /// `u64::MAX` when unknown.
    pub key_largest_seqno: u64,
    /// `u64::MAX` when unknown.
    pub key_smallest_seqno: u64,
    pub data_block_restart_interval: u64,
    pub index_block_restart_interval: u64,
    pub separate_key_value_in_data_block: u64,
    pub db_id: Vec<u8>,
    pub db_session_id: Vec<u8>,
    pub db_host_id: Vec<u8>,
    pub column_family_name: Vec<u8>,
    pub filter_policy_name: Vec<u8>,
    pub comparator_name: Vec<u8>,
    pub merge_operator_name: Vec<u8>,
    pub prefix_extractor_name: Vec<u8>,
    pub property_collectors_names: Vec<u8>,
    pub compression_name: Vec<u8>,
    pub compression_options: Vec<u8>,
    pub seqno_to_time_mapping: Vec<u8>,
    /// Every property the table holds that is not one of the above, by name.
    pub user_collected_properties: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Properties whose stored number did not decode, which RocksDB logs and skips, leaving the
    /// field at its default [R table/meta_blocks.cc:395-403].
    pub malformed: Vec<Vec<u8>>,
}

impl Default for TableProperties {
    /// RocksDB's member initializers.
    fn default() -> Self {
        Self {
            orig_file_number: 0,
            data_size: 0,
            index_size: 0,
            index_partitions: 0,
            top_level_index_size: 0,
            index_key_is_user_key: 0,
            index_value_is_delta_encoded: 0,
            udi_is_primary_index: 0,
            filter_size: 0,
            raw_key_size: 0,
            raw_value_size: 0,
            num_data_blocks: 0,
            num_data_blocks_compression_rejected: 0,
            num_data_blocks_compression_bypassed: 0,
            num_uniform_blocks: 0,
            num_entries: 0,
            num_filter_entries: 0,
            num_deletions: 0,
            num_merge_operands: 0,
            num_range_deletions: 0,
            format_version: 0,
            fixed_key_len: 0,
            column_family_id: UNKNOWN_COLUMN_FAMILY,
            creation_time: 0,
            oldest_key_time: 0,
            newest_key_time: 0,
            file_creation_time: 0,
            slow_compression_estimated_data_size: 0,
            fast_compression_estimated_data_size: 0,
            external_sst_file_global_seqno_offset: 0,
            tail_start_offset: 0,
            user_defined_timestamps_persisted: 1,
            key_largest_seqno: u64::MAX,
            key_smallest_seqno: u64::MAX,
            data_block_restart_interval: 0,
            index_block_restart_interval: 0,
            separate_key_value_in_data_block: 0,
            db_id: Vec::new(),
            db_session_id: Vec::new(),
            db_host_id: Vec::new(),
            column_family_name: Vec::new(),
            filter_policy_name: Vec::new(),
            comparator_name: Vec::new(),
            merge_operator_name: Vec::new(),
            prefix_extractor_name: Vec::new(),
            property_collectors_names: Vec::new(),
            compression_name: Vec::new(),
            compression_options: Vec::new(),
            seqno_to_time_mapping: Vec::new(),
            user_collected_properties: BTreeMap::new(),
            malformed: Vec::new(),
        }
    }
}
