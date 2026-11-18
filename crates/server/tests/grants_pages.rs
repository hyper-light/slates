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
/// Expect: `Target { TooLong }` naming the longest the daemon takes, before the daemon touches the host or presents
/// anything: a landing that could be granted is one whose grant can be listed. Typed since main's A-299 sweep made
/// `TargetUnavailable`'s words a protocol fault (2026-11-18); it carried both lengths in words before.
#[test]
fn a_target_longer_than_a_grants_page_holds_is_refused_before_presenting() {
  let (daemon, instance) = daemon("long-target");
  let mut client = connect(&instance);
  let volume = client.create(&scratch("long-target")).unwrap();
  let longest = slates_server::landing::longest_grantable_target();
  eprintln!("the longest grantable target: {longest} bytes");
  let target = format!("/{}", "t".repeat(longest));
  match client.land(volume, None, &target, Filter::default(), None) {
    Err(ClientError::Refused(Refusal::Target {
      refusal: slates_ipc::protocol::TargetRefusal::TooLong { longest: named },
    })) => {
      assert_eq!(
        usize::try_from(named).ok(),
        Some(longest),
        "the refusal names the longest target the daemon takes"
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

/// Shape: the volumes per shard whose landings the expiry audit test grants: a few on each of the two shards, so grants
/// end on the control shard, whose audit record goes in the expiry's own atom, and on the other, whose records are owed
/// to the control partition, several of them in one sweep.
const EXPIRING_PER_SHARD: usize = 3;
/// Shape: the grants landed with past their term in the check test: several, so at least one landing's check meets its
/// grant before the reap's next sweep (the sweep runs once a liveness budget, a second; each landing follows its grant's
/// term by well under a millisecond).
const LATE_LANDINGS: usize = 4;

/// Presents `volume`'s landing into `target` and approves it with a single-use grant whose term is [`SHORT_TERM_NS`]:
/// the grant.
fn short_grant(
  client: &mut slates_client::Client,
  secret: &[u8; 32],
  volume: slates_client::VolumeId,
  target: &str,
) -> u64 {
  let Ok(slates_client::Landing::GrantRequired {
    landing, manifest, ..
  }) = client.land(volume, None, target, Filter::default(), None)
  else {
    panic!("the landing was presented");
  };
  let scope = slates_ipc::protocol::GrantScope::Once;
  let proof = slates_server::landing::grant_proof(secret, landing, &manifest, scope, SHORT_TERM_NS);
  client
    .grant(landing, manifest, scope, SHORT_TERM_NS, proof)
    .unwrap()
}

/// Waits until none of `grants` is listed, each ended at its term, within [`EXPIRY_WAIT`].
fn wait_until_unlisted(client: &mut slates_client::Client, grants: &[u64]) {
  let started = std::time::Instant::now();
  loop {
    let listed: BTreeSet<u64> = client
      .grants()
      .unwrap()
      .iter()
      .map(|grant| grant.id)
      .collect();
    let left: Vec<u64> = grants
      .iter()
      .copied()
      .filter(|grant| listed.contains(grant))
      .collect();
    if left.is_empty() {
      return;
    }
    assert!(
      started.elapsed() < EXPIRY_WAIT,
      "grants {left:?} left the table within {EXPIRY_WAIT:?} of their term"
    );
    std::thread::yield_now();
  }
}

/// The grants of `grants` whose `grant_expired` record does not follow their `grant_issued` in the audit `records`.
fn unaudited_expiries(records: &[slates_client::AuditEntry], grants: &[u64]) -> Vec<u64> {
  let position = |kind: &str, grant: u64| {
    records
      .iter()
      .position(|record| record.kind == kind && record.grant == Some(grant))
  };
  grants
    .iter()
    .copied()
    .filter(|grant| {
      !matches!(
        (position("grant_issued", *grant), position("grant_expired", *grant)),
        (Some(issued), Some(expired)) if issued < expired
      )
    })
    .collect()
}

/// The status counter `kind` on each partition.
fn counted_by_partition(
  client: &mut slates_client::Client,
  kind: &str,
) -> std::collections::BTreeMap<u16, u64> {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .map(|shard| {
      let counted = shard
        .refusals
        .iter()
        .filter(|refusal| refusal.kind == kind)
        .map(|refusal| refusal.count)
        .sum();
      (shard.partition, counted)
    })
    .collect()
}

/// §4.15, AC-2.10 (A-266). Do: on a two-shard daemon present the landings of three volumes on each shard and approve
/// each with a single-use grant whose term is one millisecond; wait until the reap has ended them all; read the audit
/// and the shards' counters. Expect each grant's `grant_expired` record after its `grant_issued`, and expiries counted
/// on both shards, so the record the control shard writes in the expiry's own atom and the ones the other shard owes the
/// control partition were both read. Before 2026-11-18 an expiry wrote no audit record: an expired grant's history was
/// its issue alone.
#[test]
fn every_grant_ended_at_its_term_leaves_its_expiry_in_the_audit() {
  let (daemon, instance) = daemon("expiry-audit");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut grants = Vec::new();
  for partition in 0..TEST_SHARDS {
    let names: Vec<String> = (0..)
      .map(|n| format!("expiring-{n}"))
      .filter(|name| {
        slates_server::verbs::owner_of_name(name, usize::from(TEST_SHARDS)) == partition
      })
      .take(EXPIRING_PER_SHARD)
      .collect();
    for name in names {
      let volume = client.create(&scratch(&name)).unwrap();
      grants.push(short_grant(&mut client, &secret, volume, &target.path));
    }
  }
  wait_until_unlisted(&mut client, &grants);
  let audit = client.audit(0).unwrap();
  let expired = counted_by_partition(&mut client, slates_server::landing::GRANTS_EXPIRED);
  drop(client);
  daemon.stop();
  let unaudited = unaudited_expiries(&audit, &grants);
  assert!(
    unaudited.is_empty(),
    "every grant ended at its term has its grant_expired record after its grant_issued; these do not: {unaudited:?}"
  );
  assert!(
    (0..TEST_SHARDS).all(|partition| expired.get(&partition).is_some_and(|count| *count > 0)),
    "grants ended at their term on both shards: {expired:?}"
  );
}

/// §4.15 (A-266). Do: present four landings and approve each with a single-use grant whose term is one millisecond,
/// and land with each once its term has passed, before the reap's next sweep can end it; then wait for the grants to
/// leave the table and read the audit. Expect each landing refused `GrantInvalid`, every grant gone from the listing and
/// each one's `grant_expired` record in the audit. Before 2026-11-18 the landing's check marked its grant expired in the
/// shard's table alone, which the sweep passed over: the grant stayed listed as issued, in the shard's table until a
/// restart, and its expiry was never recorded.
#[test]
fn a_grant_a_landing_found_past_its_term_still_ends_and_is_audited() {
  let (daemon, instance) = daemon("late-landing");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let mut grants = Vec::new();
  for n in 0..LATE_LANDINGS {
    let volume = client.create(&scratch(&format!("late-{n}"))).unwrap();
    let grant = short_grant(&mut client, &secret, volume, &target.path);
    let granted = std::time::Instant::now();
    while granted.elapsed() <= std::time::Duration::from_nanos(SHORT_TERM_NS) {
      std::hint::spin_loop();
    }
    let refused = client.land(volume, None, &target.path, Filter::default(), Some(grant));
    assert!(
      matches!(refused, Err(ClientError::Refused(Refusal::GrantInvalid))),
      "a grant past its term is refused as a grant that is not valid: {refused:?}"
    );
    grants.push(grant);
  }
  wait_until_unlisted(&mut client, &grants);
  let audit = client.audit(0).unwrap();
  drop(client);
  daemon.stop();
  let unaudited = unaudited_expiries(&audit, &grants);
  assert!(
    unaudited.is_empty(),
    "every grant a landing found past its term has its expiry audited; these do not: {unaudited:?}"
  );
}

/// AC-2.3 (§4.8 transactions) and A-266 (the table holds live grants). Do: present a volume's landing twice; approve the
/// first; approve the second with the shard's next publication refused before its append, so the grant's atom rolls
/// back; then land the second presentation naming the id the refused issue took, one past the first grant's. Expect the
/// approval refused `Unpublished`, one rollback counted, and the landing refused `GrantInvalid`: no grant stands that the
/// refusal said was not made. Before 2026-11-18 the shard's own table kept the grant the rolled-back atom issued, bound
/// to the presented landing, and a landing naming its id ran under it.
#[test]
fn a_grant_whose_issue_was_refused_covers_no_landing() {
  let (daemon, instance) = daemon("refused-issue");
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let volume = client.create(&scratch("refused-issue")).unwrap();
  let granted = common::landing::approve(&mut client, &secret, volume, None, &target.path);
  let Ok(slates_client::Landing::GrantRequired {
    landing, manifest, ..
  }) = client.land(volume, None, &target.path, Filter::default(), None)
  else {
    panic!("the second landing was presented");
  };
  let scope = slates_ipc::protocol::GrantScope::Once;
  let term = common::landing::GRANT_TERM_NS;
  let proof = slates_server::landing::grant_proof(&secret, landing, &manifest, scope, term);
  daemon
    .inject_publication_fault(Some(slates_db::replay::PublicationFault::BeforeAppend))
    .unwrap();
  let refused = client.grant(landing, manifest, scope, term, proof);
  daemon.inject_publication_fault(None).unwrap();
  let rollbacks: u64 = daemon
    .db_publication_counters()
    .unwrap()
    .iter()
    .map(|counters| counters.rollbacks)
    .sum();
  let landed = client.land(
    volume,
    None,
    &target.path,
    Filter::default(),
    Some(granted + 1),
  );
  drop(client);
  daemon.stop();
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::Unpublished { .. }))
    ),
    "the approval is refused, its atom rolled back: {refused:?}"
  );
  assert_eq!(rollbacks, 1, "the grant's atom was the one rolled back");
  assert!(
    matches!(landed, Err(ClientError::Refused(Refusal::GrantInvalid))),
    "a landing naming the refused grant's id is refused as one with no valid grant: {landed:?}"
  );
}
