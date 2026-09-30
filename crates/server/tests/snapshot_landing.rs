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

/// Shape: the overlay test's volume name, and the bytes of the file the base holds and the one the mount adds.
const OVERLAY_VOLUME: &str = "overlay-landing";
const KEPT_BYTES: &[u8] = b"on the disk before the mount";
const NEW_BYTES: &[u8] = b"written through the mount";

/// §4.15 (a landing of an overlay volume into the directory it overlays): do: overlay a real directory
/// holding `kept`, create and write `new` through the mount, land the head into that directory, then read
/// both names back through the mount; expect the landing done with `new` on the disk, and both files served
/// from the disk afterwards. The engine runs the volume's base operations through the host it is given, and
/// the volume's base names the directory by the handles of the host its slot holds: the landing must use that
/// host for the base, or its handles name other directories (found 2026-09-30, AUD-29-16's sibling sweep).
#[test]
fn a_landing_of_an_overlay_into_its_base_serves_both_files_afterwards() {
  let profile = common::machine_profile();
  let instance = format!("srv-ovland-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-ovland-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = target_dir();
  target.seed("kept", KEPT_BYTES);
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let volume = client
    .create(&slates_client::CreateSpec {
      base: Some(target.path.clone()),
      ..scratch(OVERLAY_VOLUME)
    })
    .unwrap();
  let mounted_overlay = |daemon: &Daemon| {
    let port = daemon.nfs_port().expect("the daemon is serving NFS");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let capability = daemon
      .mount_capability(OVERLAY_VOLUME)
      .expect("the name's owner shard answers")
      .expect("the volume is served");
    let root = mount(&mut stream, &capability, 1);
    (stream, root)
  };
  let (mut stream, root) = mounted_overlay(&daemon);
  let file = create(&mut stream, &root, "new", 2);
  write(&mut stream, &file, NEW_BYTES, 3);
  let state = land(&mut client, &secret, volume, None, &target.path);
  assert_eq!(state, "done");
  assert_eq!(
    std::fs::read(format!("{}/new", target.path)).unwrap(),
    NEW_BYTES
  );
  let (mut stream, root) = mounted_overlay(&daemon);
  let new = lookup(&mut stream, &root, "new", 4);
  assert_eq!(read(&mut stream, &new, 5), NEW_BYTES);
  let kept = lookup(&mut stream, &root, "kept", 6);
  assert_eq!(read(&mut stream, &kept, 7), KEPT_BYTES);
  drop(client);
  daemon.stop();
}

/// Shape: a base file past a chunk window on any supported page (16 pages of at most 64 KiB), and the
/// size the large-file class starts at in this test, so an edit of its first bytes pins one window and the
/// rest is read from the disk when it lands.
const LARGE_FILE_BYTES: usize = 2 * 1024 * 1024;
const TEST_LARGE_CLASS_BYTES: u64 = 4096;

/// §4.15 (a landing reads the unpinned bytes of a large base file from the base): do: overlay a directory
/// holding a 2 MiB file with the large-file class at one page, rewrite its first bytes through the mount,
/// land the head into that directory; expect the file on the disk to be the edit followed by the base's own
/// remaining bytes. The landing reads those remaining bytes through the volume's base host: until 2026-09-30
/// it read them through the landing writer's own host, whose handle numbers name other directories.
#[test]
fn a_landing_of_a_partly_edited_large_base_file_keeps_the_disk_bytes_it_did_not_edit() {
  let profile = common::machine_profile();
  let instance = format!("srv-ovlarge-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  config.large_class_bytes = TEST_LARGE_CLASS_BYTES;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-ovlarge-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let target = target_dir();
  let original: Vec<u8> = (0..LARGE_FILE_BYTES)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  target.seed("large", &original);
  let mut client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  let volume = client
    .create(&slates_client::CreateSpec {
      base: Some(target.path.clone()),
      ..scratch(OVERLAY_VOLUME)
    })
    .unwrap();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon
    .mount_capability(OVERLAY_VOLUME)
    .expect("the name's owner shard answers")
    .expect("the volume is served");
  let root = mount(&mut stream, &capability, 1);
  let file = lookup(&mut stream, &root, "large", 2);
  write(&mut stream, &file, NEW_BYTES, 3);
  let state = land(&mut client, &secret, volume, None, &target.path);
  assert_eq!(state, "done");
  let mut expected = original.clone();
  expected[..NEW_BYTES.len()].copy_from_slice(NEW_BYTES);
  assert_eq!(
    std::fs::read(format!("{}/large", target.path)).unwrap(),
    expected,
    "the edit, then the base's own bytes"
  );
  drop(client);
  daemon.stop();
}
