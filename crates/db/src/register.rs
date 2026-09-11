//! Registers and the configuration oracle (§4.8 "Data model", D-14, D-18): every head, chain
//! version, lease and catalog entry is a register written by its owner alone under its host
//! epoch. This module is the register protocol's pure core, parameterized by the fault
//! tolerance `f`, so the laptop (`f = 0`) is the degenerate of one formula, never a mode
//! switch (R8): at `f = 0` a register's only candidate is the owner, its commit is the local
//! append, `placed` is true the moment the owner holds it, and the configuration is one
//! self-acknowledging voter. Phase 8 raises `f` and adds the holders, the takeover and the
//! mirror; nothing here changes shape, and every reply already carries `placed` and
//! `mirror_age` so no interface moves.
//!
//! What is proved here (the `FencedRegister` and `Reconfig` TLA+ models check the same
//! properties over the full protocol): a write commits only at `f + 1` acknowledgements from
//! the `2f + 1` candidates; a record under a host epoch below the highest a holder has seen is
//! refused (`StaleEpoch`), so a resumed stale owner never commits; a request under a stale
//! configuration version is refused (`ConfigurationStale`) with the current one. The N=1
//! differential test drives this at `f = 0` and at a simulated `f = 1` whose holder stubs
//! acknowledge locally, and asserts the observable outcomes are identical.

/// A host in the fleet (its id; the creator host lives in a volume id's high bits, §4.8
/// "Lookup"). One host on a laptop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostId(pub u64);

/// A register object's id — the identifier of any register the owner writes (a volume head, a chain
/// version, a landing lease, a catalog entry): 128 bits whose **high 8 bytes name the creator host**
/// and whose low 8 bytes are a unique per-creator suffix (§4.8 "Lookup"; the catalog's `VolumeId` is
/// exactly this shape). It routes by identity — a lookup extracts the creator from the high half and
/// reaches that owner, or its takeover successor, so no global catalog is needed (D-12, D-14). A bare
/// `u64` object id was a shortcut: it cannot carry a full 64-bit host id *and* a unique suffix at once,
/// which would force either a narrowed host space or a translation table — both of which the design
/// forbids. The bytes are big-endian creator-then-suffix, so the id's natural order groups objects by
/// creator, and rendezvous placement hashes all 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub [u8; 16]);

impl ObjectId {
  /// An object id from its `creator` host (the high 8 bytes) and a unique per-creator `local` suffix
  /// (the low 8 bytes), each big-endian so the id sorts by creator then suffix. `const` so a fixed
  /// object id can be a `const` (the takeover tests name one).
  pub const fn new(creator: HostId, local: u64) -> ObjectId {
    let c = creator.0.to_be_bytes();
    let l = local.to_be_bytes();
    ObjectId([
      c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], l[0], l[1], l[2], l[3], l[4], l[5], l[6],
      l[7],
    ])
  }

  /// The creator host named in the high 8 bytes — the id's owner by construction and the routing key a
  /// lookup extracts (§4.8 "a volume id carries its creator host").
  pub fn creator(&self) -> HostId {
    let mut word = [0u8; size_of::<u64>()];
    word.copy_from_slice(&self.0[..size_of::<u64>()]);
    HostId(u64::from_be_bytes(word))
  }

  /// The unique per-creator suffix in the low 8 bytes.
  pub fn local(&self) -> u64 {
    let mut word = [0u8; size_of::<u64>()];
    word.copy_from_slice(&self.0[size_of::<u64>()..]);
    u64::from_be_bytes(word)
  }
}

/// The width of an encoded object id (16 bytes) — its place in every register wire layout.
const OBJECT_BYTES: usize = size_of::<ObjectId>();

/// Reads an object id from the front of a decode split; the caller has already checked the length, so
/// a mismatch falls back to the zero id rather than panicking (the hostile-input rule).
fn object_from(bytes: &[u8]) -> ObjectId {
  ObjectId(bytes.try_into().unwrap_or([0u8; OBJECT_BYTES]))
}

/// The fault tolerance and the quorum it fixes: `2f + 1` candidates, a commit at `f + 1`
/// (§4.8 "one quorum rule"). `f = 0` gives one candidate and a commit of one, the local
/// append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quorum {
  /// The number of holder failures the object tolerates.
  pub f: u32,
}

impl Quorum {
  /// Derived: the candidate holders an object has, `2f + 1` (§4.8, D-14).
  pub fn candidates(self) -> usize {
    usize::try_from(self.f)
      .unwrap_or(usize::MAX)
      .saturating_mul(2)
      .saturating_add(1)
  }

  /// Derived: the acknowledgements a write commits at, `f + 1` (the read and write quorums
  /// intersect, so a committed record is seen by every later quorum, §4.8).
  pub fn commit(self) -> usize {
    usize::try_from(self.f)
      .unwrap_or(usize::MAX)
      .saturating_add(1)
  }

  /// Whether `acked` acknowledgements commit a write.
  pub fn committed(self, acked: usize) -> bool {
    acked >= self.commit()
  }
}

/// A host's monotonic authority over the objects it owns (§4.8 "Leases and reads"): a holder
/// accepts a record only when its epoch is at least the highest it has seen for that host, so
/// a resumed stale owner (a lower epoch after a takeover bumped it) is refused. One epoch,
/// never bumped, on a laptop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostEpoch(pub u64);

/// The first epoch a host serves under.
pub const FIRST_EPOCH: HostEpoch = HostEpoch(1);

/// A register refusal (the closed taxonomy of §4.8 that this core can raise).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterError {
  /// A record under an epoch below the highest seen for its host.
  StaleEpoch {
    /// The current (highest seen) epoch.
    current: u64,
  },
  /// A request under a configuration version other than the current one.
  ConfigurationStale {
    /// The current version.
    version: u64,
  },
  /// A scope the deployment does not have (the mirror on a laptop).
  Unsupported {
    /// The scope named.
    scope: DurabilityScope,
  },
  /// A shipped record's bytes were truncated or claimed a length past what arrived.
  MalformedRecord,
  /// A record from a principal the holder's authority does not authorize for the object.
  Unauthorized,
  /// A record under a configuration generation other than the holder's current one.
  ForeignGeneration {
    /// The holder's current generation.
    current: u64,
  },
  /// A different value offered at a ledger position that already holds one — a committed position is
  /// never rewritten (§4.8, BUG-12).
  ConflictingPosition,
}

/// What `await placed` waits for (§4.8 "Mirroring", D-18): the home region's commit, or the
/// mirror region's. The mirror does not exist at `f = 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilityScope {
  /// The owner's region: `f + 1` of its candidates.
  Region,
  /// The mirror region: `f + 1` of the mirror's candidates.
  Mirror,
}

/// A register's placement: the candidates chosen for the object and the subset that have
/// acknowledged its newest committed record. At `f = 0` both are the owner alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
  /// The candidate holders (`2f + 1`), the owner among them.
  pub candidates: Vec<HostId>,
  /// The candidates that acknowledged, recorded in the head so a reader learns the copies.
  pub acked: Vec<HostId>,
  /// The mirror candidates that acknowledged, when the record was shipped to the mirror.
  pub mirror_acked: Option<Vec<HostId>>,
}

impl Placement {
  /// The placement of an object at the moment its owner holds it and nothing else has yet: at
  /// `f = 0` this already commits (the owner is `f + 1`); at `f > 0` it is `Local` until
  /// holders acknowledge.
  pub fn local(
    owner: HostId,
    quorum: Quorum,
    neighbourhood: &[HostId],
    object: ObjectId,
  ) -> Placement {
    let candidates = candidates_for(owner, neighbourhood, object, quorum);
    let acked = if quorum.committed(1) {
      vec![owner]
    } else {
      Vec::new()
    };
    Placement {
      candidates,
      acked,
      mirror_acked: None,
    }
  }

  /// Whether the region commit holds (`f + 1` regional acknowledgements). Counts each **candidate**
  /// once: a duplicate acknowledgement from one holder, or an acknowledgement bearing a host id that
  /// is not a candidate for this object, cannot manufacture a quorum (a two-count `f = 1` commit must
  /// be two *distinct* candidates). The vector length alone is not the count.
  pub fn placed(&self, quorum: Quorum) -> bool {
    quorum.committed(self.distinct_candidate_acks(&self.acked))
  }

  /// Whether the mirror commit holds — the same distinct-candidate counting as [`Placement::placed`].
  pub fn placed_mirror(&self, quorum: Quorum) -> bool {
    self
      .mirror_acked
      .as_ref()
      .is_some_and(|m| quorum.committed(self.distinct_candidate_acks(m)))
  }

  /// The number of distinct candidates in `acks`: dedups the host ids and drops any that are not a
  /// candidate for this object, so only eligible, once-counted acknowledgements count toward a quorum.
  fn distinct_candidate_acks(&self, acks: &[HostId]) -> usize {
    acks
      .iter()
      .filter(|host| self.candidates.contains(host))
      .collect::<std::collections::BTreeSet<&HostId>>()
      .len()
  }
}

/// The candidate holders for `object`: the owner, then the highest-ranked of the neighbourhood
/// by rendezvous (highest-random-weight) hashing, to `2f + 1` total. Deterministic, so every
/// host computes the same set from the object id and the neighbourhood, with no directory
/// (§4.8 "Placement"). At `f = 0` this is the owner alone.
pub fn candidates_for(
  owner: HostId,
  neighbourhood: &[HostId],
  object: ObjectId,
  quorum: Quorum,
) -> Vec<HostId> {
  let mut chosen = vec![owner];
  let wanted = quorum.candidates();
  if wanted <= 1 {
    return chosen;
  }
  let mut ranked: Vec<(u64, HostId)> = neighbourhood
    .iter()
    .filter(|h| **h != owner)
    .map(|h| (rendezvous_weight(h.0, &object), *h))
    .collect();
  // Highest weight first; the host id breaks a tie, so the order is total and stable.
  ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  for (_, host) in ranked {
    if chosen.len() >= wanted {
      break;
    }
    chosen.push(host);
  }
  chosen
}

/// The **bounded neighbourhood** the configuration assigns `owner` from the alive set (§4.8 "Placement";
/// D-14 "a fixed set of hosts across failure domains whose size — the scatter width — bounds the copyset
/// count"): the owner plus the top `scatter - 1` other alive hosts by rendezvous weight *keyed on the
/// owner*, so the choice is deterministic (every node computes the same neighbourhood for `owner`),
/// owner-specific (a different owner scatters over a different set), and stable — a membership change moves
/// only the hosts whose rendezvous rank crossed the cut, the "add before remove" the design calls for
/// (§4.8). The caller (`ConfigGroup`) passes the derived scatter width, defaulting to the candidate floor
/// `2f+1` ([`Quorum::candidates`]) so the neighbourhood is one copyset when recovery is unsized;
/// `scatter = 0` is the explicit *unbounded* escape (the whole alive set, in id order) a test uses, never a
/// resting default. The owner is always included; the neighbourhood never exceeds `scatter`.
pub fn select_neighbourhood(owner: HostId, alive: &[HostId], scatter: u64) -> Vec<HostId> {
  let mut chosen = vec![owner];
  let bound = usize::try_from(scatter).unwrap_or(usize::MAX);
  if scatter == 0 {
    // Unbounded: the whole alive set (owner first, the rest in id order for determinism).
    let mut rest: Vec<HostId> = alive.iter().copied().filter(|h| *h != owner).collect();
    rest.sort_unstable_by_key(|h| h.0);
    chosen.extend(rest);
    return chosen;
  }
  // Rank the other alive hosts by rendezvous weight keyed on the owner (a neighbourhood is the owner's own
  // scatter set, so the owner is the natural rendezvous key, `ObjectId::new(owner, 0)`), highest first, the
  // host id breaking ties for a total, stable order.
  let key = ObjectId::new(owner, 0);
  let mut ranked: Vec<(u64, HostId)> = alive
    .iter()
    .copied()
    .filter(|h| *h != owner)
    .map(|h| (rendezvous_weight(h.0, &key), h))
    .collect();
  ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  for (_, host) in ranked {
    if chosen.len() >= bound {
      break;
    }
    chosen.push(host);
  }
  chosen
}

/// A host's **failure domain** — the node in the failure-domain tree (§4.8: thread, process, host, rack,
/// zone, region) at the level replication spreads across, typically a rack. Two hosts in the same domain
/// share a fate (a rack power loss), so a copyset must not put two copies in one domain, or a single domain
/// failure could take a whole copyset.
pub type DomainId = u64;

/// The **fixed copysets** an owner's objects place onto within its neighbourhood, the Copyset Replication
/// construction restricted to distinct failure domains ("derived approach without loss",
/// `research/metadata-replication.md` §3.1; [A: Cidon et al., ATC 2013]). Each copyset is the owner plus up
/// to `2f` co-holders drawn from **distinct** failure domains (none sharing the owner's, since the owner is
/// in every copyset), so a candidate set is `2f + 1` across `2f + 1` domains and no single domain failure
/// loses more than one copy. The neighbourhood's co-holders are partitioned across `≈⌈|co-holders|/2f⌉`
/// copysets — a count **linear** in the scatter width — rather than the `Θ(S^(2f))` an object-by-object
/// rendezvous over the raw hosts would make. Deterministic: every host builds the same copysets from the
/// same neighbourhood, so an object routes to its holders with no directory. At `f = 0` the only copyset is
/// the owner alone (the laptop degenerate). A neighbourhood too domain-poor to fill `2f` distinct domains
/// yields shorter copysets (fewer copies) — a real deficiency the copyset-count/loss check surfaces, never
/// papered over.
pub fn owner_copysets(
  owner: HostId,
  neighbourhood: &[(HostId, DomainId)],
  quorum: Quorum,
) -> Vec<Vec<HostId>> {
  let co_holders = quorum.candidates().saturating_sub(1); // 2f
  if co_holders == 0 {
    return vec![vec![owner]];
  }
  let owner_domain = neighbourhood
    .iter()
    .find(|(host, _)| *host == owner)
    .map(|(_, domain)| *domain);
  // Usable co-holders: neither the owner nor a host sharing the owner's domain (which could never sit in a
  // copyset with the owner without repeating that domain).
  let mut usable: Vec<(HostId, DomainId)> = neighbourhood
    .iter()
    .copied()
    .filter(|(host, domain)| *host != owner && Some(*domain) != owner_domain)
    .collect();
  // A deterministic order — by domain then host id — so every node builds the same copysets.
  usable.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.0.cmp(&b.0.0)));
  // Greedy first-fit: each co-holder joins the first group with room that does not already hold its domain,
  // so every group has distinct domains and the groups stay near the minimal `⌈|usable|/2f⌉`.
  let mut groups: Vec<Vec<(HostId, DomainId)>> = Vec::new();
  for (host, domain) in usable {
    match groups
      .iter_mut()
      .find(|group| group.len() < co_holders && group.iter().all(|(_, d)| *d != domain))
    {
      Some(group) => group.push((host, domain)),
      None => groups.push(vec![(host, domain)]),
    }
  }
  if groups.is_empty() {
    return vec![vec![owner]];
  }
  groups
    .into_iter()
    .map(|group| {
      let mut copyset = vec![owner];
      copyset.extend(group.into_iter().map(|(host, _)| host));
      copyset
    })
    .collect()
}

/// The copyset `object` places onto: **rendezvous over the copysets**, not over the raw hosts — the highest
/// rendezvous weight, keyed on a copyset's first co-holder (the owner heads every copyset, so it cannot
/// discriminate; the co-holder groups are disjoint, so their first members are distinct keys). Deterministic
/// and balanced across objects, so the owner's objects spread evenly over its copysets. `None` only if there
/// are no copysets.
pub fn copyset_for(object: ObjectId, copysets: &[Vec<HostId>]) -> Option<Vec<HostId>> {
  copysets
    .iter()
    .max_by_key(|copyset| {
      let key = copyset.get(1).or_else(|| copyset.first());
      let weight = key.map_or(0, |host| rendezvous_weight(host.0, &object));
      (weight, key.copied().unwrap_or(HostId(0)))
    })
    .cloned()
}

/// The rendezvous weight of a host for an object: a hash of the pair, so the ranking is
/// pseudo-random per object and stable across hosts (FNV-1a over the two words; the placement
/// research calls for a good mixer, and this is replaced by the measured one when placement is
/// tuned in Phase 8).
fn rendezvous_weight(host: u64, object: &ObjectId) -> u64 {
  /// Format: the FNV-1a 64-bit offset basis (Fowler, Noll, Vo).
  const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
  /// Format: the FNV-1a 64-bit prime.
  const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
  let mut hash = FNV_OFFSET;
  for byte in host.to_le_bytes().iter().chain(object.0.iter()) {
    hash ^= u64::from(*byte);
    hash = hash.wrapping_mul(FNV_PRIME);
  }
  // Finalize with the MurmurHash3 64-bit avalanche (Appleby's `fmix64`), so a difference in the last
  // byte fed — an object id whose creator half is constant and whose suffix differs by one low byte —
  // still reorders the ranking. Without it, plain FNV-1a leaves the last byte with no post-mixing, and
  // rendezvous collapses (every such object picks the same holder). The three shift/multiply steps are
  // the published fmix64 constants.
  /// Format: MurmurHash3 `fmix64` first multiplier (Austin Appleby, public domain).
  const FMIX_A: u64 = 0xff51_afd7_ed55_8ccd;
  /// Format: MurmurHash3 `fmix64` second multiplier (Austin Appleby, public domain).
  const FMIX_B: u64 = 0xc4ce_b9fe_1a85_ec53;
  /// Format: MurmurHash3 `fmix64` shift distance (Austin Appleby, public domain).
  const FMIX_SHIFT: u32 = 33;
  hash ^= hash >> FMIX_SHIFT;
  hash = hash.wrapping_mul(FMIX_A);
  hash ^= hash >> FMIX_SHIFT;
  hash = hash.wrapping_mul(FMIX_B);
  hash ^= hash >> FMIX_SHIFT;
  hash
}

/// The host that rendezvous ranks first for `object` among `hosts` — the highest rendezvous weight,
/// the lowest id breaking a tie (the same total order [`candidates_for`] uses for the candidates after
/// the owner). `None` if `hosts` is empty. Takeover assigns a dead owner's object to this survivor of
/// its neighbourhood (§4.8 "Promotion and takeover"; the worked example's "rendezvous ranks first
/// among {B, C, D}").
pub fn rendezvous_first(hosts: &[HostId], object: ObjectId) -> Option<HostId> {
  let mut ranked: Vec<(u64, HostId)> = hosts
    .iter()
    .map(|host| (rendezvous_weight(host.0, &object), *host))
    .collect();
  ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  ranked.first().map(|(_, host)| *host)
}

/// The number of **distinct copysets** Copyset Replication lays down over `hosts` nodes at scatter width
/// `scatter` with `copies` copies per chunk — `⌈scatter/(copies−1)⌉ · hosts / copies`, **linear in the
/// scatter width** [A: Cidon et al., "Copysets", USENIX ATC 2013, §4; read 2026-09-11]. A copyset is a set
/// of `copies` nodes that together hold every copy of some chunk, so a chunk is lost exactly when one
/// copyset's nodes all fail coincidentally; the loss probability grows with this count
/// ([`coincident_loss_probability`]), which is why it is bounded. With `copies ≤ 1` (the laptop degenerate,
/// no redundancy) each host is its own single-node copyset, so the count is `hosts`.
///
/// Verified against the paper's worked examples: `hosts = 9, scatter = 4, copies = 3` gives `6`
/// (two permutations of three), and `hosts = 5000, scatter = 10, copies = 3` gives `8333` ("about 8,300").
pub fn copyset_count(hosts: u64, scatter: u64, copies: u64) -> u64 {
  if copies <= 1 {
    return hosts;
  }
  let permutations = scatter.div_ceil(copies - 1);
  permutations.saturating_mul(hosts) / copies
}

/// The number of copysets **random replication** would create at the same parameters — `hosts · C(scatter,
/// copies−1)`, which is `Θ(scatter^(copies−1))`, super-linear (Cidon et al. §3, for `scatter < hosts/2`).
/// This is what per-object rendezvous over the raw host set produces, and what the fixed-copyset
/// construction exists to avoid; kept so the non-vacuity of the bounding is testable (the copyset count
/// must stay far below this). Verified: `hosts = 9, scatter = 4, copies = 3` gives `54` (the paper's value).
pub fn random_copyset_count(hosts: u64, scatter: u64, copies: u64) -> u64 {
  if copies <= 1 {
    return hosts;
  }
  hosts.saturating_mul(binomial(scatter, copies - 1))
}

/// `C(n, k)` computed iteratively without overflow for the small `k` (`copies − 1`) placement uses;
/// saturating so an out-of-range input cannot panic. `C(n, k) = 0` for `k > n`.
fn binomial(n: u64, k: u64) -> u64 {
  if k > n {
    return 0;
  }
  let k = k.min(n - k);
  let mut result: u64 = 1;
  for i in 0..k {
    result = result.saturating_mul(n - i) / (i + 1);
  }
  result
}

/// The **scatter width** for a host (§4.8 "Placement"; the derivation in
/// `research/metadata-replication.md` §3.1): `S = max(2f+1, ⌈D/(B·T)⌉)` — the least parallelism that
/// re-replicates a dead host's `data_bytes` (`D`) within the recovery budget `budget_ns` (`T`) at
/// per-host re-replication bandwidth `bandwidth_bytes_per_s` (`B`), never below the candidate floor `2f+1`
/// (the neighbourhood must hold a full candidate set). This is the *smallest* S meeting recovery, which by
/// the monotonicity of the loss probability in the copyset count is also the *lowest-loss* S meeting it —
/// "derived without loss". The caller then checks the copyset count the returned S implies against the
/// accepted loss probability ([`coincident_loss_probability`]); a violation is a genuine recovery-vs-
/// durability conflict, not something to paper over by scattering wider. When the bandwidth or budget is
/// unknown (`0`), the candidate floor stands — recovery cannot be sized, so the tightest, lowest-loss
/// neighbourhood is used.
pub fn scatter_width(data_bytes: u64, bandwidth_bytes_per_s: u64, budget_ns: u64, f: u64) -> u64 {
  let candidate_floor = 2 * f + 1;
  if bandwidth_bytes_per_s == 0 || budget_ns == 0 {
    return candidate_floor;
  }
  // S_recover = ⌈ D / (B · T_seconds) ⌉ = ⌈ D · 1e9 / (B · T_ns) ⌉, in u128 so a large host budget cannot
  // overflow the numerator.
  /// Format: nanoseconds per second, to turn the recovery budget (in nanoseconds) into a rate divisor.
  const NANOS_PER_SECOND: u128 = 1_000_000_000;
  let numerator = u128::from(data_bytes).saturating_mul(NANOS_PER_SECOND);
  let denominator = u128::from(bandwidth_bytes_per_s)
    .saturating_mul(u128::from(budget_ns))
    .max(1);
  let recover = numerator.div_ceil(denominator);
  let recover = u64::try_from(recover).unwrap_or(u64::MAX);
  candidate_floor.max(recover)
}

/// The probability that a coincident failure of `failed` of `hosts` nodes loses at least one chunk, given
/// `copysets` distinct copysets of `copies` nodes each: `≈ copysets · C(failed, copies) / C(hosts, copies)`
/// (Cidon et al. §3; the exact `copysets / C(hosts, copies)` is the `failed = copies` case). The binomial
/// ratio is a product of `copies` fractions each below one, evaluated in `f64` (this is a cold-path
/// durability check at a configuration change, never a data-path cost). Zero when fewer than `copies`
/// nodes fail (no full copyset can be inside the failed set) or there is no redundancy (`copies ≤ 1`).
pub fn coincident_loss_probability(copysets: u64, hosts: u64, failed: u64, copies: u64) -> f64 {
  if copies <= 1 || failed < copies || hosts < copies {
    return 0.0;
  }
  // C(failed, copies) / C(hosts, copies) = ∏_{i=0}^{copies-1} (failed - i) / (hosts - i).
  #[allow(clippy::cast_precision_loss)]
  let ratio: f64 = (0..copies)
    .map(|i| (failed - i) as f64 / (hosts - i) as f64)
    .product();
  #[allow(clippy::cast_precision_loss)]
  let expected = copysets as f64 * ratio;
  // The expected number of failed copysets bounds the loss probability from above (union bound); clamp to a
  // probability so a caller reads it as one.
  /// Format: certainty — the most a probability can be.
  const CERTAIN: f64 = 1.0;
  expected.min(CERTAIN)
}

/// A holder's fence for one host: the highest epoch it has accepted a record under (§4.8
/// "Promotion and takeover"). A record under a lower epoch is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
  /// The highest epoch seen.
  pub seen: HostEpoch,
}

impl Fence {
  /// A fence that has seen the first epoch.
  pub fn new() -> Fence {
    Fence { seen: FIRST_EPOCH }
  }

  /// Accepts a record under `epoch`, raising the fence; a lower epoch is refused so a resumed
  /// stale owner never commits (`StaleNeverCommits`).
  pub fn accept(&mut self, epoch: HostEpoch) -> Result<(), RegisterError> {
    if epoch < self.seen {
      return Err(RegisterError::StaleEpoch {
        current: self.seen.0,
      });
    }
    self.seen = epoch;
    Ok(())
  }
}

impl Default for Fence {
  fn default() -> Fence {
    Fence::new()
  }
}

/// A register record on its way to the candidate holders (§4.8 "records are sent to all candidates").
/// It names its ledger position and authority so acceptance is bound to *this specific write*: the
/// authorized `owner`, the `object` and its `sequence` (the ledger position), the owner's host `epoch`
/// (the ballot the holder fences against, D-16), the configuration `generation` it is written under,
/// and the value bytes (a head, chain version, lease or catalog entry — opaque here). Its BLAKE3
/// [`identity`](Record::identity) over all of those is what an acknowledgement binds to, so a reply for
/// a different record — different value, position, epoch or generation — cannot be counted for this one
/// (§4.8 "network receipt is not acceptance"). Rides the session plane's RPC (fleet-transport.md §8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
  /// The authorized owner writing this record; a holder refuses one from an unauthorized principal.
  pub owner: HostId,
  /// The object (volume head, chain, lease, catalog entry) this record belongs to.
  pub object: ObjectId,
  /// The ledger position within the object (monotonic per owner; a committed position is never
  /// rewritten with a different value).
  pub sequence: u64,
  /// The owner's host epoch — the ballot; a holder refuses one below the epoch it has promised.
  pub epoch: HostEpoch,
  /// The configuration generation this write is under; a holder refuses a foreign generation.
  pub generation: u64,
  /// The record's value bytes (opaque to the register protocol).
  pub value: Vec<u8>,
}

/// The fixed prefix of an encoded record: the five header words then the value's length.
/// Format: §4.8 record layout — `owner` (u64 LE), `object` (16 bytes), `sequence`, `epoch`,
/// `generation` (each u64 LE), `value_len` (u32 LE), then the value bytes; a record is MTU-shippable,
/// so the length is a `u32`.
const RECORD_PREFIX_BYTES: usize = 4 * size_of::<u64>() + OBJECT_BYTES + size_of::<u32>();

impl Record {
  /// The canonical bytes: the five header words, the value length, the value — little-endian
  /// throughout, so two hosts encode a record identically (the determinism its identity relies on).
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_PREFIX_BYTES + self.value.len());
    out.extend_from_slice(&self.owner.0.to_le_bytes());
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.sequence.to_le_bytes());
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.generation.to_le_bytes());
    let value_len = u32::try_from(self.value.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&value_len.to_le_bytes());
    out.extend_from_slice(&self.value);
    out
  }

  /// Reconstructs a record from `bytes`, checking every length against what remains before reading, so
  /// a truncated or over-claiming record is a typed [`RegisterError::MalformedRecord`], never a panic
  /// or an over-read (the hostile-input rule; this parses bytes that crossed the network).
  pub fn decode(bytes: &[u8]) -> Result<Record, RegisterError> {
    if bytes.len() < RECORD_PREFIX_BYTES {
      return Err(RegisterError::MalformedRecord);
    }
    let word = |slice: &[u8]| u64::from_le_bytes(slice.try_into().unwrap_or([0; 8]));
    let (owner_bytes, rest) = bytes.split_at(size_of::<u64>());
    let (object_bytes, rest) = rest.split_at(OBJECT_BYTES);
    let (sequence_bytes, rest) = rest.split_at(size_of::<u64>());
    let (epoch_bytes, rest) = rest.split_at(size_of::<u64>());
    let (generation_bytes, rest) = rest.split_at(size_of::<u64>());
    let (len_bytes, value_bytes) = rest.split_at(size_of::<u32>());
    let value_len = u32::from_le_bytes(len_bytes.try_into().unwrap_or([0; 4]));
    let value_len = usize::try_from(value_len).unwrap_or(usize::MAX);
    if value_bytes.len() != value_len {
      return Err(RegisterError::MalformedRecord);
    }
    Ok(Record {
      owner: HostId(word(owner_bytes)),
      object: object_from(object_bytes),
      sequence: word(sequence_bytes),
      epoch: HostEpoch(word(epoch_bytes)),
      generation: word(generation_bytes),
      value: value_bytes.to_vec(),
    })
  }

  /// The record's identity: the BLAKE3 of its canonical encoding, binding every field. An
  /// acknowledgement carries this so a reply for any other record cannot be counted for this one.
  pub fn identity(&self) -> [u8; 32] {
    *blake3::hash(&self.encode()).as_bytes()
  }
}

/// The validated authority a holder accepts records under (§4.8 "epoch allocation derives from
/// configuration authority"): the current configuration `generation`, and the `owner` authorized to
/// write in it. The cluster plane supplies this from the configuration group (owed distribution); a
/// holder refuses a record whose generation or owner does not match, so there is no unauthenticated
/// path. (One authorized owner per generation this slice; per-object authority is owed.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authority {
  /// The configuration generation this holder currently serves.
  pub generation: u64,
  /// The owner authorized to write registers in this generation.
  pub owner: HostId,
}

/// A holder's acknowledgement of a *specific* record (§4.8 "the holder set, record and its placed
/// status publish atomically"): the acknowledging holder, the ledger position, the generation, and the
/// record's identity. The owner counts it only if it [`binds`](Ack::binds) to the record shipped, so a
/// duplicate, foreign, stale or wrong-record reply cannot manufacture a quorum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
  /// The holder that accepted and stored the record.
  pub holder: HostId,
  /// The object the record belongs to.
  pub object: ObjectId,
  /// The ledger position accepted.
  pub sequence: u64,
  /// The generation it was accepted under.
  pub generation: u64,
  /// The accepted record's identity.
  pub identity: [u8; 32],
}

/// An acknowledgement's fixed wire size.
/// Format: `holder` (u64 LE), `object` (16 bytes), `sequence`, `generation` (each u64 LE), then the
/// 32-byte BLAKE3 identity.
const ACK_BYTES: usize = 3 * size_of::<u64>() + OBJECT_BYTES + 32;

impl Ack {
  /// Whether this acknowledgement is for `record` — the position, generation and identity all match.
  pub fn binds(&self, record: &Record) -> bool {
    self.object == record.object
      && self.sequence == record.sequence
      && self.generation == record.generation
      && self.identity == record.identity()
  }

  /// The canonical bytes an acknowledgement rides back on: holder, object, sequence, generation (each
  /// u64 LE), then the BLAKE3 identity.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(ACK_BYTES);
    out.extend_from_slice(&self.holder.0.to_le_bytes());
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.sequence.to_le_bytes());
    out.extend_from_slice(&self.generation.to_le_bytes());
    out.extend_from_slice(&self.identity);
    out
  }

  /// Reconstructs an acknowledgement from its bytes, or a typed [`RegisterError::MalformedRecord`] if
  /// they are the wrong length (a reply that crossed the network — hostile input).
  pub fn decode(bytes: &[u8]) -> Result<Ack, RegisterError> {
    if bytes.len() != ACK_BYTES {
      return Err(RegisterError::MalformedRecord);
    }
    let word = |slice: &[u8]| u64::from_le_bytes(slice.try_into().unwrap_or([0; 8]));
    let (holder_bytes, rest) = bytes.split_at(size_of::<u64>());
    let (object_bytes, rest) = rest.split_at(OBJECT_BYTES);
    let (sequence_bytes, rest) = rest.split_at(size_of::<u64>());
    let (generation_bytes, identity_bytes) = rest.split_at(size_of::<u64>());
    let mut identity = [0u8; 32];
    identity.copy_from_slice(identity_bytes);
    Ok(Ack {
      holder: HostId(word(holder_bytes)),
      object: object_from(object_bytes),
      sequence: word(sequence_bytes),
      generation: word(generation_bytes),
      identity,
    })
  }
}

/// The highest record a holder has accepted for one object (§4.8 "reports the highest record it holds
/// for each object"): the ledger position, the epoch it was accepted under, and the value. A holder
/// reports this in its [`Promise`] so the new owner can adopt the newest across a quorum. Ordered by
/// position then epoch, so the *newest* record is the maximum — a later write has a higher `sequence`,
/// and a re-commit of the same head under a newer epoch has a higher `epoch` at the same `sequence`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
  /// The ledger position (the head's `sequence`; a committed position is never rewritten differently).
  pub sequence: u64,
  /// The epoch this value was accepted under.
  pub epoch: HostEpoch,
  /// The accepted value bytes (opaque to the register protocol).
  pub value: Vec<u8>,
}

impl Accepted {
  /// Whether `self` is a newer record than `other`: a higher position, or the same position under a
  /// higher epoch. The adoption order of phase one — the new owner keeps the newest reported record.
  /// Public so the cluster plane's live promotion collector folds promises by the same order the
  /// sans-io [`promote_over_holders`] does.
  pub fn newer_than(&self, other: &Accepted) -> bool {
    (self.sequence, self.epoch.0) > (other.sequence, other.epoch.0)
  }
}

/// The new owner's phase-one message when it takes over an object (§4.8 "Promotion and takeover": "each
/// new owner runs phase one in one batched round … every holder raises its fence for that host to the
/// new epoch"). It names the object, the new owner running the promotion, the bumped `epoch` it will
/// serve under, and the takeover configuration `generation`. A holder that has installed that
/// generation raises its fence to `epoch` and reports its highest [`Accepted`] record for the object;
/// a holder still under the old generation, or already fenced above `epoch` by a newer takeover,
/// refuses. Rides the session plane's RPC (fleet-transport.md §8); the register protocol stays sans-io.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prepare {
  /// The new owner (the surviving candidate the configuration named) running phase one.
  pub owner: HostId,
  /// The object being taken over.
  pub object: ObjectId,
  /// The new (bumped) epoch the new owner will serve under — the ballot the holder promises.
  pub epoch: HostEpoch,
  /// The configuration generation the takeover is under; a holder under a different one refuses.
  pub generation: u64,
}

/// A prepare's fixed wire size.
/// Format: `owner` (u64 LE), `object` (16 bytes), `epoch`, `generation` (each u64 LE).
const PREPARE_BYTES: usize = 3 * size_of::<u64>() + OBJECT_BYTES;

impl Prepare {
  /// The canonical bytes: owner, object, epoch, generation, each little-endian.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREPARE_BYTES);
    out.extend_from_slice(&self.owner.0.to_le_bytes());
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.generation.to_le_bytes());
    out
  }

  /// Reconstructs a prepare from its bytes, or a typed [`RegisterError::MalformedRecord`] if they are
  /// the wrong length (a message that crossed the network — hostile input).
  pub fn decode(bytes: &[u8]) -> Result<Prepare, RegisterError> {
    if bytes.len() != PREPARE_BYTES {
      return Err(RegisterError::MalformedRecord);
    }
    let word = |slice: &[u8]| u64::from_le_bytes(slice.try_into().unwrap_or([0; 8]));
    let (owner_bytes, rest) = bytes.split_at(size_of::<u64>());
    let (object_bytes, rest) = rest.split_at(OBJECT_BYTES);
    let (epoch_bytes, generation_bytes) = rest.split_at(size_of::<u64>());
    Ok(Prepare {
      owner: HostId(word(owner_bytes)),
      object: object_from(object_bytes),
      epoch: HostEpoch(word(epoch_bytes)),
      generation: word(generation_bytes),
    })
  }
}

/// A holder's reply to a [`Prepare`] (§4.8 "reports the highest record it holds for each object"): the
/// promising holder, the object, the epoch it promised (echoing the prepare so the reply binds to it),
/// the generation, and the highest [`Accepted`] record it holds for the object — or `None` if it holds
/// nothing. The new owner counts a promise toward its phase-one quorum only if it [`binds`](Promise::binds)
/// to the prepare and comes from a distinct candidate, so a fenced, foreign or wrong-object reply
/// cannot manufacture a promotion quorum (the same discipline `commit_over_holders` uses for records).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Promise {
  /// The holder that raised its fence and reports its highest record.
  pub holder: HostId,
  /// The object the promise is for.
  pub object: ObjectId,
  /// The epoch the holder promised (echoes the prepare's epoch).
  pub epoch: HostEpoch,
  /// The generation the holder is serving under.
  pub generation: u64,
  /// The holder's highest accepted record for the object, or `None` if it holds nothing for it.
  pub highest: Option<Accepted>,
}

/// A promise's fixed prefix: holder (u64 LE), object (16 bytes), epoch, generation (each u64 LE), then
/// a one-byte flag (whether a highest record follows); a present record adds its sequence and epoch
/// (u64 LE), its value length (u32 LE) and the value bytes.
/// Format: §4.8 promise layout; a record is MTU-shippable, so the value length is a `u32`.
const PROMISE_PREFIX_BYTES: usize = 3 * size_of::<u64>() + OBJECT_BYTES + 1;
/// Format: the promise flag values — no highest record, or one follows.
const PROMISE_NONE: u8 = 0;
/// Format: a highest record follows the flag.
const PROMISE_SOME: u8 = 1;

impl Promise {
  /// Whether this promise answers `prepare` — the object, epoch and generation all match, so a reply
  /// for a different prepare (object, epoch or generation) cannot be counted for this one.
  pub fn binds(&self, prepare: &Prepare) -> bool {
    self.object == prepare.object
      && self.epoch == prepare.epoch
      && self.generation == prepare.generation
  }

  /// The canonical bytes: the header words, the highest-record flag, then the record if present.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(PROMISE_PREFIX_BYTES);
    out.extend_from_slice(&self.holder.0.to_le_bytes());
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.generation.to_le_bytes());
    match &self.highest {
      None => out.push(PROMISE_NONE),
      Some(accepted) => {
        out.push(PROMISE_SOME);
        out.extend_from_slice(&accepted.sequence.to_le_bytes());
        out.extend_from_slice(&accepted.epoch.0.to_le_bytes());
        let value_len = u32::try_from(accepted.value.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&value_len.to_le_bytes());
        out.extend_from_slice(&accepted.value);
      }
    }
    out
  }

  /// Reconstructs a promise from its bytes, checking every length against what remains before reading,
  /// so a truncated or over-claiming promise is a typed [`RegisterError::MalformedRecord`], never a
  /// panic or an over-read (the hostile-input rule; this parses bytes that crossed the network).
  pub fn decode(bytes: &[u8]) -> Result<Promise, RegisterError> {
    if bytes.len() < PROMISE_PREFIX_BYTES {
      return Err(RegisterError::MalformedRecord);
    }
    let word = |slice: &[u8]| u64::from_le_bytes(slice.try_into().unwrap_or([0; 8]));
    let (holder_bytes, rest) = bytes.split_at(size_of::<u64>());
    let (object_bytes, rest) = rest.split_at(OBJECT_BYTES);
    let (epoch_bytes, rest) = rest.split_at(size_of::<u64>());
    let (generation_bytes, rest) = rest.split_at(size_of::<u64>());
    let (&flag, rest) = rest.split_first().unwrap_or((&PROMISE_NONE, &[]));
    let highest = match flag {
      PROMISE_NONE => {
        if !rest.is_empty() {
          return Err(RegisterError::MalformedRecord);
        }
        None
      }
      PROMISE_SOME => {
        // A present record needs its sequence and epoch (two u64) and a u32 value length.
        let fixed = 2 * size_of::<u64>() + size_of::<u32>();
        if rest.len() < fixed {
          return Err(RegisterError::MalformedRecord);
        }
        let (sequence_bytes, rest) = rest.split_at(size_of::<u64>());
        let (accepted_epoch_bytes, rest) = rest.split_at(size_of::<u64>());
        let (len_bytes, value_bytes) = rest.split_at(size_of::<u32>());
        let value_len = u32::from_le_bytes(len_bytes.try_into().unwrap_or([0; 4]));
        let value_len = usize::try_from(value_len).unwrap_or(usize::MAX);
        if value_bytes.len() != value_len {
          return Err(RegisterError::MalformedRecord);
        }
        Some(Accepted {
          sequence: word(sequence_bytes),
          epoch: HostEpoch(word(accepted_epoch_bytes)),
          value: value_bytes.to_vec(),
        })
      }
      _ => return Err(RegisterError::MalformedRecord),
    };
    Ok(Promise {
      holder: HostId(word(holder_bytes)),
      object: object_from(object_bytes),
      epoch: HostEpoch(word(epoch_bytes)),
      generation: word(generation_bytes),
      highest,
    })
  }
}

/// The per-position acceptor a holder runs (§4.8 "distinguish a holder's promised epoch from its
/// accepted `(epoch, value)`"): the holder's id and authority, the highest epoch it has promised (the
/// fence), and the value it has accepted at each ledger position. Acceptance is synchronous and
/// recoverable — the accepted value and the promise are recorded *before* an acknowledgement is
/// returned, and a restart reconstructs an acceptor from them ([`Acceptor::recovered`]) — so an
/// acknowledgement always reflects a stored record (network receipt is not acceptance). db owns this
/// synchronous acceptance; the cluster plane wraps it in asynchronous dispatch.
pub struct Acceptor {
  id: HostId,
  authority: Authority,
  fence: Fence,
  accepted: std::collections::BTreeMap<(ObjectId, u64), (HostEpoch, Vec<u8>)>,
}

/// A holder's durable accepted positions, for recovery: `(object, sequence, epoch, value)` per entry.
pub type AcceptedPositions = Vec<(ObjectId, u64, HostEpoch, Vec<u8>)>;

impl Acceptor {
  /// A fresh acceptor for holder `id` serving under `authority`, having accepted nothing.
  pub fn new(id: HostId, authority: Authority) -> Acceptor {
    Acceptor {
      id,
      authority,
      fence: Fence::new(),
      accepted: std::collections::BTreeMap::new(),
    }
  }

  /// An acceptor recovered after a restart from its persisted promise and accepted positions — the
  /// state a real holder writes to anchor-owned RAM before acknowledging (§4.8 "persist effect and
  /// completion as one recoverable publication"). Acknowledged records and the fence survive.
  pub fn recovered(
    id: HostId,
    authority: Authority,
    promised: HostEpoch,
    accepted: AcceptedPositions,
  ) -> Acceptor {
    let mut fence = Fence::new();
    let _ = fence.accept(promised);
    let accepted = accepted
      .into_iter()
      .map(|(object, sequence, epoch, value)| ((object, sequence), (epoch, value)))
      .collect();
    Acceptor {
      id,
      authority,
      fence,
      accepted,
    }
  }

  /// This acceptor's durable state (the promise and the accepted positions), for a restart to recover.
  pub fn persisted(&self) -> (HostEpoch, AcceptedPositions) {
    let accepted = self
      .accepted
      .iter()
      .map(|(&(object, sequence), (epoch, value))| (object, sequence, *epoch, value.clone()))
      .collect();
    (self.fence.seen, accepted)
  }

  /// Accepts `record` under this holder's authority and fence, storing it before acknowledging, or
  /// refusing with a typed reason. The order is authority (a foreign generation or unauthorized owner
  /// refuses before touching the fence), then the fence (a stale epoch refuses), then the position (a
  /// *different* value at a position already accepted is a conflict — a committed position is never
  /// rewritten; the *same* value re-acknowledges idempotently). On acceptance the promise is raised and
  /// the accepted `(epoch, value)` recorded — recording the new epoch even when the value is unchanged
  /// (BUG-12) — before the [`Ack`] is returned.
  pub fn accept(&mut self, record: &Record) -> Result<Ack, RegisterError> {
    if record.generation != self.authority.generation {
      return Err(RegisterError::ForeignGeneration {
        current: self.authority.generation,
      });
    }
    if record.owner != self.authority.owner {
      return Err(RegisterError::Unauthorized);
    }
    self.fence.accept(record.epoch)?;
    let position = (record.object, record.sequence);
    if let Some((_, existing)) = self.accepted.get(&position)
      && existing != &record.value
    {
      return Err(RegisterError::ConflictingPosition);
    }
    // Store the accepted value and the raised promise before acknowledging.
    self
      .accepted
      .insert(position, (record.epoch, record.value.clone()));
    Ok(Ack {
      holder: self.id,
      object: record.object,
      sequence: record.sequence,
      generation: record.generation,
      identity: record.identity(),
    })
  }

  /// Installs a new configuration authority — the holder applying the configuration update the regional
  /// group distributed on a takeover or reconfiguration (§4.8 "the configuration is written only through
  /// the regional group, read by every node from its local copy"). A generation below the holder's
  /// current one is a stale configuration and is refused ([`RegisterError::ForeignGeneration`]); the
  /// current or a newer one is adopted, so the new owner becomes the sole authorized writer and the old
  /// owner is fenced by generation (its records are now [`Unauthorized`](RegisterError::Unauthorized)).
  /// The accepted records are kept — they are exactly the state phase one reads — only the authority
  /// changes; the epoch fence is raised separately, by the [`prepare`](Acceptor::prepare) round.
  pub fn install_authority(&mut self, authority: Authority) -> Result<(), RegisterError> {
    if authority.generation < self.authority.generation {
      return Err(RegisterError::ForeignGeneration {
        current: self.authority.generation,
      });
    }
    self.authority = authority;
    Ok(())
  }

  /// Answers a new owner's phase-one [`Prepare`] (§4.8 "Promotion and takeover"): raises this holder's
  /// fence for the object's host to the prepare's epoch and reports the highest record it holds for the
  /// object, so the new owner can adopt the newest across a quorum before it serves. The checks mirror
  /// [`accept`](Acceptor::accept): a prepare under a generation the holder has not installed is
  /// [`ForeignGeneration`](RegisterError::ForeignGeneration); one from a principal the installed
  /// authority does not name as owner is [`Unauthorized`](RegisterError::Unauthorized); an epoch below
  /// the fence (a newer takeover already promised higher) is [`StaleEpoch`](RegisterError::StaleEpoch).
  /// On success the fence is raised **before** the promise is returned, so a resumed stale owner writing
  /// under the old epoch can no longer commit (`StaleNeverCommits`). The reported record is the highest
  /// by position then epoch, which is at least as new as anything that ever committed under the old
  /// epoch, because a phase-one quorum and every phase-two commit quorum are both `f + 1` of `2f + 1`
  /// and so intersect.
  pub fn prepare(&mut self, prepare: &Prepare) -> Result<Promise, RegisterError> {
    if prepare.generation != self.authority.generation {
      return Err(RegisterError::ForeignGeneration {
        current: self.authority.generation,
      });
    }
    if prepare.owner != self.authority.owner {
      return Err(RegisterError::Unauthorized);
    }
    self.fence.accept(prepare.epoch)?;
    let highest = self
      .accepted
      .iter()
      .filter_map(|(position, stored)| {
        let &(object, sequence) = position;
        let (epoch, value) = stored;
        (object == prepare.object).then(|| Accepted {
          sequence,
          epoch: *epoch,
          value: value.clone(),
        })
      })
      .reduce(|best, next| if next.newer_than(&best) { next } else { best });
    Ok(Promise {
      holder: self.id,
      object: prepare.object,
      epoch: prepare.epoch,
      generation: self.authority.generation,
      highest,
    })
  }
}

/// A candidate holder of an object's records (§4.8 "2f+1 candidate holders, the owner among them"). A
/// real holder is reached over the session plane's RPC; the owner's own hold is local. `deliver`
/// applies the record through the holder's [`Acceptor`] and returns a binding [`Ack`] on acceptance,
/// or a typed refusal (a refused record does not acknowledge, so it is not counted). This is the seam
/// the transport plugs a remote holder into (the cluster plane); the register protocol stays sans-io.
pub trait Holder {
  /// Delivers `record` to this holder, returning its binding acknowledgement if it accepts
  /// (authorized, under a promise-respecting epoch, no conflicting value at the position) or refusing.
  fn deliver(&mut self, record: &Record) -> Result<Ack, RegisterError>;
}

impl Holder for Acceptor {
  fn deliver(&mut self, record: &Record) -> Result<Ack, RegisterError> {
    self.accept(record)
  }
}

/// Ships `record` to the `candidates`' `holders` (the owner's local hold among them) and returns the
/// resulting [`Placement`] — the candidates and the distinct subset that acknowledged *this* record.
/// The write commits when the placement is `placed(quorum)` (`f + 1` distinct candidate
/// acknowledgements, §4.8 "one quorum rule"). An acknowledgement is counted only if it **binds** to the
/// record shipped (position, generation and identity), comes from an actual candidate, and is not a
/// duplicate — so a fenced, foreign, stale or wrong-record reply cannot pad the quorum. At `f = 0`
/// there is one holder, the owner, and its local hold is the commit — the same code path (R8).
pub fn commit_over_holders(
  candidates: &[HostId],
  record: &Record,
  holders: &mut [&mut dyn Holder],
) -> Placement {
  let mut acked: Vec<HostId> = Vec::new();
  for holder in holders.iter_mut() {
    if let Ok(ack) = holder.deliver(record)
      && ack.binds(record)
      && candidates.contains(&ack.holder)
      && !acked.contains(&ack.holder)
    {
      acked.push(ack.holder);
    }
  }
  Placement {
    candidates: candidates.to_vec(),
    acked,
    mirror_acked: None,
  }
}

/// A candidate holder answering a new owner's phase-one [`Prepare`] (§4.8 "Promotion and takeover").
/// The seam the transport plugs a remote holder into for the promotion round (the cluster plane); the
/// owner's own hold is local. `promise` raises the holder's fence and reports its highest record, or
/// refuses (a foreign generation, an unauthorized owner, a stale epoch) — a refusal is not a promise
/// and is not counted toward the promotion quorum. Every [`Acceptor`] is a promoter.
pub trait Promoter {
  /// Answers `prepare`, returning this holder's [`Promise`] if it accepts the promotion (installed
  /// generation, authorized new owner, epoch at or above its fence) or a typed refusal.
  fn promise(&mut self, prepare: &Prepare) -> Result<Promise, RegisterError>;
}

impl Promoter for Acceptor {
  fn promise(&mut self, prepare: &Prepare) -> Result<Promise, RegisterError> {
    self.prepare(prepare)
  }
}

/// The result of a phase-one promotion round (§4.8 "Promotion and takeover"): the distinct candidates
/// that promised, and the newest record adopted across them. The promotion is safe to serve only when
/// [`promised`](Promotion::promised) is a quorum (`f + 1` distinct candidates); [`adopted`](Promotion::adopted)
/// is then at least as new as anything that ever committed under the old epoch. `adopted` is `None`
/// when no promising holder held a record for the object — the object had no committed head to inherit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Promotion {
  /// The distinct candidates that raised their fence and promised (counted once each).
  pub promised: Vec<HostId>,
  /// The newest record adopted across the promising quorum, or `None` if none held one.
  pub adopted: Option<Accepted>,
}

impl Promotion {
  /// Whether a quorum of distinct candidates promised, so the adoption is safe to serve under the new
  /// epoch (`f + 1` promises intersect every prior `f + 1` commit, so `adopted` covers the committed
  /// prefix). Below a quorum the new owner must retry against more holders before serving.
  pub fn promoted(&self, quorum: Quorum) -> bool {
    quorum.committed(self.promised.len())
  }

  /// The record the new owner re-commits to complete safe adoption (§4.8 "completes safe adoption under
  /// the new epoch"): the adopted value at its position, re-stamped with the new owner, the prepare's
  /// epoch and generation, so a phase-two [`commit_over_holders`] records the new epoch even though the
  /// bytes are unchanged (BUG-12) and fences anything older. `None` when there was nothing to adopt.
  pub fn adoption_record(&self, prepare: &Prepare) -> Option<Record> {
    self.adopted.as_ref().map(|accepted| Record {
      owner: prepare.owner,
      object: prepare.object,
      sequence: accepted.sequence,
      epoch: prepare.epoch,
      generation: prepare.generation,
      value: accepted.value.clone(),
    })
  }
}

/// Runs a new owner's phase-one round: sends `prepare` to the `candidates`' `holders` (the owner's own
/// hold among them) and returns the [`Promotion`] — the distinct candidates that promised and the
/// newest record adopted across them (§4.8 "each new owner runs phase one in one batched round … adopts
/// the newest reported record per object"). A promise is counted only if it **binds** to the prepare
/// (object, epoch and generation), comes from an actual candidate, and is not a duplicate — so a fenced,
/// foreign or wrong-object reply cannot manufacture a promotion quorum, the same discipline
/// [`commit_over_holders`] applies to acknowledgements. At `f = 0` there is one holder, the owner, and
/// its own highest record is the adoption — the same code path (R8).
pub fn promote_over_holders(
  candidates: &[HostId],
  prepare: &Prepare,
  holders: &mut [&mut dyn Promoter],
) -> Promotion {
  let mut promised: Vec<HostId> = Vec::new();
  let mut adopted: Option<Accepted> = None;
  for holder in holders.iter_mut() {
    if let Ok(promise) = holder.promise(prepare)
      && promise.binds(prepare)
      && candidates.contains(&promise.holder)
      && !promised.contains(&promise.holder)
    {
      promised.push(promise.holder);
      if let Some(reported) = promise.highest {
        let keep = match &adopted {
          Some(best) => reported.newer_than(best),
          None => true,
        };
        if keep {
          adopted = Some(reported);
        }
      }
    }
  }
  Promotion { promised, adopted }
}

/// The configuration oracle (§4.8 "Configuration, by consensus"): membership, the fault
/// tolerance, and the version every request carries. On a laptop it is one self-acknowledging
/// voter whose version never advances; in a fleet the regional group writes it and a request
/// under a stale version is refused with the current one. It is read per request and written
/// only on membership, takeover, neighbourhood and home changes, never per write (D-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
  /// The configuration version; a request carrying a lower one is refused.
  pub version: u64,
  /// This host.
  pub owner: HostId,
  /// This host's current epoch (its authority).
  pub host_epoch: HostEpoch,
  /// The neighbourhood the owner's candidates are drawn from (the owner alone on a laptop).
  pub neighbourhood: Vec<HostId>,
  /// The quorum the fault-domain tree fixes (`f = 0` on a laptop).
  pub quorum: Quorum,
  /// Whether a mirror region exists.
  pub has_mirror: bool,
}

impl Configuration {
  /// The one-node configuration: `f = 0`, one member, one voter, no mirror, version zero
  /// (§4.8 "Laptop degenerate"). The same type a fleet uses; only the numbers differ.
  pub fn solo(owner: HostId) -> Configuration {
    Configuration {
      version: 0,
      owner,
      host_epoch: FIRST_EPOCH,
      neighbourhood: vec![owner],
      quorum: Quorum { f: 0 },
      has_mirror: false,
    }
  }

  /// Refuses a request whose configuration version is not the current one, with the current
  /// version so one retry succeeds (§4.8 "the configuration is versioned").
  pub fn check_version(&self, carried: u64) -> Result<(), RegisterError> {
    if carried != self.version {
      return Err(RegisterError::ConfigurationStale {
        version: self.version,
      });
    }
    Ok(())
  }

  /// The placement an object takes when the owner first holds it (`Placement::local`).
  pub fn place(&self, object: ObjectId) -> Placement {
    Placement::local(self.owner, self.quorum, &self.neighbourhood, object)
  }

  /// Whether the region scope is placed for `placement` under this configuration.
  pub fn region_placed(&self, placement: &Placement) -> bool {
    placement.placed(self.quorum)
  }

  /// The result of `await placed(scope)` (§4.8 D-18): the region commit is the local append at
  /// `f = 0`, already placed; the mirror is refused where none exists.
  pub fn await_placed(
    &self,
    scope: DurabilityScope,
    placement: &Placement,
  ) -> Result<bool, RegisterError> {
    match scope {
      DurabilityScope::Region => Ok(self.region_placed(placement)),
      DurabilityScope::Mirror => {
        if self.has_mirror {
          Ok(placement.placed_mirror(self.quorum))
        } else {
          Err(RegisterError::Unsupported { scope })
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A record authorized under `config`, at `object`/`sequence` and host `epoch`, carrying `value`.
  fn record_under(
    config: &Configuration,
    object: u64,
    sequence: u64,
    epoch: HostEpoch,
    value: &[u8],
  ) -> Record {
    Record {
      owner: config.owner,
      // The object id carries its creator (the owner) in its high half; the `object` argument is the
      // per-creator suffix, so a test names an object by a small number as before.
      object: ObjectId::new(config.owner, object),
      sequence,
      epoch,
      generation: config.version,
      value: value.to_vec(),
    }
  }

  /// Runs a register write over `config`'s candidates through `commit_over_holders` with a real
  /// [`Acceptor`] per candidate (the same seam a transport-backed holder plugs into), and returns the
  /// placement observed. The owner's holder is local; peers are distinct logical holders — the same
  /// code at f=0 (one holder) and f=1 (three).
  fn write(config: &Configuration, object: u64, epoch: HostEpoch) -> Placement {
    let object_id = ObjectId::new(config.owner, object);
    let candidates = candidates_for(
      config.owner,
      &config.neighbourhood,
      object_id,
      config.quorum,
    );
    let authority = Authority {
      generation: config.version,
      owner: config.owner,
    };
    let record = record_under(config, object, 0, epoch, b"value");
    let mut holders: Vec<Acceptor> = candidates
      .iter()
      .map(|h| Acceptor::new(*h, authority))
      .collect();
    let mut refs: Vec<&mut dyn Holder> = holders.iter_mut().map(|h| h as &mut dyn Holder).collect();
    commit_over_holders(&candidates, &record, &mut refs)
  }

  /// AC-2.5 (the register slice): the observable outcome of a write — placed or not, and the
  /// count that committed it — is the same at f=0 (the laptop) and at a simulated f=1, because
  /// it is the same code with a different `f`. The laptop commits at one, the fleet at two,
  /// and both report `placed` true.
  #[test]
  fn the_commit_rule_is_the_same_code_at_f0_and_f1() {
    let owner = HostId(1);
    let laptop = Configuration::solo(owner);
    let fleet = Configuration {
      version: 0,
      owner,
      host_epoch: FIRST_EPOCH,
      neighbourhood: vec![owner, HostId(2), HostId(3), HostId(4)],
      quorum: Quorum { f: 1 },
      has_mirror: false,
    };
    for object in 0..64u64 {
      let laptop_place = write(&laptop, object, FIRST_EPOCH);
      assert_eq!(laptop_place.candidates, vec![owner], "f=0: the owner alone");
      assert!(laptop_place.placed(laptop.quorum), "f=0: placed at one");

      let fleet_place = write(&fleet, object, FIRST_EPOCH);
      assert_eq!(fleet_place.candidates.len(), 3, "f=1: three candidates");
      assert_eq!(
        fleet_place.candidates[0], owner,
        "the owner is always a candidate"
      );
      assert!(fleet_place.placed(fleet.quorum), "f=1: placed at two");
      // The observable is identical: both are placed. The differential is the count, which is
      // the quorum's own derivation, not a mode.
      assert_eq!(
        laptop_place.placed(laptop.quorum),
        fleet_place.placed(fleet.quorum)
      );
    }
  }

  /// A record under a stale host epoch is refused at both f=0 and f=1 (`StaleNeverCommits`):
  /// the fence is the same code; on a laptop it fences the owner's own resumed writes after a
  /// (never-occurring) bump, and the interface is present so a takeover in Phase 8 changes
  /// nothing.
  #[test]
  fn a_stale_epoch_is_refused_at_every_f() {
    let mut fence = Fence::new();
    assert!(fence.accept(HostEpoch(2)).is_ok());
    assert_eq!(
      fence.accept(HostEpoch(1)),
      Err(RegisterError::StaleEpoch { current: 2 }),
      "a lower epoch never commits"
    );
    assert!(fence.accept(HostEpoch(2)).is_ok(), "the same epoch renews");
    assert!(
      fence.accept(HostEpoch(3)).is_ok(),
      "a higher epoch raises the fence"
    );
  }

  /// The configuration version fences a stale request with the current version; at f=0 the
  /// version never advances, so the request always matches (one voter).
  #[test]
  fn a_stale_configuration_version_is_refused_with_the_current_one() {
    let config = Configuration::solo(HostId(1));
    assert!(
      config.check_version(0).is_ok(),
      "the laptop's version is stable"
    );
    let advanced = Configuration {
      version: 7,
      ..config.clone()
    };
    assert_eq!(
      advanced.check_version(3),
      Err(RegisterError::ConfigurationStale { version: 7 })
    );
    assert!(advanced.check_version(7).is_ok());
  }

  /// A record round-trips through its canonical bytes, and a truncated or over-claiming one is a
  /// typed `MalformedRecord` (hostile input; a record crosses the network). A golden vector pins the
  /// encoding so a change is caught across versions.
  #[test]
  fn a_record_round_trips_and_refuses_malformation() {
    let record = Record {
      owner: HostId(1),
      object: ObjectId::new(HostId(1), 2),
      sequence: 3,
      epoch: HostEpoch(4),
      generation: 5,
      value: b"v".to_vec(),
    };
    let bytes = record.encode();
    assert_eq!(Record::decode(&bytes), Ok(record.clone()), "round-trip");

    // Golden: owner (8 LE), object (16: creator then suffix, big-endian), sequence, epoch,
    // generation (each 8 LE), value_len (4 LE), value.
    let mut golden = Vec::new();
    golden.extend_from_slice(&1u64.to_le_bytes());
    golden.extend_from_slice(&ObjectId::new(HostId(1), 2).0);
    for word in [3u64, 4, 5] {
      golden.extend_from_slice(&word.to_le_bytes());
    }
    golden.extend_from_slice(&1u32.to_le_bytes());
    golden.push(b'v');
    assert_eq!(bytes, golden, "the canonical encoding is stable");

    // Hostile: a truncated prefix, and a value length past what arrived.
    assert_eq!(
      Record::decode(&bytes[..4]),
      Err(RegisterError::MalformedRecord)
    );
    assert_eq!(
      Record::decode(&bytes[..bytes.len() - 1]),
      Err(RegisterError::MalformedRecord),
      "a value shorter than its declared length is refused"
    );
  }

  /// A holder that fences the record (a stale epoch) does not acknowledge, so it is not counted toward
  /// the commit: at f=1 with only the owner accepting and both peers having promised a higher epoch,
  /// the write is **not** placed (one ack, commit needs two) — StaleNeverCommits, over the real
  /// ship-and-collect path.
  #[test]
  fn a_fencing_holder_is_not_counted_toward_the_commit() {
    let owner = HostId(1);
    let generation = 0u64;
    let authority = Authority { generation, owner };
    let quorum = Quorum { f: 1 };
    let candidates = vec![owner, HostId(2), HostId(3)];
    let record = Record {
      owner,
      object: ObjectId::new(HostId(1), 9),
      sequence: 0,
      epoch: HostEpoch(5),
      generation,
      value: b"v".to_vec(),
    };

    // The owner accepts; the two peers have already promised epoch 6, so they fence this epoch-5 record.
    let mut owner_holder = Acceptor::new(owner, authority);
    let mut peer_two = Acceptor::recovered(HostId(2), authority, HostEpoch(6), Vec::new());
    let mut peer_three = Acceptor::recovered(HostId(3), authority, HostEpoch(6), Vec::new());
    let mut refs: Vec<&mut dyn Holder> = vec![&mut owner_holder, &mut peer_two, &mut peer_three];

    let placement = commit_over_holders(&candidates, &record, &mut refs);
    assert_eq!(placement.acked, vec![owner], "only the owner acknowledged");
    assert!(
      !placement.placed(quorum),
      "one acknowledgement does not commit at f=1; the fenced peers are not counted"
    );
  }

  /// A holder returning whatever acknowledgement it is told to — a forged, foreign, duplicate or
  /// wrong-record reply — so the counting rule can be tested against an adversary.
  struct LyingHolder {
    reply: Ack,
  }
  impl Holder for LyingHolder {
    fn deliver(&mut self, _record: &Record) -> Result<Ack, RegisterError> {
      Ok(self.reply)
    }
  }

  /// AC (acceptance history 2): duplicate, foreign and wrong-record acknowledgements cannot manufacture
  /// a quorum. At f=1 only the owner truly accepts; adversarial holders claim a duplicate of the owner,
  /// a foreign id, and a reply bound to a different record — none is counted, so the write is not placed.
  #[test]
  fn forged_acks_cannot_manufacture_quorum() {
    let owner = HostId(1);
    let generation = 0u64;
    let authority = Authority { generation, owner };
    let quorum = Quorum { f: 1 };
    let candidates = vec![owner, HostId(2), HostId(3)];
    let record = Record {
      owner,
      object: ObjectId::new(HostId(1), 9),
      sequence: 0,
      epoch: HostEpoch(5),
      generation,
      value: b"v".to_vec(),
    };
    let good_ack = Ack {
      holder: owner,
      object: ObjectId::new(HostId(1), 9),
      sequence: 0,
      generation,
      identity: record.identity(),
    };

    let mut owner_holder = Acceptor::new(owner, authority);
    // A holder claiming the owner's id again (a duplicate), and one bound to a different record.
    let mut duplicate = LyingHolder { reply: good_ack };
    let mut wrong_record = LyingHolder {
      reply: Ack {
        holder: HostId(2),
        identity: [0xff; 32],
        ..good_ack
      },
    };
    let mut refs: Vec<&mut dyn Holder> = vec![&mut owner_holder, &mut duplicate, &mut wrong_record];
    let placement = commit_over_holders(&candidates, &record, &mut refs);
    assert_eq!(
      placement.acked,
      vec![owner],
      "only the owner's genuine ack counts"
    );
    assert!(
      !placement.placed(quorum),
      "a duplicate of the owner and a wrong-record reply cannot manufacture the second ack"
    );

    // A foreign id (not a candidate) is likewise uncounted.
    let mut foreign = LyingHolder {
      reply: Ack {
        holder: HostId(99),
        ..good_ack
      },
    };
    let mut refs: Vec<&mut dyn Holder> = vec![&mut foreign];
    let placement = commit_over_holders(&candidates, &record, &mut refs);
    assert!(
      placement.acked.is_empty(),
      "a non-candidate id is not counted"
    );
  }

  /// AC (§4.13/§4.8): a record from a principal the authority does not authorize, or under a foreign
  /// generation, is refused — there is no unauthenticated acceptance.
  #[test]
  fn authority_refuses_wrong_owner_and_generation() {
    let owner = HostId(1);
    let authority = Authority {
      generation: 7,
      owner,
    };
    let mut acceptor = Acceptor::new(HostId(2), authority);

    let foreign_owner = Record {
      owner: HostId(42),
      object: ObjectId::new(HostId(1), 1),
      sequence: 0,
      epoch: HostEpoch(1),
      generation: 7,
      value: b"x".to_vec(),
    };
    assert_eq!(
      acceptor.accept(&foreign_owner),
      Err(RegisterError::Unauthorized)
    );

    let foreign_generation = Record {
      owner,
      object: ObjectId::new(HostId(1), 1),
      sequence: 0,
      epoch: HostEpoch(1),
      generation: 6,
      value: b"x".to_vec(),
    };
    assert_eq!(
      acceptor.accept(&foreign_generation),
      Err(RegisterError::ForeignGeneration { current: 7 })
    );
  }

  /// AC (acceptance history 5): a restarted holder preserves its acknowledged records and fence — a
  /// re-delivery of the same record re-acknowledges idempotently, a *different* value at that committed
  /// position is refused, and an epoch below the recovered promise is still fenced.
  #[test]
  fn a_restarted_holder_preserves_acceptance_and_refuses_conflict() {
    let owner = HostId(1);
    let authority = Authority {
      generation: 0,
      owner,
    };
    let mut acceptor = Acceptor::new(HostId(2), authority);
    let record = Record {
      owner,
      object: ObjectId::new(HostId(1), 5),
      sequence: 0,
      epoch: HostEpoch(3),
      generation: 0,
      value: b"committed".to_vec(),
    };
    let first = acceptor.accept(&record).unwrap();

    // Restart: recover a fresh acceptor from the persisted promise and accepted positions.
    let (promised, accepted) = acceptor.persisted();
    let mut recovered = Acceptor::recovered(HostId(2), authority, promised, accepted);

    // The same record re-acknowledges identically (idempotent recovery of the same result).
    assert_eq!(
      recovered.accept(&record),
      Ok(first),
      "the acknowledged record survived the restart"
    );

    // A different value at the same position is refused — a committed position is never rewritten.
    let conflicting = Record {
      value: b"different".to_vec(),
      ..record.clone()
    };
    assert_eq!(
      recovered.accept(&conflicting),
      Err(RegisterError::ConflictingPosition)
    );

    // The recovered promise still fences a lower epoch.
    let stale = Record {
      sequence: 1,
      epoch: HostEpoch(2),
      ..record.clone()
    };
    assert_eq!(
      recovered.accept(&stale),
      Err(RegisterError::StaleEpoch { current: 3 })
    );
  }

  /// `await placed(region)` is the local append at f=0; `await placed(mirror)` is refused
  /// where no mirror exists, and both interfaces are present from the first version.
  #[test]
  fn await_placed_returns_for_the_region_and_refuses_the_absent_mirror() {
    let config = Configuration::solo(HostId(1));
    let placement = config.place(ObjectId::new(config.owner, 42));
    assert_eq!(
      config.await_placed(DurabilityScope::Region, &placement),
      Ok(true),
      "the region commit is the local append"
    );
    assert_eq!(
      config.await_placed(DurabilityScope::Mirror, &placement),
      Err(RegisterError::Unsupported {
        scope: DurabilityScope::Mirror
      }),
      "no mirror on a laptop"
    );
  }

  /// Rendezvous placement is deterministic and owner-first, and spreads objects across the
  /// neighbourhood (a non-vacuity check that the ranking is not constant).
  #[test]
  fn rendezvous_candidates_are_deterministic_owner_first_and_spread() {
    let owner = HostId(1);
    let neigh = vec![owner, HostId(2), HostId(3), HostId(4), HostId(5)];
    let quorum = Quorum { f: 2 };
    let mut seconds = std::collections::BTreeSet::new();
    for local in 0..256u64 {
      let object = ObjectId::new(owner, local);
      let a = candidates_for(owner, &neigh, object, quorum);
      let b = candidates_for(owner, &neigh, object, quorum);
      assert_eq!(a, b, "deterministic");
      assert_eq!(a.len(), 5, "2f+1 = 5");
      assert_eq!(a[0], owner, "owner first");
      seconds.insert(a[1]);
    }
    assert!(
      seconds.len() > 1,
      "objects spread over more than one second holder"
    );
  }

  /// The rendezvous-first survivor is deterministic, empty-safe, spreads across the survivors, and
  /// agrees with the ranking `candidates_for` uses — takeover picks the same host the placement would
  /// rank first among the survivors.
  #[test]
  fn rendezvous_first_is_deterministic_spreads_and_matches_the_candidate_ranking() {
    assert_eq!(
      rendezvous_first(&[], ObjectId::new(HostId(1), 42)),
      None,
      "an empty survivor set has no successor"
    );

    let owner = HostId(1);
    let neigh = vec![owner, HostId(2), HostId(3), HostId(4), HostId(5)];
    let survivors: Vec<HostId> = neigh.iter().copied().filter(|h| *h != owner).collect();
    let quorum = Quorum { f: 2 };
    let mut winners = std::collections::BTreeSet::new();
    for local in 0..256u64 {
      let object = ObjectId::new(owner, local);
      let first = rendezvous_first(&survivors, object).expect("a survivor");
      assert_eq!(
        rendezvous_first(&survivors, object),
        Some(first),
        "deterministic"
      );
      assert!(survivors.contains(&first), "the winner is a survivor");
      // The owner's first *other* candidate for this object is exactly the rendezvous-first survivor.
      assert_eq!(
        candidates_for(owner, &neigh, object, quorum)[1],
        first,
        "agrees with the candidate ranking"
      );
      winners.insert(first);
    }
    assert!(
      winners.len() > 1,
      "the successor spreads over more than one survivor"
    );
  }

  /// AC (§4.8 "Placement"; `research/metadata-replication.md` §3.1): the copyset-count formulas match the
  /// Copysets paper's own worked examples exactly — the oracle for the placement math [A: Cidon et al., ATC
  /// 2013]. The fixed-copyset count is orders of magnitude below the random one at the same parameters (the
  /// non-vacuity the whole bounding rests on).
  #[test]
  fn the_copyset_counts_match_the_copysets_paper() {
    // Paper §4: N=9, R=3, S=4 → two permutations of three → 6 copysets; random → 9·C(4,2) = 54.
    assert_eq!(
      copyset_count(9, 4, 3),
      6,
      "minimal scheme, paper's small example"
    );
    assert_eq!(
      random_copyset_count(9, 4, 3),
      54,
      "random scheme, paper's small example"
    );
    // Paper §3: N=5000, R=3, S=10 → ⌈10/2⌉·5000/3 = 8333 ("about 8,300").
    assert_eq!(copyset_count(5000, 10, 3), 8333, "paper's large example");
    // The laptop degenerate: no redundancy, each host its own copyset.
    assert_eq!(copyset_count(1, 0, 1), 1, "one host, one copyset");
  }

  /// AC (§4.8): the loss-probability formula reproduces the paper's headline figures for Facebook's HDFS
  /// (R=3, S=10, 1% of the nodes failing coincidentally): Copyset Replication ≈ 0.78 %, random replication
  /// ≈ 22.8 % — so bounding the copysets cuts the loss by ~30×. This is the durability the scatter-width
  /// derivation buys.
  #[test]
  fn the_loss_probability_reproduces_the_paper_facebook_figures() {
    let (hosts, copies, failed) = (5000, 3, 50); // 1% of 5000
    let copyset =
      coincident_loss_probability(copyset_count(hosts, 10, copies), hosts, failed, copies);
    let random = coincident_loss_probability(
      random_copyset_count(hosts, 10, copies),
      hosts,
      failed,
      copies,
    );
    assert!(
      (0.006..0.010).contains(&copyset),
      "Copyset Replication ≈ 0.78 % (got {copyset})"
    );
    assert!(
      (0.18..0.26).contains(&random),
      "random replication ≈ 22.8 % (got {random})"
    );
    assert!(
      random > copyset * 20.0,
      "bounding the copysets cuts the loss by more than an order of magnitude"
    );
    // Fewer than R coincident failures cannot lose a full copyset.
    assert_eq!(
      coincident_loss_probability(copyset_count(hosts, 10, copies), hosts, 2, copies),
      0.0
    );
  }

  /// AC (§4.8 "Placement"): the scatter width is the recovery-parallelism floor, never below the candidate
  /// floor `2f+1`, and unknown recovery inputs fall back to that floor. `S = max(2f+1, ⌈D/(B·T)⌉)`.
  #[test]
  fn the_scatter_width_is_the_recovery_floor_above_the_candidate_floor() {
    // 100 GB to restore, 1 GB/s, a 10 s budget → ⌈100/(1·10)⌉ = 10 nodes; f=1 floor is 3, so recovery wins.
    let gb: u64 = 1 << 30;
    let s = scatter_width(100 * gb, gb, 10 * 1_000_000_000, 1);
    assert_eq!(s, 10, "recovery parallelism dominates the candidate floor");
    // A tiny host: recovery needs one node, but the candidate floor 2f+1 = 5 (f=2) stands.
    assert_eq!(
      scatter_width(gb, gb, 60 * 1_000_000_000, 2),
      5,
      "the candidate floor is the lower bound"
    );
    // Unknown bandwidth or budget → the candidate floor, the tightest, lowest-loss neighbourhood.
    assert_eq!(scatter_width(100 * gb, 0, 10_000_000_000, 1), 3);
    assert_eq!(scatter_width(100 * gb, gb, 0, 1), 3);
    // The laptop: f=0 → the floor is 1 (the host itself), and a bigger recovery need cannot lower it.
    assert_eq!(scatter_width(0, gb, 1_000_000_000, 0), 1);
  }

  /// AC (§4.8 "Placement"; `research/metadata-replication.md` §3.1): the fixed copysets are the owner plus
  /// `2f` co-holders across **distinct** failure domains, they partition the neighbourhood's co-holders into
  /// a **linear** count (not the per-object super-linear one), and a host sharing the owner's domain is never
  /// a co-holder. This is the "without loss" construction — no single domain failure loses a whole copyset.
  #[test]
  fn the_fixed_copysets_are_distinct_domain_and_minimal() {
    let owner = HostId(1);
    // Owner in domain 10; four co-holders in four distinct domains; one host shares the owner's domain.
    let neighbourhood = vec![
      (owner, 10),
      (HostId(2), 20),
      (HostId(3), 30),
      (HostId(4), 40),
      (HostId(5), 50),
      (HostId(6), 10), // shares the owner's domain — must never co-hold
    ];
    let copysets = owner_copysets(owner, &neighbourhood, Quorum { f: 1 });
    // f=1 → 2 co-holders per copyset; four usable co-holders → 2 copysets (linear, ⌈4/2⌉).
    assert_eq!(
      copysets.len(),
      2,
      "the co-holders partition into ⌈4/2⌉ copysets"
    );
    let domain_of = |h: HostId| neighbourhood.iter().find(|(x, _)| *x == h).map(|(_, d)| *d);
    let mut covered = std::collections::BTreeSet::new();
    for copyset in &copysets {
      assert_eq!(copyset[0], owner, "the owner heads every copyset");
      assert_eq!(copyset.len(), 3, "2f+1 = 3 holders");
      let domains: Vec<_> = copyset.iter().map(|h| domain_of(*h)).collect();
      let distinct: std::collections::BTreeSet<_> = domains.iter().collect();
      assert_eq!(
        distinct.len(),
        domains.len(),
        "no copyset repeats a failure domain"
      );
      assert!(
        !copyset[1..].contains(&HostId(6)),
        "a host in the owner's domain never co-holds"
      );
      covered.extend(copyset[1..].iter().copied());
    }
    assert_eq!(
      covered,
      [HostId(2), HostId(3), HostId(4), HostId(5)]
        .into_iter()
        .collect(),
      "every usable co-holder is covered exactly once"
    );
  }

  /// AC (§4.8 "Placement"): `copyset_for` maps an object to exactly one of the owner's copysets,
  /// deterministically, and spreads objects across them — rendezvous over the copysets, not the hosts.
  #[test]
  fn an_object_maps_to_one_copyset_deterministically_and_spread() {
    let owner = HostId(1);
    let neighbourhood: Vec<(HostId, DomainId)> = (0..8u64)
      .map(|i| (HostId(i + 2), i)) // eight co-holders, each its own domain
      .chain(std::iter::once((owner, 100)))
      .collect();
    let copysets = owner_copysets(owner, &neighbourhood, Quorum { f: 1 });
    assert_eq!(copysets.len(), 4, "eight co-holders, 2 each → 4 copysets");
    let mut hit = std::collections::BTreeSet::new();
    for local in 0..256u64 {
      let object = ObjectId::new(owner, local);
      let chosen = copyset_for(object, &copysets).expect("a copyset");
      assert_eq!(
        copyset_for(object, &copysets).as_deref(),
        Some(chosen.as_slice()),
        "deterministic"
      );
      assert!(
        copysets.contains(&chosen),
        "the choice is one of the fixed copysets"
      );
      hit.insert(chosen);
    }
    assert!(hit.len() > 1, "objects spread over more than one copyset");
    // The laptop degenerate: f=0 → one copyset, the owner alone.
    let solo = owner_copysets(owner, &[(owner, 0)], Quorum { f: 0 });
    assert_eq!(solo, vec![vec![owner]]);
  }

  /// AC (§4.8 "Placement", D-14): the bounded neighbourhood is the owner plus the top `scatter-1` alive
  /// hosts, deterministic and owner-specific, never over `scatter`; `scatter = 0` is unbounded. Different
  /// owners scatter over different sets, and the selection is stable — the same host enters or leaves only
  /// as its rendezvous rank crosses the cut.
  #[test]
  fn the_bounded_neighbourhood_is_deterministic_owner_specific_and_capped() {
    let alive: Vec<HostId> = (1..=20u64).map(HostId).collect();
    let owner = HostId(1);
    // Unbounded: the whole alive set.
    assert_eq!(select_neighbourhood(owner, &alive, 0).len(), 20);
    // Bounded to a scatter width of 5: owner + 4.
    let n = select_neighbourhood(owner, &alive, 5);
    assert_eq!(n.len(), 5, "never over the scatter width");
    assert_eq!(n[0], owner, "the owner is always in its own neighbourhood");
    assert_eq!(
      select_neighbourhood(owner, &alive, 5),
      n,
      "deterministic — every node computes the same set"
    );
    // Owner-specific: a different owner scatters over a (generally) different set.
    let other = select_neighbourhood(HostId(2), &alive, 5);
    let a: std::collections::BTreeSet<_> = n.iter().copied().collect();
    let b: std::collections::BTreeSet<_> = other.iter().copied().collect();
    assert_ne!(a, b, "different owners get different neighbourhoods");
    // Stable under a join: adding host 99 keeps most of the neighbourhood (add-before-remove).
    let mut grown = alive.clone();
    grown.push(HostId(99));
    let after: std::collections::BTreeSet<_> = select_neighbourhood(owner, &grown, 5)
      .into_iter()
      .filter(|h| *h != HostId(99))
      .collect();
    let kept = a.intersection(&after).count();
    assert!(
      kept >= 3,
      "a join disturbs at most one member of a 5-host neighbourhood"
    );
  }

  /// The object a takeover test promotes. A committed head "v1" lives at sequence 0, epoch 1,
  /// generation 0, under the original owner D on candidates {D, H2, H3} at f=1, held by the commit
  /// quorum {D, H2}; H3 lags (it never received the head), so a promotion must recover the head from
  /// the quorum, not assume every candidate has it.
  const TAKEOVER_OBJECT: ObjectId = ObjectId::new(HostId(1), 9);

  /// Builds the committed-head state a takeover inherits: the original owner D and the two candidate
  /// holders, with the head "v1" committed to {D, H2} under epoch 1, generation 0. Returns
  /// `(owner D, holder H2, holder H3)` and the candidate set.
  fn committed_head() -> (Acceptor, Acceptor, Acceptor, Vec<HostId>) {
    let d = HostId(1);
    let h2 = HostId(2);
    let h3 = HostId(3);
    let authority = Authority {
      generation: 0,
      owner: d,
    };
    let candidates = vec![d, h2, h3];
    let mut owner = Acceptor::new(d, authority);
    let mut holder2 = Acceptor::new(h2, authority);
    let holder3 = Acceptor::new(h3, authority);
    let head = Record {
      owner: d,
      object: TAKEOVER_OBJECT,
      sequence: 0,
      epoch: HostEpoch(1),
      generation: 0,
      value: b"v1".to_vec(),
    };
    owner.accept(&head).expect("the owner accepts its own head");
    holder2
      .accept(&head)
      .expect("H2 accepts the head (the commit quorum)");
    // H3 never received the head — a lagging candidate the promotion must tolerate.
    (owner, holder2, holder3, candidates)
  }

  /// A [`Prepare`] and a [`Promise`] round-trip through their wire encoding unchanged (both a promise
  /// carrying a highest record and one carrying none), and a truncated or mis-flagged message decodes
  /// to a typed refusal, never a panic (hostile input crossing the network).
  #[test]
  fn prepare_and_promise_round_trip_and_refuse_hostile_bytes() {
    let prepare = Prepare {
      owner: HostId(7),
      object: ObjectId::new(HostId(1), 0x1234),
      epoch: HostEpoch(3),
      generation: 2,
    };
    assert_eq!(
      Prepare::decode(&prepare.encode()),
      Ok(prepare),
      "a prepare round-trips"
    );
    assert_eq!(
      Prepare::decode(&[0u8; 3]),
      Err(RegisterError::MalformedRecord),
      "a wrong-length prepare is refused"
    );

    let with_record = Promise {
      holder: HostId(2),
      object: ObjectId::new(HostId(1), 0x1234),
      epoch: HostEpoch(3),
      generation: 2,
      highest: Some(Accepted {
        sequence: 5,
        epoch: HostEpoch(2),
        value: b"head".to_vec(),
      }),
    };
    let without = Promise {
      highest: None,
      ..with_record.clone()
    };
    assert_eq!(
      Promise::decode(&with_record.encode()),
      Ok(with_record.clone()),
      "a promise with a highest record round-trips"
    );
    assert_eq!(
      Promise::decode(&without.encode()),
      Ok(without),
      "a promise with no record round-trips"
    );

    // A promise that flags a record present but carries none is refused, not read past its end.
    let mut truncated = with_record.encode();
    truncated.truncate(PROMISE_PREFIX_BYTES);
    truncated
      .last_mut()
      .map(|flag| *flag = PROMISE_SOME)
      .expect("a flag byte");
    assert_eq!(
      Promise::decode(&truncated),
      Err(RegisterError::MalformedRecord),
      "a present-flag with no record is refused"
    );
    // An unknown flag is refused.
    let mut bad_flag = with_record.encode();
    bad_flag[PROMISE_PREFIX_BYTES - 1] = 0xff;
    assert_eq!(
      Promise::decode(&bad_flag),
      Err(RegisterError::MalformedRecord),
      "an unknown flag is refused"
    );
  }

  /// AC (§4.8 "Promotion and takeover", Continuity): a new owner running phase one over a quorum
  /// adopts the head that committed under the old epoch — even from a quorum where one candidate lagged
  /// — and re-commits it under the new epoch, so the head survives the takeover. Do a takeover of a
  /// committed head; expect the head is adopted and holds under the new epoch.
  #[test]
  fn a_takeover_adopts_the_committed_head_under_the_new_epoch() {
    let (mut owner, mut holder2, mut holder3, candidates) = committed_head();
    let survivors: Vec<HostId> = candidates
      .iter()
      .copied()
      .filter(|h| *h != owner.id)
      .collect();
    let successor = rendezvous_first(&survivors, TAKEOVER_OBJECT).expect("a survivor takes over");
    let new_authority = Authority {
      generation: 1,
      owner: successor,
    };

    // The configuration group distributed the new authority (generation 1, owner = the successor);
    // the surviving holders install it, fencing the old owner by generation.
    holder2
      .install_authority(new_authority)
      .expect("H2 installs");
    holder3
      .install_authority(new_authority)
      .expect("H3 installs");

    // The successor runs phase one at the bumped epoch 2 over the two survivors (the dead owner is
    // unreachable), promising each and adopting the newest reported record.
    let prepare = Prepare {
      owner: successor,
      object: TAKEOVER_OBJECT,
      epoch: HostEpoch(2),
      generation: 1,
    };
    let quorum = Quorum { f: 1 };
    let promotion = {
      let mut holders: Vec<&mut dyn Promoter> = vec![&mut holder2, &mut holder3];
      promote_over_holders(&candidates, &prepare, &mut holders)
    };
    assert!(
      promotion.promoted(quorum),
      "two survivors promised — a quorum at f=1"
    );
    assert_eq!(
      promotion.adopted,
      Some(Accepted {
        sequence: 0,
        epoch: HostEpoch(1),
        value: b"v1".to_vec(),
      }),
      "the committed head is adopted, recovered from the one survivor that held it"
    );

    // The successor completes safe adoption: it re-commits the adopted head under the new epoch.
    let adoption = promotion
      .adoption_record(&prepare)
      .expect("there is a head to adopt");
    let placement = {
      let mut holders: Vec<&mut dyn Holder> = vec![&mut holder2, &mut holder3];
      commit_over_holders(&candidates, &adoption, &mut holders)
    };
    assert!(
      placement.placed(quorum),
      "the re-committed head places under the new epoch"
    );
    // The head survived: both survivors now hold "v1" at the new epoch (H3, which lagged, caught up).
    let (_, h2_state) = holder2.persisted();
    let (_, h3_state) = holder3.persisted();
    for state in [h2_state, h3_state] {
      assert_eq!(
        state,
        vec![(TAKEOVER_OBJECT, 0u64, HostEpoch(2), b"v1".to_vec())],
        "the head holds at the new epoch"
      );
    }
    // Keep the original owner referenced (it is the dead host; unused after the takeover).
    let _ = &mut owner;
  }

  /// AC (§4.8 "Promotion and takeover", StaleNeverCommits): after a takeover raised the holders' fence,
  /// a resumed stale owner cannot commit — its records are refused and it reaches no quorum, and a write
  /// under the current authority but an epoch below the fence is refused `StaleEpoch`. Take over a head,
  /// then have the old owner resume and try to advance it; expect no placement.
  #[test]
  fn a_stale_owner_cannot_commit_after_a_takeover() {
    let (owner, mut holder2, mut holder3, candidates) = committed_head();
    let survivors: Vec<HostId> = candidates
      .iter()
      .copied()
      .filter(|h| *h != owner.id)
      .collect();
    let successor = rendezvous_first(&survivors, TAKEOVER_OBJECT).expect("a survivor takes over");
    let new_authority = Authority {
      generation: 1,
      owner: successor,
    };
    holder2
      .install_authority(new_authority)
      .expect("H2 installs");
    holder3
      .install_authority(new_authority)
      .expect("H3 installs");
    let prepare = Prepare {
      owner: successor,
      object: TAKEOVER_OBJECT,
      epoch: HostEpoch(2),
      generation: 1,
    };
    {
      let mut holders: Vec<&mut dyn Promoter> = vec![&mut holder2, &mut holder3];
      promote_over_holders(&candidates, &prepare, &mut holders);
    }

    // The old owner D resumes, unaware it was taken over, and tries to advance the head at its old
    // epoch and generation. Every holder that installed the new authority refuses (the generation
    // moved on), so D reaches no quorum.
    let stale = Record {
      owner: owner.id,
      object: TAKEOVER_OBJECT,
      sequence: 1,
      epoch: HostEpoch(1),
      generation: 0,
      value: b"v2-stale".to_vec(),
    };
    let placement = {
      let mut holders: Vec<&mut dyn Holder> = vec![&mut holder2, &mut holder3];
      commit_over_holders(&candidates, &stale, &mut holders)
    };
    assert!(
      placement.acked.is_empty(),
      "no holder accepts the stale owner's write"
    );
    assert!(
      !placement.placed(Quorum { f: 1 }),
      "the stale owner does not commit"
    );

    // Even a write under the *current* authority but an epoch below the fence is refused StaleEpoch —
    // the fence itself, raised by the promotion, is what stops it.
    let stale_epoch = Record {
      owner: successor,
      object: TAKEOVER_OBJECT,
      sequence: 1,
      epoch: HostEpoch(1),
      generation: 1,
      value: b"v2".to_vec(),
    };
    assert_eq!(
      holder2.accept(&stale_epoch),
      Err(RegisterError::StaleEpoch { current: 2 }),
      "an epoch below the raised fence is refused"
    );
  }

  /// AC (§4.8 phase one): a promotion below a quorum of promises does not promote (the new owner must
  /// not serve on it), and a forged or non-candidate promise cannot manufacture the quorum. Prepare
  /// over one real survivor plus a lying holder; expect no promotion.
  #[test]
  fn a_promotion_below_quorum_does_not_promote() {
    let (_owner, mut holder2, _holder3, candidates) = committed_head();
    let successor = HostId(2);
    holder2
      .install_authority(Authority {
        generation: 1,
        owner: successor,
      })
      .expect("install");
    let prepare = Prepare {
      owner: successor,
      object: TAKEOVER_OBJECT,
      epoch: HostEpoch(2),
      generation: 1,
    };

    // A holder that returns a forged promise for a non-candidate id, to pad the count.
    struct LyingPromoter {
      reply: Promise,
    }
    impl Promoter for LyingPromoter {
      fn promise(&mut self, _prepare: &Prepare) -> Result<Promise, RegisterError> {
        Ok(self.reply.clone())
      }
    }
    let mut liar = LyingPromoter {
      reply: Promise {
        holder: HostId(99),
        object: TAKEOVER_OBJECT,
        epoch: HostEpoch(2),
        generation: 1,
        highest: None,
      },
    };

    let promotion = {
      let mut holders: Vec<&mut dyn Promoter> = vec![&mut holder2, &mut liar];
      promote_over_holders(&candidates, &prepare, &mut holders)
    };
    assert_eq!(
      promotion.promised,
      vec![successor],
      "only the real candidate promised; the non-candidate forgery is not counted"
    );
    assert!(
      !promotion.promoted(Quorum { f: 1 }),
      "one promise is not a quorum at f=1 — the new owner must not serve yet"
    );
  }
}
