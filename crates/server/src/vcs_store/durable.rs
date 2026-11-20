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
#[cfg_attr(not(test), allow(dead_code))] // Owed callers: w-svn's landing (A-727 slice b).
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
#[cfg_attr(not(test), allow(dead_code))] // Owed callers: w-svn's landing, git's version-3 records.
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
}
