//! A repository's logs over the fenced ledger: each log replicated on its own, every record held whole (§4.17.1 shared
//! piece 2, the fleet half; A-642, A-636; §4.8 "Promotion and takeover"; §4.16 "Commit").
//!
//! **What it is.** A repository keeps a log for each green, its chain of versions, and one for each part of its own
//! state: the names, Mercurial's phase roots and markers, Git LFS's locks (A-642). Each log is a register of its own,
//! a [`Cohort`] placed by the log's own object id, whose records commit at `f + 1` of its own `2f + 1` candidates. So a
//! push to one green waits on that green's holders alone, never on another log's (A-642: with a log per green only that
//! green's later pushes wait). One owner, the repository's partition's, proposes to every log, and a takeover moves
//! them together.
//!
//! **A record held whole.** A record moves one log or several by spans of positions ([`LogSpan`], A-635), and every
//! position it takes carries its identity; its body is content, kept beside the logs. A record is committed once every
//! log it moves holds it on `f + 1` holders and every record before it in those logs is committed too. The
//! repository's committed state is therefore a cut, one position a log: the greatest at or below each log's committed
//! length that holds every record it touches whole ([`LogsCohort::committed_cut`], the oracle;
//! [`LogsOwner::committed_cut`], what the owner learned from its acknowledgements). A holder applies the owner's cut once
//! it holds the owner's whole log ([`LogsOwner::announce`]); the cut holds every record whole, so every log of a record
//! the holder holds reaches the record in that one step, its join. A replica answers a read at a vector of positions,
//! one a log ([`LogsCohort::read`]); it refuses a vector past what it applied, and one that holds part of a record and
//! not the rest (A-642: a replica's read refuses a cut an atomic record would split).
//!
//! **Takeover.** The successor fences and reads `f + 1` holders of every log, each as [`Owner::take_over`] reads one
//! register, or of none: every log's round is asked first ([`Cohort::can_take_over`]), so a takeover that cannot fence
//! every log fences nothing. It then keeps a record only where the logs it adopted hold it at every position the record
//! moved, and otherwise drops it, with what follows it in each of its logs, until every record left is whole
//! ([`LogsOwner::take_over`]). A committed record is never dropped: each log's read meets that log's commit quorum, so
//! every log the record moves adopts it at its positions (the ledger's NoLoss, a log at a time), and every record before
//! it is committed too, so no drop reaches it. A record dropped was never committed whole, so no holder applied any
//! part of it and no replica answered with one. The superseded owner reaches at most `f` holders of any log the
//! takeover fenced, so it commits nothing more on any log (StaleNeverCommits).
//!
//! **What is simulated.** As [`super`] is, this is a pure, deterministic simulation (R8): holders are in-memory state
//! and a message is a direct call, so `f = 0` is the laptop and `f > 0` the fleet, with no mode switch. Its model test
//! drives appends, partitions, takeovers, superseded owners and replica reads, holds Agreement, NoLoss and
//! StaleNeverCommits after every step, and counts what it met. The live plane's half is owed (A-636 part 2): each log's
//! register on the wire, and the state logs' register class, whose object ids a caller names here.

use std::collections::{BTreeMap, BTreeSet};

use super::{Cohort, Owner, Reach, TakeoverError};
use crate::catalog::{LogName, LogSpan, spans_split_by};
use crate::register::{DomainId, HostId, ObjectId, Quorum};

#[cfg(test)]
mod tests;

/// Positions, one a log: a cut of a repository's logs, or a vector a replica reads at.
pub type Cut = BTreeMap<LogName, u64>;

/// The identities a replica answered, a log at a time.
pub type Answer = BTreeMap<LogName, Vec<[u8; 32]>>;

/// A record of a repository's logs as the fleet replicates it (A-636): its identity, which every position it takes
/// carries, and what it moves of each log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
  /// The record's identity: its body's, unique among the repository's records.
  pub identity: [u8; 32],
  /// What it moves of each log, from the position its push read there: one span a log.
  pub spans: Vec<LogSpan>,
}

/// Why an append was refused, nothing of it proposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppendRefused {
  /// The record moves no log.
  Empty,
  /// A span names a log the repository does not keep.
  UnknownLog {
    /// The log.
    log: LogName,
  },
  /// A span moves no position, or a second span names the same log.
  Malformed {
    /// The log.
    log: LogName,
  },
  /// A log is not at the position the record read (A-642): the owner moved it since.
  Moved {
    /// The log.
    log: LogName,
    /// The position the record read.
    read: u64,
    /// The position the log is at.
    at: u64,
  },
  /// The identity names another record already.
  IdentityTaken,
}

/// What an append reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appended {
  /// Whether the record is committed whole now: every log it moves holds it on `f + 1` holders, and every record before
  /// it is committed too.
  pub whole: bool,
}

/// Why a replica refused a read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadRefused {
  /// The replica holds no copy of the log: it is none of the log's candidates.
  NotHeld {
    /// The log.
    log: LogName,
  },
  /// The read asks past what the replica applied of the log.
  NotApplied {
    /// The log.
    log: LogName,
    /// What the replica applied.
    applied: u64,
    /// What the read asked.
    asked: u64,
  },
  /// The vector holds part of a record and not the rest.
  Split {
    /// The record.
    identity: [u8; 32],
  },
}

/// Why a takeover of a repository's logs was refused, nothing fenced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogsTakeoverRefused {
  /// The log whose round could not run.
  pub log: LogName,
  /// Why.
  pub error: TakeoverError,
}

/// A takeover's outcome: the new owner, and the records it dropped because no read held them whole.
#[derive(Clone, Debug)]
pub struct TakenOver {
  /// The new owner of every log.
  pub owner: LogsOwner,
  /// The records dropped, in the order found.
  pub dropped: Vec<[u8; 32]>,
}

/// The holders of every log of one repository (A-636): each log's [`Cohort`], the records' bodies as content, and what
/// each holder applied.
#[derive(Clone, Debug)]
pub struct LogsCohort {
  /// The repository's owner, a candidate of every log.
  owner: HostId,
  /// Each log's register.
  logs: BTreeMap<LogName, Cohort>,
  /// Each record's spans, by its identity: the bodies the logs' positions name.
  records: BTreeMap<[u8; 32], Vec<LogSpan>>,
  /// What each holder applied, a log at a time.
  applied: BTreeMap<HostId, Cut>,
}

impl LogsCohort {
  /// The holders of `logs`, each log placed by its own object id among the owner's neighbourhood as [`Cohort::new`]
  /// places one register, every holder empty.
  pub fn new(
    owner: HostId,
    neighbourhood: &[HostId],
    domains: &BTreeMap<HostId, DomainId>,
    logs: &[(LogName, ObjectId)],
    quorum: Quorum,
  ) -> LogsCohort {
    let logs = logs
      .iter()
      .map(|(name, object)| {
        let cohort = Cohort::new(owner, neighbourhood, domains, *object, quorum);
        (*name, cohort)
      })
      .collect();
    LogsCohort {
      owner,
      logs,
      records: BTreeMap::new(),
      applied: BTreeMap::new(),
    }
  }

  /// The candidates of `log`, owner first; none for a log the repository does not keep.
  pub fn candidates(&self, log: &LogName) -> &[HostId] {
    self
      .logs
      .get(log)
      .map(Cohort::candidates)
      .unwrap_or_default()
  }

  /// What `host` applied, a log at a time; nothing for a host that applied nothing.
  pub fn applied(&self, host: HostId) -> Cut {
    self.applied.get(&host).cloned().unwrap_or_default()
  }

  /// The oracle for what the repository committed (A-636): each log's prefix `f + 1` of its holders agree on, lowered
  /// to the records it holds whole.
  pub fn committed_cut(&self) -> Cut {
    let limits = self
      .logs
      .iter()
      .map(|(name, log)| (*name, as_position(log.committed_prefix().len())))
      .collect();
    let identity_at = |name: &LogName, position: usize| self.logs.get(name)?.committed_at(position);
    whole_cut(limits, &identity_at, &self.records)
  }

  /// The identities [`LogsCohort::committed_cut`] holds, a log at a time.
  pub fn committed_history(&self) -> Answer {
    let cut = self.committed_cut();
    self
      .logs
      .iter()
      .map(|(name, log)| {
        let mut prefix = log.committed_prefix();
        prefix.truncate(as_index(cut.get(name).copied().unwrap_or(0)));
        (*name, prefix)
      })
      .collect()
  }

  /// The first log and position at which two identities are each held by a quorum: the Agreement violation that must
  /// never occur.
  pub fn diverges(&self) -> Option<(LogName, usize)> {
    self.logs.iter().find_map(|(name, log)| {
      let longest = log
        .candidates()
        .iter()
        .filter_map(|host| log.held(*host))
        .map(<[_]>::len)
        .max()
        .unwrap_or(0);
      (0..longest)
        .find(|position| log.diverges_at(*position))
        .map(|position| (*name, position))
    })
  }

  /// A replica's read at `vector` (A-642): what `host` holds of each log the vector names, up to its position. Refused
  /// for a log the host holds no copy of, past what it applied, or where the vector holds part of a record and not the
  /// rest.
  pub fn read(&self, host: HostId, vector: &Cut) -> Result<Answer, ReadRefused> {
    let mut answer = Answer::new();
    for (name, asked) in vector {
      let held = self
        .logs
        .get(name)
        .and_then(|log| log.held(host))
        .ok_or(ReadRefused::NotHeld { log: *name })?;
      let applied = self.applied_at(host, name);
      if *asked > applied {
        return Err(ReadRefused::NotApplied {
          log: *name,
          applied,
          asked: *asked,
        });
      }
      let prefix = held
        .iter()
        .take(as_index(*asked))
        .map(|record| record.identity);
      answer.insert(*name, prefix.collect());
    }
    let at = |log: &LogName| vector.get(log).copied();
    let split = answer.values().flatten().find(|identity| {
      self
        .records
        .get(*identity)
        .is_some_and(|spans| spans_split_by(spans, at))
    });
    match split {
      Some(identity) => Err(ReadRefused::Split {
        identity: *identity,
      }),
      None => Ok(answer),
    }
  }

  /// What `host` applied of `log`.
  fn applied_at(&self, host: HostId, log: &LogName) -> u64 {
    self
      .applied
      .get(&host)
      .and_then(|cut| cut.get(log))
      .copied()
      .unwrap_or(0)
  }

  /// `host`, which acknowledged the owner's whole log of every log in `held`, applies the owner's committed `cut` over
  /// them. The cut holds every record whole, so every log of a record the host holds reaches the record in this one
  /// step: its join. What it applied before stays applied, since committed positions never change.
  fn apply(&mut self, host: HostId, held: &BTreeSet<LogName>, cut: &Cut) {
    let applied = self.applied.entry(host).or_default();
    for name in held {
      let at = applied.entry(*name).or_insert(0);
      *at = (*at).max(cut.get(name).copied().unwrap_or(0));
    }
  }
}

/// The owner of a repository's logs (A-636): one proposer view a log ([`Owner`]), and the length of each it learned
/// committed. A takeover makes a new one by value; the superseded one is kept, and refused, as [`Owner`] is.
#[derive(Clone, Debug)]
pub struct LogsOwner {
  /// The owning host.
  pub id: HostId,
  /// Each log's proposer view.
  owners: BTreeMap<LogName, Owner>,
  /// The length of each log `f + 1` of its holders acknowledged whole.
  committed: Cut,
}

impl LogsOwner {
  /// The first owner of a repository's fresh logs: its owner, at each log's first epoch, every log empty.
  pub fn bootstrap(cohort: &LogsCohort) -> LogsOwner {
    let owners = cohort
      .logs
      .iter()
      .map(|(name, log)| (*name, Owner::bootstrap(log)))
      .collect();
    LogsOwner {
      id: cohort.owner,
      owners,
      committed: Cut::new(),
    }
  }

  /// Where the owner holds `log`: the position a push reads there. `None` for a log the repository does not keep.
  pub fn position(&self, log: &LogName) -> Option<u64> {
    self.owners.get(log).map(|owner| as_position(owner.len()))
  }

  /// The identities the owner holds of `log`, by position.
  pub fn log(&self, log: &LogName) -> &[[u8; 32]] {
    self.owners.get(log).map(Owner::log).unwrap_or_default()
  }

  /// What this owner learned committed (A-636): each log's length `f + 1` of its holders acknowledged, lowered to whole
  /// records.
  pub fn committed_cut(&self, cohort: &LogsCohort) -> Cut {
    let limits = self
      .owners
      .keys()
      .map(|name| (*name, self.committed.get(name).copied().unwrap_or(0)))
      .collect();
    let identity_at =
      |name: &LogName, position: usize| self.owners.get(name)?.log().get(position).copied();
    whole_cut(limits, &identity_at, &cohort.records)
  }

  /// Appends `record` to every log it moves, each span's positions proposed in order to that log's holders (A-636), or
  /// refuses it whole: a log the owner moved since the record's push read it is refused `Moved` (A-642). A log commits
  /// at `f + 1` of its own holders; the record is whole once every log it moves has.
  pub fn append(
    &mut self,
    cohort: &mut LogsCohort,
    record: LogRecord,
    reachable: &Reach,
  ) -> Result<Appended, AppendRefused> {
    self.admit(cohort, &record)?;
    cohort.records.insert(record.identity, record.spans.clone());
    for span in &record.spans {
      let (Some(owner), Some(log)) = (
        self.owners.get_mut(&span.log),
        cohort.logs.get_mut(&span.log),
      ) else {
        continue;
      };
      let mut last = None;
      for _ in span.from..span.to {
        last = Some(owner.propose(log, record.identity, reachable));
      }
      if last.is_some_and(|commit| commit.committed) {
        self.committed.insert(span.log, as_position(owner.len()));
      }
    }
    Ok(Appended {
      whole: self.holds_whole(cohort, &record.spans),
    })
  }

  /// Offers every log again and has its holders apply what is committed, at their joins (A-636): a log `f + 1` of whose
  /// holders acknowledge is committed to its length; each holder that acknowledged applies the owner's committed cut over
  /// the logs it now holds. Returns that cut.
  pub fn announce(&mut self, cohort: &mut LogsCohort, reachable: &Reach) -> Cut {
    let mut holding: BTreeMap<HostId, BTreeSet<LogName>> = BTreeMap::new();
    for (name, owner) in &mut self.owners {
      let Some(log) = cohort.logs.get_mut(name) else {
        continue;
      };
      let commit = owner.offer_again(log, reachable);
      if commit.committed {
        self.committed.insert(*name, as_position(owner.len()));
      }
      for host in commit.acked {
        holding.entry(host).or_default().insert(*name);
      }
    }
    let cut = self.committed_cut(cohort);
    for (host, held) in &holding {
      cohort.apply(*host, held, &cut);
    }
    cut
  }

  /// Takes every log of the repository over as `new_id` (A-636): refused, nothing fenced, unless every log's round can
  /// run; then each log fenced, read and adopted as [`Owner::take_over`] does, and every record the adopted logs do not
  /// hold whole dropped with what follows it in each of its logs.
  pub fn take_over(
    cohort: &mut LogsCohort,
    new_id: HostId,
    reachable: &Reach,
  ) -> Result<TakenOver, LogsTakeoverRefused> {
    for (name, log) in &cohort.logs {
      log
        .can_take_over(reachable)
        .map_err(|error| LogsTakeoverRefused { log: *name, error })?;
    }
    let mut owners = BTreeMap::new();
    for (name, log) in &mut cohort.logs {
      let owner = Owner::take_over(log, new_id, reachable)
        .map_err(|error| LogsTakeoverRefused { log: *name, error })?;
      owners.insert(*name, owner);
    }
    let dropped = keep_whole(&mut owners, &cohort.records);
    Ok(TakenOver {
      owner: LogsOwner {
        id: new_id,
        owners,
        committed: Cut::new(),
      },
      dropped,
    })
  }

  /// Refuses `record` before anything is proposed: no span, an identity already taken, a log the repository does not
  /// keep, a span that moves nothing or names its log twice, or a log not at the position the record read.
  fn admit(&self, cohort: &LogsCohort, record: &LogRecord) -> Result<(), AppendRefused> {
    if record.spans.is_empty() {
      return Err(AppendRefused::Empty);
    }
    if cohort.records.contains_key(&record.identity) {
      return Err(AppendRefused::IdentityTaken);
    }
    let mut named = BTreeSet::new();
    for span in &record.spans {
      let at = self
        .position(&span.log)
        .ok_or(AppendRefused::UnknownLog { log: span.log })?;
      if span.to <= span.from || !named.insert(span.log) {
        return Err(AppendRefused::Malformed { log: span.log });
      }
      if span.from != at {
        return Err(AppendRefused::Moved {
          log: span.log,
          read: span.from,
          at,
        });
      }
    }
    Ok(())
  }

  /// Whether this owner's committed cut holds every span of `spans`.
  fn holds_whole(&self, cohort: &LogsCohort, spans: &[LogSpan]) -> bool {
    let cut = self.committed_cut(cohort);
    spans
      .iter()
      .all(|span| cut.get(&span.log).is_some_and(|at| *at >= span.to))
  }
}

/// Drops every record the adopted logs do not hold whole (A-636): a record some log of which lacks it at a position it
/// moves is dropped from each log that holds part of it, with everything after it there, until every record left is
/// whole. Returns the records dropped, in the order found.
fn keep_whole(
  owners: &mut BTreeMap<LogName, Owner>,
  records: &BTreeMap<[u8; 32], Vec<LogSpan>>,
) -> Vec<[u8; 32]> {
  let mut dropped = Vec::new();
  while let Some(identity) = first_not_whole(owners, records) {
    for owner in owners.values_mut() {
      let first = owner.log().iter().position(|held| *held == identity);
      if let Some(first) = first {
        owner.truncate(first);
      }
    }
    dropped.push(identity);
  }
  dropped
}

/// The first record, in log order, that the owners' logs hold part of and not all, or whose body is unknown.
fn first_not_whole(
  owners: &BTreeMap<LogName, Owner>,
  records: &BTreeMap<[u8; 32], Vec<LogSpan>>,
) -> Option<[u8; 32]> {
  owners
    .values()
    .flat_map(|owner| owner.log().iter())
    .find(|identity| {
      records
        .get(*identity)
        .is_none_or(|spans| !held_whole(spans, **identity, owners))
    })
    .copied()
}

/// Whether every log `spans` names holds `identity` at every position its span moves.
fn held_whole(spans: &[LogSpan], identity: [u8; 32], owners: &BTreeMap<LogName, Owner>) -> bool {
  spans.iter().all(|span| {
    owners.get(&span.log).is_some_and(|owner| {
      (span.from..span.to).all(|position| owner.log().get(as_index(position)) == Some(&identity))
    })
  })
}

/// The greatest cut at or below `limits` that holds every record it touches whole (A-642, A-636): a log's position is
/// lowered to the first position of a record that some log of it does not hold within the cut, until none is.
/// `identity_at` reads a log's identity at a position.
fn whole_cut(
  mut limits: Cut,
  identity_at: &dyn Fn(&LogName, usize) -> Option<[u8; 32]>,
  records: &BTreeMap<[u8; 32], Vec<LogSpan>>,
) -> Cut {
  while let Some((log, position)) = first_partial(&limits, identity_at, records) {
    limits.insert(log, position);
  }
  limits
}

/// The first log and position, in log order, of a record `limits` holds part of and not all, or of a position whose
/// identity or body is unknown: where [`whole_cut`] lowers that log next.
fn first_partial(
  limits: &Cut,
  identity_at: &dyn Fn(&LogName, usize) -> Option<[u8; 32]>,
  records: &BTreeMap<[u8; 32], Vec<LogSpan>>,
) -> Option<(LogName, u64)> {
  limits.iter().find_map(|(log, limit)| {
    (0..as_index(*limit))
      .find_map(|index| lowered_to(log, index, limits, identity_at, records))
      .map(|position| (*log, position))
  })
}

/// Where [`whole_cut`] lowers `log` for its entry at `index`: the first position of the record there when `limits`
/// does not hold it whole, the position itself when its identity or body is unknown, `None` when the record is whole.
fn lowered_to(
  log: &LogName,
  index: usize,
  limits: &Cut,
  identity_at: &dyn Fn(&LogName, usize) -> Option<[u8; 32]>,
  records: &BTreeMap<[u8; 32], Vec<LogSpan>>,
) -> Option<u64> {
  let position = as_position(index);
  let Some(identity) = identity_at(log, index) else {
    return Some(position);
  };
  let Some(spans) = records.get(&identity) else {
    return Some(position);
  };
  if spans
    .iter()
    .all(|span| span_within(span, identity, limits, identity_at))
  {
    return None;
  }
  let from = spans
    .iter()
    .find(|span| span.log == *log)
    .map(|span| span.from);
  Some(from.unwrap_or(position).min(position))
}

/// Whether a cut holds `span` of the record `identity`: its log's limit past the span, and every position of it carrying
/// the record.
fn span_within(
  span: &LogSpan,
  identity: [u8; 32],
  limits: &Cut,
  identity_at: &dyn Fn(&LogName, usize) -> Option<[u8; 32]>,
) -> bool {
  limits.get(&span.log).is_some_and(|limit| {
    *limit >= span.to
      && (span.from..span.to)
        .all(|position| identity_at(&span.log, as_index(position)) == Some(identity))
  })
}

/// A log length as a position.
fn as_position(length: usize) -> u64 {
  u64::try_from(length).unwrap_or(u64::MAX)
}

/// A position as a count of entries, saturating where it does not fit.
fn as_index(position: u64) -> usize {
  usize::try_from(position).unwrap_or(usize::MAX)
}
