//! The daemon's configuration: every number a derivation from the machine profile (D-11),
//! logged with its inputs at start.

use slates_anchor::Geometry;
use slates_db::partition::PartitionCaps;
use std::collections::BTreeMap;

use slates_db::register::{DomainId, HostId, Quorum, scatter_width};
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
/// Shape: the share of a shard's reserve the client regions may take (their pages are
/// populated only as bodies need them, so the share bounds the worst case); ratified in GAPS
/// §5 with the table share.
const CLIENT_SHARE_PERMILLE: u64 = 250;
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: the archive walk's slice as a share of the shard's step budget, parts per thousand: half,
/// so a seal in progress never takes the whole step from the clients (the share a cooperative destroy
/// takes, `verbs::DESTROY_SLICE_PERMILLE`).
const ARCHIVE_SLICE_PERMILLE: u64 = 500;
/// Format: nanoseconds per second, to turn a bytes-per-second throughput into bytes per step.
const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// Shape: tasks a client may need across shards at once: its request forwarded to the owner
/// and the reply carried back (a synchronous client has one request in flight; an
/// asynchronous one is bounded by its ring's credit, which the ring's slots cap).
const TASKS_PER_CLIENT: usize = 2;
/// Shape: the shard's own perpetual tasks: the server loop, the reap loop, the control loop
/// and the heartbeat, plus a spare for a shutdown message.
const LOOP_TASKS_PER_SHARD: usize = 5;
/// Shape: the idle window as a multiple of the spin window (the measured wake cost): a shard
/// keeps polling this long after its last work, so a client that pauses to think between
/// requests finds it awake; ratified in GAPS §5 until the spin-to-park ratio is measured
/// against the histogram's parked form.
pub const IDLE_WINDOW_RATIO: u64 = 100;
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

/// The fleet membership this node is configured to join (§4.8, boot step 6): the fault-tolerance
/// quorum and the peer hosts its neighbourhood is drawn from. `None` on the `DaemonConfig` is the
/// laptop — `f = 0`, solo, one member (itself), the same code path a fleet runs (R8), the cluster plane
/// degenerate. Placement, the configuration group and the owner's acceptor are built over this at boot
/// (`init_shard`). The *transport* the live probe/gossip loop dials each peer over (its address and the
/// operator-provisioned certificate, §4.8 "certificates provisioned by the operator") arrives with that
/// loop; this is the membership the placement authority is built over, which a single-host daemon leaves
/// `None`.
#[derive(Clone, Debug)]
pub struct FleetMembership {
  /// The fault tolerance: a write commits at `f + 1` acknowledgements of `2f + 1` candidate holders
  /// (§4.8). `f = 0` is the solo degenerate (one candidate, the owner) — the same as no fleet at all.
  pub quorum: Quorum,
  /// The peer hosts this node's neighbourhood is drawn from, believed alive at boot; SWIM refines the
  /// live set from here. This host is always a member and is never listed among its own peers.
  pub peers: Vec<HostId>,
  /// This node's own member id — the id its peers know it by, so a recorded holder set names this node
  /// the way every other node names it. A deployed node derives it from its certificate
  /// (`crate::deploy::host_id_of_certificate`, the same derivation its peers apply to the certificate
  /// they pin); a laptop has no fleet membership and takes its machine identity's hash instead
  /// (`init_shard`).
  pub host: HostId,
  /// Each fleet member's failure domain (from the manifest), so placement forms copysets across distinct
  /// domains (D-14). A host absent from the map is its own domain (unique-per-host) — the default when the
  /// deployment declares none. Installed into the configuration group at boot (`init_shard`).
  pub domains: BTreeMap<HostId, DomainId>,
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
  /// Derived: clients per shard, the admission limit (AC-2.6): what the client share of the
  /// reserve holds in regions.
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
  /// The operator's stated per-node **re-replication bandwidth** in bytes/second, the anchor `B` of the
  /// scatter-width derivation (§4.8 "Placement", D-14): how fast this node restores a lost host's copies.
  /// `0` — the default — leaves the scatter width at the candidate floor (one copyset); a deployment states
  /// its link so recovery sizes a wider neighbourhood, until the network-empirical measurement (§4.10a,
  /// deferred: bandwidth "needs a real network to measure") supplies it. See [`DaemonConfig::derived_scatter`].
  pub rereplication_bytes_per_second: u64,
  /// Derived: the bytes one archive-walk slice may hash — half the step budget at the machine's measured
  /// BLAKE3 throughput — so a seal (§4.10) is archived in bounded slices (§4.3) that never take the whole
  /// step from the clients.
  pub archive_slice_bytes: u64,
  /// The fleet this node joins (§4.8, boot step 6), or `None` for the laptop (`f = 0`, solo — the
  /// degenerate of the same code path, R8). A single-host daemon leaves this `None` and every placement
  /// is local; a fleet node names its quorum and peers, and the placement authority, configuration group
  /// and owner acceptor are built over them at boot.
  pub fleet: Option<FleetMembership>,
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
    derivations.push(note("requests_in_flight_per_shard", &admission));
    let mut runtime =
      RuntimeConfig::from_profile(profile, admission.get(), admission.get(), LATENCY_BUDGET_NS);
    let shards = u64::from(runtime.shards.max(1));
    // §4.2 D-12 "honest degradation": the default provisioning reserve is the shard's share of the
    // machine's total RAM, not the OS lock LIMIT. A volume's arena is locked only when a client asks
    // (`require_locked`, verbs.rs); by default it is a normal, usable — if unlocked — mapping (crate
    // mem's lock sequence keeps an unlocked region "usable, unlocked, and counted"). Deriving the
    // reserve from the mlock limit refused every volume where the OS grants little lockable RAM — the
    // design's own failure case, "a locked-down CI container refuses mlock; lock capacity 0" (§4.2 boot
    // order); a `require_locked` volume still respects the lock capacity best-effort. The basis is
    // **total** memory, not free/available: the reserve is a boot-time admission *ceiling* (the arena
    // is a lazy anonymous mapping — it costs no physical RAM until a volume stores bytes, so a virtual
    // ceiling over total is honest), and total is stable, whereas free memory is a fluctuating snapshot
    // that would make the ceiling — and whether the daemon can provision at all — depend on whatever
    // else the machine is doing at boot (racy under a busy host or a parallel test suite).
    let reserve = region_bytes(profile.facts.memory.total, shards, MEMORY_CLASSES);
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
    let archive_slice_bytes: Derived<u64> = derived!(
      (profile
        .hash
        .blake3_bytes_per_second
        .saturating_mul(runtime.step_budget_ns)
        / NANOS_PER_SECOND)
        .saturating_mul(ARCHIVE_SLICE_PERMILLE)
        / PERMILLE,
      "blake3_bytes_per_second × step_budget_ns / 1e9 × ARCHIVE_SLICE_PERMILLE / 1000",
      ["hash.blake3_bytes_per_second", "rt.step_budget_ns"]
    );
    derivations.push(note("archive_slice_bytes", &archive_slice_bytes));
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
      // The green merge chains share the partition's metadata byte budget (§4.16): the same table
      // bytes the records draw from, so a green's persisted chain is bounded by measured memory, not
      // a magic count. Checkpointing to fold old chain entries (the design's optimization) is owed.
      green_chain_bytes: usize::try_from(tables.get()).unwrap_or(usize::MAX),
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
    let region_bytes = u64::try_from(region.total_bytes())
      .unwrap_or(u64::MAX)
      .max(1);
    let clients: Derived<usize> = derived!(
      usize::try_from(
        reserve.get().saturating_mul(CLIENT_SHARE_PERMILLE) / PERMILLE / region_bytes
      )
      .unwrap_or(usize::MAX)
      .max(1),
      "reserve_per_shard × CLIENT_SHARE_PERMILLE / 1000 / region_bytes",
      ["reserve_per_shard", "region_bytes"]
    );
    derivations.push(note("clients_per_shard", &clients));
    // The task arena and the control channel hold the cross-shard traffic of the clients:
    // one task per forwarded request at its owner and one for its reply at the origin, and
    // the shard's own loops; the admission value above sizes only what one client may hold
    // in flight.
    let tasks: Derived<usize> = derived!(
      clients
        .get()
        .saturating_mul(TASKS_PER_CLIENT)
        .saturating_add(LOOP_TASKS_PER_SHARD),
      "clients_per_shard × TASKS_PER_CLIENT + LOOP_TASKS_PER_SHARD",
      ["clients_per_shard"]
    );
    derivations.push(note("tasks_per_shard", &tasks));
    runtime.tasks_per_shard = tasks.get();
    runtime.timers_per_shard = tasks.get();
    // The shard polls for the idle window after its last work before parking (§4.7 "Shards
    // poll rings while any client has activity within the measured idle window"), so a
    // request from an active client, or a forward from another shard, never pays a wake.
    let idle_window: Derived<u64> = derived!(
      d.spin_before_park_ns
        .get()
        .saturating_mul(IDLE_WINDOW_RATIO),
      "spin_before_park_ns × IDLE_WINDOW_RATIO",
      ["wake.p99_ns", "IDLE_WINDOW_RATIO"]
    );
    derivations.push(note("idle_window_ns", &idle_window));
    runtime.spin_ns = idle_window.get();
    DaemonConfig {
      runtime,
      geometry,
      caps,
      region,
      clients_per_shard: clients.get(),
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
      // Unstated by default: the scatter width stays at the candidate floor until a deployment states its
      // re-replication bandwidth (or the deferred network-empirical measurement supplies it).
      rereplication_bytes_per_second: 0,
      archive_slice_bytes: archive_slice_bytes.get(),
      // The laptop default: no fleet, `f = 0`, solo. An operator deploying a fleet sets this (with
      // `with_fleet`); the derivation from the machine profile is the same either way (R8).
      fleet: None,
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

  /// The same configuration with the operator's stated re-replication bandwidth (bytes/second), the `B`
  /// anchor of the scatter-width derivation. `0` leaves the scatter width at the candidate floor.
  pub fn with_rereplication_bandwidth(mut self, bytes_per_second: u64) -> DaemonConfig {
    self.rereplication_bytes_per_second = bytes_per_second;
    self
  }

  /// The scatter width the neighbourhood is bounded to (§4.8 "Placement", D-14), derived from this node's
  /// replicated data budget `D` (its RAM content reserve across shards), its operator-stated re-replication
  /// bandwidth `B` and the recovery budget `T` (`slates_db::replay::RECOVERY_BUDGET_NS`):
  /// `scatter_width(D, B, T, f)` — the smallest S that restores the copies within T, never below the
  /// candidate floor `2f + 1`. With `B` unstated (`0`) this is exactly the floor (one copyset), so the
  /// bounded neighbourhood is unchanged from the default; a stated bandwidth sizes a wider neighbourhood for
  /// recovery parallelism, which the fixed-copyset construction then keeps loss-free.
  pub fn derived_scatter(&self, quorum: Quorum) -> u64 {
    let data_bytes = self
      .reserve_per_shard
      .saturating_mul(u64::from(self.runtime.shards.max(1)));
    scatter_width(
      data_bytes,
      self.rereplication_bytes_per_second,
      slates_db::replay::RECOVERY_BUDGET_NS,
      u64::from(quorum.f),
    )
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

  /// The same configuration joined to a fleet (§4.8, boot step 6): the placement authority, configuration
  /// group and owner acceptor are built over `membership` (its quorum and peers) at boot, rather than the
  /// solo degenerate. An operator sets this to deploy a fleet node; a laptop leaves it unset.
  pub fn with_fleet(mut self, membership: FleetMembership) -> DaemonConfig {
    self.fleet = Some(membership);
    self
  }
}

fn note<T: std::fmt::Debug>(name: &str, d: &Derived<T>) -> String {
  format!(
    "{name} = {:?}: {} (anchors {:?})",
    d.value, d.formula, d.anchors
  )
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use slates_machine::ProfileOptions;

  use super::*;

  /// AC (§4.8 "Placement", D-14): the derived scatter width is the recovery floor — the candidate floor
  /// `2f + 1` when the re-replication bandwidth is unstated, wider when a stated bandwidth is too slow to
  /// restore the data budget within the recovery budget, and back at the floor when it is ample.
  #[test]
  fn the_derived_scatter_is_the_recovery_floor_above_the_candidate_floor() {
    let profile = MachineProfile::measure(ProfileOptions {
      budget_per_probe: Duration::from_millis(1),
      codecs: false,
      core_matrix: false,
    });
    let quorum = Quorum { f: 1 }; // candidate floor 2f + 1 = 3
    let config = DaemonConfig::derive(&profile, "scatter-test").with_shards(1);

    assert_eq!(
      config.derived_scatter(quorum),
      3,
      "an unstated bandwidth leaves the scatter at the candidate floor"
    );
    let slow = config.clone().with_rereplication_bandwidth(1);
    assert!(
      slow.derived_scatter(quorum) > 3,
      "a bandwidth too slow to restore the data budget within the recovery budget widens the scatter"
    );
    let ample = config.with_rereplication_bandwidth(u64::MAX);
    assert_eq!(
      ample.derived_scatter(quorum),
      3,
      "a bandwidth that restores the data budget within the budget leaves the scatter at the floor"
    );
  }
}
