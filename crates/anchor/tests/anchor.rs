//! The anchor's tests (Phase 2 task 1): a segment created by one mapping and attached by
//! another, the header and the published payloads under the seqlock rule, the geometry checks,
//! and supervision of a real child process: the test binary re-invoked as the "daemon", which
//! attaches the segment from its environment, writes heartbeats, and exits; the supervisor
//! restarts it until the derived bound trips and records the crash loop in the segment.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use slates_anchor::layout::State;
use slates_anchor::{
  AnchorError, AnchorSegment, Geometry, RegionKind, RestartPolicy, Step, Supervisor,
};
use slates_machine::facts::Identity;
use slates_mem::Handoff;

/// Format: the environment variable that turns this binary into the supervised child.
const CHILD_BEATS: &str = "SLATES_ANCHOR_TEST_BEATS";
/// Format: the exit code the child leaves with.
const CHILD_EXIT: &str = "SLATES_ANCHOR_TEST_EXIT";
/// Shape: how long the parent waits for the child to attach and beat (milliseconds).
const CHILD_WAIT_MS: u64 = 5_000;
/// Shape: a poll pause between supervision steps (microseconds); the test harness times the
/// steps, shipped code never sleeps (D-9).
const POLL_US: u64 = 500;
/// Shape: the child's heartbeats per run and its exit code.
const BEATS: u64 = 5;
/// Shape: the child's exit code.
const EXIT: i32 = 3;
/// Format: the environment variable carrying the port the NFS-listener-inheritance child expects the
/// descriptor it inherited to be bound to, and turning this binary into that child.
#[cfg(unix)]
const CHILD_NFS_PORT: &str = "SLATES_ANCHOR_TEST_NFS_PORT";

fn identity() -> Identity {
  Identity {
    cpu: "test".into(),
    os: "test".into(),
    arch: "test".into(),
    cores: 4,
    memory: 1,
    page: 4096,
  }
}

fn geometry() -> Geometry {
  Geometry {
    partitions: 2,
    page: 4096,
    profile_bytes: 8192,
    log_bytes: 65_536,
    snapshot_bytes: 8192,
    audit_bytes: 8192,
    landing_slots: 2,
    landing_slot_bytes: 4096,
  }
}

fn now_ns(since: Instant) -> u64 {
  u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The handoff and length a segment gives a child, as `Handoff`.
fn handoff_of(segment: &AnchorSegment) -> (Handoff, usize) {
  let env = segment.handoff_env().unwrap();
  let handoff = match env[0].1.parse::<i32>() {
    Ok(fd) if cfg!(target_os = "linux") => Handoff::Descriptor(fd),
    _ => Handoff::Name(env[0].1.clone()),
  };
  (handoff, env[1].1.parse().unwrap())
}

/// The segment is created, published into, and read through a second mapping.
#[test]
fn a_segment_is_created_attached_and_read_through_the_seqlock() {
  let id = identity();
  let mut created = AnchorSegment::create("slates-anchor-test-a", &id, geometry()).unwrap();
  created
    .publish(RegionKind::Profile, b"{\"profile\":1}")
    .unwrap();
  created
    .publish(RegionKind::Snapshot(1, 0), b"snapshot")
    .unwrap();
  created
    .publish(RegionKind::Landing(1), b"manifest")
    .unwrap();
  let (handoff, len) = handoff_of(&created);
  let attached = AnchorSegment::attach(&handoff, len, &id).unwrap();
  assert_eq!(attached.geometry(), geometry());
  assert_eq!(
    attached
      .read_published(RegionKind::Profile)
      .unwrap()
      .unwrap(),
    b"{\"profile\":1}"
  );
  assert_eq!(
    attached
      .read_published(RegionKind::Snapshot(1, 0))
      .unwrap()
      .unwrap(),
    b"snapshot"
  );
  assert_eq!(
    attached.read_published(RegionKind::Snapshot(1, 1)).unwrap(),
    None,
    "never published"
  );
  assert_eq!(
    attached
      .read_published(RegionKind::Landing(1))
      .unwrap()
      .unwrap(),
    b"manifest"
  );
  // The ring words: the capacity is the byte ring's length.
  let words = attached.ring_words(RegionKind::Log(0)).unwrap();
  assert_eq!(words[2].load(Ordering::Acquire), 65_536 - 64);
}

/// A payload larger than its region, another machine's identity and the wrong length are
/// refused with nothing written.
#[test]
fn oversize_payloads_foreign_identities_and_wrong_lengths_are_refused() {
  let id = identity();
  let mut created = AnchorSegment::create("slates-anchor-test-b", &id, geometry()).unwrap();
  created
    .publish(RegionKind::Profile, b"{\"profile\":1}")
    .unwrap();
  let big = vec![7u8; 9000];
  assert!(matches!(
    created.publish(RegionKind::Profile, &big),
    Err(AnchorError::ProfileTooLarge { .. })
  ));
  assert_eq!(
    created
      .read_published(RegionKind::Profile)
      .unwrap()
      .unwrap(),
    b"{\"profile\":1}"
  );
  let (handoff, len) = handoff_of(&created);
  let other = Identity {
    cores: 8,
    ..identity()
  };
  assert!(matches!(
    AnchorSegment::attach(&handoff, len, &other),
    Err(AnchorError::Identity { .. })
  ));
  let (handoff, len) = handoff_of(&created);
  assert!(matches!(
    AnchorSegment::attach(&handoff, len - 4096, &id),
    Err(AnchorError::Geometry { .. }) | Err(AnchorError::Memory(_))
  ));
}

/// The restart bound derives from the recovery budget and the measured start time.
#[test]
fn the_restart_policy_derives_from_the_recovery_budget() {
  let p = RestartPolicy::derive(1_000_000_000, 100_000_000);
  assert_eq!(p.value.max_restarts, 10);
  assert_eq!(p.value.window_ns, 1_000_000_000);
  assert_eq!(
    RestartPolicy::derive(1_000, 1_000_000).value.max_restarts,
    1,
    "at least one"
  );
  assert!(p.anchors.contains(&"daemon_start_p99_ns"));
}

/// The child: attaches from the environment, beats `CHILD_BEATS` times, exits with
/// `CHILD_EXIT`. Ignored so `cargo test` never runs it as an in-process thread (the parent's
/// env mutation would race into it); the parent spawns it with `--ignored --exact`.
#[test]
#[ignore = "the supervised child; run by a_crashing_daemon_... with --ignored"]
fn supervised_child() {
  let Ok(beats) = std::env::var(CHILD_BEATS) else {
    return;
  };
  let beats: u64 = beats.parse().unwrap();
  let exit: i32 = std::env::var(CHILD_EXIT).unwrap().parse().unwrap();
  let segment = AnchorSegment::attach_from_env(&identity()).unwrap();
  let supervision = segment.supervision().unwrap();
  for n in 1..=beats {
    supervision.beat(n);
  }
  std::process::exit(exit);
}

/// The NFS-listener-inheritance child (§4.6): it verifies it inherited the anchor's held listener
/// descriptor across the spawn — reads [`CHILD_NFS_PORT`] (its role signal) and `ENV_NFS_LISTENER`,
/// and exits 0 if that descriptor is a bound listening socket at the expected port, 1 otherwise.
/// Ignored like `supervised_child`, and run by the parent with `--ignored --exact`.
#[cfg(unix)]
#[test]
#[ignore = "the NFS-listener-inheritance child; run by the parent with --ignored"]
fn nfs_listener_child() {
  let Ok(expected) = std::env::var(CHILD_NFS_PORT) else {
    return;
  };
  let expected: u16 = expected.parse().unwrap();
  let raw: i32 = std::env::var(slates_anchor::ENV_NFS_LISTENER)
    .expect("the anchor handed the listener descriptor over")
    .parse()
    .unwrap();
  // SAFETY: the descriptor was inherited from the anchor across the spawn and named to us in the
  // environment; borrowing it for one getsockname does not take ownership (the process owns it).
  let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
  let port = match rustix::net::getsockname(borrowed) {
    Ok(name) => match rustix::net::SocketAddr::try_from(name) {
      Ok(rustix::net::SocketAddr::V4(v4)) => v4.port(),
      _ => 0,
    },
    Err(_) => 0,
  };
  std::process::exit(i32::from(port != expected));
}

/// The daemon inherits the anchor's held NFS listener across the spawn: the parent (the anchor) binds a
/// listener, holds it, and spawns the supervised child, which finds the same listening socket at the
/// bound port on the inherited descriptor — the §4.6 mechanism that keeps the loopback port across a
/// daemon restart. macOS hands the *segment* over by name (not a descriptor), so this is the first
/// proof here that the descriptor hand-off across `Command` works — the daemon's adoption rests on it.
#[cfg(unix)]
#[test]
fn the_supervised_child_inherits_the_held_nfs_listener() {
  use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
  /// Shape: one queued pending connection suffices; nothing connects in this test.
  const BACKLOG: i32 = 1;
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).unwrap();
  let port = listener.local_addr().unwrap().port();
  let fd = listener.into_fd();
  // Make the descriptor inheritable so the spawned child receives it across the exec.
  rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).unwrap();

  let segment = AnchorSegment::create("slates-anchor-test-nfs", &identity(), geometry()).unwrap();
  let exe = std::env::current_exe()
    .unwrap()
    .to_string_lossy()
    .into_owned();
  let args = vec![
    "--ignored".to_owned(),
    "--exact".to_owned(),
    "nfs_listener_child".to_owned(),
    "--nocapture".to_owned(),
  ];
  // Shape: a window that holds this test's one start, restart allowed once.
  let policy = RestartPolicy {
    window_ns: 60_000_000_000,
    max_restarts: 1,
  };
  let mut supervisor = Supervisor::new(segment, &exe, &args, policy);
  supervisor.hold_nfs_listener(fd);
  // SAFETY: the test is single-threaded at this point; the child reads it. The role is chosen by the
  // child's args, so a parallel test's child ignores this variable.
  unsafe {
    std::env::set_var(CHILD_NFS_PORT, port.to_string());
  }
  let clock = Instant::now();
  supervisor.start(now_ns(clock)).unwrap();
  // Drive until the child exits; it exits 0 iff it inherited the listener at the expected port.
  let deadline = Duration::from_millis(CHILD_WAIT_MS);
  let mut outcome = None;
  while clock.elapsed() < deadline {
    match supervisor.step(now_ns(clock)).unwrap() {
      Step::Running => {
        // The test harness paces the poll; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_micros(POLL_US));
      }
      Step::Restarted { exit_code, .. } | Step::CrashLoop { exit_code } => {
        outcome = Some(exit_code);
        break;
      }
      Step::Stopped => break,
    }
  }
  supervisor.stop().ok();
  // SAFETY: single-threaded cleanup.
  unsafe {
    std::env::remove_var(CHILD_NFS_PORT);
  }
  assert_eq!(
    outcome,
    Some(Some(0)),
    "the supervised child inherited the anchor's held listener at the bound port"
  );
}

/// Steps the supervisor until the crash loop (or the deadline): restarts inside the window,
/// whether the child's heartbeats were seen, and the crash loop's exit code.
fn drive_until_crash_loop(
  supervisor: &mut Supervisor,
  clock: Instant,
) -> (u32, bool, Option<Option<i32>>) {
  let mut restarts = 0u32;
  let mut saw_heartbeat = false;
  let deadline = Duration::from_millis(CHILD_WAIT_MS);
  while clock.elapsed() < deadline {
    if supervisor.segment().supervision().unwrap().heartbeat_ns() == BEATS {
      saw_heartbeat = true;
    }
    match supervisor.step(now_ns(clock)).unwrap() {
      Step::Running => {
        // The test harness paces the poll; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_micros(POLL_US));
      }
      Step::Restarted {
        exit_code,
        in_window,
      } => {
        assert_eq!(exit_code, Some(EXIT));
        restarts = in_window;
      }
      Step::CrashLoop { exit_code } => return (restarts, saw_heartbeat, Some(exit_code)),
      Step::Stopped => panic!("stopped without being asked"),
    }
  }
  (restarts, saw_heartbeat, None)
}

/// Supervision over a real child: it attaches through the handoff, its heartbeats are seen
/// through the segment, it is restarted on exit, and the derived bound ends the loop with the
/// state recorded for the health plane.
#[test]
fn a_crashing_daemon_is_restarted_until_the_derived_bound_and_the_segment_says_so() {
  let segment = AnchorSegment::create("slates-anchor-test-sup", &identity(), geometry()).unwrap();
  let exe = std::env::current_exe()
    .unwrap()
    .to_string_lossy()
    .into_owned();
  let args = vec![
    "--ignored".to_owned(),
    "--exact".to_owned(),
    "supervised_child".to_owned(),
    "--nocapture".to_owned(),
  ];
  // Shape: a window long enough to hold every restart of this test, and a bound of three.
  let policy = RestartPolicy {
    window_ns: 60_000_000_000,
    max_restarts: 3,
  };
  let mut supervisor = Supervisor::new(segment, &exe, &args, policy);
  // The child's environment rides on the test process's: the beats and the exit code.
  // SAFETY: the test is single-threaded at this point and the variables are read by the
  // child only.
  unsafe {
    std::env::set_var(CHILD_BEATS, BEATS.to_string());
    std::env::set_var(CHILD_EXIT, EXIT.to_string());
  }
  let clock = Instant::now();
  supervisor.start(now_ns(clock)).unwrap();
  assert!(supervisor.running());
  assert_eq!(
    supervisor.segment().supervision().unwrap().state(),
    State::Running
  );
  assert!(supervisor.segment().supervision().unwrap().pid() > 0);
  let (restarts, saw_heartbeat, outcome) = drive_until_crash_loop(&mut supervisor, clock);
  assert_eq!(
    outcome,
    Some(Some(EXIT)),
    "the bound tripped with the child's exit code"
  );
  assert_eq!(
    restarts, 3,
    "three restarts inside the window, the fourth exit refused"
  );
  assert!(
    saw_heartbeat,
    "the child's heartbeats were seen through the segment"
  );
  assert_crash_loop_recorded(&supervisor);
}

/// The segment records the crash loop for the health plane, and a stopped daemon is not
/// alive (the freshness is the heartbeat's age).
fn assert_crash_loop_recorded(supervisor: &Supervisor) {
  let sup = supervisor.segment().supervision().unwrap();
  assert_eq!(sup.state(), State::CrashLoop);
  assert_eq!(sup.restarts(), 3);
  assert_eq!(sup.generation(), 4, "four starts");
  assert_eq!(sup.pid(), 0);
  assert!(!supervisor.running());
  let (alive, age) = sup.alive(100, 50);
  assert!(!alive);
  assert_eq!(age, 95);
}

/// The anchor's content object — the RAM that backs a shard's volume storage (§4.8) — is held by
/// the process that created it (the supervisor) and handed off in the environment, so content
/// written through one mapping is still there when a restarted daemon re-opens it after the first
/// mapping is gone. This is the memory-level guarantee that an agent's writes survive a daemon
/// crash (BUG-11), the piece a private mapping (the store's old Region::map) could not provide.
#[test]
fn the_content_object_survives_a_daemon_restart_through_the_handoff() {
  let name = format!("slates-anc-{}", std::process::id());
  let content_name = format!("slates-anc-c-{}", std::process::id());
  let content_bytes = 64 * 4096;

  // The supervisor creates the anchor with a content object and holds it for the daemon's life.
  let segment = AnchorSegment::create(&name, &identity(), geometry())
    .unwrap()
    .with_content(&content_name, content_bytes)
    .unwrap();
  let env = segment.handoff_env().unwrap();

  // The running daemon opens the content object and writes an agent's bytes into it.
  let mut daemon = AnchorSegment::open_content(&env).unwrap().unwrap();
  let offset = 8 * 4096;
  let written = b"agent content written before the crash";
  daemon.bytes_mut()[offset..offset + written.len()].copy_from_slice(written);

  // The daemon crashes: its mapping is gone. The supervisor still holds the object alive.
  drop(daemon);

  // The restarted daemon re-opens the content object from the same handoff; the bytes survive.
  let restarted = AnchorSegment::open_content(&env).unwrap().unwrap();
  assert_eq!(
    &restarted.bytes()[offset..offset + written.len()],
    written,
    "content in the anchor's content object survives a daemon restart"
  );

  // A handoff env with no content object opens nothing (a build without anchor-backed storage).
  let plain = AnchorSegment::create(
    &format!("slates-anp-{}", std::process::id()),
    &identity(),
    geometry(),
  )
  .unwrap()
  .handoff_env()
  .unwrap();
  assert!(AnchorSegment::open_content(&plain).is_none());

  let _ = segment; // the supervisor keeps the content object alive across the restart
}
