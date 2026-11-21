//! The release this engine converts: RocksDB's `include/rocksdb/version.h`. The version is
//! written into every OPTIONS file (`rocksdb_version=11.8.1`, docs/research/24 §1.15).

/// `ROCKSDB_MAJOR` [R include/rocksdb/version.h:14].
pub const ROCKSDB_MAJOR: u32 = 11;
/// `ROCKSDB_MINOR` [R include/rocksdb/version.h:15].
pub const ROCKSDB_MINOR: u32 = 8;
/// `ROCKSDB_PATCH` [R include/rocksdb/version.h:16].
pub const ROCKSDB_PATCH: u32 = 1;

/// Decimal places each of the minor and patch numbers takes in the integer form.
const PLACE: i64 = 1000;

/// `ROCKSDB_MAKE_VERSION_INT(x, y, z)` [R include/rocksdb/version.h:23]: `x·10⁶ + y·10³ + z`.
/// Signed, as the preprocessor computes it, so a component below zero is allowed. A result past
/// `i64` saturates: such a version is after or before every release, which is its meaning.
pub const fn make_version_int(major: i64, minor: i64, patch: i64) -> i64 {
    major
        .saturating_mul(PLACE)
        .saturating_mul(PLACE)
        .saturating_add(minor.saturating_mul(PLACE))
        .saturating_add(patch)
}

/// `ROCKSDB_VERSION_INT` [R include/rocksdb/version.h:24-25].
pub const ROCKSDB_VERSION_INT: i64 = make_version_int(
    ROCKSDB_MAJOR as i64,
    ROCKSDB_MINOR as i64,
    ROCKSDB_PATCH as i64,
);

/// `ROCKSDB_VERSION_GE(x, y, z)` [R include/rocksdb/version.h:26-27]: whether this release's
/// integer form is at or above `x.y.z`'s.
pub const fn version_ge(major: i64, minor: i64, patch: i64) -> bool {
    ROCKSDB_VERSION_INT >= make_version_int(major, minor, patch)
}
