//! A work's post-state after a length change (§4.16 "Submission", "Rebase"): a run the work wrote after an earlier
//! edit changed the file's length in the same work is the work's bytes, at the green's head after a submit, and in the
//! work after a rebase. The seal once copied each run from its base-coordinate offset (`op.at`) rather than from the
//! work's final file, so such a run merged zeros (loadtest-scale's `green_post_state.rs`, head `cd\0\0` for `cdXY`;
//! assembled so since 34548a59). This is the rebase sibling's sweep: a rebase restates the work from the same
//! post-state. Driven over the client's rings against an in-process daemon, as an agent edits.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn
)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, ReadAt, Rebased, Submitted, VolumeId};
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
  let instance = format!("mps-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-mps-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let deadlines =
    Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get();
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

/// A green whose version 1 holds `f` as `abcd` and `g` as `1234`, and a work based on it whose `f` reads `cdXY`: two
/// bytes removed at the front, then two inserted after what is left.
fn a_work_that_shortened_then_extended_f(client: &mut Client) -> (VolumeId, VolumeId) {
  let green = client.create_green("g", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  client.edit(seed, "f", 0, 0, b"abcd").unwrap();
  client.edit(seed, "g", 0, 0, b"1234").unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let (work, base) = client.create_work(green, "work").unwrap();
  assert_eq!(base, 1);
  client.edit(work, "f", 0, 2, b"").unwrap();
  client.edit(work, "f", 2, 0, b"XY").unwrap();
  assert_eq!(
    client.read(work, "f", ReadAt::Head).unwrap(),
    b"cdXY".to_vec()
  );
  (green, work)
}

/// §4.16 "Submission": do shorten then extend `f` in one work and submit it; expect the head's `f` to be the work's
/// `cdXY`, not a run of zeros where the inserted bytes were.
#[test]
fn a_run_after_a_length_change_reaches_the_head_as_written() {
  let (daemon, mut client) = start("post-submit");
  let (green, work) = a_work_that_shortened_then_extended_f(&mut client);
  assert_eq!(client.submit(work).unwrap(), Submitted::Accepted(2));
  assert_eq!(
    client.read(green, "f", ReadAt::Head).unwrap(),
    b"cdXY".to_vec()
  );
  daemon.stop();
}

/// §4.16 "Rebase": do shorten then extend `f` in one work, let another work change `g` meanwhile, and rebase the first
/// onto that head; expect the first work's `f` still `cdXY` after the rebase, and the head's `f` `cdXY` after its submit.
#[test]
fn a_rebase_keeps_a_run_written_after_a_length_change() {
  let (daemon, mut client) = start("post-rebase");
  let (green, work) = a_work_that_shortened_then_extended_f(&mut client);
  let (other, _) = client.create_work(green, "other").unwrap();
  client.edit(other, "g", 0, 4, b"5678").unwrap();
  assert_eq!(client.submit(other).unwrap(), Submitted::Accepted(2));
  assert_eq!(client.rebase(work).unwrap(), Rebased::Rebased(2));
  assert_eq!(
    client.read(work, "f", ReadAt::Head).unwrap(),
    b"cdXY".to_vec(),
    "the rebased work's own bytes"
  );
  assert_eq!(client.submit(work).unwrap(), Submitted::Accepted(3));
  assert_eq!(
    client.read(green, "f", ReadAt::Head).unwrap(),
    b"cdXY".to_vec()
  );
  assert_eq!(
    client.read(green, "g", ReadAt::Head).unwrap(),
    b"5678".to_vec()
  );
  daemon.stop();
}
