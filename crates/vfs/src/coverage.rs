//! Whether a snapshot is a **complete immutable base** (§4.4 `SnapshotCoverage::Complete` against
//! `DeltaWithLiveBase`; §4.16 and the A-9 integration requirement: a green "starts from scratch or
//! a complete immutable base, never an implicitly live host directory"). A scratch volume's snapshot
//! always is: its whole logical tree is in memory. An overlay's is only when nothing in the frozen
//! tree still depends on the host directory: every merged directory's base listing was read and
//! every name it holds is in the frozen node, every base-backed file was witnessed and pinned whole,
//! and none was lost to drift — the state `pin` (the whole base) followed by `snapshot` leaves. The
//! walk is over the frozen nodes and inode records only, so it costs the snapshot's size, reads
//! memory alone, and never touches the host.
//!
//! The rule is stated on the frozen node, not on the base plane's live tables alone: a listing read
//! *after* the snapshot is in the plane but not in the frozen node, and a witness taken after the
//! freeze names a newer inode version — so a check that consulted the tables and not the frozen tree
//! would call a live-dependent snapshot complete. Checking that every listed name is present in the
//! frozen node, and reading the pinned extents off the frozen inode record, pins the answer to the
//! snapshot itself.

use std::collections::BTreeSet;

use crate::content::Extent;
use crate::dir::{BaseDirState, Child};
use crate::error::VfsError;
use crate::ids::SnapshotId;
use crate::inode::Body;
use crate::volume::{Store, Volume};

impl Volume {
  /// Whether the snapshot `id` covers its whole logical tree with nothing served live from the
  /// host: `true` for a scratch volume's snapshot; for an overlay's, `true` only when every merged
  /// directory in the frozen tree holds every name its base listing found and every base-backed file
  /// is witnessed, pinned whole and not lost. A stale snapshot id is a typed refusal.
  pub fn snapshot_is_complete(&self, store: &Store, id: SnapshotId) -> Result<bool, VfsError> {
    let (_, root) = self.snapshot_info(id)?;
    let Some(plane) = self.base_plane() else {
      return Ok(true);
    };
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
      let node = store.dirs.get(dir).map_err(|_| VfsError::StaleHandle)?;
      let mut names: BTreeSet<&str> = BTreeSet::new();
      for entry in node.iter(&store.blocks) {
        names.insert(entry.name);
        match entry.child {
          Child::Dir(child) => stack.push(child),
          Child::File(no) => {
            if !self.file_is_complete_in(store, id, no)? {
              return Ok(false);
            }
          }
          Child::Symlink(_) | Child::Fifo(_) | Child::Socket(_) | Child::Whiteout => {}
        }
      }
      if node.base == BaseDirState::Merged {
        // Every name the base listing found must be in the frozen node — else the listing was read
        // after the freeze (or never), and the frozen directory would show the live disk.
        let Some(listed) = plane.listed_names(node.inode) else {
          return Ok(false);
        };
        if listed.iter().any(|name| !names.contains(name.as_ref())) {
          return Ok(false);
        }
      }
    }
    Ok(true)
  }

  /// Whether the file `no` as snapshot `id` holds it depends on nothing live: a non-base body, or a
  /// base-backed one that is witnessed, pinned from 0 to its base length, and not lost.
  fn file_is_complete_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: crate::ids::InodeNo,
  ) -> Result<bool, VfsError> {
    Ok(match &self.inode_in(store, id, no)?.body {
      Body::Base(base) => {
        !base.lost && base.witness.is_some() && pinned_whole(&base.pinned, base.base_len)
      }
      _ => true,
    })
  }
}

/// Whether `pinned` covers every byte of `[0, base_len)`: sorted by offset, the extents must run
/// contiguously (overlaps allowed) from 0 past the base length. An empty base is covered by nothing.
fn pinned_whole(pinned: &[Extent], base_len: u64) -> bool {
  let mut extents: Vec<(u64, u64)> = pinned.iter().map(|e| (e.off, e.len)).collect();
  extents.sort_unstable();
  let mut covered_to = 0u64;
  for (off, len) in extents {
    if off > covered_to {
      return false;
    }
    covered_to = covered_to.max(off.saturating_add(len));
  }
  covered_to >= base_len
}

#[cfg(test)]
mod tests {
  use super::pinned_whole;
  use crate::content::{Extent, ExtentSrc};

  fn extent(off: u64, len: u64) -> Extent {
    Extent {
      off,
      len,
      src: ExtentSrc::Zero,
    }
  }

  /// The coverage rule on its own: contiguous extents cover, a gap does not, an empty base needs
  /// nothing, and order does not matter.
  #[test]
  fn pinned_extents_cover_only_when_contiguous_from_zero_past_the_length() {
    assert!(pinned_whole(&[], 0));
    assert!(!pinned_whole(&[], 1));
    assert!(pinned_whole(&[extent(0, 4), extent(4, 4)], 8));
    assert!(pinned_whole(&[extent(4, 4), extent(0, 4)], 8), "order-free");
    assert!(pinned_whole(&[extent(0, 6), extent(3, 5)], 8), "overlap");
    assert!(!pinned_whole(&[extent(0, 4), extent(5, 3)], 8), "a gap");
    assert!(!pinned_whole(&[extent(0, 4)], 8), "short");
    assert!(!pinned_whole(&[extent(1, 8)], 8), "not from zero");
  }
}
