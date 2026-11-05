//! An increment's identity names its work and its base (§4.16 data model: `id: blake3(work_volume,
//! base, post_state, ops_doc, filter)`), so the engine's retry cache answers only a true retry. Two
//! works that make the same edit are two increments: each is judged against what the green holds when
//! it arrives, never answered with the other's verdict. Driven over the client's rings against an
//! in-process daemon, as an agent submits.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn
)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, ReadAt, Submitted};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the test's machine profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards for the test daemon: two, so a client's shard and the control shard differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long the client retries the rendezvous while the daemon starts.
const START_WAIT: Duration = Duration::from_secs(10);

fn start(tag: &str) -> (Daemon, Client) {
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("mi-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-mi-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get();
  let started = Instant::now();
  let client = loop {
    match Client::connect(&instance, deadlines) {
      Ok(client) => break client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  };
  (daemon, client)
}

/// T-6.x (§4.16 "Commit": "an increment id already in `seen` returns its original result"; the id
/// names the work and its base): do two works based on version 1 write the same bytes over one range
/// of `f`, with a third work's differing write of that range merged between them; expect the second
/// writer's submit to be a conflict on `f` and the head to keep the third work's bytes. Then do a work
/// based on the new head write what the head already holds; expect an identical accept at a version
/// of its own, not the first writer's.
#[test]
fn the_same_edit_from_two_works_is_two_increments() {
  let (daemon, mut client) = start("same-edit");
  let green = client.create_green("g", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  client.edit(seed, "f", 0, 0, b"aaaaaaaa").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let (first, base) = client.create_work(green, "first").unwrap();
  assert_eq!(base, 1);
  let (late, base) = client.create_work(green, "late").unwrap();
  assert_eq!(base, 1);
  client.edit(first, "f", 0, 4, b"XXXX").unwrap();
  assert_eq!(client.submit(first).unwrap(), Submitted::Accepted(2));
  let (third, base) = client.create_work(green, "third").unwrap();
  assert_eq!(base, 2);
  client.edit(third, "f", 0, 4, b"ZZZZ").unwrap();
  assert_eq!(client.submit(third).unwrap(), Submitted::Accepted(3));
  // `late` writes exactly what `first` wrote, from the same base: the same ops document and post-state.
  client.edit(late, "f", 0, 4, b"XXXX").unwrap();
  match client.submit(late).unwrap() {
    Submitted::Conflict(windows) => assert!(
      windows.iter().any(|w| w.path == "f" && w.at < 4),
      "the conflict names the edited range: {windows:?}"
    ),
    other => panic!("a write over bytes the green changed since the base answered {other:?}"),
  }
  assert_eq!(
    client.read(green, "f", ReadAt::Head).unwrap(),
    b"ZZZZaaaa".to_vec()
  );
  assert_eq!(client.versions(green).unwrap(), 3);
  // Identical: `again` is based on 3 and writes the head's own bytes there; and a work based on 1
  // writing `first`'s bytes over a range nobody else touched since is judged too, never answered as
  // `first`'s retry.
  let (again, base) = client.create_work(green, "again").unwrap();
  assert_eq!(base, 3);
  client.edit(again, "f", 0, 4, b"ZZZZ").unwrap();
  assert_eq!(client.submit(again).unwrap(), Submitted::Accepted(4));
  assert_eq!(
    client.read(green, "f", ReadAt::Head).unwrap(),
    b"ZZZZaaaa".to_vec()
  );
  daemon.stop();
}
