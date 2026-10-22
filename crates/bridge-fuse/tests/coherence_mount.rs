//! Kernel coherence over a real Linux FUSE mount (§4.6 "Cache posture"; AUD-02): the kernel's
//! cache is warmed and proven warm, a change is made through **another attachment** with no kernel
//! request in flight, and a `stat` through the mount sees it — the change signal woke the serve loop
//! and the invalidation reached the kernel without waiting for a request. Then a seam refusal to
//! gather is injected under a request, and the missed change is still delivered at the next wake.
//! Counters, not stories: the loop counts the `GETATTR`s the kernel actually sent, so a stat answered
//! from the cache and one forced by an invalidation are told apart, and the coherence state counts
//! the refused gather.
//!
//! Linux only, gated: skips loudly without `fusermount3` or `/dev/fuse` (the CI Linux lane has both;
//! locally, a container with `--device /dev/fuse --cap-add SYS_ADMIN`). The mount point is in the
//! build output (`CARGO_TARGET_TMPDIR`; A-50), named with the process id, unmounted and removed at the end.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::unwrap_in_result,
  clippy::panic_in_result_fn
)]

use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use slates_bridge_core::{
  Attachments, Bridge, ObjectId, OpContext, RenameFlags, Rights, SetAttr, View,
};
use slates_bridge_fuse::abi::Opcode;
use slates_bridge_fuse::channel::{
  ChangeNotifier, ChangeSignal, ServeState, Step, serve_step, wait,
};
use slates_bridge_fuse::mount::{Mount, MountError, mount};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

mod common;
use common::{FailingGather, JOURNAL_BYTES, store, volume_for_owner};

/// Shape: how long the helper may take to hand back the device, and a report to arrive.
const MOUNT_WAIT: Duration = Duration::from_secs(60);
/// Shape: the pause between polls of the loop's reports.
const POLL_MS: u64 = 20;
/// Shape: the volume id, the transport's uid (the mounting user) and the other attachment's.
const VOLUME: VolumeId = VolumeId { bytes: [7; 16] };
const OTHER_UID: u32 = 1001;
/// Shape: the file's sizes through the scenario: four bytes written through the mount, nine after
/// the other attachment's truncate, two after the truncate made under the refused gather.
const WRITTEN: &str = "abcd";
const TRUNCATED_TO: u64 = 9;
const TRUNCATED_AGAIN_TO: u64 = 2;

/// What the test asks the serve loop to do between steps, as the daemon's shard would between a
/// kernel request and a verb: another attachment's change, an injected seam refusal, a report.
enum Ask {
  /// Truncate `name` to `size` through the other attachment.
  Truncate { name: String, size: u64 },
  /// Create `name` in the volume's root through the other attachment.
  Create { name: String },
  /// Replace `name`'s bytes with `bytes` through the other attachment, creating it if it is missing.
  Replace { name: String, bytes: Vec<u8> },
  /// Remove `name` from the volume's root through the other attachment, if it is there.
  Remove { name: String },
  /// Rename `from` to `to` in the volume's root through the other attachment, if `from` is there.
  Rename { from: String, to: String },
  /// Write `name` `times` times through the other attachment, one journal record each, creating it if it is missing.
  Churn { name: String, times: usize },
  /// Write `bytes` at `offset` of `name` through the other attachment, creating it if it is missing; nothing truncated.
  WriteAt {
    name: String,
    offset: u64,
    bytes: Vec<u8>,
  },
  /// Send the volume's root files and their bytes, by name: the oracle a generated history is held to.
  Snapshot,
  /// Refuse the next invalidation gather (the injected collection failure).
  RefuseNextGather,
  /// Refuse every invalidation gather until [`Ask::ReleaseGathers`]: the mount's rounds stay owed, held back from its
  /// kernel, which keeps every page it caches meanwhile.
  HoldGathers,
  /// Let gathers through again: the owed round is delivered at the next wake.
  ReleaseGathers,
  /// Report the loop's counters — answered after the step the ask woke, so a report is a barrier:
  /// every ask before it is applied and the round they woke is delivered.
  Report,
}

/// The loop's counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Report {
  /// `GETATTR` requests the kernel sent (a stat answered from the cache sends none).
  getattrs: u64,
  /// Gathers the seam refused.
  gather_refusals: u64,
  /// `READ` requests the kernel sent (a read answered from its page cache sends none).
  reads: u64,
  /// Rounds that found the mount behind the journal's retention (A-141).
  overruns: u64,
  /// `WRITE` requests the kernel sent, write-backs from its page cache included.
  writes: u64,
  /// Bytes of cache write-backs kept back as stale (the dirty-page write-back rule, A-140).
  kept_back: u64,
  /// Cache write-backs refused because which bytes were stale could not be told.
  unknown: u64,
  /// Entry invalidations the kernel refused, as `fuse.entry_invalidation_refused` counts them.
  entry_refusals: u64,
}

fn command_available(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// A RAM-backed scratch directory named with the pid, unmounted (lazily, in case the loop is still
/// draining) and removed on drop so a failed assertion leaves nothing behind.
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

/// A scratch directory of its own for the test named `tag`: the tests of this file run at once.
fn scratch(tag: &str) -> Scratch {
  let root = format!(
    "{}/slates-coherence-{tag}-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  let mount_point = format!("{root}/mnt");
  let made = Command::new("mkdir")
    .args(["-p", &mount_point])
    .status()
    .unwrap();
  assert!(made.success(), "mkdir -p {mount_point}");
  Scratch { root, mount_point }
}

/// `sh -c script` with `$1` the mount point; its stdout, trimmed. A failure is the test's.
fn through_the_mount(mount_point: &str, script: &str) -> String {
  let out = Command::new("sh")
    .args(["-c", script, "sh", mount_point])
    .output()
    .unwrap();
  assert!(
    out.status.success(),
    "{script}: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The size `stat` reports for `$1/f` through the kernel.
fn size_of_f(mount_point: &str) -> String {
  through_the_mount(mount_point, "stat -c %s \"$1/f\"")
}

/// The serve loop, as the daemon's shard would run it: every wake first applies what the other
/// parties asked (another attachment's change, an injected refusal, a report), then takes one step.
/// Owns the mount, the volume and the attachments; ends when the kernel unmounts.
fn serve(
  mut mounted: Mount,
  signal: ChangeSignal,
  asks: Receiver<Ask>,
  (reports, snapshots): (Sender<Report>, Sender<Files>),
) -> Result<(), slates_bridge_fuse::channel::ChannelError> {
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
  // The channel delivers invalidations before each request (AUD-29-79).
  attachments.set_coherence(transport, slates_bridge_core::CacheCoherence::Invalidated);
  let other = attachments
    .attach(
      VOLUME,
      View::Current,
      Principal::Uid { uid: OTHER_UID },
      rights,
    )
    .unwrap();
  let mut bridge = FailingGather {
    inner: VolumeBridge::new(VOLUME, &mut volume, &mut store),
    refuse_gathers: 0,
  };
  let mut state = ServeState::new();
  let mut getattrs = 0u64;
  let mut reads = 0u64;
  let mut overruns = 0u64;
  let mut writes = 0u64;
  let mut kept_back = 0u64;
  let mut unknown = 0u64;
  let mut entry_refusals = 0u64;
  let mut report_pending = false;
  loop {
    let wake = match wait(mounted.channel(), Some(&signal)) {
      Ok(wake) => wake,
      Err(e) => {
        eprintln!("the serve loop's wait failed: {e}");
        return Err(e);
      }
    };
    while let Ok(ask) = asks.try_recv() {
      match ask {
        Ask::Truncate { name, size } => {
          let cx = attachments.begin(other).unwrap();
          let root = bridge.root(&cx).unwrap();
          let file = bridge.lookup(ObjectId::new(root, 0), &cx, &name).unwrap();
          bridge
            .setattr(
              ObjectId::new(file.ino, file.generation),
              &cx,
              SetAttr {
                size: Some(size),
                ..SetAttr::default()
              },
            )
            .unwrap();
          attachments.end(other);
        }
        Ask::Create { name } => {
          let cx = attachments.begin(other).unwrap();
          let root = bridge.root(&cx).unwrap();
          let (made, fh) = bridge
            .create(ObjectId::new(root, 0), &cx, &name, 0o100644, 1)
            .unwrap();
          bridge
            .release(ObjectId::new(made.ino, made.generation), &cx, fh)
            .unwrap();
          attachments.end(other);
        }
        Ask::Replace { name, bytes } => {
          let cx = attachments.begin(other).unwrap();
          let root = bridge.root(&cx).unwrap();
          let object = file_named(&mut bridge, &cx, root, &name);
          bridge
            .setattr(
              object,
              &cx,
              SetAttr {
                size: Some(0),
                ..SetAttr::default()
              },
            )
            .unwrap();
          bridge.write(object, &cx, 0, &bytes).unwrap();
          attachments.end(other);
        }
        Ask::Remove { name } => {
          let cx = attachments.begin(other).unwrap();
          let root = bridge.root(&cx).unwrap();
          // A generated history may remove a name that is not there: the volume's refusal is the outcome.
          let removed = bridge.unlink(ObjectId::new(root, 0), &cx, &name);
          assert!(
            matches!(removed, Ok(()) | Err(VfsError::NotFound)),
            "{removed:?}"
          );
          attachments.end(other);
        }
        Ask::Rename { from, to } => {
          let cx = attachments.begin(other).unwrap();
          let root = ObjectId::new(bridge.root(&cx).unwrap(), 0);
          let renamed = bridge.rename(root, root, &cx, &from, &to, RenameFlags::default());
          assert!(
            matches!(renamed, Ok(()) | Err(VfsError::NotFound)),
            "{renamed:?}"
          );
          attachments.end(other);
        }
        Ask::Snapshot => {
          let cx = attachments.begin(other).unwrap();
          let root = bridge.root(&cx).unwrap();
          snapshots
            .send(volume_files(&mut bridge, &cx, root))
            .unwrap();
          attachments.end(other);
        }
        Ask::Churn { name, times } => {
          let cx = attachments.begin(other).unwrap();
          let root = bridge.root(&cx).unwrap();
          let object = file_named(&mut bridge, &cx, root, &name);
          for round in 0..times {
            bridge.write(object, &cx, 0, &round.to_le_bytes()).unwrap();
          }
          attachments.end(other);
        }
        Ask::WriteAt {
          name,
          offset,
          bytes,
        } => {
          let cx = attachments.begin(other).unwrap();
          let root = bridge.root(&cx).unwrap();
          let object = file_named(&mut bridge, &cx, root, &name);
          bridge.write(object, &cx, offset, &bytes).unwrap();
          attachments.end(other);
        }
        Ask::RefuseNextGather => bridge.refuse_gathers = 1,
        Ask::HoldGathers => bridge.refuse_gathers = u32::MAX,
        Ask::ReleaseGathers => bridge.refuse_gathers = 0,
        Ask::Report => report_pending = true,
      }
    }
    let step = match serve_step(
      mounted.channel(),
      &mut bridge,
      &mut attachments,
      transport,
      &mut state,
      wake,
    ) {
      Ok(step) => step,
      Err(e) => {
        eprintln!("the serve loop's step failed: {e}");
        return Err(e);
      }
    };
    match step {
      Step::Request {
        opcode: Some(Opcode::GetAttr),
        ..
      } => getattrs += 1,
      Step::Request {
        opcode: Some(Opcode::Read),
        ..
      } => reads += 1,
      Step::Request {
        opcode: Some(Opcode::Write),
        ..
      } => writes += 1,
      Step::Ended => return Ok(()),
      _ => {}
    }
    overruns += state.take_journal_overruns();
    kept_back += state.take_writeback_bytes_kept_back();
    unknown += state.take_writebacks_unknown();
    entry_refusals += state.take_entry_invalidations_refused();
    if std::mem::take(&mut report_pending) {
      reports
        .send(Report {
          getattrs,
          gather_refusals: state.coherence.gather_refusals(),
          reads,
          overruns,
          writes,
          kept_back,
          unknown,
          entry_refusals,
        })
        .unwrap();
    }
  }
}

/// A directory's regular files and their whole bytes, sorted by name.
type Files = Vec<(String, Vec<u8>)>;

/// Shape: the entries a snapshot asks for per directory page.
const SNAPSHOT_PAGE: usize = 64;

/// The file `name` in the root `root`, made empty if it is missing.
fn file_named(bridge: &mut dyn Bridge, cx: &OpContext, root: u64, name: &str) -> ObjectId {
  match bridge.lookup(ObjectId::new(root, 0), cx, name) {
    Ok(found) => ObjectId::new(found.ino, found.generation),
    Err(VfsError::NotFound) => {
      let (made, fh) = bridge
        .create(ObjectId::new(root, 0), cx, name, 0o100644, 1)
        .unwrap();
      let object = ObjectId::new(made.ino, made.generation);
      bridge.release(object, cx, fh).unwrap();
      object
    }
    Err(other) => panic!("lookup {name}: {other:?}"),
  }
}

/// The volume root's regular files and their bytes, read through the seam: the oracle.
fn volume_files(bridge: &mut dyn Bridge, cx: &OpContext, root: u64) -> Files {
  let dir = ObjectId::new(root, 0);
  let fh = bridge.opendir(dir, cx).unwrap();
  let mut files = Files::new();
  let mut cookie = 0;
  loop {
    let page = bridge.readdir(dir, cx, fh, cookie, SNAPSHOT_PAGE).unwrap();
    let Some(last) = page.last() else {
      break;
    };
    cookie = last.cookie;
    for entry in page {
      if entry.kind != Kind::File {
        continue;
      }
      let object = ObjectId::new(entry.ino, 0);
      let size = u32::try_from(bridge.getattr(object, cx).unwrap().size).unwrap();
      let mut bytes = Vec::new();
      bridge.read(object, cx, 0, size, &mut bytes).unwrap();
      files.push((entry.name, bytes));
    }
  }
  bridge.release(dir, cx, fh).unwrap();
  files.sort();
  files
}

/// The mount root's regular files and their bytes, read through the kernel. A name gone between the listing and its
/// read is left out; the caller asks again.
#[allow(clippy::disallowed_methods)] // the test's own reads through the slates mount: RAM, not disk
fn mount_files(mount_point: &str) -> Files {
  let mut files: Files = std::fs::read_dir(mount_point)
    .unwrap()
    .filter_map(|entry| {
      let entry = entry.ok()?;
      let name = entry.file_name().into_string().ok()?;
      let bytes = (entry.file_type().ok()?.is_file())
        .then(|| std::fs::read(entry.path()).ok())
        .flatten()?;
      Some((name, bytes))
    })
    .collect();
  files.sort();
  files
}

/// The test's side of the loop: asks, and waits for the answer.
struct Loop {
  asks: Sender<Ask>,
  notifier: ChangeNotifier,
  reports: Receiver<Report>,
  snapshots: Receiver<Files>,
}

impl Loop {
  /// Sends `ask` and wakes the loop so it is applied now (the daemon's verb wake).
  fn ask(&self, ask: Ask) {
    self.asks.send(ask).unwrap();
    self.notifier.notify().unwrap();
  }

  /// Sends `ask` without waking the loop: applied at its next wake, whatever wakes it.
  fn ask_quietly(&self, ask: Ask) {
    self.asks.send(ask).unwrap();
  }

  /// The volume's root files as the loop's seam reads them now: the oracle.
  fn snapshot(&self) -> Files {
    self.ask(Ask::Snapshot);
    let deadline = Instant::now() + MOUNT_WAIT;
    loop {
      if let Ok(files) = self.snapshots.try_recv() {
        return files;
      }
      assert!(
        Instant::now() < deadline,
        "the loop did not snapshot in time"
      );
      // The test harness paces its polls; shipped code parks on its driver (D-9).
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_millis(POLL_MS));
    }
  }

  fn report(&self) -> Report {
    self.ask(Ask::Report);
    let deadline = Instant::now() + MOUNT_WAIT;
    loop {
      if let Ok(report) = self.reports.try_recv() {
        return report;
      }
      assert!(Instant::now() < deadline, "the loop did not report in time");
      // The test harness paces its polls; shipped code parks on its driver (D-9).
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_millis(POLL_MS));
    }
  }
}

/// Runs `phases` against a real FUSE mount served by [`serve`] on a thread of its own, named `tag`; skips loudly when
/// the host cannot mount. The scratch unmounts during unwind before the scope joins its server.
fn with_mounted_loop(tag: &str, phases: impl FnOnce(&Loop, &str)) {
  if !command_available("fusermount3") {
    eprintln!("skipping the mounted coherence proof ({tag}): fusermount3 is not on this host");
    return;
  }
  std::thread::scope(|scope| {
    let scratch = scratch(tag);
    let mounted = match mount(&scratch.mount_point, &[], MOUNT_WAIT) {
      Ok(mounted) => mounted,
      Err(MountError::NoDevice { exit, .. } | MountError::Helper { exit, .. }) => {
        eprintln!(
          "skipping the mounted coherence proof ({tag}): fusermount3 refused the mount or found no /dev/fuse (exit {exit:?})"
        );
        return;
      }
      Err(other) => panic!("the FUSE mount did not come up: {other:?}"),
    };
    let signal = ChangeSignal::new().unwrap();
    let notifier = signal.notifier().unwrap();
    let (asks, ask_end) = channel();
    let (report_end, reports) = channel();
    let (snapshot_end, snapshots) = channel();
    let server = scope.spawn(move || serve(mounted, signal, ask_end, (report_end, snapshot_end)));
    let control = Loop {
      asks,
      notifier,
      reports,
      snapshots,
    };
    let mount_point = scratch.mount_point.clone();
    phases(&control, &mount_point);
    drop(scratch);
    server.join().unwrap().unwrap();
  });
}

/// AUD-02 over a real mount: the kernel's warm, unbounded attribute cache is told of another
/// attachment's change with no kernel request in flight (the change signal), and a gather the seam
/// refuses under a request is retried at the next wake rather than skipped.
#[test]
fn a_change_through_another_attachment_reaches_the_kernel_without_a_request_and_a_refused_gather_is_retried()
 {
  with_mounted_loop("aud02", |control, mount_point| {
    let cached = warm_and_prove_cached(control, mount_point);
    let told = a_change_is_seen_without_a_request(control, mount_point, cached);
    a_refused_gather_is_retried(control, mount_point, told);
    a_name_cached_absent_in_the_root_appears_when_another_attachment_makes_it(control, mount_point);
  });
}

/// Shape: the writes the overrun test makes in one wake: twice the records the scratch volume's journal keeps (each
/// write one record), so the mount's next round finds its cursor past retention.
const CHURN_WRITES: usize =
  2 * JOURNAL_BYTES / std::mem::size_of::<slates_vfs::journal::OpRecord>();
/// Shape: how long a change may take to show through the mount once its round is out: the notify thread's write, far
/// above it.
const DELIVERY_WAIT: Duration = Duration::from_secs(10);

/// Whether `check` holds within [`DELIVERY_WAIT`], asked every [`POLL_MS`].
fn eventually(mut check: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + DELIVERY_WAIT;
  loop {
    if check() {
      return true;
    }
    if Instant::now() >= deadline {
      return false;
    }
    // The test harness paces its polls; shipped code parks on its driver (D-9).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(POLL_MS));
  }
}

/// A-141 over a real mount: a mount that falls behind its volume journal's retention is told that anything it holds
/// may be stale, and invalidates exactly what its kernel holds, so neither a replaced file's page nor a removed name
/// outlives the change (`docs/bugs/2026-10-22-a-mount-that-falls-behind-the-journal-never-hears-of-the-changes-it-missed.md`).
/// Do: through the mount, write `kept` and `gone`, read `kept` and stat `gone`, and prove `kept`'s page cached (a second
/// read sends no `READ`); then, in the loop's one wake, replace `kept` and remove `gone` through another attachment and
/// write a third file [`CHURN_WRITES`] times, past what the journal keeps. Expect the round to count an overrun, `kept`
/// to read the replacement, and `gone` to be absent through the mount.
#[test]
fn a_mount_behind_the_journal_drops_everything_its_kernel_holds() {
  with_mounted_loop("overrun", |control, mount_point| {
    through_the_mount(
      mount_point,
      "printf A > \"$1/kept\"; printf G > \"$1/gone\"",
    );
    assert_eq!(through_the_mount(mount_point, "cat \"$1/kept\""), "A");
    through_the_mount(mount_point, "stat \"$1/gone\" > /dev/null");
    let warm = control.report();
    assert_eq!(through_the_mount(mount_point, "cat \"$1/kept\""), "A");
    let cached = control.report();
    assert_eq!(
      cached.reads, warm.reads,
      "kept's page is the kernel's: a second read sends no READ"
    );

    control.ask_quietly(Ask::Replace {
      name: "kept".to_owned(),
      bytes: b"Z".to_vec(),
    });
    control.ask_quietly(Ask::Remove {
      name: "gone".to_owned(),
    });
    control.ask(Ask::Churn {
      name: "churn".to_owned(),
      times: CHURN_WRITES,
    });
    let told = control.report();
    assert!(
      told.overruns > cached.overruns,
      "the round found the mount behind the journal: {told:?}"
    );
    assert!(
      eventually(|| through_the_mount(mount_point, "cat \"$1/kept\"") == "Z"),
      "the replacement is read through the mount"
    );
    let probe = "test -e \"$1/gone\" && echo present || echo absent";
    assert!(
      eventually(|| through_the_mount(mount_point, probe) == "absent"),
      "the removed name is gone through the mount"
    );
  });
}

/// A name the kernel caches as absent (a lookup miss is a negative entry for the directory's lifetime) in the
/// volume's root, made through another attachment: the mount sees it at once. The root is the one directory the
/// kernel names differently (node id 1) from the volume (its root inode), so its invalidations must be addressed
/// to node id 1; before 2026-10-06 they named the volume's root inode, which the kernel does not know, and every
/// change to a name in the root through another attachment was lost to a mount that had cached it.
fn a_name_cached_absent_in_the_root_appears_when_another_attachment_makes_it(
  control: &Loop,
  mount_point: &str,
) {
  let probe = "test -e \"$1/g\" && echo present || echo absent";
  assert_eq!(through_the_mount(mount_point, probe).trim(), "absent");
  assert_eq!(
    through_the_mount(mount_point, probe).trim(),
    "absent",
    "and cached so"
  );
  control.ask(Ask::Create {
    name: "g".to_owned(),
  });
  control.report();
  assert_eq!(
    through_the_mount(mount_point, probe).trim(),
    "present",
    "the name another attachment made in the root is seen through the kernel"
  );
}

/// Warms the kernel's cache through the mount (a write, a stat), and proves it warm and unbounded: a
/// second stat sends the loop no `GETATTR` at all. Returns the loop's counters at that point.
fn warm_and_prove_cached(control: &Loop, mount_point: &str) -> Report {
  through_the_mount(mount_point, &format!("printf '{WRITTEN}' > \"$1/f\""));
  assert_eq!(size_of_f(mount_point), WRITTEN.len().to_string());
  let warm = control.report();
  assert_eq!(size_of_f(mount_point), WRITTEN.len().to_string());
  let cached = control.report();
  assert_eq!(
    cached.getattrs, warm.getattrs,
    "a stat of a file the kernel caches forever sends the daemon nothing"
  );
  cached
}

/// Another attachment changes the file while the kernel is answering from its cache. No request is
/// in flight: the change signal wakes the loop, which applies the change and delivers the
/// invalidation (the report is the barrier), so the next stat must ask the daemon — exactly one
/// `GETATTR` — and see the new size.
fn a_change_is_seen_without_a_request(control: &Loop, mount_point: &str, cached: Report) -> Report {
  control.ask(Ask::Truncate {
    name: "f".to_owned(),
    size: TRUNCATED_TO,
  });
  let delivered = control.report();
  assert_eq!(
    delivered.getattrs, cached.getattrs,
    "the change and its delivery involved no kernel request"
  );
  assert_eq!(
    size_of_f(mount_point),
    TRUNCATED_TO.to_string(),
    "the other attachment's truncate is seen through the kernel with no request having woken the loop"
  );
  let told = control.report();
  assert_eq!(
    told.getattrs,
    cached.getattrs + 1,
    "the invalidation forced exactly one GETATTR"
  );
  told
}

/// The injected collection failure: the next change is made and the seam refuses the gather of the
/// round a kernel request runs; that request is served (a lookup of a name that does not exist), the
/// kernel's cache is still stale — owed, not lost — and the next wake (the report's change signal,
/// with nothing else to do) delivers the missed change; the refusal is counted once.
fn a_refused_gather_is_retried(control: &Loop, mount_point: &str, told: Report) {
  control.ask_quietly(Ask::Truncate {
    name: "f".to_owned(),
    size: TRUNCATED_AGAIN_TO,
  });
  control.ask_quietly(Ask::RefuseNextGather);
  // The kernel's request wakes the loop: the truncate is applied, the round's gather refused, the
  // lookup served. The reply is what the shell waits for, so this is a barrier too.
  through_the_mount(mount_point, "stat \"$1/absent\" 2>/dev/null || true");
  assert_eq!(
    size_of_f(mount_point),
    TRUNCATED_TO.to_string(),
    "under the refused gather the kernel still holds the old size: the round is owed, not lost"
  );
  let refused = control.report();
  assert_eq!(refused.gather_refusals, 1, "the refusal was counted once");
  assert_eq!(
    size_of_f(mount_point),
    TRUNCATED_AGAIN_TO.to_string(),
    "the change made before the refused gather is delivered at the next wake"
  );
  let retried = control.report();
  assert_eq!(
    retried.getattrs,
    told.getattrs + 1,
    "the retried delivery forced exactly one more GETATTR"
  );
}

/// Shape: the steps of each generated history.
const CHAOS_STEPS: usize = 60;
/// Shape: the histories' seeds — fixed, so a failure is a reproducible sequence; three, so one lucky sequence cannot
/// pass alone.
const CHAOS_SEEDS: [u64; 3] = [
  0x9e37_79b9_7f4a_7c15,
  0x2545_f491_4f6c_dd1d,
  0x6a09_e667_f3bc_c908,
];
/// Shape: the names a history plays over — few, so its steps collide on them.
const CHAOS_NAMES: [&str; 4] = ["n0", "n1", "n2", "n3"];
/// Shape: the longest file a history writes, in bytes.
const CHAOS_BYTES: u64 = 64;

/// The next value of a fixed-seed xorshift64 generator (Marsaglia, "Xorshift RNGs", 2003), as the CLI's chaos tests use.
fn xorshift(state: &mut u64) -> u64 {
  *state ^= *state << 13;
  *state ^= *state >> 7;
  *state ^= *state << 17;
  *state
}

/// One of [`CHAOS_NAMES`], chosen by `roll`'s bits from `shift`.
fn chaos_name(roll: u64, shift: u32) -> &'static str {
  let len = u64::try_from(CHAOS_NAMES.len()).unwrap();
  let at = usize::try_from((roll >> shift) % len).unwrap();
  CHAOS_NAMES.get(at).copied().unwrap()
}

/// What a mount or the volume shows of a history's files: the listing, and each of [`CHAOS_NAMES`] and the churned
/// file opened by name (`None` when absent). Opening by name matters: a name the kernel still holds stale is found that
/// way even when a fresh listing leaves it out.
type Shown = (Vec<String>, Vec<(String, Option<Vec<u8>>)>);

/// Every name a history opens by name.
fn chaos_names() -> impl Iterator<Item = &'static str> {
  CHAOS_NAMES.into_iter().chain(std::iter::once(CHURNED))
}

/// Shape: the file each overrun writes past what the journal keeps.
const CHURNED: &str = "churn";

/// The volume's view, from its files read through the seam.
fn volume_view(files: &Files) -> Shown {
  let listing = files.iter().map(|(name, _)| name.clone()).collect();
  let by_name = chaos_names()
    .map(|name| {
      let bytes = files
        .iter()
        .find(|(listed, _)| listed == name)
        .map(|(_, bytes)| bytes.clone());
      (name.to_owned(), bytes)
    })
    .collect();
  (listing, by_name)
}

/// The mount's view, read through the kernel.
#[allow(clippy::disallowed_methods)] // the test's own reads through the slates mount: RAM, not disk
fn mount_view(mount_point: &str) -> Shown {
  let listing = mount_files(mount_point)
    .into_iter()
    .map(|(name, _)| name)
    .collect();
  let by_name = chaos_names()
    .map(|name| {
      (
        name.to_owned(),
        std::fs::read(format!("{mount_point}/{name}")).ok(),
      )
    })
    .collect();
  (listing, by_name)
}

/// A-141 chaos over a real mount: what the kernel holds must follow the volume through any mix of other paths'
/// changes, the mount's own changes, directory pages and overruns. Do, from each of [`CHAOS_SEEDS`], [`CHAOS_STEPS`]
/// generated steps over a few names:
/// - another attachment replaces, removes or renames a name, queued without letting the loop run;
/// - an overrun: writes to another file past what the journal keeps, queued behind whatever is queued, so the loop's
///   next round finds the queued changes' records gone;
/// - the mount reads, lists, writes, removes or renames a name.
///
/// After each overrun and each step of the mount, let the loop run its round, then read the volume through the seam.
/// Expect the mount's listing, and every name opened by name, to equal the volume's within [`DELIVERY_WAIT`]; and each
/// history to overrun at least once.
#[test]
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn a_generated_history_with_overruns_leaves_the_mount_equal_to_the_volume() {
  with_mounted_loop("chaos", |control, mount_point| {
    let path = |name: &str| format!("{mount_point}/{name}");
    for seed in CHAOS_SEEDS {
      let mut state = seed;
      let overruns_before = control.report().overruns;
      for step in 0..CHAOS_STEPS {
        let roll = xorshift(&mut state);
        let (name, other) = (chaos_name(roll, 8), chaos_name(roll, 16));
        let byte = u8::try_from((roll >> 24) % 256).unwrap();
        let bytes = vec![byte; usize::try_from(1 + (roll >> 32) % CHAOS_BYTES).unwrap()];
        // A step may name a file that is not there; the refusal is part of the history, not a failure of it.
        let (what, queued) = match roll % 10 {
          0 | 1 => {
            control.ask_quietly(Ask::Replace {
              name: name.to_owned(),
              bytes,
            });
            ("another path replaces", true)
          }
          2 => {
            control.ask_quietly(Ask::Remove {
              name: name.to_owned(),
            });
            ("another path removes", true)
          }
          3 => {
            control.ask_quietly(Ask::Rename {
              from: name.to_owned(),
              to: other.to_owned(),
            });
            ("another path renames", true)
          }
          4 => {
            control.ask_quietly(Ask::Churn {
              name: CHURNED.to_owned(),
              times: CHURN_WRITES,
            });
            ("an overrun behind what was queued", false)
          }
          5 => {
            let _ = std::fs::read(path(name));
            ("the mount reads", false)
          }
          6 => {
            let _ = std::fs::read_dir(mount_point).map(Iterator::count);
            ("the mount lists", false)
          }
          7 => {
            let _ = std::fs::write(path(name), &bytes);
            ("the mount writes", false)
          }
          8 => {
            let _ = std::fs::remove_file(path(name));
            ("the mount removes", false)
          }
          _ => {
            let _ = std::fs::rename(path(name), path(other));
            ("the mount renames", false)
          }
        };
        if queued {
          continue;
        }
        control.report();
        let volume = volume_view(&control.snapshot());
        assert!(
          eventually(|| mount_view(mount_point) == volume),
          "seed {seed:#x}, step {step} ({what} {name} / {other}): the mount shows {:?}, the volume holds {volume:?}",
          mount_view(mount_point)
        );
      }
      assert!(
        control.report().overruns > overruns_before,
        "seed {seed:#x}: the history overran the journal at least once"
      );
    }
  });
}

/// Shape: the bytes a direct-read file starts with, the other attachment's replacement, and the store a map makes, all
/// distinct so each byte names its writer.
const SEED: u8 = b'A';
const REPLACED: u8 = b'S';
const STORED: u8 = b'M';
/// Shape: where in the page the map stores its byte.
const STORE_AT: usize = 10;

/// The first pages of a file in the mount, open for reading and writing and mapped `MAP_SHARED`; unmapped on drop,
/// before the file closes.
struct SharedMap {
  map: *mut u8,
  /// The bytes mapped: a whole number of pages.
  len: usize,
  /// Held so the map's file stays open; it closes after `Drop` unmaps.
  _file: std::fs::File,
}

impl SharedMap {
  /// Opens `path` for reading and writing (the file's first writable open on the mount, which drops its cached pages)
  /// and maps its first `pages` pages shared.
  #[allow(clippy::disallowed_methods)] // the test's own open through the slates mount: RAM, not disk
  fn open(path: &str, pages: usize) -> SharedMap {
    use std::os::fd::AsFd;
    let file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .open(path)
      .unwrap();
    let len = rustix::param::page_size().checked_mul(pages).unwrap();
    // SAFETY: a fresh shared map of `len` bytes of an open file this struct holds, at an address the kernel chooses;
    // nothing else maps it, and `Drop` unmaps it before the file closes.
    let map = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        len,
        rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
        rustix::mm::MapFlags::SHARED,
        file.as_fd(),
        0,
      )
    }
    .unwrap()
    .cast::<u8>();
    SharedMap {
      map,
      len,
      _file: file,
    }
  }

  /// The byte at `at` as the kernel's page holds it, faulting the page in if it is not cached.
  fn byte(&self, at: usize) -> u8 {
    assert!(at < self.len, "a byte within the map");
    // SAFETY: `at` is within the live map made in `open`; a volatile read, so the fault happens here.
    unsafe { self.map.add(at).read_volatile() }
  }

  /// Stores `value` at `at` into the kernel's page, leaving it dirty.
  fn store(&self, at: usize, value: u8) {
    assert!(at < self.len, "a byte within the map");
    // SAFETY: `at` is within the live, writable map made in `open`; volatile, so the store is not elided.
    unsafe { self.map.add(at).write_volatile(value) }
  }

  /// `msync`s the map: its dirty bytes are written back before this returns. The write-back rule may refuse them
  /// (`EIO`), which is the outcome under test, so the result is returned rather than asserted.
  fn sync(&self) -> rustix::io::Result<()> {
    // SAFETY: syncs exactly the live map made in `open`.
    unsafe { rustix::mm::msync(self.map.cast(), self.len, rustix::mm::MsyncFlags::SYNC) }
  }

  /// Whether the kernel holds each of the map's pages in its page cache, one flag a page (`mincore(2)`), as any program
  /// may ask. Reading it faults nothing in.
  fn resident_pages(&self) -> Vec<bool> {
    let page = rustix::param::page_size();
    let mut flags = vec![0u8; self.len / page];
    // SAFETY: the map made in `open` is live for `self.len` bytes from `self.map`, which mmap returned page-aligned;
    // `flags` holds one byte for each of its pages, which is what mincore writes.
    let answered = unsafe { libc::mincore(self.map.cast(), self.len, flags.as_mut_ptr()) };
    assert_eq!(answered, 0, "mincore: {}", std::io::Error::last_os_error());
    flags.iter().map(|flag| flag & 1 == 1).collect()
  }
}

impl Drop for SharedMap {
  fn drop(&mut self) {
    // SAFETY: unmaps exactly the map made in `open`, once; nothing reads it afterwards. The file field closes after.
    let unmapped = unsafe { rustix::mm::munmap(self.map.cast(), self.len) };
    // A failed assertion already unwinding through here must not abort the test in a second panic.
    assert!(
      unmapped.is_ok() || std::thread::panicking(),
      "the map is unmapped: {unmapped:?}"
    );
  }
}

/// Writes `bytes` to `path` through the mount, as the mounting user, and opens it to every user (`0o666`): the other
/// attachment's principal is not its owner, and both parties write it.
#[allow(clippy::disallowed_methods)] // the test's own writes through the slates mount: RAM, not disk
fn write_shared_file(path: &str, bytes: &[u8]) {
  use std::os::unix::fs::PermissionsExt;
  std::fs::write(path, bytes).unwrap();
  std::fs::set_permissions(path, std::fs::Permissions::from_mode(SHARED_FILE_MODE)).unwrap();
}

/// Format: the mode of a file both the mounting user and the other attachment's principal write.
const SHARED_FILE_MODE: u32 = 0o666;

/// Reads `path`'s first `page` bytes with `O_DIRECT`: the bytes the server returned, never the kernel's cached page.
fn read_directly(path: &str, page: usize) -> Vec<u8> {
  read_directly_at(path, 0, page)
}

/// Reads `page` bytes of `path` at `offset` (a page boundary) with `O_DIRECT`, into a page-aligned buffer as a
/// direct-I/O user would.
#[allow(clippy::disallowed_methods)] // the test's own open through the slates mount: RAM, not disk
fn read_directly_at(path: &str, offset: u64, page: usize) -> Vec<u8> {
  use std::os::unix::fs::{FileExt, OpenOptionsExt};
  let file = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(i32::try_from(rustix::fs::OFlags::DIRECT.bits()).unwrap())
    .open(path)
    .unwrap();
  let mut buffer = vec![0u8; page.checked_mul(2).unwrap()];
  let skip = buffer.as_ptr().align_offset(page);
  let aligned = buffer
    .get_mut(skip..skip.checked_add(page).unwrap())
    .unwrap();
  let read = file.read_at(aligned, offset).unwrap();
  aligned.get(..read).unwrap().to_vec()
}

/// Reads `len` bytes of `path` at `offset` through the kernel's page cache: a fill of whatever it does not hold.
#[allow(clippy::disallowed_methods)] // the test's own open through the slates mount: RAM, not disk
fn read_buffered_at(path: &str, offset: u64, len: usize) -> Vec<u8> {
  use std::os::unix::fs::FileExt;
  let file = std::fs::File::open(path).unwrap();
  let mut bytes = vec![0u8; len];
  let read = file.read_at(&mut bytes, offset).unwrap();
  bytes.truncate(read);
  bytes
}

/// Writes `bytes` to `path` at `offset` through the mount with `pwrite`: the mount's own write, sent to it as it is
/// made.
#[allow(clippy::disallowed_methods)] // the test's own write through the slates mount: RAM, not disk
fn write_through_the_mount_at(path: &str, offset: u64, bytes: &[u8]) {
  use std::os::unix::fs::FileExt;
  let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
  file.write_all_at(bytes, offset).unwrap();
}

/// A-144 (§4.6 cache posture; the dirty-page write-back rule, A-140;
/// `docs/bugs/2026-10-22-a-direct-read-made-a-stale-cached-page-fresh.md`). A direct READ is answered from the volume
/// and leaves the kernel's cached page as it was, so it must not make that page's bytes fresh. Do, for a file mapped
/// shared through the mount with its page faulted in: hold the mount's invalidation rounds; replace the file through
/// another attachment; [read the page with `O_DIRECT`]; store a byte through the map into the page the kernel still
/// caches; release the rounds, so the kernel launders the page, and sync the map. Expect the replacement whole in the
/// volume: without the direct read the stale page's bytes are kept back and the sync succeeds; with it, which bytes are
/// stale cannot be told, so the write-back is refused and the sync reports it. Run without and then with the direct
/// read, so it is the only difference. Before A-144 the direct read made the page fresh and its write-back reverted all
/// 4,096 bytes of the replacement (3 of 3 on the box, 2026-10-22).
#[test]
fn a_direct_read_does_not_make_a_cached_page_fresh() {
  with_mounted_loop("odirect", |control, mount_point| {
    let page = rustix::param::page_size();
    for direct in [false, true] {
      let name = format!("direct-{direct}");
      let outcome = store_into_a_stale_cached_page(control, mount_point, &name, page, direct);
      judge_the_write_back(control, &name, page, direct, &outcome);
    }
  });
}

/// What became of a store into a stale cached page: the loop's reports before the release and after the map's sync,
/// and the sync's result.
struct WriteBackOutcome {
  stored: Report,
  written: Report,
  synced: rustix::io::Result<()>,
}

/// The scenario of [`a_direct_read_does_not_make_a_cached_page_fresh`] for file `name` of one `page`: written and mapped
/// through the mount, faulted in, replaced through the other attachment while the rounds are held, read once with
/// `O_DIRECT` when `direct`, stored into through the map, the rounds released and the map synced. Asserts the steps'
/// own preconditions.
fn store_into_a_stale_cached_page(
  control: &Loop,
  mount_point: &str,
  name: &str,
  page: usize,
  direct: bool,
) -> WriteBackOutcome {
  let path = format!("{mount_point}/{name}");
  write_shared_file(&path, &vec![SEED; page]);
  control.report();
  let mapped = SharedMap::open(&path, 1);
  assert_eq!(mapped.byte(0), SEED, "the page is faulted in, fetched");
  control.ask(Ask::HoldGathers);
  control.ask(Ask::Replace {
    name: name.to_owned(),
    bytes: vec![REPLACED; page],
  });
  let reads_before = control.report().reads;
  if direct {
    assert_eq!(
      read_directly(&path, page),
      vec![REPLACED; page],
      "the direct read is answered from the volume"
    );
    assert!(
      control.report().reads > reads_before,
      "non-vacuous: the direct read reached the mount as a READ"
    );
  }
  assert_eq!(
    mapped.byte(0),
    SEED,
    "non-vacuous: with the rounds held, the kernel still caches the page from before the replacement"
  );
  mapped.store(STORE_AT, STORED);
  let stored = control.report();
  control.ask(Ask::ReleaseGathers);
  control.report();
  let synced = mapped.sync();
  drop(mapped);
  WriteBackOutcome {
    stored,
    written: control.report(),
    synced,
  }
}

/// The write-back judged as A-144 says: it reached the mount; without the direct read every byte of the stale page was
/// kept back and the sync succeeded; with it, the write-back was refused as one the states cannot tell and the sync
/// reports it; either way the volume holds the replacement whole.
fn judge_the_write_back(
  control: &Loop,
  name: &str,
  page: usize,
  direct: bool,
  outcome: &WriteBackOutcome,
) {
  let WriteBackOutcome {
    stored,
    written,
    synced,
  } = outcome;
  eprintln!(
    "direct read {direct}: {} WRITE(s) after the store, {} byte(s) kept back, {} refused unknown; the map's sync \
     {synced:?}",
    written.writes - stored.writes,
    written.kept_back - stored.kept_back,
    written.unknown - stored.unknown
  );
  assert!(
    written.writes > stored.writes,
    "non-vacuous: the stored page was written back to the mount"
  );
  let judged = if direct {
    written.unknown > stored.unknown && synced.is_err()
  } else {
    written.kept_back - stored.kept_back == u64::try_from(page).unwrap() && synced.is_ok()
  };
  assert!(
    judged,
    "with the direct read {direct}: without it every stale byte is kept back and the sync succeeds; with it the \
     write-back is refused unknown and the sync says so ({synced:?})"
  );
  let held = control
    .snapshot()
    .into_iter()
    .find(|(file, _)| file == name)
    .map(|(_, bytes)| bytes);
  assert!(
    held == Some(vec![REPLACED; page]),
    "with the direct read {direct}: the replacement survives the stale page's write-back (the map's sync: \
     {synced:?}); the volume holds {:?}",
    held.map(|bytes| bytes.iter().filter(|byte| **byte != REPLACED).count())
  );
}

/// Format: the bytes of one page-table entry on the 64-bit targets slates ships (x86_64 and aarch64).
const PAGE_TABLE_ENTRY_BYTES: usize = 8;
/// Shape: the blocks the ranged-invalidation file spans: a changed first and last, and an untouched one between.
const RANGED_BLOCKS: usize = 3;

/// The largest folio a Linux page cache makes: one page table's worth of pages (a PMD), `page / 8` entries of eight
/// bytes each, so 2 MiB with 4 KiB pages. Folios are aligned to their size, so a changed byte's folio lies within the
/// block of this size around it.
fn largest_folio(page: usize) -> usize {
  page.checked_mul(page / PAGE_TABLE_ENTRY_BYTES).unwrap()
}

/// A-146 (§4.6 cache posture). A change through another path drops from the kernel's cache the bytes it changed, not
/// the whole file. A guest's DAX map of a file shared through the mount (`docs/wip/virtiofs.md` §8 item 7) is held in
/// that cache, and one byte changed elsewhere had dropped all of it. Measured on the box (Linux 7.2.9, crosvm, a 128 MiB
/// map, 2026-10-22): each one-byte change made the guest's next scan take 356–382 ms, against 0.2–0.3 ms warm.
///
/// Do: write a file of [`RANGED_BLOCKS`] blocks through the mount (a block is [`largest_folio`]), map it shared and
/// fault every page in. Through the other attachment, change one byte in the first block, then one in the last. Wait
/// until the last block's changed page has left the cache. The kernel is told of changes one round at a time, each
/// round's messages in order, so by then the first change's invalidation has finished too.
///
/// Expect every page of the middle block, which no change touched, still cached, and the map to read both new bytes
/// and the unchanged ones beside them. Before A-146 every content change dropped the whole file, so the middle block
/// was gone with it.
#[test]
fn a_change_through_another_path_drops_only_the_pages_it_changed() {
  with_mounted_loop("ranged", |control, mount_point| {
    let page = rustix::param::page_size();
    let block = largest_folio(page);
    let len = block.checked_mul(RANGED_BLOCKS).unwrap();
    let name = "ranged";
    let mapped = written_mapped_and_cached(control, &format!("{mount_point}/{name}"), len);
    let first = page + STORE_AT;
    let last = len - page + STORE_AT;
    for at in [first, last] {
      control.ask(Ask::WriteAt {
        name: name.to_owned(),
        offset: u64::try_from(at).unwrap(),
        bytes: vec![REPLACED],
      });
    }
    control.report();
    assert!(
      eventually(|| !mapped.resident_pages()[last / page]),
      "the last block's changed page leaves the kernel's cache"
    );
    let resident = mapped.resident_pages();
    let untouched = &resident[block / page..(len - block) / page];
    let dropped = untouched.iter().filter(|resident| !**resident).count();
    assert_eq!(
      dropped,
      0,
      "the middle block, which no change touched, stays in the kernel's cache: {dropped} of its {} pages were dropped",
      untouched.len()
    );
    assert_eq!(mapped.byte(first), REPLACED, "the first change reads back");
    assert_eq!(mapped.byte(last), REPLACED, "the last change reads back");
    assert_eq!(
      mapped.byte(first - 1),
      SEED,
      "the byte before the first change is unchanged"
    );
    assert_eq!(
      mapped.byte(last + 1),
      SEED,
      "the byte after the last change is unchanged"
    );
  });
}

/// `fuse.entry_invalidation_refused` counts only names the kernel was handed (§4.6 cache posture). The kernel answers
/// `ENOENT` for any name it does not cache, and since A-137 it caches no lookup miss, so a name another path makes that
/// this mount's kernel never looked up gets `ENOENT` with nothing stale. Before, every entry invalidation's `ENOENT`
/// counted, so the count moved for such names (seen on the box during the 2026-10-22 guest run, after the guest polled
/// for a name another mount then made).
///
/// Do: map a one-page file through the mount and fault it in. Through the other attachment, create a name the kernel
/// has never looked up; then, in a later round, change the mapped file, and wait until its page has left the kernel's
/// cache. Rounds go out one at a time, and a round's outcome is taken in before the next is handed off, so by then the
/// creation's outcome is counted. Expect the count unmoved, and the mount to list the new name.
#[test]
fn a_name_the_kernel_was_never_handed_is_not_counted_as_a_refused_invalidation() {
  with_mounted_loop("unseen", |control, mount_point| {
    let page = rustix::param::page_size();
    let sentinel = written_mapped_and_cached(control, &format!("{mount_point}/sentinel"), page);
    let before = control.report().entry_refusals;
    control.ask(Ask::Create {
      name: "unseen".to_owned(),
    });
    control.report();
    control.ask(Ask::WriteAt {
      name: "sentinel".to_owned(),
      offset: 0,
      bytes: vec![REPLACED],
    });
    control.report();
    assert!(
      eventually(|| !sentinel.resident_pages()[0]),
      "the later round reaches the kernel"
    );
    let moved = control.report().entry_refusals - before;
    assert_eq!(
      moved, 0,
      "an invalidation of a name the kernel never held is not a refusal: the count moved by {moved}"
    );
    assert!(
      mount_files(mount_point)
        .iter()
        .any(|(name, _)| name == "unseen"),
      "the mount lists the name the other attachment made"
    );
  });
}

/// A file of `len` bytes of [`SEED`] written to `path` through the mount, mapped shared, and every page faulted in, so
/// the kernel caches all of it.
fn written_mapped_and_cached(control: &Loop, path: &str, len: usize) -> SharedMap {
  let page = rustix::param::page_size();
  write_shared_file(path, &vec![SEED; len]);
  control.report();
  let mapped = SharedMap::open(path, len / page);
  for at in (0..len).step_by(page) {
    assert_eq!(mapped.byte(at), SEED, "page {} is faulted in", at / page);
  }
  assert!(
    mapped.resident_pages().iter().all(|resident| *resident),
    "non-vacuous: every page is in the kernel's cache before the changes"
  );
  mapped
}

/// Shape: the pages the write-back chaos maps.
const WRITEBACK_PAGES: usize = 3;
/// Shape: the steps of each generated write-back history: fewer than either writer's stamps, so every stamp of one
/// history is distinct.
const WRITEBACK_STEPS: usize = 80;
/// Shape: the longest run another path writes in one step, and the mount in one `pwrite`.
const OTHER_RUN: u64 = 48;
const MOUNT_RUN: u64 = 8;
/// Format: the other path's stamps are 1..=120 and the mount's 128..=247, one per step, so a byte's value names its
/// writer and its step; a file starts all zero.
const OTHER_STAMPS: usize = 120;
const MOUNT_STAMP_BASE: usize = 128;

/// One byte's history as the oracle keeps it: the other path's latest write (its step and stamp), and every write the
/// mount made (its map's stores and its own `pwrite`s).
#[derive(Clone, Default)]
struct ByteHistory {
  other: Option<(usize, u8)>,
  mount: Vec<(usize, u8)>,
}

impl ByteHistory {
  /// Whether the volume may hold `value` for this byte after a sync, by the write-back rule (A-140, A-144): the other
  /// path's latest write, or a mount write made after it — never anything older; with no other write, zero or any
  /// mount write. A mount write may be lost (kept back as stale, or refused unknown); another path's write never is.
  fn allows(&self, value: u8) -> bool {
    match self.other {
      Some((step, stamp)) => {
        value == stamp
          || self
            .mount
            .iter()
            .any(|&(made, mounted)| made > step && mounted == value)
      }
      None => value == 0 || self.mount.iter().any(|&(_, mounted)| mounted == value),
    }
  }
}

/// What a generated write-back history exercised, summed over its syncs: the rule's paths, each of which must run.
#[derive(Clone, Copy, Default)]
struct WriteBackPaths {
  kept_back: u64,
  unknown: u64,
  direct_reads: u64,
}

/// One generated write-back history against a real mount, from `seed`: map stores, the mount's own writes, another
/// path's writes, direct and buffered reads, the mount's rounds held and released, and syncs, each sync checked by
/// the oracle. What it exercised.
fn writeback_history(control: &Loop, mount_point: &str, seed: u64) -> WriteBackPaths {
  let page = rustix::param::page_size();
  let len = page.checked_mul(WRITEBACK_PAGES).unwrap();
  let name = format!("chaos-writeback-{seed:x}");
  let path = format!("{mount_point}/{name}");
  write_shared_file(&path, &vec![0; len]);
  control.report();
  let mapped = SharedMap::open(&path, WRITEBACK_PAGES);
  for at in (0..len).step_by(page) {
    mapped.byte(at);
  }
  let mut history = vec![ByteHistory::default(); len];
  let mut paths = WriteBackPaths::default();
  let mut state = seed;
  let start = control.report();
  for step in 0..WRITEBACK_STEPS {
    let roll = xorshift(&mut state);
    let at = usize::try_from(xorshift(&mut state) % u64::try_from(len).unwrap()).unwrap();
    let mount_stamp = u8::try_from(MOUNT_STAMP_BASE + step).unwrap();
    let other_stamp = u8::try_from(1 + step % OTHER_STAMPS).unwrap();
    match roll % 12 {
      0..=2 => {
        mapped.store(at, mount_stamp);
        history.get_mut(at).unwrap().mount.push((step, mount_stamp));
      }
      3 | 4 => {
        let run = usize::try_from(1 + xorshift(&mut state) % OTHER_RUN)
          .unwrap()
          .min(len.saturating_sub(at));
        control.ask(Ask::WriteAt {
          name: name.clone(),
          offset: u64::try_from(at).unwrap(),
          bytes: vec![other_stamp; run],
        });
        for byte in history.get_mut(at..at + run).unwrap() {
          byte.other = Some((step, other_stamp));
        }
      }
      5 => {
        read_directly_at(&path, u64::try_from(at / page * page).unwrap(), page);
        paths.direct_reads += 1;
      }
      6 => {
        read_buffered_at(&path, u64::try_from(at / page * page).unwrap(), page);
      }
      7 => {
        let run = usize::try_from(1 + xorshift(&mut state) % MOUNT_RUN)
          .unwrap()
          .min(len.saturating_sub(at));
        write_through_the_mount_at(&path, u64::try_from(at).unwrap(), &vec![mount_stamp; run]);
        for byte in history.get_mut(at..at + run).unwrap() {
          byte.mount.push((step, mount_stamp));
        }
      }
      8 => control.ask(Ask::HoldGathers),
      9 => control.ask(Ask::ReleaseGathers),
      _ => check_writeback(control, &mapped, &name, &history, (seed, step)),
    }
  }
  control.ask(Ask::ReleaseGathers);
  check_writeback(control, &mapped, &name, &history, (seed, WRITEBACK_STEPS));
  let end = control.report();
  paths.kept_back = end.kept_back - start.kept_back;
  paths.unknown = end.unknown - start.unknown;
  paths
}

/// Syncs the map and holds the volume's file to the oracle: every byte one the write-back rule allows.
fn check_writeback(
  control: &Loop,
  mapped: &SharedMap,
  name: &str,
  history: &[ByteHistory],
  (seed, step): (u64, usize),
) {
  // A refused write-back (EIO) is one of the rule's outcomes, and the oracle allows what it leaves.
  let _refusal_is_an_outcome = mapped.sync();
  control.report();
  let held = control
    .snapshot()
    .into_iter()
    .find(|(file, _)| file == name)
    .map(|(_, bytes)| bytes)
    .unwrap();
  assert_eq!(
    held.len(),
    history.len(),
    "seed {seed:#x}, step {step}: the file keeps its length"
  );
  for (at, (value, byte)) in held.iter().zip(history).enumerate() {
    assert!(
      byte.allows(*value),
      "seed {seed:#x}, step {step}: byte {at} holds {value}, older than the write-back rule allows (the other path's \
       latest {:?}, the mount's writes {:?})",
      byte.other,
      byte.mount
    );
  }
}

/// A-140 and A-144 under generated histories over a real mount: a shared map of three pages takes stores, the mount
/// writes with `pwrite`, another attachment writes runs, pages are read directly and through the cache, and the
/// mount's rounds are held and released, so stale cached pages, ambiguous reads and write-backs meet in every order.
/// Do: three fixed seeds of 80 steps, syncing the map now and then. Expect after every sync: each byte holds the other
/// path's latest write to it or a mount write made after that, never anything older. Non-vacuous: across the seeds,
/// stale bytes were kept back, a write-back over unknown bytes was refused, and direct reads were made.
#[test]
fn generated_map_stores_direct_reads_and_other_writes_never_revert_another_paths_write() {
  with_mounted_loop("writeback-chaos", |control, mount_point| {
    let mut total = WriteBackPaths::default();
    for seed in CHAOS_SEEDS {
      let paths = writeback_history(control, mount_point, seed);
      eprintln!(
        "seed {seed:#x}: {} byte(s) kept back, {} write-back(s) refused unknown, {} direct read(s)",
        paths.kept_back, paths.unknown, paths.direct_reads
      );
      total.kept_back += paths.kept_back;
      total.unknown += paths.unknown;
      total.direct_reads += paths.direct_reads;
    }
    assert!(
      total.kept_back > 0 && total.unknown > 0 && total.direct_reads > 0,
      "non-vacuous: stale bytes kept back ({}), unknown write-backs refused ({}) and direct reads made ({})",
      total.kept_back,
      total.unknown,
      total.direct_reads
    );
  });
}
