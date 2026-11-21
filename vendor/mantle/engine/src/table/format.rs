//! Block checksums: `ChecksumType` of `include/rocksdb/table.h` [R :117-123], and
//! `ComputeBuiltinChecksum`, `ComputeBuiltinChecksumWithLastByte` and
//! `ChecksumModifierForContext` of `table/format.{h,cc}` (docs/research/24 §1.2, §1.7).
//!
//! A block's trailer stores the checksum of the block and its one compression-type byte. From
//! format_version 6 each stored checksum also carries a modifier from the file's random base and
//! the block's offset, so a block read from the wrong place, or from another file, fails its
//! check [R table/format.h:143-170].
//!
//! Also `BlockHandle`, `IndexValue`, `FooterBuilder` and `Footer` of `table/format.{h,cc}`
//! [R table/format.h:41-361, table/format.cc:50-473]: where a block lies in a file, an index
//! entry's value, and the fixed tail of every table. RocksDB checks several of the footer's
//! preconditions only with `assert`, compiled out of a release build; each is a typed error
//! here. Plain and cuckoo tables are recognised by their magic numbers and refused until their
//! phase (docs/research/24 §1.9).

use crate::error::{Error, Malformed};
use crate::util::coding::{
    MAX_VARINT64_LENGTH, decode_fixed32, decode_fixed64, encode_fixed32, encode_fixed64,
    encode_varint64, get_length_prefixed_slice, get_varint64, get_varsignedint64,
    put_length_prefixed_slice, put_varint64_varint64, put_varsignedint64,
};
use crate::util::crc32c;
use crate::util::hash::{lower32of64, upper32of64};
use crate::util::xxhash::{xxh3_64bits, xxh32, xxh32_with_last_byte, xxh64, xxh64_with_last_byte};

/// The checksum a table's blocks carry [R include/rocksdb/table.h:117-123]; the byte is stored
/// in the table's properties and footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumType {
    NoChecksum,
    Crc32c,
    XxHash,
    XxHash64,
    /// The default [R include/rocksdb/table.h:374]; RocksDB 6.27 and later.
    Xxh3,
}

impl ChecksumType {
    /// The stored byte.
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::NoChecksum => 0x0,
            Self::Crc32c => 0x1,
            Self::XxHash => 0x2,
            Self::XxHash64 => 0x3,
            Self::Xxh3 => 0x4,
        }
    }

    /// The type a stored byte names. RocksDB computes 0 for an unknown one and so reports a
    /// checksum mismatch; here it is refused as unsupported.
    pub const fn from_byte(b: u8) -> Result<Self, Error> {
        match b {
            0x0 => Ok(Self::NoChecksum),
            0x1 => Ok(Self::Crc32c),
            0x2 => Ok(Self::XxHash),
            0x3 => Ok(Self::XxHash64),
            0x4 => Ok(Self::Xxh3),
            other => Err(Error::Unsupported {
                feature: "block checksum type",
                value: other as u64,
            }),
        }
    }
}

/// The multiplier of `ModifyChecksumForLastByte` [R table/format.cc:606-612].
pub const LAST_BYTE_PRIME: u32 = 0x6b90_83d9;

/// `ModifyChecksumForLastByte` [R table/format.cc:606-612]: folds one more byte into an XXH3
/// checksum; its own inverse.
const fn modify_checksum_for_last_byte(checksum: u32, last_byte: u8) -> u32 {
    checksum ^ (last_byte as u32).wrapping_mul(LAST_BYTE_PRIME)
}

/// `ComputeBuiltinChecksum` [R table/format.cc:615-639]: the checksum of `data` by `kind`.
pub fn compute_builtin_checksum(kind: ChecksumType, data: &[u8]) -> u32 {
    match kind {
        ChecksumType::NoChecksum => 0,
        ChecksumType::Crc32c => crc32c::mask(crc32c::value(data)),
        ChecksumType::XxHash => xxh32(data, 0),
        ChecksumType::XxHash64 => lower32of64(xxh64(data, 0)),
        // All but the last byte through XXH3, then the last byte folded in; 0 for no bytes.
        ChecksumType::Xxh3 => match data.split_last() {
            Some((&last, head)) => {
                modify_checksum_for_last_byte(lower32of64(xxh3_64bits(head)), last)
            }
            None => 0,
        },
    }
}

/// `ComputeBuiltinChecksumWithLastByte` [R table/format.cc:641-682]: the checksum of
/// `data ‖ last_byte` (a block and its compression type) without joining them.
pub fn compute_builtin_checksum_with_last_byte(
    kind: ChecksumType,
    data: &[u8],
    last_byte: u8,
) -> u32 {
    match kind {
        ChecksumType::NoChecksum => 0,
        ChecksumType::Crc32c => crc32c::mask(crc32c::extend(crc32c::value(data), &[last_byte])),
        ChecksumType::XxHash => xxh32_with_last_byte(data, last_byte, 0),
        ChecksumType::XxHash64 => lower32of64(xxh64_with_last_byte(data, last_byte, 0)),
        ChecksumType::Xxh3 => {
            modify_checksum_for_last_byte(lower32of64(xxh3_64bits(data)), last_byte)
        }
    }
}

/// `ChecksumModifierForContext` [R table/format.h:143-170]: added (wrapping) to a stored
/// checksum from format_version 6; 0 when the file's `base_context_checksum` is 0.
pub const fn checksum_modifier_for_context(base_context_checksum: u32, offset: u64) -> u32 {
    if base_context_checksum == 0 {
        return 0;
    }
    base_context_checksum ^ lower32of64(offset).wrapping_add(upper32of64(offset))
}

/// The first format_version whose checksums carry the context modifier
/// [R table/format.h:218-220].
pub const CONTEXT_CHECKSUM_FORMAT_VERSION: u32 = 6;

/// `FormatVersionUsesContextChecksum` [R table/format.h:218-220].
pub const fn format_version_uses_context_checksum(version: u32) -> bool {
    version >= CONTEXT_CHECKSUM_FORMAT_VERSION
}

/// `kMagicNumberLengthByte` [R table/format.h:42].
pub const MAGIC_NUMBER_LENGTH: usize = 8;

/// `kBlockBasedTableMagicNumber` [R table/block_based/block_based_table_builder.cc:146].
pub const BLOCK_BASED_TABLE_MAGIC_NUMBER: u64 = 0x88e2_41b7_85f4_cff7;

/// `kPlainTableMagicNumber` [R table/plain/plain_table_builder.cc:55]: refused until P20.
pub const PLAIN_TABLE_MAGIC_NUMBER: u64 = 0x8242_2296_63bf_9564;

/// `kLegacyPlainTableMagicNumber` [R table/plain/plain_table_builder.cc:56]: refused until P20.
pub const LEGACY_PLAIN_TABLE_MAGIC_NUMBER: u64 = 0x4f34_18eb_7a8f_13b8;

/// `kCuckooTableMagicNumber` [R table/cuckoo/cuckoo_table_builder.cc:47]: refused until P20.
pub const CUCKOO_TABLE_MAGIC_NUMBER: u64 = 0x9267_89d0_c5f1_7873;

/// The magic number of block-based tables before format_version 2, which RocksDB 11 no longer
/// reads [R table/format.cc:344].
pub const LEGACY_BLOCK_BASED_TABLE_MAGIC_NUMBER: u64 = 0xdb47_7524_8b80_fb57;

/// `BlockBasedTable::kBlockTrailerSize` [R table/block_based/block_based_table_reader.h:133]: a
/// block's compression-type byte and its 32-bit checksum.
pub const BLOCK_TRAILER_SIZE: u64 = 5;

/// `kLatestBbtFormatVersion` [R table/format.h:172].
pub const LATEST_BBT_FORMAT_VERSION: u32 = 7;

/// `kMinSupportedBbtFormatVersionForRead` and `...ForWrite` [R table/format.h:179-187].
pub const MIN_SUPPORTED_BBT_FORMAT_VERSION: u32 = 2;

/// `IsSupportedFormatVersionForRead` and `...ForWrite` for block-based tables
/// [R table/format.h:191-215], which are the same range in 11.8.1.
pub const fn is_supported_bbt_format_version(version: u32) -> bool {
    version >= MIN_SUPPORTED_BBT_FORMAT_VERSION && version <= LATEST_BBT_FORMAT_VERSION
}

/// `FormatVersionUsesIndexHandleInFooter` [R table/format.h:222-224].
pub const fn format_version_uses_index_handle_in_footer(version: u32) -> bool {
    version < CONTEXT_CHECKSUM_FORMAT_VERSION
}

/// `FormatVersionUsesCompressionManagerName` [R table/format.h:226-228].
pub const fn format_version_uses_compression_manager_name(version: u32) -> bool {
    version >= 7
}

/// `BlockHandle` [R table/format.h:53-97]: the extent of a block in a file, without its trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockHandle {
    pub offset: u64,
    pub size: u64,
}

/// A handle's encoding: up to [`BlockHandle::MAX_ENCODED_LENGTH`] bytes and how many are used
/// (`EncodedBlockHandle` [R table/format.h:99-107]).
#[derive(Debug, Clone, Copy)]
pub struct EncodedBlockHandle {
    buffer: [u8; BlockHandle::MAX_ENCODED_LENGTH],
    len: usize,
}

impl EncodedBlockHandle {
    pub fn as_slice(&self) -> &[u8] {
        self.buffer.get(..self.len).unwrap_or_default()
    }
}

impl BlockHandle {
    /// `kMaxEncodedLength` [R table/format.h:83]: two varint64s.
    pub const MAX_ENCODED_LENGTH: usize = 2 * MAX_VARINT64_LENGTH;

    /// `kNullBlockHandle` [R table/format.cc:100]: offset and size both 0, pointing nowhere.
    pub const NULL: Self = Self { offset: 0, size: 0 };

    pub const fn new(offset: u64, size: u64) -> Self {
        Self { offset, size }
    }

    /// `IsNull` [R table/format.h:78].
    pub const fn is_null(&self) -> bool {
        self.offset == 0 && self.size == 0
    }

    /// `EncodeTo(std::string*)` [R table/format.cc:50-55]: the offset then the size, varint64s.
    pub fn encode_to(&self, dst: &mut Vec<u8>) {
        put_varint64_varint64(dst, self.offset, self.size);
    }

    /// `EncodeTo(char*)` [R table/format.cc:57-64], into a buffer of the handle's own.
    pub fn encoded(&self) -> EncodedBlockHandle {
        let mut buffer = [0u8; Self::MAX_ENCODED_LENGTH];
        let offset = encode_varint64(self.offset);
        let size = encode_varint64(self.size);
        let bytes = offset.as_bytes().iter().chain(size.as_bytes());
        let mut len = 0usize;
        for (slot, &b) in buffer.iter_mut().zip(bytes) {
            *slot = b;
            len = len.saturating_add(1);
        }
        EncodedBlockHandle { buffer, len }
    }

    /// `DecodeFrom` [R table/format.cc:66-75]: consumes a handle from the front of `input`.
    pub fn decode_from(input: &mut &[u8]) -> Result<Self, Error> {
        let offset = get_varint64(input).map_err(bad_handle)?;
        let size = get_varint64(input).map_err(bad_handle)?;
        Ok(Self { offset, size })
    }

    /// `DecodeSizeFrom` [R table/format.cc:77-87]: a handle at `offset` whose size is consumed
    /// from the front of `input`.
    pub fn decode_size_from(offset: u64, input: &mut &[u8]) -> Result<Self, Error> {
        let size = get_varint64(input).map_err(bad_handle)?;
        Ok(Self { offset, size })
    }

    /// Where the block after this one starts: past this block and its trailer.
    pub fn next_offset(&self, trailer: u64) -> Option<u64> {
        self.offset.checked_add(self.size)?.checked_add(trailer)
    }
}

/// RocksDB's "bad block handle" [R table/format.cc:73], keeping why the varint did not decode:
/// cut short, or one RocksDB would read by dropping bits it never writes (docs/research/24 §1.1).
fn bad_handle(e: Error) -> Error {
    let why = match e {
        Error::Corruption { why, .. } => why,
        _ => Malformed::Truncated,
    };
    Error::corruption("block handle", why)
}

/// `IndexValue` [R table/format.h:116-136]: an index entry's value, the handle of its data block
/// and, when the index stores it, the block's first internal key (empty for unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexValue<'a> {
    pub handle: BlockHandle,
    pub first_internal_key: &'a [u8],
}

impl<'a> IndexValue<'a> {
    /// `EncodeTo` [R table/format.cc:102-118]. With `previous`, only the size is stored, as its
    /// difference from the previous block's, and this block must start just past that one and
    /// its trailer, which RocksDB only asserts.
    pub fn encode_to(
        &self,
        dst: &mut Vec<u8>,
        have_first_key: bool,
        previous: Option<&BlockHandle>,
    ) -> Result<(), Error> {
        match previous {
            Some(previous) => {
                if previous.next_offset(BLOCK_TRAILER_SIZE) != Some(self.handle.offset) {
                    return Err(Error::InvalidArgument {
                        what: "a delta-encoded index value's block is not the next one",
                    });
                }
                // Two u64s differ by less than 2^64 in magnitude, which an i128 holds.
                let delta = i128::from(self.handle.size)
                    .checked_sub(i128::from(previous.size))
                    .and_then(|d| i64::try_from(d).ok())
                    .ok_or(Error::InvalidArgument {
                        what: "a delta-encoded index value's size difference",
                    })?;
                put_varsignedint64(dst, delta);
            }
            None => self.handle.encode_to(dst),
        }
        if have_first_key {
            put_length_prefixed_slice(dst, self.first_internal_key)?;
        }
        Ok(())
    }

    /// `DecodeFrom` [R table/format.cc:120-145]: consumes a value from the front of `input`.
    pub fn decode_from(
        input: &mut &'a [u8],
        have_first_key: bool,
        previous: Option<&BlockHandle>,
    ) -> Result<Self, Error> {
        let handle = match previous {
            Some(previous) => {
                let delta = get_varsignedint64(input).map_err(|_| {
                    Error::corruption("delta-encoded index value", Malformed::Truncated)
                })?;
                // RocksDB adds these unchecked; a delta past the previous size is no block.
                let offset = previous
                    .next_offset(BLOCK_TRAILER_SIZE)
                    .ok_or(Error::corruption(
                        "delta-encoded index value",
                        Malformed::OutOfRange,
                    ))?;
                let size = i128::from(previous.size)
                    .checked_add(i128::from(delta))
                    .and_then(|s| u64::try_from(s).ok())
                    .ok_or(Error::corruption(
                        "delta-encoded index value",
                        Malformed::OutOfRange,
                    ))?;
                BlockHandle { offset, size }
            }
            None => BlockHandle::decode_from(input)?,
        };
        let first_internal_key = if have_first_key {
            get_length_prefixed_slice(input)
                .map_err(|_| Error::corruption("first key in block info", Malformed::Truncated))?
        } else {
            &[]
        };
        Ok(Self {
            handle,
            first_internal_key,
        })
    }
}

/// `kExtendedMagic` [R table/format.cc:223]: the first bytes of a footer's part 2 from
/// format_version 6.
const EXTENDED_MAGIC: [u8; 4] = [0x3e, 0x00, 0x7a, 0x00];

/// `kFooterPart2Size` [R table/format.cc:224].
const FOOTER_PART2_SIZE: usize = 2 * BlockHandle::MAX_ENCODED_LENGTH;

/// `Footer::kVersion0EncodedLength`, `kMinEncodedLength` [R table/format.h:296-298].
pub const FOOTER_VERSION0_ENCODED_LENGTH: usize =
    2 * BlockHandle::MAX_ENCODED_LENGTH + MAGIC_NUMBER_LENGTH;

/// `Footer::kNewVersionsEncodedLength`, `kMaxEncodedLength` [R table/format.h:304-306]: the
/// checksum type, part 2, the format version and the magic number.
pub const FOOTER_ENCODED_LENGTH: usize = 1 + FOOTER_PART2_SIZE + 4 + MAGIC_NUMBER_LENGTH;

/// Where a format_version 6 footer's checksum lies: after the checksum type and the extended
/// magic [R table/format.cc:280-288].
const FOOTER_CHECKSUM_AT: usize = 1 + EXTENDED_MAGIC.len();

/// `Footer` [R table/format.h:236-323]: the fixed tail of a block-based table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    pub table_magic_number: u64,
    pub format_version: u32,
    /// See [`checksum_modifier_for_context`]; 0 before format_version 6.
    pub base_context_checksum: u32,
    pub metaindex_handle: BlockHandle,
    /// The top-level index's handle before format_version 6; null from 6, when the metaindex
    /// names it.
    pub index_handle: BlockHandle,
    pub checksum_type: ChecksumType,
    /// `GetBlockTrailerSize` [R table/format.h:286]: 5 for a block-based table.
    pub block_trailer_size: u64,
}

/// What a table's magic number says, for the footer's purposes.
fn known_magic(magic: u64) -> Result<(), Error> {
    match magic {
        BLOCK_BASED_TABLE_MAGIC_NUMBER => Ok(()),
        // RocksDB 11 reads neither [R table/format.cc:342-348].
        LEGACY_BLOCK_BASED_TABLE_MAGIC_NUMBER => Err(Error::Unsupported {
            feature: "block-based table before format_version 2 (load with RocksDB >= 4.6.0 and \
                      < 11.0.0 and run a full compaction to upgrade)",
            value: magic,
        }),
        PLAIN_TABLE_MAGIC_NUMBER | LEGACY_PLAIN_TABLE_MAGIC_NUMBER | CUCKOO_TABLE_MAGIC_NUMBER => {
            Err(Error::Unsupported {
                feature: "table format (plain and cuckoo tables are refused until phase P20)",
                value: magic,
            })
        }
        _ => Err(Error::corruption("table magic number", Malformed::BadMagic)),
    }
}

impl Footer {
    /// `Footer::DecodeFrom` [R table/format.cc:332-473]: a footer from the last bytes of `input`,
    /// which lie at `input_offset` in the file. With `enforce_magic`, any other table magic
    /// number is corruption.
    pub fn decode_from(
        input: &[u8],
        input_offset: u64,
        enforce_magic: Option<u64>,
    ) -> Result<Self, Error> {
        // RocksDB asserts this length.
        if input.len() < FOOTER_VERSION0_ENCODED_LENGTH {
            return Err(Error::truncated("table footer"));
        }
        let truncated = || Error::truncated("table footer");
        let magic_at = input
            .len()
            .checked_sub(MAGIC_NUMBER_LENGTH)
            .ok_or_else(truncated)?;
        let magic = decode_fixed64(input.get(magic_at..).unwrap_or_default())?;
        known_magic(magic)?;
        if enforce_magic.is_some_and(|m| m != magic) {
            return Err(Error::corruption("table magic number", Malformed::BadMagic));
        }
        let version_at = magic_at.checked_sub(4).ok_or_else(truncated)?;
        let format_version = decode_fixed32(input.get(version_at..magic_at).unwrap_or_default())?;
        if !is_supported_bbt_format_version(format_version) {
            return Err(Error::corruption(
                "footer format_version",
                Malformed::UnknownVersion(u64::from(format_version)),
            ));
        }
        if input.len() < FOOTER_ENCODED_LENGTH {
            return Err(Error::truncated("table footer"));
        }
        let adjustment = input
            .len()
            .checked_sub(FOOTER_ENCODED_LENGTH)
            .ok_or_else(truncated)?;
        let footer = input.get(adjustment..).unwrap_or_default();
        let footer_offset =
            input_offset
                .checked_add(adjustment as u64)
                .ok_or(Error::corruption(
                    "table footer offset",
                    Malformed::OutOfRange,
                ))?;
        let kind = footer.first().copied().unwrap_or_default();
        let checksum_type = ChecksumType::from_byte(kind)
            .map_err(|_| Error::corruption("footer checksum type", Malformed::UnknownTag(kind)))?;
        let mut part2 = footer.get(1..1 + FOOTER_PART2_SIZE).unwrap_or_default();
        if format_version < CONTEXT_CHECKSUM_FORMAT_VERSION {
            let metaindex_handle = BlockHandle::decode_from(&mut part2)?;
            let index_handle = BlockHandle::decode_from(&mut part2)?;
            // Padding in part 2 is ignored.
            return Ok(Self {
                table_magic_number: magic,
                format_version,
                base_context_checksum: 0,
                metaindex_handle,
                index_handle,
                checksum_type,
                block_trailer_size: BLOCK_TRAILER_SIZE,
            });
        }
        if part2.get(..EXTENDED_MAGIC.len()) != Some(EXTENDED_MAGIC.as_slice()) {
            return Err(Error::corruption(
                "footer extended magic",
                Malformed::BadMagic,
            ));
        }
        let field = |at: usize| decode_fixed32(part2.get(at..).unwrap_or_default());
        let stored = field(4)?;
        let base_context_checksum = field(8)?;
        let metaindex_size = field(12)?;
        if checksum_modifier_for_context(base_context_checksum, 0) == 0 {
            return Err(Error::corruption(
                "footer base context checksum",
                Malformed::Forbidden,
            ));
        }
        let mut computed = 0u32;
        if checksum_type != ChecksumType::NoChecksum {
            let mut copy = [0u8; FOOTER_ENCODED_LENGTH];
            for (to, &from) in copy.iter_mut().zip(footer) {
                *to = from;
            }
            for slot in copy.iter_mut().skip(FOOTER_CHECKSUM_AT).take(4) {
                *slot = 0;
            }
            computed = compute_builtin_checksum(checksum_type, &copy);
        }
        computed = computed.wrapping_add(checksum_modifier_for_context(
            base_context_checksum,
            footer_offset,
        ));
        if computed != stored {
            return Err(Error::corruption(
                "table footer",
                Malformed::ChecksumMismatch,
            ));
        }
        // The metaindex lies just before the footer and its trailer; RocksDB subtracts unchecked.
        let metaindex_offset = footer_offset
            .checked_sub(BLOCK_TRAILER_SIZE)
            .and_then(|end| end.checked_sub(u64::from(metaindex_size)))
            .ok_or(Error::corruption(
                "footer metaindex size",
                Malformed::OutOfRange,
            ))?;
        // 16 bytes of unchecked reserved padding, then 8 that must be zero.
        let reserved = decode_fixed64(part2.get(32..).unwrap_or_default())?;
        if reserved != 0 {
            return Err(Error::Unsupported {
                feature: "footer reserved field (a future feature)",
                value: reserved,
            });
        }
        Ok(Self {
            table_magic_number: magic,
            format_version,
            base_context_checksum,
            metaindex_handle: BlockHandle::new(metaindex_offset, u64::from(metaindex_size)),
            index_handle: BlockHandle::NULL,
            checksum_type,
            block_trailer_size: BLOCK_TRAILER_SIZE,
        })
    }
}

/// `FooterBuilder::Build` [R table/format.cc:227-330] for a block-based table: the footer
/// written at `footer_offset`. From format_version 6 the index's handle is not stored (the
/// metaindex names it), the metaindex must end just before the footer's offset less its
/// trailer, and `base_context_checksum` must enable the context; before 6 it must be 0.
pub fn build_footer(
    format_version: u32,
    footer_offset: u64,
    checksum_type: ChecksumType,
    metaindex_handle: BlockHandle,
    index_handle: BlockHandle,
    base_context_checksum: u32,
) -> Result<[u8; FOOTER_ENCODED_LENGTH], Error> {
    if !is_supported_bbt_format_version(format_version) {
        return Err(Error::Unsupported {
            feature: "block-based table format_version for writing",
            value: u64::from(format_version),
        });
    }
    let mut data = [0u8; FOOTER_ENCODED_LENGTH];
    // Part 1, the checksum type; part 3, the format version and magic number.
    if let Some(first) = data.first_mut() {
        *first = checksum_type.to_byte();
    }
    let part3 = 1 + FOOTER_PART2_SIZE;
    for (to, from) in data.iter_mut().skip(part3).zip(
        encode_fixed32(format_version)
            .iter()
            .chain(&encode_fixed64(BLOCK_BASED_TABLE_MAGIC_NUMBER)),
    ) {
        *to = *from;
    }
    let mut part2 = Vec::with_capacity(FOOTER_PART2_SIZE);
    if format_version_uses_context_checksum(format_version) {
        if checksum_modifier_for_context(base_context_checksum, 0) == 0 {
            return Err(Error::InvalidArgument {
                what: "a format_version 6 footer's base context checksum must enable context",
            });
        }
        let metaindex_size =
            u32::try_from(metaindex_handle.size).map_err(|_| Error::Unsupported {
                feature: "metaindex block larger than 4 GiB",
                value: metaindex_handle.size,
            })?;
        if metaindex_size != 0
            && metaindex_handle.next_offset(BLOCK_TRAILER_SIZE) != Some(footer_offset)
        {
            return Err(Error::InvalidArgument {
                what: "the metaindex block must end just before the footer",
            });
        }
        part2.extend_from_slice(&EXTENDED_MAGIC);
        part2.extend_from_slice(&[0; 4]);
        part2.extend_from_slice(&encode_fixed32(base_context_checksum));
        part2.extend_from_slice(&encode_fixed32(metaindex_size));
        // The rest, 24 bytes, is zero: reserved.
        part2.resize(FOOTER_PART2_SIZE, 0);
    } else {
        if base_context_checksum != 0 {
            return Err(Error::InvalidArgument {
                what: "a footer before format_version 6 has no base context checksum",
            });
        }
        metaindex_handle.encode_to(&mut part2);
        index_handle.encode_to(&mut part2);
        // Zero padding to part 2's size.
        part2.resize(FOOTER_PART2_SIZE, 0);
    }
    for (to, &from) in data.iter_mut().skip(1).zip(&part2) {
        *to = from;
    }
    if format_version_uses_context_checksum(format_version) {
        let checksum = compute_builtin_checksum(checksum_type, &data).wrapping_add(
            checksum_modifier_for_context(base_context_checksum, footer_offset),
        );
        for (to, &from) in data
            .iter_mut()
            .skip(FOOTER_CHECKSUM_AT)
            .zip(&encode_fixed32(checksum))
        {
            *to = from;
        }
    }
    Ok(data)
}
