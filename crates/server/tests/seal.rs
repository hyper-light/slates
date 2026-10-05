//! A-92 piece 2b: the key hierarchy a volume's sealed content hangs from — the node root (piece 2a), a tenant key per
//! (account, partition) wrapped by it, the tenant's naming key and the volume's lineage key wrapped by the tenant key —
//! recorded in the partition and unwrapped again after a daemon restart, never written to a disk. Driven through a
//! real daemon under an anchor the test plays, with the content plane's own seal (`slates_cluster::sealed`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::OnceLock;
use std::time::Duration;

use slates_anchor::AnchorSegment;
use slates_archive::Archive;
use slates_cluster::sealed::{SealedChunk, SealedError, open_chunk, seal_chunk};
use slates_db::catalog::VolumeId;
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::seal_keys::{lineage, namer};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the per-probe budget of the test's machine profile (a quick measurement, no codecs or core matrix).
const PROBE_MS: u64 = 25;
/// Shape: one shard: the control shard holds the volume's records.
const SHARDS: u16 = 1;
/// Shape: an observation's budget, long against a loaded test host.
const OBSERVE_NS: u64 = 10_000_000_000;
/// Shape: the segment the test seals in: one 4 KiB base page.
const SEGMENT: u32 = 4096;
/// Format: the account the volume belongs to (any host account).
const ACCOUNT: u32 = 501;
/// Format: the volume the keys are for (its records need no volume to exist: a key is keyed by id).
const VOLUME: VolumeId = VolumeId { bytes: [5; 16] };

fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure(ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures")
}

/// The anchor the test plays: a segment and its content object under `tag`.
fn played_anchor(config: &DaemonConfig, tag: &str) -> AnchorSegment {
  AnchorSegment::create(
    &format!("slates-seg-{tag}-{}", std::process::id()),
    &profile().facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-{tag}-{}", std::process::id()),
    config.content_bytes(),
  )
  .unwrap()
}

/// A daemon over `anchor`'s handoff.
fn start_over(config: &DaemonConfig, anchor: &AnchorSegment) -> Daemon {
  let (handoff, len) = anchor.handoff().unwrap();
  let content = anchor.content_handoff().unwrap();
  Daemon::start(
    &profile(),
    config.clone(),
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    },
  )
  .unwrap()
}

/// A chunk sealed for `VOLUME` under the keys `daemon`'s control shard holds for it (made and recorded if new).
fn seal_on(daemon: &Daemon) -> SealedChunk {
  daemon
    .observe_control(OBSERVE_NS, |state| {
      let lineage = lineage(state, VOLUME, ACCOUNT).unwrap();
      let namer = namer(state, ACCOUNT).unwrap();
      seal_chunk(
        &lineage,
        &namer,
        &Archive::raw_chunk(b"sealed before".repeat(500)),
        SEGMENT,
      )
      .unwrap()
    })
    .unwrap()
}

/// `sealed` opened under the keys `daemon`'s control shard holds for `VOLUME` (made now if the partition has none).
fn open_on(daemon: &Daemon, sealed: SealedChunk) -> Result<Vec<u8>, SealedError> {
  daemon
    .observe_control(OBSERVE_NS, move |state| {
      let lineage = lineage(state, VOLUME, ACCOUNT).unwrap();
      let namer = namer(state, ACCOUNT).unwrap();
      open_chunk(&lineage, &namer, &sealed).map(|chunk| chunk.payload)
    })
    .unwrap()
}

/// A-92 piece 2b: do seal a chunk under a volume's lineage key on a daemon, restart the daemon under the same anchor
/// and open it there, then open it on a daemon under a fresh anchor; expect the restarted daemon to unwrap the same
/// tenant, naming and lineage keys from the partition's records under the adopted root and open the chunk byte for
/// byte, and the fresh anchor's daemon to make keys of its own and be refused (another root wraps nothing of the
/// first: the keys die with the RAM they protect).
#[test]
fn a_volumes_keys_survive_a_daemon_restart_and_die_with_the_anchor() {
  let instance = format!("seal-keys-{}", std::process::id());
  let config = DaemonConfig::derive(&profile(), &instance, Some(SHARDS));
  let anchor = played_anchor(&config, "seal-keys");
  let first = start_over(&config, &anchor);
  let sealed = seal_on(&first);
  first.stop();
  let second = start_over(&config, &anchor);
  let opened = open_on(&second, sealed.clone());
  second.stop();
  assert_eq!(
    opened.unwrap(),
    b"sealed before".repeat(500),
    "the restart opens it"
  );
  let fresh = played_anchor(&config, "seal-keys-fresh");
  let third = start_over(&config, &fresh);
  let foreign = open_on(&third, sealed);
  third.stop();
  assert_eq!(
    foreign,
    Err(SealedError::Seal(hyper_seal::SealError::Unwrap)),
    "another anchor's keys unwrap nothing of the first's"
  );
}

/// Shape: how long the test waits for a destroyed volume's record to go (the reaper steps destroys on its cadence).
const DESTROY_WAIT: Duration = Duration::from_secs(30);

/// A typed client of `instance`, retried until the daemon answers.
fn client_of(instance: &str) -> slates_client::Client {
  let deadlines = slates_client::Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let started = std::time::Instant::now();
  loop {
    match slates_client::Client::connect(instance, deadlines) {
      Ok(client) => return client,
      Err(e) if started.elapsed() > DESTROY_WAIT => panic!("{e}"),
      Err(_) => std::hint::spin_loop(),
    }
  }
}

/// A-92 piece 2b (seal.md §3.1, cryptographic erase): do create a volume, seal a chunk under its lineage key, destroy
/// the volume and wait for its record to go, then open the chunk under the key the shard holds for that id now;
/// expect the lineage record gone with the volume (the shard makes a new key for the id) and the chunk refused
/// `Unwrap` — a destroyed volume's sealed content opens for no one, on any holder.
#[test]
fn destroying_a_volume_erases_its_sealed_content() {
  let instance = format!("seal-erase-{}", std::process::id());
  let config = DaemonConfig::derive(&profile(), &instance, Some(SHARDS));
  let anchor = played_anchor(&config, "seal-erase");
  let daemon = start_over(&config, &anchor);
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = client_of(&instance);
  let created = client
    .create(&slates_client::CreateSpec {
      name: "erased".to_owned(),
      size: slates_client::SizeClass::Bounded { limit: 1 << 20 },
      names: slates_client::NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let volume = VolumeId {
    bytes: created.bytes,
  };
  let sealed = daemon
    .observe_control(OBSERVE_NS, move |state| {
      let lineage = lineage(state, volume, ACCOUNT).unwrap();
      let namer = namer(state, ACCOUNT).unwrap();
      seal_chunk(
        &lineage,
        &namer,
        &Archive::raw_chunk(b"to be erased".repeat(100)),
        SEGMENT,
      )
      .unwrap()
    })
    .unwrap();
  client.destroy(created).unwrap();
  let started = std::time::Instant::now();
  while daemon
    .observe_control(OBSERVE_NS, move |state| {
      state.db.partition().volume(volume).is_some()
    })
    .unwrap()
  {
    assert!(started.elapsed() < DESTROY_WAIT, "the destroy completes");
    // A test thread outside the runtime waits for the reaper's next cadence; it holds no shard.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(10));
  }
  let opened = daemon
    .observe_control(OBSERVE_NS, move |state| {
      let lineage = lineage(state, volume, ACCOUNT).unwrap();
      let namer = namer(state, ACCOUNT).unwrap();
      open_chunk(&lineage, &namer, &sealed).map(|chunk| chunk.payload)
    })
    .unwrap();
  daemon.stop();
  assert_eq!(
    opened,
    Err(SealedError::Seal(hyper_seal::SealError::Unwrap))
  );
}
