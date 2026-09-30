//! The landing lease as the engine keeps it (§4.15 step 4; AUD-29-03): the caller holds the lease on the
//! target's canonical identity — its one host-local owner's record, which the server takes from the control
//! shard — and the engine writes only under a live lease for this very target, fencing every entry by the
//! lease's term, so a holder paused past its term starts no entry once another may hold the target.
//!
//! Until 2026-09-29 the engine took the lease itself from a table its caller kept per shard, keyed by the
//! target's path string: two shards each held their own, and an alias of the path was another key
//! (docs/bugs/2026-09-29-a-target-landing-lease-was-per-shard-and-per-path.md).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use slates_land::engine::{
  LandingRefusal, LandingRequest, LandingState, LandingTarget, Observer, Outcome, SkipReason,
  Unobserved, land,
};
use slates_land::grant::{GrantScope, Leases, Surface};
use slates_land::manifest::LandingEntry;
use slates_vfs::base::BaseConfig;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{
  LARGE, Session, Setup, TERM_NS, config, request, store, target_lease_key, write_file,
};

/// Shape: a short lease term for the paused-holder history, in nanoseconds: long enough that the first
/// entry's fence passes, short against the pause below.
const SHORT_TERM_NS: u64 = 50_000_000;
/// Shape: how long the paused holder pauses before its first write: twice the short term, so the term has
/// ended by the next entry.
const PAUSE: Duration = Duration::from_nanos(2 * SHORT_TERM_NS);

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    config(),
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

/// A disk with two files and an overlay that replaces both.
fn two_files() -> (SimHost, Store, Volume, LandingTarget) {
  let mut host = SimHost::new();
  host.replace_file("/a", b"old a");
  host.replace_file("/b", b"old b");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  write_file(&mut vol, &mut host, &mut store, "/a", b"new a");
  write_file(&mut vol, &mut host, &mut store, "/b", b"new b");
  let target = LandingTarget {
    dir: host.root(),
    key: "/".into(),
  };
  (host, store, vol, target)
}

/// A granted request for `two_files`' landing: presents it and issues a single-use grant.
fn granted(
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
  target: &LandingTarget,
  session: &mut Session,
) -> LandingRequest {
  let presented = Setup {
    host,
    target,
    vol,
    store,
    session,
  }
  .present(&request(1));
  LandingRequest {
    grant: session.grants.issue(
      Surface::Cli,
      presented.manifest.hash,
      presented.binding.clone(),
      GrantScope::Once,
      1,
      TERM_NS,
    ),
    ..request(1)
  }
}

/// AUD-29-03. Do: land a granted plan with no lease, with a live lease on another target's key, and with
/// this target's lease already expired. Expect: each is refused `LeaseRequired` and nothing is written; with
/// this target's live lease the landing lands.
#[test]
fn a_landing_writes_only_under_a_live_lease_on_its_own_target() {
  let (mut host, mut store, mut vol, target) = two_files();
  let mut session = Session::new();
  let req = granted(&mut host, &mut store, &mut vol, &target, &mut session);
  let key = target_lease_key(&mut host, &target);
  let mut leases = Leases::default();
  let elsewhere = leases
    .take("0000000000000000:0000000000000001", 7, 1, TERM_NS)
    .unwrap();
  let expired = leases.take(&key, 7, 0, 1).unwrap();
  for (lease, what) in [
    (None, "no lease"),
    (Some(&elsewhere), "another target's lease"),
    (Some(&expired), "an expired lease"),
  ] {
    let refused = land(
      &mut host,
      &target,
      &mut vol,
      &mut store,
      &mut session.grants,
      lease,
      &mut session.audit,
      &req,
      &mut Unobserved,
    );
    assert!(
      matches!(refused, Err(LandingRefusal::LeaseRequired)),
      "{what}: {refused:?}"
    );
    assert_eq!(host.bytes("/a").unwrap(), b"old a", "{what} wrote");
    assert_eq!(host.bytes("/b").unwrap(), b"old b", "{what} wrote");
  }
  leases.release(&expired);
  let live = leases.take(&key, 7, req.now_ns, TERM_NS).unwrap();
  let report = land(
    &mut host,
    &target,
    &mut vol,
    &mut store,
    &mut session.grants,
    Some(&live),
    &mut session.audit,
    &req,
    &mut Unobserved,
  )
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(host.bytes("/b").unwrap(), b"new b");
}

/// A holder that pauses past its lease's term before its first write.
struct Pause {
  paused: bool,
}

impl Observer<SimHost> for Pause {
  fn before_write(&mut self, _host: &mut SimHost, _entry: &LandingEntry) {
    if !self.paused {
      self.paused = true;
      // The paused holder: a test harness stands in for a descheduled landing.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(PAUSE);
    }
  }
}

/// AUD-29-03 (a paused old holder). Do: land two replacements under a lease whose term is shorter than a
/// pause the holder takes before its first write. Expect: the entry it had started lands, the next is
/// `Skipped(LeaseEnded)` and left as it was on the disk, the landing is `Partial` and keeps the skipped entry
/// in the overlay; a new holder's lease then lands the rest.
#[test]
fn a_holder_paused_past_its_term_starts_no_entry_after_it() {
  let (mut host, mut store, mut vol, target) = two_files();
  let mut session = Session::new();
  let req = granted(&mut host, &mut store, &mut vol, &target, &mut session);
  let key = target_lease_key(&mut host, &target);
  let mut leases = Leases::default();
  let short = leases.take(&key, 7, req.now_ns, SHORT_TERM_NS).unwrap();
  let report = land(
    &mut host,
    &target,
    &mut vol,
    &mut store,
    &mut session.grants,
    Some(&short),
    &mut session.audit,
    &req,
    &mut Pause { paused: false },
  )
  .unwrap();
  let outcomes: Vec<_> = report.entries.iter().map(|e| e.outcome.clone()).collect();
  assert_eq!(
    outcomes,
    vec![
      Some(Outcome::Written),
      Some(Outcome::Skipped(SkipReason::LeaseEnded))
    ],
    "{report:?}"
  );
  assert_eq!(report.state, LandingState::Partial);
  assert_eq!(host.bytes("/a").unwrap(), b"new a");
  assert_eq!(
    host.bytes("/b").unwrap(),
    b"old b",
    "no entry started after the term"
  );
  leases.release(&short);
  let resumed = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .land(request(2))
  .unwrap();
  assert_eq!(resumed.state, LandingState::Done, "{resumed:?}");
  assert_eq!(host.bytes("/b").unwrap(), b"new b");
}
