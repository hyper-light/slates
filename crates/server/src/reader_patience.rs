//! A transport mount's journal reader's patience, learned from its own rounds (§4.5 the journal's readers; R3's class
//! C, A-548; A-157, A-431, A-442).
//!
//! A FUSE or WinFsp mount reads its volume's journal in rounds its own task gathers when the volume changes, and a
//! round goes out to the mount's kernel and back before the next is gathered, so how long a record waits for its reader
//! is that kernel's pace, which a live mount on a loaded host stretches. Until A-548 every such reader was presumed
//! gone once a record had waited one liveness window, a fixed second: a live mount slower than that overran at every
//! round that met a full budget, its kernel told everything may be stale each time (A-141, a whole cold cache), and no
//! slower mount was ever waited for.
//!
//! Now each reader's patience is learned from its own rounds by the rule every learned wait shares
//! ([`LearnedWait`], RFC 6298's estimator; A-431, A-442). A round that found every record it owed still held is a wait
//! that ended, a sample. A round that found records gone is a reader presumed gone that came back: no sample, since its
//! wait's length is unknown (Karn's rule), but a back-off that doubles the patience (RFC 6298 §5.5), never past the
//! failover delay once backed off. The patience is the samples' tail plus a liveness window, so a reader is presumed
//! gone only once it has fallen a window behind its own measured pace. Before its first round a reader has the
//! learner's prior, one window, as RFC 6298 §2.1 starts its timer at a stated second until a measurement exists.
//!
//! The bound stays because a mount's kernel can stall while the mount stays open, and the journal's growth comes from
//! the shard's metadata ledger, which every volume on the shard draws from. The NFS write-back tracker's reader has no
//! bound at all (`Patience::WhileOpen`, `crate::nfs_writeback`): it is consumed at the change itself on the volume's
//! own shard, and no other party can keep a record from it.
//!
//! The rule ([`fold`], [`patience`]) is pure, so tested on every host. The mounts are Linux's (`crate::fuse`) and
//! Windows' (`crate::winfsp`), each opening its reader through [`open`] and folding its rounds after every turn through
//! [`learn`], which only those two builds call.

use slates_db::catalog::VolumeId;
use slates_vfs::journal::{Patience, ReaderRounds};

use crate::learned_wait::LearnedWait;
use crate::state::ShardState;

/// Opens journal reader `key` of `volume`, for a mount that now serves it, at a fresh learned wait's patience, its
/// prior: the mount has read nothing yet, and its own learned wait starts fresh with it. A volume not on this shard has
/// no reader to open.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub(crate) fn open(s: &mut ShardState, volume: VolumeId, key: u64) {
  let patience = patience(&LearnedWait::default(), s.failover_ns);
  if let Some(&handle) = s.by_id.get(&volume)
    && let Ok(slot) = s.volumes.get_mut(handle)
  {
    slot.volume.open_journal_reader(key, patience);
  }
}

/// After a turn of the mount whose journal reader `key` reads `volume`: folds what the reader's rounds found since the
/// last turn into `learned`, and names the reader's patience again under the failover delay as it stands now. A reader
/// not open, or a volume gone, is left alone.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub(crate) fn learn(s: &mut ShardState, volume: VolumeId, key: u64, learned: &mut LearnedWait) {
  let ceiling_ns = s.failover_ns;
  let Some(&handle) = s.by_id.get(&volume) else {
    return;
  };
  let Ok(slot) = s.volumes.get_mut(handle) else {
    return;
  };
  let Some(rounds) = slot.volume.take_journal_rounds(key) else {
    return;
  };
  fold(learned, rounds);
  slot
    .volume
    .set_journal_patience(key, patience(learned, ceiling_ns));
}

/// Folds one take of a reader's rounds into its learned wait: the longest wait of the rounds that found every record
/// still held is a sample, which ends any back-off before it; each overrun after the last such round backs the wait off
/// once. The overruns are rounds the mount ran since its last turn, so the loop is bounded by work already done.
#[cfg_attr(not(any(target_os = "linux", windows, test)), allow(dead_code))]
fn fold(learned: &mut LearnedWait, rounds: ReaderRounds) {
  if let Some(lag_ns) = rounds.longest_lag_ns {
    learned.completed(lag_ns);
  }
  for _ in 0..rounds.overruns_since {
    learned.ran_out();
  }
}

/// The patience of a reader whose learned wait is `learned`, its back-off held under `ceiling_ns`.
#[cfg_attr(not(any(target_os = "linux", windows, test)), allow(dead_code))]
fn patience(learned: &LearnedWait, ceiling_ns: u64) -> Patience {
  Patience::Bounded(learned.bound_ns(ceiling_ns))
}

#[cfg(test)]
mod tests {
  use slates_mem::budget::MetadataBudget;
  use slates_vfs::ids::Epoch;
  use slates_vfs::journal::{Op, OpLog, OpRecord};

  use super::{fold, patience};
  use crate::daemon::LIVENESS_BUDGET_NS;
  use crate::learned_wait::LearnedWait;

  /// Shape: the mount's reader's key.
  const KEY: u64 = 1;
  /// Shape: how long the mount takes to read each record: a liveness window and a half, a live mount on a loaded host.
  const LAG_NS: u64 = LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 2;
  /// Shape: the back-off's ceiling, a failover delay of four windows.
  const CEILING_NS: u64 = 4 * LIVENESS_BUDGET_NS;
  /// Shape: the rounds the mount runs: the first at the prior, the second at the back-off, the rest at the learned
  /// pace.
  const ROUNDS: usize = 6;

  /// A-548 (R3's class C; §4.5 the journal's readers). Do: open a mount's journal reader at its learned wait's
  /// patience, on a two-record budget with room in the ledger; then, each round, journal a record, let it wait a
  /// liveness window and a half for the round (a live mount whose kernel takes that long), journal past the budget as
  /// the round comes, run the round, fold what the reader's rounds found into its learned wait and name its patience
  /// again. Expect: the first round overruns, since the prior is one window, and none after it: the back-off covers the
  /// second round, whose wait is then learned. Before A-548 the patience stayed one window and every round overran, the
  /// mount's whole kernel cache dropped each time.
  #[test]
  fn a_live_mount_slower_than_a_window_is_kept_up_with_once_its_pace_is_learned() {
    let mut ledger = MetadataBudget::new(u64::MAX);
    let mut log = OpLog::new(2 * std::mem::size_of::<OpRecord>());
    let mut learned = LearnedWait::default();
    log.open_reader(KEY, log.head_seq(), patience(&learned, CEILING_NS));
    let mut now = 0;
    let mut overran = Vec::new();
    for _ in 0..ROUNDS {
      let cursor = log.head_seq();
      log.append(&mut ledger, Op::Create, "owed", None, Epoch(0), now, 0);
      now += LAG_NS;
      for _ in 0..=log.capacity() {
        log.append(&mut ledger, Op::Create, "filler", None, Epoch(0), now, 0);
      }
      overran.push(log.note_round(KEY, cursor, now).overrun);
      fold(&mut learned, log.take_rounds(KEY).unwrap());
      log.set_patience(KEY, patience(&learned, CEILING_NS));
    }
    let mut expected = vec![false; ROUNDS];
    expected[0] = true;
    assert_eq!(
      overran, expected,
      "only the first round, at the prior, overran"
    );
  }

  /// A-548 (RFC 6298 §5.5, Karn's rule). Do: fold a take of two overruns and no held round into a fresh learned wait,
  /// then a take of one held round's wait. Expect: the patience doubled twice from the prior, and the held round's wait
  /// ends the back-off, the patience then its learned bound.
  #[test]
  fn each_overrun_backs_the_patience_off_and_a_held_round_ends_it() {
    let mut learned = LearnedWait::default();
    let prior = patience(&learned, CEILING_NS);
    assert_eq!(
      prior,
      slates_vfs::journal::Patience::Bounded(LIVENESS_BUDGET_NS)
    );
    fold(
      &mut learned,
      slates_vfs::journal::ReaderRounds {
        longest_lag_ns: None,
        overruns_since: 2,
      },
    );
    assert_eq!(
      patience(&learned, CEILING_NS),
      slates_vfs::journal::Patience::Bounded(4 * LIVENESS_BUDGET_NS)
    );
    fold(
      &mut learned,
      slates_vfs::journal::ReaderRounds {
        longest_lag_ns: Some(LAG_NS),
        overruns_since: 0,
      },
    );
    let mut alone = LearnedWait::default();
    alone.completed(LAG_NS);
    assert_eq!(
      patience(&learned, CEILING_NS),
      patience(&alone, CEILING_NS),
      "the held round ended the back-off: the patience is its learned bound"
    );
  }
}
