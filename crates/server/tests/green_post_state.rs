//! An increment's post-state holds the bytes its ops add, read from where the work's file holds them (§4.16 "Submission":
//! "the sealed post-state"; W2): the deriver names each added run by its offset in the work's final file
//! (`slates_merge::derive`, `run_src`), while the seal (`verbs::assemble_post_state`) read the bytes at the op's offset
//! in base coordinates, so a run after an earlier length change in the same file was read from the wrong place.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use slates_client::Submitted;
use slates_ipc::protocol::ReadAt;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: one shard, so the green and its works are on one partition.
const TEST_SHARDS: u16 = 1;

/// §4.16. Do: on a one-shard daemon, make a green whose one file reads `abcdef`; from a new work, delete its first two
/// bytes (the work now reads `cdef`), then overwrite the work's last two bytes with `XY` (`cdXY`), and submit; read the
/// green's head. Expect: the head reads `cdXY`, the work's file. The overwrite's run sits at offset 2 of the work's
/// file and at offset 4 of the base: read at the base offset, past the work file's end, the run was zeros.
#[test]
fn a_run_after_a_length_change_in_its_file_merges_its_own_bytes() {
  let profile = common::machine_profile();
  let instance = format!("srv-post-state-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-post-state-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("post-state-green", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  client.edit(seed, "f", 0, 0, b"abcdef").unwrap();
  let seeded = client.submit(seed);
  let (work, _) = client.create_work(green, "shifted").unwrap();
  client.edit(work, "f", 0, 2, b"").unwrap();
  client.edit(work, "f", 2, 2, b"XY").unwrap();
  let in_work = client.read(work, "f", ReadAt::Head);
  let submitted = client.submit(work);
  let head = client.read(green, "f", ReadAt::Head);
  drop(client);
  daemon.stop();
  assert!(
    matches!(seeded, Ok(Submitted::Accepted(1))),
    "seeded: {seeded:?}"
  );
  assert_eq!(in_work.unwrap(), b"cdXY", "the work reads its edits");
  assert!(
    matches!(submitted, Ok(Submitted::Accepted(2))),
    "submitted: {submitted:?}"
  );
  assert_eq!(
    head.unwrap(),
    b"cdXY",
    "the green's head is the work's file"
  );
}
