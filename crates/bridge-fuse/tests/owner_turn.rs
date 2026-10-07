//! The owner's serve turn over a real Linux FUSE mount (§4.6, §4.8 barrier, D-18, R6): the mount is
//! established and torn down through the non-blocking helper handshake (`begin_mount`, `begin_unmount`) —
//! the form a shard drives without blocking — and served by `dispatch_ready` and `send_reply`, so the owner
//! makes a mutation durable between the dispatch and the reply. A refused barrier answers the caller `EIO`
//! instead of the reply, so a promise of survival is never made without the durable record behind it.
//!
//! Linux only, gated: skips loudly without `fusermount3` or `/dev/fuse` (the CI Linux lane has both;
//! locally, a container with `--device /dev/fuse --cap-add SYS_ADMIN`, run as an ordinary user). The mount
//! point is in the build output (`CARGO_TARGET_TMPDIR`; A-50), named with the process id, unmounted and
//! removed at the end.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::fd::OwnedFd;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use slates_bridge_core::{Attachments, Rights, View};
use slates_bridge_fuse::abi::Opcode;
use slates_bridge_fuse::channel::{
  FuseChannel, ServeState, Turn, dispatch_ready, reclaim_dispatched, redeliver_after_refusal,
  send_reply,
};
use slates_bridge_fuse::mount::{Awaiting, Mount, Progress, begin_mount, begin_unmount};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};

mod common;
use common::{store, volume_for_owner};

/// Shape: how long the helper may take to mount or unmount, and the serve loop to report.
const HELPER_WAIT: Duration = Duration::from_secs(60);
/// Shape: the serve loop's readiness wait between checks of its asks (milliseconds) — short, so an ask
/// sent while no request is in flight is seen promptly.
const WAIT_MS: i64 = 5;
/// Shape: the volume id.
const VOLUME: VolumeId = VolumeId { bytes: [9; 16] };
/// Format: `EIO`, the errno a refused barrier answers with.
const EIO: i32 = 5;
/// Shape: the bytes one allocation slice may materialize — one small page, so the test's 1 MiB allocation takes
/// many turns and the stepping is exercised (the daemon sizes it from the shard's quantum, A-108).
const SLICE_BYTES: u64 = 4096;
/// Shape: the test allocation's length, 1 MiB.
const ALLOCATED: u64 = 1 << 20;

/// What the loop counted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Counts {
  /// The opcodes whose reply waited for a barrier, in order.
  barriered: Vec<Opcode>,
  /// The barriers refused (answered `EIO`).
  refused: u64,
  /// The references and handles given back for the refused replies (AUD-29-85).
  reclaimed: u64,
  /// The turns an allocation continued in (A-108): the non-vacuity counter of the stepping.
  continued: u64,
}

fn command_available(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// The test's scratch in the build output, unmounted lazily and removed on drop.
struct Scratch {
  root: String,
  mount_point: String,
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = Command::new("fusermount3")
      .args(["-u", "-z", &self.mount_point])
      .output();
    let _ = Command::new("rm").args(["-rf", &self.root]).output();
  }
}

fn scratch() -> Scratch {
  let root = format!(
    "{}/slates-owner-turn-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  let mount_point = format!("{root}/mnt");
  assert!(
    Command::new("mkdir")
      .args(["-p", &mount_point])
      .status()
      .unwrap()
      .success()
  );
  Scratch { root, mount_point }
}

/// Polls `poll` until it is done, waiting on `ready` (a descriptor's readability) while it awaits one, and
/// a short pause while it awaits an exit; `None` past [`HELPER_WAIT`].
fn settle<T>(mut poll: impl FnMut() -> Option<T>, ready: impl Fn() -> Option<i32>) -> Option<T> {
  let started = Instant::now();
  while started.elapsed() < HELPER_WAIT {
    if let Some(done) = poll() {
      return Some(done);
    }
    if let Some(fd) = ready() {
      // SAFETY: `fd` names the handshake's socket, which the pending handshake keeps open across this wait.
      let socket = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
      let mut fds = [rustix::event::PollFd::new(
        &socket,
        rustix::event::PollFlags::IN,
      )];
      let _ = rustix::event::poll(
        &mut fds,
        Some(&rustix::time::Timespec {
          tv_sec: 0,
          tv_nsec: WAIT_MS * 1_000_000,
        }),
      );
    } else {
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_millis(1));
    }
  }
  None
}

/// Mounts at `mount_point` through the non-blocking handshake.
fn mount_without_blocking(mount_point: &str) -> Result<Mount, String> {
  let mut pending = begin_mount(mount_point, "slates", &[]).map_err(|e| e.to_string())?;
  let socket = std::os::fd::AsRawFd::as_raw_fd(&pending.socket());
  let awaiting = std::cell::Cell::new(Awaiting::Socket);
  let device: Option<Result<OwnedFd, String>> = settle(
    || match pending.poll() {
      Progress::Waiting(next) => {
        awaiting.set(next);
        None
      }
      Progress::Done(outcome) => Some(outcome.map_err(|e| e.to_string())),
    },
    || (awaiting.get() == Awaiting::Socket).then_some(socket),
  );
  let device = device.ok_or("the mount helper did not finish")??;
  Mount::adopt(device, mount_point).map_err(|e| e.to_string())
}

/// The serve loop as a shard drives it: wait for the device's readiness, take one turn, run the barrier
/// for a mutation (refused when the test asked), then reply. Ends when the kernel unmounts.
fn serve(mut channel: FuseChannel, refuse: Receiver<()>, counts: Sender<Counts>) {
  let mut store = store();
  let uid = rustix::process::getuid().as_raw();
  let gid = rustix::process::getgid().as_raw();
  let mut volume = volume_for_owner(&mut store, uid, gid);
  let mut attachments = Attachments::new();
  let rights = Rights {
    read: true,
    write: true,
  };
  let transport = attachments
    .attach(VOLUME, View::Current, Principal::Uid { uid }, rights)
    .unwrap();
  attachments.set_coherence(transport, slates_bridge_core::CacheCoherence::Invalidated);
  let mut bridge = VolumeBridge::new(VOLUME, &mut volume, &mut store);
  let mut state = ServeState::new();
  let mut tally = Counts::default();
  let mut refuse_next = false;
  loop {
    let raw = channel.raw_device();
    // SAFETY: `raw` is the channel's own device, open for the whole loop.
    let device = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    let mut fds = [rustix::event::PollFd::new(
      &device,
      rustix::event::PollFlags::IN,
    )];
    let _ = rustix::event::poll(
      &mut fds,
      Some(&rustix::time::Timespec {
        tv_sec: 0,
        tv_nsec: WAIT_MS * 1_000_000,
      }),
    );
    if refuse.try_recv().is_ok() {
      refuse_next = true;
    }
    match dispatch_ready(
      &mut channel,
      &mut bridge,
      &mut attachments,
      transport,
      &mut state,
      SLICE_BYTES,
    )
    .unwrap()
    {
      Turn::Idle | Turn::Dropped | Turn::Replayed => {}
      Turn::Continued => tally.continued += 1,
      Turn::Ended => {
        // The test thread may already have failed and gone; then there is no one to tell.
        let _ = counts.send(tally);
        return;
      }
      Turn::Dispatched(dispatched) => {
        let refused = dispatched.needs_barrier() && std::mem::take(&mut refuse_next);
        if dispatched.needs_barrier() {
          tally.barriered.push(dispatched.opcode.unwrap());
        }
        if refused {
          tally.refused += 1;
          let given_back = reclaim_dispatched(
            &state,
            &dispatched,
            &mut bridge,
            &mut attachments,
            transport,
          );
          tally.reclaimed += given_back.references + given_back.handles;
        }
        send_reply(&channel, &mut state, &dispatched, refused.then_some(EIO)).unwrap();
        if refused {
          // The refused change is in the volume: the kernel is told, as the daemon's serve turn tells it.
          redeliver_after_refusal(
            &channel,
            &mut state,
            &dispatched,
            &mut bridge,
            &mut attachments,
            transport,
          )
          .unwrap();
        }
      }
    }
  }
}

/// `sh -c script` with `$1` the mount point: whether it succeeded, and its stderr.
fn through_the_mount(mount_point: &str, script: &str) -> (bool, String) {
  let out = Command::new("sh")
    .args(["-c", script, "sh", mount_point])
    .output()
    .unwrap();
  (
    out.status.success(),
    String::from_utf8_lossy(&out.stderr).into_owned(),
  )
}

/// The owner's turn (§4.6, §4.8, D-18). Do: mount a volume through the non-blocking handshake and serve it
/// with `dispatch_ready`/`send_reply`; through the kernel, write a file (create, write, close) and read it
/// back; allocate through `fallocate` (A-108, [`allocate_through_the_mount`]); then ask the owner to refuse its next
/// barrier and make a directory; then unmount without blocking. The allocation is stepped in [`SLICE_BYTES`] slices,
/// counted per continued turn, and its reply waits for a barrier.
/// Expect: the create and the close's flush waited for a barrier and the write and the read did not; the
/// refused barrier answered `mkdir` with `EIO` (the caller told, not promised survival) while the directory
/// is in the volume, as an NFS mutation whose publication was refused is, and the lookup reference its lost
/// reply granted was given back (AUD-29-85); and the unmount ended the loop.
#[test]
fn a_mutations_reply_waits_for_its_barrier_and_a_refused_barrier_answers_eio() {
  if !command_available("fusermount3") || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!(
      "SKIP: no fusermount3 or /dev/fuse on this host; the owner's turn needs a real FUSE mount"
    );
    return;
  }
  let scratch = scratch();
  let mounted = match mount_without_blocking(&scratch.mount_point) {
    Ok(mounted) => mounted,
    Err(e) => {
      eprintln!("SKIP: the FUSE mount was refused here ({e})");
      return;
    }
  };
  let (device, mount_point) = mounted.into_parts();
  let (refuse_tx, refuse_rx) = channel_pair();
  let (counts_tx, counts_rx) = channel();
  let server = std::thread::spawn(move || serve(device, refuse_rx, counts_tx));

  let (ok, err) = through_the_mount(&mount_point, "printf hi > \"$1/f\" && cat \"$1/f\"");
  assert!(ok, "write and read back through the mount: {err}");
  allocate_through_the_mount(&mount_point);
  tmpfile_through_the_mount(&mount_point);
  refuse_tx.send(()).unwrap();
  let (ok, err) = through_the_mount(&mount_point, "mkdir \"$1/d\"");
  assert!(
    !ok && err.contains("Input/output error"),
    "the refused barrier answered EIO: {err}"
  );
  let (ok, err) = through_the_mount(&mount_point, "test -d \"$1/d\"");
  assert!(ok, "the directory is in the volume, unpublished: {err}");

  let mut unmount = begin_unmount(&mount_point).unwrap();
  let unmounted = settle(|| unmount.poll(), || None).expect("the unmount helper finished");
  assert!(unmounted.is_ok(), "{unmounted:?}");
  let counts = counts_rx
    .recv_timeout(HELPER_WAIT)
    .expect("the unmount ended the loop");
  server.join().unwrap();
  eprintln!("{counts:?}");
  assert_counts(&counts);
}

/// A-108 through the kernel. Do: `fallocate` 1 MiB of a new file; keep-size allocate inside a written file and past
/// its end; punch a hole. Expect: the new file is 1 MiB and reads zeros; the written file keeps its bytes and size;
/// past the end with keep-size and the punch are `EOPNOTSUPP`, the modes slates does not serve.
fn allocate_through_the_mount(mount_point: &str) {
  let (ok, err) = through_the_mount(
    mount_point,
    &format!(
      "fallocate -l {ALLOCATED} \"$1/big\" && test $(stat -c %s \"$1/big\") = {ALLOCATED} && \
       test $(tr -d '\\000' < \"$1/big\" | wc -c) = 0"
    ),
  );
  assert!(ok, "a 1 MiB allocation is a 1 MiB file of zeros: {err}");
  let (ok, err) = through_the_mount(
    mount_point,
    "fallocate -n -o 0 -l 2 \"$1/f\" && test \"$(cat \"$1/f\")\" = hi && test $(stat -c %s \"$1/f\") = 2",
  );
  assert!(
    ok,
    "a keep-size allocation inside the file keeps its bytes and size: {err}"
  );
  for (mode, what) in [
    ("-n -l 4096", "keep-size past the end"),
    ("-p -o 0 -l 1", "a punch"),
  ] {
    let (ok, err) = through_the_mount(mount_point, &format!("fallocate {mode} \"$1/f\""));
    // util-linux words `EOPNOTSUPP` as "Operation not supported", or "keep size mode is unsupported" for keep-size.
    assert!(
      !ok && err.contains("supported"),
      "{what} is EOPNOTSUPP: {err}"
    );
  }
}

/// `O_TMPFILE` through the kernel (FUSE `TMPFILE`, Linux 6.11+). Do: open an unnamed file in the mount's root, write
/// it, and name it with `linkat` through `/proc/self/fd`; open a second unnamed file and close it unnamed. Expect: the
/// named file reads back what was written; no other entry appears in the directory, the second file's hidden name
/// included, since the server unlinks it before it replies. A kernel older than 6.11 answers `EOPNOTSUPP`, which
/// skips loudly.
#[allow(clippy::disallowed_methods)] // `linkat` names a file inside the slates mount: the volume's RAM, never the disk (R1)
fn tmpfile_through_the_mount(mount_point: &str) {
  use rustix::fs::{AtFlags, CWD, Mode, OFlags};
  let unnamed = OFlags::TMPFILE | OFlags::RDWR;
  let fd = match rustix::fs::open(mount_point, unnamed, Mode::from_raw_mode(0o600)) {
    Ok(fd) => fd,
    Err(rustix::io::Errno::OPNOTSUPP) => {
      eprintln!("SKIP: this kernel has no FUSE TMPFILE (Linux 6.11+)");
      return;
    }
    Err(e) => panic!("O_TMPFILE through the mount: {e}"),
  };
  assert_eq!(rustix::io::write(&fd, b"made unnamed").unwrap(), 12);
  let by_fd = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&fd));
  rustix::fs::linkat(
    CWD,
    by_fd.as_str(),
    CWD,
    format!("{mount_point}/named").as_str(),
    AtFlags::SYMLINK_FOLLOW,
  )
  .unwrap();
  drop(fd);
  drop(rustix::fs::open(mount_point, unnamed, Mode::from_raw_mode(0o600)).unwrap());
  let (ok, err) = through_the_mount(mount_point, "test \"$(cat \"$1/named\")\" = 'made unnamed'");
  assert!(ok, "the linked file reads back: {err}");
  let (ok, err) = through_the_mount(mount_point, "! ls -a \"$1\" | grep -q slates-tmpfile");
  assert!(ok, "no hidden name is left: {err}");
}

/// What the loop must have counted: one refused barrier (the `mkdir`'s), the create and the close's flush
/// barriered, and no write or read ever waiting for one.
fn assert_counts(counts: &Counts) {
  assert_eq!(counts.refused, 1);
  assert_eq!(
    counts.reclaimed, 1,
    "the refused mkdir's lookup reference was given back"
  );
  assert!(
    counts.barriered.contains(&Opcode::Create),
    "the create waited for a barrier: {counts:?}"
  );
  assert!(
    counts.barriered.contains(&Opcode::Flush),
    "the close's flush waited for a barrier: {counts:?}"
  );
  assert!(
    counts.barriered.contains(&Opcode::MkDir),
    "the mkdir's barrier was the refused one: {counts:?}"
  );
  assert!(
    counts.barriered.contains(&Opcode::Fallocate),
    "an allocation's reply waited for a barrier: {counts:?}"
  );
  assert!(
    counts.continued >= ALLOCATED / SLICE_BYTES - 1,
    "the 1 MiB allocation ran in slices, one per turn: {counts:?}"
  );
  assert!(
    !counts
      .barriered
      .iter()
      .any(|op| matches!(op, Opcode::Write | Opcode::Read)),
    "a write and a read never wait for one: {counts:?}"
  );
}

fn channel_pair() -> (Sender<()>, Receiver<()>) {
  channel()
}
