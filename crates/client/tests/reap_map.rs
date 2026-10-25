//! The direct map's tests that need a client process of their own (§4.6; A-172, A-174). The test binary re-invoked is
//! the client (an environment variable selects its role), so what happens to it is real: a client holding a direct map
//! is killed with `SIGKILL` and the daemon reaps its copy (T-2.3's shape); a client that can be given no descriptor
//! fails its hand-off and gives the copy back. Its own binary, apart from `reap.rs`: that suite counts the
//! process-wide `CLIENTS_REAPED`, and a second reap in its process during its window broke it (2026-10-25).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::string_slice
)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slates_client::{Client, Deadlines, Intent, NamePolicy, SizeClass, VolumeId};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the environment variable selecting the mapper role, and the ones carrying its instance and volume.
const ROLE: &str = "SLATES_REAP_MAP_TEST_ROLE";
const ROLE_INSTANCE: &str = "SLATES_REAP_MAP_TEST_INSTANCE";
const ROLE_VOLUME: &str = "SLATES_REAP_MAP_TEST_VOLUME";
/// Shape: the probe budget of the quick profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: how long to wait for the reap: five liveness budgets, as `reap.rs` waits.
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

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
  let started = Instant::now();
  while !condition() {
    assert!(started.elapsed() < WAIT, "{what}");
    pause();
  }
}

/// Own the mapper through every assertion, including failures before the deliberate kill.
struct Victim(Child);

impl Drop for Victim {
  fn drop(&mut self) {
    if self
      .0
      .try_wait()
      .expect("inspect the owned mapper")
      .is_none()
    {
      self.0.kill().expect("kill the owned mapper");
      self.0.wait().expect("reap the owned mapper");
    }
  }
}

/// The starved mapper: connects, caps its descriptors at the lowest number free, so the kernel can give it no new one —
/// the copy's descriptor delivered on Linux, its claim's socket on macOS — then maps `/f.bin` of the volume it is given,
/// prints what the map returned, and waits to be killed.
fn starved_mapper() {
  use std::os::fd::AsRawFd;

  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let volume = unhex(&std::env::var(ROLE_VOLUME).unwrap());
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  // Every descriptor number below the lowest free one is open, so a cap there leaves none to give.
  let lowest_free = rustix::io::dup(std::io::stdout()).unwrap();
  let cap = u64::try_from(lowest_free.as_raw_fd()).unwrap();
  drop(lowest_free);
  let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
  rustix::process::setrlimit(
    rustix::process::Resource::Nofile,
    rustix::process::Rlimit {
      current: Some(cap),
      maximum: limit.maximum,
    },
  )
  .unwrap();
  let outcome = match client.map_read(volume, "/f.bin") {
    Ok(mapped) => format!("mapped, zero-copy {}", mapped.is_zero_copy()),
    Err(error) => format!("{error:?}"),
  };
  println!("starved-outcome: {outcome}");
  loop {
    std::thread::park();
  }
}

/// The mapper: connects, maps `/f.bin` of the volume it is given, prints its client id and whether the map is a
/// direct one, and waits to be killed with the map held.
fn mapper() {
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let volume = unhex(&std::env::var(ROLE_VOLUME).unwrap());
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  let mapped = client.map_read(volume, "/f.bin").unwrap();
  println!(
    "mapper-id: {} zero-copy: {}",
    client.client_id(),
    mapped.is_zero_copy()
  );
  loop {
    std::thread::park();
  }
}

/// How many times the daemon counted `name`, summed over its shards.
fn counted(daemon: &Daemon, name: &str) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(name)
    .copied()
    .unwrap_or(0)
}

/// Starts the test binary again as `test`'s client in `role` over `volume`, and waits for its report: the rest of its
/// line tagged `tag`.
fn start_role(
  instance: &str,
  volume: VolumeId,
  (role, test): (&str, &str),
  tag: &str,
) -> (Victim, String) {
  let mut child = Victim(
    Command::new(std::env::current_exe().unwrap())
      .args(["--exact", test, "--nocapture"])
      .env(ROLE, role)
      .env(ROLE_INSTANCE, instance)
      .env(ROLE_VOLUME, hex(volume))
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  // The harness prints its own banner first; the client's line is tagged.
  let report = BufReader::new(child.0.stdout.take().unwrap())
    .lines()
    .map(|line| line.expect("read the client's report"))
    .find_map(|line| line.strip_prefix(tag).map(str::to_owned))
    .expect("the client printed its report");
  (child, report)
}

/// A one-shard daemon of a fresh instance tagged `tag`, so a client's home shard owns the volume and can hand it a
/// descriptor, with a volume holding `/f.bin`, 64 KiB: the daemon, its instance, the volume and the file's length.
fn daemon_with_a_file(tag: &str) -> (Daemon, String, VolumeId, usize) {
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("cl-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut owner = Client::connect(&instance, deadlines()).unwrap();
  let volume = owner
    .create(&slates_client::CreateSpec {
      name: "mapped".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let bytes: Vec<u8> = (0..(64usize << 10))
    .map(|index| u8::try_from(index % 251).unwrap() | 1)
    .collect();
  let attached = owner.attach(volume, None, Intent::Write).unwrap();
  owner
    .fs_write((volume, attached.attachment), "/f.bin", &bytes, 0o644)
    .unwrap();
  owner.detach(attached.attachment).unwrap();
  (daemon, instance, volume, bytes.len())
}

/// AC-A-171 (G-direct step 2), T-2.3's shape for the direct map: a client process holding a direct map is killed.
/// Do: on a one-shard daemon, a re-invoked test binary maps a 64 KiB file and parks; then `SIGKILL` it. Expect: while
/// it lives the daemon holds one copy of the file's page span; after the kill the daemon reaps the client within the
/// liveness cadence and its copy goes with it, so the held copies return to where they started and
/// `dax.local_map_reaped` counts one. The client's own map died with its process, so nothing holds the object.
#[test]
fn a_killed_clients_direct_maps_are_reaped_and_give_their_copies_back() {
  if std::env::var(ROLE).as_deref() == Ok("mapper") {
    mapper();
    return;
  }
  let (daemon, instance, volume, file_len) = daemon_with_a_file("reapmap");
  let before = daemon.direct_map_copies().unwrap();
  let (mut child, report) = start_role(
    &instance,
    volume,
    (
      "mapper",
      "a_killed_clients_direct_maps_are_reaped_and_give_their_copies_back",
    ),
    "mapper-id: ",
  );
  assert!(
    report.ends_with("zero-copy: true"),
    "the mapper's map is a direct one: {report}"
  );
  let page = u64::try_from(rustix::param::page_size()).unwrap();
  let span = u64::try_from(file_len).unwrap().div_ceil(page) * page;
  assert_eq!(
    daemon.direct_map_copies().unwrap(),
    (before.0 + 1, before.1 + span),
    "the daemon holds the mapper's copy while the mapper lives"
  );
  child.0.kill().unwrap();
  child.0.wait().unwrap();
  wait_until("the killed mapper's copy is reaped", || {
    daemon.direct_map_copies().unwrap() == before
  });
  assert_eq!(counted(&daemon, "dax.local_map_reaped"), 1);
  daemon.stop();
}

/// AC-A-174 (G-direct): a hand-off that fails in the client gives its copy back. Do: on a one-shard daemon, a
/// re-invoked test binary connects, caps its descriptors at the lowest number free, so it can be given none (the copy's
/// descriptor delivered on Linux, its claim's socket on macOS), then maps a 64 KiB file, and parks. Expect, while it
/// lives (so no reap can have run): its map failed on the channel, not `HandOffUnreleased`; the daemon made and handed
/// the copy (`dax.local_map` counts one) and holds none of it now, and `dax.local_map_released` counts one.
#[test]
fn a_hand_off_that_fails_in_the_client_gives_its_copy_back() {
  if std::env::var(ROLE).as_deref() == Ok("starved") {
    starved_mapper();
    return;
  }
  let (daemon, instance, volume, _) = daemon_with_a_file("handoff");
  let before = daemon.direct_map_copies().unwrap();
  let (_child, outcome) = start_role(
    &instance,
    volume,
    (
      "starved",
      "a_hand_off_that_fails_in_the_client_gives_its_copy_back",
    ),
    "starved-outcome: ",
  );
  println!("the starved mapper's map: {outcome}");
  assert!(
    outcome.starts_with("Ipc("),
    "the hand-off failed on the channel and its copy was given back: {outcome}"
  );
  assert_eq!(
    counted(&daemon, "dax.local_map"),
    1,
    "the daemon made and handed the copy"
  );
  assert_eq!(
    daemon.direct_map_copies().unwrap(),
    before,
    "the failed hand-off gave its copy back while its client lives"
  );
  assert_eq!(counted(&daemon, "dax.local_map_released"), 1);
  daemon.stop();
}
