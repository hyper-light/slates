//! The daemon's Linux FUSE transport by use (§4.6 "Linux"; AUD-29-64): a volume mounted by `attach` with the
//! FUSE form — the daemon running the OS's `fusermount3` without blocking its shard and serving the device on
//! the volume's owner shard — reached by ordinary file calls through the kernel; the mount table naming the
//! attachment; and the mount ended by a `detach`, by the kernel's own unmount, and by the daemon's stop, the
//! attachment ending with it each time.
//!
//! Linux only, gated: skips loudly without `fusermount3` or `/dev/fuse` (the CI Linux lane has both; locally, a
//! container with `--device /dev/fuse --cap-add SYS_ADMIN`, run as an ordinary user). The mount points are in
//! the build output (`common::target`; A-50), removed at the end.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::time::{Duration, Instant};

use slates_client::{ClientError, Intent, Refusal};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{connect, scratch};
use common::target::target_dir;

/// Shape: two shards, so the volume's owner may not be the shard the client lands on.
const TEST_SHARDS: u16 = 2;
/// Shape: the volume's name.
const VOLUME: &str = "fuse-mounted";
/// Shape: how long an unmount takes to reach the attachment — the helper and the kernel's disconnect, then
/// one serve turn: far past their milliseconds, short enough that a lost end fails the test.
const ENDS_WITHIN: Duration = Duration::from_secs(10);

fn command_available(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// The kernel's mount table line for `point`, if a mount is there: (filesystem type, source).
fn mounted_at(point: &str) -> Option<(String, String)> {
  let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
  table.lines().find_map(|line| {
    let (head, tail) = line.split_once(" - ")?;
    (head.split(' ').nth(4)? == point).then(|| {
      let mut fields = tail.split(' ');
      (
        fields.next().unwrap_or("").to_owned(),
        fields.next().unwrap_or("").to_owned(),
      )
    })
  })
}

/// Waits until `done` holds or [`ENDS_WITHIN`] passes; whether it held.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
  let started = Instant::now();
  while started.elapsed() < ENDS_WITHIN {
    if done() {
      return true;
    }
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(20));
  }
  done()
}

/// The ordinary calls an agent makes, through the kernel: create and write, make a directory, rename, read
/// back, list, remove.
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn work_through_the_kernel(point: &str) {
  std::fs::write(format!("{point}/notes.txt"), b"first draft").unwrap();
  std::fs::create_dir(format!("{point}/src")).unwrap();
  std::fs::rename(
    format!("{point}/notes.txt"),
    format!("{point}/src/notes.txt"),
  )
  .unwrap();
  assert_eq!(
    std::fs::read(format!("{point}/src/notes.txt")).unwrap(),
    b"first draft"
  );
  let names: Vec<String> = std::fs::read_dir(point)
    .unwrap()
    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
    .collect();
  assert_eq!(names, ["src"]);
  std::fs::write(format!("{point}/scratch"), b"gone soon").unwrap();
  std::fs::remove_file(format!("{point}/scratch")).unwrap();
}

/// The first mount: attached at a fresh directory, worked through the kernel, the mount table read, then
/// detached — the mount gone and the attachment ended.
fn mount_work_and_detach(client: &mut slates_client::Client, volume: slates_client::VolumeId) {
  let point = target_dir();
  let attached = client
    .attach_fuse(volume, Intent::Write, &point.path)
    .unwrap();
  assert_eq!(attached.path.as_deref(), Some(point.path.as_str()));
  let (fstype, source) = mounted_at(&point.path).expect("the kernel lists the mount");
  assert_eq!(fstype, "fuse.slates");
  assert_eq!(source, format!("slates:{:016x}", attached.attachment));
  work_through_the_kernel(&point.path);
  assert_eq!(client.status(volume).unwrap().attachments, 1);
  // The container bind of this mount: its source authority is built (the table names this attachment, held
  // to the record), but no container workload has run through it, so it is refused typed (AUD-29-64).
  let bind = client.attach_with(
    volume,
    None,
    Intent::Write,
    slates_client::AttachRequest::Oci {
      source: point.path.clone(),
      destination: "/work".to_owned(),
    },
  );
  assert!(
    matches!(
      bind,
      Err(ClientError::Refused(Refusal::AttachmentUnsupported {
        reason: slates_client::UnsupportedReason::ContainerWorkloadUnproven,
        ..
      }))
    ),
    "{bind:?}"
  );
  client.detach(attached.attachment).unwrap();
  assert!(
    eventually(|| mounted_at(&point.path).is_none()),
    "the detach unmounted"
  );
  assert_eq!(client.status(volume).unwrap().attachments, 0);
}

/// A second mount sees what the first wrote; the kernel's own unmount ends its attachment.
fn remount_and_unmount_from_the_kernel(
  client: &mut slates_client::Client,
  volume: slates_client::VolumeId,
) {
  let point = target_dir();
  client
    .attach_fuse(volume, Intent::Write, &point.path)
    .unwrap();
  #[allow(clippy::disallowed_methods)]
  let read = std::fs::read(format!("{}/src/notes.txt", point.path)).unwrap();
  assert_eq!(read, b"first draft", "the volume outlived its first mount");
  let unmounted = Command::new("fusermount3")
    .args(["-u", &point.path])
    .status()
    .unwrap();
  assert!(unmounted.success());
  assert!(
    eventually(|| client.status(volume).unwrap().attachments == 0),
    "the kernel's unmount ended the attachment"
  );
}

/// A mount point that is not a directory is refused typed before any effect: nothing mounted there, nothing
/// recorded.
fn a_file_is_no_mount_point(client: &mut slates_client::Client, volume: slates_client::VolumeId) {
  let dir = target_dir();
  let file = format!("{}/plain-file", dir.path);
  #[allow(clippy::disallowed_methods)]
  std::fs::write(&file, b"x").unwrap();
  let refused = client.attach_fuse(volume, Intent::Write, &file);
  assert!(
    matches!(
      refused,
      Err(ClientError::Refused(Refusal::TargetUnavailable { .. }))
    ),
    "a mount point that is not a directory is refused typed: {refused:?}"
  );
  assert!(
    mounted_at(&file).is_none(),
    "nothing was mounted over the file"
  );
  assert_eq!(
    client.status(volume).unwrap().attachments,
    0,
    "nothing recorded"
  );
}

/// AUD-29-64 (the daemon serves Linux FUSE). Do: start a daemon, create a volume, and attach it with the
/// FUSE form at a fresh directory; work through the kernel; read the mount table; detach; attach again and
/// unmount from the kernel's side (`fusermount3 -u`); ask for a mount over a regular file; attach again and
/// stop the daemon. Expect: the attach answers with the mount point once mounted; every call works and reads
/// back, and a second mount sees the first's work; the table shows `fuse.slates` with the source naming this
/// attachment (`slates:<attachment>`); the detach and the kernel's unmount each leave no mount and the volume
/// with no attachment; the file is refused typed with nothing mounted or recorded; the daemon's stop leaves
/// no mount; and no barrier was refused.
#[test]
fn a_volume_mounted_through_fuse_serves_the_kernel_and_ends_with_its_mount() {
  if !command_available("fusermount3") || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
    return;
  }
  let profile = common::machine_profile();
  let instance = format!("srv-fuse-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-fuse-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client.create(&scratch(VOLUME)).unwrap();
  mount_work_and_detach(&mut client, volume);
  remount_and_unmount_from_the_kernel(&mut client, volume);
  a_file_is_no_mount_point(&mut client, volume);
  let last = target_dir();
  client
    .attach_fuse(volume, Intent::Write, &last.path)
    .unwrap();
  let refusals = daemon.fleet_refusals().unwrap();
  daemon.stop();
  assert!(
    mounted_at(&last.path).is_none(),
    "the daemon's stop unmounted"
  );
  assert_eq!(refusals.get("fuse.barrier_refused"), None, "{refusals:?}");
}
