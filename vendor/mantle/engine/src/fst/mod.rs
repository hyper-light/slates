//! A Fast Succinct Trie (Zhang et al., SIGMOD 2018; research/35): a static, order-preserving trie
//! of byte keys in LOUDS-Dense upper levels and LOUDS-Sparse lower ones, the index of a branch's
//! leaves and, with suffix bits, a bundle's range filter (docs/design/engine-structure.md §5,
//! E6).

pub mod bits;
pub mod packed;
pub mod surf;
pub mod trie;
