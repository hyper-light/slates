//! The replay of a consensus group's committed log into its application state, and the log's compaction
//! (§4.8 mechanism 2; Raft §7). Shared by the regional council ([`crate::config_group`]) and the root group
//! ([`crate::root_group`]), whose states differ only in type and command.
//!
//! **Replay.** A group's state is the deterministic fold of its committed entries over a *base*: the state
//! at the Raft snapshot boundary, which before the first compaction is the formed configuration. The fold
//! walks the committed entries above the snapshot in place. Until 2026-09-28 each group copied its whole
//! committed log on every append and every reply to do this, so a message cost time in proportion to the
//! fleet's age.
//!
//! **Compaction** follows the rule of Ongaro's thesis, §5.1.2 "When to snapshot": "Servers take a snapshot
//! once the size of the log exceeds the size of the previous snapshot times a configurable expansion
//! factor." Once the applied entries' wire bytes exceed the last snapshot's size (the base's encoding
//! before the first), they are folded into a snapshot whose state is the encoded configuration, and the
//! base moves to it.
//!
//! The expansion factor is one. The thesis trades the factor against disk bandwidth; slates is RAM-only, so
//! it trades only against the group's retained publication, which the control shard re-encodes and
//! checksums before every consensus reply (`slates_server::retention`), and a snapshot is taken at most once
//! per snapshot-sized stretch of log.
//!
//! A leader waits for its followers: past the threshold it compacts only once every follower holds what it
//! has applied, while the log stays within twice the threshold. A leader that compacted the moment a
//! majority committed left the follower one round behind to be sent the whole snapshot in place of the one
//! entry it lacked, at every compaction (measured 2026-09-28: the third voter of a three-voter council never
//! compacted, having been sent a snapshot each time). Past twice the threshold the leader compacts anyway, so
//! a follower that far behind, or gone, takes the snapshot and cannot pin the log. So the retained
//! publication holds at most the snapshot three times over plus the uncommitted tail.
//!
//! A follower whose next entries were compacted away receives the snapshot ([`Fold::install`]). Its state
//! is decoded before the Raft core sees it, so a state that does not decode is declined with nothing
//! adopted. Before 2026-09-28 the groups never compacted, so every entry of the fleet's life stayed in the
//! log and in every publication (`docs/wip/GAPS.md`, 2026-09-28).

use crate::raft::{InstallSnapshot, InstallSnapshotReply, RaftNode, RaftRecoveryError, SavedRaft};

/// Derived: the snapshots' worth of applied entries a leader holds while a follower still lacks them — the
/// rule's one, plus one for the follower still taking them (the module doc gives the measured reason). Past
/// it, that follower takes the snapshot, so the retained publication stays within three snapshots and the
/// uncommitted tail.
const HELD_FOR_FOLLOWERS: usize = 2;

/// A group state as a snapshot carries it: the bytes a learner's fetch already ships
/// ([`crate::raft_wire::encode_regional_configuration`] and its root counterpart), and back.
pub(crate) trait Snapshotted: Clone + PartialEq + Sized {
  /// The state's canonical bytes.
  fn encode(&self) -> Vec<u8>;
  /// The state these bytes carry, or `None` when they do not decode.
  fn decode(bytes: &[u8]) -> Option<Self>;
}

/// A group's application state as the fold of its committed log over the state at the snapshot boundary,
/// with the compaction rule's threshold and its count.
pub(crate) struct Fold<S> {
  /// The state at the Raft snapshot boundary, where the fold starts.
  base: S,
  /// The state after every applied entry.
  state: S,
  /// The committed entries above the snapshot already applied.
  applied: u64,
  /// The wire bytes the applied entries must exceed before they are compacted: the last snapshot's size,
  /// or the base's encoding before the first.
  threshold: usize,
  /// The compactions this fold has taken (the non-vacuity counter of the compaction path).
  compactions: u64,
}

impl<S: Snapshotted> Fold<S> {
  /// A fold starting at `base`, the state at the log's snapshot boundary.
  pub(crate) fn new(base: S) -> Fold<S> {
    Fold {
      threshold: base.encode().len(),
      state: base.clone(),
      base,
      applied: 0,
      compactions: 0,
    }
  }

  /// The fold for a retained or donated Raft state: `base` must be the state `saved`'s snapshot carries
  /// when its log was compacted — a snapshot paired with another base would fold a different history — and
  /// is the formed base before the first compaction.
  pub(crate) fn for_saved(saved: &SavedRaft, base: S) -> Result<Fold<S>, RaftRecoveryError> {
    if saved.snapshot_index > 0 && S::decode(&saved.snapshot_data).as_ref() != Some(&base) {
      return Err(RaftRecoveryError::InvalidSnapshot);
    }
    Ok(Fold::new(base))
  }

  /// The state after every applied entry.
  pub(crate) fn state(&self) -> &S {
    &self.state
  }

  /// The state at the snapshot boundary, where the fold starts.
  pub(crate) fn base(&self) -> &S {
    &self.base
  }

  /// The committed entries above the snapshot applied so far.
  pub(crate) fn applied(&self) -> u64 {
    self.applied
  }

  /// The compactions this fold has taken.
  pub(crate) fn compactions(&self) -> u64 {
    self.compactions
  }

  /// Applies each committed entry above the snapshot not yet applied, in order, with `apply` given the
  /// entry's command bytes (a configuration entry's are empty, and a command that does not decode is the
  /// caller's no-op), then compacts when the rule says so.
  pub(crate) fn advance(&mut self, raft: &mut RaftNode, mut apply: impl FnMut(&mut S, &[u8])) {
    let entries = raft.committed_entries();
    if self.applied == 0 && !entries.is_empty() {
      self.state = self.base.clone();
    }
    let start = usize::try_from(self.applied).unwrap_or(usize::MAX);
    for entry in entries.get(start..).unwrap_or(&[]) {
      apply(&mut self.state, &entry.command);
      self.applied = self.applied.saturating_add(1);
    }
    self.compact_if_due(raft);
  }

  /// Folds the applied entries into a snapshot once their wire bytes exceed the threshold (the module's
  /// rule) — waiting, while they stay within twice it, for every follower to hold them — moving the base to
  /// the state they produced.
  fn compact_if_due(&mut self, raft: &mut RaftNode) {
    if self.applied == 0 {
      return;
    }
    let through = raft.snapshot_index().saturating_add(self.applied);
    let bytes = raft.log_bytes_through(through);
    if bytes <= self.threshold {
      return;
    }
    if raft.replicated_through() < through
      && bytes <= self.threshold.saturating_mul(HELD_FOR_FOLLOWERS)
    {
      return;
    }
    let snapshot = self.state.encode();
    let size = snapshot.len();
    if raft.compact(through, snapshot) {
      self.base = self.state.clone();
      self.applied = 0;
      self.threshold = size;
      self.compactions = self.compactions.saturating_add(1);
    }
  }

  /// Handles a leader's snapshot (Raft §7): its state is decoded first, and a state that does not decode is
  /// declined — nothing adopted, a zero match returned, so the leader credits this follower with nothing.
  /// Otherwise the Raft core installs it; when that moved the core's snapshot boundary, the fold restarts
  /// from the snapshot's state and applies whatever committed entries the core kept above it.
  pub(crate) fn install(
    &mut self,
    raft: &mut RaftNode,
    request: InstallSnapshot,
    apply: impl FnMut(&mut S, &[u8]),
  ) -> InstallSnapshotReply {
    let Some(state) = S::decode(&request.state) else {
      return raft.decline_snapshot(&request);
    };
    let before = raft.snapshot_index();
    let size = request.state.len();
    let reply = raft.on_install_snapshot(request);
    if raft.snapshot_index() > before {
      self.base = state.clone();
      self.state = state;
      self.applied = 0;
      self.threshold = size;
    }
    self.advance(raft, apply);
    reply
  }
}
