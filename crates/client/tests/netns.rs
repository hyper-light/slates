//! A client in a network namespace of its own reaches the daemon through a socket file (§4.7, A-187). The test binary,
//! re-invoked under `unshare --user --map-current-user --net`, is the client, so the namespace is real: the kernel scopes
//! the abstract rendezvous to the daemon's network namespace (unix(7)), and only the socket file, reached through the
//! file system the two share, joins them, as a pod's mount of the rendezvous directory joins it to a daemon on its node.
//! Its own binary, since its test re-invokes itself inside a namespace.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::string_slice
)]
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use slates_client::{
  Client, ClientError, Deadlines, Intent, NamePolicy, ReadAt, SizeClass, VolumeId,
};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Format: the environment variable selecting the client role, and the ones carrying what it is given: the socket
/// file's instance, the volume, and a daemon's abstract instance it must fail to reach.
const ROLE: &str = "SLATES_NETNS_TEST_ROLE";
const ROLE_INSTANCE: &str = "SLATES_NETNS_TEST_INSTANCE";
const ROLE_VOLUME: &str = "SLATES_NETNS_TEST_VOLUME";
const ROLE_ABSTRACT: &str = "SLATES_NETNS_TEST_ABSTRACT";
/// Format: the tag of the client's report, found anywhere in a line (libtest prints `test <name> ... ` first).
const TAG: &str = "netns-client:";
/// Format: util-linux's `unshare` arguments that give a process a user and a network namespace of its own, the user's
/// uid mapped to itself.
const NAMESPACES: [&str; 3] = ["--user", "--map-current-user", "--net"];
/// Format: the test this binary re-invokes as the client.
const TEST: &str = "a_client_in_a_network_namespace_of_its_own_maps_a_file_through_the_socket_file";
/// Format: what the client stores into its writable map, and where.
const STORED: &[u8] = b"stored from a network namespace of its own";
const STORED_AT: u64 = 5_000;
/// Shape: the file's length, 64 KiB, as the reap tests map.
const FILE_BYTES: usize = 64 << 10;
/// Shape: the probe budget of the quick profile (milliseconds).
const PROBE_MS: u64 = 5;

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
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

/// The inode of this process's network namespace, which names it.
fn network_namespace() -> u64 {
  rustix::fs::stat("/proc/self/ns/net").unwrap().st_ino
}

/// The file's bytes: none zero, so an untouched or zeroed byte shows.
fn file_bytes() -> Vec<u8> {
  (0..FILE_BYTES)
    .map(|index| u8::try_from(index % 251).unwrap() | 1)
    .collect()
}

/// Format: the RAM-backed file system the test makes its own directory in: shared memory, there on every Linux.
const TEST_DIRECTORY_ROOT: &str = "/dev/shm";

/// The directory the daemon's socket file is bound in: one the test made for itself in shared memory, never on disk, the
/// user's alone, named with the process id, removed when dropped (CLAUDE.md §4). The daemon removes its node when it
/// stops, before this goes.
struct TestDirectory(String);

impl TestDirectory {
  fn make() -> TestDirectory {
    let path = format!(
      "{TEST_DIRECTORY_ROOT}/slates-a187-netns-{}",
      std::process::id()
    );
    // A test's own directory in shared memory, named with the process id and removed when dropped (CLAUDE.md §4).
    // structural: allow
    rustix::fs::mkdir(path.as_str(), rustix::fs::Mode::RWXU).unwrap();
    TestDirectory(path)
  }
}

impl Drop for TestDirectory {
  fn drop(&mut self) {
    // The test's own directory goes with it. structural: allow
    let _ = rustix::fs::rmdir(self.0.as_str());
  }
}

/// A one-shard daemon at `instance`, so the client's home shard owns the volume and can hand it a descriptor.
fn one_shard_daemon(profile: &MachineProfile, instance: &str, tag: &str) -> Daemon {
  let daemon = Daemon::start(
    profile,
    DaemonConfig::derive(profile, instance, Some(1)),
    SegmentSource::Create {
      name: format!("slates-seg-nn-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  daemon
}

/// The client, inside its namespaces: first a claim on the daemon at the abstract instance it is given, which must find
/// no daemon; then the socket file's daemon, whose volume it attaches for writing and whose `/f.bin` it maps read and
/// write, checks, stores [`STORED`] at [`STORED_AT`] in and syncs. Prints its network namespace, the abstract claim's
/// outcome, the map's lengths and what the sync wrote.
fn client_role() {
  let abstract_outcome = match Client::connect(&std::env::var(ROLE_ABSTRACT).unwrap(), deadlines())
  {
    Ok(_) => "connected".to_owned(),
    Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. })) => {
      "unavailable".to_owned()
    }
    Err(other) => format!("{other:?}"),
  };
  let instance = std::env::var(ROLE_INSTANCE).unwrap();
  let volume = unhex(&std::env::var(ROLE_VOLUME).unwrap());
  let mut client = Client::connect(&instance, deadlines()).unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  let mapped = client.map_write((volume, attachment), "/f.bin").unwrap();
  let mut shown = vec![0u8; FILE_BYTES];
  let read = mapped.read_at(0, &mut shown).unwrap();
  let shows_the_file = read == FILE_BYTES && shown == file_bytes();
  assert_eq!(mapped.write_at(STORED_AT, STORED).unwrap(), STORED.len());
  let wrote = client.map_sync(&mapped).unwrap();
  let (len, mapped_len) = (mapped.len(), mapped.mapped_len());
  client.release_map_write(mapped).unwrap();
  client.detach(attachment).unwrap();
  println!(
    "{TAG} net {} abstract {abstract_outcome} len {len} mapped {mapped_len} shows {shows_the_file} wrote {wrote}",
    network_namespace()
  );
}

/// The client's report: the words after each name in its tagged line.
fn field<'a>(report: &'a str, name: &str) -> &'a str {
  let mut words = report.split_whitespace();
  words
    .by_ref()
    .find(|word| *word == name)
    .and_then(|_| words.next())
    .unwrap_or_else(|| panic!("the client's report names {name}: {report}"))
}

/// A-187 (§4.7 "Rendezvous", the DAX matrix's socket-route cells for a workload in its own network namespace). Do: start
/// a one-shard daemon whose instance is a socket file in a directory of the user's alone and write a 64 KiB file to it,
/// and a second daemon at an abstract instance; re-invoke this binary as a client inside a user and a network
/// namespace of its own, which claims on the abstract daemon, then maps the file read and write by the direct map
/// through the socket file, stores a phrase and syncs. Expect: the client's network namespace is not this process's;
/// its claim on the abstract daemon, which this process reaches, finds no daemon (the namespace is real, so only the
/// socket file could have carried the map); its map is a direct one over the file's page span and shows the file; its
/// sync wrote the phrase's bytes, which the daemon's file then holds; and once the daemon stops its instance answers
/// no claim. Skips loudly where the host refuses an unprivileged user and network namespace.
#[test]
fn a_client_in_a_network_namespace_of_its_own_maps_a_file_through_the_socket_file() {
  if std::env::var(ROLE).as_deref() == Ok("client") {
    client_role();
    return;
  }
  if !namespaces_granted() {
    return;
  }
  let directory = TestDirectory::make();
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let pid = std::process::id();
  let instance = format!("{}/nn-{pid}", directory.0);
  let daemon = one_shard_daemon(&profile, &instance, "file");
  let abstract_instance = format!("nn-abstract-{pid}");
  let abstract_daemon = one_shard_daemon(&profile, &abstract_instance, "abstract");
  Client::connect(&abstract_instance, deadlines())
    .expect("this process reaches the abstract daemon");
  let mut owner = Client::connect(&instance, deadlines()).unwrap();
  let volume = volume_with_the_file(&mut owner);
  let report = run_the_client(&instance, volume, &abstract_instance);
  assert_the_report(&report);
  let file = owner.read(volume, "/f.bin", ReadAt::Head).unwrap();
  let at = usize::try_from(STORED_AT).unwrap();
  assert_eq!(
    file.get(at..at + STORED.len()),
    Some(STORED),
    "the daemon's file holds the client's stores"
  );
  drop(owner);
  daemon.stop();
  assert!(
    matches!(
      Client::connect(&instance, deadlines()),
      Err(ClientError::Ipc(
        slates_ipc::IpcError::DaemonUnavailable { .. }
      ))
    ),
    "once the daemon stops its instance answers no claim"
  );
  abstract_daemon.stop();
}

/// Whether the host grants an unprivileged user and network namespace; when it does not, says so (a loud skip).
fn namespaces_granted() -> bool {
  match Command::new("unshare")
    .args(NAMESPACES)
    .arg("true")
    .output()
  {
    Ok(probe) if probe.status.success() => true,
    other => {
      eprintln!(
        "SKIP: this host refuses an unprivileged user and network namespace (`unshare {}`): {other:?}",
        NAMESPACES.join(" ")
      );
      false
    }
  }
}

/// A volume holding `/f.bin`, [`file_bytes`], written through `owner`.
fn volume_with_the_file(owner: &mut Client) -> VolumeId {
  let volume = owner
    .create(&slates_client::CreateSpec {
      name: "mapped".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let attached = owner.attach(volume, None, Intent::Write).unwrap();
  owner
    .fs_write(
      (volume, attached.attachment),
      "/f.bin",
      &file_bytes(),
      0o644,
    )
    .unwrap();
  owner.detach(attached.attachment).unwrap();
  volume
}

/// Runs this binary as the client inside its namespaces, over `volume` at `instance`, with `abstract_instance` to fail to
/// reach: its tagged report, once it has ended cleanly.
fn run_the_client(instance: &str, volume: VolumeId, abstract_instance: &str) -> String {
  let output = Command::new("unshare")
    .args(NAMESPACES)
    .arg(std::env::current_exe().unwrap())
    .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
    .env(ROLE, "client")
    .env(ROLE_INSTANCE, instance)
    .env(ROLE_VOLUME, hex(volume))
    .env(ROLE_ABSTRACT, abstract_instance)
    .stdout(Stdio::piped())
    .spawn()
    .unwrap()
    .wait_with_output()
    .unwrap();
  let report = BufReader::new(output.stdout.as_slice())
    .lines()
    .map_while(Result::ok)
    .find_map(|line| line.split_once(TAG).map(|(_, rest)| rest.trim().to_owned()))
    .unwrap_or_else(|| panic!("the client reported: {output:?}"));
  println!("{TAG} {report}");
  assert!(
    output.status.success(),
    "the client ended cleanly: {output:?}"
  );
  report
}

/// The client's report says it ran in a network namespace of its own, found no daemon at the abstract instance, mapped
/// the file directly over its page span, saw the file, and wrote the phrase back at its sync.
fn assert_the_report(report: &str) {
  assert_ne!(
    field(report, "net").parse::<u64>().unwrap(),
    network_namespace(),
    "the client ran in a network namespace of its own"
  );
  assert_eq!(
    field(report, "abstract"),
    "unavailable",
    "the abstract rendezvous does not reach out of its network namespace"
  );
  assert_eq!(field(report, "len"), FILE_BYTES.to_string());
  assert_eq!(
    field(report, "mapped").parse::<u64>().unwrap(),
    slates_mem::PeerObject::bytes_for(u64::try_from(FILE_BYTES).unwrap()).unwrap(),
    "the map is a direct one over the file's page span"
  );
  assert_eq!(field(report, "shows"), "true", "the map showed the file");
  assert_eq!(
    field(report, "wrote"),
    STORED.len().to_string(),
    "the sync wrote the phrase's bytes"
  );
}
