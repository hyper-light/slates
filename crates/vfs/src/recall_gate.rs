//! The recall gate (RFC 8881 §10.2; §4.6 A-79): the inodes of this shard's files an NFSv4 client holds a delegation
//! of, and the recalls a change to one of them has asked for.
//!
//! A delegation promises its holder that the file's contents, attributes and names will not change without its
//! knowledge (§10.4). RFC 8881 §10.2 holds every path to that promise, not only NFSv4's: "operations done outside the
//! NFSv4.1 protocol ... also need to result in delegation recall ... and MUST be returned or revoked before allowing
//! the operation to proceed". In slates every change to a file — through NFSv3, NFSv4, FUSE, virtio-fs, WinFsp, the
//! SDK, a merge or a landing — first makes the file's inode current (copy-on-write: a version is never written in
//! place), so the gate sits there ([`crate::volume::Volume`]'s `make_current_inode`): a change to a delegated inode is
//! refused [`crate::error::VfsError::Delegated`] before anything is touched, and the inode is queued for the daemon
//! to recall. The caller retries (an NFS client is told `NFS3ERR_JUKEBOX` / `NFS4ERR_DELAY`); once the delegation is
//! returned or revoked the inode leaves the gate and the change proceeds.
//!
//! Bounds: the gate holds one entry per delegated inode (the owner's delegation table bounds them), and the recall
//! queue holds each delegated inode at most once until drained.

use std::collections::BTreeSet;

/// The shard's delegated inodes and the recalls they owe.
#[derive(Debug, Default)]
pub struct RecallGate {
  delegated: BTreeSet<u64>,
  requested: BTreeSet<u64>,
}

impl RecallGate {
  /// Closes the gate on inode `no` (a delegation of its file was granted).
  pub fn delegate(&mut self, no: u64) {
    self.delegated.insert(no);
  }

  /// Opens the gate on inode `no` (its file's last delegation was returned or revoked).
  pub fn release(&mut self, no: u64) {
    self.delegated.remove(&no);
    self.requested.remove(&no);
  }

  /// Replaces the delegated set whole (after a restore, or a client's delegations were purged).
  pub fn reset(&mut self, delegated: BTreeSet<u64>) {
    self.requested.retain(|no| delegated.contains(no));
    self.delegated = delegated;
  }

  /// Whether a change to inode `no` may proceed now; when it may not, its recall is queued.
  pub fn admit(&mut self, no: u64) -> bool {
    if self.delegated.contains(&no) {
      self.requested.insert(no);
      return false;
    }
    true
  }

  /// The inodes whose recall a refused change asked for since the last call.
  pub fn take_requested(&mut self) -> Vec<u64> {
    std::mem::take(&mut self.requested).into_iter().collect()
  }

  /// Whether any inode is delegated (the gate's fast path: nothing to check).
  pub fn is_open(&self) -> bool {
    self.delegated.is_empty()
  }
}
