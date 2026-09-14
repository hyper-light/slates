//! AC-2.13 / T-2.15 across real processes (§4.13 "a consumer channel is bound at rendezvous using
//! a capability delivered and retained outside other agents' reach"; GAP-A9-9's harness delivery
//! channel, decided 2026-09-14 as the inherited descriptor): the test is the harness — an in-process
//! daemon, a consumer enrolled by the human surface, the capability delivered to a spawned workload
//! (this binary re-invoked) on the one inherited descriptor — and the workload's `Client::connect`
//! binds its channel to the consumer with the capability in none of its arguments and none of its
//! environment; what it creates is the consumer's, not the account's (the account is refused
//! `Forbidden` on it); a sibling spawned without the delivery is the account — refused on the
//! consumer's volume and unable to bind with a forged capability (`ConsumerNotEnrolled`) — the
//! non-vacuity contrast. And a client holding a consumer identity binds the channel again by itself
//! after a daemon restart, before its retried verb runs, so the verb runs as the consumer.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::{OsStr, OsString};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slates_anchor::AnchorSegment;
use slates_client::{
  Capability, Client, ClientError, CreateSpec, Deadlines, NamePolicy, Refusal, SizeClass, VolumeId,
};
use slates_ipc::delivery::{Delivery, Output, delivered};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::landing::enroll_proof;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the environment variable selecting the child role, and the ones carrying the instance,
/// the consumer id the child expects to be bound to, and (for the sibling) the consumer's volume.
const ROLE: &str = "SLATES_CONSUMER_TEST_ROLE";
const ROLE_INSTANCE: &str = "SLATES_CONSUMER_TEST_INSTANCE";
const ROLE_CONSUMER: &str = "SLATES_CONSUMER_TEST_CONSUMER";
const ROLE_VOLUME: &str = "SLATES_CONSUMER_TEST_VOLUME";
/// Shape: the probe budget of the quick profile these tests measure (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon: two, so the consumer's record is enrolled on one partition and
/// attested from a channel on another (the cross-shard read of the attestation).
const TEST_SHARDS: u16 = 2;
/// Shape: the reply deadline of the test client (nanoseconds): a fifth of a second, far past any
/// served verb and short enough that a dead daemon is found quickly.
const REPLY_NS: u64 = 200_000_000;
/// Shape: the reconnect budget of the test client (nanoseconds): five seconds.
const RECONNECT_NS: u64 = 5_000_000_000;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);
/// Shape: the bytes of a workload's captured output the harness holds: a few tagged lines and the
/// environment dump, well under a mebibyte.
const OUTPUT_CAP: usize = 1 << 20;
/// Shape: the consumer child's exit codes, one per way it can fail, so a failure names its cause.
const EXIT_OK: i32 = 0;
const EXIT_NOT_BOUND: i32 = 10;
const EXIT_CAPABILITY_LEAKED: i32 = 11;
const EXIT_VERB_FAILED: i32 = 12;

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

fn deadlines() -> Deadlines {
  Deadlines {
    reply_ns: REPLY_NS,
    reconnect_ns: RECONNECT_NS,
  }
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

fn scratch(name: &str) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex_volume(text: &str) -> VolumeId {
  let mut bytes = [0u8; 16];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap();
  }
  VolumeId { bytes }
}

/// This process's uid — the account every client of these tests rendezvouses as.
fn current_uid() -> u32 {
  rustix::process::getuid().as_raw()
}

/// Whether `haystack` contains `needle` as a byte run.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
  !needle.is_empty()
    && haystack
      .windows(needle.len())
      .any(|window| window == needle)
}

/// The child's invocation: this binary running one ignored role function.
fn role_args(role: &str) -> Vec<OsString> {
  ["--ignored", "--exact", role, "--nocapture"]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn current_exe() -> OsString {
  std::env::current_exe().unwrap().into_os_string()
}

/// Whether the capability, raw or as hex, appears in any argument or environment value of this
/// process — the leak the delivery channel exists to prevent.
fn capability_leaked(capability: &Capability) -> bool {
  let as_hex = hex(capability);
  std::env::args_os()
    .chain(std::env::vars_os().map(|(_, value)| value))
    .any(|value| {
      let bytes = value.as_encoded_bytes();
      contains(bytes, capability) || contains(bytes, as_hex.as_bytes())
    })
}

/// The consumer child: `Client::connect` takes the delivery and binds; the capability is in no
/// argument and no environment value; a volume it creates is its own. Its environment is dumped,
/// tagged, for the parent's own look. Ignored so `cargo test` never runs it in-process; the parent
/// runs it with `--ignored --exact`.
#[test]
#[ignore = "the consumer child; run by a_spawned_consumer_... with --ignored"]
fn consumer_child() {
  let Ok(role) = std::env::var(ROLE) else {
    return;
  };
  assert_eq!(role, "consumer");
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let expected: u64 = std::env::var(ROLE_CONSUMER).unwrap().parse().unwrap();
  let mut client = match Client::connect(&instance, deadlines()) {
    Ok(client) => client,
    Err(e) => {
      println!("child: connect refused: {e}");
      std::process::exit(EXIT_NOT_BOUND);
    }
  };
  if client.consumer() != Some(expected) {
    println!("child: bound to {:?}", client.consumer());
    std::process::exit(EXIT_NOT_BOUND);
  }
  // The record the client took is this process's one delivery; the capability reached it on the
  // descriptor alone.
  let capability = delivered().unwrap().capability;
  if capability_leaked(&capability) {
    std::process::exit(EXIT_CAPABILITY_LEAKED);
  }
  for (name, value) in std::env::vars_os() {
    println!(
      "env: {}={}",
      name.to_string_lossy(),
      value.to_string_lossy()
    );
  }
  let volume = match client.create(&scratch("mine")) {
    Ok(volume) => volume,
    Err(e) => {
      println!("child: create refused: {e}");
      std::process::exit(EXIT_VERB_FAILED);
    }
  };
  if let Err(e) = client.status(volume) {
    println!("child: status refused: {e}");
    std::process::exit(EXIT_VERB_FAILED);
  }
  println!("volume: {}", hex(&volume.bytes));
  std::process::exit(EXIT_OK);
}

/// The sibling: spawned without a delivery, it connects as the account, is refused on the
/// consumer's volume, and cannot bind with a forged capability. Prints what it found, tagged.
#[test]
#[ignore = "the sibling child; run by the parent with --ignored"]
fn sibling_child() {
  let Ok(role) = std::env::var(ROLE) else {
    return;
  };
  assert_eq!(role, "sibling");
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let consumer: u64 = std::env::var(ROLE_CONSUMER).unwrap().parse().unwrap();
  let volume = unhex_volume(&std::env::var(ROLE_VOLUME).unwrap());
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  println!("sibling: consumer {:?}", client.consumer());
  match client.status(volume) {
    Err(ClientError::Refused(Refusal::Forbidden { .. })) => println!("sibling: status forbidden"),
    other => println!("sibling: status {other:?}"),
  }
  match client.attest(consumer, &[0u8; 32]) {
    Err(ClientError::Refused(Refusal::ConsumerNotEnrolled)) => {
      println!("sibling: attest not_enrolled");
    }
    other => println!("sibling: attest {other:?}"),
  }
  std::process::exit(EXIT_OK);
}

/// The line after `tag` in the child's output.
fn line_after<'a>(text: &'a str, tag: &str) -> &'a str {
  text
    .lines()
    .find_map(|line| line.strip_prefix(tag))
    .unwrap_or_else(|| panic!("no `{tag}` line in:\n{text}"))
}

/// AC-2.13 / T-2.15 (§4.13, GAP-A9-9): the harness enrolls a consumer and delivers its capability
/// to a spawned workload on the one inherited descriptor; the workload's `Client::connect` binds
/// its channel to that consumer with the capability in none of its arguments or environment (the
/// child checks both, raw and hex; the parent checks the dump again), and the volume it creates is
/// the consumer's — the account is refused `Forbidden` on it. The contrast: a sibling spawned
/// without the delivery is the account (no consumer), refused on that volume, and cannot bind with
/// a forged capability (`ConsumerNotEnrolled`).
#[test]
fn a_spawned_consumer_binds_through_the_inherited_capability_and_a_sibling_without_it_is_the_account()
 {
  let profile = profile();
  let instance = format!("cl-consumer-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: "slates-seg-cl-consumer".to_owned(),
    },
  )
  .unwrap();
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = current_uid();
  let mut owner = connect(&instance);
  let (consumer, capability) = owner
    .enroll(account, enroll_proof(&secret, account))
    .unwrap();
  let volume = run_consumer_child(&instance, consumer, &capability);

  // Ownership: the child's volume is the consumer's, not the account's.
  assert!(
    matches!(
      owner.status(volume),
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "the account is refused on the consumer's volume before any lookup"
  );
  assert_sibling_is_the_account(&instance, consumer, volume);
  daemon.stop();
}

/// The harness's spawn: the capability goes onto the one inherited descriptor and nowhere else; the
/// child's exit code names any failure, its dump is checked for the capability's hex, and the volume
/// it created is returned.
fn run_consumer_child(instance: &str, consumer: u64, capability: &Capability) -> VolumeId {
  let delivery = Delivery::prepare(consumer, capability).unwrap();
  let consumer_text = consumer.to_string();
  let environment: [(&OsStr, &OsStr); 3] = [
    (OsStr::new(ROLE), OsStr::new("consumer")),
    (OsStr::new(ROLE_INSTANCE), OsStr::new(instance)),
    (OsStr::new(ROLE_CONSUMER), OsStr::new(&consumer_text)),
  ];
  let mut child = delivery
    .spawn(
      &current_exe(),
      &role_args("consumer_child"),
      &environment,
      Output::Captured,
    )
    .unwrap();
  let (code, output) = child.wait_with_output(OUTPUT_CAP).unwrap();
  let text = String::from_utf8_lossy(&output);
  assert_eq!(
    code,
    Some(EXIT_OK),
    "the consumer child (10 not bound, 11 the capability reached its arguments or environment, 12 a verb failed):\n{text}"
  );
  assert!(
    !text.contains(&hex(capability)),
    "the capability is nowhere in the child's environment dump:\n{text}"
  );
  unhex_volume(line_after(&text, "volume: "))
}

/// The contrast: a sibling spawned without the delivery is the account — no consumer, refused on the
/// consumer's volume, unable to bind with a forged capability.
fn assert_sibling_is_the_account(instance: &str, consumer: u64, volume: VolumeId) {
  let sibling = Command::new(current_exe())
    .args(role_args("sibling_child"))
    .env(ROLE, "sibling")
    .env(ROLE_INSTANCE, instance)
    .env(ROLE_CONSUMER, consumer.to_string())
    .env(ROLE_VOLUME, hex(&volume.bytes))
    .stdout(Stdio::piped())
    .output()
    .unwrap();
  let sibling_text = String::from_utf8_lossy(&sibling.stdout);
  assert!(sibling.status.success(), "{sibling_text}");
  for expected in [
    "sibling: consumer None",
    "sibling: status forbidden",
    "sibling: attest not_enrolled",
  ] {
    assert!(
      sibling_text.contains(expected),
      "{expected} expected in:\n{sibling_text}"
    );
  }
}

/// §4.13 across a daemon restart (the test plays the anchor, as the client's restart test does): a
/// client that attested a consumer keeps the capability; when the daemon restarts, its next verb
/// finds the daemon gone, reconnects under its id, binds the fresh channel to the consumer again
/// **before** the verb runs, and is served as the consumer — the consumer's volume answers it,
/// while the account stays refused on it. Non-vacuous: `rebinds` moves from 0 to 1, and without
/// the re-bind the reconnected channel would be the account's and the verb refused `Forbidden`.
#[test]
fn a_client_holding_a_consumer_identity_binds_again_by_itself_after_a_daemon_restart() {
  let profile = profile();
  let instance = format!("cl-rebind-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
  let segment = restart_segment("cl-rebind", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  let secret = first.segment().issuer_secret().unwrap();
  let account = current_uid();
  let mut owner = connect(&instance);
  let (consumer, capability) = owner
    .enroll(account, enroll_proof(&secret, account))
    .unwrap();
  let mut workload = connect(&instance);
  assert_eq!(
    workload.consumer(),
    None,
    "an account client until it attests"
  );
  workload.attest(consumer, &capability).unwrap();
  assert_eq!(workload.consumer(), Some(consumer));
  let mine = workload.create(&scratch("mine")).unwrap();
  assert_account_refused(&mut owner, mine, "before the restart");
  assert_eq!(workload.rebinds(), 0);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  assert_served_as_the_consumer_after_the_restart(&mut workload, mine, consumer);
  assert_account_refused(&mut owner, mine, "after the restart");
  second.stop();
  drop(segment);
}

/// The anchor's segment the test holds across both daemons, with its content object sized as the
/// client's restart test sizes it: two reserve-sized slots per shard (the recovery image is a double
/// buffer) times the partitions, lazily backed.
fn restart_segment(name: &str, profile: &MachineProfile, config: &DaemonConfig) -> AnchorSegment {
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
    * 2
    * usize::from(config.geometry.partitions.max(1));
  AnchorSegment::create(
    &format!("slates-seg-{name}"),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(&format!("slates-con-{name}"), content_bytes)
  .unwrap()
}

/// The source a daemon starts over: the held segment's handoff.
fn source_of(segment: &AnchorSegment) -> SegmentSource {
  let (handoff, len) = segment.handoff().unwrap();
  let content = segment.content_handoff().unwrap();
  SegmentSource::Handoff {
    handoff,
    len,
    content,
  }
}

/// The account's client is refused `Forbidden` on a consumer's volume, before any lookup.
fn assert_account_refused(owner: &mut Client, volume: VolumeId, when: &str) {
  assert!(
    matches!(
      owner.status(volume),
      Err(ClientError::Refused(Refusal::Forbidden { .. }))
    ),
    "the account is refused on the consumer's volume {when}"
  );
}

/// The workload's next verb after the restart reconnects under its id, binds the fresh channel to
/// the consumer again by itself, and is served as the consumer.
fn assert_served_as_the_consumer_after_the_restart(
  workload: &mut Client,
  volume: VolumeId,
  consumer: u64,
) {
  let report = workload.status(volume).unwrap();
  assert_eq!(
    report.name, "mine",
    "served as the consumer after the restart"
  );
  assert_eq!(workload.reconnects(), 1, "one reconnect, under the old id");
  assert_eq!(
    workload.rebinds(),
    1,
    "the channel was bound to the consumer again by the client itself"
  );
  assert_eq!(workload.consumer(), Some(consumer));
}
