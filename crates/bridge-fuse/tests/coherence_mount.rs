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
//! locally, a container with `--device /dev/fuse --cap-add SYS_ADMIN`). The mount point is under
//! `/dev/shm`, named with the process id, unmounted and removed at the end.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use slates_bridge_core::{Attachments, Bridge, ObjectId, Rights, SetAttr, View};
use slates_bridge_fuse::abi::Opcode;
use slates_bridge_fuse::channel::{
  ChangeNotifier, ChangeSignal, ServeState, Step, serve_step, wait,
};
use slates_bridge_fuse::mount::{Mount, MountError, mount};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};

mod common;
use common::{FailingGather, store, volume};

/// Shape: how long the helper may take to hand back the device, and a report to arrive.
const MOUNT_WAIT: Duration = Duration::from_secs(60);
/// Shape: the pause between polls of the loop's reports.
const POLL_MS: u64 = 20;
/// Format: the RAM-backed scratch root every Linux lane uses.
const RAM_ROOT: &str = "/dev/shm";
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
  /// Refuse the next invalidation gather (the injected collection failure).
  RefuseNextGather,
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

fn scratch() -> Scratch {
  let root = format!("{RAM_ROOT}/slates-coherence-{}", std::process::id());
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
  reports: Sender<Report>,
) -> Result<(), slates_bridge_fuse::channel::ChannelError> {
  let mut store = store();
  let mut volume = volume(&mut store);
  let mut attachments = Attachments::new();
  let uid = rustix::process::getuid().as_raw();
  let rights = Rights {
    read: true,
    write: true,
  };
  let transport = attachments
    .attach(VOLUME, View::Current, Principal::Uid { uid }, rights)
    .unwrap();
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
        Ask::RefuseNextGather => bridge.refuse_gathers = 1,
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
      Step::Ended => return Ok(()),
      _ => {}
    }
    if std::mem::take(&mut report_pending) {
      reports
        .send(Report {
          getattrs,
          gather_refusals: state.coherence.gather_refusals(),
        })
        .unwrap();
    }
  }
}

/// The test's side of the loop: asks, and waits for the answer.
struct Loop {
  asks: Sender<Ask>,
  notifier: ChangeNotifier,
  reports: Receiver<Report>,
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

/// AUD-02 over a real mount: the kernel's warm, unbounded attribute cache is told of another
/// attachment's change with no kernel request in flight (the change signal), and a gather the seam
/// refuses under a request is retried at the next wake rather than skipped.
#[test]
fn a_change_through_another_attachment_reaches_the_kernel_without_a_request_and_a_refused_gather_is_retried()
 {
  if !command_available("fusermount3") {
    eprintln!("skipping the mounted coherence proof: fusermount3 is not on this host");
    return;
  }
  let scratch = scratch();
  let mounted = match mount(&scratch.mount_point, &[], MOUNT_WAIT) {
    Ok(mounted) => mounted,
    Err(MountError::NoDevice { exit } | MountError::Helper { exit }) => {
      eprintln!(
        "skipping the mounted coherence proof: fusermount3 refused the mount or found no /dev/fuse (exit {exit:?})"
      );
      return;
    }
    Err(other) => panic!("the FUSE mount did not come up: {other:?}"),
  };
  let signal = ChangeSignal::new().unwrap();
  let notifier = signal.notifier().unwrap();
  let (asks, ask_end) = channel();
  let (report_end, reports) = channel();
  let server = std::thread::spawn(move || serve(mounted, signal, ask_end, report_end));
  let control = Loop {
    asks,
    notifier,
    reports,
  };
  let mount_point = scratch.mount_point.clone();
  let cached = warm_and_prove_cached(&control, &mount_point);
  let told = a_change_is_seen_without_a_request(&control, &mount_point, cached);
  a_refused_gather_is_retried(&control, &mount_point, told);

  drop(scratch);
  server.join().unwrap().unwrap();
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
