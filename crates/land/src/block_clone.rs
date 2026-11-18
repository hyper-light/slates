//! Where a ReFS block clone may share a file's bytes (A-445; Microsoft Learn, "Block Cloning" and
//! `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, read 2026-11-18): the regions a whole file of `len` bytes is cloned in. The
//! source and destination regions "must begin and end at a cluster boundary", each "must be less than 4GB in length",
//! and the destination "must not extend past the end of file", so the Windows writer sets the new file's end to the
//! length rounded up to a whole cluster first, clones whole clusters in regions of at most the largest multiple of the
//! cluster below 4 GiB, and then sets the end to `len` again. The cluster is the volume's own, read from the volume
//! (`FileFsSizeInformation`, 4 KiB or 64 KiB on ReFS), never assumed. Pure, so it is tested on every host; the writer
//! (`os_windows.rs`) is its only user.

/// Format: the length every block clone region must stay below ("less than 4GB", Microsoft Learn, Block Cloning).
const REGION_LIMIT: u64 = 1 << 32;

/// Why a file's clone regions could not be laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
  /// The volume reported no cluster, or one no region below the limit can hold.
  Cluster,
  /// The file's length rounded up to a whole cluster does not fit a 64-bit offset.
  TooLong,
}

/// The file's end while it is cloned: `len` rounded up to a whole `cluster`, so every region ends at a cluster boundary
/// within the file, as the destination's region must.
pub(crate) fn clone_end(len: u64, cluster: u64) -> Result<u64, Layout> {
  if cluster == 0 {
    return Err(Layout::Cluster);
  }
  len.checked_next_multiple_of(cluster).ok_or(Layout::TooLong)
}

/// The largest region a clone may take: the greatest multiple of `cluster` below [`REGION_LIMIT`].
fn largest_region(cluster: u64) -> Result<u64, Layout> {
  let whole = REGION_LIMIT
    .checked_sub(1)
    .and_then(|below| below.checked_div(cluster))
    .and_then(|clusters| clusters.checked_mul(cluster))
    .ok_or(Layout::Cluster)?;
  if whole == 0 { Err(Layout::Cluster) } else { Ok(whole) }
}

/// The regions (`offset`, `length`) a file of `len` bytes is cloned in, at the same offsets in the source and the
/// destination: from the start, each the largest region a clone may take until the last, which reaches the length
/// rounded up to a whole cluster ([`clone_end`]). None for an empty file.
pub(crate) fn clone_regions(len: u64, cluster: u64) -> Result<Vec<(u64, u64)>, Layout> {
  let end = clone_end(len, cluster)?;
  let largest = largest_region(cluster)?;
  let mut regions = Vec::new();
  let mut offset = 0u64;
  while offset < end {
    let length = end.saturating_sub(offset).min(largest);
    regions.push((offset, length));
    offset = offset.checked_add(length).ok_or(Layout::TooLong)?;
  }
  Ok(regions)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: ReFS's default cluster and its large one (Microsoft Learn, Block Cloning: "4KB ... optionally 64KB").
  const SMALL: u64 = 4096;
  const LARGE: u64 = 65_536;

  /// A-445. Do: lay out the regions of files of every interesting length on both of ReFS's clusters. Expect each
  /// region to begin and end on a cluster boundary, to be shorter than 4 GiB, and the regions together to cover the file
  /// from its start to its length rounded up to a whole cluster, with no gap and no overlap.
  #[test]
  fn every_region_is_whole_clusters_below_four_gib_and_together_they_cover_the_file() {
    for cluster in [SMALL, LARGE] {
      for len in [1, cluster - 1, cluster, cluster + 1, 10 * cluster + 7, REGION_LIMIT - 1, REGION_LIMIT, REGION_LIMIT + 1, 3 * REGION_LIMIT + 5] {
        let regions = clone_regions(len, cluster).unwrap();
        let mut next = 0;
        for (offset, length) in &regions {
          assert_eq!(*offset, next, "contiguous: {cluster} {len}");
          assert_eq!(offset % cluster, 0, "begins on a cluster: {cluster} {len}");
          assert_eq!(length % cluster, 0, "ends on a cluster: {cluster} {len}");
          assert!(*length > 0 && *length < REGION_LIMIT, "below 4 GiB: {cluster} {len} {length}");
          next += length;
        }
        assert_eq!(next, clone_end(len, cluster).unwrap(), "{cluster} {len}");
        assert!(next >= len && next - len < cluster, "the length rounded up to a whole cluster: {cluster} {len}");
      }
    }
  }

  /// A-445. Do: lay out an empty file, a 4 GiB file and the largest region. Expect nothing to clone for an empty file; a
  /// 4 GiB file in two regions, since no region may reach 4 GiB; and the largest region the greatest multiple of the
  /// cluster below the limit.
  #[test]
  fn a_region_never_reaches_four_gib_and_an_empty_file_has_none() {
    assert_eq!(clone_regions(0, SMALL), Ok(Vec::new()));
    assert_eq!(clone_regions(REGION_LIMIT, SMALL).unwrap(), vec![(0, REGION_LIMIT - SMALL), (REGION_LIMIT - SMALL, SMALL)]);
    assert_eq!(largest_region(LARGE), Ok(REGION_LIMIT - LARGE));
  }

  /// A-445. Do: lay out a file on a volume that reported no cluster, one larger than any region, and a length whose
  /// round-up passes 64 bits. Expect each refused, by kind, never a region of length zero or a wrap.
  #[test]
  fn a_cluster_or_a_length_the_layout_cannot_hold_is_refused_by_kind() {
    assert_eq!(clone_regions(10, 0), Err(Layout::Cluster));
    assert_eq!(clone_regions(10, REGION_LIMIT), Err(Layout::Cluster), "no region below 4 GiB holds one cluster");
    assert_eq!(clone_end(u64::MAX, SMALL), Err(Layout::TooLong));
  }
}
