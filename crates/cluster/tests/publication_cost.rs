//! What a consensus retention publication costs as a group's log grows (AUD-29-30, §4.8): a publication
//! clones the node's retained Raft state (`RaftNode::saved`), encodes it (`SavedRaft::to_bytes`) and hashes the
//! bytes, on the acknowledgement path of every vote, append and configuration change. The three steps are timed
//! separately, so a decision about incremental publication rests on which of them grows and how fast.
//! Measurement only (R5's tests drive the protocol elsewhere); run by hand in release.
// Test harness code: an unwrap here is a failed measurement, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_cluster::raft::RaftNode;
use slates_db::register::HostId;
use slates_wire::Wire;

/// Format: the environment variable naming the log sizes to measure, comma separated.
const SIZES_VARIABLE: &str = "SLATES_PUBLICATION_ENTRIES";
/// Shape: the command bytes of each measured entry — a record's encoded configuration command is tens of
/// bytes, so the log's size in bytes is roughly this times its length.
const COMMAND_BYTES: usize = 64;
/// Shape: the publications timed at each size; the best of them is reported, all of them printed.
const RUNS: usize = 5;

/// A measurement tool: for each size in `SLATES_PUBLICATION_ENTRIES`, a lone leader appends that many
/// commands, then [`RUNS`] publications are timed step by step. `SLATES_PUBLICATION_ENTRIES=1000,10000,50000
/// cargo test -p slates-cluster --release --test publication_cost -- --ignored --nocapture`. Skips, saying
/// so, without the variable.
#[test]
#[ignore = "a measurement tool, run by hand with its environment set"]
fn a_publication_costs_the_clone_the_encoding_and_the_hash_of_the_whole_state() {
  let Ok(sizes) = std::env::var(SIZES_VARIABLE) else {
    eprintln!("skipping: set {SIZES_VARIABLE} to measure");
    return;
  };
  for size in sizes
    .split(',')
    .filter_map(|size| size.trim().parse::<u64>().ok())
  {
    let mut leader = RaftNode::new(HostId(1), vec![HostId(1)]);
    let _ = leader.start_election().unwrap();
    for appended in 0..size {
      let mut command = vec![0u8; COMMAND_BYTES];
      command[..8].copy_from_slice(&appended.to_le_bytes());
      assert!(leader.append_command(command));
    }
    let mut rows: Vec<(Duration, Duration, Duration, usize)> = Vec::new();
    for _ in 0..RUNS {
      let started = Instant::now();
      let saved = leader.saved();
      let cloned = started.elapsed();
      let started = Instant::now();
      let bytes = saved.to_bytes();
      let encoded = started.elapsed();
      let started = Instant::now();
      let hash = blake3::hash(&bytes);
      let hashed = started.elapsed();
      assert_ne!(hash.as_bytes(), &[0u8; 32]);
      rows.push((cloned, encoded, hashed, bytes.len()));
    }
    let best = rows
      .iter()
      .min_by_key(|(cloned, encoded, hashed, _)| *cloned + *encoded + *hashed)
      .unwrap();
    eprintln!(
      "{size} entries ({} bytes): best clone {:?}, encode {:?}, hash {:?}; all runs {:?}",
      best.3,
      best.0,
      best.1,
      best.2,
      rows
        .iter()
        .map(|(cloned, encoded, hashed, _)| *cloned + *encoded + *hashed)
        .collect::<Vec<_>>()
    );
  }
}
