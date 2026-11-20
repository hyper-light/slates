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
    let repository = self.repository;
    let ticket = self.ticket;
    let taken = TURNS.with(|turns| {
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
        *slot = Some(context.waker().clone());
      }
      Err(Some(ticket))
    });
    match taken {
      Ok(()) => Poll::Ready(Turn {
        repository: Some(repository),
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
    let Some(ticket) = self.ticket else {
      return;
    };
    let repository = self.repository;
    let woken = TURNS.with(|turns| {
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
    });
    if let Some(waker) = woken {
      waker.wake();
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
    let Some(repository) = self.repository.take() else {
      return;
    };
    let woken = TURNS.with(|turns| {
      let mut turns = turns.try_borrow_mut().ok()?;
      let line = turns.lines.get_mut(&repository)?;
      line.held = false;
      let woken = line.waiting.front_mut().and_then(|(_, waker)| waker.take());
      if line.waiting.is_empty() {
        turns.lines.remove(&repository);
      }
      woken
    });
    if let Some(waker) = woken {
      waker.wake();
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
}
