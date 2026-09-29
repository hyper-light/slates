//! A grant authorizes exactly the landing a human approved (§4.13 "Grants": the daemon verifies "the exact
//! manifest hash, target identity, intended consumer, scope and validity"; §4.15 step 3; R1, R10; AUD-29-01).
//!
//! Until 2026-09-29 a grant carried only its manifest's hash, and the check at use compared nothing else. A
//! plan's manifest names its entries relative to the target, so a create-only plan has the same manifest in
//! every empty directory and from every volume with the same content: a grant approved for one landing
//! landed another directory, another volume or another consumer's landing, and a session grant covered any
//! plan on its shard (docs/bugs/2026-09-29-a-grant-did-not-bind-its-target-volume-or-consumer.md).
//!
//! Each test approves one landing, then tries the same grant on a landing that differs in one bound field
//! and plans the **same manifest**, so manifest equality cannot hide the defect; every such landing must
//! refuse naming that field and write nothing. Each test also lands the approved landing itself, so a
//! grant refused for every landing cannot pass as a binding one.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use slates_land::engine::{
  LandingRefusal, LandingRequest, LandingState, LandingTarget, Unobserved,
};
use slates_land::grant::{BindingField, GrantRefusal, GrantScope, Surface};
use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{HostFs, HostKind};
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{Session, Setup, TERM_NS, request, scratch, store, write_file};

/// The disk as path → (kind, bytes): what "wrote nothing" is compared against.
fn disk(host: &SimHost) -> BTreeMap<String, (HostKind, Vec<u8>)> {
  host
    .paths()
    .into_iter()
    .map(|(path, kind)| {
      let bytes = host.bytes(&path).unwrap_or_default();
      (path, (kind, bytes))
    })
    .collect()
}

/// A target at `/name`, opened as the landing opens it.
fn target(host: &mut SimHost, name: &str) -> LandingTarget {
  let root = host.root();
  LandingTarget {
    dir: host.open_dir(root, name).unwrap(),
    key: format!("/{name}").into(),
  }
}

/// A scratch volume holding one file: the same content, so the same create-only manifest, whatever target
/// or volume id it is landed under.
fn volume_with_note(host: &mut SimHost, store: &mut Store) -> Volume {
  let mut vol = scratch(store);
  write_file(&mut vol, host, store, "/note", b"approved bytes");
  vol
}

/// Lands `req` into `target` and expects a refusal naming `field`, with the disk unchanged.
fn assert_unbound(
  host: &mut SimHost,
  target: &LandingTarget,
  vol: &mut Volume,
  store: &mut Store,
  session: &mut Session,
  req: &LandingRequest,
  field: BindingField,
) {
  let before = disk(host);
  let outcome = Setup {
    host,
    target,
    vol,
    store,
    session,
  }
  .try_land(req, &mut Unobserved);
  assert_eq!(
    outcome.err(),
    Some(LandingRefusal::Grant(GrantRefusal::Unbound { field })),
    "a grant bound elsewhere must refuse on its {} before writing",
    field.name()
  );
  assert_eq!(disk(host), before, "a refused landing wrote to the host");
}

/// AUD-29-01. Do: approve a single-use landing of volume V by consumer C into `/a`, then try the grant on
/// the same plan into `/b`, from another volume, by another consumer, at another snapshot, and into a
/// directory that replaced `/a` at its path. Expect: each refuses naming the differing field and writes
/// nothing; the approved landing itself lands (so the refusals are the binding's, not a dead grant's).
#[test]
fn a_single_use_grant_lands_only_its_target_volume_consumer_and_snapshot() {
  let mut host = SimHost::new();
  host.mkdir("/a");
  host.mkdir("/b");
  let mut store = store();
  let mut vol = volume_with_note(&mut host, &mut store);
  let mut session = Session::new();
  let a = target(&mut host, "a");
  let b = target(&mut host, "b");
  let presented = Setup {
    host: &mut host,
    target: &a,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .present(&request(1));
  let grant = session.grants.issue(
    Surface::Cli,
    presented.manifest.hash,
    presented.binding.clone(),
    GrantScope::Once,
    1,
    TERM_NS,
  );
  let granted = LandingRequest {
    grant,
    ..request(1)
  };
  // Retargeted: the same plan into another empty directory has the same manifest.
  assert_unbound(
    &mut host,
    &b,
    &mut vol,
    &mut store,
    &mut session,
    &granted,
    BindingField::Target,
  );
  // Another volume with the same content.
  let other_volume = LandingRequest {
    volume: [9; 16],
    ..granted.clone()
  };
  assert_unbound(
    &mut host,
    &a,
    &mut vol,
    &mut store,
    &mut session,
    &other_volume,
    BindingField::Volume,
  );
  // Another consumer presenting the grant id.
  let other_consumer = LandingRequest {
    consumer: b"consumer-two".as_slice().into(),
    ..granted.clone()
  };
  assert_unbound(
    &mut host,
    &a,
    &mut vol,
    &mut store,
    &mut session,
    &other_consumer,
    BindingField::Consumer,
  );
  // Another snapshot of the same volume.
  let other_snapshot = LandingRequest {
    snapshot: 2,
    ..granted.clone()
  };
  assert_unbound(
    &mut host,
    &a,
    &mut vol,
    &mut store,
    &mut session,
    &other_snapshot,
    BindingField::Snapshot,
  );
  // The approved landing lands.
  let report = Setup {
    host: &mut host,
    target: &a,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .try_land(&granted, &mut Unobserved)
  .unwrap();
  assert_eq!(report.state, LandingState::Done, "{report:?}");
  assert_eq!(host.bytes("/a/note").unwrap(), b"approved bytes");
}

/// AUD-29-01, the replaced target. Do: approve a landing into `/a`, move that directory away and make a new
/// empty `/a` in its place, then land with the grant into the new `/a`. Expect: refused on the target
/// (another directory at the approved path), nothing written; the moved directory is untouched.
#[test]
fn a_grant_does_not_follow_its_path_to_a_directory_that_replaced_the_target() {
  let mut host = SimHost::new();
  host.mkdir("/a");
  let mut store = store();
  let mut vol = volume_with_note(&mut host, &mut store);
  let mut session = Session::new();
  let approved = target(&mut host, "a");
  let presented = Setup {
    host: &mut host,
    target: &approved,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .present(&request(1));
  let grant = session.grants.issue(
    Surface::Cli,
    presented.manifest.hash,
    presented.binding.clone(),
    GrantScope::Once,
    1,
    TERM_NS,
  );
  host.rename("/a", "/a-approved");
  host.mkdir("/a");
  let replaced = target(&mut host, "a");
  let granted = LandingRequest {
    grant,
    ..request(1)
  };
  assert_unbound(
    &mut host,
    &replaced,
    &mut vol,
    &mut store,
    &mut session,
    &granted,
    BindingField::Target,
  );
  assert!(host.bytes("/a-approved/note").is_none());
}

/// AUD-29-01, the session grant (§4.15 step 3: "a session grant covers later landings of the same volume
/// into the same target for the session"). Do: approve a session grant for consumer C landing V into `/a`;
/// land it; change the file and land again under the same grant; then try the grant from another consumer,
/// another volume and into `/b`. Expect: both of C's landings of V into `/a` land (the second with a new
/// manifest); every other landing refuses naming its field and writes nothing.
#[test]
fn a_session_grant_covers_its_consumer_volume_and_target_and_nothing_wider() {
  let mut host = SimHost::new();
  host.mkdir("/a");
  host.mkdir("/b");
  let mut store = store();
  let mut vol = volume_with_note(&mut host, &mut store);
  let mut session = Session::new();
  let a = target(&mut host, "a");
  let b = target(&mut host, "b");
  let presented = Setup {
    host: &mut host,
    target: &a,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .present(&request(1));
  let grant = session.grants.issue(
    Surface::Confirmation,
    presented.manifest.hash,
    presented.binding.clone(),
    GrantScope::Session,
    1,
    TERM_NS,
  );
  let granted = LandingRequest {
    grant,
    ..request(1)
  };
  for (round, bytes) in [(1, b"approved bytes".as_slice()), (2, b"later bytes")] {
    if round > 1 {
      write_file(&mut vol, &mut host, &mut store, "/note", bytes);
    }
    let report = Setup {
      host: &mut host,
      target: &a,
      vol: &mut vol,
      store: &mut store,
      session: &mut session,
    }
    .try_land(
      &LandingRequest {
        snapshot: round,
        ..granted.clone()
      },
      &mut Unobserved,
    )
    .unwrap();
    assert_eq!(
      report.state,
      LandingState::Done,
      "round {round}: {report:?}"
    );
    assert_eq!(host.bytes("/a/note").unwrap(), bytes, "round {round}");
  }
  let other_consumer = LandingRequest {
    consumer: b"consumer-two".as_slice().into(),
    ..granted.clone()
  };
  assert_unbound(
    &mut host,
    &a,
    &mut vol,
    &mut store,
    &mut session,
    &other_consumer,
    BindingField::Consumer,
  );
  let other_volume = LandingRequest {
    volume: [9; 16],
    ..granted.clone()
  };
  assert_unbound(
    &mut host,
    &a,
    &mut vol,
    &mut store,
    &mut session,
    &other_volume,
    BindingField::Volume,
  );
  assert_unbound(
    &mut host,
    &b,
    &mut vol,
    &mut store,
    &mut session,
    &granted,
    BindingField::Target,
  );
}
