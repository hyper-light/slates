// Test harness code: an unwrap here is a failed test, which is what it should be.

use slates_anchor::{AnchorSegment, Geometry};
use slates_db::Op;
use slates_db::catalog::{
  BaseRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId, VolumeId,
  VolumeRecord, VolumeState,
};
use slates_db::partition::PartitionCaps;
use slates_db::replay::{Recovered, Tail, recover};
use slates_machine::facts::Identity;

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: a wide log ring: room for the whole run's tail, so the full-ring path never forces a snapshot and the cadence
/// alone decides.
const WIDE_LOG_BYTES: u64 = 64 << 20;
/// Shape: a log ring about the partition's size (raft's case): the full-ring path forces a snapshot about once a ring,
/// which the bound must survive too.
const NARROW_LOG_BYTES: u64 = 8 << 20;
/// Shape: a snapshot slot: room for the partition the test builds.
const SNAPSHOT_BYTES: u64 = 32 << 20;
/// Shape: the bytes of each increment that builds the partition.
const INCREMENT_BYTES: usize = 256 << 10;
/// Shape: the increments: a partition of about 8 MiB, far more than the 1 MB cadence a fresh boot took.
const INCREMENTS: usize = 32;
/// Shape: the log the test appends after the partition is built, in small records that do not grow it.
const TAIL_BYTES: u64 = 16 << 20;
/// Shape: a recovery budget small enough that the log it allows fits a test's ring: 10 ms, a hundredth of the
/// daemon's, so at up to 12,800 bytes a microsecond of replay the tail stays inside `BUDGET_LOG_BYTES` and only the
/// trigger decides when a snapshot comes.
const SMALL_BUDGET_NS: u64 = 10_000_000;
/// Shape: the log ring for the budget test, room for a cadence of `SMALL_BUDGET_NS` at up to 12,800 bytes a
/// microsecond.
const BUDGET_LOG_BYTES: u64 = 128 << 20;
/// Shape: snapshots taken before the measured tail, so it starts from a snapshot of the partition in its steady state
/// and of its records' kind, long after the fresh boot.
const SETTLING_SNAPSHOTS: u64 = 4;
/// Shape: snapshots a stall may bring early while the longest-tail test approaches the trigger, each restarting the
/// approach, which then predicts from at least that stall, before the test gives up.
const EARLY_SNAPSHOTS: u64 = 3;
/// Shape: recoveries of the same tail, the median judged: recovery writes nothing to a tail that verifies, so each
/// replays the same bytes. The budget is a bound a recovery meets on the machine as it is, so the median, not the
/// fastest, is held to it (raft); all of them are printed.
const RECOVERIES: usize = 5;

fn identity() -> Identity {
  Identity {
    cpu: "test".into(),
    os: "test".into(),
    arch: "test".into(),
    cores: 4,
    memory: 1,
    page: PAGE,
  }
}

/// Tells this process's segments apart: libtest runs the tests at once, two of them over the same log size, and Windows
/// refuses a second object of one name while the first is held (A-176).
static NEXT_SEGMENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn segment(log_bytes: u64) -> AnchorSegment {
  let unique = NEXT_SEGMENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  AnchorSegment::create(
    &format!(
      "slates-db-cadence-{log_bytes}-{}-{unique}",
      std::process::id()
    ),
    &identity(),
    Geometry {
      partitions: 1,
      page: PAGE,
      profile_bytes: PAGE,
      log_bytes,
      snapshot_bytes: SNAPSHOT_BYTES,
      landing_slots: 1,
      landing_slot_bytes: PAGE,
    },
  )
  .unwrap()
}

fn caps() -> PartitionCaps {
  PartitionCaps {
    volumes: 1 << 12,
    snapshots: 1 << 12,
    attachments: 1 << 12,
    segment_slots: 64,
    timers: 1 << 12,
    tick_ns: 1_000,
    green_chain_bytes: 64 << 20,
    audit_records: 1 << 12,
  }
}

fn green() -> VolumeRecord {
  let mut bytes = [0u8; 16];
  bytes[0] = 1;
  VolumeRecord {
    id: VolumeId { bytes },
    name: "green".to_owned(),
    owner_shard: 0,
    policy: PolicyRecord {
      size: SizeClass::Bounded { limit: 1 << 30 },
      names: NamePolicy::Exact,
      require_locked: false,
      role: Role::Plain,
      live_shipped: false,
      live_holders: None,
      write: None,
      write_version: 0,
      signers: Vec::new(),
    },
    base: BaseRecord::Scratch,
    head: SnapshotId { value: 0 },
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1000,
    },
    access: vec![],
    created_ns: 0,
    catalog_version: 0,
    taken_over: None,
  }
}

/// What one run measured: the partition's encoded bytes, the log appended after it was built, and what the snapshots
/// wrote meanwhile.
struct Absorbed {
  partition: u64,
  appended: u64,
  written: u64,
}

/// Recovers a fresh partition over a log ring of `log_bytes` (nothing to replay, so no measured throughput), builds it
/// to about 8 MiB with 32 increments of 256 KiB, then appends 16 MiB of log in small records that change one volume's
/// accounting and do not grow it.
fn absorb(log_bytes: u64) -> Absorbed {
  let mut seg = segment(log_bytes);
  let mut db = recover(&mut seg, 0, caps(), 0).unwrap().0;
  let record = green();
  let id = record.id;
  db.mutate(&mut seg, &Op::VolumeCreated { record }, 0)
    .unwrap();
  for n in 0..INCREMENTS {
    let increment = vec![u8::try_from(n % 251).unwrap(); INCREMENT_BYTES];
    db.mutate(
      &mut seg,
      &Op::GreenAdvanced {
        green: id,
        increment,
      },
      0,
    )
    .unwrap();
  }
  let partition = slates_wire::Wire::to_bytes(&db.partition().to_snapshot(0)).len() as u64;
  let before = db.stats();
  let mut n = 0u64;
  while db.stats().log_bytes_appended - before.log_bytes_appended < TAIL_BYTES {
    n += 1;
    let accounted = Op::VolumeAccounted {
      id,
      referenced_bytes: n,
      unique_bytes: n,
    };
    db.mutate(&mut seg, &accounted, 0).unwrap();
  }
  let after = db.stats();
  let absorbed = Absorbed {
    partition,
    appended: after.log_bytes_appended - before.log_bytes_appended,
    written: after.snapshot_bytes_written - before.snapshot_bytes_written,
  };
  println!(
    "ring {log_bytes}: partition {} bytes; {} bytes of log in {n} records; snapshots wrote {} bytes; tail {:?}, \
     floor {} bytes, longest snapshot {} ns",
    absorbed.partition,
    absorbed.appended,
    absorbed.written,
    after.tail,
    after.snapshot_floor_bytes,
    after.snapshot_ns_max
  );
  absorbed
}

/// The bound: the snapshots wrote at most twice the log they absorbed plus one partition.
fn assert_bounded(run: &Absorbed) {
  assert!(
    run.partition > INCREMENTS as u64 * INCREMENT_BYTES as u64,
    "the partition holds the increments"
  );
  assert!(
    run.written <= 2 * run.appended + run.partition,
    "snapshots wrote {} bytes while the log absorbed {} (partition {})",
    run.written,
    run.appended,
    run.partition
  );
}



/// Appends one small record that changes `id`'s accounting and does not grow the partition; returns the log bytes it
/// took.
fn account(db: &mut slates_db::replay::Db, seg: &mut AnchorSegment, id: VolumeId, n: u64) -> u64 {
  let before = db.stats().log_bytes_appended;
  let accounted = Op::VolumeAccounted {
    id,
    referenced_bytes: n,
    unique_bytes: n,
  };
  db.mutate(seg, &accounted, 0).unwrap();
  db.stats().log_bytes_appended - before
}


/// Shape: the large records of the mix test: one green increment of 64 KiB, folded at once by a small checkpoint, so
/// the partition stays small and its snapshot's size (the doubling floor) stays far below the budget's log.
const LARGE_RECORD_BYTES: usize = 64 << 10;
/// Shape: the checkpoint that folds each large increment: a few bytes, so the partition does not grow.
const FOLDING_CHECKPOINT_BYTES: usize = 8;
/// Shape: the small records appended before the first timed recovery of the mix test, which measures their replay.
const FIRST_SMALL_RECORDS: u64 = 64;
/// Shape: the replay each later chunk of small records adds, as a share of the budget: an eighth, so the checks step
/// through the tail finely enough to see it pass the budget.
const CHUNKS_PER_BUDGET: u64 = 8;
/// Shape: the timed recoveries of each check of the mix test, the median judged (raft), the spread of their inner
/// quartiles the check's own noise: five, as [`RECOVERIES`], so the quartiles are the second and the fourth cost and
/// the fastest and the slowest recovery stand outside the spread.
const CHECK_RECOVERIES: usize = RECOVERIES;

/// Appends one large record that does not grow the partition: a green increment, then the checkpoint that folds it.
fn advance_and_fold(db: &mut slates_db::replay::Db, seg: &mut AnchorSegment, id: VolumeId, n: u64) {
  let increment = vec![u8::try_from(n % 251).unwrap(); LARGE_RECORD_BYTES];
  db.mutate(
    seg,
    &Op::GreenAdvanced {
      green: id,
      increment,
    },
    0,
  )
  .unwrap();
  let version = db.partition().green_head(id);
  let checkpoint = vec![0u8; FOLDING_CHECKPOINT_BYTES];
  db.mutate(
    seg,
    &Op::GreenCheckpointed {
      green: id,
      version,
      checkpoint,
    },
    0,
  )
  .unwrap();
}

/// `CHECK_RECOVERIES` recoveries of the segment's tail as it stands ([`recovery_cost_ns`]): their median cost and their
/// spread, and the bytes replayed. The spread is the inner quartiles' (the middle half of the costs, the machine's own
/// noise at that moment), which one recovery preempted does not widen: the most less the least let one such recovery
/// cover a trigger three times late (13.9 ms past the baseline against an 8.2 ms range, load 37 on the Mac Studio).
#[derive(Clone, Copy, Debug)]
struct Timed {
  median_ns: u64,
  spread_ns: u64,
  replayed: u64,
}

fn timed_tail(seg: &mut AnchorSegment) -> Timed {
  let mut replay_ns = Vec::with_capacity(CHECK_RECOVERIES);
  let mut replayed = 0u64;
  for _ in 0..CHECK_RECOVERIES {
    let (recovered, cost_ns) = recovery_cost_ns(|| recover(seg, 0, caps(), 0).unwrap().1);
    assert!(!recovered.torn, "the tail verifies");
    replayed = recovered.replayed_bytes;
    replay_ns.push(cost_ns);
  }
  replay_ns.sort_unstable();
  let quartile = CHECK_RECOVERIES / 4;
  Timed {
    median_ns: replay_ns.get(CHECK_RECOVERIES / 2).copied().unwrap(),
    spread_ns: replay_ns[CHECK_RECOVERIES - 1 - quartile] - replay_ns[quartile],
    replayed,
  }
}

/// A recovery and what it cost, as the calling thread's CPU time: the work the replay did, which a loaded machine does
/// not stretch. Its wall time (`Recovered::replay_ns`) failed the 10 ms budget on Macs at moderate load: 12.2 ms 3 of 3
/// (liveness) and 4 of 4 at load about 44 (live-durability), 2026-11-08. The recovery runs on this thread, whose clock
/// starts and ends it.
#[cfg(unix)]
fn recovery_cost_ns(recovery: impl FnOnce() -> Recovered) -> (Recovered, u64) {
  let before =
    slates_rt::registry::calling_thread_cpu().expect("a Unix thread reads its CPU clock");
  let recovered = recovery();
  let after = slates_rt::registry::calling_thread_cpu().expect("a Unix thread reads its CPU clock");
  let cost_ns = after.spent_ns.saturating_sub(before.spent_ns);
  (recovered, cost_ns)
}

/// [`recovery_cost_ns`] on Windows, by wall time: the thread's CPU time is charged a scheduler tick at a time there
/// (15.6 ms by default, more than the 10 ms budget), so it cannot judge a span this short. CI's Windows runner is not
/// the loaded machine the CPU clock answers.
#[cfg(not(unix))]
fn recovery_cost_ns(recovery: impl FnOnce() -> Recovered) -> (Recovered, u64) {
  let recovered = recovery();
  let cost_ns = recovered.replay_ns;
  (recovered, cost_ns)
}

