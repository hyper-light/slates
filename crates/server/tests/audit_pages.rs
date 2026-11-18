//! The landing audit at the length a daemon's audit reaches (§4.15, §4.14): every record from a sequence on, however
//! many. One reply is one bulk chunk, so an audit longer than one chunk is answered in pages the client walks. Before
//! 2026-11-08 the whole audit was one reply, refused "reply too large for the bulk area" between 50 and 100 records.
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

use slates_client::{Filter, Landing};
use slates_ipc::protocol::Intent;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::target::target_dir;

/// Shape: one shard, whose audit records every presentation.
const TEST_SHARDS: u16 = 1;
/// Shape: the presentations made, each recording one `landing_planned` record: about five replies' worth.
const PRESENTATIONS: usize = 300;

fn daemon(tag: &str) -> (Daemon, String) {
  daemon_of(tag, TEST_SHARDS)
}

fn daemon_of(tag: &str, shards: u16) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-audit-pages-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(shards));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-audit-pages-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// §4.15, §4.14. Do: present one volume's landing 300 times (each into an empty target, so nothing is written; each
/// replaces the last and records `landing_planned`), then read the audit from 0 and from the middle. Expect: every
/// record once, in increasing sequence order, at least one `landing_planned` a presentation; and the read from the
/// middle is exactly the records from there. Before 2026-11-08 the read was refused
/// `BadRequest { reason: "reply too large for the bulk area" }`.
#[test]
fn the_audit_answers_every_record_however_many() {
  let (daemon, instance) = daemon("many");
  let mut client = connect(&instance);
  let target = target_dir();
  let volume = client.create(&scratch("audited")).unwrap();
  for n in 0..PRESENTATIONS {
    client
      .land(volume, None, &target.path, Filter::default(), None)
      .unwrap_or_else(|e| panic!("presentation {n}: {e}"));
  }
  let records = client.audit(0).expect("the whole audit");
  assert!(
    records.windows(2).all(|pair| pair[0].seq < pair[1].seq),
    "each record once, in sequence order"
  );
  let planned = records
    .iter()
    .filter(|record| record.kind == "landing_planned")
    .count();
  assert!(
    planned >= PRESENTATIONS,
    "{planned} landing_planned records for {PRESENTATIONS} presentations"
  );
  let middle = records[records.len() / 2].seq;
  let from_middle = client.audit(middle).unwrap();
  assert_eq!(
    from_middle,
    records
      .iter()
      .filter(|record| record.seq >= middle)
      .cloned()
      .collect::<Vec<_>>(),
    "a read from the middle holds exactly the records from there"
  );
  drop(client);
  daemon.stop();
}

/// Shape: the shards of the cross-shard audit test: the control shard, which holds the audit, and one more, which owns
/// the landed volume.
const CROSS_SHARDS: u16 = 2;
/// Shape: the clients that read the audit there: enough that at least two sit on different shards.
const READERS: usize = 4;
/// Shape: the presentations the cross-shard test makes before its granted landing.
const CROSS_PRESENTATIONS: usize = 20;
/// Shape: the files the cross-shard test's granted landing writes, each an `entry_written` record.
const CROSS_FILES: usize = 3;

/// What reader `n` read: each record once in sequence order, the presentations, the grant, the landing's writes, and
/// the grant before the landing's end.
fn every_record_once_in_order(n: usize, records: &[slates_client::AuditEntry]) {
  let count = |kind: &str| records.iter().filter(|record| record.kind == kind).count();
  assert!(
    records.windows(2).all(|pair| pair[0].seq < pair[1].seq),
    "reader {n}: each record once, in sequence order"
  );
  assert!(
    count("landing_planned") > CROSS_PRESENTATIONS,
    "reader {n}: {} landing_planned records of {} presentations",
    count("landing_planned"),
    CROSS_PRESENTATIONS + 1
  );
  assert_eq!(count("grant_issued"), 1, "reader {n}: the grant");
  assert_eq!(
    count("entry_written"),
    CROSS_FILES,
    "reader {n}: the landing's writes"
  );
  let issued = records.iter().position(|r| r.kind == "grant_issued");
  let finished = records.iter().position(|r| r.kind == "landing_finished");
  assert!(
    issued.is_some() && finished.is_some() && issued < finished,
    "reader {n}: the grant, then the landing's end: {records:?}"
  );
}

/// §4.15 "written through the control shard", §4.8's one `AuditLog` (A-265). Do: on a two-shard daemon, with a volume
/// owned by the shard that is not the control shard, present its landing 20 times, then grant and land it (three
/// files); at once after the landing's reply, read the audit from four clients, which sit on both shards. Expect: every
/// client reads every record: the 20 `landing_planned`, the `grant_issued`, the three `entry_written` and the
/// `landing_finished`, in sequence order, the grant before the landing's end. Before 2026-11-08 each shard kept its own
/// partition's audit and a read was answered from the client's own shard alone, so a client on the control shard read
/// none of the volume's records.
#[test]
fn a_client_on_any_shard_reads_every_shards_audit() {
  let (daemon, instance) = daemon_of("cross", CROSS_SHARDS);
  let mut client = connect(&instance);
  let name = (0..)
    .map(|n| format!("audited-elsewhere-{n}"))
    .find(|name| slates_server::verbs::owner_of_name(name, usize::from(CROSS_SHARDS)) != 0)
    .unwrap();
  let volume = client.create(&scratch(&name)).unwrap();
  let empty = target_dir();
  for n in 0..CROSS_PRESENTATIONS {
    client
      .land(volume, None, &empty.path, Filter::default(), None)
      .unwrap_or_else(|e| panic!("presentation {n}: {e}"));
  }
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  for n in 0..CROSS_FILES {
    client
      .fs_write((volume, attachment), &format!("f{n}.txt"), b"landed", 0o644)
      .unwrap();
  }
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = target_dir();
  let grant = common::landing::approve(&mut client, &secret, volume, None, &target.path);
  let landed = client.land(volume, None, &target.path, Filter::default(), Some(grant));
  assert!(
    matches!(&landed, Ok(Landing::Landed(outcome)) if outcome.written == CROSS_FILES as u64),
    "{landed:?}"
  );
  let mut readers: Vec<_> = (0..READERS).map(|_| connect(&instance)).collect();
  for (n, reader) in readers.iter_mut().enumerate() {
    every_record_once_in_order(n, &reader.audit(0).unwrap());
  }
  drop(readers);
  drop(client);
  daemon.stop();
}
