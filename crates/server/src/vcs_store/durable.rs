//! The content plane's durability barrier (A-727 slice b; main's order of 2026-11-20): a record that names content by
//! identity commits only after that content is durable in its shard's image.
//!
//! **Why.** A shard's content windows are durable only once a publication of its image that holds them commits (A-727
//! a3: windows are held pieces of the shard image). An entry is durable once appended. So an entry appended right
//! after its content was put can outlive that content: a crash between the two leaves, after recovery, an entry naming
//! content no image holds. svn's intake (w-svn), git's version-3 records and the region homes all name content by
//! identity, so each waits here before its append.
//!
//! **The shape** (agreed with w-svn, inbox/w-svn-from-w-balance.md). On the owner shard, after the landing's last put,
//! [`content_ticket`]: the first image generation of the shard that will hold every chunk written so far. Then, from the
//! landing's task, [`content_durable`]: resolves once a publication capturing that generation has committed
//! (`crate::content_holder::held_committed` drives it, [`super::homes::publication_committed`]). The first wait that
//! finds its generation unpublished publishes the shard itself (`crate::verbs::publish_shard`, a delta made and
//! committed in one turn), since nothing publishes on a schedule; every chunk written before rides that publication, so
//! the other waits find it published: one barrier per publication, group commit. A refusal is typed
//! ([`BarrierRefused`]): the landing gives its content back and refuses its commit.
//!
//! **One barrier.** The tickets are the region homes' (`super::homes::HomeTicket`): a window homed for the region and a
//! content put by a landing wait on the same publications.

use crate::state::ShardState;

/// Where and when content written on a shard is durable: the shard, and the first generation of its image that holds it.
pub(crate) type ContentTicket = super::homes::HomeTicket;

/// Why content was not answered durable (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BarrierRefused {
  /// The shard's publication capturing the ticket's generation was refused (its image lost, or no room for it).
  NotPublished,
  /// No answer within the wait: the shard is shutting down, gone, or no publication committed in time.
  Unanswered,
}

/// The ticket of every chunk written on `state`'s shard so far (module docs): taken after a landing's last put.
pub(crate) fn content_ticket(state: &ShardState) -> ContentTicket {
  super::homes::ticket_here(state.shard)
}

/// Resolves once the content of `ticket` is durable in its shard's image (module docs), asked from shard `origin`, in
/// the shard's own time up to `budget_ns`.
pub(crate) async fn content_durable(
  origin: u16,
  ticket: ContentTicket,
  budget_ns: u64,
) -> Result<(), BarrierRefused> {
  match super::homes::published_within(origin, ticket, budget_ns).await {
    Some(true) => Ok(()),
    Some(false) => Err(BarrierRefused::NotPublished),
    None => Err(BarrierRefused::Unanswered),
  }
}

#[cfg(test)]
mod tests {
  /// A-727 slice b, the barrier's own terms. Do: on a daemon of two shards, put content on a shard and take its ticket
  /// after the put, then wait on it from the other shard; take a ticket after that and wait again. Expect: durable both
  /// times, the shard's image of each ticket's generation published once answered (the first wait publishing it itself,
  /// nothing else having published), and the second ticket past the first: content written after a publication waits for
  /// the next.
  #[test]
  fn content_is_answered_durable_once_its_shards_image_is_published() {
    use crate::vcs_store::{content_durable, content_ticket};
    let daemon = crate::daemon::audit_daemon_of(2);
    let (sent, got) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |state| {
      let asker = state.shard;
      let owner = state
        .shards
        .iter()
        .copied()
        .find(|shard| *shard != asker)
        .unwrap_or(asker);
      let task = slates_rt::futures::spawn(async move {
        let budget = crate::daemon::LIVENESS_BUDGET_NS;
        let mut outcome: Vec<String> = Vec::new();
        let mut tickets = Vec::new();
        for text in [&b"a text"[..], &b"another text"[..]] {
          let ticket = crate::xshard::call_in_shard_time(
            asker,
            owner,
            move |state| {
              let _ = crate::vcs_store::put_names(state, text);
              content_ticket(state)
            },
            budget,
          )
          .await;
          let Some(ticket) = ticket else {
            outcome.push("no ticket".to_owned());
            continue;
          };
          if let Err(refused) = content_durable(asker, ticket, budget).await {
            outcome.push(format!("not durable: {refused:?}"));
          }
          let published = crate::xshard::call_in_shard_time(
            asker,
            owner,
            move |_| crate::vcs_store::homes::published_here(ticket.generation),
            budget,
          )
          .await;
          if published != Some(true) {
            outcome.push(format!(
              "answered durable with generation {} unpublished",
              ticket.generation
            ));
          }
          tickets.push(ticket.generation);
        }
        if tickets.windows(2).any(|pair| pair[1] <= pair[0]) {
          outcome.push(format!(
            "a ticket after a publication not past it: {tickets:?}"
          ));
        }
        let _ = sent.send(outcome);
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let outcome = got.recv().unwrap();
    assert!(outcome.is_empty(), "{outcome:?}");
  }

  /// Shape: the objects kept before the ticket's wait, so its publication finds more pages dirty than one encoder turn
  /// re-encodes at the test's small page and leaves a catch-up owed.
  const KEPT_BEFORE: usize = 2_000;
  /// Shape: how many sweep intervals the test waits for the ticket before it calls the wait lost.
  const SWEEPS_WAITED: u32 = 6;

  /// A-870 piece 2, a refused catch-up (main 01a20182's ruling of 2026-11-23: no swallowed error, banned item 9). Do: on
  /// one shard whose paged image's pages are small, keep [`KEPT_BEFORE`] objects for the region, so the next publication
  /// finds pages dirty and owes a catch-up, and refuse that catch-up's first publication; keep one more, take its ticket
  /// and wait for it. Expect: durable after all, the refusal counted and the publication begun again by the shard's sweep
  /// (counted). Failing first: the wait never answered, the publication owed with no catch-up left to make it.
  #[test]
  fn a_tickets_wait_is_answered_after_its_catch_ups_publication_was_refused() {
    use crate::vcs_store::{content_durable, content_ticket};
    let daemon = crate::daemon::audit_daemon();
    let (sent, got) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(4096));
      crate::content_holder::CATCH_UP_REFUSALS.with(|refusals| refusals.set(1));
      let origin = state.shard;
      for at in 0..KEPT_BEFORE {
        let bytes = format!("an object kept before the wait, number {at}").into_bytes();
        let _ = crate::vcs_store::homes::keep_for_region(
          state,
          &bytes,
          &[crate::vcs_store::homes::TEST_OWNER],
        );
      }
      let _ = crate::vcs_store::homes::keep_for_region(
        state,
        b"the object a homing waits on",
        &[crate::vcs_store::homes::TEST_OWNER],
      );
      let ticket = content_ticket(state);
      let waiter = slates_rt::futures::spawn(async move {
        let outcome = content_durable(origin, ticket, crate::daemon::LIVENESS_BUDGET_NS).await;
        let counts = crate::state::with_state(|state| {
          let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
          [
            counted(crate::content_holder::IMAGE_CATCH_UP_PUBLISH_REFUSED),
            counted(crate::content_holder::IMAGE_CATCH_UP_RETRIED),
          ]
        });
        let _ = sent.send((outcome, counts));
      })
      .unwrap();
      slates_rt::futures::detach(waiter).unwrap();
    });
    let patience =
      std::time::Duration::from_nanos(crate::idle_sweep::interval_ns()) * SWEEPS_WAITED;
    let answered = got.recv_timeout(patience);
    daemon.stop();
    let (outcome, counts) = answered.expect("the ticket's wait answered within a few sweeps");
    assert_eq!(outcome, Ok(()), "durable after all");
    let [refused, retried] = counts.expect("the shard's state");
    assert_eq!(
      refused, 1,
      "the catch-up's publication refused once, counted"
    );
    assert!(
      retried >= 1,
      "the sweep began the publication again, counted"
    );
  }

  /// Shape: the objects kept for the region before the wait begins (main 01a20182's ruling (c) of 2026-11-23 20:26: the
  /// backlog r8's dirtier built, 256 turns of 256), each a window and a region hold: two families' entries, the wait's
  /// cut a large one. The test asserts it reached them, so a smaller backlog cannot pass.
  const BACKLOG: u64 = 256 * 256;
  /// Shape: the most rounds the dirtying task runs before it stops on its own: past any wait the test makes.
  const DIRTYING_ROUNDS: usize = 1_000_000;

  /// What the dirtying task and the wait share (test harness, one shard's thread).
  #[derive(Default)]
  struct Dirtying {
    /// Objects kept so far.
    kept: std::cell::Cell<u64>,
    /// Whether the wait has begun.
    waiting: std::cell::Cell<bool>,
    /// Whether the wait has ended: the dirtier stops then.
    done: std::cell::Cell<bool>,
    /// Rounds the dirtier ran while the wait was on.
    rounds_in_wait: std::cell::Cell<u64>,
    /// The fewest objects one of those rounds kept.
    fewest_in_wait: std::cell::Cell<u64>,
  }

  /// A-870 piece 2 against A-727, F2 (main 01a20182's rulings of 2026-11-23: the git mirror stall's hypothesis, a homing
  /// starved by an encoder that never catches up; (c) at 20:26, the dirtier shaped as a landing in the field). Do: on
  /// one shard whose paged image's pages are small, run a task that keeps fresh objects for the region a round at a time,
  /// each round keeping objects until the shard's own step quantum's half is spent (`DaemonConfig::step_quantum_ns`, the
  /// slice a landing yields at, read from the shard), then yielding; once it has kept [`BACKLOG`], keep one more object,
  /// take its ticket and wait for it to be durable within the liveness budget, the dirtier keeping on meanwhile. Expect:
  /// durable within the budget, whatever the shard dirties after the wait's cut; the backlog reached; the dirtier keeping
  /// objects in every round of the wait (it never went idle); publications skipped meanwhile (the image fell behind).
  /// Failing first: the wait refused, no publication ever carrying the encoder's cut while the shard kept dirtying pages
  /// (r8, before F2); red again with F2 but the catch-up's byte-slice pacing removed (a turn a yield), and with the capped
  /// read counting only the entries it keeps.
  #[test]
  fn a_tickets_wait_is_answered_while_the_shard_keeps_dirtying_its_image() {
    use crate::vcs_store::{content_durable, content_ticket};
    let daemon = crate::daemon::audit_daemon();
    let (sent, got) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |state| {
      crate::content_holder::TEST_PAGE_BYTES.with(|bytes| bytes.set(4096));
      let origin = state.shard;
      // D-8 test harness: the dirtying task and the wait are the two owners, on this shard's one thread.
      #[allow(clippy::disallowed_types)]
      let shared = std::rc::Rc::new(Dirtying::default());
      #[allow(clippy::disallowed_types)]
      let dirtying = std::rc::Rc::clone(&shared);
      let dirtier = slates_rt::futures::spawn(async move {
        let mut number = 0u64;
        for _ in 0..DIRTYING_ROUNDS {
          if dirtying.done.get() {
            break;
          }
          let kept = crate::state::with_state(|state| {
            let slice = (state.config.step_quantum_ns() / 2).max(1);
            let began = slates_rt::futures::now_ns();
            let mut kept = 0u64;
            loop {
              let bytes = format!("an object a large push lands, number {number}").into_bytes();
              number += 1;
              let _ = crate::vcs_store::homes::keep_for_region(
                state,
                &bytes,
                &[crate::vcs_store::homes::TEST_OWNER],
              );
              kept += 1;
              if slates_rt::futures::now_ns().saturating_sub(began) >= slice {
                return kept;
              }
            }
          })
          .unwrap_or(0);
          dirtying.kept.set(dirtying.kept.get() + kept);
          if dirtying.waiting.get() {
            let rounds = dirtying.rounds_in_wait.get();
            let fewest = if rounds == 0 {
              kept
            } else {
              dirtying.fewest_in_wait.get().min(kept)
            };
            dirtying.fewest_in_wait.set(fewest);
            dirtying.rounds_in_wait.set(rounds + 1);
          }
          slates_rt::futures::yield_now().await;
        }
      })
      .unwrap();
      slates_rt::futures::detach(dirtier).unwrap();
      let waiter = slates_rt::futures::spawn(async move {
        // The backlog first, so the wait begins with the image far behind.
        while shared.kept.get() < BACKLOG {
          slates_rt::futures::yield_now().await;
        }
        let backlog = shared.kept.get();
        let ticket = crate::state::with_state(|state| {
          let _ = crate::vcs_store::homes::keep_for_region(
            state,
            b"the object a homing waits on",
            &[crate::vcs_store::homes::TEST_OWNER],
          );
          content_ticket(state)
        });
        shared.waiting.set(true);
        let outcome = match ticket {
          Some(ticket) => content_durable(origin, ticket, crate::daemon::LIVENESS_BUDGET_NS).await,
          None => Err(super::BarrierRefused::Unanswered),
        };
        shared.done.set(true);
        let counts = crate::state::with_state(|state| {
          let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
          (
            counted(crate::content_holder::IMAGE_SKIPPED),
            counted(crate::content_holder::IMAGE_TURNS),
            counted(crate::content_holder::IMAGE_PAGES_SPLIT),
            counted(crate::content_holder::IMAGE_CUTS),
            state.image_encoder.progress(),
          )
        });
        let dirtied = (
          backlog,
          shared.rounds_in_wait.get(),
          shared.fewest_in_wait.get(),
        );
        let _ = sent.send((outcome, counts, dirtied));
      })
      .unwrap();
      slates_rt::futures::detach(waiter).unwrap();
    });
    let (outcome, counts, (backlog, rounds, fewest)) = got.recv().unwrap();
    daemon.stop();
    assert_eq!(
      outcome,
      Ok(()),
      "the ticket answered durable within the budget (publications skipped, turns, pages split, cuts and the encoder \
       meanwhile: {counts:?}; backlog {backlog}, the dirtier's rounds in the wait {rounds}, the fewest kept in one \
       {fewest})"
    );
    assert!(
      backlog >= BACKLOG,
      "the backlog reached: {backlog} of {BACKLOG}"
    );
    assert!(
      rounds > 0 && fewest > 0,
      "the dirtier kept objects in every round of the wait: {rounds} rounds, the fewest {fewest}"
    );
    // Non-vacuous: the image fell behind while the wait ran, or the wait proves nothing about a starved encoder.
    assert!(
      counts.is_some_and(|(skipped, ..)| skipped > 0),
      "the image fell behind meanwhile: {counts:?}"
    );
  }
}
