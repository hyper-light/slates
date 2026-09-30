//! A landing lands exactly the state it names, through the daemon (§4.15 step 1, D-26; AUD-29-02, A-49): a
//! landing of a snapshot presents and writes that snapshot's bytes whatever the head has become, the head's
//! later edits stay private, and they land afterwards as replacements of what the snapshot landed.
//!
//! Until 2026-09-30 a named snapshot the head had moved past was refused (`Unsupported`), and until
//! 2026-09-29 the head was landed under the snapshot's name
//! (docs/bugs/2026-09-29-a-landing-of-a-named-snapshot-landed-the-live-head.md).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The daemon's NFS loopback transport and a real landing target: macOS and Linux.
#![cfg(unix)]

use std::net::TcpStream;

use slates_client::{Client, Landing, SnapshotId, VolumeId};
use slates_ipc::protocol::Filter;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{approve, connect, scratch};
use common::nfs::{create, lookup, mount, read, write};
use common::target::target_dir;

/// Shape: one shard: the landing plane is the subject, not placement.
const TEST_SHARDS: u16 = 1;
/// Shape: the volume's name.
const VOLUME: &str = "snapshot-landing";
/// Shape: the file's bytes at the snapshot and at the head after it — one length, so an in-place write
/// replaces them whole.
const SNAPSHOT_BYTES: &[u8] = b"the snapshot's bytes";
const HEAD_BYTES: &[u8] = b"the head's own bytes";

/// An NFS connection to `daemon` mounted on the volume's root: the stream and the root's handle.
fn mounted(daemon: &Daemon) -> (TcpStream, Vec<u8>) {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon
    .mount_capability(VOLUME)
    .expect("the name's owner shard answers")
    .expect("the volume is served");
  let root = mount(&mut stream, &capability, 1);
  (stream, root)
}

/// Lands `snapshot` (the head when `None`) into `target` under a fresh grant; the outcome's state.
fn land(
  client: &mut Client,
  secret: &[u8; 32],
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  target: &str,
) -> String {
  let grant = approve(client, secret, volume, snapshot, target);
  match client.land(volume, snapshot, target, Filter::default(), Some(grant)) {
    Ok(Landing::Landed(outcome)) => outcome.state,
    other => panic!("the granted landing: {other:?}"),
  }
}

/// AUD-29-02 (§4.15; A-49). Do: write a file through the daemon's NFS transport and snapshot; write the head's
/// own bytes over it; land the snapshot into a fresh directory; read the head; land the head. Expect: the
/// snapshot's landing writes the snapshot's bytes and the head still reads its own; the head's landing then
/// replaces them with no conflict; both end `done`.
#[test]
fn a_landing_of_a_snapshot_writes_its_bytes_and_the_head_lands_after_it() {
  let profile = common::machine_profile();
  let instance = format!("srv-snapland-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-snapland-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = target_dir();
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let volume = client.create(&scratch(VOLUME)).unwrap();
  let (mut stream, root) = mounted(&daemon);
  let file = create(&mut stream, &root, "f", 2);
  write(&mut stream, &file, SNAPSHOT_BYTES, 3);
  let snapshot = client.snapshot(volume).unwrap();
  write(&mut stream, &file, HEAD_BYTES, 4);

  let state = land(&mut client, &secret, volume, Some(snapshot), &target.path);
  assert_eq!(state, "done");
  let landed = format!("{}/f", target.path);
  assert_eq!(std::fs::read(&landed).unwrap(), SNAPSHOT_BYTES);
  let (mut stream, root) = mounted(&daemon);
  let file = lookup(&mut stream, &root, "f", 5);
  assert_eq!(
    read(&mut stream, &file, 6),
    HEAD_BYTES,
    "the head's edit stayed private"
  );

  let state = land(&mut client, &secret, volume, None, &target.path);
  assert_eq!(state, "done");
  assert_eq!(std::fs::read(&landed).unwrap(), HEAD_BYTES);
  // The landed file left the overlay: it is now a base entry of the target the volume overlays, served
  // through the host the landing kept (until 2026-09-30 the volume was served hostless after a landing).
  let (mut stream, root) = mounted(&daemon);
  let file = lookup(&mut stream, &root, "f", 7);
  assert_eq!(
    read(&mut stream, &file, 8),
    HEAD_BYTES,
    "the landed file reads back through the mount, from the disk it landed on"
  );
  drop(client);
  daemon.stop();
}
