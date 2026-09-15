//! The daemon's configuration: every number a derivation from the machine profile (D-11),
//! logged with its inputs at start.

use slates_anchor::Geometry;
use slates_db::partition::PartitionCaps;
use std::collections::BTreeMap;

use slates_db::register::{Configuration, DomainId, HostId, Quorum, RegionId, scatter_width};
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
/// Shape: the fleet's perpetual tasks per peer on the control shard: the probe loop and the record
/// link (`fleet::probe_peer`, `fleet::establish_record_link`).
const FLEET_LOOPS_PER_PEER: usize = 2;
/// Shape: the fleet's planes, each with its own socket, demultiplexer and accept loop: probes and
/// records (§4.8).
const FLEET_PLANES: usize = 2;
/// Shape: the fleet's perpetual tasks per shard beyond the per-peer ones: one demultiplexer receive
/// loop and one accept loop per plane (`fleet::run_demux`, `fleet::accept_probes`,
/// `fleet::accept_records`), plus the one coordinator (`fleet::run_record_plane`, which also drives
/// the configuration council and the root group in its period).
const FLEET_LOOPS_PER_SHARD: usize = FLEET_PLANES * 2 + 1;
/// Shape: the idle window as a multiple of the spin window (the measured wake cost): a shard
/// keeps polling this long after its last work, so a client that pauses to think between
/// requests finds it awake; ratified in GAPS §5 until the spin-to-park ratio is measured
/// against the histogram's parked form.
pub const IDLE_WINDOW_RATIO: u64 = 100;
/// The divisor between a shard's metadata class and one slab dimension's units. The class is split
/// among the inode dimension (a version slot and a table slot per inode), the directory dimension
/// (a node and a block per directory) and the volume records (journals and objects) — three parts
/// — and each slab dimension keeps half of its part for growth headroom, so the slabs' maximum
/// footprints sum to about a third of the class and the records' ledger holds the rest (§4.2 "the
/// prepared arena layout"; `Store::set_metadata_class`).
/// Shape: three parts, each slab part halved for headroom.
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
  /// Derived: the shard's metadata class in bytes (§4.2), the second of the three memory classes:
  /// the slabs above grow into it up to their bounds, and the remainder is the ledger every
  /// volume's records are reserved from (`Store::set_metadata_class`).
  pub metadata_class_bytes: u64,
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
  /// This node's manifest routing placeholder, `member_id(origin_anchor, 0)`. The daemon
  /// replaces it with its fresh per-start member id; the placeholder grants no voting rights.
  pub host: HostId,
  /// This node's **stable cert-anchor** — `deploy::host_id_of_certificate` of its own certificate (a laptop
  /// uses its machine-identity hash). Unchanged across restarts, unlike the ephemeral member id. The daemon
  /// derives its runtime member id `member_id(origin_anchor, generation)` from it, and keys a client's
  /// completion record on it so a retry meets its record after a restart (§4.8; task #22 two-id model).
  pub origin_anchor: HostId,
  /// Each fleet member's failure domain (from the manifest), so placement forms copysets across distinct
  /// domains (D-14). A host absent from the map is its own domain (unique-per-host) — the default when the
  /// deployment declares none. Installed into the configuration group at boot (`init_shard`).
  pub domains: BTreeMap<HostId, DomainId>,
  /// Each fleet member's **region** (§4.8, D-14 — "a root group across regions holds region membership"), so
  /// the root group agrees on which regions exist and the regional council scopes its membership to the hosts
  /// of one region. A host absent from the map is in the **sole region** [`RegionId(0)`] — the default when
  /// the deployment declares none, which collapses to a single-region fleet whose root group is the degenerate
  /// self-leading group (R8). This node's own region is `regions[host]` (or the sole region if absent).
  pub regions: BTreeMap<HostId, RegionId>,
  /// Each region's designated **mirror** region (§4.8 "region loss promotes the mirror through the root
  /// group"): where a region's data is asynchronously copied, and the region an operator promotes it to on
  /// loss. A region absent from the map has no mirror. Read by the root group's reconcile (a lost region with
  /// a mirror is left for a deliberate operator promotion, not auto-retired) and by the operator promotion
  /// (`Daemon::promote_region` looks up the lost region's mirror here). Fleet-wide, from the manifest.
  pub region_mirrors: BTreeMap<RegionId, RegionId>,
  /// The operator's durability policy, if declared (§4.8 "the copyset count check at every configuration
  /// change"): the accepted coincident-loss probability under a stated simultaneous failure count. `None` —
  /// the default — leaves the check disabled (an accepted loss probability is a policy, not a machine
  /// measurement, so it is never derived — it is the operator's). When set, every configuration change is
  /// checked against it and a breach is **surfaced** (a counted health signal), never silently over-scattered:
  /// a breach is a recovery-vs-durability conflict for the operator to resolve (more copies, more bandwidth,
  /// tighter failure domains).
  pub durability: Option<DurabilityBound>,
}

/// An operator durability policy (§4.8, D-14; research §3.1): the accepted coincident-loss probability
/// `accepted_loss` under a coincident failure of `coincident_failures` hosts. It bounds the copyset count a
/// configuration may reach — a wider scatter spreads objects over more copysets, raising the chance some
/// copyset lies entirely within a coincident failure. The operator states both (the accepted risk and the
/// failure scale it is accepted under); neither is derived, because an accepted loss probability is a policy,
/// not a machine measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DurabilityBound {
  /// The largest coincident-loss probability the operator accepts (`0.0` accepts none; `1.0` accepts any —
  /// the disabled bound).
  pub accepted_loss: f64,
  /// The number of hosts assumed to fail at once when evaluating the loss (the failure scale the policy is
  /// stated under — e.g. a power-domain outage of some fraction of the region).
  pub coincident_failures: u64,
}

impl DurabilityBound {
  /// Whether `configuration` **breaches** this bound — its coincident-loss probability under the policy's
  /// failure count exceeds the accepted loss. A breach is surfaced at the configuration change, never
  /// silently accepted (§4.8): the resolution is the operator's (raise `f`, the re-replication bandwidth, or
  /// the failure-domain granularity), not a quiet over-scatter.
  pub fn breached_by(&self, configuration: &Configuration) -> bool {
    self.shortfall(configuration).is_some()
  }

  /// The **measured shortfall** of `configuration` against this bound — the design's `within_loss_bound(ε, F)`
  /// check (§4.8 "Placement"), answered with its numbers: `None` when the configuration's coincident-loss
  /// probability under the policy's failure count is within the accepted ε, otherwise the probability the
  /// configuration actually carries beside the ε it was held to and the failure count it was measured under.
  /// This is the durability policy "that gates a refusal": the daemon measures it once at every configuration
  /// change (a cold path — `Configuration::coincident_loss` is a few floating-point products) and keeps the
  /// result on the shard, where the verbs read it per write as a field, never recomputing it.
  pub fn shortfall(&self, configuration: &Configuration) -> Option<DurabilityShortfall> {
    let coincident_loss = configuration.coincident_loss(self.coincident_failures);
    if coincident_loss <= self.accepted_loss {
      return None;
    }
    Some(DurabilityShortfall {
      coincident_loss,
      accepted_loss: self.accepted_loss,
      coincident_failures: self.coincident_failures,
    })
  }
}

/// A committed configuration's durability **shortfall** against the operator's policy (§4.8 "Placement"):
/// the coincident-loss probability it carries under the policy's failure count, above the accepted ε. The
/// fact a write is refused with ([`Refusal::DurabilityUnmet`](slates_ipc::protocol::Refusal)) — measured at
/// the configuration change, read per write.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DurabilityShortfall {
  /// The configuration's measured coincident-loss probability under `coincident_failures` failures.
  pub coincident_loss: f64,
  /// The loss probability the policy accepts (its ε).
  pub accepted_loss: f64,
  /// The number of hosts the policy assumes fail at once.
  pub coincident_failures: u64,
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
  /// Derived: the compress-or-not cost model each sealed chunk is stored under (§4.11, D-17): the boot
  /// profile's measured codec points (LZ4 and each zstd level's compress and decompress throughput and
  /// ratio over the probe corpus) as the policy's candidates; a byte's neutral worth = its measured
  /// memcpy cost; `E[reads] = 1` (a seal's archive is the replication copy, read about once per
  /// re-attach or takeover: §4.11 "about one per re-attach for archived"); `value_of_byte` and
  /// `value_of_cpu` at neutral. At neutral the model stores a chunk read once **raw** — moving a byte is
  /// far cheaper than compressing it, the design's "hot volumes stay raw unless pressure raises
  /// `value_of_byte`" — so what compresses is decided by the two value signals, both owed as
  /// derivations, not literals: the archive class's `value_of_byte` (a placed byte occupies `f + 1`
  /// holders' RAM for the snapshot's retention, so its worth is that retention over one memcpy time,
  /// per copy — "archived volumes compress once") and the live memory-pressure and load signals. Raw-only
  /// when the profile measured no codec.
  pub codec: slates_archive::CodecPolicy,
  /// Derived: the spans one `Telemetry` reply carries (§4.14; `crate::telemetry::spans_per_reply`): what
  /// one bulk chunk of a client's region holds past the report's fixed part, measured from the wire
  /// encoding at boot.
  pub telemetry_spans_per_reply: usize,
  /// The fleet this node joins (§4.8, boot step 6), or `None` for the laptop (`f = 0`, solo — the
  /// degenerate of the same code path, R8). A single-host daemon leaves this `None` and every placement
  /// is local; a fleet node names its quorum and peers, and the placement authority, configuration group
  /// and owner acceptor are built over them at boot.
  pub fleet: Option<FleetMembership>,
  /// Derived: a guest device attachment's credits (§4.6 A-9, §4.9): the request credit is the shard's
  /// admission limit (`requests_in_flight_per_shard`), the byte credit the §4.9 window over the measured
  /// memcpy bandwidth and the wake p99 as the kick round trip, with one request's worst case as the frame.
  /// Unix only, as the guest transport is.
  #[cfg(unix)]
  pub guest_credits: slates_bridge_virtiofs::credit::AttachmentCredits,
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
    // else the machine is doing at boot (racy under a busy host or a parallel test suite). Total is
    // clamped to the tightest OS/job/cgroup bound on the process when one is set (§4.2 "effective
    // capacity"): a container's `memory.max` or a finite `RLIMIT_AS` is a hard limit the OS enforces
    // by killing or refusing, so admitting past it would be a promise the host cannot keep; like
    // total, a configured bound is stable across the boot.
    let effective: Derived<u64> = derived!(
      slates_machine::facts::effective_capacity(
        profile.facts.memory.total,
        profile.facts.memory.limit
      ),
      "min(memory.total, the OS/job/cgroup bound when one is set)",
      ["memory.total", "memory.limit"]
    );
    derivations.push(note("effective_capacity_bytes", &effective));
    let reserve = region_bytes(effective.get(), shards, MEMORY_CLASSES);
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
    let codec = codec_policy(profile);
    derivations.push(format!(
      "codec = lz4 {}, zstd levels {:?}, byte worth {} scaled ns: the profile's measured codec points, a \
       byte worth its memcpy cost at neutral value_of_byte and value_of_cpu, E[reads] = 1 (anchors \
       [\"codecs\", \"memcpy.bytes_per_second\"])",
      codec.lz4.is_some(),
      codec.zstd.iter().map(|rate| rate.level).collect::<Vec<_>>(),
      codec.byte_ns_scaled
    ));
    // A reply rides one bulk chunk (one page each direction per slot), so a telemetry drain carries
    // what a chunk holds past the report's fixed part (§4.14 bounded export).
    let telemetry_spans_per_reply =
      crate::telemetry::spans_per_reply(usize::try_from(BULK_CHUNK_BYTES).unwrap_or(usize::MAX));
    derivations.push(note(
      "telemetry_spans_per_reply",
      &telemetry_spans_per_reply,
    ));
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
    // The store's tables (§4.2 metadata dimension): each slab's bound is the metadata class — the
    // shard's second memory class, one reserve — over the true slot cost of the dimension's unit
    // (an inode is a version slot and a table slot; a directory a node and a block), so the slabs'
    // maximum footprints are inside the class by construction and the remainder is the ledger the
    // volumes' records are reserved from. Chunk records go one per page of the arena. The
    // directory-block bound is the directory bound: every directory past the cut-over holds at
    // least one block, and a large directory's extra blocks come out of the same unit count.
    let inode_unit = u64::try_from(slates_vfs::volume::inode_unit_bytes()).unwrap_or(1);
    let dir_unit = u64::try_from(slates_vfs::volume::directory_unit_bytes()).unwrap_or(1);
    let max_inodes: Derived<usize> = derived!(
      usize::try_from(reserve.get() / inode_unit.max(1) / STORE_TABLE_DIVISOR)
        .unwrap_or(usize::MAX)
        .max(1),
      "metadata_class / (Slot<Inode> + Slot<TrieNode>) / STORE_TABLE_DIVISOR",
      ["reserve_per_shard", "vfs.inode_unit_bytes"]
    );
    derivations.push(note("max_inodes", &max_inodes));
    let max_dirs: Derived<usize> = derived!(
      usize::try_from(reserve.get() / dir_unit.max(1) / STORE_TABLE_DIVISOR)
        .unwrap_or(usize::MAX)
        .max(1),
      "metadata_class / (Slot<DirNode> + Slot<DirBlock>) / STORE_TABLE_DIVISOR",
      ["reserve_per_shard", "vfs.directory_unit_bytes"]
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
    let metadata_class: Derived<u64> = derived!(
      reserve.get(),
      "the shard's metadata class: one of the MEMORY_CLASSES shares of its RAM",
      ["reserve_per_shard", "MEMORY_CLASSES"]
    );
    derivations.push(note("metadata_class_bytes", &metadata_class));
    // The clean-file digest cache's bound (§4.15), derived by the store from the inode table it sizes;
    // computed here too so the boot log carries every derived value with its inputs (R3).
    derivations.push(note(
      "digest_cache_entries",
      &slates_vfs::base::digest_capacity(max_inodes.get()),
    ));
    let store = StoreCaps {
      max_inodes: max_inodes.get(),
      max_dirs: max_dirs.get(),
      max_chunks: max_chunks.get(),
      max_dir_blocks: max_dirs.get(),
      dir_cutover: DIR_CUTOVER,
      metadata_class_bytes: metadata_class.get(),
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
    #[cfg(unix)]
    let guest_credits = guest_credits(profile, admission.get(), &mut derivations);
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
      codec,
      telemetry_spans_per_reply: telemetry_spans_per_reply.get(),
      // The laptop default: no fleet, `f = 0`, solo. An operator deploying a fleet sets this (with
      // `with_fleet`); the derivation from the machine profile is the same either way (R8).
      fleet: None,
      #[cfg(unix)]
      guest_credits,
      derivations,
    }
  }
}

/// A guest device attachment's credits from the profile (§4.6 A-9, §4.9): the request credit is the
/// shard's admission limit; the byte credit is the credit window over the measured memcpy bandwidth (the
/// largest measured copy size, the streaming rate a request copy runs at) and the wake p99 (the floor of
/// a kick's round trip through the driver), with one request's worst case — the device's readable cap
/// plus its writable cap — as the frame, under the provisioning latency budget.
#[cfg(unix)]
fn guest_credits(
  profile: &MachineProfile,
  requests_in_flight_per_shard: usize,
  derivations: &mut Vec<String>,
) -> slates_bridge_virtiofs::credit::AttachmentCredits {
  use slates_bridge_virtiofs::credit::AttachmentCredits;
  use slates_bridge_virtiofs::device::{readable_cap, writable_cap};
  let bandwidth_bytes_per_ns: Derived<u64> = derived!(
    profile
      .memcpy
      .iter()
      .max_by_key(|point| point.bytes)
      .map_or(1, |point| point.bytes_per_second / NANOS_PER_SECOND)
      .max(1),
    "memcpy bytes per second at the largest measured copy size / 1e9 (at least one byte per nanosecond)",
    ["memcpy"]
  );
  derivations.push(note(
    "guest_bandwidth_bytes_per_ns",
    &bandwidth_bytes_per_ns,
  ));
  let frame = readable_cap().get().saturating_add(writable_cap().get());
  let credits = AttachmentCredits::derive(
    requests_in_flight_per_shard,
    bandwidth_bytes_per_ns.get(),
    profile.wake.p99_ns.max(1),
    frame,
    LATENCY_BUDGET_NS,
  );
  derivations.push(note("guest_requests_credit", &credits.requests));
  derivations.push(note("guest_bytes_credit", &credits.bytes));
  credits
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
    // The fleet's own tasks are a second population on the control shard beside the clients' (§4.3
    // "every structure has a derived bound"): per peer, the probe loop and the record link, and the
    // accept-side serve tasks — one per session the demultiplexer may hold for that peer on each
    // plane (`fleet::SESSIONS_PER_PEER` × 2 planes: the live session and the one a re-dial is
    // replacing; the demultiplexer holds a session's slot until its serve task drops it, so no more
    // can exist) — plus the receive and accept loops of both planes and the coordinator. Sized here,
    // once, from the peer count, so a burst of re-dials under
    // load fills the fleet's share and never the clients' (2026-09-14: at ~3× oversubscription the
    // shard's whole arena filled with accept-side handshakes each held for its bounded retransmit
    // budget, `adm_refused` 4,554 — `docs/wip/fleet-under-load.md`).
    let peers = membership.peers.len();
    let fleet_tasks: Derived<usize> = derived!(
      peers
        .saturating_mul(FLEET_LOOPS_PER_PEER)
        .saturating_add(peers.saturating_mul(crate::fleet::SESSIONS_PER_PEER * FLEET_PLANES))
        .saturating_add(FLEET_LOOPS_PER_SHARD),
      "peers × FLEET_LOOPS_PER_PEER + peers × SESSIONS_PER_PEER × FLEET_PLANES + FLEET_LOOPS_PER_SHARD",
      ["fleet.peers"]
    );
    self
      .derivations
      .push(note("fleet_tasks_per_shard", &fleet_tasks));
    self.runtime.tasks_per_shard = self
      .runtime
      .tasks_per_shard
      .saturating_add(fleet_tasks.get());
    self.runtime.timers_per_shard = self
      .runtime
      .timers_per_shard
      .saturating_add(fleet_tasks.get());
    self.fleet = Some(membership);
    self
  }
}

/// The cost model's policy from the profile's measured codec points (§4.11, D-17): the `lz4` point
/// (the probe codec) and every `zstd` level point become the candidates, each with its measured
/// compress and decompress cost and ratio; the value constants are neutral and `E[reads]` is one (see
/// [`DaemonConfig::codec`]). A profile that measured no codec (the probe off, or the codecs not
/// compiled in) yields the raw-only policy — the same decision path with no candidates (R8).
fn codec_policy(profile: &MachineProfile) -> slates_archive::CodecPolicy {
  let mut policy = slates_archive::CodecPolicy::raw_only();
  // The neutral worth of a byte: the measured memcpy bandwidth at the largest probed size (the
  // streaming rate a chunk moves at), so a codec pays when the bytes it saves would cost more to move
  // than to compress and decompress.
  let memcpy = profile
    .memcpy
    .iter()
    .max_by_key(|point| point.bytes)
    .map_or(0, |point| point.bytes_per_second);
  policy.byte_ns_scaled = slates_archive::CodecPolicy::byte_worth_from_memcpy(memcpy);
  for point in &profile.codecs {
    let rate = slates_archive::CodecRate::from_throughput(
      point.level,
      point.compress_bytes_per_second,
      point.decompress_bytes_per_second,
      point.ratio_permille,
    );
    match point.codec.as_str() {
      "lz4" => policy.lz4 = Some(rate),
      "zstd" => policy.zstd.push(rate),
      _ => {}
    }
  }
  policy
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

  /// AC (§4.8 "the copyset count check at every configuration change", D-14): an operator durability bound is
  /// breached when a configuration's coincident-loss probability exceeds the accepted loss — a policy that
  /// accepts **no** loss is breached by any redundant configuration (its loss is positive), while a policy
  /// that accepts **any** loss is never breached. The check reads the existing `within_loss_bound`.
  #[test]
  fn a_durability_bound_surfaces_a_breach() {
    use slates_db::register::RegionalConfiguration;
    let owner = HostId(1);
    let regional = RegionalConfiguration::formed(
      vec![owner, HostId(2), HostId(3)],
      Quorum { f: 1 },
      BTreeMap::new(),
      3,
      false,
    );
    let configuration = regional
      .configuration_for(owner)
      .expect("the owner has a placement view");

    // Accept no loss: any redundant (f = 1) configuration has a positive coincident-loss probability, so a
    // coincident failure of the two copies breaches the zero-loss policy.
    let strict = DurabilityBound {
      accepted_loss: 0.0,
      coincident_failures: 2,
    };
    assert!(
      strict.breached_by(&configuration),
      "a zero-loss policy is breached by any redundant configuration"
    );

    // Accept any loss: never breached.
    let lax = DurabilityBound {
      accepted_loss: 1.0,
      coincident_failures: 2,
    };
    assert!(
      !lax.breached_by(&configuration),
      "a policy that accepts any loss is never breached"
    );
  }

  /// AC (§4.8 "Placement" — the `within_loss_bound(ε, F)` check answered with its numbers): a breach reports
  /// its **measured shortfall** — the configuration's coincident-loss probability under the policy's failure
  /// count, beside the ε it was held to and that count — so a refusal can carry the fact, not a flag. A
  /// policy stated under a failure count no copyset can fall wholly inside (one host, at `f = 1`) has no
  /// shortfall even at zero accepted loss; and a single-copy laptop configuration is never short (R8).
  #[test]
  fn a_breach_reports_its_measured_shortfall() {
    use slates_db::register::RegionalConfiguration;
    let owner = HostId(1);
    let configuration = RegionalConfiguration::formed(
      vec![owner, HostId(2), HostId(3)],
      Quorum { f: 1 },
      BTreeMap::new(),
      3,
      false,
    )
    .configuration_for(owner)
    .expect("the owner has a placement view");

    let strict = DurabilityBound {
      accepted_loss: 0.0,
      coincident_failures: 2,
    };
    let shortfall = strict
      .shortfall(&configuration)
      .expect("two coincident failures can lose an f = 1 copyset");
    assert_eq!(
      shortfall.coincident_loss,
      configuration.coincident_loss(2),
      "the shortfall carries the configuration's own measured loss"
    );
    assert!(
      shortfall.coincident_loss > shortfall.accepted_loss,
      "a shortfall is a loss above the accepted ε"
    );
    assert_eq!(shortfall.accepted_loss, 0.0);
    assert_eq!(shortfall.coincident_failures, 2);

    let one_failure = DurabilityBound {
      accepted_loss: 0.0,
      coincident_failures: 1,
    };
    assert_eq!(
      one_failure.shortfall(&configuration),
      None,
      "one failing host cannot hold both copies of an f = 1 copyset: no shortfall even at zero accepted loss"
    );

    let laptop =
      RegionalConfiguration::formed(vec![owner], Quorum { f: 0 }, BTreeMap::new(), 1, false)
        .configuration_for(owner)
        .expect("the solo owner has a placement view");
    assert_eq!(
      strict.shortfall(&laptop),
      None,
      "a single copy has no coincident loss: the laptop is never short under any policy (R8)"
    );
  }

  /// AC-0.10 (§4.2 metadata dimension): the derived slab bounds lay the metadata class out so that
  /// every slab at its bound fits inside the class with room left for volume records — a store built
  /// on the derived caps accepts its class and offers a positive records ledger. Non-vacuous: with
  /// the directory-block bound tied to the directory count over the node's size alone, the blocks
  /// alone were several times the class and the class was refused.
  #[test]
  fn the_derived_slab_bounds_lay_the_metadata_class_out_with_room_for_records() {
    use slates_mem::arena::ChunkArena;
    use slates_mem::region::Region;
    use slates_vfs::volume::{Store, StoreConfig};
    let profile = MachineProfile::measure(ProfileOptions {
      budget_per_probe: Duration::from_millis(1),
      codecs: false,
      core_matrix: false,
    });
    let config = DaemonConfig::derive(&profile, "metadata-layout").with_shards(1);
    let page = config.page;
    let mut arena = ChunkArena::new(page);
    arena
      .add_region(Region::map(page, page, false).expect("one page maps"))
      .expect("one region");
    let mut store = Store::new(
      &StoreConfig {
        page,
        cache_line: config.cache_line,
        max_dirs: config.store.max_dirs,
        max_inodes: config.store.max_inodes,
        max_chunks: config.store.max_chunks,
        max_dir_blocks: config.store.max_dir_blocks,
        dir_cutover: config.store.dir_cutover,
      },
      arena,
      0,
    );
    let slabs = store.slab_footprint_bytes();
    let class = config.store.metadata_class_bytes;
    let records = store
      .set_metadata_class(class)
      .expect("the derived layout fits its class");
    assert_eq!(
      slabs + records,
      class,
      "the class is the slabs plus the records"
    );
    assert!(
      records >= class / 2,
      "at least half the class is left for records ({records} of {class}; slabs {slabs})"
    );
  }

  /// §4.3 "every structure has a derived bound" for the fleet's own tasks: a fleet configuration
  /// carries, per peer, the probe loop, the record link and one serve task per session the
  /// demultiplexer may hold on each plane, plus the plane loops — derived from the peer count and
  /// added to the shard's task budget, so a burst of peer re-dials fills the fleet's share and never
  /// a client's. Do: derive with and without a five-peer fleet. Expect: the fleet form's task and
  /// timer budgets exceed the solo form's by exactly the derived share, and the boot log names it
  /// with its input. Non-vacuous: before the share existed the two budgets were equal, so the accept
  /// loops admitted serve tasks against the clients' budget alone (`adm_refused` 4,554 at ~3×
  /// oversubscription, `docs/wip/fleet-under-load.md`).
  #[test]
  fn a_fleet_configuration_derives_its_own_task_share_from_the_peer_count() {
    let profile = MachineProfile::measure(ProfileOptions {
      budget_per_probe: Duration::from_millis(1),
      codecs: false,
      core_matrix: false,
    });
    let solo = DaemonConfig::derive(&profile, "fleet-share-solo");
    let peers: Vec<HostId> = (1..=5).map(HostId).collect();
    let fleet = DaemonConfig::derive(&profile, "fleet-share-fleet").with_fleet(FleetMembership {
      quorum: Quorum { f: 2 },
      peers: peers.clone(),
      host: HostId(0),
      origin_anchor: HostId(0),
      domains: BTreeMap::new(),
      regions: BTreeMap::new(),
      durability: None,
      region_mirrors: BTreeMap::new(),
    });
    let share = peers.len() * FLEET_LOOPS_PER_PEER
      + peers.len() * crate::fleet::SESSIONS_PER_PEER * FLEET_PLANES
      + FLEET_LOOPS_PER_SHARD;
    assert_eq!(
      fleet.runtime.tasks_per_shard,
      solo.runtime.tasks_per_shard + share,
      "the fleet's task share is derived from the peer count and added to the shard's budget"
    );
    assert_eq!(
      fleet.runtime.timers_per_shard,
      solo.runtime.timers_per_shard + share,
      "every fleet task may hold a timer"
    );
    assert!(
      fleet
        .derivations
        .iter()
        .any(|line| line.starts_with("fleet_tasks_per_shard")),
      "the boot log names the share with its input: {:?}",
      fleet.derivations
    );
  }

  /// AC-0.10 (§4.2 "effective capacity ... constrained by OS/job/cgroup limits"): a bound set on
  /// the process below its physical memory caps every shard's reserve, so the classes summed over
  /// the shards never exceed what the host will let the process use; without a bound, or with one
  /// above total, the reserve is the share of total 1f40689 chose. Non-vacuous: derived from total
  /// alone, a quarter-of-total bound leaves the reserve four times what the bound allows.
  #[test]
  fn a_memory_bound_below_total_caps_the_reserve() {
    let mut profile = MachineProfile::measure(ProfileOptions {
      budget_per_probe: Duration::from_millis(1),
      codecs: false,
      core_matrix: false,
    });
    let total = profile.facts.memory.total;
    profile.facts.memory.limit = None;
    let unbounded = DaemonConfig::derive(&profile, "bound-none");
    let shards = u64::from(unbounded.runtime.shards.max(1));
    assert_eq!(
      unbounded.reserve_per_shard,
      total / shards / MEMORY_CLASSES,
      "no bound: the reserve is the share of total"
    );
    profile.facts.memory.limit = Some(total / 4);
    let bounded = DaemonConfig::derive(&profile, "bound-quarter");
    assert_eq!(
      bounded.reserve_per_shard,
      total / 4 / shards / MEMORY_CLASSES,
      "a quarter-of-total bound: the reserve is the share of the bound"
    );
    assert!(
      bounded.reserve_per_shard * shards * MEMORY_CLASSES <= total / 4,
      "the classes over every shard fit the bound ({} × {shards} × {MEMORY_CLASSES} ≤ {})",
      bounded.reserve_per_shard,
      total / 4
    );
    profile.facts.memory.limit = Some(total.saturating_mul(2));
    let above = DaemonConfig::derive(&profile, "bound-above");
    assert_eq!(
      above.reserve_per_shard,
      total / shards / MEMORY_CLASSES,
      "a bound above total does not raise the reserve"
    );
  }
}
