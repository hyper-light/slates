//! The daemon's configuration: every number a derivation from the machine profile (D-11),
//! logged with its inputs at start.

use slates_anchor::Geometry;
use slates_db::partition::PartitionCaps;
use slates_ipc::RegionGeometry;
use slates_machine::{Derived, MachineProfile, derived};
use slates_mem::budget::region_bytes;
use slates_rt::RuntimeConfig;
use slates_rt::runtime::admission_limit;

/// Shape: the small-directory cut-over measured in Phase 1 (A-7: two entries inline).
const DIR_CUTOVER: usize = 2;
/// Shape: the latency budget the batch bound is calibrated against, the provisioning target
/// of R9 (50 µs).
const LATENCY_BUDGET_NS: u64 = 50_000;
/// Shape: the request rate the first admission limit assumes before a client is measured:
/// one client saturating the ring floor (Phase 2 baseline, IPC: 278 ns per round trip, so
/// under four million per second); re-derived from measured rates at the first status.
const ASSUMED_REQUESTS_PER_SECOND: u64 = 4_000_000;
/// Shape: the service-time p99 the first admission limit assumes: the database mutation
/// baseline plus the ring (Phase 2 baseline: 206 + 278 ns) with an order of magnitude of
/// headroom for the volume core's create.
const ASSUMED_SERVICE_P99_NS: u64 = 5_000;
/// Shape: the memory classes the shard reserve is split into (§4.2: metadata slabs, rings,
/// chunk regions).
const MEMORY_CLASSES: u64 = 3;
/// Shape: the share of a shard's metadata reserve the volume tables may take (the rest is the
/// store's own slabs); ratified in GAPS §5.
const TABLE_SHARE_PERMILLE: u64 = 250;
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: the divisor between a shard's reserve and one table's slots: the reserve is split
/// among the inode table, the directory table and content (three classes, §4.2), and each
/// table keeps half of its class for growth headroom.
const STORE_TABLE_DIVISOR: u64 = 6;
/// Shape: the largest inline lifecycle request: a name and a base path, both under the OS
/// path limit (Linux `PATH_MAX` 4096); one page each direction per slot.
const BULK_CHUNK_BYTES: u64 = 4096;
/// Shape: the landing slots the segment holds: landings in flight per daemon, one per shard
/// plus one for the control shard's presentation.
const LANDING_SLOTS_PER_SHARD: u32 = 1;
/// Shape: the audit ring: a page per shard; the audit rate is measured from the first landings
/// (§4.15's derived table) and the ring re-sized at the next start.
const AUDIT_PAGES_PER_SHARD: u64 = 1;

/// The store's caps per shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreCaps {
  /// Inode versions the shard may hold.
  pub max_inodes: usize,
  /// Directory nodes.
  pub max_dirs: usize,
  /// Chunks.
  pub max_chunks: usize,
  /// Directory blocks.
  pub max_dir_blocks: usize,
  /// The small-directory cut-over.
  pub dir_cutover: usize,
}

/// The daemon's configuration.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
  /// The runtime's configuration.
  pub runtime: RuntimeConfig,
  /// The segment's geometry.
  pub geometry: Geometry,
  /// The partition caps.
  pub caps: PartitionCaps,
  /// The client region's geometry.
  pub region: RegionGeometry,
  /// Derived: clients per shard, the admission limit.
  pub clients_per_shard: usize,
  /// Derived: the shard's reserve in bytes.
  pub reserve_per_shard: u64,
  /// Derived: the large-class boundary of overlay copy-ups (one arena region).
  pub large_class_bytes: u64,
  /// Derived: the store's caps per shard (inodes, directories, chunks, directory blocks).
  pub store: StoreCaps,
  /// Whether huge pages help here (measured).
  pub huge_pages: bool,
  /// The cache line in bytes.
  pub cache_line: usize,
  /// The base page in bytes.
  pub page: usize,
  /// The rendezvous instance name.
  pub instance: String,
  /// The operator's failover SLO, the lease term's ceiling (§4.4 "Derived constants", D-16).
  pub failover_slo_ns: u64,
  /// Every derivation, for the boot log.
  pub derivations: Vec<String>,
}

/// Shape: the operator's failover SLO (Gray & Cheriton: seconds): ten seconds until the
/// operator gives `slates anchor` a value.
pub const FAILOVER_SLO_NS: u64 = 10_000_000_000;

impl DaemonConfig {
  /// The configuration from a profile, for `instance`.
  pub fn derive(profile: &MachineProfile, instance: &str) -> DaemonConfig {
    let mut derivations = Vec::new();
    let d = profile.derived();
    let admission = admission_limit(ASSUMED_REQUESTS_PER_SECOND, ASSUMED_SERVICE_P99_NS);
    derivations.push(note("clients_per_shard", &admission));
    let runtime =
      RuntimeConfig::from_profile(profile, admission.get(), admission.get(), LATENCY_BUDGET_NS);
    let shards = u64::from(runtime.shards.max(1));
    let reserve = region_bytes(profile.lock.bytes, shards, MEMORY_CLASSES);
    derivations.push(note("reserve_per_shard", &reserve));
    let page = profile.facts.page.base;
    let tables: Derived<u64> = derived!(
      reserve.get().saturating_mul(TABLE_SHARE_PERMILLE) / PERMILLE,
      "reserve_per_shard × TABLE_SHARE_PERMILLE / 1000",
      ["mem.lock_capacity", "GAPS §5 table share"]
    );
    derivations.push(note("table_bytes", &tables));
    let volume_record_bytes =
      u64::try_from(size_of::<slates_db::catalog::VolumeRecord>()).unwrap_or(1);
    let volumes: Derived<usize> = derived!(
      usize::try_from(tables.get() / volume_record_bytes.max(1))
        .unwrap_or(usize::MAX)
        .max(1),
      "table_bytes / size_of::<VolumeRecord>()",
      ["table_bytes"]
    );
    derivations.push(note("volumes_per_shard", &volumes));
    let slots: Derived<u32> = derived!(
      u32::try_from(d.ring_entries.get())
        .unwrap_or(u32::MAX)
        .next_power_of_two(),
      "the runtime's ring entries (Little's law), rounded up to a power of two",
      ["rt.ring_entries"]
    );
    derivations.push(note("slots_per_ring", &slots));
    let spin: Derived<u32> = derived!(
      u32::try_from(d.spin_before_park_ns.get()).unwrap_or(u32::MAX),
      "the measured wake cost p99 (the 2-competitive spin)",
      ["wake.p99_ns"]
    );
    derivations.push(note("spin_ns", &spin));
    let log_bytes: Derived<u64> = derived!(
      slates_db::replay::RECOVERY_BUDGET_NS / 1_000 * page.max(1),
      "recovery_budget_us × one page per microsecond of replay until the first replay measures",
      ["recovery_budget_ns", "page"]
    );
    derivations.push(note("log_bytes_per_partition", &log_bytes));
    let snapshot_bytes: Derived<u64> = derived!(
      tables.get().saturating_mul(2),
      "twice the table bytes (the whole partition encoded, with growth headroom)",
      ["table_bytes"]
    );
    derivations.push(note("snapshot_bytes_per_partition", &snapshot_bytes));
    let geometry = Geometry {
      partitions: runtime.shards.max(1),
      page,
      profile_bytes: u64::try_from(profile.to_json().map(|j| j.len()).unwrap_or(0))
        .unwrap_or(0)
        .saturating_mul(2)
        .max(page),
      log_bytes: log_bytes.get(),
      snapshot_bytes: snapshot_bytes.get(),
      audit_bytes: AUDIT_PAGES_PER_SHARD
        .saturating_mul(page)
        .saturating_mul(shards),
      landing_slots: LANDING_SLOTS_PER_SHARD
        .saturating_mul(runtime.shards.max(1).into())
        .saturating_add(1),
      landing_slot_bytes: page,
    };
    let caps = PartitionCaps {
      volumes: volumes.get(),
      snapshots: volumes.get(),
      attachments: admission.get(),
      segment_slots: usize::try_from(page / volume_record_bytes.max(1))
        .unwrap_or(1)
        .max(1),
      timers: volumes.get(),
      tick_ns: runtime.timer_tick_ns,
    };
    // The store's tables: the metadata share of the reserve over each record's size, the
    // inodes and directories sharing it, chunks over the arena share.
    let inode_bytes = u64::try_from(size_of::<slates_vfs::inode::Inode>()).unwrap_or(1);
    let dir_bytes = u64::try_from(size_of::<slates_vfs::dir::DirNode>()).unwrap_or(1);
    let max_inodes: Derived<usize> = derived!(
      usize::try_from(reserve.get() / inode_bytes.max(1) / STORE_TABLE_DIVISOR)
        .unwrap_or(usize::MAX)
        .max(1),
      "reserve_per_shard / size_of::<Inode>() / STORE_TABLE_DIVISOR",
      ["reserve_per_shard"]
    );
    derivations.push(note("max_inodes", &max_inodes));
    let max_dirs: Derived<usize> = derived!(
      usize::try_from(reserve.get() / dir_bytes.max(1) / STORE_TABLE_DIVISOR)
        .unwrap_or(usize::MAX)
        .max(1),
      "reserve_per_shard / size_of::<DirNode>() / STORE_TABLE_DIVISOR",
      ["reserve_per_shard"]
    );
    derivations.push(note("max_dirs", &max_dirs));
    let max_chunks: Derived<usize> = derived!(
      usize::try_from(reserve.get() / page.max(1))
        .unwrap_or(usize::MAX)
        .max(1),
      "reserve_per_shard / page (one chunk per page at least)",
      ["reserve_per_shard", "page"]
    );
    derivations.push(note("max_chunks", &max_chunks));
    let store = StoreCaps {
      max_inodes: max_inodes.get(),
      max_dirs: max_dirs.get(),
      max_chunks: max_chunks.get(),
      max_dir_blocks: max_dirs.get(),
      dir_cutover: DIR_CUTOVER,
    };
    let region = RegionGeometry {
      slots: slots.get(),
      spin_ns: spin.get(),
      bulk_bytes: u64::from(slots.get())
        .saturating_mul(2)
        .saturating_mul(BULK_CHUNK_BYTES),
      page,
    };
    DaemonConfig {
      runtime,
      geometry,
      caps,
      region,
      clients_per_shard: admission.get(),
      reserve_per_shard: reserve.get(),
      large_class_bytes: d.arena_region_bytes.get(),
      store,
      huge_pages: slates_mem::region::huge_pages_beneficial(profile).get(),
      cache_line: usize::try_from(profile.facts.cache_line)
        .unwrap_or(1)
        .max(1),
      page: usize::try_from(page).unwrap_or(1).max(1),
      instance: instance.to_owned(),
      failover_slo_ns: FAILOVER_SLO_NS,
      derivations,
    }
  }
}

impl DaemonConfig {
  /// The same configuration with the operator's failover SLO (the lease term's ceiling).
  pub fn with_failover_slo(mut self, failover_slo_ns: u64) -> DaemonConfig {
    self.failover_slo_ns = failover_slo_ns.max(1);
    self
  }

  /// The same configuration over `shards` shards, unpinned (tests and benches that share a
  /// machine with other daemons); the segment's partitions follow.
  pub fn with_shards(mut self, shards: u16) -> DaemonConfig {
    self.runtime.shards = shards.max(1);
    self.runtime.pin = false;
    self.runtime.cores = Vec::new();
    self.geometry.partitions = shards.max(1);
    self
  }
}

fn note<T: std::fmt::Debug>(name: &str, d: &Derived<T>) -> String {
  format!(
    "{name} = {:?}: {} (anchors {:?})",
    d.value, d.formula, d.anchors
  )
}
