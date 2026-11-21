//! `BlockPrefixIndex` of `table/block_based/block_prefix_index.{h,cc}`
//! [R block_prefix_index.cc:1-226]: the hash from a key's prefix to the index entries whose
//! blocks hold keys of that prefix, built when a table with a hash-search index is opened from
//! the two blocks [`super::index_builder::HashIndexBuilder`] writes.
//!
//! Each of `prefixes + 1` buckets holds nothing, one index entry, or a run in a shared array of
//! the entries of every prefix hashed there. RocksDB trusts the stored runs: a run of no blocks
//! underflows its end, and an entry past the index is read as a restart point. Here such a run,
//! an entry that would collide with the array marker, or a total past 32 bits is corruption.

use crate::db::dbformat::extract_user_key;
use crate::error::{Error, Malformed};
use crate::util::coding::get_varint32;
use crate::util::hash::hash;
use crate::util::slice_transform::SliceTransform;

/// `kNoneBlock`: a bucket no prefix hashed to.
const NONE_BLOCK: u32 = 0x7FFF_FFFF;
/// `kBlockArrayMask`: the bit marking a bucket that indexes the block array.
const BLOCK_ARRAY_MASK: u32 = 0x8000_0000;

/// `Hash(prefix) % num_buckets`, with RocksDB's seed of 0 [R block_prefix_index.cc:18-24].
fn bucket_of(prefix: &[u8], buckets: u32) -> Option<usize> {
    usize::try_from(hash(prefix, 0).checked_rem(buckets)?).ok()
}

/// A prefix's run of index entries, as `PrefixRecord`.
#[derive(Debug, Clone, Copy)]
struct Run {
    start: u32,
    end: u32,
    blocks: u32,
    /// The run hashed to the same bucket before this one.
    next: Option<usize>,
}

fn corrupt(what: &'static str) -> Error {
    Error::corruption(what, Malformed::OutOfRange)
}

/// The hash from prefixes to the index entries that may hold them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPrefixIndex {
    extractor: SliceTransform,
    buckets: Vec<u32>,
    block_array: Vec<u32>,
}

impl BlockPrefixIndex {
    /// `Create` [R block_prefix_index.cc:178-212] and `Builder::Finish` [:88-176]: the index read
    /// from a hash-search index's prefix block and metadata block.
    pub fn create(extractor: SliceTransform, prefixes: &[u8], meta: &[u8]) -> Result<Self, Error> {
        let mut records: Vec<(&[u8], Run)> = Vec::new();
        let mut at = 0usize;
        let mut input = meta;
        while !input.is_empty() {
            let mut field = || {
                get_varint32(&mut input)
                    .map_err(|_| corrupt("prefix meta block: unable to read from it"))
            };
            let (size, start, blocks) = (field()?, field()?, field()?);
            let end_at = usize::try_from(size)
                .ok()
                .and_then(|s| at.checked_add(s))
                .filter(|&e| e <= prefixes.len())
                .ok_or_else(|| corrupt("prefix meta block: size inconsistency"))?;
            let end = blocks
                .checked_sub(1)
                .and_then(|b| start.checked_add(b))
                .filter(|&e| e < NONE_BLOCK)
                .ok_or_else(|| corrupt("prefix meta block: a run of blocks out of range"))?;
            let prefix = prefixes.get(at..end_at).unwrap_or_default();
            records.push((
                prefix,
                Run {
                    start,
                    end,
                    blocks,
                    next: None,
                },
            ));
            at = end_at;
        }
        if at != prefixes.len() {
            return Err(corrupt("prefix meta block"));
        }
        let num_buckets = u32::try_from(records.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| corrupt("prefix meta block: too many prefixes"))?;
        let bucket_count = usize::try_from(num_buckets).map_err(|_| corrupt("prefix buckets"))?;
        let mut heads: Vec<Option<usize>> = vec![None; bucket_count];
        let mut counts = vec![0u32; bucket_count];
        let too_many = || corrupt("prefix meta block: too many blocks");
        for i in 0..records.len() {
            let (prefix, current) = records.get(i).copied().ok_or_else(too_many)?;
            let bucket = bucket_of(prefix, num_buckets).ok_or_else(too_many)?;
            let head = heads.get(bucket).copied().flatten();
            let count = counts.get_mut(bucket).ok_or_else(too_many)?;
            if let Some(prev_at) = head {
                let prev = records.get_mut(prev_at).ok_or_else(too_many)?;
                // RocksDB asserts the runs ascend and subtracts unsigned; a run before the
                // previous one is a distance past one, as its wrapped difference is.
                let distance = current.start.wrapping_sub(prev.1.end);
                if distance <= 1 {
                    prev.1.end = current.end;
                    prev.1.blocks = current
                        .end
                        .checked_sub(prev.1.start)
                        .and_then(|d| d.checked_add(1))
                        .ok_or_else(too_many)?;
                    *count = count
                        .checked_add(current.blocks)
                        .and_then(|c| c.checked_add(distance))
                        .and_then(|c| c.checked_sub(1))
                        .ok_or_else(too_many)?;
                    continue;
                }
            }
            if let Some(record) = records.get_mut(i) {
                record.1.next = head;
            }
            if let Some(h) = heads.get_mut(bucket) {
                *h = Some(i);
            }
            *count = count.checked_add(current.blocks).ok_or_else(too_many)?;
        }
        let total = counts
            .iter()
            .filter(|&&n| n > 1)
            .try_fold(0u32, |sum, &n| sum.checked_add(n)?.checked_add(1))
            .filter(|&t| t < BLOCK_ARRAY_MASK)
            .ok_or_else(too_many)?;
        let mut block_array = vec![0u32; usize::try_from(total).map_err(|_| too_many())?];
        let mut buckets = Vec::with_capacity(bucket_count);
        let mut offset = 0u32;
        for (bucket, &blocks) in counts.iter().enumerate() {
            let head = heads.get(bucket).copied().flatten();
            match (blocks, head) {
                (0, _) => buckets.push(NONE_BLOCK),
                (1, Some(h)) => {
                    let run = records.get(h).ok_or_else(too_many)?.1;
                    buckets.push(run.start);
                }
                (_, Some(h)) => {
                    buckets.push(offset | BLOCK_ARRAY_MASK);
                    let base = usize::try_from(offset).map_err(|_| too_many())?;
                    *block_array.get_mut(base).ok_or_else(too_many)? = blocks;
                    // The runs, newest first, written backwards from the bucket's end.
                    let mut last =
                        base.checked_add(usize::try_from(blocks).map_err(|_| too_many())?);
                    let mut current = Some(h);
                    while let Some(c) = current {
                        let run = records.get(c).ok_or_else(too_many)?.1;
                        for k in 0..run.blocks {
                            let slot = last.ok_or_else(too_many)?;
                            *block_array.get_mut(slot).ok_or_else(too_many)? =
                                run.end.wrapping_sub(k);
                            last = slot.checked_sub(1);
                        }
                        current = run.next;
                    }
                    if last != Some(base) {
                        return Err(too_many());
                    }
                    offset = offset
                        .checked_add(blocks)
                        .and_then(|o| o.checked_add(1))
                        .ok_or_else(too_many)?;
                }
                (_, None) => return Err(too_many()),
            }
        }
        Ok(Self {
            extractor,
            buckets,
            block_array,
        })
    }

    /// `GetBlocks` [R block_prefix_index.cc:214-235]: the index entries whose blocks may hold
    /// keys of the internal key `key`'s prefix, ascending; `None` when the key is outside the
    /// extractor's domain, where the prefix index cannot answer and a total-order seek must.
    pub fn get_blocks(&self, key: &[u8]) -> Result<Option<&[u32]>, Error> {
        let Some(prefix) = self.extractor.transform(extract_user_key(key)?) else {
            return Ok(None);
        };
        let buckets = u32::try_from(self.buckets.len()).map_err(|_| corrupt("prefix buckets"))?;
        let bucket = bucket_of(prefix, buckets).ok_or_else(|| corrupt("prefix buckets"))?;
        let id = self.buckets.get(bucket).copied().unwrap_or(NONE_BLOCK);
        if id == NONE_BLOCK {
            return Ok(Some(&[]));
        }
        if id & BLOCK_ARRAY_MASK == 0 {
            return Ok(Some(self.buckets.get(bucket..=bucket).unwrap_or_default()));
        }
        let index = usize::try_from(id ^ BLOCK_ARRAY_MASK).map_err(|_| corrupt("prefix bucket"))?;
        let count = self
            .block_array
            .get(index)
            .and_then(|&n| usize::try_from(n).ok())
            .ok_or_else(|| corrupt("prefix bucket"))?;
        let first = index
            .checked_add(1)
            .ok_or_else(|| corrupt("prefix bucket"))?;
        let end = first
            .checked_add(count)
            .ok_or_else(|| corrupt("prefix bucket"))?;
        Ok(Some(
            self.block_array
                .get(first..end)
                .ok_or_else(|| corrupt("prefix bucket"))?,
        ))
    }
}
