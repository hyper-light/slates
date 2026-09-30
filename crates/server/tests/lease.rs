//! The target landing lease through the daemon (§4.15 "Ownership facts", step 4; AUD-29-03): one host
//! target has one landing lease, whichever shard owns the landing volume and whichever spelling names the
//! target. The lease is a durable record kept by the control shard under the target's canonical identity
//! (its directory's device and inode), and a granted landing runs as an owned task that takes it before the
//! engine starts and releases it before its reply.
//!
//! Until 2026-09-29 each owner shard kept its own lease table keyed by the target's path, so two volumes on
//! two shards, or two spellings of one directory, landed into it at once
//! (docs/bugs/2026-09-29-a-target-landing-lease-was-per-shard-and-per-path.md).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// These integration tests drive the daemon's NFS-loopback transport and a real landing target — macOS
// and Linux; on Windows the daemon lands nothing yet.
#![cfg(unix)]

use std::net::TcpStream;
use std::os::unix::fs::MetadataExt;

use slates_client::{Client, ClientError, Landing, SnapshotId, VolumeId};
use slates_ipc::protocol::{Filter, Refusal};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{approve, connect, scratch};
use common::lease::{hold, lease_key_of, release};
use common::nfs::{create, mount, write};
use common::target::{TargetDir, target_dir};

/// Shape: shards per test daemon: two, so the two volumes can have different owner shards.
const TEST_SHARDS: u16 = 2;
/// Shape: names tried to find one volume on each shard — the owner is a hash of the name, so a handful
/// covers two shards (checked, not assumed).
const NAMES: usize = 16;
/// Shape: the lease term of the test's holds: a minute, far past the test.
const TERM_NS: u64 = 60_000_000_000;
/// Shape: the holder the test takes the lease for: no landing attempt of the daemon's is numbered this
/// high (its counters start at one under a sixteen-bit partition).
const HOLDER: u64 = u64::MAX;

/// A volume named so that shard `shard` owns it, holding one file `file` with `bytes` (written through the
/// daemon's NFS transport), snapshotted.
fn volume_on(
  daemon: &Daemon,
  client: &mut Client,
  shard: u16,
  file: &str,
  bytes: &[u8],
) -> (VolumeId, SnapshotId) {
  let name = (0..NAMES)
    .map(|n| format!("lease-{n}"))
    .find(|name| slates_server::verbs::owner_of_name(name, usize::from(TEST_SHARDS)) == shard)
    .expect("a name the shard owns");
  let volume = client.create(&scratch(&name)).unwrap();
  assert_eq!(slates_server::verbs::owner_of(volume), shard, "{name}");
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon
    .mount_capability(&name)
    .expect("the name's owner shard answers")
    .expect("a volume by that name is served there");
  let root = mount(&mut stream, &capability, 1);
  let handle = create(&mut stream, &root, file, 2);
  write(&mut stream, &handle, bytes, 3);
  (volume, client.snapshot(volume).unwrap())
}

/// Another spelling of the directory at `path` that opens that same directory, where the host has one
/// without privilege: macOS's firmlinked data volume, or the last component in the other case on a
/// case-insensitive volume. Judged by the directory's device and inode as the host reports them.
fn alias_of(path: &str) -> Option<String> {
  let original = std::fs::metadata(path).ok()?;
  let (parent, name) = path.rsplit_once('/')?;
  let other_case: String = name
    .chars()
    .map(|c| {
      if c.is_ascii_lowercase() {
        c.to_ascii_uppercase()
      } else {
        c.to_ascii_lowercase()
      }
    })
    .collect();
  [
    format!("/System/Volumes/Data{path}"),
    format!("{parent}/{other_case}"),
  ]
  .into_iter()
  .find(|candidate| {
    candidate != path
      && std::fs::metadata(candidate)
        .is_ok_and(|other| other.dev() == original.dev() && other.ino() == original.ino())
  })
}

/// A granted landing: its volume, snapshot, target spelling and grant.
type Granted = (VolumeId, SnapshotId, String, u64);

/// Lands `granted` under its grant.
fn land(client: &mut Client, granted: &Granted) -> Result<Landing, ClientError> {
  let (volume, snapshot, spelling, grant) = granted;
  client.land(
    *volume,
    Some(*snapshot),
    spelling,
    Filter::default(),
    Some(*grant),
  )
}

/// The granted landings running as tasks and the target leases recorded, over every shard.
fn landing_plane(client: &mut Client) -> (u64, u64) {
  let report = client.daemon_status().unwrap();
  report
    .shards
    .iter()
    .fold((0, 0), |(running, leases), shard| {
      (
        running + shard.landings_in_flight,
        leases + shard.target_leases,
      )
    })
}

/// A daemon of [`TEST_SHARDS`] shards named `name`, its consensus group bootstrapped, and its instance.
fn start(name: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// Two granted landings into `target`: a volume owned by shard 0 into the target as spelled, and one owned
/// by shard 1 into another spelling of it where the host has one (the same spelling otherwise, said on
/// stderr — the alias half skips loudly; the cross-shard half still runs).
fn granted_landings(daemon: &Daemon, client: &mut Client, target: &TargetDir) -> [Granted; 2] {
  let other = match alias_of(&target.path) {
    Some(alias) => {
      eprintln!("lease: the alias half lands through {alias}");
      alias
    }
    None => {
      eprintln!(
        "lease: SKIP the alias half — this host has no unprivileged second spelling of {}; both \
         landings use one spelling (the cross-shard half still runs)",
        target.path
      );
      target.path.clone()
    }
  };
  let secret = daemon.segment().issuer_secret().unwrap();
  [
    (
      volume_on(daemon, client, 0, "a", b"from a"),
      target.path.clone(),
    ),
    (volume_on(daemon, client, 1, "b", b"from b"), other),
  ]
  .map(|((volume, snapshot), spelling)| {
    let grant = approve(client, &secret, volume, Some(snapshot), &spelling);
    (volume, snapshot, spelling, grant)
  })
}

/// Every landing in `landings` is refused `LandingLeaseHeld` naming [`HOLDER`], and `target` stays empty.
fn refused_while_held(client: &mut Client, landings: &[Granted], target: &TargetDir) {
  for granted in landings {
    let refused = land(client, granted);
    assert!(
      matches!(
        refused,
        Err(ClientError::Refused(Refusal::LandingLeaseHeld {
          holder: HOLDER
        }))
      ),
      "{}: {refused:?}",
      granted.2
    );
  }
  assert_eq!(
    std::fs::read_dir(&target.path).unwrap().count(),
    0,
    "a landing wrote while another held the target"
  );
}

/// Every landing in `landings` lands whole.
fn all_land(client: &mut Client, landings: &[Granted]) {
  for granted in landings {
    let landed = land(client, granted);
    assert!(
      matches!(&landed, Ok(Landing::Landed(outcome)) if outcome.state == "done"),
      "{}: {landed:?}",
      granted.2
    );
  }
}

/// AUD-29-03 (§4.15 step 4). Do: grant the landings of two volumes owned by different shards, one into the
/// target and one into another spelling of it (where the host has one; the same spelling otherwise, said
/// on stderr); hold the target's lease for another attempt; land both; release the lease; land both again.
/// Expect: while the lease is held both are refused `LandingLeaseHeld` naming that one holder, and the
/// target stays empty; once it is released both land, each file on the disk; afterwards no landing runs
/// and no lease is recorded.
#[test]
fn one_target_has_one_landing_lease_whichever_shard_and_spelling_lands_it() {
  let (daemon, instance) = start("lease");
  let target = target_dir();
  let mut client = connect(&instance);
  let landings = granted_landings(&daemon, &mut client, &target);
  let key = lease_key_of(&target.path);
  let held = hold(&daemon, &key, HOLDER, TERM_NS).unwrap();
  refused_while_held(&mut client, &landings, &target);
  release(&daemon, &key, held.holder);
  all_land(&mut client, &landings);
  assert_eq!(
    std::fs::read(format!("{}/a", target.path)).unwrap(),
    b"from a"
  );
  assert_eq!(
    std::fs::read(format!("{}/b", target.path)).unwrap(),
    b"from b"
  );
  assert_eq!(
    landing_plane(&mut client),
    (0, 0),
    "no landing still runs and no lease is left (landings running, leases)"
  );
  drop(client);
  daemon.stop();
}
