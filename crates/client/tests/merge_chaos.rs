//! Concurrent conflicting edits against one green (§4.16 "The verdict", T-6.7's live form): many clients, each its own
//! work, race overwrites of the same blocks of the same files and submit as fast as the daemon answers. Every verdict
//! is then judged against the design's rule, from what the clients were told alone:
//!
//! - the accepted versions are exactly 2..=head, each told to one submit;
//! - an accepted edit overlaps no edit accepted after its base and before its version (an overlapping differing edit
//!   is a conflict, never a silent overwrite);
//! - a conflict overlaps some edit accepted after its base (no false conflict), and its window names the edited range;
//! - every version, read back pinned, equals the seed with the accepted edits up to it applied in version order.
//!
//! Every edit writes bytes no other edit writes, so no accept is identical and the rule above is the whole verdict.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn,
  clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, Deadlines, ReadAt, Submitted, VolumeId};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the test's machine profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards for the test daemon: two, so clients land on both and their submits cross to the green's shard.
const TEST_SHARDS: u16 = 2;
/// Shape: how long a client retries the rendezvous while the daemon starts.
const START_WAIT: Duration = Duration::from_secs(10);
/// Shape: the racing clients.
const CLIENTS: usize = 8;
/// Shape: submits per client.
const ROUNDS: usize = 60;
/// Shape: the shared files.
const FILES: usize = 3;
/// Shape: blocks per file; few, so edits collide often.
const BLOCKS: usize = 4;
/// Shape: one block's bytes.
const BLOCK: usize = 16;

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(
        slates_ipc::IpcError::DaemonUnavailable { .. } | slates_ipc::IpcError::RingFull,
      )) if started.elapsed() < START_WAIT => std::thread::yield_now(),
      Err(e) => panic!("{e}"),
    }
  }
}

fn path_of(file: usize) -> String {
  format!("shared/f{file}")
}

/// One submit as its client saw it.
#[derive(Clone, Debug)]
struct Told {
  base: u64,
  file: usize,
  block: usize,
  bytes: Vec<u8>,
  /// `Some(version)` accepted, `None` a conflict.
  version: Option<u64>,
}

/// One client's race: edit a pseudo-random block of a pseudo-random file with bytes only it writes, submit, and
/// after a conflict replace its work (its journal holds the refused edit).
fn race(instance: &str, green: VolumeId, client_index: usize) -> Vec<Told> {
  let mut client = connect(instance);
  let mut word = u64::try_from(client_index)
    .unwrap()
    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
    | 1;
  let mut next = || {
    word ^= word << 13;
    word ^= word >> 7;
    word ^= word << 17;
    word
  };
  let mut made = 0usize;
  let mut work = client
    .create_work(green, &format!("c{client_index}-w{made}"))
    .unwrap();
  let mut told = Vec::with_capacity(ROUNDS);
  for round in 0..ROUNDS {
    let file = usize::try_from(next() % FILES as u64).unwrap();
    let block = usize::try_from(next() % BLOCKS as u64).unwrap();
    // Unique bytes: the client and round in every byte pair, so no two edits write the same block bytes.
    let tag = u16::try_from(client_index * ROUNDS + round + 1)
      .unwrap()
      .to_le_bytes();
    let bytes: Vec<u8> = tag.iter().copied().cycle().take(BLOCK).collect();
    let at = u64::try_from(block * BLOCK).unwrap();
    client
      .edit(work.0, &path_of(file), at, BLOCK as u64, &bytes)
      .unwrap();
    match client.submit(work.0).unwrap() {
      Submitted::Accepted(version) => {
        told.push(Told {
          base: work.1,
          file,
          block,
          bytes,
          version: Some(version),
        });
        work.1 = version;
      }
      Submitted::Conflict(windows) => {
        assert!(
          windows
            .iter()
            .any(|w| w.path == path_of(file) && w.at < at + BLOCK as u64 && w.at + w.len > at),
          "a conflict names the edited range: {windows:?}"
        );
        told.push(Told {
          base: work.1,
          file,
          block,
          bytes,
          version: None,
        });
        client.destroy(work.0).unwrap();
        made += 1;
        work = client
          .create_work(green, &format!("c{client_index}-w{made}"))
          .unwrap();
      }
    }
  }
  client.destroy(work.0).unwrap();
  told
}

/// The daemon the race runs against: in-process, its shards derived for the machine with [`TEST_SHARDS`].
fn start() -> (Daemon, String) {
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("mc-race-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-mc-race-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  (daemon, instance)
}

/// A green whose version 1 holds every shared file as `seed` bytes.
fn seeded_green(client: &mut Client, seed: &[u8]) -> VolumeId {
  let green = client.create_green("race", false).unwrap();
  let (work, _) = client.create_work(green, "seed").unwrap();
  for file in 0..FILES {
    client.edit(work, &path_of(file), 0, 0, seed).unwrap();
  }
  assert_eq!(client.submit(work).unwrap(), Submitted::Accepted(1));
  client.destroy(work).unwrap();
  green
}

fn overlaps(a: &Told, b: &Told) -> bool {
  a.file == b.file && a.block == b.block
}

/// The design's rule over what the clients were told: each version told once, no accept over an overlapping accept
/// made after its base, no conflict without one.
fn judge(accepted: &[&Told], conflicts: &[&Told], head: u64) {
  let versions: Vec<u64> = accepted.iter().filter_map(|t| t.version).collect();
  assert_eq!(
    versions,
    (2..=head).collect::<Vec<_>>(),
    "each version told to exactly one submit"
  );
  for a in accepted {
    let version = a.version.unwrap();
    let overwritten = accepted
      .iter()
      .filter_map(|b| {
        b.version
          .filter(|v| *v > a.base && *v < version && overlaps(a, b))
      })
      .next();
    assert_eq!(
      overwritten, None,
      "the accept at {version} (base {}) overwrote an overlapping accept",
      a.base
    );
  }
  for c in conflicts {
    assert!(
      accepted
        .iter()
        .any(|b| b.version.is_some_and(|v| v > c.base) && overlaps(c, b)),
      "a conflict from base {} with no overlapping accept after its base: {c:?}",
      c.base
    );
  }
}

/// Every version, read pinned, is the seed with the accepts up to it applied in version order.
fn read_back(client: &mut Client, green: VolumeId, accepted: &[&Told], (head, seed): (u64, &[u8])) {
  let mut model: Vec<Vec<u8>> = vec![seed.to_vec(); FILES];
  for version in 1..=head {
    if let Some(a) = accepted.iter().find(|t| t.version == Some(version)) {
      model[a.file][a.block * BLOCK..(a.block + 1) * BLOCK].copy_from_slice(&a.bytes);
    }
    for (file, expected) in model.iter().enumerate() {
      assert_eq!(
        &client
          .read(green, &path_of(file), ReadAt::Version { version })
          .unwrap(),
        expected,
        "{} at version {version}",
        path_of(file)
      );
    }
  }
}

/// T-6.7 (live, concurrent; §4.16 "The verdict", "Commit"): do race `CLIENTS` clients' overwrites of a few shared
/// blocks through their own works into one green; expect every accept and every conflict to be what the design's rule
/// gives for the order the green committed them in, and every version to read back as the seed with the accepted
/// edits up to it applied. Non-vacuous: both verdicts must occur.
#[test]
fn racing_overwrites_are_judged_by_the_rule_and_every_version_reads_back() {
  let (daemon, instance) = start();
  let mut client = connect(&instance);
  let seed = vec![b'.'; BLOCKS * BLOCK];
  let green = seeded_green(&mut client, &seed);
  let told: Vec<Told> = std::thread::scope(|scope| {
    let handles: Vec<_> = (0..CLIENTS)
      .map(|index| {
        let instance = instance.as_str();
        scope.spawn(move || race(instance, green, index))
      })
      .collect();
    handles
      .into_iter()
      .flat_map(|handle| handle.join().unwrap())
      .collect()
  });
  let mut accepted: Vec<&Told> = told.iter().filter(|t| t.version.is_some()).collect();
  accepted.sort_by_key(|t| t.version);
  let conflicts: Vec<&Told> = told.iter().filter(|t| t.version.is_none()).collect();
  assert!(
    !accepted.is_empty() && !conflicts.is_empty(),
    "both verdicts occurred: {} accepts, {} conflicts",
    accepted.len(),
    conflicts.len()
  );
  let head = client.versions(green).unwrap();
  judge(&accepted, &conflicts, head);
  read_back(&mut client, green, &accepted, (head, &seed));
  eprintln!(
    "merge race: {} accepts, {} conflicts, head {head}",
    accepted.len(),
    conflicts.len()
  );
  drop(client);
  daemon.stop();
}
