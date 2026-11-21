//! A Subversion changeset's landing on the node's changeset store (C2 of Ada's close list; A-767 item 1; main's rulings
//! of 2026-11-20 20:30 and 21:05): the store is the truth of a repository's history, the partition's register holds
//! only what the fleet commits at f+1.
//!
//! **The order.** Every record a Subversion repository takes (a revision, a lock verb's entries, a revision's
//! properties, the UUID, a deltified text) is checked, then landed whole on the store as one changeset, durable, then
//! committed in the register by an entry naming its sequence ([`reference`]), with the green's version where it moves
//! one; the client is answered after both. The register never holds the record's bytes.
//!
//! **The sequence** is the repository's changeset count: its creation is 0, and each record the register names is the
//! next. It is read from the register when the changeset is landed ([`next_sequence`]), never kept beside it, so a
//! changeset landed and never named (a crash, or a register's refusal, between the two) is taken again by the next
//! landing, its row in the store replaced. A recovery applies only what the register named; a changeset past that is
//! dropped, and the client, never answered, commits it again once.
//!
//! **The landing turn** ([`turn`]): a repository lands one changeset at a time, from its check to its register's
//! commit, so two commits never both pass the check against the same youngest revision while one is landing. A commit
//! waiting for the turn waits as a task, woken when the turn is given back, never polled.
//!
//! **The read back** ([`changeset_record`]): a recovery or a rebuild reads each named changeset's record from the store,
//! a piece at a time past one piece, before its replay; the replay is then the live path's own (`commit_record`, a
//! revision's `RevisionReplay`).

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_db::catalog::RepositoryId;
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::owner::{OwnerError, Part, StoreHandle};

use super::Uncommitted;
use crate::state::ShardState;

/// Format: the register's reference to a Subversion changeset on the store: this mark, then its sequence, eight bytes
/// little-endian.
const MARK: &[u8] = b"slates svn changeset 1\n";

/// Shape: a reference's length: the mark and the sequence.
const REFERENCE_BYTES: usize = MARK.len() + 8;

/// The register's reference to changeset `sequence` (module docs).
pub(crate) fn reference(sequence: u64) -> Vec<u8> {
  let mut bytes = Vec::with_capacity(REFERENCE_BYTES);
  bytes.extend_from_slice(MARK);
  bytes.extend_from_slice(&sequence.to_le_bytes());
  bytes
}

/// The sequence a register's reference names; none for bytes that are not one.
pub(crate) fn sequence_of(bytes: &[u8]) -> Option<u64> {
  let rest = bytes.strip_prefix(MARK)?;
  let digits: [u8; 8] = rest.try_into().ok()?;
  Some(u64::from_le_bytes(digits))
}

/// The sequence repository `repository`'s next changeset lands at: how many its register names (module docs).
pub(crate) fn next_sequence(state: &ShardState, repository: RepositoryId) -> u64 {
  let named = state.db.partition().repositories().object_count(repository);
  u64::try_from(named).unwrap_or(u64::MAX)
}

/// The store's header of repository `repository`'s changeset `sequence`, landed under the store's standing grant
/// `grant` (its `G` row, R10: the landing's provenance).
pub(crate) fn header(repository: RepositoryId, sequence: u64, grant: u64) -> Header {
  Header {
    tool: Tool::Subversion,
    repository: u128::from_be_bytes(repository.bytes),
    sequence,
    judged_over: sequence.saturating_sub(1),
    grant,
    work: 0,
    time_ns: slates_rt::futures::now_ns(),
    sections: 0,
  }
}

/// Counter: Subversion changesets landed on the node's store. Format: a counter name in the status report.
pub(crate) const LANDED: &str = "repository.svn.changeset_landed";
/// Counter: Subversion changesets the store refused (its owner busy or gone, or its write failed), answered
/// [`Uncommitted::Unstored`]: nothing of them is in the register. Format: a counter name.
pub(crate) const UNLANDED: &str = "repository.svn.changeset_unlanded";

/// Lands `parts` as repository `repository`'s changeset `sequence` on the node's store `disk`, durable once answered;
/// refused [`Uncommitted::Unstored`] where the store refused it. Counted either way. Called holding the repository's
/// landing turn.
pub(crate) async fn land(
  disk: &StoreHandle,
  (repository, sequence): (RepositoryId, u64),
  parts: Vec<Part>,
) -> Result<(), Uncommitted> {
  let landed = disk
    .land(header(repository, sequence, disk.grant()), parts)
    .await;
  let _ = crate::state::with_state(|state| match &landed {
    Ok(_) => state.count(LANDED, 1),
    Err(refused) => {
      slates_machine::log_line!(
        "slates-server: partition {}: the store refused a Subversion changeset {sequence}: {refused}",
        state.partition
      );
      state.count(UNLANDED, 1);
    }
  });
  landed.map(drop).map_err(|_| Uncommitted::Unstored)
}

/// The node's store and the sequence repository `repository`'s next changeset lands at; refused `Unstored` where the
/// daemon holds no store (a daemon started without `--vcs-store` makes no Subversion repository).
pub(crate) fn landing_place(
  state: &ShardState,
  repository: RepositoryId,
) -> Result<(StoreHandle, u64), Uncommitted> {
  let disk = state.vcs_disk.clone().ok_or(Uncommitted::Unstored)?;
  Ok((disk, next_sequence(state, repository)))
}

/// Each repository's landing turn (module docs): who holds it and the commits waiting for it. Bounded by the
/// repositories landing now and the tasks waiting on them; a repository's gate goes once no one holds or waits on it.
#[derive(Debug, Default)]
pub(crate) struct Gates {
  /// By repository.
  gates: BTreeMap<RepositoryId, Gate>,
  /// The last turn handed out: each turn's own number, so a turn given back twice gives back nothing the second time.
  tickets: u64,
}

/// One repository's landing turn.
#[derive(Debug, Default)]
struct Gate {
  /// The turn holding it, by its number.
  held: Option<u64>,
  /// The tasks waiting for it, each woken when it is given back.
  waiting: VecDeque<Waker>,
}

impl Gates {
  /// Takes repository `repository`'s turn where no one holds it: its number. Otherwise none, and `waker` waits.
  fn take(&mut self, repository: RepositoryId, waker: Option<&Waker>) -> Option<u64> {
    let gate = self.gates.entry(repository).or_default();
    if gate.held.is_some() {
      if let Some(waker) = waker
        && !gate.waiting.iter().any(|waiting| waiting.will_wake(waker))
      {
        gate.waiting.push_back(waker.clone());
      }
      return None;
    }
    self.tickets = self.tickets.wrapping_add(1);
    gate.held = Some(self.tickets);
    Some(self.tickets)
  }

  /// Gives back repository `repository`'s turn `ticket`, waking every task waiting for it (the first to run takes it).
  fn give_back(&mut self, repository: RepositoryId, ticket: u64) {
    let Some(gate) = self.gates.get_mut(&repository) else {
      return;
    };
    if gate.held != Some(ticket) {
      return;
    }
    gate.held = None;
    let waiting = core::mem::take(&mut gate.waiting);
    self.gates.remove(&repository);
    for waker in waiting {
      waker.wake();
    }
  }

  /// Whether repository `repository`'s turn is held.
  #[cfg(test)]
  pub(crate) fn held(&self, repository: RepositoryId) -> bool {
    self
      .gates
      .get(&repository)
      .is_some_and(|gate| gate.held.is_some())
  }
}

/// A repository's landing turn, held from a changeset's check to its register's commit (module docs). Given back with
/// [`Turn::give_back`] in the step that ends the landing; a turn dropped unreturned (a task ended early) gives itself
/// back as it drops, where the shard's state is not borrowed then.
#[derive(Debug)]
#[must_use = "a landing turn held and never given back stops the repository's landings"]
pub(crate) struct Turn {
  /// The repository.
  repository: RepositoryId,
  /// Its number, until it is given back.
  ticket: Option<u64>,
}

impl Drop for Turn {
  fn drop(&mut self) {
    if let Some(ticket) = self.ticket.take() {
      let repository = self.repository;
      let _ = crate::state::with_state_counted(|state| {
        state.vcs_stores.svn_landings.give_back(repository, ticket);
      });
    }
  }
}

impl Turn {
  /// Takes repository `repository`'s turn in this step where it is free: what a landing that cannot wait takes.
  pub(crate) fn take_now(state: &mut ShardState, repository: RepositoryId) -> Option<Turn> {
    let ticket = state.vcs_stores.svn_landings.take(repository, None)?;
    Some(Turn {
      repository,
      ticket: Some(ticket),
    })
  }

  /// Gives the turn back, waking the next commit waiting for it.
  pub(crate) fn give_back(mut self, state: &mut ShardState) {
    if let Some(ticket) = self.ticket.take() {
      state
        .vcs_stores
        .svn_landings
        .give_back(self.repository, ticket);
    }
  }
}

/// Repository `repository`'s landing turn, once no one holds it; none where the shard's state is gone (its shutdown).
pub(crate) fn turn(repository: RepositoryId) -> impl Future<Output = Option<Turn>> {
  Waiting { repository }
}

/// [`turn`]'s wait.
struct Waiting {
  /// The repository.
  repository: RepositoryId,
}

impl Future for Waiting {
  type Output = Option<Turn>;

  fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
    let repository = self.repository;
    let taken = crate::state::with_state(|state| {
      state
        .vcs_stores
        .svn_landings
        .take(repository, Some(context.waker()))
    });
    match taken {
      None => Poll::Ready(None),
      Some(Some(ticket)) => Poll::Ready(Some(Turn {
        repository,
        ticket: Some(ticket),
      })),
      Some(None) => Poll::Pending,
    }
  }
}

/// Why a named changeset's record could not be read back from the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unread {
  /// The store holds no changeset of the repository at the sequence.
  Missing,
  /// The changeset holds no record section.
  NoRecord,
  /// The store's owner is gone, or its read failed.
  Store,
}

/// Repository `repository`'s changeset `sequence`'s Subversion record, read back from the store `disk`: its last
/// `Record` section (a creation's changeset holds the store's own creation record first, then Subversion's), a piece
/// at a time past one piece. A busy owner is asked again after a yield.
pub(crate) async fn changeset_record(
  disk: &StoreHandle,
  repository: RepositoryId,
  sequence: u64,
) -> Result<Vec<u8>, Unread> {
  let wanted = u128::from_be_bytes(repository.bytes);
  let rows = patiently(|| disk.changesets(wanted, sequence, 1)).await?;
  let row = rows
    .into_iter()
    .find(|row| row.repository == wanted && row.sequence == sequence)
    .ok_or(Unread::Missing)?;
  let sections = patiently(|| disk.sections(row)).await?;
  let at = sections
    .into_iter()
    .rev()
    .find(|at| at.section.kind == SectionKind::Record)
    .ok_or(Unread::NoRecord)?;
  if let Some(bytes) = at.inline {
    return Ok(bytes);
  }
  let len = usize::try_from(at.section.len).map_err(|_| Unread::Store)?;
  let mut record = Vec::new();
  record.try_reserve_exact(len).map_err(|_| Unread::Store)?;
  while record.len() < len {
    let done = u64::try_from(record.len()).map_err(|_| Unread::Store)?;
    let offset = at.section.offset.saturating_add(done);
    let rest = at.section.len.saturating_sub(done);
    let piece = patiently(|| disk.read(offset, rest)).await?;
    if piece.is_empty() {
      return Err(Unread::Store);
    }
    record.extend_from_slice(&piece);
  }
  record.truncate(len);
  Ok(record)
}

/// `ask` answered, asked again after a yield while the store's owner is busy.
async fn patiently<T, F: Future<Output = Result<T, OwnerError>>>(
  mut ask: impl FnMut() -> F,
) -> Result<T, Unread> {
  loop {
    match ask().await {
      Ok(answer) => return Ok(answer),
      Err(OwnerError::Busy) => slates_rt::futures::yield_now().await,
      Err(_) => return Err(Unread::Store),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A reference names its sequence, and nothing else reads as one.
  #[test]
  fn a_reference_names_its_sequence_and_a_record_is_not_one() {
    for sequence in [0, 1, 7, u64::MAX] {
      assert_eq!(sequence_of(&reference(sequence)), Some(sequence));
    }
    assert_eq!(sequence_of(b""), None);
    assert_eq!(sequence_of(MARK), None);
    let mut long = reference(3);
    long.push(0);
    assert_eq!(sequence_of(&long), None);
  }

  /// The turn: one holder; a second waits; given back, the waiter takes it; a stale number gives back nothing.
  #[test]
  fn a_landing_turn_has_one_holder_and_its_waiter_takes_it_next() {
    let repository = RepositoryId { bytes: [7; 16] };
    let waker = Waker::noop();
    let mut gates = Gates::default();
    let first = gates.take(repository, None).unwrap();
    assert_eq!(gates.take(repository, Some(waker)), None);
    assert_eq!(gates.take(repository, Some(waker)), None);
    gates.give_back(repository, first.wrapping_add(1));
    assert!(gates.held(repository));
    gates.give_back(repository, first);
    assert!(!gates.held(repository));
    let second = gates.take(repository, Some(waker)).unwrap();
    assert_ne!(first, second);
    gates.give_back(repository, first);
    assert!(gates.held(repository));
  }
}
