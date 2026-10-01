//! A large granted landing does not monopolize its owner shard (AUD-29-25; §4.15, §4.3, R9): the landing runs
//! as an owned run stepped in slices of half the shard's step quantum, and the shard serves its other clients
//! between them. Before the sliced run, the landing was one synchronous call on the shard, and a verb that
//! arrived while it ran waited for all of it.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The daemon's NFS loopback transport and a real landing target: macOS and Linux.
#![cfg(unix)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_client::{Filter, Landing};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{approve, connect, scratch};
use common::nfs::{create, mount, write};
use common::target::target_dir;

/// Shape: one shard, so the landing and the probing client share it — the shard is the subject.
const TEST_SHARDS: u16 = 1;
/// Shape: the volume's name.
const VOLUME: &str = "fair-landing";
/// Shape: files in the landing — each a temporary created, written, synced and linked on the disk, so the
/// landing's host work spans many slices on any disk.
const FILES: usize = 600;
/// Shape: the probes taken before the landing, for the probe's own service time on this machine.
const BASELINE_PROBES: usize = 32;

/// The probe's latency: one `list` round trip.
fn probe(client: &mut slates_client::Client) -> Duration {
  let began = Instant::now();
  client.list().unwrap();
  began.elapsed()
}

/// AUD-29-25 acceptance. Do: write 600 files through the daemon's NFS transport into a one-shard volume, take
/// the probe's baseline latency, then land the volume into a fresh directory under a grant on one thread while
/// another client keeps probing the same shard. Expect: the landing is done with every file written and ran in
/// many slices; probes were answered while it ran; and no probe waited for the whole landing — the longest
/// probe during it is shorter than the landing, and no slice was more than half of it.
#[test]
fn a_large_landing_leaves_its_shard_serving_between_its_slices() {
  let profile = common::machine_profile();
  let instance = format!("srv-fairland-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  // The landing client waits as long as the landing may run — its lease's term — where the default reply
  // deadline (the liveness budget, one second) answers `Stalled` to a landing still at work (a reported
  // sibling).
  let landing_deadlines = slates_client::Deadlines {
    reply_ns: config.failover_slo_ns,
    ..common::landing::deadlines()
  };
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-fairland-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = target_dir();
  let mut client = slates_client::Client::connect(&instance, landing_deadlines).unwrap();
  let secret = daemon.segment().issuer_secret().unwrap();
  let volume = client.create(&scratch(VOLUME)).unwrap();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability(VOLUME).unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  let mut xid = 2u32;
  for n in 0..FILES {
    let file = create(&mut stream, &root, &format!("f{n:04}"), xid);
    write(&mut stream, &file, format!("file {n}").as_bytes(), xid + 1);
    xid += 2;
  }

  let mut prober = connect(&instance);
  let baseline = (0..BASELINE_PROBES)
    .map(|_| probe(&mut prober))
    .max()
    .unwrap();
  let grant = approve(&mut client, &secret, volume, None, &target.path);
  let target_path = target.path.clone();
  let landing = std::thread::spawn(move || {
    let began = Instant::now();
    let landed = client.land(volume, None, &target_path, Filter::default(), Some(grant));
    (began, began.elapsed(), landed)
  });
  let mut during = Vec::new();
  while !landing.is_finished() {
    let at = Instant::now();
    let took = probe(&mut prober);
    during.push((at, took));
  }
  let (began, took, landed) = landing.join().unwrap();
  let (slices, longest_slice_ns) = daemon.landing_slices().unwrap();
  drop(prober);
  daemon.stop();

  let Ok(Landing::Landed(outcome)) = landed else {
    panic!("the granted landing: {landed:?}");
  };
  let inside: Vec<Duration> = during
    .iter()
    .filter(|(at, probe)| *at >= began && *at + *probe <= began + took)
    .map(|(_, probe)| *probe)
    .collect();
  let longest_probe = inside.iter().max().copied().unwrap_or_default();
  let longest_slice = Duration::from_nanos(longest_slice_ns);
  eprintln!(
    "landing of {FILES} files: {took:?} in {slices} slices (longest {longest_slice:?}); \
     {} probes inside it, longest {longest_probe:?}; baseline probe {baseline:?}",
    inside.len()
  );
  assert_eq!(outcome.state, "done");
  assert_eq!(outcome.written, u64::try_from(FILES).unwrap());
  assert!(slices > 1, "the landing ran in slices: {slices}");
  assert!(
    inside.len() > 1,
    "the shard answered probes while the landing ran: {}",
    inside.len()
  );
  assert!(
    longest_probe < took,
    "no probe waited for the whole landing ({longest_probe:?} of {took:?})"
  );
  assert!(
    longest_slice * 2 <= took,
    "no slice was more than half the landing ({longest_slice:?} of {took:?})"
  );
}
