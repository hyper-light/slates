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
  // The hard-link pattern git finalizes objects with: the temporary held open, linked to the final name, then
  // removed; the final name must serve the bytes at once (2026-10-01, the OCI lane's git workload).
  let held = std::fs::File::create(format!("{point}/tmp_obj")).unwrap();
  std::io::Write::write_all(&mut &held, b"object bytes").unwrap();
  std::fs::hard_link(format!("{point}/tmp_obj"), format!("{point}/obj")).unwrap();
  std::fs::remove_file(format!("{point}/tmp_obj")).unwrap();
  assert_eq!(
    std::fs::read(format!("{point}/obj")).unwrap(),
    b"object bytes",
    "the second name outlives the first"
  );
  drop(held);
  std::fs::remove_file(format!("{point}/obj")).unwrap();
}

/// Shape: entries in the directory removed recursively — past one readdir page of short names, so the removal
/// lists the directory while it unlinks from it.
const MANY: usize = 100;

/// A directory of [`MANY`] files removed recursively through the kernel (what `rm -r` does: list, unlink each,
/// remove the directory); the error, if any. The kernel still holds its lookups of the directory and its files
/// when the directory goes, so the removal leaves referenced orphans — before 2026-10-01 the removed
/// directory's node was freed under them, every image walk met a stale handle, and the barrier answered `EIO`.
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn remove_a_full_directory(point: &str) -> std::io::Result<()> {
  let dir = format!("{point}/many");
  std::fs::create_dir(&dir)?;
  for n in 0..MANY {
    std::fs::write(format!("{dir}/obj-{n}"), b"x")?;
  }
  std::fs::remove_dir_all(&dir)
}

/// The first mount: attached at a fresh directory, worked through the kernel, the mount table read, then
/// detached — the mount gone and the attachment ended.
fn mount_work_and_detach(
  daemon: &Daemon,
  client: &mut slates_client::Client,
  volume: slates_client::VolumeId,
) {
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
  let removed = remove_a_full_directory(&point.path);
  assert!(
    removed.is_ok(),
    "{removed:?}; refusals {:?}",
    daemon.refusals_on_every_shard()
  );
  // The container bind of this mount: its source authority is verified (the table names this attachment, held
  // to the record), but the mount was made for its user alone, so a container's ids could not reach it: refused
  // `MountNotShared` (a `--shared` mount is the bind source; `a_linux_container_reaches_the_shared_mount_as_its_own_ids`).
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
        reason: slates_client::UnsupportedReason::MountNotShared,
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
  mount_work_and_detach(&daemon, &mut client, volume);
  remount_and_unmount_from_the_kernel(&mut client, volume);
  a_file_is_no_mount_point(&mut client, volume);
  let last = target_dir();
  client
    .attach_fuse(volume, Intent::Write, &last.path)
    .unwrap();
  let refusals = daemon.refusals_on_every_shard().unwrap();
  daemon.stop();
  assert!(
    mounted_at(&last.path).is_none(),
    "the daemon's stop unmounted"
  );
  assert_eq!(refusals.get("fuse.barrier_refused"), None, "{refusals:?}");
  assert_eq!(
    refusals.get("publish.volume_skipped"),
    None,
    "every publish imaged the volume: {refusals:?}"
  );
}

/// AUD-29-64 (a fenced shard's mounts). Do: on a one-shard daemon (its control shard owns the volume), attach a
/// FUSE mount, fence the control shard as a failed consensus publication does, then stop the daemon. Expect:
/// the stop unmounted the mount though the shard refuses every ordinary borrow; before 2026-10-01 the fenced
/// shard's mounts outlived the daemon, answering `ENOTCONN`.
#[test]
fn a_fenced_shards_fuse_mount_ends_with_the_daemon() {
  if !command_available("fusermount3") || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
    return;
  }
  let profile = common::machine_profile();
  let instance = format!("srv-fuse-fenced-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-fuse-fenced-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client.create(&scratch(VOLUME)).unwrap();
  let point = target_dir();
  client
    .attach_fuse(volume, Intent::Write, &point.path)
    .unwrap();
  assert!(mounted_at(&point.path).is_some(), "mounted");
  daemon.inject_consensus_failure().unwrap();
  drop(client);
  daemon.stop();
  assert!(
    mounted_at(&point.path).is_none(),
    "the fenced shard's mount ended with the daemon"
  );
}

/// Lists `dir` through the kernel, sorted.
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn listed(dir: &str) -> Vec<String> {
  let mut names: Vec<String> = std::fs::read_dir(dir)
    .unwrap()
    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
    .collect();
  names.sort();
  names
}

/// Through the whole mount at `point`: `shared/g` holding `inside`, `private/secret` holding `hidden`.
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn shared_and_private(point: &str) {
  std::fs::create_dir(format!("{point}/shared")).unwrap();
  std::fs::create_dir(format!("{point}/private")).unwrap();
  std::fs::write(format!("{point}/shared/g"), b"inside").unwrap();
  std::fs::write(format!("{point}/private/secret"), b"hidden").unwrap();
}

/// AUD-29-76 (a subtree mount, Linux FUSE). Do: through a whole FUSE mount make `shared/g` and
/// `private/secret`; attach `/shared` as a scoped FUSE mount at a second point; list and read it, write `h`
/// through it; rename `shared` to `private/moved` through the whole mount and list the scoped mount again;
/// attach a file as a subtree. Expect: the scoped mount's root is `shared` (`g` alone, then `g` and `h`; the
/// write lands in `shared` as the whole mount sees it); after the rename it still presents the same
/// directory, never `private` (the scope is the directory, not its path); a file is refused `NotDirectory`.
/// Before 2026-10-01 the Linux mount presented the volume whole and `--subtree` was refused.
#[test]
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn a_scoped_fuse_mount_presents_one_directory_and_follows_it() {
  if !command_available("fusermount3") || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
    return;
  }
  let profile = common::machine_profile();
  let instance = format!("srv-fuse-scoped-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-fuse-scoped-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client.create(&scratch(VOLUME)).unwrap();
  let whole = target_dir();
  client
    .attach_fuse(volume, Intent::Write, &whole.path)
    .unwrap();
  shared_and_private(&whole.path);
  let point = target_dir();
  client
    .attach_scoped_fuse(volume, Intent::Write, &point.path, "/shared")
    .unwrap();
  assert_eq!(listed(&point.path), ["g"]);
  assert_eq!(
    std::fs::read(format!("{}/g", point.path)).unwrap(),
    b"inside"
  );
  std::fs::write(format!("{}/h", point.path), b"landed").unwrap();
  assert_eq!(
    std::fs::read(format!("{}/shared/h", whole.path)).unwrap(),
    b"landed"
  );
  std::fs::rename(
    format!("{}/shared", whole.path),
    format!("{}/private/moved", whole.path),
  )
  .unwrap();
  assert_eq!(
    listed(&point.path),
    ["g", "h"],
    "the scope is the directory"
  );
  let file = target_dir();
  let refused = client.attach_scoped_fuse(volume, Intent::Read, &file.path, "/private/secret");
  assert!(
    matches!(&refused, Err(ClientError::Refused(Refusal::BadRequest { reason })) if reason == "NotDirectory"),
    "{refused:?}"
  );
  daemon.stop();
  assert!(
    mounted_at(&point.path).is_none(),
    "the daemon's stop unmounted"
  );
}
