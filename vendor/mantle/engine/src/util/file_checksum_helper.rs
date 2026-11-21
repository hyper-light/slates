//! The built-in whole-file checksum: `FileChecksumGenCrc32c` of RocksDB's
//! `util/file_checksum_helper.h` [R :30-68] and the names of `include/rocksdb/file_checksum.h`
//! [R :23-33] (docs/research/24 §1.2).
//!
//! The value is the unmasked CRC-32C of the whole file stored as 4 big-endian bytes; the
//! MANIFEST records it with the generator's name, and mantle's transfers check a table against
//! it (note 12 §6.3).

use crate::util::crc32c;

/// `kStandardDbFileChecksumFuncName`, the name this generator records [R file_checksum.h:33].
pub const FILE_CHECKSUM_CRC32C_NAME: &str = "FileChecksumCrc32c";

/// `kUnknownFileChecksum` [R include/rocksdb/file_checksum.h:23].
pub const UNKNOWN_FILE_CHECKSUM: &[u8] = b"";

/// `kUnknownFileChecksumFuncName` [R include/rocksdb/file_checksum.h:27]: no generator was
/// configured when the file was written.
pub const UNKNOWN_FILE_CHECKSUM_FUNC_NAME: &str = "Unknown";

/// `FileChecksumGenCrc32c`: fed the file's bytes in order, then finalized.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileChecksumGenCrc32c {
    checksum: u32,
}

impl FileChecksumGenCrc32c {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Update` [R util/file_checksum_helper.h:38-40].
    pub fn update(&mut self, data: &[u8]) {
        self.checksum = crc32c::extend(self.checksum, data);
    }

    /// `Finalize` then `GetChecksum` [R util/file_checksum_helper.h:42-51]: the CRC as 4
    /// big-endian bytes.
    pub fn finalize(self) -> [u8; 4] {
        self.checksum.to_be_bytes()
    }

    /// `Name` [R util/file_checksum_helper.h:53].
    pub fn name(&self) -> &'static str {
        FILE_CHECKSUM_CRC32C_NAME
    }
}
