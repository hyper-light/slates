//! A principal's grants at the number a daemon holds (§4.15): every grant, however many. One reply is one bulk chunk,
//! so a list longer than one chunk is answered in pages the client walks. Before 2026-11-08 the whole list was one
//! reply, refused "reply too large for the bulk area" between 25 and 50 grants.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]
// A landing presented into a real target directory: macOS and Linux.
#![cfg(unix)]

use std::collections::BTreeSet;

use slates_client::{ClientError, Filter, Refusal};
use slates_ipc::protocol::{ReplyBody, RequestBody};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::target::target_dir;

/// Shape: two shards, so the grants are gathered across shards.
const TEST_SHARDS: u16 = 2;
/// Shape: the grants issued: about ten replies' worth.
const GRANTS: usize = 300;

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-grants-pages-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-grants-pages-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// §4.15. Do: on a two-shard daemon present a landing and approve it 300 times (an empty volume into an empty target,
/// so no grant writes anything), then list the caller's grants. Expect: every issued grant exactly once, in id order.
/// Before 2026-11-08 the list was refused `BadRequest { reason: "reply too large for the bulk area" }`.
#[test]
fn a_principal_lists_every_grant_it_holds_however_many() {
  let (daemon, instance) = daemon("many");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let volume = client.create(&scratch("granted")).unwrap();
  let issued: BTreeSet<u64> = (0..GRANTS)
    .map(|_| common::landing::approve(&mut client, &secret, volume, None, &target.path))
    .collect();
  let listed = client.grants().expect("every grant");
  let ids: Vec<u64> = listed.iter().map(|grant| grant.id).collect();
  assert!(
    ids.windows(2).all(|pair| pair[0] < pair[1]),
    "each once, in id order"
  );
  assert_eq!(
    ids.into_iter().collect::<BTreeSet<_>>(),
    issued,
    "exactly the issued grants"
  );
  drop(client);
  daemon.stop();
}

/// §4.15. Do: issue 300 grants and walk the list a page at a time, issuing one more grant before each page after the
/// first. Expect: more than one page, every grant issued before the walk listed exactly once, none twice.
#[test]
fn a_grant_issued_before_the_walk_is_listed_exactly_once() {
  let (daemon, instance) = daemon("churn");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let volume = client.create(&scratch("granted")).unwrap();
  let issued: BTreeSet<u64> = (0..GRANTS)
    .map(|_| common::landing::approve(&mut client, &secret, volume, None, &target.path))
    .collect();
  let mut listed: Vec<u64> = Vec::new();
  let mut after = None;
  let mut pages = 0usize;
  loop {
    if pages > 0 {
      common::landing::approve(&mut client, &secret, volume, None, &target.path);
    }
    let ReplyBody::Grants { grants, next } = client.call(&RequestBody::Grants { after }).unwrap()
    else {
      panic!("a grants page");
    };
    pages += 1;
    listed.extend(grants.iter().map(|grant| grant.id));
    match next {
      Some(next) => after = Some(next),
      None => break,
    }
  }
  assert!(pages > 1, "the list took {pages} page(s): it must page");
  let seen: BTreeSet<u64> = listed.iter().copied().collect();
  assert_eq!(seen.len(), listed.len(), "no grant listed twice");
  assert!(
    issued.is_subset(&seen),
    "every grant issued before the walk is listed"
  );
  drop(client);
  daemon.stop();
}

/// §4.15, §4.3. Do: present a landing whose target is one byte longer than a grant's summary can carry on one page.
/// Expect: `TargetUnavailable` naming both lengths, before the daemon touches the host or presents anything: a landing
/// that could be granted is one whose grant can be listed.
#[test]
fn a_target_longer_than_a_grants_page_holds_is_refused_before_presenting() {
  let (daemon, instance) = daemon("long-target");
  let mut client = connect(&instance);
  let volume = client.create(&scratch("long-target")).unwrap();
  let longest = slates_server::landing::longest_grantable_target();
  eprintln!("the longest grantable target: {longest} bytes");
  let target = format!("/{}", "t".repeat(longest));
  match client.land(volume, None, &target, Filter::default(), None) {
    Err(ClientError::Refused(Refusal::TargetUnavailable { reason })) => {
      assert!(
        reason.contains(&target.len().to_string()) && reason.contains(&longest.to_string()),
        "the refusal names both lengths: {reason}"
      );
    }
    other => panic!("a {}-byte target: {other:?}", target.len()),
  }
  drop(client);
  daemon.stop();
}

/// Shape: the grants used up in the retention test: past a page, so a table that kept them would list them.
const CONSUMED: usize = 60;

/// Lands `grant` (an empty volume into an empty target: done, nothing written), which consumes it.
fn land_with(
  client: &mut slates_client::Client,
  volume: slates_client::VolumeId,
  target: &str,
  grant: u64,
) {
  match client.land(volume, None, target, Filter::default(), Some(grant)) {
    Ok(slates_client::Landing::Landed(outcome)) => assert_eq!(outcome.state, "done", "{outcome:?}"),
    other => panic!("the granted landing: {other:?}"),
  }
}

/// §4.15 (A-266). Do: issue and use up 60 single-use grants (each lands an empty volume into an empty target), then
/// list the caller's grants, read the audit, and present a used-up grant again. Expect: the list holds no used-up
/// grant: the table holds live grants, and a grant leaves it at its terminal step; every used-up grant's
/// `grant_issued` record stays in the audit, which is its history; and the used-up grant is still refused
/// `GrantInvalid`, never taken for a presentation. Before 2026-11-08 every grant ever issued stayed in the table and
/// was listed for the daemon's life.
#[test]
fn a_used_up_grant_leaves_the_table_and_its_history_stays_in_the_audit() {
  let (daemon, instance) = daemon("retention");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let volume = client.create(&scratch("used-up")).unwrap();
  let mut used = Vec::new();
  for _ in 0..CONSUMED {
    let grant = common::landing::approve(&mut client, &secret, volume, None, &target.path);
    land_with(&mut client, volume, &target.path, grant);
    used.push(grant);
  }
  let listed: BTreeSet<u64> = client
    .grants()
    .unwrap()
    .iter()
    .map(|grant| grant.id)
    .collect();
  assert!(
    used.iter().all(|grant| !listed.contains(grant)),
    "no used-up grant is listed: {} of {CONSUMED} were",
    used.iter().filter(|grant| listed.contains(grant)).count()
  );
  let audit = client.audit(0).unwrap();
  for grant in &used {
    assert!(
      audit
        .iter()
        .any(|record| record.kind == "grant_issued" && record.grant == Some(*grant)),
      "grant {grant}'s issue stays in the audit"
    );
  }
  let again = client.land(
    volume,
    None,
    &target.path,
    Filter::default(),
    used.first().copied(),
  );
  assert!(
    matches!(again, Err(ClientError::Refused(Refusal::GrantInvalid))),
    "a used-up grant is refused, not presented again: {again:?}"
  );
  drop(client);
  daemon.stop();
}

/// Shape: an expiring grant's term: one millisecond, so it is past its term by the next reap tick.
const SHORT_TERM_NS: u64 = 1_000_000;
/// Shape: how long the reap ticks may take to end an expired grant (the reaper runs at a cadence of well under a second).
const EXPIRY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// §4.15 (A-266). Do: present a landing and approve it with a single-use grant whose term is one millisecond; wait
/// for the reap. Expect: the grant leaves the table at its term (`grants` lists none) though nothing used it, and the
/// grant is refused `GrantInvalid` when it is presented after. Before 2026-11-08 an expired grant stayed in
/// the table for the daemon's life.
#[test]
fn an_unused_grant_leaves_the_table_at_its_term() {
  let (daemon, instance) = daemon("expiry");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let volume = client.create(&scratch("expiring")).unwrap();
  let Ok(slates_client::Landing::GrantRequired {
    landing, manifest, ..
  }) = client.land(volume, None, &target.path, Filter::default(), None)
  else {
    panic!("the landing was presented");
  };
  let scope = slates_ipc::protocol::GrantScope::Once;
  let proof =
    slates_server::landing::grant_proof(&secret, landing, &manifest, scope, SHORT_TERM_NS);
  let grant = client
    .grant(landing, manifest, scope, SHORT_TERM_NS, proof)
    .unwrap();
  let started = std::time::Instant::now();
  while client
    .grants()
    .unwrap()
    .iter()
    .any(|listed| listed.id == grant)
  {
    assert!(
      started.elapsed() < EXPIRY_WAIT,
      "grant {grant} left the table within {EXPIRY_WAIT:?} of its term"
    );
    std::thread::yield_now();
  }
  let refused = client.land(volume, None, &target.path, Filter::default(), Some(grant));
  assert!(
    matches!(refused, Err(ClientError::Refused(Refusal::GrantInvalid))),
    "an expired grant is refused as a grant that is not valid: {refused:?}"
  );
  drop(client);
  daemon.stop();
}
