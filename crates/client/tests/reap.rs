//! T-2.3 (fault; §4.7 "Failure matrix", D-16): a client process holding a write attachment is
//! killed; the daemon finds it gone within the liveness cadence and reclaims its attachment,
//! its region and its id, while its lease keeps its term and then expires; another client is
//! unaffected throughout. The test binary re-invoked is the victim (an environment variable
//! selects the role), so the death is a real `SIGKILL` of a real process.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use slates_client::{Client, Deadlines, Intent, NamePolicy, Session, SizeClass, VolumeId};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::{CLIENTS_REAPED, LIVENESS_BUDGET_NS};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the environment variable selecting the victim role, and the ones carrying its
/// instance and the volume to attach.
const ROLE: &str = "SLATES_REAP_TEST_ROLE";
const ROLE_INSTANCE: &str = "SLATES_REAP_TEST_INSTANCE";
const ROLE_VOLUME: &str = "SLATES_REAP_TEST_VOLUME";
/// Shape: the probe budget of the quick profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: the lease term for this test (nanoseconds): three seconds, longer than the reclaim
/// takes (two liveness budgets at most) so the lease is seen surviving the reclaim, and short
/// enough to see it expire.
const LEASE_TERM_NS: u64 = 3_000_000_000;
/// Shape: how long to wait for each observed transition: five liveness budgets.
const WAIT: Duration = Duration::from_nanos(5 * LIVENESS_BUDGET_NS);
/// Shape: the pause between polls.
const POLL_MS: u64 = 20;

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn pause() {
  // The harness paces its polls; shipped code parks on its driver (D-9).
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(Duration::from_millis(POLL_MS));
}

fn hex(id: VolumeId) -> String {
  id.bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> VolumeId {
  let mut bytes = [0u8; 16];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap();
  }
  VolumeId { bytes }
}

/// The victim: connects, attaches for writing, prints its client id, and waits to be killed.
fn victim() {
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let volume = unhex(&std::env::var(ROLE_VOLUME).unwrap());
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  let attached = client.attach(volume, None, Intent::Write).unwrap();
  assert_eq!(attached.lease_epoch, Some(1));
  println!("victim-id: {}", client.client_id());
  loop {
    std::thread::park();
  }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
  let started = Instant::now();
  while !condition() {
    assert!(started.elapsed() < WAIT, "{what}");
    pause();
  }
}

/// The victim is killed; the daemon reclaims its attachment, region and id; the lease keeps
/// its term and expires; the observing client is unaffected.
#[test]
fn a_killed_client_is_reclaimed_and_its_lease_expires_by_its_term() {
  if std::env::var_os(ROLE).is_some() {
    victim();
    return;
  }
  let profile = MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  });
  let instance = format!("cl-reap-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance)
    .with_shards(2)
    .with_failover_slo(LEASE_TERM_NS);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: "slates-seg-cl-reap".to_owned(),
    },
  )
  .unwrap();
  let mut observer = Client::connect(&instance, deadlines()).unwrap();
  let volume = observer
    .create(&slates_client::CreateSpec {
      name: "held".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let mut child = Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "a_killed_client_is_reclaimed_and_its_lease_expires_by_its_term",
      "--nocapture",
    ])
    .env(ROLE, "victim")
    .env(ROLE_INSTANCE, &instance)
    .env(ROLE_VOLUME, hex(volume))
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
  // The harness prints its own banner first; the victim's line is tagged.
  let victim_id: u32 = BufReader::new(child.stdout.take().unwrap())
    .lines()
    .map_while(Result::ok)
    .find_map(|line| {
      line
        .strip_prefix("victim-id: ")
        .map(|n| n.trim().parse().unwrap())
    })
    .expect("the victim printed its id");
  let held_at = Instant::now();
  let report = observer.status(volume).unwrap();
  assert_eq!(report.attachments, 1, "the victim holds its attachment");
  assert_eq!(report.lease_epoch, Some(1), "and the lease");
  let reaped_before = CLIENTS_REAPED.load(Ordering::Acquire);
  child.kill().unwrap();
  child.wait().unwrap();
  // The reclaim: the attachment leaves, the lease stays for its term.
  wait_until("the attachment is reclaimed", || {
    observer.status(volume).unwrap().attachments == 0
  });
  assert!(
    held_at.elapsed() < Duration::from_nanos(LEASE_TERM_NS),
    "the reclaim came inside the lease term: {:?}",
    held_at.elapsed()
  );
  assert_eq!(
    observer.status(volume).unwrap().lease_epoch,
    Some(1),
    "the lease keeps its term after the reclaim (a paused client is not a dead one)"
  );
  assert_eq!(
    CLIENTS_REAPED.load(Ordering::Acquire),
    reaped_before + 1,
    "one client reaped"
  );
  // The id is free again: a session under it resumes instead of being refused.
  let resumed = Client::resume(
    &instance,
    Session {
      client_id: victim_id,
      next_sequence: 1,
    },
    deadlines(),
  );
  assert!(resumed.is_ok(), "{resumed:?}");
  // The lease expires by its term, with nobody asking.
  wait_until("the lease expires", || {
    observer.status(volume).unwrap().lease_epoch.is_none()
  });
  // The observer was served throughout; its counters say so.
  assert_eq!(observer.reconnects(), 0);
  daemon.stop();
}
