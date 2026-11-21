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
//! **The weight, learned from use** (main's order of 2026-11-20: never one turn a repository as a constant). A
//! repository holds as many turns at once as it has distinct principals among its pushes holding or waiting on the shard
//! now: each agent pushing to it is one flow of the round, so one agent's three pushes share one turn and three agents'
//! pushes take three, whatever repository they share. The next turn goes to the first waiting push whose principal holds
//! none, else to the first waiting push. So a round of the shard gives every agent with work one turn, the share Ada's
//! goal names ("balancing by learned agent and usage patterns"), learned from what is under way in each deployment.
//!
//! **Spare turns.** A turn no other repository on the shard holds or waits for is spare: a waiting push takes it whatever its
//! repository's weight, so a repository alone on its shard runs as many pushes at once as it has (run 31, 2026-11-20:
//! the busiest repository's three pushes of one agent shared one turn and its shard sat busy 23.2 s of the 41.3 s it
//! waited; its shard-mates had taken 4.5 s of it). The weight binds only while another repository holds or waits for a turn.
//!
//! **Bounded.** One entry a repository with a push holding or waiting for its turn, each waiter one ticket, its principal
//! and its waker, so bounded by the pushes under way on the shard; an entry with no holder and no waiter is removed.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_db::catalog::{Principal, RepositoryId};

/// One repository's turns on this shard: the turns held, by principal, and the pushes waiting, first first.
#[derive(Debug, Default)]
struct Line {
  /// The turns held, by the principal holding them.
  holding: BTreeMap<Principal, usize>,
  /// The waiting pushes: each its ticket, its principal and the waker to wake when a turn may be its.
  waiting: VecDeque<(u64, Principal, Option<Waker>)>,
}

impl Line {
  /// The turns the repository may hold at once (module docs, "The weight"): its distinct principals holding or waiting,
  /// at least one.
  fn weight(&self) -> usize {
    let mut principals: Vec<&Principal> = self.holding.keys().collect();
    for (_, principal, _) in &self.waiting {
      if !principals.contains(&principal) {
        principals.push(principal);
      }
    }
    principals.len().max(1)
  }

  /// The ticket the next free turn goes to (module docs): the first waiting push whose principal holds none, else the
  /// first.
  fn next(&self) -> Option<u64> {
    self
      .waiting
      .iter()
      .find(|(_, principal, _)| !self.holding.contains_key(principal))
      .or_else(|| self.waiting.front())
      .map(|(ticket, _, _)| *ticket)
  }

  /// The turns held now.
  fn held(&self) -> usize {
    self.holding.values().sum()
  }

  /// Every waiter's waker, taken: each looks again whether a turn is its.
  fn wake_all(&mut self) -> Vec<Waker> {
    self
      .waiting
      .iter_mut()
      .filter_map(|(_, _, waker)| waker.take())
      .collect()
  }
}

/// Every repository's turns on this shard (module docs).
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

/// Waits for one of `repository`'s turns on this shard for a push of `principal`'s (module docs): the turn, held until it
/// is dropped or yielded holding.
pub(crate) fn take(repository: RepositoryId, principal: Principal) -> Taking {
  Taking {
    repository,
    principal: Some(principal),
    ticket: None,
  }
}

/// The wait for one of a repository's turns ([`take`]).
pub(crate) struct Taking {
  repository: RepositoryId,
  /// The principal the push is of; taken into the turn once granted.
  principal: Option<Principal>,
  /// The ticket it waits under, once it has waited.
  ticket: Option<u64>,
}

impl Future for Taking {
  type Output = Turn;

  fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Turn> {
    let repository = self.repository;
    let Some(principal) = self.principal.clone() else {
      return Poll::Pending;
    };
    let ticket = self.ticket;
    let decided = TURNS.with(|turns| {
      let Ok(mut turns) = turns.try_borrow_mut() else {
        return None;
      };
      let Turns { lines, next } = &mut *turns;
      let line = lines.entry(repository).or_default();
      let ticket = ticket.unwrap_or_else(|| {
        let id = *next;
        *next = next.wrapping_add(1);
        line.waiting.push_back((id, principal.clone(), None));
        id
      });
      let spare = lines.keys().all(|other| *other == repository);
      let Some(line) = lines.get_mut(&repository) else {
        return None;
      };
      if (spare || line.held() < line.weight()) && line.next() == Some(ticket) {
        line.waiting.retain(|(waiting, _, _)| *waiting != ticket);
        *line.holding.entry(principal.clone()).or_default() += 1;
        // The next waiter may take a turn too when one is free or spare: wake it to look.
        let woken = if spare || line.held() < line.weight() {
          line.next().and_then(|next| {
            line
              .waiting
              .iter_mut()
              .find(|(waiting, _, _)| *waiting == next)
              .and_then(|(_, _, waker)| waker.take())
          })
        } else {
          None
        };
        return Some(Ok(woken));
      }
      if let Some((_, _, slot)) = line
        .waiting
        .iter_mut()
        .find(|(waiting, _, _)| *waiting == ticket)
      {
        *slot = Some(context.waker().clone());
      }
      Some(Err(ticket))
    });
    match decided {
      Some(Ok(woken)) => {
        if let Some(waker) = woken {
          waker.wake();
        }
        self.ticket = None;
        Poll::Ready(Turn {
          held: self
            .principal
            .take()
            .map(|principal| (repository, principal)),
        })
      }
      Some(Err(ticket)) => {
        self.ticket = Some(ticket);
        Poll::Pending
      }
      None => {
        // The table was borrowed (never across an await): ask again next round.
        context.waker().wake_by_ref();
        Poll::Pending
      }
    }
  }
}

impl Drop for Taking {
  /// A wait given up (its push dropped) leaves its line, the others woken to look again.
  fn drop(&mut self) {
    let Some(ticket) = self.ticket else {
      return;
    };
    let repository = self.repository;
    let woken = TURNS.with(|turns| {
      let Ok(mut turns) = turns.try_borrow_mut() else {
        return Vec::new();
      };
      let Some(line) = turns.lines.get_mut(&repository) else {
        return Vec::new();
      };
      line.waiting.retain(|(waiting, _, _)| *waiting != ticket);
      let woken = line.wake_all();
      if line.holding.is_empty() && line.waiting.is_empty() {
        turns.lines.remove(&repository);
      }
      woken
    });
    for waker in woken {
      waker.wake();
    }
  }
}

/// One of a repository's turns on this shard, held (module docs): given back as it drops.
pub(crate) struct Turn {
  /// The repository and the principal holding it; none once given back.
  held: Option<(RepositoryId, Principal)>,
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
    let Some((repository, principal)) = self.held.take() else {
      return;
    };
    let woken = TURNS.with(|turns| {
      let Ok(mut turns) = turns.try_borrow_mut() else {
        return Vec::new();
      };
      let Some(line) = turns.lines.get_mut(&repository) else {
        return Vec::new();
      };
      if let Some(count) = line.holding.get_mut(&principal) {
        *count = count.saturating_sub(1);
        if *count == 0 {
          line.holding.remove(&principal);
        }
      }
      let woken = line.wake_all();
      if line.holding.is_empty() && line.waiting.is_empty() {
        turns.lines.remove(&repository);
      }
      woken
    });
    for waker in woken {
      waker.wake();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::take;
  use slates_db::catalog::{Principal, RepositoryId};
  use std::cell::RefCell;

  std::thread_local! {
    /// The repositories in the order their turns ran, on the test's shard.
    static LOG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// The pushes holding a turn now, the most at once, and the turns taken, on the test's shard.
    static HOLDING: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) };
  }

  /// Shape: each push's turns.
  const TURNS: usize = 20;

  /// The principal of agent `n`.
  fn agent(n: u32) -> Principal {
    Principal::Uid {
      host: slates_db::HostId(1),
      uid: n,
    }
  }

  /// The repositories in the order their turns ran when pushes of `(repository, agent)` each take [`TURNS`] turns on one
  /// shard, as a push's answer takes them (the turn, a step, the yield holding it).
  fn rounds(pushes: Vec<(u8, u32)>) -> Vec<u8> {
    let daemon = crate::daemon::audit_daemon_of(1);
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |_| {
      let task = slates_rt::futures::spawn(async move {
        let mut tasks = Vec::new();
        for (repository, who) in pushes {
          let push = slates_rt::futures::spawn(async move {
            for _ in 0..TURNS {
              let turn = take(
                RepositoryId {
                  bytes: [repository; 16],
                },
                agent(who),
              )
              .await;
              LOG.with(|log| log.borrow_mut().push(repository));
              turn.yield_holding().await;
            }
          })
          .unwrap();
          tasks.push(push);
        }
        for push in tasks {
          let _ = slates_rt::futures::join(push).await;
        }
        let _ = sent.send(LOG.with(|log| std::mem::take(&mut *log.borrow_mut())));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    answered.recv().unwrap()
  }

  /// A-724 piece 3c, spare turns (run 31, 2026-11-20). Do: on one shard, three pushes of one repository by one agent,
  /// nothing else holding or waiting, each counting the pushes holding a turn as it takes one. Expect: all three hold turns at once
  /// (a turn no other repository holds or waits for is spare), and every push takes all its turns.
  #[test]
  fn a_repository_alone_on_its_shard_takes_a_turn_for_each_push() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |_| {
      let task = slates_rt::futures::spawn(async move {
        let mut tasks = Vec::new();
        for _ in 0..3 {
          let push = slates_rt::futures::spawn(async move {
            for _ in 0..TURNS {
              let turn = take(RepositoryId { bytes: [1; 16] }, agent(1)).await;
              HOLDING.with(|holding| {
                let (now, most, taken) = holding.get();
                holding.set((now + 1, most.max(now + 1), taken + 1));
              });
              slates_rt::futures::yield_now().await;
              HOLDING.with(|holding| {
                let (now, most, taken) = holding.get();
                holding.set((now - 1, most, taken));
              });
              drop(turn);
            }
          })
          .unwrap();
          tasks.push(push);
        }
        for push in tasks {
          let _ = slates_rt::futures::join(push).await;
        }
        let _ = sent.send(HOLDING.with(std::cell::Cell::get));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let (_, most, taken) = answered.recv().unwrap();
    assert_eq!(taken, 3 * TURNS);
    assert_eq!(most, 3, "the pushes did not hold turns at once");
  }

  /// A-724 piece 3c, the weight learned from use (main's order of 2026-11-20). Do: on one shard, three pushes of one
  /// repository by three agents, and one push of another repository by a fourth, each taking its turns. Expect: between
  /// two turns of the lone push, three of the other repository's while it has work (one an agent), never more; every push
  /// takes all its turns.
  #[test]
  fn a_repositorys_pushes_by_several_agents_take_a_turn_an_agent_each_round() {
    let log = rounds(vec![(1, 1), (1, 2), (1, 3), (2, 4)]);
    assert_eq!(log.iter().filter(|r| **r == 1).count(), 3 * TURNS);
    assert_eq!(log.iter().filter(|r| **r == 2).count(), TURNS);
    let lone: Vec<usize> = log
      .iter()
      .enumerate()
      .filter(|(_, r)| **r == 2)
      .map(|(at, _)| at)
      .collect();
    let between: Vec<usize> = lone.windows(2).map(|pair| pair[1] - pair[0] - 1).collect();
    assert!(
      between.iter().all(|turns| *turns <= 3),
      "more than one turn an agent a round: {log:?}"
    );
    assert!(
      between.iter().take(TURNS / 2).all(|turns| *turns == 3),
      "an agent's turn missed: {log:?}"
    );
  }

  /// A-724 piece 3c (main's order: a small repository's wait stays within its share while a large push runs). Do: on one
  /// shard, three pushes of one repository and one of a small repository, each taking its turns as a push's answer does
  /// (the turn, a step, the yield holding it). Expect: between two turns of the small repository's push, at most one turn
  /// of the other repository while it has work, so its share of the rounds is a half, not a quarter; and every push
  /// takes all its turns.
  #[test]
  fn a_repositorys_pushes_share_one_turn_a_round_of_the_shard() {
    let log = rounds(vec![(1, 1), (1, 1), (1, 1), (2, 2)]);
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
