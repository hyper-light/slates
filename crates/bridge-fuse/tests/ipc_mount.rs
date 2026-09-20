//! A-26 / AC-3.10: real kernel IPC is local and transient; cloned names share no pipe or listener.
//! Requires the same ordinary-user FUSE environment as coherence_mount; all paths are in RAM.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use rustix::fs::{CWD, Mode, OFlags};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use slates_bridge_core::{Attachments, Rights, View, VolumeBridge};
use slates_bridge_fuse::channel::{
  ChangeSignal, ServeState, Step, serve_blocking, serve_step, wait,
};
use slates_bridge_fuse::mount::{Mount, MountError, mount};
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::clock::HostClock;
use slates_vfs::inode::Kind;
use slates_vfs::recover::VolumeImage;
use slates_vfs::volume::{Volume, VolumeConfig};

mod common;

/// Shape: the existing FUSE fixture's bounded helper handshake allowance.
const MOUNT_WAIT: Duration = Duration::from_secs(60);

/// Owns both private mounts; unmounts before scoped server threads are joined on every exit.
struct MountPaths {
  root: String,
  original: String,
  clone: String,
}

impl MountPaths {
  fn new() -> Self {
    let root = format!("/dev/shm/slates-ipc-{}", std::process::id());
    let original = format!("{root}/original");
    let clone = format!("{root}/clone");
    assert!(
      Command::new("mkdir")
        .args(["-p", &original, &clone])
        .status()
        .unwrap()
        .success()
    );
    Self {
      root,
      original,
      clone,
    }
  }
}

impl Drop for MountPaths {
  fn drop(&mut self) {
    for path in [&self.original, &self.clone] {
      let _ = Command::new("fusermount3")
        .args(["-u", "-z", path])
        .output();
    }
    let _ = Command::new("rm").args(["-rf", &self.root]).output();
  }
}

fn original_image() -> VolumeImage {
  let mut store = common::store();
  let mut original = common::volume_for_owner(
    &mut store,
    rustix::process::getuid().as_raw(),
    rustix::process::getgid().as_raw(),
  );
  let root = original.root_inode(&store).unwrap();
  for (name, kind) in [("pipe", Kind::Fifo), ("socket", Kind::Socket)] {
    let inode = original
      .mknod_no(&mut store, root, name, 0o600, kind)
      .unwrap();
    original
      .chown(
        &mut store,
        inode,
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
      )
      .unwrap();
  }
  original.to_image(&store, None).unwrap()
}

fn serve(
  mut mounted: Mount,
  image: VolumeImage,
  identity: u8,
  control: Option<(ChangeSignal, Receiver<SyncSender<VolumeImage>>)>,
) {
  let mut store = common::store();
  let mut volume = Volume::from_image(
    &mut store,
    &image,
    Box::new(HostClock::default()),
    1 << 16,
    None,
  )
  .unwrap();
  let id = VolumeId {
    bytes: [identity; 16],
  };
  let mut attachments = Attachments::new();
  let attachment = attachments
    .attach(
      id,
      View::Current,
      Principal::Uid {
        uid: rustix::process::getuid().as_raw(),
      },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  let Some((signal, requests)) = control else {
    let mut bridge = VolumeBridge::new(id, &mut volume, &mut store);
    serve_blocking(
      mounted.channel(),
      &mut bridge,
      &mut attachments,
      attachment,
      None,
    )
    .unwrap();
    return;
  };
  let mut handles = slates_bridge_core::volume_bridge::new_handle_store();
  let mut state = ServeState::new();
  loop {
    let wake = wait(mounted.channel(), Some(&signal)).unwrap();
    if let Ok(reply) = requests.try_recv() {
      // The mount is between requests: the same barrier the owning shard uses for a snapshot.
      let snapshot = volume.snapshot(&mut store).unwrap();
      let clone = Volume::clone_of(
        &store,
        &mut volume,
        snapshot,
        VolumeConfig {
          prefix: 2,
          names: slates_vfs::names::NameEquivalence::Exact,
          quota: slates_vfs::quota::Quota::Bounded { limit: 1 << 30 },
          journal_bytes: 1 << 16,
          clock: Box::new(HostClock::default()),
        },
      )
      .unwrap();
      reply.send(clone.to_image(&store, None).unwrap()).unwrap();
    }
    let mut bridge = VolumeBridge::attached(id, &mut volume, &mut store, &mut handles, None);
    if serve_step(
      mounted.channel(),
      &mut bridge,
      &mut attachments,
      attachment,
      &mut state,
      wake,
    )
    .unwrap()
      == Step::Ended
    {
      return;
    }
  }
}

/// Kernel endpoints held across the clone: bytes already queued must remain only in the original.
struct LiveIpc {
  reader: rustix::fd::OwnedFd,
  _writer: rustix::fd::OwnedFd,
  _listener: rustix::fd::OwnedFd,
  _client: rustix::fd::OwnedFd,
  accepted: rustix::fd::OwnedFd,
}

fn socket() -> rustix::fd::OwnedFd {
  rustix::net::socket_with(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
    None,
  )
  .unwrap()
}

/// AC-3.10 / A-26: queue pipe and socket bytes before taking the clone.
fn establish_ipc(paths: &MountPaths) -> LiveIpc {
  let reader = rustix::fs::open(
    format!("{}/pipe", paths.original),
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
    Mode::empty(),
  )
  .unwrap();
  let writer = rustix::fs::open(
    format!("{}/pipe", paths.original),
    OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
    Mode::empty(),
  )
  .unwrap();
  assert_eq!(rustix::io::write(&writer, b"hello").unwrap(), 5);
  rustix::fs::unlinkat(
    CWD,
    format!("{}/socket", paths.original),
    rustix::fs::AtFlags::empty(),
  )
  .unwrap();
  let listener = socket();
  let address = SocketAddrUnix::new(format!("{}/socket", paths.original)).unwrap();
  rustix::net::bind(&listener, &address).unwrap();
  rustix::net::listen(&listener, 1).unwrap(); // Shape: one client in this oracle.
  let client = socket();
  rustix::net::connect(&client, &address).unwrap();
  let accepted =
    rustix::net::accept_with(&listener, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK).unwrap();
  assert_eq!(rustix::io::write(&client, b"hello").unwrap(), 5);
  LiveIpc {
    reader,
    _writer: writer,
    _listener: listener,
    _client: client,
    accepted,
  }
}

/// AC-3.10 / A-26: neither queued bytes nor the live listener crosses the snapshot boundary.
fn verify_isolation(paths: &MountPaths, original: &LiveIpc) {
  let cloned_reader = rustix::fs::open(
    format!("{}/pipe", paths.clone),
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
    Mode::empty(),
  )
  .unwrap();
  let mut bytes = [0; 5];
  assert_eq!(
    rustix::io::read(&cloned_reader, &mut bytes).unwrap(),
    0,
    "clone has no pipe writer or queued bytes"
  );
  let client = socket();
  let cloned_socket = SocketAddrUnix::new(format!("{}/socket", paths.clone)).unwrap();
  assert_eq!(
    rustix::net::connect(&client, &cloned_socket),
    Err(rustix::io::Errno::CONNREFUSED)
  );
  for endpoint in [&original.reader, &original.accepted] {
    assert_eq!(rustix::io::read(endpoint, &mut bytes).unwrap(), bytes.len());
    assert_eq!(&bytes, b"hello");
  }
  rustix::fs::mkfifoat(
    CWD,
    format!("{}/new-pipe", paths.original),
    Mode::RUSR | Mode::WUSR,
  )
  .unwrap();
}

/// AC-3.10 / A-26: actual kernel IPC over two mounts of an original and a snapshot clone.
#[test]
fn fifo_and_socket_communication_is_local_to_each_clone() {
  std::thread::scope(|scope| {
    let paths = MountPaths::new();
    let try_mount = |path: &str| match mount(path, &[], MOUNT_WAIT) {
      Ok(mounted) => Some(mounted),
      Err(MountError::NoDevice { .. } | MountError::Helper { .. } | MountError::Spawn { .. }) => {
        eprintln!("skipping mounted IPC proof: fusermount3 or /dev/fuse unavailable");
        None
      }
      Err(error) => panic!("FUSE mount failed: {error}"),
    };
    let Some(original_mount) = try_mount(&paths.original) else {
      return;
    };
    let Some(clone_mount) = try_mount(&paths.clone) else {
      return;
    };
    let signal = ChangeSignal::new().unwrap();
    let notifier = signal.notifier().unwrap();
    // Shape: one in-flight snapshot request and one response, each owned by this test.
    let (requests, inbox) = sync_channel(1);
    let (reply, response) = sync_channel(1);
    let original = original_image();
    let first = scope.spawn(move || serve(original_mount, original, 1, Some((signal, inbox))));
    let endpoints = establish_ipc(&paths);
    requests.send(reply).unwrap();
    notifier.notify().unwrap();
    let clone = response.recv_timeout(MOUNT_WAIT).unwrap();
    let second = scope.spawn(move || serve(clone_mount, clone, 2, None));
    verify_isolation(&paths, &endpoints);
    drop(endpoints);
    drop(paths);
    first.join().unwrap();
    second.join().unwrap();
  });
}
