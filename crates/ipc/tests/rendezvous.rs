//! The rendezvous across real processes (Phase 2 task 3; §4.7 "Rendezvous", §4.13): the test
//! binary re-invoked as the client connects to the daemon end running in the test process,
//! receives its region through the platform's handoff (an abstract socket with `SCM_RIGHTS`
//! on Linux; the bootstrap object with claim slots on macOS), sends a request, and gets the
//! reply; a client with no daemon is refused `DaemonUnavailable` and creates nothing.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::time::{Duration, Instant};

use slates_ipc::region::{ClientRegion, RegionGeometry};
use slates_ipc::slot::Slot;
use slates_ipc::{ClientEnd, DaemonEnd, IpcError, Listener, Prepared, connect};

/// Format: the environment variable that turns this binary into the client.
const CHILD_ROLE: &str = "SLATES_IPC_TEST_CLIENT";
/// Shape: how long the daemon side serves before giving up (milliseconds).
const SERVE_MS: u64 = 5_000;
/// Shape: slots per ring here.
const SLOTS: u32 = 16;

fn geometry() -> RegionGeometry {
  RegionGeometry {
    slots: SLOTS,
    spin_ns: 100_000,
    bulk_bytes: 4096,
    page: 4096,
  }
}

/// The client: connects to the instance in the environment, sends one request, expects the
/// reply to be the payload reversed, exits 0. Passes trivially when not the child.
#[test]
fn rendezvous_client() {
  let Ok(instance) = std::env::var(CHILD_ROLE) else {
    return;
  };
  let connected = connect(&instance).unwrap();
  let mut client = ClientEnd::with_doorbell(connected.region, connected.doorbell);
  client
    .send(&Slot::inline(0x0007_0000_0000_0001, b"hello").unwrap())
    .unwrap();
  let reply = client.wait(Some(2_000_000_000)).unwrap();
  assert_eq!(reply.payload, b"olleh");
  assert_eq!(reply.request, 0x0007_0000_0000_0001);
  std::process::exit(0);
}

/// The daemon end accepts the child through the real rendezvous, hands it a region, serves
/// its request, and the child exits 0; no cross-uid refusals were counted.
#[test]
fn a_client_process_connects_and_completes_a_round_trip() {
  let instance = format!("test-{}", std::process::id());
  let mut listener = Listener::open(&instance).unwrap();
  let exe = std::env::current_exe().unwrap();
  let mut child = Command::new(exe)
    .args(["--exact", "rendezvous_client", "--nocapture"])
    .env(CHILD_ROLE, &instance)
    .spawn()
    .unwrap();
  let started = Instant::now();
  let mut ends: Vec<DaemonEnd> = Vec::new();
  let mut served = false;
  let pid = std::process::id();
  while started.elapsed() < Duration::from_millis(SERVE_MS) && !served {
    let accepted = listener
      .accept_pending(&mut |client_id| {
        Ok(Prepared {
          region: ClientRegion::create(
            &format!("slates-cr-{pid}-{client_id}"),
            client_id,
            0,
            geometry(),
          )?,
          kick_fd: None,
        })
      })
      .unwrap();
    for a in accepted {
      assert_eq!(a.client_id, 1);
      ends.push(DaemonEnd::new(a.region));
    }
    for end in &mut ends {
      if let Some(req) = end.try_take().unwrap() {
        let mut reply = req.payload.clone();
        reply.reverse();
        end
          .reply(&Slot::inline(req.request, &reply).unwrap())
          .unwrap();
        served = true;
      }
    }
    std::hint::spin_loop();
  }
  assert!(served, "the client's request arrived");
  let status = child.wait().unwrap();
  assert!(status.success(), "the client saw its reply: {status:?}");
  assert_eq!(listener.refused(), 0);
}

/// No daemon: the connect is refused with the endpoint named, quickly, and nothing is left
/// behind.
#[test]
fn no_daemon_means_daemon_unavailable() {
  let started = Instant::now();
  let refused = connect("slates-test-no-such-daemon").err().unwrap();
  assert!(
    matches!(refused, IpcError::DaemonUnavailable { .. }),
    "{refused:?}"
  );
  assert!(started.elapsed() < Duration::from_secs(2));
}
