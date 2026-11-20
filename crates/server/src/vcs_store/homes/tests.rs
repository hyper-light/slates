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
        let identity = home(origin, &shards, bytes.clone(), budget).await;
        let Ok(identity) = identity else {
          outcome.push(format!("window {index} not homed: {identity:?}"));
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
