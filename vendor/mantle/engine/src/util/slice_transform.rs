//! The built-in prefix extractors of `util/slice.cc` [R util/slice.cc:24-146]:
//! `NewFixedPrefixTransform`, `NewCappedPrefixTransform` and `NewNoopTransform`, which a table's
//! hash-search index and prefix filters take a key's prefix with.
//!
//! RocksDB takes any `SliceTransform` object; the port supports the three built-ins, as it does
//! the two built-in comparators (`util::comparator`), so a transform is an enum. A table naming
//! another is refused when it is opened (P6).

/// A prefix extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SliceTransform {
    /// The first `n` bytes; keys shorter are outside its domain.
    Fixed(usize),
    /// The first `n` bytes, or the whole key when shorter.
    Capped(usize),
    /// The whole key.
    Noop,
}

impl SliceTransform {
    /// `GetId`: the name RocksDB records in a table's properties, `rocksdb.FixedPrefix.n`,
    /// `rocksdb.CappedPrefix.n` or `rocksdb.Noop` [R util/slice.cc:32, :81, :124].
    pub fn id(self) -> String {
        match self {
            Self::Fixed(n) => format!("rocksdb.FixedPrefix.{n}"),
            Self::Capped(n) => format!("rocksdb.CappedPrefix.{n}"),
            Self::Noop => "rocksdb.Noop".to_owned(),
        }
    }

    /// `InDomain`: whether `key` has a prefix.
    pub fn in_domain(self, key: &[u8]) -> bool {
        match self {
            Self::Fixed(n) => key.len() >= n,
            Self::Capped(_) | Self::Noop => true,
        }
    }

    /// `Transform`: `key`'s prefix, or `None` outside the domain, where RocksDB asserts and in a
    /// release build returns bytes past the key [R util/slice.cc:55-58].
    pub fn transform(self, key: &[u8]) -> Option<&[u8]> {
        match self {
            Self::Fixed(n) => key.get(..n),
            Self::Capped(n) => key.get(..n.min(key.len())),
            Self::Noop => Some(key),
        }
    }
}
