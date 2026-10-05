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
//! The gate knows each delegated inode's holders (NFSv4 client ids) and, while a call is served, the client acting
//! for it ([`RecallGate::act_as`]): a change by the only holders of a file's delegations is that holder's own and
//! passes (a write delegation's holder writes its own file; Linux nfsd likewise lets a lease's own breaker through,
//! `nfsd_breaker_owns_lease`); any other change is refused, and the recall it asks for names its actor, so the holder
//! acting is not recalled. Every path outside an NFSv4 session acts as no client and is refused.
//!
//! A caller that would rather wait than retry (the daemon holds an NFSv3 client's refused call, whose own retry
//! backs off for seconds) notes [`RecallGate::refusals`] before its attempt, so it knows the refusal was the gate's,
//! and parks on [`RecallGate::wait_for_release`] until the delegated set changes.
//!
//! Bounds: the gate holds one entry per delegated inode with its holders (the owner's delegation table bounds both),
//! the recall queue holds each (inode, actor) pair at most once until drained (the actors are the clients the
//! table bounds, and none), and the parked wakers hold one per waiting task
//! (a task's repeated park replaces its own), so they are bounded by the tasks the shard runs.

use std::collections::{BTreeMap, BTreeSet};
use std::task::Waker;

/// The shard's delegated inodes and the recalls they owe.
#[derive(Debug, Default)]
pub struct RecallGate {
  /// Each delegated inode's holders.
  delegated: BTreeMap<u64, BTreeSet<u64>>,
  /// Each refused change's inode and actor, until drained.
  requested: BTreeSet<(u64, Option<u64>)>,
  /// The NFSv4 client the call being served acts for, if any.
  acting: Option<u64>,
  /// Changes refused at the gate, ever (the non-vacuity counter, and how a caller tells the gate's refusal apart).
  refused: u64,
  /// Bumped whenever an inode may have left the gate (a release or a reset).
  generation: u64,
  /// The tasks parked until the next release or reset.
  waiting: Vec<Waker>,
}

impl RecallGate {
  /// Closes the gate on inode `no` for every client but `holder` (a delegation of its file was granted to it).
  pub fn delegate(&mut self, no: u64, holder: u64) {
    self.delegated.entry(no).or_default().insert(holder);
  }

  /// Opens the gate on inode `no` (its file's last delegation was returned or revoked).
  pub fn release(&mut self, no: u64) {
    self.delegated.remove(&no);
    self.requested.retain(|(requested, _)| *requested != no);
    self.changed();
  }

  /// Replaces the delegated inodes and their holders whole (after any delegation changed, a restore, or a client's
  /// delegations were purged).
  pub fn reset(&mut self, delegated: BTreeMap<u64, BTreeSet<u64>>) {
    self.requested.retain(|(no, _)| delegated.contains_key(no));
    self.delegated = delegated;
    self.changed();
  }

  /// Sets inode `no`'s holders after one of its delegations changed: none opens the gate on it. Parked tasks are
  /// woken whenever a holder left (a change may now be admitted).
  pub fn set_holders(&mut self, no: u64, holders: BTreeSet<u64>) {
    if holders.is_empty() {
      self.release(no);
      return;
    }
    let shrank = self
      .delegated
      .get(&no)
      .is_some_and(|before| before.iter().any(|holder| !holders.contains(holder)));
    self.delegated.insert(no, holders);
    if shrank {
      self.changed();
    }
  }

  /// Names the NFSv4 client the call about to be served acts for (`None` for every other path, and after the call).
  pub fn act_as(&mut self, client: Option<u64>) {
    self.acting = client;
  }

  /// Marks a possible release and wakes every parked task.
  fn changed(&mut self) {
    self.generation = self.generation.wrapping_add(1);
    for waker in self.waiting.drain(..) {
      waker.wake();
    }
  }

  /// Whether a change to inode `no` may proceed now; when it may not, its recall is queued.
  pub fn admit(&mut self, no: u64) -> bool {
    let Some(holders) = self.delegated.get(&no) else {
      return true;
    };
    let own = self
      .acting
      .is_some_and(|acting| holders.iter().all(|holder| *holder == acting));
    if own {
      return true;
    }
    self.requested.insert((no, self.acting));
    self.refused = self.refused.saturating_add(1);
    false
  }

  /// The inodes, each with the actor of the change refused on it, whose recall was asked for since the last call.
  pub fn take_requested(&mut self) -> Vec<(u64, Option<u64>)> {
    std::mem::take(&mut self.requested).into_iter().collect()
  }

  /// How many changes the gate has refused, ever.
  pub fn refusals(&self) -> u64 {
    self.refused
  }

  /// The gate's generation: it moves whenever an inode may have left the gate.
  pub fn generation(&self) -> u64 {
    self.generation
  }

  /// Whether the gate has moved past generation `seen`; when it has not, `waker` is woken at its next release or
  /// reset.
  pub fn wait_for_release(&mut self, seen: u64, waker: &Waker) -> bool {
    if self.generation != seen {
      return true;
    }
    match self
      .waiting
      .iter_mut()
      .find(|parked| parked.will_wake(waker))
    {
      Some(parked) => parked.clone_from(waker),
      None => self.waiting.push(waker.clone()),
    }
    false
  }

  /// Whether any inode is delegated (the gate's fast path: nothing to check).
  pub fn is_open(&self) -> bool {
    self.delegated.is_empty()
  }
}
