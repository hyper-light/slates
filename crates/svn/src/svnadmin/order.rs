//! The orders Subversion's repository layer walks a revision in, reproduced so `svnadmin dump`'s records come out in
//! stock order: paths as `svn_path_compare_paths` sorts them (a path's children before its later siblings), the order
//! `svn_repos_replay2`'s path driver visits a revision's changed paths in; and a directory's entries in the order
//! FSFS's entries table yields them to `svn_repos_dir_delta2`, the order of a whole tree's records.
//!
//! The second is the order of an APR hash table: `fs_dir_entries` (`libsvn_fs_fs/tree.c`) inserts a directory's
//! entries, sorted by name, into a table `svn_hash__make` made (`libsvn_subr/hash.c` 654-658), whose hash function is
//! `hashfunc_compatible` (629-652), with no random seed, so the order is fixed; `apr_hash_first` then walks the buckets
//! in index order and each bucket's chain from its head. A new entry goes at its chain's end; when the table holds more
//! entries than its mask, `expand_array` doubles it and moves every entry, in walk order, to the head of its new
//! bucket. Checked against the order stock `svnadmin` 1.15.0 dumped two directories' entries in
//! (`fixtures/svnadmin/dump-r3-6.dump`).

use std::cmp::Ordering;

/// The byte between a path's components.
/// Format: Subversion's relpath separator (`svn_relpath_join`).
pub(super) const SEPARATOR: u8 = b'/';

/// The mask of a new APR hash table's bucket array: one less than its size.
/// Format: `INITIAL_MAX` 15 (`apr_hash.c`, `apr_hash_make`).
const INITIAL_MASK: usize = 15;

/// The multiplier of `hashfunc_compatible`'s hash.
/// Format: `hash = hash * 33 + c` (`libsvn_subr/hash.c` 641-649, the classic "times 33" hash).
const TIMES: u32 = 33;

/// How `svn_path_compare_paths` orders two paths (`libsvn_subr/path.c` 418-454): byte by byte, but where they first
/// differ a `/` sorts before any other byte, so a directory's children come right after it.
pub(super) fn compare_paths(first: &[u8], second: &[u8]) -> Ordering {
  let common = first
    .iter()
    .zip(second)
    .take_while(|(left, right)| left == right)
    .count();
  match (first.get(common).copied(), second.get(common).copied()) {
    (None, None) => Ordering::Equal,
    (Some(SEPARATOR), None) | (Some(_), Some(SEPARATOR)) => Ordering::Greater,
    (None, Some(SEPARATOR)) | (Some(SEPARATOR), Some(_)) => Ordering::Less,
    (left, right) => left.unwrap_or(0).cmp(&right.unwrap_or(0)),
  }
}

/// `hashfunc_compatible`'s hash of `key` (`libsvn_subr/hash.c` 629-652), in 32-bit unsigned arithmetic. It takes four
/// bytes a step as `hash * 33^4 + p0 * 33^3 + p1 * 33^2 + p2 * 33 + p3`, which is the byte-a-step `hash * 33 + p` four
/// times over, so one fold computes it.
fn hash(key: &[u8]) -> u32 {
  key
    .iter()
    .fold(0u32, |hash, &byte| hash.wrapping_mul(TIMES).wrapping_add(u32::from(byte)))
}

/// The order `apr_hash_first` walks `names` in, inserted in the order given into a table `svn_hash__make` made.
pub(super) fn hash_order<'a>(names: &[&'a [u8]]) -> Vec<&'a [u8]> {
  let mut mask = INITIAL_MASK;
  let mut buckets: Vec<Vec<(u32, &'a [u8])>> = vec![Vec::new(); mask.saturating_add(1)];
  for (count, &name) in names.iter().enumerate() {
    let hash = hash(name);
    if let Some(bucket) = buckets.get_mut(index(hash, mask)) {
      bucket.push((hash, name));
    }
    if count.saturating_add(1) > mask {
      (buckets, mask) = expand(buckets, mask);
    }
  }
  buckets.into_iter().flatten().map(|(_, name)| name).collect()
}

/// A table of twice the size and one more bucket, each entry moved, in walk order, to the head of its new bucket, as
/// `expand_array` moves them.
fn expand<'a>(buckets: Vec<Vec<(u32, &'a [u8])>>, mask: usize) -> (Vec<Vec<(u32, &'a [u8])>>, usize) {
  let wider = mask.saturating_mul(2).saturating_add(1);
  let mut fresh: Vec<Vec<(u32, &'a [u8])>> = vec![Vec::new(); wider.saturating_add(1)];
  for (hash, name) in buckets.into_iter().flatten() {
    if let Some(bucket) = fresh.get_mut(index(hash, wider)) {
      bucket.insert(0, (hash, name));
    }
  }
  (fresh, wider)
}

/// The bucket a hash falls in under `mask`.
fn index(hash: u32, mask: usize) -> usize {
  usize::try_from(hash).map_or(0, |hash| hash & mask)
}
