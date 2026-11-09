//! The anchor's tests (Phase 2 task 1): a segment created by one mapping and attached by
//! another, the header and the published payloads under the seqlock rule, the geometry checks,
//! and supervision of a real child process: the test binary re-invoked as the "daemon", which
//! attaches the segment from its environment, writes heartbeats, and exits; the supervisor
//! restarts it until the derived bound trips and records the crash loop in the segment.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use slates_anchor::layout::State;
use slates_anchor::{
  AnchorError, AnchorSegment, Geometry, RegionKind, RestartPolicy, Step, Stopping, Supervisor,
};
use slates_machine::facts::Identity;
use slates_mem::Handoff;

/// Format: the environment variable that turns this binary into the supervised child.
const CHILD_BEATS: &str = "SLATES_ANCHOR_TEST_BEATS";
/// Format: the exit code the child leaves with.
const CHILD_EXIT: &str = "SLATES_ANCHOR_TEST_EXIT";
/// Format: the environment variable that makes the supervised child mark that it serves before it exits.
const CHILD_SERVES: &str = "SLATES_ANCHOR_TEST_SERVES";
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

/// A segment name unique to this process: a fixed name collides when two runs of these tests overlap
/// on a platform that names its shared objects (macOS, Windows), and the second run's create removes
/// the first's name or meets its object. The re-invoked child learns the name from the handoff.
fn unique_name(test: &str) -> String {
  format!("{test}-{}", std::process::id())
}

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
  let mut created =
    AnchorSegment::create(&unique_name("slates-anchor-test-a"), &id, geometry()).unwrap();
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

/// A-92: do create a segment and read its sealing root, then lock its page, publish a root through one attachment
/// and read it through a later one (a restarted daemon re-attaching the anchor's segment); expect no root on the
/// fresh segment, the page locked, and the later attachment adopting the very id and key the first published.
#[test]
fn a_published_sealing_root_is_adopted_by_a_later_attachment() {
  let id = identity();
  let mut created =
    AnchorSegment::create(&unique_name("slates-anchor-test-seal"), &id, geometry()).unwrap();
  assert_eq!(
    created.seal_root().unwrap(),
    None,
    "a fresh segment holds no root"
  );
  created.protect_seal_page().unwrap();
  let (handoff, len) = handoff_of(&created);
  let mut first = AnchorSegment::attach(&handoff, len, &id).unwrap();
  let (root_id, root_key) = ([7u8; 16], [9u8; 32]);
  first.protect_seal_page().unwrap();
  first.publish_seal_root(&root_id, &root_key).unwrap();
  drop(first);
  let later = AnchorSegment::attach(&handoff, len, &id).unwrap();
  assert_eq!(later.seal_root().unwrap(), Some((root_id, root_key)));
  assert_eq!(
    later.issuer_secret().unwrap(),
    [0u8; 32],
    "the root shares the page with the issuer secret and leaves it alone"
  );
}

/// A payload larger than its region, another machine's identity and the wrong length are
/// refused with nothing written.
#[test]
fn oversize_payloads_foreign_identities_and_wrong_lengths_are_refused() {
  let id = identity();
  let mut created =
    AnchorSegment::create(&unique_name("slates-anchor-test-b"), &id, geometry()).unwrap();
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
  if std::env::var_os(CHILD_SERVES).is_some() {
    supervision.mark_serving(beats);
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
  let fd = listener.into_owned();
  // Make the descriptor inheritable so the spawned child receives it across the exec.
  rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).unwrap();

  let segment = AnchorSegment::create(
    &unique_name("slates-anchor-test-nfs"),
    &identity(),
    geometry(),
  )
  .unwrap();
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
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
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
      Step::Restarted { exit_code, .. } | Step::CrashLoop { exit_code, .. } => {
        outcome = Some(exit_code);
        break;
      }
      Step::Stopped | Step::BackingOff { .. } => break,
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
      Step::CrashLoop { exit_code, .. } => return (restarts, saw_heartbeat, Some(exit_code)),
      Step::BackingOff { .. } => panic!("backing off before a crash loop was reported"),
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
  let segment = AnchorSegment::create(
    &unique_name("slates-anchor-test-sup"),
    &identity(),
    geometry(),
  )
  .unwrap();
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
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
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

/// §2.6 step 1, the restart bound's rule ("a daemon that has failed more times inside the recovery budget than that
/// budget holds daemon starts is not recovering"): a daemon that finished its start and served before it exited was
/// recovering, so its exit is not counted. Do: supervise a child that beats, marks that it serves, and exits, under a
/// window that holds every exit of the test and a bound of three; drive it through twice the bound's exits. Expect each
/// one restarted, never a crash loop, never a back-off. On main 80be552a every exit counted, so a daemon killed while
/// serving was judged a crash loop at the fourth kill: `a_winfsp_drive_handle_and_view_survive_twenty_killed_daemons`
/// was backed off 16 s at kill 20 on both Windows runners, every earlier successor having served.
#[test]
fn a_daemon_killed_after_it_served_is_restarted_every_time() {
  /// Shape: the bound the window holds, and twice it, the exits driven.
  const BOUND: u32 = 3;
  let segment = AnchorSegment::create(
    &unique_name("slates-anchor-test-served"),
    &identity(),
    geometry(),
  )
  .unwrap();
  let args = vec![
    "--ignored".to_owned(),
    "--exact".to_owned(),
    "supervised_child".to_owned(),
    "--nocapture".to_owned(),
  ];
  let policy = RestartPolicy {
    window_ns: 60_000_000_000,
    max_restarts: BOUND,
  };
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
  // SAFETY: the test sets the child's environment before it spawns any child; the variables are read by the child only.
  unsafe {
    std::env::set_var(CHILD_BEATS, BEATS.to_string());
    std::env::set_var(CHILD_EXIT, EXIT.to_string());
    std::env::set_var(CHILD_SERVES, "1");
  }
  let clock = Instant::now();
  supervisor.start(now_ns(clock)).unwrap();
  let deadline = Duration::from_millis(CHILD_WAIT_MS);
  let mut exits = 0u32;
  let mut refused = None;
  while exits < BOUND * 2 && clock.elapsed() < deadline * (BOUND * 2) {
    match supervisor.step(now_ns(clock)).unwrap() {
      Step::Running => {
        // The test harness paces the poll; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_micros(POLL_US));
      }
      Step::Restarted { exit_code, .. } => {
        assert_eq!(exit_code, Some(EXIT));
        exits += 1;
      }
      other => {
        refused = Some(format!("{other:?}"));
        break;
      }
    }
  }
  // SAFETY: as above; no child reads it after this.
  unsafe {
    std::env::remove_var(CHILD_SERVES);
  }
  assert_eq!(
    (exits, refused),
    (BOUND * 2, None),
    "a daemon that served is restarted at every exit, past the bound of {BOUND}"
  );
}

/// A-119, supervision never gives up: do supervise a child that beats and exits at once, under a short window that
/// holds one restart. Expect the crash loop reported with a restart scheduled a window away, the supervisor backing off
/// until then (the segment saying `CrashLoop`), the daemon started again once the window passed (the segment
/// `Running`), and the next crash loop's backoff twice the first. Before A-119 the first crash loop ended supervision for
/// good, and every hard NFS mount on the daemon hung (2026-10-08, `realworld_chaos.sh` on a loaded host).
///
/// The supervisor is driven by the test's own clock, which stands still while a child lives and jumps to each
/// backoff's end: the claim is about a daemon that exits **at once**, so its life takes no time on that clock however
/// long the machine takes to run it. On the wall clock, a re-invoked child under eight spinners a CPU outlived the
/// 300 ms window, so no two exits fell in one window and no crash loop was seen: main failed it 2 of 3 times on this
/// Mac at 212-238 runnable processes, and once on windows-latest beside two spinners a CPU (diag-liveness-10).
#[test]
fn a_crash_loop_is_restarted_after_a_doubling_backoff_never_given_up() {
  /// Shape: the window and the backoff it starts.
  const WINDOW_NS: u64 = 300_000_000;
  let segment = AnchorSegment::create(
    &unique_name("slates-anchor-test-backoff"),
    &identity(),
    geometry(),
  )
  .unwrap();
  let args = vec![
    "--ignored".to_owned(),
    "--exact".to_owned(),
    "supervised_child".to_owned(),
    "--nocapture".to_owned(),
  ];
  let policy = RestartPolicy {
    window_ns: WINDOW_NS,
    max_restarts: 1,
  };
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
  // The child beats and exits, as the bound's own test's does; the same values, since the variables are the
  // process's and that test's children read them in parallel (and neither test removes them for the same reason).
  // SAFETY: both tests set the same values before spawning, and the children only read them.
  unsafe {
    std::env::set_var(CHILD_BEATS, BEATS.to_string());
    std::env::set_var(CHILD_EXIT, EXIT.to_string());
  }
  // The test's clock starts one window in, so no difference the supervisor takes reaches below its zero.
  let mut now = WINDOW_NS;
  supervisor.start(now).unwrap();
  let mut backoffs = Vec::new();
  let mut backed_off_in_crash_loop = false;
  let mut restarted_after_backoff = false;
  // A child that never exits ends the test: each child gets the wait a child gets to attach and beat, on the wall.
  let life = Duration::from_millis(CHILD_WAIT_MS);
  let mut child_began = Instant::now();
  while backoffs.len() < 2 {
    assert!(
      child_began.elapsed() < life,
      "a child neither exited nor was restarted within {life:?}"
    );
    match supervisor.step(now).unwrap() {
      Step::CrashLoop { retry_in_ns, .. } => backoffs.push(retry_in_ns),
      Step::BackingOff { retry_at_ns } => {
        backed_off_in_crash_loop |=
          supervisor.segment().supervision().unwrap().state() == State::CrashLoop;
        now = retry_at_ns;
      }
      Step::Restarted { .. } if !backoffs.is_empty() => {
        restarted_after_backoff |=
          supervisor.segment().supervision().unwrap().state() == State::Running;
        child_began = Instant::now();
      }
      Step::Restarted { .. } => child_began = Instant::now(),
      Step::Running => {}
      Step::Stopped => panic!("stopped without being asked"),
    }
    // The test harness paces the poll; shipped code parks on its driver (D-9).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_micros(POLL_US));
  }
  supervisor.stop().ok();
  assert!(
    backed_off_in_crash_loop,
    "the backoff was recorded as a crash loop"
  );
  assert!(
    restarted_after_backoff,
    "the daemon was started again after the backoff"
  );
  assert_eq!(
    backoffs,
    [WINDOW_NS, 2 * WINDOW_NS],
    "the first backoff is the window, the next twice it"
  );
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
  daemon.write(offset, written).unwrap();

  // The daemon crashes: its mapping is gone. The supervisor still holds the object alive.
  drop(daemon);

  // The restarted daemon re-opens the content object from the same handoff; the bytes survive.
  let restarted = AnchorSegment::open_content(&env).unwrap().unwrap();
  let mut survived = vec![0u8; written.len()];
  restarted.read(offset, &mut survived).unwrap();
  assert_eq!(
    &survived[..],
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

/// T-2.4 / §2.6: the re-invoked daemon uses its own HostClock to publish real heartbeats. The
/// parent kills and reaps it; this deadline also bounds it if the parent fails before cleanup.
#[test]
#[ignore = "heartbeat child; invoked by the supervision history"]
fn host_clock_heartbeat_child() {
  use slates_vfs::clock::{Clock, HostClock};
  let segment = AnchorSegment::attach_from_env(&identity()).unwrap();
  let mut clock = HostClock::new();
  let start = Instant::now();
  while start.elapsed() < Duration::from_millis(CHILD_WAIT_MS) {
    segment.supervision().unwrap().beat(clock.monotonic_ns());
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_micros(POLL_US));
  }
}

/// T-2.4 / §2.6 / §4.14: a long-lived anchor starts and restarts a child whose clock is newly
/// constructed. Both generations' first heartbeats must follow their start and be live in the
/// anchor's clock domain. Each sample is bounded by the parent's readings around the start/wait.
#[test]
fn a_delayed_child_and_its_restart_beat_in_the_anchors_clock_domain() {
  use slates_vfs::clock::{Clock, HostClock};
  /// Shape: the parent exists for this interval before either child starts; no product timeout changes.
  const PARENT_AGE: Duration = Duration::from_millis(100);
  let name = format!("slates-anchor-clock-{}", std::process::id());
  let segment = AnchorSegment::create(&name, &identity(), geometry()).unwrap();
  let args = vec![
    "--ignored".into(),
    "--exact".into(),
    "host_clock_heartbeat_child".into(),
  ];
  let budget_ns = CHILD_WAIT_MS * 1_000_000;
  let policy = RestartPolicy::derive(budget_ns, budget_ns).get();
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
  let mut clock = HostClock::new();
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(PARENT_AGE);
  let mut samples = Vec::new();
  for _ in 0..2 {
    let started = clock.monotonic_ns();
    supervisor.start(started).unwrap();
    let wait = Instant::now();
    while supervisor.segment().supervision().unwrap().heartbeat_ns() == 0
      && wait.elapsed() < Duration::from_millis(CHILD_WAIT_MS)
    {
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_micros(POLL_US));
    }
    let beat = supervisor.segment().supervision().unwrap().heartbeat_ns();
    let observed = clock.monotonic_ns();
    let alive = supervisor
      .segment()
      .supervision()
      .unwrap()
      .alive(observed, budget_ns);
    samples.push((started, beat, observed, alive));
    supervisor.stop().unwrap();
  }
  eprintln!("clock-domain observations (start, beat, observed, alive/age): {samples:?}");
  for (started, beat, observed, (alive, _)) in samples {
    assert!(
      beat >= started && beat <= observed,
      "heartbeat {beat} must be inside {started}..={observed}"
    );
    assert!(
      alive,
      "the anchor sees the child's current heartbeat as live"
    );
  }
}

/// T-2.4 / §2.6: a segment with process-relative timestamps cannot be attached by a daemon
/// using host-wide time. Reject the incompatible handoff before a heartbeat or lease is read.
#[test]
fn a_segment_with_process_relative_deadlines_is_refused_before_recovery() {
  use slates_anchor::layout::AT_VERSION;
  /// Format: layout 2 recorded timestamps relative to each constructing process/clock.
  const PROCESS_RELATIVE_LAYOUT: u32 = 2;
  let name = format!("slates-anchor-old-clock-{}", std::process::id());
  let segment = AnchorSegment::create(&name, &identity(), geometry()).unwrap();
  let (handoff, len) = handoff_of(&segment);
  // A second mapping of the header alone (its seqlock word declared), writing the old version by copy.
  let header = slates_mem::Words::new().with(slates_mem::WordRun::one(
    slates_anchor::layout::AT_GENERATION,
    slates_mem::Width::U64,
  ));
  let mut mapping = slates_mem::SparseObject::open(&handoff, len, header).unwrap();
  mapping
    .write(AT_VERSION, &PROCESS_RELATIVE_LAYOUT.to_le_bytes())
    .unwrap();
  let result = AnchorSegment::attach(&handoff, len, &identity());
  assert!(
    matches!(
      result,
      Err(AnchorError::Layout {
        reason: "wrong layout version"
      })
    ),
    "an old clock domain is incompatible: {result:?}"
  );
}

/// A-237 (§4.13): a segment written before principals carried their host names its owners, access lists and consumers
/// as bare accounts, which a daemon of today would take for its own host's. Do: write layout 4 into a segment's header
/// and attach it. Expect: refused typed at start, saying what to do (start afresh and re-enroll), never read. Before
/// A-237 layout 4 was the current one, and the attach succeeded.
#[test]
fn a_segment_from_before_host_scoped_principals_is_refused_at_start_saying_to_re_enroll() {
  use slates_anchor::layout::{AT_VERSION, LAYOUT_BEFORE_HOST_SCOPED_PRINCIPALS};
  let name = format!("slates-anchor-unscoped-{}", std::process::id());
  let segment = AnchorSegment::create(&name, &identity(), geometry()).unwrap();
  let (handoff, len) = handoff_of(&segment);
  let header = slates_mem::Words::new().with(slates_mem::WordRun::one(
    slates_anchor::layout::AT_GENERATION,
    slates_mem::Width::U64,
  ));
  let mut mapping = slates_mem::SparseObject::open(&handoff, len, header).unwrap();
  mapping
    .write(
      AT_VERSION,
      &LAYOUT_BEFORE_HOST_SCOPED_PRINCIPALS.to_le_bytes(),
    )
    .unwrap();
  let result = AnchorSegment::attach(&handoff, len, &identity());
  assert!(
    matches!(&result, Err(AnchorError::Layout { reason }) if reason.contains("re-enroll")),
    "a segment of unscoped principals is refused at start, saying to re-enroll: {result:?}"
  );
}

/// Shape: how long the graceful stopping child keeps beating after it acknowledges a stop before it exits —
/// its "drain", long enough that an anchor which did not wait for it would be seen not to.
const DRAIN_MS: u64 = 100;
/// Shape: the liveness budget the stop tests hold the child to — far above the child's beat interval
/// ([`POLL_US`]) and a loaded host's scheduling pauses, far below [`CHILD_WAIT_MS`].
const STOP_LIVENESS_MS: u64 = 300;
/// Shape: the deadline the wedged child declares — ten liveness budgets out, so a kill well before it proves
/// the lapsed heartbeat, not the deadline, ended the stop.
const WEDGED_DEADLINE_MS: u64 = 3_000;

/// How a stopping child answers the anchor's stop request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StopRole {
  /// Acknowledges with a deadline, keeps beating through a drain of [`DRAIN_MS`], and exits 0.
  Graceful,
  /// Keeps beating and never acknowledges (an older daemon, or a loop that stopped polling the request).
  Deaf,
  /// Acknowledges a distant deadline, then stops beating (a daemon wedged mid-drain).
  Wedged,
}

/// A stopping child: attaches the segment, beats in the host clock domain, and answers the stop request as
/// `role` says. It never outlives [`CHILD_WAIT_MS`]. Without the handoff in its environment (a plain
/// `--ignored` run in process) it returns at once.
fn stopping_child(role: StopRole) {
  use slates_vfs::clock::{Clock, HostClock};
  let Ok(segment) = AnchorSegment::attach_from_env(&identity()) else {
    return;
  };
  let mut clock = HostClock::new();
  let started = Instant::now();
  let mut acknowledged: Option<Instant> = None;
  while started.elapsed() < Duration::from_millis(CHILD_WAIT_MS) {
    let now = clock.monotonic_ns();
    let supervision = segment.supervision().unwrap();
    if !(role == StopRole::Wedged && acknowledged.is_some()) {
      supervision.beat(now);
    }
    if acknowledged.is_none() && supervision.stop_requested_at().is_some() {
      match role {
        StopRole::Graceful => {
          supervision.declare_stop_by(now + DRAIN_MS * 1_000_000);
          acknowledged = Some(Instant::now());
        }
        StopRole::Wedged => {
          supervision.declare_stop_by(now + WEDGED_DEADLINE_MS * 1_000_000);
          acknowledged = Some(Instant::now());
        }
        StopRole::Deaf => {}
      }
    }
    if role == StopRole::Graceful
      && acknowledged.is_some_and(|at| at.elapsed() >= Duration::from_millis(DRAIN_MS))
    {
      std::process::exit(0);
    }
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_micros(POLL_US));
  }
  std::process::exit(EXIT);
}

/// The graceful stopping child (run by its test with `--ignored --exact`).
#[test]
#[ignore = "a stopping child; run by a_graceful_stop_... with --ignored"]
fn graceful_stopping_child() {
  stopping_child(StopRole::Graceful);
}

/// The deaf stopping child (run by its test with `--ignored --exact`).
#[test]
#[ignore = "a stopping child; run by a_daemon_that_never_acknowledges_... with --ignored"]
fn deaf_stopping_child() {
  stopping_child(StopRole::Deaf);
}

/// The wedged stopping child (run by its test with `--ignored --exact`).
#[test]
#[ignore = "a stopping child; run by a_daemon_whose_heartbeat_lapses_... with --ignored"]
fn wedged_stopping_child() {
  stopping_child(StopRole::Wedged);
}

/// Starts `child` under a supervisor, waits for its first heartbeat, asks it to stop gracefully, and steps
/// the stop until it ends: what it ended as, how long after the request, and the supervision state after.
/// `tag` names the test's segment apart from the others running beside it in this process (short: a macOS
/// shared-memory name holds 31 bytes).
fn stop_through_the_protocol(child: &str, tag: &str) -> (Stopping, Duration, State) {
  use slates_vfs::clock::{Clock, HostClock};
  let segment = AnchorSegment::create(&unique_name(tag), &identity(), geometry()).unwrap();
  let args = vec![
    "--ignored".to_owned(),
    "--exact".to_owned(),
    child.to_owned(),
    "--nocapture".to_owned(),
  ];
  let budget_ns = CHILD_WAIT_MS * 1_000_000;
  let mut supervisor = Supervisor::new(
    segment,
    &args,
    RestartPolicy::derive(budget_ns, budget_ns).get(),
  )
  .unwrap();
  let mut clock = HostClock::new();
  supervisor.start(clock.monotonic_ns()).unwrap();
  let wait = Instant::now();
  while supervisor.segment().supervision().unwrap().heartbeat_ns() == 0
    && wait.elapsed() < Duration::from_millis(CHILD_WAIT_MS)
  {
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_micros(POLL_US));
  }
  let asked = Instant::now();
  supervisor.request_stop(clock.monotonic_ns()).unwrap();
  while asked.elapsed() < Duration::from_millis(CHILD_WAIT_MS) {
    match supervisor
      .stop_step(clock.monotonic_ns(), STOP_LIVENESS_MS * 1_000_000)
      .unwrap()
    {
      Stopping::Draining => {
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_micros(POLL_US));
      }
      exited => {
        let state = supervisor.segment().supervision().unwrap().state();
        return (exited, asked.elapsed(), state);
      }
    }
  }
  panic!("the stop of {child} never ended");
}

/// §2.5 / the consensus drain (`docs/wip/research/consensus-enhancements.md` §3.2): a graceful stop waits for
/// a daemon that acknowledges and drains — it exits on its own, with its own code, and is not killed — and
/// the segment records the stop.
#[test]
fn a_graceful_stop_waits_for_the_daemon_to_drain_and_exit_without_a_kill() {
  let (outcome, took, state) = stop_through_the_protocol("graceful_stopping_child", "sa-stop-g");
  assert_eq!(
    outcome,
    Stopping::Exited {
      exit_code: Some(0),
      killed: false
    }
  );
  assert!(
    took >= Duration::from_millis(DRAIN_MS),
    "the anchor waited out the drain ({took:?})"
  );
  assert_eq!(state, State::Stopped);
}

/// A daemon that never acknowledges the stop request (an older daemon, a loop no longer polling) is killed
/// once the liveness budget has passed since the request — never before it.
#[test]
fn a_daemon_that_never_acknowledges_a_stop_is_killed_after_the_liveness_budget() {
  let (outcome, took, state) = stop_through_the_protocol("deaf_stopping_child", "sa-stop-d");
  assert!(
    matches!(outcome, Stopping::Exited { killed: true, .. }),
    "{outcome:?}"
  );
  assert!(
    took >= Duration::from_millis(STOP_LIVENESS_MS),
    "not killed before the budget ({took:?})"
  );
  assert_eq!(state, State::Stopped);
}

/// A daemon whose heartbeat lapses mid-drain is killed at the lapse, long before the deadline it declared.
#[test]
fn a_daemon_whose_heartbeat_lapses_while_stopping_is_killed_before_its_declared_deadline() {
  let (outcome, took, state) = stop_through_the_protocol("wedged_stopping_child", "sa-stop-w");
  assert!(
    matches!(outcome, Stopping::Exited { killed: true, .. }),
    "{outcome:?}"
  );
  assert!(
    took < Duration::from_millis(WEDGED_DEADLINE_MS),
    "killed at the lapse, before the declared deadline ({took:?})"
  );
  assert_eq!(state, State::Stopped);
}

/// Format: the environment variable turning this binary into the device-handoff child.
#[cfg(target_os = "linux")]
const CHILD_DEVICES: &str = "SLATES_ANCHOR_TEST_DEVICES";
/// Format: the attachment the device-handoff child holds its device under.
#[cfg(target_os = "linux")]
const DEVICE_ATTACHMENT: u64 = 0x61;
/// Format: the bytes the first daemon leaves in its device for its successor to read.
#[cfg(target_os = "linux")]
const DEVICE_MARK: &[u8] = b"held across the restart";
/// Format: the session the first daemon sends.
#[cfg(target_os = "linux")]
const DEVICE_SESSION: [u8; 2] = [1, 3];
/// Format: the first daemon's exit code, distinct from the successor's verdicts.
#[cfg(target_os = "linux")]
const SENDER_EXIT: i32 = 7;

/// The device-handoff child: with no device handed over it is the first daemon — it opens a pipe, leaves
/// [`DEVICE_MARK`] in it, sends the anchor the read end as its device and the session, and exits
/// [`SENDER_EXIT`]; handed a device, it is the successor — it exits 0 iff the device is the first daemon's
/// attachment, with its session, and reads the mark. Run by the parent with `--ignored --exact`.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "the device-handoff child; run by the parent with --ignored"]
fn device_child() {
  use std::os::fd::{AsFd, FromRawFd, OwnedFd};
  if std::env::var(CHILD_DEVICES).is_err() {
    return;
  }
  let handed =
    std::env::var(slates_anchor::held::ENV_DEVICES).expect("the anchor handed the channel over");
  let inherited = slates_anchor::held::parse_env(&handed).unwrap();
  // SAFETY: the anchor made the channel's daemon end inheritable and named its number in the environment;
  // this process adopts it once, here, so the `OwnedFd` is its single owner.
  let channel = unsafe { OwnedFd::from_raw_fd(inherited.channel) };
  let Some(device) = inherited.devices.first() else {
    let (reader, writer) = rustix::pipe::pipe().unwrap();
    rustix::io::write(&writer, DEVICE_MARK).unwrap();
    slates_anchor::held::send(
      channel.as_fd(),
      &slates_anchor::held::Outgoing::Hold {
        attachment: DEVICE_ATTACHMENT,
        device: reader.as_fd(),
      },
    )
    .unwrap();
    slates_anchor::held::send(
      channel.as_fd(),
      &slates_anchor::held::Outgoing::Session {
        attachment: DEVICE_ATTACHMENT,
        session: &DEVICE_SESSION,
      },
    )
    .unwrap();
    std::process::exit(SENDER_EXIT);
  };
  // SAFETY: as the channel: inherited across the spawn at the number the environment names, adopted once.
  let held = unsafe { OwnedFd::from_raw_fd(device.fd) };
  let mut read = vec![0u8; DEVICE_MARK.len()];
  let got = rustix::io::read(&held, &mut read).unwrap_or(0);
  let right = device.attachment == DEVICE_ATTACHMENT
    && device.session.as_deref() == Some(&DEVICE_SESSION[..])
    && read.get(..got) == Some(DEVICE_MARK);
  std::process::exit(i32::from(!right));
}

/// A-61 (AC-3.4's mechanism). Do: hold devices in a real supervisor; its first daemon sends a device (a pipe
/// it left bytes in) and the session, then exits; the supervisor restarts it. Expect: the anchor held the
/// device past its sender's death, and the successor inherited it at the number the environment names, under
/// the same attachment and session, and read the first daemon's bytes from it — the same kernel object.
#[cfg(target_os = "linux")]
#[test]
fn a_device_a_daemon_sent_is_held_across_its_restart_and_handed_to_its_successor() {
  let segment = AnchorSegment::create(
    &unique_name("slates-anchor-test-devices"),
    &identity(),
    geometry(),
  )
  .unwrap();
  let args = vec![
    "--ignored".to_owned(),
    "--exact".to_owned(),
    "device_child".to_owned(),
    "--nocapture".to_owned(),
  ];
  // Shape: a window that holds this test's two starts and the restart after each.
  let policy = RestartPolicy {
    window_ns: 60_000_000_000,
    max_restarts: 3,
  };
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
  // Shape: room for this test's one device.
  supervisor.hold_descriptors(4, 4, 4).unwrap();
  // SAFETY: the test is single-threaded here; the role is chosen by the child's args, so a parallel test's child
  // ignores this variable.
  unsafe {
    std::env::set_var(CHILD_DEVICES, "1");
  }
  let clock = Instant::now();
  supervisor.start(now_ns(clock)).unwrap();
  let deadline = Duration::from_millis(CHILD_WAIT_MS);
  let mut exits = Vec::new();
  let mut held_after_first = None;
  while clock.elapsed() < deadline && exits.len() < 2 {
    match supervisor.step(now_ns(clock)).unwrap() {
      Step::Running => {
        // The test harness paces the poll; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_micros(POLL_US));
      }
      Step::Restarted { exit_code, .. } => {
        if exits.is_empty() {
          held_after_first = Some(supervisor.held_devices());
        }
        exits.push(exit_code);
      }
      Step::CrashLoop { exit_code, .. } => {
        exits.push(exit_code);
        break;
      }
      Step::BackingOff { .. } => break,
      Step::Stopped => break,
    }
  }
  supervisor.stop().ok();
  // SAFETY: single-threaded cleanup.
  unsafe {
    std::env::remove_var(CHILD_DEVICES);
  }
  assert_eq!(
    held_after_first,
    Some(1),
    "the anchor held the dead daemon's device"
  );
  assert_eq!(supervisor.device_refusals(), 0, "no message was refused");
  assert_eq!(
    exits,
    [Some(SENDER_EXIT), Some(0)],
    "the first daemon sent its device and left; its successor inherited it and read the first one's bytes"
  );
}

/// Shape: partitions of the pool tests' segment, so several claimants contend for several extents.
const POOL_PARTITIONS: u16 = 8;

/// A-98: do claim pool extents for partitions on a segment, release one by the wrong partition and by its own, and name
/// one past the pool; expect a claim to take a free extent only, the owner read back, a release by another partition
/// to change nothing, the extent free after its own release, the partition's held list, and the extent past the pool
/// refused typed.
#[test]
fn a_pool_extent_is_claimed_by_one_partition_and_released_only_by_it() {
  let id = identity();
  let segment =
    AnchorSegment::create(&unique_name("slates-anchor-test-pool"), &id, geometry()).unwrap();
  assert_eq!(segment.pool_extents(), 128, "64 extents per partition");
  assert_eq!(segment.pool_owner(0).unwrap(), None);
  assert!(segment.pool_claim(0, 1).unwrap(), "a free extent is taken");
  assert!(!segment.pool_claim(0, 0).unwrap(), "a held extent is not");
  assert_eq!(segment.pool_owner(0).unwrap(), Some(1));
  assert!(segment.pool_claim(1, 1).unwrap());
  assert_eq!(segment.pool_held_by(1).unwrap(), [0, 1]);
  releases_by_their_holder_only(&segment);
}

/// The release half of [`a_pool_extent_is_claimed_by_one_partition_and_released_only_by_it`]: partition 1 holds
/// extents 0 and 1.
fn releases_by_their_holder_only(segment: &AnchorSegment) {
  assert!(
    !segment.pool_release(0, 0).unwrap(),
    "another partition releases nothing"
  );
  assert_eq!(segment.pool_owner(0).unwrap(), Some(1));
  assert!(segment.pool_release(0, 1).unwrap());
  assert_eq!(segment.pool_owner(0).unwrap(), None);
  assert_eq!(segment.pool_held_by(1).unwrap(), [1]);
  assert!(segment.pool_claim(128, 0).is_err(), "past the pool");
}

/// A-98: do have one claimant per partition, each through its own mapping of the segment (as shards of racing daemons
/// would), claim every extent at once; expect each extent taken by exactly one partition, the owner words agreeing
/// with the winners, and the claims still there through a fresh attachment (a daemon restart keeps them).
#[test]
fn racing_claimants_take_each_pool_extent_exactly_once_and_the_claims_outlive_the_mapping() {
  let id = identity();
  let geometry = Geometry {
    partitions: POOL_PARTITIONS,
    ..geometry()
  };
  let segment =
    AnchorSegment::create(&unique_name("slates-anchor-test-pool-race"), &id, geometry).unwrap();
  let (handoff, len) = handoff_of(&segment);
  let wins: Vec<Vec<usize>> = std::thread::scope(|scope| {
    let claimants: Vec<_> = (0..POOL_PARTITIONS)
      .map(|partition| {
        let (handoff, id) = (handoff.clone(), id.clone());
        scope.spawn(move || {
          let mapped = AnchorSegment::attach(&handoff, len, &id).unwrap();
          (0..mapped.pool_extents())
            .filter(|extent| mapped.pool_claim(*extent, partition).unwrap())
            .collect::<Vec<usize>>()
        })
      })
      .collect();
    claimants
      .into_iter()
      .map(|claimant| claimant.join().unwrap())
      .collect()
  });
  let mut taken: Vec<usize> = wins.iter().flatten().copied().collect();
  taken.sort_unstable();
  assert_eq!(
    taken,
    (0..segment.pool_extents()).collect::<Vec<_>>(),
    "each extent exactly once: {wins:?}"
  );
  let later = AnchorSegment::attach(&handoff, len, &id).unwrap();
  for (partition, won) in wins.iter().enumerate() {
    assert_eq!(
      &later
        .pool_held_by(u16::try_from(partition).unwrap())
        .unwrap(),
      won
    );
  }
}

/// Format: the environment variable naming the role the re-invoked binary plays in the device-refusal fixture: the
/// anchor ([`DEVICE_REFUSALS_ANCHOR`]) or the daemon it supervises ([`DEVICE_GARBLER`]).
#[cfg(unix)]
const DEVICE_REFUSAL_ROLE: &str = "SLATES_ANCHOR_TEST_DEVICE_REFUSAL_ROLE";
/// Format: the anchor role's test name, which is also its role value.
#[cfg(unix)]
const DEVICE_REFUSALS_ANCHOR: &str = "device_refusals_anchor";
/// Format: the garbling daemon's test name, which is also its role value.
#[cfg(unix)]
const DEVICE_GARBLER: &str = "device_garbler";
/// Shape: the bad device messages the garbling daemon sends, half of each kind.
#[cfg(unix)]
const BAD_DEVICE_MESSAGES: usize = 20;
/// Shape: the distinct refusals those messages make (a message of the wrong length, and one of an unknown kind).
#[cfg(unix)]
const BAD_DEVICE_KINDS: usize = 2;
/// Format: the kind byte no device message has.
#[cfg(unix)]
const UNKNOWN_KIND: u8 = 0xff;
/// Format: the tag before the count the anchor role prints on stdout, found anywhere in a line (libtest's banner may
/// come glued to it).
#[cfg(unix)]
const DEVICE_REFUSALS_TAG: &str = "ANCHOR_DEVICE_REFUSALS=";
/// Format: the words of the anchor's log line for a refused hold message.
#[cfg(unix)]
const DEVICE_REFUSAL_LINE: &str = "a hold message was refused";
/// Format: the garbling daemon's exit code, distinct from libtest's, so a run that selected no test cannot pass.
#[cfg(unix)]
const GARBLER_EXIT: i32 = 9;
/// Format: the anchor role's exit code, for the same reason.
#[cfg(unix)]
const DEVICE_ANCHOR_EXIT: i32 = 11;

/// The garbling daemon: adopts the hold channel it inherited and sends [`BAD_DEVICE_MESSAGES`] bad messages, a
/// message of half a message's length and a whole one of an unknown kind in turn, then exits [`GARBLER_EXIT`]. Run by
/// [`device_refusals_anchor`] with `--ignored --exact`.
#[cfg(unix)]
#[test]
#[ignore = "the garbling daemon of the device-refusal fixture; run by the anchor role with --ignored"]
fn device_garbler() {
  use std::os::fd::{AsFd, FromRawFd, OwnedFd};
  if std::env::var(DEVICE_REFUSAL_ROLE).as_deref() != Ok(DEVICE_GARBLER) {
    return;
  }
  let handed =
    std::env::var(slates_anchor::held::ENV_DEVICES).expect("the anchor handed the channel over");
  let inherited = slates_anchor::held::parse_env(&handed).unwrap();
  // SAFETY: the anchor made the channel's daemon end inheritable and named its number in the environment; this
  // process adopts it once, here, so the `OwnedFd` is its single owner.
  let channel = unsafe { OwnedFd::from_raw_fd(inherited.channel) };
  let short = vec![0u8; slates_anchor::held::MESSAGE_BYTES / 2];
  let mut unknown = vec![0u8; slates_anchor::held::MESSAGE_BYTES];
  unknown[0] = UNKNOWN_KIND;
  for message in 0..BAD_DEVICE_MESSAGES {
    let bytes = if message % 2 == 0 { &short } else { &unknown };
    let sent = rustix::net::send(channel.as_fd(), bytes, rustix::net::SendFlags::empty()).unwrap();
    assert_eq!(sent, bytes.len(), "a datagram is sent whole");
  }
  std::process::exit(GARBLER_EXIT);
}

/// The anchor role: a real supervisor holding descriptors over [`device_garbler`], stepped until that daemon's exit
/// is seen and its messages drained; prints the refusals counted, tagged, and exits [`DEVICE_ANCHOR_EXIT`]. Its log is
/// this process's stderr, which the parent reads.
#[cfg(unix)]
#[test]
#[ignore = "the anchor of the device-refusal fixture; run by the parent with --ignored"]
fn device_refusals_anchor() {
  if std::env::var(DEVICE_REFUSAL_ROLE).as_deref() != Ok(DEVICE_REFUSALS_ANCHOR) {
    return;
  }
  let segment = AnchorSegment::create(
    &unique_name("slates-anchor-test-device-refusals"),
    &identity(),
    geometry(),
  )
  .unwrap();
  let args = ["--ignored", "--exact", DEVICE_GARBLER, "--nocapture"].map(str::to_owned);
  // Shape: no restart in the window, so the garbler's one exit ends the fixture's observation (a crash loop backs off
  // rather than starting a second garbler).
  let policy = RestartPolicy {
    window_ns: 60_000_000_000,
    max_restarts: 0,
  };
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
  // Shape: a drain of 28 messages a step (4 devices × 3 + 4 connections × 2 + 4 copies × 2), above the garbler's 20.
  supervisor.hold_descriptors(4, 4, 4).unwrap();
  supervisor.hold_environment(DEVICE_REFUSAL_ROLE, DEVICE_GARBLER.to_owned());
  let clock = Instant::now();
  supervisor.start(now_ns(clock)).unwrap();
  let deadline = Duration::from_millis(CHILD_WAIT_MS);
  while clock.elapsed() < deadline {
    match supervisor.step(now_ns(clock)).unwrap() {
      Step::Running => {
        // The test harness paces the poll; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_micros(POLL_US));
      }
      _ => break,
    }
  }
  // The step that saw the exit drained first; one more drain takes anything sent after it.
  supervisor.drain_devices();
  println!("{DEVICE_REFUSALS_TAG}{};", supervisor.device_refusals());
  supervisor.stop().ok();
  std::process::exit(DEVICE_ANCHOR_EXIT);
}

/// A-213, the rule against unbounded growth (the sibling of A-198's Windows fix). Do: run a real anchor as a process
/// of its own over a daemon sending 20 bad device messages, two kinds in turn. Expect: all 20 counted, and the anchor's
/// log says each kind once, so 2 lines, not one a message.
#[cfg(unix)]
#[test]
fn every_bad_device_message_is_counted_and_the_log_says_each_kind_once() {
  let output = std::process::Command::new(std::env::current_exe().unwrap())
    .args([
      "--ignored",
      "--exact",
      DEVICE_REFUSALS_ANCHOR,
      "--nocapture",
    ])
    .env(DEVICE_REFUSAL_ROLE, DEVICE_REFUSALS_ANCHOR)
    .output()
    .unwrap();
  let stdout = String::from_utf8_lossy(&output.stdout);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(
    output.status.code(),
    Some(DEVICE_ANCHOR_EXIT),
    "the anchor role ran: {stdout}\n{stderr}"
  );
  let counted = stdout
    .split(DEVICE_REFUSALS_TAG)
    .nth(1)
    .and_then(|rest| rest.split(';').next())
    .and_then(|count| count.parse::<usize>().ok());
  assert_eq!(counted, Some(BAD_DEVICE_MESSAGES), "{stdout}\n{stderr}");
  assert_eq!(
    stderr.matches(DEVICE_REFUSAL_LINE).count(),
    BAD_DEVICE_KINDS,
    "one line per kind: {stderr}"
  );
}

/// Format: the role of [`tearing_child`]: set, the child is it.
const CHILD_TEAR: &str = "SLATES_ANCHOR_TEST_TEAR";
/// Format: the exit of a [`tearing_child`] that left the profile's publication unfinished, as a daemon the anchor
/// killed inside its `publish` leaves it.
const TORE: i32 = 21;
/// Format: the exit of one that read the anchor's profile whole.
const READ_WHOLE: i32 = 0;
/// Format: the exit of one refused the profile, the daemon's own exit for a refused start.
const REFUSED: i32 = 4;
/// Format: the profile the test's anchor publishes.
const PROFILE: &[u8] = b"{\"the anchor's profile\":1}";

/// The child [`a_daemon_killed_mid_publication_leaves_its_successor_the_anchors_profile`] supervises. Its first start
/// does what a daemon's `publish` does first, making the profile's generation odd, and exits there, as a daemon the
/// anchor kills inside it does; a later start reads the profile as a daemon does at its start.
#[test]
#[ignore = "the tearing child; run by a_daemon_killed_mid_publication_leaves_its_successor_the_anchors_profile"]
fn tearing_child() {
  if std::env::var_os(CHILD_TEAR).is_none() {
    return;
  }
  let segment = AnchorSegment::attach_from_env(&identity()).unwrap();
  if segment.supervision().unwrap().restarts() == 0 {
    segment
      .payload_generation(RegionKind::Profile)
      .unwrap()
      .fetch_or(1, Ordering::AcqRel);
    std::process::exit(TORE);
  }
  match segment.read_published(RegionKind::Profile) {
    Ok(Some(profile)) if profile == PROFILE => std::process::exit(READ_WHOLE),
    _ => std::process::exit(REFUSED),
  }
}

/// §4.14, A-119. Do: publish a profile as `slates anchor` does, and supervise a child whose first start leaves the
/// profile's publication unfinished and exits, as a daemon killed inside its own republication of the profile does.
/// Expect its successor to read the anchor's profile whole. Before, the successor was refused it
/// (`PublicationInProgress`, "a payload publication is unfinished") and exited 4, and so did every daemon after it:
/// only the anchor's first publication ever wrote the region again. Under two spinners a core the anchor killed two
/// starting daemons in a row at its recovery budget, and the third exited 4 into a crash loop that failed
/// `slates_run_spawns_the_command_as_an_ephemeral_consumer` (1 of 10 runs, an 18-core Mac, 2026-11-07).
#[test]
fn a_daemon_killed_mid_publication_leaves_its_successor_the_anchors_profile() {
  let mut segment = AnchorSegment::create(
    &unique_name("slates-anchor-test-tear"),
    &identity(),
    geometry(),
  )
  .unwrap();
  segment.publish(RegionKind::Profile, PROFILE).unwrap();
  let args = ["--ignored", "--exact", "tearing_child", "--nocapture"].map(str::to_owned);
  // Shape: a window long enough to hold every restart of this test.
  let policy = RestartPolicy {
    window_ns: 60_000_000_000,
    max_restarts: 3,
  };
  let mut supervisor = Supervisor::new(segment, &args, policy).unwrap();
  supervisor.hold_environment(CHILD_TEAR, "1".to_owned());
  let clock = Instant::now();
  supervisor.start(now_ns(clock)).unwrap();
  let mut exits = Vec::new();
  while exits.len() < 2 && clock.elapsed() < Duration::from_millis(CHILD_WAIT_MS) {
    match supervisor.step(now_ns(clock)).unwrap() {
      Step::Restarted { exit_code, .. } => exits.push(exit_code),
      Step::Running => {
        // The test harness paces the poll; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_micros(POLL_US));
      }
      other => panic!("the supervisor stepped to {other:?}"),
    }
  }
  assert_eq!(
    exits,
    vec![Some(TORE), Some(READ_WHOLE)],
    "the child after the one that left the publication unfinished read the anchor's profile whole (exit {READ_WHOLE}, \
     not {REFUSED})"
  );
}
