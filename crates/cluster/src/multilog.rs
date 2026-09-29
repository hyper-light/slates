//! MLRaft (`docs/wip/research/consensus-enhancements.md` §3.6; *MLRaft: Improvement of Raft based on
//! multi-log synchronization model*, ICEITCE 2022 — abstract only, tier A flagged for verification): one
//! group's log divided into `n` Raft logs over the same voters, each electing its own leader, and merged into
//! one application order every replica reaches alike.
//!
//! **Routing.** A command is either **keyed** — it reads and writes the state of one key, and reads the
//! group's global state — or **global** — it may read and write anything. A keyed command goes to the log
//! its key hashes to ([`log_of`]); a global one goes to log 0, the designated log. Keyed commands in
//! different logs touch different keys, so they commute.
//!
//! **Synchronization.** A global command in log 0 must be seen by every other log at one point of its order:
//! each other log's leader, once its replica of log 0 has committed a global command, appends a **barrier**
//! naming it ([`MultiEntry::Barrier`]). The merge ([`Merge`]) applies:
//! - a keyed entry in its log's order, in the epoch its log's last barrier opened;
//! - a barrier once the global command it names has been applied — only then may its log's later entries be;
//! - a global command only when every other log has reached a barrier naming it or a later one, so every
//!   entry those logs ordered before it has been applied first.
//!
//! So every replica applies each key's commands in one order and each in the same epoch, and applies the
//! global commands in log 0's order at the same points: the state is the same everywhere, whatever order
//! the logs' commits arrive in. The price is the barrier's: a global command applies only once every log
//! has a leader to append its barrier.
//!
//! **n = 1** is today's single log: no barriers, and the merge is log 0 in order (R8: one code path).

use slates_db::register::HostId;

use crate::raft::{ElectionPriority, LogEntry, RaftNode, RaftRecoveryError, SavedRaft};

/// Format: the first byte of a multi-log command, naming its kind.
const TAG_GLOBAL: u8 = 0;
/// Format: a keyed command's tag; its key (a little-endian `u64`) and its command follow.
const TAG_KEYED: u8 = 1;
/// Format: a barrier's tag; the log-0 index of the global command it follows (a little-endian `u64`)
/// follows.
const TAG_BARRIER: u8 = 2;
/// Format: a key's and an index's width on the wire.
const WORD_BYTES: usize = size_of::<u64>();
/// Format: the priority a log's preferred voter advertises there — the least measurable quorum round trip,
/// with no spread, which outranks every measured priority (a zero round trip is the unknown priority,
/// `ElectionPriority::default`, which outranks nothing).
const PREFERRED: ElectionPriority = ElectionPriority {
  quorum_ns: 1,
  spread_ns: 0,
};

/// Where a command goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
  /// Log 0, the designated log: a command that may read and write any of the group's state.
  Global,
  /// The log `key` hashes to: a command that writes only `key`'s state.
  Key(u64),
}

/// A multi-log entry, decoded from a Raft log entry's command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiEntry {
  /// A global command (log 0 only).
  Global(Vec<u8>),
  /// A keyed command.
  Keyed {
    /// The key it writes.
    key: u64,
    /// The command.
    command: Vec<u8>,
  },
  /// A barrier (logs 1 and above): every entry of its log after it is applied after log 0's global command
  /// at this index, and every entry before it before that command.
  Barrier(u64),
  /// Raft's own no-op (the empty command a new leader appends), or bytes that are not a multi-log entry —
  /// applied as nothing.
  NoOp,
}

impl MultiEntry {
  /// The entry's bytes, as a Raft log entry's command carries them.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      MultiEntry::Global(command) => {
        out.push(TAG_GLOBAL);
        out.extend_from_slice(command);
      }
      MultiEntry::Keyed { key, command } => {
        out.push(TAG_KEYED);
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(command);
      }
      MultiEntry::Barrier(global) => {
        out.push(TAG_BARRIER);
        out.extend_from_slice(&global.to_le_bytes());
      }
      MultiEntry::NoOp => {}
    }
    out
  }

  /// The entry `bytes` carry: [`MultiEntry::NoOp`] for the empty command and for bytes that are not a
  /// multi-log entry, which no leader of a multi-log writes.
  pub fn decode(bytes: &[u8]) -> MultiEntry {
    let Some((&tag, rest)) = bytes.split_first() else {
      return MultiEntry::NoOp;
    };
    let word = |bytes: &[u8]| {
      bytes
        .get(..WORD_BYTES)
        .and_then(|word| <[u8; WORD_BYTES]>::try_from(word).ok())
        .map(u64::from_le_bytes)
    };
    match tag {
      TAG_GLOBAL => MultiEntry::Global(rest.to_vec()),
      TAG_KEYED => match (word(rest), rest.get(WORD_BYTES..)) {
        (Some(key), Some(command)) => MultiEntry::Keyed {
          key,
          command: command.to_vec(),
        },
        _ => MultiEntry::NoOp,
      },
      TAG_BARRIER if rest.len() == WORD_BYTES => {
        word(rest).map_or(MultiEntry::NoOp, MultiEntry::Barrier)
      }
      _ => MultiEntry::NoOp,
    }
  }
}

/// Format: splitmix64's increment and finalizer multipliers (Steele, Lea & Flood, *Fast splittable
/// pseudorandom number generators*, OOPSLA 2014) — a key's hash, spread evenly whatever the keys.
const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;
const MIX_ONE: u64 = 0xbf58_476d_1ce4_e5b9;
const MIX_TWO: u64 = 0x94d0_49bb_1331_11eb;

/// The log a keyed command with `key` goes to among `logs` (at least one): its key's splitmix64 hash modulo
/// the count, so keys spread evenly over the logs.
pub fn log_of(key: u64, logs: usize) -> usize {
  let mut z = key.wrapping_add(GOLDEN_GAMMA);
  z = (z ^ (z >> 30)).wrapping_mul(MIX_ONE);
  z = (z ^ (z >> 27)).wrapping_mul(MIX_TWO);
  z ^= z >> 31;
  let count = u64::try_from(logs.max(1)).unwrap_or(u64::MAX);
  usize::try_from(z % count).unwrap_or(0)
}

/// One command the merge applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
  /// The log it was committed in.
  pub log: usize,
  /// Its index there.
  pub index: u64,
  /// Its key, or `None` for a global command.
  pub key: Option<u64>,
  /// The command.
  pub command: Vec<u8>,
  /// The log-0 index of the last global command applied before it — the global state it saw (zero before
  /// any).
  pub epoch: u64,
}

/// The merge's place in each log, and the last global command it applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
  /// Per log, the index of the next entry the merge consumes.
  next: Vec<u64>,
  /// The log-0 index of the last global command applied.
  epoch: u64,
}

/// A log's committed entries as the merge reads them: the entries from index `first` on.
pub struct Committed<'a> {
  /// The index of `entries[0]`.
  pub first: u64,
  /// The committed entries from `first` on.
  pub entries: &'a [LogEntry],
}

impl Committed<'_> {
  fn at(&self, index: u64) -> Option<&LogEntry> {
    let offset = usize::try_from(index.checked_sub(self.first)?).ok()?;
    self.entries.get(offset)
  }
}

impl Merge {
  /// A merge at the start of `logs` logs.
  pub fn new(logs: usize) -> Merge {
    Merge {
      next: vec![1; logs.max(1)],
      epoch: 0,
    }
  }

  /// The index of the next entry the merge consumes in `log`, if there is such a log.
  pub fn next_index(&self, log: usize) -> Option<u64> {
    self.next.get(log).copied()
  }

  /// The log-0 index of the last global command applied.
  pub fn epoch(&self) -> u64 {
    self.epoch
  }

  /// Consumes every entry the logs' committed prefixes `committed` (one per log, in log order) now allow, in
  /// the merged order, returning the commands applied. An entry below a log's `first` — one it compacted
  /// away before the merge reached it — is never skipped silently: the merge stops at that log.
  pub fn advance(&mut self, committed: &[Committed<'_>]) -> Vec<Applied> {
    let mut applied = Vec::new();
    loop {
      let mut moved = false;
      for log in 1..self.next.len() {
        moved |= self.advance_log(log, committed, &mut applied);
      }
      moved |= self.advance_designated(committed, &mut applied);
      if !moved {
        return applied;
      }
    }
  }

  /// Consumes log `log`'s (1 or above) entries while it may: keyed entries and no-ops, and a barrier once the
  /// global command it names has been applied. Returns whether it consumed any.
  fn advance_log(
    &mut self,
    log: usize,
    committed: &[Committed<'_>],
    applied: &mut Vec<Applied>,
  ) -> bool {
    let mut moved = false;
    while let (Some(&index), Some(entries)) = (self.next.get(log), committed.get(log)) {
      let Some(entry) = entries.at(index) else {
        return moved;
      };
      match MultiEntry::decode(&entry.command) {
        MultiEntry::Barrier(global) if global > self.epoch => return moved,
        MultiEntry::Keyed { key, command } => applied.push(Applied {
          log,
          index,
          key: Some(key),
          command,
          epoch: self.epoch,
        }),
        MultiEntry::Barrier(_) | MultiEntry::Global(_) | MultiEntry::NoOp => {}
      }
      if let Some(next) = self.next.get_mut(log) {
        *next = index.saturating_add(1);
      }
      moved = true;
    }
    moved
  }

  /// Consumes log 0's entries while it may: keyed entries and no-ops, and a global command once every other
  /// log's next entry is a committed barrier naming it or a later one. Returns whether it consumed any.
  fn advance_designated(
    &mut self,
    committed: &[Committed<'_>],
    applied: &mut Vec<Applied>,
  ) -> bool {
    let mut moved = false;
    while let (Some(&index), Some(entries)) = (self.next.first(), committed.first()) {
      let Some(entry) = entries.at(index) else {
        return moved;
      };
      match MultiEntry::decode(&entry.command) {
        MultiEntry::Global(command) => {
          if !self.others_reached(index, committed) {
            return moved;
          }
          applied.push(Applied {
            log: 0,
            index,
            key: None,
            command,
            epoch: self.epoch,
          });
          self.epoch = index;
        }
        MultiEntry::Keyed { key, command } => applied.push(Applied {
          log: 0,
          index,
          key: Some(key),
          command,
          epoch: self.epoch,
        }),
        MultiEntry::Barrier(_) | MultiEntry::NoOp => {}
      }
      if let Some(next) = self.next.first_mut() {
        *next = index.saturating_add(1);
      }
      moved = true;
    }
    moved
  }

  /// Whether every log but log 0 has reached a committed barrier naming the global command at `global` or a
  /// later one — its every entry before that point applied.
  fn others_reached(&self, global: u64, committed: &[Committed<'_>]) -> bool {
    (1..self.next.len()).all(|log| {
      let barrier = self
        .next
        .get(log)
        .and_then(|index| committed.get(log).and_then(|entries| entries.at(*index)))
        .map(|entry| MultiEntry::decode(&entry.command));
      matches!(barrier, Some(MultiEntry::Barrier(named)) if named >= global)
    })
  }
}

/// One node's share of a multi-log group: its replica of each of the logs — each a [`RaftNode`] over the same
/// voters, electing its own leader — the merge over them, and the barriers it appended while leading.
pub struct MultiLog {
  logs: Vec<RaftNode>,
  merge: Merge,
  /// Per log (the entry for log 0 unused): the term this node last led it in, and the log-0 index of the
  /// latest global command it appended a barrier after in that term. A new leadership appends afresh — a
  /// barrier naming a global command already applied is passed over, so a repeat costs one entry.
  barriered: Vec<(u64, u64)>,
  /// The log-0 index of the latest global command this node's replica of log 0 has committed, and the log-0
  /// index scanned for them through.
  latest_global: u64,
  scanned: u64,
}

impl MultiLog {
  /// `logs` logs (at least one) for node `id` over `voters`, each a fresh Raft log.
  pub fn new(id: HostId, voters: Vec<HostId>, logs: usize) -> MultiLog {
    let count = logs.max(1);
    MultiLog {
      logs: (0..count)
        .map(|_| RaftNode::new(id, voters.clone()))
        .collect(),
      merge: Merge::new(count),
      barriered: vec![(0, 0); count],
      latest_global: 0,
      scanned: 0,
    }
  }

  /// How many logs the group has.
  pub fn count(&self) -> usize {
    self.logs.len()
  }

  /// What this node must retain for each of its logs (`RaftNode::saved`), in log order.
  pub fn saved(&self) -> Vec<SavedRaft> {
    self.logs.iter().map(RaftNode::saved).collect()
  }

  /// Whether any of its logs asks for its state to be retained before its messages leave
  /// (`RaftNode::retention_pending`).
  pub fn retention_pending(&self) -> bool {
    self.logs.iter().any(RaftNode::retention_pending)
  }

  /// Records that every log's state was retained.
  pub fn retained(&mut self) {
    for node in &mut self.logs {
      node.retained();
    }
  }

  /// A node restored from what it retained for each log: each log restores as Raft's does, and the merge
  /// starts over at each log's first entry above its snapshot, so [`apply_ready`](Self::apply_ready) applies
  /// every committed entry again and the caller rebuilds its state from them, as a restarted group rebuilds
  /// from its fold's base. (A log compacted past what the merge applied would leave entries it cannot replay;
  /// n > 1 has no compaction yet, and one log's is the group's own, `crate::fold`.)
  pub fn restore(saved: Vec<SavedRaft>) -> Result<MultiLog, RaftRecoveryError> {
    let logs: Vec<RaftNode> = saved
      .into_iter()
      .map(RaftNode::restore)
      .collect::<Result<_, _>>()?;
    let count = logs.len().max(1);
    let mut merge = Merge::new(count);
    for (next, node) in merge.next.iter_mut().zip(&logs) {
      *next = node.snapshot_index().saturating_add(1);
    }
    Ok(MultiLog {
      logs,
      merge,
      barriered: vec![(0, 0); count],
      latest_global: 0,
      scanned: 0,
    })
  }

  /// This node's replica of `log`.
  pub fn log(&self, log: usize) -> Option<&RaftNode> {
    self.logs.get(log)
  }

  /// This node's replica of `log`, to drive (elections, replication, the replies it folds).
  pub fn log_mut(&mut self, log: usize) -> Option<&mut RaftNode> {
    self.logs.get_mut(log)
  }

  /// The log a command routed by `route` goes to.
  pub fn route(&self, route: Route) -> usize {
    match route {
      Route::Global => 0,
      Route::Key(key) => log_of(key, self.logs.len()),
    }
  }

  /// Appends `command` to the log `route` names when this node leads it — the caller sends it to that log's
  /// leader otherwise — and returns whether it was appended.
  pub fn propose(&mut self, route: Route, command: Vec<u8>) -> bool {
    let log = self.route(route);
    let entry = match route {
      Route::Global => MultiEntry::Global(command),
      Route::Key(key) => MultiEntry::Keyed { key, command },
    };
    self
      .logs
      .get_mut(log)
      .is_some_and(|node| node.append_command(entry.encode()))
  }

  /// Appends to each log (1 or above) this node leads a barrier after the latest global command its replica of
  /// log 0 has committed, when none it appended this term covers that command, and returns how many it
  /// appended. The drive calls it every period: a global command is applied only once every log has one.
  pub fn append_barriers(&mut self) -> usize {
    self.scan_globals();
    let latest = self.latest_global;
    self
      .logs
      .iter_mut()
      .zip(self.barriered.iter_mut())
      .skip(1)
      .map(|(node, barrier)| append_barrier_if_owed(node, barrier, latest))
      .filter(|appended| *appended)
      .count()
  }

  /// Notes the global commands log 0 has committed at this node since the last scan.
  fn scan_globals(&mut self) {
    let Some(designated) = self.logs.first() else {
      return;
    };
    let first = designated.snapshot_index().saturating_add(1);
    let entries = designated.committed_entries();
    let committed = Committed { first, entries };
    let through = designated
      .snapshot_index()
      .saturating_add(u64::try_from(entries.len()).unwrap_or(u64::MAX));
    let mut index = self.scanned.saturating_add(1).max(first);
    while index <= through {
      if let Some(MultiEntry::Global(_)) = committed
        .at(index)
        .map(|entry| MultiEntry::decode(&entry.command))
      {
        self.latest_global = index;
      }
      index = index.saturating_add(1);
    }
    self.scanned = self.scanned.max(through);
  }

  /// Every command the logs' committed prefixes now allow, in the merged order ([`Merge`]).
  pub fn apply_ready(&mut self) -> Vec<Applied> {
    self.scan_globals();
    let committed: Vec<Committed<'_>> = self
      .logs
      .iter()
      .map(|node| Committed {
        first: node.snapshot_index().saturating_add(1),
        entries: node.committed_entries(),
      })
      .collect();
    self.merge.advance(&committed)
  }

  /// The merge's state: each log's next index to apply and the last global command applied.
  pub fn merge(&self) -> &Merge {
    &self.merge
  }

  /// Spreads the logs' leaders over the voters (the abstract's "leaders are spread by priority election and
  /// dynamic leader transfer"): log `k` prefers the voter at place `k` (cyclically) of `ranked` — the voters
  /// best first, as their measured quorum round trips rank them — so each log's priority election and
  /// priority transfer move its leadership there. This node's priority in log `k` is `measured`, its own, or
  /// [`PREFERRED`] when it is log `k`'s preferred voter.
  pub fn set_priorities(&mut self, measured: ElectionPriority, ranked: &[HostId]) {
    for (log, node) in self.logs.iter_mut().enumerate() {
      let preferred = ranked
        .get(log.checked_rem(ranked.len()).unwrap_or(0))
        .copied();
      let priority = if preferred == Some(node.id()) {
        PREFERRED
      } else {
        measured
      };
      node.set_priority(priority);
    }
  }
}

/// Appends to `node`, when it leads, a barrier after log 0's global command at `latest`, unless one it appended
/// in this term already covers it (`barrier`: the term it last appended one in, and the global it named), and
/// returns whether it did.
fn append_barrier_if_owed(node: &mut RaftNode, barrier: &mut (u64, u64), latest: u64) -> bool {
  if !node.is_leader() {
    return false;
  }
  let term = node.term();
  let covered = if barrier.0 == term { barrier.1 } else { 0 };
  if latest > covered && node.append_command(MultiEntry::Barrier(latest).encode()) {
    *barrier = (term, latest);
    return true;
  }
  false
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use super::*;

  const A: HostId = HostId(1);
  const B: HostId = HostId(2);
  const C: HostId = HostId(3);

  /// A log's entries at term 1 from `entries`.
  fn log_of_entries(entries: &[MultiEntry]) -> Vec<LogEntry> {
    entries
      .iter()
      .map(|entry| LogEntry::command(1, entry.encode()))
      .collect()
  }

  fn keyed(key: u64, command: u8) -> MultiEntry {
    MultiEntry::Keyed {
      key,
      command: vec![command],
    }
  }

  fn global(command: u8) -> MultiEntry {
    MultiEntry::Global(vec![command])
  }

  /// Advances `merge` over the first `through[k]` entries of each log.
  fn advance(merge: &mut Merge, logs: &[Vec<LogEntry>], through: &[usize]) -> Vec<Applied> {
    let committed: Vec<Committed<'_>> = logs
      .iter()
      .zip(through)
      .map(|(log, through)| Committed {
        first: 1,
        entries: log.get(..*through).unwrap_or(log),
      })
      .collect();
    merge.advance(&committed)
  }

  type Group = BTreeMap<HostId, MultiLog>;

  /// Three voters, each holding `logs` logs.
  fn group(logs: usize) -> Group {
    [A, B, C]
      .into_iter()
      .map(|id| (id, MultiLog::new(id, vec![A, B, C], logs)))
      .collect()
  }

  /// Elects `leader` in `log` with the others' votes, and has it append its no-op.
  fn elect(group: &mut Group, log: usize, leader: HostId) {
    let requests = group
      .get_mut(&leader)
      .unwrap()
      .log_mut(log)
      .unwrap()
      .start_election();
    let voters: Vec<HostId> = group.keys().copied().filter(|id| *id != leader).collect();
    for (voter, request) in voters.into_iter().zip(requests) {
      let reply = group
        .get_mut(&voter)
        .unwrap()
        .log_mut(log)
        .unwrap()
        .on_request_vote(request);
      group
        .get_mut(&leader)
        .unwrap()
        .log_mut(log)
        .unwrap()
        .on_vote_reply(reply);
    }
    let node = group.get_mut(&leader).unwrap().log_mut(log).unwrap();
    assert!(node.is_leader());
    assert!(node.append_command(Vec::new()));
  }

  /// Replicates `log` from `leader` to every other voter until each holds the leader's log.
  fn replicate(group: &mut Group, log: usize, leader: HostId) {
    let followers: Vec<HostId> = group.keys().copied().filter(|id| *id != leader).collect();
    for _ in 0..4 {
      for follower in &followers {
        let Some(append) = group
          .get_mut(&leader)
          .unwrap()
          .log_mut(log)
          .unwrap()
          .replicate_to(*follower, usize::MAX)
        else {
          continue;
        };
        let reply = group
          .get_mut(follower)
          .unwrap()
          .log_mut(log)
          .unwrap()
          .on_append_entries(append);
        group
          .get_mut(&leader)
          .unwrap()
          .log_mut(log)
          .unwrap()
          .on_append_reply(reply);
      }
    }
  }

  /// §3.6, the codec: every entry round-trips, the empty command is Raft's no-op, and bytes no multi-log
  /// writes — a truncated key or barrier, an unknown tag — decode as a no-op, never as another entry.
  #[test]
  fn entries_round_trip_and_foreign_bytes_are_no_ops() {
    for entry in [
      global(7),
      MultiEntry::Global(Vec::new()),
      keyed(u64::MAX, 9),
      MultiEntry::Barrier(42),
    ] {
      assert_eq!(MultiEntry::decode(&entry.encode()), entry);
    }
    assert_eq!(MultiEntry::decode(&[]), MultiEntry::NoOp);
    for hostile in [
      vec![TAG_KEYED, 1, 2, 3],
      vec![TAG_BARRIER, 1, 2, 3],
      vec![TAG_BARRIER, 0, 0, 0, 0, 0, 0, 0, 1, 9],
      vec![0xff],
    ] {
      assert_eq!(
        MultiEntry::decode(&hostile),
        MultiEntry::NoOp,
        "{hostile:?}"
      );
    }
  }

  /// §3.6, routing: a key goes to one log, the same every time, and keys spread over every log.
  #[test]
  fn keys_route_to_one_log_each_and_spread_over_all() {
    let mut hits = [0usize; 3];
    for key in 0..3_000u64 {
      let log = log_of(key, 3);
      assert_eq!(log, log_of(key, 3));
      hits[log] += 1;
    }
    assert!(hits.iter().all(|hits| *hits > 900), "{hits:?}");
    assert_eq!(log_of(12_345, 1), 0, "one log takes every key");
  }

  /// R8: with one log there are no barriers, and the merge is log 0 in order, each command in the epoch the
  /// last global command before it opened.
  #[test]
  fn one_log_applies_in_its_own_order() {
    let logs = vec![log_of_entries(&[
      keyed(1, 1),
      global(2),
      MultiEntry::NoOp,
      keyed(1, 3),
    ])];
    let mut merge = Merge::new(1);
    let applied = advance(&mut merge, &logs, &[4]);
    let seen: Vec<(Option<u64>, Vec<u8>, u64)> = applied
      .into_iter()
      .map(|a| (a.key, a.command, a.epoch))
      .collect();
    assert_eq!(
      seen,
      vec![
        (Some(1), vec![1], 0),
        (None, vec![2], 0),
        (Some(1), vec![3], 2)
      ]
    );
  }

  /// §3.6, synchronization: a global command waits until every other log has a barrier naming it, so the
  /// keyed commands a log ordered before its barrier are applied before it, and those after, after it.
  #[test]
  fn a_global_command_waits_for_every_logs_barrier() {
    let logs = vec![
      log_of_entries(&[global(1)]),
      log_of_entries(&[keyed(10, 2), MultiEntry::Barrier(1), keyed(10, 3)]),
      log_of_entries(&[keyed(20, 4), MultiEntry::Barrier(1)]),
    ];
    let mut merge = Merge::new(3);
    let first = advance(&mut merge, &logs, &[1, 1, 1]);
    assert_eq!(
      first.len(),
      2,
      "the two keyed commands before any barrier, at epoch 0"
    );
    assert!(first.iter().all(|applied| applied.epoch == 0));
    assert!(
      advance(&mut merge, &logs, &[1, 3, 1]).is_empty(),
      "log 2 has no barrier yet"
    );
    let rest = advance(&mut merge, &logs, &[1, 3, 2]);
    let seen: Vec<(usize, Vec<u8>, u64)> = rest
      .into_iter()
      .map(|a| (a.log, a.command, a.epoch))
      .collect();
    assert_eq!(
      seen,
      vec![(0, vec![1], 0), (1, vec![3], 1)],
      "the global, then what log 1 ordered after it"
    );
  }

  /// §3.6, determinism: replicas that learn the same logs' commits in different orders apply each key's
  /// commands in one order, each in the same epoch, and the global commands in one order — the state is the
  /// same everywhere.
  #[test]
  fn replicas_learning_commits_in_any_order_reach_one_state() {
    let logs = vec![
      log_of_entries(&[keyed(3, 1), global(2), keyed(3, 3), global(4), keyed(6, 5)]),
      log_of_entries(&[
        keyed(1, 6),
        MultiEntry::Barrier(2),
        keyed(1, 7),
        MultiEntry::Barrier(4),
        keyed(1, 8),
      ]),
      log_of_entries(&[MultiEntry::Barrier(4), keyed(2, 9), keyed(2, 10)]),
    ];
    let lengths: Vec<usize> = logs.iter().map(Vec::len).collect();
    let summary = |applied: Vec<Applied>| {
      let mut keys: BTreeMap<Option<u64>, Vec<(Vec<u8>, u64)>> = BTreeMap::new();
      for applied in applied {
        keys
          .entry(applied.key)
          .or_default()
          .push((applied.command, applied.epoch));
      }
      keys
    };
    let mut reference = None;
    for seed in 0..64u64 {
      // One replica's order of learning commits: at each step one log's committed prefix grows by one.
      let mut through = vec![0usize; logs.len()];
      let mut merge = Merge::new(logs.len());
      let mut applied = Vec::new();
      let mut draw = seed;
      while through.iter().zip(&lengths).any(|(at, length)| at < length) {
        draw = draw
          .wrapping_mul(6_364_136_223_846_793_005)
          .wrapping_add(1_442_695_040_888_963_407);
        let log = usize::try_from((draw >> 33) % 3).unwrap();
        if through[log] < lengths[log] {
          through[log] += 1;
          applied.extend(advance(&mut merge, &logs, &through));
        }
      }
      let summary = summary(applied);
      assert_eq!(
        *reference.get_or_insert_with(|| summary.clone()),
        summary,
        "seed {seed}"
      );
    }
    let reference = reference.unwrap();
    assert_eq!(
      reference[&None],
      vec![(vec![2], 0), (vec![4], 2)],
      "both globals, in log 0's order"
    );
    assert_eq!(
      reference[&Some(1)],
      vec![(vec![6], 0), (vec![7], 2), (vec![8], 4)]
    );
    assert_eq!(reference[&Some(2)], vec![(vec![9], 4), (vec![10], 4)]);
  }

  /// The first key that routes to `log` of `logs`.
  fn key_in(log: usize, logs: usize) -> u64 {
    (0..).find(|key| log_of(*key, logs) == log).unwrap()
  }

  /// Proposes `command` under a key of each log but log 0, at that log's leader (`leaders[log]`).
  fn propose_in_every_other_log(group: &mut Group, leaders: &[HostId], command: u8) {
    for (log, leader) in leaders.iter().enumerate().skip(1) {
      let key = key_in(log, leaders.len());
      assert!(
        group
          .get_mut(leader)
          .unwrap()
          .propose(Route::Key(key), vec![command])
      );
    }
  }

  /// Each log's leader (`leaders[log]`) replicates it to the other voters.
  fn replicate_every_log(group: &mut Group, leaders: &[HostId]) {
    for (log, leader) in leaders.iter().enumerate() {
      replicate(group, log, *leader);
    }
  }

  /// What `node` applies now, per key (`None` for the global commands): each command with its epoch.
  fn applications_by_key(node: &mut MultiLog) -> BTreeMap<Option<u64>, Vec<(Vec<u8>, u64)>> {
    let mut keys: BTreeMap<Option<u64>, Vec<(Vec<u8>, u64)>> = BTreeMap::new();
    for applied in node.apply_ready() {
      keys
        .entry(applied.key)
        .or_default()
        .push((applied.command, applied.epoch));
    }
    keys
  }

  /// §3.6 over real Raft logs: three logs, each led by a different voter. Keyed and global commands are
  /// proposed at their logs' leaders, each log's leader appends its barrier once log 0's global commands reach
  /// it, and every voter applies each key's commands in one order and each in one epoch.
  #[test]
  fn three_logs_led_apart_merge_alike_on_every_voter() {
    let mut group = group(3);
    let leaders = [A, B, C];
    for (log, leader) in leaders.iter().enumerate() {
      elect(&mut group, log, *leader);
    }
    propose_in_every_other_log(&mut group, &leaders, 1);
    let designated = group.get_mut(&A).unwrap();
    assert!(designated.propose(Route::Global, vec![2]));
    assert!(designated.propose(Route::Key(key_in(0, leaders.len())), vec![3]));
    replicate_every_log(&mut group, &leaders);
    let barriers: Vec<usize> = leaders[1..]
      .iter()
      .map(|leader| group.get_mut(leader).unwrap().append_barriers())
      .collect();
    assert_eq!(barriers, vec![1, 1], "one barrier after the global");
    propose_in_every_other_log(&mut group, &leaders, 4);
    replicate_every_log(&mut group, &leaders);
    let outcomes: Vec<_> = group.values_mut().map(applications_by_key).collect();
    assert!(
      outcomes.windows(2).all(|pair| pair[0] == pair[1]),
      "{outcomes:?}"
    );
    assert_eq!(
      outcomes[0][&None][0].1, 0,
      "the global applied before any other"
    );
    for log in 1..leaders.len() {
      let history = &outcomes[0][&Some(key_in(log, leaders.len()))];
      assert_eq!(history.len(), 2);
      assert!(
        history[0].1 < history[1].1,
        "the second command, after the barrier, saw the global: {history:?}"
      );
    }
  }

  /// §3.6, leaders spread by priority and transfer: with one voter leading all three logs, each log whose
  /// preferred voter is another hands off to it once its leader has led the priority windows — so the logs end
  /// led apart. Until 2026-09-29 the preferred voter advertised a zero round trip, the unknown priority, which
  /// outranks nothing: no log ever handed off, and the timed simulation's best voter led all five logs.
  #[test]
  fn each_log_hands_off_to_its_preferred_voter() {
    let mut group = group(3);
    for log in 0..3 {
      elect(&mut group, log, A);
    }
    let measured = ElectionPriority {
      quorum_ns: 80_000_000,
      spread_ns: 5_000_000,
    };
    for node in group.values_mut() {
      node.set_priorities(measured, &[A, B, C]);
    }
    let alive = [A, B, C];
    let mut targets = Vec::new();
    for log in 0..3 {
      for _ in 0..crate::raft::PRIORITY_WINDOWS {
        // A round of replies — carrying each follower's priority in this log — then the CheckQuorum tick.
        replicate(&mut group, log, A);
        group
          .get_mut(&A)
          .unwrap()
          .log_mut(log)
          .unwrap()
          .check_quorum();
      }
      targets.push(
        group
          .get_mut(&A)
          .unwrap()
          .log_mut(log)
          .unwrap()
          .priority_transfer(&alive),
      );
    }
    assert_eq!(
      targets,
      vec![None, Some(B), Some(C)],
      "log 0 stays with A; logs 1 and 2 go to B and C"
    );
  }
}
