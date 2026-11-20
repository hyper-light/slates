//! A-724 piece 5's tests (§4.17.5 "Giant repositories"): content homed by its identity across a node's shards.

use slates_db::register::rendezvous_ranked_by_key;
use slates_vcs::ContentId;

use super::{HOMED, LET_GO, home, home_of, let_go, read};

/// Shape: the windows the spread test homes: enough that every one of four shards is some window's home with
/// overwhelming probability (each misses all with probability (3/4)^64, about 1e-8).
const WINDOWS: usize = 64;

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
      // An object past a window is homed whole under the BLAKE3 of its bytes (the identity rule), each home cutting its
      // own windows; let go again, as the windows were.
      let identity = ContentId::of(&past);
      match home(origin, &shards, past, budget).await {
        Ok((homed, _)) if homed == identity => {}
        other => outcome.push(format!(
          "an object past a window not homed by its BLAKE3: {other:?}"
        )),
      }
      if let_go(origin, &shards, identity, budget).await.is_err() {
        outcome.push("the object past a window not let go".to_owned());
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv();
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
    u64::try_from(WINDOWS + 2).unwrap(),
    "every window kept at a home, one twice, and the object past a window once"
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
      // Nothing else publishes on a schedule, so the ticket's wait publishes its home itself (group commit): durable
      // only once an image of its generation is published there.
      let short = budget / 50;
      if published(origin, ticket, short).await
        && !home_published(origin, other, generation, budget).await
      {
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
  let outcome = answered.recv();
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
  let outcome = answered.recv();
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

/// A-724 piece 6's region half (main's ruling: a home acknowledges only once its image holds the window). Do: on a
/// two-shard daemon, the region's one member at f = 0 (so every window's home), serve a put from the region before the
/// home shard has published the image that holds it, then while that image is published; read it back; let it go; read
/// again; and put bytes under another name. Expect: no `Kept` before the image is published (`Unanswered` at the short
/// wait); `Kept` with the ticket's generation once it is; the bytes read back; one `LetGo` freeing the region's one hold (two puts
/// hold it once) and a second `NotHeld`; then the read `NotHeld`; and `Misnamed`.
#[test]
fn a_put_from_the_region_is_kept_on_its_home_shard_and_acknowledged_only_once_its_image_is_published()
 {
  use super::{home_of, image_begun, image_published, serve_remote};
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal};
  let daemon = crate::daemon::audit_daemon_of(2);
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |state| {
    let origin = state.shard;
    let shards = state.shards.clone();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let short = budget / 50;
      let mut outcome: Vec<String> = Vec::new();
      let bytes = window(7);
      let identity = ContentId::of(&bytes).0;
      let home = home_of(&shards, &ContentId(identity)).unwrap();
      let put = HomeMessage::Put {
        identity,
        bytes: bytes.clone(),
      }
      .encode();
      // The put's wait publishes its home itself when nothing else has (group commit): kept only once an image of the
      // generation it names is published there.
      let before = HomeMessage::decode(&serve_remote(origin, &put, short).await);
      let durable = match &before {
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Kept { generation },
          ..
        }) => home_published(origin, home, *generation, budget).await,
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Refused(HomeRefusal::Unanswered),
          ..
        }) => true,
        _ => false,
      };
      if !durable {
        outcome.push(format!(
          "acknowledged before its image was published: {before:?}"
        ));
      }
      // The home's barrier (A-727's, played here at a chosen step, no clock): the next image captures the content table
      // and is published, before the put is served again.
      let _ = crate::xshard::call_in_shard_time(
        origin,
        home,
        move |_| {
          image_begun(1);
          image_published(2);
        },
        budget,
      )
      .await;
      match HomeMessage::decode(&serve_remote(origin, &put, budget).await) {
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Kept { generation },
          ..
        }) if generation <= 2 => {}
        other => outcome.push(format!("not kept once published: {other:?}")),
      }
      let read = HomeMessage::Read { identity }.encode();
      match HomeMessage::decode(&serve_remote(origin, &read, budget).await) {
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Bytes(got),
          ..
        }) if got == bytes => {}
        other => outcome.push(format!("not read back: {other:?}")),
      }
      // The region holds a window once however often it was put (the healer's offers are idempotent): one let-go
      // frees it, and a second finds it no longer held for the region.
      let answer = HomeMessage::decode(
        &serve_remote(origin, &HomeMessage::LetGo { identity }.encode(), budget).await,
      );
      if !matches!(
        answer,
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::LetGo,
          ..
        })
      ) {
        outcome.push(format!("not let go: {answer:?}"));
      }
      let again = HomeMessage::decode(
        &serve_remote(origin, &HomeMessage::LetGo { identity }.encode(), budget).await,
      );
      if !matches!(
        again,
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Refused(HomeRefusal::NotHeld),
          ..
        })
      ) {
        outcome.push(format!("a second region let-go found a hold: {again:?}"));
      }
      let gone = HomeMessage::decode(&serve_remote(origin, &read, budget).await);
      if !matches!(
        gone,
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Refused(HomeRefusal::NotHeld),
          ..
        })
      ) {
        outcome.push(format!("read after its last let-go: {gone:?}"));
      }
      let misnamed = HomeMessage::Put {
        identity,
        bytes: b"other bytes".to_vec(),
      }
      .encode();
      let refused = HomeMessage::decode(&serve_remote(origin, &misnamed, budget).await);
      if !matches!(
        refused,
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Refused(HomeRefusal::Misnamed),
          ..
        })
      ) {
        outcome.push(format!("misnamed bytes kept: {refused:?}"));
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv();
  daemon.stop();
  let outcome = outcome.expect("the shards answered");
  assert!(outcome.is_empty(), "{outcome:#?}");
}

/// A-724 piece 6's healer, on one node. Do: hold windows for the region on a two-shard daemon whose region is this node
/// alone (f = 0), offering each twice, then run the healer's turn at the configuration. Expect: a region offer of a
/// window held already adds no hold; the healer finds every window homed here, offers nothing and lets nothing go
/// (laptop ≡ fleet: the same pass, nothing to move); one region let-go then frees each.
#[test]
fn the_healer_on_a_lone_node_moves_nothing_and_region_offers_are_idempotent() {
  use super::{
    HEAL_OFFERED, HEAL_RELEASED, heal_region, home_of, keep_for_region, let_go_for_region,
  };
  let daemon = crate::daemon::audit_daemon_of(2);
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |state| {
    let origin = state.shard;
    let shards = state.shards.clone();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let mut outcome: Vec<String> = Vec::new();
      for index in 0..8 {
        let bytes = window(index);
        let home = home_of(&shards, &ContentId::of(&bytes)).unwrap();
        let kept = crate::xshard::call_in_shard_time(
          origin,
          home,
          move |state| {
            let first = keep_for_region(state, &bytes).is_ok();
            let second = keep_for_region(state, &bytes).is_ok();
            let identity = ContentId::of(&bytes);
            let holders = state
              .vcs_stores
              .content
              .as_ref()
              .map_or(0, |content| content.holders(&identity));
            (first && second, holders)
          },
          budget,
        )
        .await;
        if kept != Some((true, 1)) {
          outcome.push(format!(
            "window {index}: region offers not idempotent: {kept:?}"
          ));
        }
      }
      heal_region(origin, &shards, crate::fleet::live_budget()).await;
      let counted = crate::state::with_state(|state| {
        (
          state.refusals.get(HEAL_OFFERED).copied().unwrap_or(0),
          state.refusals.get(HEAL_RELEASED).copied().unwrap_or(0),
        )
      });
      if counted != Some((0, 0)) {
        outcome.push(format!("a lone node's healer moved something: {counted:?}"));
      }
      for index in 0..8 {
        let identity = ContentId::of(&window(index));
        let home = home_of(&shards, &identity).unwrap();
        let freed = crate::xshard::call_in_shard_time(
          origin,
          home,
          move |state| {
            let first = let_go_for_region(state, &identity).is_ok();
            let gone = !state
              .vcs_stores
              .content
              .as_ref()
              .is_some_and(|content| content.contains(&identity));
            (first, gone)
          },
          budget,
        )
        .await;
        if freed != Some((true, true)) {
          outcome.push(format!(
            "window {index} not freed by one region let-go: {freed:?}"
          ));
        }
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv();
  daemon.stop();
  let outcome = outcome.expect("the shards answered");
  assert!(outcome.is_empty(), "{outcome:#?}");
}

/// A-724 with A-727 a3, the barrier driven by the daemon's own publications. Do: keep a window on this shard (its
/// ticket), then commit a publication of the shard's held pieces as `verbs::publish_shard` does (`held_committed`),
/// then keep a second. Expect: the first ticket not published before the publication and published after it; the
/// second's generation past it, so a window kept after a publication waits for the next.
#[test]
fn a_committed_publication_publishes_the_tickets_of_windows_kept_before_it() {
  use super::{keep_here, published_here};
  crate::daemon::audit_on_shard(|state| {
    let (_, first) = keep_here(state, &window(1)).unwrap();
    assert!(
      !published_here(first.generation),
      "durable before any publication"
    );
    crate::content_holder::held_committed(state, std::collections::BTreeMap::new(), false);
    assert!(
      published_here(first.generation),
      "a committed publication left its ticket unpublished"
    );
    let (_, second) = keep_here(state, &window(2)).unwrap();
    assert!(second.generation > first.generation);
    assert!(
      !published_here(second.generation),
      "a window kept after the publication counted durable"
    );
  });
}

/// A-724 piece 8c (main's ruling (ii): a record names its names only once they are durable at `f + 1` in the region).
/// Do: on a lone node (its own region, so it is the object's one home), home an object in the region with no publication
/// within a short budget, then again while a publication commits on its home shard. Expect: the first refused
/// `Unanswered` (never durable, so no record may name it); the second the object's BLAKE3, held for the region at its
/// home shard and counted.
#[test]
fn an_object_is_homed_in_the_region_only_once_its_homes_image_is_published() {
  use super::{HOMED_IN_REGION, home_in_region, publication_committed, region_held_here};
  let daemon = crate::daemon::audit_daemon_of(2);
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |state| {
    let origin = state.shard;
    let shards = state.shards.clone();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let mut outcome: Vec<String> = Vec::new();
      let bytes = window(7);
      let identity = ContentId::of(&bytes);
      let home = home_of(&shards, &identity).unwrap();
      let short = slates_cluster::CommitBudget::hard(budget / 50, (budget / 500).max(1));
      // Homed only once its home has published an image holding it: the wait publishes the home itself where nothing
      // else has, counted.
      if home_in_region(origin, &shards, bytes.clone(), short)
        .await
        .is_ok()
        && crate::xshard::call_in_shard_time(
          origin,
          home,
          |state| {
            state
              .refusals
              .get(super::HOME_PUBLISHED)
              .copied()
              .unwrap_or(0)
          },
          budget,
        )
        .await
        .unwrap_or(0)
          == 0
      {
        outcome.push("homed with no publication of its home".to_owned());
      }
      let publisher = slates_rt::futures::spawn(async move {
        let _ =
          crate::xshard::call_in_shard_time(origin, home, |_| publication_committed(), budget)
            .await;
      })
      .unwrap();
      slates_rt::futures::detach(publisher).unwrap();
      let full = slates_cluster::CommitBudget::hard(budget, (budget / 100).max(1));
      match home_in_region(origin, &shards, bytes, full).await {
        Ok(homed) if homed == identity => {}
        other => outcome.push(format!("not homed after the publication: {other:?}")),
      }
      let held = crate::xshard::call_in_shard_time(origin, home, |_| region_held_here(), budget)
        .await
        .unwrap_or_default();
      if !held.contains(&identity) {
        outcome.push("not held for the region at its home".to_owned());
      }
      let counted =
        crate::state::with_state(|state| state.refusals.get(HOMED_IN_REGION).copied().unwrap_or(0))
          .unwrap_or(0);
      if counted == 0 {
        outcome.push("not counted".to_owned());
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv().unwrap();
  assert!(outcome.is_empty(), "{outcome:?}");
}

/// Whether shard `shard`'s image of `generation` is published, asked from shard `origin`.
async fn home_published(origin: u16, shard: u16, generation: u64, budget: u64) -> bool {
  crate::xshard::call_in_shard_time(
    origin,
    shard,
    move |_| super::published_here(generation),
    budget,
  )
  .await
  .unwrap_or(false)
}

/// A-724 piece 8c (main's ruling: across nodes the unit is the object keyed by the BLAKE3 of its bytes, each home
/// cutting its own windows). Do: home in the region an object three windows long, then read it back from the region.
/// Expect: homed under the BLAKE3 of its whole bytes (no window's identity), and read back byte for byte.
#[test]
fn an_object_longer_than_a_window_is_homed_and_read_back_by_its_blake3() {
  use super::{home_in_region, read_in_region};
  let daemon = crate::daemon::audit_daemon_of(2);
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |state| {
    let origin = state.shard;
    let shards = state.shards.clone();
    let window = usize::try_from(state.store.content.chunk_bytes()).unwrap();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let full = slates_cluster::CommitBudget::hard(budget, (budget / 100).max(1));
      let mut outcome: Vec<String> = Vec::new();
      let bytes: Vec<u8> = (0..window * 3 + 17).map(|at| (at % 251) as u8).collect();
      let identity = ContentId::of(&bytes);
      match home_in_region(origin, &shards, bytes.clone(), full).await {
        Ok(homed) if homed == identity => {}
        other => outcome.push(format!("not homed under its BLAKE3: {other:?}")),
      }
      match read_in_region(origin, &shards, identity, full).await {
        Some(read) if read == bytes => {}
        other => outcome.push(format!(
          "not read back whole: {:?} bytes",
          other.map(|read| read.len())
        )),
      }
      let _ = sent.send(outcome);
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  let outcome = answered.recv().unwrap();
  assert!(outcome.is_empty(), "{outcome:?}");
}
