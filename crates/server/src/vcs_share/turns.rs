//! A shard's turns divided by repository, not by push (A-724 piece 3c; §4.17.5 "Shares"; main's order of 2026-11-20 to
//! fix the small repositories' starvation, failing test first).
//!
//! **What was measured.** The runtime polls its ready tasks in turn, and a push's answer takes its checks a turn at a
//! time, each about one object (15 to 23 µs on the 32-core Studio against a step quantum of a few µs). So the shard is
//! fair per push: a repository with three pushes under way takes three turns a round to a small repository's one. At
//! R = 100 with git/git v1.5.0's pushes capped at three a repository, a small repository's push was served 0.25 s and
//! waited 0.96 s before its check turns on average; the three on the hot repository's shard were served 0.25 s in a
//! span of 2.77 s, about a twelfth of their shard (BENCHMARKS.md, "A shard's turns by repository").
//!
//! **The rule.** A push takes its repository's turn before each step of its answer ([`take`]) and keeps it across the
//! yield after the step ([`Turn::yield_holding`]), so the repository's other pushes on the shard stay parked, never
//! polled, until that turn ends; it then goes to the next of them in order (a FIFO, each woken as the turn before ends).
//! So each repository with work takes one turn a round of the shard, whatever its pushes: deficit round robin with a
//! quantum of a turn, a repository its flow [A: Shreedhar and Varghese, "Efficient Fair Queueing Using Deficit Round
//! Robin", SIGCOMM 1995]. A repository's pushes all progress, in order.
//!
//! **Why the earlier lanes were even.** `agents/w-balance-lanes` (1b8010400) held a repository's lane only inside a
//! step, which runs within one poll, and gave it back before the yield, so a sibling almost never found it held: the
//! rounds stayed per push (BENCHMARKS.md, "Many repositories on one daemon"). Held across the yield, the turn is the
//! round's.
//!
//! **Bounded.** One entry a repository with a push holding or waiting for its turn, each waiter one ticket and its waker,
//! so bounded by the pushes under way on the shard; an entry with no holder and no waiter is removed.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_db::catalog::RepositoryId;

/// One repository's turn on this shard: whether a push holds it, and the pushes waiting, first first.
#[derive(Debug, Default)]
struct Line {
  /// Whether a push holds the turn.
  held: bool,
  /// The waiting pushes: each its ticket and the waker to wake when the turn is its.
  waiting: VecDeque<(u64, Option<Waker>)>,
}

/// Every repository's turn on this shard (module docs).
#[derive(Debug, Default)]
struct Turns {
  lines: BTreeMap<RepositoryId, Line>,
  /// The next ticket: counts up, one a wait.
  next: u64,
}

std::thread_local! {
  /// This shard's turns.
  static TURNS: RefCell<Turns> = RefCell::new(Turns::default());
  /// Per-repository FIFO push intake gates on this shard: serializes active pack body intake per
  /// repository so concurrent pushes keep body bytes in kernel TCP flow control instead of
  /// interleaving thousands of partial packs in userspace RAM, while different repositories and
  /// all read/fetch requests continue sharing the shard cooperatively.
  static INTAKE: RefCell<Turns> = RefCell::new(Turns::default());
  /// Per-repository FIFO pack-generation gates on this shard: serializes active `command=fetch`
  /// pack builds per repository so thousands of concurrent fetches do not run in lockstep
  /// round-robin (which multiplies per-upload RAM, destroys L1/L2 cache locality, floods kernel
  /// socket buffers, and makes every fetch finish together at the tail), while metadata queries
  /// (`info/refs`, `ls-refs`, `object-info`), pushes, and other repositories never wait.
  static FETCH: RefCell<Turns> = RefCell::new(Turns::default());
}

/// Polls a thread-local FIFO line in `cell` for `repository`, returning `Ok(())` when acquired or
/// `Err(ticket)` when queued.
fn poll_line(
  cell: &'static std::thread::LocalKey<RefCell<Turns>>,
  repository: RepositoryId,
  ticket: Option<u64>,
  waker: &Waker,
) -> Result<(), Option<u64>> {
  cell.with(|turns| {
    let Ok(mut turns) = turns.try_borrow_mut() else {
      return Err(ticket);
    };
    let Turns { lines, next } = &mut *turns;
    let line = lines.entry(repository).or_default();
    let first = line.waiting.front().map(|(id, _)| *id);
    let mine = ticket.is_some() && first == ticket;
    if !line.held && (line.waiting.is_empty() || mine) {
      if mine {
        line.waiting.pop_front();
      }
      line.held = true;
      return Ok(());
    }
    let ticket = ticket.unwrap_or_else(|| {
      let id = *next;
      *next = next.wrapping_add(1);
      line.waiting.push_back((id, None));
      id
    });
    if let Some((_, slot)) = line.waiting.iter_mut().find(|(id, _)| *id == ticket) {
      *slot = Some(waker.clone());
    }
    Err(Some(ticket))
  })
}

/// Removes a cancelled waiter `ticket` from `repository`'s line in `cell` and wakes the next
/// waiter if the line is free.
fn cancel_wait(
  cell: &'static std::thread::LocalKey<RefCell<Turns>>,
  repository: RepositoryId,
  ticket: u64,
) {
  // Through `try_with`: a wait dropped in the thread's local storage teardown has no line left to leave.
  let woken = cell
    .try_with(|turns| {
      let mut turns = turns.try_borrow_mut().ok()?;
      let line = turns.lines.get_mut(&repository)?;
      line.waiting.retain(|(id, _)| *id != ticket);
      let woken = if line.held {
        None
      } else {
        line.waiting.front_mut().and_then(|(_, waker)| waker.take())
      };
      if !line.held && line.waiting.is_empty() {
        turns.lines.remove(&repository);
      }
      woken
    })
    .ok()
    .flatten();
  if let Some(waker) = woken {
    waker.wake();
  }
}

/// Releases `repository`'s line in `cell` and wakes the next FIFO waiter, if any.
fn release_line(cell: &'static std::thread::LocalKey<RefCell<Turns>>, repository: RepositoryId) {
  // Through `try_with`, as a dropped wait's: no line left to give the turn back to in the thread's teardown.
  let woken = cell
    .try_with(|turns| {
      let mut turns = turns.try_borrow_mut().ok()?;
      let line = turns.lines.get_mut(&repository)?;
      line.held = false;
      let woken = line.waiting.front_mut().and_then(|(_, waker)| waker.take());
      if line.waiting.is_empty() {
        turns.lines.remove(&repository);
      }
      woken
    })
    .ok()
    .flatten();
  if let Some(waker) = woken {
    waker.wake();
  }
}

/// Waits for `repository`'s turn on this shard (module docs): the turn, held until it is dropped or yielded holding.
pub(crate) fn take(repository: RepositoryId) -> Taking {
  Taking {
    repository,
    ticket: None,
  }
}

/// The wait for a repository's turn ([`take`]).
pub(crate) struct Taking {
  repository: RepositoryId,
  /// The ticket it waits under, once it has waited.
  ticket: Option<u64>,
}

impl Future for Taking {
  type Output = Turn;

  fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Turn> {
    match poll_line(&TURNS, self.repository, self.ticket, context.waker()) {
      Ok(()) => Poll::Ready(Turn {
        repository: Some(self.repository),
      }),
      Err(ticket) => {
        if ticket.is_none() {
          // The table was borrowed (never across an await): ask again next round.
          context.waker().wake_by_ref();
        }
        self.ticket = ticket;
        Poll::Pending
      }
    }
  }
}

impl Drop for Taking {
  /// A wait given up (its push dropped) leaves its line, waking the next if the turn is free.
  fn drop(&mut self) {
    if let Some(ticket) = self.ticket {
      cancel_wait(&TURNS, self.repository, ticket);
    }
  }
}

/// A repository's turn on this shard, held (module docs): given back as it drops, to the next push waiting.
pub(crate) struct Turn {
  /// The repository; none once given back.
  repository: Option<RepositoryId>,
}

impl Turn {
  /// Yields the shard after a step, keeping the turn through the round (module docs, "The rule"), then gives it back.
  pub(crate) async fn yield_holding(self) {
    slates_rt::futures::yield_now().await;
    drop(self);
  }
}

impl Drop for Turn {
  fn drop(&mut self) {
    if let Some(repository) = self.repository.take() {
      release_line(&TURNS, repository);
    }
  }
}

/// Waits for `repository`'s FIFO push intake slot on this shard: resolves synchronously on the first
/// poll when no other push to `repository` is currently intaking a pack body, and holds the slot
/// until [`IntakeGuard`] drops (when intake completes, pauses for a slow/overdue peer, runs a hook,
/// or disconnects).
pub(crate) fn take_intake(repository: RepositoryId) -> TakingIntake {
  TakingIntake {
    repository,
    ticket: None,
  }
}

/// The wait for a repository's push intake slot ([`take_intake`]).
pub(crate) struct TakingIntake {
  repository: RepositoryId,
  ticket: Option<u64>,
}

impl Future for TakingIntake {
  type Output = IntakeGuard;

  fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<IntakeGuard> {
    match poll_line(&INTAKE, self.repository, self.ticket, context.waker()) {
      Ok(()) => Poll::Ready(IntakeGuard {
        repository: Some(self.repository),
      }),
      Err(ticket) => {
        if ticket.is_none() {
          context.waker().wake_by_ref();
        }
        self.ticket = ticket;
        Poll::Pending
      }
    }
  }
}

impl Drop for TakingIntake {
  fn drop(&mut self) {
    if let Some(ticket) = self.ticket {
      cancel_wait(&INTAKE, self.repository, ticket);
    }
  }
}

/// Guard holding a repository's push intake slot on this shard ([`take_intake`]).
pub(crate) struct IntakeGuard {
  repository: Option<RepositoryId>,
}

impl Drop for IntakeGuard {
  fn drop(&mut self) {
    if let Some(repository) = self.repository.take() {
      release_line(&INTAKE, repository);
    }
  }
}

/// Waits for `repository`'s FIFO pack-generation slot on this shard: resolves synchronously on the
/// first poll when no other `command=fetch` pack stream is active for `repository`, and holds the
/// slot until [`FetchGuard`] drops (when the pack finishes, the peer becomes overdue, or the
/// connection disconnects).
pub(crate) fn take_fetch(repository: RepositoryId) -> TakingFetch {
  TakingFetch {
    repository,
    ticket: None,
  }
}

/// The wait for a repository's pack-generation slot ([`take_fetch`]).
pub(crate) struct TakingFetch {
  repository: RepositoryId,
  ticket: Option<u64>,
}

impl Future for TakingFetch {
  type Output = FetchGuard;

  fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<FetchGuard> {
    match poll_line(&FETCH, self.repository, self.ticket, context.waker()) {
      Ok(()) => Poll::Ready(FetchGuard {
        repository: Some(self.repository),
      }),
      Err(ticket) => {
        if ticket.is_none() {
          context.waker().wake_by_ref();
        }
        self.ticket = ticket;
        Poll::Pending
      }
    }
  }
}

impl Drop for TakingFetch {
  fn drop(&mut self) {
    if let Some(ticket) = self.ticket {
      cancel_wait(&FETCH, self.repository, ticket);
    }
  }
}

/// Guard holding a repository's pack-generation slot on this shard ([`take_fetch`]).
pub(crate) struct FetchGuard {
  repository: Option<RepositoryId>,
}

impl Drop for FetchGuard {
  fn drop(&mut self) {
    if let Some(repository) = self.repository.take() {
      release_line(&FETCH, repository);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::take;
  use slates_db::catalog::RepositoryId;
  use std::cell::RefCell;

  std::thread_local! {
    /// The repositories in the order their turns ran, on the test's shard.
    static LOG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
  }

  /// Shape: each push's turns.
  const TURNS: usize = 20;

  /// A-724 piece 3c (main's order: a small repository's wait stays within its share while a large push runs). Do: on one
  /// shard, three pushes of one repository and one of a small repository, each taking its turns as a push's answer does
  /// (the turn, a step, the yield holding it). Expect: between two turns of the small repository's push, at most one turn
  /// of the other repository while it has work, so its share of the rounds is a half, not a quarter; and every push
  /// takes all its turns.
  #[test]
  fn a_repositorys_pushes_share_one_turn_a_round_of_the_shard() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |_| {
      let task = slates_rt::futures::spawn(async move {
        let mut pushes = Vec::new();
        for repository in [1u8, 1, 1, 2] {
          let push = slates_rt::futures::spawn(async move {
            for _ in 0..TURNS {
              let turn = take(RepositoryId {
                bytes: [repository; 16],
              })
              .await;
              LOG.with(|log| log.borrow_mut().push(repository));
              turn.yield_holding().await;
            }
          })
          .unwrap();
          pushes.push(push);
        }
        for push in pushes {
          let _ = slates_rt::futures::join(push).await;
        }
        let _ = sent.send(LOG.with(|log| log.borrow().clone()));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let log = answered.recv().unwrap();
    assert_eq!(log.iter().filter(|r| **r == 1).count(), 3 * TURNS);
    assert_eq!(log.iter().filter(|r| **r == 2).count(), TURNS);
    let small: Vec<usize> = log
      .iter()
      .enumerate()
      .filter(|(_, r)| **r == 2)
      .map(|(at, _)| at)
      .collect();
    let most_between = small
      .windows(2)
      .map(|pair| pair[1] - pair[0] - 1)
      .max()
      .unwrap_or(0);
    assert!(
      most_between <= 1,
      "the large repository took {most_between} turns between two of the small one's: {log:?}"
    );
  }

  /// Do: spawn three push intakes for the same repository and one for a different repository on
  /// one shard, yielding once while holding each intake guard. Expect: the three intakes on the
  /// same repository complete strictly in FIFO order without overlapping (each starts only after
  /// the prior drops), while the other repository proceeds concurrently.
  #[test]
  fn a_repositorys_push_intakes_serialize_fifo_without_blocking_other_repos() {
    std::thread_local! {
      static INTAKE_LOG: RefCell<Vec<(u8, &'static str)>> = const { RefCell::new(Vec::new()) };
    }
    let daemon = crate::daemon::audit_daemon_of(1);
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |_| {
      let task = slates_rt::futures::spawn(async move {
        INTAKE_LOG.with(|log| log.borrow_mut().clear());
        let repo_a = RepositoryId { bytes: [11; 16] };
        let repo_b = RepositoryId { bytes: [22; 16] };
        let mut handles = Vec::new();
        for (id, repo) in [(1_u8, repo_a), (2, repo_a), (3, repo_a), (4, repo_b)] {
          let handle = slates_rt::futures::spawn(async move {
            let guard = super::take_intake(repo).await;
            INTAKE_LOG.with(|log| log.borrow_mut().push((id, "enter")));
            slates_rt::futures::yield_now().await;
            INTAKE_LOG.with(|log| log.borrow_mut().push((id, "leave")));
            drop(guard);
          })
          .unwrap();
          handles.push(handle);
        }
        for handle in handles {
          let _ = slates_rt::futures::join(handle).await;
        }
        let _ = sent.send(INTAKE_LOG.with(|log| log.borrow().clone()));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let events = answered.recv().unwrap();
    let repo_a_events: Vec<(u8, &'static str)> =
      events.iter().copied().filter(|(id, _)| *id != 4).collect();
    assert_eq!(
      repo_a_events,
      vec![
        (1, "enter"),
        (1, "leave"),
        (2, "enter"),
        (2, "leave"),
        (3, "enter"),
        (3, "leave"),
      ]
    );
    let b_enter = events
      .iter()
      .position(|e| *e == (4, "enter"))
      .expect("repo B entered");
    let a3_enter = events
      .iter()
      .position(|e| *e == (3, "enter"))
      .expect("repo A push 3 entered");
    assert!(
      b_enter < a3_enter,
      "repo B was blocked behind repo A's queue: {events:?}"
    );
  }
}
