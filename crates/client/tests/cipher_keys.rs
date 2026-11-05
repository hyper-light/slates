//! A shard's content keys are bounded by what is alive (A-208; A-99's cipher, `crates/server/src/content_cipher.rs`): a
//! key is held for each volume sealing under it and for each sealed chunk record that names it, and goes with its last
//! hold. Driven through the Rust client against an in-process daemon, as an agent provisions: volumes created, written
//! past a chunk so their content seals, and destroyed.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn
)]

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the test's machine profile (milliseconds), as `client.rs` measures it.
const PROBE_MS: u64 = 5;
/// Shape: the volumes the loop creates, writes and destroys: enough that a key kept per volume is plain in the count.
const CYCLES: u64 = 64;
/// Shape: the file each volume is written with: a mebibyte, past the chunk size of every host (sixteen pages of at most
/// 64 KiB), so the write seals chunks under the volume's key as it goes (A-99).
const FILE_BYTES: usize = 1 << 20;
/// Shape: the room each volume has: four times its file, for the rewrite's charge and the records.
const VOLUME_BYTES: u64 = 4 << 20;
/// Shape: how long the daemon gets to finish the destroys, which it runs in cooperative slices after each reply: a
/// liveness budget per volume, the time the anchor would give each before it presumed the daemon dead.
const DESTROYS_WAIT: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS.saturating_mul(CYCLES));

fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  let measured = PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures");
  // The process's seal-key region is made once, by the first daemon (`slates_server::seal_keys::reserve`).
  static RESERVED: OnceLock<bool> = OnceLock::new();
  let _ = RESERVED.get_or_init(|| {
    let config = DaemonConfig::derive(&measured, "ck-reserve", Some(1));
    slates_server::seal_keys::reserve(config.seal_key_slots)
  });
  measured
}

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn one_shard_daemon(tag: &str) -> (Daemon, Client) {
  let profile = profile();
  let instance = format!("ck-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-ck-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let started = Instant::now();
  let client = loop {
    match Client::connect(&instance, deadlines()) {
      Ok(client) => break client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < DESTROYS_WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  };
  (daemon, client)
}

/// A-208. Do: on a one-shard daemon that seals, create a volume, write a mebibyte file into it so its content seals
/// under the volume's key, and destroy it; 64 times. Expect: once the destroys finish, the shard holds exactly the
/// keys it held before the loop. Before A-208 the cipher kept a key per volume it had ever made and never let one go,
/// so the count rose by one a volume, without bound, and every create scanned all of them.
#[test]
fn a_create_and_destroy_loop_leaves_the_shards_keys_where_they_started() {
  let (daemon, mut client) = one_shard_daemon("loop");
  let floor = daemon.content_keys_held().unwrap();
  let bytes: Vec<u8> = (0..FILE_BYTES)
    .map(|at| u8::try_from(at % 251).unwrap().wrapping_add(1))
    .collect();
  for cycle in 0..CYCLES {
    let id = client
      .create(&CreateSpec {
        name: format!("keys-{cycle}"),
        size: SizeClass::Bounded {
          limit: VOLUME_BYTES,
        },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    let attached = client.attach(id, None, Intent::Write).unwrap();
    client
      .fs_write((id, attached.attachment), "/f.bin", &bytes, 0o644)
      .unwrap();
    client.detach(attached.attachment).unwrap();
    assert!(
      daemon.content_keys_held().unwrap() > floor,
      "cycle {cycle}: a live volume holds its key"
    );
    client.destroy(id).unwrap();
  }
  let started = Instant::now();
  let mut held = daemon.content_keys_held().unwrap();
  while held != floor && started.elapsed() < DESTROYS_WAIT {
    // The destroys run as the daemon's own slices: the test sleeps between observations rather than spinning on a core
    // the shard needs, as the D-9 lint's exception for tests allows.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_nanos(LIVENESS_BUDGET_NS / CYCLES));
    held = daemon.content_keys_held().unwrap();
  }
  assert_eq!(
    held, floor,
    "keys held after {CYCLES} volumes were made, written and destroyed (floor {floor})"
  );
  daemon.stop();
}
