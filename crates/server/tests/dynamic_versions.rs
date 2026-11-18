//! A dynamic volume's inode dimension (§4.2 "Sacred bounded claims" and "Dynamic volumes and base cache"; A-342): its
//! inode allowance is a ceiling, not a claim. It reserves no version slots at admission; it holds a slot for each live
//! inode it has made, taken from the shard's unpromised version capacity as the inode is issued, never giving one back
//! while it lives, and returns the hold at teardown, as its bytes grow from the shard's unpromised byte capacity. Only
//! a bounded claim reserves its whole allowance.
//!
//! The bug these tests were red on (docs/bugs/2026-11-18-a-dynamic-volume-reserved-the-whole-version-slab.md): kiln's
//! agent volumes are dynamic with a maximum of a quarter of the machine's memory, which derives an allowance of the
//! whole slab, so one volume holding 54 bytes committed 7,456,539 of its shard's 7,456,540 version slots. The review's
//! snapshot then left no unpromised slot to retain the directory that a first read of an untouched base file
//! materializes its entry into, and the read was refused `NoSpace`.

mod common;

use common::client::connect;
use common::target::target_dir;
use slates_client::{ClientError, CreateSpec, NamePolicy, SizeClass};
use slates_ipc::protocol::{Intent, ReadAt, Refusal};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the version slab's slots, small enough that one big-maximum volume's derived inode allowance is all of it,
/// and large enough to hold several small volumes' inodes and tries.
const SLAB_SLOTS: usize = 64;
/// Shape: the shard's arena, sized with the slab so a slot is about one mebibyte of quota (A-109).
const SLAB_RESERVE: u64 = 64 << 20;
/// Shape: a dynamic maximum of twice the arena, so its derived inode allowance is every reservable slot, as a kiln
/// agent volume's quarter of the machine's memory is on a real shard.
const BIG_MAX: u64 = 2 * SLAB_RESERVE;
/// Shape: a bounded claim of a quarter of the arena: its allowance is about a quarter of the slab, reserved whole.
const CLAIM: u64 = SLAB_RESERVE / 4;
/// Shape: how long a destroyed volume's teardown, which runs in slices, may take to return its hold.
const DESTROY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A test daemon on one shard (every volume meets the one version budget) whose inode-version slab is
/// [`SLAB_SLOTS`] slots over a [`SLAB_RESERVE`] arena.
fn capped_daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-dynver-{tag}-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(1));
  config.store.max_inodes = SLAB_SLOTS;
  config.reserve_per_shard = SLAB_RESERVE;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("dynver-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn spec(name: &str, size: SizeClass, base: Option<String>) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    size,
    names: NamePolicy::Exact,
    require_locked: false,
    base,
  }
}

/// The version slots the daemon's one shard has committed.
fn committed_versions(client: &mut slates_client::Client) -> u64 {
  let report = client.daemon_status().unwrap();
  report.shards.iter().map(|s| s.committed_versions).sum()
}

/// A-342. Do: over a base folder holding `keep.txt` and `edit.txt`, make a dynamic volume whose maximum derives the
/// whole version slab as its inode allowance, write `edit.txt` through an attachment, snapshot the volume, read the
/// untouched `keep.txt` at the head, then make a second dynamic volume on the same shard. Expect: the read answers the
/// disk's bytes, the second volume is admitted, and the shard's committed version slots are a handful (the two
/// volumes' live inodes and what the snapshot retains), never the slab.
#[test]
fn a_dynamic_volume_holds_slots_only_for_its_inodes_so_its_snapshot_retains_and_a_second_volume_fits()
 {
  let (daemon, instance) = capped_daemon("hold");
  let target = target_dir();
  target.seed("keep.txt", b"base keep");
  target.seed("edit.txt", b"base edit");
  let mut client = connect(&instance);
  let big = SizeClass::Dynamic { max: BIG_MAX };
  let volume = client
    .create(&spec("agent", big, Some(target.path.clone())))
    .unwrap();
  let attachment = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write((volume, attachment), "edit.txt", b"edited", 0o644)
    .unwrap();
  client.snapshot(volume).unwrap();
  // The first read of an untouched base file after the snapshot materializes its entry into a directory the snapshot
  // pins: the retention that takes is the slot the volume must not have promised to itself.
  assert_eq!(
    client.read(volume, "/keep.txt", ReadAt::Head).unwrap(),
    b"base keep"
  );
  client
    .create(&spec("second", big, None))
    .expect("a second dynamic volume fits beside the first on the one shard");
  let committed = committed_versions(&mut client);
  let slab = u64::try_from(SLAB_SLOTS).unwrap();
  assert!(
    committed < slab / 4,
    "the volumes hold slots for their inodes, not their maxima: {committed} of {slab} committed"
  );
  daemon.stop();
}

/// A-342, AC-2.11. Do: admit a bounded claim of a quarter of the arena, then make empty files in a dynamic volume until
/// it is refused, then make the claim's files up to its allowance, then destroy the dynamic volume and make another.
/// Expect: the dynamic volume's files are refused `NoSpace` once the unpromised slots are gone, every one of the
/// claim's files within its allowance is made after that (its slots were never the dynamic volume's to take), and the
/// destroyed volume's hold comes back whole, so a new dynamic volume is admitted.
#[test]
fn a_dynamic_volume_grows_into_unpromised_slots_only_and_a_bounded_claim_stays_spendable() {
  let (daemon, instance) = capped_daemon("claim");
  let mut client = connect(&instance);
  let claim = client
    .create(&spec("claim", SizeClass::Bounded { limit: CLAIM }, None))
    .unwrap();
  let after_claim = committed_versions(&mut client);
  let grower = client
    .create(&spec("grower", SizeClass::Dynamic { max: BIG_MAX }, None))
    .unwrap();
  let grower_at = (
    grower,
    client
      .attach(grower, None, Intent::Write)
      .unwrap()
      .attachment,
  );
  // Empty files cost no bytes, so only the inode dimension can refuse them.
  let slab = u64::try_from(SLAB_SLOTS).unwrap();
  let mut made = 0u64;
  let refusal = loop {
    assert!(
      made <= slab,
      "the dynamic volume made more files than the slab has slots"
    );
    match client.fs_write(grower_at, &format!("f{made}"), b"", 0o644) {
      Ok(_) => made += 1,
      Err(e) => break e,
    }
  };
  assert!(
    matches!(refusal, ClientError::Refused(Refusal::NoSpace)),
    "a dynamic volume's growth past the unpromised slots is ENOSPC: {refusal:?}"
  );
  assert!(
    made > 0,
    "the dynamic volume grew into the unpromised slots"
  );
  // The claim's own allowance: what its admission committed.
  let claimed = after_claim;
  let claim_at = (
    claim,
    client
      .attach(claim, None, Intent::Write)
      .unwrap()
      .attachment,
  );
  // Its root is one of its live inodes; every other slot it reserved takes a file now.
  for n in 1..claimed {
    client
      .fs_write(claim_at, &format!("c{n}"), b"", 0o644)
      .unwrap_or_else(|e| panic!("the claim's file {n} of {claimed} within its allowance: {e:?}"));
  }
  client.destroy(grower).unwrap();
  let started = std::time::Instant::now();
  while let Err(e) = client.create(&spec("fresh", SizeClass::Dynamic { max: BIG_MAX }, None)) {
    assert!(
      started.elapsed() < DESTROY_WAIT,
      "the destroyed volume's hold never came back: {e:?}"
    );
  }
  daemon.stop();
}
