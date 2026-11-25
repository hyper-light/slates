//! A-724 piece 5's tests (§4.17.5 "Giant repositories"): content homed by its identity across a node's shards.

use slates_db::register::rendezvous_ranked_by_key;
use slates_vcs::ContentId;

use super::{HOMED, LET_GO, home_of, keep_region_at_home, let_go_region_at_home, read};

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

/// A-724 piece 5, homed for the region (main 01a20182's ruling of 2026-11-22 21:15: what a home keeps it keeps for the
/// region, the single-window homing gone). Do: on a four-shard daemon, from the first shard, keep many windows for the
/// region at their homes (one twice), read each back, let each go, and keep an object past a window. Expect: each window
/// kept on its home and only there, the homes spread over the shards; each read back whole from its home; a window
/// kept twice for the region held once (the region's offers are idempotent), freed at its let-go and refused a second;
/// every window freed after its let-go; the object past a window kept under its BLAKE3 and let go.
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
        let homed = keep_region_at_home(
          origin,
          &shards,
          (bytes.clone(), vec![super::TEST_OWNER]),
          budget,
        )
        .await;
        let Ok((identity, _)) = homed else {
          outcome.push(format!("window {index} not homed: {homed:?}"));
          continue;
        };
        homes.insert(home_of(&shards, &identity));
        if read(origin, &shards, identity, budget).await != Ok(bytes) {
          outcome.push(format!("window {index} not read back"));
        }
      }
      if homes.len() < 2 {
        outcome.push(format!("homes did not spread: {homes:?}"));
      }
      let twice = window(0);
      let identity = ContentId::of(&twice);
      let _ = keep_region_at_home(
        origin,
        &shards,
        (twice.clone(), vec![super::TEST_OWNER]),
        budget,
      )
      .await;
      for index in 0..WINDOWS {
        let identity = ContentId::of(&window(index));
        if let_go_region_at_home(origin, &shards, identity, Some(super::TEST_OWNER), budget)
          .await
          .is_err()
        {
          outcome.push(format!("window {index} not let go"));
        }
      }
      if read(origin, &shards, identity, budget).await.is_ok() {
        outcome.push("a window kept twice for the region outlived its one let-go".to_owned());
      }
      if let_go_region_at_home(origin, &shards, identity, Some(super::TEST_OWNER), budget)
        .await
        .is_ok()
      {
        outcome.push("a window kept twice for the region let go a second time".to_owned());
      }
      for index in 0..WINDOWS {
        if read(origin, &shards, ContentId::of(&window(index)), budget)
          .await
          .is_ok()
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
      match keep_region_at_home(origin, &shards, (past, vec![super::TEST_OWNER]), budget).await {
        Ok((homed, _)) if homed == identity => {}
        other => outcome.push(format!(
          "an object past a window not homed by its BLAKE3: {other:?}"
        )),
      }
      if let_go_region_at_home(origin, &shards, identity, Some(super::TEST_OWNER), budget)
        .await
        .is_err()
      {
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
    u64::try_from(WINDOWS + 1).unwrap(),
    "every window kept at a home once (a second offer held nothing more), and the object past a window once"
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
      let (_, ticket) =
        keep_region_at_home(origin, &shards, (bytes, vec![super::TEST_OWNER]), budget)
          .await
          .unwrap();
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
    let local = state.fleet.host();
    let shards = state.shards.clone();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let short = budget / 50;
      let mut outcome: Vec<String> = Vec::new();
      let bytes = window(7);
      let identity = ContentId::of(&bytes).0;
      let home = home_of(&shards, &ContentId(identity)).unwrap();
      let put = HomeMessage::Put {
        configuration: 0,
        owners: vec![super::TEST_OWNER],
        identity,
        bytes: bytes.clone(),
      }
      .encode();
      // The put's wait publishes its home itself when nothing else has (group commit): kept only once an image of the
      // generation it names is published there.
      let before = HomeMessage::decode(&serve_remote((origin, local), &put, short).await);
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
          let generation = super::ticket_here(home).generation;
          image_begun(generation);
          image_published(generation);
        },
        budget,
      )
      .await;
      // Kept again, held once more, under a ticket its home has published (A-870 F2: a cut's generation and the
      // publication's after it are two, so the ticket's number is not fixed here; that it is published is the rule).
      let again = HomeMessage::decode(&serve_remote((origin, local), &put, budget).await);
      let published = match &again {
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Kept { generation },
          ..
        }) => home_published(origin, home, *generation, budget).await,
        _ => false,
      };
      match again {
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Kept { .. },
          ..
        }) if published => {}
        other => outcome.push(format!("not kept once published: {other:?}")),
      }
      let read = HomeMessage::Read { identity }.encode();
      match HomeMessage::decode(&serve_remote((origin, local), &read, budget).await) {
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Bytes(got),
          ..
        }) if got == bytes => {}
        other => outcome.push(format!("not read back: {other:?}")),
      }
      // The region holds a window once however often it was put (the healer's offers are idempotent): one let-go
      // frees it, and a second finds it no longer held for the region.
      let answer = HomeMessage::decode(
        &serve_remote(
          (origin, local),
          &HomeMessage::LetGo {
            identity,
            owner: Some(super::TEST_OWNER),
          }
          .encode(),
          budget,
        )
        .await,
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
        &serve_remote(
          (origin, local),
          &HomeMessage::LetGo {
            identity,
            owner: Some(super::TEST_OWNER),
          }
          .encode(),
          budget,
        )
        .await,
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
      let gone = HomeMessage::decode(&serve_remote((origin, local), &read, budget).await);
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
        configuration: 0,
        owners: vec![super::TEST_OWNER],
        identity,
        bytes: b"other bytes".to_vec(),
      }
      .encode();
      let refused = HomeMessage::decode(&serve_remote((origin, local), &misnamed, budget).await);
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
            let first = keep_for_region(state, &bytes, &[super::TEST_OWNER]).is_ok();
            let second = keep_for_region(state, &bytes, &[super::TEST_OWNER]).is_ok();
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
            let first = let_go_for_region(state, &identity, Some(super::TEST_OWNER)).is_ok();
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
/// ticket); begin the encoder's cut with a turn, as a publication does, and commit a publication that carried the older
/// cut again; turn until the cut is encoded and commit one that carried it (`content_holder::image_committed`, A-870
/// piece 2 with F2); then keep a second. Expect: the first ticket not published before the publications nor after the
/// older cut's, published after the encoder's cut; the second's generation past it, so a window kept after a
/// publication waits for the next.
#[test]
fn a_committed_publication_publishes_the_tickets_of_windows_kept_before_it() {
  use super::{keep_here, published_here};
  crate::daemon::audit_on_shard(|state| {
    let (_, first) = keep_here(state, &window(1)).unwrap();
    assert!(
      !published_here(first.generation),
      "durable before any publication"
    );
    // A-870 piece 2: a publication that carried the committed cut again (a change not yet encoded) carried no window
    // kept since, so its ticket waits; one that carried the encoder's cut publishes it.
    let _ = crate::content_holder::image_turn(state);
    crate::content_holder::image_committed(state, false);
    assert!(
      !published_here(first.generation),
      "a publication that carried an older cut published the ticket"
    );
    while !crate::content_holder::image_turn(state).caught_up {}
    crate::content_holder::image_committed(state, true);
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
      if home_in_region(origin, &shards, (bytes.clone(), super::TEST_OWNER), short)
        .await
        .outcome
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
      match home_in_region(origin, &shards, (bytes, super::TEST_OWNER), full)
        .await
        .outcome
      {
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
    let window = state.store.content.chunk_bytes();
    let task = slates_rt::futures::spawn(async move {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let full = slates_cluster::CommitBudget::hard(budget, (budget / 100).max(1));
      let mut outcome: Vec<String> = Vec::new();
      let bytes: Vec<u8> = (0..window * 3 + 17)
        .map(|at| u8::try_from(at % 251).unwrap_or(0))
        .collect();
      let identity = ContentId::of(&bytes);
      match home_in_region(origin, &shards, (bytes.clone(), super::TEST_OWNER), full)
        .await
        .outcome
      {
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

/// A-724 piece 8c and A-863, goal 93. Do: keep a two-window object for the region on its home
/// shard. Land a Mercurial file with the same first window and a second, independent file.
/// Expect: the independent file's window leaves RAM and the home still serves the whole object.
#[test]
fn a_region_object_remains_readable_when_a_landed_file_shares_its_first_window() {
  let daemon = crate::daemon::audit_daemon_of(2);
  let (shards, window) = crate::daemon::observe_first(&daemon, |state| {
    (state.shards.clone(), state.store.content.chunk_bytes())
  });
  let mut bytes = vec![0; window + 1];
  let shared_home = home_of(&shards, &ContentId::of(&bytes[..window])).unwrap();
  // Select two different homes with the daemon's own shard IDs and placement rule.
  let object_home = (u8::MIN..=u8::MAX)
    .find_map(|tail| {
      bytes[window] = tail;
      home_of(&shards, &ContentId::of(&bytes)).filter(|home| *home != shared_home)
    })
    .expect("a two-window object whose first window ranks on the other shard");
  let partition = shards
    .iter()
    .position(|shard| *shard == object_home)
    .unwrap();
  let independent = (u8::MIN..=u8::MAX)
    .map(|byte| [byte, byte])
    .find(|text| home_of(&shards, &ContentId::of(text)) == Some(shared_home))
    .expect("an independent file whose window may leave this shard");
  let (before, after, freed) = crate::daemon::observe_at(&daemon, partition, move |state| {
    let (object, _) = super::keep_for_region(state, &bytes, &[super::TEST_OWNER]).unwrap();
    let before = super::read_here(state, &object).as_deref() == Ok(bytes.as_slice());
    let files =
      crate::vcs_store::within_budget(state, (window + independent.len()) as u64, |stores| {
        let content = stores.content_mut().unwrap();
        [
          content.put_object(&bytes[..window]).unwrap(),
          content.put_object(&independent).unwrap(),
        ]
      });
    // As a segment lands, before its entry commits: no revision noted, so the pin is what keeps a shared window.
    let files = files.map(|bytes| slates_hg::filerev::FileContents {
      path: b"file".to_vec(),
      node: [0; 20],
      bytes,
      block: None,
    });
    crate::repository::hg_run::let_go_landed(state, None, &files);
    let after = super::read_here(state, &object).map(|read| read.len());
    let freed = state
      .refusals
      .get(crate::repository::hg_run::TEXT_BYTES_LET_GO)
      .copied()
      .unwrap_or(0);
    (before, after, freed)
  });
  daemon.stop();
  assert!(
    before,
    "the home served the complete object before the two file windows were released"
  );
  assert!(
    freed >= independent.len() as u64,
    "Mercurial released the independent file"
  );
  assert_eq!(
    after,
    Ok(window + 1),
    "the region's object must remain readable after the release of a shared window; {freed} bytes left RAM"
  );
}

/// Goal 93's hg bar (main 01a20182's ruling of 2026-11-22 21:15: a window is pinned only where its shard holds it for the
/// region, never where its identity ranks; arena-3's push peaked at 4,077 MiB with 75,440 texts pinned at their segments'
/// durability, about half of them a two-shard daemon's own by rank). Do: on a two-shard daemon, on one shard, hold an
/// object for the region and put a segment's file texts whose windows all rank on that shard; let them go as a segment
/// lands, before its entry commits (no revision noted). Expect: every text's window let go and none pinned, the bytes
/// counted; the region object still read whole from its home. Failing first: each text pinned by its rank, kept in RAM
/// until the push's commit.
#[test]
fn a_segments_texts_that_rank_on_their_shard_are_let_go_at_its_durability() {
  const TEXTS: u8 = 8;
  let daemon = crate::daemon::audit_daemon_of(2);
  let shards = crate::daemon::observe_first(&daemon, |state| state.shards.clone());
  let here = shards[0];
  // Texts that each rank on `here`, so the old rule pinned every one.
  let texts: Vec<Vec<u8>> = (0u32..)
    .map(|at| format!("a segment's text {at}\n").into_bytes())
    .filter(|text| home_of(&shards, &ContentId::of(text)) == Some(here))
    .take(usize::from(TEXTS))
    .collect();
  let region_object = b"an object the region holds here".to_vec();
  let (gone, pinned, freed, read) = crate::daemon::observe_at(&daemon, 0, move |state| {
    let (object, _) = super::keep_for_region(state, &region_object, &[super::TEST_OWNER]).unwrap();
    let need: u64 = texts.iter().map(|text| text.len() as u64).sum();
    let ids: Vec<ContentId> = crate::vcs_store::within_budget(state, need, |stores| {
      let content = stores.content_mut().unwrap();
      texts
        .iter()
        .map(|text| content.put_object(text).unwrap())
        .collect()
    });
    let files: Vec<slates_hg::filerev::FileContents> = ids
      .iter()
      .enumerate()
      .map(|(at, bytes)| slates_hg::filerev::FileContents {
        path: format!("f{at}").into_bytes(),
        node: [u8::try_from(at).unwrap(); 20],
        bytes: *bytes,
        block: None,
      })
      .collect();
    crate::repository::hg_run::let_go_landed(state, None, &files);
    let gone = state.vcs_stores.content().map_or(0, |content| {
      ids.iter().filter(|id| content.is_away(id)).count()
    });
    let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    (
      gone,
      count(crate::repository::hg_run::TEXTS_PINNED),
      count(crate::repository::hg_run::TEXT_BYTES_LET_GO),
      super::read_here(state, &object),
    )
  });
  daemon.stop();
  assert_eq!(
    gone,
    usize::from(TEXTS),
    "every text's window let go at its segment's durability"
  );
  assert_eq!(pinned, 0, "none pinned by where its identity ranks");
  assert!(freed > 0, "the bytes let go counted");
  assert_eq!(
    read.as_deref(),
    Ok(b"an object the region holds here".as_slice()),
    "the region's object still read whole from its home"
  );
}

/// A-727 (d1a), owners (main 01a20182's rulings of 2026-11-23). Do: on one shard, keep an object for an attempt, again
/// for the same attempt, then for another repository's records; move the attempt's hold to its records; let each owner
/// go in turn. Expect: one content-store hold whatever the owners (retries flat); the owners each once; a move naming the
/// records, the attempt's let-go after it finding nothing; the object held until its last owner goes, then given back,
/// the store's holders as before the first keep.
#[test]
fn a_region_hold_is_kept_once_per_owner_and_goes_with_its_last_owner() {
  use slates_cluster::homes::HomeOwner;
  let outcome = crate::daemon::audit_on_shard(|state| {
    let attempt = HomeOwner::Attempt {
      repository: [1; 16],
      node: slates_db::HostId(1),
      process: 7,
      attempt: 1,
    };
    let records = HomeOwner::Record {
      repository: [1; 16],
    };
    let other = HomeOwner::Record {
      repository: [2; 16],
    };
    let bytes = b"an object two repositories home in the region".to_vec();
    let holders = |state: &crate::state::ShardState, identity| {
      state
        .vcs_stores
        .content()
        .map_or(0, |content| content.holders(identity))
    };
    let (identity, _) = super::keep_for_region(state, &bytes, &[attempt]).unwrap();
    let once = holders(state, &identity);
    let _ = super::keep_for_region(state, &bytes, &[attempt]).unwrap();
    let _ = super::keep_for_region(state, &bytes, &[other]).unwrap();
    let flat = holders(state, &identity) == once;
    let owners = super::region_owners(&identity);
    let moved = super::move_region_owner(&identity, (attempt, records)).is_ok();
    let after_move = super::region_owners(&identity);
    let stale = super::let_go_for_region(state, &identity, Some(attempt));
    let first = super::let_go_for_region(state, &identity, Some(records)).is_ok();
    let kept = super::region_holds_here(&identity);
    let last = super::let_go_for_region(state, &identity, Some(other)).is_ok();
    let gone = super::region_holds_here(&identity);
    (
      (once, flat),
      (owners, moved, after_move),
      (stale, first, kept, last, gone),
    )
  });
  let ((once, flat), (owners, moved, after_move), (stale, first, kept, last, gone)) = outcome;
  assert_eq!(once, 1, "one hold of the store's for the region");
  assert!(flat, "a retry and a second owner take no further hold");
  assert_eq!(owners.len(), 2, "two owners, each once: {owners:?}");
  assert!(moved, "the attempt's hold moved to its records");
  assert_eq!(
    after_move,
    vec![
      HomeOwner::Record {
        repository: [1; 16]
      },
      HomeOwner::Record {
        repository: [2; 16]
      },
    ],
    "the attempt's hold now its records'"
  );
  assert_eq!(
    stale,
    Err(super::HomeRefused::NotHeld),
    "the attempt holds nothing once moved"
  );
  assert!(first && kept == 1, "held while another owner remains");
  assert!(last && gone == 0, "given back with its last owner");
}

/// A-727 (d1a), the image. Do: on one shard, keep an object for two owners, take the shard's held image and read it
/// back as a restore does; then read an image holding a region hold of the retired ownerless kind. Expect: the hold and
/// both its owners read back; the retired kind refused typed (`HeldRefused::Retired`), never read as a hold of no owner.
#[test]
fn a_region_holds_owners_ride_the_image_and_the_ownerless_kind_is_refused() {
  use slates_cluster::homes::HomeOwner;
  let owners = [
    HomeOwner::Record {
      repository: [3; 16],
    },
    HomeOwner::Attempt {
      repository: [4; 16],
      node: slates_db::HostId(9),
      process: 11,
      attempt: 2,
    },
  ];
  let (identity, held, retired) = crate::daemon::audit_on_shard(move |state| {
    let (identity, _) =
      super::keep_for_region(state, b"an object held for two owners", &owners).unwrap();
    // The paged families copy the encoder's cut (A-870 piece 2): turned to its end first.
    crate::content_holder::image_drain(state);
    while !crate::content_holder::image_turn(state).caught_up {}
    let image = crate::content_holder::held_image(state);
    let held = crate::content_holder::split_held(&image).map(|held| held.region_holds);
    let retired =
      crate::content_holder::split_held(&[crate::content_holder::retired_region_hold_piece(
        &identity,
      )])
      .err();
    (identity, held, retired)
  });
  let mut expected = owners.to_vec();
  expected.sort();
  let held = held.expect("the image read back");
  let found = held
    .iter()
    .find(|(held, _)| *held == identity)
    .map(|(_, owners)| {
      let mut owners = owners.clone();
      owners.sort();
      owners
    });
  assert_eq!(found, Some(expected), "the hold and its owners read back");
  assert!(
    matches!(
      retired,
      Some(crate::content_holder::HeldRefused::Retired(14))
    ),
    "the ownerless kind refused typed: {retired:?}"
  );
}

/// A-727 (d1a), no mixed-version homing (main 01a20182's ruling of 2026-11-23: refused at once and typed, never left to
/// its sender's deadline). Do: serve a version-1 put (its kind byte first, no owner) from the region. Expect: answered at
/// once with a typed refusal of its version (`HomeRefusal::Version`), counted, nothing kept: its sender fails its homing
/// at once (a version-1 sender reads no version-2 answer, which fails its exchange as at once).
#[test]
fn a_home_message_of_another_version_is_refused_at_once_and_typed() {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal};
  let (answer, refused, held) = crate::test_store::run_on_first(
    &crate::daemon::audit_daemon(),
    || async {
      let mut first = vec![1u8];
      first.extend_from_slice(&[5; 32]);
      first.extend_from_slice(&3u32.to_le_bytes());
      first.extend_from_slice(b"abc");
      let (origin, local) =
        crate::state::with_state(|state| (state.shard, state.fleet.host())).unwrap();
      let answer =
        super::serve_remote((origin, local), &first, crate::daemon::LIVENESS_BUDGET_NS).await;
      let refused = crate::state::with_state(|state| {
        state
          .refusals
          .get(super::VERSION_REFUSED)
          .copied()
          .unwrap_or(0)
      });
      (answer, refused, super::region_held_here().len())
    },
    std::time::Duration::from_secs(60),
  );
  assert!(
    matches!(
      HomeMessage::decode(&answer),
      Ok(HomeMessage::Answer {
        outcome: HomeOutcome::Refused(HomeRefusal::Version),
        ..
      })
    ),
    "{answer:?}"
  );
  assert_eq!(refused, Some(1), "counted");
  assert_eq!(held, 0, "nothing kept");
}

/// A recovered region hold is taken only once the region's set can name it (main 01a20182's commit-2 list: a hold taken
/// first, then given back with its release's refusal dropped, could leave the store's count and the set apart). Do:
/// keep an object in a content store; borrow the region's set, as a step already holding it would; restore a region hold
/// of the object. Expect: the hold refused, the object's holders as before (no hold taken that the set could not name);
/// with the set free, the hold restored and counted in the store. Not driven red: a release of a hold just taken is
/// never refused, so the old order's dropped refusal cannot be met.
#[test]
fn a_region_hold_the_set_cannot_name_is_never_taken_in_the_store() {
  let mut content = slates_vcs::ContentStore::new(1 << 16, 1 << 8).unwrap();
  let object = content
    .put(b"an object a recovery holds for the region")
    .unwrap();
  let before = content.holders(&object);
  let holds = [(object, vec![super::TEST_OWNER])];
  let borrowed = super::REGION_HELD.with(|region| {
    let _named = region.borrow_mut();
    super::restore_region_holds(&mut content, &holds)
  });
  assert_eq!(
    borrowed,
    (0, vec![object]),
    "refused while the set is borrowed"
  );
  assert_eq!(content.holders(&object), before, "no hold taken unnamed");
  let free = super::restore_region_holds(&mut content, &holds);
  assert_eq!(free, (1, Vec::new()), "restored once the set is free");
  assert_eq!(content.holders(&object), before + 1, "held once more");
}

/// Main 01a20182's gate 7 (run 45398593779, release run 6: a push's pack homed short 0.9 ms after its body, never waiting
/// its homing's budget), w-shared agreeing the instrumentation in writing, 2026-11-24. Do: on an audit daemon's shard, wait
/// for its own image of a generation no publication will reach, so its ticket is stranded (not published and no image
/// owed). Expect: answered not durable at once, far inside the wait's budget, never left to run it out; counted
/// `vcs_store.homes.ticket_stranded` (and the branch logged), the counter that names this fast branch in a gate's log.
#[test]
fn a_stranded_tickets_wait_is_answered_not_durable_at_once_and_counted() {
  let budget = crate::daemon::LIVENESS_BUDGET_NS;
  let (answer, took_ns, stranded) = crate::test_store::run_on_first(
    &crate::daemon::audit_daemon(),
    move || async move {
      let shard = crate::state::with_state(|state| state.shard).unwrap();
      let ticket = super::HomeTicket {
        shard,
        generation: u64::MAX >> 1,
      };
      let began = slates_rt::futures::now_ns();
      let answer = super::published_within(shard, ticket, budget).await;
      let took_ns = slates_rt::futures::now_ns().saturating_sub(began);
      let stranded = crate::state::with_state(|state| {
        state
          .refusals
          .get(super::HOME_TICKET_STRANDED)
          .copied()
          .unwrap_or(0)
      })
      .unwrap_or(0);
      (answer, took_ns, stranded)
    },
    std::time::Duration::from_nanos(budget.saturating_mul(4)),
  );
  assert_eq!((answer, stranded), (Some(false), 1));
  assert!(
    took_ns < budget / 2,
    "answered at once, not at the budget: {took_ns} of {budget} ns"
  );
}

/// A-902's rule under A-912 (main 01a20182's ruling of 2026-11-24: a home behind its sender answers at once, never holding
/// the put). One node plays the home, its shard's configuration installed by the test. Do: install a configuration (one
/// past the node's own) that ranks every window at another member, then serve puts from the region:
/// (a) from a member carrying the next version;
/// (b) from a member this node's configuration does not name, at this node's own version;
/// (c) from a member carrying this node's own version;
/// (d) after installing the carried version, ranking this node (its only member), the put of (a) again.
/// Expect: (a) and (b) answered `Behind` with the installed version at once (within a tenth of the budget), counted
/// answered behind; (c) refused `NotAHome` at once, not counted; (d) kept. Red before A-912: (a) and (b) held for this
/// node's install (A-902's wait), answered only once it landed or the budget ran out.
#[test]
fn a_home_behind_its_senders_configuration_answers_behind_at_once_and_keeps_once_it_installs() {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal};
  let outcome = crate::test_store::run_on_first(
    &crate::daemon::audit_daemon_of(2),
    || async {
      let budget = crate::daemon::LIVENESS_BUDGET_NS;
      let mut outcome: Vec<String> = Vec::new();
      let Some((origin, local, own)) = crate::state::with_state(|state| {
        (
          state.shard,
          state.fleet.host(),
          state.fleet.configuration().clone(),
        )
      }) else {
        return vec!["no shard state".to_owned()];
      };
      let other = slates_db::HostId(local.0 ^ 1);
      let stranger = slates_db::HostId(local.0 ^ 2);
      let answered = || {
        crate::state::with_state(|state| {
          state
            .refusals
            .get(super::BEHIND_ANSWERED)
            .copied()
            .unwrap_or(0)
        })
        .unwrap_or(0)
      };
      let install = |version: u64, members: Vec<slates_db::HostId>| {
        let mut configuration = own.clone();
        configuration.version = version;
        let _ = crate::state::with_state(|state| {
          state.fleet.install_configuration(configuration, &members)
        });
      };
      let put = |index: usize, configuration: u64| {
        let bytes = window(1000 + index);
        HomeMessage::Put {
          identity: ContentId::of(&bytes).0,
          configuration,
          owners: vec![super::TEST_OWNER],
          bytes,
        }
        .encode()
      };
      let outcome_of = |reply: &[u8]| match HomeMessage::decode(reply) {
        Ok(HomeMessage::Answer { outcome, .. }) => Some(outcome),
        _ => None,
      };
      let behind = own.version.saturating_add(1);
      let ahead = behind.saturating_add(1);
      install(behind, vec![other]);
      for (case, sender, carried) in [("(a)", other, ahead), ("(b)", stranger, behind)] {
        let before = answered();
        let began = slates_rt::futures::now_ns();
        let reply = super::serve_remote((origin, sender), &put(0, carried), budget).await;
        let took = slates_rt::futures::now_ns().saturating_sub(began);
        let got = outcome_of(&reply);
        if got != Some(HomeOutcome::Behind { installed: behind }) || took > budget / 10 {
          outcome.push(format!(
            "{case} not answered Behind at once: {got:?} in {took} ns"
          ));
        }
        if answered() != before + 1 {
          outcome.push(format!("{case} not counted answered behind"));
        }
      }
      let before = answered();
      let reply = super::serve_remote((origin, other), &put(2, behind), budget).await;
      if outcome_of(&reply) != Some(HomeOutcome::Refused(HomeRefusal::NotAHome))
        || answered() != before
      {
        outcome.push(format!(
          "(c) not refused NotAHome at once: {:?}",
          outcome_of(&reply)
        ));
      }
      install(ahead, vec![local]);
      let reply = super::serve_remote((origin, other), &put(0, ahead), budget).await;
      if !matches!(outcome_of(&reply), Some(HomeOutcome::Kept { .. })) {
        outcome.push(format!(
          "(d) not kept once installed: {:?}",
          outcome_of(&reply)
        ));
      }
      outcome
    },
    std::time::Duration::from_secs(60),
  );
  assert!(outcome.is_empty(), "{outcome:#?}");
}
