//! The audit's crossing (§4.15 "written through the control shard", AC-2.10; A-265): a record made on a shard other
//! than the control shard is owed in the atom that made it, kept in that shard's partition until the control partition
//! has appended it, and sent again until then. Before 2026-11-18 it lived in the shard's memory between its atom's
//! commit and its batch, so a daemon that died there lost it, and a batch sent twice would have been appended twice.
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

use std::time::{Duration, Instant};

use slates_client::{AuditEntry, Client, Filter};
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::anchor::{held_anchor, source_of};
use common::client::{connect, scratch};
use common::target::target_dir;

/// Shape: two shards, so a volume is owned by a shard that is not the control shard.
const SHARDS: u16 = 2;
/// Shape: how long a record owed across shards may take to reach the audit: the reap tick that sends it again runs
/// once a liveness budget (a second), so this is many ticks on a loaded machine.
const CROSSING_WAIT: Duration = Duration::from_secs(30);
/// Shape: the pause between a polling test's reads: a hundredth of the reap tick's second, so a wait ends soon after
/// its condition holds without a read of the daemon's status on every turn of the thread.
const POLL: Duration = Duration::from_millis(10);

/// A volume named so that the shard that is not the control shard owns it, its landing presented into an empty target:
/// the presentation's `landing_planned` record is owed across shards.
fn present_off_the_control_shard(client: &mut Client, tag: &str) {
  let name = (0..)
    .map(|n| format!("{tag}-{n}"))
    .find(|name| slates_server::verbs::owner_of_name(name, usize::from(SHARDS)) != 0)
    .unwrap();
  let volume = client.create(&scratch(&name)).unwrap();
  let target = target_dir();
  client
    .land(volume, None, &target.path, Filter::default(), None)
    .unwrap();
}

/// The `landing_planned` records of an audit read.
fn planned(records: &[AuditEntry]) -> usize {
  records
    .iter()
    .filter(|record| record.kind == "landing_planned")
    .count()
}

/// The audit, read until `done` holds of it or [`CROSSING_WAIT`] passes: the last read.
fn audit_until(client: &mut Client, done: impl Fn(&[AuditEntry]) -> bool) -> Vec<AuditEntry> {
  let started = Instant::now();
  loop {
    let records = client.audit(0).unwrap();
    if done(&records) || started.elapsed() > CROSSING_WAIT {
      return records;
    }
    std::thread::sleep(POLL);
  }
}

/// The status counter `kind`, summed over the shards.
fn counted(client: &mut Client, kind: &str) -> u64 {
  client
    .daemon_status()
    .unwrap()
    .shards
    .iter()
    .flat_map(|shard| shard.refusals.iter())
    .filter(|refusal| refusal.kind == kind)
    .map(|refusal| refusal.count)
    .sum()
}

/// AC-2.10, §4.15 (A-265's crossing). Do: on a two-shard daemon over an anchor the test holds, withhold the audit's
/// crossing; present the landing of a volume the shard that is not the control shard owns; read the audit; stop the
/// daemon and start another over the same anchor; read the audit until it holds the presentation's record. Expect the
/// first read without it, since the crossing was withheld, and after the restart the record once, within the wait: it
/// was durable in the owing shard's partition from its atom on, and the new daemon's reap tick sent it. Before
/// 2026-11-18 it lived in the shard's memory between its atom's commit and its batch, and the restart lost it.
#[test]
fn a_record_owed_across_shards_survives_a_restart_before_its_batch() {
  let profile = common::machine_profile();
  let instance = format!("srv-audit-crossing-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  let anchor = held_anchor("audit-crossing", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&anchor)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  first.withhold_audit_crossing(true).unwrap();
  let mut client = connect(&instance);
  present_off_the_control_shard(&mut client, "owed-across");
  let before = client.audit(0).unwrap();
  first.stop();
  let second = Daemon::start(&profile, config, source_of(&anchor)).unwrap();
  let after = audit_until(&mut client, |records| planned(records) > 0);
  drop(client);
  second.stop();
  assert_eq!(
    planned(&before),
    0,
    "the crossing was withheld, so the owed record had not reached the audit: {before:?}"
  );
  assert_eq!(
    planned(&after),
    1,
    "the record owed before the restart reached the audit once after it: {after:?}"
  );
}

/// AC-2.10, §4.15 (A-265's crossing). Do: on a two-shard daemon, withhold the record of the control shard's
/// acknowledgements; present the landing of a volume the shard that is not the control shard owns; wait until the reap
/// tick has sent the owed record again twice; then let acknowledgements be recorded and wait for one more send. Expect
/// the presentation's record in the audit at once (its reply waits for the append), the resends counted, and the record
/// still there exactly once: the control partition appended nothing a second time. Before 2026-11-18 nothing was sent
/// again, and a batch appended again would have been a second record.
#[test]
fn a_batch_sent_again_after_a_lost_acknowledgement_appends_nothing_twice() {
  let profile = common::machine_profile();
  let instance = format!("srv-audit-resend-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  let anchor = held_anchor("audit-resend", &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&anchor)).unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  daemon.withhold_audit_acknowledgement(true).unwrap();
  let mut client = connect(&instance);
  present_off_the_control_shard(&mut client, "resent");
  let at_once = client.audit(0).unwrap();
  let started = Instant::now();
  while counted(&mut client, slates_server::landing::AUDIT_RESENT) < 2 {
    assert!(
      started.elapsed() < CROSSING_WAIT,
      "the reap tick sent the unacknowledged record again within {CROSSING_WAIT:?}"
    );
    std::thread::sleep(POLL);
  }
  daemon.withhold_audit_acknowledgement(false).unwrap();
  let resent = counted(&mut client, slates_server::landing::AUDIT_RESENT);
  let started = Instant::now();
  while counted(&mut client, slates_server::landing::AUDIT_RESENT) == resent
    && started.elapsed() < CROSSING_WAIT
  {
    std::thread::sleep(POLL);
  }
  let after = client.audit(0).unwrap();
  drop(client);
  daemon.stop();
  assert_eq!(
    planned(&at_once),
    1,
    "the presentation's reply waited for its record: {at_once:?}"
  );
  assert_eq!(
    planned(&after),
    1,
    "a batch sent again appended nothing twice: {after:?}"
  );
}
