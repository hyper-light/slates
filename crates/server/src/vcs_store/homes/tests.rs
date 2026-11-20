//! A-724 piece 5's tests (§4.17.5 "Giant repositories"): content homed by its identity across a node's shards.

use std::time::Duration;

use slates_db::register::rendezvous_ranked_by_key;
use slates_vcs::ContentId;

use super::{HOMED, HomeRefused, LET_GO, home, home_of, let_go, read};

/// Shape: the windows the spread test homes: enough that every one of four shards is some window's home with
/// overwhelming probability (each misses all with probability (3/4)^64, about 1e-8).
const WINDOWS: usize = 64;

/// Shape: how long the test thread waits for the shards' answers: a liveness budget.
const ANSWER_WAIT_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;

/// The `index`th test window: distinct bytes, well within a window.
fn window(index: usize) -> Vec<u8> {
  format!("window {index} of a monorepo's cold content\n").into_bytes()
}

/// A-724 piece 5. Do: rank four shards for many identities. Expect: each identity's home the head of the one rendezvous
/// ranking every placement uses; every shard some identity's home; none for no shards.
#[test]
fn an_identitys_home_is_the_head_of_the_one_rendezvous_ranking_and_homes_spread() {
  let shards = [3u16, 5, 8, 13];
  let mut used = std::collections::BTreeSet::new();
  for index in 0..WINDOWS {
    let identity = ContentId::of(&window(index));
    let home = home_of(&shards, &identity).unwrap();
    let ranked = rendezvous_ranked_by_key(&shards.map(u64::from), &identity.0);
    assert_eq!(u64::from(home), ranked[0]);
    used.insert(home);
  }
  assert_eq!(used.len(), shards.len(), "every shard homes something");
  assert_eq!(home_of(&[], &ContentId::of(b"x")), None);
}

/// A-724 piece 5. Do: on a four-shard daemon, from the first shard, home many windows (one twice), read each back,
/// let each go, and home bytes past a window. Expect: each window kept on its home and only there, the homes spread
/// over the shards; each read back whole from its home; a window homed twice held twice, kept until its second
/// let-go; every window freed after its last; past a window refused typed.
#[test]
fn windows_are_homed_read_and_let_go_at_their_identitys_shard() {
  let daemon = crate::daemon::audit_daemon_of(4);
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |state| {
    let origin = state.shard;
    let shards = state.shards.clone();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let mut outcome: Vec<String> = Vec::new();
      let mut homes = std::collections::BTreeSet::new();
      for index in 0..WINDOWS {
        let bytes = window(index);
        let homed = home(origin, &shards, bytes.clone(), budget).await;
        let Ok((identity, _)) = homed else {
          outcome.push(format!("window {index} not homed: {homed:?}"));
          continue;
        };
        homes.insert(home_of(&shards, &identity));
        if read(origin, &shards, identity, budget).await != Some(bytes) {
          outcome.push(format!("window {index} not read back"));
        }
      }
      if homes.len() < 2 {
        outcome.push(format!("homes did not spread: {homes:?}"));
      }
      let twice = window(0);
      let identity = ContentId::of(&twice);
      let _ = home(origin, &shards, twice.clone(), budget).await;
      for index in 0..WINDOWS {
        let identity = ContentId::of(&window(index));
        if let_go(origin, &shards, identity, budget).await.is_err() {
          outcome.push(format!("window {index} not let go"));
        }
      }
      if read(origin, &shards, identity, budget).await != Some(twice) {
        outcome.push("a window homed twice freed at its first let-go".to_owned());
      }
      if let_go(origin, &shards, identity, budget).await.is_err() {
        outcome.push("a window homed twice not let go a second time".to_owned());
      }
      for index in 0..WINDOWS {
        if read(origin, &shards, ContentId::of(&window(index)), budget)
          .await
          .is_some()
        {
          outcome.push(format!("window {index} kept after its last let-go"));
        }
      }
      let past =
        vec![
          7u8;
          crate::state::with_state(|state| state.store.content.chunk_bytes()).unwrap_or(0) + 1
        ];
      let refused = home(origin, &shards, past, budget).await;
      if refused != Err(HomeRefused::PastWindow) {
        outcome.push(format!("bytes past a window were not refused: {refused:?}"));
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv_timeout(Duration::from_nanos(ANSWER_WAIT_NS));
  let homed: u64 = (0..4)
    .map(|partition| {
      crate::daemon::observe_at(&daemon, partition, |state| {
        (
          state.refusals.get(HOMED).copied().unwrap_or(0),
          state.refusals.get(LET_GO).copied().unwrap_or(0),
        )
      })
    })
    .map(|(homed, let_go)| {
      assert_eq!(homed, let_go, "each shard let go what it homed");
      homed
    })
    .sum();
  daemon.stop();
  let outcome = outcome.expect("the shards answered");
  assert!(outcome.is_empty(), "{outcome:#?}");
  assert_eq!(
    homed,
    u64::try_from(WINDOWS + 1).unwrap(),
    "every window kept at a home, one twice"
  );
}

/// A-724 piece 5 with A-727's barrier (main's ruling: durable at its homes before any record names it). Do: on a
/// two-shard daemon, from the first shard home a window on the other, wait for its ticket while the home has published
/// nothing, then have the home begin and publish the image of the ticket's generation. Expect: the ticket names the
/// home and the first image after those begun; the wait still waiting until the publication and answering true after
/// it; a ticket of an older generation answering at once; and a wait whose image never comes answering false within its
/// budget.
#[test]
fn a_homed_windows_ticket_is_published_only_once_its_homes_image_is() {
  use super::{HomeTicket, image_begun, image_published, published};
  let daemon = crate::daemon::audit_daemon_of(2);
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |state| {
    let origin = state.shard;
    let other = state
      .shards
      .iter()
      .copied()
      .find(|shard| *shard != origin)
      .unwrap();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let mut outcome: Vec<String> = Vec::new();
      // Find a window homed on the other shard.
      let shards = [origin, other];
      let bytes = (0..)
        .map(window)
        .find(|bytes| home_of(&shards, &ContentId::of(bytes)) == Some(other))
        .unwrap();
      let (_, ticket) = home(origin, &shards, bytes, budget).await.unwrap();
      if ticket.shard != other {
        outcome.push(format!(
          "the ticket names {} not the home {other}",
          ticket.shard
        ));
      }
      // The home begins an image before the ticket's generation and publishes it: the ticket is not yet durable.
      let generation = ticket.generation;
      let _ = crate::xshard::call_in_shard_time(
        origin,
        other,
        move |_| {
          image_begun(generation.saturating_sub(1));
          image_published(generation.saturating_sub(1));
        },
        budget,
      )
      .await;
      let short = budget / 50;
      if published(origin, ticket, short).await {
        outcome.push("a ticket answered durable before its image was published".to_owned());
      }
      // The publication comes from another task while this one waits on the ticket.
      let publisher = slates_rt::futures::spawn(async move {
        let _ = crate::xshard::call_in_shard_time(
          origin,
          other,
          move |_| {
            image_begun(generation);
            image_published(generation);
          },
          budget,
        )
        .await;
      })
      .unwrap();
      slates_rt::futures::detach(publisher).unwrap();
      if !published(origin, ticket, budget).await {
        outcome.push("a ticket's wait did not end at its image's publication".to_owned());
      }
      let older = HomeTicket {
        shard: other,
        generation: generation.saturating_sub(1),
      };
      if !published(origin, older, short).await {
        outcome.push("an older ticket did not answer at once".to_owned());
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv_timeout(Duration::from_nanos(ANSWER_WAIT_NS));
  daemon.stop();
  let outcome = outcome.expect("the shards answered");
  assert!(outcome.is_empty(), "{outcome:#?}");
}

/// A-724 piece 6 (N = 1; the batch forms agreed with w-perf). Do: on a four-shard daemon, home 64 windows in one batch
/// with one of them twice, read them all back in one batch, then let them all go in one batch. Expect: every window
/// homed and read back whole in order; fewer calls than windows (one a home shard a batch while a home's quantum holds
/// its items, the rest the next turn); the window sent twice kept until its second let-go; all freed after.
#[test]
fn a_batch_of_windows_goes_to_each_home_in_one_call_a_shard() {
  use super::{BATCH_CALLS, home_many, let_go_many, read_many};
  let daemon = crate::daemon::audit_daemon_of(4);
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |state| {
    let origin = state.shard;
    let shards = state.shards.clone();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let mut outcome: Vec<String> = Vec::new();
      let mut windows: Vec<Vec<u8>> = (0..WINDOWS).map(window).collect();
      windows.push(window(0));
      let homed = home_many(origin, &shards, windows.clone(), budget).await;
      if homed.len() != windows.len() || homed.iter().any(Result::is_err) {
        outcome.push(format!("not every window homed: {homed:?}"));
      }
      let identities: Vec<ContentId> = windows.iter().map(|bytes| ContentId::of(bytes)).collect();
      let read_back = read_many(origin, &shards, &identities, budget).await;
      for (at, (bytes, got)) in windows.iter().zip(&read_back).enumerate() {
        if got.as_ref() != Some(bytes) {
          outcome.push(format!("window {at} not read back in order"));
        }
      }
      let once: Vec<ContentId> = identities.iter().take(WINDOWS).copied().collect();
      if let_go_many(origin, &shards, &once, budget)
        .await
        .iter()
        .any(Result::is_err)
      {
        outcome.push("not every window let go".to_owned());
      }
      if read(origin, &shards, identities[0], budget).await.is_none() {
        outcome.push("the window sent twice was freed at its first let-go".to_owned());
      }
      if let_go_many(origin, &shards, &identities[..1], budget)
        .await
        .iter()
        .any(Result::is_err)
      {
        outcome.push("the window sent twice was not let go again".to_owned());
      }
      if read_many(origin, &shards, &once, budget)
        .await
        .iter()
        .any(Option::is_some)
      {
        outcome.push("a window outlived its last let-go".to_owned());
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv_timeout(Duration::from_nanos(ANSWER_WAIT_NS));
  let calls: u64 = (0..4)
    .map(|partition| {
      crate::daemon::observe_at(&daemon, partition, |state| {
        state.refusals.get(BATCH_CALLS).copied().unwrap_or(0)
      })
    })
    .sum();
  daemon.stop();
  let outcome = outcome.expect("the shards answered");
  assert!(outcome.is_empty(), "{outcome:#?}");
  // Five batches over 65 + 65 + 64 + 1 + 64 = 259 windows. A call works through its home's items until the home's step
  // quantum is spent (at least one), so how many fit a call depends on the build's speed: fewer calls than windows is the
  // property, one call a home shard a batch the case when the quantum holds them all.
  println!("five batches of 259 windows over 4 homes: {calls} calls");
  assert!(
    calls < 259,
    "a batch made a call a window or more: {calls} calls"
  );
}
