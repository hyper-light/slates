//! A-92 piece 2b: the key hierarchy a volume's sealed content hangs from — the node root (piece 2a), a tenant key per
//! (account, partition) wrapped by it, the tenant's naming key and the volume's lineage key wrapped by the tenant key —
//! recorded in the partition and unwrapped again after a daemon restart, never written to a disk. Driven through a
//! real daemon under an anchor the test plays, with the content plane's own seal (`slates_cluster::sealed`).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::unwrap_in_result
)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use slates_anchor::AnchorSegment;
use slates_archive::Archive;
use slates_cluster::sealed::{SealedChunk, SealedError, open_chunk, seal_chunk};
use slates_db::catalog::VolumeId;
use slates_ipc::HeldRendezvous;
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::head::{HeadKey, HeadNaming, HeadSealing};
use slates_server::seal_keys::{
  SealKeyError, accept_handed_lineage, accept_pair, adopt_lineage, hand_lineage, lineage,
  lineage_recorded, namer, recorded_lineage, recorded_namer,
};
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
const ACCOUNT: u64 = 501;
/// Format: the volume the keys are for (its records need no volume to exist: a key is keyed by id).
const VOLUME: VolumeId = VolumeId { bytes: [5; 16] };

fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures")
}

/// The anchor the test plays: a segment and its content object, and the instance's rendezvous (A-114).
struct PlayedAnchor {
  segment: AnchorSegment,
  rendezvous: HeldRendezvous,
}

/// The anchor the test plays for `config`'s instance, its segment and content object under `tag`.
fn played_anchor(config: &DaemonConfig, tag: &str) -> PlayedAnchor {
  let segment = AnchorSegment::create(
    &common::fixture_object("slates-seg", tag),
    &profile().facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &common::fixture_object("slates-con", tag),
    config.content_bytes(),
  )
  .unwrap();
  PlayedAnchor {
    segment,
    rendezvous: HeldRendezvous::hold(&config.instance).unwrap(),
  }
}

/// A daemon over `anchor`'s handoff.
fn start_over(config: &DaemonConfig, anchor: &PlayedAnchor) -> Daemon {
  let (handoff, len) = anchor.segment.handoff().unwrap();
  let content = anchor.segment.content_handoff().unwrap();
  Daemon::start(
    &profile(),
    config.clone(),
    SegmentSource::Handoff {
      handoff,
      len,
      content,
      rendezvous: anchor.rendezvous.adoption(),
    },
  )
  .unwrap()
}

/// A chunk sealed for `VOLUME` under the keys `daemon`'s control shard holds for it (made and recorded if new).
fn seal_on(daemon: &Daemon) -> SealedChunk {
  daemon
    .observe_control(OBSERVE_NS, |state| {
      let lineage = lineage(state, VOLUME, ACCOUNT).unwrap();
      let namer = namer(state, VOLUME, ACCOUNT).unwrap();
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
      let namer = namer(state, VOLUME, ACCOUNT).unwrap();
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
  // The anchor dies before a fresh one starts for the instance, as one instance's two anchors never live at once (the
  // second could not hold the rendezvous).
  drop(anchor);
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
      let namer = namer(state, volume, ACCOUNT).unwrap();
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
      let namer = namer(state, volume, ACCOUNT).unwrap();
      open_chunk(&lineage, &namer, &sealed).map(|chunk| chunk.payload)
    })
    .unwrap();
  daemon.stop();
  assert_eq!(
    opened,
    Err(SealedError::Seal(hyper_seal::SealError::Unwrap))
  );
}

/// A-271: a successor that reads a volume's keys before they reach it records none, and adopts the right one when it
/// arrives. Do ask a daemon whose partition holds no keys for a volume for its recorded lineage and namer, then hand it
/// a lineage wrapped under its root, as `successor_lineage` does. Expect nothing read and nothing recorded by the reads,
/// and the handed key's id recorded after. Before, the read path made a key: a live takeover's successor that could not
/// unwrap the log's key made one, failed to open the log under it, and kept it, so the right key was never adopted.
#[test]
fn a_reader_without_the_key_records_none_and_adopts_the_right_one() {
  let instance = format!("seal-reader-{}", std::process::id());
  let config = DaemonConfig::derive(&profile(), &instance, Some(SHARDS));
  let anchor = played_anchor(&config, "seal-reader");
  let daemon = start_over(&config, &anchor);
  let owner = slates_db::catalog::Principal::Uid {
    host: slates_db::HostId(1),
    uid: 501,
  };
  let account = owner.tenant();
  let handed = [9; 16];
  let (read, recorded_by_reads, adopted) = daemon
    .observe_control(OBSERVE_NS, move |state| {
      let read = (
        recorded_lineage(state, VOLUME, account).unwrap().is_some(),
        recorded_namer(state, VOLUME, account).unwrap().is_some(),
      );
      let recorded_by_reads = lineage_recorded(state, VOLUME);
      let root = state.seal_root.as_ref().unwrap();
      let (secret, _) = root.make_child().unwrap();
      let wrapped = root.wrap(&secret).unwrap().encode().to_vec();
      adopt_lineage(state, VOLUME, &owner, (handed, wrapped), None).unwrap();
      let adopted = recorded_lineage(state, VOLUME, account)
        .unwrap()
        .map(|key| key.id().0);
      (read, recorded_by_reads, adopted)
    })
    .unwrap();
  daemon.stop();
  assert_eq!(
    read,
    (false, false),
    "no lineage or namer recorded: none read"
  );
  assert!(!recorded_by_reads, "and none made by the reads");
  assert_eq!(
    adopted,
    Some(handed),
    "the key handed over is the one recorded"
  );
}

/// Format: the generation the test's hand-made pair and lineage keys carry, as the daemon makes its own (seal.md §3.4).
const GENERATION: u32 = 1;

/// A-295, the hand-off of a volume's key between candidates, on one daemon's control shard. Do record a pair key from an
/// owner's anchor (delivered to this node's recipient, as `deliver_pairs` and `accept_pair` do); build that owner's
/// sealing of a volume (the lineage key for this node's anchor under the pair, the volume's naming key under the lineage
/// key); hand the lineage key from that sealing to this node's own recipient key and take the hand-off back. Then take a
/// wrong key under the same id, the right key for a sealing with no naming record, and bytes that are no hand-off; and
/// hand from a sealing with no entry for this node. Expect the hand-off kept, the key kept (opened under the node's root)
/// the lineage key itself; every other refused, the unnamed one as `Unchecked`; and nothing handed without an entry.
#[test]
fn a_lineage_key_is_handed_from_a_candidates_own_entry_and_kept_only_when_it_checks() {
  use hyper_seal::keys::{KeyId, Wrapped, WrappingKey};
  use hyper_seal::recipient::{Mode, wrap_to};
  let instance = format!("seal-hand-{}", std::process::id());
  let config = DaemonConfig::derive(&profile(), &instance, Some(SHARDS));
  let anchor = played_anchor(&config, "seal-hand");
  let daemon = start_over(&config, &anchor);
  let outcome = daemon
    .observe_control(OBSERVE_NS, move |state| {
      let public = state.seal_recipient.as_ref().unwrap().public();
      let (owner_anchor, partition, pair_id, lineage_id) = (77_u64, 0_u16, [6; 16], [3; 16]);
      // Keys made under this node's root, each kept as its record so a copy can be opened again.
      let (pair_secret, pair_record, lineage_record, wrong_record) = {
        let root = state.seal_root.as_ref().unwrap();
        let (pair_secret, pair_record) = root.make_child().unwrap();
        let (_, lineage_record) = root.make_child().unwrap();
        let (_, wrong_record) = root.make_child().unwrap();
        (pair_secret, pair_record, lineage_record, wrong_record)
      };
      let pair_delivery = wrap_to(&public, Mode::Hybrid, KeyId(pair_id), &pair_secret).unwrap();
      accept_pair(state, (owner_anchor, partition), pair_id, &pair_delivery).unwrap();
      let root = state.seal_root.as_ref().unwrap();
      let opened = |record: &Wrapped| root.unwrap(record).unwrap();
      let pair = WrappingKey::new(KeyId(pair_id), GENERATION, opened(&pair_record));
      let lineage = WrappingKey::new(KeyId(lineage_id), GENERATION, opened(&lineage_record));
      let (_, naming_record) = lineage.make_child().unwrap();
      let sealing = HeadSealing {
        owner_anchor,
        partition,
        lineage: lineage_id,
        keys: vec![HeadKey {
          anchor: state.origin_anchor.0,
          wrapped: pair
            .wrap(&opened(&lineage_record))
            .unwrap()
            .encode()
            .to_vec(),
        }],
        naming: Some(HeadNaming {
          id: [4; 16],
          wrapped: naming_record.encode().to_vec(),
        }),
      };
      let handed = hand_lineage(state, &sealing, &public);
      let kept = handed
        .as_ref()
        .and_then(|delivery| accept_handed_lineage(state, &sealing, delivery).ok())
        .map(|(id, record)| {
          let key = opened(&Wrapped::decode(&record).unwrap());
          (id, key.bytes() == opened(&lineage_record).bytes())
        });
      let wrong_delivery = wrap_to(
        &public,
        Mode::Hybrid,
        KeyId(lineage_id),
        &opened(&wrong_record),
      )
      .unwrap();
      let wrong_refused = accept_handed_lineage(state, &sealing, &wrong_delivery).is_err();
      let unnamed = HeadSealing {
        naming: None,
        ..sealing.clone()
      };
      let unchecked = handed.as_ref().is_some_and(|delivery| {
        matches!(
          accept_handed_lineage(state, &unnamed, delivery),
          Err(SealKeyError::Unchecked)
        )
      });
      let garbage_refused = accept_handed_lineage(state, &sealing, &[0; 64]).is_err();
      let no_entry = HeadSealing {
        keys: Vec::new(),
        ..sealing
      };
      let none_handed = hand_lineage(state, &no_entry, &public).is_none();
      (kept, wrong_refused, unchecked, garbage_refused, none_handed)
    })
    .unwrap();
  daemon.stop();
  let (kept, wrong_refused, unchecked, garbage_refused, none_handed) = outcome;
  assert_eq!(
    kept,
    Some(([3; 16], true)),
    "the hand-off is kept, and the key kept is the volume's lineage key"
  );
  assert!(
    wrong_refused,
    "a wrong key under the volume's key id is refused"
  );
  assert!(
    unchecked,
    "a sealing with no naming record cannot check a key: refused Unchecked"
  );
  assert!(garbage_refused, "bytes that are no hand-off are refused");
  assert!(none_handed, "a node with no entry of its own hands nothing");
}
