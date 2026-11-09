//! The daemon's Linux FUSE transport by use (§4.6 "Linux"; AUD-29-64): a volume mounted by `attach` with the
//! FUSE form — the daemon running the OS's `fusermount3` without blocking its shard and serving the device on
//! the volume's owner shard — reached by ordinary file calls through the kernel; the mount table naming the
//! attachment; and the mount ended by a `detach`, by the kernel's own unmount, and by the daemon's stop (which, A-102, leaves it in place for its user to unmount), the
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
use common::client::{connect, scratch};
use common::target::target_dir;

/// Shape: two shards, so the volume's owner may not be the shard the client lands on.
const TEST_SHARDS: u16 = 2;
/// Shape: the volume's name.
const VOLUME: &str = "fuse-mounted";
/// Shape: how long an unmount takes to reach the attachment — the helper and the kernel's disconnect, then
/// one serve turn: far past their milliseconds, short enough that a lost end fails the test.
const ENDS_WITHIN: Duration = Duration::from_secs(10);

/// Shape: how long the whole scoped-mount test may take before it is treated as wedged. Its work is milliseconds;
/// this is two orders of magnitude of headroom for a loaded box.
const WEDGED_AFTER: Duration = Duration::from_secs(90);

/// Fails the run loudly if the test wedges, instead of hanging until CI's own timeout.
///
/// This guards a real deadlock that shipped: a notification written on the thread that answers requests blocked in
/// the kernel on a lock only an answered request releases, and the mount stopped forever
/// (`docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md`). A wedged mount cannot be unwound from
/// inside the test — its thread is stuck in the kernel on a file call — so the guard aborts the process with a
/// named reason, which `cargo test --workspace` reports rather than sitting on.
///
/// The flag is a process-lifetime `&'static`, not a shared pointer: one test arms the guard, and slates shares a
/// word that way rather than by reference counting.
static FINISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct Watchdog;

impl Watchdog {
  fn set(what: &'static str) -> Watchdog {
    FINISHED.store(false, std::sync::atomic::Ordering::Relaxed);
    std::thread::spawn(move || {
      let until = Instant::now() + WEDGED_AFTER;
      while Instant::now() < until {
        if FINISHED.load(std::sync::atomic::Ordering::Relaxed) {
          return;
        }
        // A test harness thread watching for a wedge, not a shipped path (clippy.toml's stated exception).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(100));
      }
      eprintln!(
        "WEDGED: {what} made no progress in {WEDGED_AFTER:?} — see \
         docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md"
      );
      std::process::abort();
    });
    Watchdog
  }
}

impl Drop for Watchdog {
  fn drop(&mut self) {
    FINISHED.store(true, std::sync::atomic::Ordering::Relaxed);
  }
}

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
  // The source is this daemon's tag, a random value made at its start (A-227), and never the attachment's id: the mount
  // table is world-readable, so it says only whose mount this is.
  assert_eq!(
    Some(source.as_str()),
    slates_base::own_mounts::fsname().as_deref()
  );
  assert!(
    !source.contains(&format!("{:x}", attached.attachment)),
    "the mount table names no attachment: {source}"
  );
  work_through_the_kernel(&point.path);
  assert_eq!(client.status(volume).unwrap().attachments, 1);
  let removed = remove_a_full_directory(&point.path);
  assert!(
    removed.is_ok(),
    "{removed:?}; refusals {:?}",
    daemon.refusals_on_every_shard()
  );
  bind_reports_that_the_mount_is_not_shared(client, volume, &point.path);
  client.detach(attached.attachment).unwrap();
  assert!(
    eventually(|| mounted_at(&point.path).is_none()),
    "the detach unmounted"
  );
  assert_eq!(client.status(volume).unwrap().attachments, 0);
}

/// The container bind of a mount the daemon made for its own user: the source authority is verified (the
/// table names this attachment, held to the record), and the mount is **not** shared with other users.
///
/// Since 2026-10-20 that is reported rather than refused (`server/src/oci.rs`, whose
/// `a_bind_of_a_mount_not_shared_with_other_users_reports_it_rather_than_refusing` is the unit half): a
/// rootful runtime's processes would meet `EACCES` there and a rootless one would not, and slates cannot see
/// which the harness will drive, so the fact travels in the evidence for the harness's own pre-bind check.
/// Refusing it blocked the unprivileged case R10 prefers and effectively demanded a root edit of
/// `/etc/fuse.conf`.
///
/// This assertion expected the old `MountNotShared` refusal until the first CI run on a host with
/// `/dev/fuse` — the suite skips loudly without one, so this sibling was never run when the rule changed and
/// only the unit half was updated.
fn bind_reports_that_the_mount_is_not_shared(
  client: &mut slates_client::Client,
  volume: slates_client::VolumeId,
  source: &str,
) {
  let bound = client
    .attach_with(
      volume,
      None,
      Intent::Write,
      slates_client::AttachRequest::Oci {
        source: source.to_owned(),
        destination: "/work".to_owned(),
      },
    )
    .expect("an unshared source is bound and its sharing reported");
  let evidence = match &bound.established {
    slates_client::Established::OciBind { binding } => binding.evidence.clone(),
    other => panic!("an OCI request establishes an OCI bind: {other:?}"),
  };
  assert!(
    !evidence.shared_with_other_users,
    "the mount was made for its own user, and the evidence says so rather than the bind refusing: {evidence:?}"
  );
  assert!(
    evidence.names_volume && evidence.fstype == "fuse.slates",
    "the evidence names this volume's own FUSE mount as the source: {evidence:?}"
  );
  client.detach(bound.attachment).unwrap();
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

/// A-102: after its daemon has stopped, the mount at `point` is still in the kernel's table and refuses a write (the
/// kernel answers `ENOTCONN`), so nothing falls through to the directory beneath; its user's `fusermount3 -u` then
/// removes it. Asserts each step, naming `what`.
#[allow(clippy::disallowed_methods)] // a write through the dead mount, refused by the kernel
fn left_dead_until_its_user_unmounts(point: &str, what: &str) {
  assert!(
    mounted_at(point).is_some(),
    "{what}: the mount stays until its user unmounts it"
  );
  let wrote = std::fs::write(format!("{point}/after-the-stop"), b"x");
  assert!(
    wrote.is_err(),
    "{what}: a write to the ended mount is refused"
  );
  let unmounted = Command::new("fusermount3")
    .args(["-u", point])
    .status()
    .unwrap();
  assert!(unmounted.success(), "{what}: its user unmounts it");
  assert!(mounted_at(point).is_none(), "{what}: unmounted");
  let beneath: Vec<_> = std::fs::read_dir(point)
    .unwrap()
    .map(|entry| entry.unwrap().file_name())
    .collect();
  assert!(
    beneath.is_empty(),
    "{what}: nothing reached the disk beneath: {beneath:?}"
  );
}

/// AUD-29-84 on Linux (§4.13 "every later effect", A-28): a consumer's revocation must fence its FUSE mount, as it
/// fences its NFS mount on macOS (`a_revoked_consumers_mount_capability_reaches_nothing`). Do: the account creates a
/// volume, mounts it, writes `f` there and shares the volume with an enrolled consumer; a workload bound to the consumer
/// mounts it for reading at its own point and reads `f`; the human revokes the consumer; then `f` is opened afresh through the
/// consumer's mount, and through the account's. Expect: the consumer's read served before; after, its fresh open
/// refused (a fresh open asks the daemon, where a descriptor held from before could be answered from the kernel's
/// cache); the account's mount still reads. The revocation ends the consumer's attachments before it is acknowledged.
#[test]
#[allow(clippy::disallowed_methods)] // the test's own files, read and written through the slates mounts: RAM, not disk
fn a_revoked_consumers_fuse_mount_reaches_nothing() {
  if !command_available("fusermount3") || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
    return;
  }
  let profile = common::machine_profile();
  let instance = format!("srv-fuse-revoke-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "fuse-revoke"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = rustix::process::getuid().as_raw();
  let mut owner = connect(&instance);
  let mut workload = connect(&instance);
  let (consumer, capability) = owner
    .enroll(
      account,
      slates_server::landing::enroll_proof(&secret, account),
    )
    .unwrap();
  workload.attest(consumer, &capability).unwrap();
  let volume = owner.create(&scratch("fuse-revoked")).unwrap();
  let owners = target_dir();
  let owners_mount = owner
    .attach_fuse(volume, Intent::Write, &owners.path)
    .unwrap();
  std::fs::write(format!("{}/f", owners.path), b"shared bytes").unwrap();
  owner
    .share(
      volume,
      slates_client::Principal::Consumer {
        host: common::this_host(),
        account,
        consumer,
      },
      slates_ipc::protocol::Rights {
        read: true,
        write: true,
        admin: false,
      },
    )
    .unwrap();
  let consumers = target_dir();
  // For reading: the account's mount holds the volume's write lease.
  workload
    .attach_fuse(volume, Intent::Read, &consumers.path)
    .unwrap();
  let before = std::fs::read(format!("{}/f", consumers.path));
  owner
    .revoke(
      consumer,
      slates_server::landing::revoke_proof(&secret, consumer),
    )
    .unwrap();
  let after = std::fs::read(format!("{}/f", consumers.path));
  let owners_read = std::fs::read(format!("{}/f", owners.path));
  owner.detach(owners_mount.attachment).unwrap();
  let _ = Command::new("fusermount3")
    .args(["-u", "-z", &consumers.path])
    .status();
  daemon.stop();
  assert_eq!(
    before.as_deref().ok(),
    Some(&b"shared bytes"[..]),
    "the consumer's mount read before the revocation: {before:?}"
  );
  assert!(
    after.is_err(),
    "the revoked consumer's mount reaches nothing: a fresh open read {after:?}"
  );
  assert_eq!(
    owners_read.as_deref().ok(),
    Some(&b"shared bytes"[..]),
    "the account's own mount is not revoked with the consumer: {owners_read:?}"
  );
}

/// Shape: the file the revoked consumer's descriptor reads: far past what one read and its readahead bring in.
const REVOKED_FILE_BYTES: usize = 1 << 20;
/// Shape: what the consumer reads before the revocation, a page, and where it reads after: the next page, which the
/// kernel's first readahead window takes in (four times a first small read, 16 KiB here; at 32 KiB the read already
/// reached the mount, uncached, 3 of 3 on adacachy2).
const READ_BEFORE: usize = 4096;
/// Shape: see [`READ_BEFORE`].
const UNREAD_AT: u64 = 4096;

/// Format: the daemon's count of inode invalidations revoked mounts sent before they ended: the drop's non-vacuity
/// counter.
const REVOKED_INVALIDATED: &str = "fuse.revoked_invalidated";
/// Format: the daemon's count of revocations acknowledged before a mount they fenced had dropped its kernel's cache.
const REVOKED_OVERDUE: &str = "fuse.revoked_overdue";

/// The daemon's counter `name`, summed over its shards.
fn counter(daemon: &Daemon, name: &str) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(name)
    .copied()
    .unwrap_or(0)
}

/// AUD-29-84's residual on Linux: a descriptor the consumer opened before its revocation must read nothing it had not
/// already read, though the kernel's readahead may have cached bytes past the consumer's read. Do: the account writes a
/// 1 MiB file and shares the volume with an enrolled consumer, whose workload mounts it for reading; the consumer opens
/// the file and reads its first page; the human revokes the consumer; the consumer reads, through the same descriptor, the
/// next page, which it never read. Expect that read refused, `EIO` or `ENOTCONN`: the mount answers `ENOTCONN` while
/// its kernel's cache is dropped and the kernel does once the mount has ended, and the kernel turns a failed page fetch
/// under a buffered read into `EIO` (`EIO` 5 of 5 on adacachy2, Linux 7.3). The revocation is acknowledged only after
/// the mount's kernel dropped what it cached, which the daemon counts, and none was acknowledged before that.
#[test]
#[allow(clippy::disallowed_methods)] // the test's own files, read and written through the slates mounts: RAM, not disk
fn a_revoked_consumers_open_descriptor_reads_nothing_it_had_not_read() {
  use std::os::unix::fs::FileExt;
  if !command_available("fusermount3") || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
    return;
  }
  let profile = common::machine_profile();
  let instance = format!("srv-fuse-revoke-fd-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "fuse-revoke-fd"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = rustix::process::getuid().as_raw();
  let mut owner = connect(&instance);
  let mut workload = connect(&instance);
  let (consumer, capability) = owner
    .enroll(
      account,
      slates_server::landing::enroll_proof(&secret, account),
    )
    .unwrap();
  workload.attest(consumer, &capability).unwrap();
  let volume = owner.create(&scratch("fuse-revoked-fd")).unwrap();
  let owners = target_dir();
  let owners_mount = owner
    .attach_fuse(volume, Intent::Write, &owners.path)
    .unwrap();
  let bytes: Vec<u8> = (0..REVOKED_FILE_BYTES)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  std::fs::write(format!("{}/big", owners.path), &bytes).unwrap();
  owner
    .share(
      volume,
      slates_client::Principal::Consumer {
        host: common::this_host(),
        account,
        consumer,
      },
      slates_ipc::protocol::Rights {
        read: true,
        write: false,
        admin: false,
      },
    )
    .unwrap();
  let consumers = target_dir();
  workload
    .attach_fuse(volume, Intent::Read, &consumers.path)
    .unwrap();
  let held = std::fs::File::open(format!("{}/big", consumers.path)).unwrap();
  let mut first = vec![0u8; READ_BEFORE];
  held.read_exact_at(&mut first, 0).unwrap();
  owner
    .revoke(
      consumer,
      slates_server::landing::revoke_proof(&secret, consumer),
    )
    .unwrap();
  let mut unread = vec![0u8; READ_BEFORE];
  let after = held.read_exact_at(&mut unread, UNREAD_AT);
  drop(held);
  let invalidated = counter(&daemon, REVOKED_INVALIDATED);
  let overdue = counter(&daemon, REVOKED_OVERDUE);
  owner.detach(owners_mount.attachment).unwrap();
  let _ = Command::new("fusermount3")
    .args(["-u", "-z", &consumers.path])
    .status();
  daemon.stop();
  assert_eq!(
    first,
    bytes[..READ_BEFORE],
    "the consumer's first read before the revocation"
  );
  let refused_with = after.as_ref().err().and_then(std::io::Error::raw_os_error);
  assert!(
    [rustix::io::Errno::IO, rustix::io::Errno::NOTCONN]
      .iter()
      .any(|errno| refused_with == Some(errno.raw_os_error())),
    "a descriptor opened before the revocation read bytes it had not read ({} at {UNREAD_AT}): {after:?}",
    READ_BEFORE
  );
  assert!(
    invalidated >= 2,
    "the revoked mount told its kernel to drop the root and the file: {invalidated} invalidations"
  );
  assert_eq!(
    overdue, 0,
    "no revocation was acknowledged before its mount's kernel dropped what it cached"
  );
}

/// AUD-29-64 (the daemon serves Linux FUSE). Do: start a daemon, create a volume, and attach it with the
/// FUSE form at a fresh directory; work through the kernel; read the mount table; detach; attach again and
/// unmount from the kernel's side (`fusermount3 -u`); ask for a mount over a regular file; attach again and
/// stop the daemon. Expect: the attach answers with the mount point once mounted; every call works and reads
/// back, and a second mount sees the first's work; the table shows `fuse.slates` with the source naming this
/// attachment (`slates:<attachment>`); the detach and the kernel's unmount each leave no mount and the volume
/// with no attachment; the file is refused typed with nothing mounted or recorded; the daemon's stop ends the mount
/// but leaves it in place, refusing writes, until its user unmounts it (A-102); and no barrier was refused.
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
      name: common::fixture_object("slates-seg", "fuse"),
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
  left_dead_until_its_user_unmounts(&last.path, "the daemon's stop");
  assert_eq!(refusals.get("fuse.barrier_refused"), None, "{refusals:?}");
  assert_eq!(
    refusals.get("publish.volume_skipped"),
    None,
    "every publish imaged the volume: {refusals:?}"
  );
}

/// AUD-29-64 (a fenced shard's mounts). Do: on a one-shard daemon (its control shard owns the volume), attach a
/// FUSE mount, fence the control shard as a failed consensus publication does, then stop the daemon. Expect: the
/// stop ended the mount though the shard refuses every ordinary borrow: its device dropped, the mount left refusing
/// writes until its user unmounts it (A-102). Before 2026-10-01 the fenced shard's mount kept being served after the
/// daemon's stop.
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
      name: common::fixture_object("slates-seg", "fuse-fenced"),
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
  left_dead_until_its_user_unmounts(&point.path, "the fenced shard's stop");
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
  let _watchdog = Watchdog::set("a_scoped_fuse_mount_presents_one_directory_and_follows_it");
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
      name: common::fixture_object("slates-seg", "fuse-scoped"),
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
  left_dead_until_its_user_unmounts(&point.path, "the daemon's stop");
}
