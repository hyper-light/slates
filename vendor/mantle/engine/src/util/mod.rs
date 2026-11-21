//! RocksDB's `util/` directory: coding, checksums, hashes and bit arithmetic.

pub mod block_compression;
pub mod coding;
pub mod comparator;
pub mod compression;
pub mod crc32c;
pub mod fastrange;
pub mod file_checksum_helper;
pub mod hash;
pub mod incmap;
pub mod keys;
pub mod math;
pub mod math128;
pub mod prefix_varint;
pub mod slice_transform;
pub mod string_util;
pub mod xxhash;
pub mod xxph3;

/// `v`'s allocation, emptied, for elements of another type of the same size and alignment: a
/// buffer of borrowed elements kept between uses whose borrows differ (a scan's sources and
/// merge heads, which borrow the trunk). Collecting a vector's own `into_iter` in place reuses
/// its allocation when the layouts match, which they do between lifetimes of one type; the
/// engine's allocation tests count it.
pub fn reuse<T, U>(mut v: Vec<T>) -> Vec<U> {
    v.clear();
    v.into_iter().filter_map(|_| None).collect()
}

/// `a / b` rounded up; none when `b` is zero.
pub(crate) fn div_ceil(a: u64, b: u64) -> Option<u64> {
    let q = a.checked_div(b)?;
    let r = a.checked_rem(b)?;
    Some(if r == 0 { q } else { q.saturating_add(1) })
}

/// [`div_ceil`] over `usize`.
pub(crate) fn div_ceil_usize(a: usize, b: usize) -> Option<usize> {
    let q = a.checked_div(b)?;
    let r = a.checked_rem(b)?;
    Some(if r == 0 { q } else { q.saturating_add(1) })
}

#[cfg(test)]
mod reuse_tests {
    use super::reuse;

    #[test]
    fn a_reused_vector_keeps_its_allocation_across_lifetimes() {
        let a = 1u64;
        let mut v: Vec<(&u64, usize)> = Vec::with_capacity(37);
        v.push((&a, 1));
        let ptr = v.as_ptr() as usize;
        let w: Vec<(&u64, usize)> = reuse(v);
        assert_eq!((w.capacity(), w.as_ptr() as usize), (37, ptr));
    }
}
